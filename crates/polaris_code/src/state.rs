//! The agent's own state directory inside a workspace.
//!
//! `polaris-code` keeps per-repository state under `<working_dir>/.polaris`:
//! saved sessions ([`FileStore`](polaris::sessions::FileStore)) and the rewind
//! journal ([`EditJournal`](crate::terminal::EditJournal)). Keeping it beside
//! the repository is deliberate — a session's scope is the repository it was
//! opened over, which is what `/sessions` and `/resume` browse.
//!
//! That state is the agent's bookkeeping, not repository content, so git must
//! never collect it. It otherwise rides along in anything built from the
//! working tree: a `git commit -a` after an interactive run, a CI bot's diff,
//! an automated caller that stages everything (`git add -A`) and diffs against
//! the commit the agent started from.
//!
//! [`state_dir`] creates the directory and writes a self-ignoring `.gitignore`
//! into it — the marker `python -m venv` writes for the same reason. Git still
//! descends into `.polaris`, reads the marker, and then ignores every entry
//! including the marker itself. The state stays on disk and fully readable for
//! debugging a run; `git status`, `git add -A` and every diff behave as if it
//! were not there.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Directory name the agent keeps its per-workspace state under.
pub const STATE_DIR_NAME: &str = ".polaris";

/// The marker [`ensure_ignored`] writes. `*` matches every entry in the
/// directory, this file included, so nothing under it is ever staged.
const IGNORE_MARKER: &[u8] = b"\
# polaris-code keeps its own state here: saved sessions and the rewind
# journal. It is agent bookkeeping, not repository content, so `*` hides the
# whole directory - this file included - from git.
*
";

/// Return `<workspace>/.polaris`, creating it and its self-ignore marker.
///
/// Call this before anything writes under the directory. It is idempotent and
/// cheap enough to call on every path that needs the directory.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the directory or its marker cannot
/// be created — a read-only workspace, or a file already occupying the name.
pub fn state_dir(workspace: &Path) -> io::Result<PathBuf> {
    let dir = workspace.join(STATE_DIR_NAME);
    ensure_ignored(&dir)?;
    Ok(dir)
}

/// Create `dir` and keep it out of git by writing the self-ignore marker.
///
/// An existing marker is left exactly as it is, so a workspace owner who has
/// edited it keeps their version. Concurrent callers race harmlessly: the
/// marker is created exclusively and an already-created one is not an error.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if `dir` or the marker cannot be
/// created.
pub fn ensure_ignored(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"))
    {
        Ok(mut marker) => marker.write_all(IGNORE_MARKER),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_written_once_and_never_overwritten() {
        let workspace = tempfile::tempdir().unwrap();
        let dir = state_dir(workspace.path()).unwrap();
        assert_eq!(dir, workspace.path().join(STATE_DIR_NAME));
        assert_eq!(
            std::fs::read(dir.join(".gitignore")).unwrap(),
            IGNORE_MARKER
        );

        std::fs::write(dir.join(".gitignore"), b"mine\n").unwrap();
        state_dir(workspace.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.join(".gitignore")).unwrap(),
            b"mine\n",
            "an existing marker belongs to the workspace owner"
        );
    }

    #[test]
    fn a_file_occupying_the_directory_name_is_an_error_not_a_panic() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join(STATE_DIR_NAME), b"occupied").unwrap();
        assert!(state_dir(workspace.path()).is_err());
    }
}
