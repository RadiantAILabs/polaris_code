//! Host-supplied commands and completion for the conversation composer.
//!
//! The TUI ships **no command set of its own**. A host advertises the slash
//! commands it executes, supplies a source for `@` file-mention candidates,
//! and executes what the panel dispatches. This module holds everything both
//! sides share so a host and the panel agree without either owning the other's
//! policy:
//!
//! - the **wire protocol** on the existing [`UserIO`](polaris::plugins::UserIO)
//!   channel — an advertisement the host sends once, and the dispatch message
//!   the panel sends back when the user submits a `/command`;
//! - the [`CompletionSource`] contract a host implements for `@` mentions,
//!   with a [`CancelToken`] so an abandoned query stops walking the disk;
//! - the **mention syntax** (`@path`, spaces backslash-escaped) with the
//!   scanner both the panel and a host use to find mentions in text;
//! - the [`AttachmentSource`] contract a host implements to turn a mentioned
//!   path or the clipboard into a composer chip, with the
//!   [`META_ATTACHMENTS`] wire key the submitted prompt carries (see
//!   [`attach`](self#attachments)); and
//! - [`CompletionPopup`], the popup's state machine — trigger detection,
//!   filtering, selection, and what accepting an entry does to the draft —
//!   kept free of any panel or rendering type so it is testable on its own.
//!
//! # Advertising commands
//!
//! A host sends one `status` message carrying a [`META_COMMANDS`] entry, built
//! by [`advertise_commands`]. The panel caches the list for the `/` popup.
//! Until an advertisement arrives the panel treats a leading `/` as ordinary
//! text, so a consumer that never advertises is unaffected.
//!
//! ```
//! use polaris_tui::commands::{CommandSpec, advertise_commands, parse_advertisement};
//!
//! let specs = vec![CommandSpec::new("help", "List the available commands")];
//! let message = advertise_commands(&specs);
//! assert_eq!(parse_advertisement(&message).unwrap(), specs);
//! ```
//!
//! # Dispatching a command
//!
//! On `Enter`, a draft that starts with `/` becomes a user message stamped
//! `io_type = "command"` with the command [`name`](META_COMMAND_NAME) and its
//! [`args`](META_COMMAND_ARGS). The host executes it and replies with an
//! ordinary message; **an unknown command is the host's to report**, since only
//! the host knows its set. Nothing about a command reaches the model unless
//! the host chooses to forward it.
//!
//! ```
//! use polaris_tui::commands::CommandInvocation;
//!
//! let message = CommandInvocation::new("help", "").into_message("/help");
//! let parsed = CommandInvocation::from_message(&message).unwrap();
//! assert_eq!(parsed.name, "help");
//! ```
//!
//! # Reporting session status
//!
//! The same `status` message carries the host's [session status
//! report](status) — model, workspace, session, approval posture, projected
//! context window, spend, and whether the host can still process work — under
//! its own metadata key, so the panel's meta row is host-supplied too.
//!
//! # Attachments
//!
//! A host that implements [`AttachmentSource`] can turn a mentioned path — or
//! the clipboard — into a removable composer chip. The panel renders the chip
//! and stamps the handles onto the prompt; the host owns every policy question
//! behind it, including whether the active model accepts images at all. See
//! [`Attachment`] for the origins a chip can have.
//!
//! ```
//! use polaris::plugins::IOMessage;
//! use polaris_tui::commands::{Attachment, attachment_handles, stamp_attachments};
//!
//! let chips = [Attachment::new("h-1", "diagram.png", "image/png", 4_096)];
//! let prompt = stamp_attachments(IOMessage::user_text("what is this?"), &chips);
//! assert_eq!(attachment_handles(&prompt), ["h-1"]);
//! ```

