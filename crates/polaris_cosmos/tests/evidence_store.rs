//! Behavioural coverage for replaceable topology-evidence providers.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

mod contracts {
    pub use polaris_cosmos::contracts::*;
}

mod topology {
    pub use polaris_cosmos::topology::*;
}

#[allow(dead_code)]
#[path = "../src/evidence/store.rs"]
mod evidence_store;

#[allow(dead_code, unused_imports)]
mod fixtures {
    // Test-local constructors mirror the canonical contract fixtures so this
    // test does not depend on the shared fixture target.
    use std::sync::Arc;

    use polaris::system::plugin::Version;
    use polaris_cosmos::contracts::{
        AdmissionEvaluation, AdmissionResult, AvailabilityObservation, AvailabilitySnapshotId,
        AvailabilityState, BindingAlias, BindingAvailability, BindingKindId,
        CandidateConsideration, CandidateScore, CanonicalRecord, CapabilityAvailability,
        CapabilityId, CapabilityKey, CardinalityEstimate, ConfidencePpm, ConfigurationEntry,
        ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ConsequenceLevel,
        ContentDigest, ContextTypeId, ContextTypeKey, ControlFlowChoice, CoveragePpm,
        DecompositionShape, DurationMs, EmbodimentId, EmbodimentKey, EmptyStatisticsSnapshot,
        EnvironmentId, EnvironmentKey, EpistemicBasis, ErrorClassId, EvidenceKind,
        EvidenceObjectId, EvidenceRef, EvidenceStoreId, EvidenceTime, ExclusionCode,
        ExclusionPhase, ExclusionReason, ExclusionSummary, ExecutionDisposition, ExecutionFault,
        ExecutionProvenance, ExecutionStage, ExternalSessionRef, FallbackCandidate,
        FeatureAssessment, HardConstraints, HostRuntimeId, HostRuntimeKey, LogicalSequence,
        Measurement, MeasurementEntry, MeasurementId, MeasurementValue, MechanismAvailability,
        MissingPreferencePolicy, ModelId, ModelProvenance, NeedLevel, Objective, ObservedLimit,
        OutcomeObservation, OutputAvailability, PolicyEligibility, PolicyId, PolicyLabelId,
        PolicyRef, PreferenceBasisPoints, PreferenceCriterion, PreferenceWeights, ProbabilityPpm,
        Provenance, ProviderAvailability, ProviderId, ProviderKey, RecordSubjectId, RecordedAt,
        RegistrationSelectionRef, ResidualDisposition, ResolvedConfigurationRef, ResourceAccess,
        ResourceAvailability, RiskLevel, RoleScheduleCount, RoleScheduleId, RunAbandoned,
        RunCompletion, RunControlFlow, RunFaults, RunFinalized, RunOpened, RunStructure,
        RunTerminal, RunUsage, RuntimeAvailabilitySnapshot, RuntimeMechanismId,
        RuntimeMechanismKey, SamplingProvenance, SchemaVersion, ScoreComponent, ScoreComponentId,
        ScoreSource, SelectionDecision, SelectionDecisionId, SelectionInputRefs,
        SelectionNotStarted, SelectionOutcome, SelectionRationale, SelectionRecord, SelectorId,
        SelectorKey, SelectorRef, SourceId, SourceKey, SourceKind, StatisticsWindow, Status,
        StopReason, SupersedeReason, SupersedesClaim, TaskFeatures, TaskHorizon, TaskId,
        TaskProfile, TieBreakPolicy, TopologyRunRecord, UnknownEstimatePolicy, UnknownReason,
        UtilityPpm, VerifierAvailability, VerifierCapability, VerifierProfile,
    };
    use polaris_cosmos::topology::{
        ConfigurationDigest, RegistrationId, TopologyContractId, TopologyContractKey, TopologyId,
        TopologyKey,
    };

    const DECISION_A: &str = "67e55044-10b1-426f-8247-bb680e5fe0c1";
    const DECISION_B: &str = "67e55044-10b1-426f-8247-bb680e5fe0c2";
    const AVAILABILITY: &str = "67e55044-10b1-426f-8247-bb680e5fe0c3";
    const ADMISSION: &str = "67e55044-10b1-426f-8247-bb680e5fe0c4";
    const RUN: &str = "67e55044-10b1-426f-8247-bb680e5fe0c5";
    const OBSERVATION_A: &str = "67e55044-10b1-426f-8247-bb680e5fe0c6";
    const OBSERVATION_B: &str = "67e55044-10b1-426f-8247-bb680e5fe0c7";

    fn version() -> Version {
        Version::new(1, 0, 0)
    }

    fn policy(id: &str) -> PolicyRef {
        PolicyRef::new(PolicyId::new(id).unwrap(), version())
    }

    fn source() -> SourceKey {
        SourceKey::new(SourceId::new("fixtures.source").unwrap(), version())
    }

    fn evidence(id: &str) -> EvidenceRef {
        EvidenceRef::new(
            EvidenceStoreId::new("fixtures.store").unwrap(),
            EvidenceKind::VerifierOutput,
            EvidenceObjectId::new(id).unwrap(),
        )
    }

    fn provenance(id: &str, kind: SourceKind) -> Provenance {
        Provenance::new(source(), kind, EpistemicBasis::Verified).with_evidence(evidence(id))
    }

    fn known<T>(value: T, id: &str) -> FeatureAssessment<T> {
        FeatureAssessment::known(
            value,
            ConfidencePpm::new(900_000).unwrap(),
            vec![provenance(id, SourceKind::EmbodimentExtractor)],
        )
        .unwrap()
    }

    fn unknown<T>(id: &str) -> FeatureAssessment<T> {
        FeatureAssessment::unknown(
            UnknownReason::NotObserved,
            vec![provenance(id, SourceKind::EmbodimentExtractor)],
        )
        .unwrap()
    }

