//! Plain Rust predicates and one discriminator for `Choose`/`Switch` gates.
//!
//! None of these are Polaris systems: a `Choose`/`Switch` gate's predicate or
//! discriminator is a native closure over an already-produced typed value
//! (`polaris_graph::Graph::add_conditional_branch`/`add_switch`), not a graph
//! node in its own right. This module registers each one as a native
//! `BoxedPredicate`/`BoxedDiscriminator` into the composition catalogue,
//! keyed the same way a block is.

use std::collections::BTreeSet;

use polaris::graph::predicate::{BoxedDiscriminator, BoxedPredicate};
use polaris_cosmos::composition::{
    CaseKey, CompositionCatalogue, CompositionCatalogueError, DiscriminatorId, DiscriminatorKey,
    PredicateId, PredicateKey,
};
use polaris_cosmos::contracts::{Assessment, Status};

use super::super::payload::{CheckFailureClass, EditSite, SweepProgress, TestRunOutcome};
use super::super::task::{CodingTask, SuccessCheck};
use super::behavior_version;
use super::cascade::CascadeCheck;
use super::clarification::TaskDisposition;
use super::configurable::{ModelStepArtifact, ProposedPatchIntent};

fn predicate_key(id: &str) -> PredicateKey {
    PredicateKey::new(
        PredicateId::new(id).expect("predicate ID is a valid stable name"),
        behavior_version(),
    )
}

fn discriminator_key(id: &str) -> DiscriminatorKey {
    DiscriminatorKey::new(
        DiscriminatorId::new(id).expect("discriminator ID is a valid stable name"),
        behavior_version(),
    )
}

/// Whether a judged edit site passed.
///
/// The first of the `tests passed` / `review approved` / `budget left`
/// predicate family a `Choose`/`Switch` gate can name.
#[must_use]
pub fn tests_passed(assessment: &Assessment<EditSite>) -> bool {
    assessment.status() == Status::Pass
}

/// Whether a model step's review artifact approved what it reviewed.
///
/// The second predicate in the family. Any other artifact shape (the model
/// step was configured for a different output kind) is treated as not
/// approved, never as a panic: a predicate over the wrong artifact type is a
/// materializer wiring mistake, not a runtime crash.
#[must_use]
pub fn review_approved(artifact: &ModelStepArtifact) -> bool {
    matches!(
        artifact,
        ModelStepArtifact::Review {
            verdict: Status::Pass,
            ..
        }
    )
}

/// Whether a bounded worklist sweep has iteration budget left.
///
/// The third predicate in the family, over the same
/// [`SweepProgress`](super::super::payload::SweepProgress) the existing
/// sweep already tracks.
#[must_use]
pub fn budget_left(progress: &super::super::payload::SweepProgress) -> bool {
    progress.budget_left > 0
}

/// Whether a proposed patch's own answer included a self-check script.
///
/// The fourth predicate in the family, over
/// [`ProposedPatchIntent`](super::configurable::ProposedPatchIntent) — gates
/// whether a statement-only plan takes the self-check branch (run the
/// model's own reproduce/verify script and, on failure, resume the same
/// conversation for a repair) or falls back to the plain stated-edit judge.
#[must_use]
pub fn has_check(proposal: &ProposedPatchIntent) -> bool {
    proposal
        .patch
        .check
        .as_ref()
        .is_some_and(|check| !check.trim().is_empty())
}

/// Whether an optional self-check failed because of observed behavior.
#[must_use]
pub fn check_needs_repair(outcome: &TestRunOutcome) -> bool {
    outcome.check_failure == Some(CheckFailureClass::Behavior)
}

/// The closed set of coding task shapes a `Switch` gate can route on.
///
/// Mirrors [`CodingIntent::Answer`](super::super::task::CodingIntent)'s own
/// distinction between "answer a question" and "change the tree", refined by
/// [`SuccessCheck`] for the edit-shaped tasks: a task with a
/// [`SuccessCheck::Command`] needs [`super::existing::RUN_PROJECT_TESTS`]
/// rather than [`super::existing::JUDGE_STATED_EDITS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKindCase {
    /// The task asks a bounded question about the tree and changes nothing.
    Question,
    /// The task's success is judged by running a program.
    TestGated,
    /// The task's success is the stated edits landing (optionally plus a
    /// marker scan), or a verdict deferred to an external verifier.
    StatedEdit,
}