mod attach;
mod rewind;
mod status;
pub use attach::{
    Attachment, AttachmentFuture, AttachmentRejected, AttachmentRequest, AttachmentSource,
    META_ATTACHMENTS, attachment_handles, stamp_attachments,
};
pub use rewind::{
    IO_TYPE_REWIND, META_REWIND_REPORT, RewindAxis, RewindReport, parse_rewind_report,
};
pub use status::{
    Availability, ContextWindow, Cost, META_STATUS_REPORT, StatusReport, parse_status,
    report_status, report_unavailable,
};

use polaris::plugins::{IO_TYPE, IOMessage};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// `io_type` value of a panel → host command dispatch.
pub const IO_TYPE_COMMAND: &str = "command";
/// Metadata key carrying the command name (without its `/`) on a dispatch.
pub const META_COMMAND_NAME: &str = "name";
/// Metadata key carrying the rest of the line after the command name.
pub const META_COMMAND_ARGS: &str = "args";
/// Metadata key on a `status` message carrying the host's command list as a
/// JSON array of `{"name", "description", "usage"}` objects.
pub const META_COMMANDS: &str = "commands";
/// `io_type` value for host-owned session presentation events.
pub const IO_TYPE_SESSION: &str = "session";
/// Metadata key naming the session presentation action.
pub const META_SESSION_ACTION: &str = "action";
/// Metadata key carrying the active session identity.
pub const META_SESSION_ID: &str = "session_id";
/// Start of a canonical transcript replay. The panel clears the old view.
pub const SESSION_REPLAY_START: &str = "replay-start";
/// End of a canonical transcript replay.
pub const SESSION_REPLAY_END: &str = "replay-end";
/// `io_type` value the advertisement rides on. The panel's own status vocabulary.
const TYPE_STATUS: &str = "status";

/// The character that opens the command popup at the start of a draft.
pub const COMMAND_TRIGGER: char = '/';
/// The character that opens the file-mention popup.
pub const MENTION_TRIGGER: char = '@';

/// Most rows the popup lists. Enough to scan; few enough to never eat the
/// transcript.
pub const POPUP_MAX_ROWS: usize = 8;

/// One command a host executes, as it is shown in the `/` popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Name typed after `/`. Lower-case, no whitespace.
    pub name: String,
    /// One-line description shown beside the name.
    pub description: String,
    /// Optional argument hint (`<model>`), shown after the name.
    pub usage: Option<String>,
    /// Host-owned values offered after the command name.
    pub choices: Vec<String>,
}

impl CommandSpec {
    /// A command with a name and description.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            usage: None,
            choices: Vec::new(),
        }
    }

    /// Attach an argument hint.
    #[must_use]
    pub fn with_usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = Some(usage.into());
        self
    }

    /// Offer a finite set of host-owned argument values.
    #[must_use]
    pub fn with_choices(mut self, choices: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.choices = choices.into_iter().map(Into::into).collect();
        self
    }
}

/// Build the `status` message a host sends to advertise its command set.
///
/// The text is empty, so a panel that treats it as an activity update sees
/// "idle" — but the conversation panel recognises the [`META_COMMANDS`] key
/// and consumes the message as an advertisement only, leaving the activity
/// line alone.
#[must_use]
pub fn advertise_commands(specs: &[CommandSpec]) -> IOMessage {
    let list: Vec<serde_json::Value> = specs
        .iter()
        .map(|spec| {
            serde_json::json!({
                "name": spec.name,
                "description": spec.description,
                "usage": spec.usage,
                "choices": spec.choices,
            })
        })
        .collect();
    IOMessage::system_text("")
        .with_metadata(IO_TYPE, TYPE_STATUS)
        .with_metadata(META_COMMANDS, serde_json::Value::Array(list).to_string())
}

/// Tell the conversation panel to replace its visible session with a replay.
///
/// The host sends this before the canonical messages. It is presentation-only:
/// the event never enters the agent input channel and runs no graph or tool.
#[must_use]
pub fn session_replay_start(id: &str) -> IOMessage {
    IOMessage::system_text(format!("Session `{id}`"))
        .with_metadata(IO_TYPE, IO_TYPE_SESSION)
        .with_metadata(META_SESSION_ACTION, SESSION_REPLAY_START)
        .with_metadata(META_SESSION_ID, id)
}

