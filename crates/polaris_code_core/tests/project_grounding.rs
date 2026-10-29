//! Proof that project grounding reaches *above* the working directory and
//! stays fresh *within one turn* — asserted on the one thing that matters, the
//! `system` string the provider is handed.
//!
//! Two defects are proven here, both observed on this tree before the fix:
//!
//! 1. **Reach.** `read_project_doc` probed the working directory and nothing
//!    else, so a repository whose conventions live at its root gave an agent
//!    started in a subdirectory *no* project instructions at all.
//! 2. **Freshness.** `ProjectContext` was built once in `CodeAgent::setup` and
//!    never rebuilt, so a user who edited `AGENTS.md` mid-session to correct
//!    the agent was ignored until they restarted.
//!
//! The counterweight is asserted too, because the cheap fix for (2) breaks it:
//! the projection is cache-sensitive, so a turn that changed no instruction
//! document must hand the provider a **byte-identical** system prompt even
//! when the working tree moved underneath it. `a_turn_that_changed_no_document
//! _projects_a_byte_identical_prefix` edits an ordinary source file between
//! turns precisely so that a naive "re-discover everything" refresh — which
//! would pick up the new `git status --short` line — fails it.
//!
//! Only the model is a fake: a capturing [`LlmProvider`] records every request
//! and answers with one text block, so there is no key, no network and no
//! spend. Everything else is production composition — the real agent, the real
//! run surface, the real `ExecIOPlugin` front end.

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::run::{self, TurnReason};
use polaris_code_core::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// A model id in the registry's `provider/model` form, served by the capturing
/// provider below.
const MODEL: &str = "scripted/claude-sonnet-4-6";

// ─────────────────────────────────────────────────────────────────────────────
// The capturing model
// ─────────────────────────────────────────────────────────────────────────────

/// Answers every turn with one text block and keeps the `system` string it was
/// handed. The system prompt is the whole subject of this file: it is where
/// `ProjectContext` is rendered, and it is the prefix provider caching keys on.
#[derive(Default, Clone)]
struct CapturingProvider {
    systems: Arc<Mutex<Vec<String>>>,
}

impl CapturingProvider {
    fn capture(&self, request: &LlmRequest) {
        self.systems
            .lock()
            .unwrap()
            .push(request.system.clone().unwrap_or_default());
    }
}

impl LlmProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.capture(&request);
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("ok")],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        self.capture(&request);
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("ok".to_string()),
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

struct CapturingModelPlugin {
    provider: CapturingProvider,
}

impl Plugin for CapturingModelPlugin {
    const ID: &'static str = "test::project_grounding_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before the capturing model")
            .register_llm_provider(self.provider.clone());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// A multi-turn headless session
// ─────────────────────────────────────────────────────────────────────────────

/// A live session that can be driven for more than one turn — which is what a
/// freshness claim needs, since the change lands *between* two of them.
struct Session {
    server: Server,
    session_id: SessionId,
    input_tx: mpsc::Sender<IOMessage>,
    output_rx: tokio::sync::Mutex<mpsc::Receiver<IOMessage>>,
    systems: Arc<Mutex<Vec<String>>>,
    notices: Mutex<Vec<IOMessage>>,
    _data_dir: tempfile::TempDir,
}

impl Session {
    /// Boot a headless session bound to `working_dir`, composed the way
    /// `main.rs`'s `--print` branch composes one.
    async fn boot(working_dir: &Path) -> Self {
        let working_dir = working_dir
            .canonicalize()
            .expect("the working directory exists");
        let data_dir = tempfile::tempdir().expect("a temp session store");
        // Generously bounded: nothing drains this channel until the turn is
        // over, so it has to hold one whole turn's messages without
        // backpressuring the run.
        let (provider, input_tx, output_rx) = ExecIOProvider::new(4096);
        let model = CapturingProvider::default();
        let systems = model.systems.clone();

        let mut server = Server::new();
        server.insert_global(TokenMeter::new());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(CapturingModelPlugin { provider: model })
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                data_dir.path().to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ExecIOPlugin::new(Arc::new(provider)));
        server.finish().await.expect("the server starts");

