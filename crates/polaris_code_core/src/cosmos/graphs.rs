//! The seven coding topologies, as real Polaris graphs.
//!
//! Every graph here works on the actual working tree through
//! [`CodingWorkspace`] and terminates in an
//! [`AssessmentConclusion`],
//! which is the one contract they all claim. Five are mechanically
//! admissible; the other two truthfully declare host capabilities that the
//! hermetic proof does not provide.
//!
//! Six of them *edit* or *audit for missing edits*. The seventh,
//! [`tree_question_graph`], answers a bounded question about the tree and
//! stops, writing nothing — so a selector asked a question has somewhere to
//! route it.
//!
//! # Where the shape comes from
//!
//! None of the shapes below are owned here. Cosmos ships the reusable graph
//! blocks and this module supplies only the coding systems that fill them:
//!
//! | Graph | Cosmos shape |
//! |-------|--------------|
//! | Direct edit | [`blocks::verified_step`] + [`blocks::resolve_residuals`] |
//! | Prediction-error edit | [`prediction_error_loop::build`] |
//! | Worklist sweep | [`blocks::drain_worklist`] + [`blocks::resolve_residuals`] |
//! | Test-gated edit | [`blocks::verified_step`] + [`blocks::resolve_residuals`] |
//! | Model-proposed edit | [`blocks::verified_step`] + [`blocks::resolve_residuals`] |
//! | Coverage audit | [`coverage_sweep::build`] |
//!
//! One shipped block has no coding consumer: [`blocks::status_dispatch`]. No
//! coding topology branches on a [`Status`], so adopting it would mean adding a
//! four-arm switch whose arms are all empty — nodes with no behaviour — plus a
//! new `Status` graph boundary the host would have to declare available.
//! [`blocks::seeded_loop`] is reached through its [`blocks::drain_worklist`]
//! alias, which is the accounting-bearing name for what the sweep does.
//!
//! [`tree_question_graph`] is absent from that table on purpose: it is three
//! embodiment-owned systems in sequence, and no shipped block describes
//! "survey, assess, answer" without an edit in it.

use polaris::graph::Graph;
use polaris::system::param::{Out, Res, ResMut};
use polaris::system::system;
use polaris::system::system::{SystemError, erase};
use polaris_cosmos::cognition::approved::blocks::{self, VerifiedRoles};
use polaris_cosmos::cognition::approved::{OracleQualification, coverage_sweep};
use polaris_cosmos::cognition::{ResolutionInput, prediction_error_loop};
use polaris_cosmos::contracts::{
    Assessment, AssessmentConclusion, AssessmentSummary, EvidenceKind, EvidenceObjectId,
    EvidenceRef, EvidenceStoreId, Status, StopReason,
};

use super::EVIDENCE_STORE;
use super::assess::{
    conclude, conclude_findings, conclude_sites, finding_assessment, site_assessment, summarize,
};
use super::payload::{
    AppliedEdit, CodingQuestion, CodingWorkspace, EditFeedback, EditSite, PredictedEdit,
    ProjectTestRunner, QuestionFinding, SiteObservation, SweepProgress, SweepWorklist,
    VerifiedEdit,
};

/// Iteration budget for the worklist sweep.
///
/// A bound is mandatory: an unbounded sweep over a repository is not a
/// cognitive model, it is a hang. Sites beyond the budget become visible
/// residuals rather than silent omissions.
pub const SWEEP_BUDGET: usize = 8;

fn exec(error: impl std::fmt::Display) -> SystemError {
    SystemError::ExecutionError(error.to_string())
}

/// Assesses one site per edited file, all carrying `status`.
fn assess_sites(
    workspace: &CodingWorkspace,
    status: Status,
) -> Result<Vec<Assessment<EditSite>>, SystemError> {
    workspace
        .edit_sites(status)
        .into_iter()
        .map(site_assessment)
        .collect::<Result<Vec<_>, _>>()
        .map_err(exec)
}

/// Returns the oracle qualification the shipped cognition points require.
///
/// A known-negative control is the honest claim available here: the
/// deterministic verifier provably rejects a file that still carries the stub
/// marker, which is a rejected negative control and nothing stronger.
///
/// # Panics
///
/// Never: the evidence identity is a compile-time constant.
#[must_use]
pub fn oracle_qualification() -> OracleQualification {
    OracleQualification::rejected_negative_control(EvidenceRef::new(
        EvidenceStoreId::new(EVIDENCE_STORE).expect("evidence store ID is valid"),
        EvidenceKind::VerifierOutput,
        EvidenceObjectId::new("oracle.stub-marker-negative-control")
            .expect("evidence object ID is valid"),
    ))
}

