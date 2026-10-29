//! Closed task-profile and objective records used by every selector point.

use serde::{Deserialize, Serialize};

use super::encoding::content_digest;
use super::evidence::canonical_provenance;
use super::{
    CanonicalRecord, CapabilityId, CardinalityEstimate, ContentDigest, ContractError, DataPathId,
    DurationMs, ExternalSessionRef, FeatureAssessment, Money, PolicyRef, PreferenceBasisPoints,
    Provenance, ProviderId, TaskId, TenancyScopeId, TokenQuantity,
};

/// Shape of independently executable work in a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecompositionShape {
    /// No meaningful decomposition is known.
    Indivisible,
    /// Work must proceed in sequence.
    Sequential,
    /// Work contains independently executable parts.
    IndependentParts,
    /// Parts can be separated but depend on one another.
    InterdependentParts,
    /// The task mixes independent and dependent work.
    Mixed,
}

/// Bounded strength of a task need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NeedLevel {
    /// The capability is not needed.
    None,
    /// The capability is weakly useful.
    Low,
    /// The capability is materially useful.
    Medium,
    /// The capability is central to the task.
    High,
}

/// Consequence of producing a poor task result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsequenceLevel {
    /// Failure has negligible consequence.
    Negligible,
    /// Failure has low consequence.
    Low,
    /// Failure has moderate consequence.
    Moderate,
    /// Failure has high consequence.
    High,
    /// Failure has critical consequence.
    Critical,
}

/// Closed risk band used for task and candidate policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskLevel {
    /// Low risk.
    Low,
    /// Moderate risk.
    Moderate,
    /// High risk.
    High,
    /// Critical risk.
    Critical,
}

/// Expected temporal shape of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskHorizon {
    /// One action or decision.
    SingleStep,
    /// A bounded short sequence.
    ShortSequence,
    /// Repeated refinement or checking.
    Iterative,
    /// A long but bounded task.
    Extended,
    /// No fixed completion horizon is known.
    OpenEnded,
}

/// Whether a verifier can be supplied by the embodiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerifierAvailability {
    /// No verifier is available.
    Unavailable,
    /// A verifier is available only under stated conditions.
    Conditional,
    /// A verifier is available for the task.
    Available,
}

/// One non-ordinal verifier capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerifierCapability {
    /// The verifier supplies heuristic evidence.
    Heuristic,
    /// The verifier can compare candidate results.
    Comparative,
    /// The verifier can apply a deterministic check.
    Deterministic,
}

/// Available verifier capability set without an unsound total strength order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifierProfile {
    availability: VerifierAvailability,
    capabilities: Vec<VerifierCapability>,
}

impl VerifierProfile {
    /// Creates a canonical verifier profile.
    pub fn new(
        availability: VerifierAvailability,
        mut capabilities: Vec<VerifierCapability>,
    ) -> Result<Self, ContractError> {
        capabilities.sort_unstable();
        capabilities.dedup();
        let profile = Self {
            availability,
            capabilities,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Returns verifier availability.
    #[must_use]
    pub const fn availability(&self) -> VerifierAvailability {
        self.availability
    }

    /// Returns the canonical capability set.
    #[must_use]
    pub fn capabilities(&self) -> &[VerifierCapability] {
        &self.capabilities
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted("profile.verifier-capabilities", &self.capabilities)?;
        match (self.availability, self.capabilities.is_empty()) {
            (VerifierAvailability::Unavailable, false) => Err(ContractError::new(
                "profile.unavailable-verifier-capabilities",
                "an unavailable verifier cannot advertise capabilities",
            )),
            (VerifierAvailability::Available, true) => Err(ContractError::new(
                "profile.available-verifier-empty",
                "an available verifier must advertise at least one capability",
            )),
            _ => Ok(()),
        }
    }
}

impl<'de> Deserialize<'de> for VerifierProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            availability: VerifierAvailability,
            capabilities: Vec<VerifierCapability>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let profile = Self {
            availability: wire.availability,
            capabilities: wire.capabilities,
        };
        profile.validate().map_err(serde::de::Error::custom)?;
        Ok(profile)
    }
}

/// Whether an authorized extractor observed a task limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservedLimit<T> {
    /// The extractor checked and found no stated limit.
    NotStated,
    /// The request stated an inclusive upper bound.
    AtMost(T),
}

