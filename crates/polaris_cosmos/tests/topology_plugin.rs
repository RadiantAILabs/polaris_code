//! Behavioural tests for plugin-native topology discovery.

use std::any::TypeId;

use polaris::graph::Graph;
use polaris::system::param::Out;
use polaris::system::plugin;
use polaris::system::plugin::{Contract, Extends, Plugin, PluginId, Version};
use polaris::system::server::{Server, ServerBuildError};
use polaris::system::system;
use polaris_cosmos::cognition::prediction_error_loop::{self, Policies, Stages};
use polaris_cosmos::contracts::{
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ContentDigest,
    ContextTypeId, ContextTypeKey, RegistrationSelectionRef, ResolvedConfigurationRef,
};
use polaris_cosmos::topology::{
    CandidateRequirements, CompleteTopologyRegistration, RegistrationId, RegistrationProvenance,
    TopologyCatalog, TopologyCatalogPlugin, TopologyContract, TopologyContractId,
    TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey, TypeExplanationRegistry,
};

#[derive(Clone)]
struct Prediction(u32);

#[derive(Clone)]
struct Action(u32);

#[derive(Clone)]
struct Verification(u32);

#[derive(Clone)]
struct Feedback {
    complete: bool,
}

#[derive(Clone)]
struct Accepted;

#[derive(Clone)]
struct Reviewed;

#[system]
async fn initialize() -> Feedback {
    Feedback { complete: false }
}

#[system]
async fn predict() -> Prediction {
    Prediction(1)
}

#[system]
async fn act(prediction: Out<Prediction>) -> Action {
    Action(prediction.0 + 1)
}

#[system]
async fn verify(action: Out<Action>) -> Verification {
    Verification(action.0)
}

#[system]
async fn compare(prediction: Out<Prediction>, verification: Out<Verification>) -> Feedback {
    Feedback {
        complete: verification.0 > prediction.0,
    }
}

#[system]
async fn accept() -> Accepted {
    Accepted
}

#[system]
async fn reject() -> Accepted {
    Accepted
}

#[system]
async fn review() -> Reviewed {
    Reviewed
}

fn version() -> Version {
    Version::new(1, 0, 0)
}

fn contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new("tests.plugin.prediction-error-task")
            .expect("test contract ID is valid"),
        Version::new(1, 0, 0),
    )
}

fn output_key(id: &str) -> ContextTypeKey {
    ContextTypeKey::new(ContextTypeId::new(id).unwrap(), version())
}

fn configuration(id: &str) -> ResolvedConfigurationRef {
    ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new("tests.plugin.configuration").unwrap(),
            version(),
        ),
        ConfigurationValue::String(id.to_owned()),
    )
    .unwrap()
}

fn build_prediction_error_registration(catalog: &mut TopologyCatalog) {
    let contract_key = contract_key();
    catalog
        .register_contract(
            TopologyContract::new(
                contract_key.clone(),
                prediction_error_loop::contract_signature::<
                    Prediction,
                    Action,
                    Verification,
                    Feedback,
                >(),
                "one test task processed through prediction error",
                "one accepted or rejected test result",
            )
            .expect("test contract metadata is valid"),
        )
        .expect("test pack registers a unique contract");

    let mut graph = Graph::new();
    prediction_error_loop::build(
        &mut graph,
        "plugin_prediction_error",
        Stages::new(initialize, predict, act, verify, compare),
        Policies::new(
            |feedback: &Feedback| feedback.complete,
            |feedback: &Feedback| feedback.complete,
            |branch: &mut Graph| {
                branch.add_system(accept);
            },
            |branch: &mut Graph| {
                branch.add_system(reject);
            },
        ),
    );

    let registration_id = RegistrationId::new("tests.plugin.prediction-error-loop")
        .expect("test registration ID is valid");
    let descriptor = prediction_error_loop::descriptor(contract_key);
    let configuration = configuration(registration_id.as_str());
    let selection = RegistrationSelectionRef::new(
        registration_id,
        descriptor.key().clone(),
        ContentDigest::sha256(b"tests.plugin.prediction-error-loop-revision"),
        configuration,
    )
    .unwrap();
    let mut explanations = TypeExplanationRegistry::new();
    explanations
        .bind_output::<Prediction>(output_key("tests.plugin.prediction"))
        .unwrap();
    explanations
        .bind_output::<Action>(output_key("tests.plugin.action"))
        .unwrap();
    explanations
        .bind_output::<Verification>(output_key("tests.plugin.verification"))
        .unwrap();
    explanations
        .bind_output::<Feedback>(output_key("tests.plugin.feedback"))
        .unwrap();
    explanations
        .bind_output::<Accepted>(output_key("tests.plugin.accepted"))
        .unwrap();
    catalog
        .register_complete(CompleteTopologyRegistration::new(
            selection,
            descriptor,
            graph,
            RegistrationProvenance::new("tests.topology-pack").expect("test provider ID is valid"),
            CandidateRequirements::none(),
            explanations,
            Vec::new(),
        ))
        .expect("test prediction-error graph satisfies its contract");
}

