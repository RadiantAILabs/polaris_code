//! The coding block catalogue seals, and three gate plans — naming different
//! blocks in different shapes — each materialise through the composition
//! module's real `materialize` into a distinct native `Graph` that validates
//! and runs to a concluded coding contract.
//!
//! Every plan below is an ordinary [`GatePlan`] value: a tree of catalogue
//! keys and resolved configuration, never hand-built `Graph` calls. Building
//! it, checking its shape and effects, resolving every key against the sealed
//! [`coding_block_catalogue`], and validating the result against the coding
//! target contract is exactly what a real planner-submitted plan goes
//! through.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use polaris::graph::GraphExecutor;
use polaris::system::param::SystemContext;
use polaris::system::system::SystemError;
use polaris_code_core::cosmos::gate_blocks::configurable::{
    ModelStepArtifact, ModelStepConfig, ModelStepExecutor, ModelStepHandoff, ModelStepOutputKind,
    ModelStepProvider, ScriptedModelStep, TokenBudget, ToolSubset, WorkspaceView,
};
use polaris_code_core::cosmos::gate_blocks::{
    clarification, coding_block_catalogue, coding_target_contracts, configurable, existing,
    keep_last_iteration_key, no_config_schema, predicates, survey,
};
use polaris_code_core::cosmos::payload::{CodingQuestion, CodingWorkspace, ProjectTestRunner};
use polaris_code_core::cosmos::task::{CodingIntent, CodingTask, ExactEdit, SuccessCheck};
use polaris_code_core::cosmos::topology;
use polaris_code_core::usage::TokenMeter;
use polaris_cosmos::composition::{
    BlockInstanceId, CaseKey, CompositionLimits, ConfigId, GateExpr, GatePlan, LoopControl,
    StepPlan, SwitchCase, WirePlan, materialize,
};
use polaris_cosmos::contracts::{
    AssessmentConclusion, CompositionBlockKey, ConfigurationValue, ResolvedConfigurationRef,
    StopReason,
};

/// One `Step` gate over `block`, labeled `instance` and resolved against
/// `config`.
fn step(instance: &str, block: CompositionBlockKey, config: ConfigId) -> GateExpr {
    GateExpr::Step(StepPlan::new(
        BlockInstanceId::new(instance).expect("instance label is a valid stable name"),
        block,
        config,
    ))
}

/// The shared configuration key every no-config block instance below
/// resolves to.
fn no_config_id() -> ConfigId {
    ConfigId::new("no-config").expect("config label is a valid stable name")
}

/// The resolved value [`no_config_id`] maps to: a null value satisfying
/// [`no_config_schema`].
fn no_config_ref() -> ResolvedConfigurationRef {
    ResolvedConfigurationRef::inline(no_config_schema(), ConfigurationValue::Null)
        .expect("a null value satisfies the no-config schema")
}

/// Builds and validates a [`GatePlan`] against the sealed coding catalogue and
/// the coding target contract.
fn build_plan(
    catalogue: &polaris_cosmos::composition::CompositionCatalogueSnapshot,
    target: polaris_cosmos::topology::TopologyContractKey,
    expression: GateExpr,
    configurations: BTreeMap<ConfigId, ResolvedConfigurationRef>,
) -> GatePlan {
    build_plan_with_wires(catalogue, target, expression, Vec::new(), configurations)
}

/// Builds a plan that additionally contains explicit stable-port wires.
fn build_plan_with_wires(
    catalogue: &polaris_cosmos::composition::CompositionCatalogueSnapshot,
    target: polaris_cosmos::topology::TopologyContractKey,
    expression: GateExpr,
    wires: Vec<WirePlan>,
    configurations: BTreeMap<ConfigId, ResolvedConfigurationRef>,
) -> GatePlan {
    GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        target,
        expression,
        wires,
        configurations,
    )
    .expect("a well-formed plan computes its canonical digest")
}