/// Closed feature set carried by a [`TaskProfile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFeatures {
    /// Task decomposition shape.
    pub decomposability: FeatureAssessment<DecompositionShape>,
    /// Expected number of independently runnable work items.
    pub expected_independent_work: FeatureAssessment<CardinalityEstimate>,
    /// Verifier availability and capabilities.
    pub verifier: FeatureAssessment<VerifierProfile>,
    /// Uncertainty in the task or environment.
    pub uncertainty: FeatureAssessment<NeedLevel>,
    /// Need to explore alternatives.
    pub exploration_need: FeatureAssessment<NeedLevel>,
    /// Need for iterative refinement.
    pub refinement_need: FeatureAssessment<NeedLevel>,
    /// Need to isolate context between work items.
    pub context_isolation_need: FeatureAssessment<NeedLevel>,
    /// Need for human interaction.
    pub human_interaction_need: FeatureAssessment<NeedLevel>,
    /// Consequence of a poor result.
    pub consequence: FeatureAssessment<ConsequenceLevel>,
    /// Inherent risk before a topology is selected.
    pub inherent_task_risk: FeatureAssessment<RiskLevel>,
    /// Expected task horizon.
    pub horizon: FeatureAssessment<TaskHorizon>,
    /// Observed request latency limit.
    pub requested_latency_limit: FeatureAssessment<ObservedLimit<DurationMs>>,
    /// Observed request token limit.
    pub requested_token_limit: FeatureAssessment<ObservedLimit<TokenQuantity>>,
    /// Observed request monetary limit.
    pub requested_monetary_limit: FeatureAssessment<ObservedLimit<Money>>,
}

/// Embodiment observations awaiting a replaceable profile aggregation point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskProfileEvidence {
    /// Candidate observations of task decomposition.
    pub decomposability: Vec<FeatureAssessment<DecompositionShape>>,
    /// Candidate observations of independent-work cardinality.
    pub expected_independent_work: Vec<FeatureAssessment<CardinalityEstimate>>,
    /// Candidate observations of verifier support.
    pub verifier: Vec<FeatureAssessment<VerifierProfile>>,
    /// Candidate observations of uncertainty.
    pub uncertainty: Vec<FeatureAssessment<NeedLevel>>,
    /// Candidate observations of exploration need.
    pub exploration_need: Vec<FeatureAssessment<NeedLevel>>,
    /// Candidate observations of refinement need.
    pub refinement_need: Vec<FeatureAssessment<NeedLevel>>,
    /// Candidate observations of context-isolation need.
    pub context_isolation_need: Vec<FeatureAssessment<NeedLevel>>,
    /// Candidate observations of human-interaction need.
    pub human_interaction_need: Vec<FeatureAssessment<NeedLevel>>,
    /// Candidate observations of consequence.
    pub consequence: Vec<FeatureAssessment<ConsequenceLevel>>,
    /// Candidate observations of inherent risk.
    pub inherent_task_risk: Vec<FeatureAssessment<RiskLevel>>,
    /// Candidate observations of horizon.
    pub horizon: Vec<FeatureAssessment<TaskHorizon>>,
    /// Candidate observations of request latency limits.
    pub requested_latency_limit: Vec<FeatureAssessment<ObservedLimit<DurationMs>>>,
    /// Candidate observations of request token limits.
    pub requested_token_limit: Vec<FeatureAssessment<ObservedLimit<TokenQuantity>>>,
    /// Candidate observations of request monetary limits.
    pub requested_monetary_limit: Vec<FeatureAssessment<ObservedLimit<Money>>>,
}

/// Canonical, domain-neutral task profile used as selection evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskProfile {
    schema_version: super::SchemaVersion,
    task_id: TaskId,
    external_session: Option<ExternalSessionRef>,
    features: TaskFeatures,
    content_digest: ContentDigest,
}

impl TaskProfile {
    /// Creates a version-one task profile and computes its content identity.
    pub fn new(
        task_id: TaskId,
        external_session: Option<ExternalSessionRef>,
        features: TaskFeatures,
    ) -> Result<Self, ContractError> {
        let mut profile = Self {
            schema_version: super::SchemaVersion::V1,
            task_id,
            external_session,
            features,
            content_digest: ContentDigest::sha256(&[]),
        };
        profile.content_digest = content_digest(&profile.content_view())?;
        Ok(profile)
    }

    /// Returns the host-owned task identity.
    #[must_use]
    pub const fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> super::SchemaVersion {
        self.schema_version
    }

    /// Returns the optional external session reference.
    #[must_use]
    pub const fn external_session(&self) -> Option<&ExternalSessionRef> {
        self.external_session.as_ref()
    }

