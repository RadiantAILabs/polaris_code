//! Consumer proof for cooperative cancellation at the coding-agent boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use polaris::agent::Agent;
use polaris::graph::Graph;
use polaris::graph::cancel::RunCancellation;
use polaris::graph::hooks::HooksAPI;
use polaris::graph::hooks::events::GraphEvent;
use polaris::graph::hooks::schedule::OnSystemComplete;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, ToolCall,
    ToolResultContent, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistencePlugin};
use polaris::sessions::{
    AgentTypeId, InMemoryStore, SessionError, SessionId, SessionStatus, SessionsAPI, SessionsPlugin,
};
use polaris::system::param::{Out, Res, ResMut};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::resource::LocalResource;
use polaris::system::server::Server;
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, tool};
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::Notify;

fn expect_interrupted(error: SessionError) {
    assert!(
        matches!(error, SessionError::Interrupted { .. }),
        "{error:?}"
    );
}

struct ToolCallingProvider;

static REAL_STREAM_CALLS: AtomicU32 = AtomicU32::new(0);

impl LlmProvider for ToolCallingProvider {
    fn name(&self) -> &'static str {
        "cancel-proof"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("recovered")],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let events = if REAL_STREAM_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
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
            ]
        } else {
            vec![
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("done".into()),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                    id: None,
                    model: None,
                }),
            ]
        };
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct ModelPlugin;
impl Plugin for ModelPlugin {
    const ID: &'static str = "test::cancel_model";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(ToolCallingProvider);
    }
}

static SLOW_TOOL_STARTED: AtomicBool = AtomicBool::new(false);

#[tool]
/// A deliberately slow fixture that must not start after a between-node stop.
async fn slow_fixture() -> Result<String, ToolError> {
    SLOW_TOOL_STARTED.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    Ok("done".into())
}

struct SlowToolPlugin;
impl Plugin for SlowToolPlugin {
    const ID: &'static str = "test::slow_tool";
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

#[tokio::test]
async fn real_code_agent_stops_between_nodes_before_the_slow_tool_starts() {
    SLOW_TOOL_STARTED.store(false, Ordering::SeqCst);
    REAL_STREAM_CALLS.store(0, Ordering::SeqCst);
    let workspace = tempfile::tempdir().unwrap();
    let (provider, input, mut output) = ExecIOProvider::new(16);
    let provider = Arc::new(provider);
    let mut server = Server::new();
    server.insert_api(HooksAPI::new());
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ModelPlugin)
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
    let cfg = AgentConfig::new("cancel-proof/model", workspace.path().to_path_buf());
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
            IOContent::Text("use the slow fixture".into()),
        ))
        .await
        .unwrap();

    let hook_sessions = sessions.clone();
    let hook_id = id.clone();
    let starts_after_act = Arc::new(AtomicU32::new(0));
    let starts_probe = starts_after_act.clone();
    server
        .api::<HooksAPI>()
        .unwrap()
        .register_observer::<OnSystemComplete, _>(
            "cancel_code_agent_after_act",
            move |event: &GraphEvent| {
                if let GraphEvent::SystemComplete { node_name, .. } = event
                    && node_name == "act"
                    && starts_probe.fetch_add(1, Ordering::SeqCst) == 0
                {
                    assert!(hook_sessions.interrupt_turn(&hook_id).unwrap());
                }
            },
        )
        .unwrap();

    let result = sessions.process_turn(&id).await;
    if result.is_ok() {
        let mut observed = Vec::new();
        while let Ok(message) = output.try_recv() {
            observed.push(message);
        }
        panic!("expected interruption, observed output: {observed:?}");
    }
    let error = result.unwrap_err();
    expect_interrupted(error);
    assert_eq!(starts_after_act.load(Ordering::SeqCst), 1);
    assert!(
        !SLOW_TOOL_STARTED.load(Ordering::SeqCst),
        "no node after act may start"
    );
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
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 1);

    input
        .send(IOMessage::from_external(
            "test",
            IOContent::Text("finish normally".into()),
        ))
        .await
        .unwrap();
    sessions
        .process_turn(&id)
        .await
        .expect("the real CodeAgent's next turn succeeds");
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 2);
    provider.close().await;
    while output.try_recv().is_ok() {}
}

