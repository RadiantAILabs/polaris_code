//! Explained deterministic selection over frozen topology-runtime inputs.
//!
//! Selection is an ordinary Cosmos policy point. It consumes a complete
//! mechanical-admission value and immutable catalog, availability, and
//! statistics snapshots. Mechanical exclusions are copied into the decision
//! unchanged; only mechanically admitted candidates reach hard policy or
//! ranking. The policy never resolves or executes a selected graph.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use polaris::graph::{Graph, GraphSignature};
use polaris::system::param::Out;
use polaris::system::plugin::{Schedule, Version};
use polaris::system::system;
use polaris::system::system::SystemError;

use crate::contracts::{
    CandidateConsideration, CandidateScore, CanonicalRecord, CapabilityId, ConfidencePpm,
    ContentDigest, ContractError, DataPathId, DurationMs, ExclusionCode, ExclusionPhase,
    ExclusionReason, ExclusionSummary, FallbackCandidate, MaturityLevel, Measurement,
    MeasurementValue, MissingPreferencePolicy, Money, Objective, PolicyEligibility, PolicyRef,
    PreferenceCriterion, ProbabilityPpm, Provenance, ProviderId, RecordSubjectId,
    RegistrationSelectionRef, RuntimeAvailabilitySnapshot, ScoreComponent, ScoreComponentId,
    ScoreSource, SelectionDecision, SelectionDecisionId, SelectionInputRefs, SelectionOutcome,
    SelectionRationale, SelectorId, SelectorKey, SelectorRef, TaskProfile, TenancyScopeId,
    TieBreakPolicy, TokenQuantity, UnknownEstimatePolicy, UtilityPpm, VerifierCapability,
};
use crate::evidence::store::TopologyStatsSnapshot;
use crate::topology::admission::CandidateAdmissionSet;
use crate::topology::{
    RegisteredTopology, RegistrationId, TopologyCatalog, TopologyCatalogError,
    TopologyCatalogSnapshot, TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey,
};

/// Stable topology identity of the rule-based deterministic selector point.
pub const TOPOLOGY_ID: &str = "cosmos.selector.deterministic";

/// Stable selector policy identity.
pub const SELECTOR_ID: &str = "cosmos.selector.deterministic";

/// Semantic version of the deterministic selector behaviour.
pub const SELECTOR_VERSION: Version = Version::new(1, 0, 0);

/// Semantic role applied to the system that makes the selection decision.
#[derive(Debug, Clone, Copy)]
pub struct OnSelect;
impl Schedule for OnSelect {}

/// Error produced when immutable selector inputs are inconsistent or invalid.
#[derive(Debug)]
#[non_exhaustive]
pub enum SelectionError {
    /// A contract constructor or record rejected invalid input.
    Contract(ContractError),
    /// The mechanical preflight was produced for another requested contract.
    AdmissionContractMismatch,
    /// The admission set and catalog snapshot use different generations.
    CatalogGenerationMismatch,
    /// Mechanical admission did not cover the catalog candidate universe once.
    AdmissionCandidateSetMismatch,
    /// Candidate policy facts did not cover the admission candidate universe once.
    PolicyFactSetMismatch,
    /// A captured admission candidate disagreed with the catalog's immutable identity.
    CandidateIdentityMismatch {
        /// Candidate whose immutable identity disagreed.
        registration: RegistrationId,
    },
    /// A historical score component refers to a different statistics snapshot.
    StatisticsSnapshotMismatch {
        /// Candidate containing the stale historical component.
        registration: RegistrationId,
        /// Component containing the stale statistics reference.
        component: ScoreComponentId,
    },
    /// The objective names a tie-break policy this selector point does not implement.
    UnsupportedTieBreak(PolicyRef),
    /// A named selector policy used the invalid zero semantic version.
    ZeroPolicyVersion,
    /// Selector-owned defaults lacked provenance.
    EmptyPolicyProvenance,
    /// Integer arithmetic overflowed while combining normalized score evidence.
    ArithmeticOverflow,
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => error.fmt(formatter),
            Self::AdmissionContractMismatch => formatter.write_str(
                "mechanical admission and selection request must name the same contract",
            ),
            Self::CatalogGenerationMismatch => formatter.write_str(
                "mechanical admission and catalog snapshot must use the same generation",
            ),
            Self::AdmissionCandidateSetMismatch => formatter
                .write_str("mechanical admission must cover every catalog candidate exactly once"),
            Self::PolicyFactSetMismatch => {
                formatter.write_str("policy facts must cover every admitted candidate exactly once")
            }
            Self::CandidateIdentityMismatch { registration } => write!(
                formatter,
                "captured identity for candidate {registration} differs from the catalog snapshot"
            ),
            Self::StatisticsSnapshotMismatch {
                registration,
                component,
            } => write!(
                formatter,
                "historical component {component} for candidate {registration} refers to another statistics snapshot"
            ),
            Self::UnsupportedTieBreak(policy) => {
                write!(
                    formatter,
                    "tie-break policy {policy} is not implemented by this point"
                )
            }
            Self::ZeroPolicyVersion => {
                formatter.write_str("selector policies must have non-zero semantic versions")
            }
            Self::EmptyPolicyProvenance => {
                formatter.write_str("selector configuration requires policy provenance")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("selector integer arithmetic overflowed")
            }
        }
    }
}

