//! Turns a validated gate plan into a native, executable graph.
//!
//! The materializer resolves every catalogue key in a [`GatePlan`] against a
//! [`CompositionCatalogueSnapshot`], calls ordinary native [`Graph`] builder
//! methods to add and connect nodes, resolves plan-level `Wire`s, and runs
//! native [`Graph::validate`] plus a target-contract check before returning
//! the built graph.
//!
//! # The target-contract check does not wrap the plan in a scope
//!
//! The materializer does not wrap the built graph in one outer declared scope
//! and leave the check to native `Graph::validate`. A declared scope's
//! contract is only checked against its interior when the interior has no
//! nested boundary, and a `Slot` gate materializes to a native `Dynamic`
//! node, which always counts as one. Since `Slot` is one of the required
//! gates, a plan containing one would fail validation regardless of whether
//! the plan actually satisfies its target.
//!
//! Instead, the materializer checks the target contract directly against the
//! built graph's derived [`GraphSignature`]: `requires` and
//! `requires_outputs` are compared the same way native
//! [`GraphSignature::satisfies`] compares them, but `produces` is compared
//! using [`GraphSignature::guaranteed_produces`] rather than plain
//! `produces`, since a plain derived `produces` is a union over every
//! reachable node and does not reflect what a `Choose`, `Switch`, `Guard`, or
//! `Loop`-shaped plan actually guarantees on every successful path. A nested
//! `Seal` gate still materializes as an ordinary declared-contract scope and
//! is checked by native `validate`; only the whole-plan boundary skips the
//! synthetic wrap.

use std::any::TypeId;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use polaris::graph::node::Node;
use polaris::graph::predicate::{
    BoxedDiscriminator, BoxedPredicate, ErasedDiscriminator, ErasedPredicate, PredicateError,
};
use polaris::graph::selector::{BoxedSelector, ErasedSelector};
use polaris::graph::{
    CandidateSource, DynamicSlot, Graph, GraphSignature, NodeId, NodeName, ValidationError,
};
use polaris::system::param::{Access, SystemContext};

use super::catalogue::CompositionCatalogueSnapshot;
use super::factory::BlockFactory;
use super::identity::BlockInstanceId;
use super::limits::CompositionLimits;
use super::plan::{GateExpr, GatePlan, LoopControl, StepPlan, WirePlan};
use super::refusal::{GatePath, GateStep, Refusal, RefusedKey, codes};
use crate::contracts::CanonicalRecord;
use crate::topology::{TopologyCatalogSnapshot, TopologyContract, TopologyContractKey};

/// Builds and validates a native graph from a gate plan.
///
/// Checks `plan`'s shape against `limits` and its blocks' declared effects
/// against `catalogue` before building anything. Resolves every key in
/// `plan` against `catalogue`, builds an ordinary native [`Graph`], resolves
/// `plan`'s `Wire`s through [`BlockFactory::wire_input`], runs native
/// [`Graph::validate`], and checks the built graph's guaranteed production
/// against the target contract named by `plan.target()`, resolved against
/// `contracts`.
///
/// # Errors
///
/// Returns every [`Refusal`] found. A plan that fails canonical validation or
/// names a catalogue identity/digest other than `catalogue`'s own is refused
/// immediately, before any limits or effects traversal, since nothing else
/// about an untrusted or stale plan can be trusted either. A plan that fails
/// a pre-build check (limits, effects) is never built; a plan that fails key
/// resolution during the build is refused on the first such failure, since
/// further building is meaningless once a key fails to resolve; a plan that
/// builds successfully but fails native validation, a loop-initializer check,
/// or the target-contract check returns every refusal those checks found.
pub fn materialize(
    plan: &GatePlan,
    catalogue: &CompositionCatalogueSnapshot,
    limits: &CompositionLimits,
    contracts: &TopologyCatalogSnapshot,
) -> Result<Graph, Vec<Refusal>> {
    if let Err(error) = plan.validate() {
        return Err(vec![Refusal::new(
            codes::PLAN_NOT_CANONICAL,
            GatePath::root(),
            None,
            "a plan whose structure and embedded digest match its own canonical content",
            error.to_string(),
            "discard this plan and ask the planner for a freshly produced one",
        )]);
    }
    if plan.catalogue() != catalogue.catalogue() || plan.catalogue_digest() != catalogue.digest() {
        return Err(vec![Refusal::new(
            codes::CATALOGUE_MISMATCH,
            GatePath::root(),
            Some(RefusedKey::Named(plan.catalogue().to_string())),
            format!(
                "catalogue {} at digest {}",
                catalogue.catalogue(),
                catalogue.digest()
            ),
            format!(
                "catalogue {} at digest {}",
                plan.catalogue(),
                plan.catalogue_digest()
            ),
            "re-plan against the catalogue snapshot currently sealed at this host",
        )]);
    }

    let mut refusals = super::limits::check(plan, limits, catalogue);
    refusals.extend(super::effects::check(plan, catalogue));
    if !refusals.is_empty() {
        return Err(refusals);
    }

    let ctx = BuildCtx {
        catalogue,
        contracts,
        plan,
        limits,
    };
    let mut instances = BTreeMap::new();
    let mut node_paths = NodePaths::new();
    let mut built = build_expr(
        plan.expression(),
        &GatePath::root(),
        &ctx,
        &mut instances,
        &mut node_paths,
    )
    .map_err(|refusal| vec![refusal])?;

    let node_count = recursive_node_count(&built);
    if node_count > limits.max_nodes as usize {
        return Err(vec![Refusal::new(
            codes::LIMIT_EXCEEDED,
            GatePath::root(),
            None,
            format!("at most {} materialized graph nodes", limits.max_nodes),
            format!("{node_count} materialized graph nodes"),
            "reduce the plan's size, including Seal scope interiors and Slot candidates",
        )]);
    }

    let repeat_body_labels = plan.expression().repeat_body_instance_labels();
    for (index, wire) in plan.wires().iter().enumerate() {
        if repeat_body_labels.contains(wire.reader())
            || repeat_body_labels.contains(wire.producer())
        {
            // Resolved locally, once per copy, while building the Repeat
            // whose body contains it: see the `GateExpr::Repeat` arm of
            // `build_expr`. The shared `instances` table only ever holds
            // the lead copy, so resolving it again here would either wire
            // the lead copy's node twice or fail to find the loop-interior
            // copy's own node.
            continue;
        }
        let path = GatePath::root().join(GateStep::Wire(index));
        wire_one(wire, &path, &instances, &mut built).map_err(|refusal| vec![refusal])?;
    }

    let native_result = built.validate();
    let mut refusals: Vec<Refusal> = native_result
        .errors
        .iter()
        .map(|error| map_validation_error(resolve_native_error_path(error, &node_paths), error))
        .collect();

    let target_signature = match resolve_contract(contracts, plan.target(), &GatePath::root()) {
        Ok(signature) => signature,
        Err(refusal) => {
            refusals.push(refusal);
            return Err(refusals);
        }
    };
    let built_signature = built.signature();

    check_loop_initializers(
        plan.expression(),
        &GatePath::root(),
        catalogue,
        built_signature.requires_outputs(),
        target_signature.requires_outputs(),
        &mut refusals,
    );

    let mut slot_requires = Vec::new();
    let mut slot_requires_outputs = Vec::new();
    fold_slot_contract_accesses(
        plan.expression(),
        &ctx,
        &mut slot_requires,
        &mut slot_requires_outputs,
    );
    let mut effective_requires = built_signature.requires().to_vec();
    effective_requires.extend(slot_requires.iter().cloned());
    let mut effective_requires_outputs = built_signature.requires_outputs().to_vec();
    effective_requires_outputs.extend(slot_requires_outputs.iter().cloned());

    let missing_requires = access_difference(target_signature.requires(), &effective_requires);
    let unclaimed_slot_requires = access_difference(&slot_requires, target_signature.requires());
    let extra_requires_outputs = access_type_difference(
        &effective_requires_outputs,
        target_signature.requires_outputs(),
    );
    let missing_produces = access_difference(
        target_signature.produces(),
        built_signature.guaranteed_produces(),
    );
    if !missing_requires.is_empty()
        || !unclaimed_slot_requires.is_empty()
        || !extra_requires_outputs.is_empty()
        || !missing_produces.is_empty()
    {
        refusals.push(Refusal::new(
            codes::TARGET_CONTRACT_MISMATCH,
            GatePath::root(),
            None,
            "the target contract's requires, requires_outputs, and produces satisfied by the built graph's guaranteed production, including every Slot's own required inputs",
            format!(
                "missing requires: [{}]; requires needed by a Slot but not claimed by the target: [{}]; extra requires_outputs: [{}]; missing produces: [{}]",
                describe_access(&missing_requires),
                describe_access(&unclaimed_slot_requires),
                describe_access(&extra_requires_outputs),
                describe_access(&missing_produces),
            ),
            "adjust the plan so its guaranteed production covers the target's produces, its free output reads stay within the target's requires_outputs, it requires everything the target requires, and the target requires everything any Slot's admitted candidate needs",
        ));
    }

    if refusals.is_empty() {
        Ok(built)
    } else {
        Err(refusals)
    }
}