        let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
        let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
        sessions.set_serializers(persistence.serializers());
        sessions
            .register_agent(CodeAgent)
            .expect("CodeAgent registers");

        let session_id = SessionId::new();
        let config = AgentConfig::new(MODEL, working_dir);
        sessions
            .create_session_with_executor(
                server.create_context(),
                &session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(8),
                move |ctx| {
                    ctx.insert(config.clone());
                },
            )
            .expect("the session is created");

        Self {
            server,
            session_id,
            input_tx,
            output_rx: tokio::sync::Mutex::new(output_rx),
            systems,
            notices: Mutex::new(Vec::new()),
            _data_dir: data_dir,
        }
    }

    /// Drive one whole turn and return the `system` string the model was
    /// handed for it.
    async fn turn(&self, prompt: &str) -> String {
        self.input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text(prompt.into()),
            ))
            .await
            .expect("the prompt is enqueued before the turn starts");

        let before = self.systems.lock().unwrap().len();
        let sessions = self.server.api::<SessionsAPI>().expect("SessionsAPI");
        let outcome = run::drive_turn(sessions, &self.session_id).await;
        assert!(
            matches!(outcome.reason, TurnReason::Completed),
            "the turn completes: {:?}",
            outcome.reason
        );

        // Everything the front end was sent during the turn is waiting in the
        // channel; take it now so a test can assert on what the user was told.
        {
            let mut rx = self.output_rx.lock().await;
            let mut notices = self.notices.lock().unwrap();
            while let Ok(message) = rx.try_recv() {
                notices.push(message);
            }
        }

        let systems = self.systems.lock().unwrap();
        assert!(
            systems.len() > before,
            "the turn called the model exactly once more"
        );
        systems.last().cloned().expect("a captured system prompt")
    }

    /// Every line of text the session sent to the user's front end.
    fn told_the_user(&self) -> String {
        self.notices
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match &message.content {
                IOContent::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// Run a git command in `dir`, failing loudly rather than silently producing a
/// non-repository (which would make every reach assertion vacuous).
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed in {dir:?}");
}

/// `git status --short` as seen from `dir`, the same probe the environment
/// block renders.
fn git_status(dir: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(dir)
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A repository whose conventions live at its root and whose agent is started
/// in a package subdirectory — the monorepo shape the reach defect breaks.
///
/// The source file is committed on purpose: it leaves `git status --short`
/// clean, so a later edit to it *moves* the environment block. That is what
/// gives the stability test its teeth.
///
/// Returns the temp root (kept alive to hold the tree open), the repository
/// root and the package directory to bind the agent to.
fn monorepo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("a temp root");
    let repo = temp.path().join("monorepo");
    let package = repo.join("crates/inner");
    std::fs::create_dir_all(&package).expect("the package directory is created");
    std::fs::write(package.join("lib.rs"), "pub fn inner() {}").expect("a source file");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Tester"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed the package"]);
    (temp, repo, package)
}

// ─────────────────────────────────────────────────────────────────────────────
// Part one — reach
// ─────────────────────────────────────────────────────────────────────────────

/// THE REACH PROOF. Conventions at the repository root reach an agent started
/// in a subdirectory.
///
/// Before the fix this fails outright: `read_project_doc` joined the doc names
/// onto the working directory and returned `None` when neither was there, so
/// the rendered prompt carried no `<project-instructions>` block at all.
#[tokio::test]
async fn instructions_at_the_repository_root_reach_an_agent_in_a_subdirectory() {
    let (_temp, repo, package) = monorepo();
    std::fs::write(
        repo.join("AGENTS.md"),
        "# Monorepo conventions\nEvery crate is edition 2024.",
    )
    .expect("the root doc is written");

    let session = Session::boot(&package).await;
    let system = session.turn("hello").await;

    assert!(
        system.contains("Every crate is edition 2024."),
        "the repository root's conventions are in the prompt: {system}"
    );
    assert!(
        system.contains("<project-instructions>"),
        "and they are in the instruction block: {system}"
    );
}

