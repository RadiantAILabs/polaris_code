//! Atomic durable writes and containment checks shared by the flow books.
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path},
    sync::atomic::{AtomicU64, Ordering},
};
/// Error at a durable workflow boundary.
#[derive(Debug)]
pub struct FlowError(pub String);
impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for FlowError {}
impl From<io::Error> for FlowError {
    fn from(e: io::Error) -> Self {
        Self(e.to_string())
    }
}
impl From<serde_json::Error> for FlowError {
    fn from(e: serde_json::Error) -> Self {
        Self(e.to_string())
    }
}
/// Workflow operation result.
pub type FlowResult<T> = Result<T, FlowError>;
static TEMP: AtomicU64 = AtomicU64::new(0);
/// Writes a file by rename from a unique sibling, syncing its contents first.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> FlowResult<()> {
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
pub fn write_json(path: &Path, value: &impl Serialize) -> FlowResult<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}
/// Reads a JSON record.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> FlowResult<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
/// Appends one complete, flushed JSON line. Caller must serialize concurrent writes.
pub fn append_json(path: &Path, value: &impl Serialize) -> FlowResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if f.write(&line)? != line.len() {
        return Err(FlowError("short JSONL append".into()));
    }
    f.flush()?;
    f.sync_data()?;
    Ok(())
}
/// Rejects absolute paths, traversal, empty names and platform prefixes.
pub fn relative(path: &str) -> FlowResult<&Path> {
    let p = Path::new(path);
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !p.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(FlowError(format!("unsafe relative path: {path}")));
    }
    Ok(p)
}
/// Computes a SHA-256 hex digest.
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
/// Formats exact micro-dollars without floating point.
pub fn dollars(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}
/// Parses an exact dollar amount with up to six fractional digits.
pub fn parse_micros(s: &str) -> FlowResult<u64> {
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(FlowError(format!("invalid dollar amount: {s}")));
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
        .ok_or_else(|| FlowError(format!("dollar amount overflows: {s}")))
}
/// UTC timestamp with milliseconds, without a clock or formatting dependency.
pub fn timestamp() -> String {
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
