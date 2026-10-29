//! Conversation context — the canonical message history and its projection.
//!
//! `Vec<Message>` is the source of truth for provider fidelity (`tool_use` /
//! `tool_result` ordering). The Session `ABox` graph is a *projection* over this,
//! not a replacement; see [`super::trajectory`].
//!
//! The agent loop never reads `messages` directly: it appends with [`ContextManager::push`]
//! and asks for the live window with [`ContextManager::project`], which delegates
//! to a held [`ContextStrategy`]. [`LinearWindow`] is the default recency
//! window with message and token caps; callers can install another strategy
//! through [`ContextManager::with_strategy`].

use std::sync::Arc;

use polaris::models::llm::{AssistantBlock, Message, ToolResultContent, UserBlock};
use polaris::plugins::Storable;
use polaris::system::resource::LocalResource;
use serde::{Deserialize, Serialize};

use super::policy::RetrievalPolicy;

/// A projected live window plus the cache-breakpoint markers a cache-aware
/// strategy wants placed on it (consumed by the agent's `act` system, which
/// translates them into [`LlmRequestBuilder`](polaris::models::llm::LlmRequestBuilder)
/// cache verbs). Strategies that
/// don't care about caching leave [`cache_prefix`](Self::cache_prefix) false and
/// [`breakpoints`](Self::breakpoints) empty.
#[derive(Debug, Clone, Default)]
pub struct ProjectedWindow {
    /// The messages to send this turn (possibly with compacted tool results).
    pub messages: Vec<Message>,
    /// Whether to cache the stable prefix (system prompt + tool definitions).
    pub cache_prefix: bool,
    /// Indices into [`messages`](Self::messages) after which to place a cache
    /// breakpoint.
    pub breakpoints: Vec<usize>,
    /// Estimated token mass of [`messages`](Self::messages).
    pub estimated_tokens: u64,
    /// The active projection/compaction budget, when policy declares one.
    pub token_budget: Option<u64>,
    /// Whether this projection compacted old tool results.
    pub compacted: bool,
}

/// The replaceable projection behavior: how the live window is derived from the
/// canonical history. The trait lives on the *behavior*, never on the persisted
/// state — [`ContextManager`] holds an `Arc<dyn ContextStrategy>` and delegates
/// [`ContextManager::project`] to it.
pub trait ContextStrategy: std::fmt::Debug + Send + Sync {
    /// Project the window to send this turn from the canonical history in `ctx`,
    /// honoring `retrieval`'s caps, and declare any cache breakpoints.
    fn project(&self, ctx: &ContextManager, retrieval: &RetrievalPolicy) -> ProjectedWindow;
}

/// The v1 default strategy: a recency window snapped to clean user-turn
/// boundaries, bounded by the policy's message-count and token caps. Does not
/// request prompt caching — it slides the window start as the cap trips, which
/// would invalidate a cached prefix every turn. Use [`CacheAwareWindow`] for
/// cache-stable projection.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinearWindow;

impl ContextStrategy for LinearWindow {
    fn project(&self, ctx: &ContextManager, r: &RetrievalPolicy) -> ProjectedWindow {
        let ratio = ctx.token_ratio();
        let messages = provider_valid(ctx.window(r.max_messages, r.max_tokens, ratio));
        ProjectedWindow {
            estimated_tokens: estimated_tokens(&messages, ratio),
            token_budget: r.max_tokens.map(|value| value as u64),
            messages,
            cache_prefix: false,
            breakpoints: Vec::new(),
            compacted: false,
        }
    }
}

