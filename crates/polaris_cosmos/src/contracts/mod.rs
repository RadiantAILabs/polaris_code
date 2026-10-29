//! Portable Layer-1 values shared by topology selection, evidence, and cognition.
//!
//! These records are inert data contracts. They add no selector policy,
//! storage provider, plugin lifecycle, graph execution, or structural type
//! system. Persisted identities use deterministic CBOR and SHA-256; native
//! Polaris graph signatures remain the structural authority.

mod assessment;
mod availability;
mod composition;
mod encoding;
mod evidence;
mod identity;
mod profile;
mod quantity;
mod runtime;
mod selection;

pub use assessment::{
    Assessment, AssessmentAccounting, AssessmentConclusion, AssessmentSummary, ConclusionKind,
    StatusCounts, SubjectSchema,
};
pub use availability::{
    AvailabilityObservation, AvailabilityState, BindingAvailability, CapabilityAvailability,
    MechanismAvailability, OutputAvailability, ProviderAvailability, ResourceAccess,
    ResourceAvailability, RuntimeAvailabilitySnapshot,
};
pub use composition::{
    CompositionBlockRef, CompositionChoice, CompositionChoiceOrigin, CompositionDecision,
    CompositionEvidence, CompositionOutcome, CompositionPlan, CompositionRecipeRef,
    CompositionRevisionEvidence, CompositionRevisionTrail, CompositionRun, CompositionValidation,
    CompositionValidationDisposition, GateExpressionShapeKey, GatePlanRecord,
    MAX_COMPOSITION_BLOCKS, NativeGraphSignatureWitness, RecordedRefusal, RevisionAttempt,
};
pub use encoding::{CanonicalRecord, ContractError};
pub(crate) use encoding::{canonical_cbor, content_digest};
pub use evidence::{
    EpistemicBasis, EvidenceKind, EvidenceRef, EvidenceTime, ExecutionDisposition,
    FeatureAssessment, LogicalSequence, Provenance, RecordedAt, ResidualDisposition, SourceKind,
    Status, StopReason, UnknownReason,
};
pub use identity::{
    AdmissionAttemptId, AvailabilitySnapshotId, BindingAlias, BindingKindId, CapabilityId,
    CapabilityKey, CompositionBlockId, CompositionBlockKey, CompositionCatalogId,
    CompositionCatalogKey, CompositionRecipeId, CompositionRecipeKey, ConfigurationSchemaId,
    ConfigurationSchemaKey, ContentDigest, ContextTypeId, ContextTypeKey, DataPathId, EmbodimentId,
    EmbodimentKey, EnvironmentId, EnvironmentKey, ErrorClassId, EvidenceObjectId, EvidenceStoreId,
    ExclusionCode, ExternalSessionRef, HostRuntimeId, HostRuntimeKey, MeasurementId, ModelId,
    OutcomeObservationId, PolicyId, PolicyLabelId, PolicyRef, ProviderId, ProviderKey,
    RecordSubjectId, RoleScheduleId, RunId, RuntimeMechanismId, RuntimeMechanismKey, SchemaVersion,
    ScoreComponentId, SelectionDecisionId, SelectorId, SelectorKey, SourceId, SourceKey,
    SubjectSchemaId, SubjectSchemaKey, TaskId, TenancyScopeId, TokenAccountingId, VersionedKey,
};
pub use profile::{
    ConsequenceLevel, DecompositionShape, HardConstraints, MaturityLevel, MissingPreferencePolicy,
    NeedLevel, Objective, ObservedLimit, PreferenceCriterion, PreferenceWeights, RiskLevel,
    TaskFeatures, TaskHorizon, TaskProfile, TaskProfileEvidence, TieBreakPolicy,
    UnknownEstimatePolicy, VerifierAvailability, VerifierCapability, VerifierProfile,
};
pub use quantity::{
    CardinalityEstimate, ConfidencePpm, CoveragePpm, CurrencyCode, DurationMs, Money,
    PreferenceBasisPoints, ProbabilityPpm, TokenQuantity, UtilityPpm,
};
pub use runtime::{
    CancellationRecord, CancellationSource, ControlFlowChoice, EmptyStatisticsSnapshot,
    ExecutionFault, ExecutionProvenance, ExecutionStage, LoopRecord, MeasurementEntry,
    ModelProvenance, OutcomeObservation, ParallelGroupRecord, ResidualSummary, RoleScheduleCount,
    RunAbandoned, RunCompletion, RunControlFlow, RunFaults, RunFinalized, RunOpened, RunStructure,
    RunTerminal, RunUsage, SelectionNotStarted, SelectionRecord, StatisticsWindow, SupersedeReason,
    SupersedesClaim, TokenUsage, TopologyRunRecord, VerifierResult,
};
pub use selection::{
    AdmissionEvaluation, AdmissionResult, CandidateConsideration, CandidateScore,
    ConfigurationEntry, ConfigurationValue, CriterionUtility, ExclusionPhase, ExclusionReason,
    ExclusionSummary, FallbackCandidate, Measurement, MeasurementValue, PolicyEligibility,
    RegistrationSelectionRef, ResolvedConfiguration, ResolvedConfigurationRef, SamplingProvenance,
    ScoreComponent, ScoreSource, SelectionDecision, SelectionInputRefs, SelectionOutcome,
    SelectionRationale, SelectorRef,
};
