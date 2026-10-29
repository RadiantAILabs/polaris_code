//! Discovery and registration primitives for reusable Polaris topologies.
//!
//! A topology remains an ordinary [`Graph`](polaris::graph::Graph), and its
//! derived [`GraphSignature`](polaris::graph::GraphSignature) remains the
//! structural truth. This module adds stable semantic identities, contracts,
//! immutable descriptors, validated catalog registration, and selection-facing
//! snapshots without defining another executor or universal topology trait.
//!
//! Topology-pack plugins register owned graphs through [`TopologyCatalogPlugin`],
//! which seals the [`TopologyCatalog`] before systems consume a
//! [`TopologyCatalogSnapshot`]. [`admission`] then projects native signatures
//! into stable diagnostics for deterministic preflight and repeats native
//! resource validation immediately before a host starts a selected graph.

pub mod admission;
mod catalog;
mod descriptor;
mod identity;
mod plugin;

pub use admission::{
    AdmissionBuildError, AdmissionCandidate, BindingRequirement, CandidateAdmissionSet,
    CandidateMechanicalAdmission, CandidateRequirements, FallbackValidity, LiveAdmission,
    MaterializedGraphAdmissionError, MaterializedStartPermit, MaterializedStartPermitError,
    ProviderRequirement, SignatureChannel, StableResourceRequirement, StableSignatureExplanation,
    StartPermit, StartPermitError, TypeExplanationRegistry, UnexplainedBoundary,
    preflight_candidates, preflight_catalog, readmit_live, readmit_materialized_graph,
    validate_fallback_plan,
};
pub use catalog::{
    CompleteTopologyRegistration, RegisteredTopology, RegistrationAdmissionMetadata,
    TopologyCandidate, TopologyCatalog, TopologyCatalogError, TopologyCatalogSnapshot,
};
pub use descriptor::{MetadataError, TopologyContract, TopologyDescriptor};
pub use identity::{
    ConfigurationDigest, IdentifierError, RegistrationId, RegistrationProvenance,
    TopologyContractId, TopologyContractKey, TopologyId, TopologyKey, TopologyVersion,
};
pub use plugin::TopologyCatalogPlugin;
