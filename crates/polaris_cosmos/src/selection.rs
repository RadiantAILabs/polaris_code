//! Replaceable, explained topology-selection policy points.
//!
//! Selection consumes frozen catalog, availability, admission, profile, and
//! statistics values. It does not resolve or execute the chosen graph.

pub mod deterministic;
pub mod transparent_profile;

pub use deterministic::{
    CandidatePolicyFacts, DeterministicSelectorConfig, OnSelect, RootSelectorConfig,
    RootSelectorError, SelectionError, SelectionInputs,
};
pub use transparent_profile::{OnProfile, ProfileError, ProfileInputs};
