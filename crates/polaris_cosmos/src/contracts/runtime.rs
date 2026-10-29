//! Append-only selection, admission, run, outcome, and empty-statistics records.

use serde::{Deserialize, Serialize};

use super::encoding::content_digest;
use super::evidence::{ProvenanceContent, canonical_provenance};
use super::profile::validate_strictly_sorted;
use super::selection::validate_measurement;
use super::{
    AdmissionAttemptId, AdmissionEvaluation, CanonicalRecord, ConfidencePpm, ContentDigest,
    ContractError, CoveragePpm, EmbodimentKey, ErrorClassId, EvidenceRef, EvidenceTime,
    ExecutionDisposition, ExternalSessionRef, HostRuntimeKey, LogicalSequence, Measurement,
    MeasurementId, MeasurementValue, ModelId, Objective, OutcomeObservationId, PolicyRef,
    Provenance, ProviderKey, RecordSubjectId, RecordedAt, RegistrationSelectionRef,
    ResidualDisposition, RoleScheduleId, RunId, SchemaVersion, SelectionDecision,
    SelectionDecisionId, SelectionInputRefs, SelectorRef, SourceKey, SourceKind, Status,
    StopReason, TaskId, TaskProfile, TokenQuantity,
};
use crate::topology::TopologyContractKey;

/// First-class evidence record persisted before any live admission or run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRecord {
    schema_version: SchemaVersion,
    recorded_at: RecordedAt,
    decision: SelectionDecision,
    task_profile: TaskProfile,
    objective: Objective,
    content_digest: ContentDigest,
}

impl SelectionRecord {
    /// Creates a selection record after checking all inline and referenced identities.
    pub fn new(
        recorded_at: RecordedAt,
        decision: SelectionDecision,
        task_profile: TaskProfile,
        objective: Objective,
    ) -> Result<Self, ContractError> {
        let mut record = Self {
            schema_version: SchemaVersion::V1,
            recorded_at,
            decision,
            task_profile,
            objective,
            content_digest: ContentDigest::sha256(&[]),
        };
        record.validate_structure()?;
        record.content_digest = content_digest(&record.content_view())?;
        Ok(record)
    }

    /// Returns the complete selection decision.
    #[must_use]
    pub const fn decision(&self) -> &SelectionDecision {
        &self.decision
    }

    /// Returns the store-assigned selection position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the inline bounded task profile.
    #[must_use]
    pub const fn task_profile(&self) -> &TaskProfile {
        &self.task_profile
    }

    /// Returns the inline objective.
    #[must_use]
    pub const fn objective(&self) -> &Objective {
        &self.objective
    }

    /// Returns the canonical record digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.decision.validate()?;
        self.task_profile.validate()?;
        self.objective.validate()?;
        self.decision.validate_against_objective(&self.objective)?;
        if self.decision.task_id() != self.task_profile.task_id()
            || self.decision.external_session() != self.task_profile.external_session()
            || self.decision.input_refs().task_profile_digest()
                != self.task_profile.content_digest()
            || self.decision.input_refs().objective_digest() != self.objective.content_digest()
        {
            return Err(ContractError::new(
                "selection.record-input-mismatch",
                "selection decision, task profile, objective, and input digests must agree",
            ));
        }
        Ok(())
    }

    fn content_view(&self) -> SelectionRecordContent<'_> {
        SelectionRecordContent {
            schema_version: self.schema_version,
            sequence: self.recorded_at.sequence(),
            decision: &self.decision,
            task_profile: &self.task_profile,
            objective: &self.objective,
        }
    }
}

#[derive(Serialize)]
struct SelectionRecordContent<'a> {
    schema_version: SchemaVersion,
    sequence: LogicalSequence,
    decision: &'a SelectionDecision,
    task_profile: &'a TaskProfile,
    objective: &'a Objective,
}

impl CanonicalRecord for SelectionRecord {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "selection record",
        )
    }
}

/// Explicit terminal event for a selected decision whose candidates never started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionNotStarted {
    schema_version: SchemaVersion,
    decision_id: SelectionDecisionId,
    recorded_at: RecordedAt,
    admission_attempts: Vec<AdmissionAttemptId>,
    reasons: Vec<super::ExclusionReason>,
    content_digest: ContentDigest,
}

impl SelectionNotStarted {
    /// Creates a no-start terminal after at least one failed live-admission attempt.
    pub fn new(
        decision_id: SelectionDecisionId,
        recorded_at: RecordedAt,
        admission_attempts: Vec<AdmissionAttemptId>,
        mut reasons: Vec<super::ExclusionReason>,
    ) -> Result<Self, ContractError> {
        if admission_attempts.is_empty() {
            return Err(ContractError::new(
                "selection.no-start-without-admission",
                "a selected decision can become not-started only after a live admission attempt",
            ));
        }
        reasons.sort();
        reasons.dedup();
        let mut event = Self {
            schema_version: SchemaVersion::V1,
            decision_id,
            recorded_at,
            admission_attempts,
            reasons,
            content_digest: ContentDigest::sha256(&[]),
        };
        event.validate_structure()?;
        event.content_digest = content_digest(&event.content_view())?;
        Ok(event)
    }

    /// Returns the terminated decision identity.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the terminal recording position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns failed admission attempts in attempted fallback order.
    #[must_use]
    pub fn admission_attempts(&self) -> &[AdmissionAttemptId] {
        &self.admission_attempts
    }

    /// Returns complete live-readmission exclusions in canonical order.
    #[must_use]
    pub fn reasons(&self) -> &[super::ExclusionReason] {
        &self.reasons
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        if self.admission_attempts.is_empty() || self.reasons.is_empty() {
            return Err(ContractError::new(
                "selection.incomplete-no-start",
                "SelectionNotStarted requires attempts and complete exclusion reasons",
            ));
        }
        if self
            .admission_attempts
            .iter()
            .enumerate()
            .any(|(index, value)| self.admission_attempts[..index].contains(value))
        {
            return Err(ContractError::new(
                "selection.duplicate-admission-attempt",
                "SelectionNotStarted cannot repeat an admission attempt",
            ));
        }
        if self.reasons.windows(2).any(|pair| pair[0] >= pair[1])
            || self
                .reasons
                .iter()
                .any(|reason| reason.phase() != super::ExclusionPhase::LiveReadmission)
        {
            return Err(ContractError::new(
                "selection.non-canonical-no-start-reasons",
                "no-start reasons must be sorted, unique live-readmission exclusions",
            ));
        }
        for reason in &self.reasons {
            reason.validate()?;
        }
        Ok(())
    }

    fn content_view(&self) -> SelectionNotStartedContent<'_> {
        SelectionNotStartedContent {
            schema_version: self.schema_version,
            decision_id: self.decision_id,
            sequence: self.recorded_at.sequence(),
            admission_attempts: &self.admission_attempts,
            reasons: &self.reasons,
        }
    }
}

#[derive(Serialize)]
struct SelectionNotStartedContent<'a> {
    schema_version: SchemaVersion,
    decision_id: SelectionDecisionId,
    sequence: LogicalSequence,
    admission_attempts: &'a [AdmissionAttemptId],
    reasons: &'a [super::ExclusionReason],
}

impl CanonicalRecord for SelectionNotStarted {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "selection no-start event",
        )
    }
}

/// Secret-free model provenance retained by a mechanical run record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelProvenance {
    provider: ProviderKey,
    model: ModelId,
    model_version: Option<String>,
}

impl ModelProvenance {
    /// Creates model provenance without provider-local credentials.
    pub fn new(
        provider: ProviderKey,
        model: ModelId,
        model_version: Option<String>,
    ) -> Result<Self, ContractError> {
        if model_version
            .as_ref()
            .is_some_and(|version| version.is_empty() || version.len() > 128)
        {
            return Err(ContractError::new(
                "run.model-version",
                "model version must contain 1..=128 bytes when present",
            ));
        }
        Ok(Self {
            provider,
            model,
            model_version,
        })
    }

    /// Returns the provider implementation key.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Returns the stable model-family identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns the provider-local model version label, when retained.
    #[must_use]
    pub fn model_version(&self) -> Option<&str> {
        self.model_version.as_deref()
    }

