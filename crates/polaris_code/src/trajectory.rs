//! Durable ATIF export for an explicitly recorded unattended run.
//!
//! [`RunTrajectory`] owns one fresh destination and atomically checkpoints it
//! after requests, content blocks, usage updates and tool results. Model calls
//! carry the complete native request in `step.extra.request`; compaction never
//! erases earlier steps. No credentials, HTTP headers or provider configuration
//! are recorded. Tool results are the same redacted blocks the agent receives.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, LlmRequest, LlmResponse, Message,
    StreamEvent, ToolResultContent, Usage, UserBlock,
};
use polaris::system::resource::GlobalResource;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// A process-scoped handle to one unattended run's ATIF destination.
///
/// Provided optionally by the host before provider registration. The model-run
/// plugin records every provider request, while the coding context records tool
/// results. Clones share a mutex and atomic file replacement. An embedder can
/// omit it without changing the agent's behavior. The destination contains task
/// data and is created with owner-only permissions on Unix.
///
/// # Scope
///
/// Global: one explicit unattended run owns the process and destination.
///
/// # Provided By
///
/// The binary creates the recorder from `--trajectory-path` and inserts a clone
/// before server startup. Embedders may create and insert their own instance.
///
/// # Access
///
/// `Res<RunTrajectory>` or `Option<Res<RunTrajectory>>`. Methods use a shared
/// mutex and atomically replace the JSON file. Persistence errors are retained
/// and returned by `check` and `finish`, even if a later checkpoint succeeds.
///
/// # Alternatives
///
/// The agent's RDF trajectory records policy decisions; it does not replace a
/// complete provider-call export. `RunUsage` reports accounting without content.
///
/// # Example
///
/// ```no_run
/// use polaris::system::{param::Res, system};
/// use polaris_code::trajectory::RunTrajectory;
/// #[system]
/// async fn verify_recording(trace: Option<Res<RunTrajectory>>) {
///     if let Some(trace) = trace { trace.check().expect("trajectory is writable"); }
/// }
/// ```
#[derive(Clone)]
pub struct RunTrajectory(Arc<Mutex<Recording>>);

impl std::fmt::Debug for RunTrajectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunTrajectory").finish_non_exhaustive()
    }
}

impl GlobalResource for RunTrajectory {}

struct Recording {
    path: PathBuf,
    document: Value,
    failure: Option<String>,
}

impl Recording {
    fn save(&mut self) -> io::Result<()> {
        let steps = self.document["steps"].as_array().expect("steps array");
        let sum = |key: &str| {
            steps
                .iter()
                .filter_map(|step| step["metrics"][key].as_u64())
                .sum::<u64>()
        };
        self.document["final_metrics"] = json!({
            "total_prompt_tokens": sum("prompt_tokens"),
            "total_completion_tokens": sum("completion_tokens"),
            "total_cached_tokens": sum("cached_tokens"),
            "total_steps": steps.len(),
        });
        let parent = self.path.parent().expect("absolute destination has parent");
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(temporary.as_file_mut(), &self.document)?;
        temporary.as_file_mut().write_all(b"\n")?;
        temporary.as_file_mut().sync_all()?;
        temporary.persist(&self.path).map_err(|err| err.error)?;
        Ok(())
    }
}

impl RunTrajectory {
    /// Reserve a new file and write the initial user instruction as ATIF-v1.7.
    /// Existing destinations (including symlinks) are refused. Parent directories
    /// must exist, so an invalid destination fails before the first model call.
    pub fn create(
        path: &Path,
        session_id: &str,
        model: &str,
        instruction: &str,
    ) -> io::Result<Self> {
        let path = std::path::absolute(path)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let _reservation = options.open(&path)?;
        let mut recording = Recording {
            path,
            document: json!({
                "schema_version": "ATIF-v1.7",
                "session_id": session_id,
                "agent": {"name": "polaris", "version": env!("CARGO_PKG_VERSION"), "model_name": model},
                "steps": [{"step_id": 1, "timestamp": timestamp(), "source": "user", "message": instruction}],
                "extra": {"status": "running"},
            }),
            failure: None,
        };
        recording.save()?;
        Ok(Self(Arc::new(Mutex::new(recording))))
    }