impl std::error::Error for SelectionError {}

impl From<ContractError> for SelectionError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

/// Immutable, selection-facing policy facts for one concrete registration.
///
/// These facts remain separate from [`TopologyCatalogSnapshot`]. Changing an
/// objective or statistics snapshot therefore cannot mutate discovery state.
/// The mechanical admission result remains authoritative regardless of these
/// values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatePolicyFacts {
    registration_id: RegistrationId,
    estimated_latency: Option<DurationMs>,
    estimated_tokens: Option<TokenQuantity>,
    estimated_money: Option<Money>,
    verifier_capabilities: Vec<VerifierCapability>,
    capabilities: Vec<CapabilityId>,
    providers: Vec<ProviderId>,
    data_paths: Vec<DataPathId>,
    maturity: Option<MaturityLevel>,
    tenancy_scopes: Vec<TenancyScopeId>,
    privacy_policies: Vec<PolicyRef>,
    execution_failure_probability: Option<ProbabilityPpm>,
    side_effect_risk: Option<crate::contracts::RiskLevel>,
    score_components: Vec<ScoreComponent>,
}

impl CandidatePolicyFacts {
    /// Creates an empty fact set for a registration.
    #[must_use]
    pub fn new(registration_id: RegistrationId) -> Self {
        Self {
            registration_id,
            estimated_latency: None,
            estimated_tokens: None,
            estimated_money: None,
            verifier_capabilities: Vec::new(),
            capabilities: Vec::new(),
            providers: Vec::new(),
            data_paths: Vec::new(),
            maturity: None,
            tenancy_scopes: Vec::new(),
            privacy_policies: Vec::new(),
            execution_failure_probability: None,
            side_effect_risk: None,
            score_components: Vec::new(),
        }
    }

    /// Sets the conservative latency estimate used by hard policy.
    #[must_use]
    pub const fn with_estimated_latency(mut self, value: DurationMs) -> Self {
        self.estimated_latency = Some(value);
        self
    }

    /// Sets the conservative token estimate used by hard policy.
    #[must_use]
    pub fn with_estimated_tokens(mut self, value: TokenQuantity) -> Self {
        self.estimated_tokens = Some(value);
        self
    }

    /// Sets the conservative monetary estimate used by hard policy.
    #[must_use]
    pub const fn with_estimated_money(mut self, value: Money) -> Self {
        self.estimated_money = Some(value);
        self
    }

    /// Sets the verifier capabilities this registration can supply.
    #[must_use]
    pub fn with_verifier_capabilities(mut self, mut values: Vec<VerifierCapability>) -> Self {
        values.sort_unstable();
        values.dedup();
        self.verifier_capabilities = values;
        self
    }

    /// Sets semantic capabilities used by hard policy.
    #[must_use]
    pub fn with_capabilities(mut self, mut values: Vec<CapabilityId>) -> Self {
        values.sort();
        values.dedup();
        self.capabilities = values;
        self
    }

    /// Sets provider identities used by hard policy.
    #[must_use]
    pub fn with_providers(mut self, mut values: Vec<ProviderId>) -> Self {
        values.sort();
        values.dedup();
        self.providers = values;
        self
    }

    /// Sets data-path identities used by hard policy.
    #[must_use]
    pub fn with_data_paths(mut self, mut values: Vec<DataPathId>) -> Self {
        values.sort();
        values.dedup();
        self.data_paths = values;
        self
    }

    /// Sets the registration maturity band.
    #[must_use]
    pub const fn with_maturity(mut self, value: MaturityLevel) -> Self {
        self.maturity = Some(value);
        self
    }

    /// Sets tenancy scopes required by this registration.
    #[must_use]
    pub fn with_tenancy_scopes(mut self, mut values: Vec<TenancyScopeId>) -> Self {
        values.sort();
        values.dedup();
        self.tenancy_scopes = values;
        self
    }

