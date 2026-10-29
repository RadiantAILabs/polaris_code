//! Proof that a user can hand `polaris-code` a piece of coding work and find
//! out what happened to it.
//!
//! `topology_selection_host_loop.rs` proves the selection point works when
//! something is in the queue; it puts the task there itself, by calling
//! `CodingTaskQueue::accept`. That is exactly what a user could not do. These
//! tests never touch `accept`: every task below enters the queue the way a
//! person puts it there — by dispatching `/topology …` on the same
//! [`CommandRegistry`] the terminal dispatches, or through
//! [`TopologyWork::accept_spec`], which is what `--topology-task` calls.
//!
//! What each test pins down:
//!
//! - work submitted **through the affordance** reaches the queue and the turn
//!   reports `Executed`;
//! - `NoEligibleCandidate` is reported to the person who submitted it, and is
//!   distinguishable from a turn in which nothing was submitted at all;
//! - a topology turn still does not call the model, does not advance the turn
//!   number, does not write the transcript and does not move the `TokenMeter`;
//! - one admission still starts exactly once, and the sealed catalog is
//!   unchanged by accepting and running work; and
//! - a refused or failed topology turn does not take the session with it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode};
use polaris_code_core::cosmos::host::{CodingTaskQueue, CodingTaskQueuePlugin, TopologyTurn};
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::topology::{CodingTopologyPackPlugin, PREDICTION_ERROR};
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider, RunReport};
use polaris_code_core::run::{self, TurnOutcome, TurnReason};
use polaris_code_core::terminal::history::CanonicalHistory;
use polaris_code_core::terminal::topology::turn_report;
use polaris_code_core::terminal::{CommandOutcome, CommandRegistry, TopologyWork};
use polaris_code_core::usage::TokenMeter;
use polaris_cosmos::evidence::{TopologyEvidence, TopologyEvidencePlugin};
use polaris_cosmos::topology::TopologyCatalog;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::commands::CommandInvocation;
use tokio::sync::mpsc;

/// The answer the scripted provider streams for a conversational turn.
const ANSWER: &str = "nothing to change here.";

/// The prompt left waiting while a topology turn runs.
const PENDING_PROMPT: &str = "is anything wrong?";

/// Non-zero usage, so "the `TokenMeter` did not move" is an observation about
/// the topology turn rather than about a provider that reports nothing.
const SCRIPTED_INPUT_TOKENS: u64 = 11;
const SCRIPTED_OUTPUT_TOKENS: u64 = 7;
const MAX_OUTPUT_TOKENS: u32 = 4096;

const LIB_RS: &str = "\
//! A tiny crate with two unimplemented helpers.

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    todo!()
}

/// Subtracts two numbers.
pub fn sub(a: i32, b: i32) -> i32 {
    todo!()
}
";

/// The `/topology` spec a user would type to implement `add`.
///
/// Quoted values with `\n` escapes, because the find text spans lines — this
/// is the syntax the command documents, exercised rather than bypassed.
const IMPLEMENT_ADD: &str = "file=src/lib.rs \
     find=\"pub fn add(a: i32, b: i32) -> i32 {\\n    todo!()\\n}\" \
     replace=\"pub fn add(a: i32, b: i32) -> i32 {\\n    a + b\\n}\" \
     intent=careful";

/// The same work, asked for under an objective no coding topology can meet.
const IMPLEMENT_ADD_COMPARATIVE: &str = "file=src/lib.rs \
     find=\"pub fn add(a: i32, b: i32) -> i32 {\\n    todo!()\\n}\" \
     replace=\"pub fn add(a: i32, b: i32) -> i32 {\\n    a + b\\n}\" \
     intent=careful verifier=comparative";

/// A provider that streams one fixed answer and counts every call.
///
/// The counter is the evidence for "a topology run does not call the model":
/// an assertion against a provider that merely *could* have been called proves
/// nothing, so this one records it.
#[derive(Clone, Default)]
struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

