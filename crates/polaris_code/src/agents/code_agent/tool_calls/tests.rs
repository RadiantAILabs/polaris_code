//! Run the production tool graph with deterministic tools.
use super::*;
use polaris::graph::GraphExecutor;
use polaris::models::llm::{AssistantBlock, StopReason, Usage};
use polaris::plugins::{CONFIRMED, IOError, IOProvider};
use polaris::sessions::SessionId;
use polaris::tools::toolset;
use std::sync::Mutex;

#[derive(Default)]
struct Choose(Mutex<Vec<IOMessage>>);
impl IOProvider for Choose {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.0.lock().unwrap().push(message);
        Ok(())
    }
    async fn receive(&self) -> Result<IOMessage, IOError> {
        Ok(IOMessage::user_text("choose")
            .with_metadata(CONFIRMED, "true")
            .with_metadata("approval_choice", "exact"))
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
}
async fn run_calls(calls: Vec<ToolCall>, confirm: bool, bound: usize) -> Outcome {
    let root = tempfile::tempdir().unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let io = Arc::new(Choose::default());
    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(PermissionRulesPlugin::default());
    server.insert_global(OxigraphStore::in_memory().unwrap());
    server.insert_global(SecretRedactor::default());
    server.add_plugins(FixturePlugin(Fixtures(ran.clone())));
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
    ctx.insert(RunCancellation::never());
    ctx.insert(SessionInfo {
        session_id: SessionId::new(),
        turn_number: 0,
    });
    ctx.insert(SessionSettings::compatibility_default("test/model"));
    ctx.insert(BackgroundTasks::detached(Duration::from_secs(30)));
    ctx.insert(UserIO::new(io.clone()));
    ctx.insert(ToolBatch::default());
    ctx.insert(CallLifetime::default());
    ctx.insert(Script(calls));
    let mut graph = Graph::new();
    graph.add_system(seed_response).add_system(seed_reviews);
    build_bounded_tool_calls(&mut graph, bound);
    GraphExecutor::new()
        .with_default_max_iterations(2)
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("tool failures stay inside the batch");
    let messages = ctx
        .get_resource::<ContextManager>()
        .unwrap()
        .messages
        .to_vec();
    let ran = ran.lock().unwrap().clone();
    Outcome { messages, ran, io }
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
            .0
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
