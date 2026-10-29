//! The gate-expression plan tree a planner submits for materialization.
//!
//! A [`GatePlan`] never carries code, closures, serialized graphs, node IDs,
//! or `TypeId`s — only keys into a sealed catalogue plus data configuration.
//! The host resolves those keys, builds an ordinary native `Graph`, and
//! validates it; see [`crate::composition::materializer`].

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::identity::{
    BlockInstanceId, CaseKey, ConfigId, ContextPolicyKey, DiscriminatorKey, HandlerKey,
    PredicateKey, RetentionPolicyKey, RetryPolicyKey, StablePortKey, TimeoutPolicyKey,
};
use crate::contracts::{
    CanonicalRecord, CompositionBlockKey, CompositionCatalogKey, ContentDigest, ContractError,
    ResolvedConfigurationRef, SchemaVersion, SelectorKey, content_digest,
};
use crate::topology::TopologyContractKey;

/// One planner-named block instance: which registered block runs, and with
/// which resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepPlan {
    instance: BlockInstanceId,
    block: CompositionBlockKey,
    config: ConfigId,
}

impl StepPlan {
    /// Names one block instance and the configuration key it resolves to.
    #[must_use]
    pub const fn new(
        instance: BlockInstanceId,
        block: CompositionBlockKey,
        config: ConfigId,
    ) -> Self {
        Self {
            instance,
            block,
            config,
        }
    }

    /// Returns the plan-local instance label.
    #[must_use]
    pub const fn instance(&self) -> &BlockInstanceId {
        &self.instance
    }

    /// Returns the registered block identity and version.
    #[must_use]
    pub const fn block(&self) -> &CompositionBlockKey {
        &self.block
    }

    /// Returns the key into the plan's configuration table.
    #[must_use]
    pub const fn config(&self) -> &ConfigId {
        &self.config
    }
}

/// One labeled case body in a `Switch` gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchCase {
    key: CaseKey,
    body: GateExpr,
}

impl SwitchCase {
    /// Pairs a discriminator case label with the sub-expression it selects.
    #[must_use]
    pub fn new(key: CaseKey, body: GateExpr) -> Self {
        Self { key, body }
    }

    /// Returns the case label.
    #[must_use]
    pub const fn key(&self) -> &CaseKey {
        &self.key
    }

    /// Returns the sub-expression this case selects.
    #[must_use]
    pub const fn body(&self) -> &GateExpr {
        &self.body
    }
}

/// How a `Loop` gate decides to stop iterating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoopControl {
    /// Iterates until a registered predicate is satisfied.
    ///
    /// The predicate is checked before every iteration, including the first,
    /// so its input must already be produced before the loop starts.
    Until(PredicateKey),
    /// Iterates a fixed number of times, known at plan time.
    Count(u16),
}

/// A plan-level wiring relation from a producer's output to a reader's input,
/// named by a registered stable port rather than a Rust type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePlan {
    reader: BlockInstanceId,
    producer: BlockInstanceId,
    port: StablePortKey,
}

impl WirePlan {
    /// Names one reader/producer relation over a registered stable port.
    #[must_use]
    pub const fn new(
        reader: BlockInstanceId,
        producer: BlockInstanceId,
        port: StablePortKey,
    ) -> Self {
        Self {
            reader,
            producer,
            port,
        }
    }

    /// Returns the reading block instance.
    #[must_use]
    pub const fn reader(&self) -> &BlockInstanceId {
        &self.reader
    }

    /// Returns the producing block instance.
    #[must_use]
    pub const fn producer(&self) -> &BlockInstanceId {
        &self.producer
    }

    /// Returns the registered stable port identity carrying the value.
    #[must_use]
    pub const fn port(&self) -> &StablePortKey {
        &self.port
    }
}

