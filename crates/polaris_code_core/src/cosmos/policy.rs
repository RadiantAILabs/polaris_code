//! Selector configuration and the coding author priors it ranks.
//!
//! Cosmos's deterministic selector does not invent scores: it ranks the
//! [`ScoreComponent`]s an embodiment supplies, under the weights an
//! [`Objective`](polaris_cosmos::contracts::Objective) supplies. This module
//! is where `polaris_code` states what it believes about its own topologies,
//! and why.
//!
//! The four utilities below are author priors, not measurements, and each is
//! marked [`ScoreSource::AuthorPrior`] so a reader can tell. Once a run has
//! been recorded they are joined by a [`ScoreSource::HistoricalStatistics`]
//! component bound to a frozen statistics digest — see
//! [`candidate_facts`]'s `observed` argument.
//!
//! | | Quality | Coverage | Latency | Failure risk |
//! |---|---|---|---|---|
//! | `code.direct-edit` | 0.70 | 0.30 | 0.95 | 0.75 |
//! | `code.prediction-error-edit` | 0.90 | 0.40 | 0.60 | 0.90 |
//! | `code.worklist-sweep-edit` | 0.85 | 0.95 | 0.40 | 0.95 |
//! | `code.tree-question` | 0.35 | 0.10 | 0.98 | 0.90 |
//!
//! The reasoning: the direct shape is fastest because it does one pass, and
//! lowest-quality for the same reason — a landed-but-wrong edit is never
//! re-examined. The prediction-error shape pays a re-read to falsify its own
//! expectation, which is why its quality and failure-risk numbers are better
//! and its latency worse. The sweep audits every site in the tree, so it alone
//! has real coverage, and it cannot silently miss a site — at the cost of
//! latency linear in site count. The tree question is the fastest of all and
//! the weakest: it is nearly free, and it resolves *nothing* about the task
//! an edit objective is asking for, which is why its quality prior is low
//! enough that no edit objective picks it.
//!
//! # Declared capabilities
//!
//! Routing a question is a *hard* constraint, not a scoring preference. The
//! five topologies that write to the tree declare
//! [`SOURCE_MUTATION`]; an objective that must not write source names it
//! forbidden, and the selector excludes them on the ordinary
//! `capability.forbidden` channel, by name, rather than leaving a question to
//! be won on points by an edit. A declared capability is inert unless some
//! objective forbids it, so this changes nothing for the edit objectives.

use std::sync::Arc;

use polaris_cosmos::contracts::{
    CapabilityId, ConfidencePpm, ContentDigest, ContractError, CoveragePpm, DurationMs,
    EpistemicBasis, Measurement, MeasurementValue, PolicyRef, PreferenceCriterion, ProbabilityPpm,
    ScoreComponent, ScoreComponentId, ScoreSource, SelectorRef, SourceKind, Status, TaskProfile,
    UtilityPpm, VerifierCapability, VerifierProfile,
};
use polaris_cosmos::evidence::{
    BoundedEvidenceConfig, ProfileBucket, StatisticsRequest, TopologyStatsSnapshot,
};
use polaris_cosmos::selection::deterministic::{
    self, CandidatePolicyFacts, DeterministicSelectorConfig,
};
use polaris_cosmos::topology::{ConfigurationDigest, TopologyCatalogSnapshot};

use super::topology::{
    DIRECT, MODEL_PROPOSED, PREDICTION_ERROR, TEST_GATED, TREE_QUESTION, WORKLIST_SWEEP,
};
use super::{policy, provenance};

/// Capability a registration declares when it writes to the working tree.
///
/// Every edit-shaped coding topology has it; the answer shape does not. An
/// objective that must leave the tree alone forbids it, which is how asking a
/// question stops being a contest the edit shapes can win.
pub const SOURCE_MUTATION: &str = "polaris-code.source-mutation";