// ---------------------------------------------------------------------------
// Shared single-pass systems: produce one edit, judge it, account for it.
// ---------------------------------------------------------------------------

/// Judges every edited file and emits the one verdict of a single pass.
///
/// Shared by every single-pass registration: the verdict rule is the same
/// deterministic check over all the stated edits, whoever proposed them. The
/// verdict is filed against the unit of work's subject path, because
/// [`blocks::verified_step`] wants one oracle output; the per-file breakdown is
/// the resolver's job.
#[system]
pub(super) async fn judge_target_edit(
    workspace: Res<CodingWorkspace>,
    _applied: Out<AppliedEdit>,
) -> Result<Assessment<EditSite>, SystemError> {
    site_assessment(judged_subject_site(&workspace)?).map_err(exec)
}

/// Verifies the stated edits and files the whole unit of work's verdict.
fn judged_subject_site(workspace: &CodingWorkspace) -> Result<EditSite, SystemError> {
    let verified = workspace.verify().map_err(exec)?;
    let status = workspace.judge_verified(&verified);
    Ok(EditSite::new(
        workspace.subject_path().to_owned(),
        0,
        status,
    ))
}

/// Accounts for the judged pass, one site per edited file, and concludes.
///
/// A single pass has no unvisited work, so its residual accounting is the empty
/// set — stated rather than assumed. The judge's single verdict is expanded
/// here so a unit of work that stated edits in several files reaches the
/// conclusion as several sites rather than one.
#[system]
pub(super) async fn conclude_single_pass(
    workspace: Res<CodingWorkspace>,
    assessment: Out<Assessment<EditSite>>,
) -> Result<AssessmentConclusion, SystemError> {
    conclude_sites(
        assess_sites(&workspace, assessment.status())?,
        Vec::new(),
        StopReason::GoalMet,
    )
    .map_err(exec)
}

// ---------------------------------------------------------------------------
// 1. Direct edit — Cosmos `verified_step` + `resolve_residuals`.
// ---------------------------------------------------------------------------

#[system]
pub(super) async fn direct_apply(
    workspace: Res<CodingWorkspace>,
) -> Result<AppliedEdit, SystemError> {
    workspace.trace("direct.act");
    workspace.apply().map_err(exec)
}

#[system]
pub(super) async fn direct_judge(
    workspace: Res<CodingWorkspace>,
    _applied: Out<AppliedEdit>,
) -> Result<Assessment<EditSite>, SystemError> {
    let site = judged_subject_site(&workspace)?;
    workspace.trace("direct.verify");
    site_assessment(site).map_err(exec)
}

/// Builds the direct single-pass coding edit.
///
/// One read, one write, one verification. No loop, so a landed-but-unexpected
/// edit is never re-examined — which is exactly the quality cost its author
/// prior records.
///
/// The producer and the oracle are separate nodes because Cosmos's
/// [`blocks::verified_step`] makes the judgement independently schedulable;
/// fusing them would be the documented replacement, and is wrong here because
/// this oracle is a file read that does not depend on how the edit was made.
#[must_use]
pub fn direct_edit_graph() -> Graph {
    let mut graph = Graph::new();
    blocks::verified_step(
        &mut graph,
        erase(direct_apply),
        erase(direct_judge),
        &oracle_qualification(),
        VerifiedRoles::Correction,
    );
    blocks::resolve_residuals(&mut graph, erase(conclude_single_pass), false);
    graph
}

// ---------------------------------------------------------------------------
// 2. Prediction-error edit — Cosmos `prediction_error_loop`, embodied.
// ---------------------------------------------------------------------------

pub(super) async fn initialize_feedback() -> EditFeedback {
    EditFeedback {
        iteration: 0,
        matched: false,
    }
}

#[system]
pub(super) async fn predict_edit(
    workspace: Res<CodingWorkspace>,
) -> Result<PredictedEdit, SystemError> {
    workspace.trace("prediction.predict");
    workspace.predict().map_err(exec)
}

