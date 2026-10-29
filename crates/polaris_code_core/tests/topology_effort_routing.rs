//! Proof that effort matches what a turn actually needs.
//!
//! `auto_route` (`cosmos/host.rs`) classifies conversational text as a
//! light-effort question, before any model call, and accepts it the same way
//! an explicit `--topology-task` accept would — marked
//! [`TaskOrigin::AutoRouted`]. The run surface's selection point
//! (`run.rs::run_one_turn`) then honours that answer only when it is
//! [`TopologyTurn::is_conclusive`]; an inconclusive auto-routed answer falls
//! through to the ordinary conversational turn within the same request,
//! rather than surfacing a guess as the turn's result.
//!
//! What each test pins down:
//!
//! - a recognised, answerable question is served by the light path, with no
//!   model round trip and no session turn spent (red-then-green: the light
//!   path answers directly, cheaply);
//! - a recognised question the light path cannot actually resolve is not
//!   surfaced as the turn's answer: the turn falls through and the ordinary
//!   conversational graph answers instead, at bounded extra cost;
//! - the same fallthrough applies when the light path's selection finds no
//!   eligible candidate at all, not only when an executed run escalates;
//! - an *explicit* accept that reaches the same inconclusive shape remains
//!   terminal for the turn — the protected `NoEligibleCandidate`-is-terminal
//!   rule is revised only for `TaskOrigin::AutoRouted`, never for `Explicit`;
//! - the light path is deterministic: the same question, asked twice, is
//!   answered the same way both times.

mod common;

use std::time::Instant;

use common::{ANSWER, open_session, project, selecting_server};
use polaris::plugins::{IOContent, IOMessage, IOProvider};
use polaris::sessions::SessionsAPI;
use polaris_code_core::cosmos::host::{AcceptedCodingTask, TopologyTurn, auto_route};
use polaris_code_core::cosmos::task::{CodingIntent, CodingObjective};
use polaris_code_core::cosmos::topology::TREE_QUESTION;
use polaris_code_core::run::{self, TurnReason};
use polaris_cosmos::contracts::{ConclusionKind, SelectionOutcome};
use polaris_cosmos::evidence::TopologyEvidence;

/// A generous ceiling for the light path's own graph execution: three
/// systems over local file reads, no network and no model call. Kept loose
/// so the test is not flaky under load, while still catching a regression
/// that made the light path expensive (e.g. it started calling the model).
const LIGHT_PATH_BUDGET_MS: u128 = 2_000;

#[tokio::test]
async fn a_recognised_question_is_served_by_the_light_path_with_no_model_turn() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    let accepted =
        auto_route(project_dir.path(), "where is fn add").expect("a recognised question routes");
    assert_eq!(
        accepted.origin(),
        polaris_code_core::cosmos::host::TaskOrigin::AutoRouted
    );
    queue.accept(accepted);

    let started = Instant::now();
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;
    let elapsed = started.elapsed();

    assert_eq!(outcome.reason, TurnReason::Completed);
    let TopologyTurn::Executed(executed) = outcome
        .topology
        .as_ref()
        .expect("the light path answered this turn")
    else {
        panic!("expected an executed topology");
    };
    assert_eq!(executed.product.kind, ConclusionKind::Resolved);
    let answer = outcome
        .final_text
        .as_deref()
        .expect("the light path's own conclusion is the turn's answer");
    assert!(
        answer.contains("resolved"),
        "the answer carries the conclusion kind: {answer}"
    );
    assert_ne!(
        answer, ANSWER,
        "this was answered by the light path, not by a model turn"
    );

    // The light path runs outside `process_turn_with`, so it spends no turn:
    // exactly the same accounting shape a topology turn already reports.
    assert_eq!(
        outcome.turn_number, 0,
        "the light path answers without advancing the session's own turn count"
    );

    assert!(
        elapsed.as_millis() < LIGHT_PATH_BUDGET_MS,
        "the light path took {elapsed:?}, longer than a local file scan should ever take"
    );
}

#[tokio::test]
async fn a_question_the_light_path_cannot_resolve_falls_through_to_the_conversational_turn() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    let prompt = "what is in missing.rs";
    let accepted =
        auto_route(project_dir.path(), prompt).expect("an inspect question still routes");
    queue.accept(accepted);

    // The same prompt a `--print` run always enqueues regardless of whether a
    // light-path task was also queued (`main.rs::run_headless`): the
    // conversational fallback needs it if the light path guesses wrong.
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text(prompt.to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);

    let started = Instant::now();
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;
    let elapsed = started.elapsed();

    // Not surfaced as this turn's answer: the file cannot be read, so the
    // light path's own conclusion is `Escalated`, and an auto-routed accept
    // that reaches no usable conclusion is not this turn's answer.
    assert!(
        outcome.topology.is_none(),
        "an inconclusive auto-routed attempt is not carried as this turn's answer"
    );
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(
        outcome.final_text.as_deref(),
        Some(ANSWER),
        "the conversational turn answered instead, in the same request"
    );

    // The wasted light-path attempt is a bounded, cheap detour: the whole
    // turn (failed guess plus the real conversational answer) still finishes
    // quickly, because the failed guess never touched the model.
    assert!(
        elapsed.as_millis() < LIGHT_PATH_BUDGET_MS * 4,
        "the fallthrough turn took {elapsed:?}, more than a bounded wasted guess should cost"
    );

    let turn_number = sessions
        .session_info(&session_id)
        .expect("the session is addressable")
        .turn_number;
    assert_eq!(
        turn_number, 1,
        "the conversational turn that actually answered is the one the session counted"
    );
}