/// Mark a canonical replay complete.
///
/// `historical_usage` is deliberately explicit: current-process counters are
/// reset instead of being presented as measurements of the restored history.
#[must_use]
pub fn session_replay_end(id: &str) -> IOMessage {
    IOMessage::system_text(
        "Canonical conversation restored. Historical token usage was not measured in this process.",
    )
    .with_metadata(IO_TYPE, IO_TYPE_SESSION)
    .with_metadata(META_SESSION_ACTION, SESSION_REPLAY_END)
    .with_metadata(META_SESSION_ID, id)
}

/// Read a command advertisement, if `message` is one.
///
/// Returns `None` for a message without the [`META_COMMANDS`] key, and an
/// empty list for one whose payload does not parse — a host that sends garbage
/// has advertised nothing, and the panel says so rather than guessing.
#[must_use]
pub fn parse_advertisement(message: &IOMessage) -> Option<Vec<CommandSpec>> {
    let raw = message.metadata.get(META_COMMANDS)?;
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some(Vec::new());
    };
    Some(
        items
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.trim();
                if name.is_empty() || name.chars().any(char::is_whitespace) {
                    return None;
                }
                let description = item
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let usage = item
                    .get("usage")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                let choices = item
                    .get("choices")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Some(CommandSpec {
                    name: name.to_string(),
                    description: description.to_string(),
                    usage,
                    choices,
                })
            })
            .collect(),
    )
}

/// A `/command` the user submitted, as the panel dispatches it to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    /// The command name without its leading `/`.
    pub name: String,
    /// Everything after the name, trimmed. Empty when there was nothing.
    pub args: String,
}

impl CommandInvocation {
    /// An invocation of `name` with `args`.
    #[must_use]
    pub fn new(name: impl Into<String>, args: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args: args.into(),
        }
    }

    /// Split a submitted draft into an invocation.
    ///
    /// Returns `None` unless the text starts with `/` followed by a name, so
    /// `/` alone or `/ x` stay ordinary prompts. The name is what follows the
    /// slash up to the first whitespace; the args are the trimmed remainder,
    /// newlines included.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(COMMAND_TRIGGER)?;
        let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let name = &rest[..name_end];
        if name.is_empty() {
            return None;
        }
        Some(Self::new(name, rest[name_end..].trim()))
    }

    /// The dispatch message for this invocation. `raw` is the line as typed,
    /// kept as the body so a log of the channel reads naturally.
    #[must_use]
    pub fn into_message(self, raw: &str) -> IOMessage {
        IOMessage::user_text(raw)
            .with_metadata(IO_TYPE, IO_TYPE_COMMAND)
            .with_metadata(META_COMMAND_NAME, self.name)
            .with_metadata(META_COMMAND_ARGS, self.args)
    }

    /// Read a dispatch message, if `message` is one.
    #[must_use]
    pub fn from_message(message: &IOMessage) -> Option<Self> {
        if message.metadata.get(IO_TYPE).map(String::as_str) != Some(IO_TYPE_COMMAND) {
            return None;
        }
        let name = message.metadata.get(META_COMMAND_NAME)?.trim();
        if name.is_empty() {
            return None;
        }
        let args = message
            .metadata
            .get(META_COMMAND_ARGS)
            .map_or("", String::as_str);
        Some(Self::new(name, args))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Completion source
// ─────────────────────────────────────────────────────────────────────────────

/// Cooperative cancellation for an in-flight completion query.
///
/// The panel cancels the previous query on every keystroke, so a source that
/// walks a directory tree should check [`is_cancelled`](Self::is_cancelled)
/// as it goes and stop early rather than finish work nobody will read.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A live token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the query abandoned.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the query has been abandoned.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// One candidate the popup can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Text inserted into the draft when accepted, *without* the trigger
    /// character. For a file mention this is the path; the panel escapes it.
    pub insert: String,
    /// Text shown in the popup row. Usually the same as `insert`.
    pub label: String,
    /// Dimmed detail after the label (`dir`, a description).
    pub detail: String,
}

