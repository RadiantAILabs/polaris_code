//! Per-model LLM token-usage accumulator + USD pricing.
//!
//! `Spend` is a [`GlobalResource`] that records `(input_tokens, output_tokens,
//! calls)` per model id every time the extractor receives a response. Pricing
//! is held alongside the counts so a single `total_usd()` call answers the
//! "how much have we spent so far" question without any per-line work in the
//! hot path.
//!
//! The numbers come from the provider's response (`LlmResponse.usage`) — they
//! are exact and authoritative, unlike the pre-call estimate that
//! [`polaris::models::tokenizer::TiktokenCounter`] would produce.

use parking_lot::RwLock;
use polaris::system::resource::GlobalResource;
use std::collections::HashMap;

/// USD price for one million input / output tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPrice {
    /// Cost in USD per million input tokens.
    pub input_per_mtok: f64,
    /// Cost in USD per million output tokens.
    pub output_per_mtok: f64,
}

impl ModelPrice {
    /// Construct a price from the two rate components.
    #[must_use]
    pub fn new(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
        }
    }

    /// USD cost for a single call given input + output token counts.
    #[must_use]
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let input = input_tokens as f64;
        #[allow(clippy::cast_precision_loss)]
        let output = output_tokens as f64;
        (input / 1_000_000.0) * self.input_per_mtok + (output / 1_000_000.0) * self.output_per_mtok
    }
}

/// Lookup table from polaris model id (e.g. `"anthropic/claude-haiku-4-5"`)
/// to USD-per-MTok rates.
///
/// The baked-in defaults reflect Anthropic's public list prices as of the
/// crate's last release. They are *not* fetched live — verify against
/// <https://www.anthropic.com/pricing#api> before relying on the dollar
/// figure for billing. Replace the table or individual entries with
/// [`Pricing::empty`] / [`Pricing::with`] when you need a different source
/// of truth.
#[derive(Clone, Debug)]
pub struct Pricing {
    table: HashMap<String, ModelPrice>,
}

impl Pricing {
    /// Empty table — every recorded call will report `None` for cost. Use
    /// when you want to opt out of the baked-in defaults entirely.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            table: HashMap::new(),
        }
    }

    /// Anthropic public list prices for the Claude 4.x family. Keys match
    /// the polaris model ids used by `AnthropicPlugin`.
    #[must_use]
    pub fn anthropic_defaults() -> Self {
        let mut table = HashMap::new();
        table.insert(
            "anthropic/claude-haiku-4-5".to_owned(),
            ModelPrice::new(1.0, 5.0),
        );
        table.insert(
            "anthropic/claude-sonnet-4-6".to_owned(),
            ModelPrice::new(3.0, 15.0),
        );
        table.insert(
            "anthropic/claude-opus-4-7".to_owned(),
            ModelPrice::new(15.0, 75.0),
        );
        Self { table }
    }

    /// Insert or overwrite the price for `model_id`.
    #[must_use]
    pub fn with(mut self, model_id: impl Into<String>, price: ModelPrice) -> Self {
        self.table.insert(model_id.into(), price);
        self
    }

    /// Look up the price for `model_id`. `None` means the model is unpriced
    /// — callers should treat that as "cost unknown", not "cost zero".
    #[must_use]
    pub fn get(&self, model_id: &str) -> Option<ModelPrice> {
        self.table.get(model_id).copied()
    }
}

impl Default for Pricing {
    fn default() -> Self {
        Self::anthropic_defaults()
    }
}

/// Token + call counters for one model id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelUsage {
    /// Number of LLM calls recorded against this model.
    pub calls: u64,
    /// Sum of `input_tokens` over those calls.
    pub input_tokens: u64,
    /// Sum of `output_tokens` over those calls.
    pub output_tokens: u64,
}

/// One row of [`Spend::report`] — usage for a model plus the USD cost if a
/// price was registered.
#[derive(Clone, Debug)]
pub struct ModelReport {
    /// Polaris model id (e.g. `"anthropic/claude-haiku-4-5"`).
    pub model_id: String,
    /// Accumulated usage for the model.
    pub usage: ModelUsage,
    /// USD cost computed from the registered [`ModelPrice`]. `None` when
    /// the model has no entry in the active [`Pricing`] table.
    pub cost_usd: Option<f64>,
}

#[derive(Debug)]
struct SpendInner {
    pricing: Pricing,
    per_model: HashMap<String, ModelUsage>,
}

/// Per-model LLM token-usage accumulator with USD pricing.
///
/// Use this when a system needs to record what an LLM call cost or report
/// spend so far. Call [`Spend::record`] with the model id and the exact
/// `(input, output)` token counts from the provider response after every
/// call; read [`Spend::report`] / [`Spend::total_usd`] to render a cost
/// summary. Pricing lives alongside the counts, so the dollar figure is a
/// pure read with no per-line work in the hot path.
///
/// # Scope
///
/// `Global` — every per-trace session makes its own LLM call, but the running
/// total must aggregate across all of them, so a single shared accumulator is
/// the natural home.
///
/// # Provided by
///
/// [`crate::ExtractionPlugin`] and
/// [`crate::plugins::oxigraph_extraction::OxigraphExtractionPlugin`] each
/// insert `Spend::with_pricing(...)` in `build()`, defaulting to
/// [`Pricing::anthropic_defaults`] unless overridden via the plugin's
/// `with_pricing` builder.
///
/// # Access
///
/// Consume as `Res<Spend>`. `ResMut<T>` is not used for globals in this crate;
/// [`Spend::record`] mutates through the interior [`parking_lot::RwLock`] and
/// so takes `&self`.
///
/// Mutation contract: **monotonic accumulation** — each [`Spend::record`]
/// bumps the per-model `calls`, `input_tokens`, and `output_tokens` counters
/// (a provider that reports no counts still increments `calls`). Counters only
/// grow; there is no reset.
///
/// # Alternatives
///
/// | Alternative | When |
/// | --- | --- |
/// | _none_ | This is the only spend/usage accumulator in the crate. |
///
/// # Example system
///
/// ```no_run
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::Spend;
///
/// /// Log running spend and any models that were recorded without a price.
/// #[system]
/// async fn log_spend(spend: Res<Spend>) {
///     let (total, unpriced) = spend.total_usd();
///     tracing::info!(total_usd = total, ?unpriced, "llm spend so far");
/// }
/// ```
#[derive(Debug)]
pub struct Spend {
    inner: RwLock<SpendInner>,
}