/// Capability-name prefix every registration's own self-tag carries.
///
/// [`forbid_all_but`] is the mechanism behind naming one exact topology: every
/// candidate always declares its own tag (see [`declared_capabilities`]), and
/// forbidding every tag but one leaves exactly one candidate able to reach
/// ranking, on the same `capability.forbidden` channel [`SOURCE_MUTATION`]
/// already uses for the answer objective. No second selection mechanism, no
/// override registry — one more capability, and one more objective built from
/// it.
const SELF_TAG_PREFIX: &str = "polaris-code.topology-self";

/// Policy naming how a raw measurement becomes a bounded utility.
pub const NORMALIZATION_POLICY: &str = "polaris-code.linear-utility";

/// Policy naming how missing preference evidence is treated.
pub const MISSING_PREFERENCE_POLICY: &str = "polaris-code.pessimistic-missing";

/// Policy naming how component confidence is combined.
pub const CONFIDENCE_POLICY: &str = "polaris-code.minimum-confidence";

/// Policy naming how contextual statistics are aggregated.
pub const STATISTICS_POLICY: &str = "polaris-code.coding-statistics";

/// Policy naming which observations count toward a cohort.
pub const OBSERVATION_POLICY: &str = "polaris-code.latest-observation-wins";

/// Policy naming the interval estimator applied to a cohort.
pub const INTERVAL_POLICY: &str = "polaris-code.integer-wilson";

/// Confidence attached to an author prior.
///
/// Deliberately below the 0.99 a deterministic file check reports: an author's
/// belief about a topology is weaker evidence than a measurement of one run.
const PRIOR_CONFIDENCE: u32 = 700_000;

/// Confidence attached to a utility read off recorded outcomes.
///
/// Above [`PRIOR_CONFIDENCE`]: a rate computed from runs that actually
/// happened is better evidence than an author's belief about them. Still below
/// the 0.99 a deterministic file check reports, because the retained window is
/// small and a small window is a small sample.
const OBSERVED_CONFIDENCE: u32 = 800_000;

/// One whole, in the integer parts-per-million every Cosmos ratio uses.
const ONE_PPM: u32 = 1_000_000;

struct Prior {
    quality: u32,
    coverage: u32,
    latency_ms: u64,
    latency_utility: u32,
    failure_utility: u32,
    failure_probability: u32,
}

fn prior(registration: &str) -> Option<Prior> {
    match registration {
        DIRECT => Some(Prior {
            quality: 700_000,
            coverage: 300_000,
            latency_ms: 40,
            latency_utility: 950_000,
            failure_utility: 750_000,
            failure_probability: 200_000,
        }),
        PREDICTION_ERROR => Some(Prior {
            quality: 900_000,
            coverage: 400_000,
            latency_ms: 120,
            latency_utility: 600_000,
            failure_utility: 900_000,
            failure_probability: 80_000,
        }),
        WORKLIST_SWEEP => Some(Prior {
            quality: 850_000,
            coverage: 950_000,
            latency_ms: 260,
            latency_utility: 400_000,
            failure_utility: 950_000,
            failure_probability: 40_000,
        }),
        TREE_QUESTION => Some(Prior {
            quality: 350_000,
            coverage: 100_000,
            latency_ms: 25,
            latency_utility: 980_000,
            failure_utility: 900_000,
            failure_probability: 100_000,
        }),
        // These candidates are not enabled for policy selection yet. Keeping
        // their catalog registrations complete but prior-free lets discovery
        // and mechanical admission remain distinct from host policy.
        _ => None,
    }
}

