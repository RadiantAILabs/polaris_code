//! File-tail trace source — `tail -F` over a newline-delimited file.
//!
//! [`FileTailIOPlugin`] watches an append-mostly file (e.g. an agent's
//! session log), starting from byte 0 so any pre-existing history is
//! extracted before live appends are picked up. Reopens the file every
//! poll cycle to pick up truncation and rotation without depending on
//! inotify/FSEvents.

use crate::plugins::trace_ingest::{META_TRACE_ID, TraceIngestAPI};
use polaris::plugins::{IOError, IOMessage, IOProvider};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::mpsc;

/// How often the tail loop polls the file for new bytes.
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// [`IOProvider`] that tails a newline-delimited file.
///
/// Each [`receive`](IOProvider::receive) returns the next complete line.
/// Partial trailing lines (writer hasn't flushed the `\n`) are deferred to
/// the next poll. [`send`](IOProvider::send) returns [`IOError::Unsupported`].
pub struct FileTailProvider {
    path: PathBuf,
    /// Outbound queue, populated by [`pull_more`].
    queue: Mutex<std::collections::VecDeque<IOMessage>>,
    state: Mutex<TailState>,
    /// Optional trace identifier stamped onto every emitted [`IOMessage`]'s
    /// metadata under [`META_TRACE_ID`].
    trace_id: Option<String>,
}

#[derive(Default)]
struct TailState {
    /// Byte offset already consumed. Starts at 0 so the initial poll
    /// backfills any pre-existing content before catching up to live appends.
    position: u64,
}

impl FileTailProvider {
    /// Construct a provider for the given path. The file does not need to
    /// exist yet — [`receive`](IOProvider::receive) will retry until it does.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            queue: Mutex::new(std::collections::VecDeque::new()),
            state: Mutex::new(TailState::default()),
            trace_id: None,
        }
    }

    /// Stamp every emitted [`IOMessage`] with `id` under the [`META_TRACE_ID`]
    /// metadata key. Downstream the ingest dispatcher lifts this onto
    /// [`crate::resources::trace_line::TraceLine::trace_id`], so the agent and
    /// the KG can attribute each observation to its source stream.
    #[must_use]
    pub fn with_trace_id(mut self, id: impl Into<String>) -> Self {
        self.trace_id = Some(id.into());
        self
    }

    /// Pop one buffered line if available, returning `None` if the queue is empty.
    fn pop(&self) -> Option<IOMessage> {
        self.queue
            .lock()
            .expect("FileTailProvider queue poisoned")
            .pop_front()
    }

    /// One poll cycle: open the file, detect truncation, seek to the last
    /// known position, and read every complete line into the queue.
    /// Returns `true` if any progress was made (file is at >= position).
    async fn pull_more(&self) -> Result<bool, IOError> {
        let file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(IOError::Provider(err.to_string())),
        };
        let size = file
            .metadata()
            .await
            .map_err(|e| IOError::Provider(e.to_string()))?
            .len();

        let mut position = {
            let mut state = self.state.lock().expect("FileTailProvider state poisoned");
            if size < state.position {
                tracing::info!(
                    path = %self.path.display(),
                    old = state.position,
                    new = size,
                    "tail: file shrunk, resetting",
                );
                state.position = 0;
            }
            state.position
        };

        if size == position {
            return Ok(false);
        }

        let mut file = file;
        file.seek(SeekFrom::Start(position))
            .await
            .map_err(|e| IOError::Provider(e.to_string()))?;
        let mut reader = BufReader::new(file);

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(n) => {
                    if !line.ends_with('\n') {
                        // Partial line — re-read these bytes next cycle.
                        break;
                    }
                    position += n as u64;
                    let trimmed = line.trim_end_matches(['\n', '\r']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    let mut msg = IOMessage::user_text(trimmed.to_string());
                    if let Some(id) = &self.trace_id {
                        msg = msg.with_metadata(META_TRACE_ID, id);
                    }
                    self.queue
                        .lock()
                        .expect("FileTailProvider queue poisoned")
                        .push_back(msg);
                }
                Err(err) => return Err(IOError::Provider(err.to_string())),
            }
        }

        self.state
            .lock()
            .expect("FileTailProvider state poisoned")
            .position = position;
        Ok(true)
    }
}

