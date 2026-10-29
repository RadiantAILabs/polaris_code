//! Immutable, secret-free runtime-availability observations used by selection.

use serde::{Deserialize, Serialize};

use super::encoding::content_digest;
use super::evidence::{ProvenanceContent, canonical_provenance};
use super::profile::validate_strictly_sorted;
use super::{
    AvailabilitySnapshotId, BindingAlias, BindingKindId, CanonicalRecord, CapabilityKey,
    ContentDigest, ContextTypeKey, ContractError, DurationMs, EnvironmentKey, EvidenceTime,
    PolicyLabelId, Provenance, ProviderKey, RuntimeMechanismKey, SchemaVersion,
};

/// Availability of a resource, output, provider, capability, mechanism, or binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AvailabilityState {
    /// The subject was observed available.
    Available,
    /// The subject was observed unavailable.
    Unavailable,
    /// The observation could not establish availability.
    Unknown,
}

/// Native access mode retained for a resource requirement explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceAccess {
    /// Shared read access.
    Read,
    /// Exclusive mutable access.
    Write,
}

/// Common observation metadata for one availability fact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AvailabilityObservation {
    state: AvailabilityState,
    provider: Option<ProviderKey>,
    freshness_bound: Option<DurationMs>,
    provenance: Vec<Provenance>,
}

impl AvailabilityObservation {
    /// Creates attributed availability metadata.
    pub fn new(
        state: AvailabilityState,
        provider: Option<ProviderKey>,
        freshness_bound: Option<DurationMs>,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        let value = Self {
            state,
            provider,
            freshness_bound,
            provenance: canonical_provenance(provenance)?,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the observed state.
    #[must_use]
    pub const fn state(&self) -> AvailabilityState {
        self.state
    }

    /// Returns the provider attributed to the fact, when applicable.
    #[must_use]
    pub const fn provider(&self) -> Option<&ProviderKey> {
        self.provider.as_ref()
    }

    /// Returns the explicit freshness bound, when supplied.
    #[must_use]
    pub const fn freshness_bound(&self) -> Option<DurationMs> {
        self.freshness_bound
    }

    /// Returns observation provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_provenance(&self.provenance)
    }

    fn content_view(&self) -> AvailabilityObservationContent<'_> {
        AvailabilityObservationContent {
            state: self.state,
            provider: self.provider.as_ref(),
            freshness_bound: self.freshness_bound,
            provenance: provenance_content(&self.provenance),
        }
    }
}

#[derive(Serialize)]
struct AvailabilityObservationContent<'a> {
    state: AvailabilityState,
    provider: Option<&'a ProviderKey>,
    freshness_bound: Option<DurationMs>,
    provenance: Vec<ProvenanceContent<'a>>,
}

/// Resource presence and native access available to one context type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceAvailability {
    type_key: ContextTypeKey,
    access: ResourceAccess,
    observation: AvailabilityObservation,
}

impl ResourceAvailability {
    /// Creates a resource availability fact.
    pub fn new(
        type_key: ContextTypeKey,
        access: ResourceAccess,
        observation: AvailabilityObservation,
    ) -> Result<Self, ContractError> {
        observation.validate()?;
        Ok(Self {
            type_key,
            access,
            observation,
        })
    }

    /// Returns the stable explanation key.
    #[must_use]
    pub const fn type_key(&self) -> &ContextTypeKey {
        &self.type_key
    }

    /// Returns native read or write access.
    #[must_use]
    pub const fn access(&self) -> ResourceAccess {
        self.access
    }

    /// Returns the attributed observation.
    #[must_use]
    pub const fn observation(&self) -> &AvailabilityObservation {
        &self.observation
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.observation.validate()
    }

    fn content_view(&self) -> ResourceAvailabilityContent<'_> {
        ResourceAvailabilityContent {
            type_key: &self.type_key,
            access: self.access,
            observation: self.observation.content_view(),
        }
    }
}

#[derive(Serialize)]
struct ResourceAvailabilityContent<'a> {
    type_key: &'a ContextTypeKey,
    access: ResourceAccess,
    observation: AvailabilityObservationContent<'a>,
}

