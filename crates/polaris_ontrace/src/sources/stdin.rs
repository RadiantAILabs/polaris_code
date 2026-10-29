//! Stdin trace source.
//!
//! [`StdinIOPlugin`] reads newline-delimited lines from this process's
//! stdin, wraps each as an [`IOMessage`], and forwards to
//! [`TraceIngestAPI`](crate::TraceIngestAPI).
//!
//! Use cases: piping any upstream agent's stdout into ontrace
//! (`agent | ontrace`), or replaying a captured NDJSON file (`cat run.ndjson | ontrace`).

use crate::plugins::trace_ingest::TraceIngestAPI;
use polaris::plugins::{IOError, IOMessage, IOProvider};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};
use tokio::sync::Mutex;

/// [`IOProvider`] backed by this process's stdin.
///
/// Each [`receive`](IOProvider::receive) reads a single newline-terminated
/// line. [`send`](IOProvider::send) returns [`IOError::Unsupported`] —
/// stdin is one-way.
#[derive(Default)]
pub struct StdinProvider {
    lines: Mutex<Option<Lines<BufReader<Stdin>>>>,
}

impl StdinProvider {
    /// Construct a fresh provider; stdin is acquired lazily on first
    /// [`receive`](IOProvider::receive).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn ensure_lines(&self) {
        let mut guard = self.lines.lock().await;
        if guard.is_none() {
            *guard = Some(BufReader::new(tokio::io::stdin()).lines());
        }
    }
}

impl IOProvider for StdinProvider {
    async fn send(&self, _message: IOMessage) -> Result<(), IOError> {
        Err(IOError::Unsupported("stdin is read-only".into()))
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.ensure_lines().await;
        let mut guard = self.lines.lock().await;
        let lines = guard.as_mut().expect("lines populated by ensure_lines");
        match lines.next_line().await {
            Ok(Some(line)) => Ok(IOMessage::user_text(line)),
            Ok(None) => Err(IOError::Closed),
            Err(err) => Err(IOError::Provider(err.to_string())),
        }
    }
}

/// Source plugin that streams this process's stdin into the trace pipeline.
///
/// In [`ready`](Plugin::ready) it spawns a long-lived background task that
/// reads newline-delimited lines from stdin via a [`StdinProvider`], wraps
/// each as an [`IOMessage`], and pushes it into
/// [`TraceIngestAPI`](crate::TraceIngestAPI). The configured extraction
/// backend drains those messages and turns them into knowledge-graph triples.
///
/// Reach for this when the upstream agent writes its event stream to stdout
/// and you want to pipe it straight in (`agent | ontrace`), or when replaying
/// a captured NDJSON file (`cat run.ndjson | ontrace`). It holds no
/// configuration and reads from a single stdin handle, so register at most one
/// per server.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | This is a transport source: it produces no `Res`-accessible resources. It pushes [`IOMessage`]s into [`TraceIngestAPI`](crate::TraceIngestAPI), and the backend (`ExtractionPlugin` / `OxigraphExtractionPlugin`) owns the durable [`KnowledgeGraph`](crate::KnowledgeGraph) / store that results. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Sources are consumers, not publishers. This plugin acquires and pushes into [`TraceIngestAPI`](crate::TraceIngestAPI) (published by [`TraceIngestPlugin`](crate::TraceIngestPlugin)); it exposes no `API` of its own. |
///
/// # Dependencies
///
/// Declared by capability in [`access`](Plugin::access): `requires`
/// [`TraceIngestAPI`](crate::TraceIngestAPI), which
/// [`TraceIngestPlugin`](crate::TraceIngestPlugin) `provides`. The resolver
/// validates the provider exists at `finish()` rather than panicking at
/// runtime, so register [`TraceIngestPlugin`](crate::TraceIngestPlugin) and a
/// backend alongside this source.
///
/// # Lifecycle
///
/// [`ready`](Plugin::ready) fetches the [`TraceIngestAPI`](crate::TraceIngestAPI)
/// sender and `tokio::spawn`s a task that drives a [`StdinProvider`] in a loop,
/// forwarding every line into the ingest channel. The task runs for the life
/// of the server; it exits when stdin reaches EOF or the ingest channel
/// closes. Because stdin is one-way, the provider's
/// [`send`](IOProvider::send) returns [`IOError::Unsupported`].
///
/// # Example
///
/// ```no_run
/// use polaris::models::{AnthropicPlugin, ModelsPlugin};
/// use polaris::plugins::PersistencePlugin;
/// use polaris::sessions::SessionsPlugin;
/// use polaris::sessions::store::memory::InMemoryStore;
/// use polaris::system::server::Server;
/// use polaris_ontrace::{ExtractionPlugin, StdinIOPlugin, TraceIngestPlugin};
/// use std::sync::Arc;
///
/// # async fn wire() -> Result<(), Box<dyn std::error::Error>> {
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(ModelsPlugin)
///     .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(TraceIngestPlugin::new())  // publishes TraceIngestAPI + TraceIngestRx
///     .add_plugins(ExtractionPlugin::new())   // backend: claims TraceIngestRx
///     .add_plugins(StdinIOPlugin::new());      // source: requires TraceIngestAPI
/// server.finish().await?;
/// # Ok(())
/// # }
/// ```
pub struct StdinIOPlugin;

impl StdinIOPlugin {
    /// Construct the plugin. Holds no configuration.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for StdinIOPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for StdinIOPlugin {
    const ID: &'static str = "ontrace::sources::stdin";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, _server: &mut Server) {}

    async fn ready(&self, server: &mut Server) {
        let tx = server
            .api::<TraceIngestAPI>()
            .expect("StdinIOPlugin requires TraceIngestPlugin")
            .sender();
        let provider = StdinProvider::new();
        tokio::spawn(drive(provider, tx));
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TraceIngestAPI>(VersionReq::caret(TraceIngestAPI::CONTRACT_VERSION))
    }
}

async fn drive(provider: StdinProvider, tx: tokio::sync::mpsc::Sender<IOMessage>) {
    loop {
        match provider.receive().await {
            Ok(message) => {
                if tx.send(message).await.is_err() {
                    tracing::info!("trace channel closed; stdin source exiting");
                    return;
                }
            }
            Err(IOError::Closed) => {
                tracing::info!("stdin EOF; source exiting");
                return;
            }
            Err(err) => {
                tracing::warn!(error = %err, "stdin read error; source exiting");
                return;
            }
        }
    }
}
