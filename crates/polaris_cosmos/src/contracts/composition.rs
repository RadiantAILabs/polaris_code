//! Bounded runtime-composition identity, provenance, and evidence records.
//!
//! A composition names approved recipe and block versions plus their resolved
//! configuration. It is deliberately not a serialised Polaris graph: a host
//! still materialises an ordinary graph, calls its native validation, and uses
//! its native signature and executor. The records here retain a digest witness
//! for that native signature so a durable semantic record cannot erase the
//! structural boundary it describes.

use serde::{Deserialize, Serialize};

use super::encoding::content_digest;
use super::evidence::canonical_provenance;
use super::{
    AdmissionAttemptId, CanonicalRecord, CompositionBlockKey, CompositionCatalogKey,
    CompositionRecipeKey, ContentDigest, ContractError, ExclusionCode, LogicalSequence,
    OutcomeObservationId, PolicyRef, Provenance, RecordedAt, ResolvedConfigurationRef, RunId,
    SchemaVersion, SelectionDecisionId, SourceKey,
};
use crate::composition::{GatePath, Refusal, RefusedKey};
use crate::topology::TopologyContractKey;

/// Largest legal number of ordered blocks in one serialisable composition.
///
/// This is a bounded description limit, not a claim that a host can execute a
/// runtime-sized map, fan-in, or generated graph.
pub const MAX_COMPOSITION_BLOCKS: usize = 16;

/// A positive, one-based attempt number within one planner revision loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionAttempt(u8);

impl RevisionAttempt {
    /// Creates a one-based revision-attempt number.
    pub fn new(value: u8) -> Result<Self, ContractError> {
        if value == 0 {
            return Err(ContractError::new(
                "composition.invalid-revision-attempt",
                "a revision attempt must be one or greater",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the one-based revision-attempt number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Canonical identity of a gate-expression shape with configuration values omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GateExpressionShapeKey(ContentDigest);

impl GateExpressionShapeKey {
    /// Creates a shape key from canonical, configuration-independent bytes.
    #[must_use]
    pub const fn new(digest: ContentDigest) -> Self {
        Self(digest)
    }

    /// Returns the canonical digest of the expression shape.
    #[must_use]
    pub const fn digest(self) -> ContentDigest {
        self.0
    }
}

/// Durable gate-plan identity shared by decision, validation, run, and outcome records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePlanRecord {
    plan_digest: ContentDigest,
    catalogue_digest: ContentDigest,
    shape: GateExpressionShapeKey,
}

/// Serializable planner-actionable refusal retained by a refused validation.
///
/// This owned form retains every materializer detail needed to revise a whole
/// plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedRefusal {
    code: String,
    path: GatePath,
    key: Option<RefusedKey>,
    expected: String,
    actual: String,
    repair_hint: String,
}

impl RecordedRefusal {
    /// Copies one materializer refusal into durable evidence.
    #[must_use]
    pub fn from_refusal(refusal: &Refusal) -> Self {
        Self {
            code: refusal.code().to_owned(),
            path: refusal.path().clone(),
            key: refusal.key().cloned(),
            expected: refusal.expected().to_owned(),
            actual: refusal.actual().to_owned(),
            repair_hint: refusal.repair_hint().to_owned(),
        }
    }

    /// Returns the stable machine-readable refusal code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the path to the failed gate.
    #[must_use]
    pub const fn path(&self) -> &GatePath {
        &self.path
    }

    /// Returns the involved key or plan-local label, when one applies.
    #[must_use]
    pub const fn key(&self) -> Option<&RefusedKey> {
        self.key.as_ref()
    }

    /// Returns the requirement the plan failed to satisfy.
    #[must_use]
    pub fn expected(&self) -> &str {
        &self.expected
    }

    /// Returns the observed conflicting value or graph fact.
    #[must_use]
    pub fn actual(&self) -> &str {
        &self.actual
    }

    /// Returns the actionable repair instruction.
    #[must_use]
    pub fn repair_hint(&self) -> &str {
        &self.repair_hint
    }
}

impl GatePlanRecord {
    /// Creates the immutable identity retained for one gate plan.
    #[must_use]
    pub const fn new(
        plan_digest: ContentDigest,
        catalogue_digest: ContentDigest,
        shape: GateExpressionShapeKey,
    ) -> Self {
        Self {
            plan_digest,
            catalogue_digest,
            shape,
        }
    }

    /// Returns the complete canonical gate-plan digest.
    #[must_use]
    pub const fn plan_digest(self) -> ContentDigest {
        self.plan_digest
    }

    /// Returns the sealed catalogue snapshot digest used to resolve the plan.
    #[must_use]
    pub const fn catalogue_digest(self) -> ContentDigest {
        self.catalogue_digest
    }

    /// Returns the configuration-independent expression-shape key.
    #[must_use]
    pub const fn shape(self) -> GateExpressionShapeKey {
        self.shape
    }
}

/// One versioned block and its immutable resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionBlockRef {
    block: CompositionBlockKey,
    configuration: ResolvedConfigurationRef,
}

impl CompositionBlockRef {
    /// Creates a reference to one approved composition block.
    pub fn new(
        block: CompositionBlockKey,
        configuration: ResolvedConfigurationRef,
    ) -> Result<Self, ContractError> {
        configuration.validate()?;
        Ok(Self {
            block,
            configuration,
        })
    }

    /// Returns the block identity and semantic version.
    #[must_use]
    pub const fn block(&self) -> &CompositionBlockKey {
        &self.block
    }

    /// Returns the immutable resolved configuration reference.
    #[must_use]
    pub const fn configuration(&self) -> &ResolvedConfigurationRef {
        &self.configuration
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.configuration.validate()
    }
}

/// The selected recipe and its immutable resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionRecipeRef {
    recipe: CompositionRecipeKey,
    configuration: ResolvedConfigurationRef,
}

impl CompositionRecipeRef {
    /// Creates a reference to one approved finite composition recipe.
    pub fn new(
        recipe: CompositionRecipeKey,
        configuration: ResolvedConfigurationRef,
    ) -> Result<Self, ContractError> {
        configuration.validate()?;
        Ok(Self {
            recipe,
            configuration,
        })
    }

