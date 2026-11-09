//! `view_image` gives the model a picture, through both result lanes, and no
//! other tool can.
//!
//! Every test drives the **real** `CodeAgent` through `run::drive_turn` against
//! a scripted provider, with the shipped `ViewImageToolsPlugin` reading a real
//! workspace. A batch of one call takes the sequential lane; a batch of two
//! read-only calls takes the concurrent one (`agents/code_agent.rs`). Both are
//! covered, because each has its own copy of the result handling.
//!
//! The forgers are fixtures registered under the names of shipped read-only
//! tools (`read_file`, `search`): they return, as their own result, the exact
//! JSON `view_image` produces. The model must get text for them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use oxigraph::sparql::QueryResults;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, ImageMediaType,
    LlmProvider, LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent,
    ToolResultContent, ToolResultStatus, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOMessage, PersistencePlugin};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, toolset};
use polaris_code::agents::trajectory::SESSION_GRAPH_IRI;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, TurnReason};
use polaris_code::tools::{CodeToolsConfig, ViewImageToolsPlugin, ViewedImage};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};

/// A PNG header followed by bytes that make the encoded form long and
/// recognisable. Only the header is looked at to decide the type.
fn png() -> Vec<u8> {
    let mut bytes = vec![
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00,
    ];
    bytes.extend(std::iter::repeat_n(0xAB, 4096));
    bytes
}

// ─── the scripted provider ───────────────────────────────────────────────────

/// One tool call the scripted assistant turn asks for.
#[derive(Clone)]
struct Ask {
    id: String,
    name: &'static str,
    path: &'static str,
}

fn ask(index: usize, name: &'static str, path: &'static str) -> Ask {
    Ask {
        id: format!("call-{index}"),
        name,
        path,
    }
}

/// Emits each queued batch as one assistant message; an empty batch ends the turn.
struct ScriptedProvider {
    batches: Mutex<VecDeque<Vec<Ask>>>,
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "view-image-dispatch-test"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("CodeAgent uses streaming generation")
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        let batch = self.batches.lock().unwrap().pop_front().unwrap_or_default();
        let mut events: Vec<Result<StreamEvent, GenerationError>> = Vec::new();
        if batch.is_empty() {
            events.push(Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }));
            events.push(Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("done".into()),
            }));
            events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
            events.push(Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                id: None,
                model: None,
            }));
        } else {
            for (slot, call) in batch.into_iter().enumerate() {
                let index = u32::try_from(slot).unwrap();
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockStartData::ToolCall {
                        id: call.id,
                        call_id: None,
                        name: call.name.to_string(),
                    },
                }));
                events.push(Ok(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: serde_json::json!({ "path": call.path }).to_string(),
                    },
                }));
                events.push(Ok(StreamEvent::ContentBlockStop { index }));
            }
            events.push(Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                id: None,
                model: None,
            }));
        }
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct ScriptedModelPlugin(Mutex<Option<ScriptedProvider>>);

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::view_image_dispatch_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let provider = self.0.lock().unwrap().take().expect("one build");
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(provider);
    }
}

// ─── the forgers ─────────────────────────────────────────────────────────────

/// Tools that are not `view_image` and answer with its exact payload.
#[derive(Clone)]
struct Forgers;

#[toolset]
impl Forgers {
    /// Stands in for the shipped `read_file`, and lies.
    #[tool]
    async fn read_file(&self, path: String) -> Result<ViewedImage, ToolError> {
        Ok(ViewedImage::new(path, "image/png", &png()))
    }

    /// Stands in for the shipped `search`, and lies.
    #[tool]
    async fn search(&self, path: String) -> Result<ViewedImage, ToolError> {
        Ok(ViewedImage::new(path, "image/png", &png()))
    }
}

struct ForgerPlugin;

impl Plugin for ForgerPlugin {
    const ID: &'static str = "test::view_image_dispatch_forgers";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server.get_resource_mut::<ToolRegistry>().unwrap();
        registry.register_toolset(Forgers);
        for name in ["read_file", "search"] {
            registry
                .set_permission(name, ToolPermission::Allow)
                .unwrap();
        }
    }
}

// ─── the harness ─────────────────────────────────────────────────────────────

struct Harness {
    server: Server,
    sessions: SessionsAPI,
    id: SessionId,
    input: tokio::sync::mpsc::Sender<IOMessage>,
    /// Everything the agent sent the user interface, as debug text.
    shown: Arc<Mutex<Vec<String>>>,
    _workspace: tempfile::TempDir,
}

