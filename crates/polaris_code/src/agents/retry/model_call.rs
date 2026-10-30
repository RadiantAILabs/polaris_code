//! The model call as stages of the agent graph.
//!
//! One call to the model is several steps, and the decisions between them are
//! graph structure rather than a loop inside one system:
//!
//! ```text
//! begin_model_call → act → loop("model_retries") until the attempt is settled {
//!     decide_retry → retry_or_give_up ? (wait_to_retry → act) : abandon
//! } → model_answered ? (accept_response → …) : report_failure
//! ```
//!
//! - [`act`] is one attempt. It streams the model's answer to the user while
//!   it accounts for the tokens, and ends in a typed [`AttemptOutcome`]: a
//!   complete response, or a failure that carries its classification and
//!   whether the attempt had already shown or billed anything. It appears
//!   twice in the graph: as the first attempt, and as the edge back into the
//!   call that a retry takes.
//! - [`decide_retry`] asks [`RetryPolicy::next_step`](super::super::policy::RetryPolicy)
//!   what follows a failure and records the answer on the trajectory.
//! - [`wait_to_retry`] is the backoff. It ends at once when the turn is
//!   interrupted.
//! - [`abandon`] settles the call as given up, and [`report_failure`] hands the
//!   classified failure to the graph's error handler, so it is reported once.
//! - [`accept_response`] takes a complete response into the conversation.
//!
//! Replacing a decision is rewiring a node. A topology that never retries
//! leaves the loop out; one that retries differently swaps [`decide_retry`].
//! Every system in the retry loop is total apart from an interrupt, which ends
//! the turn before any handler runs, so an error handler can never leave the
//! loop running.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use polaris::graph::RunCancellation;
use polaris::models::llm::{GenerationError, LlmResponse, Message, StreamEvent, StreamEventExt};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, UserIO};
use polaris::prelude::Out;
use polaris::system::param::{Res, ResMut};
use polaris::system::prelude::SystemError;
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris::tools::{LlmRequestBuilderExt, ToolRegistry};
use polaris_ontrace::oxigraph::OxigraphStore;

use super::{Commitment, RetryStep, guard_idle, jitter, within};
use crate::agents::code_agent::{AgentLlm, PreparedContext};
use crate::agents::context::ContextManager;
use crate::agents::failure::{self, Attempts, FailureKind, TurnFailure};
use crate::agents::policy::{PolicySnapshot, ToolPalettePolicy};
use crate::agents::state::LoopState;
use crate::agents::trajectory::Decision;
use crate::terminal::status;
use crate::terminal::transcript::{LiveTranscript, OUTCOME_COMPLETE, OUTCOME_ERROR, forward};
use crate::usage::TokenMeter;

/// How a turn ends when an interrupt reaches a model call.
const INTERRUPTED: &str = "turn interrupted";

/// The attempts one model call has used, and when it started.
///
/// Reset by [`begin_model_call`] and advanced by every [`act`], so the retry
/// decision and the final report read the same count.
#[derive(Debug, Default)]
pub(crate) struct AttemptLedger {
    attempts: u32,
    started: Option<tokio::time::Instant>,
}

impl LocalResource for AttemptLedger {}

impl AttemptLedger {
    fn begin(&mut self) {
        self.attempts = 0;
        self.started = Some(tokio::time::Instant::now());
    }

    fn next_attempt(&mut self) -> u32 {
        self.attempts += 1;
        self.attempts
    }

    fn elapsed(&self) -> Duration {
        self.started
            .map_or(Duration::ZERO, |started| started.elapsed())
    }
}

/// What one model attempt, or the call as a whole, came to.
#[derive(Debug)]
pub(crate) enum AttemptOutcome {
    /// The attempt produced a complete response.
    Succeeded(Reply),
    /// The attempt failed. Whether to try again is [`decide_retry`]'s call.
    Failed(AttemptFailure),
    /// The call is over without a response: the policy declined another
    /// attempt. Written by [`abandon`].
    GaveUp(TurnFailure),
}

