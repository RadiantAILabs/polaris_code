//! Proof that topology selection ranks on recorded outcomes, not priors alone.
//!
//! Every test here fixes the catalog, the task profile, the availability
//! probe, the mechanical admission set and the author priors, and varies only
//! one thing: what the retained evidence says previous runs did. The selector
//! is `polaris_cosmos`'s own `deterministic::select`, unchanged — what this
//! crate changed is the facts it is handed.
//!
//! The synthetic snapshots are built field by field rather than captured from
//! a store, so a test can state one candidate's record exactly. The last two
//! tests close that gap from the other side: they take a snapshot a real run
//! actually produced and check the same lookup finds it.

use std::collections::BTreeMap;

use polaris::system::server::Server;
use polaris_code::cosmos::policy;
use polaris_code::cosmos::proof::{CodingProof, ProofEventIds};
use polaris_code::cosmos::task::{CodingIntent, CodingObjective, CodingTask};
use polaris_code::cosmos::topology::{
    self, CodingTopologyPackPlugin, DIRECT, HostCapabilities, PREDICTION_ERROR, WORKLIST_SWEEP,
};
use polaris_cosmos::contracts::{
    ContentDigest, LogicalSequence, Objective, RecordedAt, RuntimeAvailabilitySnapshot,
    ScoreSource, SelectionDecision, SelectionDecisionId, SelectionOutcome, SelectionRecord,
    StatisticsWindow, Status, TaskProfile,
};
use polaris_cosmos::evidence::{CohortKey, CohortStatistics, ProfileBucket, TopologyStatsSnapshot};
use polaris_cosmos::selection::deterministic::{self, SelectionInputs};
use polaris_cosmos::topology::{
    CandidateAdmissionSet, TopologyCatalog, TopologyCatalogPlugin, TopologyCatalogSnapshot,
    TopologyContract, preflight_catalog,
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

const ADD_STUB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}";
const ADD_DONE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";

fn uuid(suffix: &str) -> String {
    format!("c0c0c0c0-0000-4000-8000-0000000000{suffix}")
}

/// Everything a selection needs except the evidence, held fixed across tests.
struct Bench {
    _dir: tempfile::TempDir,
    snapshot: TopologyCatalogSnapshot,
    contract: TopologyContract,
    profile: TaskProfile,
    availability: RuntimeAvailabilitySnapshot,
    admission: CandidateAdmissionSet,
}

impl Bench {
    async fn new() -> Self {
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
            CodingIntent::Careful,
        )
        .expect("the coding task is well formed");

        let mut server = Server::new();
        server
            .add_plugins(TopologyCatalogPlugin)
            .add_plugins(CodingTopologyPackPlugin);
        server.finish().await.expect("the coding pack assembles");
        let snapshot = server
            .get_global::<TopologyCatalog>()
            .expect("the provider globalizes the catalog")
            .snapshot()
            .expect("the catalog is sealed");
        let contract = snapshot
            .contracts()
            .iter()
            .find(|contract| contract.key().id().as_str() == topology::CONTRACT)
            .cloned()
            .expect("the coding pack registered its contract");

        let profile = task.profile().expect("the profile aggregates");
        let availability = topology::probe_availability(
            &task,
            uuid("01").parse().expect("availability ID is a UUIDv4"),
            0,
            HostCapabilities::hermetic(),
        )
        .expect("the availability probe succeeds");
        let admission = preflight_catalog(&snapshot, contract.key(), &availability, false)
            .expect("mechanical preflight succeeds");