/// Upstream graph output available under one stable native-type explanation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutputAvailability {
    type_key: ContextTypeKey,
    observation: AvailabilityObservation,
}

impl OutputAvailability {
    /// Creates an upstream-output availability fact.
    pub fn new(
        type_key: ContextTypeKey,
        observation: AvailabilityObservation,
    ) -> Result<Self, ContractError> {
        observation.validate()?;
        Ok(Self {
            type_key,
            observation,
        })
    }

    /// Returns the stable output type explanation.
    #[must_use]
    pub const fn type_key(&self) -> &ContextTypeKey {
        &self.type_key
    }

    /// Returns the attributed observation.
    #[must_use]
    pub const fn observation(&self) -> &AvailabilityObservation {
        &self.observation
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.observation.validate()
    }

    fn content_view(&self) -> OutputAvailabilityContent<'_> {
        OutputAvailabilityContent {
            type_key: &self.type_key,
            observation: self.observation.content_view(),
        }
    }
}

#[derive(Serialize)]
struct OutputAvailabilityContent<'a> {
    type_key: &'a ContextTypeKey,
    observation: AvailabilityObservationContent<'a>,
}

/// One provider implementation and deployment configuration observation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProviderAvailability {
    provider: ProviderKey,
    provider_configuration_digest: ContentDigest,
    observation: AvailabilityObservation,
}

impl ProviderAvailability {
    /// Creates a provider availability fact without credentials or handles.
    pub fn new(
        provider: ProviderKey,
        provider_configuration_digest: ContentDigest,
        observation: AvailabilityObservation,
    ) -> Result<Self, ContractError> {
        let value = Self {
            provider,
            provider_configuration_digest,
            observation,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the stable provider implementation key.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Returns the secret-free provider-configuration digest.
    #[must_use]
    pub const fn provider_configuration_digest(&self) -> ContentDigest {
        self.provider_configuration_digest
    }

    /// Returns the attributed observation.
    #[must_use]
    pub const fn observation(&self) -> &AvailabilityObservation {
        &self.observation
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.observation.validate()?;
        if self.observation.provider() == Some(&self.provider) {
            Ok(())
        } else {
            Err(ContractError::new(
                "availability.provider-attribution-mismatch",
                "provider availability must be attributed to the same provider key",
            ))
        }
    }

    fn content_view(&self) -> ProviderAvailabilityContent<'_> {
        ProviderAvailabilityContent {
            provider: &self.provider,
            provider_configuration_digest: self.provider_configuration_digest,
            observation: self.observation.content_view(),
        }
    }
}

#[derive(Serialize)]
struct ProviderAvailabilityContent<'a> {
    provider: &'a ProviderKey,
    provider_configuration_digest: ContentDigest,
    observation: AvailabilityObservationContent<'a>,
}

/// One semantic capability availability observation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CapabilityAvailability {
    capability: CapabilityKey,
    observation: AvailabilityObservation,
}

impl CapabilityAvailability {
    /// Creates a semantic capability availability fact.
    pub fn new(
        capability: CapabilityKey,
        observation: AvailabilityObservation,
    ) -> Result<Self, ContractError> {
        observation.validate()?;
        Ok(Self {
            capability,
            observation,
        })
    }

    /// Returns the stable capability key.
    #[must_use]
    pub const fn capability(&self) -> &CapabilityKey {
        &self.capability
    }

    /// Returns the attributed observation.
    #[must_use]
    pub const fn observation(&self) -> &AvailabilityObservation {
        &self.observation
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.observation.validate()
    }

    fn content_view(&self) -> CapabilityAvailabilityContent<'_> {
        CapabilityAvailabilityContent {
            capability: &self.capability,
            observation: self.observation.content_view(),
        }
    }
}

#[derive(Serialize)]
struct CapabilityAvailabilityContent<'a> {
    capability: &'a CapabilityKey,
    observation: AvailabilityObservationContent<'a>,
}

