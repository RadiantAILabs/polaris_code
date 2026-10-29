//! Shared status, provenance, evidence-reference, and disposition values.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ConfidencePpm, ContentDigest, ContractError, EvidenceObjectId, EvidenceStoreId, SourceKey,
};

/// Applicability and judgement status shared by assessments and observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// The check applied and held.
    Pass,
    /// The check applied and did not hold.
    Issue,
    /// A prerequisite was absent, so the check was void.
    NotApplicable,
    /// The check applied but did not conclude.
    Indeterminate,
}

/// Mechanical disposition of one started graph execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionDisposition {
    /// The graph reached mechanical completion.
    Completed,
    /// Execution failed.
    Failed,
    /// Execution was cancelled.
    Cancelled,
    /// A supervisor explicitly marked an unfinalizable run abandoned.
    Abandoned,
}

/// Treatment of one unresolved assessment or work residual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResidualDisposition {
    /// The residual was resolved.
    Resolved,
    /// The residual remains visible but is intentionally parked.
    Parked,
    /// The residual is deferred to later work.
    Deferred,
    /// The residual cannot proceed under current conditions.
    Blocked,
}

/// Semantic reason a bounded controller or topology stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    /// The declared goal was met.
    GoalMet,
    /// A configured stopping predicate held.
    PredicateSatisfied,
    /// No work remained.
    WorkExhausted,
    /// The configured iteration limit was reached.
    IterationLimit,
    /// A budget was exhausted.
    BudgetExhausted,
    /// Further work saturated under the selected policy.
    Saturated,
    /// Progress stagnated under the selected policy.
    Stagnated,
    /// A circuit breaker opened.
    CircuitBroken,
    /// Remaining work was handed off through escalation.
    EscalatedOut,
    /// A human stopped processing.
    HumanStopped,
}

/// Closed reason an uncertain feature has no known numeric value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnknownReason {
    /// The feature was not observed.
    NotObserved,
    /// The feature does not apply.
    NotApplicable,
    /// Available evidence was insufficient.
    InsufficientEvidence,
    /// Credible evidence disagreed.
    ConflictingEvidence,
    /// The value was intentionally withheld.
    Withheld,
}

/// Closed origin category for one provenance record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// A declaration in the originating request.
    RequestDeclaration,
    /// An embodiment-owned feature extractor.
    EmbodimentExtractor,
    /// A host observation.
    HostObservation,
    /// A Cosmos policy point.
    CosmosPolicy,
    /// A named policy default.
    PolicyDefault,
    /// An automated verifier.
    AutomatedVerifier,
    /// A human review channel.
    HumanReview,
    /// A delayed downstream signal.
    DownstreamSignal,
    /// A simulation source.
    Simulation,
}

/// Epistemic basis attributed by a provenance source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EpistemicBasis {
    /// The source asserted the claim.
    Asserted,
    /// The source verified the claim.
    Verified,
    /// The source independently re-derived the claim.
    ReDerived,
}

/// Closed category of externally retained evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceKind {
    /// Output from an automated verifier.
    VerifierOutput,
    /// A model interaction retained outside the shared record.
    ModelInteraction,
    /// The original task payload.
    TaskPayload,
    /// An artifact or state difference.
    ArtifactDiff,
    /// A bounded observation note.
    ObservationNote,
    /// A human review transcript.
    HumanTranscript,
    /// A report owned by an external system.
    ExternalReport,
}

/// Secret-free opaque reference to evidence held outside shared records.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct EvidenceRef {
    store: EvidenceStoreId,
    kind: EvidenceKind,
    id: EvidenceObjectId,
    content_digest: Option<ContentDigest>,
    media_type: Option<String>,
    byte_len: Option<u64>,
}

impl EvidenceRef {
    /// Creates an opaque evidence reference with no optional metadata.
    #[must_use]
    pub const fn new(store: EvidenceStoreId, kind: EvidenceKind, id: EvidenceObjectId) -> Self {
        Self {
            store,
            kind,
            id,
            content_digest: None,
            media_type: None,
            byte_len: None,
        }
    }

