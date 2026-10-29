//! `/diff` — one read-only view of everything this session changed.
//!
//! This composes three things that already exist and adds no fourth: the
//! additive [`CommandRegistry`](super::CommandRegistry), the
//! [`EditJournal`](super::EditJournal) — which already holds an exact
//! before-image and after-image for every `edit_file` write — and the
//! conversation panel's existing unified-diff rendering, which colours a
//! ```` ```diff ```` fence and folds it behind `Ctrl-O`. There is no new
//! panel, no diff engine (`similar` already produces the per-edit diff
//! `edit_file` returns) and no change-tracking service.
//!
//! # What the change is measured against, and why
//!
//! **Baseline:** the bytes each file held when *this session* first edited it
//! — the `before` image of the first tracked edit at or after the session's
//! earliest retained rewind boundary.
//!
//! **Current:** the bytes on disk right now. Not the journal's last
//! after-image.
//!
//! Three alternatives were rejected. Measuring from the *workspace's* first
//! observation ignores session scoping: the journal is per-workspace and
//! durable, so yesterday's session would show up in today's review. Measuring
//! from the *last rewind point* answers "what changed since I last rewound",
//! which is a different and narrower question than "what did this session
//! do". Measuring the journal's last after-image as the current state would
//! be a lie after a `/rewind`, after a shell write or after an external edit:
//! the journal records the write it made, not the file's fate. Reading disk
//! keeps the view honest in all three cases — a rewound file correctly drops
//! out of the change set, and a file something else wrote afterwards is
//! flagged rather than silently attributed to the agent.
//!
//! # What this view cannot see
//!
//! Everything outside the journal's boundary
//! ([`rewind`](super::rewind)): shell writes, editor and external edits,
//! files created or deleted by anything other than `edit_file`, and any file
//! this session never passed to `edit_file`. A file the journal knows about
//! *and* something else also wrote is detectable — disk no longer matches the
//! last tracked image — and is flagged per row. A file the journal never saw
//! is not detectable at all. Both facts are stated in the view itself, every
//! time, including when nothing changed, so a user never reads a short list
//! as a complete one.
//!
//! # Bounds
//!
//! A large change set degrades deterministically rather than emergently:
//! at most [`MAX_SUMMARY_ROWS`] summary rows, inline diffs for at most
//! [`MAX_DIFF_FILES`] files at [`MAX_FILE_DIFF_LINES`] lines each, and every
//! rendered line clipped to [`MAX_LINE_CHARS`]. Anything elided says so and
//! names `/diff <path>`, which renders one file at [`MAX_SINGLE_DIFF_LINES`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use polaris::sessions::SessionId;
use polaris_tui::commands::CommandSpec;

use super::{CommandError, CommandOutcome, CommandRegistry, EditJournal, TrackedPath};

/// Summary rows listed before the rest collapse into a count.
pub const MAX_SUMMARY_ROWS: usize = 100;
/// Files whose diff is rendered inline by `/diff` with no argument.
pub const MAX_DIFF_FILES: usize = 20;
/// Diff lines rendered per file in the aggregate view.
pub const MAX_FILE_DIFF_LINES: usize = 120;
/// Diff lines rendered by `/diff <path>` for a single file.
pub const MAX_SINGLE_DIFF_LINES: usize = 600;
/// Characters kept per rendered diff line.
pub const MAX_LINE_CHARS: usize = 500;

/// The standing disclosure. Present in every reply, including the empty one.
const BOUNDARY_NOTE: &str = "**Partial by construction.** This is built from the `edit_file` \
     journal, so it sees only files `edit_file` wrote in this session, measured against the bytes \
     each held when this session first edited it. Shell writes, external edits, and files created \
     or deleted outside `edit_file` are not tracked — run `git status` for the whole working tree. \
     Reviewing changes nothing.";

/// How a tracked path currently differs from its session baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeStatus {
    /// Both images are text and a unified diff was produced.
    Modified,
    /// The file is gone from disk. Deletion is outside the journal's boundary.
    Deleted,
    /// The change is real but not renderable as a line diff; the string says why.
    Opaque(String),
}

/// One changed file in the session's change set.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Workspace-relative where possible, else absolute.
    pub path: PathBuf,
    /// What kind of change this is.
    pub status: ChangeStatus,
    /// Lines added relative to the session baseline.
    pub added: usize,
    /// Lines removed relative to the session baseline.
    pub removed: usize,
    /// Disk no longer matches the last image `edit_file` wrote, so at least
    /// one writer outside the journal also touched this path.
    pub drifted: bool,
    /// The unified diff, when one could be produced.
    pub diff: Option<String>,
}

