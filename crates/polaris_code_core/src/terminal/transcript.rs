//! The live-transcript vocabulary: streaming assistant text and structured
//! tool activity, expressed as [`IOMessage`] metadata on the ordinary
//! [`UserIO`] channel.
//!
//! No new transport and no upstream type. `polaris-tui` keys off `io_type`
//! exactly as it already does for `status` / `tokens` / `confirm`; this module
//! is the coding host's side of that contract, so the panel renders a block's
//! label, phase and detail without learning what any tool does.
//!
//! # `io_type = "message"` — one assistant block that grows in place
//!
//! | Key | Meaning |
//! |-----|---------|
//! | `id` | Stable identifier for the block. Messages sharing an `id` fold into one. |
//! | `seq` | Monotonic per `id`. A stale or repeated `seq` is dropped, so a resend cannot duplicate text. |
//! | `final` | `"true"` on the closing message. Absent means more may follow. |
//! | `outcome` | On the closing message only: why the block ended (see [`OUTCOME_COMPLETE`]). |
//!
//! A non-final body is a **delta** — the new text only, matching
//! `ContentBlockDelta` so the producer forwards without buffering. The closing
//! message carries the **authoritative full text**, which replaces whatever the
//! deltas accumulated; an empty closing body leaves the accumulation standing.
//! That is what reconciles a finished message without duplicating it, and it
//! does not depend on the deltas having arrived intact.
//!
//! A message with no `id` is a complete, standalone message — today's
//! behaviour, unchanged.
//!
//! # `io_type = "tool"` — one activity block per invocation
//!
//! | Key | Meaning |
//! |-----|---------|
//! | `id` | The tool invocation id. Later messages with the same `id` update the block in place. |
//! | `parent` | The `message` id that requested it, for nesting. Absent on a turn that produced no text. |
//! | `phase` | [`PHASE_PENDING`], [`PHASE_START`], [`PHASE_RUNNING`], [`PHASE_BACKGROUND`], [`PHASE_OK`], [`PHASE_DENIED`] or [`PHASE_ERROR`]. |
//! | `label` | Short human name (`edit_file`). |
//! | `elapsed_ms` | Wall time from `start` to a terminal phase. |
//!
//! Body is a one-line detail while running (the path, the command) and the
//! result once terminal.
//!
//! # Blocks that outlive their turn
//!
//! A `run_command` block streams: [`PHASE_RUNNING`] updates carry a coalesced
//! tail while the command runs, and if the user backgrounds it the block moves
//! to [`PHASE_BACKGROUND`] and stops updating until the task finishes or is
//! followed again. The closing update can therefore arrive long after the turn
//! that opened the block has ended, from
//! [`background`](super::background) rather than from `execute_tools` — which
//! is why [`tool_message`] exists as a free function: the identity is the
//! provider's `tool_call.id`, not something a live [`ToolActivity`] has to
//! still be holding.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use polaris::models::llm::{ContentBlockDelta, StreamEvent};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, UserIO};
use polaris::system::resource::LocalResource;

/// `io_type` value marking an assistant message block.
pub const IO_TYPE_MESSAGE: &str = "message";
/// `io_type` value marking a tool activity block.
pub const IO_TYPE_TOOL: &str = "tool";

/// Metadata key carrying a block's stable identity.
pub const META_ID: &str = "id";
/// Metadata key carrying a message delta's monotonic sequence number.
pub const META_SEQ: &str = "seq";
/// Metadata key marking the closing message of a block.
pub const META_FINAL: &str = "final";
/// Metadata key naming the `message` block a tool invocation belongs to.
pub const META_PARENT: &str = "parent";
/// Metadata key carrying a tool invocation's lifecycle phase.
pub const META_PHASE: &str = "phase";
/// Metadata key carrying a tool invocation's short human name.
pub const META_LABEL: &str = "label";
/// Metadata key carrying a tool invocation's wall-clock duration.
pub const META_ELAPSED_MS: &str = "elapsed_ms";
/// Metadata key carrying why a message block ended.
pub const META_OUTCOME: &str = "outcome";

/// [`META_FINAL`] value marking a block closed.
pub const FINAL_TRUE: &str = "true";

/// [`META_OUTCOME`]: the model finished the message normally.
pub const OUTCOME_COMPLETE: &str = "complete";
/// [`META_OUTCOME`]: the stream failed part-way; the partial text is retained.
pub const OUTCOME_ERROR: &str = "error";