    fn validate(&self) -> Result<(), ContractError> {
        if self
            .model_version
            .as_ref()
            .is_some_and(|version| version.is_empty() || version.len() > 128)
        {
            Err(ContractError::new(
                "run.model-version",
                "model version must contain 1..=128 bytes when present",
            ))
        } else {
            Ok(())
        }
    }
}

/// Host, embodiment, provider, and model provenance for one started run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProvenance {
    embodiment: EmbodimentKey,
    host_runtime: HostRuntimeKey,
    providers: Vec<ProviderKey>,
    models: Vec<ModelProvenance>,
    provenance: Vec<Provenance>,
}

impl ExecutionProvenance {
    /// Creates canonical secret-free execution provenance.
    pub fn new(
        embodiment: EmbodimentKey,
        host_runtime: HostRuntimeKey,
        mut providers: Vec<ProviderKey>,
        mut models: Vec<ModelProvenance>,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        providers.sort();
        providers.dedup();
        models.sort();
        models.dedup();
        let value = Self {
            embodiment,
            host_runtime,
            providers,
            models,
            provenance: canonical_provenance(provenance)?,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the embodiment implementation key.
    #[must_use]
    pub const fn embodiment(&self) -> &EmbodimentKey {
        &self.embodiment
    }

    /// Returns the host-runtime implementation key.
    #[must_use]
    pub const fn host_runtime(&self) -> &HostRuntimeKey {
        &self.host_runtime
    }

    /// Returns provider keys in canonical order.
    #[must_use]
    pub fn providers(&self) -> &[ProviderKey] {
        &self.providers
    }

    /// Returns model provenance in canonical order.
    #[must_use]
    pub fn models(&self) -> &[ModelProvenance] {
        &self.models
    }

    /// Returns execution provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted("run.providers", &self.providers)?;
        validate_strictly_sorted("run.models", &self.models)?;
        for model in &self.models {
            model.validate()?;
            if !self.providers.contains(&model.provider) {
                return Err(ContractError::new(
                    "run.model-provider-missing",
                    "every model provenance entry must reference a retained provider",
                ));
            }
        }
        validate_provenance(&self.provenance)
    }
}

/// Start event allocated only after successful live admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOpened {
    schema_version: SchemaVersion,
    run_id: RunId,
    task_id: TaskId,
    external_session: Option<ExternalSessionRef>,
    recorded_at: RecordedAt,
    decision_id: SelectionDecisionId,
    decision_fingerprint: ContentDigest,
    selector: SelectorRef,
    requested_contract: TopologyContractKey,
    admission_attempt_id: AdmissionAttemptId,
    selected: RegistrationSelectionRef,
    claimed_contracts: Vec<TopologyContractKey>,
    fallback_position: u64,
    input_refs: SelectionInputRefs,
    provenance: ExecutionProvenance,
    content_digest: ContentDigest,
}

impl RunOpened {
    /// Opens a run from the exact candidate admitted by a successful live attempt.
    pub fn new(
        run_id: RunId,
        recorded_at: RecordedAt,
        decision: &SelectionDecision,
        admission: &AdmissionEvaluation,
        mut claimed_contracts: Vec<TopologyContractKey>,
        fallback_position: u64,
        provenance: ExecutionProvenance,
    ) -> Result<Self, ContractError> {
        decision.validate()?;
        admission.validate()?;
        if !admission.is_admitted() || admission.decision_id() != decision.decision_id() {
            return Err(ContractError::new(
                "run.missing-successful-admission",
                "RunOpened requires a successful admission for the same decision",
            ));
        }
        if recorded_at.sequence() <= admission.recorded_at().sequence() {
            return Err(ContractError::new(
                "run.open-order",
                "RunOpened must follow successful live admission in store order",
            ));
        }
        claimed_contracts.sort();
        claimed_contracts.dedup();
        let mut event = Self {
            schema_version: SchemaVersion::V1,
            run_id,
            task_id: decision.task_id().clone(),
            external_session: decision.external_session().cloned(),
            recorded_at,
            decision_id: decision.decision_id(),
            decision_fingerprint: decision.fingerprint(),
            selector: decision.selector().clone(),
            requested_contract: decision.requested_contract().clone(),
            admission_attempt_id: admission.attempt_id(),
            selected: admission.candidate().clone(),
            claimed_contracts,
            fallback_position,
            input_refs: decision.input_refs().clone(),
            provenance,
            content_digest: ContentDigest::sha256(&[]),
        };
        event.validate_against(decision, admission)?;
        event.content_digest = content_digest(&event.content_view())?;
        Ok(event)
    }

    /// Returns the started run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the selection decision identity.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns the host-owned task identity.
    #[must_use]
    pub const fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// Returns the optional external session reference.
    #[must_use]
    pub const fn external_session(&self) -> Option<&ExternalSessionRef> {
        self.external_session.as_ref()
    }

    /// Returns the immutable selection fingerprint.
    #[must_use]
    pub const fn decision_fingerprint(&self) -> ContentDigest {
        self.decision_fingerprint
    }

    /// Returns the selector identity and configuration.
    #[must_use]
    pub const fn selector(&self) -> &SelectorRef {
        &self.selector
    }

    /// Returns the requested semantic topology contract.
    #[must_use]
    pub const fn requested_contract(&self) -> &TopologyContractKey {
        &self.requested_contract
    }

    /// Returns the successful admission attempt identity.
    #[must_use]
    pub const fn admission_attempt_id(&self) -> AdmissionAttemptId {
        self.admission_attempt_id
    }

    /// Returns the selected registration and configuration.
    #[must_use]
    pub const fn selected(&self) -> &RegistrationSelectionRef {
        &self.selected
    }

    /// Returns the store-assigned start position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns all claimed contracts in canonical order.
    #[must_use]
    pub fn claimed_contracts(&self) -> &[TopologyContractKey] {
        &self.claimed_contracts
    }

    /// Returns zero for the primary choice or the one-based fallback position.
    #[must_use]
    pub const fn fallback_position(&self) -> u64 {
        self.fallback_position
    }

    /// Returns the immutable selection input references.
    #[must_use]
    pub const fn input_refs(&self) -> &SelectionInputRefs {
        &self.input_refs
    }

    /// Returns host, embodiment, provider, and model provenance.
    #[must_use]
    pub const fn provenance(&self) -> &ExecutionProvenance {
        &self.provenance
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_against(
        &self,
        decision: &SelectionDecision,
        admission: &AdmissionEvaluation,
    ) -> Result<(), ContractError> {
        self.validate_structure()?;
        if self.decision_id != decision.decision_id()
            || self.decision_fingerprint != decision.fingerprint()
            || self.admission_attempt_id != admission.attempt_id()
            || self.selected != *admission.candidate()
        {
            return Err(ContractError::new(
                "run.correlation-mismatch",
                "run opening does not match its selection decision and admission event",
            ));
        }
        let expected = match decision.outcome() {
            super::SelectionOutcome::Selected {
                selected,
                fallback_plan,
                ..
            } if self.fallback_position == 0 => Some(selected),
            super::SelectionOutcome::Selected { fallback_plan, .. } => self
                .fallback_position
                .checked_sub(1)
                .and_then(|position| usize::try_from(position).ok())
                .and_then(|position| fallback_plan.get(position))
                .map(super::FallbackCandidate::candidate),
            super::SelectionOutcome::NoEligibleCandidate { .. } => None,
        };
        if expected != Some(&self.selected) {
            return Err(ContractError::new(
                "run.fallback-position",
                "run selection does not match the captured primary or fallback position",
            ));
        }
        Ok(())
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.selected.validate()?;
        self.provenance.validate()?;
        validate_strictly_sorted("run.claimed-contracts", &self.claimed_contracts)?;
        if !self.claimed_contracts.contains(&self.requested_contract) {
            return Err(ContractError::new(
                "run.requested-contract-not-claimed",
                "started registration must retain the requested contract claim",
            ));
        }
        Ok(())
    }

    fn content_view(&self) -> RunOpenedContent<'_> {
        RunOpenedContent {
            schema_version: self.schema_version,
            run_id: self.run_id,
            task_id: &self.task_id,
            external_session: self.external_session.as_ref(),
            sequence: self.recorded_at.sequence(),
            decision_id: self.decision_id,
            decision_fingerprint: self.decision_fingerprint,
            selector: &self.selector,
            requested_contract: &self.requested_contract,
            admission_attempt_id: self.admission_attempt_id,
            selected: &self.selected,
            claimed_contracts: &self.claimed_contracts,
            fallback_position: self.fallback_position,
            input_refs: &self.input_refs,
            provenance: &self.provenance,
        }
    }
}

