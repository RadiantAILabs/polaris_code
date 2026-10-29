//! Fact and port vocabulary for the coding gate-block catalogue's
//! [`BlockDescription`]s, plus the one query a gate planner needs over them:
//! which registered blocks are admissible given what is actually true about
//! the run it is planning for.
//!
//! Every function here returns a stable identity, not a value that varies by
//! call site — the fact and port keys name the same closed vocabulary every
//! `describe_block` call in [`super::gate_blocks`] draws from, so a planner
//! reading [`admissible_gate_blocks`]'s result and a registration describing
//! a block agree on what each key means without either side spelling out a
//! string literal twice.

use polaris_cosmos::composition::{
    BlockDescription, CompositionCatalogueSnapshot, FactKey, StablePortId, StablePortKey,
};
use polaris_cosmos::contracts::CompositionBlockKey;

use super::behavior_version;
use super::planner::PlannerFacts;

fn fact(id: &str) -> FactKey {
    FactKey::new(id).expect("fact key is a valid stable name")
}

fn port(id: &str) -> StablePortKey {
    StablePortKey::new(
        StablePortId::new(id).expect("port ID is a valid stable name"),
        behavior_version(),
    )
}

// ---------------------------------------------------------------------------
// Fact vocabulary — matches `PlannerFacts`'s own boolean fields one for one,
// so the planner can map a fact it holds onto the key a description
// requires without a second table.
// ---------------------------------------------------------------------------

/// The caller stated at least one exact edit. Matches
/// [`PlannerFacts::has_stated_edits`].
#[must_use]
pub fn stated_edits_present() -> FactKey {
    fact("code.stated-edits-present")
}

/// The host declared a test runner available. Matches
/// [`PlannerFacts::test_runner_available`].
#[must_use]
pub fn test_runner_available() -> FactKey {
    fact("code.test-runner-available")
}

/// The host declared a model provider available. Matches
/// [`PlannerFacts::model_provider_available`].
#[must_use]
pub fn model_provider_available() -> FactKey {
    fact("code.model-provider-available")
}

/// The task is a read-only question. Matches [`PlannerFacts::is_question`].
#[must_use]
pub fn question_task() -> FactKey {
    fact("code.question-task")
}

/// The task is a read-only survey. Matches [`PlannerFacts::is_survey`].
///
/// Declared for parity with `PlannerFacts`, though no block registered in
/// [`super::gate_blocks`] requires it today: `code.survey-tree` (the one
/// survey-shaped existing block) is gated on [`question_task`] instead, since
/// it answers a closed-vocabulary question rather than scanning for a marker.
#[must_use]
pub fn survey_task() -> FactKey {
    fact("code.survey-task")
}

/// The session's shell policy would allow the reproduce script's own run
/// command. Matches [`PlannerFacts::reproduce_runner_available`].
#[must_use]
pub fn reproduce_runner_available() -> FactKey {
    fact("code.reproduce-runner-available")
}

/// A shell policy on this host would let a bounded model step run its own
/// `python` scripts. Matches [`PlannerFacts::shell_available`].
#[must_use]
pub fn shell_available() -> FactKey {
    fact("code.shell-available")
}

// ---------------------------------------------------------------------------
// Port vocabulary — one canonical `StablePortKey` per distinct payload type a
// registered block produces, reused across every block that produces or
// reads that same type. Only a block whose `wire_input` actually checks a
// port (today, only `code.apply-patch-intent`) names one in `inputs()`; every
// other upstream `Out<T>` read is ordinary sequential placement, which the
// materializer wires by type without consulting `wire_input` at all.
// ---------------------------------------------------------------------------

/// Port key for [`super::payload::AppliedEdit`].
#[must_use]
pub fn applied_edit_port() -> StablePortKey {
    port("code.applied-edit")
}

/// Port key for `Assessment<EditSite>`.
#[must_use]
pub fn edit_site_assessment_port() -> StablePortKey {
    port("code.edit-site-assessment")
}

/// Port key for `AssessmentConclusion`.
#[must_use]
pub fn assessment_conclusion_port() -> StablePortKey {
    port("code.assessment-conclusion")
}

/// Port key for `EditFeedback`.
#[must_use]
pub fn edit_feedback_port() -> StablePortKey {
    port("code.edit-feedback")
}

/// Port key for `Vec<QuestionFinding>`.
#[must_use]
pub fn question_findings_port() -> StablePortKey {
    port("code.question-findings")
}