impl Completion {
    /// A candidate whose label is its insert text.
    #[must_use]
    pub fn new(insert: impl Into<String>) -> Self {
        let insert = insert.into();
        Self {
            label: insert.clone(),
            insert,
            detail: String::new(),
        }
    }

    /// Attach a dimmed detail.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// Boxed future returned by [`CompletionSource::complete`].
pub type CompletionFuture = Pin<Box<dyn Future<Output = Vec<Completion>> + Send + 'static>>;

/// A host-supplied source of `@` mention candidates.
///
/// The panel owns presentation — the popup, its keys, what accepting does to
/// the draft — and nothing else: which paths exist, which are ignored, and how
/// they rank is the host's policy. `query` is the text typed after `@` with
/// escapes removed; the result is the ranked candidates for it, best first,
/// already capped to what is worth showing.
pub trait CompletionSource: Send + Sync + 'static {
    /// Rank candidates for `query`. Honour `cancel` in long walks.
    fn complete(&self, query: String, cancel: CancelToken) -> CompletionFuture;
}

// ─────────────────────────────────────────────────────────────────────────────
// Mention syntax
// ─────────────────────────────────────────────────────────────────────────────

/// A `@path` mention found in text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// Byte offset of the `@`.
    pub start: usize,
    /// Byte offset one past the mention's last byte.
    pub end: usize,
    /// The path with escapes removed.
    pub path: String,
}

/// Escape a path for insertion after `@`: a space becomes `\ ` and a
/// backslash `\\`, so the mention survives as one token.
#[must_use]
pub fn escape_mention(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            ' ' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

/// Scan `text` for `@` mentions.
///
/// A mention starts with `@` at the beginning of the text or after
/// whitespace, so `me@example.com` is not one. It runs until unescaped
/// whitespace; `\ ` is a literal space and `\\` a literal backslash. A bare
/// `@` with nothing after it is not a mention.
#[must_use]
pub fn parse_mentions(text: &str) -> Vec<Mention> {
    let mut mentions = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let Some(offset) = text[at..].find(MENTION_TRIGGER) else {
            break;
        };
        let start = at + offset;
        let preceded_by_boundary = start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if !preceded_by_boundary {
            at = start + MENTION_TRIGGER.len_utf8();
            continue;
        }
        let (path, end) = read_mention_token(text, start + MENTION_TRIGGER.len_utf8());
        if !path.is_empty() {
            mentions.push(Mention { start, end, path });
        }
        at = end.max(start + MENTION_TRIGGER.len_utf8());
    }
    mentions
}

/// Read a mention token starting at `from`: returns the unescaped path and the
/// byte offset where the token ends.
fn read_mention_token(text: &str, from: usize) -> (String, usize) {
    let mut path = String::new();
    let mut chars = text[from..].char_indices().peekable();
    let mut end = from;
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some((next_offset, escaped)) => {
                    path.push(escaped);
                    end = from + next_offset + escaped.len_utf8();
                }
                None => {
                    // A trailing backslash is literal — there is nothing to
                    // escape.
                    path.push('\\');
                    end = from + offset + 1;
                }
            },
            c if c.is_whitespace() => break,
            c => {
                path.push(c);
                end = from + offset + c.len_utf8();
            }
        }
    }
    (path, end)
}

// ─────────────────────────────────────────────────────────────────────────────
// Popup state machine
// ─────────────────────────────────────────────────────────────────────────────