impl LlmProvider for CountingProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(ANSWER)],
            usage: scripted_usage(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(ANSWER.to_string()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: scripted_usage(),
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

fn scripted_usage() -> Usage {
    Usage {
        input_tokens: Some(SCRIPTED_INPUT_TOKENS),
        output_tokens: Some(SCRIPTED_OUTPUT_TOKENS),
        total_tokens: Some(SCRIPTED_INPUT_TOKENS + SCRIPTED_OUTPUT_TOKENS),
        ..Usage::default()
    }
}

/// Registers [`CountingProvider`] the way `AnthropicPlugin` registers its own.
struct CountingModelPlugin(CountingProvider);

impl Plugin for CountingModelPlugin {
    const ID: &'static str = "test::counting_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before CountingModelPlugin")
            .register_llm_provider(self.0.clone());
    }
}

/// Everything one test drives: the assembled host, the affordance a user
/// reaches, and the handles the assertions read.
struct Harness {
    server: Server,
    queue: CodingTaskQueue,
    work: TopologyWork,
    commands: CommandRegistry,
    provider: Arc<ExecIOProvider>,
    input_tx: Option<mpsc::Sender<IOMessage>>,
    calls: Arc<AtomicUsize>,
    meter: TokenMeter,
    session_id: SessionId,
    project: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl Harness {
    /// Assembles the host `main.rs` assembles, in `mode`, over a temporary
    /// crate — including the `/topology` registration, so the affordance under
    /// test is reached exactly as the binary reaches it.
    async fn start(mode: PermissionMode) -> Self {
        let project = tempfile::tempdir().expect("temp project");
        std::fs::create_dir_all(project.path().join("src")).expect("src dir");
        std::fs::write(project.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
        let data = tempfile::tempdir().expect("temp data dir");

        let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
        let provider = Arc::new(provider);
        let queue = CodingTaskQueue::new();
        let model = CountingProvider::default();
        let calls = model.calls.clone();
        let meter = TokenMeter::new();

        let mut server = Server::new();
        server.insert_global(meter.clone());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(CountingModelPlugin(model))
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                data.path().to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ExecIOPlugin::new(provider.clone()))
            .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
            .add_plugins(CodingTopologyPackPlugin)
            .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
            .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));
        server.finish().await.expect("the host assembles");
        let topology_catalog = server
            .get_global::<TopologyCatalog>()
            .expect("TopologyCatalog globalizes")
            .snapshot()
            .expect("the catalog is sealed");

        let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
        let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
        sessions.set_serializers(persistence.serializers());
        sessions
            .register_agent(CodeAgent)
            .expect("CodeAgent registers");

        let session_id = SessionId::new();
        let config = AgentConfig::new("scripted/test-model", project.path().to_path_buf());
        sessions
            .create_session_with_executor(
                server.create_context(),
                &session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
                move |ctx| {
                    ctx.insert(config.clone());
                },
            )
            .expect("the session is created");

        let work = TopologyWork::new(queue.clone(), project.path(), topology_catalog, move || {
            mode.clone()
        });
        let commands = CommandRegistry::with_builtins();
        work.register(&commands).expect("/topology registers");

        Self {
            server,
            queue,
            work,
            commands,
            provider,
            input_tx: Some(input_tx),
            calls,
            meter,
            session_id,
            project,
            _data: data,
        }
    }

    fn sessions(&self) -> &SessionsAPI {
        self.server.api::<SessionsAPI>().expect("SessionsAPI")
    }

    fn evidence(&self) -> TopologyEvidence {
        self.server
            .get_global::<TopologyEvidence>()
            .map(|handle| TopologyEvidence::clone(&handle))
            .expect("the evidence plugin published its capability")
    }

    /// Types `/topology <args>` exactly as the terminal dispatches it.
    async fn topology(&self, args: &str) -> CommandOutcome {
        self.commands
            .dispatch(&CommandInvocation::new("topology", args))
            .await
    }

    /// Leaves an ordinary prompt waiting, then drives one turn.
    ///
    /// Every topology turn below is driven this way on purpose. A pending
    /// prompt is what the session looks like in practice, and it makes the
    /// failure mode observable: if the work never reached the queue, the turn
    /// serves the prompt conversationally and returns instead of blocking, so
    /// each assertion below fails rather than hanging. The prompt must still
    /// be there afterwards — a topology turn consumes no user input.
    async fn topology_turn(&self) -> TurnOutcome {
        self.prompt(PENDING_PROMPT).await;
        let outcome = self.turn().await;
        assert!(
            outcome.topology.is_some(),
            "the turn was served conversationally, so nothing reached the queue: {:?}",
            outcome.reason
        );
        outcome
    }

    /// Types an ordinary prompt, as a user would before a conversational turn.
    async fn prompt(&self, text: &str) {
        self.input_tx
            .as_ref()
            .expect("the prompt channel is open")
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text(text.into()),
            ))
            .await
            .expect("the prompt is enqueued before the turn starts");
    }

    async fn turn(&self) -> TurnOutcome {
        run::drive_turn(self.sessions(), &self.session_id).await
    }

    fn source(&self) -> String {
        std::fs::read_to_string(self.project.path().join("src/lib.rs")).expect("the seeded file")
    }

    /// The sealed catalog's identity, for a before/after comparison.
    fn catalog_identity(&self) -> (u64, String, Vec<String>) {
        let snapshot = self
            .server
            .get_global::<TopologyCatalog>()
            .expect("the catalog plugin published its capability")
            .snapshot()
            .expect("the catalog is sealed and readable");
        (
            snapshot.generation(),
            format!("{:?}", snapshot.content_digest()),
            snapshot
                .candidates()
                .iter()
                .map(|candidate| candidate.registration_id().to_string())
                .collect(),
        )
    }
}