/// One implemented runtime-mechanism availability observation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MechanismAvailability {
    mechanism: RuntimeMechanismKey,
    observation: AvailabilityObservation,
}

impl MechanismAvailability {
    /// Creates a runtime-mechanism availability fact.
    pub fn new(
        mechanism: RuntimeMechanismKey,
        observation: AvailabilityObservation,
    ) -> Result<Self, ContractError> {
        observation.validate()?;
        Ok(Self {
            mechanism,
            observation,
        })
    }

    /// Returns the stable runtime-mechanism key.
    #[must_use]
    pub const fn mechanism(&self) -> &RuntimeMechanismKey {
        &self.mechanism
    }

    /// Returns the attributed observation.
    #[must_use]
    pub const fn observation(&self) -> &AvailabilityObservation {
        &self.observation
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.observation.validate()
    }

    fn content_view(&self) -> MechanismAvailabilityContent<'_> {
        MechanismAvailabilityContent {
            mechanism: &self.mechanism,
            observation: self.observation.content_view(),
        }
    }
}

#[derive(Serialize)]
struct MechanismAvailabilityContent<'a> {
    mechanism: &'a RuntimeMechanismKey,
    observation: AvailabilityObservationContent<'a>,
}

/// Secret-free host binding observation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BindingAvailability {
    alias: BindingAlias,
    kind: BindingKindId,
    provider: ProviderKey,
    observation: AvailabilityObservation,
}

impl BindingAvailability {
    /// Creates a binding observation without resolving or storing its secret value.
    pub fn new(
        alias: BindingAlias,
        kind: BindingKindId,
        provider: ProviderKey,
        observation: AvailabilityObservation,
    ) -> Result<Self, ContractError> {
        let value = Self {
            alias,
            kind,
            provider,
            observation,
        };
        value.validate()?;
        Ok(value)
    }

    /// Returns the non-secret binding alias.
    #[must_use]
    pub const fn alias(&self) -> &BindingAlias {
        &self.alias
    }

    /// Returns the extensible binding category.
    #[must_use]
    pub const fn kind(&self) -> &BindingKindId {
        &self.kind
    }

    /// Returns the provider expected to resolve the binding.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Returns the attributed observation.
    #[must_use]
    pub const fn observation(&self) -> &AvailabilityObservation {
        &self.observation
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.observation.validate()?;
        if self.observation.provider() == Some(&self.provider) {
            Ok(())
        } else {
            Err(ContractError::new(
                "availability.binding-provider-mismatch",
                "binding availability must be attributed to its declared provider key",
            ))
        }
    }

    fn content_view(&self) -> BindingAvailabilityContent<'_> {
        BindingAvailabilityContent {
            alias: &self.alias,
            kind: &self.kind,
            provider: &self.provider,
            observation: self.observation.content_view(),
        }
    }
}

#[derive(Serialize)]
struct BindingAvailabilityContent<'a> {
    alias: &'a BindingAlias,
    kind: &'a BindingKindId,
    provider: &'a ProviderKey,
    observation: AvailabilityObservationContent<'a>,
}

/// Immutable, explainable selection input that never contains live resource values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAvailabilitySnapshot {
    schema_version: SchemaVersion,
    snapshot_id: AvailabilitySnapshotId,
    content_digest: ContentDigest,
    captured_at: EvidenceTime,
    environment: EnvironmentKey,
    resources: Vec<ResourceAvailability>,
    upstream_outputs: Vec<OutputAvailability>,
    providers: Vec<ProviderAvailability>,
    capabilities: Vec<CapabilityAvailability>,
    mechanisms: Vec<MechanismAvailability>,
    bindings: Vec<BindingAvailability>,
    policy_labels: Vec<PolicyLabelId>,
    provenance: Vec<Provenance>,
}

