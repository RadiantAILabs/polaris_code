//! Behavioural tests for validated topology discovery.

use polaris::graph::{Graph, GraphSignature};
use polaris::system::param::{Out, Res};
use polaris::system::plugin::Version;
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris_cosmos::cognition::prediction_error_loop::{self, Policies, Stages};
use polaris_cosmos::contracts::{
    ConfidencePpm, ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue,
    ContentDigest, CoveragePpm, EpistemicBasis, Measurement, MeasurementValue, PolicyId, PolicyRef,
    PreferenceCriterion, Provenance, RegistrationSelectionRef, ResolvedConfigurationRef,
    ScoreComponent, ScoreComponentId, ScoreSource, SourceId, SourceKey, SourceKind, UtilityPpm,
};
use polaris_cosmos::topology::{
    CandidateRequirements, CompleteTopologyRegistration, ConfigurationDigest, RegistrationId,
    RegistrationProvenance, TopologyCatalog, TopologyCatalogError, TopologyContract,
    TopologyContractId, TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey,
    TypeExplanationRegistry,
};

#[derive(Clone)]
struct TaskResult;

#[derive(Clone)]
struct InspectionResult;

struct RunBudget;
impl LocalResource for RunBudget {}

#[derive(Clone)]
struct Seed;

#[derive(Clone)]
struct LoopPrediction(u32);

#[derive(Clone)]
struct LoopAction(u32);

#[derive(Clone)]
struct LoopVerified(u32);

#[derive(Clone)]
struct LoopFeedback {
    done: bool,
}

#[system]
async fn direct() -> TaskResult {
    TaskResult
}

#[system]
async fn budgeted(_budget: Res<RunBudget>) -> TaskResult {
    TaskResult
}

#[system]
async fn inspect() -> InspectionResult {
    InspectionResult
}

#[system]
async fn needs_unsanctioned_seed(_seed: Out<Seed>) -> TaskResult {
    TaskResult
}

#[system]
async fn loop_initialize() -> LoopFeedback {
    LoopFeedback { done: false }
}

#[system]
async fn loop_predict() -> LoopPrediction {
    LoopPrediction(1)
}

#[system]
async fn loop_act(prediction: Out<LoopPrediction>) -> LoopAction {
    LoopAction(prediction.0 + 1)
}

#[system]
async fn loop_verify(action: Out<LoopAction>) -> LoopVerified {
    LoopVerified(action.0)
}

#[system]
async fn loop_compare(
    prediction: Out<LoopPrediction>,
    verified: Out<LoopVerified>,
) -> LoopFeedback {
    LoopFeedback {
        done: verified.0 > prediction.0,
    }
}

#[system]
async fn loop_accept() -> TaskResult {
    TaskResult
}

#[system]
async fn loop_reject() -> TaskResult {
    TaskResult
}

fn id(value: &str) -> TopologyId {
    TopologyId::new(value).expect("test topology ID is valid")
}

fn contract_id(value: &str) -> TopologyContractId {
    TopologyContractId::new(value).expect("test contract ID is valid")
}

fn registration_id(value: &str) -> RegistrationId {
    RegistrationId::new(value).expect("test registration ID is valid")
}

fn digest(value: &str) -> ConfigurationDigest {
    ConfigurationDigest::new(value).expect("test digest is valid")
}

fn provenance() -> RegistrationProvenance {
    RegistrationProvenance::new("tests.topology-pack").expect("test provider is valid")
}

fn solve_contract_key() -> TopologyContractKey {
    TopologyContractKey::new(contract_id("task.solve"), Version::new(1, 0, 0))
}

fn inspect_contract_key() -> TopologyContractKey {
    TopologyContractKey::new(contract_id("task.inspect"), Version::new(1, 0, 0))
}

fn contract(key: TopologyContractKey, signature: GraphSignature) -> TopologyContract {
    TopologyContract::new(key, signature, "one admitted task", "one typed result")
        .expect("test contract metadata is valid")
}

fn descriptor(topology: &str, title: &str, contract: TopologyContractKey) -> TopologyDescriptor {
    TopologyDescriptor::new(
        TopologyKey::new(id(topology), Version::new(1, 0, 0)),
        title,
        "test topology",
    )
    .expect("test descriptor metadata is valid")
    .claim(contract)
}

fn schema_key() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("tests.topology-configuration").unwrap(),
        Version::new(1, 0, 0),
    )
}

