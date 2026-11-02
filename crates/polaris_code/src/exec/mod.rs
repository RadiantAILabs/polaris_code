//! Non-interactive front-end IO: a channel-backed [`UserIO`] used for exactly
//! one headless turn.
//!
//! [`ExecIOPlugin`] registers [`UserIO`] as a local resource exactly the way
//! `polaris-tui`'s `ConversationPanelPlugin` does — a plain `server.register_local`
//! factory, not a per-turn override — so the non-interactive front end
//! installs it *instead of* the TUI plugins, never alongside them.
//! [`ExecIOProvider`] is the bounded-channel [`IOProvider`] behind it,
//! structurally identical to `polaris-ai`'s `HttpIOProvider`: the caller
//! enqueues the turn's one prompt on the input channel before the turn starts
//! (`receive_user_input` swallows a closed channel rather than erroring, so a
//! late send would hang forever) and drains the output channel — directly, or
//! around a [`crate::run::drive_turn`] call — as `IOMessage`s arrive.
//!
//! [`report`] is the other half of the front end: what the run writes when the
//! turn is over. It holds the `--output-format` vocabulary, the run's
//! machine-readable token and cost accounting, and the only stdout-producing
//! code in this crate, so every format's rendering is testable without running
//! the binary.

pub mod models;
pub mod report;

pub use models::ModelsReport;
pub use report::{
    InventoryReport, OutputFormat, RunReport, TopologySelectionReport, USAGE_IO_TYPE,
    UnknownOutputFormat, stream_json_line, usage_message,
};

use std::sync::{Arc, Mutex};

use polaris::plugins::{IOError, IOMessage, IOProvider, UserIO};
use polaris::system::plugin::{Plugin, Version};
use polaris::system::server::Server;
use tokio::sync::mpsc;

/// Channel-based [`IOProvider`] for the non-interactive front end.
///
/// Both channels are bounded for the same reason `HttpIOProvider`'s are: a
/// misbehaving agent (or a front end that stops draining) cannot drive
/// unbounded memory growth. The input channel only ever carries one message
/// in practice — one headless invocation drives exactly one turn — but is a
/// channel rather than a single slot so `send`/`receive` stay the plain
/// [`IOProvider`] shape every other provider in this codebase already uses.
pub struct ExecIOProvider {
    input_rx: tokio::sync::Mutex<mpsc::Receiver<IOMessage>>,
    output_tx: Mutex<Option<mpsc::Sender<IOMessage>>>,
}

impl ExecIOProvider {
    /// Creates a provider with a single-slot input channel and the given
    /// output-channel capacity, plus the input sender and output receiver the
    /// front end keeps to enqueue the prompt and drain the response.
    #[must_use]
    pub fn new(output_buffer: usize) -> (Self, mpsc::Sender<IOMessage>, mpsc::Receiver<IOMessage>) {
        let (input_tx, input_rx) = mpsc::channel(1);
        let (output_tx, output_rx) = mpsc::channel(output_buffer);
        let provider = Self {
            input_rx: tokio::sync::Mutex::new(input_rx),
            output_tx: Mutex::new(Some(output_tx)),
        };
        (provider, input_tx, output_rx)
    }
}

impl IOProvider for ExecIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        // Clone the sender out of the mutex so the guard is never held across
        // the bounded `send().await` below, which would otherwise block a
        // std::sync::Mutex guard across an await point.
        let tx = {
            let guard = self
                .output_tx
                .lock()
                .expect("ExecIOProvider output lock poisoned");
            guard.as_ref().ok_or(IOError::Closed)?.clone()
        };
        tx.send(message).await.map_err(|_| IOError::Closed)
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.input_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or(IOError::Closed)
    }

    async fn close(&self) {
        self.output_tx
            .lock()
            .expect("ExecIOProvider output lock poisoned")
            .take();
    }
}

/// Plugin that registers [`UserIO`] over a shared [`ExecIOProvider`] as a
/// local resource — install this *instead of* the TUI panel plugins for the
/// non-interactive front end, never alongside them.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`UserIO`] | Local | Cloned from the shared [`ExecIOProvider`] this plugin was built with. |
///
/// # APIs Provided
///
/// _none_ — the caller keeps its own input sender / output receiver from
/// [`ExecIOProvider::new`] rather than reaching them back through the server.
///
/// # Dependencies
///
/// None.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris::system::server::Server;
/// use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
///
/// let (provider, _input_tx, _output_rx) = ExecIOProvider::new(64);
/// let mut server = Server::new();
/// server.add_plugins(ExecIOPlugin::new(Arc::new(provider)));
/// ```
pub struct ExecIOPlugin {
    provider: Arc<ExecIOProvider>,
}

impl ExecIOPlugin {
    /// Wrap an already-constructed [`ExecIOProvider`] so its owner keeps the
    /// input sender / output receiver [`ExecIOProvider::new`] returned.
    #[must_use]
    pub fn new(provider: Arc<ExecIOProvider>) -> Self {
        Self { provider }
    }
}

impl Plugin for ExecIOPlugin {
    const ID: &'static str = "polaris_code::exec_io";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let factory_provider = self.provider.clone();
        server.register_local(move || UserIO::new(factory_provider.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::plugins::IOContent;

    #[tokio::test]
    async fn enqueued_prompt_is_received_once_then_closed() {
        let (provider, input_tx, _output_rx) = ExecIOProvider::new(8);
        input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text("hi".into()),
            ))
            .await
            .unwrap();
        drop(input_tx);

        let received = provider.receive().await.unwrap();
        assert!(matches!(received.content, IOContent::Text(ref t) if t == "hi"));
        assert!(matches!(provider.receive().await, Err(IOError::Closed)));
    }

    #[tokio::test]
    async fn close_ends_the_output_stream() {
        let (provider, _input_tx, mut output_rx) = ExecIOProvider::new(8);
        provider
            .send(IOMessage::system_text("first"))
            .await
            .unwrap();
        provider.close().await;
        assert!(provider.send(IOMessage::system_text("late")).await.is_err());
        assert!(output_rx.recv().await.is_some());
        assert!(output_rx.recv().await.is_none());
    }
}
