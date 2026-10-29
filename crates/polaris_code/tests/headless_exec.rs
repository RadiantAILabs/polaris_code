//! Proof that the non-interactive entry point drives one coding task through
//! the same session, real agent step, and executor as the TUI
//! (the non-interactive capability gate).
//!
//! Builds a server with a scripted LLM provider standing in for
//! `AnthropicPlugin` — no `ANTHROPIC_API_KEY`, no network — installs
//! [`ExecIOPlugin`] instead of the TUI panels exactly as `main.rs`'s headless
//! branch does, drives one turn with [`polaris_code::run::drive_turn`],
//! and checks the emitted `stream-json` events (rendered through the same
//! [`polaris_code::exec::stream_json_line`] the binary prints with), the
//! identity triple in all three output formats, and the exit-code mapping.

use std::path::Path;
use std::sync::Arc;

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager, LoopState};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider, OutputFormat, RunReport, stream_json_line};
use polaris_code::run::{self, TurnReason};
use polaris_code::terminal::history::CanonicalHistory;
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// The text answer [`ScriptedProvider`] returns from the real streaming step.
const ANSWER: &str = "all good, no tool calls needed.";

/// A deliberately different answer for the agent loop's recovery request.
///
/// Keeping the two provider paths observably distinct makes the primary test
/// fail if a missing resource (or another system error) silently substitutes
/// `recover` for the real streaming `act` step.
const RECOVERY_ANSWER: &str = "recovered after a system error";

/// The output token limit these reports state. [`ScriptedProvider`] has no
/// such limit, so this is what the shipped binary registers when nothing
/// configures one — the reported value is the registration, not a measurement.
const MAX_OUTPUT_TOKENS: u32 = 4096;

/// An `LlmProvider` that streams one fixed text answer and no tool calls while
/// returning a distinct answer to non-streaming recovery requests.
///
/// The distinction proves which path served the turn without needing a real
/// model or an API key.
struct ScriptedProvider;

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(RECOVERY_ANSWER)],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(ANSWER.to_string()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                    ..Default::default()
                },
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers [`ScriptedProvider`] with [`ModelRegistry`] once `ModelsPlugin`
/// has inserted it — the same seam `AnthropicPlugin` uses in production.
struct ScriptedModelPlugin;

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::scripted_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before ScriptedModelPlugin")
            .register_llm_provider(ScriptedProvider);
    }
}

/// An `LlmProvider` whose streaming step and its own recovery `generate` call
/// both fail, so a turn takes the doubly-failed-recovery path: `act`'s
/// retries exhaust, the graph's error edge routes into `recover`, and
/// `recover`'s own model call errors too — leaving no assistant answer at all.
struct FailingProvider;

impl LlmProvider for FailingProvider {
    fn name(&self) -> &'static str {
        "failing"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Err(GenerationError::Http("recovery call failed too".into()))
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        Err(GenerationError::Http("streaming call failed".into()))
    }
}

/// Registers [`FailingProvider`] the same seam [`ScriptedModelPlugin`] uses.
struct FailingModelPlugin;

impl Plugin for FailingModelPlugin {
    const ID: &'static str = "test::failing_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before FailingModelPlugin")
            .register_llm_provider(FailingProvider);
    }
}

/// The graph executor both front ends use, 25-iteration limit included.
fn executor() -> polaris::graph::GraphExecutor {
    polaris::graph::GraphExecutor::new().with_default_max_iterations(25)
}

/// Builds and finishes a server wired exactly as `main.rs`'s non-interactive
/// branch does: the scripted model instead of `AnthropicPlugin`, and
/// [`ExecIOPlugin`] instead of the TUI panels. `data_dir` backs the session
/// store, so two servers sharing one directory can hand a session over.
///
/// Returns the server (which owns the APIs the caller reads back) plus the
/// prompt sender and message receiver the front end keeps.
async fn headless_server(
    data_dir: &Path,
) -> (
    Server,
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
    mpsc::Receiver<IOMessage>,
) {
    let (provider, input_tx, output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);

    let mut server = Server::new();
    // `act` requires the same global meter that `main.rs` installs. Without
    // it, parameter fetching fails before the streaming model call and the
    // graph's error handler serves the turn through `recover` instead.
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));

    server.finish().await.unwrap();

    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).unwrap();

    (server, provider, input_tx, output_rx)
}

