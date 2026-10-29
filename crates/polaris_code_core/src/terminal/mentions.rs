//! `@path` mentions in a prompt, resolved against the working directory.
//!
//! The panel completes mentions from candidates this host supplied, but a
//! mention can also be typed by hand, so every prompt is checked before it is
//! forwarded: a mention that resolves **outside** the working directory —
//! `@../secrets`, or a symlink that points out — is refused with an error
//! instead of being handed to the model, because the tools it would ask for
//! are sandboxed to the working directory and could never honour it. A
//! mention of a path that does not exist is left as ordinary text; it may be
//! a handle rather than a file.
//!
//! # Where the check runs
//!
//! [`ContainedPrompt`] is the answer, and it is a *type* rather than a step,
//! because a check attached to one transport can be bypassed by a host that
//! submits another way. A prompt reaches the model only as the
//! [`user_message`](super::attachments::user_message) this crate assembles,
//! and that function takes a [`ContainedPrompt`] — whose only constructor is
//! [`ContainedPrompt::check`]. So the containment decision is a precondition
//! of building the message, on every submit path there is and every one there
//! could be:
//!
//! | Submit path | Where the check happens |
//! |---|---|
//! | `UserIO` channel | [`validate`], in [`CommandDispatch`](super::CommandDispatch), before the agent's `receive` returns — so the refusal answers the prompt and the channel keeps waiting. |
//! | Turn-input queue ([`TurnSteering`](super::TurnSteering)) | At ingress, in every [`TurnInputHandle`](polaris_tui::panels::conversation::TurnInputHandle) mutator, so the refusal reaches the composer while it still holds the user's draft. Nothing unchecked can be queued. |
//! | Anything else, including a front end with no decorator | `receive_user_input`, which cannot build the message without a [`ContainedPrompt`] and ends the turn with the refusal rather than sending it. |

use std::path::{Path, PathBuf};

use polaris::plugins::{IO_TYPE, IOContent, IOMessage};
use polaris_tui::commands::parse_mentions;

/// Metadata key stamped on a forwarded prompt: a JSON array of the mentioned
/// paths that resolved inside the working directory, relative to it.
pub const META_MENTIONS: &str = "mentions";

/// A mention that must not be forwarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionRejected {
    /// The path as typed.
    pub path: String,
    /// Why.
    pub reason: String,
}

impl MentionRejected {
    /// The one-line refusal, in the words the user reads.
    ///
    /// Both surfaces a refusal can arrive on render this same string: the
    /// transcript error [`into_message`](Self::into_message) builds, and the
    /// composer notice a [`TurnInputHandle`](polaris_tui::panels::conversation::TurnInputHandle)
    /// returns. One wording, so a user cannot be told two different things
    /// about the same mention depending on which path they submitted by.
    #[must_use]
    pub fn notice(&self) -> String {
        format!(
            "`@{}` was not sent: {}. Mention a path inside the working directory, or drop the `@`.",
            self.path, self.reason
        )
    }

    /// The error message the user sees in place of a reply.
    #[must_use]
    pub fn into_message(self) -> IOMessage {
        IOMessage::system_text(self.notice()).with_metadata(IO_TYPE, "error")
    }
}

/// Prompt text whose `@` mentions have all been resolved against a working
/// directory, and found to stay inside it.
///
/// This type is the containment guarantee, not a convenience. The only way to
/// obtain one is [`check`](Self::check), and
/// [`user_message`](super::attachments::user_message) — the one function in
/// this crate that turns a user's prompt into the `Message` a turn starts
/// from — accepts nothing else. A submit path that forgets the check therefore
/// does not compile; a check attached to one transport can be bypassed by a
/// host that submits another way.
///
/// The text itself is carried through **unchanged**. Containment is a decision
/// about whether to send the prompt at all, never a rewrite of what the user
/// wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainedPrompt {
    text: String,
    inside: Vec<PathBuf>,
}

impl ContainedPrompt {
    /// Check every `@` mention in `text` against `working_dir`.
    ///
    /// # Errors
    ///
    /// Returns the first mention that resolves outside `working_dir`. Nothing
    /// is rewritten and nothing partial is produced: either the whole prompt
    /// is contained or it is refused.
    pub fn check(text: String, working_dir: &Path) -> Result<Self, MentionRejected> {
        let inside = resolve_inside(&text, working_dir)?;
        Ok(Self { text, inside })
    }

