//! Existing coding stages, individually addressable as gate-plan blocks.
//!
//! Every [`BlockFactory`] here adds one already-shipped coding system — or
//! one already-shipped Cosmos combinator instantiated with already-shipped
//! coding systems — onto the graph a gate-plan materializer is building.
//! Nothing here changes what [`graphs`] already builds and registers:
//! [`graphs::direct_edit_graph`], [`graphs::prediction_error_edit_graph`],
//! [`graphs::worklist_sweep_graph`], and [`graphs::tree_question_graph`] keep
//! building exactly the graphs they always did. This module only makes their
//! pieces individually reachable, so a gate plan can recombine them in shapes
//! those functions never built — for example judging a model-proposed edit
//! without the direct graph's own apply stage, or concluding a single pass
//! after a differently sourced [`AppliedEdit`].
//!
//! None of these blocks take configuration: they all share
//! [`super::no_config_schema`].
//!
//! Two families stay bundled rather than decomposed into one block per
//! system: `code.predict-edit-loop` wraps the whole
//! [`prediction_error_loop::build`] shape (its accept/reject branches and its
//! pre-loop initializer are one indivisible native loop), and
//! `code.drain-worklist-sweep` wraps the whole seeded worklist drain plus its
//! resolver. Decomposing either into free-standing `Step` gates would
//! duplicate control flow Cosmos already ships as one reusable builder.
//!
//! # Guaranteed vs. merely produced
//!
//! `polaris_graph::GraphSignature::guaranteed_produces` states which outputs
//! survive every successful path — a loop body's own production is excluded
//! in general, since a loop may run zero iterations before its termination
//! check first succeeds. That rule turns out not to weaken any block here:
//! every block below, including `code.predict-edit-loop`, guarantees its
//! documented output on every successful run. `tests` below checks each
//! one against the real derived signature rather than asserting it by hand
//! — this catalogue makes no *unverified* claim about what a future `Seal`
//! or `Slot` referencing one of these blocks alone could rely on.

use std::sync::Arc;

use polaris::graph::{Graph, NodeId};
use polaris::system::system::erase;
use polaris_cosmos::cognition::approved::blocks;
use polaris_cosmos::cognition::prediction_error_loop;
use polaris_cosmos::composition::{
    BlockBuildError, BlockDescription, BlockFactory, CompositionCatalogue,
    CompositionCatalogueError, Effect,
};
use polaris_cosmos::contracts::{
    CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef,
};

use super::super::block_cards;
use super::super::graphs::{
    accept_edit, act_edit, answer_question, assess_findings, compare_edit, conclude_prediction,
    conclude_single_pass, conclude_sweep, direct_apply, direct_judge, initialize_feedback,
    predict_edit, reject_edit, seed_worklist, survey_tree, test_gated_judge, verify_edit,
    visit_site,
};
use super::super::payload::{EditFeedback, SweepProgress};
use super::behavior_version;

fn describe(
    summary: &str,
    inputs: Vec<polaris_cosmos::composition::StablePortKey>,
    output: polaris_cosmos::composition::StablePortKey,
    example: &str,
) -> BlockDescription {
    BlockDescription::new(summary, inputs, output, example)
        .expect("block description is well-formed")
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("block ID is a valid stable name"),
        behavior_version(),
    )
}

/// Applies a unit of work's stated exact-string edits to the real tree.
///
/// Wraps [`direct_apply`]: `CodingWorkspace -> AppliedEdit`. Mutating, not
/// repeatable.
pub struct ApplyDirect;

impl BlockFactory for ApplyDirect {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(direct_apply))
    }
}

/// Returns [`ApplyDirect`]'s registered key.
#[must_use]
pub fn apply_direct_key() -> CompositionBlockKey {
    block_key("code.apply-direct")
}

/// Judges the stated edits against what actually landed on disk.
///
/// Wraps [`direct_judge`]: `CodingWorkspace, Out<AppliedEdit> ->
/// Assessment<EditSite>`. Read-only.
pub struct JudgeStatedEdits;

impl BlockFactory for JudgeStatedEdits {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(direct_judge))
    }
}

/// Returns [`JudgeStatedEdits`]'s registered key.
#[must_use]
pub fn judge_stated_edits_key() -> CompositionBlockKey {
    block_key("code.judge-stated-edits")
}