/// A cache-aware strategy that keeps the message prefix **byte-stable** across
/// turns so provider prompt caching actually hits.
///
/// Where [`LinearWindow`] slides the window start whenever the token cap trips —
/// invalidating the cached prefix every turn — this strategy holds the start
/// stable (honoring only the message-count cap, a rare jump) and absorbs routine
/// token pressure by **compacting**: stubbing the bodies of the oldest tool
/// results in batches (`retrieval.compact_*`). Because compaction is a
/// deterministic function of the history and the policy, the same prefix is
/// produced turn over turn until a batch threshold or count cap forces a single
/// "jump", after which it is stable again.
///
/// It requests caching of the stable prefix (system + tools) plus a rolling
/// breakpoint near the window end (the incremental-caching pattern) and a stable
/// breakpoint at the active-turn boundary, which is unchanged across a turn's
/// tool round-trips.
#[derive(Debug, Default, Clone, Copy)]
pub struct CacheAwareWindow;

impl ContextStrategy for CacheAwareWindow {
    fn project(&self, ctx: &ContextManager, r: &RetrievalPolicy) -> ProjectedWindow {
        let ratio = ctx.token_ratio();
        // Stable base: honor the message-count cap only. Routine token pressure
        // is handled by compaction below, not by sliding the start (which would
        // break the cached prefix).
        let mut messages = ctx.window(r.max_messages, None, ratio);
        let before = message_chars(&messages);

        compact_tool_results(
            &mut messages,
            r.compact_after_tokens,
            r.compact_batch.max(1),
            r.compact_keep_recent,
            ratio,
        );
        // Measure compaction before the provider-validity guard runs: that guard
        // may append a synthetic tool result, which would otherwise mask a
        // compaction that did happen.
        let compacted = message_chars(&messages) < before;
        let messages = provider_valid(messages);

        let breakpoints = cache_breakpoints(&messages);
        ProjectedWindow {
            estimated_tokens: estimated_tokens(&messages, ratio),
            token_budget: r.compact_after_tokens.map(|value| value as u64),
            compacted,
            messages,
            cache_prefix: true,
            breakpoints,
        }
    }
}

/// Ensure a provider projection never ends with unmatched tool calls.
///
/// The live write path records an interrupted tool result before returning.
/// This projection-side guard also repairs resumed history written by an older
/// version that did not yet enforce that invariant.
fn provider_valid(mut messages: Vec<Message>) -> Vec<Message> {
    let mut pending = Vec::new();
    for message in &messages {
        match message {
            Message::Assistant { content, .. } => pending.extend(content.iter().filter_map(|b| {
                if let AssistantBlock::ToolCall(call) = b {
                    Some(call.id.clone())
                } else {
                    None
                }
            })),
            Message::User { content } => {
                for result in content.iter().filter_map(|b| {
                    if let UserBlock::ToolResult(result) = b {
                        Some(result)
                    } else {
                        None
                    }
                }) {
                    pending.retain(|id| id != &result.id);
                }
            }
        }
    }
    if !pending.is_empty() {
        messages.push(Message::User {
            content: pending
                .into_iter()
                .map(|id| {
                    UserBlock::tool_error(
                        &id,
                        ToolResultContent::Text("Interrupted before the tool completed".into()),
                    )
                })
                .collect(),
        });
    }
    messages
}

/// Estimate the token mass of a projected window at the calibrated ratio.
fn estimated_tokens(messages: &[Message], ratio: f64) -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let tokens = (message_chars(messages) as f64 * ratio).ceil() as u64;
    tokens
}

/// Choose up to two message cache breakpoints: a **stable** one at the start of
/// the active turn (unchanged while that turn's tool loop iterates) and a
/// **rolling** one at the last message (so this turn's window becomes a cache
/// read next turn). Deduplicated and returned in ascending order.
///
/// `pub(crate)`: also the breakpoint chooser
/// [`gate_blocks::configurable::LiveModelStep`](crate::cosmos::gate_blocks::configurable::LiveModelStep)
/// reuses for its own single-conversation cache-aware request, rather than
/// reimplementing the same choice.
pub(crate) fn cache_breakpoints(msgs: &[Message]) -> Vec<usize> {
    let len = msgs.len();
    if len == 0 {
        return Vec::new();
    }
    let mut points = Vec::with_capacity(2);
    // Stable: the last clean user-turn boundary (start of the active turn).
    if let Some(boundary) = (0..len).rev().find(|&i| is_turn_boundary(&msgs[i]))
        && boundary < len - 1
    {
        points.push(boundary);
    }
    // Rolling: the final message.
    points.push(len - 1);
    points.dedup();
    points
}