/// The coding target contract every plan below materialises against.
fn target_contract() -> polaris_cosmos::topology::TopologyContractKey {
    topology::contract()
        .expect("the coding contract registers cleanly")
        .key()
        .clone()
}

fn edit_workspace(dir: &std::path::Path, find: &str, replace: &str) -> CodingWorkspace {
    let task = CodingTask::new(
        "gate-plan-fixture",
        dir,
        "lib.rs",
        find,
        replace,
        "todo!()",
        CodingIntent::Careful,
    )
    .expect("well-formed task");
    CodingWorkspace::for_task(&task)
}

/// Plan 1: one stated edit, fast objective —
/// `Seq(apply-direct, judge-stated-edits, conclude-single-pass)`.
#[tokio::test]
async fn plan_one_stated_edit_materialises_and_validates() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
    let workspace = edit_workspace(dir.path(), "todo!()", "1 + 1");

    let config = no_config_id();
    let expression = GateExpr::Seq(vec![
        step("apply", existing::apply_direct_key(), config.clone()),
        step("judge", existing::judge_stated_edits_key(), config.clone()),
        step(
            "conclude",
            existing::conclude_single_pass_key(),
            config.clone(),
        ),
    ]);
    let plan = build_plan(
        &catalogue,
        target_contract(),
        expression,
        BTreeMap::from([(config, no_config_ref())]),
    );

    let graph = materialize(
        &plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("plan 1 materialises");
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

    let mut context = SystemContext::new();
    context.insert(workspace.clone());
    let execution = GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .expect("plan 1 executes");
    let conclusion = execution
        .output::<AssessmentConclusion>()
        .expect("concludes the coding contract");
    assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    assert_eq!(workspace.read("lib.rs").expect("read back"), "1 + 1");
}

