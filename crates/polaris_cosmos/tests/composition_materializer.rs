//! End-to-end and refusal-path tests for the gate-plan materializer.

use polaris::graph::{ExecutionError, GraphExecutor};
use polaris::system::param::SystemContext;

use polaris_cosmos::composition::refusal::codes;
use polaris_cosmos::composition::{
    BlockInstanceId, CompositionLimits, ConfigId, GateExpr, GatePath, GatePlan, GateStep,
    LoopControl, StablePortId, StablePortKey, SwitchCase, WirePlan, materialize,
};
use polaris_cosmos::contracts::{ConfigurationValue, ResolvedConfigurationRef};
use polaris_cosmos::topology::TopologyContractKey;

#[path = "fixtures/composition.rs"]
mod fixtures;

fn build_plan(expression: GateExpr, target: TopologyContractKey, wires: Vec<WirePlan>) -> GatePlan {
    let catalogue = fixtures::catalogue();
    GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        target,
        expression,
        wires,
        fixtures::configurations(),
    )
    .unwrap()
}

fn doubling_plan(seed_instance: &str, double_instance: &str, seed_key: bool) -> GateExpr {
    let seed_step = if seed_key {
        fixtures::emit_positive_step(seed_instance)
    } else {
        fixtures::emit_zero_step(seed_instance)
    };
    GateExpr::Seq(vec![
        GateExpr::Step(seed_step),
        GateExpr::Step(fixtures::double_step(double_instance)),
    ])
}

#[tokio::test]
async fn materialize_builds_and_runs_a_simple_plan() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("plan should materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.output::<fixtures::Doubled>(),
        Some(&fixtures::Doubled(2))
    );
}

#[tokio::test]
async fn two_plans_over_one_catalogue_produce_two_graphs_that_both_run() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let limits = CompositionLimits::conservative();

    let plan_a = build_plan(
        doubling_plan("emit-a", "double-a", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );
    let plan_b = build_plan(
        doubling_plan("emit-b", "double-b", false),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    let graph_a =
        materialize(&plan_a, &catalogue, &limits, &contracts).expect("plan a should materialize");
    let graph_b =
        materialize(&plan_b, &catalogue, &limits, &contracts).expect("plan b should materialize");

    let executor = GraphExecutor::new();

    let mut ctx_a = SystemContext::new();
    let result_a = executor
        .execute(&graph_a, &mut ctx_a, None, None)
        .await
        .unwrap();
    assert_eq!(
        result_a.output::<fixtures::Doubled>(),
        Some(&fixtures::Doubled(2))
    );

    let mut ctx_b = SystemContext::new();
    let result_b = executor
        .execute(&graph_b, &mut ctx_b, None, None)
        .await
        .unwrap();
    assert_eq!(
        result_b.output::<fixtures::Doubled>(),
        Some(&fixtures::Doubled(0))
    );
}

#[test]
fn plan_json_round_trip_and_digest_stability() {
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    let json = serde_json::to_string(&gate_plan).unwrap();
    let restored: GatePlan = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, gate_plan);
    assert_eq!(restored.plan_digest(), gate_plan.plan_digest());

    // Digest stability: an independently rebuilt, structurally identical plan
    // gets the same digest.
    let rebuilt = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );
    assert_eq!(rebuilt.plan_digest(), gate_plan.plan_digest());
}

#[test]
fn refuses_unknown_block() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let unknown_block = polaris_cosmos::contracts::CompositionBlockKey::new(
        polaris_cosmos::contracts::CompositionBlockId::new("fixtures.composition.does-not-exist")
            .unwrap(),
        polaris::system::plugin::Version::new(1, 0, 0),
    );
    let expr = GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
        BlockInstanceId::new("unknown").unwrap(),
        unknown_block,
        fixtures::seed_config_id(),
    ));
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("an unregistered block must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::UNKNOWN_BLOCK)
    );
}

#[test]
fn refuses_unknown_config_key() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
        BlockInstanceId::new("step").unwrap(),
        fixtures::emit_positive_seed_key(),
        polaris_cosmos::composition::ConfigId::new("fixtures.composition.does-not-exist").unwrap(),
    ));
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("an unresolved configuration id must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::UNKNOWN_KEY)
    );
}

#[test]
fn refuses_bad_config_schema() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
        BlockInstanceId::new("step").unwrap(),
        fixtures::emit_positive_seed_key(),
        fixtures::mismatched_config_id(),
    ));
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a configuration using the wrong schema must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::BAD_CONFIG)
    );
}