/// The canonical transcript, read the way the terminal reads it.
async fn transcript(harness: &Harness) -> Vec<String> {
    let history = CanonicalHistory::default();
    history
        .refresh(harness.sessions(), &harness.session_id)
        .await
        .expect("the canonical transcript is readable");
    history
        .replay()
        .expect("history is available")
        .iter()
        .map(|message| format!("{:?}", message.content))
        .collect()
}

fn report_text(outcome: &TurnOutcome) -> String {
    let message = turn_report(outcome).expect("a topology turn reports itself");
    match message.content {
        IOContent::Text(text) => text,
        other => panic!("expected a text report, got {other:?}"),
    }
}

#[tokio::test]
async fn work_submitted_through_the_command_reaches_the_queue_and_runs() {
    let harness = Harness::start(PermissionMode::AutoEdit).await;
    assert!(harness.queue.is_empty(), "nothing is queued at startup");
    assert!(harness.source().contains("todo!()"));

    // The whole point: the task enters the queue by being typed, not by a
    // test calling `CodingTaskQueue::accept`.
    let submitted = harness.topology(IMPLEMENT_ADD).await;
    let CommandOutcome::Control(receipt) = &submitted else {
        panic!("an allowed posture accepts the work outright, got {submitted:?}");
    };
    assert!(receipt.contains("Accepted coding work"), "{receipt}");
    assert!(
        submitted.ends_turn(),
        "the idle turn ends so the next one reaches the selection point"
    );
    assert_eq!(
        harness.queue.len(),
        1,
        "the command put exactly one task into the host's own queue"
    );

    // The next turn is the one `drive_turn` was going to run anyway.
    let outcome = harness.topology_turn().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    let topology = outcome.topology.as_ref().expect("the turn selected");
    let TopologyTurn::Executed(executed) = topology else {
        panic!("expected an executed topology, got {topology:?}");
    };
    assert_eq!(executed.registration_id, PREDICTION_ERROR);
    assert!(executed.nodes_executed > 0, "the native graph really ran");
    assert!(
        harness.source().contains("a + b"),
        "the topology applied the edit the user asked for: {}",
        harness.source()
    );
    assert!(
        harness.queue.is_empty(),
        "the queue was drained by the turn"
    );

    // And the user is told what happened, naming the topology and the work.
    let report = report_text(&outcome);
    assert!(report.contains("A topology ran"), "{report}");
    assert!(report.contains(PREDICTION_ERROR), "{report}");
    assert!(report.contains("user/src-lib-rs/1"), "{report}");
    assert!(report.contains("why this one"), "{report}");
    assert!(report.contains("alternatives considered"), "{report}");

    // Rendering is a pure projection of the record the turn already carries:
    // the same outcome renders byte-identical text every time, so nothing is
    // re-derived from live catalog or selector state on each call.
    assert_eq!(report_text(&outcome), report);

    // The non-interactive front end names the same winner, structured rather
    // than in prose — both front ends agree on which topology ran.
    let selection = RunReport::new(&harness.session_id, &outcome, MAX_OUTPUT_TOKENS)
        .topology
        .expect("an executed turn's json reports its selection");
    assert_eq!(selection.registration_id, PREDICTION_ERROR);
    assert_eq!(selection.decision_id, executed.decision_id);
    assert!(
        selection.candidates.iter().any(|candidate| candidate
            .candidate()
            .registration_id()
            .as_str()
            == PREDICTION_ERROR),
        "the winner is among the candidates the decision considered"
    );

    // The prompt that was waiting is still waiting: the topology turn took
    // the accepted work, not the user's next sentence.
    let next = harness.turn().await;
    assert!(next.topology.is_none());
    assert_eq!(next.final_text.as_deref(), Some(ANSWER));
    harness.provider.close().await;
}

