use std::collections::{BTreeSet, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;

use polaris::graph::node::Node;
use polaris::graph::{Graph, GraphSignature};
use polaris::system::param::{Out, Res, ResMut, SystemContext};
use polaris::system::plugin::Version;
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris::system::system::erase;
use serde::{Deserialize, Serialize};

use polaris_cosmos::cognition::approved::{
    OracleQualification, ResolutionInput, bounded_correction, coverage_sweep,
};
use polaris_cosmos::cognition::prediction_error_loop;
use polaris_cosmos::contracts::{
    Assessment, AssessmentAccounting, AssessmentConclusion, AssessmentSummary,
    AvailabilityObservation, AvailabilitySnapshotId, AvailabilityState, ConfidencePpm,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ContentDigest,
    ContextTypeId, ContextTypeKey, CoveragePpm, DecompositionShape, DurationMs, EmbodimentId,
    EmbodimentKey, EnvironmentId, EnvironmentKey, EpistemicBasis, EvidenceKind, EvidenceObjectId,
    EvidenceRef, EvidenceStoreId, EvidenceTime, ExecutionProvenance, HardConstraints,
    HostRuntimeId, HostRuntimeKey, LogicalSequence, Measurement, MeasurementEntry, MeasurementId,
    MeasurementValue, MissingPreferencePolicy, ModelId, ModelProvenance, Objective,
    OutcomeObservation, OutputAvailability, PolicyId, PolicyRef, PreferenceBasisPoints,
    PreferenceCriterion, PreferenceWeights, Provenance, RegistrationSelectionRef,
    ResidualDisposition, ResolvedConfigurationRef, ResourceAccess, ResourceAvailability,
    ScoreComponent, ScoreComponentId, ScoreSource, SelectorRef, SourceId, SourceKey, SourceKind,
    StatisticsWindow, Status, StopReason, SubjectSchema, SubjectSchemaId, SubjectSchemaKey,
    SupersedeReason, SupersedesClaim, TaskId, TaskProfile, TaskProfileEvidence, TieBreakPolicy,
    UtilityPpm, VerifierCapability,
};
use polaris_cosmos::evidence::{BoundedEvidenceConfig, StatisticsRequest, TopologyStatsSnapshot};
use polaris_cosmos::selection::deterministic::{
    self, CandidatePolicyFacts, DeterministicSelectorConfig, SelectionInputs,
};
use polaris_cosmos::selection::transparent_profile::{self, ProfileInputs};
use polaris_cosmos::topology::{
    AdmissionCandidate, CandidateAdmissionSet, CandidateRequirements, CompleteTopologyRegistration,
    ConfigurationDigest, RegistrationId, RegistrationProvenance, TopologyCatalog,
    TopologyCatalogSnapshot, TopologyContract, TopologyContractId, TopologyContractKey,
    TopologyDescriptor, TopologyId, TopologyKey, TypeExplanationRegistry, preflight_catalog,
};

pub const DECISION_QUALITY: &str = "87e55044-10b1-426f-8247-bb680e5fe101";
pub const DECISION_NO_ELIGIBLE: &str = "87e55044-10b1-426f-8247-bb680e5fe102";
pub const DECISION_LATER: &str = "87e55044-10b1-426f-8247-bb680e5fe103";
pub const ADMISSION: &str = "87e55044-10b1-426f-8247-bb680e5fe104";
pub const RUN: &str = "87e55044-10b1-426f-8247-bb680e5fe105";
pub const OBSERVATION_PASS: &str = "87e55044-10b1-426f-8247-bb680e5fe106";
pub const OBSERVATION_ISSUE: &str = "87e55044-10b1-426f-8247-bb680e5fe107";

pub const DIRECT_REGISTRATION: &str = "fixtures.direct";
pub const PREDICTION_REGISTRATION: &str = "fixtures.prediction-error";
pub const COVERAGE_REGISTRATION: &str = "fixtures.coverage";
pub const CORRECTION_REGISTRATION: &str = "fixtures.correction";
pub const UNAVAILABLE_REGISTRATION: &str = "fixtures.unavailable-direct";

fn version() -> Version {
    Version::new(1, 0, 0)
}

pub fn policy(id: &str) -> PolicyRef {
    PolicyRef::new(PolicyId::new(id).unwrap(), version())
}

fn evidence(id: &str) -> EvidenceRef {
    EvidenceRef::new(
        EvidenceStoreId::new("fixtures.topology-runtime").unwrap(),
        EvidenceKind::VerifierOutput,
        EvidenceObjectId::new(id).unwrap(),
    )
}

pub fn provenance(id: &str, kind: SourceKind) -> Provenance {
    Provenance::new(
        SourceKey::new(
            SourceId::new("fixtures.topology-runtime").unwrap(),
            version(),
        ),
        kind,
        EpistemicBasis::Verified,
    )
    .with_evidence(evidence(id))
}

fn schema() -> SubjectSchema<Criterion> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("fixtures.runtime-criterion").unwrap(),
            version(),
        ),
        "A domain-neutral criterion used by the topology-runtime fixture.",
        r#"{"type":"object","required":["id","status"]}"#,
        |subject: &Criterion| {
            if subject.id.is_empty() {
                Err("criterion ID must not be empty".to_owned())
            } else {
                Ok(())
            }
        },
    )
    .unwrap()
}