    fn update(&self, change: impl FnOnce(&mut Value)) {
        self.update_if(|document| {
            change(document);
            true
        });
    }

    fn update_if(&self, change: impl FnOnce(&mut Value) -> bool) {
        let mut recording = self.0.lock().unwrap_or_else(|err| err.into_inner());
        if !change(&mut recording.document) {
            return;
        }
        if let Err(error) = recording.save() {
            // Keep the error outside the artifact, because a failing destination
            // cannot reliably record its own failure. The host checks it at exit.
            tracing::error!(%error, "could not checkpoint trajectory");
            recording.failure.get_or_insert_with(|| error.to_string());
        }
    }

    /// Report the first persistence failure, if any. A caller requesting an
    /// export must not receive a successful run with silently missing evidence.
    pub fn check(&self) -> io::Result<()> {
        let recording = self.0.lock().unwrap_or_else(|err| err.into_inner());
        match &recording.failure {
            Some(error) => Err(io::Error::other(error.clone())),
            None => Ok(()),
        }
    }

    /// Finalize run status after tool cleanup and session persistence.
    pub fn finish(&self, exit_code: i32) -> io::Result<()> {
        self.update(|document| {
            document["extra"]["status"] = json!(if exit_code == 0 {
                "completed"
            } else {
                "failed"
            });
            document["extra"]["exit_code"] = json!(exit_code);
        });
        self.check()
    }

    pub(crate) fn begin(&self, model: &str, request: &LlmRequest) -> usize {
        for message in &request.messages {
            self.message(message);
        }
        let mut index = 0;
        self.update(|document| {
            let steps = document["steps"].as_array_mut().expect("steps array");
            index = steps.len();
            let mut step = json!({
                "step_id": index + 1, "timestamp": timestamp(), "source": "agent", "model_name": model,
                "message": "", "extra": {"request": request, "status": "in_flight"},
            });
            // Read from the serialized request so the ATIF representation uses
            // exactly the framework's stable wire spelling.
            if let Some(effort) = step["extra"]["request"]
                .get("reasoning_effort")
                .filter(|v| !v.is_null())
                .cloned()
            {
                step["reasoning_effort"] = effort;
            }
            steps.push(step);
        });
        index
    }

    pub(crate) fn response(&self, index: usize, response: &LlmResponse) {
        self.update(|document| {
            let step = &mut document["steps"][index];
            step["message"] = json!(response.text());
            let mut reasoning = String::new();
            let mut calls = Vec::new();
            for block in &response.content {
                match block {
                    AssistantBlock::Reasoning(block) => {
                        reasoning.push_str(&block.reasoning.join(""))
                    }
                    AssistantBlock::ToolCall(call) => calls.push(tool_call(
                        &call.id,
                        &call.function.name,
                        call.function.arguments.clone(),
                    )),
                    _ => {}
                }
            }
            if !reasoning.is_empty() {
                step["reasoning_content"] = json!(reasoning);
            }
            if !calls.is_empty() {
                step["tool_calls"] = json!(calls);
            }
            step["metrics"] = metrics(&response.usage);
            step["extra"]["response"] = json!(response);
            step["extra"]["status"] = json!("completed");
        });
    }

    pub(crate) fn failure(&self, index: usize, kind: &str) {
        self.update(|document| {
            document["steps"][index]["extra"]["status"] = json!("failed");
            document["steps"][index]["extra"]["error_type"] = json!(kind);
        });
    }

    /// Record tool results before a following model call can project or compact
    /// them. Duplicate observations from batch completion are ignored.
    pub(crate) fn tool_results(&self, blocks: &[UserBlock]) {
        self.update_if(|document| {
            let mut changed = false;
            for block in blocks {
                let UserBlock::ToolResult(result) = block else { continue };
                let steps = document["steps"].as_array_mut().expect("steps array");
                let step = steps.iter_mut().rev().find(|step| {
                    step["tool_calls"].as_array().is_some_and(|calls| calls.iter().any(|call| call["tool_call_id"] == result.id))
                });
                let Some(step) = step else { continue };
                if step.get("observation").is_none() { step["observation"] = json!({"results": []}); }
                let observations = step["observation"]["results"].as_array_mut().expect("results array");
                if observations.iter().any(|item| item["source_call_id"] == result.id) { continue; }
                let text = match &result.content {
                    ToolResultContent::Text(text) => text.clone(),
                    ToolResultContent::Image(image) => serde_json::to_string(image).expect("image serialization"),
                };
                changed = true;
                observations.push(json!({"source_call_id": result.id, "content": text, "extra": {"native_result": result}}));
            }
            changed
        });
    }

