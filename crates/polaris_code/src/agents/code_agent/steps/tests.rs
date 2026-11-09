use std::collections::VecDeque;
use std::sync::Mutex;

use futures_util::StreamExt;
use polaris::graph::GraphExecutor;
use polaris::graph::edge::Edge;
use polaris::models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest, LlmStream,
    StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Contract, PluginAccess, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::panels::GraphFeed;

use super::*;
use crate::agents::{AgentConfig, CodeAgentPlugin};
use crate::exec::{ExecIOPlugin, ExecIOProvider};
use crate::graph_view::{GraphViewPlugin, graph_model};
use crate::progress::ProgressMiddlewarePlugin;
use crate::run::{TurnOutcome, TurnReason, drive_turn};
use crate::tools::{CodeToolsConfig, CodeToolsPlugin};
use crate::usage::TokenMeter;

#[derive(Default)]
struct Probe {
    order: Mutex<Vec<&'static str>>,
    after_results: Mutex<Vec<String>>,
    finish_answers: Mutex<Vec<String>>,
}
impl polaris::system::resource::GlobalResource for Probe {}

#[system]
async fn observe_tools(probe: Res<Probe>, mut context: ResMut<ContextManager>) {
    probe.order.lock().unwrap().push("observe");
    probe
        .after_results
        .lock()
        .unwrap()
        .push(format!("{:?}", context.messages));
    context.push(Message::user("Observation: the tool round is complete."));
}

#[system]
async fn first_check(probe: Res<Probe>, mut gate: ResMut<FinishGate>, answer: Out<LlmResponse>) {
    probe.order.lock().unwrap().push("first");
    probe.finish_answers.lock().unwrap().push(answer.text());
    if answer.text() != "verified" {
        gate.hold("First check: please verify.");
    }
}

#[system]
async fn second_check(probe: Res<Probe>, mut gate: ResMut<FinishGate>, answer: Out<LlmResponse>) {
    probe.order.lock().unwrap().push("second");
    if answer.text() != "verified" {
        gate.hold("Second check: report the verification.");
    }
}

struct ObserverPlugin;
impl polaris::system::plugin::Plugin for ObserverPlugin {
    const ID: &'static str = "test::observer_steps";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        let mut steps = server.get_resource_mut::<CodeAgentSteps>().unwrap();
        steps
            .after_tools("example.observe", 0, |g| {
                g.add_system(observe_tools);
            })
            .unwrap();
        steps
            .before_finish("example.first", 0, |g| {
                g.add_system(first_check);
            })
            .unwrap();
    }
}

struct SecondPlugin;
impl polaris::system::plugin::Plugin for SecondPlugin {
    const ID: &'static str = "test::second_steps";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<CodeAgentSteps>()
            .unwrap()
            .before_finish("example.second", 0, |g| {
                g.add_system(second_check);
            })
            .unwrap();
    }
}

#[derive(Clone)]
enum Reply {
    Text(&'static str),
    Tools(u32),
}

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Vec<Message>>>,
}
struct Provider(Arc<Script>);
impl LlmProvider for Provider {
    fn name(&self) -> &'static str {
        "scripted"
    }
    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        panic!("the normal agent streams; recovery must not run");
    }
    async fn stream(&self, _: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        self.0.requests.lock().unwrap().push(request.messages);
        let reply = self
            .0
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted reply");
        let mut events = Vec::new();
        let stop_reason = match reply {
            Reply::Text(text) => {
                events.extend([
                    StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::Text,
                    },
                    StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::Text(text.into()),
                    },
                    StreamEvent::ContentBlockStop { index: 0 },
                ]);
                StopReason::EndTurn
            }
            Reply::Tools(count) => {
                for index in 0..count {
                    events.extend([
                        StreamEvent::ContentBlockStart {
                            index,
                            block: ContentBlockStartData::ToolCall {
                                id: format!("read-{index}"),
                                call_id: None,
                                name: "read_file".into(),
                            },
                        },
                        StreamEvent::ContentBlockDelta {
                            index,
                            delta: ContentBlockDelta::ToolCall {
                                arguments: r#"{"path":"result.txt"}"#.into(),
                            },
                        },
                        StreamEvent::ContentBlockStop { index },
                    ]);
                }
                StopReason::ToolUse
            }
        };
        events.push(StreamEvent::MessageStop {
            stop_reason,
            usage: Usage {
                input_tokens: Some(100),
                output_tokens: Some(10),
                total_tokens: Some(110),
                ..Default::default()
            },
            id: None,
            model: None,
        });
        Ok(futures_util::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}
