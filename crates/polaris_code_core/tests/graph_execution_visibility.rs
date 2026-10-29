//! Proof that a user can watch *any* graph execute, not only the agent loop.
//!
//! The visible surface is the status-message vocabulary the `polaris_tui`
//! conversation panel already renders: `io_type=status` carrying `node` and
//! `seq` metadata. This file drives a real coding topology through
//! [`polaris_code_core::run::drive_turn`] on a server assembled the way
//! `main.rs` assembles it, and reads what reached [`UserIO`].
//!
//! The same assembly drives the negative cases: no `UserIO` at all, no
//! progress plugin at all, and a node a pack deliberately silenced.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use polaris_code_core::cosmos::host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, TopologyTurn,
};
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::task::{CodingIntent, CodingTask};
use polaris_code_core::cosmos::topology::CodingTopologyPackPlugin;
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::progress::{NodeActivity, ProgressMiddlewarePlugin};
use polaris_code_core::run;
use polaris_code_core::usage::TokenMeter;
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

const ADD_STUB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}";
const ADD_DONE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";

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

/// How many times any test in this file reached a model. A topology run must
/// leave this at zero however closely it is watched.
static MODEL_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Reads the model-call count.
fn model_calls() -> usize {
    MODEL_CALLS.load(Ordering::Relaxed)
}

/// A provider that would stream a fixed answer — installed so the server
/// assembles, and asserted *never* to be called on a topology turn.
struct ScriptedProvider;

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        MODEL_CALLS.fetch_add(1, Ordering::Relaxed);
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("unused")],
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
        use futures_util::StreamExt as _;
        MODEL_CALLS.fetch_add(1, Ordering::Relaxed);
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("unused".to_string()),
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

struct ScriptedModelPlugin;

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::scripted_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before ScriptedModelPlugin")
            .register_llm_provider(ScriptedProvider);
    }
}

/// A temporary crate plus the coding task that implements its `add` stub.
fn project() -> (tempfile::TempDir, CodingTask) {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(dir.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
    let task = CodingTask::new(
        "implement-add",
        dir.path(),
        "src/lib.rs",
        ADD_STUB,
        ADD_DONE,
        "todo!()",
        CodingIntent::Careful,
    )
    .expect("the coding task is well formed");
    (dir, task)
}

/// How a server is wired for one of these tests.
#[derive(Clone, Copy)]
struct Wiring {
    /// Install [`ExecIOPlugin`], i.e. is anyone watching.
    user_io: bool,
    /// Install [`ProgressMiddlewarePlugin`], i.e. is anything instrumented.
    progress: bool,
}

/// Builds the selecting server `main.rs` builds, under the given wiring.
async fn selecting_server(
    data_dir: &Path,
    wiring: Wiring,
) -> (
    Server,
    CodingTaskQueue,
    Arc<ExecIOProvider>,
    mpsc::Receiver<IOMessage>,
) {
    selecting_server_with(data_dir, wiring, |_| {}).await
}

/// As [`selecting_server`], with `extra` given the server before it finishes —
/// which is where a plugin contributed later would install itself.
async fn selecting_server_with(
    data_dir: &Path,
    wiring: Wiring,
    extra: impl FnOnce(&mut Server),
) -> (
    Server,
    CodingTaskQueue,
    Arc<ExecIOProvider>,
    mpsc::Receiver<IOMessage>,
) {
    let (provider, _input_tx, output_rx) = ExecIOProvider::new(256);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));
    if wiring.user_io {
        server.add_plugins(ExecIOPlugin::new(provider.clone()));
    }
    if wiring.progress {
        server.add_plugins(ProgressMiddlewarePlugin);
    }
    extra(&mut server);

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

    (server, queue, provider, output_rx)
}

/// Creates the one session every test drives, exactly as the front ends do.
fn open_session(server: &Server, workspace: &Path) -> SessionId {
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = SessionId::new();
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let config = AgentConfig::new("scripted/test-model", workspace.to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &agent_type,
            polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .expect("the session is created");
    session_id
}

/// One status message as the panel would read it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Status {
    node: String,
    seq: String,
    text: String,
}