    fn profile(namespace: &str, task: &str, shape: DecompositionShape) -> TaskProfile {
        TaskProfile::new(
            TaskId::new(namespace, task).unwrap(),
            Some(ExternalSessionRef::new("fixture-session", "session-1").unwrap()),
            TaskFeatures {
                decomposability: known(shape, "profile.decomposition"),
                expected_independent_work: known(
                    CardinalityEstimate::new(1, 3, 4).unwrap(),
                    "profile.cardinality",
                ),
                verifier: known(
                    VerifierProfile::new(
                        VerifierAvailability::Available,
                        vec![VerifierCapability::Deterministic],
                    )
                    .unwrap(),
                    "profile.verifier",
                ),
                uncertainty: known(NeedLevel::Medium, "profile.uncertainty"),
                exploration_need: known(NeedLevel::Low, "profile.exploration"),
                refinement_need: known(NeedLevel::Medium, "profile.refinement"),
                context_isolation_need: known(NeedLevel::High, "profile.isolation"),
                human_interaction_need: known(NeedLevel::Low, "profile.human"),
                consequence: known(ConsequenceLevel::Moderate, "profile.consequence"),
                inherent_task_risk: known(RiskLevel::Moderate, "profile.risk"),
                horizon: known(TaskHorizon::Iterative, "profile.horizon"),
                requested_latency_limit: known(
                    ObservedLimit::AtMost(DurationMs::new(30_000)),
                    "profile.latency",
                ),
                requested_token_limit: unknown("profile.tokens"),
                requested_monetary_limit: unknown("profile.money"),
            },
        )
        .unwrap()
    }

    fn objective() -> Objective {
        let preferences = PreferenceWeights::new(
            PreferenceBasisPoints::new(3_500).unwrap(),
            PreferenceBasisPoints::new(3_500).unwrap(),
            PreferenceBasisPoints::new(500).unwrap(),
            PreferenceBasisPoints::new(400).unwrap(),
            PreferenceBasisPoints::new(400).unwrap(),
            PreferenceBasisPoints::new(1_500).unwrap(),
            PreferenceBasisPoints::new(200).unwrap(),
            PreferenceBasisPoints::new(0).unwrap(),
        )
        .unwrap();
        Objective::new(
            HardConstraints::none()
                .with_required_verifier_capabilities(vec![VerifierCapability::Deterministic])
                .with_unknown_estimate_policy(UnknownEstimatePolicy::Reject),
            preferences,
            MissingPreferencePolicy::Pessimistic,
            TieBreakPolicy::AscendingRegistrationId,
            vec![provenance("objective.policy", SourceKind::PolicyDefault)],
        )
        .unwrap()
    }

    fn topology_contract() -> TopologyContractKey {
        TopologyContractKey::new(
            TopologyContractId::new("fixture.task-result").unwrap(),
            version(),
        )
    }

    fn selected_registration(id: &str) -> RegistrationSelectionRef {
        let value = ConfigurationValue::map(vec![
            ConfigurationEntry::new("mode", ConfigurationValue::String("bounded".into())).unwrap(),
            ConfigurationEntry::new("workers", ConfigurationValue::Unsigned(3)).unwrap(),
        ])
        .unwrap();
        let configuration = ResolvedConfigurationRef::inline(
            ConfigurationSchemaKey::new(
                ConfigurationSchemaId::new("fixture.topology-config").unwrap(),
                version(),
            ),
            value,
        )
        .unwrap();
        RegistrationSelectionRef::new(
            RegistrationId::new(id).unwrap(),
            TopologyKey::new(TopologyId::new("fixture.survey-fold").unwrap(), version()),
            ContentDigest::sha256(format!("revision:{id}").as_bytes()),
            configuration,
        )
        .unwrap()
    }

    fn criterion_utilities() -> Vec<polaris_cosmos::contracts::CriterionUtility> {
        [
            PreferenceCriterion::Quality,
            PreferenceCriterion::VerifiedCoverage,
            PreferenceCriterion::Latency,
            PreferenceCriterion::TokenCost,
            PreferenceCriterion::MonetaryCost,
            PreferenceCriterion::ExecutionFailureRisk,
            PreferenceCriterion::HumanIntervention,
            PreferenceCriterion::Exploration,
        ]
        .into_iter()
        .map(|criterion| {
            polaris_cosmos::contracts::CriterionUtility::new(
                criterion,
                UtilityPpm::new(750_000).unwrap(),
            )
        })
        .collect()
    }

    fn candidate_score(component_id: &str, weights: &PreferenceWeights) -> CandidateScore {
        let measurement = Measurement::new(
            MeasurementValue::Proportion(CoveragePpm::new(750_000).unwrap()),
            Some(policy("fixture.quality-calibration")),
            ConfidencePpm::new(850_000).unwrap(),
            vec![provenance("score.measurement", SourceKind::CosmosPolicy)],
        )
        .unwrap();
        let component = ScoreComponent::new(
            ScoreComponentId::new(component_id).unwrap(),
            PreferenceCriterion::Quality,
            ScoreSource::AuthorPrior,
            measurement,
            policy("fixture.normalize-quality"),
            UtilityPpm::new(750_000).unwrap(),
            ConfidencePpm::new(850_000).unwrap(),
            vec![provenance("score.component", SourceKind::CosmosPolicy)],
            None,
        )
        .unwrap();
        CandidateScore::new(vec![component], criterion_utilities(), weights).unwrap()
    }

    fn empty_stats() -> EmptyStatisticsSnapshot {
        EmptyStatisticsSnapshot::new(
            policy("fixture.statistics"),
            policy("fixture.observation-selection"),
            policy("fixture.interval"),
            RecordedAt::new(LogicalSequence::new(0), Some(1_000)),
            StatisticsWindow::new(LogicalSequence::new(0), LogicalSequence::new(0), 0, 0).unwrap(),
            3,
        )
        .unwrap()
    }

