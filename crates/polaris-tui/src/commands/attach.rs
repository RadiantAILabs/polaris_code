//! Host-resolved attachments for the conversation composer.
//!
//! The panel shows a **chip** per attachment and stamps the submitted prompt
//! with the host's handles. It never opens a file, never sniffs a MIME type,
//! never sees a byte of image data, and never decides what the active model
//! can accept. All of that is workspace policy, and workspace policy is the
//! host's — the same boundary [`CompletionSource`](super::CompletionSource)
//! draws for `@` mention candidates.
//!
//! # Where a chip comes from
//!
//! Two origins, and a chip records which one it has:
//!
//! - **A `@path` mention in the draft.** The panel asks the host to resolve
//!   every finished mention once; a mention the host reports as attachable
//!   grows a chip, and the chip lives exactly as long as the mention does.
//!   Deleting the mention text removes the chip, and removing the chip deletes
//!   the mention text — the two directions are the same fact.
//! - **The clipboard**, via the panel's attach chord. The host answers with an
//!   attachment that has no [`mention`](Attachment::mention); that chip stands
//!   on its own until it is removed or submitted. This is the origin that a
//!   terminal without clipboard-image support cannot serve, which is why the
//!   `@path` route above is the load-bearing one and not a convenience.
//!
//! # The wire
//!
//! A submitted prompt carries [`META_ATTACHMENTS`]: a JSON array of the host's
//! own [`handle`](Attachment::handle) strings, in draft order. The handle is
//! opaque here — the host minted it when it validated and read the file, so
//! redeeming it costs no second read and leaves no window in which the file
//! could change between validation and use.
//!
//! ```
//! use polaris::plugins::IOMessage;
//! use polaris_tui::commands::{Attachment, attachment_handles, stamp_attachments};
//!
//! let chips = [Attachment::new("h-1", "diagram.png", "image/png", 4_096)];
//! let message = stamp_attachments(IOMessage::user_text("look at @diagram.png"), &chips);
//! assert_eq!(attachment_handles(&message), ["h-1"]);
//! ```
//!
//! A host that supplies no [`AttachmentSource`] never gets a chip, never gets
//! this metadata key, and its prompts are unchanged.

use std::future::Future;
use std::pin::Pin;

use polaris::plugins::IOMessage;

use super::CancelToken;

/// Metadata key on a submitted prompt: a JSON array of host attachment
/// handles, in the order their chips appear.
pub const META_ATTACHMENTS: &str = "attachments";

/// One attachment the host resolved, as the panel holds it.
///
/// Carries what a chip has to render and the handle the host redeems. It
/// deliberately carries **no data**: the bytes stayed with the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The host's opaque identifier, sent back on submission. Never parsed
    /// here.
    pub handle: String,
    /// Short name for the chip (`diagram.png`).
    pub label: String,
    /// The media type the host determined (`image/png`). Shown, not trusted.
    pub media_type: String,
    /// Size in bytes, shown beside the label.
    pub bytes: u64,
    /// The `@` mention this chip follows, when it came from one. `None` for a
    /// chip with no text in the draft — a clipboard capture.
    pub mention: Option<String>,
}

impl Attachment {
    /// A standalone chip: no mention in the draft backs it.
    #[must_use]
    pub fn new(
        handle: impl Into<String>,
        label: impl Into<String>,
        media_type: impl Into<String>,
        bytes: u64,
    ) -> Self {
        Self {
            handle: handle.into(),
            label: label.into(),
            media_type: media_type.into(),
            bytes,
            mention: None,
        }
    }

    /// Tie this chip to the `@` mention at `path`, so the draft and the chip
    /// list stay one fact.
    #[must_use]
    pub fn with_mention(mut self, path: impl Into<String>) -> Self {
        self.mention = Some(path.into());
        self
    }

    /// The chip as one line of text: `diagram.png · image/png · 4.0 KB`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} · {} · {}",
            self.label,
            self.media_type,
            fmt_bytes(self.bytes)
        )
    }
}

/// Why a request did not become an attachment.
///
/// The two cases are genuinely different and the panel treats them
/// differently, so they are not one error string: a mention of an ordinary
/// text file is *supposed* to stay text, and reporting that as a failure
/// would put a red notice under every `@src/main.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentRejected {
    /// Nothing is wrong; this simply is not an attachment. Leave the draft
    /// alone and say nothing.
    NotAttachable,
    /// The request could have been an attachment but was refused. The string
    /// is shown to the user verbatim and must say what to do about it.
    Failed(String),
}

impl AttachmentRejected {
    /// A refusal carrying an actionable reason.
    #[must_use]
    pub fn failed(reason: impl Into<String>) -> Self {
        Self::Failed(reason.into())
    }

