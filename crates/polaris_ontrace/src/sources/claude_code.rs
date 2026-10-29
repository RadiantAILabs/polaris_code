//! Convenience wrapper for tapping Claude Code session streams.
//!
//! Claude Code persists every running session as a newline-delimited JSON
//! file under `~/.claude/projects/<slug>/<session-uuid>.jsonl`, and its
//! `--print --output-format stream-json` mode emits the same NDJSON event
//! shape on stdout. Both are covered by the existing low-level transports
//! ([`FileTailIOPlugin`], [`StdinIOPlugin`]) — this module just adds the
//! Claude-Code-specific path conventions on top.
//!
//! # Constructors
//!
//! | Constructor | What it does |
//! |-------------|--------------|
//! | [`ClaudeCodeIOPlugin::newest_session`] | Find newest `*.jsonl` under `~/.claude/projects/` and tail it. |
//! | [`ClaudeCodeIOPlugin::session`] | Tail a specific session by UUID. |
//! | [`ClaudeCodeIOPlugin::from_file`] | Tail an explicit path. |
//! | [`ClaudeCodeIOPlugin::from_stream_json_stdin`] | Read `--output-format stream-json` piped on stdin. |
//!
//! # Session discovery
//!
//! [`list_sessions`], [`find_session_by_id`], and [`newest_session_anywhere`]
//! are exposed for CLI front-ends that want to enumerate sessions without
//! constructing a plugin.

use crate::plugins::trace_ingest::TraceIngestAPI;
use crate::sources::file_tail::FileTailProvider;
use crate::sources::stdin::StdinProvider;
use polaris::plugins::{IOContent, IOError, IOMessage, IOProvider};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::sync::mpsc;

/// Source plugin that taps a Claude Code session into the trace pipeline.
///
/// A convenience wrapper over the low-level transports: it knows the
/// `~/.claude/projects/<slug>/<session-uuid>.jsonl` layout and the
/// `--print --output-format stream-json` stdout shape, and in
/// [`ready`](Plugin::ready) constructs the matching underlying provider — a
/// [`FileTailProvider`] for a session file or a [`StdinProvider`] for piped
/// `stream-json` — then drives it and pushes into
/// [`TraceIngestAPI`](crate::TraceIngestAPI). Before forwarding, each line is
/// run through a signature-stripping filter and a low-signal-event filter:
/// Claude Code stamps every `thinking` block with an opaque base64
/// `signature` blob that balloons LLM input tokens (see [`strip_signatures`]),
/// and emits bookkeeping events (permission-mode, file-history-snapshot,
/// skill-listing, deferred-tools) that carry no extractable relations.
///
/// Reach for this instead of [`FileTailIOPlugin`] /
/// [`StdinIOPlugin`](crate::StdinIOPlugin) when the source is specifically a
/// Claude Code session and you want session discovery plus the Claude-Code
/// cleanup built in. Pick a constructor by source:
/// [`newest_session`](Self::newest_session) /
/// [`session`](Self::session) / [`from_file`](Self::from_file) tail a session
/// `*.jsonl`; [`from_stream_json_stdin`](Self::from_stream_json_stdin) reads
/// piped stdout.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | This is a transport source: it produces no `Res`-accessible resources. It pushes [`IOMessage`]s into [`TraceIngestAPI`](crate::TraceIngestAPI), and the backend (`ExtractionPlugin` / `OxigraphExtractionPlugin`) owns the durable [`KnowledgeGraph`](crate::KnowledgeGraph) / store that results. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Sources are consumers, not publishers. This plugin acquires and pushes into [`TraceIngestAPI`](crate::TraceIngestAPI) (published by [`TraceIngestPlugin`](crate::TraceIngestPlugin)); it exposes no `API` of its own. |
///
/// # Dependencies
///
/// Declared by capability in [`access`](Plugin::access): `requires`
/// [`TraceIngestAPI`](crate::TraceIngestAPI), which
/// [`TraceIngestPlugin`](crate::TraceIngestPlugin) `provides`. The resolver
/// validates the provider exists at `finish()` rather than panicking at
/// runtime, so register [`TraceIngestPlugin`](crate::TraceIngestPlugin) and a
/// backend alongside this source.
///
/// # Lifecycle
///
/// The `~/.claude/projects/` discovery runs at construction time, not in
/// [`ready`](Plugin::ready): [`newest_session`](Self::newest_session) and
/// [`session`](Self::session) resolve a path eagerly (and return an error if
/// none matches), reusing the standalone [`list_sessions`],
/// [`find_session_by_id`], and [`newest_session_anywhere`] helpers.
/// [`ready`](Plugin::ready) then fetches the
/// [`TraceIngestAPI`](crate::TraceIngestAPI) sender and `tokio::spawn`s a task
/// that drives the chosen provider — a [`FileTailProvider`] (inheriting its
/// `tail -F` reopen/truncation semantics) or a [`StdinProvider`] — for the
/// life of the server, stripping signatures and dropping low-signal events
/// before forwarding each line. The file-tail variant stamps each line with
/// the session UUID as the trace id.
///
/// # Example
///
/// ```no_run
/// use polaris::models::{AnthropicPlugin, ModelsPlugin};
/// use polaris::plugins::PersistencePlugin;
/// use polaris::sessions::SessionsPlugin;
/// use polaris::sessions::store::memory::InMemoryStore;
/// use polaris::system::server::Server;
/// use polaris_ontrace::{ClaudeCodeIOPlugin, ExtractionPlugin, TraceIngestPlugin};
/// use std::sync::Arc;
///
/// # async fn wire() -> Result<(), Box<dyn std::error::Error>> {
/// let source = ClaudeCodeIOPlugin::newest_session()?; // discovery happens here
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(ModelsPlugin)
///     .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(TraceIngestPlugin::new())  // publishes TraceIngestAPI + TraceIngestRx
///     .add_plugins(ExtractionPlugin::new())   // backend: claims TraceIngestRx
///     .add_plugins(source);                    // source: requires TraceIngestAPI
/// server.finish().await?;
/// # Ok(())
/// # }
/// ```
pub struct ClaudeCodeIOPlugin {
    inner: Inner,
}

