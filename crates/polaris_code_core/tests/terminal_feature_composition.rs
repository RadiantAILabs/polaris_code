//! Semantic composition proof for the terminal features working together.
//!
//! Four features meet here, and each one's own suite can only see its own
//! half:
//!
//! - permission modes resolve against hand-supplied registry defaults;
//! - the multi-language index drives the indexer with no policy over it;
//! - terminal session settings exercise `/model` and `/permission` as command
//!   handlers and never run a turn;
//! - context status reports a turn's model, context and cost, without the
//!   permission surface or the settings commands.
//!
//! `tests/code_lane_composition.rs` already proves the first two meet at the
//! `execute_tools` gate. What that cannot see — and what only exists once these
//! features are combined — is `prepare_context` in `src/agents/code_agent.rs`,
//! where a settings snapshot now rewrites the turn's permission policy and its
//! model *before* the status projection reports on that same turn. Several features
//! write to that function, so the two questions below are the ones the
//! combination has to answer:
//!
//! 1. does the mode chosen at the terminal reach the gate that governs the
//!    indexer's tools, and
//! 2. does the status/cost projection describe the model the turn really ran on,
//!    rather than the startup default it was written against?
//!
//! Unlike `tests/headless_exec.rs`, this harness installs a [`TokenMeter`] the
//! way `main.rs` does, so turns complete through `act` and the cost projection
//! is actually reached.

use std::path::Path;
use std::sync::Arc;

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolPermission, ToolRegistry, ToolsPlugin};
use polaris_code_core::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use polaris_code_core::config::ModelCatalog;
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::run;
use polaris_code_core::terminal::{CommandRegistry, SessionSettings};
use polaris_code_core::tools::FILE_WRITE_TOOLS;
use polaris_code_core::usage::TokenMeter;
use polaris_code_core::{CodeGraphTools, CodeIndexer, CodeTools, CodeToolsConfig};
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};
use polaris_tui::commands::{CommandInvocation, StatusReport, parse_status};
use tokio::sync::mpsc;

/// The model the host boots with. Every assertion about "the startup default"
/// names this one.
const STARTUP_MODEL: &str = "scripted/startup";
/// The model a `/model` command moves the session onto.
const CHOSEN_MODEL: &str = "scripted/chosen";

/// Every navigation tool the multi-language indexer populates, as
/// `tests/code_lane_composition.rs` enumerates them.
const INDEXER_TOOLS: [&str; 7] = [
    "find_definition",
    "find_references",
    "outline",
    "read_symbol",
    "callers_of",
    "callees_of",
    "impls_of_trait",
];

/// The one answer [`ScriptedProvider`] returns, however it is called.
const ANSWER: &str = "done";
/// Billed usage every turn reports, so the session meter has something to price.
const USAGE: Usage = Usage {
    input_tokens: Some(1_000),
    output_tokens: Some(100),
    total_tokens: Some(1_100),
    cache_read_tokens: None,
    cache_creation_tokens: None,
    reasoning_output_tokens: None,
};

/// Streams one fixed answer with no tool calls, so a turn finishes in one
/// iteration without a key or a network.
struct ScriptedProvider;

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
            usage: USAGE,
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
                usage: USAGE,
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers [`ScriptedProvider`] once `ModelsPlugin` has inserted the registry.
struct ScriptedModelPlugin;

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::terminal_feature_model";
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

fn executor() -> polaris::graph::GraphExecutor {
    polaris::graph::GraphExecutor::new().with_default_max_iterations(25)
}

/// A server wired like `main.rs`: the scripted provider in place of
/// `AnthropicPlugin`, and — the part that matters here — the session
/// [`TokenMeter`] registered as a global before any context is created, so
/// `act` runs and reaches the cost projection.
async fn scripted_server(
    data_dir: &Path,
) -> (
    Server,
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
    mpsc::Receiver<IOMessage>,
) {
    let (provider, input_tx, output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);

    let mut server = Server::new();
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
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    server.insert_global(TokenMeter::new());

    server.finish().await.unwrap();

    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).unwrap();

    (server, provider, input_tx, output_rx)
}

