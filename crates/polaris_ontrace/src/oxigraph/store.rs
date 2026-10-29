//! [`OxigraphStore`] — `GlobalResource` wrapper over an [`oxigraph::store::Store`].
//!
//! Oxigraph's [`Store`] is internally `Arc`-shared and `Clone` is cheap: every
//! handle points to the same underlying dataset. We expose that handle as a
//! `Send + Sync + 'static` global resource — interior mutability is the store's
//! responsibility, so we don't wrap it in a `RwLock` of our own.
//!
//! The store is partitioned by convention into two named graphs:
//!
//! | Graph IRI | Purpose |
//! |-----------|---------|
//! | [`DEFAULT_ONTOLOGY_IRI`] | Curated `TBox`: classes, predicate definitions, OWL axioms |
//! | [`DEFAULT_DATA_IRI`] | Runtime `ABox`: extracted observations |
//!
//! Per-session named graphs (see [`super::OxigraphSession`]) live alongside
//! these and can be SPARQL-unioned at query time.

use std::sync::Arc;

use oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use oxigraph::model::{GraphNameRef, NamedNode, NamedNodeRef, NamedOrBlankNodeRef, QuadRef};
use oxigraph::sparql::{EvaluationError, QueryResults};
use oxigraph::store::{LoaderError, SerializerError, StorageError, Store};
use polaris::system::plugin::{Contract, Version};
use polaris::system::resource::GlobalResource;

/// Default IRI for the curated ontology (`TBox`) named graph.
pub const DEFAULT_ONTOLOGY_IRI: &str = "urn:polaris:graph:ontology";

/// Default IRI for the runtime data (`ABox`) named graph.
pub const DEFAULT_DATA_IRI: &str = "urn:polaris:graph:data";

/// Errors raised by [`OxigraphStore`] construction and IO.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Underlying `RocksDB` / in-memory storage error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// RDF parser / loader error while ingesting Turtle/N-Triples/etc.
    #[error("loader error: {0}")]
    Loader(#[from] LoaderError),

    /// RDF serializer error while dumping a graph.
    #[error("serializer error: {0}")]
    Serializer(#[from] SerializerError),

    /// SPARQL evaluation error.
    #[error("sparql error: {0}")]
    Sparql(#[from] EvaluationError),

    /// The configured ontology, data, or base IRI is not a valid IRI.
    #[error("invalid IRI: {0}")]
    InvalidIri(String),
}

/// Immutable per-store configuration (graph IRIs and optional base IRI).
///
/// Kept behind an `Arc` so cheap [`Clone`] of [`OxigraphStore`] doesn't copy
/// the strings.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// IRI of the ontology (`TBox`) named graph.
    pub ontology_graph: NamedNode,
    /// IRI of the default data (`ABox`) named graph.
    pub data_graph: NamedNode,
    /// Optional base IRI used when parsing Turtle / N3 fragments.
    pub base_iri: Option<String>,
}

impl StoreConfig {
    /// Build the default configuration (sentinel `urn:polaris:graph:*` IRIs).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidIri`] if the default IRI constants are
    /// rejected by oxigraph — in practice this is unreachable since the
    /// constants are validated at compile time by the test suite.
    pub fn defaults() -> Result<Self, StoreError> {
        Ok(Self {
            ontology_graph: NamedNode::new(DEFAULT_ONTOLOGY_IRI)
                .map_err(|e| StoreError::InvalidIri(e.to_string()))?,
            data_graph: NamedNode::new(DEFAULT_DATA_IRI)
                .map_err(|e| StoreError::InvalidIri(e.to_string()))?,
            base_iri: None,
        })
    }
}

/// An RDF/SPARQL-backed triple store exposed as a Polaris `GlobalResource`.
///
/// This is the durable, query-rich backend alternative to the naive in-memory
/// [`KnowledgeGraph`](crate::KnowledgeGraph). Reach for `OxigraphStore` when a
/// system needs to run SPARQL 1.1 queries/updates, load or dump RDF Turtle, or
/// reason over a curated `TBox` (including SHACL shapes) — rather than the flat,
/// append-only triple list the naive graph offers. Triples live in two named
/// graphs by convention: the curated ontology (`TBox`) graph
/// ([`Self::ontology_graph`]) and the runtime data (`ABox`) graph
/// ([`Self::data_graph`]).
///
/// # Scope
///
/// **Global.** A single store is shared across every session on the server.
/// The underlying [`oxigraph::store::Store`] is internally `Arc`-shared, so the
/// resource is registered once and every reader observes the same dataset; this
/// is durable state that must outlive any one trace, hence global rather than
/// per-session.
///
/// # Provided by
///
/// Inserted by [`OxigraphPlugin`](crate::oxigraph::OxigraphPlugin) (cargo
/// feature `oxigraph`) during its [`Plugin::build`](polaris::system::plugin::Plugin::build):
/// it constructs the store for the selected backend (in-memory or `RocksDB`),
/// pre-loads any `with_ontology_file` Turtle into the `TBox` graph, and calls
/// `server.insert_global(store)`. The plugin must be registered before any
/// plugin that reads the store.
///
/// # Access
///
/// Read it with `Res<OxigraphStore>` (`get_resource` / `get_global`). The
/// resource is taken by shared reference, but every method — including the
/// write paths [`Self::update`], [`Self::insert_quad`], and [`Self::load_turtle`]
/// — operates through `&self`: mutation goes through oxigraph's own
/// thread-safe interior, so there is no `ResMut` and no outer `RwLock`. The
/// handle is `Send + Sync + 'static` and cheaply [`Clone`]; clones share one
/// dataset and writes are visible to every handle.
///
/// # Alternatives
///
/// [`KnowledgeGraph`](crate::KnowledgeGraph) is the in-memory alternative: same
/// triple-extraction target, but a naive append-only list with no SPARQL,
/// Turtle, or SHACL surface. Use it when you want zero external dependencies;
/// use `OxigraphStore` when you need the query/validation surface. Exactly one
/// extraction backend writes to one of these per server.
///
/// # Example system
///
/// ```no_run
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::oxigraph::OxigraphStore;
///
/// /// Count the curated `TBox` triples for the current session.
/// #[system]
/// async fn ontology_size(store: Res<OxigraphStore>) {
///     if let Ok(n) = store.len() {
///         tracing::info!(quads = n, "store size");
///     }
/// }
/// ```
#[derive(Clone)]
pub struct OxigraphStore {
    store: Store,
    config: Arc<StoreConfig>,
}

impl std::fmt::Debug for OxigraphStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OxigraphStore")
            .field("ontology_graph", &self.config.ontology_graph.as_str())
            .field("data_graph", &self.config.data_graph.as_str())
            .field("base_iri", &self.config.base_iri)
            .finish()
    }
}

