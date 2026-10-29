//! Behavioural tests for stable preflight and native live readmission.

use std::collections::BTreeSet;
use std::sync::Arc;

use polaris::graph::{Graph, GraphExecutor, GraphSignature};
use polaris::system::param::{Out, ResMut, SystemContext};
use polaris::system::plugin::Version;
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris_cosmos::contracts::{
    AdmissionAttemptId, AvailabilityObservation, AvailabilitySnapshotId, AvailabilityState,
    BindingAlias, BindingAvailability, BindingKindId, CapabilityAvailability, CapabilityId,
    CapabilityKey, ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue,
    ContentDigest, ContextTypeId, ContextTypeKey, DurationMs, EnvironmentId, EnvironmentKey,
    EpistemicBasis, EvidenceKind, EvidenceObjectId, EvidenceRef, EvidenceStoreId, EvidenceTime,
    LogicalSequence, MechanismAvailability, OutputAvailability, Provenance, ProviderAvailability,
    ProviderId, ProviderKey, RecordedAt, RegistrationSelectionRef, ResolvedConfigurationRef,
    ResourceAccess, ResourceAvailability, RuntimeAvailabilitySnapshot, RuntimeMechanismId,
    RuntimeMechanismKey, SelectionDecisionId, SourceId, SourceKey, SourceKind,
};
use polaris_cosmos::topology::*;

#[derive(Clone)]
struct ModelResource;
impl LocalResource for ModelResource {}

#[derive(Clone)]
struct Seed;

#[derive(Clone)]
struct ResultValue;

#[system]
async fn run_model(_resource: ResMut<ModelResource>, _seed: Out<Seed>) -> ResultValue {
    ResultValue
}

const AVAILABILITY_ID: &str = "67e55044-10b1-426f-8247-bb680e5fe101";
const DECISION_ID: &str = "67e55044-10b1-426f-8247-bb680e5fe102";
const ATTEMPT_ID: &str = "67e55044-10b1-426f-8247-bb680e5fe103";

fn version() -> Version {
    Version::new(1, 0, 0)
}

fn source() -> SourceKey {
    SourceKey::new(SourceId::new("tests.admission-source").unwrap(), version())
}

fn provenance(subject: &str) -> Provenance {
    Provenance::new(
        source(),
        SourceKind::HostObservation,
        EpistemicBasis::Verified,
    )
    .with_evidence(EvidenceRef::new(
        EvidenceStoreId::new("tests.admission-store").unwrap(),
        EvidenceKind::ObservationNote,
        EvidenceObjectId::new(subject).unwrap(),
    ))
}

fn observation(state: AvailabilityState, provider: Option<ProviderKey>) -> AvailabilityObservation {
    AvailabilityObservation::new(
        state,
        provider,
        Some(DurationMs::new(10_000)),
        vec![provenance("availability-observation")],
    )
    .unwrap()
}

fn provider_key() -> ProviderKey {
    ProviderKey::new(ProviderId::new("tests.model-provider").unwrap(), version())
}

fn resource_key() -> ContextTypeKey {
    ContextTypeKey::new(
        ContextTypeId::new("tests.model-resource").unwrap(),
        version(),
    )
}

fn seed_key() -> ContextTypeKey {
    ContextTypeKey::new(ContextTypeId::new("tests.seed-output").unwrap(), version())
}

fn result_key() -> ContextTypeKey {
    ContextTypeKey::new(
        ContextTypeId::new("tests.result-output").unwrap(),
        version(),
    )
}

fn schema_key() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("tests.topology-config").unwrap(),
        version(),
    )
}

fn capability_key() -> CapabilityKey {
    CapabilityKey::new(
        CapabilityId::new("tests.reasoning-capability").unwrap(),
        version(),
    )
}

fn mechanism_key() -> RuntimeMechanismKey {
    RuntimeMechanismKey::new(
        RuntimeMechanismId::new("tests.sequential-runtime").unwrap(),
        version(),
    )
}