    pub(crate) fn message(&self, message: &Message) {
        if let Message::User { content } = message
            && content
                .iter()
                .any(|block| matches!(block, UserBlock::ToolResult(_)))
        {
            self.tool_results(content);
        }
    }
}

// ATIF requires an object even when the model supplied invalid scalar/array
// arguments. Retain that call and its exact native arguments for auditing.
fn tool_call(id: &str, name: &str, arguments: Value) -> Value {
    let object = if arguments.is_object() {
        arguments.clone()
    } else {
        json!({"_native_arguments": arguments})
    };
    json!({"tool_call_id": id, "function_name": name, "arguments": object, "extra": {"native_arguments": arguments}})
}

fn timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC timestamp is representable as RFC 3339")
}

fn metrics(usage: &Usage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens.map(|input| input.saturating_add(usage.cache_read_tokens.unwrap_or(0)).saturating_add(usage.cache_creation_tokens.unwrap_or(0))),
        "completion_tokens": usage.output_tokens,
        "cached_tokens": usage.cache_read_tokens,
        "extra": {"native_usage": usage},
    })
}

/// In-memory accumulator for one live stream. Captures native events so partial
/// streams, reasoning signatures and provider identifiers remain recoverable.
pub(crate) struct StreamCapture {
    trajectory: RunTrajectory,
    index: usize,
    events: Vec<StreamEvent>,
    blocks: BTreeMap<u32, (ContentBlockStartData, String)>,
    completed: bool,
}

impl StreamCapture {
    pub(crate) fn new(trajectory: RunTrajectory, index: usize) -> Self {
        Self {
            trajectory,
            index,
            events: Vec::new(),
            blocks: BTreeMap::new(),
            completed: false,
        }
    }

    pub(crate) fn event(&mut self, event: &StreamEvent) {
        self.events.push(event.clone());
        match event {
            StreamEvent::ContentBlockStart { index, block } => {
                self.blocks.insert(*index, (block.clone(), String::new()));
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some((_, text)) = self.blocks.get_mut(index) {
                    match delta {
                        ContentBlockDelta::Text(value)
                        | ContentBlockDelta::Reasoning(value)
                        | ContentBlockDelta::ToolCall { arguments: value } => text.push_str(value),
                        ContentBlockDelta::Signature(_) => {}
                    }
                }
            }
            StreamEvent::MessageStop { .. } => self.completed = true,
            _ => {}
        }
        // A delta remains in memory until the next content-block boundary or
        // drop. This avoids rewriting a growing trajectory for every token.
        if matches!(
            event,
            StreamEvent::ContentBlockStop { .. }
                | StreamEvent::MessageDelta { .. }
                | StreamEvent::MessageStop { .. }
        ) {
            self.checkpoint();
        }
    }

    fn checkpoint(&self) {
        self.trajectory.update(|document| {
            let step = &mut document["steps"][self.index];
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls = Vec::new();
            for (kind, content) in self.blocks.values() {
                match kind {
                    ContentBlockStartData::Text => text.push_str(content),
                    ContentBlockStartData::Reasoning => reasoning.push_str(content),
                    ContentBlockStartData::ToolCall { id, name, .. } => {
                        if let Ok(arguments) =
                            serde_json::from_str::<Value>(if content.is_empty() {
                                "{}"
                            } else {
                                content
                            })
                        {
                            calls.push(tool_call(id, name, arguments));
                        }
                    }
                }
            }
            step["message"] = json!(text);
            if !reasoning.is_empty() {
                step["reasoning_content"] = json!(reasoning);
            }
            if !calls.is_empty() {
                step["tool_calls"] = json!(calls);
            }
            for event in self.events.iter().rev() {
                if let StreamEvent::MessageStop { usage, .. }
                | StreamEvent::MessageDelta { usage } = event
                {
                    step["metrics"] = metrics(usage);
                    break;
                }
            }
            step["extra"]["stream_events"] = json!(self.events);
            step["extra"]["status"] = json!(if self.completed {
                "completed"
            } else {
                "partial"
            });
        });
    }
}