        Self {
            _dir: dir,
            snapshot,
            contract,
            profile,
            availability,
            admission,
        }
    }

    /// The cohort bucket this bench's task falls into.
    fn bucket(&self) -> ProfileBucket {
        let features = self.profile.features();
        ProfileBucket {
            decomposability: features.decomposability.known_value().copied(),
            verifier_availability: features
                .verifier
                .known_value()
                .map(polaris_cosmos::contracts::VerifierProfile::availability),
            horizon: features.horizon.known_value().copied(),
            consequence: features.consequence.known_value().copied(),
        }
    }

    /// A statistics snapshot recording `(passes, issues)` per registration.
    ///
    /// `label` only seeds the content digest, which the selector requires the
    /// score components to be bound to. Two records that differ must get
    /// different labels, exactly as two real captures get different digests.
    fn statistics(&self, label: &str, records: &[(&str, u64, u64)]) -> TopologyStatsSnapshot {
        let bucket = self.bucket();
        let cohorts = records
            .iter()
            .map(|(registration, passes, issues)| {
                let candidate = self
                    .snapshot
                    .candidates()
                    .iter()
                    .find(|candidate| candidate.registration_id().as_str() == *registration)
                    .expect("the recorded registration is in the catalog");
                let mut observation_statuses = BTreeMap::new();
                if *passes > 0 {
                    observation_statuses.insert(Status::Pass, *passes);
                }
                if *issues > 0 {
                    observation_statuses.insert(Status::Issue, *issues);
                }
                CohortStatistics {
                    key: CohortKey {
                        profile: bucket.clone(),
                        topology: candidate.descriptor().key().clone(),
                        registration: candidate.registration_id().clone(),
                        configuration: candidate.configuration_digest().clone(),
                        contract_key: self.contract.key().clone(),
                        execution: None,
                    },
                    times_considered: 1,
                    times_eligible: 1,
                    times_selected: 1,
                    propensity_sum_ppm: 1_000_000,
                    excluded_by_code: BTreeMap::new(),
                    runs_opened: 1,
                    runs_finalized: 1,
                    runs_abandoned: 0,
                    execution_dispositions: BTreeMap::new(),
                    observed_runs: 1,
                    unobserved_runs: 0,
                    conflicting_runs: u64::from(*passes > 0 && *issues > 0),
                    observation_statuses,
                    measurements: Vec::new(),
                    sample_count: passes + issues,
                    below_minimum: passes + issues == 0,
                }
            })
            .collect();
        TopologyStatsSnapshot {
            statistics_policy: policy::statistics_request().statistics_policy,
            observation_selection_policy: policy::statistics_request().observation_selection_policy,
            interval_policy: policy::statistics_request().interval_policy,
            captured_at: RecordedAt::new(LogicalSequence::new(1), None),
            window: StatisticsWindow::new(LogicalSequence::new(1), LogicalSequence::new(0), 0, 0)
                .expect("the retained window is ordered"),
            minimum_sample_count: policy::statistics_request().minimum_sample_count.get() as u64,
            cohorts,
            content_digest: ContentDigest::sha256(label.as_bytes()),
        }
    }

    /// One selection under `intent`, against `observed` (or a cold start).
    fn decide(
        &self,
        intent: CodingIntent,
        decision: &str,
        observed: Option<&TopologyStatsSnapshot>,
    ) -> SelectionDecision {
        let objective = CodingObjective::new(intent)
            .build()
            .expect("the objective builds");
        self.decide_with(&objective, &self.profile, decision, observed)
    }

    fn decide_with(
        &self,
        objective: &Objective,
        profile: &TaskProfile,
        decision: &str,
        observed: Option<&TopologyStatsSnapshot>,
    ) -> SelectionDecision {
        // A cold start still hands the selector a statistics input; an empty
        // store simply has nothing in it.
        let empty = self.statistics("empty", &[]);
        let statistics = observed.unwrap_or(&empty);
        let decision_id: SelectionDecisionId =
            uuid(decision).parse().expect("decision ID is a UUIDv4");
        let facts = policy::candidate_facts(
            &self.snapshot,
            observed.map(|statistics| (statistics, profile)),
        )
        .expect("selector facts build");
        deterministic::select(&SelectionInputs::new(
            decision_id,
            profile.clone(),
            objective.clone(),
            self.contract.key().clone(),
            self.snapshot.clone(),
            self.availability.clone(),
            self.admission.clone(),
            statistics.clone(),
            facts,
            policy::selector_config().expect("the selector configures"),
        ))
        .expect("selection reaches a decision")
    }
}

fn winner(decision: &SelectionDecision) -> String {
    match decision.outcome() {
        SelectionOutcome::Selected { selected, .. } => {
            selected.registration_id().as_str().to_owned()
        }
        SelectionOutcome::NoEligibleCandidate { .. } => {
            panic!("expected an eligible candidate")
        }
    }
}