    /// Adds a content digest for resolution-time integrity checking.
    #[must_use]
    pub const fn with_content_digest(mut self, digest: ContentDigest) -> Self {
        self.content_digest = Some(digest);
        self
    }

    /// Adds a validated lowercase IANA media type without parameters.
    pub fn with_media_type(mut self, media_type: impl Into<String>) -> Result<Self, ContractError> {
        let media_type = media_type.into();
        validate_media_type(&media_type)?;
        self.media_type = Some(media_type);
        Ok(self)
    }

    /// Adds the externally retained byte length.
    #[must_use]
    pub const fn with_byte_len(mut self, byte_len: u64) -> Self {
        self.byte_len = Some(byte_len);
        self
    }

    /// Returns the evidence store identity.
    #[must_use]
    pub const fn store(&self) -> &EvidenceStoreId {
        &self.store
    }

    /// Returns the closed evidence category.
    #[must_use]
    pub const fn kind(&self) -> EvidenceKind {
        self.kind
    }

    /// Returns the opaque object identity within the store.
    #[must_use]
    pub const fn id(&self) -> &EvidenceObjectId {
        &self.id
    }

    /// Returns the optional content digest.
    #[must_use]
    pub const fn content_digest(&self) -> Option<ContentDigest> {
        self.content_digest
    }

    /// Returns the optional media type.
    #[must_use]
    pub fn media_type(&self) -> Option<&str> {
        self.media_type.as_deref()
    }

    /// Returns the optional byte length.
    #[must_use]
    pub const fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

impl<'de> Deserialize<'de> for EvidenceRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            store: EvidenceStoreId,
            kind: EvidenceKind,
            id: EvidenceObjectId,
            content_digest: Option<ContentDigest>,
            media_type: Option<String>,
            byte_len: Option<u64>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let mut reference = Self::new(wire.store, wire.kind, wire.id);
        if let Some(digest) = wire.content_digest {
            reference = reference.with_content_digest(digest);
        }
        if let Some(media_type) = wire.media_type {
            reference = reference
                .with_media_type(media_type)
                .map_err(D::Error::custom)?;
        }
        if let Some(byte_len) = wire.byte_len {
            reference = reference.with_byte_len(byte_len);
        }
        Ok(reference)
    }
}

fn validate_media_type(value: &str) -> Result<(), ContractError> {
    let Some((kind, subtype)) = value.split_once('/') else {
        return Err(ContractError::new(
            "evidence.media-type",
            "media type must contain one '/' separator",
        ));
    };
    let valid_token = |token: &str| {
        !token.is_empty()
            && token.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    if value.len() <= 127 && valid_token(kind) && valid_token(subtype) {
        Ok(())
    } else {
        Err(ContractError::new(
            "evidence.media-type",
            "media type must be a bounded lowercase IANA type without parameters",
        ))
    }
}

/// External observation time that has not been assigned evidence-store order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct EvidenceTime {
    logical_ref: Option<EvidenceObjectId>,
    wall_clock_ms: Option<u64>,
}

impl EvidenceTime {
    /// Creates an observation time with at least one external time witness.
    pub fn new(
        logical_ref: Option<EvidenceObjectId>,
        wall_clock_ms: Option<u64>,
    ) -> Result<Self, ContractError> {
        if logical_ref.is_none() && wall_clock_ms.is_none() {
            Err(ContractError::new(
                "evidence.empty-time",
                "evidence time requires a logical reference, wall clock, or both",
            ))
        } else {
            Ok(Self {
                logical_ref,
                wall_clock_ms,
            })
        }
    }

    /// Returns the optional external logical reference.
    #[must_use]
    pub const fn logical_ref(&self) -> Option<&EvidenceObjectId> {
        self.logical_ref.as_ref()
    }