impl Drop for StreamCapture {
    fn drop(&mut self) {
        self.checkpoint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::llm::{StopReason, ToolCall};

    fn document(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn native_requests_responses_results_and_usage_survive_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace =
            RunTrajectory::create(&path, "run-1", "openai-chat/vendor/model:route", "Fix it")
                .unwrap();
        let request = LlmRequest {
            system: Some("System prompt".into()),
            messages: vec![Message::user("Fix it")],
            ..Default::default()
        };
        let index = trace.begin("openai-chat/vendor/model:route", &request);
        let call = ToolCall::new("call-1", "run_command", json!({"command": "echo done"}));
        let response = LlmResponse {
            content: vec![
                AssistantBlock::Reasoning(polaris::models::llm::ReasoningBlock {
                    id: Some("reason-1".into()),
                    reasoning: vec!["Check it".into()],
                    signature: Some("signature".into()),
                }),
                AssistantBlock::ToolCall(call.clone()),
            ],
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                cache_read_tokens: Some(3),
                cache_creation_tokens: Some(2),
                ..Default::default()
            },
            stop_reason: StopReason::ToolUse,
            id: Some("response-1".into()),
            model: Some("vendor/model:route".into()),
        };
        trace.response(index, &response);
        let block = call.result(ToolResultContent::Text("done".into()));
        trace.tool_results(std::slice::from_ref(&block));
        trace.tool_results(&[block]);
        trace.finish(0).unwrap();
        let value = document(&path);
        assert_eq!(value["schema_version"], "ATIF-v1.7");
        assert_eq!(value["steps"][0]["message"], "Fix it");
        let timestamps = value["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| {
                OffsetDateTime::parse(step["timestamp"].as_str().unwrap(), &Rfc3339).unwrap()
            })
            .collect::<Vec<_>>();
        assert!(timestamps.windows(2).all(|pair| pair[0] <= pair[1]));
        let step = &value["steps"][1];
        assert_eq!(step["extra"]["request"], json!(request));
        assert_eq!(step["extra"]["response"], json!(response));
        assert_eq!(step["reasoning_content"], "Check it");
        assert_eq!(step["tool_calls"][0]["tool_call_id"], "call-1");
        assert_eq!(step["observation"]["results"].as_array().unwrap().len(), 1);
        assert_eq!(step["observation"]["results"][0]["content"], "done");
        assert_eq!(value["final_metrics"]["total_prompt_tokens"], 15);
        assert_eq!(value["final_metrics"]["total_completion_tokens"], 4);
        assert_eq!(value["extra"]["status"], "completed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn dropping_a_partial_stream_preserves_reasoning_tools_and_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let index = trace.begin("fake/model", &LlmRequest::default());
        let mut capture = StreamCapture::new(trace, index);
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning,
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("Think".into()),
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Signature("sig".into()),
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlockStartData::ToolCall {
                    id: "tool".into(),
                    call_id: Some("wire-id".into()),
                    name: "read_file".into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "{\"path\":\"a.rs\"}".into(),
                },
            },
        ];
        for event in &events {
            capture.event(event);
        }
        drop(capture);
        let value = document(&path);
        let step = &value["steps"][1];
        assert_eq!(step["reasoning_content"], "Think");
        assert_eq!(step["tool_calls"][0]["arguments"]["path"], "a.rs");
        assert_eq!(step["extra"]["stream_events"], json!(events));
        assert_eq!(step["extra"]["status"], "partial");
    }

    #[test]
    fn malformed_tool_arguments_stay_visible_without_invalidating_atif() {
        let call = tool_call("call", "read_file", json!([1, 2]));
        assert!(call["arguments"].is_object());
        assert_eq!(call["arguments"]["_native_arguments"], json!([1, 2]));
        assert_eq!(call["extra"]["native_arguments"], json!([1, 2]));
    }

    #[test]
    fn export_refuses_existing_destinations_and_remembers_write_failures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        assert!(RunTrajectory::create(&path, "other", "fake/model", "task").is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(trace.finish(0).is_err());
        assert!(trace.check().is_err());
    }
}
