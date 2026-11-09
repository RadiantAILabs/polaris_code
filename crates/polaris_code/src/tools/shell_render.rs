//! Rendering `run_command`'s response as the text a shell produced.
//!
//! The shell toolset answers with a JSON document — `{"status":"executed",
//! "exit_code":3,"stdout":"line one\nline two\n",…}` — which is the right shape
//! for a host that wants to branch on the fields. It is the wrong shape for the
//! model: every newline arrives escaped, the exit status is one field among
//! fourteen, and half the document is executor bookkeeping (`process`,
//! `dropped_events`, `cleanup_completed`) that no next tool call depends on.
//!
//! This module is the consumer-side render of that document, applied where the
//! coding agent turns a tool value into a `tool_result` block. It is a
//! polaris-code concern and not an upstream one: the JSON is the executor's
//! honest report, and the trajectory keeps it. The model and host transcript
//! show rendered text; the host block also carries the bounded JSON document
//! in its `document` metadata.
//!
//! The rendered shape, smallest first:
//!
//! ```text
//! [no output; exit code: 0]
//!
//! line one
//! line two
//! [stderr]
//! boom
//! [exit code: 3]
//! ```
//!
//! Unlabelled text is stdout; a `[stderr]` line introduces stderr; the last
//! line is always the status marker.
//!
//! A long stream is shown as its start and its end, with a line saying how
//! much was left out between them ([`StreamView`]). Build logs and test runs
//! put their errors and summaries at the end, so the end is the part the
//! model most needs; the start says what ran. When the executor's capture was
//! cut at its byte cap and the whole stream went to an overflow file, the end
//! is read from that file.

use std::io::{Read, Seek, SeekFrom};

use serde_json::Value;

/// Default characters of one stream the model sees: half from the start and
/// half from the end.
pub const DEFAULT_STREAM_VIEW_CHARS: usize = 10_000;

/// How much of each `run_command` output stream reaches the model and terminal.
///
/// The trajectory keeps the executor's document, and the overflow file keeps
/// the whole stream.
///
/// # Examples
///
/// ```
/// use polaris_code::tools::shell_render::StreamView;
///
/// let bounded = StreamView::bounded(2_000);
/// assert_eq!(bounded.max_chars, Some(2_000));
/// assert_eq!(StreamView::FULL.max_chars, None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamView {
    /// Most characters of one stream shown, split between its start and its
    /// end. `None` shows the whole capture.
    pub max_chars: Option<usize>,
}

impl StreamView {
    /// Show every captured character.
    pub const FULL: Self = Self { max_chars: None };

    /// Show at most `max_chars` characters of each stream.
    #[must_use]
    pub const fn bounded(max_chars: usize) -> Self {
        Self {
            max_chars: Some(max_chars),
        }
    }
}

impl Default for StreamView {
    /// [`DEFAULT_STREAM_VIEW_CHARS`].
    fn default() -> Self {
        Self::bounded(DEFAULT_STREAM_VIEW_CHARS)
    }
}

/// Render `run_command`'s response for the model, or `None` if `value` is not
/// one (in which case the caller keeps whatever the tool returned).
/// Captures and optional stdout/stderr tails must be redacted before rendering.
pub(crate) fn render_run_command(
    value: &Value,
    view: StreamView,
    tails: [Option<&str>; 2],
) -> Option<String> {
    match value.get("status").and_then(Value::as_str)? {
        "executed" => Some(render_executed(value, view, tails)),
        // The shell's own policy wants an approval that never came. Say what
        // did *not* happen, so the model does not read an empty result as a
        // command that ran and printed nothing.
        "confirmation_required" => Some(format!(
            "[not run: the shell policy requires confirmation for: {}]",
            text(value, "command")
        )),
        _ => None,
    }
}