fn contract_key(id: &str) -> TopologyContractKey {
    TopologyContractKey::new(TopologyContractId::new(id).unwrap(), version())
}

fn type_registry(include_result: bool) -> TypeExplanationRegistry {
    let mut registry = TypeExplanationRegistry::new();
    registry
        .bind_resource::<ModelResource>(resource_key())
        .unwrap();
    registry.bind_output::<Seed>(seed_key()).unwrap();
    if include_result {
        registry.bind_output::<ResultValue>(result_key()).unwrap();
    }
    registry
}

fn requirements() -> CandidateRequirements {
    CandidateRequirements::new(
        vec![ProviderRequirement::new(
            provider_key(),
            ContentDigest::sha256(b"provider-configuration"),
        )],
        vec![BindingRequirement::new(
            BindingAlias::new("tests.model-binding").unwrap(),
            BindingKindId::new("model-provider").unwrap(),
            provider_key(),
        )],
        vec![capability_key()],
        vec![mechanism_key()],
    )
}

fn availability(
    resource_access: Option<ResourceAccess>,
    include_declared: bool,
) -> RuntimeAvailabilitySnapshot {
    let provider = provider_key();
    let attributed = observation(AvailabilityState::Available, Some(provider.clone()));
    let providers = if include_declared {
        vec![
            ProviderAvailability::new(
                provider.clone(),
                ContentDigest::sha256(b"provider-configuration"),
                attributed.clone(),
            )
            .unwrap(),
        ]
    } else {
        Vec::new()
    };
    let capabilities = if include_declared {
        vec![
            CapabilityAvailability::new(
                capability_key(),
                observation(AvailabilityState::Available, None),
            )
            .unwrap(),
        ]
    } else {
        Vec::new()
    };
    let mechanisms = if include_declared {
        vec![
            MechanismAvailability::new(
                mechanism_key(),
                observation(AvailabilityState::Available, None),
            )
            .unwrap(),
        ]
    } else {
        Vec::new()
    };
    let bindings = if include_declared {
        vec![
            BindingAvailability::new(
                BindingAlias::new("tests.model-binding").unwrap(),
                BindingKindId::new("model-provider").unwrap(),
                provider,
                attributed,
            )
            .unwrap(),
        ]
    } else {
        Vec::new()
    };
    RuntimeAvailabilitySnapshot::new(
        AVAILABILITY_ID.parse::<AvailabilitySnapshotId>().unwrap(),
        EvidenceTime::new(None, Some(1_000)).unwrap(),
        EnvironmentKey::new(EnvironmentId::new("tests.environment").unwrap(), version()),
        resource_access
            .map(|access| {
                vec![
                    ResourceAvailability::new(
                        resource_key(),
                        access,
                        observation(AvailabilityState::Available, None),
                    )
                    .unwrap(),
                ]
            })
            .unwrap_or_default(),
        vec![
            OutputAvailability::new(seed_key(), observation(AvailabilityState::Available, None))
                .unwrap(),
        ],
        providers,
        capabilities,
        mechanisms,
        bindings,
        vec![],
        vec![provenance("availability-snapshot")],
    )
    .unwrap()
}

struct Fixture {
    catalog: TopologyCatalog,
    snapshot: TopologyCatalogSnapshot,
    contract: TopologyContract,
    candidate: AdmissionCandidate,
    selection: RegistrationSelectionRef,
}