/// Enqueues one prompt the way the front end does — tagged
/// `IOSource::External`, and *before* the turn starts, because
/// `receive_user_input` swallows a closed channel rather than erroring.
async fn enqueue(input_tx: mpsc::Sender<IOMessage>, prompt: &str) {
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text(prompt.to_string()),
        ))
        .await
        .unwrap();
    drop(input_tx);
}

#[tokio::test]
async fn headless_turn_streams_events_and_reports_identity_and_exit_code() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();

    let (server, provider, input_tx, mut output_rx) = headless_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    // A fresh id, exactly as the non-interactive front end defaults.
    let session_id = SessionId::new();
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let agent_config = AgentConfig::new("scripted/test-model", workspace.path().to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &agent_type,
            executor(),
            {
                let cfg = agent_config.clone();
                move |ctx| {
                    ctx.insert(cfg.clone());
                }
            },
        )
        .unwrap();

    enqueue(input_tx, "say hi").await;

    // Drain concurrently, exactly as `stream-json` mode does: render each
    // `IOMessage` through the binary's own line renderer as it arrives.
    let drain = tokio::spawn(async move {
        let mut lines = Vec::new();
        while let Some(message) = output_rx.recv().await {
            lines.push(stream_json_line(&message).expect("IOMessage serializes"));
        }
        lines
    });

    let outcome = run::drive_turn(sessions, &session_id).await;

    provider.close().await;
    let stream_json_lines = drain.await.unwrap();
    assert!(
        OutputFormat::StreamJson.streams_messages(),
        "only `stream-json` prints these lines to stdout"
    );

    // The turn completed through the real streaming step: one iteration, no
    // tool calls. The provider's recovery answer is deliberately different,
    // so this assertion fails if the error handler served the turn instead.
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(outcome.reason.exit_code(), 0);
    assert_eq!(outcome.final_text.as_deref(), Some(ANSWER));
    assert_eq!(outcome.turn_number, 1, "the session's first turn completed");
    assert!(
        outcome.run_id.is_some(),
        "a completed turn always has an executor run id"
    );

    // The identity triple, as the three output formats actually report it.
    // `text` and `stream-json` reserve stdout for the answer / the message
    // stream, so their copy is the stderr identity line; `json` embeds the
    // same three identifiers in its stdout object. All three carry all three.
    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    let run_id = outcome.run_id.as_ref().unwrap().as_str();
    assert!(!session_id.as_str().is_empty());
    assert!(!run_id.is_empty());

    let identity = report.identity_line();
    for expected in [
        format!("session={}", session_id.as_str()),
        "turn=1".to_string(),
        format!("run_id={run_id}"),
    ] {
        assert!(
            identity.contains(&expected),
            "the stderr identity line {identity:?} is missing {expected:?}"
        );
    }

    let json = report
        .stdout_payload(OutputFormat::Json)
        .expect("`json` always prints one object");
    let object: serde_json::Value = serde_json::from_str(&json).expect("the object parses");
    assert_eq!(object["session_id"], session_id.as_str());
    assert_eq!(object["turn_number"], 1);
    assert_eq!(object["run_id"], run_id);
    assert_eq!(object["status"], "completed");
    assert_eq!(object["result"], ANSWER);

    // `text` prints exactly the final answer; `stream-json` has already
    // printed its lines and adds nothing at the end.
    assert_eq!(
        report.stdout_payload(OutputFormat::Text).as_deref(),
        Some(ANSWER)
    );
    assert_eq!(report.stdout_payload(OutputFormat::StreamJson), None);
    assert_eq!(report.exit_code(), 0);
    assert!(report.error_line().is_none());

    // stream-json events: at least one line, and each round-trips through
    // `IOMessage` exactly as a `stream-json` consumer would parse it.
    assert!(
        !stream_json_lines.is_empty(),
        "the agent's status/answer messages should have streamed"
    );
    let mut saw_final_answer = false;
    for line in &stream_json_lines {
        let message: IOMessage = serde_json::from_str(line).expect("each line is one IOMessage");
        if let IOContent::Text(text) = &message.content
            && text.contains(ANSWER)
        {
            saw_final_answer = true;
        }
    }
    assert!(
        saw_final_answer,
        "the final answer should appear in the streamed messages: {stream_json_lines:#?}"
    );
}