fn complete_registration(
    registration: &str,
    descriptor: TopologyDescriptor,
    graph: Graph,
    type_explanations: TypeExplanationRegistry,
    author_priors: Vec<ScoreComponent>,
) -> CompleteTopologyRegistration {
    let registration_id = registration_id(registration);
    let topology_key = descriptor.key().clone();
    let configuration = ResolvedConfigurationRef::inline(
        schema_key(),
        ConfigurationValue::String(registration.to_owned()),
    )
    .unwrap();
    let selection = RegistrationSelectionRef::new(
        registration_id,
        topology_key,
        ContentDigest::sha256(format!("revision:{registration}").as_bytes()),
        configuration,
    )
    .unwrap();
    CompleteTopologyRegistration::new(
        selection,
        descriptor,
        graph,
        provenance(),
        CandidateRequirements::none(),
        type_explanations,
        author_priors,
    )
}

fn output_explanations<T: 'static>(key: &str) -> TypeExplanationRegistry {
    let mut registry = TypeExplanationRegistry::new();
    registry
        .bind_output::<T>(polaris_cosmos::contracts::ContextTypeKey::new(
            polaris_cosmos::contracts::ContextTypeId::new(key).unwrap(),
            Version::new(1, 0, 0),
        ))
        .unwrap();
    registry
}

fn author_prior(id: &str, utility: u32) -> ScoreComponent {
    let provenance = Provenance::new(
        SourceKey::new(
            SourceId::new("tests.topology-author").unwrap(),
            Version::new(1, 0, 0),
        ),
        SourceKind::HumanReview,
        EpistemicBasis::Asserted,
    );
    ScoreComponent::new(
        ScoreComponentId::new(id).unwrap(),
        PreferenceCriterion::Quality,
        ScoreSource::AuthorPrior,
        Measurement::new(
            MeasurementValue::Proportion(CoveragePpm::new(utility).unwrap()),
            None,
            ConfidencePpm::new(800_000).unwrap(),
            vec![provenance.clone()],
        )
        .unwrap(),
        PolicyRef::new(
            PolicyId::new("tests.author-normalization").unwrap(),
            Version::new(1, 0, 0),
        ),
        UtilityPpm::new(utility).unwrap(),
        ConfidencePpm::new(800_000).unwrap(),
        vec![provenance],
        None,
    )
    .unwrap()
}

#[test]
fn registers_heterogeneous_topologies_and_seals_an_immutable_snapshot() {
    let solve = solve_contract_key();
    let inspect_contract = inspect_contract_key();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(
            solve.clone(),
            GraphSignature::new().produce::<TaskResult>(),
        ))
        .unwrap();
    catalog
        .register_contract(contract(
            inspect_contract.clone(),
            GraphSignature::new().produce::<InspectionResult>(),
        ))
        .unwrap();

    let mut direct_graph = Graph::new();
    direct_graph.add_system(direct);
    let direct_id = registration_id("tests.direct");
    let direct_registration = catalog
        .register(
            direct_id.clone(),
            descriptor("cosmos.direct", "Direct", solve.clone()),
            digest("sha256:direct"),
            direct_graph,
            provenance(),
        )
        .unwrap();

    let mut budgeted_graph = Graph::new();
    budgeted_graph.add_system(budgeted);
    let budgeted_id = registration_id("tests.budgeted");
    let budgeted_registration = catalog
        .register(
            budgeted_id.clone(),
            descriptor("cosmos.budgeted", "Budgeted", solve),
            digest("sha256:budgeted"),
            budgeted_graph,
            provenance(),
        )
        .unwrap();

    let mut inspect_graph = Graph::new();
    inspect_graph.add_system(inspect);
    let inspect_id = registration_id("tests.inspect");
    catalog
        .register(
            inspect_id.clone(),
            descriptor("cosmos.inspect", "Inspect", inspect_contract),
            digest("sha256:inspect"),
            inspect_graph,
            provenance(),
        )
        .unwrap();

    assert_ne!(
        direct_registration.signature(),
        budgeted_registration.signature(),
        "embodied registrations may add different resource requirements"
    );
    assert_eq!(catalog.len(), 3);

    let snapshot = catalog.seal();
    assert!(catalog.is_sealed());
    assert_eq!(snapshot.contracts().len(), 2);
    assert_eq!(snapshot.candidates().len(), 3);
    assert_eq!(snapshot.candidates()[0].registration_id(), &budgeted_id);
    assert_eq!(snapshot.candidates()[1].registration_id(), &direct_id);
    assert_eq!(snapshot.candidates()[2].registration_id(), &inspect_id);

    let resolved = catalog
        .resolve_at(&direct_id, snapshot.generation())
        .unwrap();
    assert_eq!(resolved.registration_id(), &direct_id);
    assert_eq!(resolved.graph().node_count(), 1);
    assert!(matches!(
        catalog.resolve_at(&direct_id, snapshot.generation() - 1),
        Err(TopologyCatalogError::StaleGeneration { .. })
    ));

    let late_contract = contract(
        TopologyContractKey::new(contract_id("task.late"), Version::new(1, 0, 0)),
        GraphSignature::new(),
    );
    assert!(matches!(
        catalog.register_contract(late_contract),
        Err(TopologyCatalogError::Sealed)
    ));
}

