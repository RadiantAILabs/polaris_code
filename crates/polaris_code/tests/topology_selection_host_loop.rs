//! Proof that topology selection is wired into the live host loop.
//!
//! One server assembled the way `main.rs` assembles it — the catalog
//! provider, the coding pack, an evidence capability, the accepted-task queue,
//! and the real [`CodeAgent`] behind a scripted model provider — and one real
//! coding task on a real temporary crate, driven through
//! [`polaris_code::run::drive_turn`]. Nothing here calls
//! [`polaris_code::cosmos::proof::CodingProof`]: the point is the *live*
//! path, the branch the run surface takes before `process_turn_with`.
//!
//! What each test pins down:
//!
//! - the ordered lifecycle really runs in order, once per accepted task, and
//!   the selected native graph is what edited the file on disk;
//! - `NoEligibleCandidate` is persisted and returned typed, and **no** graph
//!   runs in its place — not another candidate, not the conversational agent;
//! - a conversational turn with nothing accepted still works end to end;
//! - selection happens before anything starts, so a turn that started no
//!   topology leaves the session's turn number and transcript untouched; and
//! - a unit of work stated as a problem across several files, with no
//!   precomputed edit, is accepted and run through that same selection point,
//!   and its verdict comes from the success check the caller supplied; and
//! - the second selection reads what the first run recorded, so an identical
//!   task put twice in front of the same host is not answered twice the same
//!   way.

mod common;

use std::path::Path;

use common::{ANSWER, open_session, project, selecting_server};
use polaris::plugins::{IOContent, IOMessage, IOProvider};
use polaris::sessions::SessionsAPI;
use polaris_code::cosmos::host::{AcceptedCodingTask, TopologyTurn};
use polaris_code::cosmos::task::{CodingIntent, CodingObjective, CodingTask, SuccessCheck};
use polaris_code::cosmos::topology::{PREDICTION_ERROR, WORKLIST_SWEEP};
use polaris_code::exec::{OutputFormat, RunReport};
use polaris_code::run::{self, TurnReason};
use polaris_cosmos::contracts::{
    CanonicalRecord, DecompositionShape, ScoreSource, SelectionOutcome, Status,
};
use polaris_cosmos::evidence::TopologyEvidence;

/// A temporary crate plus a unit of work stated as a *problem*, not an edit.
///
/// This is the shape a real benchmark instance has: a sentence saying what is
/// wrong, a tree to work in, the files believed to be in scope, and a command
/// that decides whether the work succeeded. No find, no replace, no marker —
/// the accepting side has computed nothing.
fn stated_problem(dir: &Path) -> CodingTask {
    std::fs::create_dir_all(dir.join("src")).expect("src dir");
    std::fs::write(dir.join("src/parser.rs"), "pub fn parse() {}\n").expect("seed parser.rs");
    std::fs::write(dir.join("src/lexer.rs"), "pub fn lex() {}\n").expect("seed lexer.rs");
    CodingTask::from_statement(
        "reject-trailing-commas",
        dir,
        "Reject trailing commas in the parser and report the offending span.",
        CodingIntent::Careful,
        SuccessCheck::command("/bin/sh", ["-c", "test -f ready.marker"])
            .expect("the success command is well formed"),
    )
    .expect("the unit of work is well formed")
    .with_scope(["src/parser.rs", "src/lexer.rs"])
}