    /// Sets privacy policies satisfied by this registration.
    #[must_use]
    pub fn with_privacy_policies(mut self, mut values: Vec<PolicyRef>) -> Self {
        values.sort();
        values.dedup();
        self.privacy_policies = values;
        self
    }

    /// Sets the conservative mechanical execution-failure estimate.
    #[must_use]
    pub const fn with_execution_failure_probability(mut self, value: ProbabilityPpm) -> Self {
        self.execution_failure_probability = Some(value);
        self
    }

    /// Sets the conservative side-effect risk band.
    #[must_use]
    pub const fn with_side_effect_risk(mut self, value: crate::contracts::RiskLevel) -> Self {
        self.side_effect_risk = Some(value);
        self
    }

    /// Sets attributed, already-normalized author or historical score inputs.
    ///
    /// Raw measurements and named normalization policies remain visible in
    /// every [`ScoreComponent`]. Historical components must refer to the exact
    /// statistics snapshot supplied to selection.
    #[must_use]
    pub fn with_score_components(mut self, mut values: Vec<ScoreComponent>) -> Self {
        values.sort_by(|left, right| left.component_id().cmp(right.component_id()));
        values.dedup_by(|left, right| left.component_id() == right.component_id());
        self.score_components = values;
        self
    }

    /// Returns this fact set's registration identity.
    #[must_use]
    pub const fn registration_id(&self) -> &RegistrationId {
        &self.registration_id
    }
}

/// Immutable configuration of the deterministic selector point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterministicSelectorConfig {
    selector: SelectorRef,
    missing_evidence_normalization: PolicyRef,
    confidence_policy: PolicyRef,
    policy_provenance: Vec<Provenance>,
}

impl DeterministicSelectorConfig {
    /// Creates selector configuration with explicit policy identities.
    pub fn new(
        selector: SelectorRef,
        missing_evidence_normalization: PolicyRef,
        confidence_policy: PolicyRef,
        mut policy_provenance: Vec<Provenance>,
    ) -> Result<Self, SelectionError> {
        if missing_evidence_normalization.version() == Version::new(0, 0, 0)
            || confidence_policy.version() == Version::new(0, 0, 0)
        {
            return Err(SelectionError::ZeroPolicyVersion);
        }
        policy_provenance.sort();
        policy_provenance.dedup();
        if policy_provenance.is_empty() {
            return Err(SelectionError::EmptyPolicyProvenance);
        }
        Ok(Self {
            selector,
            missing_evidence_normalization,
            confidence_policy,
            policy_provenance,
        })
    }

    /// Returns the stable selector and immutable configuration identity.
    #[must_use]
    pub const fn selector(&self) -> &SelectorRef {
        &self.selector
    }
}

/// Complete owned inputs consumed by the selector graph.
#[derive(Debug, Clone)]
pub struct SelectionInputs {
    decision_id: SelectionDecisionId,
    profile: TaskProfile,
    objective: Objective,
    requested_contract: TopologyContractKey,
    catalog: TopologyCatalogSnapshot,
    availability: RuntimeAvailabilitySnapshot,
    admission: CandidateAdmissionSet,
    statistics: TopologyStatsSnapshot,
    policy_facts: Vec<CandidatePolicyFacts>,
    config: DeterministicSelectorConfig,
}

impl SelectionInputs {
    /// Captures all immutable selection values for one decision attempt.
    #[expect(
        clippy::too_many_arguments,
        reason = "the selector input makes every frozen snapshot explicit"
    )]
    #[must_use]
    pub fn new(
        decision_id: SelectionDecisionId,
        profile: TaskProfile,
        objective: Objective,
        requested_contract: TopologyContractKey,
        catalog: TopologyCatalogSnapshot,
        availability: RuntimeAvailabilitySnapshot,
        admission: CandidateAdmissionSet,
        statistics: TopologyStatsSnapshot,
        mut policy_facts: Vec<CandidatePolicyFacts>,
        config: DeterministicSelectorConfig,
    ) -> Self {
        policy_facts.sort_by(|left, right| left.registration_id.cmp(&right.registration_id));
        Self {
            decision_id,
            profile,
            objective,
            requested_contract,
            catalog,
            availability,
            admission,
            statistics,
            policy_facts,
            config,
        }
    }
}

/// Returns the stable versioned topology identity of this selector graph.
#[must_use]
pub fn topology_key() -> TopologyKey {
    TopologyKey::new(
        TopologyId::new(TOPOLOGY_ID).expect("deterministic selector topology ID is valid"),
        SELECTOR_VERSION,
    )
}

