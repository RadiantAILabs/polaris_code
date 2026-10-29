//! Verification of the answer-shaped coding registration.
//!
//! The coding pack's other registrations all *edit*. This file pins the one
//! that does not: `code.tree-question` answers a bounded class of question
//! about the working tree, terminates in the ordinary coding contract, and
//! writes nothing.
//!
//! Four properties are checked, each of which was observed failing before the
//! registration existed:
//!
//! 1. it is mechanically admissible on a host with no test runner, no model
//!    provider and nothing else installed;
//! 2. the deterministic selector returns it — and returns it *first* — under an
//!    objective that forbids source mutation;
//! 3. running it over a real tree produces a terminal conclusion and leaves
//!    every byte of that tree unchanged; and
//! 4. a question it cannot serve terminates *distinguishably* from one it can,
//!    so a router can fall back instead of presenting a shrug as an answer.
//!
//! A fifth test guards the six edit- and audit-shaped registrations that were
//! here first: same admitted set, same exclusion channels, same winners.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use polaris::graph::GraphExecutor;
use polaris::system::param::SystemContext;
use polaris::system::server::Server;
use polaris_code::cosmos::graphs;
use polaris_code::cosmos::payload::{CodingQuestion, CodingWorkspace, SweepWorklist};
use polaris_code::cosmos::policy;
use polaris_code::cosmos::task::{CodingIntent, CodingObjective, CodingTask};
use polaris_code::cosmos::topology::{
    self, COVERAGE_AUDIT, CodingTopologyPackPlugin, DIRECT, HostCapabilities, MODEL_PROPOSED,
    PREDICTION_ERROR, TEST_GATED, TREE_QUESTION, WORKLIST_SWEEP,
};
use polaris_cosmos::contracts::{
    AssessmentConclusion, ConclusionKind, PolicyEligibility, RuntimeAvailabilitySnapshot,
    SelectionOutcome, Status, StopReason,
};
use polaris_cosmos::evidence::{BoundedInMemoryEvidence, TopologyEvidence};
use polaris_cosmos::selection::deterministic::{self, SelectionInputs};
use polaris_cosmos::topology::{
    CandidateAdmissionSet, RegistrationId, TopologyCatalog, TopologyCatalogPlugin,
    TopologyCatalogSnapshot, TopologyContract, preflight_catalog,
};

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

const README: &str = "# fixture\n\nNothing to see.\n";

const ADD_STUB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}";
const ADD_DONE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";

fn project() -> (tempfile::TempDir, CodingTask) {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(dir.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
    std::fs::write(dir.path().join("README.md"), README).expect("seed README.md");
    let task = CodingTask::new(
        "answer-shape",
        dir.path(),
        "src/lib.rs",
        ADD_STUB,
        ADD_DONE,
        "todo!()",
        CodingIntent::Answer,
    )
    .expect("the coding task is well formed");
    (dir, task)
}

/// Every file under `root`, sorted, with its exact bytes.
///
/// Comparing this before and after a run is what makes "writes nothing" a
/// measured claim about the whole tree rather than about one watched file.
fn tree_bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        snapshot.insert(
            relative,
            std::fs::read(entry.path()).expect("fixture file is readable"),
        );
    }
    snapshot
}

async fn coding_pack_server() -> Server {
    let mut server = Server::new();
    server
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyCatalogPlugin);
    server.finish().await.expect("the coding pack assembles");
    server
}

fn coding_contract(snapshot: &TopologyCatalogSnapshot) -> TopologyContract {
    snapshot
        .contracts()
        .iter()
        .find(|contract| contract.key().id().as_str() == topology::CONTRACT)
        .cloned()
        .expect("the coding pack registered its contract")
}