enum Inner {
    File {
        path: PathBuf,
        trace_id: Option<String>,
    },
    Stdin,
}

impl ClaudeCodeIOPlugin {
    /// Tail the newest `*.jsonl` session file anywhere under `~/.claude/projects/`.
    ///
    /// # Errors
    ///
    /// Returns an error if `$HOME` is unset, the `~/.claude/projects/`
    /// directory cannot be read, or no `*.jsonl` files exist under it.
    pub fn newest_session() -> Result<Self, String> {
        Ok(Self::from_file(newest_session_anywhere()?))
    }

    /// Tail the session whose filename stem matches `id`.
    ///
    /// The session UUID is stamped onto every emitted [`polaris::plugins::IOMessage`]
    /// as `META_TRACE_ID`, so the originating session id flows end-to-end onto
    /// [`crate::resources::trace_line::TraceLine::trace_id`].
    ///
    /// # Errors
    ///
    /// Returns an error if no matching session file exists.
    pub fn session(id: &str) -> Result<Self, String> {
        Ok(Self::from_file(find_session_by_id(id)?))
    }

    /// Tail an explicit file path.
    ///
    /// The file stem (filename without `.jsonl`) is treated as the session
    /// UUID and stamped onto every emitted [`polaris::plugins::IOMessage`] as
    /// `META_TRACE_ID`. If the path has no stem, no trace id is set.
    #[must_use]
    pub fn from_file(path: PathBuf) -> Self {
        let trace_id = session_id_from_path(&path);
        Self {
            inner: Inner::File { path, trace_id },
        }
    }

    /// Read Claude Code's `stream-json` output piped on stdin.
    ///
    /// Wire it up with:
    ///
    /// ```sh
    /// claude --print --output-format stream-json -p "..." | ontrace --stdin
    /// ```
    #[must_use]
    pub fn from_stream_json_stdin() -> Self {
        Self {
            inner: Inner::Stdin,
        }
    }
}

impl Plugin for ClaudeCodeIOPlugin {
    const ID: &'static str = "ontrace::sources::claude_code";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, _server: &mut Server) {}

    async fn ready(&self, server: &mut Server) {
        let tx = server
            .api::<TraceIngestAPI>()
            .expect("ClaudeCodeIOPlugin requires TraceIngestPlugin")
            .sender();
        match &self.inner {
            Inner::File { path, trace_id } => {
                let mut provider = FileTailProvider::new(path.clone());
                if let Some(id) = trace_id {
                    provider = provider.with_trace_id(id.clone());
                }
                tokio::spawn(drive(provider, tx));
            }
            Inner::Stdin => {
                tokio::spawn(drive(StdinProvider::new(), tx));
            }
        }
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TraceIngestAPI>(VersionReq::caret(TraceIngestAPI::CONTRACT_VERSION))
    }
}

