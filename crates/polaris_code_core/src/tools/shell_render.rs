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
//! honest report, and the trajectory and the host transcript keep it — only the
//! model's copy is rendered.
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

use serde_json::Value;

/// Render `run_command`'s response for the model, or `None` if `value` is not
/// one (in which case the caller keeps whatever the tool returned).
pub(crate) fn render_run_command(value: &Value) -> Option<String> {
    match value.get("status").and_then(Value::as_str)? {
        "executed" => Some(render_executed(value)),
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

/// A completed run, as the text it produced plus a status marker.
fn render_executed(value: &Value) -> String {
    let mut out = String::new();

    let stdout = text(value, "stdout");
    if !stdout.is_empty() {
        out.push_str(stdout.trim_end_matches('\n'));
        out.push('\n');
    }
    let stderr = text(value, "stderr");
    if !stderr.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(stderr.trim_end_matches('\n'));
        out.push('\n');
    }
    for (stream, truncated, overflow) in [
        ("stdout", "stdout_truncated", "stdout_overflow_path"),
        ("stderr", "stderr_truncated", "stderr_overflow_path"),
    ] {
        if value.get(truncated).and_then(Value::as_bool) == Some(true) {
            match value.get(overflow).and_then(Value::as_str) {
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
            render_run_command(&executed("line one\nline two\n", "boom\n", Some(3))).unwrap();
        assert_eq!(
            rendered,
            "line one\nline two\n[stderr]\nboom\n[exit code: 3]"
        );
    }

    #[test]
    fn stderr_alone_is_still_attributed_to_stderr() {
        let rendered = render_run_command(&executed("", "warning: unused\n", Some(0))).unwrap();
        assert_eq!(rendered, "[stderr]\nwarning: unused\n[exit code: 0]");
    }

    #[test]
    fn a_silent_run_is_one_line() {
        assert_eq!(
            render_run_command(&executed("", "", Some(0))).unwrap(),
            "[no output; exit code: 0]"
        );
    }

    #[test]
    fn a_timeout_says_how_long_it_waited_and_what_to_change() {
        let mut value = executed("partial\n", "", None);
        value["timed_out"] = json!(true);
        value["outcome"] = json!("timed_out");
        value["duration"] = json!({ "secs": 30, "nanos": 0 });
        let rendered = render_run_command(&value).unwrap();
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
            render_run_command(&value).unwrap(),
            "[no output; cancelled before it exited]"
        );
    }

    #[test]
    fn truncated_output_names_the_file_holding_the_rest() {
        let mut value = executed("first half\n", "", Some(0));
        value["stdout_truncated"] = json!(true);
        value["stdout_overflow_path"] = json!("/cache/run-1.stdout");
        let rendered = render_run_command(&value).unwrap();
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
        let rendered = render_run_command(&value).unwrap();
        assert!(rendered.contains("not run"), "{rendered}");
        assert!(rendered.contains("cargo publish"), "{rendered}");
    }

    #[test]
    fn a_value_that_is_not_a_shell_response_is_left_alone() {
        assert!(render_run_command(&json!("just a string")).is_none());
        assert!(render_run_command(&json!({ "status": "something else" })).is_none());
        assert!(render_run_command(&json!({ "exit_code": 0 })).is_none());
    }

    #[test]
    fn the_render_is_smaller_than_the_document_it_replaces() {
        // The budget claim, asserted rather than assumed: escaping newlines
        // and carrying bookkeeping fields costs more than the markers do.
        let value = executed("line one\nline two\n", "boom\n", Some(3));
        let rendered = render_run_command(&value).unwrap();
        assert!(
            rendered.len() < value.to_string().len() / 2,
            "rendered {} bytes vs {} bytes of JSON",
            rendered.len(),
            value.to_string().len(),
        );
    }
}
