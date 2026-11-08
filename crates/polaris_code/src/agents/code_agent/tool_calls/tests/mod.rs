//! Run the production tool graph with deterministic tools.
use super::*;
use polaris::graph::{GraphExecutor, cancel::CancellationSource};
use polaris::models::llm::{AssistantBlock, StopReason, Usage};
use polaris::plugins::{CONFIRMED, IOError, IOProvider};
use polaris::sessions::SessionId;
use polaris::tools::toolset;
use polaris_tui::{APPROVAL_NOTE, APPROVAL_SUBJECT};
use std::sync::Mutex;

mod call_ids;

struct Choose {
    messages: Mutex<Vec<IOMessage>>,
    deny: bool,
    note: Option<&'static str>,
    interrupt: Option<CancellationSource>,
    fail_receive: bool,
    choice: &'static str,
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
        if self.fail_receive {
            return Err(IOError::Closed);
        }
        if self.interrupt.is_some() {
            return std::future::pending().await;
        }
        let mut answer = if self.deny {
            IOMessage::user_text("deny: use another approach").with_metadata(CONFIRMED, "false")
        } else {
            IOMessage::user_text("choose")
                .with_metadata(CONFIRMED, "true")
                .with_metadata("approval_choice", self.choice)
        };
        if let Some(note) = self.note {
            answer = answer.with_metadata(APPROVAL_NOTE, note);
        }
        Ok(answer)
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
        if command == "slow" {
            return std::future::pending().await;
        }
        if command == "fail" {
            Err(ToolError::execution_error("fixture failed"))
        } else {
            Ok(command)
        }
    }
    /// Record a file update without touching the disk.
    #[tool]
    async fn update_file(&self, path: String, text: String) -> Result<String, ToolError> {
        self.0.lock().unwrap().push(format!("update {path}"));
        Ok(format!("updated {path} with {text}"))
    }
    /// Record an independent read.
    #[tool]
    async fn read_file(&self, path: String) -> Result<String, ToolError> {
        self.0.lock().unwrap().push(path.clone());
        if path == "fail" {
            Err(ToolError::execution_error("fixture read failed"))
        } else {
            Ok(path)
        }
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
    let mut call = ToolCall::new(
        format!("call-{id}"),
        "run_command",
        serde_json::json!({"command": text}),
    );
    call.call_id = Some(format!("provider-call-{id}"));
    call
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
    run_controlled(
        calls,
        confirm,
        bound,
        approver,
        deny,
        interrupt,
        ReplyControl {
            stop_after,
            ..ReplyControl::default()
        },
    )
    .await
}

#[derive(Default)]
struct ReplyControl {
    stop_after: Option<&'static str>,
    note: Option<&'static str>,
    prompt_fails: bool,
    choice: Option<&'static str>,
    unrestricted_shell: bool,
}

