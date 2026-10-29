//! [`WorkspaceFiles`] — the `@` mention source over the working directory.
//!
//! The panel asks for candidates on every keystroke and cancels the query it
//! no longer needs; this source does the walking. It lists files and
//! directories under the working directory, skipping `.git` and whatever the
//! tree's `.gitignore` files rule out (see [`IgnoreRules`]), and ranks them
//! against the query: paths that start with it first, then file names that
//! do, then paths containing it, then paths spelling it as a subsequence.
//! Matching is case-insensitive over Unicode; a path with spaces is offered
//! as-is and the panel escapes it on insertion.
//!
//! The walk runs on a blocking thread, checks the cancel token as it goes,
//! stops at [`MAX_ENTRIES`], and is cached briefly so a burst of keystrokes
//! reranks one listing rather than re-reading the tree per key.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use polaris_tui::commands::{CancelToken, Completion, CompletionFuture, CompletionSource};
use walkdir::WalkDir;

use super::ignore::IgnoreRules;

/// Most entries the walk collects before giving up on the rest. A workspace
/// bigger than this still completes — its first entries in walk order — but
/// the popup is for finding a file by name, not for listing everything.
pub const MAX_ENTRIES: usize = 20_000;
/// Most candidates returned for one query. The popup shows a window; more
/// than this is noise.
pub const MAX_RESULTS: usize = 50;
/// How long a listing is reused before the tree is walked again.
const CACHE_TTL: Duration = Duration::from_secs(3);
/// How many entries pass between cancellation checks during the walk.
const CANCEL_STRIDE: usize = 256;

/// One listed path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// Path relative to the root, `/`-separated; directories end in `/`.
    relative: String,
    /// Lower-cased `relative`, for matching.
    folded: String,
    is_dir: bool,
}

struct Inner {
    root: PathBuf,
    max_entries: usize,
    cache: Mutex<Option<(Instant, Arc<Vec<Entry>>)>>,
}

/// [`CompletionSource`] over the files of a working directory.
#[derive(Clone)]
pub struct WorkspaceFiles {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for WorkspaceFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceFiles")
            .field("root", &self.inner.root)
            .finish()
    }
}

impl WorkspaceFiles {
    /// A source rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: root.into(),
                max_entries: MAX_ENTRIES,
                cache: Mutex::new(None),
            }),
        }
    }

    /// Cap the walk at `max_entries` instead of [`MAX_ENTRIES`].
    #[must_use]
    pub fn with_max_entries(self, max_entries: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: self.inner.root.clone(),
                max_entries,
                cache: Mutex::new(None),
            }),
        }
    }

    /// The directory being listed.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Rank the listing against `query` synchronously. What
    /// [`complete`](CompletionSource::complete) runs on its blocking thread.
    #[must_use]
    pub fn rank(&self, query: &str, cancel: &CancelToken) -> Vec<Completion> {
        let Some(entries) = self.inner.listing(cancel) else {
            return Vec::new();
        };
        rank_entries(&entries, query)
    }
}

impl Inner {
    /// The cached listing, walked afresh when stale. `None` if cancelled
    /// mid-walk.
    fn listing(&self, cancel: &CancelToken) -> Option<Arc<Vec<Entry>>> {
        if let Some((walked_at, entries)) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && walked_at.elapsed() < CACHE_TTL
        {
            return Some(entries.clone());
        }
        let entries = Arc::new(walk(&self.root, self.max_entries, cancel)?);
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), entries.clone()));
        Some(entries)
    }
}

impl CompletionSource for WorkspaceFiles {
    fn complete(&self, query: String, cancel: CancelToken) -> CompletionFuture {
        let source = self.clone();
        Box::pin(async move {
            let worker = cancel.clone();
            tokio::task::spawn_blocking(move || source.rank(&query, &worker))
                .await
                .unwrap_or_default()
        })
    }
}

