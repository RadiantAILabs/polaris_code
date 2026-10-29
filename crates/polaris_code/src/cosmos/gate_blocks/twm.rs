//! Typed working memory for an opted-in lean or loop model step.
//!
//! In plain words: instead of only ever growing the chat transcript, keep a
//! few small typed slots (`WorkingSet`, `Edits`, `TestStatus`, `Errors`,
//! `Budget`, a model-writable `note`) that plain rules — not the model — keep
//! current. Between rebuilds
//! the transcript still just grows by appending, exactly as today. Each
//! time the whole prompt passes the configured size limit, the prompt is
//! rebuilt once from the slots plus a short recent tail, so per-turn cost
//! stays roughly flat instead of growing with the whole history.
//!
//! Every update rule here is a plain, deterministic function: same event,
//! same prior state, same result, no model call. [`TwmState::observe`] is the
//! one dispatcher that reads new messages and calls them; [`TwmState::render`]
//! is the one function that turns the current state into a prompt. Both are
//! plain data in, data out, and both are tested that way below — no live
//! model, no filesystem, needed for either.

use std::path::Path;

use polaris::models::llm::{
    AssistantBlock, LlmRequest, Message, ToolCall, ToolDefinition, ToolResultContent,
    ToolResultStatus, UserBlock,
};

use crate::tools::fileset::{WalkLimits, select_files};

/// The tool name the model calls to write its one small `note` slot.
pub(crate) const NOTE_TOOL: &str = "note";
const EDIT_FILE_TOOL_NAME: &str = "edit_file";
const READ_FILE_TOOL_NAME: &str = "read_file";
const AUTOTEST_MARKER: &str = "\nAutotest";

const TASK_BRIEF_CAP: usize = 6_000;
const WORKING_SET_CAP: usize = 40_000;
const WORKING_SET_ENTRY_CAP: usize = 8_000;
const EDITS_CAP: usize = 20_000;
const TEST_STATUS_CAP: usize = 4_000;
const ERRORS_CAP: usize = 2_000;
const NOTE_CAP: usize = 500;
const CODEMAP_CAP: usize = 6_000;
/// How many of the most recent model (assistant) turns — each with its own
/// tool results — a rebuild keeps verbatim after the rendered context. Kept
/// small and fixed rather than a switch: the point of a rebuild is to make
/// the kept tail small, so this is not a knob callers should need to widen.
const REBUILD_KEEP_TURNS: usize = 2;

/// Truncates to at most `max_chars` characters, always on a character
/// boundary — never a byte count, so multi-byte text cannot panic or split a
/// character in half.
fn cap_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(" [capped]");
    out
}

pub(crate) fn note_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        NOTE_TOOL,
        "Writes your one small working note (at most 500 characters; a new \
         call replaces the old text). Use it for something you want to \
         remember a few turns from now that is not a file, a diff or a test \
         result — those are already kept for you.",
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false,
        }),
    )
}

/// Env-configured switches for typed working memory. `None` (the parsed
/// result of the master switch being off) means TWM is inert; a model step
/// behaves exactly as it did before this module existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwmOptions {
    /// Whole prompt size that triggers a rebuild, estimated as JSON characters / 4.
    pub rebuild_tokens: usize,
    /// Offer the model its own small `note` slot.
    pub note: bool,
    /// Capture test-command output as a typed `TestStatus` entry.
    pub test_status: bool,
    /// Build a free, no-model-call code map into the unchanging first block.
    pub codemap: bool,
}

impl Default for TwmOptions {
    fn default() -> Self {
        Self {
            rebuild_tokens: 40_000,
            note: false,
            test_status: false,
            codemap: false,
        }
    }
}

impl TwmOptions {
    /// Reads the lean and loop arms' TWM switches. Returns `None` when the master
    /// switch (`POLARIS_CODE_LEAN_TWM`) is off or unset — the default. Call
    /// only for arms `lean` and `loop`; other arms ignore these variables.
    ///
    /// # Errors
    /// Rejects malformed booleans and a nonpositive/non-numeric rebuild
    /// trigger, regardless of whether the master switch is on.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Pure parsing seam for hosts and tests, without mutating process env.
    ///
    /// # Errors
    /// Same validation as [`Self::from_env`].
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let boolean = |key, default| match get(key).as_deref().map(str::trim) {
            None | Some("") => Ok(default),
            Some("0") => Ok(false),
            Some("1") => Ok(true),
            _ => Err(format!("{key} must be 0 or 1")),
        };
        let enabled = boolean("POLARIS_CODE_LEAN_TWM", false)?;
        let mut options = Self {
            note: boolean("POLARIS_CODE_LEAN_TWM_NOTE", false)?,
            test_status: boolean("POLARIS_CODE_LEAN_TWM_TESTSTATUS", false)?,
            codemap: boolean("POLARIS_CODE_LEAN_TWM_CODEMAP", false)?,
            ..Self::default()
        };
        if let Some(raw) =
            get("POLARIS_CODE_LEAN_TWM_REBUILD_TOKENS").filter(|s| !s.trim().is_empty())
        {
            options.rebuild_tokens = raw
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or("POLARIS_CODE_LEAN_TWM_REBUILD_TOKENS must be a positive integer")?;
        }
        Ok(enabled.then_some(options))
    }
}

