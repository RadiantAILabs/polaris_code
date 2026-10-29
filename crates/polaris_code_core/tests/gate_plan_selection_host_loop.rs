//! Proof that the gate-plan composition policy is wired into the live
//! selection loop, not just exercised through `planner`'s own unit tests.
//!
//! One server assembled the way `main.rs` assembles it, with the gate-plan
//! composition policy installed instead of the deterministic finite-recipe
//! one, and one real coding task on a real temporary crate, driven through
//! [`polaris_code_core::run::drive_turn`].
//!
//! - A plan that materialises runs through exactly the same live-admission
//!   and execution path a finite-recipe composition does, and the executed
//!   turn's revision trail names the one admitted attempt.
//! - A plan that can never satisfy the host's own materialisation limits
//!   exhausts its revision loop and falls back to the finite recipe
//!   grammar for the same candidate — the turn still completes, but not on
//!   its own gate-plan composition.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{ADD_DONE, ScriptedModelPlugin, open_session, project};
use polaris::models::ModelsPlugin;
use polaris::plugins::{IOContent, IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{FileStore, SessionsAPI, SessionsPlugin};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::CodeAgent;
use polaris_code_core::cosmos::composition::CompositionPolicy;
use polaris_code_core::cosmos::host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, TopologyTurn,
};
use polaris_code_core::cosmos::planner::GatePlanLimits;
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::task::{CodingIntent, CodingTask, SuccessCheck};
use polaris_code_core::cosmos::topology::{CodingTopologyPackPlugin, HostCapabilities};
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::run;
use polaris_code_core::usage::{TaskCostLimit, TokenMeter};
use polaris_cosmos::composition::CompositionLimits;
use polaris_cosmos::contracts::{RunTerminal, StopReason};
use polaris_cosmos::evidence::{TopologyEvidence, TopologyEvidencePlugin};
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// Builds the same selecting server `common::selecting_server` does, plus
/// [`CompositionPolicy::Gates`] and an optional [`GatePlanLimits`] override
/// installed as global resources before `finish()`, exactly like
/// `main.rs` installs an [`OperatorTopologyPin`](polaris_code_core::cosmos::host::OperatorTopologyPin)
/// from its own env var.
async fn gate_plan_server(
    data_dir: &Path,
    limits: Option<CompositionLimits>,
    cap: Option<f64>,
) -> (Server, CodingTaskQueue, mpsc::Sender<IOMessage>) {
    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server.insert_global(CompositionPolicy::Gates);
    if let Some(cap) = cap {
        server.insert_global(TaskCostLimit::new(cap).unwrap());
    }
    if let Some(limits) = limits {
        server.insert_global(GatePlanLimits(limits));
    }
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(polaris_code_core::agents::CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));

    server
        .finish()
        .await
        .expect("the gate-plan selecting server assembles");

    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");

    (server, queue, input_tx)
}

