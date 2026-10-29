//! Fixture blocks, predicates, and catalogue for gate-plan materializer tests.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use polaris::graph::predicate::{BoxedDiscriminator, BoxedPredicate};
use polaris::graph::selector::{BoxedSelector, ErasedSelector};
use polaris::graph::{ContextPolicy, Graph, GraphSignature, NodeId, RetryPolicy};
use polaris::system::param::{Out, Res, ResMut, SystemContext};
use polaris::system::plugin::Version;
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris::system::system::SystemError;

use polaris_cosmos::composition::{
    BlockBuildError, BlockFactory, BlockInstanceId, BlockWireError, CaseKey, CompositionCatalogue,
    CompositionCatalogueSnapshot, ConfigId, ContextPolicyId, ContextPolicyKey, DiscriminatorId,
    DiscriminatorKey, Effect, GateExpr, HandlerId, HandlerKey, PredicateId, PredicateKey,
    RetentionPolicy, RetentionPolicyId, RetentionPolicyKey, RetryPolicyId, RetryPolicyKey,
    StablePortId, StablePortKey, StepPlan, TimeoutPolicyEntry, TimeoutPolicyId, TimeoutPolicyKey,
};
use polaris_cosmos::contracts::{
    CompositionBlockId, CompositionBlockKey, CompositionCatalogId, ConfigurationSchemaId,
    ConfigurationSchemaKey, ConfigurationValue, ResolvedConfigurationRef, SelectorId, SelectorKey,
};
use polaris_cosmos::topology::{
    TopologyCatalog, TopologyCatalogSnapshot, TopologyContract, TopologyContractId,
    TopologyContractKey,
};

/// A seed value produced with no inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seed(pub i32);

/// A seed value doubled by a downstream block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Doubled(pub i32);

async fn emit_positive_seed() -> Seed {
    Seed(1)
}

async fn emit_zero_seed() -> Seed {
    Seed(0)
}

#[system]
async fn double_seed(seed: Out<'_, Seed>) -> Doubled {
    Doubled(seed.0 * 2)
}

async fn mutate_seed() -> Seed {
    Seed(2)
}

/// Increments [`Attempts`] (proving a mutation happened) and doubles a wired
/// [`Seed`] input, so a caller can test the Loop/Repeat fresh-input effects
/// exception against a block that genuinely reads another block's output.
#[system]
async fn mutate_with_input(mut attempts: ResMut<Attempts>, seed: Out<'_, Seed>) -> Doubled {
    attempts.count += 1;
    Doubled(seed.0 * 2)
}

/// Increments [`Attempts`] and produces the new count as a [`Seed`], so a
/// caller can prove exactly how many times a repeated body actually ran.
#[system]
async fn count_seed(mut attempts: ResMut<Attempts>) -> Seed {
    attempts.count += 1;
    Seed(attempts.count as i32)
}

/// Increments [`Attempts`] and doubles a wired, optional [`Seed`] input,
/// tolerating a missing entry as `Doubled(0)`. Native validation only permits
/// a loop's backward wire — one whose producer runs later in the same body,
/// so it can supply only the previous iteration's entry — through
/// `Option<Out<T>>`, since the entry does not exist on the first iteration.
#[system]
async fn mutate_with_optional_input(
    mut attempts: ResMut<Attempts>,
    seed: Option<Out<'_, Seed>>,
) -> Doubled {
    attempts.count += 1;
    Doubled(seed.map_or(0, |seed| seed.0 * 2))
}

/// A seed value produced by the timeout handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutSeed(pub i32);

async fn emit_timeout_seed() -> TimeoutSeed {
    TimeoutSeed(3)
}

/// A resource a `Slot` candidate reads across a shared context boundary,
/// rather than one produced within the plan itself.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    pub tools: Vec<String>,
}

impl LocalResource for ToolRegistry {}

/// A value derived from [`ToolRegistry`], produced by a `Slot` candidate that
/// reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer(pub i32);

#[system]
async fn use_tool_registry(registry: Res<ToolRegistry>) -> Answer {
    Answer(registry.tools.len() as i32)
}

/// Counts calls to [`flaky_once`], shared across a run via the context.
#[derive(Debug, Default)]
pub struct Attempts {
    pub count: u32,
}

impl LocalResource for Attempts {}

