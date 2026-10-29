//! Plain admission, scoring, and selection-decision records.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use polaris::system::plugin::Version;
use serde::de::Error as _;
use serde::ser::SerializeStruct as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::encoding::content_digest;
use super::evidence::canonical_provenance;
use super::profile::{
    Objective, PreferenceCriterion, PreferenceWeights, TieBreakPolicy, validate_strictly_sorted,
};
use super::{
    AdmissionAttemptId, AvailabilitySnapshotId, CanonicalRecord, ConfidencePpm,
    ConfigurationSchemaKey, ContentDigest, ContractError, CoveragePpm, DurationMs, EvidenceRef,
    ExclusionCode, ExternalSessionRef, MeasurementId, Money, PolicyRef, ProbabilityPpm, Provenance,
    RecordSubjectId, SchemaVersion, ScoreComponentId, SelectionDecisionId, SelectorKey, TaskId,
    TokenQuantity, UtilityPpm,
};
use crate::topology::{
    ConfigurationDigest, RegistrationId, TopologyContractId, TopologyContractKey, TopologyId,
    TopologyKey,
};

impl Serialize for RegistrationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RegistrationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Arc::<str>::from(String::deserialize(deserializer)?)).map_err(D::Error::custom)
    }
}

impl Serialize for ConfigurationDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ConfigurationDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Arc::<str>::from(String::deserialize(deserializer)?)).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SemanticVersionWire {
    major: u64,
    minor: u64,
    patch: u64,
}

impl From<Version> for SemanticVersionWire {
    fn from(value: Version) -> Self {
        Self {
            major: value.major,
            minor: value.minor,
            patch: value.patch,
        }
    }
}

impl From<SemanticVersionWire> for Version {
    fn from(value: SemanticVersionWire) -> Self {
        Self::new(value.major, value.minor, value.patch)
    }
}

impl Serialize for TopologyKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("TopologyKey", 2)?;
        state.serialize_field("id", self.id().as_str())?;
        state.serialize_field("version", &SemanticVersionWire::from(self.version()))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for TopologyKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: String,
            version: SemanticVersionWire,
        }
        let wire = Wire::deserialize(deserializer)?;
        let id = TopologyId::new(Arc::<str>::from(wire.id)).map_err(D::Error::custom)?;
        Ok(Self::new(id, wire.version.into()))
    }
}

impl Serialize for TopologyContractKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("TopologyContractKey", 2)?;
        state.serialize_field("id", self.id().as_str())?;
        state.serialize_field("version", &SemanticVersionWire::from(self.version()))?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for TopologyContractKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: String,
            version: SemanticVersionWire,
        }
        let wire = Wire::deserialize(deserializer)?;
        let id = TopologyContractId::new(Arc::<str>::from(wire.id)).map_err(D::Error::custom)?;
        Ok(Self::new(id, wire.version.into()))
    }
}

/// One bounded, deterministic configuration value safe to persist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigurationValue {
    /// Explicit null.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Signed integer.
    Signed(i64),
    /// Unsigned integer.
    Unsigned(u64),
    /// UTF-8 string.
    String(String),
    /// Ordered list.
    List(Vec<ConfigurationValue>),
    /// Canonically sorted string-key map.
    Map(Vec<ConfigurationEntry>),
}

impl ConfigurationValue {
    /// Creates a canonical map value, rejecting empty, duplicate, or oversized keys.
    pub fn map(mut entries: Vec<ConfigurationEntry>) -> Result<Self, ContractError> {
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        if entries.windows(2).any(|pair| pair[0].key == pair[1].key) {
            return Err(ContractError::new(
                "configuration.duplicate-key",
                "configuration maps cannot contain duplicate keys",
            ));
        }
        Ok(Self::Map(entries))
    }

    pub(crate) fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::String(value) if value.len() > 16_384 => Err(ContractError::new(
                "configuration.string-too-long",
                "configuration strings must contain at most 16,384 bytes",
            )),
            Self::List(values) => {
                for value in values {
                    value.validate()?;
                }
                Ok(())
            }
            Self::Map(entries) => {
                if entries.windows(2).any(|pair| pair[0].key >= pair[1].key) {
                    return Err(ContractError::new(
                        "configuration.non-canonical-map",
                        "configuration map keys must be strictly sorted",
                    ));
                }
                for entry in entries {
                    entry.validate()?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// One key-value entry in a canonical configuration map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationEntry {
    key: String,
    value: ConfigurationValue,
}

impl ConfigurationEntry {
    /// Creates a bounded configuration entry.
    pub fn new(key: impl Into<String>, value: ConfigurationValue) -> Result<Self, ContractError> {
        let key = key.into();
        if key.is_empty() || key.len() > 128 {
            return Err(ContractError::new(
                "configuration.key",
                "configuration keys must contain 1..=128 bytes",
            ));
        }
        value.validate()?;
        Ok(Self { key, value })
    }

    /// Returns the string key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the configuration value.
    #[must_use]
    pub const fn value(&self) -> &ConfigurationValue {
        &self.value
    }

    fn validate(&self) -> Result<(), ContractError> {
        if self.key.is_empty() || self.key.len() > 128 {
            return Err(ContractError::new(
                "configuration.key",
                "configuration keys must contain 1..=128 bytes",
            ));
        }
        self.value.validate()
    }
}

/// Location of an immutable, secret-free resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolvedConfiguration {
    /// Complete resolved configuration stored inline.
    Inline(ConfigurationValue),
    /// Content-addressed configuration retained by another capability.
    ContentAddressed(ContentDigest),
}

/// Stable reference to the resolved configuration used by one registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedConfigurationRef {
    schema: ConfigurationSchemaKey,
    digest: ConfigurationDigest,
    resolved: ResolvedConfiguration,
}

impl ResolvedConfigurationRef {
    /// Creates an inline resolved configuration and its SHA-256 digest.
    pub fn inline(
        schema: ConfigurationSchemaKey,
        value: ConfigurationValue,
    ) -> Result<Self, ContractError> {
        value.validate()?;
        let digest = content_digest(&value)?;
        let digest = ConfigurationDigest::new(Arc::<str>::from(digest.to_string()))
            .map_err(|error| ContractError::new("configuration.digest", error.to_string()))?;
        Ok(Self {
            schema,
            digest,
            resolved: ResolvedConfiguration::Inline(value),
        })
    }

