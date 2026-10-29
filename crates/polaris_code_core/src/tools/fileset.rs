//! Choosing which files a tool looks at.
//!
//! [`CodeTools::search`](super::CodeTools) and [`CodeTools::glob`](super::CodeTools)
//! differ only in what they do with a file once they have it, so they share one
//! selection pass: walk down from a start path inside the sandbox, honour the
//! repository's own `.gitignore` files unless the caller opts out, keep only
//! paths a caller-supplied [`PathGlob`] accepts, and stop at documented limits
//! so the result a model sees stays bounded.
//!
//! The ignore semantics are [`IgnoreRules`]', already used by the `@`-mention
//! walk — the same subset of the `.gitignore` format, the same
//! deeper-file-overrides-shallower precedence, the same "a ruled-out directory
//! is never descended into". Reusing it is also why [`PathGlob`] speaks that
//! dialect rather than inventing a second one: a coding agent that can write a
//! `.gitignore` line can write a file filter.

use crate::terminal::IgnoreRules;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// A compiled path filter in the repository's own `.gitignore` pattern dialect.
///
/// `*` matches any run of characters within one path component, `?` one
/// character, `[a-z]` a class, and `**` any number of components. A pattern
/// with no `/` matches a file's name at any depth (`*.rs`); a pattern
/// containing one is anchored to the directory the walk started from
/// (`src/**/*.rs`); a trailing `/` selects everything beneath a directory
/// (`tests/`).
#[derive(Debug, Clone)]
pub struct PathGlob {
    rules: IgnoreRules,
    pattern: String,
}

impl PathGlob {
    /// Compiles `pattern`, or explains why it selects nothing.
    ///
    /// The dialect has no syntax errors — every byte sequence is some literal —
    /// so the diagnostics here are for patterns that would silently match
    /// nothing: an empty one, a `!` negation (this is a selector, not an
    /// ignore file), a `#` comment, and an embedded newline.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming what about `pattern` cannot be used.
    pub fn compile(pattern: &str) -> Result<Self, String> {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return Err("glob pattern is empty; omit it to select every file".to_string());
        }
        if trimmed.contains('\n') {
            return Err(format!(
                "glob pattern '{pattern}' contains a newline; pass one pattern, \
                 and use '{{…}}'-free alternatives like '**/*.ts' instead"
            ));
        }
        if let Some(rest) = trimmed.strip_prefix('!') {
            return Err(format!(
                "glob pattern '{pattern}' starts with '!', which negates a rule in a \
                 .gitignore but selects nothing here; pass '{rest}' to select those \
                 files, or narrow with the path argument instead"
            ));
        }
        if trimmed.starts_with('#') {
            return Err(format!(
                "glob pattern '{pattern}' starts with '#', which is a comment in the \
                 .gitignore dialect this argument uses; drop the '#'"
            ));
        }
        let rules = IgnoreRules::parse(trimmed);
        if rules.is_empty() {
            return Err(format!(
                "glob pattern '{pattern}' selects nothing; try a name pattern like \
                 '*.rs' or an anchored one like 'src/**/*.rs'"
            ));
        }
        Ok(Self {
            rules,
            pattern: trimmed.to_string(),
        })
    }

    /// Whether `relative` (a `/`-separated path relative to the walk root) is
    /// selected.
    #[must_use]
    pub fn matches(&self, relative: &str) -> bool {
        self.rules.verdict(relative, false) == Some(true)
    }

    /// The pattern as compiled, for diagnostics.
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// The bounds a selection stops at, so one tool call cannot walk forever or
/// read a gigabyte into the context window.
#[derive(Debug, Clone, Copy)]
pub struct WalkLimits {
    /// Stop selecting after this many files.
    pub max_files: usize,
    /// Skip any file larger than this, counting it in
    /// [`Selection::skipped_large`].
    pub max_file_bytes: u64,
}

/// The files a walk selected, plus what it left out.
#[derive(Debug, Default)]
pub struct Selection {
    /// Absolute paths, in walk order (name-sorted, parents before children).
    pub files: Vec<PathBuf>,
    /// Whether [`WalkLimits::max_files`] cut the walk short.
    pub stopped_at_limit: bool,
    /// How many files [`WalkLimits::max_file_bytes`] excluded.
    pub skipped_large: usize,
}

/// Selects the files under `start` a tool should look at.
///
/// `sandbox_root` is the configured working directory: ignore files are read
/// from it down to `start`, so a rule the repository states at its root still
/// applies when the caller searches one subtree. `respect_ignore` false keeps
/// every ignore file out of the decision — `.git` is skipped either way, being
/// git's own storage rather than something the repository asked to ignore.
///
/// A `start` that names a file selects exactly that file (an explicitly named
/// path is never second-guessed by an ignore rule), subject to `glob` matching
/// its name.
///
/// This blocks on the filesystem; callers in async context should wrap it in
/// [`spawn_blocking`](tokio::task::spawn_blocking).
#[must_use]
pub fn select_files(
    sandbox_root: &Path,
    start: &Path,
    glob: Option<&PathGlob>,
    respect_ignore: bool,
    limits: WalkLimits,
) -> Selection {
    let mut selection = Selection::default();

    if start.is_file() {
        let name = start
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if glob.is_none_or(|g| g.matches(&name)) {
            selection.files.push(start.to_path_buf());
        }
        return selection;
    }

    // Ignore rules per directory, loaded as the walk enters each one. The
    // filter sees a directory before any of its children, so a directory's own
    // `.gitignore` is in place by the time they are judged.
    let mut rules = if respect_ignore {
        seed_rules(sandbox_root, start)
    } else {
        Vec::new()
    };
    let walker = WalkDir::new(start)
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
            if !respect_ignore {
                return true;
            }
            // Rules apply from the root down; a deeper file's verdict overrides
            // a shallower one, matching git's precedence.
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

    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(start) else {
            continue;
        };
        if glob.is_some_and(|g| !g.matches(&slashed(relative))) {
            continue;
        }
        if entry
            .metadata()
            .is_ok_and(|meta| meta.len() > limits.max_file_bytes)
        {
            selection.skipped_large += 1;
            continue;
        }
        if selection.files.len() >= limits.max_files {
            selection.stopped_at_limit = true;
            break;
        }
        selection.files.push(entry.path().to_path_buf());
    }

    selection
}

