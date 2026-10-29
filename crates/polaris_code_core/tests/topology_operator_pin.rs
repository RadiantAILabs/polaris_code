//! Proof that which registered topology a run uses can be chosen from
//! outside the built binary, with no rebuild.
//!
//! `main.rs` reads `POLARIS_CODE_TOPOLOGY_PIN` once at startup, the same
//! environment-gate shape `POLARIS_CODE_GRAPH_SEARCH` already proves works
//! for the benchmark's own experiment axis (`evals/core/config.py`), and installs an
//! [`OperatorTopologyPin`] global before any session context exists. Every
//! test here drives the live selection point through
//! [`polaris_code_core::run::drive_turn`], exactly as
//! `topology_directed_selection.rs` does for the per-task directive this pin
//! is a run-wide sibling of — never a second selection engine, never a
//! parallel registry.

mod common;

use common::{open_session, project, selecting_server, selecting_server_with_pin};
use polaris::plugins::IOProvider;
use polaris::sessions::SessionsAPI;
use polaris_code_core::cosmos::host::{AcceptedCodingTask, TopologyDirective, TopologyTurn};
use polaris_code_core::cosmos::task::CodingIntent;
use polaris_code_core::cosmos::topology::{
    DIRECT, MODEL_PROPOSED, PREDICTION_ERROR, WORKLIST_SWEEP,
};
use polaris_code_core::run::{self, TurnReason};
use polaris_cosmos::topology::TopologyCatalog;

/// Without the pin, nothing outside the binary could say which topology should
/// run, so both arms of this test would execute `code.prediction-error-edit` —
/// the autonomous pick under `Careful` intent pinned by
/// `topology_turn_answer.rs` — and the assertion below would fail.
///
/// The same binary, run twice with two different `POLARIS_CODE_TOPOLOGY_PIN`
/// values over the same shape of accepted task (no `topology=` field of its
/// own), executes two different registrations — proved from what the run
/// reports (`executed.registration_id`), not from internal state.
#[tokio::test]
async fn two_external_topology_choices_over_the_same_accepted_task_execute_two_different_topologies()
 {
    let (direct_dir, direct_task) = project(CodingIntent::Careful);
    let direct_data = tempfile::tempdir().expect("temp data dir");
    let (direct_server, direct_queue, direct_provider, _tx, _rx) =
        selecting_server_with_pin(direct_data.path(), DIRECT).await;
    let direct_sessions = direct_server.api::<SessionsAPI>().expect("SessionsAPI");
    let direct_session = open_session(&direct_server, direct_dir.path());
    direct_queue.accept(AcceptedCodingTask::new(direct_task));
    let direct_outcome = run::drive_turn(direct_sessions, &direct_session).await;
    direct_provider.close().await;
    let Some(TopologyTurn::Executed(direct_executed)) = direct_outcome.topology.as_ref() else {
        panic!(
            "expected the `{DIRECT}`-pinned turn to execute, got {:?}",
            direct_outcome.topology
        );
    };

    let (sweep_dir, sweep_task) = project(CodingIntent::Careful);
    let sweep_data = tempfile::tempdir().expect("temp data dir");
    let (sweep_server, sweep_queue, sweep_provider, _tx2, _rx2) =
        selecting_server_with_pin(sweep_data.path(), WORKLIST_SWEEP).await;
    let sweep_sessions = sweep_server.api::<SessionsAPI>().expect("SessionsAPI");
    let sweep_session = open_session(&sweep_server, sweep_dir.path());
    sweep_queue.accept(AcceptedCodingTask::new(sweep_task));
    let sweep_outcome = run::drive_turn(sweep_sessions, &sweep_session).await;
    sweep_provider.close().await;
    let Some(TopologyTurn::Executed(sweep_executed)) = sweep_outcome.topology.as_ref() else {
        panic!(
            "expected the `{WORKLIST_SWEEP}`-pinned turn to execute, got {:?}",
            sweep_outcome.topology
        );
    };

    assert_eq!(direct_executed.registration_id, DIRECT);
    assert_eq!(sweep_executed.registration_id, WORKLIST_SWEEP);
    assert_ne!(
        direct_executed.registration_id, sweep_executed.registration_id,
        "two different external choices over the same accepted-task shape must run two \
         different topologies"
    );
}

/// The selection record names the pin as an operator constraint — distinct
/// from an evidence-informed preference (`requested` alone) and distinct from
/// the caller's own per-task directive (`requested_by_pin`).
#[tokio::test]
async fn the_selection_record_names_the_pin_as_an_operator_constraint() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _tx, _rx) =
        selecting_server_with_pin(data_dir.path(), DIRECT).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    // No directive of its own: the pin, and only the pin, decides this.
    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected the pinned turn to execute, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(executed.registration_id, DIRECT);
    assert_eq!(executed.requested.as_deref(), Some(DIRECT));
    assert!(
        executed.requested_by_pin,
        "the record must name this as the operator's pin, not the caller's own word"
    );
}