impl TaskKindCase {
    const fn case_key_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::TestGated => "test-gated",
            Self::StatedEdit => "stated-edit",
        }
    }
}

/// Discriminates one coding task into the case a `Switch` gate routes on.
#[must_use]
pub fn task_kind(task: &CodingTask) -> TaskKindCase {
    if task.question().is_answerable() {
        return TaskKindCase::Question;
    }
    match task.success() {
        SuccessCheck::Command(_) => TaskKindCase::TestGated,
        SuccessCheck::StatedEdits { .. } | SuccessCheck::Delegated { .. } => {
            TaskKindCase::StatedEdit
        }
    }
}

/// Returns [`tests_passed`]'s registered key.
#[must_use]
pub fn tests_passed_key() -> PredicateKey {
    predicate_key("code.tests-passed")
}

/// Returns [`review_approved`]'s registered key.
#[must_use]
pub fn review_approved_key() -> PredicateKey {
    predicate_key("code.review-approved")
}

/// Returns [`budget_left`]'s registered key.
#[must_use]
pub fn budget_left_key() -> PredicateKey {
    predicate_key("code.budget-left")
}

/// Returns [`has_check`]'s registered key.
#[must_use]
pub fn has_check_key() -> PredicateKey {
    predicate_key("code.has-check")
}

/// Returns the self-check repair predicate's registered key.
#[must_use]
pub fn check_needs_repair_key() -> PredicateKey {
    predicate_key("code.check-needs-repair")
}

/// Whether every deterministic cascade check passed.
#[must_use]
pub fn cascade_passed(check: &CascadeCheck) -> bool {
    check.passed
}

/// Registered cascade predicate key.
#[must_use]
pub fn cascade_passed_key() -> PredicateKey {
    predicate_key("code.cascade-passed")
}

/// Returns [`task_kind`]'s registered discriminator key.
#[must_use]
pub fn task_kind_key() -> DiscriminatorKey {
    discriminator_key("code.task-kind")
}

/// Returns [`task_disposition`]'s registered discriminator key.
#[must_use]
pub fn task_disposition_key() -> DiscriminatorKey {
    discriminator_key("code.task-disposition")
}

/// Discriminates a classifier output into the corresponding `Switch` case.
#[must_use]
pub fn task_disposition(disposition: &TaskDisposition) -> &'static str {
    disposition.case_key()
}

/// Returns the closed set of cases [`task_disposition`] can route to.
#[must_use]
pub fn task_disposition_cases() -> BTreeSet<CaseKey> {
    [
        TaskDisposition::Edit,
        TaskDisposition::Answer,
        TaskDisposition::Clarify,
    ]
    .into_iter()
    .map(|disposition| {
        CaseKey::new(disposition.case_key()).expect("disposition labels are valid stable names")
    })
    .collect()
}

/// Returns the closed set of case keys [`task_kind`] can route to.
///
/// # Panics
///
/// Never: every case label is a compile-time constant valid stable name.
#[must_use]
pub fn task_kind_cases() -> BTreeSet<CaseKey> {
    [
        TaskKindCase::Question.case_key_str(),
        TaskKindCase::TestGated.case_key_str(),
        TaskKindCase::StatedEdit.case_key_str(),
    ]
    .into_iter()
    .map(|case| CaseKey::new(case).expect("case label is a valid stable name"))
    .collect()
}

/// Registers every predicate and discriminator in this module into
/// `catalogue`.
///
/// # Errors
///
/// Returns [`CompositionCatalogueError::DuplicatePredicate`] or
/// [`CompositionCatalogueError::DuplicateDiscriminator`] if a key here
/// collides with one already registered.
pub(super) fn register_all(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    catalogue.register_predicate(
        tests_passed_key(),
        BoxedPredicate::new::<Assessment<EditSite>>(tests_passed),
    )?;
    catalogue.register_predicate(
        review_approved_key(),
        BoxedPredicate::new::<ModelStepArtifact>(review_approved),
    )?;
    catalogue.register_predicate(
        budget_left_key(),
        BoxedPredicate::new::<SweepProgress>(budget_left),
    )?;
    catalogue.register_predicate(
        has_check_key(),
        BoxedPredicate::new::<ProposedPatchIntent>(has_check),
    )?;
    catalogue.register_predicate(
        check_needs_repair_key(),
        BoxedPredicate::new::<TestRunOutcome>(check_needs_repair),
    )?;
    catalogue.register_discriminator(
        task_kind_key(),
        BoxedDiscriminator::new::<CodingTask, _>(|task| task_kind(task).case_key_str()),
        task_kind_cases(),
    )?;
    catalogue.register_discriminator(
        task_disposition_key(),
        BoxedDiscriminator::new::<TaskDisposition, _>(task_disposition),
        task_disposition_cases(),
    )?;
    Ok(())
}

