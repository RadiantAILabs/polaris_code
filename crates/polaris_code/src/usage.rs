//! Session token accounting and cost estimation.
//!
//! The conversation panel keeps a live token tally for the on-screen meter, but
//! that count lives inside the TUI and is gone once the terminal tears down.
//! [`TokenMeter`] is the binary-side mirror: a shared, thread-safe counter the
//! agent's `act` system feeds the same usage deltas, so the binary can print a
//! usage + cost summary on exit — after the alternate screen is restored.
//!
//! Cost is an *estimate* read from the shared, LiteLLM-backed
//! [`PriceStore`](polaris::models::pricing::PriceStore) ([`price_for`]); batch
//! and other negotiated discounts are excluded, so treat it as a ceiling.
//!
//! The meter is also the *only* accounting authority a non-interactive run
//! reports. [`TokenMeter::snapshot`] freezes it into a [`RunUsage`] — the
//! versioned, machine-readable shape a headless run emits and a downstream
//! consumer asserts against. A snapshot is a read of the counters the run
//! already accumulated: nothing in it is recomputed from the transcript,
//! re-tokenized, or estimated after the fact. [`TokenMeter::summary`] remains
//! the separate human-readable rendering the interactive front end prints on
//! exit.
//!
//! # Making the number enforceable
//!
//! [`RunCeiling`] is the operator's side of the same accounting: a spend limit
//! in money, in tokens, or in both. It decides nothing by itself — it answers
//! [`evaluate`](RunCeiling::evaluate) for a [`RunUsage`] and the run surface
//! ([`drive_turn`](crate::run::drive_turn)) consults that answer on a turn
//! boundary. The meter measures, the ceiling judges, the loop stops.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use polaris::models::bedrock::normalize_bedrock_model_id;
use polaris::models::llm::{LlmRequest, ModelPricing};
use polaris::models::pricing::PriceStore;
use polaris::system::resource::{GlobalResource, LocalResource};
use serde::Serialize;

/// Schema identity of the accounting a run emits, carried in every
/// [`RunUsage`] as its `schema` field.
///
/// A consumer reads this first: it tells the consumer which contract it is
/// holding, and a later incompatible change to the shape bumps the version
/// rather than silently altering what the same fields mean. The spelling
/// matches the `polaris-code-<contract>/v<n>` convention the harness-eval
/// storage records already use.
pub const RUN_USAGE_SCHEMA: &str = "polaris-code-run-usage/v1";

/// Schema identity of the ceiling accounting a run emits, carried in every
/// [`CeilingStatus`] as its `schema` field.
///
/// A separate contract from [`RUN_USAGE_SCHEMA`] on purpose: the measurement a
/// run reports and the limit an operator set are different facts with
/// different lifetimes, and adding a limit to the usage object would change
/// what `polaris-code-run-usage/v1` means for every consumer already parsing
/// it.
pub const RUN_CEILING_SCHEMA: &str = "polaris-code-run-ceiling/v1";

/// Stable identity of the price table [`price_for`] resolves against.
///
/// Reported by every [`RunUsage`] as `pricing_basis`. [`price_for`] delegates
/// to the shared [`PriceStore`], which may answer from a live-fetched table, a
/// cached table, or its built-in fallback depending on what has succeeded so
/// far — so this names the *source* consulted, not a fixed set of rates. A
/// recorded cost stays attributable to "the shared price store" even as the
/// store's own snapshot moves underneath it.
pub const PRICE_BASIS: &str = "litellm-price-store";

/// Local input estimate: serialized bytes / 4, plus 25% and 512 tokens for
/// token density and provider framing. Includes tools. This is not an exact
/// tokenizer or a guaranteed upper bound for every language or image input.
#[must_use]
pub fn request_input_bound(request: &LlmRequest) -> u64 {
    let bytes = serde_json::to_vec(request).map_or(u64::MAX, |raw| raw.len() as u64);
    bytes
        .div_ceil(4)
        .saturating_add(bytes.div_ceil(16))
        .saturating_add(512)
}

/// One recently confirmed cache prefix, retained only inside this task meter.
#[derive(Debug)]
struct CachedRequest {
    prefix: Vec<u8>,
    messages: usize,
    tokens: u64,
    sent_at: Instant,
}

impl CachedRequest {
    fn prefix(request: &LlmRequest, messages: usize) -> Option<Vec<u8>> {
        serde_json::to_vec(&(
            &request.system,
            &request.tools,
            &request.tool_choice,
            &request.output_schema,
            request.messages.get(..messages)?,
        ))
        .ok()
    }

    fn new(request: &LlmRequest) -> Option<Self> {
        if request.cache.is_disabled() {
            return None;
        }
        let messages = request
            .cache
            .breakpoints
            .iter()
            .copied()
            .filter(|&index| index < request.messages.len())
            .max()
            .map_or(0, |index| index + 1);
        Some(Self {
            prefix: Self::prefix(request, messages)?,
            messages,
            tokens: 0,
            sent_at: Instant::now(),
        })
    }

    fn reusable_tokens(&self, request: &LlmRequest) -> u64 {
        // Leave a minute of the usual five-minute lifetime for the next call.
        // A changed tool choice, tools, system or any prefix message is cold.
        let reachable = if self.messages == 0 {
            request.cache.prefix
        } else {
            request
                .cache
                .breakpoints
                .iter()
                .any(|&index| index < request.messages.len() && index + 1 >= self.messages)
        };
        if reachable
            && self.sent_at.elapsed() < Duration::from_secs(240)
            && Self::prefix(request, self.messages).as_ref() == Some(&self.prefix)
        {
            // Hold back 10% even for an identical, confirmed prefix.
            self.tokens.saturating_mul(9) / 10
        } else {
            0
        }
    }
}

/// Validated per-coding-task USD limit, installed only when explicitly set.
#[derive(Clone, Copy, Debug)]
pub struct TaskCostLimit(pub f64);

impl TaskCostLimit {
    /// Refuse limits that cannot bound a task.
    pub fn new(limit: f64) -> Result<Self, InvalidCeiling> {
        RunCeiling::from_limits(Some(limit), None)?;
        Ok(Self(limit))
    }
}

impl GlobalResource for TaskCostLimit {}

/// Shared, thread-safe meter accumulating billed token counts for the session.
///
/// Written by the agent's `act` system as streaming usage deltas arrive (the
/// same increments it forwards to the conversation panel); read by the binary
/// on exit. Cloning is cheap — all clones share one set of atomic counters and
/// one [`PriceStore`].
#[derive(Clone, Debug)]
pub struct TokenMeter {
    inner: Arc<MeterInner>,
    pricing: PriceStore,
}

impl Default for TokenMeter {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            pricing: PriceStore::live(),
        }
    }
}

#[derive(Default, Debug)]
struct MeterInner {
    model_turns: AtomicU64,
    input: AtomicU64,
    output: AtomicU64,
    cache_read: AtomicU64,
    cache_creation: AtomicU64,
    by_model: Mutex<BTreeMap<String, [u64; 4]>>,
    cost_cap: Mutex<Option<TaskCostState>>,
}

#[derive(Debug)]
struct TaskCostState {
    limit: f64,
    spent: f64,
    reserved: f64,
    warned: bool,
    notice_sent: bool,
    stopped: bool,
    cached: BTreeMap<String, CachedRequest>,
}

