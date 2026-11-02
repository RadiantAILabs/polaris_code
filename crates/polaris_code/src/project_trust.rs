//! Persisted trust for content a project controls.
//!
//! A repository can ship files that start programs (`.mcp.json`) or loosen
//! permissions (`polaris.toml`, `polaris.permissions.toml`). Opening it must not
//! act on them until the user has said so. One decision is stored per canonical
//! project root and per [`Subject`], bound to a SHA-256 digest of the exact
//! bytes or declaration it covers. Editing the content changes the digest, so
//! the earlier decision no longer applies.
//!
//! The store lives in the user configuration directory (`project-trust.json`),
//! never in the project, so a repository cannot grant itself trust. When no
//! store can be located or read, nothing is trusted.
//!
//! [`read_content`] is the shared way to read project files: it takes regular
//! files only, caps the bytes read, and returns the digest of the same bytes
//! the caller parses, so the digest never describes content other than what
//! was used.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use polaris_cosmos::contracts::ContentDigest;
use serde::{Deserialize, Serialize};

/// File name of the trust store inside the user configuration directory.
pub const STORE_FILE: &str = "project-trust.json";

/// Largest project or store file read while starting up.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

const STORE_VERSION: u32 = 1;

/// One piece of project-controlled content a trust decision can cover.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subject {
    /// The project's `polaris.toml`, where it asks for a more permissive
    /// permission mode or ceiling than the user's own settings allow.
    Settings,
    /// The project's permission rule file, `polaris.permissions.toml`.
    PermissionRules,
    /// One server declared in the project's `.mcp.json`.
    McpServer(String),
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Settings => f.write_str("settings"),
            Self::PermissionRules => f.write_str("permission-rules"),
            Self::McpServer(name) => write!(f, "mcp:{name}"),
        }
    }
}

/// Bytes read from a project file, with the digest of those same bytes.
#[derive(Debug, Clone)]
pub struct Content {
    /// The file's bytes, at most the limit given to [`read_content`].
    pub bytes: Vec<u8>,
    /// SHA-256 of [`bytes`](Self::bytes).
    pub digest: ContentDigest,
}