#[tokio::test]
async fn a_light_path_selection_with_no_eligible_candidate_also_falls_through() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    let prompt = "where is fn add";
    // No coding topology in this embodiment offers a comparative verifier, so
    // this forces `NoEligibleCandidate` even though the question itself is
    // one the light path would otherwise answer.
    let accepted = auto_route(project_dir.path(), prompt)
        .expect("a recognised question routes")
        .with_objective(
            CodingObjective::new(CodingIntent::Answer).requiring_comparative_verifier(),
        );
    queue.accept(accepted);

    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text(prompt.to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);

    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert!(
        outcome.topology.is_none(),
        "a no-eligible-candidate auto-routed attempt also falls through, not only an escalated run"
    );
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(outcome.final_text.as_deref(), Some(ANSWER));
}

#[tokio::test]
async fn an_explicit_accept_that_reaches_the_same_shape_remains_terminal_for_the_turn() {
    // The one established rule effort routing leaves alone — `NoEligibleCandidate`
    // (and, by the same `is_conclusive` reading, an escalated run) is
    // terminal for the turn — stands unrevised for an `Explicit` accept.
    // Only `TaskOrigin::AutoRouted` gets the fallthrough.
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task).with_objective(
        CodingObjective::new(CodingIntent::Careful).requiring_comparative_verifier(),
    ));

    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("anything else?".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);

    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::NoTopologyStarted);
    let TopologyTurn::NoEligibleCandidate(unstarted) = outcome
        .topology
        .as_ref()
        .expect("an explicit no-eligible-candidate answer is still this turn's answer")
    else {
        panic!("expected a no-eligible answer");
    };
    assert_eq!(
        unstarted.origin,
        polaris_code_core::cosmos::host::TaskOrigin::Explicit
    );
    assert_ne!(
        outcome.final_text.as_deref(),
        Some(ANSWER),
        "the conversational turn never ran: the explicit accept ended the turn on its own"
    );
    assert!(
        outcome
            .final_text
            .as_deref()
            .expect("a terminal no-candidate answer is still an answer")
            .contains("no eligible candidate"),
    );
}

#[tokio::test]
async fn the_light_path_answers_the_same_recognised_question_the_same_way_twice() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        auto_route(project_dir.path(), "where is fn add").expect("a recognised question routes"),
    );
    let first = run::drive_turn(sessions, &session_id).await;
    let TopologyTurn::Executed(first_run) = first
        .topology
        .as_ref()
        .expect("the first turn was answered by the light path")
    else {
        panic!("expected an executed topology");
    };
    let first_kind = first_run.product.kind;
    let first_answer = first_run.product.answer();

    // A topology-served turn does not advance the session's own turn number
    // (asserted above by the first test in this file), and the selector's own
    // event identities are seeded from that number — so replaying the light
    // path immediately, at the same turn number, is not a scenario the host
    // ever sees live. An intervening conversational turn is what actually
    // separates two accepted tasks in the live loop, exactly as
    // `topology_selection_host_loop.rs` already exercises.
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("thanks".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    let between = run::drive_turn(sessions, &session_id).await;
    assert!(between.topology.is_none());

    queue.accept(
        auto_route(project_dir.path(), "where is fn add").expect("a recognised question routes"),
    );
    let second = run::drive_turn(sessions, &session_id).await;
    drop(input_tx);
    provider.close().await;
    let TopologyTurn::Executed(second_run) = second
        .topology
        .as_ref()
        .expect("the second turn was answered by the light path")
    else {
        panic!("expected an executed topology");
    };

    assert_eq!(first_kind, second_run.product.kind);
    assert_eq!(first_answer, second_run.product.answer());

    // Not just equal outcomes in memory: the grounds for each decision are
    // independently recoverable from what the turn persisted, by decision id
    // alone, the same way `topology_selection_host_loop.rs` reads back an
    // explicit accept's evidence episode.
    assert_eq!(first_run.registration_id, TREE_QUESTION);
    assert_eq!(second_run.registration_id, TREE_QUESTION);
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");
    let first_episode = evidence
        .read_episode(first_run.decision_id)
        .expect("the first episode is readable")
        .expect("the first decision was persisted");
    let second_episode = evidence
        .read_episode(second_run.decision_id)
        .expect("the second episode is readable")
        .expect("the second decision was persisted");
    for episode in [&first_episode, &second_episode] {
        let SelectionOutcome::Selected { selected, .. } = episode.selection.decision().outcome()
        else {
            panic!("expected a selected outcome in the persisted episode");
        };
        assert_eq!(
            selected.registration_id().as_str(),
            TREE_QUESTION,
            "the persisted episode names the same registration the turn reported"
        );
    }
}