#[tokio::test]
async fn the_launch_flag_reaches_the_same_queue() {
    // `--topology-task` is fail-closed unattended: it accepts only with the
    // explicit `--allow-edit` grant, which is what this mode represents.
    let harness = Harness::start(PermissionMode::Unattended(
        polaris_code_core::agents::UnattendedGrants {
            edit_file: true,
            run_command_patterns: vec![],
        },
    ))
    .await;

    let accepted = harness
        .work
        .accept_spec(IMPLEMENT_ADD)
        .expect("a granted unattended run accepts the spec");
    assert_eq!(accepted.queued, 1);

    let outcome = harness.topology_turn().await;
    assert!(matches!(
        outcome.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));
    assert!(harness.source().contains("a + b"));
    assert_eq!(
        RunReport::new(&harness.session_id, &outcome, MAX_OUTPUT_TOKENS).exit_code(),
        0,
        "the work was done, so the unattended run succeeded"
    );
}

#[tokio::test]
async fn a_refused_candidate_is_reported_and_a_quiet_turn_is_not() {
    let harness = Harness::start(PermissionMode::AutoEdit).await;

    // Submitted, considered, and refused.
    let submitted = harness.topology(IMPLEMENT_ADD_COMPARATIVE).await;
    assert!(matches!(submitted, CommandOutcome::Control(_)));
    let refused = harness.topology_turn().await;

    assert_eq!(refused.reason, TurnReason::NoTopologyStarted);
    let topology = refused.topology.as_ref().expect("the turn made a decision");
    let TopologyTurn::NoEligibleCandidate(unstarted) = topology else {
        panic!("expected a no-eligible answer, got {topology:?}");
    };
    assert!(harness.source().contains("todo!()"), "nothing was edited");

    // The user is told, in as many words, that it was refused rather than
    // forgotten — with the selector's own codes.
    let report = report_text(&refused);
    assert!(report.contains("No topology was eligible"), "{report}");
    assert!(report.contains("considered and refused"), "{report}");
    assert!(report.contains("Your workspace is unchanged"), "{report}");
    assert!(report.contains(&unstarted.reasons[0]), "{report}");

    // The non-interactive front end says the same thing in its own idiom.
    let error_line = RunReport::new(&harness.session_id, &refused, MAX_OUTPUT_TOKENS)
        .error_line()
        .expect("a no-start turn explains itself");
    assert!(
        error_line.starts_with("polaris-code: no topology started: "),
        "{error_line}"
    );
    assert!(
        RunReport::new(&harness.session_id, &refused, MAX_OUTPUT_TOKENS)
            .topology
            .is_none(),
        "nothing executed, so there is no selection to report structurally"
    );

    // A turn in which nothing was submitted is silent here — which is how the
    // two are told apart. "Refused" is a message; "never asked" is no message.
    // This is the very prompt that was already waiting during the refused
    // turn, so the same session gives both answers back to back.
    let quiet = harness.turn().await;
    assert_eq!(quiet.reason, TurnReason::Completed);
    assert!(quiet.topology.is_none());
    assert!(
        turn_report(&quiet).is_none(),
        "a conversational turn reports no topology answer at all"
    );
}