#[system]
async fn always_fail(mut attempts: ResMut<Attempts>) -> Result<Seed, SystemError> {
    attempts.count += 1;
    Err(SystemError::ExecutionError(
        "fixture: always fails".to_owned(),
    ))
}

/// Fails on the first call and succeeds with a distinct value after that, so
/// a caller can tell a retry actually ran (this block's own success output)
/// from a fallback that ran because it never did (the error handler's
/// output).
#[system]
async fn flaky_once(mut attempts: ResMut<Attempts>) -> Result<Seed, SystemError> {
    attempts.count += 1;
    if attempts.count == 1 {
        Err(SystemError::ExecutionError(
            "fixture: fails on the first attempt".to_owned(),
        ))
    } else {
        Ok(Seed(2))
    }
}

/// Sleeps well past any fixture timeout before producing a [`Seed`], so a
/// `Guard`'s timeout edge is exercised deterministically rather than by
/// racing a real failure.
async fn slow_step() -> Seed {
    tokio::time::sleep(Duration::from_millis(300)).await;
    Seed(1)
}

/// Builds one system node producing a positive [`Seed`], with no input.
pub struct EmitPositiveSeed;

impl BlockFactory for EmitPositiveSeed {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(emit_positive_seed))
    }
}

/// Builds one system node producing a zero [`Seed`], with no input.
pub struct EmitZeroSeed;

impl BlockFactory for EmitZeroSeed {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(emit_zero_seed))
    }
}

/// Builds one system node doubling a preceding [`Seed`], accepting a wired
/// input on [`seed_port_key`] naming which upstream instance's [`Seed`] to
/// read.
pub struct DoubleSeed;

impl BlockFactory for DoubleSeed {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(double_seed))
    }

    fn wire_input(
        &self,
        graph: &mut Graph,
        reader: NodeId,
        producer: NodeId,
        port: &StablePortKey,
    ) -> Result<(), BlockWireError> {
        if *port == seed_port_key() {
            graph.read_from::<Seed>(reader, producer);
            Ok(())
        } else {
            Err(BlockWireError::unsupported(port))
        }
    }
}

/// Builds one system node producing a [`TimeoutSeed`], with no input — used
/// as a `Guard`'s timeout-handler recipe.
pub struct EmitTimeoutSeed;

impl BlockFactory for EmitTimeoutSeed {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(emit_timeout_seed))
    }
}

/// Builds one system node that always fails.
pub struct AlwaysFailStep;

impl BlockFactory for AlwaysFailStep {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(always_fail))
    }
}

/// Builds one system node that fails once, then succeeds.
pub struct FlakyOnceStep;

impl BlockFactory for FlakyOnceStep {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(flaky_once))
    }
}

/// Builds one system node that sleeps well past any fixture `Guard` timeout.
pub struct SlowStep;

impl BlockFactory for SlowStep {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(slow_step))
    }
}

/// Builds one system node declared mutating and not safe to repeat.
pub struct MutateSeed;

impl BlockFactory for MutateSeed {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(mutate_seed))
    }
}

/// Builds one system node declared mutating and not safe to repeat, that
/// reads a wired [`Seed`] input over [`seed_port_key`].
pub struct MutateWithInput;

impl BlockFactory for MutateWithInput {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(mutate_with_input))
    }

    fn wire_input(
        &self,
        graph: &mut Graph,
        reader: NodeId,
        producer: NodeId,
        port: &StablePortKey,
    ) -> Result<(), BlockWireError> {
        if *port == seed_port_key() {
            graph.read_from::<Seed>(reader, producer);
            Ok(())
        } else {
            Err(BlockWireError::unsupported(port))
        }
    }
}

/// Builds one system node declared mutating and not safe to repeat, that
/// reads an optional wired [`Seed`] input over [`seed_port_key`], tolerating
/// a missing entry.
pub struct MutateWithOptionalInput;

impl BlockFactory for MutateWithOptionalInput {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(mutate_with_optional_input))
    }

    fn wire_input(
        &self,
        graph: &mut Graph,
        reader: NodeId,
        producer: NodeId,
        port: &StablePortKey,
    ) -> Result<(), BlockWireError> {
        if *port == seed_port_key() {
            graph.read_from::<Seed>(reader, producer);
            Ok(())
        } else {
            Err(BlockWireError::unsupported(port))
        }
    }
}

