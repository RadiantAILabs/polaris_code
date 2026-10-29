//! Coding-side policy for composer attachments: what may be attached, from
//! where, and to which model.
//!
//! `polaris-tui` owns the chip and the chord and deliberately ships no
//! file-system rules, no MIME table and no model knowledge — the same division
//! [`super::mentions`] already draws for `@` paths. This module supplies all of
//! it for the coding agent:
//!
//! | Piece | Role |
//! |-------|------|
//! | [`WorkspaceAttachments`] | The [`AttachmentSource`] the panel calls. Applies containment, content, size and capability policy, then parks the bytes. |
//! | [`AttachmentStore`] | Where the bytes wait between validation and the turn that uses them. A session resource, so the agent reads it as `Res<AttachmentStore>`. |
//! | [`ClipboardImages`] | The platform seam for a pasted image, and the honest place to say a platform has none. |
//!
//! # Why a handle and not a path
//!
//! The panel is handed an opaque handle, and the prompt carries that handle
//! rather than the path. The file is read **once**, when the user attaches it
//! and while the refusal can still reach them; nothing re-reads it at send
//! time. A path on the wire would reopen the file after validation, so a file
//! that grew, moved, or became a symlink out of the workspace in between would
//! be validated in one state and sent in another.
//!
//! # What is checked, in order
//!
//! 1. **Containment.** The path must resolve inside the working directory,
//!    symlinks followed first — the same rule, and the same code, that refuses
//!    `@../secrets` in an ordinary prompt.
//! 2. **Content.** The type comes from the file's magic bytes, never its
//!    extension: `notes.png` that is really a ZIP is not an image.
//! 3. **Size.** [`ATTACHMENT_BYTES_MAX`] on the raw file, chosen so the
//!    base64 that actually goes on the wire stays under the 5 MB per-image
//!    limit the providers impose.
//! 4. **Capability.** [`model_accepts_images`] on the session's *current*
//!    model, so switching to a text-only model with `/model` refuses the next
//!    attachment instead of silently dropping it at request time.
//!
//! A path that fails 1, 3 or 4 is an actionable refusal the user sees. A path
//! that fails 2 — an ordinary text file — is not a failure at all: it stays a
//! plain `@` mention, which is what mentioning source code has always meant.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use polaris::models::llm::{ImageMediaType, Message, TextBlock, UserBlock};
use polaris::system::resource::LocalResource;
use polaris_tui::commands::{
    Attachment, AttachmentFuture, AttachmentRejected, AttachmentRequest, AttachmentSource,
    CancelToken,
};

use super::SessionSettings;
use super::mentions::{self, ContainedPrompt};

/// Largest file that may be attached, in bytes.
///
/// The provider limit is on the **encoded** image, and base64 costs four bytes
/// for every three, so the raw cap is three quarters of it: 3.75 MB in gives
/// 5 MB out. Capping the raw file is what lets the refusal quote a number the
/// user can compare against what `ls` told them.
pub const ATTACHMENT_BYTES_MAX: u64 = 3_750_000;

/// Most attachments one session keeps resolved at a time.
///
/// A resolved attachment holds its bytes until the prompt that names it is
/// taken. A draft that is edited away, or a prompt the channel refused, leaves
/// its handle unredeemed, so the store evicts oldest-first rather than growing
/// for the life of the session.
const STORE_CAPACITY: usize = 16;

// ─────────────────────────────────────────────────────────────────────────────
// The store
// ─────────────────────────────────────────────────────────────────────────────

/// One validated attachment, waiting for the prompt that names it.
#[derive(Debug, Clone)]
pub struct StoredAttachment {
    /// File name shown on the chip and in a refusal.
    pub label: String,
    /// The image type, read from the file's magic bytes.
    pub media_type: ImageMediaType,
    /// The bytes, exactly as they were read.
    pub data: Vec<u8>,
}

