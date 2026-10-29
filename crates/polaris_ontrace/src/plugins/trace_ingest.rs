//! [`TraceIngestPlugin`] — the fan-in dispatcher between source plugins and
//! extraction backends.
//!
//! Owns the mpsc channel that every source plugin (stdin, file tail, OTLP,
//! Claude Code) pushes into and that exactly one extraction backend
//! ([`crate::ExtractionPlugin`] or [`crate::OxigraphExtractionPlugin`])
//! drains.
//!
//! Splitting the channel out of the backend plugins decouples both sides:
//! source plugins declare a single, stable [`Plugin`] dependency on
//! [`TraceIngestPlugin`] and work unchanged regardless of which backend is
//! wired up. Backends grab the receiver via [`TraceIngestRx::take`] in their
//! `ready`.
//!
//! # APIs provided
//!
//! | API | Used by | Description |
//! |-----|---------|-------------|
//! | [`TraceIngestAPI`] | sources | `push` [`IOMessage`]s into the channel; cheaply clonable sender. |
//! | [`TraceIngestRx`]  | backend | One-shot handoff of the receiver. Second `take` panics — only one backend may drain the channel. |
//!
//! # Wiring
//!
//! Register the dispatcher *before* the backend plugin:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use polaris::plugins::PersistencePlugin;
//! # use polaris::sessions::SessionsPlugin;
//! # use polaris::sessions::store::memory::InMemoryStore;
//! # use polaris::system::server::Server;
//! # use polaris_ontrace::{ExtractionPlugin, StdinIOPlugin, TraceIngestPlugin};
//! # async fn example() {
//! let mut server = Server::new();
//! server
//!     .add_plugins(PersistencePlugin)
//!     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
//!     .add_plugins(TraceIngestPlugin::new())
//!     .add_plugins(ExtractionPlugin::new())
//!     .add_plugins(StdinIOPlugin::new());
//! server.finish().await.unwrap();
//! # }
//! ```

use crate::resources::trace_line::TraceLine;
use polaris::plugins::{IOContent, IOMessage};
use polaris::system::api::API;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::server::Server;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Default mpsc channel capacity between source plugins and the extraction
/// worker.
const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Fan-in dispatcher: owns the trace-ingest mpsc channel between source
/// plugins and the extraction backend.
///
/// Register this whenever you wire one or more transport sources (stdin, file
/// tail, OTLP, Claude Code) to an extraction backend. It decouples the two
/// sides — sources push into [`TraceIngestAPI`]; exactly one backend claims
/// the receiver via [`TraceIngestRx::take`] — so the source set is indifferent
/// to which backend is wired up. Register it *before* the backend plugin.
///
/// # Resources Provided
///
/// _None._ The channel endpoints are published as APIs (see below), not as
/// `GlobalResource`s; the sender lives in [`TraceIngestAPI`] and the receiver
/// in [`TraceIngestRx`].
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`TraceIngestAPI`] | Cheaply clonable sender; source plugins `push` [`IOMessage`]s into the channel. |
/// | [`TraceIngestRx`] | One-shot handoff of the receiver; the single extraction backend `take`s it in `ready`. |
///
/// # Dependencies
///
/// None — [`access`](Plugin::access) declares no `requires`. It only
/// `provides` [`TraceIngestAPI`] and [`TraceIngestRx`] (by capability), so the
/// resolver can order requiring plugins (sources, the backend) after it and
/// verify both APIs exist at [`Server::finish`].
///
/// # Lifecycle
///
/// [`build`](Plugin::build) inserts both APIs synchronously — it moves the
/// receiver out of the plugin's `Mutex<Option<…>>`, so calling `build` twice
/// panics. No background tasks are spawned here; sources spawn their own
/// pushers and the backend spawns the draining worker.
///
/// # Example
///
/// ```no_run
/// # use std::sync::Arc;
/// # use polaris::plugins::PersistencePlugin;
/// # use polaris::sessions::SessionsPlugin;
/// # use polaris::sessions::store::memory::InMemoryStore;
/// # use polaris::system::server::Server;
/// # use polaris_ontrace::{ExtractionPlugin, StdinIOPlugin, TraceIngestPlugin};
/// # async fn example() {
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(TraceIngestPlugin::new())
///     .add_plugins(ExtractionPlugin::new())
///     .add_plugins(StdinIOPlugin::new());
/// server.finish().await.unwrap();
/// # }
/// ```
pub struct TraceIngestPlugin {
    tx: mpsc::Sender<IOMessage>,
    rx: Mutex<Option<mpsc::Receiver<IOMessage>>>,
}