/// Walk `root`, honouring `.gitignore` files and skipping `.git`. Returns
/// `None` if `cancel` fired before the walk finished.
fn walk(root: &Path, max_entries: usize, cancel: &CancelToken) -> Option<Vec<Entry>> {
    // Ignore rules per directory, loaded as the walk enters each one. The
    // filter sees a directory before any of its children, so a directory's
    // own `.gitignore` is in place by the time they are judged.
    let mut rules: Vec<(PathBuf, IgnoreRules)> =
        vec![(root.to_path_buf(), IgnoreRules::load(root))];
    let mut entries = Vec::new();
    let mut seen = 0usize;
    let walker = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_dir();
            if is_dir && entry.file_name() == ".git" {
                return false;
            }
            // Rules apply from the root down; a deeper file's verdict
            // overrides a shallower one, matching git's precedence.
            let mut verdict = None;
            for (dir, ignore) in &rules {
                if let Ok(relative) = entry.path().strip_prefix(dir)
                    && let Some(v) = ignore.verdict(&slashed(relative), is_dir)
                {
                    verdict = Some(v);
                }
            }
            if verdict.unwrap_or(false) {
                return false;
            }
            if is_dir {
                let ignore = IgnoreRules::load(entry.path());
                if !ignore.is_empty() {
                    rules.push((entry.path().to_path_buf(), ignore));
                }
            }
            true
        });
    for entry in walker {
        seen += 1;
        if seen.is_multiple_of(CANCEL_STRIDE) && cancel.is_cancelled() {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let is_dir = entry.file_type().is_dir();
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let mut relative = slashed(relative);
        if is_dir {
            relative.push('/');
        }
        entries.push(Entry {
            folded: relative.to_lowercase(),
            relative,
            is_dir,
        });
        if entries.len() >= max_entries {
            break;
        }
    }
    if cancel.is_cancelled() {
        return None;
    }
    Some(entries)
}

/// A relative path as a `/`-separated string. Non-UTF-8 components are
/// replaced lossily; such a path can still be listed, just not typed.
fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Rank `entries` against `query`, best first, capped at [`MAX_RESULTS`].
fn rank_entries(entries: &[Entry], query: &str) -> Vec<Completion> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &Entry)> = entries
        .iter()
        .filter_map(|entry| score(entry, &query).map(|s| (s, entry.relative.len(), entry)))
        .collect();
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then_with(|| a.2.relative.cmp(&b.2.relative))
    });
    scored
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, _, entry)| {
            let completion = Completion::new(entry.relative.clone());
            if entry.is_dir {
                completion.with_detail("dir")
            } else {
                completion
            }
        })
        .collect()
}

/// Lower is better. `None` means no match.
fn score(entry: &Entry, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let folded = &entry.folded;
    if folded.starts_with(query) {
        return Some(0);
    }
    let name = folded
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(folded);
    if name.starts_with(query) {
        return Some(1);
    }
    if folded.contains(query) {
        return Some(2);
    }
    is_subsequence(query, folded).then_some(3)
}

