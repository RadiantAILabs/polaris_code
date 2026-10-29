//! Refusals a planner can read and act on.
//!
//! Materialization and bound checks never panic on an invalid plan: they
//! return [`Refusal`]s naming a stable code, where in the plan tree the
//! problem is, which catalogue key is involved, what was expected versus
//! what the plan stated, and a short repair hint. The host returns these to
//! the planner so it can submit a revised plan.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::identity::{BlockInstanceId, CaseKey};
use crate::contracts::CompositionBlockKey;

/// One step from the root of a gate-expression tree toward the node a
/// refusal concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateStep {
    /// Descended into a `Seq` child at this position.
    Seq(usize),
    /// Descended into `Choose`'s true branch.
    ChooseTrue,
    /// Descended into `Choose`'s false branch.
    ChooseFalse,
    /// Descended into a `Switch` case at this position.
    SwitchCase(usize),
    /// Descended into `Switch`'s default branch.
    SwitchDefault,
    /// Descended into a `Fork` arm at this position.
    ForkArm(usize),
    /// Descended into a `Fork`'s reducer step.
    ForkReducer,
    /// Descended into a `Loop`'s body.
    LoopBody,
    /// Descended into a `Repeat`'s body, either its guaranteed first run or
    /// its derived `Loop`'s own body.
    RepeatBody,
    /// Descended into a `Guard`'s guarded step.
    GuardStep,
    /// Descended into a `Guard`'s error-handler recipe.
    GuardOnError,
    /// Descended into a `Guard`'s timeout-handler recipe.
    GuardOnTimeout,
    /// Descended into a `Seal`'s body.
    SealBody,
    /// Descended into a `Slot` candidate at this position.
    SlotCandidate(usize),
    /// Named the plan-level wire at this position.
    Wire(usize),
}

/// A path from a gate plan's root to the gate a refusal concerns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePath(Vec<GateStep>);

impl GatePath {
    /// The empty path, naming the plan's root expression.
    #[must_use]
    pub const fn root() -> Self {
        Self(Vec::new())
    }

    /// Returns a copy of this path extended by one more step.
    #[must_use]
    pub fn join(&self, step: GateStep) -> Self {
        let mut steps = self.0.clone();
        steps.push(step);
        Self(steps)
    }

    /// Returns the steps from the root.
    #[must_use]
    pub fn steps(&self) -> &[GateStep] {
        &self.0
    }
}

impl fmt::Display for GatePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return formatter.write_str("$");
        }
        write!(formatter, "$")?;
        for step in &self.0 {
            write!(formatter, ".{step:?}")?;
        }
        Ok(())
    }
}

/// The catalogue key or plan-local label a refusal concerns, if a single one
/// applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusedKey {
    /// A block instance and the registered block key it named.
    Block {
        /// The plan-local instance label.
        instance: BlockInstanceId,
        /// The registered block key it named.
        block: CompositionBlockKey,
    },
    /// A `Switch` case label.
    Case(CaseKey),
    /// An opaque catalogue key name, for keys without a dedicated variant.
    Named(String),
}

/// A stable, model-actionable reason a plan was not materialized or admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    code: &'static str,
    path: GatePath,
    key: Option<RefusedKey>,
    expected: String,
    actual: String,
    repair_hint: String,
}

impl Refusal {
    /// Creates a refusal a planner can read and act on.
    #[must_use]
    pub fn new(
        code: &'static str,
        path: GatePath,
        key: Option<RefusedKey>,
        expected: impl Into<String>,
        actual: impl Into<String>,
        repair_hint: impl Into<String>,
    ) -> Self {
        Self {
            code,
            path,
            key,
            expected: expected.into(),
            actual: actual.into(),
            repair_hint: repair_hint.into(),
        }
    }

    /// Returns the stable, machine-readable refusal code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Returns the path in the plan tree the refusal concerns.
    #[must_use]
    pub const fn path(&self) -> &GatePath {
        &self.path
    }

    /// Returns the catalogue key or plan-local label involved, if a single
    /// one applies.
    #[must_use]
    pub const fn key(&self) -> Option<&RefusedKey> {
        self.key.as_ref()
    }

    /// Returns what the plan needed to satisfy.
    #[must_use]
    pub fn expected(&self) -> &str {
        &self.expected
    }

    /// Returns what the plan actually stated or produced.
    #[must_use]
    pub fn actual(&self) -> &str {
        &self.actual
    }

