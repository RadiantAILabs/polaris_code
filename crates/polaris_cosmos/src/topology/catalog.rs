//! Validated topology registration and immutable discovery snapshots.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use polaris::graph::{Graph, GraphSignature, SignatureDiff, ValidationResult, ValidationWarning};
use polaris::system::plugin::{Contract, Version};
use polaris::system::resource::GlobalResource;
use serde::Serialize;

use crate::contracts::{
    ContentDigest, ContractError, RegistrationSelectionRef, ScoreComponent, ScoreComponentId,
    ScoreSource,
};

use super::{
    AdmissionBuildError, CandidateRequirements, ConfigurationDigest, RegistrationId,
    RegistrationProvenance, StableSignatureExplanation, TopologyContract, TopologyContractKey,
    TopologyDescriptor, TypeExplanationRegistry, UnexplainedBoundary,
};

/// Immutable admission and author-prior metadata owned beside one catalog graph.
#[derive(Debug, Clone)]
pub struct RegistrationAdmissionMetadata {
    selection: RegistrationSelectionRef,
    requirements: CandidateRequirements,
    signature_explanation: StableSignatureExplanation,
    author_priors: Arc<[ScoreComponent]>,
}

impl RegistrationAdmissionMetadata {
    /// Returns the stable registration, revision, and resolved configuration reference.
    #[must_use]
    pub const fn selection(&self) -> &RegistrationSelectionRef {
        &self.selection
    }

    /// Returns secret-free provider, binding, capability, and mechanism requirements.
    #[must_use]
    pub const fn requirements(&self) -> &CandidateRequirements {
        &self.requirements
    }

    /// Returns the complete stable projection of the native graph signature.
    #[must_use]
    pub const fn signature_explanation(&self) -> &StableSignatureExplanation {
        &self.signature_explanation
    }

    /// Returns attributed author-prior score components in canonical identity order.
    #[must_use]
    pub fn author_priors(&self) -> &[ScoreComponent] {
        &self.author_priors
    }

    /// Returns the stable configuration-schema identity used by this registration.
    #[must_use]
    pub const fn configuration_schema(&self) -> &crate::contracts::ConfigurationSchemaKey {
        self.selection.configuration().schema()
    }
}

/// Complete owned input for one atomic, admission-ready catalog registration.
///
/// The graph remains an ordinary native [`Graph`]. The supplied type registry
/// only explains that graph's process-local signature boundaries; the catalog
/// merges it into its one registry transactionally after every validation has
/// succeeded.
#[derive(Debug)]
pub struct CompleteTopologyRegistration {
    selection: RegistrationSelectionRef,
    descriptor: TopologyDescriptor,
    graph: Graph,
    provenance: RegistrationProvenance,
    requirements: CandidateRequirements,
    type_explanations: TypeExplanationRegistry,
    author_priors: Vec<ScoreComponent>,
}

impl CompleteTopologyRegistration {
    /// Creates one complete registration command.
    #[must_use]
    pub fn new(
        selection: RegistrationSelectionRef,
        descriptor: TopologyDescriptor,
        graph: Graph,
        provenance: RegistrationProvenance,
        requirements: CandidateRequirements,
        type_explanations: TypeExplanationRegistry,
        author_priors: Vec<ScoreComponent>,
    ) -> Self {
        Self {
            selection,
            descriptor,
            graph,
            provenance,
            requirements,
            type_explanations,
            author_priors,
        }
    }
}

/// One validated, immutable, embodied topology available for execution.
///
/// The graph has no per-run state and may be shared by many executions. Its
/// signature is always derived during catalog registration and cannot be
/// supplied independently by a caller.
#[derive(Debug)]
pub struct RegisteredTopology {
    registration_id: RegistrationId,
    descriptor: Arc<TopologyDescriptor>,
    configuration_digest: ConfigurationDigest,
    graph: Arc<Graph>,
    signature: GraphSignature,
    validation_warnings: Arc<[ValidationWarning]>,
    provenance: RegistrationProvenance,
    admission_metadata: Option<Arc<RegistrationAdmissionMetadata>>,
}

