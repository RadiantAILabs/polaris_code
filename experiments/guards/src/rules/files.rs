//! Protected paths, the sealed copy of them, and a fingerprint of the working
//! tree.
//!
//! Everything here is about files on disk and holds no model or graph state:
//!
//! - [`ProtectedSet`] says which paths the rules protect and answers "would
//!   touching this path touch a protected one?" for a path given any way a tool
//!   can give it (relative, `..`, through a symlink, a hard link).
//! - [`Seal`] keeps the protected files' bytes **in memory**, taken before the
//!   first model step. It is never written under the working tree, so nothing
//!   the agent's tools do to the tree can reach it.
//! - [`Fingerprinter`] hashes the working tree so a later check can say whether
//!   anything changed since an earlier one, whichever tool changed it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use polaris_cosmos::contracts::ContentDigest;
use walkdir::WalkDir;

/// Directories that hold regenerated caches or the agent's own bookkeeping.
///
/// They are left out of fingerprints, of the sealed check's copy, and of the
/// sealed protected directories, because their content changes without anyone
/// editing the work.
pub const CACHE_DIRS: &[&str] = &[
    ".git",
    ".polaris",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".hypothesis",
];

/// Heavy directories a search for runner configuration does not enter.
const HEAVY_DIRS: &[&str] = &["node_modules", "target", ".venv", "venv", ".tox"];

/// The most files and bytes the sealed check will copy.
const COPY_MAX_FILES: usize = 100_000;
const COPY_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Whether a file name configures how tests run, so changing or adding one
/// changes what a test run means.
///
/// These are the files a test runner reads on its own, with no flag, from the
/// tree: pytest's `conftest.py` and ini files, `tox.ini`, and Python's
/// start-up hooks (`sitecustomize.py`, `usercustomize.py`, `*.pth`).
#[must_use]
pub fn is_runner_config_name(name: &str) -> bool {
    matches!(
        name,
        "conftest.py"
            | "pytest.ini"
            | ".pytest.ini"
            | "pytest.toml"
            | ".pytest.toml"
            | "tox.ini"
            | "sitecustomize.py"
            | "usercustomize.py"
    ) || name.ends_with(".pth")
}

/// File names that carry test-runner settings *among other things*. Writing one
/// is only a change to how tests run when the written text mentions the runner.
#[must_use]
pub fn is_shared_config_name(name: &str) -> bool {
    matches!(name, "pyproject.toml" | "setup.cfg")
}

/// Words that make a write to a shared configuration file a change to how
/// tests run.
pub const RUNNER_WORDS: &[&str] = &[
    "pytest",
    "unittest",
    "testpaths",
    "addopts",
    "deselect",
    "--ignore",
    "filterwarnings",
    "markers",
    "collect_ignore",
    "norecursedirs",
    "python_files",
    "python_functions",
];

/// Remove `.` and resolve `..` without touching the file system.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with every symlink in its existing part resolved, and the part that
/// does not exist yet appended unchanged.
#[must_use]
pub fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut existing = path.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (
            existing.file_name().map(ToOwned::to_owned),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path,
        }
    }
}

/// Why a path counts as protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touch {
    /// The path is a protected path, or inside a protected directory.
    Protected(String),
    /// The path contains a protected path, so removing or moving it would
    /// remove or move the protected one.
    Contains(String),
    /// The path is the same file as a protected file, reached by another name.
    Linked(String),
    /// The path is a file a test runner reads on its own.
    RunnerConfig(String),
}

impl Touch {
    /// A sentence fragment naming what was touched.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Protected(p) => format!("the protected path `{p}`"),
            Self::Contains(p) => format!("a directory that contains the protected path `{p}`"),
            Self::Linked(p) => format!("another name for the protected file `{p}`"),
            Self::RunnerConfig(p) => {
                format!("`{p}`, a file that changes how tests are run")
            }
        }
    }

    /// A short machine-readable kind.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Protected(_) => "protected-path",
            Self::Contains(_) => "contains-protected-path",
            Self::Linked(_) => "linked-protected-file",
            Self::RunnerConfig(_) => "runner-config",
        }
    }
}

/// The paths the rules protect, inside one working directory.
#[derive(Debug, Clone)]
pub struct ProtectedSet {
    root: PathBuf,
    entries: Vec<PathBuf>,
}

