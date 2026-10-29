//! Append-only topology evidence storage and contextual aggregate snapshots.
//!
//! This module is intentionally a leaf implementation; crate-root exports and
//! plugin wiring live elsewhere.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::contracts::{
    AdmissionAttemptId, AdmissionEvaluation, CanonicalRecord, ConsequenceLevel, ContentDigest,
    DecompositionShape, EmbodimentKey, ExecutionDisposition, LogicalSequence, MeasurementEntry,
    MeasurementId, MeasurementValue, ModelId, OutcomeObservation, OutcomeObservationId, PolicyRef,
    Provenance, ProviderKey, RecordedAt, RunAbandoned, RunFinalized, RunId, RunOpened, RunTerminal,
    SelectionDecisionId, SelectionNotStarted, SelectionOutcome, SelectionRecord, StatisticsWindow,
    Status, TaskHorizon, VerifierAvailability,
};
use crate::topology::{ConfigurationDigest, RegistrationId, TopologyContractKey, TopologyKey};

/// Current evidence-facade contract version.
pub const TOPOLOGY_EVIDENCE_VERSION: u16 = 1;

/// Bounded entry capacities for the in-memory evidence provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedEvidenceConfig {
    /// Maximum retained selection episodes.
    pub max_episodes: NonZeroUsize,
    /// Maximum retained started runs.
    pub max_runs: NonZeroUsize,
    /// Maximum retained observations across all runs.
    pub max_observations_total: NonZeroUsize,
    /// Maximum observations retained for one run.
    pub max_observations_per_run: NonZeroUsize,
    /// Maximum explicit evidence references on one append.
    pub max_evidence_refs_per_record: NonZeroUsize,
    /// Maximum simultaneously open runs.
    pub max_open_runs: NonZeroUsize,
}

impl Default for BoundedEvidenceConfig {
    fn default() -> Self {
        const fn nonzero(value: usize) -> NonZeroUsize {
            match NonZeroUsize::new(value) {
                Some(value) => value,
                None => unreachable!(),
            }
        }
        Self {
            max_episodes: nonzero(256),
            max_runs: nonzero(256),
            max_observations_total: nonzero(1_024),
            max_observations_per_run: nonzero(16),
            max_evidence_refs_per_record: nonzero(64),
            max_open_runs: nonzero(32),
        }
    }
}

/// Stable outcome of an idempotent append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendReceipt {
    sequence: LogicalSequence,
    inserted: bool,
}

impl AppendReceipt {
    /// Constructs a provider receipt for a newly inserted event.
    #[must_use]
    pub const fn inserted(sequence: LogicalSequence) -> Self {
        Self {
            sequence,
            inserted: true,
        }
    }

    /// Constructs a provider receipt for an idempotent retry.
    #[must_use]
    pub const fn idempotent(sequence: LogicalSequence) -> Self {
        Self {
            sequence,
            inserted: false,
        }
    }

    /// Returns the event's store-local sequence.
    #[must_use]
    pub const fn sequence(self) -> LogicalSequence {
        self.sequence
    }

    /// Returns `true` for a new append and `false` for an idempotent retry.
    #[must_use]
    pub const fn was_inserted(self) -> bool {
        self.inserted
    }
}

/// Typed evidence-store error with a stable machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceError {
    code: &'static str,
    detail: String,
}

impl EvidenceError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Constructs a typed error in a replacement evidence provider.
    #[must_use]
    pub fn provider(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(code, detail)
    }

    /// Returns the stable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Returns display-oriented diagnostic detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for EvidenceError {}

/// One policy-attributed contribution retained after outcome observations.
///
/// This is deliberately a vector of named measurements, not a canonical
/// reward. Different policies may append conflicting assignments unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditAssignment {
    credit_id: String,
    policy: PolicyRef,
    run_id: RunId,
    decision_id: SelectionDecisionId,
    observation_ids: Vec<OutcomeObservationId>,
    contributions: Vec<MeasurementEntry>,
    recorded_at: RecordedAt,
    provenance: Vec<Provenance>,
    content_digest: ContentDigest,
}

impl CreditAssignment {
    /// Creates an immutable, attributed credit append.
    #[expect(
        clippy::too_many_arguments,
        reason = "credit joins its identity, policy, correlations, values, and attribution"
    )]
    pub fn new(
        credit_id: impl Into<String>,
        policy: PolicyRef,
        run_id: RunId,
        decision_id: SelectionDecisionId,
        mut observation_ids: Vec<OutcomeObservationId>,
        mut contributions: Vec<MeasurementEntry>,
        recorded_at: RecordedAt,
        mut provenance: Vec<Provenance>,
    ) -> Result<Self, EvidenceError> {
        let credit_id = credit_id.into();
        validate_credit_id(&credit_id)?;
        observation_ids.sort_unstable();
        observation_ids.dedup();
        if observation_ids.is_empty() {
            return Err(EvidenceError::new(
                "credit.missing-observation",
                "credit requires at least one outcome observation",
            ));
        }
        contributions.sort_by(|left, right| left.id().cmp(right.id()));
        if contributions.is_empty()
            || contributions
                .windows(2)
                .any(|pair| pair[0].id() == pair[1].id())
        {
            return Err(EvidenceError::new(
                "credit.invalid-contributions",
                "credit requires unique named measurement contributions",
            ));
        }
        provenance.sort();
        provenance.dedup();
        if provenance.is_empty() {
            return Err(EvidenceError::new(
                "credit.missing-provenance",
                "credit requires attributed provenance",
            ));
        }
        let mut assignment = Self {
            credit_id,
            policy,
            run_id,
            decision_id,
            observation_ids,
            contributions,
            recorded_at,
            provenance,
            content_digest: ContentDigest::sha256(&[]),
        };
        assignment.content_digest = assignment.expected_digest()?;
        Ok(assignment)
    }

    /// Returns the submitter-owned credit identity.
    #[must_use]
    pub fn credit_id(&self) -> &str {
        &self.credit_id
    }

    /// Returns the named credit policy.
    #[must_use]
    pub const fn policy(&self) -> &PolicyRef {
        &self.policy
    }

    /// Returns the attributed run.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the originating selection decision.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns the observations used by the policy.
    #[must_use]
    pub fn observation_ids(&self) -> &[OutcomeObservationId] {
        &self.observation_ids
    }

    /// Returns the named, unit-bearing contributions.
    #[must_use]
    pub fn contributions(&self) -> &[MeasurementEntry] {
        &self.contributions
    }

    /// Returns the store-local position supplied for the append.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns canonical attribution.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns the immutable content digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate(&self) -> Result<(), EvidenceError> {
        validate_credit_id(&self.credit_id)?;
        if self.observation_ids.is_empty()
            || self
                .observation_ids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(EvidenceError::new(
                "credit.invalid-observations",
                "credit observation identities must be non-empty, sorted, and unique",
            ));
        }
        if self.contributions.is_empty()
            || self
                .contributions
                .windows(2)
                .any(|pair| pair[0].id() >= pair[1].id())
            || self.provenance.is_empty()
            || self.provenance.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(EvidenceError::new(
                "credit.non-canonical",
                "credit contributions and provenance must be non-empty, sorted, and unique",
            ));
        }
        if self.content_digest != self.expected_digest()? {
            return Err(EvidenceError::new(
                "credit.digest-mismatch",
                "credit content does not match its immutable digest",
            ));
        }
        Ok(())
    }

    fn expected_digest(&self) -> Result<ContentDigest, EvidenceError> {
        #[derive(Serialize)]
        struct Content<'a> {
            credit_id: &'a str,
            policy: &'a PolicyRef,
            run_id: RunId,
            decision_id: SelectionDecisionId,
            observation_ids: &'a [OutcomeObservationId],
            contributions: &'a [MeasurementEntry],
            sequence: LogicalSequence,
            provenance: &'a [Provenance],
        }
        let content = Content {
            credit_id: &self.credit_id,
            policy: &self.policy,
            run_id: self.run_id,
            decision_id: self.decision_id,
            observation_ids: &self.observation_ids,
            contributions: &self.contributions,
            sequence: self.recorded_at.sequence(),
            provenance: &self.provenance,
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&content, &mut bytes).map_err(|error| {
            EvidenceError::new("credit.encode", format!("cannot encode credit: {error}"))
        })?;
        Ok(ContentDigest::sha256(&bytes))
    }
}