    /// The reason to show, if this refusal has one.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        match self {
            Self::NotAttachable => None,
            Self::Failed(reason) => Some(reason),
        }
    }
}

/// What the panel is asking the host to attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentRequest {
    /// A path the draft mentions, exactly as the user typed it (unescaped).
    Path(String),
    /// Whatever image the system clipboard holds.
    ///
    /// A terminal cannot read the clipboard itself — the emulator owns it and
    /// delivers only text through bracketed paste — so this is a request the
    /// host may well be unable to serve. Answering
    /// [`Failed`](AttachmentRejected::Failed) with the reason and the `@path`
    /// alternative is the expected, honest outcome on such a platform.
    Clipboard,
}

/// Boxed future returned by [`AttachmentSource::resolve`].
pub type AttachmentFuture =
    Pin<Box<dyn Future<Output = Result<Attachment, AttachmentRejected>> + Send + 'static>>;

/// A host-supplied resolver for composer attachments.
///
/// The host owns every question the panel cannot answer: whether the path is
/// inside the workspace, what the file actually contains, whether it is small
/// enough, whether the active model accepts it at all, and whether this
/// platform can produce a clipboard image. Honour `cancel` — the panel cancels
/// a resolve whose mention left the draft.
pub trait AttachmentSource: Send + Sync + 'static {
    /// Resolve `request` into an attachment, or say why not.
    fn resolve(&self, request: AttachmentRequest, cancel: CancelToken) -> AttachmentFuture;
}

/// Stamp `message` with the handles of `attachments`, in order.
///
/// A message with no attachments comes back untouched, so the key appears only
/// on a prompt that actually carries something.
#[must_use]
pub fn stamp_attachments(message: IOMessage, attachments: &[Attachment]) -> IOMessage {
    if attachments.is_empty() {
        return message;
    }
    let handles: Vec<&str> = attachments
        .iter()
        .map(|attachment| attachment.handle.as_str())
        .collect();
    match serde_json::to_string(&handles) {
        Ok(list) => message.with_metadata(META_ATTACHMENTS, list),
        Err(_) => message,
    }
}

/// The attachment handles `message` carries, in order.
///
/// Empty when the key is absent or is not a JSON array of strings; a
/// malformed value is treated as no attachments rather than as an error,
/// because the alternative is dropping a prompt the user did write.
#[must_use]
pub fn attachment_handles(message: &IOMessage) -> Vec<String> {
    message
        .metadata
        .get(META_ATTACHMENTS)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
}

/// Render a byte count for a chip: `842 B`, `4.0 KB`, `2.3 MB`.
fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1_024.0;
    const MB: f64 = KB * 1_024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= MB {
        format!("{:.1} MB", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.1} KB", bytes_f / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chip_reads_as_name_type_and_size() {
        let chip = Attachment::new("h", "diagram.png", "image/png", 4_096);
        assert_eq!(chip.summary(), "diagram.png · image/png · 4.0 KB");
        assert_eq!(
            Attachment::new("h", "tiny.gif", "image/gif", 842).summary(),
            "tiny.gif · image/gif · 842 B"
        );
        assert_eq!(
            Attachment::new("h", "big.png", "image/png", 2_411_724).summary(),
            "big.png · image/png · 2.3 MB"
        );
    }

    #[test]
    fn handles_round_trip_in_draft_order() {
        let chips = [
            Attachment::new("h-2", "b.png", "image/png", 1),
            Attachment::new("h-1", "a.png", "image/png", 1),
        ];
        let message = stamp_attachments(IOMessage::user_text("both"), &chips);
        assert_eq!(attachment_handles(&message), ["h-2", "h-1"]);
    }

    #[test]
    fn an_unattached_prompt_carries_no_key() {
        let message = stamp_attachments(IOMessage::user_text("plain"), &[]);
        assert!(!message.metadata.contains_key(META_ATTACHMENTS));
        assert!(attachment_handles(&message).is_empty());
    }

    /// A garbled value must not cost the user their prompt: the prompt is
    /// still a prompt, it simply carries nothing.
    #[test]
    fn a_malformed_value_reads_as_no_attachments() {
        let message = IOMessage::user_text("x").with_metadata(META_ATTACHMENTS, "not json");
        assert!(attachment_handles(&message).is_empty());
        let numbers = IOMessage::user_text("x").with_metadata(META_ATTACHMENTS, "[1,2]");
        assert!(attachment_handles(&numbers).is_empty());
    }

    #[test]
    fn only_a_refusal_carries_a_notice() {
        assert_eq!(AttachmentRejected::NotAttachable.notice(), None);
        assert_eq!(
            AttachmentRejected::failed("too large; cap is 3.6 MB").notice(),
            Some("too large; cap is 3.6 MB")
        );
    }
}
