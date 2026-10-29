//! `polaris_cosmos` — domain-neutral cognition for Polaris agents.
//!
//! Polaris supplies the graph runtime; this crate supplies a library of
//! **cognitive models** expressed as Polaris graphs. Agent builders choose a
//! model and provide its domain-specific embodiment: actions, observations,
//! verifiers, tools, and environment resources. A coding agent supplies files,
//! editors, shells, and tests; the same cognitive graph can be embodied for a
//! different domain.
//!
//! A cognitive model is a **point**, not the cognitive-model axis itself. No
//! reasoning topology is canonical. Each model builds ordinary
//! [`polaris::graph::Graph`] systems, and its derived
//! [`polaris::graph::GraphSignature`] is the structural contract presented to
//! its surroundings.
//!
//! The crate also owns the **memory** (`W`) side of the self-improving agent
//! model `M = (G, W)` and the domain-neutral logic coupling memory to a graph
//! schema (`G`): **recall**, **consolidate**, **rewire**, and credit assignment.
//!
//! See [`docs::taxonomy`](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/taxonomy.md)
//! for the boundary between Polaris graph mechanics, Cosmos cognition, and
//! domain embodiments.
//!
//! # Status
//!
//! Shipped cognitive points include [`cognition::prediction_error_loop`],
//! [`cognition::approved::coverage_sweep`], and
//! [`cognition::approved::bounded_correction`]. The topology runtime combines
//! the [`topology`] catalog and admission boundary, the [`selection`] policy
//! points, the [`evidence`] provider contract, and the shared [`contracts`]
//! vocabulary. The [`composition`] module turns a planner's gate-expression
//! plan into a native graph, built from an embodiment's own catalogue of
//! reusable blocks. Hosts still own pristine contexts and actual graph
//! execution. This crate does not provide a world-model backend or any
//! callable Polaris APIs.

pub mod cognition;
pub mod composition;
pub mod contracts;
pub mod evidence;
pub mod selection;
pub mod topology;

/// Discoverability catalogs: indexes of every cognitive model, plugin, API,
/// and consumer-facing resource this crate ships, each linking to rustdoc.
///
/// These are doc-only modules whose contents are authored in
/// `src/docs/`. Plugin, API, and resource entries are kept in sync with source
/// by their drift guards under `tests/`. See the reference guides under
/// `docs/reference/` for their documentation contracts.
pub mod docs {
    #[doc = include_str!("docs/models.md")]
    pub mod models {}

    #[doc = include_str!("docs/plugins.md")]
    pub mod plugins {}

    #[doc = include_str!("docs/apis.md")]
    pub mod apis {}

    #[doc = include_str!("docs/resources.md")]
    pub mod resources {}
}
