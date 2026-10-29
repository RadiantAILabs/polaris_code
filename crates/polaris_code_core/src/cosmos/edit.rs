//! The real coding actions and verifiers behind the coding embodiment.
//!
//! Everything in this module touches the actual working tree. The graphs in
//! [`super::topology`] call these functions, and the verifiers here are what
//! turn a finished graph run into
//! [`OutcomeObservation`](polaris_cosmos::contracts::OutcomeObservation)s.
//!
//! Sandboxing is not reimplemented: every path goes through
//! [`CodeToolsConfig::resolve_path`], the same check the agent-facing
//! `edit_file` tool uses, so a coding topology cannot write outside the
//! configured working directory. The exact-string-replace contract
//! (unique match required, whitespace significant) mirrors
//! [`CodeTools::edit_file`](crate::tools::CodeTools) as well; it is restated
//! here because a graph system needs a typed result rather than the
//! diff-formatted string a model-facing tool returns.

use std::fmt;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::tools::CodeToolsConfig;

/// One unimplemented site found in the working tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StubSite {
    /// Path relative to the sandbox root, with `/` separators.
    pub path: String,
    /// One-based line number of the marker.
    pub line: usize,
}

/// Why a coding action could not be applied.
#[derive(Debug)]
#[non_exhaustive]
pub enum EditError {
    /// The requested path resolved outside the sandbox root.
    OutsideSandbox {
        /// The offending relative path.
        path: String,
    },
    /// The file could not be read or written.
    Io {
        /// The path being read or written.
        path: String,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// The search text does not occur in the file.
    NotFound {
        /// The path that was searched.
        path: String,
    },
    /// The search text occurs more than once, so the edit is not a unique edit.
    Ambiguous {
        /// The path that was searched.
        path: String,
        /// How many times the search text occurred.
        matches: usize,
    },
    /// A new file was requested at a path that already exists.
    AlreadyExists {
        /// The path that already existed.
        path: String,
    },
}

impl fmt::Display for EditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideSandbox { path } => {
                write!(formatter, "path '{path}' escapes the sandbox root")
            }
            Self::Io { path, source } => write!(formatter, "{path}: {source}"),
            Self::NotFound { path } => write!(
                formatter,
                "search text not found in {path} (whitespace must match exactly)"
            ),
            Self::Ambiguous { path, matches } => write!(
                formatter,
                "search text is not unique in {path} ({matches} matches)"
            ),
            Self::AlreadyExists { path } => {
                write!(formatter, "{path} already exists")
            }
        }
    }
}

impl std::error::Error for EditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// What applying one exact-string edit did to the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// The file was rewritten.
    Applied {
        /// Path relative to the sandbox root.
        path: String,
        /// Bytes in the file after the edit.
        bytes_after: usize,
    },
    /// The replacement was already present and the search text was gone, so
    /// the tree already satisfied the request and nothing was written.
    ///
    /// A bounded retry loop re-entering its act stage relies on this: a second
    /// attempt is a no-op rather than a spurious [`EditError::NotFound`].
    AlreadySatisfied {
        /// Path relative to the sandbox root.
        path: String,
    },
}

impl EditOutcome {
    /// Returns the path the outcome refers to.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Applied { path, .. } | Self::AlreadySatisfied { path } => path,
        }
    }

    /// Returns whether this outcome rewrote the file.
    #[must_use]
    pub const fn wrote(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
}

fn resolve(config: &CodeToolsConfig, path: &str) -> Result<PathBuf, EditError> {
    config
        .resolve_path(path)
        .ok_or_else(|| EditError::OutsideSandbox {
            path: path.to_owned(),
        })
}

/// Reads one sandbox-relative file to a string.
pub fn read_relative(config: &CodeToolsConfig, path: &str) -> Result<String, EditError> {
    let resolved = resolve(config, path)?;
    std::fs::read_to_string(&resolved).map_err(|source| EditError::Io {
        path: path.to_owned(),
        source,
    })
}

