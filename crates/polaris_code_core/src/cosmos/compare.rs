//! Running one stated coding edit under two independently chosen intents, so
//! a user can see how two topologies really handled the same work.
//!
//! # What "two topologies" honestly means here
//!
//! There is no primitive in this codebase — not in `polaris_cosmos`'s selector
//! ([`deterministic::select`](polaris_cosmos::selection::deterministic::select),
//! its [`HardConstraints`](polaris_cosmos::contracts::HardConstraints)), not in
//! [`AcceptedCodingTask`](super::host::AcceptedCodingTask) — that names a
//! registration and forces it to run. The only field that closes off any part
//! of the catalog is [`CodingObjective`](super::task::CodingObjective), which
//! `/topology` already exposes as `intent=fast|careful|thorough`. Building a
//! [`SelectionOutcome::Selected`](polaris_cosmos::contracts::SelectionOutcome)
//! for a hand-picked registration would mean fabricating a
//! [`SelectionRationale`](polaris_cosmos::contracts::SelectionRationale) —
//! Cosmos requires it to carry a non-empty list of decisive score components,
//! i.e. a claim that real ranking happened — which this module refuses to do.
//! See `docs/topology-comparison.md` for the fuller account of why that is a
//! Cosmos-shaped gap this module reports rather than works around.
//!
//! So a comparison is two *real* selections, one per user-chosen intent, each
//! free to land on a different registration — or, honestly, the same one,
//! which is itself a reportable outcome, not a failure of the affordance.
//!
//! # Isolation
//!
//! Each arm gets its own disposable copy of the source tree
//! ([`copy_workspace_snapshot`]) in a fresh [`tempfile::tempdir`]. Neither arm
//! ever opens the caller's real tree for writing:
//! [`CodingTask::tools_config`](super::task::CodingTask::tools_config) derives
//! its sandbox root purely from the `working_dir` a task is built with, so
//! pointing two tasks at two disjoint temporary directories is sufficient by
//! construction — not a policy this module has to enforce separately. Both
//! copies, edited or not, are dropped once that arm's outcome is captured
//! ([`run_one_arm`]'s `workspace` binding), so nothing survives on disk after
//! [`compare_intents`] returns and the original tree is provably unchanged.
//!
//! # No contamination of future autonomous selection
//!
//! Each arm also gets a brand-new, single-use [`Server`] with its own
//! [`TopologyEvidencePlugin`], never the live session's. Outcome-learning
//! ([`policy::candidate_facts`](super::policy::candidate_facts)) only ever
//! reads the evidence store installed on the session actually driving a live
//! turn; a comparison's servers are stood up here, used once and dropped, so
//! there is no shared, persisted record for a comparison to mislabel. This is
//! the same ephemeral-evidence pattern [`super::proof::CodingProof`] uses for
//! its own offline driver.
//!
//! # Never a winner
//!
//! [`compare_intents`] cannot fail: every way an arm can go wrong — its
//! isolated workspace could not be built, Cosmos selection itself errored, or
//! selection reached a real "nothing eligible" / "nothing started" answer —
//! is a value in that arm's own [`ArmOutcome`], not a `Result` that aborts the
//! other arm. Nothing here scores, ranks, or declares between the two arms;
//! that is left to whoever renders the report.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use polaris::graph::GraphExecutor;
use polaris::models::llm::{
    AssistantBlock, GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream, StopReason,
    Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_cosmos::contracts::DurationMs;
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_cosmos::topology::TopologyCatalogPlugin;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use walkdir::WalkDir;

use crate::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use crate::usage::TokenMeter;

use super::host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, TopologySelectionError,
    TopologyTurn, select_and_run,
};
use super::policy;
use super::task::{CodingIntent, CodingObjective, CodingTask};
use super::topology::CodingTopologyPackPlugin;

