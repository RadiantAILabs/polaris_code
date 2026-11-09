//! The tabs the interactive front end shows around the conversation.
//!
//! `main.rs` chooses and wires plugins; this is the one place that says which
//! tabs the default `polaris` shell has besides the conversation, so the binary
//! and its tests assemble the same set.
//!
//! | Tab | Plugin | Shows |
//! |-----|--------|-------|
//! | Conversation | [`ConversationPanelPlugin`](polaris_tui::panels::ConversationPanelPlugin) | The transcript; added by the caller, because it carries the host's callbacks. |
//! | Graph | [`GraphPanelPlugin`] with [`GraphViewPlugin`] | The agent's graph, with the executing node lit, or the topology a turn selected. |
//! | Help | [`HelpPanelPlugin`] | Keys. |
//!
//! There is no Logs tab. The log plugin is still installed, without its tab,
//! because it owns the global `tracing` subscriber and fills the
//! [`LogBuffer`] the conversation panel's `Ctrl-O` node timeline reads: each
//! node's captured output appears under that node there.

use crate::graph_view::GraphViewPlugin;
use polaris::agent::Agent;
use polaris::system::server::Server;
use polaris_tui::panels::{GraphPanelPlugin, HelpPanelPlugin, LogBuffer, LogPanelPlugin};

/// Only events from these target prefixes reach the log buffer, which drops the
/// noise of transitive dependencies (HTTP clients, the triple store) and keeps
/// the agent's own activity.
const LOG_TARGETS: [&str; 1] = ["polaris"];

/// Adds the shell's tabs other than the conversation.
///
/// `log_buffer` is the buffer the caller also gives the conversation panel
/// (`with_log_buffer`), and `agent` is the agent whose graph the Graph tab shows
/// until a topology replaces it for a turn.
///
/// # Requirements
///
/// The plugins added here register their panels in the `TuiRegistry` that
/// [`TuiPlugin`](polaris_tui::TuiPlugin) provides, so the server needs that
/// plugin. It may be added before or after this call, because the server
/// orders plugins by what they require. If it is never added,
/// [`Server::finish`] returns an error that names the missing capability and the
/// plugins that need it, and no tab is shown.
///
/// # Tab order
///
/// The server builds the last-added of its independent plugins first, and the
/// shell lists panels in the order they were built, so tabs appear in the
/// reverse of the order their plugins are added. Add the conversation plugin
/// *after* this call and it is the first tab, with Graph then Help behind it.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::CodeAgent;
/// use polaris_code::tabs::add_shell_tabs;
/// use polaris_tui::panels::{ConversationPanelPlugin, LogBuffer};
/// use polaris_tui::{TuiConfig, TuiPlugin};
///
/// # async fn assemble() {
/// let log_buffer = LogBuffer::with_capacity(2_000);
/// let mut server = Server::new();
/// server.add_plugins(TuiPlugin::new(TuiConfig::default()));
/// // Graph and Help, plus the log capture the conversation's `Ctrl-O` reads.
/// add_shell_tabs(&mut server, log_buffer.clone(), &CodeAgent);
/// // Added last, so it is the first tab.
/// server.add_plugins(ConversationPanelPlugin::default().with_log_buffer(log_buffer));
/// server.finish().await.expect("the shell assembles");
/// # }
/// ```
pub fn add_shell_tabs(server: &mut Server, log_buffer: LogBuffer, agent: &impl Agent) {
    add_tabs(server, log_buffer, GraphViewPlugin::for_agent(agent));
}

/// Adds the shell's tabs other than the conversation, with the Graph tab
/// showing the coding agent as plugins extended it.
///
/// This is [`add_shell_tabs`] for a host that registers
/// [`CodeAgent::with_steps`](crate::agents::CodeAgent::with_steps): the tab
/// draws the graph built from the step registry once every plugin has built, so
/// steps that plugins add appear in it. With no steps added the graph is the
/// stock one. Everything else, including the requirements and the tab order, is
/// as for [`add_shell_tabs`]. The server also needs
/// [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin), which owns the registry.
pub fn add_code_agent_shell_tabs(server: &mut Server, log_buffer: LogBuffer) {
    add_tabs(server, log_buffer, GraphViewPlugin::for_code_agent_steps());
}

fn add_tabs(server: &mut Server, log_buffer: LogBuffer, graph: GraphViewPlugin) {
    server
        .add_plugins(HelpPanelPlugin)
        .add_plugins(
            LogPanelPlugin::with_buffer(log_buffer)
                .with_target_prefixes(LOG_TARGETS)
                .without_panel(),
        )
        .add_plugins(GraphPanelPlugin::default())
        .add_plugins(graph);
}
