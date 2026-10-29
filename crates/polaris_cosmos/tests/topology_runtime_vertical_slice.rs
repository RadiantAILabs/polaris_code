//! End-to-end test of host-neutral topology selection, execution and evidence.

use std::sync::Arc;

use polaris::graph::{GraphExecutor, GraphSignature};
use polaris::system::param::SystemContext;

use polaris_cosmos::contracts::{
    AssessmentConclusion, CanonicalRecord, CoveragePpm, ExecutionDisposition, LogicalSequence,
    RecordedAt, RunCompletion, RunControlFlow, RunFaults, RunFinalized, RunOpened, RunStructure,
    RunUsage, SelectionOutcome, SelectionRecord, Status, StopReason, VerifierCapability,
};
use polaris_cosmos::evidence::{BoundedInMemoryEvidence, EvidenceEvent, TopologyEvidence};
use polaris_cosmos::selection::deterministic;
use polaris_cosmos::topology::RegistrationId;

#[path = "fixtures/topology_runtime.rs"]
mod fixtures;

fn selected_registration(
    decision: &polaris_cosmos::contracts::SelectionDecision,
) -> &RegistrationId {
    match decision.outcome() {
        SelectionOutcome::Selected { selected, .. } => selected.registration_id(),
        SelectionOutcome::NoEligibleCandidate { .. } => panic!("expected a selected candidate"),
    }
}

fn evaluation<'a>(
    admission: &'a polaris_cosmos::topology::CandidateAdmissionSet,
    registration: &str,
) -> &'a polaris_cosmos::topology::CandidateMechanicalAdmission {
    admission
        .evaluations()
        .iter()
        .find(|evaluation| evaluation.candidate().registration_id().as_str() == registration)
        .unwrap()
}

fn mechanical_codes(
    admission: &polaris_cosmos::topology::CandidateAdmissionSet,
    registration: &str,
) -> Vec<String> {
    evaluation(admission, registration)
        .result()
        .reasons()
        .iter()
        .map(|reason| reason.code().as_str().to_owned())
        .collect()
}