/// Counts every node the built graph actually contains, recursively
/// descending into every `Seal` scope's embedded graph and every `Slot`'s
/// inline candidate graphs.
///
/// A flat [`Graph::nodes`] walk already includes `Choose`, `Switch`, `Fork`,
/// `Loop`, and `Guard` recipe content, since the native builder flattens all
/// of those into the same top-level node list; only a `Scope` node's
/// embedded graph and a `Dynamic` node's inline candidates are held apart
/// from that list, so only those two need explicit recursion to reach a true
/// total.
fn recursive_node_count(graph: &Graph) -> usize {
    let mut total = graph.node_count();
    for node in graph.nodes() {
        match node {
            Node::Scope(scope) => total += recursive_node_count(scope.graph()),
            Node::Dynamic(dynamic) => {
                if let CandidateSource::Inline(candidates) = dynamic.source() {
                    for (_, candidate) in candidates {
                        total += recursive_node_count(candidate);
                    }
                }
            }
            _ => {}
        }
    }
    total
}

/// Maps a native validation error onto a stable, model-actionable refusal.
///
/// Kept separate from [`materialize`] so plan-time checks that already
/// caught the same condition (for example, a `Switch` the catalogue's
/// discriminator registration proves incomplete) can produce a more specific
/// code and repair hint without waiting for native validation to run.
#[must_use]
pub fn map_validation_error(path: GatePath, error: &ValidationError) -> Refusal {
    if let ValidationError::ScopeContractViolated { diff, .. } = error {
        return Refusal::new(
            codes::SEAL_CONTRACT_MISMATCH,
            path,
            None,
            "the Seal's stated contract satisfied by its body's guaranteed production",
            diff.to_string(),
            "adjust the Seal's contract or its body so the body's guaranteed production satisfies the declared contract",
        );
    }
    Refusal::new(
        codes::NATIVE_VALIDATION_ERROR,
        path,
        None,
        "a graph accepted by native Graph::validate",
        error.to_string(),
        "revise the plan so the referenced node, branch, or predicate is reachable and complete",
    )
}

/// Resolves the plan path a native validation error concerns, by looking up
/// [`primary_node`]'s chosen node in `node_paths`.
///
/// Falls back to [`GatePath::root`] when the error names no single node (for
/// example [`ValidationError::NoEntryPoint`] or an ambiguous multi-orphan
/// [`ValidationError::DisconnectedNodes`]), or when the chosen node was never
/// recorded during the build — both native validation and this map can only
/// ever narrow the plan-time root default, never regress it.
fn resolve_native_error_path(error: &ValidationError, node_paths: &NodePaths) -> GatePath {
    primary_node(error)
        .and_then(|node_id| node_paths.get(node_id))
        .cloned()
        .unwrap_or_else(GatePath::root)
}