/// A reservation for the largest estimated bill of one model request.
/// Dropping it without a response releases the reservation.
#[derive(Debug)]
pub struct CostPermit {
    meter: TokenMeter,
    reserved: f64,
    price: Price,
    settled: bool,
    cache_candidate: Option<(String, CachedRequest)>,
}

impl CostPermit {
    /// Replace the reservation with the provider's billed usage.
    /// Missing usage is charged at the reservation estimate.
    pub fn settle(mut self, usage: &polaris::models::llm::Usage) {
        let actual = match (usage.input_tokens, usage.output_tokens) {
            (Some(input), Some(output)) => self.price.cost_usd(
                input,
                output,
                usage.cache_read_tokens.unwrap_or_default(),
                usage.cache_creation_tokens.unwrap_or_default(),
            ),
            _ => self.reserved,
        };
        tracing::debug!(
            reserved_usd = self.reserved,
            actual_usd = actual,
            "Settled model call cost"
        );
        let mut state = self.meter.inner.cost_cap.lock().expect("cost cap poisoned");
        if let Some(state) = state.as_mut() {
            state.reserved -= self.reserved;
            state.spent += actual;
            if let Some((model, mut cached)) = self.cache_candidate.take() {
                cached.tokens = usage
                    .cache_read_tokens
                    .unwrap_or_default()
                    .saturating_add(usage.cache_creation_tokens.unwrap_or_default());
                if usage.input_tokens.is_some()
                    && usage.output_tokens.is_some()
                    && cached.tokens > 0
                {
                    state.cached.insert(model, cached);
                } else {
                    state.cached.remove(&model);
                }
            }
            if state.spent >= state.limit * 0.8 {
                state.warned = true;
            }
        }
        self.settled = true;
    }
}

impl Drop for CostPermit {
    fn drop(&mut self) {
        if !self.settled {
            let mut state = self.meter.inner.cost_cap.lock().expect("cost cap poisoned");
            if let Some(state) = state.as_mut() {
                state.reserved -= self.reserved;
            }
        }
    }
}

impl TokenMeter {
    /// Create an empty meter.
    ///
    /// Pricing defaults to [`PriceStore::live`] — the process-wide,
    /// web-fetched price store. Model providers use it only when given it
    /// with `with_pricing_store`, as the binary does. Use
    /// [`with_pricing_store`](Self::with_pricing_store) to inject a different
    /// source or, in tests, a fixture that never touches the network.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces this meter's price store (default: [`PriceStore::live`]).
    #[must_use]
    pub fn with_pricing_store(mut self, pricing: PriceStore) -> Self {
        self.pricing = pricing;
        self
    }

    /// Count one model request attempt, independently of its usage or success.
    /// Call once at the provider boundary, never for streaming token deltas.
    pub fn record_model_turn(&self) {
        self.inner.model_turns.fetch_add(1, Ordering::Relaxed);
    }

    /// Enable a per-task spend limit on this meter and all its clones.
    pub fn set_task_cost_cap(&self, limit: f64) -> Result<(), InvalidCeiling> {
        RunCeiling::from_limits(Some(limit), None)?;
        *self.inner.cost_cap.lock().expect("cost cap poisoned") = Some(TaskCostState {
            limit,
            spent: 0.0,
            reserved: 0.0,
            warned: false,
            notice_sent: false,
            stopped: false,
            cached: BTreeMap::new(),
        });
        Ok(())
    }

    /// Reserve an uncached request before the provider is called.
    /// `input_bound` must include the request's tools and message overhead.
    pub fn reserve_model_call(
        &self,
        model_id: &str,
        input_bound: u64,
        max_output: u32,
    ) -> Result<Option<CostPermit>, String> {
        self.reserve_call(model_id, input_bound, max_output, None)
    }

    /// Reserve estimated input and the full output allowance. Only a recent,
    /// byte-identical prefix confirmed by billed cache usage earns read pricing;
    /// new text, changed prompts and expired prefixes reserve write pricing.
    pub fn reserve_request(
        &self,
        model_id: &str,
        request: &LlmRequest,
        max_output: u32,
    ) -> Result<Option<CostPermit>, String> {
        if !self.cost_cap_enabled() {
            return Ok(None);
        }
        self.reserve_call(
            model_id,
            request_input_bound(request),
            max_output,
            Some(request),
        )
    }

    fn reserve_call(
        &self,
        model_id: &str,
        input_bound: u64,
        max_output: u32,
        request: Option<&LlmRequest>,
    ) -> Result<Option<CostPermit>, String> {
        let mut guard = self.inner.cost_cap.lock().expect("cost cap poisoned");
        let Some(state) = guard.as_mut() else {
            return Ok(None);
        };
        let Some(price) = price_for(&self.pricing, model_id) else {
            state.stopped = true;
            return Err(format!("cost-cap: model {model_id} has no known price"));
        };
        let cache_read = request
            .and_then(|request| {
                state
                    .cached
                    .get(model_id)
                    .map(|cached| cached.reusable_tokens(request))
            })
            .unwrap_or_default()
            .min(input_bound);
        let worst = price.cost_usd(0, max_output.into(), cache_read, input_bound - cache_read);
        if worst > state.limit - state.spent - state.reserved {
            state.stopped = true;
            return Err(format!(
                "cost-cap: ${:.4} spent; next call could cost ${worst:.4}, exceeding ${:.4}",
                state.spent, state.limit
            ));
        }
        tracing::debug!(
            model_id,
            input_bound,
            cache_read,
            reserved_usd = worst,
            "Reserved model call cost"
        );
        state.reserved += worst;
        Ok(Some(CostPermit {
            meter: self.clone(),
            reserved: worst,
            price,
            settled: false,
            cache_candidate: request
                .and_then(CachedRequest::new)
                .map(|cached| (model_id.to_owned(), cached)),
        }))
    }

    /// One short notice, delivered on the first model call after 80% is spent.
    pub fn take_cost_notice(&self) -> bool {
        let mut guard = self.inner.cost_cap.lock().expect("cost cap poisoned");
        let Some(state) = guard.as_mut() else {
            return false;
        };
        if state.warned && !state.notice_sent {
            state.notice_sent = true;
            return true;
        }
        false
    }

    /// Whether a pre-call check stopped this task.
    #[must_use]
    pub fn cost_cap_stopped(&self) -> bool {
        self.inner
            .cost_cap
            .lock()
            .expect("cost cap poisoned")
            .as_ref()
            .is_some_and(|state| state.stopped)
    }

    /// Whether this task has an active cost ceiling.
    #[must_use]
    pub fn cost_cap_enabled(&self) -> bool {
        self.inner
            .cost_cap
            .lock()
            .expect("cost cap poisoned")
            .is_some()
    }

    /// Add a full-price usage increment (uncached input and output tokens billed
    /// since the last observation). Safe to call concurrently and through a
    /// shared reference.
    pub fn add(&self, input_delta: u64, output_delta: u64) {
        self.inner.input.fetch_add(input_delta, Ordering::Relaxed);
        self.inner.output.fetch_add(output_delta, Ordering::Relaxed);
    }