fn fixture(configuration_matches: bool) -> Fixture {
    let contract_key = contract_key("tests.solve");
    let signature = GraphSignature::new()
        .require_write::<ModelResource>()
        .require_output::<Seed>()
        .produce::<ResultValue>();
    let contract = TopologyContract::new(
        contract_key.clone(),
        signature,
        "one typed task",
        "one typed result",
    )
    .unwrap();
    let topology_key = TopologyKey::new(TopologyId::new("tests.direct-model").unwrap(), version());
    let registration_id = RegistrationId::new("tests.direct-registration").unwrap();
    let configuration = polaris_cosmos::contracts::ResolvedConfigurationRef::inline(
        schema_key(),
        ConfigurationValue::Map(vec![]),
    )
    .unwrap();
    let registration_configuration =
        ConfigurationDigest::new(Arc::<str>::from(configuration.digest().as_str())).unwrap();
    let mut graph = Graph::new();
    graph.add_system(run_model);
    let descriptor = TopologyDescriptor::new(
        topology_key.clone(),
        "Direct model",
        "Fixture for topology admission",
    )
    .unwrap()
    .claim(contract_key);
    let mut catalog = TopologyCatalog::new();
    catalog.register_contract(contract.clone()).unwrap();
    catalog
        .register(
            registration_id.clone(),
            descriptor,
            registration_configuration,
            graph,
            RegistrationProvenance::new("tests.admission-pack").unwrap(),
        )
        .unwrap();
    let snapshot = catalog.seal();
    let native = snapshot.candidates()[0].clone();
    let selection_configuration = if configuration_matches {
        configuration
    } else {
        polaris_cosmos::contracts::ResolvedConfigurationRef::inline(
            schema_key(),
            ConfigurationValue::String("different".into()),
        )
        .unwrap()
    };
    let selection = RegistrationSelectionRef::new(
        registration_id,
        topology_key,
        ContentDigest::sha256(b"registration-revision"),
        selection_configuration,
    )
    .unwrap();
    let candidate = AdmissionCandidate::new(native, selection.clone(), requirements());
    Fixture {
        catalog,
        snapshot,
        contract,
        candidate,
        selection,
    }
}

fn schemas() -> BTreeSet<ConfigurationSchemaKey> {
    BTreeSet::from([schema_key()])
}

fn reason_codes(result: &polaris_cosmos::contracts::AdmissionResult) -> BTreeSet<&str> {
    result
        .reasons()
        .iter()
        .map(|reason| reason.code().as_str())
        .collect()
}

#[test]
fn stable_type_explanations_are_bijective_and_complete() {
    let mut registry = type_registry(true);
    let explanation = StableSignatureExplanation::derive(
        &GraphSignature::new()
            .require_write::<ModelResource>()
            .require_output::<Seed>()
            .produce::<ResultValue>(),
        &registry,
    )
    .unwrap();
    assert_eq!(explanation.produced_outputs(), &[result_key()]);
    let _empty_requirements = CandidateRequirements::none();
    registry
        .bind_resource::<ModelResource>(resource_key())
        .unwrap();
    assert!(
        registry
            .bind_resource::<ModelResource>(ContextTypeKey::new(
                ContextTypeId::new("tests.other-resource").unwrap(),
                version(),
            ))
            .is_err()
    );
    assert!(registry.bind_resource::<u32>(resource_key()).is_err());

    let fixture = fixture(true);
    assert_eq!(fixture.candidate.selection(), &fixture.selection);
    let set = preflight_candidates(
        &fixture.snapshot,
        ContentDigest::sha256(b"catalog"),
        fixture.contract.key(),
        &availability(Some(ResourceAccess::Write), true),
        false,
        &schemas(),
        &registry,
        vec![fixture.candidate],
    )
    .unwrap();
    assert_eq!(set.requested_contract(), fixture.contract.key());
    assert_eq!(set.catalog_generation(), fixture.snapshot.generation());
    assert_eq!(
        set.catalog_snapshot_digest(),
        ContentDigest::sha256(b"catalog")
    );
    assert_eq!(set.evaluations()[0].candidate(), &fixture.selection);
    assert!(set.evaluations()[0].result().is_admitted());
}