#[test]
fn refuses_plans_deeper_than_the_configured_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Seq(vec![GateExpr::Seq(vec![GateExpr::Step(
        fixtures::emit_positive_step("nested"),
    )])]);
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());
    let limits = CompositionLimits {
        max_depth: 1,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a plan deeper than max_depth must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_a_plan_whose_materialized_node_count_exceeds_the_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let steps = (0..50)
        .map(|index| GateExpr::Step(fixtures::emit_positive_step(&format!("wide-{index}"))))
        .collect();
    let gate_plan = build_plan(
        GateExpr::Seq(steps),
        fixtures::seed_contract_key(),
        Vec::new(),
    );
    let limits = CompositionLimits {
        max_nodes: 5,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a plan materializing more nodes than max_nodes must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn counts_a_seals_interior_nodes_toward_the_materialized_node_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let steps = (0..10)
        .map(|index| GateExpr::Step(fixtures::emit_positive_step(&format!("sealed-{index}"))))
        .collect();
    let expr = GateExpr::Seal {
        body: Box::new(GateExpr::Seq(steps)),
        contract: fixtures::seed_contract_key(),
        policy: fixtures::isolated_context_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());
    let limits = CompositionLimits {
        max_nodes: 3,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts).expect_err(
        "a Seal whose interior alone exceeds max_nodes must be refused, even though the \
         Seal contributes only one node to a flat top-level count",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_a_plan_with_more_wires_than_the_configured_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(fixtures::emit_positive_step("emit"));
    let wires = (0..10)
        .map(|index| {
            WirePlan::new(
                BlockInstanceId::new(format!("does-not-exist-{index}")).unwrap(),
                BlockInstanceId::new("emit").unwrap(),
                fixtures::seed_port_key(),
            )
        })
        .collect();
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), wires);
    let limits = CompositionLimits {
        max_wires: 5,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a plan with more wires than max_wires must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_a_plan_with_more_configurations_than_the_configured_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(fixtures::emit_positive_step("emit"));
    let mut configurations = fixtures::configurations();
    for index in 0..10 {
        configurations.insert(
            ConfigId::new(format!("fixtures.composition.extra-config-{index}")).unwrap(),
            ResolvedConfigurationRef::inline(fixtures::seed_schema(), ConfigurationValue::Null)
                .unwrap(),
        );
    }
    let catalogue_snapshot = fixtures::catalogue();
    let gate_plan = GatePlan::new(
        catalogue_snapshot.catalogue().clone(),
        catalogue_snapshot.digest(),
        fixtures::seed_contract_key(),
        expr,
        Vec::new(),
        configurations,
    )
    .unwrap();
    let limits = CompositionLimits {
        max_configurations: 5,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a plan with more configurations than max_configurations must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_a_configuration_deeper_than_the_configured_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(fixtures::emit_positive_step("emit"));
    let mut configurations = fixtures::configurations();
    let deep_value = ConfigurationValue::List(vec![ConfigurationValue::List(vec![
        ConfigurationValue::List(vec![ConfigurationValue::Null]),
    ])]);
    configurations.insert(
        ConfigId::new("fixtures.composition.deep-config").unwrap(),
        ResolvedConfigurationRef::inline(fixtures::seed_schema(), deep_value).unwrap(),
    );
    let catalogue_snapshot = fixtures::catalogue();
    let gate_plan = GatePlan::new(
        catalogue_snapshot.catalogue().clone(),
        catalogue_snapshot.digest(),
        fixtures::seed_contract_key(),
        expr,
        Vec::new(),
        configurations,
    )
    .unwrap();
    let limits = CompositionLimits {
        max_configuration_depth: 2,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a configuration nested deeper than max_configuration_depth must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_a_configuration_larger_than_the_configured_byte_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(fixtures::emit_positive_step("emit"));
    let mut configurations = fixtures::configurations();
    let large_value = ConfigurationValue::String("x".repeat(5_000));
    configurations.insert(
        ConfigId::new("fixtures.composition.large-config").unwrap(),
        ResolvedConfigurationRef::inline(fixtures::seed_schema(), large_value).unwrap(),
    );
    let catalogue_snapshot = fixtures::catalogue();
    let gate_plan = GatePlan::new(
        catalogue_snapshot.catalogue().clone(),
        catalogue_snapshot.digest(),
        fixtures::seed_contract_key(),
        expr,
        Vec::new(),
        configurations,
    )
    .unwrap();
    let limits = CompositionLimits {
        max_configuration_bytes: 100,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a configuration larger than max_configuration_bytes must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_a_mutating_block_under_fork() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Fork {
        arms: vec![GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
            BlockInstanceId::new("mutate").unwrap(),
            fixtures::mutate_seed_key(),
            fixtures::seed_config_id(),
        ))],
        reducer: fixtures::emit_positive_step("reduce"),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a mutating block under Fork must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn refuses_a_wire_naming_an_unknown_instance() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(fixtures::emit_positive_step("emit"));
    let port = StablePortKey::new(
        StablePortId::new("fixtures.composition.unused-port").unwrap(),
        polaris::system::plugin::Version::new(1, 0, 0),
    );
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("does-not-exist").unwrap(),
        BlockInstanceId::new("emit").unwrap(),
        port,
    )];
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), wires);

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a wire naming an unknown reader instance must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::BAD_WIRE)
    );
}

#[test]
fn native_validation_refusal_names_the_nested_seals_own_plan_path() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Seq(vec![
        GateExpr::Step(fixtures::emit_positive_step("leaf")),
        GateExpr::Seal {
            body: Box::new(GateExpr::Step(fixtures::emit_zero_step("z"))),
            contract: fixtures::doubled_contract_key(),
            policy: fixtures::isolated_context_key(),
        },
    ]);
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a nested Seal body that does not guarantee its stated contract must be refused");
    let refusal = refusals
        .iter()
        .find(|refusal| refusal.code() == codes::SEAL_CONTRACT_MISMATCH)
        .expect("a SEAL_CONTRACT_MISMATCH refusal");
    assert_eq!(
        refusal.path(),
        &GatePath::root().join(GateStep::Seq(1)),
        "the refusal must name the nested Seal's own position in the plan tree, not the plan root"
    );
}

#[test]
fn native_validation_refusal_for_a_slot_candidate_names_the_candidates_own_seal_node() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Seq(vec![
        GateExpr::Step(fixtures::emit_positive_step("leaf")),
        GateExpr::Slot {
            contract: fixtures::doubled_contract_key(),
            candidates: vec![GateExpr::Seal {
                body: Box::new(GateExpr::Step(fixtures::emit_zero_step("z"))),
                contract: fixtures::doubled_contract_key(),
                policy: fixtures::isolated_context_key(),
            }],
            selector: fixtures::pick_first_key(),
            policy: fixtures::isolated_context_key(),
        },
    ]);
    let gate_plan = build_plan(expr, fixtures::doubled_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a Slot candidate whose own Seal contract is violated must be refused");
    let refusal = refusals
        .iter()
        .find(|refusal| refusal.code() == codes::NATIVE_VALIDATION_ERROR)
        .expect("a NATIVE_VALIDATION_ERROR refusal");
    assert_eq!(
        refusal.path(),
        &GatePath::root()
            .join(GateStep::Seq(1))
            .join(GateStep::SlotCandidate(0)),
        "the refusal must name the candidate Seal's own position, not the plan root"
    );
}

#[test]
fn bad_wire_refusal_names_the_failing_wires_own_index() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Seq(vec![
        GateExpr::Fork {
            arms: vec![
                GateExpr::Step(fixtures::emit_positive_step("arm-a")),
                GateExpr::Step(fixtures::emit_zero_step("arm-b")),
            ],
            reducer: fixtures::double_step("reduce"),
        },
        GateExpr::Step(fixtures::double_step("after")),
    ]);
    let wires = vec![
        WirePlan::new(
            BlockInstanceId::new("reduce").unwrap(),
            BlockInstanceId::new("arm-a").unwrap(),
            fixtures::seed_port_key(),
        ),
        WirePlan::new(
            BlockInstanceId::new("after").unwrap(),
            BlockInstanceId::new("does-not-exist").unwrap(),
            fixtures::seed_port_key(),
        ),
    ];
    let gate_plan = build_plan(expr, fixtures::doubled_contract_key(), wires);

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("the second wire, naming an unknown producer, must be refused");
    let refusal = refusals
        .iter()
        .find(|refusal| refusal.code() == codes::BAD_WIRE)
        .expect("a BAD_WIRE refusal");
    assert_eq!(
        refusal.path(),
        &GatePath::root().join(GateStep::Wire(1)),
        "the refusal must name the failing wire's own index, not the plan root or wire 0"
    );
}