/// [`META_PHASE`]: parked in [`UserIO::confirm`], awaiting the user's answer.
pub const PHASE_PENDING: &str = "pending";
/// [`META_PHASE`]: executing.
pub const PHASE_START: &str = "start";
/// [`META_PHASE`]: executing, with live output arriving under the same block.
pub const PHASE_RUNNING: &str = "running";
/// [`META_PHASE`]: still running, but no longer inside the turn that started it.
///
/// A host word, not a `polaris-tui` one: the panel has no arm for it and needs
/// none, because an unrecognised phase renders with the fallthrough glyph.
pub const PHASE_BACKGROUND: &str = "background";
/// [`META_PHASE`]: completed successfully.
pub const PHASE_OK: &str = "ok";
/// [`META_PHASE`]: the user, or policy, refused it.
pub const PHASE_DENIED: &str = "denied";
/// [`META_PHASE`]: it ran and failed.
pub const PHASE_ERROR: &str = "error";

/// Longest tool detail or result forwarded to the panel.
///
/// The panel bounds its own rendering and `Ctrl-O` expands it, but a tool that
/// returns a whole file should not be copied through the IO channel in full
/// either. What the model sees is unaffected — this bounds the *display* copy.
const MAX_TOOL_BODY: usize = 8 * 1024;

/// Process-wide source of minted message ids.
///
/// The provider reports its own id only at `MessageStop`, which is after every
/// delta it identifies, so the transcript cannot key deltas off it. The host
/// mints instead. This keeps identity policy in the consumer: `polaris-ai`
/// mints nothing, and a session record stays a faithful mirror of what the
/// provider actually asserted.
static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(0);

