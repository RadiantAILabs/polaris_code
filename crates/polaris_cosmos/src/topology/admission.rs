//! Explainable topology admission over native Polaris validation.
//!
//! This module projects native type requirements into stable keys for persisted
//! diagnostics. It never uses those keys to satisfy a live requirement:
//! [`GraphExecutor::validate_resources`] remains authoritative immediately
//! before execution.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use polaris::graph::{Graph, GraphExecutor, GraphSignature, ResourceValidationError};
use polaris::system::param::{AccessMode, SystemContext};
use serde::{Deserialize, Serialize};

use crate::contracts::{
    AdmissionAttemptId, AdmissionEvaluation, AdmissionResult, AvailabilityState, BindingAlias,
    BindingKindId, CapabilityKey, ConfigurationSchemaKey, ContentDigest, ContextTypeKey,
    ContractError, ExclusionCode, ExclusionPhase, ExclusionReason, ProviderKey, RecordSubjectId,
    RecordedAt, RegistrationSelectionRef, ResourceAccess, RuntimeAvailabilitySnapshot,
    RuntimeMechanismKey, SelectionDecisionId,
};

use super::{
    RegisteredTopology, RegistrationId, TopologyCandidate, TopologyCatalog, TopologyCatalogError,
    TopologyCatalogSnapshot, TopologyContract, TopologyContractKey,
};

/// Native signature channel associated with a stable type explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SignatureChannel {
    /// A local resource boundary (`Res<T>` or `ResMut<T>`).
    Resource,
    /// An ephemeral graph-output boundary (`Out<T>`).
    Output,
}

/// Error while assembling trustworthy admission inputs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AdmissionBuildError {
    /// One native type was assigned two stable keys in the same channel.
    NativeTypeRebound {
        /// Channel containing the conflicting native type.
        channel: SignatureChannel,
    },
    /// One stable key was assigned to two native types in the same channel.
    StableKeyRebound {
        /// Channel containing the conflicting key.
        channel: SignatureChannel,
        /// Conflicting stable key.
        key: ContextTypeKey,
    },
    /// Candidate metadata did not cover exactly the captured catalog candidates.
    IncompleteCandidateSet,
    /// A partial registration did not carry complete catalog-owned admission metadata.
    IncompleteRegistration {
        /// Registration that cannot participate in generic preflight.
        registration: RegistrationId,
    },
    /// A contract record constructor rejected invalid input.
    Contract(ContractError),
}

impl fmt::Display for AdmissionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeTypeRebound { channel } => {
                write!(
                    formatter,
                    "native {channel:?} type already has another stable key"
                )
            }
            Self::StableKeyRebound { channel, key } => {
                write!(
                    formatter,
                    "stable {channel:?} key {key} already describes another type"
                )
            }
            Self::IncompleteCandidateSet => formatter.write_str(
                "admission inputs must cover every captured catalog candidate exactly once",
            ),
            Self::IncompleteRegistration { registration } => write!(
                formatter,
                "topology registration {registration} has no complete admission metadata"
            ),
            Self::Contract(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AdmissionBuildError {}

impl From<ContractError> for AdmissionBuildError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

/// Process-local bijection between native boundary types and stable explanations.
///
/// The registry retains [`TypeId`] only in memory. Persisted admission records
/// contain the corresponding [`ContextTypeKey`] and never a rendered Rust name.
#[derive(Debug, Clone, Default)]
pub struct TypeExplanationRegistry {
    resources: BTreeMap<TypeId, ContextTypeKey>,
    resource_keys: BTreeMap<ContextTypeKey, TypeId>,
    outputs: BTreeMap<TypeId, ContextTypeKey>,
    output_keys: BTreeMap<ContextTypeKey, TypeId>,
}

impl TypeExplanationRegistry {
    /// Creates an empty process-local registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds a native resource type to one stable key.
    pub fn bind_resource<T: 'static>(
        &mut self,
        key: ContextTypeKey,
    ) -> Result<(), AdmissionBuildError> {
        self.bind(SignatureChannel::Resource, TypeId::of::<T>(), key)
    }

    /// Binds a native output type to one stable key.
    pub fn bind_output<T: 'static>(
        &mut self,
        key: ContextTypeKey,
    ) -> Result<(), AdmissionBuildError> {
        self.bind(SignatureChannel::Output, TypeId::of::<T>(), key)
    }

    fn bind(
        &mut self,
        channel: SignatureChannel,
        native: TypeId,
        key: ContextTypeKey,
    ) -> Result<(), AdmissionBuildError> {
        let (by_native, by_key) = match channel {
            SignatureChannel::Resource => (&mut self.resources, &mut self.resource_keys),
            SignatureChannel::Output => (&mut self.outputs, &mut self.output_keys),
        };
        if let Some(existing) = by_native.get(&native) {
            return if existing == &key {
                Ok(())
            } else {
                Err(AdmissionBuildError::NativeTypeRebound { channel })
            };
        }
        if let Some(existing) = by_key.get(&key) {
            return if existing == &native {
                Ok(())
            } else {
                Err(AdmissionBuildError::StableKeyRebound { channel, key })
            };
        }
        by_native.insert(native, key.clone());
        by_key.insert(key, native);
        Ok(())
    }

    fn key(&self, channel: SignatureChannel, native: TypeId) -> Option<&ContextTypeKey> {
        match channel {
            SignatureChannel::Resource => self.resources.get(&native),
            SignatureChannel::Output => self.outputs.get(&native),
        }
    }

    pub(crate) fn merged_with(&self, other: &Self) -> Result<Self, AdmissionBuildError> {
        let mut merged = self.clone();
        for (native, key) in &other.resources {
            merged.bind(SignatureChannel::Resource, *native, key.clone())?;
        }
        for (native, key) in &other.outputs {
            merged.bind(SignatureChannel::Output, *native, key.clone())?;
        }
        Ok(merged)
    }
}