fn bare_host(task: &CodingTask) -> RuntimeAvailabilitySnapshot {
    let snapshot_id = "c4d5e6f7-0000-4000-8000-000000000001"
        .parse()
        .expect("availability snapshot ID is a UUIDv4");
    // A machine with nothing extra installed: no project test runner, no
    // configured model provider.
    topology::probe_availability(
        task,
        snapshot_id,
        1_700_000_000_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host availability snapshot builds")
}

fn preflight(
    snapshot: &TopologyCatalogSnapshot,
    contract: &TopologyContract,
    availability: &RuntimeAvailabilitySnapshot,
) -> CandidateAdmissionSet {
    preflight_catalog(snapshot, contract.key(), availability, false)
        .expect("mechanical preflight completes")
}

fn exclusion_codes(admission: &CandidateAdmissionSet) -> BTreeMap<String, Vec<String>> {
    admission
        .evaluations()
        .iter()
        .map(|evaluation| {
            (
                evaluation.candidate().registration_id().as_str().to_owned(),
                evaluation
                    .result()
                    .reasons()
                    .iter()
                    .map(|reason| reason.code().as_str().to_owned())
                    .collect(),
            )
        })
        .collect()
}

/// One selection under one coding intent, over a frozen catalog and host.
fn select(
    task: &CodingTask,
    snapshot: &TopologyCatalogSnapshot,
    intent: CodingIntent,
    decision_suffix: &str,
) -> polaris_cosmos::contracts::SelectionDecision {
    let contract = coding_contract(snapshot);
    let availability = bare_host(task);
    let admission = preflight(snapshot, &contract, &availability);
    let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(
        policy::evidence_config(),
    )));
    let statistics = evidence
        .capture_statistics(&policy::statistics_request())
        .expect("empty statistics capture");
    let decision_id = format!("d5e6f7a8-0000-4000-8000-0000000000{decision_suffix}")
        .parse()
        .expect("decision ID is a UUIDv4");
    let inputs = SelectionInputs::new(
        decision_id,
        task.profile().expect("the profile aggregates"),
        CodingObjective::new(intent)
            .build()
            .expect("the objective builds"),
        contract.key().clone(),
        snapshot.clone(),
        availability,
        admission,
        statistics,
        policy::candidate_facts(snapshot, None).expect("selector facts build"),
        policy::selector_config().expect("the selector configures"),
    );
    deterministic::select(&inputs).expect("selection completes")
}

fn winner(decision: &polaris_cosmos::contracts::SelectionDecision) -> String {
    match decision.outcome() {
        SelectionOutcome::Selected { selected, .. } => {
            selected.registration_id().as_str().to_owned()
        }
        SelectionOutcome::NoEligibleCandidate { diagnostics } => {
            panic!("no eligible candidate: {diagnostics:?}")
        }
    }
}

/// Runs the *registered* answer graph — the one in the catalog, not a
/// hand-built copy — over a prepared live context.
async fn run_registered_question(
    workspace: CodingWorkspace,
    question: CodingQuestion,
) -> AssessmentConclusion {
    let server = coding_pack_server().await;
    let registered = {
        let catalog = server
            .get_global::<TopologyCatalog>()
            .expect("the provider globalizes the catalog");
        let snapshot = catalog.snapshot().expect("the catalog is sealed");
        catalog
            .resolve_at(
                &RegistrationId::new(TREE_QUESTION).expect("registration ID is a stable name"),
                snapshot.generation(),
            )
            .expect("the answer registration resolves")
    };
    let mut context = SystemContext::new();
    context.insert(workspace);
    context.insert(question);
    context.insert(SweepWorklist::new(graphs::SWEEP_BUDGET));
    let execution = GraphExecutor::new()
        .execute(registered.graph(), &mut context, None, None)
        .await
        .expect("the answer graph runs");
    execution
        .output::<AssessmentConclusion>()
        .expect("the answer graph produces a conclusion")
        .clone()
}

fn workspace_for(task: &CodingTask) -> CodingWorkspace {
    CodingWorkspace::for_task(task)
}