/// Drive an underlying [`IOProvider`], strip Claude Code `signature` fields
/// from each line, drop known low-signal session-state events, and forward
/// the rest to the trace-ingest channel.
async fn drive<P: IOProvider>(provider: P, tx: mpsc::Sender<IOMessage>) {
    loop {
        match provider.receive().await {
            Ok(mut message) => {
                strip_signatures_in_message(&mut message);
                if is_low_signal_event_message(&message) {
                    tracing::debug!("claude_code: dropping low-signal session-state event");
                    continue;
                }
                if tx.send(message).await.is_err() {
                    tracing::info!("trace channel closed; claude_code source exiting");
                    return;
                }
            }
            Err(IOError::Closed) => {
                tracing::info!("claude_code source EOF; exiting");
                return;
            }
            Err(err) => {
                tracing::warn!(error = %err, "claude_code source read error; exiting");
                return;
            }
        }
    }
}

/// `true` when `message`'s JSON shape matches a known Claude Code event that
/// carries no factual relations worth the LLM extractor's attention.
///
/// Recognises four shapes by inspecting `type` (top-level) or
/// `attachment.type` (nested):
///
/// - `permission-mode` and `file-history-snapshot` describe session
///   bookkeeping.
/// - `skill_listing` enumerates the harness's skill catalog (left
///   unfiltered, the extractor turns it into spurious `has_role` triples).
/// - `deferred_tools_delta` advertises tool availability.
///
/// Non-JSON or unrecognized shapes pass through to the extractor.
fn is_low_signal_event_message(message: &IOMessage) -> bool {
    match &message.content {
        IOContent::Text(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) => is_low_signal_event_value(&v),
            Err(_) => false,
        },
        IOContent::Structured(v) => is_low_signal_event_value(v),
        IOContent::Binary { .. } => false,
    }
}

fn is_low_signal_event_value(value: &serde_json::Value) -> bool {
    match value.get("type").and_then(|v| v.as_str()) {
        Some("permission-mode" | "file-history-snapshot") => true,
        Some("attachment") => matches!(
            value
                .get("attachment")
                .and_then(|a| a.get("type"))
                .and_then(|v| v.as_str()),
            Some("skill_listing" | "deferred_tools_delta")
        ),
        _ => false,
    }
}

/// Strip Claude Code `signature` fields from an [`IOMessage`] in-place.
///
/// Text content is parsed as JSON; on parse failure the line is left
/// untouched. Structured content is mutated directly. Binary content is
/// ignored.
fn strip_signatures_in_message(message: &mut IOMessage) {
    match &mut message.content {
        IOContent::Text(s) => {
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(s)
                && strip_signatures(&mut value)
            {
                *s = value.to_string();
            }
        }
        IOContent::Structured(value) => {
            strip_signatures(value);
        }
        IOContent::Binary { .. } => {}
    }
}

/// Recursively remove every `"signature"` key from a JSON value.
///
/// Returns `true` if at least one key was removed, so callers can avoid
/// re-serializing when the line was clean.
fn strip_signatures(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            let mut removed = map.remove("signature").is_some();
            for v in map.values_mut() {
                removed |= strip_signatures(v);
            }
            removed
        }
        serde_json::Value::Array(arr) => {
            let mut removed = false;
            for v in arr.iter_mut() {
                removed |= strip_signatures(v);
            }
            removed
        }
        _ => false,
    }
}

/// Extract the session UUID from a `~/.claude/projects/<slug>/<uuid>.jsonl`
/// path. Returns the filename stem (the portion before `.jsonl`) if present.
fn session_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

// ─────────────────────────────────────────────────────────────────────────────
// Session discovery helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Path to `~/.claude/projects/`.
///
/// # Errors
///
/// Returns an error if `$HOME` is not set in the environment.
pub fn claude_projects_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    Ok(PathBuf::from(home).join(".claude").join("projects"))
}

/// Walk one level deep under `~/.claude/projects/<slug>/` for `*.jsonl` files.
///
/// Returns an empty vec if the projects directory does not exist yet
/// (fresh install before any session has been recorded).
///
/// # Errors
///
/// Propagates any IO error other than `NotFound` on the projects directory.
pub fn walk_session_files(projects_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let top = match std::fs::read_dir(projects_dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for project in top {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(project.path())? {
            let entry = entry?;
            if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(entry.path());
            }
        }
    }
    Ok(out)
}

