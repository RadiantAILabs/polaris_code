//! Redaction before shell output selection through a real coding turn.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oxigraph::sparql::QueryResults;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, ToolCall,
    ToolResultContent, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    IO_TYPE, IOContent, IOError, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin, UserIO,
};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::shell::{ShellConfig, ShellPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolPermission;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, PolicySnapshot};
use polaris_code::run::{self, TurnReason};
use polaris_code::terminal::transcript::{IO_TYPE_TOOL, META_PHASE, PHASE_OK};
use polaris_code::tools::approval::ApprovalShellPlugin;
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};
use serde_json::json;

const CALL_ID: &str = "call-shell-1";

const FINAL_ANSWER: &str = "done.";

#[derive(Default)]
struct ScriptState {
    calls: usize,
    requests: Vec<LlmRequest>,
}

struct ScriptedProvider {
    state: Mutex<ScriptState>,
}

impl ScriptedProvider {
    fn new() -> Self {
        Self {
            state: Mutex::new(ScriptState::default()),
        }
    }

    fn next_response(&self, request: LlmRequest) -> LlmResponse {
        let mut state = self.state.lock().expect("script lock poisoned");
        let first = state.calls == 0;
        state.calls += 1;
        state.requests.push(request);
        drop(state);
        let content = if first {
            vec![AssistantBlock::tool_call(ToolCall::new(
                CALL_ID,
                "run_command",
                json!({ "command": "cat output.txt; exit 3" }),
            ))]
        } else {
            vec![AssistantBlock::text(FINAL_ANSWER)]
        };
        LlmResponse {
            content,
            usage: Usage::default(),
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            id: None,
            model: None,
        }
    }

    fn tool_results(&self) -> Vec<String> {
        let state = self.state.lock().expect("script lock poisoned");
        state
            .requests
            .iter()
            .flat_map(|request| request.messages.iter())
            .filter_map(|message| match message {
                Message::User { content } => Some(content),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                UserBlock::ToolResult(result) => Some(match &result.content {
                    ToolResultContent::Text(text) => text.clone(),
                    other => format!("{other:?}"),
                }),
                _ => None,
            })
            .collect()
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Ok(self.next_response(request))
    }

    async fn stream(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let response = self.next_response(request);
        let mut events = Vec::new();
        for block in response.content {
            match block {
                AssistantBlock::Text(text) => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::Text,
                    }));
                    events.push(Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::Text(text.text),
                    }));
                    events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                }
                AssistantBlock::ToolCall(call) => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::ToolCall {
                            id: call.id,
                            call_id: None,
                            name: call.function.name,
                        },
                    }));
                    events.push(Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::ToolCall {
                            arguments: call.function.arguments.to_string(),
                        },
                    }));
                    events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                }
                AssistantBlock::Reasoning(_) => {}
            }
        }
        events.push(Ok(StreamEvent::MessageStop {
            stop_reason: response.stop_reason,
            usage: Usage::default(),
            id: None,
            model: None,
        }));
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct ScriptedModelPlugin {
    provider: Arc<ScriptedProvider>,
}

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::scripted_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let provider = self.provider.clone();
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before ScriptedModelPlugin")
            .register_llm_provider(SharedProvider(provider));
    }
}

struct SharedProvider(Arc<ScriptedProvider>);

impl LlmProvider for SharedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.0.generate(model, request).await
    }

    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        self.0.stream(model, request).await
    }
}

struct ScriptedIOProvider {
    state: Mutex<(Option<String>, Vec<IOMessage>)>,
}

impl ScriptedIOProvider {
    fn new(prompt: &str) -> Self {
        Self {
            state: Mutex::new((Some(prompt.to_owned()), Vec::new())),
        }
    }

    fn tool_results(&self) -> Vec<String> {
        let state = self.state.lock().expect("scripted IO lock poisoned");
        state
            .1
            .iter()
            .filter(|message| {
                message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_TOOL)
                    && message.metadata.get(META_PHASE).map(String::as_str) == Some(PHASE_OK)
            })
            .map(|message| match &message.content {
                IOContent::Text(text) => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }
}

impl IOProvider for ScriptedIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.state
            .lock()
            .expect("scripted IO lock poisoned")
            .1
            .push(message);
        Ok(())
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        let mut state = self.state.lock().expect("scripted IO lock poisoned");
        state.0.take().map_or(Err(IOError::Closed), |prompt| {
            Ok(IOMessage::from_external("test", IOContent::Text(prompt)))
        })
    }
}

struct ScriptedIOPlugin {
    provider: Arc<ScriptedIOProvider>,
}

impl Plugin for ScriptedIOPlugin {
    const ID: &'static str = "test::scripted_io";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let provider = self.provider.clone();
        server.register_local(move || UserIO::new(provider.clone()));
    }
}

