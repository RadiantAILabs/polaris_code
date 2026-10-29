//! Consumer proof for file creation: the agent can bring a file into existence
//! at a path that does not exist yet, and that capability is gated, contained
//! and journaled on exactly the footing `edit_file` already has.
//!
//! Part 1 drives one real coding turn — a scripted `LlmProvider` asks for
//! `create_file` and a scripted `IOProvider` answers the confirm dialog — through
//! the binary's own `receive_user_input → act → execute_tools` graph, with the
//! same `CodeToolsPlugin` registration and the same per-mode `PolicySnapshot`
//! `main.rs` installs. The observable effect is a file on disk, so "created" and
//! "not created" are read from the filesystem rather than inferred from a log
//! line. The prompt is the repository's own documented headless example,
//! `polaris-code -p "add a README"`, which could not succeed before this tool
//! existed: `edit_file` reads its target first, so no path that does not exist
//! was addressable by any tool the agent had.
//!
//! Part 2 reaches the tool through the finished registry the way the agent
//! reaches it, and checks the properties the tool description promises: the
//! sandbox rule is the one `edit_file` enforces, an existing path is never
//! clobbered, and a creation is journaled so `/rewind` can see it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, ToolCall, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    CONFIRMED, CONFIRMED_FALSE, CONFIRMED_TRUE, IO_TYPE, IO_TYPE_CONFIRM, IOContent, IOError,
    IOMessage, IOProvider, PersistenceAPI, PersistencePlugin, UserIO,
};
use polaris::sessions::checkpoint::RevisionId;
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolPermission, ToolRegistry, ToolsPlugin};
use polaris_code_core::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use polaris_code_core::run::{self, TurnReason};
use polaris_code_core::terminal::transcript::{
    IO_TYPE_TOOL, META_LABEL, META_PHASE, PHASE_DENIED, PHASE_OK,
};
use polaris_code_core::terminal::{EditJournal, PathOutcome, RewindPoint};
use polaris_code_core::tools::{CodeToolsConfig, CodeToolsPlugin};
use polaris_code_core::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use serde_json::json;

/// The tool under test.
const TOOL: &str = "create_file";

/// The file the documented headless example asks for.
const README: &str = "README.md";

/// What the scripted model writes into it.
const README_CONTENT: &str = "# demo\n\nCreated by the agent.\n";

/// The tool-call id the scripted model uses; the transcript keys its activity
/// block on it.
const CALL_ID: &str = "call-create-file-1";

/// The text the scripted model answers with once the tool result comes back,
/// which ends the `ReAct` loop.
const FINAL_ANSWER: &str = "added the README.";

// ─────────────────────────────────────────────────────────────────────────────
// The scripted model and front end
// ─────────────────────────────────────────────────────────────────────────────

/// An `LlmProvider` that asks for `create_file` once and then stops.
///
/// Turn shape: first call → one `create_file` tool call with `path` and
/// `content`; every later call → plain text, so `has_tool_calls` routes to
/// `finalize` and the loop terminates. No key, no network.
struct ScriptedProvider {
    /// The path the model asks to create.
    path: String,
    /// The content it asks to write.
    content: String,
    /// How many times the agent has called the model this turn.
    calls: AtomicUsize,
}