/// One file range the agent has read, and what it currently shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkingSetEntry {
    path: String,
    start_line: Option<usize>,
    max_lines: Option<usize>,
    content: String,
    last_turn: u16,
}

/// Files the agent has read, each with the range looked at and that range's
/// current content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkingSet {
    entries: Vec<WorkingSetEntry>,
}

impl WorkingSet {
    /// Records a read, replacing any entry for the same `(path, start_line,
    /// max_lines)` instead of duplicating it, then enforces the total cap by
    /// dropping the least-recently-touched entry until the render fits.
    fn record(
        &mut self,
        path: &str,
        start_line: Option<usize>,
        max_lines: Option<usize>,
        content: &str,
        turn: u16,
    ) {
        let content = cap_chars(content, WORKING_SET_ENTRY_CAP);
        if let Some(existing) = self.entries.iter_mut().find(|entry| {
            entry.path == path && entry.start_line == start_line && entry.max_lines == max_lines
        }) {
            existing.content = content;
            existing.last_turn = turn;
        } else {
            self.entries.push(WorkingSetEntry {
                path: path.to_owned(),
                start_line,
                max_lines,
                content,
                last_turn: turn,
            });
        }
        self.enforce_cap();
    }

    /// Drops every range on `path` — an edit landed there, so the content the
    /// agent last saw is stale; a fresh `read_file` earns a current one.
    fn invalidate_path(&mut self, path: &str) {
        self.entries.retain(|entry| entry.path != path);
    }

    /// Drops ranges untouched for more than `max_age_turns`.
    fn drop_stale(&mut self, turn: u16, max_age_turns: u16) {
        self.entries
            .retain(|entry| turn.saturating_sub(entry.last_turn) <= max_age_turns);
    }

    fn enforce_cap(&mut self) {
        while self.rendered_len() > WORKING_SET_CAP && !self.entries.is_empty() {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_turn)
                .map(|(index, _)| index)
                .expect("checked non-empty above");
            self.entries.remove(oldest);
        }
    }

    fn rendered_len(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.content.len() + entry.path.len() + 32)
            .sum()
    }

    fn render(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("Working set (files read so far):\n");
        for entry in &self.entries {
            let range = match (entry.start_line, entry.max_lines) {
                (Some(start), Some(count)) => format!(" lines {start}..{}", start + count),
                (Some(start), None) => format!(" from line {start}"),
                _ => String::new(),
            };
            out.push_str(&format!(
                "--- {}{range} ---\n{}\n",
                entry.path, entry.content
            ));
        }
        out
    }
}

/// A unified-diff hunk for one edit, keyed by path for the drop-oldest rule.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditHunk {
    path: String,
    diff: String,
    turn: u16,
}

/// The current diff, built from every successful `edit_file` call's own
/// `old_string`/`new_string` — never a log of the calls themselves, a diff.
/// Because `edit_file` only ever applies an exact, unique match, each hunk
/// composes validly with the ones before it on the same path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Edits {
    hunks: Vec<EditHunk>,
}

impl Edits {
    fn record(&mut self, path: &str, old: &str, new: &str, turn: u16) {
        let diff = similar::TextDiff::from_lines(old, new)
            .unified_diff()
            .header(path, path)
            .to_string();
        self.hunks.push(EditHunk {
            path: path.to_owned(),
            diff,
            turn,
        });
        self.enforce_cap();
    }

    fn enforce_cap(&mut self) {
        while self.rendered_len() > EDITS_CAP && !self.hunks.is_empty() {
            self.hunks.remove(0);
        }
    }

    fn rendered_len(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.diff.len()).sum()
    }

    fn render(&self) -> String {
        if self.hunks.is_empty() {
            return String::new();
        }
        let mut out = String::from("Current edits (unified diff, oldest first):\n");
        for hunk in &self.hunks {
            out.push_str(&hunk.diff);
        }
        out
    }
}

/// The last result per test command — replaced, never appended, on each new
/// run of the same command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TestStatus {
    entries: Vec<(String, String)>,
}

impl TestStatus {
    fn record(&mut self, command: &str, output: &str) {
        let output = cap_chars(output, TEST_STATUS_CAP);
        if let Some(existing) = self.entries.iter_mut().find(|(key, _)| key == command) {
            existing.1 = output;
        } else {
            self.entries.push((command.to_owned(), output));
        }
        while self.rendered_len() > TEST_STATUS_CAP && self.entries.len() > 1 {
            self.entries.remove(0);
        }
    }