/// One node in the gate-expression tree a planner submits.
///
/// Every variant names catalogue keys and nested expressions only; blocks,
/// predicates, discriminators, reducers, handlers, and selectors are all
/// resolved later, against a sealed catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateExpr {
    /// Runs one registered block.
    Step(StepPlan),
    /// Runs children in order.
    Seq(Vec<GateExpr>),
    /// Binary branch over a registered predicate.
    Choose {
        /// The registered predicate deciding the branch.
        predicate: PredicateKey,
        /// The sub-expression run when the predicate holds.
        when_true: Box<GateExpr>,
        /// The sub-expression run when the predicate does not hold.
        when_false: Box<GateExpr>,
    },
    /// Branches over a registered discriminator's cases.
    Switch {
        /// The registered discriminator producing a case key.
        discriminator: DiscriminatorKey,
        /// The case bodies this plan covers.
        cases: Vec<SwitchCase>,
        /// The sub-expression run when no case key matches.
        default: Option<Box<GateExpr>>,
    },
    /// Runs arms concurrently and reduces their outputs with a registered
    /// reducer block. Arm width is fixed at plan time.
    Fork {
        /// The concurrent arms.
        arms: Vec<GateExpr>,
        /// The registered block reducing arm outputs after they all finish.
        reducer: StepPlan,
    },
    /// Repeats a body under a termination control and retention policy.
    Loop {
        /// The repeated sub-expression.
        body: Box<GateExpr>,
        /// How the loop decides to stop.
        control: LoopControl,
        /// The registered policy governing which iterations' outputs are kept.
        retention: RetentionPolicyKey,
    },
    /// A derived do-while gate: runs `body` once unconditionally, then
    /// continues under the same termination control a bare `Loop` would use.
    ///
    /// A bare `Loop`'s guaranteed production is always empty, since a
    /// `Loop::Until` may run its body zero times when the predicate already
    /// holds on entry. `Repeat` exists for the common case where the body
    /// must run at least once: it materializes as `Seq(body, Loop(control;
    /// body))`, so the whole gate's guaranteed production is exactly the
    /// body's own, credited by the leading, unconditional run.
    ///
    /// `control` governs how many further times the body repeats *after*
    /// that leading run, so `Repeat { control: Count(3), .. }` runs the body
    /// four times in total, and `Repeat { control: Count(0), .. }` runs it
    /// exactly once. Because the body is built twice (the leading run and the
    /// derived loop's own body are two independent native subgraphs), a
    /// plan-level `Wire` may only name instances that are both inside this
    /// body — resolved separately in each copy, against that copy's own
    /// instances — or both outside it; a `Wire` crossing the boundary is
    /// refused: see [`GatePlan::new`].
    Repeat {
        /// The sub-expression guaranteed to run at least once.
        body: Box<GateExpr>,
        /// How the derived loop decides to stop repeating after the first run.
        control: LoopControl,
        /// The registered policy governing which iterations' outputs are kept.
        retention: RetentionPolicyKey,
    },
    /// Runs one step guarded by registered recovery recipes.
    ///
    /// `on_error` and `on_timeout` each name a registered recipe — itself a
    /// `GateExpr`, most simply a `Seq` of steps — not a single block. Prefer
    /// `Choose` over `Guard` for an ordinary pass/fail branch: `Guard`'s
    /// barrier withholds guaranteed production credit for whatever the
    /// handler recipe produces.
    Guard {
        /// The guarded step.
        step: StepPlan,
        /// The registered recipe run when the step errors.
        on_error: HandlerKey,
        /// The registered policy (duration plus, where it names a subgraph,
        /// a handler recipe) applied when the step times out.
        on_timeout: TimeoutPolicyKey,
        /// An optional registered retry policy applied before falling back
        /// to `on_error`.
        retry: Option<RetryPolicyKey>,
    },
    /// Wraps a body as a declared scope boundary with a stated contract.
    ///
    /// A declared scope's subset check is against the body's *guaranteed*
    /// production, not a flat union over every reachable node — the only
    /// sound way to check a branch-shaped body against a contract. See
    /// [`crate::composition::materializer`].
    Seal {
        /// The wrapped sub-expression.
        body: Box<GateExpr>,
        /// The contract the scope boundary must satisfy.
        contract: TopologyContractKey,
        /// The registered context policy applied at the scope boundary.
        policy: ContextPolicyKey,
    },
    /// Selects one candidate body at build time against a stated contract.
    ///
    /// The native graph builder credits a `Slot`'s guaranteed production as
    /// its stated contract's `produces`, the same as any other declared
    /// boundary — a `Slot` nested under a `Seal` can satisfy the outer
    /// contract through the slot alone.
    Slot {
        /// The contract every candidate must satisfy.
        contract: TopologyContractKey,
        /// The candidate sub-expressions.
        candidates: Vec<GateExpr>,
        /// The registered selector choosing among admissible candidates.
        selector: SelectorKey,
        /// The registered context policy applied around the selected
        /// candidate.
        ///
        /// A candidate that reads a resource named in the slot's contract can
        /// only see it at run time if this policy actually crosses that
        /// resource; naming the wrong policy here is a plan defect, not a
        /// runtime surprise the boundary should paper over.
        policy: ContextPolicyKey,
    },
}