    /// Returns the closed task features.
    #[must_use]
    pub const fn features(&self) -> &TaskFeatures {
        &self.features
    }

    /// Returns the canonical profile digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn content_view(&self) -> TaskProfileContent<'_> {
        TaskProfileContent {
            schema_version: self.schema_version,
            task_id: &self.task_id,
            external_session: self.external_session.as_ref(),
            features: &self.features,
        }
    }
}

#[derive(Serialize)]
struct TaskProfileContent<'a> {
    schema_version: super::SchemaVersion,
    task_id: &'a TaskId,
    external_session: Option<&'a ExternalSessionRef>,
    features: &'a TaskFeatures,
}

impl CanonicalRecord for TaskProfile {
    fn validate(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        let expected = content_digest(&self.content_view())?;
        if expected == self.content_digest {
            Ok(())
        } else {
            Err(ContractError::new(
                "identity.digest-mismatch",
                "task profile content digest does not match its canonical content",
            ))
        }
    }
}

/// Maturity floor used by objective policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaturityLevel {
    /// Experimental registration with little evidence.
    Experimental,
    /// Preview registration suitable for bounded trials.
    Preview,
    /// Stable registration with supported behaviour.
    Stable,
}

/// How policy treats a missing estimate needed by a hard numeric constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnknownEstimatePolicy {
    /// Exclude the candidate conservatively.
    Reject,
    /// Permit the candidate despite the missing estimate.
    Allow,
}

/// Closed hard-constraint family evaluated before ranking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardConstraints {
    maximum_latency: Option<DurationMs>,
    maximum_tokens: Option<TokenQuantity>,
    maximum_money: Option<Money>,
    required_verifier_capabilities: Vec<VerifierCapability>,
    forbidden_capabilities: Vec<CapabilityId>,
    forbidden_providers: Vec<ProviderId>,
    forbidden_data_paths: Vec<DataPathId>,
    minimum_maturity: Option<MaturityLevel>,
    allowed_tenancy_scopes: Option<Vec<TenancyScopeId>>,
    required_privacy_policy: Option<PolicyRef>,
    maximum_execution_failure_probability: Option<super::ProbabilityPpm>,
    maximum_side_effect_risk: Option<RiskLevel>,
    unknown_estimate_policy: UnknownEstimatePolicy,
}