#[derive(Clone, Copy)]
enum StopAt {
    Input = 0,
    Act = 1,
    Tools = 2,
    None = 3,
}

#[derive(Clone)]
struct Probe {
    stop_at: Arc<AtomicU8>,
    entered: Arc<Notify>,
    recoveries: Arc<AtomicU32>,
    later_nodes: Arc<AtomicU32>,
    queued_prompts: Arc<AtomicU32>,
}
impl LocalResource for Probe {}

#[derive(Clone)]
struct Reply {
    call: Option<ToolCall>,
}

#[system]
async fn local_receive(cancel: Res<RunCancellation>, probe: Res<Probe>) -> Result<(), SystemError> {
    if probe.stop_at.load(Ordering::SeqCst) == StopAt::Input as u8 {
        probe.entered.notify_one();
        if cancel
            .run_until_cancelled(std::future::pending::<()>())
            .await
            .is_none()
        {
            return Err(SystemError::ExecutionError("interrupted input".into()));
        }
    }
    probe.queued_prompts.fetch_sub(1, Ordering::SeqCst);
    Ok(())
}
#[system]
async fn init() -> bool {
    false
}
#[system]
async fn prepare() {}

#[system]
async fn local_act(
    cancel: Res<RunCancellation>,
    probe: Res<Probe>,
    mut context: ResMut<ContextManager>,
) -> Result<Reply, SystemError> {
    if probe.stop_at.load(Ordering::SeqCst) == StopAt::Act as u8 {
        probe.entered.notify_one();
        if cancel
            .run_until_cancelled(std::future::pending::<()>())
            .await
            .is_none()
        {
            return Err(SystemError::ExecutionError("interrupted act".into()));
        }
    }
    let call = if probe.stop_at.load(Ordering::SeqCst) == StopAt::Tools as u8 {
        Some(ToolCall::new(
            "local-slow-1",
            "slow_fixture",
            serde_json::json!({}),
        ))
    } else {
        None
    };
    context.push(Message::Assistant {
        id: None,
        content: call.clone().map_or_else(
            || vec![AssistantBlock::text("complete")],
            |call| vec![AssistantBlock::tool_call(call)],
        ),
    });
    Ok(Reply { call })
}

#[system]
async fn local_tools(
    reply: Out<Reply>,
    cancel: Res<RunCancellation>,
    probe: Res<Probe>,
    mut context: ResMut<ContextManager>,
) -> Result<(), SystemError> {
    let call = reply.call.as_ref().expect("tool branch requires a call");
    probe.entered.notify_one();
    if cancel
        .run_until_cancelled(std::future::pending::<()>())
        .await
        .is_none()
    {
        context.push(Message::User {
            content: vec![UserBlock::tool_error(
                &call.id,
                ToolResultContent::Text("[Request interrupted by user for tool use]".into()),
            )],
        });
        return Err(SystemError::ExecutionError("interrupted tools".into()));
    }
    context.push(Message::User {
        content: vec![UserBlock::tool_error(
            &call.id,
            ToolResultContent::Text("completed".into()),
        )],
    });
    Ok(())
}

#[system]
async fn finalize(probe: Res<Probe>) -> bool {
    probe.later_nodes.fetch_add(1, Ordering::SeqCst);
    true
}
#[system]
async fn recover(probe: Res<Probe>) {
    probe.recoveries.fetch_add(1, Ordering::SeqCst);
}