impl ScriptedProvider {
    /// The assistant message for this call, tool call first.
    fn next_response(&self) -> LlmResponse {
        let first = self.calls.fetch_add(1, Ordering::Relaxed) == 0;
        let content = if first {
            vec![AssistantBlock::tool_call(ToolCall::new(
                CALL_ID,
                TOOL,
                json!({ "path": self.path, "content": self.content }),
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
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Ok(self.next_response())
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let response = self.next_response();
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

/// Registers [`ScriptedProvider`] with [`ModelRegistry`] through the same seam
/// `AnthropicPlugin` uses.
struct ScriptedModelPlugin {
    /// The path the scripted model asks to create.
    path: String,
    /// The content it asks to write.
    content: String,
}

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
            .register_llm_provider(ScriptedProvider {
                path: self.path.clone(),
                content: self.content.clone(),
                calls: AtomicUsize::new(0),
            });
    }
}

/// What the scripted user does when the agent opens a confirm dialog.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Stamp the reply `confirmed: true`, as an approving front end does.
    Approve,
    /// Stamp the reply `confirmed: false`, as a denying front end does.
    Deny,
}

/// Mutable state behind [`ScriptedIOProvider`].
#[derive(Default)]
struct ScriptedIOState {
    /// The turn's prompt, taken by the first `receive`.
    prompt: Option<String>,
    /// Set by the outbound confirm request, cleared by the reply.
    confirm_open: bool,
    /// Every message the agent sent, in order.
    sent: Vec<IOMessage>,
}

/// A scripted front end: hands the agent one prompt, then answers each confirm
/// dialog the same way, and records everything the agent sends back.
struct ScriptedIOProvider {
    /// How the scripted user answers every confirm dialog.
    answer: Answer,
    /// Prompt, dialog state, and the outbound transcript.
    state: Mutex<ScriptedIOState>,
}

impl ScriptedIOProvider {
    /// Creates a provider that delivers `prompt` once and answers with `answer`.
    fn new(prompt: &str, answer: Answer) -> Self {
        Self {
            answer,
            state: Mutex::new(ScriptedIOState {
                prompt: Some(prompt.to_owned()),
                ..ScriptedIOState::default()
            }),
        }
    }

    /// The text of every `create_file` activity message that reached `phase`.
    fn tool_messages(&self, phase: &str) -> Vec<String> {
        let state = self.state.lock().expect("scripted IO lock poisoned");
        state
            .sent
            .iter()
            .filter(|message| {
                message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_TOOL)
                    && message.metadata.get(META_PHASE).map(String::as_str) == Some(phase)
                    && message.metadata.get(META_LABEL).map(String::as_str) == Some(TOOL)
            })
            .map(|message| match &message.content {
                IOContent::Text(text) => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// The text of every confirm dialog the agent opened.
    fn confirm_prompts(&self) -> Vec<String> {
        let state = self.state.lock().expect("scripted IO lock poisoned");
        state
            .sent
            .iter()
            .filter(|message| {
                message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_CONFIRM)
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
        let mut state = self.state.lock().expect("scripted IO lock poisoned");
        if message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_CONFIRM) {
            state.confirm_open = true;
        }
        state.sent.push(message);
        Ok(())
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        let mut state = self.state.lock().expect("scripted IO lock poisoned");
        if state.confirm_open {
            state.confirm_open = false;
            let confirmed = match self.answer {
                Answer::Approve => CONFIRMED_TRUE,
                Answer::Deny => CONFIRMED_FALSE,
            };
            return Ok(IOMessage::user_text("answer").with_metadata(CONFIRMED, confirmed));
        }
        // The turn's one prompt, then end-of-input — never a hang.
        state.prompt.take().map_or(Err(IOError::Closed), |prompt| {
            Ok(IOMessage::from_external("test", IOContent::Text(prompt)))
        })
    }
}

/// Installs the scripted front end exactly the way `ExecIOPlugin` installs its
/// own: one `register_local` factory over a shared provider.
struct ScriptedIOPlugin {
    /// The shared provider the test also reads its transcript back from.
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
// Part 1 — one real coding turn per permission mode
// ─────────────────────────────────────────────────────────────────────────────

/// One coding turn, wired as `main.rs` wires it apart from the scripted model
/// and front end: `CodeToolsPlugin` over the working directory with the
/// workspace rewind journal, and the mode's permission policy installed into
/// the session's `PolicySnapshot`.
struct CodingTurn {
    /// The finished server; owns the APIs and the frozen tool registry.
    server: Server,
    /// The scripted front end's transcript.
    io: Arc<ScriptedIOProvider>,
    /// The sandbox root every tool path resolves against.
    working_dir: PathBuf,
    /// The permission mode the turn runs under.
    mode: PermissionMode,
    /// The journal `create_file` records into.
    journal: EditJournal,
    /// Kept alive for the duration of the turn.
    _workspace: tempfile::TempDir,
    /// Kept alive for the duration of the turn.
    _data: tempfile::TempDir,
}

impl CodingTurn {
    /// Builds the turn: the scripted model asks to create `README.md`, and the
    /// scripted user answers any confirm dialog with `answer`.
    async fn build(mode: PermissionMode, answer: Answer) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let working_dir = workspace.path().canonicalize().unwrap();
        let journal = EditJournal::open(&working_dir).unwrap();

        let io = Arc::new(ScriptedIOProvider::new("add a README", answer));
        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin {
                path: README.to_string(),
                content: README_CONTENT.to_string(),
            })
            .add_plugins(ToolsPlugin)
            .add_plugins(CodeToolsPlugin::new(
                CodeToolsConfig::new(&working_dir).with_edit_journal(journal.clone()),
            ))
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                data.path().to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ScriptedIOPlugin {
                provider: io.clone(),
            });
        // `act` reads the session token meter through `Res<TokenMeter>`; the
        // binary registers it as a global before `finish()`, and so must this.
        server.insert_global(TokenMeter::new());
        server.finish().await.unwrap();

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        Self {
            server,
            io,
            working_dir,
            mode,
            journal,
            _workspace: workspace,
            _data: data,
        }
    }

    /// Drives exactly one turn through the production run surface, with the
    /// binary's coding permission policy applied the same way `main.rs`
    /// applies it: into the session context, after the agent's `setup` has
    /// seeded the defaults.
    async fn drive(&self) -> run::TurnOutcome {
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

        let mut policy = PolicySnapshot::defaults();
        policy.permission = self.mode.permission_policy();
        sessions
            .with_context(&session_id, move |ctx| {
                ctx.insert(policy);
            })
            .await
            .unwrap();

        run::drive_turn(sessions, &session_id).await
    }

    /// The frozen registry the agent resolves tools against.
    fn registry(&self) -> impl std::ops::Deref<Target = ToolRegistry> + '_ {
        self.server
            .get_global::<ToolRegistry>()
            .expect("the registry is a global once the server is finished")
    }

    /// The README's bytes, if the turn produced it.
    fn readme(&self) -> Option<String> {
        std::fs::read_to_string(self.working_dir.join(README)).ok()
    }
}

/// The documented headless example, `polaris-code -p "add a README"
/// --allow-edit`: an unattended run carrying the edit grant creates the file
/// without ever asking, and the content is on disk afterwards.
#[tokio::test]
async fn an_allow_edit_grant_creates_the_documented_readme_unattended() {
    let turn = CodingTurn::build(
        PermissionMode::Unattended(UnattendedGrants {
            edit_file: true,
            run_command_patterns: Vec::new(),
        }),
        Answer::Deny,
    )
    .await;
    assert!(turn.readme().is_none(), "the workspace starts without it");

    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(outcome.final_text.as_deref(), Some(FINAL_ANSWER));
    assert_eq!(
        turn.readme().as_deref(),
        Some(README_CONTENT),
        "the file the model asked for must be on disk with exactly its content"
    );
    assert!(
        turn.io.confirm_prompts().is_empty(),
        "an unattended run never asks: {:?}",
        turn.io.confirm_prompts()
    );
    let ok = turn.io.tool_messages(PHASE_OK);
    assert_eq!(ok.len(), 1, "one successful create_file block: {ok:?}");
    assert!(ok[0].contains(README), "the receipt names the path: {ok:?}");
}

/// Confirm mode asks first. An approval creates the file; the dialog names the
/// tool and the path so the user knows what they are approving.
#[tokio::test]
async fn confirm_mode_asks_and_an_approval_creates_the_file() {
    let turn = CodingTurn::build(PermissionMode::Confirm, Answer::Approve).await;

    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    let prompts = turn.io.confirm_prompts();
    assert_eq!(prompts.len(), 1, "exactly one dialog: {prompts:?}");
    assert!(
        prompts[0].contains(README),
        "the dialog names the path being created: {prompts:?}"
    );
    assert_eq!(turn.readme().as_deref(), Some(README_CONTENT));
    assert_eq!(turn.io.tool_messages(PHASE_OK).len(), 1);
}

/// Confirm mode asks first, and a refusal creates nothing.
#[tokio::test]
async fn confirm_mode_asks_and_a_refusal_creates_nothing() {
    let turn = CodingTurn::build(PermissionMode::Confirm, Answer::Deny).await;

    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(
        turn.io.confirm_prompts().len(),
        1,
        "the user was asked: {:?}",
        turn.io.confirm_prompts()
    );
    assert!(turn.readme().is_none(), "a denied creation leaves no file");
    let denied = turn.io.tool_messages(PHASE_DENIED);
    assert_eq!(denied.len(), 1, "the block ends denied: {denied:?}");
    assert!(
        denied[0].contains("User denied"),
        "the denial is the user's, not the policy's: {denied:?}"
    );
}

/// Plan mode refuses creation outright — no dialog, no file — exactly as it
/// refuses `edit_file`.
#[tokio::test]
async fn plan_mode_refuses_creation_without_asking() {
    let turn = CodingTurn::build(PermissionMode::Plan, Answer::Approve).await;

    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert!(
        turn.io.confirm_prompts().is_empty(),
        "plan mode never asks: {:?}",
        turn.io.confirm_prompts()
    );
    assert!(turn.readme().is_none(), "plan mode creates nothing");
    let denied = turn.io.tool_messages(PHASE_DENIED);
    assert_eq!(denied.len(), 1, "the block ends denied: {denied:?}");
    assert!(
        denied[0].contains("Permission denied"),
        "the refusal is the policy's: {denied:?}"
    );
    // The same policy that refuses the edit refuses the creation.
    let policy = PermissionMode::Plan.permission_policy();
    let registry = turn.registry();
    for tool in ["edit_file", TOOL] {
        let default = registry
            .permission(tool)
            .unwrap_or_else(|| panic!("{tool} is registered"));
        assert_eq!(policy.resolve(tool, default), ToolPermission::Deny);
    }
}

/// Auto-edit creates without asking, as it edits without asking.
#[tokio::test]
async fn auto_edit_creates_without_asking() {
    let turn = CodingTurn::build(PermissionMode::AutoEdit, Answer::Deny).await;

    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert!(
        turn.io.confirm_prompts().is_empty(),
        "auto-edit never asks for a file write: {:?}",
        turn.io.confirm_prompts()
    );
    assert_eq!(turn.readme().as_deref(), Some(README_CONTENT));
}

/// An unattended run without the edit grant creates nothing: the tool's
/// confirming default collapses to a refusal, never to a question.
#[tokio::test]
async fn no_grant_means_no_creation() {
    let turn = CodingTurn::build(
        PermissionMode::Unattended(UnattendedGrants::default()),
        Answer::Approve,
    )
    .await;

    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert!(
        turn.io.confirm_prompts().is_empty(),
        "an unattended run never asks: {:?}",
        turn.io.confirm_prompts()
    );
    assert!(turn.readme().is_none(), "no grant, no file");
    assert_eq!(turn.io.tool_messages(PHASE_DENIED).len(), 1);
}

/// The creation the turn made is in the workspace journal, so `/rewind` lists
/// its path and can take the file back to its exact pre-image.
#[tokio::test]
async fn a_created_file_is_journaled_and_visible_to_rewind() {
    let turn = CodingTurn::build(PermissionMode::AutoEdit, Answer::Deny).await;
    let before = point(&turn.journal);

    let outcome = turn.drive().await;
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(turn.readme().as_deref(), Some(README_CONTENT));

    let affected = turn.journal.affected_paths(&before);
    assert_eq!(
        affected,
        vec![turn.working_dir.join(README)],
        "the rewind preview lists the created path"
    );
    let outcomes = turn.journal.restore_edits(&before);
    assert!(
        matches!(outcomes.as_slice(), [PathOutcome::Restored(path)] if path == &turn.working_dir.join(README)),
        "a creation rewinds like any other tracked write: {outcomes:?}"
    );
    // The journal restores exact images; the pre-image of a file that did not
    // exist is empty, so the rewound file is empty rather than gone.
    assert_eq!(turn.readme().as_deref(), Some(""));
}

/// A rewind point at the journal's current cursor, the way the host adds one
/// before each prompt.
fn point(journal: &EditJournal) -> RewindPoint {
    journal
        .add_point(
            &SessionId::from_string("s"),
            RevisionId::default(),
            serde_json::from_str("1").unwrap(),
            "before prompt".into(),
        )
        .unwrap()
}

// ─────────────────────────────────────────────────────────────────────────────
// Part 2 — the tool's own contract, through the finished registry
// ─────────────────────────────────────────────────────────────────────────────

/// The composition root's file-tool surface: `ToolsPlugin` then
/// `CodeToolsPlugin`, finished so the registry is the frozen global the agent
/// reads.
struct FileTools {
    /// Owns the frozen registry.
    server: Server,
    /// The sandbox root.
    working_dir: PathBuf,
    /// The journal the tools record into.
    journal: EditJournal,
    /// Kept alive for the duration of the test.
    _workspace: tempfile::TempDir,
}

impl FileTools {
    async fn build() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let working_dir = workspace.path().canonicalize().unwrap();
        let journal = EditJournal::open(&working_dir).unwrap();
        let mut server = Server::new();
        server
            .add_plugins(ToolsPlugin)
            .add_plugins(CodeToolsPlugin::new(
                CodeToolsConfig::new(&working_dir).with_edit_journal(journal.clone()),
            ));
        server.finish().await.unwrap();
        Self {
            server,
            working_dir,
            journal,
            _workspace: workspace,
        }
    }

    fn registry(&self) -> impl std::ops::Deref<Target = ToolRegistry> + '_ {
        self.server
            .get_global::<ToolRegistry>()
            .expect("the registry is a global once the server is finished")
    }

