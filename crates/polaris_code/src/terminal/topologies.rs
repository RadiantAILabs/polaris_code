//! `/topologies` — what this build registered, and what can run here.
//!
//! The command owns no list. It holds the sealed
//! [`TopologyCatalogSnapshot`] the composition root already took and asks
//! [`cosmos::inventory`](crate::cosmos::inventory) the question each time, so
//! the answer is whatever the catalog actually sealed. Holding the snapshot is
//! safe precisely because sealing is final: a sealed catalog cannot gain or
//! lose a registration, so one read cannot go stale.
//!
//! Running the command registers nothing, starts nothing, and changes nothing.
//! The only thing it touches outside its own memory is one `stat` of the
//! workspace root, which is how "can it run here" stops being a guess.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use polaris_cosmos::topology::TopologyCatalogSnapshot;
use polaris_tui::commands::CommandSpec;

use super::{CommandFuture, CommandOutcome, CommandRegistry};
use crate::cosmos::inventory;
use crate::cosmos::topology::HostCapabilities;

/// The read-only listing behind `/topologies`.
#[derive(Debug, Clone)]
pub struct TopologyListing {
    catalog: TopologyCatalogSnapshot,
    workspace_root: PathBuf,
    host: HostCapabilities,
}

impl TopologyListing {
    /// Captures the sealed catalog and the host this build is running on.
    #[must_use]
    pub fn new(
        catalog: TopologyCatalogSnapshot,
        workspace_root: impl Into<PathBuf>,
        host: HostCapabilities,
    ) -> Self {
        Self {
            catalog,
            workspace_root: workspace_root.into(),
            host,
        }
    }

    /// Registers `/topologies` on the additive command registry.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is already registered.
    pub fn register(&self, registry: &CommandRegistry) -> Result<(), super::CommandError> {
        let listing = self.clone();
        registry.register(
            CommandSpec::new(
                "topologies",
                "List the topologies this build registered and whether each can run here",
            ),
            Arc::new(move |_ctx| {
                let listing = listing.clone();
                Box::pin(async move { listing.reply() }) as CommandFuture<'_>
            }),
        )
    }

    /// Renders the listing exactly as the command replies with it.
    #[must_use]
    pub fn render(&self) -> String {
        match inventory::observe(
            &self.catalog,
            &self.workspace_root,
            self.host,
            unix_epoch_ms(),
        ) {
            Ok(listed) => listed.to_text(),
            Err(error) => format!("could not read the sealed topology catalog: {error}"),
        }
    }

    fn reply(&self) -> CommandOutcome {
        match inventory::observe(
            &self.catalog,
            &self.workspace_root,
            self.host,
            unix_epoch_ms(),
        ) {
            Ok(listed) => CommandOutcome::Reply(listed.to_text()),
            Err(error) => CommandOutcome::Error(format!(
                "could not read the sealed topology catalog: {error}"
            )),
        }
    }
}

fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use polaris::system::server::Server;
    use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogPlugin};
    use polaris_tui::commands::CommandInvocation;

    use super::*;
    use crate::cosmos::topology::CodingTopologyPackPlugin;

    async fn sealed() -> TopologyCatalogSnapshot {
        let mut server = Server::new();
        server.add_plugins(TopologyCatalogPlugin);
        server.add_plugins(CodingTopologyPackPlugin);
        server.finish().await.expect("the coding pack assembles");
        server
            .get_global::<TopologyCatalog>()
            .expect("the provider globalizes the catalog")
            .snapshot()
            .expect("the catalog is sealed")
    }

    #[tokio::test]
    async fn the_command_answers_from_the_sealed_catalog() {
        let catalog = sealed().await;
        let directory = tempfile::tempdir().expect("temp workspace");
        let listing = TopologyListing::new(
            catalog.clone(),
            directory.path(),
            HostCapabilities::hermetic(),
        );
        let registry = CommandRegistry::with_builtins();
        listing.register(&registry).expect("/topologies registers");
        assert!(registry.contains("topologies"));

        let CommandOutcome::Reply(text) = registry
            .dispatch(&CommandInvocation::new("topologies", ""))
            .await
        else {
            panic!("/topologies replies with the listing");
        };
        for candidate in catalog.candidates() {
            assert!(
                text.contains(candidate.registration_id().as_str()),
                "the reply names every sealed registration"
            );
        }
    }
}
