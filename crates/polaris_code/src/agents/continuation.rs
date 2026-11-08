//! Whether a model reply that calls no tool ends the turn.
//!
//! The agent loop ends a turn on the first reply without a tool call. Two such
//! replies are not answers:
//!
//! - **A truncated reply.** The provider stopped at the output limit
//!   ([`StopReason::MaxOutputTokens`]), so the reply is cut off. Ending there
//!   would report part of an answer as the whole one.
//! - **An unattended pause.** In a run nobody watches (`--print`), a reply that
//!   describes the next step instead of taking it ends the run with the work
//!   undone, because nobody is there to say "go on".
//!
//! This module is the decision seam for both. [`ContinuationPolicy`] (part of
//! the per-turn [`PolicySnapshot`](super::PolicySnapshot)) says how many times
//! each may be continued in one turn; [`ContinuationLedger`] counts what this
//! turn has used; `decide_continuation` chooses and records the decision;
//! `continue_turn` appends the continuation prompt and keeps the loop going.
//! Every budget is finite, so a model that keeps replying without tools still
//! ends the turn.
//!
//! The graph wires it into the no-tool-call branch of the agent loop:
//!
//! ```text
//! has_tool_calls? ── no ─▶ decide_continuation ─▶ continues? ── yes ─▶ continue_turn
//!                                                            └─ no ──▶ finalize
//! ```
//!
//! A continuation prompt continues the active user turn; it is not a new one.
//! [`is_continuation_prompt`] lets context projection keep it from starting a
//! window boundary, so the task statement is never dropped in favour of it.

use polaris::models::llm::{LlmResponse, Message, StopReason, UserBlock};
use polaris::prelude::Out;
use polaris::system::param::{Res, ResMut};
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris_ontrace::oxigraph::OxigraphStore;

use super::code_agent::PreparedContext;
use super::context::ContextManager;
use super::policy::PolicySnapshot;
use super::state::LoopState;
use super::trajectory::Decision;

/// Default number of truncated replies continued in one turn.
pub const DEFAULT_TRUNCATION_CONTINUES: u32 = 3;

/// Default number of times an unattended `--print` run is asked to go on
/// after a reply without a tool call.
///
/// The `polaris` binary applies it to its unattended runs
/// (`POLARIS_CODE_UNATTENDED_NUDGES` overrides it). [`ContinuationPolicy::defaults`]
/// leaves nudges off, so a host that embeds the agent keeps its own contract
/// for what a reply without a tool call means.
pub const DEFAULT_UNATTENDED_NUDGES: u32 = 1;

/// The prefix every continuation prompt starts with.
///
/// It marks the message as written by the agent loop, not the user, both for
/// the model and for [`is_continuation_prompt`].
pub const CONTINUATION_MARKER: &str = "[continuation] ";

/// How many replies without a tool call one turn may continue past.
///
/// [`Default`] continues nothing, which is the loop's behaviour without this
/// policy; [`PolicySnapshot::defaults`](super::PolicySnapshot::defaults) uses
/// [`ContinuationPolicy::defaults`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContinuationPolicy {
    /// Truncated replies continued per turn, in any front end.
    pub truncation_continues: u32,
    /// Replies without a tool call continued per turn when nobody can answer
    /// the agent. Has no effect on an attended turn: there, such a reply is the
    /// agent handing the conversation back to the user.
    pub unattended_nudges: u32,
}

impl ContinuationPolicy {
    /// The library defaults: [`DEFAULT_TRUNCATION_CONTINUES`] truncation
    /// continues and no unattended nudges. A truncated reply is never a whole
    /// answer, whoever hosts the agent; whether a reply without a tool call is
    /// one depends on the host, so the host opts in with
    /// [`with_unattended_nudges`](Self::with_unattended_nudges).
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            truncation_continues: DEFAULT_TRUNCATION_CONTINUES,
            unattended_nudges: 0,
        }
    }

    /// This policy with `nudges` unattended nudges per turn.
    #[must_use]
    pub fn with_unattended_nudges(mut self, nudges: u32) -> Self {
        self.unattended_nudges = nudges;
        self
    }

    /// Decide what follows a reply without a tool call.
    ///
    /// A truncated reply is continued while its budget lasts. Otherwise, an
    /// unattended turn is nudged while that budget lasts. Everything else
    /// finishes the turn.
    #[must_use]
    pub fn decide(
        &self,
        stop_reason: &StopReason,
        unattended: bool,
        used: &ContinuationLedger,
    ) -> Continuation {
        if matches!(stop_reason, StopReason::MaxOutputTokens)
            && used.truncations < self.truncation_continues
        {
            return Continuation::Continue(ContinueReason::Truncated);
        }
        if unattended && used.nudges < self.unattended_nudges {
            return Continuation::Continue(ContinueReason::Unattended {
                last: used.nudges + 1 >= self.unattended_nudges,
            });
        }
        Continuation::Finish
    }
}