impl RuntimeAvailabilitySnapshot {
    /// Creates a canonical availability observation and computes its stable content identity.
    #[expect(
        clippy::too_many_arguments,
        reason = "the snapshot has seven fact families"
    )]
    pub fn new(
        snapshot_id: AvailabilitySnapshotId,
        captured_at: EvidenceTime,
        environment: EnvironmentKey,
        mut resources: Vec<ResourceAvailability>,
        mut upstream_outputs: Vec<OutputAvailability>,
        mut providers: Vec<ProviderAvailability>,
        mut capabilities: Vec<CapabilityAvailability>,
        mut mechanisms: Vec<MechanismAvailability>,
        mut bindings: Vec<BindingAvailability>,
        mut policy_labels: Vec<PolicyLabelId>,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        sort_and_reject_duplicate_subjects(&mut resources, |left, right| {
            (&left.type_key, left.access).cmp(&(&right.type_key, right.access))
        })?;
        sort_and_reject_duplicate_subjects(&mut upstream_outputs, |left, right| {
            left.type_key.cmp(&right.type_key)
        })?;
        sort_and_reject_duplicate_subjects(&mut providers, |left, right| {
            left.provider.cmp(&right.provider)
        })?;
        sort_and_reject_duplicate_subjects(&mut capabilities, |left, right| {
            left.capability.cmp(&right.capability)
        })?;
        sort_and_reject_duplicate_subjects(&mut mechanisms, |left, right| {
            left.mechanism.cmp(&right.mechanism)
        })?;
        sort_and_reject_duplicate_subjects(&mut bindings, |left, right| {
            left.alias.cmp(&right.alias)
        })?;
        policy_labels.sort();
        policy_labels.dedup();
        let mut snapshot = Self {
            schema_version: SchemaVersion::V1,
            snapshot_id,
            content_digest: ContentDigest::sha256(&[]),
            captured_at,
            environment,
            resources,
            upstream_outputs,
            providers,
            capabilities,
            mechanisms,
            bindings,
            policy_labels,
            provenance: canonical_provenance(provenance)?,
        };
        snapshot.validate_structure()?;
        snapshot.content_digest = content_digest(&snapshot.content_view())?;
        Ok(snapshot)
    }

    /// Returns the distinct observation event identity.
    #[must_use]
    pub const fn snapshot_id(&self) -> AvailabilitySnapshotId {
        self.snapshot_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the content identity, which omits event ID and observation times.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    /// Returns the observation time, which does not affect content identity.
    #[must_use]
    pub const fn captured_at(&self) -> &EvidenceTime {
        &self.captured_at
    }

    /// Returns the execution-environment key.
    #[must_use]
    pub const fn environment(&self) -> &EnvironmentKey {
        &self.environment
    }

    /// Returns resource facts in canonical order.
    #[must_use]
    pub fn resources(&self) -> &[ResourceAvailability] {
        &self.resources
    }

    /// Returns upstream-output facts in canonical order.
    #[must_use]
    pub fn upstream_outputs(&self) -> &[OutputAvailability] {
        &self.upstream_outputs
    }

    /// Returns provider facts in canonical order.
    #[must_use]
    pub fn providers(&self) -> &[ProviderAvailability] {
        &self.providers
    }

    /// Returns semantic capability facts in canonical order.
    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityAvailability] {
        &self.capabilities
    }

    /// Returns implemented runtime-mechanism facts in canonical order.
    #[must_use]
    pub fn mechanisms(&self) -> &[MechanismAvailability] {
        &self.mechanisms
    }

    /// Returns secret-free binding facts in canonical order.
    #[must_use]
    pub fn bindings(&self) -> &[BindingAvailability] {
        &self.bindings
    }

    /// Returns extensible policy labels in canonical order.
    #[must_use]
    pub fn policy_labels(&self) -> &[PolicyLabelId] {
        &self.policy_labels
    }

    /// Returns snapshot provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        validate_strictly_sorted("availability.resources", &self.resources)?;
        validate_unique_adjacent(&self.resources, |left, right| {
            (&left.type_key, left.access).cmp(&(&right.type_key, right.access))
        })?;
        validate_strictly_sorted("availability.outputs", &self.upstream_outputs)?;
        validate_unique_adjacent(&self.upstream_outputs, |left, right| {
            left.type_key.cmp(&right.type_key)
        })?;
        validate_strictly_sorted("availability.providers", &self.providers)?;
        validate_unique_adjacent(&self.providers, |left, right| {
            left.provider.cmp(&right.provider)
        })?;
        validate_strictly_sorted("availability.capabilities", &self.capabilities)?;
        validate_unique_adjacent(&self.capabilities, |left, right| {
            left.capability.cmp(&right.capability)
        })?;
        validate_strictly_sorted("availability.mechanisms", &self.mechanisms)?;
        validate_unique_adjacent(&self.mechanisms, |left, right| {
            left.mechanism.cmp(&right.mechanism)
        })?;
        validate_strictly_sorted("availability.bindings", &self.bindings)?;
        validate_unique_adjacent(&self.bindings, |left, right| left.alias.cmp(&right.alias))?;
        validate_strictly_sorted("availability.policy-labels", &self.policy_labels)?;
        validate_provenance(&self.provenance)?;
        for value in &self.resources {
            value.validate()?;
        }
        for value in &self.upstream_outputs {
            value.validate()?;
        }
        for value in &self.providers {
            value.validate()?;
        }
        for value in &self.capabilities {
            value.validate()?;
        }
        for value in &self.mechanisms {
            value.validate()?;
        }
        for value in &self.bindings {
            value.validate()?;
        }
        Ok(())
    }

    fn content_view(&self) -> RuntimeAvailabilityContent<'_> {
        RuntimeAvailabilityContent {
            schema_version: self.schema_version,
            environment: &self.environment,
            resources: self
                .resources
                .iter()
                .map(ResourceAvailability::content_view)
                .collect(),
            upstream_outputs: self
                .upstream_outputs
                .iter()
                .map(OutputAvailability::content_view)
                .collect(),
            providers: self
                .providers
                .iter()
                .map(ProviderAvailability::content_view)
                .collect(),
            capabilities: self
                .capabilities
                .iter()
                .map(CapabilityAvailability::content_view)
                .collect(),
            mechanisms: self
                .mechanisms
                .iter()
                .map(MechanismAvailability::content_view)
                .collect(),
            bindings: self
                .bindings
                .iter()
                .map(BindingAvailability::content_view)
                .collect(),
            policy_labels: &self.policy_labels,
            provenance: provenance_content(&self.provenance),
        }
    }
}

