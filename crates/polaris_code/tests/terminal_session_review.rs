//! Consumer proofs for `/diff`, the session's aggregate change review.
//!
//! These use the production pieces the interactive host wires together: the
//! real `CodeTools::edit_file` (so the journal entries are the ones the agent
//! actually produces), the real durable [`EditJournal`], the real
//! [`CommandRegistry`] and the real [`SessionReview`]. Nothing here is a
//! stand-in for the seam `main.rs` registers.

use std::path::{Path, PathBuf};

use polaris::sessions::SessionId;
use polaris::sessions::checkpoint::{CheckpointId, RevisionId};
use polaris::tools::ToolRegistry;
use polaris_code::terminal::{
    CommandOutcome, CommandRegistry, EditJournal, SessionReview,
    review::{MAX_DIFF_FILES, MAX_SUMMARY_ROWS},
};
use polaris_code::tools::{CodeTools, CodeToolsConfig};
use polaris_tui::commands::CommandInvocation;
use serde_json::json;

const SESSION: &str = "review-proof";

struct Host {
    tools: ToolRegistry,
    journal: EditJournal,
    commands: CommandRegistry,
    review: SessionReview,
    working_dir: PathBuf,
    points: std::cell::Cell<u64>,
    _root: tempfile::TempDir,
}

impl Host {
    fn boot() -> Self {
        let root = tempfile::tempdir().unwrap();
        let working_dir = root.path().join("repo");
        std::fs::create_dir_all(&working_dir).unwrap();
        let working_dir = working_dir.canonicalize().unwrap();
        let journal = EditJournal::open(&working_dir).unwrap();
        let mut tools = ToolRegistry::new();
        tools.register_toolset(CodeTools::new(
            CodeToolsConfig::new(&working_dir).with_edit_journal(journal.clone()),
        ));
        let commands = CommandRegistry::with_builtins();
        let review = SessionReview::new(journal.clone(), || SessionId::from_string(SESSION));
        review.register(&commands).unwrap();
        Self {
            tools,
            journal,
            commands,
            review,
            working_dir,
            points: std::cell::Cell::new(0),
            _root: root,
        }
    }