    /// Creates a content-addressed resolved-configuration reference.
    pub fn content_addressed(
        schema: ConfigurationSchemaKey,
        digest: ConfigurationDigest,
        reference: ContentDigest,
    ) -> Result<Self, ContractError> {
        let value = Self {
            schema,
            digest,
            resolved: ResolvedConfiguration::ContentAddressed(reference),
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the configuration schema.
    #[must_use]
    pub const fn schema(&self) -> &ConfigurationSchemaKey {
        &self.schema
    }

    /// Returns the immutable configuration digest.
    #[must_use]
    pub const fn digest(&self) -> &ConfigurationDigest {
        &self.digest
    }

    /// Returns the resolved configuration location.
    #[must_use]
    pub const fn resolved(&self) -> &ResolvedConfiguration {
        &self.resolved
    }

    pub(crate) fn validate(&self) -> Result<(), ContractError> {
        let parsed = self
            .digest
            .as_str()
            .parse::<ContentDigest>()
            .map_err(|error| ContractError::new("configuration.digest", error.to_string()))?;
        if let ResolvedConfiguration::Inline(value) = &self.resolved {
            value.validate()?;
            let expected = content_digest(value)?.to_string();
            if self.digest.as_str() != expected {
                return Err(ContractError::new(
                    "configuration.digest-mismatch",
                    "inline configuration digest does not match its canonical value",
                ));
            }
        } else if let ResolvedConfiguration::ContentAddressed(reference) = &self.resolved
            && parsed != *reference
        {
            return Err(ContractError::new(
                "configuration.digest-mismatch",
                "content-addressed configuration digest must match its stable reference",
            ));
        }
        Ok(())
    }
}

/// Immutable candidate identity and configuration captured by selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationSelectionRef {
    registration_id: RegistrationId,
    topology_key: TopologyKey,
    registration_revision_digest: ContentDigest,
    configuration: ResolvedConfigurationRef,
}

impl RegistrationSelectionRef {
    /// Creates a captured registration reference.
    pub fn new(
        registration_id: RegistrationId,
        topology_key: TopologyKey,
        registration_revision_digest: ContentDigest,
        configuration: ResolvedConfigurationRef,
    ) -> Result<Self, ContractError> {
        configuration.validate()?;
        Ok(Self {
            registration_id,
            topology_key,
            registration_revision_digest,
            configuration,
        })
    }

    /// Returns the registration identity.
    #[must_use]
    pub const fn registration_id(&self) -> &RegistrationId {
        &self.registration_id
    }

    /// Returns the topology identity and version.
    #[must_use]
    pub const fn topology_key(&self) -> &TopologyKey {
        &self.topology_key
    }

    /// Returns the registration revision digest.
    #[must_use]
    pub const fn registration_revision_digest(&self) -> ContentDigest {
        self.registration_revision_digest
    }

    /// Returns the resolved configuration reference.
    #[must_use]
    pub const fn configuration(&self) -> &ResolvedConfigurationRef {
        &self.configuration
    }

    pub(crate) fn validate(&self) -> Result<(), ContractError> {
        self.configuration.validate()
    }
}

/// Selector definition and immutable selector configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectorRef {
    key: SelectorKey,
    configuration_digest: ConfigurationDigest,
}

impl SelectorRef {
    /// Creates a selector reference.
    pub fn new(
        key: SelectorKey,
        configuration_digest: ConfigurationDigest,
    ) -> Result<Self, ContractError> {
        configuration_digest
            .as_str()
            .parse::<ContentDigest>()
            .map_err(|error| ContractError::new("configuration.digest", error.to_string()))?;
        Ok(Self {
            key,
            configuration_digest,
        })
    }

    /// Returns the selector key.
    #[must_use]
    pub const fn key(&self) -> &SelectorKey {
        &self.key
    }

    /// Returns the selector configuration digest.
    #[must_use]
    pub const fn configuration_digest(&self) -> &ConfigurationDigest {
        &self.configuration_digest
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.configuration_digest
            .as_str()
            .parse::<ContentDigest>()
            .map(|_| ())
            .map_err(|error| ContractError::new("configuration.digest", error.to_string()))
    }
}

/// Immutable references to all selection inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionInputRefs {
    task_profile_digest: ContentDigest,
    objective_digest: ContentDigest,
    catalog_snapshot_digest: ContentDigest,
    availability_snapshot_id: AvailabilitySnapshotId,
    availability_snapshot_digest: ContentDigest,
    statistics_snapshot_digest: ContentDigest,
}

impl SelectionInputRefs {
    /// Creates the complete immutable input-reference set.
    #[must_use]
    pub const fn new(
        task_profile_digest: ContentDigest,
        objective_digest: ContentDigest,
        catalog_snapshot_digest: ContentDigest,
        availability_snapshot_id: AvailabilitySnapshotId,
        availability_snapshot_digest: ContentDigest,
        statistics_snapshot_digest: ContentDigest,
    ) -> Self {
        Self {
            task_profile_digest,
            objective_digest,
            catalog_snapshot_digest,
            availability_snapshot_id,
            availability_snapshot_digest,
            statistics_snapshot_digest,
        }
    }

    /// Returns the task-profile digest.
    #[must_use]
    pub const fn task_profile_digest(&self) -> ContentDigest {
        self.task_profile_digest
    }

    /// Returns the objective digest.
    #[must_use]
    pub const fn objective_digest(&self) -> ContentDigest {
        self.objective_digest
    }

    /// Returns the catalog content digest.
    #[must_use]
    pub const fn catalog_snapshot_digest(&self) -> ContentDigest {
        self.catalog_snapshot_digest
    }

    /// Returns the availability observation identity.
    #[must_use]
    pub const fn availability_snapshot_id(&self) -> AvailabilitySnapshotId {
        self.availability_snapshot_id
    }

    /// Returns the availability content digest.
    #[must_use]
    pub const fn availability_snapshot_digest(&self) -> ContentDigest {
        self.availability_snapshot_digest
    }

    /// Returns the statistics content digest.
    #[must_use]
    pub const fn statistics_snapshot_digest(&self) -> ContentDigest {
        self.statistics_snapshot_digest
    }
}

/// Phase that excluded a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExclusionPhase {
    /// Immutable mechanical preflight.
    Mechanical,
    /// Hard objective policy.
    Policy,
    /// Live pre-start readmission.
    LiveReadmission,
}

/// Stable, forward-compatible reason a candidate could not proceed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExclusionReason {
    phase: ExclusionPhase,
    code: ExclusionCode,
    subjects: Vec<RecordSubjectId>,
    required: Option<RecordSubjectId>,
    observed: Option<RecordSubjectId>,
    snapshot_refs: Vec<ContentDigest>,
    detail: Option<String>,
}

impl ExclusionReason {
    /// Creates one exclusion reason with canonical subjects and snapshot links.
    pub fn new(
        phase: ExclusionPhase,
        code: ExclusionCode,
        mut subjects: Vec<RecordSubjectId>,
        mut snapshot_refs: Vec<ContentDigest>,
    ) -> Self {
        subjects.sort();
        subjects.dedup();
        snapshot_refs.sort();
        snapshot_refs.dedup();
        Self {
            phase,
            code,
            subjects,
            required: None,
            observed: None,
            snapshot_refs,
            detail: None,
        }
    }

    /// Adds stable required and observed values.
    #[must_use]
    pub fn with_comparison(
        mut self,
        required: Option<RecordSubjectId>,
        observed: Option<RecordSubjectId>,
    ) -> Self {
        self.required = required;
        self.observed = observed;
        self
    }