/// Judges an applied edit by running the declared [`ProjectTestRunner`]
/// command.
///
/// Wraps [`test_gated_judge`]: `CodingWorkspace, ProjectTestRunner,
/// Out<AppliedEdit> -> Assessment<EditSite>`. Read-only. A materializer
/// admitting a plan naming this block onto a host with no installed
/// [`ProjectTestRunner`](super::super::payload::ProjectTestRunner) will see
/// the same `resource.missing` mechanical exclusion
/// [`graphs::test_gated_edit_graph`] already does.
pub struct RunProjectTests;

impl BlockFactory for RunProjectTests {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(test_gated_judge))
    }
}

/// Returns [`RunProjectTests`]'s registered key.
#[must_use]
pub fn run_project_tests_key() -> CompositionBlockKey {
    block_key("code.run-project-tests")
}

/// Accounts every stated edit site and concludes a single-pass unit of work.
///
/// Wraps [`conclude_single_pass`]: `CodingWorkspace, Out<Assessment<EditSite>>
/// -> AssessmentConclusion`. Read-only.
pub struct ConcludeSinglePass;

impl BlockFactory for ConcludeSinglePass {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(conclude_single_pass))
    }
}

/// Returns [`ConcludeSinglePass`]'s registered key.
#[must_use]
pub fn conclude_single_pass_key() -> CompositionBlockKey {
    block_key("code.conclude-single-pass")
}

/// Runs the whole predict/act/verify/compare loop over the stated edits.
///
/// Wraps [`prediction_error_loop::build`] with the coding stages
/// [`predict_edit`] etc.: `CodingWorkspace -> EditFeedback`. Mutating and not
/// repeatable — its `act` stage writes to the tree exactly once, which is
/// why the loop itself is capped at one committed prediction (see
/// [`graphs::prediction_error_edit_graph`]'s own doc comment). `EditFeedback`
/// is guaranteed on every successful run — verified, not assumed, in
/// `tests::predict_edit_loop_guarantees_edit_feedback`.
pub struct PredictEditLoop;

impl BlockFactory for PredictEditLoop {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        prediction_error_loop::build(
            graph,
            "polaris_code_gate_predict_edit_loop",
            prediction_error_loop::Stages::new(
                initialize_feedback,
                predict_edit,
                act_edit,
                verify_edit,
                compare_edit,
            ),
            prediction_error_loop::Policies::new(
                |feedback: &EditFeedback| feedback.iteration >= 1,
                |feedback: &EditFeedback| feedback.matched,
                |branch: &mut Graph| {
                    branch.add_system(accept_edit);
                },
                |branch: &mut Graph| {
                    branch.add_system(reject_edit);
                },
            ),
        );
        graph
            .last_node()
            .ok_or_else(|| BlockBuildError::new("predict-edit-loop added no node"))
    }
}

/// Returns [`PredictEditLoop`]'s registered key.
#[must_use]
pub fn predict_edit_loop_key() -> CompositionBlockKey {
    block_key("code.predict-edit-loop")
}

/// Concludes a prediction-error loop's feedback into the coding contract.
///
/// Wraps [`conclude_prediction`]: `CodingWorkspace, Out<EditFeedback> ->
/// AssessmentConclusion`. Read-only. The specialised conclusion
/// [`PredictEditLoop`] needs, since that block's own signature stops at
/// `EditFeedback`.
pub struct ConcludePrediction;

impl BlockFactory for ConcludePrediction {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(conclude_prediction))
    }
}

/// Returns [`ConcludePrediction`]'s registered key.
#[must_use]
pub fn conclude_prediction_key() -> CompositionBlockKey {
    block_key("code.conclude-prediction")
}

/// Runs the bounded worklist sweep over every stub site and concludes it.
///
/// Wraps [`blocks::drain_worklist`] seeded by [`seed_worklist`] and bodied by
/// [`visit_site`], closed by [`blocks::resolve_residuals`] over
/// [`conclude_sweep`] — the same shape [`graphs::worklist_sweep_graph`]
/// already builds and registers. `CodingWorkspace -> AssessmentConclusion`.
/// Mutating and not repeatable: a visited site that is a stated target is
/// rewritten in place. Guaranteed: the resolver runs unconditionally after
/// the loop, not inside its body.
pub struct DrainWorklistSweep;

impl BlockFactory for DrainWorklistSweep {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        blocks::drain_worklist(
            graph,
            "polaris_code_gate_worklist_sweep",
            erase(seed_worklist),
            Box::new(|progress: &SweepProgress| {
                progress.remaining == 0 || progress.budget_left == 0
            }),
            Box::new(|body: &mut Graph| {
                body.add_system(visit_site);
            }),
        );
        blocks::resolve_residuals(graph, erase(conclude_sweep), false);
        graph
            .last_node()
            .ok_or_else(|| BlockBuildError::new("drain-worklist-sweep added no node"))
    }
}