#[derive(Serialize)]
struct RuntimeAvailabilityContent<'a> {
    schema_version: SchemaVersion,
    environment: &'a EnvironmentKey,
    resources: Vec<ResourceAvailabilityContent<'a>>,
    upstream_outputs: Vec<OutputAvailabilityContent<'a>>,
    providers: Vec<ProviderAvailabilityContent<'a>>,
    capabilities: Vec<CapabilityAvailabilityContent<'a>>,
    mechanisms: Vec<MechanismAvailabilityContent<'a>>,
    bindings: Vec<BindingAvailabilityContent<'a>>,
    policy_labels: &'a [PolicyLabelId],
    provenance: Vec<ProvenanceContent<'a>>,
}

impl CanonicalRecord for RuntimeAvailabilitySnapshot {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        let expected = content_digest(&self.content_view())?;
        if expected == self.content_digest {
            Ok(())
        } else {
            Err(ContractError::new(
                "identity.digest-mismatch",
                "availability snapshot digest does not match its canonical facts",
            ))
        }
    }
}

fn sort_and_reject_duplicate_subjects<T>(
    values: &mut [T],
    compare: impl Fn(&T, &T) -> std::cmp::Ordering,
) -> Result<(), ContractError> {
    values.sort_by(|left, right| compare(left, right));
    validate_unique_adjacent(values, compare)
}

fn validate_unique_adjacent<T>(
    values: &[T],
    compare: impl Fn(&T, &T) -> std::cmp::Ordering,
) -> Result<(), ContractError> {
    if values
        .windows(2)
        .any(|pair| compare(&pair[0], &pair[1]).is_eq())
    {
        Err(ContractError::new(
            "availability.duplicate-subject",
            "each availability subject may appear exactly once in its fact family",
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
