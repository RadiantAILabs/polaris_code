//! `SymPy`'s experience domain variant: the `experience` arm's own plan and
//! notes.
//!
//! [`NOTES`] carries sympy-specific environment facts, a way to verify a fix
//! in place against the real tests, and a caution that sympy's printing and
//! parsing code is often tested by exact string comparison. All of it targets
//! the model's own instruction text.
//!
//! [`plan`] is left as the unmodified statement-only shape with [`NOTES`];
//! no phase (locate, reproduce, fix, cover, verify) needs a different
//! structure for these tasks, and `has_stated_edits` is false for them, so
//! there is no worklist-sweep or multi-target phase.
//!
//! See `cosmos/domains/README.md` for how to edit and test this variant.

use polaris_cosmos::contracts::RevisionAttempt;

use crate::cosmos::planner::{self, PlannerFacts, StatementPlanBody};

/// Domain notes injected into every statement-only model step's instruction
/// for this variant's own `experience` plan below.
pub const NOTES: &str = include_str!("notes.md");

/// This variant's plan for the statement-only edit shape: the unmodified
/// generic shape with [`NOTES`]. See this module's own doc comment for why
/// every phase is left unchanged.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    planner::statement_only_plan_body(facts, attempt, NOTES)
}

/// Extra gate blocks this variant contributes to the sealed coding block
/// catalogue. Empty: this variant adds instruction content ([`NOTES`]), not a
/// new block — see this module's own doc comment.
pub mod blocks {
    use polaris_cosmos::composition::{CompositionCatalogue, CompositionCatalogueError};
    use polaris_cosmos::contracts::CompositionBlockKey;

    /// This variant's own block keys, folded into the sealed catalogue's key
    /// listing test.
    #[must_use]
    pub fn keys() -> Vec<CompositionBlockKey> {
        Vec::new()
    }

    /// Registers this variant's own blocks into the sealed catalogue. A
    /// no-op: this variant adds no blocks.
    ///
    /// # Errors
    /// Returns [`CompositionCatalogueError::DuplicateBlock`] if a later
    /// addition here names a key already registered elsewhere.
    pub fn register_all(
        _catalogue: &mut CompositionCatalogue,
    ) -> Result<(), CompositionCatalogueError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use polaris_cosmos::composition::{CompositionLimits, materialize};

    use super::*;
    use crate::cosmos::domains::{Arm, Domain, DomainSelection, plan_for_selection};
    use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};

    fn facts() -> PlannerFacts {
        PlannerFacts {
            registration: "test".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available: false,
            reproduce_runner_available: false,
            model_provider_available: false,
            is_question: false,
            is_survey: false,
            has_stated_edits: false,
            statement: "add() subtracts instead of adding: fix the implementation".to_owned(),
            shell_available: true,
        }
    }

    fn attempt() -> RevisionAttempt {
        RevisionAttempt::new(1).expect("one is a valid attempt number")
    }

    #[test]
    fn notes_are_present_and_within_the_word_budget() {
        assert!(!NOTES.trim().is_empty(), "mined notes must not be empty");
        let words = NOTES.split_whitespace().count();
        assert!(
            words <= 1200,
            "notes.md must stay within 1200 words, got {words}"
        );
    }

    #[test]
    fn notes_name_no_task_id() {
        // Domain notes must generalise to the whole project, not one mined
        // issue: `sympy__sympy-` is the benchmark's task-id prefix.
        assert!(!NOTES.contains("sympy__sympy-"));
    }

    #[test]
    fn plan_matches_the_unmodified_statement_only_shape_with_notes() {
        let expected = planner::statement_only_plan_body(&facts(), attempt(), NOTES)
            .expect("the generic shape with these notes builds");
        let (actual_expr, actual_extra) =
            plan(&facts(), attempt()).expect("this variant's plan builds");
        assert_eq!(
            actual_expr, expected.0,
            "this variant's gate expression must stay the unmodified statement-only shape"
        );
        assert_eq!(
            actual_extra.len(),
            expected.1.len(),
            "this variant must add no extra model-step configurations"
        );
    }

    #[test]
    fn contributes_no_extra_blocks() {
        assert!(blocks::keys().is_empty());
    }

    #[test]
    fn plan_materializes_under_conservative_limits() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = crate::cosmos::topology::contract()
            .expect("the coding contract registers cleanly")
            .key()
            .clone();
        let selection = DomainSelection {
            domain: Domain::Sympy,
            arm: Arm::Experience,
        };
        let plan = plan_for_selection(selection, &facts(), attempt(), &catalogue, &target)
            .expect("this variant's plan builds through plan_for_selection");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("this variant's plan materialises under conservative limits");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }
}