impl OxigraphStore {
    /// Open an in-memory store with default graph IRIs.
    ///
    /// # Errors
    ///
    /// Propagates oxigraph initialization failures via [`StoreError::Storage`].
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_store(Store::new()?, StoreConfig::defaults()?)
    }

    /// Open or create a RocksDB-backed store at `path`.
    ///
    /// Only one process may hold a read-write store on the same directory at
    /// a time — see [`Store::open`].
    ///
    /// # Errors
    ///
    /// Propagates oxigraph initialization failures via [`StoreError::Storage`].
    #[cfg(feature = "oxigraph-rocksdb")]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        Self::from_store(Store::open(path)?, StoreConfig::defaults()?)
    }

    /// Construct from a pre-built [`Store`] and config. Ensures the configured
    /// named graphs exist (no-op if they already do).
    pub fn from_store(store: Store, config: StoreConfig) -> Result<Self, StoreError> {
        store.insert_named_graph(config.ontology_graph.as_ref())?;
        store.insert_named_graph(config.data_graph.as_ref())?;
        Ok(Self {
            store,
            config: Arc::new(config),
        })
    }

    /// Borrow the underlying oxigraph store for advanced use.
    #[must_use]
    pub fn raw(&self) -> &Store {
        &self.store
    }

    /// IRI of the ontology (`TBox`) named graph.
    #[must_use]
    pub fn ontology_graph(&self) -> NamedNodeRef<'_> {
        self.config.ontology_graph.as_ref()
    }

    /// IRI of the default data (`ABox`) named graph.
    #[must_use]
    pub fn data_graph(&self) -> NamedNodeRef<'_> {
        self.config.data_graph.as_ref()
    }

    /// Optional base IRI used when parsing Turtle fragments.
    #[must_use]
    pub fn base_iri(&self) -> Option<&str> {
        self.config.base_iri.as_deref()
    }

    // ─── SPARQL ───────────────────────────────────────────────────────────

    /// Execute a SPARQL 1.1 query (SELECT / ASK / CONSTRUCT / DESCRIBE).
    ///
    /// # Errors
    ///
    /// Returns [`EvaluationError`] on parse or evaluation failure.
    pub fn query(&self, sparql: &str) -> Result<QueryResults, EvaluationError> {
        self.store.query(sparql)
    }

    /// Apply a SPARQL 1.1 UPDATE (INSERT / DELETE / LOAD / CLEAR / ...).
    ///
    /// # Errors
    ///
    /// Returns [`EvaluationError`] on parse or evaluation failure.
    pub fn update(&self, sparql: &str) -> Result<(), EvaluationError> {
        self.store.update(sparql)
    }

    // ─── Quad CRUD ────────────────────────────────────────────────────────

    /// Insert a quad. Returns `true` if the quad did not previously exist.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] on storage layer failure.
    pub fn insert_quad<'a>(&self, quad: impl Into<QuadRef<'a>>) -> Result<bool, StorageError> {
        self.store.insert(quad)
    }

    /// Total number of quads across all named graphs.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] on storage layer failure.
    pub fn len(&self) -> Result<usize, StorageError> {
        self.store.len()
    }

    /// `true` if the store has no quads.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] on storage layer failure.
    pub fn is_empty(&self) -> Result<bool, StorageError> {
        self.store.is_empty()
    }

    /// Register a named graph (no-op if it already exists).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] on storage layer failure.
    pub fn ensure_named_graph<'a>(
        &self,
        graph: impl Into<NamedOrBlankNodeRef<'a>>,
    ) -> Result<bool, StorageError> {
        self.store.insert_named_graph(graph)
    }

    // ─── Bulk I/O ─────────────────────────────────────────────────────────

    /// Parse a Turtle fragment and insert every triple into `into_graph`.
    ///
    /// Use [`Self::ontology_graph`] for `into_graph` to load `TBox` content.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Loader`] on parse failure or
    /// [`StoreError::InvalidIri`] if the configured base IRI is invalid.
    pub fn load_turtle<'a>(
        &self,
        turtle: &str,
        into_graph: impl Into<GraphNameRef<'a>>,
    ) -> Result<(), StoreError> {
        let mut parser = RdfParser::from_format(RdfFormat::Turtle)
            .without_named_graphs()
            .with_default_graph(into_graph.into().into_owned());
        if let Some(base) = &self.config.base_iri {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| StoreError::InvalidIri(e.to_string()))?;
        }
        self.store.load_from_reader(parser, turtle.as_bytes())?;
        Ok(())
    }

    /// Dump a single named graph as Turtle.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Serializer`] on serializer failure.
    pub fn dump_turtle<'a>(
        &self,
        graph: impl Into<GraphNameRef<'a>>,
    ) -> Result<String, StoreError> {
        let buf = self.store.dump_graph_to_writer(
            graph.into(),
            RdfSerializer::from_format(RdfFormat::Turtle),
            Vec::<u8>::new(),
        )?;
        // Turtle output is valid UTF-8 by construction.
        Ok(String::from_utf8(buf).expect("turtle serializer emits utf-8"))
    }
}

