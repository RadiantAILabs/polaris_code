//! [`TuiPlugin`] — registers [`TuiRegistry`] and spawns the TUI runtime.
//!
//! Mirrors the lifecycle of
//! [`polaris::app::AppPlugin`](https://docs.rs/polaris-ai/latest/polaris_ai/app/index.html):
//! drains contributions in `ready()`, spawns a background tokio task that
//! owns the terminal, and exposes a [`TuiHandle`] API so `cleanup()` can
//! join the task on shutdown.

use crate::config::TuiConfig;
use crate::registry::TuiRegistry;
use crate::runtime::{self, RuntimeShutdown, TerminalRequest};
use parking_lot::Mutex;
use polaris::system::api::API;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::server::Server;
use std::io;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// Polaris plugin that hosts the TUI shell.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | Contributes APIs and a background task only.       |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`TuiRegistry`] | Build-time contribution surface for panels. |
/// | [`TuiHandle`]   | Runtime handle: terminal handoff, shutdown and task joining. |
///
/// # Lifecycle
///
/// - `build()` inserts an empty [`TuiRegistry`] for contributors.
/// - `ready()` drains the registry, spawns the runtime task, and stores a
///   [`TuiHandle`].
/// - `cleanup()` signals shutdown and joins the task with a timeout.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_tui::{TuiConfig, TuiPlugin};
///
/// # async fn run() {
/// let mut server = Server::new();
/// server.add_plugins(TuiPlugin::new(TuiConfig::default()));
/// // …contributor plugins call server.api::<TuiRegistry>().add_panel(…)…
/// server.run().await;
/// # }
/// ```
#[derive(Default)]
pub struct TuiPlugin {
    config: TuiConfig,
}

impl TuiPlugin {
    /// Creates the plugin with the given config.
    #[must_use]
    pub fn new(config: TuiConfig) -> Self {
        Self { config }
    }
}

impl Plugin for TuiPlugin {
    const ID: &'static str = "polaris::tui";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Provides the `TuiRegistry` capability that every panel plugin requires.
        // The resolver orders this plugin before its requirers and verifies the
        // registry was actually inserted in `build()`, so panels can rely on it.
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }

    async fn ready(&self, server: &mut Server) {
        let registry = server
            .api::<TuiRegistry>()
            .expect("TuiRegistry must exist (registered in build)")
            .clone();
        let mut panels = registry.drain();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (terminal_tx, terminal_rx) = mpsc::channel(1);
        let handle_api = TuiHandle {
            shutdown_tx: Arc::new(Mutex::new(Some(shutdown_tx))),
            handle: Arc::new(Mutex::new(None)),
            terminal_tx,
        };
        for panel in &mut panels {
            panel.attach_runtime(handle_api.clone());
        }
        let config = self.config.clone();
        let handle = tokio::spawn(async move {
            if let Err(err) = runtime::run(
                panels,
                config,
                RuntimeShutdown::new(shutdown_rx),
                terminal_rx,
            )
            .await
            {
                tracing::error!(error = %err, "TUI runtime exited with error");
            }
        });
        *handle_api.handle.lock() = Some(handle);
        server.insert_api(handle_api);
    }

    async fn cleanup(&self, server: &mut Server) {
        let Some(handle_api) = server.api::<TuiHandle>() else {
            return;
        };
        handle_api.shutdown();
        let Some(join) = handle_api.handle.lock().take() else {
            return;
        };
        let timeout = Duration::from_secs(2);
        match tokio::time::timeout(timeout, join).await {
            Ok(Ok(())) => tracing::info!("TUI runtime shut down gracefully"),
            Ok(Err(err)) => tracing::warn!(error = %err, "TUI runtime task panicked"),
            Err(_) => tracing::warn!("TUI runtime shutdown timed out"),
        }
    }
}