impl HardConstraints {
    /// Creates the trivial constraint set with conservative unknown handling.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            maximum_latency: None,
            maximum_tokens: None,
            maximum_money: None,
            required_verifier_capabilities: Vec::new(),
            forbidden_capabilities: Vec::new(),
            forbidden_providers: Vec::new(),
            forbidden_data_paths: Vec::new(),
            minimum_maturity: None,
            allowed_tenancy_scopes: None,
            required_privacy_policy: None,
            maximum_execution_failure_probability: None,
            maximum_side_effect_risk: None,
            unknown_estimate_policy: UnknownEstimatePolicy::Reject,
        }
    }

    /// Sets the latency ceiling.
    #[must_use]
    pub const fn with_maximum_latency(mut self, value: DurationMs) -> Self {
        self.maximum_latency = Some(value);
        self
    }

    /// Sets the token ceiling and accounting witness.
    #[must_use]
    pub fn with_maximum_tokens(mut self, value: TokenQuantity) -> Self {
        self.maximum_tokens = Some(value);
        self
    }

    /// Sets the monetary ceiling and currency witness.
    #[must_use]
    pub const fn with_maximum_money(mut self, value: Money) -> Self {
        self.maximum_money = Some(value);
        self
    }

    /// Sets required verifier capabilities in canonical order.
    #[must_use]
    pub fn with_required_verifier_capabilities(
        mut self,
        mut values: Vec<VerifierCapability>,
    ) -> Self {
        values.sort_unstable();
        values.dedup();
        self.required_verifier_capabilities = values;
        self
    }

    /// Sets forbidden semantic capabilities in canonical order.
    #[must_use]
    pub fn with_forbidden_capabilities(mut self, mut values: Vec<CapabilityId>) -> Self {
        values.sort();
        values.dedup();
        self.forbidden_capabilities = values;
        self
    }

    /// Sets forbidden providers in canonical order.
    #[must_use]
    pub fn with_forbidden_providers(mut self, mut values: Vec<ProviderId>) -> Self {
        values.sort();
        values.dedup();
        self.forbidden_providers = values;
        self
    }

    /// Sets forbidden data paths in canonical order.
    #[must_use]
    pub fn with_forbidden_data_paths(mut self, mut values: Vec<DataPathId>) -> Self {
        values.sort();
        values.dedup();
        self.forbidden_data_paths = values;
        self
    }

    /// Sets the minimum maturity.
    #[must_use]
    pub const fn with_minimum_maturity(mut self, value: MaturityLevel) -> Self {
        self.minimum_maturity = Some(value);
        self
    }

    /// Sets a non-empty canonical tenancy allow-list.
    pub fn with_allowed_tenancy_scopes(
        mut self,
        mut values: Vec<TenancyScopeId>,
    ) -> Result<Self, ContractError> {
        values.sort();
        values.dedup();
        if values.is_empty() {
            return Err(ContractError::new(
                "objective.empty-tenancy-allow-list",
                "configured tenancy allow-list must not be empty",
            ));
        }
        self.allowed_tenancy_scopes = Some(values);
        Ok(self)
    }

    /// Sets the required privacy policy.
    #[must_use]
    pub fn with_required_privacy_policy(mut self, value: PolicyRef) -> Self {
        self.required_privacy_policy = Some(value);
        self
    }

    /// Sets the maximum candidate execution-failure probability.
    #[must_use]
    pub const fn with_maximum_execution_failure_probability(
        mut self,
        value: super::ProbabilityPpm,
    ) -> Self {
        self.maximum_execution_failure_probability = Some(value);
        self
    }

    /// Sets the maximum side-effect risk band.
    #[must_use]
    pub const fn with_maximum_side_effect_risk(mut self, value: RiskLevel) -> Self {
        self.maximum_side_effect_risk = Some(value);
        self
    }

    /// Sets explicit missing-estimate treatment.
    #[must_use]
    pub const fn with_unknown_estimate_policy(mut self, value: UnknownEstimatePolicy) -> Self {
        self.unknown_estimate_policy = value;
        self
    }

    /// Returns the configured latency ceiling.
    #[must_use]
    pub const fn maximum_latency(&self) -> Option<DurationMs> {
        self.maximum_latency
    }

    /// Returns the configured token ceiling and accounting witness.
    #[must_use]
    pub const fn maximum_tokens(&self) -> Option<&TokenQuantity> {
        self.maximum_tokens.as_ref()
    }

    /// Returns the configured monetary ceiling and currency witness.
    #[must_use]
    pub const fn maximum_money(&self) -> Option<Money> {
        self.maximum_money
    }

    /// Returns required verifier capabilities in canonical order.
    #[must_use]
    pub fn required_verifier_capabilities(&self) -> &[VerifierCapability] {
        &self.required_verifier_capabilities
    }

    /// Returns forbidden semantic capability identities in canonical order.
    #[must_use]
    pub fn forbidden_capabilities(&self) -> &[CapabilityId] {
        &self.forbidden_capabilities
    }

    /// Returns forbidden provider identities in canonical order.
    #[must_use]
    pub fn forbidden_providers(&self) -> &[ProviderId] {
        &self.forbidden_providers
    }

    /// Returns forbidden data-path identities in canonical order.
    #[must_use]
    pub fn forbidden_data_paths(&self) -> &[DataPathId] {
        &self.forbidden_data_paths
    }

    /// Returns the configured minimum maturity.
    #[must_use]
    pub const fn minimum_maturity(&self) -> Option<MaturityLevel> {
        self.minimum_maturity
    }

    /// Returns the configured tenancy allow-list in canonical order.
    #[must_use]
    pub fn allowed_tenancy_scopes(&self) -> Option<&[TenancyScopeId]> {
        self.allowed_tenancy_scopes.as_deref()
    }

    /// Returns the required privacy policy.
    #[must_use]
    pub const fn required_privacy_policy(&self) -> Option<&PolicyRef> {
        self.required_privacy_policy.as_ref()
    }

    /// Returns the configured execution-failure probability ceiling.
    #[must_use]
    pub const fn maximum_execution_failure_probability(&self) -> Option<super::ProbabilityPpm> {
        self.maximum_execution_failure_probability
    }

    /// Returns the configured side-effect risk ceiling.
    #[must_use]
    pub const fn maximum_side_effect_risk(&self) -> Option<RiskLevel> {
        self.maximum_side_effect_risk
    }

    /// Returns explicit treatment of estimates missing for hard constraints.
    #[must_use]
    pub const fn unknown_estimate_policy(&self) -> UnknownEstimatePolicy {
        self.unknown_estimate_policy
    }

    fn validate(&self) -> Result<(), ContractError> {
        validate_strictly_sorted(
            "objective.required-verifier-capabilities",
            &self.required_verifier_capabilities,
        )?;
        validate_strictly_sorted(
            "objective.forbidden-capabilities",
            &self.forbidden_capabilities,
        )?;
        validate_strictly_sorted("objective.forbidden-providers", &self.forbidden_providers)?;
        validate_strictly_sorted("objective.forbidden-data-paths", &self.forbidden_data_paths)?;
        if let Some(values) = &self.allowed_tenancy_scopes {
            if values.is_empty() {
                return Err(ContractError::new(
                    "objective.empty-tenancy-allow-list",
                    "configured tenancy allow-list must not be empty",
                ));
            }
            validate_strictly_sorted("objective.allowed-tenancy-scopes", values)?;
        }
        Ok(())
    }
}

