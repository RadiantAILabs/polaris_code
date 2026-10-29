//! A batch of independent read-only tool calls runs concurrently, in order.
//!
//! Every test here drives the **real** `CodeAgent` through `run::drive_turn`
//! against a scripted provider that emits one assistant message carrying a
//! whole batch of tool calls. The fixture toolset borrows the names the shipped
//! toolsets use (`read_file`, `read_span`, `search`, `list_files`, `outline`,
//! `edit_file`) precisely because the agent's read-only/mutating split is keyed
//! on the tool's declared name via [`polaris_code::tool_is_read_only`] —
//! a fixture called `slow_fixture` would exercise nothing.
//!
//! Each fixture's single argument is a spec, `"<label>:<delay_ms>"` or
//! `"<label>:pending"`, so a test can make completion order disagree with
//! request order and still demand request order out of both the canonical
//! message history and the reified trajectory.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use oxigraph::sparql::QueryResults;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, ToolResultContent, Usage,
    UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    CONFIRMED, CONFIRMED_TRUE, IO_TYPE, IO_TYPE_CONFIRM, IOContent, IOMessage, PersistencePlugin,
};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, toolset};
use polaris_code::agents::code_agent::MAX_CONCURRENT_READ_ONLY_CALLS;
use polaris_code::agents::trajectory::SESSION_GRAPH_IRI;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, TurnReason};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};

// ─── the scripted provider ───────────────────────────────────────────────────

/// One tool call the scripted assistant turn asks for.
#[derive(Clone)]
struct Ask {
    id: String,
    name: String,
    /// Argument name the fixture declares — `edit_file` deliberately differs.
    arg: &'static str,
    spec: String,
}

fn ask(index: usize, name: &str, spec: &str) -> Ask {
    Ask {
        id: format!("call-{index}"),
        name: name.to_string(),
        arg: "path",
        spec: spec.to_string(),
    }
}

fn ask_edit(index: usize, spec: &str) -> Ask {
    Ask {
        arg: "spec",
        ..ask(index, "edit_file", spec)
    }
}

/// Emits each queued batch as one assistant message; an empty batch ends the turn.
struct ScriptedProvider {
    batches: Mutex<VecDeque<Vec<Ask>>>,
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "parallel-batch-test"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("CodeAgent uses streaming generation")
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        let batch = self.batches.lock().unwrap().pop_front().unwrap_or_default();
        let mut events: Vec<Result<StreamEvent, GenerationError>> = Vec::new();
        if batch.is_empty() {
            events.push(Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }));
            events.push(Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("batch done".into()),
            }));
            events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
            events.push(Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                id: None,
                model: None,
            }));
        } else {
            for (slot, call) in batch.into_iter().enumerate() {
                let index = u32::try_from(slot).unwrap();
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockStartData::ToolCall {
                        id: call.id,
                        call_id: None,
                        name: call.name,
                    },
                }));
                events.push(Ok(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: serde_json::json!({ call.arg: call.spec }).to_string(),
                    },
                }));
                events.push(Ok(StreamEvent::ContentBlockStop { index }));
            }
            events.push(Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                id: None,
                model: None,
            }));
        }
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct ScriptedModelPlugin(Mutex<Option<ScriptedProvider>>);

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::parallel_batch_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let provider = self.0.lock().unwrap().take().expect("one build");
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(provider);
    }
}

// ─── the fixture toolset ─────────────────────────────────────────────────────

/// What the fixtures observed: how many ran at once, and how many started.
#[derive(Default)]
struct FixtureState {
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    entered: AtomicUsize,
}

/// Increments the in-flight count for as long as a fixture body is alive.
///
/// The decrement is in `Drop`, so a fixture whose future is dropped at an await
/// point — exactly what turn cancellation does — is counted as no longer
/// running without the body having to reach any particular line.
struct InFlight(Arc<FixtureState>);