    /// Adds bounded display-only detail.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Result<Self, ContractError> {
        let detail = detail.into();
        if detail.len() > 1_024 {
            return Err(ContractError::new(
                "exclusion.detail-too-long",
                "exclusion detail must contain at most 1,024 bytes",
            ));
        }
        self.detail = Some(detail);
        Ok(self)
    }

    /// Returns the phase.
    #[must_use]
    pub const fn phase(&self) -> ExclusionPhase {
        self.phase
    }

    /// Returns the extensible exclusion code.
    #[must_use]
    pub const fn code(&self) -> &ExclusionCode {
        &self.code
    }

    /// Returns canonical stable subjects implicated by the exclusion.
    #[must_use]
    pub fn subjects(&self) -> &[RecordSubjectId] {
        &self.subjects
    }

    /// Returns the required stable value, when a comparison was recorded.
    #[must_use]
    pub const fn required(&self) -> Option<&RecordSubjectId> {
        self.required.as_ref()
    }

    /// Returns the observed stable value, when a comparison was recorded.
    #[must_use]
    pub const fn observed(&self) -> Option<&RecordSubjectId> {
        self.observed.as_ref()
    }

    /// Returns immutable snapshot identities that support the exclusion.
    #[must_use]
    pub fn snapshot_refs(&self) -> &[ContentDigest] {
        &self.snapshot_refs
    }

    /// Returns optional display-only detail.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub(crate) fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted("exclusion.subjects", &self.subjects)?;
        validate_strictly_sorted("exclusion.snapshot-refs", &self.snapshot_refs)?;
        if self
            .detail
            .as_ref()
            .is_some_and(|value| value.len() > 1_024)
        {
            return Err(ContractError::new(
                "exclusion.detail-too-long",
                "exclusion detail must contain at most 1,024 bytes",
            ));
        }
        Ok(())
    }
}

/// Mechanical admission result for a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdmissionResult {
    /// Candidate passed mechanical preflight.
    Admitted,
    /// Candidate was mechanically excluded with complete reasons.
    Excluded {
        /// Complete mechanical exclusion reasons.
        reasons: Vec<ExclusionReason>,
    },
}

impl AdmissionResult {
    /// Creates an excluded result with at least one canonical reason.
    pub fn excluded(mut reasons: Vec<ExclusionReason>) -> Result<Self, ContractError> {
        canonicalize_reasons(&mut reasons)?;
        Ok(Self::Excluded { reasons })
    }

    /// Returns whether admission succeeded.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }

    /// Returns complete exclusion reasons, or an empty slice when admitted.
    #[must_use]
    pub fn reasons(&self) -> &[ExclusionReason] {
        match self {
            Self::Admitted => &[],
            Self::Excluded { reasons } => reasons,
        }
    }

    fn validate_for(&self, phase: ExclusionPhase) -> Result<(), ContractError> {
        match self {
            Self::Admitted => Ok(()),
            Self::Excluded { reasons } => validate_reasons(reasons, Some(phase)),
        }
    }
}

/// Hard policy result after mechanical admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyEligibility {
    /// Policy was intentionally skipped after mechanical exclusion.
    NotEvaluated,
    /// Candidate passed hard policy constraints.
    Eligible,
    /// Candidate was excluded by hard policy.
    Excluded {
        /// Complete hard-policy exclusion reasons.
        reasons: Vec<ExclusionReason>,
    },
}

impl PolicyEligibility {
    /// Creates a policy exclusion with at least one canonical reason.
    pub fn excluded(mut reasons: Vec<ExclusionReason>) -> Result<Self, ContractError> {
        canonicalize_reasons(&mut reasons)?;
        Ok(Self::Excluded { reasons })
    }

    /// Returns whether hard policy admitted the candidate.
    #[must_use]
    pub const fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible)
    }

    /// Returns complete policy exclusion reasons, or an empty slice otherwise.
    #[must_use]
    pub fn reasons(&self) -> &[ExclusionReason] {
        match self {
            Self::Excluded { reasons } => reasons,
            Self::NotEvaluated | Self::Eligible => &[],
        }
    }

    fn validate(&self) -> Result<(), ContractError> {
        if let Self::Excluded { reasons } = self {
            validate_reasons(reasons, Some(ExclusionPhase::Policy))?;
        }
        Ok(())
    }
}

fn canonicalize_reasons(reasons: &mut Vec<ExclusionReason>) -> Result<(), ContractError> {
    if reasons.is_empty() {
        return Err(ContractError::new(
            "exclusion.empty",
            "an excluded result requires at least one reason",
        ));
    }
    for reason in reasons.iter() {
        reason.validate()?;
    }
    reasons.sort();
    reasons.dedup();
    Ok(())
}

fn validate_reasons(
    reasons: &[ExclusionReason],
    expected_phase: Option<ExclusionPhase>,
) -> Result<(), ContractError> {
    if reasons.is_empty() {
        return Err(ContractError::new(
            "exclusion.empty",
            "an excluded result requires at least one reason",
        ));
    }
    for reason in reasons {
        reason.validate()?;
        if expected_phase.is_some_and(|phase| phase != reason.phase) {
            return Err(ContractError::new(
                "exclusion.phase-mismatch",
                "exclusion reason is attached to the wrong phase",
            ));
        }
    }
    if reasons.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ContractError::new(
            "exclusion.non-canonical",
            "exclusion reasons must be sorted and duplicate-free",
        ));
    }
    Ok(())
}

/// Comparable measurement value with an explicit unit witness where required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeasurementValue {
    /// Proportion in integer parts per million.
    Proportion(CoveragePpm),
    /// Unsigned count.
    Count(u64),
    /// Unsigned duration.
    Duration(DurationMs),
    /// Tokens under a stable accounting scheme.
    Tokens(TokenQuantity),
    /// Money in one currency.
    Money(Money),
    /// Position on a named ordinal scale.
    Ordinal {
        /// Stable scale identity.
        scale: PolicyRef,
        /// One-based position.
        position: u64,
        /// Number of positions on the scale.
        of: u64,
    },
}

impl MeasurementValue {
    fn validate(&self) -> Result<(), ContractError> {
        if let Self::Ordinal { position, of, .. } = self
            && (*of == 0 || *position == 0 || position > of)
        {
            return Err(ContractError::new(
                "measurement.ordinal-range",
                "ordinal position must be in 1..=of and of must be non-zero",
            ));
        }
        Ok(())
    }
}

/// Attributed measurement used by scores and outcome observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measurement {
    schema_version: SchemaVersion,
    value: MeasurementValue,
    calibration: Option<PolicyRef>,
    confidence: ConfidencePpm,
    provenance: Vec<Provenance>,
}

impl Measurement {
    /// Creates an attributed measurement.
    pub fn new(
        value: MeasurementValue,
        calibration: Option<PolicyRef>,
        confidence: ConfidencePpm,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        value.validate()?;
        Ok(Self {
            schema_version: SchemaVersion::V1,
            value,
            calibration,
            confidence,
            provenance: canonical_provenance(provenance)?,
        })
    }

    /// Returns the measured value.
    #[must_use]
    pub const fn value(&self) -> &MeasurementValue {
        &self.value
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the optional calibration policy.
    #[must_use]
    pub const fn calibration(&self) -> Option<&PolicyRef> {
        self.calibration.as_ref()
    }

    /// Returns confidence in the measurement.
    #[must_use]
    pub const fn confidence(&self) -> ConfidencePpm {
        self.confidence
    }

    /// Returns measurement provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.value.validate()?;
        validate_provenance(&self.provenance)
    }
}

/// Origin of one score component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScoreSource {
    /// Descriptor author prior.
    AuthorPrior,
    /// Immutable historical statistics.
    HistoricalStatistics,
    /// Task-profile evidence.
    ProfileDerived,
    /// Availability evidence.
    AvailabilityDerived,
    /// Selector-owned policy.
    SelectorPolicy,
}

/// Named, inspectable contribution to a candidate score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreComponent {
    component_id: ScoreComponentId,
    criterion: PreferenceCriterion,
    source: ScoreSource,
    raw_measurement: Measurement,
    normalization_policy: PolicyRef,
    normalized_utility: UtilityPpm,
    confidence: ConfidencePpm,
    provenance: Vec<Provenance>,
    statistics_snapshot_digest: Option<ContentDigest>,
}

