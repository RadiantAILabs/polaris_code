//! [`OxigraphPlugin`] — wires an [`OxigraphStore`] into a Polaris [`Server`].
//!
//! Always inserts [`OxigraphStore`] as a `GlobalResource`. If
//! [`OxigraphPlugin::with_tools`] is set, it also registers [`OxigraphTools`]
//! with the [`ToolRegistry`] and declares [`ToolsPlugin`] as a dependency.
//!
//! [`Server`]: polaris::system::server::Server
//! [`ToolsPlugin`]: polaris::tools::ToolsPlugin
//! [`ToolRegistry`]: polaris::tools::ToolRegistry

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use polaris::tools::{ToolPermission, ToolRegistry};

use super::store::{OxigraphStore, StoreConfig};
use super::tools::OxigraphTools;

/// Storage backend selection for [`OxigraphPlugin`].
#[derive(Debug, Clone)]
pub enum Backend {
    /// Fresh in-memory store (default). Lost on shutdown.
    InMemory,
    /// RocksDB-backed store at the given path. Requires the
    /// `oxigraph-rocksdb` cargo feature.
    #[cfg(feature = "oxigraph-rocksdb")]
    Path(std::path::PathBuf),
}

/// Installs an [`OxigraphStore`] as a `GlobalResource` (feature `oxigraph`)
/// and, optionally, an [`OxigraphTools`] SPARQL toolset for LLM agents.
///
/// Add this when you want a shared RDF store on the server — the write target
/// for [`OxigraphExtractionPlugin`](crate::OxigraphExtractionPlugin), the
/// query backend for the `ontrace inspect` shell, or simply a SPARQL store
/// other plugins read. It must be registered **before** any plugin that
/// requires the store. Choose [`Self::in_memory`] for an ephemeral store or
/// [`Self::with_path`] (feature `oxigraph-rocksdb`) for an on-disk one, and
/// pre-load curated schema with [`Self::with_ontology_file`].
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`OxigraphStore`] | Global | Shared RDF store (in-memory or RocksDB-backed) with data (`ABox`) and ontology (`TBox`) named graphs. |
///
/// # APIs Provided
///
/// _none_ — consumers reach the store through the [`OxigraphStore`] global
/// resource (`get_global`), not an `API` handle.
///
/// # Tools Provided
///
/// Only when [`Self::with_tools`]`(true)` is set; the toolset is registered
/// with the [`ToolRegistry`]. All four operate over the same shared
/// [`OxigraphStore`] handle:
///
/// | Tool | Description |
/// |------|-------------|
/// | `sparql_query` | Execute a SPARQL 1.1 read query (`SELECT` / `ASK` → JSON, `CONSTRUCT` / `DESCRIBE` → Turtle); output truncated past 32 KiB. |
/// | `sparql_update` | Apply a SPARQL 1.1 UPDATE (`INSERT` / `DELETE` / `LOAD` / `CLEAR` / `CREATE` / `DROP`); permission [`ToolPermission::Confirm`] by default, override via [`Self::with_update_permission`]. |
/// | `load_ontology_turtle` | Parse a Turtle fragment into the ontology (`TBox`) named graph; permission [`ToolPermission::Confirm`] by default, override via [`Self::with_load_permission`]. |
/// | `describe_resource` | Return all triples mentioning an IRI as subject or object, as Turtle (a `DESCRIBE` shorthand). |
///
/// # Dependencies
///
/// Declared by capability in [`Plugin::access`]:
///
/// - provides [`OxigraphStore`] — always.
/// - extends [`ToolRegistry`] — **only** when [`Self::with_tools`]`(true)`.
///   In that case [`ToolsPlugin`](polaris::tools::ToolsPlugin) must also be
///   added to the server; a tools-less server needs no `ToolsPlugin`.
///
/// # Lifecycle
///
/// Gated behind the `oxigraph` cargo feature. Everything happens in
/// [`Plugin::build`]: the store is constructed for the selected
/// [`Backend`], every [`Self::with_ontology_file`] path is read and loaded
/// into the ontology (`TBox`) named graph (so curated shapes are present on
/// the first trace), the [`OxigraphTools`] toolset and its tool permissions
/// are registered if `with_tools` is enabled, and finally the store is
/// inserted as a global. There is no background worker.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_ontrace::oxigraph::OxigraphPlugin;
///
/// # async fn example() {
/// let mut server = Server::new();
/// server
///     // ToolsPlugin is required only because with_tools(true) is set.
///     .add_plugins(ToolsPlugin)
///     .add_plugins(OxigraphPlugin::in_memory().with_tools(true));
/// server.finish().await.unwrap();
/// # }
/// ```
pub struct OxigraphPlugin {
    backend: Backend,
    base_iri: Option<String>,
    register_tools: bool,
    update_permission: ToolPermission,
    load_permission: ToolPermission,
    ontology_files: Vec<std::path::PathBuf>,
}

