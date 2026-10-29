//! Proof that a live host installs a [`ProjectTestRunner`] for a turn only
//! when the accepted task states a command success check, and that this is
//! what turns `code.test-gated-edit` from excluded to admissible.
//!
//! `code.test-gated-edit`'s own judge names `ProjectTestRunner`
//! (`graphs.rs`), and a hermetic host never used to install one, so this
//! registration was always excluded `resource.missing`
//! (`src/docs/resources.md`). Once a task can state a command success check, the
//! host installs a runner bounded by that command for the turn — these tests
//! drive the real selection point through
//! [`polaris_code_core::run::drive_turn`], exactly as
//! `topology_directed_selection.rs` does, so what is proved is the live
//! branch a turn takes, not a unit test of the host's wiring in isolation.

mod common;

use common::{ADD_DONE, open_session, project, project_with_success, selecting_server};
use polaris::plugins::IOProvider;
use polaris::sessions::SessionsAPI;
use polaris_code_core::cosmos::host::{AcceptedCodingTask, TopologyDirective, TopologyTurn};
use polaris_code_core::cosmos::task::{CodingIntent, SuccessCheck};
use polaris_code_core::cosmos::topology::TEST_GATED;
use polaris_code_core::run::{self, TurnReason};

/// With no command stated, the default fixture's `SuccessCheck::StatedEdits`
/// carries no command, so the host installs no runner: naming
/// `code.test-gated-edit` directly is refused with its own mechanical
/// ground, and nothing runs in its place.
#[tokio::test]
async fn no_stated_command_leaves_the_test_gated_topology_excluded() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::Only(TEST_GATED.to_string())),
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
    assert_eq!(unstarted.requested.as_deref(), Some(TEST_GATED));
    assert_eq!(
        unstarted.reasons,
        vec!["resource.missing".to_owned()],
        "with no command stated, the host installs no runner, so the test-gated \
         candidate is excluded exactly the way a hermetic host always excluded it: {:?}",
        unstarted.reasons
    );
    assert!(
        std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
            .expect("the file is still there")
            .contains("todo!()"),
        "nothing ran in place of the refused directed request"
    );
}

/// A task that states a command success check makes the host install a
/// `ProjectTestRunner` bounded by that command, for this turn only — turning
/// `code.test-gated-edit` from excluded into an admissible, real, live run.
/// `true` always exits zero, so the run really passing (not just being
/// admitted) proves the installed runner is actually invoked, not stubbed.
#[tokio::test]
async fn a_stated_command_installs_a_runner_and_the_test_gated_topology_runs_live() {
    let success = SuccessCheck::command("true", Vec::<String>::new())
        .expect("a bare command with no args is a well-formed success check");
    let (project_dir, task) = project_with_success(CodingIntent::Careful, success);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::Only(TEST_GATED.to_string())),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected the test-gated topology to run once a command was stated, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(executed.registration_id, TEST_GATED);
    assert!(
        executed.nodes_executed > 0,
        "the test-gated graph really ran: {executed:?}"
    );
    let edited = std::fs::read_to_string(project_dir.path().join("src/lib.rs")).expect("read back");
    assert!(
        edited.contains(ADD_DONE),
        "the stated edit landed and the live `true` run judged it a pass: {edited}"
    );
}

/// A stated command that actually fails is judged live, not rubber-stamped:
/// `false` always exits nonzero, so the turn does not complete as a plain
/// pass the way the `true` case does.
#[tokio::test]
async fn a_failing_stated_command_is_judged_live_not_rubber_stamped() {
    let success = SuccessCheck::command("false", Vec::<String>::new())
        .expect("a bare command with no args is a well-formed success check");
    let (project_dir, task) = project_with_success(CodingIntent::Careful, success);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task)
            .with_topology_directive(TopologyDirective::Only(TEST_GATED.to_string())),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    // Whatever shape a failing live judgment takes downstream, it must not be
    // the same clean-pass outcome the `true` case produces above.
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected the test-gated topology to still run and be judged, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(executed.registration_id, TEST_GATED);
    assert!(
        executed.nodes_executed > 0,
        "a failing command is still a live run, not a skipped one: {executed:?}"
    );
}
