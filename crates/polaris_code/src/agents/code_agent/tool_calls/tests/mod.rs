//! Run the production tool graph with deterministic tools.
use super::*;
use polaris::graph::{GraphExecutor, cancel::CancellationSource};
use polaris::models::llm::{AssistantBlock, StopReason, Usage};
use polaris::plugins::{CONFIRMED, IOError, IOProvider};
use polaris::sessions::SessionId;
use polaris::tools::toolset;
use std::sync::Mutex;

struct Choose {
    messages: Mutex<Vec<IOMessage>>,
    deny: bool,
    interrupt: Option<CancellationSource>,
}
impl IOProvider for Choose {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        if message
            .metadata
            .get(IO_TYPE)
            .is_some_and(|kind| kind == "confirm")
            && let Some(cancel) = &self.interrupt
        {
            cancel.cancel();
        }
        self.messages.lock().unwrap().push(message);
        Ok(())
    }
    async fn receive(&self) -> Result<IOMessage, IOError> {
        if self.interrupt.is_some() {
            return std::future::pending().await;
        }
        if self.deny {
            Ok(
                IOMessage::user_text("deny: use another approach")
                    .with_metadata(CONFIRMED, "false"),
            )
        } else {
            Ok(IOMessage::user_text("choose")
                .with_metadata(CONFIRMED, "true")
                .with_metadata("approval_choice", "exact"))
        }
    }
}
#[derive(Clone)]
struct Fixtures(Arc<Mutex<Vec<String>>>);
#[toolset]
impl Fixtures {
    /// Record a command without starting a shell.
    #[tool]
    async fn run_command(&self, command: String) -> Result<String, ToolError> {
        self.0.lock().unwrap().push(command.clone());
        if command == "fail" {
            Err(ToolError::execution_error("fixture failed"))
        } else {
            Ok(command)
        }
    }
    /// Record an independent read.
    #[tool]
    async fn read_file(&self, path: String) -> Result<String, ToolError> {
        self.0.lock().unwrap().push(path.clone());
        Ok(path)
    }
}
struct FixturePlugin(Fixtures);
impl Plugin for FixturePlugin {
    const ID: &'static str = "test::tool_calls";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }
    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ToolRegistry>()
            .unwrap()
            .register_toolset(self.0.clone());
    }
}

