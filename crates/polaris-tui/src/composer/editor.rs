//! Editor command parsing and temporary draft files; terminal modes belong to runtime.

use crate::TuiHandle;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// An external editor executable and literal argument vector.
///
/// The draft path is appended as one final argument. No shell is used and
/// no variables, substitutions, glob patterns or pipelines are expanded.
/// Editors that detach need a wait flag (for example `code --wait`).
#[derive(Debug, Clone)]
pub struct EditorCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl EditorCommand {
    /// Choose an executable without parsing shell syntax.
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Append one literal argument (such as `--wait`).
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Parse a command using whitespace, single/double quotes and backslash
    /// escaping. Quotes group arguments; shell operators have no special meaning.
    /// For paths with literal backslashes use single quotes or [`new`](Self::new).
    ///
    /// # Errors
    /// Rejects empty programs, unmatched quotes, dangling escapes and NUL bytes.
    pub fn parse(value: &str) -> io::Result<Self> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quote = None;
        let mut escaped = false;
        let mut started = false;
        for ch in value.chars() {
            if ch == '\0' {
                return Err(invalid("NUL in editor command"));
            }
            if escaped {
                word.push(ch);
                escaped = false;
                started = true;
            } else if ch == '\\' && quote != Some('\'') {
                escaped = true;
                started = true;
            } else if let Some(delimiter) = quote {
                if ch == delimiter {
                    quote = None;
                } else {
                    word.push(ch);
                }
            } else if ch == '\'' || ch == '"' {
                quote = Some(ch);
                started = true;
            } else if ch.is_whitespace() {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            } else {
                word.push(ch);
                started = true;
            }
        }
        if quote.is_some() || escaped {
            return Err(invalid("Unclosed quote or escape in editor command"));
        }
        if started {
            words.push(word);
        }
        let mut words = words.into_iter();
        let program = words
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("Empty editor command"))?;
        Ok(Self {
            program: program.into(),
            args: words.map(OsString::from).collect(),
        })
    }

    /// Read `VISUAL`, falling back to `EDITOR` when VISUAL is unset or blank.
    ///
    /// # Errors
    /// Returns an error for an invalid command, non-Unicode environment value,
    /// or when neither variable names an editor. Prefer [`new`](Self::new) for
    /// a non-Unicode executable path.
    pub fn from_env() -> io::Result<Self> {
        for name in ["VISUAL", "EDITOR"] {
            match std::env::var(name) {
                Ok(value) if !value.trim().is_empty() => return Self::parse(&value),
                Ok(_) | Err(std::env::VarError::NotPresent) => {}
                Err(error) => return Err(invalid(&error.to_string())),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Set VISUAL or EDITOR to edit the draft",
        ))
    }

    fn command(&self, path: &OsStr) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).arg(path);
        command
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

static NEXT_DRAFT: AtomicU64 = AtomicU64::new(0);
const MAX_EDITED_BYTES: u64 = 16 * 1024 * 1024;

struct DraftFile {
    directory: PathBuf,
    path: PathBuf,
}

impl DraftFile {
    fn create(draft: &str) -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let root = std::env::temp_dir();
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        for _ in 0..16 {
            let id = NEXT_DRAFT.fetch_add(1, Ordering::Relaxed);
            let directory = root.join(format!("polaris-draft-{}-{nonce}-{id}", std::process::id()));
            match builder.create(&directory) {
                Ok(()) => {
                    let file = Self {
                        path: directory.join("prompt.txt"),
                        directory,
                    };
                    let mut options = OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    options.open(&file.path)?.write_all(draft.as_bytes())?;
                    return Ok(file);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Cannot create a private editor draft",
        ))
    }

    fn read_after(&self, status: ExitStatus) -> io::Result<String> {
        if !status.success() {
            return Err(io::Error::other(format!(
                "Editor exited with {status}; draft unchanged"
            )));
        }
        // Editors commonly replace their file atomically. Reopen the path,
        // accepting regular files only, with a cap before allocating contents.
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.file_type().is_file() {
            return Err(invalid("Editor draft is not a regular file"));
        }
        let mut text = String::new();
        File::open(&self.path)?
            .take(MAX_EDITED_BYTES + 1)
            .read_to_string(&mut text)?;
        if text.len() as u64 > MAX_EDITED_BYTES {
            return Err(invalid("Editor draft exceeds 16 MiB"));
        }
        Ok(text)
    }
}

impl Drop for DraftFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        // Remove only the directory we created, never a recursive tree.
        let _ = fs::remove_dir(&self.directory);
    }
}

pub(crate) async fn edit_draft(
    handle: TuiHandle,
    editor: EditorCommand,
    draft: String,
) -> io::Result<String> {
    let file = tokio::task::spawn_blocking(move || DraftFile::create(&draft))
        .await
        .map_err(io::Error::other)??;
    let status = handle
        .run_in_terminal(editor.command(file.path.as_os_str()))
        .await?;
    tokio::task::spawn_blocking(move || file.read_after(status))
        .await
        .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_arguments_are_literal_and_quotes_preserve_spaces() {
        let editor = EditorCommand::parse(
            r#"'/Applications/My Editor' --wait "two words" '' '$(touch marker)' ';' '$HOME'"#,
        )
        .unwrap();
        let command = editor.command(OsStr::new("/tmp/draft with spaces.txt"));
        assert_eq!(command.get_program(), "/Applications/My Editor");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--wait",
                "two words",
                "",
                "$(touch marker)",
                ";",
                "$HOME",
                "/tmp/draft with spaces.txt"
            ]
        );
        for invalid in ["", "   ", "''", "'unterminated", "dangling\\", "bad\0value"] {
            assert!(EditorCommand::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn fake_editor_preserves_unicode_newlines_and_does_not_accept_failure_output() {
        let file = DraftFile::create("original 世界\n\n").unwrap();
        let path = file.path.clone();
        let directory = file.directory.clone();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let editor = EditorCommand::new("/bin/sh")
            .arg("-c")
            .arg("printf 'edited 世界\\n\\n' > \"$1\"")
            .arg("fake-editor");
        let status = editor.command(path.as_os_str()).status().unwrap();
        assert_eq!(file.read_after(status).unwrap(), "edited 世界\n\n");
        let failing = EditorCommand::new("/bin/sh")
            .arg("-c")
            .arg("printf 'discard this' > \"$1\"; exit 9")
            .arg("fake-editor");
        let status = failing.command(path.as_os_str()).status().unwrap();
        assert!(
            file.read_after(status)
                .unwrap_err()
                .to_string()
                .contains("draft unchanged")
        );
        drop(file);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn invalid_editor_output_is_rejected() {
        let file = DraftFile::create("draft").unwrap();
        let success = Command::new("/usr/bin/true").status().unwrap();
        fs::write(&file.path, [0xff]).unwrap();
        assert_eq!(
            file.read_after(success).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(&file.path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &file.path).unwrap();
        assert!(file.read_after(success).is_err());
    }
}