/// The `.gitignore` files from `sandbox_root` down to `start`, paired with the
/// directory each was read from.
fn seed_rules(sandbox_root: &Path, start: &Path) -> Vec<(PathBuf, IgnoreRules)> {
    let mut dirs = vec![sandbox_root.to_path_buf()];
    if let Ok(relative) = start.strip_prefix(sandbox_root) {
        let mut current = sandbox_root.to_path_buf();
        for component in relative.components() {
            if matches!(component, Component::Normal(_)) {
                current.push(component);
                dirs.push(current.clone());
            }
        }
    }
    dirs.into_iter()
        .filter_map(|dir| {
            let rules = IgnoreRules::load(&dir);
            (!rules.is_empty()).then_some((dir, rules))
        })
        .collect()
}

/// A relative path as the `/`-separated string the ignore rules match against.
pub(crate) fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> WalkLimits {
        WalkLimits {
            max_files: 1000,
            max_file_bytes: 1 << 20,
        }
    }

    fn names(selection: &Selection, root: &Path) -> Vec<String> {
        selection
            .files
            .iter()
            .map(|p| slashed(p.strip_prefix(root).unwrap()))
            .collect()
    }

    /// A tree with a root `.gitignore`, a nested one, and a `.git` directory.
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "vendor/\n*.log\n").unwrap();
        std::fs::write(root.join("src/.gitignore"), "generated.rs\n").unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("a.log"), "noise\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(root.join("src/generated.rs"), "fn gen() {}\n").unwrap();
        std::fs::write(root.join("src/deep/c.rs"), "fn c() {}\n").unwrap();
        std::fs::write(root.join("vendor/v.rs"), "fn v() {}\n").unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        dir
    }

    #[test]
    fn ignored_paths_are_excluded_by_default_and_included_on_request() {
        let dir = tree();
        let root = dir.path();

        let default = select_files(root, root, None, true, limits());
        assert_eq!(
            names(&default, root),
            vec![
                ".gitignore",
                "a.rs",
                "src/.gitignore",
                "src/b.rs",
                "src/deep/c.rs"
            ],
            "the root .gitignore, the nested one and .git all apply"
        );

        let all = select_files(root, root, None, false, limits());
        let listed = names(&all, root);
        assert!(listed.contains(&"vendor/v.rs".to_string()), "{listed:?}");
        assert!(listed.contains(&"a.log".to_string()), "{listed:?}");
        assert!(
            !listed.iter().any(|p| p.starts_with(".git/")),
            ".git is never walked: {listed:?}"
        );
    }

    #[test]
    fn a_root_rule_still_applies_when_the_walk_starts_deeper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/build")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(root.join("src/keep.rs"), "").unwrap();
        std::fs::write(root.join("src/build/out.rs"), "").unwrap();

        let selection = select_files(root, &root.join("src"), None, true, limits());
        assert_eq!(names(&selection, root), vec!["src/keep.rs"]);
    }

    #[test]
    fn a_glob_selects_a_subset_relative_to_the_start() {
        let dir = tree();
        let root = dir.path();
        let glob = PathGlob::compile("deep/*.rs").unwrap();
        let selection = select_files(root, &root.join("src"), Some(&glob), true, limits());
        assert_eq!(names(&selection, root), vec!["src/deep/c.rs"]);

        let any_rs = PathGlob::compile("*.rs").unwrap();
        let selection = select_files(root, root, Some(&any_rs), true, limits());
        assert_eq!(
            names(&selection, root),
            vec!["a.rs", "src/b.rs", "src/deep/c.rs"],
            "an unanchored name pattern matches at any depth"
        );
    }

    #[test]
    fn an_explicit_file_path_is_selected_even_when_ignored() {
        let dir = tree();
        let root = dir.path();
        let selection = select_files(root, &root.join("a.log"), None, true, limits());
        assert_eq!(names(&selection, root), vec!["a.log"]);
    }

    #[test]
    fn limits_stop_the_walk_and_skip_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for n in 0..5 {
            std::fs::write(root.join(format!("f{n}.txt")), "x").unwrap();
        }
        // Sorts first, so the walk sees it before the file cap bites.
        std::fs::write(root.join("a_huge.txt"), "y".repeat(4096)).unwrap();

        let selection = select_files(
            root,
            root,
            None,
            true,
            WalkLimits {
                max_files: 3,
                max_file_bytes: 1024,
            },
        );
        assert_eq!(selection.files.len(), 3);
        assert!(selection.stopped_at_limit);
        assert_eq!(selection.skipped_large, 1);
    }

    #[test]
    fn malformed_globs_are_named_rather_than_silently_empty() {
        for (pattern, expected) in [
            ("", "empty"),
            ("   ", "empty"),
            ("!*.rs", "starts with '!'"),
            ("#*.rs", "starts with '#'"),
            ("*.rs\n*.ts", "newline"),
        ] {
            let err = PathGlob::compile(pattern).unwrap_err();
            assert!(err.contains(expected), "pattern {pattern:?} gave: {err}");
        }
    }
}
