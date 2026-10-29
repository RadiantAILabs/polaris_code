//! Behavioural matrix for explained deterministic topology selection.

use std::sync::Arc;

use polaris::graph::{Graph, GraphExecutor, GraphSignature};
use polaris::system::param::SystemContext;
use polaris::system::plugin::Version;
use polaris::system::system;
use polaris_cosmos::contracts::{
    AdmissionResult, AvailabilitySnapshotId, CanonicalRecord, ConfidencePpm, ConfigurationSchemaId,
    ConfigurationSchemaKey, ConfigurationValue, ContentDigest, DecompositionShape, DurationMs,
    EnvironmentId, EnvironmentKey, EpistemicBasis, EvidenceKind, EvidenceObjectId, EvidenceRef,
    EvidenceStoreId, EvidenceTime, ExclusionCode, ExclusionPhase, ExclusionReason,
    FeatureAssessment, HardConstraints, MaturityLevel, Measurement, MeasurementValue,
    MissingPreferencePolicy, NeedLevel, Objective, ObservedLimit, PolicyId, PolicyRef,
    PreferenceBasisPoints, PreferenceCriterion, PreferenceWeights, ProbabilityPpm, Provenance,
    RegistrationSelectionRef, ResolvedConfigurationRef, RiskLevel, RuntimeAvailabilitySnapshot,
    ScoreComponent, ScoreComponentId, ScoreSource, SelectionDecisionId, SelectionOutcome,
    SelectorRef, SourceId, SourceKey, SourceKind, TaskFeatures, TaskHorizon, TaskId, TaskProfile,
    TaskProfileEvidence, TenancyScopeId, TieBreakPolicy, UnknownEstimatePolicy, UnknownReason,
    UtilityPpm, VerifierAvailability, VerifierCapability, VerifierProfile,
};
use polaris_cosmos::topology::{
    ConfigurationDigest, RegisteredTopology, RegistrationId, RegistrationProvenance,
    TopologyCatalog, TopologyCatalogSnapshot, TopologyContract, TopologyContractId,
    TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey,
};

mod contracts {
    pub use polaris_cosmos::contracts::*;
}

mod topology {
    pub use polaris_cosmos::topology::*;

    pub mod admission {
        use crate::contracts::{AdmissionResult, ContentDigest, RegistrationSelectionRef};
        use crate::topology::{TopologyCatalogSnapshot, TopologyContractKey};

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct CandidateMechanicalAdmission {
            candidate: RegistrationSelectionRef,
            result: AdmissionResult,
        }

        impl CandidateMechanicalAdmission {
            pub const fn new(candidate: RegistrationSelectionRef, result: AdmissionResult) -> Self {
                Self { candidate, result }
            }

            pub const fn candidate(&self) -> &RegistrationSelectionRef {
                &self.candidate
            }

            pub const fn result(&self) -> &AdmissionResult {
                &self.result
            }
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct CandidateAdmissionSet {
            requested_contract: TopologyContractKey,
            catalog_generation: u64,
            catalog_snapshot_digest: ContentDigest,
            evaluations: Vec<CandidateMechanicalAdmission>,
        }

        impl CandidateAdmissionSet {
            pub fn stand_in(
                catalog: &TopologyCatalogSnapshot,
                requested_contract: TopologyContractKey,
                catalog_snapshot_digest: ContentDigest,
                mut evaluations: Vec<CandidateMechanicalAdmission>,
            ) -> Self {
                evaluations.sort_by(|left, right| {
                    left.candidate()
                        .registration_id()
                        .cmp(right.candidate().registration_id())
                });
                Self {
                    requested_contract,
                    catalog_generation: catalog.generation(),
                    catalog_snapshot_digest,
                    evaluations,
                }
            }

            pub const fn requested_contract(&self) -> &TopologyContractKey {
                &self.requested_contract
            }

            pub const fn catalog_generation(&self) -> u64 {
                self.catalog_generation
            }

            pub const fn catalog_snapshot_digest(&self) -> ContentDigest {
                self.catalog_snapshot_digest
            }

            pub fn evaluations(&self) -> &[CandidateMechanicalAdmission] {
                &self.evaluations
            }
        }
    }
}

mod evidence {
    pub mod store {
        use crate::contracts::ContentDigest;

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct TopologyStatsSnapshot {
            content_digest: ContentDigest,
        }

