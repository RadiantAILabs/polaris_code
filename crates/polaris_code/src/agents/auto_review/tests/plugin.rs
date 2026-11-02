//! The plugin assembles from the dependencies it declares, whatever order an
//! assembly registers them in.

use super::*;
use crate::agents::permission_rules::PermissionRulesPlugin;
use crate::tools::{ToolPolicyPlugin, ToolPolicyRegistry};

fn reviewer() -> ReviewPolicy {
    ReviewPolicy {
        timeout: Duration::from_secs(7),
        ..Default::default()
    }
}

/// Starts `server` with the usage meter the host supplies and returns what the
/// plugin published: its policy, and whether each local resource resolves.
async fn started(mut server: Server) -> (Duration, bool, bool, bool) {
    server.insert_global(TokenMeter::new());
    server.finish().await.unwrap();
    let ctx = server.create_context();
    (
        server.get_global::<ReviewPolicy>().unwrap().timeout,
        server.get_global::<PermissionRules>().is_some()
            && server.get_global::<ToolPolicyRegistry>().is_some(),
        ctx.get_resource::<ReviewIntent>().is_ok(),
        ctx.get_resource::<ReviewHistory>().is_ok(),
    )
}

#[tokio::test]
async fn assembles_from_its_declared_dependencies_alone() {
    // Only the models plugin is added: the plugin names the permission rules and
    // the tool policy it needs, and the server supplies their defaults.
    let mut server = Server::new();
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(AutoReviewPlugin::new(reviewer()));
    assert_eq!(
        started(server).await,
        (Duration::from_secs(7), true, true, true)
    );
}

#[tokio::test]
async fn registration_order_does_not_change_the_result() {
    let mut forward = Server::new();
    forward
        .add_plugins(ModelsPlugin)
        .add_plugins(ToolPolicyPlugin)
        .add_plugins(PermissionRulesPlugin::default())
        .add_plugins(AutoReviewPlugin::new(reviewer()));
    let mut reverse = Server::new();
    reverse
        .add_plugins(AutoReviewPlugin::new(reviewer()))
        .add_plugins(PermissionRulesPlugin::default())
        .add_plugins(ToolPolicyPlugin)
        .add_plugins(ModelsPlugin);
    let expected = (Duration::from_secs(7), true, true, true);
    assert_eq!(started(forward).await, expected);
    assert_eq!(started(reverse).await, expected);
}