/// Port key for `Vec<Assessment<QuestionFinding>>`.
#[must_use]
pub fn assessed_question_findings_port() -> StablePortKey {
    port("code.assessed-question-findings")
}

/// Port key for `ModelStepArtifact`, `code.model-propose`'s output when it is
/// not configured for [`ModelStepOutputKind::PatchIntent`](super::gate_blocks::configurable::ModelStepOutputKind::PatchIntent).
/// A `patch-intent`-configured instance produces
/// [`patch_intent_port_key`](super::gate_blocks::configurable::patch_intent_port_key)'s
/// type instead — the same one `code.apply-patch-intent` wires — since one
/// registration's `BlockDescription` names a single canonical output and this
/// is the more general of the two.
#[must_use]
pub fn model_step_artifact_port() -> StablePortKey {
    port("code.model-step-artifact")
}

/// Port key for `ProposedPatchIntent`, produced by
/// `code.model-propose-conversation` or `code.resume-model-propose` — the
/// conversation-keeping counterparts of a plain `code.model-propose`, which
/// pair a proposed patch with the conversation that produced it so a repair
/// round can resume it instead of starting a fresh one.
/// `code.extract-patch-from-proposal` projects this down to
/// [`super::gate_blocks::configurable::patch_intent_port_key`]'s plain
/// `PatchIntent` type for the blocks (apply, judge) that only need the patch.
#[must_use]
pub fn proposed_patch_intent_port() -> StablePortKey {
    port("code.proposed-patch-intent")
}

/// Port key for `CreatedFile`.
#[must_use]
pub fn created_file_port() -> StablePortKey {
    port("code.created-file")
}

/// Port key for `Vec<PathSurveyFinding>`.
#[must_use]
pub fn path_survey_findings_port() -> StablePortKey {
    port("code.path-survey-findings")
}

/// Port key for `Vec<EditSite>` as produced by a marker scan.
#[must_use]
pub fn marker_scan_sites_port() -> StablePortKey {
    port("code.marker-scan-sites")
}

/// Port key for `Vec<Assessment<EditSite>>` as produced by a marker-site
/// assessment.
#[must_use]
pub fn site_audit_assessments_port() -> StablePortKey {
    port("code.site-audit-assessments")
}

/// Port key for `TaskDisposition`.
#[must_use]
pub fn task_disposition_port() -> StablePortKey {
    port("code.task-disposition")
}

/// Port key for `TestRunOutcome`.
#[must_use]
pub fn test_run_outcome_port() -> StablePortKey {
    port("code.test-run-outcome")
}

/// Port key for `WroteReproduceScript`.
#[must_use]
pub fn wrote_reproduce_script_port() -> StablePortKey {
    port("code.wrote-reproduce-script")
}