/// The nearest document wins, and the render says so rather than leaving the
/// model to infer it from order alone.
#[tokio::test]
async fn the_nearest_document_is_rendered_first_and_declared_to_win() {
    let (_temp, repo, package) = monorepo();
    std::fs::write(
        repo.join("AGENTS.md"),
        "# Monorepo conventions\nIndent with four spaces.",
    )
    .expect("the root doc is written");
    std::fs::write(
        package.join("AGENTS.md"),
        "# Inner crate\nIndent with two spaces.",
    )
    .expect("the package doc is written");

    let session = Session::boot(&package).await;
    let system = session.turn("hello").await;

    let nearer = system
        .find("Indent with two spaces.")
        .expect("the package doc is present");
    let farther = system
        .find("Indent with four spaces.")
        .expect("the repository-root doc is present");
    assert!(
        nearer < farther,
        "the nearer document is rendered first: {system}"
    );
    assert!(
        system.contains("nearest first"),
        "and the block states the precedence rule the order encodes: {system}"
    );
    assert!(
        system.contains(&package.join("AGENTS.md").display().to_string()),
        "each document names the file it came from: {system}"
    );
}

/// The walk stops at the repository root. A document above it belongs to some
/// other project and is never read.
#[tokio::test]
async fn the_walk_stops_at_the_repository_root() {
    let (temp, repo, package) = monorepo();
    std::fs::write(
        temp.path().join("AGENTS.md"),
        "# Not this project\nDelete everything.",
    )
    .expect("the outside doc is written");
    std::fs::write(
        repo.join("AGENTS.md"),
        "# Monorepo conventions\nBe careful.",
    )
    .expect("the root doc is written");

    let session = Session::boot(&package).await;
    let system = session.turn("hello").await;

    assert!(
        system.contains("Be careful."),
        "the repository's own conventions are read: {system}"
    );
    assert!(
        !system.contains("Delete everything."),
        "but nothing above the repository root is: {system}"
    );
}

/// A document from outside the working directory is never honoured silently:
/// the user is told which files are in force.
#[tokio::test]
async fn documents_from_outside_the_working_directory_are_announced() {
    let (_temp, repo, package) = monorepo();
    std::fs::write(
        repo.join("AGENTS.md"),
        "# Monorepo conventions\nBe careful.",
    )
    .expect("the root doc is written");

    let session = Session::boot(&package).await;
    session.turn("hello").await;

    let told = session.told_the_user();
    assert!(
        told.contains(&repo.join("AGENTS.md").display().to_string()),
        "the user is told which document above the working directory is in force: {told}"
    );
}

