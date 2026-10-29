//! Declared effects of a registered block.
//!
//! Every catalogue block declares whether it reads only, or may change
//! external state — and, if it may, whether running it again with the same
//! resolved configuration is safe. The materializer uses this to refuse a
//! `Fork` over a mutating block, and a `Guard` retry or a `Loop`/`Repeat`
//! body around a mutating block not declared safe to repeat — unless, for a
//! `Loop`/`Repeat` body specifically, the block reads a wired input from a
//! producer guaranteed to complete earlier in that same body on every
//! iteration, since then its input changes on every iteration too. A
//! producer only counts if it is guaranteed on every path: earlier in the
//! body's own sequential order, not inside a branch the reader is outside
//! of, not inside a nested `Loop`, and not after the reader.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::catalogue::CompositionCatalogueSnapshot;
use super::identity::{BlockInstanceId, HandlerKey, TimeoutPolicyKey};
use super::plan::{GateExpr, GatePlan, WirePlan};
use super::refusal::{GatePath, GateStep, Refusal, RefusedKey, codes, recipe_cycle_refusal};
use crate::contracts::CompositionBlockKey;

/// Whether a block reads only, or may change external state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Effect {
    /// The block never changes state outside the graph run.
    ReadOnly,
    /// The block may change external state.
    Mutating {
        /// Whether running the block again, given the same resolved
        /// configuration, is safe.
        repeat_safe: bool,
    },
}

impl Effect {
    /// Returns whether the block may change external state.
    #[must_use]
    pub const fn is_mutating(self) -> bool {
        matches!(self, Self::Mutating { .. })
    }

    /// Returns whether running the block more than once is declared safe.
    ///
    /// Always `true` for a read-only block.
    #[must_use]
    pub const fn is_repeat_safe(self) -> bool {
        match self {
            Self::ReadOnly => true,
            Self::Mutating { repeat_safe } => repeat_safe,
        }
    }
}

struct Context<'a> {
    under_fork: bool,
    /// The instance labels guaranteed to have already completed, on every
    /// path, strictly earlier in the current position of the innermost
    /// enclosing `Loop`/`Repeat` body's own iteration.
    ///
    /// A mutating block not declared `repeat_safe` is allowed inside that
    /// body anyway if it reads a wired input from one of these labels: that
    /// producer is guaranteed to run again before the reader on every
    /// iteration, so the input is never stale. `None` outside any
    /// `Loop`/`Repeat` body, where such a block is refused outright since it
    /// never repeats there at all.
    ///
    /// Reset to an empty set at every `Loop`/`Repeat` body's own entry:
    /// nothing guaranteed in an enclosing body is necessarily rebuilt on
    /// every iteration of a *nested* loop, so it cannot supply a fresh input
    /// there either.
    guaranteed: Option<HashSet<&'a BlockInstanceId>>,
}

impl<'a> Context<'a> {
    /// Returns a context identical to `self`, with `additional` folded into
    /// its guaranteed set — for checking whatever comes after the instances
    /// `additional` names, sequentially, in the same body. A no-op outside
    /// any `Loop`/`Repeat` body.
    fn extended(&self, additional: &HashSet<&'a BlockInstanceId>) -> Self {
        let guaranteed = self.guaranteed.as_ref().map(|scope| {
            let mut scope = scope.clone();
            scope.extend(additional.iter().copied());
            scope
        });
        Self {
            under_fork: self.under_fork,
            guaranteed,
        }
    }
}

/// Handler and timeout-policy keys currently being expanded along the
/// current recursion path.
///
/// Only ancestors on the current path are tracked, not every key ever
/// visited, so the same recipe reused at two unrelated positions in a plan
/// is not mistaken for a cycle.
#[derive(Default)]
struct RecipeStack {
    handlers: Vec<HandlerKey>,
    timeout_policies: Vec<TimeoutPolicyKey>,
}

/// Checks a gate plan's blocks against their declared effects.
///
/// Refuses any block under a `Fork` arm that may change external state
/// (concurrent arms cannot be ordered against each other, so even a
/// repeat-safe mutation is unsafe there); any block retried by a `Guard`
/// that may change external state without being declared safe to run more
/// than once (a retry re-invokes the same step with the same resolved
/// input, so no read can make it safe); and any block under a `Loop`/
/// `Repeat` body that may change external state without being declared safe
/// to repeat, unless it reads a wired input from another instance built
/// fresh within that same body.
///
/// A block instance naming a key absent from `catalogue` is not reported
/// here: that is a separate unknown-block refusal, checked when the plan's
/// keys are resolved against the catalogue.
#[must_use]
pub fn check<'a>(plan: &'a GatePlan, catalogue: &'a CompositionCatalogueSnapshot) -> Vec<Refusal> {
    let mut refusals = Vec::new();
    let context = Context {
        under_fork: false,
        guaranteed: None,
    };
    let mut stack = RecipeStack::default();
    check_expr(
        plan.expression(),
        &GatePath::root(),
        catalogue,
        &context,
        plan.wires(),
        &mut stack,
        &mut refusals,
    );
    refusals
}