/// Builds one system node incrementing [`Attempts`] and producing the new
/// count as a [`Seed`].
pub struct CountSeedStep;

impl BlockFactory for CountSeedStep {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(count_seed))
    }
}

/// Builds one system node reading [`ToolRegistry`] and producing [`Answer`],
/// used as a `Slot` candidate that needs a resource crossing the slot's
/// context boundary.
pub struct UseToolRegistry;

impl BlockFactory for UseToolRegistry {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(use_tool_registry))
    }
}

/// A selector that always picks the first inline candidate.
struct PickFirst;

impl ErasedSelector for PickFirst {
    fn select(
        &self,
        _ctx: &SystemContext<'_>,
    ) -> Result<Arc<str>, polaris::graph::predicate::PredicateError> {
        Ok(Arc::from("candidate-0"))
    }

    fn input_type_name(&self) -> &'static str {
        "()"
    }
}

fn version() -> Version {
    Version::new(1, 0, 0)
}

/// The configuration schema every fixture block declares.
pub fn seed_schema() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("fixtures.composition.seed-schema").unwrap(),
        version(),
    )
}

/// A configuration schema no fixture block declares, for mismatch tests.
pub fn other_schema() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("fixtures.composition.other-schema").unwrap(),
        version(),
    )
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(CompositionBlockId::new(id).unwrap(), version())
}

/// The registered key for [`EmitPositiveSeed`].
pub fn emit_positive_seed_key() -> CompositionBlockKey {
    block_key("fixtures.composition.emit-positive-seed")
}

/// The registered key for [`EmitZeroSeed`].
pub fn emit_zero_seed_key() -> CompositionBlockKey {
    block_key("fixtures.composition.emit-zero-seed")
}

/// The registered key for [`DoubleSeed`].
pub fn double_seed_key() -> CompositionBlockKey {
    block_key("fixtures.composition.double-seed")
}

/// The registered key for [`MutateSeed`].
pub fn mutate_seed_key() -> CompositionBlockKey {
    block_key("fixtures.composition.mutate-seed")
}

/// The registered key for [`MutateWithInput`].
pub fn mutate_with_input_key() -> CompositionBlockKey {
    block_key("fixtures.composition.mutate-with-input")
}

/// The registered key for [`MutateWithOptionalInput`].
pub fn mutate_with_optional_input_key() -> CompositionBlockKey {
    block_key("fixtures.composition.mutate-with-optional-input")
}

/// The registered key for [`EmitTimeoutSeed`].
pub fn emit_timeout_seed_key() -> CompositionBlockKey {
    block_key("fixtures.composition.emit-timeout-seed")
}

/// The registered key for [`AlwaysFailStep`].
pub fn always_fail_key() -> CompositionBlockKey {
    block_key("fixtures.composition.always-fail")
}

/// The registered key for [`FlakyOnceStep`].
pub fn flaky_once_key() -> CompositionBlockKey {
    block_key("fixtures.composition.flaky-once")
}

/// The registered key for [`SlowStep`].
pub fn slow_step_key() -> CompositionBlockKey {
    block_key("fixtures.composition.slow-step")
}

/// The registered key for [`UseToolRegistry`].
pub fn use_tool_registry_key() -> CompositionBlockKey {
    block_key("fixtures.composition.use-tool-registry")
}

/// The registered key for [`CountSeedStep`].
pub fn count_seed_key() -> CompositionBlockKey {
    block_key("fixtures.composition.count-seed")
}

/// The stable port [`DoubleSeed`] accepts a wired [`Seed`] input on.
pub fn seed_port_key() -> StablePortKey {
    StablePortKey::new(
        StablePortId::new("fixtures.composition.seed-port").unwrap(),
        version(),
    )
}

/// A predicate reading a [`Seed`] and testing whether it is positive.
pub fn seed_positive_key() -> PredicateKey {
    PredicateKey::new(
        PredicateId::new("fixtures.composition.seed-positive").unwrap(),
        version(),
    )
}

/// A predicate reading a [`Seed`] and testing whether it has reached 3,
/// paired with [`count_seed_key`] to prove exactly how many times a repeated
/// body ran.
pub fn seed_reaches_three_key() -> PredicateKey {
    PredicateKey::new(
        PredicateId::new("fixtures.composition.seed-reaches-three").unwrap(),
        version(),
    )
}