/// Returns the stable versioned selector-policy identity.
#[must_use]
pub fn selector_key() -> SelectorKey {
    SelectorKey::new(
        SelectorId::new(SELECTOR_ID).expect("deterministic selector ID is valid"),
        SELECTOR_VERSION,
    )
}

/// Creates catalog metadata for the deterministic selector graph.
#[must_use]
pub fn descriptor(selector_contract: TopologyContractKey) -> TopologyDescriptor {
    TopologyDescriptor::new(
        topology_key(),
        "Explained deterministic selector",
        "Hard-gate mechanically admitted candidates, rank explicit evidence, and emit a reproducible decision",
    )
    .expect("deterministic selector descriptor metadata is valid")
    .claim(selector_contract)
}

/// Returns the native structural contract of the selector graph.
#[must_use]
pub fn contract_signature() -> GraphSignature {
    GraphSignature::new()
        .require_output::<SelectionInputs>()
        .produce::<SelectionDecision>()
}

/// Adds the fallible deterministic selection system to a Polaris graph.
pub fn build(graph: &mut Graph) {
    graph.add_system((OnSelect, choose));
}

#[system]
async fn choose(inputs: Out<SelectionInputs>) -> Result<SelectionDecision, SystemError> {
    select(&inputs).map_err(|error| SystemError::ExecutionError(error.to_string()))
}

