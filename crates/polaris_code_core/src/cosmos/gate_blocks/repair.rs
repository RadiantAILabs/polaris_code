//! Blocks that support a model-proposed repair loop: running the project's
//! tests with the failure visible for a retry, and judging a model-proposed
//! patch rather than the task's own stated edits.
//!
//! These exist for the same reason [`super::existing`] wraps the coding
//! stages `graphs.rs` already ships: a statement-only unit of work states no
//! edit at all, so [`super::existing::JudgeStatedEdits`] (which judges the
//! *task's own* stated edits) has nothing to judge, and the ordinary
//! [`super::existing::RunProjectTests`] block discards the very failure text
//! a repair attempt needs to act on. [`JudgeTestOutcomeBlock`] and
//! [`RunProjectTestsCapturedBlock`] are that judge and that run, adapted to a
//! model-proposed patch; [`JudgePatchIntentBlock`] closes the same gap for the
//! no-test-runner shape.

use std::fmt;
use std::sync::Arc;

use polaris::graph::{Graph, NodeId};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockDescription, BlockFactory, CompositionCatalogue,
    CompositionCatalogueError, Effect, StablePortKey,
};
use polaris_cosmos::contracts::{
    Assessment, CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef, Status,
};

use super::super::assess::site_assessment;
use super::super::block_cards;
use super::super::payload::{
    AppliedEdit, CheckFailureClass, CodingWorkspace, EditSite, ProjectTestRunner, TestRunOutcome,
};
use super::behavior_version;
use super::configurable::{
    self, ModelStepArtifact, ModelStepConfig, ModelStepHandoff, ModelStepProvider, PatchIntent,
    ProposedPatchIntent, WorkspaceView,
};
use crate::usage::TokenMeter;

fn describe(
    summary: &str,
    inputs: Vec<StablePortKey>,
    output: StablePortKey,
    example: &str,
) -> BlockDescription {
    BlockDescription::new(summary, inputs, output, example)
        .expect("block description is well-formed")
}

fn exec(error: impl fmt::Display) -> SystemError {
    SystemError::ExecutionError(error.to_string())
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("block ID is a valid stable name"),
        behavior_version(),
    )
}

// ---------------------------------------------------------------------------
// Running the project's tests with the output kept, not just the verdict.
// ---------------------------------------------------------------------------

#[system]
async fn run_project_tests_captured(
    workspace: Res<CodingWorkspace>,
    runner: Res<ProjectTestRunner>,
) -> TestRunOutcome {
    runner.run(&workspace.config().working_dir)
}

/// Runs the declared [`ProjectTestRunner`] command and keeps a bounded tail
/// of its combined stdout/stderr alongside the verdict.
///
/// Wraps [`run_project_tests_captured`]: `CodingWorkspace, ProjectTestRunner
/// -> TestRunOutcome`. Read-only, and the same `resource.missing` mechanical
/// exclusion [`super::existing::RunProjectTests`] gets on a host with no
/// installed runner. Runs the command exactly once; [`JudgeTestOutcomeBlock`]
/// turns its `TestRunOutcome` into the ordinary [`Assessment<EditSite>`] a
/// `Choose` predicate or [`super::existing::ConcludeSinglePass`] reads,
/// so a repair loop never runs the project's tests twice to get both.
pub struct RunProjectTestsCapturedBlock;

impl BlockFactory for RunProjectTestsCapturedBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(run_project_tests_captured))
    }
}

/// Returns [`RunProjectTestsCapturedBlock`]'s registered key.
#[must_use]
pub fn run_project_tests_captured_key() -> CompositionBlockKey {
    block_key("code.run-project-tests-captured")
}

#[system]
async fn judge_test_outcome(
    workspace: Res<CodingWorkspace>,
    outcome: Out<TestRunOutcome>,
) -> Result<Assessment<EditSite>, SystemError> {
    site_assessment(EditSite::new(
        workspace.subject_path().to_owned(),
        0,
        outcome.status,
    ))
    .map_err(exec)
}

/// Projects a captured test run's verdict into the ordinary coding
/// assessment ledger, without re-running anything.
///
/// Wraps [`judge_test_outcome`]: `CodingWorkspace, Out<TestRunOutcome> ->
/// Assessment<EditSite>`. Read-only.
pub struct JudgeTestOutcomeBlock;