#[tokio::test]
async fn a_topology_turn_touches_none_of_the_four_session_facts() {
    let harness = Harness::start(PermissionMode::AutoEdit).await;

    // Establish that each of the four *can* move, by moving them: one ordinary
    // conversational turn. An assertion that something stayed put is worthless
    // unless the same harness has been seen to move it.
    harness.prompt(PENDING_PROMPT).await;
    let conversational = harness.turn().await;
    assert_eq!(conversational.turn_number, 1);
    assert_eq!(conversational.final_text.as_deref(), Some(ANSWER));
    let calls_after_chat = harness.calls.load(Ordering::SeqCst);
    let transcript_after_chat = transcript(&harness).await;
    let tokens_after_chat = harness.meter.total_tokens();
    assert!(
        calls_after_chat > 0,
        "the conversational turn called the model"
    );
    assert!(
        !transcript_after_chat.is_empty(),
        "the conversational turn wrote the transcript"
    );
    assert!(
        tokens_after_chat > 0,
        "the conversational turn moved the TokenMeter"
    );

    // Now the topology turn, through the affordance.
    assert!(matches!(
        harness.topology(IMPLEMENT_ADD).await,
        CommandOutcome::Control(_)
    ));
    let outcome = harness.topology_turn().await;
    assert!(matches!(
        outcome.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));
    assert!(harness.source().contains("a + b"), "the topology did run");

    assert_eq!(
        harness.calls.load(Ordering::SeqCst),
        calls_after_chat,
        "a topology run does not call the model"
    );
    assert_eq!(
        outcome.turn_number, 1,
        "a topology run does not advance the session's turn number"
    );
    assert_eq!(
        transcript(&harness).await,
        transcript_after_chat,
        "a topology run does not write the conversational transcript"
    );
    assert_eq!(
        harness.meter.total_tokens(),
        tokens_after_chat,
        "a topology run does not move the TokenMeter"
    );

    // All four move again the moment a conversational turn serves the prompt
    // the topology turn left untouched — so the four readings above are the
    // topology turn's own, not a harness that stopped measuring.
    let resumed = harness.turn().await;
    assert!(resumed.topology.is_none());
    assert_eq!(resumed.turn_number, 2);
    assert!(harness.calls.load(Ordering::SeqCst) > calls_after_chat);
    assert!(harness.meter.total_tokens() > tokens_after_chat);
    assert!(transcript(&harness).await.len() > transcript_after_chat.len());
    harness.provider.close().await;
}

