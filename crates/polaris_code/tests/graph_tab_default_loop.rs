//! Proof that the Graph tab follows the agent's own loop through a real session.
//!
//! A session builds its own copy of the agent's graph, so its node ids are not
//! those of the model published at start-up; the tab matches by name instead.
//! This drives one ordinary conversational turn through a real `Server` and
//! session and reads which nodes of the published model the tab marked as run.

use std::path::Path;
use std::sync::Arc;

use polaris::agent::Agent as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider as _, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use polaris_code::cosmos::host::CodingTaskQueue;
use polaris_code::cosmos::host::CodingTaskQueuePlugin;
use polaris_code::cosmos::policy;
use polaris_code::cosmos::topology::CodingTopologyPackPlugin;
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::graph_view::graph_model;
use polaris_code::progress::ProgressMiddlewarePlugin;
use polaris_code::run;
use polaris_code::usage::TokenMeter;
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::panels::{GraphFeed, GraphModel, GraphNode};

/// What the scripted model answers.
const ANSWER: &str = "nothing to change here.";

/// A provider that streams one fixed answer, with no key and no network.
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

/// Every node of `steps` and everything nested in it, in drawing order.
fn nodes_of<'a>(steps: &'a [GraphNode], out: &mut Vec<&'a GraphNode>) {
    for node in steps {
        out.push(node);
        for branch in node.branches() {
            nodes_of(branch.steps(), out);
        }
    }
}

/// The nodes of `model` called `label`, in drawing order.
fn called<'a>(model: &'a GraphModel, label: &str) -> Vec<&'a GraphNode> {
    let mut all = Vec::new();
    nodes_of(model.steps(), &mut all);
    for branch in model.side_branches() {
        nodes_of(branch.steps(), &mut all);
    }
    all.into_iter()
        .filter(|node| node.label() == label)
        .collect()
}

#[tokio::test]
async fn a_conversational_turn_marks_the_published_nodes_it_ran() {
    let feed = GraphFeed::new();
    feed.publish(graph_model(CodeAgent::NAME, &CodeAgent.to_graph()));

    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let project_dir = tempfile::tempdir().expect("temp project");
    let data_dir = tempfile::tempdir().expect("temp data dir");

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server.insert_global(feed.clone());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.path().to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ProgressMiddlewarePlugin)
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(CodingTaskQueue::new()));
    server.finish().await.expect("the server assembles");

    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");
    let session_id = SessionId::new();
    let config = AgentConfig::new("scripted/test-model", Path::to_path_buf(project_dir.path()));
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
    assert_eq!(outcome.reason, run::TurnReason::Completed);
    assert_eq!(outcome.final_text.as_deref(), Some(ANSWER));

    let snapshot = feed.snapshot();
    let model = snapshot.model().expect("the tab has a graph");
    assert_eq!(model.name(), CodeAgent::NAME, "no topology was announced");
    assert_eq!(snapshot.run(), 1);
    assert!(snapshot.running().is_empty(), "the turn is over");

    // The turn read a message, thought once, answered and finished.
    for label in [
        "receive_user_input",
        "init_loop",
        "agent_loop",
        "manual_compaction",
        "prepare_context",
        "act",
        "model_answered",
        "has_tool_calls",
        "finalize",
    ] {
        let nodes = called(model, label);
        assert!(!nodes.is_empty(), "the model has a node called {label}");
        assert!(
            nodes
                .iter()
                .any(|node| snapshot.finished(node.key()).is_some()),
            "{label} ran, so some node of that name is marked: the session's ids \
             are not the published model's, and the tab matched by name"
        );
    }

    // `act` is two nodes: the first attempt and the retry. Only the first ran.
    let acts = called(model, "act");
    assert_eq!(acts.len(), 2);
    assert_eq!(
        snapshot.finished(acts[0].key()).map(|run| run.count),
        Some(1)
    );
    assert_eq!(
        snapshot.finished(acts[1].key()),
        None,
        "the retry did not run, and must not be marked because it shares a name"
    );

    // Paths this turn did not take stay unmarked.
    for label in [
        "execute_tools",
        "wait_to_retry",
        "recover",
        "report_failure",
    ] {
        for node in called(model, label) {
            assert_eq!(snapshot.finished(node.key()), None, "{label} did not run");
        }
    }
}