impl GateExpr {
    /// Checks structural well-formedness (no empty `Seq`/`Switch`/`Fork`/
    /// `Slot`, no zero-count `Loop`), independent of any catalogue.
    pub(super) fn validate_shape(&self) -> Result<(), ContractError> {
        match self {
            Self::Step(_) => Ok(()),
            Self::Seq(children) => {
                if children.is_empty() {
                    return Err(ContractError::new(
                        "plan.empty-seq",
                        "a Seq gate requires at least one child",
                    ));
                }
                children.iter().try_for_each(Self::validate_shape)
            }
            Self::Choose {
                when_true,
                when_false,
                ..
            } => {
                when_true.validate_shape()?;
                when_false.validate_shape()
            }
            Self::Switch { cases, default, .. } => {
                if cases.is_empty() && default.is_none() {
                    return Err(ContractError::new(
                        "plan.empty-switch",
                        "a Switch gate requires at least one case or a default",
                    ));
                }
                cases
                    .iter()
                    .try_for_each(|case| case.body.validate_shape())?;
                default.as_deref().map_or(Ok(()), GateExpr::validate_shape)
            }
            Self::Fork { arms, .. } => {
                if arms.is_empty() {
                    return Err(ContractError::new(
                        "plan.empty-fork",
                        "a Fork gate requires at least one arm",
                    ));
                }
                arms.iter().try_for_each(Self::validate_shape)
            }
            Self::Loop { body, control, .. } => {
                if let LoopControl::Count(0) = control {
                    return Err(ContractError::new(
                        "plan.zero-loop-count",
                        "a counted Loop gate requires at least one iteration",
                    ));
                }
                body.validate_shape()
            }
            Self::Repeat { body, .. } => body.validate_shape(),
            Self::Guard { .. } => Ok(()),
            Self::Seal { body, .. } => body.validate_shape(),
            Self::Slot { candidates, .. } => {
                if candidates.is_empty() {
                    return Err(ContractError::new(
                        "plan.empty-slot",
                        "a Slot gate requires at least one candidate",
                    ));
                }
                candidates.iter().try_for_each(Self::validate_shape)
            }
        }
    }

    /// Checks that no `Step`, `Guard` step, or `Fork` reducer reuses another
    /// one's plan-local instance label, independent of any catalogue.
    ///
    /// Every instance label shares one flat namespace across the whole plan,
    /// including mutually exclusive `Choose`/`Switch`/`Slot` branches: the
    /// materializer resolves `Wire`s against a single table keyed by label,
    /// built by walking the whole tree, so a label reused anywhere in the
    /// tree — even on branches that can never both run — would let that
    /// table silently keep whichever occurrence was built last and resolve
    /// a `Wire` to the wrong node instead of being refused.
    pub(super) fn check_unique_instances(&self) -> Result<(), ContractError> {
        let mut seen = HashSet::new();
        self.collect_instances(&mut seen)
    }

