//! Real-agent proof that steering waits for model/tool boundaries.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest,
    LlmResponse, LlmStream, Message, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::PersistencePlugin;
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, tool};
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, TurnReason};
use polaris_code::terminal::TurnSteering;
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::panels::conversation::{TurnInputHandle, TurnInputMode};
use tokio::sync::Notify;

static TOOL_EFFECTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct ModelState {
    calls: AtomicUsize,
    first_entered: Notify,
    release_first: Notify,
}

struct SteeringProvider(Arc<ModelState>);

impl LlmProvider for SteeringProvider {
    fn name(&self) -> &'static str {
        "terminal-steering-test"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("CodeAgent streams")
    }

    async fn stream(&self, _: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let call = self.0.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            let state = self.0.clone();
            return Ok(futures_util::stream::once(async move {
                state.first_entered.notify_one();
                state.release_first.notified().await;
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::ToolCall {
                        id: "effect-1".into(),
                        call_id: None,
                        name: "record_effect".into(),
                    },
                })
            })
            .chain(futures_util::stream::iter([
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: "{}".into(),
                    },
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                    id: None,
                    model: None,
                }),
            ]))
            .boxed());
        }

        if call == 1 {
            let saw_steering = request.messages.iter().any(|message| match message {
                Message::User { content } => content
                    .iter()
                    .any(|block| format!("{block:?}").contains("use the safer direction")),
                Message::Assistant { .. } => false,
            });
            assert!(saw_steering, "next model call must include steering");
        }

        let text = if call == 1 { "steered" } else { "follow-up" };
        Ok(futures_util::stream::iter([
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(text.into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                id: None,
                model: None,
            }),
        ])
        .boxed())
    }
}

struct SteeringModelPlugin(Arc<ModelState>);

impl Plugin for SteeringModelPlugin {
    const ID: &'static str = "test::terminal_steering_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(SteeringProvider(self.0.clone()));
    }
}

#[tool]
/// Record one externally visible tool effect.
async fn record_effect() -> Result<String, ToolError> {
    TOOL_EFFECTS.fetch_add(1, Ordering::SeqCst);
    Ok("effect recorded".into())
}

struct SteeringToolPlugin;

impl Plugin for SteeringToolPlugin {
    const ID: &'static str = "test::terminal_steering_tool";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server.get_resource_mut::<ToolRegistry>().unwrap();
        registry.register(record_effect());
        registry
            .set_permission("record_effect", ToolPermission::Allow)
            .unwrap();
    }
}

#[tokio::test]
async fn steering_during_model_work_applies_after_tool_effect_then_follow_up_runs_once() {
    TOOL_EFFECTS.store(0, Ordering::SeqCst);
    let model = Arc::new(ModelState::default());
    let steering = TurnSteering::enabled();
    let workspace = tempfile::tempdir().unwrap();
    let (io, _input, _output) = ExecIOProvider::new(64);

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(SteeringModelPlugin(model.clone()))
        .add_plugins(ToolsPlugin)
        .add_plugins(SteeringToolPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(Arc::new(io)));
    server.finish().await.unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(CodeAgent).unwrap();
    let id = SessionId::new();
    let cfg = AgentConfig::new("terminal-steering-test/model", workspace.path());
    let session_steering = steering.clone();
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            move |ctx| {
                ctx.insert(cfg.clone());
                ctx.insert(session_steering.clone());
            },
        )
        .unwrap();

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { run::drive_turn(&turn_sessions, &turn_id).await });
    steering
        .submit("initial task".into(), TurnInputMode::Queue)
        .unwrap();
    model.first_entered.notified().await;
    let receipt = steering
        .submit("use the safer direction".into(), TurnInputMode::Steer)
        .unwrap();
    assert_eq!(receipt.target_run, 1);
    model.release_first.notify_one();
    let outcome = turn.await.unwrap();
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(outcome.final_text.as_deref(), Some("steered"));
    assert_eq!(TOOL_EFFECTS.load(Ordering::SeqCst), 1);

    let follow_up = steering
        .submit("verify the result".into(), TurnInputMode::Queue)
        .unwrap();
    assert_eq!(follow_up.target_run, 2);
    let outcome = run::drive_turn(sessions, &id).await;
    assert_eq!(outcome.final_text.as_deref(), Some("follow-up"));
    assert_eq!(TOOL_EFFECTS.load(Ordering::SeqCst), 1);

    let history = sessions
        .with_context(&id, |ctx| {
            ctx.get_resource::<ContextManager>()
                .unwrap()
                .messages
                .clone()
        })
        .await
        .unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|message| format!("{message:?}").contains("effect-1"))
            .count(),
        2,
        "one tool call and its one result remain provider-valid"
    );
}