/// Evaluates hard policy, deterministic ranking, and complete explanation.
pub fn select(inputs: &SelectionInputs) -> Result<SelectionDecision, SelectionError> {
    validate_inputs(inputs)?;

    let facts_by_id: BTreeMap<_, _> = inputs
        .policy_facts
        .iter()
        .map(|facts| (facts.registration_id(), facts))
        .collect();
    let mut mechanically_excluded = Vec::new();
    let mut policy_excluded = Vec::new();
    let mut eligible = Vec::new();

    for evaluation in inputs.admission.evaluations() {
        let candidate = evaluation.candidate().clone();
        if !evaluation.result().is_admitted() {
            mechanically_excluded.push(CandidateConsideration::mechanically_excluded(
                candidate,
                evaluation.result().clone(),
            )?);
            continue;
        }

        let facts = facts_by_id
            .get(evaluation.candidate().registration_id())
            .expect("input validation established complete policy facts");
        let reasons = hard_policy_reasons(inputs.objective.hard(), facts)?;
        if !reasons.is_empty() {
            policy_excluded.push(CandidateConsideration::policy_excluded(
                candidate,
                PolicyEligibility::excluded(reasons)?,
            )?);
            continue;
        }
        let score = score_candidate(
            facts,
            &inputs.objective,
            &inputs.config,
            inputs.statistics.content_digest(),
        )?;
        eligible.push((candidate, score));
    }

    rank_eligible(&mut eligible, inputs.objective.tie_break())?;
    let mut candidates = mechanically_excluded;
    candidates.extend(policy_excluded);

    let outcome = if eligible.is_empty() {
        let provisional: Vec<_> = candidates.iter().collect();
        SelectionOutcome::NoEligibleCandidate {
            diagnostics: exclusion_summaries(&provisional)?,
        }
    } else {
        let selected = eligible[0].0.clone();
        let winner_score = &eligible[0].1;
        let tie_break_applied = eligible.get(1).is_some_and(|(_, score)| {
            score.weighted_utility_numerator() == winner_score.weighted_utility_numerator()
        });
        let decisive_components = decisive_components(winner_score, &inputs.objective);
        let decisive_component_set: BTreeSet<_> = decisive_components.iter().collect();
        let confidence = winner_score
            .components()
            .iter()
            .filter(|component| decisive_component_set.contains(component.component_id()))
            .map(ScoreComponent::confidence)
            .min()
            .unwrap_or(ConfidencePpm::ZERO);
        let fallback_plan = eligible
            .iter()
            .skip(1)
            .map(|(candidate, _)| FallbackCandidate::new(candidate.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        SelectionOutcome::Selected {
            selected,
            fallback_plan,
            rationale: SelectionRationale::new(
                decisive_components,
                tie_break_applied,
                false,
                Some(format!(
                    "ranked {} eligible candidate(s) by explicit weighted utility",
                    eligible.len()
                )),
                inputs.config.confidence_policy.clone(),
            )?,
            confidence,
            selected_propensity: ProbabilityPpm::ONE,
            sampling: None,
        }
    };

    for (index, (candidate, score)) in eligible.into_iter().enumerate() {
        candidates.push(CandidateConsideration::eligible(
            candidate,
            score,
            u64::try_from(index + 1).map_err(|_| SelectionError::ArithmeticOverflow)?,
            if index == 0 {
                ProbabilityPpm::ONE
            } else {
                ProbabilityPpm::ZERO
            },
        )?);
    }

    let decision = SelectionDecision::new(
        inputs.decision_id,
        inputs.profile.task_id().clone(),
        inputs.profile.external_session().cloned(),
        inputs.config.selector.clone(),
        inputs.requested_contract.clone(),
        SelectionInputRefs::new(
            inputs.profile.content_digest(),
            inputs.objective.content_digest(),
            inputs.admission.catalog_snapshot_digest(),
            inputs.availability.snapshot_id(),
            inputs.availability.content_digest(),
            inputs.statistics.content_digest(),
        ),
        candidates,
        outcome,
    )?;
    Ok(decision)
}

fn validate_inputs(inputs: &SelectionInputs) -> Result<(), SelectionError> {
    inputs.profile.validate()?;
    inputs.objective.validate()?;
    inputs.availability.validate()?;
    if inputs.admission.requested_contract() != &inputs.requested_contract {
        return Err(SelectionError::AdmissionContractMismatch);
    }
    if inputs.admission.catalog_generation() != inputs.catalog.generation() {
        return Err(SelectionError::CatalogGenerationMismatch);
    }
    let catalog_ids: Vec<_> = inputs
        .catalog
        .candidates()
        .iter()
        .map(|candidate| candidate.registration_id())
        .collect();
    let admission_ids: Vec<_> = inputs
        .admission
        .evaluations()
        .iter()
        .map(|evaluation| evaluation.candidate().registration_id())
        .collect();
    if catalog_ids != admission_ids || admission_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SelectionError::AdmissionCandidateSetMismatch);
    }

    let fact_ids: Vec<_> = inputs
        .policy_facts
        .iter()
        .map(CandidatePolicyFacts::registration_id)
        .collect();
    if admission_ids != fact_ids || fact_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SelectionError::PolicyFactSetMismatch);
    }

    for (native, evaluation) in inputs
        .catalog
        .candidates()
        .iter()
        .zip(inputs.admission.evaluations())
    {
        let captured = evaluation.candidate();
        if native.registration_id() != captured.registration_id()
            || native.descriptor().key() != captured.topology_key()
            || native.configuration_digest() != captured.configuration().digest()
        {
            return Err(SelectionError::CandidateIdentityMismatch {
                registration: captured.registration_id().clone(),
            });
        }
    }

    let statistics_digest = inputs.statistics.content_digest();
    for facts in &inputs.policy_facts {
        for component in &facts.score_components {
            if component.source() == ScoreSource::HistoricalStatistics
                && component.statistics_snapshot_digest() != Some(statistics_digest)
            {
                return Err(SelectionError::StatisticsSnapshotMismatch {
                    registration: facts.registration_id.clone(),
                    component: component.component_id().clone(),
                });
            }
        }
    }

    if let TieBreakPolicy::Named(policy) = inputs.objective.tie_break() {
        return Err(SelectionError::UnsupportedTieBreak(policy.clone()));
    }
    Ok(())
}