/// Returns [`DrainWorklistSweep`]'s registered key.
#[must_use]
pub fn drain_worklist_sweep_key() -> CompositionBlockKey {
    block_key("code.drain-worklist-sweep")
}

/// Surveys the working tree for one closed-vocabulary question.
///
/// Wraps [`survey_tree`]: `CodingWorkspace, CodingQuestion ->
/// Vec<QuestionFinding>`. Read-only.
pub struct SurveyTree;

impl BlockFactory for SurveyTree {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(survey_tree))
    }
}

/// Returns [`SurveyTree`]'s registered key.
#[must_use]
pub fn survey_tree_key() -> CompositionBlockKey {
    block_key("code.survey-tree")
}

/// Validates a finding ledger against the question-finding subject schema.
///
/// Wraps [`assess_findings`]: `Out<Vec<QuestionFinding>> ->
/// Vec<Assessment<QuestionFinding>>`. Read-only.
pub struct AssessFindings;

impl BlockFactory for AssessFindings {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(assess_findings))
    }
}

/// Returns [`AssessFindings`]'s registered key.
#[must_use]
pub fn assess_findings_key() -> CompositionBlockKey {
    block_key("code.assess-findings")
}

/// Terminates a question ledger in the coding contract.
///
/// Wraps [`answer_question`]: `CodingWorkspace,
/// Out<Vec<Assessment<QuestionFinding>>> -> AssessmentConclusion`. Read-only.
pub struct AnswerQuestion;

impl BlockFactory for AnswerQuestion {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(answer_question))
    }
}

/// Returns [`AnswerQuestion`]'s registered key.
#[must_use]
pub fn answer_question_key() -> CompositionBlockKey {
    block_key("code.answer-question")
}