impl RegisteredTopology {
    /// Returns the stable identity of this concrete registration.
    #[must_use]
    pub fn registration_id(&self) -> &RegistrationId {
        &self.registration_id
    }

    /// Returns the topology's semantic descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &TopologyDescriptor {
        &self.descriptor
    }

    /// Returns the digest of the embodied topology configuration.
    #[must_use]
    pub fn configuration_digest(&self) -> &ConfigurationDigest {
        &self.configuration_digest
    }

    /// Returns the immutable executable graph.
    #[must_use]
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Clones the shared executable graph handle for a runtime owner.
    #[must_use]
    pub fn shared_graph(&self) -> Arc<Graph> {
        Arc::clone(&self.graph)
    }

    /// Returns the graph signature derived at registration.
    #[must_use]
    pub fn signature(&self) -> &GraphSignature {
        &self.signature
    }

    /// Returns non-fatal graph-validation findings retained at registration.
    #[must_use]
    pub fn validation_warnings(&self) -> &[ValidationWarning] {
        &self.validation_warnings
    }

    /// Returns the pack, embodiment, or application that supplied the graph.
    #[must_use]
    pub fn provenance(&self) -> &RegistrationProvenance {
        &self.provenance
    }

    /// Returns catalog-owned admission metadata for a complete registration.
    ///
    /// Partial registrations created with [`TopologyCatalog::register`] return
    /// `None` and cannot pass the provider's admission-ready sealing audit.
    #[must_use]
    pub fn admission_metadata(&self) -> Option<&RegistrationAdmissionMetadata> {
        self.admission_metadata.as_deref()
    }

    fn candidate(&self) -> TopologyCandidate {
        TopologyCandidate {
            registration_id: self.registration_id.clone(),
            descriptor: Arc::clone(&self.descriptor),
            configuration_digest: self.configuration_digest.clone(),
            signature: self.signature.clone(),
            validation_warnings: Arc::clone(&self.validation_warnings),
            provenance: self.provenance.clone(),
            admission_metadata: self.admission_metadata.clone(),
        }
    }
}

/// Selection-facing summary of one registered topology.
///
/// A candidate contains the exact semantic metadata and derived signature used
/// at registration and, for complete registrations, immutable admission
/// metadata. It deliberately omits the executable [`Graph`]. The runtime
/// resolves the chosen [`RegistrationId`] against the live catalog and
/// generation after selection.
#[derive(Debug, Clone)]
pub struct TopologyCandidate {
    registration_id: RegistrationId,
    descriptor: Arc<TopologyDescriptor>,
    configuration_digest: ConfigurationDigest,
    signature: GraphSignature,
    validation_warnings: Arc<[ValidationWarning]>,
    provenance: RegistrationProvenance,
    admission_metadata: Option<Arc<RegistrationAdmissionMetadata>>,
}

impl TopologyCandidate {
    /// Returns the concrete registration identity.
    #[must_use]
    pub fn registration_id(&self) -> &RegistrationId {
        &self.registration_id
    }

    /// Returns the topology's semantic descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &TopologyDescriptor {
        &self.descriptor
    }

    /// Returns the configuration digest used by this registration.
    #[must_use]
    pub fn configuration_digest(&self) -> &ConfigurationDigest {
        &self.configuration_digest
    }

    /// Returns the graph signature derived during registration.
    #[must_use]
    pub fn signature(&self) -> &GraphSignature {
        &self.signature
    }

    /// Returns non-fatal graph-validation findings.
    #[must_use]
    pub fn validation_warnings(&self) -> &[ValidationWarning] {
        &self.validation_warnings
    }

    /// Returns the registration provider provenance.
    #[must_use]
    pub fn provenance(&self) -> &RegistrationProvenance {
        &self.provenance
    }

    /// Returns complete immutable admission metadata when registered atomically.
    #[must_use]
    pub fn admission_metadata(&self) -> Option<&RegistrationAdmissionMetadata> {
        self.admission_metadata.as_deref()
    }
}