fn validate_credit_id(credit_id: &str) -> Result<(), EvidenceError> {
    let stable_char = |character: char| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '-' | '_' | '/' | ':')
    };
    let boundary_char =
        |character: char| character.is_ascii_lowercase() || character.is_ascii_digit();
    if credit_id.is_empty()
        || credit_id.len() > 128
        || !credit_id.chars().all(stable_char)
        || !credit_id.chars().next().is_some_and(boundary_char)
        || !credit_id.chars().next_back().is_some_and(boundary_char)
    {
        Err(EvidenceError::new(
            "credit.invalid-id",
            "credit identity must follow the stable lowercase identifier grammar",
        ))
    } else {
        Ok(())
    }
}

/// One typed append accepted by an evidence provider.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "typed append ownership avoids another heap allocation for ordinary run events"
)]
pub enum EvidenceEvent {
    /// Complete selection decision, profile, and objective.
    Selection(Box<SelectionRecord>),
    /// One live-admission result.
    Admission(AdmissionEvaluation),
    /// Terminal selected decision for which no graph started.
    SelectionNotStarted(SelectionNotStarted),
    /// Run opening recorded before graph execution.
    RunOpened(RunOpened),
    /// Normal mechanical run terminal.
    RunFinalized(RunFinalized),
    /// Explicit supervisor or recovery abandonment.
    RunAbandoned(RunAbandoned),
    /// Later outcome judgement.
    Observation(OutcomeObservation),
    /// Policy-attributed credit append.
    Credit(CreditAssignment),
}

impl EvidenceEvent {
    /// Returns the append's requested logical sequence.
    #[must_use]
    pub const fn sequence(&self) -> LogicalSequence {
        match self {
            Self::Selection(value) => value.recorded_at().sequence(),
            Self::Admission(value) => value.recorded_at().sequence(),
            Self::SelectionNotStarted(value) => value.recorded_at().sequence(),
            Self::RunOpened(value) => value.recorded_at().sequence(),
            Self::RunFinalized(value) => value.recorded_at().sequence(),
            Self::RunAbandoned(value) => value.recorded_at().sequence(),
            Self::Observation(value) => value.recorded_at().sequence(),
            Self::Credit(value) => value.recorded_at().sequence(),
        }
    }

    fn digest(&self) -> ContentDigest {
        match self {
            Self::Selection(value) => value.content_digest(),
            Self::Admission(value) => value.content_digest(),
            Self::SelectionNotStarted(value) => value.content_digest(),
            Self::RunOpened(value) => value.content_digest(),
            Self::RunFinalized(value) => value.content_digest(),
            Self::RunAbandoned(value) => value.content_digest(),
            Self::Observation(value) => value.content_digest(),
            Self::Credit(value) => value.content_digest(),
        }
    }

    fn validate(&self) -> Result<(), EvidenceError> {
        let result = match self {
            Self::Selection(value) => value.validate(),
            Self::Admission(value) => value.validate(),
            Self::SelectionNotStarted(value) => value.validate(),
            Self::RunOpened(value) => value.validate(),
            Self::RunFinalized(value) => value.validate(),
            Self::RunAbandoned(value) => value.validate(),
            Self::Observation(value) => value.validate(),
            Self::Credit(value) => return value.validate(),
        };
        result.map_err(|error| EvidenceError::new("record.invalid", error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum EventKey {
    Selection(SelectionDecisionId),
    Admission(AdmissionAttemptId),
    SelectionNotStarted(SelectionDecisionId),
    RunOpened(RunId),
    RunFinalized(RunId),
    RunAbandoned(RunId),
    Observation(OutcomeObservationId),
    Credit(String),
}

impl EvidenceEvent {
    fn key(&self) -> EventKey {
        match self {
            Self::Selection(value) => EventKey::Selection(value.decision().decision_id()),
            Self::Admission(value) => EventKey::Admission(value.attempt_id()),
            Self::SelectionNotStarted(value) => EventKey::SelectionNotStarted(value.decision_id()),
            Self::RunOpened(value) => EventKey::RunOpened(value.run_id()),
            Self::RunFinalized(value) => EventKey::RunFinalized(value.run_id()),
            Self::RunAbandoned(value) => EventKey::RunAbandoned(value.run_id()),
            Self::Observation(value) => EventKey::Observation(value.observation_id()),
            Self::Credit(value) => EventKey::Credit(value.credit_id().to_owned()),
        }
    }
}

/// Immutable view of one run and all later evidence retained with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunView {
    /// Opening append.
    pub opened: RunOpened,
    /// Explicit terminal append, absent while the run remains open.
    pub terminal: Option<RunTerminal>,
    /// Append-only observations in store order.
    pub observations: Vec<OutcomeObservation>,
    /// Append-only credit assignments in store order.
    pub credits: Vec<CreditAssignment>,
}

/// Immutable view of one complete selection episode prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionEpisodeView {
    /// Selection record that begins the episode.
    pub selection: SelectionRecord,
    /// Live admission attempts in fallback order.
    pub admissions: Vec<AdmissionEvaluation>,
    /// Explicit no-start terminal, when every fallback failed.
    pub not_started: Option<SelectionNotStarted>,
    /// Started runs; v1 normally contains zero or one.
    pub runs: Vec<RunView>,
}

impl SelectionEpisodeView {
    /// Returns whether retention may evict this whole episode.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        if matches!(
            self.selection.decision().outcome(),
            SelectionOutcome::NoEligibleCandidate { .. }
        ) || self.not_started.is_some()
        {
            return true;
        }
        !self.runs.is_empty() && self.runs.iter().all(|run| run.terminal.is_some())
    }
}