/// A tool result whose body we may stub: a `User` `ToolResult` carrying text
/// that has not already been compacted.
fn is_stubbable(msg: &Message) -> bool {
    match msg {
        Message::User { content } => content.iter().any(|b| match b {
            UserBlock::ToolResult(tr) => match &tr.content {
                ToolResultContent::Text(text) => !text.starts_with(COMPACTED_MARKER),
                ToolResultContent::Image(_) => false,
            },
            _ => false,
        }),
        Message::Assistant { .. } => false,
    }
}

/// Sentinel prefix marking an already-compacted tool result, so a later
/// projection neither re-stubs nor double-counts it.
///
/// `pub(crate)` so a reuser of [`compact_tool_results`] (currently
/// [`gate_blocks::configurable::LiveModelStep`](crate::cosmos::gate_blocks::configurable::LiveModelStep))
/// can recognize an already-stubbed result in its own tests without
/// duplicating the sentinel string.
pub(crate) const COMPACTED_MARKER: &str = "[elided ";

/// Replace every text tool-result body in `msg` with a compact stub. The stub
/// text is a pure function of the original body length, so re-projecting the
/// same history yields a byte-identical message.
fn stub_message(msg: &mut Message) {
    if let Message::User { content } = msg {
        for block in content.iter_mut() {
            if let UserBlock::ToolResult(tr) = block
                && let ToolResultContent::Text(text) = &tr.content
                && !text.starts_with(COMPACTED_MARKER)
            {
                let chars = text.chars().count();
                tr.content = ToolResultContent::Text(format!(
                    "{COMPACTED_MARKER}{chars} chars of tool output to keep the cache prefix stable]"
                ));
            }
        }
    }
}

/// Estimated token mass of `msgs` under `ratio`.
fn est_tokens(msgs: &[Message], ratio: f64) -> usize {
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let est = (ratio * message_chars(msgs) as f64) as usize;
    est
}

/// Jump-compaction: stub the oldest stubbable tool-result bodies in batches of
/// `batch` until the estimated window fits `trigger`, never touching the most
/// recent `keep_recent` messages. Returns the number of messages stubbed.
///
/// Deterministic given `(msgs, trigger, batch, keep_recent, ratio)`, which is
/// what keeps the cached prefix stable between jumps.
///
/// `pub(crate)`: [`gate_blocks::configurable::LiveModelStep`](crate::cosmos::gate_blocks::configurable::LiveModelStep)
/// reuses this directly over its own growing turn history rather than
/// reimplementing tool-result compaction a second time.
pub(crate) fn compact_tool_results(
    msgs: &mut [Message],
    trigger: Option<usize>,
    batch: usize,
    keep_recent: usize,
    ratio: f64,
) -> usize {
    let Some(trigger) = trigger else {
        return 0;
    };
    if msgs.len() <= keep_recent || est_tokens(msgs, ratio) <= trigger {
        return 0;
    }
    let cutoff = msgs.len() - keep_recent;
    let stubbable: Vec<usize> = (0..cutoff).filter(|&i| is_stubbable(&msgs[i])).collect();

    let mut stubbed = 0;
    let mut next = 0;
    while next < stubbable.len() && est_tokens(msgs, ratio) > trigger {
        for &idx in stubbable.iter().skip(next).take(batch) {
            stub_message(&mut msgs[idx]);
            stubbed += 1;
        }
        next += batch;
    }
    stubbed
}

fn default_strategy() -> Arc<dyn ContextStrategy> {
    Arc::new(LinearWindow)
}

/// Manages conversation history for the agent.
///
/// Persisted state is only the canonical [`messages`](Self::messages). The
/// calibration fields and the projection [`strategy`](ContextStrategy) are
/// runtime-only: they are skipped on serialization and re-seeded on resume.
#[derive(Debug, Clone, Serialize, Deserialize, Storable)]
#[storable(key = "CodeContextManager")]
pub struct ContextManager {
    /// The conversation message history (canonical).
    pub messages: Vec<Message>,