/// Which trigger a popup belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// `/` at the start of the draft: host commands.
    Command,
    /// A finite argument advertised by a host command.
    CommandArgument,
    /// `@` at a word boundary: host file mentions.
    Mention,
}

/// The token under the caret that a popup completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveToken {
    /// Which popup this token opens.
    pub trigger: Trigger,
    /// Byte offset of the trigger character.
    pub anchor: usize,
    /// Byte offset one past the token, i.e. the caret.
    pub end: usize,
    /// The text between the trigger and the caret, unescaped.
    pub query: String,
}

/// Find the completion token the caret sits in, if any.
///
/// A command token is a draft that starts with `/` while the caret is still
/// inside the command name — no whitespace between the slash and the caret.
/// A mention token is the nearest `@` before the caret that sits at a word
/// boundary and has no unescaped whitespace between it and the caret. Once
/// the caret leaves the token (a space, a move away) there is nothing to
/// complete and the popup closes.
#[must_use]
pub fn active_token(text: &str, cursor: usize) -> Option<ActiveToken> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    if let Some(name) = before.strip_prefix(COMMAND_TRIGGER)
        && !name.chars().any(char::is_whitespace)
    {
        return Some(ActiveToken {
            trigger: Trigger::Command,
            anchor: 0,
            end: cursor,
            query: name.to_string(),
        });
    }
    // Walk mentions in the text before the caret: the one that reaches the
    // caret is the one being typed. `parse_mentions` skips a bare `@`, so
    // check that case separately — it is exactly when the popup should open
    // with an empty query.
    if let Some(mention) = parse_mentions(before)
        .into_iter()
        .find(|mention| mention.end == cursor)
    {
        return Some(ActiveToken {
            trigger: Trigger::Mention,
            anchor: mention.start,
            end: cursor,
            query: mention.path,
        });
    }
    if before.ends_with(MENTION_TRIGGER) {
        let anchor = cursor - MENTION_TRIGGER.len_utf8();
        let at_boundary = anchor == 0
            || text[..anchor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if at_boundary {
            return Some(ActiveToken {
                trigger: Trigger::Mention,
                anchor,
                end: cursor,
                query: String::new(),
            });
        }
    }
    None
}

/// Find an advertised command argument under the caret.
#[must_use]
pub fn active_command_argument_token(
    text: &str,
    cursor: usize,
    commands: &[CommandSpec],
) -> Option<(ActiveToken, Vec<String>)> {
    let cursor = cursor.min(text.len());
    let before = text.get(..cursor)?;
    let invocation = CommandInvocation::parse(before)?;
    let spec = commands
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(&invocation.name))?;
    if spec.choices.is_empty() {
        return None;
    }
    let name_end = before.find(char::is_whitespace)?;
    let anchor = before[name_end..]
        .find(|ch: char| !ch.is_whitespace())
        .map_or(cursor, |offset| name_end + offset);
    let query = before.get(anchor..)?.to_string();
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((
        ActiveToken {
            trigger: Trigger::CommandArgument,
            anchor,
            end: cursor,
            query,
        },
        spec.choices.clone(),
    ))
}

/// What the popup shows in place of rows when it has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupNotice {
    /// A mention query is still running.
    Searching,
    /// The query matched nothing the host offers.
    NoMatches,
}

/// The open popup: its token, rows, and selection.
///
/// Pure state — the panel feeds it the draft and keystrokes and reads back
/// what to draw and what accepting does. It never touches a composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionPopup {
    /// The token being completed.
    pub token: ActiveToken,
    /// Rows to show, best first.
    pub items: Vec<Completion>,
    /// Index into `items` of the highlighted row.
    pub selected: usize,
    /// Whether a mention query is in flight for the current token.
    pub loading: bool,
}

impl CompletionPopup {
    /// Open a popup for `token` with no rows yet.
    #[must_use]
    pub fn new(token: ActiveToken) -> Self {
        Self {
            token,
            items: Vec::new(),
            selected: 0,
            loading: false,
        }
    }

