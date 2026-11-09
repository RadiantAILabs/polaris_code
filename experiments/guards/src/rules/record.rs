//! The machine-readable record a run writes: what its setup did, in facts a
//! program can read.
//!
//! Everything here is something the host observed or decided, never something
//! the model said about itself. The record is written as JSON, replaced whole
//! on every event (so a run that is killed still leaves a readable file) and
//! once more at the end with the end reason.

use serde::Serialize;
use serde_json::Value;

use super::files::TamperKind;

/// The record's format version. Bumped when a field changes meaning.
pub const SCHEMA: u32 = 1;

/// Which mechanisms a run had, so no reader has to infer them from the arm.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Mechanisms {
    /// The rules are in the system prompt. Route advertisement follows its
    /// separate configuration.
    pub prompt_rules: bool,
    /// Each file and shell tool call is checked before it runs.
    pub per_call_guard: bool,
    /// The test command can run over a sealed copy of the tests.
    pub sealed_check: bool,
    /// The run ends as done only when a sealed check passed after the last edit.
    pub done_gate: bool,
    /// Protected files are put back after a step that changed them.
    pub restore_protected: bool,
    /// The final report is built from recorded checks, and an unsupported
    /// claim that tests pass is flagged.
    pub report_check: bool,
    /// After the run ends, the test command is run once over the sealed copy
    /// and recorded. It does not influence the run.
    pub final_observation: bool,
    /// The files the work is made of are read, after each step and when the
    /// agent tries to finish, for code that games the tests, and the run does
    /// not end as done while such code is there.
    pub solution_scan: bool,
}

/// The per-run inputs, as the run received them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Inputs {
    /// The working directory.
    pub workspace: String,
    /// Protected paths, relative to the working directory.
    pub protected: Vec<String>,
    /// The test command, when one was given.
    pub test_command: Option<String>,
    /// The time limit of a sealed check, in seconds.
    pub check_timeout_secs: u64,
    /// How many times the done gate may send the agent back before the run
    /// ends as not done.
    pub max_holds: u32,
    /// Whether the final-message give-up route was explicitly disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_give_up: Option<bool>,
    /// Replacement rule words, including whitespace from the input file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_text: Option<String>,
    /// Conversation message ceiling, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_messages: Option<usize>,
}

/// One model attempt, as billed.
#[derive(Debug, Clone, Serialize)]
pub struct ModelCall {
    /// 1-based, in order.
    pub index: u32,
    /// Milliseconds since the run started, when the attempt ended.
    pub at_ms: u64,
    /// Whether the attempt produced a response.
    pub ok: bool,
    /// Full-price input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_creation_tokens: u64,
    /// Why the response stopped, when it produced one.
    pub stop_reason: Option<String>,
}

/// One tool call and how it ended.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRecord {
    /// 1-based, in order of the conversation.
    pub index: u32,
    /// The provider's id for the call.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// The arguments, each long string cut to an excerpt.
    pub arguments: Value,
    /// `ok`, `error`, `refused` (by the per-call check) or `denied` (by the
    /// permission policy).
    pub outcome: &'static str,
    /// The first part of the result text.
    pub result_excerpt: String,
}

/// A call the per-call check refused.
#[derive(Debug, Clone, Serialize)]
pub struct RefusalRecord {
    /// 1-based, in order.
    pub index: u32,
    /// The provider's id for the refused call.
    pub tool_call_id: String,
    /// The tool name.
    pub tool: String,
    /// The rule the call would break.
    pub rule: u8,
    /// A short machine-readable kind.
    pub code: String,
    /// The reason the model was given.
    pub reason: String,
    /// The arguments, each long string cut to an excerpt.
    pub arguments: Value,
    /// SHA-256 of the full arguments.
    pub arguments_sha256: String,
}

/// One sealed check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckRecord {
    /// 1-based, in order.
    pub index: u32,
    /// What started it: `done_gate` or `final_observation`.
    pub trigger: &'static str,
    /// Digest of the working tree it ran over.
    pub workspace_sha256: String,
    /// The tool-step count when it ran (0 for a check before any step).
    pub after_tool_calls: u32,
    /// The check's own result.
    #[serde(flatten)]
    pub result: super::check::CheckResult,
}

/// One decision of the done gate.
#[derive(Debug, Clone, Serialize)]
pub struct GateEvent {
    /// 1-based, in order.
    pub index: u32,
    /// `open`, `hold`, `exhausted` or `give_up`.
    pub action: &'static str,
    /// Why, in a sentence.
    pub reason: String,
}

