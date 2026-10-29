//! Polaris-ontrace — extracts entity/predicate triples per trace via a small
//! LLM and builds an in-memory temporal knowledge graph.
//!
//! # Module layout
//!
//! The crate splits along four axes so a new agent pattern (different
//! prompt, different graph, different LLM) is one new file in [`agents`]
//! plus a thin plugin in [`plugins`].
//!
//! | Module | Holds |
//! |--------|-------|
//! | [`agents`] | Per-pattern `Agent` implementations: graph build + `#[system]` functions + agent-local LLM/config resources. |
//! | [`resources`] | Shared `GlobalResource` / `LocalResource` types — knowledge graph, alias table, trace-line input. |
//! | [`plugins`] | Polaris `Plugin`s that wire one agent + the resources it needs into a `Server`. |
//! | [`sources`] | Transport plugins (stdin, file tail, OTLP, Claude Code) that push into [`TraceIngestAPI`]. |
//! | [`dump`] | RDF Turtle serializer for the in-memory KG. |
//! | [`oxigraph`] | (feature-gated) Oxigraph-backed store, session staging, and SPARQL toolset. |
//!
//! # Pipeline
//!
//! ```text
//!                       ┌────────────────────────┐    ┌──────────────────────────┐
//!  StdinIOPlugin     ──┐│  TraceIngestPlugin     │    │  (one of)                │
//!  FileTailIOPlugin  ──┼│   TraceIngestAPI ──┐   │    │   ExtractionPlugin       │
//!  OtlpIOPlugin      ──┤│        push        ▼   │    │     or                   │
//!  ClaudeCodeIOPlugin──┘│   mpsc<IOMessage> ─┴───┼──▶ │   OxigraphExtractionPlug │
//!                       │   TraceIngestRx::take  │    │     │                    │
//!                       └────────────────────────┘    │     ▼                    │
//!                                                     │   worker ─► run_oneshot  │
//!                                                     │     │                    │
//!                                                     │     ▼                    │
//!                                                     │   KnowledgeGraph         │
//!                                                     │     or OxigraphStore     │
//!                                                     └──────────────────────────┘
//! ```
//!
//! Compose source plugins freely — multiple transports can feed the same
//! extractor in parallel. Exactly one extraction backend may be registered;
//! the dispatcher's [`TraceIngestRx`](plugins::trace_ingest::TraceIngestRx)
//! panics if a second backend tries to claim the receiver. See
//! `examples/stdin_demo.rs` for end-to-end wiring.

pub mod agents;
pub mod dump;
pub(crate) mod iri;
#[cfg(feature = "oxigraph")]
pub mod oxigraph;
pub mod plugins;
pub mod resources;
pub(crate) mod scalar;
pub mod sources;
#[cfg(feature = "oxigraph")]
pub mod tui;

pub use agents::extraction::{OntologyContext, OntologyPolicy, RawTriple};
pub use agents::seed::SeedOntology;
pub use plugins::extraction::{DEFAULT_MODEL_ID, ExtractionPlugin};
pub use plugins::trace_ingest::{TraceIngestAPI, TraceIngestPlugin};
pub use resources::aliases::{Aliases, Resolution, load_alias_seeds, parse_alias_seeds};
pub use resources::kg::{Entity, KnowledgeGraph, Predicate, Timestamp, Triple};
pub use resources::ontology_gap::{GapReport, GapStat, OntologyGap};
pub use resources::sightings::{Sighting, Sightings};
pub use resources::spend::{ModelPrice, ModelReport, ModelUsage, Pricing, Spend};
pub use resources::trace_line::TraceLine;
pub use sources::{ClaudeCodeIOPlugin, FileTailIOPlugin, OtlpIOPlugin, StdinIOPlugin};

#[cfg(feature = "oxigraph")]
pub use agents::inspect::{
    DEFAULT_MAX_ITERATIONS as DEFAULT_INSPECT_MAX_ITERATIONS, InspectAgent, InspectConfig,
    InspectInput, InspectOntologyOverview,
};
#[cfg(feature = "oxigraph")]
pub use agents::oxigraph_extraction::OxigraphExtractionAgent;
#[cfg(feature = "oxigraph")]
pub use plugins::inspect::{DEFAULT_INSPECT_MODEL_ID, InspectAPI, InspectPlugin};
#[cfg(feature = "oxigraph")]
pub use plugins::oxigraph_extraction::OxigraphExtractionPlugin;
#[cfg(feature = "oxigraph")]
pub use tui::{FilteredLogPanelPlugin, InspectChatPanel, InspectTuiPlugin, SparqlPanel};

/// Discoverability catalogs: indexes of every plugin, API, and consumer-facing
/// resource this crate ships, each linking to the item's rustdoc.
///
/// These are doc-only modules whose contents are authored in
/// `src/docs/{plugins,apis,resources}.md`. See the reference standards under
/// `docs/reference/` for what each item's rustdoc must contain.
pub mod docs {
    #[doc = include_str!("docs/plugins.md")]
    pub mod plugins {}

    #[doc = include_str!("docs/apis.md")]
    pub mod apis {}

    #[doc = include_str!("docs/resources.md")]
    pub mod resources {}
}
