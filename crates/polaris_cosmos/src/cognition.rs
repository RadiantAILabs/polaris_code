//! Domain-neutral cognitive models expressed as Polaris graphs.
//!
//! A model in this module is a named, swappable **point** in cognitive-model
//! space. Models compose ordinary graph systems supplied by an agent builder;
//! they do not define a second executor or type system. The graph's derived
//! [`GraphSignature`](polaris::graph::GraphSignature) is its structural
//! contract.
//!
//! Role schedules provide semantic observability across models. They label
//! what a system is doing without constraining the system's payload type.

use polaris::system::plugin::Schedule;

pub mod approved;
pub mod prediction_error_loop;

pub use approved::{
    CognitionError, CognitivePointDescriptor, GraphBuilder, OnAdjudicate, OnCombine, OnEscalate,
    OnProbe, OnResolve, OnScope, OracleQualification, OracleWaiver, OutputPredicate, PointMetadata,
    ResolutionInput,
};

/// Role marker for a system that commits an expected outcome before acting.
#[derive(Debug, Clone, Copy)]
pub struct OnPredict;
impl Schedule for OnPredict {}

/// Role marker for a system that acts through the agent's embodiment.
#[derive(Debug, Clone, Copy)]
pub struct OnAct;
impl Schedule for OnAct {}

/// Role marker for a system that verifies or scores an observed outcome.
#[derive(Debug, Clone, Copy)]
pub struct OnVerify;
impl Schedule for OnVerify {}

/// Role marker for a system that compares a prediction with a verified outcome.
#[derive(Debug, Clone, Copy)]
pub struct OnPredictionError;
impl Schedule for OnPredictionError {}
