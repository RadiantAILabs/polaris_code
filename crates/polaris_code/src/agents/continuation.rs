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

use polaris::models::llm::{AssistantBlock, LlmResponse, Message, StopReason, UserBlock};
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
/// after a reply without a tool call: none.
///
/// The `polaris` binary applies it to its unattended runs
/// (`POLARIS_CODE_UNATTENDED_NUDGES` overrides it). It is 0 because, on
/// benchmark runs, every nudge came after the work was already finished and
/// only added a model call. A model that tends to stop half-way can be given
/// nudges with the variable, or by an embedder with
/// [`ContinuationPolicy::with_unattended_nudges`].
pub const DEFAULT_UNATTENDED_NUDGES: u32 = 0;

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
///
/// # Examples
///
/// ```
/// use polaris::models::llm::StopReason;
/// use polaris_code::agents::continuation::{
///     Continuation, ContinuationLedger, ContinuationPolicy, ContinueReason,
/// };
///
/// let policy = ContinuationPolicy::defaults().with_unattended_nudges(1);
/// let used = ContinuationLedger::default();
/// assert_eq!(
///     policy.decide(&StopReason::EndTurn, true, &used),
///     Continuation::Continue(ContinueReason::Unattended { last: true }),
/// );
/// ```
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

/// Project the last answer, joining only fragments separated by output-limit
/// continuation prompts. Canonical provider messages remain separate.
pub(crate) fn final_answer_text(messages: &[Message]) -> Option<String> {
    let mut start = messages.len().checked_sub(1)?;
    let no_tools = |message: &Message| {
        matches!(message, Message::Assistant { content, .. }
            if !content.iter().any(|block| matches!(block, AssistantBlock::ToolCall(_))))
    };
    let prompt = ContinueReason::Truncated.prompt();
    while start >= 2
        && no_tools(&messages[start])
        && matches!(&messages[start - 1], Message::User { content }
            if matches!(content.as_slice(), [UserBlock::Text(block)] if block.text == prompt))
        && no_tools(&messages[start - 2])
    {
        start -= 2;
    }
    let text: String = messages[start..]
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(content),
            Message::User { .. } => None,
        })
        .flatten()
        .filter_map(|block| match block {
            AssistantBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then_some(text)
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
    fn final_answer_joins_only_the_last_output_limit_chain() {
        let cutoff = || Message::user(ContinueReason::Truncated.prompt());
        let nudge = || Message::user(ContinueReason::Unattended { last: true }.prompt());
        let cases = [
            (
                vec![
                    Message::user("task"),
                    Message::assistant("I'll get started."),
                    nudge(),
                    Message::assistant("The answer"),
                    cutoff(),
                    Message::assistant(" begins"),
                    cutoff(),
                    Message::assistant(" here."),
                ],
                "The answer begins here.",
            ),
            (
                vec![
                    Message::assistant("discarded"),
                    cutoff(),
                    Message::assistant(" pause"),
                    nudge(),
                    Message::assistant("final summary"),
                ],
                "final summary",
            ),
            (
                vec![
                    Message::assistant("old answer"),
                    Message::user("new task"),
                    Message::assistant("new answer"),
                ],
                "new answer",
            ),
            (
                vec![
                    Message::assistant("I'll read the"),
                    cutoff(),
                    Message::Assistant {
                        id: None,
                        content: vec![
                            AssistantBlock::text("tool narration"),
                            AssistantBlock::tool_call(polaris::models::llm::ToolCall::new(
                                "call",
                                "read_file",
                                serde_json::json!({"path": "file"}),
                            )),
                        ],
                    },
                    Message::User {
                        content: vec![UserBlock::tool_result_with_call_id(
                            "call",
                            "call",
                            polaris::models::llm::ToolResultContent::Text("file contents".into()),
                        )],
                    },
                    Message::assistant("final answer"),
                    cutoff(),
                    Message::assistant(" suffix"),
                ],
                "final answer suffix",
            ),
        ];
        for (messages, expected) in cases {
            assert_eq!(final_answer_text(&messages).as_deref(), Some(expected));
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
        let policy = ContinuationPolicy::defaults().with_unattended_nudges(1);
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

    mod lifecycle {
        use std::sync::{Arc, Mutex};

        use futures_util::StreamExt;
        use polaris::agent::Agent;
        use polaris::graph::{GraphExecutor, MiddlewareAPI, middleware::info::SystemInfo};
        use polaris::models::llm::{
            ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest,
            LlmStream, StreamEvent, Usage,
        };
        use polaris::models::{ModelRegistry, ModelsPlugin};
        use polaris::plugins::{IOContent, IOMessage, PersistencePlugin};
        use polaris::sessions::{SessionId, SessionInfo};
        use polaris::system::plugin::{Plugin, PluginId, Version};
        use polaris::system::server::Server;
        use polaris::tools::ToolsPlugin;

        use super::*;
        use crate::agents::{
            AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, UnattendedGrants,
        };
        use crate::exec::{ExecIOPlugin, ExecIOProvider};
        use crate::usage::TokenMeter;

        /// Every call stops at the output limit, even after the budget expires.
        struct CutoffProvider;

        impl LlmProvider for CutoffProvider {
            fn name(&self) -> &'static str {
                "cutoff"
            }

            async fn generate(
                &self,
                _: &str,
                _: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                panic!("the agent must use streaming");
            }

            async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
                Ok(futures_util::stream::iter(vec![
                    Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::Text,
                    }),
                    Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::Text("fragment".into()),
                    }),
                    Ok(StreamEvent::ContentBlockStop { index: 0 }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::MaxOutputTokens,
                        usage: Usage::default(),
                        id: None,
                        model: None,
                    }),
                ])
                .boxed())
            }
        }

        impl Plugin for CutoffProvider {
            const ID: &'static str = "test::cutoff_provider";
            const VERSION: Version = Version::new(0, 1, 0);

            fn dependencies(&self) -> Vec<PluginId> {
                vec![PluginId::of::<ModelsPlugin>()]
            }

            fn build(&self, server: &mut Server) {
                server
                    .get_resource_mut::<ModelRegistry>()
                    .unwrap()
                    .register_llm_provider(Self);
            }
        }

        #[tokio::test]
        async fn real_graph_exhausts_continuations_and_resets_them_on_the_next_turn() {
            for nudges in [0, 1] {
                let workspace = tempfile::tempdir().unwrap();
                let (io, input, mut output) = ExecIOProvider::new(1024);
                let mut server = Server::new();
                server.insert_global(TokenMeter::new());
                server.insert_global(OxigraphStore::in_memory().unwrap());
                server
                    .add_plugins(ModelsPlugin)
                    .add_plugins(CutoffProvider)
                    .add_plugins(ToolsPlugin)
                    .add_plugins(PersistencePlugin)
                    .add_plugins(CodeAgentPlugin::new())
                    .add_plugins(ExecIOPlugin::new(Arc::new(io)));
                server.finish().await.unwrap();
                let mut ctx = server.create_context();
                ctx.insert(polaris::graph::RunCancellation::never());
                ctx.insert(AgentConfig::new("cutoff/model", workspace.path()));
                ctx.insert(SessionInfo {
                    session_id: SessionId::new(),
                    turn_number: 0,
                });
                CodeAgent.setup(&mut ctx).unwrap();
                {
                    let mut policy = ctx.get_resource_mut::<PolicySnapshot>().unwrap();
                    policy.permission =
                        PermissionMode::Unattended(UnattendedGrants::default()).permission_policy();
                    policy.continuation =
                        ContinuationPolicy::defaults().with_unattended_nudges(nudges);
                }
                let graph = CodeAgent.to_graph();
                assert!(graph.validate().is_ok());
                let seen = Arc::new(Mutex::new(Vec::new()));
                let middleware = MiddlewareAPI::new();
                let recorded = seen.clone();
                middleware.register_system(
                    "continuation_counts",
                    move |info: SystemInfo, ctx, next| {
                        let recorded = recorded.clone();
                        Box::pin(async move {
                            let ledger = *ctx.get_resource::<ContinuationLedger>().unwrap();
                            recorded
                                .lock()
                                .unwrap()
                                .push((info.node_name.to_string(), ledger));
                            next.run(ctx).await
                        })
                    },
                );
                for turn in 0..2 {
                    ctx.get_resource_mut::<SessionInfo>().unwrap().turn_number = turn;
                    input
                        .send(IOMessage::from_external(
                            "test",
                            IOContent::Text(format!("task {turn}")),
                        ))
                        .await
                        .unwrap();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        GraphExecutor::new().execute(&graph, &mut ctx, None, Some(&middleware)),
                    )
                    .await
                    .expect("the finite continuation budget must end the turn")
                    .unwrap();
                    let observed = std::mem::take(&mut *seen.lock().unwrap());
                    let ledgers = |name: &str| {
                        observed
                            .iter()
                            .filter(|(node, _)| node == name)
                            .map(|(_, ledger)| *ledger)
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(ledgers("reset_continuations").len(), 1);
                    assert_eq!(
                        ledgers("decide_continuation")[0],
                        ContinuationLedger::default()
                    );
                    let mut expected = (0..DEFAULT_TRUNCATION_CONTINUES)
                        .map(|truncations| used(truncations, 0))
                        .collect::<Vec<_>>();
                    if nudges > 0 {
                        expected.push(used(DEFAULT_TRUNCATION_CONTINUES, 0));
                    }
                    assert_eq!(ledgers("continue_turn"), expected);
                    assert_eq!(
                        ledgers("finalize"),
                        vec![used(DEFAULT_TRUNCATION_CONTINUES, nudges)]
                    );
                    assert_eq!(
                        ledgers("act").len(),
                        (DEFAULT_TRUNCATION_CONTINUES + nudges + 1) as usize
                    );
                    assert!(ledgers("recover").is_empty());
                    assert_eq!(
                        *ctx.get_resource::<ContinuationLedger>().unwrap(),
                        used(DEFAULT_TRUNCATION_CONTINUES, nudges)
                    );
                    let history = ctx.get_resource::<ContextManager>().unwrap();
                    assert_eq!(
                        history
                            .messages
                            .iter()
                            .filter(|message| is_continuation_prompt(message))
                            .count(),
                        ((turn + 1) * (DEFAULT_TRUNCATION_CONTINUES + nudges)) as usize
                    );
                    let expected_answer = "fragment".repeat(if nudges == 0 { 4 } else { 1 });
                    assert_eq!(
                        final_answer_text(&history.messages).as_deref(),
                        Some(expected_answer.as_str())
                    );
                    let mut final_text = None;
                    while let Ok(message) = output.try_recv() {
                        if message
                            .metadata
                            .get("final")
                            .is_some_and(|value| value == "true")
                        {
                            final_text = Some(message.content);
                        }
                    }
                    assert!(
                        matches!(final_text, Some(IOContent::Text(text)) if text == expected_answer)
                    );
                }
            }
        }
    }
}