impl ProtectedSet {
    /// Protect `given` (relative to `root`, or absolute inside it).
    ///
    /// # Errors
    ///
    /// A message when a path does not exist, or is outside `root`.
    pub fn new(root: &Path, given: &[PathBuf]) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("working directory {}: {e}", root.display()))?;
        let mut entries = Vec::new();
        for path in given {
            let joined = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            let resolved = joined
                .canonicalize()
                .map_err(|e| format!("protected path {}: {e}", path.display()))?;
            if !resolved.starts_with(&root) {
                return Err(format!(
                    "protected path {} is outside the working directory",
                    path.display()
                ));
            }
            if resolved == root {
                return Err("the working directory itself cannot be a protected path".into());
            }
            if !entries.contains(&resolved) {
                entries.push(resolved);
            }
        }
        Ok(Self { root, entries })
    }

    /// The canonical working directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The protected paths, relative to the working directory, as text.
    #[must_use]
    pub fn relative_entries(&self) -> Vec<String> {
        self.entries.iter().map(|e| self.relative(e)).collect()
    }

    /// The protected paths as canonical absolute paths.
    #[must_use]
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// `path` relative to the working directory, as text; unchanged when it is
    /// outside.
    #[must_use]
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// The absolute, symlink-resolved form of `raw`, read from `cwd`.
    #[must_use]
    pub fn resolve(&self, cwd: &Path, raw: &str) -> PathBuf {
        let raw = raw.trim();
        let joined = if let Some(rest) = raw.strip_prefix("~/") {
            std::env::var_os("HOME")
                .map_or_else(|| cwd.join(raw), |home| PathBuf::from(home).join(rest))
        } else if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            cwd.join(raw)
        };
        resolve_existing_prefix(&joined)
    }

    /// Whether mutating `abs` (already resolved) would touch a protected path.
    #[must_use]
    pub fn touch(&self, abs: &Path) -> Option<Touch> {
        for entry in &self.entries {
            if abs == entry || abs.starts_with(entry) {
                return Some(Touch::Protected(self.relative(entry)));
            }
            if entry.starts_with(abs) {
                return Some(Touch::Contains(self.relative(entry)));
            }
        }
        if let Some(name) = abs.file_name().and_then(|n| n.to_str())
            && abs.starts_with(&self.root)
            && is_runner_config_name(name)
        {
            return Some(Touch::RunnerConfig(self.relative(abs)));
        }
        if let Some(linked) = self.linked_protected_file(abs) {
            return Some(Touch::Linked(linked));
        }
        None
    }

    /// The protected file `abs` is a hard link to, if any.
    #[cfg(unix)]
    fn linked_protected_file(&self, abs: &Path) -> Option<String> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(abs).ok()?;
        if !meta.is_file() || meta.nlink() < 2 {
            return None;
        }
        let key = (meta.dev(), meta.ino());
        for entry in &self.entries {
            for file in files_under(entry) {
                if let Ok(m) = std::fs::metadata(&file)
                    && (m.dev(), m.ino()) == key
                {
                    return Some(self.relative(&file));
                }
            }
        }
        None
    }

    #[cfg(not(unix))]
    fn linked_protected_file(&self, _abs: &Path) -> Option<String> {
        None
    }
}

/// Every regular file at or under `path`, skipping caches.
fn files_under(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_cache_dir(e.file_name().to_str().unwrap_or_default(), e.depth()))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect()
}

fn is_cache_dir(name: &str, depth: usize) -> bool {
    depth > 0 && CACHE_DIRS.contains(&name)
}

/// What differs between the sealed files and the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tamper {
    /// Path relative to the working directory.
    pub path: String,
    /// What happened to it.
    pub kind: TamperKind,
}

/// How a sealed path differs from the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TamperKind {
    /// Content, permissions or file type differ from the seal.
    Modified,
    /// A sealed file is gone.
    Deleted,
    /// A file that was not sealed sits in a protected directory, or is a
    /// runner configuration file the seal did not hold.
    Created,
}

/// The sealed bytes of every protected file and every runner configuration
/// file, taken before the first model step and kept in memory.
#[derive(Debug, Clone)]
pub struct Seal {
    root: PathBuf,
    entries: Vec<PathBuf>,
    files: BTreeMap<PathBuf, (Vec<u8>, u32)>,
    dirs: BTreeSet<PathBuf>,
}

