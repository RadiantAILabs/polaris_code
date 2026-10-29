//! Executed behaviour of each coding topology graph, pinned node by node.
//!
//! The end-to-end proof in `cosmos_contracts_proof.rs` only ever *runs* the
//! prediction-error loop; the other five registrations are admitted, ranked,
//! and then left alone. That is enough to check admission, and not enough to
//! check that a graph still does the same thing after its shape is rebuilt on
//! Cosmos's reusable blocks. So this file runs every graph directly against a
//! real temporary crate and pins what it observes: the bytes left on disk, the
//! role trace the workspace recorded, and the terminal conclusion.

use polaris::graph::{Graph, GraphExecutor};
use polaris::system::param::SystemContext;
use polaris_code_core::cosmos::graphs;
use polaris_code_core::cosmos::host::TopologyProduct;
use polaris_code_core::cosmos::payload::{
    CodingWorkspace, EditSite, ProjectTestRunner, SweepWorklist,
};
use polaris_code_core::cosmos::task::{CodingIntent, CodingTask, SuccessCheck};
use polaris_cosmos::cognition::approved::coverage_sweep;
use polaris_cosmos::contracts::{AssessmentConclusion, ConclusionKind, StopReason};

const ADD_STUB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}";
const ADD_DONE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";

const LIB_RS: &str = "\
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

/// The same crate with nothing left stubbed, so a scan finds no site.
const LIB_RS_COMPLETE: &str = "\
//! A tiny crate with two finished helpers.

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Subtracts two numbers.
pub fn sub(a: i32, b: i32) -> i32 {
    a - b
}
";

/// What one graph run left behind.
#[derive(Debug)]
struct Run {
    conclusion: AssessmentConclusion,
    roles: Vec<String>,
    target: String,
    nodes: usize,
}

/// Which host-supplied resources a graph run installs beyond the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provide {
    /// Only what the embodiment owns: the workspace and its sweep worklist.
    EmbodimentOnly,
    /// Also the project test runner the test-gated graph's judge declares.
    TestRunner,
    /// Also Cosmos's private coverage state, installed by Cosmos itself.
    CoverageState,
}