    fn rendered_len(&self) -> usize {
        self.entries.iter().map(|(_, v)| v.len()).sum()
    }

    fn render(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("Test status:\n");
        for (command, result) in &self.entries {
            out.push_str(&format!("- {command}: {result}\n"));
        }
        out
    }
}

/// The last error per tool — cleared once the same tool succeeds again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Errors {
    entries: Vec<(String, String)>,
}

impl Errors {
    fn record(&mut self, tool: &str, message: &str) {
        let message = cap_chars(message, ERRORS_CAP);
        if let Some(existing) = self.entries.iter_mut().find(|(key, _)| key == tool) {
            existing.1 = message;
        } else {
            self.entries.push((tool.to_owned(), message));
        }
    }

    fn clear(&mut self, tool: &str) {
        self.entries.retain(|(key, _)| key != tool);
    }

    fn render(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("Open tool errors:\n");
        for (tool, message) in &self.entries {
            out.push_str(&format!("- {tool}: {message}\n"));
        }
        out
    }
}

/// Turns used/left this step. It carries no cost estimate because the model
/// step's `TokenMeter` is not passed to the observer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Budget {
    turns_used: u16,
    turns_left: u16,
}

impl Budget {
    fn update(&mut self, turn: u16, max_turns: u16) {
        self.turns_used = turn;
        self.turns_left = max_turns.saturating_sub(turn);
    }

    fn render(&self) -> String {
        format!(
            "Budget: turn {} of {} ({} left).",
            self.turns_used,
            self.turns_used + self.turns_left,
            self.turns_left
        )
    }
}

/// The model's one small free-text slot — the only resource here the model
/// itself writes, via the `note` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Note {
    text: String,
}

impl Note {
    fn set(&mut self, text: &str) {
        self.text = cap_chars(text, NOTE_CAP);
    }

    fn render(&self) -> String {
        if self.text.is_empty() {
            String::new()
        } else {
            format!("Your note: {}", self.text)
        }
    }
}

/// One task's typed working memory, owned by a single [`LiveModelStep::run`]
/// call for the lifetime of that step. Not a graph-visible resource today —
/// nothing outside the step reads it.
#[derive(Debug, Clone)]
pub(crate) struct TwmState {
    options: TwmOptions,
    task_brief: String,
    codemap: Option<String>,
    working_set: WorkingSet,
    edits: Edits,
    test_status: TestStatus,
    errors: Errors,
    budget: Budget,
    note: Note,
    /// How many of the running step's messages have already been scanned by
    /// [`Self::observe`].
    observed_len: usize,
}

impl TwmState {
    pub(crate) fn new(options: TwmOptions, statement: &str, codemap: Option<String>) -> Self {
        Self {
            options,
            task_brief: cap_chars(statement, TASK_BRIEF_CAP),
            codemap: codemap.map(|text| cap_chars(&text, CODEMAP_CAP)),
            working_set: WorkingSet::default(),
            edits: Edits::default(),
            test_status: TestStatus::default(),
            errors: Errors::default(),
            budget: Budget::default(),
            note: Note::default(),
            observed_len: 0,
        }
    }

    /// Scans `messages[self.observed_len..]` for tool calls/results and
    /// applies the matching update rule to each. Deterministic: the same
    /// slice of messages always produces the same state change, and no rule
    /// here makes a model call.
    pub(crate) fn observe(&mut self, messages: &[Message], turn: u16, max_turns: u16) {
        for (index, message) in messages.iter().enumerate().skip(self.observed_len) {
            let Message::User { content } = message else {
                continue;
            };
            for block in content {
                let UserBlock::ToolResult(result) = block else {
                    continue;
                };
                let ToolResultContent::Text(text) = &result.content else {
                    continue;
                };
                let Some(call) = find_call(messages, index, &result.id) else {
                    continue;
                };
                let succeeded = result.status == ToolResultStatus::Success;
                let name = call.function.name.as_str();
                if succeeded {
                    self.errors.clear(name);
                } else {
                    self.errors.record(name, text);
                }
                if !succeeded {
                    continue;
                }
                match name {
                    READ_FILE_TOOL_NAME => self.observe_read(call, text, turn),
                    EDIT_FILE_TOOL_NAME => self.observe_edit(call, turn),
                    NOTE_TOOL if self.options.note => self.observe_note(call),
                    _ => {}
                }
                if self.options.test_status
                    && let Some(start) = text.find(AUTOTEST_MARKER)
                {
                    self.test_status.record("autotest", &text[start + 1..]);
                }
            }
        }
        self.observed_len = messages.len();
        self.budget.update(turn, max_turns);
    }

