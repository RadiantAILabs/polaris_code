//! Plugin lifecycle for hosts that assemble a flow with a Polaris server.
use super::{EventLog, SpendBook};
use polaris::system::{plugin, plugin::Plugin, resource::GlobalResource, server::Server};

type Installer = Box<dyn Fn(&mut Server) + Send + Sync>;

/// Registers the flow resources a host already built and settles spend left
/// over from an earlier run.
///
/// Reach for this plugin when a flow runs inside a Polaris [`Server`]. The host
/// opens its resources (`SpendBook`, `StepJournal`, `EventLog`, `PhaseControl`
/// and the rest) with their own settings and hands each one over with
/// [`with_resource`](Self::with_resource). The plugin adds no policy of its own.
///
/// # Resources Provided
///
/// Every resource handed to [`with_resource`](Self::with_resource), as a
/// global.
///
/// # Dependencies
///
/// None.
///
/// # Lifecycle
///
/// - **`build()`** — inserts each supplied resource as a global.
/// - **`ready()`** — when a [`SpendBook`] was supplied, charges every call it
///   still holds as in flight at its full ceiling, because no call survives a
///   restart.
/// - **`cleanup()`** — when an [`EventLog`] was supplied, stops its running
///   phase timers.
///
/// # Panics
///
/// `build()` panics when two supplied resources have the same type, and
/// `ready()` panics when abandoned reservations cannot be settled.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_cosmos::flow::{EventLog, FlowPlugin, SpendBook};
/// # async fn assemble(root: &std::path::Path) -> Result<(), polaris_cosmos::flow::FlowError> {
/// let spend = SpendBook::open(root, "run-1", 10_000_000, [("build".into(), 1_000_000)].into(), 0)?;
/// let events = EventLog::open(root, "run-1");
/// let mut server = Server::new();
/// server.add_plugins(
///     FlowPlugin::new()
///         .with_resource(spend)
///         .with_resource(events),
/// );
/// server.finish().await.expect("flow plugin should resolve");
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct FlowPlugin {
    installers: Vec<Installer>,
    model_id: String,
}
impl std::fmt::Debug for FlowPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowPlugin")
            .field("resources", &self.installers.len())
            .field("model_id", &self.model_id)
            .finish()
    }
}
impl FlowPlugin {
    /// Creates a plugin with no resources.
    pub fn new() -> Self {
        Self::default()
    }
    /// Supplies one resource the host built, to be registered as a global.
    pub fn with_resource<R: GlobalResource + Clone>(mut self, resource: R) -> Self {
        self.installers.push(Box::new(move |server| {
            assert!(
                server.insert_global(resource.clone()).is_none(),
                "{} was supplied to the flow plugin twice",
                std::any::type_name::<R>()
            );
        }));
        self
    }
    /// Names the model recorded on spend charged for abandoned calls.
    pub fn recovering_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = model_id.into();
        self
    }
}

#[plugin(id = "cosmos::flow", version = "0.1.0")]
impl Plugin for FlowPlugin {
    fn build(&self, server: &mut Server) {
        for install in &self.installers {
            install(server);
        }
    }

    async fn ready(&self, server: &mut Server) {
        if let Some(spend) = server.get_global::<SpendBook>()
            && let Err(error) = spend.recover_abandoned_with_model(&self.model_id)
        {
            panic!("abandoned reservations could not be settled: {error}");
        }
    }

    async fn cleanup(&self, server: &mut Server) {
        if let Some(events) = server.get_global::<EventLog>() {
            events.stop_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::{RunHandle, RunMode};
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn supplied_resources_are_registered_and_abandoned_spend_is_charged() {
        let root = std::env::temp_dir().join(format!("flow-plugin-{}", std::process::id()));
        let shares = BTreeMap::from([("build".to_owned(), 1_000_000)]);
        let spend = SpendBook::open(&root, "run", 10_000_000, shares, 0).unwrap();
        spend.reserve("build", "worker", 1_000_000).unwrap();
        let handle = RunHandle {
            run_id: "run".into(),
            root: root.clone(),
            mode: RunMode::Resume,
        };
        let mut server = Server::new();
        server.add_plugins(
            FlowPlugin::new()
                .with_resource(spend)
                .with_resource(handle)
                .recovering_model("model"),
        );
        server.finish().await.expect("flow plugin should resolve");
        let spend = server.get_global::<SpendBook>().expect("spend book");
        let summary = spend.summary().unwrap();
        assert_eq!(summary.reserved_usd.micros(), 0);
        assert_eq!(summary.total_cost_usd.micros(), 1_000_000);
        assert_eq!(summary.models_called["model"], 1);
        assert!(server.get_global::<RunHandle>().is_some());
        drop(spend);
        server.cleanup().await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