impl ScoreComponent {
    /// Creates one score component.
    #[expect(
        clippy::too_many_arguments,
        reason = "the score component is an audit record"
    )]
    pub fn new(
        component_id: ScoreComponentId,
        criterion: PreferenceCriterion,
        source: ScoreSource,
        raw_measurement: Measurement,
        normalization_policy: PolicyRef,
        normalized_utility: UtilityPpm,
        confidence: ConfidencePpm,
        provenance: Vec<Provenance>,
        statistics_snapshot_digest: Option<ContentDigest>,
    ) -> Result<Self, ContractError> {
        raw_measurement.validate()?;
        if normalization_policy.version() == Version::new(0, 0, 0) {
            return Err(ContractError::new(
                "score.zero-normalization-version",
                "normalization policy must have a non-zero semantic version",
            ));
        }
        if matches!(source, ScoreSource::HistoricalStatistics)
            != statistics_snapshot_digest.is_some()
        {
            return Err(ContractError::new(
                "score.statistics-reference",
                "historical components require a statistics snapshot and other sources forbid it",
            ));
        }
        Ok(Self {
            component_id,
            criterion,
            source,
            raw_measurement,
            normalization_policy,
            normalized_utility,
            confidence,
            provenance: canonical_provenance(provenance)?,
            statistics_snapshot_digest,
        })
    }

    /// Returns the component identity.
    #[must_use]
    pub const fn component_id(&self) -> &ScoreComponentId {
        &self.component_id
    }

    /// Returns the weighted criterion informed by this component.
    #[must_use]
    pub const fn criterion(&self) -> PreferenceCriterion {
        self.criterion
    }

    /// Returns the component's evidence source class.
    #[must_use]
    pub const fn source(&self) -> ScoreSource {
        self.source
    }

    /// Returns the raw attributed measurement.
    #[must_use]
    pub const fn raw_measurement(&self) -> &Measurement {
        &self.raw_measurement
    }

    /// Returns the normalization policy.
    #[must_use]
    pub const fn normalization_policy(&self) -> &PolicyRef {
        &self.normalization_policy
    }

    /// Returns normalized integer utility.
    #[must_use]
    pub const fn normalized_utility(&self) -> UtilityPpm {
        self.normalized_utility
    }

    /// Returns confidence in the component.
    #[must_use]
    pub const fn confidence(&self) -> ConfidencePpm {
        self.confidence
    }

    /// Returns component provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns the exact statistics snapshot used by a historical component.
    #[must_use]
    pub const fn statistics_snapshot_digest(&self) -> Option<ContentDigest> {
        self.statistics_snapshot_digest
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.raw_measurement.validate()?;
        validate_provenance(&self.provenance)?;
        if self.normalization_policy.version() == Version::new(0, 0, 0) {
            return Err(ContractError::new(
                "score.zero-normalization-version",
                "normalization policy must have a non-zero semantic version",
            ));
        }
        if matches!(self.source, ScoreSource::HistoricalStatistics)
            != self.statistics_snapshot_digest.is_some()
        {
            return Err(ContractError::new(
                "score.statistics-reference",
                "historical components require a statistics snapshot and other sources forbid it",
            ));
        }
        Ok(())
    }
}

/// Final utility assigned to one weighted criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionUtility {
    criterion: PreferenceCriterion,
    utility: UtilityPpm,
}

impl CriterionUtility {
    /// Creates a criterion utility.
    #[must_use]
    pub const fn new(criterion: PreferenceCriterion, utility: UtilityPpm) -> Self {
        Self { criterion, utility }
    }

    /// Returns the criterion.
    #[must_use]
    pub const fn criterion(self) -> PreferenceCriterion {
        self.criterion
    }

    /// Returns normalized utility.
    #[must_use]
    pub const fn utility(self) -> UtilityPpm {
        self.utility
    }
}

/// Complete score for one eligible candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateScore {
    components: Vec<ScoreComponent>,
    criterion_utilities: Vec<CriterionUtility>,
    weighted_utility_numerator: u64,
}

impl CandidateScore {
    /// Creates a complete candidate score.
    pub fn new(
        mut components: Vec<ScoreComponent>,
        mut criterion_utilities: Vec<CriterionUtility>,
        weights: &PreferenceWeights,
    ) -> Result<Self, ContractError> {
        if components.is_empty() {
            return Err(ContractError::new(
                "score.empty-components",
                "candidate score requires at least one inspectable component",
            ));
        }
        components.sort_by(|left, right| left.component_id.cmp(&right.component_id));
        if components
            .windows(2)
            .any(|pair| pair[0].component_id == pair[1].component_id)
        {
            return Err(ContractError::new(
                "score.duplicate-component",
                "score component IDs must be unique",
            ));
        }
        criterion_utilities.sort_by_key(|value| value.criterion);
        let weighted_utility_numerator = weighted_utility(&criterion_utilities, weights)?;
        let score = Self {
            components,
            criterion_utilities,
            weighted_utility_numerator,
        };
        score.validate()?;
        Ok(score)
    }

    /// Returns the unrounded weighted numerator used for ordering.
    #[must_use]
    pub const fn weighted_utility_numerator(&self) -> u64 {
        self.weighted_utility_numerator
    }

    /// Returns score components in canonical identity order.
    #[must_use]
    pub fn components(&self) -> &[ScoreComponent] {
        &self.components
    }

    /// Returns exactly one utility for each closed preference criterion.
    #[must_use]
    pub fn criterion_utilities(&self) -> &[CriterionUtility] {
        &self.criterion_utilities
    }

    fn validate(&self) -> Result<(), ContractError> {
        if self.components.is_empty() {
            return Err(ContractError::new(
                "score.empty-components",
                "candidate score requires at least one inspectable component",
            ));
        }
        for component in &self.components {
            component.validate()?;
        }
        if self
            .components
            .windows(2)
            .any(|pair| pair[0].component_id >= pair[1].component_id)
        {
            return Err(ContractError::new(
                "score.non-canonical-components",
                "score components must be sorted and duplicate-free",
            ));
        }
        const CRITERIA: [PreferenceCriterion; 8] = [
            PreferenceCriterion::Quality,
            PreferenceCriterion::VerifiedCoverage,
            PreferenceCriterion::Latency,
            PreferenceCriterion::TokenCost,
            PreferenceCriterion::MonetaryCost,
            PreferenceCriterion::ExecutionFailureRisk,
            PreferenceCriterion::HumanIntervention,
            PreferenceCriterion::Exploration,
        ];
        if self.criterion_utilities.len() != CRITERIA.len()
            || self
                .criterion_utilities
                .iter()
                .map(|value| value.criterion)
                .ne(CRITERIA)
        {
            return Err(ContractError::new(
                "score.incomplete-criteria",
                "candidate score requires exactly one utility for every closed criterion",
            ));
        }
        Ok(())
    }

    fn validate_with_preferences(&self, weights: &PreferenceWeights) -> Result<(), ContractError> {
        self.validate()?;
        let expected = weighted_utility(&self.criterion_utilities, weights)?;
        if expected == self.weighted_utility_numerator {
            Ok(())
        } else {
            Err(ContractError::new(
                "score.weighted-utility-mismatch",
                "weighted utility numerator does not match the objective preference vector",
            ))
        }
    }
}