#[system]
pub(super) async fn act_edit(
    workspace: Res<CodingWorkspace>,
    _prediction: Out<PredictedEdit>,
) -> Result<AppliedEdit, SystemError> {
    workspace.trace("prediction.act");
    workspace.apply().map_err(exec)
}

#[system]
pub(super) async fn verify_edit(
    workspace: Res<CodingWorkspace>,
    _applied: Out<AppliedEdit>,
) -> Result<VerifiedEdit, SystemError> {
    workspace.trace("prediction.verify");
    workspace.verify().map_err(exec)
}

#[system]
pub(super) async fn compare_edit(
    workspace: Res<CodingWorkspace>,
    prediction: Out<PredictedEdit>,
    verified: Out<VerifiedEdit>,
) -> EditFeedback {
    workspace.trace("prediction.compare");
    EditFeedback {
        iteration: workspace.record_attempt(),
        matched: prediction.matches(&verified),
    }
}

#[system]
pub(super) async fn accept_edit(workspace: Res<CodingWorkspace>) {
    workspace.trace("prediction.accept");
}

#[system]
pub(super) async fn reject_edit(workspace: Res<CodingWorkspace>) {
    workspace.trace("prediction.reject");
}

#[system]
pub(super) async fn conclude_prediction(
    workspace: Res<CodingWorkspace>,
    feedback: Out<EditFeedback>,
) -> Result<AssessmentConclusion, SystemError> {
    let verified = workspace.verify().map_err(exec)?;
    // The prediction check and the content check are separate claims: the disk
    // may match the prediction while the edit itself is still wrong.
    let status = if feedback.matched {
        workspace.judge_verified(&verified)
    } else {
        Status::Issue
    };
    conclude_sites(
        assess_sites(&workspace, status)?,
        Vec::new(),
        if feedback.matched {
            StopReason::PredicateSatisfied
        } else {
            StopReason::Stagnated
        },
    )
    .map_err(exec)
}

/// Builds the prediction-error coding loop.
///
/// The prediction is the full expected post-edit file content, committed
/// before the write; verification re-reads the bytes from disk. The comparison
/// is therefore a real falsifiable prediction error, not a self-report.
#[must_use]
pub fn prediction_error_edit_graph() -> Graph {
    let mut graph = Graph::new();
    prediction_error_loop::build(
        &mut graph,
        "polaris_code_prediction_error_edit",
        prediction_error_loop::Stages::new(
            initialize_feedback,
            predict_edit,
            act_edit,
            verify_edit,
            compare_edit,
        ),
        prediction_error_loop::Policies::new(
            // One committed prediction per request: a second iteration would
            // predict against an already-edited file.
            |feedback: &EditFeedback| feedback.iteration >= 1,
            |feedback: &EditFeedback| feedback.matched,
            |branch: &mut Graph| {
                branch.add_system(accept_edit);
            },
            |branch: &mut Graph| {
                branch.add_system(reject_edit);
            },
        ),
    );
    graph.add_system(conclude_prediction);
    graph
}

// ---------------------------------------------------------------------------
// 3. Worklist sweep — Cosmos `drain_worklist` + `resolve_residuals`.
// ---------------------------------------------------------------------------

#[system]
pub(super) async fn seed_worklist(
    workspace: Res<CodingWorkspace>,
    mut worklist: ResMut<SweepWorklist>,
) -> SweepProgress {
    workspace.trace("sweep.scope");
    let scanned = workspace.stub_sites();
    worklist.seed(scanned.iter().map(EditSite::pending).collect());
    worklist.progress()
}

#[system]
pub(super) async fn visit_site(
    workspace: Res<CodingWorkspace>,
    mut worklist: ResMut<SweepWorklist>,
) -> Result<SweepProgress, SystemError> {
    if let Some(site) = worklist.take_next() {
        workspace.trace("sweep.probe");
        // The target site is the one this request is actually allowed to edit;
        // every other site is audited, never rewritten.
        let status = if workspace.targets_path(&site.path) {
            workspace.apply_path(&site.path).map_err(exec)?
        } else {
            let content = workspace.read(&site.path).map_err(exec)?;
            workspace.judge_marker(&content)
        };
        worklist.record(EditSite::new(site.path, site.line, status));
    }
    Ok(worklist.progress())
}