impl BlockFactory for JudgeTestOutcomeBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(judge_test_outcome))
    }
}

/// Returns [`JudgeTestOutcomeBlock`]'s registered key.
#[must_use]
pub fn judge_test_outcome_key() -> CompositionBlockKey {
    block_key("code.judge-test-outcome")
}

// ---------------------------------------------------------------------------
// Judging a model-proposed patch, rather than the task's own stated edits.
// ---------------------------------------------------------------------------

#[system]
async fn judge_patch_intent(
    workspace: Res<CodingWorkspace>,
    patch: Out<PatchIntent>,
    _applied: Out<AppliedEdit>,
) -> Result<Assessment<EditSite>, SystemError> {
    // A model that proposed no edit at all never counts as landed: an empty
    // patch is not a smaller success, it is the absence of the one thing this
    // block exists to judge.
    let landed = !patch.edits.is_empty()
        && patch.edits.iter().all(|edit| {
            workspace.read(edit.file()).is_ok_and(|actual| {
                actual.contains(edit.replace())
                    && (edit.replace().contains(edit.find()) || !actual.contains(edit.find()))
            })
        });
    site_assessment(EditSite::new(
        workspace.subject_path().to_owned(),
        0,
        if landed { Status::Pass } else { Status::Issue },
    ))
    .map_err(exec)
}

/// Judges a model-proposed patch against what actually landed on disk.
///
/// Wraps [`judge_patch_intent`]: `CodingWorkspace, Out<PatchIntent>,
/// Out<AppliedEdit> -> Assessment<EditSite>`. Read-only. Unlike
/// [`super::existing::JudgeStatedEdits`] (which judges the task's own stated
/// edits, and has nothing to judge when there are none), this judges the
/// edits the model itself proposed — including the empty case, which is
/// [`Status::Issue`], never a vacuous pass.
pub struct JudgePatchIntentBlock;

impl BlockFactory for JudgePatchIntentBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(judge_patch_intent))
    }
}

/// Returns [`JudgePatchIntentBlock`]'s registered key.
#[must_use]
pub fn judge_patch_intent_key() -> CompositionBlockKey {
    block_key("code.judge-patch-intent")
}

// ---------------------------------------------------------------------------
// Keeping one model conversation for a whole self-check repair loop, rather
// than a fresh one per round that must re-read the code from scratch.
// ---------------------------------------------------------------------------

/// Fixed, host-controlled relative path (inside the agent's own
/// [`crate::state::state_dir`], so it is git-invisible and never lands in a
/// patch) [`RunCheckScriptBlock`] writes a submitted `check` script to before
/// running it. Distinct from
/// [`super::configurable::PYTHON_SCRIPT_PATH`](super::configurable) (what a
/// `run_python` tool call writes its exploratory script to) so a check
/// written post-apply never collides with whatever the model last explored
/// with `run_python`.
pub(super) const CHECK_SCRIPT_PATH: &str = "check_script.py";

fn classify_check_failure(outcome: &configurable::PythonRunOutcome) -> CheckFailureClass {
    if outcome.timed_out || matches!(outcome.exit_code, None | Some(126 | 127)) {
        return CheckFailureClass::Infrastructure;
    }
    let stderr = outcome.stderr.as_str();
    let last_exception = stderr
        .lines()
        .rev()
        .find(|line| line.trim().contains("Error:"))
        .unwrap_or("")
        .trim();
    if last_exception.starts_with("ModuleNotFoundError:")
        || last_exception.starts_with("ImportError:")
        || ((last_exception.starts_with("SyntaxError:")
            || last_exception.starts_with("IndentationError:"))
            && stderr.contains(CHECK_SCRIPT_PATH))
        || stderr.contains("Failed building wheel")
        || stderr.contains("error: subprocess-exited-with-error")
        || stderr.contains("compilation terminated")
        || stderr.contains("error: command '") && stderr.contains("failed")
    {
        CheckFailureClass::Infrastructure
    } else {
        CheckFailureClass::Behavior
    }
}