    /// Add a prompt-cache usage increment (tokens served from / written to the
    /// cache since the last observation), billed at the discounted cache tiers.
    pub fn add_cache(&self, cache_read_delta: u64, cache_creation_delta: u64) {
        self.inner
            .cache_read
            .fetch_add(cache_read_delta, Ordering::Relaxed);
        self.inner
            .cache_creation
            .fetch_add(cache_creation_delta, Ordering::Relaxed);
    }

    /// Adds one model step's billed usage under its actual provider/model ID.
    /// The ordinary counters still include these tokens for total reporting.
    pub fn add_for_model(
        &self,
        model_id: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_creation: u64,
    ) {
        self.add(input, output);
        self.add_cache(cache_read, cache_creation);
        let mut by_model = self.inner.by_model.lock().expect("model usage lock");
        let entry = by_model.entry(model_id.to_owned()).or_default();
        for (count, delta) in entry
            .iter_mut()
            .zip([input, output, cache_read, cache_creation])
        {
            *count += delta;
        }
    }

    /// Folds another meter's usage into this one, retaining per-model pricing.
    pub fn merge_from(&self, other: &Self) {
        self.add(other.input_tokens(), other.output_tokens());
        self.add_cache(other.cache_read_tokens(), other.cache_creation_tokens());
        let other_models = other.inner.by_model.lock().expect("model usage lock");
        let mut models = self.inner.by_model.lock().expect("model usage lock");
        for (id, counts) in other_models.iter() {
            let entry = models.entry(id.clone()).or_default();
            for (count, delta) in entry.iter_mut().zip(counts) {
                *count += delta;
            }
        }
    }

    /// Cumulative full-price (uncached) input tokens billed this session.
    #[must_use]
    pub fn input_tokens(&self) -> u64 {
        self.inner.input.load(Ordering::Relaxed)
    }

    /// Cumulative output tokens billed this session.
    #[must_use]
    pub fn output_tokens(&self) -> u64 {
        self.inner.output.load(Ordering::Relaxed)
    }

    /// Cumulative cache-read (hit) tokens this session.
    #[must_use]
    pub fn cache_read_tokens(&self) -> u64 {
        self.inner.cache_read.load(Ordering::Relaxed)
    }

    /// Cumulative cache-creation (write) tokens this session.
    #[must_use]
    pub fn cache_creation_tokens(&self) -> u64 {
        self.inner.cache_creation.load(Ordering::Relaxed)
    }

    /// Cumulative tokens billed this session across every tier.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens()
            + self.output_tokens()
            + self.cache_read_tokens()
            + self.cache_creation_tokens()
    }

    /// Estimated session cost in USD for `model_id`, or `None` when the model
    /// family is unpriced (see [`price_for`]). Bills cached input at the
    /// cache-read / cache-write tiers and the rest at the full rate.
    #[must_use]
    pub fn estimated_cost_usd(&self, model_id: &str) -> Option<f64> {
        self.snapshot(model_id).estimated_cost_usd
    }

    /// Freeze the meter into the versioned accounting a run reports
    /// ([`RunUsage`]), priced for `model_id`.
    ///
    /// This is the machine-readable counterpart to [`summary`](Self::summary),
    /// and the only accounting a non-interactive run emits. Every number in
    /// the result is a read of the counters this meter already accumulated —
    /// none is recomputed from the transcript or re-estimated afterwards — so
    /// a consumer may treat the snapshot as a measurement rather than a
    /// reconstruction.
    ///
    /// Each counter is loaded exactly once and `total_tokens` and
    /// `estimated_cost_usd` are derived from those same four values, so a
    /// snapshot is internally consistent even if the meter is still being
    /// written: the emitted total always equals the sum of the emitted parts,
    /// and the emitted cost always prices exactly the emitted counts.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_code::usage::{RUN_USAGE_SCHEMA, TokenMeter};
    ///
    /// let meter = TokenMeter::new();
    /// meter.add(1_000, 100);
    /// meter.add_cache(400, 0);
    ///
    /// let usage = meter.snapshot("anthropic/claude-sonnet-4-6");
    /// assert_eq!(usage.schema, RUN_USAGE_SCHEMA);
    /// assert_eq!(usage.input_tokens, 1_000);
    /// assert_eq!(usage.cache_read_tokens, 400);
    /// assert_eq!(usage.total_tokens, 1_500);
    /// assert!(usage.pricing.is_some());
    /// assert!(usage.estimated_cost_usd.is_some());
    /// ```
    #[must_use]
    pub fn snapshot(&self, model_id: &str) -> RunUsage {
        let input_tokens = self.input_tokens();
        let output_tokens = self.output_tokens();
        let cache_read_tokens = self.cache_read_tokens();
        let cache_creation_tokens = self.cache_creation_tokens();
        let pricing = price_for(&self.pricing, model_id);
        let mut default_counts = [
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        ];
        let mut model_cost = Some(0.0);
        let mut model_steps = Vec::new();
        for (id, counts) in self.inner.by_model.lock().expect("model usage lock").iter() {
            for (default, count) in default_counts.iter_mut().zip(counts) {
                *default = default.saturating_sub(*count);
            }
            let model_price = price_for(&self.pricing, id);
            let step_cost =
                model_price.map(|price| price.cost_usd(counts[0], counts[1], counts[2], counts[3]));
            model_cost = model_cost.zip(step_cost).map(|(cost, step)| cost + step);
            model_steps.push(ModelUsage {
                model_id: id.clone(),
                input_tokens: counts[0],
                output_tokens: counts[1],
                cache_read_tokens: counts[2],
                cache_creation_tokens: counts[3],
                estimated_cost_usd: step_cost,
                pricing: model_price,
            });
        }
        RunUsage {
            schema: RUN_USAGE_SCHEMA,
            num_turns: self.inner.model_turns.load(Ordering::Relaxed),
            model_id: model_id.to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            total_tokens: input_tokens + output_tokens + cache_read_tokens + cache_creation_tokens,
            estimated_cost_usd: pricing.zip(model_cost).map(|(price, cost)| {
                cost + price.cost_usd(
                    default_counts[0],
                    default_counts[1],
                    default_counts[2],
                    default_counts[3],
                )
            }),
            pricing_basis: PRICE_BASIS,
            pricing,
            model_steps,
        }
    }

    /// Multi-line summary for the binary to print on exit: a header followed by
    /// indented token counts and the estimated cost when the model family is
    /// priced. Cache tiers are shown so the effect of prompt caching is visible.
    ///
    /// `max_output_tokens` is the per-request output limit the session
    /// registered its model provider with. It is a bound, not a measurement,
    /// so it is reported last and separately from the counters — an
    /// interactive session states the limit it ran under for the same reason
    /// a non-interactive one does
    /// ([`RunReport::max_output_tokens`](crate::exec::RunReport::max_output_tokens)).
    #[must_use]
    pub fn summary(&self, model_id: &str, max_output_tokens: u32) -> String {
        let cost = match self.estimated_cost_usd(model_id) {
            Some(cost) => format!("${cost:.4}"),
            None => "n/a".to_string(),
        };
        format!(
            "Session usage\n  input:      {:>10} tok\n  output:     {:>10} tok\n  cache read: {:>10} tok\n  cache write:{:>10} tok\n  total:      {:>10} tok\n  est. cost:  {:>10}  ({model_id})\n  max output: {max_output_tokens:>10} tok/response",
            self.input_tokens(),
            self.output_tokens(),
            self.cache_read_tokens(),
            self.cache_creation_tokens(),
            self.total_tokens(),
            cost,
        )
    }
}