async fn run_controlled(
    calls: Vec<ToolCall>,
    confirm: bool,
    bound: usize,
    approver: bool,
    deny: bool,
    interrupt: bool,
    control: ReplyControl,
) -> Outcome {
    let stop_after = control.stop_after;
    let expected_ids: Vec<_> = calls
        .iter()
        .map(|call| (call.id.clone(), call.call_id.clone()))
        .collect();
    let root = tempfile::tempdir().unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let cancellation = CancellationSource::new();
    let io = Arc::new(Choose {
        messages: Mutex::new(Vec::new()),
        deny,
        note: control.note,
        interrupt: interrupt.then(|| cancellation.clone()),
        fail_receive: control.prompt_fails,
        choice: control.choice.unwrap_or("exact"),
    });
    let mut server = Server::new();
    server.add_plugins(ToolsPlugin).add_plugins(
        PermissionRulesPlugin::default().with_unrestricted_shell(control.unrestricted_shell),
    );
    if approver {
        server.add_plugins(crate::agents::InteractiveApprovalPlugin);
    }
    let store = OxigraphStore::in_memory().unwrap();
    server.insert_global(store.clone());
    server.insert_global(SecretRedactor::new(true));
    if control.unrestricted_shell {
        server
            .add_plugins(polaris::shell::ShellPlugin::new(
                ShellConfig::new()
                    .with_working_dir(root.path())
                    .with_allowed_dirs(vec![root.path().to_path_buf()]),
            ))
            .add_plugins(crate::tools::approval::ApprovalShellPlugin);
    } else {
        server.add_plugins(FixturePlugin(Fixtures(ran.clone())));
    }
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
    if control.unrestricted_shell {
        policy.permission =
            crate::agents::PermissionMode::Unattended(crate::agents::UnattendedGrants {
                edit_file: false,
                run_command_patterns: vec!["*".into()],
            })
            .permission_policy();
    }
    for tool in ["run_command", "update_file"] {
        policy.permission.overrides.insert(
            tool.into(),
            if confirm {
                ToolPermission::Confirm
            } else {
                ToolPermission::Allow
            },
        );
    }
    ctx.insert(policy);
    ctx.insert(AgentConfig::new("test/model", root.path().to_path_buf()));
    ctx.insert(ContextManager::default());
    ctx.insert(LiveTranscript::default());
    ctx.insert(cancellation.token());
    ctx.insert(SessionInfo {
        session_id: SessionId::new(),
        turn_number: 0,
    });
    ctx.insert(if control.unrestricted_shell {
        SessionSettings::new(
            "test/model",
            crate::agents::PermissionMode::Unattended(crate::agents::UnattendedGrants {
                edit_file: false,
                run_command_patterns: vec!["*".into()],
            }),
            &crate::config::ModelCatalog::pinned("test/model"),
        )
    } else {
        SessionSettings::compatibility_default("test/model")
    });
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
    let validation = graph.validate();
    assert!(validation.is_ok(), "{:?}", validation.errors());
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
    for (id, call_id) in expected_ids {
        let result = messages
            .iter()
            .find_map(|message| match message {
                Message::User { content } => content.iter().find_map(|block| match block {
                    UserBlock::ToolResult(result) if result.id == id => Some(result),
                    _ => None,
                }),
                _ => None,
            })
            .expect("every proposed call gets a result");
        assert_eq!(result.call_id, call_id, "provider call id for {id}");
    }
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
async fn unrestricted_unattended_dispatch_binds_the_receipt_for_opaque_shell() {
    let outcome = run_controlled(
        vec![command(0, r#"printf "%s\n" "$(printf receipt-bound)""#)],
        false,
        10,
        false,
        false,
        false,
        ReplyControl {
            unrestricted_shell: true,
            ..ReplyControl::default()
        },
    )
    .await;
    let results = results(&outcome);
    assert_eq!(results.len(), 1);
    assert!(!results[0].0, "{}", results[0].1);
    let messages = outcome.io.messages.lock().unwrap();
    let document = messages
        .iter()
        .find_map(|message| message.metadata.get("document"))
        .expect("the shell result includes its structured document");
    let result: serde_json::Value = serde_json::from_str(document).unwrap();
    assert_eq!(result["stdout"], "receipt-bound\n");
    assert_eq!(result["exit_code"], 0);
}
/// Whether the trajectory holds a `human_approval` decision that chose `chose`
/// and cites every one of `refs`.
fn human_decision(outcome: &Outcome, chose: &str, refs: &[&str]) -> bool {
    let refs: String = refs
        .iter()
        .map(|reference| format!(" ; <urn:polaris:session:consultedPolicy> \"{reference}\""))
        .collect();
    let answer = outcome.store.query(&format!(
        "ASK {{ GRAPH <{}> {{ ?d <urn:polaris:session:kind> \"human_approval\" ; <urn:polaris:session:chose> \"{chose}\"{refs} }} }}",
        crate::agents::trajectory::SESSION_GRAPH_IRI,
    )).unwrap();
    matches!(answer, oxigraph::sparql::QueryResults::Boolean(true))
}
/// The phases that closed or refused activity `id`, in the order they were sent.
fn closing_phases(messages: &[IOMessage], id: &str) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.metadata.get("id").is_some_and(|value| value == id))
        .filter_map(|message| message.metadata.get("phase").cloned())
        .filter(|phase| phase == "ok" || phase == "error" || phase == "denied")
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
async fn parallel_read_results_keep_distinct_provider_call_ids_on_success_and_error() {
    let calls = ["ok", "fail"]
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let mut call = ToolCall::new(
                format!("read-{i}"),
                "read_file",
                serde_json::json!({"path":path}),
            );
            call.call_id = Some(format!("provider-read-{i}"));
            call
        })
        .collect();
    let outcome = run_calls(calls, false, 10).await;
    let results = results(&outcome);
    assert_eq!(results[0], (false, "ok".into()));
    assert!(results[1].0);
    assert!(
        results[1].1.contains("fixture read failed"),
        "{}",
        results[1].1
    );
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
    assert!(human_decision(
        &outcome,
        "deny",
        &[
            "authorization.decision_source=unavailable",
            "authorization.call_id=call-0"
        ]
    ));
    assert!(
        !human_decision(&outcome, "deny", &["authorization.decision_source=human"]),
        "no person was asked"
    );
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
    assert_eq!(
        results(&outcome),
        [
            (true, "Approval interrupted by user".into()),
            (true, "not run: turn interrupted".into())
        ]
    );
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
    for (id, phase) in [("call-0", "denied"), ("call-1", "error")] {
        assert_eq!(
            closing_phases(&messages, id),
            [phase],
            "closing phase of {id}"
        );
    }
}