impl OxigraphPlugin {
    /// Plugin configured to use a fresh in-memory store.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::with_backend(Backend::InMemory)
    }

    /// Plugin configured to use a RocksDB-backed store at `path`.
    ///
    /// Requires the `oxigraph-rocksdb` cargo feature.
    #[cfg(feature = "oxigraph-rocksdb")]
    #[must_use]
    pub fn with_path(path: impl Into<std::path::PathBuf>) -> Self {
        Self::with_backend(Backend::Path(path.into()))
    }

    fn with_backend(backend: Backend) -> Self {
        Self {
            backend,
            base_iri: None,
            register_tools: false,
            update_permission: ToolPermission::Confirm,
            load_permission: ToolPermission::Confirm,
            ontology_files: Vec::new(),
        }
    }

    /// Set the base IRI used when parsing Turtle fragments.
    #[must_use]
    pub fn with_base_iri(mut self, base_iri: impl Into<String>) -> Self {
        self.base_iri = Some(base_iri.into());
        self
    }

    /// Toggle registration of the [`OxigraphTools`] SPARQL toolset.
    ///
    /// When enabled, [`ToolsPlugin`] must also be added to the server.
    /// Default: `false`.
    #[must_use]
    pub fn with_tools(mut self, yes: bool) -> Self {
        self.register_tools = yes;
        self
    }

    /// Override the permission policy for the mutating `sparql_update` tool.
    /// Default: [`ToolPermission::Confirm`]. Use [`ToolPermission::Allow`]
    /// for fully autonomous agents.
    #[must_use]
    pub fn with_update_permission(mut self, p: ToolPermission) -> Self {
        self.update_permission = p;
        self
    }

    /// Override the permission policy for `load_ontology_turtle`.
    /// Default: [`ToolPermission::Confirm`].
    #[must_use]
    pub fn with_load_permission(mut self, p: ToolPermission) -> Self {
        self.load_permission = p;
        self
    }

    /// Seed the ontology (`TBox`) named graph from a Turtle file at startup.
    ///
    /// Loaded once during [`Plugin::build`] before any extraction runs, so
    /// downstream agents can rely on the curated shapes being present on the
    /// first trace. May be called multiple times to merge several files.
    #[must_use]
    pub fn with_ontology_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.ontology_files.push(path.into());
        self
    }

    fn build_store(&self) -> OxigraphStore {
        let mut config = StoreConfig::defaults().expect("default IRIs are valid");
        config.base_iri.clone_from(&self.base_iri);

        let store = match &self.backend {
            Backend::InMemory => OxigraphStore::from_store(
                oxigraph::store::Store::new().expect("oxigraph in-memory init"),
                config,
            )
            .expect("oxigraph in-memory init"),
            #[cfg(feature = "oxigraph-rocksdb")]
            Backend::Path(p) => OxigraphStore::from_store(
                oxigraph::store::Store::open(p).expect("oxigraph rocksdb open"),
                config,
            )
            .expect("oxigraph rocksdb init"),
        };

        for path in &self.ontology_files {
            let turtle = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read ontology file {}: {e}", path.display()));
            store
                .load_turtle(&turtle, store.ontology_graph())
                .unwrap_or_else(|e| panic!("load ontology file {}: {e}", path.display()));
        }

        store
    }
}

impl Plugin for OxigraphPlugin {
    const ID: &'static str = "ontrace::oxigraph";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Always provides the `OxigraphStore` global. Only extends the
        // `ToolRegistry` when the SPARQL toolset is enabled, so a tools-less
        // server needn't register `ToolsPlugin`. The conditional makes this
        // config-derived — the `#[plugin]` macro's static `access()` could not
        // express it, hence the hand-written form.
        let access = PluginAccess::new().provides::<OxigraphStore>(OxigraphStore::CONTRACT_VERSION);
        if self.register_tools {
            access.extends::<ToolRegistry>(VersionReq::caret(ToolRegistry::CONTRACT_VERSION))
        } else {
            access
        }
    }

    fn build(&self, server: &mut Server) {
        let store = self.build_store();

        if self.register_tools {
            let mut registry = server
                .get_resource_mut::<ToolRegistry>()
                .expect("OxigraphPlugin::with_tools(true) requires ToolsPlugin");
            registry.register_toolset(OxigraphTools::new(store.clone()));
            registry
                .set_permission("sparql_update", self.update_permission)
                .expect("sparql_update should be registered by OxigraphTools toolset");
            registry
                .set_permission("load_ontology_turtle", self.load_permission)
                .expect("load_ontology_turtle should be registered by OxigraphTools toolset");
        }

        server.insert_global(store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_builds_store() {
        let plugin = OxigraphPlugin::in_memory();
        let store = plugin.build_store();
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn with_base_iri_propagates_to_store() {
        let plugin = OxigraphPlugin::in_memory().with_base_iri("urn:test:base/");
        let store = plugin.build_store();
        assert_eq!(store.base_iri(), Some("urn:test:base/"));
    }

    #[test]
    fn provides_store_and_no_tool_extension_without_tools() {
        let plugin = OxigraphPlugin::in_memory();
        let access = plugin.access();
        assert_eq!(access.provided().len(), 1);
        assert_eq!(
            access.provided()[0].type_id(),
            std::any::TypeId::of::<OxigraphStore>()
        );
        assert!(access.extended().is_empty());
    }

    #[test]
    fn extends_tool_registry_when_tools_enabled() {
        let plugin = OxigraphPlugin::in_memory().with_tools(true);
        let access = plugin.access();
        assert_eq!(access.extended().len(), 1);
        assert_eq!(
            access.extended()[0].type_id(),
            std::any::TypeId::of::<ToolRegistry>()
        );
    }

    #[test]
    fn with_ontology_file_loads_turtle_into_ontology_graph() {
        use oxigraph::sparql::QueryResults;
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
             <urn:ex:Person> a owl:Class ."
        )
        .unwrap();

        let plugin = OxigraphPlugin::in_memory().with_ontology_file(tmp.path());
        let store = plugin.build_store();

        let ok = store
            .query(
                "ASK { GRAPH <urn:polaris:graph:ontology> {\
                 <urn:ex:Person> a <http://www.w3.org/2002/07/owl#Class>\
                 } }",
            )
            .unwrap();
        match ok {
            QueryResults::Boolean(b) => assert!(b, "ontology file content missing after build"),
            _ => panic!("expected boolean result"),
        }
    }
}