/// Picks the single native [`NodeId`] a validation error most precisely
/// concerns.
///
/// [`ValidationError::ScopeGraphInvalid`] and
/// [`ValidationError::DynamicCandidateInvalid`] wrap an inner error from a
/// nested graph; the inner error's own node, when it names one, is always
/// more precise than the outer `Scope`/`Dynamic` boundary node, so recursion
/// prefers it. Returns `None` for the entry-point and multi-orphan variants,
/// which name no single node, and for any variant added to the
/// `#[non_exhaustive]` enum after this match was written.
fn primary_node(error: &ValidationError) -> Option<&NodeId> {
    match error {
        ValidationError::ScopeGraphInvalid { inner, node, .. }
        | ValidationError::DynamicCandidateInvalid { inner, node, .. } => {
            primary_node(inner).or(Some(node))
        }
        ValidationError::InvalidEntryPoint(node) => Some(node),
        ValidationError::InvalidEdgeSource { node, .. }
        | ValidationError::InvalidEdgeTarget { node, .. }
        | ValidationError::AmbiguousRoute { node, .. }
        | ValidationError::MissingPredicate { node, .. }
        | ValidationError::MissingBranch { node, .. }
        | ValidationError::InvalidBranchTarget { node, .. }
        | ValidationError::MissingDiscriminator { node, .. }
        | ValidationError::EmptySwitch { node, .. }
        | ValidationError::InvalidCaseTarget { node, .. }
        | ValidationError::InvalidDefaultTarget { node, .. }
        | ValidationError::EmptyParallel { node, .. }
        | ValidationError::NoTerminationCondition { node, .. }
        | ValidationError::EmptyLoopBody { node, .. }
        | ValidationError::InvalidLoopBody { node, .. }
        | ValidationError::LoopPredicateOutputNotProduced { node, .. }
        | ValidationError::MissingEdgeRequirement { node, .. }
        | ValidationError::EmptyScopeGraph { node, .. }
        | ValidationError::ScopeGraphNoEntryPoint { node, .. }
        | ValidationError::ScopeContractRequireBlocked { node, .. }
        | ValidationError::ScopeContractFactoryUnsupported { node, .. }
        | ValidationError::ScopeContractOutputsBlocked { node, .. }
        | ValidationError::ScopeContractNested { node, .. }
        | ValidationError::ScopeContractViolated { node, .. }
        | ValidationError::EmptyDynamicSource { node, .. }
        | ValidationError::DynamicCandidateIncompatible { node, .. }
        | ValidationError::DynamicCandidateMissingGuaranteedOutputs { node, .. }
        | ValidationError::DynamicCandidateNested { node, .. }
        | ValidationError::DynamicCandidateDuplicate { node, .. }
        | ValidationError::ReadFromSelf { node, .. } => Some(node),
        ValidationError::ReadFromProducerMissing { reader, .. }
        | ValidationError::ReadFromProducerNotASystem { reader, .. }
        | ValidationError::ReadFromTypeMismatch { reader, .. }
        | ValidationError::ReadFromNotGuaranteed { reader, .. }
        | ValidationError::ReadFromUnread { reader, .. }
        | ValidationError::ReadFromBackwardRequiresOptional { reader, .. } => Some(reader),
        _ => None,
    }
}

/// Bundles the immutable inputs threaded through every recursive build call.
struct BuildCtx<'a> {
    catalogue: &'a CompositionCatalogueSnapshot,
    contracts: &'a TopologyCatalogSnapshot,
    plan: &'a GatePlan,
    limits: &'a CompositionLimits,
}

/// Every block instance built so far, keyed by its plan-local label: the
/// native node other blocks wire to, and a clone of the factory that built
/// it (needed later to resolve a `Wire` naming this instance as a reader).
type BlockInstances = BTreeMap<BlockInstanceId, (NodeId, Arc<dyn BlockFactory>)>;

/// Every native node built so far, keyed by its own [`NodeId`], to the plan
/// path of the gate expression that built it.
///
/// Native [`NodeId`]s are process-wide unique and never remapped by
/// `Graph::append` or any other merge, so one flat map, populated as
/// [`build_expr`] and [`build_step`] descend, correctly locates a node built
/// anywhere in the tree, including inside a `Seal`'s body or a `Slot`'s
/// candidates. Used to translate a native [`ValidationError`]'s own node
/// reference back into the plan path a refusal should name, once
/// `Graph::validate` runs on the fully built, flattened graph and the
/// recursive-descent `path` that built each node is no longer in scope.
///
/// A [`HashMap`], not a [`BTreeMap`]: [`NodeId`] derives `Hash`/`Eq` but not
/// `Ord`.
type NodePaths = HashMap<NodeId, GatePath>;

/// A `Switch` branch body, built and boxed for the native `add_switch_boxed`
/// builder.
type SwitchBranch = Box<dyn FnOnce(&mut Graph)>;

/// Finds the topology contract named by `key` and returns its native
/// signature, or a [`codes::UNKNOWN_KEY`] refusal if `contracts` has no
/// contract registered under that key.
#[expect(
    clippy::result_large_err,
    reason = "Refusal carries the detail a planner needs to act on; boxing it would only move the cost to every caller"
)]
fn resolve_contract<'a>(
    contracts: &'a TopologyCatalogSnapshot,
    key: &TopologyContractKey,
    path: &GatePath,
) -> Result<&'a GraphSignature, Refusal> {
    contracts
        .contracts()
        .iter()
        .find(|contract| contract.key() == key)
        .map(TopologyContract::signature)
        .ok_or_else(|| {
            Refusal::new(
                codes::UNKNOWN_KEY,
                path.clone(),
                Some(RefusedKey::Named(key.to_string())),
                "a topology contract registered in the sealed topology catalog",
                format!("contract {key}"),
                "register this contract, or reference a different registered contract key",
            )
        })
}

fn unknown_key_refusal(path: &GatePath, kind: &str, key: impl std::fmt::Display) -> Refusal {
    Refusal::new(
        codes::UNKNOWN_KEY,
        path.clone(),
        Some(RefusedKey::Named(key.to_string())),
        format!("a {kind} registered in the sealed composition catalogue"),
        format!("{kind} {key}"),
        format!("register this {kind}, or reference a different registered {kind} key"),
    )
}