/// Plan 2: multi-file edit, test runner — a bounded repair loop unrolled as
/// two nested `Choose` gates rather than a native `Loop`, since a repair
/// attempt applies a patch (`Mutating { repeat_safe: false }`), which a
/// `Loop` body may not contain.
///
/// Shape: `Seq(apply-direct, run-project-tests,
/// Choose(tests-passed; affirm-assessment,
///   Seq(model-propose, apply-patch-intent, run-project-tests,
///     Choose(tests-passed; affirm-assessment,
///       Seq(model-propose, apply-patch-intent, run-project-tests)))),
/// conclude-single-pass)`. `affirm-assessment` occupies the "already passing,
/// nothing to do" arms so `conclude-single-pass`'s edit-site assessment stays
/// guaranteed regardless of which arm ran.
#[tokio::test]
async fn plan_two_test_gated_repair_loop_materialises_and_validates() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let dir = tempfile::tempdir().expect("temp project");
    // The stated edit lands somewhere that does not itself satisfy the
    // success check, so the initial test run genuinely fails and the repair
    // arm is the one exercised.
    std::fs::write(dir.path().join("lib.rs"), "todo!()\n// note").expect("seed file");
    let workspace = edit_workspace(dir.path(), "// note", "// noted");

    let config = no_config_id();
    let model_config = ConfigId::new("propose-fix").expect("config label is a valid stable name");

    let innermost_repair = GateExpr::Seq(vec![
        step(
            "propose-2",
            configurable::model_propose_key(),
            model_config.clone(),
        ),
        step(
            "apply-patch-2",
            configurable::apply_patch_intent_key(),
            config.clone(),
        ),
        step(
            "run-tests-3",
            existing::run_project_tests_key(),
            config.clone(),
        ),
    ]);
    let inner_choose = GateExpr::Choose {
        predicate: predicates::tests_passed_key(),
        when_true: Box::new(step(
            "affirm-2",
            configurable::affirm_assessment_key(),
            config.clone(),
        )),
        when_false: Box::new(innermost_repair),
    };
    let outer_repair = GateExpr::Seq(vec![
        step(
            "propose-1",
            configurable::model_propose_key(),
            model_config.clone(),
        ),
        step(
            "apply-patch-1",
            configurable::apply_patch_intent_key(),
            config.clone(),
        ),
        step(
            "run-tests-2",
            existing::run_project_tests_key(),
            config.clone(),
        ),
        inner_choose,
    ]);
    let outer_choose = GateExpr::Choose {
        predicate: predicates::tests_passed_key(),
        when_true: Box::new(step(
            "affirm-1",
            configurable::affirm_assessment_key(),
            config.clone(),
        )),
        when_false: Box::new(outer_repair),
    };
    let expression = GateExpr::Seq(vec![
        step("apply", existing::apply_direct_key(), config.clone()),
        step(
            "run-tests-1",
            existing::run_project_tests_key(),
            config.clone(),
        ),
        outer_choose,
        step(
            "conclude",
            existing::conclude_single_pass_key(),
            config.clone(),
        ),
    ]);

    let mut configurations = BTreeMap::new();
    configurations.insert(config, no_config_ref());
    configurations.insert(
        model_config,
        ModelStepConfig::new(
            "revise".to_owned(),
            "propose the fix that makes the failing check pass".to_owned(),
            ToolSubset::Only(vec!["read_file".to_owned()]),
            TokenBudget(4_000),
            ModelStepOutputKind::PatchIntent,
        )
        .encode()
        .expect("well-formed configuration encodes"),
    );
    let plan = build_plan(&catalogue, target_contract(), expression, configurations);

    let graph = materialize(
        &plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("plan 2 materialises");
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

    let mut context = SystemContext::new();
    context.insert(workspace.clone());
    context.insert(TokenMeter::default());
    context.insert(ProjectTestRunner::command(
        "sh",
        ["-c", "grep -q '1 + 1' lib.rs"],
    ));
    context.insert(ModelStepHandoff::default());
    context.insert(ModelStepProvider::new(ScriptedModelStep::new(
        ModelStepArtifact::PatchIntent {
            edits: vec![ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit")],
            check: None,
        },
    )));

    let execution = GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .expect("plan 2 executes");
    let conclusion = execution
        .output::<AssessmentConclusion>()
        .expect("concludes the coding contract");
    assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    assert!(
        workspace
            .read("lib.rs")
            .expect("read back")
            .contains("1 + 1"),
        "the repair arm's proposed patch must have landed"
    );
}

/// Plan 3: a read-only question —
/// `Seq(survey-tree, assess-findings, answer-question)`, writing nothing.
#[tokio::test]
async fn plan_three_read_only_question_materialises_and_validates() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
    let task = CodingTask::from_statement(
        "gate-plan-question",
        dir.path(),
        "where does todo!() occur",
        CodingIntent::Answer,
        SuccessCheck::stated_edits(),
    )
    .expect("well-formed question task")
    .with_question("where is todo!()");
    let workspace = CodingWorkspace::for_task(&task);

    let config = no_config_id();
    let expression = GateExpr::Seq(vec![
        step("survey", existing::survey_tree_key(), config.clone()),
        step("assess", existing::assess_findings_key(), config.clone()),
        step("answer", existing::answer_question_key(), config.clone()),
    ]);
    let plan = build_plan(
        &catalogue,
        target_contract(),
        expression,
        BTreeMap::from([(config, no_config_ref())]),
    );

    let graph = materialize(
        &plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("plan 3 materialises");
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

    let mut context = SystemContext::new();
    context.insert(workspace.clone());
    context.insert(CodingQuestion::locate("todo!()"));
    let execution = GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .expect("plan 3 executes");
    let conclusion = execution
        .output::<AssessmentConclusion>()
        .expect("concludes the coding contract");
    assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    // Writing nothing is the whole point of the read-only shape.
    assert_eq!(workspace.read("lib.rs").expect("read back"), "todo!()");
}

/// The three plans above are genuinely different graphs, not the same shape
/// relabelled: their derived signatures differ.
#[test]
fn the_three_plans_derive_different_signatures() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let limits = CompositionLimits::conservative();

    let edit_config = no_config_id();
    let edit_expression = GateExpr::Seq(vec![
        step("apply", existing::apply_direct_key(), edit_config.clone()),
        step(
            "judge",
            existing::judge_stated_edits_key(),
            edit_config.clone(),
        ),
        step(
            "conclude",
            existing::conclude_single_pass_key(),
            edit_config.clone(),
        ),
    ]);
    let edit_plan = build_plan(
        &catalogue,
        target_contract(),
        edit_expression,
        BTreeMap::from([(edit_config, no_config_ref())]),
    );
    let edit_graph = materialize(&edit_plan, &catalogue, &limits, &contracts)
        .expect("the stated-edit plan materialises");

    let question_config = no_config_id();
    let question_expression = GateExpr::Seq(vec![
        step(
            "survey",
            existing::survey_tree_key(),
            question_config.clone(),
        ),
        step(
            "assess",
            existing::assess_findings_key(),
            question_config.clone(),
        ),
        step(
            "answer",
            existing::answer_question_key(),
            question_config.clone(),
        ),
    ]);
    let question_plan = build_plan(
        &catalogue,
        target_contract(),
        question_expression,
        BTreeMap::from([(question_config, no_config_ref())]),
    );
    let question_graph = materialize(&question_plan, &catalogue, &limits, &contracts)
        .expect("the read-only question plan materialises");

    assert_ne!(
        edit_graph.signature(),
        question_graph.signature(),
        "a stated-edit plan and a read-only question plan must derive different signatures"
    );
}