impl TraceIngestPlugin {
    /// Construct with the default channel capacity (256).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CHANNEL_CAPACITY)
    }

    /// Construct with a custom channel capacity.
    ///
    /// A smaller bound applies backpressure on upstream sources sooner; a
    /// larger bound smooths over bursts at the cost of memory.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }
}

impl Default for TraceIngestPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for TraceIngestPlugin {
    const ID: &'static str = "ontrace::trace_ingest";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Both APIs are inserted in `build()` below, so they satisfy the
        // post-build `provides` verification. Sources requiring `TraceIngestAPI`
        // and the backend requiring `TraceIngestRx` resolve to this provider
        // by type, regardless of which backend is wired up.
        PluginAccess::new()
            .provides::<TraceIngestAPI>(TraceIngestAPI::CONTRACT_VERSION)
            .provides::<TraceIngestRx>(TraceIngestRx::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_api(TraceIngestAPI::from_sender(self.tx.clone()));
        let rx = self
            .rx
            .lock()
            .expect("TraceIngestPlugin rx lock poisoned")
            .take()
            .expect("TraceIngestPlugin::build called twice");
        server.insert_api(TraceIngestRx::new(rx));
    }
}

/// Sender half of the trace-ingest channel, exposed to source plugins.
///
/// Holds a clone of the mpsc sender; cheap to fetch and clone per source.
/// Fetch it in a source plugin's `ready` to forward inbound [`IOMessage`]s to
/// the extraction backend.
///
/// # Provided by
///
/// [`TraceIngestPlugin`].
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`push`](Self::push) | `async`; send one [`IOMessage`] into the channel. `Err` (with the message back, boxed) once the worker has shut down. |
/// | [`sender`](Self::sender) | Clone the underlying [`mpsc::Sender`] for pushing from a detached background task without holding the API. |
///
/// # Lifecycle
///
/// Available after [`TraceIngestPlugin`] runs `build`. Sources typically fetch
/// it in their own `ready`. `push` succeeds while the backend's draining
/// worker is alive and returns `Err` after it has dropped the receiver.
///
/// # Composition
///
/// Open extension — any number of source plugins may fetch this API and push
/// concurrently into the shared channel. The sender is `Clone`, so fan-in is
/// unbounded by design.
///
/// # Example consumers
///
/// [`crate::StdinIOPlugin`], [`crate::FileTailIOPlugin`], and
/// [`crate::OtlpIOPlugin`] each fetch this API in `ready` and push their
/// decoded records into it.
///
/// # Example
///
/// Provider side ([`TraceIngestPlugin::build`] inserts the API):
///
/// ```ignore
/// server.insert_api(TraceIngestAPI::from_sender(self.tx.clone()));
/// ```
///
/// Consumer side (a source plugin):
///
/// ```no_run
/// # use polaris::plugins::IOMessage;
/// # use polaris::system::server::Server;
/// # use polaris_ontrace::TraceIngestAPI;
/// # async fn example(server: &Server) {
/// let api = server.api::<TraceIngestAPI>().expect("TraceIngestPlugin not registered");
/// api.push(IOMessage::user_text("alice met bob")).await.ok();
/// # }
/// ```
pub struct TraceIngestAPI {
    tx: mpsc::Sender<IOMessage>,
}

impl API for TraceIngestAPI {}

impl Contract for TraceIngestAPI {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl TraceIngestAPI {
    /// Construct from a pre-built mpsc sender.
    pub(crate) fn from_sender(tx: mpsc::Sender<IOMessage>) -> Self {
        Self { tx }
    }

    /// Push a single message into the extractor's input channel.
    ///
    /// # Errors
    ///
    /// Returns `Err` (with the message back) if the extraction worker has
    /// shut down and is no longer receiving.
    ///
    /// The [`SendError`](mpsc::error::SendError) is boxed because it owns the
    /// returned [`IOMessage`], which is large enough that an inline `Err`
    /// variant would widen every `push` result — the success case included —
    /// to the size of a whole trace record. Callers recover the message the
    /// same way as from an unboxed send: `err.0`.
    pub async fn push(
        &self,
        message: IOMessage,
    ) -> Result<(), Box<mpsc::error::SendError<IOMessage>>> {
        self.tx.send(message).await.map_err(Box::new)
    }