#[system]
pub(super) async fn conclude_sweep(
    worklist: Res<SweepWorklist>,
) -> Result<AssessmentConclusion, SystemError> {
    let mut assessed = Vec::new();
    for site in worklist.visited() {
        assessed.push(site_assessment(site.clone()).map_err(exec)?);
    }
    let unvisited = worklist.unvisited();
    let stop = if unvisited.is_empty() {
        StopReason::WorkExhausted
    } else {
        StopReason::BudgetExhausted
    };
    conclude_sites(assessed, unvisited, stop).map_err(exec)
}

/// Builds the bounded worklist sweep over embodiment-owned state.
///
/// The *shape* is Cosmos's [`blocks::drain_worklist`] — a seeded native loop
/// with a typed exhaustion predicate — closed by
/// [`blocks::resolve_residuals`], so unvisited sites reach an accounting role
/// instead of being dropped. What stays here is the coding state: this point
/// keeps its own worklist resource, while the separate [`coverage_sweep`]
/// registration uses the richer Cosmos-owned worklist and ledger. Both remain
/// ordinary catalog candidates rather than one being treated as the canonical
/// sweep.
#[must_use]
pub fn worklist_sweep_graph() -> Graph {
    let mut graph = Graph::new();
    blocks::drain_worklist(
        &mut graph,
        "polaris_code_stub_worklist",
        erase(seed_worklist),
        Box::new(|progress: &SweepProgress| progress.remaining == 0 || progress.budget_left == 0),
        Box::new(|body: &mut Graph| {
            body.add_system(visit_site);
        }),
    );
    blocks::resolve_residuals(&mut graph, erase(conclude_sweep), false);
    graph
}

// ---------------------------------------------------------------------------
// 4. Test-gated edit — the right oracle, which this host cannot supply.
// ---------------------------------------------------------------------------

#[system]
async fn test_gated_apply(workspace: Res<CodingWorkspace>) -> Result<AppliedEdit, SystemError> {
    workspace.trace("test-gated.act");
    workspace.apply().map_err(exec)
}

#[system]
pub(super) async fn test_gated_judge(
    workspace: Res<CodingWorkspace>,
    runner: Res<ProjectTestRunner>,
    _applied: Out<AppliedEdit>,
) -> Result<Assessment<EditSite>, SystemError> {
    let status = runner.judge(workspace.config().working_dir.as_path());
    site_assessment(EditSite::new(
        workspace.subject_path().to_owned(),
        0,
        status,
    ))
    .map_err(exec)
}

/// Builds the test-gated coding edit.
///
/// A real project test run is the strongest oracle a coding agent has, and it
/// is the one a hermetic host will not wire: this registration's judge names
/// [`ProjectTestRunner`] and the graph is excluded `resource.missing` until a
/// host installs one. The runner sits on the judge rather than the producer
/// because it is the *oracle* that needs it, and
/// [`blocks::verified_step`] is what makes that distinction visible.
#[must_use]
pub fn test_gated_edit_graph() -> Graph {
    let mut graph = Graph::new();
    blocks::verified_step(
        &mut graph,
        erase(test_gated_apply),
        erase(test_gated_judge),
        &oracle_qualification(),
        VerifiedRoles::Correction,
    );
    blocks::resolve_residuals(&mut graph, erase(conclude_single_pass), false);
    graph
}

// ---------------------------------------------------------------------------
// 5. Model-proposed edit — provider-backed variant of the direct shape.
// ---------------------------------------------------------------------------

#[system]
async fn model_proposed_apply(workspace: Res<CodingWorkspace>) -> Result<AppliedEdit, SystemError> {
    workspace.trace("model-proposed.act");
    workspace.apply().map_err(exec)
}

/// Builds the model-proposed coding edit.
///
/// Structurally this is the direct shape, deliberately: what a model adds is
/// not a different *graph*, it is a provider dependency. The registration
/// declares that provider through
/// [`CandidateRequirements`](polaris_cosmos::topology::CandidateRequirements)
/// so this candidate is excluded on the non-typed requirement channel rather
/// than on anything visible in its signature. It reuses the shared oracle,
/// which is the honest claim: a model proposes the edit, a deterministic check
/// judges it.
#[must_use]
pub fn model_proposed_edit_graph() -> Graph {
    let mut graph = Graph::new();
    blocks::verified_step(
        &mut graph,
        erase(model_proposed_apply),
        erase(judge_target_edit),
        &oracle_qualification(),
        VerifiedRoles::Correction,
    );
    blocks::resolve_residuals(&mut graph, erase(conclude_single_pass), false);
    graph
}