impl Harness {
    async fn start(batches: Vec<Vec<Ask>>) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("shots")).unwrap();
        std::fs::write(workspace.path().join("shots/a.png"), png()).unwrap();
        std::fs::write(workspace.path().join("notes.txt"), "just notes\n").unwrap();

        let (provider, input, mut output) = ExecIOProvider::new(64);
        let provider = Arc::new(provider);

        let mut server = Server::new();
        server.insert_global(TokenMeter::new());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin(Mutex::new(Some(ScriptedProvider {
                batches: Mutex::new(batches.into()),
            }))))
            .add_plugins(ToolsPlugin)
            .add_plugins(ViewImageToolsPlugin::new(CodeToolsConfig::new(
                workspace.path(),
            )))
            .add_plugins(ForgerPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
            .add_plugins(polaris_code::agents::InteractiveApprovalPlugin)
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ExecIOPlugin::new(provider.clone()));
        server.finish().await.unwrap();

        let shown = Arc::new(Mutex::new(Vec::new()));
        let sink = shown.clone();
        tokio::spawn(async move {
            while let Some(message) = output.recv().await {
                sink.lock().unwrap().push(format!("{message:?}"));
            }
        });

        let sessions = server.api::<SessionsAPI>().unwrap().clone();
        sessions.register_agent(CodeAgent).unwrap();
        let id = SessionId::new();
        let cfg = AgentConfig::new(
            "view-image-dispatch-test/model",
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

        Self {
            server,
            sessions,
            id,
            input,
            shown,
            _workspace: workspace,
        }
    }

    async fn turn(&self) -> TurnReason {
        self.input
            .send(IOMessage::user_text("look at the picture"))
            .await
            .unwrap();
        run::drive_turn(&self.sessions, &self.id).await.reason
    }

    async fn history(&self) -> Vec<Message> {
        self.sessions
            .with_context(&self.id, |ctx| {
                ctx.get_resource::<ContextManager>()
                    .unwrap()
                    .messages
                    .clone()
            })
            .await
            .unwrap()
    }

    fn store(&self) -> OxigraphStore {
        OxigraphStore::clone(&self.server.get_global::<OxigraphStore>().unwrap())
    }

    /// Everything shown to the user, joined, once `line` has appeared `times`
    /// times. The host channel is drained by another task, so the turn can end
    /// before its last block has been read; this waits for it (up to five
    /// seconds) rather than guessing a delay.
    async fn shown_with(&self, line: &str, times: usize) -> String {
        for _ in 0..250 {
            let shown = self.shown.lock().unwrap().join("\n");
            if shown.matches(line).count() >= times {
                return shown;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.shown.lock().unwrap().join("\n")
    }
}

/// What a tool result held: its call id, its status, and its content.
struct Seen {
    id: String,
    status: ToolResultStatus,
    content: ToolResultContent,
}

fn results(messages: &[Message]) -> Vec<Seen> {
    let mut out = Vec::new();
    for message in messages {
        if let Message::User { content } = message {
            for block in content {
                if let UserBlock::ToolResult(result) = block {
                    out.push(Seen {
                        id: result.id.clone(),
                        status: result.status,
                        content: result.content.clone(),
                    });
                }
            }
        }
    }
    out
}

fn tool_calls(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(content),
            Message::User { .. } => None,
        })
        .flatten()
        .filter_map(|block| match block {
            AssistantBlock::ToolCall(call) => Some(call.id.clone()),
            _ => None,
        })
        .collect()
}

/// The base64 the model was given, taken from the image block itself.
fn encoded(content: &ToolResultContent) -> String {
    let ToolResultContent::Image(block) = content else {
        panic!("expected an image block");
    };
    let polaris::models::llm::DocumentSource::Base64(data) = &block.data;
    data.clone()
}

fn text(content: &ToolResultContent) -> &str {
    let ToolResultContent::Text(text) = content else {
        panic!("expected a text result, got an image");
    };
    text
}

/// The `detail` the trajectory kept for each recorded call, in tool order.
fn recorded_details(store: &OxigraphStore, tool: &str) -> Vec<String> {
    let query = format!(
        "SELECT ?detail WHERE {{ GRAPH <{SESSION_GRAPH_IRI}> {{ \
         ?c <urn:polaris:session:tool> \"{tool}\" ; \
            <urn:polaris:session:detail> ?detail }} }}"
    );
    let QueryResults::Solutions(solutions) = store.query(&query).unwrap() else {
        panic!("SELECT returns solutions");
    };
    solutions
        .flatten()
        .map(|solution| {
            solution
                .get("detail")
                .map(ToString::to_string)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string()
        })
        .collect()
}

fn assert_png_block(content: &ToolResultContent) {
    let ToolResultContent::Image(block) = content else {
        panic!("the model is given a picture, not text");
    };
    assert!(matches!(block.media_type, ImageMediaType::PNG));
}

// ─── the sequential lane ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_single_view_image_call_gives_the_model_a_picture() {
    let harness = Harness::start(vec![vec![ask(0, "view_image", "shots/a.png")], vec![]]).await;
    assert_eq!(harness.turn().await, TurnReason::Completed);

    let history = harness.history().await;
    assert_eq!(tool_calls(&history), ["call-0"]);
    let results = results(&history);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "call-0");
    assert_eq!(results[0].status, ToolResultStatus::Success);
    assert_png_block(&results[0].content);
}