#[tokio::test]
async fn one_admission_starts_once_and_the_catalog_is_unchanged() {
    let harness = Harness::start(PermissionMode::AutoEdit).await;
    let before = harness.catalog_identity();

    assert!(matches!(
        harness.topology(IMPLEMENT_ADD).await,
        CommandOutcome::Control(_)
    ));
    let after_accept = harness.catalog_identity();
    assert_eq!(
        before, after_accept,
        "accepting work registers, unregisters and mutates nothing"
    );

    let outcome = harness.topology_turn().await;
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", outcome.topology);
    };
    assert_eq!(
        before,
        harness.catalog_identity(),
        "running a topology leaves the sealed catalog exactly as it was"
    );

    // One decision, one live admission, one run. The permit that authorised it
    // was consumed by value — a second `consume` is the `compile_fail,E0382`
    // doctest on `cosmos::host`, not a runtime branch this test could reach.
    let episode = harness
        .evidence()
        .read_episode(executed.decision_id)
        .expect("the episode is readable")
        .expect("the decision was persisted");
    assert_eq!(
        episode.admissions.len(),
        1,
        "exactly one admission was attempted and permitted"
    );
    assert!(episode.admissions[0].is_admitted());
    assert_eq!(episode.runs.len(), 1, "exactly one run started");
    assert!(
        episode.not_started.is_none(),
        "a started run has no no-start terminal"
    );

    // A second turn with nothing further submitted starts nothing more: the
    // one accepted task cannot be started again.
    let second = harness.turn().await;
    assert!(second.topology.is_none());
    assert_eq!(
        harness
            .evidence()
            .store_status()
            .expect("status")
            .retained_runs,
        1,
        "no second run was opened for the one accepted task"
    );
}

#[tokio::test]
async fn the_session_survives_a_refused_run_and_takes_the_next_prompt() {
    let harness = Harness::start(PermissionMode::AutoEdit).await;

    // A refused topology turn.
    assert!(matches!(
        harness.topology(IMPLEMENT_ADD_COMPARATIVE).await,
        CommandOutcome::Control(_)
    ));
    let refused = harness.topology_turn().await;
    assert!(matches!(
        refused.topology.as_ref(),
        Some(TopologyTurn::NoEligibleCandidate(_))
    ));
    assert!(
        run::driver_continues(&refused.reason),
        "a refusal is an answer, not a failure that stops the driver"
    );

    // The session takes the next prompt and answers it normally.
    let next = harness.turn().await;
    assert_eq!(next.reason, TurnReason::Completed);
    assert_eq!(next.final_text.as_deref(), Some(ANSWER));
    assert_eq!(next.turn_number, 1, "the conversational turn advanced");

    // And it still takes new coding work afterwards, which now runs.
    assert!(matches!(
        harness.topology(IMPLEMENT_ADD).await,
        CommandOutcome::Control(_)
    ));
    let third = harness.topology_turn().await;
    assert!(matches!(
        third.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));
    assert!(harness.source().contains("a + b"));

    harness.provider.close().await;
}

/// A spec the workspace cannot honour is refused at submission, so no turn is
/// spent on it and the session is never in a half-accepted state.
#[tokio::test]
async fn unreachable_work_is_refused_before_a_turn_is_spent() {
    let harness = Harness::start(PermissionMode::AutoEdit).await;

    let refused = harness.topology("file=../escape.rs find=x replace=y").await;
    assert!(
        matches!(refused, CommandOutcome::Error(_)),
        "{refused:?} should be refused"
    );
    assert!(!refused.ends_turn(), "a refusal does not end the turn");
    assert!(harness.queue.is_empty());

    // The session is untouched: the next prompt is served normally.
    harness.prompt(PENDING_PROMPT).await;
    let next = harness.turn().await;
    assert_eq!(next.reason, TurnReason::Completed);
    assert!(next.topology.is_none());
    harness.provider.close().await;
}

/// General coding work uses the same queue and selection point as an exact
/// edit; it only omits the precomputed mutation. This is the shape an external
/// benchmark adapter can submit without inventing a second run surface.
#[tokio::test]
async fn a_statement_task_reaches_the_queue_and_runs_the_answer_topology() {
    let harness = Harness::start(PermissionMode::AutoEdit).await;
    let submitted = harness
        .topology(
            "statement=\"where is fn add\" ref=statement-task intent=answer \
             topology=code.tree-question",
        )
        .await;
    assert!(
        matches!(submitted, CommandOutcome::Control(_)),
        "{submitted:?}"
    );

    let outcome = harness.topology_turn().await;
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected an executed statement task, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(executed.registration_id, "code.tree-question");
    harness.provider.close().await;
}