fn component(
    registration: &str,
    suffix: &str,
    criterion: PreferenceCriterion,
    value: MeasurementValue,
    utility: u32,
) -> Result<ScoreComponent, ContractError> {
    let confidence = ConfidencePpm::new(PRIOR_CONFIDENCE)?;
    ScoreComponent::new(
        ScoreComponentId::new(format!("{registration}.{suffix}"))?,
        criterion,
        ScoreSource::AuthorPrior,
        Measurement::new(
            value,
            None,
            confidence,
            vec![provenance(
                SourceKind::PolicyDefault,
                EpistemicBasis::Asserted,
            )],
        )?,
        policy(NORMALIZATION_POLICY),
        UtilityPpm::new(utility)?,
        confidence,
        vec![provenance(
            SourceKind::PolicyDefault,
            EpistemicBasis::Asserted,
        )],
        None,
    )
}

/// Builds the one score component that carries a measured, not asserted, value.
///
/// It is attributed to the host rather than to a policy default, and its basis
/// is [`EpistemicBasis::Verified`]: the statuses behind it were written by the
/// coding verifiers after real runs. It is bound to `statistics` because
/// Cosmos refuses a [`ScoreSource::HistoricalStatistics`] component whose
/// digest is not the snapshot the decision was made against — which is what
/// makes a past decision replayable.
fn observed_component(
    registration: &str,
    suffix: &str,
    criterion: PreferenceCriterion,
    value: MeasurementValue,
    utility: u32,
    statistics: ContentDigest,
) -> Result<ScoreComponent, ContractError> {
    let confidence = ConfidencePpm::new(OBSERVED_CONFIDENCE)?;
    let attribution = || {
        vec![provenance(
            SourceKind::HostObservation,
            EpistemicBasis::Verified,
        )]
    };
    ScoreComponent::new(
        ScoreComponentId::new(format!("{registration}.{suffix}"))?,
        criterion,
        ScoreSource::HistoricalStatistics,
        Measurement::new(value, None, confidence, attribution())?,
        policy(NORMALIZATION_POLICY),
        UtilityPpm::new(utility)?,
        confidence,
        attribution(),
        Some(statistics),
    )
}

/// Returns the immutable author priors stored with one complete registration.
///
/// Registrations without a pack-level selection policy intentionally carry no
/// priors. Mechanical admission remains authoritative regardless: discovery
/// does not imply that the host should rank every graph under every objective.
///
/// # Errors
///
/// Returns an error if a score component rejects its stable identity,
/// measurement, normalization policy, or provenance.
pub fn author_priors(registration: &str) -> Result<Vec<ScoreComponent>, ContractError> {
    let Some(prior) = prior(registration) else {
        return Ok(Vec::new());
    };
    Ok(vec![
        component(
            registration,
            "quality",
            PreferenceCriterion::Quality,
            MeasurementValue::Proportion(CoveragePpm::new(prior.quality)?),
            prior.quality,
        )?,
        component(
            registration,
            "coverage",
            PreferenceCriterion::VerifiedCoverage,
            MeasurementValue::Proportion(CoveragePpm::new(prior.coverage)?),
            prior.coverage,
        )?,
        component(
            registration,
            "latency",
            PreferenceCriterion::Latency,
            MeasurementValue::Duration(DurationMs::new(prior.latency_ms)),
            prior.latency_utility,
        )?,
        component(
            registration,
            "failure-risk",
            PreferenceCriterion::ExecutionFailureRisk,
            MeasurementValue::Proportion(CoveragePpm::new(prior.failure_probability)?),
            prior.failure_utility,
        )?,
    ])
}

/// The low-cardinality cohort bucket one task profile falls into.
///
/// This mirrors the bucketing
/// [`aggregate_snapshot`](polaris_cosmos::evidence::aggregate_snapshot)
/// applies when it writes a cohort key, and it has to: a lookup that bucketed
/// differently would never match the cohorts the evidence store actually
/// wrote, and every candidate would silently look unproven.
fn profile_bucket(profile: &TaskProfile) -> ProfileBucket {
    let features = profile.features();
    ProfileBucket {
        decomposability: features.decomposability.known_value().copied(),
        verifier_availability: features
            .verifier
            .known_value()
            .map(VerifierProfile::availability),
        horizon: features.horizon.known_value().copied(),
        consequence: features.consequence.known_value().copied(),
    }
}