#[allow(clippy::too_many_arguments)]
fn check_step(
    instance: &BlockInstanceId,
    block: &CompositionBlockKey,
    path: &GatePath,
    catalogue: &CompositionCatalogueSnapshot,
    context: &Context,
    wires: &[WirePlan],
    retried: bool,
    refusals: &mut Vec<Refusal>,
) {
    let Some((_, _, effect)) = catalogue.block(block) else {
        return;
    };
    if !effect.is_mutating() {
        return;
    }
    if context.under_fork {
        refusals.push(Refusal::new(
            codes::EFFECTS_VIOLATION,
            path.clone(),
            Some(RefusedKey::Block {
                instance: instance.clone(),
                block: block.clone(),
            }),
            "a read-only block, or a Fork restructured to run this block outside its arms",
            "a mutating block inside a Fork arm",
            "move the mutating block after the Fork, or replace it with a read-only step",
        ));
        return;
    }
    if effect.is_repeat_safe() {
        return;
    }
    if retried {
        refusals.push(Refusal::new(
            codes::EFFECTS_VIOLATION,
            path.clone(),
            Some(RefusedKey::Block {
                instance: instance.clone(),
                block: block.clone(),
            }),
            "a read-only block, or a block declared safe to repeat",
            "a mutating block not declared safe to repeat, retried by a Guard",
            "remove the retry around this block, or register it with repeat_safe: true if repeating it is actually safe",
        ));
        return;
    }
    let Some(scope) = context.guaranteed.as_ref() else {
        return;
    };
    let reads_fresh_input = wires
        .iter()
        .any(|wire| wire.reader() == instance && scope.contains(wire.producer()));
    if !reads_fresh_input {
        refusals.push(Refusal::new(
            codes::EFFECTS_VIOLATION,
            path.clone(),
            Some(RefusedKey::Block {
                instance: instance.clone(),
                block: block.clone(),
            }),
            "a read-only block, a block declared safe to repeat, or one reading a wired input from a producer guaranteed to run earlier in the same body iteration",
            "a mutating block not declared safe to repeat inside a Loop/Repeat body, with no input guaranteed fresh each iteration",
            "declare the block repeat-safe, or move/wire a producer that runs earlier in the same iteration",
        ));
    }
}