fn qualification() -> OracleQualification {
    OracleQualification::rejected_negative_control(evidence("known-negative-rejected"))
}

fn assessment(subject: Criterion) -> Assessment<Criterion> {
    let status = subject.status;
    Assessment::new(
        &schema(),
        subject,
        status,
        vec![provenance("assessment", SourceKind::AutomatedVerifier)],
    )
    .unwrap()
}

fn summarize(input: &ResolutionInput<Criterion>) -> AssessmentSummary {
    AssessmentSummary::new(
        input.assessments(),
        policy("fixtures.combine"),
        ContentDigest::sha256(b"fixtures-combine-v1"),
        vec![provenance("combination", SourceKind::CosmosPolicy)],
    )
    .unwrap()
}

fn conclude(
    input: &ResolutionInput<Criterion>,
    summary: AssessmentSummary,
) -> AssessmentConclusion {
    let accounting = input
        .assessments()
        .iter()
        .filter(|item| matches!(item.status(), Status::Issue | Status::Indeterminate))
        .map(|item| {
            AssessmentAccounting::new(
                item.content_digest(),
                if input.stop_reason() == StopReason::EscalatedOut {
                    ResidualDisposition::Blocked
                } else {
                    ResidualDisposition::Deferred
                },
                policy("fixtures.resolve"),
                ContentDigest::sha256(b"fixtures-resolve-v1"),
                vec![provenance("accounting", SourceKind::CosmosPolicy)],
            )
            .unwrap()
        })
        .collect();
    AssessmentConclusion::new(
        summary,
        accounting,
        input.stop_reason(),
        input.assessments(),
    )
    .unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Criterion {
    id: String,
    status: Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Observation(String);

#[derive(Debug)]
struct Plan {
    scope: Vec<Criterion>,
    correction: VecDeque<Status>,
    produced: usize,
}
impl LocalResource for Plan {}

#[system]
async fn scope(plan: Res<Plan>) -> Vec<Criterion> {
    plan.scope.clone()
}

#[system]
async fn empty_scope(_plan: Res<Plan>) -> Assessment<Criterion> {
    assessment(Criterion {
        id: "empty".to_owned(),
        status: Status::NotApplicable,
    })
}

#[system]
async fn probe(subject: Out<Criterion>) -> Observation {
    Observation(subject.id.clone())
}

#[system]
async fn adjudicate(
    subject: Out<Criterion>,
    observation: Out<Observation>,
) -> Assessment<Criterion> {
    assert_eq!(subject.id, observation.0);
    assessment(subject.clone())
}

#[system]
async fn assess_gaps(pending: Out<Vec<Criterion>>) -> Vec<Assessment<Criterion>> {
    pending.iter().cloned().map(assessment).collect()
}

#[system]
async fn combine(input: Out<ResolutionInput<Criterion>>) -> AssessmentSummary {
    summarize(&input)
}

#[system]
async fn resolve(
    input: Out<ResolutionInput<Criterion>>,
    summary: Out<AssessmentSummary>,
) -> AssessmentConclusion {
    conclude(&input, summary.clone())
}

#[system]
async fn escalate(
    input: Out<ResolutionInput<Criterion>>,
    summary: Out<AssessmentSummary>,
) -> AssessmentConclusion {
    conclude(&input, summary.clone())
}

#[system]
async fn produce(mut plan: ResMut<Plan>) -> Criterion {
    plan.produced += 1;
    Criterion {
        id: format!("round-{}", plan.produced),
        status: plan.correction.pop_front().unwrap_or(Status::Pass),
    }
}

#[system]
async fn judge(subject: Out<Criterion>) -> Assessment<Criterion> {
    assessment(subject.clone())
}

async fn direct_conclusion() -> AssessmentConclusion {
    let input = ResolutionInput::new(
        vec![assessment(Criterion {
            id: "direct".to_owned(),
            status: Status::Pass,
        })],
        Vec::new(),
        StopReason::PredicateSatisfied,
    );
    let summary = summarize(&input);
    conclude(&input, summary)
}

#[derive(Clone)]
struct Prediction;

#[derive(Clone)]
struct Action;

#[derive(Clone)]
struct Verified;

#[derive(Clone)]
struct Feedback {
    iteration: u8,
}

async fn initialize() -> Feedback {
    Feedback { iteration: 0 }
}

async fn predict() -> Prediction {
    Prediction
}

async fn act() -> Action {
    Action
}

#[system]
async fn verify(_action: Out<Action>) -> Verified {
    Verified
}

#[system]
async fn compare(_prediction: Out<Prediction>, _verified: Out<Verified>) -> Feedback {
    Feedback { iteration: 1 }
}

async fn accept() {}

async fn reject() {}

#[system]
async fn prediction_conclusion(_feedback: Out<Feedback>) -> AssessmentConclusion {
    direct_conclusion().await
}

struct MissingResource;
impl LocalResource for MissingResource {}

#[system]
async fn unavailable_conclusion(_missing: Res<MissingResource>) -> AssessmentConclusion {
    direct_conclusion().await
}

fn direct_descriptor(id: &str, contract: TopologyContractKey) -> TopologyDescriptor {
    TopologyDescriptor::new(
        TopologyKey::new(TopologyId::new(id).unwrap(), version()),
        id,
        "Direct routed embodiment pattern; one non-canonical low-overhead option.",
    )
    .unwrap()
    .claim(contract)
}

fn direct_graph() -> Graph {
    let mut graph = Graph::new();
    graph.add_system(direct_conclusion);
    graph
}

fn prediction_graph() -> Graph {
    let mut graph = Graph::new();
    prediction_error_loop::build(
        &mut graph,
        "fixture_prediction_error",
        prediction_error_loop::Stages::new(initialize, predict, act, verify, compare),
        prediction_error_loop::Policies::new(
            |feedback: &Feedback| feedback.iteration >= 1,
            |_feedback: &Feedback| true,
            |branch: &mut Graph| {
                branch.add_system(accept);
            },
            |branch: &mut Graph| {
                branch.add_system(reject);
            },
        ),
    );
    graph.add_system(prediction_conclusion);
    graph
}

fn coverage_graph() -> Graph {
    let mut graph = Graph::new();
    coverage_sweep::build(
        &mut graph,
        coverage_sweep::Stages {
            scope: erase(scope),
            empty_scope: erase(empty_scope),
            probe: erase(probe),
            adjudicate: erase(adjudicate),
            assess_gaps: erase(assess_gaps),
            combine: erase(combine),
            resolve: erase(resolve),
        },
        qualification(),
    );
    graph
}

fn correction_graph() -> Graph {
    let mut graph = Graph::new();
    bounded_correction::build(
        &mut graph,
        bounded_correction::Stages {
            produce: erase(produce),
            judge: erase(judge),
            combine: erase(combine),
            resolve: erase(resolve),
            escalate: erase(escalate),
        },
        qualification(),
    );
    graph
}

fn unavailable_graph() -> Graph {
    let mut graph = Graph::new();
    graph.add_system(unavailable_conclusion);
    graph
}

fn shape(graph: &Graph) -> (usize, usize, usize, usize) {
    graph.nodes().iter().fold(
        (graph.nodes().len(), 0, 0, 0),
        |(nodes, loops, decisions, switches), node| match node {
            Node::Loop(_) => (nodes, loops + 1, decisions, switches),
            Node::Decision(_) => (nodes, loops, decisions + 1, switches),
            Node::Switch(_) => (nodes, loops, decisions, switches + 1),
            _ => (nodes, loops, decisions, switches),
        },
    )
}

fn contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new("fixtures.assessment-conclusion").unwrap(),
        version(),
    )
}