#[system]
async fn model_propose_conversation(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<ProposedPatchIntent, SystemError> {
    let view = WorkspaceView::new(&workspace);
    let (artifact, conversation) =
        configurable::run_with_handoff_conversation(&config, &provider, &view, &meter, &handoff)
            .await?;
    let ModelStepArtifact::PatchIntent { edits, check } = artifact else {
        return Err(exec(
            "a model-propose-conversation block configured for patch-intent returned another \
             artifact kind",
        ));
    };
    Ok(ProposedPatchIntent {
        patch: PatchIntent { edits, check },
        conversation,
    })
}

/// Like [`super::configurable::ModelProposeBlock`] configured for
/// `patch-intent`, but keeps the conversation that produced the patch around
/// ([`ProposedPatchIntent`]) instead of discarding it, so a failed self-check
/// can resume the same conversation for a repair round
/// ([`ResumeModelProposeBlock`]) rather than starting a fresh one that must
/// re-read the code from scratch.
///
/// Wraps [`model_propose_conversation`]: `CodingWorkspace, Out<ModelStepConfig>,
/// ModelStepProvider, TokenMeter -> ProposedPatchIntent`. Read-only, so this
/// block may sit inside a `Fork` the same way `ModelProposeBlock` can.
pub struct ModelProposeConversationBlock;

impl BlockFactory for ModelProposeConversationBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let decoded = ModelStepConfig::decode(config)?;
        graph.add_system_node(move || {
            let decoded = decoded.clone();
            async move { decoded }
        });
        Ok(graph.add_system_node(model_propose_conversation))
    }
}

/// Returns [`ModelProposeConversationBlock`]'s registered key.
#[must_use]
pub fn model_propose_conversation_key() -> CompositionBlockKey {
    block_key("code.model-propose-conversation")
}

#[system]
async fn model_resume_patch_intent(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    prior: Out<ProposedPatchIntent>,
    outcome: Out<TestRunOutcome>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<ProposedPatchIntent, SystemError> {
    let augmented = configurable::with_prior_failure(&config, &outcome);
    let view = WorkspaceView::new(&workspace);
    let (artifact, conversation) = provider
        .propose_conversation(&augmented, &view, &meter, Some(&prior.conversation))
        .await?;
    handoff.record(
        &augmented,
        &artifact,
        conversation.clone(),
        prior.conversation.len(),
    );
    let ModelStepArtifact::PatchIntent { edits, check } = artifact else {
        return Err(exec(
            "a model-resume-propose block configured for patch-intent returned another artifact \
             kind",
        ));
    };
    Ok(ProposedPatchIntent {
        patch: PatchIntent { edits, check },
        conversation,
    })
}

/// Resumes an earlier [`ModelProposeConversationBlock`] (or an earlier
/// [`ResumeModelProposeBlock`]) round's own conversation for a repair
/// attempt, with the preceding [`TestRunOutcome`]'s captured failure folded
/// in via [`super::configurable::with_prior_failure`] — the same feedback
/// [`super::configurable::ModelProposeBlock`]'s
/// `sees_prior_failure`-configured repair steps already fold in, just
/// carried on the same conversation instead of a fresh one.
///
/// Wraps [`model_resume_patch_intent`]: `CodingWorkspace, Out<ModelStepConfig>,
/// Out<ProposedPatchIntent>, Out<TestRunOutcome>, ModelStepProvider, TokenMeter
/// -> ProposedPatchIntent`. Read-only. The materializer refuses a plan naming
/// this block with no upstream `ProposedPatchIntent`/`TestRunOutcome` in
/// scope, before execution rather than at run time.
pub struct ResumeModelProposeBlock;

impl BlockFactory for ResumeModelProposeBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let decoded = ModelStepConfig::decode(config)?;
        graph.add_system_node(move || {
            let decoded = decoded.clone();
            async move { decoded }
        });
        Ok(graph.add_system_node(model_resume_patch_intent))
    }
}

/// Returns [`ResumeModelProposeBlock`]'s registered key.
#[must_use]
pub fn resume_model_propose_key() -> CompositionBlockKey {
    block_key("code.resume-model-propose")
}

#[system]
async fn patch_from_proposal(proposal: Out<ProposedPatchIntent>) -> PatchIntent {
    proposal.patch.clone()
}

/// Projects a [`ProposedPatchIntent`] down to the plain [`PatchIntent`]
/// [`super::configurable::ApplyPatchIntentBlock`] and [`JudgePatchIntentBlock`]
/// already read, so those blocks stay unchanged downstream of a
/// conversation-keeping propose step.
///
/// Wraps [`patch_from_proposal`]: `Out<ProposedPatchIntent> -> PatchIntent`.
/// Read-only.
pub struct ExtractPatchFromProposalBlock;