    fn collect_instances<'a>(
        &'a self,
        seen: &mut HashSet<&'a BlockInstanceId>,
    ) -> Result<(), ContractError> {
        match self {
            Self::Step(step) => note_instance(seen, step.instance()),
            Self::Seq(children) => children
                .iter()
                .try_for_each(|child| child.collect_instances(seen)),
            Self::Choose {
                when_true,
                when_false,
                ..
            } => {
                when_true.collect_instances(seen)?;
                when_false.collect_instances(seen)
            }
            Self::Switch { cases, default, .. } => {
                cases
                    .iter()
                    .try_for_each(|case| case.body.collect_instances(seen))?;
                default
                    .as_deref()
                    .map_or(Ok(()), |body| body.collect_instances(seen))
            }
            Self::Fork { arms, reducer } => {
                arms.iter()
                    .try_for_each(|arm| arm.collect_instances(seen))?;
                note_instance(seen, reducer.instance())
            }
            Self::Loop { body, .. } | Self::Repeat { body, .. } => body.collect_instances(seen),
            Self::Guard { step, .. } => note_instance(seen, step.instance()),
            Self::Seal { body, .. } => body.collect_instances(seen),
            Self::Slot { candidates, .. } => candidates
                .iter()
                .try_for_each(|candidate| candidate.collect_instances(seen)),
        }
    }

    /// Collects every plan-local instance label within `self`.
    ///
    /// Exposed at `pub(crate)` visibility so `effects::check` can tell
    /// whether a step reads a wired input from another instance built fresh
    /// within the same `Loop`/`Repeat` body. Instance-label uniqueness is
    /// validated separately by [`GatePlan::new`], so this ignores the
    /// duplicate-label error `collect_instances` would otherwise return.
    #[must_use]
    pub(crate) fn instance_labels(&self) -> HashSet<&BlockInstanceId> {
        let mut seen = HashSet::new();
        let _ = self.collect_instances(&mut seen);
        seen
    }

    /// Collects the plan-local instance labels that lie within any `Repeat`'s
    /// body, anywhere in the tree, merged into one set.
    ///
    /// A `Repeat` builds its body twice (see [`GateExpr::Repeat`]): the lead
    /// build populates the plan's shared instance table, the loop-interior
    /// build uses an isolated, throwaway one. [`materializer`](super) uses
    /// this merged set to tell which plan-level `Wire`s it already resolved
    /// locally, once per `Repeat`, instead of through the top-level wire pass
    /// that only ever sees the shared table.
    #[must_use]
    pub(crate) fn repeat_body_instance_labels(&self) -> HashSet<&BlockInstanceId> {
        let mut out = HashSet::new();
        self.collect_repeat_body_instances(&mut out);
        out
    }

    fn collect_repeat_body_instances<'a>(&'a self, out: &mut HashSet<&'a BlockInstanceId>) {
        match self {
            Self::Step(_) | Self::Guard { .. } => {}
            Self::Seq(children) => {
                for child in children {
                    child.collect_repeat_body_instances(out);
                }
            }
            Self::Choose {
                when_true,
                when_false,
                ..
            } => {
                when_true.collect_repeat_body_instances(out);
                when_false.collect_repeat_body_instances(out);
            }
            Self::Switch { cases, default, .. } => {
                for case in cases {
                    case.body.collect_repeat_body_instances(out);
                }
                if let Some(default) = default {
                    default.collect_repeat_body_instances(out);
                }
            }
            Self::Fork { arms, .. } => {
                for arm in arms {
                    arm.collect_repeat_body_instances(out);
                }
            }
            Self::Loop { body, .. } => body.collect_repeat_body_instances(out),
            Self::Repeat { body, .. } => {
                // Uniqueness across the whole plan is already validated by
                // the time this runs, so every label this finds is fresh.
                let _ = body.collect_instances(out);
                body.collect_repeat_body_instances(out);
            }
            Self::Seal { body, .. } => body.collect_repeat_body_instances(out),
            Self::Slot { candidates, .. } => {
                for candidate in candidates {
                    candidate.collect_repeat_body_instances(out);
                }
            }
        }
    }

    /// Collects one instance-label set per `Repeat` node in the tree, kept
    /// separate rather than merged.
    ///
    /// Used by [`GatePlan::check_wires_outside_repeat_bodies`] to tell a wire
    /// that crosses a `Repeat` body's boundary (refused: exactly one endpoint
    /// is inside that body) apart from one that stays inside a single body
    /// (allowed: both endpoints are inside the same body). A flat, merged set
    /// cannot make that distinction between sibling or nested `Repeat`s.
    fn collect_repeat_body_label_sets<'a>(&'a self, out: &mut Vec<HashSet<&'a BlockInstanceId>>) {
        match self {
            Self::Step(_) | Self::Guard { .. } => {}
            Self::Seq(children) => {
                for child in children {
                    child.collect_repeat_body_label_sets(out);
                }
            }
            Self::Choose {
                when_true,
                when_false,
                ..
            } => {
                when_true.collect_repeat_body_label_sets(out);
                when_false.collect_repeat_body_label_sets(out);
            }
            Self::Switch { cases, default, .. } => {
                for case in cases {
                    case.body.collect_repeat_body_label_sets(out);
                }
                if let Some(default) = default {
                    default.collect_repeat_body_label_sets(out);
                }
            }
            Self::Fork { arms, .. } => {
                for arm in arms {
                    arm.collect_repeat_body_label_sets(out);
                }
            }
            Self::Loop { body, .. } => body.collect_repeat_body_label_sets(out),
            Self::Repeat { body, .. } => {
                out.push(body.instance_labels());
                body.collect_repeat_body_label_sets(out);
            }
            Self::Seal { body, .. } => body.collect_repeat_body_label_sets(out),
            Self::Slot { candidates, .. } => {
                for candidate in candidates {
                    candidate.collect_repeat_body_label_sets(out);
                }
            }
        }
    }

    /// Collects the instance labels directly inside `self`, not further
    /// inside any nested `Repeat`'s own body.
    ///
    /// Exposed at `pub(crate)` visibility so the materializer can tell which
    /// `Repeat`'s own doubled build a same-body wire belongs to when
    /// `Repeat`s nest: a wire fully inside an inner `Repeat`'s body is
    /// resolved there, once per its own two copies, not again by an
    /// enclosing `Repeat`.
    #[must_use]
    pub(crate) fn direct_instance_labels(&self) -> HashSet<&BlockInstanceId> {
        let mut out = HashSet::new();
        self.collect_direct_instances(&mut out);
        out
    }

    fn collect_direct_instances<'a>(&'a self, out: &mut HashSet<&'a BlockInstanceId>) {
        match self {
            Self::Step(step) => {
                out.insert(step.instance());
            }
            Self::Seq(children) => {
                for child in children {
                    child.collect_direct_instances(out);
                }
            }
            Self::Choose {
                when_true,
                when_false,
                ..
            } => {
                when_true.collect_direct_instances(out);
                when_false.collect_direct_instances(out);
            }
            Self::Switch { cases, default, .. } => {
                for case in cases {
                    case.body.collect_direct_instances(out);
                }
                if let Some(default) = default {
                    default.collect_direct_instances(out);
                }
            }
            Self::Fork { arms, reducer } => {
                for arm in arms {
                    arm.collect_direct_instances(out);
                }
                out.insert(reducer.instance());
            }
            Self::Loop { body, .. } => body.collect_direct_instances(out),
            // A nested Repeat's body is its own scope: its labels are not
            // direct members of the enclosing expression's scope.
            Self::Repeat { .. } => {}
            Self::Guard { step, .. } => {
                out.insert(step.instance());
            }
            Self::Seal { body, .. } => body.collect_direct_instances(out),
            Self::Slot { candidates, .. } => {
                for candidate in candidates {
                    candidate.collect_direct_instances(out);
                }
            }
        }
    }
}