struct ModelPlugin(Arc<Script>);
impl polaris::system::plugin::Plugin for ModelPlugin {
    const ID: &'static str = "test::steps_model";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(Provider(self.0.clone()));
    }
}

// The same contract the graph panel provides, without starting a terminal.
struct FeedPlugin(GraphFeed);
impl polaris::system::plugin::Plugin for FeedPlugin {
    const ID: &'static str = "test::steps_graph_feed";
    const VERSION: Version = Version::new(0, 1, 0);
    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<GraphFeed>(GraphFeed::CONTRACT_VERSION)
    }
    fn build(&self, server: &mut Server) {
        server.insert_global(self.0.clone());
    }
}

struct Outcome {
    turn: TurnOutcome,
    requests: Vec<Vec<Message>>,
    history: Vec<Message>,
    order: Vec<&'static str>,
    results: Vec<String>,
    answers: Vec<String>,
    shown: Vec<IOMessage>,
    graph_labels: String,
}

async fn run(replies: Vec<Reply>, max_turns: usize, plugins: u8) -> Outcome {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("result.txt"), "tool-result-marker").unwrap();
    let script = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        ..Default::default()
    });
    let feed = GraphFeed::new();
    let (io, input, mut output) = ExecIOProvider::new(256);
    let io = Arc::new(io);
    input
        .send(IOMessage::from_external(
            "test",
            IOContent::Text("Do the task.".into()),
        ))
        .await
        .unwrap();
    drop(input);
    let drain = tokio::spawn(async move {
        let mut messages = Vec::new();
        while let Some(message) = output.recv().await {
            messages.push(message);
        }
        messages
    });
    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server.insert_global(Probe::default());
    server
        .add_plugins(FeedPlugin(feed.clone()))
        .add_plugins(ModelsPlugin)
        .add_plugins(ModelPlugin(script.clone()))
        .add_plugins(ToolsPlugin)
        .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(root.path())))
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(io.clone()))
        .add_plugins(ProgressMiddlewarePlugin)
        .add_plugins(GraphViewPlugin::for_code_agent_steps());
    if plugins >= 1 {
        server.add_plugins(ObserverPlugin);
    }
    if plugins >= 2 {
        server.add_plugins(SecondPlugin);
    }
    server.finish().await.unwrap();
    let steps = server.get_global::<CodeAgentSteps>().unwrap();
    let agent = CodeAgent::with_steps(&steps);
    assert_eq!(agent.name(), CodeAgent::NAME);
    assert!(agent.to_graph().validate().is_ok());
    let graph_labels = format!("{:?}", feed.snapshot().model().unwrap());
    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(agent).unwrap();
    let id = SessionId::new();
    let config = AgentConfig::new("scripted/test-model", root.path());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            GraphExecutor::new().with_default_max_iterations(max_turns),
            move |ctx| {
                ctx.insert(config);
            },
        )
        .unwrap();
    let turn = drive_turn(sessions, &id).await;
    let history = sessions
        .with_context(&id, |ctx| {
            ctx.get_resource::<ContextManager>()
                .unwrap()
                .messages
                .clone()
        })
        .await
        .unwrap();
    io.close().await;
    let shown = drain.await.unwrap();
    let probe = server.get_global::<Probe>().unwrap();
    let order = probe.order.lock().unwrap().clone();
    let results = probe.after_results.lock().unwrap().clone();
    let answers = probe.finish_answers.lock().unwrap().clone();
    let requests = script.requests.lock().unwrap().clone();
    Outcome {
        turn,
        requests,
        history,
        order,
        results,
        answers,
        shown,
        graph_labels,
    }
}

