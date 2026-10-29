//! Multi-panel example: the built-in `help`, `logs`, and `conversation`
//! panels plus a tiny custom panel.
//!
//! `Tab` switches panels, `Ctrl-L` toggles between single and tiled layout.
//! No agent is wired in this example — `ConversationPanel` works
//! standalone, echoing whatever you type into the transcript. Hook it up to
//! a real agent by adding a `Plugin` whose `build()` resolves
//! `Res<UserIO>` in a system and runs it from `ready()`.

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use polaris_tui::panels::{ConversationPanelPlugin, HelpPanelPlugin, LogPanelPlugin};
use polaris_tui::ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use polaris_tui::{LayoutMode, TuiConfig, TuiHandle, TuiPanel, TuiPlugin, TuiRegistry};

struct UptimePanel {
    started: std::time::Instant,
}

impl TuiPanel for UptimePanel {
    fn id(&self) -> &str {
        "uptime"
    }
    fn title(&self) -> &str {
        "Uptime"
    }
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let elapsed = self.started.elapsed().as_secs();
        let lines = vec![
            Line::from("custom panel demo"),
            Line::from(""),
            Line::from(vec![
                Span::raw("session uptime: "),
                Span::styled(format!("{elapsed}s"), Style::default().fg(Color::Cyan)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "panels live next to the built-in ones — pick what you want.",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }
}

struct UptimeContribution;

impl Plugin for UptimeContribution {
    const ID: &'static str = "examples::uptime";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin must be added first")
            .add_panel(UptimePanel {
                started: std::time::Instant::now(),
            });
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
    }
}

#[tokio::main]
async fn main() {
    let mut server = Server::new();
    server
        .add_plugins(TuiPlugin::new(
            TuiConfig::default()
                .with_layout(LayoutMode::Single)
                // Enable mouse capture so the wheel can scroll the transcript /
                // logs. The shell still launches in selection mode (the mouse is
                // left to the terminal so its native drag-select + copy work);
                // press Ctrl-S to grab the mouse for wheel scrolling, Ctrl-S
                // again to release it. Shift/Option-drag also selects natively in
                // most terminals without toggling.
                .with_mouse(true),
        ))
        .add_plugins(LogPanelPlugin::default())
        .add_plugins(ConversationPanelPlugin::default())
        .add_plugins(UptimeContribution)
        .add_plugins(HelpPanelPlugin);
    server.run().await.expect("server failed to start");

    // Emit a couple of demo log lines so the Logs panel has something to
    // show without an agent wired in.
    tracing::info!("polaris-tui shell example started");
    tracing::warn!(target: "examples::shell", "this is a demo warning");

    let wait = server
        .api::<TuiHandle>()
        .expect("TuiPlugin must publish TuiHandle in ready()")
        .wait();
    wait.await;
}