/// Every block key this module registers.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![
        apply_direct_key(),
        judge_stated_edits_key(),
        run_project_tests_key(),
        conclude_single_pass_key(),
        predict_edit_loop_key(),
        conclude_prediction_key(),
        drain_worklist_sweep_key(),
        survey_tree_key(),
        assess_findings_key(),
        answer_question_key(),
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
        apply_direct_key(),
        Arc::new(ApplyDirect),
        schema.clone(),
        Effect::Mutating { repeat_safe: false },
    )?;
    catalogue.describe_block(
        &apply_direct_key(),
        describe(
            "Applies the task's own stated exact-string edits to the working tree.",
            Vec::new(),
            block_cards::applied_edit_port(),
            "first step of a direct single-pass edit plan, run before code.judge-stated-edits",
        )
        .requires(block_cards::stated_edits_present()),
    )?;
    catalogue.register_block(
        judge_stated_edits_key(),
        Arc::new(JudgeStatedEdits),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &judge_stated_edits_key(),
        describe(
            "Judges the task's stated edits against what actually landed on disk.",
            Vec::new(),
            block_cards::edit_site_assessment_port(),
            "wired after code.apply-direct in a Seq to verify the edit landed",
        )
        .requires(block_cards::stated_edits_present()),
    )?;
    catalogue.register_block(
        run_project_tests_key(),
        Arc::new(RunProjectTests),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &run_project_tests_key(),
        describe(
            "Judges an applied edit by running the project's declared test command.",
            Vec::new(),
            block_cards::edit_site_assessment_port(),
            "placed after an apply block in a Seq when a test runner is installed",
        )
        .requires(block_cards::test_runner_available()),
    )?;
    catalogue.register_block(
        conclude_single_pass_key(),
        Arc::new(ConcludeSinglePass),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &conclude_single_pass_key(),
        describe(
            "Accounts a single edit-site assessment and concludes a single-pass unit of work.",
            Vec::new(),
            block_cards::assessment_conclusion_port(),
            "final step of a direct or test-gated single-pass plan",
        ),
    )?;
    catalogue.register_block(
        predict_edit_loop_key(),
        Arc::new(PredictEditLoop),
        schema.clone(),
        Effect::Mutating { repeat_safe: false },
    )?;
    catalogue.describe_block(
        &predict_edit_loop_key(),
        describe(
            "Runs the whole predict/act/verify/compare loop over the task's stated edits, \
             committing at most one prediction.",
            Vec::new(),
            block_cards::edit_feedback_port(),
            "used in place of code.apply-direct when the plan wants a self-checked single edit \
             attempt",
        )
        .requires(block_cards::stated_edits_present()),
    )?;
    catalogue.register_block(
        conclude_prediction_key(),
        Arc::new(ConcludePrediction),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &conclude_prediction_key(),
        describe(
            "Concludes a prediction-error loop's feedback into the coding contract.",
            Vec::new(),
            block_cards::assessment_conclusion_port(),
            "wired after code.predict-edit-loop to terminate the plan",
        )
        .requires(block_cards::stated_edits_present()),
    )?;
    catalogue.register_block(
        drain_worklist_sweep_key(),
        Arc::new(DrainWorklistSweep),
        schema.clone(),
        Effect::Mutating { repeat_safe: false },
    )?;
    catalogue.describe_block(
        &drain_worklist_sweep_key(),
        describe(
            "Runs a bounded sweep over every stub site the workspace itself reports and \
             concludes it.",
            Vec::new(),
            block_cards::assessment_conclusion_port(),
            "a whole self-contained plan for a worklist sweep over the workspace's own stub sites",
        ),
    )?;
    catalogue.register_block(
        survey_tree_key(),
        Arc::new(SurveyTree),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &survey_tree_key(),
        describe(
            "Surveys the working tree for one closed-vocabulary question.",
            Vec::new(),
            block_cards::question_findings_port(),
            "first step of a read-only question-answering plan",
        )
        .requires(block_cards::question_task()),
    )?;
    catalogue.register_block(
        assess_findings_key(),
        Arc::new(AssessFindings),
        schema.clone(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &assess_findings_key(),
        describe(
            "Validates a finding ledger against the question-finding subject schema.",
            Vec::new(),
            block_cards::assessed_question_findings_port(),
            "wired after code.survey-tree to turn its findings into an assessment ledger",
        ),
    )?;
    catalogue.register_block(
        answer_question_key(),
        Arc::new(AnswerQuestion),
        schema,
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &answer_question_key(),
        describe(
            "Terminates a question ledger in the coding contract.",
            Vec::new(),
            block_cards::assessment_conclusion_port(),
            "final step of a read-only question-answering plan, after code.assess-findings",
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use polaris::graph::GraphExecutor;
    use polaris::system::param::{Access, SystemContext};
    use polaris_cosmos::contracts::{AssessmentConclusion, ConfigurationValue, Status};

    use super::*;
    use crate::cosmos::payload::{AppliedEdit, CodingWorkspace, ProjectTestRunner, SweepWorklist};
    use crate::cosmos::task::{CodingIntent, CodingTask};

    const ADD_STUB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}";
    const ADD_DONE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";
    const LIB_RS: &str = "\
//! A tiny crate with one unimplemented helper.

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    todo!()
}
";

    fn no_config() -> ResolvedConfigurationRef {
        ResolvedConfigurationRef::inline(super::super::no_config_schema(), ConfigurationValue::Null)
            .expect("a null value satisfies the no-config schema")
    }

    fn workspace() -> (tempfile::TempDir, CodingWorkspace) {
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
        std::fs::write(dir.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
        let task = CodingTask::new(
            "gate-block-isolation",
            dir.path(),
            "src/lib.rs",
            ADD_STUB,
            ADD_DONE,
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("the coding task is well formed");
        (dir, CodingWorkspace::for_task(&task))
    }

    fn guarantees(graph: &Graph, type_id: TypeId) -> bool {
        graph
            .signature()
            .guaranteed_produces()
            .iter()
            .any(|access: &Access| access.type_id == type_id)
    }

    #[tokio::test]
    async fn apply_direct_writes_the_stated_edit_alone() {
        let (_dir, workspace) = workspace();
        let mut graph = Graph::new();
        ApplyDirect.build(&mut graph, &no_config()).expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<AppliedEdit>()));

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("apply-direct runs standalone");

        let target = workspace.read(workspace.subject_path()).expect("read back");
        assert!(target.contains("a + b"));
        assert!(!target.contains(ADD_STUB));
    }

    #[tokio::test]
    async fn judge_stated_edits_runs_after_an_upstream_apply() {
        let (_dir, workspace) = workspace();
        let mut graph = Graph::new();
        ApplyDirect.build(&mut graph, &no_config()).expect("builds");
        JudgeStatedEdits
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(
            &graph,
            TypeId::of::<polaris_cosmos::contracts::Assessment<crate::cosmos::payload::EditSite>>()
        ));

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("apply then judge runs");
        let assessment = execution
            .output::<polaris_cosmos::contracts::Assessment<crate::cosmos::payload::EditSite>>()
            .expect("judge produces an assessment");
        assert_eq!(assessment.status(), Status::Pass);
    }

    #[tokio::test]
    async fn run_project_tests_reflects_the_real_command_exit_status() {
        let build_workspace = workspace;
        let (_dir, workspace) = build_workspace();
        let mut graph = Graph::new();
        ApplyDirect.build(&mut graph, &no_config()).expect("builds");
        RunProjectTests
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        // No runner installed: the same mechanical exclusion
        // `graphs::test_gated_edit_graph` gets on a hermetic host.
        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let outcome = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await;
        assert!(outcome.is_err(), "no runner is installed");

        context.insert(ProjectTestRunner::command("true", Vec::<String>::new()));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("with a passing command installed, the block runs");
        let assessment = execution
            .output::<polaris_cosmos::contracts::Assessment<crate::cosmos::payload::EditSite>>()
            .expect("judge produces an assessment");
        assert_eq!(assessment.status(), Status::Pass);

        let (_dir2, workspace2) = build_workspace();
        let mut context2 = SystemContext::new();
        context2.insert(workspace2);
        context2.insert(ProjectTestRunner::command("false", Vec::<String>::new()));
        let execution2 = GraphExecutor::new()
            .execute(&graph, &mut context2, None, None)
            .await
            .expect("a failing command still runs the block");
        let assessment2 = execution2
            .output::<polaris_cosmos::contracts::Assessment<crate::cosmos::payload::EditSite>>()
            .expect("judge produces an assessment");
        assert_eq!(
            assessment2.status(),
            Status::Issue,
            "a nonzero exit is an issue, not a silent pass"
        );
    }

    #[tokio::test]
    async fn conclude_single_pass_terminates_the_coding_contract() {
        let (_dir, workspace) = workspace();
        let mut graph = Graph::new();
        ApplyDirect.build(&mut graph, &no_config()).expect("builds");
        JudgeStatedEdits
            .build(&mut graph, &no_config())
            .expect("builds");
        ConcludeSinglePass
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<AssessmentConclusion>()));

        let mut context = SystemContext::new();
        context.insert(workspace);
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the whole single-pass chain runs");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(
            conclusion.stop_reason(),
            polaris_cosmos::contracts::StopReason::GoalMet
        );
    }

    #[tokio::test]
    async fn predict_edit_loop_guarantees_edit_feedback() {
        let (_dir, workspace) = workspace();
        let mut graph = Graph::new();
        PredictEditLoop
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<EditFeedback>()));

        // The graph's own last-executed node is the accept/reject branch
        // marker (unit output), not the EditFeedback producer — a plain
        // `ExecutionResult::output` check would test the wrong thing here.
        // `guaranteed_produces` above is the real claim this test makes.
        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("predict-edit-loop runs standalone");
    }

    #[tokio::test]
    async fn predict_edit_loop_then_conclude_prediction_falsifies_its_own_prediction() {
        let (_dir, workspace) = workspace();
        let mut graph = Graph::new();
        PredictEditLoop
            .build(&mut graph, &no_config())
            .expect("builds");
        ConcludePrediction
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("predict-edit-loop then conclude-prediction runs");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(
            conclusion.stop_reason(),
            polaris_cosmos::contracts::StopReason::PredicateSatisfied
        );
        assert!(
            workspace
                .read(workspace.subject_path())
                .expect("read back")
                .contains("a + b")
        );
    }

    #[tokio::test]
    async fn drain_worklist_sweep_guarantees_its_conclusion() {
        let (_dir, workspace) = workspace();
        let mut graph = Graph::new();
        DrainWorklistSweep
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<AssessmentConclusion>()));

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(SweepWorklist::new(crate::cosmos::graphs::SWEEP_BUDGET));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the sweep runs standalone");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(
            conclusion.stop_reason(),
            polaris_cosmos::contracts::StopReason::WorkExhausted
        );
        assert!(
            workspace
                .read(workspace.subject_path())
                .expect("read back")
                .contains("a + b")
        );
    }

    #[tokio::test]
    async fn survey_assess_answer_compose_freely_for_a_read_only_question() {
        let (_dir, workspace) = workspace();
        let mut graph = Graph::new();
        SurveyTree.build(&mut graph, &no_config()).expect("builds");
        AssessFindings
            .build(&mut graph, &no_config())
            .expect("builds");
        AnswerQuestion
            .build(&mut graph, &no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<AssessmentConclusion>()));

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(crate::cosmos::payload::CodingQuestion::locate("todo!()"));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the three question stages run in sequence");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(
            conclusion.stop_reason(),
            polaris_cosmos::contracts::StopReason::GoalMet
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
