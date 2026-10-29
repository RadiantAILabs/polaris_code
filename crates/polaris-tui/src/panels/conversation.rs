//! Chat-style conversation panel wired to Polaris [`UserIO`].
//!
//! Mirrors the shape of
//! [`polaris::sessions::http::HttpIOProvider`](https://docs.rs/polaris-ai/latest/polaris_ai/sessions/http/struct.HttpIOProvider.html):
//! a [`TuiIOProvider`] hands the agent two `tokio::sync::mpsc` channels — one
//! for messages flowing *from* the agent (rendered as transcript) and one
//! *to* the agent (filled by the input prompt). Both halves can be cheaply
//! cloned because they live behind `Arc`.
//!
//! # Wiring
//!
//! ```no_run
//! use std::sync::Arc;
//! use polaris::plugins::UserIO;
//! use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
//! use polaris::system::server::Server;
//! use polaris_tui::TuiRegistry;
//! use polaris_tui::panels::{ConversationPanel, TuiIOProvider};
//!
//! struct MyConversationPlugin;
//!
//! impl Plugin for MyConversationPlugin {
//!     const ID: &'static str = "myapp::conversation_panel";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!
//!     fn access(&self) -> PluginAccess {
//!         PluginAccess::new()
//!             .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
//!     }
//!
//!     fn build(&self, server: &mut Server) {
//!         let (provider, panel) = TuiIOProvider::new_with_panel(64);
//!         let provider = Arc::new(provider);
//!         let user_io_provider = provider.clone();
//!         server.register_local(move || UserIO::new(user_io_provider.clone()));
//!         server
//!             .api::<TuiRegistry>()
//!             .expect("TuiPlugin provides TuiRegistry; the resolver orders it first")
//!             .add_panel(panel);
//!     }
//! }
//! ```
//!
//! # Status & token protocol
//!
//! Beyond the transcript, the panel renders an optional one-line activity
//! indicator (a spinner + the current step, with a running token tally). It is
//! driven entirely by [`IOMessage`] metadata, so it stays agnostic of any
//! particular agent — emit these and the line appears; emit nothing and the
//! panel is a plain transcript + input (the meta row collapses to zero height):
//!
//! | `metadata["io_type"]` | Effect |
//! |-----------------------|--------|
//! | `"status"` | Sets the activity line to the message's text; empty text clears it to idle. A `status_report` payload instead refreshes host-owned identity, projected context, cost, and availability without changing activity. Not added to the transcript. |
//! | `"interrupting"` | Host raised the interrupt signal: append an `Interrupting` row and keep the activity line busy. |
//! | `"cancelled"` | Host confirms the turn stopped: append a `Cancelled` row and return to idle. |
//! | `"tokens"` | Adds `metadata["in"]` / `metadata["out"]` (parsed as integers) to the cumulative session tally. Not added to the transcript. |
//! | `"confirm"` | Opens an approval dialog (see below). The message itself — the command or edit preview — *is* added to the transcript. |
//! | `"error"` | Always renders as content. A system-sourced error also latches the meta row visibly until the host reports availability. |
//! | `"message"` | An assistant message block. With an `"id"`, updates sharing it fold into **one** transcript block that grows in place (see below); without one, an ordinary standalone message. |
//! | `"tool"` | A tool activity block. With an `"id"`, later updates replace the block in place so one invocation stays one row through its phases. |
//!
//! # Live blocks — one message, one tool, one row each
//!
//! A host that streams its answer sends the same block many times; the panel
//! folds those updates so the user reads a message that grows rather than a
//! column of fragments. Folding is opt-in: it happens only when `"id"` is
//! present, so a consumer that sends whole messages is unaffected.
//!
//! | `metadata` key | On | Meaning |
//! |----------------|-----|---------|
//! | `"id"` | both | The block's stable identity. Its presence is what makes an update fold rather than append. |
//! | `"seq"` | `message` | Monotonic per `"id"`. An update whose `seq` does not advance the block is **dropped** — that is what stops a resent or reordered delivery from printing the same text twice. |
//! | `"final"` | `message` | `"true"` closes the block. A non-empty body on the closing update *replaces* what the deltas accumulated, reconciling a lost delta; an empty one keeps the accumulation, which is how a failed stream retains its partial text. |
//! | `"outcome"` | `message` | Why the block ended. `"error"` marks it as having stopped before the answer did. |
//! | `"parent"` | `tool` | The `message` block that requested it; the tool renders indented beneath it. |
//! | `"phase"` | `tool` | `pending`, `start`, `ok`, `denied` or `error`, rendered as a glyph. An unrecognised value still renders, so a host may extend the lifecycle. |
//! | `"label"` | `tool` | The invocation's short name. The panel prints it and knows nothing about what the tool does. |
//! | `"elapsed_ms"` | `tool` | Wall time, shown once the block reaches a terminal phase. |
//!
//! A non-final `message` body is a **delta** — the new text only — so a
//! producer forwards what its provider emits without buffering the answer.
//!
//! Tool and approval bodies are previewed rather than dumped: a long result
//! collapses to a bounded block with a `Ctrl-O` affordance, and nothing is
//! discarded to do it.
//!
//! The key is polaris core's [`IO_TYPE`] (`"io_type"`), reused rather than
//! forked, so one message-kind vocabulary spans the framework and this panel.
//! The panel's own values (`status`, `tokens`) sit alongside core's `confirm`
//! on the same channel.
//!
//! Absent `io_type` stays plain text. `message`, `tool`, `confirm` and `error`
//! plus `interrupting` and `cancelled` are visible content; other types are retained and revealed with Ctrl-O.
//! A withheld count appears in the meta line. A producer
//! typically sends a `status` before a long step and a `tokens` delta after each
//! LLM call; see `polaris_code_core`'s `ProgressMiddlewarePlugin` for a reference driver.
//!
//! # Interrupting a turn
//!
//! [`ConversationPanelPlugin::with_interrupt_handle`] accepts a non-blocking
//! host callback. Ctrl-C denies an open approval first, clears a draft next,
//! then requests interruption when a non-empty host `status` indicates work.
//! It invokes the handle at most once until the host reports idle or cancelled.
//! A second Ctrl-C within 1500 ms falls through to the shell; Ctrl-Q quits.
//! Without a handle, the existing approval/draft/quit behaviour is unchanged.
//!
//! The callback requests work; it does not prove the signal was raised. The
//! host sends `interrupting` when raised and `cancelled` when stopped, through
//! ordinary [`UserIO`] messages. Only those messages produce the corresponding
//! transcript rows. No timer infers completion, and this crate kills no process.
//! The host must send statuses in turn order, ending with `cancelled` or an
//! empty `status`, before sending the next turn's work status.
//!
//! # Backgrounding the live work
//!
//! [`ConversationPanelPlugin::with_background_handle`] accepts a second
//! non-blocking host callback, bound to `Ctrl-T` and useful while a tool block
//! is live. It is the interrupt contract with a different verb: the panel
//! forwards the press and learns nothing back, so it never finds out what is
//! running, which work may be backgrounded, or whether the host agreed. Every
//! consequence — a receipt, or a typed refusal such as "nothing is running to
//! background" — is an ordinary host message. Without a handle the chord is
//! unbound and behaviour is unchanged.
//!
//! `Ctrl-T` rather than the Unix `Ctrl-Z` because this panel is reusable: a
//! host that has not put the terminal in raw mode would have `Ctrl-Z` suspend
//! the process instead of delivering a key. A host that does own raw mode may
//! offer `Ctrl-Z` itself, on top of this.
//!
//! A backgrounded invocation typically keeps its block and moves to
//! `phase = "background"`, which needs nothing here: an unrecognised phase
//! already renders. How many tasks are running is a separate, optional fact on
//! the meta row — [`StatusReport::with_tasks_running`](crate::commands::StatusReport::with_tasks_running)
//! — so work pushed off-screen stays discoverable.
//!
//! # Approvals
//!
//! [`UserIO::confirm`] — what a tool whose permission resolves to
//! `ToolPermission::Confirm` calls — sends an `io_type = "confirm"` request and
//! takes the next inbound message as the answer, reading its
//! [`CONFIRMED`] metadata and treating an absent value as denial. The panel
//! implements exactly that contract, and nothing more:
//!
//! - a `confirm` request opens a **pending dialog**: the prompt enters the
//!   transcript under an `approve ❯` label (so the command or edit being
//!   approved is on screen), and a banner above the input names the keys that
//!   resolve it;
//! - while a dialog is open, an unambiguous `y`/`yes`/`approve`/`allow`/`ok`
//!   or `n`/`no`/`deny`/`reject`/`cancel` — or `Esc`, which denies — is
//!   delivered stamped with [`CONFIRMED`], and only that answer is;
//! - anything else stays an ordinary prompt. So does anything typed *before*
//!   the request arrived. [`TuiIOProvider`] holds such prompts back for the
//!   next ordinary [`receive`](IOProvider::receive) rather than letting one
//!   answer for the user — which would both deny the tool and swallow the
//!   prompt;
//! - a plain "yes" typed with no dialog open is just a message, and an answer
//!   that cannot be delivered leaves the dialog open. Only a stamped, delivered
//!   approval runs the tool.
//!
//! The panel decides nothing about *which* actions need approval — that is the
//! tool's permission, set by the consumer. This is presentation and the answer
//! protocol only.
//!
//! # The composer
//!
//! The input is a `Composer`: a multi-line buffer whose caret is a byte
//! offset held on a grapheme boundary and measured in **display columns**, so
//! CJK, emoji and combining marks put it under the glyph the user sees rather
//! than one cell off per wide character.
//!
//! - `Enter` submits the whole draft; `Shift+Enter` (and `Alt`/`Super+Enter`)
//!   inserts a newline. Those chords reach the panel only where the terminal
//!   speaks the kitty keyboard protocol — the runtime negotiates it at startup
//!   — so a trailing `\` continues a line on every other terminal. That
//!   fallback is the only way to reach a second line in stock Terminal.app,
//!   which cannot report *any* modifier with `Enter`.
//! - A bracketed [`Paste`](TuiEvent::Paste) is inserted whole and never
//!   interpreted: its newlines do not submit and a leading `/` does not
//!   dispatch. CRLF is normalised and C0 controls are dropped, so pasted text
//!   cannot smuggle an escape sequence into the terminal.
//! - The frame grows with the draft up to ten rows and then
//!   scrolls internally, always leaving the transcript at least one row.
//! - `↑`/`↓` move between rows, walking host-supplied history at the first
//!   and last row when configured with [`ConversationPanelPlugin::with_history`].
//!   `Esc` restores the draft and caret after browsing.
//! - `Ctrl-R` searches history incrementally; another `Ctrl-R` finds an older
//!   match. `Enter` accepts the match into the draft without submitting it;
//!   `Esc`, `Ctrl-G` or `Ctrl-C` restores the draft from before search.
//! - `Ctrl-G` outside search opens VISUAL/EDITOR (or a host-supplied
//!   [`EditorCommand`]). The runtime hands over the terminal and restores it
//!   afterward. Success replaces the draft; failure leaves text and caret intact.
//!   Editor output uses the same newline/control normalization as pasted text.
//!
//! # Commands and file mentions
//!
//! The panel ships no command set. A host advertises the `/commands` it
//! executes with [`commands::advertise_commands`](crate::commands::advertise_commands)
//! — a `status` message carrying a `commands` list, which the panel caches
//! without touching the activity line — and supplies `@` mention candidates
//! through a [`CompletionSource`] given to
//! [`ConversationPanelPlugin::with_completion_source`]. Until a host does
//! either, `/` and `@` are ordinary text and no popup opens.
//!
//! - `/` at the start of the draft opens the command popup, filtered as the
//!   name is typed. On `Enter`, a draft starting with `/` is dispatched as a
//!   user message stamped `io_type = "command"` with `name` and `args`
//!   metadata ([`CommandInvocation`]) and echoed locally like a prompt. It is
//!   never sent as a prompt, so it cannot reach a model by accident; an
//!   unknown name is dispatched too, because only the host knows its set and
//!   the host is what reports it. A host runs commands before the agent sees
//!   them by wrapping the provider with
//!   [`ConversationPanelPlugin::with_io_decorator`].
//! - `@` at a word boundary opens the file popup. Each keystroke cancels the
//!   query in flight and starts another on a task; a result carrying an
//!   older sequence number than the current query is dropped, so the rows
//!   always belong to what was typed last. Accepted paths are inserted with
//!   spaces backslash-escaped so a mention stays one token.
//! - While a popup is open the panel claims `Tab` (accept), `Shift-Tab`,
//!   `↑`/`↓` (move the highlight) and `Esc` (close until the draft changes).
//!   `Enter` fills in a partial match and submits once the query spells the
//!   highlighted entry exactly. With no popup, every one of those keys keeps
//!   its usual meaning — `Tab` falls through to the shell's focus cycling —
//!   and accepting or dismissing changes nothing outside the token.
//! - The popup draws on the bottom rows of the transcript, above the meta
//!   line, and never covers the approval or delivery banners.
//!
//! # Input delivery
//!
//! The inbound channel is bounded, so a submission can be refused — the agent
//! is behind, or gone. The panel never lets that pass as success:
//!
//! - the draft is cleared only once the channel has **accepted** the message.
//!   A refusal leaves the input text and cursor exactly as they were, so Enter
//!   retries the same prompt; `try_send` hands a refused message back rather
//!   than queueing it, so a retry cannot deliver a second copy;
//! - a refused prompt is **not** echoed to the transcript. Showing it would
//!   present a message the agent will never see as delivered;
//! - a banner above the input says which it was — a full inbox (retryable) or
//!   a closed channel (not) — and never blocks the render loop to say it;
//! - an accepted prompt is only *queued*. It carries a `· queued` marker until
//!   the agent's [`receive`](IOProvider::receive) actually takes it, so
//!   "the channel took it" never reads as "the agent read it".
//!
//! # Node timeline (Ctrl-O)
//!
//! On top of the status line, a `status` message may carry two extra metadata
//! keys that build a per-turn timeline of the graph nodes that ran — generic
//! over any graph, since they name nothing agent-specific:
//!
//! | `metadata` key | Meaning |
//! |----------------|---------|
//! | `"node"` | The graph system's node name. A non-empty status with this key appends a timeline entry; an empty status with it (idle) arms a reset so the next turn starts fresh. |
//! | `"seq"` | Per-turn sequence index, disambiguating repeated invocations of the same node across loop iterations. |
//!
//! **Ctrl-O** expands diff previews, reveals retained unknown messages, and
//! appends those nodes in order after the transcript. When the
//! panel is given a shared [`LogBuffer`](crate::panels::LogBuffer) (see
//! [`ConversationPanelPlugin::with_log_buffer`]), each node also shows the
//! `tracing` events emitted within its span — entries whose `(node, seq)` match
//! — so a node's output is whatever it logged, with no special message type.
//! Only framework events (`polaris_*` / `polaris::*` targets) are shown;
//! dependency and consumer-code noise that merely ran under a node span is
//! filtered out.

#[path = "../transcript/navigation.rs"]
mod navigation;
pub use navigation::{HistoryFuture, HistoryPage, TranscriptHistory};

use crate::commands::{
    Attachment, AttachmentRejected, AttachmentRequest, AttachmentSource, Availability, CancelToken,
    CommandInvocation, CommandSpec, Completion, CompletionPopup, CompletionSource, Cost,
    IO_TYPE_SESSION, META_SESSION_ACTION, POPUP_MAX_ROWS, PopupNotice, SESSION_REPLAY_END,
    SESSION_REPLAY_START, StatusReport, Trigger, active_command_argument_token, active_token,
    attachment_handles, parse_advertisement, parse_mentions, parse_status, stamp_attachments,
};
use crate::composer::Composer;
use crate::composer::editor::edit_draft;
use crate::composer::history::{History, replace};
use crate::event::TuiEvent;
use crate::nav::{KeyMap, NavIntent};
use crate::panel::{EventOutcome, TuiPanel};
use crate::panels::LogBuffer;
use crate::panels::logs::FRAMEWORK_TARGET_PREFIX;
use crate::registry::TuiRegistry;
use crate::transcript::{body_lines, preview_lines, safe_text};
use crate::widgets::ScrollableTranscript;
use crate::{EditorCommand, PromptHistoryConfig, TuiHandle};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use parking_lot::Mutex;
use polaris::plugins::{
    CONFIRMED, CONFIRMED_FALSE, CONFIRMED_TRUE, IO_TYPE, IO_TYPE_CONFIRM, IOContent, IOError,
    IOMessage, IOProvider, IOSource, UserIO,
};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, mpsc, oneshot};

/// Where a submitted message should enter the host's turn lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnInputMode {
    /// Hold the message for the next run.
    Queue,
    /// Offer the message to the active run at its next safe boundary.
    Steer,
}

/// Host-confirmed identity and target for a queued or steering message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInputReceipt {
    /// Host identity used by edit and remove operations.
    pub id: u64,
    /// One-based run number that will consume the message.
    pub target_run: u64,
    /// Whether the target is the next run or a safe boundary in the active run.
    pub mode: TurnInputMode,
}

/// Host-owned queue and steering contract used by the conversation panel.
///
/// Methods are synchronous because key handling runs on the UI task. Hosts must
/// only mutate bounded in-memory state and wake their run driver; model, tool,
/// persistence, and network work belongs behind that driver.
pub trait TurnInputHandle: Send + Sync {
    /// Submit `text`, returning the run the host promises will consume it.
    fn submit(&self, text: String, mode: TurnInputMode) -> Result<TurnInputReceipt, String>;
    /// Submit `text` together with the handles of the attachments its composer
    /// chips carry, in chip order.
    ///
    /// Defaulted to [`submit`](Self::submit) because a host that does not
    /// implement it also does not supply an
    /// [`AttachmentSource`](crate::commands::AttachmentSource) — and without
    /// one the panel can never grow a chip, so the list it would be handed is
    /// always empty. Overriding this is what makes attachments reach the
    /// model; a host that supplies a source and leaves this defaulted would
    /// drop them, which is why the two go together.
    fn submit_with_attachments(
        &self,
        text: String,
        mode: TurnInputMode,
        attachments: Vec<String>,
    ) -> Result<TurnInputReceipt, String> {
        let _ = attachments;
        self.submit(text, mode)
    }
    /// Replace a message that has not yet been consumed.
    fn replace(&self, id: u64, text: String) -> Result<(), String>;
    /// Remove a message that has not yet been consumed.
    fn remove(&self, id: u64) -> Result<(), String>;
}

#[derive(Debug, Clone)]
struct PendingTurnInput {
    receipt: TurnInputReceipt,
    text: String,
}

const TRANSCRIPT_CAP: usize = 1024;

/// Metadata key the sender stamps to mark a message's kind. Reuses polaris
/// core's [`IO_TYPE`] convention (`"io_type"`) rather than forking a new key,
/// so one message-kind vocabulary spans the framework and this panel: the
/// panel's own values (`status`, `tokens`) sit alongside core's `confirm`,
/// which it also answers (see [`TYPE_CONFIRM`]).
const META_TYPE: &str = IO_TYPE;
/// Ephemeral activity update: replaces the status line rather than appending to
/// the transcript. Empty text clears it (returns the line to "ready").
const TYPE_STATUS: &str = "status";
/// Token-usage delta: accumulated into the running tally, never shown inline.
const TYPE_TOKENS: &str = "tokens";
/// Agent or system failure. A content type: it always renders in the
/// transcript, and — when it comes from [`IOSource::System`], i.e. the host
/// speaking about itself rather than the model's own output — it also latches
/// the meta row into its error state so a dead turn cannot read as "ready".
const TYPE_ERROR: &str = "error";
/// Host acknowledgement that the interrupt signal was raised, not completion.
const TYPE_INTERRUPTING: &str = "interrupting";
/// Host confirmation that the interrupted turn has actually stopped.
const TYPE_CANCELLED: &str = "cancelled";
/// Approval request: a tool is parked in [`UserIO::confirm`] waiting for a
/// yes/no. Core's [`IO_TYPE_CONFIRM`] value, reused rather than forked, so the
/// panel speaks the protocol `confirm` already defines instead of a second one.
const TYPE_CONFIRM: &str = IO_TYPE_CONFIRM;
/// Assistant message block. Messages sharing a [`META_ID`] fold into one entry
/// that grows in place, so a streamed answer is one block, not one block per
/// token.
const TYPE_MESSAGE: &str = "message";
/// Tool activity block. Messages sharing a [`META_ID`] update one entry in
/// place, so an invocation is one row that changes phase, not four rows.
const TYPE_TOOL: &str = "tool";
/// Metadata key carrying a `message` or `tool` block's stable identity. Its
/// presence is what makes a message an update to an existing block rather than
/// a new one; without it a message is standalone.
const META_ID: &str = "id";
/// Metadata key marking the closing message of a `message` block.
const META_FINAL: &str = "final";
/// Metadata key naming the `message` block a `tool` belongs to, for nesting.
const META_PARENT: &str = "parent";
/// Metadata key carrying a `tool` block's lifecycle phase.
const META_PHASE: &str = "phase";
/// Metadata key carrying a `tool` block's short human name.
const META_LABEL: &str = "label";
/// Metadata key carrying a `tool` block's wall-clock duration in milliseconds.
const META_ELAPSED_MS: &str = "elapsed_ms";
/// Metadata key carrying why a `message` block ended.
const META_OUTCOME: &str = "outcome";
/// [`META_FINAL`] value marking a block closed.
const FINAL_TRUE: &str = "true";
/// [`META_OUTCOME`] value marking a block that ended before the answer did.
const OUTCOME_ERROR: &str = "error";
/// Columns a tool block is indented by under the message that requested it.
const TOOL_INDENT: usize = 2;
/// Metadata key naming the graph node a `status` message belongs to. Its
/// presence turns a status into a node-timeline entry.
const META_NODE: &str = "node";
/// Metadata key carrying the per-turn sequence index of a `node`.
const META_SEQ: &str = "seq";
/// Metadata key on a `tokens` message carrying the input-token delta.
const META_IN: &str = "in";
/// Metadata key on a `tokens` message carrying the output-token delta.
const META_OUT: &str = "out";
/// Metadata key stamped on the panel's *local echo* of a submitted prompt,
/// carrying its submission number. Compared against the provider's consumed
/// count to tell a prompt still waiting in the channel from one the agent has
/// taken. Panel-private: the copy sent to the agent never carries it.
const META_SUBMISSION: &str = "submission";
/// Rows scrolled per mouse-wheel notch. One row per event feels glacial;
/// three matches the de-facto terminal convention.
const WHEEL_LINES: usize = 3;
/// How long after a `Ctrl-C` the panel keeps declining the next one.
///
/// The first press acts inside the panel (deny a dialog, clear a draft); a
/// second press within this window falls through to the shell and quits. The
/// window exists so the escape hatch is always at most two presses away, no
/// matter what the panel is doing. `Ctrl-Q` remains a single-press exit
/// throughout, so nobody depends on double-press timing to leave.
const INTERRUPT_ESCALATION: Duration = Duration::from_millis(1500);
/// Rows the composer may grow to before it stops growing and scrolls its own
/// contents. Tall enough for a pasted function, short enough that the
/// transcript never collapses to nothing.
const COMPOSER_MAX_ROWS: usize = 10;
/// Composer width assumed before the first frame has measured the real one.
const DEFAULT_COMPOSER_WIDTH: usize = 80;
/// Braille spinner frames cycled while a step is in flight.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Default mpsc buffer for [`TuiIOProvider::new`] when the caller doesn't
/// have a specific backpressure target in mind.
pub const DEFAULT_CHANNEL_BUFFER: usize = 64;

/// An [`IOProvider`] backed by two `tokio::mpsc` channels and shared with a
/// [`ConversationPanel`].
///
/// The agent half (the `IOProvider` impl) sends *to* the user and receives
/// *from* the user. The panel half does the opposite — it appends agent
/// messages to its transcript and forwards user keystrokes (on Enter) into
/// the input channel.
pub struct TuiIOProvider {
    /// Agent → user: agent calls `send`, panel reads from `out_rx`.
    out_tx: mpsc::Sender<IOMessage>,
    /// User → agent: panel pushes here, agent's `receive` pulls.
    ///
    /// Wrapped in a `tokio::sync::Mutex` because the agent's `receive`
    /// awaits inside the critical section — a `parking_lot::Mutex` here
    /// would block runtime threads.
    in_rx: tokio::sync::Mutex<mpsc::Receiver<IOMessage>>,
    /// Wakes the panel when a new outbound message lands. Lets the panel
    /// flush its inbox on every tick *and* immediately on FocusGained
    /// without polling at the tick rate.
    out_notify: Arc<Notify>,
    /// Set while a `confirm` request has been sent and its answer has not yet
    /// been received. [`UserIO::confirm`] is precisely "send a `confirm`
    /// request, then take the next message", so without this latch the next
    /// message — whatever it is — becomes the answer.
    awaiting_confirm: AtomicBool,
    /// Ordinary prompts pulled off the channel while `awaiting_confirm` was
    /// set. They arrived before the answer but are not the answer, so they wait
    /// here, in order, for the next ordinary [`receive`](IOProvider::receive)
    /// rather than being consumed (and denied) by the dialog.
    deferred: Mutex<VecDeque<IOMessage>>,
    /// Count of ordinary prompts handed to the agent, shared with the panel.
    ///
    /// The channel accepting a message only means it is *queued*; it is taken
    /// when `receive` returns it. The panel labels its own submissions against
    /// this count, so "waiting for the agent" never reads as "delivered".
    consumed: Arc<AtomicU64>,
}

impl TuiIOProvider {
    /// Construct a provider and a paired panel sharing the same channels.
    ///
    /// `buffer` is the bounded mpsc capacity for both directions.
    #[must_use]
    pub fn new_with_panel(buffer: usize) -> (Self, ConversationPanel) {
        let (out_tx, mut out_rx) = mpsc::channel::<IOMessage>(buffer);
        let (in_tx, in_rx) = mpsc::channel::<IOMessage>(buffer);
        let out_notify = Arc::new(Notify::new());
        let out_buffer: Arc<Mutex<VecDeque<IOMessage>>> = Arc::default();

        // Drain agent → user into the shared buffer on a background task,
        // notifying the panel each time. The panel can then drain without
        // blocking — keeping the render path off `tokio::sync::Mutex`.
        let buf_clone = out_buffer.clone();
        let notify_clone = out_notify.clone();
        tokio::spawn(async move {
            while let Some(message) = out_rx.recv().await {
                buf_clone.lock().push_back(message);
                notify_clone.notify_waiters();
            }
        });

        let consumed = Arc::new(AtomicU64::new(0));
        let provider = Self {
            out_tx,
            in_rx: tokio::sync::Mutex::new(in_rx),
            out_notify: out_notify.clone(),
            awaiting_confirm: AtomicBool::new(false),
            deferred: Mutex::new(VecDeque::new()),
            consumed: consumed.clone(),
        };
        let panel = ConversationPanel::new(in_tx, out_buffer, out_notify, consumed);
        (provider, panel)
    }

    /// Record a message as taken by the agent.
    ///
    /// Confirmation answers are excluded: the panel tracks delivery for
    /// ordinary prompts only, and counting an answer would retire a queued
    /// prompt that is in fact still waiting behind the dialog.
    fn note_consumed(&self, message: &IOMessage) {
        if !message.metadata.contains_key(CONFIRMED) {
            self.consumed.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Same as [`new_with_panel`](Self::new_with_panel) but with the default
    /// buffer size.
    #[must_use]
    pub fn new() -> (Self, ConversationPanel) {
        Self::new_with_panel(DEFAULT_CHANNEL_BUFFER)
    }
}

impl IOProvider for TuiIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        // Latch the dialog on the way out. `confirm` gives the provider no
        // other signal that the receive which follows is an answer and not the
        // next thing the user happened to type.
        if message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_CONFIRM) {
            self.awaiting_confirm.store(true, Ordering::SeqCst);
        }
        self.out_tx
            .send(message)
            .await
            .map_err(|err| IOError::Provider(err.to_string()))
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        // Prompts held back during a dialog come first: they were typed before
        // anything still sitting in the channel.
        if !self.awaiting_confirm.load(Ordering::SeqCst)
            && let Some(message) = self.deferred.lock().pop_front()
        {
            self.note_consumed(&message);
            return Ok(message);
        }
        let mut rx = self.in_rx.lock().await;
        loop {
            // `recv` returns None once every sender has been dropped. We only
            // have one sender (the panel), so that path means shutdown. A
            // shutdown mid-dialog resolves nothing: `confirm` propagates the
            // error and its tool never runs.
            let Some(message) = rx.recv().await else {
                self.awaiting_confirm.store(false, Ordering::SeqCst);
                return Err(IOError::Closed);
            };
            if !self.awaiting_confirm.load(Ordering::SeqCst) {
                self.note_consumed(&message);
                return Ok(message);
            }
            // Only a message the panel stamped answers the dialog. Anything
            // else is an ordinary prompt: handing it to `confirm` would both
            // deny the tool (no `confirmed` metadata) and swallow the prompt.
            if message.metadata.contains_key(CONFIRMED) {
                self.awaiting_confirm.store(false, Ordering::SeqCst);
                return Ok(message);
            }
            self.deferred.lock().push_back(message);
        }
    }

    async fn close(&self) {
        // Wake any panel parked on a notify so it can observe shutdown
        // on its next tick (the agent stops sending; the buffer just
        // stops filling).
        self.out_notify.notify_waiters();
    }
}

/// Navigation map for the transcript viewport.
///
/// Deliberately just `PageUp`/`PageDown`: this panel owns a text input, so
/// the arrow and `Home`/`End` keys belong to the cursor, and
/// [`KeyMap::vim_paging`]'s `Ctrl-U` would collide with this panel's
/// clear-line chord — a clash [`KeyMap::conflicts`] would report. A
/// typing-free panel can afford a richer map.
fn transcript_nav() -> KeyMap {
    KeyMap::new()
        .bind(KeyCode::PageUp, KeyModifiers::NONE, NavIntent::PageUp)
        .bind(KeyCode::PageDown, KeyModifiers::NONE, NavIntent::PageDown)
}