    fn observe_read(&mut self, call: &ToolCall, text: &str, turn: u16) {
        let Some(path) = call.function.arguments.get("path").and_then(|v| v.as_str()) else {
            return;
        };
        let start_line = call
            .function
            .arguments
            .get("start_line")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize);
        let max_lines = call
            .function
            .arguments
            .get("max_lines")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize);
        self.working_set
            .record(path, start_line, max_lines, text, turn);
    }

    fn observe_edit(&mut self, call: &ToolCall, turn: u16) {
        let args = &call.function.arguments;
        let (Some(path), Some(old), Some(new)) = (
            args.get("path").and_then(|v| v.as_str()),
            args.get("old_string").and_then(|v| v.as_str()),
            args.get("new_string").and_then(|v| v.as_str()),
        ) else {
            return;
        };
        self.edits.record(path, old, new, turn);
        self.working_set.invalidate_path(path);
    }

    fn observe_note(&mut self, call: &ToolCall) {
        if let Some(text) = call.function.arguments.get("text").and_then(|v| v.as_str()) {
            self.note.set(text);
        }
    }

    /// Drops working-set ranges nothing has touched in a while — called once
    /// per turn from the same place [`Self::observe`] is.
    pub(crate) fn age_out(&mut self, turn: u16) {
        const MAX_AGE_TURNS: u16 = 12;
        self.working_set.drop_stale(turn, MAX_AGE_TURNS);
    }

    /// Whether the next whole request exceeds the size limit. Counts system,
    /// tools, all messages (including a prior render and its kept tail), and
    /// JSON framing. Uses characters, not UTF-8 bytes; no turn/growth trigger.
    pub(crate) fn should_rebuild(&self, request: &LlmRequest) -> bool {
        serde_json::to_string(request)
            .is_ok_and(|json| json.chars().count() > self.options.rebuild_tokens.saturating_mul(4))
    }

    /// Rebuilds `messages` into a fresh context message followed by only the
    /// last [`REBUILD_KEEP_TURNS`] model turns (each with its own tool
    /// results) — never the whole history since the last rebuild, which
    /// would never shrink and would leave the byte trigger permanently
    /// crossed once it was crossed once. The kept part always starts at an
    /// assistant message (or at the conversation's own root, which is
    /// itself never split from what follows it), so no tool result is ever
    /// orphaned from the call it answers. The next size check includes this
    /// rendered context and the kept tail as well as any new messages.
    pub(crate) fn rebuild(&mut self, messages: &[Message]) -> Vec<Message> {
        let keep_from = tail_from_last_assistant_turns(messages, REBUILD_KEEP_TURNS);
        let tail = messages.get(keep_from..).unwrap_or_default();
        let mut out = vec![Message::user(self.render())];
        out.extend_from_slice(tail);
        self.observed_len = out.len();
        out
    }

    fn render_stable_block(&self) -> String {
        let mut out = self.task_brief.clone();
        if let Some(codemap) = &self.codemap {
            out.push_str("\n\nCode map:\n");
            out.push_str(codemap);
        }
        out
    }

    fn render_working_block(&self) -> String {
        let mut out = String::new();
        let working_set = self.working_set.render();
        if !working_set.is_empty() {
            out.push_str(&working_set);
        }
        let edits = self.edits.render();
        if !edits.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&edits);
        }
        out
    }

    fn render_status_block(&self) -> String {
        let mut parts = Vec::new();
        let test_status = self.test_status.render();
        if !test_status.is_empty() {
            parts.push(test_status);
        }
        let errors = self.errors.render();
        if !errors.is_empty() {
            parts.push(errors);
        }
        parts.push(self.budget.render());
        let note = self.note.render();
        if !note.is_empty() {
            parts.push(note);
        }
        parts.join("\n")
    }

    /// The whole rendered prompt prefix, in a fixed order:
    /// least likely to change first, most likely to change last.
    pub(crate) fn render(&self) -> String {
        let mut out = self.render_stable_block();
        let working = self.render_working_block();
        if !working.is_empty() {
            out.push_str("\n\n");
            out.push_str(&working);
        }
        let status = self.render_status_block();
        if !status.is_empty() {
            out.push_str("\n\n");
            out.push_str(&status);
        }
        out
    }
}

/// The index in `messages` where the last `keep_turns` assistant messages'
/// own turns begin — that is, the index of the assistant message that is
/// `keep_turns` back from the end, counting only assistant messages. Never
/// returns an index inside the conversation's own root message (index 0):
/// that message is always replaced by the caller's fresh rendered context,
/// never duplicated or split from what follows it, so when fewer than
/// `keep_turns` assistant messages exist this returns `1` (or `0` for a
/// messages list too short to have a message at index 1 at all) rather than
/// `0` — keeping everything after the root, never the root itself again.
fn tail_from_last_assistant_turns(messages: &[Message], keep_turns: usize) -> usize {
    let mut assistant_seen = 0usize;
    for (index, message) in messages.iter().enumerate().rev() {
        if index == 0 {
            break;
        }
        if matches!(message, Message::Assistant { .. }) {
            assistant_seen += 1;
            if assistant_seen == keep_turns {
                return index;
            }
        }
    }
    1.min(messages.len())
}