#[derive(Serialize)]
struct RunOpenedContent<'a> {
    schema_version: SchemaVersion,
    run_id: RunId,
    task_id: &'a TaskId,
    external_session: Option<&'a ExternalSessionRef>,
    sequence: LogicalSequence,
    decision_id: SelectionDecisionId,
    decision_fingerprint: ContentDigest,
    selector: &'a SelectorRef,
    requested_contract: &'a TopologyContractKey,
    admission_attempt_id: AdmissionAttemptId,
    selected: &'a RegistrationSelectionRef,
    claimed_contracts: &'a [TopologyContractKey],
    fallback_position: u64,
    input_refs: &'a SelectionInputRefs,
    provenance: &'a ExecutionProvenance,
}

impl CanonicalRecord for RunOpened {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "run-opened event",
        )
    }
}

/// Count of one semantic role schedule during a run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RoleScheduleCount {
    role: RoleScheduleId,
    count: u64,
}

impl RoleScheduleCount {
    /// Creates one role-schedule count.
    #[must_use]
    pub const fn new(role: RoleScheduleId, count: u64) -> Self {
        Self { role, count }
    }

    /// Returns the semantic role-schedule identity.
    #[must_use]
    pub const fn role(&self) -> &RoleScheduleId {
        &self.role
    }

    /// Returns the number of executions of this role schedule.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }
}

/// Mechanical structure counts reported by the executor and recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStructure {
    nodes_scheduled: u64,
    nodes_completed: u64,
    nodes_skipped: Option<u64>,
    graphs_executed: u64,
    systems_executed: u64,
    role_schedule_counts: Vec<RoleScheduleCount>,
}

impl RunStructure {
    /// Creates mechanical run counts; unavailable skip counts remain `None`.
    pub fn new(
        nodes_scheduled: u64,
        nodes_completed: u64,
        nodes_skipped: Option<u64>,
        graphs_executed: u64,
        systems_executed: u64,
        mut role_schedule_counts: Vec<RoleScheduleCount>,
    ) -> Result<Self, ContractError> {
        role_schedule_counts.sort();
        if role_schedule_counts
            .windows(2)
            .any(|pair| pair[0].role == pair[1].role)
        {
            return Err(ContractError::new(
                "run.duplicate-role-count",
                "each semantic role schedule may appear once",
            ));
        }
        let value = Self {
            nodes_scheduled,
            nodes_completed,
            nodes_skipped,
            graphs_executed,
            systems_executed,
            role_schedule_counts,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the number of scheduled nodes.
    #[must_use]
    pub const fn nodes_scheduled(&self) -> u64 {
        self.nodes_scheduled
    }

    /// Returns the number of completed nodes.
    #[must_use]
    pub const fn nodes_completed(&self) -> u64 {
        self.nodes_completed
    }

    /// Returns the number of skipped nodes when the recorder observed it.
    #[must_use]
    pub const fn nodes_skipped(&self) -> Option<u64> {
        self.nodes_skipped
    }

    /// Returns the number of graph executions.
    #[must_use]
    pub const fn graphs_executed(&self) -> u64 {
        self.graphs_executed
    }

    /// Returns the number of system executions.
    #[must_use]
    pub const fn systems_executed(&self) -> u64 {
        self.systems_executed
    }

    /// Returns role-schedule counts in canonical order.
    #[must_use]
    pub fn role_schedule_counts(&self) -> &[RoleScheduleCount] {
        &self.role_schedule_counts
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted("run.role-counts", &self.role_schedule_counts)?;
        if self
            .role_schedule_counts
            .windows(2)
            .any(|pair| pair[0].role == pair[1].role)
        {
            return Err(ContractError::new(
                "run.duplicate-role-count",
                "each semantic role schedule may appear once",
            ));
        }
        if self.nodes_completed > self.nodes_scheduled
            || self.nodes_skipped.is_some_and(|skipped| {
                skipped > self.nodes_scheduled.saturating_sub(self.nodes_completed)
            })
        {
            return Err(ContractError::new(
                "run.structure-counts",
                "completed and skipped nodes cannot exceed scheduled nodes",
            ));
        }
        Ok(())
    }
}

/// Display-only control-flow choice; labels never key cohorts or policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlFlowChoice {
    point_label: String,
    selected_label: String,
}

impl ControlFlowChoice {
    /// Creates a bounded diagnostic choice.
    pub fn new(
        point_label: impl Into<String>,
        selected_label: impl Into<String>,
    ) -> Result<Self, ContractError> {
        let value = Self {
            point_label: point_label.into(),
            selected_label: selected_label.into(),
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the display-only choice-point label.
    #[must_use]
    pub fn point_label(&self) -> &str {
        &self.point_label
    }

    /// Returns the display-only selected label.
    #[must_use]
    pub fn selected_label(&self) -> &str {
        &self.selected_label
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_label(&self.point_label)?;
        validate_label(&self.selected_label)
    }
}

/// One bounded loop's mechanical completion record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRecord {
    loop_label: String,
    iterations_started: u64,
    iterations_completed: u64,
    stop_reason: Option<StopReason>,
}

/// Display-only completion counts for one parallel group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelGroupRecord {
    group_label: String,
    branches_started: u64,
    branches_completed: u64,
}

impl ParallelGroupRecord {
    /// Creates a bounded parallel-group diagnostic.
    pub fn new(
        group_label: impl Into<String>,
        branches_started: u64,
        branches_completed: u64,
    ) -> Result<Self, ContractError> {
        let value = Self {
            group_label: group_label.into(),
            branches_started,
            branches_completed,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the display-only group label.
    #[must_use]
    pub fn group_label(&self) -> &str {
        &self.group_label
    }

    /// Returns the number of parallel branches started.
    #[must_use]
    pub const fn branches_started(&self) -> u64 {
        self.branches_started
    }

    /// Returns the number of parallel branches completed.
    #[must_use]
    pub const fn branches_completed(&self) -> u64 {
        self.branches_completed
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_label(&self.group_label)?;
        if self.branches_completed > self.branches_started {
            Err(ContractError::new(
                "run.parallel-group-counts",
                "completed parallel branches cannot exceed started branches",
            ))
        } else {
            Ok(())
        }
    }
}

impl LoopRecord {
    /// Creates a loop record while preserving unavailable stop reasons as absent.
    pub fn new(
        loop_label: impl Into<String>,
        iterations_started: u64,
        iterations_completed: u64,
        stop_reason: Option<StopReason>,
    ) -> Result<Self, ContractError> {
        let value = Self {
            loop_label: loop_label.into(),
            iterations_started,
            iterations_completed,
            stop_reason,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the display-only loop label.
    #[must_use]
    pub fn loop_label(&self) -> &str {
        &self.loop_label
    }

    /// Returns the number of loop iterations started.
    #[must_use]
    pub const fn iterations_started(&self) -> u64 {
        self.iterations_started
    }

    /// Returns the number of loop iterations completed.
    #[must_use]
    pub const fn iterations_completed(&self) -> u64 {
        self.iterations_completed
    }

    /// Returns the loop's mechanical stop reason when observed.
    #[must_use]
    pub const fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_label(&self.loop_label)?;
        if self.iterations_completed > self.iterations_started {
            Err(ContractError::new(
                "run.loop-counts",
                "completed loop iterations cannot exceed started iterations",
            ))
        } else {
            Ok(())
        }
    }
}

/// Bounded diagnostic control-flow trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunControlFlow {
    branch_choices: Vec<ControlFlowChoice>,
    switch_choices: Vec<ControlFlowChoice>,
    dynamic_resolutions: Vec<ControlFlowChoice>,
    parallel_groups: Vec<ParallelGroupRecord>,
    loops: Vec<LoopRecord>,
}

impl RunControlFlow {
    /// Creates a control-flow trace in observed order.
    #[must_use]
    pub const fn new(
        branch_choices: Vec<ControlFlowChoice>,
        switch_choices: Vec<ControlFlowChoice>,
        dynamic_resolutions: Vec<ControlFlowChoice>,
        parallel_groups: Vec<ParallelGroupRecord>,
        loops: Vec<LoopRecord>,
    ) -> Self {
        Self {
            branch_choices,
            switch_choices,
            dynamic_resolutions,
            parallel_groups,
            loops,
        }
    }

    /// Creates an empty trace when no diagnostic control-flow data is available.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())
    }

    /// Returns branch choices in observed order.
    #[must_use]
    pub fn branch_choices(&self) -> &[ControlFlowChoice] {
        &self.branch_choices
    }

    /// Returns switch choices in observed order.
    #[must_use]
    pub fn switch_choices(&self) -> &[ControlFlowChoice] {
        &self.switch_choices
    }

    /// Returns dynamic-resolution choices in observed order.
    #[must_use]
    pub fn dynamic_resolutions(&self) -> &[ControlFlowChoice] {
        &self.dynamic_resolutions
    }

    /// Returns parallel-group diagnostics in observed order.
    #[must_use]
    pub fn parallel_groups(&self) -> &[ParallelGroupRecord] {
        &self.parallel_groups
    }

    /// Returns loop records in observed order.
    #[must_use]
    pub fn loops(&self) -> &[LoopRecord] {
        &self.loops
    }

    fn validate(&self) -> Result<(), ContractError> {
        for choice in self
            .branch_choices
            .iter()
            .chain(&self.switch_choices)
            .chain(&self.dynamic_resolutions)
        {
            choice.validate()?;
        }
        for loop_record in &self.loops {
            loop_record.validate()?;
        }
        for group in &self.parallel_groups {
            group.validate()?;
        }
        Ok(())
    }
}

/// Closed stage where a mechanical fault occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionStage {
    /// Pre-start live admission.
    Admission,
    /// Candidate or binding resolution.
    Resolution,
    /// Graph execution.
    Execution,
    /// Run finalization.
    Finalization,
}

/// One classified mechanical execution fault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionFault {
    error_class: ErrorClassId,
    stage: ExecutionStage,
    node_label: Option<String>,
}