#[tokio::test]
async fn a_pre_call_cost_refusal_finalizes_the_episode_and_keeps_the_workspace() {
    let (project_dir, _) = project(CodingIntent::Fast);
    let current = format!(
        "{}\n// An edit already present when the cap stops the task.\n",
        std::fs::read_to_string(project_dir.path().join("src/lib.rs")).unwrap()
    );
    std::fs::write(project_dir.path().join("src/lib.rs"), &current).unwrap();
    let task = CodingTask::from_statement(
        "cap-test",
        project_dir.path(),
        "Implement the unfinished helper",
        CodingIntent::Fast,
        SuccessCheck::delegated("test-verifier").unwrap(),
    )
    .unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (server, queue, _input_tx) = gate_plan_server(data_dir.path(), None, Some(0.01)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let session_id = open_session(&server, project_dir.path());
    queue.accept(
        AcceptedCodingTask::new(task).with_host_capabilities(HostCapabilities {
            test_runner: false,
            model_provider: true,
        }),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    let TopologyTurn::CostCapped(stopped) = outcome.topology.as_ref().unwrap() else {
        panic!("expected cost cap, got {outcome:?}");
    };
    assert_eq!(outcome.reason, run::TurnReason::CostCapped);
    assert_eq!(outcome.reason.exit_code(), run::CEILING_EXIT_CODE);
    let evidence = server.get_global::<TopologyEvidence>().unwrap();
    let episode = evidence.read_episode(stopped.decision_id).unwrap().unwrap();
    let run = episode.runs.first().expect("the selected run was opened");
    let Some(RunTerminal::Finalized(finalized)) = &run.terminal else {
        panic!("cost cap must finalize the opened run");
    };
    assert_eq!(
        finalized.completion().stop_reason(),
        Some(StopReason::BudgetExhausted)
    );
    assert!(
        finalized
            .faults()
            .errors()
            .iter()
            .any(|fault| fault.error_class().as_str() == "cost-cap")
    );
    assert_eq!(
        std::fs::read_to_string(project_dir.path().join("src/lib.rs")).unwrap(),
        current
    );
}

#[tokio::test]
async fn a_materialised_gate_plan_runs_end_to_end_and_names_its_revision_trail() {
    let (project_dir, task) = project(polaris_code_core::cosmos::task::CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, _input_tx) = gate_plan_server(data_dir.path(), None, None).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    let topology = outcome
        .topology
        .as_ref()
        .unwrap_or_else(|| panic!("the turn selected a topology; reason: {:?}", outcome.reason));
    let TopologyTurn::Executed(executed) = topology else {
        panic!("expected an executed topology, got {topology:?}");
    };
    assert!(
        executed.nodes_executed > 0,
        "the gate-plan-materialised graph really ran: {executed:?}"
    );
    let edited = std::fs::read_to_string(project_dir.path().join("src/lib.rs")).expect("read back");
    assert!(
        edited.contains(ADD_DONE),
        "the direct-edit gate-plan shape actually applied the stated edit: {edited}"
    );

    let request = executed
        .composition_request
        .as_ref()
        .expect("a gate-plan composition still carries a composition request");
    assert_eq!(
        request.policy().as_str(),
        CompositionPolicy::Gates.as_str(),
        "the composition request names the gate-plan policy, not deterministic or model"
    );

    let trail = executed
        .gate_plan_trail
        .as_ref()
        .expect("an executed gate-plan composition carries its revision trail");
    assert_eq!(
        trail.revisions().len(),
        1,
        "the first attempt materialised, so the trail holds exactly one, admitted revision"
    );
    assert!(
        trail.revisions()[0].validation().is_admitted(),
        "the one revision in the trail is the admitted one that ran"
    );
    assert_eq!(trail.revisions()[0].revision_attempt().get(), 1);

    // The same fact reaches the headless JSON report, not just the in-memory
    // executed-topology value: `RunReport` is the public surface a
    // `--output-format json` run actually prints.
    let report = polaris_code_core::exec::RunReport::new(&session_id, &outcome, 4_096);
    let reported_gate_plan = report
        .topology
        .as_ref()
        .expect("the report carries this turn's topology selection")
        .gate_plan
        .as_ref()
        .expect("the report names the admitted gate plan");
    assert_eq!(reported_gate_plan.revision_attempt, 1);
    assert_eq!(reported_gate_plan.prior_refused_attempts, 0);
    assert_eq!(
        reported_gate_plan.planner,
        polaris_code_core::cosmos::planner::GatePlannerMode::Scripted.as_str(),
        "the report names which planner assembled this plan"
    );
    assert_eq!(
        reported_gate_plan.shape, "Seq(apply-direct, judge-stated-edits, conclude-single-pass)",
        "the reported shape renders the whole gate tree with gate names, not a flat, \
         unordered block list — two differently shaped plans over the same blocks must not \
         print the same line"
    );

    // And the interactive panel message names it too.
    let panel = polaris_code_core::terminal::topology::turn_report(&outcome)
        .expect("an executed topology gets a panel message");
    let IOContent::Text(text) = panel.content else {
        panic!("panel message is not plain text");
    };
    assert!(
        text.contains("gate plan:"),
        "the panel message names the gate plan: {text}"
    );
}

#[tokio::test]
async fn an_unsatisfiable_gate_plan_limit_falls_back_to_the_finite_recipe_grammar() {
    let (project_dir, task) = project(polaris_code_core::cosmos::task::CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    // Even the shallowest scripted shape (a flat three-step sequence, depth
    // two) cannot fit under a maximum depth of one: every gate-plan attempt
    // this turn makes is refused.
    let unsatisfiable = CompositionLimits {
        max_depth: 1,
        ..CompositionLimits::conservative()
    };
    let (server, queue, _input_tx) =
        gate_plan_server(data_dir.path(), Some(unsatisfiable), None).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    let topology = outcome
        .topology
        .as_ref()
        .expect("the turn selected a topology");
    let TopologyTurn::Executed(executed) = topology else {
        panic!("expected the finite-recipe fallback to still execute, got {topology:?}");
    };
    assert!(
        executed.nodes_executed > 0,
        "the finite-recipe fallback graph really ran: {executed:?}"
    );
    let edited = std::fs::read_to_string(project_dir.path().join("src/lib.rs")).expect("read back");
    assert!(
        edited.contains(ADD_DONE),
        "the finite-recipe fallback still applied the stated edit: {edited}"
    );
    assert!(
        executed.gate_plan_trail.is_none(),
        "a finite-recipe fallback composition carries no gate-plan revision trail"
    );
    let request = executed
        .composition_request
        .as_ref()
        .expect("every executed composition carries a composition request");
    assert_ne!(
        request.policy().as_str(),
        CompositionPolicy::Gates.as_str(),
        "the fallback composition was chosen by the finite-recipe grammar, not the gate planner"
    );

    // The exhausted revision loop's own refused attempts survive the
    // fallback: every one is retained, kept apart from `gate_plan_trail`
    // (which the finite-recipe composition never touches), and reaches the
    // headless JSON report and the interactive panel message too.
    let exhausted = executed
        .exhausted_gate_plan_trail
        .as_ref()
        .expect("the exhausted revision loop's own refused attempts are retained");
    assert_eq!(
        exhausted.revisions().len(),
        3,
        "every one of the three allowed attempts was refused before exhaustion"
    );
    for revision in exhausted.revisions() {
        assert!(
            !revision.validation().is_admitted(),
            "an exhausted trail never contains an admitted revision"
        );
    }

    let report = polaris_code_core::exec::RunReport::new(&session_id, &outcome, 4_096);
    let selection = report
        .topology
        .as_ref()
        .expect("the report carries this turn's topology selection");
    assert!(
        selection.gate_plan.is_none(),
        "the report's own gate_plan field names only an admitted plan"
    );
    let reported_exhaustion = selection
        .exhausted_gate_plan
        .as_ref()
        .expect("the report names the exhausted revision loop separately");
    assert_eq!(reported_exhaustion.refused_attempts, 3);

    let panel = polaris_code_core::terminal::topology::turn_report(&outcome)
        .expect("an executed topology gets a panel message");
    let IOContent::Text(text) = panel.content else {
        panic!("panel message is not plain text");
    };
    assert!(
        text.contains("gate planner: exhausted after 3 refused attempt(s)"),
        "the panel message names the exhausted revision loop: {text}"
    );
}

/// A provider that answers an ordinary conversational prompt with fixed
/// text (never counted), and answers the gate planner's own prompt — found
/// by its literal opening words — with one scripted answer per call, oldest
/// first: a well-formed but overlong first plan the materializer's own
/// instance limit refuses, then the same shape trimmed back down to the
/// three direct-edit blocks the first passing test already proves run live,
/// so this test needs no extra live resources (a real test runner, a real
/// model-step executor) beyond what every other test in this file already
/// has.
struct SequencedGatePlanProvider {
    answers: std::sync::Mutex<std::collections::VecDeque<&'static str>>,
}

impl SequencedGatePlanProvider {
    const GATE_PLAN_MARKER: &'static str = "Submit a gate plan for one coding task,";

    fn answer_for(&self, request: &polaris::models::llm::LlmRequest) -> String {
        let is_gate_plan_prompt = request.messages.iter().any(|message| {
            matches!(message, polaris::models::llm::Message::User { content }
            if content.iter().any(|block| matches!(
                block,
                polaris::models::llm::UserBlock::Text(text)
                    if text.text.starts_with(Self::GATE_PLAN_MARKER)
            )))
        });
        if !is_gate_plan_prompt {
            return "nothing to change here.".to_owned();
        }
        self.answers
            .lock()
            .expect("test mutex")
            .pop_front()
            .expect("as many scripted answers as gate-plan calls made")
            .to_owned()
    }
}

impl polaris::models::llm::LlmProvider for SequencedGatePlanProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        request: polaris::models::llm::LlmRequest,
    ) -> Result<polaris::models::llm::LlmResponse, polaris::models::llm::GenerationError> {
        Ok(polaris::models::llm::LlmResponse {
            content: vec![polaris::models::llm::AssistantBlock::text(
                self.answer_for(&request),
            )],
            usage: polaris::models::llm::Usage {
                input_tokens: Some(20),
                output_tokens: Some(10),
                total_tokens: Some(30),
                ..Default::default()
            },
            stop_reason: polaris::models::llm::StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        _request: polaris::models::llm::LlmRequest,
    ) -> Result<polaris::models::llm::LlmStream, polaris::models::llm::GenerationError> {
        unreachable!("this test never streams a response")
    }
}

struct SequencedGatePlanModelPlugin;

impl polaris::system::plugin::Plugin for SequencedGatePlanModelPlugin {
    const ID: &'static str = "test::sequenced_gate_plan_model";
    const VERSION: polaris::system::plugin::Version =
        polaris::system::plugin::Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<polaris::system::plugin::PluginId> {
        vec![polaris::system::plugin::PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<polaris::models::ModelRegistry>()
            .expect("ModelsPlugin must build before SequencedGatePlanModelPlugin")
            .register_llm_provider(SequencedGatePlanProvider {
                answers: std::sync::Mutex::new(
                    vec![
                        r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                            {"instance":"judge1","block":"judge-stated-edits"},
                            {"instance":"judge2","block":"judge-stated-edits"},
                            {"instance":"conclude","block":"conclude-single-pass"}],
                            "grounds":"a well-formed but overlong first plan"}"#,
                        r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                            {"instance":"judge","block":"judge-stated-edits"},
                            {"instance":"conclude","block":"conclude-single-pass"}],
                            "grounds":"revised down to three steps after the first was refused"}"#,
                    ]
                    .into(),
                ),
            });
    }
}

