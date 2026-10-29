//! Plugin lifecycle for topology discovery.

use polaris::system::plugin;
use polaris::system::plugin::Plugin;
use polaris::system::server::Server;

use super::TopologyCatalog;

/// Provides the startup-extensible [`TopologyCatalog`] for topology discovery.
///
/// Reach for this plugin when an application assembles topology-pack plugins
/// and needs one sealed, process-wide catalog for selector and runtime systems.
/// Packs depend on the catalog capability rather than this concrete provider:
/// their `build()` methods accept
/// [`Extends<TopologyCatalog>`](polaris::system::plugin::Extends), which lets
/// Polaris order them after whichever compatible catalog provider is installed.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`TopologyCatalog`] | Global (post-`ready`) | Mutable registration target during plugin `build()`; startup-sealed, immutable discovery catalog during execution. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Systems consume the catalog through `Res<TopologyCatalog>`; no callable host API is installed. |
///
/// # Dependencies
///
/// None. Topology packs extend the capability this plugin provides.
///
/// # Lifecycle
///
/// - **`build()`** — inserts an empty mutable catalog resource so topology-pack
///   plugins can register contracts and owned graphs.
/// - **`ready()`** — removes the build-phase resource, audits that every graph
///   has complete admission metadata, seals it, and inserts it as an immutable
///   global resource. An incomplete registration fails assembly with its typed
///   catalog error rather than reaching runtime preflight.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogPlugin};
///
/// # async fn assemble() {
/// let mut server = Server::new();
/// server.add_plugins(TopologyCatalogPlugin);
/// // Add topology-pack plugins in any order; capability resolution places
/// // their `Extends<TopologyCatalog>` build step after this provider.
/// server.finish().await.expect("topology plugins should resolve");
///
/// let catalog = server
///     .get_global::<TopologyCatalog>()
///     .expect("the finished server exposes a sealed topology catalog");
/// assert!(catalog.is_sealed());
/// # }
/// ```
#[derive(Debug, Default, Copy, Clone)]
pub struct TopologyCatalogPlugin;

#[plugin(
    id = "cosmos::topology_catalog",
    version = "0.1.0",
    provides(TopologyCatalog)
)]
impl Plugin for TopologyCatalogPlugin {
    fn build(&self, server: &mut Server) {
        assert!(
            server.insert_resource(TopologyCatalog::new()).is_none(),
            "TopologyCatalog capability already has a build-phase resource"
        );
    }

    async fn ready(&self, server: &mut Server) {
        let mut catalog = server
            .remove_resource::<TopologyCatalog>()
            .expect("TopologyCatalogPlugin inserted the build-phase catalog");
        if let Err(error) = catalog.seal_admission_ready() {
            panic!("topology catalog registrations must be admission-ready: {error}");
        }
        assert!(
            server.insert_global(catalog).is_none(),
            "TopologyCatalog capability already has a global resource"
        );
    }
}