#[tokio::test]
async fn one_accepted_coding_task_runs_the_ordered_lifecycle_in_the_live_loop() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, _provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");

    // Nothing has happened yet: the stub is on disk and the store is empty.
    let target = project_dir.path().join("src/lib.rs");
    assert!(
        std::fs::read_to_string(&target)
            .expect("seeded file")
            .contains("todo!()")
    );
    assert_eq!(
        evidence.store_status().expect("status").retained_episodes,
        0
    );

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    // The turn was served by the selected topology, not by the agent.
    assert_eq!(outcome.reason, TurnReason::Completed);
    let topology = outcome
        .topology
        .as_ref()
        .expect("the turn selected a topology");
    let TopologyTurn::Executed(executed) = topology else {
        panic!("expected an executed topology, got {topology:?}");
    };
    assert_eq!(
        executed.registration_id, PREDICTION_ERROR,
        "the careful objective ranks the prediction-error loop first"
    );
    assert_eq!(
        executed.fallback_position, 0,
        "the decision's primary choice was admitted, so no fallback was used"
    );
    assert!(
        executed.nodes_executed > 0,
        "the native graph really ran: {executed:?}"
    );
    assert_eq!(outcome.nodes_executed, executed.nodes_executed);
    assert_eq!(
        outcome.run_id.as_ref(),
        Some(&executed.run_id),
        "the reported executor run id is the selected graph's"
    );
    let composition = executed
        .composition
        .as_ref()
        .expect("every newly executed topology carries composition evidence");
    composition
        .validate()
        .expect("composition evidence is canonical");
    assert_eq!(
        composition.decision().decision_id(),
        executed.decision_id,
        "composition and ordinary topology evidence share the selection decision"
    );
    assert_eq!(composition.run().run_id(), executed.cosmos_run_id);
    assert_eq!(
        composition
            .decision()
            .choice()
            .plan()
            .recipe()
            .recipe()
            .id()
            .as_str(),
        "polaris-code.recipe.prediction-error",
        "the selected coding shape was re-materialised from its bounded recipe"
    );
    let json = RunReport::new(&session_id, &outcome, 4096)
        .stdout_payload(OutputFormat::Json)
        .expect("json report");
    let rendered: serde_json::Value = serde_json::from_str(&json).expect("parse report");
    assert_eq!(
        rendered["topology"]["composition"]["recipe"],
        "polaris-code.recipe.prediction-error@1.0.0"
    );
    assert_eq!(
        rendered["topology"]["composition"]["forced"], false,
        "the ordinary selection was autonomous rather than caller-forced"
    );
    assert!(
        rendered["topology"]["composition"]["evidence"]["decision"]["choice"]["plan"]["blocks"]
            .as_array()
            .is_some_and(|blocks| !blocks.is_empty()),
        "headless evidence includes the versioned block records and configurations: {json}"
    );

    // Execution had the side effect only a real run can have.
    let after = std::fs::read_to_string(&target).expect("edited file");
    assert!(
        after.contains("a + b"),
        "the selected graph applied the real edit: {after}"
    );

    // The queue is drained: selection is once per accepted coding task.
    assert!(
        queue.is_empty(),
        "one turn consumes exactly one accepted coding task"
    );

    // The evidence episode is the documented lifecycle order, and nothing else.
    let episode = evidence
        .read_episode(executed.decision_id)
        .expect("the episode is readable")
        .expect("the decision was persisted before anything started");
    assert!(
        matches!(
            episode.selection.decision().outcome(),
            SelectionOutcome::Selected { .. }
        ),
        "the persisted record is the decision that ran"
    );
    assert_eq!(
        episode.admissions.len(),
        1,
        "at most one registration starts per decision, so exactly one live attempt"
    );
    assert!(episode.admissions[0].is_admitted());
    assert!(
        episode.not_started.is_none(),
        "a started run has no no-start terminal"
    );
    assert_eq!(episode.runs.len(), 1, "exactly one run started");

    let run_view = &episode.runs[0];
    assert_eq!(run_view.opened.run_id(), executed.cosmos_run_id);
    assert!(
        run_view.terminal.is_some(),
        "the host finalized the run it opened"
    );
    assert_eq!(
        run_view.observations.len(),
        2,
        "both of the embodiment's verifiers reported"
    );

    // Selection -> admission -> open -> finalize -> observations, in store order.
    let selection_at = episode.selection.recorded_at().sequence().get();
    let admission_at = episode.admissions[0].recorded_at().sequence().get();
    let opened_at = run_view.opened.recorded_at().sequence().get();
    assert!(
        selection_at < admission_at && admission_at < opened_at,
        "selection is evidence before live admission, which precedes the run: \
         {selection_at} < {admission_at} < {opened_at}"
    );
    let mut observed = run_view
        .observations
        .iter()
        .map(|observation| observation.recorded_at().sequence().get());
    let first_observation = observed.next().expect("an applied-edit verdict");
    assert!(
        opened_at < first_observation,
        "outcome observations follow the run they judge"
    );

    // The verifiers disagree, which is the honest state of this tree: the edit
    // landed, and other stub sites remain.
    assert!(
        run_view
            .observations
            .iter()
            .any(|observation| observation.status() == Status::Pass),
        "the applied-edit verifier passed"
    );
    assert!(
        run_view
            .observations
            .iter()
            .any(|observation| observation.status() == Status::Issue),
        "the residual-stub verifier still sees `sub`"
    );

    // The turn ran outside `process_turn_with`, so the session's own turn
    // accounting is untouched — no renumbering, no invented turn.
    assert_eq!(
        outcome.turn_number, 0,
        "a topology turn does not advance the session's turn number"
    );
}