    /// Provider-native `input_tokens` for the most recently sent window, fed back
    /// by [`record_sent_window`](Self::record_sent_window) for token calibration.
    #[serde(skip)]
    last_prompt_tokens: Option<u64>,
    /// Character mass of the most recently sent window (paired with
    /// `last_prompt_tokens` to derive a tokens-per-char ratio).
    #[serde(skip)]
    last_window_chars: Option<usize>,
    /// The projection behavior behind [`project`](Self::project).
    #[serde(skip, default = "default_strategy")]
    strategy: Arc<dyn ContextStrategy>,
}

impl Default for ContextManager {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            last_prompt_tokens: None,
            last_window_chars: None,
            strategy: default_strategy(),
        }
    }
}

impl ContextManager {
    /// Reestablish non-persisted projection state after a native restore while
    /// preserving the restored canonical messages.
    pub fn reset_runtime(&mut self, prompt_cache: bool) {
        self.last_prompt_tokens = None;
        self.last_window_chars = None;
        self.strategy = if prompt_cache {
            Arc::new(CacheAwareWindow)
        } else {
            default_strategy()
        };
    }

    /// Create a new context seeded with an initial user request.
    #[must_use]
    pub fn new(request: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user(request)],
            ..Self::default()
        }
    }

    /// Create a context with an explicit projection strategy — the boot-time
    /// injection point (`register_local(|| ContextManager::with_strategy(...))`).
    #[must_use]
    pub fn with_strategy(strategy: Arc<dyn ContextStrategy>) -> Self {
        Self {
            strategy,
            ..Self::default()
        }
    }

    /// Append a message to the history.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Project the live window to send this turn — the loop-facing seam.
    /// Delegates to the held [`ContextStrategy`].
    #[must_use]
    pub fn project(&self, retrieval: &RetrievalPolicy) -> ProjectedWindow {
        self.strategy.project(self, retrieval)
    }

    /// Feed the provider's real `input_tokens` and the window that produced them
    /// back for token calibration. Call only on a successful provider response so
    /// a failed/retried attempt never miscalibrates the ratio.
    pub fn record_sent_window(&mut self, prompt_tokens: u64, window: &[Message]) {
        self.last_prompt_tokens = Some(prompt_tokens);
        self.last_window_chars = Some(message_chars(window));
    }

    /// A coarse turn marker derived from history length, used to stamp reified
    /// trajectory nodes. (Real `SessionInfo` turn numbers are a later refinement.)
    #[must_use]
    pub fn turn_marker(&self) -> usize {
        self.messages.len()
    }

    /// Whether any message carries tool-call or tool-result blocks.
    #[must_use]
    pub fn has_tool_blocks(&self) -> bool {
        self.messages.iter().any(|msg| match msg {
            Message::User { content } => content
                .iter()
                .any(|b| matches!(b, UserBlock::ToolResult(_))),
            Message::Assistant { content, .. } => content
                .iter()
                .any(|b| matches!(b, AssistantBlock::ToolCall(_))),
        })
    }

    /// The provider-native `input_tokens` recorded for the most recently sent
    /// window, if any. Used for retrieval-decision provenance.
    #[must_use]
    pub fn last_prompt_tokens(&self) -> Option<u64> {
        self.last_prompt_tokens
    }

    /// Tokens-per-char ratio derived from the last sent window, clamped to a sane
    /// band so the fixed system/tool-schema overhead can't inflate it on a small
    /// window. Defaults to `0.3` before the first sample.
    #[must_use]
    pub fn token_ratio(&self) -> f64 {
        match (self.last_prompt_tokens, self.last_window_chars) {
            (Some(tokens), Some(chars)) if chars > 0 => {
                #[allow(clippy::cast_precision_loss)]
                let ratio = tokens as f64 / chars as f64;
                ratio.clamp(0.2, 0.6)
            }
            _ => 0.3,
        }
    }

    /// The pure, testable projection core: a recency window over `messages`
    /// snapped to clean user-turn boundaries.
    ///
    /// 1. find boundary indices (clean user-turn starts), always including the
    ///    most recent boundary; `start = 0` if none.
    /// 2. if `max_messages`: pick the smallest boundary `b` with `len - b <= max`
    ///    (keep the most that fits); else `0`.
    /// 3. if `max_tokens`: while the estimated token mass exceeds the cap **and** a
    ///    later boundary exists, advance `start`. Never past the last boundary — the
    ///    active turn always stays (a warning is logged if it alone exceeds budget).
    /// 4. return `messages[start..]`.
    fn window(
        &self,
        max_messages: Option<usize>,
        max_tokens: Option<usize>,
        ratio: f64,
    ) -> Vec<Message> {
        let msgs = &self.messages;
        let len = msgs.len();
        if len == 0 {
            return Vec::new();
        }

        // (1) clean user-turn boundary indices, in ascending order.
        let boundaries: Vec<usize> = (0..len).filter(|&i| is_turn_boundary(&msgs[i])).collect();
        // Fall back to a single virtual boundary at 0 when none exist.
        let boundaries: Vec<usize> = if boundaries.is_empty() {
            vec![0]
        } else {
            boundaries
        };
        let last_boundary = *boundaries.last().expect("boundaries is non-empty");

        // (2) message-count cap: smallest boundary that keeps the window within cap.
        let mut start = match max_messages {
            Some(max) => boundaries
                .iter()
                .copied()
                .find(|&b| len - b <= max)
                .unwrap_or(last_boundary),
            None => boundaries[0],
        };

        // (3) token cap: advance start to a later boundary until the window fits,
        // never dropping the active (last) turn.
        if let Some(max) = max_tokens {
            while start < last_boundary {
                #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
                let est = (ratio * message_chars(&msgs[start..]) as f64) as usize;
                if est <= max {
                    break;
                }
                // advance to the next boundary strictly after `start`.
                match boundaries.iter().copied().find(|&b| b > start) {
                    Some(next) => start = next,
                    None => break,
                }
            }

            #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
            let est = (ratio * message_chars(&msgs[start..]) as f64) as usize;
            if est > max {
                tracing::warn!(
                    "context window: active turn (~{est} tokens) alone exceeds max_tokens={max}; \
                     sending it intact (summarization is an out-of-scope seam)"
                );
            }
        }

        msgs[start..].to_vec()
    }
}