/// A predicate reading a [`Seed`] that never holds, for proving a `Loop`'s
/// native maximum iteration count actually stops it.
pub fn never_true_key() -> PredicateKey {
    PredicateKey::new(
        PredicateId::new("fixtures.composition.never-true").unwrap(),
        version(),
    )
}

/// A discriminator routing a [`Seed`] to `"positive"` or `"non-positive"`.
pub fn seed_sign_key() -> DiscriminatorKey {
    DiscriminatorKey::new(
        DiscriminatorId::new("fixtures.composition.seed-sign").unwrap(),
        version(),
    )
}

/// The closed case-key set [`seed_sign_key`] declares.
pub fn positive_case() -> CaseKey {
    CaseKey::new("positive").unwrap()
}

/// The closed case-key set [`seed_sign_key`] declares.
pub fn non_positive_case() -> CaseKey {
    CaseKey::new("non-positive").unwrap()
}

/// A selector that always picks the first inline `Slot` candidate.
pub fn pick_first_key() -> SelectorKey {
    SelectorKey::new(
        SelectorId::new("fixtures.composition.pick-first").unwrap(),
        version(),
    )
}

/// A `Guard` error-handler recipe: emit a fresh positive seed.
pub fn recover_key() -> HandlerKey {
    HandlerKey::new(
        HandlerId::new("fixtures.composition.recover").unwrap(),
        version(),
    )
}

/// A short timeout with no handler.
pub fn short_timeout_key() -> TimeoutPolicyKey {
    TimeoutPolicyKey::new(
        TimeoutPolicyId::new("fixtures.composition.short-timeout").unwrap(),
        version(),
    )
}

/// A short timeout with a handler recipe, for a test that drives an actual
/// timeout to completion.
pub fn timeout_with_handler_key() -> TimeoutPolicyKey {
    TimeoutPolicyKey::new(
        TimeoutPolicyId::new("fixtures.composition.timeout-with-handler").unwrap(),
        version(),
    )
}

/// A `Guard` error-handler recipe that runs a mutating block inside a `Fork`
/// arm, for a test proving a handler recipe is checked against the same
/// effect rules as the rest of the plan.
pub fn mutating_fork_handler_key() -> HandlerKey {
    HandlerKey::new(
        HandlerId::new("fixtures.composition.mutating-fork-handler").unwrap(),
        version(),
    )
}

/// A timeout-policy handler recipe that runs a mutating block inside a
/// `Fork` arm, mirroring [`mutating_fork_handler_key`] for the timeout path.
pub fn mutating_fork_timeout_key() -> TimeoutPolicyKey {
    TimeoutPolicyKey::new(
        TimeoutPolicyId::new("fixtures.composition.mutating-fork-timeout").unwrap(),
        version(),
    )
}

/// A `Guard` error-handler recipe with more `Fork` arms than the
/// conservative default `max_arms`, for a test proving a handler recipe is
/// checked against the same plan-shape limits as the rest of the plan.
pub fn oversized_handler_key() -> HandlerKey {
    HandlerKey::new(
        HandlerId::new("fixtures.composition.oversized-handler").unwrap(),
        version(),
    )
}

/// A `Guard` error-handler recipe that names itself as its own `on_error`,
/// for a cycle-detection test.
pub fn self_referencing_handler_key() -> HandlerKey {
    HandlerKey::new(
        HandlerId::new("fixtures.composition.self-referencing-handler").unwrap(),
        version(),
    )
}

/// A single-retry policy.
pub fn retry_once_key() -> RetryPolicyKey {
    RetryPolicyKey::new(
        RetryPolicyId::new("fixtures.composition.retry-once").unwrap(),
        version(),
    )
}

/// A retention policy keeping the most recent iteration.
pub fn keep_last_key() -> RetentionPolicyKey {
    RetentionPolicyKey::new(
        RetentionPolicyId::new("fixtures.composition.keep-1").unwrap(),
        version(),
    )
}

/// The isolated default context policy, registered under a stable key.
pub fn isolated_context_key() -> ContextPolicyKey {
    ContextPolicyKey::new(
        ContextPolicyId::new("fixtures.composition.isolated").unwrap(),
        version(),
    )
}