fn workspace(lib_rs: &str) -> (tempfile::TempDir, CodingWorkspace) {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(dir.path().join("src/lib.rs"), lib_rs).expect("seed lib.rs");
    let task = CodingTask::new(
        "implement-add",
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

/// Executes `graph` against a fresh copy of `lib_rs`, keeping any failure.
async fn execute(
    graph: &Graph,
    provide: Provide,
    lib_rs: &str,
) -> (tempfile::TempDir, Result<Run, String>) {
    let (dir, workspace) = workspace(lib_rs);
    let mut context = SystemContext::new();
    context.insert(workspace.clone());
    context.insert(SweepWorklist::new(graphs::SWEEP_BUDGET));
    match provide {
        Provide::EmbodimentOnly => {}
        Provide::TestRunner => {
            context.insert(ProjectTestRunner::command("true", Vec::<String>::new()));
        }
        Provide::CoverageState => coverage_sweep::prepare_context::<EditSite>(
            &mut context,
            coverage_sweep::Config::new(
                graphs::SWEEP_BUDGET,
                graphs::SWEEP_BUDGET * 2,
                graphs::SWEEP_BUDGET,
                StopReason::BudgetExhausted,
            )
            .expect("the coverage bounds are well formed"),
        )
        .expect("coverage state installs into a fresh context"),
    }
    let outcome = match GraphExecutor::new()
        .execute(graph, &mut context, None, None)
        .await
    {
        Ok(execution) => {
            let conclusion = execution
                .output::<AssessmentConclusion>()
                .expect("the graph produces the one contracted conclusion")
                .clone();
            Ok(Run {
                conclusion,
                roles: workspace.roles(),
                target: workspace
                    .read(workspace.subject_path())
                    .expect("the target file is readable"),
                nodes: execution.nodes_executed(),
            })
        }
        Err(error) => Err(error.to_string()),
    };
    (dir, outcome)
}

/// Executes `graph` against the stubbed sample crate and requires success.
async fn run(graph: &Graph, provide: Provide) -> (tempfile::TempDir, Run) {
    let (dir, outcome) = execute(graph, provide, LIB_RS).await;
    let run = outcome.expect("the coding graph executes");
    (dir, run)
}

#[tokio::test]
async fn the_direct_edit_graph_applies_and_judges_in_one_pass() {
    let (_dir, run) = run(&graphs::direct_edit_graph(), Provide::EmbodimentOnly).await;

    assert_eq!(run.roles, vec!["direct.act", "direct.verify"]);
    assert!(run.target.contains("a + b"));
    assert!(!run.target.contains(ADD_STUB));
    assert_eq!(run.conclusion.stop_reason(), StopReason::GoalMet);
    assert_eq!(run.conclusion.summary().counts_by_status().pass(), 1);
    assert_eq!(run.conclusion.summary().counts_by_status().issue(), 0);
    // A passing single pass leaves nothing to account for.
    assert!(run.conclusion.accounting().is_empty());
}

#[tokio::test]
async fn the_prediction_error_graph_falsifies_its_own_prediction() {
    let (_dir, run) = run(
        &graphs::prediction_error_edit_graph(),
        Provide::EmbodimentOnly,
    )
    .await;

    assert_eq!(
        run.roles,
        vec![
            "prediction.predict",
            "prediction.act",
            "prediction.verify",
            "prediction.compare",
            "prediction.accept",
        ]
    );
    assert!(run.target.contains("a + b"));
    assert_eq!(run.conclusion.stop_reason(), StopReason::PredicateSatisfied);
    assert_eq!(run.conclusion.summary().counts_by_status().pass(), 1);
}

#[tokio::test]
async fn the_worklist_sweep_graph_drains_every_stub_site() {
    let (_dir, run) = run(&graphs::worklist_sweep_graph(), Provide::EmbodimentOnly).await;

    // The sample crate has two stub sites, so the scope role runs once and the
    // probe role runs once per site.
    assert_eq!(run.roles, vec!["sweep.scope", "sweep.probe", "sweep.probe"]);
    assert!(run.target.contains("a + b"));
    // Work ran out before the budget did.
    assert_eq!(run.conclusion.stop_reason(), StopReason::WorkExhausted);
    let counts = run.conclusion.summary().counts_by_status();
    // Both scanned sites are in the target file, so both are judged by the
    // whole-file rule and both read Pass once the edit has landed. That is the
    // sweep's real behaviour, recorded rather than wished for.
    assert_eq!(counts.pass(), 2);
    assert_eq!(counts.issue(), 0);
    assert!(run.conclusion.accounting().is_empty());
}

#[tokio::test]
async fn the_test_gated_graph_runs_with_its_declared_runner() {
    let (_dir, run) = run(&graphs::test_gated_edit_graph(), Provide::TestRunner).await;

    assert_eq!(run.roles, vec!["test-gated.act"]);
    assert!(run.target.contains("a + b"));
    assert_eq!(run.conclusion.stop_reason(), StopReason::GoalMet);
    assert_eq!(run.conclusion.summary().counts_by_status().pass(), 1);
}

#[tokio::test]
async fn the_test_gated_graph_refuses_to_run_without_its_runner() {
    let (_dir, outcome) = execute(
        &graphs::test_gated_edit_graph(),
        Provide::EmbodimentOnly,
        LIB_RS,
    )
    .await;

    // The runner sits on the judge, so the graph still declares it and a
    // context without it cannot satisfy the oracle.
    let error = outcome.expect_err("a missing test runner must stop the graph");
    assert!(error.contains("ProjectTestRunner"), "error was {error}");
}

#[tokio::test]
async fn the_model_proposed_graph_matches_the_direct_shape() {
    let (_dir, proposed) = run(
        &graphs::model_proposed_edit_graph(),
        Provide::EmbodimentOnly,
    )
    .await;
    let (_direct_dir, direct) = run(&graphs::direct_edit_graph(), Provide::EmbodimentOnly).await;

    assert_eq!(proposed.roles, vec!["model-proposed.act"]);
    assert!(proposed.target.contains("a + b"));
    assert_eq!(proposed.conclusion.stop_reason(), StopReason::GoalMet);
    // Same shape means the same node count and the same verdict; only the role
    // trace and the declared provider differ. The count is compared rather
    // than pinned because the block rebuild deliberately split one fused
    // act-and-judge system into a producer and an oracle, taking both graphs
    // from one executed node to three while leaving every observed effect the
    // same.
    assert_eq!(proposed.nodes, direct.nodes);
    assert_eq!(
        proposed.conclusion.summary().counts_by_status(),
        direct.conclusion.summary().counts_by_status()
    );
}

#[tokio::test]
async fn the_coverage_audit_graph_resolves_an_empty_scope_as_a_clean_survey() {
    let (_dir, outcome) = execute(
        &graphs::coverage_audit_graph(),
        Provide::CoverageState,
        LIB_RS_COMPLETE,
    )
    .await;
    let run = outcome.expect("the coverage audit executes over an empty scope");

    assert_eq!(run.roles, vec!["coverage.scope"]);
    assert_eq!(run.conclusion.stop_reason(), StopReason::WorkExhausted);
    let counts = run.conclusion.summary().counts_by_status();
    // Nothing was stubbed, so the scan established that the survey has no
    // outstanding work. This is a pass, not a vacuous inapplicable verdict.
    assert_eq!(counts.pass(), 1);
    assert_eq!(counts.issue(), 0);
    assert!(run.conclusion.accounting().is_empty());
    assert_eq!(run.conclusion.conclusion_kind(), ConclusionKind::Resolved);
}

#[tokio::test]
async fn an_already_satisfied_stated_edit_resolves_without_writing() {
    let (_dir, outcome) = execute(
        &graphs::direct_edit_graph(),
        Provide::EmbodimentOnly,
        LIB_RS_COMPLETE,
    )
    .await;
    let run = outcome.expect("an already-satisfied edit is a valid no-op");

    assert_eq!(run.conclusion.conclusion_kind(), ConclusionKind::Resolved);
    assert_eq!(run.conclusion.summary().counts_by_status().pass(), 1);
    assert!(run.target.contains("a + b"));
}

#[tokio::test]
async fn a_noop_edit_graph_cannot_resolve_a_statement_only_task() {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    let source = LIB_RS_COMPLETE.to_owned();
    std::fs::write(dir.path().join("src/lib.rs"), &source).expect("seed lib.rs");
    let task = CodingTask::from_statement(
        "statement-needs-work",
        dir.path(),
        "repair the reported bug",
        CodingIntent::Careful,
        SuccessCheck::command("false", Vec::<String>::new())
            .expect("a concrete success check is valid"),
    )
    .expect("statement task is well formed");
    let workspace = CodingWorkspace::for_task(&task);
    let mut context = SystemContext::new();
    context.insert(workspace);
    context.insert(SweepWorklist::new(graphs::SWEEP_BUDGET));

    let execution = GraphExecutor::new()
        .execute(
            &graphs::prediction_error_edit_graph(),
            &mut context,
            None,
            None,
        )
        .await
        .expect("the no-op graph still executes");
    let conclusion = execution
        .output::<AssessmentConclusion>()
        .expect("the graph concludes");
    let answer = TopologyProduct::of(conclusion)
        .answer()
        .expect("the issue assessment is rendered");

    assert_eq!(
        conclusion.conclusion_kind(),
        ConclusionKind::Unresolved,
        "an edit graph that did no work cannot resolve a statement-only task"
    );
    assert_eq!(conclusion.summary().counts_by_status().issue(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read source"),
        source,
        "the graph wrote no bytes"
    );
    assert!(
        answer.starts_with("left the coding task unresolved"),
        "{answer}"
    );
    assert!(!answer.starts_with("resolved the coding task"), "{answer}");
}

#[tokio::test]
async fn the_coverage_audit_graph_accounts_for_every_residual_site() {
    let (_dir, outcome) = execute(
        &graphs::coverage_audit_graph(),
        Provide::CoverageState,
        LIB_RS,
    )
    .await;

    // The audit never writes, so every scanned site is judged `Issue` and
    // becomes a residual. Concluding over those residuals used to fail —
    // `cosmos::assess::conclude` built an `AssessmentAccounting` whose
    // provenance carried no evidence reference, which Cosmos rejects with
    // `assessment.accounting-missing-evidence`. `conclude` now cites the
    // residual-treatment decision, so the graph concludes.
    let run = outcome.expect("residual accounting is accepted");

    assert_eq!(run.conclusion.stop_reason(), StopReason::WorkExhausted);
    let counts = run.conclusion.summary().counts_by_status();
    assert_eq!(counts.pass(), 0);
    assert_eq!(counts.issue(), 2, "both stubbed helpers are residual");
    assert_eq!(
        run.conclusion.accounting().len(),
        2,
        "every residual site is accounted rather than dropped"
    );
    assert!(
        run.conclusion.accounting().iter().all(|entry| {
            entry
                .provenance()
                .iter()
                .any(|record| !record.evidence_refs().is_empty())
        }),
        "each accounting entry cites the decision it rests on"
    );
}
