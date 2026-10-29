//! Proof that a turn served by a selected topology reaches the user as an
//! answer.
//!
//! The live selecting host of `topology_selection_host_loop.rs`, driven for
//! what a *consumer* receives rather than for what the host recorded: the
//! stdout payload of each `--output-format`, the parsed `json` object, the
//! rendered `stream-json` lines, and the [`IOMessage`] stream a front end
//! renders. Nothing here asserts on `TurnOutcome::final_text` alone — that is
//! an internal field, and the acceptance is about the answer arriving.
//!
//! What each test pins down:
//!
//! - an executed topology's answer is the product the topology itself
//!   concluded, not the registration id and a machine tally;
//! - a terminal no-candidate answer is delivered, with the selector's own
//!   exclusion codes, rather than the turn ending silently;
//! - the accounting triple a topology turn reports is the same shape a
//!   conversational turn reports, with no second kind of usage record;
//! - every output format stays coherent and parseable for both kinds of turn;
//! - a conversational turn is untouched; and
//! - a product that accounts for nothing has no answer, so a registration
//!   with nothing to say cannot render as a blank turn.

mod common;

use std::collections::BTreeSet;

use common::{ANSWER, open_session, project, selecting_server};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, IOProvider};
use polaris::sessions::SessionsAPI;
use polaris_code::cosmos::host::{AcceptedCodingTask, TopologyProduct, TopologyTurn};
use polaris_code::cosmos::task::{CodingIntent, CodingObjective};
use polaris_code::cosmos::topology::PREDICTION_ERROR;
use polaris_code::exec::{OutputFormat, RunReport, stream_json_line};
use polaris_code::run::{self, StatusEmitter, TurnReason};
use polaris_code::terminal::transcript::{
    FINAL_TRUE, IO_TYPE_MESSAGE, META_FINAL, META_OUTCOME, OUTCOME_COMPLETE,
};
use polaris_cosmos::contracts::{ConclusionKind, StopReason};
use tokio::sync::mpsc;

/// The output token limit these tests report a turn as having run under.
const MAX_OUTPUT_TOKENS: u32 = 4096;

/// The text of a message, or `None` for a non-text modality.
fn text_of(message: &IOMessage) -> Option<&str> {
    match &message.content {
        IOContent::Text(text) => Some(text.as_str()),
        _ => None,
    }
}

/// Everything the front end's provider was sent, in order.
fn drain(output_rx: &mut mpsc::Receiver<IOMessage>) -> Vec<IOMessage> {
    let mut messages = Vec::new();
    while let Ok(message) = output_rx.try_recv() {
        messages.push(message);
    }
    messages
}

/// The closed transcript blocks in a drained stream: exactly the shape the
/// conversation panel renders an assistant answer from.
fn closed_answers(messages: &[IOMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| {
            message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_MESSAGE)
                && message.metadata.get(META_FINAL).map(String::as_str) == Some(FINAL_TRUE)
        })
        .filter_map(|message| text_of(message).map(str::to_owned))
        .collect()
}

/// The `stream-json` rendering of a drained stream, as the front end prints it.
fn stream_json(messages: &[IOMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|message| {
            let line = stream_json_line(message).expect("every message renders as one line");
            assert!(
                !line.contains('\n'),
                "a stream-json line must be one line: {line}"
            );
            serde_json::from_str(&line).expect("every stream-json line parses")
        })
        .collect()
}

