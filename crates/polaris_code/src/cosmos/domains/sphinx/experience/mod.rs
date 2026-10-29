//! Sphinx's experience domain variant: the `experience` arm's own plan and
//! notes.
//!
//! [`plan`] keeps the unmodified statement-only shape
//! (`planner::statement_only_plan_body`) with [`NOTES`] threaded through.
//! The problems the notes address (dependency-install trial-and-error, a
//! self-check that stops one layer short of the real call site, a repair
//! round that fixes the wrong thing) are about how the model uses the tools it
//! already has (`search`, `read_file`, `run_python`), not a gap in the shape
//! itself. A new gate block would not help either: a model step's instruction is decoded once
//! from the plan's own configuration table before the graph runs
//! (`gate_blocks::mod`'s "How configuration reaches a block"), so nothing a
//! new block computes at run time has any wiring back into what the model
//! is told. [`NOTES`] is therefore this variant's whole contribution; see
//! `cosmos/domains/README.md` for how to edit and test it.

use polaris_cosmos::contracts::RevisionAttempt;

use crate::cosmos::planner::{self, PlannerFacts, StatementPlanBody};

/// Domain notes injected into every statement-only model step's instruction
/// for this variant's own `experience` plan below.
pub const NOTES: &str = include_str!("notes.md");

/// This variant's plan for the statement-only edit shape: the unmodified
/// generic shape with [`NOTES`]. See this module's own doc comment for why
/// it uses no structural change.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    planner::statement_only_plan_body(facts, attempt, NOTES)
}

/// Extra gate blocks this variant contributes to the sealed coding block
/// catalogue. Empty because no block would have a route back into a model
/// step's instruction (see this module's own doc comment).
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
    /// no-op today.
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
    use polaris_cosmos::composition::CompositionLimits;
    use polaris_cosmos::evidence::gate_plan_record;

    use super::*;
    use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};

    fn target() -> polaris_cosmos::topology::TopologyContractKey {
        crate::cosmos::topology::contract()
            .expect("the coding contract registers cleanly")
            .key()
            .clone()
    }

    /// The shape a Sphinx task typically runs under: no test runner, but a
    /// usable shell policy.
    fn statement_only_facts() -> PlannerFacts {
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
            statement: "autodoc does not document a stacked classmethod property".to_owned(),
            shell_available: true,
        }
    }

    fn attempt() -> RevisionAttempt {
        RevisionAttempt::new(1).expect("one is a valid attempt number")
    }

    #[test]
    fn notes_are_populated_and_within_the_1200_word_budget() {
        assert!(!NOTES.trim().is_empty(), "notes.md must no longer be empty");
        let words = NOTES.split_whitespace().count();
        assert!(
            words <= 1200,
            "domain notes must stay at most 1,200 words, got {words}"
        );
    }

    #[test]
    fn plan_materialises_under_conservative_limits() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target();
        let facts = statement_only_facts();

        let gate_plan =
            planner::scripted_plan_generic(&facts, attempt(), &catalogue, &target, plan)
                .expect("the sphinx experience plan builds");

        let graph = polaris_cosmos::composition::materialize(
            &gate_plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the sphinx experience plan materialises under conservative limits");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    /// Structure is unchanged from the generic arm: only the notes threaded
    /// through the model step's configuration differ, never the gate tree
    /// itself. This is the direct, checkable form of the module's structural
    /// conclusion, not an assertion of intent.
    #[test]
    fn plan_shape_is_identical_to_the_generic_arm() {
        let catalogue = coding_block_catalogue();
        let target = target();
        let facts = statement_only_facts();

        let generic = planner::scripted_plan(&facts, attempt(), &catalogue, &target)
            .expect("the generic plan builds");
        let mine = planner::scripted_plan_generic(&facts, attempt(), &catalogue, &target, plan)
            .expect("the sphinx experience plan builds");

        assert_eq!(
            gate_plan_record(&generic).shape(),
            gate_plan_record(&mine).shape(),
            "the sphinx experience plan must keep the generic arm's gate-expression shape"
        );
    }

    /// The plan's full digest still differs from the generic arm's, since
    /// non-empty notes reach the model step's own configuration — this is
    /// what proves `NOTES` is actually threaded through, not silently
    /// dropped.
    #[test]
    fn plan_digest_differs_from_the_generic_arm() {
        let catalogue = coding_block_catalogue();
        let target = target();
        let facts = statement_only_facts();

        let generic = planner::scripted_plan(&facts, attempt(), &catalogue, &target)
            .expect("the generic plan builds");
        let mine = planner::scripted_plan_generic(&facts, attempt(), &catalogue, &target, plan)
            .expect("the sphinx experience plan builds");

        assert_ne!(
            gate_plan_record(&generic).plan_digest().to_string(),
            gate_plan_record(&mine).plan_digest().to_string(),
            "non-empty sphinx notes must change the plan digest"
        );
    }

    #[test]
    fn blocks_module_registers_nothing() {
        assert!(
            blocks::keys().is_empty(),
            "no gate block is justified by the mined evidence"
        );
        let mut catalogue = polaris_cosmos::composition::CompositionCatalogue::new(
            polaris_cosmos::contracts::CompositionCatalogId::new("test-catalogue")
                .expect("valid catalogue id"),
            polaris::system::plugin::Version::new(1, 0, 0),
        );
        blocks::register_all(&mut catalogue).expect("registering nothing never fails");
    }
}