/// Build the session settings the way `main.rs` does for one starting posture.
/// Both front ends land here: the interactive one from `--permission-mode` (or
/// a later `/permission`), the headless one from its immutable unattended
/// grant. Only the posture differs, so only it is a parameter.
fn settings_in(server: &Server, mode: PermissionMode) -> SessionSettings {
    let models = server
        .get_global::<ModelRegistry>()
        .expect("ModelsPlugin registers the model registry");
    let catalog = ModelCatalog::new(STARTUP_MODEL, [STARTUP_MODEL, CHOSEN_MODEL])
        .restricted_to_providers(&models.llm_provider_names());
    SessionSettings::new(STARTUP_MODEL, mode, &catalog)
}

/// Build the interactive settings the way `main.rs` does, with `/model` and
/// `/permission` registered on a real command registry.
fn interactive_settings(server: &Server) -> (CommandRegistry, SessionSettings) {
    let settings = settings_in(server, PermissionMode::Confirm);
    let commands = CommandRegistry::with_builtins();
    settings.register(&commands).unwrap();
    (commands, settings)
}

/// Drive one turn and fold every emitted status report the way the conversation
/// panel does, returning what the meta row would end up displaying.
async fn turn_report(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    provider: Arc<ExecIOProvider>,
    input_tx: mpsc::Sender<IOMessage>,
    mut output_rx: mpsc::Receiver<IOMessage>,
) -> StatusReport {
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("say hi".to_string()),
        ))
        .await
        .unwrap();
    drop(input_tx);

    let drain = tokio::spawn(async move {
        let mut report = StatusReport::default();
        while let Some(message) = output_rx.recv().await {
            if let Some(update) = parse_status(&message) {
                report.merge(update);
            }
        }
        report
    });

    run::drive_turn(sessions, session_id).await;
    provider.close().await;
    drain.await.unwrap()
}

/// The merged composition root's tool surface, exactly as
/// `tests/code_lane_composition.rs` builds it: `CodeTools` with its mutating
/// tools narrowed to `Confirm`, plus the indexer's `CodeGraphTools`.
async fn merged_registry(dir: &Path) -> ToolRegistry {
    std::fs::write(
        dir.join("service.py"),
        "def py_helper():\n    return 1\n\ndef py_run():\n    return py_helper()\n",
    )
    .unwrap();
    let store = OxigraphStore::in_memory().unwrap();
    CodeIndexer::index_repo(&store, dir).await.unwrap();

    let config = CodeToolsConfig::new(dir);
    let mut registry = ToolRegistry::new();
    registry.register_toolset(CodeTools::new(config.clone()));
    for tool in FILE_WRITE_TOOLS {
        registry
            .set_permission(tool, ToolPermission::Confirm)
            .unwrap_or_else(|_| panic!("{tool} should be registered by CodeTools toolset"));
    }
    registry.register_toolset(CodeGraphTools::new(store, config));
    registry
}

/// Create a session seeded the way `main.rs` seeds one, optionally with
/// interactive settings. Without them `CodeAgent::setup` installs its
/// compatibility default, which deliberately does *not* apply at turn
/// boundaries.
fn create_session(
    server: &Server,
    sessions: &SessionsAPI,
    workspace: &Path,
    settings: Option<SessionSettings>,
) -> SessionId {
    let session_id = SessionId::new();
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let config = AgentConfig::new(STARTUP_MODEL, workspace.to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &agent_type,
            executor(),
            move |ctx| {
                ctx.insert(config.clone());
                if let Some(settings) = settings.clone() {
                    ctx.insert(settings);
                }
            },
        )
        .unwrap();
    session_id
}

