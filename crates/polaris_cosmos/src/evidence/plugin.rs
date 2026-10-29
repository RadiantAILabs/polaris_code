//! Plugin lifecycle for the default topology-evidence provider.

use std::sync::Arc;

use super::{BoundedEvidenceConfig, BoundedInMemoryEvidence, TopologyEvidence};
use polaris::system::plugin;
use polaris::system::plugin::{Contract, Plugin, Version};
use polaris::system::resource::GlobalResource;

impl GlobalResource for TopologyEvidence {}

impl Contract for TopologyEvidence {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

/// Provides bounded in-memory topology evidence for hosts that need a trivial default.
///
/// Reach for this plugin when a Polaris server needs one process-local evidence
/// capability and durable cross-restart retention is not required. A replacement
/// plugin can provide the same [`TopologyEvidence`] contract around another
/// [`TopologyEvidenceProvider`](super::TopologyEvidenceProvider).
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`TopologyEvidence`] | Global | Cloneable facade over one bounded, append-only in-memory provider. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Systems consume the facade through `Res<TopologyEvidence>`; hosts may also retain a clone. |
///
/// # Dependencies
///
/// None.
///
/// # Axis
///
/// This is the non-durable, bounded in-memory point on the topology-evidence
/// storage axis. Durable providers remain replaceable implementations of the
/// same provider and plugin capability contracts.
///
/// # Lifecycle
///
/// - **`build()`** — creates one provider with the configured bounds and inserts
///   its facade as a global resource.
/// - The plugin starts no task and interprets no outcome. Hosts and recorders
///   append explicit typed events through the facade.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_cosmos::evidence::{TopologyEvidence, TopologyEvidencePlugin};
///
/// # async fn assemble() {
/// let mut server = Server::new();
/// server.add_plugins(TopologyEvidencePlugin::default());
/// server.finish().await.expect("evidence plugin should resolve");
///
/// let evidence = server
///     .get_global::<TopologyEvidence>()
///     .expect("the finished server exposes topology evidence");
/// assert_eq!(evidence.store_status().unwrap().retained_episodes, 0);
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct TopologyEvidencePlugin {
    config: BoundedEvidenceConfig,
}

impl TopologyEvidencePlugin {
    /// Creates the default provider plugin with explicit retention bounds.
    #[must_use]
    pub const fn new(config: BoundedEvidenceConfig) -> Self {
        Self { config }
    }
}

#[plugin(
    id = "cosmos::topology_evidence",
    version = "0.1.0",
    provides(TopologyEvidence)
)]
impl Plugin for TopologyEvidencePlugin {
    fn build(&self, server: &mut Server) {
        let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(self.config)));
        assert!(
            server.insert_global(evidence).is_none(),
            "TopologyEvidence capability already has a global resource"
        );
    }
}