/// A context policy sharing [`ToolRegistry`] into the child context, so a
/// `Slot` candidate reading it can see the parent's copy.
pub fn shared_tool_registry_key() -> ContextPolicyKey {
    ContextPolicyKey::new(
        ContextPolicyId::new("fixtures.composition.shared-tool-registry").unwrap(),
        version(),
    )
}

/// A `Step` naming [`EmitPositiveSeed`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn emit_positive_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        emit_positive_seed_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`EmitZeroSeed`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn emit_zero_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        emit_zero_seed_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`DoubleSeed`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn double_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        double_seed_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`AlwaysFailStep`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn always_fail_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        always_fail_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`FlakyOnceStep`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn flaky_once_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        flaky_once_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`SlowStep`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn slow_step_plan(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        slow_step_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`CountSeedStep`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn count_seed_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        count_seed_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`MutateWithInput`] under `instance`, resolved against
/// [`seed_config_id`].
pub fn mutate_with_input_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        mutate_with_input_key(),
        seed_config_id(),
    )
}

/// A `Step` naming [`MutateWithOptionalInput`] under `instance`, resolved
/// against [`seed_config_id`].
pub fn mutate_with_optional_input_step(instance: &str) -> StepPlan {
    StepPlan::new(
        BlockInstanceId::new(instance).unwrap(),
        mutate_with_optional_input_key(),
        seed_config_id(),
    )
}

/// The configuration key every fixture step resolves against, matching
/// [`seed_schema`].
pub fn seed_config_id() -> ConfigId {
    ConfigId::new("fixtures.composition.seed-config").unwrap()
}

/// A configuration key whose resolved value uses [`other_schema`] instead of
/// the block's declared [`seed_schema`], for a schema-mismatch test.
pub fn mismatched_config_id() -> ConfigId {
    ConfigId::new("fixtures.composition.mismatched-config").unwrap()
}

/// The configuration table every fixture plan resolves against.
pub fn configurations() -> BTreeMap<ConfigId, ResolvedConfigurationRef> {
    let mut configurations = BTreeMap::new();
    configurations.insert(
        seed_config_id(),
        ResolvedConfigurationRef::inline(seed_schema(), ConfigurationValue::Null).unwrap(),
    );
    configurations.insert(
        mismatched_config_id(),
        ResolvedConfigurationRef::inline(other_schema(), ConfigurationValue::Null).unwrap(),
    );
    configurations
}

/// Builds and seals the fixture composition catalogue.
pub fn catalogue() -> CompositionCatalogueSnapshot {
    build_catalogue("fixtures.composition.catalogue", |_| {})
}

/// The same registrations as [`catalogue`], but sealed under a different
/// catalogue identity — for a test proving a plan naming one catalogue key is
/// refused when materialized against a snapshot sealed under another, even
/// though every block and policy key it references still resolves.
pub fn relabeled_catalogue() -> CompositionCatalogueSnapshot {
    build_catalogue("fixtures.composition.relabeled-catalogue", |_| {})
}

/// The same catalogue identity and version as [`catalogue`], but with one
/// extra, otherwise-unused registration, so it seals to a different digest —
/// for a test proving a plan naming the right catalogue key but a stale
/// digest is refused, mirroring a catalogue re-sealed after the plan was
/// produced.
pub fn stale_catalogue() -> CompositionCatalogueSnapshot {
    build_catalogue("fixtures.composition.catalogue", |catalogue| {
        catalogue
            .register_context_policy(
                ContextPolicyKey::new(
                    ContextPolicyId::new("fixtures.composition.stale-marker").unwrap(),
                    version(),
                ),
                ContextPolicy::new(),
            )
            .unwrap();
    })
}