    /// Replace the rows, keeping the selection in range.
    pub fn set_items(&mut self, items: Vec<Completion>) {
        self.items = items;
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.loading = false;
    }

    /// Rank `commands` against the token's query. Prefix matches lead,
    /// substring matches follow, both case-insensitively; an empty query
    /// lists everything.
    pub fn filter_commands(&mut self, commands: &[CommandSpec]) {
        let query = self.token.query.to_lowercase();
        let mut prefix = Vec::new();
        let mut infix = Vec::new();
        for spec in commands {
            let name = spec.name.to_lowercase();
            let label = match &spec.usage {
                Some(usage) => format!("/{} {usage}", spec.name),
                None => format!("/{}", spec.name),
            };
            let item = Completion {
                insert: spec.name.clone(),
                label,
                detail: spec.description.clone(),
            };
            if name.starts_with(&query) {
                prefix.push(item);
            } else if name.contains(&query) {
                infix.push(item);
            }
        }
        prefix.extend(infix);
        self.set_items(prefix);
    }

    /// Rank finite command argument choices against the current query.
    pub fn filter_choices(&mut self, choices: &[String]) {
        let query = self.token.query.to_lowercase();
        let mut prefix = Vec::new();
        let mut infix = Vec::new();
        for choice in choices {
            let item = Completion::new(choice);
            let lower = choice.to_lowercase();
            if lower.starts_with(&query) {
                prefix.push(item);
            } else if lower.contains(&query) {
                infix.push(item);
            }
        }
        prefix.extend(infix);
        self.set_items(prefix);
    }

    /// The highlighted candidate, if there is one.
    #[must_use]
    pub fn selected_item(&self) -> Option<&Completion> {
        self.items.get(self.selected)
    }

    /// What to say when there are no rows.
    #[must_use]
    pub fn notice(&self) -> Option<PopupNotice> {
        if !self.items.is_empty() {
            None
        } else if self.loading {
            Some(PopupNotice::Searching)
        } else {
            Some(PopupNotice::NoMatches)
        }
    }

    /// Move the highlight down, wrapping.
    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    /// Move the highlight up, wrapping.
    pub fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }

    /// Whether the query already spells the highlighted candidate exactly.
    ///
    /// This is what lets `Enter` on a fully typed `/help` run it rather than
    /// re-insert it: accepting would change nothing, so the key falls through
    /// to submission.
    #[must_use]
    pub fn query_is_exact(&self) -> bool {
        self.selected_item()
            .is_some_and(|item| item.insert == self.token.query)
    }

    /// The edit accepting the highlighted candidate makes: replace
    /// `[anchor, end)` of the draft with the returned text, then place the
    /// caret at its end. The text ends in a space so the token is closed and
    /// the popup does not reopen on the same anchor.
    #[must_use]
    pub fn acceptance(&self) -> Option<Acceptance> {
        let item = self.selected_item()?;
        let insert = match self.token.trigger {
            Trigger::Command => format!("{COMMAND_TRIGGER}{} ", item.insert),
            Trigger::CommandArgument => item.insert.clone(),
            Trigger::Mention => format!("{MENTION_TRIGGER}{} ", escape_mention(&item.insert)),
        };
        Some(Acceptance {
            from: self.token.anchor,
            to: self.token.end,
            insert,
        })
    }
}