    async fn create(&self, path: &str, content: &str) -> Result<String, String> {
        self.registry()
            .execute(TOOL, &json!({ "path": path, "content": content }))
            .await
            .map(|value| {
                value
                    .as_str()
                    .map(String::from)
                    .unwrap_or(value.to_string())
            })
            .map_err(|err| err.to_string())
    }

    async fn edit(&self, path: &str) -> Result<String, String> {
        self.registry()
            .execute(
                "edit_file",
                &json!({ "path": path, "old_string": "a", "new_string": "b", "replace_all": false }),
            )
            .await
            .map(|value| value.as_str().map(String::from).unwrap_or(value.to_string()))
            .map_err(|err| err.to_string())
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.working_dir.join(path)).ok()
    }
}

/// The diagnosis `create_file` rests on: `edit_file` reads its target before it
/// edits, so a path that does not exist is not something it can address, and
/// asking it leaves the workspace untouched.
#[tokio::test]
async fn edit_file_cannot_address_a_path_that_does_not_exist() {
    let tools = FileTools::build().await;

    let err = tools
        .edit("missing.txt")
        .await
        .expect_err("edit_file must not create the file it is asked to edit");

    assert!(
        err.contains("No such file") || err.contains("not found"),
        "the refusal is the read failing, not a creation: {err}"
    );
    assert!(tools.read("missing.txt").is_none());
}