#[test]
fn sealed_catalog_supplies_generic_preflight_inputs_without_parallel_registries() {
    let contract_key = contract_key("tests.complete-solve");
    let contract = TopologyContract::new(
        contract_key.clone(),
        GraphSignature::new()
            .require_write::<ModelResource>()
            .require_output::<Seed>()
            .produce::<ResultValue>(),
        "one typed task",
        "one typed result",
    )
    .unwrap();
    let topology_key = TopologyKey::new(
        TopologyId::new("tests.complete-direct-model").unwrap(),
        version(),
    );
    let registration_id = RegistrationId::new("tests.complete-direct-registration").unwrap();
    let configuration =
        ResolvedConfigurationRef::inline(schema_key(), ConfigurationValue::Map(Vec::new()))
            .unwrap();
    let selection = RegistrationSelectionRef::new(
        registration_id.clone(),
        topology_key.clone(),
        ContentDigest::sha256(b"complete-registration-revision"),
        configuration,
    )
    .unwrap();
    let descriptor = TopologyDescriptor::new(
        topology_key,
        "Complete direct model",
        "Catalog-owned admission inputs",
    )
    .unwrap()
    .claim(contract_key.clone());
    let mut graph = Graph::new();
    graph.add_system(run_model);
    let mut catalog = TopologyCatalog::new();
    catalog.register_contract(contract).unwrap();
    catalog
        .register_complete(CompleteTopologyRegistration::new(
            selection.clone(),
            descriptor,
            graph,
            RegistrationProvenance::new("tests.complete-admission-pack").unwrap(),
            requirements(),
            type_registry(true),
            Vec::new(),
        ))
        .unwrap();
    let snapshot = catalog.seal_admission_ready().unwrap();

    let set = preflight_catalog(
        &snapshot,
        &contract_key,
        &availability(Some(ResourceAccess::Write), true),
        false,
    )
    .unwrap();
    assert_eq!(set.catalog_snapshot_digest(), snapshot.content_digest());
    assert_eq!(set.evaluations()[0].candidate(), &selection);
    assert!(set.evaluations()[0].result().is_admitted());
    let mut explanations = catalog.type_explanations().clone();
    assert!(
        explanations
            .bind_output::<ResultValue>(result_key())
            .is_ok()
    );
}

#[test]
fn preflight_collects_every_exclusion_family_without_short_circuiting() {
    let fixture = fixture(false);
    let unavailable = availability(Some(ResourceAccess::Read), false);
    let set = preflight_candidates(
        &fixture.snapshot,
        ContentDigest::sha256(b"catalog"),
        &contract_key("tests.other-contract"),
        &unavailable,
        true,
        &BTreeSet::new(),
        &type_registry(true),
        vec![fixture.candidate],
    )
    .unwrap();
    let codes = reason_codes(set.evaluations()[0].result());
    for expected in [
        "availability.snapshot-stale",
        "capability.missing",
        "configuration.binding-unavailable",
        "configuration.digest-mismatch",
        "configuration.schema-unknown",
        "contract.not-requested",
        "mechanism.unsupported",
        "provider.unavailable",
        "resource.access-insufficient",
    ] {
        assert!(codes.contains(expected), "missing {expected}: {codes:?}");
    }
}

#[test]
fn unexplained_native_types_are_exclusions_not_persisted_type_names() {
    let missing = admission::StableSignatureExplanation::derive(
        &GraphSignature::new().produce::<ResultValue>(),
        &type_registry(false),
    )
    .unwrap_err();
    assert_eq!(missing[0].channel(), SignatureChannel::Output);
    assert!(missing[0].display_type().contains("ResultValue"));
    let fixture = fixture(true);
    let set = preflight_candidates(
        &fixture.snapshot,
        ContentDigest::sha256(b"catalog"),
        fixture.contract.key(),
        &availability(Some(ResourceAccess::Write), true),
        false,
        &schemas(),
        &type_registry(false),
        vec![fixture.candidate],
    )
    .unwrap();
    let reasons = set.evaluations()[0].result().reasons();
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code().as_str(), "type-explanation.missing");
    assert_eq!(
        reasons[0].subjects()[0].as_str(),
        "tests.direct-registration"
    );
    assert!(reasons[0].detail().unwrap().contains("ResultValue"));
}