/// Counts the outcome statuses recorded for one registration in one bucket.
///
/// Returns `(passes, applied)`. Every cohort in the frozen snapshot whose key
/// names this registration *and* this profile bucket is summed: one
/// registration can hold several cohorts in the same bucket — they differ by
/// resolved configuration, requested contract, and the execution provenance a
/// started run adds — and all of them are runs of the same topology on the
/// same shape of task.
///
/// [`Status::NotApplicable`] counts toward neither total. A check whose
/// prerequisite was absent never applied, so it is not a mark against the
/// candidate and not a mark for it either.
fn recorded_outcomes(
    statistics: &TopologyStatsSnapshot,
    bucket: &ProfileBucket,
    registration: &str,
) -> (u64, u64) {
    let mut passes: u64 = 0;
    let mut applied: u64 = 0;
    for cohort in &statistics.cohorts {
        if &cohort.key.profile != bucket || cohort.key.registration.as_str() != registration {
            continue;
        }
        for (status, count) in &cohort.observation_statuses {
            match status {
                Status::Pass => {
                    passes = passes.saturating_add(*count);
                    applied = applied.saturating_add(*count);
                }
                Status::Issue | Status::Indeterminate => {
                    applied = applied.saturating_add(*count);
                }
                Status::NotApplicable => {}
            }
        }
    }
    (passes, applied)
}

/// Returns the capability that names exactly `registration` and no other.
///
/// Every registration always carries its own tag (see
/// [`declared_capabilities`]), which is what makes it something an objective
/// can forbid.
///
/// # Errors
///
/// Returns an error if the composed name is rejected.
fn self_capability(registration: &str) -> Result<CapabilityId, ContractError> {
    CapabilityId::new(format!("{SELF_TAG_PREFIX}.{registration}"))
}

/// Returns the capabilities one registration declares it exercises.
///
/// Listed by registration rather than inferred, because "does this graph
/// write to the tree" is a claim about the graph's *behaviour* that no
/// signature exposes. The answer shape is absent from the mutation list, and
/// that absence is the load-bearing part. The self-tag is unconditional: every
/// registration carries one, mutating or not, so [`forbid_all_but`] can narrow
/// to any of them.
///
/// # Errors
///
/// Returns an error if a capability identity is rejected.
fn declared_capabilities(registration: &str) -> Result<Vec<CapabilityId>, ContractError> {
    let mutates = matches!(
        registration,
        DIRECT | PREDICTION_ERROR | WORKLIST_SWEEP | TEST_GATED | MODEL_PROPOSED
    );
    let mut capabilities = vec![self_capability(registration)?];
    if mutates {
        capabilities.push(CapabilityId::new(SOURCE_MUTATION)?);
    }
    Ok(capabilities)
}

/// Returns the capability set that forbids every catalog candidate but `only`.
///
/// Merged into an objective's own [`HardConstraints::forbidden_capabilities`]
/// (alongside anything the intent already forbids), this leaves `only` the
/// single candidate that cannot be excluded by this channel: every other
/// registration always declares its own tag (`declared_capabilities`), so a
/// candidate whose name does not match anything in the catalog forbids every
/// tag and yields the honest `NoEligibleCandidate` answer, not a silent
/// substitution.
///
/// # Errors
///
/// Returns an error if a composed capability name is rejected.
///
/// [`HardConstraints::forbidden_capabilities`]: polaris_cosmos::contracts::HardConstraints::forbidden_capabilities
pub fn forbid_all_but(
    catalog: &TopologyCatalogSnapshot,
    only: &str,
) -> Result<Vec<CapabilityId>, ContractError> {
    catalog
        .candidates()
        .iter()
        .map(|candidate| candidate.registration_id().as_str())
        .filter(|id| *id != only)
        .map(self_capability)
        .collect()
}