/// The member names of the `usage` object a report serializes.
fn usage_keys(report: &RunReport) -> BTreeSet<String> {
    let json = report
        .stdout_payload(OutputFormat::Json)
        .expect("json always prints an object");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
    parsed["usage"]
        .as_object()
        .expect("a measured run reports its accounting")
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn an_executed_topology_returns_its_own_product_as_the_answer() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, mut output_rx) =
        selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let status = StatusEmitter::new(provider.clone());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn_with_status(sessions, &session_id, Some(&status)).await;
    provider.close().await;
    let messages = drain(&mut output_rx);

    assert_eq!(outcome.reason, TurnReason::Completed);
    let TopologyTurn::Executed(executed) = outcome
        .topology
        .as_ref()
        .expect("the turn selected a topology")
    else {
        panic!("expected an executed topology");
    };

    // The answer is what the topology concluded: the prediction-error loop
    // resolved the task with one passing assessment and stopped because its
    // predicate held. Every one of those facts comes from the conclusion the
    // graph produced, none from the host's own bookkeeping.
    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    let answer = report
        .stdout_payload(OutputFormat::Text)
        .expect("a served turn prints its answer");
    assert!(
        answer.contains("resolved"),
        "the answer carries the conclusion kind: {answer}"
    );
    assert!(
        answer.contains('1'),
        "the answer carries what was assessed: {answer}"
    );
    assert!(
        answer.contains("predicate"),
        "the answer carries why the topology stopped: {answer}"
    );
    assert!(
        !answer.contains(PREDICTION_ERROR),
        "a registration id is a machine label, not an answer: {answer}"
    );
    assert!(
        !answer.contains(&executed.decision_id.to_string()),
        "a decision id is a machine label, not an answer: {answer}"
    );

    // `json` carries the same answer under the member a consumer already reads
    // for a conversational turn, and says nothing that distinguishes the two
    // beyond `status`.
    let json = report
        .stdout_payload(OutputFormat::Json)
        .expect("json always prints an object");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
    assert_eq!(parsed["result"], answer.as_str());
    assert_eq!(parsed["status"], "completed");
    assert!(parsed["error"].is_null());
    assert_eq!(report.exit_code(), 0);

    // `stream-json` carries it too, and an interactive front end receives the
    // same closed transcript block it renders a conversational answer from.
    let lines = stream_json(&messages);
    assert!(
        lines
            .iter()
            .any(|line| line["content"]["Text"] == answer.as_str()),
        "the answer is in the message stream: {lines:?}"
    );
    assert_eq!(
        closed_answers(&messages),
        vec![answer.clone()],
        "exactly one closed answer block, exactly as a conversational turn sends"
    );
    let block = messages
        .iter()
        .find(|message| message.metadata.get(META_FINAL).map(String::as_str) == Some(FINAL_TRUE))
        .expect("the answer block is closed");
    assert_eq!(
        block.metadata.get(META_OUTCOME).map(String::as_str),
        Some(OUTCOME_COMPLETE)
    );
}

#[tokio::test]
async fn a_no_candidate_answer_is_delivered_with_the_selector_explanation() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, mut output_rx) =
        selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let status = StatusEmitter::new(provider.clone());

    // No coding topology in this embodiment offers a comparative verifier.
    queue.accept(AcceptedCodingTask::new(task).with_objective(
        CodingObjective::new(CodingIntent::Careful).requiring_comparative_verifier(),
    ));
    let outcome = run::drive_turn_with_status(sessions, &session_id, Some(&status)).await;
    provider.close().await;
    let messages = drain(&mut output_rx);

    assert_eq!(outcome.reason, TurnReason::NoTopologyStarted);
    let TopologyTurn::NoEligibleCandidate(unstarted) =
        outcome.topology.as_ref().expect("the turn made a decision")
    else {
        panic!("expected a no-eligible answer");
    };
    let code = unstarted.reasons[0].clone();

    // The turn does not end silently: the user is told, on stdout, in the same
    // place a conversational answer appears.
    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    let answer = report
        .stdout_payload(OutputFormat::Text)
        .expect("a terminal no-candidate answer is still an answer");
    assert!(
        answer.contains("no eligible candidate"),
        "the answer says what happened: {answer}"
    );
    assert!(
        answer.contains(&code),
        "the answer carries the selector's own code: {answer}"
    );

    // It is not reported as a completed empty turn: `status` and the exit code
    // are the fields that say which kind of turn this was.
    let json = report
        .stdout_payload(OutputFormat::Json)
        .expect("json always prints an object");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
    assert_eq!(parsed["status"], "no-topology-started");
    assert_eq!(parsed["result"], answer.as_str());
    assert_eq!(
        parsed["error"],
        answer.as_str(),
        "the answer and the explanation are one text, rendered once"
    );
    assert_eq!(report.exit_code(), 1);
    assert_eq!(
        report
            .error_line()
            .expect("the stderr diagnostic still explains it"),
        format!("polaris: no topology started: {answer}")
    );

    // An interactive front end receives it as a closed answer block too, so
    // the TUI does not sit there having shown nothing.
    assert_eq!(closed_answers(&messages), vec![answer.clone()]);
    let lines = stream_json(&messages);
    assert!(
        lines
            .iter()
            .any(|line| line["content"]["Text"] == answer.as_str()),
        "the answer is in the message stream: {lines:?}"
    );
}