#[test]
fn live_readmission_defers_to_native_context_validation() {
    let fixture = fixture(true);
    let availability = availability(Some(ResourceAccess::Write), true);
    let context = SystemContext::new();
    let result = readmit_live(
        &fixture.catalog,
        fixture.snapshot.generation(),
        ContentDigest::sha256(b"catalog"),
        ContentDigest::sha256(b"catalog"),
        &fixture.candidate,
        &fixture.contract,
        &availability,
        &schemas(),
        &type_registry(true),
        &GraphExecutor::new(),
        &context,
        DECISION_ID.parse::<SelectionDecisionId>().unwrap(),
        ATTEMPT_ID.parse::<AdmissionAttemptId>().unwrap(),
        RecordedAt::new(LogicalSequence::new(1), Some(1_000)),
    )
    .unwrap();
    assert!(!result.is_permitted());
    let codes = reason_codes(result.evaluation().result());
    assert!(codes.contains("resource.missing"));
    assert!(codes.contains("upstream-output.missing"));
}

#[test]
fn successful_live_readmission_issues_a_context_bound_single_use_permit() {
    let fixture = fixture(true);
    let availability = availability(Some(ResourceAccess::Write), true);
    let mut context = SystemContext::new().with(ModelResource);
    context.insert_output(Seed);
    let decision_id = DECISION_ID.parse::<SelectionDecisionId>().unwrap();
    let result = readmit_live(
        &fixture.catalog,
        fixture.snapshot.generation(),
        ContentDigest::sha256(b"catalog"),
        ContentDigest::sha256(b"catalog"),
        &fixture.candidate,
        &fixture.contract,
        &availability,
        &schemas(),
        &type_registry(true),
        &GraphExecutor::new(),
        &context,
        decision_id,
        ATTEMPT_ID.parse::<AdmissionAttemptId>().unwrap(),
        RecordedAt::new(LogicalSequence::new(1), Some(1_000)),
    )
    .unwrap();
    assert!(result.evaluation().is_admitted());
    let (_, permit) = result.into_parts();
    let started = permit
        .unwrap()
        .consume(decision_id, &fixture.selection, &context)
        .unwrap();
    assert_eq!(started, fixture.selection);
}

#[test]
fn freshly_materialized_graphs_receive_the_same_context_bound_start_protection() {
    let fixture = fixture(true);
    let registration = fixture
        .catalog
        .resolve_at(
            fixture.candidate.selection().registration_id(),
            fixture.snapshot.generation(),
        )
        .unwrap();
    let mut context = SystemContext::new().with(ModelResource);
    context.insert_output(Seed);
    let decision_id = DECISION_ID.parse::<SelectionDecisionId>().unwrap();
    let attempt_id = ATTEMPT_ID.parse::<AdmissionAttemptId>().unwrap();
    let permit = readmit_materialized_graph(
        registration.graph(),
        &GraphExecutor::new(),
        &context,
        decision_id,
        attempt_id,
    )
    .unwrap();

    permit
        .consume(decision_id, attempt_id, registration.graph(), &context)
        .unwrap();
}

#[test]
fn materialized_graph_admission_refuses_invalid_graphs_and_live_requirements() {
    let executor = GraphExecutor::new();
    let context = SystemContext::new();
    let decision_id = DECISION_ID.parse::<SelectionDecisionId>().unwrap();
    let attempt_id = ATTEMPT_ID.parse::<AdmissionAttemptId>().unwrap();

    let empty = Graph::new();
    assert!(matches!(
        readmit_materialized_graph(&empty, &executor, &context, decision_id, attempt_id),
        Err(MaterializedGraphAdmissionError::GraphValidation(_))
    ));

    let mut requires_context = Graph::new();
    requires_context.add_system(run_model);
    assert!(matches!(
        readmit_materialized_graph(
            &requires_context,
            &executor,
            &context,
            decision_id,
            attempt_id,
        ),
        Err(MaterializedGraphAdmissionError::ResourceValidation(errors)) if !errors.is_empty()
    ));
}