    /// Refuse every mention in `text`, because there is no working directory
    /// to resolve them against yet.
    ///
    /// For a host whose containment root is not bound: it cannot answer the
    /// question, and answering "inside" anyway is precisely the fail-open
    /// hole this type exists to close. A prompt that mentions nothing is
    /// still contained — there is nothing to resolve — so ordinary text
    /// always gets through.
    ///
    /// # Errors
    ///
    /// Returns the first mention in `text`, if there is one.
    pub fn check_unbound(text: String) -> Result<Self, MentionRejected> {
        if let Some(mention) = parse_mentions(&text).into_iter().next() {
            return Err(MentionRejected {
                path: mention.path,
                reason: "the workspace it would resolve against is not bound yet".into(),
            });
        }
        Ok(Self {
            text,
            inside: Vec::new(),
        })
    }

    /// The prompt, exactly as the user typed it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The mentioned paths that resolved inside the working directory,
    /// relative to it. Empty when the prompt mentions nothing that exists.
    #[must_use]
    pub fn inside(&self) -> &[PathBuf] {
        &self.inside
    }

    /// Consume this into the text it proved contained.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }
}

/// Check every mention in `message` against `working_dir`.
///
/// On success the message comes back with [`META_MENTIONS`] listing the
/// resolved paths (only when there are any). On failure the first offending
/// mention is returned and the message is not to be forwarded.
///
/// # Errors
///
/// Returns the first mention that resolves outside `working_dir`.
pub fn validate(message: IOMessage, working_dir: &Path) -> Result<IOMessage, MentionRejected> {
    let IOContent::Text(text) = &message.content else {
        return Ok(message);
    };
    let resolved = resolve_inside(text, working_dir)?;
    if resolved.is_empty() {
        return Ok(message);
    }
    let list = serde_json::Value::Array(
        resolved
            .into_iter()
            .map(|path| serde_json::Value::String(path.to_string_lossy().into_owned()))
            .collect(),
    );
    Ok(message.with_metadata(META_MENTIONS, list.to_string()))
}

/// The containment rule itself: the mentions in `text` that resolve inside
/// `working_dir`, relative to it, or the first one that escapes.
///
/// One function so [`validate`] and [`ContainedPrompt::check`] cannot drift
/// into disagreeing about what "inside" means — the same reason
/// [`attachments`](super::attachments) reuses [`contain`] rather than
/// reimplementing it.
fn resolve_inside(text: &str, working_dir: &Path) -> Result<Vec<PathBuf>, MentionRejected> {
    let mentions = parse_mentions(text);
    if mentions.is_empty() {
        return Ok(Vec::new());
    }
    let root = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let mut resolved = Vec::new();
    for mention in mentions {
        match contain(working_dir, &mention.path) {
            Containment::Inside(absolute) => {
                resolved.push(
                    absolute
                        .strip_prefix(&root)
                        .unwrap_or(&absolute)
                        .to_path_buf(),
                );
            }
            Containment::Missing => {}
            Containment::Outside => {
                return Err(MentionRejected {
                    path: mention.path,
                    reason: "it resolves outside the working directory".into(),
                });
            }
        }
    }
    Ok(resolved)
}

/// Where a mentioned path sits relative to the working directory.
///
/// The three cases are distinct on purpose. `Outside` is a refusal; `Missing`
/// is not, because a mention of something that is not there may simply be a
/// handle rather than a file, and has always been left as ordinary text.
pub(crate) enum Containment {
    /// Inside the working directory, canonicalised.
    Inside(PathBuf),
    /// Nothing is there, but the path stays inside once `..` is folded.
    Missing,
    /// Resolves outside the working directory.
    Outside,
}

/// Resolve a mentioned path against `working_dir`.
///
/// Symlinks are followed before the containment check, so a link inside the
/// tree that points out is `Outside`. This is the one containment rule the
/// terminal has: [`ContainedPrompt::check`] refuses a prompt with it (and
/// [`validate`] refuses a channel message through the same helper), while
/// [`attachments`](super::attachments) refuses an attachment with it, so the
/// three can never disagree about what is inside the workspace.
pub(crate) fn contain(working_dir: &Path, mentioned: &str) -> Containment {
    let root = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let joined = if Path::new(mentioned).is_absolute() {
        PathBuf::from(mentioned)
    } else {
        root.join(mentioned)
    };
    let Ok(canonical) = joined.canonicalize() else {
        // Nonexistent: decide containment lexically so `@../x` is still
        // refused even when nothing is there.
        return if lexically_inside(&root, &joined) {
            Containment::Missing
        } else {
            Containment::Outside
        };
    };
    if canonical.starts_with(&root) {
        Containment::Inside(canonical)
    } else {
        Containment::Outside
    }
}