impl ExecutionFault {
    /// Creates a classified fault with optional display-only node label.
    pub fn new(
        error_class: ErrorClassId,
        stage: ExecutionStage,
        node_label: Option<String>,
    ) -> Result<Self, ContractError> {
        if let Some(label) = &node_label {
            validate_label(label)?;
        }
        Ok(Self {
            error_class,
            stage,
            node_label,
        })
    }

    /// Returns the stable mechanical error class.
    #[must_use]
    pub const fn error_class(&self) -> &ErrorClassId {
        &self.error_class
    }

    /// Returns the stage where the fault occurred.
    #[must_use]
    pub const fn stage(&self) -> ExecutionStage {
        self.stage
    }

    /// Returns the optional display-only node label.
    #[must_use]
    pub fn node_label(&self) -> Option<&str> {
        self.node_label.as_deref()
    }

    fn validate(&self) -> Result<(), ContractError> {
        if let Some(label) = &self.node_label {
            validate_label(label)?;
        }
        Ok(())
    }
}

/// Source of an execution cancellation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CancellationSource {
    /// The host requested cancellation.
    Host,
    /// A budget controller requested cancellation.
    Budget,
    /// A timeout requested cancellation.
    Timeout,
    /// A supervisor requested cancellation.
    Supervisor,
}

/// Recorded cancellation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationRecord {
    requested_at: RecordedAt,
    source: CancellationSource,
}

impl CancellationRecord {
    /// Creates a cancellation record.
    #[must_use]
    pub const fn new(requested_at: RecordedAt, source: CancellationSource) -> Self {
        Self {
            requested_at,
            source,
        }
    }

    /// Returns the cancellation recording position.
    #[must_use]
    pub const fn requested_at(&self) -> RecordedAt {
        self.requested_at
    }

    /// Returns the cancellation source class.
    #[must_use]
    pub const fn source(&self) -> CancellationSource {
        self.source
    }
}

/// Fault, retry, skip, and cancellation summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFaults {
    errors: Vec<ExecutionFault>,
    retries: u64,
    skips: u64,
    cancellation: Option<CancellationRecord>,
}

impl RunFaults {
    /// Creates the mechanical fault summary in observed order.
    #[must_use]
    pub const fn new(
        errors: Vec<ExecutionFault>,
        retries: u64,
        skips: u64,
        cancellation: Option<CancellationRecord>,
    ) -> Self {
        Self {
            errors,
            retries,
            skips,
            cancellation,
        }
    }

    /// Creates an empty fault summary.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(Vec::new(), 0, 0, None)
    }

    /// Returns mechanical faults in observed order.
    #[must_use]
    pub fn errors(&self) -> &[ExecutionFault] {
        &self.errors
    }

    /// Returns the number of mechanical retries.
    #[must_use]
    pub const fn retries(&self) -> u64 {
        self.retries
    }

    /// Returns the number of mechanical skips.
    #[must_use]
    pub const fn skips(&self) -> u64 {
        self.skips
    }

    /// Returns cancellation details when cancellation was observed.
    #[must_use]
    pub const fn cancellation(&self) -> Option<CancellationRecord> {
        self.cancellation
    }

    fn validate(&self) -> Result<(), ContractError> {
        for error in &self.errors {
            error.validate()?;
        }
        Ok(())
    }
}

/// Provider/model token usage with an explicit accounting witness.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TokenUsage {
    provider: ProviderKey,
    model: ModelId,
    input: TokenQuantity,
    output: TokenQuantity,
}

impl TokenUsage {
    /// Creates one token-usage entry.
    pub fn new(
        provider: ProviderKey,
        model: ModelId,
        input: TokenQuantity,
        output: TokenQuantity,
    ) -> Result<Self, ContractError> {
        if input.accounting_id() != output.accounting_id() {
            return Err(ContractError::new(
                "run.token-accounting-mismatch",
                "input and output token usage require the same accounting scheme",
            ));
        }
        Ok(Self {
            provider,
            model,
            input,
            output,
        })
    }

    /// Returns the provider key.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Returns the stable model-family identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns input-token usage.
    #[must_use]
    pub const fn input(&self) -> &TokenQuantity {
        &self.input
    }

    /// Returns output-token usage.
    #[must_use]
    pub const fn output(&self) -> &TokenQuantity {
        &self.output
    }

    fn validate(&self) -> Result<(), ContractError> {
        if self.input.accounting_id() == self.output.accounting_id() {
            Ok(())
        } else {
            Err(ContractError::new(
                "run.token-accounting-mismatch",
                "input and output token usage require the same accounting scheme",
            ))
        }
    }
}

/// Mechanical resource-usage summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    tokens: Vec<TokenUsage>,
    estimated_cost: Option<super::Money>,
    pricing_policy: Option<PolicyRef>,
}

impl RunUsage {
    /// Creates a usage summary.
    pub fn new(
        mut tokens: Vec<TokenUsage>,
        estimated_cost: Option<super::Money>,
        pricing_policy: Option<PolicyRef>,
    ) -> Result<Self, ContractError> {
        if estimated_cost.is_some() != pricing_policy.is_some() {
            return Err(ContractError::new(
                "run.pricing-policy",
                "estimated cost and pricing policy must be present together",
            ));
        }
        tokens.sort();
        tokens.dedup();
        let value = Self {
            tokens,
            estimated_cost,
            pricing_policy,
        };
        value.validate()?;
        Ok(value)
    }