/// Newest session file by mtime across every project.
///
/// # Errors
///
/// Returns an error if no `*.jsonl` files exist under `~/.claude/projects/`.
pub fn newest_session_anywhere() -> Result<PathBuf, String> {
    let projects = claude_projects_dir()?;
    let files = walk_session_files(&projects)
        .map_err(|e| format!("scanning {}: {e}", projects.display()))?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for path in files {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        match &best {
            Some((cur, _)) if &mtime <= cur => {}
            _ => best = Some((mtime, path)),
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| format!("no .jsonl sessions found under {}", projects.display()))
}

/// Locate a session file by its UUID stem.
///
/// # Errors
///
/// Returns an error if no session with that id is found.
pub fn find_session_by_id(id: &str) -> Result<PathBuf, String> {
    let projects = claude_projects_dir()?;
    let target = format!("{id}.jsonl");
    let files = walk_session_files(&projects)
        .map_err(|e| format!("scanning {}: {e}", projects.display()))?;
    for path in files {
        if path.file_name().and_then(|s| s.to_str()) == Some(&target) {
            return Ok(path);
        }
    }
    Err(format!(
        "session {id} not found under {} — try listing available sessions",
        projects.display()
    ))
}

/// One discovered session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Last-modified time of the session JSONL.
    pub mtime: SystemTime,
    /// UUID stem (filename without `.jsonl`).
    pub id: String,
    /// Project slug (parent directory name, with `/`→`-` encoding).
    pub project: String,
    /// Absolute path to the session file.
    pub path: PathBuf,
}

/// Enumerate all known sessions, newest first.
///
/// # Errors
///
/// Returns an error if `$HOME` is unset or the projects directory cannot
/// be walked.
pub fn list_sessions() -> Result<Vec<SessionInfo>, String> {
    let projects = claude_projects_dir()?;
    let files = walk_session_files(&projects)
        .map_err(|e| format!("scanning {}: {e}", projects.display()))?;
    let mut rows: Vec<SessionInfo> = files
        .into_iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            let mtime = meta.modified().ok()?;
            let id = p.file_stem()?.to_str()?.to_string();
            let project = p.parent()?.file_name()?.to_str()?.to_string();
            Some(SessionInfo {
                mtime,
                id,
                project,
                path: p,
            })
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.mtime));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_signature_in_nested_thinking_block() {
        let mut value = json!({
            "message": {
                "content": [
                    {"type": "thinking", "thinking": "...", "signature": "EoBqClkIDR=="},
                    {"type": "text", "text": "hello"},
                ],
            },
        });
        assert!(strip_signatures(&mut value));
        let content = &value["message"]["content"];
        assert!(content[0].get("signature").is_none());
        assert_eq!(content[0]["thinking"], "...");
        assert_eq!(content[1]["text"], "hello");
    }

    #[test]
    fn leaves_clean_payload_untouched() {
        let mut value = json!({"role": "user", "content": "alice met bob"});
        assert!(!strip_signatures(&mut value));
    }

    #[test]
    fn non_json_text_line_is_passed_through() {
        let mut msg = IOMessage::user_text("alice met bob");
        strip_signatures_in_message(&mut msg);
        match &msg.content {
            IOContent::Text(s) => assert_eq!(s, "alice met bob"),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn json_text_line_is_rewritten_without_signature() {
        let raw = r#"{"content":[{"type":"thinking","signature":"abc"}]}"#;
        let mut msg = IOMessage::user_text(raw);
        strip_signatures_in_message(&mut msg);
        match &msg.content {
            IOContent::Text(s) => {
                assert!(!s.contains("signature"));
                let v: serde_json::Value = serde_json::from_str(s).expect("still valid JSON");
                assert!(v["content"][0].get("signature").is_none());
            }
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn drops_top_level_permission_and_history_events() {
        let perm = IOMessage::user_text(r#"{"type":"permission-mode","permissionMode":"default"}"#);
        let snap = IOMessage::user_text(r#"{"type":"file-history-snapshot","messageId":"x"}"#);
        assert!(is_low_signal_event_message(&perm));
        assert!(is_low_signal_event_message(&snap));
    }

    #[test]
    fn drops_skill_listing_and_deferred_tools_attachments() {
        let skill = IOMessage::user_text(
            r#"{"type":"attachment","attachment":{"type":"skill_listing","content":"..."}}"#,
        );
        let tools = IOMessage::user_text(
            r#"{"type":"attachment","attachment":{"type":"deferred_tools_delta","addedNames":[]}}"#,
        );
        assert!(is_low_signal_event_message(&skill));
        assert!(is_low_signal_event_message(&tools));
    }

    #[test]
    fn passes_through_user_messages_and_tool_results() {
        let user =
            IOMessage::user_text(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#);
        let result = IOMessage::user_text(
            r#"{"type":"attachment","attachment":{"type":"tool_result","content":"ok"}}"#,
        );
        assert!(!is_low_signal_event_message(&user));
        assert!(!is_low_signal_event_message(&result));
    }

    #[test]
    fn passes_through_non_json_text() {
        let msg = IOMessage::user_text("alice met bob");
        assert!(!is_low_signal_event_message(&msg));
    }

    #[test]
    fn structured_low_signal_value_is_dropped() {
        let value = json!({"type": "permission-mode", "permissionMode": "default"});
        assert!(is_low_signal_event_value(&value));
    }
}
