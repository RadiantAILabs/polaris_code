//! A host can keep the log subscriber and buffer without showing a Logs tab.
//!
//! This file is its own test binary, with a single test, because the plugin
//! installs the process's global tracing subscriber and only the first install
//! can win.

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::server::Server;
use polaris_tui::TuiRegistry;
use polaris_tui::panels::{LogBuffer, LogPanelPlugin};

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

#[tokio::test]
async fn without_a_panel_the_buffer_still_fills_and_no_tab_is_added() {
    let buffer = LogBuffer::with_capacity(16);
    let mut server = Server::new();
    server
        .add_plugins(ShellStandIn)
        .add_plugins(LogPanelPlugin::with_buffer(buffer.clone()).without_panel());
    server.finish().await.expect("the server assembles");

    let registry = server.api::<TuiRegistry>().expect("registry");
    assert!(
        registry.is_empty(),
        "no Logs tab was contributed: {registry:?}"
    );

    tracing::info!("still captured");
    let entries = buffer.snapshot();
    assert!(
        entries
            .iter()
            .any(|entry| entry.message == "still captured"),
        "the subscriber was installed over the shared buffer: {entries:?}"
    );

    // The default still contributes its tab. Its subscriber install loses to
    // the one above, which only costs a warning.
    let mut with_tab = Server::new();
    with_tab
        .add_plugins(ShellStandIn)
        .add_plugins(LogPanelPlugin::default());
    with_tab.finish().await.expect("the server assembles");
    let panels = with_tab.api::<TuiRegistry>().expect("registry").drain();
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].id(), "logs");
}
