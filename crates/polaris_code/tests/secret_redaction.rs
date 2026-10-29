//! Proof that a secret a tool reads never leaves the agent loop in clear text.
//!
//! One real coding turn runs through the binary's own
//! `receive_user_input → act → execute_tools` graph — the same plugin set
//! `src/main.rs` composes, with a scripted `LlmProvider` and a scripted
//! `IOProvider` standing in for the model and the terminal, so there is no key
//! and no network. The model asks to `read_file` a `.env` that holds
//! recognisable credentials, and the test then looks at every place that tool
//! result lands:
//!
//! 1. the **provider-bound message** — the `LlmRequest` the next model call
//!    carries, recorded by the scripted provider;
//! 2. the **persisted session** — the JSON the `FileStore` writes to disk;
//! 3. the **session `ABox`** — the reified `ToolCall` detail in the graph; and
//! 4. the **user-visible rendering** — the tool-activity message the front end
//!    draws.
//!
//! The second half is the other half of the contract: ordinary source, an
//! ordinary diff and ordinary command output must come back byte-identical,
//! because redaction that eats a tool result is worse than no redaction at all.
//!
//! Redaction is **not** a permission boundary. It reduces accidental
//! exfiltration by an agent doing ordinary work; a model that wants a secret
//! can still ask a tool to transform the bytes first. These tests prove the
//! accident case and nothing stronger.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
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
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, PolicySnapshot};
use polaris_code::run::{self, TurnReason};
use polaris_code::terminal::transcript::{IO_TYPE_TOOL, META_PHASE, PHASE_OK};
use polaris_code::tools::{CodeToolsConfig, CodeToolsPlugin};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};
use serde_json::json;

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// A syntactically valid Anthropic key shape that has never been issued.
const FAKE_ANTHROPIC_KEY: &str = "sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn4Oo5Pp6Qq7Rr8Ss9Tt0Uu1Vv2Ww3Xx4Yy5Zz-_AA";

/// AWS's own documentation example access key id.
const FAKE_AWS_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";

/// A password that is only ever recognisable by the name it is bound to.
const FAKE_PASSWORD: &str = "hunter2-correct-horse";

/// The `.env` the scripted model asks to read.
fn dotenv_fixture() -> String {
    format!(
        "# local development credentials\n\
         ANTHROPIC_API_KEY={FAKE_ANTHROPIC_KEY}\n\
         AWS_ACCESS_KEY_ID={FAKE_AWS_KEY_ID}\n\
         DATABASE_PASSWORD={FAKE_PASSWORD}\n\
         POLARIS_CODE_GRAPH_SEARCH=curated\n\
         \n\
         -----BEGIN OPENSSH PRIVATE KEY-----\n\
         b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
         QyNTUxOQAAACDqZm9vYmFyYmF6cXV1eGNvcmdlZ3JhdWx0Z2FycGx5Z2g9PQAAAJi3\n\
         -----END OPENSSH PRIVATE KEY-----\n"
    )
}

/// Ordinary repository content: source, a unified diff and command output,
/// deliberately seeded with near-miss shapes a careless rule would eat.
const ORDINARY_FIXTURE: &str = r#"// src/lexer.rs
const MAX_TOKENS: usize = 4096;
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

struct Config {
    password: Option<String>,
    api_key_env: String,
    public_key: String,
}

fn parse(&mut self) {
    let token = self.lexer.next_token();
    let sort_key = "created_at";
    let error_token = "unexpected token";
    let endpoint = "https://github.com/RadiantAILabs/polaris_code";
    let template = "${DATABASE_PASSWORD}";
}

--- a/src/lexer.rs
+++ b/src/lexer.rs
@@ -1,3 +1,3 @@
-const SECRET_HEADER: &str = "x-request-id";
+const SECRET_HEADER: &str = "x-correlation-id";

running 3 tests
test parse::token_stream_is_stable ... ok
test config::api_key_env_is_read_once ... ok
test lexer::disk-usage-reporting-path ... ok
test result: ok. 3 passed; 0 failed; 0 ignored
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Scripted model
// ─────────────────────────────────────────────────────────────────────────────

/// The tool-call id the scripted model uses.
const CALL_ID: &str = "call-read-file-1";

/// What the scripted model answers with once the tool result comes back.
const FINAL_ANSWER: &str = "read.";

/// Everything the scripted provider saw and said.
#[derive(Default)]
struct ScriptState {
    /// How many times the agent has called the model this turn.
    calls: usize,
    /// Every request the agent sent, in order — the provider-bound transcript.
    requests: Vec<LlmRequest>,
}

/// An `LlmProvider` that asks to read one file and then stops, recording every
/// request it is handed so the test can read the provider-bound messages back.
struct ScriptedProvider {
    /// Path (relative to the working dir) the model asks to read.
    path: String,
    /// Call count plus the recorded requests.
    state: Mutex<ScriptState>,
    /// Independent call counter for the streaming path.
    _streamed: AtomicUsize,
}

impl ScriptedProvider {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            state: Mutex::new(ScriptState::default()),
            _streamed: AtomicUsize::new(0),
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
                "read_file",
                json!({ "path": self.path }),
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