impl AttemptOutcome {
    /// Whether a response arrived.
    pub(crate) fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded(_))
    }

    /// Whether the call is finished one way or the other. A failed attempt is
    /// not: the retry stage has yet to decide.
    pub(crate) fn is_settled(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }
}

/// A complete model response and what the provider reported about the window
/// that produced it.
#[derive(Debug)]
pub(crate) struct Reply {
    response: LlmResponse,
    /// Provider-native `input_tokens` for the window sent, for calibrating the
    /// context manager's estimate.
    prompt_tokens: u64,
}

/// A failed attempt, classified where the error is known.
#[derive(Debug)]
pub(crate) struct AttemptFailure {
    error: GenerationError,
    kind: FailureKind,
    commitment: Commitment,
}

/// The tools the model is offered this turn: the registry's exposed tools
/// narrowed by the palette policy.
fn offered_tools(registry: &ToolRegistry, palette: &ToolPalettePolicy) -> Vec<String> {
    registry
        .definitions_for(|name| palette.includes(name))
        .into_iter()
        .map(|def| def.name)
        .collect()
}

/// Send the model's token usage to the panel as an *increment* of the totals.
///
/// The conversation panel adds each delta's `in`/`out` counts into a running
/// session tally, so during a streaming call the change since the last
/// emission is forwarded, not the cumulative total the stream reports, and the
/// sum still matches cost.
async fn send_token_delta(user_io: &UserIO, in_delta: u64, out_delta: u64) {
    let msg = IOMessage::from_agent("code", IOContent::Text(String::new()))
        .with_metadata(IO_TYPE, "tokens")
        .with_metadata("in", in_delta.to_string())
        .with_metadata("out", out_delta.to_string());
    let _ = user_io.send(msg).await;
}

/// Fold a cumulative usage reading into the increment to emit this step.
///
/// Streams report *cumulative* `input`/`output` token totals, but the panel
/// tally and the session [`TokenMeter`] are additive, so each step forwards only
/// the increase since the previous reading. A repeated or decreasing reading
/// (which would otherwise double-count or underflow) yields `(0, 0)`.
fn token_increment(prev_in: u64, prev_out: u64, in_now: u64, out_now: u64) -> (u64, u64) {
    (
        in_now.saturating_sub(prev_in),
        out_now.saturating_sub(prev_out),
    )
}

/// Assemble the reified `tool_palette` [`Decision`] for a turn: the tools
/// actually offered (`offered`, already intersected with exposure), how the
/// palette was selected, and the policy values that drove it.
///
/// Split out of [`accept_response`] so the provenance shape is unit-testable
/// without standing up the full system. Consumes `offered` into the decision's
/// options.
fn tool_palette_decision(
    palette: &ToolPalettePolicy,
    offered: Vec<String>,
    max_attempts: u32,
    attempts_used: u32,
) -> Decision {
    let mode = palette.mode.label();
    let count = offered.len();
    Decision {
        kind: "tool_palette",
        intent: None,
        chosen: format!("{mode}:{count}"),
        options: offered,
        policy_refs: vec![
            format!("tool_palette.mode={mode}"),
            format!("tool_palette.denied={}", palette.denied.len()),
            format!("retry.max_attempts={max_attempts}"),
            format!("attempts_used={attempts_used}"),
        ],
    }
}

/// Log the model's reasoning and chosen actions.
///
/// They become node-scoped `tracing` events: they nest under the span of the
/// system that logs them, which is [`act`], and surface under that node in the
/// panel's Ctrl-O timeline rather than cluttering the transcript. On a terminal
/// turn `finalize` still emits the answer to the transcript, so there is no
/// double print.
fn log_response(response: &LlmResponse) {
    let reasoning = response.text();
    if !reasoning.is_empty() {
        tracing::info!("REASON: {reasoning}");
    }
    for call in response.tool_calls() {
        tracing::info!(
            "Action: {}({})",
            call.function.name,
            call.function.arguments
        );
    }
}

/// Start counting the attempts of one model call.
#[system]
pub(crate) async fn begin_model_call(mut ledger: ResMut<AttemptLedger>) {
    ledger.begin();
}