#[tokio::test]
async fn a_multi_file_unit_of_work_runs_without_a_precomputed_edit() {
    let project_dir = tempfile::tempdir().expect("temp project");
    let task = stated_problem(project_dir.path());

    // The accepting side supplied no edit at all, and the profile is derived
    // from the scope it declared rather than from a find-and-replace.
    assert!(task.edits().is_empty(), "no edit was precomputed");
    assert_eq!(task.scope(), ["src/parser.rs", "src/lexer.rs"]);
    assert_eq!(task.stub_marker(), None);
    let features = task.profile().expect("the profile aggregates");
    let features = features.features();
    assert_eq!(
        features.decomposability.known_value().copied(),
        Some(DecompositionShape::IndependentParts),
        "two files in scope are two independent parts"
    );
    assert_eq!(
        features
            .expected_independent_work
            .known_value()
            .map(|estimate| estimate.likely()),
        Some(2)
    );

    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, _provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    // It went through the *existing* selection point and a topology ran.
    assert_eq!(outcome.reason, TurnReason::Completed);
    let topology = outcome
        .topology
        .as_ref()
        .expect("the stated problem reached the selection point");
    let TopologyTurn::Executed(executed) = topology else {
        panic!("expected an executed topology, got {topology:?}");
    };
    assert_eq!(
        executed.registration_id, PREDICTION_ERROR,
        "the careful objective ranks the prediction-error loop first, edit or no edit"
    );
    assert!(executed.nodes_executed > 0);

    // Both verdicts are honest about a unit of work that stated no edit: there
    // was nothing for the applied-edit verifier to check, and the caller's own
    // command says the work is not done.
    assert_eq!(
        executed.observations.len(),
        2,
        "both of the embodiment's verifiers reported"
    );
    assert_eq!(executed.observations[0].status(), Status::NotApplicable);
    assert_eq!(
        executed.observations[1].status(),
        Status::Issue,
        "the caller's success command failed, so the work did not succeed"
    );
    assert!(
        executed.observations[1].supersedes_claim().is_some(),
        "the success check supersedes the applied-edit verdict"
    );
    assert_eq!(
        outcome.turn_number, 0,
        "a topology turn does not advance the session's turn number"
    );
}

#[tokio::test]
async fn the_callers_success_check_decides_the_verdict() {
    let project_dir = tempfile::tempdir().expect("temp project");
    let task = stated_problem(project_dir.path());
    // The only difference from the failing run: what the caller's check finds.
    std::fs::write(project_dir.path().join("ready.marker"), "done\n").expect("seed the marker");

    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, _provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", outcome.topology);
    };
    assert_eq!(
        executed.observations[1].status(),
        Status::Pass,
        "the same unit of work passes once the caller's command passes"
    );
    // Nothing about the graph changed — only the caller's check did.
    assert_eq!(executed.observations[0].status(), Status::NotApplicable);
    assert_eq!(executed.registration_id, PREDICTION_ERROR);
}

