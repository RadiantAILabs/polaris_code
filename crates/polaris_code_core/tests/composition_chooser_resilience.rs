//! Proof that an ordinary composition-chooser failure never ends the
//! session, and that a turn's real chooser calls are bounded and evidenced.
//!
//! Two failure modes of the model chooser: (1)
//! `select_and_run`'s fallback loop aborted the *whole* turn — and, via
//! `DriverStep::Stop`, the whole session — on the first candidate's ordinary
//! chooser failure (bad JSON, an unapproved recipe, an out-of-bounds
//! worklist bound, empty grounds), even though a normal, already-eligible
//! fallback candidate sat right there in `fallback_plan`; and (2) the loop
//! could make one real, billed model call per catalog registration with no
//! cap, and every discarded candidate's call vanished from evidence except
//! for an unnarrated meter delta.
//!
//! Both are pinned here against the *shipped* catalog
//! (`CodingTopologyPackPlugin`), where a fresh
//! `CodingIntent::Careful` task has more than one eligible registration
//! (`tests/topology_selection_host_loop.rs`'s own
//! `the_second_selection_ranks_on_what_the_first_run_recorded` proves a
//! second one exists), so `fallback_plan` here is never structurally empty
//! the way it was in `tests/topology_model_access.rs`'s single-registration
//! test pack.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{open_session, project};
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason as LlmStopReason, StreamEvent, Usage,
    UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{FileStore, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{CodeAgent, CodeAgentPlugin};
use polaris_code_core::cosmos::composition::{
    CompositionPolicy, MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN,
};
use polaris_code_core::cosmos::host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, TopologyTurn,
};
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::task::CodingIntent;
use polaris_code_core::cosmos::topology::{CodingTopologyPackPlugin, HostCapabilities};
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::run::{self, TurnReason};
use polaris_code_core::usage::TokenMeter;
use polaris_cosmos::evidence::{TopologyEvidence, TopologyEvidencePlugin};
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// The literal fixed text every composition-chooser prompt starts with
/// (`cosmos::composition::choose`'s own prompt template) — the discriminator
/// this file's mock provider uses to tell a chooser call apart from an
/// ordinary conversational one.
const CHOOSER_MARKER: &str = "Choose exactly one approved coding composition";

fn scripted_usage() -> Usage {
    Usage {
        input_tokens: Some(37),
        output_tokens: Some(11),
        total_tokens: Some(48),
        ..Default::default()
    }
}

