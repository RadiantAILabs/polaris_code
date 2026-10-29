//! The coding block catalogue: small, typed `Graph` fragments a planner
//! composes into a task-specific topology, registered through
//! `polaris_cosmos`'s domain-neutral gate-composition module.
//!
//! # Where this sits
//!
//! `polaris_cosmos::composition` owns the plan format (`GatePlan`/`GateExpr`),
//! the sealed catalogue type (`CompositionCatalogue`/
//! `CompositionCatalogueSnapshot`), the block factory trait (`BlockFactory`),
//! limits, effect checks, refusal codes, and the gate-to-graph materializer
//! (`materialize`) — all domain-neutral. This module supplies the other half:
//! concrete, typed coding blocks registered into that catalogue.
//!
//! Nothing here invents a competing plan format or a second executor.
//! [`coding_block_catalogue`] builds and seals one real
//! [`CompositionCatalogueSnapshot`]; a planner's [`GatePlan`](polaris_cosmos::composition::GatePlan)
//! naming these blocks materialises through
//! [`polaris_cosmos::composition::materialize`] into an ordinary native
//! `Graph`, exactly like any other embodiment's blocks would.
//!
//! # Layout
//!
//! - [`existing`] wraps the coding stage systems `cosmos::graphs` already
//!   ships (apply, judge, run-tests, the prediction loop, the worklist sweep,
//!   the question-answer stages, and their conclusions) as individually
//!   addressable blocks. The seven whole-graph registrations in `graphs.rs`
//!   are unchanged; this module only makes their pieces separately reachable.
//! - [`configurable`] adds the small, data-configured blocks the existing
//!   stages cannot express on their own: a bounded, tool-restricted model
//!   step, a generic read-only path survey, applying a model-proposed patch,
//!   and a fork-output reducer.
//! - [`predicates`] adds the plain Rust predicates and one discriminator a
//!   `Choose`/`Switch` gate needs, over the same typed artifacts, registered
//!   as native `BoxedPredicate`/`BoxedDiscriminator` values.
//! - [`survey`] adds bounded, read-only marker scans and site assessments for
//!   plans that must inspect a workspace before deciding whether to edit it.
//! - [`clarification`] classifies a coding task into a bounded disposition and
//!   produces an explicit clarification conclusion when the task is not ready
//!   to edit or answer.
//! - [`repair`] adds the blocks a model-proposed repair loop needs and the
//!   existing single-pass stages do not supply: running the project's tests
//!   with the failure kept legible, and judging a model-proposed patch
//!   (including the empty one) rather than the task's own stated edits.
//! - [`reproduce`] adds the one block a statement-only repair loop needs when
//!   no test command was stated: writing a model-authored script that
//!   reproduces the issue, to a fixed location [`repair`]'s existing blocks
//!   can then run and judge unchanged.
//!
//! # How configuration reaches a block
//!
//! A plan never hands a block raw data directly: [`BlockFactory::build`]
//! receives a [`ResolvedConfigurationRef`](polaris_cosmos::contracts::ResolvedConfigurationRef)
//! and is responsible for decoding its own fields. A block that needs no
//! configuration (every block in [`existing`]) ignores it and shares
//! [`no_config_schema`]. A block whose behaviour is genuinely
//! per-instance data (`code.model-propose`'s role/instruction/tool
//! subset/budget/output kind, `code.survey-paths`'s path set) decodes its
//! configuration inside `build` and seeds it as a small closure-captured
//! producer system ahead of its own logic, which reads the seeded value as an
//! ordinary upstream `Out<T>` rather than an ambient `Res<T>`. This is what
//! lets two instances of the same block, configured differently, coexist in
//! one plan (for example inside a `Fork`) — each instance's `build` call
//! captures its own decoded value in its own closure, wired to its own node.

mod autotest;
pub mod cascade;
pub mod clarification;
mod command;
pub mod configurable;
pub mod existing;
pub mod fine;
pub mod predicates;
pub mod repair;
pub mod reproduce;
pub mod survey;
pub(crate) mod twm;

use polaris::system::plugin::Version;
use polaris_cosmos::composition::{
    CompositionCatalogue, CompositionCatalogueSnapshot, RetentionPolicy, RetentionPolicyId,
    RetentionPolicyKey,
};
use polaris_cosmos::contracts::{
    CompositionCatalogId, ConfigurationSchemaId, ConfigurationSchemaKey,
};
use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogSnapshot};

use super::behavior_version;
use super::domains;
use super::topology::CatalogError;

/// Stable identity of the coding gate-plan block catalogue.
pub const CATALOGUE: &str = "polaris-code.coding-gate-blocks";

/// Stable identity of the configuration schema shared by every block that
/// takes no configuration.
pub const NO_CONFIG_SCHEMA: &str = "polaris-code.gate-blocks.no-config";

/// Stable identity of the one-iteration loop-retention policy gate plans use
/// when only the current repair attempt may feed the next one.
pub const KEEP_LAST_ITERATION: &str = "polaris-code.gate-blocks.keep-last-iteration";