impl GlobalResource for TokenMeter {}

// A topology run gets its own meter, inserted into the run context where it
// shadows the session's global one. Same counter type, narrower scope: the
// graph bills the run, and the session's total is left where it was.
impl LocalResource for TokenMeter {}

/// One run's token and cost accounting, frozen from the [`TokenMeter`] the run
/// accumulated into — the versioned, machine-readable contract a
/// non-interactive run emits.
///
/// Produced by [`TokenMeter::snapshot`], carried on
/// [`TurnOutcome::usage`](crate::run::TurnOutcome::usage), and emitted by the
/// non-interactive front end (see
/// [`RunReport::usage`](crate::exec::RunReport::usage) and
/// [`usage_message`](crate::exec::usage_message)). A consumer that needs the
/// cost of a run reads this and nothing else.
///
/// # Reading it
///
/// - `schema` is [`RUN_USAGE_SCHEMA`]. Check it before reading any other
///   field; an unrecognised value means the contract moved and the rest of the
///   object may not mean what the consumer expects.
/// - The four counters are **billed tokens**, split by tier, exactly as the
///   provider reported them to the run. `total_tokens` is their sum.
/// - `estimated_cost_usd` is `None` — JSON `null` — for a model family this
///   crate does not price. `null` means *not priced*, never *free*.
/// - `pricing_basis` is always present, even when `pricing` is `None`: it
///   names the table that was consulted, so "unpriced" is attributable to a
///   specific table rather than to an unknown one.
/// - `pricing` is the matched rate card, or `None` when the family is
///   unpriced. When present, `estimated_cost_usd` is exactly
///   [`Price::cost_usd`] of the four counters above at those rates.
///
/// # Scope of the numbers
///
/// The counters are cumulative for the meter the run drove, read at the end of
/// the turn. A `--print` run drives exactly one turn in a fresh process with a
/// fresh meter, so for a headless run this *is* that run's total. An
/// interactive session shares one meter across every turn, so there the same
/// snapshot reads as "the session so far".
///
/// # Example
///
/// ```
/// use polaris_code::usage::TokenMeter;
///
/// let meter = TokenMeter::new();
/// meter.add(120, 8);
/// let usage = meter.snapshot("scripted/unpriced-model");
///
/// // An unpriced family still reports its counters and the basis consulted.
/// assert_eq!(usage.total_tokens, 128);
/// assert!(usage.estimated_cost_usd.is_none());
/// assert!(usage.pricing.is_none());
/// assert!(!usage.pricing_basis.is_empty());
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct RunUsage {
    /// Model request attempts in this session, including failed calls; not user turns.
    pub num_turns: u64,
    /// Schema identity of this shape — always [`RUN_USAGE_SCHEMA`].
    pub schema: &'static str,
    /// The model id the run actually drove, read back from the session's own
    /// `AgentConfig` rather than from the command line.
    pub model_id: String,
    /// Cumulative full-price (uncached) input tokens billed.
    pub input_tokens: u64,
    /// Cumulative output tokens billed.
    pub output_tokens: u64,
    /// Cumulative cache-read (hit) tokens billed at the discounted read tier.
    pub cache_read_tokens: u64,
    /// Cumulative cache-creation (write) tokens billed at the write tier.
    pub cache_creation_tokens: u64,
    /// The sum of the four counters above.
    pub total_tokens: u64,
    /// Estimated USD cost of the default-model remainder plus each entry in
    /// `model_steps` at its own rate, or `None` if any model is unpriced.
    pub estimated_cost_usd: Option<f64>,
    /// Stable identity of the price table consulted — [`PRICE_BASIS`].
    /// Reported whether or not a rate was found.
    pub pricing_basis: &'static str,
    /// The default model's rate card; `model_steps` carries alternate rates.
    pub pricing: Option<Price>,
    /// Usage billed to explicit per-step models. Omitted for ordinary runs,
    /// keeping their serialized accounting shape byte-for-byte unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_steps: Vec<ModelUsage>,
}

/// Billed usage for one explicitly selected step model, aggregated by ID.
#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    /// Provider/model identifier used for these calls.
    pub model_id: String,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cached input tokens read.
    pub cache_read_tokens: u64,
    /// Cached input tokens written.
    pub cache_creation_tokens: u64,
    /// Estimated cost at this model's rate, or `None` if unknown.
    pub estimated_cost_usd: Option<f64>,
    /// Rate card applied to this step model.
    pub pricing: Option<Price>,
}

/// USD price per million tokens for a model.
///
/// Emitted verbatim inside [`RunUsage::pricing`] so a recorded cost is
/// self-describing: a consumer can see which rates produced the number,
/// without holding a copy of the price table. A direct field copy of the
/// [`ModelPricing`] [`price_for`] resolved, kept as a local type (rather than
/// serializing `ModelPricing` itself) because `ModelPricing` does not derive
/// [`Serialize`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Price {
    /// Input (prompt) price per 1M full-price tokens.
    pub input_per_mtok: f64,
    /// Output (completion) price per 1M tokens.
    pub output_per_mtok: f64,
    /// Cache-read price per 1M tokens.
    pub cache_read_per_mtok: f64,
    /// Cache-write price per 1M tokens.
    pub cache_write_per_mtok: f64,
}

impl From<ModelPricing> for Price {
    fn from(pricing: ModelPricing) -> Self {
        Self {
            input_per_mtok: pricing.input_per_million_usd,
            output_per_mtok: pricing.output_per_million_usd,
            cache_read_per_mtok: pricing.cache_read_per_million_usd,
            cache_write_per_mtok: pricing.cache_write_per_million_usd,
        }
    }
}

impl Price {
    /// USD cost of these token counts at these rates.
    ///
    /// The single cost formula in this crate: both
    /// [`TokenMeter::estimated_cost_usd`] and [`TokenMeter::snapshot`] price
    /// through it, so the human summary and the machine-readable accounting
    /// can never disagree.
    #[must_use]
    pub fn cost_usd(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_mtok
            + (output_tokens as f64 / 1_000_000.0) * self.output_per_mtok
            + (cache_read_tokens as f64 / 1_000_000.0) * self.cache_read_per_mtok
            + (cache_creation_tokens as f64 / 1_000_000.0) * self.cache_write_per_mtok
    }
}