/// One closed ranking criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreferenceCriterion {
    /// Expected result quality.
    Quality,
    /// Expected verified coverage.
    VerifiedCoverage,
    /// End-to-end latency.
    Latency,
    /// Token consumption.
    TokenCost,
    /// Monetary cost.
    MonetaryCost,
    /// Mechanical execution-failure risk.
    ExecutionFailureRisk,
    /// Expected human intervention.
    HumanIntervention,
    /// Explicit exploration utility.
    Exploration,
}

/// Complete fixed preference vector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceWeights {
    quality: PreferenceBasisPoints,
    verified_coverage: PreferenceBasisPoints,
    latency: PreferenceBasisPoints,
    token_cost: PreferenceBasisPoints,
    monetary_cost: PreferenceBasisPoints,
    execution_failure_risk: PreferenceBasisPoints,
    human_intervention: PreferenceBasisPoints,
    exploration: PreferenceBasisPoints,
}

impl PreferenceWeights {
    /// Creates a complete preference vector whose weights total 10,000.
    #[expect(
        clippy::too_many_arguments,
        reason = "the closed v1 vector has eight fixed criteria"
    )]
    pub fn new(
        quality: PreferenceBasisPoints,
        verified_coverage: PreferenceBasisPoints,
        latency: PreferenceBasisPoints,
        token_cost: PreferenceBasisPoints,
        monetary_cost: PreferenceBasisPoints,
        execution_failure_risk: PreferenceBasisPoints,
        human_intervention: PreferenceBasisPoints,
        exploration: PreferenceBasisPoints,
    ) -> Result<Self, ContractError> {
        PreferenceBasisPoints::validate_total([
            quality,
            verified_coverage,
            latency,
            token_cost,
            monetary_cost,
            execution_failure_risk,
            human_intervention,
            exploration,
        ])?;
        Ok(Self {
            quality,
            verified_coverage,
            latency,
            token_cost,
            monetary_cost,
            execution_failure_risk,
            human_intervention,
            exploration,
        })
    }

    /// Returns the approved trivial default vector.
    #[must_use]
    pub fn trivial_default() -> Self {
        Self::new(
            PreferenceBasisPoints::new(3_500).expect("constant is in range"),
            PreferenceBasisPoints::new(3_500).expect("constant is in range"),
            PreferenceBasisPoints::new(500).expect("constant is in range"),
            PreferenceBasisPoints::new(400).expect("constant is in range"),
            PreferenceBasisPoints::new(400).expect("constant is in range"),
            PreferenceBasisPoints::new(1_500).expect("constant is in range"),
            PreferenceBasisPoints::new(200).expect("constant is in range"),
            PreferenceBasisPoints::new(0).expect("constant is in range"),
        )
        .expect("approved default totals 10,000 basis points")
    }

    /// Returns the weight for one criterion.
    #[must_use]
    pub const fn get(&self, criterion: PreferenceCriterion) -> PreferenceBasisPoints {
        match criterion {
            PreferenceCriterion::Quality => self.quality,
            PreferenceCriterion::VerifiedCoverage => self.verified_coverage,
            PreferenceCriterion::Latency => self.latency,
            PreferenceCriterion::TokenCost => self.token_cost,
            PreferenceCriterion::MonetaryCost => self.monetary_cost,
            PreferenceCriterion::ExecutionFailureRisk => self.execution_failure_risk,
            PreferenceCriterion::HumanIntervention => self.human_intervention,
            PreferenceCriterion::Exploration => self.exploration,
        }
    }

    fn validate(&self) -> Result<(), ContractError> {
        PreferenceBasisPoints::validate_total([
            self.quality,
            self.verified_coverage,
            self.latency,
            self.token_cost,
            self.monetary_cost,
            self.execution_failure_risk,
            self.human_intervention,
            self.exploration,
        ])
    }
}