    /// Returns the recipe identity and semantic version.
    #[must_use]
    pub const fn recipe(&self) -> &CompositionRecipeKey {
        &self.recipe
    }

    /// Returns the immutable resolved configuration reference.
    #[must_use]
    pub const fn configuration(&self) -> &ResolvedConfigurationRef {
        &self.configuration
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.configuration.validate()
    }
}

/// A finite choice over a sealed recipe catalogue, without a native graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionPlan {
    schema_version: SchemaVersion,
    catalogue: CompositionCatalogKey,
    catalogue_digest: ContentDigest,
    recipe: CompositionRecipeRef,
    blocks: Vec<CompositionBlockRef>,
    composition_digest: ContentDigest,
}

impl CompositionPlan {
    /// Creates a bounded ordered composition and its canonical identity.
    pub fn new(
        catalogue: CompositionCatalogKey,
        catalogue_digest: ContentDigest,
        recipe: CompositionRecipeRef,
        blocks: Vec<CompositionBlockRef>,
    ) -> Result<Self, ContractError> {
        let mut plan = Self {
            schema_version: SchemaVersion::V1,
            catalogue,
            catalogue_digest,
            recipe,
            blocks,
            composition_digest: ContentDigest::sha256(&[]),
        };
        plan.validate_structure()?;
        plan.composition_digest = content_digest(&plan.identity_view())?;
        Ok(plan)
    }

    /// Returns the sealed catalogue identity and version.
    #[must_use]
    pub const fn catalogue(&self) -> &CompositionCatalogKey {
        &self.catalogue
    }

    /// Returns the immutable snapshot digest of that catalogue.
    #[must_use]
    pub const fn catalogue_digest(&self) -> ContentDigest {
        self.catalogue_digest
    }

    /// Returns the approved recipe identity, version, and configuration.
    #[must_use]
    pub const fn recipe(&self) -> &CompositionRecipeRef {
        &self.recipe
    }

    /// Returns the recipe-ordered approved block references.
    #[must_use]
    pub fn blocks(&self) -> &[CompositionBlockRef] {
        &self.blocks
    }

    /// Returns the canonical identity shared by all composition evidence.
    #[must_use]
    pub const fn composition_digest(&self) -> ContentDigest {
        self.composition_digest
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.recipe.validate()?;
        if self.blocks.is_empty() || self.blocks.len() > MAX_COMPOSITION_BLOCKS {
            return Err(ContractError::new(
                "composition.invalid-block-count",
                format!(
                    "a composition requires 1..={MAX_COMPOSITION_BLOCKS} ordered approved blocks"
                ),
            ));
        }
        for block in &self.blocks {
            block.validate()?;
        }
        Ok(())
    }

    fn identity_view(&self) -> CompositionPlanIdentity<'_> {
        CompositionPlanIdentity {
            schema_version: self.schema_version,
            catalogue: &self.catalogue,
            catalogue_digest: self.catalogue_digest,
            recipe: &self.recipe,
            blocks: &self.blocks,
        }
    }
}

#[derive(Serialize)]
struct CompositionPlanIdentity<'a> {
    schema_version: SchemaVersion,
    catalogue: &'a CompositionCatalogKey,
    catalogue_digest: ContentDigest,
    recipe: &'a CompositionRecipeRef,
    blocks: &'a [CompositionBlockRef],
}

impl CanonicalRecord for CompositionPlan {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        let expected = content_digest(&self.identity_view())?;
        if expected == self.composition_digest {
            Ok(())
        } else {
            Err(ContractError::new(
                "composition.digest-mismatch",
                "composition identity does not match its canonical plan",
            ))
        }
    }
}

/// Why a valid bounded composition was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompositionChoiceOrigin {
    /// A replaceable policy chose the plan from the sealed catalogue.
    Autonomous {
        /// The versioned policy responsible for the choice.
        policy: PolicyRef,
    },
    /// A caller or experiment explicitly requested the plan.
    Forced {
        /// The source that issued the explicit direction.
        source: SourceKey,
    },
}