#[test]
fn refuses_a_seal_whose_body_does_not_guarantee_its_contract() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Seal {
        body: Box::new(GateExpr::Step(fixtures::emit_zero_step("z"))),
        contract: fixtures::doubled_contract_key(),
        policy: fixtures::isolated_context_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a Seal body that does not guarantee its stated contract must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::SEAL_CONTRACT_MISMATCH)
    );
}

#[test]
fn refuses_a_plan_that_does_not_satisfy_its_target_contract() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Step(fixtures::emit_zero_step("z"));
    let gate_plan = build_plan(expr, fixtures::doubled_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a plan that only guarantees Seed cannot satisfy a Doubled target");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::TARGET_CONTRACT_MISMATCH)
    );
}

#[test]
fn refuses_a_loop_whose_predicate_input_is_never_produced_before_it() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Step(fixtures::emit_positive_step("body"))),
        control: LoopControl::Until(fixtures::seed_positive_key()),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a loop whose predicate input is only produced inside its own body must be refused",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::MISSING_LOOP_INITIALIZER)
    );
}

#[test]
fn refuses_a_switch_that_does_not_cover_every_case() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Switch {
        discriminator: fixtures::seed_sign_key(),
        cases: vec![SwitchCase::new(
            fixtures::positive_case(),
            GateExpr::Step(fixtures::emit_positive_step("pos")),
        )],
        default: None,
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a Switch missing a case with no default must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::SWITCH_NOT_EXHAUSTIVE)
    );
}

#[test]
fn refuses_a_slot_candidate_that_does_not_guarantee_the_contract() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let candidate = GateExpr::Choose {
        predicate: fixtures::seed_positive_key(),
        when_true: Box::new(doubling_plan("cand-true-seed", "cand-true-double", true)),
        when_false: Box::new(GateExpr::Step(fixtures::emit_zero_step("cand-false"))),
    };
    let expr = GateExpr::Slot {
        contract: fixtures::doubled_contract_key(),
        candidates: vec![candidate],
        selector: fixtures::pick_first_key(),
        policy: fixtures::isolated_context_key(),
    };
    let gate_plan = build_plan(expr, fixtures::doubled_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a Slot candidate that only guarantees the contract on one branch must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::NATIVE_VALIDATION_ERROR)
    );
}

fn tool_registry_slot(policy: polaris_cosmos::composition::ContextPolicyKey) -> GateExpr {
    GateExpr::Slot {
        contract: fixtures::tool_answer_contract_key(),
        candidates: vec![GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
            BlockInstanceId::new("use-tools").unwrap(),
            fixtures::use_tool_registry_key(),
            fixtures::seed_config_id(),
        ))],
        selector: fixtures::pick_first_key(),
        policy,
    }
}

#[tokio::test]
async fn slot_candidate_reads_a_resource_the_target_contract_claims() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = tool_registry_slot(fixtures::shared_tool_registry_key());
    let gate_plan = build_plan(expr, fixtures::tool_answer_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a target that claims the resource a slot candidate needs must materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    ctx.insert(fixtures::ToolRegistry {
        tools: vec!["search".to_owned(), "calculator".to_owned()],
    });
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.output::<fixtures::Answer>(),
        Some(&fixtures::Answer(2))
    );
}

#[test]
fn refuses_a_target_that_omits_a_resource_a_slot_candidate_needs() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = tool_registry_slot(fixtures::shared_tool_registry_key());
    let gate_plan = build_plan(expr, fixtures::answer_only_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a target that omits a resource a slot candidate needs must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::TARGET_CONTRACT_MISMATCH)
    );
}

/// A tiny `SplitMix64` step — deterministic, dependency-free pseudo-randomness
/// for the generator below (no external `proptest`/`rand` crate needed for
/// one bounded property test).
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Generator state threaded through [`random_nested_tree`]: the PRNG state
/// and a counter minting unique block-instance labels.
struct GenState {
    rng: u64,
    next_id: u32,
}

impl GenState {
    fn choose(&mut self, options: u64) -> u64 {
        next_u64(&mut self.rng) % options
    }

    fn fresh_leaf(&mut self) -> GateExpr {
        let id = self.next_id;
        self.next_id += 1;
        GateExpr::Step(fixtures::emit_positive_step(&format!("n{id}")))
    }

    fn fresh_reducer(&mut self) -> polaris_cosmos::composition::StepPlan {
        let id = self.next_id;
        self.next_id += 1;
        fixtures::emit_positive_step(&format!("n{id}"))
    }
}

