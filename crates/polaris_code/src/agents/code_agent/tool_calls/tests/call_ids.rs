//! Provider call identities through the real coding-agent graph.

use super::*;
use crate::agents::{CodeAgent, CodeAgentPlugin, PermissionMode, UnattendedGrants};
use crate::exec::{ExecIOPlugin, ExecIOProvider};
use polaris::models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest, LlmStream,
    StreamEvent, ToolResultStatus,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::PersistencePlugin;
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionsAPI, SessionsPlugin};
use polaris_ontrace::oxigraph::OxigraphPlugin;

#[derive(Clone)]
struct ScriptedModel {
    call: Option<ToolCall>,
    requests: Arc<Mutex<Vec<LlmRequest>>>,
}

impl LlmProvider for ScriptedModel {
    fn name(&self) -> &'static str {
        "call-id"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("the coding loop streams")
    }

    async fn stream(&self, _: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt;
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len() == 1
        };
        let mut events = Vec::new();
        let stop_reason = if let Some(call) = self.call.as_ref().filter(|_| first) {
            events.push(Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall {
                    id: call.id.clone(),
                    call_id: call.call_id.clone(),
                    name: call.function.name.clone(),
                },
            }));
            events.push(Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall {
                    arguments: call.function.arguments.to_string(),
                },
            }));
            StopReason::ToolUse
        } else {
            events.push(Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }));
            events.push(Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("done".into()),
            }));
            StopReason::EndTurn
        };
        events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
        events.push(Ok(StreamEvent::MessageStop {
            stop_reason,
            usage: Usage::default(),
            id: None,
            model: None,
        }));
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct ScriptedPlugin(ScriptedModel);
impl Plugin for ScriptedPlugin {
    const ID: &'static str = "test::provider_call_ids";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(self.0.clone());
    }
}

#[tokio::test]
async fn scripted_provider_keeps_call_ids_on_ok_error_denied_interrupted_and_repaired_results() {
    for path in [
        "ok",
        "error",
        "denied",
        "interrupted",
        "interrupted-tool",
        "repaired",
    ] {
        let call = command(
            0,
            match path {
                "error" => "fail",
                "interrupted-tool" => "slow",
                _ => "ok",
            },
        );
        assert_ne!(call.call_id.as_deref(), Some(call.id.as_str()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            call: (path != "repaired").then(|| call.clone()),
            requests: requests.clone(),
        };
        let workspace = tempfile::tempdir().unwrap();
        let (io, input, mut output) = ExecIOProvider::new(128);
        let ran = Arc::new(Mutex::new(Vec::new()));
        let mut server = Server::new();
        server.insert_global(crate::usage::TokenMeter::new());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedPlugin(model))
            .add_plugins(ToolsPlugin)
            .add_plugins(FixturePlugin(Fixtures(ran.clone())))
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ExecIOPlugin::new(Arc::new(io)));
        server.finish().await.unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions.register_agent(CodeAgent).unwrap();
        let id = SessionId::new();
        let settings = SessionSettings::new(
            "call-id/model",
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: false,
                run_command_patterns: if path == "denied" {
                    vec![]
                } else {
                    vec!["*".into()]
                },
            }),
            &crate::config::ModelCatalog::pinned("call-id/model"),
        );
        let config = AgentConfig::new("call-id/model", workspace.path());
        sessions
            .create_session_with(
                server.create_context(),
                &id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                move |ctx| {
                    ctx.insert(config);
                    ctx.insert(settings);
                },
            )
            .unwrap();
        sessions.setup_session(&id).await.unwrap();
        if path == "repaired" {
            let pending = call.clone();
            sessions
                .with_context(&id, move |ctx| {
                    let mut context = ctx.get_resource_mut::<ContextManager>().unwrap();
                    context.messages = vec![
                        Message::user("previous prompt"),
                        Message::Assistant {
                            id: None,
                            content: vec![AssistantBlock::ToolCall(pending)],
                        },
                    ];
                })
                .await
                .unwrap();
        }
        if path == "interrupted" {
            let sessions = sessions.clone();
            let id = id.clone();
            server
                .api::<polaris::graph::MiddlewareAPI>()
                .unwrap()
                .register_system("test::interrupt_call_id", move |info, ctx, next| {
                    let sessions = sessions.clone();
                    let id = id.clone();
                    Box::pin(async move {
                        let result = next.run(ctx).await;
                        if info.node_name == "authorize_call" {
                            assert!(sessions.interrupt_turn(&id).unwrap());
                        }
                        result
                    })
                });
        }
        input
            .send(IOMessage::user_text("use the fixture"))
            .await
            .unwrap();
        let outcome = if path == "interrupted-tool" {
            let turn_sessions = sessions.clone();
            let turn_id = id.clone();
            let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
            tokio::time::timeout(Duration::from_secs(5), async {
                while ran.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(sessions.interrupt_turn(&id).unwrap());
            turn.await.unwrap()
        } else {
            sessions.process_turn(&id).await
        };
        if path.starts_with("interrupted") {
            assert!(matches!(
                outcome,
                Err(polaris::sessions::SessionError::Interrupted { .. })
            ));
        } else {
            outcome.unwrap();
        }
        while output.try_recv().is_ok() {}
        let messages = if path.starts_with("interrupted") {
            sessions
                .with_context(&id, |ctx| {
                    ctx.get_resource::<ContextManager>()
                        .unwrap()
                        .messages
                        .clone()
                })
                .await
                .unwrap()
        } else {
            requests.lock().unwrap().last().unwrap().messages.clone()
        };
        let result = messages
            .iter()
            .find_map(|message| match message {
                Message::User { content } => content.iter().find_map(|block| match block {
                    UserBlock::ToolResult(result) if result.id == call.id => Some(result),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{path}: missing tool result"));
        assert_eq!(result.call_id, call.call_id, "{path}");
        assert_eq!(
            result.status,
            if path == "ok" {
                ToolResultStatus::Success
            } else {
                ToolResultStatus::Error
            },
            "{path}"
        );
        assert_eq!(
            ran.lock().unwrap().len(),
            usize::from(matches!(path, "ok" | "error" | "interrupted-tool")),
            "{path}"
        );
    }
}
