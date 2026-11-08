//! Summary checkpoints through the real graph, command dispatch, and fake model.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use polaris::models::ModelsPlugin;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, ReasoningBlock, StopReason, StreamEvent, ToolCall,
    ToolResultContent, Usage, UserBlock,
};
use polaris::models::pricing::PriceStore;
use polaris::plugins::{IOMessage, PersistenceAPI, PersistencePlugin, UserIO};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::context::CompactionPlugin;
use polaris_code::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager, PolicySnapshot,
};
use polaris_code::exec::ExecIOProvider;
use polaris_code::model_run::{ModelRunOptions, ModelRunPlugin};
use polaris_code::run::{TurnReason, drive_turn};
use polaris_code::terminal::{CommandDispatch, CommandRegistry};
use polaris_code::trajectory::{RunTrajectory, rebuild_request};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::commands::{CommandInvocation, parse_status};

const SUMMARY: &str = r#"{"goal":"Fix the parser","decisions":["Keep the existing API"],"files_touched":["src/parser.rs: fix edge case"],"open_tasks":["Run the parser tests"],"key_facts":["Empty inputs caused the failure"]}"#;
const MODEL: &str = "fake/claude-sonnet-5";

/// Where a summary request stops answering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stall {
    /// The call never returns a stream.
    Open,
    /// The stream sends some text and then goes silent.
    Stream,
}

#[derive(Clone, Default)]
struct FakeModel {
    requests: Arc<Mutex<Vec<LlmRequest>>>,
    summaries: Arc<Mutex<Vec<LlmRequest>>>,
    answers: Arc<Mutex<Vec<LlmRequest>>>,
    failure: Option<&'static str>,
    response: Option<&'static str>,
    stream_failure: bool,
    stall: Option<Stall>,
}

impl LlmProvider for FakeModel {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("The agent and summary node use streaming generation")
    }

    async fn stream(&self, _: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt;
        self.requests.lock().unwrap().push(request.clone());
        let summary = request
            .system
            .as_deref()
            .is_some_and(|prompt| prompt.starts_with("Summarize the older conversation"));
        let (text, usage) = if summary {
            self.summaries.lock().unwrap().push(request);
            if let Some(error) = self.failure {
                return Err(GenerationError::Http(error.into()));
            }
            if self.stall == Some(Stall::Open) {
                std::future::pending::<()>().await;
            }
            (
                self.response.unwrap_or(SUMMARY),
                Usage {
                    input_tokens: Some(100),
                    output_tokens: Some(50),
                    cache_read_tokens: Some(20),
                    cache_creation_tokens: Some(10),
                    ..Default::default()
                },
            )
        } else {
            self.answers.lock().unwrap().push(request);
            (
                "Continue with the parser tests.",
                Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    ..Default::default()
                },
            )
        };
        let mut events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(text.into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
        ];
        if summary && self.stall == Some(Stall::Stream) {
            // The text arrives, then nothing ever does: no usage is reported
            // and no stop event closes the response.
            return Ok(futures_util::stream::iter(events)
                .chain(futures_util::stream::pending())
                .boxed());
        }
        if summary {
            events.push(Ok(StreamEvent::MessageDelta {
                usage: usage.clone(),
            }));
        }
        events.push(if summary && self.stream_failure {
            Err(GenerationError::Http("stream interrupted".into()))
        } else {
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage,
                id: None,
                model: None,
            })
        });
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct TestIOPlugin(Arc<CommandDispatch<ExecIOProvider>>);

impl Plugin for TestIOPlugin {
    const ID: &'static str = "test::compaction_io";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let provider = self.0.clone();
        server.register_local(move || UserIO::new(provider.clone()));
    }
}