#[tokio::test]
async fn a_session_error_before_execution_is_reported_as_failed_with_exit_code_one() {
    // Driving a session id that was never created never reaches the
    // executor: no run id, turn number defaults to 0, and the reason names
    // the session error rather than panicking.
    let mut server = Server::new();
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            tempfile::tempdir().unwrap().path().to_str().unwrap(),
        ))))
        .add_plugins(CodeAgentPlugin::new());
    server.finish().await.unwrap();

    let sessions = server.api::<SessionsAPI>().unwrap();
    let session_id = SessionId::new();

    let outcome = run::drive_turn(sessions, &session_id).await;

    assert!(matches!(outcome.reason, TurnReason::Failed(_)));
    assert_eq!(outcome.reason.exit_code(), 1);
    assert!(outcome.run_id.is_none());
    assert!(outcome.final_text.is_none());

    // The front end still reports an identity line and a stderr diagnostic
    // rather than exiting silently, and `json` still emits one parseable
    // object naming the failure.
    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    assert_eq!(report.exit_code(), 1);
    assert!(report.identity_line().contains("run_id=-"));
    assert!(
        report
            .error_line()
            .expect("a failed turn reports a diagnostic")
            .contains("turn failed")
    );
    let json = report
        .stdout_payload(OutputFormat::Json)
        .expect("`json` always prints one object");
    let object: serde_json::Value = serde_json::from_str(&json).expect("the object parses");
    assert_eq!(object["status"], "failed");
    assert!(object["error"].is_string());
    assert_eq!(report.stdout_payload(OutputFormat::Text), None);
}

#[tokio::test]
async fn a_resumed_session_continues_its_turn_numbering() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let agent_config = AgentConfig::new("scripted/test-model", workspace.path().to_path_buf());
    let session_id = SessionId::new();

    // First non-interactive run: creates the session and drives turn 1, then
    // saves it exactly as `run_headless` does before exiting.
    {
        let (server, provider, input_tx, mut output_rx) = headless_server(data_dir.path()).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions
            .create_session_with_executor(
                server.create_context(),
                &session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                executor(),
                {
                    let cfg = agent_config.clone();
                    move |ctx| {
                        ctx.insert(cfg.clone());
                    }
                },
            )
            .unwrap();

        enqueue(input_tx, "first task").await;
        let drain = tokio::spawn(async move { while output_rx.recv().await.is_some() {} });
        let outcome = run::drive_turn(sessions, &session_id).await;
        provider.close().await;
        drain.await.unwrap();

        assert_eq!(outcome.reason, TurnReason::Completed);
        assert_eq!(outcome.turn_number, 1, "the session's first turn");
        sessions.save_session(&session_id).await.unwrap();
    }

    // Second run, a separate process in production: `--session <id>` resumes
    // the saved session instead of creating one, and the turn numbering
    // continues rather than restarting.
    let (server, provider, input_tx, mut output_rx) = headless_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .resume_session_with_executor(server.create_context(), &session_id, executor(), {
            let cfg = agent_config.clone();
            move |ctx| {
                ctx.insert(cfg.clone());
            }
        })
        .await
        .expect("the saved session resumes");

    enqueue(input_tx, "second task").await;
    let drain = tokio::spawn(async move { while output_rx.recv().await.is_some() {} });
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;
    drain.await.unwrap();

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(
        outcome.turn_number, 2,
        "a resumed session continues its own turn numbering"
    );

    // The identity triple a resumed run reports names the *same* session and
    // the next turn, with a new executor run id for this turn.
    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    assert!(report.identity_line().contains(session_id.as_str()));
    assert!(report.identity_line().contains("turn=2"));
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn repository_store_restarts_and_replays_two_sessions_without_running_tools() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let first = SessionId::from_string("first");
    let second = SessionId::from_string("second");
    let config = AgentConfig::new("scripted/test-model", workspace.path().to_path_buf());

    {
        let (server, provider, _input, _output) = headless_server(data_dir.path()).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        for id in [&first, &second] {
            sessions
                .create_session_with_executor(
                    server.create_context(),
                    id,
                    &AgentTypeId::from_name(CodeAgent::NAME),
                    executor(),
                    {
                        let config = config.clone();
                        move |ctx| {
                            ctx.insert(config.clone());
                        }
                    },
                )
                .unwrap();
        }
        sessions
            .with_context(&first, |ctx| {
                let mut history = ctx.get_resource_mut::<ContextManager>().unwrap();
                history.messages.push(Message::user("saved question"));
                history.messages.push(Message::assistant("saved answer"));
            })
            .await
            .unwrap();
        sessions.save_session(&first).await.unwrap();
        sessions.save_session(&second).await.unwrap();
        provider.close().await;
    }

    let (server, provider, _input, mut output) = headless_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let mut ids = sessions.list_sessions().await.unwrap();
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    assert_eq!(ids, [first.clone(), second.clone()]);
    sessions
        .resume_session_with_executor(server.create_context(), &first, executor(), {
            let config = config.clone();
            move |ctx| {
                ctx.insert(config.clone());
            }
        })
        .await
        .unwrap();
    let history = CanonicalHistory::default();
    history.refresh(sessions, &first).await.unwrap();
    let replay = history.replay().unwrap();
    assert_eq!(replay.len(), 2);
    assert!(matches!(&replay[0].content, IOContent::Text(text) if text == "saved question"));
    assert!(matches!(&replay[1].content, IOContent::Text(text) if text == "saved answer"));
    assert!(
        output.try_recv().is_err(),
        "replay reads never execute the graph or tools"
    );
    provider.close().await;
}

