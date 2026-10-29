//! Smallest example: register a single custom panel under `TuiPlugin`.

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use polaris_tui::ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use polaris_tui::{TuiConfig, TuiHandle, TuiPanel, TuiPlugin, TuiRegistry};

struct GreetingPanel {
    ticks: u64,
}

impl TuiPanel for GreetingPanel {
    fn id(&self) -> &str {
        "greeting"
    }
    fn title(&self) -> &str {
        "Greeting"
    }
    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
    }
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let lines = vec![
            Line::from("hello, polaris-tui"),
            Line::from(""),
            Line::from(vec![
                Span::raw("ticks observed: "),
                Span::styled(self.ticks.to_string(), Style::default().fg(Color::Cyan)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "press ctrl-c to quit",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }
}

struct HelloContribution;

impl Plugin for HelloContribution {
    const ID: &'static str = "examples::hello_contribution";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin must be added first")
            .add_panel(GreetingPanel { ticks: 0 });
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
        .add_plugins(TuiPlugin::new(TuiConfig::default()))
        .add_plugins(HelloContribution);
    server.run().await.expect("server failed to start");

    let wait = server
        .api::<TuiHandle>()
        .expect("TuiPlugin must publish TuiHandle in ready()")
        .wait();
    wait.await;
}