// ---------------------------------------------------------------------------
// 1. Admissible on an ordinary machine with nothing installed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_answer_shape_is_admissible_with_no_capability_installed() {
    let (_dir, task) = project();
    let server = coding_pack_server().await;
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed");

    assert!(
        snapshot
            .candidates()
            .iter()
            .any(|candidate| candidate.registration_id().as_str() == TREE_QUESTION),
        "the coding pack registers no answer-shaped topology"
    );

    let contract = coding_contract(&snapshot);
    let availability = bare_host(&task);
    let admission = preflight(&snapshot, &contract, &availability);
    let codes = exclusion_codes(&admission);
    assert_eq!(
        codes.get(TREE_QUESTION).map(Vec::as_slice),
        Some(&[][..]),
        "the answer shape was mechanically excluded: {:?}",
        codes.get(TREE_QUESTION)
    );
}

// ---------------------------------------------------------------------------
// 2. Selectable — a correct selector has somewhere to route a question.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_answer_shape_wins_an_objective_that_forbids_source_mutation() {
    let (_dir, task) = project();
    let server = coding_pack_server().await;
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed");

    let decision = select(&task, &snapshot, CodingIntent::Answer, "01");
    assert_eq!(winner(&decision), TREE_QUESTION);

    // Every mutating registration that got as far as the policy channel is
    // refused there by name, rather than being out-scored. `code.test-gated-edit`
    // and `code.model-proposed-edit` are absent because mechanical admission
    // already excluded them on this host, which is the earlier gate.
    let mut forbidden = Vec::new();
    for consideration in decision.candidates() {
        if let PolicyEligibility::Excluded { reasons } = consideration.policy_eligibility()
            && reasons
                .iter()
                .any(|reason| reason.code().as_str() == "capability.forbidden")
        {
            forbidden.push(
                consideration
                    .candidate()
                    .registration_id()
                    .as_str()
                    .to_owned(),
            );
        }
    }
    forbidden.sort();
    assert_eq!(
        forbidden,
        vec![
            DIRECT.to_owned(),
            PREDICTION_ERROR.to_owned(),
            WORKLIST_SWEEP.to_owned(),
        ]
    );
    // The audit shape is not forbidden — it writes nothing either — so it is
    // eligible and simply loses on score. That the answer shape beats it is a
    // ranking claim, and the assertion above is what makes it one.
    assert!(decision.candidates().iter().any(|consideration| {
        consideration.candidate().registration_id().as_str() == COVERAGE_AUDIT
            && matches!(
                consideration.policy_eligibility(),
                PolicyEligibility::Eligible
            )
    }));
}

// ---------------------------------------------------------------------------
// 3. It terminates, and the tree is byte-identical afterwards.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn answering_a_question_leaves_the_working_tree_byte_identical() {
    let (dir, task) = project();
    let before = tree_bytes(dir.path());

    let conclusion =
        run_registered_question(workspace_for(&task), CodingQuestion::locate("fn sub")).await;

    assert_eq!(conclusion.conclusion_kind(), ConclusionKind::Resolved);
    assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    assert_eq!(conclusion.summary().counts_by_status().pass(), 1);
    assert!(
        conclusion.accounting().is_empty(),
        "an answered question leaves no residual to account"
    );

    assert_eq!(
        tree_bytes(dir.path()),
        before,
        "the answer topology modified the working tree"
    );
}

#[tokio::test]
async fn a_question_with_no_matches_is_still_a_well_founded_answer() {
    let (dir, task) = project();
    let before = tree_bytes(dir.path());

    let conclusion =
        run_registered_question(workspace_for(&task), CodingQuestion::locate("fn multiply")).await;

    // "Nothing in this tree matches" is knowledge, not a failure.
    assert_eq!(conclusion.conclusion_kind(), ConclusionKind::Resolved);
    assert_eq!(conclusion.summary().counts_by_status().pass(), 1);
    assert_eq!(tree_bytes(dir.path()), before);
}

#[tokio::test]
async fn a_file_inspection_answers_from_the_tree_without_writing() {
    let (dir, task) = project();
    let before = tree_bytes(dir.path());

    let conclusion =
        run_registered_question(workspace_for(&task), CodingQuestion::inspect("src/lib.rs")).await;

    assert_eq!(conclusion.conclusion_kind(), ConclusionKind::Resolved);
    assert_eq!(tree_bytes(dir.path()), before);
}