/// Whether `msg` begins a clean user-input turn — a `Message::User` carrying **no**
/// `ToolResult` block. A window may start here without orphaning a `tool_result`.
fn is_turn_boundary(msg: &Message) -> bool {
    match msg {
        Message::User { content } => !content
            .iter()
            .any(|b| matches!(b, UserBlock::ToolResult(_))),
        Message::Assistant { .. } => false,
    }
}

/// What one image block contributes to the char estimate.
///
/// An image has almost no characters and a large token cost, so counting it by
/// its text — zero — would make a window holding an attachment read as free
/// and let the budget seam project far more history than the request can
/// carry. Providers bill a full-size image at roughly 1,600 tokens, and this
/// estimator converts chars to tokens with a calibrated ratio near 0.3, so
/// ~6,400 chars is the equivalent mass. This is an estimate and is documented
/// as one: the ratio is re-calibrated from observed usage after every call, so
/// a persistent error corrects itself rather than compounding.
const IMAGE_CHARS: usize = 6_400;

/// Total text mass across the blocks of `msgs` — the per-message proxy used to
/// estimate token budget. Counts user/assistant text, tool-result text, image
/// blocks at [`IMAGE_CHARS`], and the JSON of tool-call arguments.
pub(crate) fn message_chars(msgs: &[Message]) -> usize {
    msgs.iter()
        .map(|msg| match msg {
            Message::User { content } => content
                .iter()
                .map(|b| match b {
                    UserBlock::Text(t) => t.text.len(),
                    UserBlock::ToolResult(tr) => match &tr.content {
                        ToolResultContent::Text(s) => s.len(),
                        ToolResultContent::Image(_) => IMAGE_CHARS,
                    },
                    UserBlock::Image(_) => IMAGE_CHARS,
                    _ => 0,
                })
                .sum::<usize>(),
            Message::Assistant { content, .. } => content
                .iter()
                .map(|b| match b {
                    AssistantBlock::Text(t) => t.text.len(),
                    AssistantBlock::ToolCall(tc) => tc.function.arguments.to_string().len(),
                    AssistantBlock::Reasoning(_) => 0,
                })
                .sum::<usize>(),
        })
        .sum()
}