/// A non-repeat-safe patch application is legal in a `Repeat` only when the
/// current iteration explicitly supplies a newly produced patch intent.
#[test]
fn repeat_repair_accepts_the_fresh_patch_intent_wire_and_refuses_its_absence() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let no_config = no_config_id();
    let model_config = ConfigId::new("repair-proposal").expect("config label is valid");
    let body = GateExpr::Seq(vec![
        step(
            "propose",
            configurable::model_propose_key(),
            model_config.clone(),
        ),
        step(
            "apply",
            configurable::apply_patch_intent_key(),
            no_config.clone(),
        ),
        step("test", existing::run_project_tests_key(), no_config.clone()),
        step(
            "conclude",
            existing::conclude_single_pass_key(),
            no_config.clone(),
        ),
    ]);
    let expression = GateExpr::Repeat {
        body: Box::new(body),
        control: LoopControl::Until(predicates::tests_passed_key()),
        retention: keep_last_iteration_key(),
    };
    let configurations = BTreeMap::from([
        (no_config, no_config_ref()),
        (
            model_config,
            ModelStepConfig::new(
                "repair".to_owned(),
                "propose one bounded repair patch".to_owned(),
                ToolSubset::Only(vec!["read_file".to_owned()]),
                TokenBudget(4_000),
                ModelStepOutputKind::PatchIntent,
            )
            .encode()
            .expect("well-formed model configuration"),
        ),
    ]);
    let wire = WirePlan::new(
        BlockInstanceId::new("apply").expect("instance label is valid"),
        BlockInstanceId::new("propose").expect("instance label is valid"),
        configurable::patch_intent_port_key(),
    );

    let wired = build_plan_with_wires(
        &catalogue,
        target_contract(),
        expression.clone(),
        vec![wire],
        configurations.clone(),
    );
    let graph = materialize(
        &wired,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a same-iteration patch-intent wire admits the repeat repair");
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

    let unwired = build_plan(&catalogue, target_contract(), expression, configurations);
    let refusals = materialize(
        &unwired,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a non-repeat-safe applier without a fresh input must be refused");
    assert!(refusals.iter().any(|refusal| {
        refusal.code() == polaris_cosmos::composition::refusal::codes::EFFECTS_VIOLATION
    }));
}

/// Patch application accepts only the dedicated patch-intent output, so every
/// other model output is refused while materialising the plan.
#[test]
fn patch_port_refuses_every_non_patch_model_output() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    for (label, output) in [
        ("plan", ModelStepOutputKind::Plan),
        ("findings", ModelStepOutputKind::Findings),
        ("review", ModelStepOutputKind::Review),
        ("answer", ModelStepOutputKind::Answer),
    ] {
        let no_config = no_config_id();
        let proposal = ConfigId::new(format!("{label}-proposal"))
            .expect("config label is a valid stable name");
        let plan = build_plan_with_wires(
            &catalogue,
            target_contract(),
            GateExpr::Seq(vec![
                step(
                    "propose",
                    configurable::model_propose_key(),
                    proposal.clone(),
                ),
                step(
                    "apply",
                    configurable::apply_patch_intent_key(),
                    no_config.clone(),
                ),
            ]),
            vec![WirePlan::new(
                BlockInstanceId::new("apply").expect("instance label is valid"),
                BlockInstanceId::new("propose").expect("instance label is valid"),
                configurable::patch_intent_port_key(),
            )],
            BTreeMap::from([
                (no_config, no_config_ref()),
                (
                    proposal,
                    ModelStepConfig::new(
                        label.to_owned(),
                        "produce the configured artifact".to_owned(),
                        ToolSubset::None,
                        TokenBudget(500),
                        output,
                    )
                    .encode()
                    .expect("well-formed model configuration"),
                ),
            ]),
        );
        let refusals = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect_err("a non-patch producer must not satisfy the patch port");
        assert!(refusals.iter().any(|refusal| {
            refusal.code() == polaris_cosmos::composition::refusal::codes::NATIVE_VALIDATION_ERROR
        }));
    }
}