/// Checks `expr`'s blocks against their declared effects and returns the
/// instance labels `expr` guarantees to have built, on every path, by the
/// time it completes — for the caller to fold into whatever runs next in the
/// same body.
#[allow(clippy::too_many_arguments)]
fn check_expr<'a>(
    expr: &'a GateExpr,
    path: &GatePath,
    catalogue: &'a CompositionCatalogueSnapshot,
    context: &Context<'a>,
    wires: &[WirePlan],
    stack: &mut RecipeStack,
    refusals: &mut Vec<Refusal>,
) -> HashSet<&'a BlockInstanceId> {
    match expr {
        GateExpr::Step(step) => {
            check_step(
                step.instance(),
                step.block(),
                path,
                catalogue,
                context,
                wires,
                false,
                refusals,
            );
            HashSet::from([step.instance()])
        }
        GateExpr::Seq(children) => {
            let mut guaranteed = HashSet::new();
            for (index, child) in children.iter().enumerate() {
                let child_context = context.extended(&guaranteed);
                let added = check_expr(
                    child,
                    &path.join(GateStep::Seq(index)),
                    catalogue,
                    &child_context,
                    wires,
                    stack,
                    refusals,
                );
                guaranteed.extend(added);
            }
            guaranteed
        }
        GateExpr::Choose {
            when_true,
            when_false,
            ..
        } => {
            check_expr(
                when_true,
                &path.join(GateStep::ChooseTrue),
                catalogue,
                context,
                wires,
                stack,
                refusals,
            );
            check_expr(
                when_false,
                &path.join(GateStep::ChooseFalse),
                catalogue,
                context,
                wires,
                stack,
                refusals,
            );
            // Only one branch ever runs, so neither branch's own production
            // is guaranteed to the caller.
            HashSet::new()
        }
        GateExpr::Switch { cases, default, .. } => {
            for (index, case) in cases.iter().enumerate() {
                check_expr(
                    case.body(),
                    &path.join(GateStep::SwitchCase(index)),
                    catalogue,
                    context,
                    wires,
                    stack,
                    refusals,
                );
            }
            if let Some(default) = default {
                check_expr(
                    default,
                    &path.join(GateStep::SwitchDefault),
                    catalogue,
                    context,
                    wires,
                    stack,
                    refusals,
                );
            }
            // Exactly one case (or the default) ever runs, so none of their
            // productions are guaranteed to the caller.
            HashSet::new()
        }
        GateExpr::Fork { arms, reducer } => {
            let arm_context = Context {
                under_fork: true,
                guaranteed: context.guaranteed.clone(),
            };
            let mut guaranteed = HashSet::new();
            for (index, arm) in arms.iter().enumerate() {
                let added = check_expr(
                    arm,
                    &path.join(GateStep::ForkArm(index)),
                    catalogue,
                    &arm_context,
                    wires,
                    stack,
                    refusals,
                );
                guaranteed.extend(added);
            }
            // Every arm is guaranteed to complete before the reducer runs.
            let reducer_context = context.extended(&guaranteed);
            check_step(
                reducer.instance(),
                reducer.block(),
                &path.join(GateStep::ForkReducer),
                catalogue,
                &reducer_context,
                wires,
                false,
                refusals,
            );
            guaranteed.insert(reducer.instance());
            guaranteed
        }
        GateExpr::Loop { body, .. } => {
            let body_context = Context {
                under_fork: context.under_fork,
                guaranteed: Some(HashSet::new()),
            };
            check_expr(
                body,
                &path.join(GateStep::LoopBody),
                catalogue,
                &body_context,
                wires,
                stack,
                refusals,
            );
            // A bare Loop may run its body zero times, so nothing it builds
            // is guaranteed to the caller.
            HashSet::new()
        }
        GateExpr::Repeat { body, .. } => {
            // The body is guaranteed to run once, but the derived Loop may
            // run it again, so within the body the same fresh-input rule
            // applies as inside a bare Loop — but since the first run always
            // happens, whatever the body itself guarantees is guaranteed to
            // the caller too.
            let body_context = Context {
                under_fork: context.under_fork,
                guaranteed: Some(HashSet::new()),
            };
            check_expr(
                body,
                &path.join(GateStep::RepeatBody),
                catalogue,
                &body_context,
                wires,
                stack,
                refusals,
            )
        }
        GateExpr::Guard {
            step,
            retry,
            on_error,
            on_timeout,
        } => {
            check_step(
                step.instance(),
                step.block(),
                &path.join(GateStep::GuardStep),
                catalogue,
                context,
                wires,
                retry.is_some(),
                refusals,
            );
            expand_handler(
                on_error,
                &path.join(GateStep::GuardOnError),
                catalogue,
                context,
                wires,
                stack,
                refusals,
            );
            if let Some(timeout_policy) = catalogue.timeout_policy(on_timeout)
                && let Some(handler) = timeout_policy.handler()
            {
                expand_timeout_handler(
                    on_timeout,
                    handler,
                    &path.join(GateStep::GuardOnTimeout),
                    catalogue,
                    context,
                    wires,
                    stack,
                    refusals,
                );
            }
            // An error or timeout diverts to a handler instead, so the
            // guarded step's own success is not guaranteed to the caller.
            HashSet::new()
        }
        GateExpr::Seal { body, .. } => {
            // Transparent: a Seal's body always runs in place, so whatever
            // it guarantees is guaranteed to the caller too.
            check_expr(
                body,
                &path.join(GateStep::SealBody),
                catalogue,
                context,
                wires,
                stack,
                refusals,
            )
        }
        GateExpr::Slot { candidates, .. } => {
            for (index, candidate) in candidates.iter().enumerate() {
                check_expr(
                    candidate,
                    &path.join(GateStep::SlotCandidate(index)),
                    catalogue,
                    context,
                    wires,
                    stack,
                    refusals,
                );
            }
            // Exactly one candidate is ever built, so no candidate's own
            // production is guaranteed to the caller.
            HashSet::new()
        }
    }
}

/// Expands a `Guard`'s `on_error` handler recipe in place, checking it
/// against the guard's ambient context.
///
/// The recipe is a one-shot terminal fallback, not something a retry policy
/// repeats, so it is checked against `context` rather than the guarded
/// step's retry-augmented context.
fn expand_handler<'a>(
    key: &HandlerKey,
    path: &GatePath,
    catalogue: &'a CompositionCatalogueSnapshot,
    context: &Context<'a>,
    wires: &[WirePlan],
    stack: &mut RecipeStack,
    refusals: &mut Vec<Refusal>,
) {
    let Some(recipe) = catalogue.handler(key) else {
        return;
    };
    if stack.handlers.contains(key) {
        refusals.push(recipe_cycle_refusal(path, key));
        return;
    }
    stack.handlers.push(key.clone());
    check_expr(recipe, path, catalogue, context, wires, stack, refusals);
    stack.handlers.pop();
}

/// Expands a `Guard`'s `on_timeout` handler recipe in place, mirroring
/// [`expand_handler`] but tracked against the timeout-policy key so a
/// handler and a timeout policy naming each other are still caught.
#[allow(clippy::too_many_arguments)]
fn expand_timeout_handler<'a>(
    key: &TimeoutPolicyKey,
    recipe: &'a GateExpr,
    path: &GatePath,
    catalogue: &'a CompositionCatalogueSnapshot,
    context: &Context<'a>,
    wires: &[WirePlan],
    stack: &mut RecipeStack,
    refusals: &mut Vec<Refusal>,
) {
    if stack.timeout_policies.contains(key) {
        refusals.push(recipe_cycle_refusal(path, key));
        return;
    }
    stack.timeout_policies.push(key.clone());
    check_expr(recipe, path, catalogue, context, wires, stack, refusals);
    stack.timeout_policies.pop();
}