/// Adds the lean-only system-check predicate to the lean catalogue.
pub(super) fn register_cascade(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    catalogue.register_predicate(
        cascade_passed_key(),
        BoxedPredicate::new::<CascadeCheck>(cascade_passed),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosmos::task::{CodingIntent, CodingTask};

    fn assessment(status: Status) -> Assessment<EditSite> {
        crate::cosmos::assess::site_assessment(EditSite::new("src/lib.rs".to_owned(), 0, status))
            .expect("a well-formed site assessment")
    }

    #[test]
    fn tests_passed_reads_the_judged_status() {
        assert!(tests_passed(&assessment(Status::Pass)));
        assert!(!tests_passed(&assessment(Status::Issue)));
    }

    #[test]
    fn review_approved_requires_a_passing_review_artifact() {
        assert!(review_approved(&ModelStepArtifact::Review {
            verdict: Status::Pass,
            notes: "looks good".to_owned(),
        }));
        assert!(!review_approved(&ModelStepArtifact::Review {
            verdict: Status::Issue,
            notes: "needs another pass".to_owned(),
        }));
        assert!(!review_approved(&ModelStepArtifact::Answer(
            "not a review".to_owned()
        )));
    }

    #[test]
    fn budget_left_reads_the_remaining_count() {
        use crate::cosmos::payload::SweepProgress;
        assert!(budget_left(&SweepProgress {
            remaining: 1,
            visited: 0,
            budget_left: 3,
        }));
        assert!(!budget_left(&SweepProgress {
            remaining: 1,
            visited: 0,
            budget_left: 0,
        }));
    }

    #[test]
    fn has_check_reads_whether_the_proposal_included_a_script() {
        use super::super::configurable::PatchIntent;

        let with_check = ProposedPatchIntent {
            patch: PatchIntent {
                edits: Vec::new(),
                check: Some("print('ok')".to_owned()),
            },
            conversation: Default::default(),
        };
        let without_check = ProposedPatchIntent {
            patch: PatchIntent {
                edits: Vec::new(),
                check: None,
            },
            conversation: Default::default(),
        };
        assert!(has_check(&with_check));
        assert!(!has_check(&without_check));
        let empty_check = ProposedPatchIntent {
            patch: PatchIntent {
                edits: Vec::new(),
                check: Some(" \n\t ".to_owned()),
            },
            conversation: Default::default(),
        };
        assert!(!has_check(&empty_check));
    }

    #[test]
    fn only_behavior_check_failures_request_repair() {
        let mut outcome = TestRunOutcome {
            status: Status::Issue,
            output: String::new(),
            check_failure: Some(CheckFailureClass::Infrastructure),
        };
        assert!(!check_needs_repair(&outcome));
        outcome.check_failure = Some(CheckFailureClass::Behavior);
        assert!(check_needs_repair(&outcome));
        outcome.status = Status::Pass;
        outcome.check_failure = None;
        assert!(!check_needs_repair(&outcome));
    }

    #[test]
    fn task_kind_discriminates_question_test_gated_and_stated_edit() {
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");

        let stated_edit = CodingTask::new(
            "kind-stated-edit",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed stated-edit task");
        assert_eq!(task_kind(&stated_edit), TaskKindCase::StatedEdit);

        let test_gated = CodingTask::from_statement(
            "kind-test-gated",
            dir.path(),
            "make the tests pass",
            CodingIntent::Careful,
            SuccessCheck::command("true", Vec::<String>::new())
                .expect("well-formed success command"),
        )
        .expect("well-formed test-gated task");
        assert_eq!(task_kind(&test_gated), TaskKindCase::TestGated);

        let question = CodingTask::from_statement(
            "kind-question",
            dir.path(),
            "where does todo!() occur",
            CodingIntent::Answer,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed question task")
        .with_question("where is todo!()");
        assert_eq!(task_kind(&question), TaskKindCase::Question);
    }
}