/// A patch source outside a repeat body is stale for later iterations even
/// though its Rust output type is compatible with the patch port.
#[test]
fn repeat_repair_refuses_a_cross_repeat_patch_wire() {
    let catalogue = coding_block_catalogue();
    let no_config = no_config_id();
    let proposal = ConfigId::new("outer-proposal").expect("config label is a valid stable name");
    let expression = GateExpr::Seq(vec![
        step(
            "outside-propose",
            configurable::model_propose_key(),
            proposal.clone(),
        ),
        GateExpr::Repeat {
            body: Box::new(GateExpr::Seq(vec![
                step(
                    "apply",
                    configurable::apply_patch_intent_key(),
                    no_config.clone(),
                ),
                step("test", existing::run_project_tests_key(), no_config.clone()),
                step(
                    "conclude",
                    existing::conclude_single_pass_key(),
                    no_config.clone(),
                ),
            ])),
            control: LoopControl::Until(predicates::tests_passed_key()),
            retention: keep_last_iteration_key(),
        },
    ]);
    let error = GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        target_contract(),
        expression,
        vec![WirePlan::new(
            BlockInstanceId::new("apply").expect("instance label is valid"),
            BlockInstanceId::new("outside-propose").expect("instance label is valid"),
            configurable::patch_intent_port_key(),
        )],
        BTreeMap::from([
            (no_config, no_config_ref()),
            (
                proposal,
                ModelStepConfig::new(
                    "outside".to_owned(),
                    "produce one patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed model configuration"),
            ),
        ]),
    )
    .expect_err("a repeat body cannot receive a wire from outside its body");
    assert!(error.to_string().contains("wire-into-repeat-body"));
}

struct RolePatchModel;

impl ModelStepExecutor for RolePatchModel {
    fn propose<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        _workspace: &'a WorkspaceView<'a>,
        _meter: &'a TokenMeter,
    ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>> {
        let edit = match config.role.as_str() {
            "outside" => ExactEdit::new("lib.rs", "outside-only", "incorrect output"),
            "repeat" => ExactEdit::new("lib.rs", "todo!()", "1 + 1"),
            role => panic!("unexpected model role {role}"),
        }
        .expect("well-formed edit");
        Box::pin(async move {
            Ok(ModelStepArtifact::PatchIntent {
                edits: vec![edit],
                check: None,
            })
        })
    }
}