fn note_instance<'a>(
    seen: &mut HashSet<&'a BlockInstanceId>,
    instance: &'a BlockInstanceId,
) -> Result<(), ContractError> {
    if seen.insert(instance) {
        Ok(())
    } else {
        Err(ContractError::new(
            "plan.duplicate-instance",
            format!("block instance {instance} is used more than once in this plan"),
        ))
    }
}

/// A canonical, versioned gate plan a planner submits for materialization.
///
/// Limits (depth, instances, arms, nested loops, loop iterations, nodes) are
/// host policy, not part of the plan: the host applies its own
/// [`CompositionLimits`](super::CompositionLimits) when it decides whether to
/// materialize a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePlan {
    schema_version: SchemaVersion,
    catalogue: CompositionCatalogKey,
    catalogue_digest: ContentDigest,
    target: TopologyContractKey,
    expression: GateExpr,
    wires: Vec<WirePlan>,
    configurations: BTreeMap<ConfigId, ResolvedConfigurationRef>,
    plan_digest: ContentDigest,
}

impl GatePlan {
    /// Creates a gate plan and computes its canonical digest.
    pub fn new(
        catalogue: CompositionCatalogKey,
        catalogue_digest: ContentDigest,
        target: TopologyContractKey,
        expression: GateExpr,
        wires: Vec<WirePlan>,
        configurations: BTreeMap<ConfigId, ResolvedConfigurationRef>,
    ) -> Result<Self, ContractError> {
        let mut plan = Self {
            schema_version: SchemaVersion::V1,
            catalogue,
            catalogue_digest,
            target,
            expression,
            wires,
            configurations,
            plan_digest: ContentDigest::sha256(&[]),
        };
        plan.validate_structure()?;
        plan.plan_digest = content_digest(&plan.identity_view())?;
        Ok(plan)
    }

    /// Returns the sealed catalogue identity and version this plan resolves
    /// its keys against.
    #[must_use]
    pub const fn catalogue(&self) -> &CompositionCatalogKey {
        &self.catalogue
    }