/// Map a model id to its list price via the shared `store`, matched on the
/// model name with any `provider/` prefix stripped (e.g.
/// `anthropic/claude-sonnet-4-6` looks up `claude-sonnet-4-6`, mirroring the
/// `{provider}/{model}` split [`ModelRegistry::llm`](polaris::models::ModelRegistry::llm)
/// applies before a provider ever sees the id). Returns `None` when `store`
/// has no matching row, so callers can show "n/a" rather than a wrong number.
///
/// Tries the stripped id verbatim first (matches a regional Bedrock row like
/// `us.anthropic.claude-opus-5-5`, which can price differently from its
/// first-party counterpart), then falls back to
/// [`normalize_bedrock_model_id`](polaris::models::bedrock::normalize_bedrock_model_id)
/// of it — the same two-step lookup [`BedrockProvider`](polaris::models::bedrock::BedrockProvider)
/// uses for its own pricing, so a Bedrock id such as
/// `aws.bedrock/global.anthropic.claude-sonnet-5` still resolves against the
/// first-party `claude-sonnet-5` row.
///
/// Rates come from `store` — a live-fetched or cached table when available,
/// its small built-in fallback otherwise (see
/// [`PriceStore`]) — and cache tiers are exactly what the table carries, so a
/// fully uncached estimate is an upper bound only insofar as the table omits
/// negotiated discounts.
#[must_use]
pub fn price_for(store: &PriceStore, model_id: &str) -> Option<Price> {
    let raw = model_id.split_once('/').map_or(model_id, |(_, raw)| raw);
    let raw = raw.to_ascii_lowercase();
    store
        .get(&raw)
        .or_else(|| store.get(normalize_bedrock_model_id(&raw)))
        .map(Price::from)
}

/// An operator-set spend limit for one run: money, tokens, or both.
///
/// Installed as a global by the binary when the operator asked for one
/// (`--max-cost-usd` / `--max-tokens`), and read back on a turn boundary by
/// [`drive_turn`](crate::run::drive_turn). A run with no ceiling installs
/// nothing, so the absence of this resource is "no limit was set" — never "the
/// limit is zero".
///
/// Both limits are *inclusive*: reaching one stops the run, because the next
/// turn would go past it.
///
/// # Example
///
/// ```
/// use polaris_code::usage::{RunCeiling, TokenMeter};
///
/// let ceiling = RunCeiling::from_limits(Some(100.00), None)
///     .expect("a positive dollar limit is valid")
///     .expect("one limit was given, so there is a ceiling");
///
/// let meter = TokenMeter::new();
/// meter.add(100_000, 10_000);
/// let status = ceiling.evaluate(&meter.snapshot("anthropic/claude-sonnet-4-6"));
///
/// // Well under the $100.00 allowed at any plausible per-token rate.
/// assert!(!status.stopped);
/// assert!(status.statement().contains("of $100.0000 allowed"));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RunCeiling {
    max_cost_usd: Option<f64>,
    max_tokens: Option<u64>,
}

impl RunCeiling {
    /// Build a ceiling from the operator's limits, or `Ok(None)` when neither
    /// was given.
    ///
    /// Validates here rather than at every read: a limit that cannot bound a
    /// run — not finite, or not positive — is a configuration error the
    /// operator has to see, not a value to silently coerce.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCeiling`] when a supplied limit is not a positive,
    /// finite bound.
    pub fn from_limits(
        max_cost_usd: Option<f64>,
        max_tokens: Option<u64>,
    ) -> Result<Option<Self>, InvalidCeiling> {
        if let Some(limit) = max_cost_usd
            && !(limit.is_finite() && limit > 0.0)
        {
            return Err(InvalidCeiling(format!(
                "a spend ceiling must be a positive amount in USD, got {limit}"
            )));
        }
        if max_tokens == Some(0) {
            return Err(InvalidCeiling(
                "a token ceiling must be a positive number of tokens, got 0".to_string(),
            ));
        }
        if max_cost_usd.is_none() && max_tokens.is_none() {
            return Ok(None);
        }
        Ok(Some(Self {
            max_cost_usd,
            max_tokens,
        }))
    }

    /// The money limit, when one was set.
    #[must_use]
    pub fn max_cost_usd(&self) -> Option<f64> {
        self.max_cost_usd
    }

    /// The token limit, when one was set.
    #[must_use]
    pub fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    /// Why this ceiling cannot bound a run on `model_id` under `store`, if it
    /// cannot.
    ///
    /// The unpriced case, stated once and consulted from both ends: a money
    /// ceiling over a model [`price_for`] has no rate for cannot be shown to
    /// hold, and `null` cost means *not priced*, never *free*. The binary
    /// calls this before the run starts so an unenforceable ceiling is a
    /// configuration error rather than a run that quietly spends without one;
    /// [`evaluate`](Self::evaluate) applies the same rule at every turn
    /// boundary, so a model that becomes unpriced mid-run stops it instead of
    /// widening it.
    #[must_use]
    pub fn unenforceable_for(&self, store: &PriceStore, model_id: &str) -> Option<String> {
        let limit = self.max_cost_usd?;
        if price_for(store, model_id).is_some() {
            return None;
        }
        Some(format!(
            "no price is known for model `{model_id}` under pricing basis `{PRICE_BASIS}`, \
             so a ${limit:.4} spend ceiling cannot be enforced; bound the run in tokens instead"
        ))
    }

    /// Where a run that has billed `usage` stands against this ceiling.
    ///
    /// Pure: it reads the accounting and reports, and nothing here stops
    /// anything. The run surface decides what to do with
    /// [`CeilingStatus::stopped`], and it decides it between turns.
    #[must_use]
    pub fn evaluate(&self, usage: &RunUsage) -> CeilingStatus {
        let mut reason = None;
        if let Some(limit) = self.max_cost_usd {
            reason = match usage.estimated_cost_usd {
                // Unpriced is not unlimited: without a rate there is no
                // evidence the run is inside the money ceiling, so it stops.
                None => Some(format!(
                    "model `{}` is unpriced under basis `{}`, so spend cannot be shown to be \
                     within the ${limit:.4} allowed",
                    usage.model_id, usage.pricing_basis
                )),
                Some(spent) if spent >= limit => {
                    Some(format!("spent ${spent:.4} of the ${limit:.4} allowed"))
                }
                Some(_) => None,
            };
        }
        if reason.is_none()
            && let Some(limit) = self.max_tokens
            && usage.total_tokens >= limit
        {
            reason = Some(format!(
                "spent {} tokens of the {limit} allowed",
                usage.total_tokens
            ));
        }
        CeilingStatus {
            schema: RUN_CEILING_SCHEMA,
            max_cost_usd: self.max_cost_usd,
            max_tokens: self.max_tokens,
            spent_usd: usage.estimated_cost_usd,
            spent_tokens: usage.total_tokens,
            stopped: reason.is_some(),
            reason,
        }
    }
}

impl GlobalResource for RunCeiling {}

/// The error [`RunCeiling::from_limits`] returns for a limit that cannot bound
/// a run.
///
/// Reaching the argument parser, this is a *usage* error, which the
/// non-interactive contract reports with exit code `2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCeiling(String);

impl fmt::Display for InvalidCeiling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidCeiling {}