impl StoredAttachment {
    /// The canonical model block for this attachment.
    #[must_use]
    pub fn into_block(self) -> UserBlock {
        UserBlock::image_base64(base64(&self.data), self.media_type)
    }
}

/// Resolved attachments, keyed by the handle the composer carries.
///
/// Cloning shares one store: the terminal resolver fills it and the agent's
/// `receive_user_input` empties it, and both hold their own clone.
#[derive(Debug, Clone, Default)]
pub struct AttachmentStore {
    entries: Arc<Mutex<Vec<(String, StoredAttachment)>>>,
    next: Arc<AtomicU64>,
}

impl LocalResource for AttachmentStore {}

impl AttachmentStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Park `attachment` and mint the handle that redeems it.
    pub fn put(&self, attachment: StoredAttachment) -> String {
        let handle = format!("att-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut entries = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        while entries.len() >= STORE_CAPACITY {
            entries.remove(0);
        }
        entries.push((handle.clone(), attachment));
        handle
    }

    /// Redeem `handle`, removing it. A handle is one-shot: the same prompt
    /// cannot be replayed into two turns' worth of image blocks.
    #[must_use]
    pub fn take(&self, handle: &str) -> Option<StoredAttachment> {
        let mut entries = self.entries.lock().unwrap_or_else(|err| err.into_inner());
        let index = entries.iter().position(|(key, _)| key == handle)?;
        Some(entries.remove(index).1)
    }

    /// How many attachments are parked. Test and diagnostic use.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .len()
    }

    /// Whether nothing is parked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Build the canonical user message for one prompt and the attachments
/// `handles` still names.
///
/// Images come **first**: providers read an instruction that follows its
/// images more reliably than one that precedes them. The prompt text is
/// carried through unchanged and is never rewritten to describe what was
/// attached — the image block is the attachment, and a sentence about the
/// image is not a substitute for it.
///
/// A handle the store no longer holds is skipped rather than failing the turn.
/// It means the attachment was evicted or already redeemed, and dropping the
/// user's prompt over it would be a worse answer than sending the text.
///
/// # Why the text is a [`ContainedPrompt`]
///
/// This is the only function that turns a user's prompt into the `Message` a
/// turn starts from, which makes it the one place the working-directory
/// containment rule can be made unforgeable rather than remembered. Taking
/// [`ContainedPrompt`] instead of `String` means a submit path cannot hand
/// the model mention text whose paths were never resolved: it has to produce
/// the proof first, and [`ContainedPrompt::check`] is the only thing that
/// makes one. A new submit path that forgets the check does not compile.
#[must_use]
pub fn user_message(store: &AttachmentStore, text: ContainedPrompt, handles: &[String]) -> Message {
    let text = text.into_text();
    if handles.is_empty() {
        return Message::user(text);
    }
    let mut content: Vec<UserBlock> = handles
        .iter()
        .filter_map(|handle| store.take(handle))
        .map(StoredAttachment::into_block)
        .collect();
    content.push(UserBlock::Text(TextBlock { text }));
    Message::User { content }
}

// ─────────────────────────────────────────────────────────────────────────────
// Content and capability policy
// ─────────────────────────────────────────────────────────────────────────────

/// The image type `data` actually is, read from its leading bytes.
///
/// Extension is not consulted at all. A screenshot saved as `.txt` attaches;
/// an archive named `.png` does not.
#[must_use]
pub fn sniff_image(data: &[u8]) -> Option<(ImageMediaType, &'static str)> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const GIF87: &[u8] = b"GIF87a";
    const GIF89: &[u8] = b"GIF89a";
    if data.starts_with(PNG) {
        return Some((ImageMediaType::PNG, "image/png"));
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some((ImageMediaType::JPEG, "image/jpeg"));
    }
    if data.starts_with(GIF87) || data.starts_with(GIF89) {
        return Some((ImageMediaType::GIF, "image/gif"));
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some((ImageMediaType::WEBP, "image/webp"));
    }
    None
}