fn native_build_error(path: &GatePath, detail: impl std::fmt::Display) -> Refusal {
    Refusal::new(
        codes::NATIVE_VALIDATION_ERROR,
        path.clone(),
        None,
        "a graph the native builder can merge",
        detail.to_string(),
        "revise the plan so this branch's blocks connect into one well-formed subgraph",
    )
}

/// Builds one registered block instance, recording its native node ID and a
/// clone of its factory handle in `instances` for later `Wire` resolution.
///
/// Used for `Step`, `Guard`'s guarded step, and `Fork`'s reducer — every
/// place a plan names one [`StepPlan`] directly.
#[expect(
    clippy::result_large_err,
    reason = "Refusal carries the detail a planner needs to act on; boxing it would only move the cost to every caller"
)]
fn build_step(
    step: &StepPlan,
    path: &GatePath,
    ctx: &BuildCtx<'_>,
    instances: &mut BlockInstances,
    node_paths: &mut NodePaths,
) -> Result<(Graph, NodeId), Refusal> {
    let Some((factory, schema, _effect)) = ctx.catalogue.block(step.block()) else {
        return Err(Refusal::new(
            codes::UNKNOWN_BLOCK,
            path.clone(),
            Some(RefusedKey::Block {
                instance: step.instance().clone(),
                block: step.block().clone(),
            }),
            "a block registered in the sealed composition catalogue",
            format!("block {}", step.block()),
            "register this block under that key, or name a different registered block",
        ));
    };
    let Some(config) = ctx.plan.configurations().get(step.config()) else {
        return Err(Refusal::new(
            codes::UNKNOWN_KEY,
            path.clone(),
            Some(RefusedKey::Named(step.config().to_string())),
            "a configuration present in the plan's configuration table",
            format!("configuration {}", step.config()),
            "add this configuration id to the plan's configuration table",
        ));
    };
    if config.schema() != schema {
        return Err(Refusal::new(
            codes::BAD_CONFIG,
            path.clone(),
            Some(RefusedKey::Block {
                instance: step.instance().clone(),
                block: step.block().clone(),
            }),
            format!("a configuration matching schema {schema}"),
            format!("configuration matching schema {}", config.schema()),
            "resolve this step's configuration against the block's declared configuration schema",
        ));
    }
    let mut graph = Graph::new();
    let node_id = factory.build(&mut graph, config).map_err(|error| {
        Refusal::new(
            codes::BAD_CONFIG,
            path.clone(),
            Some(RefusedKey::Block {
                instance: step.instance().clone(),
                block: step.block().clone(),
            }),
            "a configuration the block accepts",
            error.to_string(),
            "adjust this step's configuration to satisfy the block's requirements",
        )
    })?;
    instances.insert(
        step.instance().clone(),
        (node_id.clone(), Arc::clone(factory)),
    );
    node_paths.insert(node_id.clone(), path.clone());
    Ok((graph, node_id))
}

