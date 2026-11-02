//! [`CommandDispatch`] — the [`IOProvider`] decorator that runs `/commands`
//! before the agent ever sees them.
//!
//! The coding agent's graph starts each turn by awaiting `UserIO::receive`
//! and pushing whatever text arrives into the model's history. That is the
//! wrong place for a command: `/help` is a local action, not a prompt, and
//! must never become a model request. Rather than teach the agent about
//! commands, this wrapper sits between the panel's provider and the `UserIO`
//! the agent resolves. Its `receive` loops: a command message is executed
//! against the [`CommandRegistry`] and answered through the inner `send`,
//! then it keeps waiting; an ordinary prompt has its `@` mentions checked
//! and is returned. A command that answers with an
//! [`AgentRequest`](super::commands::AgentRequest) instead
//! returns it to the agent as private control data, without a reply and
//! without entering canonical history as a user prompt; the dispatcher does not
//! know what any request means.
//!
//! The wrapper also advertises the command set to the panel on the first
//! `receive` — the earliest moment the panel is listening — so the `/`
//! popup knows what to offer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use polaris::plugins::{IO_TYPE, IOError, IOMessage, IOProvider, IOSource, IOStream};
use polaris_tui::commands::{CommandInvocation, advertise_commands};

use super::commands::{CommandOutcome, CommandRegistry};
use super::mentions;

const HOST_CONTROL_TYPE: &str = "polaris-code:host-control";
const SESSION_SWITCH_CONTROL: &str = "session-switch";

fn session_switch_signal() -> IOMessage {
    IOMessage::system_text(SESSION_SWITCH_CONTROL).with_metadata(IO_TYPE, HOST_CONTROL_TYPE)
}

/// Whether `message` is the private control signal that releases an idle turn.
pub(crate) fn is_session_switch_signal(message: &IOMessage) -> bool {
    message.source == IOSource::System
        && message.metadata.get(IO_TYPE).map(String::as_str) == Some(HOST_CONTROL_TYPE)
        && matches!(&message.content, polaris::plugins::IOContent::Text(text) if text == SESSION_SWITCH_CONTROL)
}

/// [`IOProvider`] decorator that executes `/commands` locally and validates
/// `@` mentions, forwarding everything else to `inner`.
pub struct CommandDispatch<P> {
    inner: Arc<P>,
    registry: Arc<CommandRegistry>,
    working_dir: PathBuf,
    advertised: AtomicBool,
}

impl<P: IOProvider> CommandDispatch<P> {
    /// Wrap `inner`. `working_dir` bounds `@` mentions.
    #[must_use]
    pub fn new(inner: Arc<P>, registry: Arc<CommandRegistry>, working_dir: PathBuf) -> Self {
        Self {
            inner,
            registry,
            working_dir,
            advertised: AtomicBool::new(false),
        }
    }

    /// The wrapped provider.
    #[must_use]
    pub fn inner(&self) -> &Arc<P> {
        &self.inner
    }

    /// Send the command advertisement once. Idempotent; a failed send is
    /// retried on the next `receive`, since it means the panel is not
    /// listening yet.
    async fn advertise(&self) -> Result<(), IOError> {
        if self.advertised.load(Ordering::Acquire) {
            return Ok(());
        }
        self.inner
            .send(advertise_commands(&self.registry.specs()))
            .await?;
        self.advertised.store(true, Ordering::Release);
        Ok(())
    }
}

impl<P: IOProvider> IOProvider for CommandDispatch<P> {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.inner.send(message).await
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.advertise().await?;
        loop {
            let message = self.inner.receive().await?;
            if let Some(invocation) = CommandInvocation::from_message(&message) {
                let outcome = self.registry.dispatch(&invocation).await;
                if let CommandOutcome::Request(request) = outcome {
                    return Ok(request.into_message());
                }
                let ends_turn = outcome.ends_turn();
                self.inner
                    .send(outcome.into_message(&invocation.name))
                    .await?;
                if ends_turn {
                    return Ok(session_switch_signal());
                }
                continue;
            }
            match mentions::validate(message, &self.working_dir) {
                Ok(message) => return Ok(message),
                Err(rejected) => {
                    self.inner.send(rejected.into_message()).await?;
                }
            }
        }
    }

    async fn stream(&self) -> Result<IOStream, IOError> {
        self.inner.stream().await
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}
