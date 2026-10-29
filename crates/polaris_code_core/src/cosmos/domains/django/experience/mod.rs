//! Django's `experience` domain variant: the arm's own plan and notes.
//!
//! [`plan`] keeps the generic statement-only shape with this variant's own
//! [`NOTES`]; no extra gate block or phase is used.

use polaris_cosmos::contracts::RevisionAttempt;

use crate::cosmos::planner::{self, PlannerFacts, StatementPlanBody};

/// Domain notes injected into every statement-only model step's instruction
/// for this variant's own `experience` plan below.
pub const NOTES: &str = include_str!("notes.md");

/// This variant's statement-only edit plan keeps the generic bounded
/// propose/apply/check/repair shape and adds only [`NOTES`].
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    planner::statement_only_plan_body(facts, attempt, NOTES)
}

/// Extra gate blocks this variant contributes to the sealed coding block
/// catalogue. This variant uses none.
pub mod blocks {
    use polaris_cosmos::composition::{CompositionCatalogue, CompositionCatalogueError};
    use polaris_cosmos::contracts::CompositionBlockKey;

    /// This variant's own block keys, folded into the sealed catalogue's key
    /// listing test.
    #[must_use]
    pub fn keys() -> Vec<CompositionBlockKey> {
        Vec::new()
    }

    /// Registers this variant's own blocks into the sealed catalogue.
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
    use polaris_cosmos::composition::{CompositionLimits, GateExpr, materialize};

    use super::*;
    use crate::cosmos::domains::{Arm, Domain, DomainSelection, plan_for_selection};
    use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};

    #[test]
    fn experience_plan_materialises_with_conservative_limits_for_every_host_shape() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contracts");
        let target = crate::cosmos::topology::contract()
            .expect("coding contract")
            .key()
            .clone();
        let selection = DomainSelection {
            domain: Domain::Django,
            arm: Arm::Experience,
        };
        for (test_runner_available, shell_available) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            for attempt_number in [1, 2] {
                let facts = PlannerFacts {
                    registration: "test".to_owned(),
                    scope_size: 1,
                    success_check: "stated-edits",
                    objective: "careful",
                    latency_budget_ms: None,
                    test_runner_available,
                    reproduce_runner_available: false,
                    model_provider_available: true,
                    shell_available,
                    is_question: false,
                    is_survey: false,
                    has_stated_edits: false,
                    statement: "repair a Django behavior regression".to_owned(),
                };
                let attempt = RevisionAttempt::new(attempt_number).expect("valid attempt");
                let (expression, _) = plan(&facts, attempt).expect("statement body");
                let plan = plan_for_selection(selection, &facts, attempt, &catalogue, &target)
                    .expect("domain plan");
                let graph = materialize(
                    &plan,
                    &catalogue,
                    &CompositionLimits::conservative(),
                    &contracts,
                )
                .expect("conservative materialization");
                assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
                assert!(matches!(expression, GateExpr::Seq(_)));
            }
        }
    }
}