/// A composition plan paired with its autonomous or forced choice provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionChoice {
    plan: CompositionPlan,
    origin: CompositionChoiceOrigin,
}

impl CompositionChoice {
    /// Creates a typed composition choice.
    pub fn new(
        plan: CompositionPlan,
        origin: CompositionChoiceOrigin,
    ) -> Result<Self, ContractError> {
        plan.validate()?;
        Ok(Self { plan, origin })
    }

    /// Returns the selected bounded plan.
    #[must_use]
    pub const fn plan(&self) -> &CompositionPlan {
        &self.plan
    }

    /// Returns whether this choice was autonomous or forced.
    #[must_use]
    pub const fn origin(&self) -> &CompositionChoiceOrigin {
        &self.origin
    }

    fn validate(&self) -> Result<(), ContractError> {
        self.plan.validate()
    }
}

/// The pre-materialisation composition decision recorded for one selection event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionDecision {
    schema_version: SchemaVersion,
    decision_id: SelectionDecisionId,
    recorded_at: RecordedAt,
    choice: CompositionChoice,
    provenance: Vec<Provenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
    content_digest: ContentDigest,
}

impl CompositionDecision {
    /// Creates a canonical composition decision before native materialisation.
    pub fn new(
        decision_id: SelectionDecisionId,
        recorded_at: RecordedAt,
        choice: CompositionChoice,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        let mut decision = Self {
            schema_version: SchemaVersion::V1,
            decision_id,
            recorded_at,
            choice,
            provenance: canonical_provenance(provenance)?,
            gate_plan: None,
            revision_attempt: None,
            content_digest: ContentDigest::sha256(&[]),
        };
        decision.validate_structure()?;
        decision.content_digest = content_digest(&decision.content_view())?;
        Ok(decision)
    }

    /// Returns the decision event identity shared with topology evidence.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns its authoritative evidence-store position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns the bounded plan and its choice provenance.
    #[must_use]
    pub const fn choice(&self) -> &CompositionChoice {
        &self.choice
    }

    /// Returns the canonical composition identity.
    #[must_use]
    pub const fn composition_digest(&self) -> ContentDigest {
        self.choice.plan.composition_digest()
    }

    /// Returns choice provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    /// Attaches a gate-plan identity and revision number to this decision.
    ///
    /// A decision without gate-plan metadata omits it from its canonical
    /// encoding.
    pub fn with_gate_plan(
        mut self,
        gate_plan: GatePlanRecord,
        revision_attempt: RevisionAttempt,
    ) -> Result<Self, ContractError> {
        self.gate_plan = Some(gate_plan);
        self.revision_attempt = Some(revision_attempt);
        self.content_digest = content_digest(&self.content_view())?;
        Ok(self)
    }

    /// Returns the retained gate-plan identity, when this is a gate-plan decision.
    #[must_use]
    pub const fn gate_plan(&self) -> Option<GatePlanRecord> {
        self.gate_plan
    }

    /// Returns this decision's revision-loop position, when retained.
    #[must_use]
    pub const fn revision_attempt(&self) -> Option<RevisionAttempt> {
        self.revision_attempt
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.choice.validate()?;
        if self.gate_plan.is_some() != self.revision_attempt.is_some() {
            return Err(ContractError::new(
                "composition.incomplete-gate-plan-record",
                "gate-plan evidence requires both a plan identity and revision attempt",
            ));
        }
        if self.provenance.is_empty() {
            return Err(ContractError::new(
                "composition.missing-provenance",
                "a composition decision requires provenance",
            ));
        }
        canonical_provenance(self.provenance.clone()).map(|_| ())
    }

    fn content_view(&self) -> CompositionDecisionContent<'_> {
        CompositionDecisionContent {
            schema_version: self.schema_version,
            decision_id: self.decision_id,
            sequence: self.recorded_at.sequence(),
            choice: &self.choice,
            provenance: &self.provenance,
            gate_plan: self.gate_plan,
            revision_attempt: self.revision_attempt,
        }
    }
}

#[derive(Serialize)]
struct CompositionDecisionContent<'a> {
    schema_version: SchemaVersion,
    decision_id: SelectionDecisionId,
    sequence: LogicalSequence,
    choice: &'a CompositionChoice,
    provenance: &'a [Provenance],
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
}

impl CanonicalRecord for CompositionDecision {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "composition decision",
        )
    }
}

/// A digest witness of a graph signature derived by the native graph runtime.
///
/// It is an audit reference only. It cannot be converted into a native
/// `GraphSignature`, satisfy a graph boundary, or replace `Graph::validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeGraphSignatureWitness {
    contract: TopologyContractKey,
    signature_digest: ContentDigest,
}

impl NativeGraphSignatureWitness {
    /// Creates a witness after the host derives the ordinary graph signature.
    #[must_use]
    pub const fn new(contract: TopologyContractKey, signature_digest: ContentDigest) -> Self {
        Self {
            contract,
            signature_digest,
        }
    }