/// Recursively builds one gate expression into an owned, self-contained
/// [`Graph`].
///
/// Fails on the first refusal encountered (unlike the whole-tree
/// [`super::limits::check`] and [`super::effects::check`], which accumulate
/// every violation), since further building is meaningless once a key fails
/// to resolve.
#[expect(
    clippy::result_large_err,
    reason = "Refusal carries the detail a planner needs to act on; boxing it would only move the cost to every caller"
)]
fn build_expr(
    expr: &GateExpr,
    path: &GatePath,
    ctx: &BuildCtx<'_>,
    instances: &mut BlockInstances,
    node_paths: &mut NodePaths,
) -> Result<Graph, Refusal> {
    match expr {
        GateExpr::Step(step) => {
            let (graph, _node_id) = build_step(step, path, ctx, instances, node_paths)?;
            Ok(graph)
        }
        GateExpr::Seq(children) => {
            let mut graph = Graph::new();
            for (index, child) in children.iter().enumerate() {
                let sub = build_expr(
                    child,
                    &path.join(GateStep::Seq(index)),
                    ctx,
                    instances,
                    node_paths,
                )?;
                graph
                    .append(sub)
                    .map_err(|error| native_build_error(path, error))?;
            }
            Ok(graph)
        }
        GateExpr::Choose {
            predicate,
            when_true,
            when_false,
        } => {
            let shared = ctx
                .catalogue
                .predicate(predicate)
                .ok_or_else(|| unknown_key_refusal(path, "predicate", predicate))?;
            let true_graph = build_expr(
                when_true,
                &path.join(GateStep::ChooseTrue),
                ctx,
                instances,
                node_paths,
            )?;
            let false_graph = build_expr(
                when_false,
                &path.join(GateStep::ChooseFalse),
                ctx,
                instances,
                node_paths,
            )?;
            let mut graph = Graph::new();
            graph.add_conditional_branch_boxed(
                "choose",
                fresh_predicate(&shared),
                move |g: &mut Graph| *g = true_graph,
                move |g: &mut Graph| *g = false_graph,
            );
            let choose_id = graph
                .last_node()
                .expect("add_conditional_branch_boxed just added the choose node");
            node_paths.insert(choose_id, path.clone());
            Ok(graph)
        }
        GateExpr::Switch {
            discriminator,
            cases,
            default,
        } => {
            let (shared, closed_cases) = ctx
                .catalogue
                .discriminator(discriminator)
                .ok_or_else(|| unknown_key_refusal(path, "discriminator", discriminator))?;
            if default.is_none() {
                let missing: Vec<_> = closed_cases
                    .iter()
                    .filter(|case| !cases.iter().any(|covered| covered.key() == *case))
                    .collect();
                if let Some(first_missing) = missing.first() {
                    return Err(Refusal::new(
                        codes::SWITCH_NOT_EXHAUSTIVE,
                        path.clone(),
                        Some(RefusedKey::Case((*first_missing).clone())),
                        "every case in the discriminator's closed key set, or a default branch",
                        format!(
                            "missing case(s): {}",
                            missing
                                .iter()
                                .map(|case| case.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        "add the missing case bodies, or add a default branch",
                    ));
                }
            }
            let mut case_closures: Vec<(String, SwitchBranch)> = Vec::with_capacity(cases.len());
            for (index, case) in cases.iter().enumerate() {
                let sub = build_expr(
                    case.body(),
                    &path.join(GateStep::SwitchCase(index)),
                    ctx,
                    instances,
                    node_paths,
                )?;
                case_closures.push((
                    case.key().as_str().to_string(),
                    Box::new(move |g: &mut Graph| *g = sub),
                ));
            }
            let default_closure: Option<SwitchBranch> = match default {
                Some(expr) => {
                    let sub = build_expr(
                        expr,
                        &path.join(GateStep::SwitchDefault),
                        ctx,
                        instances,
                        node_paths,
                    )?;
                    Some(Box::new(move |g: &mut Graph| *g = sub))
                }
                None => None,
            };
            let mut graph = Graph::new();
            graph.add_switch_boxed(
                "switch",
                fresh_discriminator(&shared),
                case_closures,
                default_closure,
            );
            let switch_id = graph
                .last_node()
                .expect("add_switch_boxed just added the switch node");
            node_paths.insert(switch_id, path.clone());
            Ok(graph)
        }
        GateExpr::Fork { arms, reducer } => {
            let mut arm_graphs = Vec::with_capacity(arms.len());
            for (index, arm) in arms.iter().enumerate() {
                arm_graphs.push(build_expr(
                    arm,
                    &path.join(GateStep::ForkArm(index)),
                    ctx,
                    instances,
                    node_paths,
                )?);
            }
            let mut graph = Graph::new();
            graph.add_parallel(
                "fork",
                arm_graphs
                    .into_iter()
                    .map(|sub| move |g: &mut Graph| *g = sub),
            );
            let fork_id = graph
                .last_node()
                .expect("add_parallel just added the fork node");
            node_paths.insert(fork_id, path.clone());
            let (reducer_graph, _reducer_id) = build_step(
                reducer,
                &path.join(GateStep::ForkReducer),
                ctx,
                instances,
                node_paths,
            )?;
            graph
                .append(reducer_graph)
                .map_err(|error| native_build_error(path, error))?;
            Ok(graph)
        }
        GateExpr::Loop {
            body,
            control,
            retention,
        } => {
            let retention_policy = ctx
                .catalogue
                .retention_policy(retention)
                .ok_or_else(|| unknown_key_refusal(path, "retention policy", retention))?;
            let body_graph = build_expr(
                body,
                &path.join(GateStep::LoopBody),
                ctx,
                instances,
                node_paths,
            )?;
            let mut graph = Graph::new();
            match control {
                LoopControl::Until(predicate_key) => {
                    let shared = ctx
                        .catalogue
                        .predicate(predicate_key)
                        .ok_or_else(|| unknown_key_refusal(path, "predicate", predicate_key))?;
                    graph.add_loop_boxed(
                        "loop_until",
                        fresh_predicate(&shared),
                        move |g: &mut Graph| {
                            *g = body_graph;
                        },
                    );
                }
                LoopControl::Count(count) => {
                    graph.add_loop_n("loop_n", usize::from(*count), move |g: &mut Graph| {
                        *g = body_graph;
                    });
                }
            }
            let loop_id = graph
                .last_node()
                .expect("add_loop_boxed/add_loop_n just added the loop node");
            if matches!(control, LoopControl::Until(_)) {
                graph.set_loop_max_iterations(
                    loop_id.clone(),
                    ctx.limits.max_loop_iterations as usize,
                );
            }
            graph.set_loop_retention(loop_id.clone(), retention_policy.iterations() as usize);
            node_paths.insert(loop_id, path.clone());
            Ok(graph)
        }
        GateExpr::Repeat {
            body,
            control,
            retention,
        } => {
            let retention_policy = ctx
                .catalogue
                .retention_policy(retention)
                .ok_or_else(|| unknown_key_refusal(path, "retention policy", retention))?;

            // The leading run is built first, into the shared instance
            // table, so a plan-level Wire whose endpoints are both outside
            // this body resolves against it later, in the top-level wire
            // pass.
            let mut lead_graph = build_expr(
                body,
                &path.join(GateStep::RepeatBody),
                ctx,
                instances,
                node_paths,
            )?;

            // The derived loop's own body is an independent copy, built into
            // an isolated, throwaway instance table it never shares with the
            // rest of the plan.
            let mut loop_instances = BlockInstances::new();
            let mut body_graph = build_expr(
                body,
                &path.join(GateStep::RepeatBody),
                ctx,
                &mut loop_instances,
                node_paths,
            )?;

            // A Wire whose producer and reader are both directly inside this
            // body (not further inside a nested Repeat's own body, which
            // resolves its own same-body wires the same way) is resolved
            // here, once against each copy's own instances, since the
            // top-level wire pass only ever sees the shared table and so
            // could resolve at most the lead copy. `GatePlan::new` already
            // refuses any Wire that crosses this body's boundary.
            let direct_labels = body.direct_instance_labels();
            for (index, wire) in ctx.plan.wires().iter().enumerate() {
                if direct_labels.contains(wire.reader()) && direct_labels.contains(wire.producer())
                {
                    let wire_path = GatePath::root().join(GateStep::Wire(index));
                    wire_one(wire, &wire_path, instances, &mut lead_graph)?;
                    wire_one(wire, &wire_path, &loop_instances, &mut body_graph)?;
                }
            }

            let mut loop_graph = Graph::new();
            match control {
                LoopControl::Until(predicate_key) => {
                    let shared = ctx
                        .catalogue
                        .predicate(predicate_key)
                        .ok_or_else(|| unknown_key_refusal(path, "predicate", predicate_key))?;
                    loop_graph.add_loop_boxed(
                        "repeat_until",
                        fresh_predicate(&shared),
                        move |g: &mut Graph| {
                            *g = body_graph;
                        },
                    );
                }
                LoopControl::Count(count) => {
                    loop_graph.add_loop_n("repeat_n", usize::from(*count), move |g: &mut Graph| {
                        *g = body_graph;
                    });
                }
            }
            let loop_id = loop_graph
                .last_node()
                .expect("add_loop_boxed/add_loop_n just added the loop node");
            if matches!(control, LoopControl::Until(_)) {
                loop_graph.set_loop_max_iterations(
                    loop_id.clone(),
                    ctx.limits.max_loop_iterations as usize,
                );
            }
            loop_graph.set_loop_retention(loop_id.clone(), retention_policy.iterations() as usize);
            node_paths.insert(loop_id, path.clone());

            let mut graph = Graph::new();
            graph
                .append(lead_graph)
                .map_err(|error| native_build_error(path, error))?;
            graph
                .append(loop_graph)
                .map_err(|error| native_build_error(path, error))?;
            Ok(graph)
        }
        GateExpr::Guard {
            step,
            on_error,
            on_timeout,
            retry,
        } => {
            let (mut graph, node_id) = build_step(
                step,
                &path.join(GateStep::GuardStep),
                ctx,
                instances,
                node_paths,
            )?;

            let handler_expr = ctx
                .catalogue
                .handler(on_error)
                .ok_or_else(|| unknown_key_refusal(path, "handler", on_error))?;
            let handler_graph = build_expr(
                handler_expr,
                &path.join(GateStep::GuardOnError),
                ctx,
                instances,
                node_paths,
            )?;
            graph.add_error_handler_for([node_id.clone()], move |g: &mut Graph| {
                *g = handler_graph;
            });

            let timeout_entry = ctx
                .catalogue
                .timeout_policy(on_timeout)
                .ok_or_else(|| unknown_key_refusal(path, "timeout policy", on_timeout))?;
            graph.set_timeout(node_id.clone(), timeout_entry.duration());
            if let Some(timeout_handler_expr) = timeout_entry.handler() {
                let timeout_handler_graph = build_expr(
                    timeout_handler_expr,
                    &path.join(GateStep::GuardOnTimeout),
                    ctx,
                    instances,
                    node_paths,
                )?;
                graph.add_timeout_handler([node_id.clone()], move |g: &mut Graph| {
                    *g = timeout_handler_graph;
                });
            }

            if let Some(retry_key) = retry {
                let retry_policy = ctx
                    .catalogue
                    .retry_policy(retry_key)
                    .ok_or_else(|| unknown_key_refusal(path, "retry policy", retry_key))?;
                graph.set_retry_policy(node_id.clone(), retry_policy.clone());
            }

            Ok(graph)
        }
        GateExpr::Seal {
            body,
            contract,
            policy,
        } => {
            let contract_signature = resolve_contract(ctx.contracts, contract, path)?.clone();
            let context_policy = ctx
                .catalogue
                .context_policy(policy)
                .ok_or_else(|| unknown_key_refusal(path, "context policy", policy))?
                .clone();
            let body_graph = build_expr(
                body,
                &path.join(GateStep::SealBody),
                ctx,
                instances,
                node_paths,
            )?;
            let mut graph = Graph::new();
            graph.add_scope_with_contract("seal", body_graph, context_policy, contract_signature);
            let seal_id = graph
                .last_node()
                .expect("add_scope_with_contract just added the seal node");
            node_paths.insert(seal_id, path.clone());
            Ok(graph)
        }
        GateExpr::Slot {
            contract,
            candidates,
            selector,
            policy,
        } => {
            let contract_signature = resolve_contract(ctx.contracts, contract, path)?.clone();
            let context_policy = ctx
                .catalogue
                .context_policy(policy)
                .ok_or_else(|| unknown_key_refusal(path, "context policy", policy))?
                .clone();
            let shared_selector = ctx
                .catalogue
                .selector(selector)
                .ok_or_else(|| unknown_key_refusal(path, "selector", selector))?;
            let mut built_candidates = Vec::with_capacity(candidates.len());
            for (index, candidate) in candidates.iter().enumerate() {
                let sub = build_expr(
                    candidate,
                    &path.join(GateStep::SlotCandidate(index)),
                    ctx,
                    instances,
                    node_paths,
                )?;
                built_candidates.push((Arc::<str>::from(format!("candidate-{index}")), sub));
            }
            let mut graph = Graph::new();
            let slot = DynamicSlot::new(contract_signature, context_policy);
            graph.add_dynamic_boxed(
                "slot",
                fresh_selector(&shared_selector),
                built_candidates,
                slot,
            );
            let slot_id = graph
                .last_node()
                .expect("add_dynamic_boxed just added the slot node");
            node_paths.insert(slot_id, path.clone());
            Ok(graph)
        }
    }
}

/// Resolves one plan-level `Wire` against the block instances already built,
/// calling the reading block's factory to perform the typed read.
#[expect(
    clippy::result_large_err,
    reason = "Refusal carries the detail a planner needs to act on; boxing it would only move the cost to every caller"
)]
fn wire_one(
    wire: &WirePlan,
    path: &GatePath,
    instances: &BlockInstances,
    graph: &mut Graph,
) -> Result<(), Refusal> {
    let Some((reader_id, reader_factory)) = instances.get(wire.reader()) else {
        return Err(Refusal::new(
            codes::BAD_WIRE,
            path.clone(),
            Some(RefusedKey::Named(wire.reader().to_string())),
            "a block instance built earlier in the plan",
            format!("unknown reader instance {}", wire.reader()),
            "wire from/to instance labels that appear as a Step, Guard step, or Fork reducer in the plan",
        ));
    };
    let Some((producer_id, _)) = instances.get(wire.producer()) else {
        return Err(Refusal::new(
            codes::BAD_WIRE,
            path.clone(),
            Some(RefusedKey::Named(wire.producer().to_string())),
            "a block instance built earlier in the plan",
            format!("unknown producer instance {}", wire.producer()),
            "wire from/to instance labels that appear as a Step, Guard step, or Fork reducer in the plan",
        ));
    };
    reader_factory
        .wire_input(graph, reader_id.clone(), producer_id.clone(), wire.port())
        .map_err(|error| {
            Refusal::new(
                codes::BAD_WIRE,
                path.clone(),
                Some(RefusedKey::Named(wire.port().to_string())),
                "a port this reader block accepts an input on",
                error.to_string(),
                "wire this port to a block that supplies a compatible output, or remove the wire",
            )
        })
}