    /// Returns a short, actionable hint for a revised plan.
    #[must_use]
    pub fn repair_hint(&self) -> &str {
        &self.repair_hint
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}: expected {}, found {} ({})",
            self.code, self.path, self.expected, self.actual, self.repair_hint
        )
    }
}

/// Stable refusal codes the bound checks and materializer can return.
///
/// Native [`ValidationError`](polaris::graph::ValidationError) variants map
/// onto [`NATIVE_VALIDATION_ERROR`] with the native variant's `Display` text
/// as `actual`, except where a more specific plan-time check already caught
/// the same condition earlier and with a better repair hint.
pub mod codes {
    /// A `Step`, `Guard` step, or `Fork` reducer named a block not in the
    /// sealed catalogue.
    pub const UNKNOWN_BLOCK: &str = "plan.unknown-block";
    /// A gate named a predicate, discriminator, handler, timeout policy,
    /// retry policy, retention policy, context policy, or selector not in
    /// the sealed catalogue.
    pub const UNKNOWN_KEY: &str = "plan.unknown-key";
    /// A `ConfigId` was missing from the plan's configuration table, or its
    /// resolved value did not satisfy the named block's declared
    /// configuration schema.
    pub const BAD_CONFIG: &str = "plan.bad-config";
    /// The plan exceeded a host-supplied composition-limits bound.
    pub const LIMIT_EXCEEDED: &str = "plan.limit-exceeded";
    /// A `Fork` arm, or a `Loop`/`Guard` retry, would run a block whose
    /// declared effects forbid it: a mutating block under `Fork`, or a
    /// mutating block not declared repeat-safe under `Loop`/retry.
    pub const EFFECTS_VIOLATION: &str = "plan.effects-violation";
    /// A `Wire` named a reader, producer, or stable port that does not
    /// exist, is not yet produced at that point in the plan, or that the
    /// reader's factory rejected.
    pub const BAD_WIRE: &str = "plan.bad-wire";
    /// A `Seal`'s stated contract is not satisfied by its body's guaranteed
    /// production.
    pub const SEAL_CONTRACT_MISMATCH: &str = "plan.seal-contract-mismatch";
    /// The materialized graph's guaranteed production does not satisfy the
    /// plan's stated target contract.
    pub const TARGET_CONTRACT_MISMATCH: &str = "plan.target-contract-mismatch";
    /// An `Until` loop's predicate reads an input never produced before the
    /// loop starts.
    pub const MISSING_LOOP_INITIALIZER: &str = "plan.missing-loop-initializer";
    /// A `Switch`'s cases do not cover its discriminator's declared closed
    /// key set, and it has no default.
    pub const SWITCH_NOT_EXHAUSTIVE: &str = "plan.switch-not-exhaustive";
    /// The native graph builder or `Graph::validate` rejected the
    /// materialized graph for a reason not covered by a more specific code.
    pub const NATIVE_VALIDATION_ERROR: &str = "plan.native-validation-error";
    /// A `Guard`'s `on_error` or `on_timeout` recipe refers back to itself,
    /// directly or through another registered recipe.
    pub const RECIPE_CYCLE: &str = "plan.recipe-cycle";
    /// A plan's structure or embedded digest does not match the canonical
    /// content a freshly built `GatePlan` would itself produce — for example,
    /// an edited field in a deserialized plan.
    pub const PLAN_NOT_CANONICAL: &str = "plan.not-canonical";
    /// A plan's declared catalogue identity or digest does not match the
    /// catalogue snapshot it is being materialized against.
    pub const CATALOGUE_MISMATCH: &str = "plan.catalogue-mismatch";
}

/// Builds a [`codes::RECIPE_CYCLE`] refusal for a handler or timeout-policy
/// key that is already being expanded along the current path.
///
/// Shared by [`super::effects`] and [`super::limits`]: both expand a
/// `Guard`'s registered recipes in place, and both must stop rather than
/// recurse into a recipe that refers back to itself.
#[must_use]
pub(super) fn recipe_cycle_refusal(path: &GatePath, key: impl fmt::Display) -> Refusal {
    Refusal::new(
        codes::RECIPE_CYCLE,
        path.clone(),
        Some(RefusedKey::Named(key.to_string())),
        "a handler or timeout-policy recipe that does not refer back to itself",
        format!("{key} is already being expanded along this path"),
        "break the cycle: have this recipe fall back to a plain step, or to a different, non-cyclic recipe",
    )
}