/// How a completed run ended and how much it printed, for the collapsed
/// terminal row (`exit 0 · 40 lines`), or `None` if `value` is not one.
///
/// Counted over the whole of stdout and stderr, not the bounded display copy,
/// so the number is what the command produced.
pub(crate) fn run_command_outcome(value: &Value) -> Option<String> {
    if value.get("status").and_then(Value::as_str)? != "executed" {
        return None;
    }
    let outcome = value.get("outcome").and_then(Value::as_str).unwrap_or("");
    let ended = if value.get("timed_out").and_then(Value::as_bool) == Some(true)
        || outcome == "timed_out"
    {
        "timed out".to_string()
    } else if outcome == "cancelled" {
        "cancelled".to_string()
    } else {
        match value.get("exit_code").and_then(Value::as_i64) {
            Some(code) => format!("exit {code}"),
            None => "killed".to_string(),
        }
    };
    let lines = ["stdout", "stderr"]
        .iter()
        .map(|stream| text(value, stream).trim_end_matches('\n').lines().count())
        .sum::<usize>();
    Some(match lines {
        0 => format!("{ended} · no output"),
        1 => format!("{ended} · 1 line"),
        n => format!("{ended} · {n} lines"),
    })
}

/// Whether `value` is a run that exited 0 — the only result whose output is
/// safe to fold behind a one-line summary. A failure, timeout, kill or
/// cancellation keeps its output in view: that text is the reason.
pub(crate) fn run_command_succeeded(value: &Value) -> bool {
    value.get("status").and_then(Value::as_str) == Some("executed")
        && value.get("timed_out").and_then(Value::as_bool) != Some(true)
        && !matches!(
            value.get("outcome").and_then(Value::as_str),
            Some("timed_out" | "cancelled")
        )
        && value.get("exit_code").and_then(Value::as_i64) == Some(0)
}

/// A completed run, as the text it produced plus a status marker.
fn render_executed(value: &Value, view: StreamView, tails: [Option<&str>; 2]) -> String {
    let mut out = String::new();
    for ((stream, truncated, overflow, label), tail) in [
        ("stdout", "stdout_truncated", "stdout_overflow_path", None),
        (
            "stderr",
            "stderr_truncated",
            "stderr_overflow_path",
            Some("[stderr]\n"),
        ),
    ]
    .into_iter()
    .zip(tails)
    {
        let captured = text(value, stream);
        if captured.is_empty() {
            continue;
        }
        let cut = value.get(truncated).and_then(Value::as_bool) == Some(true);
        let path = value.get(overflow).and_then(Value::as_str);
        if let Some(label) = label {
            out.push_str(label);
        }
        out.push_str(view_stream(captured, tail, view).trim_end_matches('\n'));
        out.push('\n');
        if cut {
            match path {
                Some(path) => out.push_str(&format!(
                    "[{stream} truncated at the shell's output cap; all of it is in {path}]\n"
                )),
                None => out.push_str(&format!("[{stream} truncated at the shell's output cap]\n")),
            }
        }
    }

    let marker = status_marker(value);
    if out.is_empty() {
        format!("[no output; {marker}]")
    } else {
        format!("{out}[{marker}]")
    }
}

/// The part of one stream the model sees under `view`.
///
/// A stream within the budget is returned whole. A longer one keeps its first
/// and last halves with an omission line between. When the capture was `cut`
/// at the executor's cap and an overflow file holds the whole stream, the
/// last half comes from that file, so the real end of the output is shown.
fn view_stream(captured: &str, tail: Option<&str>, view: StreamView) -> String {
    let Some(budget) = view.max_chars else {
        return captured.to_string();
    };
    let half = budget / 2;
    match tail {
        Some(tail) => format!(
            "{}\n[... middle of the output omitted ...]\n{}",
            head_chars(captured, half),
            tail_chars(tail.trim_end_matches('\n'), half)
        ),
        None => {
            let total = captured.chars().count();
            if total <= budget {
                return captured.to_string();
            }
            let head = head_chars(captured, half);
            let tail = tail_chars(captured, half);
            let omitted = total - head.chars().count() - tail.chars().count();
            format!("{head}\n[... {omitted} characters omitted ...]\n{tail}")
        }
    }
}

/// At most `chars` characters from the start of `text`, ending at a line end
/// when one falls in the second half of that span.
fn head_chars(text: &str, chars: usize) -> &str {
    let end = text
        .char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index);
    let head = &text[..end];
    match head.rfind('\n') {
        Some(newline) if end == text.len() || newline >= end / 2 => &head[..newline],
        _ => head,
    }
}