impl InFlight {
    fn enter(state: &Arc<FixtureState>) -> Self {
        let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(now, Ordering::SeqCst);
        state.entered.fetch_add(1, Ordering::SeqCst);
        Self(state.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Fixtures registered under the shipped tools' names.
#[derive(Clone)]
struct BatchFixtures {
    state: Arc<FixtureState>,
}

impl BatchFixtures {
    async fn run(&self, spec: &str) -> Result<String, ToolError> {
        let _guard = InFlight::enter(&self.state);
        let (label, delay) = spec.split_once(':').unwrap_or((spec, "0"));
        if delay == "pending" {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(Duration::from_millis(delay.parse().unwrap_or(0))).await;
        Ok(format!("fixture:{label}"))
    }
}

#[toolset]
impl BatchFixtures {
    /// Read-only fixture standing in for the shipped `read_file`.
    #[tool]
    async fn read_file(
        &self,
        /// Fixture spec, `<label>:<delay_ms>`.
        path: String,
    ) -> Result<String, ToolError> {
        self.run(&path).await
    }

    /// Read-only fixture standing in for the shipped `read_span`.
    #[tool]
    async fn read_span(
        &self,
        /// Fixture spec, `<label>:<delay_ms>`.
        path: String,
    ) -> Result<String, ToolError> {
        self.run(&path).await
    }

    /// Read-only fixture standing in for the shipped `search`.
    #[tool]
    async fn search(
        &self,
        /// Fixture spec, `<label>:<delay_ms>`.
        path: String,
    ) -> Result<String, ToolError> {
        self.run(&path).await
    }

    /// Read-only fixture standing in for the shipped `list_files`.
    #[tool]
    async fn list_files(
        &self,
        /// Fixture spec, `<label>:<delay_ms>`.
        path: String,
    ) -> Result<String, ToolError> {
        self.run(&path).await
    }

    /// Read-only fixture standing in for the shipped `outline`.
    #[tool]
    async fn outline(
        &self,
        /// Fixture spec, `<label>:<delay_ms>`.
        path: String,
    ) -> Result<String, ToolError> {
        self.run(&path).await
    }

    /// Mutating fixture standing in for the shipped `edit_file`.
    ///
    /// The argument is deliberately **not** called `path`: the agent re-indexes
    /// whatever path a successful `edit_file` names, and this fixture writes no
    /// file, so naming it `path` would only exercise the indexer's error log.
    #[tool]
    async fn edit_file(
        &self,
        /// Fixture spec, `<label>:<delay_ms>`.
        spec: String,
    ) -> Result<String, ToolError> {
        self.run(&spec).await
    }
}

struct FixturePlugin {
    fixtures: BatchFixtures,
    confirm: Vec<&'static str>,
}

impl Plugin for FixturePlugin {
    const ID: &'static str = "test::parallel_batch_fixtures";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server.get_resource_mut::<ToolRegistry>().unwrap();
        registry.register_toolset(self.fixtures.clone());
        for name in &self.confirm {
            registry
                .set_permission(name, ToolPermission::Confirm)
                .unwrap();
        }
    }
}

// ─── the harness ─────────────────────────────────────────────────────────────

struct Harness {
    server: Server,
    sessions: SessionsAPI,
    id: SessionId,
    state: Arc<FixtureState>,
    input: tokio::sync::mpsc::Sender<IOMessage>,
    _workspace: tempfile::TempDir,
}

impl Harness {
    /// Boot the real agent over `batches`; `confirm` names tools gated on the
    /// user's approval, which the harness answers with "yes".
    async fn start(batches: Vec<Vec<Ask>>, confirm: Vec<&'static str>) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let state = Arc::new(FixtureState::default());
        let (provider, input, mut output) = ExecIOProvider::new(64);
        let provider = Arc::new(provider);

        let mut server = Server::new();
        server.insert_global(TokenMeter::new());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin(Mutex::new(Some(ScriptedProvider {
                batches: Mutex::new(batches.into()),
            }))))
            .add_plugins(ToolsPlugin)
            .add_plugins(FixturePlugin {
                fixtures: BatchFixtures {
                    state: state.clone(),
                },
                confirm,
            })
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ExecIOPlugin::new(provider.clone()));
        server.finish().await.unwrap();

        // Drain the host channel so a wide batch's activity blocks can never
        // fill it and serialise the agent behind backpressure, and answer any
        // confirmation prompt affirmatively.
        let approver = input.clone();
        tokio::spawn(async move {
            while let Some(message) = output.recv().await {
                if message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_CONFIRM) {
                    let _ = approver
                        .send(
                            IOMessage::from_external("test", IOContent::Text("yes".into()))
                                .with_metadata(CONFIRMED, CONFIRMED_TRUE),
                        )
                        .await;
                }
            }
        });

        let sessions = server.api::<SessionsAPI>().unwrap().clone();
        sessions.register_agent(CodeAgent).unwrap();
        let id = SessionId::new();
        let cfg = AgentConfig::new("parallel-batch-test/model", workspace.path().to_path_buf());
        sessions
            .create_session_with(
                server.create_context(),
                &id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                move |ctx| {
                    ctx.insert(cfg.clone());
                },
            )
            .unwrap();

        Self {
            server,
            sessions,
            id,
            state,
            input,
            _workspace: workspace,
        }
    }