/// Supplies the review regression's otherwise-valid test-only plan. It has no
/// edit producer, so the host must refuse to report its passing test as a
/// resolved statement-only edit even when the command writes an artifact.
struct ArtifactWritingTestPlanModelPlugin;

impl polaris::system::plugin::Plugin for ArtifactWritingTestPlanModelPlugin {
    const ID: &'static str = "test::artifact_writing_test_plan";
    const VERSION: polaris::system::plugin::Version =
        polaris::system::plugin::Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<polaris::system::plugin::PluginId> {
        vec![polaris::system::plugin::PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<polaris::models::ModelRegistry>()
            .expect("ModelsPlugin must build before ArtifactWritingTestPlanModelPlugin")
            .register_llm_provider(SequencedGatePlanProvider {
                answers: std::sync::Mutex::new(
                    vec![
                        r#"{"steps":[
                            {"instance":"run-tests","block":"run-project-tests-captured"},
                            {"instance":"judge-tests","block":"judge-test-outcome"},
                            {"instance":"conclude","block":"conclude-single-pass"}],
                            "grounds":"exercise the passing test command"}"#,
                    ]
                    .into(),
                ),
            });
    }
}

/// A test report written by the success command is not edit provenance. This
/// drives the materialized test-only plan through the live host selection
/// path, rather than calling the product projection directly.
#[tokio::test]
async fn an_artifact_writing_test_does_not_resolve_a_statement_only_edit() {
    let project_dir = tempfile::tempdir().expect("temp project");
    let source = "fn broken() { todo!() }\n";
    std::fs::write(project_dir.path().join("lib.rs"), source).expect("seed source");
    let task = CodingTask::from_statement(
        "artifact-writing-test-only-plan",
        project_dir.path(),
        "repair the reported bug",
        CodingIntent::Careful,
        SuccessCheck::command("sh", ["-c", "printf run > test-results.xml; true"])
            .expect("well-formed command"),
    )
    .expect("well-formed statement-only task");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server.insert_global(CompositionPolicy::Gates);
    server.insert_global(polaris_code_core::cosmos::planner::GatePlannerMode::Model);
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ArtifactWritingTestPlanModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.path().to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(polaris_code_core::agents::CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider))
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));
    server
        .finish()
        .await
        .expect("the selecting server assembles");
    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");
    let _ = input_tx;

    let session_id = open_session(&server, project_dir.path());
    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    let TopologyTurn::Executed(executed) = outcome
        .topology
        .as_ref()
        .expect("the test-only plan is admitted and executed")
    else {
        panic!("expected an executed topology, got {:?}", outcome.topology);
    };
    assert_eq!(
        executed.product.kind,
        polaris_cosmos::contracts::ConclusionKind::Unresolved
    );
    assert!(
        executed
            .product
            .answer()
            .expect("the unresolved run has an answer")
            .starts_with("left the coding task unresolved")
    );
    assert_eq!(
        std::fs::read_to_string(project_dir.path().join("lib.rs")).expect("read source"),
        source,
        "the test-only graph left the source untouched"
    );
    assert_eq!(
        std::fs::read_to_string(project_dir.path().join("test-results.xml"))
            .expect("the passing command wrote its report"),
        "run",
        "the artifact is deliberately present but is not edit provenance"
    );
}