    fn availability_snapshot(
        snapshot_id: &str,
        captured_at_ms: u64,
    ) -> RuntimeAvailabilitySnapshot {
        let provider = ProviderKey::new(ProviderId::new("fixture.provider").unwrap(), version());
        let observed = AvailabilityObservation::new(
            AvailabilityState::Available,
            Some(provider.clone()),
            Some(DurationMs::new(10_000)),
            vec![provenance("availability.fact", SourceKind::HostObservation)],
        )
        .unwrap();
        let unknown = AvailabilityObservation::new(
            AvailabilityState::Unknown,
            None,
            None,
            vec![provenance(
                "availability.future-capability",
                SourceKind::HostObservation,
            )],
        )
        .unwrap();

        RuntimeAvailabilitySnapshot::new(
            snapshot_id.parse::<AvailabilitySnapshotId>().unwrap(),
            EvidenceTime::new(None, Some(captured_at_ms)).unwrap(),
            EnvironmentKey::new(
                EnvironmentId::new("fixture.environment").unwrap(),
                version(),
            ),
            vec![
                ResourceAvailability::new(
                    ContextTypeKey::new(
                        ContextTypeId::new("fixture.model-resource").unwrap(),
                        version(),
                    ),
                    ResourceAccess::Read,
                    observed.clone(),
                )
                .unwrap(),
            ],
            vec![
                OutputAvailability::new(
                    ContextTypeKey::new(
                        ContextTypeId::new("fixture.task-output").unwrap(),
                        version(),
                    ),
                    observed.clone(),
                )
                .unwrap(),
            ],
            vec![
                ProviderAvailability::new(
                    provider.clone(),
                    ContentDigest::sha256(b"provider-config"),
                    observed.clone(),
                )
                .unwrap(),
            ],
            vec![
                CapabilityAvailability::new(
                    CapabilityKey::new(
                        CapabilityId::new("future.streaming-proof").unwrap(),
                        version(),
                    ),
                    unknown,
                )
                .unwrap(),
            ],
            vec![
                MechanismAvailability::new(
                    RuntimeMechanismKey::new(
                        RuntimeMechanismId::new("fixture.sequential").unwrap(),
                        version(),
                    ),
                    observed.clone(),
                )
                .unwrap(),
            ],
            vec![
                BindingAvailability::new(
                    BindingAlias::new("fixture.model-binding").unwrap(),
                    BindingKindId::new("model-provider").unwrap(),
                    provider,
                    observed,
                )
                .unwrap(),
            ],
            vec![PolicyLabelId::new("fixture.private-network").unwrap()],
            vec![provenance(
                "availability.snapshot",
                SourceKind::HostObservation,
            )],
        )
        .unwrap()
    }

    fn selected_decision(
        decision_id: &str,
        profile: &TaskProfile,
        objective: &Objective,
    ) -> SelectionDecision {
        let stats = empty_stats();
        let candidate_ref = selected_registration("fixture.registration-a");
        let fallback_ref = selected_registration("fixture.registration-b");
        let excluded_ref = selected_registration("fixture.registration-c");
        let component_id = "fixture.quality-prior";
        let candidate = CandidateConsideration::eligible(
            candidate_ref.clone(),
            candidate_score(component_id, objective.preferences()),
            1,
            ProbabilityPpm::ONE,
        )
        .unwrap();
        let fallback = CandidateConsideration::eligible(
            fallback_ref.clone(),
            candidate_score("fixture.fallback-quality-prior", objective.preferences()),
            2,
            ProbabilityPpm::ZERO,
        )
        .unwrap();
        let excluded = CandidateConsideration::mechanically_excluded(
            excluded_ref,
            AdmissionResult::excluded(vec![ExclusionReason::new(
                ExclusionPhase::Mechanical,
                ExclusionCode::new("contract.unsatisfied").unwrap(),
                vec![RecordSubjectId::new("fixture.task-result").unwrap()],
                vec![ContentDigest::sha256(b"catalog")],
            )])
            .unwrap(),
        )
        .unwrap();
        let outcome = SelectionOutcome::Selected {
            selected: candidate_ref,
            fallback_plan: vec![FallbackCandidate::new(fallback_ref).unwrap()],
            rationale: SelectionRationale::new(
                vec![ScoreComponentId::new(component_id).unwrap()],
                false,
                false,
                Some("Highest quality utility under the explicit objective.".into()),
                policy("fixture.confidence"),
            )
            .unwrap(),
            confidence: ConfidencePpm::new(850_000).unwrap(),
            selected_propensity: ProbabilityPpm::ONE,
            sampling: None,
        };
        SelectionDecision::new(
            decision_id.parse::<SelectionDecisionId>().unwrap(),
            profile.task_id().clone(),
            profile.external_session().cloned(),
            SelectorRef::new(
                SelectorKey::new(SelectorId::new("fixture.selector").unwrap(), version()),
                ConfigurationDigest::new(Arc::<str>::from(
                    ContentDigest::sha256(b"selector-config").to_string(),
                ))
                .unwrap(),
            )
            .unwrap(),
            topology_contract(),
            SelectionInputRefs::new(
                profile.content_digest(),
                objective.content_digest(),
                ContentDigest::sha256(b"catalog"),
                AVAILABILITY.parse().unwrap(),
                ContentDigest::sha256(b"availability"),
                stats.content_digest(),
            ),
            vec![excluded, fallback, candidate],
            outcome,
        )
        .unwrap()
    }

    fn execution_provenance() -> ExecutionProvenance {
        let provider = ProviderKey::new(ProviderId::new("fixture.provider").unwrap(), version());
        ExecutionProvenance::new(
            EmbodimentKey::new(EmbodimentId::new("fixture.embodiment").unwrap(), version()),
            HostRuntimeKey::new(HostRuntimeId::new("fixture.runtime").unwrap(), version()),
            vec![provider.clone()],
            vec![
                ModelProvenance::new(
                    provider,
                    ModelId::new("fixture.model").unwrap(),
                    Some("2026-09".into()),
                )
                .unwrap(),
            ],
            vec![provenance("run.provenance", SourceKind::HostObservation)],
        )
        .unwrap()
    }