fn hard_policy_reasons(
    hard: &crate::contracts::HardConstraints,
    facts: &CandidatePolicyFacts,
) -> Result<Vec<ExclusionReason>, SelectionError> {
    let mut reasons = Vec::new();
    numeric_limit(
        hard.maximum_latency(),
        facts.estimated_latency,
        hard.unknown_estimate_policy(),
        "budget.latency",
        facts,
        |observed, required| observed <= required,
        &mut reasons,
    )?;
    token_limit(hard, facts, &mut reasons)?;
    money_limit(hard, facts, &mut reasons)?;

    if !hard
        .required_verifier_capabilities()
        .iter()
        .all(|required| facts.verifier_capabilities.contains(required))
    {
        reasons.push(policy_reason(
            "verifier.insufficient",
            facts,
            "registration lacks at least one required verifier capability",
        )?);
    }
    if hard
        .forbidden_capabilities()
        .iter()
        .any(|forbidden| facts.capabilities.contains(forbidden))
    {
        reasons.push(policy_reason(
            "capability.forbidden",
            facts,
            "registration requires a forbidden capability",
        )?);
    }
    if hard
        .forbidden_providers()
        .iter()
        .any(|forbidden| facts.providers.contains(forbidden))
    {
        reasons.push(policy_reason(
            "provider.forbidden",
            facts,
            "registration uses a forbidden provider",
        )?);
    }
    if hard
        .forbidden_data_paths()
        .iter()
        .any(|forbidden| facts.data_paths.contains(forbidden))
    {
        reasons.push(policy_reason(
            "data-path.forbidden",
            facts,
            "registration uses a forbidden data path",
        )?);
    }
    if let Some(required) = hard.minimum_maturity()
        && facts.maturity.is_none_or(|observed| observed < required)
    {
        reasons.push(policy_reason(
            "maturity.below-minimum",
            facts,
            "registration maturity is missing or below the required floor",
        )?);
    }
    if let Some(allowed) = hard.allowed_tenancy_scopes()
        && facts
            .tenancy_scopes
            .iter()
            .any(|scope| !allowed.contains(scope))
    {
        reasons.push(policy_reason(
            "tenancy.disallowed",
            facts,
            "registration requires a tenancy scope outside the allow-list",
        )?);
    }
    if let Some(required) = hard.required_privacy_policy()
        && !facts.privacy_policies.contains(required)
    {
        reasons.push(policy_reason(
            "privacy.disallowed",
            facts,
            "registration does not satisfy the required privacy policy",
        )?);
    }
    numeric_limit(
        hard.maximum_execution_failure_probability(),
        facts.execution_failure_probability,
        hard.unknown_estimate_policy(),
        "risk.execution-failure",
        facts,
        |observed, required| observed <= required,
        &mut reasons,
    )?;
    if let Some(required) = hard.maximum_side_effect_risk()
        && facts
            .side_effect_risk
            .is_none_or(|observed| observed > required)
    {
        reasons.push(policy_reason(
            "risk.side-effect",
            facts,
            "registration side-effect risk is missing or above the ceiling",
        )?);
    }

    reasons.sort();
    reasons.dedup();
    Ok(reasons)
}

fn numeric_limit<T: Copy>(
    required: Option<T>,
    observed: Option<T>,
    unknown_policy: UnknownEstimatePolicy,
    code: &str,
    facts: &CandidatePolicyFacts,
    passes: impl FnOnce(T, T) -> bool,
    reasons: &mut Vec<ExclusionReason>,
) -> Result<(), SelectionError> {
    let Some(required) = required else {
        return Ok(());
    };
    match observed {
        Some(observed) if !passes(observed, required) => {
            reasons.push(policy_reason(
                code,
                facts,
                "candidate estimate exceeds the hard limit",
            )?);
        }
        None if unknown_policy == UnknownEstimatePolicy::Reject => {
            reasons.push(policy_reason(
                "estimate.unknown-for-hard-constraint",
                facts,
                &format!("candidate has no estimate required by {code}"),
            )?);
        }
        Some(_) | None => {}
    }
    Ok(())
}

fn token_limit(
    hard: &crate::contracts::HardConstraints,
    facts: &CandidatePolicyFacts,
    reasons: &mut Vec<ExclusionReason>,
) -> Result<(), SelectionError> {
    let Some(required) = hard.maximum_tokens() else {
        return Ok(());
    };
    match &facts.estimated_tokens {
        Some(observed)
            if observed.accounting_id() != required.accounting_id()
                || observed.count() > required.count() =>
        {
            reasons.push(policy_reason(
                "budget.tokens",
                facts,
                "candidate token estimate is incomparable with or above the hard limit",
            )?);
        }
        None if hard.unknown_estimate_policy() == UnknownEstimatePolicy::Reject => {
            reasons.push(policy_reason(
                "estimate.unknown-for-hard-constraint",
                facts,
                "candidate has no token estimate required by budget.tokens",
            )?);
        }
        Some(_) | None => {}
    }
    Ok(())
}

fn money_limit(
    hard: &crate::contracts::HardConstraints,
    facts: &CandidatePolicyFacts,
    reasons: &mut Vec<ExclusionReason>,
) -> Result<(), SelectionError> {
    let Some(required) = hard.maximum_money() else {
        return Ok(());
    };
    match facts.estimated_money {
        Some(observed)
            if observed.currency() != required.currency()
                || observed.micros() > required.micros() =>
        {
            reasons.push(policy_reason(
                "budget.money",
                facts,
                "candidate monetary estimate is incomparable with or above the hard limit",
            )?);
        }
        None if hard.unknown_estimate_policy() == UnknownEstimatePolicy::Reject => {
            reasons.push(policy_reason(
                "estimate.unknown-for-hard-constraint",
                facts,
                "candidate has no monetary estimate required by budget.money",
            )?);
        }
        Some(_) | None => {}
    }
    Ok(())
}