    /// Creates an empty usage summary.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tokens: Vec::new(),
            estimated_cost: None,
            pricing_policy: None,
        }
    }

    /// Returns token usage in canonical provider/model/accounting order.
    #[must_use]
    pub fn tokens(&self) -> &[TokenUsage] {
        &self.tokens
    }

    /// Returns the estimated run cost when recorded.
    #[must_use]
    pub const fn estimated_cost(&self) -> Option<super::Money> {
        self.estimated_cost
    }

    /// Returns the policy used to calculate estimated cost.
    #[must_use]
    pub const fn pricing_policy(&self) -> Option<&PolicyRef> {
        self.pricing_policy.as_ref()
    }

    fn validate(&self) -> Result<(), ContractError> {
        if self.estimated_cost.is_some() != self.pricing_policy.is_some() {
            return Err(ContractError::new(
                "run.pricing-policy",
                "estimated cost and pricing policy must be present together",
            ));
        }
        for usage in &self.tokens {
            usage.validate()?;
        }
        validate_strictly_sorted("run.token-usage", &self.tokens)?;
        if self.tokens.windows(2).any(|pair| {
            pair[0].provider == pair[1].provider
                && pair[0].model == pair[1].model
                && pair[0].input.accounting_id() == pair[1].input.accounting_id()
        }) {
            return Err(ContractError::new(
                "run.duplicate-token-usage",
                "each provider, model, and accounting scheme may appear once",
            ));
        }
        Ok(())
    }
}

/// Mechanical completion facts; this is never a task-success judgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCompletion {
    disposition: ExecutionDisposition,
    stop_reason: Option<StopReason>,
    reported_coverage: Option<CoveragePpm>,
    unresolved_residuals: Option<u64>,
}

impl RunCompletion {
    /// Creates a writable finalization disposition.
    pub fn new(
        disposition: ExecutionDisposition,
        stop_reason: Option<StopReason>,
        reported_coverage: Option<CoveragePpm>,
        unresolved_residuals: Option<u64>,
    ) -> Result<Self, ContractError> {
        if disposition == ExecutionDisposition::Abandoned {
            return Err(ContractError::new(
                "run.abandonment-requires-event",
                "abandonment must be recorded through an explicit RunAbandoned event",
            ));
        }
        Ok(Self {
            disposition,
            stop_reason,
            reported_coverage,
            unresolved_residuals,
        })
    }

    /// Returns the mechanical execution disposition.
    #[must_use]
    pub const fn disposition(&self) -> ExecutionDisposition {
        self.disposition
    }

    /// Returns the mechanical stop reason when one was reported.
    #[must_use]
    pub const fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }

    /// Returns graph-reported mechanical coverage, without interpreting success.
    #[must_use]
    pub const fn reported_coverage(&self) -> Option<CoveragePpm> {
        self.reported_coverage
    }

    /// Returns the graph-reported unresolved residual count.
    #[must_use]
    pub const fn unresolved_residuals(&self) -> Option<u64> {
        self.unresolved_residuals
    }

    fn validate(&self) -> Result<(), ContractError> {
        if self.disposition == ExecutionDisposition::Abandoned {
            Err(ContractError::new(
                "run.abandonment-requires-event",
                "abandonment must be recorded through an explicit RunAbandoned event",
            ))
        } else {
            Ok(())
        }
    }
}

/// Exactly-once mechanical terminal append for a completed, failed, or cancelled run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFinalized {
    schema_version: SchemaVersion,
    run_id: RunId,
    recorded_at: RecordedAt,
    execution_duration_ms: super::DurationMs,
    structure: RunStructure,
    control_flow: RunControlFlow,
    faults: RunFaults,
    usage: RunUsage,
    completion: RunCompletion,
    evidence_refs: Vec<EvidenceRef>,
    content_digest: ContentDigest,
}

impl RunFinalized {
    /// Creates a mechanical run finalization without interpreting correctness.
    #[expect(
        clippy::too_many_arguments,
        reason = "the finalization is a complete mechanical record"
    )]
    pub fn new(
        run_id: RunId,
        recorded_at: RecordedAt,
        execution_duration_ms: super::DurationMs,
        structure: RunStructure,
        control_flow: RunControlFlow,
        faults: RunFaults,
        usage: RunUsage,
        completion: RunCompletion,
        mut evidence_refs: Vec<EvidenceRef>,
    ) -> Result<Self, ContractError> {
        evidence_refs.sort();
        evidence_refs.dedup();
        let mut event = Self {
            schema_version: SchemaVersion::V1,
            run_id,
            recorded_at,
            execution_duration_ms,
            structure,
            control_flow,
            faults,
            usage,
            completion,
            evidence_refs,
            content_digest: ContentDigest::sha256(&[]),
        };
        event.validate_structure()?;
        event.content_digest = content_digest(&event.content_view())?;
        Ok(event)
    }

    /// Returns the finalized run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the terminal recording position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns mechanical completion facts.
    #[must_use]
    pub const fn completion(&self) -> &RunCompletion {
        &self.completion
    }

    /// Returns measured execution duration.
    #[must_use]
    pub const fn execution_duration_ms(&self) -> super::DurationMs {
        self.execution_duration_ms
    }

    /// Returns mechanical structure counts.
    #[must_use]
    pub const fn structure(&self) -> &RunStructure {
        &self.structure
    }

    /// Returns bounded display-only control-flow diagnostics.
    #[must_use]
    pub const fn control_flow(&self) -> &RunControlFlow {
        &self.control_flow
    }

    /// Returns mechanical fault, retry, skip, and cancellation facts.
    #[must_use]
    pub const fn faults(&self) -> &RunFaults {
        &self.faults
    }

    /// Returns mechanical resource usage.
    #[must_use]
    pub const fn usage(&self) -> &RunUsage {
        &self.usage
    }

    /// Returns immutable evidence references in canonical order.
    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.structure.validate()?;
        self.control_flow.validate()?;
        self.faults.validate()?;
        self.usage.validate()?;
        self.completion.validate()?;
        validate_strictly_sorted("run.evidence-refs", &self.evidence_refs)
    }

    fn content_view(&self) -> RunFinalizedContent<'_> {
        RunFinalizedContent {
            schema_version: self.schema_version,
            run_id: self.run_id,
            sequence: self.recorded_at.sequence(),
            execution_duration_ms: self.execution_duration_ms,
            structure: &self.structure,
            control_flow: &self.control_flow,
            faults: &self.faults,
            usage: &self.usage,
            completion: &self.completion,
            evidence_refs: &self.evidence_refs,
        }
    }
}

#[derive(Serialize)]
struct RunFinalizedContent<'a> {
    schema_version: SchemaVersion,
    run_id: RunId,
    sequence: LogicalSequence,
    execution_duration_ms: super::DurationMs,
    structure: &'a RunStructure,
    control_flow: &'a RunControlFlow,
    faults: &'a RunFaults,
    usage: &'a RunUsage,
    completion: &'a RunCompletion,
    evidence_refs: &'a [EvidenceRef],
}

impl CanonicalRecord for RunFinalized {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "run-finalized event",
        )
    }
}

/// Explicit supervisor or recovery append for an opened run that cannot finalize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAbandoned {
    schema_version: SchemaVersion,
    run_id: RunId,
    recorded_at: RecordedAt,
    recovery_policy: PolicyRef,
    provenance: Vec<Provenance>,
    evidence_refs: Vec<EvidenceRef>,
    content_digest: ContentDigest,
}

impl RunAbandoned {
    /// Creates an explicit abandonment event; elapsed time alone can never create one.
    pub fn new(
        run_id: RunId,
        recorded_at: RecordedAt,
        recovery_policy: PolicyRef,
        provenance: Vec<Provenance>,
        mut evidence_refs: Vec<EvidenceRef>,
    ) -> Result<Self, ContractError> {
        evidence_refs.sort();
        evidence_refs.dedup();
        let mut event = Self {
            schema_version: SchemaVersion::V1,
            run_id,
            recorded_at,
            recovery_policy,
            provenance: canonical_provenance(provenance)?,
            evidence_refs,
            content_digest: ContentDigest::sha256(&[]),
        };
        event.validate_structure()?;
        event.content_digest = content_digest(&event.content_view())?;
        Ok(event)
    }

    /// Returns the abandoned run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the terminal recording position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns the named supervisor or recovery policy.
    #[must_use]
    pub const fn recovery_policy(&self) -> &PolicyRef {
        &self.recovery_policy
    }

