//! Graph publication and panel registration follow declared dependencies.

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::server::Server;
use polaris_code::CodeAgent;
use polaris_code::graph_view::GraphViewPlugin;
use polaris_tui::TuiRegistry;
use polaris_tui::panels::{GraphFeed, GraphPanelPlugin};

/// Provides the shell registry without taking over the terminal.
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
async fn reversed_registration_still_publishes_the_model_and_registers_the_panel() {
    let view = GraphViewPlugin::for_agent(&CodeAgent);
    let expected = view.model().clone();
    let mut server = Server::new();
    server
        .add_plugins(view)
        .add_plugins(GraphPanelPlugin::default())
        .add_plugins(ShellStandIn);
    server.finish().await.expect("the dependencies resolve");

    let feed = server.get_global::<GraphFeed>().expect("the feed");
    assert_eq!(
        feed.snapshot().model().map(|model| model.as_ref()),
        Some(&expected)
    );
    let panels = server.api::<TuiRegistry>().expect("registry").drain();
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].id(), "graph");
    assert_eq!(panels[0].title(), "Graph");
}