#[tokio::test]
async fn corrupt_resume_and_failed_save_are_explicit_errors() {
    let corrupt_dir = tempfile::tempdir().unwrap();
    tokio::fs::write(corrupt_dir.path().join("broken.json"), b"not-json")
        .await
        .unwrap();
    let (server, provider, _input, _output) = headless_server(corrupt_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let broken = SessionId::from_string("broken");
    let result = sessions
        .resume_session_with_executor(server.create_context(), &broken, executor(), |_| {})
        .await;
    assert!(result.is_err());
    assert!(!sessions.list_live_sessions().contains(&broken));
    provider.close().await;

    let root = tempfile::tempdir().unwrap();
    let not_a_directory = root.path().join("sessions");
    tokio::fs::write(&not_a_directory, b"occupied")
        .await
        .unwrap();
    let (server, provider, _input, _output) = headless_server(&not_a_directory).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::from_string("unsavable");
    let config = AgentConfig::new("scripted/test-model", root.path().to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            executor(),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .unwrap();
    assert!(sessions.save_session(&id).await.is_err());
    provider.close().await;
}

#[tokio::test]
async fn a_doubly_failed_recovery_reports_the_turn_as_failed_not_completed() {
    // `act`'s retries exhaust (every attempt errors), routing into `recover`
    // through the graph's error edge — and `recover`'s own model call fails
    // too, so there is no assistant answer for this turn. Before the fix, the
    // graph still reached its terminal `is_complete` state cleanly here, so
    // the turn reported `status: "completed"` with a null result instead of
    // surfacing the failure.
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (provider, input_tx, mut output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(FailingModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.path().to_str().unwrap(),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    server.finish().await.unwrap();

    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).unwrap();

    let session_id = SessionId::new();
    let agent_config = AgentConfig::new("failing/test-model", workspace.path().to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            executor(),
            move |ctx| {
                ctx.insert(agent_config.clone());
            },
        )
        .unwrap();

    enqueue(input_tx, "say hi").await;
    let drain = tokio::spawn(async move { while output_rx.recv().await.is_some() {} });
    let outcome = run::drive_turn(sessions, &session_id).await;

    // `recover` sets this on the `LoopState` *resource*, not just its own
    // graph-output return value — `run_one_turn` reads the resource, which
    // outlives this one run, to tell a doubly-failed recovery apart from an
    // ordinary completion.
    let loop_state_failure = sessions
        .with_context(&session_id, |ctx| {
            ctx.get_resource::<LoopState>()
                .ok()
                .and_then(|state| state.failure.clone())
        })
        .await
        .unwrap_or_default();
    provider.close().await;
    drain.await.unwrap();

    assert!(
        loop_state_failure.is_some(),
        "recover must record its failure on the LoopState resource, not only its own output"
    );
    assert!(
        matches!(outcome.reason, TurnReason::Failed(_)),
        "a doubly-failed model call must not report as completed: {:?}",
        outcome.reason
    );
    assert_eq!(outcome.reason.exit_code(), 1);
    assert!(outcome.final_text.is_none());

    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    assert_eq!(report.exit_code(), 1);
    let json = report
        .stdout_payload(OutputFormat::Json)
        .expect("`json` always prints one object");
    let object: serde_json::Value = serde_json::from_str(&json).expect("the object parses");
    assert_eq!(object["status"], "failed");
    assert!(object["error"].is_string());
    assert!(object["result"].is_null());
}