// ---------------------------------------------------------------------------
// 4. Honest failure: a shrug must not look like an answer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_question_it_cannot_serve_terminates_distinguishably() {
    let (dir, task) = project();
    let before = tree_bytes(dir.path());

    let answered =
        run_registered_question(workspace_for(&task), CodingQuestion::locate("fn add")).await;
    let refused = run_registered_question(
        workspace_for(&task),
        CodingQuestion::parse("why is this crate structured the way it is?"),
    )
    .await;

    // The two terminate in the *same* vocabulary and are still distinguishable
    // by kind, which is what lets a router fall back rather than report a shrug.
    assert_eq!(answered.conclusion_kind(), ConclusionKind::Resolved);
    assert_eq!(refused.conclusion_kind(), ConclusionKind::Escalated);
    assert_eq!(refused.stop_reason(), StopReason::EscalatedOut);
    assert_eq!(refused.summary().counts_by_status().pass(), 0);
    assert_eq!(refused.summary().counts_by_status().indeterminate(), 1);
    assert_eq!(refused.accounting().len(), 1);

    assert_eq!(tree_bytes(dir.path()), before);
}

#[tokio::test]
async fn an_unreadable_path_escalates_rather_than_claiming_an_answer() {
    let (_dir, task) = project();
    let escaping = run_registered_question(
        workspace_for(&task),
        CodingQuestion::inspect("../outside.rs"),
    )
    .await;
    assert_eq!(escaping.conclusion_kind(), ConclusionKind::Escalated);

    let absent = run_registered_question(
        workspace_for(&task),
        CodingQuestion::inspect("src/missing.rs"),
    )
    .await;
    assert_eq!(absent.conclusion_kind(), ConclusionKind::Escalated);
}

#[test]
fn the_recognized_question_shapes_are_exactly_the_ones_it_can_serve() {
    // The typed constructors are the API; `parse` is a documented convenience
    // over a closed set of phrasings, and everything else is refused up front.
    assert!(CodingQuestion::parse("where is fn add?").is_answerable());
    assert!(CodingQuestion::parse("which files contain todo!()").is_answerable());
    assert!(CodingQuestion::parse("what is in src/lib.rs?").is_answerable());
    assert!(!CodingQuestion::parse("should I refactor this module?").is_answerable());
    assert!(!CodingQuestion::parse("").is_answerable());
    assert!(!CodingQuestion::none().is_answerable());
    assert!(CodingQuestion::locate("todo!()").is_answerable());
    assert!(CodingQuestion::inspect("src/lib.rs").is_answerable());
}

// ---------------------------------------------------------------------------
// 5. What the shape actually buys, measured.
// ---------------------------------------------------------------------------

/// Answers a real question about *this* crate's own tree, and times it.
///
/// The fixture tree above is three files, which measures nothing interesting.
/// This runs the registered graph over `crates/`, a few hundred real source
/// files, so the measurement is about a repository.
///
/// The ceiling is deliberately loose. The claim being pinned is an order of
/// magnitude — that answering is bounded by a filesystem scan and not by a
/// model round trip — not a specific millisecond count on specific hardware.
#[tokio::test]
async fn answering_a_question_about_this_repository_costs_a_tree_scan() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let task = CodingTask::new(
        "measure-answer",
        root,
        "src/lib.rs",
        ADD_STUB,
        ADD_DONE,
        "todo!()",
        CodingIntent::Answer,
    )
    .expect("the coding task is well formed");

    let started = std::time::Instant::now();
    let conclusion = run_registered_question(
        workspace_for(&task),
        CodingQuestion::locate("pub fn tree_question_graph"),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(conclusion.conclusion_kind(), ConclusionKind::Resolved);
    assert!(
        conclusion.summary().counts_by_status().pass() >= 1,
        "the question about this repository found nothing"
    );
    eprintln!(
        "code.tree-question answered `where does pub fn tree_question_graph occur` \
         over {} in {elapsed:?}",
        root.display()
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "answering took {elapsed:?}, which is model-round-trip territory rather \
         than tree-scan territory"
    );
}