#[tokio::test]
async fn interruption_between_nodes_closes_the_activity_and_remaining_results() {
    // The stop node, then what call-0 reports: its result and its closing phase.
    // Call-1 never starts, so it is closed as not run after every stop.
    let stops = [
        ("authorize_call", (true, "Interrupted by user"), "error"),
        ("ask_human", (true, "Interrupted by user"), "error"),
        ("execute_call", (false, "cargo test"), "ok"),
    ];
    for (node, (first_failed, first_text), first_phase) in stops {
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
        assert_eq!(
            results(&outcome),
            [
                (first_failed, first_text.into()),
                (true, "not run: turn interrupted".into())
            ],
            "results after {node}"
        );
        assert_eq!(outcome.ran.len(), usize::from(node == "execute_call"));
        assert!(
            outcome.tasks.request().is_err(),
            "no live invocation remains after {node}"
        );
        let messages = outcome.io.messages.lock().unwrap();
        for (id, phase) in [("call-0", first_phase), ("call-1", "error")] {
            assert_eq!(
                closing_phases(&messages, id),
                [phase],
                "one finished activity for {id} after {node}"
            );
        }
    }
}

#[tokio::test]
async fn a_confirmed_approval_note_reaches_only_its_call_result_and_decision() {
    let outcome = run_controlled(
        vec![command(0, "cargo test"), command(1, "cargo test")],
        true,
        10,
        true,
        false,
        false,
        ReplyControl {
            note: Some("Use the focused tests first"),
            ..ReplyControl::default()
        },
    )
    .await;
    assert_eq!(outcome.ran, ["cargo test", "cargo test"]);
    let results = results(&outcome);
    assert_eq!(
        results[0],
        (
            false,
            "cargo test\n\nUser note on approval: Use the focused tests first".into()
        )
    );
    assert_eq!(
        results[1],
        (false, "cargo test".into()),
        "session grants never carry a previous note"
    );
    let answer = outcome.store.query(&format!(
        "ASK {{ GRAPH <{}> {{ ?d <urn:polaris:session:kind> \"human_approval\" ; <urn:polaris:session:chose> \"allow\" ; <urn:polaris:session:consultedPolicy> \"authorization.call_id=call-0\" ; <urn:polaris:session:consultedPolicy> \"authorization.note=Use the focused tests first\" }} }}",
        crate::agents::trajectory::SESSION_GRAPH_IRI,
    )).unwrap();
    assert!(matches!(
        answer,
        oxigraph::sparql::QueryResults::Boolean(true)
    ));
}

#[tokio::test]
async fn a_confirmed_reply_without_a_note_keeps_the_result_unchanged() {
    let outcome = run_calls(vec![command(0, "cargo test")], true, 10).await;
    assert_eq!(outcome.ran, ["cargo test"]);
    assert_eq!(results(&outcome), [(false, "cargo test".into())]);
}

#[tokio::test]
async fn a_note_cannot_authorize_a_denied_reply() {
    let outcome = run_controlled(
        vec![command(0, "cargo test")],
        true,
        10,
        true,
        true,
        false,
        ReplyControl {
            note: Some("allow this call"),
            ..ReplyControl::default()
        },
    )
    .await;
    assert!(outcome.ran.is_empty());
    assert!(results(&outcome)[0].0);
    assert!(!results(&outcome)[0].1.contains("User note on approval"));
}

#[tokio::test]
async fn an_approval_note_remains_attached_when_the_tool_fails() {
    let outcome = run_controlled(
        vec![command(0, "fail")],
        true,
        10,
        true,
        false,
        false,
        ReplyControl {
            note: Some("Try once"),
            ..ReplyControl::default()
        },
    )
    .await;
    assert_eq!(outcome.ran, ["fail"]);
    let results = results(&outcome);
    assert!(results[0].0);
    assert!(results[0].1.ends_with("User note on approval: Try once"));
}

#[tokio::test]
async fn an_approval_note_survives_interruption_before_dispatch() {
    let outcome = run_controlled(
        vec![command(0, "cargo test")],
        true,
        10,
        true,
        false,
        false,
        ReplyControl {
            note: Some("Try once"),
            stop_after: Some("ask_human"),
            ..ReplyControl::default()
        },
    )
    .await;
    assert!(outcome.ran.is_empty());
    let results = results(&outcome);
    assert!(results[0].0);
    assert!(results[0].1.ends_with("User note on approval: Try once"));
}