/// Builds the dynamic selector facts around catalog-owned author priors.
///
/// Candidate identity and static score components come from the sealed
/// catalog; this function supplies the remaining selector-policy shape and,
/// when `observed` is given, one [`ScoreSource::HistoricalStatistics`]
/// component per candidate that the recorded past actually supports.
///
/// `observed` is the frozen statistics snapshot the decision is being made
/// against, together with the profile of the task in front of the selector.
/// Passing `None` is the cold start: no run has been recorded yet, so the
/// author priors decide alone.
///
/// # The learning window
///
/// What a candidate's record is allowed to say here is exactly:
///
/// * **the retained evidence**, and no more. The snapshot is derived from the
///   bounded store [`evidence_config`] configures — 16 episodes and 64
///   outcome observations. Reaching further back would need a second,
///   durable store, which this host does not have. It is also the right
///   bound on its own terms: a topology that has been fixed should stop being
///   judged on how it used to behave, and a window that forgets is what
///   lets it recover.
/// * **under this task's shape**, and no other. Cohorts are matched on
///   [`ProfileBucket`], so how a topology fared on a decomposable,
///   verifier-backed task is not evidence about an opaque one.
/// * **once there is enough of it**. A cohort speaks only when its applied
///   observations reach the snapshot's `minimum_sample_count`, which is the
///   sufficiency threshold [`statistics_request`] already declares. No second
///   threshold is introduced here.
/// * **about quality, and nothing else.** Pass/issue statuses are the only
///   outcome signal the coding verifiers put into a cohort: their measurement
///   ids (`edit-applied`, `stub-free-coverage`) are outside Cosmos's v1
///   aggregate set, so `CohortStatistics::measurements` stays empty. Coverage,
///   latency and failure risk therefore keep their priors — there is no
///   measured fact to replace them with, and inventing one would be the very
///   thing this is meant to stop.
///
/// A candidate with no matching observations gets no historical component and
/// is scored on its priors alone, so it stays fully selectable. The selector
/// takes the *minimum* utility across the components of a criterion, so a
/// recorded record can only ever hold a candidate back, never promote one
/// above what its author claimed for it.
///
/// # Errors
///
/// Returns an error if a score component or identity is rejected.
pub fn candidate_facts(
    catalog: &TopologyCatalogSnapshot,
    observed: Option<(&TopologyStatsSnapshot, &TaskProfile)>,
) -> Result<Vec<CandidatePolicyFacts>, ContractError> {
    let bucket = observed.map(|(_, profile)| profile_bucket(profile));
    let mut facts = Vec::with_capacity(catalog.candidates().len());
    for registered in catalog.candidates() {
        let registration = registered.registration_id();
        let id = registration.as_str();
        let mut components = registered
            .admission_metadata()
            .map(|metadata| metadata.author_priors().to_vec())
            .unwrap_or_default();
        let estimated_latency = components.iter().find_map(|component| {
            if component.criterion() == PreferenceCriterion::Latency
                && let MeasurementValue::Duration(value) = component.raw_measurement().value()
            {
                Some(*value)
            } else {
                None
            }
        });
        let failure_probability = components.iter().find_map(|component| {
            if component.criterion() == PreferenceCriterion::ExecutionFailureRisk
                && let MeasurementValue::Proportion(value) = component.raw_measurement().value()
            {
                Some(value.get())
            } else {
                None
            }
        });
        if let Some((statistics, _)) = observed
            && let Some(bucket) = bucket.as_ref()
        {
            let (passes, applied) = recorded_outcomes(statistics, bucket, id);
            if applied > 0 && applied >= statistics.minimum_sample_count {
                // `passes <= applied`, so the scaled quotient never exceeds
                // one whole and the conversion always succeeds; the fallback
                // states that bound instead of introducing a panicking path.
                let rate =
                    u32::try_from(u128::from(passes) * u128::from(ONE_PPM) / u128::from(applied))
                        .unwrap_or(ONE_PPM);
                components.push(observed_component(
                    id,
                    "observed-quality",
                    PreferenceCriterion::Quality,
                    MeasurementValue::Proportion(CoveragePpm::new(rate)?),
                    rate,
                    statistics.content_digest(),
                )?);
            }
        }
        let mut candidate = CandidatePolicyFacts::new(registration.clone())
            .with_verifier_capabilities(vec![VerifierCapability::Deterministic])
            .with_capabilities(declared_capabilities(id)?)
            .with_score_components(components);
        if let Some(value) = estimated_latency {
            candidate = candidate.with_estimated_latency(value);
        }
        if let Some(value) = failure_probability {
            candidate = candidate.with_execution_failure_probability(ProbabilityPpm::new(value)?);
        }
        facts.push(candidate);
    }
    Ok(facts)
}

