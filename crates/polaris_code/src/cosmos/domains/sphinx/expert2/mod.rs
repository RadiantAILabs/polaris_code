//! Sphinx's `expert2` variant: the expert plan rebuilt on the finer control
//! surface. Starts as the `expert` arm's own plan.

use polaris_cosmos::contracts::RevisionAttempt;

use crate::cosmos::planner::{PlannerFacts, StatementPlanBody};

/// This variant's statement-only edit plan; the `expert` arm's plan until rebuilt.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    super::expert::plan(facts, attempt)
}

/// Extra gate blocks this variant contributes to the sealed coding block
/// catalogue. This variant currently contributes none.
pub mod blocks {
    use polaris_cosmos::composition::{CompositionCatalogue, CompositionCatalogueError};
    use polaris_cosmos::contracts::CompositionBlockKey;

    /// This variant's own block keys.
    #[must_use]
    pub fn keys() -> Vec<CompositionBlockKey> {
        Vec::new()
    }

    /// Registers this variant's own blocks into the sealed catalogue.
    ///
    /// # Errors
    /// Returns [`CompositionCatalogueError::DuplicateBlock`] if a key is
    /// already registered elsewhere.
    pub fn register_all(
        _catalogue: &mut CompositionCatalogue,
    ) -> Result<(), CompositionCatalogueError> {
        Ok(())
    }
}