        impl TopologyStatsSnapshot {
            pub const fn stand_in(content_digest: ContentDigest) -> Self {
                Self { content_digest }
            }

            pub const fn content_digest(&self) -> ContentDigest {
                self.content_digest
            }
        }
    }
}

#[allow(dead_code)]
#[path = "../src/selection/deterministic.rs"]
mod deterministic;

#[allow(dead_code)]
#[path = "../src/selection/transparent_profile.rs"]
mod transparent_profile;

use deterministic::{
    CandidatePolicyFacts, DeterministicSelectorConfig, RootSelectorConfig, SelectionInputs,
};
use evidence::store::TopologyStatsSnapshot;
use topology::admission::{CandidateAdmissionSet, CandidateMechanicalAdmission};
use transparent_profile::ProfileInputs;

const DECISION_A: &str = "67e55044-10b1-426f-8247-bb680e5fe201";
const DECISION_B: &str = "67e55044-10b1-426f-8247-bb680e5fe202";
const AVAILABILITY_ID: &str = "67e55044-10b1-426f-8247-bb680e5fe203";

#[derive(Clone)]
struct FixtureResult;

#[system]
async fn candidate_system() -> FixtureResult {
    FixtureResult
}

fn version() -> Version {
    Version::new(1, 0, 0)
}

fn policy(id: &str) -> PolicyRef {
    PolicyRef::new(PolicyId::new(id).unwrap(), version())
}

fn evidence(id: &str) -> EvidenceRef {
    EvidenceRef::new(
        EvidenceStoreId::new("selection.tests").unwrap(),
        EvidenceKind::ObservationNote,
        EvidenceObjectId::new(id).unwrap(),
    )
}

fn provenance(id: &str, kind: SourceKind) -> Provenance {
    Provenance::new(
        SourceKey::new(SourceId::new("selection.fixture").unwrap(), version()),
        kind,
        EpistemicBasis::Verified,
    )
    .with_evidence(evidence(id))
}

fn known<T>(value: T, id: &str) -> FeatureAssessment<T> {
    FeatureAssessment::known(
        value,
        ConfidencePpm::new(900_000).unwrap(),
        vec![provenance(id, SourceKind::EmbodimentExtractor)],
    )
    .unwrap()
}

fn unknown<T>(id: &str) -> FeatureAssessment<T> {
    FeatureAssessment::unknown(
        UnknownReason::NotObserved,
        vec![provenance(id, SourceKind::EmbodimentExtractor)],
    )
    .unwrap()
}

fn known_with_confidence<T>(value: T, confidence: u32, id: &str) -> FeatureAssessment<T> {
    FeatureAssessment::known(
        value,
        ConfidencePpm::new(confidence).unwrap(),
        vec![provenance(id, SourceKind::EmbodimentExtractor)],
    )
    .unwrap()
}

fn empty_profile_evidence() -> TaskProfileEvidence {
    TaskProfileEvidence {
        decomposability: Vec::new(),
        expected_independent_work: Vec::new(),
        verifier: Vec::new(),
        uncertainty: Vec::new(),
        exploration_need: Vec::new(),
        refinement_need: Vec::new(),
        context_isolation_need: Vec::new(),
        human_interaction_need: Vec::new(),
        consequence: Vec::new(),
        inherent_task_risk: Vec::new(),
        horizon: Vec::new(),
        requested_latency_limit: Vec::new(),
        requested_token_limit: Vec::new(),
        requested_monetary_limit: Vec::new(),
    }
}

fn profile(namespace: &str, task: &str, shape: DecompositionShape) -> TaskProfile {
    TaskProfile::new(
        TaskId::new(namespace, task).unwrap(),
        None,
        TaskFeatures {
            decomposability: known(shape, "profile.decomposition"),
            expected_independent_work: unknown("profile.cardinality"),
            verifier: known(
                VerifierProfile::new(
                    VerifierAvailability::Available,
                    vec![VerifierCapability::Deterministic],
                )
                .unwrap(),
                "profile.verifier",
            ),
            uncertainty: known(NeedLevel::Medium, "profile.uncertainty"),
            exploration_need: known(NeedLevel::Low, "profile.exploration"),
            refinement_need: known(NeedLevel::Medium, "profile.refinement"),
            context_isolation_need: known(NeedLevel::Low, "profile.isolation"),
            human_interaction_need: known(NeedLevel::Low, "profile.human"),
            consequence: unknown("profile.consequence"),
            inherent_task_risk: known(RiskLevel::Moderate, "profile.risk"),
            horizon: known(TaskHorizon::Iterative, "profile.horizon"),
            requested_latency_limit: known(ObservedLimit::NotStated, "profile.requested-latency"),
            requested_token_limit: unknown("profile.requested-tokens"),
            requested_monetary_limit: unknown("profile.requested-money"),
        },
    )
    .unwrap()
}

fn weights(quality: u16, latency: u16) -> PreferenceWeights {
    PreferenceWeights::new(
        PreferenceBasisPoints::new(quality).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(latency).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
    )
    .unwrap()
}

fn objective(hard: HardConstraints, quality: u16, latency: u16) -> Objective {
    Objective::new(
        hard,
        weights(quality, latency),
        MissingPreferencePolicy::Pessimistic,
        TieBreakPolicy::AscendingRegistrationId,
        vec![provenance("objective", SourceKind::PolicyDefault)],
    )
    .unwrap()
}

fn contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new("selection.fixture-task").unwrap(),
        version(),
    )
}