/// A protected file found changed after a step.
#[derive(Debug, Clone, Serialize)]
pub struct TamperRecord {
    /// The tool-call count when it was found.
    pub after_tool_calls: u32,
    /// The path.
    pub path: String,
    /// What had happened to it.
    pub kind: TamperKind,
    /// Whether it was put back.
    pub restored: bool,
}

/// Code that games the tests, found in a file of the work by a step.
#[derive(Debug, Clone, Serialize)]
pub struct SolutionRecord {
    /// The tool-call count when it was found.
    pub after_tool_calls: u32,
    /// What found it: `after_step` or `done_gate`.
    pub trigger: &'static str,
    /// The file, relative to the working directory.
    pub path: String,
    /// A short machine-readable kind.
    pub code: String,
    /// The sentence the agent was told.
    pub reason: String,
}

/// How a protected path stood when the run ended.
#[derive(Debug, Clone, Serialize)]
pub struct ProtectedState {
    /// The path.
    pub path: String,
    /// `modified`, `deleted` or `created`.
    pub state: TamperKind,
}

/// The whole record.
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    /// [`SCHEMA`].
    pub schema: u32,
    /// `A0` to `A5`.
    pub arm: &'static str,
    /// The arm's command-line name.
    pub arm_name: &'static str,
    /// SHA-256 of the rules text put in the prompt; absent when none was.
    pub rules_text_sha256: Option<String>,
    /// The mechanisms this run had.
    pub mechanisms: Mechanisms,
    /// The per-run inputs.
    pub inputs: Inputs,
    /// The model the run was configured with.
    pub model: Option<String>,
    /// Wall-clock start, milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
    /// Wall-clock end, milliseconds since the Unix epoch.
    pub ended_unix_ms: Option<u64>,
    /// Every model attempt, as billed.
    pub model_calls: Vec<ModelCall>,
    /// Every tool call and its outcome.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Every call the per-call check refused.
    pub refusals: Vec<RefusalRecord>,
    /// Every sealed check and its result.
    pub checks: Vec<CheckRecord>,
    /// Every decision of the done gate.
    pub gate_events: Vec<GateEvent>,
    /// Every protected file found changed after a step.
    pub tamper_events: Vec<TamperRecord>,
    /// Every piece of code that games the tests, found in the work by a step.
    pub solution_findings: Vec<SolutionRecord>,
    /// How the protected paths stood when the run ended, against the seal.
    pub protected_at_end: Vec<ProtectedState>,
    /// The model's last message, exactly as it wrote it.
    pub model_final_message: Option<String>,
    /// The final message as the run reported it. Differs from the model's only
    /// when the report check added to it.
    pub reported_final_message: Option<String>,
    /// Whether the model's last message claims that tests pass.
    pub claims_tests_pass: bool,
    /// The reason after `CANNOT_COMPLETE:`, when the run gave up.
    pub give_up_reason: Option<String>,
    /// `done`, `gave_up`, `not_done`, `limits`, `error`, `interrupted` or
    /// `unfinished` (the file was last written before the run ended).
    pub end_reason: String,
    /// A sentence of detail for `limits`, `error` and `not_done`.
    pub end_detail: Option<String>,
    /// Conversation count at the last boundary, or the proposed count of a
    /// rejected append. Includes one system message and each tool result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    /// The process exit code the run reported.
    pub exit_code: Option<i32>,
    /// The run's token and cost accounting.
    pub usage: Option<Value>,
}

/// Cut every long string in `value` to `max` characters, keeping its shape.
#[must_use]
pub fn excerpt_value(value: &Value, max: usize) -> Value {
    match value {
        Value::String(s) if s.chars().count() > max => {
            let head: String = s.chars().take(max).collect();
            Value::String(format!("{head}…[{} characters in all]", s.chars().count()))
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| excerpt_value(v, max)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), excerpt_value(v, max)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The first `max` characters of `text`.
#[must_use]
pub fn excerpt(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn long_strings_are_cut_and_short_ones_kept() {
        let long = "x".repeat(50);
        let value = json!({"path": "a.py", "content": long, "nested": ["y".repeat(30)]});
        let cut = excerpt_value(&value, 10);
        assert_eq!(cut["path"], "a.py");
        assert!(
            cut["content"]
                .as_str()
                .unwrap()
                .contains("50 characters in all")
        );
        assert!(
            cut["nested"][0]
                .as_str()
                .unwrap()
                .starts_with("yyyyyyyyyy…")
        );
    }

    #[test]
    fn excerpt_counts_characters_not_bytes() {
        assert_eq!(excerpt("héllo", 10), "héllo");
        assert_eq!(excerpt("héllo wörld", 5), "héllo…");
    }
}