impl IOProvider for FileTailProvider {
    async fn send(&self, _message: IOMessage) -> Result<(), IOError> {
        Err(IOError::Unsupported("file tail is read-only".into()))
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        loop {
            if let Some(msg) = self.pop() {
                return Ok(msg);
            }
            match self.pull_more().await {
                Ok(true) => continue,
                Ok(false) => tokio::time::sleep(TAIL_POLL_INTERVAL).await,
                Err(err) => {
                    tracing::warn!(error = %err, "tail: pull failed; retrying");
                    tokio::time::sleep(TAIL_POLL_INTERVAL).await;
                }
            }
        }
    }
}

/// Source plugin that tails a newline-delimited file into the trace pipeline.
///
/// In [`ready`](Plugin::ready) it spawns a long-lived background task that
/// drives a [`FileTailProvider`] with `tail -F` semantics — reading from byte
/// 0 so pre-existing history is extracted before live appends — wraps each
/// complete line as an [`IOMessage`], and pushes it into
/// [`TraceIngestAPI`](crate::TraceIngestAPI). The configured extraction
/// backend drains those messages into knowledge-graph triples.
///
/// Reach for this to follow an agent's append-mostly session log (or any
/// growing NDJSON file) while it is still being written, without coupling to
/// inotify/FSEvents. The file need not exist at registration time — the tail
/// loop retries until it appears. Optionally call
/// [`with_trace_id`](Self::with_trace_id) to attribute every emitted line to a
/// named source stream.
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
/// sender and `tokio::spawn`s a task that drives a [`FileTailProvider`] in a
/// loop, forwarding every complete line into the ingest channel. The task runs
/// for the life of the server. Each poll cycle reopens the file and compares
/// its size against the last consumed offset, so the loop survives truncation
/// and rotation: a file that shrank below the saved position resets to byte 0
/// and re-reads from the top. Partial trailing lines (no `\n` yet) are
/// deferred to the next poll. The file is read-only, so the provider's
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
/// use polaris_ontrace::{ExtractionPlugin, FileTailIOPlugin, TraceIngestPlugin};
/// use std::path::PathBuf;
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
///     .add_plugins(
///         FileTailIOPlugin::new(PathBuf::from("/tmp/agent.jsonl"))
///             .with_trace_id("agent-session-1"),
///     );
/// server.finish().await?;
/// # Ok(())
/// # }
/// ```
pub struct FileTailIOPlugin {
    path: PathBuf,
    trace_id: Option<String>,
}

impl FileTailIOPlugin {
    /// Construct a plugin that tails the file at `path`. The file does not
    /// need to exist at construction time.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            trace_id: None,
        }
    }

    /// Stamp every emitted [`IOMessage`] with `id` under the [`META_TRACE_ID`]
    /// metadata key. See [`FileTailProvider::with_trace_id`].
    #[must_use]
    pub fn with_trace_id(mut self, id: impl Into<String>) -> Self {
        self.trace_id = Some(id.into());
        self
    }
}

impl Plugin for FileTailIOPlugin {
    const ID: &'static str = "ontrace::sources::file_tail";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, _server: &mut Server) {}

    async fn ready(&self, server: &mut Server) {
        let tx = server
            .api::<TraceIngestAPI>()
            .expect("FileTailIOPlugin requires TraceIngestPlugin")
            .sender();
        let mut provider = FileTailProvider::new(self.path.clone());
        if let Some(id) = &self.trace_id {
            provider = provider.with_trace_id(id.clone());
        }
        tokio::spawn(drive(provider, tx));
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TraceIngestAPI>(VersionReq::caret(TraceIngestAPI::CONTRACT_VERSION))
    }
}

async fn drive(provider: FileTailProvider, tx: mpsc::Sender<IOMessage>) {
    loop {
        match provider.receive().await {
            Ok(message) => {
                if tx.send(message).await.is_err() {
                    tracing::info!("trace channel closed; file tail exiting");
                    return;
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "tail: fatal read error");
                return;
            }
        }
    }
}