/// What this turn has used of its [`ContinuationPolicy`] budgets.
///
/// Reset at the start of every turn by `reset_continuations`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContinuationLedger {
    /// Truncated replies continued so far.
    pub truncations: u32,
    /// Unattended nudges sent so far.
    pub nudges: u32,
}

impl LocalResource for ContinuationLedger {}

/// Why a reply without a tool call does not end the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueReason {
    /// The reply stopped at the output limit.
    Truncated,
    /// Nobody can answer, and the reply took no action.
    Unattended {
        /// Whether this is the turn's last nudge, so the prompt can say the
        /// next reply without a tool call ends the run.
        last: bool,
    },
}

impl ContinueReason {
    /// Short label for the recorded decision.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Truncated => "continue:truncated",
            Self::Unattended { .. } => "continue:unattended",
        }
    }

    /// The prompt the agent loop sends in place of a user reply.
    #[must_use]
    pub fn prompt(self) -> String {
        let body = match self {
            Self::Truncated => {
                "Your last reply stopped at the output limit before it was finished. \
                 Continue from where it stopped. If you were about to call a tool, call it now."
            }
            Self::Unattended { last: true } => {
                "Your last reply called no tool. This run is unattended: nobody will answer, \
                 and the run ends at your next reply that calls no tool. If the task is not \
                 finished, keep working with the tools now. If it is finished, reply with a \
                 short final summary."
            }
            Self::Unattended { last: false } => {
                "Your last reply called no tool. This run is unattended: nobody will answer. \
                 If the task is not finished, keep working with the tools now. If it is \
                 finished, reply with a short final summary."
            }
        };
        format!("{CONTINUATION_MARKER}{body}")
    }
}

/// The outcome of `decide_continuation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuation {
    /// End the turn on this reply.
    Finish,
    /// Ask the model to go on.
    Continue(ContinueReason),
}

impl Continuation {
    /// Whether the turn goes on; the graph's branch predicate.
    #[must_use]
    pub fn continues(&self) -> bool {
        matches!(self, Self::Continue(_))
    }
}

/// Whether `message` is a continuation prompt this module wrote.
///
/// Context projection uses this so a continuation prompt does not start a
/// window boundary: it belongs to the turn it continues.
#[must_use]
pub fn is_continuation_prompt(message: &Message) -> bool {
    match message {
        Message::User { content } => matches!(
            content.as_slice(),
            [UserBlock::Text(block)] if block.text.starts_with(CONTINUATION_MARKER)
        ),
        Message::Assistant { .. } => false,
    }
}

/// Start the turn with every continuation budget unused.
#[system]
pub(crate) async fn reset_continuations(mut ledger: ResMut<ContinuationLedger>) {
    *ledger = ContinuationLedger::default();
}

/// Decide whether a reply without a tool call ends the turn, and record why.
#[system]
pub(crate) async fn decide_continuation(
    response: Out<LlmResponse>,
    prepared: Out<PreparedContext>,
    policy: Res<PolicySnapshot>,
    ledger: Res<ContinuationLedger>,
    store: Res<OxigraphStore>,
) -> Continuation {
    let unattended = policy.permission.is_unattended();
    let budget = &policy.continuation;
    let continuation = budget.decide(&response.stop_reason, unattended, &ledger);
    Decision {
        kind: "continuation",
        intent: None,
        options: vec!["finish".into(), "continue".into()],
        chosen: match continuation {
            Continuation::Finish => "finish".into(),
            Continuation::Continue(reason) => reason.label().into(),
        },
        policy_refs: vec![
            format!("stop_reason={:?}", response.stop_reason),
            format!("unattended={unattended}"),
            format!(
                "continuation.truncation_continues={}/{}",
                ledger.truncations, budget.truncation_continues
            ),
            format!(
                "continuation.unattended_nudges={}/{}",
                ledger.nudges, budget.unattended_nudges
            ),
        ],
    }
    .record(&store, prepared.turn);
    continuation
}