/// Treatment of missing evidence for a weighted preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissingPreferencePolicy {
    /// Use the least favourable utility.
    Pessimistic,
    /// Use neutral utility.
    Neutral,
    /// Ignore the criterion and explicitly renormalize remaining weights.
    IgnoreAndRenormalize,
}

/// Exact ranking tie-break rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TieBreakPolicy {
    /// Choose ascending registration identity.
    AscendingRegistrationId,
    /// Use a named replacement policy.
    Named(PolicyRef),
}

/// Canonical objective separating hard constraints from weighted preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Objective {
    schema_version: super::SchemaVersion,
    hard: HardConstraints,
    preferences: PreferenceWeights,
    missing_preference_evidence: MissingPreferencePolicy,
    tie_break: TieBreakPolicy,
    provenance: Vec<Provenance>,
    content_digest: ContentDigest,
}

impl Objective {
    /// Creates a version-one objective and computes its content identity.
    pub fn new(
        hard: HardConstraints,
        preferences: PreferenceWeights,
        missing_preference_evidence: MissingPreferencePolicy,
        tie_break: TieBreakPolicy,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        hard.validate()?;
        preferences.validate()?;
        let provenance = canonical_provenance(provenance)?;
        let mut objective = Self {
            schema_version: super::SchemaVersion::V1,
            hard,
            preferences,
            missing_preference_evidence,
            tie_break,
            provenance,
            content_digest: ContentDigest::sha256(&[]),
        };
        objective.content_digest = content_digest(&objective.content_view())?;
        Ok(objective)
    }

    /// Creates the approved trivial default with explicit provenance.
    pub fn trivial_default(provenance: Vec<Provenance>) -> Result<Self, ContractError> {
        Self::new(
            HardConstraints::none(),
            PreferenceWeights::trivial_default(),
            MissingPreferencePolicy::Pessimistic,
            TieBreakPolicy::AscendingRegistrationId,
            provenance,
        )
    }

    /// Returns hard eligibility constraints.
    #[must_use]
    pub const fn hard(&self) -> &HardConstraints {
        &self.hard
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> super::SchemaVersion {
        self.schema_version
    }

    /// Returns the complete preference vector.
    #[must_use]
    pub const fn preferences(&self) -> &PreferenceWeights {
        &self.preferences
    }

    /// Returns treatment of missing preference evidence.
    #[must_use]
    pub const fn missing_preference_evidence(&self) -> MissingPreferencePolicy {
        self.missing_preference_evidence
    }

    /// Returns the exact ranking tie-break policy.
    #[must_use]
    pub const fn tie_break(&self) -> &TieBreakPolicy {
        &self.tie_break
    }

    /// Returns objective provenance in canonical order.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns the canonical objective digest.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn content_view(&self) -> ObjectiveContent<'_> {
        ObjectiveContent {
            schema_version: self.schema_version,
            hard: &self.hard,
            preferences: &self.preferences,
            missing_preference_evidence: self.missing_preference_evidence,
            tie_break: &self.tie_break,
            provenance: &self.provenance,
        }
    }
}

#[derive(Serialize)]
struct ObjectiveContent<'a> {
    schema_version: super::SchemaVersion,
    hard: &'a HardConstraints,
    preferences: &'a PreferenceWeights,
    missing_preference_evidence: MissingPreferencePolicy,
    tie_break: &'a TieBreakPolicy,
    provenance: &'a [Provenance],
}

impl CanonicalRecord for Objective {
    fn validate(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.hard.validate()?;
        self.preferences.validate()?;
        if self.provenance.is_empty() || self.provenance.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ContractError::new(
                "provenance.non-canonical",
                "objective provenance must be non-empty, sorted, and duplicate-free",
            ));
        }
        let expected = content_digest(&self.content_view())?;
        if expected == self.content_digest {
            Ok(())
        } else {
            Err(ContractError::new(
                "identity.digest-mismatch",
                "objective content digest does not match its canonical content",
            ))
        }
    }
}

pub(crate) fn validate_strictly_sorted<T: Ord>(
    code: &'static str,
    values: &[T],
) -> Result<(), ContractError> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        Err(ContractError::new(
            code,
            "values must be strictly sorted and duplicate-free",
        ))
    } else {
        Ok(())
    }
}