/// The seam where status and settings meet. Status reporting first read
/// `AgentConfig::model_id`, the only model a session could have; session
/// settings then made the turn's model changeable from the terminal. Reporting the startup default here would leave the meta row — and
/// the cost priced from the same id — describing a model the turn did not use.
#[tokio::test]
async fn status_reports_the_model_the_settings_chose_for_the_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (server, provider, input_tx, output_rx) = scripted_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let (commands, settings) = interactive_settings(&server);

    // The user moves the session onto another model, through the real command.
    commands
        .dispatch(&CommandInvocation::new("model", CHOSEN_MODEL))
        .await;
    assert_eq!(settings.snapshot().model_id, CHOSEN_MODEL);

    let session_id = create_session(&server, sessions, workspace.path(), Some(settings));
    let report = turn_report(sessions, &session_id, provider, input_tx, output_rx).await;

    assert_eq!(
        report.model.as_deref(),
        Some(CHOSEN_MODEL),
        "the status projection must name the model the turn actually ran on"
    );
    assert!(
        report.context.is_some_and(|window| window.estimated),
        "the projected context window is still reported as an estimate"
    );
}

/// The control that makes the assertion above discriminating rather than
/// tautological: with no interactive settings the compatibility default keeps
/// `turn_snapshot` inert, and the very same projection reports the startup
/// model. If the code had simply hard-wired one side's model id, one of these
/// two tests would fail.
#[tokio::test]
async fn status_reports_the_startup_model_when_settings_are_inert() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (server, provider, input_tx, output_rx) = scripted_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let session_id = create_session(&server, sessions, workspace.path(), None);
    let report = turn_report(sessions, &session_id, provider, input_tx, output_rx).await;

    assert_eq!(
        report.model.as_deref(),
        Some(STARTUP_MODEL),
        "an inert settings snapshot must leave the startup model reported"
    );
}

/// The permission mode chosen at the terminal has to reach the same gate that
/// `tests/code_lane_composition.rs` proves governs the indexer's tools. That
/// proof resolves hand-built modes; this one starts from a `/permission`
/// command and reads the policy the turn actually left in the session.
#[tokio::test]
async fn the_permission_mode_chosen_at_the_terminal_governs_the_indexer_tools() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (server, provider, input_tx, output_rx) = scripted_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let (commands, settings) = interactive_settings(&server);

    commands
        .dispatch(&CommandInvocation::new("permission", "plan"))
        .await;
    assert_eq!(settings.snapshot().permission_mode, PermissionMode::Plan);

    let session_id = create_session(&server, sessions, workspace.path(), Some(settings));
    turn_report(sessions, &session_id, provider, input_tx, output_rx).await;

    // The policy the turn ran under, read back from the session context rather
    // than rebuilt from the mode.
    let permission = sessions
        .with_context(&session_id, |ctx| {
            ctx.get_resource::<PolicySnapshot>()
                .expect("prepare_context leaves a policy snapshot")
                .permission
                .clone()
        })
        .await
        .expect("the session context outlives the turn");

    let registry = merged_registry(workspace.path()).await;
    for tool in INDEXER_TOOLS {
        let default = registry.permission(tool).unwrap_or(ToolPermission::Deny);
        assert_eq!(
            permission.resolve(tool, default),
            ToolPermission::Allow,
            "plan mode selected at the terminal must still admit the indexer's {tool}"
        );
    }
    let edit_default = registry
        .permission("edit_file")
        .expect("edit_file is registered");
    assert_eq!(
        permission.resolve("edit_file", edit_default),
        ToolPermission::Deny,
        "plan mode selected at the terminal must deny the edit that feeds the index"
    );
}

/// Every posture a turn can run under. `/permission` offers the first three;
/// the fourth is what `--print` builds, and it is the one the old literal
/// misdescribed most dangerously — an unattended run never asks anyone.
fn every_permission_mode() -> [PermissionMode; 4] {
    [
        PermissionMode::Confirm,
        PermissionMode::AutoEdit,
        PermissionMode::Plan,
        PermissionMode::Unattended(UnattendedGrants {
            edit_file: true,
            run_command_patterns: vec!["ls *".to_string()],
        }),
    ]
}