fn build_review_registration(catalog: &mut TopologyCatalog) {
    let contract_key = TopologyContractKey::new(
        TopologyContractId::new("tests.plugin.review-task").unwrap(),
        version(),
    );
    catalog
        .register_contract(
            TopologyContract::new(
                contract_key.clone(),
                polaris::graph::GraphSignature::new().produce::<Reviewed>(),
                "one review task",
                "one review result",
            )
            .unwrap(),
        )
        .unwrap();
    let mut graph = Graph::new();
    graph.add_system(review);
    let registration_id = RegistrationId::new("tests.plugin.review").unwrap();
    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(TopologyId::new("tests.plugin.review").unwrap(), version()),
        "Review",
        "Independent second-pack registration",
    )
    .unwrap()
    .claim(contract_key);
    let selection = RegistrationSelectionRef::new(
        registration_id,
        descriptor.key().clone(),
        ContentDigest::sha256(b"tests.plugin.review-revision"),
        configuration("tests.plugin.review"),
    )
    .unwrap();
    catalog
        .register_complete(CompleteTopologyRegistration::new(
            selection,
            descriptor,
            graph,
            RegistrationProvenance::new("tests.review-pack").unwrap(),
            CandidateRequirements::none(),
            output_explanations_for_review(),
            Vec::new(),
        ))
        .unwrap();
}

fn output_explanations_for_review() -> TypeExplanationRegistry {
    let mut explanations = TypeExplanationRegistry::new();
    explanations
        .bind_output::<Reviewed>(output_key("tests.plugin.reviewed"))
        .unwrap();
    explanations
}

struct TestTopologyPackPlugin;

#[plugin(id = "tests::topology_pack", version = "0.1.0")]
impl Plugin for TestTopologyPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        build_prediction_error_registration(&mut catalog);
    }
}

struct ReviewTopologyPackPlugin;

#[plugin(id = "tests::review_topology_pack", version = "0.1.0")]
impl Plugin for ReviewTopologyPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        build_review_registration(&mut catalog);
    }
}

struct MalformedTopologyPackPlugin;

#[plugin(id = "tests::malformed_topology_pack", version = "0.1.0")]
impl Plugin for MalformedTopologyPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        let contract_key = TopologyContractKey::new(
            TopologyContractId::new("tests.plugin.malformed-task").unwrap(),
            version(),
        );
        catalog
            .register_contract(
                TopologyContract::new(
                    contract_key.clone(),
                    polaris::graph::GraphSignature::new().produce::<Reviewed>(),
                    "one malformed task",
                    "one unreachable result",
                )
                .unwrap(),
            )
            .unwrap();
        let mut graph = Graph::new();
        graph.add_system(review);
        let registration_id = RegistrationId::new("tests.plugin.malformed").unwrap();
        let descriptor = TopologyDescriptor::new(
            TopologyKey::new(
                TopologyId::new("tests.plugin.malformed").unwrap(),
                version(),
            ),
            "Malformed",
            "Complete registration missing its type explanation",
        )
        .unwrap()
        .claim(contract_key);
        let registration = CompleteTopologyRegistration::new(
            RegistrationSelectionRef::new(
                registration_id,
                descriptor.key().clone(),
                ContentDigest::sha256(b"tests.plugin.malformed-revision"),
                configuration("tests.plugin.malformed"),
            )
            .unwrap(),
            descriptor,
            graph,
            RegistrationProvenance::new("tests.malformed-pack").unwrap(),
            CandidateRequirements::none(),
            TypeExplanationRegistry::new(),
            Vec::new(),
        );
        if let Err(error) = catalog.register_complete(registration) {
            panic!("malformed pack registration failed: {error}");
        }
    }
}

struct ReplacementCatalogPlugin;

#[plugin(
    id = "tests::replacement_topology_catalog",
    version = "0.1.0",
    provides(TopologyCatalog)
)]
impl Plugin for ReplacementCatalogPlugin {
    fn build(&self, server: &mut Server) {
        assert!(server.insert_resource(TopologyCatalog::new()).is_none());
    }

    async fn ready(&self, server: &mut Server) {
        let mut catalog = server
            .remove_resource::<TopologyCatalog>()
            .expect("replacement provider inserted the catalog");
        catalog
            .seal_admission_ready()
            .expect("replacement provider requires complete registrations");
        assert!(server.insert_global(catalog).is_none());
    }
}