/// Visible bounded-store capacity and current usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityReport {
    /// Configured maximum episodes.
    pub max_episodes: usize,
    /// Configured maximum runs.
    pub max_runs: usize,
    /// Configured maximum observations.
    pub max_observations_total: usize,
    /// Configured maximum observations for one run.
    pub max_observations_per_run: usize,
    /// Configured maximum explicit evidence references on one append.
    pub max_evidence_refs_per_record: usize,
    /// Configured maximum open runs.
    pub max_open_runs: usize,
}

/// Evidence-provider health and retained-window report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreStatus {
    /// Greatest sequence accepted by this provider.
    pub observed_through: LogicalSequence,
    /// Earliest retained append sequence.
    pub earliest_retained_sequence: LogicalSequence,
    /// Retained selection episodes.
    pub retained_episodes: usize,
    /// Retained started runs.
    pub retained_runs: usize,
    /// Retained outcome observations.
    pub retained_observations: usize,
    /// Whole episodes evicted since provider creation.
    pub evicted_episodes_total: u64,
    /// Runs removed by whole-episode eviction.
    pub evicted_runs_total: u64,
    /// Failed appends observed by the provider.
    pub dropped_append_total: u64,
    /// Configured capacity witness.
    pub capacity: CapacityReport,
}

/// Replaceable provider contract used by recorders, selectors, and readers.
pub trait TopologyEvidenceProvider: Send + Sync {
    /// Appends one typed event under provider ordering and retention rules.
    fn append(&self, event: EvidenceEvent) -> Result<AppendReceipt, EvidenceError>;

    /// Reads one retained selection episode at a consistent provider prefix.
    fn read_episode(
        &self,
        decision_id: SelectionDecisionId,
    ) -> Result<Option<SelectionEpisodeView>, EvidenceError>;

    /// Reads one retained run at a consistent provider prefix.
    fn read_run(&self, run_id: RunId) -> Result<Option<RunView>, EvidenceError>;

    /// Returns visible retained-window and loss metadata.
    fn store_status(&self) -> Result<StoreStatus, EvidenceError>;

    /// Captures contextual immutable statistics at the current watermark.
    fn capture_statistics(
        &self,
        request: &StatisticsRequest,
    ) -> Result<TopologyStatsSnapshot, EvidenceError>;
}

/// Use `Res<TopologyEvidence>` when a system needs to append topology-runtime
/// evidence, read a retained episode, or capture a frozen statistics snapshot.
///
/// The facade keeps recorders and selectors independent of provider storage.
/// Explicit append failures are returned to callers; the best-effort
/// [`record`](Self::record) path never changes graph behavior and exposes loss
/// through [`StoreStatus`].
///
/// # Scope
///
/// **Global.** One provider orders selection, admission, run, observation, and
/// credit events across sessions in the current process.
///
/// # Provided by
///
/// [`TopologyEvidencePlugin`](super::TopologyEvidencePlugin) supplies the
/// bounded in-memory default. A host may instead register a facade over any
/// [`TopologyEvidenceProvider`].
///
/// # Access
///
/// Systems use `Res<TopologyEvidence>`. The facade is cloneable and delegates
/// interior mutability and synchronization to its `Arc`-shared provider;
/// callers never receive `ResMut<TopologyEvidence>` merely to append an event.
///
/// # Alternatives
///
/// Replace [`BoundedInMemoryEvidence`] with a conforming durable provider when
/// evidence must survive process loss. Provider replacement does not change
/// the facade, selector, recorder, or event contracts.
///
/// # Axis
///
/// The facade is the stable capability boundary on the topology-evidence
/// storage axis. The bounded in-memory provider is its trivial non-durable
/// point; durable provider points remain open.
///
/// # Example system
///
/// ```ignore
/// use polaris::system::param::Res;
/// use polaris::system::system;
/// use polaris_cosmos::evidence::{EvidenceEvent, TopologyEvidence};
///
/// #[system]
/// async fn record_selection(evidence: Res<TopologyEvidence>, event: Res<SelectionEvent>) {
///     let _receipt = evidence.append(EvidenceEvent::Selection(event.record.clone()))?;
///     Ok::<_, EvidenceError>(())
/// }
/// ```
#[derive(Clone)]
pub struct TopologyEvidence {
    provider: Arc<dyn TopologyEvidenceProvider>,
}

impl fmt::Debug for TopologyEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TopologyEvidence")
            .field("version", &TOPOLOGY_EVIDENCE_VERSION)
            .finish_non_exhaustive()
    }
}

impl TopologyEvidence {
    /// Creates a facade around any conforming provider.
    #[must_use]
    pub fn new(provider: Arc<dyn TopologyEvidenceProvider>) -> Self {
        Self { provider }
    }

    /// Appends one event and surfaces any evidence failure to explicit callers.
    pub fn append(&self, event: EvidenceEvent) -> Result<AppendReceipt, EvidenceError> {
        self.provider.append(event)
    }

    /// Best-effort recorder path whose failure cannot alter agent behaviour.
    ///
    /// Loss remains visible through [`StoreStatus::dropped_append_total`].
    pub fn record(&self, event: EvidenceEvent) -> Option<AppendReceipt> {
        self.provider.append(event).ok()
    }

    /// Reads one retained selection episode.
    pub fn read_episode(
        &self,
        decision_id: SelectionDecisionId,
    ) -> Result<Option<SelectionEpisodeView>, EvidenceError> {
        self.provider.read_episode(decision_id)
    }

    /// Reads one retained run.
    pub fn read_run(&self, run_id: RunId) -> Result<Option<RunView>, EvidenceError> {
        self.provider.read_run(run_id)
    }

    /// Returns provider status and visible loss metadata.
    pub fn store_status(&self) -> Result<StoreStatus, EvidenceError> {
        self.provider.store_status()
    }

    /// Captures one immutable statistics input for a selector.
    pub fn capture_statistics(
        &self,
        request: &StatisticsRequest,
    ) -> Result<TopologyStatsSnapshot, EvidenceError> {
        self.provider.capture_statistics(request)
    }
}

/// Statistics policies and sufficiency threshold for one capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatisticsRequest {
    /// Named aggregation policy.
    pub statistics_policy: PolicyRef,
    /// Named conflict/supersession policy.
    pub observation_selection_policy: PolicyRef,
    /// Named uncertainty-interval policy.
    pub interval_policy: PolicyRef,
    /// Minimum observations for a sufficient cohort.
    pub minimum_sample_count: NonZeroUsize,
}