/// Stable resource requirement projected from a native graph signature.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StableResourceRequirement {
    type_key: ContextTypeKey,
    access: ResourceAccess,
}

impl StableResourceRequirement {
    /// Returns the stable type key.
    #[must_use]
    pub const fn type_key(&self) -> &ContextTypeKey {
        &self.type_key
    }

    /// Returns the native read/write requirement.
    #[must_use]
    pub const fn access(&self) -> ResourceAccess {
        self.access
    }
}

/// Persistable explanation derived from a native [`GraphSignature`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableSignatureExplanation {
    required_resources: Vec<StableResourceRequirement>,
    required_outputs: Vec<ContextTypeKey>,
    produced_outputs: Vec<ContextTypeKey>,
}

impl StableSignatureExplanation {
    /// Projects every native signature boundary through the supplied registry.
    ///
    /// Missing bindings are returned together so callers can report complete
    /// `type-explanation.missing` diagnostics.
    pub fn derive(
        signature: &GraphSignature,
        registry: &TypeExplanationRegistry,
    ) -> Result<Self, Vec<UnexplainedBoundary>> {
        let mut missing = Vec::new();
        let mut required_resources = Vec::new();
        let mut required_outputs = Vec::new();
        let mut produced_outputs = Vec::new();

        for access in signature.requires() {
            if let Some(key) = registry.key(SignatureChannel::Resource, access.type_id) {
                required_resources.push(StableResourceRequirement {
                    type_key: key.clone(),
                    access: match access.mode {
                        AccessMode::Read => ResourceAccess::Read,
                        AccessMode::Write => ResourceAccess::Write,
                    },
                });
            } else {
                missing.push(UnexplainedBoundary::new(
                    SignatureChannel::Resource,
                    access.type_name,
                ));
            }
        }
        for access in signature.requires_outputs() {
            if let Some(key) = registry.key(SignatureChannel::Output, access.type_id) {
                required_outputs.push(key.clone());
            } else {
                missing.push(UnexplainedBoundary::new(
                    SignatureChannel::Output,
                    access.type_name,
                ));
            }
        }
        for access in signature.produces() {
            if let Some(key) = registry.key(SignatureChannel::Output, access.type_id) {
                produced_outputs.push(key.clone());
            } else {
                missing.push(UnexplainedBoundary::new(
                    SignatureChannel::Output,
                    access.type_name,
                ));
            }
        }

        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Err(missing);
        }
        required_resources.sort();
        required_resources.dedup();
        required_outputs.sort();
        required_outputs.dedup();
        produced_outputs.sort();
        produced_outputs.dedup();
        Ok(Self {
            required_resources,
            required_outputs,
            produced_outputs,
        })
    }

    /// Returns required resources in canonical order.
    #[must_use]
    pub fn required_resources(&self) -> &[StableResourceRequirement] {
        &self.required_resources
    }

    /// Returns required upstream outputs in canonical order.
    #[must_use]
    pub fn required_outputs(&self) -> &[ContextTypeKey] {
        &self.required_outputs
    }

    /// Returns produced outputs in canonical order.
    #[must_use]
    pub fn produced_outputs(&self) -> &[ContextTypeKey] {
        &self.produced_outputs
    }
}

/// Ephemeral diagnostic for a native boundary that lacks a stable explanation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnexplainedBoundary {
    channel: SignatureChannel,
    display_type: &'static str,
}

impl UnexplainedBoundary {
    fn new(channel: SignatureChannel, display_type: &'static str) -> Self {
        Self {
            channel,
            display_type,
        }
    }

    /// Returns the unexplained native channel.
    #[must_use]
    pub const fn channel(&self) -> SignatureChannel {
        self.channel
    }

    /// Returns an ephemeral display label that is never used as identity.
    #[must_use]
    pub const fn display_type(&self) -> &'static str {
        self.display_type
    }
}

/// Required provider plus its immutable, secret-free configuration identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ProviderRequirement {
    provider: ProviderKey,
    configuration_digest: ContentDigest,
}

impl ProviderRequirement {
    /// Creates a provider requirement.
    #[must_use]
    pub const fn new(provider: ProviderKey, configuration_digest: ContentDigest) -> Self {
        Self {
            provider,
            configuration_digest,
        }
    }

    /// Returns the required provider identity.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }

    /// Returns the required provider configuration identity.
    #[must_use]
    pub const fn configuration_digest(&self) -> ContentDigest {
        self.configuration_digest
    }
}

/// Required host binding without its credential or resolved value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct BindingRequirement {
    alias: BindingAlias,
    kind: BindingKindId,
    provider: ProviderKey,
}