/// Finds the tool call answered by the result at `messages[before_index]`,
/// searching backward by call id — the same lookup
/// [`super::configurable`]'s own batched trim already does.
fn find_call<'a>(
    messages: &'a [Message],
    before_index: usize,
    result_id: &str,
) -> Option<&'a ToolCall> {
    messages[..before_index].iter().rev().find_map(|message| {
        let Message::Assistant { content, .. } = message else {
            return None;
        };
        content.iter().rev().find_map(|block| match block {
            AssistantBlock::ToolCall(call) if call.id == result_id => Some(call),
            _ => None,
        })
    })
}

/// A free, no-model-call map of top-level definitions across a bounded
/// sample of source files — Aider-style, placed in the unchanging first
/// block so it costs nothing to keep cached. Deterministic: the same tree
/// always produces the same map.
pub(crate) fn build_codemap(working_dir: &Path) -> String {
    let selection = select_files(
        working_dir,
        working_dir,
        None,
        true,
        WalkLimits {
            max_files: 60,
            max_file_bytes: 200_000,
        },
    );
    let definition = regex::Regex::new(
        r"^\s*(pub(\(crate\))?\s+)?(fn|struct|enum|trait|impl|class|def|interface|function|const|type)\s+\w+",
    )
    .expect("fixed pattern compiles");
    let mut out = String::new();
    'files: for path in &selection.files {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !matches!(ext, "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "go") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let relative = path
            .strip_prefix(working_dir)
            .unwrap_or(path)
            .to_string_lossy();
        let mut file_lines = Vec::new();
        for line in content.lines() {
            if definition.is_match(line) {
                file_lines.push(line.trim());
            }
        }
        if file_lines.is_empty() {
            continue;
        }
        let block = format!("{relative}:\n  {}\n", file_lines.join("\n  "));
        if out.len() + block.len() > CODEMAP_CAP {
            break 'files;
        }
        out.push_str(&block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::llm::{ToolFunction, ToolResult};

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                call_id: None,
                function: ToolFunction {
                    name: name.to_owned(),
                    arguments: args,
                },
                signature: None,
                additional_params: None,
            })],
        }
    }

    fn tool_result(id: &str, text: &str, status: ToolResultStatus) -> Message {
        Message::User {
            content: vec![UserBlock::ToolResult(ToolResult {
                id: id.to_owned(),
                call_id: None,
                content: ToolResultContent::Text(text.to_owned()),
                status,
            })],
        }
    }

    fn options() -> TwmOptions {
        TwmOptions {
            note: true,
            test_status: true,
            codemap: false,
            ..TwmOptions::default()
        }
    }

    // ---- env parsing ----

    #[test]
    fn master_switch_off_by_default_and_gates_the_others() {
        assert_eq!(TwmOptions::from_lookup(|_| None).unwrap(), None);
        let on =
            TwmOptions::from_lookup(|key| (key == "POLARIS_CODE_LEAN_TWM").then(|| "1".to_owned()))
                .unwrap()
                .expect("master switch on");
        assert_eq!(on, TwmOptions::default());
    }

    #[test]
    fn sub_switches_parse_regardless_of_the_master_switch() {
        let options = TwmOptions::from_lookup(|key| match key {
            "POLARIS_CODE_LEAN_TWM" => Some("1".to_owned()),
            "POLARIS_CODE_LEAN_TWM_NOTE" | "POLARIS_CODE_LEAN_TWM_TESTSTATUS" => {
                Some("1".to_owned())
            }
            "POLARIS_CODE_LEAN_TWM_REBUILD_TOKENS" => Some("999".to_owned()),
            _ => None,
        })
        .unwrap()
        .expect("master switch on");
        assert!(options.note && options.test_status && !options.codemap);
        assert_eq!(options.rebuild_tokens, 999);

        for (key, bad) in [
            ("POLARIS_CODE_LEAN_TWM", "yes"),
            ("POLARIS_CODE_LEAN_TWM_NOTE", "yes"),
            ("POLARIS_CODE_LEAN_TWM_REBUILD_TOKENS", "0"),
            ("POLARIS_CODE_LEAN_TWM_REBUILD_TOKENS", "-1"),
        ] {
            assert!(
                TwmOptions::from_lookup(|k| (k == key).then(|| bad.to_owned())).is_err(),
                "{key}={bad} should have been rejected"
            );
        }
    }

    // ---- update rules: WorkingSet ----

    #[test]
    fn working_set_add_then_replace_on_the_same_range() {
        let mut set = WorkingSet::default();
        set.record("a.rs", Some(1), Some(10), "old content", 1);
        assert_eq!(set.entries.len(), 1);
        set.record("a.rs", Some(1), Some(10), "new content", 2);
        assert_eq!(set.entries.len(), 1, "same range replaces, does not add");
        assert_eq!(set.entries[0].content, "new content");
        assert_eq!(set.entries[0].last_turn, 2);
    }

    #[test]
    fn working_set_distinct_ranges_add_separately() {
        let mut set = WorkingSet::default();
        set.record("a.rs", Some(1), Some(10), "x", 1);
        set.record("a.rs", Some(11), Some(10), "y", 1);
        assert_eq!(set.entries.len(), 2);
    }

    #[test]
    fn working_set_drops_the_least_recently_touched_entry_over_cap() {
        let mut set = WorkingSet::default();
        let big = "x".repeat(WORKING_SET_ENTRY_CAP);
        for turn in 0..6u16 {
            set.record(&format!("f{turn}.rs"), None, None, &big, turn);
        }
        assert!(set.rendered_len() <= WORKING_SET_CAP);
        assert!(
            !set.entries.iter().any(|e| e.path == "f0.rs"),
            "oldest-touched entry should have been dropped first"
        );
        assert!(set.entries.iter().any(|e| e.path == "f5.rs"));
    }

    #[test]
    fn working_set_drop_stale_removes_untouched_ranges() {
        let mut set = WorkingSet::default();
        set.record("a.rs", None, None, "x", 1);
        set.record("b.rs", None, None, "y", 10);
        set.drop_stale(20, 12);
        assert!(!set.entries.iter().any(|e| e.path == "a.rs"));
        assert!(set.entries.iter().any(|e| e.path == "b.rs"));
    }

    #[test]
    fn working_set_invalidate_path_drops_all_its_ranges() {
        let mut set = WorkingSet::default();
        set.record("a.rs", Some(1), Some(5), "x", 1);
        set.record("a.rs", Some(6), Some(5), "y", 1);
        set.record("b.rs", None, None, "z", 1);
        set.invalidate_path("a.rs");
        assert!(set.entries.iter().all(|e| e.path != "a.rs"));
        assert_eq!(set.entries.len(), 1);
    }

    // ---- update rules: Edits ----

    #[test]
    fn edits_record_produces_a_unified_diff_never_a_call_log() {
        let mut edits = Edits::default();
        edits.record("a.rs", "let x = 1;", "let x = 2;", 1);
        let rendered = edits.render();
        assert!(rendered.contains("-let x = 1;"));
        assert!(rendered.contains("+let x = 2;"));
        assert!(!rendered.contains("old_string"), "not a log of the calls");
    }

    #[test]
    fn edits_drops_oldest_hunk_over_cap() {
        let mut edits = Edits::default();
        let old = "a\n".repeat(2_000);
        let new = "b\n".repeat(2_000);
        edits.record("first.rs", &old, &new, 1);
        edits.record("second.rs", &old, &new, 2);
        assert!(edits.rendered_len() <= EDITS_CAP);
        assert!(!edits.hunks.iter().any(|h| h.path == "first.rs"));
    }

    // ---- update rules: TestStatus / Errors ----

    #[test]
    fn test_status_replaces_the_same_command_never_appends() {
        let mut status = TestStatus::default();
        status.record("pytest tests/", "1 failed");
        status.record("pytest tests/", "2 passed");
        assert_eq!(status.entries.len(), 1);
        assert!(status.render().contains("2 passed"));
        assert!(!status.render().contains("1 failed"));
    }

    #[test]
    fn errors_record_then_clear_on_success() {
        let mut errors = Errors::default();
        errors.record("run_command", "exit 1");
        assert!(errors.render().contains("run_command"));
        errors.clear("run_command");
        assert!(errors.render().is_empty());
    }

    // ---- render order ----

    #[test]
    fn render_order_is_brief_then_working_then_status() {
        let mut state = TwmState::new(options(), "fix the bug", None);
        state.working_set.record("a.rs", None, None, "content", 1);
        state.test_status.record("pytest", "ok");
        state.note.set("remember this");
        let rendered = state.render();
        let brief_at = rendered.find("fix the bug").unwrap();
        let working_at = rendered.find("Working set").unwrap();
        let status_at = rendered.find("Test status").unwrap();
        let note_at = rendered.find("remember this").unwrap();
        assert!(brief_at < working_at, "brief renders before working set");
        assert!(working_at < status_at, "working set renders before status");
        assert!(
            status_at < note_at,
            "note renders inside/after status block"
        );
    }

    #[test]
    fn render_respects_each_block_cap() {
        let mut state = TwmState::new(options(), &"s".repeat(TASK_BRIEF_CAP * 2), None);
        assert!(state.task_brief.chars().count() <= TASK_BRIEF_CAP + 20);
        state.note.set(&"n".repeat(NOTE_CAP * 2));
        assert!(state.note.text.chars().count() <= NOTE_CAP + 20);
    }

    // ---- observe: the dispatcher ----

    #[test]
    fn observe_read_file_populates_working_set() {
        let mut state = TwmState::new(options(), "task", None);
        let messages = vec![
            tool_call("c1", "read_file", serde_json::json!({"path": "a.rs"})),
            tool_result("c1", "1: fn main() {}", ToolResultStatus::Success),
        ];
        state.observe(&messages, 1, 10);
        assert_eq!(state.working_set.entries.len(), 1);
        assert_eq!(state.working_set.entries[0].path, "a.rs");
    }

    #[test]
    fn observe_edit_file_records_a_diff_and_invalidates_the_read_range() {
        let mut state = TwmState::new(options(), "task", None);
        let messages = vec![
            tool_call("c1", "read_file", serde_json::json!({"path": "a.rs"})),
            tool_result("c1", "1: old", ToolResultStatus::Success),
            tool_call(
                "c2",
                "edit_file",
                serde_json::json!({"path": "a.rs", "old_string": "old", "new_string": "new"}),
            ),
            tool_result(
                "c2",
                "Edited a.rs (10 bytes after edit).",
                ToolResultStatus::Success,
            ),
        ];
        state.observe(&messages, 2, 10);
        assert!(state.working_set.entries.is_empty(), "stale range dropped");
        assert_eq!(state.edits.hunks.len(), 1);
        assert!(state.edits.render().contains("-old"));
    }

    #[test]
    fn observe_note_only_applies_when_the_switch_is_on() {
        let mut off = TwmState::new(TwmOptions::default(), "task", None);
        let mut on = TwmState::new(options(), "task", None);
        let messages = vec![
            tool_call("c1", NOTE_TOOL, serde_json::json!({"text": "remember X"})),
            tool_result("c1", "noted", ToolResultStatus::Success),
        ];
        off.observe(&messages, 1, 10);
        on.observe(&messages, 1, 10);
        assert!(off.note.text.is_empty());
        assert_eq!(on.note.text, "remember X");
    }

    #[test]
    fn observe_autotest_marker_feeds_test_status_only_when_enabled() {
        let mut state = TwmState::new(options(), "task", None);
        let messages = vec![
            tool_call(
                "c1",
                "edit_file",
                serde_json::json!({"path": "a.rs", "old_string": "a", "new_string": "b"}),
            ),
            tool_result(
                "c1",
                "Edited a.rs.\nAutotest: 1 passed, 0 failed",
                ToolResultStatus::Success,
            ),
        ];
        state.observe(&messages, 1, 10);
        assert!(state.test_status.render().contains("1 passed"));
    }

    #[test]
    fn observe_error_then_clear_on_a_later_success() {
        // `observe` only ever scans forward from `observed_len`, exactly like
        // the live turn loop's own growing `messages` vector — so a second
        // call must extend the same vector, never hand it a disjoint one.
        let mut state = TwmState::new(options(), "task", None);
        let mut messages = vec![
            tool_call(
                "c1",
                "run_command",
                serde_json::json!({"command": "pytest"}),
            ),
            tool_result("c1", "exit 1", ToolResultStatus::Error),
        ];
        state.observe(&messages, 1, 10);
        assert!(state.errors.render().contains("run_command"));
        messages.push(tool_call(
            "c2",
            "run_command",
            serde_json::json!({"command": "pytest"}),
        ));
        messages.push(tool_result("c2", "exit 0", ToolResultStatus::Success));
        state.observe(&messages, 2, 10);
        assert!(state.errors.render().is_empty());
    }

    #[test]
    fn observe_is_idempotent_past_the_observed_boundary() {
        let mut state = TwmState::new(options(), "task", None);
        let messages = vec![
            tool_call("c1", "read_file", serde_json::json!({"path": "a.rs"})),
            tool_result("c1", "content", ToolResultStatus::Success),
        ];
        state.observe(&messages, 1, 10);
        state.observe(&messages, 1, 10);
        assert_eq!(
            state.working_set.entries.len(),
            1,
            "re-observing the same messages must not duplicate state"
        );
    }

    // ---- the mixed approach: append between rebuilds, rebuild on trigger ----

    fn request(messages: &[Message]) -> LlmRequest {
        LlmRequest {
            messages: messages.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn whole_prompt_includes_system_tools_and_unicode_characters() {
        let state = TwmState::new(TwmOptions::default(), "task", None);
        let mut prompt = request(&[Message::user("界".repeat(80_000))]);
        assert!(
            !state.should_rebuild(&prompt),
            "characters, not UTF-8 bytes"
        );
        prompt.system = Some("s".repeat(80_000));
        assert!(state.should_rebuild(&prompt));
        prompt.system = None;
        prompt.tools = Some(vec![ToolDefinition::new(
            "tool",
            "d".repeat(80_000),
            serde_json::json!({}),
        )]);
        assert!(state.should_rebuild(&prompt));
    }

    /// One realistic model turn: a tool call and its matching result.
    fn scripted_turn(id: &str, tool_output_len: usize) -> Vec<Message> {
        vec![
            tool_call(id, "read_file", serde_json::json!({"path": "a.rs"})),
            tool_result(id, &"x".repeat(tool_output_len), ToolResultStatus::Success),
        ]
    }

    #[test]
    fn rebuild_shrinks_history_and_keeps_only_the_last_assistant_turns() {
        let mut state = TwmState::new(options(), "task", None);
        state.working_set.record("a.rs", None, None, "content", 1);
        let mut messages = vec![Message::user("root instruction")];
        for turn in 0..5u16 {
            messages.extend(scripted_turn(&format!("c{turn}"), 50));
        }
        let before_len = messages.len();
        let rebuilt = state.rebuild(&messages);
        assert!(
            rebuilt.len() < before_len,
            "history must drop at a rebuild: {before_len} -> {}",
            rebuilt.len()
        );
        // REBUILD_KEEP_TURNS (2) assistant turns, 2 messages each, plus the
        // fresh rendered context message.
        assert_eq!(rebuilt.len(), 1 + REBUILD_KEEP_TURNS * 2);
        assert!(
            matches!(rebuilt[1], Message::Assistant { .. }),
            "the kept tail must start at an assistant message, not an orphaned tool result"
        );
        let Message::User { content } = &rebuilt[0] else {
            panic!("rebuilt context message must be a user message");
        };
        let UserBlock::Text(text) = &content[0] else {
            panic!("expected a text block");
        };
        assert!(text.text.contains("Working set"));
    }

    #[test]
    fn tool_call_and_result_pairs_stay_intact_after_a_rebuild() {
        let mut state = TwmState::new(options(), "task", None);
        let mut messages = vec![Message::user("root instruction")];
        for turn in 0..4u16 {
            messages.extend(scripted_turn(&format!("c{turn}"), 20));
        }
        let rebuilt = state.rebuild(&messages);
        let mut checked_a_call = false;
        for (index, message) in rebuilt.iter().enumerate() {
            let Message::Assistant { content, .. } = message else {
                continue;
            };
            for block in content {
                let AssistantBlock::ToolCall(call) = block else {
                    continue;
                };
                checked_a_call = true;
                let answered = rebuilt.get(index + 1).is_some_and(|next| {
                    matches!(next, Message::User { content }
                    if content.iter().any(|block| matches!(
                        block,
                        UserBlock::ToolResult(result) if result.id == call.id
                    )))
                });
                assert!(
                    answered,
                    "tool call {} has no matching result immediately after it post-rebuild",
                    call.id
                );
            }
        }
        assert!(
            checked_a_call,
            "test setup must include at least one tool call"
        );
    }

    #[test]
    fn thirty_short_turns_never_rebuild() {
        let mut state = TwmState::new(TwmOptions::default(), "task", None);
        let mut messages = vec![Message::user("start")];
        for turn in 0..30 {
            messages.extend(scripted_turn(&format!("c{turn}"), 100));
            let before = serde_json::to_string(&messages).unwrap();
            state.observe(&messages, turn, 100);
            assert!(!state.should_rebuild(&request(&messages)));
            assert_eq!(serde_json::to_string(&messages).unwrap(), before);
        }
    }

    #[test]
    fn long_task_rebuilds_at_whole_prompt_limit_then_waits_for_size_again() {
        let mut state = TwmState::new(TwmOptions::default(), "task", None);
        let mut messages = vec![Message::user("root instruction")];
        let mut fired_on = Vec::new();
        for turn in 0..40 {
            messages.extend(scripted_turn(&format!("c{turn}"), 10_000));
            state.observe(&messages, turn, 100);
            let prompt = request(&messages);
            let chars = serde_json::to_string(&prompt).unwrap().chars().count();
            assert_eq!(state.should_rebuild(&prompt), chars > 160_000);
            if state.should_rebuild(&prompt) {
                messages = state.rebuild(&messages);
                assert!(!state.should_rebuild(&request(&messages)));
                fired_on.push(turn);
            }
        }
        assert!(fired_on.len() >= 2, "must cross the full size limit again");
        for pair in fired_on.windows(2) {
            assert!(
                pair[1] - pair[0] > 1,
                "no consecutive rebuilds: {fired_on:?}"
            );
        }
    }

    // ---- codemap ----

    #[test]
    fn build_codemap_lists_top_level_definitions_deterministically() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("a.rs"), "pub fn one() {}\nfn two() {}\n")
            .expect("seed file");
        let first = build_codemap(dir.path());
        let second = build_codemap(dir.path());
        assert_eq!(first, second, "same tree, same map");
        assert!(first.contains("a.rs"));
        assert!(first.contains("fn one"));
        assert!(first.contains("fn two"));
    }

    #[test]
    fn build_codemap_ignores_non_source_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("notes.txt"), "fn looks_like_code() {}\n")
            .expect("seed file");
        let map = build_codemap(dir.path());
        assert!(map.is_empty());
    }
}
