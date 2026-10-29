//! Proof that a user can direct topology selection per submitted task —
//! name one exactly, or ask for none — and is told plainly when a directed
//! request cannot be honoured.
//!
//! Naming a topology or turning
//! selection off are **commands**, scoped to the one submitted task they ride
//! with (`TopologyDirective` on `AcceptedCodingTask`), never a sticky session
//! setting and never a preference the selector merely weighs. Every test here
//! drives the live selection point through [`polaris_code_core::run::drive_turn`],
//! exactly as `topology_selection_host_loop.rs` does, so what is proved is the
//! real branch a turn takes, not a unit test of `select` in isolation.

mod common;

use common::{ANSWER, open_session, project, selecting_server};
use polaris::plugins::{IOContent, IOMessage, IOProvider};
use polaris::sessions::SessionsAPI;
use polaris_code_core::cosmos::host::{AcceptedCodingTask, TopologyDirective, TopologyTurn};
use polaris_code_core::cosmos::task::CodingIntent;
use polaris_code_core::cosmos::topology::{
    COVERAGE_AUDIT, DIRECT, MODEL_PROPOSED, PREDICTION_ERROR,
};
use polaris_code_core::run::{self, TurnReason};

/// A directed, admissible request wins over whatever the autonomous selector
/// would have picked, and the record shows it ran because it was asked for.
///
/// Without a directive, a `Careful`-intent task always runs the autonomous
/// pick — `code.prediction-error-edit`, pinned by
/// `an_executed_topology_returns_its_own_product_as_the_answer` in
/// `topology_turn_answer.rs`. Naming `code.direct-edit` instead has to change
/// which one actually runs, not just how it is reported.
#[tokio::test]
async fn a_named_admissible_topology_wins_over_the_autonomous_pick() {
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
    assert_eq!(
        executed.registration_id, DIRECT,
        "the named topology ran, not the selector's own autonomous pick \
         (`code.prediction-error-edit`)"
    );
    assert_ne!(
        executed.registration_id, PREDICTION_ERROR,
        "sanity: the autonomous pick under this intent must differ from the one we named, \
         or this test would pass for the wrong reason"
    );
    // The record shows it ran because it was asked for.
    assert_eq!(
        executed.requested.as_deref(),
        Some(DIRECT),
        "a directed run's own record names what was requested"
    );
}

/// `code.coverage-audit` actually runs on a hermetic host, not just admits.
///
/// Its graph reads three resources (`Worklist`, `Ledger`, `SweepControl`,
/// private to `polaris_cosmos::cognition::approved`) that only
/// `coverage_sweep::prepare_context` can seed — nothing exercised this
/// registration's own live execution path before, so a missing call to that
/// function went unnoticed: every accepted turn failed live resource
/// validation instead of running.
#[tokio::test]
async fn a_directed_coverage_audit_actually_runs_not_just_admits() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::Only(COVERAGE_AUDIT.to_string())),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::Completed, "{outcome:?}");
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected the coverage audit to run, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(executed.registration_id, COVERAGE_AUDIT);
    assert!(
        executed.nodes_executed > 0,
        "the coverage-audit graph really ran: {executed:?}"
    );
}

/// A directed run and an autonomous run of the *same* topology are
/// distinguishable in the record: `requested` is `Some` only for the one that
/// was asked for by name.
///
/// Two independent servers, deliberately, each with its own fresh evidence
/// store: outcomes feed later rankings, so a directed run's recorded pass can
/// shift what an *unrelated* later task's autonomous selection prefers if the
/// two share one store. That is a real, known limitation of evidence not
/// itself being tagged directed — it is not what this test is about, so it is
/// designed out here rather than around.
#[tokio::test]
async fn a_directed_run_and_an_autonomous_run_are_distinguishable_in_the_record() {
    let (directed_dir, directed_task) = project(CodingIntent::Careful);
    let directed_data = tempfile::tempdir().expect("temp data dir");
    let (directed_server, directed_queue, directed_provider, _tx, _rx) =
        selecting_server(directed_data.path()).await;
    let directed_sessions = directed_server.api::<SessionsAPI>().expect("SessionsAPI");
    let directed_session = open_session(&directed_server, directed_dir.path());
    directed_queue.accept(
        AcceptedCodingTask::new(directed_task)
            .with_topology_directive(TopologyDirective::Only(PREDICTION_ERROR.to_string())),
    );
    let directed_outcome = run::drive_turn(directed_sessions, &directed_session).await;
    directed_provider.close().await;
    let Some(TopologyTurn::Executed(directed)) = directed_outcome.topology.as_ref() else {
        panic!(
            "expected the directed turn to execute, got {:?}",
            directed_outcome.topology
        );
    };
    assert_eq!(directed.registration_id, PREDICTION_ERROR);
    assert_eq!(directed.requested.as_deref(), Some(PREDICTION_ERROR));

    // Autonomous: no directive at all, on its own server with its own cold
    // evidence store. The selector picks the same registration on priors
    // alone, exactly as `an_executed_topology_returns_its_own_product_as_the_answer`
    // in `topology_turn_answer.rs` already pins for this crate shape and intent.
    let (auto_dir, auto_task) = project(CodingIntent::Careful);
    let auto_data = tempfile::tempdir().expect("temp data dir");
    let (auto_server, auto_queue, auto_provider, _tx, _rx) =
        selecting_server(auto_data.path()).await;
    let auto_sessions = auto_server.api::<SessionsAPI>().expect("SessionsAPI");
    let auto_session = open_session(&auto_server, auto_dir.path());
    auto_queue.accept(AcceptedCodingTask::new(auto_task));
    let auto_outcome = run::drive_turn(auto_sessions, &auto_session).await;
    auto_provider.close().await;
    let Some(TopologyTurn::Executed(autonomous)) = auto_outcome.topology.as_ref() else {
        panic!(
            "expected the autonomous turn to execute, got {:?}",
            auto_outcome.topology
        );
    };
    assert_eq!(
        autonomous.registration_id, PREDICTION_ERROR,
        "sanity: both runs selected the same registration"
    );
    assert_eq!(
        autonomous.requested, None,
        "the autonomous run's own record carries no requested topology, even though it \
         picked the exact same registration the directed run named"
    );
}