impl BindingRequirement {
    /// Creates a secret-free binding requirement.
    #[must_use]
    pub const fn new(alias: BindingAlias, kind: BindingKindId, provider: ProviderKey) -> Self {
        Self {
            alias,
            kind,
            provider,
        }
    }

    /// Returns the secret-free binding alias.
    #[must_use]
    pub const fn alias(&self) -> &BindingAlias {
        &self.alias
    }

    /// Returns the binding kind.
    #[must_use]
    pub const fn kind(&self) -> &BindingKindId {
        &self.kind
    }

    /// Returns the provider expected to satisfy the binding.
    #[must_use]
    pub const fn provider(&self) -> &ProviderKey {
        &self.provider
    }
}

/// Stable non-typed requirements attached to one embodied registration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CandidateRequirements {
    providers: Vec<ProviderRequirement>,
    bindings: Vec<BindingRequirement>,
    capabilities: Vec<CapabilityKey>,
    mechanisms: Vec<RuntimeMechanismKey>,
}

impl CandidateRequirements {
    /// Creates an empty requirement set.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Creates a canonical requirement set.
    #[must_use]
    pub fn new(
        mut providers: Vec<ProviderRequirement>,
        mut bindings: Vec<BindingRequirement>,
        mut capabilities: Vec<CapabilityKey>,
        mut mechanisms: Vec<RuntimeMechanismKey>,
    ) -> Self {
        providers.sort();
        providers.dedup();
        bindings.sort();
        bindings.dedup();
        capabilities.sort();
        capabilities.dedup();
        mechanisms.sort();
        mechanisms.dedup();
        Self {
            providers,
            bindings,
            capabilities,
            mechanisms,
        }
    }

    /// Returns provider requirements in canonical order.
    #[must_use]
    pub fn providers(&self) -> &[ProviderRequirement] {
        &self.providers
    }

    /// Returns host-binding requirements in canonical order.
    #[must_use]
    pub fn bindings(&self) -> &[BindingRequirement] {
        &self.bindings
    }

    /// Returns semantic capability requirements in canonical order.
    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityKey] {
        &self.capabilities
    }

    /// Returns runtime-mechanism requirements in canonical order.
    #[must_use]
    pub fn mechanisms(&self) -> &[RuntimeMechanismKey] {
        &self.mechanisms
    }
}

/// One catalog candidate plus immutable admission metadata supplied at registration.
#[derive(Debug, Clone)]
pub struct AdmissionCandidate {
    native: TopologyCandidate,
    selection: RegistrationSelectionRef,
    requirements: CandidateRequirements,
}

impl AdmissionCandidate {
    /// Couples a catalog-produced candidate to its frozen selection record.
    #[must_use]
    pub const fn new(
        native: TopologyCandidate,
        selection: RegistrationSelectionRef,
        requirements: CandidateRequirements,
    ) -> Self {
        Self {
            native,
            selection,
            requirements,
        }
    }

    /// Returns the captured registration and configuration reference.
    #[must_use]
    pub const fn selection(&self) -> &RegistrationSelectionRef {
        &self.selection
    }

    /// Returns the native catalog candidate.
    #[must_use]
    pub const fn native(&self) -> &TopologyCandidate {
        &self.native
    }

    /// Returns stable non-typed requirements supplied by the registration.
    #[must_use]
    pub const fn requirements(&self) -> &CandidateRequirements {
        &self.requirements
    }
}

/// One candidate's complete immutable mechanical-preflight result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateMechanicalAdmission {
    candidate: RegistrationSelectionRef,
    result: AdmissionResult,
}

impl CandidateMechanicalAdmission {
    /// Returns the captured candidate reference.
    #[must_use]
    pub const fn candidate(&self) -> &RegistrationSelectionRef {
        &self.candidate
    }

    /// Returns the complete mechanical result.
    #[must_use]
    pub const fn result(&self) -> &AdmissionResult {
        &self.result
    }
}

/// Complete deterministic preflight over one immutable catalog candidate set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateAdmissionSet {
    requested_contract: TopologyContractKey,
    catalog_generation: u64,
    catalog_snapshot_digest: ContentDigest,
    availability_snapshot_id: crate::contracts::AvailabilitySnapshotId,
    availability_snapshot_digest: ContentDigest,
    evaluations: Vec<CandidateMechanicalAdmission>,
}

impl CandidateAdmissionSet {
    /// Returns the requested semantic contract.
    #[must_use]
    pub const fn requested_contract(&self) -> &TopologyContractKey {
        &self.requested_contract
    }

    /// Returns the process-local catalog generation used for later resolution.
    #[must_use]
    pub const fn catalog_generation(&self) -> u64 {
        self.catalog_generation
    }

    /// Returns stable catalog selection-content identity.
    #[must_use]
    pub const fn catalog_snapshot_digest(&self) -> ContentDigest {
        self.catalog_snapshot_digest
    }

    /// Returns immutable candidate evaluations in registration-ID order.
    #[must_use]
    pub fn evaluations(&self) -> &[CandidateMechanicalAdmission] {
        &self.evaluations
    }
}

