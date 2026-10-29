//! Transparent, provenance-preserving task-profile aggregation.
//!
//! Embodiments retain ownership of task payloads and extraction. This point
//! receives only the closed [`TaskProfileEvidence`] vocabulary, combines
//! identical observations conservatively, and makes conflict or missing data
//! explicit without inventing a domain extension bag.

use std::fmt;

use polaris::graph::{Graph, GraphSignature};
use polaris::system::param::Out;
use polaris::system::plugin::{Schedule, Version};
use polaris::system::system;
use polaris::system::system::SystemError;

use crate::contracts::{
    ContractError, ExternalSessionRef, FeatureAssessment, Provenance, SourceKind, TaskFeatures,
    TaskId, TaskProfile, TaskProfileEvidence, UnknownReason,
};

/// Semantic role applied to transparent profile aggregation.
#[derive(Debug, Clone, Copy)]
pub struct OnProfile;
impl Schedule for OnProfile {}

/// Complete owned inputs for transparent profile aggregation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInputs {
    task_id: TaskId,
    external_session: Option<ExternalSessionRef>,
    evidence: TaskProfileEvidence,
    aggregation_policy_version: Version,
    policy_provenance: Vec<Provenance>,
}

impl ProfileInputs {
    /// Captures host identity, embodiment evidence, and explicit policy attribution.
    pub fn new(
        task_id: TaskId,
        external_session: Option<ExternalSessionRef>,
        evidence: TaskProfileEvidence,
        aggregation_policy_version: Version,
        mut policy_provenance: Vec<Provenance>,
    ) -> Result<Self, ProfileError> {
        if aggregation_policy_version == Version::new(0, 0, 0) {
            return Err(ProfileError::ZeroPolicyVersion);
        }
        policy_provenance.sort();
        policy_provenance.dedup();
        if policy_provenance.is_empty() {
            return Err(ProfileError::EmptyPolicyProvenance);
        }
        if !policy_provenance.iter().any(|attribution| {
            matches!(
                attribution.source_kind(),
                SourceKind::CosmosPolicy | SourceKind::PolicyDefault
            ) && attribution.source().version() == aggregation_policy_version
        }) {
            return Err(ProfileError::PolicyProvenanceVersionMismatch);
        }
        Ok(Self {
            task_id,
            external_session,
            evidence,
            aggregation_policy_version,
            policy_provenance,
        })
    }

    /// Returns the explicit aggregation-policy version.
    #[must_use]
    pub const fn aggregation_policy_version(&self) -> Version {
        self.aggregation_policy_version
    }
}

/// Error produced by transparent profile aggregation.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProfileError {
    /// A contract record rejected invalid output.
    Contract(ContractError),
    /// The aggregation policy used a zero semantic version.
    ZeroPolicyVersion,
    /// No provenance was supplied for aggregation decisions or missing values.
    EmptyPolicyProvenance,
    /// Policy provenance did not attest to the configured behavior version.
    PolicyProvenanceVersionMismatch,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => error.fmt(formatter),
            Self::ZeroPolicyVersion => {
                formatter.write_str("profile aggregation policy must have a non-zero version")
            }
            Self::EmptyPolicyProvenance => {
                formatter.write_str("profile aggregation requires policy provenance")
            }
            Self::PolicyProvenanceVersionMismatch => formatter.write_str(
                "profile policy provenance must attest to the aggregation behavior version",
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

impl From<ContractError> for ProfileError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

/// Returns the native structural contract of the transparent profiler graph.
#[must_use]
pub fn contract_signature() -> GraphSignature {
    GraphSignature::new()
        .require_output::<ProfileInputs>()
        .produce::<TaskProfile>()
}

/// Adds transparent profile aggregation to a Polaris graph.
pub fn build(graph: &mut Graph) {
    graph.add_system((OnProfile, aggregate_system));
}

#[system]
async fn aggregate_system(inputs: Out<ProfileInputs>) -> Result<TaskProfile, SystemError> {
    aggregate(&inputs).map_err(|error| SystemError::ExecutionError(error.to_string()))
}

/// Aggregates the closed evidence fields with one policy applied uniformly.
pub fn aggregate(inputs: &ProfileInputs) -> Result<TaskProfile, ProfileError> {
    let evidence = &inputs.evidence;
    TaskProfile::new(
        inputs.task_id.clone(),
        inputs.external_session.clone(),
        TaskFeatures {
            decomposability: aggregate_feature(
                &evidence.decomposability,
                &inputs.policy_provenance,
            )?,
            expected_independent_work: aggregate_feature(
                &evidence.expected_independent_work,
                &inputs.policy_provenance,
            )?,
            verifier: aggregate_feature(&evidence.verifier, &inputs.policy_provenance)?,
            uncertainty: aggregate_feature(&evidence.uncertainty, &inputs.policy_provenance)?,
            exploration_need: aggregate_feature(
                &evidence.exploration_need,
                &inputs.policy_provenance,
            )?,
            refinement_need: aggregate_feature(
                &evidence.refinement_need,
                &inputs.policy_provenance,
            )?,
            context_isolation_need: aggregate_feature(
                &evidence.context_isolation_need,
                &inputs.policy_provenance,
            )?,
            human_interaction_need: aggregate_feature(
                &evidence.human_interaction_need,
                &inputs.policy_provenance,
            )?,
            consequence: aggregate_feature(&evidence.consequence, &inputs.policy_provenance)?,
            inherent_task_risk: aggregate_feature(
                &evidence.inherent_task_risk,
                &inputs.policy_provenance,
            )?,
            horizon: aggregate_feature(&evidence.horizon, &inputs.policy_provenance)?,
            requested_latency_limit: aggregate_feature(
                &evidence.requested_latency_limit,
                &inputs.policy_provenance,
            )?,
            requested_token_limit: aggregate_feature(
                &evidence.requested_token_limit,
                &inputs.policy_provenance,
            )?,
            requested_monetary_limit: aggregate_feature(
                &evidence.requested_monetary_limit,
                &inputs.policy_provenance,
            )?,
        },
    )
    .map_err(ProfileError::from)
}

fn aggregate_feature<T: Clone + Eq>(
    observations: &[FeatureAssessment<T>],
    policy_provenance: &[Provenance],
) -> Result<FeatureAssessment<T>, ContractError> {
    let mut provenance = policy_provenance.to_vec();
    for observation in observations {
        provenance.extend_from_slice(observation.provenance());
    }
    provenance.sort();
    provenance.dedup();

    let known: Vec<_> = observations
        .iter()
        .filter_map(|observation| {
            observation.known_value().map(|value| {
                (
                    value,
                    observation.confidence().expect("known has confidence"),
                )
            })
        })
        .collect();
    if let Some((first, _)) = known.first()
        && known.iter().all(|(value, _)| *value == *first)
    {
        let confidence = known
            .iter()
            .map(|(_, confidence)| *confidence)
            .min()
            .expect("known observations are non-empty");
        return FeatureAssessment::known((*first).clone(), confidence, provenance);
    }
    if !known.is_empty() {
        return FeatureAssessment::unknown(UnknownReason::ConflictingEvidence, provenance);
    }

    let mut unknown_reasons = observations
        .iter()
        .filter_map(FeatureAssessment::unknown_reason);
    let Some(first_reason) = unknown_reasons.next() else {
        return FeatureAssessment::unknown(UnknownReason::NotObserved, provenance);
    };
    let reason = if unknown_reasons.all(|reason| reason == first_reason) {
        first_reason
    } else {
        UnknownReason::ConflictingEvidence
    };
    FeatureAssessment::unknown(reason, provenance)
}