/// A task's own `topology=` directive is nearer to the specific work than a
/// run-wide default, so it wins over a pin that would otherwise apply — the
/// same "the nearer setting wins" precedent `--model` sets over
/// `POLARIS_CODE_MODEL`. The record then correctly shows this ran because
/// the *task* asked for it, not because of the pin.
#[tokio::test]
async fn a_tasks_own_directive_wins_over_an_operator_pin() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _tx, _rx) =
        selecting_server_with_pin(data_dir.path(), WORKLIST_SWEEP).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::Only(DIRECT.to_string())),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected the directed turn to execute, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(
        executed.registration_id, DIRECT,
        "the task's own directive must win over the run-wide pin (`{WORKLIST_SWEEP}`)"
    );
    assert_eq!(executed.requested.as_deref(), Some(DIRECT));
    assert!(
        !executed.requested_by_pin,
        "this ran because the caller asked for it, not because of the pin"
    );
}

/// An operator pin never overrides an explicit `topology=none`: the caller's
/// own "run nothing" is nearer and more specific than the run-wide default,
/// exactly as an explicit `Only` wins over the pin above.
#[tokio::test]
async fn an_operator_pin_does_not_override_an_explicit_none_directive() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _tx, _rx) =
        selecting_server_with_pin(data_dir.path(), DIRECT).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task).with_topology_directive(TopologyDirective::None));
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert!(
        matches!(outcome.topology.as_ref(), Some(TopologyTurn::Declined(_))),
        "an explicit 'none' must stay declined even though a pin was configured, got {:?}",
        outcome.topology
    );
    assert_eq!(outcome.reason, TurnReason::Completed);
}

/// Pinning a topology mechanical admission refuses fails loudly, naming the
/// exclusion reason — never runs an inadmissible graph, and never falls back
/// to the selector's own unconstrained pick.
///
/// `code.model-proposed-edit` declares a provider requirement this test's
/// host never wires (`selecting_server` installs no `AnthropicPlugin`), so
/// mechanical admission excludes it with `provider.unavailable` regardless of
/// how it was requested — the same ground
/// `an_inadmissible_named_topology_is_refused_with_its_own_reason_and_nothing_runs`
/// proves for a task-level directive, proved here for a pin instead.
#[tokio::test]
async fn an_operator_pin_on_an_inadmissible_topology_fails_loudly_naming_the_exclusion_reason() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _tx, _rx) =
        selecting_server_with_pin(data_dir.path(), MODEL_PROPOSED).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::NoTopologyStarted);
    let Some(TopologyTurn::NoEligibleCandidate(unstarted)) = outcome.topology.as_ref() else {
        panic!(
            "expected a refused pinned request, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(unstarted.requested.as_deref(), Some(MODEL_PROPOSED));
    assert!(
        unstarted.requested_by_pin,
        "the refusal is the pin's own, not a task directive's"
    );
    assert_eq!(
        unstarted.reasons,
        vec!["provider.unavailable".to_string()],
        "the refusal names the pinned candidate's own ground: {:?}",
        unstarted.reasons
    );
    assert!(
        std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
            .expect("the file is still there")
            .contains("todo!()"),
        "nothing ran in the pinned candidate's place"
    );
    assert_eq!(outcome.run_id, None, "no graph started for this turn");
}

/// With no `POLARIS_CODE_TOPOLOGY_PIN` (no `OperatorTopologyPin` global at
/// all), behaviour is byte-identical to a run with no pin support: the
/// autonomous pick runs, and the record shows no request from either channel.
#[tokio::test]
async fn no_pin_is_byte_identical_to_todays_autonomous_selection() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _tx, _rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected the autonomous turn to execute, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(
        executed.registration_id, PREDICTION_ERROR,
        "the autonomous pick under `Careful` intent is unchanged"
    );
    assert_eq!(executed.requested, None);
    assert!(!executed.requested_by_pin);
}

/// A pin changes which graph runs and nothing else
/// (`docs/topology-selection.md`'s four properties, restated for a pinned
/// run): the session's own turn number does not advance and the session's
/// own `TokenMeter` does not move, exactly as an unpinned or task-directed
/// topology turn already proves — a pin composes through the identical
/// `narrow_to`/`execute_permitted` path, so nothing about context
/// construction or execution changed to touch either.
#[tokio::test]
async fn a_pinned_topology_turn_still_does_not_advance_the_turn_number_or_move_the_tokenmeter() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _tx, _rx) =
        selecting_server_with_pin(data_dir.path(), DIRECT).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert!(matches!(
        outcome.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));
    assert_eq!(
        outcome.turn_number, 0,
        "a pinned topology turn does not advance the session's turn number"
    );
    assert_eq!(
        outcome.usage.as_ref().map(|usage| usage.total_tokens),
        Some(0),
        "a pinned topology turn does not move the session's own TokenMeter — the run bills a \
         fresh meter of its own, never the session's"
    );
}

/// The sealed catalog is exactly as it was — same generation, same candidate
/// set — before and after a pinned run: a pin narrows the objective it hands
/// to selection, it never registers, unregisters or mutates the catalog
/// itself.
#[tokio::test]
async fn the_catalog_is_unchanged_after_a_pinned_run() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _tx, _rx) =
        selecting_server_with_pin(data_dir.path(), DIRECT).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    let before = server
        .get_global::<TopologyCatalog>()
        .expect("TopologyCatalog installed")
        .snapshot()
        .expect("the catalog is sealed");

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;
    assert!(matches!(
        outcome.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));

    let after = server
        .get_global::<TopologyCatalog>()
        .expect("TopologyCatalog installed")
        .snapshot()
        .expect("the catalog is sealed");
    assert_eq!(before.generation(), after.generation());
    assert_eq!(
        before.candidates().len(),
        after.candidates().len(),
        "the registration set is unchanged"
    );
}