    struct StartedFixture {
        profile: TaskProfile,
        objective: Objective,
        decision: SelectionDecision,
        admission: AdmissionEvaluation,
        opened: RunOpened,
    }

    fn started_fixture(namespace: &str, task: &str, decision_id: &str) -> StartedFixture {
        let profile = profile(namespace, task, DecompositionShape::IndependentParts);
        let objective = objective();
        let decision = selected_decision(decision_id, &profile, &objective);
        let admission = AdmissionEvaluation::new(
            ADMISSION.parse().unwrap(),
            decision.decision_id(),
            match decision.outcome() {
                SelectionOutcome::Selected { selected, .. } => selected.clone(),
                SelectionOutcome::NoEligibleCandidate { .. } => unreachable!(),
            },
            decision.input_refs().catalog_snapshot_digest(),
            decision.input_refs().availability_snapshot_id(),
            decision.input_refs().availability_snapshot_digest(),
            AdmissionResult::Admitted,
            RecordedAt::new(LogicalSequence::new(2), Some(2_000)),
        )
        .unwrap();
        let opened = RunOpened::new(
            RUN.parse().unwrap(),
            RecordedAt::new(LogicalSequence::new(3), Some(3_000)),
            &decision,
            &admission,
            vec![topology_contract()],
            0,
            execution_provenance(),
        )
        .unwrap();
        StartedFixture {
            profile,
            objective,
            decision,
            admission,
            opened,
        }
    }

    fn verified_success(value: CoveragePpm, evidence_id: &str) -> MeasurementEntry {
        MeasurementEntry::new(
            MeasurementId::new("verified-success").unwrap(),
            Measurement::new(
                MeasurementValue::Proportion(value),
                None,
                ConfidencePpm::new(950_000).unwrap(),
                vec![provenance(evidence_id, SourceKind::AutomatedVerifier)],
            )
            .unwrap(),
        )
        .unwrap()
    }

    pub(super) struct EpisodeRecords {
        pub selection: SelectionRecord,
        pub admission: AdmissionEvaluation,
        pub opened: RunOpened,
        pub finalized: RunFinalized,
        pub observations: Vec<OutcomeObservation>,
    }

