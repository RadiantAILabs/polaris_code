//! Host-supplied bounds on a gate plan's shape.
//!
//! Limits are never part of a [`GatePlan`](super::GatePlan): the model plans
//! freely within whatever bounds the host chooses to enforce, and the host
//! may change those bounds without changing the plan schema. The host passes
//! a [`CompositionLimits`] to [`check`] before materializing a plan.

use super::catalogue::CompositionCatalogueSnapshot;
use super::identity::{ConfigId, HandlerKey, TimeoutPolicyKey};
use super::plan::{GateExpr, GatePlan, LoopControl};
use super::refusal::{GatePath, GateStep, Refusal, RefusedKey, codes, recipe_cycle_refusal};
use crate::contracts::{
    ConfigurationValue, ResolvedConfiguration, ResolvedConfigurationRef, canonical_cbor,
};

/// Bounds a host applies to a gate plan's shape before materializing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositionLimits {
    /// Maximum nesting depth of the gate-expression tree.
    pub max_depth: u32,
    /// Maximum number of block instances (`Step`, `Guard` steps, and `Fork`
    /// reducers) across the whole plan.
    pub max_instances: u32,
    /// Maximum number of arms in any single `Fork`.
    pub max_arms: u32,
    /// Maximum number of `Loop` gates nested inside one another.
    pub max_nested_loops: u32,
    /// Maximum iteration count for a `Loop`/`Repeat`'s loop portion.
    ///
    /// Governs [`LoopControl::Count`] directly: a plan naming a larger count
    /// is refused before materialization. For [`LoopControl::Until`], the
    /// materializer sets this same value as the native loop node's explicit
    /// maximum iteration count, so a predicate that never turns true still
    /// ends deterministically with a `MaxIterationsExceeded` execution error
    /// instead of running unbounded. For `Repeat`, the limit governs only the
    /// derived loop's own iterations, not the guaranteed leading run: the
    /// leading run and the loop's first capped iteration are the same
    /// consistent accounting `Repeat::Count` already uses.
    pub max_loop_iterations: u32,
    /// Maximum number of native graph nodes the plan may materialize to.
    ///
    /// Checked after materialization, against the built graph's actual node
    /// count, recursively including every `Seal` scope's embedded graph and
    /// every inline `Slot` candidate's graph, since a `Guard`'s recipes and a
    /// `Loop`'s unrolled iterations only determine their final node count
    /// once resolved against the catalogue.
    pub max_nodes: u32,
    /// Maximum number of wires in the plan.
    pub max_wires: u32,
    /// Maximum number of entries in the plan's configuration table.
    pub max_configurations: u32,
    /// Maximum canonical-CBOR-encoded byte size of any single resolved
    /// configuration value.
    pub max_configuration_bytes: u32,
    /// Maximum nesting depth of any single resolved configuration value's
    /// `List`/`Map` structure.
    pub max_configuration_depth: u32,
}

impl CompositionLimits {
    /// Conservative default bounds, suitable for a first integration.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            max_depth: 12,
            max_instances: 64,
            max_arms: 8,
            max_nested_loops: 2,
            max_loop_iterations: 256,
            max_nodes: 256,
            max_wires: 128,
            max_configurations: 64,
            max_configuration_bytes: 16_384,
            max_configuration_depth: 8,
        }
    }
}