/// Low-cardinality task-profile dimensions retained in each cohort key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProfileBucket {
    /// Known decomposition shape, or `None` for a distinct unknown bucket.
    pub decomposability: Option<DecompositionShape>,
    /// Known verifier availability, or `None` for unknown.
    pub verifier_availability: Option<VerifierAvailability>,
    /// Known task horizon, or `None` for unknown.
    pub horizon: Option<TaskHorizon>,
    /// Known consequence band, or `None` for unknown.
    pub consequence: Option<ConsequenceLevel>,
}

/// Provider and model-family identity retained without a provider-local model version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProviderModelBucket {
    /// Versioned provider implementation used by the model.
    pub provider: ProviderKey,
    /// Stable model-family identity; provider-local revisions remain run evidence.
    pub model: ModelId,
}

/// Execution dimensions known only after the selected registration starts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExecutionBucket {
    /// Versioned embodiment that supplied the domain ports.
    pub embodiment: EmbodimentKey,
    /// Providers used by the run in canonical order.
    pub providers: Vec<ProviderKey>,
    /// Provider/model-family pairs in canonical order.
    pub models: Vec<ProviderModelBucket>,
}

/// Stable contextual cohort identity independent of process-local graph labels.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CohortKey {
    /// Bucketed task context.
    pub profile: ProfileBucket,
    /// Semantic topology identity.
    pub topology: TopologyKey,
    /// Concrete catalog registration.
    pub registration: RegistrationId,
    /// Resolved secret-free configuration content identity.
    pub configuration: ConfigurationDigest,
    /// Contract requested by the selection decision.
    pub contract_key: TopologyContractKey,
    /// Execution provenance for a started selected candidate; `None` for zero-run cohorts.
    pub execution: Option<ExecutionBucket>,
}

/// Comparable measurement unit/calibration witness.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum UnitWitness {
    /// Proportion, optionally partitioned by calibration policy.
    Proportion(Option<PolicyRef>),
    /// Unsigned count.
    Count,
    /// Milliseconds.
    DurationMilliseconds,
    /// Stable token-accounting identity.
    Tokens(crate::contracts::TokenAccountingId),
    /// Currency identity.
    Money(crate::contracts::CurrencyCode),
    /// Named ordinal scale and its number of positions.
    Ordinal(PolicyRef, u64),
}

/// Integer-only deterministic summary of comparable measurements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementAggregate {
    /// Measurement registry identity.
    pub measurement_id: MeasurementId,
    /// Unit and calibration partition.
    pub unit_witness: UnitWitness,
    /// Number of selected observations.
    pub sample_count: u64,
    /// Minimum observed integer value in the witnessed unit.
    pub minimum: u64,
    /// Nearest-rank median.
    pub p50: u64,
    /// Nearest-rank 90th percentile.
    pub p90: u64,
    /// Nearest-rank 95th percentile.
    pub p95: u64,
    /// Maximum observed integer value.
    pub maximum: u64,
    /// Checked sum of values.
    pub total: u64,
    /// Integer arithmetic mean.
    pub mean: u64,
    /// Named-policy lower uncertainty bound for proportions.
    pub interval_low_ppm: Option<u32>,
    /// Named-policy upper uncertainty bound for proportions.
    pub interval_high_ppm: Option<u32>,
}

/// Contextual statistics for one candidate cohort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CohortStatistics {
    /// Canonical cohort key.
    pub key: CohortKey,
    /// Times the registration appeared in a complete candidate set.
    pub times_considered: u64,
    /// Times it passed mechanical and policy eligibility.
    pub times_eligible: u64,
    /// Times selected, including a later no-start.
    pub times_selected: u64,
    /// Sum of recorded selection propensities in integer ppm.
    pub propensity_sum_ppm: u64,
    /// Exclusion counts by stable code.
    pub excluded_by_code: BTreeMap<crate::contracts::ExclusionCode, u64>,
    /// Started runs.
    pub runs_opened: u64,
    /// Normally finalized runs.
    pub runs_finalized: u64,
    /// Explicitly abandoned runs.
    pub runs_abandoned: u64,
    /// Mechanical terminal dispositions, never task success.
    pub execution_dispositions: BTreeMap<ExecutionDisposition, u64>,
    /// Runs with at least one outcome judgement.
    pub observed_runs: u64,
    /// Terminal runs without an outcome judgement.
    pub unobserved_runs: u64,
    /// Runs retaining disagreeing statuses.
    pub conflicting_runs: u64,
    /// Outcome statuses, kept separate from mechanical completion.
    pub observation_statuses: BTreeMap<Status, u64>,
    /// Unit-partitioned outcome measurements.
    pub measurements: Vec<MeasurementAggregate>,
    /// Number of outcome observations in this cohort.
    pub sample_count: u64,
    /// Whether `sample_count` is below the requested threshold.
    pub below_minimum: bool,
}

/// Immutable deterministic statistics capture used as a selection input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyStatsSnapshot {
    /// Aggregation policy identity.
    pub statistics_policy: PolicyRef,
    /// Observation conflict/supersession policy identity.
    pub observation_selection_policy: PolicyRef,
    /// Uncertainty interval policy identity.
    pub interval_policy: PolicyRef,
    /// Capture position; wall time is deliberately absent.
    pub captured_at: RecordedAt,
    /// Retained evidence window and visible loss.
    pub window: StatisticsWindow,
    /// Sufficiency threshold.
    pub minimum_sample_count: u64,
    /// Canonically ordered contextual cohorts.
    pub cohorts: Vec<CohortStatistics>,
    /// Content identity of the complete derived snapshot.
    pub content_digest: ContentDigest,
}

impl TopologyStatsSnapshot {
    /// Returns the immutable selection-input identity.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }
}

/// Default bounded, non-durable evidence provider.
#[derive(Debug)]
pub struct BoundedInMemoryEvidence {
    state: Mutex<State>,
}