/// Why a project file could not be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReadError {
    /// The path is a directory, FIFO, device, socket, or a link to one.
    NotRegular(PathBuf),
    /// The file is longer than the allowed number of bytes.
    TooLarge {
        /// File that was too long.
        path: PathBuf,
        /// Allowed number of bytes.
        limit: u64,
    },
    /// The operating system refused to open or read the file.
    Io {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegular(path) => write!(f, "{}: not a regular file", path.display()),
            Self::TooLarge { path, limit } => {
                write!(f, "{}: larger than {limit} bytes", path.display())
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Read a regular file of at most `limit` bytes.
///
/// A symlink is followed, but only a regular file is accepted: a FIFO or device
/// (which could block or never end) is refused before it is read, and the
/// opened handle is checked again. At most `limit + 1` bytes are ever read.
/// Returns `Ok(None)` when the file does not exist.
///
/// # Errors
///
/// [`ReadError::NotRegular`] for anything but a regular file,
/// [`ReadError::TooLarge`] past `limit`, and [`ReadError::Io`] otherwise.
pub fn read_content(path: &Path, limit: u64) -> Result<Option<Content>, ReadError> {
    let io = |source| ReadError::Io {
        path: path.to_path_buf(),
        source,
    };
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(error)),
    };
    if !metadata.is_file() {
        return Err(ReadError::NotRegular(path.to_path_buf()));
    }
    let file = std::fs::File::open(path).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        return Err(ReadError::NotRegular(path.to_path_buf()));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > limit {
        return Err(ReadError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    let digest = ContentDigest::sha256(&bytes);
    Ok(Some(Content { bytes, digest }))
}

/// Why a trust decision could not be read or saved.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrustError {
    /// No user configuration directory could be located to hold the store.
    NoStore,
    /// The subject was not loaded by this process, so there is no digest to trust.
    NotObserved(Subject),
    /// The store could not be read.
    Read(ReadError),
    /// The store is not valid; it is left as it is rather than overwritten.
    Malformed(PathBuf),
    /// The store could not be written.
    Write {
        /// Store that could not be written.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStore => f.write_str(
                "cannot locate the user configuration directory; set POLARIS_CONFIG_DIR, \
                 XDG_CONFIG_HOME or HOME",
            ),
            Self::NotObserved(subject) => {
                write!(f, "this session did not load project content `{subject}`")
            }
            Self::Read(error) => write!(f, "cannot read the trust store: {error}"),
            Self::Malformed(path) => write!(
                f,
                "{}: not a valid trust store; fix or remove it",
                path.display()
            ),
            Self::Write { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for TrustError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, BTreeMap<String, ContentDigest>>,
}

/// The user's trust decisions for one project root.
///
/// Cloning shares the record of what this process loaded
/// ([`observe`](Self::observe)), so a command can trust exactly the content
/// startup used, not whatever is on disk by then.
///
/// # Examples
///
/// ```
/// use polaris_code::project_trust::{ProjectTrust, Subject};
/// use polaris_cosmos::contracts::ContentDigest;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let config = tempfile::tempdir()?;
/// let project = tempfile::tempdir()?;
/// let trust = ProjectTrust::new(Some(config.path().join("project-trust.json")), project.path());
///
/// let server = Subject::McpServer("docs".into());
/// let reviewed = ContentDigest::sha256(b"the declaration the user read");
/// assert!(!trust.is_trusted(&server, reviewed));
///
/// trust.grant(&server, reviewed)?;
/// assert!(trust.is_trusted(&server, reviewed));
/// // A changed declaration is a different digest and is not trusted.
/// assert!(!trust.is_trusted(&server, ContentDigest::sha256(b"edited")));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct ProjectTrust {
    store: Option<PathBuf>,
    root: PathBuf,
    observed: Arc<Mutex<BTreeMap<Subject, ContentDigest>>>,
}

impl ProjectTrust {
    /// Trust decisions kept in `store` (none when `None`) for the project at `root`.
    ///
    /// The root is canonicalized so one directory has one record however it is
    /// spelled.
    #[must_use]
    pub fn new(store: Option<PathBuf>, root: &Path) -> Self {
        Self {
            store,
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            observed: Arc::default(),
        }
    }

    /// Decisions kept in the user configuration directory, for the project at `root`.
    #[must_use]
    pub fn for_user(root: &Path) -> Self {
        Self::new(crate::config_dir::config_file(STORE_FILE), root)
    }

    /// The canonical project root decisions are recorded under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Record the digest of `subject` as this process loaded it.
    pub fn observe(&self, subject: Subject, digest: ContentDigest) {
        if let Ok(mut observed) = self.observed.lock() {
            observed.insert(subject, digest);
        }
    }

    /// The digest of `subject` as this process loaded it, if it did.
    #[must_use]
    pub fn observed(&self, subject: &Subject) -> Option<ContentDigest> {
        self.observed.lock().ok()?.get(subject).copied()
    }

    /// Whether the user trusted `subject` with exactly this `digest`.
    ///
    /// Fails closed: a missing, unreadable or invalid store trusts nothing.
    #[must_use]
    pub fn is_trusted(&self, subject: &Subject, digest: ContentDigest) -> bool {
        let Some(path) = &self.store else {
            return false;
        };
        match self.read_document(path) {
            Ok(document) => document
                .projects
                .get(&self.key())
                .and_then(|subjects| subjects.get(&subject.to_string()))
                .is_some_and(|trusted| *trusted == digest),
            Err(error) => {
                tracing::warn!(%error, "project trust store ignored; nothing is trusted");
                false
            }
        }
    }

    /// Trust `subject` with exactly this `digest`, replacing an earlier decision.
    ///
    /// # Errors
    ///
    /// [`TrustError::NoStore`] without a store, [`TrustError::Malformed`] when
    /// the existing store is not valid, and the read or write failure otherwise.
    pub fn grant(&self, subject: &Subject, digest: ContentDigest) -> Result<(), TrustError> {
        self.update(|subjects| {
            subjects.insert(subject.to_string(), digest);
            true
        })
        .map(|_| ())
    }

    /// Trust `subject` with the digest this process loaded.
    ///
    /// # Errors
    ///
    /// [`TrustError::NotObserved`] when this process did not load it, otherwise
    /// as [`grant`](Self::grant).
    pub fn grant_observed(&self, subject: &Subject) -> Result<ContentDigest, TrustError> {
        let digest = self
            .observed(subject)
            .ok_or_else(|| TrustError::NotObserved(subject.clone()))?;
        self.grant(subject, digest)?;
        Ok(digest)
    }

    /// Withdraw the decision for `subject`. Returns whether one existed.
    ///
    /// # Errors
    ///
    /// As [`grant`](Self::grant).
    pub fn revoke(&self, subject: &Subject) -> Result<bool, TrustError> {
        self.update(|subjects| subjects.remove(&subject.to_string()).is_some())
    }

    fn key(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    fn read_document(&self, path: &Path) -> Result<Document, TrustError> {
        let Some(content) = read_content(path, MAX_FILE_BYTES).map_err(TrustError::Read)? else {
            return Ok(Document::default());
        };
        let document: Document = serde_json::from_slice(&content.bytes)
            .map_err(|_| TrustError::Malformed(path.to_path_buf()))?;
        if document.version > STORE_VERSION {
            return Err(TrustError::Malformed(path.to_path_buf()));
        }
        Ok(document)
    }

    // Read, change this project's record, and replace the file atomically. The
    // change reports whether it altered anything; an unaltered store is not rewritten.
    fn update(
        &self,
        change: impl FnOnce(&mut BTreeMap<String, ContentDigest>) -> bool,
    ) -> Result<bool, TrustError> {
        let path = self.store.as_ref().ok_or(TrustError::NoStore)?;
        let mut document = self.read_document(path)?;
        let subjects = document.projects.entry(self.key()).or_default();
        let changed = change(subjects);
        if subjects.is_empty() {
            document.projects.remove(&self.key());
        }
        if !changed {
            return Ok(false);
        }
        document.version = STORE_VERSION;
        let write = |source| TrustError::Write {
            path: path.clone(),
            source,
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(write)?;
        let mut text = serde_json::to_vec_pretty(&document)
            .map_err(|error| write(std::io::Error::other(error)))?;
        text.push(b'\n');
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(write)?;
        temporary.write_all(&text).map_err(write)?;
        temporary.as_file().sync_all().map_err(write)?;
        temporary
            .persist(path)
            .map_err(|error| write(error.error))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> ProjectTrust {
        ProjectTrust::new(
            Some(dir.join("config").join(STORE_FILE)),
            &dir.join("project"),
        )
    }

    #[test]
    fn a_decision_covers_only_the_digest_it_was_made_for() {
        let dir = tempfile::tempdir().unwrap();
        let trust = store(dir.path());
        let subject = Subject::Settings;
        let reviewed = ContentDigest::sha256(b"reviewed");
        assert!(!trust.is_trusted(&subject, reviewed));
        trust.grant(&subject, reviewed).unwrap();
        assert!(trust.is_trusted(&subject, reviewed));
        assert!(!trust.is_trusted(&subject, ContentDigest::sha256(b"edited")));
        assert!(!trust.is_trusted(&Subject::PermissionRules, reviewed));
        assert!(!trust.is_trusted(&Subject::McpServer("settings".into()), reviewed));
    }

    #[test]
    fn decisions_are_per_project_root_and_survive_a_new_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE);
        let (one, two) = (dir.path().join("one"), dir.path().join("two"));
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let digest = ContentDigest::sha256(b"x");
        let subject = Subject::McpServer("docs".into());
        ProjectTrust::new(Some(path.clone()), &one)
            .grant(&subject, digest)
            .unwrap();
        assert!(ProjectTrust::new(Some(path.clone()), &one).is_trusted(&subject, digest));
        assert!(!ProjectTrust::new(Some(path.clone()), &two).is_trusted(&subject, digest));
        // The same directory spelled differently is the same project.
        assert!(
            ProjectTrust::new(Some(path), &one.join("..").join("one")).is_trusted(&subject, digest)
        );
    }

    #[test]
    fn revoking_removes_only_that_subject() {
        let dir = tempfile::tempdir().unwrap();
        let trust = store(dir.path());
        let digest = ContentDigest::sha256(b"x");
        trust.grant(&Subject::Settings, digest).unwrap();
        trust.grant(&Subject::PermissionRules, digest).unwrap();
        assert!(trust.revoke(&Subject::Settings).unwrap());
        assert!(!trust.revoke(&Subject::Settings).unwrap());
        assert!(!trust.is_trusted(&Subject::Settings, digest));
        assert!(trust.is_trusted(&Subject::PermissionRules, digest));
    }

    #[test]
    fn nothing_is_trusted_without_a_usable_store() {
        let digest = ContentDigest::sha256(b"x");
        let none = ProjectTrust::new(None, Path::new("/project"));
        assert!(!none.is_trusted(&Subject::Settings, digest));
        assert!(matches!(
            none.grant(&Subject::Settings, digest),
            Err(TrustError::NoStore)
        ));

        let dir = tempfile::tempdir().unwrap();
        let trust = store(dir.path());
        trust.grant(&Subject::Settings, digest).unwrap();
        let path = dir.path().join("config").join(STORE_FILE);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(!trust.is_trusted(&Subject::Settings, digest));
        // A damaged store is reported, not replaced.
        assert!(matches!(
            trust.grant(&Subject::PermissionRules, digest),
            Err(TrustError::Malformed(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn a_store_from_a_newer_version_or_with_unknown_fields_trusts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let trust = store(dir.path());
        let digest = ContentDigest::sha256(b"x");
        trust.grant(&Subject::Settings, digest).unwrap();
        let path = dir.path().join("config").join(STORE_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("\"version\": 1", "\"version\": 2")).unwrap();
        assert!(!trust.is_trusted(&Subject::Settings, digest));
        std::fs::write(&path, text.replace("\"version\": 1", "\"extra\": 1")).unwrap();
        assert!(!trust.is_trusted(&Subject::Settings, digest));
    }

    #[test]
    fn only_content_this_process_loaded_can_be_trusted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let trust = store(dir.path());
        assert!(matches!(
            trust.grant_observed(&Subject::Settings),
            Err(TrustError::NotObserved(_))
        ));
        let loaded = ContentDigest::sha256(b"loaded");
        trust.observe(Subject::Settings, loaded);
        // A clone shares what was observed.
        let clone = trust.clone();
        assert_eq!(clone.grant_observed(&Subject::Settings).unwrap(), loaded);
        assert!(trust.is_trusted(&Subject::Settings, loaded));
    }

    #[test]
    fn the_store_is_written_for_the_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let trust = store(dir.path());
        trust
            .grant(&Subject::Settings, ContentDigest::sha256(b"x"))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("config").join(STORE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "store mode {mode:o}");
        }
    }

    #[test]
    fn read_content_returns_the_bytes_and_their_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        assert!(read_content(&path, 16).unwrap().is_none());
        std::fs::write(&path, b"hello").unwrap();
        let content = read_content(&path, 16).unwrap().unwrap();
        assert_eq!(content.bytes, b"hello");
        assert_eq!(content.digest, ContentDigest::sha256(b"hello"));
        std::fs::write(&path, vec![b'x'; 17]).unwrap();
        assert!(matches!(
            read_content(&path, 16),
            Err(ReadError::TooLarge { limit: 16, .. })
        ));
        std::fs::write(&path, vec![b'x'; 16]).unwrap();
        assert!(read_content(&path, 16).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn read_content_refuses_special_files_and_links_to_them() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_content(dir.path(), 16),
            Err(ReadError::NotRegular(_))
        ));
        // A device would never end; refusing it must not read it.
        assert!(matches!(
            read_content(Path::new("/dev/zero"), 16),
            Err(ReadError::NotRegular(_))
        ));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
        assert!(matches!(
            read_content(&link, 16),
            Err(ReadError::NotRegular(_))
        ));
        // A FIFO would block until something wrote to it.
        let fifo = dir.path().join("fifo");
        if std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|status| status.success())
        {
            assert!(matches!(
                read_content(&fifo, 16),
                Err(ReadError::NotRegular(_))
            ));
        }
        // A link to a regular file is followed, as dotfile setups need.
        let target = dir.path().join("target");
        std::fs::write(&target, b"ok").unwrap();
        let good = dir.path().join("good");
        std::os::unix::fs::symlink(&target, &good).unwrap();
        assert_eq!(read_content(&good, 16).unwrap().unwrap().bytes, b"ok");
        // A link to nothing is a missing file.
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("absent"), &dangling).unwrap();
        assert!(read_content(&dangling, 16).unwrap().is_none());
    }
}
