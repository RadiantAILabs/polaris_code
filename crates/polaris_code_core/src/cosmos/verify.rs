//! Real coding verifiers, projected onto outcome observations.
//!
//! Graph completion is mechanical: a coding graph that ran to the end says
//! only that it ran. Whether the *unit of work* succeeded is a separate,
//! later, append-only claim — and the two verifiers here deliberately
//! disagree, because that is what happens in real coding work.
//!
//! - [`applied_edit_observation`] checks the sites the request named. It is
//!   immediate and, on a clean run, passes. A unit of work that named no edit
//!   is [`Status::NotApplicable`] here, not a pass.
//! - [`work_outcome_observation`] answers the later question — *did the work
//!   succeed?* — by running whatever check the caller supplied.
//!
//! The second supersedes the first
//! ([`SupersedeReason::SupersededByLaterEvidence`]) rather than overwriting
//! it: both verdicts stay in the evidence log, and the disagreement is the
//! record.
//!
//! Success is the caller's to define, because this host cannot know it. The
//! three arms of [`SuccessCheck`] are the three honest answers:
//!
//! | [`SuccessCheck`] | Verifier here | Verdict |
//! |---|---|---|
//! | [`StatedEdits`](SuccessCheck::StatedEdits) | [`residual_stub_observation`] re-scans the tree for the stub marker | pass / issue |
//! | [`Command`](SuccessCheck::Command) | [`command_success_observation`] runs the caller's program in the sandbox | pass / issue, or indeterminate if it will not start |
//! | [`Delegated`](SuccessCheck::Delegated) | [`delegated_success_observation`] records that nobody here judged it | indeterminate, deferred |
//!
//! The command arm is what lets an external harness state its own test suite
//! without this crate learning that harness's conventions: the program and its
//! arguments come from the accepted unit of work, and nothing here inspects
//! them. It runs on the *host* side, after the graph, for the same reason the
//! residual scan does — a topology executing inside the pristine context must
//! not be able to reach a subprocess.

use std::process::Command;

use polaris_cosmos::contracts::{
    ConfidencePpm, ContractError, CoveragePpm, DurationMs, EpistemicBasis, EvidenceKind,
    LogicalSequence, Measurement, MeasurementEntry, MeasurementId, MeasurementValue,
    OutcomeObservation, OutcomeObservationId, RecordedAt, ResidualDisposition, RunId, SourceKind,
    Status, SupersedeReason, SupersedesClaim, TaskId,
};

use super::payload::{CodingWorkspace, VerifiedEdit};
use super::task::{SuccessCheck, SuccessCommand};
use super::{evidenced, source_key};

/// Measurement recording whether the named edits landed.
pub const EDIT_APPLIED: &str = "edit-applied";

/// Measurement recording how much of the tree is free of stub markers.
pub const STUB_FREE_COVERAGE: &str = "stub-free-coverage";

/// Measurement recording whether the caller's success check passed.
///
/// Cosmos reserves this measurement ID: it validates that the proportion
/// agrees with the observation status, so a "succeeded" claim cannot drift
/// from its own number.
pub const VERIFIED_SUCCESS: &str = "verified-success";

/// Confidence a deterministic file-content or exit-status check reports.
const DETERMINISTIC_CONFIDENCE: u32 = 990_000;

/// Confidence attached to a verdict this host did not itself check.
///
/// Deliberately low. The record is certain about one thing only — that the
/// judgement was delegated — and pretending otherwise would let an unchecked
/// run outrank a checked one.
const DELEGATED_CONFIDENCE: u32 = 100_000;

/// Where a later verdict is written into the evidence log.
///
/// These fields travel together because Cosmos wants all of them for one
/// append, and because none of them depends on which success check ran: the
/// record's own identity, the run and task it belongs to, its place in the
/// evidence order, how long after the run the check was taken, and the earlier
/// verdict it supersedes.
#[derive(Debug, Clone)]
pub struct ObservationSlot {
    /// Identity of the record being written.
    pub observation_id: OutcomeObservationId,
    /// Run whose work is being judged.
    pub run_id: RunId,
    /// Task that run was working on.
    pub task_id: TaskId,
    /// Position of this record in the evidence order.
    pub sequence: u64,
    /// Milliseconds between the run finishing and this check being taken.
    pub delay_ms: u64,
    /// The earlier, immediate verdict this one supersedes.
    pub supersedes: OutcomeObservationId,
}