// ---------------------------------------------------------------------------
// 6. Coverage audit — Cosmos `coverage_sweep`, embodied directly.
// ---------------------------------------------------------------------------

#[system]
async fn sweep_scope(workspace: Res<CodingWorkspace>) -> Vec<EditSite> {
    workspace.trace("coverage.scope");
    workspace
        .stub_sites()
        .iter()
        .map(EditSite::pending)
        .collect()
}

#[system]
async fn sweep_empty(workspace: Res<CodingWorkspace>) -> Result<Assessment<EditSite>, SystemError> {
    // The scope was produced by a real marker scan. Finding no sites is thus
    // positive evidence that this audit has no outstanding work, not merely a
    // verifier that happened not to apply.
    site_assessment(EditSite::new(
        workspace.subject_path().to_owned(),
        0,
        Status::Pass,
    ))
    .map_err(exec)
}

#[system]
async fn sweep_probe(
    workspace: Res<CodingWorkspace>,
    subject: Out<EditSite>,
) -> Result<SiteObservation, SystemError> {
    let content = workspace.read(&subject.path).map_err(exec)?;
    Ok(SiteObservation {
        path: subject.path.clone(),
        marker_present: workspace.marker_present(&content),
        bytes: content.len(),
    })
}

#[system]
async fn sweep_adjudicate(
    subject: Out<EditSite>,
    observation: Out<SiteObservation>,
) -> Result<Assessment<EditSite>, SystemError> {
    let status = if observation.marker_present {
        Status::Issue
    } else {
        Status::Pass
    };
    site_assessment(EditSite::new(subject.path.clone(), subject.line, status)).map_err(exec)
}