/// The historical-statistics components recorded for `registration`.
fn observed_components<'a>(
    decision: &'a SelectionDecision,
    registration: &str,
) -> Vec<&'a polaris_cosmos::contracts::ScoreComponent> {
    decision
        .candidates()
        .iter()
        .filter(|candidate| candidate.candidate().registration_id().as_str() == registration)
        .filter_map(|candidate| candidate.score())
        .flat_map(|score| score.components())
        .filter(|component| component.source() == ScoreSource::HistoricalStatistics)
        .collect()
}

#[tokio::test]
async fn reversing_the_recorded_outcomes_reverses_the_winner() {
    let bench = Bench::new().await;

    // The prediction-error loop has a clean record; the direct edit does not.
    // The sweep's record is the same in both halves, so it takes no part in
    // the reversal.
    let prediction_error_wins = bench.statistics(
        "prediction-error-clean",
        &[
            (PREDICTION_ERROR, 4, 0),
            (DIRECT, 0, 4),
            (WORKLIST_SWEEP, 0, 4),
        ],
    );
    // The same two records, swapped. Nothing else differs.
    let direct_wins = bench.statistics(
        "direct-clean",
        &[
            (PREDICTION_ERROR, 0, 4),
            (DIRECT, 4, 0),
            (WORKLIST_SWEEP, 0, 4),
        ],
    );

    let first = bench.decide(CodingIntent::Careful, "10", Some(&prediction_error_wins));
    let second = bench.decide(CodingIntent::Careful, "11", Some(&direct_wins));

    assert_eq!(winner(&first), PREDICTION_ERROR);
    assert_eq!(
        winner(&second),
        DIRECT,
        "swapping the two records must swap the winner: the priors, the \
         catalog, the admission set and the objective are identical"
    );
}

#[tokio::test]
async fn an_empty_record_leaves_the_cold_start_exactly_as_it_was() {
    let bench = Bench::new().await;
    let empty = bench.statistics("empty", &[]);

    for (intent, expected) in [
        (CodingIntent::Fast, DIRECT),
        (CodingIntent::Careful, PREDICTION_ERROR),
        (CodingIntent::Thorough, WORKLIST_SWEEP),
    ] {
        let priors_only = bench.decide(intent, "20", None);
        let empty_record = bench.decide(intent, "20", Some(&empty));
        assert_eq!(winner(&priors_only), expected);
        assert_eq!(
            priors_only, empty_record,
            "before anything has been recorded, reading the evidence must \
             produce the very same decision the author priors produce"
        );
        assert!(
            observed_components(&empty_record, expected).is_empty(),
            "an empty store is not a measurement of anything"
        );
    }
}

#[tokio::test]
async fn a_candidate_with_no_record_stays_reachable() {
    let bench = Bench::new().await;
    // Both proven candidates failed everything. The sweep has never run.
    let others_failed = bench.statistics(
        "both-proven-candidates-failed",
        &[(PREDICTION_ERROR, 0, 4), (DIRECT, 0, 4)],
    );

    let decision = bench.decide(CodingIntent::Careful, "30", Some(&others_failed));

    assert_eq!(
        winner(&decision),
        WORKLIST_SWEEP,
        "a candidate nothing has been recorded against is still selectable"
    );
    assert!(
        observed_components(&decision, WORKLIST_SWEEP).is_empty(),
        "an unproven candidate carries no historical component at all, so it \
         is ranked on its priors rather than on a default"
    );
    assert_eq!(observed_components(&decision, DIRECT).len(), 1);
}

#[tokio::test]
async fn the_same_record_replays_to_the_same_decision() {
    let bench = Bench::new().await;
    let record = bench.statistics(
        "replayed",
        &[
            (PREDICTION_ERROR, 1, 3),
            (DIRECT, 3, 1),
            (WORKLIST_SWEEP, 2, 2),
        ],
    );

    let first = bench.decide(CodingIntent::Careful, "40", Some(&record));
    let replay = bench.decide(CodingIntent::Careful, "40", Some(&record));

    assert_eq!(
        first, replay,
        "the same frozen snapshot must give the byte-identical decision"
    );
    assert_eq!(
        first.input_refs().statistics_snapshot_digest(),
        record.content_digest(),
        "the decision names the exact snapshot it was made against"
    );
    for component in observed_components(&first, DIRECT) {
        assert_eq!(
            component.statistics_snapshot_digest(),
            Some(record.content_digest()),
            "every measured component is pinned to that same snapshot"
        );
    }
}