/// What an exact-string match decides against `original`'s current content,
/// without writing anything.
enum EditCheck {
    /// `find` occurs exactly once: applying would rewrite the file.
    WouldApply,
    /// `replace` is already present and applying again would duplicate it.
    AlreadySatisfied,
}

/// Decides what one exact-string edit would do against `original`, the
/// single source of truth [`apply_exact_edit`] and [`verify_exact_edit`]
/// both read through, so a caller checking a proposed edit before accepting
/// it agrees with what actually applying it later would do.
fn check_exact_edit(
    original: &str,
    find: &str,
    replace: &str,
    path: &str,
) -> Result<EditCheck, EditError> {
    // A replacement may retain its own search text (for example, adding an
    // option beside an existing registration). Check the complete replacement
    // first so a resumed proposal cannot insert that registration again.
    if original.contains(replace) && (replace.contains(find) || !original.contains(find)) {
        return Ok(EditCheck::AlreadySatisfied);
    }
    let matches = original.matches(find).count();
    if matches == 0 {
        return if original.contains(replace) {
            Ok(EditCheck::AlreadySatisfied)
        } else {
            Err(EditError::NotFound {
                path: path.to_owned(),
            })
        };
    }
    if matches > 1 {
        return Err(EditError::Ambiguous {
            path: path.to_owned(),
            matches,
        });
    }
    Ok(EditCheck::WouldApply)
}

/// Applies one exact-string replacement, requiring a unique match.
///
/// Returns [`EditOutcome::AlreadySatisfied`] when `replace` is already present,
/// including when it retains `find`, so re-running the request is idempotent.
pub fn apply_exact_edit(
    config: &CodeToolsConfig,
    path: &str,
    find: &str,
    replace: &str,
) -> Result<EditOutcome, EditError> {
    let resolved = resolve(config, path)?;
    let original = std::fs::read_to_string(&resolved).map_err(|source| EditError::Io {
        path: path.to_owned(),
        source,
    })?;

    match check_exact_edit(&original, find, replace, path)? {
        EditCheck::AlreadySatisfied => Ok(EditOutcome::AlreadySatisfied {
            path: path.to_owned(),
        }),
        EditCheck::WouldApply => {
            let updated = original.replacen(find, replace, 1);
            std::fs::write(&resolved, &updated).map_err(|source| EditError::Io {
                path: path.to_owned(),
                source,
            })?;
            Ok(EditOutcome::Applied {
                path: path.to_owned(),
                bytes_after: updated.len(),
            })
        }
    }
}

/// Checks whether one exact-string edit would apply cleanly against `path`'s
/// current content, without writing anything: the file must exist and be
/// readable, and `find` must either occur exactly once or already be
/// resolved (see [`EditOutcome::AlreadySatisfied`]). Shares
/// [`check_exact_edit`] with [`apply_exact_edit`], so a caller validating a
/// model-proposed edit before accepting it can never disagree with what
/// applying it later actually does.
pub fn verify_exact_edit(
    config: &CodeToolsConfig,
    path: &str,
    find: &str,
    replace: &str,
) -> Result<(), EditError> {
    let resolved = resolve(config, path)?;
    let original = std::fs::read_to_string(&resolved).map_err(|source| EditError::Io {
        path: path.to_owned(),
        source,
    })?;
    check_exact_edit(&original, find, replace, path).map(|_| ())
}

/// Writes a new sandbox-relative file, refusing to overwrite an existing one.
///
/// Creates any missing parent directories inside the sandbox first, so a new
/// test file can land under a path that does not exist yet (`tests/new.rs`
/// in a tree with no `tests/` directory).
///
/// # Errors
///
/// Returns [`EditError::OutsideSandbox`] if `path` escapes the sandbox,
/// [`EditError::AlreadyExists`] if a file is already there, or
/// [`EditError::Io`] if a directory or the file itself cannot be written.
pub fn create_file(config: &CodeToolsConfig, path: &str, content: &str) -> Result<(), EditError> {
    let resolved = resolve(config, path)?;
    if resolved.exists() {
        return Err(EditError::AlreadyExists {
            path: path.to_owned(),
        });
    }
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent).map_err(|source| EditError::Io {
            path: path.to_owned(),
            source,
        })?;
    }
    std::fs::write(&resolved, content).map_err(|source| EditError::Io {
        path: path.to_owned(),
        source,
    })
}