/// Drains the output channel and keeps the status messages, in arrival order.
async fn drain_statuses(
    provider: &ExecIOProvider,
    output_rx: &mut mpsc::Receiver<IOMessage>,
) -> Vec<Status> {
    use polaris::plugins::IOProvider as _;
    provider.close().await;
    let mut statuses = Vec::new();
    while let Some(message) = output_rx.recv().await {
        if message.metadata.get("io_type").map(String::as_str) != Some("status") {
            continue;
        }
        let IOContent::Text(ref text) = message.content else {
            continue;
        };
        statuses.push(Status {
            node: message.metadata.get("node").cloned().unwrap_or_default(),
            seq: message.metadata.get("seq").cloned().unwrap_or_default(),
            text: text.clone(),
        });
    }
    statuses
}

#[tokio::test]
async fn a_topology_graph_reports_per_node_status() {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let wiring = Wiring {
        user_io: true,
        progress: true,
    };
    let (server, queue, provider, mut output_rx) = selecting_server(data_dir.path(), wiring).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    assert_eq!(outcome.reason, run::TurnReason::Completed);
    assert!(outcome.nodes_executed > 0, "the graph really ran");

    let statuses = drain_statuses(&provider, &mut output_rx).await;
    assert!(
        !statuses.is_empty(),
        "the topology's nodes were watchable: {statuses:?}"
    );

    // Nobody wrote a label for any of these: the node's own name is the label,
    // and the whole graph is legible without anyone having edited this crate.
    let seen: Vec<(&str, &str)> = statuses
        .iter()
        .map(|status| (status.node.as_str(), status.text.as_str()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("initialize_feedback", "Initialize feedback…"),
            ("predict_edit", "Predict edit…"),
            ("act_edit", "Act edit…"),
            ("verify_edit", "Verify edit…"),
            ("compare_edit", "Compare edit…"),
            ("accept_edit", "Accept edit…"),
            ("conclude_prediction", "Conclude prediction…"),
        ],
        "the prediction-error loop's nodes, in the order they ran"
    );

    // Every status carries what the panel buckets its node timeline on, and
    // the sequence distinguishes one node's turn from the next.
    let seqs: Vec<&str> = statuses.iter().map(|status| status.seq.as_str()).collect();
    assert_eq!(seqs, vec!["1", "2", "3", "4", "5", "6", "7"]);
}

#[tokio::test]
async fn with_no_user_io_the_same_graph_runs_identically_and_says_nothing() {
    let watched = run_one_topology_turn(Wiring {
        user_io: true,
        progress: true,
    })
    .await;
    let unwatched = run_one_topology_turn(Wiring {
        user_io: false,
        progress: true,
    })
    .await;

    // The middleware still fires — it is registered and passed — and finds no
    // `UserIO`. That is the whole of its behaviour: nothing sent, nothing
    // raised, nothing logged, and the run is the same run.
    assert!(unwatched.statuses.is_empty());
    assert_eq!(unwatched.reason, run::TurnReason::Completed);
    assert_eq!(unwatched.outcome, watched.outcome);
}

#[tokio::test]
async fn instrumentation_changes_nothing_about_the_run() {
    let instrumented = run_one_topology_turn(Wiring {
        user_io: true,
        progress: true,
    })
    .await;
    let bare = run_one_topology_turn(Wiring {
        user_io: true,
        progress: false,
    })
    .await;

    // Same nodes, same count, same terminal conclusion, same edit on disk,
    // same verifier observations in the same order — with and without anyone
    // watching. Only the status stream differs.
    assert_eq!(bare.outcome, instrumented.outcome);
    assert!(
        bare.statuses.is_empty(),
        "a host that installs no progress plugin publishes no instrumentation, \
         so the run is not narrated: {:?}",
        bare.statuses
    );
    assert_eq!(instrumented.statuses.len(), 7);
}