    /// Returns the semantic contract associated with the native graph.
    #[must_use]
    pub const fn contract(&self) -> &TopologyContractKey {
        &self.contract
    }

    /// Returns the host-produced digest of the derived native signature.
    #[must_use]
    pub const fn signature_digest(&self) -> ContentDigest {
        self.signature_digest
    }
}

/// Result of native graph validation and host admission for a composition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompositionValidationDisposition {
    /// Native graph validation and the host's admission boundary both accepted it.
    Admitted,
    /// Materialisation or admission refused the plan before a graph started.
    Refused {
        /// Stable stage name such as `native-validation` or `live-admission`.
        stage: ExclusionCode,
        /// Stable refusal code supplied by the owning native or host boundary.
        reason: ExclusionCode,
    },
}

impl CompositionValidationDisposition {
    /// Returns whether a graph may be started.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }
}

/// Validation and admission evidence for a single materialised composition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionValidation {
    schema_version: SchemaVersion,
    attempt_id: AdmissionAttemptId,
    decision_id: SelectionDecisionId,
    composition_digest: ContentDigest,
    recorded_at: RecordedAt,
    native_signature: NativeGraphSignatureWitness,
    disposition: CompositionValidationDisposition,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    refusals: Vec<RecordedRefusal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
    content_digest: ContentDigest,
}

impl CompositionValidation {
    /// Records validation and admission after a composition decision.
    pub fn new(
        attempt_id: AdmissionAttemptId,
        decision: &CompositionDecision,
        recorded_at: RecordedAt,
        native_signature: NativeGraphSignatureWitness,
        disposition: CompositionValidationDisposition,
    ) -> Result<Self, ContractError> {
        Self::new_with_refusals(
            attempt_id,
            decision,
            recorded_at,
            native_signature,
            disposition,
            Vec::new(),
        )
    }

    /// Records a refused gate-plan validation with actionable materializer reasons.
    pub fn refused(
        attempt_id: AdmissionAttemptId,
        decision: &CompositionDecision,
        recorded_at: RecordedAt,
        native_signature: NativeGraphSignatureWitness,
        stage: ExclusionCode,
        reason: ExclusionCode,
        refusals: Vec<Refusal>,
    ) -> Result<Self, ContractError> {
        Self::new_with_refusals(
            attempt_id,
            decision,
            recorded_at,
            native_signature,
            CompositionValidationDisposition::Refused { stage, reason },
            refusals,
        )
    }

    fn new_with_refusals(
        attempt_id: AdmissionAttemptId,
        decision: &CompositionDecision,
        recorded_at: RecordedAt,
        native_signature: NativeGraphSignatureWitness,
        disposition: CompositionValidationDisposition,
        refusals: Vec<Refusal>,
    ) -> Result<Self, ContractError> {
        if recorded_at.sequence() <= decision.recorded_at().sequence() {
            return Err(ContractError::new(
                "composition.validation-order",
                "composition validation must follow its decision in store order",
            ));
        }
        let mut validation = Self {
            schema_version: SchemaVersion::V1,
            attempt_id,
            decision_id: decision.decision_id(),
            composition_digest: decision.composition_digest(),
            recorded_at,
            native_signature,
            disposition,
            refusals: refusals.iter().map(RecordedRefusal::from_refusal).collect(),
            gate_plan: decision.gate_plan(),
            revision_attempt: decision.revision_attempt(),
            content_digest: ContentDigest::sha256(&[]),
        };
        validation.validate_structure()?;
        validation.content_digest = content_digest(&validation.content_view())?;
        Ok(validation)
    }

    /// Returns the shared live-admission attempt identity.
    #[must_use]
    pub const fn attempt_id(&self) -> AdmissionAttemptId {
        self.attempt_id
    }

    /// Returns the originating decision identity.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns the canonical composition identity.
    #[must_use]
    pub const fn composition_digest(&self) -> ContentDigest {
        self.composition_digest
    }

    /// Returns native signature audit information without serialising its graph.
    #[must_use]
    pub const fn native_signature(&self) -> &NativeGraphSignatureWitness {
        &self.native_signature
    }

    /// Returns the validation and admission disposition.
    #[must_use]
    pub const fn disposition(&self) -> &CompositionValidationDisposition {
        &self.disposition
    }