fn weighted_utility(
    utilities: &[CriterionUtility],
    weights: &PreferenceWeights,
) -> Result<u64, ContractError> {
    utilities.iter().try_fold(0_u64, |total, value| {
        let contribution = u64::from(weights.get(value.criterion).get())
            .checked_mul(u64::from(value.utility.get()))
            .ok_or_else(|| {
                ContractError::new(
                    "score.weighted-utility-overflow",
                    "weighted criterion utility overflowed u64",
                )
            })?;
        total.checked_add(contribution).ok_or_else(|| {
            ContractError::new(
                "score.weighted-utility-overflow",
                "candidate weighted utility overflowed u64",
            )
        })
    })
}

/// Complete consideration of one catalog candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateConsideration {
    candidate: RegistrationSelectionRef,
    mechanical_admission: AdmissionResult,
    policy_eligibility: PolicyEligibility,
    score: Option<CandidateScore>,
    rank: Option<u64>,
    selection_propensity: ProbabilityPpm,
}

impl CandidateConsideration {
    /// Creates a mechanically excluded candidate.
    pub fn mechanically_excluded(
        candidate: RegistrationSelectionRef,
        mechanical_admission: AdmissionResult,
    ) -> Result<Self, ContractError> {
        let value = Self {
            candidate,
            mechanical_admission,
            policy_eligibility: PolicyEligibility::NotEvaluated,
            score: None,
            rank: None,
            selection_propensity: ProbabilityPpm::ZERO,
        };
        value.validate()?;
        Ok(value)
    }

    /// Creates a candidate excluded by hard policy after mechanical admission.
    pub fn policy_excluded(
        candidate: RegistrationSelectionRef,
        policy_eligibility: PolicyEligibility,
    ) -> Result<Self, ContractError> {
        let value = Self {
            candidate,
            mechanical_admission: AdmissionResult::Admitted,
            policy_eligibility,
            score: None,
            rank: None,
            selection_propensity: ProbabilityPpm::ZERO,
        };
        value.validate()?;
        Ok(value)
    }

    /// Creates an eligible, ranked candidate.
    pub fn eligible(
        candidate: RegistrationSelectionRef,
        score: CandidateScore,
        rank: u64,
        selection_propensity: ProbabilityPpm,
    ) -> Result<Self, ContractError> {
        let value = Self {
            candidate,
            mechanical_admission: AdmissionResult::Admitted,
            policy_eligibility: PolicyEligibility::Eligible,
            score: Some(score),
            rank: Some(rank),
            selection_propensity,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the captured candidate.
    #[must_use]
    pub const fn candidate(&self) -> &RegistrationSelectionRef {
        &self.candidate
    }

    /// Returns whether both mechanical and policy admission succeeded.
    #[must_use]
    pub const fn is_eligible(&self) -> bool {
        self.mechanical_admission.is_admitted() && self.policy_eligibility.is_eligible()
    }

    /// Returns captured mechanical preflight.
    #[must_use]
    pub const fn mechanical_admission(&self) -> &AdmissionResult {
        &self.mechanical_admission
    }

    /// Returns hard-policy eligibility after mechanical preflight.
    #[must_use]
    pub const fn policy_eligibility(&self) -> &PolicyEligibility {
        &self.policy_eligibility
    }

    /// Returns the score for an eligible candidate.
    #[must_use]
    pub const fn score(&self) -> Option<&CandidateScore> {
        self.score.as_ref()
    }

    /// Returns the one-based rank for an eligible candidate.
    #[must_use]
    pub const fn rank(&self) -> Option<u64> {
        self.rank
    }

    /// Returns the selection propensity.
    #[must_use]
    pub const fn selection_propensity(&self) -> ProbabilityPpm {
        self.selection_propensity
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.candidate.validate()?;
        self.mechanical_admission
            .validate_for(ExclusionPhase::Mechanical)?;
        self.policy_eligibility.validate()?;
        match (
            self.mechanical_admission.is_admitted(),
            &self.policy_eligibility,
            &self.score,
            self.rank,
            self.selection_propensity,
        ) {
            (false, PolicyEligibility::NotEvaluated, None, None, ProbabilityPpm::ZERO) => Ok(()),
            (true, PolicyEligibility::Excluded { .. }, None, None, ProbabilityPpm::ZERO) => Ok(()),
            (true, PolicyEligibility::Eligible, Some(score), Some(rank), _) if rank > 0 => {
                score.validate()
            }
            _ => Err(ContractError::new(
                "selection.candidate-state",
                "candidate score, rank, propensity, and policy state are inconsistent",
            )),
        }
    }
}

/// One captured pre-start fallback candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackCandidate {
    candidate: RegistrationSelectionRef,
    captured_preflight: AdmissionResult,
}

impl FallbackCandidate {
    /// Creates a fallback that passed captured mechanical preflight.
    pub fn new(candidate: RegistrationSelectionRef) -> Result<Self, ContractError> {
        candidate.validate()?;
        Ok(Self {
            candidate,
            captured_preflight: AdmissionResult::Admitted,
        })
    }

    /// Returns the captured fallback candidate.
    #[must_use]
    pub const fn candidate(&self) -> &RegistrationSelectionRef {
        &self.candidate
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.candidate.validate()?;
        if !self.captured_preflight.is_admitted() {
            return Err(ContractError::new(
                "selection.fallback-not-admitted",
                "fallback plan can contain only mechanically admitted candidates",
            ));
        }
        Ok(())
    }
}

/// Structured explanation of why one eligible candidate was selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRationale {
    decisive_components: Vec<ScoreComponentId>,
    tie_break_applied: bool,
    exploration_changed_winner: bool,
    display_summary: Option<String>,
    confidence_policy: PolicyRef,
}

impl SelectionRationale {
    /// Creates a canonical structured rationale.
    pub fn new(
        mut decisive_components: Vec<ScoreComponentId>,
        tie_break_applied: bool,
        exploration_changed_winner: bool,
        display_summary: Option<String>,
        confidence_policy: PolicyRef,
    ) -> Result<Self, ContractError> {
        decisive_components.sort();
        decisive_components.dedup();
        if decisive_components.is_empty() {
            return Err(ContractError::new(
                "selection.empty-rationale",
                "selection rationale requires at least one decisive score component",
            ));
        }
        if display_summary
            .as_ref()
            .is_some_and(|summary| summary.len() > 2_048)
        {
            return Err(ContractError::new(
                "selection.rationale-too-long",
                "display rationale must contain at most 2,048 bytes",
            ));
        }
        Ok(Self {
            decisive_components,
            tie_break_applied,
            exploration_changed_winner,
            display_summary,
            confidence_policy,
        })
    }

    /// Returns decisive score-component identities in canonical order.
    #[must_use]
    pub fn decisive_components(&self) -> &[ScoreComponentId] {
        &self.decisive_components
    }

    /// Returns whether the explicit tie-break policy was applied.
    #[must_use]
    pub const fn tie_break_applied(&self) -> bool {
        self.tie_break_applied
    }

    /// Returns whether exploration changed the highest-utility winner.
    #[must_use]
    pub const fn exploration_changed_winner(&self) -> bool {
        self.exploration_changed_winner
    }