/// Performs deterministic selection-time preflight without touching a live context.
#[expect(
    clippy::too_many_arguments,
    reason = "preflight binds both immutable snapshots and their registries"
)]
pub fn preflight_candidates(
    catalog: &TopologyCatalogSnapshot,
    catalog_snapshot_digest: ContentDigest,
    requested_contract: &TopologyContractKey,
    availability: &RuntimeAvailabilitySnapshot,
    availability_is_stale: bool,
    known_configuration_schemas: &BTreeSet<ConfigurationSchemaKey>,
    type_registry: &TypeExplanationRegistry,
    mut candidates: Vec<AdmissionCandidate>,
) -> Result<CandidateAdmissionSet, AdmissionBuildError> {
    candidates.sort_by(|left, right| {
        left.selection
            .registration_id()
            .cmp(right.selection.registration_id())
    });
    let captured_ids: Vec<_> = catalog
        .candidates()
        .iter()
        .map(TopologyCandidate::registration_id)
        .collect();
    let supplied_ids: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.selection.registration_id())
        .collect();
    if captured_ids != supplied_ids || supplied_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(AdmissionBuildError::IncompleteCandidateSet);
    }

    let mut evaluations = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let reasons = preflight_reasons(
            &candidate,
            requested_contract,
            availability,
            availability_is_stale,
            known_configuration_schemas,
            type_registry,
            ExclusionPhase::Mechanical,
        )?;
        let result = if reasons.is_empty() {
            AdmissionResult::Admitted
        } else {
            AdmissionResult::excluded(reasons)?
        };
        evaluations.push(CandidateMechanicalAdmission {
            candidate: candidate.selection,
            result,
        });
    }
    Ok(CandidateAdmissionSet {
        requested_contract: requested_contract.clone(),
        catalog_generation: catalog.generation(),
        catalog_snapshot_digest,
        availability_snapshot_id: availability.snapshot_id(),
        availability_snapshot_digest: availability.content_digest(),
        evaluations,
    })
}

/// Performs generic preflight directly from one complete sealed catalog snapshot.
///
/// Complete registration makes configuration identity, stable native-signature
/// explanations, and declared requirements catalog-owned. Hosts therefore only
/// supply the requested contract and their immutable availability observation.
pub fn preflight_catalog(
    catalog: &TopologyCatalogSnapshot,
    requested_contract: &TopologyContractKey,
    availability: &RuntimeAvailabilitySnapshot,
    availability_is_stale: bool,
) -> Result<CandidateAdmissionSet, AdmissionBuildError> {
    let mut evaluations = Vec::with_capacity(catalog.candidates().len());
    for candidate in catalog.candidates() {
        let metadata = candidate.admission_metadata().ok_or_else(|| {
            AdmissionBuildError::IncompleteRegistration {
                registration: candidate.registration_id().clone(),
            }
        })?;
        let mut reasons = Vec::new();
        let snapshot_ref = availability.content_digest();
        let registration_subject = candidate.registration_id().as_str();

        if availability_is_stale {
            reasons.push(reason(
                ExclusionPhase::Mechanical,
                "availability.snapshot-stale",
                registration_subject,
                snapshot_ref,
                None,
            )?);
        }
        if !candidate
            .descriptor()
            .contracts()
            .contains(requested_contract)
        {
            reasons.push(reason(
                ExclusionPhase::Mechanical,
                "contract.not-requested",
                requested_contract.id().as_str(),
                snapshot_ref,
                None,
            )?);
        }
        check_resource_requirements(
            metadata.signature_explanation(),
            availability,
            ExclusionPhase::Mechanical,
            &mut reasons,
        )?;
        check_declared_requirements(
            metadata.requirements(),
            availability,
            ExclusionPhase::Mechanical,
            &mut reasons,
        )?;
        reasons.sort();
        reasons.dedup();
        let result = if reasons.is_empty() {
            AdmissionResult::Admitted
        } else {
            AdmissionResult::excluded(reasons)?
        };
        evaluations.push(CandidateMechanicalAdmission {
            candidate: metadata.selection().clone(),
            result,
        });
    }

    Ok(CandidateAdmissionSet {
        requested_contract: requested_contract.clone(),
        catalog_generation: catalog.generation(),
        catalog_snapshot_digest: catalog.content_digest(),
        availability_snapshot_id: availability.snapshot_id(),
        availability_snapshot_digest: availability.content_digest(),
        evaluations,
    })
}