/// At most `chars` characters from the end of `text`, starting after a line
/// end when one falls in the first half of that span.
fn tail_chars(text: &str, chars: usize) -> &str {
    let total = text.chars().count();
    let start = text
        .char_indices()
        .nth(total.saturating_sub(chars))
        .map_or(text.len(), |(index, _)| index);
    let tail = &text[start..];
    match tail.find('\n') {
        Some(newline) if start > 0 && newline <= tail.len() / 2 => &tail[newline + 1..],
        _ => tail,
    }
}

/// Read bounded overflow windows off the async worker. The caller must redact
/// these windows and the captured streams before passing them to the renderer.
pub(crate) async fn overflow_tails(value: &Value, view: StreamView) -> [Option<String>; 2] {
    let Some(budget) = view.max_chars else {
        return [None, None];
    };
    let paths = [
        ("stdout_truncated", "stdout_overflow_path"),
        ("stderr_truncated", "stderr_overflow_path"),
    ]
    .map(|(cut, path)| {
        (value.get(cut).and_then(Value::as_bool) == Some(true))
            .then(|| value.get(path).and_then(Value::as_str).map(String::from))
            .flatten()
    });
    if paths.iter().all(Option::is_none) {
        return [None, None];
    }
    tokio::task::spawn_blocking(move || {
        paths.map(|path| path.and_then(|path| file_tail(&path, budget / 2)))
    })
    .await
    .unwrap_or([None, None])
}

/// Read at most four bytes per requested character. Discard a leading partial
/// line so its missing assignment label cannot hide a secret from redaction.
/// Character selection happens only after the caller redacts the whole window.
fn file_tail(path: &str, chars: usize) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    read_tail_window(file, metadata.len(), chars)
}

/// The length is a snapshot: a writer may append before or during this read.
fn read_tail_window(mut file: impl Read + Seek, len: u64, chars: usize) -> Option<String> {
    let want = u64::try_from(chars.saturating_mul(4)).ok()?.min(len);
    file.seek(SeekFrom::Start(len - want)).ok()?;
    let mut bytes = Vec::with_capacity(usize::try_from(want).ok()?);
    file.take(want).read_to_end(&mut bytes).ok()?;
    let bytes = if want < len {
        match bytes.iter().position(|byte| *byte == b'\n') {
            Some(newline) => &bytes[newline + 1..],
            None => &[],
        }
    } else {
        &bytes
    };
    let text = String::from_utf8_lossy(bytes);
    // A private-key record spans lines. If its first delimiter is a footer,
    // the header was outside this window; withhold the incomplete record.
    let mut end = 0;
    if want < len {
        for line in text.split_inclusive('\n') {
            end += line.len();
            if line.contains("PRIVATE KEY-----") {
                if line.contains("-----END ") {
                    return Some(text[end..].to_string());
                }
                if line.contains("-----BEGIN ") {
                    break;
                }
            }
        }
    }
    Some(text.into_owned())
}

/// How the run ended, in the terms the model's next decision turns on.
fn status_marker(value: &Value) -> String {
    let outcome = value.get("outcome").and_then(Value::as_str).unwrap_or("");
    let timed_out = value.get("timed_out").and_then(Value::as_bool) == Some(true);
    if timed_out || outcome == "timed_out" {
        // The one case with advice attached: the model chose the timeout and
        // can choose a different one.
        return format!(
            "timed out after {}s and was killed; rerun with a larger timeout_secs \
             or a narrower command",
            seconds(value)
        );
    }
    if outcome == "cancelled" {
        return "cancelled before it exited".to_string();
    }
    match value.get("exit_code").and_then(Value::as_i64) {
        Some(code) => format!("exit code: {code}"),
        None => "killed before it exited; no exit code".to_string(),
    }
}