/// Where one run stands against its [`RunCeiling`] — what it was allowed, what
/// it has spent, and whether that is the end of the run.
///
/// Produced by [`RunCeiling::evaluate`], carried on
/// [`TurnOutcome::ceiling`](crate::run::TurnOutcome::ceiling), and emitted by
/// the non-interactive front end as the `ceiling` member of the `json` object.
///
/// # Reading it
///
/// - `schema` is [`RUN_CEILING_SCHEMA`]. Check it first, exactly as with
///   [`RUN_USAGE_SCHEMA`].
/// - `max_cost_usd` / `max_tokens` are the limits the operator set; each is
///   `None` when that limit was not set. At least one is always present,
///   because a run with neither installs no ceiling and reports none.
/// - `spent_usd` / `spent_tokens` are read straight off the same
///   [`RunUsage`] the run reports, so the two objects can never disagree.
///   `spent_usd` is `None` for an unpriced model — *not priced*, never *free*.
/// - `stopped` is the decision: `true` means the run took no further turn.
/// - `reason` is the one sentence naming which limit stopped it, present
///   exactly when `stopped` is.
#[derive(Debug, Clone, Serialize)]
pub struct CeilingStatus {
    /// Schema identity of this shape — always [`RUN_CEILING_SCHEMA`].
    pub schema: &'static str,
    /// The money limit the operator set, in USD, when they set one.
    pub max_cost_usd: Option<f64>,
    /// The token limit the operator set, when they set one.
    pub max_tokens: Option<u64>,
    /// Estimated USD spent so far, or `None` for an unpriced model.
    pub spent_usd: Option<f64>,
    /// Billed tokens spent so far, across every tier.
    pub spent_tokens: u64,
    /// Whether the run stops here.
    pub stopped: bool,
    /// Which limit stopped it, present exactly when `stopped` is.
    pub reason: Option<String>,
}

impl CeilingStatus {
    /// One line stating what the run spent against what it was allowed.
    ///
    /// Reported at the stop, so the operator never has to reconstruct the
    /// comparison from two separate numbers.
    #[must_use]
    pub fn statement(&self) -> String {
        let mut parts = Vec::new();
        if let Some(limit) = self.max_cost_usd {
            let spent = self
                .spent_usd
                .map_or_else(|| "an unpriced amount".to_string(), |v| format!("${v:.4}"));
            parts.push(format!("{spent} of ${limit:.4} allowed"));
        }
        if let Some(limit) = self.max_tokens {
            parts.push(format!("{} tokens of {limit} allowed", self.spent_tokens));
        }
        if parts.is_empty() {
            // Unreachable through `from_limits`, which refuses an empty
            // ceiling; stated rather than unwrapped so the renderer has no
            // panic in it.
            return "no spend ceiling was set".to_string();
        }
        parts.join("; ")
    }