    pub(super) fn completed_episode(
        namespace: &str,
        task: &str,
        decision_id: &str,
        admission_id: &str,
        run_id: &str,
        observation_ids: &[&str],
        first_sequence: u64,
    ) -> EpisodeRecords {
        let profile = profile(namespace, task, DecompositionShape::IndependentParts);
        let objective = objective();
        let decision = selected_decision(decision_id, &profile, &objective);
        let selection = SelectionRecord::new(
            RecordedAt::new(LogicalSequence::new(first_sequence), None),
            decision.clone(),
            profile.clone(),
            objective,
        )
        .unwrap();
        let candidate = match decision.outcome() {
            SelectionOutcome::Selected { selected, .. } => selected.clone(),
            SelectionOutcome::NoEligibleCandidate { .. } => unreachable!(),
        };
        let admission = AdmissionEvaluation::new(
            admission_id.parse().unwrap(),
            decision.decision_id(),
            candidate,
            decision.input_refs().catalog_snapshot_digest(),
            decision.input_refs().availability_snapshot_id(),
            decision.input_refs().availability_snapshot_digest(),
            AdmissionResult::Admitted,
            RecordedAt::new(LogicalSequence::new(first_sequence + 1), None),
        )
        .unwrap();
        let opened = RunOpened::new(
            run_id.parse().unwrap(),
            RecordedAt::new(LogicalSequence::new(first_sequence + 2), None),
            &decision,
            &admission,
            vec![topology_contract()],
            0,
            execution_provenance(),
        )
        .unwrap();
        let finalized = RunFinalized::new(
            opened.run_id(),
            RecordedAt::new(LogicalSequence::new(first_sequence + 3), None),
            DurationMs::new(500),
            RunStructure::new(1, 1, Some(0), 1, 1, Vec::new()).unwrap(),
            RunControlFlow::new(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            RunFaults::new(Vec::new(), 0, 0, None),
            RunUsage::empty(),
            RunCompletion::new(
                ExecutionDisposition::Completed,
                Some(StopReason::GoalMet),
                Some(CoveragePpm::ONE),
                Some(0),
            )
            .unwrap(),
            Vec::new(),
        )
        .unwrap();
        let observations = observation_ids
            .iter()
            .enumerate()
            .map(|(index, observation_id)| {
                let pass = index % 2 == 0;
                let status = if pass { Status::Pass } else { Status::Issue };
                let value = if pass {
                    CoveragePpm::ONE
                } else {
                    CoveragePpm::ZERO
                };
                OutcomeObservation::new(
                    observation_id.parse().unwrap(),
                    opened.run_id(),
                    profile.task_id().clone(),
                    RecordedAt::new(
                        LogicalSequence::new(first_sequence + 4 + index as u64),
                        None,
                    ),
                    None,
                    Some(DurationMs::new(10_000 * index as u64)),
                    status,
                    None,
                    vec![verified_success(value, &format!("outcome.{index}"))],
                    None,
                    Vec::new(),
                    None,
                    source(),
                    if pass {
                        SourceKind::AutomatedVerifier
                    } else {
                        SourceKind::DownstreamSignal
                    },
                    ConfidencePpm::new(900_000).unwrap(),
                    vec![provenance(
                        &format!("outcome.{index}"),
                        if pass {
                            SourceKind::AutomatedVerifier
                        } else {
                            SourceKind::DownstreamSignal
                        },
                    )],
                    Vec::new(),
                )
                .unwrap()
            })
            .collect();
        EpisodeRecords {
            selection,
            admission,
            opened,
            finalized,
            observations,
        }
    }

    pub(super) fn selected_record(
        namespace: &str,
        task: &str,
        decision_id: &str,
        sequence: u64,
    ) -> SelectionRecord {
        let profile = profile(namespace, task, DecompositionShape::Sequential);
        let objective = objective();
        SelectionRecord::new(
            RecordedAt::new(LogicalSequence::new(sequence), None),
            selected_decision(decision_id, &profile, &objective),
            profile,
            objective,
        )
        .unwrap()
    }

    pub(super) fn abandoned(opened: &RunOpened, sequence: u64) -> RunAbandoned {
        RunAbandoned::new(
            opened.run_id(),
            RecordedAt::new(LogicalSequence::new(sequence), None),
            policy("fixture.recovery"),
            vec![provenance("run.recovery", SourceKind::HostObservation)],
            Vec::new(),
        )
        .unwrap()
    }

    pub(super) fn no_start_episode(
        decision_id: &str,
        admission_id: &str,
    ) -> (SelectionRecord, AdmissionEvaluation, SelectionNotStarted) {
        let selection = selected_record("coding", "cannot-start", decision_id, 1);
        let decision = selection.decision();
        let candidate = match decision.outcome() {
            SelectionOutcome::Selected { selected, .. } => selected.clone(),
            SelectionOutcome::NoEligibleCandidate { .. } => unreachable!(),
        };
        let reason = ExclusionReason::new(
            ExclusionPhase::LiveReadmission,
            ExclusionCode::new("provider.drained").unwrap(),
            Vec::new(),
            vec![decision.input_refs().availability_snapshot_digest()],
        );
        let admission = AdmissionEvaluation::new(
            admission_id.parse().unwrap(),
            decision.decision_id(),
            candidate,
            decision.input_refs().catalog_snapshot_digest(),
            decision.input_refs().availability_snapshot_id(),
            decision.input_refs().availability_snapshot_digest(),
            AdmissionResult::excluded(vec![reason.clone()]).unwrap(),
            RecordedAt::new(LogicalSequence::new(2), None),
        )
        .unwrap();
        let no_start = SelectionNotStarted::new(
            decision.decision_id(),
            RecordedAt::new(LogicalSequence::new(3), None),
            vec![admission.attempt_id()],
            vec![reason],
        )
        .unwrap();
        (selection, admission, no_start)
    }

    pub(super) fn credit(
        run: &RunOpened,
        observation: &OutcomeObservation,
        sequence: u64,
    ) -> super::evidence_store::CreditAssignment {
        super::evidence_store::CreditAssignment::new(
            format!("credit.{}", sequence),
            policy("fixture.credit"),
            run.run_id(),
            run.decision_id(),
            vec![observation.observation_id()],
            vec![verified_success(CoveragePpm::ONE, "credit.contribution")],
            RecordedAt::new(LogicalSequence::new(sequence), None),
            vec![provenance("credit", SourceKind::CosmosPolicy)],
        )
        .unwrap()
    }

    pub(super) fn statistics_request() -> super::evidence_store::StatisticsRequest {
        super::evidence_store::StatisticsRequest {
            statistics_policy: policy("fixture.statistics"),
            observation_selection_policy: policy("fixture.observation-selection"),
            interval_policy: policy("fixture.integer-interval"),
            minimum_sample_count: std::num::NonZeroUsize::new(3).unwrap(),
        }
    }
}

use contracts::{LogicalSequence, RunId, SelectionDecisionId};
use evidence_store::{
    AppendReceipt, BoundedEvidenceConfig, BoundedInMemoryEvidence, CapacityReport, EvidenceError,
    EvidenceEvent, RunView, SelectionEpisodeView, StatisticsRequest, StoreStatus, TopologyEvidence,
    TopologyEvidenceProvider, TopologyStatsSnapshot, aggregate_snapshot,
};

const DECISION_ONE: &str = "77e55044-10b1-426f-8247-bb680e5fe101";
const DECISION_TWO: &str = "77e55044-10b1-426f-8247-bb680e5fe102";
const ADMISSION_ONE: &str = "77e55044-10b1-426f-8247-bb680e5fe103";
const RUN_ONE: &str = "77e55044-10b1-426f-8247-bb680e5fe105";
const OBSERVATION_ONE: &str = "77e55044-10b1-426f-8247-bb680e5fe107";
const OBSERVATION_TWO: &str = "77e55044-10b1-426f-8247-bb680e5fe108";

fn bounded_config(max_episodes: usize) -> BoundedEvidenceConfig {
    BoundedEvidenceConfig {
        max_episodes: NonZeroUsize::new(max_episodes).unwrap(),
        max_runs: NonZeroUsize::new(max_episodes.max(1)).unwrap(),
        max_observations_total: NonZeroUsize::new(8).unwrap(),
        max_observations_per_run: NonZeroUsize::new(4).unwrap(),
        max_evidence_refs_per_record: NonZeroUsize::new(8).unwrap(),
        max_open_runs: NonZeroUsize::new(max_episodes.max(1)).unwrap(),
    }
}

#[derive(Default)]
struct StandInProvider {
    events: Mutex<Vec<EvidenceEvent>>,
    dropped: Mutex<u64>,
}

impl StandInProvider {
    fn lock_events(&self) -> Result<std::sync::MutexGuard<'_, Vec<EvidenceEvent>>, EvidenceError> {
        self.events
            .lock()
            .map_err(|_| stand_in_error("store.poisoned"))
    }