/// A first, well-formed but overlong plan refused by the materializer's own
/// instance limit, then a second, trimmed plan admitted — the case a small,
/// store-independent local counter for refused attempts has to keep in
/// order against the admitted pair's own numbering without knowing in
/// advance how much room the real store will actually leave (a fresh,
/// single-candidate turn like this one admits at whatever low position live
/// admission lands on, regardless of how many attempts were refused first).
#[tokio::test]
async fn a_refused_attempt_then_an_admitted_one_keeps_the_revision_trail_in_order() {
    let (project_dir, task) = project(polaris_code_core::cosmos::task::CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server.insert_global(CompositionPolicy::Gates);
    server.insert_global(polaris_code_core::cosmos::planner::GatePlannerMode::Model);
    // The first answer names four instances (an extra, harmless repeated
    // read-only judge step padding it out); the revised, admitted answer
    // names the ordinary three. Both are flat sequences, so `max_depth`
    // cannot tell them apart — `max_instances` is the limit a flat plan's
    // own step count actually trips.
    server.insert_global(GatePlanLimits(CompositionLimits {
        max_instances: 3,
        ..CompositionLimits::conservative()
    }));
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(SequencedGatePlanModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.path().to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(polaris_code_core::agents::CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));
    server
        .finish()
        .await
        .expect("the gate-plan selecting server assembles");
    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");
    let _ = input_tx;

    let session_id = open_session(&server, project_dir.path());
    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    let topology = outcome
        .topology
        .as_ref()
        .unwrap_or_else(|| panic!("the turn selected a topology; reason: {:?}", outcome.reason));
    let TopologyTurn::Executed(executed) = topology else {
        panic!("expected an executed topology, got {topology:?}");
    };
    assert!(
        executed.nodes_executed > 0,
        "the admitted, shallower plan really ran: {executed:?}"
    );
    let edited = std::fs::read_to_string(project_dir.path().join("src/lib.rs")).expect("read back");
    assert!(
        edited.contains(ADD_DONE),
        "the admitted, model-planned direct-edit shape actually applied the stated edit: {edited}"
    );

    let trail = executed
        .gate_plan_trail
        .as_ref()
        .expect("an executed gate-plan composition carries its revision trail");
    assert_eq!(
        trail.revisions().len(),
        2,
        "one refused, overlong answer, then the admitted, trimmed one"
    );
    assert!(
        !trail.revisions()[0].validation().is_admitted(),
        "the first, overlong model answer was refused for exceeding the instance limit"
    );
    assert_eq!(trail.revisions()[0].revision_attempt().get(), 1);
    assert!(
        trail.revisions()[1].validation().is_admitted(),
        "the second, valid model answer is the one that ran"
    );
    assert_eq!(trail.revisions()[1].revision_attempt().get(), 2);

    let report = polaris_code_core::exec::RunReport::new(&session_id, &outcome, 4_096);
    let reported_gate_plan = report
        .topology
        .as_ref()
        .expect("the report carries this turn's topology selection")
        .gate_plan
        .as_ref()
        .expect("the report names the admitted gate plan");
    assert_eq!(
        reported_gate_plan.planner,
        polaris_code_core::cosmos::planner::GatePlannerMode::Model.as_str(),
        "the report names the model planner, not the scripted one, for this turn"
    );

    // The whole evidence chain this trail closes into is in one strict
    // order, end to end: decision(refused) < validation(refused) <
    // decision(admitted) < validation(admitted) < run < outcome. Proving
    // this holds is the point of the test: the refused attempt's own
    // numbering must never collide with or outrun the admitted attempt's,
    // no matter how low a real store position live admission actually lands
    // on (a fresh, single-candidate turn like this one admits at position
    // two, regardless of how many attempts were refused first).
    let composition = executed
        .composition
        .as_ref()
        .expect("an executed gate-plan turn still carries ordinary composition evidence");
    let refused_decision = trail.revisions()[0].decision().recorded_at().sequence();
    let refused_validation = trail.revisions()[0].validation().recorded_at().sequence();
    let admitted_decision = trail.revisions()[1].decision().recorded_at().sequence();
    let admitted_validation = trail.revisions()[1].validation().recorded_at().sequence();
    let run_sequence = composition.run().recorded_at().sequence();
    let outcome_sequence = composition.outcome().recorded_at().sequence();
    assert!(
        refused_decision < refused_validation,
        "{refused_decision:?} < {refused_validation:?}"
    );
    assert!(
        refused_validation < admitted_decision,
        "{refused_validation:?} < {admitted_decision:?}"
    );
    assert!(
        admitted_decision < admitted_validation,
        "{admitted_decision:?} < {admitted_validation:?}"
    );
    assert!(
        admitted_validation < run_sequence,
        "{admitted_validation:?} < {run_sequence:?}"
    );
    assert!(
        run_sequence < outcome_sequence,
        "{run_sequence:?} < {outcome_sequence:?}"
    );
}