#[tokio::test]
async fn an_unsatisfiable_objective_is_persisted_and_starts_nothing() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, _provider, _input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");

    // No coding topology in this embodiment offers a comparative verifier.
    queue.accept(AcceptedCodingTask::new(task).with_objective(
        CodingObjective::new(CodingIntent::Careful).requiring_comparative_verifier(),
    ));
    let outcome = run::drive_turn(sessions, &session_id).await;

    assert_eq!(
        outcome.reason,
        TurnReason::NoTopologyStarted,
        "a no-eligible answer is typed, not a generic failure"
    );
    assert_eq!(
        outcome.reason.exit_code(),
        1,
        "accepted work that never ran is not a successful run"
    );
    let topology = outcome.topology.as_ref().expect("the turn made a decision");
    let TopologyTurn::NoEligibleCandidate(unstarted) = topology else {
        panic!("expected a no-eligible answer, got {topology:?}");
    };
    assert!(
        !unstarted.reasons.is_empty(),
        "the answer explains itself with the selector's own codes"
    );
    assert!(
        unstarted.attempted.is_empty(),
        "nothing was selected, so no live readmission was attempted"
    );
    assert_eq!(topology.started(), None);

    // It never fell through to a default graph: no candidate ran, and the
    // conversational agent did not serve the turn either.
    assert_eq!(outcome.run_id, None);
    assert_eq!(outcome.nodes_executed, 0);
    assert_eq!(
        outcome.final_text.as_deref(),
        Some(topology.answer().as_str()),
        "the turn's answer is the selector's, not an agent's"
    );
    assert_ne!(outcome.final_text.as_deref(), Some(ANSWER));
    assert_eq!(
        outcome.turn_number, 0,
        "no turn was processed in the selected graph's place"
    );
    let after = std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
        .expect("the seeded file is still there");
    assert!(
        after.contains("todo!()") && !after.contains("a + b"),
        "nothing edited the tree: {after}"
    );

    // The decision is persisted even though nothing started.
    let episode = evidence
        .read_episode(unstarted.decision_id)
        .expect("the episode is readable")
        .expect("a no-eligible decision is still evidence");
    assert!(matches!(
        episode.selection.decision().outcome(),
        SelectionOutcome::NoEligibleCandidate { .. }
    ));
    assert!(episode.admissions.is_empty());
    assert!(episode.runs.is_empty(), "no run was ever opened");

    // The non-interactive emitter reports it as its own status, not as a
    // failure with an invented message.
    // The scripted provider has no output limit; report the value the shipped
    // binary registers when nothing configures one.
    let report = RunReport::new(&session_id, &outcome, 4096);
    let error_line = report
        .error_line()
        .expect("a no-start turn explains itself");
    assert!(
        error_line.starts_with("polaris-code: no topology started: "),
        "unexpected diagnostic: {error_line}"
    );
    assert!(
        error_line.contains(&unstarted.reasons[0]),
        "the diagnostic carries the selector's codes: {error_line}"
    );
    assert_eq!(report.exit_code(), 1);

    // The driver keeps going: the user can accept a different task next turn.
    assert!(run::driver_continues(&outcome.reason));
}

#[tokio::test]
async fn a_turn_that_selects_no_topology_still_works_end_to_end() {
    let (project_dir, _task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");

    // The catalog and evidence capability are installed and the queue is
    // empty — which is the shipped binary's own state. The turn must be the
    // ordinary conversational one.
    assert!(queue.is_empty());
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("is anything wrong?".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);

    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert!(
        outcome.topology.is_none(),
        "no coding task was accepted, so the selection point stayed inert"
    );
    assert_eq!(
        outcome.final_text.as_deref(),
        Some(ANSWER),
        "the real agent step served the turn"
    );
    assert_eq!(
        outcome.turn_number, 1,
        "the conversational turn advanced the session exactly as before"
    );
    assert!(outcome.run_id.is_some());
    assert_eq!(outcome.reason.exit_code(), 0);

    // Selection wrote nothing: an inert branch leaves no evidence behind.
    let status = evidence.store_status().expect("status");
    assert_eq!(status.retained_episodes, 0);
    assert_eq!(status.retained_runs, 0);
}

#[tokio::test]
async fn a_second_turn_selects_again_only_when_another_task_is_accepted() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");

    queue.accept(AcceptedCodingTask::new(task));
    let first = run::drive_turn(sessions, &session_id).await;
    assert!(matches!(
        first.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));

    // Nothing more was accepted, so the next turn is conversational again and
    // the loop the two share is unchanged.
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("thanks".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);
    let second = run::drive_turn(sessions, &session_id).await;
    provider.close().await;

    assert!(second.topology.is_none());
    assert_eq!(second.reason, TurnReason::Completed);
    assert_eq!(second.final_text.as_deref(), Some(ANSWER));

    // Exactly one episode, from the one accepted task.
    assert_eq!(
        evidence.store_status().expect("status").retained_episodes,
        1,
        "the conversational turn appended no second decision"
    );
}