/// Append the continuation prompt, count it, and keep the loop going.
#[system]
pub(crate) async fn continue_turn(
    continuation: Out<Continuation>,
    mut ledger: ResMut<ContinuationLedger>,
    mut context: ResMut<ContextManager>,
) -> LoopState {
    if let Continuation::Continue(reason) = *continuation {
        match reason {
            ContinueReason::Truncated => ledger.truncations += 1,
            ContinueReason::Unattended { .. } => ledger.nudges += 1,
        }
        tracing::info!(reason = reason.label(), "continuing the turn");
        context.push(Message::user(reason.prompt()));
    }
    LoopState {
        is_complete: false,
        failure: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(truncations: u32, nudges: u32) -> ContinuationLedger {
        ContinuationLedger {
            truncations,
            nudges,
        }
    }

    #[test]
    fn default_policy_continues_nothing() {
        let policy = ContinuationPolicy::default();
        assert_eq!(
            policy.decide(&StopReason::MaxOutputTokens, true, &used(0, 0)),
            Continuation::Finish
        );
    }

    #[test]
    fn a_truncated_reply_continues_until_its_budget_is_spent() {
        let policy = ContinuationPolicy::defaults();
        for spent in 0..DEFAULT_TRUNCATION_CONTINUES {
            assert_eq!(
                policy.decide(&StopReason::MaxOutputTokens, false, &used(spent, 0)),
                Continuation::Continue(ContinueReason::Truncated)
            );
        }
        assert_eq!(
            policy.decide(
                &StopReason::MaxOutputTokens,
                false,
                &used(DEFAULT_TRUNCATION_CONTINUES, 0)
            ),
            Continuation::Finish,
            "an attended turn finishes once truncation continues are spent"
        );
    }

    #[test]
    fn an_attended_reply_that_ends_its_turn_finishes() {
        let policy = ContinuationPolicy::defaults();
        assert_eq!(
            policy.decide(&StopReason::EndTurn, false, &used(0, 0)),
            Continuation::Finish
        );
    }

    #[test]
    fn an_unattended_reply_is_nudged_then_finishes() {
        let policy = ContinuationPolicy {
            truncation_continues: 0,
            unattended_nudges: 2,
        };
        assert_eq!(
            policy.decide(&StopReason::EndTurn, true, &used(0, 0)),
            Continuation::Continue(ContinueReason::Unattended { last: false })
        );
        assert_eq!(
            policy.decide(&StopReason::EndTurn, true, &used(0, 1)),
            Continuation::Continue(ContinueReason::Unattended { last: true })
        );
        assert_eq!(
            policy.decide(&StopReason::EndTurn, true, &used(0, 2)),
            Continuation::Finish
        );
    }

    #[test]
    fn library_defaults_continue_truncation_but_never_nudge() {
        let policy = ContinuationPolicy::defaults();
        assert_eq!(policy.truncation_continues, DEFAULT_TRUNCATION_CONTINUES);
        assert_eq!(
            policy.decide(&StopReason::EndTurn, true, &used(0, 0)),
            Continuation::Finish
        );
    }

    #[test]
    fn spent_truncation_budget_falls_back_to_the_unattended_nudge() {
        let policy =
            ContinuationPolicy::defaults().with_unattended_nudges(DEFAULT_UNATTENDED_NUDGES);
        assert_eq!(
            policy.decide(
                &StopReason::MaxOutputTokens,
                true,
                &used(DEFAULT_TRUNCATION_CONTINUES, 0)
            ),
            Continuation::Continue(ContinueReason::Unattended { last: true })
        );
    }

    #[test]
    fn prompts_carry_the_marker_and_are_recognised() {
        for reason in [
            ContinueReason::Truncated,
            ContinueReason::Unattended { last: true },
            ContinueReason::Unattended { last: false },
        ] {
            let message = Message::user(reason.prompt());
            assert!(is_continuation_prompt(&message), "{reason:?}");
        }
        assert!(
            ContinueReason::Unattended { last: true }
                .prompt()
                .contains("the run ends at your next reply")
        );
        assert!(
            !ContinueReason::Unattended { last: false }
                .prompt()
                .contains("the run ends")
        );
        assert!(!is_continuation_prompt(&Message::user("fix the bug")));
        assert!(!is_continuation_prompt(&Message::assistant(
            ContinueReason::Truncated.prompt()
        )));
    }
}