    /// Returns abandonment provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns immutable evidence references in canonical order.
    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        validate_provenance(&self.provenance)?;
        validate_strictly_sorted("run.abandonment-evidence-refs", &self.evidence_refs)
    }

    fn content_view(&self) -> RunAbandonedContent<'_> {
        RunAbandonedContent {
            schema_version: self.schema_version,
            run_id: self.run_id,
            sequence: self.recorded_at.sequence(),
            recovery_policy: &self.recovery_policy,
            provenance: provenance_content(&self.provenance),
            evidence_refs: &self.evidence_refs,
        }
    }
}

#[derive(Serialize)]
struct RunAbandonedContent<'a> {
    schema_version: SchemaVersion,
    run_id: RunId,
    sequence: LogicalSequence,
    recovery_policy: &'a PolicyRef,
    provenance: Vec<ProvenanceContent<'a>>,
    evidence_refs: &'a [EvidenceRef],
}

impl CanonicalRecord for RunAbandoned {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "run-abandoned event",
        )
    }
}

/// Explicit terminal append joined to an opened run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[expect(
    clippy::large_enum_variant,
    reason = "terminal records stay inline so their canonical wire shape has no storage indirection"
)]
pub enum RunTerminal {
    /// Normal mechanical finalization.
    Finalized(RunFinalized),
    /// Explicit recovery or supervisor abandonment.
    Abandoned(RunAbandoned),
}

impl RunTerminal {
    /// Returns the terminal run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        match self {
            Self::Finalized(value) => value.run_id(),
            Self::Abandoned(value) => value.run_id(),
        }
    }

    /// Returns the terminal recording position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        match self {
            Self::Finalized(value) => value.recorded_at(),
            Self::Abandoned(value) => value.recorded_at(),
        }
    }

    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Finalized(value) => value.validate(),
            Self::Abandoned(value) => value.validate(),
        }
    }
}

/// Complete read projection joining one opening with exactly one terminal append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyRunRecord {
    schema_version: SchemaVersion,
    opened: RunOpened,
    terminal: RunTerminal,
    content_digest: ContentDigest,
}

impl TopologyRunRecord {
    /// Joins a run opening and terminal append without interpreting task success.
    pub fn new(opened: RunOpened, terminal: RunTerminal) -> Result<Self, ContractError> {
        let mut record = Self {
            schema_version: SchemaVersion::V1,
            opened,
            terminal,
            content_digest: ContentDigest::sha256(&[]),
        };
        record.validate_structure()?;
        record.content_digest = content_digest(&record.content_view())?;
        Ok(record)
    }

    /// Returns the run opening.
    #[must_use]
    pub const fn opened(&self) -> &RunOpened {
        &self.opened
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the explicit terminal append.
    #[must_use]
    pub const fn terminal(&self) -> &RunTerminal {
        &self.terminal
    }

    /// Returns the mechanical terminal disposition.
    #[must_use]
    pub const fn disposition(&self) -> ExecutionDisposition {
        match &self.terminal {
            RunTerminal::Finalized(value) => value.completion().disposition(),
            RunTerminal::Abandoned(_) => ExecutionDisposition::Abandoned,
        }
    }

    /// Returns the canonical read-projection digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.opened.validate()?;
        self.terminal.validate()?;
        if self.opened.run_id() != self.terminal.run_id() {
            return Err(ContractError::new(
                "run.terminal-id-mismatch",
                "run opening and terminal append must use the same RunId",
            ));
        }
        if self.terminal.recorded_at().sequence() <= self.opened.recorded_at().sequence() {
            return Err(ContractError::new(
                "run.terminal-order",
                "run terminal append must follow RunOpened in store order",
            ));
        }
        Ok(())
    }

    fn content_view(&self) -> TopologyRunContent<'_> {
        TopologyRunContent {
            schema_version: self.schema_version,
            opened: &self.opened,
            terminal: &self.terminal,
        }
    }
}

#[derive(Serialize)]
struct TopologyRunContent<'a> {
    schema_version: SchemaVersion,
    opened: &'a RunOpened,
    terminal: &'a RunTerminal,
}

impl CanonicalRecord for TopologyRunRecord {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "topology run record",
        )
    }
}

/// One named outcome measurement entry in canonical registry order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementEntry {
    id: MeasurementId,
    measurement: Measurement,
}

impl MeasurementEntry {
    /// Creates a named measurement and applies known-v1 safe validation.
    pub fn new(id: MeasurementId, measurement: Measurement) -> Result<Self, ContractError> {
        validate_measurement(&id, &measurement)?;
        Ok(Self { id, measurement })
    }

    /// Returns the extensible measurement identity.
    #[must_use]
    pub const fn id(&self) -> &MeasurementId {
        &self.id
    }

    /// Returns the attributed measurement.
    #[must_use]
    pub const fn measurement(&self) -> &Measurement {
        &self.measurement
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_measurement(&self.id, &self.measurement)
    }
}

/// Compact verifier result without raw domain output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierResult {
    verifier: SourceKey,
    status: Status,
    capabilities: Vec<super::VerifierCapability>,
    checks_total: Option<u64>,
    checks_passed: Option<u64>,
    evidence_refs: Vec<EvidenceRef>,
}

impl VerifierResult {
    /// Creates a compact verifier result.
    pub fn new(
        verifier: SourceKey,
        status: Status,
        mut capabilities: Vec<super::VerifierCapability>,
        checks_total: Option<u64>,
        checks_passed: Option<u64>,
        mut evidence_refs: Vec<EvidenceRef>,
    ) -> Result<Self, ContractError> {
        capabilities.sort_unstable();
        capabilities.dedup();
        evidence_refs.sort();
        evidence_refs.dedup();
        let value = Self {
            verifier,
            status,
            capabilities,
            checks_total,
            checks_passed,
            evidence_refs,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the verifier identity and version.
    #[must_use]
    pub const fn verifier(&self) -> &SourceKey {
        &self.verifier
    }

    /// Returns the verifier's status judgement.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }

    /// Returns verifier capabilities in canonical order.
    #[must_use]
    pub fn capabilities(&self) -> &[super::VerifierCapability] {
        &self.capabilities
    }

    /// Returns total checks when count accounting was reported.
    #[must_use]
    pub const fn checks_total(&self) -> Option<u64> {
        self.checks_total
    }

    /// Returns passed checks when count accounting was reported.
    #[must_use]
    pub const fn checks_passed(&self) -> Option<u64> {
        self.checks_passed
    }

    /// Returns verifier evidence in canonical order.
    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted("outcome.verifier-capabilities", &self.capabilities)?;
        validate_strictly_sorted("outcome.verifier-evidence", &self.evidence_refs)?;
        if self.checks_total.is_some() != self.checks_passed.is_some()
            || self
                .checks_total
                .zip(self.checks_passed)
                .is_some_and(|(total, passed)| passed > total)
        {
            return Err(ContractError::new(
                "outcome.verifier-counts",
                "verifier check counts must be present together with passed <= total",
            ));
        }
        Ok(())
    }
}

/// Portable summary of unresolved work without embedding its domain payload.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResidualSummary {
    residual_class: RecordSubjectId,
    count: u64,
    disposition: ResidualDisposition,
    evidence_refs: Vec<EvidenceRef>,
}

impl ResidualSummary {
    /// Creates one residual summary.
    #[must_use]
    pub fn new(
        residual_class: RecordSubjectId,
        count: u64,
        disposition: ResidualDisposition,
        mut evidence_refs: Vec<EvidenceRef>,
    ) -> Self {
        evidence_refs.sort();
        evidence_refs.dedup();
        Self {
            residual_class,
            count,
            disposition,
            evidence_refs,
        }
    }

    /// Returns the portable residual class.
    #[must_use]
    pub const fn residual_class(&self) -> &RecordSubjectId {
        &self.residual_class
    }

    /// Returns the number of residuals in this class.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Returns the reported residual disposition.
    #[must_use]
    pub const fn disposition(&self) -> ResidualDisposition {
        self.disposition
    }

