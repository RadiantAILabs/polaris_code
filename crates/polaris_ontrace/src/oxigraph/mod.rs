//! Oxigraph-backed knowledge graph plugin.
//!
//! Gated behind the `oxigraph` cargo feature. Provides:
//!
//! - [`OxigraphStore`] — `GlobalResource` wrapping an [`oxigraph::store::Store`]
//!   with conventional ontology/data named graphs.
//! - [`OxigraphSession`] — `LocalResource` holding a per-session named-graph
//!   URI and a staging buffer with explicit `commit` / `discard`.
//! - [`OxigraphPlugin`] — wires the store as a global resource, optionally
//!   registers [`OxigraphTools`] with the [`ToolRegistry`].
//! - [`OxigraphTools`] — LLM-callable SPARQL toolset
//!   (`sparql_query`, `sparql_update`, `load_ontology_turtle`,
//!   `describe_resource`).
//!
//! See the module-level doc on [`store`] for the conventional graph split
//! between ontology (`TBox`) and runtime data (`ABox`).
//!
//! [`ToolRegistry`]: polaris::tools::ToolRegistry

pub mod canonicalize;
pub mod plugin;
pub mod session;
pub mod store;
pub mod tools;

pub use canonicalize::{canonicalize_default_graph, canonicalize_named_graph};
pub use plugin::{Backend, OxigraphPlugin};
pub use session::OxigraphSession;
pub use store::{DEFAULT_DATA_IRI, DEFAULT_ONTOLOGY_IRI, OxigraphStore, StoreConfig, StoreError};
pub use tools::OxigraphTools;
