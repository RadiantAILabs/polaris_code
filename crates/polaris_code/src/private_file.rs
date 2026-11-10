//! Owner-only credential files: no symlinks, Unix ownership checks, and
//! durable atomic replacement.
//!
//! On Unix every file is opened with `O_NOFOLLOW | O_NONBLOCK` and refused
//! unless it is a regular file owned by the current user with no group or world
//! permissions. Other platforms have no equivalent check, so files are opened
//! as they are; callers that require owner-only storage use [`require_unix`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

/// Largest credential file read.
const MAX_BYTES: usize = 1024 * 1024;

/// Why a credential path was refused. Carried inside the returned [`io::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The path is a symbolic link.
    Symlink,
    /// The path is not a regular file, such as a FIFO or directory.
    NotRegular,
    /// The file grants group or world permissions; holds its mode bits.
    Exposed(u32),
    /// The file belongs to another user.
    NotOwned,
    /// The file exceeds [`MAX_BYTES`].
    TooLarge,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Symlink => f.write_str("credential path is a symbolic link"),
            Self::NotRegular => f.write_str("credential path is not a regular file"),
            Self::Exposed(mode) => write!(
                f,
                "credential file can be read by other users (mode {:03o}); run chmod 600 on it",
                mode & 0o777
            ),
            Self::NotOwned => f.write_str("credential file is owned by another user"),
            Self::TooLarge => f.write_str("credential file exceeds 1 MiB"),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for io::Error {
    fn from(refusal: Refusal) -> Self {
        Self::other(refusal)
    }
}

/// The [`Refusal`] behind `error`, if it was one.
pub(crate) fn refusal(error: &io::Error) -> Option<Refusal> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Refusal>())
        .copied()
}

/// Fail on platforms without Unix owner-only permissions.
pub(crate) fn require_unix() -> io::Result<()> {
    if cfg!(unix) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private credential files require Unix owner-only permissions",
        ))
    }
}

/// Open a private file for reading, or for writing and creation when `create`.
#[cfg(unix)]
pub(crate) fn open(path: &Path, create: bool) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        // O_NOFOLLOW refuses a symlink; O_NONBLOCK keeps a FIFO from blocking.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) => Refusal::Symlink.into(),
            _ => error,
        })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Refusal::NotRegular.into());
    }
    // SAFETY: geteuid has no arguments or memory preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(Refusal::NotOwned.into());
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(Refusal::Exposed(metadata.mode()).into());
    }
    Ok(file)
}

/// Open a file for reading, or for writing and creation when `create`.
#[cfg(not(unix))]
pub(crate) fn open(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Refusal::NotRegular.into());
    }
    Ok(file)
}

/// Read a private file, or `None` when it does not exist.
pub(crate) fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match open(path, false) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(Refusal::TooLarge.into());
    }
    Ok(Some(bytes))
}

/// Create `path` and missing parents, owner-only on Unix. Refuses a symlink.
pub(crate) fn directory(path: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other("credential directory is a symbolic link"));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Replace `path` with `bytes` atomically and durably.
///
/// An existing file must pass the same checks as [`read`]. The parent directory
/// is synced after the rename so the replacement survives a crash.
pub(crate) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    read(path)?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    directory(parent)?;
    // NamedTempFile restricts permissions before any credentials are written.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|err| err.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn read_rejects_fifo_without_blocking() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("credentials");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
        // SAFETY: name is NUL-terminated and remains valid for the call.
        assert_eq!(
            unsafe { libc::mkfifo(name.as_ptr(), 0o600) },
            0,
            "mkfifo should create the test FIFO"
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            sender
                .send(read(&reader_path))
                .expect("test receiver is alive");
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        // Release a blocked reader so the test can join its thread on failure.
        let release = result.is_err().then(|| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("open FIFO to release the reader")
        });
        reader.join().expect("reader thread does not panic");
        drop(release);
        let error = result
            .expect("opening a credential FIFO must not block")
            .expect_err("a FIFO is not a credential file");
        assert_eq!(
            refusal(&error),
            Some(Refusal::NotRegular),
            "a FIFO is refused as not regular"
        );
    }

    #[test]
    fn write_refuses_symlink_and_exposed_files() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let target = dir.path().join("target");
        std::fs::write(&target, b"secret-token").expect("write symlink target");
        let path = dir.path().join("credentials");
        std::os::unix::fs::symlink(&target, &path).expect("create symlink");
        let error = write(&path, b"replacement").expect_err("a symlink is not replaced");
        assert_eq!(
            refusal(&error),
            Some(Refusal::Symlink),
            "O_NOFOLLOW identifies the symlink"
        );
        assert_eq!(
            std::fs::read(&target).expect("read symlink target"),
            b"secret-token",
            "the symlink target is untouched"
        );

        std::fs::remove_file(&path).expect("remove symlink");
        write(&path, b"first").expect("write a new private file");
        let mode = std::fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "new files are owner-only");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen permissions");
        let error = read(&path).expect_err("an exposed file is refused");
        assert!(
            matches!(refusal(&error), Some(Refusal::Exposed(_))),
            "group or world bits are reported as exposure"
        );
        assert!(
            error.to_string().contains("chmod 600"),
            "the message names the fix"
        );
    }
}