/// One success check's answer, before it becomes a Cosmos record.
struct Verdict {
    status: Status,
    disposition: Option<ResidualDisposition>,
    measurements: Vec<MeasurementEntry>,
    source_kind: SourceKind,
    basis: EpistemicBasis,
    evidence_kind: EvidenceKind,
    object: &'static str,
    confidence: u32,
}

fn proportion(
    id: &str,
    value: CoveragePpm,
    object: &str,
) -> Result<MeasurementEntry, ContractError> {
    MeasurementEntry::new(
        MeasurementId::new(id)?,
        Measurement::new(
            MeasurementValue::Proportion(value),
            None,
            ConfidencePpm::new(DETERMINISTIC_CONFIDENCE)?,
            vec![evidenced(
                polaris_cosmos::contracts::SourceKind::AutomatedVerifier,
                EpistemicBasis::Verified,
                EvidenceKind::VerifierOutput,
                object,
            )],
        )?,
    )
}

/// Verifies that the stated edits landed in the files they named.
///
/// `verified` is the edited files as re-read from disk by the caller, not the
/// run's own report of what it wrote. The read is the caller's so this
/// projection stays pure and cannot fail for an I/O reason.
///
/// A unit of work that stated no edits has nothing for this verifier to
/// check. It reports [`Status::NotApplicable`] with **no** measurement rather
/// than a zero one: "no edit was stated" and "the stated edit did not land"
/// are different facts, and a zero proportion would conflate them.
///
/// # Errors
///
/// Returns an error if a Cosmos record rejects a derived value.
pub fn applied_edit_observation(
    workspace: &CodingWorkspace,
    verified: &VerifiedEdit,
    observation_id: OutcomeObservationId,
    run_id: RunId,
    task_id: TaskId,
    sequence: u64,
) -> Result<OutcomeObservation, ContractError> {
    let status = workspace.applied_edit_status(verified);
    let measurements = if status == Status::NotApplicable {
        Vec::new()
    } else {
        let landed = if status == Status::Pass {
            CoveragePpm::ONE
        } else {
            CoveragePpm::ZERO
        };
        vec![proportion(EDIT_APPLIED, landed, "verifier.applied-edit")?]
    };
    OutcomeObservation::new(
        observation_id,
        run_id,
        task_id,
        RecordedAt::new(LogicalSequence::new(sequence), None),
        None,
        Some(DurationMs::new(0)),
        status,
        None,
        measurements,
        None,
        Vec::new(),
        None,
        source_key(),
        polaris_cosmos::contracts::SourceKind::AutomatedVerifier,
        ConfidencePpm::new(DETERMINISTIC_CONFIDENCE)?,
        vec![evidenced(
            polaris_cosmos::contracts::SourceKind::AutomatedVerifier,
            EpistemicBasis::Verified,
            EvidenceKind::VerifierOutput,
            "verifier.applied-edit",
        )],
        Vec::new(),
    )
}

/// Answers *did the work succeed?* with the check the caller supplied.
///
/// This is the later, superseding verdict. Which verifier runs is the
/// caller's choice, made when the unit of work was accepted; this function
/// only dispatches. Nothing here knows what the work was for.
///
/// # Errors
///
/// Returns an error if a record is rejected.
pub fn work_outcome_observation(
    success: &SuccessCheck,
    workspace: &CodingWorkspace,
    slot: &ObservationSlot,
) -> Result<OutcomeObservation, ContractError> {
    match success {
        SuccessCheck::StatedEdits { .. } => residual_stub_observation(workspace, slot),
        SuccessCheck::Command(command) => command_success_observation(command, workspace, slot),
        SuccessCheck::Delegated { .. } => delegated_success_observation(slot),
    }
}

/// Re-scans the whole tree and reports any stub site that survives.
///
/// This runs *after* the immediate check and supersedes it, because a run that
/// correctly applied the edit it was asked for can still leave the project
/// unfinished. Reporting only the first verdict would call that a success.
///
/// A unit of work that named no stub marker has no surviving sites to find,
/// so this passes. That is the honest reading: the caller asked for the
/// stated edits and nothing more.
///
/// # Errors
///
/// Returns an error if a record is rejected.
pub fn residual_stub_observation(
    workspace: &CodingWorkspace,
    slot: &ObservationSlot,
) -> Result<OutcomeObservation, ContractError> {
    let clean = workspace.stub_sites().is_empty();
    let status = if clean { Status::Pass } else { Status::Issue };
    let coverage = if clean {
        CoveragePpm::ONE
    } else {
        CoveragePpm::ZERO
    };
    later_observation(
        slot,
        Verdict {
            status,
            // A surviving stub is deferred work, not a blocked run: the edit
            // that was asked for did land.
            disposition: (status == Status::Issue).then_some(ResidualDisposition::Deferred),
            measurements: vec![proportion(
                STUB_FREE_COVERAGE,
                coverage,
                "verifier.residual-stub-scan",
            )?],
            source_kind: SourceKind::AutomatedVerifier,
            basis: EpistemicBasis::Verified,
            evidence_kind: EvidenceKind::VerifierOutput,
            object: "verifier.residual-stub-scan",
            confidence: DETERMINISTIC_CONFIDENCE,
        },
    )
}

