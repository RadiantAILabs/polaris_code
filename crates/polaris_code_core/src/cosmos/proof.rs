//! The host-owned driver: one real coding task through the whole contract set.
//!
//! Cosmos supplies contracts, policy points, and an evidence store. It does
//! not run anything. Every step that *acts* in the sequence below belongs to
//! `polaris_code`:
//!
//! ```text
//! coding task
//!   -> profile evidence            (embodiment reads the repository)
//!   -> TaskProfile                 (Cosmos aggregation policy)
//!   -> availability snapshot       (embodiment observes its own host)
//!   -> mechanical preflight        (Cosmos, deterministic)
//!   -> deterministic selection     (Cosmos, per objective)
//!   -> SelectionRecord             (evidence)
//!   -> live readmission + permit   (Cosmos checks; embodiment prepared the context)
//!   -> GraphExecutor::execute      (embodiment; Cosmos never executes)
//!   -> RunOpened / RunFinalized    (evidence, mechanical)
//!   -> outcome observations        (embodiment's real verifiers, delayed, conflicting)
//!   -> statistics snapshot         (Cosmos, frozen)
//!   -> later selection             (Cosmos, bound to that exact digest)
//! ```
//!
//! Event identities are injected through [`ProofEventIds`] rather than
//! allocated here. Cosmos requires caller-allocated `UUIDv4` event IDs and
//! `polaris_code` has no UUID dependency, so a production host would generate
//! them and a test supplies fixed ones — which is also what makes the whole
//! sequence replayable.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use polaris::graph::{GraphExecutor, GraphSignature};
use polaris::system::param::SystemContext;
use polaris::system::server::Server;
use polaris_cosmos::contracts::{
    AdmissionAttemptId, AssessmentConclusion, AvailabilitySnapshotId, CanonicalRecord, CoveragePpm,
    DurationMs, EmbodimentId, EmbodimentKey, EpistemicBasis, ExecutionDisposition,
    ExecutionProvenance, HostRuntimeId, HostRuntimeKey, LogicalSequence, Objective,
    OutcomeObservation, OutcomeObservationId, RecordedAt, RunCompletion, RunControlFlow, RunFaults,
    RunFinalized, RunOpened, RunStructure, RunUsage, SelectionDecision, SelectionDecisionId,
    SelectionOutcome, SelectionRecord, SourceKind, Status, StopReason, TaskProfile,
};
use polaris_cosmos::evidence::{
    BoundedInMemoryEvidence, EvidenceEvent, TopologyEvidence, TopologyStatsSnapshot,
};
use polaris_cosmos::selection::deterministic::{self, SelectionInputs};
use polaris_cosmos::topology::{
    AdmissionCandidate, CandidateAdmissionSet, TopologyCatalog, TopologyCatalogPlugin,
    TopologyCatalogSnapshot, TopologyContract, preflight_catalog,
};

use super::payload::{CodingWorkspace, SweepWorklist};
use super::task::{CodingIntent, CodingObjective, CodingTask};
use super::topology::{self, CodingTopologyPackPlugin, HostCapabilities, PREDICTION_ERROR};
use super::{graphs, policy, provenance, verify};

/// Caller-allocated event identities for one proof run.
///
/// Every field is a `UUIDv4` in lowercase hyphenated form. They are correlation
/// identities, never clocks: ordering comes from the evidence store's
/// [`LogicalSequence`] alone.
#[derive(Debug, Clone)]
pub struct ProofEventIds {
    /// Identity of the availability observation.
    pub availability: AvailabilitySnapshotId,
    /// Decision that selects under the `Fast` objective.
    pub fast_decision: SelectionDecisionId,
    /// Decision that selects under the `Careful` objective.
    pub careful_decision: SelectionDecisionId,
    /// Decision that selects under the `Thorough` objective.
    pub thorough_decision: SelectionDecisionId,
    /// Decision that finds no eligible candidate.
    pub no_eligible_decision: SelectionDecisionId,
    /// Decision replayed from the frozen statistics digest.
    pub later_decision: SelectionDecisionId,
    /// Live readmission attempt for the executed candidate.
    pub admission: AdmissionAttemptId,
    /// The one started run.
    pub run: RunId,
    /// Immediate applied-edit verdict.
    pub applied_observation: OutcomeObservationId,
    /// Delayed project-wide residual verdict.
    pub residual_observation: OutcomeObservationId,
}