    /// Returns optional display-only rationale text.
    #[must_use]
    pub fn display_summary(&self) -> Option<&str> {
        self.display_summary.as_deref()
    }

    /// Returns the policy used to derive decision confidence.
    #[must_use]
    pub const fn confidence_policy(&self) -> &PolicyRef {
        &self.confidence_policy
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted("selection.decisive-components", &self.decisive_components)?;
        if self.decisive_components.is_empty() {
            return Err(ContractError::new(
                "selection.empty-rationale",
                "selection rationale requires at least one decisive score component",
            ));
        }
        if self
            .display_summary
            .as_ref()
            .is_some_and(|summary| summary.len() > 2_048)
        {
            return Err(ContractError::new(
                "selection.rationale-too-long",
                "display rationale must contain at most 2,048 bytes",
            ));
        }
        Ok(())
    }
}

/// Aggregated exclusion diagnostic for a no-eligible decision.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExclusionSummary {
    phase: ExclusionPhase,
    code: ExclusionCode,
    candidate_count: u64,
}

impl ExclusionSummary {
    /// Creates a non-zero exclusion summary.
    pub fn new(
        phase: ExclusionPhase,
        code: ExclusionCode,
        candidate_count: u64,
    ) -> Result<Self, ContractError> {
        if candidate_count == 0 && code.as_str() != "catalog.empty" {
            Err(ContractError::new(
                "selection.empty-exclusion-summary",
                "only catalog.empty may summarize zero candidates",
            ))
        } else {
            Ok(Self {
                phase,
                code,
                candidate_count,
            })
        }
    }

    /// Returns the extensible exclusion code.
    #[must_use]
    pub const fn code(&self) -> &ExclusionCode {
        &self.code
    }

    /// Returns the phase summarized by this diagnostic.
    #[must_use]
    pub const fn phase(&self) -> ExclusionPhase {
        self.phase
    }

    /// Returns the number of distinct candidates excluded under this code.
    #[must_use]
    pub const fn candidate_count(&self) -> u64 {
        self.candidate_count
    }
}

/// Reproducible evidence for an exploratory draw from the recorded propensities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SamplingProvenance {
    policy: PolicyRef,
    randomness_reference: EvidenceRef,
    provenance: Vec<Provenance>,
}

impl SamplingProvenance {
    /// Creates an attributed sampling record with an opaque randomness reference.
    pub fn new(
        policy: PolicyRef,
        randomness_reference: EvidenceRef,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        if policy.version() == Version::new(0, 0, 0) {
            return Err(ContractError::new(
                "selection.zero-sampling-version",
                "sampling policy must have a non-zero semantic version",
            ));
        }
        Ok(Self {
            policy,
            randomness_reference,
            provenance: canonical_provenance(provenance)?,
        })
    }

    /// Returns the sampling policy.
    #[must_use]
    pub const fn policy(&self) -> &PolicyRef {
        &self.policy
    }

    /// Returns the opaque randomness reference.
    #[must_use]
    pub const fn randomness_reference(&self) -> &EvidenceRef {
        &self.randomness_reference
    }

    /// Returns sampling provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    fn validate(&self) -> Result<(), ContractError> {
        if self.policy.version() == Version::new(0, 0, 0) {
            return Err(ContractError::new(
                "selection.zero-sampling-version",
                "sampling policy must have a non-zero semantic version",
            ));
        }
        validate_provenance(&self.provenance)
    }
}

/// Result of a complete selection evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[expect(
    clippy::large_enum_variant,
    reason = "the selected outcome keeps its complete immutable audit record inline"
)]
pub enum SelectionOutcome {
    /// One candidate was selected with a pre-start-only fallback plan.
    Selected {
        /// Selected registration and resolved configuration.
        selected: RegistrationSelectionRef,
        /// Remaining eligible candidates in selection order.
        fallback_plan: Vec<FallbackCandidate>,
        /// Structured explanation.
        rationale: SelectionRationale,
        /// Confidence in winner inputs that affected eligibility or score.
        confidence: ConfidencePpm,
        /// Selected candidate's probability under the selector policy.
        selected_propensity: ProbabilityPpm,
        /// Reproducible sampling evidence, required for a non-deterministic distribution.
        sampling: Option<SamplingProvenance>,
    },
    /// Selection completed but no candidate passed all hard gates.
    NoEligibleCandidate {
        /// Complete grouped diagnostics, including `catalog.empty` when applicable.
        diagnostics: Vec<ExclusionSummary>,
    },
}

/// Immutable selection decision containing the complete candidate universe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionDecision {
    schema_version: SchemaVersion,
    decision_id: SelectionDecisionId,
    fingerprint: ContentDigest,
    task_id: TaskId,
    external_session: Option<ExternalSessionRef>,
    selector: SelectorRef,
    requested_contract: TopologyContractKey,
    input_refs: SelectionInputRefs,
    candidates: Vec<CandidateConsideration>,
    outcome: SelectionOutcome,
}