/// Builds a random gate tree over every gate this module supports (`Seq`,
/// `Choose`, `Switch`, `Fork` with a reducer, `Loop` under both `Until` and
/// `Count`, `Guard`, `Seal`, `Slot`), to a maximum nesting depth of `depth`
/// (the test uses depth 4).
///
/// Every leaf is a fresh, uniquely instanced `Step` producing a positive
/// [`fixtures::Seed`] and nothing else, and every construct that discards or
/// intersects guarantees (`Loop`'s body, `Choose`/`Switch`'s branches,
/// `Guard`'s handlers) is built so the *whole tree* still guarantees exactly
/// `Seed` — by the crediting rules a graph's derived signature actually
/// applies (`Seq`/`Fork` union, `Choose`/`Switch`/`Guard` intersection,
/// `Loop` empty, `Seal`/`Slot` the declared contract), a set every arm of
/// which already contains `Seed` stays exactly `{Seed}` after any of those
/// operations. This lets the property test that drives this generator check
/// the built graph's `guaranteed_produces()` against that one fixed
/// prediction for every shape. The prediction is constant because the fixture
/// is engineered to make it constant, not because the crediting rules are
/// trivial.
///
/// A `Choose`/`Switch`/`Loop-Until` is always preceded by its own
/// seed-producing `Step` in the same `Seq`, so its predicate/discriminator
/// input is always available by the time it runs. `Loop-Until`'s own body is
/// a single fresh leaf rather than a further recursive subtree: `validate`
/// requires the predicate's input type to be produced by an ordinary system
/// reachable inside the loop body itself, a requirement a freely recursive
/// interior (one that might land on a `Seal`, `Slot`, `Fork`, or `Guard`)
/// cannot be guaranteed to satisfy. `Seal`/`Slot` wrap a single fresh leaf for
/// a related reason: both require their interior to satisfy a contract under
/// native `Graph::validate`'s exact-match rules, which a freely recursive
/// interior cannot be guaranteed to do either. Wrapping one known-good leaf
/// in each case keeps every generated plan valid while still exercising the
/// gate at whatever depth the generator reaches it.
fn random_nested_tree(depth: u32, state: &mut GenState) -> GateExpr {
    if depth == 0 {
        return state.fresh_leaf();
    }
    match state.choose(9) {
        0 => GateExpr::Seq(vec![
            state.fresh_leaf(),
            random_nested_tree(depth - 1, state),
        ]),
        1 => GateExpr::Seq(vec![
            state.fresh_leaf(),
            GateExpr::Choose {
                predicate: fixtures::seed_positive_key(),
                when_true: Box::new(random_nested_tree(depth - 1, state)),
                when_false: Box::new(random_nested_tree(depth - 1, state)),
            },
        ]),
        2 => GateExpr::Seq(vec![
            state.fresh_leaf(),
            GateExpr::Switch {
                discriminator: fixtures::seed_sign_key(),
                cases: vec![
                    SwitchCase::new(
                        fixtures::positive_case(),
                        random_nested_tree(depth - 1, state),
                    ),
                    SwitchCase::new(
                        fixtures::non_positive_case(),
                        random_nested_tree(depth - 1, state),
                    ),
                ],
                default: None,
            },
        ]),
        3 => GateExpr::Fork {
            arms: vec![
                random_nested_tree(depth - 1, state),
                random_nested_tree(depth - 1, state),
            ],
            reducer: state.fresh_reducer(),
        },
        4 => GateExpr::Seq(vec![
            state.fresh_leaf(),
            GateExpr::Loop {
                body: Box::new(state.fresh_leaf()),
                control: LoopControl::Until(fixtures::seed_positive_key()),
                retention: fixtures::keep_last_key(),
            },
            state.fresh_leaf(),
        ]),
        5 => GateExpr::Seq(vec![
            GateExpr::Loop {
                body: Box::new(random_nested_tree(depth - 1, state)),
                control: LoopControl::Count(2),
                retention: fixtures::keep_last_key(),
            },
            state.fresh_leaf(),
        ]),
        6 => GateExpr::Seal {
            body: Box::new(state.fresh_leaf()),
            contract: fixtures::seed_contract_key(),
            policy: fixtures::isolated_context_key(),
        },
        7 => GateExpr::Slot {
            contract: fixtures::seed_contract_key(),
            candidates: vec![state.fresh_leaf(), state.fresh_leaf()],
            selector: fixtures::pick_first_key(),
            policy: fixtures::isolated_context_key(),
        },
        8 => GateExpr::Guard {
            step: state.fresh_reducer(),
            on_error: fixtures::recover_key(),
            on_timeout: fixtures::short_timeout_key(),
            retry: Some(fixtures::retry_once_key()),
        },
        _ => unreachable!("choose(9) is always in 0..9"),
    }
}

#[tokio::test]
async fn random_nested_plans_over_every_gate_materialize_and_match_predicted_signature() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let limits = CompositionLimits {
        max_depth: 64,
        max_instances: 1024,
        max_nested_loops: 8,
        max_nodes: 4096,
        ..CompositionLimits::conservative()
    };
    let executor = GraphExecutor::new();
    let seed_type_id = std::any::TypeId::of::<fixtures::Seed>();

    for seed in 0_u64..64 {
        let mut state = GenState {
            rng: seed,
            next_id: 0,
        };
        let expr = random_nested_tree(4, &mut state);
        let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

        let graph = materialize(&gate_plan, &catalogue, &limits, &contracts)
            .unwrap_or_else(|refusals| panic!("seed {seed} should materialize, got {refusals:?}"));

        // The crediting-rule check: every shape the generator can produce
        // predicts exactly `{Seed}` as guaranteed production (see
        // `random_nested_tree`'s doc comment for why that prediction is
        // sound for every gate it uses).
        let guaranteed = graph.signature().guaranteed_produces().to_vec();
        assert_eq!(
            guaranteed.len(),
            1,
            "seed {seed} predicted guaranteed_produces == {{Seed}}, got {guaranteed:?}"
        );
        assert_eq!(
            guaranteed[0].type_id, seed_type_id,
            "seed {seed} predicted guaranteed_produces == {{Seed}}, got {guaranteed:?}"
        );

        let mut ctx = SystemContext::new();
        let result = executor
            .execute(&graph, &mut ctx, None, None)
            .await
            .unwrap_or_else(|error| panic!("seed {seed} should run, got {error:?}"));
        assert_eq!(
            result.output::<fixtures::Seed>(),
            Some(&fixtures::Seed(1)),
            "seed {seed} should guarantee a positive Seed"
        );
    }
}

#[tokio::test]
async fn materialize_builds_and_runs_a_guarded_step() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::emit_positive_step("guarded"),
        on_error: fixtures::recover_key(),
        on_timeout: fixtures::short_timeout_key(),
        retry: Some(fixtures::retry_once_key()),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a guarded step that never errors or times out should materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(result.output::<fixtures::Seed>(), Some(&fixtures::Seed(1)));
}

#[tokio::test]
async fn fork_reducer_runs_end_to_end_with_wired_arms() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Fork {
        arms: vec![
            GateExpr::Step(fixtures::emit_positive_step("arm-a")),
            GateExpr::Step(fixtures::emit_zero_step("arm-b")),
        ],
        reducer: fixtures::double_step("reduce"),
    };
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("reduce").unwrap(),
        BlockInstanceId::new("arm-a").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::doubled_contract_key(), wires);

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a Fork whose reducer is wired to one arm's output should materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    // The reducer is wired to `arm-a` (positive seed), not `arm-b` (zero
    // seed): a result of `Doubled(2)` proves the wire actually selected
    // which arm's output the reducer read, not just that some `Seed` was
    // reachable in context.
    assert_eq!(
        result.output::<fixtures::Doubled>(),
        Some(&fixtures::Doubled(2))
    );
}