/// Whether this host will send image blocks to `model_id`.
///
/// **Fail-closed.** The framework's `ModelRegistry` carries no capability
/// metadata, so this is a host-owned allowlist, and a model it does not
/// recognise is treated as text-only. Refusing an attachment for a model that
/// could in fact have read it costs the user one message; sending one to a
/// model that cannot costs them a failed request they did not ask for and
/// cannot diagnose.
///
/// The id is the registry's `provider/model` form. Known exceptions inside an
/// otherwise vision-capable family are listed explicitly:
/// `claude-3-5-haiku` and the `claude-2`/`claude-instant` generations take
/// text only.
///
/// ```
/// use polaris_code::terminal::model_accepts_images;
///
/// assert!(model_accepts_images("anthropic/claude-sonnet-4-6"));
/// assert!(!model_accepts_images("anthropic/claude-3-5-haiku-latest"));
/// assert!(!model_accepts_images("somevendor/brand-new-model"));
/// ```
#[must_use]
pub fn model_accepts_images(model_id: &str) -> bool {
    let model = model_id
        .rsplit_once('/')
        .map_or(model_id, |(_, model)| model)
        .to_ascii_lowercase();
    const TEXT_ONLY: &[&str] = &[
        "claude-3-5-haiku",
        "claude-3.5-haiku",
        "claude-instant",
        "claude-2",
    ];
    if TEXT_ONLY.iter().any(|name| model.starts_with(name)) {
        return false;
    }
    const VISION: &[&str] = &[
        "claude-3",
        "claude-sonnet-",
        "claude-opus-",
        "claude-haiku-",
        "gpt-4o",
        "gpt-4-turbo",
        "gpt-4.1",
        "gpt-5",
        "o3",
        "o4",
    ];
    VISION.iter().any(|name| model.starts_with(name))
}

// ─────────────────────────────────────────────────────────────────────────────
// Clipboard
// ─────────────────────────────────────────────────────────────────────────────

/// A platform source of clipboard images.
///
/// A terminal cannot read the system clipboard itself: the emulator owns it
/// and hands the process only text, through bracketed paste. Reaching an image
/// means asking a platform helper, and on a platform with none installed the
/// right answer is to say so — which is why [`capture`](Self::capture) returns
/// a reason rather than an empty buffer.
pub trait ClipboardImages: Send + Sync + 'static {
    /// The image bytes currently on the clipboard, or why there are none.
    fn capture(&self) -> Result<Vec<u8>, String>;
}

/// A clipboard reader that shells out to a platform helper.
#[derive(Debug, Clone)]
pub struct CommandClipboard {
    program: PathBuf,
    args: Vec<String>,
    name: &'static str,
}

impl ClipboardImages for CommandClipboard {
    fn capture(&self) -> Result<Vec<u8>, String> {
        let output = std::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|err| format!("{} could not be run: {err}", self.name))?;
        if !output.status.success() || output.stdout.is_empty() {
            return Err(format!(
                "the clipboard holds no image ({} returned nothing)",
                self.name
            ));
        }
        Ok(output.stdout)
    }
}

/// The clipboard helper available on this machine, if any.
///
/// Deliberately narrow and deliberately conditional on a display being
/// present, because a helper on `PATH` with no display attached fails at the
/// moment the user presses the chord rather than at startup:
///
/// | Platform | Helper | Also requires |
/// |---|---|---|
/// | Wayland | `wl-paste --type image/png` | `WAYLAND_DISPLAY` |
/// | X11 | `xclip -selection clipboard -t image/png -o` | `DISPLAY` |
/// | macOS | `pngpaste -` (`brew install pngpaste`) | — |
/// | Windows, or none of the above | *(none)* | — |
///
/// Returning `None` is a supported outcome, not a defect: the composer's
/// `@path` route works everywhere and is what the refusal names.
#[must_use]
pub fn detect_clipboard() -> Option<Box<dyn ClipboardImages>> {
    let has_display = |var: &str| std::env::var_os(var).is_some_and(|value| !value.is_empty());
    let candidates: [(&'static str, &[&str], bool); 3] = [
        (
            "wl-paste",
            &["--type", "image/png"],
            has_display("WAYLAND_DISPLAY"),
        ),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
            has_display("DISPLAY"),
        ),
        ("pngpaste", &["-"], cfg!(target_os = "macos")),
    ];
    for (name, args, usable) in candidates {
        if !usable {
            continue;
        }
        if let Some(program) = which(name) {
            return Some(Box::new(CommandClipboard {
                program,
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                name,
            }));
        }
    }
    None
}