/// One attempt at the model call, with the tools the palette offers.
///
/// The stream's text is forwarded to the user as it arrives and its usage is
/// folded into the session meter as it is reported. The attempt is *committed*
/// once either has happened: shown text and billed tokens cannot be rolled
/// back, so a failure after that point is not worth another attempt.
///
/// Both the wait for the response and every gap between its events are
/// bounded by [`attempt_timeout`](crate::agents::policy::RetryPolicy), because
/// the Anthropic client sets no HTTP timeout of its own; expiry is a transient
/// failure like any other.
///
/// A failure is an [`AttemptOutcome::Failed`], not an error: the retry stage
/// decides what it means. Only an interrupt is an error, and it ends the turn.
#[system]
pub(crate) async fn act(
    prepared: Out<PreparedContext>,
    llm: Res<AgentLlm>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    policy: Res<PolicySnapshot>,
    meter: Res<TokenMeter>,
    mut live: ResMut<LiveTranscript>,
    cancel: Res<RunCancellation>,
    mut ledger: ResMut<AttemptLedger>,
) -> Result<AttemptOutcome, SystemError> {
    ledger.next_attempt();
    let attempt_timeout = policy.retry.attempt_timeout;
    let messages = prepared.messages.clone();

    // An empty palette is a legitimate pure-reasoning turn (the model is sent
    // no tools), but it is always intentional — log it so it is never silent,
    // and so an accidentally over-narrow policy is observable.
    if offered_tools(&tool_registry, &policy.tool_palette).is_empty() {
        tracing::debug!(
            mode = policy.tool_palette.mode.label(),
            "act: tool palette is empty — offering the model no tools this turn"
        );
    }
    // Cheap Arc clone of the session meter to fold usage into from the stream
    // closure; the binary reads it on exit to print a cost summary.
    let meter = TokenMeter::clone(&meter);

    // Per-attempt live token accounting. Each `MessageDelta`/`MessageStop`
    // carries *cumulative* usage; we forward the *increment* since the last
    // emission, because the conversation panel adds deltas into its running
    // tally. `committed` flips true once we've emitted anything this attempt;
    // past that point the additive tally can't be rolled back, so a failed
    // stream is terminal rather than retryable.
    let last_in = Arc::new(AtomicU64::new(0));
    let last_out = Arc::new(AtomicU64::new(0));
    let last_cache_read = Arc::new(AtomicU64::new(0));
    let last_cache_creation = Arc::new(AtomicU64::new(0));
    let committed = Arc::new(AtomicBool::new(false));
    // A fresh transcript block per attempt. A retry that has shown nothing
    // reuses nothing; a retry after visible text cannot happen at all
    // (emitting a delta marks the attempt committed, below), so one block
    // never mixes two attempts' text.
    // Close the previous iteration's block first: a ReAct turn emits one
    // block per iteration (reasoning, then its tools), and an unclosed one
    // would render as still being written.
    live.close(&user_io, "", OUTCOME_COMPLETE).await;
    let block = live.open();

    let user_io_ref: &UserIO = &user_io;
    let result = cancel
        .run_until_cancelled(async {
            // Cache-aware projection: the strategy decides whether to
            // cache the stable prefix (system + tools) and where to place the
            // rolling/stable conversation breakpoints; we forward those onto the
            // request so the provider emits `cache_control`.
            let mut builder = llm
                .builder()
                .with_registry_filtered(&tool_registry, |name| policy.tool_palette.includes(name))
                .system(prepared.system_prompt.clone())
                .messages(messages);
            if prepared.cache_prefix {
                builder = builder.cache_prefix();
            }
            builder = builder.cache_breakpoints(prepared.cache_breakpoints.clone());
            meter.record_model_turn();
            // A provider that goes quiet must not hold the turn for ever:
            // both the wait for the response and every gap between its
            // events are bounded, and expiry is a transient failure.
            let stream = within(attempt_timeout, builder.stream()).await?;

            // Emit token increments as usage events arrive, then hand the
            // (still complete) event stream to the collector for assembly.
            guard_idle(stream, attempt_timeout)
                .then(|event| {
                    let block = block.clone();
                    let last_in = last_in.clone();
                    let last_out = last_out.clone();
                    let last_cache_read = last_cache_read.clone();
                    let last_cache_creation = last_cache_creation.clone();
                    let committed = committed.clone();
                    let meter = meter.clone();
                    async move {
                        // Forward the model's text as it arrives; anything
                        // that reached the screen makes the attempt terminal.
                        if forward(&block, user_io_ref, &event).await {
                            committed.store(true, Ordering::Relaxed);
                        }
                        let usage = match &event {
                            Ok(StreamEvent::MessageDelta { usage }) => Some(usage),
                            Ok(StreamEvent::MessageStop { usage, .. }) => Some(usage),
                            _ => None,
                        };
                        if let Some(usage) = usage {
                            let in_now = usage.input_tokens.unwrap_or(0);
                            let out_now = usage.output_tokens.unwrap_or(0);
                            let (in_delta, out_delta) = token_increment(
                                last_in.swap(in_now, Ordering::Relaxed),
                                last_out.swap(out_now, Ordering::Relaxed),
                                in_now,
                                out_now,
                            );
                            // Cache tiers: bill cache read/write into
                            // the meter at their discounted rates. Same cumulative
                            // → increment fold as input/output.
                            let cr_now = usage.cache_read_tokens.unwrap_or(0);
                            let cc_now = usage.cache_creation_tokens.unwrap_or(0);
                            let (cr_delta, cc_delta) = token_increment(
                                last_cache_read.swap(cr_now, Ordering::Relaxed),
                                last_cache_creation.swap(cc_now, Ordering::Relaxed),
                                cr_now,
                                cc_now,
                            );
                            // Anything billed to the meter makes this attempt
                            // terminal — a retry would double-count.
                            if cr_delta > 0 || cc_delta > 0 || in_delta > 0 || out_delta > 0 {
                                committed.store(true, Ordering::Relaxed);
                            }
                            if cr_delta > 0 || cc_delta > 0 {
                                meter.add_cache(cr_delta, cc_delta);
                            }
                            if in_delta > 0 || out_delta > 0 {
                                // Mirror the same increment into the session meter
                                // (binary-side, survives the TUI) and the panel.
                                meter.add(in_delta, out_delta);
                                send_token_delta(user_io_ref, in_delta, out_delta).await;
                            }
                        }
                        event
                    }
                })
                .collect_response()
                .await
        })
        .await;

    let Some(result) = result else {
        live.close(&user_io, "", OUTCOME_ERROR).await;
        return Err(SystemError::ExecutionError(INTERRUPTED.into()));
    };

    Ok(match result {
        Ok(response) => {
            log_response(&response);
            AttemptOutcome::Succeeded(Reply {
                response,
                prompt_tokens: last_in.load(Ordering::Relaxed),
            })
        }
        Err(error) => AttemptOutcome::Failed(AttemptFailure {
            kind: failure::classify(&error),
            error,
            commitment: Commitment::from(committed.load(Ordering::Relaxed)),
        }),
    })
}