fn edge_shape(graph: &Graph) -> Vec<(String, Vec<usize>)> {
    let index = |id: &polaris::graph::NodeId| {
        graph
            .nodes()
            .iter()
            .position(|node| node.id() == *id)
            .unwrap()
    };
    graph
        .edges()
        .iter()
        .map(|edge| {
            let mut endpoints = vec![index(&edge.from())];
            let kind = match edge {
                Edge::Sequential(e) => {
                    endpoints.push(index(&e.to));
                    "sequential"
                }
                Edge::Conditional(e) => {
                    endpoints.extend([index(&e.true_target), index(&e.false_target)]);
                    "conditional"
                }
                Edge::Parallel(e) => {
                    endpoints.extend(e.targets.iter().map(index));
                    "parallel"
                }
                Edge::LoopBack(e) => {
                    endpoints.push(index(&e.to));
                    "loop"
                }
                Edge::Error(e) => {
                    endpoints.push(index(&e.to));
                    "error"
                }
                Edge::Timeout(e) => {
                    endpoints.push(index(&e.to));
                    "timeout"
                }
            };
            (kind.to_string(), endpoints)
        })
        .collect()
}

#[test]
fn empty_registry_preserves_names_edges_and_resource_signature() {
    let stock = CodeAgent.to_graph();
    let extended = CodeAgent::with_steps(&CodeAgentSteps::default()).to_graph();
    assert_eq!(
        stock.nodes().iter().map(|n| n.name()).collect::<Vec<_>>(),
        extended
            .nodes()
            .iter()
            .map(|n| n.name())
            .collect::<Vec<_>>()
    );
    assert_eq!(edge_shape(&stock), edge_shape(&extended));
    assert_eq!(stock.signature(), extended.signature());
    assert_eq!(
        graph_model(CodeAgent::NAME, &stock).node_count(),
        graph_model(CodeAgent::NAME, &extended).node_count()
    );
}

#[test]
fn duplicate_keys_are_rejected_without_replacing_the_builder() {
    let mut steps = CodeAgentSteps::default();
    steps.after_tools("same", 0, |_| {}).unwrap();
    assert!(steps.after_tools("same", 1, |_| {}).is_err());
    steps.before_finish("same", 0, |_| {}).unwrap();
    assert_eq!(steps.after_tools.len(), 1);
}

#[tokio::test]
async fn no_added_steps_runs_the_stock_answer_path() {
    let outcome = run(vec![Reply::Text("stock answer")], 3, 0).await;
    assert_eq!(outcome.turn.reason, TurnReason::Completed);
    assert_eq!(outcome.turn.final_text.as_deref(), Some("stock answer"));
    assert_eq!(outcome.requests.len(), 1);
    assert!(outcome.order.is_empty());
}

#[tokio::test]
async fn after_tools_runs_once_per_round_and_next_request_reads_its_message() {
    let outcome = run(
        vec![Reply::Tools(2), Reply::Tools(1), Reply::Text("verified")],
        5,
        1,
    )
    .await;
    assert_eq!(outcome.turn.reason, TurnReason::Completed);
    assert_eq!(outcome.order, ["observe", "observe", "first"]);
    assert_eq!(outcome.results.len(), 2);
    assert_eq!(outcome.results[0].matches("tool-result-marker").count(), 2);
    for result in &outcome.results {
        assert!(result.contains("tool-result-marker"), "{result}");
    }
    let sent = format!("{:?}", outcome.requests[1]);
    assert!(sent.contains("tool-result-marker"));
    assert!(sent.contains("Observation: the tool round is complete."));
    assert_eq!(outcome.answers, ["verified"]);
}

#[tokio::test]
async fn hold_retains_answer_then_messages_and_acceptance_finishes() {
    let outcome = run(
        vec![Reply::Text("candidate"), Reply::Text("verified")],
        5,
        1,
    )
    .await;
    assert_eq!(outcome.turn.reason, TurnReason::Completed);
    assert_eq!(outcome.turn.final_text.as_deref(), Some("verified"));
    assert_eq!(outcome.answers, ["candidate", "verified"]);
    let sent = &outcome.requests[1];
    assert!(matches!(&sent[sent.len() - 2], Message::Assistant { .. }));
    assert!(format!("{:?}", sent[sent.len() - 2]).contains("candidate"));
    assert!(format!("{:?}", sent[sent.len() - 1]).contains("First check: please verify."));
    assert!(format!("{:?}", outcome.history).contains("candidate"));
}

