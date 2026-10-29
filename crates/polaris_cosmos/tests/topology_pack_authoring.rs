//! Standalone embodiment pack: direct reporting or a checked conditional route.
//! Only public Cosmos/Polaris surfaces are used; no shared fixture internals.

use polaris::graph::{Graph, GraphExecutor, GraphSignature};
use polaris::system::param::{Out, SystemContext};
use polaris::system::plugin;
use polaris::system::plugin::{Contract, Extends, Plugin, Requires, Version};
use polaris::system::resource::LocalResource;
use polaris::system::server::{Server, ServerBuildError};
use polaris::system::system;
use polaris_cosmos::contracts::{
    AvailabilityObservation, AvailabilitySnapshotId, AvailabilityState, ConfigurationEntry,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ContentDigest,
    ContextTypeId, ContextTypeKey, EnvironmentId, EnvironmentKey, EpistemicBasis, EvidenceTime,
    OutputAvailability, Provenance, RegistrationSelectionRef, ResolvedConfigurationRef,
    RuntimeAvailabilitySnapshot, SourceId, SourceKey, SourceKind,
};
use polaris_cosmos::topology::{
    CandidateRequirements, CompleteTopologyRegistration, RegistrationId, RegistrationProvenance,
    TopologyCatalog, TopologyCatalogError, TopologyCatalogPlugin, TopologyContract,
    TopologyContractId, TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey,
    TypeExplanationRegistry, preflight_catalog,
};

// These domain ports belong to the embodiment, not Cosmos.
#[derive(Clone)]
struct Request(i32);
#[derive(Clone, Debug, PartialEq, Eq)]
struct Report {
    value: i32,
    checked: bool,
}
#[derive(Clone)]
struct Check(bool);

#[system]
async fn direct(request: Out<Request>) -> Report {
    Report {
        value: request.0,
        checked: false,
    }
}
#[system]
async fn check(request: Out<Request>) -> Check {
    Check(request.0 >= 0)
}
#[system]
async fn accepted(request: Out<Request>) -> Report {
    Report {
        value: request.0,
        checked: true,
    }
}
#[system]
async fn rejected() -> Report {
    Report {
        value: 0,
        checked: true,
    }
}

fn version() -> Version {
    Version::new(1, 0, 0)
}
fn type_key(id: &str) -> ContextTypeKey {
    ContextTypeKey::new(ContextTypeId::new(id).unwrap(), version())
}
fn contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new("example.report").unwrap(),
        version(),
    )
}
fn report_contract() -> TopologyContract {
    TopologyContract::new(
        contract_key(),
        GraphSignature::new()
            .require_output::<Request>()
            .produce::<Report>(),
        "Report an integer supplied by the host",
        "A report carries its value and whether a nonnegative check was applied",
    )
    .unwrap()
}
fn explanations() -> TypeExplanationRegistry {
    let mut types = TypeExplanationRegistry::new();
    types
        .bind_output::<Request>(type_key("example.request"))
        .unwrap();
    types
        .bind_output::<Report>(type_key("example.report"))
        .unwrap();
    types
        .bind_output::<Check>(type_key("example.check"))
        .unwrap();
    types
}
fn registration(checked: bool) -> CompleteTopologyRegistration {
    let name = if checked {
        "example.checked"
    } else {
        "example.direct"
    };
    let mut graph = Graph::new();
    if checked {
        graph.add_system(check);
        graph.add_conditional_branch::<Check, _, _, _>(
            "nonnegative",
            |value: &Check| value.0,
            |branch: &mut Graph| {
                branch.add_system(accepted);
            },
            |branch: &mut Graph| {
                branch.add_system(rejected);
            },
        );
    } else {
        graph.add_system(direct);
    }
    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(TopologyId::new(name).unwrap(), version()),
        name,
        if checked {
            "Check sign and clamp negative values to zero"
        } else {
            "Report without checking sign"
        },
    )
    .unwrap()
    .claim(contract_key());
    // Fixed fixture configuration. A production author validates domain settings
    // before constructing this bounded, secret-free canonical value.
    let configuration = ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new("example.report-config").unwrap(),
            version(),
        ),
        ConfigurationValue::map(vec![
            ConfigurationEntry::new("checked", ConfigurationValue::Bool(checked)).unwrap(),
        ])
        .unwrap(),
    )
    .unwrap();
    let selection = RegistrationSelectionRef::new(
        RegistrationId::new(name).unwrap(),
        descriptor.key().clone(),
        ContentDigest::sha256(format!("{name}:implementation-v1").as_bytes()),
        configuration,
    )
    .unwrap();
    CompleteTopologyRegistration::new(
        selection,
        descriptor,
        graph,
        RegistrationProvenance::new("example.report-pack").unwrap(),
        CandidateRequirements::none(),
        explanations(),
        Vec::new(),
    )
}