    /// Returns optional Unix-epoch milliseconds.
    #[must_use]
    pub const fn wall_clock_ms(&self) -> Option<u64> {
        self.wall_clock_ms
    }
}

impl<'de> Deserialize<'de> for EvidenceTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            logical_ref: Option<EvidenceObjectId>,
            wall_clock_ms: Option<u64>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.logical_ref, wire.wall_clock_ms).map_err(D::Error::custom)
    }
}

/// Attribution for a shared value or judgement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Provenance {
    source: SourceKey,
    source_kind: SourceKind,
    basis: EpistemicBasis,
    evidence_refs: Vec<EvidenceRef>,
    derived_from: Vec<EvidenceRef>,
    observed_at: Option<EvidenceTime>,
}

impl Provenance {
    /// Creates attribution with no evidence links or observation time.
    #[must_use]
    pub const fn new(source: SourceKey, source_kind: SourceKind, basis: EpistemicBasis) -> Self {
        Self {
            source,
            source_kind,
            basis,
            evidence_refs: Vec::new(),
            derived_from: Vec::new(),
            observed_at: None,
        }
    }

    /// Adds a direct evidence link, preserving canonical set order.
    #[must_use]
    pub fn with_evidence(mut self, reference: EvidenceRef) -> Self {
        insert_canonical(&mut self.evidence_refs, reference);
        self
    }

    /// Adds a derivation link, preserving canonical set order.
    #[must_use]
    pub fn with_derived_from(mut self, reference: EvidenceRef) -> Self {
        insert_canonical(&mut self.derived_from, reference);
        self
    }

    /// Adds optional external observation time.
    #[must_use]
    pub fn observed_at(mut self, observed_at: EvidenceTime) -> Self {
        self.observed_at = Some(observed_at);
        self
    }

    /// Returns the stable source identity.
    #[must_use]
    pub const fn source(&self) -> &SourceKey {
        &self.source
    }

    /// Returns the source category.
    #[must_use]
    pub const fn source_kind(&self) -> SourceKind {
        self.source_kind
    }

    /// Returns the epistemic basis.
    #[must_use]
    pub const fn basis(&self) -> EpistemicBasis {
        self.basis
    }

    /// Returns direct evidence links in canonical order.
    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    /// Returns derivation links in canonical order.
    #[must_use]
    pub fn derived_from(&self) -> &[EvidenceRef] {
        &self.derived_from
    }

    /// Returns optional external observation time.
    #[must_use]
    pub const fn observation_time(&self) -> Option<&EvidenceTime> {
        self.observed_at.as_ref()
    }

    pub(crate) fn has_evidence(&self) -> bool {
        !self.evidence_refs.is_empty() || !self.derived_from.is_empty()
    }

    pub(crate) fn validate(&self) -> Result<(), ContractError> {
        validate_canonical_refs("provenance.evidence-refs", &self.evidence_refs)?;
        validate_canonical_refs("provenance.derived-from", &self.derived_from)
    }