/// Walks the plan tree looking for an `Until` loop whose predicate reads an
/// input that is never guaranteed before the loop is reached, and that the
/// target contract does not itself advertise as an externally supplied
/// input.
///
/// The native graph builder does not catch this statically: a loop
/// predicate's input is checked at run start, not at `Graph::validate` time,
/// so an unguarded plan would only fail once it runs. `built_requires_outputs`
/// and `target_requires_outputs` come from the fully materialized graph and
/// the resolved target contract, respectively.
fn check_loop_initializers(
    expr: &GateExpr,
    path: &GatePath,
    catalogue: &CompositionCatalogueSnapshot,
    built_requires_outputs: &[Access],
    target_requires_outputs: &[Access],
    refusals: &mut Vec<Refusal>,
) {
    match expr {
        GateExpr::Step(_) | GateExpr::Guard { .. } => {}
        GateExpr::Seq(children) => {
            for (index, child) in children.iter().enumerate() {
                check_loop_initializers(
                    child,
                    &path.join(GateStep::Seq(index)),
                    catalogue,
                    built_requires_outputs,
                    target_requires_outputs,
                    refusals,
                );
            }
        }
        GateExpr::Choose {
            when_true,
            when_false,
            ..
        } => {
            check_loop_initializers(
                when_true,
                &path.join(GateStep::ChooseTrue),
                catalogue,
                built_requires_outputs,
                target_requires_outputs,
                refusals,
            );
            check_loop_initializers(
                when_false,
                &path.join(GateStep::ChooseFalse),
                catalogue,
                built_requires_outputs,
                target_requires_outputs,
                refusals,
            );
        }
        GateExpr::Switch { cases, default, .. } => {
            for (index, case) in cases.iter().enumerate() {
                check_loop_initializers(
                    case.body(),
                    &path.join(GateStep::SwitchCase(index)),
                    catalogue,
                    built_requires_outputs,
                    target_requires_outputs,
                    refusals,
                );
            }
            if let Some(default) = default {
                check_loop_initializers(
                    default,
                    &path.join(GateStep::SwitchDefault),
                    catalogue,
                    built_requires_outputs,
                    target_requires_outputs,
                    refusals,
                );
            }
        }
        GateExpr::Fork { arms, .. } => {
            for (index, arm) in arms.iter().enumerate() {
                check_loop_initializers(
                    arm,
                    &path.join(GateStep::ForkArm(index)),
                    catalogue,
                    built_requires_outputs,
                    target_requires_outputs,
                    refusals,
                );
            }
        }
        GateExpr::Loop { body, control, .. } => {
            if let LoopControl::Until(predicate_key) = control
                && let Some(shared) = catalogue.predicate(predicate_key)
            {
                let type_id = shared.input_type_id();
                let missing_on_entry = built_requires_outputs
                    .iter()
                    .any(|access| access.type_id == type_id);
                let advertised_by_target = target_requires_outputs
                    .iter()
                    .any(|access| access.type_id == type_id);
                if missing_on_entry && !advertised_by_target {
                    refusals.push(Refusal::new(
                        codes::MISSING_LOOP_INITIALIZER,
                        path.clone(),
                        Some(RefusedKey::Named(predicate_key.to_string())),
                        "an input produced before this loop starts",
                        format!(
                            "{} is never guaranteed before the loop's first check",
                            shared.input_type_name()
                        ),
                        "produce this input earlier in the plan, or change the loop's control to a fixed count",
                    ));
                }
            }
            check_loop_initializers(
                body,
                &path.join(GateStep::LoopBody),
                catalogue,
                built_requires_outputs,
                target_requires_outputs,
                refusals,
            );
        }
        GateExpr::Repeat { body, control, .. } => {
            // Unlike a bare Loop, a Repeat's guaranteed leading run can
            // itself produce the predicate's input as a normal, guaranteed
            // in-graph dependency rather than a free/backward read, so this
            // is not a false positive for the common do-while case; it only
            // fires when nothing reachable before the derived loop's check
            // — including the leading run — actually guarantees the input.
            if let LoopControl::Until(predicate_key) = control
                && let Some(shared) = catalogue.predicate(predicate_key)
            {
                let type_id = shared.input_type_id();
                let missing_on_entry = built_requires_outputs
                    .iter()
                    .any(|access| access.type_id == type_id);
                let advertised_by_target = target_requires_outputs
                    .iter()
                    .any(|access| access.type_id == type_id);
                if missing_on_entry && !advertised_by_target {
                    refusals.push(Refusal::new(
                        codes::MISSING_LOOP_INITIALIZER,
                        path.clone(),
                        Some(RefusedKey::Named(predicate_key.to_string())),
                        "an input produced before this loop starts",
                        format!(
                            "{} is never guaranteed before the loop's first check",
                            shared.input_type_name()
                        ),
                        "produce this input earlier in the plan or in the Repeat's own body, or change the loop's control to a fixed count",
                    ));
                }
            }
            check_loop_initializers(
                body,
                &path.join(GateStep::RepeatBody),
                catalogue,
                built_requires_outputs,
                target_requires_outputs,
                refusals,
            );
        }
        GateExpr::Seal { body, .. } => {
            check_loop_initializers(
                body,
                &path.join(GateStep::SealBody),
                catalogue,
                built_requires_outputs,
                target_requires_outputs,
                refusals,
            );
        }
        GateExpr::Slot { candidates, .. } => {
            for (index, candidate) in candidates.iter().enumerate() {
                check_loop_initializers(
                    candidate,
                    &path.join(GateStep::SlotCandidate(index)),
                    catalogue,
                    built_requires_outputs,
                    target_requires_outputs,
                    refusals,
                );
            }
        }
    }
}