/// Parse an unsigned-integer metadata value, defaulting to 0 when absent or
/// malformed. Used to fold token-usage deltas into the running tally.
fn parse_meta_u64(message: &IOMessage, key: &str) -> u64 {
    message
        .metadata
        .get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Split text into physical lines on `\n`, tolerating Windows `\r\n`. Always
/// yields at least one segment (the empty string for empty input), so a caller
/// mapping segments to `Line`s renders an empty message as one blank row rather
/// than dropping it. Borrows from `text`; callers clone per segment as needed.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// Format a token count for the activity line: counts of 1,000 or more collapse
/// to thousands with one decimal (`133854` → `133.9k`), keeping the tally short
/// and the layout stable as the numbers grow; smaller counts stay as plain
/// integers (`850`).
fn fmt_tokens(count: u64) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

/// One node in the per-turn execution timeline: a graph system that ran, in the
/// order it ran. Its captured output is correlated from the shared
/// [`LogBuffer`] by `(node_name, seq)` at render time.
struct TimelineNode {
    /// Graph system node name (e.g. `act`).
    node_name: String,
    /// Per-turn sequence index, distinguishing repeated invocations.
    seq: u64,
    /// Human-readable activity label carried by the status message.
    label: String,
}

/// An approval request the agent is parked on, awaiting the user's answer.
///
/// Its presence is the whole difference between a `y` that approves and a `y`
/// that is just a message: [`UserIO::confirm`] reads the [`CONFIRMED`]
/// metadata and treats its absence as denial, so the panel stamps a response
/// only while a dialog is actually open.
struct PendingConfirm {
    /// First line of the agent's prompt — the command or edit being approved —
    /// echoed in the banner above the input so what `y` accepts stays on screen
    /// even after the transcript has scrolled past the full preview.
    summary: String,
}

/// Why the last submission did not reach the agent, and whether retrying helps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryFailure {
    /// The agent's inbox is full — it is behind, not gone. Enter retries.
    Backlogged,
    /// The receiving half is gone. Nothing will be delivered again; the draft
    /// is kept so it can still be copied out of the terminal.
    Closed,
    /// The host has reported it cannot process work (see
    /// [`Availability::Unavailable`]). The channel may well be open — that is
    /// exactly the trap: a prompt would be accepted, marked `queued`, and
    /// never read. The draft is kept; the reason is on the banner above.
    Unavailable,
}

impl DeliveryFailure {
    /// Actionable one-line explanation for the banner above the input.
    fn message(self) -> &'static str {
        match self {
            Self::Backlogged => "not sent — the agent's inbox is full; press Enter to retry",
            Self::Closed => "not sent — the agent is gone; the draft is kept",
            Self::Unavailable => "not sent — the host cannot process work; the draft is kept",
        }
    }
}

/// Interpret a submitted line as an answer to a pending approval.
///
/// Deliberately strict: only an unambiguous yes or no resolves a dialog.
/// Anything else — "yes, but rename it first" included — is an ordinary
/// prompt, because guessing wrong either runs a command the user never
/// approved or throws their message away.
fn parse_confirmation(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "approve" | "allow" | "ok" => Some(true),
        "n" | "no" | "deny" | "reject" | "cancel" => Some(false),
        _ => None,
    }
}

/// Chat transcript + input prompt for [`TuiIOProvider`].
pub struct ConversationPanel {
    browse: navigation::BrowseState,
    transcript: VecDeque<IOMessage>,
    /// The draft being edited. Owns its own caret; see [`Composer`] for why the
    /// caret is a byte offset measured in display columns rather than a char
    /// index.
    composer: Composer,
    history: Option<History>,
    runtime: Option<TuiHandle>,
    editor: Option<EditorCommand>,
    editor_result: Option<oneshot::Receiver<std::io::Result<String>>>,
    editor_notice: Option<String>,
    /// First composer row on screen. Non-zero only once the draft is taller
    /// than the bounded input area and the caret has moved past the fold.
    composer_scroll: usize,
    /// Inner width of the input area at the last render, in columns.
    ///
    /// Wrapping is a render-time property, but vertical caret motion has to
    /// know where the rows break, and key handling runs before the next
    /// render. Seeding it with a plausible width keeps the first keypress of a
    /// session sane on the off chance it precedes the first frame.
    composer_width: usize,
    view: ScrollableTranscript,
    nav: KeyMap,
    focused: bool,
    /// Per-turn timeline of graph nodes that ran, oldest first. Reset at the
    /// start of each turn (see `reset_pending`).
    timeline: Vec<TimelineNode>,
    /// Whether Ctrl-O has expanded the panel into the node-timeline view.
    expanded: bool,
    /// Set when an idle status (a turn finished) arrives; the next node-bearing
    /// status clears the timeline. Deferring the clear keeps the finished turn
    /// visible until the next turn actually starts producing work.
    reset_pending: bool,
    /// Optional shared log buffer; when present, the expanded timeline shows the
    /// `tracing` events captured within each node's span.
    log_buffer: Option<LogBuffer>,
    /// Open approval request; `None` when no dialog is pending. Set from an
    /// `io_type = "confirm"` message and cleared once an answer is delivered.
    pending_confirm: Option<PendingConfirm>,
    /// Transient explanation appended to the approval banner — why the last
    /// submission did not resolve the dialog.
    confirm_notice: Option<String>,
    /// Why the last submission was refused by the channel; `None` once one
    /// lands. Drives the delivery banner and keeps the draft in the input.
    delivery_failure: Option<DeliveryFailure>,
    /// When the last `Ctrl-C` the panel consumed happened. A second press
    /// within [`INTERRUPT_ESCALATION`] is declined so the shell can quit.
    last_interrupt: Option<Instant>,
    /// Host-owned signal operation; never an executor or cancellation token.
    interrupt_handle: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Host-owned backgrounding request; the same shape, a different verb.
    background_handle: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Optional host-owned queue/steering contract. Without it, submissions use
    /// the original `UserIO` channel unchanged.
    turn_input: Option<Arc<dyn TurnInputHandle>>,
    /// Messages accepted by `turn_input` and not yet observed as consumed.
    pending_turn_inputs: VecDeque<PendingTurnInput>,
    /// Latches one request until the host reports idle or a terminal status.
    interrupt_requested: bool,
    /// True only after the host acknowledges the signal through IO.
    interrupting: bool,
    /// Number of prompts the channel has accepted from this panel. Doubles as
    /// the next submission number stamped on a local echo.
    submissions: u64,
    /// Provider-side count of prompts actually handed to the agent. A local
    /// echo whose submission number is at or above this is still queued.
    consumed: Arc<AtomicU64>,
    /// Current in-flight activity (e.g. "Thinking…"); `None` when idle.
    status: Option<String>,
    /// Spinner animation frame, advanced on each tick while `status` is set.
    spinner: usize,
    /// Cumulative input tokens billed this session.
    tokens_in: u64,
    /// Cumulative output tokens billed this session.
    tokens_out: u64,
    /// Whether any status/token message has been seen. Until then the meta
    /// line is hidden entirely, so consumers that don't drive the
    /// status/token protocol render exactly as a plain transcript + input.
    meta_active: bool,
    /// Everything the host has reported about the session behind this
    /// transcript — model, workspace, approval posture, projected context
    /// window, spend, availability. Folded from every `status` message that
    /// carries a [`StatusReport`]; empty until one does, and then only ever
    /// as complete as the host chose to make it.
    report: StatusReport,
    /// One-line reason from the last host-sourced `error`, shown on the meta
    /// row while idle so a turn that died does not read as "ready". Cleared
    /// when the host reports work again (a non-empty `status`).
    last_error: Option<String>,
    /// Monotonic content fingerprint, bumped whenever the lines the scroll view
    /// renders could change (a transcript or timeline mutation). Folded into the
    /// revision handed to [`ScrollableTranscript`] so it re-wraps only on change
    /// — spinner/token ticks, which don't touch the rendered lines, don't.
    content_rev: u64,
    /// The host's advertised slash commands. `None` until an advertisement
    /// arrives, and while it is `None` a leading `/` is ordinary text: a
    /// consumer that never advertises gets no popup and no dispatch.
    commands: Option<Vec<CommandSpec>>,
    /// Host-supplied `@` mention candidates. `None` leaves `@` as text.
    completion_source: Option<Arc<dyn CompletionSource>>,
    /// The open completion popup, if any.
    popup: Option<CompletionPopup>,
    /// Set by `Esc` while a popup is open: keep it closed until the draft
    /// changes again, so dismissing it does not fight the next render.
    popup_suppressed: bool,
    /// The in-flight mention query, so the next keystroke can cancel it.
    mention_request: Option<MentionRequest>,
    /// Sequence number of the most recent mention query. A result carrying
    /// an older number is stale and dropped.
    mention_seq: u64,
    /// Where a finished mention query leaves its rows for the panel to pick
    /// up on the next tick or frame — the query runs on a task, the panel
    /// reads on the render path, and neither waits on the other.
    mention_results: MentionSlot,
    /// Host-supplied attachment resolver. `None` leaves the composer with no
    /// chips at all, and no attachment metadata on anything it sends.
    attachment_source: Option<Arc<dyn AttachmentSource>>,
    /// The chips under the draft, in the order they were added — which is the
    /// order their handles are stamped in.
    attachments: Vec<Attachment>,
    /// Mentioned paths whose resolve has finished, either way. Keeps a draft
    /// from re-asking the host about the same path on every keystroke, and is
    /// pruned to what the draft still mentions.
    attachment_settled: Vec<String>,
    /// Resolves in flight. Dropping one cancels it.
    attachment_requests: Vec<AttachmentRequest_>,
    /// Sequence number of the most recent resolve; a result carrying a number
    /// no request still holds is stale.
    attachment_seq: u64,
    /// Where finished resolves land, read on the render path.
    attachment_results: AttachmentSlot,
    /// The last actionable refusal, shown under the composer until the next
    /// resolve settles it.
    attachment_notice: Option<String>,
    in_tx: mpsc::Sender<IOMessage>,
    inbox: Arc<Mutex<VecDeque<IOMessage>>>,
    notify: Arc<Notify>,
}

/// The mailbox a finished mention query drops its rows into, tagged with the
/// query's sequence number.
type MentionSlot = Arc<Mutex<Option<(u64, Vec<Completion>)>>>;

/// A mention query in flight.
struct MentionRequest {
    /// Its sequence number, matched against `mention_seq` when it lands.
    seq: u64,
    /// Cancels the source's walk.
    cancel: CancelToken,
    /// The task awaiting the source, aborted on cancellation.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MentionRequest {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

/// What the panel asked the host to attach, kept so a landing result knows
/// which mention — if any — the chip belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachmentOrigin {
    /// A `@path` mention in the draft, by its unescaped path.
    Mention(String),
    /// The clipboard, which no mention backs.
    Clipboard,
}

/// The mailbox finished attachment resolves drop their outcomes into.
type AttachmentSlot = Arc<Mutex<Vec<(u64, Result<Attachment, AttachmentRejected>)>>>;

/// An attachment resolve in flight. Named with a trailing underscore because
/// [`AttachmentRequest`] is the host-facing request *value*, and both are in
/// scope here.
struct AttachmentRequest_ {
    /// Its sequence number, matched when the outcome lands.
    seq: u64,
    /// What was asked for.
    origin: AttachmentOrigin,
    /// Cancels the host's work.
    cancel: CancelToken,
    /// The task awaiting the host, aborted on cancellation.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AttachmentRequest_ {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl ConversationPanel {
    fn new(
        in_tx: mpsc::Sender<IOMessage>,
        inbox: Arc<Mutex<VecDeque<IOMessage>>>,
        notify: Arc<Notify>,
        consumed: Arc<AtomicU64>,
    ) -> Self {
        Self {
            browse: navigation::BrowseState::default(),
            transcript: VecDeque::new(),
            composer: Composer::new(),
            history: None,
            runtime: None,
            editor: None,
            editor_result: None,
            editor_notice: None,
            composer_scroll: 0,
            composer_width: DEFAULT_COMPOSER_WIDTH,
            view: ScrollableTranscript::new(),
            nav: transcript_nav(),
            focused: true,
            timeline: Vec::new(),
            expanded: false,
            reset_pending: false,
            log_buffer: None,
            pending_confirm: None,
            confirm_notice: None,
            delivery_failure: None,
            last_interrupt: None,
            interrupt_handle: None,
            background_handle: None,
            turn_input: None,
            pending_turn_inputs: VecDeque::new(),
            interrupt_requested: false,
            interrupting: false,
            submissions: 0,
            consumed,
            status: None,
            spinner: 0,
            tokens_in: 0,
            tokens_out: 0,
            meta_active: false,
            report: StatusReport::default(),
            last_error: None,
            content_rev: 0,
            commands: None,
            completion_source: None,
            popup: None,
            popup_suppressed: false,
            mention_request: None,
            mention_seq: 0,
            mention_results: Arc::default(),
            attachment_source: None,
            attachments: Vec::new(),
            attachment_settled: Vec::new(),
            attachment_requests: Vec::new(),
            attachment_seq: 0,
            attachment_results: Arc::default(),
            attachment_notice: None,
            in_tx,
            inbox,
            notify,
        }
    }

    /// Supply the host's `@` mention candidates. Set by
    /// [`ConversationPanelPlugin::with_completion_source`].
    fn set_completion_source(&mut self, source: Arc<dyn CompletionSource>) {
        self.completion_source = Some(source);
    }

    /// Supply the host's attachment resolver. Set by
    /// [`ConversationPanelPlugin::with_attachment_source`].
    fn set_attachment_source(&mut self, source: Arc<dyn AttachmentSource>) {
        self.attachment_source = Some(source);
    }

    /// Enable history through an explicit host store and namespace.
    ///
    /// Load and append run off the render loop. Only accepted ordinary prompts
    /// are persisted; failed submissions and approval replies are excluded.
    #[must_use]
    pub fn with_history(mut self, config: PromptHistoryConfig) -> Self {
        self.history = Some(History::new(config));
        self
    }

    /// Override VISUAL/EDITOR with a host-supplied executable and argument list.
    #[must_use]
    pub fn with_editor(mut self, editor: EditorCommand) -> Self {
        self.editor = Some(editor);
        self
    }