fn history() -> Vec<Message> {
    let mut messages = Vec::new();
    for turn in 0..3 {
        messages.push(Message::user(format!(
            "Fix the parser: {}",
            "constraints ".repeat(200)
        )));
        let id = format!("read-{turn}");
        messages.push(Message::Assistant {
            id: None,
            content: vec![AssistantBlock::ToolCall(ToolCall::new(
                &id,
                "read_file",
                serde_json::json!({"path": "src/parser.rs"}),
            ))],
        });
        messages.push(Message::User {
            content: vec![UserBlock::tool_result_with_call_id(
                &id,
                &id,
                ToolResultContent::Text("observed parser behavior ".repeat(200)),
            )],
        });
        messages.push(Message::assistant("Completed investigation. ".repeat(200)));
    }
    for (index, message) in messages.iter_mut().enumerate() {
        if let Message::Assistant { content, .. } = message {
            content.insert(
                0,
                AssistantBlock::Reasoning(ReasoningBlock {
                    id: Some(format!("thought-{index}")),
                    reasoning: vec![
                        format!("reasoning for message {index}"),
                        "preserve all fragments".into(),
                    ],
                    signature: Some(format!("signature-{index}")),
                }),
            );
        }
    }
    messages
}

async fn run_case(
    model: FakeModel,
    manual: bool,
    cost_cap: Option<f64>,
) -> (ContextManager, FakeModel, TokenMeter, Vec<IOMessage>) {
    run_history_case(model, manual, cost_cap, history()).await
}

async fn run_history_case(
    model: FakeModel,
    manual: bool,
    cost_cap: Option<f64>,
    messages: Vec<Message>,
) -> (ContextManager, FakeModel, TokenMeter, Vec<IOMessage>) {
    let case = Case {
        manual,
        cost_cap,
        ..Case::default()
    };
    run_configured(model, case, messages).await
}

/// How a session under test is set up and driven.
struct Case {
    model_id: &'static str,
    /// Drive `/compact` rather than an ordinary prompt.
    manual: bool,
    cost_cap: Option<f64>,
    /// The longest a model call may go silent, when a test shortens it.
    attempt_timeout: Option<Duration>,
    /// How many times the input is sent, each driving one turn.
    turns: usize,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            model_id: MODEL,
            manual: false,
            cost_cap: None,
            attempt_timeout: None,
            turns: 1,
        }
    }
}

async fn run_configured(
    model: FakeModel,
    case: Case,
    messages: Vec<Message>,
) -> (ContextManager, FakeModel, TokenMeter, Vec<IOMessage>) {
    let Case {
        model_id,
        manual,
        cost_cap,
        attempt_timeout,
        turns,
    } = case;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (provider, input, mut output) = ExecIOProvider::new(128);
    let commands = Arc::new(CommandRegistry::with_builtins());
    let provider = Arc::new(CommandDispatch::new(
        Arc::new(provider),
        commands.clone(),
        workspace.path().to_path_buf(),
    ));
    let meter = TokenMeter::new().with_pricing_store(PriceStore::builtin());
    if let Some(limit) = cost_cap {
        meter.set_task_cost_cap(limit).unwrap();
    }
    let mut server = Server::new();
    server.insert_global(meter.clone());
    let trace_path = data.path().join("trajectory.json");
    let trace = RunTrajectory::create(
        &trace_path,
        "compaction-test",
        model_id,
        "Continue the work.",
    )
    .unwrap();
    server.insert_global(trace.clone());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(
            ModelRunPlugin::new(
                model.clone(),
                ModelRunOptions {
                    trajectory: Some(trace.clone()),
                    ..Default::default()
                },
            )
            .with_registry_name(model_id.split_once('/').unwrap().0),
        )
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data.path().to_str().unwrap(),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(CompactionPlugin::new(commands))
        .add_plugins(TestIOPlugin(provider));
    server.finish().await.unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(server.api::<PersistenceAPI>().unwrap().serializers());
    sessions.register_agent(CodeAgent).unwrap();
    let id = SessionId::new();
    let config = AgentConfig::new(model_id, workspace.path().to_path_buf());
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .unwrap();
    sessions.setup_session(&id).await.unwrap();
    sessions
        .with_context(&id, move |ctx| {
            ctx.get_resource_mut::<ContextManager>().unwrap().messages = messages;
            let mut policy = PolicySnapshot::defaults();
            policy.retrieval.compact_after_tokens = Some(200);
            policy.retrieval.summarize_after_tokens = Some(500);
            policy.retrieval.compact_keep_recent = 2;
            policy.retrieval.max_messages = Some(100);
            if let Some(timeout) = attempt_timeout {
                policy.retry.attempt_timeout = timeout;
            }
            ctx.insert(policy);
        })
        .await
        .unwrap();
    for _ in 0..turns {
        input
            .send(if manual {
                CommandInvocation::new("compact", "preserve the test failures")
                    .into_message("/compact")
            } else {
                IOMessage::user_text("Continue the work.")
            })
            .await
            .unwrap();
        let outcome = drive_turn(sessions, &id).await;
        assert!(
            matches!(outcome.reason, TurnReason::Completed),
            "{:?}",
            outcome.reason
        );
    }
    let context = sessions
        .with_context(&id, |ctx| {
            ctx.get_resource::<ContextManager>().unwrap().clone()
        })
        .await
        .unwrap();
    let mut shown = Vec::new();
    while let Ok(message) = output.try_recv() {
        shown.push(message);
    }
    trace.finish(0).unwrap();
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(trace_path).unwrap()).unwrap();
    let recorded: Vec<_> = document["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["extra"]["kind"] == "model_call")
        .collect();
    let requests = model.requests.lock().unwrap();
    assert_eq!(recorded.len(), requests.len());
    for (step, request) in recorded.iter().zip(requests.iter()) {
        let step_id = step["step_id"].as_u64().unwrap() as usize;
        assert_eq!(
            serde_json::to_value(rebuild_request(&document, step_id).unwrap()).unwrap(),
            serde_json::to_value(request).unwrap(),
            "request at step {step_id} must survive summary and elision"
        );
    }
    drop(requests);
    (context, model, meter, shown)
}