impl BoundedInMemoryEvidence {
    /// Creates an empty provider with explicit entry capacities.
    #[must_use]
    pub fn new(config: BoundedEvidenceConfig) -> Self {
        Self {
            state: Mutex::new(State::new(config)),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, EvidenceError> {
        self.state.lock().map_err(|_| {
            EvidenceError::new("store.poisoned", "evidence provider lock was poisoned")
        })
    }
}

impl TopologyEvidenceProvider for BoundedInMemoryEvidence {
    fn append(&self, event: EvidenceEvent) -> Result<AppendReceipt, EvidenceError> {
        let mut state = self.lock()?;
        match state.append(event) {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                state.dropped_append_total = state.dropped_append_total.saturating_add(1);
                Err(error)
            }
        }
    }

    fn read_episode(
        &self,
        decision_id: SelectionDecisionId,
    ) -> Result<Option<SelectionEpisodeView>, EvidenceError> {
        Ok(self.lock()?.episodes.get(&decision_id).cloned())
    }

    fn read_run(&self, run_id: RunId) -> Result<Option<RunView>, EvidenceError> {
        let state = self.lock()?;
        let Some(decision_id) = state.run_index.get(&run_id) else {
            return Ok(None);
        };
        Ok(state
            .episodes
            .get(decision_id)
            .and_then(|episode| {
                episode
                    .runs
                    .iter()
                    .find(|run| run.opened.run_id() == run_id)
            })
            .cloned())
    }

    fn store_status(&self) -> Result<StoreStatus, EvidenceError> {
        Ok(self.lock()?.status())
    }

    fn capture_statistics(
        &self,
        request: &StatisticsRequest,
    ) -> Result<TopologyStatsSnapshot, EvidenceError> {
        let state = self.lock()?;
        aggregate_snapshot(state.episodes.values().cloned(), state.status(), request)
    }
}

#[derive(Debug)]
struct State {
    config: BoundedEvidenceConfig,
    next_sequence: u64,
    episodes: BTreeMap<SelectionDecisionId, SelectionEpisodeView>,
    episode_order: VecDeque<SelectionDecisionId>,
    run_index: BTreeMap<RunId, SelectionDecisionId>,
    seen: BTreeMap<EventKey, (ContentDigest, LogicalSequence)>,
    evicted_episodes_total: u64,
    evicted_runs_total: u64,
    dropped_append_total: u64,
}

impl State {
    fn new(config: BoundedEvidenceConfig) -> Self {
        Self {
            config,
            next_sequence: 1,
            episodes: BTreeMap::new(),
            episode_order: VecDeque::new(),
            run_index: BTreeMap::new(),
            seen: BTreeMap::new(),
            evicted_episodes_total: 0,
            evicted_runs_total: 0,
            dropped_append_total: 0,
        }
    }

    fn append(&mut self, event: EvidenceEvent) -> Result<AppendReceipt, EvidenceError> {
        event.validate()?;
        let key = event.key();
        let digest = event.digest();
        if let Some((retained_digest, sequence)) = self.seen.get(&key) {
            if retained_digest == &digest {
                return Ok(AppendReceipt {
                    sequence: *sequence,
                    inserted: false,
                });
            }
            return Err(EvidenceError::new(
                "conflict.digest-mismatch",
                "an append identity already exists with different content",
            ));
        }
        let expected = LogicalSequence::new(self.next_sequence);
        if event.sequence() != expected {
            return Err(EvidenceError::new(
                "order.sequence",
                format!(
                    "provider expected sequence {}, found {}",
                    expected.get(),
                    event.sequence().get()
                ),
            ));
        }
        self.check_reference_limit(&event)?;
        self.apply(event)?;
        self.seen.insert(key, (digest, expected));
        self.next_sequence = self.next_sequence.checked_add(1).ok_or_else(|| {
            EvidenceError::new("order.exhausted", "logical sequence space was exhausted")
        })?;
        Ok(AppendReceipt {
            sequence: expected,
            inserted: true,
        })
    }

    fn check_reference_limit(&self, event: &EvidenceEvent) -> Result<(), EvidenceError> {
        let count = match event {
            EvidenceEvent::RunFinalized(value) => value.evidence_refs().len(),
            EvidenceEvent::RunAbandoned(value) => value.evidence_refs().len(),
            EvidenceEvent::Observation(value) => value.evidence_refs().len(),
            _ => 0,
        };
        if count > self.config.max_evidence_refs_per_record.get() {
            Err(EvidenceError::new(
                "capacity.evidence-refs-exhausted",
                "append exceeds the configured evidence-reference limit",
            ))
        } else {
            Ok(())
        }
    }

    fn apply(&mut self, event: EvidenceEvent) -> Result<(), EvidenceError> {
        match event {
            EvidenceEvent::Selection(selection) => self.append_selection(*selection),
            EvidenceEvent::Admission(admission) => self.append_admission(admission),
            EvidenceEvent::SelectionNotStarted(value) => self.append_not_started(value),
            EvidenceEvent::RunOpened(opened) => self.append_opened(opened),
            EvidenceEvent::RunFinalized(finalized) => self.append_finalized(finalized),
            EvidenceEvent::RunAbandoned(abandoned) => self.append_abandoned(abandoned),
            EvidenceEvent::Observation(observation) => self.append_observation(observation),
            EvidenceEvent::Credit(credit) => self.append_credit(credit),
        }
    }

    fn append_selection(&mut self, selection: SelectionRecord) -> Result<(), EvidenceError> {
        self.ensure_episode_capacity(None)?;
        let decision_id = selection.decision().decision_id();
        self.episodes.insert(
            decision_id,
            SelectionEpisodeView {
                selection,
                admissions: Vec::new(),
                not_started: None,
                runs: Vec::new(),
            },
        );
        self.episode_order.push_back(decision_id);
        Ok(())
    }

    fn append_admission(&mut self, admission: AdmissionEvaluation) -> Result<(), EvidenceError> {
        let episode = self
            .episodes
            .get_mut(&admission.decision_id())
            .ok_or_else(|| {
                EvidenceError::new(
                    "conflict.unknown-decision",
                    "admission requires a retained selection",
                )
            })?;
        if episode.is_terminal() {
            return Err(EvidenceError::new(
                "conflict.terminal-episode",
                "admission cannot follow a terminal selection episode",
            ));
        }
        if !episode
            .selection
            .decision()
            .candidates()
            .iter()
            .any(|candidate| candidate.candidate() == admission.candidate())
        {
            return Err(EvidenceError::new(
                "conflict.unknown-candidate",
                "admission candidate was not in the recorded candidate set",
            ));
        }
        episode.admissions.push(admission);
        Ok(())
    }

    fn append_not_started(&mut self, value: SelectionNotStarted) -> Result<(), EvidenceError> {
        let episode = self.episodes.get_mut(&value.decision_id()).ok_or_else(|| {
            EvidenceError::new(
                "conflict.unknown-decision",
                "no-start requires a retained selection",
            )
        })?;
        if episode.not_started.is_some() || !episode.runs.is_empty() {
            return Err(EvidenceError::new(
                "conflict.already-started-or-terminal",
                "no-start conflicts with an existing terminal or started run",
            ));
        }
        let retained_attempts = episode
            .admissions
            .iter()
            .filter(|admission| !admission.is_admitted())
            .map(AdmissionEvaluation::attempt_id)
            .collect::<Vec<_>>();
        if retained_attempts != value.admission_attempts() {
            return Err(EvidenceError::new(
                "conflict.admission-history",
                "no-start must name every failed admission attempt in retained order",
            ));
        }
        episode.not_started = Some(value);
        Ok(())
    }