/// The first executable named `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

// ─────────────────────────────────────────────────────────────────────────────
// The source
// ─────────────────────────────────────────────────────────────────────────────

/// The coding host's [`AttachmentSource`]: workspace paths and the clipboard.
#[derive(Clone)]
pub struct WorkspaceAttachments {
    working_dir: PathBuf,
    store: AttachmentStore,
    /// Model used when no session settings have been published yet — the
    /// configured default, which is what the first turn would use anyway.
    default_model: String,
    /// The live session settings, once the composition root has them. `/model`
    /// changes the answer to the capability check from the next attachment on.
    settings: Arc<OnceLock<SessionSettings>>,
    clipboard: Option<Arc<dyn ClipboardImages>>,
    clipboard_seq: Arc<AtomicU64>,
}

impl WorkspaceAttachments {
    /// Attach from `working_dir`, parking bytes in `store`, for `model_id`.
    ///
    /// The platform clipboard helper is detected here; use
    /// [`with_clipboard`](Self::with_clipboard) to supply one explicitly.
    #[must_use]
    pub fn new(
        working_dir: impl Into<PathBuf>,
        store: AttachmentStore,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            working_dir: working_dir.into(),
            store,
            default_model: model_id.into(),
            settings: Arc::new(OnceLock::new()),
            clipboard: detect_clipboard().map(Arc::from),
            clipboard_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Track the session's live model and permission settings.
    #[must_use]
    pub fn with_settings(mut self, settings: Arc<OnceLock<SessionSettings>>) -> Self {
        self.settings = settings;
        self
    }

    /// Replace the detected clipboard helper.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Arc<dyn ClipboardImages>) -> Self {
        self.clipboard = Some(clipboard);
        self
    }

    /// Drop clipboard support, whatever the platform offers.
    #[must_use]
    pub fn without_clipboard(mut self) -> Self {
        self.clipboard = None;
        self
    }

    /// The model the next attachment is checked against.
    fn model(&self) -> String {
        self.settings
            .get()
            .map_or_else(|| self.default_model.clone(), |s| s.snapshot().model_id)
    }

    /// Validate `data` and park it, or say why it cannot be attached.
    fn accept(&self, label: String, data: Vec<u8>) -> Result<Attachment, AttachmentRejected> {
        let Some((media_type, mime)) = sniff_image(&data) else {
            return Err(AttachmentRejected::NotAttachable);
        };
        let bytes = data.len() as u64;
        if bytes > ATTACHMENT_BYTES_MAX {
            return Err(AttachmentRejected::failed(format!(
                "`{label}` is {}; attachments are capped at {}",
                fmt_bytes(bytes),
                fmt_bytes(ATTACHMENT_BYTES_MAX)
            )));
        }
        let model = self.model();
        if !model_accepts_images(&model) {
            return Err(AttachmentRejected::failed(format!(
                "`{model}` does not read images; switch with /model, or describe `{label}` in words"
            )));
        }
        let handle = self.store.put(StoredAttachment {
            label: label.clone(),
            media_type,
            data,
        });
        Ok(Attachment::new(handle, label, mime, bytes))
    }