/// Truncate a display body on a char boundary, marking what was cut.
fn bounded(text: &str) -> String {
    if text.len() <= MAX_TOOL_BODY {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(MAX_TOOL_BODY);
    format!(
        "{}\n… {} more bytes (the model received the whole result)",
        &text[..end],
        text.len() - end
    )
}

/// A handle to the open message block, cheap to clone into a stream combinator.
///
/// The sequence counter and the opened flag are shared rather than owned
/// because the deltas are emitted from inside the provider's event stream,
/// which cannot borrow the [`LiveTranscript`] resource across its awaits. The
/// resource and the handle therefore observe the same counter, so the closing
/// message stamps a `seq` that really follows the last delta sent.
#[derive(Clone)]
pub struct MessageHandle {
    /// The block's minted identity, shared by every delta and by its tools.
    id: String,
    /// Next sequence number to stamp. Monotonic per `id`.
    seq: Arc<AtomicU64>,
    /// Whether any delta has actually been sent under this id.
    opened: Arc<AtomicBool>,
}

impl MessageHandle {
    /// The block's identity, for stamping a tool's `parent`.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether any text has been emitted under this block.
    #[must_use]
    pub fn opened(&self) -> bool {
        self.opened.load(Ordering::Relaxed)
    }

    /// Forward one text delta. Empty deltas are dropped rather than stamped,
    /// so a provider emitting keep-alive chunks cannot burn sequence numbers.
    pub async fn delta(&self, user_io: &UserIO, text: &str) {
        if text.is_empty() {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.opened.store(true, Ordering::Relaxed);
        let msg = IOMessage::from_agent("code", IOContent::Text(text.to_owned()))
            .with_metadata(IO_TYPE, IO_TYPE_MESSAGE)
            .with_metadata(META_ID, self.id.clone())
            .with_metadata(META_SEQ, seq.to_string());
        let _ = user_io.send(msg).await;
    }
}

/// Forward one provider stream event to the transcript.
///
/// Returns `true` when the event put text on screen, which makes the attempt
/// **committed**: text the user has already read is as unrollbackable as a
/// billed token, so a retry past this point could only duplicate it.
///
/// Reasoning and tool-argument fragments are deliberately not forwarded. The
/// first is the `Ctrl-O` deep view's material — the transcript stays the
/// task-oriented view — and the second is partial JSON that means nothing
/// until its block stops.
pub async fn forward(
    block: &MessageHandle,
    user_io: &UserIO,
    event: &Result<StreamEvent, impl std::error::Error>,
) -> bool {
    let Ok(StreamEvent::ContentBlockDelta {
        delta: ContentBlockDelta::Text(text),
        ..
    }) = event
    else {
        return false;
    };
    if text.is_empty() {
        return false;
    }
    block.delta(user_io, text).await;
    true
}

/// Per-session live-transcript state: the open message block, if any.
///
/// Inserted by [`CodeAgent::setup`](crate::agents::CodeAgent) and read as
/// `ResMut` by the systems that emit. It holds no text — the panel accumulates
/// deltas, and the closing message carries the authoritative version — so a
/// long turn does not grow a second copy of the answer here.
#[derive(Default)]
pub struct LiveTranscript {
    current: Option<MessageHandle>,
}

impl LocalResource for LiveTranscript {}

impl LiveTranscript {
    /// A handle to the open block, if one is open.
    #[must_use]
    pub fn current(&self) -> Option<&MessageHandle> {
        self.current.as_ref()
    }

    /// The open block's id, if a block is open.
    #[must_use]
    pub fn current_id(&self) -> Option<&str> {
        self.current.as_ref().map(MessageHandle::id)
    }

    /// Whether the open block has emitted any text.
    #[must_use]
    pub fn has_streamed(&self) -> bool {
        self.current.as_ref().is_some_and(MessageHandle::opened)
    }

    /// Mint a new block, closing nothing. Called at the start of an attempt.
    ///
    /// A fresh id per attempt is what makes a retry safe: the retried text
    /// lands in its own block rather than re-entering one the user has already
    /// seen, so no `seq` can collide across attempts.
    pub fn open(&mut self) -> MessageHandle {
        let n = NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
        let handle = MessageHandle {
            id: format!("m{n}"),
            seq: Arc::new(AtomicU64::new(0)),
            opened: Arc::new(AtomicBool::new(false)),
        };
        self.current = Some(handle.clone());
        handle
    }

    /// Close the open block.
    ///
    /// `text` is the authoritative full message and replaces whatever the
    /// deltas accumulated; pass `""` to keep the accumulation and merely mark
    /// the block finished, which is what a failed stream does with its partial
    /// text. Closing a block that never emitted anything sends nothing, so a
    /// pure tool-call turn leaves no empty block behind.
    pub async fn close(&mut self, user_io: &UserIO, text: &str, outcome: &str) {
        let Some(block) = self.current.take() else {
            return;
        };
        if !block.opened() && text.is_empty() {
            return;
        }
        let msg = IOMessage::from_agent("code", IOContent::Text(text.to_owned()))
            .with_metadata(IO_TYPE, IO_TYPE_MESSAGE)
            .with_metadata(META_ID, block.id)
            .with_metadata(
                META_SEQ,
                block.seq.fetch_add(1, Ordering::Relaxed).to_string(),
            )
            .with_metadata(META_FINAL, FINAL_TRUE)
            .with_metadata(META_OUTCOME, outcome.to_owned());
        let _ = user_io.send(msg).await;
    }

    /// Emit a whole message as one closed block, for a path that has no stream
    /// to forward (error recovery). Renders identically to a streamed block.
    pub async fn whole(&mut self, user_io: &UserIO, text: &str, outcome: &str) {
        let handle = self.open();
        handle.opened.store(true, Ordering::Relaxed);
        self.close(user_io, text, outcome).await;
    }
}

/// One tool invocation's activity block, from `start` to a terminal phase.
///
/// Holds the invocation's identity and start instant so the terminal message
/// can report elapsed time under the same `id` the `start` message used.
pub struct ToolActivity {
    id: String,
    parent: Option<String>,
    label: String,
    started: Instant,
}

impl ToolActivity {
    /// Announce a tool invocation, nested under `parent` when the turn produced
    /// text. `detail` is the one-line summary shown while it runs.
    pub async fn start(
        user_io: &UserIO,
        id: impl Into<String>,
        parent: Option<&str>,
        label: impl Into<String>,
        detail: &str,
    ) -> Self {
        let activity = Self {
            id: id.into(),
            parent: parent.map(str::to_owned),
            label: label.into(),
            started: Instant::now(),
        };
        activity.emit(user_io, PHASE_START, detail).await;
        activity
    }

    /// Move the block to `phase` while it is still running — used for the
    /// approval wait, which is time the user is spending, not the tool.
    pub async fn phase(&self, user_io: &UserIO, phase: &str, detail: &str) {
        self.emit(user_io, phase, detail).await;
    }

    /// Close the block with its terminal phase and result body.
    pub async fn finish(self, user_io: &UserIO, phase: &str, result: &str) {
        self.emit(user_io, phase, result).await;
    }

    async fn emit(&self, user_io: &UserIO, phase: &str, body: &str) {
        let msg = tool_message(
            &self.id,
            self.parent.as_deref(),
            &self.label,
            phase,
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            body,
        );
        let _ = user_io.send(msg).await;
    }
}

/// One update for the tool block `id`, ready to send on any host channel.
///
/// The block's identity is the provider's `tool_call.id`, so an update needs
/// nothing but that string — not the [`ToolActivity`] that opened it, and not
/// the turn it belonged to. That is what lets a backgrounded command close its
/// own block after its turn is over, and what keeps the metadata shape in one
/// place rather than two.
#[must_use]
pub fn tool_message(
    id: &str,
    parent: Option<&str>,
    label: &str,
    phase: &str,
    elapsed_ms: u64,
    body: &str,
) -> IOMessage {
    let mut msg = IOMessage::from_agent("code", IOContent::Text(bounded(body)))
        .with_metadata(IO_TYPE, IO_TYPE_TOOL)
        .with_metadata(META_ID, id.to_owned())
        .with_metadata(META_PHASE, phase.to_owned())
        .with_metadata(META_LABEL, label.to_owned())
        .with_metadata(META_ELAPSED_MS, elapsed_ms.to_string());
    if let Some(parent) = parent {
        msg = msg.with_metadata(META_PARENT, parent.to_owned());
    }
    msg
}

/// The one-line detail shown beside a running tool.
///
/// Deliberately the arguments the user cares about, not the whole JSON: a path
/// for a file tool, the command for a shell tool, a query for a search. Falls
/// back to the compact argument object so an unknown tool still says something.
///
/// Capped at [`DETAIL_CHARS`], because this line sits beside a spinner: a long
/// command or a deep path must not push the rest of the block off the row.
#[must_use]
pub fn tool_detail(tool_name: &str, args: &serde_json::Value) -> String {
    let field = |key: &str| args.get(key).and_then(serde_json::Value::as_str);
    let detail = match tool_name {
        "run_command" => field("command"),
        "search" => field("pattern").or_else(|| field("query")),
        // The graph and memory tools name their subject with none of the three
        // obvious keys, so an un-listed key here is the difference between a
        // readable line and a raw JSON object beside the spinner.
        _ => field("path")
            .or_else(|| field("symbol"))
            .or_else(|| field("symbol_name"))
            .or_else(|| field("name"))
            .or_else(|| field("trait_name"))
            .or_else(|| field("role"))
            .or_else(|| field("resource"))
            .or_else(|| field("iri"))
            .or_else(|| field("question")),
    };
    cap_detail(&detail.map_or_else(|| args.to_string(), str::to_owned))
}

/// How much of a tool's detail line is shown before it is elided.
const DETAIL_CHARS: usize = 80;

/// Trim `text` and keep at most [`DETAIL_CHARS`] characters, marking a cut.
fn cap_detail(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() > DETAIL_CHARS {
        format!(
            "{}…",
            text.chars().take(DETAIL_CHARS - 1).collect::<String>()
        )
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_are_unique() {
        let mut a = LiveTranscript::default();
        let mut b = LiveTranscript::default();
        assert_ne!(a.open().id().to_owned(), b.open().id().to_owned());
    }

    #[test]
    fn reopening_mints_a_fresh_id() {
        let mut live = LiveTranscript::default();
        let first = live.open().id().to_owned();
        let second = live.open().id().to_owned();
        assert_ne!(
            first, second,
            "a retry must not reuse the first attempt's id"
        );
    }

    #[test]
    fn a_handle_shares_its_counter_with_the_resource() {
        let mut live = LiveTranscript::default();
        let handle = live.open();
        handle.opened.store(true, Ordering::Relaxed);
        assert!(
            live.has_streamed(),
            "the resource must observe what the stream closure emitted"
        );
        assert_eq!(live.current_id(), Some(handle.id()));
    }

    #[test]
    fn detail_prefers_the_argument_a_user_reads() {
        let args =
            serde_json::json!({ "path": "src/lib.rs", "old_string": "a", "new_string": "b" });
        assert_eq!(tool_detail("edit_file", &args), "src/lib.rs");

        let args = serde_json::json!({ "command": "cargo test" });
        assert_eq!(tool_detail("run_command", &args), "cargo test");
    }

    #[test]
    fn detail_falls_back_to_the_arguments() {
        let args = serde_json::json!({ "unexpected": 3 });
        assert_eq!(tool_detail("mystery", &args), r#"{"unexpected":3}"#);
    }

    #[test]
    fn detail_names_the_subject_of_a_graph_or_memory_call() {
        for key in [
            "symbol_name",
            "name",
            "trait_name",
            "role",
            "resource",
            "iri",
        ] {
            let args = serde_json::json!({ key: "Server" });
            assert_eq!(
                tool_detail("find_by_role", &args),
                "Server",
                "the {key} argument is what a reader recognises the call by"
            );
        }
    }

    #[test]
    fn a_long_detail_is_capped_so_it_fits_beside_the_spinner() {
        let args = serde_json::json!({ "command": "e".repeat(200) });
        let detail = tool_detail("run_command", &args);
        assert_eq!(detail.chars().count(), DETAIL_CHARS);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn a_long_body_is_bounded_on_a_char_boundary() {
        let body = "é".repeat(MAX_TOOL_BODY);
        let out = bounded(&body);
        assert!(out.len() < body.len());
        assert!(out.contains("more bytes"));
        // Truncating mid-`é` would panic on the slice; reaching here proves it did not.
        assert!(out.starts_with('é'));
    }

    #[test]
    fn a_short_body_is_untouched() {
        assert_eq!(bounded("ok"), "ok");
    }
}