/// Everything this session changed, as far as the journal can tell.
#[derive(Debug, Clone, Default)]
pub struct ChangeSet {
    /// Changed files, ordered by path.
    pub files: Vec<FileChange>,
    /// Tracked paths whose current bytes equal their session baseline — a
    /// rewound or hand-reverted edit nets out and is not a change.
    pub reverted: usize,
    /// Journal retention evicted edits this window needed, so a baseline may
    /// be later than the session's real first observation.
    pub truncated_history: bool,
    /// No rewind boundary for this session survived, so the window could not
    /// be scoped to the session and was not attempted.
    pub unscoped: bool,
}

impl ChangeSet {
    /// Whether the session changed nothing the journal can see.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Total lines added across every changed file.
    #[must_use]
    pub fn added(&self) -> usize {
        self.files.iter().map(|file| file.added).sum()
    }

    /// Total lines removed across every changed file.
    #[must_use]
    pub fn removed(&self) -> usize {
        self.files.iter().map(|file| file.removed).sum()
    }
}

/// The `/diff` command: a read-only aggregate over the edit journal.
///
/// Holds no session handle on purpose. Reviewing must not be able to mutate
/// conversation state, so this seam is given the journal, the workspace root
/// and a way to ask which session is current — nothing it could write through.
#[derive(Clone)]
pub struct SessionReview {
    journal: EditJournal,
    workspace: PathBuf,
    current: Arc<dyn Fn() -> SessionId + Send + Sync>,
}