#[tokio::test]
async fn threshold_compaction_keeps_canonical_history_and_projects_the_checkpoint() {
    let (context, model, meter, shown) = run_case(FakeModel::default(), false, None).await;
    assert_eq!(context.checkpoints.len(), 1);
    let original = history();
    assert_eq!(
        serde_json::to_value(&context.messages[..original.len()]).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(
        context.messages.len(),
        14,
        "only the new prompt and answer are appended"
    );
    let calls = model.summaries.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].max_output_tokens.unwrap().get(), 2_048);
    assert!(calls[0].tools.is_none());
    let answers = model.answers.lock().unwrap();
    assert_eq!(answers.len(), 1);
    for request in [&calls[0], &answers[0]] {
        let assistants = request
            .messages
            .iter()
            .filter(|message| matches!(message, Message::Assistant { .. }))
            .collect::<Vec<_>>();
        assert!(!assistants.is_empty());
        for message in assistants {
            assert!(
                original
                    .iter()
                    .any(|source| serde_json::to_value(source).unwrap()
                        == serde_json::to_value(message).unwrap()),
                "summary input and retained history preserve complete assistant messages, including reasoning and signatures"
            );
        }
    }
    let projected = serde_json::to_string(&answers[0].messages).unwrap();
    assert!(projected.contains("Conversation summary checkpoint"));
    assert!(projected.contains("Empty inputs caused the failure"));
    assert!(answers[0].cache.prefix);
    assert!(
        answers[0]
            .cache
            .breakpoints
            .iter()
            .all(|&i| i < answers[0].messages.len())
    );
    assert_eq!(meter.snapshot(MODEL).input_tokens, 110);
    assert_eq!(meter.snapshot(MODEL).output_tokens, 55);
    assert_eq!(meter.snapshot(MODEL).num_turns, 2);
    assert!(
        shown
            .iter()
            .filter_map(parse_status)
            .any(|status| status.context.is_some_and(|window| window.compacting))
    );

    let saved = serde_json::to_string(&context).unwrap();
    let mut restored: ContextManager = serde_json::from_str(&saved).unwrap();
    restored.reset_runtime(true);
    let policy = PolicySnapshot::defaults();
    assert_eq!(
        serde_json::to_value(context.project(&policy.retrieval).messages).unwrap(),
        serde_json::to_value(restored.project(&policy.retrieval).messages).unwrap()
    );
    // Restoring a checkpoint before compaction removes its summary with its history.
    restored.messages.truncate(1);
    assert!(
        !serde_json::to_string(&restored.project(&policy.retrieval).messages)
            .unwrap()
            .contains("Conversation summary checkpoint")
    );
}