fn selector_contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new("selection.selector").unwrap(),
        version(),
    )
}

fn availability() -> RuntimeAvailabilitySnapshot {
    RuntimeAvailabilitySnapshot::new(
        AVAILABILITY_ID.parse::<AvailabilitySnapshotId>().unwrap(),
        EvidenceTime::new(None, Some(1_000)).unwrap(),
        EnvironmentKey::new(
            EnvironmentId::new("selection.environment").unwrap(),
            version(),
        ),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![provenance("availability", SourceKind::HostObservation)],
    )
    .unwrap()
}

fn selector_config() -> DeterministicSelectorConfig {
    DeterministicSelectorConfig::new(
        SelectorRef::new(
            deterministic::selector_key(),
            ConfigurationDigest::new(Arc::<str>::from(
                ContentDigest::sha256(b"selector-config").to_string(),
            ))
            .unwrap(),
        )
        .unwrap(),
        policy("selection.missing-evidence"),
        policy("selection.minimum-confidence"),
        vec![provenance("selector", SourceKind::CosmosPolicy)],
    )
    .unwrap()
}

fn score_component(id: &str, criterion: PreferenceCriterion, utility: u32) -> ScoreComponent {
    let measurement = match criterion {
        PreferenceCriterion::Latency => MeasurementValue::Duration(DurationMs::new(utility.into())),
        _ => MeasurementValue::Proportion(
            polaris_cosmos::contracts::CoveragePpm::new(utility).unwrap(),
        ),
    };
    ScoreComponent::new(
        ScoreComponentId::new(id).unwrap(),
        criterion,
        ScoreSource::AuthorPrior,
        Measurement::new(
            measurement,
            None,
            ConfidencePpm::new(850_000).unwrap(),
            vec![provenance(id, SourceKind::CosmosPolicy)],
        )
        .unwrap(),
        policy("selection.linear-normalization"),
        UtilityPpm::new(utility).unwrap(),
        ConfidencePpm::new(850_000).unwrap(),
        vec![provenance(id, SourceKind::CosmosPolicy)],
        None,
    )
    .unwrap()
}

fn historical_component(id: &str, statistics_digest: ContentDigest) -> ScoreComponent {
    ScoreComponent::new(
        ScoreComponentId::new(id).unwrap(),
        PreferenceCriterion::Quality,
        ScoreSource::HistoricalStatistics,
        Measurement::new(
            MeasurementValue::Proportion(
                polaris_cosmos::contracts::CoveragePpm::new(750_000).unwrap(),
            ),
            None,
            ConfidencePpm::new(800_000).unwrap(),
            vec![provenance(id, SourceKind::CosmosPolicy)],
        )
        .unwrap(),
        policy("selection.historical-normalization"),
        UtilityPpm::new(750_000).unwrap(),
        ConfidencePpm::new(800_000).unwrap(),
        vec![provenance(id, SourceKind::CosmosPolicy)],
        Some(statistics_digest),
    )
    .unwrap()
}