#[tokio::test]
async fn a_pack_describes_and_silences_its_own_nodes() {
    let described = run_one_topology_turn_with(
        Wiring {
            user_io: true,
            progress: true,
        },
        |server| {
            server.add_plugins(EditPackLabelsPlugin);
        },
    )
    .await;

    let by_node: Vec<(&str, &str)> = described
        .statuses
        .iter()
        .map(|status| (status.node.as_str(), status.text.as_str()))
        .collect();
    assert!(
        by_node.contains(&("predict_edit", "Predicting the edit…")),
        "a later plugin's own wording wins over the default: {by_node:?}"
    );
    assert!(
        !described
            .statuses
            .iter()
            .any(|status| status.node == "act_edit"),
        "a node the rule silences stays silent: {by_node:?}"
    );
    // Silencing describes the node, it does not skip it: the same nodes ran
    // and the edit still landed.
    assert_eq!(
        described.outcome,
        run_one_topology_turn(Wiring {
            user_io: true,
            progress: true,
        })
        .await
        .outcome
    );
}

#[tokio::test]
async fn a_topology_turn_calls_no_model_and_leaves_the_session_untouched() {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let wiring = Wiring {
        user_io: true,
        progress: true,
    };
    let (server, queue, provider, mut output_rx) = selecting_server(data_dir.path(), wiring).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());
    let meter = server
        .get_global::<TokenMeter>()
        .expect("the run surface's token meter");

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;
    let statuses = drain_statuses(&provider, &mut output_rx).await;

    assert_eq!(outcome.reason, run::TurnReason::Completed);
    assert!(!statuses.is_empty(), "the run was watched");
    assert_eq!(
        model_calls(),
        0,
        "watching a topology run must not make it talk to a model"
    );
    assert_eq!(
        outcome.turn_number, 0,
        "a topology turn does not advance the session's turn number"
    );
    // What the turn reported is the graph's own conclusion, not a model's.
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", outcome.topology);
    };
    let reported = outcome.final_text.as_deref().expect("the turn reported");
    assert!(
        reported
            == executed
                .product
                .answer()
                .expect("an executed topology has an answer"),
        "the turn reported the graph's own conclusion: {reported}"
    );
    assert_eq!(
        sessions
            .turn_history(&session_id, true)
            .expect("the session is live")
            .len(),
        0,
        "no transcript was written"
    );
    assert_eq!(meter.total_tokens(), 0, "the TokenMeter did not move");
    assert_eq!(meter.cache_read_tokens(), 0);
    assert_eq!(meter.cache_creation_tokens(), 0);
}

/// Everything one topology turn is judged by, other than what was narrated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnOutcome {
    registration_id: String,
    nodes_executed: usize,
    answer: String,
    observations: Vec<String>,
    file_after: String,
}

/// One turn's outcome plus the statuses it produced.
struct WatchedTurn {
    reason: run::TurnReason,
    outcome: TurnOutcome,
    statuses: Vec<Status>,
}

/// Accepts one coding task and drives exactly one turn under `wiring`.
async fn run_one_topology_turn(wiring: Wiring) -> WatchedTurn {
    run_one_topology_turn_with(wiring, |_| {}).await
}

/// As [`run_one_topology_turn`], with an extra plugin installed.
async fn run_one_topology_turn_with(
    wiring: Wiring,
    extra: impl FnOnce(&mut Server),
) -> WatchedTurn {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, mut output_rx) =
        selecting_server_with(data_dir.path(), wiring, extra).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let turn = run::drive_turn(sessions, &session_id).await;
    let statuses = drain_statuses(&provider, &mut output_rx).await;

    let Some(TopologyTurn::Executed(executed)) = turn.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", turn.topology);
    };
    let file_after = std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
        .expect("the edited file is readable");
    WatchedTurn {
        reason: turn.reason,
        outcome: TurnOutcome {
            registration_id: executed.registration_id.clone(),
            nodes_executed: executed.nodes_executed,
            answer: executed
                .product
                .answer()
                .expect("an executed topology has an answer"),
            observations: executed
                .observations
                .iter()
                .map(|observation| format!("{:?}", observation.status()))
                .collect(),
            file_after,
        },
        statuses,
    }
}

/// A plugin contributed after the fact, describing nodes it owns: one gets its
/// own wording, one is deliberately not reported at all.
struct EditPackLabelsPlugin;

#[plugin(id = "test::edit_pack_labels", version = "0.1.0")]
impl Plugin for EditPackLabelsPlugin {
    fn build(&self, activities: Extends<NodeActivity>) {
        activities.describe("predict_edit", "Predicting the edit…");
        activities.silence("act_edit");
    }
}
