//! The tabs the default interactive shell shows, and the node timeline that
//! survives the Logs tab being folded away.
//!
//! The shell has Conversation, Graph and Help. There is no Logs tab, but the
//! log plugin is still installed: it owns the global `tracing` subscriber and
//! fills the buffer behind the conversation panel's `Ctrl-O` node timeline.
//!
//! This file is its own test binary, with a single test, because that
//! subscriber is process-global and only the first install can win.

use polaris::plugins::{IOContent, IOMessage, UserIO};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::server::Server;
use polaris_code::CodeAgent;
use polaris_code::tabs::add_shell_tabs;
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use polaris_tui::panels::{ConversationPanelPlugin, GraphFeed, LogBuffer};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{TuiEvent, TuiPanel, TuiRegistry};

/// Stands in for the shell plugin, which owns the registry panels join and
/// would otherwise take over the terminal.
struct ShellStandIn;

impl Plugin for ShellStandIn {
    const ID: &'static str = "test::shell";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }
}

fn screen(panel: &mut dyn TuiPanel, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| panel.render(frame, frame.area()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn ctrl_o() -> TuiEvent {
    TuiEvent::Key(KeyEvent {
        code: KeyCode::Char('o'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

#[tokio::test]
async fn the_default_tabs_are_conversation_graph_and_help_and_ctrl_o_still_shows_node_logs() {
    let log_buffer = LogBuffer::with_capacity(256);
    let mut server = Server::new();
    server.add_plugins(ShellStandIn);
    // Assembled in the order `main.rs` uses: the other tabs, then the
    // conversation, which therefore comes first.
    add_shell_tabs(&mut server, log_buffer.clone(), &CodeAgent);
    server.add_plugins(ConversationPanelPlugin::default().with_log_buffer(log_buffer.clone()));
    server.finish().await.expect("the shell assembles");

    // The tab set, in the order the shell shows it.
    let mut panels = server.api::<TuiRegistry>().expect("registry").drain();
    let ids: Vec<&str> = panels.iter().map(|panel| panel.id()).collect();
    assert_eq!(
        ids,
        ["conversation", "graph", "help"],
        "no Logs tab; Graph sits between the conversation and Help"
    );
    let titles: Vec<&str> = panels.iter().map(|panel| panel.title()).collect();
    assert_eq!(titles, ["Conversation", "Graph", "Help"]);

    // The Graph tab already draws the agent's graph before any turn runs.
    let feed = server.get_global::<GraphFeed>().expect("the feed");
    assert_eq!(
        feed.snapshot()
            .model()
            .map(|model| model.name().to_string()),
        Some("CodeAgent".to_string())
    );
    let graph = panels
        .iter_mut()
        .find(|panel| panel.id() == "graph")
        .expect("the graph panel");
    let lines = screen(graph.as_mut(), 80, 24);
    assert!(lines[0].contains("CodeAgent"), "{lines:?}");
    assert!(
        lines.iter().any(|row| row.contains("receive_user_input")),
        "{lines:?}"
    );

    // Ctrl-O: the conversation's node timeline shows each node's own log lines.
    let conversation = panels
        .iter_mut()
        .find(|panel| panel.id() == "conversation")
        .expect("the conversation panel");
    let context = server.create_context();
    let io = context
        .get_resource::<UserIO>()
        .expect("the conversation panel provides UserIO");
    io.send(
        IOMessage::from_agent("code", IOContent::Text("Thinking…".to_string()))
            .with_metadata("io_type", "status")
            .with_metadata("node", "act")
            .with_metadata("seq", "1"),
    )
    .await
    .expect("the status reaches the panel");
    // The span the progress middleware opens around a node, and a framework
    // event inside it.
    let span = tracing::info_span!(
        "polaris.graph.execute_system",
        polaris.graph.system.name = "act",
        polaris.graph.system.seq = 1u64,
    );
    span.in_scope(|| tracing::info!(target: "polaris_code::shell_tabs", "model call started"));
    // An event from a dependency is not the agent's own activity.
    span.in_scope(|| tracing::info!(target: "hyper::client", "socket opened"));

    conversation.handle_event(&ctrl_o());
    let mut shown = Vec::new();
    for _ in 0..200 {
        conversation.handle_event(&TuiEvent::Tick);
        shown = screen(conversation.as_mut(), 100, 30);
        if shown.iter().any(|row| row.contains("model call started")) {
            break;
        }
        tokio::task::yield_now().await;
    }
    let text = shown.join("\n");
    assert!(text.contains("act"), "the node is on the timeline:\n{text}");
    assert!(
        text.contains("model call started"),
        "the node's own log line is shown under it:\n{text}"
    );
    assert!(
        !text.contains("socket opened"),
        "noise from dependencies is filtered:\n{text}"
    );
}