struct Script(Vec<ToolCall>);
impl LocalResource for Script {}
#[system]
async fn seed_response(script: Res<Script>) -> LlmResponse {
    LlmResponse {
        content: script
            .0
            .iter()
            .cloned()
            .map(AssistantBlock::ToolCall)
            .collect(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        id: None,
        model: None,
    }
}
#[system]
async fn seed_reviews() -> ReviewResult {
    ReviewResult::default()
}
fn command(id: usize, text: &str) -> ToolCall {
    ToolCall::new(
        format!("call-{id}"),
        "run_command",
        serde_json::json!({"command": text}),
    )
}
struct Outcome {
    messages: Vec<Message>,
    ran: Vec<String>,
    io: Arc<Choose>,
    store: OxigraphStore,
    tasks: BackgroundTasks,
}
async fn run_calls(calls: Vec<ToolCall>, confirm: bool, bound: usize) -> Outcome {
    run_with_approver(calls, confirm, bound, true).await
}

async fn run_with_approver(
    calls: Vec<ToolCall>,
    confirm: bool,
    bound: usize,
    approver: bool,
) -> Outcome {
    run_with_answer(calls, confirm, bound, approver, false).await
}

async fn run_with_answer(
    calls: Vec<ToolCall>,
    confirm: bool,
    bound: usize,
    approver: bool,
    deny: bool,
) -> Outcome {
    run_fixture(calls, confirm, bound, approver, deny, false).await
}

async fn run_fixture(
    calls: Vec<ToolCall>,
    confirm: bool,
    bound: usize,
    approver: bool,
    deny: bool,
    interrupt: bool,
) -> Outcome {
    run_with_stop(calls, confirm, bound, approver, deny, interrupt, None).await
}

async fn run_with_stop(
    calls: Vec<ToolCall>,
    confirm: bool,
    bound: usize,
    approver: bool,
    deny: bool,
    interrupt: bool,
    stop_after: Option<&'static str>,
) -> Outcome {
    let root = tempfile::tempdir().unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let cancellation = CancellationSource::new();
    let io = Arc::new(Choose {
        messages: Mutex::new(Vec::new()),
        deny,
        interrupt: interrupt.then(|| cancellation.clone()),
    });
    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(PermissionRulesPlugin::default());
    if approver {
        server.add_plugins(crate::agents::InteractiveApprovalPlugin);
    }
    let store = OxigraphStore::in_memory().unwrap();
    server.insert_global(store.clone());
    server.insert_global(SecretRedactor::default());
    server.add_plugins(FixturePlugin(Fixtures(ran.clone())));
    install_cleanup(&mut server);
    if let Some(stop_after) = stop_after {
        let cancel = cancellation.clone();
        server
            .api::<polaris::graph::MiddlewareAPI>()
            .unwrap()
            .register_system("test::stop_between_nodes", move |info, ctx, next| {
                let cancel = cancel.clone();
                Box::pin(async move {
                    let result = next.run(ctx).await;
                    if info.node_name == stop_after {
                        cancel.cancel();
                    }
                    result
                })
            });
    }
    server.finish().await.unwrap();
    let mut ctx = server.create_context();
    let mut policy = PolicySnapshot::defaults();
    policy.permission.overrides.insert(
        "run_command".into(),
        if confirm {
            ToolPermission::Confirm
        } else {
            ToolPermission::Allow
        },
    );
    ctx.insert(policy);
    ctx.insert(AgentConfig::new("test/model", root.path().to_path_buf()));
    ctx.insert(ContextManager::default());
    ctx.insert(LiveTranscript::default());
    ctx.insert(cancellation.token());
    ctx.insert(SessionInfo {
        session_id: SessionId::new(),
        turn_number: 0,
    });
    ctx.insert(SessionSettings::compatibility_default("test/model"));
    let tasks = BackgroundTasks::detached(Duration::from_secs(30));
    ctx.insert(tasks.clone());
    ctx.insert(UserIO::new(io.clone()));
    setup_cleanup(&mut ctx);
    ctx.insert(ToolBatch::default());
    ctx.insert(CallLifetime::default());
    ctx.insert(Script(calls));
    let mut graph = Graph::new();
    graph.add_system(seed_response).add_system(seed_reviews);
    build_bounded_tool_calls(&mut graph, bound);
    let result = GraphExecutor::new()
        .with_default_max_iterations(2)
        .execute(
            &graph,
            &mut ctx,
            server.api::<polaris::graph::hooks::HooksAPI>(),
            server.api::<polaris::graph::MiddlewareAPI>(),
        )
        .await;
    if interrupt || stop_after.is_some() {
        assert!(result.is_err(), "the executor observes the interruption");
    } else {
        result.expect("tool failures stay inside the batch");
    }
    let messages = ctx
        .get_resource::<ContextManager>()
        .unwrap()
        .messages
        .to_vec();
    let ran = ran.lock().unwrap().clone();
    Outcome {
        messages,
        ran,
        io,
        store,
        tasks,
    }
}
fn results(outcome: &Outcome) -> Vec<(bool, String)> {
    outcome
        .messages
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content
                .iter()
                .filter_map(|block| match block {
                    UserBlock::ToolResult(result) => {
                        let ToolResultContent::Text(text) = &result.content else {
                            return None;
                        };
                        Some((
                            result.status == polaris::models::llm::ToolResultStatus::Error,
                            text.clone(),
                        ))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}
#[tokio::test]
async fn the_batch_bound_returns_an_error_for_every_unrun_call() {
    let outcome = run_calls((0..5).map(|i| command(i, "ok")).collect(), false, 2).await;
    assert_eq!(outcome.ran.len(), 2);
    let results = results(&outcome);
    assert_eq!(results.len(), 5);
    assert!(results[..2].iter().all(|(error, _)| !error));
    assert!(
        results[2..]
            .iter()
            .all(|(error, text)| *error && text == "not run: batch limit reached")
    );
}
#[tokio::test]
async fn a_tool_failure_keeps_the_remaining_calls_running() {
    let outcome = run_calls(vec![command(0, "fail"), command(1, "ok")], false, 10).await;
    assert_eq!(outcome.ran, ["fail", "ok"]);
    let results = results(&outcome);
    assert!(results[0].0);
    assert!(!results[1].0);
}
#[tokio::test]
async fn the_outer_turn_bound_does_not_truncate_a_batch() {
    let outcome = run_calls((0..8).map(|i| command(i, "ok")).collect(), false, 10).await;
    assert_eq!(outcome.ran.len(), 8);
    assert_eq!(results(&outcome).len(), 8);
}
#[tokio::test]
async fn an_exact_session_approval_applies_to_the_next_call() {
    let outcome = run_calls(
        vec![command(0, "cargo test"), command(1, "cargo test")],
        true,
        10,
    )
    .await;
    assert_eq!(outcome.ran.len(), 2);
    assert_eq!(
        outcome
            .io
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message
                .metadata
                .get(IO_TYPE)
                .is_some_and(|kind| kind == "confirm"))
            .count(),
        1
    );
}

#[tokio::test]
async fn an_absent_approver_denies_only_the_call_that_needs_approval() {
    let read = ToolCall::new("read-1", "read_file", serde_json::json!({"path": "ok"}));
    let outcome = run_with_approver(vec![command(0, "cargo test"), read], true, 10, false).await;
    assert_eq!(outcome.ran, ["ok"]);
    let results = results(&outcome);
    assert_eq!(results.len(), 2);
    assert!(results[0].0);
    assert!(results[0].1.contains("no approver is registered"));
    assert!(!results[1].0);
    assert!(!outcome.io.messages.lock().unwrap().iter().any(|message| {
        message
            .metadata
            .get(IO_TYPE)
            .is_some_and(|kind| kind == "confirm")
    }));
}

#[tokio::test]
async fn allowed_calls_do_not_need_an_approver() {
    let outcome = run_with_approver(vec![command(0, "ok")], false, 10, false).await;
    assert_eq!(outcome.ran, ["ok"]);
    assert!(!results(&outcome)[0].0);
}

#[tokio::test]
async fn human_approval_records_the_scope_and_call_identity() {
    let outcome = run_calls(vec![command(0, "cargo test")], true, 10).await;
    let answer = outcome.store.query(&format!(
        "ASK {{ GRAPH <{}> {{ ?d <urn:polaris:session:kind> \"human_approval\" ; <urn:polaris:session:chose> \"allow\" ; <urn:polaris:session:consultedPolicy> \"authorization.scope=exact\" ; <urn:polaris:session:consultedPolicy> \"authorization.call_id=call-0\" }} }}",
        crate::agents::trajectory::SESSION_GRAPH_IRI,
    )).unwrap();
    assert!(matches!(
        answer,
        oxigraph::sparql::QueryResults::Boolean(true)
    ));
}

#[tokio::test]
async fn human_denial_records_the_reason_and_keeps_the_batch_running() {
    let read = ToolCall::new("read-1", "read_file", serde_json::json!({"path": "ok"}));
    let outcome = run_with_answer(vec![command(0, "cargo test"), read], true, 10, true, true).await;
    assert_eq!(outcome.ran, ["ok"]);
    assert!(results(&outcome)[0].1.contains("use another approach"));
    let answer = outcome.store.query(&format!(
        "ASK {{ GRAPH <{}> {{ ?d <urn:polaris:session:kind> \"human_approval\" ; <urn:polaris:session:chose> \"deny\" ; <urn:polaris:session:consultedPolicy> ?reason . FILTER(CONTAINS(?reason, \"use another approach\")) }} }}",
        crate::agents::trajectory::SESSION_GRAPH_IRI,
    )).unwrap();
    assert!(matches!(
        answer,
        oxigraph::sparql::QueryResults::Boolean(true)
    ));
}

#[tokio::test]
async fn interrupted_approval_resolves_the_prompt_and_closes_every_call() {
    let outcome = run_fixture(
        vec![command(0, "cargo test"), command(1, "other")],
        true,
        10,
        true,
        false,
        true,
    )
    .await;
    assert!(outcome.ran.is_empty());
    assert_eq!(results(&outcome).len(), 2);
    let messages = outcome.io.messages.lock().unwrap();
    let request = messages
        .iter()
        .find(|message| {
            message
                .metadata
                .get(IO_TYPE)
                .is_some_and(|kind| kind == "confirm")
        })
        .unwrap();
    let resolution = messages
        .iter()
        .find(|message| {
            message
                .metadata
                .get(IO_TYPE)
                .is_some_and(|kind| kind == "approval_resolved")
        })
        .unwrap();
    assert_eq!(
        request.metadata.get("approval_request_id"),
        resolution.metadata.get("approval_request_id")
    );
    for id in ["call-0", "call-1"] {
        assert_eq!(
            messages
                .iter()
                .filter(
                    |message| message.metadata.get("id").is_some_and(|value| value == id)
                        && message
                            .metadata
                            .get("phase")
                            .is_some_and(|phase| phase == "denied" || phase == "error")
                )
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn interruption_between_nodes_closes_the_activity_and_remaining_results() {
    for node in ["authorize_call", "ask_human", "execute_call"] {
        let outcome = run_with_stop(
            vec![command(0, "cargo test"), command(1, "other")],
            true,
            10,
            true,
            false,
            false,
            Some(node),
        )
        .await;
        assert_eq!(results(&outcome).len(), 2, "stopped after {node}");
        assert_eq!(outcome.ran.len(), usize::from(node == "execute_call"));
        assert!(
            outcome.tasks.request().is_err(),
            "no live invocation remains after {node}"
        );
        let messages = outcome.io.messages.lock().unwrap();
        for id in ["call-0", "call-1"] {
            assert_eq!(
                messages
                    .iter()
                    .filter(
                        |message| message.metadata.get("id").is_some_and(|value| value == id)
                            && message
                                .metadata
                                .get("phase")
                                .is_some_and(|phase| phase == "ok" || phase == "error")
                    )
                    .count(),
                1,
                "one finished activity for {id} after {node}"
            );
        }
    }
}
