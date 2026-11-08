//! Durable ATIF export with shared, reconstructable model-call histories.
//!
//! Messages are recorded once. Each model call references a persistent history,
//! system prompt, tool definitions and request parameters. New summaries and
//! elided observations are new context steps; earlier evidence remains intact.
//!
//! The [embedding guide](crate::docs::embedding) shows how a host wires the
//! recorder in. `docs/unattended-runs.md` in the repository describes the run
//! controls and the file format.

mod history;
mod messages;

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, LlmRequest, LlmResponse, Message,
    ReasoningBlock, StopReason, StreamEvent, TextBlock, ToolCall, Usage, UserBlock,
};
use polaris::system::resource::GlobalResource;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// A process-scoped handle to one unattended run's ATIF destination.
///
/// Provided optionally by the host before provider registration. The
/// [`ModelRunPlugin`](crate::model_run::ModelRunPlugin) records every provider
/// request, while the coding context records tool results. Clones share a mutex
/// and atomic file replacement. An embedder can omit it without changing the
/// agent's behavior. The destination contains task data and is created with
/// owner-only permissions on Unix.
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
/// and returned by [`check`](Self::check) and [`finish`](Self::finish), even if
/// a later checkpoint succeeds. The model-run plugin writes its checkpoints on
/// Tokio's blocking pool, so a provider call never blocks an async worker with
/// file I/O. A dropped response stream only records its partial content in
/// memory; the next checkpoint or [`finish`](Self::finish) writes it.
///
/// # Alternatives
///
/// The agent's RDF trajectory records policy decisions; it does not replace a
/// complete provider-call export. [`RunUsage`](crate::usage::RunUsage) reports
/// accounting without content.
///
/// # Example
///
/// ```no_run
/// use polaris::system::system::SystemError;
/// use polaris::system::{param::Res, system};
/// use polaris_code::trajectory::RunTrajectory;
///
/// #[system]
/// async fn verify_recording(trace: Option<Res<RunTrajectory>>) -> Result<(), SystemError> {
///     if let Some(trace) = trace {
///         trace
///             .check()
///             .map_err(|error| SystemError::ExecutionError(error.to_string()))?;
///     }
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct RunTrajectory(Arc<Shared>);

struct Shared {
    recording: Mutex<Recording>,
    /// The first persistence failure. It lives outside the recording lock so
    /// `check` never waits for a file write that is in progress.
    failure: OnceLock<String>,
}

impl std::fmt::Debug for RunTrajectory {
    // Shows counts only. Task content never appears, and a busy lock is not waited for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("RunTrajectory");
        match self.0.recording.try_lock() {
            Ok(recording) => debug.field("steps", &recording.step_count()),
            Err(TryLockError::Poisoned(poisoned)) => {
                debug.field("steps", &poisoned.into_inner().step_count())
            }
            Err(TryLockError::WouldBlock) => debug.field("steps", &format_args!("<busy>")),
        };
        debug
            .field("failed", &self.0.failure.get().is_some())
            .finish_non_exhaustive()
    }
}

impl GlobalResource for RunTrajectory {}

struct Recording {
    path: PathBuf,
    document: Value,
    /// Set when a change is in memory but not yet in the file.
    unsaved: bool,
    messages: HashMap<u64, Vec<usize>>,
    configurations: HashMap<(String, u64), Vec<usize>>,
    history: history::History,
}

fn fingerprint(value: &Value) -> u64 {
    let mut hash = std::hash::DefaultHasher::new();
    value.to_string().hash(&mut hash);
    hash.finish()
}

impl Recording {
    fn step_count(&self) -> usize {
        self.document["steps"].as_array().map_or(0, Vec::len)
    }

    fn save(&mut self) -> io::Result<()> {
        let steps = self.document["steps"].as_array().expect("steps array");
        let sum = |key: &str| {
            steps
                .iter()
                .filter_map(|step| step["metrics"][key].as_u64())
                .fold(0u64, u64::saturating_add)
        };
        self.document["final_metrics"] = json!({
            "total_prompt_tokens": sum("prompt_tokens"), "total_completion_tokens": sum("completion_tokens"),
            "total_cached_tokens": sum("cached_tokens"), "total_steps": steps.len(),
        });
        let parent = self.path.parent().expect("absolute destination has parent");
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(temporary.as_file_mut(), &self.document)?;
        temporary.as_file_mut().write_all(b"\n")?;
        temporary.as_file_mut().sync_all()?;
        temporary.persist(&self.path).map_err(|err| err.error)?;
        Ok(())
    }

    fn configuration(&mut self, kind: &str, value: &Value) -> Option<usize> {
        if value.is_null() {
            return None;
        }
        let key = (kind.to_string(), fingerprint(value));
        if let Some(candidates) = self.configurations.get(&key) {
            for id in candidates {
                let step = &self.document["steps"][id - 1];
                let stored = if kind == "system_prompt" {
                    &step["message"]
                } else {
                    &step["extra"]["value"]
                };
                if stored == value {
                    return Some(*id);
                }
            }
        }
        let id = messages::step(&mut self.document, "system", kind);
        let step = &mut self.document["steps"][id - 1];
        if kind == "system_prompt" {
            step["message"] = value.clone();
        } else {
            step["extra"]["value"] = value.clone();
        }
        self.configurations.entry(key).or_default().push(id);
        Some(id)
    }