/// Every registration whose native boundaries lack a stable explanation.
///
/// A Cosmos-shipped cognition point must explain its own private state, so any
/// name in this list is a defect in the shipped point rather than a fixture
/// gap: an unexplained boundary makes the point permanently unselectable.
fn unexplained_registrations(
    admission: &polaris_cosmos::topology::CandidateAdmissionSet,
) -> Vec<String> {
    admission
        .evaluations()
        .iter()
        .filter(|evaluation| {
            evaluation
                .result()
                .reasons()
                .iter()
                .any(|reason| reason.code().as_str() == "type-explanation.missing")
        })
        .map(|evaluation| evaluation.candidate().registration_id().as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn portable_profile_selects_executes_and_learns_from_ordered_evidence() {
    let fixture = fixtures::fixture();
    assert_eq!(fixture.snapshot.candidates().len(), 5);
    assert_eq!(fixture.catalog.len(), 5);
    assert_eq!(fixture.selections.len(), 5);

    let profile = fixtures::profile();
    assert!(fixtures::profile_has_only_explicit_unknowns(&profile));
    assert_eq!(fixtures::profile_shape(&profile), None);

    let admission = fixture.preflight();
    assert_eq!(
        unexplained_registrations(&admission),
        Vec::<String>::new(),
        "a registered topology left a native boundary unexplained"
    );
    for registration in [
        fixtures::DIRECT_REGISTRATION,
        fixtures::PREDICTION_REGISTRATION,
        fixtures::COVERAGE_REGISTRATION,
        fixtures::CORRECTION_REGISTRATION,
    ] {
        assert!(
            evaluation(&admission, registration).result().is_admitted(),
            "{registration} must survive mechanical admission"
        );
    }
    assert!(
        mechanical_codes(&admission, fixtures::UNAVAILABLE_REGISTRATION)
            .iter()
            .any(|code| code == "resource.missing")
    );

    let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(
        fixtures::evidence_config(),
    )));
    let statistics_request = fixtures::statistics_request();
    let initial_statistics = evidence.capture_statistics(&statistics_request).unwrap();
    assert_eq!(initial_statistics.window, fixtures::empty_window());

    let quality_objective = fixtures::objective(10_000, 0, None);
    let quality_inputs = fixtures::selection_inputs(
        &fixture,
        fixtures::DECISION_QUALITY,
        profile.clone(),
        quality_objective.clone(),
        admission.clone(),
        initial_statistics.clone(),
        fixtures::policy_facts(&fixture.selections, None),
    );
    let quality_decision = deterministic::select(&quality_inputs).unwrap();
    let stable_replay = deterministic::select(&quality_inputs).unwrap();
    assert_eq!(quality_decision, stable_replay);
    assert_eq!(
        selected_registration(&quality_decision).as_str(),
        fixtures::COVERAGE_REGISTRATION
    );
    assert_eq!(quality_decision.candidates().len(), 5);
    assert_eq!(
        quality_decision
            .candidates()
            .iter()
            .filter(|candidate| candidate.rank().is_some())
            .count(),
        4,
        "every admitted candidate must be ranked, not merely admitted"
    );

    let unavailable_consideration = quality_decision
        .candidates()
        .iter()
        .find(|candidate| {
            candidate.candidate().registration_id().as_str() == fixtures::UNAVAILABLE_REGISTRATION
        })
        .unwrap();
    assert!(
        !unavailable_consideration
            .mechanical_admission()
            .is_admitted()
    );
    assert!(unavailable_consideration.score().is_none());
    assert_eq!(unavailable_consideration.rank(), None);

    let latency_decision = deterministic::select(&fixtures::selection_inputs(
        &fixture,
        fixtures::DECISION_QUALITY,
        profile.clone(),
        fixtures::objective(0, 10_000, None),
        admission.clone(),
        initial_statistics.clone(),
        fixtures::policy_facts(&fixture.selections, None),
    ))
    .unwrap();
    assert_eq!(
        selected_registration(&latency_decision).as_str(),
        fixtures::PREDICTION_REGISTRATION
    );
    assert_eq!(fixture.catalog.generation(), fixture.snapshot.generation());

    let no_eligible = deterministic::select(&fixtures::selection_inputs(
        &fixture,
        fixtures::DECISION_NO_ELIGIBLE,
        profile.clone(),
        fixtures::objective(10_000, 0, Some(VerifierCapability::Comparative)),
        admission.clone(),
        initial_statistics,
        fixtures::policy_facts(&fixture.selections, None),
    ))
    .unwrap();
    assert!(matches!(
        no_eligible.outcome(),
        SelectionOutcome::NoEligibleCandidate { diagnostics } if !diagnostics.is_empty()
    ));

    let selection_record = SelectionRecord::new(
        RecordedAt::new(LogicalSequence::new(1), None),
        quality_decision.clone(),
        profile.clone(),
        quality_objective.clone(),
    )
    .unwrap();
    selection_record.validate().unwrap();
    evidence
        .append(EvidenceEvent::Selection(Box::new(selection_record)))
        .unwrap();

    let selected_id = selected_registration(&quality_decision).clone();
    let selected_reference = match quality_decision.outcome() {
        SelectionOutcome::Selected { selected, .. } => selected.clone(),
        SelectionOutcome::NoEligibleCandidate { .. } => unreachable!(),
    };
    let executor = GraphExecutor::new();
    let mut context = SystemContext::new();
    fixtures::prepare_coverage_context(&mut context);
    let live = polaris_cosmos::topology::readmit_live(
        &fixture.catalog,
        fixture.snapshot.generation(),
        fixture.catalog_digest,
        fixture.catalog_digest,
        fixture.candidate(&selected_id),
        &fixture.contract,
        &fixture.availability,
        &fixture.schemas,
        &fixture.type_registry,
        &executor,
        &context,
        quality_decision.decision_id(),
        fixtures::ADMISSION.parse().unwrap(),
        RecordedAt::new(LogicalSequence::new(2), None),
    )
    .unwrap();
    assert!(live.is_permitted());
    let (admission_event, permit) = live.into_parts();
    evidence
        .append(EvidenceEvent::Admission(admission_event.clone()))
        .unwrap();
    let permitted = permit
        .unwrap()
        .consume(
            quality_decision.decision_id(),
            &selected_reference,
            &context,
        )
        .unwrap();
    assert_eq!(permitted, selected_reference);

    let registered = fixture
        .catalog
        .resolve_at(&selected_id, fixture.snapshot.generation())
        .unwrap();
    assert!(
        registered
            .signature()
            .satisfies(&GraphSignature::new().produce::<AssessmentConclusion>())
    );
    let opened = RunOpened::new(
        fixtures::RUN.parse().unwrap(),
        RecordedAt::new(LogicalSequence::new(3), None),
        &quality_decision,
        &admission_event,
        vec![fixture.contract.key().clone()],
        0,
        fixtures::execution_provenance(),
    )
    .unwrap();
    evidence
        .append(EvidenceEvent::RunOpened(opened.clone()))
        .unwrap();

    let execution = executor
        .execute(registered.graph(), &mut context, None, None)
        .await
        .unwrap();
    let conclusion = execution.output::<AssessmentConclusion>().unwrap();
    assert_eq!(conclusion.stop_reason(), StopReason::WorkExhausted);
    assert_eq!(conclusion.accounting().len(), 1);

    let stop_reason = conclusion.stop_reason();
    let node_count = u64::try_from(execution.nodes_executed()).unwrap();
    let finalized = RunFinalized::new(
        opened.run_id(),
        RecordedAt::new(LogicalSequence::new(4), None),
        polaris_cosmos::contracts::DurationMs::new(12),
        RunStructure::new(node_count, node_count, Some(0), 1, node_count, Vec::new()).unwrap(),
        RunControlFlow::empty(),
        RunFaults::empty(),
        RunUsage::empty(),
        RunCompletion::new(
            ExecutionDisposition::Completed,
            Some(stop_reason),
            Some(CoveragePpm::ONE),
            Some(0),
        )
        .unwrap(),
        Vec::new(),
    )
    .unwrap();
    evidence
        .append(EvidenceEvent::RunFinalized(finalized.clone()))
        .unwrap();

    let pass = fixtures::observation(
        fixtures::OBSERVATION_PASS,
        opened.run_id(),
        profile.task_id().clone(),
        5,
        Status::Pass,
        None,
    );
    let issue = fixtures::observation(
        fixtures::OBSERVATION_ISSUE,
        opened.run_id(),
        profile.task_id().clone(),
        6,
        Status::Issue,
        Some(fixtures::OBSERVATION_PASS),
    );
    evidence
        .append(EvidenceEvent::Observation(pass.clone()))
        .unwrap();
    evidence
        .append(EvidenceEvent::Observation(issue.clone()))
        .unwrap();

    let episode = evidence
        .read_episode(quality_decision.decision_id())
        .unwrap()
        .unwrap();
    assert_eq!(episode.selection.decision(), &quality_decision);
    assert_eq!(episode.admissions, vec![admission_event]);
    assert_eq!(episode.runs.len(), 1);
    assert_eq!(episode.runs[0].observations, vec![pass, issue]);
    assert_eq!(
        episode.runs[0].terminal.as_ref().unwrap().recorded_at(),
        finalized.recorded_at()
    );
    assert_eq!(
        finalized.completion().disposition(),
        ExecutionDisposition::Completed
    );

    let statistics = evidence.capture_statistics(&statistics_request).unwrap();
    assert_eq!(statistics.cohorts.len(), 5);
    let selected_cohort = statistics
        .cohorts
        .iter()
        .find(|cohort| cohort.key.registration == selected_id)
        .unwrap();
    assert_eq!(
        selected_cohort.key.topology,
        selected_reference.topology_key().clone()
    );
    assert_eq!(
        selected_cohort.key.configuration,
        selected_reference.configuration().digest().clone()
    );
    assert_eq!(
        selected_cohort.key.contract_key,
        fixture.contract.key().clone()
    );
    let execution_bucket = selected_cohort.key.execution.as_ref().unwrap();
    assert_eq!(
        execution_bucket.embodiment,
        opened.provenance().embodiment().clone()
    );
    assert_eq!(execution_bucket.providers, opened.provenance().providers());
    assert_eq!(execution_bucket.models.len(), 1);
    assert_eq!(
        execution_bucket.models[0].provider,
        opened.provenance().models()[0].provider().clone()
    );
    assert_eq!(
        execution_bucket.models[0].model,
        opened.provenance().models()[0].model().clone()
    );
    assert_eq!(selected_cohort.times_considered, 1);
    assert_eq!(selected_cohort.times_selected, 1);
    assert_eq!(selected_cohort.runs_opened, 1);
    assert_eq!(selected_cohort.runs_finalized, 1);
    assert_eq!(selected_cohort.observed_runs, 1);
    assert_eq!(selected_cohort.conflicting_runs, 1);
    assert_eq!(selected_cohort.sample_count, 2);
    assert_eq!(
        selected_cohort
            .execution_dispositions
            .get(&ExecutionDisposition::Completed),
        Some(&1)
    );
    assert_eq!(
        selected_cohort.observation_statuses.get(&Status::Pass),
        Some(&1)
    );
    assert_eq!(
        selected_cohort.observation_statuses.get(&Status::Issue),
        Some(&1)
    );

    let later_inputs = fixtures::selection_inputs(
        &fixture,
        fixtures::DECISION_LATER,
        profile,
        quality_objective,
        admission,
        statistics.clone(),
        fixtures::policy_facts(&fixture.selections, Some(statistics.content_digest())),
    );
    let later = deterministic::select(&later_inputs).unwrap();
    let later_replay = deterministic::select(&later_inputs).unwrap();
    assert_eq!(later, later_replay);
    assert_eq!(
        later.input_refs().statistics_snapshot_digest(),
        statistics.content_digest()
    );
}