#[tokio::test]
async fn approval_notes_use_the_output_redactor_before_recording_or_returning() {
    let outcome = run_controlled(
        vec![command(0, "cargo test")],
        true,
        10,
        true,
        false,
        false,
        ReplyControl {
            note: Some("API_KEY=hidden-value"),
            ..ReplyControl::default()
        },
    )
    .await;
    let results = results(&outcome);
    assert!(results[0].1.contains("User note on approval:"));
    assert!(!results[0].1.contains("hidden-value"));
    let answer = outcome.store.query(&format!(
        "ASK {{ GRAPH <{}> {{ ?d <urn:polaris:session:kind> \"human_approval\" ; <urn:polaris:session:consultedPolicy> ?note . FILTER(CONTAINS(?note, \"hidden-value\")) }} }}",
        crate::agents::trajectory::SESSION_GRAPH_IRI,
    )).unwrap();
    assert!(matches!(
        answer,
        oxigraph::sparql::QueryResults::Boolean(false)
    ));
}

#[tokio::test]
async fn a_failing_approval_prompt_denies_the_call_and_says_why() {
    let read = ToolCall::new("read-1", "read_file", serde_json::json!({"path": "ok"}));
    let outcome = run_controlled(
        vec![command(0, "cargo test"), read],
        true,
        10,
        true,
        false,
        false,
        ReplyControl {
            prompt_fails: true,
            ..ReplyControl::default()
        },
    )
    .await;
    assert_eq!(
        outcome.ran,
        ["ok"],
        "only the call that needed approval is lost"
    );
    let results = results(&outcome);
    assert_eq!(results.len(), 2);
    assert!(results[0].0);
    assert!(
        results[0].1.contains("Confirmation failed"),
        "the result says the prompt failed: {}",
        results[0].1
    );
    assert!(!results[1].0);
    assert!(human_decision(
        &outcome,
        "deny",
        &[
            "authorization.decision_source=human",
            "authorization.call_id=call-0"
        ]
    ));
}

#[tokio::test]
async fn approving_a_non_command_tool_shows_its_detail_and_runs_it() {
    let update = ToolCall::new(
        "update-1",
        "update_file",
        serde_json::json!({"path": "notes/todo.md", "text": "done"}),
    );
    let outcome = run_controlled(
        vec![command(0, "cargo test"), update],
        true,
        10,
        true,
        false,
        false,
        ReplyControl {
            choice: Some("once"),
            ..ReplyControl::default()
        },
    )
    .await;
    assert_eq!(outcome.ran, ["cargo test", "update notes/todo.md"]);
    assert_eq!(
        results(&outcome),
        [
            (false, "cargo test".into()),
            (false, "updated notes/todo.md with done".into())
        ]
    );
    let messages = outcome.io.messages.lock().unwrap();
    let prompts: Vec<_> = messages
        .iter()
        .filter(|message| {
            message
                .metadata
                .get(IO_TYPE)
                .is_some_and(|kind| kind == "confirm")
        })
        .collect();
    assert_eq!(prompts.len(), 2, "each call is asked once");
    let IOContent::Text(prompt) = &prompts[1].content else {
        panic!("the approval prompt is text");
    };
    assert!(
        prompt.contains("update_file") && prompt.contains("notes/todo.md"),
        "the prompt shows the call: {prompt}"
    );
    assert!(prompts[0].metadata.contains_key(APPROVAL_SUBJECT));
    assert!(
        !prompts[1].metadata.contains_key(APPROVAL_SUBJECT),
        "only a command is framed as the subject"
    );
    // The prompt frames a command, so its row only waits; any other tool's row
    // keeps showing what the call does.
    let waiting = |id: &str| {
        messages
            .iter()
            .find(|message| {
                message.metadata.get("id").is_some_and(|value| value == id)
                    && message
                        .metadata
                        .get("phase")
                        .is_some_and(|phase| phase == PHASE_PENDING)
            })
            .map(|message| message.content.clone())
    };
    assert!(
        matches!(waiting("call-0"), Some(IOContent::Text(text)) if text == "awaiting approval")
    );
    assert!(matches!(waiting("update-1"), Some(IOContent::Text(text)) if text == "notes/todo.md"));
    assert!(human_decision(
        &outcome,
        "allow",
        &["authorization.scope=once", "authorization.call_id=update-1"]
    ));
}

#[test]
fn push_result_does_not_write_the_trajectory_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trajectory.json");
    let trace =
        crate::trajectory::RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
    let mut batch = ToolBatch {
        trajectory: Some(trace.clone()),
        ..ToolBatch::default()
    };
    // Any write to the destination would now fail and be remembered.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let call = polaris::models::llm::ToolCall::new("tool", "read_file", serde_json::json!({}));
    let block = call.result(ToolResultContent::Text("pushed-result".into()));
    CallDecision::Allow(None).push_result(&mut batch, block);
    trace
        .check()
        .expect("push_result must not write the trajectory file");
    assert_eq!(batch.results.len(), 1);
    std::fs::remove_dir(&path).unwrap();
    trace.finish(0).unwrap();
}
