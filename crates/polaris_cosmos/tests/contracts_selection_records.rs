//! Full lifecycle and cross-domain fixtures for selection and evidence records.

use std::sync::Arc;

use polaris::system::plugin::Version;
use polaris_cosmos::contracts::{
    AdmissionEvaluation, AdmissionResult, AvailabilityObservation, AvailabilitySnapshotId,
    AvailabilityState, BindingAlias, BindingAvailability, BindingKindId, CandidateConsideration,
    CandidateScore, CanonicalRecord, CapabilityAvailability, CapabilityId, CapabilityKey,
    CardinalityEstimate, ConfidencePpm, ConfigurationEntry, ConfigurationSchemaId,
    ConfigurationSchemaKey, ConfigurationValue, ConsequenceLevel, ContentDigest, ContextTypeId,
    ContextTypeKey, ControlFlowChoice, CoveragePpm, DecompositionShape, DurationMs, EmbodimentId,
    EmbodimentKey, EmptyStatisticsSnapshot, EnvironmentId, EnvironmentKey, EpistemicBasis,
    ErrorClassId, EvidenceKind, EvidenceObjectId, EvidenceRef, EvidenceStoreId, EvidenceTime,
    ExclusionCode, ExclusionPhase, ExclusionReason, ExclusionSummary, ExecutionDisposition,
    ExecutionFault, ExecutionProvenance, ExecutionStage, ExternalSessionRef, FallbackCandidate,
    FeatureAssessment, HardConstraints, HostRuntimeId, HostRuntimeKey, LogicalSequence,
    Measurement, MeasurementEntry, MeasurementId, MeasurementValue, MechanismAvailability,
    MissingPreferencePolicy, ModelId, ModelProvenance, NeedLevel, Objective, ObservedLimit,
    OutcomeObservation, OutputAvailability, PolicyEligibility, PolicyId, PolicyLabelId, PolicyRef,
    PreferenceBasisPoints, PreferenceCriterion, PreferenceWeights, ProbabilityPpm, Provenance,
    ProviderAvailability, ProviderId, ProviderKey, RecordSubjectId, RecordedAt,
    RegistrationSelectionRef, ResidualDisposition, ResolvedConfigurationRef, ResourceAccess,
    ResourceAvailability, RiskLevel, RoleScheduleCount, RoleScheduleId, RunAbandoned,
    RunCompletion, RunControlFlow, RunFaults, RunFinalized, RunOpened, RunStructure, RunTerminal,
    RunUsage, RuntimeAvailabilitySnapshot, RuntimeMechanismId, RuntimeMechanismKey,
    SamplingProvenance, SchemaVersion, ScoreComponent, ScoreComponentId, ScoreSource,
    SelectionDecision, SelectionDecisionId, SelectionInputRefs, SelectionNotStarted,
    SelectionOutcome, SelectionRationale, SelectionRecord, SelectorId, SelectorKey, SelectorRef,
    SourceId, SourceKey, SourceKind, StatisticsWindow, Status, StopReason, SupersedeReason,
    SupersedesClaim, TaskFeatures, TaskHorizon, TaskId, TaskProfile, TieBreakPolicy,
    TopologyRunRecord, UnknownEstimatePolicy, UnknownReason, UtilityPpm, VerifierAvailability,
    VerifierCapability, VerifierProfile,
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

fn availability_snapshot(snapshot_id: &str, captured_at_ms: u64) -> RuntimeAvailabilitySnapshot {
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

#[test]
fn complete_selection_precedes_admission_and_run_and_round_trips_canonically() {
    let fixture = started_fixture("coding", "fix-independent-fixtures", DECISION_A);
    let record = SelectionRecord::new(
        RecordedAt::new(LogicalSequence::new(1), Some(1_000)),
        fixture.decision.clone(),
        fixture.profile.clone(),
        fixture.objective.clone(),
    )
    .unwrap();
    assert_eq!(
        SelectionRecord::from_canonical_cbor(&record.to_canonical_cbor().unwrap()).unwrap(),
        record
    );
    assert_eq!(
        AdmissionEvaluation::from_canonical_cbor(&fixture.admission.to_canonical_cbor().unwrap())
            .unwrap(),
        fixture.admission
    );
    assert_eq!(
        RunOpened::from_canonical_cbor(&fixture.opened.to_canonical_cbor().unwrap()).unwrap(),
        fixture.opened
    );

    let structure = RunStructure::new(
        4,
        4,
        Some(0),
        1,
        3,
        vec![RoleScheduleCount::new(
            RoleScheduleId::new("fixture.verify").unwrap(),
            1,
        )],
    )
    .unwrap();
    let control_flow = RunControlFlow::new(
        vec![ControlFlowChoice::new("gate", "accept").unwrap()],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let faults = RunFaults::new(
        vec![
            ExecutionFault::new(
                ErrorClassId::new("fixture.recovered-warning").unwrap(),
                ExecutionStage::Execution,
                Some("verify".into()),
            )
            .unwrap(),
        ],
        1,
        0,
        None,
    );
    let finalized = RunFinalized::new(
        fixture.opened.run_id(),
        RecordedAt::new(LogicalSequence::new(4), Some(4_000)),
        DurationMs::new(1_000),
        structure,
        control_flow,
        faults,
        RunUsage::empty(),
        RunCompletion::new(
            ExecutionDisposition::Completed,
            Some(StopReason::GoalMet),
            Some(CoveragePpm::ONE),
            Some(0),
        )
        .unwrap(),
        vec![evidence("run.summary")],
    )
    .unwrap();
    let run = TopologyRunRecord::new(fixture.opened, RunTerminal::Finalized(finalized)).unwrap();
    assert_eq!(run.disposition(), ExecutionDisposition::Completed);
    assert_eq!(
        TopologyRunRecord::from_canonical_cbor(&run.to_canonical_cbor().unwrap()).unwrap(),
        run
    );
}

#[test]
fn availability_snapshot_is_secret_free_extensible_and_content_addressed() {
    let first = availability_snapshot(AVAILABILITY, 1_000);
    let second = availability_snapshot("67e55044-10b1-426f-8247-bb680e5fe0c8", 86_401_000);

    assert_ne!(first.snapshot_id(), second.snapshot_id());
    assert_ne!(first.captured_at(), second.captured_at());
    assert_eq!(
        first.content_digest(),
        second.content_digest(),
        "event identity and observation time do not change fact content identity"
    );
    assert_eq!(
        first.capabilities()[0].capability().id().as_str(),
        "future.streaming-proof"
    );
    assert_eq!(
        first.capabilities()[0].observation().state(),
        AvailabilityState::Unknown
    );
    assert_eq!(
        RuntimeAvailabilitySnapshot::from_canonical_cbor(&first.to_canonical_cbor().unwrap())
            .unwrap(),
        first
    );
}

#[test]
fn failed_live_admission_has_an_explicit_no_start_and_cannot_open_a_run() {
    let fixture = started_fixture("coding", "cannot-start", DECISION_A);
    let reason = ExclusionReason::new(
        ExclusionPhase::LiveReadmission,
        ExclusionCode::new("future.provider-drained").unwrap(),
        vec![RecordSubjectId::new("fixture.provider").unwrap()],
        vec![fixture.decision.input_refs().availability_snapshot_digest()],
    );
    let failed = AdmissionEvaluation::new(
        ADMISSION.parse().unwrap(),
        fixture.decision.decision_id(),
        fixture.admission.candidate().clone(),
        fixture.decision.input_refs().catalog_snapshot_digest(),
        fixture.decision.input_refs().availability_snapshot_id(),
        fixture.decision.input_refs().availability_snapshot_digest(),
        AdmissionResult::excluded(vec![reason.clone()]).unwrap(),
        RecordedAt::new(LogicalSequence::new(2), None),
    )
    .unwrap();
    assert_eq!(
        RunOpened::new(
            RUN.parse().unwrap(),
            RecordedAt::new(LogicalSequence::new(3), None),
            &fixture.decision,
            &failed,
            vec![topology_contract()],
            0,
            execution_provenance(),
        )
        .unwrap_err()
        .code(),
        "run.missing-successful-admission"
    );
    let no_start = SelectionNotStarted::new(
        fixture.decision.decision_id(),
        RecordedAt::new(LogicalSequence::new(3), None),
        vec![failed.attempt_id()],
        vec![reason],
    )
    .unwrap();
    assert_eq!(
        SelectionNotStarted::from_canonical_cbor(&no_start.to_canonical_cbor().unwrap()).unwrap(),
        no_start
    );
}

#[test]
fn abandonment_requires_an_explicit_terminal_event() {
    let fixture = started_fixture("research", "recover-open-run", DECISION_A);
    assert_eq!(
        RunCompletion::new(ExecutionDisposition::Abandoned, None, None, None)
            .unwrap_err()
            .code(),
        "run.abandonment-requires-event"
    );
    let abandoned = RunAbandoned::new(
        fixture.opened.run_id(),
        RecordedAt::new(LogicalSequence::new(9), Some(90_000)),
        policy("fixture.recovery"),
        vec![provenance("run.recovery", SourceKind::HostObservation)],
        vec![evidence("run.recovery-log")],
    )
    .unwrap();
    let record = TopologyRunRecord::new(fixture.opened, RunTerminal::Abandoned(abandoned)).unwrap();
    assert_eq!(record.disposition(), ExecutionDisposition::Abandoned);
    assert_eq!(
        TopologyRunRecord::from_canonical_cbor(&record.to_canonical_cbor().unwrap()).unwrap(),
        record
    );
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

#[test]
fn delayed_and_conflicting_observations_remain_append_only() {
    let fixture = started_fixture("research", "compare-primary-sources", DECISION_A);
    let first = OutcomeObservation::new(
        OBSERVATION_A.parse().unwrap(),
        fixture.opened.run_id(),
        fixture.profile.task_id().clone(),
        RecordedAt::new(LogicalSequence::new(5), Some(5_000)),
        Some(EvidenceTime::new(None, Some(4_500)).unwrap()),
        Some(DurationMs::new(500)),
        Status::Pass,
        Some(ResidualDisposition::Resolved),
        vec![verified_success(CoveragePpm::ONE, "outcome.first")],
        None,
        Vec::new(),
        None,
        source(),
        SourceKind::AutomatedVerifier,
        ConfidencePpm::new(950_000).unwrap(),
        vec![provenance("outcome.first", SourceKind::AutomatedVerifier)],
        vec![evidence("outcome.first")],
    )
    .unwrap();
    let later = OutcomeObservation::new(
        OBSERVATION_B.parse().unwrap(),
        fixture.opened.run_id(),
        fixture.profile.task_id().clone(),
        RecordedAt::new(LogicalSequence::new(6), Some(86_405_000)),
        Some(EvidenceTime::new(None, Some(86_405_000)).unwrap()),
        Some(DurationMs::new(86_400_000)),
        Status::Issue,
        Some(ResidualDisposition::Deferred),
        vec![
            verified_success(CoveragePpm::ZERO, "outcome.later"),
            MeasurementEntry::new(
                MeasurementId::new("future.reproducibility-score").unwrap(),
                Measurement::new(
                    MeasurementValue::Count(7),
                    None,
                    ConfidencePpm::new(700_000).unwrap(),
                    vec![provenance(
                        "outcome.future-measurement",
                        SourceKind::DownstreamSignal,
                    )],
                )
                .unwrap(),
            )
            .unwrap(),
        ],
        None,
        Vec::new(),
        Some(SupersedesClaim::new(
            first.observation_id(),
            SupersedeReason::SupersededByLaterEvidence,
        )),
        source(),
        SourceKind::DownstreamSignal,
        ConfidencePpm::new(900_000).unwrap(),
        vec![provenance("outcome.later", SourceKind::DownstreamSignal)],
        vec![evidence("outcome.later")],
    )
    .unwrap();

    assert_eq!(first.status(), Status::Pass);
    assert_eq!(later.status(), Status::Issue);
    assert_ne!(first.observation_id(), later.observation_id());
    assert!(
        later
            .measurements()
            .iter()
            .any(|entry| entry.id().as_str() == "future.reproducibility-score"),
        "unknown measurement registry entries are retained verbatim"
    );
    assert_eq!(
        OutcomeObservation::from_canonical_cbor(&first.to_canonical_cbor().unwrap()).unwrap(),
        first
    );
    assert_eq!(
        OutcomeObservation::from_canonical_cbor(&later.to_canonical_cbor().unwrap()).unwrap(),
        later
    );
}

#[test]
fn coding_and_research_episodes_share_one_domain_neutral_record_family() {
    let coding_profile = profile(
        "coding",
        "repair-independent-tests",
        DecompositionShape::IndependentParts,
    );
    let research_profile = profile(
        "research",
        "synthesize-independent-sources",
        DecompositionShape::IndependentParts,
    );
    let objective = objective();
    let coding = selected_decision(DECISION_A, &coding_profile, &objective);
    let research = selected_decision(DECISION_B, &research_profile, &objective);

    assert_eq!(coding.candidates().len(), 3);
    assert_eq!(research.candidates().len(), 3);
    assert_ne!(coding.task_id(), research.task_id());
    assert_ne!(
        coding.fingerprint(),
        research.fingerprint(),
        "host task identity is part of the otherwise shared domain-neutral shape"
    );
    assert_eq!(SchemaVersion::V1.major(), 1);
    SelectionDecision::from_canonical_cbor(&coding.to_canonical_cbor().unwrap()).unwrap();
    SelectionDecision::from_canonical_cbor(&research.to_canonical_cbor().unwrap()).unwrap();
}

#[test]
fn decision_fingerprint_omits_event_identity_and_no_eligible_is_typed() {
    let profile = profile("coding", "stable-decision", DecompositionShape::Sequential);
    let objective = objective();
    let first = selected_decision(DECISION_A, &profile, &objective);
    let second = selected_decision(DECISION_B, &profile, &objective);
    assert_ne!(first.decision_id(), second.decision_id());
    assert_eq!(first.fingerprint(), second.fingerprint());

    let diagnostics = vec![
        ExclusionSummary::new(
            ExclusionPhase::Mechanical,
            ExclusionCode::new("catalog.empty").unwrap(),
            0,
        )
        .unwrap(),
    ];
    let no_eligible = SelectionDecision::new(
        DECISION_A.parse().unwrap(),
        profile.task_id().clone(),
        profile.external_session().cloned(),
        first.selector().clone(),
        topology_contract(),
        first.input_refs().clone(),
        Vec::new(),
        SelectionOutcome::NoEligibleCandidate { diagnostics },
    )
    .unwrap();
    assert!(matches!(
        no_eligible.outcome(),
        SelectionOutcome::NoEligibleCandidate { .. }
    ));
    SelectionDecision::from_canonical_cbor(&no_eligible.to_canonical_cbor().unwrap()).unwrap();

    let mechanical = CandidateConsideration::mechanically_excluded(
        selected_registration("fixture.no-eligible-a"),
        AdmissionResult::excluded(vec![ExclusionReason::new(
            ExclusionPhase::Mechanical,
            ExclusionCode::new("future.runtime-shape").unwrap(),
            Vec::new(),
            vec![ContentDigest::sha256(b"catalog")],
        )])
        .unwrap(),
    )
    .unwrap();
    let policy_excluded = CandidateConsideration::policy_excluded(
        selected_registration("fixture.no-eligible-b"),
        PolicyEligibility::excluded(vec![ExclusionReason::new(
            ExclusionPhase::Policy,
            ExclusionCode::new("budget.tokens").unwrap(),
            Vec::new(),
            Vec::new(),
        )])
        .unwrap(),
    )
    .unwrap();
    let complete_diagnostics = vec![
        ExclusionSummary::new(
            ExclusionPhase::Mechanical,
            ExclusionCode::new("future.runtime-shape").unwrap(),
            1,
        )
        .unwrap(),
        ExclusionSummary::new(
            ExclusionPhase::Policy,
            ExclusionCode::new("budget.tokens").unwrap(),
            1,
        )
        .unwrap(),
    ];
    let complete = SelectionDecision::new(
        DECISION_B.parse().unwrap(),
        profile.task_id().clone(),
        profile.external_session().cloned(),
        first.selector().clone(),
        topology_contract(),
        first.input_refs().clone(),
        vec![policy_excluded.clone(), mechanical.clone()],
        SelectionOutcome::NoEligibleCandidate {
            diagnostics: complete_diagnostics,
        },
    )
    .unwrap();
    SelectionDecision::from_canonical_cbor(&complete.to_canonical_cbor().unwrap()).unwrap();

    let incomplete = SelectionDecision::new(
        DECISION_B.parse().unwrap(),
        profile.task_id().clone(),
        profile.external_session().cloned(),
        first.selector().clone(),
        topology_contract(),
        first.input_refs().clone(),
        vec![mechanical, policy_excluded],
        SelectionOutcome::NoEligibleCandidate {
            diagnostics: vec![
                ExclusionSummary::new(
                    ExclusionPhase::Policy,
                    ExclusionCode::new("budget.tokens").unwrap(),
                    1,
                )
                .unwrap(),
            ],
        },
    )
    .unwrap_err();
    assert_eq!(incomplete.code(), "selection.incomplete-diagnostics");
}

#[test]
fn exploratory_propensities_require_reproducible_sampling_provenance() {
    let profile = profile("research", "explore-candidates", DecompositionShape::Mixed);
    let objective = objective();
    let stats = empty_stats();
    let primary = selected_registration("fixture.exploration-a");
    let fallback = selected_registration("fixture.exploration-b");
    let candidates = vec![
        CandidateConsideration::eligible(
            primary.clone(),
            candidate_score("fixture.exploration-score-a", objective.preferences()),
            1,
            ProbabilityPpm::new(600_000).unwrap(),
        )
        .unwrap(),
        CandidateConsideration::eligible(
            fallback.clone(),
            candidate_score("fixture.exploration-score-b", objective.preferences()),
            2,
            ProbabilityPpm::new(400_000).unwrap(),
        )
        .unwrap(),
    ];
    let selector = SelectorRef::new(
        SelectorKey::new(SelectorId::new("fixture.selector").unwrap(), version()),
        ConfigurationDigest::new(Arc::<str>::from(
            ContentDigest::sha256(b"selector-config").to_string(),
        ))
        .unwrap(),
    )
    .unwrap();
    let inputs = SelectionInputRefs::new(
        profile.content_digest(),
        objective.content_digest(),
        ContentDigest::sha256(b"catalog"),
        AVAILABILITY.parse().unwrap(),
        ContentDigest::sha256(b"availability"),
        stats.content_digest(),
    );
    let rationale = || {
        SelectionRationale::new(
            vec![ScoreComponentId::new("fixture.exploration-score-a").unwrap()],
            false,
            false,
            None,
            policy("fixture.confidence"),
        )
        .unwrap()
    };
    let missing = SelectionDecision::new(
        DECISION_A.parse().unwrap(),
        profile.task_id().clone(),
        profile.external_session().cloned(),
        selector.clone(),
        topology_contract(),
        inputs.clone(),
        candidates.clone(),
        SelectionOutcome::Selected {
            selected: primary.clone(),
            fallback_plan: vec![FallbackCandidate::new(fallback.clone()).unwrap()],
            rationale: rationale(),
            confidence: ConfidencePpm::new(800_000).unwrap(),
            selected_propensity: ProbabilityPpm::new(600_000).unwrap(),
            sampling: None,
        },
    )
    .unwrap_err();
    assert_eq!(missing.code(), "selection.missing-sampling-provenance");

    let sampling = SamplingProvenance::new(
        policy("fixture.sampling"),
        evidence("selection.randomness"),
        vec![provenance("selection.sampling", SourceKind::CosmosPolicy)],
    )
    .unwrap();
    let decision = SelectionDecision::new(
        DECISION_A.parse().unwrap(),
        profile.task_id().clone(),
        profile.external_session().cloned(),
        selector,
        topology_contract(),
        inputs,
        candidates,
        SelectionOutcome::Selected {
            selected: primary,
            fallback_plan: vec![FallbackCandidate::new(fallback).unwrap()],
            rationale: rationale(),
            confidence: ConfidencePpm::new(800_000).unwrap(),
            selected_propensity: ProbabilityPpm::new(600_000).unwrap(),
            sampling: Some(sampling),
        },
    )
    .unwrap();
    assert_eq!(
        SelectionDecision::from_canonical_cbor(&decision.to_canonical_cbor().unwrap()).unwrap(),
        decision
    );
}