fn policy_reason(
    code: &str,
    facts: &CandidatePolicyFacts,
    detail: &str,
) -> Result<ExclusionReason, SelectionError> {
    Ok(ExclusionReason::new(
        ExclusionPhase::Policy,
        ExclusionCode::new(code)?,
        vec![RecordSubjectId::new(
            facts.registration_id.as_str().to_owned(),
        )?],
        Vec::new(),
    )
    .with_detail(detail)?)
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

fn score_candidate(
    facts: &CandidatePolicyFacts,
    objective: &Objective,
    config: &DeterministicSelectorConfig,
    statistics_digest: ContentDigest,
) -> Result<CandidateScore, SelectionError> {
    let mut components = facts.score_components.clone();
    let mut utilities = BTreeMap::new();
    let mut missing = Vec::new();

    for criterion in CRITERIA {
        let values: Vec<_> = components
            .iter()
            .filter(|component| component.criterion() == criterion)
            .map(ScoreComponent::normalized_utility)
            .collect();
        if let Some(value) = values.into_iter().min() {
            utilities.insert(criterion, value);
        } else {
            missing.push(criterion);
        }
    }

    let missing_utility = match objective.missing_preference_evidence() {
        MissingPreferencePolicy::Pessimistic => UtilityPpm::ZERO,
        MissingPreferencePolicy::Neutral => UtilityPpm::new(500_000)?,
        MissingPreferencePolicy::IgnoreAndRenormalize => {
            ignore_and_renormalize_utility(&utilities, objective)?
        }
    };

    for criterion in missing {
        components.push(ScoreComponent::new(
            ScoreComponentId::new(format!("policy.missing.{}", criterion_label(criterion)))?,
            criterion,
            ScoreSource::SelectorPolicy,
            Measurement::new(
                MeasurementValue::Count(0),
                None,
                ConfidencePpm::ZERO,
                config.policy_provenance.clone(),
            )?,
            config.missing_evidence_normalization.clone(),
            missing_utility,
            ConfidencePpm::ZERO,
            config.policy_provenance.clone(),
            None,
        )?);
        utilities.insert(criterion, missing_utility);
    }

    for component in &components {
        if component.source() == ScoreSource::HistoricalStatistics
            && component.statistics_snapshot_digest() != Some(statistics_digest)
        {
            return Err(SelectionError::StatisticsSnapshotMismatch {
                registration: facts.registration_id.clone(),
                component: component.component_id().clone(),
            });
        }
    }

    let criterion_utilities = CRITERIA
        .into_iter()
        .map(|criterion| {
            crate::contracts::CriterionUtility::new(
                criterion,
                *utilities
                    .get(&criterion)
                    .expect("every criterion receives an explicit utility"),
            )
        })
        .collect();
    Ok(CandidateScore::new(
        components,
        criterion_utilities,
        objective.preferences(),
    )?)
}

fn ignore_and_renormalize_utility(
    utilities: &BTreeMap<PreferenceCriterion, UtilityPpm>,
    objective: &Objective,
) -> Result<UtilityPpm, SelectionError> {
    let (weighted, weight) = utilities.iter().try_fold(
        (0_u64, 0_u64),
        |(weighted, weight), (criterion, utility)| {
            let criterion_weight = u64::from(objective.preferences().get(*criterion).get());
            Ok::<_, SelectionError>((
                weighted
                    .checked_add(
                        criterion_weight
                            .checked_mul(u64::from(utility.get()))
                            .ok_or(SelectionError::ArithmeticOverflow)?,
                    )
                    .ok_or(SelectionError::ArithmeticOverflow)?,
                weight
                    .checked_add(criterion_weight)
                    .ok_or(SelectionError::ArithmeticOverflow)?,
            ))
        },
    )?;
    if weight == 0 {
        return Ok(UtilityPpm::new(500_000)?);
    }
    let utility =
        u32::try_from(weighted / weight).map_err(|_| SelectionError::ArithmeticOverflow)?;
    Ok(UtilityPpm::new(utility)?)
}

fn rank_eligible(
    eligible: &mut [(RegistrationSelectionRef, CandidateScore)],
    tie_break: &TieBreakPolicy,
) -> Result<(), SelectionError> {
    match tie_break {
        TieBreakPolicy::AscendingRegistrationId => {
            eligible.sort_by(|left, right| {
                right
                    .1
                    .weighted_utility_numerator()
                    .cmp(&left.1.weighted_utility_numerator())
                    .then_with(|| left.0.registration_id().cmp(right.0.registration_id()))
            });
            Ok(())
        }
        TieBreakPolicy::Named(policy) => Err(SelectionError::UnsupportedTieBreak(policy.clone())),
    }
}

fn decisive_components(score: &CandidateScore, objective: &Objective) -> Vec<ScoreComponentId> {
    let positive: BTreeSet<_> = CRITERIA
        .into_iter()
        .filter(|criterion| objective.preferences().get(*criterion).get() > 0)
        .collect();
    score
        .components()
        .iter()
        .filter(|component| positive.contains(&component.criterion()))
        .map(|component| component.component_id().clone())
        .collect()
}

fn exclusion_summaries(
    candidates: &[&CandidateConsideration],
) -> Result<Vec<ExclusionSummary>, SelectionError> {
    if candidates.is_empty() {
        return Ok(vec![ExclusionSummary::new(
            ExclusionPhase::Mechanical,
            ExclusionCode::new("catalog.empty")?,
            0,
        )?]);
    }
    let mut grouped: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for candidate in candidates {
        let reasons = if candidate.mechanical_admission().is_admitted() {
            candidate.policy_eligibility().reasons()
        } else {
            candidate.mechanical_admission().reasons()
        };
        for reason in reasons {
            grouped
                .entry((reason.phase(), reason.code().clone()))
                .or_default()
                .insert(candidate.candidate().registration_id().clone());
        }
    }
    grouped
        .into_iter()
        .map(|((phase, code), registrations)| {
            Ok(ExclusionSummary::new(
                phase,
                code,
                u64::try_from(registrations.len())
                    .map_err(|_| SelectionError::ArithmeticOverflow)?,
            )?)
        })
        .collect()
}

const fn criterion_label(criterion: PreferenceCriterion) -> &'static str {
    match criterion {
        PreferenceCriterion::Quality => "quality",
        PreferenceCriterion::VerifiedCoverage => "verified-coverage",
        PreferenceCriterion::Latency => "latency",
        PreferenceCriterion::TokenCost => "token-cost",
        PreferenceCriterion::MonetaryCost => "monetary-cost",
        PreferenceCriterion::ExecutionFailureRisk => "execution-failure-risk",
        PreferenceCriterion::HumanIntervention => "human-intervention",
        PreferenceCriterion::Exploration => "exploration",
    }
}

