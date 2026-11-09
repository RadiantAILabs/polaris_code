//! A rule a plugin wrote in code refuses a call the way a permission-file rule
//! does, through the production tool graph.
use super::*;

const NO_GENERATED_EDITS: &str =
    "files under generated/ are produced by the build; edit the source instead";
const NO_RM: &str = "rm is not allowed in this task";

fn no_generated_edits(
    tool: &str,
    args: &serde_json::Value,
    _root: &std::path::Path,
) -> Option<(RuleDecision, String)> {
    let path = args.get("path")?.as_str()?;
    (tool == "update_file" && path.starts_with("generated/"))
        .then(|| (RuleDecision::Deny, NO_GENERATED_EDITS.to_string()))
}

fn no_rm_commands(
    tool: &str,
    args: &serde_json::Value,
    _root: &std::path::Path,
) -> Option<(RuleDecision, String)> {
    let command = args.get("command")?.as_str()?;
    (tool == "run_command" && command.trim_start().starts_with("rm "))
        .then(|| (RuleDecision::Deny, NO_RM.to_string()))
}

fn update(id: usize, path: &str) -> ToolCall {
    let mut call = ToolCall::new(
        format!("call-{id}"),
        "update_file",
        serde_json::json!({"path": path, "text": "new"}),
    );
    call.call_id = Some(format!("provider-call-{id}"));
    call
}

async fn run(calls: Vec<ToolCall>, rules: Vec<(&'static str, FixtureRule)>) -> Outcome {
    run_controlled(
        calls,
        false,
        10,
        false,
        false,
        false,
        ReplyControl {
            code_rules: rules,
            ..ReplyControl::default()
        },
    )
    .await
}

/// Whether the trajectory holds a call on `tool` that ended as `status` with a
/// detail containing `text`.
fn recorded_call(outcome: &Outcome, tool: &str, status: &str, text: &str) -> bool {
    let answer = outcome
        .store
        .query(&format!(
            "ASK {{ GRAPH <{}> {{ ?c a <urn:polaris:session:ToolCall> ; \
             <urn:polaris:session:tool> \"{tool}\" ; \
             <urn:polaris:session:status> \"{status}\" ; \
             <urn:polaris:session:detail> ?detail . \
             FILTER(CONTAINS(?detail, \"{text}\")) }} }}",
            crate::agents::trajectory::SESSION_GRAPH_IRI,
        ))
        .unwrap();
    matches!(answer, oxigraph::sparql::QueryResults::Boolean(true))
}

/// Whether the trajectory holds a decision that chose `chose` and cites a
/// policy reference containing `text`.
fn recorded_decision(outcome: &Outcome, chose: &str, text: &str) -> bool {
    let answer = outcome
        .store
        .query(&format!(
            "ASK {{ GRAPH <{}> {{ ?d <urn:polaris:session:chose> \"{chose}\" ; \
             <urn:polaris:session:consultedPolicy> ?reference . \
             FILTER(CONTAINS(?reference, \"{text}\")) }} }}",
            crate::agents::trajectory::SESSION_GRAPH_IRI,
        ))
        .unwrap();
    matches!(answer, oxigraph::sparql::QueryResults::Boolean(true))
}

#[tokio::test]
async fn a_code_rule_deny_reaches_the_model_and_the_trajectory_as_a_denial() {
    let calls = || vec![update(0, "generated/api.rs"), update(1, "src/lib.rs")];

    // Without the rule both edits run, so the rule is what stops the first.
    let free = run(calls(), Vec::new()).await;
    assert_eq!(free.ran, ["update generated/api.rs", "update src/lib.rs"]);

    let outcome = run(calls(), vec![("no-generated-edits", no_generated_edits)]).await;
    assert_eq!(
        outcome.ran,
        ["update src/lib.rs"],
        "the other edit still runs"
    );
    let results = results(&outcome);
    assert_eq!(results.len(), 2);
    assert!(results[0].0, "the refusal is an error result");
    assert_eq!(
        results[0].1,
        format!("Call refused: code rule \"no-generated-edits\": {NO_GENERATED_EDITS}")
    );
    assert!(!results[1].0, "{}", results[1].1);
    {
        let messages = outcome.io.messages.lock().unwrap();
        assert_eq!(closing_phases(&messages, "call-0"), ["denied"]);
        assert_eq!(closing_phases(&messages, "call-1"), ["ok"]);
    }
    assert!(recorded_call(
        &outcome,
        "update_file",
        "denied",
        NO_GENERATED_EDITS
    ));
    assert!(recorded_decision(
        &outcome,
        "deny",
        "authorization.source=code rule"
    ));
    assert!(recorded_decision(&outcome, "deny", NO_GENERATED_EDITS));
}

#[tokio::test]
async fn a_code_rule_sees_each_command_of_a_chain_and_refuses_the_whole_call() {
    // The whole text starts with `echo`; only the second command is `rm`.
    let chain = "echo cleaning && rm -rf generated";
    let free = run(vec![command(0, chain)], Vec::new()).await;
    assert_eq!(free.ran, [chain]);

    let outcome = run(vec![command(0, chain)], vec![("no-rm", no_rm_commands)]).await;
    assert!(outcome.ran.is_empty(), "nothing ran: {:?}", outcome.ran);
    let results = results(&outcome);
    assert!(results[0].0);
    assert_eq!(
        results[0].1,
        format!("Call refused: code rule \"no-rm\": {NO_RM}")
    );
    assert_eq!(
        closing_phases(&outcome.io.messages.lock().unwrap(), "call-0"),
        ["denied"]
    );
    assert!(recorded_call(&outcome, "run_command", "denied", NO_RM));
}

const EXACT_REFUSAL: &str =
    "  This call cannot change generated output.\nChange its generator instead.\n";

fn exact_generated_refusal(
    tool: &str,
    args: &serde_json::Value,
    root: &std::path::Path,
) -> Option<(RuleDecision, String)> {
    no_generated_edits(tool, args, root).map(|(decision, _)| (decision, EXACT_REFUSAL.into()))
}

#[tokio::test]
async fn a_code_rule_can_supply_the_complete_refusal_without_changing_provenance() {
    let outcome = run_controlled(
        vec![update(0, "generated/api.rs"), update(1, "src/lib.rs")],
        false,
        10,
        false,
        false,
        false,
        ReplyControl {
            code_rules: vec![("generated-output", exact_generated_refusal)],
            own_refusal_text: true,
            ..ReplyControl::default()
        },
    )
    .await;
    assert_eq!(outcome.ran, ["update src/lib.rs"]);
    let results = results(&outcome);
    assert!(results[0].0);
    assert_eq!(results[0].1, EXACT_REFUSAL);
    assert!(!results[1].0);
    assert!(recorded_decision(
        &outcome,
        "deny",
        "authorization.source=code rule"
    ));
    assert!(recorded_decision(&outcome, "deny", "generated-output"));

    let outcome = run_controlled(
        vec![command(0, "echo cleaning && rm -rf generated")],
        false,
        10,
        false,
        false,
        false,
        ReplyControl {
            code_rules: vec![("no-rm", no_rm_commands)],
            own_refusal_text: true,
            ..ReplyControl::default()
        },
    )
    .await;
    assert!(outcome.ran.is_empty());
    assert_eq!(super::results(&outcome)[0].1, NO_RM);
}