fn facts(id: &RegistrationId, quality: u32, latency_utility: u32) -> CandidatePolicyFacts {
    CandidatePolicyFacts::new(id.clone())
        .with_estimated_latency(DurationMs::new(1_000_000 - u64::from(latency_utility)))
        .with_maturity(MaturityLevel::Stable)
        .with_side_effect_risk(RiskLevel::Low)
        .with_execution_failure_probability(ProbabilityPpm::new(10_000).unwrap())
        .with_score_components(vec![
            score_component(
                &format!("{}.quality", id.as_str()),
                PreferenceCriterion::Quality,
                quality,
            ),
            score_component(
                &format!("{}.latency", id.as_str()),
                PreferenceCriterion::Latency,
                latency_utility,
            ),
        ])
}

struct CatalogFixture {
    catalog: TopologyCatalog,
    snapshot: TopologyCatalogSnapshot,
    selections: Vec<RegistrationSelectionRef>,
    digest: ContentDigest,
}

fn catalog_fixture(ids: &[&str]) -> CatalogFixture {
    let contract_key = contract_key();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(
            TopologyContract::new(
                contract_key.clone(),
                GraphSignature::new().produce::<FixtureResult>(),
                "A portable task",
                "A portable result",
            )
            .unwrap(),
        )
        .unwrap();
    let mut selections = Vec::new();
    for id in ids {
        let registration_id = RegistrationId::new(*id).unwrap();
        let topology_key = TopologyKey::new(
            TopologyId::new(format!("topology.{id}")).unwrap(),
            version(),
        );
        let configuration = ResolvedConfigurationRef::inline(
            ConfigurationSchemaKey::new(
                ConfigurationSchemaId::new("selection.fixture-config").unwrap(),
                version(),
            ),
            ConfigurationValue::String((*id).to_owned()),
        )
        .unwrap();
        let mut graph = Graph::new();
        graph.add_system(candidate_system);
        catalog
            .register(
                registration_id.clone(),
                TopologyDescriptor::new(
                    topology_key.clone(),
                    format!("Candidate {id}"),
                    "A deterministic fixture candidate",
                )
                .unwrap()
                .claim(contract_key.clone()),
                configuration.digest().clone(),
                graph,
                RegistrationProvenance::new("selection.fixture-pack").unwrap(),
            )
            .unwrap();
        selections.push(
            RegistrationSelectionRef::new(
                registration_id,
                topology_key,
                ContentDigest::sha256(format!("revision:{id}").as_bytes()),
                configuration,
            )
            .unwrap(),
        );
    }
    let snapshot = catalog.seal();
    let digest = ContentDigest::sha256(ids.join(",").as_bytes());
    CatalogFixture {
        catalog,
        snapshot,
        selections,
        digest,
    }
}

fn admitted_set(fixture: &CatalogFixture, results: Vec<AdmissionResult>) -> CandidateAdmissionSet {
    CandidateAdmissionSet::stand_in(
        &fixture.snapshot,
        contract_key(),
        fixture.digest,
        fixture
            .selections
            .iter()
            .cloned()
            .zip(results)
            .map(|(candidate, result)| CandidateMechanicalAdmission::new(candidate, result))
            .collect(),
    )
}

fn selection_inputs(
    fixture: &CatalogFixture,
    decision_id: &str,
    objective: Objective,
    admission: CandidateAdmissionSet,
    policy_facts: Vec<CandidatePolicyFacts>,
) -> SelectionInputs {
    SelectionInputs::new(
        decision_id.parse::<SelectionDecisionId>().unwrap(),
        profile(
            "coding",
            "fix-independent-modules",
            DecompositionShape::IndependentParts,
        ),
        objective,
        contract_key(),
        fixture.snapshot.clone(),
        availability(),
        admission,
        TopologyStatsSnapshot::stand_in(ContentDigest::sha256(b"statistics")),
        policy_facts,
        selector_config(),
    )
}

fn selected_id(decision: &polaris_cosmos::contracts::SelectionDecision) -> &RegistrationId {
    match decision.outcome() {
        SelectionOutcome::Selected { selected, .. } => selected.registration_id(),
        SelectionOutcome::NoEligibleCandidate { .. } => panic!("expected a selected candidate"),
    }
}