    /// Returns whether the composition can proceed to a run.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        self.disposition.is_admitted()
    }

    /// Returns its authoritative evidence-store position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    /// Retains planner-actionable refusals for a refused validation.
    pub fn with_refusals(mut self, refusals: Vec<Refusal>) -> Result<Self, ContractError> {
        if self.is_admitted() {
            return Err(ContractError::new(
                "composition.admitted-refusals",
                "an admitted validation cannot retain refusal reasons",
            ));
        }
        self.refusals = refusals.iter().map(RecordedRefusal::from_refusal).collect();
        self.validate_structure()?;
        self.content_digest = content_digest(&self.content_view())?;
        Ok(self)
    }

    /// Returns planner-actionable refusals in their materializer order.
    #[must_use]
    pub fn refusals(&self) -> &[RecordedRefusal] {
        &self.refusals
    }

    /// Returns the retained gate-plan identity, when this is gate-plan evidence.
    #[must_use]
    pub const fn gate_plan(&self) -> Option<GatePlanRecord> {
        self.gate_plan
    }

    /// Returns this validation's revision-loop position, when retained.
    #[must_use]
    pub const fn revision_attempt(&self) -> Option<RevisionAttempt> {
        self.revision_attempt
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        if self.gate_plan.is_some() != self.revision_attempt.is_some() {
            return Err(ContractError::new(
                "composition.incomplete-gate-plan-record",
                "gate-plan evidence requires both a plan identity and revision attempt",
            ));
        }
        if self.is_admitted() && !self.refusals.is_empty() {
            return Err(ContractError::new(
                "composition.admitted-refusals",
                "an admitted validation cannot retain refusal reasons",
            ));
        }
        if self.gate_plan.is_some() && !self.is_admitted() && self.refusals.is_empty() {
            return Err(ContractError::new(
                "composition.gate-plan-refusal-missing",
                "a refused gate-plan validation requires at least one actionable refusal",
            ));
        }
        Ok(())
    }

    fn content_view(&self) -> CompositionValidationContent<'_> {
        CompositionValidationContent {
            schema_version: self.schema_version,
            attempt_id: self.attempt_id,
            decision_id: self.decision_id,
            composition_digest: self.composition_digest,
            sequence: self.recorded_at.sequence(),
            native_signature: &self.native_signature,
            disposition: &self.disposition,
            refusals: &self.refusals,
            gate_plan: self.gate_plan,
            revision_attempt: self.revision_attempt,
        }
    }
}

#[derive(Serialize)]
struct CompositionValidationContent<'a> {
    schema_version: SchemaVersion,
    attempt_id: AdmissionAttemptId,
    decision_id: SelectionDecisionId,
    composition_digest: ContentDigest,
    sequence: LogicalSequence,
    native_signature: &'a NativeGraphSignatureWitness,
    disposition: &'a CompositionValidationDisposition,
    #[serde(skip_serializing_if = "slice_is_empty")]
    refusals: &'a [RecordedRefusal],
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
}

impl CanonicalRecord for CompositionValidation {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "composition validation",
        )
    }
}

/// Run-opening evidence for an admitted materialised composition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionRun {
    schema_version: SchemaVersion,
    run_id: RunId,
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
    composition_digest: ContentDigest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
    recorded_at: RecordedAt,
    content_digest: ContentDigest,
}

impl CompositionRun {
    /// Opens a composition run only after a matching successful validation.
    pub fn new(
        run_id: RunId,
        validation: &CompositionValidation,
        recorded_at: RecordedAt,
    ) -> Result<Self, ContractError> {
        if !validation.is_admitted() {
            return Err(ContractError::new(
                "composition.run-without-admission",
                "a refused composition cannot open a run",
            ));
        }
        if recorded_at.sequence() <= validation.recorded_at().sequence() {
            return Err(ContractError::new(
                "composition.run-order",
                "a composition run must follow successful validation in store order",
            ));
        }
        let mut run = Self {
            schema_version: SchemaVersion::V1,
            run_id,
            decision_id: validation.decision_id(),
            attempt_id: validation.attempt_id(),
            composition_digest: validation.composition_digest(),
            gate_plan: validation.gate_plan(),
            revision_attempt: validation.revision_attempt(),
            recorded_at,
            content_digest: ContentDigest::sha256(&[]),
        };
        run.validate_structure()?;
        run.content_digest = content_digest(&run.content_view())?;
        Ok(run)
    }

    /// Returns the shared started-run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the originating decision identity.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns the admission attempt that authorized this start.
    #[must_use]
    pub const fn attempt_id(&self) -> AdmissionAttemptId {
        self.attempt_id
    }

    /// Returns the canonical composition identity.
    #[must_use]
    pub const fn composition_digest(&self) -> ContentDigest {
        self.composition_digest
    }

    /// Returns the retained gate-plan identity, when this is gate-plan evidence.
    #[must_use]
    pub const fn gate_plan(&self) -> Option<GatePlanRecord> {
        self.gate_plan
    }

    /// Returns this run's revision-loop position, when retained.
    #[must_use]
    pub const fn revision_attempt(&self) -> Option<RevisionAttempt> {
        self.revision_attempt
    }

    /// Returns its authoritative evidence-store position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        if self.gate_plan.is_some() != self.revision_attempt.is_some() {
            return Err(ContractError::new(
                "composition.incomplete-gate-plan-record",
                "gate-plan evidence requires both a plan identity and revision attempt",
            ));
        }
        Ok(())
    }

    fn content_view(&self) -> CompositionRunContent {
        CompositionRunContent {
            schema_version: self.schema_version,
            run_id: self.run_id,
            decision_id: self.decision_id,
            attempt_id: self.attempt_id,
            composition_digest: self.composition_digest,
            gate_plan: self.gate_plan,
            revision_attempt: self.revision_attempt,
            sequence: self.recorded_at.sequence(),
        }
    }
}

