//! Projection of coding verdicts onto the Cosmos assessment vocabulary.
//!
//! Every coding topology terminates in an [`AssessmentConclusion`]. This
//! module owns that projection: the subject schemas, the per-subject
//! assessment, the combination policy, and the residual accounting that keeps
//! unfinished work *visible* instead of silently dropped.
//!
//! There are two subjects, not one. The edit- and audit-shaped topologies
//! conclude over an [`EditSite`] — a place something should be written. The
//! answer-shaped topology concludes over a [`QuestionFinding`] — something
//! established about the tree by reading it. Both go through the same
//! [`summarize`]/[`conclude`] pair, which is generic in the subject, so
//! "answering needed no edits" is sayable in the ordinary contract without
//! widening what an edit site means.
//!
//! Two properties are load-bearing and are asserted in
//! `tests/cosmos_contracts_proof.rs`:
//!
//! - a site the sweep never reached is accounted
//!   [`ResidualDisposition::Deferred`], never omitted; and
//! - graph completion is mechanical and says nothing about success — task
//!   success comes only from the outcome observations in [`super::verify`].

use polaris_cosmos::cognition::ResolutionInput;
use polaris_cosmos::contracts::{
    Assessment, AssessmentAccounting, AssessmentConclusion, AssessmentSummary, ContentDigest,
    ContractError, EpistemicBasis, EvidenceKind, ResidualDisposition, SourceKind, Status,
    SubjectSchema, SubjectSchemaId, SubjectSchemaKey,
};

use serde::Serialize;

use super::payload::{EditSite, QuestionFinding};
use super::{behavior_version, evidenced, policy, provenance};

/// Stable identity of the coding subject schema.
pub const SUBJECT_SCHEMA: &str = "polaris-code.edit-site";

/// Policy under which per-site assessments are combined.
pub const COMBINE_POLICY: &str = "polaris-code.combine-site-assessments";

/// Policy under which residual sites are accounted.
pub const RESIDUAL_POLICY: &str = "polaris-code.account-residual-sites";

/// Returns the schema every coding assessment subject is validated against.
///
/// # Panics
///
/// Never: the identity and JSON schema are compile-time constants.
#[must_use]
pub fn subject_schema() -> SubjectSchema<EditSite> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new(SUBJECT_SCHEMA).expect("subject schema ID is a valid stable name"),
            behavior_version(),
        ),
        "One adjudicated coding site: a sandbox-relative path, an optional line, and a verdict.",
        r#"{"type":"object","required":["path","line","status"]}"#,
        |subject: &EditSite| {
            if subject.path.is_empty() {
                Err("a coding site must name a path".to_owned())
            } else {
                Ok(())
            }
        },
    )
    .expect("coding subject schema is valid")
}

/// Builds one evidence-bearing assessment of a coding site.
///
/// # Errors
///
/// Returns an error if the site fails schema validation.
pub fn site_assessment(site: EditSite) -> Result<Assessment<EditSite>, ContractError> {
    let status = site.status;
    Assessment::new(
        &subject_schema(),
        site,
        status,
        vec![evidenced(
            SourceKind::AutomatedVerifier,
            EpistemicBasis::Verified,
            EvidenceKind::ArtifactDiff,
            "assessment.site-verdict",
        )],
    )
}

/// Combines a complete ledger under the named coding combination policy.
///
/// Generic over the subject because this embodiment has two of them — an
/// [`EditSite`] and a [`QuestionFinding`] — and the *combination* rule is the
/// same for both. Only what counts as a subject differs; how a finished
/// ledger becomes one summary does not.
///
/// # Errors
///
/// Returns an error if the ledger is inconsistent with the summary contract.
pub fn summarize<T: Serialize>(
    input: &ResolutionInput<T>,
) -> Result<AssessmentSummary, ContractError> {
    AssessmentSummary::new(
        input.assessments(),
        policy(COMBINE_POLICY),
        ContentDigest::sha256(COMBINE_POLICY.as_bytes()),
        vec![provenance(
            SourceKind::CosmosPolicy,
            EpistemicBasis::ReDerived,
        )],
    )
}

