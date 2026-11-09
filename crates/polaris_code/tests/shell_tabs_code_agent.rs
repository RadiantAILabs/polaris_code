//! The shell's tabs when the agent is built from the plugins' step registry,
//! which is how `polaris` starts: the tab set and its order are the default
//! shell's, and the Graph tab draws the steps plugins added.
//!
//! This file is its own test binary, with a single test, because the log plugin
//! installs the process-global `tracing` subscriber and only the first install
//! can win.

use std::sync::Arc;

use polaris::models::ModelsPlugin;
use polaris::plugins::PersistencePlugin;
use polaris::sessions::{InMemoryStore, SessionsPlugin};
use polaris::system::param::ResMut;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version};
use polaris::system::server::Server;
use polaris::system::system;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::code_agent::steps::CodeAgentSteps;
use polaris_code::agents::{CodeAgentPlugin, ContextManager};
use polaris_code::tabs::add_code_agent_shell_tabs;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::panels::{ConversationPanelPlugin, GraphFeed, LogBuffer};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{TuiPanel, TuiRegistry};

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

#[system]
async fn count_the_tool_round(_context: ResMut<ContextManager>) {}

/// Adds one step after each round of tool calls.
struct AddsAStep;

impl Plugin for AddsAStep {
    const ID: &'static str = "test::adds_a_step";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<CodeAgentPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<CodeAgentSteps>()
            .expect("CodeAgentPlugin provides the step registry")
            .after_tools("test.count", 0, |graph| {
                graph.add_system(count_the_tool_round);
            })
            .expect("the key is new");
    }
}

fn screen(panel: &mut dyn TuiPanel, width: u16, height: u16) -> String {
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
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_graph_tab_draws_the_agent_with_the_steps_plugins_added() {
    let log_buffer = LogBuffer::with_capacity(256);
    let mut server = Server::new();
    server
        .add_plugins(ShellStandIn)
        .add_plugins(ModelsPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(AddsAStep);
    // The order `polaris` uses: the other tabs, then the conversation.
    add_code_agent_shell_tabs(&mut server, log_buffer.clone());
    server.add_plugins(ConversationPanelPlugin::default().with_log_buffer(log_buffer));
    server.finish().await.expect("the shell assembles");

    let mut panels = server.api::<TuiRegistry>().expect("registry").drain();
    let ids: Vec<&str> = panels.iter().map(|panel| panel.id()).collect();
    assert_eq!(
        ids,
        ["conversation", "graph", "help"],
        "the same tabs, in the same order, as the default shell"
    );

    let feed = server.get_global::<GraphFeed>().expect("the feed");
    assert_eq!(
        feed.snapshot()
            .model()
            .map(|model| model.name().to_string()),
        Some("CodeAgent".to_string()),
        "the agent keeps its name"
    );
    let graph = panels
        .iter_mut()
        .find(|panel| panel.id() == "graph")
        .expect("the graph panel");
    let drawn = screen(graph.as_mut(), 100, 200);
    assert!(drawn.contains("receive_user_input"), "{drawn}");
    assert!(
        drawn.contains("count_the_tool_round"),
        "the added step is on the Graph tab:\n{drawn}"
    );
}