#[derive(Serialize)]
struct CompositionRunContent {
    schema_version: SchemaVersion,
    run_id: RunId,
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
    composition_digest: ContentDigest,
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
    sequence: LogicalSequence,
}

impl CanonicalRecord for CompositionRun {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "composition run",
        )
    }
}

/// A delayed registered-topology outcome linked to a composition run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionOutcome {
    schema_version: SchemaVersion,
    observation_id: OutcomeObservationId,
    run_id: RunId,
    decision_id: SelectionDecisionId,
    composition_digest: ContentDigest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
    recorded_at: RecordedAt,
    content_digest: ContentDigest,
}

impl CompositionOutcome {
    /// Links a later ordinary outcome observation to the composition that ran.
    pub fn new(
        observation_id: OutcomeObservationId,
        run: &CompositionRun,
        recorded_at: RecordedAt,
    ) -> Result<Self, ContractError> {
        if recorded_at.sequence() <= run.recorded_at().sequence() {
            return Err(ContractError::new(
                "composition.outcome-order",
                "a delayed composition outcome must follow its run in store order",
            ));
        }
        let mut outcome = Self {
            schema_version: SchemaVersion::V1,
            observation_id,
            run_id: run.run_id(),
            decision_id: run.decision_id(),
            composition_digest: run.composition_digest(),
            gate_plan: run.gate_plan(),
            revision_attempt: run.revision_attempt(),
            recorded_at,
            content_digest: ContentDigest::sha256(&[]),
        };
        outcome.validate_structure()?;
        outcome.content_digest = content_digest(&outcome.content_view())?;
        Ok(outcome)
    }

    /// Returns the ordinary delayed-outcome event identity.
    #[must_use]
    pub const fn observation_id(&self) -> OutcomeObservationId {
        self.observation_id
    }

    /// Returns the linked ordinary run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the originating decision identity.
    #[must_use]
    pub const fn decision_id(&self) -> SelectionDecisionId {
        self.decision_id
    }

    /// Returns the canonical composition identity.
    #[must_use]
    pub const fn composition_digest(&self) -> ContentDigest {
        self.composition_digest
    }

    /// Returns the retained gate-plan identity, when this is gate-plan evidence.
    #[must_use]
    pub const fn gate_plan(&self) -> Option<GatePlanRecord> {
        self.gate_plan
    }

    /// Returns this outcome's revision-loop position, when retained.
    #[must_use]
    pub const fn revision_attempt(&self) -> Option<RevisionAttempt> {
        self.revision_attempt
    }

    /// Returns its authoritative evidence-store position.
    #[must_use]
    pub const fn recorded_at(&self) -> RecordedAt {
        self.recorded_at
    }

    /// Returns the canonical event digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        if self.gate_plan.is_some() != self.revision_attempt.is_some() {
            return Err(ContractError::new(
                "composition.incomplete-gate-plan-record",
                "gate-plan evidence requires both a plan identity and revision attempt",
            ));
        }
        Ok(())
    }

    fn content_view(&self) -> CompositionOutcomeContent {
        CompositionOutcomeContent {
            schema_version: self.schema_version,
            observation_id: self.observation_id,
            run_id: self.run_id,
            decision_id: self.decision_id,
            composition_digest: self.composition_digest,
            gate_plan: self.gate_plan,
            revision_attempt: self.revision_attempt,
            sequence: self.recorded_at.sequence(),
        }
    }
}

#[derive(Serialize)]
struct CompositionOutcomeContent {
    schema_version: SchemaVersion,
    observation_id: OutcomeObservationId,
    run_id: RunId,
    decision_id: SelectionDecisionId,
    composition_digest: ContentDigest,
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_plan: Option<GatePlanRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_attempt: Option<RevisionAttempt>,
    sequence: LogicalSequence,
}

impl CanonicalRecord for CompositionOutcome {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "composition outcome",
        )
    }
}

/// Ordered composition evidence, linked to the existing registered-topology IDs.
///
/// This is a serialisable compatibility envelope, not an evidence store or a
/// second executor. A host appends the corresponding ordinary selection,
/// admission, run, and outcome records through its existing evidence provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionEvidence {
    schema_version: SchemaVersion,
    decision: CompositionDecision,
    validation: CompositionValidation,
    run: CompositionRun,
    outcome: CompositionOutcome,
    content_digest: ContentDigest,
}

impl CompositionEvidence {
    /// Joins one complete, ordered composition evidence path.
    pub fn new(
        decision: CompositionDecision,
        validation: CompositionValidation,
        run: CompositionRun,
        outcome: CompositionOutcome,
    ) -> Result<Self, ContractError> {
        let mut evidence = Self {
            schema_version: SchemaVersion::V1,
            decision,
            validation,
            run,
            outcome,
            content_digest: ContentDigest::sha256(&[]),
        };
        evidence.validate_structure()?;
        evidence.content_digest = content_digest(&evidence.content_view())?;
        Ok(evidence)
    }