/// A bounded repeat executes using its explicit same-body producer even when
/// a compatible patch artifact is already available outside the repeat.
#[tokio::test]
async fn wired_repeat_reads_its_same_body_patch_producer() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let dir = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(dir.path().join("lib.rs"), "todo!()\n").expect("fixture");
    let workspace = edit_workspace(dir.path(), "todo!()", "1 + 1");
    let no_config = no_config_id();
    let outside = ConfigId::new("outside-proposal").expect("config label is valid");
    let repeat = ConfigId::new("repeat-proposal").expect("config label is valid");
    let expression = GateExpr::Seq(vec![
        step(
            "outside-propose",
            configurable::model_propose_key(),
            outside.clone(),
        ),
        GateExpr::Repeat {
            body: Box::new(GateExpr::Seq(vec![
                step("propose", configurable::model_propose_key(), repeat.clone()),
                step(
                    "apply",
                    configurable::apply_patch_intent_key(),
                    no_config.clone(),
                ),
                step("test", existing::run_project_tests_key(), no_config.clone()),
                step(
                    "conclude",
                    existing::conclude_single_pass_key(),
                    no_config.clone(),
                ),
            ])),
            control: LoopControl::Until(predicates::tests_passed_key()),
            retention: keep_last_iteration_key(),
        },
    ]);
    let plan = build_plan_with_wires(
        &catalogue,
        target_contract(),
        expression,
        vec![WirePlan::new(
            BlockInstanceId::new("apply").expect("instance label is valid"),
            BlockInstanceId::new("propose").expect("instance label is valid"),
            configurable::patch_intent_port_key(),
        )],
        BTreeMap::from([
            (no_config, no_config_ref()),
            (
                outside,
                ModelStepConfig::new(
                    "outside".to_owned(),
                    "propose an unused patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed model configuration"),
            ),
            (
                repeat,
                ModelStepConfig::new(
                    "repeat".to_owned(),
                    "propose the repair patch".to_owned(),
                    ToolSubset::None,
                    TokenBudget(500),
                    ModelStepOutputKind::PatchIntent,
                )
                .encode()
                .expect("well-formed model configuration"),
            ),
        ]),
    );
    let graph = materialize(
        &plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("wired repeat materialises");
    let mut context = SystemContext::new();
    context.insert(workspace.clone());
    context.insert(ProjectTestRunner::command(
        "sh",
        ["-c", "grep -q '1 + 1' lib.rs"],
    ));
    context.insert(TokenMeter::default());
    context.insert(ModelStepHandoff::default());
    context.insert(ModelStepProvider::new(RolePatchModel));
    GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .expect("the bounded repeat executes");
    assert_eq!(workspace.read("lib.rs").expect("read back"), "1 + 1\n");
}

/// A marker audit is a read-only task-specific graph, rather than a fixed
/// question shape with a closed marker vocabulary.
#[tokio::test]
async fn marker_survey_plan_materialises_validates_and_answers() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let dir = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(dir.path().join("alpha.rs"), "// TODO: alpha\n").expect("fixture");
    std::fs::write(dir.path().join("beta.rs"), "// TODO: beta\n").expect("fixture");
    let task = CodingTask::from_statement(
        "marker-audit-plan",
        dir.path(),
        "audit TODO markers",
        CodingIntent::Answer,
        SuccessCheck::stated_edits(),
    )
    .expect("task");
    let workspace = CodingWorkspace::for_task(&task);
    let no_config = no_config_id();
    let scan_config = ConfigId::new("marker-scan").expect("config label is valid");
    let scan = ConfigurationValue::map(vec![
        polaris_cosmos::contracts::ConfigurationEntry::new(
            "marker",
            ConfigurationValue::String("TODO".to_owned()),
        )
        .expect("entry"),
        polaris_cosmos::contracts::ConfigurationEntry::new(
            "max_sites",
            ConfigurationValue::Unsigned(8),
        )
        .expect("entry"),
        polaris_cosmos::contracts::ConfigurationEntry::new("root", ConfigurationValue::Bool(true))
            .expect("entry"),
    ])
    .expect("configuration map");
    let plan = build_plan(
        &catalogue,
        target_contract(),
        GateExpr::Seq(vec![
            step("scan", survey::scan_markers_key(), scan_config.clone()),
            step("assess", survey::assess_sites_key(), no_config.clone()),
            step("answer", survey::answer_site_audit_key(), no_config.clone()),
        ]),
        BTreeMap::from([
            (no_config, no_config_ref()),
            (
                scan_config,
                ResolvedConfigurationRef::inline(survey::scan_markers_schema(), scan)
                    .expect("resolved scan configuration"),
            ),
        ]),
    );
    let graph = materialize(
        &plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("marker survey materialises");
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    let mut context = SystemContext::new();
    context.insert(workspace);
    let result = GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .expect("marker survey executes");
    assert_eq!(
        result
            .output::<AssessmentConclusion>()
            .expect("marker survey concludes")
            .conclusion_kind(),
        polaris_cosmos::contracts::ConclusionKind::Unresolved
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("alpha.rs")).expect("read"),
        "// TODO: alpha\n"
    );
}