/// Naming a topology the host cannot admit here is refused with *its own*
/// mechanical reason, and nothing else runs in its place — not a fallback
/// candidate, not the conversational agent.
///
/// `code.model-proposed-edit` declares a provider requirement
/// (`CandidateRequirements`) this test's host never wires (`selecting_server`
/// installs no `AnthropicPlugin`), so mechanical admission — run unmodified,
/// over the whole catalog, regardless of the directive — excludes it with
/// `provider.unavailable`. A directed request must report exactly that
/// candidate's own ground, never the aggregate noise from every other
/// candidate this decision also excluded to narrow the field down to one.
#[tokio::test]
async fn an_inadmissible_named_topology_is_refused_with_its_own_reason_and_nothing_runs() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::Only(MODEL_PROPOSED.to_string())),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::NoTopologyStarted);
    let Some(TopologyTurn::NoEligibleCandidate(unstarted)) = outcome.topology.as_ref() else {
        panic!(
            "expected a refused directed request, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(unstarted.requested.as_deref(), Some(MODEL_PROPOSED));
    assert_eq!(
        unstarted.reasons,
        vec!["provider.unavailable".to_string()],
        "the refusal names the requested candidate's own ground, not the noise from every \
         other candidate this decision also excluded to narrow the field to just that one: {:?}",
        unstarted.reasons
    );

    // Nothing ran in its place: the file the task would have edited is
    // untouched, and the turn started no graph at all.
    assert!(
        std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
            .expect("the file is still there")
            .contains("todo!()"),
        "no candidate admission refused was substituted in for the one named"
    );
    assert_eq!(outcome.run_id, None, "no graph started for this turn");
}

/// `TopologyDirective::None` runs no topology for the accepted task, the turn
/// still completes normally — not as a failed no-start answer, because the
/// request was fully honoured — and the workspace is untouched.
#[tokio::test]
async fn no_topology_directive_runs_nothing_and_the_turn_completes_normally() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task).with_topology_directive(TopologyDirective::None));
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert!(matches!(
        outcome.topology.as_ref(),
        Some(TopologyTurn::Declined(_))
    ));
    assert_eq!(
        outcome.reason,
        TurnReason::Completed,
        "an honoured 'run nothing' request completes the turn, it does not fail it"
    );
    assert!(
        outcome
            .final_text
            .as_deref()
            .is_some_and(|text| text.contains("none")),
        "the turn still says what happened, rather than ending silently: {:?}",
        outcome.final_text
    );
    assert!(
        std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
            .expect("the file is still there")
            .contains("todo!()"),
        "the workspace is untouched: 'none' must not run the default topology quietly"
    );
}

/// The directive is scoped to the task it was submitted with, not to the
/// session or to whatever turn follows: a second task accepted with no
/// directive of its own is selected autonomously, even immediately after a
/// directed task ran on the same session.
#[tokio::test]
async fn the_directive_does_not_outlive_the_task_it_was_submitted_with() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");

    let (first_dir, first_task) = project(CodingIntent::Careful);
    let first_session = open_session(&server, first_dir.path());
    queue.accept(
        AcceptedCodingTask::new(first_task)
            .with_topology_directive(TopologyDirective::Only(DIRECT.to_string())),
    );
    let first = run::drive_turn(sessions, &first_session).await;
    let Some(TopologyTurn::Executed(first_executed)) = first.topology.as_ref() else {
        panic!(
            "expected the first, directed turn to execute, got {:?}",
            first.topology
        );
    };
    assert_eq!(first_executed.registration_id, DIRECT);

    // A second task, on a fresh crate, submitted with no directive of its own.
    let (second_dir, second_task) = project(CodingIntent::Careful);
    let second_session = open_session(&server, second_dir.path());
    queue.accept(AcceptedCodingTask::new(second_task));
    let second = run::drive_turn(sessions, &second_session).await;
    provider.close().await;

    let Some(TopologyTurn::Executed(second_executed)) = second.topology.as_ref() else {
        panic!(
            "expected the second, undirected turn to execute, got {:?}",
            second.topology
        );
    };
    assert_eq!(
        second_executed.registration_id, PREDICTION_ERROR,
        "an undirected task after a directed one still gets the ordinary autonomous pick, \
         not the previous task's `code.direct-edit` request"
    );
    assert_eq!(
        second_executed.requested, None,
        "the earlier directive left no trace on a task that never carried one"
    );
}

/// Sanity anchor: a plain conversational turn (nothing accepted) is
/// unaffected by any of the above — the same property
/// `topology_turn_answer.rs` already pins, restated here so this file stands
/// on its own.
#[tokio::test]
async fn a_conversational_turn_still_ignores_the_selection_point_entirely() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    assert!(queue.is_empty());
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("hello".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert!(outcome.topology.is_none());
    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(outcome.final_text.as_deref(), Some(ANSWER));
}