/// Whether `path` stays under `root` once `.` and `..` are folded, without
/// touching the filesystem.
fn lexically_inside(root: &Path, path: &Path) -> bool {
    use std::path::Component;
    let mut depth: usize = 0;
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    for component in relative.components() {
        match component {
            Component::ParentDir => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("read me.md"), "# hi").unwrap();
        dir
    }

    fn text(message: &IOMessage) -> &str {
        match &message.content {
            IOContent::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn inside_mentions_are_stamped_and_missing_ones_left_alone() {
        let ws = workspace();
        let message = IOMessage::user_text("look at @src/main.rs and @read\\ me.md and @ghost.rs");
        let forwarded = validate(message, ws.path()).unwrap();
        assert_eq!(
            text(&forwarded),
            "look at @src/main.rs and @read\\ me.md and @ghost.rs",
            "the prompt text is untouched"
        );
        let mentions: Vec<String> =
            serde_json::from_str(forwarded.metadata.get(META_MENTIONS).unwrap()).unwrap();
        assert_eq!(mentions, ["src/main.rs", "read me.md"]);

        let plain = validate(IOMessage::user_text("no mentions here"), ws.path()).unwrap();
        assert!(!plain.metadata.contains_key(META_MENTIONS));
        let email = validate(IOMessage::user_text("mail me@example.com"), ws.path()).unwrap();
        assert!(!email.metadata.contains_key(META_MENTIONS));
    }

    #[test]
    fn escapes_are_rejected_whether_or_not_the_target_exists() {
        let ws = workspace();
        let outside = tempfile::NamedTempFile::new().unwrap();
        for mention in [
            "../".to_string(),
            "../secrets.txt".to_string(),
            "src/../../x".to_string(),
            outside.path().display().to_string(),
            "/etc/passwd".to_string(),
        ] {
            let message = IOMessage::user_text(format!("see @{mention}"));
            let rejected = validate(message, ws.path()).expect_err(&mention);
            assert_eq!(rejected.path, mention);
            let reply = rejected.into_message();
            assert_eq!(reply.metadata.get(IO_TYPE).unwrap(), "error");
            assert!(text(&reply).contains("was not sent"));
        }
        // Folding `..` back inside is fine.
        let ok = validate(IOMessage::user_text("@src/../src/main.rs"), ws.path()).unwrap();
        assert!(ok.metadata.contains_key(META_MENTIONS));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_is_rejected() {
        let ws = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("leak.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("leak.txt"), ws.path().join("link.txt"))
            .unwrap();
        let rejected = validate(IOMessage::user_text("@link.txt"), ws.path()).unwrap_err();
        assert_eq!(rejected.path, "link.txt");
    }

    #[test]
    fn a_contained_prompt_keeps_the_text_and_answers_the_same_as_validate() {
        let ws = workspace();
        let prompt =
            ContainedPrompt::check("look at @src/main.rs and @ghost.rs".to_string(), ws.path())
                .expect("nothing escapes");
        assert_eq!(
            prompt.as_str(),
            "look at @src/main.rs and @ghost.rs",
            "the text is carried through, never rewritten"
        );
        assert_eq!(
            prompt.inside(),
            [std::path::PathBuf::from("src/main.rs")],
            "and it reports what it resolved, exactly as the channel path stamps"
        );

        // The two entry points are the same rule: whatever `validate` refuses
        // on the channel, `check` refuses at a queue, and vice versa.
        for text in [
            "see @../secrets.txt",
            "see @/etc/passwd",
            "see @src/../../x",
            "see @../",
        ] {
            let by_channel = validate(IOMessage::user_text(text), ws.path()).expect_err(text);
            let by_type = ContainedPrompt::check(text.to_string(), ws.path()).expect_err(text);
            assert_eq!(by_channel, by_type, "one rule, two entry points: {text}");
            assert!(by_type.notice().contains("was not sent"));
        }
    }

    #[test]
    fn a_prompt_with_no_mentions_is_contained_without_touching_the_filesystem() {
        // The working directory here does not exist, so a check that stat'ed
        // anything would fail. A prompt that mentions nothing has nothing to
        // resolve, and refusing it would refuse every ordinary sentence.
        let nowhere = std::path::Path::new("/nonexistent-working-dir-for-this-test");
        let prompt = ContainedPrompt::check("just a question".to_string(), nowhere)
            .expect("no mentions, nothing to contain");
        assert_eq!(prompt.as_str(), "just a question");
        assert!(prompt.inside().is_empty());
        // An escape is still refused there: containment falls back to folding
        // `..` lexically precisely so a missing target cannot buy a bypass.
        assert!(
            ContainedPrompt::check("read @../secrets.txt".to_string(), nowhere).is_err(),
            "an escape is refused whether or not the working directory resolves"
        );
    }
}