    /// Read and validate one workspace path.
    fn resolve_path(&self, mentioned: &str) -> Result<Attachment, AttachmentRejected> {
        let inside = match mentions::contain(&self.working_dir, mentioned) {
            // A path that is not there yet is not a refusal — the user may
            // still be typing it, and an unresolvable mention has always been
            // left as ordinary text.
            mentions::Containment::Missing => return Err(AttachmentRejected::NotAttachable),
            mentions::Containment::Outside => {
                return Err(AttachmentRejected::failed(format!(
                    "`{mentioned}` is outside the working directory and cannot be attached"
                )));
            }
            mentions::Containment::Inside(path) => path,
        };
        let metadata = match std::fs::metadata(&inside) {
            Ok(metadata) => metadata,
            Err(_) => return Err(AttachmentRejected::NotAttachable),
        };
        if !metadata.is_file() {
            return Err(AttachmentRejected::NotAttachable);
        }
        // Check the size before reading, so an enormous file is refused rather
        // than pulled into memory to be refused.
        if metadata.len() > ATTACHMENT_BYTES_MAX {
            return Err(AttachmentRejected::failed(format!(
                "`{mentioned}` is {}; attachments are capped at {}",
                fmt_bytes(metadata.len()),
                fmt_bytes(ATTACHMENT_BYTES_MAX)
            )));
        }
        let data = std::fs::read(&inside).map_err(|err| {
            AttachmentRejected::failed(format!("`{mentioned}` is unreadable: {err}"))
        })?;
        self.accept(mentioned.to_string(), data)
    }

    /// Capture and validate a clipboard image.
    fn resolve_clipboard(&self) -> Result<Attachment, AttachmentRejected> {
        let Some(clipboard) = &self.clipboard else {
            return Err(AttachmentRejected::failed(
                "this terminal cannot read clipboard images \
                 (install wl-clipboard, xclip, or pngpaste); \
                 save the image into the working directory and attach it with `@file`",
            ));
        };
        let data = clipboard.capture().map_err(AttachmentRejected::failed)?;
        let label = format!(
            "clipboard-{}.png",
            self.clipboard_seq.fetch_add(1, Ordering::Relaxed)
        );
        match self.accept(label, data) {
            // A clipboard capture that is not an image is worth saying out
            // loud: the user pressed a key expecting something to happen, and
            // unlike a mention there is no draft text to fall back to.
            Err(AttachmentRejected::NotAttachable) => Err(AttachmentRejected::failed(
                "the clipboard does not hold a PNG, JPEG, GIF or WebP image",
            )),
            other => other,
        }
    }
}

