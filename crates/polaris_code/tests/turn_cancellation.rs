//! Real-CodeAgent cancellation coverage for both in-node await points.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, ToolResultContent, Usage,
    UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, IOProvider, PersistencePlugin};
use polaris::sessions::{
    AgentTypeId, InMemoryStore, SessionId, SessionStatus, SessionsAPI, SessionsPlugin,
};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, tool};
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, StatusEmitter, TurnReason};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::Notify;

const MODE_SLOW_STREAM: u8 = 0;
const MODE_SLOW_TOOL: u8 = 1;
const MODE_COMPLETE: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(MODE_SLOW_STREAM);
static ACT_ENTERED: OnceLock<Notify> = OnceLock::new();
static TOOL_ENTERED: OnceLock<Notify> = OnceLock::new();

fn act_entered() -> &'static Notify {
    ACT_ENTERED.get_or_init(Notify::new)
}

fn tool_entered() -> &'static Notify {
    TOOL_ENTERED.get_or_init(Notify::new)
}

struct ScriptedProvider;

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "real-cancellation-test"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("CodeAgent uses streaming generation")
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        let stream: LlmStream = match MODE.load(Ordering::SeqCst) {
            MODE_SLOW_STREAM => futures_util::stream::once(async {
                act_entered().notify_one();
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                })
            })
            .chain(futures_util::stream::pending())
            .boxed(),
            MODE_SLOW_TOOL => futures_util::stream::iter([
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::ToolCall {
                        id: "slow-1".into(),
                        call_id: None,
                        name: "slow_fixture".into(),
                    },
                }),
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
            ])
            .boxed(),
            MODE_COMPLETE => futures_util::stream::iter([
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("recovered".into()),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                    id: None,
                    model: None,
                }),
            ])
            .boxed(),
            mode => panic!("unexpected scripted mode {mode}"),
        };
        Ok(stream)
    }
}

struct ScriptedModelPlugin;

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::real_cancellation_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(ScriptedProvider);
    }
}

#[tool]
/// A tool that remains pending until the turn cancellation drops its future.
async fn slow_fixture() -> Result<String, ToolError> {
    tool_entered().notify_one();
    std::future::pending().await
}

struct SlowToolPlugin;

impl Plugin for SlowToolPlugin {
    const ID: &'static str = "test::real_cancellation_tool";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server.get_resource_mut::<ToolRegistry>().unwrap();
        registry.register(slow_fixture());
        registry
            .set_permission("slow_fixture", ToolPermission::Allow)
            .unwrap();
    }
}

fn tool_ids(messages: &[Message]) -> (Vec<String>, Vec<String>) {
    let mut uses = Vec::new();
    let mut results = Vec::new();
    for message in messages {
        match message {
            Message::Assistant { content, .. } => {
                for block in content {
                    if let AssistantBlock::ToolCall(call) = block {
                        uses.push(call.id.clone());
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let UserBlock::ToolResult(result) = block {
                        results.push(result.id.clone());
                        assert!(matches!(result.content, ToolResultContent::Text(_)));
                    }
                }
            }
        }
    }
    (uses, results)
}

#[tokio::test]
async fn headless_real_agent_interrupts_mid_stream_and_mid_tool_then_resumes() {
    MODE.store(MODE_SLOW_STREAM, Ordering::SeqCst);
    let workspace = tempfile::tempdir().unwrap();
    let (provider, input, mut output) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let status = StatusEmitter::new(provider.clone());

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(SlowToolPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    server.finish().await.unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(CodeAgent).unwrap();
    let id = SessionId::new();
    let cfg = AgentConfig::new(
        "real-cancellation-test/model",
        workspace.path().to_path_buf(),
    );
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            move |ctx| {
                ctx.insert(cfg.clone());
            },
        )
        .unwrap();

    input
        .send(IOMessage::from_external(
            "test",
            IOContent::Text("stream".into()),
        ))
        .await
        .unwrap();
    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn_status = status.clone();
    let turn = tokio::spawn(async move {
        run::drive_turn_with_status(&turn_sessions, &turn_id, Some(&turn_status)).await
    });
    act_entered().notified().await;
    assert!(run::request_interrupt(sessions, &id));
    let outcome = turn.await.unwrap();
    assert_eq!(outcome.reason, TurnReason::Interrupted);
    assert_eq!(outcome.reason.exit_code(), 130);

    let mut lifecycle = Vec::new();
    while let Ok(message) = output.try_recv() {
        if let Some(io_type) = message.metadata.get(IO_TYPE)
            && matches!(io_type.as_str(), "interrupting" | "cancelled")
        {
            lifecycle.push(io_type.clone());
        }
    }
    assert_eq!(lifecycle, ["interrupting", "cancelled"]);
    assert_eq!(
        sessions.session_info(&id).unwrap().status,
        SessionStatus::Active
    );
    assert!(
        sessions
            .checkpoints()
            .list(&id)
            .unwrap()
            .iter()
            .any(|point| point.resume_turn == 1)
    );

    MODE.store(MODE_SLOW_TOOL, Ordering::SeqCst);
    input
        .send(IOMessage::from_external(
            "test",
            IOContent::Text("tool".into()),
        ))
        .await
        .unwrap();
    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { run::drive_turn(&turn_sessions, &turn_id).await });
    tool_entered().notified().await;
    assert!(run::request_interrupt(sessions, &id));
    assert_eq!(turn.await.unwrap().reason, TurnReason::Interrupted);

    let history = sessions
        .with_context(&id, |ctx| {
            ctx.get_resource::<ContextManager>()
                .unwrap()
                .messages
                .clone()
        })
        .await
        .unwrap();
    let (uses, results) = tool_ids(&history);
    assert_eq!(uses, ["slow-1"]);
    assert_eq!(
        results,
        ["slow-1"],
        "canonical history must pair every tool use"
    );
    assert!(
        sessions
            .checkpoints()
            .list(&id)
            .unwrap()
            .iter()
            .any(|point| point.resume_turn == 2)
    );

    MODE.store(MODE_COMPLETE, Ordering::SeqCst);
    input
        .send(IOMessage::from_external(
            "test",
            IOContent::Text("continue".into()),
        ))
        .await
        .unwrap();
    let resumed = run::drive_turn(sessions, &id).await;
    assert_eq!(resumed.reason, TurnReason::Completed);
    assert_eq!(resumed.final_text.as_deref(), Some("recovered"));
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 3);
    provider.close().await;
}