    fn episodes(events: &[EvidenceEvent]) -> BTreeMap<SelectionDecisionId, SelectionEpisodeView> {
        let mut episodes = BTreeMap::new();
        for event in events {
            match event {
                EvidenceEvent::Selection(value) => {
                    episodes.insert(
                        value.decision().decision_id(),
                        SelectionEpisodeView {
                            selection: *value.clone(),
                            admissions: Vec::new(),
                            not_started: None,
                            runs: Vec::new(),
                        },
                    );
                }
                EvidenceEvent::Admission(value) => episodes
                    .get_mut(&value.decision_id())
                    .unwrap()
                    .admissions
                    .push(value.clone()),
                EvidenceEvent::SelectionNotStarted(value) => {
                    episodes.get_mut(&value.decision_id()).unwrap().not_started =
                        Some(value.clone());
                }
                EvidenceEvent::RunOpened(value) => episodes
                    .get_mut(&value.decision_id())
                    .unwrap()
                    .runs
                    .push(RunView {
                        opened: value.clone(),
                        terminal: None,
                        observations: Vec::new(),
                        credits: Vec::new(),
                    }),
                EvidenceEvent::RunFinalized(value) => {
                    find_run_mut(&mut episodes, value.run_id()).terminal =
                        Some(contracts::RunTerminal::Finalized(value.clone()));
                }
                EvidenceEvent::RunAbandoned(value) => {
                    find_run_mut(&mut episodes, value.run_id()).terminal =
                        Some(contracts::RunTerminal::Abandoned(value.clone()));
                }
                EvidenceEvent::Observation(value) => find_run_mut(&mut episodes, value.run_id())
                    .observations
                    .push(value.clone()),
                EvidenceEvent::Credit(value) => find_run_mut(&mut episodes, value.run_id())
                    .credits
                    .push(value.clone()),
            }
        }
        episodes
    }

    fn status_for(&self, events: &[EvidenceEvent]) -> StoreStatus {
        let episodes = Self::episodes(events);
        let retained_runs = episodes.values().map(|episode| episode.runs.len()).sum();
        let retained_observations = episodes
            .values()
            .flat_map(|episode| &episode.runs)
            .map(|run| run.observations.len())
            .sum();
        let observed = events
            .last()
            .map_or(LogicalSequence::new(0), EvidenceEvent::sequence);
        StoreStatus {
            observed_through: observed,
            earliest_retained_sequence: events.first().map_or(observed, EvidenceEvent::sequence),
            retained_episodes: episodes.len(),
            retained_runs,
            retained_observations,
            evicted_episodes_total: 0,
            evicted_runs_total: 0,
            dropped_append_total: *self.dropped.lock().unwrap(),
            capacity: CapacityReport {
                max_episodes: usize::MAX,
                max_runs: usize::MAX,
                max_observations_total: usize::MAX,
                max_observations_per_run: usize::MAX,
                max_evidence_refs_per_record: usize::MAX,
                max_open_runs: usize::MAX,
            },
        }
    }
}

impl TopologyEvidenceProvider for StandInProvider {
    fn append(&self, event: EvidenceEvent) -> Result<AppendReceipt, EvidenceError> {
        let mut events = self.lock_events()?;
        if let Some(existing) = events
            .iter()
            .find(|existing| stand_in_id(existing) == stand_in_id(&event))
        {
            if existing == &event {
                return Ok(stand_in_receipt(event.sequence(), false));
            }
            *self.dropped.lock().unwrap() += 1;
            return Err(stand_in_error("conflict.digest-mismatch"));
        }
        let expected = events.len() as u64 + 1;
        if event.sequence().get() != expected {
            *self.dropped.lock().unwrap() += 1;
            return Err(stand_in_error("order.sequence"));
        }
        let prefix = Self::episodes(&events);
        let valid = match &event {
            EvidenceEvent::Selection(_) => true,
            EvidenceEvent::Admission(value) => prefix.contains_key(&value.decision_id()),
            EvidenceEvent::SelectionNotStarted(value) => prefix.contains_key(&value.decision_id()),
            EvidenceEvent::RunOpened(value) => prefix.contains_key(&value.decision_id()),
            EvidenceEvent::RunFinalized(value) => prefix
                .values()
                .flat_map(|episode| &episode.runs)
                .any(|run| run.opened.run_id() == value.run_id() && run.terminal.is_none()),
            EvidenceEvent::RunAbandoned(value) => prefix
                .values()
                .flat_map(|episode| &episode.runs)
                .any(|run| run.opened.run_id() == value.run_id() && run.terminal.is_none()),
            EvidenceEvent::Observation(value) => prefix
                .values()
                .flat_map(|episode| &episode.runs)
                .any(|run| run.opened.run_id() == value.run_id() && run.terminal.is_some()),
            EvidenceEvent::Credit(value) => {
                prefix
                    .values()
                    .flat_map(|episode| &episode.runs)
                    .any(|run| {
                        run.opened.run_id() == value.run_id()
                            && value.observation_ids().iter().all(|id| {
                                run.observations
                                    .iter()
                                    .any(|observation| observation.observation_id() == *id)
                            })
                    })
            }
        };
        if !valid {
            *self.dropped.lock().unwrap() += 1;
            return Err(stand_in_error("conflict.missing-prefix"));
        }
        events.push(event);
        Ok(stand_in_receipt(LogicalSequence::new(expected), true))
    }

    fn read_episode(
        &self,
        decision_id: SelectionDecisionId,
    ) -> Result<Option<SelectionEpisodeView>, EvidenceError> {
        Ok(Self::episodes(&self.lock_events()?).remove(&decision_id))
    }

    fn read_run(&self, run_id: RunId) -> Result<Option<RunView>, EvidenceError> {
        Ok(Self::episodes(&self.lock_events()?)
            .into_values()
            .flat_map(|episode| episode.runs)
            .find(|run| run.opened.run_id() == run_id))
    }

    fn store_status(&self) -> Result<StoreStatus, EvidenceError> {
        let events = self.lock_events()?;
        Ok(self.status_for(&events))
    }

    fn capture_statistics(
        &self,
        request: &StatisticsRequest,
    ) -> Result<TopologyStatsSnapshot, EvidenceError> {
        let events = self.lock_events()?;
        let status = self.status_for(&events);
        aggregate_snapshot(Self::episodes(&events).into_values(), status, request)
    }
}