    /// Returns the initial choice record.
    #[must_use]
    pub const fn decision(&self) -> &CompositionDecision {
        &self.decision
    }

    /// Returns validation and admission evidence.
    #[must_use]
    pub const fn validation(&self) -> &CompositionValidation {
        &self.validation
    }

    /// Returns the admitted run-opening evidence.
    #[must_use]
    pub const fn run(&self) -> &CompositionRun {
        &self.run
    }

    /// Returns the linked delayed outcome.
    #[must_use]
    pub const fn outcome(&self) -> &CompositionOutcome {
        &self.outcome
    }

    /// Returns the one identity shared by every event in the path.
    #[must_use]
    pub const fn composition_digest(&self) -> ContentDigest {
        self.decision.composition_digest()
    }

    /// Returns the canonical envelope digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.decision.validate()?;
        self.validation.validate()?;
        self.run.validate()?;
        self.outcome.validate()?;
        let identity = self.decision.composition_digest();
        if self.validation.decision_id != self.decision.decision_id
            || self.run.decision_id != self.decision.decision_id
            || self.outcome.decision_id != self.decision.decision_id
            || self.validation.composition_digest != identity
            || self.run.composition_digest != identity
            || self.outcome.composition_digest != identity
            || self.run.attempt_id != self.validation.attempt_id
            || self.outcome.run_id != self.run.run_id
        {
            return Err(ContractError::new(
                "composition.evidence-link-mismatch",
                "composition evidence events must retain the same IDs and composition identity",
            ));
        }
        let decision = self.decision.recorded_at.sequence();
        let validation = self.validation.recorded_at.sequence();
        let run = self.run.recorded_at.sequence();
        let outcome = self.outcome.recorded_at.sequence();
        if !(decision < validation && validation < run && run < outcome) {
            return Err(ContractError::new(
                "composition.evidence-order",
                "composition evidence must be decision, validation, run, then delayed outcome",
            ));
        }
        if !self.validation.is_admitted() {
            return Err(ContractError::new(
                "composition.evidence-refused-run",
                "a complete composition evidence path cannot run after refusal",
            ));
        }
        Ok(())
    }

    fn content_view(&self) -> CompositionEvidenceContent<'_> {
        CompositionEvidenceContent {
            schema_version: self.schema_version,
            decision: &self.decision,
            validation: &self.validation,
            run: &self.run,
            outcome: &self.outcome,
        }
    }
}

#[derive(Serialize)]
struct CompositionEvidenceContent<'a> {
    schema_version: SchemaVersion,
    decision: &'a CompositionDecision,
    validation: &'a CompositionValidation,
    run: &'a CompositionRun,
    outcome: &'a CompositionOutcome,
}

impl CanonicalRecord for CompositionEvidence {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "composition evidence",
        )
    }
}

/// One decision-to-validation pair in a gate-plan revision loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionRevisionEvidence {
    decision: CompositionDecision,
    validation: CompositionValidation,
}

impl CompositionRevisionEvidence {
    /// Joins a gate-plan decision to its immediately following validation.
    pub fn new(
        decision: CompositionDecision,
        validation: CompositionValidation,
    ) -> Result<Self, ContractError> {
        let value = Self {
            decision,
            validation,
        };
        value.validate_structure()?;
        Ok(value)
    }

    /// Returns the planner decision for this revision attempt.
    #[must_use]
    pub const fn decision(&self) -> &CompositionDecision {
        &self.decision
    }

    /// Returns the materialization and admission result for this attempt.
    #[must_use]
    pub const fn validation(&self) -> &CompositionValidation {
        &self.validation
    }

    /// Returns the one-based revision-loop position.
    #[must_use]
    pub const fn revision_attempt(&self) -> RevisionAttempt {
        self.decision
            .revision_attempt()
            .expect("validated gate-plan revision always has an attempt number")
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.decision.validate()?;
        self.validation.validate()?;
        let Some(gate_plan) = self.decision.gate_plan() else {
            return Err(ContractError::new(
                "composition.missing-gate-plan-record",
                "a revision-loop decision must retain a gate-plan identity",
            ));
        };
        let Some(revision_attempt) = self.decision.revision_attempt() else {
            return Err(ContractError::new(
                "composition.missing-revision-attempt",
                "a revision-loop decision must retain its attempt number",
            ));
        };
        if self.validation.decision_id() != self.decision.decision_id()
            || self.validation.gate_plan() != Some(gate_plan)
            || self.validation.revision_attempt() != Some(revision_attempt)
            || self.validation.recorded_at().sequence() <= self.decision.recorded_at().sequence()
        {
            return Err(ContractError::new(
                "composition.revision-link-mismatch",
                "revision validation must immediately follow and retain its gate-plan decision",
            ));
        }
        Ok(())
    }
}