/// Decide what follows a failed attempt, and record the decision.
///
/// The answer is [`RetryPolicy::next_step`](crate::agents::policy::RetryPolicy):
/// a pure function of the attempt count, the failure's classification, whether
/// the attempt was committed and a jitter. The graph routes on it. An outcome
/// that is not a failure is not retried.
#[system]
pub(crate) async fn decide_retry(
    outcome: Out<AttemptOutcome>,
    prepared: Out<PreparedContext>,
    ledger: Res<AttemptLedger>,
    policy: Res<PolicySnapshot>,
    store: Res<OxigraphStore>,
) -> RetryStep {
    let AttemptOutcome::Failed(failed) = &*outcome else {
        return RetryStep::GiveUp;
    };
    let step = policy
        .retry
        .next_step(ledger.attempts, &failed.error, failed.commitment, jitter());
    let (chosen, wait) = match step {
        RetryStep::Retry { after } => {
            tracing::warn!(
                kind = %failed.kind,
                wait = ?after,
                "llm attempt {} of {} failed: {}; retrying",
                ledger.attempts,
                policy.retry.max_attempts.max(1),
                failed.error
            );
            ("retry", format!("{after:?}"))
        }
        RetryStep::GiveUp => ("give-up", "none".to_string()),
    };
    Decision {
        kind: "retry",
        intent: None,
        options: vec!["retry".into(), "give-up".into()],
        chosen: chosen.into(),
        policy_refs: vec![
            format!("retry.max_attempts={}", policy.retry.max_attempts),
            format!("attempt={}", ledger.attempts),
            format!("failure.kind={}", failed.kind),
            format!("committed={}", failed.commitment.is_committed()),
            format!("wait={wait}"),
        ],
    }
    .record(&store, prepared.turn);
    step
}