fn build_catalogue(
    id: &str,
    extra: impl FnOnce(&mut CompositionCatalogue),
) -> CompositionCatalogueSnapshot {
    let mut catalogue =
        CompositionCatalogue::new(CompositionCatalogId::new(id).unwrap(), version());
    catalogue
        .register_block(
            emit_positive_seed_key(),
            Arc::new(EmitPositiveSeed),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            emit_zero_seed_key(),
            Arc::new(EmitZeroSeed),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            double_seed_key(),
            Arc::new(DoubleSeed),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            mutate_seed_key(),
            Arc::new(MutateSeed),
            seed_schema(),
            Effect::Mutating { repeat_safe: false },
        )
        .unwrap();
    catalogue
        .register_block(
            mutate_with_input_key(),
            Arc::new(MutateWithInput),
            seed_schema(),
            Effect::Mutating { repeat_safe: false },
        )
        .unwrap();
    catalogue
        .register_block(
            mutate_with_optional_input_key(),
            Arc::new(MutateWithOptionalInput),
            seed_schema(),
            Effect::Mutating { repeat_safe: false },
        )
        .unwrap();
    catalogue
        .register_block(
            emit_timeout_seed_key(),
            Arc::new(EmitTimeoutSeed),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            always_fail_key(),
            Arc::new(AlwaysFailStep),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            flaky_once_key(),
            Arc::new(FlakyOnceStep),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            slow_step_key(),
            Arc::new(SlowStep),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            use_tool_registry_key(),
            Arc::new(UseToolRegistry),
            seed_schema(),
            Effect::ReadOnly,
        )
        .unwrap();
    catalogue
        .register_block(
            count_seed_key(),
            Arc::new(CountSeedStep),
            seed_schema(),
            Effect::Mutating { repeat_safe: true },
        )
        .unwrap();
    catalogue
        .register_predicate(
            seed_positive_key(),
            BoxedPredicate::new::<Seed>(|seed| seed.0 > 0),
        )
        .unwrap();
    catalogue
        .register_predicate(
            seed_reaches_three_key(),
            BoxedPredicate::new::<Seed>(|seed| seed.0 >= 3),
        )
        .unwrap();
    catalogue
        .register_predicate(never_true_key(), BoxedPredicate::new::<Seed>(|_seed| false))
        .unwrap();
    catalogue
        .register_discriminator(
            seed_sign_key(),
            BoxedDiscriminator::new::<Seed, _>(|seed| {
                if seed.0 > 0 {
                    "positive"
                } else {
                    "non-positive"
                }
            }),
            [positive_case(), non_positive_case()].into_iter().collect(),
        )
        .unwrap();
    let selector: BoxedSelector = Box::new(PickFirst);
    catalogue
        .register_selector(pick_first_key(), selector)
        .unwrap();
    catalogue
        .register_handler(recover_key(), GateExpr::Step(emit_positive_step("recover")))
        .unwrap();
    catalogue
        .register_timeout_policy(
            short_timeout_key(),
            TimeoutPolicyEntry::new(Duration::from_millis(200), None),
        )
        .unwrap();
    catalogue
        .register_timeout_policy(
            timeout_with_handler_key(),
            TimeoutPolicyEntry::new(
                Duration::from_millis(30),
                Some(GateExpr::Step(StepPlan::new(
                    BlockInstanceId::new("timeout-handler").unwrap(),
                    emit_timeout_seed_key(),
                    seed_config_id(),
                ))),
            ),
        )
        .unwrap();
    catalogue
        .register_handler(
            mutating_fork_handler_key(),
            GateExpr::Fork {
                arms: vec![GateExpr::Step(StepPlan::new(
                    BlockInstanceId::new("mutating-fork-handler-arm").unwrap(),
                    mutate_seed_key(),
                    seed_config_id(),
                ))],
                reducer: emit_positive_step("mutating-fork-handler-reduce"),
            },
        )
        .unwrap();
    catalogue
        .register_timeout_policy(
            mutating_fork_timeout_key(),
            TimeoutPolicyEntry::new(
                Duration::from_millis(200),
                Some(GateExpr::Fork {
                    arms: vec![GateExpr::Step(StepPlan::new(
                        BlockInstanceId::new("mutating-fork-timeout-arm").unwrap(),
                        mutate_seed_key(),
                        seed_config_id(),
                    ))],
                    reducer: emit_positive_step("mutating-fork-timeout-reduce"),
                }),
            ),
        )
        .unwrap();
    catalogue
        .register_handler(
            oversized_handler_key(),
            GateExpr::Fork {
                arms: (0..9)
                    .map(|index| {
                        GateExpr::Step(emit_positive_step(&format!(
                            "oversized-handler-arm-{index}"
                        )))
                    })
                    .collect(),
                reducer: emit_positive_step("oversized-handler-reduce"),
            },
        )
        .unwrap();
    catalogue
        .register_handler(
            self_referencing_handler_key(),
            GateExpr::Guard {
                step: emit_positive_step("self-referencing-handler-step"),
                on_error: self_referencing_handler_key(),
                on_timeout: short_timeout_key(),
                retry: None,
            },
        )
        .unwrap();
    catalogue
        .register_retry_policy(
            retry_once_key(),
            RetryPolicy::fixed(1, Duration::from_millis(1)),
        )
        .unwrap();
    catalogue
        .register_retention_policy(keep_last_key(), RetentionPolicy::keep_last(1))
        .unwrap();
    catalogue
        .register_context_policy(isolated_context_key(), ContextPolicy::new())
        .unwrap();
    catalogue
        .register_context_policy(
            shared_tool_registry_key(),
            ContextPolicy::new().share::<ToolRegistry>(),
        )
        .unwrap();
    extra(&mut catalogue);
    catalogue.seal().unwrap()
}