// ---------------------------------------------------------------------------
// 6. The registrations that were here first are untouched.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_edit_shaped_registrations_admit_and_rank_exactly_as_before() {
    let (_dir, task) = project();
    let server = coding_pack_server().await;
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed");

    let contract = coding_contract(&snapshot);
    let availability = bare_host(&task);
    let admission = preflight(&snapshot, &contract, &availability);
    let codes = exclusion_codes(&admission);

    for admitted in [COVERAGE_AUDIT, DIRECT, PREDICTION_ERROR, WORKLIST_SWEEP] {
        assert_eq!(
            codes[admitted],
            Vec::<String>::new(),
            "{admitted} regressed"
        );
    }
    assert_eq!(codes[TEST_GATED], vec!["resource.missing".to_owned()]);
    assert_eq!(
        codes[MODEL_PROPOSED],
        vec!["provider.unavailable".to_owned()]
    );

    // Adding an answer shape must not steal an edit objective from the edit
    // shape that already won it.
    assert_eq!(
        winner(&select(&task, &snapshot, CodingIntent::Fast, "02")),
        DIRECT
    );
    assert_eq!(
        winner(&select(&task, &snapshot, CodingIntent::Careful, "03")),
        PREDICTION_ERROR
    );
    assert_eq!(
        winner(&select(&task, &snapshot, CodingIntent::Thorough, "04")),
        WORKLIST_SWEEP
    );
}

#[tokio::test]
async fn the_answer_shape_still_terminates_in_the_one_coding_contract() {
    let server = coding_pack_server().await;
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed");
    let contract = coding_contract(&snapshot);
    let candidate = snapshot
        .candidates()
        .iter()
        .find(|candidate| candidate.registration_id().as_str() == TREE_QUESTION)
        .expect("the answer registration is in the catalog");
    assert!(
        candidate.descriptor().contracts().contains(contract.key()),
        "the answer shape does not claim the coding contract"
    );
    assert!(
        candidate
            .signature()
            .satisfies(&polaris::graph::GraphSignature::new().produce::<AssessmentConclusion>()),
        "the answer graph does not produce the contract's terminal record"
    );
}

#[test]
fn a_question_finding_is_a_subject_in_its_own_right() {
    // Making "this needed no edits" sayable must not widen what an edit site
    // means: the answer shape brings its own subject schema.
    use polaris_code::cosmos::payload::QuestionFinding;
    let finding = QuestionFinding::new(
        "where is fn add".to_owned(),
        "src/lib.rs".to_owned(),
        4,
        "occurs at src/lib.rs:4".to_owned(),
        Status::Pass,
    );
    let assessment = polaris_code::cosmos::assess::finding_assessment(finding.clone())
        .expect("the finding validates against the question subject schema");
    assert_eq!(assessment.status(), Status::Pass);
    assert_eq!(assessment.subject(), &finding);
    assert_ne!(
        polaris_code::cosmos::assess::QUESTION_SUBJECT_SCHEMA,
        polaris_code::cosmos::assess::SUBJECT_SCHEMA
    );

    // And it is a schema, not a wrapper: a finding that names no question, or
    // states no claim, is rejected rather than recorded. An unattributed claim
    // is what would let a shrug be read back later as an answer.
    for blank in [
        QuestionFinding::new(
            "   ".to_owned(),
            "src/lib.rs".to_owned(),
            4,
            "occurs at src/lib.rs:4".to_owned(),
            Status::Pass,
        ),
        QuestionFinding::new(
            "where is fn add".to_owned(),
            "src/lib.rs".to_owned(),
            4,
            "  ".to_owned(),
            Status::Pass,
        ),
    ] {
        assert!(
            polaris_code::cosmos::assess::finding_assessment(blank.clone()).is_err(),
            "the question subject schema accepted {blank:?}"
        );
    }
}