/// The plain text of every user block in a request, concatenated. Good
/// enough to find the chooser's own marker string and its "Approved
/// recipes: ..." fact list.
fn request_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => content.iter().find_map(|block| match block {
                UserBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
            Message::Assistant { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reads the first recipe name out of a chooser prompt's own
/// "Approved recipes: X, Y. Facts: ..." fact list — always a member of
/// *that* call's registration's approved grammar, whichever registration it
/// turns out to be.
fn first_approved_recipe(prompt: &str) -> Option<String> {
    let marker = "Approved recipes: ";
    let start = prompt.find(marker)? + marker.len();
    let rest = &prompt[start..];
    let end = rest.find(". Facts:")?;
    rest[..end].split(", ").next().map(str::to_owned)
}

/// A provider that fails the chooser's first `fail_first` real calls with an
/// unparseable answer, then answers every later chooser call with the first
/// recipe its own prompt names as approved — so whichever registration is
/// actually asked next always gets a valid, approved answer, regardless of
/// which candidate the catalog's own ranking put there. An ordinary
/// conversational prompt (no chooser marker) always gets a fixed plain-text
/// answer and is never counted.
struct AdaptiveChooserProvider {
    chooser_calls: Arc<AtomicUsize>,
    fail_first: usize,
}

impl AdaptiveChooserProvider {
    fn answer_for(&self, request: &LlmRequest) -> String {
        let prompt = request_text(request);
        if !prompt.contains(CHOOSER_MARKER) {
            return "nothing to change here.".to_owned();
        }
        let index = self.chooser_calls.fetch_add(1, Ordering::SeqCst);
        if index < self.fail_first {
            return "not json at all".to_owned();
        }
        match first_approved_recipe(&prompt) {
            Some(recipe) => {
                let bound = if recipe == "polaris-code.recipe.bounded-worklist" {
                    ",\"worklist_bound\":4"
                } else {
                    ""
                };
                format!(
                    r#"{{"recipe":"{recipe}"{bound},"grounds":"adaptive test chooser names the first approved recipe"}}"#
                )
            }
            None => "not json at all".to_owned(),
        }
    }
}

impl LlmProvider for AdaptiveChooserProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(self.answer_for(&request))],
            usage: scripted_usage(),
            stop_reason: LlmStopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let text = self.answer_for(&request);
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(text),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: LlmStopReason::EndTurn,
                usage: scripted_usage(),
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct AdaptiveChooserModelPlugin {
    chooser_calls: Arc<AtomicUsize>,
    fail_first: usize,
}

impl Plugin for AdaptiveChooserModelPlugin {
    const ID: &'static str = "test::adaptive_chooser_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before AdaptiveChooserModelPlugin")
            .register_llm_provider(AdaptiveChooserProvider {
                chooser_calls: self.chooser_calls.clone(),
                fail_first: self.fail_first,
            });
    }
}

/// One assembled host, the shipped catalog (`CodingTopologyPackPlugin`)
/// behind the adaptive scripted provider above, with the model composition
/// policy installed and no network.
struct Harness {
    server: Server,
    queue: CodingTaskQueue,
    input_tx: mpsc::Sender<IOMessage>,
    chooser_calls: Arc<AtomicUsize>,
}

/// `fail_first` real chooser calls answer with unparseable text; every call
/// after that answers validly for whichever registration it was asked about.
async fn selecting_server(data_dir: &Path, fail_first: usize) -> Harness {
    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();
    let chooser_calls = Arc::new(AtomicUsize::new(0));

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server.insert_global(CompositionPolicy::Model);
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(AdaptiveChooserModelPlugin {
            chooser_calls: chooser_calls.clone(),
            fail_first,
        })
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        // Exactly the four `main.rs` installs.
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

    Harness {
        server,
        queue,
        input_tx,
        chooser_calls,
    }
}

fn accept(queue: &CodingTaskQueue, task: polaris_code_core::cosmos::task::CodingTask) {
    queue.accept(
        AcceptedCodingTask::new(task).with_host_capabilities(HostCapabilities {
            test_runner: false,
            model_provider: true,
        }),
    );
}

/// Sends one plain-text prompt and drives it, proving the session still
/// serves an ordinary conversational turn after whatever came before it.
async fn assert_session_still_serves_a_turn(
    sessions: &SessionsAPI,
    session_id: &polaris::sessions::SessionId,
    input_tx: mpsc::Sender<IOMessage>,
) {
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("thanks".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);
    let after = run::drive_turn(sessions, session_id).await;
    assert_eq!(
        after.reason,
        TurnReason::Completed,
        "the session must still serve an ordinary turn: {after:?}"
    );
    assert!(after.topology.is_none());
}

#[tokio::test]
async fn a_bad_first_chooser_answer_falls_through_to_the_next_candidate() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let harness = selecting_server(data_dir.path(), 1).await;
    let Harness {
        server,
        queue,
        input_tx,
        chooser_calls,
    } = harness;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    accept(&queue, task);
    let outcome = run::drive_turn(sessions, &session_id).await;

    // (1) The turn completed through a fallback candidate, not a fatal stop.
    assert_eq!(outcome.reason, TurnReason::Completed, "{outcome:?}");
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected a fallback candidate to execute, got {:?}",
            outcome.topology
        );
    };
    assert!(
        executed.fallback_position >= 1,
        "the top candidate's chooser call failed, so the winner must be a fallback: {executed:?}"
    );
    assert!(
        chooser_calls.load(Ordering::SeqCst) >= 2,
        "the refused attempt and the winning attempt each call the model once: {}",
        chooser_calls.load(Ordering::SeqCst)
    );

    // (2) The refused candidate's own billed call is durable evidence, with
    // its digests — not discarded because it did not win.
    let evidence = server
        .get_global::<TopologyEvidence>()
        .map(|handle| TopologyEvidence::clone(&handle))
        .expect("the evidence plugin published its capability");
    let episode = evidence
        .read_episode(executed.decision_id)
        .expect("the episode is readable")
        .expect("the decision was persisted");
    assert!(
        episode.admissions.len() >= 2,
        "both the refused attempt and the admitted one are recorded: {episode:?}"
    );
    let refused = episode
        .admissions
        .iter()
        .find(|admission| !admission.is_admitted())
        .expect("one attempt was refused by the chooser, not by live admission");
    let reasons = refused.result().reasons();
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code().as_str(), "composition.chooser-refused");
    assert!(
        !reasons[0].snapshot_refs().is_empty(),
        "the refused call's prompt/response digests are retained, not just its meter delta"
    );

    // (3) The session is not ended: it still serves the next ordinary turn.
    assert_session_still_serves_a_turn(sessions, &session_id, input_tx).await;
}

#[tokio::test]
async fn the_chooser_stops_after_its_per_turn_call_budget_and_the_session_survives() {
    let (project_dir, task) = project(CodingIntent::Careful);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    // Every real chooser call this turn makes fails, so the loop can only
    // stop for one of two reasons: it ran out of eligible candidates, or it
    // hit its own per-turn ceiling first. The assertions below tell them
    // apart.
    let harness = selecting_server(data_dir.path(), usize::MAX).await;
    let Harness {
        server,
        queue,
        input_tx,
        chooser_calls,
    } = harness;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    accept(&queue, task);
    let outcome = run::drive_turn(sessions, &session_id).await;

    // (1) A stated, recoverable refusal — never a fatal stop.
    assert_eq!(outcome.reason, TurnReason::NoTopologyStarted, "{outcome:?}");
    assert!(
        run::driver_continues(&outcome.reason),
        "an exhausted chooser budget must not stop the driver"
    );
    let Some(TopologyTurn::NotStarted(unstarted)) = outcome.topology.as_ref() else {
        panic!(
            "expected a stated no-start refusal, got {:?}",
            outcome.topology
        );
    };
    assert!(
        unstarted
            .reasons
            .iter()
            .any(|reason| reason == "composition.chooser-budget-exhausted"),
        "the refusal must explain that the budget stopped it, not the catalog running out: {:?}",
        unstarted.reasons
    );

    // (2) Bounded: the host made exactly its own ceiling's worth of real
    // calls, never one per catalog registration.
    assert_eq!(
        chooser_calls.load(Ordering::SeqCst),
        MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN,
        "the host must stop making real chooser calls at its own bound"
    );

    // (3) The session is not ended: it still serves the next ordinary turn.
    assert_session_still_serves_a_turn(sessions, &session_id, input_tx).await;
}
