//! Proof that a directed topology comparison never writes into a live
//! session's own evidence store.
//!
//! Outcome-learning ([`polaris_code::cosmos::policy::candidate_facts`])
//! reads whatever the session's own [`TopologyEvidence`] holds and feeds it
//! back into future ranking. `compare_intents` builds a brand-new, single-use
//! server and evidence store per arm ([`cosmos::compare`]'s module docs), so a
//! comparison must be structurally incapable of changing what a live,
//! autonomous session's own store reports — not merely unlikely to.
//!
//! This drives one real accepted task through the live host loop (the same
//! pattern `topology_selection_host_loop.rs` uses) to give that session's
//! evidence store a real episode, snapshots its status, runs a comparison
//! against a wholly unrelated tree, and asserts the live store's status is
//! byte-for-byte the same afterward.

mod common;

use common::{open_session, project, selecting_server};
use polaris::sessions::SessionsAPI;
use polaris_code::cosmos::host::AcceptedCodingTask;
use polaris_code::cosmos::task::CodingIntent;
use polaris_code::cosmos::{ArmOutcome, compare_intents};
use polaris_code::run;
use polaris_cosmos::evidence::TopologyEvidence;

#[tokio::test]
async fn a_directed_comparison_never_touches_the_live_sessions_evidence_store() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, _provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");

    // Give the live session's own evidence store a real episode before the
    // comparison runs, so "unchanged" below is a meaningful claim.
    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    assert!(
        outcome.topology.is_some(),
        "the live turn must really select a topology before this test means anything: {outcome:?}"
    );

    let baseline = evidence
        .store_status()
        .expect("the live evidence store reports its status");
    assert!(
        baseline.retained_episodes > 0,
        "the live run must have recorded a real episode: {baseline:?}"
    );

    // A completely unrelated tree, compared under two intents. Each arm builds
    // and tears down its own ephemeral server and evidence store.
    let (compare_dir, _unused_task) = project(CodingIntent::Fast);
    let report = compare_intents(
        "compare/distinguishability-probe",
        compare_dir.path(),
        "src/lib.rs",
        common::ADD_STUB,
        common::ADD_DONE,
        "todo!()",
        None,
        false,
        [CodingIntent::Fast, CodingIntent::Thorough],
    )
    .await;
    assert!(
        report
            .arms
            .iter()
            .all(|arm| matches!(arm.outcome, ArmOutcome::Selected(_))),
        "both arms of an edit-shaped comparison on a fresh stub should reach a real selection answer: {report:?}"
    );

    let after = evidence
        .store_status()
        .expect("the live evidence store reports its status");
    assert_eq!(
        baseline, after,
        "a directed comparison must not change the live session's own evidence store"
    );
}