fn find_run_mut(
    episodes: &mut BTreeMap<SelectionDecisionId, SelectionEpisodeView>,
    run_id: RunId,
) -> &mut RunView {
    episodes
        .values_mut()
        .flat_map(|episode| &mut episode.runs)
        .find(|run| run.opened.run_id() == run_id)
        .unwrap()
}

fn stand_in_id(event: &EvidenceEvent) -> String {
    match event {
        EvidenceEvent::Selection(value) => {
            format!("selection:{:?}", value.decision().decision_id())
        }
        EvidenceEvent::Admission(value) => format!("admission:{:?}", value.attempt_id()),
        EvidenceEvent::SelectionNotStarted(value) => {
            format!("not-started:{:?}", value.decision_id())
        }
        EvidenceEvent::RunOpened(value) => format!("opened:{:?}", value.run_id()),
        EvidenceEvent::RunFinalized(value) => format!("finalized:{:?}", value.run_id()),
        EvidenceEvent::RunAbandoned(value) => format!("abandoned:{:?}", value.run_id()),
        EvidenceEvent::Observation(value) => {
            format!("observation:{:?}", value.observation_id())
        }
        EvidenceEvent::Credit(value) => format!("credit:{}", value.credit_id()),
    }
}

fn stand_in_error(code: &'static str) -> EvidenceError {
    EvidenceError::provider(code, "stand-in provider rejected the append")
}

fn stand_in_receipt(sequence: LogicalSequence, inserted: bool) -> AppendReceipt {
    if inserted {
        AppendReceipt::inserted(sequence)
    } else {
        AppendReceipt::idempotent(sequence)
    }
}

fn append_completed_episode(
    evidence: &TopologyEvidence,
    observation_ids: &[&str],
) -> (RunId, SelectionDecisionId) {
    let records = fixtures::completed_episode(
        "coding",
        "provider-conformance",
        DECISION_ONE,
        ADMISSION_ONE,
        RUN_ONE,
        observation_ids,
        1,
    );
    let run_id = records.opened.run_id();
    let decision_id = records.selection.decision().decision_id();
    let mut events = vec![
        EvidenceEvent::Selection(Box::new(records.selection)),
        EvidenceEvent::Admission(records.admission),
        EvidenceEvent::RunOpened(records.opened.clone()),
        EvidenceEvent::RunFinalized(records.finalized),
    ];
    events.extend(
        records
            .observations
            .iter()
            .cloned()
            .map(EvidenceEvent::Observation),
    );
    if let Some(observation) = records.observations.last() {
        events.push(EvidenceEvent::Credit(fixtures::credit(
            &records.opened,
            observation,
            5 + observation_ids.len() as u64,
        )));
    }
    for (index, event) in events.into_iter().enumerate() {
        let receipt = evidence.append(event).unwrap();
        assert!(receipt.was_inserted());
        assert_eq!(receipt.sequence().get(), index as u64 + 1);
    }
    (run_id, decision_id)
}

fn provider_conformance(provider: Arc<dyn TopologyEvidenceProvider>) {
    let evidence = TopologyEvidence::new(provider);
    let (run_id, decision_id) = append_completed_episode(&evidence, &[OBSERVATION_ONE]);

    let episode = evidence.read_episode(decision_id).unwrap().unwrap();
    assert!(episode.is_terminal());
    assert_eq!(episode.admissions.len(), 1);
    assert_eq!(episode.runs.len(), 1);
    let run = evidence.read_run(run_id).unwrap().unwrap();
    assert!(run.terminal.is_some());
    assert_eq!(run.observations.len(), 1);
    assert_eq!(run.credits.len(), 1);

    let duplicate = EvidenceEvent::Credit(run.credits[0].clone());
    let receipt = evidence.append(duplicate).unwrap();
    assert!(!receipt.was_inserted());
    assert_eq!(receipt.sequence().get(), 6);

    let snapshot = evidence
        .capture_statistics(&fixtures::statistics_request())
        .unwrap();
    assert_eq!(snapshot.window.observed_through().get(), 6);
    assert_eq!(snapshot.cohorts.len(), 3);
    let selected = snapshot
        .cohorts
        .iter()
        .find(|cohort| cohort.times_selected == 1)
        .unwrap();
    assert_eq!(selected.runs_opened, 1);
    assert_eq!(selected.runs_finalized, 1);
    assert_eq!(selected.observed_runs, 1);
    assert_eq!(selected.sample_count, 1);
    assert!(selected.below_minimum);
}

#[test]
fn one_behavioural_suite_accepts_bounded_and_independent_stand_in_providers() {
    provider_conformance(Arc::new(BoundedInMemoryEvidence::new(bounded_config(4))));
    provider_conformance(Arc::new(StandInProvider::default()));
}

#[test]
fn append_order_idempotence_and_nonblocking_loss_are_explicit() {
    let provider = Arc::new(BoundedInMemoryEvidence::new(bounded_config(2)));
    let evidence = TopologyEvidence::new(provider);
    let record = fixtures::selected_record("coding", "ordered", DECISION_ONE, 1);

    let out_of_order = fixtures::selected_record("coding", "future", DECISION_TWO, 2);
    assert!(
        evidence
            .record(EvidenceEvent::Selection(Box::new(out_of_order)))
            .is_none()
    );
    assert_eq!(evidence.store_status().unwrap().dropped_append_total, 1);

    let first = evidence
        .append(EvidenceEvent::Selection(Box::new(record.clone())))
        .unwrap();
    let retry = evidence
        .append(EvidenceEvent::Selection(Box::new(record.clone())))
        .unwrap();
    assert!(first.was_inserted());
    assert!(!retry.was_inserted());
    assert_eq!(first.sequence(), retry.sequence());

    let conflicting = fixtures::selected_record("research", "different", DECISION_ONE, 1);
    let error = evidence
        .append(EvidenceEvent::Selection(Box::new(conflicting)))
        .unwrap_err();
    assert_eq!(error.code(), "conflict.digest-mismatch");
    assert!(error.detail().contains("different content"));
    assert_eq!(evidence.store_status().unwrap().dropped_append_total, 2);
}