    /// Returns supporting evidence references in canonical order.
    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted("outcome.residual-evidence", &self.evidence_refs)
    }
}

/// Closed reason one observer claims an earlier observation should be superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupersedeReason {
    /// The earlier observation contained an error.
    Corrected,
    /// The earlier observation is withdrawn.
    Retracted,
    /// Later evidence supports a replacement judgement.
    SupersededByLaterEvidence,
}

/// Append-only claim that an earlier observation should lose precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersedesClaim {
    observation_id: OutcomeObservationId,
    reason: SupersedeReason,
}

impl SupersedesClaim {
    /// Creates a supersession claim without editing the earlier observation.
    #[must_use]
    pub const fn new(observation_id: OutcomeObservationId, reason: SupersedeReason) -> Self {
        Self {
            observation_id,
            reason,
        }
    }

    /// Returns the earlier observation named by this claim.
    #[must_use]
    pub const fn observation_id(&self) -> OutcomeObservationId {
        self.observation_id
    }

    /// Returns the claimed supersession reason.
    #[must_use]
    pub const fn reason(&self) -> SupersedeReason {
        self.reason
    }
}

/// Separate, delayed, and potentially conflicting judgement about a finalized run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeObservation {
    schema_version: SchemaVersion,
    observation_id: OutcomeObservationId,
    run_id: RunId,
    task_id: TaskId,
    recorded_at: RecordedAt,
    observed_at: Option<EvidenceTime>,
    delay_after_run_ms: Option<super::DurationMs>,
    status: Status,
    disposition: Option<ResidualDisposition>,
    measurements: Vec<MeasurementEntry>,
    verifier: Option<VerifierResult>,
    unresolved_residuals: Vec<ResidualSummary>,
    supersedes_claim: Option<SupersedesClaim>,
    source: SourceKey,
    source_kind: SourceKind,
    confidence: ConfidencePpm,
    provenance: Vec<Provenance>,
    evidence_refs: Vec<EvidenceRef>,
    content_digest: ContentDigest,
}

impl OutcomeObservation {
    /// Creates an append-only outcome observation without changing run evidence.
    #[expect(
        clippy::too_many_arguments,
        reason = "the outcome shape is an audit record"
    )]
    pub fn new(
        observation_id: OutcomeObservationId,
        run_id: RunId,
        task_id: TaskId,
        recorded_at: RecordedAt,
        observed_at: Option<EvidenceTime>,
        delay_after_run_ms: Option<super::DurationMs>,
        status: Status,
        disposition: Option<ResidualDisposition>,
        mut measurements: Vec<MeasurementEntry>,
        verifier: Option<VerifierResult>,
        mut unresolved_residuals: Vec<ResidualSummary>,
        supersedes_claim: Option<SupersedesClaim>,
        source: SourceKey,
        source_kind: SourceKind,
        confidence: ConfidencePpm,
        provenance: Vec<Provenance>,
        mut evidence_refs: Vec<EvidenceRef>,
    ) -> Result<Self, ContractError> {
        measurements.sort_by(|left, right| left.id.cmp(&right.id));
        unresolved_residuals.sort();
        unresolved_residuals.dedup();
        evidence_refs.sort();
        evidence_refs.dedup();
        let mut observation = Self {
            schema_version: SchemaVersion::V1,
            observation_id,
            run_id,
            task_id,
            recorded_at,
            observed_at,
            delay_after_run_ms,
            status,
            disposition,
            measurements,
            verifier,
            unresolved_residuals,
            supersedes_claim,
            source,
            source_kind,
            confidence,
            provenance: canonical_provenance(provenance)?,
            evidence_refs,
            content_digest: ContentDigest::sha256(&[]),
        };
        observation.validate_structure()?;
        observation.content_digest = content_digest(&observation.content_view())?;
        Ok(observation)
    }

    /// Returns the observation event identity.
    #[must_use]
    pub const fn observation_id(&self) -> OutcomeObservationId {
        self.observation_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the judged run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the host-owned task identity.
    #[must_use]
    pub const fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// Returns the store-assigned observation position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns the source's observation time when supplied.
    #[must_use]
    pub const fn observed_at(&self) -> Option<&EvidenceTime> {
        self.observed_at.as_ref()
    }

    /// Returns explicitly measured delay after the run when supplied.
    #[must_use]
    pub const fn delay_after_run_ms(&self) -> Option<super::DurationMs> {
        self.delay_after_run_ms
    }

    /// Returns the judgement status.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }

    /// Returns canonical named measurements, including unknown registry entries.
    #[must_use]
    pub fn measurements(&self) -> &[MeasurementEntry] {
        &self.measurements
    }

    /// Returns the outcome-level residual disposition when supplied.
    #[must_use]
    pub const fn disposition(&self) -> Option<ResidualDisposition> {
        self.disposition
    }

    /// Returns a compact verifier result when supplied.
    #[must_use]
    pub const fn verifier(&self) -> Option<&VerifierResult> {
        self.verifier.as_ref()
    }

    /// Returns unresolved residual summaries in canonical order.
    #[must_use]
    pub fn unresolved_residuals(&self) -> &[ResidualSummary] {
        &self.unresolved_residuals
    }

    /// Returns an append-only supersession claim when supplied.
    #[must_use]
    pub const fn supersedes_claim(&self) -> Option<SupersedesClaim> {
        self.supersedes_claim
    }

    /// Returns the observation source identity.
    #[must_use]
    pub const fn source(&self) -> &SourceKey {
        &self.source
    }

    /// Returns the observation source class.
    #[must_use]
    pub const fn source_kind(&self) -> SourceKind {
        self.source_kind
    }

    /// Returns confidence in this judgement.
    #[must_use]
    pub const fn confidence(&self) -> ConfidencePpm {
        self.confidence
    }

    /// Returns observation provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns immutable evidence references in canonical order.
    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    /// Returns the canonical observation content digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        validate_provenance(&self.provenance)?;
        if matches!(
            self.source_kind,
            SourceKind::RequestDeclaration | SourceKind::CosmosPolicy | SourceKind::PolicyDefault
        ) {
            return Err(ContractError::new(
                "outcome.invalid-source-kind",
                "outcome judgements require an observation, verifier, review, or downstream source",
            ));
        }
        if !self.provenance.iter().any(|provenance| {
            provenance.source() == &self.source && provenance.source_kind() == self.source_kind
        }) {
            return Err(ContractError::new(
                "outcome.source-provenance-mismatch",
                "outcome source identity and kind must appear in its provenance",
            ));
        }
        validate_strictly_sorted("outcome.evidence-refs", &self.evidence_refs)?;
        if self
            .measurements
            .windows(2)
            .any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(ContractError::new(
                "outcome.non-canonical-measurements",
                "measurement IDs must be strictly sorted and unique",
            ));
        }
        for entry in &self.measurements {
            entry.validate()?;
        }
        if let Some(verifier) = &self.verifier {
            verifier.validate()?;
            if verifier.status() != self.status {
                return Err(ContractError::new(
                    "outcome.verifier-status-mismatch",
                    "the retained verifier result must agree with the observation status",
                ));
            }
        }
        for residual in &self.unresolved_residuals {
            residual.validate()?;
        }
        validate_strictly_sorted("outcome.residuals", &self.unresolved_residuals)?;
        if self
            .unresolved_residuals
            .windows(2)
            .any(|pair| pair[0].residual_class == pair[1].residual_class)
        {
            return Err(ContractError::new(
                "outcome.duplicate-residual-class",
                "each residual class may appear once in an observation",
            ));
        }
        if self
            .supersedes_claim
            .is_some_and(|claim| claim.observation_id == self.observation_id)
        {
            return Err(ContractError::new(
                "outcome.self-supersession",
                "an observation cannot supersede itself",
            ));
        }
        if let Some(success) = self
            .measurements
            .iter()
            .find(|entry| entry.id.as_str() == "verified-success")
        {
            let MeasurementValue::Proportion(value) = success.measurement.value() else {
                return Err(ContractError::new(
                    "outcome.verified-success-type",
                    "verified-success must be a proportion",
                ));
            };
            let valid = match self.status {
                Status::Pass => *value == CoveragePpm::ONE,
                Status::Issue => *value == CoveragePpm::ZERO,
                Status::NotApplicable | Status::Indeterminate => false,
            };
            if !valid {
                return Err(ContractError::new(
                    "outcome.verified-success-status",
                    "binary verified-success must agree with pass or issue status",
                ));
            }
        }
        Ok(())
    }

    fn content_view(&self) -> OutcomeContent<'_> {
        OutcomeContent {
            schema_version: self.schema_version,
            observation_id: self.observation_id,
            run_id: self.run_id,
            task_id: &self.task_id,
            sequence: self.recorded_at.sequence(),
            delay_after_run_ms: self.delay_after_run_ms,
            status: self.status,
            disposition: self.disposition,
            measurements: &self.measurements,
            verifier: self.verifier.as_ref(),
            unresolved_residuals: &self.unresolved_residuals,
            supersedes_claim: self.supersedes_claim,
            source: &self.source,
            source_kind: self.source_kind,
            confidence: self.confidence,
            provenance: provenance_content(&self.provenance),
            evidence_refs: &self.evidence_refs,
        }
    }
}