/// THE CEILING PROOF. When the chain's documents exceed the budget, the one
/// that no longer fits is dropped **whole** — never cut off mid-convention —
/// and both the model and the user are told which one it was.
#[tokio::test]
async fn a_document_over_the_budget_is_dropped_whole_and_the_user_is_told() {
    let (_temp, repo, package) = monorepo();
    // The nearest document all but exhausts the 8,000-character budget, so the
    // repository root's cannot fit whole.
    std::fs::write(package.join("AGENTS.md"), "x".repeat(7_900)).expect("the near doc is written");
    std::fs::write(
        repo.join("AGENTS.md"),
        format!("ROOT RULE: never force-push.\n{}", "y".repeat(400)),
    )
    .expect("the root doc is written");

    let session = Session::boot(&package).await;
    let system = session.turn("hello").await;
    let dropped = repo.join("AGENTS.md").display().to_string();

    assert!(
        !system.contains("ROOT RULE"),
        "a document that does not fit whole is not half-rendered: {system}"
    );
    assert!(
        system.contains(&format!("<omitted path=\"{dropped}\"")),
        "the block names what was dropped, so the model knows it is reasoning \
         from an incomplete set: {system}"
    );

    let told = session.told_the_user();
    assert!(
        told.contains("omitted") && told.contains(&dropped),
        "and so does the user: {told}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Part two — freshness
// ─────────────────────────────────────────────────────────────────────────────

/// THE FRESHNESS PROOF. A document edited between turn N and turn N+1 changes
/// what turn N+1 projects.
///
/// Before the fix this fails: `ProjectContext` was built once in `setup` and
/// `prepare_context` re-rendered that same frozen struct every turn, so turn 2
/// carried the superseded rule verbatim.
#[tokio::test]
async fn a_document_edited_between_turns_reaches_the_model_on_the_next_one() {
    let (_temp, _repo, package) = monorepo();
    let doc = package.join("AGENTS.md");
    std::fs::write(&doc, "# Rules\nAlways run the full test suite.").expect("the doc is written");

    let session = Session::boot(&package).await;
    let first = session.turn("hello").await;
    assert!(
        first.contains("Always run the full test suite."),
        "turn one carries the original rule: {first}"
    );

    std::fs::write(&doc, "# Rules\nNever run the full test suite.").expect("the doc is corrected");

    let second = session.turn("and again").await;
    assert!(
        second.contains("Never run the full test suite."),
        "turn two carries the correction: {second}"
    );
    assert!(
        !second.contains("Always run the full test suite."),
        "and no longer carries the rule it replaced: {second}"
    );

    let told = session.told_the_user();
    assert!(
        told.contains("Project instructions reloaded"),
        "and the user is told the correction landed, which is the other half \
         of the complaint: {told}"
    );
    assert!(
        told.contains(&doc.display().to_string()),
        "naming the document it came from: {told}"
    );
}

/// THE STABILITY COUNTERWEIGHT. A turn that changed no instruction document
/// hands the provider a byte-identical prefix — even though the working tree
/// moved underneath it.
///
/// The ordinary source edit between the turns is the point: a refresh that
/// re-ran the git probes as well as the document read would pick up the new
/// `git status --short` line, move the cached prefix and silently cost a cache
/// miss on every turn of a normal coding session. The refresh is scoped to the
/// documents precisely so this holds.
#[tokio::test]
async fn a_turn_that_changed_no_document_projects_a_byte_identical_prefix() {
    let (_temp, _repo, package) = monorepo();
    std::fs::write(package.join("AGENTS.md"), "# Rules\nBe precise.").expect("the doc is written");

    let session = Session::boot(&package).await;
    let first = session.turn("hello").await;

    // Ordinary work: a source file changes, so `git status --short` does too.
    let before = git_status(&package);
    std::fs::write(package.join("lib.rs"), "pub fn inner() { /* edited */ }")
        .expect("an ordinary source edit");
    let after = git_status(&package);
    assert_ne!(
        before, after,
        "the fixture must actually move the git state, or this test proves nothing"
    );

    let second = session.turn("and again").await;
    assert_eq!(
        first, second,
        "no instruction document changed, so the cached prefix must not move"
    );
}

/// A removed document is a change like any other: the instructions it carried
/// stop being projected within one turn.
#[tokio::test]
async fn a_removed_document_stops_being_projected() {
    let (_temp, repo, package) = monorepo();
    std::fs::write(
        repo.join("AGENTS.md"),
        "# Monorepo conventions\nBe careful.",
    )
    .expect("the root doc is written");

    let session = Session::boot(&package).await;
    let first = session.turn("hello").await;
    assert!(first.contains("Be careful."), "turn one has it: {first}");

    std::fs::remove_file(repo.join("AGENTS.md")).expect("the doc is removed");

    let second = session.turn("and again").await;
    assert!(
        !second.contains("Be careful."),
        "turn two no longer projects a document that is gone: {second}"
    );
}