// A build-time capability can come from any compatible provider. It need not
// be a graph resource or an availability fact when no runtime graph needs it.
struct ReportSettings;
impl LocalResource for ReportSettings {}
impl Contract for ReportSettings {
    const CONTRACT_VERSION: Version = Version::new(1, 0, 0);
}
struct SettingsPlugin;
#[plugin(id = "example::settings", version = "1.0.0", provides(ReportSettings))]
impl Plugin for SettingsPlugin {
    fn build(&self, server: &mut Server) {
        assert!(server.insert_resource(ReportSettings).is_none());
    }
}
struct ReportPack;
#[plugin(id = "example::report_pack", version = "1.0.0")]
impl Plugin for ReportPack {
    fn build(&self, mut catalog: Extends<TopologyCatalog>, _settings: Requires<ReportSettings>) {
        catalog
            .register_contract(report_contract())
            .expect("unique report contract");
        catalog
            .register_complete(registration(false))
            .expect("complete direct graph");
        catalog
            .register_complete(registration(true))
            .expect("complete checked graph");
    }
}
struct AlternateCatalog;
#[plugin(
    id = "example::alternate_catalog",
    version = "1.0.0",
    provides(TopologyCatalog)
)]
impl Plugin for AlternateCatalog {
    fn build(&self, server: &mut Server) {
        assert!(server.insert_resource(TopologyCatalog::new()).is_none());
    }
    async fn ready(&self, server: &mut Server) {
        let mut catalog = server.remove_resource::<TopologyCatalog>().unwrap();
        catalog
            .seal_admission_ready()
            .expect("all registrations complete");
        assert!(server.insert_global(catalog).is_none());
    }
}
async fn assemble(alternate: bool) -> Server {
    let mut server = Server::new();
    server.add_plugins(ReportPack);
    server.add_plugins(SettingsPlugin);
    if alternate {
        server.add_plugins(AlternateCatalog);
    } else {
        server.add_plugins(TopologyCatalogPlugin);
    }
    server.finish().await.unwrap();
    server
}
fn availability(has_request: bool) -> RuntimeAvailabilitySnapshot {
    let provenance = Provenance::new(
        SourceKey::new(SourceId::new("example.host").unwrap(), version()),
        SourceKind::HostObservation,
        EpistemicBasis::Asserted,
    );
    let outputs = if has_request {
        vec![
            OutputAvailability::new(
                type_key("example.request"),
                AvailabilityObservation::new(
                    AvailabilityState::Available,
                    None,
                    None,
                    vec![provenance.clone()],
                )
                .unwrap(),
            )
            .unwrap(),
        ]
    } else {
        Vec::new()
    };
    RuntimeAvailabilitySnapshot::new(
        "67e55044-10b1-426f-8247-bb680e5fe201"
            .parse::<AvailabilitySnapshotId>()
            .unwrap(),
        EvidenceTime::new(None, Some(1000)).unwrap(),
        EnvironmentKey::new(EnvironmentId::new("example.host").unwrap(), version()),
        vec![],
        outputs,
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![provenance],
    )
    .unwrap()
}