#[system]
async fn sweep_gaps(pending: Out<Vec<EditSite>>) -> Result<Vec<Assessment<EditSite>>, SystemError> {
    pending
        .iter()
        .cloned()
        .map(|site| site_assessment(EditSite::new(site.path, site.line, Status::Indeterminate)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(exec)
}

#[system]
async fn sweep_combine(
    input: Out<ResolutionInput<EditSite>>,
) -> Result<AssessmentSummary, SystemError> {
    summarize(&input).map_err(exec)
}

#[system]
async fn sweep_resolve(
    input: Out<ResolutionInput<EditSite>>,
    summary: Out<AssessmentSummary>,
) -> Result<AssessmentConclusion, SystemError> {
    conclude(&input, summary.clone()).map_err(exec)
}

/// Builds the coding embodiment of Cosmos's shipped coverage sweep.
///
/// The coding pack binds its public payloads and calls
/// [`coverage_sweep::register_type_explanations`] for the private worklist,
/// ledger, control, progress, and resolution boundaries. The graph can
/// therefore be registered atomically and discovered without duplicating the
/// Cosmos shape.
#[must_use]
pub fn coverage_audit_graph() -> Graph {
    let mut graph = Graph::new();
    coverage_sweep::build(
        &mut graph,
        coverage_sweep::Stages {
            scope: erase(sweep_scope),
            empty_scope: erase(sweep_empty),
            probe: erase(sweep_probe),
            adjudicate: erase(sweep_adjudicate),
            assess_gaps: erase(sweep_gaps),
            combine: erase(sweep_combine),
            resolve: erase(sweep_resolve),
        },
        oracle_qualification(),
    );
    graph
}

// ---------------------------------------------------------------------------
// 7. Tree question — answer and stop, writing nothing.
// ---------------------------------------------------------------------------

/// How many occurrences one [`CodingQuestion::Locate`] will report.
///
/// A bound is mandatory for the same reason the sweep has one, and it is
/// *visible* rather than silent: a scan that hits the ceiling reports a
/// truncation finding, so the conclusion says the answer is partial instead
/// of pretending the tenth match was the last one.
pub const QUESTION_BUDGET: usize = 16;

/// Reads the working tree and turns one question into a finding ledger.
///
/// The ledger is never empty. That is the whole reason this shape can
/// terminate in the ordinary contract: "nothing matches" and "I cannot answer
/// that" are both *findings*, with a status that tells them apart, rather
/// than an absence the summary contract would reject.
#[system]
pub(super) async fn survey_tree(
    workspace: Res<CodingWorkspace>,
    question: Res<CodingQuestion>,
) -> Result<Vec<QuestionFinding>, SystemError> {
    workspace.trace("question.survey");
    let asked = question.asked();
    let finding = |path: String, line: usize, claim: String, status: Status| {
        QuestionFinding::new(asked.clone(), path, line, claim, status)
    };
    let findings = match &*question {
        CodingQuestion::Locate { needle } => {
            let hits = super::edit::scan_occurrences(
                workspace.config().working_dir.as_path(),
                needle.as_str(),
            );
            if hits.is_empty() {
                vec![finding(
                    String::new(),
                    0,
                    format!("no line in the working tree contains `{needle}`"),
                    Status::Pass,
                )]
            } else {
                let truncated = hits.len() > QUESTION_BUDGET;
                let total = hits.len();
                let mut findings: Vec<_> = hits
                    .into_iter()
                    .take(QUESTION_BUDGET)
                    .map(|hit| {
                        let claim = format!("`{needle}` occurs at {}:{}", hit.path, hit.line);
                        finding(hit.path, hit.line, claim, Status::Pass)
                    })
                    .collect();
                if truncated {
                    findings.push(finding(
                        String::new(),
                        0,
                        format!(
                            "{total} lines match, but only the first {QUESTION_BUDGET} are \
                             reported; this answer is partial"
                        ),
                        Status::Issue,
                    ));
                }
                findings
            }
        }
        CodingQuestion::Inspect { path } => match workspace.read(path) {
            Ok(content) => vec![finding(
                path.clone(),
                0,
                format!(
                    "{path} is {} lines, {} bytes",
                    content.lines().count(),
                    content.len()
                ),
                Status::Pass,
            )],
            // A path that cannot be read is not an answer with a caveat. The
            // question stands unanswered and the conclusion will say so.
            Err(error) => vec![finding(
                path.clone(),
                0,
                format!("{path} could not be read: {error}"),
                Status::Indeterminate,
            )],
        },
        CodingQuestion::Unsupported { .. } => vec![finding(
            String::new(),
            0,
            "this topology answers only `where does X occur` and `what is in \
             <path>`; it cannot answer this question"
                .to_owned(),
            Status::Indeterminate,
        )],
    };
    Ok(findings)
}

/// Validates each finding against the question subject schema.
#[system]
pub(super) async fn assess_findings(
    findings: Out<Vec<QuestionFinding>>,
) -> Result<Vec<Assessment<QuestionFinding>>, SystemError> {
    findings
        .iter()
        .cloned()
        .map(finding_assessment)
        .collect::<Result<Vec<_>, _>>()
        .map_err(exec)
}

/// Terminates the answer in the one coding contract.
///
/// The stop reason is *derived* from the ledger, which is what makes the
/// three outcomes distinguishable downstream without a second vocabulary: an
/// unanswerable question escalates out, a truncated scan exhausts its budget,
/// and an answer met its goal.
#[system]
pub(super) async fn answer_question(
    workspace: Res<CodingWorkspace>,
    findings: Out<Vec<Assessment<QuestionFinding>>>,
) -> Result<AssessmentConclusion, SystemError> {
    workspace.trace("question.answer");
    let stop_reason = if findings
        .iter()
        .any(|assessment| assessment.status() == Status::Indeterminate)
    {
        StopReason::EscalatedOut
    } else if findings
        .iter()
        .any(|assessment| assessment.status() == Status::Issue)
    {
        StopReason::BudgetExhausted
    } else {
        StopReason::GoalMet
    };
    conclude_findings(findings.clone(), Vec::new(), stop_reason).map_err(exec)
}

/// Builds the answer-shaped coding topology.
///
/// Three stages, no loop, and no write: survey the tree, assess what was
/// found, conclude. It is registered with
/// [`CandidateRequirements::none`](polaris_cosmos::topology::CandidateRequirements::none)
/// because it genuinely needs nothing beyond a readable working directory,
/// which is why it is the one registration in this pack that is admissible on
/// a bare developer machine.
#[must_use]
pub fn tree_question_graph() -> Graph {
    let mut graph = Graph::new();
    graph.add_system(survey_tree);
    graph.add_system(assess_findings);
    graph.add_system(answer_question);
    graph
}
