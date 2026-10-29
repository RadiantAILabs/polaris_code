//! Real-session proof of canonical transcript paging and read-only replay.
use std::sync::Arc;

use polaris::agent::Agent;
use polaris::graph::Graph;
use polaris::models::llm::{AssistantBlock, Message, ToolCall, ToolResultContent, UserBlock};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, IOProvider};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI};
use polaris::system::param::SystemContext;
use polaris::system::system;
use polaris_code::agents::ContextManager;
use polaris_code::terminal::history::CanonicalHistory;
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::conversation::TranscriptHistory;
use polaris_tui::panels::{ConversationPanel, TuiIOProvider};
use polaris_tui::ratatui::{Terminal, backend::TestBackend};
use polaris_tui::{TuiEvent, TuiPanel};

#[system]
async fn must_not_run() {
    panic!("history browsing executed the agent");
}
struct HistoryAgent;
impl Agent for HistoryAgent {
    fn name(&self) -> &'static str {
        "history-proof"
    }
    fn build(&self, graph: &mut Graph) {
        graph.add_system(must_not_run);
    }
}
fn session(messages: Vec<Message>) -> (SessionsAPI, SessionId) {
    let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    sessions.register_agent(HistoryAgent).unwrap();
    let id = SessionId::from_string("history-proof");
    let mut context = ContextManager::default();
    context.messages = messages;
    sessions
        .create_session_with(
            SystemContext::new(),
            &id,
            &AgentTypeId::from_name("history-proof"),
            move |ctx| {
                ctx.insert(context);
            },
        )
        .unwrap();
    (sessions, id)
}
fn body(message: &IOMessage) -> &str {
    match &message.content {
        IOContent::Text(text) => text,
        _ => panic!("expected text"),
    }
}
fn key(panel: &mut ConversationPanel, code: KeyCode, modifiers: KeyModifiers) {
    panel.handle_event(&TuiEvent::Key(KeyEvent::new(code, modifiers)));
}
async fn screen(panel: &mut ConversationPanel) -> String {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    panel.tick();
    let mut terminal = Terminal::new(TestBackend::new(100, 15)).unwrap();
    terminal
        .draw(|frame| panel.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..15)
        .map(|y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn pages_preserve_identical_prompts_order_and_full_tool_results() {
    let text = "\tfull result\n".repeat(2000);
    let mut messages: Vec<_> = (0..2050)
        .map(|i| Message::user(format!("prompt {i}")))
        .collect();
    messages.push(Message::user("same"));
    messages.push(Message::user("same"));
    messages.push(Message::Assistant {
        id: None,
        content: vec![
            AssistantBlock::reasoning("private reasoning"),
            AssistantBlock::text("answer"),
            AssistantBlock::tool_call(ToolCall::new(
                "tool-1",
                "read_file",
                serde_json::json!({"path":"file.rs"}),
            )),
        ],
    });
    messages.push(Message::User {
        content: vec![UserBlock::tool_result(
            "tool-1",
            ToolResultContent::Text(text.clone()),
        )],
    });
    let (sessions, id) = session(messages);
    let history = CanonicalHistory::default();
    assert!(history.load(None, 1024).await.is_err());
    history.refresh(&sessions, &id).await.unwrap();
    let latest = history.load(None, 1024).await.unwrap();
    assert_eq!(
        (latest.start, latest.total, latest.messages.len()),
        (1030, 2054, 1024)
    );
    let repeated: Vec<_> = latest
        .messages
        .iter()
        .filter(|m| body(m) == "same")
        .collect();
    assert_eq!(repeated.len(), 2);
    assert_ne!(repeated[0].metadata["id"], repeated[1].metadata["id"]);
    assert_eq!(body(latest.messages.back().unwrap()), text);
    assert_eq!(latest.messages.back().unwrap().metadata[IO_TYPE], "tool");
    assert!(
        !latest
            .messages
            .iter()
            .any(|m| body(m).contains("private reasoning"))
    );
    let older = history.load(Some(latest.start), 1024).await.unwrap();
    assert_eq!((older.start, older.messages.len()), (6, 1024));
    assert_eq!(body(older.messages.front().unwrap()), "prompt 6");
    let oldest = history.load(Some(older.start), 1024).await.unwrap();
    assert_eq!((oldest.start, oldest.messages.len()), (0, 6));
    let replay = history.load(Some(latest.start), 1024).await.unwrap();
    assert_eq!(
        serde_json::to_value(&older.messages).unwrap(),
        serde_json::to_value(&replay.messages).unwrap()
    );
    assert_eq!(
        history.load(None, usize::MAX).await.unwrap().messages.len(),
        1024
    );
}

#[tokio::test]
async fn evicted_history_is_recovered_without_merging_or_executing_it() {
    let messages = (0..1100)
        .map(|i| Message::user(format!("canonical {i:04}")))
        .collect();
    let (sessions, id) = session(messages);
    let history = Arc::new(CanonicalHistory::default());
    history.refresh(&sessions, &id).await.unwrap();
    let (provider, panel) = TuiIOProvider::new_with_panel(2048);
    let mut panel = panel.with_history_source(history);
    for i in 0..1100 {
        provider
            .send(IOMessage::user_text(format!("canonical {i:04}")))
            .await
            .unwrap();
    }
    let live = screen(&mut panel).await;
    assert!(!live.contains("canonical 0000"));
    key(&mut panel, KeyCode::Char('g'), KeyModifiers::CONTROL);
    key(&mut panel, KeyCode::Char('h'), KeyModifiers::NONE);
    screen(&mut panel).await;
    key(&mut panel, KeyCode::Char('h'), KeyModifiers::NONE);
    let older = screen(&mut panel).await;
    assert_eq!(older.matches("canonical 0000").count(), 1);
    provider
        .send(IOMessage::system_text("live output while browsing"))
        .await
        .unwrap();
    let frozen = screen(&mut panel).await;
    assert!(!frozen.contains("live output while browsing"));
    assert_eq!(frozen.matches("canonical 0000").count(), 1);
    key(&mut panel, KeyCode::End, KeyModifiers::CONTROL);
    let live = screen(&mut panel).await;
    assert!(live.contains("live output while browsing"));
    assert!(!live.contains("canonical 0000"));
    // The fixture's graph panics if executed. Canonical reads also leave the
    // live provider empty: neither replay nor navigation becomes input.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), provider.receive())
            .await
            .is_err()
    );
    let count = sessions
        .with_context(&id, |ctx| {
            ctx.get_resource::<ContextManager>().unwrap().messages.len()
        })
        .await
        .unwrap();
    assert_eq!(count, 1100);
}

#[tokio::test]
async fn snapshots_change_only_at_refresh_and_bad_session_invalidates_cache() {
    let (sessions, id) = session(vec![Message::user("before")]);
    let history = CanonicalHistory::default();
    history.refresh(&sessions, &id).await.unwrap();
    sessions
        .with_context(&id, |ctx| {
            ctx.get_resource_mut::<ContextManager>()
                .unwrap()
                .push(Message::assistant("after"))
        })
        .await
        .unwrap();
    assert_eq!(history.load(None, 10).await.unwrap().total, 1);
    history.refresh(&sessions, &id).await.unwrap();
    assert_eq!(history.load(None, 10).await.unwrap().total, 2);
    assert!(
        history
            .refresh(&sessions, &SessionId::from_string("missing"))
            .await
            .is_err()
    );
    assert!(history.load(None, 10).await.is_err());
}