impl LocalResource for ContextManager {}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::llm::ToolCall;

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(text)
    }

    /// An assistant turn that calls a tool, followed by its tool result — the
    /// pair that must never be split.
    fn tool_exchange(id: &str, args_len: usize) -> [Message; 2] {
        let args = serde_json::json!({ "x": "a".repeat(args_len) });
        let call = Message::Assistant {
            id: None,
            content: vec![AssistantBlock::ToolCall(ToolCall::new(
                id,
                "read_file",
                args,
            ))],
        };
        let result = Message::User {
            content: vec![UserBlock::tool_result(
                id,
                ToolResultContent::Text("ok".into()),
            )],
        };
        [call, result]
    }

    fn ctx(messages: Vec<Message>) -> ContextManager {
        ContextManager {
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn short_history_is_returned_unchanged() {
        let c = ctx(vec![user("hi"), assistant("hello"), user("more")]);
        let window = c.window(Some(50), Some(120_000), 0.3);
        assert_eq!(window.len(), 3);
    }

    #[test]
    fn over_count_trims_to_a_boundary_never_an_orphan_tool_result() {
        // Build: user, (call, result) x4 — 9 messages, boundaries at 0 only among
        // the tool exchanges, plus interleaved user turns.
        let mut messages = vec![user("start")];
        for i in 0..4 {
            messages.push(user(&format!("turn {i}")));
            let [call, result] = tool_exchange(&format!("t{i}"), 1);
            messages.push(call);
            messages.push(result);
        }
        let c = ctx(messages);
        // Cap small enough to force trimming.
        let window = c.window(Some(3), None, 0.3);
        assert!(
            window.len() <= 3 + 1,
            "kept the most that fits at a boundary"
        );
        // Must start on a clean user turn, not an orphaned tool result.
        assert!(is_turn_boundary(&window[0]), "window starts on a boundary");
    }

    #[test]
    fn token_cap_shrinks_below_the_count_cap() {
        // Three user turns, the last two large. With a generous count cap but a
        // tight token cap, projection drops earlier turns.
        let messages = vec![
            user(&"a".repeat(1000)),
            user(&"b".repeat(1000)),
            user(&"c".repeat(1000)),
        ];
        let c = ctx(messages);
        // ratio 0.3 → each turn ~300 tokens. Cap at 350 keeps only the last turn.
        let window = c.window(Some(50), Some(350), 0.3);
        assert_eq!(window.len(), 1);
        assert!(is_turn_boundary(&window[0]));
    }

    #[test]
    fn oversized_last_turn_is_kept_intact() {
        let messages = vec![
            user("small"),
            user(&"x".repeat(10_000)), // active turn alone blows the budget
        ];
        let c = ctx(messages);
        let window = c.window(Some(50), Some(100), 0.3);
        // The active turn is never dropped, even though it exceeds the cap.
        assert_eq!(window.len(), 1);
        assert!(is_turn_boundary(&window[0]));
    }

    #[test]
    fn token_ratio_clamps_an_inflated_small_window_sample() {
        let mut c = ctx(vec![user("hi")]);
        // 5000 tokens reported for a 100-char window → raw ratio 50, clamped to 0.6.
        c.record_sent_window(5000, &[user(&"a".repeat(100))]);
        assert!((c.token_ratio() - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn token_ratio_defaults_before_first_sample() {
        let c = ctx(vec![user("hi")]);
        assert!((c.token_ratio() - 0.3).abs() < f64::EPSILON);
    }

    // ── cache-aware projection ──

    /// A user message carrying a single tool result with a `chars`-long body.
    fn big_tool_result(id: &str, chars: usize) -> Message {
        Message::User {
            content: vec![UserBlock::tool_result(
                id,
                ToolResultContent::Text("x".repeat(chars)),
            )],
        }
    }

    /// History: a leading user turn, then `n` (tool-call, big tool-result) pairs.
    fn tool_heavy_history(n: usize, result_chars: usize) -> Vec<Message> {
        let mut msgs = vec![user("question")];
        for i in 0..n {
            let [call, _] = tool_exchange(&format!("t{i}"), 1);
            msgs.push(call);
            msgs.push(big_tool_result(&format!("t{i}"), result_chars));
        }
        msgs
    }

    /// History whose oldest tool results are large and whose most-recent ones are
    /// small: compacting the old, unprotected results is enough to drop the window
    /// back under a trigger, so the exact stub count is deterministic.
    fn front_heavy_history(big: usize, small: usize) -> Vec<Message> {
        let mut msgs = vec![user("question")];
        for i in 0..big {
            let [call, _] = tool_exchange(&format!("b{i}"), 1);
            msgs.push(call);
            msgs.push(big_tool_result(&format!("b{i}"), 4000));
        }
        for i in 0..small {
            let [call, _] = tool_exchange(&format!("s{i}"), 1);
            msgs.push(call);
            msgs.push(big_tool_result(&format!("s{i}"), 40));
        }
        msgs
    }

    fn cache_ctx(messages: Vec<Message>) -> ContextManager {
        ContextManager {
            messages,
            strategy: Arc::new(CacheAwareWindow),
            ..Default::default()
        }
    }

    fn stubbed_count(msgs: &[Message]) -> usize {
        msgs.iter()
            .filter(|m| match m {
                Message::User { content } => content.iter().any(|b| match b {
                    UserBlock::ToolResult(tr) => matches!(
                        &tr.content,
                        ToolResultContent::Text(t) if t.starts_with(COMPACTED_MARKER)
                    ),
                    _ => false,
                }),
                Message::Assistant { .. } => false,
            })
            .count()
    }

    #[test]
    fn linear_window_requests_no_caching() {
        let c = ctx(vec![user("a"), assistant("b"), user("c")]);
        let w = LinearWindow.project(&c, &RetrievalPolicy::default());
        assert!(!w.cache_prefix);
        assert!(w.breakpoints.is_empty());
    }

    #[test]
    fn cache_aware_caches_prefix_and_marks_breakpoints() {
        // A turn mid tool-loop: user("q") starts the turn, then a tool round-trip.
        // The only clean boundary is index 0, so the stable and rolling
        // breakpoints are distinct.
        let [call, result] = tool_exchange("t0", 1);
        let c = cache_ctx(vec![user("q"), call, result]);
        // No token-cap pressure → no compaction, but caching is still requested.
        let pol = RetrievalPolicy {
            compact_after_tokens: Some(1_000_000),
            compact_batch: 4,
            compact_keep_recent: 4,
            ..Default::default()
        };
        let w = c.project(&pol);
        assert!(w.cache_prefix, "stable prefix is cached");
        assert_eq!(
            w.breakpoints,
            vec![0, 2],
            "stable breakpoint at the turn boundary, rolling at the last message"
        );
        assert_eq!(
            stubbed_count(&w.messages),
            0,
            "no compaction under the trigger"
        );
    }

    #[test]
    fn compaction_stubs_exactly_enough_to_fit_the_trigger() {
        // 4 big old results (4000 chars) + 2 small recent ones (40 chars). With
        // batch=2/keep_recent=2 the stub count is deterministic: the two batches
        // that clear the four big results, and no more.
        let c = cache_ctx(front_heavy_history(4, 2));
        let pol = RetrievalPolicy {
            max_messages: Some(100),
            max_tokens: Some(120_000),
            compact_after_tokens: Some(1_000), // ~3.3k chars budget → must stub
            compact_batch: 2,
            compact_keep_recent: 2,
        };
        let w = c.project(&pol);
        assert_eq!(
            stubbed_count(&w.messages),
            4,
            "stubs exactly the four big old results, in two batches of two"
        );
        // Compaction relieved pressure back under the trigger (the default ratio
        // is 0.3, since no provider sample has been recorded).
        assert!(
            est_tokens(&w.messages, 0.3) <= 1_000,
            "post-compaction window fits compact_after_tokens"
        );
        // The final two messages (keep_recent=2) are never touched.
        let len = w.messages.len();
        assert_eq!(
            stubbed_count(&w.messages[len - 2..]),
            0,
            "the most recent messages stay intact"
        );
    }

    #[test]
    fn reprojecting_a_stubbed_window_is_idempotent() {
        // The core "byte-identical projection turn over turn" guarantee: feeding an
        // already-stubbed window back through project() must neither re-stub the
        // sentinel-marked results nor change the output.
        let pol = RetrievalPolicy {
            max_messages: Some(100),
            max_tokens: Some(120_000),
            compact_after_tokens: Some(1_000),
            compact_batch: 2,
            compact_keep_recent: 2,
        };
        let first = cache_ctx(front_heavy_history(4, 2)).project(&pol);
        assert!(
            stubbed_count(&first.messages) > 0,
            "the first projection actually stubs something to re-feed"
        );
        // Re-project the already-stubbed messages.
        let second = cache_ctx(first.messages.clone()).project(&pol);
        assert_eq!(
            serde_json::to_string(&first.messages).unwrap(),
            serde_json::to_string(&second.messages).unwrap(),
            "re-projecting a stubbed window is byte-identical"
        );
        assert_eq!(
            stubbed_count(&first.messages),
            stubbed_count(&second.messages),
            "the COMPACTED_MARKER guard prevents a re-stub / double-count"
        );
    }

    #[test]
    fn cache_aware_empty_history_requests_no_breakpoints() {
        let c = cache_ctx(Vec::new());
        let w = c.project(&RetrievalPolicy::default());
        assert!(w.messages.is_empty());
        assert!(
            w.breakpoints.is_empty(),
            "no messages → no breakpoints to place"
        );
    }

    #[test]
    fn cache_breakpoints_collapse_to_rolling_only_for_one_message() {
        // A single clean user turn: the stable breakpoint would land on the last
        // message, so it is suppressed and only the rolling breakpoint remains.
        let c = cache_ctx(vec![user("only")]);
        let w = c.project(&RetrievalPolicy::default());
        assert_eq!(w.breakpoints, vec![0]);
    }

    #[test]
    fn compaction_is_deterministic_for_a_stable_prefix() {
        let history = tool_heavy_history(6, 4000);
        let pol = RetrievalPolicy {
            max_messages: Some(100),
            max_tokens: Some(120_000),
            compact_after_tokens: Some(1_000),
            compact_batch: 2,
            compact_keep_recent: 2,
        };
        let first =
            serde_json::to_string(&cache_ctx(history.clone()).project(&pol).messages).unwrap();
        let second = serde_json::to_string(&cache_ctx(history).project(&pol).messages).unwrap();
        assert_eq!(first, second, "same history projects byte-identically");
    }

    #[test]
    fn compaction_disabled_when_trigger_is_none() {
        let c = cache_ctx(tool_heavy_history(6, 4000));
        let pol = RetrievalPolicy {
            max_messages: Some(100),
            max_tokens: Some(120_000),
            compact_after_tokens: None,
            compact_batch: 2,
            compact_keep_recent: 2,
        };
        let w = c.project(&pol);
        assert_eq!(stubbed_count(&w.messages), 0, "no trigger → no compaction");
    }
}