#[tokio::test]
async fn a_measured_component_is_visible_in_the_selection_record() {
    let bench = Bench::new().await;
    // Three of four runs of the direct edit reported an issue.
    let record = bench.statistics("one-in-four", &[(DIRECT, 1, 3)]);
    let decision = bench.decide(CodingIntent::Careful, "50", Some(&record));

    let components = observed_components(&decision, DIRECT);
    assert_eq!(components.len(), 1, "one measured component, for quality");
    let observed = components[0];
    assert_eq!(
        observed.component_id().as_str(),
        format!("{DIRECT}.observed-quality")
    );
    assert_eq!(
        observed.normalized_utility().get(),
        250_000,
        "one pass in four applied observations is a quarter"
    );
    assert!(
        observed.confidence().get() > 700_000,
        "a measured rate is stronger evidence than an author's prior"
    );

    // The record the host appends carries the whole decision, so the reason
    // the ranking moved survives past the turn that made it.
    let objective = CodingObjective::new(CodingIntent::Careful)
        .build()
        .expect("the objective builds");
    let persisted = SelectionRecord::new(
        RecordedAt::new(LogicalSequence::new(1), None),
        decision.clone(),
        bench.profile.clone(),
        objective,
    )
    .expect("the selection record validates");
    assert!(
        observed_components(persisted.decision(), DIRECT)
            .iter()
            .any(|component| component.component_id().as_str()
                == format!("{DIRECT}.observed-quality")),
        "the measured component is in the appended record, not only in the \
         outcome it produced"
    );

    // The winner's rationale names every component on a weighted criterion,
    // so a reader of the record can see quality was decided on measurement.
    let SelectionOutcome::Selected { rationale, .. } = decision.outcome() else {
        panic!("expected an eligible candidate");
    };
    assert!(
        rationale
            .decisive_components()
            .iter()
            .any(|id| id.as_str() == format!("{PREDICTION_ERROR}.quality")),
        "the winner's quality component is decisive under the careful objective"
    );
}

fn proof_ids() -> ProofEventIds {
    let id = |suffix: &str| format!("d1d2d3d4-0000-4000-8000-0000000000{suffix}");
    ProofEventIds {
        availability: id("01").parse().expect("availability ID is a UUIDv4"),
        fast_decision: id("02").parse().expect("fast decision ID is a UUIDv4"),
        careful_decision: id("03").parse().expect("careful decision ID is a UUIDv4"),
        thorough_decision: id("04").parse().expect("thorough decision ID is a UUIDv4"),
        no_eligible_decision: id("05").parse().expect("no-eligible ID is a UUIDv4"),
        later_decision: id("06").parse().expect("later decision ID is a UUIDv4"),
        admission: id("07").parse().expect("admission ID is a UUIDv4"),
        run: id("08").parse().expect("run ID is a UUIDv4"),
        applied_observation: id("09").parse().expect("applied observation is a UUIDv4"),
        residual_observation: id("10").parse().expect("residual observation is a UUIDv4"),
    }
}

#[tokio::test]
async fn a_snapshot_a_real_run_produced_reaches_the_selector() {
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
        CodingIntent::Careful,
    )
    .expect("the coding task is well formed");
    let report = CodingProof::new(task, proof_ids(), HostCapabilities::hermetic())
        .run()
        .await
        .expect("the coding proof completes");

    // The proof runs the prediction-error loop once; its two verifiers
    // disagree, so the cohort holds one pass and one issue.
    assert_eq!(report.observations[0].status(), Status::Pass);
    assert_eq!(report.observations[1].status(), Status::Issue);

    // A later selection, over the snapshot that run actually produced.
    let bench = Bench::new().await;
    let objective = CodingObjective::new(CodingIntent::Careful)
        .build()
        .expect("the objective builds");
    let decision = bench.decide_with(&objective, &report.profile, "60", Some(&report.statistics));

    let measured = |registration: &str| {
        observed_components(&decision, registration)
            .iter()
            .map(|component| component.normalized_utility().get())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        measured(PREDICTION_ERROR),
        vec![500_000],
        "the cohort a real capture wrote is the cohort the lookup finds: one \
         pass out of two applied observations"
    );
    assert!(
        measured(DIRECT).is_empty(),
        "nothing ran the direct edit, so it has no record to read"
    );
}