    fn message(&mut self, message: &Message) -> usize {
        if let Message::User { content } = message {
            messages::tool_results(&mut self.document, content);
        }
        let native = json!(message);
        let hash = fingerprint(&native);
        if let Some(candidates) = self.messages.get(&hash) {
            for id in candidates {
                if messages::load(&self.document, *id).is_ok_and(|stored| json!(stored) == native) {
                    return *id;
                }
            }
        }
        let compacted = matches!(message, Message::User { content } if content.iter().any(|block| match block {
            UserBlock::Text(text) => text.text.starts_with("Conversation summary checkpoint"),
            UserBlock::ToolResult(result) => matches!(&result.content, polaris::models::llm::ToolResultContent::Text(text) if text.starts_with("[elided ")),
            _ => false,
        }));
        let (source, kind) = match message {
            Message::Assistant { .. } => ("agent", "context_message"),
            Message::User { .. } if compacted => ("system", "compacted_context"),
            Message::User { content }
                if content
                    .iter()
                    .any(|block| matches!(block, UserBlock::ToolResult(_))) =>
            {
                ("system", "tool_results")
            }
            _ => ("user", "message"),
        };
        let id = messages::step(&mut self.document, source, kind);
        messages::store(&mut self.document, id, message);
        if source == "agent" {
            self.document["steps"][id - 1]["is_copied_context"] = json!(true);
        }
        self.messages.entry(hash).or_default().push(id);
        id
    }
}