#[tokio::test]
async fn the_second_selection_ranks_on_what_the_first_run_recorded() {
    // Two separate temporary crates with identical contents. They have to be
    // separate: the first run edits its tree, and a task over an edited tree
    // profiles differently and so falls in a different cohort. Identical
    // contents are what make the second decision a fair comparison with the
    // first — same profile, same objective, same admission set, same priors.
    let (first_dir, first_task) = project(CodingIntent::Careful);
    let (second_dir, second_task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx, _output_rx) = selecting_server(data_dir.path()).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, first_dir.path());

    queue.accept(AcceptedCodingTask::new(first_task));
    let first = run::drive_turn(sessions, &session_id).await;
    let Some(TopologyTurn::Executed(first_executed)) = first.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", first.topology);
    };
    assert_eq!(
        first_executed.registration_id, PREDICTION_ERROR,
        "with nothing recorded, the author priors decide the first selection"
    );

    // An ordinary conversational turn between the two coding tasks, which is
    // how a session actually reads. It also advances the session's turn
    // number, and that number seeds the evidence event identities: two
    // *consecutive* topology turns in one session would derive the same
    // identities and the second append would be refused.
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("now the other crate".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);
    let between = run::drive_turn(sessions, &session_id).await;
    provider.close().await;
    assert!(between.topology.is_none());

    // The first run is now on the record, and its two verifiers disagreed:
    // one pass out of two applied observations.
    queue.accept(AcceptedCodingTask::new(second_task));
    let second = run::drive_turn(sessions, &session_id).await;
    let Some(TopologyTurn::Executed(second_executed)) = second.topology.as_ref() else {
        panic!(
            "expected an executed topology, got {:?} ({:?})",
            second.topology, second.reason
        );
    };
    assert_eq!(
        second_executed.registration_id, WORKLIST_SWEEP,
        "the prediction-error loop's recorded half-pass rate drops it below \
         the two candidates nothing has been recorded against"
    );
    let worklist_composition = second_executed
        .composition
        .as_ref()
        .expect("the worklist run carries its composition evidence");
    assert_eq!(
        worklist_composition
            .decision()
            .choice()
            .plan()
            .recipe()
            .recipe()
            .id()
            .as_str(),
        "polaris-code.recipe.bounded-worklist"
    );
    assert_ne!(
        first_executed
            .composition
            .as_ref()
            .expect("the first run carries its composition evidence")
            .composition_digest(),
        worklist_composition.composition_digest(),
        "different task/evidence facts selected a different bounded graph recipe"
    );

    // The second graph really ran on the second tree, and the first tree was
    // not touched again.
    let second_after =
        std::fs::read_to_string(second_dir.path().join("src/lib.rs")).expect("edited file");
    assert!(
        second_after.contains("a + b"),
        "the newly selected graph applied the edit: {second_after}"
    );

    // The change is visible in the decision itself, not only in the outcome.
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");
    let episode = evidence
        .read_episode(second_executed.decision_id)
        .expect("the episode is readable")
        .expect("the second decision was persisted");
    let measured: Vec<_> = episode
        .selection
        .decision()
        .candidates()
        .iter()
        .filter(|candidate| candidate.candidate().registration_id().as_str() == PREDICTION_ERROR)
        .filter_map(|candidate| candidate.score())
        .flat_map(|score| score.components())
        .filter(|component| component.source() == ScoreSource::HistoricalStatistics)
        .collect();
    assert_eq!(measured.len(), 1, "one measured component, for quality");
    assert_eq!(
        measured[0].normalized_utility().get(),
        500_000,
        "one pass and one issue is a half"
    );
    assert_eq!(
        measured[0].statistics_snapshot_digest(),
        Some(
            episode
                .selection
                .decision()
                .input_refs()
                .statistics_snapshot_digest()
        ),
        "the measured component is pinned to the snapshot the decision names"
    );
}