#[tokio::test]
async fn manual_compaction_preserves_focus_and_skips_the_answer_call() {
    let (context, model, meter, shown) = run_case(FakeModel::default(), true, None).await;
    assert_eq!(
        serde_json::to_value(&context.messages).unwrap(),
        serde_json::to_value(history()).unwrap()
    );
    assert_eq!(context.checkpoints.len(), 1);
    let calls = model.summaries.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        serde_json::to_string(&calls[0].messages)
            .unwrap()
            .contains("preserve the test failures")
    );
    assert!(model.answers.lock().unwrap().is_empty());
    assert_eq!(meter.snapshot(MODEL).input_tokens, 100);
    assert!(shown.iter().filter_map(parse_status).any(|status| {
        status
            .session
            .is_some_and(|label| label.contains("summarized at turn"))
    }));
}

#[tokio::test]
async fn failed_summary_falls_back_to_elision_and_the_turn_completes() {
    let model = FakeModel {
        failure: Some("summary unavailable"),
        ..Default::default()
    };
    let (context, model, meter, _) = run_case(model, false, None).await;
    assert!(context.checkpoints.is_empty());
    assert_eq!(model.summaries.lock().unwrap().len(), 1);
    let answers = model.answers.lock().unwrap();
    assert_eq!(answers.len(), 1);
    assert!(
        serde_json::to_string(&answers[0].messages)
            .unwrap()
            .contains("[elided ")
    );
    assert_eq!(meter.snapshot(MODEL).input_tokens, 10);
}

#[tokio::test]
async fn compaction_uses_the_selected_models_reasoning_replay() {
    for (model_id, fragments, summaries) in [
        (
            "aws.bedrock/global.openai.gpt-oss-120b-1:0",
            vec!["x".repeat(8_000), "keep".into()],
            0,
        ),
        ("anthropic/test-model", vec![String::new(); 4_000], 1),
    ] {
        let messages = vec![
            Message::user("Fix the parser"),
            Message::Assistant {
                id: None,
                content: vec![
                    AssistantBlock::Reasoning(ReasoningBlock {
                        id: Some("thinking".into()),
                        reasoning: fragments,
                        signature: Some("signature".into()),
                    }),
                    AssistantBlock::text("Investigated the parser"),
                ],
            },
            Message::user("Keep working"),
            Message::assistant("Ready to test"),
        ];
        let case = Case {
            model_id,
            ..Case::default()
        };
        let (context, model, _, _) = run_configured(FakeModel::default(), case, messages).await;
        assert_eq!(
            model.summaries.lock().unwrap().len(),
            summaries,
            "{model_id}"
        );
        assert_eq!(context.checkpoints.len(), summaries, "{model_id}");
        assert_eq!(model.answers.lock().unwrap().len(), 1, "{model_id}");
    }
}

#[tokio::test]
async fn summary_cost_reservation_refuses_before_calling_the_provider() {
    let (context, model, meter, _) = run_case(FakeModel::default(), true, Some(0.000_001)).await;
    assert!(context.checkpoints.is_empty());
    assert!(model.summaries.lock().unwrap().is_empty());
    assert!(model.answers.lock().unwrap().is_empty());
    assert_eq!(meter.snapshot(MODEL).input_tokens, 0);
    assert!(meter.cost_cap_stopped());
}

