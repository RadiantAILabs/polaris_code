//! The headline measurement: wall-clock and model-call cost, light turns vs.
//! heavy turns, without and with `auto_route`.
//!
//! `common::ScriptedProvider` always returns `Usage::default()` (there is no
//! network and no API key in this harness), so `RunUsage`'s token counters
//! read zero for every turn here regardless of path — that is a property of
//! the test double, not a finding. What this file measures instead is the
//! number of model round trips a turn actually makes, with a counting
//! provider local to this file, plus the wall-clock and
//! [`TurnOutcome::nodes_executed`] the run already reports. Against a real
//! provider, cost scales with round trips 1:1, so a round-trip count of zero
//! is exactly the token cost the light path claims: none.
//!
//! The "without `auto_route`" light-turn rows need no source revert: calling
//! `auto_route` at all is the difference, so a test that simply never calls it
//! reproduces exactly what a `--print` run of "where is fn add" does without
//! the router — the ordinary conversational turn, in full. The heavy-turn row
//! is not run twice: `run_one_turn`'s fallthrough guard (`run.rs`) only ever
//! matches `TaskOrigin::AutoRouted`, and `AcceptedCodingTask::new` defaults to
//! `TaskOrigin::Explicit`, so the code path an explicit accept takes is the
//! same with or without the router — there is nothing to run twice.
//!
//! Run with `cargo test -p polaris_code_core --test topology_effort_measurement
//! -- --nocapture --test-threads=1` to see the table this file prints.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use common::{ANSWER, open_session, project};
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{FileStore, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{CodeAgent, CodeAgentPlugin};
use polaris_code_core::cosmos::host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, auto_route,
};
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::task::CodingIntent;
use polaris_code_core::cosmos::topology::CodingTopologyPackPlugin;
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::run::{self, TurnOutcome};
use polaris_code_core::usage::TokenMeter;
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// [`common::ScriptedProvider`] with one addition: every `generate`/`stream`
/// call increments a counter its owner can read back — the measured proxy for
/// what a real provider would bill tokens for.
struct CountingProvider(Arc<AtomicUsize>);

impl LlmProvider for CountingProvider {
    fn name(&self) -> &'static str {
        // Must match the provider prefix `common::open_session` bakes into
        // its `AgentConfig` model id ("scripted/test-model").
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(ANSWER)],
            usage: Usage::default(),
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
        self.0.fetch_add(1, Ordering::SeqCst);
        use futures_util::StreamExt as _;
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
                usage: Usage::default(),
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers a fresh [`CountingProvider`] sharing `calls` with its caller.
struct CountingModelPlugin(Arc<AtomicUsize>);

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
            .register_llm_provider(CountingProvider(self.0.clone()));
    }
}

/// The same server `common::selecting_server` assembles, with a
/// [`CountingProvider`] in place of the plain scripted one so a test can read
/// back how many model round trips its turn actually made.
async fn measuring_server(
    data_dir: &Path,
) -> (
    Server,
    CodingTaskQueue,
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
    mpsc::Receiver<IOMessage>,
    Arc<AtomicUsize>,
) {
    let (provider, input_tx, output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();
    let calls = Arc::new(AtomicUsize::new(0));

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(CountingModelPlugin(calls.clone()))
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));

    server
        .finish()
        .await
        .expect("the measuring server assembles");

    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");

    (server, queue, provider, input_tx, output_rx, calls)
}

/// One measured row: what a turn actually cost.
struct Measurement {
    label: &'static str,
    elapsed_ms: u128,
    nodes_executed: usize,
    model_calls: usize,
}

impl Measurement {
    fn print(&self) {
        println!(
            "{:<70} wall={:>6}ms  nodes={:>3}  model_calls={:>2}",
            self.label, self.elapsed_ms, self.nodes_executed, self.model_calls
        );
    }
}

fn measure(
    label: &'static str,
    calls_before: usize,
    calls: &AtomicUsize,
    outcome: &TurnOutcome,
    elapsed: std::time::Duration,
) -> Measurement {
    Measurement {
        label,
        elapsed_ms: elapsed.as_millis(),
        nodes_executed: outcome.nodes_executed,
        model_calls: calls.load(Ordering::SeqCst) - calls_before,
    }
}