fn contract_id(id: &str) -> TopologyContractKey {
    TopologyContractKey::new(TopologyContractId::new(id).unwrap(), version())
}

/// The target contract satisfied by a plan that guarantees a [`Doubled`]
/// output.
pub fn doubled_contract_key() -> TopologyContractKey {
    contract_id("fixtures.composition.doubled")
}

/// The target contract satisfied by a plan that guarantees a [`Seed`]
/// output — deliberately weaker than what most fixture plans build, so a
/// plan targeting [`doubled_contract_key`] while only guaranteeing this one
/// exercises the target-contract-mismatch refusal.
pub fn seed_contract_key() -> TopologyContractKey {
    contract_id("fixtures.composition.seed")
}

/// A target contract with no required production, for a plan whose two
/// continuations guarantee genuinely different output types (a `Guard`
/// whose timeout handler diverts to its own type instead of its guarded
/// step's) — there, no single type is guaranteed, so any contract requiring
/// one would rightly refuse the plan.
pub fn no_guarantee_contract_key() -> TopologyContractKey {
    contract_id("fixtures.composition.no-guarantee")
}

/// The contract satisfied by a plan that reads [`ToolRegistry`] and
/// guarantees an [`Answer`] — used both as a `Slot`'s own contract and,
/// unchanged, as a plan target that correctly claims the resource a slot
/// candidate needs.
pub fn tool_answer_contract_key() -> TopologyContractKey {
    contract_id("fixtures.composition.tool-answer")
}

/// A target contract that only names [`Answer`], omitting the
/// [`ToolRegistry`] requirement a `Slot`'s own contract declares — used to
/// exercise the refusal when a plan's target does not claim a resource a
/// slot candidate needs.
pub fn answer_only_contract_key() -> TopologyContractKey {
    contract_id("fixtures.composition.answer-only")
}

/// Builds and seals the fixture topology contract catalog, with
/// [`doubled_contract_key`], [`seed_contract_key`],
/// [`no_guarantee_contract_key`], [`tool_answer_contract_key`], and
/// [`answer_only_contract_key`] registered.
pub fn contracts() -> TopologyCatalogSnapshot {
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(
            TopologyContract::new(
                doubled_contract_key(),
                GraphSignature::new().produce::<Doubled>(),
                "double a seed value",
                "the doubled value",
            )
            .unwrap(),
        )
        .unwrap();
    catalog
        .register_contract(
            TopologyContract::new(
                seed_contract_key(),
                GraphSignature::new().produce::<Seed>(),
                "produce a seed value",
                "the seed value",
            )
            .unwrap(),
        )
        .unwrap();
    catalog
        .register_contract(
            TopologyContract::new(
                no_guarantee_contract_key(),
                GraphSignature::new(),
                "run to completion with no guaranteed output",
                "nothing in particular",
            )
            .unwrap(),
        )
        .unwrap();
    catalog
        .register_contract(
            TopologyContract::new(
                tool_answer_contract_key(),
                GraphSignature::new()
                    .require_read::<ToolRegistry>()
                    .produce::<Answer>(),
                "answer a question using the tool registry",
                "the derived answer",
            )
            .unwrap(),
        )
        .unwrap();
    catalog
        .register_contract(
            TopologyContract::new(
                answer_only_contract_key(),
                GraphSignature::new().produce::<Answer>(),
                "produce an answer",
                "the answer, however it was derived",
            )
            .unwrap(),
        )
        .unwrap();
    catalog.seal()
}
