//! Optional observation of model attempts and completed turns.
//!
//! These handles let a host record execution without replacing the model or
//! copying the turn driver. No observer is installed by default.
//!
//! Prefer Polaris's graph hooks and middleware for graph/system lifecycle
//! instrumentation, and `TokenMeter` for aggregate usage. These handles cover
//! information those boundaries do not expose: partial billed tokens kept inside
//! a model attempt, and the driver's classified outcome after graph execution
//! and session processing. The ATIF recorder writes full requests to a file; it
//! does not offer an in-memory subscription for another recorder.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use polaris::models::llm::Message;
use polaris::system::resource::GlobalResource;

use crate::run::TurnOutcome;

/// Billed usage and completion status of one model attempt.
#[derive(Debug, Clone)]
pub struct ModelAttempt {
    /// Whether a response was collected successfully.
    pub ok: bool,
    /// Full-price input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens read from cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to cache.
    pub cache_creation_tokens: u64,
    /// Provider stop reason, when a response was collected.
    pub stop_reason: Option<String>,
}

type ObserveModel = dyn Fn(&str, ModelAttempt) + Send + Sync;

/// A synchronous attempt observer configured on [`super::AgentConfig`].
///
/// Called after each collected attempt, including failed attempts, before the
/// retry policy decides what happens next. Callbacks must be quick and must not
/// panic. Cancellation before collection produces no observation. Clones share
/// the callback. Several consumers can be composed inside a callback.
#[derive(Clone)]
pub struct ModelObserver(Arc<ObserveModel>);

impl fmt::Debug for ModelObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ModelObserver(..)")
    }
}

impl ModelObserver {
    /// Observe completed attempts; `model` is the resolved provider/model id.
    pub fn new(observe: impl Fn(&str, ModelAttempt) + Send + Sync + 'static) -> Self {
        Self(Arc::new(observe))
    }

    pub(crate) fn observe(&self, model: &str, attempt: ModelAttempt) {
        (self.0)(model, attempt);
    }
}

type ObserveTurn = dyn Fn(Vec<Message>, TurnOutcome) -> Pin<Box<dyn Future<Output = TurnOutcome> + Send>>
    + Send
    + Sync;

/// Optional completed-turn recorder and report projection.
///
/// # Scope
///
/// **Global**, supplied by a host plugin with `server.insert_global`. The stock
/// host supplies none. One handle is supported; compose consumers in its callback.
///
/// # Provided By
///
/// A host's recording plugin, using `server.insert_global`; no stock plugin
/// provides this optional resource.
///
/// # Access
///
/// `Res<TurnObserver>` reads a shared immutable callback handle. The driver
/// discovers it through the session context; there is no mutable system access.
/// The shared turn driver calls it once after the turn has released its context,
/// on completion, failure, limits or interruption, before any front end reports
/// or persists the result. It receives canonical history and the original
/// outcome. A callback can record both, then return a report with amended text.
/// Keep execution identity, reason and accounting intact. Callbacks must finish
/// promptly, cooperate with cancellation for long work, and must not panic.
///
/// # Alternatives
///
/// Use Polaris's `OnGraphComplete`/`OnGraphFailure` hooks or graph middleware
/// when graph completion is sufficient. Those run before session errors,
/// recovery failures and host spending outcomes are classified, and middleware
/// deliberately erases the continuation result. Use `TokenMeter` for aggregate
/// usage or the ATIF recorder for full requests.
///
/// # Example
///
/// ```
/// use polaris_code::agents::observation::TurnObserver;
/// use polaris::system::server::Server;
/// let mut server = Server::new();
/// server.insert_global(TurnObserver::new(|history, outcome| async move {
///     tracing::debug!(messages = history.len(), "turn completed");
///     outcome
/// }));
/// ```
#[derive(Clone)]
pub struct TurnObserver(Arc<ObserveTurn>);

impl GlobalResource for TurnObserver {}

impl TurnObserver {
    /// Create an observer called by every shared-driver turn on this server.
    pub fn new<F, Fut>(observe: F) -> Self
    where
        F: Fn(Vec<Message>, TurnOutcome) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = TurnOutcome> + Send + 'static,
    {
        Self(Arc::new(move |history, outcome| {
            Box::pin(observe(history, outcome))
        }))
    }

    pub(crate) async fn observe(&self, history: Vec<Message>, outcome: TurnOutcome) -> TurnOutcome {
        (self.0)(history, outcome).await
    }
}