impl RunTrajectory {
    /// Reserve a fresh owner-only file and record the initial instruction.
    /// Existing destinations, including symlinks, are refused. Its parent must exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination already exists (a symlink counts),
    /// if its parent directory is missing, or if the first write fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use polaris_code::trajectory::RunTrajectory;
    ///
    /// let trace = RunTrajectory::create(
    ///     Path::new("out/trajectory.json"),
    ///     "session-1",
    ///     "provider/model",
    ///     "Fix the failing test.",
    /// )?;
    /// // Run the agent with a clone of `trace` published as a resource, then:
    /// trace.finish(0)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
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
                "schema_version": "ATIF-v1.7", "session_id": session_id,
                "agent": {"name": "polaris", "version": env!("CARGO_PKG_VERSION"), "model_name": model},
                "steps": [], "extra": {"status": "running", "history_format": "polaris-code-message-history/v1", "histories": []},
            }),
            unsaved: false,
            messages: HashMap::new(),
            configurations: HashMap::new(),
            history: history::History::default(),
        };
        recording.message(&Message::user(instruction));
        recording.save()?;
        Ok(Self(Arc::new(Shared {
            recording: Mutex::new(recording),
            failure: OnceLock::new(),
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Recording> {
        self.0
            .recording
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    /// Apply `change` in memory, then write the file. A change that returns
    /// `true` leaves the recording unsaved until a write succeeds, so a later
    /// checkpoint also writes anything that was deferred.
    fn update(&self, change: impl FnOnce(&mut Recording) -> bool) {
        self.apply(false, change);
    }

    /// Like [`update`](Self::update), but with `defer` set the file is not
    /// touched. A deferred change is written by the next checkpoint.
    fn apply(&self, defer: bool, change: impl FnOnce(&mut Recording) -> bool) {
        let mut recording = self.lock();
        let changed = change(&mut recording);
        recording.unsaved |= changed;
        if defer || !recording.unsaved {
            return;
        }
        match recording.save() {
            Ok(()) => recording.unsaved = false,
            Err(error) => {
                tracing::error!(%error, "could not checkpoint trajectory");
                let _ = self.0.failure.set(error.to_string());
            }
        }
    }

    /// Run `work` on Tokio's blocking pool, so file I/O never stalls an async
    /// worker. Needs a Tokio runtime. A panic in `work` is raised again here.
    pub(crate) async fn blocking<R: Send + 'static>(
        &self,
        work: impl FnOnce(&Self) -> R + Send + 'static,
    ) -> R {
        let trace = self.clone();
        match tokio::task::spawn_blocking(move || work(&trace)).await {
            Ok(result) => result,
            Err(error) => match error.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                Err(error) => panic!("trajectory write task stopped: {error}"),
            },
        }
    }

    /// Return any retained persistence failure, even if a later checkpoint succeeded.
    ///
    /// This never waits for a file write in progress.
    ///
    /// # Errors
    ///
    /// Returns the first failure to write the destination file.
    pub fn check(&self) -> io::Result<()> {
        match self.0.failure.get() {
            Some(error) => Err(io::Error::other(error.clone())),
            None => Ok(()),
        }
    }

    /// Finalize the run after tool cleanup and session persistence. Also writes
    /// any change a dropped stream left in memory.
    ///
    /// # Errors
    ///
    /// Returns the first failure to write the destination file, including one
    /// from an earlier checkpoint, or the failure of this final write.
    pub fn finish(&self, exit_code: i32) -> io::Result<()> {
        self.update(|recording| {
            recording.document["extra"]["status"] = json!(if exit_code == 0 {
                "completed"
            } else {
                "failed"
            });
            recording.document["extra"]["exit_code"] = json!(exit_code);
            true
        });
        self.check()
    }

    pub(crate) fn begin(&self, model: &str, request: &LlmRequest) -> usize {
        let mut index = 0;
        self.update(|recording| {
            let mut parameters = json!(request);
            for key in ["messages", "system", "tools", "reasoning_effort"] { parameters.as_object_mut().unwrap().remove(key); }
            let system = recording.configuration("system_prompt", &json!(request.system));
            let tools = request.tools.as_ref().map(|definitions| {
                let references = definitions.iter().map(|definition| {
                    recording.configuration("tool_definition", &json!(definition)).expect("tool definition")
                }).collect::<Vec<_>>();
                recording.configuration("tool_definitions", &json!(references)).expect("tool palette")
            });
            let references = parameters.as_object().unwrap().iter().map(|(key, value)| {
                let reference = recording.configuration(&format!("request_parameter/{key}"), value);
                (key.clone(), json!(reference))
            }).collect::<serde_json::Map<_, _>>();
            let parameters = recording.configuration("request_parameters", &json!(references)).expect("parameters object");
            let messages = request.messages.iter().map(|message| recording.message(message)).collect();
            let history = recording.history.record(&mut recording.document, messages);
            let id = messages::step(&mut recording.document, "agent", "model_call");
            index = id - 1;
            let step = &mut recording.document["steps"][index];
            step["model_name"] = json!(model);
            step["llm_call_count"] = json!(1);
            step["extra"]["input"] = json!({"history": history, "system": system, "tools": tools, "parameters": parameters});
            step["extra"]["status"] = json!("in_flight");
            if let Some(effort) = &request.reasoning_effort { step["reasoning_effort"] = json!(effort); }
            true
        });
        index
    }

    /// Store one response snapshot. With `defer` set, only memory changes.
    pub(crate) fn assistant(&self, index: usize, snapshot: Snapshot, defer: bool) {
        self.apply(defer, |recording| {
            let Snapshot {
                message,
                usage,
                metadata,
                completed,
            } = snapshot;
            messages::store(&mut recording.document, index + 1, &message);
            let step = &mut recording.document["steps"][index];
            step["metrics"] = metrics(&usage);
            for (key, value) in metadata.as_object().expect("response metadata") {
                step["extra"][key] = value.clone();
            }
            // A failed step stays failed. Its partial content is still kept,
            // but a later snapshot must not replace the failure status.
            if step["extra"]["status"] != "failed" {
                step["extra"]["status"] = json!(if completed { "completed" } else { "partial" });
            }
            recording
                .messages
                .entry(fingerprint(&json!(message)))
                .or_default()
                .push(index + 1);
            true
        });
    }

    pub(crate) fn response(&self, index: usize, response: &LlmResponse) {
        // The snapshot owns a copy: the caller keeps its response, and the
        // write may run on another thread.
        let snapshot = Snapshot {
            message: Message::Assistant {
                id: None,
                content: response.content.clone(),
            },
            usage: response.usage.clone(),
            metadata: json!({"stop_reason": response.stop_reason, "response_id": response.id, "response_model": response.model}),
            completed: true,
        };
        self.assistant(index, snapshot, false);
    }

    pub(crate) fn failure(&self, index: usize, kind: &str) {
        self.update(|recording| {
            recording.document["steps"][index]["extra"]["status"] = json!("failed");
            recording.document["steps"][index]["extra"]["error_type"] = json!(kind);
            true
        });
    }

    // Tool results and messages change memory only. Their callers are sync
    // code on async workers, so a full-document write here would stall them.
    // The next model call or `finish` writes them.
    pub(crate) fn tool_results(&self, blocks: &[UserBlock]) {
        self.apply(true, |recording| {
            messages::tool_results(&mut recording.document, blocks)
        });
    }

    pub(crate) fn message(&self, message: &Message) {
        self.apply(true, |recording| {
            let count = recording.document["steps"].as_array().unwrap().len();
            let observed = if let Message::User { content } = message {
                messages::tool_results(&mut recording.document, content)
            } else {
                false
            };
            recording.message(message);
            observed || count != recording.document["steps"].as_array().unwrap().len()
        });
    }
}

/// Reconstruct the exact native request sent by a model-call step (one-based
/// ATIF step id). Resolves shared history ranges, configuration steps and native
/// message field references, including compacted context and linked tool results.
///
/// # Errors
///
/// Returns an error if `step_id` is zero, out of range or not a model-call
/// step, if a history, configuration or message reference is invalid or points
/// forward, if a native message field is missing, or if the rebuilt value is not
/// a valid request.
///
/// # Examples
///
/// ```no_run
/// use polaris_code::trajectory::rebuild_request;
///
/// let document: serde_json::Value =
///     serde_json::from_slice(&std::fs::read("out/trajectory.json")?)?;
/// // Step ids are one-based, as in ATIF: the first step is 1, not 0.
/// let request = rebuild_request(&document, 3)?;
/// println!("{} messages", request.messages.len());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn rebuild_request(document: &Value, step_id: usize) -> io::Result<LlmRequest> {
    let invalid = || io::Error::other("invalid model-call step");
    let step = document["steps"]
        .get(step_id.checked_sub(1).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    let input = &step["extra"]["input"];
    let config = |key: &str, field: &str| -> io::Result<Value> {
        match input[key].as_u64() {
            Some(id) if id > 0 => document["steps"]
                .get(id as usize - 1)
                .and_then(|step| step.pointer(field))
                .cloned()
                .ok_or_else(invalid),
            None if input[key].is_null() => Ok(Value::Null),
            _ => Err(invalid()),
        }
    };
    let parameters = config("parameters", "/extra/value")?;
    let resolve_value = |reference: &Value| -> io::Result<Value> {
        if reference.is_null() {
            return Ok(Value::Null);
        }
        let id = reference
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(invalid)? as usize;
        document["steps"]
            .get(id - 1)
            .and_then(|step| step.pointer("/extra/value"))
            .cloned()
            .ok_or_else(invalid)
    };
    let mut request = Value::Object(
        parameters
            .as_object()
            .ok_or_else(invalid)?
            .iter()
            .map(|(key, reference)| Ok((key.clone(), resolve_value(reference)?)))
            .collect::<io::Result<serde_json::Map<_, _>>>()?,
    );
    let history = input["history"].as_u64().ok_or_else(invalid)? as usize;
    let messages = history::resolve(document, history)?
        .into_iter()
        .map(|id| messages::load(document, id))
        .collect::<io::Result<Vec<_>>>()?;
    request["messages"] = json!(messages);
    request["system"] = config("system", "/message")?;
    let tools = config("tools", "/extra/value")?;
    request["tools"] = if tools.is_null() {
        Value::Null
    } else {
        Value::Array(
            tools
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(resolve_value)
                .collect::<io::Result<Vec<_>>>()?,
        )
    };
    request["reasoning_effort"] = step.get("reasoning_effort").cloned().unwrap_or(Value::Null);
    serde_json::from_value(request).map_err(io::Error::other)
}

fn tool_call(id: &str, name: &str, arguments: Value) -> Value {
    if arguments.is_object() {
        json!({"tool_call_id": id, "function_name": name, "arguments": arguments})
    } else {
        json!({"tool_call_id": id, "function_name": name, "arguments": {"_native_arguments": arguments}, "extra": {"invalid_arguments": true}})
    }
}

fn timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC timestamp is representable as RFC 3339")
}

fn metrics(usage: &Usage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens.map(|input| input.saturating_add(usage.cache_read_tokens.unwrap_or(0)).saturating_add(usage.cache_creation_tokens.unwrap_or(0))),
        "completion_tokens": usage.output_tokens, "cached_tokens": usage.cache_read_tokens,
        "extra": {"native_usage": usage},
    })
}