/// Static same-catalog root-selector bootstrap configuration.
///
/// The configured registration is resolved directly against the captured
/// catalog generation. This operation never invokes selection and therefore
/// cannot recursively select the selector used to begin selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSelectorConfig {
    registration_id: RegistrationId,
    selector_contract: TopologyContractKey,
}

impl RootSelectorConfig {
    /// Creates a static root-selector reference.
    #[must_use]
    pub const fn new(
        registration_id: RegistrationId,
        selector_contract: TopologyContractKey,
    ) -> Self {
        Self {
            registration_id,
            selector_contract,
        }
    }

    /// Resolves the configured selector directly from the same catalog.
    pub fn resolve(
        &self,
        catalog: &TopologyCatalog,
        snapshot: &TopologyCatalogSnapshot,
    ) -> Result<Arc<RegisteredTopology>, RootSelectorError> {
        let candidate = snapshot
            .candidate(&self.registration_id)
            .ok_or_else(|| RootSelectorError::UnknownRegistration(self.registration_id.clone()))?;
        if !candidate
            .descriptor()
            .contracts()
            .contains(&self.selector_contract)
        {
            return Err(RootSelectorError::ContractNotClaimed {
                registration: self.registration_id.clone(),
                contract: self.selector_contract.clone(),
            });
        }
        catalog
            .resolve_at(&self.registration_id, snapshot.generation())
            .map_err(RootSelectorError::Catalog)
    }
}

/// Failure while directly resolving a statically configured root selector.
#[derive(Debug)]
#[non_exhaustive]
pub enum RootSelectorError {
    /// The captured catalog does not contain the configured registration.
    UnknownRegistration(RegistrationId),
    /// The configured registration does not claim the selector contract.
    ContractNotClaimed {
        /// Configured registration.
        registration: RegistrationId,
        /// Required selector contract.
        contract: TopologyContractKey,
    },
    /// Native catalog resolution failed.
    Catalog(TopologyCatalogError),
}

impl fmt::Display for RootSelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRegistration(registration) => {
                write!(
                    formatter,
                    "root selector registration {registration} is absent"
                )
            }
            Self::ContractNotClaimed {
                registration,
                contract,
            } => write!(
                formatter,
                "root selector registration {registration} does not claim {contract}"
            ),
            Self::Catalog(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RootSelectorError {}