use polaris_cosmos::contracts::RunId;

/// Why the coding proof could not complete.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProofError {
    /// The catalog provider and coding pack could not be assembled.
    Assembly(String),
    /// The coding task or its profile was rejected.
    Task(super::task::CodingTaskError),
    /// Selector configuration or author priors were rejected.
    Policy(super::policy::PolicyError),
    /// A Cosmos record was rejected.
    Contract(polaris_cosmos::contracts::ContractError),
    /// Mechanical preflight or live readmission failed to build.
    Admission(String),
    /// Selection failed.
    Selection(String),
    /// The evidence store rejected an append or a statistics capture.
    Evidence(String),
    /// A step that must succeed for the proof to mean anything did not.
    Unmet(String),
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Assembly(detail) => formatter.write_str(detail),
            Self::Task(error) => error.fmt(formatter),
            Self::Policy(error) => error.fmt(formatter),
            Self::Contract(error) => error.fmt(formatter),
            Self::Admission(detail)
            | Self::Selection(detail)
            | Self::Evidence(detail)
            | Self::Unmet(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ProofError {}

impl From<super::task::CodingTaskError> for ProofError {
    fn from(error: super::task::CodingTaskError) -> Self {
        Self::Task(error)
    }
}

impl From<super::policy::PolicyError> for ProofError {
    fn from(error: super::policy::PolicyError) -> Self {
        Self::Policy(error)
    }
}

impl From<polaris_cosmos::contracts::ContractError> for ProofError {
    fn from(error: polaris_cosmos::contracts::ContractError) -> Self {
        Self::Contract(error)
    }
}

/// Everything one proof run establishes, for the consumer report to quote.
pub struct CodingProofReport {
    /// The frozen profile the coding payload aggregated to.
    pub profile: TaskProfile,
    /// Mechanical preflight result over all seven registrations.
    pub admission: CandidateAdmissionSet,
    /// Mechanical exclusion codes per registration, in canonical order.
    pub exclusions: BTreeMap<String, Vec<String>>,
    /// Human-readable detail for each mechanical exclusion, per registration.
    ///
    /// Empty for admitted candidates; retained so host-capability exclusions
    /// can include their stable detail without changing the report shape.
    pub exclusion_details: BTreeMap<String, Vec<String>>,
    /// Winner under each objective, by registration ID.
    pub winners: BTreeMap<&'static str, String>,
    /// The decision that was executed.
    pub executed_decision: SelectionDecision,
    /// The objective the executed decision ranked under.
    pub executed_objective: Objective,
    /// Role markers the executed graph actually ran, in order.
    pub executed_roles: Vec<String>,
    /// Whether the typed no-eligible diagnostic was produced.
    pub no_eligible_diagnostics: Vec<String>,
    /// The two conflicting outcome observations, in append order.
    pub observations: Vec<OutcomeObservation>,
    /// The frozen statistics snapshot captured after the run.
    pub statistics: TopologyStatsSnapshot,
    /// Whether a later selection replayed identically from that digest.
    pub later_selection_is_stable: bool,
    /// Whether the one-use start permit refused a second consumption.
    pub permit_is_single_use: bool,
    /// The file content the run left on disk.
    pub final_target_content: String,
}

/// The immutable snapshots every selection in one proof run ranks against.
///
/// Bundled so each decision provably reads the *same* catalog generation,
/// availability observation, and mechanical admission set: rebuilding any of
/// them per decision would make the comparison between objectives meaningless.
struct FrozenInputs<'run> {
    catalog: &'run TopologyCatalogSnapshot,
    contract: &'run TopologyContract,
    profile: &'run TaskProfile,
    availability: &'run polaris_cosmos::contracts::RuntimeAvailabilitySnapshot,
    admission: &'run CandidateAdmissionSet,
}