/// Immutable selection input captured from one sealed catalog generation.
///
/// Snapshots contain contract definitions, candidate summaries, and a stable
/// content digest, never live mutable maps or executable graph handles.
#[derive(Debug, Clone)]
pub struct TopologyCatalogSnapshot {
    generation: u64,
    content_digest: ContentDigest,
    contracts: Arc<[TopologyContract]>,
    candidates: Arc<[TopologyCandidate]>,
}

impl TopologyCatalogSnapshot {
    /// Returns the catalog generation this snapshot represents.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns stable identity for every catalog field that can affect admission or selection.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    /// Returns semantic contracts in deterministic key order.
    #[must_use]
    pub fn contracts(&self) -> &[TopologyContract] {
        &self.contracts
    }

    /// Returns selection candidates in deterministic registration-ID order.
    #[must_use]
    pub fn candidates(&self) -> &[TopologyCandidate] {
        &self.candidates
    }

    /// Finds a candidate by concrete registration identity.
    #[must_use]
    pub fn candidate(&self, id: &RegistrationId) -> Option<&TopologyCandidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.registration_id() == id)
    }
}

/// Catalog registration or resolution failure.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TopologyCatalogError {
    /// A mutation was attempted after startup sealing.
    Sealed,
    /// A snapshot or resolution was requested before startup sealing.
    NotSealed,
    /// A semantic contract key is already registered.
    DuplicateContract {
        /// The duplicate contract key.
        contract: TopologyContractKey,
    },
    /// A concrete registration identity is already present.
    DuplicateRegistration {
        /// The duplicate registration identity.
        registration: RegistrationId,
    },
    /// Complete registration metadata names a different topology than its descriptor.
    RegistrationTopologyMismatch {
        /// Registration containing the inconsistent identity.
        registration: RegistrationId,
    },
    /// A frozen selection reference failed its own contract validation.
    InvalidSelectionMetadata {
        /// Registration containing invalid selection metadata.
        registration: RegistrationId,
        /// Stable contract validation error.
        source: ContractError,
    },
    /// Catalog and registration-local type explanations conflict.
    AdmissionMetadataConflict {
        /// Registration whose explanations conflicted.
        registration: RegistrationId,
        /// Typed explanation-registry conflict.
        source: Box<AdmissionBuildError>,
    },
    /// The graph signature contains boundaries without stable explanations.
    UnexplainedSignature {
        /// Registration rejected before insertion.
        registration: RegistrationId,
        /// Complete set of missing native channels and ephemeral display labels.
        missing: Arc<[UnexplainedBoundary]>,
    },
    /// A static prior was not explicitly classified as author-supplied.
    InvalidAuthorPrior {
        /// Registration containing the invalid component.
        registration: RegistrationId,
        /// Conflicting score component identity.
        component: ScoreComponentId,
        /// Actual component source.
        source: ScoreSource,
    },
    /// Two author priors use the same stable component identity.
    DuplicateAuthorPrior {
        /// Registration containing the duplicate component.
        registration: RegistrationId,
        /// Duplicate score component identity.
        component: ScoreComponentId,
    },
    /// Admission-ready sealing found one or more partial registrations.
    IncompleteRegistrations {
        /// Registrations without complete catalog-owned admission metadata.
        registrations: Arc<[RegistrationId]>,
    },
    /// A topology descriptor claims no semantic contract.
    NoContractClaims {
        /// The registration rejected for being undiscoverable.
        registration: RegistrationId,
    },
    /// A descriptor claims a contract the catalog does not know.
    UnknownContract {
        /// The registration making the claim.
        registration: RegistrationId,
        /// The missing contract key.
        contract: TopologyContractKey,
    },
    /// The graph failed native Polaris structural validation.
    InvalidGraph {
        /// The rejected registration.
        registration: RegistrationId,
        /// Native validation errors and warnings.
        validation: ValidationResult,
    },
    /// A derived graph signature does not satisfy a claimed contract.
    UnsatisfiedContract {
        /// The rejected registration.
        registration: RegistrationId,
        /// The semantic contract that was not satisfied.
        contract: TopologyContractKey,
        /// Native Polaris capability-satisfaction differences.
        diff: Box<SignatureDiff>,
    },
    /// A selection decision names an older or otherwise different generation.
    StaleGeneration {
        /// Generation captured by selection.
        expected: u64,
        /// Current sealed catalog generation.
        actual: u64,
    },
    /// No registered topology has the requested identity.
    UnknownRegistration {
        /// The unresolved registration identity.
        registration: RegistrationId,
    },
}

