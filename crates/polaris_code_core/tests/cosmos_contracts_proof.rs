//! Consumer verification of the `polaris_cosmos` topology-runtime contracts.
//!
//! One real coding task, on a real temporary crate, driven end to end through
//! the consumer-facing Cosmos contracts: profiling, mechanical admission,
//! selection under three objectives plus one unsatisfiable one, live
//! readmission, execution owned by `polaris_code`, ordered evidence, delayed
//! conflicting outcome observations, frozen statistics, and a later selection
//! pinned to that exact digest.

use polaris::system::plugin::{Plugin, PluginId};
use polaris::system::server::Server;
use polaris_code_core::cosmos::payload::EditSite;
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::proof::{CodingProof, ProofEventIds};
use polaris_code_core::cosmos::task::{CodingIntent, CodingObjective, CodingTask};
use polaris_code_core::cosmos::topology::{
    COVERAGE_AUDIT, CodingTopologyPackPlugin, DIRECT, HostCapabilities, MODEL_PROPOSED,
    PREDICTION_ERROR, TEST_GATED, TREE_QUESTION, WORKLIST_SWEEP,
};
use polaris_cosmos::contracts::{
    CanonicalRecord, DecompositionShape, NeedLevel, Status, TaskHorizon, UnknownReason,
    VerifierAvailability, VerifierCapability,
};
use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogPlugin};

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

fn ids() -> ProofEventIds {
    let id = |suffix: &str| format!("b1c2d3e4-0000-4000-8000-0000000000{suffix}");
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

fn project() -> (tempfile::TempDir, CodingTask) {
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
    (dir, task)
}

#[test]
fn coding_payload_profiles_into_the_closed_feature_vocabulary() {
    let (_dir, task) = project();
    let profile = task.profile().expect("the profile aggregates");

    // Measured off the real tree: two stub sites, so independent parts.
    let features = profile.features();
    assert_eq!(
        features.decomposability.known_value().copied(),
        Some(DecompositionShape::IndependentParts)
    );
    assert_eq!(
        features
            .expected_independent_work
            .known_value()
            .map(|estimate| estimate.likely()),
        Some(2)
    );
    assert_eq!(
        features.horizon.known_value().copied(),
        Some(TaskHorizon::ShortSequence)
    );

    // The embodiment really does ship a deterministic verifier and nothing else.
    let verifier = features.verifier.known_value().expect("verifier is known");
    assert_eq!(verifier.availability(), VerifierAvailability::Available);
    assert_eq!(verifier.capabilities(), [VerifierCapability::Deterministic]);

    // `edit_file` is Confirm-gated in this host, so a human is in the path.
    assert_eq!(
        features.human_interaction_need.known_value().copied(),
        Some(NeedLevel::Medium)
    );

    // Budgets the terminal never asks for are unobserved, not defaulted to none.
    assert_eq!(
        features.requested_token_limit.unknown_reason(),
        Some(UnknownReason::NotObserved)
    );
    assert_eq!(features.requested_token_limit.confidence(), None);
    assert_eq!(
        features.requested_monetary_limit.unknown_reason(),
        Some(UnknownReason::NotObserved)
    );

    // Every field carries attribution.
    assert!(!features.decomposability.provenance().is_empty());
    profile.validate().expect("the profile is canonical");
}

async fn coding_pack_server() -> Server {
    let mut server = Server::new();
    server
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyCatalogPlugin);
    server.finish().await.expect("the coding pack assembles");
    server
}

#[tokio::test]
async fn every_registration_claims_the_one_coding_contract() {
    let server = coding_pack_server().await;
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed");
    assert_eq!(snapshot.candidates().len(), 7);
    let contract = snapshot.contracts()[0].key();
    for candidate in snapshot.candidates() {
        assert!(
            candidate.descriptor().contracts().contains(contract),
            "{} does not claim the coding contract",
            candidate.registration_id().as_str()
        );
    }
}

#[tokio::test]
async fn one_pack_plugin_contributes_every_topology_without_host_registration_wiring() {
    let server = coding_pack_server().await;
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed");
    assert_eq!(snapshot.candidates().len(), 7);

    let pack = server
        .plugin_manifest()
        .entry(&PluginId::new(CodingTopologyPackPlugin::ID))
        .expect("the host enables one coding pack plugin");
    assert_eq!(pack.extends().len(), 1);
    assert_eq!(
        pack.extends()[0].provider(),
        Some(&PluginId::new(TopologyCatalogPlugin::ID))
    );
    assert!(
        snapshot.candidates().len() > pack.extends().len(),
        "host assembly is pack-level, not one wiring edge per topology"
    );
}