    /// The boundary `main.rs` captures before every prompt, including the first.
    fn open_session(&self) {
        let next = self.points.get() + 1;
        self.points.set(next);
        self.journal
            .add_point(
                &SessionId::from_string(SESSION),
                RevisionId::default(),
                serde_json::from_str::<CheckpointId>(&next.to_string()).unwrap(),
                format!("before turn {next}"),
            )
            .unwrap();
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.working_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    async fn edit(&self, name: &str, old: &str, new: &str) {
        self.tools
            .execute(
                "edit_file",
                &json!({
                    "path": name,
                    "old_string": old,
                    "new_string": new,
                    "replace_all": false,
                }),
            )
            .await
            .unwrap_or_else(|err| panic!("edit_file({name}) failed: {err}"));
    }

    async fn diff(&self, args: &str) -> String {
        match self
            .commands
            .dispatch(&CommandInvocation::new("diff", args))
            .await
        {
            CommandOutcome::Reply(text) => text,
            other => panic!("/diff {args} did not reply: {other:?}"),
        }
    }
}

/// Every byte a review could possibly disturb: the working tree and the journal.
fn fingerprint(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                entries.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    entries.sort();
    entries
}

#[tokio::test]
async fn two_edited_files_are_both_listed_with_the_change_to_each() {
    let host = Host::boot();
    host.write("src/alpha.rs", "fn alpha() {}\n");
    host.write("src/beta.rs", "fn beta() {}\n");
    host.open_session();
    host.edit("src/alpha.rs", "fn alpha() {}", "fn alpha() -> u8 { 1 }")
        .await;
    host.edit("src/beta.rs", "fn beta() {}", "fn beta() -> u8 { 2 }")
        .await;

    let text = host.diff("").await;
    assert!(text.contains("2 file(s)"), "{text}");
    assert!(text.contains("`src/alpha.rs`"), "{text}");
    assert!(text.contains("`src/beta.rs`"), "{text}");
    assert!(text.contains("+fn alpha() -> u8 { 1 }"), "{text}");
    assert!(text.contains("-fn alpha() {}"), "{text}");
    assert!(text.contains("+fn beta() -> u8 { 2 }"), "{text}");
    assert!(text.contains("-fn beta() {}"), "{text}");
    assert_eq!(text.matches("```diff").count(), 2, "{text}");

    // Several edits to one file collapse into one baseline→now change.
    host.edit("src/alpha.rs", "1", "3").await;
    let text = host.diff("").await;
    assert!(text.contains("2 file(s)"), "{text}");
    assert!(text.contains("+fn alpha() -> u8 { 3 }"), "{text}");
    assert!(!text.contains("u8 { 1 }"), "an intermediate leaked: {text}");

    // One file on demand, at the larger single-file budget.
    let single = host.diff("src/beta.rs").await;
    assert!(single.contains("+fn beta() -> u8 { 2 }"), "{single}");
    assert!(!single.contains("src/alpha.rs"), "{single}");
}

#[tokio::test]
async fn only_this_session_is_reported() {
    let host = Host::boot();
    host.write("src/old.rs", "old\n");
    host.edit("src/old.rs", "old", "older").await;
    // The boundary the *current* session opens on is captured after that edit.
    host.open_session();
    host.write("src/new.rs", "new\n");
    host.edit("src/new.rs", "new", "newer").await;

    let text = host.diff("").await;
    assert!(text.contains("1 file(s)"), "{text}");
    assert!(text.contains("`src/new.rs`"), "{text}");
    assert!(
        !text.contains("`src/old.rs`"),
        "a prior session leaked: {text}"
    );
}

#[tokio::test]
async fn reviewing_changes_nothing() {
    let host = Host::boot();
    host.write("src/a.rs", "a\n");
    host.open_session();
    host.edit("src/a.rs", "a", "b").await;

    let before = fingerprint(&host.working_dir);
    let points_before = host
        .journal
        .points(&SessionId::from_string(SESSION), RevisionId::default());
    let window_before = host.journal.tracked_since(0).paths.len();

    let _ = host.diff("").await;
    let _ = host.diff("src/a.rs").await;
    let _ = host.review.changes();

    assert_eq!(
        fingerprint(&host.working_dir),
        before,
        "review wrote to disk"
    );
    assert_eq!(
        host.journal
            .points(&SessionId::from_string(SESSION), RevisionId::default())
            .len(),
        points_before.len(),
        "review moved the journal's boundaries"
    );
    assert_eq!(
        host.journal.tracked_since(0).paths.len(),
        window_before,
        "review moved the journal's edits"
    );
}

#[tokio::test]
async fn changes_outside_the_journal_are_disclosed_rather_than_shown_as_unchanged() {
    let host = Host::boot();
    host.write("src/tracked.rs", "one\n");
    host.open_session();
    host.edit("src/tracked.rs", "one", "two").await;
    // A shell write on top of a tracked file: the journal cannot see it, but
    // disk no longer matches the image `edit_file` left.
    host.write("src/tracked.rs", "three\n");
    // A file `edit_file` never touched at all: outside the boundary entirely.
    host.write("src/shell_only.rs", "created by a shell\n");
    // A tracked file removed afterwards: deletion is outside the boundary too.
    host.write("src/gone.rs", "here\n");
    host.edit("src/gone.rs", "here", "still here").await;
    std::fs::remove_file(host.working_dir.join("src/gone.rs")).unwrap();

    let text = host.diff("").await;
    assert!(
        text.contains("`src/tracked.rs`") && text.contains("⚠ also written outside `edit_file`"),
        "drift was not flagged: {text}"
    );
    assert!(
        text.contains("+three"),
        "disk state is what is diffed: {text}"
    );
    assert!(
        text.contains("`src/gone.rs`") && text.contains("deleted — outside `edit_file`"),
        "a deleted file was not reported: {text}"
    );
    // The untracked file is invisible — so the view must say it might be.
    assert!(!text.contains("shell_only"), "{text}");
    assert!(
        text.contains("Partial by construction")
            && text.contains("Shell writes, external edits")
            && text.contains("git status"),
        "the view claimed to be complete: {text}"
    );
}

#[tokio::test]
async fn a_session_that_changed_nothing_says_so() {
    let host = Host::boot();
    host.open_session();
    let text = host.diff("").await;
    assert!(
        text.contains("No tracked file changes in this session"),
        "{text}"
    );
    assert!(text.contains("Partial by construction"), "{text}");

    // An edit that is put back nets out, and that is stated distinctly.
    host.write("src/a.rs", "a\n");
    host.edit("src/a.rs", "a", "b").await;
    host.write("src/a.rs", "a\n");
    let text = host.diff("").await;
    assert!(
        text.contains("every tracked edit was reverted or rewound"),
        "{text}"
    );
    assert!(text.contains("Partial by construction"), "{text}");
}

#[tokio::test]
async fn a_large_change_set_degrades_by_the_declared_budget() {
    let host = Host::boot();
    let count = MAX_DIFF_FILES + 5;
    for index in 0..count {
        host.write(&format!("src/f{index:03}.rs"), "v0\n");
    }
    host.open_session();
    for index in 0..count {
        host.edit(&format!("src/f{index:03}.rs"), "v0", "v1").await;
    }

    let text = host.diff("").await;
    assert!(text.contains(&format!("{count} file(s)")), "{text}");
    // Every changed file is still named, under the summary-row bound.
    assert!(count <= MAX_SUMMARY_ROWS);
    for index in 0..count {
        assert!(text.contains(&format!("`src/f{index:03}.rs`")), "{index}");
    }
    // But only the declared number of diffs is rendered inline.
    assert_eq!(text.matches("```diff").count(), MAX_DIFF_FILES, "{text}");
    assert!(
        text.contains("Diffs for 5 further file(s) were not rendered here"),
        "{text}"
    );
    // And the elided ones are reachable one at a time.
    let last = host.diff(&format!("src/f{:03}.rs", count - 1)).await;
    assert!(last.contains("+v1"), "{last}");
}

#[tokio::test]
async fn a_session_with_no_retained_boundary_is_disclosed_not_shown_as_empty() {
    let host = Host::boot();
    host.write("src/a.rs", "a\n");
    host.edit("src/a.rs", "a", "b").await;
    let text = host.diff("").await;
    assert!(
        text.contains("could not be built"),
        "a missing boundary was not disclosed: {text}"
    );
}

#[tokio::test]
async fn journal_retention_past_the_window_is_disclosed() {
    // Past the journal's retention bound, the earliest baselines are gone and
    // the change the view shows is smaller than the change that happened.
    let host = Host::boot();
    host.write("src/wide.rs", "v0\n");
    host.open_session();
    host.edit("src/wide.rs", "v0", "v1").await;
    for step in 1..60 {
        host.write(&format!("src/pad{step:03}.rs"), "p0\n");
        host.edit(&format!("src/pad{step:03}.rs"), "p0", "p1").await;
    }
    let text = host.diff("").await;
    assert!(
        text.contains("Journal retention evicted earlier edits"),
        "eviction was not disclosed: {text}"
    );
}

#[tokio::test]
async fn a_path_that_is_not_in_the_change_set_is_an_actionable_error() {
    let host = Host::boot();
    host.write("src/a.rs", "a\n");
    host.open_session();
    host.edit("src/a.rs", "a", "b").await;
    let outcome = host
        .commands
        .dispatch(&CommandInvocation::new("diff", "src/nope.rs"))
        .await;
    let CommandOutcome::Error(text) = outcome else {
        panic!("an unknown path should error")
    };
    assert!(
        text.contains("no tracked change to `src/nope.rs`"),
        "{text}"
    );
    assert!(text.contains("`src/a.rs`"), "{text}");
}