#[tokio::test]
async fn guard_error_handler_runs_when_the_guarded_step_always_fails() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::always_fail_step("flaky"),
        on_error: fixtures::recover_key(),
        on_timeout: fixtures::short_timeout_key(),
        retry: Some(fixtures::retry_once_key()),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a Guard around an always-failing step should still materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    ctx.insert(fixtures::Attempts::default());
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    // `recover_key`'s handler emits a positive Seed; the guarded step itself
    // never succeeds, so this value can only have come from the handler.
    assert_eq!(result.output::<fixtures::Seed>(), Some(&fixtures::Seed(1)));
}

#[tokio::test]
async fn guard_retry_recovers_a_step_that_fails_once() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::flaky_once_step("flaky"),
        on_error: fixtures::recover_key(),
        on_timeout: fixtures::short_timeout_key(),
        retry: Some(fixtures::retry_once_key()),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a Guard around a step that fails once and then succeeds should materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    ctx.insert(fixtures::Attempts::default());
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    // `Seed(2)` is the flaky step's own success value, distinct from
    // `recover_key`'s handler output (`Seed(1)`): only the retry succeeding
    // on the second attempt can have produced it. Without a retry the first
    // failure would fall straight through to the error handler instead.
    assert_eq!(result.output::<fixtures::Seed>(), Some(&fixtures::Seed(2)));
}

#[tokio::test]
async fn guard_timeout_handler_runs_when_the_guarded_step_times_out() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::slow_step_plan("slow"),
        on_error: fixtures::recover_key(),
        on_timeout: fixtures::timeout_with_handler_key(),
        retry: None,
    };
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a Guard around a step that always times out should materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    // `timeout_with_handler_key`'s handler emits a `TimeoutSeed`, which
    // nothing else in this plan produces; seeing it proves the timeout edge
    // fired and its handler ran.
    assert_eq!(
        result.output::<fixtures::TimeoutSeed>(),
        Some(&fixtures::TimeoutSeed(3))
    );
}

#[test]
fn refuses_a_mutating_block_under_fork_inside_a_guards_error_handler() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::emit_positive_step("guarded"),
        on_error: fixtures::mutating_fork_handler_key(),
        on_timeout: fixtures::short_timeout_key(),
        retry: None,
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a mutating block under Fork inside an error-handler recipe must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn refuses_a_mutating_block_under_fork_inside_a_guards_timeout_handler() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::emit_positive_step("guarded"),
        on_error: fixtures::recover_key(),
        on_timeout: fixtures::mutating_fork_timeout_key(),
        retry: None,
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a mutating block under Fork inside a timeout-handler recipe must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn refuses_a_guards_error_handler_that_exceeds_the_fork_arm_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::emit_positive_step("guarded"),
        on_error: fixtures::oversized_handler_key(),
        on_timeout: fixtures::short_timeout_key(),
        retry: None,
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("an error-handler recipe exceeding max_arms must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

// A handler that refers back to itself must return `RECIPE_CYCLE`, not
// overflow the stack.
#[test]
fn refuses_a_handler_that_refers_back_to_itself() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Guard {
        step: fixtures::emit_positive_step("guarded"),
        on_error: fixtures::self_referencing_handler_key(),
        on_timeout: fixtures::short_timeout_key(),
        retry: None,
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a handler that refers back to itself must be refused, not recursed into");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::RECIPE_CYCLE)
    );
}

#[test]
fn refuses_a_plan_with_a_tampered_digest() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    let mut json = serde_json::to_value(&gate_plan).unwrap();
    json["plan_digest"] = serde_json::Value::String(format!("sha256:{}", "0".repeat(64)));
    let tampered: GatePlan = serde_json::from_value(json).unwrap();

    let refusals = materialize(
        &tampered,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a plan whose embedded digest does not match its own content must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::PLAN_NOT_CANONICAL)
    );
}

#[test]
fn refuses_a_plan_naming_a_different_catalogue_key() {
    let contracts = fixtures::contracts();
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    let refusals = materialize(
        &gate_plan,
        &fixtures::relabeled_catalogue(),
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a plan naming a catalogue key different from the supplied snapshot must be refused",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::CATALOGUE_MISMATCH)
    );
}

#[test]
fn refuses_a_plan_against_a_stale_catalogue_digest() {
    let contracts = fixtures::contracts();
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    let refusals = materialize(
        &gate_plan,
        &fixtures::stale_catalogue(),
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a plan whose catalogue digest is stale relative to the supplied snapshot must be refused",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::CATALOGUE_MISMATCH)
    );
}

#[test]
fn materialize_admits_a_plan_whose_catalogue_key_and_digest_match_the_snapshot() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a plan naming the supplied catalogue's own key and digest must be admitted");
}

#[test]
fn refuses_a_plan_with_two_steps_sharing_the_same_instance_label() {
    let catalogue = fixtures::catalogue();
    let expr = GateExpr::Seq(vec![
        GateExpr::Step(fixtures::emit_positive_step("dup")),
        GateExpr::Step(fixtures::double_step("dup")),
    ]);

    let error = GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        fixtures::doubled_contract_key(),
        expr,
        Vec::new(),
        fixtures::configurations(),
    )
    .expect_err("two Steps sharing the same plan-local instance label must be refused");
    assert_eq!(error.code(), "plan.duplicate-instance");
}

#[test]
fn materialize_refuses_a_plan_tampered_to_duplicate_an_instance_label() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );

    let mut json = serde_json::to_value(&gate_plan).unwrap();
    json["expression"]["seq"][1]["step"]["instance"] = serde_json::Value::String("emit".to_owned());
    let tampered: GatePlan = serde_json::from_value(json).unwrap();

    let refusals = materialize(
        &tampered,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a plan tampered to reuse one instance label for two Steps must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::PLAN_NOT_CANONICAL)
    );
}

#[tokio::test]
async fn materialize_builds_and_runs_a_plan_with_a_successful_switch() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Seq(vec![
        GateExpr::Step(fixtures::emit_positive_step("seed")),
        GateExpr::Switch {
            discriminator: fixtures::seed_sign_key(),
            cases: vec![
                SwitchCase::new(
                    fixtures::positive_case(),
                    GateExpr::Step(fixtures::double_step("double-pos")),
                ),
                SwitchCase::new(
                    fixtures::non_positive_case(),
                    GateExpr::Step(fixtures::double_step("double-neg")),
                ),
            ],
            default: None,
        },
    ]);
    let gate_plan = build_plan(expr, fixtures::doubled_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("an exhaustive Switch whose every case guarantees the target must materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.output::<fixtures::Doubled>(),
        Some(&fixtures::Doubled(2)),
        "a positive seed must take the positive case and double to 2"
    );
}