#[test]
fn materialized_start_permit_rejects_changed_start_identity() {
    let fixture = fixture(true);
    let registration = fixture
        .catalog
        .resolve_at(
            fixture.candidate.selection().registration_id(),
            fixture.snapshot.generation(),
        )
        .unwrap();
    let mut context = SystemContext::new().with(ModelResource);
    context.insert_output(Seed);
    let decision_id = DECISION_ID.parse::<SelectionDecisionId>().unwrap();
    let attempt_id = ATTEMPT_ID.parse::<AdmissionAttemptId>().unwrap();
    let executor = GraphExecutor::new();

    let permit = readmit_materialized_graph(
        registration.graph(),
        &executor,
        &context,
        decision_id,
        attempt_id,
    )
    .unwrap();
    assert_eq!(
        permit.consume(
            "67e55044-10b1-426f-8247-bb680e5fe104".parse().unwrap(),
            attempt_id,
            registration.graph(),
            &context,
        ),
        Err(MaterializedStartPermitError::Decision)
    );

    let permit = readmit_materialized_graph(
        registration.graph(),
        &executor,
        &context,
        decision_id,
        attempt_id,
    )
    .unwrap();
    assert_eq!(
        permit.consume(
            decision_id,
            "67e55044-10b1-426f-8247-bb680e5fe104".parse().unwrap(),
            registration.graph(),
            &context,
        ),
        Err(MaterializedStartPermitError::Attempt)
    );

    let permit = readmit_materialized_graph(
        registration.graph(),
        &executor,
        &context,
        decision_id,
        attempt_id,
    )
    .unwrap();
    let other_graph = Graph::new();
    assert_eq!(
        permit.consume(decision_id, attempt_id, &other_graph, &context),
        Err(MaterializedStartPermitError::GraphIdentity)
    );

    let permit = readmit_materialized_graph(
        registration.graph(),
        &executor,
        &context,
        decision_id,
        attempt_id,
    )
    .unwrap();
    let mut other_context = SystemContext::new().with(ModelResource);
    other_context.insert_output(Seed);
    assert_eq!(
        permit.consume(
            decision_id,
            attempt_id,
            registration.graph(),
            &other_context,
        ),
        Err(MaterializedStartPermitError::ContextIdentity)
    );
}

#[test]
fn permits_and_fallbacks_reject_changed_pre_start_identity() {
    let fixture = fixture(true);
    let availability = availability(Some(ResourceAccess::Write), true);
    let mut context = SystemContext::new().with(ModelResource);
    context.insert_output(Seed);
    let result = readmit_live(
        &fixture.catalog,
        fixture.snapshot.generation(),
        ContentDigest::sha256(b"catalog"),
        ContentDigest::sha256(b"catalog"),
        &fixture.candidate,
        &fixture.contract,
        &availability,
        &schemas(),
        &type_registry(true),
        &GraphExecutor::new(),
        &context,
        DECISION_ID.parse().unwrap(),
        ATTEMPT_ID.parse().unwrap(),
        RecordedAt::new(LogicalSequence::new(1), Some(1_000)),
    )
    .unwrap();
    let (_, permit) = result.into_parts();
    assert_eq!(
        permit.unwrap().consume(
            "67e55044-10b1-426f-8247-bb680e5fe104".parse().unwrap(),
            &fixture.selection,
            &context,
        ),
        Err(StartPermitError::Decision)
    );

    assert_eq!(
        validate_fallback_plan(
            ContentDigest::sha256(b"catalog"),
            ContentDigest::sha256(b"catalog"),
            std::slice::from_ref(&fixture.selection),
            std::slice::from_ref(&fixture.selection),
        ),
        FallbackValidity::ReuseCapturedOrder
    );
    assert_eq!(
        validate_fallback_plan(
            ContentDigest::sha256(b"catalog"),
            ContentDigest::sha256(b"changed-catalog"),
            std::slice::from_ref(&fixture.selection),
            std::slice::from_ref(&fixture.selection),
        ),
        FallbackValidity::Reselect
    );
}