/// One end-to-end coding proof against the Cosmos topology-runtime contracts.
pub struct CodingProof {
    task: CodingTask,
    ids: ProofEventIds,
    host: HostCapabilities,
}

impl CodingProof {
    /// Prepares a proof for one coding task.
    #[must_use]
    pub const fn new(task: CodingTask, ids: ProofEventIds, host: HostCapabilities) -> Self {
        Self { task, ids, host }
    }

    fn select(
        frozen: &FrozenInputs<'_>,
        decision: SelectionDecisionId,
        objective: &Objective,
        statistics: &TopologyStatsSnapshot,
        observed: bool,
    ) -> Result<SelectionDecision, ProofError> {
        let inputs = SelectionInputs::new(
            decision,
            frozen.profile.clone(),
            objective.clone(),
            frozen.contract.key().clone(),
            frozen.catalog.clone(),
            frozen.availability.clone(),
            frozen.admission.clone(),
            statistics.clone(),
            policy::candidate_facts(
                frozen.catalog,
                observed.then_some((statistics, frozen.profile)),
            )?,
            policy::selector_config()?,
        );
        deterministic::select(&inputs).map_err(|error| ProofError::Selection(error.to_string()))
    }

    /// Runs the full sequence and returns what it established.
    ///
    /// # Errors
    ///
    /// Returns an error if any contract rejects a value, if the selected
    /// candidate fails live readmission, or if a step the proof depends on
    /// does not hold.
    pub async fn run(&self) -> Result<CodingProofReport, ProofError> {
        let mut server = Server::new();
        server
            .add_plugins(TopologyCatalogPlugin)
            .add_plugins(CodingTopologyPackPlugin);
        server
            .finish()
            .await
            .map_err(|error| ProofError::Assembly(error.to_string()))?;
        let snapshot = server
            .get_global::<TopologyCatalog>()
            .ok_or_else(|| ProofError::Assembly("catalog provider exposed no global".into()))?
            .snapshot()
            .map_err(|error| ProofError::Assembly(error.to_string()))?;
        let contract = snapshot
            .contracts()
            .iter()
            .find(|contract| contract.key().id().as_str() == topology::CONTRACT)
            .cloned()
            .ok_or_else(|| ProofError::Assembly("coding pack registered no contract".into()))?;
        let profile = self.task.profile()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let availability =
            topology::probe_availability(&self.task, self.ids.availability, now, self.host)?;

        // --- mechanical preflight ---------------------------------------
        let admission = preflight_catalog(&snapshot, contract.key(), &availability, false)
            .map_err(|error| ProofError::Admission(error.to_string()))?;

        let mut exclusions = BTreeMap::new();
        let mut exclusion_details = BTreeMap::new();
        for evaluation in admission.evaluations() {
            let registration = evaluation.candidate().registration_id().as_str().to_owned();
            exclusions.insert(
                registration.clone(),
                evaluation
                    .result()
                    .reasons()
                    .iter()
                    .map(|reason| reason.code().as_str().to_owned())
                    .collect::<Vec<_>>(),
            );
            exclusion_details.insert(
                registration,
                evaluation
                    .result()
                    .reasons()
                    .iter()
                    .filter_map(|reason| reason.detail().map(str::to_owned))
                    .collect::<Vec<_>>(),
            );
        }

        // --- selection under three objectives, plus one unsatisfiable ----
        let evidence = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(
            policy::evidence_config(),
        )));
        let statistics_request = policy::statistics_request();
        let empty_statistics = evidence
            .capture_statistics(&statistics_request)
            .map_err(|error| ProofError::Evidence(error.detail().to_owned()))?;

        let frozen = FrozenInputs {
            catalog: &snapshot,
            contract: &contract,
            profile: &profile,
            availability: &availability,
            admission: &admission,
        };

        let mut winners = BTreeMap::new();
        let mut executed = None;
        for (label, intent, decision_id) in [
            ("fast", CodingIntent::Fast, self.ids.fast_decision),
            ("careful", CodingIntent::Careful, self.ids.careful_decision),
            (
                "thorough",
                CodingIntent::Thorough,
                self.ids.thorough_decision,
            ),
        ] {
            let objective = CodingObjective::new(intent).build()?;
            let decision =
                Self::select(&frozen, decision_id, &objective, &empty_statistics, false)?;
            let winner = match decision.outcome() {
                SelectionOutcome::Selected { selected, .. } => {
                    selected.registration_id().as_str().to_owned()
                }
                SelectionOutcome::NoEligibleCandidate { .. } => {
                    return Err(ProofError::Unmet(format!(
                        "objective '{label}' found no eligible coding candidate"
                    )));
                }
            };
            if intent == CodingIntent::Careful {
                executed = Some((decision.clone(), objective.clone()));
            }
            winners.insert(label, winner);
        }

        let unsatisfiable = CodingObjective::new(CodingIntent::Careful)
            .requiring_comparative_verifier()
            .build()?;
        let no_eligible = Self::select(
            &frozen,
            self.ids.no_eligible_decision,
            &unsatisfiable,
            &empty_statistics,
            false,
        )?;
        let no_eligible_diagnostics = match no_eligible.outcome() {
            SelectionOutcome::NoEligibleCandidate { diagnostics } => diagnostics
                .iter()
                .map(|reason| reason.code().as_str().to_owned())
                .collect::<Vec<_>>(),
            SelectionOutcome::Selected { selected, .. } => {
                return Err(ProofError::Unmet(format!(
                    "a comparative verifier was required but '{}' was selected",
                    selected.registration_id().as_str()
                )));
            }
        };

        let (executed_decision, executed_objective) = executed.ok_or_else(|| {
            ProofError::Unmet("no objective produced an executed decision".into())
        })?;
        let selected = match executed_decision.outcome() {
            SelectionOutcome::Selected { selected, .. } => selected.clone(),
            SelectionOutcome::NoEligibleCandidate { .. } => {
                return Err(ProofError::Unmet(
                    "executed decision selected nothing".into(),
                ));
            }
        };
        if selected.registration_id().as_str() != PREDICTION_ERROR {
            return Err(ProofError::Unmet(format!(
                "expected the prediction-error loop to win the careful objective, got '{}'",
                selected.registration_id().as_str()
            )));
        }

        // --- selection is evidence, including before any run -------------
        let selection_record = SelectionRecord::new(
            RecordedAt::new(LogicalSequence::new(1), None),
            executed_decision.clone(),
            profile.clone(),
            executed_objective.clone(),
        )?;
        selection_record.validate()?;
        evidence
            .append(EvidenceEvent::Selection(Box::new(selection_record)))
            .map_err(|error| ProofError::Evidence(error.detail().to_owned()))?;

        // --- live context preparation is the embodiment's job ------------
        let workspace = CodingWorkspace::for_task(&self.task);
        let executor = GraphExecutor::new();
        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(super::gate_blocks::configurable::ModelStepHandoff::default());
        context.insert(SweepWorklist::new(graphs::SWEEP_BUDGET));
        // The offline driver has no session to inherit globals from, but the
        // run meter is the embodiment's own, and `probe_availability` reports
        // it as always available — so this context must carry one too.
        context.insert(crate::usage::TokenMeter::new());
        context.insert(self.task.question());

        let native_candidate = snapshot
            .candidates()
            .iter()
            .find(|candidate| candidate.registration_id().as_str() == PREDICTION_ERROR)
            .ok_or_else(|| ProofError::Unmet("selected candidate is not in the catalog".into()))?;
        let metadata = native_candidate.admission_metadata().ok_or_else(|| {
            ProofError::Unmet("selected candidate has no admission metadata".into())
        })?;
        let live_candidate = AdmissionCandidate::new(
            native_candidate.clone(),
            metadata.selection().clone(),
            metadata.requirements().clone(),
        );
        let known_schemas = snapshot
            .candidates()
            .iter()
            .filter_map(|candidate| candidate.admission_metadata())
            .map(|metadata| metadata.configuration_schema().clone())
            .collect::<BTreeSet<_>>();
        let live = {
            let catalog = server
                .get_global::<TopologyCatalog>()
                .ok_or_else(|| ProofError::Assembly("catalog provider exposed no global".into()))?;
            polaris_cosmos::topology::readmit_live(
                &catalog,
                snapshot.generation(),
                snapshot.content_digest(),
                snapshot.content_digest(),
                &live_candidate,
                &contract,
                &availability,
                &known_schemas,
                catalog.type_explanations(),
                &executor,
                &context,
                executed_decision.decision_id(),
                self.ids.admission,
                RecordedAt::new(LogicalSequence::new(2), None),
            )
            .map_err(|error| ProofError::Admission(error.to_string()))?
        };

        if !live.is_permitted() {
            let codes = live
                .evaluation()
                .result()
                .reasons()
                .iter()
                .map(|reason| reason.code().as_str().to_owned())
                .collect::<Vec<_>>();
            return Err(ProofError::Unmet(format!(
                "live readmission refused the selected coding topology: {codes:?}"
            )));
        }
        let (admission_event, permit) = live.into_parts();
        evidence
            .append(EvidenceEvent::Admission(admission_event.clone()))
            .map_err(|error| ProofError::Evidence(error.detail().to_owned()))?;

        let permit = permit
            .ok_or_else(|| ProofError::Unmet("a permitted admission yielded no permit".into()))?;
        let permitted = permit
            .consume(executed_decision.decision_id(), &selected, &context)
            .map_err(|error| ProofError::Admission(error.to_string()))?;
        if permitted != selected {
            return Err(ProofError::Unmet(
                "the permit released a different registration than was selected".into(),
            ));
        }
        // The permit is consumed by value, so a second start is a compile-time
        // impossibility rather than a runtime check.
        let permit_is_single_use = true;

        // --- the embodiment executes; Cosmos never does ------------------
        let registered = server
            .get_global::<TopologyCatalog>()
            .ok_or_else(|| ProofError::Assembly("catalog provider exposed no global".into()))?
            .resolve_at(selected.registration_id(), snapshot.generation())
            .map_err(|error| ProofError::Admission(error.to_string()))?;
        if !registered
            .signature()
            .satisfies(&GraphSignature::new().produce::<AssessmentConclusion>())
        {
            return Err(ProofError::Unmet(
                "the selected graph does not satisfy the coding contract".into(),
            ));
        }

        let opened = RunOpened::new(
            self.ids.run,
            RecordedAt::new(LogicalSequence::new(3), None),
            &executed_decision,
            &admission_event,
            vec![contract.key().clone()],
            0,
            execution_provenance()?,
        )?;
        evidence
            .append(EvidenceEvent::RunOpened(opened.clone()))
            .map_err(|error| ProofError::Evidence(error.detail().to_owned()))?;

        let started = std::time::Instant::now();
        let execution = executor
            .execute(registered.graph(), &mut context, None, None)
            .await
            .map_err(|error| {
                ProofError::Unmet(format!("coding graph execution failed: {error}"))
            })?;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .ok_or_else(|| ProofError::Unmet("the coding run produced no conclusion".into()))?;
        let _ = conclusion;

        // Finalization is mechanical: it records that the graph ran, and makes
        // no claim about whether the coding task succeeded.
        let nodes = u64::try_from(execution.nodes_executed()).unwrap_or(u64::MAX);
        let finalized = RunFinalized::new(
            opened.run_id(),
            RecordedAt::new(LogicalSequence::new(4), None),
            DurationMs::new(elapsed),
            RunStructure::new(nodes, nodes, Some(0), 1, 1, Vec::new())?,
            RunControlFlow::empty(),
            RunFaults::empty(),
            RunUsage::empty(),
            RunCompletion::new(
                ExecutionDisposition::Completed,
                Some(StopReason::PredicateSatisfied),
                Some(CoveragePpm::ONE),
                Some(0),
            )?,
            Vec::new(),
        )?;
        evidence
            .append(EvidenceEvent::RunFinalized(finalized))
            .map_err(|error| ProofError::Evidence(error.detail().to_owned()))?;

        // --- later judgement: two real verifiers that disagree -----------
        let verified = workspace
            .verify()
            .map_err(|error| ProofError::Unmet(error.to_string()))?;
        let final_target_content = verified
            .files
            .first()
            .map(|file| file.actual.clone())
            .unwrap_or_default();
        let applied = verify::applied_edit_observation(
            &workspace,
            &verified,
            self.ids.applied_observation,
            opened.run_id(),
            profile.task_id().clone(),
            5,
        )?;
        let residual = verify::work_outcome_observation(
            self.task.success(),
            &workspace,
            &verify::ObservationSlot {
                observation_id: self.ids.residual_observation,
                run_id: opened.run_id(),
                task_id: profile.task_id().clone(),
                sequence: 6,
                delay_ms: 1_000,
                supersedes: self.ids.applied_observation,
            },
        )?;
        for observation in [applied.clone(), residual.clone()] {
            evidence
                .append(EvidenceEvent::Observation(observation))
                .map_err(|error| ProofError::Evidence(error.detail().to_owned()))?;
        }

        let statistics = evidence
            .capture_statistics(&statistics_request)
            .map_err(|error| ProofError::Evidence(error.detail().to_owned()))?;

        // --- a later selection is pinned to that exact frozen digest -----
        let later = Self::select(
            &frozen,
            self.ids.later_decision,
            &executed_objective,
            &statistics,
            true,
        )?;
        let replay = Self::select(
            &frozen,
            self.ids.later_decision,
            &executed_objective,
            &statistics,
            true,
        )?;
        let later_selection_is_stable = later == replay
            && later.input_refs().statistics_snapshot_digest() == statistics.content_digest();

        Ok(CodingProofReport {
            profile,
            admission,
            exclusions,
            exclusion_details,
            winners,
            executed_decision,
            executed_objective,
            executed_roles: workspace.roles(),
            no_eligible_diagnostics,
            observations: vec![applied, residual],
            statistics,
            later_selection_is_stable,
            permit_is_single_use,
            final_target_content,
        })
    }
}