/// The task disposition is a real discriminator output a `Switch` can route,
/// and an ambiguous request reaches the clarification conclusion without a
/// mutation block.
#[tokio::test]
async fn clarification_disposition_switch_materialises_and_answers() {
    let catalogue = coding_block_catalogue();
    let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
    let dir = tempfile::tempdir().expect("temporary workspace");
    let task = CodingTask::from_statement(
        "clarification-plan",
        dir.path(),
        "make it better",
        CodingIntent::Careful,
        SuccessCheck::stated_edits(),
    )
    .expect("task");
    let classifier = ConfigId::new("dispositions").expect("config label is valid");
    let clarification = ConfigId::new("clarification-limit").expect("config label is valid");
    let case = |key: &str, instance: &str| {
        SwitchCase::new(
            CaseKey::new(key).expect("case key is valid"),
            step(
                instance,
                clarification::request_clarification_key(),
                clarification.clone(),
            ),
        )
    };
    let plan = build_plan(
        &catalogue,
        target_contract(),
        GateExpr::Seq(vec![
            step(
                "classify",
                clarification::classify_request_key(),
                classifier.clone(),
            ),
            GateExpr::Switch {
                discriminator: clarification::task_disposition_key(),
                cases: vec![
                    case("edit", "clarify-edit"),
                    case("answer", "clarify-answer"),
                    case("clarify", "clarify-request"),
                ],
                default: None,
            },
        ]),
        BTreeMap::from([
            (
                classifier,
                ResolvedConfigurationRef::inline(
                    clarification::classify_request_schema(),
                    ConfigurationValue::map(vec![
                        polaris_cosmos::contracts::ConfigurationEntry::new(
                            "allowed_dispositions",
                            ConfigurationValue::List(vec![
                                ConfigurationValue::String("edit".to_owned()),
                                ConfigurationValue::String("answer".to_owned()),
                                ConfigurationValue::String("clarify".to_owned()),
                            ]),
                        )
                        .expect("entry"),
                    ])
                    .expect("configuration map"),
                )
                .expect("resolved configuration"),
            ),
            (
                clarification,
                ResolvedConfigurationRef::inline(
                    clarification::request_clarification_schema(),
                    ConfigurationValue::map(vec![
                        polaris_cosmos::contracts::ConfigurationEntry::new(
                            "max_questions",
                            ConfigurationValue::Unsigned(1),
                        )
                        .expect("entry"),
                    ])
                    .expect("configuration map"),
                )
                .expect("resolved configuration"),
            ),
        ]),
    );
    let graph = materialize(
        &plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("clarification switch materialises");
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    let mut context = SystemContext::new();
    context.insert(task);
    let result = GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .expect("clarification graph executes");
    assert_eq!(
        result
            .output::<AssessmentConclusion>()
            .expect("clarification concludes")
            .stop_reason(),
        StopReason::EscalatedOut
    );
}