fn configuration_schema() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("fixtures.topology-configuration").unwrap(),
        version(),
    )
}

fn configuration(id: &str) -> ResolvedConfigurationRef {
    ResolvedConfigurationRef::inline(
        configuration_schema(),
        ConfigurationValue::String(id.to_owned()),
    )
    .unwrap()
}

fn register(
    catalog: &mut TopologyCatalog,
    id: &str,
    descriptor: TopologyDescriptor,
    graph: Graph,
    type_explanations: &TypeExplanationRegistry,
) {
    let registration = RegistrationId::new(id).unwrap();
    let topology = descriptor.key().clone();
    let configuration = configuration(id);
    let selection = RegistrationSelectionRef::new(
        registration,
        topology,
        ContentDigest::sha256(format!("revision:{id}").as_bytes()),
        configuration,
    )
    .unwrap();
    catalog
        .register_complete(CompleteTopologyRegistration::new(
            selection,
            descriptor,
            graph,
            RegistrationProvenance::new("fixtures.approved-cognition-pack").unwrap(),
            CandidateRequirements::none(),
            type_explanations.clone(),
            Vec::new(),
        ))
        .unwrap();
}

fn output_key(id: &str) -> ContextTypeKey {
    ContextTypeKey::new(ContextTypeId::new(id).unwrap(), version())
}