    pub(crate) fn content_view(&self) -> ProvenanceContent<'_> {
        ProvenanceContent {
            source: &self.source,
            source_kind: self.source_kind,
            basis: self.basis,
            evidence_refs: &self.evidence_refs,
            derived_from: &self.derived_from,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ProvenanceContent<'a> {
    source: &'a SourceKey,
    source_kind: SourceKind,
    basis: EpistemicBasis,
    evidence_refs: &'a [EvidenceRef],
    derived_from: &'a [EvidenceRef],
}

fn validate_canonical_refs(
    code: &'static str,
    values: &[EvidenceRef],
) -> Result<(), ContractError> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        Err(ContractError::new(
            code,
            "evidence references must be strictly sorted and duplicate-free",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn insert_canonical<T: Ord>(values: &mut Vec<T>, value: T) {
    match values.binary_search(&value) {
        Ok(_) => {}
        Err(index) => values.insert(index, value),
    }
}

/// Uncertain feature value that never invents confidence for unknown data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct FeatureAssessment<T>(FeatureAssessmentValue<T>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FeatureAssessmentValue<T> {
    Known {
        value: T,
        confidence: ConfidencePpm,
        provenance: Vec<Provenance>,
    },
    Unknown {
        reason: UnknownReason,
        provenance: Vec<Provenance>,
    },
}

impl<T> FeatureAssessment<T> {
    /// Creates a known feature with non-zero confidence and attribution.
    pub fn known(
        value: T,
        confidence: ConfidencePpm,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        if confidence == ConfidencePpm::ZERO {
            return Err(ContractError::new(
                "feature.zero-confidence",
                "a known feature requires confidence in 1..=1_000_000",
            ));
        }
        let provenance = canonical_provenance(provenance)?;
        Ok(Self(FeatureAssessmentValue::Known {
            value,
            confidence,
            provenance,
        }))
    }

    /// Creates an unknown feature with a closed reason and no fake confidence.
    pub fn unknown(
        reason: UnknownReason,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        let provenance = canonical_provenance(provenance)?;
        Ok(Self(FeatureAssessmentValue::Unknown { reason, provenance }))
    }

    /// Returns the known value, when present.
    #[must_use]
    pub const fn known_value(&self) -> Option<&T> {
        match &self.0 {
            FeatureAssessmentValue::Known { value, .. } => Some(value),
            FeatureAssessmentValue::Unknown { .. } => None,
        }
    }

    /// Returns confidence only for a known value.
    #[must_use]
    pub const fn confidence(&self) -> Option<ConfidencePpm> {
        match self.0 {
            FeatureAssessmentValue::Known { confidence, .. } => Some(confidence),
            FeatureAssessmentValue::Unknown { .. } => None,
        }
    }

    /// Returns the closed unknown reason, when unknown.
    #[must_use]
    pub const fn unknown_reason(&self) -> Option<UnknownReason> {
        match self.0 {
            FeatureAssessmentValue::Known { .. } => None,
            FeatureAssessmentValue::Unknown { reason, .. } => Some(reason),
        }
    }

    /// Returns non-empty canonical provenance.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        match &self.0 {
            FeatureAssessmentValue::Known { provenance, .. }
            | FeatureAssessmentValue::Unknown { provenance, .. } => provenance,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for FeatureAssessment<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match FeatureAssessmentValue::deserialize(deserializer)? {
            FeatureAssessmentValue::Known {
                value,
                confidence,
                provenance,
            } => Self::known(value, confidence, provenance).map_err(D::Error::custom),
            FeatureAssessmentValue::Unknown { reason, provenance } => {
                Self::unknown(reason, provenance).map_err(D::Error::custom)
            }
        }
    }
}

pub(crate) fn canonical_provenance(
    mut provenance: Vec<Provenance>,
) -> Result<Vec<Provenance>, ContractError> {
    if provenance.is_empty() {
        return Err(ContractError::new(
            "provenance.empty",
            "at least one provenance record is required",
        ));
    }
    for attribution in &provenance {
        attribution.validate()?;
    }
    provenance.sort();
    provenance.dedup();
    Ok(provenance)
}

/// Store-local total ordering position for evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LogicalSequence(u64);

impl LogicalSequence {
    /// Creates a store-local sequence position.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the monotonic position.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Evidence-store recording position with optional informational wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RecordedAt {
    sequence: LogicalSequence,
    wall_clock_ms: Option<u64>,
}

impl RecordedAt {
    /// Creates recording metadata allocated by an evidence store.
    #[must_use]
    pub const fn new(sequence: LogicalSequence, wall_clock_ms: Option<u64>) -> Self {
        Self {
            sequence,
            wall_clock_ms,
        }
    }

    /// Returns the authoritative store-local order.
    #[must_use]
    pub const fn sequence(self) -> LogicalSequence {
        self.sequence
    }

    /// Returns optional informational Unix-epoch milliseconds.
    #[must_use]
    pub const fn wall_clock_ms(self) -> Option<u64> {
        self.wall_clock_ms
    }
}