#[test]
fn registers_an_embodied_prediction_error_loop_with_canonical_metadata() {
    let contract_key = TopologyContractKey::new(
        contract_id("tests.prediction-error-task"),
        Version::new(1, 0, 0),
    );
    let expected_signature = prediction_error_loop::contract_signature::<
        LoopPrediction,
        LoopAction,
        LoopVerified,
        LoopFeedback,
    >();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(
            TopologyContract::new(
                contract_key.clone(),
                expected_signature.clone(),
                "one embodied task processed through prediction error",
                "feedback and the embodiment's accepted or rejected result",
            )
            .unwrap(),
        )
        .unwrap();

    let mut graph = Graph::new();
    prediction_error_loop::build(
        &mut graph,
        "catalog_prediction_error",
        Stages::new(
            loop_initialize,
            loop_predict,
            loop_act,
            loop_verify,
            loop_compare,
        ),
        Policies::new(
            |feedback: &LoopFeedback| feedback.done,
            |feedback: &LoopFeedback| feedback.done,
            |branch: &mut Graph| {
                branch.add_system(loop_accept);
            },
            |branch: &mut Graph| {
                branch.add_system(loop_reject);
            },
        ),
    );

    let registered = catalog
        .register(
            registration_id("tests.prediction-error-loop"),
            prediction_error_loop::descriptor(contract_key),
            digest("sha256:prediction-error-loop"),
            graph,
            provenance(),
        )
        .unwrap();

    assert_eq!(
        registered.descriptor().key(),
        &prediction_error_loop::topology_key()
    );
    assert_eq!(
        registered.descriptor().key().id().as_str(),
        prediction_error_loop::TOPOLOGY_ID
    );
    assert_eq!(
        registered.descriptor().key().version(),
        prediction_error_loop::TOPOLOGY_VERSION
    );
    assert!(registered.signature().satisfies(&expected_signature));

    let snapshot = catalog.seal();
    assert_eq!(snapshot.candidates().len(), 1);
    assert_eq!(
        snapshot.candidates()[0].descriptor().key(),
        &prediction_error_loop::topology_key()
    );
}

#[test]
fn rejects_unknown_contracts_and_duplicate_registration_ids() {
    let solve = solve_contract_key();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(
            solve.clone(),
            GraphSignature::new().produce::<TaskResult>(),
        ))
        .unwrap();

    let mut first_graph = Graph::new();
    first_graph.add_system(direct);
    let duplicate_id = registration_id("tests.duplicate");
    catalog
        .register(
            duplicate_id.clone(),
            descriptor("cosmos.first", "First", solve),
            digest("sha256:first"),
            first_graph,
            provenance(),
        )
        .unwrap();

    let mut duplicate_graph = Graph::new();
    duplicate_graph.add_system(direct);
    assert!(matches!(
        catalog.register(
            duplicate_id,
            descriptor(
                "cosmos.second",
                "Second",
                TopologyContractKey::new(contract_id("task.solve"), Version::new(1, 0, 0)),
            ),
            digest("sha256:second"),
            duplicate_graph,
            provenance(),
        ),
        Err(TopologyCatalogError::DuplicateRegistration { .. })
    ));

    let unknown = TopologyContractKey::new(contract_id("task.unknown"), Version::new(1, 0, 0));
    let mut unknown_graph = Graph::new();
    unknown_graph.add_system(direct);
    assert!(matches!(
        catalog.register(
            registration_id("tests.unknown"),
            descriptor("cosmos.unknown", "Unknown", unknown),
            digest("sha256:unknown"),
            unknown_graph,
            provenance(),
        ),
        Err(TopologyCatalogError::UnknownContract { .. })
    ));
    assert_eq!(catalog.len(), 1, "rejections must not mutate the catalog");
}