/// Returns the shared configuration schema for a block with nothing to
/// configure.
///
/// # Panics
///
/// Never: [`NO_CONFIG_SCHEMA`] is a compile-time constant valid stable name.
#[must_use]
pub fn no_config_schema() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new(NO_CONFIG_SCHEMA).expect("schema ID is a valid stable name"),
        behavior_version(),
    )
}

/// Returns the registered policy that retains the most recent loop iteration.
///
/// # Panics
///
/// Never: the policy ID is a compile-time valid stable name.
#[must_use]
pub fn keep_last_iteration_key() -> RetentionPolicyKey {
    RetentionPolicyKey::new(
        RetentionPolicyId::new(KEEP_LAST_ITERATION).expect("retention policy ID is valid"),
        behavior_version(),
    )
}

/// Builds and seals the coding gate-plan block catalogue: every block,
/// predicate, and discriminator this embodiment registers for a gate-plan
/// materializer to resolve.
///
/// # Panics
///
/// Never: every key registered by [`existing::register_all`],
/// [`configurable::register_all`], [`survey::register_all`],
/// [`clarification::register_all`], [`repair::register_all`],
/// [`domains::register_all`], and [`predicates::register_all`] is a
/// distinct compile-time constant, so a
/// [`CompositionCatalogueError`] can only follow from a programming error in
/// this module, one every test that calls this function would already fail
/// on; sealing can only fail if the catalogue's digest cannot be computed,
/// which does not happen for the plain data types registered here.
#[must_use]
pub fn coding_block_catalogue() -> CompositionCatalogueSnapshot {
    build_catalogue(false)
}

/// Builds lean's extended catalogue with the cascade check and continuation.
/// The ordinary catalogue remains byte-stable for all other arms.
#[must_use]
pub fn coding_block_catalogue_for_lean() -> CompositionCatalogueSnapshot {
    build_catalogue(true)
}

fn build_catalogue(lean: bool) -> CompositionCatalogueSnapshot {
    let mut catalogue = CompositionCatalogue::new(
        CompositionCatalogId::new(CATALOGUE).expect("catalogue ID is a valid stable name"),
        Version::new(1, 0, 0),
    );
    existing::register_all(&mut catalogue).expect("coding blocks register cleanly");
    configurable::register_all(&mut catalogue).expect("coding blocks register cleanly");
    survey::register_all(&mut catalogue).expect("coding blocks register cleanly");
    clarification::register_all(&mut catalogue).expect("coding blocks register cleanly");
    repair::register_all(&mut catalogue).expect("coding blocks register cleanly");
    reproduce::register_all(&mut catalogue).expect("coding blocks register cleanly");
    fine::register_all(&mut catalogue).expect("fine blocks register cleanly");
    if lean {
        cascade::register_all(&mut catalogue).expect("cascade blocks register cleanly");
    }
    domains::register_all(&mut catalogue).expect("domain blocks register cleanly");
    catalogue
        .register_retention_policy(keep_last_iteration_key(), RetentionPolicy::keep_last(1))
        .expect("coding retention policy registers cleanly");
    predicates::register_all(&mut catalogue).expect("coding predicates register cleanly");
    if lean {
        predicates::register_cascade(&mut catalogue).expect("cascade predicate registers cleanly");
    }
    catalogue.seal().expect("coding catalogue digest computes")
}

/// Builds and seals a topology contract catalog carrying only the shared
/// coding contract ([`super::topology::contract`]).
///
/// A gate-plan materializer resolves a plan's target contract against a
/// [`TopologyCatalogSnapshot`] separately from the block catalogue (contracts
/// are not composition-catalogue data). A host wiring the full topology
/// catalog already has one with this contract in it; this helper exists for
/// a caller — chiefly this crate's own tests — that wants to materialise a
/// coding gate plan without assembling the whole topology pack.
///
/// # Errors
///
/// Returns an error if the coding contract itself is rejected.
pub fn coding_target_contracts() -> Result<TopologyCatalogSnapshot, CatalogError> {
    let mut catalog = TopologyCatalog::new();
    catalog.register_contract(super::topology::contract()?)?;
    Ok(catalog.seal())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_catalogue_seals_with_every_registration_distinct() {
        let catalogue = coding_block_catalogue();
        for key in existing::keys()
            .into_iter()
            .chain(configurable::keys())
            .chain(survey::keys())
            .chain(clarification::keys())
            .chain(repair::keys())
            .chain(reproduce::keys())
            .chain(fine::keys())
            .chain(domains::keys())
        {
            assert!(
                catalogue.block(&key).is_some(),
                "{key} is missing from the sealed catalogue"
            );
        }
    }

    #[test]
    fn the_coding_target_contract_catalog_seals() {
        let contracts = coding_target_contracts().expect("the coding contract registers cleanly");
        assert!(
            contracts
                .contracts()
                .iter()
                .any(|contract| contract.key().to_string().contains("coding-assessment"))
        );
    }
}