#[test]
fn retention_evicts_only_whole_terminal_episodes_and_never_open_runs() {
    let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(bounded_config(1))));
    let records = fixtures::completed_episode(
        "coding",
        "terminal",
        DECISION_ONE,
        ADMISSION_ONE,
        RUN_ONE,
        &[],
        1,
    );
    for event in [
        EvidenceEvent::Selection(Box::new(records.selection)),
        EvidenceEvent::Admission(records.admission),
        EvidenceEvent::RunOpened(records.opened),
        EvidenceEvent::RunFinalized(records.finalized),
    ] {
        evidence.append(event).unwrap();
    }
    evidence
        .append(EvidenceEvent::Selection(Box::new(
            fixtures::selected_record("research", "replacement", DECISION_TWO, 5),
        )))
        .unwrap();
    let status = evidence.store_status().unwrap();
    assert_eq!(status.evicted_episodes_total, 1);
    assert_eq!(status.evicted_runs_total, 1);
    assert_eq!(status.earliest_retained_sequence.get(), 5);

    let open_evidence =
        TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(bounded_config(1))));
    let open = fixtures::completed_episode(
        "coding",
        "open",
        DECISION_ONE,
        ADMISSION_ONE,
        RUN_ONE,
        &[],
        1,
    );
    for event in [
        EvidenceEvent::Selection(Box::new(open.selection)),
        EvidenceEvent::Admission(open.admission),
        EvidenceEvent::RunOpened(open.opened.clone()),
    ] {
        open_evidence.append(event).unwrap();
    }
    let error = open_evidence
        .append(EvidenceEvent::Selection(Box::new(
            fixtures::selected_record("research", "blocked-by-open", DECISION_TWO, 4),
        )))
        .unwrap_err();
    assert_eq!(error.code(), "capacity.exhausted");
    open_evidence
        .append(EvidenceEvent::RunAbandoned(fixtures::abandoned(
            &open.opened,
            4,
        )))
        .unwrap();
    open_evidence
        .append(EvidenceEvent::Selection(Box::new(
            fixtures::selected_record("research", "after-abandonment", DECISION_TWO, 5),
        )))
        .unwrap();
    assert_eq!(
        open_evidence.store_status().unwrap().evicted_episodes_total,
        1
    );
}

#[test]
fn explicit_no_start_is_terminal_and_preserves_failed_admission_order() {
    let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(bounded_config(2))));
    let (selection, admission, not_started) =
        fixtures::no_start_episode(DECISION_ONE, ADMISSION_ONE);
    let decision_id = selection.decision().decision_id();
    evidence
        .append(EvidenceEvent::Selection(Box::new(selection)))
        .unwrap();
    evidence
        .append(EvidenceEvent::Admission(admission))
        .unwrap();
    evidence
        .append(EvidenceEvent::SelectionNotStarted(not_started))
        .unwrap();

    let episode = evidence.read_episode(decision_id).unwrap().unwrap();
    assert!(episode.is_terminal());
    assert!(episode.not_started.is_some());
    assert!(episode.runs.is_empty());
}

#[test]
fn delayed_conflicting_observations_are_retained_and_aggregated_deterministically() {
    let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(bounded_config(2))));
    append_completed_episode(&evidence, &[OBSERVATION_ONE, OBSERVATION_TWO]);
    let request = fixtures::statistics_request();
    let first = evidence.capture_statistics(&request).unwrap();
    let second = evidence.capture_statistics(&request).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.content_digest(), second.content_digest());

    let selected = first
        .cohorts
        .iter()
        .find(|cohort| cohort.times_selected == 1)
        .unwrap();
    assert_eq!(selected.conflicting_runs, 1);
    assert_eq!(selected.observation_statuses.len(), 2);
    assert_eq!(selected.sample_count, 2);
    let verified = selected
        .measurements
        .iter()
        .find(|measurement| measurement.measurement_id.as_str() == "verified-success")
        .unwrap();
    assert_eq!(verified.sample_count, 2);
    assert_eq!(verified.mean, 500_000);
    assert!(verified.interval_low_ppm.is_some());
    assert!(verified.interval_high_ppm.is_some());
}

#[test]
fn observation_capacity_preserves_prior_judgements_and_reports_loss() {
    let mut config = bounded_config(2);
    config.max_observations_per_run = NonZeroUsize::new(1).unwrap();
    let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(config)));
    let records = fixtures::completed_episode(
        "research",
        "bounded-observations",
        DECISION_ONE,
        ADMISSION_ONE,
        RUN_ONE,
        &[OBSERVATION_ONE, OBSERVATION_TWO],
        1,
    );
    let run_id = records.opened.run_id();
    for event in [
        EvidenceEvent::Selection(Box::new(records.selection)),
        EvidenceEvent::Admission(records.admission),
        EvidenceEvent::RunOpened(records.opened),
        EvidenceEvent::RunFinalized(records.finalized),
        EvidenceEvent::Observation(records.observations[0].clone()),
    ] {
        evidence.append(event).unwrap();
    }
    let error = evidence
        .append(EvidenceEvent::Observation(records.observations[1].clone()))
        .unwrap_err();
    assert_eq!(error.code(), "capacity.run-observations-exhausted");
    let run = evidence.read_run(run_id).unwrap().unwrap();
    assert_eq!(run.observations, vec![records.observations[0].clone()]);
    assert_eq!(evidence.store_status().unwrap().dropped_append_total, 1);
}

#[test]
fn evidence_contract_version_is_explicit() {
    assert_eq!(evidence_store::TOPOLOGY_EVIDENCE_VERSION, 1);
}