#[test]
fn mechanical_exclusion_is_never_reversed_or_ranked() {
    let fixture = catalog_fixture(&["candidate.a", "candidate.b"]);
    let exclusion = AdmissionResult::excluded(vec![ExclusionReason::new(
        ExclusionPhase::Mechanical,
        ExclusionCode::new("resource.missing").unwrap(),
        Vec::new(),
        vec![fixture.digest],
    )])
    .unwrap();
    let admission = admitted_set(&fixture, vec![AdmissionResult::Admitted, exclusion]);
    let decision = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_A,
        objective(HardConstraints::none(), 10_000, 0),
        admission,
        vec![
            facts(
                &fixture.selections[0].registration_id().clone(),
                100_000,
                100_000,
            ),
            facts(
                &fixture.selections[1].registration_id().clone(),
                999_000,
                999_000,
            ),
        ],
    ))
    .unwrap();

    assert_eq!(selected_id(&decision).as_str(), "candidate.a");
    let excluded = &decision.candidates()[1];
    assert!(!excluded.mechanical_admission().is_admitted());
    assert!(matches!(
        excluded.policy_eligibility(),
        polaris_cosmos::contracts::PolicyEligibility::NotEvaluated
    ));
    assert!(excluded.score().is_none());
    assert_eq!(excluded.rank(), None);
    assert_eq!(excluded.selection_propensity(), ProbabilityPpm::ZERO);
}

#[test]
fn objective_changes_ranking_without_mutating_the_catalog() {
    let fixture = catalog_fixture(&["candidate.a", "candidate.b"]);
    let admission = admitted_set(
        &fixture,
        vec![AdmissionResult::Admitted, AdmissionResult::Admitted],
    );
    let policy_facts = vec![
        facts(
            &fixture.selections[0].registration_id().clone(),
            900_000,
            100_000,
        ),
        facts(
            &fixture.selections[1].registration_id().clone(),
            600_000,
            950_000,
        ),
    ];
    let generation = fixture.snapshot.generation();
    let quality = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_A,
        objective(HardConstraints::none(), 10_000, 0),
        admission.clone(),
        policy_facts.clone(),
    ))
    .unwrap();
    let latency = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_A,
        objective(HardConstraints::none(), 0, 10_000),
        admission,
        policy_facts,
    ))
    .unwrap();

    assert_eq!(selected_id(&quality).as_str(), "candidate.a");
    assert_eq!(selected_id(&latency).as_str(), "candidate.b");
    assert_eq!(fixture.catalog.generation(), generation);
    assert_eq!(fixture.catalog.snapshot().unwrap().generation(), generation);
}

#[test]
fn identical_inputs_replay_and_exact_ties_follow_registration_id() {
    let fixture = catalog_fixture(&["candidate.a", "candidate.b", "candidate.c"]);
    let admission = admitted_set(
        &fixture,
        vec![
            AdmissionResult::Admitted,
            AdmissionResult::Admitted,
            AdmissionResult::Admitted,
        ],
    );
    let policy_facts: Vec<_> = fixture
        .selections
        .iter()
        .map(|candidate| facts(candidate.registration_id(), 700_000, 700_000))
        .collect();
    let first = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_A,
        objective(HardConstraints::none(), 5_000, 5_000),
        admission.clone(),
        policy_facts.clone(),
    ))
    .unwrap();
    let second = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_B,
        objective(HardConstraints::none(), 5_000, 5_000),
        admission,
        policy_facts,
    ))
    .unwrap();

    assert_eq!(first.fingerprint(), second.fingerprint());
    assert_ne!(first.decision_id(), second.decision_id());
    assert_eq!(first.candidates(), second.candidates());
    assert_eq!(first.outcome(), second.outcome());
    first.validate().unwrap();
    second.validate().unwrap();
    assert_eq!(selected_id(&first).as_str(), "candidate.a");
    assert_eq!(
        first
            .candidates()
            .iter()
            .filter_map(|candidate| candidate
                .rank()
                .map(|rank| (candidate.candidate().registration_id().as_str(), rank)))
            .collect::<Vec<_>>(),
        vec![("candidate.a", 1), ("candidate.b", 2), ("candidate.c", 3)]
    );
    match first.outcome() {
        SelectionOutcome::Selected {
            fallback_plan,
            rationale,
            confidence,
            ..
        } => {
            assert!(rationale.tie_break_applied());
            assert!(!rationale.decisive_components().is_empty());
            assert_eq!(*confidence, ConfidencePpm::new(850_000).unwrap());
            assert_eq!(
                fallback_plan
                    .iter()
                    .map(|fallback| fallback.candidate().registration_id().as_str())
                    .collect::<Vec<_>>(),
                vec!["candidate.b", "candidate.c"]
            );
        }
        SelectionOutcome::NoEligibleCandidate { .. } => unreachable!(),
    }
}