#[tokio::test]
async fn several_plugins_run_by_key_and_keep_every_hold_message() {
    let outcome = run(
        vec![Reply::Text("candidate"), Reply::Text("verified")],
        5,
        2,
    )
    .await;
    assert_eq!(outcome.turn.reason, TurnReason::Completed);
    assert_eq!(outcome.order, ["first", "second", "first", "second"]);
    let messages = &outcome.requests[1];
    assert!(matches!(
        &messages[messages.len() - 3],
        Message::Assistant { .. }
    ));
    assert!(matches!(
        &messages[messages.len() - 2],
        Message::User { .. }
    ));
    assert!(matches!(
        &messages[messages.len() - 1],
        Message::User { .. }
    ));
    let sent = format!("{messages:?}");
    assert!(sent.find("candidate").unwrap() < sent.find("First check").unwrap());
    assert!(sent.find("First check").unwrap() < sent.find("Second check").unwrap());
}

#[tokio::test]
async fn always_held_answers_still_end_at_the_turn_limit() {
    let outcome = run(vec![Reply::Text("candidate"); 3], 3, 1).await;
    let TurnReason::Failed(failure) = &outcome.turn.reason else {
        panic!("expected the iteration limit: {:?}", outcome.turn.reason);
    };
    assert!(
        failure.message.contains("max iterations (3) exceeded"),
        "{failure:?}"
    );
    assert_eq!(outcome.requests.len(), 3);
    assert_eq!(outcome.answers.len(), 3);
}

#[tokio::test]
async fn contributed_nodes_reach_the_graph_tab_and_execution_trace() {
    let outcome = run(vec![Reply::Tools(1), Reply::Text("verified")], 4, 2).await;
    let model = &outcome.graph_labels;
    for name in ["observe_tools", "first_check", "second_check"] {
        assert!(model.contains(name), "{model}");
        assert!(
            outcome.shown.iter().any(|message| message
                .metadata
                .get("node")
                .is_some_and(|node| node.contains(name))),
            "missing trace for {name}"
        );
    }
}

#[test]
fn priorities_precede_keys_and_agent_snapshots_do_not_change() {
    let mut steps = CodeAgentSteps::default();
    let built = Arc::new(Mutex::new(Vec::new()));
    for (key, priority) in [("z", 0), ("a", 1), ("b", 0)] {
        let built = built.clone();
        steps
            .after_tools(key, priority, move |_| {
                built.lock().unwrap().push(key);
            })
            .unwrap();
    }
    let agent = CodeAgent::with_steps(&steps);
    steps
        .after_tools("later", -1, |_| {
            panic!("snapshot changed");
        })
        .unwrap();
    let _ = agent.to_graph();
    assert_eq!(*built.lock().unwrap(), ["b", "z", "a"]);
}


#[test]
fn session_initializers_are_ordered_and_snapshots_keep_their_configuration() {
    let mut steps = CodeAgentSteps::default();
    steps.configure_session("b", |ctx| {
        ctx.get_resource_mut::<AgentConfig>().map_err(SetupError::new)?
            .prompt_sections.push("second".into());
        Ok(())
    }).unwrap();
    steps.configure_session("a", |ctx| {
        ctx.get_resource_mut::<AgentConfig>().map_err(SetupError::new)?
            .prompt_sections.push("first".into());
        Ok(())
    }).unwrap();
    assert!(steps.configure_session("a", |_| Ok(())).is_err());
    let agent = CodeAgent::with_steps(&steps);
    steps.configure_session("c", |_| panic!("snapshot must be fixed")).unwrap();
    let mut ctx = SystemContext::new();
    ctx.insert(AgentConfig::new("scripted/model", "."));
    for configure in agent.steps.setup.values() { configure(&mut ctx).unwrap(); }
    assert_eq!(ctx.get_resource::<AgentConfig>().unwrap().prompt_sections, ["first", "second"]);
}