#[tokio::test]
async fn a_real_coding_task_drives_the_cosmos_contracts_end_to_end() {
    let (_dir, task) = project();
    let report = CodingProof::new(task, ids(), HostCapabilities::hermetic())
        .run()
        .await
        .expect("the coding proof completes");

    // --- mechanical admission: Cosmos now explains its private sweep state
    assert_eq!(
        report.admitted(),
        vec![
            COVERAGE_AUDIT.to_owned(),
            DIRECT.to_owned(),
            PREDICTION_ERROR.to_owned(),
            // The answer shape requires nothing, so it admits everywhere the
            // working directory is readable — including this hermetic host.
            TREE_QUESTION.to_owned(),
            WORKLIST_SWEEP.to_owned()
        ],
        "exclusions were {:?}",
        report.exclusions
    );
    assert!(
        report.exclusions[TEST_GATED].contains(&"resource.missing".to_owned()),
        "test-gated exclusions were {:?}",
        report.exclusions[TEST_GATED]
    );
    assert!(report.exclusions[COVERAGE_AUDIT].is_empty());
    assert!(
        !report.exclusions[MODEL_PROPOSED].is_empty(),
        "the model-proposed candidate should be excluded for want of a provider"
    );

    // --- objective-driven reranking: three objectives, three winners
    assert_eq!(report.winners["fast"], DIRECT);
    assert_eq!(report.winners["careful"], PREDICTION_ERROR);
    assert_eq!(report.winners["thorough"], WORKLIST_SWEEP);

    // --- an unsatisfiable objective yields a typed diagnostic, not a guess
    assert!(
        !report.no_eligible_diagnostics.is_empty(),
        "a comparative-verifier requirement must produce diagnostics"
    );

    // --- the executed run really edited the file, through the loop
    assert!(report.final_target_content.contains("a + b"));
    assert!(!report.final_target_content.contains(ADD_STUB));
    assert_eq!(
        report.executed_roles,
        vec![
            "prediction.predict",
            "prediction.act",
            "prediction.verify",
            "prediction.compare",
            "prediction.accept",
        ],
        "the prediction-error loop did not run its stages in order"
    );

    // --- graph completion is mechanical; success is a separate later claim
    assert!(
        report.observations_conflict(),
        "the applied-edit and residual-scan verifiers must disagree here"
    );
    assert_eq!(report.observations[0].status(), Status::Pass);
    assert_eq!(report.observations[1].status(), Status::Issue);
    assert!(
        report.observations[1].supersedes_claim().is_some(),
        "the delayed verdict must supersede rather than overwrite"
    );

    // --- statistics are frozen and a later selection is pinned to them
    assert!(report.later_selection_is_stable);
    assert!(report.permit_is_single_use);
    assert_eq!(report.considered().len(), 7);
}

#[tokio::test]
async fn author_priors_are_owned_by_complete_catalog_registrations() {
    let server = coding_pack_server().await;
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed");
    let facts = policy::candidate_facts(&snapshot, None).expect("selector facts build");
    assert_eq!(facts.len(), 7);
    let scored = snapshot
        .candidates()
        .iter()
        .filter(|candidate| {
            candidate
                .admission_metadata()
                .is_some_and(|metadata| !metadata.author_priors().is_empty())
        })
        .count();
    assert_eq!(scored, 4);
}

#[test]
fn a_coding_site_is_the_assessment_subject() {
    let site = EditSite::new("src/lib.rs".to_owned(), 5, Status::Pass);
    let assessment = polaris_code_core::cosmos::assess::site_assessment(site.clone())
        .expect("the site validates against the coding subject schema");
    assert_eq!(assessment.status(), Status::Pass);
    assert_eq!(assessment.subject(), &site);
}

#[test]
fn an_objective_can_demand_a_capability_this_embodiment_lacks() {
    let objective = CodingObjective::new(CodingIntent::Careful)
        .requiring_comparative_verifier()
        .build()
        .expect("the objective builds");
    assert_eq!(
        objective.hard().required_verifier_capabilities(),
        [VerifierCapability::Comparative]
    );
}

#[tokio::test]
async fn each_exclusion_channel_reports_its_own_code() {
    let (_dir, task) = project();
    let report = CodingProof::new(task, ids(), HostCapabilities::hermetic())
        .run()
        .await
        .expect("the coding proof completes");
    // Pinned so a change in which channel rejects which candidate is visible.
    assert_eq!(
        report.exclusions[TEST_GATED],
        vec!["resource.missing".to_owned()]
    );
    assert_eq!(
        report.exclusions[MODEL_PROPOSED],
        vec!["provider.unavailable".to_owned()]
    );
    // The pack reuses Cosmos's explanation helper, so its private boundaries
    // are complete at assembly and no longer become a runtime exclusion.
    assert!(report.exclusions[COVERAGE_AUDIT].is_empty());
    assert!(report.exclusion_details[COVERAGE_AUDIT].is_empty());
    for admitted in [
        COVERAGE_AUDIT,
        DIRECT,
        PREDICTION_ERROR,
        TREE_QUESTION,
        WORKLIST_SWEEP,
    ] {
        assert!(report.exclusions[admitted].is_empty());
    }
}