    fn append_opened(&mut self, opened: RunOpened) -> Result<(), EvidenceError> {
        let episode = self.episodes.get(&opened.decision_id()).ok_or_else(|| {
            EvidenceError::new(
                "conflict.unknown-decision",
                "run requires a retained selection",
            )
        })?;
        if episode.not_started.is_some() || !episode.runs.is_empty() {
            return Err(EvidenceError::new(
                "conflict.already-started-or-terminal",
                "one selection episode may start at most one run",
            ));
        }
        let admitted = episode.admissions.iter().any(|admission| {
            admission.attempt_id() == opened.admission_attempt_id()
                && admission.is_admitted()
                && admission.candidate() == opened.selected()
        });
        if !admitted {
            return Err(EvidenceError::new(
                "conflict.missing-admission",
                "run opening requires its retained successful admission",
            ));
        }
        self.ensure_run_capacity(opened.decision_id())?;
        let episode = self
            .episodes
            .get_mut(&opened.decision_id())
            .expect("validated protected episode cannot be evicted");
        let run_id = opened.run_id();
        episode.runs.push(RunView {
            opened,
            terminal: None,
            observations: Vec::new(),
            credits: Vec::new(),
        });
        self.run_index
            .insert(run_id, episode.selection.decision().decision_id());
        Ok(())
    }

    fn append_finalized(&mut self, finalized: RunFinalized) -> Result<(), EvidenceError> {
        let run = self.run_mut(finalized.run_id())?;
        if run.terminal.is_some() {
            return Err(EvidenceError::new(
                "conflict.already-finalized",
                "run already has an explicit terminal append",
            ));
        }
        run.terminal = Some(RunTerminal::Finalized(finalized));
        Ok(())
    }

    fn append_abandoned(&mut self, abandoned: RunAbandoned) -> Result<(), EvidenceError> {
        let run = self.run_mut(abandoned.run_id())?;
        if run.terminal.is_some() {
            return Err(EvidenceError::new(
                "conflict.already-finalized",
                "run already has an explicit terminal append",
            ));
        }
        run.terminal = Some(RunTerminal::Abandoned(abandoned));
        Ok(())
    }

    fn append_observation(&mut self, observation: OutcomeObservation) -> Result<(), EvidenceError> {
        let decision_id = *self.run_index.get(&observation.run_id()).ok_or_else(|| {
            EvidenceError::new(
                "conflict.unknown-run",
                "observation requires a retained run",
            )
        })?;
        let run = self
            .episodes
            .get(&decision_id)
            .and_then(|episode| {
                episode
                    .runs
                    .iter()
                    .find(|run| run.opened.run_id() == observation.run_id())
            })
            .expect("run index and retained episodes stay consistent");
        if run.terminal.is_none() {
            return Err(EvidenceError::new(
                "conflict.open-run",
                "outcome observations require an explicit run terminal",
            ));
        }
        if observation.task_id() != run.opened.task_id() {
            return Err(EvidenceError::new(
                "conflict.task-mismatch",
                "observation task does not match the run opening",
            ));
        }
        self.ensure_observation_capacity(decision_id, observation.run_id())?;
        let run = self.run_mut(observation.run_id())?;
        run.observations.push(observation);
        Ok(())
    }

    fn append_credit(&mut self, credit: CreditAssignment) -> Result<(), EvidenceError> {
        let run = self.run_mut(credit.run_id())?;
        if credit.decision_id() != run.opened.decision_id() {
            return Err(EvidenceError::new(
                "conflict.decision-mismatch",
                "credit decision does not match the run opening",
            ));
        }
        let available = run
            .observations
            .iter()
            .map(OutcomeObservation::observation_id)
            .collect::<BTreeSet<_>>();
        if !credit
            .observation_ids()
            .iter()
            .all(|observation_id| available.contains(observation_id))
        {
            return Err(EvidenceError::new(
                "conflict.unknown-observation",
                "credit requires every referenced observation to be retained on the run",
            ));
        }
        run.credits.push(credit);
        Ok(())
    }

    fn run_mut(&mut self, run_id: RunId) -> Result<&mut RunView, EvidenceError> {
        let decision_id = *self.run_index.get(&run_id).ok_or_else(|| {
            EvidenceError::new("conflict.unknown-run", "append requires a retained run")
        })?;
        self.episodes
            .get_mut(&decision_id)
            .and_then(|episode| {
                episode
                    .runs
                    .iter_mut()
                    .find(|run| run.opened.run_id() == run_id)
            })
            .ok_or_else(|| {
                EvidenceError::new("conflict.unknown-run", "append requires a retained run")
            })
    }

    fn ensure_episode_capacity(
        &mut self,
        protected: Option<SelectionDecisionId>,
    ) -> Result<(), EvidenceError> {
        while self.episodes.len() >= self.config.max_episodes.get() {
            self.evict_oldest_terminal(protected)?;
        }
        Ok(())
    }

    fn ensure_run_capacity(&mut self, protected: SelectionDecisionId) -> Result<(), EvidenceError> {
        while self.run_count() >= self.config.max_runs.get() {
            self.evict_oldest_terminal(Some(protected))?;
        }
        if self.open_run_count() >= self.config.max_open_runs.get() {
            return Err(EvidenceError::new(
                "capacity.open-runs-exhausted",
                "no additional open run can be retained",
            ));
        }
        Ok(())
    }

    fn ensure_observation_capacity(
        &mut self,
        protected: SelectionDecisionId,
        run_id: RunId,
    ) -> Result<(), EvidenceError> {
        let per_run = self
            .episodes
            .get(&protected)
            .and_then(|episode| {
                episode
                    .runs
                    .iter()
                    .find(|run| run.opened.run_id() == run_id)
            })
            .map_or(0, |run| run.observations.len());
        if per_run >= self.config.max_observations_per_run.get() {
            return Err(EvidenceError::new(
                "capacity.run-observations-exhausted",
                "run reached its observation limit; earlier judgements remain intact",
            ));
        }
        while self.observation_count() >= self.config.max_observations_total.get() {
            self.evict_oldest_terminal(Some(protected))?;
        }
        Ok(())
    }

    fn evict_oldest_terminal(
        &mut self,
        protected: Option<SelectionDecisionId>,
    ) -> Result<(), EvidenceError> {
        let Some(index) = self.episode_order.iter().position(|decision_id| {
            Some(*decision_id) != protected
                && self
                    .episodes
                    .get(decision_id)
                    .is_some_and(SelectionEpisodeView::is_terminal)
        }) else {
            return Err(EvidenceError::new(
                "capacity.exhausted",
                "capacity is occupied by open or protected selection episodes",
            ));
        };
        let decision_id = self
            .episode_order
            .remove(index)
            .expect("position came from the same deque");
        let episode = self
            .episodes
            .remove(&decision_id)
            .expect("episode order and map stay consistent");
        let evicted_runs = u64::try_from(episode.runs.len()).unwrap_or(u64::MAX);
        for run in episode.runs {
            self.run_index.remove(&run.opened.run_id());
        }
        self.evicted_episodes_total = self.evicted_episodes_total.saturating_add(1);
        self.evicted_runs_total = self.evicted_runs_total.saturating_add(evicted_runs);
        Ok(())
    }

    fn run_count(&self) -> usize {
        self.episodes
            .values()
            .map(|episode| episode.runs.len())
            .sum()
    }