impl CodingProofReport {
    /// Returns the registrations mechanical preflight admitted.
    #[must_use]
    pub fn admitted(&self) -> Vec<String> {
        self.admission
            .evaluations()
            .iter()
            .filter(|evaluation| evaluation.result().is_admitted())
            .map(|evaluation| evaluation.candidate().registration_id().as_str().to_owned())
            .collect()
    }

    /// Returns whether the two verifiers reached opposing verdicts.
    #[must_use]
    pub fn observations_conflict(&self) -> bool {
        self.observations
            .iter()
            .any(|observation| observation.status() == Status::Pass)
            && self
                .observations
                .iter()
                .any(|observation| observation.status() == Status::Issue)
    }

    /// Returns the canonical order of every registration considered.
    #[must_use]
    pub fn considered(&self) -> Vec<String> {
        self.admission
            .evaluations()
            .iter()
            .map(|evaluation| evaluation.candidate().registration_id().as_str().to_owned())
            .collect()
    }
}

fn execution_provenance() -> Result<ExecutionProvenance, polaris_cosmos::contracts::ContractError> {
    ExecutionProvenance::new(
        EmbodimentKey::new(
            EmbodimentId::new("polaris-code.coding-embodiment")?,
            super::behavior_version(),
        ),
        HostRuntimeKey::new(
            HostRuntimeId::new("polaris-code.terminal-host")?,
            super::behavior_version(),
        ),
        // No model provider took part in this run, and no provider is claimed.
        Vec::new(),
        Vec::new(),
        vec![provenance(
            SourceKind::HostObservation,
            EpistemicBasis::Verified,
        )],
    )
}
