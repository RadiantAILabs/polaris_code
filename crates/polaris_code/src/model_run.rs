//! Run-wide model request policy and recording at the provider boundary.
//!
//! Wrapping a provider once covers the main loop, retries, context summaries,
//! reviewers and model-calling topologies. Systems continue to consume the
//! ordinary [`ModelRegistry`] contract.
//!
//! The [embedding guide](crate::docs::embedding) shows how a host assembles this
//! plugin. `docs/unattended-runs.md` in the repository describes the run
//! controls it serves.

use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::StreamExt;
use polaris::models::ModelRegistry;
#[cfg(doc)]
use polaris::models::ModelsPlugin;
use polaris::models::llm::{
    DynLlmProvider, GenerationError, LlmRequest, LlmResponse, LlmStream, ModelPricing,
    ReasoningEffort,
};
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin};

use crate::trajectory::{RunTrajectory, StreamCapture};

/// Immutable host policy applied to every request made through a provider.
#[derive(Debug, Clone, Default)]
pub struct ModelRunOptions {
    /// Output ceiling. A caller may request less, never more.
    pub max_output_tokens: Option<NonZeroU32>,
    /// Pinned provider-neutral effort, overriding every caller's request.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Optional complete-run ATIF recorder, created before any model call.
    pub trajectory: Option<RunTrajectory>,
}

struct RunProvider {
    inner: Arc<dyn DynLlmProvider>,
    name: &'static str,
    options: ModelRunOptions,
}

impl RunProvider {
    fn request(&self, mut request: LlmRequest) -> LlmRequest {
        if let Some(limit) = self.options.max_output_tokens {
            request.max_output_tokens = Some(
                request
                    .max_output_tokens
                    .map_or(limit, |value| value.min(limit)),
            );
        }
        if let Some(effort) = &self.options.reasoning_effort {
            request.reasoning_effort = Some(effort.clone());
        }
        request
    }
}

impl DynLlmProvider for RunProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn generate<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>> {
        Box::pin(async move {
            let request = self.request(request);
            // Recorder writes run on the blocking pool, never on this worker.
            let label = format!("{}/{model}", self.name());
            let (index, request) = match &self.options.trajectory {
                Some(trace) => {
                    let (index, request) = trace
                        .blocking(move |trace| (trace.begin(&label, &request), request))
                        .await;
                    (Some(index), request)
                }
                None => (None, request),
            };
            if let Some(trace) = &self.options.trajectory {
                trace.check().map_err(recording_error)?;
            }
            let result = self.inner.generate(model, request).await;
            let result = match (&self.options.trajectory, index) {
                (Some(trace), Some(index)) => {
                    trace
                        .blocking(move |trace| {
                            match &result {
                                Ok(response) => trace.response(index, response),
                                Err(error) => trace.failure(index, error.error_type()),
                            }
                            result
                        })
                        .await
                }
                _ => result,
            };
            if let Some(trace) = &self.options.trajectory {
                trace.check().map_err(recording_error)?;
            }
            result
        })
    }

    fn stream<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmStream, GenerationError>> + Send + 'a>> {
        Box::pin(async move {
            let request = self.request(request);
            let label = format!("{}/{model}", self.name());
            let (recording, request) = match self.options.trajectory.clone() {
                Some(trace) => {
                    let (index, request) = trace
                        .blocking(move |trace| (trace.begin(&label, &request), request))
                        .await;
                    (Some((trace, index)), request)
                }
                None => (None, request),
            };
            if let Some((trace, _)) = &recording {
                trace.check().map_err(recording_error)?;
            }
            let stream = match self.inner.stream(model, request).await {
                Ok(stream) => stream,
                Err(error) => {
                    if let Some((trace, index)) = recording {
                        let kind = error.error_type();
                        trace
                            .blocking(move |trace| trace.failure(index, kind))
                            .await;
                    }
                    return Err(error);
                }
            };
            let Some((trace, index)) = recording else {
                return Ok(stream);
            };
            let mut capture = StreamCapture::new(trace.clone(), index);
            // `then` handles one event at a time, so writes keep their order.
            // Only events that end a block or carry usage cost a write.
            Ok(Box::pin(stream.then(move |event| {
                let due = match &event {
                    Ok(event) => capture.event(event),
                    Err(_) => None,
                };
                let trace = trace.clone();
                async move {
                    match &event {
                        Ok(_) => {
                            if let Some(snapshot) = due {
                                trace
                                    .blocking(move |trace| trace.assistant(index, snapshot, false))
                                    .await;
                            }
                        }
                        Err(error) => {
                            let kind = error.error_type();
                            trace
                                .blocking(move |trace| trace.failure(index, kind))
                                .await;
                        }
                    }
                    trace.check().map_err(recording_error)?;
                    event
                }
            })) as LlmStream)
        })
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.inner.pricing(model)
    }
    fn endpoint(&self) -> Option<String> {
        self.inner.endpoint()
    }
}