#[tokio::test]
async fn provider_orders_pack_then_seals_and_globalizes_its_registration() {
    let mut server = Server::new();
    server.add_plugins(TestTopologyPackPlugin);
    server.add_plugins(TopologyCatalogPlugin);
    server
        .finish()
        .await
        .expect("catalog plugins should resolve");

    let catalog = server
        .get_global::<TopologyCatalog>()
        .expect("provider globalizes the topology catalog");
    assert!(catalog.is_sealed());
    assert_eq!(catalog.len(), 1);

    let snapshot = catalog.snapshot().expect("global catalog is sealed");
    assert_eq!(snapshot.contracts().len(), 1);
    assert_eq!(snapshot.candidates().len(), 1);
    assert_eq!(
        snapshot.candidates()[0].descriptor().key(),
        &prediction_error_loop::topology_key()
    );

    let manifest = server.plugin_manifest();
    let provider_id = PluginId::new(TopologyCatalogPlugin::ID);
    let provider = manifest
        .entry(&provider_id)
        .expect("catalog provider appears in the manifest");
    assert_eq!(provider.provides().len(), 1);
    assert_eq!(
        provider.provides()[0].type_id(),
        TypeId::of::<TopologyCatalog>()
    );
    assert_eq!(
        provider.provides()[0].version(),
        TopologyCatalog::CONTRACT_VERSION
    );

    let pack = manifest
        .entry(&PluginId::new(TestTopologyPackPlugin::ID))
        .expect("topology pack appears in the manifest");
    assert_eq!(pack.extends().len(), 1);
    assert_eq!(pack.extends()[0].provider(), Some(&provider_id));
    assert_eq!(
        pack.extends()[0].provider_version(),
        Some(TopologyCatalog::CONTRACT_VERSION)
    );
}

#[tokio::test]
async fn pack_binds_to_the_catalog_capability_not_the_default_provider() {
    let mut server = Server::new();
    server.add_plugins(TestTopologyPackPlugin);
    server.add_plugins(ReplacementCatalogPlugin);
    server
        .finish()
        .await
        .expect("replacement provider should satisfy the catalog capability");

    let catalog = server
        .get_global::<TopologyCatalog>()
        .expect("replacement provider globalizes the catalog");
    assert!(catalog.is_sealed());
    assert_eq!(catalog.len(), 1);

    let pack = server
        .plugin_manifest()
        .entry(&PluginId::new(TestTopologyPackPlugin::ID))
        .expect("topology pack appears in the manifest");
    assert_eq!(
        pack.extends()[0].provider(),
        Some(&PluginId::new(ReplacementCatalogPlugin::ID))
    );
}

async fn assembled_multi_pack_snapshot(reverse: bool) -> (ContentDigest, Vec<String>) {
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    if reverse {
        server.add_plugins(ReviewTopologyPackPlugin);
        server.add_plugins(TestTopologyPackPlugin);
    } else {
        server.add_plugins(TestTopologyPackPlugin);
        server.add_plugins(ReviewTopologyPackPlugin);
    }
    server.finish().await.unwrap();
    let snapshot = server
        .get_global::<TopologyCatalog>()
        .unwrap()
        .snapshot()
        .unwrap();
    (
        snapshot.content_digest(),
        snapshot
            .candidates()
            .iter()
            .map(|candidate| candidate.registration_id().as_str().to_owned())
            .collect(),
    )
}

#[tokio::test]
async fn two_independent_packs_share_one_order_independent_sealed_catalog() {
    let forward = assembled_multi_pack_snapshot(false).await;
    let reverse = assembled_multi_pack_snapshot(true).await;
    assert_eq!(forward, reverse);
    assert_eq!(
        forward.1,
        ["tests.plugin.prediction-error-loop", "tests.plugin.review"]
    );
}

#[tokio::test]
#[should_panic(expected = "unexplained signature boundary")]
async fn malformed_pack_stops_provider_assembly_before_runtime_preflight() {
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    server.add_plugins(MalformedTopologyPackPlugin);
    let _ = server.finish().await;
}

#[tokio::test]
async fn pack_without_a_catalog_provider_is_a_capability_resolution_error() {
    let mut server = Server::new();
    server.add_plugins(TestTopologyPackPlugin);

    let error = server
        .finish()
        .await
        .expect_err("an extender without a provider must fail before build");
    assert!(matches!(error, ServerBuildError::CapabilityConflicts(_)));
    assert!(error.to_string().contains("no plugin provides it"));
    assert!(error.to_string().contains("TopologyCatalog"));
}