impl Spend {
    /// Create a `Spend` using the default Anthropic pricing table.
    #[must_use]
    pub fn new() -> Self {
        Self::with_pricing(Pricing::default())
    }

    /// Create a `Spend` with a caller-supplied pricing table.
    #[must_use]
    pub fn with_pricing(pricing: Pricing) -> Self {
        Self {
            inner: RwLock::new(SpendInner {
                pricing,
                per_model: HashMap::new(),
            }),
        }
    }

    /// Record one LLM call's token usage. Token fields are `Option<u64>` on
    /// `LlmResponse.usage`, so we accept the `None` case by treating it as
    /// zero — a provider that doesn't report counts simply doesn't move the
    /// counter, but the call is still recorded.
    pub fn record(&self, model_id: &str, input_tokens: Option<u64>, output_tokens: Option<u64>) {
        let mut g = self.inner.write();
        let entry = g.per_model.entry(model_id.to_owned()).or_default();
        entry.calls += 1;
        entry.input_tokens += input_tokens.unwrap_or(0);
        entry.output_tokens += output_tokens.unwrap_or(0);
    }

    /// Snapshot every recorded model along with its computed USD cost.
    ///
    /// Rows are sorted by `model_id` for stable display. Models whose ids
    /// are not in the active [`Pricing`] table appear with `cost_usd: None`.
    #[must_use]
    pub fn report(&self) -> Vec<ModelReport> {
        let g = self.inner.read();
        let mut rows: Vec<ModelReport> = g
            .per_model
            .iter()
            .map(|(model_id, usage)| {
                let cost_usd = g
                    .pricing
                    .get(model_id)
                    .map(|p| p.cost(usage.input_tokens, usage.output_tokens));
                ModelReport {
                    model_id: model_id.clone(),
                    usage: *usage,
                    cost_usd,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        rows
    }

    /// Total USD across all priced models.
    ///
    /// The returned `(total, unpriced)` pair makes missing-price calls
    /// visible: `unpriced` lists every model id that was recorded but had
    /// no entry in the [`Pricing`] table, so a CLI can warn instead of
    /// silently underreporting.
    #[must_use]
    pub fn total_usd(&self) -> (f64, Vec<String>) {
        let mut total = 0.0;
        let mut unpriced = Vec::new();
        for row in self.report() {
            match row.cost_usd {
                Some(c) => total += c,
                None if row.usage.calls > 0 => unpriced.push(row.model_id),
                None => {}
            }
        }
        (total, unpriced)
    }

    /// Number of distinct model ids that have been recorded.
    #[must_use]
    pub fn model_count(&self) -> usize {
        self.inner.read().per_model.len()
    }
}

impl Default for Spend {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalResource for Spend {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_accumulates_per_model() {
        let s = Spend::new();
        s.record("anthropic/claude-haiku-4-5", Some(100), Some(50));
        s.record("anthropic/claude-haiku-4-5", Some(200), Some(75));
        let rows = s.report();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].usage.calls, 2);
        assert_eq!(rows[0].usage.input_tokens, 300);
        assert_eq!(rows[0].usage.output_tokens, 125);
    }

    #[test]
    fn cost_uses_default_anthropic_table() {
        let s = Spend::new();
        // 1M input tokens at $1/MTok + 1M output at $5/MTok = $6.
        s.record(
            "anthropic/claude-haiku-4-5",
            Some(1_000_000),
            Some(1_000_000),
        );
        let (total, unpriced) = s.total_usd();
        assert!((total - 6.0).abs() < 1e-9, "got {total}");
        assert!(unpriced.is_empty());
    }

    #[test]
    fn unknown_model_surfaces_in_unpriced() {
        let s = Spend::new();
        s.record("custom/local-model", Some(10), Some(10));
        let (total, unpriced) = s.total_usd();
        assert!((total - 0.0).abs() < 1e-9);
        assert_eq!(unpriced, vec!["custom/local-model".to_string()]);
    }

    #[test]
    fn missing_usage_fields_default_to_zero() {
        let s = Spend::new();
        s.record("anthropic/claude-haiku-4-5", None, None);
        let rows = s.report();
        assert_eq!(rows[0].usage.calls, 1);
        assert_eq!(rows[0].usage.input_tokens, 0);
        assert_eq!(rows[0].usage.output_tokens, 0);
    }

    #[test]
    fn custom_pricing_overrides_defaults() {
        let pricing = Pricing::empty().with("custom/m", ModelPrice::new(2.0, 4.0));
        let s = Spend::with_pricing(pricing);
        s.record("custom/m", Some(500_000), Some(250_000));
        // 0.5 * $2 + 0.25 * $4 = $1 + $1 = $2.
        let (total, unpriced) = s.total_usd();
        assert!((total - 2.0).abs() < 1e-9, "got {total}");
        assert!(unpriced.is_empty());
    }
}