fn preflight_reasons(
    candidate: &AdmissionCandidate,
    requested_contract: &TopologyContractKey,
    availability: &RuntimeAvailabilitySnapshot,
    availability_is_stale: bool,
    known_configuration_schemas: &BTreeSet<ConfigurationSchemaKey>,
    type_registry: &TypeExplanationRegistry,
    phase: ExclusionPhase,
) -> Result<Vec<ExclusionReason>, AdmissionBuildError> {
    let mut reasons = Vec::new();
    let snapshot_ref = availability.content_digest();
    let registration_subject = candidate.selection.registration_id().as_str();

    if availability_is_stale {
        reasons.push(reason(
            phase,
            "availability.snapshot-stale",
            registration_subject,
            snapshot_ref,
            None,
        )?);
    }
    if candidate.native.registration_id() != candidate.selection.registration_id() {
        reasons.push(reason(
            phase,
            "registration.unknown",
            registration_subject,
            snapshot_ref,
            None,
        )?);
    }
    if candidate.native.descriptor().key() != candidate.selection.topology_key()
        || candidate.native.configuration_digest() != candidate.selection.configuration().digest()
    {
        reasons.push(reason(
            phase,
            "configuration.digest-mismatch",
            registration_subject,
            snapshot_ref,
            None,
        )?);
    }
    if !candidate
        .native
        .descriptor()
        .contracts()
        .contains(requested_contract)
    {
        reasons.push(reason(
            phase,
            "contract.not-requested",
            requested_contract.id().as_str(),
            snapshot_ref,
            None,
        )?);
    }
    if !known_configuration_schemas.contains(candidate.selection.configuration().schema()) {
        reasons.push(reason(
            phase,
            "configuration.schema-unknown",
            candidate.selection.configuration().schema().id().as_str(),
            snapshot_ref,
            None,
        )?);
    }

    match StableSignatureExplanation::derive(candidate.native.signature(), type_registry) {
        Ok(explanation) => {
            check_resource_requirements(&explanation, availability, phase, &mut reasons)?;
        }
        Err(missing) => {
            for boundary in missing {
                reasons.push(reason(
                    phase,
                    "type-explanation.missing",
                    registration_subject,
                    snapshot_ref,
                    Some(format!(
                        "unexplained {:?} boundary {}",
                        boundary.channel, boundary.display_type
                    )),
                )?);
            }
        }
    }
    check_declared_requirements(&candidate.requirements, availability, phase, &mut reasons)?;
    reasons.sort();
    reasons.dedup();
    Ok(reasons)
}

fn check_resource_requirements(
    explanation: &StableSignatureExplanation,
    availability: &RuntimeAvailabilitySnapshot,
    phase: ExclusionPhase,
    reasons: &mut Vec<ExclusionReason>,
) -> Result<(), AdmissionBuildError> {
    let snapshot_ref = availability.content_digest();
    for requirement in explanation.required_resources() {
        let exact = availability.resources().iter().find(|fact| {
            fact.type_key() == requirement.type_key() && fact.access() == requirement.access()
        });
        let compatible_write = (requirement.access() == ResourceAccess::Read).then(|| {
            availability.resources().iter().find(|fact| {
                fact.type_key() == requirement.type_key() && fact.access() == ResourceAccess::Write
            })
        });
        let fact = exact.or_else(|| compatible_write.flatten());
        match fact.map(|fact| fact.observation().state()) {
            Some(AvailabilityState::Available) => {}
            Some(AvailabilityState::Unknown) => reasons.push(reason(
                phase,
                "availability.unknown",
                requirement.type_key().id().as_str(),
                snapshot_ref,
                None,
            )?),
            Some(AvailabilityState::Unavailable) | None => {
                let has_read = availability.resources().iter().any(|fact| {
                    fact.type_key() == requirement.type_key()
                        && fact.access() == ResourceAccess::Read
                        && fact.observation().state() == AvailabilityState::Available
                });
                let code = if requirement.access() == ResourceAccess::Write && has_read {
                    "resource.access-insufficient"
                } else {
                    "resource.missing"
                };
                reasons.push(reason(
                    phase,
                    code,
                    requirement.type_key().id().as_str(),
                    snapshot_ref,
                    None,
                )?);
            }
        }
    }
    for required in explanation.required_outputs() {
        match availability
            .upstream_outputs()
            .iter()
            .find(|fact| fact.type_key() == required)
            .map(|fact| fact.observation().state())
        {
            Some(AvailabilityState::Available) => {}
            Some(AvailabilityState::Unknown) => reasons.push(reason(
                phase,
                "availability.unknown",
                required.id().as_str(),
                snapshot_ref,
                None,
            )?),
            Some(AvailabilityState::Unavailable) | None => reasons.push(reason(
                phase,
                "upstream-output.missing",
                required.id().as_str(),
                snapshot_ref,
                None,
            )?),
        }
    }
    Ok(())
}

fn check_declared_requirements(
    requirements: &CandidateRequirements,
    availability: &RuntimeAvailabilitySnapshot,
    phase: ExclusionPhase,
    reasons: &mut Vec<ExclusionReason>,
) -> Result<(), AdmissionBuildError> {
    let snapshot_ref = availability.content_digest();
    for required in &requirements.providers {
        let fact = availability
            .providers()
            .iter()
            .find(|fact| fact.provider() == &required.provider);
        if !fact.is_some_and(|fact| {
            fact.observation().state() == AvailabilityState::Available
                && fact.provider_configuration_digest() == required.configuration_digest
        }) {
            reasons.push(reason(
                phase,
                "provider.unavailable",
                required.provider.id().as_str(),
                snapshot_ref,
                None,
            )?);
        }
    }
    for required in &requirements.bindings {
        let fact = availability
            .bindings()
            .iter()
            .find(|fact| fact.alias() == &required.alias);
        if !fact.is_some_and(|fact| {
            fact.kind() == &required.kind
                && fact.provider() == &required.provider
                && fact.observation().state() == AvailabilityState::Available
        }) {
            reasons.push(reason(
                phase,
                "configuration.binding-unavailable",
                required.alias.as_str(),
                snapshot_ref,
                None,
            )?);
        }
    }
    for required in &requirements.capabilities {
        if !availability.capabilities().iter().any(|fact| {
            fact.capability() == required
                && fact.observation().state() == AvailabilityState::Available
        }) {
            reasons.push(reason(
                phase,
                "capability.missing",
                required.id().as_str(),
                snapshot_ref,
                None,
            )?);
        }
    }
    for required in &requirements.mechanisms {
        if !availability.mechanisms().iter().any(|fact| {
            fact.mechanism() == required
                && fact.observation().state() == AvailabilityState::Available
        }) {
            reasons.push(reason(
                phase,
                "mechanism.unsupported",
                required.id().as_str(),
                snapshot_ref,
                None,
            )?);
        }
    }
    Ok(())
}