#[test]
fn rejects_a_graph_that_does_not_satisfy_its_claimed_contract() {
    let solve = solve_contract_key();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(
            solve.clone(),
            GraphSignature::new().produce::<TaskResult>(),
        ))
        .unwrap();

    let mut graph = Graph::new();
    graph.add_system(needs_unsanctioned_seed);
    let result = catalog.register(
        registration_id("tests.unsatisfied"),
        descriptor("cosmos.unsatisfied", "Unsatisfied", solve),
        digest("sha256:unsatisfied"),
        graph,
        provenance(),
    );

    match result {
        Err(TopologyCatalogError::UnsatisfiedContract { diff, .. }) => {
            assert_eq!(diff.extra_requires_outputs().len(), 1);
        }
        other => panic!("expected an unsatisfied-contract error, got {other:?}"),
    }
    assert!(catalog.is_empty(), "rejection must not mutate the catalog");
}

#[test]
fn rejects_a_native_graph_validation_failure() {
    let solve = solve_contract_key();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(
            solve.clone(),
            GraphSignature::new().produce::<TaskResult>(),
        ))
        .unwrap();

    let result = catalog.register(
        registration_id("tests.empty"),
        descriptor("cosmos.empty", "Empty", solve),
        digest("sha256:empty"),
        Graph::new(),
        provenance(),
    );

    match result {
        Err(TopologyCatalogError::InvalidGraph { validation, .. }) => {
            assert!(validation.is_err());
        }
        other => panic!("expected a native graph-validation error, got {other:?}"),
    }
    assert!(catalog.is_empty(), "rejection must not mutate the catalog");
}

fn complete_snapshot(
    order: &[&str],
    prior_utility: u32,
) -> polaris_cosmos::topology::TopologyCatalogSnapshot {
    let key = TopologyContractKey::new(
        contract_id("tests.complete-registration"),
        Version::new(1, 0, 0),
    );
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(key.clone(), GraphSignature::new()))
        .unwrap();
    for registration in order {
        match *registration {
            "tests.pack-a" => {
                let mut graph = Graph::new();
                graph.add_system(direct);
                catalog
                    .register_complete(complete_registration(
                        registration,
                        descriptor("cosmos.pack-a", "Pack A", key.clone()),
                        graph,
                        output_explanations::<TaskResult>("tests.task-result"),
                        vec![author_prior("tests.pack-a-quality", prior_utility)],
                    ))
                    .unwrap();
            }
            "tests.pack-b" => {
                let mut graph = Graph::new();
                graph.add_system(inspect);
                catalog
                    .register_complete(complete_registration(
                        registration,
                        descriptor("cosmos.pack-b", "Pack B", key.clone()),
                        graph,
                        output_explanations::<InspectionResult>("tests.inspection-result"),
                        Vec::new(),
                    ))
                    .unwrap();
            }
            other => panic!("unexpected fixture registration {other}"),
        }
    }
    catalog.seal_admission_ready().unwrap()
}

#[test]
fn complete_registrations_are_self_contained_and_order_independent() {
    let forward = complete_snapshot(&["tests.pack-a", "tests.pack-b"], 800_000);
    let reverse = complete_snapshot(&["tests.pack-b", "tests.pack-a"], 800_000);
    assert_eq!(forward.content_digest(), reverse.content_digest());
    assert_eq!(
        forward
            .candidates()
            .iter()
            .map(|candidate| candidate.registration_id().as_str())
            .collect::<Vec<_>>(),
        ["tests.pack-a", "tests.pack-b"]
    );

    let metadata = forward.candidates()[0].admission_metadata().unwrap();
    assert_eq!(
        metadata.selection().registration_id(),
        forward.candidates()[0].registration_id()
    );
    assert_eq!(metadata.configuration_schema(), &schema_key());
    assert_eq!(metadata.author_priors().len(), 1);
    assert_eq!(
        metadata.author_priors()[0].source(),
        ScoreSource::AuthorPrior
    );
    assert_eq!(metadata.signature_explanation().produced_outputs().len(), 1);
    assert_ne!(
        forward.content_digest(),
        complete_snapshot(&["tests.pack-a", "tests.pack-b"], 700_000).content_digest(),
        "author priors must participate in catalog content identity"
    );
}