impl fmt::Display for TopologyCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sealed => formatter.write_str("topology catalog is sealed"),
            Self::NotSealed => formatter.write_str("topology catalog is not sealed"),
            Self::DuplicateContract { contract } => {
                write!(
                    formatter,
                    "topology contract {contract} is already registered"
                )
            }
            Self::DuplicateRegistration { registration } => write!(
                formatter,
                "topology registration {registration} is already registered"
            ),
            Self::RegistrationTopologyMismatch { registration } => write!(
                formatter,
                "topology registration {registration} has different descriptor and selection topology identities"
            ),
            Self::InvalidSelectionMetadata {
                registration,
                source,
            } => write!(
                formatter,
                "topology registration {registration} has invalid selection metadata: {source}"
            ),
            Self::AdmissionMetadataConflict {
                registration,
                source,
            } => write!(
                formatter,
                "topology registration {registration} has conflicting type explanations: {source}"
            ),
            Self::UnexplainedSignature {
                registration,
                missing,
            } => {
                write!(
                    formatter,
                    "topology registration {registration} has {} unexplained signature boundary(s)",
                    missing.len()
                )?;
                for boundary in missing.iter() {
                    write!(
                        formatter,
                        "; {:?} {}",
                        boundary.channel(),
                        boundary.display_type()
                    )?;
                }
                Ok(())
            }
            Self::InvalidAuthorPrior {
                registration,
                component,
                source,
            } => write!(
                formatter,
                "topology registration {registration} component {component} is {source:?}, not an author prior"
            ),
            Self::DuplicateAuthorPrior {
                registration,
                component,
            } => write!(
                formatter,
                "topology registration {registration} repeats author-prior component {component}"
            ),
            Self::IncompleteRegistrations { registrations } => {
                formatter.write_str("topology catalog contains incomplete registrations")?;
                for registration in registrations.iter() {
                    write!(formatter, "; {registration}")?;
                }
                Ok(())
            }
            Self::NoContractClaims { registration } => write!(
                formatter,
                "topology registration {registration} claims no semantic contract"
            ),
            Self::UnknownContract {
                registration,
                contract,
            } => write!(
                formatter,
                "topology registration {registration} claims unknown contract {contract}"
            ),
            Self::InvalidGraph {
                registration,
                validation,
            } => write!(
                formatter,
                "topology registration {registration} has an invalid graph: {validation}"
            ),
            Self::UnsatisfiedContract {
                registration,
                contract,
                diff,
            } => write!(
                formatter,
                "topology registration {registration} does not satisfy contract {contract}: {diff}"
            ),
            Self::StaleGeneration { expected, actual } => write!(
                formatter,
                "topology selection used catalog generation {expected}, but current generation is {actual}"
            ),
            Self::UnknownRegistration { registration } => {
                write!(formatter, "unknown topology registration {registration}")
            }
        }
    }
}

impl std::error::Error for TopologyCatalogError {}