/// The embodiment's own stable identity for the subject it assesses.
fn criterion_key() -> ContextTypeKey {
    output_key("fixtures.criterion")
}

fn type_registry() -> TypeExplanationRegistry {
    let mut registry = TypeExplanationRegistry::new();
    registry
        .bind_resource::<Plan>(output_key("fixtures.plan"))
        .unwrap();
    registry
        .bind_resource::<MissingResource>(output_key("fixtures.missing-resource"))
        .unwrap();
    registry
        .bind_output::<AssessmentConclusion>(output_key("fixtures.assessment-conclusion"))
        .unwrap();
    registry.bind_output::<Criterion>(criterion_key()).unwrap();
    registry
        .bind_output::<Observation>(output_key("fixtures.observation"))
        .unwrap();
    registry
        .bind_output::<Assessment<Criterion>>(output_key("fixtures.criterion-assessment"))
        .unwrap();
    registry
        .bind_output::<Vec<Criterion>>(output_key("fixtures.criterion-list"))
        .unwrap();
    registry
        .bind_output::<Vec<Assessment<Criterion>>>(output_key("fixtures.criterion-assessments"))
        .unwrap();
    registry
        .bind_output::<AssessmentSummary>(output_key("fixtures.assessment-summary"))
        .unwrap();
    coverage_sweep::register_type_explanations::<Criterion>(&mut registry, &criterion_key())
        .unwrap();
    bounded_correction::register_type_explanations::<Criterion>(&mut registry, &criterion_key())
        .unwrap();
    registry
        .bind_output::<Prediction>(output_key("fixtures.prediction"))
        .unwrap();
    registry
        .bind_output::<Action>(output_key("fixtures.action"))
        .unwrap();
    registry
        .bind_output::<Verified>(output_key("fixtures.verified"))
        .unwrap();
    registry
        .bind_output::<Feedback>(output_key("fixtures.feedback"))
        .unwrap();
    registry
}