#[derive(Serialize)]
struct OutcomeContent<'a> {
    schema_version: SchemaVersion,
    observation_id: OutcomeObservationId,
    run_id: RunId,
    task_id: &'a TaskId,
    sequence: LogicalSequence,
    delay_after_run_ms: Option<super::DurationMs>,
    status: Status,
    disposition: Option<ResidualDisposition>,
    measurements: &'a [MeasurementEntry],
    verifier: Option<&'a VerifierResult>,
    unresolved_residuals: &'a [ResidualSummary],
    supersedes_claim: Option<SupersedesClaim>,
    source: &'a SourceKey,
    source_kind: SourceKind,
    confidence: ConfidencePpm,
    provenance: Vec<ProvenanceContent<'a>>,
    evidence_refs: &'a [EvidenceRef],
}

impl CanonicalRecord for OutcomeObservation {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "outcome observation",
        )
    }
}

/// Evidence-store window attached to a statistics snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatisticsWindow {
    observed_through: LogicalSequence,
    earliest_retained_sequence: LogicalSequence,
    evicted_episodes_total: u64,
    dropped_append_total: u64,
}

impl StatisticsWindow {
    /// Creates a retained evidence window.
    pub fn new(
        observed_through: LogicalSequence,
        earliest_retained_sequence: LogicalSequence,
        evicted_episodes_total: u64,
        dropped_append_total: u64,
    ) -> Result<Self, ContractError> {
        if earliest_retained_sequence > observed_through {
            return Err(ContractError::new(
                "statistics.window-order",
                "earliest retained sequence cannot follow the watermark",
            ));
        }
        Ok(Self {
            observed_through,
            earliest_retained_sequence,
            evicted_episodes_total,
            dropped_append_total,
        })
    }

    /// Returns the evidence-store watermark included in the snapshot.
    #[must_use]
    pub const fn observed_through(&self) -> LogicalSequence {
        self.observed_through
    }

    /// Returns the earliest retained evidence sequence.
    #[must_use]
    pub const fn earliest_retained_sequence(&self) -> LogicalSequence {
        self.earliest_retained_sequence
    }

    /// Returns the cumulative number of evicted episodes.
    #[must_use]
    pub const fn evicted_episodes_total(&self) -> u64 {
        self.evicted_episodes_total
    }

    /// Returns the cumulative number of dropped appends.
    #[must_use]
    pub const fn dropped_append_total(&self) -> u64 {
        self.dropped_append_total
    }
}

/// Synthetic empty statistics input with a zero-sample cohort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyStatisticsSnapshot {
    schema_version: SchemaVersion,
    statistics_policy: PolicyRef,
    observation_selection_policy: PolicyRef,
    interval_policy: PolicyRef,
    captured_at: RecordedAt,
    window: StatisticsWindow,
    minimum_sample_count: u64,
    content_digest: ContentDigest,
}

impl EmptyStatisticsSnapshot {
    /// Creates a deterministic zero-cohort statistics snapshot.
    pub fn new(
        statistics_policy: PolicyRef,
        observation_selection_policy: PolicyRef,
        interval_policy: PolicyRef,
        captured_at: RecordedAt,
        window: StatisticsWindow,
        minimum_sample_count: u64,
    ) -> Result<Self, ContractError> {
        if minimum_sample_count == 0 {
            return Err(ContractError::new(
                "statistics.zero-minimum-sample",
                "minimum sample count must be non-zero",
            ));
        }
        let mut snapshot = Self {
            schema_version: SchemaVersion::V1,
            statistics_policy,
            observation_selection_policy,
            interval_policy,
            captured_at,
            window,
            minimum_sample_count,
            content_digest: ContentDigest::sha256(&[]),
        };
        snapshot.content_digest = content_digest(&snapshot.content_view())?;
        Ok(snapshot)
    }

    /// Returns the stable content identity used by selection decisions.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the statistics policy identity and version.
    #[must_use]
    pub const fn statistics_policy(&self) -> &PolicyRef {
        &self.statistics_policy
    }

    /// Returns the observation-selection policy identity and version.
    #[must_use]
    pub const fn observation_selection_policy(&self) -> &PolicyRef {
        &self.observation_selection_policy
    }

    /// Returns the interval policy identity and version.
    #[must_use]
    pub const fn interval_policy(&self) -> &PolicyRef {
        &self.interval_policy
    }

    /// Returns the snapshot recording position.
    #[must_use]
    pub const fn captured_at(&self) -> RecordedAt {
        self.captured_at
    }

    /// Returns the retained evidence window.
    #[must_use]
    pub const fn window(&self) -> StatisticsWindow {
        self.window
    }

    /// Returns the minimum cohort sample count.
    #[must_use]
    pub const fn minimum_sample_count(&self) -> u64 {
        self.minimum_sample_count
    }

    fn content_view(&self) -> EmptyStatisticsContent<'_> {
        EmptyStatisticsContent {
            schema_version: self.schema_version,
            statistics_policy: &self.statistics_policy,
            observation_selection_policy: &self.observation_selection_policy,
            interval_policy: &self.interval_policy,
            sequence: self.captured_at.sequence(),
            window: self.window,
            minimum_sample_count: self.minimum_sample_count,
            cohorts: &[],
        }
    }
}

#[derive(Serialize)]
struct EmptyStatisticsContent<'a> {
    schema_version: SchemaVersion,
    statistics_policy: &'a PolicyRef,
    observation_selection_policy: &'a PolicyRef,
    interval_policy: &'a PolicyRef,
    sequence: LogicalSequence,
    window: StatisticsWindow,
    minimum_sample_count: u64,
    cohorts: &'static [ContentDigest],
}

impl CanonicalRecord for EmptyStatisticsSnapshot {
    fn validate(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        if self.minimum_sample_count == 0 {
            return Err(ContractError::new(
                "statistics.zero-minimum-sample",
                "minimum sample count must be non-zero",
            ));
        }
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "empty statistics snapshot",
        )
    }
}

fn validate_label(value: &str) -> Result<(), ContractError> {
    if value.is_empty() || value.len() > 256 {
        Err(ContractError::new(
            "run.diagnostic-label",
            "diagnostic labels must contain 1..=256 bytes",
        ))
    } else {
        Ok(())
    }
}

fn validate_provenance(values: &[Provenance]) -> Result<(), ContractError> {
    if values.is_empty() || values.windows(2).any(|pair| pair[0] >= pair[1]) {
        Err(ContractError::new(
            "provenance.non-canonical",
            "provenance must be non-empty, sorted, and duplicate-free",
        ))
    } else {
        Ok(())
    }
}

fn provenance_content(values: &[Provenance]) -> Vec<ProvenanceContent<'_>> {
    values.iter().map(Provenance::content_view).collect()
}

fn require_digest(
    actual: ContentDigest,
    expected: ContentDigest,
    record_name: &'static str,
) -> Result<(), ContractError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ContractError::new(
            "identity.digest-mismatch",
            format!("{record_name} digest does not match its canonical content"),
        ))
    }
}