/// Use `Res<TopologyCatalog>` when a system needs to inspect the sealed library
/// of topology contracts and candidates or capture a selection snapshot.
///
/// Complete registration consumes owned graphs, runs native
/// [`Graph::validate`], derives their [`GraphSignature`], verifies every claimed
/// semantic contract with [`GraphSignature::satisfies`], and retains stable
/// admission metadata atomically. The default provider calls
/// [`seal_admission_ready`](Self::seal_admission_ready) after startup; mutation
/// is then rejected and systems may take immutable snapshots.
///
/// # Scope
///
/// **Global.** The first implementation is a process-wide, startup-sealed
/// library shared by every agent execution.
///
/// # Provided by
///
/// [`TopologyCatalogPlugin`](super::TopologyCatalogPlugin) provides the default
/// lifecycle point. Alternative providers may expose the same capability
/// contract.
///
/// # Access
///
/// Topology-pack plugins populate the catalog during startup through
/// [`Extends<TopologyCatalog>`](polaris::system::plugin::Extends). Systems then
/// access the sealed catalog through `Res<TopologyCatalog>`. The resource has no
/// interior mutability. Selection systems should flow the resulting
/// [`TopologyCatalogSnapshot`] through `Out<T>`.
///
/// # Alternatives
///
/// Use a [`TopologyCatalogSnapshot`] when deterministic selection must be tied
/// to one generation. Polaris's `SubgraphRegistry` remains the alternative for
/// hot-swapping implementations inside one fixed signature contract.
///
/// # Example system
///
/// ```
/// use polaris::system::param::Res;
/// use polaris::system::system;
/// use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogSnapshot};
///
/// #[system]
/// async fn snapshot(catalog: Res<TopologyCatalog>) -> TopologyCatalogSnapshot {
///     catalog.snapshot().expect("catalog is sealed before execution")
/// }
/// ```
#[derive(Debug, Default)]
pub struct TopologyCatalog {
    generation: u64,
    sealed: bool,
    contracts: BTreeMap<TopologyContractKey, TopologyContract>,
    registrations: BTreeMap<RegistrationId, Arc<RegisteredTopology>>,
    type_explanations: TypeExplanationRegistry,
}