fn mode_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

fn remove_any(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            // A directory the agent made unreadable must still go.
            for entry in WalkDir::new(path)
                .contents_first(false)
                .into_iter()
                .flatten()
            {
                if entry.file_type().is_dir() {
                    set_mode(entry.path(), 0o755);
                }
            }
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// `path` relative to `root`, refusing anything that is not strictly inside it.
///
/// Every path the seal keeps goes through here, so a sealed path can never be
/// absolute or climb out of the tree it is later written back into.
fn inside(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let rel = path.strip_prefix(root).map_err(|_| {
        io::Error::other(format!(
            "{} is outside the working directory",
            path.display()
        ))
    })?;
    if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(io::Error::other(format!(
            "{} is not a path inside the working directory",
            path.display()
        )));
    }
    Ok(rel.to_path_buf())
}

impl Seal {
    /// Read every protected file, and every runner configuration file in the
    /// tree.
    ///
    /// # Errors
    ///
    /// The I/O error from reading a protected file.
    pub fn take(protected: &ProtectedSet) -> io::Result<Self> {
        let root = protected.root().to_path_buf();
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        for entry in protected.entries() {
            if entry.is_dir() {
                dirs.insert(inside(&root, entry)?);
            }
            for file in files_under(entry) {
                let meta = std::fs::metadata(&file)?;
                files.insert(
                    inside(&root, &file)?,
                    (std::fs::read(&file)?, mode_of(&meta)),
                );
            }
        }
        for file in config_files_in(&root) {
            let meta = std::fs::metadata(&file)?;
            let rel = inside(&root, &file)?;
            files
                .entry(rel)
                .or_insert((std::fs::read(&file)?, mode_of(&meta)));
        }
        let entries = protected
            .entries()
            .iter()
            .map(|e| inside(&root, e))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            root,
            entries,
            files,
            dirs,
        })
    }

    /// The number of sealed files.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// A digest over every sealed path, mode and byte.
    #[must_use]
    pub fn digest(&self) -> ContentDigest {
        let mut bytes = Vec::new();
        for (path, (data, mode)) in &self.files {
            bytes.extend_from_slice(path.to_string_lossy().as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(format!("{mode:o}").as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(ContentDigest::sha256(data).as_bytes());
        }
        ContentDigest::sha256(&bytes)
    }

    /// How the tree under `root` differs from the seal.
    #[must_use]
    pub fn diff(&self, root: &Path) -> Vec<Tamper> {
        let mut out = Vec::new();
        for (rel, (data, mode)) in &self.files {
            let abs = root.join(rel);
            let kind = match std::fs::symlink_metadata(&abs) {
                Err(_) => Some(TamperKind::Deleted),
                Ok(meta) if !meta.is_file() => Some(TamperKind::Modified),
                Ok(meta) => match std::fs::read(&abs) {
                    Ok(now) if now == *data && mode_of(&meta) == *mode => None,
                    _ => Some(TamperKind::Modified),
                },
            };
            if let Some(kind) = kind {
                out.push(Tamper {
                    path: rel.to_string_lossy().into_owned(),
                    kind,
                });
            }
        }
        for entry in &self.entries {
            let abs = root.join(entry);
            if !self.dirs.contains(entry) {
                continue;
            }
            for file in files_under(&abs) {
                let rel = file.strip_prefix(root).unwrap_or(&file);
                if !self.files.contains_key(rel) {
                    out.push(Tamper {
                        path: rel.to_string_lossy().into_owned(),
                        kind: TamperKind::Created,
                    });
                }
            }
        }
        for file in config_files_in(root) {
            let rel = file.strip_prefix(root).unwrap_or(&file);
            if !self.files.contains_key(rel) && !out.iter().any(|t| Path::new(&t.path) == rel) {
                out.push(Tamper {
                    path: rel.to_string_lossy().into_owned(),
                    kind: TamperKind::Created,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Put the sealed state back in `root`: rewrite modified and deleted
    /// files, remove files the seal did not hold. Returns the paths it could
    /// not restore.
    pub fn restore(&self, root: &Path, tampers: &[Tamper]) -> Vec<String> {
        let mut failed = Vec::new();
        for tamper in tampers {
            let rel = Path::new(&tamper.path);
            let abs = root.join(rel);
            let ok = match tamper.kind {
                TamperKind::Created => {
                    remove_any(&abs);
                    !abs.exists()
                }
                TamperKind::Modified | TamperKind::Deleted => match self.files.get(rel) {
                    Some((data, mode)) => self.write_sealed(&abs, data, *mode),
                    None => false,
                },
            };
            if !ok {
                failed.push(tamper.path.clone());
            }
        }
        failed
    }

    fn write_sealed(&self, abs: &Path, data: &[u8], mode: u32) -> bool {
        // A directory, link or read-only file in the way goes first.
        if let Ok(meta) = std::fs::symlink_metadata(abs) {
            if meta.is_dir() || meta.file_type().is_symlink() {
                remove_any(abs);
            } else {
                set_mode(abs, 0o644);
            }
        }
        if let Some(parent) = abs.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return false;
        }
        if std::fs::write(abs, data).is_err() {
            return false;
        }
        set_mode(abs, mode);
        true
    }

    /// Make the tree under `dest` carry exactly the sealed protected files and
    /// no other runner configuration file. `dest` is a copy of the working
    /// tree; the seal is applied to it, never to the working tree itself.
    pub fn apply_to_copy(&self, dest: &Path) -> io::Result<()> {
        for entry in &self.entries {
            remove_any(&dest.join(entry));
        }
        for file in config_files_in(dest) {
            let rel = file.strip_prefix(dest).unwrap_or(&file);
            if !self.files.contains_key(rel) {
                remove_any(&file);
            }
        }
        for (rel, (data, mode)) in &self.files {
            let abs = dest.join(rel);
            if !self.write_sealed(&abs, data, *mode) {
                return Err(io::Error::other(format!(
                    "could not place sealed file {}",
                    rel.display()
                )));
            }
        }
        for dir in &self.dirs {
            std::fs::create_dir_all(dest.join(dir))?;
        }
        Ok(())
    }

    /// The working directory the seal was taken in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Runner configuration files anywhere in the tree, outside caches and heavy
/// directories.
fn config_files_in(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or_default();
            !(e.depth() > 0 && (CACHE_DIRS.contains(&name) || HEAVY_DIRS.contains(&name)))
        })
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file() && e.file_name().to_str().is_some_and(is_runner_config_name)
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Copy the working tree to `dest` for a sealed check, leaving out caches.
///
/// # Errors
///
/// An I/O error, or a refusal when the tree is larger than the copy limit.
pub fn copy_tree(root: &Path, dest: &Path) -> io::Result<()> {
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_cache_dir(e.file_name().to_str().unwrap_or_default(), e.depth()))
    {
        let entry = entry.map_err(io::Error::other)?;
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let target = dest.join(rel);
        let file_type = entry.file_type();
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            {
                let link = std::fs::read_link(entry.path())?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::os::unix::fs::symlink(link, &target)?;
            }
        } else if file_type.is_file() {
            files += 1;
            bytes += entry.metadata().map_err(io::Error::other)?.len();
            if files > COPY_MAX_FILES || bytes > COPY_MAX_BYTES {
                return Err(io::Error::other(format!(
                    "the working tree is too large to copy for a sealed check \
                     (limit {COPY_MAX_FILES} files or {COPY_MAX_BYTES} bytes)"
                )));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Per-file facts that say "unchanged" without reading the file again.
type Signature = (u64, i128, i128);

fn signature(meta: &std::fs::Metadata) -> Signature {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(0));
    // The change time moves on every write and cannot be set back by a tool
    // that only sets the modification time.
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
    };
    #[cfg(not(unix))]
    let changed = 0;
    (meta.len(), mtime, changed)
}

/// Hashes the working tree, reading only the files whose size or times moved.
#[derive(Debug, Clone)]
pub struct Fingerprinter {
    root: PathBuf,
    cache: HashMap<PathBuf, (Signature, ContentDigest)>,
}

impl Fingerprinter {
    /// A fingerprinter over `root`.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cache: HashMap::new(),
        }
    }

    /// The digest of every path, file type and file content in the tree,
    /// outside caches.
    ///
    /// # Errors
    ///
    /// An I/O error from walking or reading.
    pub fn scan(&mut self) -> io::Result<ContentDigest> {
        let mut lines: Vec<(String, String)> = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_cache_dir(e.file_name().to_str().unwrap_or_default(), e.depth()))
        {
            let entry = entry.map_err(io::Error::other)?;
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .to_path_buf();
            if rel.as_os_str().is_empty() {
                continue;
            }
            let file_type = entry.file_type();
            let describe = if file_type.is_symlink() {
                let target = std::fs::read_link(entry.path())?;
                format!("link:{}", target.display())
            } else if file_type.is_file() {
                let meta = entry.metadata().map_err(io::Error::other)?;
                let sig = signature(&meta);
                let digest = match self.cache.get(&rel) {
                    Some((old, digest)) if *old == sig => *digest,
                    _ => {
                        let digest = ContentDigest::sha256(&std::fs::read(entry.path())?);
                        self.cache.insert(rel.clone(), (sig, digest));
                        digest
                    }
                };
                format!("file:{digest}")
            } else {
                "dir".to_string()
            };
            lines.push((rel.to_string_lossy().into_owned(), describe));
            seen.insert(rel);
        }
        self.cache.retain(|path, _| seen.contains(path));
        lines.sort();
        let mut bytes = Vec::new();
        for (path, describe) in lines {
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(describe.as_bytes());
            bytes.push(b'\n');
        }
        Ok(ContentDigest::sha256(&bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("solution.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(dir.path().join("test.py"), "assert f() == 1\n").unwrap();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/test_a.py"), "a\n").unwrap();
        dir
    }

    fn set(dir: &Path, paths: &[&str]) -> ProtectedSet {
        let given: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        ProtectedSet::new(dir, &given).unwrap()
    }

    #[test]
    fn a_protected_path_must_exist_and_stay_inside_the_tree() {
        let dir = tree();
        assert!(ProtectedSet::new(dir.path(), &[PathBuf::from("nope.py")]).is_err());
        assert!(ProtectedSet::new(dir.path(), &[PathBuf::from("..")]).is_err());
        assert!(ProtectedSet::new(dir.path(), &[PathBuf::from(".")]).is_err());
        assert!(ProtectedSet::new(dir.path(), &[PathBuf::from("test.py")]).is_ok());
    }

    #[test]
    fn touch_covers_the_path_its_directory_and_its_parents() {
        let dir = tree();
        let protected = set(dir.path(), &["test.py", "tests"]);
        let root = protected.root().to_path_buf();
        assert!(matches!(
            protected.touch(&root.join("test.py")),
            Some(Touch::Protected(p)) if p == "test.py"
        ));
        assert!(matches!(
            protected.touch(&root.join("tests/new_file.py")),
            Some(Touch::Protected(_))
        ));
        assert!(matches!(protected.touch(&root), Some(Touch::Contains(_))));
        assert!(protected.touch(&root.join("solution.py")).is_none());
        assert!(matches!(
            protected.touch(&root.join("sub/conftest.py")),
            Some(Touch::RunnerConfig(_))
        ));
    }

    #[test]
    fn dot_dot_and_symlinks_resolve_to_the_protected_path() {
        let dir = tree();
        let protected = set(dir.path(), &["test.py"]);
        let root = protected.root().to_path_buf();
        let viaparent = protected.resolve(&root, "tests/../test.py");
        assert!(protected.touch(&viaparent).is_some());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("test.py"), root.join("alias.py")).unwrap();
            let alias = protected.resolve(&root, "alias.py");
            assert!(protected.touch(&alias).is_some());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_to_a_protected_file_is_the_same_file() {
        let dir = tree();
        let protected = set(dir.path(), &["test.py"]);
        let root = protected.root().to_path_buf();
        std::fs::hard_link(root.join("test.py"), root.join("twin.py")).unwrap();
        assert!(matches!(
            protected.touch(&root.join("twin.py")),
            Some(Touch::Linked(_))
        ));
    }

    #[test]
    fn the_seal_sees_edits_deletes_and_new_files() {
        let dir = tree();
        let protected = set(dir.path(), &["test.py", "tests"]);
        let seal = Seal::take(&protected).unwrap();
        let root = dir.path();
        assert!(seal.diff(root).is_empty());

        std::fs::write(root.join("test.py"), "pass\n").unwrap();
        std::fs::remove_file(root.join("tests/test_a.py")).unwrap();
        std::fs::write(root.join("tests/extra.py"), "x\n").unwrap();
        std::fs::write(root.join("conftest.py"), "x\n").unwrap();
        // A change outside the protected paths is not tampering.
        std::fs::write(root.join("solution.py"), "changed\n").unwrap();

        let diff = seal.diff(root);
        let kinds: Vec<(String, TamperKind)> =
            diff.iter().map(|t| (t.path.clone(), t.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("conftest.py".to_string(), TamperKind::Created),
                ("test.py".to_string(), TamperKind::Modified),
                ("tests/extra.py".to_string(), TamperKind::Created),
                ("tests/test_a.py".to_string(), TamperKind::Deleted),
            ]
        );

        assert!(seal.restore(root, &diff).is_empty());
        assert!(seal.diff(root).is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("test.py")).unwrap(),
            "assert f() == 1\n"
        );
        assert!(!root.join("conftest.py").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("solution.py")).unwrap(),
            "changed\n",
            "restoring leaves the work alone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_protected_file_replaced_by_a_symlink_is_a_change() {
        let dir = tree();
        let protected = set(dir.path(), &["test.py"]);
        let seal = Seal::take(&protected).unwrap();
        let root = dir.path();
        std::fs::write(root.join("other.py"), "assert f() == 1\n").unwrap();
        std::fs::remove_file(root.join("test.py")).unwrap();
        std::os::unix::fs::symlink(root.join("other.py"), root.join("test.py")).unwrap();
        let diff = seal.diff(root);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].kind, TamperKind::Modified);
        assert!(seal.restore(root, &diff).is_empty());
        assert!(
            !std::fs::symlink_metadata(root.join("test.py"))
                .unwrap()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_permission_change_on_a_protected_file_is_a_change() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tree();
        let protected = set(dir.path(), &["test.py"]);
        let seal = Seal::take(&protected).unwrap();
        std::fs::set_permissions(
            dir.path().join("test.py"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let diff = seal.diff(dir.path());
        assert_eq!(diff.len(), 1);
        assert!(seal.restore(dir.path(), &diff).is_empty());
        assert!(seal.diff(dir.path()).is_empty());
    }

    #[test]
    fn a_copy_gets_the_sealed_tests_whatever_the_tree_holds() {
        let dir = tree();
        let protected = set(dir.path(), &["test.py"]);
        let seal = Seal::take(&protected).unwrap();
        std::fs::write(dir.path().join("test.py"), "pass\n").unwrap();
        std::fs::write(dir.path().join("conftest.py"), "x\n").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "x\n").unwrap();

        let copy = tempfile::tempdir().unwrap();
        copy_tree(dir.path(), copy.path()).unwrap();
        assert!(!copy.path().join(".git").exists(), "caches are left out");
        seal.apply_to_copy(copy.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(copy.path().join("test.py")).unwrap(),
            "assert f() == 1\n"
        );
        assert!(!copy.path().join("conftest.py").exists());
        assert!(copy.path().join("solution.py").exists());
    }

    #[test]
    fn the_fingerprint_moves_with_any_change_and_ignores_caches() {
        let dir = tree();
        let mut prints = Fingerprinter::new(dir.path());
        let first = prints.scan().unwrap();
        assert_eq!(first, prints.scan().unwrap());

        std::fs::create_dir(dir.path().join("__pycache__")).unwrap();
        std::fs::write(dir.path().join("__pycache__/x.pyc"), "x").unwrap();
        assert_eq!(first, prints.scan().unwrap(), "a cache is not an edit");

        std::fs::write(dir.path().join("solution.py"), "def f():\n    return 2\n").unwrap();
        let second = prints.scan().unwrap();
        assert_ne!(first, second);

        std::fs::remove_file(dir.path().join("solution.py")).unwrap();
        assert_ne!(second, prints.scan().unwrap());
    }

    #[test]
    fn same_size_rewrites_within_one_tick_are_still_seen() {
        let dir = tree();
        let mut prints = Fingerprinter::new(dir.path());
        let first = prints.scan().unwrap();
        // Same length, different byte; the signature includes the change time.
        std::fs::write(dir.path().join("solution.py"), "def f():\n    return 2\n").unwrap();
        assert_ne!(first, prints.scan().unwrap());
    }
}
