//! Terminal-side coding policy behind the `polaris-tui` conversation panel:
//! slash commands, and `@` file mentions over the working directory.
//!
//! `polaris-tui` owns the presentation — the popup, its keys, the wire
//! protocol — and deliberately ships no command set and no file-system rules.
//! This module supplies both for the coding agent. Command handlers run locally;
//! a command that needs the agent's graph hands it an [`AgentRequest`]:
//!
//! | Piece | Role |
//! |-------|------|
//! | [`CommandRegistry`] | The `/commands` the terminal runs locally. Ships `/help`; other features register theirs. |
//! | [`AgentRequest`] | What a command hands the agent's input system when its effect belongs to the agent's graph. The terminal never learns what one means; the capability that registered the command reads it. |
//! | [`CommandDispatch`] | An [`IOProvider`](polaris::plugins::IOProvider) decorator that executes dispatched commands and answers them **before** the agent's `receive` returns, advertises the set to the panel, and validates `@` mentions on ordinary prompts crossing the channel. |
//! | [`WorkspaceFiles`] | The [`CompletionSource`](polaris_tui::commands::CompletionSource) for `@` mentions: a cancellable, `.gitignore`-aware walk of the working directory, ranked against the query. |
//! | [`mentions`] | Resolves `@path` mentions against the working directory and refuses any that escape it. [`ContainedPrompt`] is the proof it hands out; [`user_message`] takes nothing else. |
//! | [`TurnSteering`] | The host-owned turn-input queue. It holds contained prompts, so a submission's mentions are resolved at ingress and a refusal reaches the composer while the draft is still there. |
//! | [`WorkspaceAttachments`] | The [`AttachmentSource`](polaris_tui::commands::AttachmentSource) behind the composer's chips: which mentioned files are images, how big they may be, and whether the session's model can read one. |
//! | [`review`] | `/diff`: one read-only aggregate of every file this session changed, built from the edit journal and rendered by the transcript's existing diff colouring. |
//! | [`background`] | Managed shell runs pushed out of a turn — by the user's `Ctrl-T` chord or by the model's own [`run_in_background`](crate::tools::TaskTools) call: the table that remembers which run owns which tool block, the sink that folds its output, `/tasks`, `/task`, `/fg`, `/stop`, and the bounded ring of finished results both read back. |
//! | [`TopologyWork`] | `/topology`: one exact coding edit submitted to the topology runtime's queue, gated by the same posture `edit_file` resolves to, and the turn report that says which of the three answers came back. |
//! | [`history`] | Read-only pages of the canonical conversation, refreshed between turns. |
//! | [`transcript`] | The live-transcript vocabulary the agent streams its answer and tool activity through. |
//!
//! [`attach`] wires commands, workspace completion, attachments, transcript
//! events, turn steering, and status onto a [`ConversationPanelPlugin`] in one call;
//! the binary's composition root is the only place that needs to know how.
//!
//! # What never happens
//!
//! A `/command` never becomes a canonical user prompt. The panel dispatches it as a
//! tagged message, [`CommandDispatch::receive`](CommandDispatch) consumes it
//! and loops for local replies. A command that answers with an [`AgentRequest`]
//! instead returns a private control message: for `/compact`,
//! `receive_user_input` requests a summary without appending the command or its
//! focus to history, and the graph skips the normal answer call. An
//! unknown command is answered with an error naming `/help`, not forwarded.
//! A prompt mentioning a path outside the working directory is refused, not
//! forwarded either: the tools are sandboxed there and could not act on it.
//! That holds on **every** path a prompt can be submitted by, not only the
//! channel [`CommandDispatch`] decorates. The guarantee is a type rather than
//! a step: the model is only ever asked about a prompt assembled by
//! [`user_message`], which takes a [`ContainedPrompt`],
//! and the only way to make one is to resolve the mentions. A submit path
//! that forgets does not compile. Where the refusal *surfaces* still differs
//! by path — the composer keeps the draft when the turn-input queue refuses
//! at ingress, the channel answers the prompt with an error and keeps
//! waiting, and a front end with no decorator at all ends the turn with the
//! reason rather than calling the model.

pub mod attachments;
pub mod background;
pub mod commands;
pub mod dispatch;
pub mod files;
pub mod history;
pub mod ignore;
pub mod mentions;
pub mod notify;
pub mod review;
pub mod rewind;
pub mod sessions;
pub mod settings;
pub mod status;
pub mod steering;
pub mod topologies;
pub mod topology;
pub mod transcript;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use polaris::plugins::{IOProvider, UserIO};
use polaris_tui::nav::Key;
use polaris_tui::panels::{ConversationPanelPlugin, TuiIOProvider};
use polaris_tui::ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::run::StatusEmitter;

