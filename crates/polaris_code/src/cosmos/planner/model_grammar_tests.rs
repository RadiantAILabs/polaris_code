// Included in planner::tests to reuse the hermetic model providers.

#[test]
fn submit_plan_keeps_optional_fields_and_dynamic_switch_cases() {
    let tool = submit_plan_tool_definition();
    assert!(!tool.strict);
    // `steps` is optional since a `plan` tree may be submitted instead.
    assert_eq!(tool.parameters["required"], serde_json::json!(["grounds"]));
    assert!(tool.parameters["properties"]["expression"].is_object());
    assert!(tool.parameters["$defs"]["expression"].is_object());
    assert!(tool.parameters["$defs"]["node"].is_object());
}

fn branching_answer() -> serde_json::Value {
    serde_json::json!({
        "steps": [
            {"instance":"apply","block":"apply-direct"},
            {"instance":"judge","block":"judge-stated-edits"},
            {"instance":"pass","block":"affirm-assessment"},
            {"instance":"retry","block":"judge-stated-edits"},
            {"instance":"conclude","block":"conclude-single-pass"}
        ],
        "expression":{"seq":[{"step":"apply"},{"step":"judge"},
            {"choose":{"predicate":"code.tests-passed","when_true":{"step":"pass"},
                "when_false":{"repeat":{"body":{"step":"retry"},"count":1}}}},
            {"step":"conclude"}]},
        "grounds":"only retry a failed judgment"
    })
}

async fn parse_answer(answer: serde_json::Value) -> GatePlan {
    model_plan(&edit_facts(false), None, &coding_block_catalogue(), &target_contract(),
        Some(&registry_with_tool_call(answer)), Some("scripted-tool-call/model"), &TokenMeter::default(), None)
        .await.expect("grammar accepts the answer")
}

fn assert_materializes(plan: &GatePlan) {
    let graph = materialize(plan, &coding_block_catalogue(), &CompositionLimits::conservative(),
        &coding_target_contracts().unwrap()).expect("native plan materializes");
    assert!(graph.validate().is_ok());
}

#[tokio::test]
async fn model_tree_preserves_choose_repeat_and_step_configuration_order() {
    let plan = parse_answer(branching_answer()).await;
    let step = |id, key, index| GateExpr::Step(StepPlan::new(
        BlockInstanceId::new(id).unwrap(), key, ConfigId::new(format!("model-step-{index}")).unwrap()));
    assert_eq!(plan.expression(), &GateExpr::Seq(vec![
        step("apply", existing::apply_direct_key(), 0),
        step("judge", existing::judge_stated_edits_key(), 1),
        GateExpr::Choose {
            predicate: super::super::gate_blocks::predicates::tests_passed_key(),
            when_true: Box::new(step("pass", configurable::affirm_assessment_key(), 2)),
            when_false: Box::new(GateExpr::Repeat {
                body: Box::new(step("retry", existing::judge_stated_edits_key(), 3)),
                control: polaris_cosmos::composition::LoopControl::Count(1),
                retention: super::super::gate_blocks::keep_last_iteration_key(),
            }),
        },
        step("conclude", existing::conclude_single_pass_key(), 4),
    ]));
    assert_materializes(&plan);
}

#[tokio::test]
async fn model_switch_uses_registered_cases_and_materializes() {
    let mut answer = branching_answer();
    answer["steps"].as_array_mut().unwrap().insert(2, serde_json::json!({
        "instance":"classify","block":"classify-request",
        "config":{"registered":{"allowed_dispositions":["edit","answer","clarify"]}}
    }));
    answer["expression"]["seq"].as_array_mut().unwrap().insert(2, serde_json::json!({"step":"classify"}));
    answer["expression"]["seq"][3] = serde_json::json!({"switch":{
        "discriminator":"code.task-disposition",
        "cases":{"edit":{"step":"pass"}},
        "default":{"step":"retry"}
    }});
    let plan = parse_answer(answer).await;
    let GateExpr::Seq(children) = plan.expression() else { panic!("sequence") };
    let GateExpr::Switch { discriminator, cases, default } = &children[3] else { panic!("switch") };
    assert_eq!(*discriminator, super::super::gate_blocks::predicates::task_disposition_key());
    assert_eq!(cases[0].key().as_str(), "edit");
    assert!(default.is_some());
    assert_materializes(&plan);
}

#[tokio::test]
async fn model_typed_patch_wire_materializes() {
    let plan = parse_answer(serde_json::json!({
        "steps":[
            {"instance":"propose","block":"model-propose","config":{"model-propose":{
                "role":"propose","instruction":"fix the issue","tools":["read_file"],"token_budget":1000}}},
            {"instance":"apply","block":"apply-patch-intent"},
            {"instance":"judge","block":"judge-patch-intent"},
            {"instance":"conclude","block":"conclude-single-pass"}],
        "wires":[{"reader":"apply","producer":"propose","port":"code.patch-intent"}],
        "grounds":"use a typed predecessor"
    })).await;
    assert_eq!(plan.wires().len(), 1);
    assert_materializes(&plan);
}