fn reason(
    phase: ExclusionPhase,
    code: &str,
    subject: &str,
    snapshot_ref: ContentDigest,
    detail: Option<String>,
) -> Result<ExclusionReason, ContractError> {
    let value = ExclusionReason::new(
        phase,
        ExclusionCode::new(code)?,
        vec![RecordSubjectId::new(subject)?],
        vec![snapshot_ref],
    );
    match detail {
        Some(detail) => value.with_detail(detail),
        None => Ok(value),
    }
}

/// Result of live native readmission and its optional one-use start capability.
pub struct LiveAdmission<'context> {
    evaluation: AdmissionEvaluation,
    permit: Option<StartPermit<'context>>,
}

impl<'context> LiveAdmission<'context> {
    /// Returns the append-only live-admission event.
    #[must_use]
    pub const fn evaluation(&self) -> &AdmissionEvaluation {
        &self.evaluation
    }

    /// Returns whether a start permit was issued.
    #[must_use]
    pub const fn is_permitted(&self) -> bool {
        self.permit.is_some()
    }

    /// Splits the durable evaluation from the non-serializable permit.
    #[must_use]
    pub fn into_parts(self) -> (AdmissionEvaluation, Option<StartPermit<'context>>) {
        (self.evaluation, self.permit)
    }
}

/// Performs live readmission against the exact graph and pristine context.
#[expect(
    clippy::too_many_arguments,
    reason = "live admission joins the decision, snapshots, catalog, and context"
)]
pub fn readmit_live<'context>(
    catalog: &TopologyCatalog,
    catalog_generation: u64,
    expected_catalog_digest: ContentDigest,
    current_catalog_digest: ContentDigest,
    candidate: &AdmissionCandidate,
    requested_contract: &TopologyContract,
    availability: &RuntimeAvailabilitySnapshot,
    known_configuration_schemas: &BTreeSet<ConfigurationSchemaKey>,
    type_registry: &TypeExplanationRegistry,
    executor: &GraphExecutor,
    context: &'context SystemContext<'_>,
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
    recorded_at: RecordedAt,
) -> Result<LiveAdmission<'context>, AdmissionBuildError> {
    let mut reasons = Vec::new();
    if expected_catalog_digest != current_catalog_digest {
        reasons.push(reason(
            ExclusionPhase::LiveReadmission,
            "catalog.stale",
            candidate.selection.registration_id().as_str(),
            current_catalog_digest,
            None,
        )?);
    }

    let resolved =
        match catalog.resolve_at(candidate.selection.registration_id(), catalog_generation) {
            Ok(registered) => Some(registered),
            Err(TopologyCatalogError::StaleGeneration { .. }) => {
                reasons.push(reason(
                    ExclusionPhase::LiveReadmission,
                    "catalog.stale",
                    candidate.selection.registration_id().as_str(),
                    current_catalog_digest,
                    None,
                )?);
                None
            }
            Err(TopologyCatalogError::UnknownRegistration { .. }) => {
                reasons.push(reason(
                    ExclusionPhase::LiveReadmission,
                    "registration.unknown",
                    candidate.selection.registration_id().as_str(),
                    current_catalog_digest,
                    None,
                )?);
                None
            }
            Err(error) => {
                reasons.push(reason(
                    ExclusionPhase::LiveReadmission,
                    "catalog.stale",
                    candidate.selection.registration_id().as_str(),
                    current_catalog_digest,
                    Some(error.to_string()),
                )?);
                None
            }
        };

    if let Some(registered) = resolved.as_deref() {
        check_live_registration(
            registered,
            candidate,
            requested_contract,
            availability,
            known_configuration_schemas,
            type_registry,
            executor,
            context,
            &mut reasons,
        )?;
    }

    reasons.sort();
    reasons.dedup();
    let result = if reasons.is_empty() {
        AdmissionResult::Admitted
    } else {
        AdmissionResult::excluded(reasons)?
    };
    let evaluation = AdmissionEvaluation::new(
        attempt_id,
        decision_id,
        candidate.selection.clone(),
        current_catalog_digest,
        availability.snapshot_id(),
        availability.content_digest(),
        result,
        recorded_at,
    )?;
    let permit = evaluation.is_admitted().then(|| StartPermit {
        decision_id,
        candidate: candidate.selection.clone(),
        context,
    });
    Ok(LiveAdmission { evaluation, permit })
}