/// The composition root registers the creating tool on the same footing as
/// the editing one: present, advertised to the model with its two parameters,
/// and confirming by default.
#[tokio::test]
async fn the_composition_root_registers_create_file_gated_like_edit_file() {
    let tools = FileTools::build().await;
    let registry = tools.registry();

    assert_eq!(
        registry.permission(TOOL),
        Some(ToolPermission::Confirm),
        "create_file must confirm by default, exactly as edit_file does"
    );
    assert_eq!(registry.permission("edit_file"), registry.permission(TOOL));

    let definitions = registry.definitions();
    let create = definitions
        .iter()
        .find(|definition| definition.name == TOOL)
        .unwrap_or_else(|| {
            panic!(
                "create_file is advertised to the model: {:?}",
                definitions
                    .iter()
                    .map(|definition| definition.name.as_str())
                    .collect::<Vec<_>>()
            )
        });
    let props = &create.parameters["properties"];
    assert!(props.get("path").is_some(), "{:?}", create.parameters);
    assert!(props.get("content").is_some(), "{:?}", create.parameters);
    assert!(
        create.description.contains("edit_file"),
        "the description tells the model what to use for an existing file: {}",
        create.description
    );
}

/// A creation lands with exactly the requested bytes, and missing parent
/// directories inside the sandbox are created on the way.
#[tokio::test]
async fn create_file_writes_the_content_and_makes_parent_directories() {
    let tools = FileTools::build().await;

    let receipt = tools
        .create("src/new_module/mod.rs", "pub fn added() {}\n")
        .await
        .expect("a new file under a new directory is created");

    assert!(receipt.contains("src/new_module/mod.rs"), "{receipt}");
    assert_eq!(
        tools.read("src/new_module/mod.rs").as_deref(),
        Some("pub fn added() {}\n")
    );
}