impl std::fmt::Debug for SessionReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionReview")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl SessionReview {
    /// Bind the journal and current-session lookup.
    pub fn new(
        journal: EditJournal,
        current: impl Fn() -> SessionId + Send + Sync + 'static,
    ) -> Self {
        let workspace = journal.workspace();
        Self {
            journal,
            workspace,
            current: Arc::new(current),
        }
    }

    /// Register `/diff`.
    ///
    /// # Errors
    ///
    /// Refused if `/diff` is already registered.
    pub fn register(&self, registry: &CommandRegistry) -> Result<(), CommandError> {
        let this = self.clone();
        registry.register_sync(
            CommandSpec::new("diff", "Review every file this session changed").with_usage("[path]"),
            move |_, args| this.command(args),
        )
    }

    /// The session's change set. Reads the journal and the working tree; writes nothing.
    #[must_use]
    pub fn changes(&self) -> ChangeSet {
        let session = (self.current)();
        let Some(cursor) = self.journal.session_start(&session) else {
            return ChangeSet {
                unscoped: true,
                ..ChangeSet::default()
            };
        };
        let window = self.journal.tracked_since(cursor);
        let mut set = ChangeSet {
            truncated_history: window.truncated,
            ..ChangeSet::default()
        };
        for tracked in &window.paths {
            match describe(&self.workspace, tracked) {
                Some(change) => set.files.push(change),
                None => set.reverted += 1,
            }
        }
        set
    }

    fn command(&self, args: &str) -> CommandOutcome {
        let wanted = args.trim();
        let set = self.changes();
        if wanted.is_empty() {
            return CommandOutcome::Reply(render(&set));
        }
        match select(&set, wanted) {
            Some(change) => CommandOutcome::Reply(render_one(change)),
            None if set.is_empty() => CommandOutcome::Error(format!(
                "no tracked change to `{wanted}` in this session — `/diff` lists what is tracked"
            )),
            None => CommandOutcome::Error(format!(
                "no tracked change to `{wanted}` in this session. Changed: {}",
                set.files
                    .iter()
                    .take(MAX_SUMMARY_ROWS)
                    .map(|file| format!("`{}`", file.path.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Resolve a user-typed path against the change set: exact first, then unique suffix.
fn select<'a>(set: &'a ChangeSet, wanted: &str) -> Option<&'a FileChange> {
    let wanted = Path::new(wanted);
    if let Some(exact) = set.files.iter().find(|file| file.path == wanted) {
        return Some(exact);
    }
    let mut matches = set
        .files
        .iter()
        .filter(|file| file.path.ends_with(wanted) || file.path.file_name() == wanted.file_name());
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Compare one tracked chain's baseline against the file on disk.
///
/// `None` means the path nets out to no change — a `/rewind` or a hand-revert
/// put the session's original bytes back, and reporting it as changed would
/// be wrong.
fn describe(workspace: &Path, tracked: &TrackedPath) -> Option<FileChange> {
    let path = tracked
        .path
        .strip_prefix(workspace)
        .unwrap_or(&tracked.path)
        .to_path_buf();
    let current = match std::fs::read(&tracked.path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Some(FileChange {
                path,
                status: ChangeStatus::Opaque(format!("unreadable: {error}")),
                added: 0,
                removed: 0,
                drifted: true,
                diff: None,
            });
        }
    };
    let Some(current) = current else {
        return Some(FileChange {
            path,
            status: ChangeStatus::Deleted,
            added: 0,
            removed: 0,
            // The journal never records a delete, so whatever removed the file
            // did so outside the boundary.
            drifted: true,
            diff: None,
        });
    };
    if !tracked.images_complete {
        return Some(FileChange {
            path,
            status: ChangeStatus::Opaque(
                "an image in this chain was too large to retain; no baseline to diff against"
                    .into(),
            ),
            added: 0,
            removed: 0,
            drifted: false,
            diff: None,
        });
    }
    if current == tracked.before {
        return None;
    }
    let drifted = current != tracked.after;
    let (Ok(before), Ok(after)) = (
        std::str::from_utf8(&tracked.before),
        std::str::from_utf8(&current),
    ) else {
        return Some(FileChange {
            path,
            status: ChangeStatus::Opaque(format!(
                "binary or non-UTF-8 content ({} → {} bytes)",
                tracked.before.len(),
                current.len()
            )),
            added: 0,
            removed: 0,
            drifted,
            diff: None,
        });
    };
    let label = path.display().to_string();
    let text = similar::TextDiff::from_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    for change in text.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let diff = text
        .unified_diff()
        .context_radius(3)
        .header(&label, &label)
        .to_string();
    Some(FileChange {
        path,
        status: ChangeStatus::Modified,
        added,
        removed,
        drifted,
        diff: Some(diff),
    })
}

/// The aggregate reply: header, every row, bounded diffs, then the disclosure.
#[must_use]
pub fn render(set: &ChangeSet) -> String {
    let mut text = String::new();
    if set.is_empty() {
        text.push_str(if set.reverted > 0 {
            "No net file changes in this session: every tracked edit was reverted or rewound.\n\n"
        } else {
            "No tracked file changes in this session.\n\n"
        });
        push_caveats(&mut text, set);
        text.push_str(BOUNDARY_NOTE);
        return text;
    }
    let _ = writeln!(
        text,
        "Changed this session — {} file(s), +{} −{}.\n",
        set.files.len(),
        set.added(),
        set.removed()
    );
    for file in set.files.iter().take(MAX_SUMMARY_ROWS) {
        let _ = writeln!(text, "- {}", row(file));
    }
    if set.files.len() > MAX_SUMMARY_ROWS {
        let _ = writeln!(
            text,
            "- … {} more file(s) not listed — run `/diff <path>` for one of them.",
            set.files.len() - MAX_SUMMARY_ROWS
        );
    }
    text.push('\n');
    for file in set.files.iter().take(MAX_DIFF_FILES) {
        push_diff(&mut text, file, MAX_FILE_DIFF_LINES);
    }
    if set.files.len() > MAX_DIFF_FILES {
        let _ = writeln!(
            text,
            "Diffs for {} further file(s) were not rendered here — run `/diff <path>` to see one.\n",
            set.files.len() - MAX_DIFF_FILES
        );
    }
    push_caveats(&mut text, set);
    text.push_str(BOUNDARY_NOTE);
    text
}

/// One file's reply, at the larger single-file budget.
#[must_use]
pub fn render_one(file: &FileChange) -> String {
    let mut text = format!("{}\n\n", row(file));
    push_diff(&mut text, file, MAX_SINGLE_DIFF_LINES);
    text.push_str(BOUNDARY_NOTE);
    text
}

fn row(file: &FileChange) -> String {
    let mut row = format!("`{}`", file.path.display());
    match &file.status {
        ChangeStatus::Modified => {
            let _ = write!(row, "  +{} −{}", file.added, file.removed);
        }
        ChangeStatus::Deleted => row.push_str("  deleted — outside `edit_file`, not restorable"),
        ChangeStatus::Opaque(reason) => {
            let _ = write!(row, "  changed — {reason}");
        }
    }
    if file.drifted && file.status != ChangeStatus::Deleted {
        row.push_str("  ⚠ also written outside `edit_file`");
    }
    row
}

fn push_diff(text: &mut String, file: &FileChange, budget: usize) {
    let Some(diff) = &file.diff else {
        return;
    };
    let rows: Vec<&str> = diff.lines().collect();
    text.push_str("```diff\n");
    for line in rows.iter().take(budget) {
        let clipped = line.char_indices().nth(MAX_LINE_CHARS).map(|(at, _)| at);
        match clipped {
            Some(at) => {
                text.push_str(&line[..at]);
                text.push_str(" …\n");
            }
            None => {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    text.push_str("```\n");
    if rows.len() > budget {
        let _ = writeln!(
            text,
            "… {} more diff line(s) in `{}` not rendered.",
            rows.len() - budget,
            file.path.display()
        );
    }
    text.push('\n');
}

fn push_caveats(text: &mut String, set: &ChangeSet) {
    if set.unscoped {
        text.push_str(
            "No rewind boundary for this session was retained, so a session-scoped change set \
             could not be built. Nothing below is scoped to this session.\n\n",
        );
    }
    if set.truncated_history {
        text.push_str(
            "⚠ Journal retention evicted earlier edits, so some baselines are later than this \
             session's first edit and those files' changes are understated.\n\n",
        );
    }
    if set.reverted > 0 && !set.is_empty() {
        let _ = writeln!(
            text,
            "{} further tracked file(s) were edited and then restored to their session baseline.\n",
            set.reverted
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, added: usize, removed: usize) -> FileChange {
        FileChange {
            path: PathBuf::from(path),
            status: ChangeStatus::Modified,
            added,
            removed,
            drifted: false,
            diff: Some(format!("--- {path}\n+++ {path}\n@@ -1 +1 @@\n-a\n+b\n")),
        }
    }

    #[test]
    fn a_wide_change_set_lists_every_row_but_bounds_the_diffs() {
        let files = (0..MAX_DIFF_FILES + 7)
            .map(|index| change(&format!("src/f{index:03}.rs"), 1, 1))
            .collect();
        let text = render(&ChangeSet {
            files,
            ..ChangeSet::default()
        });
        assert_eq!(text.matches("```diff").count(), MAX_DIFF_FILES, "{text}");
        assert!(text.contains("Diffs for 7 further file(s)"), "{text}");
        assert!(text.contains("`src/f026.rs`  +1 −1"), "{text}");
    }

    fn deep(path: &str, lines: usize) -> FileChange {
        let body = (0..lines)
            .map(|index| format!("+line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut file = change(path, lines, 0);
        file.diff = Some(format!("--- {path}\n+++ {path}\n{body}\n"));
        file
    }

    #[test]
    fn a_deep_diff_is_clipped_at_each_views_own_budget() {
        // The aggregate view's per-file budget.
        let over = deep("src/deep.rs", MAX_FILE_DIFF_LINES + 40);
        let text = render(&ChangeSet {
            files: vec![over.clone()],
            ..ChangeSet::default()
        });
        assert!(
            // 40 body lines over the budget, plus the two header lines.
            text.contains("… 42 more diff line(s) in `src/deep.rs`"),
            "{text}"
        );
        // The same file is not clipped by the larger single-file budget.
        assert!(
            !render_one(&over).contains("more diff line(s)"),
            "the single-file budget is not the aggregate one"
        );
        // Which has its own ceiling.
        let huge = deep("src/huge.rs", MAX_SINGLE_DIFF_LINES + 10);
        assert!(
            render_one(&huge).contains("more diff line(s) in `src/huge.rs`"),
            "the single-file budget is unbounded"
        );
    }

    #[test]
    fn a_single_wide_line_is_clipped_in_the_single_file_view() {
        let mut file = change("src/min.js", 1, 1);
        file.diff = Some(format!(
            "--- src/min.js\n+++ src/min.js\n+{}\n",
            "y".repeat(MAX_LINE_CHARS * 3)
        ));
        let text = render_one(&file);
        assert!(text.contains(" …"), "{text}");
        assert!(
            !text.contains(&"y".repeat(MAX_LINE_CHARS + 1)),
            "not clipped"
        );
    }

    #[test]
    fn every_reply_carries_the_boundary_note() {
        assert!(render(&ChangeSet::default()).contains("Shell writes, external edits"));
        assert!(render_one(&change("a", 1, 1)).contains("Shell writes, external edits"));
    }
}
