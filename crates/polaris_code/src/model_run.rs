//! Run-wide model request policy and recording at the provider boundary.
//!
//! Wrapping a provider once covers the main loop, retries, context summaries,
//! reviewers and model-calling topologies. Systems continue to consume the
//! ordinary `ModelRegistry` contract.

use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::StreamExt;
use polaris::models::ModelRegistry;
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
            let index = self
                .options
                .trajectory
                .as_ref()
                .map(|trace| trace.begin(&format!("{}/{model}", self.name()), &request));
            if let Some(trace) = &self.options.trajectory {
                trace.check().map_err(recording_error)?;
            }
            let result = self.inner.generate(model, request).await;
            if let (Some(trace), Some(index)) = (&self.options.trajectory, index) {
                match &result {
                    Ok(response) => trace.response(index, response),
                    Err(error) => trace.failure(index, error.error_type()),
                }
            }
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
            let recording = self.options.trajectory.clone().map(|trace| {
                let index = trace.begin(&format!("{}/{model}", self.name()), &request);
                (trace, index)
            });
            if let Some((trace, _)) = &recording {
                trace.check().map_err(recording_error)?;
            }
            let stream = match self.inner.stream(model, request).await {
                Ok(stream) => stream,
                Err(error) => {
                    if let Some((trace, index)) = recording {
                        trace.failure(index, error.error_type());
                    }
                    return Err(error);
                }
            };
            let Some((trace, index)) = recording else {
                return Ok(stream);
            };
            let mut capture = StreamCapture::new(trace.clone(), index);
            Ok(Box::pin(stream.map(move |event| {
                match &event {
                    Ok(event) => capture.event(event),
                    Err(error) => trace.failure(index, error.error_type()),
                }
                trace.check().map_err(recording_error)?;
                event
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
/// | _none_ | Consumers use the ordinary `ModelRegistry::llm` contract. |
///
/// # Dependencies
///
/// `ModelsPlugin`, which provides `ModelRegistry`. The macro declares an
/// extension of that capability. Do not also register the same provider name.
/// A recorder, when wanted, is supplied through `ModelRunOptions`; the host
/// separately publishes the same `RunTrajectory` for coding tool results.
///
/// # Lifecycle
///
/// Construction starts no I/O. `build` registers one decorated provider.
/// Requests and responses are recorded during calls; the host finalizes the
/// recorder after tool cleanup. Dropped streams checkpoint partial content.
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
        registry.register_llm_provider(RunProvider {
            inner: self.provider.clone(),
            name: self.name,
            options: self.options.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::llm::{
        AssistantBlock, LlmProvider, Message, ReasoningEffort, StopReason, Usage,
    };
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
        assert!(error.to_string().contains("trajectory export failed"));
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
}