#[tokio::test]
async fn headline_measurement_light_vs_heavy_before_and_after() {
    let mut rows = Vec::new();

    // Row A — light turn, WITHOUT the router: `auto_route` is simply never
    // called, so the prompt reaches only the ordinary conversational turn,
    // exactly as a `--print "where is fn add"` run would without it.
    {
        let (project_dir, _task) = project(CodingIntent::Careful);
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (server, _queue, provider, input_tx, _output_rx, calls) =
            measuring_server(data_dir.path()).await;
        let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
        let session_id = open_session(&server, project_dir.path());
        input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text("where is fn add".to_string()),
            ))
            .await
            .expect("prompt enqueued");
        drop(input_tx);
        let calls_before = calls.load(Ordering::SeqCst);
        let started = Instant::now();
        let outcome = run::drive_turn(sessions, &session_id).await;
        let elapsed = started.elapsed();
        provider.close().await;
        assert_eq!(outcome.final_text.as_deref(), Some(ANSWER));
        rows.push(measure(
            "A. light turn, BEFORE (no auto_route: full conversational turn)",
            calls_before,
            &calls,
            &outcome,
            elapsed,
        ));
    }

    // Row B — the same light turn, WITH the router: `auto_route` recognises
    // it and the light path answers, with the same prompt still enqueued for
    // the conversational fallback exactly as `main.rs` enqueues it.
    {
        let (project_dir, _task) = project(CodingIntent::Careful);
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (server, queue, provider, input_tx, _output_rx, calls) =
            measuring_server(data_dir.path()).await;
        let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
        let session_id = open_session(&server, project_dir.path());
        let prompt = "where is fn add";
        queue.accept(auto_route(project_dir.path(), prompt).expect("a recognised question routes"));
        input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text(prompt.to_string()),
            ))
            .await
            .expect("prompt enqueued");
        drop(input_tx);
        let calls_before = calls.load(Ordering::SeqCst);
        let started = Instant::now();
        let outcome = run::drive_turn(sessions, &session_id).await;
        let elapsed = started.elapsed();
        provider.close().await;
        assert!(
            outcome.topology.is_some(),
            "the light path answered this turn"
        );
        assert_ne!(outcome.final_text.as_deref(), Some(ANSWER));
        rows.push(measure(
            "B. light turn, AFTER (auto_route: light path answers)",
            calls_before,
            &calls,
            &outcome,
            elapsed,
        ));
    }

    // Row C — a wrong guess, WITH the router: the light path attempts and
    // escalates, and the conversational turn still answers, in the same
    // request. This is the bound on wasted effort a misjudged route costs.
    {
        let (project_dir, _task) = project(CodingIntent::Careful);
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (server, queue, provider, input_tx, _output_rx, calls) =
            measuring_server(data_dir.path()).await;
        let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
        let session_id = open_session(&server, project_dir.path());
        let prompt = "what is in missing.rs";
        queue.accept(auto_route(project_dir.path(), prompt).expect("an inspect question routes"));
        input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text(prompt.to_string()),
            ))
            .await
            .expect("prompt enqueued");
        drop(input_tx);
        let calls_before = calls.load(Ordering::SeqCst);
        let started = Instant::now();
        let outcome = run::drive_turn(sessions, &session_id).await;
        let elapsed = started.elapsed();
        provider.close().await;
        assert!(outcome.topology.is_none(), "the wrong guess fell through");
        assert_eq!(outcome.final_text.as_deref(), Some(ANSWER));
        rows.push(measure(
            "C. light turn, wrong guess (auto_route escalates, falls through)",
            calls_before,
            &calls,
            &outcome,
            elapsed,
        ));
    }

    // Row D — heavy turn (an edit-and-verify task), run once: `run_one_turn`'s
    // fallthrough guard only matches `TaskOrigin::AutoRouted`, and
    // `AcceptedCodingTask::new` defaults to `TaskOrigin::Explicit`, so this
    // path is provably identical with and without the router — there is no
    // second run that could produce a different number.
    {
        let (project_dir, task) = project(CodingIntent::Careful);
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (server, queue, provider, input_tx, _output_rx, calls) =
            measuring_server(data_dir.path()).await;
        let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
        let session_id = open_session(&server, project_dir.path());
        queue.accept(AcceptedCodingTask::new(task));
        drop(input_tx);
        let calls_before = calls.load(Ordering::SeqCst);
        let started = Instant::now();
        let outcome = run::drive_turn(sessions, &session_id).await;
        let elapsed = started.elapsed();
        provider.close().await;
        assert!(
            outcome.topology.is_some(),
            "the heavy turn's own topology answered"
        );
        rows.push(measure(
            "D. heavy turn, BEFORE == AFTER (explicit accept, edit + verify)",
            calls_before,
            &calls,
            &outcome,
            elapsed,
        ));
    }

    println!(
        "\n--- effort/cost measurement (crates/polaris_code_core/tests/topology_effort_measurement.rs) ---"
    );
    for row in &rows {
        row.print();
    }
    println!("---\n");

    // The headline claims, asserted rather than merely printed:
    // - without the router, even a recognised question costs a real model turn;
    // - with it, the light path answers the identical question with none;
    // - a wrong guess still falls through to that same one-model-call
    //   answer, so it never compounds into two.
    assert!(
        rows[0].model_calls >= 1,
        "without the router, the only way to answer is the full conversational turn"
    );
    assert_eq!(
        rows[1].model_calls, 0,
        "the light path never calls the model"
    );
    assert_eq!(
        rows[2].model_calls, rows[0].model_calls,
        "a wrong guess costs exactly one conversational turn, the same as never having guessed"
    );
}