/// Creation is not a way around the path containment `edit_file` enforces:
/// the same resolver refuses the same escapes, and nothing lands outside.
#[tokio::test]
async fn creation_outside_the_working_directory_is_refused_by_the_edit_rule() {
    let tools = FileTools::build().await;
    // The sandbox's parent is a shared temp root, so the probe name is unique
    // to this process and the "nothing landed" check cannot be fooled by a
    // neighbour's file.
    let outside = tools.working_dir.parent().unwrap().to_path_buf();
    let name = format!("escape-{}.txt", std::process::id());
    let escape = format!("../{name}");

    let create_err = tools
        .create(&escape, "leak")
        .await
        .expect_err("a path escaping the sandbox is refused");
    let edit_err = tools.edit(&escape).await.expect_err("edit refuses it too");

    assert!(
        create_err.contains("escapes sandbox"),
        "the refusal is the sandbox rule: {create_err}"
    );
    assert_eq!(
        create_err, edit_err,
        "creation and edit are refused by the same rule with the same words"
    );

    // An absolute path outside the sandbox and a `..` hidden behind a
    // not-yet-existing directory are the same escape.
    for path in [
        outside.join(&name).display().to_string(),
        format!("newdir/../../{name}"),
    ] {
        let err = tools
            .create(&path, "leak")
            .await
            .expect_err("every escape spelling is refused");
        assert!(err.contains("escapes sandbox"), "{path}: {err}");
    }
    assert!(
        !outside.join(&name).exists(),
        "nothing was written outside the working directory"
    );
}