impl TopologyCatalog {
    /// Creates an empty, mutable startup catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the monotonically increasing catalog generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns whether startup registration has been sealed.
    #[must_use]
    pub const fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// Returns the number of registered executable topologies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.registrations.len()
    }

    /// Returns whether no executable topology has been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }

    /// Returns the one process-local type-explanation registry assembled by complete registrations.
    ///
    /// Live native validation uses this registry only to explain failures;
    /// Polaris's `TypeId`-based context validation remains authoritative.
    #[must_use]
    pub const fn type_explanations(&self) -> &TypeExplanationRegistry {
        &self.type_explanations
    }

    /// Registers one semantic topology contract during startup.
    pub fn register_contract(
        &mut self,
        contract: TopologyContract,
    ) -> Result<(), TopologyCatalogError> {
        self.ensure_mutable()?;
        let key = contract.key().clone();
        if self.contracts.contains_key(&key) {
            return Err(TopologyCatalogError::DuplicateContract { contract: key });
        }
        self.contracts.insert(key, contract);
        self.advance_generation();
        Ok(())
    }

    /// Validates and registers one concrete executable topology during startup.
    ///
    /// The graph is accepted by ownership so no caller can mutate it after its
    /// signature has been derived. This compatibility operation does not carry
    /// admission metadata, so a catalog using it cannot pass
    /// [`seal_admission_ready`](Self::seal_admission_ready). Topology packs
    /// should use [`register_complete`](Self::register_complete).
    pub fn register(
        &mut self,
        registration_id: RegistrationId,
        descriptor: TopologyDescriptor,
        configuration_digest: ConfigurationDigest,
        graph: Graph,
        provenance: RegistrationProvenance,
    ) -> Result<Arc<RegisteredTopology>, TopologyCatalogError> {
        self.ensure_mutable()?;
        if self.registrations.contains_key(&registration_id) {
            return Err(TopologyCatalogError::DuplicateRegistration {
                registration: registration_id,
            });
        }
        if descriptor.contracts().is_empty() {
            return Err(TopologyCatalogError::NoContractClaims {
                registration: registration_id,
            });
        }

        let validation = graph.validate();
        if validation.is_err() {
            return Err(TopologyCatalogError::InvalidGraph {
                registration: registration_id,
                validation,
            });
        }

        let signature = graph.signature();
        for contract_key in descriptor.contracts() {
            let Some(contract) = self.contracts.get(contract_key) else {
                return Err(TopologyCatalogError::UnknownContract {
                    registration: registration_id,
                    contract: contract_key.clone(),
                });
            };
            let diff = signature.satisfies_diff(contract.signature());
            if !diff.is_empty() {
                return Err(TopologyCatalogError::UnsatisfiedContract {
                    registration: registration_id,
                    contract: contract_key.clone(),
                    diff: Box::new(diff),
                });
            }
        }

        let registered = Arc::new(RegisteredTopology {
            registration_id: registration_id.clone(),
            descriptor: Arc::new(descriptor),
            configuration_digest,
            graph: Arc::new(graph),
            signature,
            validation_warnings: validation.warnings.into(),
            provenance,
            admission_metadata: None,
        });
        self.registrations
            .insert(registration_id, Arc::clone(&registered));
        self.advance_generation();
        Ok(registered)
    }

    /// Atomically validates and inserts one admission-ready registration.
    ///
    /// The operation stages type-registry merging, native graph validation,
    /// semantic-contract satisfaction, stable signature explanation, resolved
    /// configuration identity, and author priors before mutating the catalog.
    /// Any error leaves registrations, explanations, and generation unchanged.
    pub fn register_complete(
        &mut self,
        registration: CompleteTopologyRegistration,
    ) -> Result<Arc<RegisteredTopology>, TopologyCatalogError> {
        self.ensure_mutable()?;
        let CompleteTopologyRegistration {
            selection,
            descriptor,
            graph,
            provenance,
            requirements,
            type_explanations,
            mut author_priors,
        } = registration;
        let registration_id = selection.registration_id().clone();
        if self.registrations.contains_key(&registration_id) {
            return Err(TopologyCatalogError::DuplicateRegistration {
                registration: registration_id,
            });
        }
        selection
            .validate()
            .map_err(|source| TopologyCatalogError::InvalidSelectionMetadata {
                registration: registration_id.clone(),
                source,
            })?;
        if descriptor.key() != selection.topology_key() {
            return Err(TopologyCatalogError::RegistrationTopologyMismatch {
                registration: registration_id,
            });
        }
        if descriptor.contracts().is_empty() {
            return Err(TopologyCatalogError::NoContractClaims {
                registration: registration_id,
            });
        }

        author_priors.sort_by(|left, right| left.component_id().cmp(right.component_id()));
        for component in &author_priors {
            if component.source() != ScoreSource::AuthorPrior {
                return Err(TopologyCatalogError::InvalidAuthorPrior {
                    registration: registration_id,
                    component: component.component_id().clone(),
                    source: component.source(),
                });
            }
        }
        if let Some(duplicate) = author_priors
            .windows(2)
            .find(|pair| pair[0].component_id() == pair[1].component_id())
        {
            return Err(TopologyCatalogError::DuplicateAuthorPrior {
                registration: registration_id,
                component: duplicate[0].component_id().clone(),
            });
        }

        let staged_explanations = self
            .type_explanations
            .merged_with(&type_explanations)
            .map_err(|source| TopologyCatalogError::AdmissionMetadataConflict {
                registration: registration_id.clone(),
                source: Box::new(source),
            })?;

        let validation = graph.validate();
        if validation.is_err() {
            return Err(TopologyCatalogError::InvalidGraph {
                registration: registration_id,
                validation,
            });
        }
        let signature = graph.signature();
        for contract_key in descriptor.contracts() {
            let Some(contract) = self.contracts.get(contract_key) else {
                return Err(TopologyCatalogError::UnknownContract {
                    registration: registration_id,
                    contract: contract_key.clone(),
                });
            };
            let diff = signature.satisfies_diff(contract.signature());
            if !diff.is_empty() {
                return Err(TopologyCatalogError::UnsatisfiedContract {
                    registration: registration_id,
                    contract: contract_key.clone(),
                    diff: Box::new(diff),
                });
            }
        }
        let signature_explanation =
            StableSignatureExplanation::derive(&signature, &staged_explanations).map_err(
                |missing| TopologyCatalogError::UnexplainedSignature {
                    registration: registration_id.clone(),
                    missing: missing.into(),
                },
            )?;
        let configuration_digest = selection.configuration().digest().clone();
        let admission_metadata = Arc::new(RegistrationAdmissionMetadata {
            selection,
            requirements,
            signature_explanation,
            author_priors: author_priors.into(),
        });
        let registered = Arc::new(RegisteredTopology {
            registration_id: registration_id.clone(),
            descriptor: Arc::new(descriptor),
            configuration_digest,
            graph: Arc::new(graph),
            signature,
            validation_warnings: validation.warnings.into(),
            provenance,
            admission_metadata: Some(admission_metadata),
        });

        self.type_explanations = staged_explanations;
        self.registrations
            .insert(registration_id, Arc::clone(&registered));
        self.advance_generation();
        Ok(registered)
    }

    /// Seals startup registration and returns an immutable discovery snapshot.
    ///
    /// Calling this again is idempotent and returns the same generation. This
    /// compatibility boundary does not audit partial registrations; plugin
    /// providers and production assemblies should use
    /// [`seal_admission_ready`](Self::seal_admission_ready).
    #[must_use]
    pub fn seal(&mut self) -> TopologyCatalogSnapshot {
        self.sealed = true;
        self.build_snapshot()
    }

    /// Defensively audits complete-registration invariants, then seals the catalog.
    ///
    /// The default [`TopologyCatalogPlugin`](super::TopologyCatalogPlugin) uses
    /// this boundary so a partially assembled registration cannot
    /// become visible to runtime selectors. Repeating the audit after sealing
    /// is idempotent.
    pub fn seal_admission_ready(
        &mut self,
    ) -> Result<TopologyCatalogSnapshot, TopologyCatalogError> {
        let incomplete: Vec<_> = self
            .registrations
            .values()
            .filter(|registered| registered.admission_metadata().is_none())
            .map(|registered| registered.registration_id().clone())
            .collect();
        if !incomplete.is_empty() {
            return Err(TopologyCatalogError::IncompleteRegistrations {
                registrations: incomplete.into(),
            });
        }
        if self.sealed {
            return Ok(self.build_snapshot());
        }
        self.sealed = true;
        Ok(self.build_snapshot())
    }

    /// Returns an immutable snapshot of the sealed catalog.
    pub fn snapshot(&self) -> Result<TopologyCatalogSnapshot, TopologyCatalogError> {
        if !self.sealed {
            return Err(TopologyCatalogError::NotSealed);
        }
        Ok(self.build_snapshot())
    }

    /// Resolves a selected registration against its captured catalog generation.
    pub fn resolve_at(
        &self,
        registration: &RegistrationId,
        generation: u64,
    ) -> Result<Arc<RegisteredTopology>, TopologyCatalogError> {
        if !self.sealed {
            return Err(TopologyCatalogError::NotSealed);
        }
        if generation != self.generation {
            return Err(TopologyCatalogError::StaleGeneration {
                expected: generation,
                actual: self.generation,
            });
        }
        self.registrations
            .get(registration)
            .cloned()
            .ok_or_else(|| TopologyCatalogError::UnknownRegistration {
                registration: registration.clone(),
            })
    }

    fn ensure_mutable(&self) -> Result<(), TopologyCatalogError> {
        if self.sealed {
            Err(TopologyCatalogError::Sealed)
        } else {
            Ok(())
        }
    }

    fn advance_generation(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("topology catalog generation overflow");
    }

    fn build_snapshot(&self) -> TopologyCatalogSnapshot {
        let contracts: Arc<[TopologyContract]> = self.contracts.values().cloned().collect();
        let candidates: Arc<[TopologyCandidate]> = self
            .registrations
            .values()
            .map(|registered| registered.candidate())
            .collect();
        TopologyCatalogSnapshot {
            generation: self.generation,
            content_digest: catalog_content_digest(&contracts, &candidates),
            contracts,
            candidates,
        }
    }
}

