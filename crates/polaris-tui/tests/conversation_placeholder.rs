//! `ConversationPanelPlugin::with_placeholder` reaches the input a user sees.
//!
//! The render rules (typed text replaces the hint, an unfocused panel shows
//! none) are unit-tested inside the panel, which owns that state. What these
//! tests pin is the part a host depends on: that the builder option travels
//! through plugin assembly into the panel the registry hands the shell, that
//! the default is `DEFAULT_PLACEHOLDER`, and that the last call wins.

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::server::Server;
use polaris_tui::TuiRegistry;
use polaris_tui::panels::ConversationPanelPlugin;
use polaris_tui::panels::conversation::DEFAULT_PLACEHOLDER;
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;

/// Stands in for the shell plugin, which owns the registry panels join.
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

/// Assemble a server with `plugin`, take the one panel it contributed, and
/// return what that panel draws in a 60×8 terminal.
async fn screen_of(plugin: ConversationPanelPlugin) -> String {
    let mut server = Server::new();
    server.add_plugins(ShellStandIn).add_plugins(plugin);
    server.finish().await.expect("the server assembles");
    let mut panels = server.api::<TuiRegistry>().expect("registry").drain();
    assert_eq!(panels.len(), 1, "the plugin contributes one panel");
    let mut panel = panels.pop().expect("one panel");
    assert_eq!(panel.id(), "conversation");

    let (width, height) = (60, 8);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| panel.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn without_the_option_the_empty_input_shows_the_default_hint() {
    let screen = screen_of(ConversationPanelPlugin::default()).await;
    assert!(
        screen.contains(&format!(
            "{DEFAULT_PLACEHOLDER}  (Ctrl-N browse transcript)"
        )),
        "{screen}"
    );
}

#[tokio::test]
async fn the_option_replaces_the_hint_and_keeps_the_browse_cue() {
    let screen =
        screen_of(ConversationPanelPlugin::default().with_placeholder("Ask Polaris…")).await;
    assert!(
        screen.contains("Ask Polaris…  (Ctrl-N browse transcript)"),
        "{screen}"
    );
    assert!(
        !screen.contains(DEFAULT_PLACEHOLDER),
        "the default is replaced, not shown beside it:\n{screen}"
    );
}

#[tokio::test]
async fn the_last_call_wins() {
    let screen = screen_of(
        ConversationPanelPlugin::default()
            .with_placeholder("first")
            .with_placeholder("second"),
    )
    .await;
    assert!(screen.contains("second  (Ctrl-N"), "{screen}");
    assert!(!screen.contains("first"), "{screen}");
}