    /// Every tool-result body the agent sent back to the provider.
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

/// Registers [`ScriptedProvider`] through the same seam `AnthropicPlugin` uses.
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

/// Newtype so the shared `Arc` can be registered as a provider.
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

// ─────────────────────────────────────────────────────────────────────────────
// Scripted front end
// ─────────────────────────────────────────────────────────────────────────────

/// A scripted front end: hands the agent one prompt and records what it draws.
struct ScriptedIOProvider {
    state: Mutex<(Option<String>, Vec<IOMessage>)>,
}

impl ScriptedIOProvider {
    fn new(prompt: &str) -> Self {
        Self {
            state: Mutex::new((Some(prompt.to_owned()), Vec::new())),
        }
    }

    /// The text of every completed tool-activity message.
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

// ─────────────────────────────────────────────────────────────────────────────
// One real coding turn
// ─────────────────────────────────────────────────────────────────────────────

/// A turn wired the way `main.rs` wires one, minus the model and the terminal.
struct ReadingTurn {
    server: Server,
    model: Arc<ScriptedProvider>,
    io: Arc<ScriptedIOProvider>,
    working_dir: PathBuf,
    data_dir: PathBuf,
    _workspace: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl ReadingTurn {
    /// Writes `contents` to `name` in a fresh workspace and builds the server.
    async fn build(name: &str, contents: &str) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let working_dir = workspace.path().canonicalize().unwrap();
        std::fs::write(working_dir.join(name), contents).unwrap();

        let model = Arc::new(ScriptedProvider::new(name));
        let io = Arc::new(ScriptedIOProvider::new("read it"));
        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin {
                provider: model.clone(),
            })
            .add_plugins(ToolsPlugin)
            .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(&working_dir)))
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

    /// Drives one turn and saves the session, returning what happened plus the
    /// on-disk session JSON.
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
                ctx.insert(PolicySnapshot::defaults());
            })
            .await
            .unwrap();

        let outcome = run::drive_turn(sessions, &session_id).await;
        sessions.save_session(&session_id).await.unwrap();
        let persisted = std::fs::read_to_string(session_file(&self.data_dir, &session_id))
            .expect("the session was persisted");
        (outcome, persisted)
    }

    /// Whether any literal anywhere in the store — the reified trajectory
    /// included — contains `needle`.
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

// ─────────────────────────────────────────────────────────────────────────────
// The contract
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_secret_a_tool_read_reaches_no_consumer_in_clear_text() {
    let fixture = dotenv_fixture();
    let turn = ReadingTurn::build(".env", &fixture).await;
    let (outcome, persisted) = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);

    // The tool genuinely ran and genuinely returned the file.
    let rendered = turn.io.tool_results();
    assert_eq!(rendered.len(), 1, "one completed tool block: {rendered:?}");

    let provider_bound = turn.model.tool_results();
    assert!(
        !provider_bound.is_empty(),
        "the tool result must reach the provider-bound messages at all"
    );

    const SECRETS: [&str; 4] = [
        FAKE_ANTHROPIC_KEY,
        FAKE_AWS_KEY_ID,
        FAKE_PASSWORD,
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmU",
    ];

    for (what, haystacks) in [
        ("the provider-bound message", provider_bound.clone()),
        ("the user-visible rendering", rendered.clone()),
        ("the persisted session", vec![persisted.clone()]),
    ] {
        for haystack in haystacks {
            for secret in SECRETS {
                assert!(
                    !haystack.contains(secret),
                    "{what} carries {secret} in clear text"
                );
            }
        }
    }

    for secret in SECRETS {
        assert!(
            !turn.graph_carries(secret),
            "the session graph carries {secret} in clear text"
        );
    }
}

#[tokio::test]
async fn the_substitution_is_visible_rather_than_silent() {
    let fixture = dotenv_fixture();
    let turn = ReadingTurn::build(".env", &fixture).await;
    let (outcome, _) = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);

    let rendered = turn.io.tool_results();
    let shown = rendered.first().expect("one completed tool block");
    assert!(
        shown.contains("[REDACTED:"),
        "a developer must be able to see that a value was withheld, not mistake \
         the substitution for the real value: {shown}"
    );
    assert!(
        shown.contains("anthropic-api-key"),
        "the marker names what shape was recognised: {shown}"
    );
    assert!(
        shown.contains("POLARIS_CODE_GRAPH_SEARCH=curated"),
        "everything that is not a secret is still there: {shown}"
    );
}

#[tokio::test]
async fn ordinary_output_passes_through_unaltered() {
    let turn = ReadingTurn::build("lexer.rs", ORDINARY_FIXTURE).await;
    let (outcome, _) = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);

    let rendered = turn.io.tool_results();
    let shown = rendered.first().expect("one completed tool block");
    assert_eq!(
        shown, ORDINARY_FIXTURE,
        "ordinary source, an ordinary diff and ordinary command output must \
         come back byte-identical"
    );

    for provider_bound in turn.model.tool_results() {
        assert_eq!(provider_bound, ORDINARY_FIXTURE);
    }
}