/// Wait out a backoff before the next attempt.
///
/// The wait is cancellable like the call itself, so an interrupt during a
/// long backoff ends the turn now. Anything but a retry step waits for nothing.
#[system]
pub(crate) async fn wait_to_retry(
    step: Out<RetryStep>,
    cancel: Res<RunCancellation>,
    user_io: Res<UserIO>,
    mut live: ResMut<LiveTranscript>,
) -> Result<(), SystemError> {
    let RetryStep::Retry { after } = *step else {
        return Ok(());
    };
    if cancel
        .run_until_cancelled(tokio::time::sleep(after))
        .await
        .is_none()
    {
        live.close(&user_io, "", OUTCOME_ERROR).await;
        return Err(SystemError::ExecutionError(INTERRUPTED.into()));
    }
    Ok(())
}

/// Settle a call the policy will not try again as given up, with the
/// classified summary that will be reported.
///
/// The raw error stays in the log; the summary is what reaches the user, once,
/// from [`report_failure`]. An outcome that is not a failure is left as it is.
#[system]
pub(crate) async fn abandon(
    outcome: Out<AttemptOutcome>,
    prepared: Out<PreparedContext>,
    ledger: Res<AttemptLedger>,
) -> AttemptOutcome {
    let AttemptOutcome::Failed(failed) = &*outcome else {
        return AttemptOutcome::GaveUp(TurnFailure::from(
            "The model call ended without a response.",
        ));
    };
    let failure = failure::describe(
        &failed.error,
        &prepared.model_id,
        Attempts {
            retries: ledger.attempts.saturating_sub(1),
            elapsed: ledger.elapsed(),
            committed: failed.commitment.is_committed(),
        },
    );
    tracing::error!(
        kind = %failure.kind,
        "llm call failed after {} attempt(s): {}",
        ledger.attempts,
        failed.error
    );
    AttemptOutcome::GaveUp(failure)
}

/// Take a complete response into the conversation.
///
/// Feeds the provider's real `input_tokens` and the window sent back to the
/// context manager so the next projection can calibrate its tokens-per-char
/// estimate. That happens here, on a call that succeeded, so a failed or
/// retried attempt never miscalibrates. Then it reports cost, appends the
/// assistant message and records the tool-palette decision.
#[system]
pub(crate) async fn accept_response(
    outcome: Out<AttemptOutcome>,
    prepared: Out<PreparedContext>,
    ledger: Res<AttemptLedger>,
    mut context: ResMut<ContextManager>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    policy: Res<PolicySnapshot>,
    store: Res<OxigraphStore>,
    meter: Res<TokenMeter>,
) -> Result<LlmResponse, SystemError> {
    let AttemptOutcome::Succeeded(reply) = &*outcome else {
        return Err(SystemError::ExecutionError(
            "a response was accepted from a call that produced none".into(),
        ));
    };
    tool_palette_decision(
        &policy.tool_palette,
        offered_tools(&tool_registry, &policy.tool_palette),
        policy.retry.max_attempts.max(1),
        ledger.attempts,
    )
    .record(&store, prepared.turn);

    context.record_sent_window(reply.prompt_tokens, &prepared.messages);
    status::send_cost(&user_io, &prepared.model_id, &meter).await;

    let response = &reply.response;
    context.push(Message::Assistant {
        id: None,
        content: response.content.clone(),
    });

    Ok(response.clone())
}

