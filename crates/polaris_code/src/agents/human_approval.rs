//! Optional human approval transport. Policy decides whether to ask before this
//! capability is consulted; an absent capability can never grant permission.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use polaris::plugins::{IOError, UserIO};
use polaris::system::plugin::{Plugin, Version};
use polaris::system::resource::GlobalResource;
use polaris::system::server::Server;

use super::permission_rules::{ApprovalAnswer, ApprovalError, ApprovalRequest, SessionGrants};

/// Transport for a single human decision and cancellation of its pending prompt.
///
/// Implementations must report a deliberate human choice and resolve a
/// cancelled prompt before returning from [`Self::cancel`]. The interactive
/// implementation uses [`ApprovalRequest`]'s
/// existing confirmation protocol.
pub trait HumanApprover: Send + Sync + 'static {
    /// Present the exact request and return its scope and optional note or refusal.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::Denied`] when the person refuses, and
    /// [`ApprovalError::Prompt`] when the prompt channel fails.
    fn ask<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        io: &'a UserIO,
        prompt: String,
    ) -> BoxFuture<'a, Result<ApprovalAnswer, ApprovalError>>;

    /// Resolve a prompt whose wait was interrupted, before another input receive.
    ///
    /// # Errors
    ///
    /// Returns an [`IOError`] when the channel fails before the resolution is
    /// delivered. A refusal does not apply to cancellation.
    fn cancel<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        io: &'a UserIO,
    ) -> BoxFuture<'a, Result<(), IOError>>;
}

/// The host's optional human approval capability.
///
/// Use it from a system when authorization has left a call needing a person:
/// it asks for a decision over the session's input channel and resolves a
/// prompt that was interrupted.
///
/// # Scope
///
/// Global transport shared across sessions. Each request receives the session's
/// own input channel and grants; the resource stores no pending session state.
///
/// # Provided By
///
/// [`InteractiveApprovalPlugin`], or an embedding host using [`Self::new`].
/// Neither the agent nor its permission plugin installs a default approver.
///
/// # Access
///
/// Read as `Option<Res<HumanApproval>>`. Absence denies any call that still
/// needs a person after deterministic authorization and automatic review.
///
/// [`Self::ask`] re-checks the scope the transport returns against the choices
/// the request offered, and may update the supplied [`SessionGrants`]. A
/// replacement [`HumanApprover`] must be `Send + Sync` and handle its own
/// shared state.
///
/// # Alternatives
///
/// Deterministic permission modes and explicit run grants settle calls before
/// human approval. [`HumanApprover`] permits another human input transport.
///
/// # Example
///
/// ```
/// use polaris::system::{param::Res, system};
/// use polaris_code::agents::human_approval::HumanApproval;
/// #[system]
/// async fn inspect(approval: Option<Res<HumanApproval>>) {
///     tracing::debug!(available = approval.is_some());
/// }
/// ```
#[derive(Clone)]
pub struct HumanApproval(Arc<dyn HumanApprover>);
impl GlobalResource for HumanApproval {}

impl std::fmt::Debug for HumanApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HumanApproval(..)")
    }
}

impl HumanApproval {
    /// Bind a replacement human input transport at assembly time.
    pub fn new(approver: impl HumanApprover) -> Self {
        Self(Arc::new(approver))
    }

    /// Ask the registered transport for an explicitly offered scope and optional note.
    ///
    /// # Errors
    ///
    /// Returns the person's refusal or an approval-channel error.
    pub async fn ask(
        &self,
        request: &ApprovalRequest,
        io: &UserIO,
        prompt: String,
        grants: &SessionGrants,
    ) -> Result<ApprovalAnswer, ApprovalError> {
        let answer = self.0.ask(request, io, prompt).await?;
        request.remember_scope(grants, answer.scope)?;
        Ok(answer)
    }

    /// Resolve an interrupted prompt before the next input receive.
    ///
    /// # Errors
    ///
    /// Returns the transport error if resolution cannot be delivered.
    pub async fn cancel(&self, request: &ApprovalRequest, io: &UserIO) -> Result<(), IOError> {
        self.0.cancel(request, io).await
    }
}

struct InteractiveApprover;
impl HumanApprover for InteractiveApprover {
    fn ask<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        io: &'a UserIO,
        prompt: String,
    ) -> BoxFuture<'a, Result<ApprovalAnswer, ApprovalError>> {
        Box::pin(request.receive_answer(io, prompt))
    }

    fn cancel<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        io: &'a UserIO,
    ) -> BoxFuture<'a, Result<(), IOError>> {
        Box::pin(request.cancel(io))
    }
}

/// Supplies human approval over each session's existing [`UserIO`] channel.
///
/// Install it when a host runs attended sessions and wants a person to confirm
/// tool calls over the input channel it already has. Omit it for headless runs.
/// The approval contract is described in the
/// [shell approval guide](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/shell-approval.md).
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`HumanApproval`] | Global | Confirmation transport, offered scopes and prompt cancellation. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | The plugin publishes only the [`HumanApproval`] resource; it provides no API of its own. |
///
/// # Dependencies
///
/// None. The caller supplies the session's `UserIO` and
/// [`SessionGrants`](crate::agents::permission_rules::SessionGrants) when
/// asking. The plugin does not create another input receive boundary.
///
/// # Lifecycle
///
/// `build` publishes the transport. It starts no tasks and stores no grants.
/// Omit this plugin to deny calls that need a human; permission modes that
/// allow calls outright continue to work.
///
/// # Example
///
/// ```
/// use polaris::system::server::Server;
/// use polaris_code::agents::human_approval::InteractiveApprovalPlugin;
/// let mut server = Server::new();
/// server.add_plugins(InteractiveApprovalPlugin);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct InteractiveApprovalPlugin;
impl Plugin for InteractiveApprovalPlugin {
    const ID: &'static str = "polaris_code::interactive_approval";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server.insert_global(HumanApproval::new(InteractiveApprover));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::permission_rules::ApprovalScope;
    use polaris::plugins::{IOMessage, IOProvider};
    use serde_json::json;

    struct UnavailableScope;
    impl HumanApprover for UnavailableScope {
        fn ask<'a>(
            &'a self,
            _: &'a ApprovalRequest,
            _: &'a UserIO,
            _: String,
        ) -> BoxFuture<'a, Result<ApprovalAnswer, ApprovalError>> {
            Box::pin(async { Ok(ApprovalScope::Exact.into()) })
        }
        fn cancel<'a>(
            &'a self,
            _: &'a ApprovalRequest,
            _: &'a UserIO,
        ) -> BoxFuture<'a, Result<(), IOError>> {
            Box::pin(async { Ok(()) })
        }
    }
    struct NoInput;
    impl IOProvider for NoInput {
        async fn send(&self, _: IOMessage) -> Result<(), IOError> {
            Ok(())
        }
        async fn receive(&self) -> Result<IOMessage, IOError> {
            Err(IOError::Closed)
        }
    }

    #[tokio::test]
    async fn a_replacement_transport_cannot_remember_an_unoffered_scope() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"command": "cargo test"});
        let request = ApprovalRequest::new("run_command", &args, dir.path(), 1, false);
        let approval = HumanApproval::new(UnavailableScope);
        let grants = SessionGrants::default();
        let answer = approval
            .ask(
                &request,
                &UserIO::new(Arc::new(NoInput)),
                "Run?".into(),
                &grants,
            )
            .await;
        assert!(matches!(
            answer,
            Err(ApprovalError::Denied { ref reason })
                if reason == "Confirmation selected an unavailable approval scope"
        ));
        assert!(!grants.allows(1, "run_command", &args, dir.path()));
    }
}