/// Whole seconds of the run's `duration`, which serializes as `{secs, nanos}`.
fn seconds(value: &Value) -> u64 {
    value
        .get("duration")
        .and_then(|d| d.get("secs"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// A string field, or `""` when absent or of another type.
fn text<'v>(value: &'v Value, field: &str) -> &'v str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_collapsed_outcome_counts_every_line_the_command_printed() {
        let value = json!({
            "status": "executed", "exit_code": 3, "outcome": "exited",
            "stdout": "a\nb\nc\n", "stderr": "boom\n",
        });
        assert_eq!(run_command_outcome(&value).unwrap(), "exit 3 · 4 lines");
        let quiet = json!({"status": "executed", "exit_code": 0, "stdout": "", "stderr": ""});
        assert_eq!(run_command_outcome(&quiet).unwrap(), "exit 0 · no output");
        let slow = json!({"status": "executed", "timed_out": true, "stdout": "x"});
        assert_eq!(run_command_outcome(&slow).unwrap(), "timed out · 1 line");
        assert!(
            run_command_outcome(&json!({"status": "confirmation_required"})).is_none(),
            "a command that never ran has no outcome"
        );
        let cancelled = json!({"status": "executed", "outcome": "cancelled", "stdout": ""});
        assert_eq!(
            run_command_outcome(&cancelled).unwrap(),
            "cancelled · no output"
        );
        let killed = json!({"status": "executed", "outcome": "exited", "stdout": "a\nb"});
        assert_eq!(run_command_outcome(&killed).unwrap(), "killed · 2 lines");
    }

    #[test]
    fn only_a_zero_exit_counts_as_success() {
        for (case, value, success) in [
            (
                "exit 0",
                json!({"status": "executed", "exit_code": 0}),
                true,
            ),
            (
                "exit 3",
                json!({"status": "executed", "exit_code": 3}),
                false,
            ),
            ("killed", json!({"status": "executed"}), false),
            (
                "timed out",
                json!({"status": "executed", "exit_code": 0, "timed_out": true}),
                false,
            ),
            (
                "cancelled",
                json!({"status": "executed", "exit_code": 0, "outcome": "cancelled"}),
                false,
            ),
            (
                "not run",
                json!({"status": "confirmation_required", "exit_code": 0}),
                false,
            ),
        ] {
            assert_eq!(run_command_succeeded(&value), success, "{case}");
        }
    }

    fn render_run_command_full(value: &Value) -> Option<String> {
        render_run_command(value, StreamView::FULL, [None, None])
    }

    /// The executor's document for a run that wrote to both streams and failed.
    fn executed(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Value {
        json!({
            "status": "executed",
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "timed_out": false,
            "stdout_truncated": false,
            "stderr_truncated": false,
            "stdout_overflow_path": null,
            "stderr_overflow_path": null,
            "outcome": "exited",
            "process": 1,
            "duration": { "secs": 0, "nanos": 120_764 },
            "dropped_events": 0,
            "cleanup_completed": true,
        })
    }

    #[test]
    fn both_streams_are_labelled_and_the_status_is_the_last_line() {
        let rendered =
            render_run_command_full(&executed("line one\nline two\n", "boom\n", Some(3))).unwrap();
        assert_eq!(
            rendered,
            "line one\nline two\n[stderr]\nboom\n[exit code: 3]"
        );
    }

    #[test]
    fn stderr_alone_is_still_attributed_to_stderr() {
        let rendered =
            render_run_command_full(&executed("", "warning: unused\n", Some(0))).unwrap();
        assert_eq!(rendered, "[stderr]\nwarning: unused\n[exit code: 0]");
    }

    #[test]
    fn a_silent_run_is_one_line() {
        assert_eq!(
            render_run_command_full(&executed("", "", Some(0))).unwrap(),
            "[no output; exit code: 0]"
        );
    }

    #[test]
    fn a_timeout_says_how_long_it_waited_and_what_to_change() {
        let mut value = executed("partial\n", "", None);
        value["timed_out"] = json!(true);
        value["outcome"] = json!("timed_out");
        value["duration"] = json!({ "secs": 30, "nanos": 0 });
        let rendered = render_run_command_full(&value).unwrap();
        assert!(rendered.contains("timed out after 30s"), "{rendered}");
        assert!(rendered.contains("timeout_secs"), "{rendered}");
        assert!(
            rendered.starts_with("partial\n"),
            "what the command managed to print is kept: {rendered}"
        );
    }

    #[test]
    fn a_cancelled_run_is_not_reported_as_a_clean_exit() {
        let mut value = executed("", "", None);
        value["outcome"] = json!("cancelled");
        assert_eq!(
            render_run_command_full(&value).unwrap(),
            "[no output; cancelled before it exited]"
        );
    }

    #[test]
    fn truncated_output_names_the_file_holding_the_rest() {
        let mut value = executed("first half\n", "", Some(0));
        value["stdout_truncated"] = json!(true);
        value["stdout_overflow_path"] = json!("/cache/run-1.stdout");
        let rendered = render_run_command_full(&value).unwrap();
        assert!(
            rendered.contains("/cache/run-1.stdout"),
            "the overflow file is named so the model can go and get it: {rendered}"
        );
        assert!(rendered.contains("truncated"), "{rendered}");
    }

    #[test]
    fn an_unapproved_command_says_it_did_not_run() {
        let value = json!({
            "status": "confirmation_required",
            "command": "cargo publish",
            "working_dir": null,
            "timeout_secs": null,
        });
        let rendered = render_run_command_full(&value).unwrap();
        assert!(rendered.contains("not run"), "{rendered}");
        assert!(rendered.contains("cargo publish"), "{rendered}");
    }

    #[test]
    fn a_value_that_is_not_a_shell_response_is_left_alone() {
        assert!(render_run_command_full(&json!("just a string")).is_none());
        assert!(render_run_command_full(&json!({ "status": "something else" })).is_none());
        assert!(render_run_command_full(&json!({ "exit_code": 0 })).is_none());
    }

    #[test]
    fn the_render_is_smaller_than_the_document_it_replaces() {
        // The budget claim, asserted rather than assumed: escaping newlines
        // and carrying bookkeeping fields costs more than the markers do.
        let value = executed("line one\nline two\n", "boom\n", Some(3));
        let rendered = render_run_command_full(&value).unwrap();
        assert!(
            rendered.len() < value.to_string().len() / 2,
            "rendered {} bytes vs {} bytes of JSON",
            rendered.len(),
            value.to_string().len(),
        );
    }

    #[test]
    fn a_long_stream_keeps_its_start_and_end() {
        let stdout: String = (0..2_000).map(|n| format!("line {n}\n")).collect();
        let rendered = render_run_command(
            &executed(&stdout, "", Some(1)),
            StreamView::bounded(200),
            [None, None],
        )
        .unwrap();
        assert!(rendered.starts_with("line 0\n"), "{rendered}");
        assert!(rendered.contains("line 1999\n[exit code: 1]"), "{rendered}");
        assert!(rendered.contains("characters omitted"), "{rendered}");
        assert!(rendered.len() < 400, "{} chars: {rendered}", rendered.len());
        assert!(!rendered.contains("line 1000\n"), "{rendered}");
    }

    #[test]
    fn a_stream_within_the_budget_is_shown_whole() {
        let rendered = render_run_command(
            &executed("a\nb\n", "", Some(0)),
            StreamView::bounded(200),
            [None, None],
        )
        .unwrap();
        assert_eq!(rendered, "a\nb\n[exit code: 0]");
    }

    #[tokio::test]
    async fn a_cut_capture_takes_its_end_from_the_overflow_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.stdout");
        let whole: String = (0..5_000).map(|n| format!("row {n}\n")).collect();
        std::fs::write(&path, &whole).unwrap();
        let mut value = executed(&whole[..1_000], "", Some(1));
        value["stdout_truncated"] = json!(true);
        value["stdout_overflow_path"] = json!(path.to_str().unwrap());
        let tails = overflow_tails(&value, StreamView::bounded(200)).await;
        let rendered = render_run_command(
            &value,
            StreamView::bounded(200),
            [tails[0].as_deref(), tails[1].as_deref()],
        )
        .unwrap();
        assert!(rendered.starts_with("row 0\n"), "{rendered}");
        assert!(
            rendered.contains("row 4999\n[stdout truncated"),
            "the real end comes from the file: {rendered}"
        );
        assert!(
            rendered.contains("middle of the output omitted"),
            "{rendered}"
        );
    }

    #[test]
    fn a_multibyte_stream_is_cut_on_character_boundaries() {
        let stdout = "é".repeat(1_000);
        let rendered = render_run_command(
            &executed(&stdout, "", Some(0)),
            StreamView::bounded(10),
            [None, None],
        )
        .unwrap();
        assert!(
            rendered.starts_with("ééééé\n[... 990 characters omitted ...]\nééééé\n"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn a_missing_overflow_file_falls_back_to_the_capture() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = executed("first\nlast\n", "", Some(3));
        value["stdout_truncated"] = json!(true);
        value["stdout_overflow_path"] = json!(dir.path().join("missing.stdout"));
        let tails = overflow_tails(&value, StreamView::bounded(200)).await;
        let rendered = render_run_command(
            &value,
            StreamView::bounded(200),
            [tails[0].as_deref(), tails[1].as_deref()],
        )
        .unwrap();
        assert!(rendered.starts_with("first\nlast\n"), "{rendered}");
        assert!(rendered.ends_with("[exit code: 3]"), "{rendered}");
    }

    #[tokio::test]
    async fn a_multibyte_file_tail_preserves_the_real_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.stdout");
        let whole = format!("{}終わり\n", "ééé\n".repeat(1000));
        std::fs::write(&path, &whole).unwrap();
        let mut value = executed("start\n", "", Some(0));
        value["stdout_truncated"] = json!(true);
        value["stdout_overflow_path"] = json!(path);
        let tails = overflow_tails(&value, StreamView::bounded(20)).await;
        let rendered = render_run_command(
            &value,
            StreamView::bounded(20),
            [tails[0].as_deref(), tails[1].as_deref()],
        )
        .unwrap();
        assert!(rendered.starts_with("start\n"), "{rendered}");
        assert!(rendered.contains("終わり\n[stdout truncated"), "{rendered}");
        assert!(!rendered.contains('�'), "{rendered}");
        assert!(rendered.ends_with("[exit code: 0]"), "{rendered}");
    }

    #[test]
    fn a_tail_read_drops_a_leading_partial_assignment() {
        let output = format!("PASSWORD={}hunter2\nFINAL ERROR\n", "x".repeat(1000));
        let tail = read_tail_window(
            std::io::Cursor::new(output.as_bytes()),
            output.len() as u64,
            20,
        )
        .unwrap();
        assert_eq!(tail, "FINAL ERROR\n");
    }

    #[test]
    fn a_tail_read_withholds_a_key_whose_header_is_outside_the_window() {
        let output = format!(
            "-----BEGIN PRIVATE KEY-----\n{}-----END PRIVATE KEY-----\nFINAL ERROR\n",
            "private-payload\n".repeat(1000),
        );
        let tail = read_tail_window(
            std::io::Cursor::new(output.as_bytes()),
            output.len() as u64,
            100,
        )
        .unwrap();
        assert_eq!(tail, "FINAL ERROR\n");
    }

    #[test]
    fn a_tail_read_stops_at_the_length_snapshot_even_if_the_file_grows() {
        let before = "old line\n".repeat(100);
        let grown = format!("{before}{}", "appended line\n".repeat(1000));
        let tail = read_tail_window(std::io::Cursor::new(grown), before.len() as u64, 10).unwrap();
        assert!(tail.ends_with("old line\n"), "{tail}");
        assert!(!tail.contains("appended"), "{tail}");
        assert!(tail.len() <= 40, "the read stays within its byte budget");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn overflow_reads_do_not_block_the_async_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        // Opening the FIFO waits for the writer. A separate thread releases
        // even a blocked runtime, so a regression fails instead of hanging.
        let writer = std::thread::spawn(move || {
            let progressed = rx.recv_timeout(std::time::Duration::from_secs(1)).is_ok();
            let _writer = std::fs::File::create(writer_path).unwrap();
            progressed
        });
        let value = json!({
            "stdout_truncated": true,
            "stdout_overflow_path": path,
        });
        let (tails, ()) = tokio::join!(overflow_tails(&value, StreamView::default()), async {
            tokio::task::yield_now().await;
            let _ = tx.send(());
        },);
        assert!(
            writer.join().unwrap(),
            "the async worker must keep making progress during open"
        );
        assert_eq!(
            tails,
            [None, None],
            "non-regular files use the captured fallback"
        );
    }
}