    async fn prompt(&self, text: &str) {
        self.input
            .send(IOMessage::from_external(
                "test",
                IOContent::Text(text.into()),
            ))
            .await
            .unwrap();
    }

    /// Run one turn and return how long it took.
    async fn turn(&self, text: &str) -> (TurnReason, Duration) {
        self.prompt(text).await;
        let started = Instant::now();
        let outcome = run::drive_turn(&self.sessions, &self.id).await;
        (outcome.reason, started.elapsed())
    }

    async fn history(&self) -> Vec<Message> {
        self.sessions
            .with_context(&self.id, |ctx| {
                ctx.get_resource::<ContextManager>()
                    .unwrap()
                    .messages
                    .clone()
            })
            .await
            .unwrap()
    }

    fn store(&self) -> OxigraphStore {
        OxigraphStore::clone(&self.server.get_global::<OxigraphStore>().unwrap())
    }
}

// ─── readers ─────────────────────────────────────────────────────────────────

/// `(tool_use ids, (tool_result id, body))` in canonical-history order.
fn exchange(messages: &[Message]) -> (Vec<String>, Vec<(String, String)>) {
    let mut uses = Vec::new();
    let mut results = Vec::new();
    for message in messages {
        match message {
            Message::Assistant { content, .. } => {
                for block in content {
                    if let AssistantBlock::ToolCall(call) = block {
                        uses.push(call.id.clone());
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let UserBlock::ToolResult(result) = block {
                        let ToolResultContent::Text(body) = &result.content else {
                            panic!("tool results are text");
                        };
                        results.push((result.id.clone(), body.clone()));
                    }
                }
            }
        }
    }
    (uses, results)
}

/// Every reified `ToolCall` as `(tool, status)`, in the order it was written.
///
/// Write order is recoverable from the node IRI: `record_tool_call` suffixes it
/// with a process-monotonic counter, so sorting on that suffix reconstructs the
/// sequence the agent recorded regardless of how the store returns solutions.
fn recorded(store: &OxigraphStore) -> Vec<(String, String)> {
    let query = format!(
        "SELECT ?c ?tool ?status WHERE {{ GRAPH <{SESSION_GRAPH_IRI}> {{ \
         ?c a <urn:polaris:session:ToolCall> ; \
            <urn:polaris:session:tool> ?tool ; \
            <urn:polaris:session:status> ?status }} }}"
    );
    let QueryResults::Solutions(solutions) = store.query(&query).unwrap() else {
        panic!("SELECT returns solutions");
    };
    let mut rows: Vec<(u64, String, String)> = solutions
        .flatten()
        .map(|solution| {
            let iri = solution
                .get("c")
                .map(ToString::to_string)
                .unwrap_or_default();
            let seq = iri
                .trim_end_matches('>')
                .rsplit('/')
                .next()
                .and_then(|tail| tail.parse().ok())
                .unwrap_or(0);
            (
                seq,
                literal(&solution, "tool"),
                literal(&solution, "status"),
            )
        })
        .collect();
    rows.sort_by_key(|(seq, _, _)| *seq);
    rows.into_iter().map(|(_, t, s)| (t, s)).collect()
}

fn literal(solution: &oxigraph::sparql::QuerySolution, name: &str) -> String {
    solution
        .get(name)
        .map(ToString::to_string)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

fn tools(recorded: &[(String, String)]) -> Vec<&str> {
    recorded.iter().map(|(tool, _)| tool.as_str()).collect()
}

fn statuses(recorded: &[(String, String)]) -> Vec<&str> {
    recorded.iter().map(|(_, status)| status.as_str()).collect()
}

fn bodies(results: &[(String, String)]) -> Vec<&str> {
    results.iter().map(|(_, body)| body.as_str()).collect()
}

fn ids(results: &[(String, String)]) -> Vec<&str> {
    results.iter().map(|(id, _)| id.as_str()).collect()
}

// ─── the measurement ─────────────────────────────────────────────────────────

/// Five reads whose latencies sum to 900 ms, requested in the order that makes
/// completion order the exact reverse.
fn staggered_reads() -> Vec<Ask> {
    vec![
        ask(0, "read_file", "a:300"),
        ask(1, "read_span", "b:240"),
        ask(2, "search", "c:180"),
        ask(3, "list_files", "d:120"),
        ask(4, "outline", "e:60"),
    ]
}

const STAGGERED_SUM: Duration = Duration::from_millis(900);
const STAGGERED_SLOWEST: Duration = Duration::from_millis(300);

#[tokio::test]
async fn an_independent_read_batch_costs_its_slowest_call_not_their_sum() {
    let harness = Harness::start(vec![staggered_reads(), vec![]], vec![]).await;
    let (reason, elapsed) = harness.turn("read five files").await;
    assert_eq!(reason, TurnReason::Completed);

    eprintln!(
        "five independent reads: sum of parts {} ms, wall clock {} ms",
        STAGGERED_SUM.as_millis(),
        elapsed.as_millis()
    );
    assert!(
        elapsed >= STAGGERED_SLOWEST,
        "the batch cannot beat its slowest call ({} ms)",
        elapsed.as_millis()
    );
    assert!(
        elapsed < STAGGERED_SUM / 2,
        "the batch took {} ms; sequential execution costs {} ms",
        elapsed.as_millis(),
        STAGGERED_SUM.as_millis()
    );
}

#[tokio::test]
async fn results_and_trajectory_follow_request_order_not_completion_order() {
    let harness = Harness::start(vec![staggered_reads(), vec![]], vec![]).await;
    assert_eq!(
        harness.turn("read five files").await.0,
        TurnReason::Completed
    );

    let (uses, results) = exchange(&harness.history().await);
    assert_eq!(uses, ["call-0", "call-1", "call-2", "call-3", "call-4"]);
    assert_eq!(
        ids(&results),
        ["call-0", "call-1", "call-2", "call-3", "call-4"],
        "results must pair the calls in the order the model asked"
    );
    assert_eq!(
        bodies(&results),
        [
            "fixture:a",
            "fixture:b",
            "fixture:c",
            "fixture:d",
            "fixture:e"
        ],
        "the slowest call was requested first and must still be reported first"
    );

    let recorded = recorded(&harness.store());
    assert_eq!(
        tools(&recorded),
        ["read_file", "read_span", "search", "list_files", "outline"],
        "the reified trajectory must be written in request order"
    );
    assert_eq!(statuses(&recorded), ["ok", "ok", "ok", "ok", "ok"]);
}

#[tokio::test]
async fn request_order_holds_across_repeated_batches() {
    // Twenty batches whose delays rotate, so across the run every call position
    // gets to be the first and the last to finish. One assertion that passed by
    // luck would have to get lucky twenty times over.
    const ROUNDS: usize = 20;
    let names = ["read_file", "read_span", "search", "list_files", "outline"];
    let mut batches = Vec::new();
    for round in 0..ROUNDS {
        let batch = names
            .iter()
            .enumerate()
            .map(|(slot, name)| {
                let delay = 2 + ((slot + round) % names.len()) * 3;
                ask(slot, name, &format!("{slot}:{delay}"))
            })
            .collect();
        batches.push(batch);
        batches.push(vec![]);
    }

    let harness = Harness::start(batches, vec![]).await;
    for round in 0..ROUNDS {
        assert_eq!(
            harness.turn("read five files").await.0,
            TurnReason::Completed,
            "round {round}"
        );
        let (_, results) = exchange(&harness.history().await);
        let tail = &results[results.len() - names.len()..];
        assert_eq!(
            ids(tail),
            ["call-0", "call-1", "call-2", "call-3", "call-4"],
            "round {round} reordered its results"
        );
        assert_eq!(
            bodies(tail),
            [
                "fixture:0",
                "fixture:1",
                "fixture:2",
                "fixture:3",
                "fixture:4"
            ],
            "round {round} reordered its results"
        );
    }

    let recorded = recorded(&harness.store());
    assert_eq!(recorded.len(), ROUNDS * names.len());
    for (round, chunk) in recorded.chunks(names.len()).enumerate() {
        assert_eq!(
            tools(chunk),
            names,
            "round {round} reordered its trajectory"
        );
    }
}

// ─── the sequential fallbacks ────────────────────────────────────────────────

const GATED_STEP: Duration = Duration::from_millis(150);
const GATED_SUM: Duration = Duration::from_millis(450);

#[tokio::test]
async fn a_batch_containing_a_mutating_tool_stays_sequential() {
    let batch = vec![
        ask(0, "read_file", "a:150"),
        ask_edit(1, "b:150"),
        ask(2, "read_span", "c:150"),
    ];
    let harness = Harness::start(vec![batch, vec![]], vec![]).await;
    let (reason, elapsed) = harness.turn("read, write, read").await;
    assert_eq!(reason, TurnReason::Completed);
    assert!(
        elapsed >= GATED_SUM - GATED_STEP / 3,
        "a batch with a mutating call must not overlap: took {} ms, sequential is {} ms",
        elapsed.as_millis(),
        GATED_SUM.as_millis()
    );

    let (_, results) = exchange(&harness.history().await);
    assert_eq!(bodies(&results), ["fixture:a", "fixture:b", "fixture:c"]);
    assert_eq!(
        tools(&recorded(&harness.store())),
        ["read_file", "edit_file", "read_span"]
    );
}

#[tokio::test]
async fn a_batch_containing_a_confirmation_gated_tool_stays_sequential() {
    let batch = vec![
        ask(0, "read_file", "a:150"),
        ask(1, "search", "b:150"),
        ask(2, "read_span", "c:150"),
    ];
    // Every call is read-only; only the gate differs.
    let harness = Harness::start(vec![batch, vec![]], vec!["search"]).await;
    let (reason, elapsed) = harness.turn("read three files, one gated").await;
    assert_eq!(reason, TurnReason::Completed);
    assert!(
        elapsed >= GATED_SUM - GATED_STEP / 3,
        "a confirmation-gated call must not be raced against its neighbours: \
         took {} ms, sequential is {} ms",
        elapsed.as_millis(),
        GATED_SUM.as_millis()
    );

    let (_, results) = exchange(&harness.history().await);
    assert_eq!(bodies(&results), ["fixture:a", "fixture:b", "fixture:c"]);
}

// ─── the bound ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn concurrency_is_bounded() {
    let names = ["read_file", "read_span", "search", "list_files", "outline"];
    let width = MAX_CONCURRENT_READ_ONLY_CALLS * 3;
    let batch = (0..width)
        .map(|slot| ask(slot, names[slot % names.len()], &format!("{slot}:40")))
        .collect();

    let harness = Harness::start(vec![batch, vec![]], vec![]).await;
    assert_eq!(
        harness.turn("read everything").await.0,
        TurnReason::Completed
    );

    let peak = harness.state.peak.load(Ordering::SeqCst);
    assert!(
        peak <= MAX_CONCURRENT_READ_ONLY_CALLS,
        "{width} reads ran {peak} at once, above the {MAX_CONCURRENT_READ_ONLY_CALLS} bound"
    );
    assert_eq!(
        peak, MAX_CONCURRENT_READ_ONLY_CALLS,
        "{width} reads should saturate the bound, not trickle"
    );

    let (_, results) = exchange(&harness.history().await);
    let expected: Vec<String> = (0..width).map(|slot| format!("fixture:{slot}")).collect();
    assert_eq!(bodies(&results), expected);
}

// ─── cancellation ────────────────────────────────────────────────────────────

#[tokio::test]
async fn interrupting_a_concurrent_batch_leaves_nothing_running() {
    let batch = vec![
        ask(0, "read_file", "a:0"),
        ask(1, "read_span", "b:0"),
        ask(2, "search", "c:pending"),
        ask(3, "list_files", "d:pending"),
        ask(4, "outline", "e:pending"),
    ];
    let harness = Harness::start(vec![batch, vec![]], vec![]).await;
    harness.prompt("read five files").await;

    let sessions = harness.sessions.clone();
    let id = harness.id.clone();
    let turn = tokio::spawn(async move { run::drive_turn(&sessions, &id).await });

    // Wait for all five to have started and the two finite ones to have
    // finished, so the interrupt lands with a partially-complete batch.
    let state = harness.state.clone();
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state.entered.load(Ordering::SeqCst) == 5
                && state.in_flight.load(Ordering::SeqCst) == 3
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(settled.is_ok(), "the batch never reached three in flight");

    assert!(run::request_interrupt(&harness.sessions, &harness.id));
    assert_eq!(turn.await.unwrap().reason, TurnReason::Interrupted);

    assert_eq!(
        harness.state.in_flight.load(Ordering::SeqCst),
        0,
        "an interrupted batch must leave nothing running"
    );
    assert_eq!(
        harness.state.entered.load(Ordering::SeqCst),
        5,
        "cancellation must not restart anything"
    );

    let (uses, results) = exchange(&harness.history().await);
    assert_eq!(uses.len(), 5);
    assert_eq!(
        ids(&results),
        ["call-0", "call-1", "call-2", "call-3", "call-4"],
        "canonical history must pair every tool use even when interrupted"
    );
    assert_eq!(
        bodies(&results),
        [
            "fixture:a",
            "fixture:b",
            "Interrupted by user",
            "Interrupted by user",
            "Interrupted by user"
        ],
        "the batch must record what did and did not complete"
    );
    assert_eq!(
        statuses(&recorded(&harness.store())),
        ["ok", "ok", "interrupted", "interrupted", "interrupted"]
    );
}