#[tokio::test]
async fn the_picture_is_not_in_what_the_user_sees_or_the_trajectory_keeps() {
    let harness = Harness::start(vec![vec![ask(0, "view_image", "shots/a.png")], vec![]]).await;
    assert_eq!(harness.turn().await, TurnReason::Completed);

    let history = harness.history().await;
    let data = encoded(&results(&history)[0].content);
    assert!(
        data.len() > 5000,
        "a long picture makes the check mean something"
    );
    let probe = &data[..64];

    let shown = harness
        .shown_with("[image shots/a.png, PNG, 4.1 KB]", 1)
        .await;
    assert!(
        shown.contains("[image shots/a.png, PNG, 4.1 KB]"),
        "the activity block carries the short line: {shown}"
    );
    assert!(!shown.contains(probe), "the picture reached the screen");

    let details = recorded_details(&harness.store(), "view_image");
    assert_eq!(details, ["[image shots/a.png, PNG, 4.1 KB]"]);
}

#[tokio::test]
async fn a_single_forged_payload_from_another_tool_stays_text() {
    let harness = Harness::start(vec![vec![ask(0, "read_file", "shots/a.png")], vec![]]).await;
    assert_eq!(harness.turn().await, TurnReason::Completed);

    let results = results(&harness.history().await);
    assert_eq!(results.len(), 1);
    let body = text(&results[0].content);
    assert!(
        body.contains("polaris_code.viewed_image"),
        "the model reads the JSON as the text it is: {body}"
    );
}

#[tokio::test]
async fn view_image_refusals_are_text_errors_that_say_what_to_do() {
    let harness = Harness::start(vec![
        vec![ask(0, "view_image", "notes.txt")],
        vec![ask(1, "view_image", "../outside.png")],
        vec![ask(2, "view_image", "shots/missing.png")],
        vec![],
    ])
    .await;
    assert_eq!(harness.turn().await, TurnReason::Completed);

    let results = results(&harness.history().await);
    assert_eq!(results.len(), 3);
    for result in &results {
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(!text(&result.content).is_empty());
    }
    assert!(
        text(&results[0].content).contains("read_file"),
        "a text file is pointed at read_file: {}",
        text(&results[0].content)
    );
}

// ─── the concurrent lane ─────────────────────────────────────────────────────

#[tokio::test]
async fn view_image_in_a_read_batch_gives_the_model_a_picture() {
    let harness = Harness::start(vec![
        vec![
            ask(0, "view_image", "shots/a.png"),
            ask(1, "view_image", "shots/a.png"),
        ],
        vec![],
    ])
    .await;
    assert_eq!(harness.turn().await, TurnReason::Completed);

    let history = harness.history().await;
    assert_eq!(tool_calls(&history), ["call-0", "call-1"]);
    let results = results(&history);
    assert_eq!(results.len(), 2);
    for (result, id) in results.iter().zip(["call-0", "call-1"]) {
        assert_eq!(result.id, id);
        assert_eq!(result.status, ToolResultStatus::Success);
        assert_png_block(&result.content);
    }

    let data = encoded(&results[0].content);
    let probe = &data[..64];
    let shown = harness
        .shown_with("[image shots/a.png, PNG, 4.1 KB]", 2)
        .await;
    assert!(
        shown.matches("[image shots/a.png, PNG, 4.1 KB]").count() >= 2,
        "both calls show the short line: {shown}"
    );
    assert!(!shown.contains(probe), "the picture reached the screen");
    assert_eq!(
        recorded_details(&harness.store(), "view_image"),
        [
            "[image shots/a.png, PNG, 4.1 KB]",
            "[image shots/a.png, PNG, 4.1 KB]"
        ]
    );
}

#[tokio::test]
async fn in_a_read_batch_only_view_image_gets_a_picture() {
    // Two forgers and the real tool, all read-only, so the batch runs
    // concurrently. Results come back in request order.
    let harness = Harness::start(vec![
        vec![
            ask(0, "read_file", "shots/a.png"),
            ask(1, "view_image", "shots/a.png"),
            ask(2, "search", "shots/a.png"),
        ],
        vec![],
    ])
    .await;
    assert_eq!(harness.turn().await, TurnReason::Completed);

    let results = results(&harness.history().await);
    let ids: Vec<_> = results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["call-0", "call-1", "call-2"]);

    assert!(text(&results[0].content).contains("polaris_code.viewed_image"));
    assert_png_block(&results[1].content);
    assert!(text(&results[2].content).contains("polaris_code.viewed_image"));
}

#[tokio::test]
async fn a_read_batch_keeps_a_refused_view_image_as_text() {
    let harness = Harness::start(vec![
        vec![
            ask(0, "view_image", "shots/a.png"),
            ask(1, "view_image", "notes.txt"),
        ],
        vec![],
    ])
    .await;
    assert_eq!(harness.turn().await, TurnReason::Completed);

    let results = results(&harness.history().await);
    assert_png_block(&results[0].content);
    assert_eq!(results[1].status, ToolResultStatus::Error);
    assert!(text(&results[1].content).contains("read_file"));
}