pub use attachments::{
    ATTACHMENT_BYTES_MAX, AttachmentStore, ClipboardImages, StoredAttachment, WorkspaceAttachments,
    model_accepts_images, user_message,
};
pub use background::{
    BACKGROUND_DEFAULT_TIMEOUT_SECS, BackgroundCommands, BackgroundReporter, BackgroundTasks,
    FINISHED_RETAINED, FinishedTask, collect_result,
};
pub use commands::{
    AgentRequest, CommandContext, CommandError, CommandFuture, CommandHandler, CommandOutcome,
    CommandRegistry,
};
pub use dispatch::CommandDispatch;
pub use files::WorkspaceFiles;
pub use ignore::IgnoreRules;
pub use mentions::{ContainedPrompt, META_MENTIONS, MentionRejected};
pub use review::{ChangeSet, ChangeStatus, FileChange, SessionReview};
pub use rewind::{
    EditJournal, PathOutcome, RewindApplication, RewindApplicationOutcome, RewindControl,
    RewindMode, RewindPoint, RewindRequest, TrackedPath, TrackedWindow,
};
pub use sessions::{SessionAction, SessionNavigation};
pub use settings::{SessionSettings, SettingsSnapshot};
pub use steering::{SteerRefused, TurnPrompt, TurnSteering};
pub use topologies::TopologyListing;
pub use topology::{AcceptedWork, PreparedWork, TopologyWork, WorkRefused};
pub use transcript::{LiveTranscript, MessageHandle, ToolActivity, forward};

/// A handle for host-originated messages to the conversation panel.
#[derive(Clone, Default)]
pub struct TerminalOutput {
    provider: Arc<std::sync::RwLock<Option<Arc<TuiIOProvider>>>>,
}

impl TerminalOutput {
    /// Send a presentation event after the panel provider has been built.
    pub async fn send(&self, message: polaris::plugins::IOMessage) -> Result<(), String> {
        let provider = self
            .provider
            .read()
            .map_err(|err| err.to_string())?
            .clone()
            .ok_or("conversation panel output is not ready")?;
        provider.send(message).await.map_err(|err| err.to_string())
    }
}

/// Wire commands and file mentions onto the conversation panel.
///
/// The panel's provider is wrapped in a [`CommandDispatch`] over `registry`
/// so agents resolve a [`UserIO`] that runs commands locally, and a
/// [`WorkspaceFiles`] source rooted at `working_dir` supplies `@` mention
/// candidates. Call it on the plugin before adding it to the server.
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris_code::terminal::{CommandRegistry, attach};
/// use polaris_tui::panels::ConversationPanelPlugin;
///
/// let registry = Arc::new(CommandRegistry::with_builtins());
/// let panel = attach(
///     ConversationPanelPlugin::default(),
///     registry,
///     std::env::current_dir().unwrap(),
/// );
/// # let _ = panel;
/// ```
#[must_use]
pub fn attach(
    panel: ConversationPanelPlugin,
    registry: Arc<CommandRegistry>,
    working_dir: impl AsRef<Path>,
) -> ConversationPanelPlugin {
    attach_with_output(panel, registry, working_dir).0
}

/// Wire terminal policy and also return a host output handle for session replay.
#[must_use]
pub fn attach_with_output(
    panel: ConversationPanelPlugin,
    registry: Arc<CommandRegistry>,
    working_dir: impl AsRef<Path>,
) -> (ConversationPanelPlugin, TerminalOutput) {
    decorate(panel, registry, working_dir, None)
}

/// Wire the terminal integrations, expose the panel provider as a run-status
/// sink, and return the same host output handle [`attach_with_output`] gives.
///
/// Session replay and run-status delivery both need the provider the panel
/// builds, and the panel takes a single IO decorator, so one call has to hand
/// back both handles.
#[must_use]
pub fn attach_with_status(
    panel: ConversationPanelPlugin,
    registry: Arc<CommandRegistry>,
    working_dir: impl AsRef<Path>,
    status: Arc<std::sync::OnceLock<StatusEmitter>>,
) -> (ConversationPanelPlugin, TerminalOutput) {
    decorate(panel, registry, working_dir, Some(status))
}

/// The one decorator behind `attach*`: command dispatch, `@` mention
/// candidates, the Shift-Tab permission-mode key, the replay output handle,
/// and — when asked for — the status sink.
fn decorate(
    panel: ConversationPanelPlugin,
    registry: Arc<CommandRegistry>,
    working_dir: impl AsRef<Path>,
    status: Option<Arc<std::sync::OnceLock<StatusEmitter>>>,
) -> (ConversationPanelPlugin, TerminalOutput) {
    let working_dir: PathBuf = working_dir.as_ref().to_path_buf();
    let files = WorkspaceFiles::new(working_dir.clone());
    let output = TerminalOutput::default();
    let capture = output.clone();
    let panel = panel
        .with_completion_source(Arc::new(files))
        // Shift-Tab steps the permission mode. The panel honours the key only
        // while `/permission` is advertised, and otherwise leaves it to the
        // shell (backward panel focus).
        .with_key_command(
            Key::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            "/permission cycle",
        )
        .with_io_decorator(move |provider: Arc<TuiIOProvider>| {
            if let Ok(mut slot) = capture.provider.write() {
                *slot = Some(provider.clone());
            }
            if let Some(status) = &status {
                let _ = status.set(StatusEmitter::new(provider.clone()));
            }
            UserIO::new(Arc::new(CommandDispatch::new(
                provider,
                registry.clone(),
                working_dir.clone(),
            )))
        });
    (panel, output)
}
