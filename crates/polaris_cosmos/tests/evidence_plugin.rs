//! Plugin-manifest and lifecycle proof for the default evidence provider.

use std::any::TypeId;

use polaris::system::plugin::{Contract, Plugin, PluginId};
use polaris::system::server::Server;
use polaris_cosmos::evidence::{TopologyEvidence, TopologyEvidencePlugin};

#[tokio::test]
async fn default_plugin_publishes_the_replaceable_evidence_contract() {
    let mut server = Server::new();
    server.add_plugins(TopologyEvidencePlugin::default());
    server.finish().await.unwrap();

    let evidence = server.get_global::<TopologyEvidence>().unwrap();
    assert_eq!(evidence.store_status().unwrap().retained_episodes, 0);

    let manifest = server.plugin_manifest();
    let entry = manifest
        .entry(&PluginId::new(TopologyEvidencePlugin::ID))
        .unwrap();
    assert_eq!(entry.provides().len(), 1);
    assert_eq!(
        entry.provides()[0].type_id(),
        TypeId::of::<TopologyEvidence>()
    );
    assert_eq!(
        entry.provides()[0].version(),
        TopologyEvidence::CONTRACT_VERSION
    );
}