/// Hand a call that was given up on to the graph's error handler.
///
/// The failure is already classified and worded. It is stored on
/// [`LoopState::failure`] so the turn's result carries it, and the handler
/// reports it once without asking the same provider again.
#[system]
pub(crate) async fn report_failure(
    outcome: Out<AttemptOutcome>,
    mut loop_state: ResMut<LoopState>,
) -> Result<(), SystemError> {
    let failure = match &*outcome {
        AttemptOutcome::GaveUp(failure) => failure.clone(),
        _ => TurnFailure::from("The model call ended without a response."),
    };
    let message = failure.message.clone();
    loop_state.failure = Some(failure);
    Err(SystemError::ExecutionError(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::policy::PaletteMode;

    #[test]
    fn token_increment_forwards_only_the_increase() {
        // Cumulative readings produce the per-step increment.
        assert_eq!(token_increment(0, 0, 10, 4), (10, 4));
        assert_eq!(token_increment(10, 4, 25, 9), (15, 5));
        // A repeated reading adds nothing.
        assert_eq!(token_increment(25, 9, 25, 9), (0, 0));
        // A decreasing/out-of-order reading saturates to zero rather than
        // underflowing (which would corrupt the additive tally).
        assert_eq!(token_increment(25, 9, 20, 9), (0, 0));
    }

    #[test]
    fn tool_palette_decision_reifies_mode_and_offered() {
        let palette = ToolPalettePolicy {
            mode: PaletteMode::Only(vec!["read_file".into()]),
            denied: vec!["edit_file".into()],
        };
        let decision = tool_palette_decision(&palette, vec!["read_file".into()], 2, 1);
        assert_eq!(decision.kind, "tool_palette");
        // `<mode-label>:<offered-count>` — the reified summary the ABox stores.
        assert_eq!(decision.chosen, "selected:1");
        assert_eq!(decision.options, vec!["read_file".to_string()]);
        assert!(
            decision
                .policy_refs
                .contains(&"tool_palette.mode=selected".to_string())
        );
        assert!(
            decision
                .policy_refs
                .contains(&"tool_palette.denied=1".to_string())
        );
        assert!(
            decision
                .policy_refs
                .contains(&"retry.max_attempts=2".to_string())
        );
        assert!(
            decision
                .policy_refs
                .contains(&"attempts_used=1".to_string())
        );
    }

    #[test]
    fn tool_palette_decision_reports_empty_none_palette() {
        // The explicit no-tools turn reifies as `none:0` with no options — the
        // empty palette is recorded, never silent.
        let palette = ToolPalettePolicy {
            mode: PaletteMode::None,
            denied: Vec::new(),
        };
        let decision = tool_palette_decision(&palette, Vec::new(), 2, 1);
        assert_eq!(decision.chosen, "none:0");
        assert!(decision.options.is_empty());
        assert!(
            decision
                .policy_refs
                .contains(&"tool_palette.mode=none".to_string())
        );
    }

    #[test]
    fn only_a_failed_attempt_is_unsettled() {
        let failed = AttemptOutcome::Failed(AttemptFailure {
            error: GenerationError::Http("reset".into()),
            kind: FailureKind::Network,
            commitment: Commitment::Uncommitted,
        });
        let gave_up = AttemptOutcome::GaveUp(TurnFailure::from("done"));
        let answered = AttemptOutcome::Succeeded(Reply {
            response: LlmResponse {
                content: Vec::new(),
                usage: Default::default(),
                stop_reason: polaris::models::llm::StopReason::EndTurn,
                id: None,
                model: None,
            },
            prompt_tokens: 0,
        });
        assert!(!failed.is_settled() && !failed.is_success());
        assert!(gave_up.is_settled() && !gave_up.is_success());
        assert!(answered.is_settled() && answered.is_success());
    }

    #[test]
    fn the_ledger_counts_attempts_from_one_and_restarts() {
        let mut ledger = AttemptLedger::default();
        assert_eq!(ledger.elapsed(), Duration::ZERO, "no call has begun");
        ledger.begin();
        assert_eq!(ledger.next_attempt(), 1);
        assert_eq!(ledger.next_attempt(), 2);
        ledger.begin();
        assert_eq!(ledger.next_attempt(), 1, "a new call starts over");
    }
}