    fn open_run_count(&self) -> usize {
        self.episodes
            .values()
            .flat_map(|episode| &episode.runs)
            .filter(|run| run.terminal.is_none())
            .count()
    }

    fn observation_count(&self) -> usize {
        self.episodes
            .values()
            .flat_map(|episode| &episode.runs)
            .map(|run| run.observations.len())
            .sum()
    }

    fn status(&self) -> StoreStatus {
        let observed_through = LogicalSequence::new(self.next_sequence.saturating_sub(1));
        let earliest = self
            .episodes
            .values()
            .map(|episode| episode.selection.recorded_at().sequence())
            .min()
            .unwrap_or(observed_through);
        StoreStatus {
            observed_through,
            earliest_retained_sequence: earliest,
            retained_episodes: self.episodes.len(),
            retained_runs: self.run_count(),
            retained_observations: self.observation_count(),
            evicted_episodes_total: self.evicted_episodes_total,
            evicted_runs_total: self.evicted_runs_total,
            dropped_append_total: self.dropped_append_total,
            capacity: CapacityReport {
                max_episodes: self.config.max_episodes.get(),
                max_runs: self.config.max_runs.get(),
                max_observations_total: self.config.max_observations_total.get(),
                max_observations_per_run: self.config.max_observations_per_run.get(),
                max_evidence_refs_per_record: self.config.max_evidence_refs_per_record.get(),
                max_open_runs: self.config.max_open_runs.get(),
            },
        }
    }
}

#[derive(Default)]
struct CohortBuilder {
    times_considered: u64,
    times_eligible: u64,
    times_selected: u64,
    propensity_sum_ppm: u64,
    excluded_by_code: BTreeMap<crate::contracts::ExclusionCode, u64>,
    runs_opened: u64,
    runs_finalized: u64,
    runs_abandoned: u64,
    execution_dispositions: BTreeMap<ExecutionDisposition, u64>,
    observed_runs: u64,
    unobserved_runs: u64,
    conflicting_runs: u64,
    observation_statuses: BTreeMap<Status, u64>,
    measurements: BTreeMap<(MeasurementId, UnitWitness), Vec<u64>>,
    sample_count: u64,
}

/// Derives a deterministic snapshot from any provider's consistent episode prefix.
pub fn aggregate_snapshot(
    episodes: impl IntoIterator<Item = SelectionEpisodeView>,
    status: StoreStatus,
    request: &StatisticsRequest,
) -> Result<TopologyStatsSnapshot, EvidenceError> {
    let mut cohorts = BTreeMap::<CohortKey, CohortBuilder>::new();
    for episode in episodes {
        let decision = episode.selection.decision();
        let features = episode.selection.task_profile().features();
        let profile = ProfileBucket {
            decomposability: features.decomposability.known_value().copied(),
            verifier_availability: features
                .verifier
                .known_value()
                .map(|verifier| verifier.availability()),
            horizon: features.horizon.known_value().copied(),
            consequence: features.consequence.known_value().copied(),
        };
        let selected = match decision.outcome() {
            SelectionOutcome::Selected { selected, .. } => Some(selected.registration_id()),
            SelectionOutcome::NoEligibleCandidate { .. } => None,
        };
        let started = episode.runs.first().map(|run| {
            let provenance = run.opened.provenance();
            (
                run.opened.selected().registration_id().clone(),
                ExecutionBucket {
                    embodiment: provenance.embodiment().clone(),
                    providers: provenance.providers().to_vec(),
                    models: provenance
                        .models()
                        .iter()
                        .map(|model| ProviderModelBucket {
                            provider: model.provider().clone(),
                            model: model.model().clone(),
                        })
                        .collect(),
                },
            )
        });
        let mut keys = BTreeMap::<RegistrationId, CohortKey>::new();
        for candidate in decision.candidates() {
            let reference = candidate.candidate();
            let key = CohortKey {
                profile: profile.clone(),
                topology: reference.topology_key().clone(),
                registration: reference.registration_id().clone(),
                configuration: reference.configuration().digest().clone(),
                contract_key: decision.requested_contract().clone(),
                execution: started.as_ref().and_then(|(registration, bucket)| {
                    (registration == reference.registration_id()).then(|| bucket.clone())
                }),
            };
            keys.insert(reference.registration_id().clone(), key.clone());
            let cohort = cohorts.entry(key).or_default();
            cohort.times_considered = cohort.times_considered.saturating_add(1);
            cohort.propensity_sum_ppm = cohort
                .propensity_sum_ppm
                .saturating_add(u64::from(candidate.selection_propensity().get()));
            if candidate.is_eligible() {
                cohort.times_eligible = cohort.times_eligible.saturating_add(1);
            }
            if selected == Some(reference.registration_id()) {
                cohort.times_selected = cohort.times_selected.saturating_add(1);
            }
            for reason in candidate
                .mechanical_admission()
                .reasons()
                .iter()
                .chain(candidate.policy_eligibility().reasons())
            {
                increment(&mut cohort.excluded_by_code, reason.code().clone());
            }
        }
        for run in episode.runs {
            let Some(key) = keys.get(run.opened.selected().registration_id()).cloned() else {
                continue;
            };
            let cohort = cohorts.entry(key).or_default();
            cohort.runs_opened = cohort.runs_opened.saturating_add(1);
            match &run.terminal {
                Some(RunTerminal::Finalized(finalized)) => {
                    cohort.runs_finalized = cohort.runs_finalized.saturating_add(1);
                    increment(
                        &mut cohort.execution_dispositions,
                        finalized.completion().disposition(),
                    );
                }
                Some(RunTerminal::Abandoned(_)) => {
                    cohort.runs_abandoned = cohort.runs_abandoned.saturating_add(1);
                    increment(
                        &mut cohort.execution_dispositions,
                        ExecutionDisposition::Abandoned,
                    );
                }
                None => {}
            }
            if run.terminal.is_some() && run.observations.is_empty() {
                cohort.unobserved_runs = cohort.unobserved_runs.saturating_add(1);
            }
            if !run.observations.is_empty() {
                cohort.observed_runs = cohort.observed_runs.saturating_add(1);
            }
            let statuses = run
                .observations
                .iter()
                .map(OutcomeObservation::status)
                .collect::<BTreeSet<_>>();
            if statuses.len() > 1 {
                cohort.conflicting_runs = cohort.conflicting_runs.saturating_add(1);
            }
            for observation in run.observations {
                cohort.sample_count = cohort.sample_count.saturating_add(1);
                increment(&mut cohort.observation_statuses, observation.status());
                for entry in observation.measurements() {
                    if is_v1_aggregate_measurement(entry.id())
                        && let Some((witness, value)) = measurement_value(entry)
                    {
                        cohort
                            .measurements
                            .entry((entry.id().clone(), witness))
                            .or_default()
                            .push(value);
                    }
                }
            }
        }
    }

    let minimum = u64::try_from(request.minimum_sample_count.get()).unwrap_or(u64::MAX);
    let cohorts = cohorts
        .into_iter()
        .map(|(key, builder)| build_cohort(key, builder, minimum))
        .collect::<Result<Vec<_>, _>>()?;
    let window = StatisticsWindow::new(
        status.observed_through,
        status.earliest_retained_sequence,
        status.evicted_episodes_total,
        status.dropped_append_total,
    )
    .map_err(|error| EvidenceError::new("statistics.window", error.to_string()))?;
    let mut snapshot = TopologyStatsSnapshot {
        statistics_policy: request.statistics_policy.clone(),
        observation_selection_policy: request.observation_selection_policy.clone(),
        interval_policy: request.interval_policy.clone(),
        captured_at: RecordedAt::new(status.observed_through, None),
        window,
        minimum_sample_count: minimum,
        cohorts,
        content_digest: ContentDigest::sha256(&[]),
    };
    snapshot.content_digest = snapshot_digest(&snapshot)?;
    Ok(snapshot)
}

fn is_v1_aggregate_measurement(id: &MeasurementId) -> bool {
    matches!(
        id.as_str(),
        "verified-success"
            | "objective-coverage"
            | "residual-count"
            | "severity-fold"
            | "quality"
            | "human-intervention-count"
            | "end-to-end-latency"
            | "token-cost"
            | "monetary-cost"
            | "rework-required"
    )
}

fn build_cohort(
    key: CohortKey,
    builder: CohortBuilder,
    minimum: u64,
) -> Result<CohortStatistics, EvidenceError> {
    let measurements = builder
        .measurements
        .into_iter()
        .map(|((measurement_id, witness), values)| {
            aggregate_measurement(measurement_id, witness, values)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CohortStatistics {
        key,
        times_considered: builder.times_considered,
        times_eligible: builder.times_eligible,
        times_selected: builder.times_selected,
        propensity_sum_ppm: builder.propensity_sum_ppm,
        excluded_by_code: builder.excluded_by_code,
        runs_opened: builder.runs_opened,
        runs_finalized: builder.runs_finalized,
        runs_abandoned: builder.runs_abandoned,
        execution_dispositions: builder.execution_dispositions,
        observed_runs: builder.observed_runs,
        unobserved_runs: builder.unobserved_runs,
        conflicting_runs: builder.conflicting_runs,
        observation_statuses: builder.observation_statuses,
        measurements,
        sample_count: builder.sample_count,
        below_minimum: builder.sample_count < minimum,
    })
}

fn measurement_value(entry: &MeasurementEntry) -> Option<(UnitWitness, u64)> {
    let calibration = entry.measurement().calibration().cloned();
    match entry.measurement().value() {
        MeasurementValue::Proportion(value) => {
            Some((UnitWitness::Proportion(calibration), u64::from(value.get())))
        }
        MeasurementValue::Count(value) => Some((UnitWitness::Count, *value)),
        MeasurementValue::Duration(value) => Some((UnitWitness::DurationMilliseconds, value.get())),
        MeasurementValue::Tokens(value) => Some((
            UnitWitness::Tokens(value.accounting_id().clone()),
            value.count(),
        )),
        MeasurementValue::Money(value) => {
            Some((UnitWitness::Money(value.currency()), value.micros()))
        }
        MeasurementValue::Ordinal {
            scale,
            position,
            of,
        } => Some((UnitWitness::Ordinal(scale.clone(), *of), *position)),
    }
}

fn aggregate_measurement(
    measurement_id: MeasurementId,
    unit_witness: UnitWitness,
    mut values: Vec<u64>,
) -> Result<MeasurementAggregate, EvidenceError> {
    values.sort_unstable();
    let sample_count = u64::try_from(values.len()).map_err(|_| {
        EvidenceError::new(
            "statistics.overflow",
            "measurement sample count exceeds u64",
        )
    })?;
    let total = values.iter().try_fold(0_u64, |sum, value| {
        sum.checked_add(*value)
            .ok_or_else(|| EvidenceError::new("statistics.overflow", "measurement sum exceeds u64"))
    })?;
    let mean = total / sample_count;
    let (low, high) = if matches!(unit_witness, UnitWitness::Proportion(_)) {
        proportion_interval(mean, sample_count)
    } else {
        (None, None)
    };
    Ok(MeasurementAggregate {
        measurement_id,
        unit_witness,
        sample_count,
        minimum: values[0],
        p50: nearest_rank(&values, 50),
        p90: nearest_rank(&values, 90),
        p95: nearest_rank(&values, 95),
        maximum: *values.last().expect("aggregation requires one value"),
        total,
        mean,
        interval_low_ppm: low,
        interval_high_ppm: high,
    })
}

fn nearest_rank(values: &[u64], percentile: usize) -> u64 {
    let rank = values.len().saturating_mul(percentile).saturating_add(99) / 100;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn proportion_interval(mean_ppm: u64, sample_count: u64) -> (Option<u32>, Option<u32>) {
    const SCALE: u128 = 1_000_000;
    let mean = u128::from(mean_ppm.min(1_000_000));
    let variance = mean.saturating_mul(SCALE.saturating_sub(mean));
    let radicand = variance.saturating_mul(4) / u128::from(sample_count.max(1));
    let radius = integer_sqrt(radicand).min(SCALE);
    let low = mean.saturating_sub(radius);
    let high = mean.saturating_add(radius).min(SCALE);
    (u32::try_from(low).ok(), u32::try_from(high).ok())
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut left = 1_u128;
    let mut right = value / 2 + 1;
    while left < right {
        let middle = (left + right).div_ceil(2);
        if middle <= value / middle {
            left = middle;
        } else {
            right = middle - 1;
        }
    }
    left
}

fn snapshot_digest(snapshot: &TopologyStatsSnapshot) -> Result<ContentDigest, EvidenceError> {
    #[derive(Serialize)]
    struct Content<'a> {
        statistics_policy: &'a PolicyRef,
        observation_selection_policy: &'a PolicyRef,
        interval_policy: &'a PolicyRef,
        captured_sequence: LogicalSequence,
        window: &'a StatisticsWindow,
        minimum_sample_count: u64,
        cohorts: &'a [CohortStatistics],
    }
    let content = Content {
        statistics_policy: &snapshot.statistics_policy,
        observation_selection_policy: &snapshot.observation_selection_policy,
        interval_policy: &snapshot.interval_policy,
        captured_sequence: snapshot.captured_at.sequence(),
        window: &snapshot.window,
        minimum_sample_count: snapshot.minimum_sample_count,
        cohorts: &snapshot.cohorts,
    };
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&content, &mut bytes).map_err(|error| {
        EvidenceError::new(
            "statistics.encode",
            format!("cannot encode statistics snapshot: {error}"),
        )
    })?;
    Ok(ContentDigest::sha256(&bytes))
}

fn increment<K: Ord>(counts: &mut BTreeMap<K, u64>, key: K) {
    let count = counts.entry(key).or_default();
    *count = count.saturating_add(1);
}