#[expect(
    clippy::too_many_arguments,
    reason = "the native readmission phase intentionally checks every authority"
)]
fn check_live_registration(
    registered: &RegisteredTopology,
    candidate: &AdmissionCandidate,
    requested_contract: &TopologyContract,
    availability: &RuntimeAvailabilitySnapshot,
    known_configuration_schemas: &BTreeSet<ConfigurationSchemaKey>,
    type_registry: &TypeExplanationRegistry,
    executor: &GraphExecutor,
    context: &SystemContext<'_>,
    reasons: &mut Vec<ExclusionReason>,
) -> Result<(), AdmissionBuildError> {
    let phase = ExclusionPhase::LiveReadmission;
    let snapshot_ref = availability.content_digest();
    let registration_subject = candidate.selection.registration_id().as_str();
    if registered.descriptor().key() != candidate.selection.topology_key()
        || registered.configuration_digest() != candidate.selection.configuration().digest()
    {
        reasons.push(reason(
            phase,
            "configuration.digest-mismatch",
            registration_subject,
            snapshot_ref,
            None,
        )?);
    }
    if !known_configuration_schemas.contains(candidate.selection.configuration().schema()) {
        reasons.push(reason(
            phase,
            "configuration.schema-unknown",
            candidate.selection.configuration().schema().id().as_str(),
            snapshot_ref,
            None,
        )?);
    }
    let validation = registered.graph().validate();
    if validation.is_err() {
        reasons.push(reason(
            phase,
            "graph.invalid",
            registration_subject,
            snapshot_ref,
            Some(validation.to_string()),
        )?);
    }
    if !registered
        .descriptor()
        .contracts()
        .contains(requested_contract.key())
        || !registered
            .signature()
            .satisfies(requested_contract.signature())
    {
        reasons.push(reason(
            phase,
            "contract.unsatisfied",
            requested_contract.key().id().as_str(),
            snapshot_ref,
            None,
        )?);
    }
    match StableSignatureExplanation::derive(registered.signature(), type_registry) {
        Ok(explanation) => {
            check_resource_requirements(&explanation, availability, phase, reasons)?;
        }
        Err(missing) => {
            for boundary in missing {
                reasons.push(reason(
                    phase,
                    "type-explanation.missing",
                    registration_subject,
                    snapshot_ref,
                    Some(format!(
                        "unexplained {:?} boundary {}",
                        boundary.channel, boundary.display_type
                    )),
                )?);
            }
        }
    }
    check_declared_requirements(&candidate.requirements, availability, phase, reasons)?;

    if let Err(errors) = executor.validate_resources(registered.graph(), context, None) {
        for error in errors {
            reasons.push(native_validation_reason(
                error,
                type_registry,
                snapshot_ref,
            )?);
        }
    }
    Ok(())
}

fn native_validation_reason(
    error: ResourceValidationError,
    type_registry: &TypeExplanationRegistry,
    snapshot_ref: ContentDigest,
) -> Result<ExclusionReason, ContractError> {
    match error {
        ResourceValidationError::MissingResource {
            type_id,
            resource_type,
            ..
        } => match type_registry.key(SignatureChannel::Resource, type_id) {
            Some(key) => reason(
                ExclusionPhase::LiveReadmission,
                "resource.missing",
                key.id().as_str(),
                snapshot_ref,
                Some(format!(
                    "native validation could not resolve {resource_type}"
                )),
            ),
            None => reason(
                ExclusionPhase::LiveReadmission,
                "type-explanation.missing",
                "native-resource-boundary",
                snapshot_ref,
                Some(format!(
                    "native validation reported unexplained {resource_type}"
                )),
            ),
        },
        ResourceValidationError::MissingOutput {
            type_id,
            output_type,
            ..
        } => match type_registry.key(SignatureChannel::Output, type_id) {
            Some(key) => reason(
                ExclusionPhase::LiveReadmission,
                "upstream-output.missing",
                key.id().as_str(),
                snapshot_ref,
                Some(format!("native validation could not resolve {output_type}")),
            ),
            None => reason(
                ExclusionPhase::LiveReadmission,
                "type-explanation.missing",
                "native-output-boundary",
                snapshot_ref,
                Some(format!(
                    "native validation reported unexplained {output_type}"
                )),
            ),
        },
        other => reason(
            ExclusionPhase::LiveReadmission,
            "graph.invalid",
            "native-validation",
            snapshot_ref,
            Some(other.to_string()),
        ),
    }
}

/// Non-serializable, one-use proof that one candidate passed live readmission.
pub struct StartPermit<'context> {
    decision_id: SelectionDecisionId,
    candidate: RegistrationSelectionRef,
    context: &'context SystemContext<'context>,
}

impl<'context> StartPermit<'context> {
    /// Consumes the permit at the host's graph-start boundary.
    ///
    /// A different decision, registration revision, configuration, or context
    /// is rejected. Ownership makes successful consumption single-use.
    pub fn consume(
        self,
        decision_id: SelectionDecisionId,
        candidate: &RegistrationSelectionRef,
        context: &SystemContext<'context>,
    ) -> Result<RegistrationSelectionRef, StartPermitError> {
        if self.decision_id != decision_id {
            return Err(StartPermitError::Decision);
        }
        if &self.candidate != candidate {
            return Err(StartPermitError::RegistrationRevision);
        }
        if !std::ptr::eq(self.context, context) {
            return Err(StartPermitError::ContextIdentity);
        }
        Ok(self.candidate)
    }
}