/// Ordered gate-plan revisions with at most one admitted run and outcome.
///
/// A planner can submit a new complete plan after each refusal. Every earlier
/// pair must therefore be refused; only the last admitted pair can authorize a
/// run. This keeps a run after a refusal from being mistaken for an admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionRevisionTrail {
    schema_version: SchemaVersion,
    revisions: Vec<CompositionRevisionEvidence>,
    run: Option<CompositionRun>,
    outcome: Option<CompositionOutcome>,
    content_digest: ContentDigest,
}

impl CompositionRevisionTrail {
    /// Creates an ordered revision trail, optionally completed by one run and outcome.
    pub fn new(
        revisions: Vec<CompositionRevisionEvidence>,
        run: Option<CompositionRun>,
        outcome: Option<CompositionOutcome>,
    ) -> Result<Self, ContractError> {
        let mut value = Self {
            schema_version: SchemaVersion::V1,
            revisions,
            run,
            outcome,
            content_digest: ContentDigest::sha256(&[]),
        };
        value.validate_structure()?;
        value.content_digest = content_digest(&value.content_view())?;
        Ok(value)
    }

    /// Returns decision-to-validation pairs in planner revision order.
    #[must_use]
    pub fn revisions(&self) -> &[CompositionRevisionEvidence] {
        &self.revisions
    }

    /// Returns the only permitted run, when a final attempt was admitted.
    #[must_use]
    pub const fn run(&self) -> Option<&CompositionRun> {
        self.run.as_ref()
    }

    /// Returns the only permitted delayed outcome, when retained.
    #[must_use]
    pub const fn outcome(&self) -> Option<&CompositionOutcome> {
        self.outcome.as_ref()
    }

    /// Returns the canonical revision-trail digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        if self.revisions.is_empty() {
            return Err(ContractError::new(
                "composition.empty-revision-trail",
                "a gate-plan revision trail requires at least one attempt",
            ));
        }
        if self.revisions[0].revision_attempt().get() != 1 {
            return Err(ContractError::new(
                "composition.revision-first-attempt",
                "a revision trail must begin with attempt one",
            ));
        }
        for (index, revision) in self.revisions.iter().enumerate() {
            revision.validate_structure()?;
            if index > 0 {
                let previous = &self.revisions[index - 1];
                if Some(revision.revision_attempt().get())
                    != previous.revision_attempt().get().checked_add(1)
                    || revision.decision().recorded_at().sequence()
                        <= previous.validation().recorded_at().sequence()
                    || previous.validation().is_admitted()
                {
                    return Err(ContractError::new(
                        "composition.revision-order",
                        "revisions must be consecutive refused decision-validation pairs",
                    ));
                }
            }
        }
        let final_revision = self.revisions.last().expect("checked non-empty revisions");
        match (&self.run, &self.outcome) {
            (None, None) => Ok(()),
            (None, Some(_)) => Err(ContractError::new(
                "composition.outcome-without-run",
                "a composition outcome requires the one admitted run",
            )),
            (Some(run), outcome) => {
                run.validate()?;
                if !final_revision.validation().is_admitted()
                    || run.decision_id() != final_revision.decision().decision_id()
                    || run.attempt_id() != final_revision.validation().attempt_id()
                    || run.gate_plan() != final_revision.validation().gate_plan()
                    || run.revision_attempt() != final_revision.validation().revision_attempt()
                    || run.recorded_at().sequence()
                        <= final_revision.validation().recorded_at().sequence()
                {
                    return Err(ContractError::new(
                        "composition.run-without-final-admission",
                        "a revision trail can run only after its final admitted validation",
                    ));
                }
                if let Some(outcome) = outcome {
                    outcome.validate()?;
                    if outcome.run_id() != run.run_id()
                        || outcome.gate_plan() != run.gate_plan()
                        || outcome.revision_attempt() != run.revision_attempt()
                        || outcome.recorded_at().sequence() <= run.recorded_at().sequence()
                    {
                        return Err(ContractError::new(
                            "composition.outcome-link-mismatch",
                            "a revision-trail outcome must follow and retain its one run",
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    fn content_view(&self) -> CompositionRevisionTrailContent<'_> {
        CompositionRevisionTrailContent {
            schema_version: self.schema_version,
            revisions: &self.revisions,
            run: self.run.as_ref(),
            outcome: self.outcome.as_ref(),
        }
    }
}

#[derive(Serialize)]
struct CompositionRevisionTrailContent<'a> {
    schema_version: SchemaVersion,
    revisions: &'a [CompositionRevisionEvidence],
    run: Option<&'a CompositionRun>,
    outcome: Option<&'a CompositionOutcome>,
}

impl CanonicalRecord for CompositionRevisionTrail {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            content_digest(&self.content_view())?,
            "composition revision trail",
        )
    }
}

fn slice_is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}

fn require_digest(
    actual: ContentDigest,
    expected: ContentDigest,
    record: &'static str,
) -> Result<(), ContractError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ContractError::new(
            "identity.digest-mismatch",
            format!("{record} does not match its canonical content"),
        ))
    }
}