struct ConsumerAgent;
impl Agent for ConsumerAgent {
    fn name(&self) -> &'static str {
        "cancellation-consumer"
    }
    fn build(&self, graph: &mut Graph) {
        graph.add_system(local_receive).add_system(init);
        graph.add_loop::<bool, _, _>(
            "agent_loop",
            |done| *done,
            |g| {
                g.add_system(prepare)
                    .add_system(local_act)
                    .add_conditional_branch::<Reply, _, _, _>(
                        "has_tool_calls",
                        |r| r.call.is_some(),
                        |tools| {
                            tools.add_system(local_tools).add_system(finalize);
                        },
                        |done| {
                            done.add_system(finalize);
                        },
                    );
                g.add_error_handler(|h| {
                    h.add_system(recover);
                });
            },
        );
    }
}

async fn prove_in_node_stop(stop_at: StopAt) {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
    server.finish().await.unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(ConsumerAgent).unwrap();
    let id = SessionId::new();
    let probe = Probe {
        stop_at: Arc::new(AtomicU8::new(stop_at as u8)),
        entered: Arc::new(Notify::new()),
        recoveries: Arc::new(AtomicU32::new(0)),
        later_nodes: Arc::new(AtomicU32::new(0)),
        queued_prompts: Arc::new(AtomicU32::new(2)),
    };
    let inserted = probe.clone();
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("cancellation-consumer"),
            move |ctx| {
                ctx.insert(inserted.clone());
                ctx.insert(ContextManager::default());
            },
        )
        .unwrap();
    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    probe.entered.notified().await;
    assert!(sessions.interrupt_turn(&id).unwrap());
    expect_interrupted(turn.await.unwrap().unwrap_err());
    assert_eq!(
        probe.recoveries.load(Ordering::SeqCst),
        0,
        "recover must not run"
    );
    assert_eq!(
        probe.later_nodes.load(Ordering::SeqCst),
        0,
        "no later node starts"
    );
    let history = sessions
        .with_context(&id, |ctx| {
            ctx.get_resource::<ContextManager>()
                .unwrap()
                .messages
                .clone()
        })
        .await
        .unwrap();
    let tool_uses = history
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { content, .. } => content
                .iter()
                .filter_map(|block| match block {
                    AssistantBlock::ToolCall(call) => Some(call.id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Message::User { .. } => Vec::new(),
        })
        .collect::<Vec<_>>();
    let tool_results = history
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content
                .iter()
                .filter_map(|block| match block {
                    UserBlock::ToolResult(result) => Some(result.id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Message::Assistant { .. } => Vec::new(),
        })
        .collect::<Vec<_>>();
    assert!(
        tool_uses.iter().all(|id| tool_results.contains(id)),
        "canonical history has no orphaned tool_use: uses={tool_uses:?}, results={tool_results:?}"
    );
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
    let prompts_after_interrupt = if matches!(stop_at, StopAt::Input) {
        2
    } else {
        1
    };
    assert_eq!(
        probe.queued_prompts.load(Ordering::SeqCst),
        prompts_after_interrupt,
        "a parked input interruption must not consume its prompt"
    );

    probe.stop_at.store(StopAt::None as u8, Ordering::SeqCst);
    sessions
        .process_turn(&id)
        .await
        .expect("the next turn succeeds");
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 2);
    assert_eq!(
        probe.queued_prompts.load(Ordering::SeqCst),
        prompts_after_interrupt - 1
    );
}

#[tokio::test]
async fn cancellation_while_act_streams_preserves_the_session() {
    prove_in_node_stop(StopAt::Act).await;
}

#[tokio::test]
async fn cancellation_while_execute_tools_awaits_preserves_the_session() {
    prove_in_node_stop(StopAt::Tools).await;
}

#[tokio::test]
async fn cancellation_while_input_is_parked_does_not_swallow_the_prompt() {
    prove_in_node_stop(StopAt::Input).await;
}

#[tokio::test]
async fn cancellation_token_meets_the_managed_shell_floor() {
    let source = polaris::graph::cancel::CancellationSource::new();
    let waiter = source.token().cancelled_owned();
    source.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .unwrap();
}