/// Returns every catalogue-registered block whose description is admissible
/// given `facts`, in the catalogue's own registration order.
///
/// A block with no description never appears: [`admissible_blocks`] only
/// considers described blocks, and a block's own required facts (declared at
/// `describe_block` time) gate it independently of whatever else `facts`
/// says. Cosmos never sees `facts`' fields directly — this is the one place
/// that translates them into the opaque [`FactKey`]s a description names.
///
/// [`admissible_blocks`]: CompositionCatalogueSnapshot::admissible_blocks
#[must_use]
pub fn admissible_gate_blocks<'a>(
    catalogue: &'a CompositionCatalogueSnapshot,
    facts: &PlannerFacts,
) -> Vec<(&'a CompositionBlockKey, &'a BlockDescription)> {
    catalogue.admissible_blocks(|required| {
        (*required == stated_edits_present() && facts.has_stated_edits)
            || (*required == test_runner_available() && facts.test_runner_available)
            || (*required == reproduce_runner_available() && facts.reproduce_runner_available)
            || (*required == shell_available() && facts.shell_available)
            || (*required == model_provider_available() && facts.model_provider_available)
            || (*required == question_task() && facts.is_question)
            || (*required == survey_task() && facts.is_survey)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosmos::gate_blocks::{
        clarification, configurable, existing, repair, reproduce, survey,
    };

    fn facts(
        has_stated_edits: bool,
        test_runner_available: bool,
        model_provider_available: bool,
        is_question: bool,
        is_survey: bool,
    ) -> PlannerFacts {
        PlannerFacts {
            registration: "polaris-code.coding.direct-edit".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available,
            reproduce_runner_available: false,
            model_provider_available,
            shell_available: false,
            is_question,
            is_survey,
            has_stated_edits,
            statement: "fix the bug".to_owned(),
        }
    }

    #[test]
    fn every_registered_gate_block_has_a_description() {
        let catalogue = super::super::gate_blocks::coding_block_catalogue();
        for key in existing::keys()
            .into_iter()
            .chain(configurable::keys())
            .chain(survey::keys())
            .chain(clarification::keys())
            .chain(repair::keys())
            .chain(reproduce::keys())
        {
            assert!(
                catalogue.block_description(&key).is_some(),
                "{key} is registered without a description"
            );
        }
    }

    #[test]
    fn with_no_stated_edits_the_stated_edit_blocks_are_inadmissible() {
        let catalogue = super::super::gate_blocks::coding_block_catalogue();
        let facts = facts(false, true, true, false, false);
        let admissible: Vec<CompositionBlockKey> = admissible_gate_blocks(&catalogue, &facts)
            .into_iter()
            .map(|(key, _)| key.clone())
            .collect();
        for key in [
            existing::apply_direct_key(),
            existing::judge_stated_edits_key(),
            existing::predict_edit_loop_key(),
            existing::conclude_prediction_key(),
        ] {
            assert!(
                !admissible.contains(&key),
                "{key} must not be admissible with no stated edits"
            );
        }
    }

    #[test]
    fn with_stated_edits_the_stated_edit_blocks_are_admissible() {
        let catalogue = super::super::gate_blocks::coding_block_catalogue();
        let facts = facts(true, false, false, false, false);
        let admissible: Vec<CompositionBlockKey> = admissible_gate_blocks(&catalogue, &facts)
            .into_iter()
            .map(|(key, _)| key.clone())
            .collect();
        for key in [
            existing::apply_direct_key(),
            existing::judge_stated_edits_key(),
            existing::predict_edit_loop_key(),
            existing::conclude_prediction_key(),
        ] {
            assert!(admissible.contains(&key), "{key} must be admissible");
        }
    }

    #[test]
    fn without_a_reproduce_runner_the_reproduce_block_is_inadmissible() {
        let catalogue = super::super::gate_blocks::coding_block_catalogue();
        let facts = facts(false, false, true, false, false);
        let admissible: Vec<CompositionBlockKey> = admissible_gate_blocks(&catalogue, &facts)
            .into_iter()
            .map(|(key, _)| key.clone())
            .collect();
        assert!(
            !admissible.contains(&reproduce::write_reproduce_script_key()),
            "write-reproduce-script must not be admissible with no reproduce runner"
        );
    }

    #[test]
    fn with_a_reproduce_runner_the_reproduce_block_is_admissible() {
        let catalogue = super::super::gate_blocks::coding_block_catalogue();
        let facts = PlannerFacts {
            reproduce_runner_available: true,
            ..facts(false, false, true, false, false)
        };
        let admissible: Vec<CompositionBlockKey> = admissible_gate_blocks(&catalogue, &facts)
            .into_iter()
            .map(|(key, _)| key.clone())
            .collect();
        assert!(
            admissible.contains(&reproduce::write_reproduce_script_key()),
            "write-reproduce-script must be admissible with a reproduce runner available"
        );
    }

    #[test]
    fn without_shell_available_the_self_check_blocks_are_inadmissible() {
        let catalogue = super::super::gate_blocks::coding_block_catalogue();
        let facts = facts(false, false, true, false, false);
        let admissible: Vec<CompositionBlockKey> = admissible_gate_blocks(&catalogue, &facts)
            .into_iter()
            .map(|(key, _)| key.clone())
            .collect();
        for key in [
            repair::model_propose_conversation_key(),
            repair::resume_model_propose_key(),
            repair::run_check_script_key(),
        ] {
            assert!(
                !admissible.contains(&key),
                "{key} must not be admissible with no shell available"
            );
        }
    }

    #[test]
    fn with_shell_available_the_self_check_blocks_are_admissible() {
        let catalogue = super::super::gate_blocks::coding_block_catalogue();
        let facts = PlannerFacts {
            shell_available: true,
            ..facts(false, false, true, false, false)
        };
        let admissible: Vec<CompositionBlockKey> = admissible_gate_blocks(&catalogue, &facts)
            .into_iter()
            .map(|(key, _)| key.clone())
            .collect();
        for key in [
            repair::model_propose_conversation_key(),
            repair::resume_model_propose_key(),
            repair::run_check_script_key(),
        ] {
            assert!(admissible.contains(&key), "{key} must be admissible");
        }
    }
}