    fn poll_composer_services(&mut self) {
        if let Some(history) = &mut self.history {
            history.poll();
        }
        self.drain_attachment_results();
        let Some(receiver) = &mut self.editor_result else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => Err(std::io::Error::other(
                "Editor task stopped; draft unchanged",
            )),
        };
        self.editor_result = None;
        match result {
            Ok(text) => {
                replace(&mut self.composer, &text);
                self.composer_scroll = 0;
                self.editor_notice = None;
                if let Some(history) = &mut self.history {
                    history.reset();
                }
            }
            Err(error) => self.editor_notice = Some(format!("Editor: {error}")),
        }
    }

    fn open_editor(&mut self) {
        let Some(handle) = self.runtime.clone() else {
            self.editor_notice =
                Some("Editor unavailable: panel is not attached to a TUI runtime".into());
            return;
        };
        let editor = match self
            .editor
            .clone()
            .map(Ok)
            .unwrap_or_else(EditorCommand::from_env)
        {
            Ok(editor) => editor,
            Err(error) => {
                self.editor_notice = Some(error.to_string());
                return;
            }
        };
        let (reply, result) = oneshot::channel();
        let draft = self.composer.text().to_owned();
        self.editor_result = Some(result);
        self.editor_notice = None;
        tokio::spawn(async move {
            let _ = reply.send(edit_draft(handle, editor, draft).await);
        });
    }

    fn history_cancel(&mut self) -> bool {
        self.history
            .as_mut()
            .is_some_and(|history| history.cancel(&mut self.composer))
    }

    fn search_key(&mut self, key: KeyEvent) -> EventOutcome {
        let history = self.history.as_mut().expect("called only during search");
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                history.cancel(&mut self.composer);
            }
            (KeyCode::Char('c' | 'g'), modifiers) if modifiers == KeyModifiers::CONTROL => {
                // Search cancellation restores the draft and never answers an
                // approval that happened to arrive during the search.
                history.cancel(&mut self.composer);
                if key.code == KeyCode::Char('c') {
                    self.last_interrupt = Some(Instant::now());
                }
            }
            (KeyCode::Char('r'), modifiers) if modifiers == KeyModifiers::CONTROL => {
                history.search(&mut self.composer)
            }
            (KeyCode::Enter, _) => history.accept_search(&mut self.composer),
            (KeyCode::Backspace, _) => history.query_backspace(&mut self.composer),
            (KeyCode::Char(ch), modifiers)
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
            {
                history.query_insert(&mut self.composer, &ch.to_string());
            }
            _ => {}
        }
        EventOutcome::Consumed
    }

    /// Share a [`LogBuffer`] so the expanded timeline can show each node's
    /// captured `tracing` output. Set by [`ConversationPanelPlugin`] when a
    /// buffer is supplied.
    fn set_log_buffer(&mut self, buffer: LogBuffer) {
        self.log_buffer = Some(buffer);
    }

    /// Drop everything the panel holds *about the session on screen*, so a
    /// replay starts the incoming session from nothing rather than from the
    /// outgoing one's leftovers.
    ///
    /// A replay replaces the transcript, and every fact drawn beside it
    /// belongs to the transcript that was there: the host's [`StatusReport`]
    /// — model, workspace, session, approval posture, projected context
    /// window, spend, availability — the last failure it reported, the turn
    /// in flight, this process's token tally, and any message queued against
    /// the outgoing session's runs.
    ///
    /// Each of those goes back to *the host has not said*, never to a guess
    /// or a zero, because the meta row already renders "has not said" as
    /// absence: no context meter rather than the previous session's 90%
    /// full, `● ready` rather than a failure from another conversation. The
    /// host re-states what is true of the new session and the row fills in
    /// again; until then the panel claims nothing about it.
    ///
    /// The panel has to be the one to do this. [`StatusReport::merge`] only
    /// ever *sets* the fields an update asserts — that is what lets a host
    /// send its identity once and a cheap refresh every turn — so no message
    /// a host could send would clear a stale field from outside.
    ///
    /// Deliberately kept, because none of it belongs to the session:
    ///
    /// - the draft, its history and its completion state: the user's own
    ///   unsent typing, which a switch they asked for must not throw away;
    /// - the host's advertised commands, which the host owns and not the
    ///   session — dropping them would make a leading `/` plain text again
    ///   until the next advertisement;
    /// - [`submissions`](Self::submissions), counted against the provider's
    ///   shared `consumed` tally: restarting it would mark the next echo
    ///   delivered before the agent had taken it;
    /// - a `Backlogged` or `Closed` delivery failure, which describes the
    ///   channel to the agent rather than the session on screen and is still
    ///   true after the switch; and
    /// - view state — expansion, focus, key map — which is the user's.
    fn clear_session_state(&mut self) {
        self.transcript.clear();
        self.timeline.clear();
        self.reset_pending = false;
        self.browse = navigation::BrowseState::default();
        self.view = ScrollableTranscript::new();
        self.pending_confirm = None;
        self.confirm_notice = None;
        self.status = None;
        self.spinner = 0;
        self.interrupt_requested = false;
        self.interrupting = false;
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.report = StatusReport::default();
        self.last_error = None;
        // Only the variant the outgoing session's posture produced. The
        // banner is cleared rather than suppressed: the reason behind it is
        // gone with the report it came from, and the draft it refused is
        // still in the composer for the user to send to this session.
        if self.delivery_failure == Some(DeliveryFailure::Unavailable) {
            self.delivery_failure = None;
        }
        // Receipts name ids and run numbers in the *outgoing* session's host
        // queue, and their local echoes went with the transcript. Dropping
        // the panel's copy gives up its claim to edit or remove them; it
        // does not tell the host to discard the messages, which is the
        // host's call about its own queue and not a presentation panel's to
        // assert across a session boundary.
        self.pending_turn_inputs.clear();
        // The handles name bytes the *outgoing* session's host is holding.
        // Carrying them into another session would stamp a prompt with
        // references its host never minted.
        self.clear_attachments();
    }

    fn drain_inbox(&mut self) {
        // Take the batch under the lock, then handle it unlocked: the arms
        // below mutate `self`, and holding the guard across them would also
        // keep the producer task waiting on a render-path lock.
        let batch: Vec<IOMessage> = self.inbox.lock().drain(..).collect();
        for message in batch {
            match message.metadata.get(META_TYPE).map(String::as_str) {
                // Ephemeral status — drives the spinner line, never the transcript.
                // When stamped with a `node`, it also extends the node timeline.
                Some(TYPE_STATUS) => {
                    let text = match &message.content {
                        IOContent::Text(text) if !text.is_empty() => Some(text.clone()),
                        _ => None,
                    };
                    let report = parse_status(&message);
                    if let Some(update) = report.clone() {
                        if update.availability == Availability::Available {
                            self.last_error = None;
                        }
                        self.report.merge(update);
                        self.meta_active = true;
                    }
                    // A command advertisement rides on a status message. It
                    // replaces the cached command set; with no activity text
                    // it is an advertisement only and leaves the activity
                    // line — and the meta row's visibility — untouched. That
                    // includes the interrupt lifecycle below: an advertisement
                    // is not the host reporting the turn idle.
                    if let Some(specs) = parse_advertisement(&message) {
                        self.commands = Some(specs);
                        self.refresh_popup();
                        if text.is_none() {
                            continue;
                        }
                    }
                    // A report with no activity text is an identity/budget
                    // refresh, not an idle transition. In particular it must
                    // not clear an in-flight spinner or interrupt lifecycle.
                    if report.is_some() && text.is_none() {
                        continue;
                    }
                    if text.is_none() {
                        self.interrupt_requested = false;
                        self.interrupting = false;
                    }
                    if !self.interrupting {
                        self.status = text.clone();
                    }
                    self.spinner = 0;
                    self.meta_active = true;
                    if let Some(node_name) = message.metadata.get(META_NODE).cloned() {
                        match text {
                            // Idle marker (empty status, e.g. `receive_user_input`):
                            // arm a reset so the just-finished turn stays visible
                            // until the next turn produces work.
                            None => self.reset_pending = true,
                            Some(label) => {
                                if self.reset_pending {
                                    self.timeline.clear();
                                    self.reset_pending = false;
                                }
                                self.timeline.push(TimelineNode {
                                    node_name,
                                    seq: parse_meta_u64(&message, META_SEQ),
                                    label,
                                });
                                self.content_rev = self.content_rev.wrapping_add(1);
                            }
                        }
                    }
                }
                // These rows attest to host events, never to a timer or merely
                // pressing a key. Hosts may also interrupt from outside the TUI.
                Some(TYPE_INTERRUPTING) => {
                    self.interrupt_requested = true;
                    self.interrupting = true;
                    self.status = Some("Interrupting".to_string());
                    self.spinner = 0;
                    self.meta_active = true;
                    self.push_transcript(message);
                }
                Some(TYPE_CANCELLED) => {
                    self.interrupt_requested = false;
                    self.interrupting = false;
                    self.status = None;
                    self.reset_pending = true;
                    self.meta_active = true;
                    self.push_transcript(message);
                }
                // Token-usage delta — fold into the running tally.
                Some(TYPE_TOKENS) => {
                    self.tokens_in = self
                        .tokens_in
                        .saturating_add(parse_meta_u64(&message, META_IN));
                    self.tokens_out = self
                        .tokens_out
                        .saturating_add(parse_meta_u64(&message, META_OUT));
                    self.meta_active = true;
                }
                Some(IO_TYPE_SESSION) => {
                    match message
                        .metadata
                        .get(META_SESSION_ACTION)
                        .map(String::as_str)
                    {
                        Some(SESSION_REPLAY_START) => {
                            self.clear_session_state();
                            // The replay itself is a meta event: the row is
                            // shown, and now says only "ready".
                            self.meta_active = true;
                            self.push_transcript(message);
                        }
                        Some(SESSION_REPLAY_END) => self.push_transcript(message),
                        _ => self.push_transcript(message),
                    }
                }
                Some(TYPE_ERROR) => {
                    if message.source == IOSource::System {
                        self.last_error = match &message.content {
                            IOContent::Text(text) => split_lines(text)
                                .find(|line| !line.trim().is_empty())
                                .map(|line| safe_text(line.trim())),
                            _ => Some("the host reported an error".to_string()),
                        };
                        self.status = None;
                        self.meta_active = true;
                    }
                    self.push_transcript(message);
                }
                // Approval request — the agent is parked in `UserIO::confirm`.
                // The message carries the command or edit preview, so it goes
                // to the transcript like any other; opening the dialog is what
                // makes the next `y`/`n` an answer rather than a message.
                Some(TYPE_CONFIRM) => {
                    let summary = match &message.content {
                        IOContent::Text(text) => {
                            split_lines(text).next().unwrap_or_default().to_string()
                        }
                        _ => String::new(),
                    };
                    self.pending_confirm = Some(PendingConfirm { summary });
                    self.confirm_notice = None;
                    self.push_transcript(message);
                }
                // An assistant message block: deltas sharing an `id` fold
                // into one entry. Without an `id` it is a standalone message
                // and falls through to the ordinary append.
                Some(TYPE_MESSAGE) if message.metadata.contains_key(META_ID) => {
                    self.fold_message(message);
                }
                // A tool activity block: later phases replace the entry in
                // place, so the row's identity is the invocation, not the
                // update that last touched it.
                Some(TYPE_TOOL) if message.metadata.contains_key(META_ID) => {
                    self.fold_tool(message);
                }
                _ => self.push_transcript(message),
            }
        }
    }

    /// Fold one `message` update into its block, or start the block.
    ///
    /// Ordering is enforced by [`META_SEQ`]: an update whose sequence number
    /// does not advance the block is **dropped**, which is what stops a resend
    /// or a reordered delivery from appending the same text twice. A closing
    /// update (`final`) with a non-empty body *replaces* the accumulated text
    /// with the sender's authoritative version, so a delta that never arrived
    /// is repaired rather than leaving a gap; an empty closing body keeps what
    /// accumulated, which is how a failed stream retains its partial text.
    fn fold_message(&mut self, message: IOMessage) {
        let id = message.metadata.get(META_ID).cloned().unwrap_or_default();
        let Some(index) = self.block_index(TYPE_MESSAGE, &id) else {
            self.push_transcript(message);
            return;
        };
        let seq = parse_meta_u64(&message, META_SEQ);
        let block = &mut self.transcript[index];
        if seq <= parse_meta_u64(block, META_SEQ) {
            return;
        }
        let delta = match &message.content {
            IOContent::Text(text) => text.clone(),
            other => other_body(other),
        };
        let is_final = message.metadata.get(META_FINAL).map(String::as_str) == Some(FINAL_TRUE);
        if let IOContent::Text(existing) = &mut block.content {
            if is_final {
                if !delta.is_empty() {
                    *existing = delta;
                }
            } else {
                existing.push_str(&delta);
            }
        }
        block.metadata.insert(META_SEQ.to_string(), seq.to_string());
        if is_final {
            block
                .metadata
                .insert(META_FINAL.to_string(), FINAL_TRUE.to_string());
            if let Some(outcome) = message.metadata.get(META_OUTCOME) {
                block
                    .metadata
                    .insert(META_OUTCOME.to_string(), outcome.clone());
            }
        }
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    /// Replace a `tool` block with its latest phase, or start the block.
    ///
    /// Unlike a message, a tool update is not additive: each one carries the
    /// invocation's current state in full, so the newest simply wins. That
    /// keeps one invocation on one row through `start` → `ok`, however many
    /// updates it took to get there.
    fn fold_tool(&mut self, message: IOMessage) {
        let id = message.metadata.get(META_ID).cloned().unwrap_or_default();
        match self.block_index(TYPE_TOOL, &id) {
            Some(index) => {
                self.transcript[index] = message;
                self.content_rev = self.content_rev.wrapping_add(1);
            }
            None => self.push_transcript(message),
        }
    }

    /// Position of the open block with this kind and id, searched from the
    /// newest entry because a block is folded into while it is still the most
    /// recent thing on screen.
    fn block_index(&self, kind: &str, id: &str) -> Option<usize> {
        self.transcript.iter().rposition(|message| {
            message.metadata.get(META_TYPE).map(String::as_str) == Some(kind)
                && message.metadata.get(META_ID).map(String::as_str) == Some(id)
        })
    }

    /// Append a message to the transcript, trimming to [`TRANSCRIPT_CAP`] and
    /// bumping the content fingerprint so the scroll view re-wraps.
    fn push_transcript(&mut self, message: IOMessage) {
        self.transcript.push_back(message);
        // Unknown messages must remain recoverable even when ordinary history
        // reaches its rolling cap. Never evict one merely because it is hidden.
        while self
            .transcript
            .iter()
            .filter(|message| !unknown_message(message))
            .count()
            > TRANSCRIPT_CAP
        {
            if let Some(index) = self
                .transcript
                .iter()
                .position(|message| !unknown_message(message))
            {
                if self.browse.page.is_none() && !self.view.is_following() {
                    let removed_lines = build_message_lines(
                        &VecDeque::from([self.transcript[index].clone()]),
                        self.consumed.load(Ordering::Relaxed),
                        self.expanded,
                    )
                    .len();
                    if let Some((line, _)) = self.view.anchor() {
                        self.view.jump_to_line(line.saturating_sub(removed_lines));
                    }
                    self.browse.selected = self.browse.selected.and_then(|selected| {
                        if selected == index {
                            None
                        } else {
                            selected.checked_sub(usize::from(index < selected))
                        }
                    });
                }
                self.transcript.remove(index);
            }
        }
        if unknown_message(self.transcript.back().expect("just appended")) {
            self.meta_active = true;
        }
        self.content_rev = self.content_rev.wrapping_add(1);
    }

    /// One-line activity indicator rendered between the transcript and input:
    /// a spinner + the current step while busy, "ready" when idle, with the
    /// cumulative session token tally trailing either way.
    #[cfg(test)]
    fn meta_line(&self) -> Line<'static> {
        self.meta_line_for_width(u16::MAX)
    }

    fn meta_line_for_width(&self, width: u16) -> Line<'static> {
        let total = self.tokens_in.saturating_add(self.tokens_out);
        let tokens = format!(
            "↑{} ↓{} · {} tok",
            fmt_tokens(self.tokens_in),
            fmt_tokens(self.tokens_out),
            fmt_tokens(total),
        );
        let show_tokens = width >= 40;
        let dim = Style::default().fg(Color::DarkGray);
        let availability_error = self.report.availability.unavailable_reason();
        let error = availability_error.or(self.last_error.as_deref());
        let mut spans = if let Some(reason) = error {
            vec![Span::styled(
                format!("● error · {}", safe_text(reason)),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )]
        } else {
            match &self.status {
                Some(activity) => {
                    let frame = SPINNER[self.spinner % SPINNER.len()];
                    vec![
                        Span::styled(
                            format!("{frame} {}", safe_text(activity)),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            if show_tokens {
                                format!("  {tokens}")
                            } else {
                                String::new()
                            },
                            dim,
                        ),
                    ]
                }
                None => vec![Span::styled(
                    if show_tokens {
                        format!("● ready  {tokens}")
                    } else {
                        "● ready".to_string()
                    },
                    dim,
                )],
            }
        };
        let mut facts = Vec::new();
        if width >= 40 {
            if let Some(model) = &self.report.model {
                facts.push(safe_text(model));
            }
            if let Some(context) = self.report.context {
                let estimate = if context.estimated { "~" } else { "" };
                let mut value = match context.limit {
                    Some(limit) => format!(
                        "ctx {estimate}{}/{} ({}%)",
                        fmt_tokens(context.tokens),
                        fmt_tokens(limit),
                        context.percent().unwrap_or(0)
                    ),
                    None => format!("ctx {estimate}{} / ?", fmt_tokens(context.tokens)),
                };
                if context.compacting {
                    value.push_str(" · compacting");
                }
                facts.push(value);
            }
            match self.report.cost {
                Cost::Usd(amount) => facts.push(format!("${amount:.4}")),
                Cost::Unpriced => facts.push("cost n/a".to_string()),
                Cost::Unstated => {}
            }
            // Work the user pushed off-screen is the archetypal hidden state,
            // so its count keeps the same priority as cost rather than being
            // dropped first on a narrow terminal. A reported zero renders
            // nothing: there is nothing left to discover.
            if let Some(running) = self.report.tasks_running.filter(|count| *count > 0) {
                facts.push(format!("{running} running"));
            }
        }
        if width >= 70 {
            if let Some(workspace) = &self.report.workspace {
                facts.push(safe_text(workspace));
            }
            if let Some(session) = &self.report.session {
                facts.push(safe_text(session));
            }
            if let Some(permissions) = &self.report.permissions {
                facts.push(safe_text(permissions));
            }
        }
        if !facts.is_empty() && error.is_none() {
            spans.push(Span::styled(format!("  · {}", facts.join(" · ")), dim));
        }
        // Ctrl-O affordance, shown once the timeline has any nodes.
        if !self.timeline.is_empty() {
            let hint = if self.expanded {
                format!("  · ctrl-o: hide ({} steps)", self.timeline.len())
            } else {
                format!("  · ctrl-o: {} steps", self.timeline.len())
            };
            spans.push(Span::styled(hint, dim));
        }
        let hidden = self
            .transcript
            .iter()
            .filter(|message| unknown_message(message))
            .count();
        if hidden > 0 {
            spans.insert(
                0,
                Span::styled(
                    format!(
                        "{hidden} {} · Ctrl-O  ",
                        if self.expanded { "revealed" } else { "hidden" }
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            );
        }
        Line::from(spans)
    }

    /// One-line approval banner shown above the input while a dialog is open:
    /// what is being approved and exactly which answers resolve it. Returns
    /// `None` — and the row collapses to zero height — when nothing is pending.
    fn confirm_line(&self) -> Option<Line<'static>> {
        let pending = self.pending_confirm.as_ref()?;
        let accent = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![
            Span::styled("approve? ", accent),
            Span::styled("y/yes runs it · n/no or Esc denies", accent),
        ];
        if !pending.summary.is_empty() {
            spans.push(Span::styled(
                format!("  {}", safe_text(&pending.summary)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(notice) = &self.confirm_notice {
            spans.push(Span::styled(
                format!("  ({notice})"),
                Style::default().fg(Color::Red),
            ));
        }
        Some(Line::from(spans))
    }

    /// One-line delivery banner: why the last submission was refused, and
    /// whether Enter can retry it. `None` once a submission lands.
    fn delivery_line(&self) -> Option<Line<'static>> {
        let failure = self.delivery_failure?;
        Some(Line::from(Span::styled(
            failure.message(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )))
    }

    /// The banner rows between the meta line and the input: the approval
    /// dialog, the delivery failure, or neither. The row block is exactly as
    /// tall as the lines it has, so it disappears when there is nothing to say.
    fn banner_lines(&self) -> Vec<Line<'static>> {
        let history = self.history.as_ref().and_then(History::banner);
        let editor = if self.editor_result.is_some() {
            Some("Editing draft externally…".to_string())
        } else {
            self.editor_notice.clone()
        };
        let notice = |text: String| {
            let mut safe = Composer::new();
            safe.insert_paste(&text);
            Line::from(Span::styled(
                safe.text().replace(['\n', '\t'], " "),
                Style::default().fg(Color::Yellow),
            ))
        };
        [
            self.confirm_line(),
            self.delivery_line(),
            history.map(notice),
            editor.map(notice),
            self.attachment_notice.clone().map(notice),
            self.attachment_line(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Render the per-turn node timeline (shown when [`expanded`](Self::expanded)).
    /// Each node is a header line followed by the `tracing` events captured
    /// within its span — correlated from the shared [`LogBuffer`] by
    /// `(node_name, seq)`, restricted to framework targets (`polaris*`).
    /// Without a log buffer, only the headers show.
    #[cfg(test)]
    fn timeline_lines(&self) -> Vec<Line<'static>> {
        build_timeline_lines(&self.timeline, self.log_buffer.as_ref())
    }

    /// Handle `Ctrl-C`.
    ///
    /// Claimed only where there is something to act on — an open approval to
    /// deny, or a draft to clear — and declined otherwise, so an idle panel
    /// leaves the shell's quit behaviour untouched. A second press inside
    /// [`INTERRUPT_ESCALATION`] is always declined, so quitting is never more
    /// than two presses away regardless of panel state.
    ///
    /// After approval and draft handling, an active host status plus a handle
    /// lets this panel request interruption once. Only host IO acknowledges the
    /// signal and its completion; invoking the callback fabricates no status.
    fn interrupt(&mut self) -> EventOutcome {
        let now = Instant::now();
        // Second press in the window: decline, and let the shell quit.
        if let Some(previous) = self.last_interrupt
            && now.duration_since(previous) < INTERRUPT_ESCALATION
        {
            return EventOutcome::Continue;
        }
        if self.pending_confirm.is_some() {
            self.answer_confirmation("no", false);
        } else if !self.composer.is_empty() {
            self.composer.clear();
            self.composer_scroll = 0;
            self.delivery_failure = None;
            self.refresh_popup();
            if let Some(history) = &mut self.history {
                history.reset();
            }
        } else if self.status.is_some()
            && let Some(handle) = &self.interrupt_handle
        {
            if !self.interrupt_requested {
                self.interrupt_requested = true;
                handle();
            }
        } else {
            // Nothing to interrupt or clear: the shell owns this press.
            return EventOutcome::Continue;
        }
        self.last_interrupt = Some(now);
        EventOutcome::Consumed
    }

    /// Handle `Ctrl-T`: ask the host to background the live work.
    ///
    /// The panel forwards the request and decides nothing about it. It does not
    /// consult its own transcript for a live tool block, because what is
    /// running is the host's knowledge, not the panel's cache of what it was
    /// last told: a host with nothing in flight answers "nothing is running",
    /// one whose block is still awaiting approval refuses, and one that
    /// detaches sends a receipt. All three are host messages on the existing
    /// channel. Deciding here would turn the first two into a key that does
    /// nothing at all.
    fn background(&mut self) -> EventOutcome {
        let Some(handle) = &self.background_handle else {
            return EventOutcome::Continue;
        };
        handle();
        EventOutcome::Consumed
    }

    /// Submit the current draft.
    ///
    /// The draft is cleared only once the channel has actually accepted it.
    /// A refusal leaves the input — text *and* cursor — exactly as it was, so
    /// nothing the user typed is lost to a busy or departed agent, and Enter
    /// retries the same message rather than queueing a second copy.
    fn submit_input(&mut self) {
        self.submit_input_mode(TurnInputMode::Queue);
    }

    fn submit_input_mode(&mut self, requested_mode: TurnInputMode) {
        if self.composer.is_empty() {
            return;
        }
        // Never submit over a resolve still in flight. The alternative is a
        // race the user loses silently: a mention typed and sent in one motion
        // would go without its attachment, and a refusal the host was about to
        // report — too large, wrong type, model cannot read images — would
        // arrive after the draft it applies to had already gone.
        self.drain_attachment_results();
        if !self.attachment_requests.is_empty() {
            self.attachment_notice =
                Some("still checking an attachment — press Enter again".to_string());
            return;
        }
        let text = self.composer.text().to_string();
        if self.report.availability.unavailable_reason().is_some() {
            self.delivery_failure = Some(DeliveryFailure::Unavailable);
            return;
        }
        // A `/command` goes to the host as a dispatch, never to the model —
        // but only once the host has advertised that it executes commands.
        // Before that, a leading slash is text like any other, so a consumer
        // that never advertises sends its prompts unchanged.
        let invocation = self
            .commands
            .as_ref()
            .and_then(|_| CommandInvocation::parse(&text));
        // An open dialog claims this line only when it actually reads as an
        // answer. Everything else stays an ordinary prompt.
        let accepted = match (invocation, parse_confirmation(&text)) {
            (Some(invocation), _) => self.submit_command(invocation, &text),
            (None, Some(confirmed)) if self.pending_confirm.is_some() => {
                self.answer_confirmation(&text, confirmed)
            }
            _ if self.turn_input.is_some() => {
                let mode = if self.pending_confirm.is_some() {
                    TurnInputMode::Queue
                } else {
                    requested_mode
                };
                self.submit_turn_input(&text, mode)
            }
            _ => self.submit_prompt(&text),
        };
        if accepted {
            if let Some(history) = &mut self.history {
                history.reset();
            }
            self.composer.clear();
            self.composer_scroll = 0;
            // The chips went with the message. Clearing them here rather than
            // inside each submit path is deliberate: a refused submission
            // keeps its draft, and a draft that still says `@diagram.png` must
            // still show the chip that goes with it.
            self.clear_attachments();
            self.refresh_popup();
        }
    }

    fn submit_turn_input(&mut self, text: &str, mode: TurnInputMode) -> bool {
        let Some(handle) = self.turn_input.as_ref() else {
            return false;
        };
        let receipt =
            match handle.submit_with_attachments(text.to_string(), mode, self.attachment_handles())
            {
                Ok(receipt) => receipt,
                Err(reason) => {
                    self.delivery_failure = Some(DeliveryFailure::Unavailable);
                    self.editor_notice = Some(reason);
                    return false;
                }
            };
        let label = match receipt.mode {
            TurnInputMode::Queue => "queued",
            TurnInputMode::Steer => "steering",
        };
        self.push_transcript(stamp_attachments(
            IOMessage::user_text(text.to_string())
                .with_metadata("turn_input", label)
                .with_metadata("target_run", receipt.target_run.to_string()),
            &self.attachments,
        ));
        self.pending_turn_inputs.push_back(PendingTurnInput {
            receipt,
            text: text.to_string(),
        });
        if self.pending_confirm.is_some() {
            self.confirm_notice = Some("queued for the next run — still waiting on y/n".into());
        }
        if let Some(history) = &mut self.history {
            history.record(text);
        }
        true
    }

    fn edit_last_turn_input(&mut self) -> bool {
        let Some(pending) = self.pending_turn_inputs.pop_back() else {
            return false;
        };
        let Some(handle) = self.turn_input.as_ref() else {
            return false;
        };
        if let Err(reason) = handle.remove(pending.receipt.id) {
            self.pending_turn_inputs.push_back(pending);
            self.editor_notice = Some(reason);
            return true;
        }
        self.composer.clear();
        self.composer.insert_paste(&pending.text);
        self.refresh_popup();
        true
    }

    fn remove_last_turn_input(&mut self) -> bool {
        let Some(pending) = self.pending_turn_inputs.pop_back() else {
            return false;
        };
        let Some(handle) = self.turn_input.as_ref() else {
            return false;
        };
        if let Err(reason) = handle.remove(pending.receipt.id) {
            self.pending_turn_inputs.push_back(pending);
            self.editor_notice = Some(reason);
        }
        true
    }

    /// Hand `message` to the channel, recording why it was refused. Returns
    /// whether the channel took it.
    ///
    /// `try_send` gives a refused message back rather than queueing it, so a
    /// failure here means nothing was enqueued and a retry cannot duplicate.
    fn offer(&mut self, message: IOMessage) -> bool {
        match self.in_tx.try_send(message) {
            Ok(()) => {
                self.delivery_failure = None;
                self.notify.notify_waiters();
                true
            }
            Err(TrySendError::Full(_)) => {
                self.delivery_failure = Some(DeliveryFailure::Backlogged);
                false
            }
            Err(TrySendError::Closed(_)) => {
                self.delivery_failure = Some(DeliveryFailure::Closed);
                false
            }
        }
    }

    /// Deliver `text` as the answer to the pending approval, stamped with the
    /// [`CONFIRMED`] metadata [`UserIO::confirm`] reads. Returns whether it was
    /// delivered.
    ///
    /// A failed send leaves the dialog open on purpose: retrying as an
    /// unstamped message would reach `confirm` as a denial, and dropping it
    /// quietly would strand the agent with no sign of why. Either way the tool
    /// does not run until an answer actually lands.
    fn answer_confirmation(&mut self, text: &str, confirmed: bool) -> bool {
        let value = if confirmed {
            CONFIRMED_TRUE
        } else {
            CONFIRMED_FALSE
        };
        let response = IOMessage::user_text(text.to_string()).with_metadata(CONFIRMED, value);
        if !self.offer(response) {
            return false;
        }
        self.pending_confirm = None;
        self.confirm_notice = None;
        self.push_transcript(IOMessage::user_text(text.to_string()));
        true
    }

    /// Deliver `text` as an ordinary prompt.
    ///
    /// This stays the right destination even while a dialog is open: the
    /// provider holds such prompts back until the dialog is answered instead of
    /// letting one answer for the user, so the prompt keeps its intended turn
    /// and the banner just says the approval is still waiting.
    fn submit_prompt(&mut self, text: &str) -> bool {
        // Offer first, echo second. Echoing a message the channel refused
        // would present it as delivered when the agent will never see it.
        if !self.offer(stamp_attachments(
            IOMessage::user_text(text.to_string()),
            &self.attachments,
        )) {
            return false;
        }
        // Echo locally so the user sees their message even if the agent
        // hasn't roundtripped it back. The submission number rides on this
        // local copy only, marking it queued until the agent takes it.
        self.push_transcript(stamp_attachments(
            IOMessage::user_text(text.to_string())
                .with_metadata(META_SUBMISSION, self.submissions.to_string()),
            &self.attachments,
        ));
        self.submissions = self.submissions.wrapping_add(1);
        if let Some(history) = &mut self.history {
            history.record(text);
        }
        if self.pending_confirm.is_some() {
            self.confirm_notice = Some("queued for the next turn — still waiting on y/n".into());
        }
        true
    }

    /// Dispatch `invocation` to the host as a command.
    ///
    /// The host executes it and answers on the ordinary channel; whether the
    /// name is one it knows is the host's to say, since only the host knows
    /// its set. The line is echoed locally exactly like a prompt, queued
    /// marker included, so the user sees what was sent and when it was taken.
    fn submit_command(&mut self, invocation: CommandInvocation, raw: &str) -> bool {
        if !self.offer(invocation.into_message(raw)) {
            return false;
        }
        self.push_transcript(
            IOMessage::user_text(raw.to_string())
                .with_metadata(META_SUBMISSION, self.submissions.to_string()),
        );
        self.submissions = self.submissions.wrapping_add(1);
        if self.pending_confirm.is_some() {
            self.confirm_notice = Some("command queued until the approval is answered".into());
        }
        true
    }

    // — Completion popup ————————————————————————————————————————

    /// Re-derive the popup from the draft after any edit.
    ///
    /// The token under the caret decides everything: no token, or one whose
    /// trigger the host has not equipped (no advertised commands, no mention
    /// source), closes the popup and cancels any query still running. A
    /// command token filters the cached set synchronously. A mention token
    /// starts a query only when the query text changed, so cursor motion
    /// inside a token does not re-walk the disk.
    fn refresh_popup(&mut self) {
        let argument = self.commands.as_deref().and_then(|commands| {
            active_command_argument_token(self.composer.text(), self.composer.cursor(), commands)
        });
        let token = argument
            .as_ref()
            .map(|(token, _)| token.clone())
            .or_else(|| active_token(self.composer.text(), self.composer.cursor()));
        let equipped = match token.as_ref().map(|token| token.trigger) {
            Some(Trigger::Command) => self.commands.is_some(),
            Some(Trigger::CommandArgument) => argument.is_some(),
            Some(Trigger::Mention) => self.completion_source.is_some(),
            None => false,
        };
        if !equipped || self.popup_suppressed {
            self.close_popup();
            return;
        }
        let token = token.expect("equipped implies a token");
        match token.trigger {
            Trigger::Command => {
                // Keep the highlight across cursor motion; a changed query
                // re-ranks, so the highlight goes back to the best match.
                let mut popup = match self.popup.take() {
                    Some(popup)
                        if popup.token.trigger == Trigger::Command
                            && popup.token.query == token.query =>
                    {
                        CompletionPopup { token, ..popup }
                    }
                    _ => CompletionPopup::new(token),
                };
                if let Some(commands) = &self.commands {
                    popup.filter_commands(commands);
                }
                self.popup = Some(popup);
            }
            Trigger::CommandArgument => {
                let choices = argument.expect("argument trigger has choices").1;
                let mut popup = CompletionPopup::new(token);
                popup.filter_choices(&choices);
                self.popup = Some(popup);
            }
            Trigger::Mention => {
                let unchanged = self.popup.as_ref().is_some_and(|popup| {
                    popup.token.trigger == Trigger::Mention
                        && popup.token.anchor == token.anchor
                        && popup.token.query == token.query
                });
                if unchanged {
                    if let Some(popup) = &mut self.popup {
                        popup.token = token;
                    }
                    return;
                }
                let query = token.query.clone();
                let mut popup = CompletionPopup::new(token);
                popup.loading = true;
                self.popup = Some(popup);
                self.start_mention_query(query);
            }
        }
    }

    /// Close the popup and abandon any query it was waiting on.
    fn close_popup(&mut self) {
        self.popup = None;
        // Dropping the request cancels the token and aborts the task.
        self.mention_request = None;
    }

    /// Ask the host for candidates matching `query`, replacing any query in
    /// flight. The result lands in `mention_results` tagged with this
    /// request's sequence number; the panel applies it only if no newer
    /// query has started since.
    fn start_mention_query(&mut self, query: String) {
        let Some(source) = self.completion_source.clone() else {
            return;
        };
        self.mention_seq = self.mention_seq.wrapping_add(1);
        let seq = self.mention_seq;
        let cancel = CancelToken::new();
        let results = self.mention_results.clone();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            let items = source.complete(query, token.clone()).await;
            if token.is_cancelled() {
                return;
            }
            let mut slot = results.lock();
            // Never let an older query's rows overwrite a newer query's.
            if slot.as_ref().is_none_or(|(existing, _)| *existing < seq) {
                *slot = Some((seq, items));
            }
        });
        // Assigning drops the previous request, cancelling it.
        self.mention_request = Some(MentionRequest { seq, cancel, task });
    }

    /// Pick up a finished mention query. Rows tagged with anything but the
    /// current request's sequence number are stale — the user kept typing —
    /// and are dropped without touching the popup.
    fn drain_mention_results(&mut self) {
        let Some((seq, items)) = self.mention_results.lock().take() else {
            return;
        };
        let current = self.mention_request.as_ref().map(|request| request.seq);
        if current != Some(seq) {
            return;
        }
        self.mention_request = None;
        if let Some(popup) = &mut self.popup
            && popup.token.trigger == Trigger::Mention
        {
            popup.set_items(items);
        }
    }

    // — Attachments ————————————————————————————————————————————

    /// Re-derive the chip set from the draft after an edit.
    ///
    /// A chip that came from a `@path` mention lives exactly as long as that
    /// mention does, so a chip whose mention has been edited away goes with
    /// it, and so does the memo that stopped it being re-resolved. Each
    /// remaining mention is offered to the host **once**: a path that resolved
    /// to an ordinary text file is not asked about again on the next
    /// keystroke.
    ///
    /// The mention under the caret is resolved like any other, half-typed or
    /// not. Skipping it would save a few resolves of `@d`, `@di`, `@dia` — and
    /// would then drop the attachment of anyone who finishes a path and presses
    /// `Enter` without typing a space first, which is the ordinary way to send
    /// one. A prefix that names nothing comes back
    /// [`NotAttachable`](AttachmentRejected::NotAttachable), which is silent.
    fn refresh_attachments(&mut self) {
        if self.attachment_source.is_none() {
            return;
        }
        let text = self.composer.text().to_string();
        let settled: Vec<String> = parse_mentions(&text)
            .into_iter()
            .map(|mention| mention.path)
            .collect();
        self.attachments.retain(|chip| match &chip.mention {
            Some(path) => settled.contains(path),
            None => true,
        });
        self.attachment_settled
            .retain(|path| settled.contains(path));
        self.attachment_requests
            .retain(|request| match &request.origin {
                AttachmentOrigin::Mention(path) => settled.contains(path),
                AttachmentOrigin::Clipboard => true,
            });
        for path in settled {
            if self.knows_attachment(&path) {
                continue;
            }
            self.start_attachment(AttachmentOrigin::Mention(path));
        }
    }

    /// Whether this mentioned path already has a chip, a settled answer, or a
    /// resolve in flight.
    fn knows_attachment(&self, path: &str) -> bool {
        self.attachment_settled.iter().any(|known| known == path)
            || self
                .attachments
                .iter()
                .any(|chip| chip.mention.as_deref() == Some(path))
            || self
                .attachment_requests
                .iter()
                .any(|request| request.origin == AttachmentOrigin::Mention(path.to_string()))
    }

    /// Ask the host to resolve one attachment, off the UI task.
    fn start_attachment(&mut self, origin: AttachmentOrigin) {
        let Some(source) = self.attachment_source.clone() else {
            return;
        };
        let request = match &origin {
            AttachmentOrigin::Mention(path) => AttachmentRequest::Path(path.clone()),
            AttachmentOrigin::Clipboard => AttachmentRequest::Clipboard,
        };
        self.attachment_seq = self.attachment_seq.wrapping_add(1);
        let seq = self.attachment_seq;
        let cancel = CancelToken::new();
        let token = cancel.clone();
        let results = self.attachment_results.clone();
        let task = tokio::spawn(async move {
            let outcome = source.resolve(request, token.clone()).await;
            if token.is_cancelled() {
                return;
            }
            results.lock().push((seq, outcome));
        });
        self.attachment_requests.push(AttachmentRequest_ {
            seq,
            origin,
            cancel,
            task,
        });
    }

    /// Pick up finished resolves. An outcome whose request is gone — its
    /// mention left the draft while the host was working — is dropped.
    fn drain_attachment_results(&mut self) {
        let landed = {
            let mut slot = self.attachment_results.lock();
            if slot.is_empty() {
                return;
            }
            std::mem::take(&mut *slot)
        };
        for (seq, outcome) in landed {
            let Some(index) = self
                .attachment_requests
                .iter()
                .position(|request| request.seq == seq)
            else {
                continue;
            };
            let request = self.attachment_requests.remove(index);
            if let AttachmentOrigin::Mention(path) = &request.origin {
                self.attachment_settled.push(path.clone());
            }
            match outcome {
                Ok(mut attachment) => {
                    // The panel, not the host, decides what a chip is tied to:
                    // a mention-origin chip follows its mention and a
                    // clipboard chip stands alone, whatever the host filled in.
                    attachment.mention = match &request.origin {
                        AttachmentOrigin::Mention(path) => Some(path.clone()),
                        AttachmentOrigin::Clipboard => None,
                    };
                    self.attachment_notice = None;
                    if !self
                        .attachments
                        .iter()
                        .any(|chip| chip.handle == attachment.handle)
                    {
                        self.attachments.push(attachment);
                    }
                }
                Err(rejection) => {
                    if let Some(reason) = rejection.notice() {
                        self.attachment_notice = Some(reason.to_string());
                    }
                }
            }
        }
    }

    /// Ask the host for a clipboard image.
    ///
    /// The draft is not touched either way — a refusal costs the user nothing
    /// but the keystroke, which is the whole point of attaching being separate
    /// from submitting.
    fn attach_from_clipboard(&mut self) {
        if self.attachment_source.is_none() {
            self.attachment_notice =
                Some("Attachments are not configured by this host".to_string());
            return;
        }
        self.attachment_notice = None;
        self.start_attachment(AttachmentOrigin::Clipboard);
    }

    /// Remove the most recently added chip. Returns whether there was one.
    ///
    /// A chip that came from a mention takes its mention text with it — every
    /// occurrence of that path, because the chip stands for the attachment and
    /// not for one particular place the path was typed. Leaving a mention
    /// behind would leave the draft claiming an attachment that is no longer
    /// going anywhere.
    fn remove_last_attachment(&mut self) -> bool {
        let Some(chip) = self.attachments.pop() else {
            return false;
        };
        if let Some(path) = &chip.mention {
            let mut spans: Vec<(usize, usize)> = parse_mentions(self.composer.text())
                .into_iter()
                .filter(|mention| &mention.path == path)
                .map(|mention| (mention.start, mention.end))
                .collect();
            // Back to front, so an earlier deletion cannot invalidate a later
            // offset.
            spans.reverse();
            for (from, to) in spans {
                let mut to = to;
                // Take one trailing space with it, so removing a mention from
                // the middle of a sentence does not leave a double space.
                if self.composer.text()[to..].starts_with(' ') {
                    to += 1;
                }
                self.composer.delete_range(from, to);
            }
            self.attachment_settled.retain(|known| known != path);
        }
        self.attachment_notice = None;
        self.refresh_popup();
        self.refresh_attachments();
        true
    }

    /// Forget every chip and every in-flight resolve.
    fn clear_attachments(&mut self) {
        self.attachments.clear();
        self.attachment_settled.clear();
        self.attachment_requests.clear();
        self.attachment_notice = None;
    }

    /// The handles to stamp on the next submission, in chip order.
    fn attachment_handles(&self) -> Vec<String> {
        self.attachments
            .iter()
            .map(|chip| chip.handle.clone())
            .collect()
    }

    /// The chip row shown under the draft: one line, however many chips, so a
    /// long list scrolls off the right rather than eating the transcript.
    fn attachment_line(&self) -> Option<Line<'static>> {
        if self.attachments.is_empty() {
            return None;
        }
        let mut spans = vec![Span::styled(
            "attached ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        for (index, chip) in self.attachments.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!("[{}]", safe_text(&chip.summary())),
                Style::default().fg(Color::Cyan),
            ));
        }
        spans.push(Span::styled(
            "  Ctrl-X removes the last",
            Style::default().fg(Color::DarkGray),
        ));
        Some(Line::from(spans))
    }

    /// Accept the highlighted candidate into the draft. Returns whether
    /// anything was inserted.
    fn accept_completion(&mut self) -> bool {
        let Some(edit) = self.popup.as_ref().and_then(CompletionPopup::acceptance) else {
            return false;
        };
        self.composer.delete_range(edit.from, edit.to);
        self.composer.insert_paste(&edit.insert);
        self.refresh_popup();
        self.refresh_attachments();
        true
    }

    /// Dismiss the popup until the draft changes again.
    fn dismiss_popup(&mut self) {
        self.popup_suppressed = true;
        self.close_popup();
    }

    /// Route a key to the open popup. `None` means the popup does not claim
    /// it and ordinary handling proceeds.
    fn handle_popup_key(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        let popup = self.popup.as_mut()?;
        let plain = key.modifiers.is_empty();
        match key.code {
            KeyCode::Tab if plain => {
                self.accept_completion();
                Some(EventOutcome::Consumed)
            }
            KeyCode::BackTab | KeyCode::Up => {
                popup.select_prev();
                Some(EventOutcome::Consumed)
            }
            KeyCode::Down => {
                popup.select_next();
                Some(EventOutcome::Consumed)
            }
            KeyCode::Esc => {
                self.dismiss_popup();
                Some(EventOutcome::Consumed)
            }
            // Enter fills in the highlighted candidate — unless the query
            // already spells it, in which case there is nothing to fill and
            // the press submits like any other.
            KeyCode::Enter if plain && popup.selected_item().is_some() => {
                if popup.query_is_exact() {
                    return None;
                }
                self.accept_completion();
                Some(EventOutcome::Consumed)
            }
            _ => None,
        }
    }

    /// Rows of the open popup: one per candidate, the highlighted one
    /// inverted, or a single notice row when there are none.
    fn popup_lines(&self) -> Vec<Line<'static>> {
        let Some(popup) = &self.popup else {
            return Vec::new();
        };
        if let Some(notice) = popup.notice() {
            let text = match (notice, popup.token.trigger) {
                (PopupNotice::Searching, _) => "searching…",
                (PopupNotice::NoMatches, Trigger::Command) => "no matching commands",
                (PopupNotice::NoMatches, Trigger::CommandArgument) => "no matching choices",
                (PopupNotice::NoMatches, Trigger::Mention) => "no matching files",
            };
            return vec![Line::from(Span::styled(
                format!(" {text}"),
                Style::default().fg(Color::DarkGray),
            ))];
        }
        // Keep the highlighted row on screen when the list is longer than
        // the popup: show the window of rows around it.
        let window = POPUP_MAX_ROWS;
        let first = popup
            .selected
            .saturating_sub(window - 1)
            .min(popup.items.len().saturating_sub(window));
        let label_width = popup
            .items
            .iter()
            .map(|item| unicode_width::UnicodeWidthStr::width(item.label.as_str()))
            .max()
            .unwrap_or(0)
            .min(40);
        popup
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(window)
            .map(|(index, item)| {
                let selected = index == popup.selected;
                let marker = if selected { "❯ " } else { "  " };
                let label = safe_text(&item.label);
                let pad = label_width
                    .saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()));
                let mut spans = vec![Span::styled(
                    format!("{marker}{label}{}", " ".repeat(pad)),
                    if selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                )];
                if !item.detail.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", safe_text(&item.detail)),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }

    #[cfg(test)]
    fn message_lines(&self) -> Vec<Line<'static>> {
        build_message_lines(
            &self.transcript,
            self.consumed.load(Ordering::Relaxed),
            self.expanded,
        )
    }

    /// Cheap, monotonic-ish fingerprint of the content the scroll view renders,
    /// handed to [`ScrollableTranscript`] so it re-wraps only on a real change.
    ///
    /// `content_rev` already covers transcript/timeline mutations; when the
    /// timeline is expanded its body also includes the shared [`LogBuffer`],
    /// which grows asynchronously, so the buffer's length (an `O(1)` read) and
    /// the node count are folded in too. `expanded` flips the view entirely, so
    /// it's part of the key.
    fn view_revision(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if self.browse.page.is_some() {
            self.browse.history_revision.hash(&mut hasher);
        } else {
            self.content_rev.hash(&mut hasher);
        }
        self.browse.page.is_some().hash(&mut hasher);
        self.browse.selected.hash(&mut hasher);
        self.expanded.hash(&mut hasher);
        // A prompt losing its "queued" badge changes rendered rows without
        // touching the transcript, so the consumed count is part of the key.
        self.consumed.load(Ordering::Relaxed).hash(&mut hasher);
        if self.expanded {
            self.timeline.len().hash(&mut hasher);
            self.log_buffer
                .as_ref()
                .map_or(0, LogBuffer::len)
                .hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Render a message body as one string, whatever its content shape.
fn other_body(content: &IOContent) -> String {
    match content {
        IOContent::Text(text) => text.clone(),
        IOContent::Structured(value) => value.to_string(),
        IOContent::Binary { mime_type, data } => {
            format!("<binary {} ({} bytes)>", mime_type, data.len())
        }
    }
}

/// The glyph, label suffix and colour a tool block's phase renders with.
///
/// The panel knows the lifecycle, not the tools: `edit_file` and `run_command`
/// are strings it prints. Phases it does not recognise keep the raw value, so a
/// host that adds one stays legible instead of rendering blank.
fn phase_style(phase: &str) -> (&'static str, Color) {
    match phase {
        "pending" => ("…", Color::Yellow),
        "start" | "running" => ("⋯", Color::Yellow),
        "ok" => ("✓", Color::Green),
        "denied" => ("⊘", Color::Magenta),
        "error" => ("✗", Color::Red),
        _ => ("·", Color::Yellow),
    }
}

/// Format a tool block's elapsed milliseconds for display beside its label.
fn fmt_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

fn unknown_message(message: &IOMessage) -> bool {
    message.metadata.get(IO_TYPE).is_some_and(|kind| {
        !matches!(
            kind.as_str(),
            "message"
                | "tool"
                | "confirm"
                | "error"
                | "status"
                | "tokens"
                | TYPE_INTERRUPTING
                | TYPE_CANCELLED
        )
    })
}

/// Build the transcript's logical lines: one labelled block per message, split
/// into one [`Line`] per physical line (see [`split_lines`]) so embedded
/// newlines wrap rather than render as control glyphs. A free function (rather
/// than a `&self` method) so [`ScrollableTranscript::render`]'s build closure
/// can borrow just the transcript, disjoint from the `&mut` view.
fn build_message_lines(
    transcript: &VecDeque<IOMessage>,
    consumed: u64,
    expanded: bool,
) -> Vec<Line<'static>> {
    build_message_layout(transcript, consumed, expanded).0
}

fn build_message_layout(
    transcript: &VecDeque<IOMessage>,
    consumed: u64,
    expanded: bool,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let mut starts = Vec::new();
    // Message bodies are agent/user-controlled, but terminal escape and control
    // chars are neutralised downstream by ratatui-core (`Span::styled_graphemes`
    // / `Buffer::set_stringn`), so raw text can't inject cursor moves or colour
    // codes into the alternate screen. Don't drop that filter.
    let mut lines = Vec::with_capacity(transcript.len() * 2);
    for (message_index, message) in transcript.iter().enumerate() {
        if unknown_message(message) && !expanded {
            continue;
        }
        starts.push((message_index, lines.len()));
        // An approval request is the one message labelled by kind rather than
        // source: it carries the command or edit about to run, so it must not
        // read as ordinary system chatter scrolling by.
        let is_confirm = message.metadata.get(IO_TYPE).map(String::as_str) == Some(TYPE_CONFIRM);
        let kind = message.metadata.get(IO_TYPE).map(String::as_str);
        let (label, color) = match &message.source {
            _ if kind == Some(TYPE_INTERRUPTING) => ("Interrupting".to_string(), Color::Yellow),
            _ if kind == Some(TYPE_CANCELLED) => ("Cancelled".to_string(), Color::DarkGray),
            _ if kind == Some("error") => ("error".to_string(), Color::Red),
            // A tool block reads as one activity line: what ran, how it ended,
            // and how long it took. The phase glyph carries the state so the
            // row stays scannable once several have scrolled past.
            _ if kind == Some(TYPE_TOOL) => {
                let phase = message.metadata.get(META_PHASE).map_or("", String::as_str);
                let (glyph, phase_color) = phase_style(phase);
                let elapsed = message
                    .metadata
                    .get(META_ELAPSED_MS)
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|_| !matches!(phase, "pending" | "start" | "running"))
                    .map(|ms| format!(" · {}", fmt_elapsed(ms)))
                    .unwrap_or_default();
                (
                    format!(
                        "{glyph} {}{elapsed}",
                        message
                            .metadata
                            .get(META_LABEL)
                            .map_or("tool", String::as_str),
                    ),
                    phase_color,
                )
            }
            _ if is_confirm => ("approve".to_string(), Color::Yellow),
            IOSource::User => ("you".to_string(), Color::Cyan),
            IOSource::Agent(name) => (name.clone(), Color::Green),
            IOSource::External(name) => (name.clone(), Color::Yellow),
            IOSource::System => ("system".to_string(), Color::Magenta),
        };
        let body = other_body(&message.content);
        // A submitted prompt the agent has not taken yet is queued, not
        // delivered. Say so on its last row rather than letting the channel's
        // acceptance stand in for the agent having read it.
        let queued = message
            .metadata
            .get(META_SUBMISSION)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|submission| submission >= consumed);
        let turn_target = message.metadata.get("turn_input").and_then(|mode| {
            message
                .metadata
                .get("target_run")
                .map(|run| format!("  · {mode} for run {run}"))
        });
        let attached = Some(attachment_handles(message).len()).filter(|count| *count > 0);
        // A `Span` is a single styled run; ratatui only breaks *between* `Line`s,
        // so an embedded `\n` would render as a control glyph rather than a wrap.
        // Split the body into one `Line` per physical line: the prefix labels the
        // first, continuations indent to align.
        // A tool the agent requested inside a message is shown as a child of
        // it, so a turn reads as one answer with its actions under it rather
        // than as a flat interleaving.
        let nest = if kind == Some(TYPE_TOOL) && message.metadata.contains_key(META_PARENT) {
            " ".repeat(TOOL_INDENT)
        } else {
            String::new()
        };
        let prefix = format!("{nest}{} ❯ ", safe_text(&label));
        let indent = " ".repeat(unicode_width::UnicodeWidthStr::width(prefix.as_str()));
        let rich = matches!(kind, Some(TYPE_MESSAGE | "error"));
        let mut rendered = if matches!(kind, Some(TYPE_TOOL | TYPE_CONFIRM)) {
            preview_lines(&body, expanded)
        } else {
            body_lines(&body, rich, expanded)
        };
        // A block that ended before its answer did keeps the text that did
        // arrive and says so, rather than trailing off as if more were coming.
        if message.metadata.get(META_OUTCOME).map(String::as_str) == Some(OUTCOME_ERROR) {
            rendered.push(Line::styled(
                "⚠ response ended early",
                Style::default().fg(Color::Red),
            ));
        }
        if kind == Some("error") {
            for line in &mut rendered {
                line.spans
                    .insert(0, Span::styled("│ ", Style::default().fg(Color::Red)));
            }
        }
        let last = rendered.len().saturating_sub(1);
        for (index, mut line) in rendered.into_iter().enumerate() {
            let lead = if index == 0 {
                Span::styled(
                    prefix.clone(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(indent.clone())
            };
            line.spans.insert(0, lead);
            if queued && index == last {
                line.spans.push(Span::styled(
                    "  · queued",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            // A clipboard attachment leaves no `@path` in the prompt text, so
            // without this the transcript would show a bare sentence and no
            // sign that an image went with it.
            if index == last
                && let Some(count) = attached
            {
                line.spans.push(Span::styled(
                    format!("  · {count} attached"),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if index == last
                && let Some(target) = &turn_target
            {
                line.spans.push(Span::styled(
                    target.clone(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(line);
        }
    }
    (lines, starts)
}

/// Render the per-turn node timeline (shown when expanded). Each node is a
/// header line followed by the `tracing` events captured within its span —
/// correlated from the shared [`LogBuffer`] by `(node_name, seq)`, restricted to
/// framework targets (`polaris*`). Multi-line messages break into one row each.
/// Without a log buffer, only the headers show.
///
/// A free function (rather than a `&self` method) so the scroll view's build
/// closure can borrow just the timeline + log buffer, disjoint from the `&mut`
/// view.
fn build_timeline_lines(
    timeline: &[TimelineNode],
    log_buffer: Option<&LogBuffer>,
) -> Vec<Line<'static>> {
    if timeline.is_empty() {
        return vec![Line::from(Span::styled(
            "no node activity yet",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    let logs = log_buffer.map(LogBuffer::snapshot).unwrap_or_default();
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::with_capacity(timeline.len() * 2);
    for node in timeline {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} · {}", node.seq, node.node_name),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", node.label), dim),
        ]));
        for entry in logs.iter().filter(|entry| {
            entry.node.as_deref() == Some(node.node_name.as_str())
                && entry.seq == Some(node.seq)
                // Only events emitted from within the framework itself —
                // `polaris_*` crate module paths and explicit `polaris::*`
                // targets. Drops noise from dependencies (hyper, reqwest, …)
                // and consumer code that merely ran under a node span.
                && entry.target.starts_with(FRAMEWORK_TARGET_PREFIX)
        }) {
            // Multi-line log messages (e.g. an error with context) break into one
            // row each; the level badge leads the first, the rest indent under the
            // message column.
            for (i, segment) in split_lines(&entry.message).enumerate() {
                let badge = if i == 0 {
                    Span::styled(
                        entry.level.label(),
                        Style::default().fg(entry.level.color()),
                    )
                } else {
                    Span::raw("     ")
                };
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    badge,
                    Span::raw(" "),
                    Span::raw(segment.to_string()),
                ]));
            }
        }
    }
    lines
}

impl TuiPanel for ConversationPanel {
    fn attach_runtime(&mut self, handle: TuiHandle) {
        self.runtime = Some(handle);
    }

    fn id(&self) -> &str {
        "conversation"
    }
    fn title(&self) -> &str {
        "Conversation"
    }

    fn tick(&mut self) {
        self.poll_composer_services();
        self.drain_inbox();
        self.drain_mention_results();
        self.drain_attachment_results();
        self.drain_browse_results();
        if self.status.is_some() {
            self.spinner = self.spinner.wrapping_add(1);
        }
    }

    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
        self.poll_composer_services();
        match event {
            TuiEvent::FocusGained => {
                self.focused = true;
                self.drain_inbox();
                EventOutcome::Continue
            }
            TuiEvent::FocusLost => {
                self.focused = false;
                EventOutcome::Continue
            }
            TuiEvent::Key(key) => self.handle_key(*key),
            // A bracketed paste is inserted whole and never interpreted. Its
            // newlines do not submit and a leading `/` does not dispatch —
            // that is the entire reason the runtime brackets it rather than
            // letting it arrive as a burst of key events.
            TuiEvent::Paste(text) => {
                if self.editor_result.is_some() {
                    return EventOutcome::Consumed;
                }
                if let Some(history) = &mut self.history
                    && history.searching()
                {
                    history.query_insert(&mut self.composer, text);
                } else if self.browse_paste(text) {
                    return EventOutcome::Consumed;
                } else {
                    self.composer.insert_paste(text);
                    self.popup_suppressed = false;
                    self.refresh_popup();
                }
                EventOutcome::Consumed
            }
            // Mouse-wheel scroll, and a left click/drag on the scrollbar
            // gutter, over the transcript / expanded timeline. Only arrives
            // when the host enables mouse capture; a no-op otherwise. Both
            // views share `self.view`, so both act on whichever is on.
            TuiEvent::Mouse(mouse) => {
                let intent = match mouse.kind {
                    MouseEventKind::ScrollUp => Some(NavIntent::LineUp),
                    MouseEventKind::ScrollDown => Some(NavIntent::LineDown),
                    _ => None,
                };
                match intent {
                    Some(intent) => {
                        // A few lines per notch — one row per event crawls.
                        for _ in 0..WHEEL_LINES {
                            self.view.apply(intent);
                        }
                        EventOutcome::Consumed
                    }
                    // Every other kind — including a left click/drag anywhere
                    // outside the gutter — passes through so the terminal's
                    // native text-selection keeps working over the
                    // transcript. `drag_gutter` itself is the only thing that
                    // knows the gutter's column, so this can't accidentally
                    // widen what a click consumes past that one column.
                    None => {
                        let press = matches!(
                            mouse.kind,
                            MouseEventKind::Down(MouseButton::Left)
                                | MouseEventKind::Drag(MouseButton::Left)
                        );
                        if press && self.view.drag_gutter(mouse.column, mouse.row) {
                            EventOutcome::Consumed
                        } else {
                            EventOutcome::Continue
                        }
                    }
                }
            }
            _ => EventOutcome::Continue,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.drain_inbox();
        self.drain_mention_results();
        self.drain_attachment_results();
        self.drain_browse_results();
        // The meta line is shown only once a status/token message has arrived,
        // so panels that don't drive that protocol look like a plain
        // transcript + input. When hidden, its row collapses to zero height.
        let meta_height = u16::from(self.meta_active);
        // Approval and delivery banners take a row each, only while they have
        // something to report; the block collapses to zero height otherwise.
        let mut banner_lines = self.banner_lines();
        if let Some(line) = self.browse_banner() {
            banner_lines.push(line);
        }
        let banner_height = u16::try_from(banner_lines.len()).unwrap_or(u16::MAX);
        // The composer grows with its content and then scrolls inside a fixed
        // frame. Its width is known before layout (the area's, less borders),
        // so the wrapped row count — and therefore the height it wants — can be
        // measured up front.
        let composer_width = usize::from(area.width.saturating_sub(2)).max(1);
        self.composer_width = composer_width;
        let composer_rows = self.composer.rows(composer_width);
        // Never grow so far that the transcript is squeezed out: reserve the
        // meta and banner rows, the composer's own borders, and one row of
        // transcript before spending anything on the draft.
        let reserved = usize::from(meta_height) + usize::from(banner_height) + 3;
        let budget = usize::from(area.height).saturating_sub(reserved);
        let max_rows = budget.clamp(1, COMPOSER_MAX_ROWS);
        let visible_rows = composer_rows.len().clamp(1, max_rows);
        let composer_height = u16::try_from(visible_rows + 2).unwrap_or(u16::MAX);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(meta_height),
                Constraint::Length(banner_height),
                Constraint::Length(composer_height),
            ])
            .split(area);

        // Ctrl-O reveals retained messages, full diffs and the node timeline. Only one
        // view shows at a time, so they share the scroll viewport. The view
        // re-wraps only when `revision` changes; the build closure runs solely on
        // a cache miss, so an idle frame rebuilds nothing. Borrow the content
        // fields individually so the closure stays disjoint from `&mut view`.
        let revision = self.view_revision();
        let consumed = self.consumed.load(Ordering::Relaxed);
        let view = &mut self.view;
        let history_page = self.browse.page.as_ref();
        let transcript = history_page.map_or(&self.transcript, |page| &page.messages);
        let selected = self.browse.selected;
        let starts = &mut self.browse.starts;
        let timeline = &self.timeline;
        let log_buffer = self.log_buffer.as_ref();
        let expanded = self.expanded;
        view.render(frame, chunks[0], revision, || {
            let (mut lines, message_starts) = build_message_layout(transcript, consumed, expanded);
            if let Some(selected) = selected
                && let Some(position) = message_starts.iter().position(|(i, _)| *i == selected)
            {
                let from = message_starts[position].1;
                let end = message_starts
                    .get(position + 1)
                    .map_or(lines.len(), |(_, line)| *line);
                for line in &mut lines[from..end] {
                    line.style = line.style.bg(Color::DarkGray);
                }
            }
            *starts = message_starts;
            if expanded && history_page.is_none() {
                lines.extend(build_timeline_lines(timeline, log_buffer));
            }
            lines
        });

        // The completion popup sits directly above the meta and banner rows,
        // overlaying the bottom of the transcript. It never covers the
        // approval banner: that row keeps its own slot, and the popup only
        // borrows transcript rows. Skipped when there is no room to draw a
        // bordered box at all.
        if let Some(popup) = &self.popup {
            let rows = self.popup_lines();
            let height = u16::try_from(rows.len() + 2).unwrap_or(u16::MAX);
            if chunks[0].height >= height && chunks[0].width >= 4 {
                let area = Rect {
                    x: chunks[0].x,
                    y: chunks[0].y + chunks[0].height - height,
                    width: chunks[0].width,
                    height,
                };
                let (title, hint) = match popup.token.trigger {
                    Trigger::Command => (" commands ", " Tab accepts · Esc closes "),
                    Trigger::CommandArgument => (" choices ", " Tab accepts · Esc closes "),
                    Trigger::Mention => (" files ", " Tab accepts · Esc closes "),
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_bottom(Line::from(hint).right_aligned())
                    .border_style(Style::default().fg(Color::Cyan));
                let inner = block.inner(area);
                frame.render_widget(Clear, area);
                frame.render_widget(block, area);
                frame.render_widget(Paragraph::new(rows), inner);
            }
        }

        if self.meta_active {
            frame.render_widget(
                Paragraph::new(self.meta_line_for_width(chunks[1].width)),
                chunks[1],
            );
        }

        if !banner_lines.is_empty() {
            frame.render_widget(Paragraph::new(banner_lines), chunks[2]);
        }

        // A multi-line draft says so in its title: `Enter` submits the whole
        // thing, and the row count is the only cue that there is more above
        // the fold once the draft outgrows the frame.
        let title = match (self.focused, self.composer.is_multiline()) {
            (true, true) => format!(" › {} lines ", self.composer.text().split('\n').count()),
            (true, false) => " › · Ctrl-G browse ".to_string(),
            (false, _) => " input ".to_string(),
        };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if self.focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let input_inner = input_block.inner(chunks[3]);
        frame.render_widget(input_block, chunks[3]);

        // Keep the caret on screen: scroll only as far as it takes, and never
        // past the last row, so a shrinking draft pulls the view back down.
        let (caret_row, caret_col) = self.composer.caret(composer_width);
        if caret_row < self.composer_scroll {
            self.composer_scroll = caret_row;
        } else if caret_row >= self.composer_scroll + visible_rows {
            self.composer_scroll = caret_row + 1 - visible_rows;
        }
        self.composer_scroll = self
            .composer_scroll
            .min(composer_rows.len().saturating_sub(visible_rows));

        let lines: Vec<Line<'_>> = composer_rows
            .iter()
            .skip(self.composer_scroll)
            .take(visible_rows)
            .map(|row| Line::from(self.composer.render_row(*row)))
            .collect();
        frame.render_widget(Paragraph::new(lines), input_inner);

        if self.focused && !self.browse.active && input_inner.width > 0 && input_inner.height > 0 {
            // The caret is placed in display columns, which is why the composer
            // measures glyph widths rather than counting chars: a draft of CJK
            // or emoji otherwise drifts one cell per wide glyph.
            let col = u16::try_from(caret_col).unwrap_or(u16::MAX);
            let row =
                u16::try_from(caret_row.saturating_sub(self.composer_scroll)).unwrap_or(u16::MAX);
            frame.set_cursor_position((
                input_inner.x + col.min(input_inner.width.saturating_sub(1)),
                input_inner.y + row.min(input_inner.height.saturating_sub(1)),
            ));
        }
    }
}

impl ConversationPanel {
    fn handle_key(&mut self, key: KeyEvent) -> EventOutcome {
        self.poll_composer_services();
        if self.editor_result.is_some() {
            return EventOutcome::Consumed;
        }
        // Prompt-history search is checked before transcript browsing so that
        // its own Ctrl-G cancel keeps working: only one modal composer state is
        // ever live, and browsing cannot be entered from inside a search.
        if self.history.as_ref().is_some_and(History::searching) {
            return self.search_key(key);
        }
        if let Some(outcome) = self.browse_key(key) {
            return outcome;
        }
        // An open popup gets first refusal on the keys it claims. Everything
        // it declines is an ordinary edit, after which the popup is
        // re-derived from the draft — a changed draft also lifts an `Esc`.
        if let Some(outcome) = self.handle_popup_key(key) {
            return outcome;
        }
        let before_text = self.composer.text().to_string();
        let before_cursor = self.composer.cursor();
        let outcome = self.handle_edit_key(key);
        let text_changed = self.composer.text() != before_text;
        if text_changed {
            self.popup_suppressed = false;
        }
        if text_changed || self.composer.cursor() != before_cursor {
            self.refresh_popup();
            self.refresh_attachments();
        }
        outcome
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> EventOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let superk = key.modifiers.contains(KeyModifiers::SUPER);
        let width = self.composer_width;
        match (key.code, ctrl, alt) {
            (KeyCode::Char('r'), true, false) => {
                if let Some(history) = &mut self.history {
                    history.search(&mut self.composer);
                } else {
                    self.editor_notice =
                        Some("Prompt history is not configured by this host".into());
                }
                EventOutcome::Consumed
            }
            (KeyCode::Char('g'), true, false) => {
                self.open_editor();
                EventOutcome::Consumed
            }
            (KeyCode::Esc, false, false) if self.history_cancel() => EventOutcome::Consumed,
            // — Newline vs submit.
            //
            // `Shift+Enter` is the documented chord, and reaches us only when
            // the keyboard enhancement protocol is negotiated; `Alt+Enter` and
            // `Super+Enter` are accepted too because which of the three a
            // terminal reports varies, and accepting all three costs nothing.
            //
            // On a terminal with no enhancement *none* of them arrive — every
            // modifier+Enter is indistinguishable from a bare `Enter` — so a
            // trailing backslash continues the line instead. That fallback is
            // load-bearing, not a nicety: it is the only way to reach a second
            // line in stock Terminal.app.
            (KeyCode::Enter, false, true) => {
                self.composer.insert_newline();
                EventOutcome::Consumed
            }
            (KeyCode::Enter, true, false) => {
                self.submit_input_mode(TurnInputMode::Steer);
                EventOutcome::Consumed
            }
            (KeyCode::Enter, false, false) if shift || superk => {
                self.composer.insert_newline();
                EventOutcome::Consumed
            }
            (KeyCode::Enter, false, false) => {
                if !self.composer.take_line_continuation() {
                    self.submit_input();
                }
                EventOutcome::Consumed
            }

            // Esc denies an open approval — the fast, safe answer. With no
            // dialog open it falls through to the host, which owns the key.
            (KeyCode::Esc, false, false) if self.pending_confirm.is_some() => {
                self.answer_confirmation("no", false);
                EventOutcome::Consumed
            }

            // Ctrl-C is a shell fallback chord: the panel claims it only when
            // it has something to act on, and declines otherwise so the shell
            // quits.
            (KeyCode::Char('c'), true, false) => self.interrupt(),

            // Ctrl-T asks the host to background what is running. Claimed only
            // when a host installed a handle, so a host that did not opt in
            // leaves the key to the shell.
            (KeyCode::Char('t'), true, false) if self.background_handle.is_some() => {
                self.background()
            }
            (KeyCode::Up, false, true) if self.edit_last_turn_input() => EventOutcome::Consumed,
            (KeyCode::Backspace, false, true) if self.remove_last_turn_input() => {
                EventOutcome::Consumed
            }

            // — Word-wise motion: Option/Alt + arrows, and the readline
            //   `Alt-b` / `Alt-f` aliases iTerm sends for the same chords.
            (KeyCode::Left, _, true) | (KeyCode::Char('b'), false, true) => {
                let at = self.composer.prev_word();
                self.composer.seek(at);
                EventOutcome::Consumed
            }
            (KeyCode::Right, _, true) | (KeyCode::Char('f'), false, true) => {
                let at = self.composer.next_word();
                self.composer.seek(at);
                EventOutcome::Consumed
            }

            // — Char-wise motion: arrows plus the emacs `Ctrl-B` / `Ctrl-F`.
            //   Steps by grapheme cluster, so one press crosses one glyph
            //   however many bytes or scalars it happens to be built from.
            (KeyCode::Left, false, false) | (KeyCode::Char('b'), true, false) => {
                self.composer.move_left();
                EventOutcome::Consumed
            }
            (KeyCode::Right, false, false) | (KeyCode::Char('f'), true, false) => {
                self.composer.move_right();
                EventOutcome::Consumed
            }

            // Move inside the draft first; history claims only its boundaries.
            (KeyCode::Up, false, false) if self.composer.move_row_up(width) => {
                EventOutcome::Consumed
            }
            (KeyCode::Down, false, false) if self.composer.move_row_down(width) => {
                EventOutcome::Consumed
            }

            (KeyCode::Up, false, false)
                if self
                    .history
                    .as_mut()
                    .is_some_and(|history| history.previous(&mut self.composer)) =>
            {
                EventOutcome::Consumed
            }
            (KeyCode::Down, false, false)
                if self
                    .history
                    .as_mut()
                    .is_some_and(|history| history.next(&mut self.composer)) =>
            {
                EventOutcome::Consumed
            }

            // — Line ends: Home/End (fn+arrows on macOS), plus emacs Ctrl-A/E.
            //   Bounded to the caret's own logical line, which is identical to
            //   start/end of text while the draft is a single line.
            (KeyCode::Home, ..) | (KeyCode::Char('a'), true, false) => {
                self.composer.move_line_start();
                EventOutcome::Consumed
            }
            (KeyCode::End, ..) | (KeyCode::Char('e'), true, false) => {
                self.composer.move_line_end();
                EventOutcome::Consumed
            }

            // — Word deletion: Option/Alt+Backspace and Ctrl-W delete the word
            //   before the cursor; Option/Alt+Delete and Alt-d the word after.
            (KeyCode::Backspace, false, true) | (KeyCode::Char('w'), true, false) => {
                let (from, to) = (self.composer.prev_word(), self.composer.cursor());
                self.composer.delete_range(from, to);
                EventOutcome::Consumed
            }
            (KeyCode::Delete, false, true) | (KeyCode::Char('d'), false, true) => {
                let (from, to) = (self.composer.cursor(), self.composer.next_word());
                self.composer.delete_range(from, to);
                EventOutcome::Consumed
            }

            // — Char deletion: Backspace back, Delete / emacs Ctrl-D forward.
            (KeyCode::Backspace, false, false) => {
                self.composer.backspace();
                EventOutcome::Consumed
            }
            (KeyCode::Delete, false, false) | (KeyCode::Char('d'), true, false) => {
                self.composer.delete_forward();
                EventOutcome::Consumed
            }

            // — Line kills: Ctrl-U back to the line start (zsh/readline),
            //   Ctrl-K to the line end.
            (KeyCode::Char('u'), true, false) => {
                let at = self.composer.cursor();
                let start = self.composer.line_start(at);
                self.composer.delete_range(start, at);
                EventOutcome::Consumed
            }
            (KeyCode::Char('k'), true, false) => {
                let at = self.composer.cursor();
                let end = self.composer.line_end(at);
                self.composer.delete_range(at, end);
                EventOutcome::Consumed
            }

            (KeyCode::Char('o'), true, false) => {
                // Ctrl-O: reveal hidden content, expand diffs and show the timeline.
                self.expanded = !self.expanded;
                EventOutcome::Consumed
            }

            // — Attachments. `Ctrl-V` is free in a terminal composer: the
            //   system paste is `Cmd-V` / `Ctrl-Shift-V` at the emulator and
            //   arrives as bracketed paste, never as this chord.
            (KeyCode::Char('v'), true, false) => {
                self.attach_from_clipboard();
                EventOutcome::Consumed
            }
            (KeyCode::Char('x'), true, false) => {
                self.remove_last_attachment();
                EventOutcome::Consumed
            }
            (KeyCode::Char(c), false, false) => {
                self.composer.insert_char(c);
                EventOutcome::Consumed
            }
            // Editing keys are handled above; anything left falls through to
            // the transcript navigation map.
            _ => match self.nav.resolve(key) {
                Some(intent) => {
                    self.view.apply(intent);
                    EventOutcome::Consumed
                }
                None => EventOutcome::Continue,
            },
        }
    }
}

/// Plugin that contributes a [`ConversationPanel`] and registers a
/// [`UserIO`] factory pointing at the same [`TuiIOProvider`].
///
/// The panel is registered with [`TuiRegistry`]; the `UserIO` is registered
/// as a local resource via `server.register_local`. Any agent that resolves
/// `Res<UserIO>` or `ResMut<UserIO>` will receive a `UserIO` cloning the
/// same `Arc<TuiIOProvider>`.
pub struct ConversationPanelPlugin {
    buffer: usize,
    log_buffer: Option<LogBuffer>,
    completion_source: Option<Arc<dyn CompletionSource>>,
    io_decorator: Option<IoDecorator>,
    history: Option<PromptHistoryConfig>,
    editor: Option<EditorCommand>,
    history_source: Option<Arc<dyn TranscriptHistory>>,
    interrupt_handle: Option<Arc<dyn Fn() + Send + Sync>>,
    background_handle: Option<Arc<dyn Fn() + Send + Sync>>,
    turn_input: Option<Arc<dyn TurnInputHandle>>,
    attachment_source: Option<Arc<dyn AttachmentSource>>,
}

/// A host hook that builds the [`UserIO`] handed to agents from the panel's
/// provider, so the host can sit between the panel and the agent.
type IoDecorator = Arc<dyn Fn(Arc<TuiIOProvider>) -> UserIO + Send + Sync>;

impl Default for ConversationPanelPlugin {
    fn default() -> Self {
        Self {
            buffer: DEFAULT_CHANNEL_BUFFER,
            log_buffer: None,
            completion_source: None,
            io_decorator: None,
            history: None,
            editor: None,
            history_source: None,
            interrupt_handle: None,
            background_handle: None,
            turn_input: None,
            attachment_source: None,
        }
    }
}

impl ConversationPanelPlugin {
    /// Enable prompt history using the host's persistence and explicit namespace.
    /// See [`PromptHistoryStore`](crate::PromptHistoryStore) for the store contract.
    #[must_use]
    pub fn with_history(mut self, history: PromptHistoryConfig) -> Self {
        self.history = Some(history);
        self
    }

    /// Override VISUAL/EDITOR with a literal executable and argument vector.
    #[must_use]
    pub fn with_editor(mut self, editor: EditorCommand) -> Self {
        self.editor = Some(editor);
        self
    }

    /// Supply the host's read-only canonical history. Ctrl-G, H requests a
    /// bounded page; it never feeds replay into the agent's input channel.
    #[must_use]
    pub fn with_history_source(mut self, source: Arc<dyn TranscriptHistory>) -> Self {
        self.history_source = Some(source);
        self
    }

    /// Construct the plugin with a custom mpsc buffer size.
    #[must_use]
    pub fn with_buffer(buffer: usize) -> Self {
        Self {
            buffer,
            ..Self::default()
        }
    }

    /// Supply the candidates for `@` file mentions.
    ///
    /// Which paths exist, which are ignored and how they rank is the host's
    /// policy; the panel only presents what the source returns. Without a
    /// source, `@` is ordinary text and no popup opens.
    #[must_use]
    pub fn with_completion_source(mut self, source: Arc<dyn CompletionSource>) -> Self {
        self.completion_source = Some(source);
        self
    }

    /// Wrap the panel's [`TuiIOProvider`] before it becomes the [`UserIO`]
    /// agents resolve.
    ///
    /// This is how a host executes `/commands` without the agent ever seeing
    /// them: the decorator returns a `UserIO` over its own [`IOProvider`]
    /// that intercepts [`io_type = "command"`](crate::commands::IO_TYPE_COMMAND)
    /// messages on `receive`, runs them, replies through the inner provider's
    /// `send`, and hands everything else through untouched. The decorator
    /// runs once per execution context, so it should be cheap — clone `Arc`s,
    /// do not build registries.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris::plugins::UserIO;
    /// use polaris_tui::panels::ConversationPanelPlugin;
    ///
    /// let plugin = ConversationPanelPlugin::default()
    ///     .with_io_decorator(|provider| UserIO::new(provider));
    /// # let _ = plugin;
    /// ```
    #[must_use]
    pub fn with_io_decorator<F>(mut self, decorate: F) -> Self
    where
        F: Fn(Arc<TuiIOProvider>) -> UserIO + Send + Sync + 'static,
    {
        self.io_decorator = Some(Arc::new(decorate));
        self
    }

    /// Supply the host's non-blocking operation for requesting an interrupt.
    ///
    /// After approval/draft precedence, Ctrl-C calls this once while the host's
    /// `io_type = "status"` is non-empty. A second press within 1500 ms falls
    /// through to the shell. Without a handle, Ctrl-C behaviour is unchanged.
    /// The callback runs on the UI task: signal or enqueue work and return;
    /// never block waiting for the turn to stop.
    ///
    /// The host must send `io_type = "interrupting"` when it raises the signal
    /// and `io_type = "cancelled"` only when the turn has stopped, using the
    /// existing `UserIO` channel. Those messages produce the transcript rows;
    /// invoking this callback alone produces neither. An empty `status` also
    /// returns the panel to idle, without claiming cancellation. Emit statuses
    /// in turn order, and emit no work statuses after the terminal message.
    /// The host owns cancellation, persistence and process cleanup.
    #[must_use]
    pub fn with_interrupt_handle(mut self, handle: impl Fn() + Send + Sync + 'static) -> Self {
        self.interrupt_handle = Some(Arc::new(handle));
        self
    }

    /// Supply the host's non-blocking operation for moving the work behind the
    /// live tool block into the background.
    ///
    /// `Ctrl-T` invokes it once per press, and is useful while a tool block is
    /// live. Without a handle, `Ctrl-T` is unbound and behaviour is unchanged.
    /// The callback runs on the UI task: signal or enqueue work and return;
    /// never block waiting for the host to detach anything.
    ///
    /// The panel decides nothing about the request. It does not know what is
    /// running, which work may be backgrounded, or whether the host will agree
    /// — the callback takes no argument and returns nothing, so it cannot
    /// learn any of that even accidentally. Every visible consequence, receipt
    /// or refusal alike, is a message the host sends on the existing `UserIO`
    /// channel; invoking this callback alone produces none. That includes the
    /// case where nothing is running: the panel still calls the handle, because
    /// only the host can say so, and a silent chord would be the one refusal
    /// the user could not act on.
    ///
    /// `Ctrl-T` rather than the Unix `Ctrl-Z`: this panel is a reusable
    /// component and cannot require its host to hold the terminal in raw mode,
    /// where `Ctrl-Z` would otherwise suspend the whole process. A host that
    /// does own raw mode may offer `Ctrl-Z` as its own composition on top.
    #[must_use]
    pub fn with_background_handle(mut self, handle: impl Fn() + Send + Sync + 'static) -> Self {
        self.background_handle = Some(Arc::new(handle));
        self
    }

    /// Supply the host's attachment resolver, enabling composer chips.
    ///
    /// Without one the composer has no chips, `Ctrl-V` reports that the host
    /// does not do attachments, and nothing the panel sends carries
    /// [`META_ATTACHMENTS`](crate::commands::META_ATTACHMENTS) — a consumer
    /// that does not opt in is unchanged. A host that supplies a source and
    /// also supplies a [`TurnInputHandle`] should override
    /// [`TurnInputHandle::submit_with_attachments`], or queued prompts will
    /// arrive without their attachments.
    #[must_use]
    pub fn with_attachment_source(mut self, source: Arc<dyn AttachmentSource>) -> Self {
        self.attachment_source = Some(source);
        self
    }

    /// Supply a host-owned queue and safe-boundary steering contract.
    ///
    /// With this handle, Enter queues for the next run and Ctrl-Enter steers
    /// the active run. Alt-Up recalls the newest unconsumed message into the
    /// composer; Alt-Backspace removes it. Approval answers continue through
    /// `UserIO` and can never enter this contract.
    #[must_use]
    pub fn with_turn_input_handle(mut self, handle: Arc<dyn TurnInputHandle>) -> Self {
        self.turn_input = Some(handle);
        self
    }

    /// Share a [`LogBuffer`] with the panel so its Ctrl-O timeline shows each
    /// node's captured `tracing` output. Pair this with
    /// [`LogPanelPlugin::with_buffer`](crate::panels::LogPanelPlugin::with_buffer)
    /// over the *same* buffer to feed both panels from one subscriber layer.
    #[must_use]
    pub fn with_log_buffer(mut self, log_buffer: LogBuffer) -> Self {
        self.log_buffer = Some(log_buffer);
        self
    }
}

impl Plugin for ConversationPanelPlugin {
    const ID: &'static str = "polaris::tui::conversation_panel";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(self.buffer);
        if let Some(history) = &self.history {
            panel = panel.with_history(history.clone());
        }
        if let Some(editor) = &self.editor {
            panel = panel.with_editor(editor.clone());
        }
        panel.interrupt_handle = self.interrupt_handle.clone();
        panel.background_handle = self.background_handle.clone();
        panel.turn_input = self.turn_input.clone();
        if let Some(source) = &self.attachment_source {
            panel.set_attachment_source(source.clone());
        }
        if let Some(log_buffer) = &self.log_buffer {
            panel.set_log_buffer(log_buffer.clone());
        }
        if let Some(source) = &self.completion_source {
            panel.set_completion_source(source.clone());
        }
        if let Some(source) = &self.history_source {
            panel = panel.with_history_source(source.clone());
        }
        let provider = Arc::new(provider);
        let factory_provider = provider.clone();
        match self.io_decorator.clone() {
            Some(decorate) => {
                server.register_local(move || decorate(factory_provider.clone()));
            }
            None => server.register_local(move || UserIO::new(factory_provider.clone())),
        }
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin must be added before ConversationPanelPlugin")
            .add_panel(panel);
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use std::sync::atomic::AtomicUsize;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn modified(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Whether the panel has handed anything to the agent channel. Reading
    /// capacity is deterministic, unlike racing a `recv` against a timeout.
    fn nothing_submitted(panel: &ConversationPanel) -> bool {
        panel.in_tx.capacity() == panel.in_tx.max_capacity()
    }

    /// Draw the panel into a `width x height` test terminal and return the
    /// rendered rows plus the caret position the panel asked for.
    fn draw(
        panel: &mut ConversationPanel,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Option<(u16, u16)>) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                panel.render(frame, area);
            })
            .unwrap();
        let caret = terminal.get_cursor_position().ok().map(|p| (p.x, p.y));
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        (rows, caret)
    }

    fn type_str(panel: &mut ConversationPanel, s: &str) {
        for c in s.chars() {
            panel.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn fmt_tokens_collapses_thousands_with_one_decimal() {
        // Small counts stay plain integers.
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(850), "850");
        assert_eq!(fmt_tokens(999), "999");
        // 1,000 and up collapse to thousands, rounded to one decimal.
        assert_eq!(fmt_tokens(1000), "1.0k");
        assert_eq!(fmt_tokens(5120), "5.1k");
        assert_eq!(fmt_tokens(133854), "133.9k");
    }

    #[test]
    fn split_lines_breaks_on_newlines_and_strips_cr() {
        assert_eq!(split_lines("a\nb\nc").collect::<Vec<_>>(), ["a", "b", "c"]);
        // Windows line endings: the trailing `\r` is stripped per segment.
        assert_eq!(split_lines("a\r\nb").collect::<Vec<_>>(), ["a", "b"]);
        // Empty input still yields one (empty) segment so it renders a blank row.
        assert_eq!(split_lines("").collect::<Vec<_>>(), [""]);
        assert_eq!(split_lines("solo").collect::<Vec<_>>(), ["solo"]);
    }

    #[tokio::test]
    async fn multiline_message_renders_one_line_per_physical_line() {
        // A `\n` inside a message body must break into rows rather than render
        // as a control glyph inside a single `Span`.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.transcript.push_back(IOMessage::from_agent(
            "code",
            IOContent::Text("one\ntwo".into()),
        ));
        let rendered: Vec<String> = panel
            .message_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        assert_eq!(rendered.len(), 2, "two physical lines → two rows");
        assert!(
            rendered[0].contains("code ❯ "),
            "first row carries the label"
        );
        assert!(rendered[0].ends_with("one"));
        // Continuation indents to the label width rather than repeating it.
        assert!(!rendered[1].contains('❯'));
        assert!(rendered[1].ends_with("two"));
    }

    #[tokio::test]
    async fn structured_and_binary_bodies_render_their_summaries() {
        // The non-`Text` content arms each have a distinct rendering: structured
        // values stringify, binary collapses to a `<binary mime (n bytes)>` tag.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.transcript.push_back(IOMessage::from_agent(
            "code",
            IOContent::Structured(serde_json::json!({"k": "v"})),
        ));
        panel.transcript.push_back(IOMessage::from_agent(
            "code",
            IOContent::Binary {
                mime_type: "image/png".into(),
                data: vec![0u8; 12],
            },
        ));
        let rendered: Vec<String> = panel
            .message_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("\"k\"") && l.contains("\"v\"")),
            "structured body stringifies its JSON: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("<binary image/png (12 bytes)>")),
            "binary body collapses to a mime + size tag: {rendered:?}"
        );
    }

    #[tokio::test]
    async fn ascii_typing_and_backspace() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "abc");
        assert_eq!(panel.composer.text(), "abc");
        assert_eq!(panel.composer.cursor(), 3);
        panel.handle_key(key(KeyCode::Backspace));
        assert_eq!(panel.composer.text(), "ab");
        assert_eq!(panel.composer.cursor(), 2);
    }

    #[tokio::test]
    async fn multibyte_input_does_not_panic_and_edits_correctly() {
        // The caret is a byte offset held on a grapheme boundary (a char count
        // would panic on a non-char boundary, since String::insert/remove are
        // byte-indexed), and the *column* is measured separately.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "café"); // 'é' is two bytes
        assert_eq!(panel.composer.text(), "café");
        assert_eq!(panel.composer.cursor(), 5, "four glyphs, five bytes");
        assert_eq!(
            panel.composer.caret(80),
            (0, 4),
            "but only four columns wide"
        );

        // Move left over 'é', insert in the middle, then backspace it out.
        panel.handle_key(key(KeyCode::Left));
        assert_eq!(panel.composer.cursor(), 3);
        panel.handle_key(key(KeyCode::Char('X')));
        assert_eq!(panel.composer.text(), "cafXé");
        assert_eq!(panel.composer.cursor(), 4);
        panel.handle_key(key(KeyCode::Backspace));
        assert_eq!(panel.composer.text(), "café");
        assert_eq!(panel.composer.cursor(), 3);

        // Home then backspace at column 0 is a no-op (no underflow/panic).
        panel.handle_key(key(KeyCode::Home));
        assert_eq!(panel.composer.cursor(), 0);
        panel.handle_key(key(KeyCode::Backspace));
        assert_eq!(panel.composer.text(), "café");
        assert_eq!(panel.composer.cursor(), 0);
    }

    #[tokio::test]
    async fn cursor_navigation_clamps_to_grapheme_bounds() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "héllo"); // 5 glyphs, 6 bytes
        panel.handle_key(key(KeyCode::End));
        assert_eq!(panel.composer.cursor(), 6);
        assert_eq!(panel.composer.caret(80), (0, 5), "five columns");
        // Right past the end is clamped.
        panel.handle_key(key(KeyCode::Right));
        assert_eq!(panel.composer.cursor(), 6);
        panel.handle_key(key(KeyCode::Home));
        assert_eq!(panel.composer.cursor(), 0);
        // Left past the start saturates.
        panel.handle_key(key(KeyCode::Left));
        assert_eq!(panel.composer.cursor(), 0);
    }

    #[tokio::test]
    async fn ctrl_u_clears_input() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "drop me");
        panel.handle_key(ctrl(KeyCode::Char('u')));
        assert_eq!(panel.composer.text(), "");
        assert_eq!(panel.composer.cursor(), 0);
    }

    #[tokio::test]
    async fn alt_arrows_jump_by_word() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "the quick brown fox");
        assert_eq!(panel.composer.cursor(), 19);
        // Alt+Left walks back over each word.
        panel.handle_key(alt(KeyCode::Left));
        assert_eq!(panel.composer.cursor(), 16, "start of 'fox'");
        panel.handle_key(alt(KeyCode::Left));
        assert_eq!(panel.composer.cursor(), 10, "start of 'brown'");
        // The readline `Alt-b` alias iTerm sends behaves identically.
        panel.handle_key(alt(KeyCode::Char('b')));
        assert_eq!(panel.composer.cursor(), 4, "start of 'quick'");
        // Alt+Right walks forward to the end of each word.
        panel.handle_key(alt(KeyCode::Right));
        assert_eq!(panel.composer.cursor(), 9, "end of 'quick'");
        panel.handle_key(alt(KeyCode::Char('f')));
        assert_eq!(panel.composer.cursor(), 15, "end of 'brown'");
    }

    #[tokio::test]
    async fn word_jumps_clamp_at_the_ends() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "one two");
        panel.handle_key(alt(KeyCode::Right));
        assert_eq!(panel.composer.cursor(), 7, "already at end stays at end");
        for _ in 0..3 {
            panel.handle_key(alt(KeyCode::Left));
        }
        assert_eq!(panel.composer.cursor(), 0, "past the start saturates to 0");
    }

    #[tokio::test]
    async fn alt_backspace_and_ctrl_w_delete_word_back() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "alpha beta gamma");
        panel.handle_key(alt(KeyCode::Backspace));
        assert_eq!(panel.composer.text(), "alpha beta ");
        assert_eq!(panel.composer.cursor(), 11);
        panel.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(panel.composer.text(), "alpha ");
        assert_eq!(panel.composer.cursor(), 6);
    }

    #[tokio::test]
    async fn alt_delete_removes_word_forward() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "alpha beta gamma");
        panel.handle_key(key(KeyCode::Home));
        panel.handle_key(alt(KeyCode::Delete));
        assert_eq!(panel.composer.text(), " beta gamma");
        assert_eq!(panel.composer.cursor(), 0);
    }

    #[tokio::test]
    async fn ctrl_a_e_and_k_act_on_line_ends() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "hello world");
        panel.handle_key(ctrl(KeyCode::Char('a')));
        assert_eq!(panel.composer.cursor(), 0, "Ctrl-A to start");
        panel.handle_key(ctrl(KeyCode::Char('e')));
        assert_eq!(panel.composer.cursor(), 11, "Ctrl-E to end");
        // Ctrl-K kills from the cursor to the end of the line.
        panel.handle_key(ctrl(KeyCode::Char('a')));
        panel.handle_key(key(KeyCode::Right));
        panel.handle_key(key(KeyCode::Right));
        panel.handle_key(key(KeyCode::Right));
        panel.handle_key(key(KeyCode::Right));
        panel.handle_key(key(KeyCode::Right));
        panel.handle_key(ctrl(KeyCode::Char('k')));
        assert_eq!(panel.composer.text(), "hello");
        assert_eq!(panel.composer.cursor(), 5);
    }

    #[tokio::test]
    async fn delete_and_ctrl_d_remove_forward() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "héllo"); // 'é' is two bytes
        panel.handle_key(key(KeyCode::Home));
        panel.handle_key(key(KeyCode::Right));
        // Forward-delete the multibyte 'é' without panicking.
        panel.handle_key(key(KeyCode::Delete));
        assert_eq!(panel.composer.text(), "hllo");
        assert_eq!(panel.composer.cursor(), 1);
        panel.handle_key(ctrl(KeyCode::Char('d')));
        assert_eq!(panel.composer.text(), "hlo");
        assert_eq!(panel.composer.cursor(), 1);
        // Delete at end of line is a no-op.
        panel.handle_key(key(KeyCode::End));
        panel.handle_key(key(KeyCode::Delete));
        assert_eq!(panel.composer.text(), "hlo");
    }

    #[tokio::test]
    async fn submit_echoes_locally_and_resets_cursor() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "hi");
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.composer.text(), "");
        assert_eq!(panel.composer.cursor(), 0);
        // Local echo lands in the transcript immediately.
        assert_eq!(panel.transcript.len(), 1);
        let echoed = &panel.transcript[0];
        assert!(matches!(echoed.source, IOSource::User));
        match &echoed.content {
            IOContent::Text(t) => assert_eq!(t, "hi"),
            other => panic!("expected text echo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_send_surfaces_through_the_drain_task_into_the_transcript() {
        use polaris::plugins::IOProvider;
        // The real outbound path: `send()` enqueues on `out_tx`, the spawned
        // drain task moves it into the shared inbox and notifies, and the panel
        // picks it up on its next `tick()`. None of that is exercised by poking
        // `inbox` directly, so drive it end to end here.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        provider
            .send(IOMessage::from_agent(
                "code",
                IOContent::Text("hello from agent".into()),
            ))
            .await
            .unwrap();

        // The drain task runs asynchronously; yield until the message lands in
        // the inbox (bounded so a regression fails fast rather than hanging).
        for _ in 0..100 {
            if !panel.inbox.lock().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        panel.tick();
        assert_eq!(
            panel.transcript.len(),
            1,
            "drained message reaches transcript"
        );
        match &panel.transcript[0].content {
            IOContent::Text(t) => assert_eq!(t, "hello from agent"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submitted_input_is_delivered_to_receive() {
        use polaris::plugins::IOProvider;
        // The real inbound path: typing + Enter routes through `submit_input`'s
        // `in_tx.try_send`, which the agent's `receive()` pulls off `in_rx`.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "ping");
        panel.handle_key(key(KeyCode::Enter));

        let msg = provider.receive().await.unwrap();
        assert!(matches!(msg.source, IOSource::User));
        match &msg.content {
            IOContent::Text(t) => assert_eq!(t, "ping"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn receive_reports_closed_once_the_panel_sender_drops() {
        use polaris::plugins::IOProvider;
        // The panel owns the sole `in_tx`; dropping it must surface to a parked
        // `receive()` as `IOError::Closed` (the shutdown signal), not hang.
        let (provider, panel) = TuiIOProvider::new_with_panel(8);
        drop(panel);
        let err = provider.receive().await.unwrap_err();
        assert!(matches!(err, IOError::Closed), "got {err:?}");
    }

    // ── Multi-line composer ─────────────────────────────────────────────────

    #[tokio::test]
    async fn a_bracketed_paste_becomes_one_draft_and_never_submits() {
        // The defect this closes: without bracketing, a pasted block arrives as
        // key events and each embedded newline submits a separate prompt.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.handle_event(&TuiEvent::Paste("first\nsecond\nthird".into()));
        assert_eq!(panel.composer.text(), "first\nsecond\nthird");
        assert!(
            nothing_submitted(&panel),
            "a paste is a draft, not a submission"
        );
    }

    #[tokio::test]
    async fn a_pasted_slash_command_stays_ordinary_text() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.handle_event(&TuiEvent::Paste("/model opus\n/cost\n".into()));
        assert_eq!(panel.composer.text(), "/model opus\n/cost\n");
        assert!(nothing_submitted(&panel), "pasted commands do not dispatch");
    }

    #[tokio::test]
    async fn a_pasted_crlf_block_normalises_to_one_line_per_row() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.handle_event(&TuiEvent::Paste("alpha\r\nbeta\r\n".into()));
        assert_eq!(panel.composer.text(), "alpha\nbeta\n");
        assert!(!panel.composer.text().contains('\r'));
    }

    #[tokio::test]
    async fn modifier_enter_inserts_a_newline_and_plain_enter_submits() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "one");
        // Shift+Enter is the documented chord; it only reaches us when the
        // keyboard enhancement protocol is negotiated.
        panel.handle_key(modified(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut panel, "two");
        // Alt and Super are accepted as the same action — terminals differ in
        // which of the three they report.
        panel.handle_key(modified(KeyCode::Enter, KeyModifiers::ALT));
        type_str(&mut panel, "three");
        panel.handle_key(modified(KeyCode::Enter, KeyModifiers::SUPER));
        type_str(&mut panel, "four");
        assert_eq!(panel.composer.text(), "one\ntwo\nthree\nfour");
        assert!(nothing_submitted(&panel), "no modifier+Enter submits");

        panel.handle_key(key(KeyCode::Enter));
        let msg = {
            use polaris::plugins::IOProvider;
            provider.receive().await.unwrap()
        };
        match &msg.content {
            IOContent::Text(t) => assert_eq!(t, "one\ntwo\nthree\nfour"),
            other => panic!("expected the whole draft as one message, got {other:?}"),
        }
        assert!(panel.composer.is_empty(), "an accepted draft is cleared");
    }

    #[tokio::test]
    async fn a_trailing_backslash_continues_instead_of_submitting() {
        // The fallback for terminals that cannot report Shift+Enter at all.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "first\\");
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.composer.text(), "first\n");
        assert!(nothing_submitted(&panel));

        type_str(&mut panel, "second");
        panel.handle_key(key(KeyCode::Enter));
        assert!(!nothing_submitted(&panel), "a bare Enter still submits");
    }

    #[tokio::test]
    async fn an_escaped_backslash_submits_rather_than_continuing() {
        // A pasted Windows path ends in a doubled backslash; treating that as a
        // continuation would strand the draft with no way to send it.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "C:\\\\dir\\\\");
        panel.handle_key(key(KeyCode::Enter));
        let msg = {
            use polaris::plugins::IOProvider;
            provider.receive().await.unwrap()
        };
        match &msg.content {
            IOContent::Text(t) => assert_eq!(t, "C:\\\\dir\\\\"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn arrow_keys_move_between_rows_before_falling_through() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.handle_event(&TuiEvent::Paste("top\nbottom".into()));
        // Caret is on the last row; Up has somewhere to go, so the panel claims it.
        assert!(matches!(
            panel.handle_key(key(KeyCode::Up)),
            EventOutcome::Consumed
        ));
        assert_eq!(panel.composer.caret(80).0, 0);
        // At the first row it declines, leaving the key free for the shell —
        // and for prompt-history walking when a history store is configured.
        assert!(matches!(
            panel.handle_key(key(KeyCode::Up)),
            EventOutcome::Continue
        ));
    }

    #[tokio::test]
    async fn a_single_line_draft_leaves_the_arrows_alone() {
        // With nothing to move between, Up/Down must still fall through to
        // the shell.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "one line");
        assert!(matches!(
            panel.handle_key(key(KeyCode::Up)),
            EventOutcome::Continue
        ));
        assert!(matches!(
            panel.handle_key(key(KeyCode::Down)),
            EventOutcome::Continue
        ));
    }

    #[tokio::test]
    async fn ctrl_c_clears_a_multi_line_draft_whole() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.handle_event(&TuiEvent::Paste("a\nb\nc".into()));
        assert!(matches!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Consumed
        ));
        assert!(panel.composer.is_empty());
    }

    // ── Composer rendering ──────────────────────────────────────────────────

    #[tokio::test]
    async fn the_composer_grows_with_the_draft_then_stops() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let (rows, _) = draw(&mut panel, 30, 20);
        let empty_border = rows.iter().filter(|r| r.contains('│')).count();
        assert_eq!(empty_border, 1, "an empty draft occupies a single row");

        panel.handle_event(&TuiEvent::Paste("1\n2\n3\n4".into()));
        let (rows, _) = draw(&mut panel, 30, 20);
        assert_eq!(
            rows.iter().filter(|r| r.contains('│')).count(),
            4,
            "the frame grows to fit the draft"
        );

        // Well past the bound: growth stops so the transcript survives.
        let long = (0..40)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        panel.handle_event(&TuiEvent::Paste(long));
        let (rows, _) = draw(&mut panel, 30, 20);
        let grown = rows.iter().filter(|r| r.contains('│')).count();
        assert_eq!(grown, COMPOSER_MAX_ROWS, "growth is bounded");
    }

    #[tokio::test]
    async fn the_composer_never_squeezes_out_the_transcript() {
        // A tall draft in a short terminal must still leave the transcript a row.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let long = (0..40)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        panel.handle_event(&TuiEvent::Paste(long));
        let (rows, _) = draw(&mut panel, 30, 6);
        let composer_rows = rows.iter().filter(|r| r.contains('│')).count();
        assert!(
            composer_rows <= 3,
            "6 rows total, 2 borders, 1 transcript: got {composer_rows}"
        );
    }

    #[tokio::test]
    async fn the_caret_sits_on_the_glyph_for_wide_characters() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "世界");
        let (_, caret) = draw(&mut panel, 30, 10);
        let (x, _) = caret.expect("a focused panel places the caret");
        // Two glyphs, four columns. Counting chars would have put it at 2 —
        // two cells inside the text the user can see.
        assert_eq!(x, 1 + 4, "one column of border, then four of glyphs");
    }

    #[tokio::test]
    async fn the_caret_follows_a_wrapped_long_line() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // 30 columns wide, 2 of border: a 40-char line takes two rows.
        type_str(&mut panel, &"x".repeat(40));
        let (_, caret) = draw(&mut panel, 30, 10);
        let (x, y) = caret.expect("caret placed");
        assert_eq!(x, 1 + (40 - 28), "12 glyphs onto the second row");
        let (rows, _) = draw(&mut panel, 30, 10);
        let first_composer_row = rows.iter().position(|r| r.contains('│')).unwrap();
        assert_eq!(
            usize::from(y),
            first_composer_row + 1,
            "caret is on the wrapped second row"
        );
    }

    #[tokio::test]
    async fn a_long_draft_scrolls_to_keep_the_caret_visible() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let long = (0..40)
            .map(|i| format!("row{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        panel.handle_event(&TuiEvent::Paste(long));
        let (rows, caret) = draw(&mut panel, 30, 20);
        // The caret is at the end of the draft, so the view is scrolled to the
        // bottom and the last line is on screen while the first is not.
        assert!(
            rows.iter().any(|r| r.contains("row39")),
            "the caret's row is visible"
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("row0 ") || r.contains("│row0")),
            "the top of the draft has scrolled away"
        );
        assert!(caret.is_some());
    }

    #[tokio::test]
    async fn a_narrow_terminal_still_renders_without_panicking() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "世界 hello");
        for width in [1_u16, 2, 3, 8, 40] {
            let (_, caret) = draw(&mut panel, width, 8);
            if let Some((x, _)) = caret {
                assert!(x < width, "caret stays inside a {width}-column terminal");
            }
        }
    }

    #[tokio::test]
    async fn shrinking_the_terminal_reflows_without_stranding_the_view() {
        // The hazard: a scroll offset computed at one width outlives a resize
        // and points past the end of the reflowed draft.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let long = (0..30)
            .map(|i| format!("row{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        panel.handle_event(&TuiEvent::Paste(long));
        draw(&mut panel, 80, 24);
        let wide_scroll = panel.composer_scroll;
        assert!(wide_scroll > 0, "a 30-row draft scrolls at 24 rows tall");

        panel.handle_event(&TuiEvent::Resize { cols: 12, rows: 8 });
        let (rows, caret) = draw(&mut panel, 12, 8);
        let composer_rows = rows.iter().filter(|r| r.contains('│')).count();
        assert!(composer_rows >= 1, "the composer keeps at least one row");
        assert!(
            panel.composer_scroll + composer_rows <= panel.composer.rows(10).len(),
            "scroll stays within the reflowed draft"
        );
        if let Some((x, y)) = caret {
            assert!(x < 12 && y < 8, "caret stays on screen after the resize");
        }

        // And back out again.
        let (_, caret) = draw(&mut panel, 80, 24);
        assert!(caret.is_some(), "still renders after growing back");
    }

    // ── Ctrl-C claiming ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn ctrl_c_is_declined_when_there_is_nothing_to_act_on() {
        // Idle with an empty draft: the shell owns the press and quits.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Continue
        );
    }

    #[tokio::test]
    async fn ctrl_c_clears_a_draft_before_it_quits() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "half a thought");
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Consumed
        );
        assert!(panel.composer.text().is_empty(), "the draft is cleared");
        assert_eq!(panel.composer.cursor(), 0);
    }

    #[tokio::test]
    async fn ctrl_c_denies_a_pending_approval_without_quitting() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let user_io = UserIO::new(Arc::new(provider));
        let mutations = AtomicUsize::new(0);

        let agent = guarded_tool(&user_io, "run `rm -rf build/`?", &mutations);
        let user = async {
            assert!(tick_until_confirm(&mut panel, 32).await);
            assert_eq!(
                panel.handle_key(ctrl(KeyCode::Char('c'))),
                EventOutcome::Consumed,
                "an open dialog claims Ctrl-C rather than letting the shell quit"
            );
        };
        let (confirmed, ()) = tokio::join!(agent, user);

        assert!(!confirmed.unwrap(), "and it denies");
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        assert!(panel.pending_confirm.is_none());
    }

    #[tokio::test]
    async fn a_second_ctrl_c_falls_through_so_the_shell_can_quit() {
        // The escape hatch: whatever the panel is doing, two presses leave.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "first");
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Consumed
        );
        type_str(&mut panel, "second");
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Continue,
            "a second press inside the window is declined even with a draft to clear"
        );
    }

    #[tokio::test]
    async fn ctrl_c_claims_again_once_the_escalation_window_lapses() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "first");
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Consumed
        );
        // Age the recorded press past the window rather than sleeping.
        panel.last_interrupt =
            Some(Instant::now() - INTERRUPT_ESCALATION - Duration::from_millis(1));
        type_str(&mut panel, "second");
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Consumed,
            "an unrelated later press is a first press again"
        );
        assert!(panel.composer.text().is_empty());
    }

    #[tokio::test]
    async fn running_without_a_handle_keeps_the_existing_ctrl_c_behaviour() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().push_back(status_msg("Working"));
        panel.tick();
        assert_eq!(panel.interrupt(), EventOutcome::Continue);
        assert!(panel.transcript.is_empty());
        type_str(&mut panel, "draft");
        assert_eq!(panel.interrupt(), EventOutcome::Consumed);
        assert!(panel.composer.is_empty());
        assert_eq!(panel.status.as_deref(), Some("Working"));
    }

    // ── Ctrl-T: backgrounding the live work ────────────────────────────────

    #[tokio::test]
    async fn ctrl_t_is_unbound_without_a_handle() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(tool("t0", None, "run_command", "start", "cargo test"));
        panel.tick();
        let before = draw(&mut panel, 80, 20).0;

        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('t'))),
            EventOutcome::Continue,
            "a host that installed no handle must leave Ctrl-T to the shell"
        );
        assert_eq!(
            draw(&mut panel, 80, 20).0,
            before,
            "an unbound chord changes nothing on screen"
        );
    }

    #[tokio::test]
    async fn ctrl_t_asks_the_host_once_per_press_and_renders_nothing_itself() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        panel.background_handle = Some(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        panel
            .inbox
            .lock()
            .push_back(tool("t0", None, "run_command", "start", "cargo test"));
        panel.tick();
        let before = draw(&mut panel, 80, 20).0;

        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('t'))),
            EventOutcome::Consumed
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            draw(&mut panel, 80, 20).0,
            before,
            "the callback alone produces no receipt: only a host message does"
        );

        // Unlike Ctrl-C there is no escalation latch — a second request is a
        // second question for the host, which may now answer differently.
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('t'))),
            EventOutcome::Consumed
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ctrl_t_still_asks_the_host_when_the_panel_sees_nothing_running() {
        // The panel's transcript is a cache of what it was last told, not the
        // host's process table. If it declined here, "nothing is running to
        // background" — a refusal only the host can phrase — would become a
        // key that silently does nothing.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        panel.background_handle = Some(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(panel.transcript.is_empty());

        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('t'))),
            EventOutcome::Consumed
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ctrl_t_does_not_disturb_the_draft_or_the_composer() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.background_handle = Some(Arc::new(|| {}));
        type_str(&mut panel, "half a prompt");
        let cursor = panel.composer.cursor();

        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('t'))),
            EventOutcome::Consumed
        );
        assert_eq!(panel.composer.text(), "half a prompt");
        assert_eq!(panel.composer.cursor(), cursor);
    }

    #[tokio::test]
    async fn interrupt_requests_once_but_renders_only_host_acknowledgements() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        panel.interrupt_handle = Some(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        // Installing a handle does not make an idle panel busy.
        assert_eq!(panel.interrupt(), EventOutcome::Continue);
        panel.inbox.lock().push_back(status_msg("Working"));
        panel.tick();
        let draft = panel.composer.text().to_string();
        assert_eq!(panel.interrupt(), EventOutcome::Consumed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(panel.composer.text(), draft);
        assert!(
            panel.transcript.is_empty(),
            "a callback is not host acknowledgement"
        );
        assert_eq!(panel.status.as_deref(), Some("Working"));
        assert_eq!(panel.interrupt(), EventOutcome::Continue);

        // Time cannot manufacture an acknowledgement or resend the request.
        panel.last_interrupt = Some(Instant::now() - INTERRUPT_ESCALATION);
        panel.tick();
        assert_eq!(panel.interrupt(), EventOutcome::Consumed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(panel.transcript.is_empty());

        panel.inbox.lock().push_back(
            IOMessage::system_text("Signal raised").with_metadata(IO_TYPE, "interrupting"),
        );
        panel.tick();
        let (rows, _) = draw(&mut panel, 80, 20);
        assert!(
            rows.iter()
                .any(|row| row.contains("Interrupting ❯ Signal raised"))
        );
        assert!(!rows.iter().any(|row| row.contains("Cancelled")));
        assert!(!rows.iter().any(|row| row.contains("hidden")));

        // Progress and token traffic during shutdown cannot erase the state.
        panel.inbox.lock().push_back(status_msg("Finishing node"));
        panel.tick();
        assert_eq!(panel.status.as_deref(), Some("Interrupting"));
        type_str(&mut panel, "next turn draft");
        let cursor = panel.composer.cursor();
        panel
            .inbox
            .lock()
            .push_back(IOMessage::system_text("Stopped").with_metadata(IO_TYPE, "cancelled"));
        panel.tick();
        let (rows, _) = draw(&mut panel, 80, 20);
        assert!(rows.iter().any(|row| row.contains("Cancelled ❯ Stopped")));
        assert_eq!(panel.status, None);
        assert_eq!(panel.composer.text(), "next turn draft");
        assert_eq!(panel.composer.cursor(), cursor);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        panel.composer.clear();
        panel.last_interrupt = Some(Instant::now() - INTERRUPT_ESCALATION);
        panel.inbox.lock().push_back(status_msg("Next turn"));
        panel.tick();
        assert_eq!(panel.interrupt(), EventOutcome::Consumed);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn interrupt_handle_keeps_approval_and_draft_precedence() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        panel.interrupt_handle = Some(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        panel.inbox.lock().extend([
            status_msg("Working"),
            IOMessage::system_text("Run command?").with_metadata(IO_TYPE, TYPE_CONFIRM),
        ]);
        panel.tick();
        type_str(&mut panel, "keep this draft");
        let cursor = panel.composer.cursor();
        assert_eq!(panel.interrupt(), EventOutcome::Consumed);
        assert_eq!(panel.composer.text(), "keep this draft");
        assert_eq!(panel.composer.cursor(), cursor);
        assert_eq!(
            provider.receive().await.unwrap().metadata[CONFIRMED],
            CONFIRMED_FALSE
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(panel.pending_confirm.is_none());
        // The second chord always declines, even though a draft remains.
        assert_eq!(panel.interrupt(), EventOutcome::Continue);
        panel.last_interrupt = Some(Instant::now() - INTERRUPT_ESCALATION);
        assert_eq!(panel.interrupt(), EventOutcome::Consumed);
        assert!(panel.composer.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        panel.last_interrupt = Some(Instant::now() - INTERRUPT_ESCALATION);
        assert_eq!(panel.interrupt(), EventOutcome::Consumed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn external_interrupt_statuses_render_without_a_local_handle() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(IOMessage::system_text("").with_metadata(IO_TYPE, "interrupting"));
        panel.tick();
        assert_eq!(panel.status.as_deref(), Some("Interrupting"));
        panel.inbox.lock().push_back(status_msg(""));
        panel.tick();
        assert_eq!(panel.status, None);
        let (rows, _) = draw(&mut panel, 80, 20);
        assert!(rows.iter().any(|row| row.contains("Interrupting")));
        assert!(
            !rows.iter().any(|row| row.contains("Cancelled")),
            "idle is not cancellation"
        );
        panel
            .inbox
            .lock()
            .push_back(IOMessage::system_text("").with_metadata(IO_TYPE, "cancelled"));
        panel.tick();
        let (rows, _) = draw(&mut panel, 80, 20);
        assert!(rows.iter().any(|row| row.contains("Cancelled")));
    }

    // ── Input delivery ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_full_channel_keeps_the_draft_and_echoes_nothing() {
        // submit_input checks the try_send result before it echoes: a refused
        // prompt stays in the input and does not appear in the transcript as
        // though the agent had it.
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(1);
        type_str(&mut panel, "first");
        panel.handle_key(key(KeyCode::Enter));
        assert!(
            panel.composer.text().is_empty(),
            "an accepted prompt clears the draft"
        );

        // The one slot is taken, so this one cannot be queued.
        type_str(&mut panel, "second");
        panel.handle_key(key(KeyCode::Left));
        let cursor = panel.composer.cursor();
        panel.handle_key(key(KeyCode::Enter));

        assert_eq!(
            panel.composer.text(),
            "second",
            "the draft survives a refusal"
        );
        assert_eq!(panel.composer.cursor(), cursor, "and so does the cursor");
        assert_eq!(
            panel.delivery_failure,
            Some(DeliveryFailure::Backlogged),
            "the refusal is reported, not swallowed"
        );
        assert_eq!(
            panel.transcript.len(),
            1,
            "no phantom entry for the refused prompt: {:?}",
            panel.transcript
        );
    }

    #[tokio::test]
    async fn a_closed_channel_reports_unavailable_rather_than_retryable() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(4);
        drop(provider);
        type_str(&mut panel, "anyone there?");
        panel.handle_key(key(KeyCode::Enter));

        assert_eq!(panel.composer.text(), "anyone there?", "the draft is kept");
        assert_eq!(
            panel.delivery_failure,
            Some(DeliveryFailure::Closed),
            "a departed agent is not a retry-later condition"
        );
        assert!(panel.transcript.is_empty());
    }

    #[tokio::test]
    async fn retry_after_draining_delivers_once_in_order() {
        use polaris::plugins::IOProvider;
        // The full flow the failure mode broke: saturate one slot, get refused,
        // let the agent drain, retry. Exactly two messages arrive, in order,
        // and the retried one is not duplicated.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(1);
        type_str(&mut panel, "first");
        panel.handle_key(key(KeyCode::Enter));
        type_str(&mut panel, "second");
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.delivery_failure, Some(DeliveryFailure::Backlogged));

        let first = provider.receive().await.unwrap();
        match &first.content {
            IOContent::Text(text) => assert_eq!(text, "first"),
            other => panic!("expected text, got {other:?}"),
        }

        // The slot is free again; Enter retries the preserved draft.
        panel.handle_key(key(KeyCode::Enter));
        assert!(
            panel.composer.text().is_empty(),
            "the retry clears the draft"
        );
        assert_eq!(panel.delivery_failure, None, "and clears the failure state");

        let second = provider.receive().await.unwrap();
        match &second.content {
            IOContent::Text(text) => assert_eq!(text, "second"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(
            panel.transcript.len(),
            2,
            "one transcript entry per delivered prompt, no duplicate: {:?}",
            panel.transcript
        );
        // Nothing further is in flight: the refused attempt queued nothing of
        // its own. Checked against the channel rather than a timeout so the
        // test stays deterministic and instant.
        assert_eq!(
            panel.in_tx.capacity(),
            panel.in_tx.max_capacity(),
            "the refused attempt left nothing behind in the channel"
        );
    }

    #[tokio::test]
    async fn a_refused_prompt_preserves_multibyte_text_and_cursor() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(1);
        type_str(&mut panel, "filler");
        panel.handle_key(key(KeyCode::Enter));

        type_str(&mut panel, "café ☕");
        panel.handle_key(key(KeyCode::Home));
        panel.handle_key(key(KeyCode::Right));
        panel.handle_key(key(KeyCode::Right));
        let cursor = panel.composer.cursor();
        panel.handle_key(key(KeyCode::Enter));

        assert_eq!(
            panel.composer.text(),
            "café ☕",
            "UTF-8 survives byte-for-byte"
        );
        assert_eq!(
            panel.composer.cursor(),
            cursor,
            "the cursor stays where it was"
        );
        // Editing still works on the restored draft — the char/byte mapping is intact.
        panel.handle_key(key(KeyCode::Backspace));
        assert_eq!(
            panel.composer.text(),
            "cfé ☕",
            "backspace at index 2 removes the 'a'"
        );
    }

    #[tokio::test]
    async fn a_queued_prompt_is_labelled_until_the_agent_takes_it() {
        use polaris::plugins::IOProvider;
        // Accepted by the channel is not the same as read by the agent.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(4);
        type_str(&mut panel, "ping");
        panel.handle_key(key(KeyCode::Enter));

        let queued: Vec<String> = panel.message_lines().iter().map(line_text).collect();
        assert!(
            queued[0].contains("· queued"),
            "a prompt waiting in the channel says so: {queued:?}"
        );

        provider.receive().await.unwrap();
        let taken: Vec<String> = panel.message_lines().iter().map(line_text).collect();
        assert!(
            !taken[0].contains("· queued"),
            "the badge clears once the agent takes it: {taken:?}"
        );
        assert!(taken[0].ends_with("ping"), "and the text is untouched");
    }

    #[tokio::test]
    async fn a_confirmation_answer_does_not_retire_a_deferred_prompt() {
        use polaris::plugins::IOProvider;
        // The answer jumps ahead of a prompt the provider is holding back, so
        // counting it as consumption would clear that prompt's queued badge
        // while it is in fact still waiting.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "later");
        panel.handle_key(key(KeyCode::Enter));
        panel.inbox.lock().push_back(confirm_msg("run it?"));
        panel.tick();
        type_str(&mut panel, "y");
        panel.handle_key(key(KeyCode::Enter));

        // Drive the provider the way `confirm` does: latch, then receive.
        provider
            .send(IOMessage::system_text("run it?").with_metadata(IO_TYPE, IO_TYPE_CONFIRM))
            .await
            .unwrap();
        let answer = provider.receive().await.unwrap();
        assert_eq!(
            answer.metadata.get(CONFIRMED).map(String::as_str),
            Some("true")
        );

        let lines: Vec<String> = panel.message_lines().iter().map(line_text).collect();
        assert!(
            lines[0].contains("· queued"),
            "the deferred prompt is still queued: {lines:?}"
        );

        // Now the agent actually takes it, and only then does the badge clear.
        provider.receive().await.unwrap();
        let lines: Vec<String> = panel.message_lines().iter().map(line_text).collect();
        assert!(!lines[0].contains("· queued"), "{lines:?}");
    }

    #[tokio::test]
    async fn locally_submitted_messages_respect_the_transcript_bound() {
        // The local echo respects the same transcript cap as drained inbound
        // messages.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(4);
        for i in 0..(TRANSCRIPT_CAP + 16) {
            type_str(&mut panel, &format!("m{i}"));
            panel.handle_key(key(KeyCode::Enter));
            // Keep the 4-slot channel drained so every prompt is accepted.
            provider.receive().await.unwrap();
        }
        assert_eq!(
            panel.transcript.len(),
            TRANSCRIPT_CAP,
            "locally submitted messages are bounded like inbound ones"
        );
        match &panel.transcript.back().unwrap().content {
            IOContent::Text(text) => {
                assert_eq!(text, &format!("m{}", TRANSCRIPT_CAP + 15), "newest is kept");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    // ── Approval dialogs (`UserIO::confirm`) ────────────────────────────────

    /// The request an agent parked in `UserIO::confirm` puts on the wire.
    fn confirm_msg(prompt: &str) -> IOMessage {
        IOMessage::system_text(prompt).with_metadata(IO_TYPE, IO_TYPE_CONFIRM)
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Drive the panel until a dialog is open, yielding between ticks so the
    /// agent half of a `join!` — and the provider's outbound drain task — can
    /// make progress. Returns whether the dialog appeared within `budget`.
    async fn tick_until_confirm(panel: &mut ConversationPanel, budget: usize) -> bool {
        for _ in 0..budget {
            panel.tick();
            if panel.pending_confirm.is_some() {
                return true;
            }
            tokio::task::yield_now().await;
        }
        false
    }

    /// A tool guarded by a real `UserIO::confirm`, bumping `mutations` only on
    /// approval. Stands in for a destructive action without being one.
    async fn guarded_tool(
        user_io: &UserIO,
        prompt: &str,
        mutations: &AtomicUsize,
    ) -> Result<bool, IOError> {
        let response = user_io.confirm(prompt).await?;
        if response.confirmed {
            mutations.fetch_add(1, Ordering::SeqCst);
        }
        Ok(response.confirmed)
    }

    #[tokio::test]
    async fn typed_yes_approves_the_pending_tool_exactly_once() {
        // `submit_input` sends the `confirmed` metadata `UserIO::confirm`
        // reads; without it every typed "yes" would be denied.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let user_io = UserIO::new(Arc::new(provider));
        let mutations = AtomicUsize::new(0);

        let agent = guarded_tool(&user_io, "run `rm -rf build/`?", &mutations);
        let user = async {
            assert!(
                tick_until_confirm(&mut panel, 32).await,
                "the confirm request opens a dialog"
            );
            type_str(&mut panel, "yes");
            panel.handle_key(key(KeyCode::Enter));
        };
        let (confirmed, ()) = tokio::join!(agent, user);

        assert!(confirmed.unwrap(), "a typed yes must approve");
        assert_eq!(
            mutations.load(Ordering::SeqCst),
            1,
            "approval runs the tool exactly once"
        );
        assert!(panel.pending_confirm.is_none(), "an answered dialog closes");
    }

    #[tokio::test]
    async fn typed_no_denies_and_the_tool_never_runs() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let user_io = UserIO::new(Arc::new(provider));
        let mutations = AtomicUsize::new(0);

        let agent = guarded_tool(&user_io, "run `rm -rf build/`?", &mutations);
        let user = async {
            assert!(tick_until_confirm(&mut panel, 32).await);
            type_str(&mut panel, "n");
            panel.handle_key(key(KeyCode::Enter));
        };
        let (confirmed, ()) = tokio::join!(agent, user);

        assert!(!confirmed.unwrap(), "a typed no must deny");
        assert_eq!(
            mutations.load(Ordering::SeqCst),
            0,
            "denial runs the tool zero times"
        );
        assert!(panel.pending_confirm.is_none());
    }

    #[tokio::test]
    async fn esc_denies_the_open_dialog() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let user_io = UserIO::new(Arc::new(provider));
        let mutations = AtomicUsize::new(0);

        let agent = guarded_tool(&user_io, "run `rm -rf build/`?", &mutations);
        let user = async {
            assert!(tick_until_confirm(&mut panel, 32).await);
            panel.handle_key(key(KeyCode::Esc));
        };
        let (confirmed, ()) = tokio::join!(agent, user);

        assert!(!confirmed.unwrap());
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        assert!(panel.pending_confirm.is_none());
    }

    #[tokio::test]
    async fn an_ordinary_yes_outside_a_dialog_carries_no_approval() {
        use polaris::plugins::IOProvider;
        // Nothing is pending, so "yes" is just a message. Stamping it would
        // let an unrelated line approve the next tool that asks.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "yes");
        panel.handle_key(key(KeyCode::Enter));

        let message = provider.receive().await.unwrap();
        assert!(
            !message.metadata.contains_key(CONFIRMED),
            "no dialog → no confirmed metadata: {:?}",
            message.metadata
        );
    }

    #[tokio::test]
    async fn a_queued_prompt_neither_approves_nor_is_lost() {
        // Typed before the tool asked, so it is already in the channel when
        // `confirm` starts receiving. It must not become the answer, and it
        // must still arrive on the turn it was meant for.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let user_io = UserIO::new(Arc::new(provider));
        let mutations = AtomicUsize::new(0);
        type_str(&mut panel, "yes");
        panel.handle_key(key(KeyCode::Enter));

        let agent = async {
            let confirmed = guarded_tool(&user_io, "run `rm -rf build/`?", &mutations).await;
            (confirmed, user_io.receive().await)
        };
        let user = async {
            assert!(tick_until_confirm(&mut panel, 32).await);
            type_str(&mut panel, "n");
            panel.handle_key(key(KeyCode::Enter));
        };
        let ((confirmed, queued), ()) = tokio::join!(agent, user);

        assert!(
            !confirmed.unwrap(),
            "the pre-typed prompt must not answer the dialog"
        );
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        let queued = queued.unwrap();
        match &queued.content {
            IOContent::Text(text) => assert_eq!(text, "yes", "the prompt keeps its own turn"),
            other => panic!("expected text, got {other:?}"),
        }
        assert!(!queued.metadata.contains_key(CONFIRMED));
    }

    #[tokio::test]
    async fn ambiguous_input_leaves_the_dialog_open_and_keeps_the_prompt() {
        // Anything that is not a plain yes/no is an ordinary prompt: it cannot
        // run the tool, and it is not thrown away as a failed answer either.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let user_io = UserIO::new(Arc::new(provider));
        let mutations = AtomicUsize::new(0);

        let agent = async {
            let confirmed = guarded_tool(&user_io, "run `rm -rf build/`?", &mutations).await;
            (confirmed, user_io.receive().await)
        };
        let user = async {
            assert!(tick_until_confirm(&mut panel, 32).await);
            type_str(&mut panel, "actually, rename it first");
            panel.handle_key(key(KeyCode::Enter));
            assert!(
                panel.pending_confirm.is_some(),
                "an ambiguous line does not resolve the dialog"
            );
            assert!(
                panel.confirm_notice.is_some(),
                "the banner explains why the dialog is still waiting"
            );
            assert_eq!(mutations.load(Ordering::SeqCst), 0, "and nothing ran");
            type_str(&mut panel, "y");
            panel.handle_key(key(KeyCode::Enter));
        };
        let ((confirmed, queued), ()) = tokio::join!(agent, user);

        assert!(confirmed.unwrap(), "the later yes still approves");
        assert_eq!(mutations.load(Ordering::SeqCst), 1);
        match &queued.unwrap().content {
            IOContent::Text(text) => assert_eq!(text, "actually, rename it first"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closing_the_channel_mid_dialog_cannot_run_the_tool() {
        let (provider, panel) = TuiIOProvider::new_with_panel(8);
        let user_io = UserIO::new(Arc::new(provider));
        let mutations = AtomicUsize::new(0);

        let agent = guarded_tool(&user_io, "run `rm -rf build/`?", &mutations);
        let user = async move {
            let mut panel = panel;
            assert!(tick_until_confirm(&mut panel, 32).await);
            // The panel owns the sole inbound sender; dropping it is shutdown.
            drop(panel);
        };
        let (result, ()) = tokio::join!(agent, user);

        assert!(
            matches!(result, Err(IOError::Closed)),
            "a closed confirmation path is an error, not an approval: {result:?}"
        );
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_undeliverable_answer_keeps_the_dialog_open() {
        use polaris::plugins::IOProvider;
        // A full channel must not degrade into an unstamped message — that
        // would reach `confirm` as a denial the user never gave — nor vanish.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(1);
        type_str(&mut panel, "filler");
        panel.handle_key(key(KeyCode::Enter));
        panel
            .inbox
            .lock()
            .push_back(confirm_msg("run `rm -rf build/`?"));
        panel.tick();
        assert!(panel.pending_confirm.is_some());

        type_str(&mut panel, "y");
        panel.handle_key(key(KeyCode::Enter));

        assert!(
            panel.pending_confirm.is_some(),
            "an undelivered answer leaves the dialog open"
        );
        assert_eq!(
            panel.composer.text(),
            "y",
            "and leaves the answer in the input"
        );
        assert_eq!(panel.composer.cursor(), 1);
        assert_eq!(
            panel.delivery_failure,
            Some(DeliveryFailure::Backlogged),
            "the banner says the channel refused it, and that Enter retries"
        );

        // The one queued message is still the filler, not a stand-in answer.
        let message = provider.receive().await.unwrap();
        match &message.content {
            IOContent::Text(text) => assert_eq!(text, "filler"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirm_request_shows_its_preview_and_an_actionable_banner() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(confirm_msg("apply edit to src/main.rs\n+ let x = 1;"));
        panel.tick();

        let rendered: Vec<String> = panel.message_lines().iter().map(line_text).collect();
        assert!(
            rendered[0].starts_with("approve ❯ "),
            "an approval request is labelled by kind, not source: {rendered:?}"
        );
        assert!(rendered[0].ends_with("apply edit to src/main.rs"));
        assert!(
            rendered[1].ends_with("+ let x = 1;"),
            "the preview keeps its own rows: {rendered:?}"
        );

        let banner = line_text(&panel.confirm_line().expect("dialog is open"));
        assert!(
            banner.contains("y/yes"),
            "banner names the approve key: {banner}"
        );
        assert!(
            banner.contains("n/no"),
            "banner names the deny key: {banner}"
        );
        assert!(
            banner.contains("apply edit to src/main.rs"),
            "banner keeps the preview in view: {banner}"
        );
    }

    #[tokio::test]
    async fn submit_empty_is_noop() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.handle_key(key(KeyCode::Enter));
        assert!(panel.transcript.is_empty());
    }

    fn status_msg(text: &str) -> IOMessage {
        IOMessage::from_agent("code", IOContent::Text(text.to_string()))
            .with_metadata(META_TYPE, TYPE_STATUS)
    }

    #[tokio::test]
    async fn status_messages_drive_the_status_line_not_the_transcript() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().push_back(status_msg("Thinking…"));
        panel.tick();
        assert_eq!(panel.status.as_deref(), Some("Thinking…"));
        assert!(
            panel.transcript.is_empty(),
            "status must not append to transcript"
        );

        // Empty status clears the line back to idle.
        panel.inbox.lock().push_back(status_msg(""));
        panel.tick();
        assert_eq!(panel.status, None);
        assert!(panel.transcript.is_empty());
    }

    #[tokio::test]
    async fn status_report_keeps_activity_and_distinguishes_window_from_tally() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().push_back(status_msg("Thinking…"));
        panel.inbox.lock().push_back(crate::commands::report_status(
            &StatusReport::default()
                .with_model("model-x")
                .with_context(
                    crate::commands::ContextWindow::new(48_000)
                        .with_limit(120_000)
                        .estimated(true)
                        .compacting(true),
                )
                .unpriced(),
        ));
        panel.tick();
        assert_eq!(panel.status.as_deref(), Some("Thinking…"));
        assert_eq!(panel.tokens_in, 0, "projected context is not billed usage");
        let line = panel.meta_line().to_string();
        assert!(line.contains("ctx ~48.0k/120.0k (40%)"), "{line}");
        assert!(line.contains("compacting"), "{line}");
        assert!(line.contains("cost n/a"), "{line}");
    }

    #[tokio::test]
    async fn a_running_task_count_is_one_more_dim_fact_and_zero_retracts_it() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().push_back(crate::commands::report_status(
            &StatusReport::default()
                .with_model("model-x")
                .with_cost_usd(0.0412),
        ));
        panel.tick();
        let quiet = panel.meta_line().to_string();
        assert!(!quiet.contains("running"), "{quiet}");

        // A partial refresh naming only the count keeps everything else.
        panel.inbox.lock().push_back(crate::commands::report_status(
            &StatusReport::default().with_tasks_running(2),
        ));
        panel.tick();
        let busy = panel.meta_line().to_string();
        assert!(busy.contains("2 running"), "{busy}");
        assert!(
            busy.contains("model-x"),
            "a partial report forgets nothing: {busy}"
        );
        assert!(busy.contains("$0.0412"), "{busy}");

        // The last task finishing is reported as zero and renders as nothing:
        // there is no hidden work left to be discoverable about.
        panel.inbox.lock().push_back(crate::commands::report_status(
            &StatusReport::default().with_tasks_running(0),
        ));
        panel.tick();
        let idle = panel.meta_line().to_string();
        assert!(!idle.contains("running"), "{idle}");
        assert!(idle.contains("model-x"), "{idle}");
    }

    #[tokio::test]
    async fn a_backgrounded_tool_block_renders_and_later_updates_still_fold() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(tool("t0", None, "run_command", "start", "cargo test"));
        panel.inbox.lock().push_back(tool(
            "t0",
            None,
            "run_command",
            "running",
            "   Compiling polaris-tui",
        ));
        panel.tick();
        assert_eq!(
            panel.transcript.len(),
            1,
            "a live update folds into the block it names"
        );

        panel.inbox.lock().push_back(tool(
            "t0",
            None,
            "run_command",
            "background",
            "backgrounded as task 3 — /tasks lists it, /stop 3 ends it",
        ));
        panel.tick();
        assert_eq!(
            panel.transcript.len(),
            1,
            "backgrounding is not a new block"
        );
        let text = rendered(&panel);
        assert!(
            text.contains("backgrounded as task 3"),
            "an unrecognised phase still renders its body: {text}"
        );
        assert!(
            text.contains("run_command"),
            "and keeps the label it had: {text}"
        );

        // The completion arrives long after the turn moved on, and still folds.
        panel
            .inbox
            .lock()
            .push_back(tool("t0", None, "run_command", "ok", "exit 0 in 94s"));
        panel.tick();
        assert_eq!(panel.transcript.len(), 1);
        let text = rendered(&panel);
        assert!(text.contains("exit 0 in 94s"), "{text}");
        assert!(
            !text.contains("backgrounded as task 3"),
            "the block shows its state, not a pile of them: {text}"
        );
    }

    #[tokio::test]
    async fn unavailable_host_preserves_draft_and_refuses_delivery() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(crate::commands::report_unavailable(
                "driver stopped; restart the session",
            ));
        panel.tick();
        type_str(&mut panel, "do not lose me");
        panel.submit_input();
        assert_eq!(panel.composer.text(), "do not lose me");
        assert_eq!(panel.delivery_failure, Some(DeliveryFailure::Unavailable));
        assert!(panel.meta_line().to_string().contains("driver stopped"));
        assert!(nothing_submitted(&panel));
    }

    #[tokio::test]
    async fn system_error_latches_until_host_reports_available() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().push_back(
            IOMessage::new(
                IOContent::Text("provider failed; retry".into()),
                IOSource::System,
            )
            .with_metadata(IO_TYPE, TYPE_ERROR),
        );
        panel.tick();
        assert!(panel.meta_line().to_string().contains("provider failed"));
        assert_eq!(
            panel.transcript.len(),
            1,
            "errors remain actionable content"
        );

        panel.inbox.lock().push_back(crate::commands::report_status(
            &StatusReport::default().with_availability(Availability::Available),
        ));
        panel.tick();
        assert!(!panel.meta_line().to_string().contains("provider failed"));
    }

    #[tokio::test]
    async fn narrow_meta_line_drops_token_tally_before_activity() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.status = Some("Working".into());
        panel.tokens_in = 1234;
        panel.tokens_out = 56;
        let line = panel.meta_line_for_width(32).to_string();
        assert!(line.contains("Working"), "{line}");
        assert!(!line.contains("tok"), "{line}");
    }

    #[tokio::test]
    async fn token_messages_accumulate_into_the_tally() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let tokens = |i: &str, o: &str| {
            IOMessage::from_agent("code", IOContent::Text(String::new()))
                .with_metadata(META_TYPE, TYPE_TOKENS)
                .with_metadata("in", i)
                .with_metadata("out", o)
        };
        panel.inbox.lock().push_back(tokens("100", "20"));
        panel.inbox.lock().push_back(tokens("50", "5"));
        panel.tick();
        assert_eq!(panel.tokens_in, 150);
        assert_eq!(panel.tokens_out, 25);
        // Token deltas are not rendered as transcript lines.
        assert!(panel.transcript.is_empty());
    }

    #[tokio::test]
    async fn malformed_or_absent_token_metadata_counts_as_zero() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let base = || {
            IOMessage::from_agent("code", IOContent::Text(String::new()))
                .with_metadata(META_TYPE, TYPE_TOKENS)
        };
        // Non-numeric, negative, and overflowing values all parse to 0 rather
        // than poisoning the tally; absent keys likewise contribute 0.
        panel
            .inbox
            .lock()
            .push_back(base().with_metadata("in", "abc"));
        panel
            .inbox
            .lock()
            .push_back(base().with_metadata("out", "-5"));
        panel
            .inbox
            .lock()
            .push_back(base().with_metadata("in", "99999999999999999999999999"));
        panel.inbox.lock().push_back(base()); // both keys absent
        // One well-formed delta proves the panel still accumulates afterward.
        panel
            .inbox
            .lock()
            .push_back(base().with_metadata("in", "7").with_metadata("out", "3"));
        panel.tick();
        assert_eq!(panel.tokens_in, 7);
        assert_eq!(panel.tokens_out, 3);
        assert!(panel.transcript.is_empty());
    }

    #[tokio::test]
    async fn meta_line_hidden_until_protocol_is_used() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // No status/token traffic → meta line stays collapsed.
        panel.tick();
        assert!(!panel.meta_active);
        // First status activates it, and it stays active thereafter.
        panel.inbox.lock().push_back(status_msg("Thinking…"));
        panel.tick();
        assert!(panel.meta_active);
    }

    #[tokio::test]
    async fn spinner_advances_only_while_busy() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // Idle: spinner frozen.
        panel.tick();
        assert_eq!(panel.spinner, 0);
        // Busy: spinner advances each tick.
        panel.inbox.lock().push_back(status_msg("Running tools…"));
        panel.tick();
        let after_first = panel.spinner;
        panel.tick();
        assert!(panel.spinner > after_first);
    }

    fn node_status(node: &str, seq: u64, text: &str) -> IOMessage {
        IOMessage::from_agent("code", IOContent::Text(text.to_string()))
            .with_metadata(META_TYPE, TYPE_STATUS)
            .with_metadata(META_NODE, node)
            .with_metadata(META_SEQ, seq.to_string())
    }

    #[tokio::test]
    async fn node_bearing_status_grows_the_timeline_in_order() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(node_status("prepare_context", 1, "Preparing context…"));
        panel
            .inbox
            .lock()
            .push_back(node_status("act", 2, "Thinking…"));
        panel.tick();
        assert_eq!(panel.timeline.len(), 2);
        assert_eq!(panel.timeline[0].node_name, "prepare_context");
        assert_eq!(panel.timeline[1].node_name, "act");
        assert_eq!(panel.timeline[1].seq, 2);
        // A plain status without `node` must not touch the timeline.
        panel.inbox.lock().push_back(status_msg("Finalizing…"));
        panel.tick();
        assert_eq!(panel.timeline.len(), 2);
    }

    #[tokio::test]
    async fn idle_status_arms_a_reset_cleared_by_the_next_turn() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(node_status("act", 1, "Thinking…"));
        panel.tick();
        assert_eq!(panel.timeline.len(), 1);

        // Turn finished → idle marker. The finished turn stays visible.
        panel
            .inbox
            .lock()
            .push_back(node_status("receive_user_input", 0, ""));
        panel.tick();
        assert!(panel.reset_pending);
        assert_eq!(
            panel.timeline.len(),
            1,
            "finished turn stays until next work"
        );

        // Next turn's first node clears the old timeline, then appends.
        panel
            .inbox
            .lock()
            .push_back(node_status("prepare_context", 1, "Preparing context…"));
        panel.tick();
        assert!(!panel.reset_pending);
        assert_eq!(panel.timeline.len(), 1);
        assert_eq!(panel.timeline[0].node_name, "prepare_context");
    }

    #[tokio::test]
    async fn wheel_scroll_is_consumed_and_clicks_pass_through() {
        // The panel's contract is which mouse events it claims; the scroll
        // *effect* on the viewport is covered by the `ScrollableTranscript`
        // tests in `widgets`. A click must fall through so it isn't swallowed.
        use crossterm::event::{MouseButton, MouseEvent};
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let wheel = |kind| {
            TuiEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
            assert!(matches!(
                panel.handle_event(&wheel(kind)),
                EventOutcome::Consumed
            ));
        }
        assert!(matches!(
            panel.handle_event(&wheel(MouseEventKind::Down(MouseButton::Left))),
            EventOutcome::Continue
        ));
    }

    /// Push enough short messages that the transcript overflows a small
    /// viewport, then render at `width x height` so `panel.view` picks up a
    /// scrollbar gutter at its last column.
    fn overflowing_panel(width: u16, height: u16) -> (ConversationPanel, u16) {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        for i in 0..50 {
            panel.transcript.push_back(IOMessage::from_agent(
                "code",
                IOContent::Text(format!("line {i}")),
            ));
        }
        draw(&mut panel, width, height);
        assert!(
            panel.view.is_following(),
            "fresh transcript starts pinned to the tail"
        );
        (panel, width - 1)
    }

    #[tokio::test]
    async fn gutter_click_and_drag_scroll_the_transcript() {
        use crossterm::event::MouseEvent;
        // 24x9: the composer's own 3 rows leave a 6-row transcript viewport
        // (rows 0..=5), far short of 50 messages, so a gutter is reserved.
        let (mut panel, gutter_column) = overflowing_panel(24, 9);
        let click = |kind, row| {
            TuiEvent::Mouse(MouseEvent {
                kind,
                column: gutter_column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        assert_eq!(
            panel.handle_event(&click(MouseEventKind::Down(MouseButton::Left), 0)),
            EventOutcome::Consumed,
            "a press on the gutter must be claimed, not passed through"
        );
        assert_eq!(
            panel.view.offset(),
            0,
            "the top of the track jumps to the very start"
        );
        assert!(!panel.view.is_following());

        assert_eq!(
            panel.handle_event(&click(MouseEventKind::Drag(MouseButton::Left), 5)),
            EventOutcome::Consumed,
            "a drag on the gutter must also be claimed"
        );
        assert!(
            panel.view.offset() > 0,
            "dragging away from the top must move the scroll position"
        );
    }

    #[tokio::test]
    async fn click_and_drag_outside_the_gutter_still_pass_through() {
        use crossterm::event::MouseEvent;
        let (mut panel, gutter_column) = overflowing_panel(24, 9);
        let click = |kind, column| {
            TuiEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 2,
                modifiers: KeyModifiers::NONE,
            })
        };

        for column in [0, gutter_column - 1] {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Drag(MouseButton::Left),
            ] {
                assert_eq!(
                    panel.handle_event(&click(kind, column)),
                    EventOutcome::Continue,
                    "column {column} is outside the gutter and must reach the terminal, \
                     so native text selection keeps working"
                );
            }
        }
        assert!(
            panel.view.is_following(),
            "an unclaimed click must not move the scroll"
        );
    }

    #[tokio::test]
    async fn no_gutter_click_passes_through_on_every_column() {
        use crossterm::event::MouseEvent;
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .transcript
            .push_back(IOMessage::from_agent("code", IOContent::Text("hi".into())));
        // Tall enough that one short message never overflows: gutter = 0.
        let (width, height) = (24, 20);
        draw(&mut panel, width, height);
        assert!(panel.view.is_following());

        let click = |kind, column| {
            TuiEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        };
        for column in 0..width {
            assert_eq!(
                panel.handle_event(&click(MouseEventKind::Down(MouseButton::Left), column)),
                EventOutcome::Continue,
                "column {column} must pass through: there is no scrollbar to hit"
            );
        }
    }

    #[tokio::test]
    async fn ctrl_o_toggles_the_timeline_view() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        assert!(!panel.expanded);
        panel.handle_key(ctrl(KeyCode::Char('o')));
        assert!(panel.expanded);
        panel.handle_key(ctrl(KeyCode::Char('o')));
        assert!(!panel.expanded);
    }

    #[tokio::test]
    async fn expanded_timeline_shows_only_matching_node_scoped_logs() {
        use crate::panels::{LogEntry, LogLevel};

        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let buffer = LogBuffer::with_capacity(16);
        let log = |message: &str, target: &str, node: &str, seq: u64| LogEntry {
            level: LogLevel::Info,
            target: target.to_string(),
            message: message.to_string(),
            node: Some(node.to_string()),
            seq: Some(seq),
        };
        buffer.push(log(
            "ran the tool",
            "polaris_graph::executor",
            "execute_tools",
            2,
        ));
        buffer.push(log(
            "from another node",
            "polaris_graph::executor",
            "act",
            1,
        ));
        // Non-framework target under the same (node, seq) — must be filtered.
        buffer.push(log("hyper connection", "hyper::client", "execute_tools", 2));
        panel.set_log_buffer(buffer);

        panel
            .inbox
            .lock()
            .push_back(node_status("execute_tools", 2, "Running tools…"));
        panel.tick();

        let text: String = panel
            .timeline_lines()
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("execute_tools"));
        assert!(text.contains("ran the tool"));
        // A log from a different (node, seq) must not leak into this node.
        assert!(!text.contains("from another node"));
        // A non-`polaris*` target under this node is filtered out.
        assert!(!text.contains("hyper connection"));
    }
    // ── Live agent transcript: streamed message blocks and tool activity ────

    /// One `message` delta under `id`, at `seq`.
    fn delta(id: &str, seq: u64, text: &str) -> IOMessage {
        IOMessage::from_agent("code", IOContent::Text(text.to_string()))
            .with_metadata(META_TYPE, TYPE_MESSAGE)
            .with_metadata(META_ID, id)
            .with_metadata(META_SEQ, seq.to_string())
    }

    /// The closing message of block `id`. An empty `text` keeps whatever the
    /// deltas accumulated.
    fn closing(id: &str, seq: u64, text: &str, outcome: &str) -> IOMessage {
        delta(id, seq, text)
            .with_metadata(META_FINAL, FINAL_TRUE)
            .with_metadata(META_OUTCOME, outcome)
    }

    /// One `tool` activity update.
    fn tool(id: &str, parent: Option<&str>, label: &str, phase: &str, body: &str) -> IOMessage {
        let mut msg = IOMessage::from_agent("code", IOContent::Text(body.to_string()))
            .with_metadata(META_TYPE, TYPE_TOOL)
            .with_metadata(META_ID, id)
            .with_metadata(META_LABEL, label)
            .with_metadata(META_PHASE, phase)
            .with_metadata(META_ELAPSED_MS, "1500");
        if let Some(parent) = parent {
            msg = msg.with_metadata(META_PARENT, parent);
        }
        msg
    }

    fn rendered(panel: &ConversationPanel) -> String {
        panel
            .message_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn text_deltas_fold_into_one_growing_block() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            delta("m0", 0, "Hel"),
            delta("m0", 1, "lo "),
            delta("m0", 2, "world"),
        ]);
        panel.drain_inbox();

        assert_eq!(
            panel.transcript.len(),
            1,
            "three deltas of one message must be one transcript block, not three"
        );
        assert!(rendered(&panel).contains("Hello world"));
    }

    #[tokio::test]
    async fn a_repeated_or_reordered_delta_cannot_duplicate_text() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            delta("m0", 0, "once"),
            // A resend of a delta already applied.
            delta("m0", 0, "once"),
            delta("m0", 1, " twice"),
            // An out-of-order arrival: `seq` does not advance the block.
            delta("m0", 1, " twice"),
            delta("m0", 0, "once"),
        ]);
        panel.drain_inbox();

        let text = rendered(&panel);
        assert_eq!(
            text.matches("once").count(),
            1,
            "a repeated seq must be dropped, not appended: {text}"
        );
        assert_eq!(text.matches("twice").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn the_closing_message_reconciles_without_a_second_copy() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // A delta goes missing between "The " and "answer".
        panel.inbox.lock().extend([
            delta("m0", 0, "The "),
            closing("m0", 5, "The full answer", "complete"),
        ]);
        panel.drain_inbox();

        let text = rendered(&panel);
        assert_eq!(panel.transcript.len(), 1, "the final must fold, not append");
        assert!(
            text.contains("The full answer"),
            "the authoritative text must replace the accumulation: {text}"
        );
        assert_eq!(
            text.matches("The ").count(),
            1,
            "the partial prefix must not survive alongside it: {text}"
        );
    }

    #[tokio::test]
    async fn a_duplicate_final_is_dropped() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            delta("m0", 0, "answer"),
            closing("m0", 1, "answer", "complete"),
            closing("m0", 1, "answer", "complete"),
        ]);
        panel.drain_inbox();

        let text = rendered(&panel);
        assert_eq!(panel.transcript.len(), 1);
        assert_eq!(text.matches("answer").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn a_failed_stream_keeps_its_partial_text_and_says_it_ended() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            delta("m0", 0, "as far as I got"),
            closing("m0", 1, "", "error"),
        ]);
        panel.drain_inbox();

        let text = rendered(&panel);
        assert!(
            text.contains("as far as I got"),
            "an empty closing body must keep the partial text: {text}"
        );
        assert!(
            text.contains("response ended early"),
            "a failed stream needs an explicit outcome: {text}"
        );
    }

    #[tokio::test]
    async fn a_retry_streams_into_its_own_block() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // The first attempt died; the retry carries a fresh id.
        panel.inbox.lock().extend([
            delta("m0", 0, "first try"),
            closing("m0", 1, "", "error"),
            delta("m1", 0, "second try"),
            closing("m1", 1, "second try", "complete"),
        ]);
        panel.drain_inbox();

        assert_eq!(
            panel.transcript.len(),
            2,
            "a retry must not re-enter the dead block"
        );
        let text = rendered(&panel);
        assert!(
            text.contains("first try") && text.contains("second try"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_tool_invocation_stays_one_row_through_its_phases() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            tool("call_1", Some("m0"), "edit_file", "start", "src/lib.rs"),
            tool("call_1", Some("m0"), "edit_file", "pending", "src/lib.rs"),
            tool(
                "call_1",
                Some("m0"),
                "edit_file",
                "ok",
                "replaced 1 occurrence",
            ),
        ]);
        panel.drain_inbox();

        assert_eq!(
            panel.transcript.len(),
            1,
            "three phases of one invocation must be one row"
        );
        let text = rendered(&panel);
        assert!(text.contains("edit_file"), "{text}");
        assert!(text.contains("replaced 1 occurrence"), "{text}");
        // A finished phase reports how long it took; a running one does not,
        // because the number would still be climbing.
        assert!(text.contains("1.5s"), "{text}");
        assert!(text.contains('✓'), "{text}");
    }

    #[tokio::test]
    async fn a_running_tool_shows_no_elapsed_time() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(tool("call_1", None, "search", "start", "needle"));
        panel.drain_inbox();
        assert!(!rendered(&panel).contains("1.5s"));
    }

    #[tokio::test]
    async fn a_denied_tool_renders_its_own_state() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            tool("call_1", Some("m0"), "run_command", "pending", "rm -rf /"),
            tool(
                "call_1",
                Some("m0"),
                "run_command",
                "denied",
                "User denied execution",
            ),
        ]);
        panel.drain_inbox();

        let text = rendered(&panel);
        assert_eq!(panel.transcript.len(), 1);
        assert!(text.contains('⊘'), "{text}");
        assert!(text.contains("User denied execution"), "{text}");
    }

    #[tokio::test]
    async fn a_tool_nests_under_the_message_that_requested_it() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            delta("m0", 0, "Let me read that file."),
            tool("call_1", Some("m0"), "read_file", "ok", "…"),
            tool("call_2", None, "search", "ok", "…"),
        ]);
        panel.drain_inbox();

        let lines: Vec<String> = panel
            .message_lines()
            .iter()
            .map(ToString::to_string)
            .collect();
        let nested = lines
            .iter()
            .find(|line| line.contains("read_file"))
            .expect("nested tool");
        let orphan = lines
            .iter()
            .find(|line| line.contains("search"))
            .expect("orphan tool");
        assert!(
            nested.starts_with("  "),
            "a parented tool indents: {nested:?}"
        );
        assert!(
            !orphan.starts_with(' '),
            "a tool with no parent stays at the top level: {orphan:?}"
        );
    }

    #[tokio::test]
    async fn interleaved_blocks_keep_their_own_identities() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // Two tools run under one message and report out of order.
        panel.inbox.lock().extend([
            delta("m0", 0, "Reading both."),
            tool("call_1", Some("m0"), "read_file", "start", "a.rs"),
            tool("call_2", Some("m0"), "read_file", "start", "b.rs"),
            tool("call_2", Some("m0"), "read_file", "ok", "contents of b"),
            delta("m0", 1, " Nearly done."),
            tool("call_1", Some("m0"), "read_file", "ok", "contents of a"),
        ]);
        panel.drain_inbox();

        assert_eq!(
            panel.transcript.len(),
            3,
            "one message block plus two tool rows"
        );
        let text = rendered(&panel);
        assert!(text.contains("Reading both. Nearly done."), "{text}");
        assert!(
            text.contains("contents of a") && text.contains("contents of b"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_message_without_an_id_is_standalone() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // The plain-consumer guarantee: no `id` means no folding, and an
        // absent `io_type` renders as plain text.
        panel.inbox.lock().extend([
            IOMessage::from_agent("code", IOContent::Text("first".into()))
                .with_metadata(META_TYPE, TYPE_MESSAGE),
            IOMessage::from_agent("code", IOContent::Text("second".into()))
                .with_metadata(META_TYPE, TYPE_MESSAGE),
            IOMessage::from_agent("code", IOContent::Text("plain".into())),
        ]);
        panel.drain_inbox();

        assert_eq!(panel.transcript.len(), 3, "without an id nothing folds");
        let text = rendered(&panel);
        assert!(text.contains("first") && text.contains("second") && text.contains("plain"));
    }

    #[tokio::test]
    async fn a_bounded_large_tool_result_renders_without_stalling() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let huge = "line of output\n".repeat(4000);
        panel
            .inbox
            .lock()
            .push_back(tool("call_1", Some("m0"), "run_command", "ok", &huge));
        panel.drain_inbox();

        let bounded = panel.message_lines();
        assert!(
            bounded.len() < 4000,
            "a large result must be bounded in the default view, got {} rows",
            bounded.len()
        );
        assert!(
            rendered(&panel).contains("Ctrl-O to expand"),
            "the user must be able to tell there is more"
        );

        // Bounded is a view budget, not a truncation: Ctrl-O gets it all back.
        panel.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(
            panel.message_lines().len() > bounded.len(),
            "the whole result must remain recoverable in the same session"
        );
    }

    #[tokio::test]
    async fn a_rich_message_with_a_large_non_diff_fenced_block_folds_behind_ctrl_o() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let code = (0..80)
            .map(|i| format!("let x{i} = {i};\n"))
            .collect::<String>();
        let body = format!("Here is the change:\n\n```rust\n{code}```");
        panel
            .inbox
            .lock()
            .push_back(closing("m0", 0, &body, "complete"));
        panel.drain_inbox();

        let bounded = panel.message_lines();
        let text = rendered(&panel);
        assert!(
            text.contains("Ctrl-O to expand"),
            "a large non-diff fenced block must fold exactly like a diff does: {text}"
        );
        assert!(!text.contains("x79"), "the tail must stay hidden: {text}");

        panel.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let text = rendered(&panel);
        assert!(
            panel.message_lines().len() > bounded.len(),
            "the whole block must remain recoverable in the same session"
        );
        assert!(text.contains("x79"), "Ctrl-O must return the hidden tail");
    }

    #[tokio::test]
    async fn a_rich_message_of_long_plain_prose_folds_behind_ctrl_o() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let body = (0..60)
            .map(|i| format!("Paragraph number {i}.\n\n"))
            .collect::<String>();
        panel
            .inbox
            .lock()
            .push_back(closing("m0", 0, &body, "complete"));
        panel.drain_inbox();

        let bounded = panel.message_lines();
        let text = rendered(&panel);
        assert!(
            text.contains("Ctrl-O to expand"),
            "a long fence-free message must fold too: {text}"
        );
        assert!(!text.contains("Paragraph number 59."));

        panel.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let text = rendered(&panel);
        assert!(panel.message_lines().len() > bounded.len());
        assert!(text.contains("Paragraph number 59."));
    }

    #[test]
    fn elapsed_time_reads_at_every_scale() {
        assert_eq!(fmt_elapsed(0), "0ms");
        assert_eq!(fmt_elapsed(999), "999ms");
        assert_eq!(fmt_elapsed(1000), "1.0s");
        assert_eq!(fmt_elapsed(59_999), "60.0s");
        assert_eq!(fmt_elapsed(60_000), "1m00s");
        assert_eq!(fmt_elapsed(125_000), "2m05s");
    }

    #[test]
    fn an_unrecognised_phase_stays_legible() {
        // The panel knows the lifecycle, not the host's vocabulary. A phase it
        // has never seen must still render a label, not a blank.
        let (glyph, _) = phase_style("quarantined");
        assert!(!glyph.is_empty());
    }

    #[tokio::test]
    async fn unknown_messages_are_retained_revealed_and_counted_while_errors_stay_visible() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().extend([
            IOMessage::system_text("plain message"),
            IOMessage::system_text("private diagnostic").with_metadata(IO_TYPE, "future-kind"),
            IOMessage::system_text("agent failed").with_metadata(IO_TYPE, "error"),
            IOMessage::system_text("working").with_metadata(IO_TYPE, "status"),
            IOMessage::system_text("usage").with_metadata(IO_TYPE, "tokens"),
        ]);
        panel.drain_inbox();
        let rendered = panel
            .message_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(rendered.contains("plain message"));
        assert!(rendered.contains("error ❯ │ agent failed"));
        assert!(!rendered.contains("private diagnostic"));
        assert!(!rendered.contains("working"));
        assert!(!rendered.contains("usage"));
        assert!(panel.meta_line().to_string().contains("1 hidden · Ctrl-O"));
        panel.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(
            panel
                .message_lines()
                .iter()
                .any(|line| line.to_string().contains("private diagnostic"))
        );
        assert!(panel.meta_line().to_string().contains("1 revealed"));
        panel.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        for _ in 0..TRANSCRIPT_CAP + 1 {
            panel.push_transcript(IOMessage::system_text("ordinary"));
        }
        panel.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(
            panel
                .message_lines()
                .iter()
                .any(|line| line.to_string().contains("private diagnostic"))
        );
    }
    #[tokio::test]
    async fn backend_shows_hidden_count_and_ctrl_o_reveals_the_retained_body() {
        use ratatui::{Terminal, backend::TestBackend};
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(IOMessage::system_text("hidden detail").with_metadata(IO_TYPE, "future"));
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let before: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(before.contains("1 hidden · Ctrl-O"));
        assert!(!before.contains("hidden detail"));
        panel.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let after: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(after.contains("hidden detail"));
        assert!(after.contains("1 revealed"));
    }

    // — Command and mention completion ——————————————————————————

    use crate::commands::{CompletionFuture, advertise_commands};

    fn host_commands() -> Vec<CommandSpec> {
        vec![
            CommandSpec::new("help", "List commands"),
            CommandSpec::new("model", "Pick a model").with_usage("<id>"),
            CommandSpec::new("sessions", "Browse sessions"),
        ]
    }

    /// Deliver a command advertisement the way a host would: through the
    /// provider's `send`, drained on the next tick.
    async fn advertise(provider: &TuiIOProvider, panel: &mut ConversationPanel) {
        provider
            .send(advertise_commands(&host_commands()))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        for _ in 0..50 {
            panel.tick();
            if panel.commands.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("advertisement never reached the panel");
    }

    /// A mention source that answers with `entries` filtered by substring,
    /// after an optional hold that a test releases by hand — long enough to
    /// observe a query in flight, and to make a stale result arrive late.
    struct ScriptedSource {
        entries: Vec<&'static str>,
        hold: Option<Arc<Notify>>,
        calls: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
    }

    impl CompletionSource for ScriptedSource {
        fn complete(&self, query: String, cancel: CancelToken) -> CompletionFuture {
            let entries = self.entries.clone();
            let hold = self.hold.clone();
            let calls = self.calls.clone();
            let cancelled = self.cancelled.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if let Some(hold) = hold {
                    hold.notified().await;
                }
                if cancel.is_cancelled() {
                    cancelled.fetch_add(1, Ordering::SeqCst);
                    return Vec::new();
                }
                entries
                    .into_iter()
                    .filter(|entry| entry.contains(query.as_str()))
                    .map(Completion::new)
                    .collect()
            })
        }
    }

    fn mention_panel(
        hold: Option<Arc<Notify>>,
    ) -> (TuiIOProvider, ConversationPanel, Arc<AtomicUsize>) {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let calls = Arc::new(AtomicUsize::new(0));
        panel.set_completion_source(Arc::new(ScriptedSource {
            entries: vec![
                "src/main.rs",
                "src/lib.rs",
                "docs/read me.md",
                "日本語/メモ.txt",
            ],
            hold,
            calls: calls.clone(),
            cancelled: Arc::new(AtomicUsize::new(0)),
        }));
        (provider, panel, calls)
    }

    /// Wait for the in-flight mention query to land in the popup.
    async fn settle(panel: &mut ConversationPanel) {
        for _ in 0..100 {
            tokio::task::yield_now().await;
            panel.tick();
            if panel.popup.as_ref().is_some_and(|popup| !popup.loading) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("mention query never settled");
    }

    fn popup_inserts(panel: &ConversationPanel) -> Vec<String> {
        panel
            .popup
            .as_ref()
            .map(|popup| popup.items.iter().map(|item| item.insert.clone()).collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn slash_is_plain_text_until_the_host_advertises_commands() {
        // The plain-consumer guarantee, on the input side: a consumer that never
        // advertises gets no popup, and `/help` reaches it as an ordinary
        // prompt with no command metadata.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "/help");
        assert!(panel.popup.is_none(), "no advertisement, no popup");
        assert_eq!(
            panel.handle_key(key(KeyCode::Tab)),
            EventOutcome::Continue,
            "Tab still cycles focus"
        );
        panel.handle_key(key(KeyCode::Enter));
        let received = provider.receive().await.unwrap();
        assert!(matches!(&received.content, IOContent::Text(t) if t == "/help"));
        assert!(!received.metadata.contains_key(IO_TYPE));
    }

    #[tokio::test]
    async fn advertisement_is_cached_without_disturbing_the_activity_line() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        // A running status first, so we can see the advertisement leave it be.
        provider.send(status_msg("Thinking…")).await.unwrap();
        advertise(&provider, &mut panel).await;
        assert_eq!(panel.commands.as_ref().unwrap().len(), 3);
        assert_eq!(panel.status.as_deref(), Some("Thinking…"));

        // With no status at all, an advertisement does not summon the meta
        // row: nothing about the transcript's look changes.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        assert!(!panel.meta_active);
        assert!(panel.transcript.is_empty());
    }

    #[tokio::test]
    async fn slash_opens_a_filtered_popup_and_tab_accepts() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        type_str(&mut panel, "/");
        assert_eq!(popup_inserts(&panel), ["help", "model", "sessions"]);
        type_str(&mut panel, "mo");
        assert_eq!(popup_inserts(&panel), ["model"]);

        // Tab is claimed only now, and accepting closes the token.
        assert_eq!(panel.handle_key(key(KeyCode::Tab)), EventOutcome::Consumed);
        assert_eq!(panel.composer.text(), "/model ");
        assert_eq!(panel.composer.cursor(), 7);
        assert!(
            panel.popup.is_none(),
            "a closed token has nothing to complete"
        );
        assert_eq!(
            panel.handle_key(key(KeyCode::Tab)),
            EventOutcome::Continue,
            "with the popup gone Tab is the shell's again"
        );
    }

    #[tokio::test]
    async fn popup_keys_cycle_dismiss_and_leave_the_draft_alone() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        type_str(&mut panel, "/");
        panel.handle_key(key(KeyCode::Down));
        assert_eq!(panel.popup.as_ref().unwrap().selected, 1);
        panel.handle_key(key(KeyCode::BackTab));
        assert_eq!(panel.popup.as_ref().unwrap().selected, 0);
        panel.handle_key(key(KeyCode::Up));
        assert_eq!(panel.popup.as_ref().unwrap().selected, 2);
        // Cycling never touched the draft.
        assert_eq!(panel.composer.text(), "/");
        assert_eq!(panel.composer.cursor(), 1);

        // Esc closes the popup and stays closed for cursor motion, but the
        // next edit brings it back.
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), EventOutcome::Consumed);
        assert!(panel.popup.is_none());
        assert_eq!(panel.composer.text(), "/");
        panel.handle_key(key(KeyCode::Home));
        panel.handle_key(key(KeyCode::End));
        assert!(panel.popup.is_none(), "an Esc survives cursor motion");
        type_str(&mut panel, "h");
        assert_eq!(popup_inserts(&panel), ["help"]);
    }

    #[tokio::test]
    async fn enter_fills_a_partial_command_and_runs_an_exact_one() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        type_str(&mut panel, "/he");
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.composer.text(), "/help ", "partial: Enter fills");
        assert!(nothing_submitted(&panel));

        // Fully typed: Enter dispatches rather than re-inserting.
        panel.composer.clear();
        panel.refresh_popup();
        type_str(&mut panel, "/help");
        assert!(panel.popup.as_ref().unwrap().query_is_exact());
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.composer.text(), "");
        let received = provider.receive().await.unwrap();
        assert_eq!(
            CommandInvocation::from_message(&received),
            Some(CommandInvocation::new("help", ""))
        );
    }

    #[tokio::test]
    async fn commands_dispatch_with_name_and_args_and_echo_locally() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        type_str(&mut panel, "/model opus");
        panel.handle_key(key(KeyCode::Enter));
        let received = provider.receive().await.unwrap();
        assert_eq!(
            CommandInvocation::from_message(&received),
            Some(CommandInvocation::new("model", "opus"))
        );
        // The echo is the line as typed, marked like any prompt.
        let echoed = &panel.transcript[0];
        assert!(matches!(&echoed.content, IOContent::Text(t) if t == "/model opus"));
        assert!(echoed.metadata.contains_key(META_SUBMISSION));
    }

    #[tokio::test]
    async fn unknown_commands_are_still_dispatched_for_the_host_to_report() {
        // The TUI does not know the host's set well enough to refuse; only
        // the host can say what it runs. Unknown names go through as
        // commands, never as prompts, so they cannot reach a model by
        // accident.
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        type_str(&mut panel, "/frobnicate");
        assert_eq!(popup_inserts(&panel), Vec::<String>::new());
        assert!(panel.popup.is_some(), "an empty popup still says so");
        // Tab with nothing to accept is a consumed no-op, not a focus change.
        assert_eq!(panel.handle_key(key(KeyCode::Tab)), EventOutcome::Consumed);
        assert_eq!(panel.composer.text(), "/frobnicate");
        type_str(&mut panel, " now");
        assert!(panel.popup.is_none(), "the space closed the command token");
        panel.handle_key(key(KeyCode::Enter));
        let received = provider.receive().await.unwrap();
        assert_eq!(
            CommandInvocation::from_message(&received),
            Some(CommandInvocation::new("frobnicate", "now"))
        );
    }

    #[tokio::test]
    async fn a_refused_command_keeps_the_draft() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(1);
        // Fill the single slot so the next offer is refused.
        panel
            .in_tx
            .try_send(IOMessage::user_text("filler"))
            .unwrap();
        let (provider, _) = TuiIOProvider::new_with_panel(8);
        let _ = provider;
        panel.commands = Some(host_commands());
        type_str(&mut panel, "/help");
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.composer.text(), "/help");
        assert_eq!(panel.delivery_failure, Some(DeliveryFailure::Backlogged));
        assert!(
            panel.transcript.is_empty(),
            "a refused command is not echoed"
        );
    }

    #[tokio::test]
    async fn at_is_plain_text_without_a_source() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "@src");
        assert!(panel.popup.is_none());
        assert_eq!(panel.handle_key(key(KeyCode::Tab)), EventOutcome::Continue);
    }

    #[tokio::test]
    async fn mentions_complete_asynchronously_and_escape_spaces_and_unicode() {
        let (_provider, mut panel, calls) = mention_panel(None);
        type_str(&mut panel, "see @");
        assert!(panel.popup.as_ref().unwrap().loading);
        settle(&mut panel).await;
        assert_eq!(popup_inserts(&panel).len(), 4);

        type_str(&mut panel, "read");
        settle(&mut panel).await;
        assert_eq!(popup_inserts(&panel), ["docs/read me.md"]);
        panel.handle_key(key(KeyCode::Tab));
        assert_eq!(panel.composer.text(), "see @docs/read\\ me.md ");
        assert!(panel.popup.is_none());

        type_str(&mut panel, "and @メモ");
        settle(&mut panel).await;
        assert_eq!(popup_inserts(&panel), ["日本語/メモ.txt"]);
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(
            panel.composer.text(),
            "see @docs/read\\ me.md and @日本語/メモ.txt "
        );
        assert!(
            nothing_submitted(&panel),
            "Enter accepted, it did not submit"
        );
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn cursor_motion_inside_a_mention_does_not_requery() {
        let (_provider, mut panel, calls) = mention_panel(None);
        type_str(&mut panel, "@src");
        settle(&mut panel).await;
        let before = calls.load(Ordering::SeqCst);
        panel.handle_key(key(KeyCode::Left));
        panel.handle_key(key(KeyCode::Right));
        assert_eq!(calls.load(Ordering::SeqCst), before);
        assert!(panel.popup.is_some());
        // Leaving the token closes the popup.
        panel.handle_key(key(KeyCode::Home));
        assert!(panel.popup.is_none());
    }

    #[tokio::test]
    async fn a_stale_mention_result_is_dropped() {
        // Query 1 is held; the user keeps typing, so query 2 runs and lands.
        // When query 1 is finally released its rows must not replace query
        // 2's — the popup shows what the user typed last.
        let hold = Arc::new(Notify::new());
        let (_provider, mut panel, _) = mention_panel(Some(hold.clone()));
        type_str(&mut panel, "@lib");
        assert!(panel.popup.as_ref().unwrap().loading);
        // Nothing lands while held.
        for _ in 0..3 {
            tokio::task::yield_now().await;
            panel.tick();
        }
        assert!(panel.popup.as_ref().unwrap().loading);

        // A new keystroke supersedes it: the first task is aborted, so the
        // hold release below reaches only the live query.
        type_str(&mut panel, "x");
        hold.notify_waiters();
        tokio::task::yield_now().await;
        hold.notify_waiters();
        settle(&mut panel).await;
        assert_eq!(popup_inserts(&panel), Vec::<String>::new());

        // Now force the race the other way round: plant a stale result by
        // hand and check the panel ignores it.
        let current = panel.mention_request.as_ref().map(|r| r.seq);
        assert!(current.is_none(), "the live query has settled");
        *panel.mention_results.lock() = Some((0, vec![Completion::new("ghost")]));
        panel.tick();
        assert_eq!(popup_inserts(&panel), Vec::<String>::new());
    }

    #[tokio::test]
    async fn an_abandoned_mention_query_is_cancelled() {
        let hold = Arc::new(Notify::new());
        let (_provider, mut panel, _) = mention_panel(Some(hold.clone()));
        type_str(&mut panel, "@src");
        let request = panel.mention_request.as_ref().unwrap();
        let token = request.cancel.clone();
        assert!(!token.is_cancelled());
        // Typing a space closes the token; the query is abandoned.
        type_str(&mut panel, " ");
        assert!(panel.popup.is_none());
        assert!(token.is_cancelled());
        assert!(panel.mention_request.is_none());
    }

    #[tokio::test]
    async fn the_popup_draws_above_the_banners_and_never_covers_them() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        provider.send(confirm_msg("run `ls`?")).await.unwrap();
        tokio::task::yield_now().await;
        for _ in 0..20 {
            panel.tick();
            if panel.pending_confirm.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(panel.pending_confirm.is_some());
        type_str(&mut panel, "/");
        let (rows, _) = draw(&mut panel, 60, 16);
        let joined = rows.join("\n");
        assert!(joined.contains("commands"), "{joined}");
        assert!(joined.contains("/help"), "{joined}");
        assert!(joined.contains("List commands"), "{joined}");
        assert!(
            joined.contains("approve?"),
            "the approval banner is intact\n{joined}"
        );
        let popup_row = rows.iter().position(|r| r.contains("/help")).unwrap();
        let banner_row = rows.iter().position(|r| r.contains("approve?")).unwrap();
        let input_row = rows.iter().position(|r| r.contains("› ")).unwrap();
        assert!(popup_row < banner_row && banner_row < input_row, "{joined}");

        // Too short to fit a box: the popup is simply not drawn, and nothing
        // else is disturbed.
        let (rows, _) = draw(&mut panel, 60, 5);
        assert!(!rows.join("\n").contains("List commands"));
    }

    #[tokio::test]
    async fn ctrl_c_clearing_the_draft_closes_the_popup() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        type_str(&mut panel, "/he");
        assert!(panel.popup.is_some());
        panel.handle_key(ctrl(KeyCode::Char('c')));
        assert_eq!(panel.composer.text(), "");
        assert!(panel.popup.is_none());
    }

    #[tokio::test]
    async fn a_pasted_command_line_opens_the_popup_but_does_not_dispatch() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        advertise(&provider, &mut panel).await;
        panel.handle_event(&TuiEvent::Paste("/mod".into()));
        assert_eq!(popup_inserts(&panel), ["model"]);
        assert!(nothing_submitted(&panel));
    }

    async fn history_ready(panel: &mut ConversationPanel) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                panel.tick();
                if !panel
                    .history
                    .as_ref()
                    .unwrap()
                    .banner()
                    .is_some_and(|banner| banner.contains("Loading"))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn history_only_activates_at_multiline_boundaries_and_restores_draft_on_escape() {
        use crate::composer::history::test_support::{Store, config};
        let store = Arc::new(Store::default());
        store
            .prompts
            .lock()
            .insert("p".into(), vec!["previous".into()]);
        let (_provider, panel) = TuiIOProvider::new();
        let mut panel = panel.with_history(config(store, "p", "s", 10));
        history_ready(&mut panel).await;
        panel.handle_event(&TuiEvent::Paste("first\nlast".into()));
        panel.handle_key(key(KeyCode::Up));
        assert_eq!(panel.composer.text(), "first\nlast");
        assert_eq!(panel.composer.caret(80).0, 0);
        let cursor = panel.composer.cursor();
        panel.handle_key(key(KeyCode::Up));
        assert_eq!(panel.composer.text(), "previous");
        panel.handle_key(key(KeyCode::Esc));
        assert_eq!(panel.composer.text(), "first\nlast");
        assert_eq!(panel.composer.cursor(), cursor);
        assert!(nothing_submitted(&panel));
        panel.composer.seek(panel.composer.text().len());
        panel.handle_key(key(KeyCode::Up));
        panel.handle_key(key(KeyCode::Up));
        panel.handle_key(key(KeyCode::Down));
        assert_eq!(panel.composer.text(), "first\nlast");
    }

    #[tokio::test]
    async fn search_accept_never_submits_and_cancel_does_not_answer_approval() {
        use crate::composer::history::test_support::{Store, config};
        let store = Arc::new(Store::default());
        store
            .prompts
            .lock()
            .insert("p".into(), vec!["yes".into(), "find 世界\nline".into()]);
        let (_provider, panel) = TuiIOProvider::new();
        let mut panel = panel.with_history(config(store, "p", "s", 10));
        history_ready(&mut panel).await;
        type_str(&mut panel, "draft");
        panel.composer.seek(2);
        panel.handle_key(ctrl(KeyCode::Char('r')));
        panel.handle_event(&TuiEvent::Paste("世界".into()));
        let (rows, _) = draw(&mut panel, 100, 15);
        assert!(rows.iter().any(|row| row.contains("Search: 世 界 ")));
        assert_eq!(panel.composer.text(), "find 世界\nline");
        panel.handle_key(key(KeyCode::Enter));
        assert!(nothing_submitted(&panel));
        assert_eq!(panel.composer.text(), "find 世界\nline");
        panel.handle_key(ctrl(KeyCode::Char('r')));
        type_str(&mut panel, "yes");
        panel.inbox.lock().push_back(confirm_msg("Allow?"));
        panel.tick();
        panel.handle_key(key(KeyCode::Esc));
        assert!(panel.pending_confirm.is_some());
        assert_eq!(panel.composer.text(), "find 世界\nline");
        assert!(nothing_submitted(&panel));
        panel.handle_key(ctrl(KeyCode::Char('r')));
        panel.handle_key(ctrl(KeyCode::Char('c')));
        assert!(panel.pending_confirm.is_some());
        assert_eq!(
            panel.handle_key(ctrl(KeyCode::Char('c'))),
            EventOutcome::Continue
        );
    }

    #[tokio::test]
    async fn only_accepted_ordinary_prompts_are_persisted_once() {
        use crate::composer::history::test_support::{Store, config};
        let store = Arc::new(Store::default());
        let (provider, panel) = TuiIOProvider::new_with_panel(1);
        let mut panel = panel.with_history(config(store.clone(), "project", "session", 10));
        history_ready(&mut panel).await;
        assert!(panel.submit_prompt("first"));
        assert!(!panel.submit_prompt("retry me"));
        provider.receive().await.unwrap();
        assert!(panel.submit_prompt("retry me"));
        provider.receive().await.unwrap();
        panel.inbox.lock().push_back(confirm_msg("approve?"));
        panel.tick();
        type_str(&mut panel, "yes");
        panel.submit_input();
        provider.receive().await.unwrap();
        assert!(panel.submit_prompt("yes")); // no dialog: this one is history
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.namespaces.lock().len() == 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store.prompts.lock()["project"],
            ["first", "retry me", "yes"]
        );
        assert!(
            store
                .namespaces
                .lock()
                .iter()
                .all(|ns| ns.session == "session")
        );
    }

    #[tokio::test]
    async fn editor_failure_preserves_caret_and_success_remains_an_editable_draft() {
        let (_provider, mut panel) = TuiIOProvider::new();
        type_str(&mut panel, "original 世界");
        panel.composer.seek(2);
        let (reply, receiver) = oneshot::channel();
        panel.editor_result = Some(receiver);
        panel.handle_key(key(KeyCode::Char('x')));
        panel.handle_event(&TuiEvent::Paste("ignored".into()));
        reply
            .send(Err(std::io::Error::other("fake editor failed")))
            .unwrap();
        panel.tick();
        assert_eq!(panel.composer.text(), "original 世界");
        assert_eq!(panel.composer.cursor(), 2);
        assert!(
            panel
                .banner_lines()
                .iter()
                .any(|line| line_text(line).contains("fake editor failed"))
        );
        let (reply, receiver) = oneshot::channel();
        panel.editor_result = Some(receiver);
        reply.send(Ok("edited 世界\n\n".into())).unwrap();
        panel.tick();
        assert_eq!(panel.composer.text(), "edited 世界\n\n");
        assert!(panel.editor_notice.is_none());
        assert!(nothing_submitted(&panel));
        panel.handle_key(key(KeyCode::Char('x')));
        assert_eq!(panel.composer.text(), "edited 世界\n\nx");
    }

    #[tokio::test]
    async fn session_replay_replaces_transcript_and_resets_process_counters() {
        use crate::commands::{session_replay_end, session_replay_start};

        let (_provider, mut panel) = TuiIOProvider::new();
        panel.push_transcript(IOMessage::system_text("old session output"));
        panel.tokens_in = 99;
        panel.tokens_out = 7;
        panel.inbox.lock().extend([
            session_replay_start("second"),
            IOMessage::user_text("saved prompt")
                .with_metadata(IO_TYPE, TYPE_MESSAGE)
                .with_metadata(META_ID, "history:0"),
            session_replay_end("second"),
        ]);

        panel.drain_inbox();

        assert_eq!(panel.transcript.len(), 3);
        assert!(panel.transcript.iter().all(|message| {
            !matches!(&message.content, IOContent::Text(text) if text == "old session output")
        }));
        assert_eq!(panel.tokens_in, 0);
        assert_eq!(panel.tokens_out, 0);
        assert!(panel.pending_confirm.is_none());
    }

    /// A host queue that records what the panel asked it to do, so a test can
    /// prove the panel did *not* reach into the outgoing session's queue.
    #[derive(Default)]
    struct RecordingQueue {
        removed: Mutex<Vec<u64>>,
    }

    impl TurnInputHandle for RecordingQueue {
        fn submit(&self, _text: String, mode: TurnInputMode) -> Result<TurnInputReceipt, String> {
            Ok(TurnInputReceipt {
                id: 7,
                target_run: 3,
                mode,
            })
        }

        fn replace(&self, _id: u64, _text: String) -> Result<(), String> {
            Ok(())
        }

        fn remove(&self, id: u64) -> Result<(), String> {
            self.removed.lock().push(id);
            Ok(())
        }
    }

    fn replay(panel: &mut ConversationPanel, id: &str) {
        use crate::commands::{session_replay_end, session_replay_start};
        panel
            .inbox
            .lock()
            .extend([session_replay_start(id), session_replay_end(id)]);
        panel.drain_inbox();
    }

    #[tokio::test]
    async fn session_replay_clears_the_status_report_and_the_last_error() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.inbox.lock().push_back(crate::commands::report_status(
            &StatusReport::default()
                .with_model("first-model")
                .with_workspace("first-workspace")
                .with_session("first-session")
                .with_permissions("first-permissions")
                .with_context(crate::commands::ContextWindow::new(90_000).with_limit(100_000))
                .with_cost_usd(12.34),
        ));
        panel.drain_inbox();
        assert!(panel.meta_line().to_string().contains("first-model"));

        // An error takes the whole row — the facts are still cached behind it,
        // which is exactly how they survive a switch unnoticed.
        panel.inbox.lock().push_back(
            IOMessage::new(IOContent::Text("first failed".into()), IOSource::System)
                .with_metadata(IO_TYPE, TYPE_ERROR),
        );
        panel.drain_inbox();
        assert!(panel.meta_line().to_string().contains("first failed"));
        // Per-turn progress state the previous session left behind. Neither is
        // drawn on its own, but both describe the turn that was running there.
        panel.reset_pending = true;
        panel.spinner = 5;

        replay(&mut panel, "second");

        assert_eq!(
            panel.report,
            StatusReport::default(),
            "no field of the outgoing session's report survives the switch"
        );
        assert_eq!(panel.last_error, None);
        assert!(!panel.reset_pending);
        assert_eq!(panel.spinner, 0);
        let line = panel.meta_line().to_string();
        assert!(line.contains("ready"), "{line}");
        for stale in [
            "first-model",
            "first-workspace",
            "first-session",
            "first-permissions",
            "ctx ",
            "$12.34",
            "first failed",
        ] {
            assert!(
                !line.contains(stale),
                "{stale} outlived its session: {line}"
            );
        }

        // The point of clearing rather than overwriting: `merge` only ever
        // *sets* the fields an update asserts, so a partial report from the
        // new session could never have displaced what the old one left.
        panel.inbox.lock().push_back(crate::commands::report_status(
            &StatusReport::default().with_model("second-model"),
        ));
        panel.drain_inbox();
        assert_eq!(panel.report.model.as_deref(), Some("second-model"));
        assert_eq!(panel.report.workspace, None);
        assert_eq!(panel.report.context, None);
        assert_eq!(panel.report.cost, Cost::Unstated);
    }

    #[tokio::test]
    async fn session_replay_lifts_unavailability_but_keeps_a_channel_failure() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(crate::commands::report_unavailable("first host is down"));
        panel.drain_inbox();
        type_str(&mut panel, "held back");
        panel.submit_input();
        assert_eq!(panel.delivery_failure, Some(DeliveryFailure::Unavailable));

        replay(&mut panel, "second");

        assert_eq!(panel.report.availability, Availability::Unstated);
        assert_eq!(
            panel.delivery_failure, None,
            "the refusal went with the posture that caused it"
        );
        assert_eq!(
            panel.composer.text(),
            "held back",
            "the draft the previous session refused is still the user's"
        );
        panel.submit_input();
        assert!(!nothing_submitted(&panel), "and now reaches the agent");

        // A backlogged channel is not a property of the session on screen:
        // the agent is still behind after the switch, and saying so is true.
        panel.delivery_failure = Some(DeliveryFailure::Backlogged);
        replay(&mut panel, "third");
        assert_eq!(panel.delivery_failure, Some(DeliveryFailure::Backlogged));
    }

    #[tokio::test]
    async fn session_replay_drops_queued_input_from_the_previous_session() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        let queue = Arc::new(RecordingQueue::default());
        panel.turn_input = Some(queue.clone());
        type_str(&mut panel, "queued for the first session");
        panel.submit_input();
        assert_eq!(panel.pending_turn_inputs.len(), 1);

        replay(&mut panel, "second");

        assert!(
            panel.pending_turn_inputs.is_empty(),
            "receipts name runs in the session that was just replaced"
        );
        assert!(
            queue.removed.lock().is_empty(),
            "the panel gives up its claim to the messages; it does not \
             decide the host's queue across a session boundary"
        );
        // Alt-Up recalls the newest unconsumed message. With none tracked it
        // is not the panel's key, and cannot paste the old session's text.
        assert_eq!(
            panel.handle_key(alt(KeyCode::Up)),
            EventOutcome::Continue,
            "no queued message means nothing to recall"
        );
        assert_eq!(panel.composer.text(), "");
    }

    #[tokio::test]
    async fn session_replay_keeps_the_draft_the_commands_and_the_submission_count() {
        let (_provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel
            .inbox
            .lock()
            .push_back(crate::commands::advertise_commands(&[
                crate::commands::CommandSpec::new("help", "List the available commands"),
            ]));
        panel.drain_inbox();
        type_str(&mut panel, "sent before the switch");
        panel.submit_input();
        assert_eq!(panel.submissions, 1);
        type_str(&mut panel, "still typing");
        panel.expanded = true;

        replay(&mut panel, "second");

        assert_eq!(
            panel.composer.text(),
            "still typing",
            "a switch the user asked for does not throw away their typing"
        );
        assert!(
            panel.commands.is_some(),
            "the host owns its command set, not the session"
        );
        assert_eq!(
            panel.submissions, 1,
            "the echo counter is paired with the provider's consumed tally"
        );
        assert!(
            panel.expanded,
            "the timeline view the user opened is theirs, not the session's"
        );
    }

    // ── Attachments ─────────────────────────────────────────────────────────

    /// A host resolver that answers from a fixed table, so every attachment
    /// test is deterministic and none of them touches a disk or a clipboard.
    #[derive(Default)]
    struct FakeAttachments {
        paths: Mutex<std::collections::HashMap<String, Result<Attachment, AttachmentRejected>>>,
        clipboard: Mutex<Option<Result<Attachment, AttachmentRejected>>>,
        /// When set, a resolve parks here until the test hands out a permit.
        gate: Option<Arc<tokio::sync::Semaphore>>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeAttachments {
        fn with(self, path: &str, outcome: Result<Attachment, AttachmentRejected>) -> Self {
            self.paths.lock().insert(path.to_string(), outcome);
            self
        }

        fn clipboard(self, outcome: Result<Attachment, AttachmentRejected>) -> Self {
            *self.clipboard.lock() = Some(outcome);
            self
        }

        fn gated(mut self, gate: Arc<tokio::sync::Semaphore>) -> Self {
            self.gate = Some(gate);
            self
        }
    }

    impl AttachmentSource for FakeAttachments {
        fn resolve(
            &self,
            request: AttachmentRequest,
            _cancel: CancelToken,
        ) -> crate::commands::AttachmentFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let answer = match &request {
                AttachmentRequest::Path(path) => self
                    .paths
                    .lock()
                    .get(path)
                    .cloned()
                    .unwrap_or(Err(AttachmentRejected::NotAttachable)),
                AttachmentRequest::Clipboard => self
                    .clipboard
                    .lock()
                    .clone()
                    .unwrap_or_else(|| Err(AttachmentRejected::failed("no clipboard image"))),
            };
            let gate = self.gate.clone();
            Box::pin(async move {
                if let Some(gate) = gate {
                    let _permit = gate.acquire().await.expect("gate closed");
                }
                answer
            })
        }
    }

    fn png(name: &str) -> Attachment {
        Attachment::new(format!("h-{name}"), name, "image/png", 4_096)
    }

    /// Let the spawned resolves finish and fold their results into the panel.
    async fn settle_attachments(panel: &mut ConversationPanel) {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        panel.drain_attachment_results();
    }

    fn chips(panel: &ConversationPanel) -> Vec<String> {
        panel
            .attachments
            .iter()
            .map(|chip| chip.summary())
            .collect()
    }

    fn attach_panel(source: Arc<FakeAttachments>) -> (TuiIOProvider, ConversationPanel) {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        panel.set_attachment_source(source);
        (provider, panel)
    }

    #[tokio::test]
    async fn a_mentioned_image_becomes_a_chip_whose_handle_rides_the_prompt() {
        let source =
            Arc::new(FakeAttachments::default().with("diagram.png", Ok(png("diagram.png"))));
        let (provider, mut panel) = attach_panel(source);

        type_str(&mut panel, "look at @diagram.png");
        settle_attachments(&mut panel).await;
        assert_eq!(chips(&panel), ["diagram.png · image/png · 4.0 KB"]);

        panel.handle_key(key(KeyCode::Enter));
        let sent = provider.receive().await.unwrap();
        match &sent.content {
            IOContent::Text(text) => assert_eq!(
                text, "look at @diagram.png",
                "the prompt text is the user's, unchanged — the image rides beside it"
            ),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(
            attachment_handles(&sent),
            ["h-diagram.png"],
            "the host redeems its own handle; the panel never carried the bytes"
        );
        assert!(
            panel.attachments.is_empty(),
            "a delivered prompt takes its chips with it"
        );
    }

    /// The chip and the mention are one fact, so editing either one moves both.
    #[tokio::test]
    async fn editing_the_mention_away_drops_the_chip_and_retyping_brings_it_back() {
        let source = Arc::new(FakeAttachments::default().with("a.png", Ok(png("a.png"))));
        let calls = source.calls.clone();
        let (_provider, mut panel) = attach_panel(source);

        type_str(&mut panel, "@a.png");
        settle_attachments(&mut panel).await;
        assert_eq!(chips(&panel).len(), 1);
        let resolved_once = calls.load(Ordering::SeqCst);

        // Re-rendering, moving the caret, typing more text: none of it re-asks.
        panel.handle_key(key(KeyCode::Left));
        panel.handle_key(key(KeyCode::Right));
        type_str(&mut panel, " please");
        settle_attachments(&mut panel).await;
        assert_eq!(chips(&panel).len(), 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            resolved_once,
            "a settled path is not re-resolved on every keystroke"
        );

        for _ in 0.."@a.png please".len() {
            panel.handle_key(key(KeyCode::Backspace));
        }
        settle_attachments(&mut panel).await;
        assert!(
            panel.attachments.is_empty(),
            "the mention is gone, so the attachment is gone"
        );

        type_str(&mut panel, "@a.png");
        settle_attachments(&mut panel).await;
        assert_eq!(
            chips(&panel).len(),
            1,
            "and typing it again attaches it again — the memo was pruned with it"
        );
    }

    #[tokio::test]
    async fn ctrl_x_removes_the_chip_together_with_its_mention_text() {
        let source = Arc::new(FakeAttachments::default().with("a.png", Ok(png("a.png"))));
        let (_provider, mut panel) = attach_panel(source);

        type_str(&mut panel, "compare @a.png with the spec");
        settle_attachments(&mut panel).await;
        assert_eq!(chips(&panel).len(), 1);

        panel.handle_key(ctrl(KeyCode::Char('x')));
        settle_attachments(&mut panel).await;
        assert!(panel.attachments.is_empty());
        assert_eq!(
            panel.composer.text(),
            "compare with the spec",
            "the mention leaves with the chip, and takes one space with it"
        );

        panel.handle_key(ctrl(KeyCode::Char('x')));
        assert!(
            panel.attachments.is_empty() && panel.composer.text() == "compare with the spec",
            "removing with nothing attached is harmless"
        );
    }

    /// An ordinary source file is *supposed* to stay text. Reporting that as a
    /// failure would put a notice under every `@src/main.rs`.
    #[tokio::test]
    async fn a_mention_that_is_not_attachable_is_silent() {
        let source = Arc::new(
            FakeAttachments::default().with("src/main.rs", Err(AttachmentRejected::NotAttachable)),
        );
        let (provider, mut panel) = attach_panel(source);

        type_str(&mut panel, "read @src/main.rs");
        settle_attachments(&mut panel).await;
        assert!(panel.attachments.is_empty());
        assert_eq!(panel.attachment_notice, None);

        panel.handle_key(key(KeyCode::Enter));
        let sent = provider.receive().await.unwrap();
        assert!(
            !sent
                .metadata
                .contains_key(crate::commands::META_ATTACHMENTS),
            "a prompt with nothing attached carries no attachment key at all"
        );
    }

    #[tokio::test]
    async fn a_refused_file_explains_itself_and_keeps_the_draft() {
        let source = Arc::new(FakeAttachments::default().with(
            "huge.png",
            Err(AttachmentRejected::failed(
                "`huge.png` is 9.4 MB; attachments are capped at 3.6 MB",
            )),
        ));
        let (provider, mut panel) = attach_panel(source);

        type_str(&mut panel, "what is in @huge.png");
        settle_attachments(&mut panel).await;
        assert!(panel.attachments.is_empty());
        assert_eq!(
            panel.attachment_notice.as_deref(),
            Some("`huge.png` is 9.4 MB; attachments are capped at 3.6 MB")
        );
        assert_eq!(
            panel.composer.text(),
            "what is in @huge.png",
            "the draft survives the refusal — it is the user's to fix"
        );
        let banner = panel
            .banner_lines()
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(banner.contains("capped at 3.6 MB"), "{banner}");

        // The prompt still goes, without an attachment it never had.
        panel.handle_key(key(KeyCode::Enter));
        let sent = provider.receive().await.unwrap();
        assert!(attachment_handles(&sent).is_empty());
    }

    #[tokio::test]
    async fn a_clipboard_capture_attaches_without_touching_the_draft() {
        let source = Arc::new(FakeAttachments::default().clipboard(Ok(png("clipboard-1.png"))));
        let (provider, mut panel) = attach_panel(source);

        type_str(&mut panel, "what is this");
        panel.handle_key(ctrl(KeyCode::Char('v')));
        settle_attachments(&mut panel).await;
        assert_eq!(chips(&panel), ["clipboard-1.png · image/png · 4.0 KB"]);
        assert_eq!(
            panel.composer.text(),
            "what is this",
            "a clipboard capture has no path, so it writes nothing into the draft"
        );

        panel.handle_key(key(KeyCode::Enter));
        let sent = provider.receive().await.unwrap();
        assert_eq!(attachment_handles(&sent), ["h-clipboard-1.png"]);
        // Nothing in the prompt text says an image went, so the echo must.
        let echo = panel
            .message_lines()
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(echo.contains("1 attached"), "{echo}");
    }

    /// The honest degradation: a terminal that cannot produce a clipboard image
    /// says so and names the alternative, rather than appearing to work.
    #[tokio::test]
    async fn a_platform_without_clipboard_images_says_so_and_points_at_the_path_route() {
        let source = Arc::new(FakeAttachments::default().clipboard(Err(
            AttachmentRejected::failed(
                "clipboard images are unavailable here; attach by path with `@file`",
            ),
        )));
        let (_provider, mut panel) = attach_panel(source);

        panel.handle_key(ctrl(KeyCode::Char('v')));
        settle_attachments(&mut panel).await;
        assert!(panel.attachments.is_empty());
        assert_eq!(
            panel.attachment_notice.as_deref(),
            Some("clipboard images are unavailable here; attach by path with `@file`")
        );
    }

    /// Typing a path and pressing Enter in one motion must not outrun the host.
    #[tokio::test]
    async fn enter_will_not_race_a_resolve_that_is_still_in_flight() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let source = Arc::new(
            FakeAttachments::default()
                .with("slow.png", Ok(png("slow.png")))
                .gated(gate.clone()),
        );
        let (provider, mut panel) = attach_panel(source);

        type_str(&mut panel, "@slow.png");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(
            panel.composer.text(),
            "@slow.png",
            "the draft is held, not sent half-resolved"
        );
        assert!(
            panel
                .attachment_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("still checking")),
            "and the user is told why: {:?}",
            panel.attachment_notice
        );

        gate.add_permits(1);
        settle_attachments(&mut panel).await;
        panel.handle_key(key(KeyCode::Enter));
        let sent = provider.receive().await.unwrap();
        assert_eq!(attachment_handles(&sent), ["h-slow.png"]);
    }

    #[tokio::test]
    async fn a_host_that_supplies_no_source_is_unchanged() {
        let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
        type_str(&mut panel, "look at @diagram.png");
        settle_attachments(&mut panel).await;
        assert!(panel.attachments.is_empty());
        assert!(panel.banner_lines().is_empty());

        panel.handle_key(ctrl(KeyCode::Char('v')));
        assert_eq!(
            panel.attachment_notice.as_deref(),
            Some("Attachments are not configured by this host"),
            "Ctrl-V says the host does not do this rather than doing nothing"
        );
        panel.attachment_notice = None;

        panel.handle_key(key(KeyCode::Enter));
        let sent = provider.receive().await.unwrap();
        assert!(
            !sent
                .metadata
                .contains_key(crate::commands::META_ATTACHMENTS)
        );
    }

    /// Handles name bytes the outgoing session's host is holding. Carrying them
    /// across would stamp a prompt with references the new host never minted.
    #[tokio::test]
    async fn chips_do_not_survive_a_session_boundary() {
        let source = Arc::new(FakeAttachments::default().with("a.png", Ok(png("a.png"))));
        let (_provider, mut panel) = attach_panel(source);
        type_str(&mut panel, "@a.png");
        settle_attachments(&mut panel).await;
        assert_eq!(chips(&panel).len(), 1);

        panel.clear_session_state();
        assert!(panel.attachments.is_empty());
        assert_eq!(panel.attachment_notice, None);
    }

    #[tokio::test]
    async fn the_chip_row_renders_above_the_input() {
        let source = Arc::new(FakeAttachments::default().with("a.png", Ok(png("a.png"))));
        let (_provider, mut panel) = attach_panel(source);
        type_str(&mut panel, "@a.png");
        settle_attachments(&mut panel).await;

        let (rows, _) = draw(&mut panel, 64, 10);
        let rendered = rows.join("\n");
        assert!(
            rendered.contains("attached") && rendered.contains("a.png"),
            "the chip row is on screen: {rendered}"
        );
        let chip_row = rows
            .iter()
            .position(|row| row.contains("attached"))
            .expect("chip row");
        let input_row = rows
            .iter()
            .position(|row| row.contains('›'))
            .expect("input border");
        assert!(
            chip_row < input_row,
            "chips sit directly above the draft they belong to: {rendered}"
        );
    }

    /// A queued or steering submission carries its chips too — that path, not
    /// the bare channel, is the one an interactive host actually uses.
    #[tokio::test]
    async fn a_queued_submission_carries_its_attachments() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<(String, Vec<String>)>>);
        impl TurnInputHandle for Recorder {
            fn submit(
                &self,
                text: String,
                mode: TurnInputMode,
            ) -> Result<TurnInputReceipt, String> {
                self.submit_with_attachments(text, mode, Vec::new())
            }
            fn submit_with_attachments(
                &self,
                text: String,
                mode: TurnInputMode,
                attachments: Vec<String>,
            ) -> Result<TurnInputReceipt, String> {
                let mut seen = self.0.lock();
                seen.push((text, attachments));
                Ok(TurnInputReceipt {
                    id: seen.len() as u64,
                    target_run: 1,
                    mode,
                })
            }
            fn replace(&self, _id: u64, _text: String) -> Result<(), String> {
                Ok(())
            }
            fn remove(&self, _id: u64) -> Result<(), String> {
                Ok(())
            }
        }

        let source = Arc::new(FakeAttachments::default().with("a.png", Ok(png("a.png"))));
        let recorder = Arc::new(Recorder::default());
        let (_provider, mut panel) = attach_panel(source);
        panel.turn_input = Some(recorder.clone());

        type_str(&mut panel, "check @a.png");
        settle_attachments(&mut panel).await;
        panel.handle_key(key(KeyCode::Enter));

        let seen = recorder.0.lock().clone();
        assert_eq!(
            seen,
            [("check @a.png".to_string(), vec!["h-a.png".to_string()])]
        );
    }

    /// The default body exists so hosts that do not use attachments need not
    /// implement it. Such a host supplies no source, so the list is always
    /// empty — the default is never asked to drop anything.
    #[test]
    fn the_defaulted_submit_forwards_to_the_plain_one() {
        struct Legacy(Mutex<Vec<String>>);
        impl TurnInputHandle for Legacy {
            fn submit(
                &self,
                text: String,
                mode: TurnInputMode,
            ) -> Result<TurnInputReceipt, String> {
                self.0.lock().push(text);
                Ok(TurnInputReceipt {
                    id: 0,
                    target_run: 1,
                    mode,
                })
            }
            fn replace(&self, _id: u64, _text: String) -> Result<(), String> {
                Ok(())
            }
            fn remove(&self, _id: u64) -> Result<(), String> {
                Ok(())
            }
        }
        let legacy = Legacy(Mutex::new(Vec::new()));
        legacy
            .submit_with_attachments("hi".into(), TurnInputMode::Queue, Vec::new())
            .unwrap();
        assert_eq!(*legacy.0.lock(), ["hi"]);
    }
}