#[tokio::test]
async fn malformed_summary_is_billed_under_the_ceiling_and_keeps_elision() {
    let model = FakeModel {
        response: Some("not a structured summary"),
        ..Default::default()
    };
    let (context, model, meter, _) = run_case(model, true, Some(1.0)).await;
    assert!(context.checkpoints.is_empty());
    assert_eq!(model.summaries.lock().unwrap().len(), 1);
    assert!(model.answers.lock().unwrap().is_empty());
    let usage = meter.snapshot(MODEL);
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.cache_read_tokens, 20);
    assert_eq!(usage.cache_creation_tokens, 10);
    assert!(usage.estimated_cost_usd.unwrap() > 0.0);
    assert!(!meter.cost_cap_stopped());
}

#[tokio::test]
async fn failed_summary_stream_still_bills_reported_usage_and_keeps_elision() {
    let model = FakeModel {
        stream_failure: true,
        ..Default::default()
    };
    let (context, model, meter, _) = run_case(model, true, Some(1.0)).await;
    assert!(context.checkpoints.is_empty());
    assert_eq!(model.summaries.lock().unwrap().len(), 1);
    assert!(model.answers.lock().unwrap().is_empty());
    let usage = meter.snapshot(MODEL);
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.cache_read_tokens, 20);
    assert_eq!(usage.cache_creation_tokens, 10);
    assert!(usage.estimated_cost_usd.unwrap() > 0.0);
    assert!(!meter.cost_cap_stopped());
}