    /// Clone of the underlying sender, for sources that push from a
    /// background task without holding an API reference.
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<IOMessage> {
        self.tx.clone()
    }
}

/// Receiver half of the trace-ingest channel, exposed to the extraction
/// backend.
///
/// The one backend in the server calls [`take`](Self::take) once in its
/// `ready` to claim ownership of the receiver and drive the draining worker.
///
/// # Provided by
///
/// [`TraceIngestPlugin`].
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`take`](Self::take) | Claim ownership of the [`mpsc::Receiver`]. Panics if called twice — only one backend may drain the channel. |
///
/// # Lifecycle
///
/// Available after [`TraceIngestPlugin`] runs `build`. A backend calls `take`
/// exactly once in its `ready`. A second `take` — whether the same backend
/// retries or a second backend is registered — panics, because the receiver
/// has already been moved out.
///
/// # Composition
///
/// Provider-scoped, single-claimant. Exactly one consumer may `take` the
/// receiver; this enforces the "one extraction backend" invariant at the type
/// level rather than relying on convention.
///
/// # Example consumers
///
/// [`crate::ExtractionPlugin`] (in-memory `KnowledgeGraph`) and
/// [`crate::OxigraphExtractionPlugin`] (Oxigraph store) each `take` the
/// receiver in `ready`; registering both panics on the second claim.
///
/// # Example
///
/// Provider side ([`TraceIngestPlugin::build`] inserts the API):
///
/// ```ignore
/// server.insert_api(TraceIngestRx::new(rx));
/// ```
///
/// Consumer side (the extraction backend, in `ready`):
///
/// ```no_run
/// # use polaris::system::server::Server;
/// # use polaris_ontrace::plugins::trace_ingest::TraceIngestRx;
/// # async fn example(server: &Server) {
/// let rx = server
///     .api::<TraceIngestRx>()
///     .expect("TraceIngestPlugin not registered")
///     .take();
/// // spawn a worker that drains `rx`…
/// drop(rx);
/// # }
/// ```
pub struct TraceIngestRx {
    inner: Mutex<Option<mpsc::Receiver<IOMessage>>>,
}

impl API for TraceIngestRx {}

impl Contract for TraceIngestRx {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl TraceIngestRx {
    pub(crate) fn new(rx: mpsc::Receiver<IOMessage>) -> Self {
        Self {
            inner: Mutex::new(Some(rx)),
        }
    }

    /// Claim ownership of the trace-ingest receiver.
    ///
    /// # Panics
    ///
    /// Panics if called twice on the same server — only one extraction
    /// backend may drain the channel.
    #[must_use]
    pub fn take(&self) -> mpsc::Receiver<IOMessage> {
        self.inner
            .lock()
            .expect("TraceIngestRx mutex poisoned")
            .take()
            .expect(
                "TraceIngestRx: receiver already claimed — only one extraction backend may be registered",
            )
    }
}

/// Convert an inbound [`IOMessage`] into a [`TraceLine`] for the agent.
///
/// Text content passes through as-is; structured content is re-serialized to
/// JSON so the LLM sees a single string per trace. Binary content is
/// skipped — the extractor has no use for it.
///
/// Optional metadata keys [`META_TRACE_ID`] and [`META_OBSERVED_AT`] override
/// the defaults (we leave `trace_id = None` and stamp `observed_at = now` if
/// absent).
pub(crate) fn trace_line_from_message(message: &IOMessage) -> Option<TraceLine> {
    let text = match &message.content {
        IOContent::Text(s) => s.clone(),
        IOContent::Structured(v) => v.to_string(),
        IOContent::Binary { .. } => return None,
    };
    let trace_id = message.metadata.get(META_TRACE_ID).cloned();
    let observed_at = message
        .metadata
        .get(META_OBSERVED_AT)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(chrono::Utc::now, |dt| dt.with_timezone(&chrono::Utc));
    Some(TraceLine {
        text,
        observed_at,
        trace_id,
    })
}

/// Metadata key on incoming [`IOMessage`]s for an upstream trace identifier
/// (propagated onto every emitted KG triple).
pub const META_TRACE_ID: &str = "trace_id";

/// Metadata key on incoming [`IOMessage`]s for an observation timestamp
/// (RFC 3339). Falls back to wall-clock now if missing or unparseable.
pub const META_OBSERVED_AT: &str = "observed_at";