impl BlockFactory for ExtractPatchFromProposalBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(patch_from_proposal))
    }
}

/// Returns [`ExtractPatchFromProposalBlock`]'s registered key.
#[must_use]
pub fn extract_patch_from_proposal_key() -> CompositionBlockKey {
    block_key("code.extract-patch-from-proposal")
}

#[system]
async fn run_check_script(
    workspace: Res<CodingWorkspace>,
    shell: Res<polaris::shell::ShellExecutor>,
    proposal: Out<ProposedPatchIntent>,
    _applied: Out<AppliedEdit>,
) -> Result<TestRunOutcome, SystemError> {
    let Some(check) = proposal.patch.check.as_deref() else {
        return Err(exec(
            "a run-check-script block ran with no check script in the proposal; the has-check \
             gate should have routed elsewhere",
        ));
    };
    if check.trim().is_empty() {
        return Ok(TestRunOutcome {
            status: Status::Issue,
            output: "no check submitted".to_owned(),
            check_failure: None,
        });
    }
    match configurable::run_python_script(
        Some(&shell),
        workspace.config().working_dir.as_path(),
        CHECK_SCRIPT_PATH,
        check,
    )
    .await
    {
        Ok(outcome) => {
            let check_failure = (!outcome.success).then(|| classify_check_failure(&outcome));
            Ok(TestRunOutcome {
                status: if outcome.success {
                    Status::Pass
                } else {
                    Status::Issue
                },
                output: match check_failure {
                    Some(class) => format!("check failure: {class:?}\n{}", outcome.rendered),
                    None => outcome.rendered,
                },
                check_failure,
            })
        }
        Err(message) => Ok(TestRunOutcome {
            status: Status::Issue,
            output: format!("check failure: infrastructure\n{message}"),
            check_failure: Some(CheckFailureClass::Infrastructure),
        }),
    }
}

/// Runs the answer's own submitted `check` script — the same sandboxed
/// `python` execution [`super::configurable::run_python_script`]'s
/// `run_python` tool uses, against the fixed [`CHECK_SCRIPT_PATH`] rather
/// than the tool's own scratch path — and keeps its captured output, the
/// same shape [`RunProjectTestsCapturedBlock`] gives the project's own tests.
///
/// Wraps [`run_check_script`]: `CodingWorkspace, ShellExecutor,
/// Out<ProposedPatchIntent>, Out<AppliedEdit> -> TestRunOutcome`. Read-only —
/// it runs a script, not an edit — and gets the same `resource.missing`
/// mechanical exclusion [`RunProjectTestsCapturedBlock`] gets on a host with
/// no installed [`polaris::shell::ShellExecutor`]. A shell policy that
/// refuses the command, or a script that itself fails, is captured as
/// [`Status::Issue`] with the refusal or failure text as output, never a
/// hard error — only a proposal with no `check` at all (which the
/// [`super::predicates::has_check`] gate exists to route around) is.
pub struct RunCheckScriptBlock;

impl BlockFactory for RunCheckScriptBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(run_check_script))
    }
}

/// Returns [`RunCheckScriptBlock`]'s registered key.
#[must_use]
pub fn run_check_script_key() -> CompositionBlockKey {
    block_key("code.run-check-script")
}

/// Every block key this module registers.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![
        run_project_tests_captured_key(),
        judge_test_outcome_key(),
        judge_patch_intent_key(),
        model_propose_conversation_key(),
        resume_model_propose_key(),
        extract_patch_from_proposal_key(),
        run_check_script_key(),
    ]
}