#[tokio::test]
async fn pack_registers_distinct_graphs_and_explains_missing_host_input() {
    let server = assemble(false).await;
    let catalog = server.get_global::<TopologyCatalog>().unwrap();
    let snapshot = catalog.snapshot().unwrap();
    assert_eq!(snapshot.candidates().len(), 2);
    let admitted =
        preflight_catalog(&snapshot, &contract_key(), &availability(true), false).unwrap();
    assert!(
        admitted
            .evaluations()
            .iter()
            .all(|entry| entry.result().is_admitted())
    );
    let missing =
        preflight_catalog(&snapshot, &contract_key(), &availability(false), false).unwrap();
    for entry in missing.evaluations() {
        assert_eq!(entry.result().reasons().len(), 1);
        assert_eq!(
            entry.result().reasons()[0].code().as_str(),
            "upstream-output.missing"
        );
    }
    let stale = preflight_catalog(&snapshot, &contract_key(), &availability(true), true).unwrap();
    assert!(stale.evaluations().iter().all(|entry| {
        entry
            .result()
            .reasons()
            .iter()
            .any(|reason| reason.code().as_str() == "availability.snapshot-stale")
    }));

    // This is a graph-behavior test, not a selection host. Production hosts also
    // perform live readmission and consume a StartPermit before execution.
    for candidate in snapshot.candidates() {
        let registered = catalog
            .resolve_at(candidate.registration_id(), snapshot.generation())
            .unwrap();
        assert!(
            registered
                .signature()
                .satisfies(report_contract().signature())
        );
        for input in [-3, 7] {
            let mut context = SystemContext::new();
            context.insert_output(Request(input));
            let result = GraphExecutor::new()
                .execute(registered.graph(), &mut context, None, None)
                .await
                .unwrap();
            let report = result.output::<Report>().unwrap();
            let checked = candidate.registration_id().as_str() == "example.checked";
            assert_eq!(
                *report,
                Report {
                    value: if checked { input.max(0) } else { input },
                    checked
                }
            );
        }
    }
}

#[tokio::test]
async fn catalog_provider_replacement_preserves_pack_and_snapshot() {
    let default = assemble(false).await;
    let alternate = assemble(true).await;
    let snapshot = |server: &Server| {
        server
            .get_global::<TopologyCatalog>()
            .unwrap()
            .snapshot()
            .unwrap()
    };
    assert_eq!(
        snapshot(&default).content_digest(),
        snapshot(&alternate).content_digest()
    );
    assert_eq!(snapshot(&alternate).candidates().len(), 2);
}

#[tokio::test]
async fn missing_pack_capability_fails_before_build() {
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    server.add_plugins(ReportPack);
    let error = server.finish().await.unwrap_err();
    assert!(matches!(error, ServerBuildError::CapabilityConflicts(_)));
    assert!(error.to_string().contains("ReportSettings"));
}

#[test]
fn bad_contract_and_missing_explanation_leave_catalog_unchanged() {
    let mut catalog = TopologyCatalog::new();
    // Same semantic key, wrong native output: names cannot satisfy live types.
    catalog
        .register_contract(
            TopologyContract::new(
                contract_key(),
                GraphSignature::new().produce::<String>(),
                "input",
                "wrong result",
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        catalog.register_complete(registration(false)),
        Err(TopologyCatalogError::UnsatisfiedContract { .. })
    ));
    assert!(catalog.is_empty());

    let mut catalog = TopologyCatalog::new();
    catalog.register_contract(report_contract()).unwrap();
    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(TopologyId::new("example.incomplete").unwrap(), version()),
        "Incomplete",
        "Missing stable boundaries",
    )
    .unwrap()
    .claim(contract_key());
    let mut graph = Graph::new();
    graph.add_system(direct);
    let configuration = ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new("example.empty").unwrap(),
            version(),
        ),
        ConfigurationValue::Null,
    )
    .unwrap();
    let selection = RegistrationSelectionRef::new(
        RegistrationId::new("example.incomplete").unwrap(),
        descriptor.key().clone(),
        ContentDigest::sha256(b"incomplete-v1"),
        configuration,
    )
    .unwrap();
    let result = catalog.register_complete(CompleteTopologyRegistration::new(
        selection,
        descriptor,
        graph,
        RegistrationProvenance::new("example.report-pack").unwrap(),
        CandidateRequirements::none(),
        TypeExplanationRegistry::new(),
        vec![],
    ));
    assert!(matches!(
        result,
        Err(TopologyCatalogError::UnexplainedSignature { .. })
    ));
    assert!(catalog.is_empty());
    catalog.register_complete(registration(false)).unwrap();
    assert_eq!(
        catalog.seal_admission_ready().unwrap().candidates().len(),
        1
    );
}