fn recording_error(error: std::io::Error) -> GenerationError {
    GenerationError::UnsupportedOperation(format!("trajectory export failed: {error}"))
}

/// Register a provider with run-wide effort, output limits and optional ATIF
/// capture. Use this instead of its ordinary registration plugin when every
/// caller must follow the same immutable request policy.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// | --- | --- | --- |
/// | _none_ | — | Extends the existing model registry; owns no session state. |
///
/// # APIs Provided
///
/// | API | Description |
/// | --- | --- |
/// | _none_ | Consumers use the ordinary [`ModelRegistry::llm`] contract. |
///
/// # Dependencies
///
/// [`ModelsPlugin`], which provides [`ModelRegistry`]. The macro declares an
/// extension of that capability. Do not also register the same provider name.
/// A recorder, when wanted, is supplied through [`ModelRunOptions`]; the host
/// separately publishes the same [`RunTrajectory`] for coding tool results.
///
/// # Extends
///
/// - [`ModelsPlugin`] / [`ModelRegistry`]: registers one provider under the
///   wrapped provider's name, or the alias from
///   [`with_registry_name`](Self::with_registry_name). Every request through it
///   gets the output ceiling and pinned reasoning effort from
///   [`ModelRunOptions`], and is recorded when a [`RunTrajectory`] is supplied.
///   The remote model name is unchanged.
///
/// # Lifecycle
///
/// Construction starts no I/O. `build` registers one decorated provider.
/// Requests and responses are recorded during calls, with file writes on
/// Tokio's blocking pool; the host finalizes the recorder after tool cleanup.
/// Tool results, messages and the partial content of a dropped stream are
/// queued in memory, and the next checkpoint or [`RunTrajectory::finish`]
/// writes them.
///
/// # Example
///
/// ```no_run
/// use polaris::models::{ModelsPlugin, openai::OpenAiProvider};
/// use polaris::system::server::Server;
/// use polaris_code::model_run::{ModelRunOptions, ModelRunPlugin};
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(ModelRunPlugin::new(
///     OpenAiProvider::new(std::env::var("OPENAI_API_KEY")?),
///     ModelRunOptions::default(),
/// ));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct ModelRunPlugin {
    provider: Arc<dyn DynLlmProvider>,
    name: &'static str,
    options: ModelRunOptions,
}

impl std::fmt::Debug for ModelRunPlugin {
    // The wrapped provider is left out: its own debug output may hold credentials.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRunPlugin")
            .field("name", &self.name)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl ModelRunPlugin {
    /// Decorate one provider with the given process-wide request policy.
    pub fn new(provider: impl DynLlmProvider, options: ModelRunOptions) -> Self {
        Self {
            name: provider.name(),
            provider: Arc::new(provider),
            options,
        }
    }
    /// Register under a host-facing alias, preserving the remote model name.
    #[must_use]
    pub fn with_registry_name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }
}

#[plugin(id = "polaris_code::model_run", version = "0.1.0")]
impl Plugin for ModelRunPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        register(
            &mut registry,
            self.provider.clone(),
            self.name,
            &self.options,
        );
    }
}

