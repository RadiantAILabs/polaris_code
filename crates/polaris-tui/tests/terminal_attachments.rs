//! Public host contract coverage for composer attachments.
//!
//! The panel's own behaviour — chips appearing, `Ctrl-V`, `Ctrl-X`, the
//! submission guard — is covered inside `panels::conversation`, which can drive
//! a panel directly. What is checked here is the part a *host* has to program
//! against from outside the crate: the wire key, the request/refusal vocabulary,
//! and that supplying a source is all the opt-in there is.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use polaris::plugins::IOMessage;
use polaris_tui::commands::{
    Attachment, AttachmentFuture, AttachmentRejected, AttachmentRequest, AttachmentSource,
    CancelToken, META_ATTACHMENTS, attachment_handles, escape_mention, parse_mentions,
    stamp_attachments,
};
use polaris_tui::panels::ConversationPanelPlugin;

/// A host resolver that answers images by extension and refuses anything else,
/// standing in for the policy a real host would apply.
struct ExtensionHost {
    calls: Arc<AtomicUsize>,
    cap_bytes: u64,
}

impl AttachmentSource for ExtensionHost {
    fn resolve(&self, request: AttachmentRequest, cancel: CancelToken) -> AttachmentFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let cap = self.cap_bytes;
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(AttachmentRejected::NotAttachable);
            }
            let path = match request {
                AttachmentRequest::Path(path) => path,
                AttachmentRequest::Clipboard => {
                    return Err(AttachmentRejected::failed(
                        "clipboard images are unavailable here; attach by path with `@file`",
                    ));
                }
            };
            if !path.ends_with(".png") {
                return Err(AttachmentRejected::NotAttachable);
            }
            if path.starts_with("huge") {
                return Err(AttachmentRejected::failed(format!(
                    "`{path}` is over the {cap} byte cap"
                )));
            }
            Ok(Attachment::new(
                format!("h:{path}"),
                path,
                "image/png",
                1_024,
            ))
        })
    }
}

#[tokio::test]
async fn a_host_source_answers_paths_and_refuses_the_clipboard_honestly() {
    let calls = Arc::new(AtomicUsize::new(0));
    let host = ExtensionHost {
        calls: calls.clone(),
        cap_bytes: 3_750_000,
    };

    let attached = host
        .resolve(
            AttachmentRequest::Path("diagram.png".into()),
            CancelToken::new(),
        )
        .await
        .expect("a png attaches");
    assert_eq!(attached.handle, "h:diagram.png");
    assert_eq!(attached.media_type, "image/png");
    assert_eq!(attached.summary(), "diagram.png · image/png · 1.0 KB");

    let plain = host
        .resolve(
            AttachmentRequest::Path("src/main.rs".into()),
            CancelToken::new(),
        )
        .await
        .expect_err("a source file is not an attachment");
    assert_eq!(plain, AttachmentRejected::NotAttachable);
    assert_eq!(
        plain.notice(),
        None,
        "an ordinary mention must not raise a notice — it stays text, and that is correct"
    );

    let refused = host
        .resolve(
            AttachmentRequest::Path("huge.png".into()),
            CancelToken::new(),
        )
        .await
        .expect_err("over the cap");
    assert!(
        refused
            .notice()
            .is_some_and(|reason| reason.contains("cap")),
        "a refusal has to say what to do about it: {refused:?}"
    );

    let clipboard = host
        .resolve(AttachmentRequest::Clipboard, CancelToken::new())
        .await
        .expect_err("this platform has no clipboard images");
    assert!(
        clipboard
            .notice()
            .is_some_and(|reason| reason.contains("@file")),
        "degrading honestly means naming the route that does work: {clipboard:?}"
    );

    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

/// A cancelled resolve stops rather than finishing work nobody will read.
#[tokio::test]
async fn a_cancelled_resolve_gives_up() {
    let host = ExtensionHost {
        calls: Arc::new(AtomicUsize::new(0)),
        cap_bytes: 1,
    };
    let cancel = CancelToken::new();
    cancel.cancel();
    let outcome = host
        .resolve(AttachmentRequest::Path("diagram.png".into()), cancel)
        .await;
    assert_eq!(outcome, Err(AttachmentRejected::NotAttachable));
}

#[test]
fn handles_ride_the_prompt_and_nothing_else_does() {
    let chips = [
        Attachment::new("h:a.png", "a.png", "image/png", 10).with_mention("a.png"),
        Attachment::new("h:clip", "clipboard-1.png", "image/png", 20),
    ];
    let prompt = stamp_attachments(IOMessage::user_text("compare @a.png"), &chips);

    assert_eq!(attachment_handles(&prompt), ["h:a.png", "h:clip"]);
    assert_eq!(
        prompt.metadata.len(),
        1,
        "the only thing attachments add to a prompt is the handle list"
    );
    let body = match &prompt.content {
        polaris::plugins::IOContent::Text(text) => text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(
        body, "compare @a.png",
        "the prompt text is never rewritten to describe what was attached"
    );
    assert!(
        !body.contains("h:a.png") && !body.contains("image/png"),
        "and no handle or media type leaks into what the model reads: {body}"
    );
}

/// The mention syntax is what ties a chip to the draft, so a path with a space
/// has to survive the round trip.
#[test]
fn a_mention_with_a_space_round_trips_to_the_path_a_host_is_handed() {
    let drafted = format!("look at @{}", escape_mention("design notes/plan a.png"));
    assert_eq!(drafted, "look at @design\\ notes/plan\\ a.png");
    let mentions = parse_mentions(&drafted);
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].path, "design notes/plan a.png");
}

#[test]
fn a_consumer_that_does_not_opt_in_is_unchanged() {
    let plain = ConversationPanelPlugin::default();
    let attaching =
        ConversationPanelPlugin::default().with_attachment_source(Arc::new(ExtensionHost {
            calls: Arc::new(AtomicUsize::new(0)),
            cap_bytes: 1,
        }));
    // Both are valid plugins; the difference is entirely in what the panel is
    // then able to do, never in what it sends when nothing is attached.
    let _ = (plain, attaching);

    let untouched = stamp_attachments(IOMessage::user_text("hello"), &[]);
    assert!(!untouched.metadata.contains_key(META_ATTACHMENTS));
    assert!(attachment_handles(&untouched).is_empty());
}