#[tokio::test]
async fn materialize_builds_and_runs_a_loop_to_completion() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // A bare `Loop` guarantees nothing on its own (its crediting rule is the
    // empty set, unlike `Seq`'s union), so a trailing step after it is what
    // lets the whole plan guarantee `Seed` — the loop still has to run all
    // three counted iterations for that trailing step to execute at all.
    let expr = GateExpr::Seq(vec![
        GateExpr::Loop {
            body: Box::new(GateExpr::Step(fixtures::emit_positive_step("body"))),
            control: LoopControl::Count(3),
            retention: fixtures::keep_last_key(),
        },
        GateExpr::Step(fixtures::emit_positive_step("after")),
    ]);
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a counted Loop within the configured limits must materialize");

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap_or_else(|error| panic!("the loop should run to completion, got {error:?}"));
    assert_eq!(
        result.output::<fixtures::Seed>(),
        Some(&fixtures::Seed(1)),
        "the step after the loop should run once the loop's three iterations finish"
    );
}

#[test]
fn refuses_a_plan_with_more_block_instances_than_the_configured_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let gate_plan = build_plan(
        doubling_plan("emit", "double", true),
        fixtures::doubled_contract_key(),
        Vec::new(),
    );
    let limits = CompositionLimits {
        max_instances: 1,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a plan with more block instances than the configured limit must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_loops_nested_deeper_than_the_configured_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Loop {
            body: Box::new(GateExpr::Step(fixtures::emit_positive_step("inner"))),
            control: LoopControl::Count(1),
            retention: fixtures::keep_last_key(),
        }),
        control: LoopControl::Count(1),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());
    let limits = CompositionLimits {
        max_nested_loops: 1,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts)
        .expect_err("a Loop nested inside another beyond the configured limit must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn refuses_a_counted_loop_exceeding_the_max_loop_iterations_limit() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Step(fixtures::emit_positive_step("body"))),
        control: LoopControl::Count(5),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());
    let limits = CompositionLimits {
        max_loop_iterations: 2,
        ..CompositionLimits::conservative()
    };

    let refusals = materialize(&gate_plan, &catalogue, &limits, &contracts).expect_err(
        "a counted Loop whose count exceeds the configured iteration limit must be refused",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::LIMIT_EXCEEDED)
    );
}

#[test]
fn materialize_credits_a_repeats_guaranteed_first_run_to_the_target() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // A bare `Loop` guarantees nothing (its crediting rule is the empty
    // set), so this same body under `GateExpr::Loop` targeting the same
    // contract would be refused. `Repeat` must succeed here precisely
    // because its leading, unconditional run is credited to the target.
    let expr = GateExpr::Repeat {
        body: Box::new(GateExpr::Step(fixtures::emit_positive_step("body"))),
        control: LoopControl::Count(2),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect("a Repeat's guaranteed leading run must satisfy a target requiring its body's output");
}

#[tokio::test]
async fn materialize_runs_a_repeats_body_at_least_once_then_continues_until_the_predicate_holds() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // `count_seed` increments a fresh per-run counter and emits it as
    // `Seed`; `seed_reaches_three` stops once that reaches 3. The predicate
    // is checked before every iteration, including the first — but here its
    // only possible input source is the Repeat's own guaranteed leading run,
    // which must count as an ordinary in-graph dependency rather than a
    // free/backward read, or this plan would be wrongly refused as missing a
    // loop initializer.
    let expr = GateExpr::Repeat {
        body: Box::new(GateExpr::Step(fixtures::count_seed_step("count"))),
        control: LoopControl::Until(fixtures::seed_reaches_three_key()),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect(
        "a Repeat::Until predicate reading only the body's own guaranteed first-run output must not be refused as a missing loop initializer",
    );

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    ctx.insert(fixtures::Attempts::default());
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.output::<fixtures::Seed>(),
        Some(&fixtures::Seed(3)),
        "the body must run once unconditionally and then twice more until the count reaches 3"
    );
}

#[tokio::test]
async fn materialized_loop_until_carries_the_configured_max_iterations_as_its_native_cap() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Seq(vec![
        GateExpr::Step(fixtures::emit_positive_step("seed")),
        GateExpr::Loop {
            body: Box::new(GateExpr::Step(fixtures::emit_positive_step("body"))),
            control: LoopControl::Until(fixtures::never_true_key()),
            retention: fixtures::keep_last_key(),
        },
    ]);
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());
    let limits = CompositionLimits {
        max_loop_iterations: 3,
        ..CompositionLimits::conservative()
    };

    let graph = materialize(&gate_plan, &catalogue, &limits, &contracts).expect(
        "a Loop::Until predicate reading the plan's own guaranteed leading seed must not be refused",
    );

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;
    match result {
        Err(ExecutionError::MaxIterationsExceeded { max, .. }) => assert_eq!(
            max, 3,
            "the materialized loop must carry the configured max_loop_iterations as its native cap"
        ),
        other => panic!(
            "a Loop::Until whose predicate never holds must fail with MaxIterationsExceeded once it reaches the configured cap, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn materialized_repeat_until_carries_the_configured_max_iterations_on_its_loop_portion() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Repeat {
        body: Box::new(GateExpr::Step(fixtures::count_seed_step("count"))),
        control: LoopControl::Until(fixtures::never_true_key()),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());
    let limits = CompositionLimits {
        max_loop_iterations: 2,
        ..CompositionLimits::conservative()
    };

    let graph = materialize(&gate_plan, &catalogue, &limits, &contracts).expect(
        "a Repeat::Until predicate reading only the body's own guaranteed first-run output must not be refused",
    );

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    ctx.insert(fixtures::Attempts::default());
    let result = executor.execute(&graph, &mut ctx, None, None).await;
    match result {
        Err(ExecutionError::MaxIterationsExceeded { max, .. }) => assert_eq!(
            max, 2,
            "the derived loop portion must carry the configured max_loop_iterations as its own native cap, independent of the guaranteed leading run"
        ),
        other => panic!(
            "a Repeat::Until whose predicate never holds must fail with MaxIterationsExceeded once its loop portion reaches the configured cap, got {other:?}"
        ),
    }
}