/// Every Cosmos-owned state resource the two approved points install.
fn cognition_state_keys() -> BTreeSet<ContextTypeKey> {
    coverage_sweep::state_resource_keys(&criterion_key())
        .unwrap()
        .into_iter()
        .chain(bounded_correction::state_resource_keys(&criterion_key()).unwrap())
        .collect()
}

fn available_observation(id: &str) -> AvailabilityObservation {
    AvailabilityObservation::new(
        AvailabilityState::Available,
        None,
        Some(DurationMs::new(10_000)),
        vec![provenance(id, SourceKind::HostObservation)],
    )
    .unwrap()
}

fn availability() -> polaris_cosmos::contracts::RuntimeAvailabilitySnapshot {
    polaris_cosmos::contracts::RuntimeAvailabilitySnapshot::new(
        "87e55044-10b1-426f-8247-bb680e5fe120"
            .parse::<AvailabilitySnapshotId>()
            .unwrap(),
        EvidenceTime::new(None, Some(1_000)).unwrap(),
        EnvironmentKey::new(
            EnvironmentId::new("fixtures.host-environment").unwrap(),
            version(),
        ),
        cognition_state_keys()
            .into_iter()
            .map(|key| {
                let id = key.id().as_str().to_owned();
                ResourceAvailability::new(
                    key,
                    ResourceAccess::Write,
                    available_observation(&format!("availability.{id}")),
                )
                .unwrap()
            })
            .chain([ResourceAvailability::new(
                output_key("fixtures.plan"),
                ResourceAccess::Write,
                available_observation("availability.plan"),
            )
            .unwrap()])
            .collect(),
        [
            "fixtures.prediction",
            "fixtures.action",
            "fixtures.verified",
            "fixtures.feedback",
        ]
        .into_iter()
        .map(|id| {
            OutputAvailability::new(
                output_key(id),
                available_observation(&format!("availability.{id}")),
            )
            .unwrap()
        })
        .collect(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![provenance(
            "availability.snapshot",
            SourceKind::HostObservation,
        )],
    )
    .unwrap()
}

pub struct VerticalFixture {
    pub catalog: TopologyCatalog,
    pub snapshot: TopologyCatalogSnapshot,
    pub contract: TopologyContract,
    pub catalog_digest: ContentDigest,
    pub availability: polaris_cosmos::contracts::RuntimeAvailabilitySnapshot,
    pub candidates: Vec<AdmissionCandidate>,
    pub selections: Vec<RegistrationSelectionRef>,
    pub schemas: BTreeSet<ConfigurationSchemaKey>,
    pub type_registry: TypeExplanationRegistry,
}

impl VerticalFixture {
    pub fn candidate(&self, id: &RegistrationId) -> &AdmissionCandidate {
        self.candidates
            .iter()
            .find(|candidate| candidate.selection().registration_id() == id)
            .unwrap()
    }

    pub fn preflight(&self) -> CandidateAdmissionSet {
        preflight_catalog(
            &self.snapshot,
            self.contract.key(),
            &self.availability,
            false,
        )
        .unwrap()
    }
}

pub fn fixture() -> VerticalFixture {
    let key = contract_key();
    let contract = TopologyContract::new(
        key.clone(),
        GraphSignature::new().produce::<AssessmentConclusion>(),
        "produce an evidence-bearing assessment conclusion",
        "one immutable terminal assessment conclusion",
    )
    .unwrap();
    let direct = direct_graph();
    let prediction = prediction_graph();
    let coverage = coverage_graph();
    let correction = correction_graph();
    let shapes = BTreeSet::from([
        shape(&direct),
        shape(&prediction),
        shape(&coverage),
        shape(&correction),
    ]);
    assert_eq!(
        shapes.len(),
        4,
        "approved registrations must differ in shape"
    );

    let mut catalog = TopologyCatalog::new();
    catalog.register_contract(contract.clone()).unwrap();
    let type_explanations = type_registry();
    register(
        &mut catalog,
        DIRECT_REGISTRATION,
        direct_descriptor(DIRECT_REGISTRATION, key.clone()),
        direct,
        &type_explanations,
    );
    register(
        &mut catalog,
        PREDICTION_REGISTRATION,
        prediction_error_loop::descriptor(key.clone()),
        prediction,
        &type_explanations,
    );
    register(
        &mut catalog,
        COVERAGE_REGISTRATION,
        coverage_sweep::descriptor(key.clone(), qualification()).into_topology(),
        coverage,
        &type_explanations,
    );
    register(
        &mut catalog,
        CORRECTION_REGISTRATION,
        bounded_correction::descriptor(key.clone(), qualification()).into_topology(),
        correction,
        &type_explanations,
    );
    register(
        &mut catalog,
        UNAVAILABLE_REGISTRATION,
        direct_descriptor(UNAVAILABLE_REGISTRATION, key),
        unavailable_graph(),
        &type_explanations,
    );
    let snapshot = catalog.seal_admission_ready().unwrap();
    let candidates = snapshot
        .candidates()
        .iter()
        .map(|native| {
            let metadata = native.admission_metadata().unwrap();
            AdmissionCandidate::new(
                native.clone(),
                metadata.selection().clone(),
                metadata.requirements().clone(),
            )
        })
        .collect::<Vec<_>>();
    let selections = candidates
        .iter()
        .map(|candidate| candidate.selection().clone())
        .collect();
    let schemas = snapshot
        .candidates()
        .iter()
        .map(|candidate| {
            candidate
                .admission_metadata()
                .unwrap()
                .configuration_schema()
                .clone()
        })
        .collect();
    let catalog_digest = snapshot.content_digest();
    VerticalFixture {
        catalog,
        snapshot,
        contract,
        catalog_digest,
        availability: availability(),
        candidates,
        selections,
        schemas,
        type_registry: type_explanations,
    }
}

/// Installs the coverage point's private state and the fixture plan.
///
/// The host owns this step: `prepare_context` is what makes the resources the
/// availability snapshot advertises actually present before a permitted start.
pub fn prepare_coverage_context(context: &mut SystemContext<'_>) {
    coverage_sweep::prepare_context::<Criterion>(
        context,
        coverage_sweep::Config::new(4, 4, 4, StopReason::BudgetExhausted).unwrap(),
    )
    .unwrap();
    context.insert(Plan {
        scope: vec![
            Criterion {
                id: "first".to_owned(),
                status: Status::Pass,
            },
            Criterion {
                id: "second".to_owned(),
                status: Status::Issue,
            },
        ],
        correction: VecDeque::new(),
        produced: 0,
    });
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

pub fn profile() -> TaskProfile {
    transparent_profile::aggregate(
        &ProfileInputs::new(
            TaskId::new("portable", "assess-subject").unwrap(),
            None,
            empty_profile_evidence(),
            version(),
            vec![provenance("profile.policy", SourceKind::CosmosPolicy)],
        )
        .unwrap(),
    )
    .unwrap()
}

pub fn objective(
    quality: u16,
    latency: u16,
    required_verifier: Option<VerifierCapability>,
) -> Objective {
    let weights = PreferenceWeights::new(
        PreferenceBasisPoints::new(quality).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(latency).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
        PreferenceBasisPoints::new(0).unwrap(),
    )
    .unwrap();
    let hard = required_verifier.map_or_else(HardConstraints::none, |required| {
        HardConstraints::none().with_required_verifier_capabilities(vec![required])
    });
    Objective::new(
        hard,
        weights,
        MissingPreferencePolicy::Pessimistic,
        TieBreakPolicy::AscendingRegistrationId,
        vec![provenance("objective.policy", SourceKind::PolicyDefault)],
    )
    .unwrap()
}

fn score_component(
    id: &str,
    criterion: PreferenceCriterion,
    utility: u32,
    statistics: Option<ContentDigest>,
) -> ScoreComponent {
    let measurement = match criterion {
        PreferenceCriterion::Latency => MeasurementValue::Duration(DurationMs::new(utility.into())),
        _ => MeasurementValue::Proportion(CoveragePpm::new(utility).unwrap()),
    };
    ScoreComponent::new(
        ScoreComponentId::new(id).unwrap(),
        criterion,
        if statistics.is_some() {
            ScoreSource::HistoricalStatistics
        } else {
            ScoreSource::AuthorPrior
        },
        Measurement::new(
            measurement,
            None,
            ConfidencePpm::new(850_000).unwrap(),
            vec![provenance(id, SourceKind::CosmosPolicy)],
        )
        .unwrap(),
        policy("fixtures.linear-normalization"),
        UtilityPpm::new(utility).unwrap(),
        ConfidencePpm::new(850_000).unwrap(),
        vec![provenance(id, SourceKind::CosmosPolicy)],
        statistics,
    )
    .unwrap()
}

pub fn policy_facts(
    selections: &[RegistrationSelectionRef],
    statistics: Option<ContentDigest>,
) -> Vec<CandidatePolicyFacts> {
    selections
        .iter()
        .map(|selection| {
            let id = selection.registration_id();
            let mut facts = CandidatePolicyFacts::new(id.clone());
            // Author priors, taken from each point's declared metadata: the
            // exhaustive sweep buys coverage with latency, the prediction-error
            // loop is the cheapest, and bounded correction sits between them.
            let priors = match id.as_str() {
                DIRECT_REGISTRATION => Some((900_000, 100_000)),
                PREDICTION_REGISTRATION => Some((500_000, 950_000)),
                COVERAGE_REGISTRATION => Some((980_000, 50_000)),
                CORRECTION_REGISTRATION => Some((700_000, 400_000)),
                _ => None,
            };
            if let Some((quality, latency)) = priors {
                let mut components = vec![
                    score_component(
                        &format!("{}.quality", id.as_str()),
                        PreferenceCriterion::Quality,
                        quality,
                        None,
                    ),
                    score_component(
                        &format!("{}.latency", id.as_str()),
                        PreferenceCriterion::Latency,
                        latency,
                        None,
                    ),
                ];
                if let Some(digest) = statistics {
                    components.push(score_component(
                        &format!("{}.history", id.as_str()),
                        PreferenceCriterion::VerifiedCoverage,
                        500_000,
                        Some(digest),
                    ));
                }
                facts = facts
                    .with_verifier_capabilities(vec![VerifierCapability::Deterministic])
                    .with_score_components(components);
            }
            facts
        })
        .collect()
}

fn selector_config() -> DeterministicSelectorConfig {
    DeterministicSelectorConfig::new(
        SelectorRef::new(
            deterministic::selector_key(),
            ConfigurationDigest::new(Arc::<str>::from(
                ContentDigest::sha256(b"fixtures.selector-config").to_string(),
            ))
            .unwrap(),
        )
        .unwrap(),
        policy("fixtures.missing-preference"),
        policy("fixtures.confidence"),
        vec![provenance("selector.policy", SourceKind::CosmosPolicy)],
    )
    .unwrap()
}

pub fn selection_inputs(
    fixture: &VerticalFixture,
    decision_id: &str,
    profile: TaskProfile,
    objective: Objective,
    admission: CandidateAdmissionSet,
    statistics: TopologyStatsSnapshot,
    policy_facts: Vec<CandidatePolicyFacts>,
) -> SelectionInputs {
    SelectionInputs::new(
        decision_id.parse().unwrap(),
        profile,
        objective,
        fixture.contract.key().clone(),
        fixture.snapshot.clone(),
        fixture.availability.clone(),
        admission,
        statistics,
        policy_facts,
        selector_config(),
    )
}

pub fn statistics_request() -> StatisticsRequest {
    StatisticsRequest {
        statistics_policy: policy("fixtures.statistics"),
        observation_selection_policy: policy("fixtures.observation-selection"),
        interval_policy: policy("fixtures.wilson-integer"),
        minimum_sample_count: NonZeroUsize::new(1).unwrap(),
    }
}

pub fn evidence_config() -> BoundedEvidenceConfig {
    BoundedEvidenceConfig {
        max_episodes: NonZeroUsize::new(8).unwrap(),
        max_runs: NonZeroUsize::new(8).unwrap(),
        max_observations_total: NonZeroUsize::new(16).unwrap(),
        max_observations_per_run: NonZeroUsize::new(4).unwrap(),
        max_evidence_refs_per_record: NonZeroUsize::new(16).unwrap(),
        max_open_runs: NonZeroUsize::new(4).unwrap(),
    }
}

pub fn execution_provenance() -> ExecutionProvenance {
    let provider = polaris_cosmos::contracts::ProviderKey::new(
        polaris_cosmos::contracts::ProviderId::new("fixtures.model-provider").unwrap(),
        version(),
    );
    ExecutionProvenance::new(
        EmbodimentKey::new(
            EmbodimentId::new("fixtures.portable-embodiment").unwrap(),
            version(),
        ),
        HostRuntimeKey::new(
            HostRuntimeId::new("fixtures.external-harness").unwrap(),
            version(),
        ),
        vec![provider.clone()],
        vec![
            ModelProvenance::new(
                provider,
                ModelId::new("fixtures.reasoning-model").unwrap(),
                Some("fixture-v1".to_owned()),
            )
            .unwrap(),
        ],
        vec![provenance("run.provenance", SourceKind::HostObservation)],
    )
    .unwrap()
}

fn verified_success(status: Status, id: &str) -> MeasurementEntry {
    MeasurementEntry::new(
        MeasurementId::new("verified-success").unwrap(),
        Measurement::new(
            MeasurementValue::Proportion(if status == Status::Pass {
                CoveragePpm::ONE
            } else {
                CoveragePpm::ZERO
            }),
            None,
            ConfidencePpm::new(950_000).unwrap(),
            vec![provenance(id, SourceKind::AutomatedVerifier)],
        )
        .unwrap(),
    )
    .unwrap()
}

pub fn observation(
    id: &str,
    run_id: polaris_cosmos::contracts::RunId,
    task_id: TaskId,
    sequence: u64,
    status: Status,
    supersedes: Option<&str>,
) -> OutcomeObservation {
    OutcomeObservation::new(
        id.parse().unwrap(),
        run_id,
        task_id,
        polaris_cosmos::contracts::RecordedAt::new(LogicalSequence::new(sequence), None),
        None,
        Some(DurationMs::new(sequence * 1_000)),
        status,
        None,
        vec![verified_success(status, &format!("outcome.{sequence}"))],
        None,
        Vec::new(),
        supersedes.map(|earlier| {
            SupersedesClaim::new(
                earlier.parse().unwrap(),
                SupersedeReason::SupersededByLaterEvidence,
            )
        }),
        SourceKey::new(
            SourceId::new("fixtures.topology-runtime").unwrap(),
            version(),
        ),
        SourceKind::AutomatedVerifier,
        ConfidencePpm::new(900_000).unwrap(),
        vec![provenance(
            &format!("outcome.provenance.{sequence}"),
            SourceKind::AutomatedVerifier,
        )],
        Vec::new(),
    )
    .unwrap()
}

pub fn empty_window() -> StatisticsWindow {
    StatisticsWindow::new(LogicalSequence::new(0), LogicalSequence::new(0), 0, 0).unwrap()
}

pub fn profile_shape(profile: &TaskProfile) -> Option<DecompositionShape> {
    profile.features().decomposability.known_value().copied()
}

pub fn profile_has_only_explicit_unknowns(profile: &TaskProfile) -> bool {
    profile.features().decomposability.known_value().is_none()
}