#[tokio::test]
async fn a_topology_turn_reports_the_same_accounting_shape_as_a_conversational_turn() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let served = run::drive_turn(sessions, &session_id).await;
    let served_report = RunReport::new(&session_id, &served, MAX_OUTPUT_TOKENS);

    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("anything else?".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);
    let chatted = run::drive_turn(sessions, &session_id).await;
    provider.close().await;
    let chatted_report = RunReport::new(&session_id, &chatted, MAX_OUTPUT_TOKENS);

    // One shape of usage record, not two: the same members, the same schema,
    // the same model id the session actually ran under.
    assert_eq!(usage_keys(&served_report), usage_keys(&chatted_report));
    let served_usage = served.usage.as_ref().expect("a topology turn is measured");
    let chatted_usage = chatted
        .usage
        .as_ref()
        .expect("a conversational turn is measured");
    assert_eq!(served_usage.schema, chatted_usage.schema);
    assert_eq!(served_usage.model_id, chatted_usage.model_id);
    assert_eq!(served_usage.pricing_basis, chatted_usage.pricing_basis);

    // The run id is the executor's, reported on the same terms.
    assert!(
        served_report.run_id.is_some(),
        "the selected graph's run id is the turn's run id"
    );
    assert!(chatted_report.run_id.is_some());
    assert!(!served_report.identity_line().contains("run_id=-"));

    // The turn number is the session's own reading after the turn, for both.
    let final_turn = sessions
        .session_info(&session_id)
        .expect("the session is addressable")
        .turn_number;
    assert_eq!(chatted_report.turn_number, final_turn);
    assert_eq!(
        served_report.turn_number, 0,
        "a topology turn runs outside process_turn_with, so it renumbers nothing"
    );
}

#[tokio::test]
async fn a_conversational_turn_is_unchanged() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, mut output_rx) =
        selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let status = StatusEmitter::new(provider.clone());

    assert!(queue.is_empty());
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("is anything wrong?".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);
    let outcome = run::drive_turn_with_status(sessions, &session_id, Some(&status)).await;
    provider.close().await;
    let messages = drain(&mut output_rx);

    // The agent's own answer, once, through the agent's own transcript block.
    // The selection point emitted nothing beside it.
    assert!(outcome.topology.is_none());
    assert_eq!(outcome.reason, TurnReason::Completed);
    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    assert_eq!(
        report.stdout_payload(OutputFormat::Text).as_deref(),
        Some(ANSWER)
    );
    assert_eq!(closed_answers(&messages), vec![ANSWER.to_string()]);
    assert_eq!(report.exit_code(), 0);
    assert_eq!(report.status, "completed");
}

#[test]
fn a_product_that_accounts_for_nothing_has_no_answer() {
    // The floor the host reads is the coding contract's own declared output,
    // so a product is the only thing the host projects. One that assessed
    // nothing has nothing to say, and says so rather than rendering as an
    // empty answer — which is why `execute_permitted` refuses to return it.
    let silent = TopologyProduct {
        kind: ConclusionKind::Resolved,
        stop_reason: StopReason::WorkExhausted,
        pass: 0,
        issue: 0,
        not_applicable: 0,
        indeterminate: 0,
    };
    assert_eq!(silent.assessed(), 0);
    assert_eq!(silent.answer(), None);

    let spoken = TopologyProduct { pass: 1, ..silent };
    assert_eq!(spoken.assessed(), 1);
    let answer = spoken.answer().expect("a product that assessed something");
    assert!(!answer.is_empty());
    assert!(answer.contains("resolved"), "unexpected answer: {answer}");
}

#[test]
fn an_all_not_applicable_product_never_renders_as_resolved() {
    // This is the host-side backstop for a graph that changed no files and
    // produced only a verifier's "not applicable" placeholder. The graph
    // conclusion normally prevents this earlier; the renderer must preserve
    // the same truth if a future registration bypasses that helper.
    let product = TopologyProduct {
        kind: ConclusionKind::Resolved,
        stop_reason: StopReason::PredicateSatisfied,
        pass: 0,
        issue: 0,
        not_applicable: 1,
        indeterminate: 0,
    };

    let answer = product
        .answer()
        .expect("the inapplicable verdict is visible");
    assert!(
        answer.starts_with("left the coding task unresolved"),
        "{answer}"
    );
    assert!(!answer.starts_with("resolved the coding task"), "{answer}");
}