#[test]
fn refuses_a_wire_naming_an_instance_inside_a_repeats_body() {
    let catalogue = fixtures::catalogue();
    let expr = GateExpr::Seq(vec![
        GateExpr::Repeat {
            body: Box::new(GateExpr::Step(fixtures::emit_positive_step("body"))),
            control: LoopControl::Count(1),
            retention: fixtures::keep_last_key(),
        },
        GateExpr::Step(fixtures::double_step("after")),
    ]);
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("after").unwrap(),
        BlockInstanceId::new("body").unwrap(),
        fixtures::seed_port_key(),
    )];

    let error = GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        fixtures::doubled_contract_key(),
        expr,
        wires,
        fixtures::configurations(),
    )
    .expect_err("a Wire naming an instance built twice inside a Repeat's body must be refused");
    assert_eq!(error.code(), "plan.wire-into-repeat-body");
}

#[test]
fn refuses_a_mutating_block_not_declared_repeat_safe_inside_a_repeats_body() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Repeat {
        body: Box::new(GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
            BlockInstanceId::new("mutate").unwrap(),
            fixtures::mutate_seed_key(),
            fixtures::seed_config_id(),
        ))),
        control: LoopControl::Count(1),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::seed_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a mutating block not declared repeat_safe under a Repeat body must be refused");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn allows_a_non_repeat_safe_mutating_block_under_loop_when_it_reads_a_fresh_body_input() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // "seed" is rebuilt fresh every iteration, exactly like "mutate" itself,
    // so "mutate" reading its output over a wire is safe to repeat even
    // though the block is not declared repeat_safe.
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Seq(vec![
            GateExpr::Step(fixtures::emit_positive_step("seed")),
            GateExpr::Step(fixtures::mutate_with_input_step("mutate")),
        ])),
        control: LoopControl::Count(1),
        retention: fixtures::keep_last_key(),
    };
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("mutate").unwrap(),
        BlockInstanceId::new("seed").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), wires);

    materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect(
        "a mutating block under Loop reading a fresh input produced earlier in the same body iteration must be allowed",
    );
}

#[test]
fn refuses_a_non_repeat_safe_mutating_block_under_loop_when_its_input_is_not_fresh_each_iteration()
{
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // "seed" runs once, before the loop starts, so it is not rebuilt fresh
    // on every iteration: "mutate" reading it is exactly as unsafe as a
    // mutating block with no input at all.
    let expr = GateExpr::Seq(vec![
        GateExpr::Step(fixtures::emit_positive_step("seed")),
        GateExpr::Loop {
            body: Box::new(GateExpr::Step(fixtures::mutate_with_input_step("mutate"))),
            control: LoopControl::Count(1),
            retention: fixtures::keep_last_key(),
        },
    ]);
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("mutate").unwrap(),
        BlockInstanceId::new("seed").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), wires);

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a mutating block under Loop whose input is produced only once before the loop, not fresh each iteration, must still be refused",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn refuses_a_guards_retried_non_repeat_safe_mutating_step_even_with_a_fresh_body_input() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // Even though "mutate" reads "seed", fresh every iteration, a retry
    // re-invokes the very same guarded step with the very same resolved
    // input on every attempt — repetition here comes from the retry, not
    // from the loop, so the fresh-body-input exception does not apply.
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Seq(vec![
            GateExpr::Step(fixtures::emit_positive_step("seed")),
            GateExpr::Guard {
                step: fixtures::mutate_with_input_step("mutate"),
                retry: Some(fixtures::retry_once_key()),
                on_error: fixtures::recover_key(),
                on_timeout: fixtures::short_timeout_key(),
            },
        ])),
        control: LoopControl::Count(1),
        retention: fixtures::keep_last_key(),
    };
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("mutate").unwrap(),
        BlockInstanceId::new("seed").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), wires);

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err("a retried mutating step must stay refused even when it reads a fresh body input");
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[tokio::test]
async fn materialize_runs_a_propose_apply_test_repair_loop_wired_entirely_inside_its_repeat_body() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // "propose" and "apply" are both rebuilt fresh every time the body runs,
    // so wiring "apply" (Mutating, not declared repeat_safe) to "propose"'s
    // output inside the same body is allowed. "run_tests" shares the same
    // Attempts counter "apply" bumps, so its own emitted Seed tracks how many
    // times the body has actually run, and the Until predicate reads that to
    // decide when to stop.
    let expr = GateExpr::Repeat {
        body: Box::new(GateExpr::Seq(vec![
            GateExpr::Step(fixtures::emit_positive_step("propose")),
            GateExpr::Step(fixtures::mutate_with_input_step("apply")),
            GateExpr::Step(fixtures::count_seed_step("run_tests")),
        ])),
        control: LoopControl::Until(fixtures::seed_reaches_three_key()),
        retention: fixtures::keep_last_key(),
    };
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("apply").unwrap(),
        BlockInstanceId::new("propose").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::doubled_contract_key(), wires);

    let graph = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect(
        "a propose/apply/test repair loop, wired entirely inside its own Repeat body, must admit, validate, and meet a target contract satisfied by the guaranteed first run's output",
    );

    let executor = GraphExecutor::new();
    let mut ctx = SystemContext::new();
    ctx.insert(fixtures::Attempts::default());
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    // "run_tests" is the last system to run each time the body runs, so it is
    // the one whose own output `ExecutionResult::output` reports; "apply"'s
    // output is checked straight from the context's accumulated outputs
    // instead, since it is not the last system to run.
    assert_eq!(
        ctx.get_output::<fixtures::Doubled>().ok().as_deref(),
        Some(&fixtures::Doubled(2)),
        "apply must double propose's constant seed on every run, including the guaranteed first one"
    );
    assert_eq!(
        result.output::<fixtures::Seed>(),
        Some(&fixtures::Seed(4)),
        "the body must run twice: once unconditionally, then once more because run_tests' first count (2) does not yet satisfy the predicate, stopping only once it reaches 4"
    );
}