#[test]
fn no_eligible_candidate_retains_complete_mechanical_and_policy_diagnostics() {
    let fixture = catalog_fixture(&["candidate.a", "candidate.b"]);
    let mechanical = AdmissionResult::excluded(vec![ExclusionReason::new(
        ExclusionPhase::Mechanical,
        ExclusionCode::new("contract.unsatisfied").unwrap(),
        Vec::new(),
        vec![fixture.digest],
    )])
    .unwrap();
    let admission = admitted_set(&fixture, vec![mechanical, AdmissionResult::Admitted]);
    let hard = HardConstraints::none()
        .with_maximum_latency(DurationMs::new(10))
        .with_required_verifier_capabilities(vec![VerifierCapability::Deterministic])
        .with_minimum_maturity(MaturityLevel::Stable)
        .with_allowed_tenancy_scopes(vec![TenancyScopeId::new("approved").unwrap()])
        .unwrap()
        .with_unknown_estimate_policy(UnknownEstimatePolicy::Reject);
    let violating = CandidatePolicyFacts::new(fixture.selections[1].registration_id().clone())
        .with_estimated_latency(DurationMs::new(1_000))
        .with_maturity(MaturityLevel::Experimental)
        .with_tenancy_scopes(vec![TenancyScopeId::new("outside").unwrap()]);
    let decision = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_A,
        objective(hard, 10_000, 0),
        admission,
        vec![
            facts(fixture.selections[0].registration_id(), 1, 1),
            violating,
        ],
    ))
    .unwrap();

    assert!(
        decision
            .candidates()
            .iter()
            .all(|candidate| candidate.score().is_none())
    );
    let SelectionOutcome::NoEligibleCandidate { diagnostics } = decision.outcome() else {
        panic!("expected typed no-eligible outcome");
    };
    let codes: Vec<_> = diagnostics
        .iter()
        .map(|summary| (summary.phase(), summary.code().as_str()))
        .collect();
    assert!(codes.contains(&(ExclusionPhase::Mechanical, "contract.unsatisfied")));
    assert!(codes.contains(&(ExclusionPhase::Policy, "budget.latency")));
    assert!(codes.contains(&(ExclusionPhase::Policy, "verifier.insufficient")));
    assert!(codes.contains(&(ExclusionPhase::Policy, "maturity.below-minimum")));
    assert!(codes.contains(&(ExclusionPhase::Policy, "tenancy.disallowed")));
}

#[test]
fn empty_catalog_has_the_global_no_candidate_diagnostic() {
    let fixture = catalog_fixture(&[]);
    let decision = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_A,
        objective(HardConstraints::none(), 10_000, 0),
        admitted_set(&fixture, Vec::new()),
        Vec::new(),
    ))
    .unwrap();
    assert!(decision.candidates().is_empty());
    let SelectionOutcome::NoEligibleCandidate { diagnostics } = decision.outcome() else {
        panic!("expected typed no-eligible outcome");
    };
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().as_str(), "catalog.empty");
    assert_eq!(diagnostics[0].candidate_count(), 0);
}

#[test]
fn historical_components_are_bound_to_the_frozen_statistics_snapshot() {
    let fixture = catalog_fixture(&["candidate.a"]);
    let facts = CandidatePolicyFacts::new(fixture.selections[0].registration_id().clone())
        .with_score_components(vec![historical_component(
            "candidate.a.history",
            ContentDigest::sha256(b"other-statistics"),
        )]);
    let result = deterministic::select(&selection_inputs(
        &fixture,
        DECISION_A,
        objective(HardConstraints::none(), 10_000, 0),
        admitted_set(&fixture, vec![AdmissionResult::Admitted]),
        vec![facts],
    ));

    assert!(matches!(
        result,
        Err(deterministic::SelectionError::StatisticsSnapshotMismatch { .. })
    ));
}