#[test]
fn failed_complete_registration_leaves_no_graph_or_type_binding() {
    let key = TopologyContractKey::new(
        contract_id("tests.atomic-registration"),
        Version::new(1, 0, 0),
    );
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(key.clone(), GraphSignature::new()))
        .unwrap();
    let generation_before = catalog.generation();

    let mut failed_graph = Graph::new();
    failed_graph.add_system(direct);
    failed_graph.add_system(inspect);
    let failed = catalog.register_complete(complete_registration(
        "tests.failed-atomic",
        descriptor("cosmos.failed-atomic", "Failed atomic", key.clone()),
        failed_graph,
        output_explanations::<TaskResult>("tests.shared-output-key"),
        Vec::new(),
    ));
    match failed {
        Err(TopologyCatalogError::UnexplainedSignature {
            registration,
            missing,
        }) => {
            assert_eq!(registration.as_str(), "tests.failed-atomic");
            assert_eq!(missing.len(), 1);
            assert!(missing[0].display_type().contains("InspectionResult"));
        }
        other => panic!("expected complete missing-boundary error, got {other:?}"),
    }
    assert!(catalog.is_empty());
    assert_eq!(catalog.generation(), generation_before);

    let mut replacement_graph = Graph::new();
    replacement_graph.add_system(inspect);
    catalog
        .register_complete(complete_registration(
            "tests.replacement",
            descriptor("cosmos.replacement", "Replacement", key),
            replacement_graph,
            output_explanations::<InspectionResult>("tests.shared-output-key"),
            Vec::new(),
        ))
        .expect("the failed insertion must not leak its staged type binding");
    assert_eq!(catalog.len(), 1);
}

#[test]
fn conflicting_pack_bindings_reject_only_the_second_registration() {
    let key =
        TopologyContractKey::new(contract_id("tests.binding-conflict"), Version::new(1, 0, 0));
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(key.clone(), GraphSignature::new()))
        .unwrap();
    let mut first = Graph::new();
    first.add_system(direct);
    catalog
        .register_complete(complete_registration(
            "tests.binding-first",
            descriptor("cosmos.binding-first", "Binding first", key.clone()),
            first,
            output_explanations::<TaskResult>("tests.conflicting-key"),
            Vec::new(),
        ))
        .unwrap();
    let generation_before = catalog.generation();

    let mut second = Graph::new();
    second.add_system(inspect);
    assert!(matches!(
        catalog.register_complete(complete_registration(
            "tests.binding-second",
            descriptor("cosmos.binding-second", "Binding second", key),
            second,
            output_explanations::<InspectionResult>("tests.conflicting-key"),
            Vec::new(),
        )),
        Err(TopologyCatalogError::AdmissionMetadataConflict { .. })
    ));
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.generation(), generation_before);
}

#[test]
fn admission_ready_seal_rejects_legacy_partial_entries_without_sealing() {
    let key = solve_contract_key();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(contract(
            key.clone(),
            GraphSignature::new().produce::<TaskResult>(),
        ))
        .unwrap();
    let mut graph = Graph::new();
    graph.add_system(direct);
    catalog
        .register(
            registration_id("tests.legacy-partial"),
            descriptor("cosmos.legacy-partial", "Legacy partial", key),
            digest("sha256:legacy-partial"),
            graph,
            provenance(),
        )
        .unwrap();

    assert!(matches!(
        catalog.seal_admission_ready(),
        Err(TopologyCatalogError::IncompleteRegistrations { .. })
    ));
    assert!(!catalog.is_sealed());
    assert!(matches!(
        catalog.snapshot(),
        Err(TopologyCatalogError::NotSealed)
    ));
}

#[test]
fn stable_identifiers_reject_process_local_or_display_oriented_names() {
    assert!(TopologyId::new("cosmos.prediction-error-loop").is_ok());
    assert!(RegistrationProvenance::new("polaris::topology_pack").is_ok());
    assert!(TopologyId::new("").is_err());
    assert!(TopologyId::new("Cosmos::Model<u8>").is_err());
    assert!(RegistrationId::new("tests.registration/").is_err());
}