/// One assistant response, detached from its source so a blocking thread can
/// store it.
pub(crate) struct Snapshot {
    message: Message,
    usage: Usage,
    metadata: Value,
    completed: bool,
}

struct CapturedBlock {
    kind: ContentBlockStartData,
    text: String,
    signature: Option<String>,
}

/// Accumulates one response; token-level events are not duplicated in the file.
///
/// `event` only updates memory and returns a [`Snapshot`] when a checkpoint is
/// due; the caller writes it off the async worker. Dropping a capture that did
/// not finish records its partial content in memory without any file I/O.
pub(crate) struct StreamCapture {
    trajectory: RunTrajectory,
    index: usize,
    blocks: BTreeMap<u32, CapturedBlock>,
    closed: Vec<u32>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    response_id: Option<String>,
    model: Option<String>,
    completed: bool,
}

impl StreamCapture {
    pub(crate) fn new(trajectory: RunTrajectory, index: usize) -> Self {
        Self {
            trajectory,
            index,
            blocks: BTreeMap::new(),
            closed: Vec::new(),
            usage: Usage::default(),
            stop_reason: None,
            response_id: None,
            model: None,
            completed: false,
        }
    }
    /// Fold one event in. Returns a snapshot when the event ends a block or
    /// carries usage, which is when the file should be updated.
    pub(crate) fn event(&mut self, event: &StreamEvent) -> Option<Snapshot> {
        match event {
            StreamEvent::ContentBlockStart { index, block } => {
                self.blocks.insert(
                    *index,
                    CapturedBlock {
                        kind: block.clone(),
                        text: String::new(),
                        signature: None,
                    },
                );
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(block) = self.blocks.get_mut(index) {
                    match delta {
                        ContentBlockDelta::Text(value)
                        | ContentBlockDelta::Reasoning(value)
                        | ContentBlockDelta::ToolCall { arguments: value } => {
                            block.text.push_str(value)
                        }
                        ContentBlockDelta::Signature(value) => {
                            block.signature = Some(value.clone())
                        }
                    }
                }
            }
            StreamEvent::ContentBlockStop { index } => {
                if !self.closed.contains(index) {
                    self.closed.push(*index);
                }
            }
            StreamEvent::MessageDelta { usage } => self.usage = usage.clone(),
            StreamEvent::MessageStop {
                usage,
                stop_reason,
                id,
                model,
            } => {
                self.usage = usage.clone();
                self.stop_reason = Some(stop_reason.clone());
                self.response_id = id.clone();
                self.model = model.clone();
                self.completed = true;
            }
        }
        matches!(
            event,
            StreamEvent::ContentBlockStop { .. }
                | StreamEvent::MessageDelta { .. }
                | StreamEvent::MessageStop { .. }
        )
        .then(|| self.snapshot())
    }
    fn snapshot(&self) -> Snapshot {
        let mut content = Vec::new();
        let mut partial = Vec::new();
        let indices = self.closed.iter().copied().chain(
            self.blocks
                .keys()
                .filter(|index| !self.closed.contains(index))
                .copied(),
        );
        for index in indices {
            let Some(block) = self.blocks.get(&index) else {
                continue;
            };
            match &block.kind {
                // Text is cloned because the capture keeps growing it, and the
                // snapshot may be written later on another thread.
                ContentBlockStartData::Text => content.push(AssistantBlock::Text(TextBlock { text: block.text.clone() })),
                ContentBlockStartData::Reasoning => content.push(AssistantBlock::Reasoning(ReasoningBlock { id: None, reasoning: vec![block.text.clone()], signature: block.signature.clone() })),
                ContentBlockStartData::ToolCall { id, call_id, name } => {
                    match serde_json::from_str::<Value>(if block.text.is_empty() { "{}" } else { &block.text }) {
                        Ok(arguments) => {
                            let mut call = ToolCall::new(id.clone(), name.clone(), arguments);
                            call.call_id = call_id.clone();
                            content.push(AssistantBlock::ToolCall(call));
                        }
                        Err(_) => partial.push(json!({"index": index, "id": id, "call_id": call_id, "name": name, "arguments": block.text})),
                    }
                }
            }
        }
        Snapshot {
            message: Message::Assistant { id: None, content },
            usage: self.usage.clone(),
            metadata: json!({"stop_reason": self.stop_reason, "response_id": self.response_id, "response_model": self.model, "partial_tool_calls": partial}),
            completed: self.completed,
        }
    }
}
impl Drop for StreamCapture {
    // No file I/O here: a drop can run on an async worker, so the partial
    // response is only recorded in memory. The next checkpoint or `finish`
    // writes it. A step that already failed keeps its failure status.
    fn drop(&mut self) {
        if !self.completed {
            self.trajectory.assistant(self.index, self.snapshot(), true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::llm::{ReasoningEffort, ToolDefinition, ToolResultContent};
    use std::num::NonZeroU32;

    fn document(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
    fn request(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            system: Some("unique-system-prompt".into()),
            messages,
            tools: Some(vec![ToolDefinition::new(
                "read_file",
                "unique-tool-description",
                json!({"type":"object"}),
            )]),
            reasoning_effort: Some(ReasoningEffort::High),
            max_output_tokens: NonZeroU32::new(512),
            output_schema: Some(json!({"type":"object", "description":"unique-output-schema"})),
            ..Default::default()
        }
    }
    /// Feed one event and write any checkpoint it asks for, as the decorator does.
    fn feed(capture: &mut StreamCapture, event: StreamEvent) {
        if let Some(snapshot) = capture.event(&event) {
            capture
                .trajectory
                .clone()
                .assistant(capture.index, snapshot, false);
        }
    }
    fn response(content: Vec<AssistantBlock>) -> LlmResponse {
        LlmResponse {
            content,
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: Some("response-id".into()),
            model: Some("remote/model".into()),
        }
    }

    #[test]
    fn requests_rebuild_exactly_from_messages_configuration_and_observations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let first = request(vec![Message::user("task")]);
        let index = trace.begin("fake/model", &first);
        let mut call = ToolCall::new("call-1", "read_file", json!({"path":"a.rs"}));
        call.call_id = Some("wire-id".into());
        call.signature = Some("tool-signature".into());
        let mut reply = response(vec![
            AssistantBlock::text("text-one"),
            AssistantBlock::Reasoning(ReasoningBlock {
                id: Some("reason-id".into()),
                reasoning: vec!["α-one".into(), "β-two".into()],
                signature: Some("reason-signature".into()),
            }),
            AssistantBlock::ToolCall(call.clone()),
            AssistantBlock::text("text-two"),
        ]);
        reply.usage = Usage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            cache_read_tokens: Some(3),
            cache_creation_tokens: Some(2),
            reasoning_output_tokens: Some(2),
            ..Default::default()
        };
        trace.response(index, &reply);
        let result = Message::User {
            content: vec![call.result(ToolResultContent::Text("unique-result-content".into()))],
        };
        trace.message(&result);
        let second = request(vec![
            Message::user("task"),
            Message::Assistant {
                id: None,
                content: reply.content.clone(),
            },
            result,
        ]);
        let next = trace.begin("fake/model", &second);
        trace.response(next, &response(vec![AssistantBlock::text("done")]));
        trace.finish(0).unwrap();
        let doc = document(&path);
        assert_eq!(
            json!(rebuild_request(&doc, index + 1).unwrap()),
            json!(first)
        );
        assert_eq!(
            json!(rebuild_request(&doc, next + 1).unwrap()),
            json!(second)
        );
        assert_eq!(
            doc["steps"][index]["observation"]["results"][0]["source_call_id"],
            "call-1"
        );
        assert_eq!(
            doc["steps"][index]["extra"]["stop_reason"],
            json!(StopReason::EndTurn)
        );
        assert_eq!(doc["steps"][index]["extra"]["response_id"], "response-id");
        assert_eq!(doc["final_metrics"]["total_prompt_tokens"], 15);
        assert_eq!(doc["final_metrics"]["total_completion_tokens"], 4);
        let raw = std::fs::read_to_string(&path).unwrap();
        for unique in [
            "unique-system-prompt",
            "unique-tool-description",
            "unique-output-schema",
            "unique-result-content",
            "α-one",
            "reason-signature",
        ] {
            assert_eq!(raw.matches(unique).count(), 1, "duplicated {unique}");
        }
        assert!(!raw.contains("stream_events"));
        for step in doc["steps"].as_array().unwrap() {
            assert!(step["extra"].get("request").is_none());
            OffsetDateTime::parse(step["timestamp"].as_str().unwrap(), &Rfc3339).unwrap();
        }
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
    fn long_histories_grow_linearly_even_with_identical_replies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let mut messages = vec![Message::user("task")];
        let mut checks = Vec::new();
        let mut midpoint = 0;
        for turn in 0..128 {
            let mut input = request(messages.clone());
            input.cache = polaris::models::llm::CacheControl::prefix()
                .with_breakpoints([0, messages.len() - 1]);
            let index = trace.begin("fake/model", &input);
            trace.response(index, &response(vec![AssistantBlock::text("same-answer")]));
            if [0, 1, 63, 127].contains(&turn) {
                checks.push((index + 1, input));
            }
            messages.push(Message::assistant("same-answer"));
            messages.push(Message::user(format!("turn-{turn}: {}", "x".repeat(512))));
            if turn == 63 {
                midpoint = std::fs::metadata(&path).unwrap().len();
            }
        }
        let final_size = std::fs::metadata(&path).unwrap().len();
        assert!(
            final_size < midpoint * 23 / 10,
            "growth {midpoint} -> {final_size}"
        );
        let doc = document(&path);
        for (step, input) in checks {
            assert_eq!(json!(rebuild_request(&doc, step).unwrap()), json!(input));
        }
    }

    #[test]
    fn compaction_and_sliding_histories_preserve_original_results_and_new_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let call = ToolCall::new("tool", "read_file", json!({"path":"a"}));
        let first = trace.begin("fake/model", &request(vec![Message::user("task")]));
        trace.response(
            first,
            &response(vec![AssistantBlock::ToolCall(call.clone())]),
        );
        let original = Message::User {
            content: vec![call.result(ToolResultContent::Text("original-observation-body".into()))],
        };
        trace.message(&original);
        let original_request = request(vec![
            Message::user("task"),
            Message::Assistant {
                id: None,
                content: vec![AssistantBlock::ToolCall(call.clone())],
            },
            original,
        ]);
        let before = trace.begin("fake/model", &original_request);
        trace.response(before, &response(vec![AssistantBlock::text("continued")]));
        let compacted = request(vec![
            Message::user("Conversation summary checkpoint (older history): remembered facts"),
            Message::Assistant {
                id: None,
                content: vec![AssistantBlock::ToolCall(call.clone())],
            },
            Message::User {
                content: vec![call.result(ToolResultContent::Text(
                    "[elided 2048 chars of tool output]".into(),
                ))],
            },
            Message::assistant("continued"),
            Message::user("next"),
        ]);
        let after = trace.begin("fake/model", &compacted);
        let sliding = request(
            compacted.messages[1..]
                .iter()
                .cloned()
                .chain([Message::user("new tail")])
                .collect(),
        );
        let slid = trace.begin("fake/model", &sliding);
        let doc = document(&path);
        for (id, expected) in [
            (before + 1, original_request),
            (after + 1, compacted),
            (slid + 1, sliding),
        ] {
            assert_eq!(json!(rebuild_request(&doc, id).unwrap()), json!(expected));
        }
        assert_eq!(
            doc["steps"][first]["observation"]["results"][0]["content"],
            "original-observation-body"
        );
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .matches("original-observation-body")
                .count(),
            1
        );
        assert_eq!(
            doc["steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|step| step["extra"]["kind"] == "compacted_context")
                .count(),
            2
        );
    }