/// The draft edit that accepts a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    /// Start of the range to replace (the trigger character).
    pub from: usize,
    /// End of the range to replace (the caret).
    pub to: usize,
    /// Replacement text, trigger included and space-terminated.
    pub insert: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::plugins::IOContent;

    fn specs() -> Vec<CommandSpec> {
        vec![
            CommandSpec::new("help", "List commands"),
            CommandSpec::new("model", "Pick a model").with_usage("<id>"),
            CommandSpec::new("sessions", "Browse sessions"),
        ]
    }

    #[test]
    fn advertisement_round_trips_and_drops_malformed_entries() {
        let message = advertise_commands(&specs());
        assert_eq!(message.metadata.get(IO_TYPE).unwrap(), "status");
        assert!(matches!(&message.content, IOContent::Text(t) if t.is_empty()));
        assert_eq!(parse_advertisement(&message).unwrap(), specs());

        let junk = IOMessage::system_text("")
            .with_metadata(IO_TYPE, "status")
            .with_metadata(META_COMMANDS, "not json");
        assert_eq!(parse_advertisement(&junk), Some(Vec::new()));

        let partial = IOMessage::system_text("").with_metadata(
            META_COMMANDS,
            r#"[{"name":"ok"},{"name":"has space"},{"description":"nameless"},{"name":""}]"#,
        );
        let parsed = parse_advertisement(&partial).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "ok");

        assert!(parse_advertisement(&IOMessage::system_text("plain")).is_none());
    }

    #[test]
    fn invocation_parses_name_and_args() {
        assert_eq!(
            CommandInvocation::parse("/model  opus  "),
            Some(CommandInvocation::new("model", "opus"))
        );
        assert_eq!(
            CommandInvocation::parse("/help"),
            Some(CommandInvocation::new("help", ""))
        );
        assert_eq!(
            CommandInvocation::parse("/note first\nsecond"),
            Some(CommandInvocation::new("note", "first\nsecond"))
        );
        assert_eq!(CommandInvocation::parse("/"), None);
        assert_eq!(CommandInvocation::parse("/ x"), None);
        assert_eq!(CommandInvocation::parse("hello /x"), None);
    }

    #[test]
    fn invocation_round_trips_through_a_message() {
        let message = CommandInvocation::new("model", "opus").into_message("/model opus");
        assert_eq!(message.metadata.get(IO_TYPE).unwrap(), IO_TYPE_COMMAND);
        assert!(matches!(&message.content, IOContent::Text(t) if t == "/model opus"));
        assert_eq!(
            CommandInvocation::from_message(&message),
            Some(CommandInvocation::new("model", "opus"))
        );
        assert!(CommandInvocation::from_message(&IOMessage::user_text("/model")).is_none());
        let nameless = IOMessage::user_text("").with_metadata(IO_TYPE, IO_TYPE_COMMAND);
        assert!(CommandInvocation::from_message(&nameless).is_none());
    }

    #[test]
    fn mentions_are_found_at_word_boundaries_only() {
        let text = "see @src/main.rs and me@example.com plus @docs/read\\ me.md @";
        let found = parse_mentions(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, "src/main.rs");
        assert_eq!(&text[found[0].start..found[0].end], "@src/main.rs");
        assert_eq!(found[1].path, "docs/read me.md");
        assert_eq!(&text[found[1].start..found[1].end], "@docs/read\\ me.md");
    }

    #[test]
    fn mention_escaping_round_trips_spaces_backslashes_and_unicode() {
        for path in [
            "a b/c d.txt",
            "back\\slash",
            "日本語/ファイル.rs",
            "émoji 🚀.md",
        ] {
            let text = format!("@{}", escape_mention(path));
            let found = parse_mentions(&text);
            assert_eq!(found.len(), 1, "{text}");
            assert_eq!(found[0].path, path);
            assert_eq!(found[0].end, text.len());
        }
    }

    #[test]
    fn active_token_tracks_the_caret() {
        let t = active_token("/he", 3).unwrap();
        assert_eq!(t.trigger, Trigger::Command);
        assert_eq!((t.anchor, t.end, t.query.as_str()), (0, 3, "he"));
        // A space closes the command token.
        assert!(active_token("/help ", 6).is_none());
        // The caret must be inside the name.
        assert!(active_token("/help x", 7).is_none());
        // A slash not at column 0 is text.
        assert!(active_token("a /he", 5).is_none());

        let t = active_token("look at @src/ma", 15).unwrap();
        assert_eq!(t.trigger, Trigger::Mention);
        assert_eq!((t.anchor, t.query.as_str()), (8, "src/ma"));
        // A bare `@` opens with an empty query.
        let t = active_token("look at @", 9).unwrap();
        assert_eq!((t.trigger, t.query.as_str()), (Trigger::Mention, ""));
        // Not inside an email address.
        assert!(active_token("me@ex", 5).is_none());
        // The caret moved off the token.
        assert!(active_token("@src/main.rs done", 17).is_none());
        // Escaped spaces keep the token open.
        let t = active_token("@my\\ fi", 7).unwrap();
        assert_eq!(t.query, "my fi");
        // A caret inside a multibyte glyph never panics.
        assert!(active_token("@日", 2).is_none());
    }

    #[test]
    fn command_filtering_ranks_prefix_before_infix() {
        let mut popup = CompletionPopup::new(active_token("/s", 2).unwrap());
        popup.filter_commands(&specs());
        let names: Vec<&str> = popup.items.iter().map(|i| i.insert.as_str()).collect();
        assert_eq!(names, ["sessions"]);

        let mut popup = CompletionPopup::new(active_token("/e", 2).unwrap());
        popup.filter_commands(&specs());
        let names: Vec<&str> = popup.items.iter().map(|i| i.insert.as_str()).collect();
        // No prefix match; every name containing `e`, in advertised order.
        assert_eq!(names, ["help", "model", "sessions"]);

        let mut popup = CompletionPopup::new(active_token("/", 1).unwrap());
        popup.filter_commands(&specs());
        assert_eq!(popup.items.len(), 3);
        assert_eq!(popup.items[1].label, "/model <id>");

        let mut popup = CompletionPopup::new(active_token("/zzz", 4).unwrap());
        popup.filter_commands(&specs());
        assert!(popup.items.is_empty());
        assert_eq!(popup.notice(), Some(PopupNotice::NoMatches));
    }

    #[test]
    fn selection_wraps_and_survives_item_replacement() {
        let mut popup = CompletionPopup::new(active_token("/", 1).unwrap());
        popup.filter_commands(&specs());
        popup.select_prev();
        assert_eq!(popup.selected, 2);
        popup.select_next();
        assert_eq!(popup.selected, 0);
        popup.select_next();
        popup.select_next();
        popup.set_items(vec![Completion::new("only")]);
        assert_eq!(popup.selected, 0);
        popup.set_items(Vec::new());
        popup.select_next();
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn acceptance_replaces_the_token_and_closes_it() {
        let mut popup = CompletionPopup::new(active_token("/he", 3).unwrap());
        popup.filter_commands(&specs());
        let edit = popup.acceptance().unwrap();
        assert_eq!((edit.from, edit.to, edit.insert.as_str()), (0, 3, "/help "));
        assert!(!popup.query_is_exact());

        let mut popup = CompletionPopup::new(active_token("/help", 5).unwrap());
        popup.filter_commands(&specs());
        assert!(popup.query_is_exact());

        let mut popup = CompletionPopup::new(active_token("see @re", 7).unwrap());
        popup.set_items(vec![Completion::new("read me.md")]);
        let edit = popup.acceptance().unwrap();
        assert_eq!((edit.from, edit.to), (4, 7));
        assert_eq!(edit.insert, "@read\\ me.md ");

        let popup = CompletionPopup::new(active_token("@", 1).unwrap());
        assert!(popup.acceptance().is_none());
        let mut loading = popup;
        loading.loading = true;
        assert_eq!(loading.notice(), Some(PopupNotice::Searching));
    }

    #[test]
    fn cancel_token_is_shared() {
        let token = CancelToken::new();
        let observer = token.clone();
        assert!(!observer.is_cancelled());
        token.cancel();
        assert!(observer.is_cancelled());
    }
}