/// An existing path is never clobbered — a file keeps its bytes, and the error
/// points the model at `edit_file`, which is the tool for what it wanted.
#[tokio::test]
async fn creating_over_an_existing_path_is_refused() {
    let tools = FileTools::build().await;
    std::fs::write(tools.working_dir.join("keep.txt"), "original\n").unwrap();
    std::fs::create_dir(tools.working_dir.join("dir")).unwrap();

    let err = tools
        .create("keep.txt", "replacement\n")
        .await
        .expect_err("an existing file is not overwritten");
    assert!(err.contains("already exists"), "{err}");
    assert!(err.contains("edit_file"), "{err}");
    assert_eq!(tools.read("keep.txt").as_deref(), Some("original\n"));

    let err = tools
        .create("dir", "x")
        .await
        .expect_err("an existing directory is not replaced");
    assert!(err.contains("already exists"), "{err}");
    assert!(tools.working_dir.join("dir").is_dir());
}

/// A creation is journaled with an empty pre-image, so it is visible to the
/// same rewind that tracks edits, and the refused clobber records nothing.
#[tokio::test]
async fn a_creation_is_journaled_and_a_refusal_is_not() {
    let tools = FileTools::build().await;
    std::fs::write(tools.working_dir.join("keep.txt"), "original\n").unwrap();
    let start = point(&tools.journal);

    tools.create("keep.txt", "x").await.unwrap_err();
    tools.create("../escape.txt", "x").await.unwrap_err();
    assert!(
        tools.journal.affected_paths(&start).is_empty(),
        "a refused creation leaves no journal entry"
    );

    tools.create("notes/new.md", "# new\n").await.unwrap();
    assert_eq!(
        tools.journal.affected_paths(&start),
        vec![tools.working_dir.join("notes/new.md")]
    );
    assert!(tools.journal.edits_available(&start));
}

/// Empty content is a legitimate creation — a placeholder module, a marker
/// file — and lands as a zero-byte file.
#[tokio::test]
async fn create_file_accepts_empty_content() {
    let tools = FileTools::build().await;
    tools.create(".keep", "").await.unwrap();
    assert_eq!(tools.read(".keep").as_deref(), Some(""));
}

/// The refusal for an escape is the resolver's, shared with `edit_file`.
#[test]
fn the_resolver_is_the_shared_containment_rule() {
    let dir = tempfile::tempdir().unwrap();
    let config = CodeToolsConfig::new(dir.path());
    assert!(config.resolve_path("../escape.txt").is_none());
    assert!(config.resolve_path("newdir/new.txt").is_some());
}