struct Counters {
    depth: u32,
    instances: u32,
    nested_loops: u32,
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

/// Checks a gate plan's shape against host-supplied limits, before
/// materialization.
///
/// Expands every reachable `Guard`'s registered `on_error` and `on_timeout`
/// recipes in place against `catalogue`, so a recipe cannot bypass depth,
/// instance, arm, or nested-loop bounds. Also bounds the plan's wire count,
/// its configuration table's size, and each inline configuration value's
/// canonical byte size and nesting depth. Returns every bound violation
/// found, each naming the path in the plan tree where it occurred. Does not
/// check the materialized node count; [`super::materializer::materialize`]
/// checks [`CompositionLimits::max_nodes`] itself after materializing,
/// against the built graph, recursively including every `Seal` scope and
/// inline `Slot` candidate.
pub fn check(
    plan: &GatePlan,
    limits: &CompositionLimits,
    catalogue: &CompositionCatalogueSnapshot,
) -> Vec<Refusal> {
    let mut refusals = Vec::new();
    let mut counters = Counters {
        depth: 0,
        instances: 0,
        nested_loops: 0,
    };
    let mut stack = RecipeStack::default();
    check_expr(
        plan.expression(),
        &GatePath::root(),
        limits,
        catalogue,
        &mut counters,
        &mut stack,
        &mut refusals,
    );
    if counters.instances > limits.max_instances {
        refusals.push(Refusal::new(
            codes::LIMIT_EXCEEDED,
            GatePath::root(),
            None,
            format!("at most {} block instances", limits.max_instances),
            format!("{} block instances", counters.instances),
            "reduce the number of Step, Guard, and Fork-reducer instances in the plan",
        ));
    }

    let wire_count = plan.wires().len();
    if wire_count as u32 > limits.max_wires {
        refusals.push(Refusal::new(
            codes::LIMIT_EXCEEDED,
            GatePath::root(),
            None,
            format!("at most {} wires", limits.max_wires),
            format!("{wire_count} wires"),
            "reduce the number of wires connecting plan outputs to plan inputs",
        ));
    }

    let configuration_count = plan.configurations().len();
    if configuration_count as u32 > limits.max_configurations {
        refusals.push(Refusal::new(
            codes::LIMIT_EXCEEDED,
            GatePath::root(),
            None,
            format!("at most {} configurations", limits.max_configurations),
            format!("{configuration_count} configurations"),
            "reduce the number of entries in the plan's configuration table",
        ));
    }

    for (config_id, resolved) in plan.configurations() {
        check_configuration_bounds(config_id, resolved, limits, &mut refusals);
    }

    refusals
}

/// Bounds one resolved configuration's canonical byte size and nesting
/// depth.
///
/// A content-addressed configuration carries no value in the plan itself,
/// so it contributes nothing to either bound; the host that resolves its
/// digest is responsible for bounding the value it retrieves.
fn check_configuration_bounds(
    config_id: &ConfigId,
    resolved: &ResolvedConfigurationRef,
    limits: &CompositionLimits,
    refusals: &mut Vec<Refusal>,
) {
    let ResolvedConfiguration::Inline(value) = resolved.resolved() else {
        return;
    };

    let depth = configuration_depth(value);
    if depth > limits.max_configuration_depth {
        refusals.push(Refusal::new(
            codes::LIMIT_EXCEEDED,
            GatePath::root(),
            Some(RefusedKey::Named(config_id.to_string())),
            format!(
                "configuration nesting depth at most {}",
                limits.max_configuration_depth
            ),
            format!("configuration nesting depth {depth}"),
            "flatten this configuration value's List and Map nesting",
        ));
    }

    let bytes = canonical_cbor(value).map_or(0, |encoded| encoded.len());
    if bytes as u32 > limits.max_configuration_bytes {
        refusals.push(Refusal::new(
            codes::LIMIT_EXCEEDED,
            GatePath::root(),
            Some(RefusedKey::Named(config_id.to_string())),
            format!(
                "configuration size at most {} bytes",
                limits.max_configuration_bytes
            ),
            format!("configuration size {bytes} bytes"),
            "shrink this configuration value or move it to content-addressed storage",
        ));
    }
}

/// Nesting depth of a configuration value's `List`/`Map` structure.
///
/// A scalar has depth 0; each level of `List` or `Map` nesting adds one.
fn configuration_depth(value: &ConfigurationValue) -> u32 {
    match value {
        ConfigurationValue::List(values) => {
            1 + values.iter().map(configuration_depth).max().unwrap_or(0)
        }
        ConfigurationValue::Map(entries) => {
            1 + entries
                .iter()
                .map(|entry| configuration_depth(entry.value()))
                .max()
                .unwrap_or(0)
        }
        ConfigurationValue::Null
        | ConfigurationValue::Bool(_)
        | ConfigurationValue::Signed(_)
        | ConfigurationValue::Unsigned(_)
        | ConfigurationValue::String(_) => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn check_expr(
    expr: &GateExpr,
    path: &GatePath,
    limits: &CompositionLimits,
    catalogue: &CompositionCatalogueSnapshot,
    counters: &mut Counters,
    stack: &mut RecipeStack,
    refusals: &mut Vec<Refusal>,
) {
    counters.depth += 1;
    if counters.depth > limits.max_depth {
        refusals.push(Refusal::new(
            codes::LIMIT_EXCEEDED,
            path.clone(),
            None,
            format!("nesting depth at most {}", limits.max_depth),
            format!("nesting depth {}", counters.depth),
            "flatten this branch of the plan, for example by replacing nested Seq gates with one Seq",
        ));
    }

    match expr {
        GateExpr::Step(_) => {
            counters.instances += 1;
        }
        GateExpr::Seq(children) => {
            for (index, child) in children.iter().enumerate() {
                check_expr(
                    child,
                    &path.join(GateStep::Seq(index)),
                    limits,
                    catalogue,
                    counters,
                    stack,
                    refusals,
                );
            }
        }
        GateExpr::Choose {
            when_true,
            when_false,
            ..
        } => {
            check_expr(
                when_true,
                &path.join(GateStep::ChooseTrue),
                limits,
                catalogue,
                counters,
                stack,
                refusals,
            );
            check_expr(
                when_false,
                &path.join(GateStep::ChooseFalse),
                limits,
                catalogue,
                counters,
                stack,
                refusals,
            );
        }
        GateExpr::Switch { cases, default, .. } => {
            for (index, case) in cases.iter().enumerate() {
                check_expr(
                    case.body(),
                    &path.join(GateStep::SwitchCase(index)),
                    limits,
                    catalogue,
                    counters,
                    stack,
                    refusals,
                );
            }
            if let Some(default) = default {
                check_expr(
                    default,
                    &path.join(GateStep::SwitchDefault),
                    limits,
                    catalogue,
                    counters,
                    stack,
                    refusals,
                );
            }
        }
        GateExpr::Fork { arms, reducer } => {
            if arms.len() as u32 > limits.max_arms {
                refusals.push(Refusal::new(
                    codes::LIMIT_EXCEEDED,
                    path.clone(),
                    None,
                    format!("at most {} Fork arms", limits.max_arms),
                    format!("{} Fork arms", arms.len()),
                    "split this Fork into a smaller Fork plus a follow-on step, or reduce the arm count",
                ));
            }
            for (index, arm) in arms.iter().enumerate() {
                check_expr(
                    arm,
                    &path.join(GateStep::ForkArm(index)),
                    limits,
                    catalogue,
                    counters,
                    stack,
                    refusals,
                );
            }
            let _ = reducer;
            counters.instances += 1;
        }
        GateExpr::Loop { body, control, .. } => {
            counters.nested_loops += 1;
            if counters.nested_loops > limits.max_nested_loops {
                refusals.push(Refusal::new(
                    codes::LIMIT_EXCEEDED,
                    path.clone(),
                    None,
                    format!("at most {} nested Loop gates", limits.max_nested_loops),
                    format!("{} nested Loop gates", counters.nested_loops),
                    "flatten nested loops, for example by merging the inner loop's body into the outer loop",
                ));
            }
            if let LoopControl::Count(count) = control
                && u32::from(*count) > limits.max_loop_iterations
            {
                refusals.push(Refusal::new(
                    codes::LIMIT_EXCEEDED,
                    path.clone(),
                    None,
                    format!("at most {} loop iterations", limits.max_loop_iterations),
                    format!("{count} loop iterations"),
                    "lower the loop's iteration count",
                ));
            }
            check_expr(
                body,
                &path.join(GateStep::LoopBody),
                limits,
                catalogue,
                counters,
                stack,
                refusals,
            );
            counters.nested_loops -= 1;
        }
        GateExpr::Repeat { body, control, .. } => {
            counters.nested_loops += 1;
            if counters.nested_loops > limits.max_nested_loops {
                refusals.push(Refusal::new(
                    codes::LIMIT_EXCEEDED,
                    path.clone(),
                    None,
                    format!("at most {} nested Loop gates", limits.max_nested_loops),
                    format!("{} nested Loop gates", counters.nested_loops),
                    "flatten nested loops, for example by merging the inner loop's body into the outer loop",
                ));
            }
            if let LoopControl::Count(count) = control
                && u32::from(*count) > limits.max_loop_iterations
            {
                refusals.push(Refusal::new(
                    codes::LIMIT_EXCEEDED,
                    path.clone(),
                    None,
                    format!("at most {} loop iterations", limits.max_loop_iterations),
                    format!("{count} loop iterations after the guaranteed first run"),
                    "lower the loop's iteration count",
                ));
            }
            check_expr(
                body,
                &path.join(GateStep::RepeatBody),
                limits,
                catalogue,
                counters,
                stack,
                refusals,
            );
            counters.nested_loops -= 1;
        }
        GateExpr::Guard {
            on_error,
            on_timeout,
            ..
        } => {
            counters.instances += 1;
            expand_handler(
                on_error,
                &path.join(GateStep::GuardOnError),
                limits,
                catalogue,
                counters,
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
                    limits,
                    catalogue,
                    counters,
                    stack,
                    refusals,
                );
            }
        }
        GateExpr::Seal { body, .. } => {
            check_expr(
                body,
                &path.join(GateStep::SealBody),
                limits,
                catalogue,
                counters,
                stack,
                refusals,
            );
        }
        GateExpr::Slot { candidates, .. } => {
            for (index, candidate) in candidates.iter().enumerate() {
                check_expr(
                    candidate,
                    &path.join(GateStep::SlotCandidate(index)),
                    limits,
                    catalogue,
                    counters,
                    stack,
                    refusals,
                );
            }
        }
    }

    counters.depth -= 1;
}

/// Expands a `Guard`'s `on_error` handler recipe in place against the same
/// counters and limits as the surrounding plan.
#[allow(clippy::too_many_arguments)]
fn expand_handler(
    key: &HandlerKey,
    path: &GatePath,
    limits: &CompositionLimits,
    catalogue: &CompositionCatalogueSnapshot,
    counters: &mut Counters,
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
    check_expr(recipe, path, limits, catalogue, counters, stack, refusals);
    stack.handlers.pop();
}

/// Expands a `Guard`'s `on_timeout` handler recipe in place, mirroring
/// [`expand_handler`] but tracked against the timeout-policy key so a
/// handler and a timeout policy naming each other are still caught.
#[allow(clippy::too_many_arguments)]
fn expand_timeout_handler(
    key: &TimeoutPolicyKey,
    recipe: &GateExpr,
    path: &GatePath,
    limits: &CompositionLimits,
    catalogue: &CompositionCatalogueSnapshot,
    counters: &mut Counters,
    stack: &mut RecipeStack,
    refusals: &mut Vec<Refusal>,
) {
    if stack.timeout_policies.contains(key) {
        refusals.push(recipe_cycle_refusal(path, key));
        return;
    }
    stack.timeout_policies.push(key.clone());
    check_expr(recipe, path, limits, catalogue, counters, stack, refusals);
    stack.timeout_policies.pop();
}