impl AttachmentSource for WorkspaceAttachments {
    fn resolve(&self, request: AttachmentRequest, cancel: CancelToken) -> AttachmentFuture {
        let host = self.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(AttachmentRejected::NotAttachable);
            }
            // Reading a file and shelling out to a clipboard helper both block;
            // the panel calls this from the UI task.
            let outcome = tokio::task::spawn_blocking(move || match request {
                AttachmentRequest::Path(path) => host.resolve_path(&path),
                AttachmentRequest::Clipboard => host.resolve_clipboard(),
            })
            .await;
            match outcome {
                Ok(result) => result,
                Err(err) => Err(AttachmentRejected::failed(format!(
                    "attachment check did not finish: {err}"
                ))),
            }
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Standard base64, no line breaks — the encoding the provider block wants.
///
/// Hand-rolled to avoid a dependency for one alphabet lookup.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A byte count as a refusal quotes it.
fn fmt_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64;
    if value >= 1_000_000.0 {
        format!("{:.1} MB", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1} KB", value / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

/// The handles a prompt carries, resolved against a store that may no longer
/// hold them. Exposed for hosts that bridge the panel to their own queue.
#[must_use]
pub fn take_all(store: &AttachmentStore, handles: &[String]) -> HashMap<String, StoredAttachment> {
    handles
        .iter()
        .filter_map(|handle| store.take(handle).map(|entry| (handle.clone(), entry)))
        .collect()
}

/// Whether `path` is a file the host would attach, used by the tests and by
/// callers that want to explain the rule.
#[must_use]
pub fn is_attachable(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .is_some_and(|data| sniff_image(&data).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prompt text for a test that is not about containment. None of these
    /// mention a path, so every containment root gives the same answer and
    /// the test does not need a workspace to be checked against.
    fn prompt(text: &str) -> ContainedPrompt {
        ContainedPrompt::check_unbound(text.to_string()).expect("no mentions to contain")
    }

    const PNG_1PX: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00,
    ];

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("design")).unwrap();
        std::fs::write(dir.path().join("design/diagram.png"), PNG_1PX).unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"plain text").unwrap();
        // A file whose extension lies about its content.
        std::fs::write(dir.path().join("fake.png"), b"PK\x03\x04 not an image").unwrap();
        dir
    }

    fn host(dir: &tempfile::TempDir) -> WorkspaceAttachments {
        WorkspaceAttachments::new(
            dir.path(),
            AttachmentStore::new(),
            "anthropic/claude-sonnet-4-6",
        )
        .without_clipboard()
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // Every byte value, so a lookup off by one in the high half shows up.
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = base64(&all);
        assert_eq!(encoded.len(), 344);
        assert!(
            encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKiss")
        );
        assert!(encoded.ends_with("+/w=="));
    }

    #[test]
    fn content_decides_the_type_not_the_extension() {
        assert!(matches!(
            sniff_image(PNG_1PX),
            Some((ImageMediaType::PNG, "image/png"))
        ));
        assert!(matches!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some((ImageMediaType::JPEG, "image/jpeg"))
        ));
        assert!(matches!(
            sniff_image(b"GIF89a....."),
            Some((ImageMediaType::GIF, "image/gif"))
        ));
        assert!(matches!(
            sniff_image(b"RIFF\0\0\0\0WEBPVP8 "),
            Some((ImageMediaType::WEBP, "image/webp"))
        ));
        assert!(sniff_image(b"PK\x03\x04").is_none());
        assert!(sniff_image(b"").is_none());
        // RIFF with a different payload (a WAV) is not an image.
        assert!(sniff_image(b"RIFF\0\0\0\0WAVEfmt ").is_none());
    }

    #[test]
    fn an_unknown_model_is_treated_as_text_only() {
        assert!(model_accepts_images("anthropic/claude-sonnet-4-6"));
        assert!(model_accepts_images("anthropic/claude-opus-4-1"));
        assert!(model_accepts_images("anthropic/claude-3-7-sonnet-latest"));
        assert!(model_accepts_images("openai/gpt-4o-mini"));
        assert!(!model_accepts_images("anthropic/claude-3-5-haiku-latest"));
        assert!(!model_accepts_images("anthropic/claude-2.1"));
        assert!(!model_accepts_images("anthropic/claude-instant-1.2"));
        assert!(
            !model_accepts_images("somevendor/brand-new-model"),
            "a model this host has never heard of fails closed"
        );
    }

    #[tokio::test]
    async fn a_workspace_image_attaches_and_its_handle_redeems_once() {
        let dir = workspace();
        let host = host(&dir);
        let store = host.store.clone();

        let attached = host
            .resolve(
                AttachmentRequest::Path("design/diagram.png".into()),
                CancelToken::new(),
            )
            .await
            .expect("a png inside the workspace attaches");
        assert_eq!(attached.media_type, "image/png");
        assert_eq!(attached.label, "design/diagram.png");
        assert_eq!(attached.bytes, PNG_1PX.len() as u64);
        assert_eq!(store.len(), 1);

        let message = user_message(
            &store,
            prompt("what is this"),
            std::slice::from_ref(&attached.handle),
        );
        let Message::User { content } = message else {
            panic!("expected a user message");
        };
        assert_eq!(content.len(), 2);
        assert!(
            matches!(content[0], UserBlock::Image(_)),
            "the image leads, the instruction follows it"
        );
        match &content[1] {
            UserBlock::Text(text) => assert_eq!(
                text.text, "what is this",
                "the prompt text is carried through, not replaced by a description"
            ),
            other => panic!("expected text, got {other:?}"),
        }
        assert!(store.is_empty(), "a handle is one-shot");

        // Redeeming it again yields the text alone rather than failing the turn.
        let again = user_message(&store, prompt("again"), &[attached.handle]);
        let Message::User { content } = again else {
            panic!("expected a user message");
        };
        assert_eq!(content.len(), 1);
    }

    #[tokio::test]
    async fn an_ordinary_file_is_left_as_text_and_a_liar_is_not_attached() {
        let dir = workspace();
        let host = host(&dir);
        for path in ["notes.txt", "fake.png", "design", "nothing-here.png"] {
            let outcome = host
                .resolve(AttachmentRequest::Path(path.into()), CancelToken::new())
                .await;
            assert_eq!(
                outcome.expect_err(path),
                AttachmentRejected::NotAttachable,
                "`{path}` stays an ordinary mention, silently"
            );
        }
        assert!(host.store.is_empty());
    }

    #[tokio::test]
    async fn a_path_escape_is_refused_with_a_reason() {
        let dir = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("leak.png"), PNG_1PX).unwrap();
        let host = host(&dir);

        for mention in [
            "../leak.png".to_string(),
            outside.path().join("leak.png").display().to_string(),
        ] {
            let refused = host
                .resolve(AttachmentRequest::Path(mention.clone()), CancelToken::new())
                .await
                .expect_err(&mention);
            assert!(
                refused
                    .notice()
                    .is_some_and(|reason| reason.contains("outside the working directory")),
                "{mention}: {refused:?}"
            );
        }
        assert!(host.store.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_pointing_out_of_the_workspace_is_refused() {
        let dir = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("leak.png"), PNG_1PX).unwrap();
        std::os::unix::fs::symlink(outside.path().join("leak.png"), dir.path().join("link.png"))
            .unwrap();

        let refused = host(&dir)
            .resolve(
                AttachmentRequest::Path("link.png".into()),
                CancelToken::new(),
            )
            .await
            .expect_err("a link out is still out");
        assert!(
            refused
                .notice()
                .is_some_and(|reason| reason.contains("outside the working directory")),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn an_oversized_image_is_refused_with_both_numbers() {
        let dir = workspace();
        let mut big = PNG_1PX.to_vec();
        big.resize(ATTACHMENT_BYTES_MAX as usize + 1, 0);
        std::fs::write(dir.path().join("huge.png"), &big).unwrap();

        let refused = host(&dir)
            .resolve(
                AttachmentRequest::Path("huge.png".into()),
                CancelToken::new(),
            )
            .await
            .expect_err("over the cap");
        let reason = refused.notice().expect("actionable");
        assert!(reason.contains("3.8 MB"), "{reason}");
        assert!(reason.contains("capped at 3.8 MB"), "{reason}");
    }

    #[tokio::test]
    async fn a_text_only_model_refuses_before_anything_is_sent() {
        let dir = workspace();
        let host = WorkspaceAttachments::new(
            dir.path(),
            AttachmentStore::new(),
            "anthropic/claude-3-5-haiku-latest",
        )
        .without_clipboard();

        let refused = host
            .resolve(
                AttachmentRequest::Path("design/diagram.png".into()),
                CancelToken::new(),
            )
            .await
            .expect_err("a text-only model cannot take it");
        let reason = refused.notice().expect("actionable");
        assert!(reason.contains("does not read images"), "{reason}");
        assert!(reason.contains("/model"), "it names the way out: {reason}");
        assert!(
            host.store.is_empty(),
            "and nothing was parked for a turn that will not happen"
        );
    }

    #[tokio::test]
    async fn a_platform_without_a_clipboard_helper_names_the_path_route() {
        let dir = workspace();
        let refused = host(&dir)
            .resolve(AttachmentRequest::Clipboard, CancelToken::new())
            .await
            .expect_err("no helper configured");
        let reason = refused.notice().expect("actionable");
        assert!(reason.contains("cannot read clipboard images"), "{reason}");
        assert!(reason.contains("@file"), "{reason}");
    }

    #[tokio::test]
    async fn a_clipboard_image_attaches_under_a_generated_name() {
        struct Fake(Vec<u8>);
        impl ClipboardImages for Fake {
            fn capture(&self) -> Result<Vec<u8>, String> {
                Ok(self.0.clone())
            }
        }
        let dir = workspace();
        let host = host(&dir).with_clipboard(Arc::new(Fake(PNG_1PX.to_vec())));

        let first = host
            .resolve(AttachmentRequest::Clipboard, CancelToken::new())
            .await
            .expect("clipboard png");
        let second = host
            .resolve(AttachmentRequest::Clipboard, CancelToken::new())
            .await
            .expect("clipboard png");
        assert_eq!(first.label, "clipboard-1.png");
        assert_eq!(second.label, "clipboard-2.png");
        assert_ne!(first.handle, second.handle);
        assert_eq!(host.store.len(), 2);
    }

    #[tokio::test]
    async fn a_clipboard_holding_something_else_says_so() {
        struct NotAnImage;
        impl ClipboardImages for NotAnImage {
            fn capture(&self) -> Result<Vec<u8>, String> {
                Ok(b"just some text".to_vec())
            }
        }
        let dir = workspace();
        let refused = host(&dir)
            .with_clipboard(Arc::new(NotAnImage))
            .resolve(AttachmentRequest::Clipboard, CancelToken::new())
            .await
            .expect_err("text on the clipboard is not an attachment");
        assert!(
            refused
                .notice()
                .is_some_and(|reason| reason.contains("does not hold a PNG")),
            "unlike a mention there is no draft to fall back on, so this one speaks: {refused:?}"
        );
    }

    #[test]
    fn the_store_evicts_oldest_first_rather_than_growing_without_bound() {
        let store = AttachmentStore::new();
        let handles: Vec<String> = (0..STORE_CAPACITY + 4)
            .map(|index| {
                store.put(StoredAttachment {
                    label: format!("{index}.png"),
                    media_type: ImageMediaType::PNG,
                    data: PNG_1PX.to_vec(),
                })
            })
            .collect();
        assert_eq!(store.len(), STORE_CAPACITY);
        assert!(
            store.take(&handles[0]).is_none(),
            "the oldest abandoned handle is the one that went"
        );
        assert!(store.take(handles.last().unwrap()).is_some());
    }

    #[test]
    fn a_prompt_with_no_handles_is_the_plain_message_it_always_was() {
        let store = AttachmentStore::new();
        let message = user_message(&store, prompt("hello"), &[]);
        let Message::User { content } = message else {
            panic!("expected a user message");
        };
        assert_eq!(content.len(), 1);
        assert!(matches!(content[0], UserBlock::Text(_)));
    }

    #[test]
    fn is_attachable_reads_the_file_rather_than_the_name() {
        let dir = workspace();
        assert!(is_attachable(&dir.path().join("design/diagram.png")));
        assert!(!is_attachable(&dir.path().join("fake.png")));
        assert!(!is_attachable(&dir.path().join("notes.txt")));
    }

    #[test]
    fn take_all_skips_handles_the_store_no_longer_holds() {
        let store = AttachmentStore::new();
        let handle = store.put(StoredAttachment {
            label: "a.png".into(),
            media_type: ImageMediaType::PNG,
            data: PNG_1PX.to_vec(),
        });
        let taken = take_all(&store, &[handle.clone(), "att-missing".into()]);
        assert_eq!(taken.len(), 1);
        assert!(taken.contains_key(&handle));
    }
}