impl GlobalResource for OxigraphStore {}

impl Contract for OxigraphStore {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::NamedNode;

    #[test]
    fn default_iris_are_valid() {
        NamedNode::new(DEFAULT_ONTOLOGY_IRI).expect("ontology iri valid");
        NamedNode::new(DEFAULT_DATA_IRI).expect("data iri valid");
    }

    #[test]
    fn in_memory_round_trip_via_sparql() {
        let store = OxigraphStore::in_memory().unwrap();
        store
            .update(
                "INSERT DATA { GRAPH <urn:polaris:graph:data> {\
                 <urn:alice> <urn:knows> <urn:bob>\
                 } }",
            )
            .unwrap();

        let results = store
            .query("SELECT ?o WHERE { GRAPH ?g { <urn:alice> <urn:knows> ?o } }")
            .unwrap();
        let buf = results
            .write(
                Vec::<u8>::new(),
                oxigraph::sparql::results::QueryResultsFormat::Json,
            )
            .unwrap();
        let json = String::from_utf8(buf).unwrap();
        assert!(json.contains("urn:bob"), "json was: {json}");
    }

    #[test]
    fn load_turtle_into_ontology_graph() {
        let store = OxigraphStore::in_memory().unwrap();
        let ttl = "@prefix ex: <urn:ex:> .\n\
                   ex:Person a <http://www.w3.org/2002/07/owl#Class> .";
        store.load_turtle(ttl, store.ontology_graph()).unwrap();

        let ok = store
            .query(
                "ASK { GRAPH <urn:polaris:graph:ontology> {\
                 <urn:ex:Person> a <http://www.w3.org/2002/07/owl#Class>\
                 } }",
            )
            .unwrap();
        match ok {
            QueryResults::Boolean(b) => assert!(b),
            _ => panic!("expected boolean result"),
        }
    }

    #[test]
    fn dump_turtle_includes_inserts() {
        let store = OxigraphStore::in_memory().unwrap();
        store
            .update(
                "INSERT DATA { GRAPH <urn:polaris:graph:ontology> {\
                 <urn:ex:Person> a <http://www.w3.org/2002/07/owl#Class>\
                 } }",
            )
            .unwrap();
        let ttl = store.dump_turtle(store.ontology_graph()).unwrap();
        assert!(
            ttl.contains("urn:ex:Person"),
            "expected dumped turtle to mention Person, got: {ttl}"
        );
    }

    #[test]
    fn clone_handle_shares_dataset() {
        let a = OxigraphStore::in_memory().unwrap();
        let b = a.clone();
        a.update("INSERT DATA { <urn:x> <urn:p> <urn:y> }").unwrap();
        assert_eq!(b.len().unwrap(), 1, "clones must observe writes");
    }
}