#[tokio::test]
async fn model_custom_configuration_uses_the_registered_schema() {
    let mut answer = branching_answer();
    answer["steps"].as_array_mut().unwrap().insert(0, serde_json::json!({
        "instance":"survey","block":"survey-paths","config":{"registered":["src/lib.rs"]}}));
    answer["expression"]["seq"].as_array_mut().unwrap().insert(0, serde_json::json!({"step":"survey"}));
    let plan = parse_answer(answer.clone()).await;
    assert_eq!(plan.configurations()[&ConfigId::new("model-step-0").unwrap()].schema(),
        &configurable::survey_paths_schema());
    assert_materializes(&plan);
    answer["steps"][0]["config"]["registered"] = serde_json::json!([42]);
    let bad = parse_answer(answer).await;
    assert!(materialize(&bad, &coding_block_catalogue(), &CompositionLimits::conservative(),
        &coding_target_contracts().unwrap()).is_err(), "factory rejects invalid custom values");
}

#[tokio::test]
async fn invalid_predicate_and_native_wire_refusals_feed_the_next_model_revision() {
    let mut bad_predicate = branching_answer();
    bad_predicate["expression"]["seq"][2]["choose"]["predicate"] = serde_json::json!("code.invented-predicate");
    let mut bad_wire = branching_answer();
    // Known port and labels, wrong typed producer and unsupported reader:
    // this must reach Cosmos's wire validator, not just JSON decoding.
    bad_wire["wires"] = serde_json::json!([{"reader":"judge","producer":"apply","port":"code.patch-intent"}]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ModelRegistry::new();
    registry.register_llm_provider(SequencedAnswerProvider {
        answers: Mutex::new(vec![bad_predicate.to_string(), bad_wire.to_string(), branching_answer().to_string()].into()),
        calls: Arc::new(AtomicUsize::new(0)), requests: Arc::clone(&requests),
    });
    let attempts = attempt_gate_plan(GatePlannerMode::Model, Default::default(), &edit_facts(false),
        &coding_block_catalogue(), &CompositionLimits::conservative(), &coding_target_contracts().unwrap(),
        &target_contract(), Some(&registry), Some("scripted/model"), &TokenMeter::default(), &mut 0, None)
        .await.unwrap();
    assert_eq!(attempts.len(), 3);
    assert!(matches!(attempts[0], GatePlanAttemptResult::Refused { .. }));
    let GatePlanAttemptResult::Refused { refusals, .. } = &attempts[1] else { panic!("wire refused") };
    assert!(refusals.iter().any(|r| r.code() != INVALID_MODEL_ANSWER_CODE));
    assert!(matches!(attempts[2], GatePlanAttemptResult::Materialized { .. }));
    let requests = requests.lock().unwrap();
    assert!(requests[1].contains("unknown predicate 'code.invented-predicate'"));
    assert!(requests[2].contains(refusals[0].code()));
    assert_refused_attempts_form_a_revision_trail(&attempts, &target_contract());
}

#[tokio::test]
async fn ten_and_sixteen_block_model_plans_are_admitted() {
    // `MAX_COMPOSITION_BLOCKS` (16) bounds every gate plan's own composition
    // envelope (`gate_plan_choice` -> `CompositionPlan::new`), tighter than
    // `CompositionLimits::conservative().max_instances` (64), which only
    // bounds native graph materialization — see "Writing and running the
    // script" in `docs/gate-planning.md` for why the smaller number is the one a
    // real plan must budget against. This test proves a plan at that real
    // ceiling still admits, not just one comfortably under it.
    for count in [10, MAX_COMPOSITION_BLOCKS] {
        let mut steps = vec![serde_json::json!({"instance":"apply","block":"apply-direct"})];
        steps.extend((1..count-1).map(|i| serde_json::json!({"instance":format!("judge-{i}"),"block":"judge-stated-edits"})));
        steps.push(serde_json::json!({"instance":"conclude","block":"conclude-single-pass"}));
        let answer = serde_json::json!({"steps":steps,"grounds":"check a larger plan"});
        let registry = registry_with_tool_call(answer);
        let attempts = attempt_gate_plan(GatePlannerMode::Model, Default::default(), &edit_facts(false),
            &coding_block_catalogue(), &CompositionLimits::conservative(), &coding_target_contracts().unwrap(),
            &target_contract(), Some(&registry), Some("scripted-tool-call/model"), &TokenMeter::default(), &mut 0, None)
            .await.unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(matches!(attempts[0], GatePlanAttemptResult::Materialized { .. }), "{count} blocks refused");
    }
}

#[tokio::test]
async fn old_flat_answer_keeps_identical_plan_bytes_with_optional_fields_omitted() {
    let mut answer = branching_answer();
    answer.as_object_mut().unwrap().remove("expression");
    let flat = parse_answer(answer.clone()).await;
    answer["expression"] = serde_json::json!({"seq":[{"step":"apply"},{"step":"judge"},{"step":"pass"},{"step":"retry"},{"step":"conclude"}]});
    answer["wires"] = serde_json::json!([]);
    let explicit = parse_answer(answer).await;
    assert_eq!(serde_json::to_vec(&flat).unwrap(), serde_json::to_vec(&explicit).unwrap());
}