    #[test]
    fn stream_checkpoints_preserve_signatures_partial_arguments_and_final_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let index = trace.begin("fake/model", &request(vec![Message::user("task")]));
        let mut capture = StreamCapture::new(trace.clone(), index);
        feed(
            &mut capture,
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning,
            },
        );
        feed(
            &mut capture,
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("thought".into()),
            },
        );
        feed(
            &mut capture,
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Signature("signature".into()),
            },
        );
        feed(&mut capture, StreamEvent::ContentBlockStop { index: 0 });
        feed(
            &mut capture,
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlockStartData::ToolCall {
                    id: "call".into(),
                    call_id: Some("wire".into()),
                    name: "read_file".into(),
                },
            },
        );
        feed(
            &mut capture,
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "{\"path\":".into(),
                },
            },
        );
        let snapshot = capture.snapshot();
        trace.assistant(index, snapshot, false);
        assert_eq!(
            document(&path)["steps"][index]["extra"]["partial_tool_calls"][0]["arguments"],
            "{\"path\":"
        );
        feed(
            &mut capture,
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "\"a\"}".into(),
                },
            },
        );
        // Concurrent tool blocks can close in a different order than they
        // started. Match the framework collector's completed-block ordering.
        feed(
            &mut capture,
            StreamEvent::ContentBlockStart {
                index: 2,
                block: ContentBlockStartData::ToolCall {
                    id: "second".into(),
                    call_id: Some("second-wire".into()),
                    name: "read_file".into(),
                },
            },
        );
        feed(
            &mut capture,
            StreamEvent::ContentBlockDelta {
                index: 2,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "{}".into(),
                },
            },
        );
        feed(&mut capture, StreamEvent::ContentBlockStop { index: 2 });
        feed(&mut capture, StreamEvent::ContentBlockStop { index: 1 });
        let usage = Usage {
            input_tokens: Some(30),
            output_tokens: Some(5),
            ..Default::default()
        };
        feed(
            &mut capture,
            StreamEvent::MessageDelta {
                usage: usage.clone(),
            },
        );
        feed(
            &mut capture,
            StreamEvent::MessageStop {
                usage,
                stop_reason: StopReason::ToolUse,
                id: Some("response".into()),
                model: Some("remote".into()),
            },
        );
        drop(capture);
        let doc = document(&path);
        let message = messages::load(&doc, index + 1).unwrap();
        let Message::Assistant { content, .. } = message else {
            panic!("assistant response")
        };
        assert!(
            matches!(&content[0], AssistantBlock::Reasoning(block) if block.signature.as_deref() == Some("signature"))
        );
        assert!(
            matches!(&content[1], AssistantBlock::ToolCall(call) if call.call_id.as_deref() == Some("second-wire"))
        );
        assert!(
            matches!(&content[2], AssistantBlock::ToolCall(call) if call.call_id.as_deref() == Some("wire"))
        );
        assert_eq!(doc["steps"][index]["extra"]["status"], "completed");
        assert_eq!(doc["final_metrics"]["total_prompt_tokens"], 30);
        assert_eq!(doc["final_metrics"]["total_completion_tokens"], 5);
    }

    #[test]
    fn native_media_empty_reasoning_and_invalid_tool_arguments_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let call = ToolCall::new("call", "read_file", json!([1, 2]));
        let reply = response(vec![
            AssistantBlock::Reasoning(ReasoningBlock {
                id: None,
                reasoning: vec![String::new()],
                signature: Some("signature".into()),
            }),
            AssistantBlock::ToolCall(call),
        ]);
        let first = trace.begin("fake/model", &request(vec![Message::user("task")]));
        trace.response(first, &reply);
        let input = request(vec![
            Message::Assistant {
                id: None,
                content: reply.content,
            },
            Message::User {
                content: vec![UserBlock::image_base64(
                    "aW1hZ2U=",
                    polaris::models::llm::ImageMediaType::PNG,
                )],
            },
        ]);
        let id = trace.begin("fake/model", &input);
        let doc = document(&path);
        assert_eq!(json!(rebuild_request(&doc, id + 1).unwrap()), json!(input));
        assert!(doc["steps"][first]["tool_calls"][0]["arguments"].is_object());
    }

    fn text_capture(trace: &RunTrajectory, index: usize, text: &str) -> StreamCapture {
        let mut capture = StreamCapture::new(trace.clone(), index);
        feed(
            &mut capture,
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            },
        );
        feed(
            &mut capture,
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(text.into()),
            },
        );
        capture
    }

    fn texts(doc: &Value, index: usize) -> Vec<String> {
        let Message::Assistant { content, .. } = messages::load(doc, index + 1).unwrap() else {
            panic!("assistant response")
        };
        content
            .into_iter()
            .map(|block| match block {
                AssistantBlock::Text(text) => text.text,
                other => panic!("unexpected block {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_dropped_stream_is_partial_and_keeps_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let index = trace.begin("fake/model", &request(vec![Message::user("task")]));
        drop(text_capture(&trace, index, "half an answer"));
        trace.finish(0).unwrap();
        let doc = document(&path);
        assert_eq!(doc["steps"][index]["extra"]["status"], "partial");
        assert_eq!(texts(&doc, index), ["half an answer"]);
    }

    #[test]
    fn an_error_event_followed_by_a_drop_stays_failed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let index = trace.begin("fake/model", &request(vec![Message::user("task")]));
        let capture = text_capture(&trace, index, "cut off");
        trace.failure(index, "http");
        drop(capture);
        trace.finish(1).unwrap();
        let doc = document(&path);
        assert_eq!(doc["steps"][index]["extra"]["status"], "failed");
        assert_eq!(doc["steps"][index]["extra"]["error_type"], "http");
        assert_eq!(texts(&doc, index), ["cut off"]);
        assert_eq!(doc["extra"]["status"], "failed");
        assert_eq!(doc["extra"]["exit_code"], 1);
    }

    #[test]
    fn dropping_a_stream_does_no_file_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let index = trace.begin("fake/model", &request(vec![Message::user("task")]));
        let capture = text_capture(&trace, index, "unwritten");
        // Any write to the destination would now fail and be remembered.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        drop(capture);
        trace.check().expect("drop must not write the file");
        std::fs::remove_dir(&path).unwrap();
        // The next checkpoint writes what the drop kept in memory.
        trace.finish(0).unwrap();
        assert_eq!(texts(&document(&path), index), ["unwritten"]);
    }

    #[test]
    fn tool_results_and_messages_do_no_file_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let call = ToolCall::new("tool", "read_file", json!({"path":"a"}));
        let index = trace.begin("fake/model", &request(vec![Message::user("task")]));
        trace.response(
            index,
            &response(vec![AssistantBlock::ToolCall(call.clone())]),
        );
        // Any write to the destination would now fail and be remembered.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let result = call.result(ToolResultContent::Text("deferred-result".into()));
        trace.tool_results(std::slice::from_ref(&result));
        trace.message(&Message::user("deferred-message"));
        trace
            .check()
            .expect("tool results and messages must not write the file");
        std::fs::remove_dir(&path).unwrap();
        // The next checkpoint writes both.
        trace.finish(0).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("deferred-result"), "{written}");
        assert!(written.contains("deferred-message"), "{written}");
    }

    #[test]
    fn debug_shows_counts_without_content_and_never_waits_for_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "secret-task-text").unwrap();
        let shown = format!("{trace:?}");
        assert!(shown.contains("steps: 1"), "{shown}");
        assert!(shown.contains("failed: false"), "{shown}");
        assert!(!shown.contains("secret-task-text"), "{shown}");
        let guard = trace.lock();
        let busy = format!("{trace:?}");
        drop(guard);
        assert!(busy.contains("<busy>"), "{busy}");
    }

    #[test]
    fn existing_destinations_are_refused_and_write_failures_stay_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let error = RunTrajectory::create(&path, "other", "fake/model", "task").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(trace.finish(0).is_err());
        assert!(trace.check().is_err());
        // A later write works, yet the earlier failure is still reported.
        std::fs::remove_dir(&path).unwrap();
        assert!(trace.finish(0).is_err());
        assert_eq!(document(&path)["extra"]["status"], "completed");
    }

    #[test]
    fn tampered_documents_are_rejected_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        let first = trace.begin("fake/model", &request(vec![Message::user("task")]));
        trace.finish(0).unwrap();
        let doc = document(&path);
        rebuild_request(&doc, first + 1).expect("the untouched document rebuilds");
        let reason = |doc: &Value, id: usize| rebuild_request(doc, id).unwrap_err().to_string();
        // Step ids are one-based: zero, past the end and a non-model-call step.
        for id in [0, 999, 1] {
            assert_eq!(reason(&doc, id), "invalid model-call step", "step id {id}");
        }
        let input = format!("/steps/{first}/extra/input");
        let edits = [
            (
                format!("{input}/parameters"),
                json!(0),
                "invalid model-call step",
            ),
            (
                format!("{input}/history"),
                json!("x"),
                "invalid model-call step",
            ),
            (
                "/extra/histories/0/length".into(),
                json!(5),
                "invalid trajectory history reference",
            ),
            (
                "/extra/histories/0/segments".into(),
                json!([{"history": 1, "start": 0, "length": 1}]),
                "invalid trajectory history reference",
            ),
        ];
        for (pointer, value, expected) in edits {
            let mut edited = doc.clone();
            *edited.pointer_mut(&pointer).unwrap() = value;
            assert_eq!(reason(&edited, first + 1), expected, "edit at {pointer}");
        }
        let mut edited = doc.clone();
        edited["steps"][0]["extra"]
            .as_object_mut()
            .unwrap()
            .remove("native_message");
        assert_eq!(
            reason(&edited, first + 1),
            "invalid trajectory message reference"
        );
    }
}