/// Directory names never copied into an arm's isolated workspace: the
/// agent's own state directory and version-control history. Neither the
/// coding topologies nor their verifiers read either, and skipping them keeps
/// an isolated copy cheap regardless of the source tree's git history.
const EXCLUDED_DIR_NAMES: [&str; 2] = [".git", ".polaris"];

/// The model id every arm's isolated session is opened with.
///
/// Never resolved against a real provider: every accepted comparison task
/// declares [`HostCapabilities::hermetic`](super::topology::HostCapabilities::hermetic),
/// exactly like every task `/topology` itself accepts
/// ([`AcceptedCodingTask::new`]'s own default), so `MODEL_PROPOSED` is
/// mechanically excluded before anything is readmitted and the conversational
/// agent never runs — a task is always pre-accepted, so its selection point
/// is never inert. [`InertModelProvider`] exists only so [`ModelsPlugin`] and
/// [`CodeAgentPlugin`] see a registered model id at session-creation time.
const COMPARISON_MODEL_ID: &str = "comparison/unreachable-model";

/// One arm's real, independent outcome.
#[derive(Debug)]
pub struct ComparisonArm {
    /// The intent this arm was selected under.
    pub intent: CodingIntent,
    /// What running this arm actually produced.
    pub outcome: ArmOutcome,
}

/// What one comparison arm produced. Every variant is a result to report —
/// none of them is treated as aborting the other arm.
#[derive(Debug)]
pub enum ArmOutcome {
    /// Cosmos selection ran and reached a terminal answer: a topology
    /// executed, or a real "nothing eligible" / "nothing started" decision.
    Selected(TopologyTurn),
    /// The task was accepted but Cosmos selection itself could not complete
    /// (a rejected record, a missing capability on the isolated server).
    SelectionFailed(TopologySelectionError),
    /// This arm's isolated workspace or ephemeral run context could not be
    /// built at all, so selection was never attempted.
    SetupFailed(String),
}

/// Two arms' real, independent outcomes for one stated coding edit.
///
/// Deliberately carries no ranking, score, or declared winner — see the
/// module docs.
#[derive(Debug)]
pub struct ComparisonReport {
    /// The Cosmos task reference both arms shared.
    pub reference: String,
    /// The tree the comparison was run against. Unchanged by construction:
    /// every edit lands in a disposable per-arm copy, never here.
    pub source_tree: PathBuf,
    /// The two arms, in the order the caller supplied their intents.
    pub arms: [ComparisonArm; 2],
}

/// Runs one stated coding edit under two intents, each against its own
/// disposable copy of `source_tree`, and reports both real outcomes.
///
/// See the module docs for the isolation and non-contamination guarantees.
/// Never returns an error: a problem specific to one arm is that arm's own
/// [`ArmOutcome::SetupFailed`] or [`ArmOutcome::SelectionFailed`], and never
/// stops the other arm from running.
#[allow(clippy::too_many_arguments)]
pub async fn compare_intents(
    reference: &str,
    source_tree: &Path,
    file: &str,
    find: &str,
    replace: &str,
    stub_marker: &str,
    latency_budget: Option<DurationMs>,
    comparative: bool,
    intents: [CodingIntent; 2],
) -> ComparisonReport {
    let mut arms = Vec::with_capacity(2);
    for intent in intents {
        let outcome = run_one_arm(
            reference,
            source_tree,
            file,
            find,
            replace,
            stub_marker,
            latency_budget,
            comparative,
            intent,
        )
        .await;
        arms.push(ComparisonArm { intent, outcome });
    }
    let arms: [ComparisonArm; 2] = arms
        .try_into()
        .unwrap_or_else(|_| unreachable!("exactly one arm was pushed per requested intent"));
    ComparisonReport {
        reference: reference.to_string(),
        source_tree: source_tree.to_path_buf(),
        arms,
    }
}