impl SelectionDecision {
    /// Creates a complete version-one selection decision.
    #[expect(
        clippy::too_many_arguments,
        reason = "the decision joins all immutable inputs"
    )]
    pub fn new(
        decision_id: SelectionDecisionId,
        task_id: TaskId,
        external_session: Option<ExternalSessionRef>,
        selector: SelectorRef,
        requested_contract: TopologyContractKey,
        input_refs: SelectionInputRefs,
        mut candidates: Vec<CandidateConsideration>,
        mut outcome: SelectionOutcome,
    ) -> Result<Self, ContractError> {
        candidates.sort_by(|left, right| {
            left.candidate
                .registration_id
                .cmp(&right.candidate.registration_id)
        });
        if let SelectionOutcome::NoEligibleCandidate { diagnostics } = &mut outcome {
            diagnostics.sort();
        }
        let mut decision = Self {
            schema_version: SchemaVersion::V1,
            decision_id,
            fingerprint: ContentDigest::sha256(&[]),
            task_id,
            external_session,
            selector,
            requested_contract,
            input_refs,
            candidates,
            outcome,
        };
        decision.validate_structure()?;
        decision.fingerprint = content_digest(&decision.fingerprint_view())?;
        Ok(decision)
    }

    /// Returns the event identity.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns deterministic content identity excluding the event ID.
    #[must_use]
    pub const fn fingerprint(&self) -> ContentDigest {
        self.fingerprint
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

    /// Returns all catalog candidates in canonical order.
    #[must_use]
    pub fn candidates(&self) -> &[CandidateConsideration] {
        &self.candidates
    }

    /// Returns the selected or no-eligible outcome.
    #[must_use]
    pub const fn outcome(&self) -> &SelectionOutcome {
        &self.outcome
    }

    /// Returns immutable input references.
    #[must_use]
    pub const fn input_refs(&self) -> &SelectionInputRefs {
        &self.input_refs
    }

    fn fingerprint_view(&self) -> SelectionFingerprint<'_> {
        SelectionFingerprint {
            schema_version: self.schema_version,
            task_id: &self.task_id,
            external_session: self.external_session.as_ref(),
            selector: &self.selector,
            requested_contract: &self.requested_contract,
            input_refs: &self.input_refs,
            candidates: &self.candidates,
            outcome: &self.outcome,
        }
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.selector.validate()?;
        for candidate in &self.candidates {
            candidate.validate()?;
        }
        if self
            .candidates
            .windows(2)
            .any(|pair| pair[0].candidate.registration_id >= pair[1].candidate.registration_id)
        {
            return Err(ContractError::new(
                "selection.non-canonical-candidates",
                "candidate set must be sorted by unique registration ID",
            ));
        }
        let eligible: Vec<_> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.is_eligible())
            .collect();
        let propensity_total = eligible.iter().try_fold(0_u64, |total, candidate| {
            total
                .checked_add(u64::from(candidate.selection_propensity.get()))
                .ok_or_else(|| {
                    ContractError::new(
                        "selection.propensity-overflow",
                        "eligible propensity total overflowed u64",
                    )
                })
        })?;
        let mut ranks: Vec<_> = eligible
            .iter()
            .filter_map(|candidate| candidate.rank)
            .collect();
        ranks.sort_unstable();
        if ranks.iter().copied().ne(1..=ranks.len() as u64) {
            return Err(ContractError::new(
                "selection.rank-sequence",
                "eligible candidate ranks must be unique and contiguous from one",
            ));
        }

        match &self.outcome {
            SelectionOutcome::Selected {
                selected,
                fallback_plan,
                rationale,
                selected_propensity,
                sampling,
                ..
            } => {
                selected.validate()?;
                rationale.validate()?;
                if propensity_total != 1_000_000 {
                    return Err(ContractError::new(
                        "selection.propensity-total",
                        "selected decisions require eligible propensities to total 1,000,000",
                    ));
                }
                let selected_candidate = eligible
                    .iter()
                    .find(|candidate| {
                        candidate.candidate.registration_id == selected.registration_id
                    })
                    .ok_or_else(|| {
                        ContractError::new(
                            "selection.selected-not-eligible",
                            "selected registration is absent from the eligible candidate set",
                        )
                    })?;
                if selected_candidate.candidate != *selected
                    || selected_candidate.selection_propensity != *selected_propensity
                {
                    return Err(ContractError::new(
                        "selection.selected-mismatch",
                        "selected reference or propensity differs from its candidate record",
                    ));
                }
                let selected_score = selected_candidate
                    .score
                    .as_ref()
                    .expect("eligible candidates have scores");
                if rationale.decisive_components.iter().any(|component_id| {
                    !selected_score
                        .components
                        .iter()
                        .any(|component| component.component_id == *component_id)
                }) {
                    return Err(ContractError::new(
                        "selection.unknown-decisive-component",
                        "every decisive component must belong to the selected candidate score",
                    ));
                }
                if selected_propensity.get() == 0 {
                    return Err(ContractError::new(
                        "selection.zero-selected-propensity",
                        "the selected candidate must have non-zero selection propensity",
                    ));
                }
                if let Some(sampling) = sampling {
                    sampling.validate()?;
                } else if *selected_propensity != ProbabilityPpm::ONE
                    || eligible.iter().any(|candidate| {
                        candidate.candidate.registration_id != selected.registration_id
                            && candidate.selection_propensity != ProbabilityPpm::ZERO
                    })
                {
                    return Err(ContractError::new(
                        "selection.missing-sampling-provenance",
                        "a non-deterministic propensity distribution requires sampling provenance",
                    ));
                }
                if rationale.exploration_changed_winner && sampling.is_none() {
                    return Err(ContractError::new(
                        "selection.exploration-without-sampling",
                        "exploration cannot change the winner without sampling provenance",
                    ));
                }
                let mut expected_fallbacks: Vec<_> = eligible
                    .iter()
                    .filter(|candidate| {
                        candidate.candidate.registration_id != selected.registration_id
                    })
                    .map(|candidate| {
                        (
                            candidate.rank.expect("eligible candidates have ranks"),
                            &candidate.candidate,
                        )
                    })
                    .collect();
                expected_fallbacks.sort_by_key(|(rank, _)| *rank);
                if fallback_plan.len() != expected_fallbacks.len() {
                    return Err(ContractError::new(
                        "selection.incomplete-fallback-plan",
                        "fallback plan must contain every remaining eligible candidate",
                    ));
                }
                for (fallback, (_, expected)) in fallback_plan.iter().zip(expected_fallbacks) {
                    fallback.validate()?;
                    if fallback.candidate != *expected {
                        return Err(ContractError::new(
                            "selection.fallback-order",
                            "fallback plan must follow eligible candidate selection order",
                        ));
                    }
                }
            }
            SelectionOutcome::NoEligibleCandidate { diagnostics } => {
                if !eligible.is_empty() {
                    return Err(ContractError::new(
                        "selection.no-eligible-with-eligible-candidate",
                        "NoEligibleCandidate cannot contain an eligible candidate",
                    ));
                }
                if diagnostics.is_empty() || diagnostics.windows(2).any(|pair| pair[0] >= pair[1]) {
                    return Err(ContractError::new(
                        "selection.non-canonical-diagnostics",
                        "NoEligibleCandidate diagnostics must be non-empty, sorted, and unique",
                    ));
                }
                let expected = expected_exclusion_summaries(&self.candidates)?;
                if diagnostics != &expected {
                    return Err(ContractError::new(
                        "selection.incomplete-diagnostics",
                        "NoEligibleCandidate diagnostics must summarize every excluded candidate exactly once per code",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn validate_against_objective(
        &self,
        objective: &Objective,
    ) -> Result<(), ContractError> {
        if self.input_refs.objective_digest != objective.content_digest() {
            return Err(ContractError::new(
                "selection.objective-digest-mismatch",
                "selection decision does not reference the supplied objective",
            ));
        }
        for candidate in &self.candidates {
            if let Some(score) = &candidate.score {
                score.validate_with_preferences(objective.preferences())?;
            }
        }
        let mut ranked: Vec<_> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.is_eligible())
            .collect();
        ranked.sort_by_key(|candidate| candidate.rank.expect("eligible candidates have ranks"));
        for pair in ranked.windows(2) {
            let left = pair[0];
            let right = pair[1];
            let left_utility = left
                .score
                .as_ref()
                .expect("eligible candidates have scores")
                .weighted_utility_numerator;
            let right_utility = right
                .score
                .as_ref()
                .expect("eligible candidates have scores")
                .weighted_utility_numerator;
            if left_utility < right_utility {
                return Err(ContractError::new(
                    "selection.rank-utility-order",
                    "candidate rank must descend by weighted utility",
                ));
            }
            if left_utility == right_utility
                && matches!(
                    objective.tie_break(),
                    TieBreakPolicy::AscendingRegistrationId
                )
                && left.candidate.registration_id > right.candidate.registration_id
            {
                return Err(ContractError::new(
                    "selection.rank-tie-break",
                    "equal utility must follow the objective's registration-ID tie-break",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct SelectionFingerprint<'a> {
    schema_version: SchemaVersion,
    task_id: &'a TaskId,
    external_session: Option<&'a ExternalSessionRef>,
    selector: &'a SelectorRef,
    requested_contract: &'a TopologyContractKey,
    input_refs: &'a SelectionInputRefs,
    candidates: &'a [CandidateConsideration],
    outcome: &'a SelectionOutcome,
}

impl CanonicalRecord for SelectionDecision {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        let expected = content_digest(&self.fingerprint_view())?;
        if expected == self.fingerprint {
            Ok(())
        } else {
            Err(ContractError::new(
                "identity.digest-mismatch",
                "selection fingerprint does not match its canonical content",
            ))
        }
    }
}

fn expected_exclusion_summaries(
    candidates: &[CandidateConsideration],
) -> Result<Vec<ExclusionSummary>, ContractError> {
    if candidates.is_empty() {
        return Ok(vec![ExclusionSummary::new(
            ExclusionPhase::Mechanical,
            ExclusionCode::new("catalog.empty")?,
            0,
        )?]);
    }

    let mut candidates_by_code: BTreeMap<
        (ExclusionPhase, ExclusionCode),
        BTreeSet<RegistrationId>,
    > = BTreeMap::new();
    for candidate in candidates {
        let reasons = match (
            &candidate.mechanical_admission,
            &candidate.policy_eligibility,
        ) {
            (AdmissionResult::Excluded { reasons }, _) => reasons,
            (AdmissionResult::Admitted, PolicyEligibility::Excluded { reasons }) => reasons,
            _ => {
                return Err(ContractError::new(
                    "selection.incomplete-diagnostics",
                    "NoEligibleCandidate contains a candidate without exclusion reasons",
                ));
            }
        };
        let keys: BTreeSet<_> = reasons
            .iter()
            .map(|reason| (reason.phase, reason.code.clone()))
            .collect();
        for key in keys {
            candidates_by_code
                .entry(key)
                .or_default()
                .insert(candidate.candidate.registration_id.clone());
        }
    }

    candidates_by_code
        .into_iter()
        .map(|((phase, code), registrations)| {
            let count = u64::try_from(registrations.len()).map_err(|_| {
                ContractError::new(
                    "selection.exclusion-count-overflow",
                    "excluded candidate count exceeds u64",
                )
            })?;
            ExclusionSummary::new(phase, code, count)
        })
        .collect()
}

/// One append-only live-admission evaluation recorded before any [`super::RunId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionEvaluation {
    schema_version: SchemaVersion,
    attempt_id: AdmissionAttemptId,
    decision_id: SelectionDecisionId,
    candidate: RegistrationSelectionRef,
    catalog_snapshot_digest: ContentDigest,
    availability_snapshot_id: AvailabilitySnapshotId,
    availability_snapshot_digest: ContentDigest,
    result: AdmissionResult,
    recorded_at: super::RecordedAt,
    content_digest: ContentDigest,
}

impl AdmissionEvaluation {
    /// Creates one live readmission event without allocating a run identity.
    #[expect(
        clippy::too_many_arguments,
        reason = "the admission event binds all immutable snapshots"
    )]
    pub fn new(
        attempt_id: AdmissionAttemptId,
        decision_id: SelectionDecisionId,
        candidate: RegistrationSelectionRef,
        catalog_snapshot_digest: ContentDigest,
        availability_snapshot_id: AvailabilitySnapshotId,
        availability_snapshot_digest: ContentDigest,
        result: AdmissionResult,
        recorded_at: super::RecordedAt,
    ) -> Result<Self, ContractError> {
        candidate.validate()?;
        result.validate_for(ExclusionPhase::LiveReadmission)?;
        let mut evaluation = Self {
            schema_version: SchemaVersion::V1,
            attempt_id,
            decision_id,
            candidate,
            catalog_snapshot_digest,
            availability_snapshot_id,
            availability_snapshot_digest,
            result,
            recorded_at,
            content_digest: ContentDigest::sha256(&[]),
        };
        evaluation.content_digest = content_digest(&evaluation.content_view())?;
        Ok(evaluation)
    }

    /// Returns the admission-attempt identity.
    #[must_use]
    pub const fn attempt_id(&self) -> AdmissionAttemptId {
        self.attempt_id
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

    /// Returns the candidate that was readmitted.
    #[must_use]
    pub const fn candidate(&self) -> &RegistrationSelectionRef {
        &self.candidate
    }

    /// Returns the captured catalog content identity.
    #[must_use]
    pub const fn catalog_snapshot_digest(&self) -> ContentDigest {
        self.catalog_snapshot_digest
    }

    /// Returns the live availability observation identity.
    #[must_use]
    pub const fn availability_snapshot_id(&self) -> AvailabilitySnapshotId {
        self.availability_snapshot_id
    }

    /// Returns the live availability content identity.
    #[must_use]
    pub const fn availability_snapshot_digest(&self) -> ContentDigest {
        self.availability_snapshot_digest
    }

    /// Returns the live admission result.
    #[must_use]
    pub const fn result(&self) -> &AdmissionResult {
        &self.result
    }

    /// Returns the store-assigned recording position.
    #[must_use]
    pub const fn recorded_at(&self) -> super::RecordedAt {
        self.recorded_at
    }

    /// Returns whether live admission succeeded.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        self.result.is_admitted()
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn content_view(&self) -> AdmissionContent<'_> {
        AdmissionContent {
            schema_version: self.schema_version,
            attempt_id: self.attempt_id,
            decision_id: self.decision_id,
            candidate: &self.candidate,
            catalog_snapshot_digest: self.catalog_snapshot_digest,
            availability_snapshot_id: self.availability_snapshot_id,
            availability_snapshot_digest: self.availability_snapshot_digest,
            result: &self.result,
            sequence: self.recorded_at.sequence(),
        }
    }
}

#[derive(Serialize)]
struct AdmissionContent<'a> {
    schema_version: SchemaVersion,
    attempt_id: AdmissionAttemptId,
    decision_id: SelectionDecisionId,
    candidate: &'a RegistrationSelectionRef,
    catalog_snapshot_digest: ContentDigest,
    availability_snapshot_id: AvailabilitySnapshotId,
    availability_snapshot_digest: ContentDigest,
    result: &'a AdmissionResult,
    sequence: super::LogicalSequence,
}

impl CanonicalRecord for AdmissionEvaluation {
    fn validate(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.candidate.validate()?;
        self.result.validate_for(ExclusionPhase::LiveReadmission)?;
        let expected = content_digest(&self.content_view())?;
        if expected == self.content_digest {
            Ok(())
        } else {
            Err(ContractError::new(
                "identity.digest-mismatch",
                "admission event digest does not match its canonical content",
            ))
        }
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

pub(crate) fn validate_measurement(
    id: &MeasurementId,
    measurement: &Measurement,
) -> Result<(), ContractError> {
    measurement.validate()?;
    let expected = match id.as_str() {
        "verified-success" | "objective-coverage" | "quality" | "rework-required" => {
            Some("proportion")
        }
        "residual-count" | "human-intervention-count" => Some("count"),
        "severity-fold" => Some("ordinal"),
        "end-to-end-latency" => Some("duration"),
        "token-cost" => Some("tokens"),
        "monetary-cost" => Some("money"),
        _ => None,
    };
    let actual = match measurement.value() {
        MeasurementValue::Proportion(_) => "proportion",
        MeasurementValue::Count(_) => "count",
        MeasurementValue::Duration(_) => "duration",
        MeasurementValue::Tokens(_) => "tokens",
        MeasurementValue::Money(_) => "money",
        MeasurementValue::Ordinal { .. } => "ordinal",
    };
    if expected.is_some_and(|expected| expected != actual) {
        return Err(ContractError::new(
            "measurement.registry-type",
            "known measurement registry entry has the wrong value type",
        ));
    }
    if id.as_str() == "quality" && measurement.calibration().is_none() {
        return Err(ContractError::new(
            "measurement.quality-calibration",
            "quality measurements require a named calibration policy",
        ));
    }
    Ok(())
}