/// Whether every char of `needle` appears in `haystack`, in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = needle.chars();
    let mut want = chars.next();
    for ch in haystack.chars() {
        if want == Some(ch) {
            want = chars.next();
            if want.is_none() {
                return true;
            }
        }
    }
    want.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("docs/my notes")).unwrap();
        std::fs::create_dir_all(root.join("日本語")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/nested/mod.rs"), "").unwrap();
        std::fs::write(root.join("src/nested/.gitignore"), "secret.rs\n").unwrap();
        std::fs::write(root.join("src/nested/secret.rs"), "").unwrap();
        std::fs::write(root.join("target/debug/bin"), "").unwrap();
        std::fs::write(root.join("build.log"), "").unwrap();
        std::fs::write(root.join(".git/HEAD"), "").unwrap();
        std::fs::write(root.join("docs/my notes/read me.md"), "").unwrap();
        std::fs::write(root.join("日本語/メモ.txt"), "").unwrap();
        std::fs::write(root.join("README.md"), "").unwrap();
        dir
    }

    fn inserts(items: &[Completion]) -> Vec<&str> {
        items.iter().map(|c| c.insert.as_str()).collect()
    }

    #[test]
    fn listing_honours_ignore_rules_at_every_level_and_skips_git() {
        let ws = workspace();
        let source = WorkspaceFiles::new(ws.path());
        let all = source.rank("", &CancelToken::new());
        let paths = inserts(&all);
        assert!(paths.contains(&"src/main.rs"), "{paths:?}");
        assert!(paths.contains(&"src/nested/mod.rs"), "{paths:?}");
        assert!(
            paths.contains(&"src/"),
            "directories are offered: {paths:?}"
        );
        assert!(paths.contains(&"docs/my notes/read me.md"), "{paths:?}");
        assert!(paths.contains(&"日本語/メモ.txt"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with("target")), "{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with(".git/")), "{paths:?}");
        assert!(!paths.contains(&"build.log"), "root rule: {paths:?}");
        assert!(
            !paths.contains(&"src/nested/secret.rs"),
            "nested rule: {paths:?}"
        );
        let dir = all.iter().find(|c| c.insert == "src/").unwrap();
        assert_eq!(dir.detail, "dir");
    }

    #[test]
    fn ranking_prefers_path_prefix_then_name_then_substring_then_subsequence() {
        let ws = workspace();
        let source = WorkspaceFiles::new(ws.path());
        let cancel = CancelToken::new();
        assert_eq!(inserts(&source.rank("src/ma", &cancel)), ["src/main.rs"]);
        // Name prefix beats substring: `mod.rs` starts with `mo`; nothing
        // else does, but `README.md`? no. Check ordering with `re`.
        let re = source.rank("re", &cancel);
        let paths = inserts(&re);
        assert_eq!(paths[0], "README.md", "name prefix first: {paths:?}");
        assert!(paths.contains(&"docs/my notes/read me.md"), "{paths:?}");
        // Case-insensitive, Unicode. `readme` is also a subsequence of
        // `read me.md`, which ranks behind the prefix match.
        assert_eq!(
            inserts(&source.rank("readme", &cancel)),
            ["README.md", "docs/my notes/read me.md"]
        );
        assert_eq!(inserts(&source.rank("メモ", &cancel)), ["日本語/メモ.txt"]);
        // Subsequence.
        assert!(inserts(&source.rank("smr", &cancel)).contains(&"src/main.rs"));
        // Spaces in the query match spaces in the path.
        assert_eq!(
            inserts(&source.rank("my notes/read", &cancel)),
            ["docs/my notes/read me.md"]
        );
        assert!(source.rank("zzzzzz", &cancel).is_empty());
    }

    #[test]
    fn a_cancelled_walk_returns_nothing_and_the_cap_holds() {
        let ws = workspace();
        let source = WorkspaceFiles::new(ws.path());
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(source.rank("", &cancel).is_empty());

        let capped = WorkspaceFiles::new(ws.path()).with_max_entries(3);
        assert_eq!(capped.rank("", &CancelToken::new()).len(), 3);
    }

    #[test]
    fn the_listing_is_cached_briefly() {
        let ws = workspace();
        let source = WorkspaceFiles::new(ws.path());
        let cancel = CancelToken::new();
        let before = source.rank("", &cancel).len();
        std::fs::write(ws.path().join("late.rs"), "").unwrap();
        assert_eq!(source.rank("", &cancel).len(), before, "served from cache");
        // Expire the cache by hand and the new file appears.
        *source.inner.cache.lock().unwrap() = None;
        assert_eq!(source.rank("", &cancel).len(), before + 1);
    }

    #[tokio::test]
    async fn complete_runs_off_the_async_thread() {
        let ws = workspace();
        let source = WorkspaceFiles::new(ws.path());
        let items = source.complete("lib".into(), CancelToken::new()).await;
        assert_eq!(inserts(&items), ["src/lib.rs"]);
    }

    #[test]
    fn a_missing_root_lists_nothing() {
        let source = WorkspaceFiles::new("/definitely/not/here");
        assert!(source.rank("", &CancelToken::new()).is_empty());
    }
}