#[derive(Serialize)]
struct CatalogContentView<'a> {
    schema: &'static str,
    contracts: Vec<ContractContentView<'a>>,
    candidates: Vec<CandidateContentView<'a>>,
}

#[derive(Serialize)]
struct ContractContentView<'a> {
    key: &'a TopologyContractKey,
    task_semantics: &'a str,
    result_semantics: &'a str,
    outcome_interpretation: Option<&'a str>,
    documentation: Option<&'a str>,
}

#[derive(Serialize)]
struct CandidateContentView<'a> {
    registration_id: &'a RegistrationId,
    topology_key: &'a super::TopologyKey,
    title: &'a str,
    summary: &'a str,
    contracts: &'a [TopologyContractKey],
    configuration_digest: &'a ConfigurationDigest,
    provenance: &'a str,
    admission: Option<AdmissionContentView<'a>>,
}

#[derive(Serialize)]
struct AdmissionContentView<'a> {
    selection: &'a RegistrationSelectionRef,
    requirements: &'a CandidateRequirements,
    signature_explanation: &'a StableSignatureExplanation,
    author_priors: &'a [ScoreComponent],
}

fn catalog_content_digest(
    contracts: &[TopologyContract],
    candidates: &[TopologyCandidate],
) -> ContentDigest {
    let view = CatalogContentView {
        schema: "polaris-cosmos.topology-catalog-snapshot.v1",
        contracts: contracts
            .iter()
            .map(|contract| ContractContentView {
                key: contract.key(),
                task_semantics: contract.task_semantics(),
                result_semantics: contract.result_semantics(),
                outcome_interpretation: contract.outcome_interpretation(),
                documentation: contract.documentation(),
            })
            .collect(),
        candidates: candidates
            .iter()
            .map(|candidate| CandidateContentView {
                registration_id: candidate.registration_id(),
                topology_key: candidate.descriptor().key(),
                title: candidate.descriptor().title(),
                summary: candidate.descriptor().summary(),
                contracts: candidate.descriptor().contracts(),
                configuration_digest: candidate.configuration_digest(),
                provenance: candidate.provenance().provider(),
                admission: candidate
                    .admission_metadata()
                    .map(|metadata| AdmissionContentView {
                        selection: metadata.selection(),
                        requirements: metadata.requirements(),
                        signature_explanation: metadata.signature_explanation(),
                        author_priors: metadata.author_priors(),
                    }),
            })
            .collect(),
    };
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&view, &mut bytes)
        .expect("catalog content projection contains only serializable values");
    ContentDigest::sha256(&bytes)
}

impl GlobalResource for TopologyCatalog {}

/// Exposes the topology catalog as a replaceable, versioned plugin capability.
///
/// Provider plugins advertise this version, while topology-pack plugins derive
/// a compatible requirement by accepting
/// [`Extends<TopologyCatalog>`](polaris::system::plugin::Extends) during
/// `build()`.
impl Contract for TopologyCatalog {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}