/// Runtime handle for the TUI shell, including interactive editor handoffs.
///
/// # Provided By
/// [`TuiPlugin`] publishes this API during `ready()` and attaches a clone to
/// each panel through [`TuiPanel::attach_runtime`](crate::TuiPanel::attach_runtime).
///
/// # Surface
/// | Method | Purpose |
/// |--------|---------|
/// | [`run_in_terminal`](Self::run_in_terminal) | Execute an interactive child with terminal modes suspended and restored. |
/// | [`shutdown`](Self::shutdown) | Request cooperative shutdown, including stopping an active child. |
/// | [`wait`](Self::wait) | Join the runtime task; only the first caller takes the join handle. |
///
/// # Lifecycle
/// Available after the provider's `ready()`. Panels receive it before `init()`.
/// Terminal requests must be awaited outside event handlers. Shutdown closes
/// the request channel; subsequent requests report an error. The first `wait()`
/// or provider cleanup takes the shared join handle; later joins are no-ops.
///
/// # Composition
/// **Provider-scoped.** Clones address the same shell, serialize terminal
/// handoffs and share shutdown/join state. This is a UI handoff, not a tool
/// permission mechanism or background-process manager.
///
/// # Example
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_tui::{TuiPlugin, TuiHandle};
/// use std::process::Command;
///
/// let mut server = Server::new();
/// server.add_plugins(TuiPlugin::default());
/// // In a consumer's ready(), after TuiPlugin is ready:
/// # fn consumer_ready(server: &Server) {
/// if let Some(handle) = server.api::<TuiHandle>() {
///     let handle = handle.clone();
///     tokio::spawn(async move {
///         let _ = handle.run_in_terminal(Command::new("vi")).await;
///     });
/// }
/// # }
/// ```
#[derive(Clone)]
pub struct TuiHandle {
    shutdown_tx: Arc<Mutex<Option<watch::Sender<bool>>>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    terminal_tx: mpsc::Sender<TerminalRequest>,
}

impl std::fmt::Debug for TuiHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiHandle")
            .field("running", &self.handle.lock().is_some())
            .finish()
    }
}

impl API for TuiHandle {}

impl TuiHandle {
    /// Run an interactive child while the runtime releases the terminal.
    ///
    /// The runtime pauses input reading and drawing, releases raw mode, mouse
    /// capture, bracketed paste and keyboard enhancement, and leaves the
    /// alternate screen. It restores these modes before returning the exit
    /// status, including after spawn failure or a non-zero exit. Standard IO
    /// is inherited; the command is executed directly, without a shell.
    ///
    /// Call from a background task, never block a panel event handler on this
    /// future. Requests are serialized with a bounded queue. Shutdown kills
    /// and reaps the active child; dropping this future before dispatch cancels
    /// a queued request, but after dispatch the runtime owns the child until
    /// exit or shutdown. Child processes must remain in the foreground.
    ///
    /// # Errors
    /// Returns an IO error if the runtime is unavailable, terminal transition
    /// fails, the child cannot start, or shutdown interrupts it.
    pub async fn run_in_terminal(&self, command: Command) -> io::Result<ExitStatus> {
        let (reply, result) = oneshot::channel();
        self.terminal_tx
            .send(TerminalRequest { command, reply })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI runtime stopped"))?;
        result
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI runtime stopped"))?
    }

    /// Signals the TUI runtime to exit. Idempotent.
    pub fn shutdown(&self) {
        if let Some(tx) = self.shutdown_tx.lock().take() {
            let _ = tx.send(true);
        }
    }

    /// Returns a future that resolves when the TUI task exits.
    ///
    /// Only the first caller takes the underlying `JoinHandle`; subsequent
    /// callers see `None` and the future resolves immediately. The handle
    /// is also taken by [`TuiPlugin::cleanup`](TuiPlugin), so don't call
    /// `wait` if you also rely on the server's `cleanup()` phase to join —
    /// pick one.
    pub fn wait(&self) -> impl std::future::Future<Output = ()> + 'static {
        let join = self.handle.lock().take();
        async move {
            if let Some(handle) = join {
                let _ = handle.await;
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn test_handle() -> (TuiHandle, mpsc::Receiver<TerminalRequest>) {
    let (shutdown, _) = watch::channel(false);
    let (terminal_tx, terminal_rx) = mpsc::channel(1);
    (
        TuiHandle {
            shutdown_tx: Arc::new(Mutex::new(Some(shutdown))),
            handle: Arc::new(Mutex::new(None)),
            terminal_tx,
        },
        terminal_rx,
    )
}
