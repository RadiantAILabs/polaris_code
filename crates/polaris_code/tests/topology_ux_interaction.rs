//! Proof that the four topology-UX features — per-task direction, effort
//! auto-routing, selection explanation, and two-topology comparison — compose
//! coherently where they touch the same record and the same run surface,
//! rather than merely coexisting in the same binary.
//!
//! Each feature has its own tests; none of them can see the other three. What
//! this file pins down is specifically the seams where they meet:
//!
//! - a directed run's panel message names *both* what was asked for
//!   ([`TopologyDirective::Only`]) and why it won over the alternatives
//!   (`SelectionRationale`) — one explanation, not two competing ones;
//! - an auto-routed answer ([`TaskOrigin::AutoRouted`]) is explained exactly
//!   as richly as an explicit one, because the explanation is a pure
//!   projection of the decision record and never branches on how the task was
//!   accepted;
//! - [`TopologyDirective::None`] (an explicit "run nothing") is never
//!   mistaken for an auto-routed guess that needs the effort router's
//!   fallthrough, even when a caller builds both knobs onto the same
//!   accepted task through the public API — the combination that makes
//!   `TopologyTurn::origin`/`is_conclusive` need an arm for
//!   `TopologyTurn::Declined` (the per-task direction's variant; the two
//!   methods reading it belong to effort routing).

mod common;

use common::{ANSWER, open_session, project, selecting_server};
use polaris::plugins::{IOContent, IOMessage, IOProvider};
use polaris::sessions::SessionsAPI;
use polaris_code::cosmos::host::{
    AcceptedCodingTask, TaskOrigin, TopologyDirective, TopologyTurn, auto_route,
};
use polaris_code::cosmos::task::CodingIntent;
use polaris_code::cosmos::topology::DIRECT;
use polaris_code::run::{self, TurnReason};
use polaris_code::terminal::topology::turn_report;

/// Renders a turn's panel message, the same way a live session would show it.
fn report_text(outcome: &polaris_code::run::TurnOutcome) -> String {
    let message = turn_report(outcome).expect("a topology turn reports itself");
    match message.content {
        IOContent::Text(text) => text,
        other => panic!("expected a text report, got {other:?}"),
    }
}

/// A directed run's explanation names both halves at once: that it ran
/// because it was asked for, and why it beat the alternatives the decision
/// actually ranked.
///
/// Each half has its own coverage elsewhere — `topology_directed_selection.rs`
/// checks `requested` on the record, `topology_work_affordance.rs` checks "why
/// this one" on an *autonomous* run's panel message. Neither exercises a
/// directed run's panel message, which is exactly the case
/// `render_topology_turn` (carrying the per-task `requested` note and the
/// selection rationale in the same function) has to get right for both
/// features to still say true things about the same run.
#[tokio::test]
async fn a_directed_runs_panel_message_names_both_what_was_asked_for_and_why_it_won() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::Only(DIRECT.to_string())),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", outcome.topology);
    };
    assert_eq!(executed.registration_id, DIRECT);
    assert_eq!(executed.requested.as_deref(), Some(DIRECT));

    let report = report_text(&outcome);
    assert!(
        report.contains("you asked for this topology"),
        "the directed note is still shown for a directed run: {report}"
    );
    assert!(
        report.contains("why this one"),
        "the rationale explanation still runs for a directed decision, not only an \
         autonomous one: {report}"
    );
    assert!(report.contains("alternatives considered"), "{report}");
}

/// An auto-routed answer is explained exactly as an explicit one would be:
/// the explanation is a pure projection of the decision record, and the
/// record does not carry how the task was accepted.
///
/// This is the other half of the same seam: effort routing's light path
/// answers through the same `ExecutedTopology` any other run does, so the
/// selection explanation's rendering — which reads
/// `run.candidates`/`run.rationale`, never `run.origin` — has nothing extra
/// to special-case for it.
#[tokio::test]
async fn an_auto_routed_answer_is_explained_the_same_way_an_explicit_one_is() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    let accepted =
        auto_route(project_dir.path(), "where is fn add").expect("a recognised question routes");
    assert_eq!(accepted.origin(), TaskOrigin::AutoRouted);
    queue.accept(accepted);

    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", outcome.topology);
    };
    assert_eq!(
        executed.origin,
        TaskOrigin::AutoRouted,
        "sanity: this run really was auto-routed"
    );
    assert_eq!(
        executed.requested, None,
        "auto-routing never names a topology; it only marks the origin"
    );

    let report = report_text(&outcome);
    assert!(
        report.contains("why this one"),
        "the explanation feature does not gate on task origin: {report}"
    );
    assert!(report.contains("alternatives considered"), "{report}");
    assert!(
        !report.contains("you asked for this topology"),
        "auto-routing named no topology, so the directed note must not appear: {report}"
    );
}

/// [`TopologyDirective::None`] — an explicit, honoured "run nothing" — is
/// never treated as an unconfirmed auto-routed guess that needs the effort
/// router's fallthrough, even when both knobs are set on the same accepted
/// task through the public builder API.
///
/// `auto_route` itself never produces this combination (it only ever sets
/// [`TaskOrigin::AutoRouted`], never [`TopologyDirective::None`]), but
/// [`AcceptedCodingTask::with_topology_directive`] and
/// [`AcceptedCodingTask::auto_routed`] are both public, so a future caller —
/// or a future, broader auto-router — can reach it. This is exactly the
/// shape that makes `TopologyTurn::origin`/`is_conclusive` need an arm for the
/// `Declined` variant (added by per-task direction) alongside the two methods
/// reading it (added by effort routing). `Declined` is always conclusive, so `run_one_turn`'s
/// `origin() == AutoRouted && !is_conclusive()` fallthrough guard can never
/// fire for it, regardless of origin.
#[tokio::test]
async fn declining_a_topology_is_never_treated_as_an_unconfirmed_auto_routed_guess() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::None)
            .auto_routed(),
    );
    // If the fallthrough guard mistakenly fired, this is what it would fall
    // through to — present so a regression would be caught as "answered by
    // the conversational turn" rather than merely "topology is None".
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

    assert_eq!(outcome.reason, TurnReason::Completed);
    let Some(TopologyTurn::Declined(declined)) = outcome.topology.as_ref() else {
        panic!(
            "expected the decline to be this turn's answer, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(declined.origin, TaskOrigin::AutoRouted);
    assert_ne!(
        outcome.final_text.as_deref(),
        Some(ANSWER),
        "the conversational turn never ran: the decline is honest and terminal regardless \
         of origin"
    );

    let report = report_text(&outcome);
    assert!(report.contains("because you asked for none"), "{report}");
}