/// Why the coding selector could not be configured.
///
/// Three distinct upstream error types meet in one constructor chain
/// (`IdentifierError`, `ContractError`, `SelectionError`) and Cosmos offers no
/// common error, so a consumer has to introduce its own union here.
#[derive(Debug)]
#[non_exhaustive]
pub enum PolicyError {
    /// A Cosmos record rejected a value.
    Contract(ContractError),
    /// A stable name or digest was rejected.
    Identifier(polaris_cosmos::topology::IdentifierError),
    /// The selector rejected its configuration.
    Selector(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => error.fmt(formatter),
            Self::Identifier(error) => error.fmt(formatter),
            Self::Selector(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for PolicyError {}

impl From<ContractError> for PolicyError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

impl From<polaris_cosmos::topology::IdentifierError> for PolicyError {
    fn from(error: polaris_cosmos::topology::IdentifierError) -> Self {
        Self::Identifier(error)
    }
}

/// Returns the deterministic selector configuration for coding selection.
///
/// # Errors
///
/// Returns an error if the selector reference or policy set is rejected.
pub fn selector_config() -> Result<DeterministicSelectorConfig, PolicyError> {
    DeterministicSelectorConfig::new(
        SelectorRef::new(
            deterministic::selector_key(),
            ConfigurationDigest::new(Arc::<str>::from(
                ContentDigest::sha256(b"polaris-code.selector-config").to_string(),
            ))?,
        )?,
        policy(MISSING_PREFERENCE_POLICY),
        policy(CONFIDENCE_POLICY),
        vec![provenance(
            SourceKind::CosmosPolicy,
            EpistemicBasis::Asserted,
        )],
    )
    .map_err(|error| PolicyError::Selector(error.to_string()))
}

/// Returns the statistics request the host captures cohorts under.
///
/// # Panics
///
/// Never: the minimum sample count is a non-zero constant.
#[must_use]
pub fn statistics_request() -> StatisticsRequest {
    StatisticsRequest {
        statistics_policy: policy(STATISTICS_POLICY),
        observation_selection_policy: policy(OBSERVATION_POLICY),
        interval_policy: policy(INTERVAL_POLICY),
        minimum_sample_count: std::num::NonZeroUsize::new(1).expect("one is non-zero"),
    }
}

/// Returns the bounded evidence limits this host runs the default store under.
///
/// # Panics
///
/// Never: every limit is a non-zero constant.
#[must_use]
pub fn evidence_config() -> BoundedEvidenceConfig {
    let limit = |value: usize| std::num::NonZeroUsize::new(value).expect("limit is non-zero");
    BoundedEvidenceConfig {
        max_episodes: limit(16),
        max_runs: limit(16),
        max_observations_total: limit(64),
        max_observations_per_run: limit(8),
        max_evidence_refs_per_record: limit(16),
        max_open_runs: limit(4),
    }
}

/// Returns the named policy under which coding objectives are derived.
#[must_use]
pub fn objective_policy() -> PolicyRef {
    policy("polaris-code.objective-derivation")
}