/// Accounts every residual and emits the terminal conclusion.
///
/// A subject whose verdict is [`Status::Issue`] or [`Status::Indeterminate`]
/// is accounted rather than discarded: `Blocked` when the episode escalated
/// out, `Deferred` otherwise. That is what makes an unfinished coding run
/// legible instead of a silent partial success — and, for the answer shape,
/// it is what makes "I could not answer that" a *typed* terminal outcome
/// rather than an empty conclusion.
///
/// # Errors
///
/// Returns an error if an accounting record or the conclusion is rejected.
pub fn conclude<T: Serialize>(
    input: &ResolutionInput<T>,
    summary: AssessmentSummary,
) -> Result<AssessmentConclusion, ContractError> {
    let escalated = input.stop_reason() == polaris_cosmos::contracts::StopReason::EscalatedOut;
    let mut accounting = Vec::new();
    for item in input.assessments() {
        if !matches!(item.status(), Status::Issue | Status::Indeterminate) {
            continue;
        }
        accounting.push(AssessmentAccounting::new(
            item.content_digest(),
            if escalated {
                ResidualDisposition::Blocked
            } else {
                ResidualDisposition::Deferred
            },
            policy(RESIDUAL_POLICY),
            ContentDigest::sha256(RESIDUAL_POLICY.as_bytes()),
            // Cosmos requires an accounting entry to carry *typed decision
            // evidence*: deciding to defer or block a residual is a judgement,
            // and an unattributable judgement is rejected. Plain provenance is
            // not enough, which is why this is the one place in the module
            // that names the residual-treatment evidence object.
            vec![evidenced(
                SourceKind::CosmosPolicy,
                EpistemicBasis::ReDerived,
                EvidenceKind::ObservationNote,
                "assessment.residual-treatment",
            )],
        )?);
    }
    AssessmentConclusion::new(
        summary,
        accounting,
        input.stop_reason(),
        input.assessments(),
    )
}

/// Builds a conclusion from any finished coding ledger in one step.
///
/// # Errors
///
/// Returns an error if combination, accounting, or the conclusion is rejected.
pub fn conclude_ledger<T: Serialize>(
    assessed: Vec<Assessment<T>>,
    residual: Vec<T>,
    stop_reason: polaris_cosmos::contracts::StopReason,
) -> Result<AssessmentConclusion, ContractError> {
    let input = ResolutionInput::new(assessed, residual, stop_reason);
    let summary = summarize(&input)?;
    conclude(&input, summary)
}

/// Builds a conclusion from a finished site ledger in one step.
///
/// # Errors
///
/// Returns an error if combination, accounting, or the conclusion is rejected.
pub fn conclude_sites(
    assessed: Vec<Assessment<EditSite>>,
    unvisited: Vec<EditSite>,
    stop_reason: polaris_cosmos::contracts::StopReason,
) -> Result<AssessmentConclusion, ContractError> {
    conclude_ledger(assessed, unvisited, stop_reason)
}

// ─────────────────────────────────────────────────────────────────────────────
// The answer shape's subject
// ─────────────────────────────────────────────────────────────────────────────

/// Stable identity of the question-finding subject schema.
///
/// Deliberately *not* [`SUBJECT_SCHEMA`]. The alternative — letting a finding
/// be an [`EditSite`] with some conventional path — would make "an edit site"
/// mean "a place something might be written, or a place something was merely
/// read", and every edit-shaped topology's ledger would become ambiguous. A
/// second schema costs one constant and keeps both vocabularies honest.
pub const QUESTION_SUBJECT_SCHEMA: &str = "polaris-code.question-finding";

/// Returns the schema every question finding is validated against.
///
/// # Panics
///
/// Never: the identity and JSON schema are compile-time constants.
#[must_use]
pub fn question_subject_schema() -> SubjectSchema<QuestionFinding> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new(QUESTION_SUBJECT_SCHEMA)
                .expect("subject schema ID is a valid stable name"),
            behavior_version(),
        ),
        "One thing established about the working tree in answer to a question, \
         with the path and line it was established from.",
        r#"{"type":"object","required":["question","path","line","claim","status"]}"#,
        |subject: &QuestionFinding| {
            if subject.question.trim().is_empty() {
                Err("a question finding must name the question it answers".to_owned())
            } else if subject.claim.trim().is_empty() {
                Err("a question finding must state a claim".to_owned())
            } else {
                Ok(())
            }
        },
    )
    .expect("question subject schema is valid")
}

/// Builds one evidence-bearing assessment of a question finding.
///
/// The evidence is an [`EvidenceKind::ObservationNote`] the host
/// [`EpistemicBasis::Verified`] by reading the tree itself: a finding is
/// something this host looked at, which is a different claim from the
/// artifact diff an edit site carries and is recorded as one.
///
/// # Errors
///
/// Returns an error if the finding fails schema validation.
pub fn finding_assessment(
    finding: QuestionFinding,
) -> Result<Assessment<QuestionFinding>, ContractError> {
    let status = finding.status;
    Assessment::new(
        &question_subject_schema(),
        finding,
        status,
        vec![evidenced(
            SourceKind::AutomatedVerifier,
            EpistemicBasis::Verified,
            EvidenceKind::ObservationNote,
            "assessment.question-finding",
        )],
    )
}

/// Builds a conclusion from a finished question ledger in one step.
///
/// # Errors
///
/// Returns an error if combination, accounting, or the conclusion is rejected.
pub fn conclude_findings(
    assessed: Vec<Assessment<QuestionFinding>>,
    unanswered: Vec<QuestionFinding>,
    stop_reason: polaris_cosmos::contracts::StopReason,
) -> Result<AssessmentConclusion, ContractError> {
    conclude_ledger(assessed, unanswered, stop_reason)
}
