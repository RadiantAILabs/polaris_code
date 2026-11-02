//! The subscriber and buffer can be used without a TUI shell.
//!
//! Kept in its own test binary because subscriber installation is global.

use polaris::system::server::Server;
use polaris_tui::TuiRegistry;
use polaris_tui::panels::{LogBuffer, LogPanelPlugin};

#[tokio::test]
async fn without_panel_assembles_and_captures_logs_without_a_shell() {
    let buffer = LogBuffer::with_capacity(16);
    let mut server = Server::new();
    server.add_plugins(LogPanelPlugin::with_buffer(buffer.clone()).without_panel());
    server
        .finish()
        .await
        .expect("the log plugin assembles without a TUI shell");

    assert!(server.api::<TuiRegistry>().is_none());
    tracing::info!("captured without a shell");
    assert!(
        buffer
            .snapshot()
            .iter()
            .any(|entry| entry.message == "captured without a shell")
    );
}