/// Register `provider` under `name` with `options` applied to every request.
///
/// The decoration [`ModelRunPlugin`] installs, for capabilities that register
/// several routes from one plugin.
pub(crate) fn register(
    registry: &mut ModelRegistry,
    provider: Arc<dyn DynLlmProvider>,
    name: &'static str,
    options: &ModelRunOptions,
) {
    registry.register_llm_provider(RunProvider {
        inner: provider,
        name,
        options: options.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::llm::{
        AssistantBlock, ContentBlockDelta, ContentBlockStartData, LlmProvider, Message,
        ReasoningEffort, StopReason, StreamEvent, Usage,
    };
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct Fake(Arc<Mutex<Vec<(String, LlmRequest)>>>);
    impl LlmProvider for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn generate(
            &self,
            model: &str,
            request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            self.0.lock().unwrap().push((model.into(), request));
            Ok(LlmResponse {
                content: vec![AssistantBlock::text("answer")],
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
                id: None,
                model: None,
            })
        }
        async fn stream(
            &self,
            model: &str,
            request: LlmRequest,
        ) -> Result<LlmStream, GenerationError> {
            self.0.lock().unwrap().push((model.into(), request));
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    #[tokio::test]
    async fn every_call_pins_effort_and_caps_output_without_changing_the_remote_id() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = RunProvider {
            name: "openai-chat",
            inner: Arc::new(Fake(requests.clone())),
            options: ModelRunOptions {
                max_output_tokens: NonZeroU32::new(512),
                reasoning_effort: Some(ReasoningEffort::Max),
                trajectory: None,
            },
        };
        let request = LlmRequest {
            messages: vec![Message::user("main")],
            max_output_tokens: NonZeroU32::new(1024),
            reasoning_effort: Some(ReasoningEffort::Low),
            ..Default::default()
        };
        provider
            .generate("vendor/model:route", request)
            .await
            .unwrap();
        let stream = provider
            .stream(
                "vendor/model:route",
                LlmRequest {
                    max_output_tokens: NonZeroU32::new(64),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        drop(stream);
        provider
            .generate("vendor/model:route", LlmRequest::default())
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (model, request) in requests.iter() {
            assert_eq!(model, "vendor/model:route");
            assert_eq!(request.reasoning_effort, Some(ReasoningEffort::Max));
        }
        assert_eq!(requests[0].1.max_output_tokens.unwrap().get(), 512);
        assert_eq!(requests[1].1.max_output_tokens.unwrap().get(), 64);
        assert_eq!(requests[2].1.max_output_tokens.unwrap().get(), 512);
    }

    #[tokio::test]
    async fn a_failed_recording_stops_before_another_provider_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trajectory.json");
        let trajectory = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = RunProvider {
            name: "fake",
            inner: Arc::new(Fake(requests.clone())),
            options: ModelRunOptions {
                trajectory: Some(trajectory),
                ..Default::default()
            },
        };
        let error = provider
            .generate("model", LlmRequest::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, GenerationError::UnsupportedOperation(message) if message.contains("trajectory export failed")),
            "{error:?}"
        );
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_defaults_remain_when_effort_is_unset() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = RunProvider {
            name: "fake",
            inner: Arc::new(Fake(requests.clone())),
            options: ModelRunOptions::default(),
        };
        provider
            .generate("model", LlmRequest::default())
            .await
            .unwrap();
        assert!(requests.lock().unwrap()[0].1.reasoning_effort.is_none());
    }

    type Calls = Arc<Mutex<Vec<(String, LlmRequest)>>>;

    /// What a [`Scripted`] provider does on its next call.
    enum Script {
        Answer,
        GenerateError,
        StreamStartError,
        Stream(Vec<Result<StreamEvent, GenerationError>>),
    }

    /// A provider whose next result is chosen by the test.
    struct Scripted {
        calls: Calls,
        script: Mutex<Option<Script>>,
    }
    impl LlmProvider for Scripted {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn generate(
            &self,
            model: &str,
            request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            self.calls.lock().unwrap().push((model.into(), request));
            match self.script.lock().unwrap().take() {
                Some(Script::GenerateError) => {
                    Err(GenerationError::Http("connection reset".into()))
                }
                _ => Ok(LlmResponse {
                    content: vec![AssistantBlock::text("answer")],
                    usage: Usage::default(),
                    stop_reason: StopReason::EndTurn,
                    id: Some("response-id".into()),
                    model: None,
                }),
            }
        }
        async fn stream(
            &self,
            model: &str,
            request: LlmRequest,
        ) -> Result<LlmStream, GenerationError> {
            self.calls.lock().unwrap().push((model.into(), request));
            match self.script.lock().unwrap().take() {
                Some(Script::StreamStartError) => Err(GenerationError::Auth("bad key".into())),
                Some(Script::Stream(events)) => Ok(Box::pin(futures_util::stream::iter(events))),
                _ => Ok(Box::pin(futures_util::stream::empty())),
            }
        }
    }

    /// A decorated provider writing to a real recorder in a temporary directory.
    struct Recorded {
        provider: RunProvider,
        calls: Calls,
        trace: RunTrajectory,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Recorded {
        fn new(script: Script) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("trajectory.json");
            let trace = RunTrajectory::create(&path, "run", "fake/model", "task").unwrap();
            let calls = Calls::default();
            let provider = RunProvider {
                name: "fake",
                inner: Arc::new(Scripted {
                    calls: calls.clone(),
                    script: Mutex::new(Some(script)),
                }),
                options: ModelRunOptions {
                    trajectory: Some(trace.clone()),
                    ..Default::default()
                },
            };
            Self {
                provider,
                calls,
                trace,
                path,
                _dir: dir,
            }
        }

        /// Make every later write to the destination fail.
        fn break_destination(&self) {
            std::fs::remove_file(&self.path).unwrap();
            std::fs::create_dir(&self.path).unwrap();
        }

        /// The one model-call step, after writing everything that is pending.
        fn model_call(&self) -> Value {
            let _ = self.trace.finish(0);
            let document: Value =
                serde_json::from_slice(&std::fs::read(&self.path).unwrap()).unwrap();
            let mut calls = document["steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|step| step["extra"]["kind"] == "model_call");
            let step = calls.next().expect("a model-call step").clone();
            assert!(calls.next().is_none(), "exactly one model-call step");
            step
        }
    }

    fn block_start(
        index: u32,
        block: ContentBlockStartData,
    ) -> Result<StreamEvent, GenerationError> {
        Ok(StreamEvent::ContentBlockStart { index, block })
    }

    fn text_delta(index: u32, text: &str) -> Result<StreamEvent, GenerationError> {
        Ok(StreamEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::Text(text.into()),
        })
    }

    #[tokio::test]
    async fn a_recorded_generate_call_stores_the_request_and_the_response() {
        let run = Recorded::new(Script::Answer);
        let request = LlmRequest {
            messages: vec![Message::user("task")],
            ..Default::default()
        };
        let response = run.provider.generate("model", request).await.unwrap();
        assert_eq!(response.id.as_deref(), Some("response-id"));
        let step = run.model_call();
        assert_eq!(step["model_name"], "fake/model");
        assert_eq!(step["extra"]["status"], "completed");
        assert_eq!(step["extra"]["response_id"], "response-id");
        run.trace.check().unwrap();
        assert_eq!(run.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_recorded_generate_error_is_returned_and_marks_the_step_failed() {
        let run = Recorded::new(Script::GenerateError);
        let error = run
            .provider
            .generate("model", LlmRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(error, GenerationError::Http(_)), "{error:?}");
        let step = run.model_call();
        assert_eq!(step["extra"]["status"], "failed");
        assert_eq!(step["extra"]["error_type"], "http");
        run.trace.check().unwrap();
    }

    #[tokio::test]
    async fn a_stream_that_cannot_start_marks_the_step_failed() {
        let run = Recorded::new(Script::StreamStartError);
        let Err(error) = run.provider.stream("model", LlmRequest::default()).await else {
            panic!("the provider refused to start the stream")
        };
        assert!(matches!(error, GenerationError::Auth(_)), "{error:?}");
        let step = run.model_call();
        assert_eq!(step["extra"]["status"], "failed");
        assert_eq!(step["extra"]["error_type"], "auth");
    }

    #[tokio::test]
    async fn a_mid_stream_error_reaches_the_caller_and_keeps_the_failure() {
        let run = Recorded::new(Script::Stream(vec![
            block_start(0, ContentBlockStartData::Text),
            text_delta(0, "cut off"),
            Err(GenerationError::Http("connection reset".into())),
        ]));
        let mut stream = run
            .provider
            .stream("model", LlmRequest::default())
            .await
            .unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        let error = stream.next().await.unwrap().unwrap_err();
        assert!(matches!(error, GenerationError::Http(_)), "{error:?}");
        // Dropping the errored stream must not turn the failure into a partial.
        drop(stream);
        let step = run.model_call();
        assert_eq!(step["extra"]["status"], "failed");
        assert_eq!(step["extra"]["error_type"], "http");
    }

    #[tokio::test]
    async fn a_stream_dropped_midway_is_recorded_as_partial() {
        let run = Recorded::new(Script::Stream(vec![
            block_start(0, ContentBlockStartData::Text),
            text_delta(0, "half an answer"),
            text_delta(0, " that never ends"),
        ]));
        let mut stream = run
            .provider
            .stream("model", LlmRequest::default())
            .await
            .unwrap();
        for _ in 0..2 {
            stream.next().await.unwrap().unwrap();
        }
        drop(stream);
        let step = run.model_call();
        assert_eq!(step["extra"]["status"], "partial");
        // The half that arrived is kept in the step's message text.
        assert_eq!(step["message"], "half an answer");
    }

    #[tokio::test]
    async fn a_recording_that_fails_mid_stream_ends_the_stream_with_an_error() {
        let run = Recorded::new(Script::Stream(vec![
            block_start(0, ContentBlockStartData::Text),
            text_delta(0, "answer"),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
        ]));
        let mut stream = run
            .provider
            .stream("model", LlmRequest::default())
            .await
            .unwrap();
        // Deltas need no write, so a broken destination goes unnoticed until
        // the first event that does.
        run.break_destination();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        let error = stream.next().await.unwrap().unwrap_err();
        assert!(
            matches!(&error, GenerationError::UnsupportedOperation(message) if message.contains("trajectory export failed")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_recording_stops_a_stream_before_the_provider_is_called() {
        let run = Recorded::new(Script::Answer);
        run.break_destination();
        let Err(error) = run.provider.stream("model", LlmRequest::default()).await else {
            panic!("the stream must not start")
        };
        assert!(
            matches!(&error, GenerationError::UnsupportedOperation(message) if message.contains("trajectory export failed")),
            "{error:?}"
        );
        assert!(run.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn debug_hides_the_wrapped_provider() {
        struct Secret;
        impl LlmProvider for Secret {
            fn name(&self) -> &'static str {
                "secret-provider"
            }
            async fn generate(
                &self,
                _: &str,
                _: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                unreachable!("not called")
            }
        }
        impl std::fmt::Debug for Secret {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("api-key-123")
            }
        }
        let plugin = ModelRunPlugin::new(Secret, ModelRunOptions::default());
        let shown = format!("{plugin:?}");
        assert!(shown.contains("secret-provider"), "{shown}");
        assert!(!shown.contains("api-key-123"), "{shown}");
    }
}
