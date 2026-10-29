//! The live selecting-host harness shared by the topology integration tests.
//!
//! One server assembled the way `main.rs` assembles it — the catalog provider,
//! the coding pack, an evidence capability, the accepted-task queue, and the
//! real [`CodeAgent`] behind a scripted model provider — plus one real coding
//! task on a real temporary crate.
//!
//! Accepting a coding task through [`CodingTaskQueue`] is a *test* harness
//! only: it stands a task up far enough to observe a real projection, and is
//! not a user-facing way to submit one.
//!
//! Each test binary that includes this module uses a different part of it, so
//! the module allows dead code rather than splitting into per-test copies.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use polaris_code_core::cosmos::host::{
    CodingTaskQueue, CodingTaskQueuePlugin, OperatorTopologyPin,
};
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::task::{CodingIntent, CodingTask, ExactEdit, SuccessCheck};
use polaris_code_core::cosmos::topology::CodingTopologyPackPlugin;
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::usage::TokenMeter;
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// The answer the scripted provider streams for a conversational turn.
pub const ANSWER: &str = "nothing to change here.";

pub const ADD_STUB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}";
pub const ADD_DONE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";

pub const LIB_RS: &str = "\
//! A tiny crate with two unimplemented helpers.

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    todo!()
}

/// Subtracts two numbers.
pub fn sub(a: i32, b: i32) -> i32 {
    todo!()
}
";

/// A provider that streams one fixed answer, so a conversational turn can
/// complete with no API key and no network.
pub struct ScriptedProvider;

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
            content: vec![AssistantBlock::text(ANSWER)],
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
                usage: Usage::default(),
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers [`ScriptedProvider`] the same way `AnthropicPlugin` registers its own.
pub struct ScriptedModelPlugin;

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

/// A temporary crate plus the coding task that implements its `add` stub.
pub fn project(intent: CodingIntent) -> (tempfile::TempDir, CodingTask) {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(dir.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
    let task = CodingTask::new(
        "implement-add",
        dir.path(),
        "src/lib.rs",
        ADD_STUB,
        ADD_DONE,
        "todo!()",
        intent,
    )
    .expect("the coding task is well formed");
    (dir, task)
}

/// Same fixture as [`project`], but with a caller-chosen success check
/// instead of the default stated-edits one — for a task that names a command
/// success check, so a live host installs a
/// [`ProjectTestRunner`](polaris_code_core::cosmos::payload::ProjectTestRunner)
/// for it.
pub fn project_with_success(
    intent: CodingIntent,
    success: SuccessCheck,
) -> (tempfile::TempDir, CodingTask) {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(dir.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
    let task = CodingTask::from_statement(
        "implement-add",
        dir.path(),
        "apply the stated edit to src/lib.rs",
        intent,
        success,
    )
    .expect("the coding task is well formed")
    .with_edits(vec![
        ExactEdit::new("src/lib.rs", ADD_STUB, ADD_DONE).expect("the edit is well formed"),
    ]);
    (dir, task)
}

/// Builds and finishes the server `main.rs` builds for selection: the four
/// topology plugins beside the ordinary agent stack, with a scripted model
/// instead of `AnthropicPlugin` and [`ExecIOPlugin`] instead of the TUI.
///
/// Returns the server plus the host's own queue handle and the prompt sender.
pub async fn selecting_server(
    data_dir: &Path,
) -> (
    Server,
    CodingTaskQueue,
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
    mpsc::Receiver<IOMessage>,
) {
    build_selecting_server(data_dir, None).await
}

/// Same as [`selecting_server`], with an [`OperatorTopologyPin`] installed
/// exactly as `main.rs` installs one from `POLARIS_CODE_TOPOLOGY_PIN` — a
/// global resource, present before any session context is created, so a test
/// can prove the pin without spawning a second process.
pub async fn selecting_server_with_pin(
    data_dir: &Path,
    registration_id: &str,
) -> (
    Server,
    CodingTaskQueue,
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
    mpsc::Receiver<IOMessage>,
) {
    build_selecting_server(data_dir, Some(registration_id)).await
}

async fn build_selecting_server(
    data_dir: &Path,
    pin: Option<&str>,
) -> (
    Server,
    CodingTaskQueue,
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
    mpsc::Receiver<IOMessage>,
) {
    let (provider, input_tx, output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    // Installed before `finish()`, like `main.rs` does: `finish()` itself
    // creates a system context (it kicks off background code indexing), and
    // `Server::insert_global` panics once one exists.
    if let Some(registration_id) = pin {
        server.insert_global(OperatorTopologyPin::new(registration_id));
    }
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
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        // Exactly the four `main.rs` installs.
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));

    server
        .finish()
        .await
        .expect("the selecting server assembles");

    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");

    (server, queue, provider, input_tx, output_rx)
}

/// Creates the one session every test drives, exactly as the front ends do.
pub fn open_session(server: &Server, workspace: &Path) -> SessionId {
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = SessionId::new();
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let config = AgentConfig::new("scripted/test-model", workspace.to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &agent_type,
            polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .expect("the session is created");
    session_id
}