/// Registers every block in this module into `catalogue`.
///
/// # Errors
///
/// Returns [`CompositionCatalogueError::DuplicateBlock`] if a key here
/// collides with one already registered.
pub(super) fn register_all(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    let schema = super::no_config_schema();
    catalogue.register_block(
        run_project_tests_captured_key(),
        Arc::new(RunProjectTestsCapturedBlock),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &run_project_tests_captured_key(),
        describe(
            "Runs the project's declared test command and keeps a bounded tail of its output \
             alongside the verdict.",
            Vec::new(),
            block_cards::test_run_outcome_port(),
            "first step of a repair loop's retry arm, wired ahead of code.judge-test-outcome and \
             a feedback-visible code.model-propose",
        )
        .requires(block_cards::test_runner_available()),
    )?;
    catalogue.register_block(
        judge_test_outcome_key(),
        Arc::new(JudgeTestOutcomeBlock),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &judge_test_outcome_key(),
        describe(
            "Projects a captured test run's verdict into the ordinary edit-site assessment, \
             without re-running anything.",
            Vec::new(),
            block_cards::edit_site_assessment_port(),
            "wired after code.run-project-tests-captured to feed a Choose predicate or \
             code.conclude-single-pass",
        ),
    )?;
    catalogue.register_block(
        judge_patch_intent_key(),
        Arc::new(JudgePatchIntentBlock),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &judge_patch_intent_key(),
        describe(
            "Judges a model-proposed patch against what actually landed on disk, treating an \
             empty proposal as an issue.",
            Vec::new(),
            block_cards::edit_site_assessment_port(),
            "wired after code.apply-patch-intent in a no-test-runner repair loop",
        ),
    )?;
    catalogue.register_block(
        model_propose_conversation_key(),
        Arc::new(ModelProposeConversationBlock),
        configurable::model_propose_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &model_propose_conversation_key(),
        describe(
            "Like code.model-propose, but keeps the full conversation so a later repair round \
             can resume it instead of starting fresh.",
            Vec::new(),
            block_cards::proposed_patch_intent_port(),
            "propose-0 of the self-check-gated statement-only shape, ahead of \
             code.extract-patch-from-proposal",
        )
        .requires(block_cards::model_provider_available())
        .requires(block_cards::shell_available()),
    )?;
    catalogue.register_block(
        resume_model_propose_key(),
        Arc::new(ResumeModelProposeBlock),
        configurable::model_propose_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &resume_model_propose_key(),
        describe(
            "Resumes an earlier model-propose-conversation (or resume-model-propose) round's \
             conversation for a repair attempt, with the preceding test outcome's captured \
             failure folded in.",
            Vec::new(),
            block_cards::proposed_patch_intent_port(),
            "a self-check repair round, reading the prior round's ProposedPatchIntent and \
             TestRunOutcome instead of starting a fresh conversation",
        )
        .requires(block_cards::model_provider_available())
        .requires(block_cards::shell_available()),
    )?;
    catalogue.register_block(
        extract_patch_from_proposal_key(),
        Arc::new(ExtractPatchFromProposalBlock),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &extract_patch_from_proposal_key(),
        describe(
            "Projects a conversation-keeping propose step's ProposedPatchIntent down to the \
             plain PatchIntent code.apply-patch-intent and code.judge-patch-intent already read.",
            Vec::new(),
            configurable::patch_intent_port_key(),
            "wired between code.model-propose-conversation (or code.resume-model-propose) and \
             code.apply-patch-intent",
        ),
    )?;
    catalogue.register_block(
        run_check_script_key(),
        Arc::new(RunCheckScriptBlock),
        schema,
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &run_check_script_key(),
        describe(
            "Runs the answer's own submitted check script and keeps its captured output \
             alongside the verdict, the same shape code.run-project-tests-captured gives the \
             project's own tests.",
            Vec::new(),
            block_cards::test_run_outcome_port(),
            "wired after code.apply-patch-intent in the self-check-gated statement-only shape, \
             gated by code.has-check",
        )
        .requires(block_cards::shell_available()),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use polaris::graph::GraphExecutor;
    use polaris::system::param::{Access, SystemContext};

    use super::*;

    use crate::cosmos::gate_blocks::configurable::ApplyPatchIntentBlock;
    use crate::cosmos::gate_blocks::configurable::{
        ModelConversation, ModelProposeBlock, ModelStepArtifact, ModelStepConfig,
        ModelStepOutputKind, ModelStepProvider, ScriptedModelStep, TokenBudget, ToolSubset,
    };
    use crate::cosmos::gate_blocks::existing::ConcludeSinglePass;
    use crate::cosmos::host::TopologyProduct;
    use crate::cosmos::task::{CodingIntent, CodingTask, ExactEdit, SuccessCheck};
    use crate::usage::TokenMeter;

    #[test]
    fn check_failure_classification_separates_runtime_from_behavior() {
        let failed = |stderr: &str, exit_code, timed_out| configurable::PythonRunOutcome {
            success: false,
            rendered: String::new(),
            stderr: stderr.to_owned(),
            exit_code,
            timed_out,
        };
        for stderr in [
            "ModuleNotFoundError: No module named 'x'",
            "File \".polaris/check_script.py\"\nSyntaxError: invalid syntax",
            "error: subprocess-exited-with-error",
        ] {
            assert_eq!(
                classify_check_failure(&failed(stderr, Some(1), false)),
                CheckFailureClass::Infrastructure
            );
        }
        for stderr in ["AssertionError: wrong result", "RuntimeError: bad result"] {
            assert_eq!(
                classify_check_failure(&failed(stderr, Some(1), false)),
                CheckFailureClass::Behavior
            );
        }
        assert_eq!(
            classify_check_failure(&failed("", None, true)),
            CheckFailureClass::Infrastructure
        );
    }

    fn no_config() -> ResolvedConfigurationRef {
        ResolvedConfigurationRef::inline(
            super::super::no_config_schema(),
            polaris_cosmos::contracts::ConfigurationValue::Null,
        )
        .expect("a null value satisfies the no-config schema")
    }

    fn guarantees(graph: &Graph, type_id: TypeId) -> bool {
        graph
            .signature()
            .guaranteed_produces()
            .iter()
            .any(|access: &Access| access.type_id == type_id)
    }

    fn workspace_with(content: &str) -> (tempfile::TempDir, CodingWorkspace) {
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), content).expect("seed file");
        let task = CodingTask::from_statement(
            "repair-block-fixture",
            dir.path(),
            "fix the bug",
            CodingIntent::Careful,
            SuccessCheck::command("true", Vec::<String>::new()).expect("command check"),
        )
        .expect("well-formed task");
        (dir, CodingWorkspace::for_task(&task))
    }

    #[tokio::test]
    async fn run_project_tests_captured_keeps_the_failing_output() {
        let (_dir, workspace) = workspace_with("todo!()");
        let mut graph = Graph::new();
        RunProjectTestsCapturedBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<TestRunOutcome>()));

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(ProjectTestRunner::command(
            "sh",
            ["-c", "echo boom-goes-the-test >&2; exit 1"],
        ));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("captured test run executes");
        let outcome = execution
            .output::<TestRunOutcome>()
            .expect("produces a TestRunOutcome");
        assert_eq!(outcome.status, Status::Issue);
        assert!(
            outcome.output.contains("boom-goes-the-test"),
            "the failing command's own output must be captured: {:?}",
            outcome.output
        );
    }

    #[tokio::test]
    async fn judge_test_outcome_projects_the_captured_verdict() {
        let (_dir, workspace) = workspace_with("todo!()");
        let mut graph = Graph::new();
        RunProjectTestsCapturedBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        JudgeTestOutcomeBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<Assessment<EditSite>>()));

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(ProjectTestRunner::command("true", Vec::<String>::new()));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("run then judge runs");
        let assessment = execution
            .output::<Assessment<EditSite>>()
            .expect("judge produces an assessment");
        assert_eq!(assessment.status(), Status::Pass);
    }

    #[tokio::test]
    async fn a_passing_test_only_plan_is_not_reported_resolved_for_a_statement_only_edit() {
        let dir = tempfile::tempdir().expect("temp project");
        let source = "todo!()";
        std::fs::write(dir.path().join("lib.rs"), source).expect("seed file");
        let task = CodingTask::from_statement(
            "test-only-plan",
            dir.path(),
            "repair the reported bug",
            CodingIntent::Careful,
            SuccessCheck::command("true", Vec::<String>::new()).expect("command check"),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        // This plan has no edit stage, then a passing test command, then the
        // ordinary single-pass conclusion.
        let mut graph = Graph::new();
        RunProjectTestsCapturedBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        JudgeTestOutcomeBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        ConcludeSinglePass
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(ProjectTestRunner::command("true", Vec::<String>::new()));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the test-only plan executes");
        let conclusion = execution
            .output::<polaris_cosmos::contracts::AssessmentConclusion>()
            .expect("the plan concludes");
        assert_eq!(
            conclusion.stop_reason(),
            polaris_cosmos::contracts::StopReason::GoalMet,
            "the captured-test graph itself sees the passing command"
        );
        let product = TopologyProduct::for_task(conclusion, &task, false);

        assert_eq!(
            product.kind,
            polaris_cosmos::contracts::ConclusionKind::Unresolved
        );
        assert_eq!(
            product.stop_reason,
            polaris_cosmos::contracts::StopReason::Stagnated
        );
        let answer = product
            .answer()
            .expect("the unresolved outcome is rendered");
        assert!(
            answer.starts_with("left the coding task unresolved"),
            "{answer}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lib.rs")).expect("read source"),
            source,
            "the test-only plan changed no workspace bytes"
        );
    }

    #[tokio::test]
    async fn judge_patch_intent_passes_a_landed_model_proposed_edit() {
        let (_dir, workspace) = workspace_with("todo!()");
        let mut graph = Graph::new();
        ModelProposeBlock
            .build(
                &mut graph,
                &ModelStepConfig::new(
                    "revise".to_owned(),
                    "propose a patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed configuration"),
            )
            .expect("builds");
        ApplyPatchIntentBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        JudgePatchIntentBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit"),
                ],
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("propose, apply, judge runs");
        let assessment = execution
            .output::<Assessment<EditSite>>()
            .expect("judge produces an assessment");
        assert_eq!(assessment.status(), Status::Pass);
    }

    #[tokio::test]
    async fn judge_patch_intent_refuses_an_empty_proposed_patch_as_a_pass() {
        let (_dir, workspace) = workspace_with("todo!()");
        let mut graph = Graph::new();
        ModelProposeBlock
            .build(
                &mut graph,
                &ModelStepConfig::new(
                    "revise".to_owned(),
                    "propose a patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed configuration"),
            )
            .expect("builds");
        ApplyPatchIntentBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        JudgePatchIntentBlock
            .build(&mut graph, &no_config())
            .expect("builds");

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: Vec::new(),
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("propose, apply, judge runs even for an empty patch");
        let assessment = execution
            .output::<Assessment<EditSite>>()
            .expect("judge produces an assessment");
        assert_eq!(
            assessment.status(),
            Status::Issue,
            "an empty proposed patch must never judge as a pass"
        );
    }

    #[tokio::test]
    async fn model_propose_conversation_produces_one_turn_per_round() {
        let (_dir, workspace) = workspace_with("todo!()");
        let mut graph = Graph::new();
        ModelProposeConversationBlock
            .build(
                &mut graph,
                &ModelStepConfig::new(
                    "revise".to_owned(),
                    "propose a patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed configuration"),
            )
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit"),
                ],
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("propose-conversation runs");
        let proposal = execution
            .output::<ProposedPatchIntent>()
            .expect("produces a ProposedPatchIntent");
        assert_eq!(
            proposal.conversation.len(),
            1,
            "one round must record exactly one turn: {:?}",
            proposal.conversation
        );
    }

    #[tokio::test]
    async fn resume_model_propose_continues_the_prior_conversation() {
        let (_dir, workspace) = workspace_with("todo!()");
        let mut graph = Graph::new();
        ModelProposeConversationBlock
            .build(
                &mut graph,
                &ModelStepConfig::new(
                    "revise".to_owned(),
                    "propose a patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed configuration"),
            )
            .expect("builds");
        // Stands in for a real check run, so this test isolates conversation
        // continuity from `RunCheckScriptBlock`'s own shell dependency.
        graph.add_system_node(|| async {
            TestRunOutcome {
                status: Status::Issue,
                output: "boom".to_owned(),
                check_failure: Some(CheckFailureClass::Behavior),
            }
        });
        ResumeModelProposeBlock
            .build(
                &mut graph,
                &ModelStepConfig::new(
                    "revise".to_owned(),
                    "propose a corrected patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed configuration"),
            )
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit"),
                ],
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("propose then resume runs");
        let resumed = execution
            .output::<ProposedPatchIntent>()
            .expect("resume produces a ProposedPatchIntent");
        assert_eq!(
            resumed.conversation.len(),
            2,
            "the resumed conversation must carry the first round's turn forward, plus its own: {:?}",
            resumed.conversation
        );
    }

    #[tokio::test]
    async fn extract_patch_from_proposal_projects_the_plain_patch() {
        let mut graph = Graph::new();
        graph.add_system_node(|| async {
            ProposedPatchIntent {
                patch: PatchIntent {
                    edits: vec![
                        ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit"),
                    ],
                    check: None,
                },
                conversation: ModelConversation::default(),
            }
        });
        ExtractPatchFromProposalBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("extract runs");
        let patch = execution
            .output::<PatchIntent>()
            .expect("produces a PatchIntent");
        assert_eq!(patch.edits.len(), 1);
    }

    #[tokio::test]
    async fn run_check_script_reports_issue_when_the_shell_policy_denies_the_script() {
        let (_dir, workspace) = workspace_with("todo!()");
        let working_dir = workspace.config().working_dir.clone();
        // No allowed-command pattern at all, so `python ...` defaults to
        // `ShellPermission::Confirm` — not `Allow` — and the block must fail
        // closed rather than block on a human that is not there.
        let shell = polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new().with_working_dir(&working_dir),
        );

        let mut graph = Graph::new();
        graph.add_system_node(|| async {
            ProposedPatchIntent {
                patch: PatchIntent {
                    edits: Vec::new(),
                    check: Some("print('ok')".to_owned()),
                },
                conversation: ModelConversation::default(),
            }
        });
        graph.add_system_node(|| async {
            AppliedEdit {
                outcomes: Vec::new(),
            }
        });
        RunCheckScriptBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut globals = polaris::system::resource::Resources::new();
        globals.insert(shell);
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(workspace);
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("run-check-script runs even when the shell policy denies it");
        let outcome = execution
            .output::<TestRunOutcome>()
            .expect("produces a TestRunOutcome");
        assert_eq!(outcome.status, Status::Issue);
        assert!(
            outcome.output.contains("does not allow"),
            "the denial reason must be captured as output: {:?}",
            outcome.output
        );
    }

    #[tokio::test]
    async fn run_check_script_reports_pass_when_the_script_exits_zero() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, workspace) = workspace_with("todo!()");
        let working_dir = workspace.config().working_dir.clone();

        // This sandbox may only have `python3` on its real PATH, but
        // `run_python_script` always shells out to the literal `python`; a
        // shim standing in for it keeps this test independent of the host's
        // own installed interpreters.
        let bin_dir = tempfile::tempdir().expect("temp bin dir");
        let python_shim = bin_dir.path().join("python");
        std::fs::write(&python_shim, "#!/bin/sh\nexec python3 \"$@\"\n").expect("write shim");
        let mut perms = std::fs::metadata(&python_shim)
            .expect("stat shim")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&python_shim, perms).expect("chmod shim");

        let shell = polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(&working_dir)
                .with_allowed_dirs(vec![working_dir.clone()])
                .with_allowed_commands(vec!["python *".to_owned()])
                .with_path(format!("{}:/usr/bin:/bin", bin_dir.path().display())),
        );

        let mut graph = Graph::new();
        graph.add_system_node(|| async {
            ProposedPatchIntent {
                patch: PatchIntent {
                    edits: Vec::new(),
                    check: Some("print('first')\\nprint('ok')".to_owned()),
                },
                conversation: ModelConversation::default(),
            }
        });
        graph.add_system_node(|| async {
            AppliedEdit {
                outcomes: Vec::new(),
            }
        });
        RunCheckScriptBlock
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut globals = polaris::system::resource::Resources::new();
        globals.insert(shell);
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(workspace);
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("run-check-script runs");
        let outcome = execution
            .output::<TestRunOutcome>()
            .expect("produces a TestRunOutcome");
        assert_eq!(outcome.status, Status::Pass, "{:?}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(working_dir.join(".polaris/check_script.py"))
                .expect("submitted script was written"),
            "print('first')\nprint('ok')"
        );
    }

    #[tokio::test]
    async fn run_check_script_fails_hard_with_no_check_in_the_proposal() {
        let (_dir, workspace) = workspace_with("todo!()");
        let working_dir = workspace.config().working_dir.clone();
        let shell = polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new().with_working_dir(&working_dir),
        );

        let mut graph = Graph::new();
        graph.add_system_node(|| async {
            ProposedPatchIntent {
                patch: PatchIntent {
                    edits: Vec::new(),
                    check: None,
                },
                conversation: ModelConversation::default(),
            }
        });
        graph.add_system_node(|| async {
            AppliedEdit {
                outcomes: Vec::new(),
            }
        });
        RunCheckScriptBlock
            .build(&mut graph, &no_config())
            .expect("builds");

        let mut globals = polaris::system::resource::Resources::new();
        globals.insert(shell);
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(workspace);
        let result = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await;
        assert!(
            result.is_err(),
            "a run-check-script block with no check in the proposal must not silently succeed"
        );
    }

    #[test]
    fn every_key_is_distinct() {
        let mut keys = keys();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), before);
    }
}