    /// Returns the immutable snapshot digest of that catalogue.
    #[must_use]
    pub const fn catalogue_digest(&self) -> ContentDigest {
        self.catalogue_digest
    }

    /// Returns the contract the materialized graph must satisfy.
    #[must_use]
    pub const fn target(&self) -> &TopologyContractKey {
        &self.target
    }

    /// Returns the root of the gate-expression tree.
    #[must_use]
    pub const fn expression(&self) -> &GateExpr {
        &self.expression
    }

    /// Returns the plan-level wiring relations.
    #[must_use]
    pub fn wires(&self) -> &[WirePlan] {
        &self.wires
    }

    /// Returns the resolved configuration table, keyed by [`ConfigId`].
    #[must_use]
    pub const fn configurations(&self) -> &BTreeMap<ConfigId, ResolvedConfigurationRef> {
        &self.configurations
    }

    /// Returns the canonical plan identity.
    #[must_use]
    pub const fn plan_digest(&self) -> ContentDigest {
        self.plan_digest
    }

    /// Returns the wire schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.expression.validate_shape()?;
        self.expression.check_unique_instances()?;
        self.check_no_duplicate_wire_targets()?;
        self.check_wires_outside_repeat_bodies()?;
        for configuration in self.configurations.values() {
            configuration.validate()?;
        }
        Ok(())
    }

    /// Refuses two `Wire`s that name the same reader and port.
    ///
    /// A second native `read_from` binding for the same reader and output
    /// type replaces the first rather than adding to it (see
    /// [`crate::composition::materializer`]), so only one of two such wires
    /// ever takes effect. Accepting both would let an earlier wire's producer
    /// satisfy the freshness check while a later, effective wire silently
    /// reads a different, possibly stale, producer instead.
    fn check_no_duplicate_wire_targets(&self) -> Result<(), ContractError> {
        let mut seen = HashSet::new();
        for wire in &self.wires {
            if !seen.insert((wire.reader(), wire.port())) {
                return Err(ContractError::new(
                    "plan.duplicate-wire",
                    format!(
                        "more than one wire targets {} on port {}; only one binding can take effect, so the plan is ambiguous — keep one wire",
                        wire.reader(),
                        wire.port(),
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Refuses any `Wire` that crosses a `Repeat` body's boundary: one
    /// endpoint inside the body, the other outside it, or inside a different
    /// `Repeat`'s body. A wire whose producer and reader are both inside the
    /// same `Repeat` body is allowed: see [`GateExpr::Repeat`] and
    /// [`GateExpr::collect_repeat_body_label_sets`].
    fn check_wires_outside_repeat_bodies(&self) -> Result<(), ContractError> {
        let mut label_sets = Vec::new();
        self.expression
            .collect_repeat_body_label_sets(&mut label_sets);
        for wire in &self.wires {
            for labels in &label_sets {
                if labels.contains(wire.reader()) != labels.contains(wire.producer()) {
                    return Err(ContractError::new(
                        "plan.wire-into-repeat-body",
                        format!(
                            "wire from {} to {} crosses a Repeat body's boundary; only a wire whose producer and reader are both built inside the same Repeat body is allowed",
                            wire.producer(),
                            wire.reader(),
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn identity_view(&self) -> GatePlanIdentity<'_> {
        GatePlanIdentity {
            schema_version: self.schema_version,
            catalogue: &self.catalogue,
            catalogue_digest: self.catalogue_digest,
            target: &self.target,
            expression: &self.expression,
            wires: &self.wires,
            configurations: &self.configurations,
        }
    }
}

#[derive(Serialize)]
struct GatePlanIdentity<'a> {
    schema_version: SchemaVersion,
    catalogue: &'a CompositionCatalogKey,
    catalogue_digest: ContentDigest,
    target: &'a TopologyContractKey,
    expression: &'a GateExpr,
    wires: &'a [WirePlan],
    configurations: &'a BTreeMap<ConfigId, ResolvedConfigurationRef>,
}

impl CanonicalRecord for GatePlan {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_structure()?;
        let expected = content_digest(&self.identity_view())?;
        if expected == self.plan_digest {
            Ok(())
        } else {
            Err(ContractError::new(
                "plan.digest-mismatch",
                "gate plan identity does not match its canonical content",
            ))
        }
    }
}