#[test]
fn refuses_a_wire_crossing_a_repeats_body_boundary_even_alongside_an_internal_one() {
    let catalogue = fixtures::catalogue();
    let expr = GateExpr::Seq(vec![
        GateExpr::Repeat {
            body: Box::new(GateExpr::Seq(vec![
                GateExpr::Step(fixtures::emit_positive_step("propose")),
                GateExpr::Step(fixtures::mutate_with_input_step("apply")),
            ])),
            control: LoopControl::Count(1),
            retention: fixtures::keep_last_key(),
        },
        GateExpr::Step(fixtures::double_step("after")),
    ]);
    let wires = vec![
        WirePlan::new(
            BlockInstanceId::new("apply").unwrap(),
            BlockInstanceId::new("propose").unwrap(),
            fixtures::seed_port_key(),
        ),
        WirePlan::new(
            BlockInstanceId::new("after").unwrap(),
            BlockInstanceId::new("apply").unwrap(),
            fixtures::seed_port_key(),
        ),
    ];

    let error = GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        fixtures::doubled_contract_key(),
        expr,
        wires,
        fixtures::configurations(),
    )
    .expect_err(
        "a wire from inside a Repeat's body to an instance outside it must be refused even when a same-body wire is also present",
    );
    assert_eq!(error.code(), "plan.wire-into-repeat-body");
}

#[test]
fn refuses_a_repeats_apply_step_left_unwired_to_its_same_body_producer() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    let expr = GateExpr::Repeat {
        body: Box::new(GateExpr::Seq(vec![
            GateExpr::Step(fixtures::emit_positive_step("propose")),
            GateExpr::Step(fixtures::mutate_with_input_step("apply")),
        ])),
        control: LoopControl::Count(1),
        retention: fixtures::keep_last_key(),
    };
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), Vec::new());

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a mutating block not declared repeat_safe inside a Repeat body, left unwired to any same-body producer, must still be refused",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn refuses_two_wires_naming_the_same_reader_and_port() {
    let catalogue = fixtures::catalogue();
    // A native read_from binding for the same reader and output type
    // replaces an earlier one instead of adding to it, so only the second
    // wire below ever takes effect: "apply" ends up reading "stale", which
    // ran once before the loop, not "fresh", which the first wire names.
    let expr = GateExpr::Seq(vec![
        GateExpr::Step(fixtures::emit_positive_step("stale")),
        GateExpr::Loop {
            body: Box::new(GateExpr::Seq(vec![
                GateExpr::Step(fixtures::emit_positive_step("fresh")),
                GateExpr::Step(fixtures::mutate_with_input_step("apply")),
            ])),
            control: LoopControl::Count(2),
            retention: fixtures::keep_last_key(),
        },
    ]);
    let wires = vec![
        WirePlan::new(
            BlockInstanceId::new("apply").unwrap(),
            BlockInstanceId::new("fresh").unwrap(),
            fixtures::seed_port_key(),
        ),
        WirePlan::new(
            BlockInstanceId::new("apply").unwrap(),
            BlockInstanceId::new("stale").unwrap(),
            fixtures::seed_port_key(),
        ),
    ];

    let error = GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        fixtures::no_guarantee_contract_key(),
        expr,
        wires,
        fixtures::configurations(),
    )
    .expect_err(
        "two wires naming the same reader and port must be refused: only one binding can take effect, so the plan is ambiguous about which producer actually supplies the reader",
    );
    assert_eq!(error.code(), "plan.duplicate-wire");
}

#[test]
fn refuses_a_non_repeat_safe_block_wired_backward_to_a_producer_later_in_the_same_loop_body() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // "apply" is built before "fresh" in this body's sequential order, so a
    // wire from "apply" to "fresh" can only ever supply the previous
    // iteration's output — native validation permits this backward reference
    // only because the input is Option<Out<Seed>>, but it is not a
    // same-iteration fresh input.
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Seq(vec![
            GateExpr::Step(fixtures::mutate_with_optional_input_step("apply")),
            GateExpr::Step(fixtures::emit_positive_step("fresh")),
        ])),
        control: LoopControl::Count(2),
        retention: fixtures::keep_last_key(),
    };
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("apply").unwrap(),
        BlockInstanceId::new("fresh").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), wires);

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a non-repeat-safe block wired backward to a producer that runs later in the same loop body must be refused: that wire supplies only the previous iteration's output, not a fresh one",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn refuses_a_non_repeat_safe_block_wired_to_a_producer_inside_a_choose_arm() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // "fresh" is built only inside the Choose's true arm, so it is not
    // guaranteed to run at all on every path, let alone before "apply".
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Seq(vec![
            GateExpr::Choose {
                predicate: fixtures::seed_positive_key(),
                when_true: Box::new(GateExpr::Step(fixtures::emit_positive_step("fresh"))),
                when_false: Box::new(GateExpr::Step(fixtures::emit_zero_step("fresh-else"))),
            },
            GateExpr::Step(fixtures::mutate_with_input_step("apply")),
        ])),
        control: LoopControl::Count(2),
        retention: fixtures::keep_last_key(),
    };
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("apply").unwrap(),
        BlockInstanceId::new("fresh").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), wires);

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a non-repeat-safe block wired to a producer built only inside a Choose arm must be refused: only one arm runs, so that producer is not guaranteed on every path",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}

#[test]
fn refuses_a_non_repeat_safe_block_wired_to_a_producer_inside_a_nested_loop() {
    let catalogue = fixtures::catalogue();
    let contracts = fixtures::contracts();
    // "fresh" lives inside a nested Loop, which may run zero times, so it is
    // never guaranteed even though it is built somewhere inside the outer
    // body.
    let expr = GateExpr::Loop {
        body: Box::new(GateExpr::Seq(vec![
            GateExpr::Loop {
                body: Box::new(GateExpr::Step(fixtures::emit_positive_step("fresh"))),
                control: LoopControl::Until(fixtures::seed_reaches_three_key()),
                retention: fixtures::keep_last_key(),
            },
            GateExpr::Step(fixtures::mutate_with_input_step("apply")),
        ])),
        control: LoopControl::Count(2),
        retention: fixtures::keep_last_key(),
    };
    let wires = vec![WirePlan::new(
        BlockInstanceId::new("apply").unwrap(),
        BlockInstanceId::new("fresh").unwrap(),
        fixtures::seed_port_key(),
    )];
    let gate_plan = build_plan(expr, fixtures::no_guarantee_contract_key(), wires);

    let refusals = materialize(
        &gate_plan,
        &catalogue,
        &CompositionLimits::conservative(),
        &contracts,
    )
    .expect_err(
        "a non-repeat-safe block wired to a producer built only inside a nested Loop must be refused: the nested loop may run zero times, so that producer is never guaranteed",
    );
    assert!(
        refusals
            .iter()
            .any(|refusal| refusal.code() == codes::EFFECTS_VIOLATION)
    );
}