/// Builds one arm's isolated workspace and ephemeral run context, accepts the
/// same edit under `intent`, and runs the real selection lifecycle over it.
#[allow(clippy::too_many_arguments)]
async fn run_one_arm(
    reference: &str,
    source_tree: &Path,
    file: &str,
    find: &str,
    replace: &str,
    stub_marker: &str,
    latency_budget: Option<DurationMs>,
    comparative: bool,
    intent: CodingIntent,
) -> ArmOutcome {
    let workspace = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => {
            return ArmOutcome::SetupFailed(format!(
                "could not create an isolated workspace: {err}"
            ));
        }
    };
    if let Err(err) = copy_workspace_snapshot(source_tree, workspace.path()) {
        return ArmOutcome::SetupFailed(format!(
            "could not copy the tree into an isolated workspace: {err}"
        ));
    }

    let mut task = match CodingTask::new(
        reference,
        workspace.path(),
        file,
        find,
        replace,
        stub_marker,
        intent,
    ) {
        Ok(task) => task,
        Err(err) => {
            return ArmOutcome::SetupFailed(format!(
                "the coding task was rejected against the isolated copy: {err}"
            ));
        }
    };
    if let Some(budget) = latency_budget {
        task = task.with_latency_budget(budget);
    }
    let objective = if comparative {
        CodingObjective::new(intent).requiring_comparative_verifier()
    } else {
        CodingObjective::new(intent)
    };

    let data_dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => {
            return ArmOutcome::SetupFailed(format!("could not create a session store: {err}"));
        }
    };
    let Some(data_dir_str) = data_dir.path().to_str() else {
        return ArmOutcome::SetupFailed(
            "the session store's temporary path is not valid UTF-8".to_string(),
        );
    };

    let queue = CodingTaskQueue::new();
    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(InertModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(data_dir_str))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));

    if let Err(err) = server.finish().await {
        return ArmOutcome::SetupFailed(format!(
            "the comparison's isolated run context failed to assemble: {err}"
        ));
    }

    let outcome = run_arm_session(&server, &queue, task, objective, workspace.path()).await;
    let _ = tokio::time::timeout(Duration::from_millis(500), server.cleanup()).await;
    // `workspace` and `data_dir` are dropped here, once `outcome` has been
    // read out of them: the isolated copy, edited or not, is discarded.
    outcome
}

/// The part of one arm that needs a live session: create it, accept the task,
/// and run the same selection lifecycle the live host loop runs.
async fn run_arm_session(
    server: &Server,
    queue: &CodingTaskQueue,
    task: CodingTask,
    objective: CodingObjective,
    workspace: &Path,
) -> ArmOutcome {
    let Some(persistence) = server.api::<PersistenceAPI>() else {
        return ArmOutcome::SetupFailed("PersistenceAPI missing from the isolated context".into());
    };
    let Some(sessions) = server.api::<SessionsAPI>().cloned() else {
        return ArmOutcome::SetupFailed("SessionsAPI missing from the isolated context".into());
    };
    sessions.set_serializers(persistence.serializers());
    if let Err(err) = sessions.register_agent(CodeAgent) {
        return ArmOutcome::SetupFailed(format!("CodeAgent could not register: {err}"));
    }

    let session_id = SessionId::new();
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let config = AgentConfig::new(COMPARISON_MODEL_ID, workspace.to_path_buf());
    if let Err(err) = sessions.create_session_with_executor(
        server.create_context(),
        &session_id,
        &agent_type,
        GraphExecutor::new().with_default_max_iterations(crate::agents::DEFAULT_MAX_TURNS),
        move |ctx| {
            ctx.insert(config.clone());
        },
    ) {
        return ArmOutcome::SetupFailed(format!(
            "the isolated session could not be created: {err}"
        ));
    }

    queue.accept(AcceptedCodingTask::new(task).with_objective(objective));

    let turn_number = sessions
        .session_info(&session_id)
        .map(|info| info.turn_number)
        .unwrap_or_default();
    match select_and_run(&sessions, &session_id, turn_number).await {
        Ok(Some(turn)) => ArmOutcome::Selected(turn),
        Ok(None) => ArmOutcome::SetupFailed(
            "the isolated session reported no accepted coding task, though one was queued".into(),
        ),
        Err(err) => ArmOutcome::SelectionFailed(err),
    }
}