/// The defect this test was written against: `status::build` set the meta row's
/// permission field to the literal `"confirm edits and commands"` on every
/// refresh, whatever the session's mode. So a `--print` run that structurally
/// never asks a human, and a `/permission plan` session that refuses every edit
/// outright, both told the user their edits and commands would be confirmed.
/// That is not a vague status line; it is a false one, about the one thing on
/// the row that describes the session's safety posture.
///
/// Each mode is driven through the production projection on its own scripted
/// server, because the posture is read where the turn runs, not where the mode
/// was chosen.
#[tokio::test]
async fn the_status_line_reports_the_permission_mode_the_turn_runs_under() {
    let mut reported: Vec<(PermissionMode, String)> = Vec::new();
    for mode in every_permission_mode() {
        let workspace = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let (server, provider, input_tx, output_rx) = scripted_server(data_dir.path()).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        let settings = settings_in(&server, mode.clone());
        let session_id = create_session(&server, sessions, workspace.path(), Some(settings));

        let report = turn_report(sessions, &session_id, provider, input_tx, output_rx).await;
        let posture = report
            .permissions
            .expect("the host states its approval posture on every status refresh");
        reported.push((mode, posture));
    }

    for (mode, posture) in &reported {
        assert!(
            posture.contains(mode.label()),
            "a posture has to name the mode in force; `{}` reported {posture:?}",
            mode.label()
        );
    }

    for (index, (mode, posture)) in reported.iter().enumerate() {
        for (other_mode, other) in &reported[index + 1..] {
            assert_ne!(
                posture,
                other,
                "`{}` and `{}` permit different things and may not read identically",
                mode.label(),
                other_mode.label()
            );
        }
    }

    let unattended = reported
        .iter()
        .find(|(mode, _)| matches!(mode, PermissionMode::Unattended(_)))
        .map(|(_, posture)| posture.as_str())
        .expect("the unattended mode was driven above");
    assert!(
        !unattended.contains("confirm"),
        "an unattended run asks no one anything, so its posture may not promise a \
         confirmation: {unattended:?}"
    );
}

/// A posture is only true while it holds. `/permission` changes the mode the
/// *next* turn runs under, so the meta row has to follow it inside one session.
/// A per-session constant chosen at startup would satisfy the test above and
/// still lie here, which is the failure mode this one exists to catch.
#[tokio::test]
async fn the_reported_posture_follows_a_mid_session_permission_change() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (server, provider, input_tx, mut output_rx) = scripted_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let (commands, settings) = interactive_settings(&server);
    let session_id = create_session(&server, sessions, workspace.path(), Some(settings));

    // Fold reports the way the panel does, but keep each *distinct* posture in
    // the order it was asserted rather than only the last one.
    let drain = tokio::spawn(async move {
        let mut postures: Vec<String> = Vec::new();
        while let Some(message) = output_rx.recv().await {
            if let Some(update) = parse_status(&message)
                && let Some(posture) = update.permissions
                && postures.last() != Some(&posture)
            {
                postures.push(posture);
            }
        }
        postures
    });

    for prompt in ["say hi", "and again"] {
        input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text(prompt.to_string()),
            ))
            .await
            .unwrap();
        run::drive_turn(sessions, &session_id).await;
        // Between the two turns, and only then, the user narrows the session.
        if prompt == "say hi" {
            commands
                .dispatch(&CommandInvocation::new("permission", "plan"))
                .await;
        }
    }
    drop(input_tx);
    provider.close().await;
    let postures = drain.await.unwrap();

    assert_eq!(
        postures.len(),
        2,
        "one turn before and one after `/permission plan` must report two \
         different postures, got {postures:?}"
    );
    assert!(
        postures[0].contains("confirm"),
        "the session starts in the fail-closed confirm posture: {:?}",
        postures[0]
    );
    assert!(
        postures[1].contains("plan"),
        "the turn after `/permission plan` runs in plan: {:?}",
        postures[1]
    );
    assert!(
        !postures[1].contains("confirm"),
        "plan refuses edits and commands outright; it does not confirm them: {:?}",
        postures[1]
    );
}
