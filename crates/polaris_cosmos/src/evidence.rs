//! Append-only topology evidence and contextual statistics.
//!
//! The [`store`] module defines the provider boundary and the bounded in-memory
//! reference implementation. Hosts retain ownership of durable storage,
//! lifecycle policy, and backend replacement.

mod gate_plan;
mod plugin;
pub mod store;

pub use gate_plan::{
    GatePlanOutcomeIndex, gate_expression_shape, gate_plan_record, gate_plan_shape,
};
pub use plugin::TopologyEvidencePlugin;

pub use store::{
    AppendReceipt, BoundedEvidenceConfig, BoundedInMemoryEvidence, CapacityReport, CohortKey,
    CohortStatistics, CreditAssignment, EvidenceError, EvidenceEvent, ExecutionBucket,
    MeasurementAggregate, ProfileBucket, ProviderModelBucket, RunView, SelectionEpisodeView,
    StatisticsRequest, StoreStatus, TOPOLOGY_EVIDENCE_VERSION, TopologyEvidence,
    TopologyEvidenceProvider, TopologyStatsSnapshot, UnitWitness, aggregate_snapshot,
};