/// Copies `source` into `dest`, skipping [`EXCLUDED_DIR_NAMES`], symlinks, and
/// anything else that is neither a plain file nor a directory.
///
/// Not a faithful archive — a disposable working copy for one topology run,
/// which never needs to follow a link out of the tree it was given.
fn copy_workspace_snapshot(source: &Path, dest: &Path) -> std::io::Result<()> {
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !EXCLUDED_DIR_NAMES.contains(&entry.file_name().to_string_lossy().as_ref())
        })
    {
        let entry = entry.map_err(std::io::Error::other)?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(std::io::Error::other)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(relative);
        let file_type = entry.file_type();
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// A provider that streams one fixed answer if it is ever called.
///
/// It never should be: see [`COMPARISON_MODEL_ID`]. This exists only so
/// [`ModelsPlugin`] has a registered provider to resolve the comparison
/// session's model id against.
struct InertModelProvider;

impl LlmProvider for InertModelProvider {
    fn name(&self) -> &'static str {
        "comparison"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(
                "a topology comparison never calls the model",
            )],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let response = self.generate(model, request).await?;
        Ok(
            futures_util::stream::iter([Ok(polaris::models::llm::StreamEvent::MessageStop {
                stop_reason: response.stop_reason,
                usage: response.usage,
                id: response.id,
                model: response.model,
            })])
            .boxed(),
        )
    }
}

/// Registers [`InertModelProvider`] the same way `AnthropicPlugin` registers
/// its own.
struct InertModelPlugin;

impl Plugin for InertModelPlugin {
    const ID: &'static str = "polaris_code::cosmos::compare::inert_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before InertModelPlugin")
            .register_llm_provider(InertModelProvider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_lib_rs(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).expect("src dir");
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}\n",
        )
        .expect("seed lib.rs");
    }

    #[test]
    fn copy_workspace_snapshot_excludes_git_and_polaris_state() {
        let source = tempfile::tempdir().expect("temp source");
        seed_lib_rs(source.path());
        std::fs::create_dir_all(source.path().join(".git")).expect(".git dir");
        std::fs::write(source.path().join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("seed .git/HEAD");
        std::fs::create_dir_all(source.path().join(".polaris")).expect(".polaris dir");
        std::fs::write(source.path().join(".polaris/state.json"), "{}").expect("seed state");

        let dest = tempfile::tempdir().expect("temp dest");
        copy_workspace_snapshot(source.path(), dest.path()).expect("the copy succeeds");

        assert!(dest.path().join("src/lib.rs").is_file());
        assert!(!dest.path().join(".git").exists());
        assert!(!dest.path().join(".polaris").exists());
    }

    #[tokio::test]
    async fn both_arms_run_independently_and_the_source_tree_is_never_touched() {
        let source = tempfile::tempdir().expect("temp source");
        seed_lib_rs(source.path());
        let before = std::fs::read(source.path().join("src/lib.rs")).expect("seeded file");

        let report = compare_intents(
            "implement-add",
            source.path(),
            "src/lib.rs",
            "todo!()",
            "a + b",
            "todo!()",
            None,
            false,
            [CodingIntent::Careful, CodingIntent::Fast],
        )
        .await;

        assert_eq!(report.reference, "implement-add");
        assert_eq!(report.arms.len(), 2);
        assert_eq!(report.arms[0].intent, CodingIntent::Careful);
        assert_eq!(report.arms[1].intent, CodingIntent::Fast);
        for arm in &report.arms {
            assert!(
                matches!(arm.outcome, ArmOutcome::Selected(_)),
                "{:?} did not reach a real selection answer: {:?}",
                arm.intent,
                arm.outcome
            );
        }

        let after = std::fs::read(source.path().join("src/lib.rs")).expect("still there");
        assert_eq!(before, after, "the source tree must never be edited");
    }
}