/// Runs the caller's success command in the sandbox and reports its verdict.
///
/// Exit zero is a pass and anything else is an issue. A command that will not
/// start at all is [`Status::Indeterminate`] and [`Blocked`], not a failure:
/// "the work is wrong" and "we could not find out" are different claims, and
/// an unrunnable verifier proves nothing about the code.
///
/// The command runs with the sandbox root as its working directory. It is not
/// otherwise confined — running it is the caller's decision, taken when the
/// unit of work was accepted.
///
/// [`Blocked`]: ResidualDisposition::Blocked
///
/// # Errors
///
/// Returns an error if a record is rejected.
pub fn command_success_observation(
    command: &SuccessCommand,
    workspace: &CodingWorkspace,
    slot: &ObservationSlot,
) -> Result<OutcomeObservation, ContractError> {
    let outcome = Command::new(command.program())
        .args(command.args())
        .current_dir(workspace.config().working_dir.as_path())
        .output();
    let (status, disposition, measurements) = match outcome {
        Ok(output) if output.status.success() => (
            Status::Pass,
            None,
            vec![proportion(
                VERIFIED_SUCCESS,
                CoveragePpm::ONE,
                "verifier.success-command",
            )?],
        ),
        Ok(_) => (
            Status::Issue,
            Some(ResidualDisposition::Deferred),
            vec![proportion(
                VERIFIED_SUCCESS,
                CoveragePpm::ZERO,
                "verifier.success-command",
            )?],
        ),
        Err(error) => {
            tracing::warn!(
                program = command.program(),
                %error,
                "the success command could not be run"
            );
            (
                Status::Indeterminate,
                Some(ResidualDisposition::Blocked),
                Vec::new(),
            )
        }
    };
    later_observation(
        slot,
        Verdict {
            status,
            disposition,
            measurements,
            source_kind: SourceKind::AutomatedVerifier,
            basis: EpistemicBasis::Verified,
            evidence_kind: EvidenceKind::VerifierOutput,
            object: "verifier.success-command",
            confidence: DETERMINISTIC_CONFIDENCE,
        },
    )
}

/// Records that success was left for somebody else to judge.
///
/// No measurement is written, because none was taken. The record exists so
/// that "nobody checked" is in the evidence log rather than absent from it.
///
/// # Errors
///
/// Returns an error if a record is rejected.
pub fn delegated_success_observation(
    slot: &ObservationSlot,
) -> Result<OutcomeObservation, ContractError> {
    later_observation(
        slot,
        Verdict {
            status: Status::Indeterminate,
            disposition: Some(ResidualDisposition::Deferred),
            measurements: Vec::new(),
            source_kind: SourceKind::HostObservation,
            basis: EpistemicBasis::Asserted,
            evidence_kind: EvidenceKind::ObservationNote,
            object: "verifier.delegated",
            confidence: DELEGATED_CONFIDENCE,
        },
    )
}

/// Builds the delayed, superseding observation every success check produces.
fn later_observation(
    slot: &ObservationSlot,
    verdict: Verdict,
) -> Result<OutcomeObservation, ContractError> {
    OutcomeObservation::new(
        slot.observation_id,
        slot.run_id,
        slot.task_id.clone(),
        RecordedAt::new(LogicalSequence::new(slot.sequence), None),
        None,
        Some(DurationMs::new(slot.delay_ms)),
        verdict.status,
        verdict.disposition,
        verdict.measurements,
        None,
        Vec::new(),
        Some(SupersedesClaim::new(
            slot.supersedes,
            SupersedeReason::SupersededByLaterEvidence,
        )),
        source_key(),
        verdict.source_kind,
        ConfidencePpm::new(verdict.confidence)?,
        vec![evidenced(
            verdict.source_kind,
            verdict.basis,
            verdict.evidence_kind,
            verdict.object,
        )],
        Vec::new(),
    )
}