/// Reason a start permit could not be consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPermitError {
    /// The host supplied another selection decision.
    Decision,
    /// The host supplied another registration revision or configuration.
    RegistrationRevision,
    /// The host supplied another live execution context.
    ContextIdentity,
}

impl fmt::Display for StartPermitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decision => formatter.write_str("start permit decision mismatch"),
            Self::RegistrationRevision => formatter.write_str("start permit candidate mismatch"),
            Self::ContextIdentity => formatter.write_str("start permit context mismatch"),
        }
    }
}

impl std::error::Error for StartPermitError {}

/// Failure while admitting a freshly materialized graph for one live context.
#[derive(Debug)]
#[non_exhaustive]
pub enum MaterializedGraphAdmissionError {
    /// Native structural validation rejected the graph before execution.
    GraphValidation(String),
    /// Native run-start resource validation found unsatisfied live requirements.
    ResourceValidation(Vec<ResourceValidationError>),
}

impl fmt::Display for MaterializedGraphAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GraphValidation(error) => {
                write!(formatter, "materialized graph is invalid: {error}")
            }
            Self::ResourceValidation(errors) => write!(
                formatter,
                "materialized graph has {} unsatisfied live requirement(s)",
                errors.len()
            ),
        }
    }
}

impl std::error::Error for MaterializedGraphAdmissionError {}

/// One-use proof that a freshly materialized graph passed live native checks.
pub struct MaterializedStartPermit<'context> {
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
    graph: &'context Graph,
    context: &'context SystemContext<'context>,
}

impl<'context> MaterializedStartPermit<'context> {
    /// Consumes the permit at the graph-start boundary.
    ///
    /// The host must present the same decision, attempt, graph allocation, and
    /// pristine context that native validation just admitted. Ownership makes a
    /// successful start single-use.
    pub fn consume(
        self,
        decision_id: SelectionDecisionId,
        attempt_id: AdmissionAttemptId,
        graph: &Graph,
        context: &SystemContext<'context>,
    ) -> Result<(), MaterializedStartPermitError> {
        if self.decision_id != decision_id {
            return Err(MaterializedStartPermitError::Decision);
        }
        if self.attempt_id != attempt_id {
            return Err(MaterializedStartPermitError::Attempt);
        }
        if !std::ptr::eq(self.graph, graph) {
            return Err(MaterializedStartPermitError::GraphIdentity);
        }
        if !std::ptr::eq(self.context, context) {
            return Err(MaterializedStartPermitError::ContextIdentity);
        }
        Ok(())
    }
}

/// Reason a freshly-materialized start permit could not be consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializedStartPermitError {
    /// The host supplied another selection decision.
    Decision,
    /// The host supplied another live-admission attempt.
    Attempt,
    /// The host supplied a different graph allocation.
    GraphIdentity,
    /// The host supplied another live execution context.
    ContextIdentity,
}

impl fmt::Display for MaterializedStartPermitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decision => formatter.write_str("materialized start permit decision mismatch"),
            Self::Attempt => formatter.write_str("materialized start permit attempt mismatch"),
            Self::GraphIdentity => formatter.write_str("materialized start permit graph mismatch"),
            Self::ContextIdentity => {
                formatter.write_str("materialized start permit context mismatch")
            }
        }
    }
}

impl std::error::Error for MaterializedStartPermitError {}

/// Performs native live admission for a graph produced outside the topology catalog.
///
/// `composition::materialize` has already checked catalogue resolution,
/// effects, limits, and target-contract satisfaction. This final boundary
/// repeats native structural validation and checks the actual pristine context
/// before issuing the same one-use start capability used by catalog entries.
pub fn readmit_materialized_graph<'context>(
    graph: &'context Graph,
    executor: &GraphExecutor,
    context: &'context SystemContext<'context>,
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
) -> Result<MaterializedStartPermit<'context>, MaterializedGraphAdmissionError> {
    let validation = graph.validate();
    if !validation.is_ok() {
        return Err(MaterializedGraphAdmissionError::GraphValidation(
            validation.to_string(),
        ));
    }
    executor
        .validate_resources(graph, context, None)
        .map_err(MaterializedGraphAdmissionError::ResourceValidation)?;
    Ok(MaterializedStartPermit {
        decision_id,
        attempt_id,
        graph,
        context,
    })
}

/// Whether a captured fallback order remains valid before any permit is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackValidity {
    /// Stable catalog content and every captured registration revision match.
    ReuseCapturedOrder,
    /// The candidate universe or one registration revision changed; reselect.
    Reselect,
}

/// Checks whether a pre-start fallback plan can be reused.
#[must_use]
pub fn validate_fallback_plan(
    captured_catalog_digest: ContentDigest,
    current_catalog_digest: ContentDigest,
    captured: &[RegistrationSelectionRef],
    current: &[RegistrationSelectionRef],
) -> FallbackValidity {
    if captured_catalog_digest != current_catalog_digest || captured.len() != current.len() {
        return FallbackValidity::Reselect;
    }
    let mut captured = captured.to_vec();
    let mut current = current.to_vec();
    captured.sort_by(|left, right| left.registration_id().cmp(right.registration_id()));
    current.sort_by(|left, right| left.registration_id().cmp(right.registration_id()));
    if captured == current {
        FallbackValidity::ReuseCapturedOrder
    } else {
        FallbackValidity::Reselect
    }
}