#[tokio::test]
async fn many_small_messages_do_not_request_a_summary_before_sliding() {
    let original: Vec<_> = (0..60)
        .flat_map(|_| [Message::user("ok"), Message::assistant("ok")])
        .collect();
    let (context, model, meter, _) =
        run_history_case(FakeModel::default(), false, None, original.clone()).await;
    assert!(context.checkpoints.is_empty());
    assert!(model.summaries.lock().unwrap().is_empty());
    let answers = model.answers.lock().unwrap();
    assert_eq!(answers.len(), 1);
    assert!(answers[0].messages.len() <= 100);
    assert_eq!(meter.snapshot(MODEL).input_tokens, 10);
    assert_eq!(
        serde_json::to_value(&context.messages[..original.len()]).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}

#[tokio::test]
async fn a_long_tool_loop_summarizes_completed_exchanges_before_another_user_boundary() {
    let mut original = history();
    let mut user_turns = 0;
    original.retain(|message| {
        if let Message::User { content } = message
            && content
                .iter()
                .all(|block| !matches!(block, UserBlock::ToolResult(_)))
        {
            user_turns += 1;
            return user_turns == 1;
        }
        true
    });
    assert!(matches!(original.pop(), Some(Message::Assistant { .. })));
    let (context, model, _, _) =
        run_history_case(FakeModel::default(), false, None, original.clone()).await;
    assert_eq!(context.checkpoints.len(), 1);
    assert!(matches!(
        context.messages[context.checkpoints[0].through_messages],
        Message::Assistant { .. }
    ));
    assert_eq!(
        serde_json::to_value(&context.messages[..original.len()]).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(model.summaries.lock().unwrap().len(), 1);
    let answers = model.answers.lock().unwrap();
    assert_eq!(answers.len(), 1);
    assert!(
        serde_json::to_string(&answers[0].messages)
            .unwrap()
            .contains("Conversation summary checkpoint")
    );
    let mut pending = std::collections::HashSet::new();
    let mut results = 0;
    for message in &answers[0].messages {
        match message {
            Message::Assistant { content, .. } => {
                for block in content {
                    if let AssistantBlock::ToolCall(call) = block {
                        pending.insert(call.id.as_str());
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let UserBlock::ToolResult(result) = block {
                        results += 1;
                        assert!(
                            pending.remove(result.id.as_str()),
                            "orphan result {}",
                            result.id
                        );
                    }
                }
            }
        }
    }
    assert!(
        results > 0,
        "the recent projection still contains tool results"
    );
    assert!(pending.is_empty());
}

/// How long a stalled summary call is allowed to stay silent. A stalled call
/// waits this long however slow the host is, so no test depends on a margin.
const STALL_TIMEOUT: Duration = Duration::from_millis(600);

/// The most a stalled case may take. A regression fails after this instead of
/// hanging the suite.
const GIVE_UP: Duration = Duration::from_secs(60);

async fn run_stalled(
    model: FakeModel,
    manual: bool,
    cost_cap: Option<f64>,
    turns: usize,
) -> (ContextManager, FakeModel, TokenMeter, Vec<IOMessage>) {
    let case = Case {
        manual,
        cost_cap,
        attempt_timeout: Some(STALL_TIMEOUT),
        turns,
        ..Case::default()
    };
    tokio::time::timeout(GIVE_UP, run_configured(model, case, history()))
        .await
        .expect("a stalled summary call must not hold the turn")
}

fn mentions_elision(request: &LlmRequest) -> bool {
    serde_json::to_string(&request.messages)
        .unwrap()
        .contains("[elided ")
}

#[tokio::test]
async fn a_summary_call_that_never_starts_falls_back_to_elision_and_the_turn_completes() {
    let model = FakeModel {
        stall: Some(Stall::Open),
        ..Default::default()
    };
    let (context, model, _, _) = run_stalled(model, false, None, 1).await;

    assert!(context.checkpoints.is_empty());
    assert_eq!(model.summaries.lock().unwrap().len(), 1);
    let answers = model.answers.lock().unwrap();
    assert_eq!(answers.len(), 1, "the turn went on to its answer");
    assert!(mentions_elision(&answers[0]));
}

#[tokio::test]
async fn a_summary_stream_that_goes_quiet_falls_back_to_elision_and_the_turn_completes() {
    let model = FakeModel {
        stall: Some(Stall::Stream),
        ..Default::default()
    };
    let (context, model, _, _) = run_stalled(model, false, None, 1).await;

    assert!(context.checkpoints.is_empty());
    assert_eq!(model.summaries.lock().unwrap().len(), 1);
    let answers = model.answers.lock().unwrap();
    assert_eq!(answers.len(), 1);
    assert!(mentions_elision(&answers[0]));
}

#[tokio::test]
async fn a_stalled_manual_compaction_says_the_summary_is_unavailable() {
    for stall in [Stall::Open, Stall::Stream] {
        let model = FakeModel {
            stall: Some(stall),
            ..Default::default()
        };
        let (context, model, _, shown) = run_stalled(model, true, None, 1).await;

        assert!(context.checkpoints.is_empty(), "{stall:?}");
        assert!(
            model.answers.lock().unwrap().is_empty(),
            "{stall:?}: a manual request skips the answer call"
        );
        let notices: Vec<String> = shown
            .iter()
            .filter_map(|message| match &message.content {
                polaris::plugins::IOContent::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|text| text == "Summary unavailable; kept the existing context projection."),
            "{stall:?}: {notices:?}"
        );
    }
}

#[tokio::test]
async fn a_summary_that_timed_out_stays_charged_against_the_cost_cap() {
    // Calibrate: the request a manual `/compact` sends over this history, and
    // so the most it can cost.
    let (_, calibration, _, _) = run_case(FakeModel::default(), true, None).await;
    let request = calibration.summaries.lock().unwrap()[0].clone();
    let reserved = polaris_code::usage::price_for(&PriceStore::builtin(), MODEL)
        .expect("the built-in prices know the model")
        .cost_usd(
            0,
            2_048,
            0,
            polaris_code::usage::request_input_bound(&request),
        );

    // A cap that fits one reservation but not two. The first summary stalls
    // and is abandoned with its request in flight; the provider may bill it,
    // so its estimate stays spent and the second request does not fit.
    let model = FakeModel {
        stall: Some(Stall::Open),
        ..Default::default()
    };
    let (_, model, meter, _) = run_stalled(model, true, Some(reserved * 1.5), 2).await;

    assert_eq!(
        model.summaries.lock().unwrap().len(),
        1,
        "the second request was refused before it was sent: the first one's estimate was released instead of charged"
    );
    assert!(meter.cost_cap_stopped());
}