struct ShellTurn {
    server: Server,
    model: Arc<ScriptedProvider>,
    io: Arc<ScriptedIOProvider>,
    working_dir: PathBuf,
    data_dir: PathBuf,
    _workspace: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl ShellTurn {
    async fn build(contents: &str) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let working_dir = workspace.path().canonicalize().unwrap();
        std::fs::write(working_dir.join("output.txt"), contents).unwrap();

        let model = Arc::new(ScriptedProvider::new());
        let io = Arc::new(ScriptedIOProvider::new("run the command"));
        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin {
                provider: model.clone(),
            })
            .add_plugins(ToolsPlugin)
            .add_plugins(ShellPlugin::new(
                ShellConfig::new()
                    .with_working_dir(&working_dir)
                    .with_allowed_dirs(vec![working_dir.clone()])
                    .with_allowed_commands(vec!["*".into()])
                    .with_max_output_bytes(64 * 1024)
                    .with_cache_dir(data.path().join("shell")),
            ))
            .add_plugins(ApprovalShellPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                data.path().to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ScriptedIOPlugin {
                provider: io.clone(),
            });
        server.insert_global(TokenMeter::new());
        server.finish().await.unwrap();

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        Self {
            server,
            model,
            io,
            working_dir,
            data_dir: data.path().to_path_buf(),
            _workspace: workspace,
            _data: data,
        }
    }

    async fn drive(&self) -> (run::TurnOutcome, String) {
        let sessions = self.server.api::<SessionsAPI>().unwrap();
        let session_id = SessionId::new();
        sessions
            .create_session_with_executor(
                self.server.create_context(),
                &session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
                {
                    let config = AgentConfig::new("scripted/test-model", self.working_dir.clone());
                    move |ctx| {
                        ctx.insert(config.clone());
                    }
                },
            )
            .unwrap();
        sessions
            .with_context(&session_id, move |ctx| {
                let mut policy = PolicySnapshot::defaults();
                policy
                    .permission
                    .overrides
                    .insert("run_command".into(), ToolPermission::Allow);
                ctx.insert(policy);
            })
            .await
            .unwrap();

        let outcome = run::drive_turn(sessions, &session_id).await;
        sessions.save_session(&session_id).await.unwrap();
        let persisted = std::fs::read_to_string(session_file(&self.data_dir, &session_id))
            .expect("the session was persisted");
        (outcome, persisted)
    }

    fn graph_carries(&self, needle: &str) -> bool {
        let store = self
            .server
            .get_global::<OxigraphStore>()
            .expect("the ontrace store is a global once the server is finished");
        let query = format!(
            "ASK {{ GRAPH ?g {{ ?s ?p ?o . \
             FILTER(isLiteral(?o) && CONTAINS(STR(?o), \"{needle}\")) }} }}"
        );
        matches!(
            store.query(&query).expect("the session graph is queryable"),
            QueryResults::Boolean(true)
        )
    }
}

fn session_file(data_dir: &Path, id: &SessionId) -> PathBuf {
    data_dir.join(format!("{}.json", id.as_str()))
}

fn assert_no_secret(turn: &ShellTurn, persisted: &str, secrets: &[&str]) {
    let provider = turn.model.tool_results();
    let terminal = turn.io.tool_results();
    assert_eq!(provider.len(), 1, "one provider-bound result: {provider:?}");
    assert_eq!(terminal.len(), 1, "one completed terminal result");
    for secret in secrets {
        assert!(
            !provider[0].contains(secret),
            "the provider result contains {secret}"
        );
        assert!(
            !terminal[0].contains(secret),
            "the completed terminal result contains {secret}"
        );
        assert!(
            !persisted.contains(secret),
            "the persisted session contains {secret}"
        );
        assert!(
            !turn.graph_carries(secret),
            "the session graph contains {secret}"
        );
    }
    assert!(provider[0].contains("[exit code: 3]"), "{}", provider[0]);
}

#[tokio::test]
async fn shell_redacts_an_assignment_before_its_label_is_omitted() {
    let stdout = format!("{} PASSWORD=hunter2{}", "x".repeat(6000), " ".repeat(4993));
    let turn = ShellTurn::build(&stdout).await;
    let (outcome, persisted) = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_no_secret(&turn, &persisted, &["hunter2"]);
    assert!(turn.model.tool_results()[0].contains("characters omitted"));
    assert!(
        persisted.contains("redacted"),
        "the redaction notice is retained"
    );
}

#[tokio::test]
async fn shell_redacts_overflow_output_before_selecting_its_tail() {
    let stdout = format!(
        "{}{} PASSWORD=hunter2{}\nPASSWORD=overflow-password\nFINAL ERROR\n",
        "ordinary log line\n".repeat(5000),
        "x".repeat(6000),
        " ".repeat(4993),
    );
    assert!(stdout.len() > 64 * 1024);
    let turn = ShellTurn::build(&stdout).await;
    let (outcome, persisted) = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_no_secret(&turn, &persisted, &["hunter2", "overflow-password"]);
    let result = &turn.model.tool_results()[0];
    assert!(
        result.contains("FINAL ERROR"),
        "the real end is retained: {result}"
    );
    assert!(result.contains("[stdout truncated"), "{result}");
    assert!(
        result.contains("[REDACTED:secret-assignment]"),
        "the overflow secret was recognized: {result}"
    );
    assert!(!persisted.contains("PASSWORD=hunter2"));
}

#[tokio::test]
async fn shell_redacts_a_private_key_before_its_header_is_omitted() {
    let stdout = format!(
        "{}\n-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\nFINAL ERROR\n",
        "ordinary prefix\n".repeat(400),
        "private-key-payload".repeat(400),
    );
    let turn = ShellTurn::build(&stdout).await;
    let (outcome, persisted) = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_no_secret(&turn, &persisted, &["private-key-payload"]);
    let result = &turn.model.tool_results()[0];
    assert!(result.contains("[REDACTED:private-key]"), "{result}");
    assert!(result.contains("FINAL ERROR"), "{result}");
}

#[tokio::test]
async fn shell_withholds_an_overflow_key_whose_header_precedes_the_read_window() {
    let stdout = format!(
        "{}-----BEGIN PRIVATE KEY-----\n{}-----END PRIVATE KEY-----\nFINAL ERROR\n",
        "ordinary log line\n".repeat(5000),
        "private-key-payload\n".repeat(1500),
    );
    let turn = ShellTurn::build(&stdout).await;
    let (outcome, persisted) = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_no_secret(&turn, &persisted, &["private-key-payload"]);
    let result = &turn.model.tool_results()[0];
    assert!(result.contains("FINAL ERROR"), "{result}");
    assert!(result.contains("[stdout truncated"), "{result}");
}