/// Scans the working tree for remaining stub markers.
///
/// A named use of [`scan_occurrences`]: a stub site is just an occurrence of
/// the configured marker. Kept as its own name because the coverage
/// topologies are about stubs specifically, not about literals in general.
#[must_use]
pub fn scan_stub_sites(root: &Path, marker: &str) -> Vec<StubSite> {
    scan_occurrences(root, marker)
}

/// Scans the working tree for every line containing `needle`.
///
/// Skips hidden entries and `target/`, matching the `search` tool's traversal
/// rules so a topology sees what the agent would see. Results are sorted,
/// which is what makes a scanning topology's scope deterministic. The scan is
/// literal — not a regex — and reads only, so it is safe for a topology that
/// promises to leave the tree untouched.
#[must_use]
pub fn scan_occurrences(root: &Path, needle: &str) -> Vec<StubSite> {
    let mut sites = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            // Depth zero is the sandbox root itself. It must never be pruned:
            // a root whose own name begins with a dot (every `tempfile`
            // directory, for one) would otherwise prune the entire walk.
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !name.starts_with('.') && name != "target"
        })
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        for (index, line) in contents.lines().enumerate() {
            if line.contains(needle) {
                sites.push(StubSite {
                    path: relative.clone(),
                    line: index + 1,
                });
            }
        }
    }
    sites.sort();
    sites
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(contents: &str) -> (tempfile::TempDir, CodeToolsConfig) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("lib.rs"), contents).expect("seed file");
        let config = CodeToolsConfig::new(dir.path());
        (dir, config)
    }

    #[test]
    fn create_file_writes_a_new_path_including_missing_parents() {
        let (_dir, config) = workspace("fn add() { todo!() }\n");
        create_file(&config, "tests/new.rs", "// a new test\n").expect("creates the file");
        assert_eq!(
            read_relative(&config, "tests/new.rs").expect("read back"),
            "// a new test\n"
        );
    }

    #[test]
    fn create_file_refuses_to_overwrite_an_existing_path() {
        let (_dir, config) = workspace("fn add() { todo!() }\n");
        let error =
            create_file(&config, "lib.rs", "overwritten").expect_err("lib.rs already exists");
        assert!(matches!(error, EditError::AlreadyExists { .. }));
        assert_eq!(
            read_relative(&config, "lib.rs").expect("unchanged"),
            "fn add() { todo!() }\n"
        );
    }

    #[test]
    fn unique_match_is_applied() {
        let (_dir, config) = workspace("fn add() { todo!() }\n");
        let outcome =
            apply_exact_edit(&config, "lib.rs", "todo!()", "1 + 1").expect("edit applies");
        assert!(outcome.wrote());
        assert_eq!(
            read_relative(&config, "lib.rs").expect("read back"),
            "fn add() { 1 + 1 }\n"
        );
    }

    #[test]
    fn retained_find_does_not_duplicate_an_option_registration() {
        let original = "parser.add_argument('--old')\n";
        let replacement = "parser.add_argument('--old')\nparser.add_argument('--new')\n";
        let (_dir, config) = workspace(original);
        assert!(
            apply_exact_edit(&config, "lib.rs", original, replacement)
                .expect("first edit")
                .wrote()
        );
        verify_exact_edit(&config, "lib.rs", original, replacement).expect("valid retry");
        assert_eq!(
            apply_exact_edit(&config, "lib.rs", original, replacement).expect("retry"),
            EditOutcome::AlreadySatisfied {
                path: "lib.rs".to_owned()
            }
        );
        assert_eq!(read_relative(&config, "lib.rs").expect("read"), replacement);
    }

    #[test]
    fn verify_exact_edit_agrees_with_apply_on_a_clean_unique_match() {
        let (_dir, config) = workspace("fn add() { todo!() }\n");
        verify_exact_edit(&config, "lib.rs", "todo!()", "1 + 1").expect("would apply cleanly");
        assert_eq!(
            read_relative(&config, "lib.rs").expect("verify never writes"),
            "fn add() { todo!() }\n"
        );
    }

    #[test]
    fn verify_exact_edit_agrees_with_apply_on_an_already_satisfied_edit() {
        let (_dir, config) = workspace("fn add() { 1 + 1 }\n");
        verify_exact_edit(&config, "lib.rs", "todo!()", "1 + 1")
            .expect("already-satisfied edits verify cleanly too");
    }

    #[test]
    fn verify_exact_edit_rejects_an_ambiguous_match() {
        let (_dir, config) = workspace("todo!()\ntodo!()\n");
        let error = verify_exact_edit(&config, "lib.rs", "todo!()", "1")
            .expect_err("two matches are ambiguous");
        assert!(matches!(error, EditError::Ambiguous { matches: 2, .. }));
    }

    #[test]
    fn verify_exact_edit_rejects_a_missing_file() {
        let (_dir, config) = workspace("fn add() { todo!() }\n");
        let error = verify_exact_edit(&config, "missing.rs", "todo!()", "1 + 1")
            .expect_err("the file does not exist");
        assert!(matches!(error, EditError::Io { .. }));
    }

    #[test]
    fn ambiguous_match_is_refused_without_writing() {
        let (_dir, config) = workspace("todo!()\ntodo!()\n");
        let error = apply_exact_edit(&config, "lib.rs", "todo!()", "1")
            .expect_err("two matches are ambiguous");
        assert!(matches!(error, EditError::Ambiguous { matches: 2, .. }));
        assert_eq!(
            read_relative(&config, "lib.rs").expect("read back"),
            "todo!()\ntodo!()\n"
        );
    }

    #[test]
    fn reapplying_a_landed_edit_is_a_no_op() {
        let (_dir, config) = workspace("fn add() { 1 + 1 }\n");
        let outcome =
            apply_exact_edit(&config, "lib.rs", "todo!()", "1 + 1").expect("already satisfied");
        assert!(!outcome.wrote());
        assert!(matches!(outcome, EditOutcome::AlreadySatisfied { .. }));
    }

    #[test]
    fn missing_text_is_an_error_not_a_silent_pass() {
        let (_dir, config) = workspace("fn add() { unreachable!() }\n");
        let error = apply_exact_edit(&config, "lib.rs", "todo!()", "1 + 1")
            .expect_err("neither search nor replacement is present");
        assert!(matches!(error, EditError::NotFound { .. }));
    }

    #[test]
    fn escaping_the_sandbox_is_refused() {
        let (_dir, config) = workspace("fn add() { todo!() }\n");
        let error = apply_exact_edit(&config, "../outside.rs", "a", "b")
            .expect_err("path escapes the sandbox");
        assert!(matches!(error, EditError::OutsideSandbox { .. }));
    }

    #[test]
    fn stub_scan_finds_every_site_in_order_and_skips_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
        std::fs::create_dir_all(dir.path().join("target")).expect("target dir");
        std::fs::write(dir.path().join("src/a.rs"), "fn a() { todo!() }\n").expect("a");
        std::fs::write(
            dir.path().join("src/b.rs"),
            "fn b() { 1 }\nfn c() { todo!() }\n",
        )
        .expect("b");
        std::fs::write(dir.path().join("target/built.rs"), "todo!()\n").expect("built");
        let sites = scan_stub_sites(dir.path(), "todo!()");
        assert_eq!(
            sites,
            vec![
                StubSite {
                    path: "src/a.rs".to_owned(),
                    line: 1
                },
                StubSite {
                    path: "src/b.rs".to_owned(),
                    line: 2
                },
            ]
        );
    }
}
