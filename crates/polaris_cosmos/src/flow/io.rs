//! Atomic durable writes and containment checks shared by the flow books.
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};
/// Error at a durable workflow boundary.
///
/// File system and JSON failures keep their original error, so callers can
/// branch on [`io::ErrorKind`] and walk [`source`](std::error::Error::source).
#[derive(Debug)]
#[non_exhaustive]
pub enum FlowError {
    /// A file system operation failed.
    Io(io::Error),
    /// A durable record could not be encoded or decoded.
    Json(serde_json::Error),
    /// An argument, record or state was rejected, with the reason.
    Invalid(String),
}
impl FlowError {
    /// Builds an [`Invalid`](Self::Invalid) error from a reason.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
    /// The kind of the underlying I/O error, when this is an I/O failure.
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Self::Io(error) => Some(error.kind()),
            _ => None,
        }
    }
}
impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Json(error) => error.fmt(f),
            Self::Invalid(message) => message.fmt(f),
        }
    }
}
impl std::error::Error for FlowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}
impl From<io::Error> for FlowError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<serde_json::Error> for FlowError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
/// Workflow operation result.
pub type FlowResult<T> = Result<T, FlowError>;
/// Locks a mutex and keeps going if another thread panicked while holding it.
///
/// Every book updates a copy and publishes it only after the copy is durable,
/// so a panic never leaves a half-applied state behind the lock.
pub(crate) trait LockExt<T> {
    /// Acquires the lock, ignoring poisoning.
    fn guard(&self) -> MutexGuard<'_, T>;
}
impl<T> LockExt<T> for Mutex<T> {
    fn guard(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
static TEMP: AtomicU64 = AtomicU64::new(0);
/// Writes a file by rename from a unique sibling, syncing its contents first.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> FlowResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    Ok(())
}
/// Atomically writes a JSON record.
pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> FlowResult<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}
/// Reads a JSON record.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> FlowResult<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
/// Appends one complete, flushed JSON line. Caller must serialize concurrent writes.
pub(crate) fn append_json(path: &Path, value: &impl Serialize) -> FlowResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if f.write(&line)? != line.len() {
        return Err(FlowError::invalid("short JSONL append"));
    }
    f.flush()?;
    f.sync_data()?;
    Ok(())
}
/// Rejects absolute paths, traversal, empty names and platform prefixes.
pub(crate) fn relative(path: &str) -> FlowResult<&Path> {
    let p = Path::new(path);
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !p.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(FlowError::invalid(format!("unsafe relative path: {path}")));
    }
    Ok(p)
}
/// Computes a SHA-256 hex digest.
#[doc(hidden)]
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
/// Streams an ordinary file through SHA-256 and returns its digest and length.
///
/// The type and size are checked on the opened handle, and no more than `cap`
/// bytes are ever read, so a file that grows during the call is still bounded.
pub(crate) fn hash_file(path: &Path, cap: u64) -> FlowResult<(String, u64)> {
    let file = fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(FlowError::invalid(
            "expected output must be an ordinary file",
        ));
    }
    let too_big = || {
        FlowError::invalid(format!(
            "output {} is over the {cap} byte limit",
            path.display()
        ))
    };
    if meta.len() > cap {
        return Err(too_big());
    }
    let mut reader = file.take(cap.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > cap {
            return Err(too_big());
        }
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}
/// Formats exact micro-dollars without floating point.
pub fn dollars(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}
/// Parses an exact dollar amount with up to six fractional digits.
///
/// # Errors
///
/// Returns [`FlowError::Invalid`] for an empty, signed, non-decimal or
/// over-precise amount, and for one that overflows `u64` micro-dollars.
pub fn parse_micros(s: &str) -> FlowResult<u64> {
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(FlowError::invalid(format!("invalid dollar amount: {s}")));
    }
    whole
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(1_000_000))
        .and_then(|v| {
            fraction
                .parse::<u64>()
                .unwrap_or(0)
                .checked_mul(10u64.pow((6 - fraction.len()) as u32))
                .and_then(|f| v.checked_add(f))
        })
        .ok_or_else(|| FlowError::invalid(format!("dollar amount overflows: {s}")))
}
/// UTC timestamp with milliseconds, without a clock or formatting dependency.
pub(crate) fn timestamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let days = (ms / 86_400_000) as i64;
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    let seconds = ms / 1000;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        ms % 1000
    )
}

impl From<FlowError> for polaris::system::system::SystemError {
    fn from(e: FlowError) -> Self {
        Self::ExecutionError(e.to_string())
    }
}