#[tokio::test]
async fn selector_is_an_ordinary_graph_and_profile_is_portable() {
    let fixture = catalog_fixture(&["candidate.a"]);
    let inputs = selection_inputs(
        &fixture,
        DECISION_A,
        objective(HardConstraints::none(), 10_000, 0),
        admitted_set(&fixture, vec![AdmissionResult::Admitted]),
        vec![facts(
            fixture.selections[0].registration_id(),
            800_000,
            500_000,
        )],
    );
    let mut graph = Graph::new();
    deterministic::build(&mut graph);
    assert!(graph.validate().is_ok());
    assert!(
        graph
            .signature()
            .satisfies(&deterministic::contract_signature())
    );

    let mut context = SystemContext::new();
    context.insert_output(inputs);
    let execution = GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .unwrap();
    assert_eq!(
        selected_id(execution.output().unwrap()).as_str(),
        "candidate.a"
    );

    let coding = profile(
        "coding",
        "fix-modules",
        DecompositionShape::IndependentParts,
    );
    let research = profile(
        "research",
        "compare-sources",
        DecompositionShape::Sequential,
    );
    assert_ne!(coding.content_digest(), research.content_digest());
    assert_eq!(coding.schema_version(), research.schema_version());
}

#[test]
fn static_root_selector_resolves_directly_from_the_same_catalog() {
    let selector_contract = selector_contract_key();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(
            TopologyContract::new(
                selector_contract.clone(),
                deterministic::contract_signature(),
                "Frozen selection inputs",
                "Explained selection decision",
            )
            .unwrap(),
        )
        .unwrap();
    let registration = RegistrationId::new("selector.root").unwrap();
    let mut graph = Graph::new();
    deterministic::build(&mut graph);
    catalog
        .register(
            registration.clone(),
            deterministic::descriptor(selector_contract.clone()),
            ConfigurationDigest::new("sha256:selector-root").unwrap(),
            graph,
            RegistrationProvenance::new("selection.fixture-pack").unwrap(),
        )
        .unwrap();
    let snapshot = catalog.seal();

    let resolved: Arc<RegisteredTopology> =
        RootSelectorConfig::new(registration.clone(), selector_contract)
            .resolve(&catalog, &snapshot)
            .unwrap();
    assert_eq!(resolved.registration_id(), &registration);
    assert_eq!(resolved.signature(), &deterministic::contract_signature());
}

#[test]
fn transparent_profiler_combines_identical_evidence_conservatively() {
    let mut evidence = empty_profile_evidence();
    evidence.decomposability = vec![
        known_with_confidence(
            DecompositionShape::IndependentParts,
            900_000,
            "profile.first",
        ),
        known_with_confidence(
            DecompositionShape::IndependentParts,
            600_000,
            "profile.second",
        ),
    ];
    let inputs = ProfileInputs::new(
        TaskId::new("research", "survey-sources").unwrap(),
        None,
        evidence,
        version(),
        vec![provenance("profile.policy", SourceKind::CosmosPolicy)],
    )
    .unwrap();
    let profile = transparent_profile::aggregate(&inputs).unwrap();

    assert_eq!(
        profile.features().decomposability.known_value(),
        Some(&DecompositionShape::IndependentParts)
    );
    assert_eq!(
        profile.features().decomposability.confidence(),
        Some(ConfidencePpm::new(600_000).unwrap())
    );
    assert_eq!(profile.features().decomposability.provenance().len(), 3);
    assert_eq!(
        profile
            .features()
            .expected_independent_work
            .unknown_reason(),
        Some(UnknownReason::NotObserved)
    );
}

#[tokio::test]
async fn transparent_profiler_surfaces_conflicts_as_an_ordinary_graph() {
    let mut evidence = empty_profile_evidence();
    evidence.decomposability = vec![
        known(DecompositionShape::IndependentParts, "profile.independent"),
        known(DecompositionShape::Sequential, "profile.sequential"),
    ];
    let inputs = ProfileInputs::new(
        TaskId::new("coding", "inspect-worktree").unwrap(),
        None,
        evidence,
        version(),
        vec![provenance("profile.policy", SourceKind::CosmosPolicy)],
    )
    .unwrap();
    let mut graph = Graph::new();
    transparent_profile::build(&mut graph);
    assert!(graph.validate().is_ok());
    assert!(
        graph
            .signature()
            .satisfies(&transparent_profile::contract_signature())
    );
    let mut context = SystemContext::new();
    context.insert_output(inputs);
    let execution = GraphExecutor::new()
        .execute(&graph, &mut context, None, None)
        .await
        .unwrap();
    let aggregated: &TaskProfile = execution.output().unwrap();
    assert_eq!(
        aggregated.features().decomposability.unknown_reason(),
        Some(UnknownReason::ConflictingEvidence)
    );
}