/// Folds every reachable `Slot`'s contract `requires`/`requires_outputs` into
/// `requires`/`requires_outputs`, so the target-contract check in
/// [`materialize`] can see them.
///
/// Native `Graph::signature` derives `requires` and `requires_outputs` by
/// walking reachable nodes, but a `Dynamic` node (what `Slot` materializes
/// to) contributes nothing on either axis: the admitted candidate's actual
/// reads are opaque to the enclosing graph's derived signature, unlike a
/// declared-contract `Seal` scope, whose contract is folded in. A slot
/// contract's own `requires`/`requires_outputs` describe exactly what an
/// admitted candidate is allowed to read, so the materializer folds them in
/// here instead of leaving that boundary's resource needs invisible to the
/// target check.
fn fold_slot_contract_accesses(
    expr: &GateExpr,
    ctx: &BuildCtx<'_>,
    requires: &mut Vec<Access>,
    requires_outputs: &mut Vec<Access>,
) {
    match expr {
        GateExpr::Step(_) | GateExpr::Guard { .. } => {}
        GateExpr::Seq(children) => {
            for child in children {
                fold_slot_contract_accesses(child, ctx, requires, requires_outputs);
            }
        }
        GateExpr::Choose {
            when_true,
            when_false,
            ..
        } => {
            fold_slot_contract_accesses(when_true, ctx, requires, requires_outputs);
            fold_slot_contract_accesses(when_false, ctx, requires, requires_outputs);
        }
        GateExpr::Switch { cases, default, .. } => {
            for case in cases {
                fold_slot_contract_accesses(case.body(), ctx, requires, requires_outputs);
            }
            if let Some(default) = default {
                fold_slot_contract_accesses(default, ctx, requires, requires_outputs);
            }
        }
        GateExpr::Fork { arms, .. } => {
            for arm in arms {
                fold_slot_contract_accesses(arm, ctx, requires, requires_outputs);
            }
        }
        GateExpr::Loop { body, .. }
        | GateExpr::Seal { body, .. }
        | GateExpr::Repeat { body, .. } => {
            fold_slot_contract_accesses(body, ctx, requires, requires_outputs);
        }
        GateExpr::Slot {
            contract,
            candidates,
            ..
        } => {
            if let Ok(signature) = resolve_contract(ctx.contracts, contract, &GatePath::root()) {
                requires.extend(signature.requires().iter().cloned());
                requires_outputs.extend(signature.requires_outputs().iter().cloned());
            }
            for candidate in candidates {
                fold_slot_contract_accesses(candidate, ctx, requires, requires_outputs);
            }
        }
    }
}