    /// How much of the ceiling the run has used, as a fraction of the
    /// tightest limit set — `1.0` is exactly at it.
    ///
    /// What a front end watches to warn that a run is *approaching* its limit
    /// rather than only announcing the stop. An unpriced model under a money
    /// ceiling reports `1.0`: it cannot be shown to have room left.
    #[must_use]
    pub fn used_fraction(&self) -> Option<f64> {
        let cost = match (self.max_cost_usd, self.spent_usd) {
            (Some(limit), Some(spent)) if limit > 0.0 => Some(spent / limit),
            (Some(_), None) => Some(1.0),
            _ => None,
        };
        let tokens = self
            .max_tokens
            .filter(|limit| *limit > 0)
            .map(|limit| self.spent_tokens as f64 / limit as f64);
        match (cost, tokens) {
            (Some(cost), Some(tokens)) => Some(cost.max(tokens)),
            (cost, tokens) => cost.or(tokens),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store that answers only from
    /// [`polaris::models::pricing::fallback_table`] — deterministic dollar
    /// amounts with no network access, per the [`PriceStore`] testing policy.
    fn fallback_only_pricing_store() -> PriceStore {
        PriceStore::builtin()
    }

    /// An empty meter backed by the deterministic fallback-only store, for
    /// every test that reads a dollar amount off it.
    fn test_meter() -> TokenMeter {
        TokenMeter::new().with_pricing_store(fallback_only_pricing_store())
    }

    #[test]
    fn add_accumulates_through_shared_clones() {
        let meter = test_meter();
        let clone = meter.clone();
        meter.add(100, 20);
        clone.add(50, 5);
        assert_eq!(meter.input_tokens(), 150);
        assert_eq!(meter.output_tokens(), 25);
        assert_eq!(meter.total_tokens(), 175);
    }

    #[test]
    fn cost_uses_family_price() {
        let meter = test_meter();
        meter.add(1_000_000, 1_000_000);
        // Sonnet: $3 in + $15 out per 1M.
        let cost = meter
            .estimated_cost_usd("anthropic/claude-sonnet-4-6")
            .unwrap();
        assert!((cost - 18.0).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn mixed_model_steps_are_priced_at_their_own_rates_after_merge() {
        let run = test_meter();
        run.add(1_000_000, 0);
        run.add_for_model("anthropic/claude-haiku-4-5-20251001", 1_000_000, 0, 0, 0);
        let session = test_meter();
        session.merge_from(&run);
        let usage = session.snapshot("anthropic/claude-sonnet-5");
        assert_eq!(usage.input_tokens, 2_000_000);
        assert!((usage.estimated_cost_usd.unwrap() - 3.0).abs() < 1e-9);
        assert_eq!(usage.model_steps.len(), 1);
        assert_eq!(
            usage.model_steps[0].model_id,
            "anthropic/claude-haiku-4-5-20251001"
        );
        assert!((usage.model_steps[0].estimated_cost_usd.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn sonnet_5_uses_its_own_price() {
        let meter = test_meter();
        meter.add(1_000_000, 1_000_000);
        // Sonnet 5: $2 in + $10 out per 1M.
        let cost = meter
            .estimated_cost_usd("anthropic/claude-sonnet-5")
            .unwrap();
        assert!((cost - 12.0).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn cache_tiers_are_billed_at_a_discount() {
        let meter = test_meter();
        // 1M cache-read + 1M cache-write on Sonnet ($3/M input base).
        meter.add_cache(1_000_000, 1_000_000);
        let cost = meter
            .estimated_cost_usd("anthropic/claude-sonnet-4-6")
            .unwrap();
        // read = 0.1×$3 = $0.30; write = 1.25×$3 = $3.75; total = $4.05.
        assert!((cost - 4.05).abs() < 1e-9, "got {cost}");
        // Cached tokens count toward the total.
        assert_eq!(meter.total_tokens(), 2_000_000);
        assert_eq!(meter.cache_read_tokens(), 1_000_000);
    }

    #[test]
    fn cache_read_is_ten_times_cheaper_than_fresh_input() {
        let cached = test_meter();
        cached.add_cache(1_000_000, 0);
        let fresh = test_meter();
        fresh.add(1_000_000, 0);
        let model = "anthropic/claude-sonnet-4-6";
        let cached_cost = cached.estimated_cost_usd(model).unwrap();
        let fresh_cost = fresh.estimated_cost_usd(model).unwrap();
        assert!((fresh_cost - 10.0 * cached_cost).abs() < 1e-9);
    }

    #[test]
    fn unpriced_model_has_no_cost() {
        let meter = test_meter();
        meter.add(10, 10);
        assert!(meter.estimated_cost_usd("some/unknown-model").is_none());
        let summary = meter.summary("some/unknown-model", 4096);
        assert!(summary.contains("n/a"));
        // Renders over multiple lines for readability.
        assert!(
            summary.lines().count() >= 4,
            "expected multi-line: {summary:?}"
        );
    }

    /// The interactive summary states the output limit the session ran under.
    #[test]
    fn summary_states_the_output_token_limit() {
        let meter = test_meter();
        assert!(
            meter
                .summary("anthropic/claude-sonnet-4-6", 32_000)
                .contains("max output:      32000 tok/response"),
            "summary should state the configured limit"
        );
        assert!(
            meter
                .summary("anthropic/claude-sonnet-4-6", 4096)
                .contains("max output:       4096 tok/response"),
            "summary should state the default limit"
        );
    }

    #[test]
    fn price_lookup_is_case_insensitive_and_strips_the_provider_prefix() {
        let store = fallback_only_pricing_store();
        assert_eq!(
            price_for(&store, "Anthropic/Claude-OPUS-4")
                .unwrap()
                .output_per_mtok,
            75.0
        );
        assert_eq!(
            price_for(&store, "claude-haiku-4-5")
                .unwrap()
                .input_per_mtok,
            1.0
        );
        assert_eq!(
            price_for(&store, "claude-sonnet-4-6")
                .unwrap()
                .input_per_mtok,
            3.0
        );
    }

    #[test]
    fn price_lookup_normalizes_a_bedrock_cross_region_model_id() {
        // The shape this crate's own `--model` flag actually sees on Bedrock:
        // a cross-region-inference prefix and no bare `claude-sonnet-5` row
        // to match verbatim. Without the same normalization the Bedrock
        // provider itself applies, this id prices as "unknown".
        let store = fallback_only_pricing_store();
        assert_eq!(
            price_for(&store, "aws.bedrock/global.anthropic.claude-sonnet-5")
                .unwrap()
                .input_per_mtok,
            2.0
        );
    }

    const SONNET: &str = "anthropic/claude-sonnet-4-6";
    const UNPRICED: &str = "some/unknown-model";

    fn ceiling(cost: Option<f64>, tokens: Option<u64>) -> RunCeiling {
        RunCeiling::from_limits(cost, tokens)
            .expect("valid limits")
            .expect("at least one limit")
    }

    #[test]
    fn no_limits_means_no_ceiling() {
        assert_eq!(RunCeiling::from_limits(None, None), Ok(None));
    }

    #[test]
    fn limits_that_cannot_bound_a_run_are_rejected() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                RunCeiling::from_limits(Some(bad), None).is_err(),
                "expected {bad} to be refused"
            );
        }
        assert!(RunCeiling::from_limits(None, Some(0)).is_err());
        // A positive limit of either kind is accepted.
        assert!(RunCeiling::from_limits(Some(0.01), None).is_ok());
        assert!(RunCeiling::from_limits(None, Some(1)).is_ok());
    }

    #[test]
    fn a_run_under_its_money_limit_keeps_going() {
        let meter = test_meter();
        meter.add(100_000, 10_000); // $0.30 + $0.15 = $0.45
        let status = ceiling(Some(1.00), None).evaluate(&meter.snapshot(SONNET));
        assert!(!status.stopped);
        assert_eq!(status.reason, None);
        assert_eq!(status.spent_tokens, 110_000);
        assert!((status.used_fraction().unwrap() - 0.45).abs() < 1e-9);
    }

    #[test]
    fn reaching_the_money_limit_stops_and_names_both_numbers() {
        let meter = test_meter();
        meter.add(100_000, 10_000); // $0.45
        let status = ceiling(Some(0.45), None).evaluate(&meter.snapshot(SONNET));
        assert!(status.stopped, "the limit is inclusive");
        let reason = status.reason.clone().unwrap();
        assert!(reason.contains("$0.4500"), "{reason}");
        assert!(reason.contains("of the $0.4500 allowed"), "{reason}");
        assert_eq!(status.statement(), "$0.4500 of $0.4500 allowed");
    }

    #[test]
    fn reaching_the_token_limit_stops_and_names_both_numbers() {
        let meter = test_meter();
        meter.add_cache(900, 100);
        meter.add(10, 0);
        let status = ceiling(None, Some(1_000)).evaluate(&meter.snapshot(SONNET));
        assert!(status.stopped);
        assert_eq!(status.spent_tokens, 1_010, "cache tiers count too");
        assert_eq!(
            status.reason.as_deref(),
            Some("spent 1010 tokens of the 1000 allowed")
        );
        assert_eq!(status.statement(), "1010 tokens of 1000 allowed");
    }

    #[test]
    fn an_unpriced_model_under_a_money_ceiling_is_stopped_not_unlimited() {
        let meter = test_meter();
        meter.add(10, 10);
        let snapshot = meter.snapshot(UNPRICED);
        assert_eq!(snapshot.estimated_cost_usd, None, "not priced, not free");

        let status = ceiling(Some(1_000.0), None).evaluate(&snapshot);
        assert!(status.stopped, "unpriced must never mean unlimited");
        assert!(status.reason.as_deref().unwrap().contains("unpriced"));
        assert_eq!(status.spent_usd, None);
        assert_eq!(
            status.statement(),
            "an unpriced amount of $1000.0000 allowed"
        );
        assert_eq!(status.used_fraction(), Some(1.0));

        // The same rule, stated up front for the operator.
        let store = fallback_only_pricing_store();
        let why = ceiling(Some(1_000.0), None).unenforceable_for(&store, UNPRICED);
        assert!(why.unwrap().contains("no price is known"));
        assert_eq!(
            ceiling(Some(1_000.0), None).unenforceable_for(&store, SONNET),
            None
        );
    }

    #[test]
    fn a_token_ceiling_is_enforceable_on_an_unpriced_model() {
        let meter = test_meter();
        meter.add(10, 10);
        assert_eq!(
            ceiling(None, Some(100)).unenforceable_for(&fallback_only_pricing_store(), UNPRICED),
            None
        );
        let status = ceiling(None, Some(100)).evaluate(&meter.snapshot(UNPRICED));
        assert!(!status.stopped, "tokens are counted without a price");
        assert_eq!(status.statement(), "20 tokens of 100 allowed");
    }

    #[test]
    fn either_limit_alone_stops_a_run_with_both_set() {
        let meter = test_meter();
        meter.add(1_000_000, 0); // $3.00, 1M tokens
        let snapshot = meter.snapshot(SONNET);

        let money_first = ceiling(Some(1.0), Some(10_000_000)).evaluate(&snapshot);
        assert!(money_first.reason.unwrap().contains("$3.0000"));

        let tokens_first = ceiling(Some(100.0), Some(1_000)).evaluate(&snapshot);
        assert!(tokens_first.reason.unwrap().contains("tokens"));

        let neither = ceiling(Some(100.0), Some(10_000_000)).evaluate(&snapshot);
        assert!(!neither.stopped);
        assert_eq!(
            neither.statement(),
            "$3.0000 of $100.0000 allowed; 1000000 tokens of 10000000 allowed"
        );
        // The tightest of the two drives the approach warning.
        assert!((neither.used_fraction().unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn the_ceiling_carries_its_own_schema() {
        let meter = test_meter();
        let status = ceiling(None, Some(10)).evaluate(&meter.snapshot(SONNET));
        assert_eq!(status.schema, RUN_CEILING_SCHEMA);
        assert_ne!(
            RUN_CEILING_SCHEMA, RUN_USAGE_SCHEMA,
            "the limit and the measurement are separate contracts"
        );
    }

    fn known_request() -> LlmRequest {
        use polaris::models::llm::{CacheControl, Message};
        LlmRequest {
            messages: vec![Message::user("word".repeat(27_000))],
            cache: CacheControl::prefix().with_breakpoints([0]),
            max_output_tokens: std::num::NonZeroU32::new(1_000),
            ..Default::default()
        }
    }

    fn known_usage(cached: bool) -> polaris::models::llm::Usage {
        polaris::models::llm::Usage {
            input_tokens: Some(0),
            output_tokens: Some(1_000),
            cache_read_tokens: Some(if cached { 27_000 } else { 0 }),
            cache_creation_tokens: Some(if cached { 0 } else { 27_000 }),
            ..Default::default()
        }
    }

    #[test]
    fn reservation_is_close_to_known_cold_and_cached_bills() {
        // Scripted billing fixture: 108k text bytes, 27k input tokens, 1k output.
        // No live token count is claimed. Cold reserve <= 1.5x this bill;
        // warm reserve <= 3x (includes density/framing and cache-miss margins).
        let meter = test_meter();
        meter.set_task_cost_cap(1.5).unwrap();
        let request = known_request();
        let price = price_for(&fallback_only_pricing_store(), "claude-sonnet-5").unwrap();
        let cold = meter
            .reserve_request("claude-sonnet-5", &request, 1_000)
            .unwrap()
            .unwrap();
        let cold_bill = price.cost_usd(0, 1_000, 0, 27_000);
        assert!(cold.reserved >= cold_bill && cold.reserved <= 1.5 * cold_bill);
        let cold_reserve = cold.reserved;
        cold.settle(&known_usage(false));
        let warm = meter
            .reserve_request("claude-sonnet-5", &request, 1_000)
            .unwrap()
            .unwrap();
        let warm_bill = price.cost_usd(0, 1_000, 27_000, 0);
        assert!(warm.reserved >= warm_bill && warm.reserved <= 3.0 * warm_bill);
        assert!(warm.reserved < cold_reserve / 2.0);
        warm.settle(&known_usage(true));
        let state = meter.inner.cost_cap.lock().unwrap();
        assert!((state.as_ref().unwrap().spent - cold_bill - warm_bill).abs() < 1e-9);
    }

    #[test]
    fn only_confirmed_unchanged_recent_prefixes_get_read_pricing() {
        use polaris::models::llm::{CacheControl, Message, ToolChoice, ToolDefinition};
        let meter = test_meter();
        meter.set_task_cost_cap(10.0).unwrap();
        let request = known_request();
        let cold = meter
            .reserve_request("claude-sonnet-5", &request, 1_000)
            .unwrap()
            .unwrap();
        let cold_reserve = cold.reserved;
        drop(cold); // A cancelled/failed call proves no cache write.
        let still_cold = meter
            .reserve_request("claude-sonnet-5", &request, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(still_cold.reserved, cold_reserve);
        still_cold.settle(&known_usage(false));
        let mut appended = request.clone();
        appended.messages.push(Message::user("next"));
        appended.cache = CacheControl::prefix().with_breakpoints([1]);
        assert!(
            meter
                .reserve_request("claude-sonnet-5", &appended, 1_000)
                .unwrap()
                .unwrap()
                .reserved
                < cold_reserve
        );
        for variant in 0..5 {
            let mut changed = request.clone();
            match variant {
                0 => changed.messages[0] = Message::user("edit".repeat(27_000)),
                1 => changed.system = Some("changed".into()),
                2 => {
                    changed.tools = Some(vec![ToolDefinition::new(
                        "new",
                        "new",
                        serde_json::json!({}),
                    )])
                }
                3 => changed.tool_choice = Some(ToolChoice::Auto),
                _ => changed.cache = CacheControl::default(),
            }
            let permit = meter
                .reserve_request("claude-sonnet-5", &changed, 1_000)
                .unwrap()
                .unwrap();
            assert_eq!(
                permit.reserved,
                permit
                    .price
                    .cost_usd(0, 1_000, 0, request_input_bound(&changed))
            );
        }
        let other_model = meter
            .reserve_request("claude-haiku-4-5", &request, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(
            other_model.reserved,
            other_model
                .price
                .cost_usd(0, 1_000, 0, request_input_bound(&request))
        );
        drop(other_model);
        meter
            .inner
            .cost_cap
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .cached
            .get_mut("claude-sonnet-5")
            .unwrap()
            .sent_at -= Duration::from_secs(241);
        assert_eq!(
            meter
                .reserve_request("claude-sonnet-5", &request, 1_000)
                .unwrap()
                .unwrap()
                .reserved,
            cold_reserve
        );
    }

    #[test]
    fn actual_bill_above_cap_stops_the_next_request() {
        let meter = test_meter();
        meter.set_task_cost_cap(0.11).unwrap();
        let permit = meter
            .reserve_request("claude-sonnet-5", &known_request(), 1_000)
            .unwrap()
            .unwrap();
        // Simulate unusually dense input: actual usage is authoritative even
        // when it exceeds the local estimate, and cannot fund another call.
        permit.settle(&polaris::models::llm::Usage {
            input_tokens: Some(100_000),
            output_tokens: Some(1_000),
            ..Default::default()
        });
        assert!(
            meter
                .reserve_request("claude-sonnet-5", &known_request(), 1_000)
                .is_err()
        );
        assert!(meter.cost_cap_stopped());
    }

    #[test]
    fn task_cap_sums_models_and_refuses_before_the_next_call() {
        use polaris::models::llm::Usage;
        let meter = test_meter();
        meter.set_task_cost_cap(0.01).unwrap();
        let sonnet = meter
            .reserve_model_call("claude-sonnet-5", 1, 800)
            .unwrap()
            .unwrap();
        sonnet.settle(&Usage {
            input_tokens: Some(0),
            output_tokens: Some(800),
            ..Usage::default()
        });
        assert!(meter.take_cost_notice());
        assert!(!meter.take_cost_notice());
        let haiku = meter
            .reserve_model_call("claude-haiku-4-5", 1, 200)
            .unwrap()
            .unwrap();
        haiku.settle(&Usage {
            input_tokens: Some(0),
            output_tokens: Some(200),
            ..Usage::default()
        });
        assert!(
            meter
                .reserve_model_call("claude-haiku-4-5", 1, 500)
                .is_err()
        );
        assert!(meter.cost_cap_stopped());
        let store = fallback_only_pricing_store();
        assert_eq!(
            price_for(&store, "claude-sonnet-5")
                .unwrap()
                .output_per_mtok,
            10.0
        );
        assert_eq!(
            price_for(&store, "claude-haiku-4-5")
                .unwrap()
                .output_per_mtok,
            5.0
        );
    }

    #[test]
    fn cancelled_call_releases_its_reservation() {
        let meter = test_meter();
        meter.set_task_cost_cap(0.01).unwrap();
        let permit = meter
            .reserve_model_call("claude-sonnet-5", 1, 800)
            .unwrap()
            .unwrap();
        assert!(meter.reserve_model_call("claude-sonnet-5", 1, 800).is_err());
        drop(permit);
        assert!(meter.reserve_model_call("claude-sonnet-5", 1, 800).is_ok());
    }
}