/// Elements of `from` (by `(type_id, mode)`) not present in `remove`. Mirrors
/// native `GraphSignature`'s private helper of the same name.
fn access_difference(from: &[Access], remove: &[Access]) -> Vec<Access> {
    from.iter()
        .filter(|access| {
            !remove
                .iter()
                .any(|other| access.type_id == other.type_id && access.mode == other.mode)
        })
        .cloned()
        .collect()
}

/// Elements of `from` whose type is not present in `remove`, ignoring access
/// mode. Mirrors native `GraphSignature`'s private helper of the same name.
fn access_type_difference(from: &[Access], remove: &[Access]) -> Vec<Access> {
    from.iter()
        .filter(|access| !remove.iter().any(|other| access.type_id == other.type_id))
        .cloned()
        .collect()
}

fn describe_access(list: &[Access]) -> String {
    list.iter()
        .map(|access| access.type_name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A predicate that delegates to a shared, catalogue-owned handle.
///
/// Native builder methods consume a `BoxedPredicate` by value, but a plan may
/// reference the same registered predicate at more than one tree position
/// (within one plan, or across plans built against the same sealed
/// catalogue). This wrapper mints a fresh owned `BoxedPredicate` per
/// reference from one shared `Arc`-backed registration, rather than
/// requiring the catalogue to hand out an owned predicate it can never get
/// back.
struct SharedPredicate(Arc<BoxedPredicate>);

impl ErasedPredicate for SharedPredicate {
    fn evaluate(&self, ctx: &SystemContext<'_>) -> Result<bool, PredicateError> {
        self.0.evaluate(ctx)
    }

    fn input_type_id(&self) -> TypeId {
        self.0.input_type_id()
    }

    fn input_type_name(&self) -> &'static str {
        self.0.input_type_name()
    }
}

fn fresh_predicate(shared: &Arc<BoxedPredicate>) -> BoxedPredicate {
    let boxed: Box<dyn ErasedPredicate> = Box::new(SharedPredicate(Arc::clone(shared)));
    boxed.into()
}

/// A discriminator that delegates to a shared, catalogue-owned handle. See
/// [`SharedPredicate`] for why this indirection exists.
struct SharedDiscriminator(Arc<BoxedDiscriminator>);

impl ErasedDiscriminator for SharedDiscriminator {
    fn discriminate(&self, ctx: &SystemContext<'_>) -> Result<NodeName, PredicateError> {
        self.0.discriminate(ctx)
    }

    fn input_type_id(&self) -> TypeId {
        self.0.input_type_id()
    }

    fn input_type_name(&self) -> &'static str {
        self.0.input_type_name()
    }
}

fn fresh_discriminator(shared: &Arc<BoxedDiscriminator>) -> BoxedDiscriminator {
    let boxed: Box<dyn ErasedDiscriminator> = Box::new(SharedDiscriminator(Arc::clone(shared)));
    boxed.into()
}

/// A selector that delegates to a shared, catalogue-owned handle. See
/// [`SharedPredicate`] for why this indirection exists.
struct SharedSelector(Arc<BoxedSelector>);

impl ErasedSelector for SharedSelector {
    fn select(&self, ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError> {
        self.0.select(ctx)
    }

    fn input_type_name(&self) -> &'static str {
        self.0.input_type_name()
    }
}

fn fresh_selector(shared: &Arc<BoxedSelector>) -> BoxedSelector {
    Box::new(SharedSelector(Arc::clone(shared)))
}
