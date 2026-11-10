//! Switchable model connections.
//!
//! [`ConnectionPlugin`] registers model routes that forward to whichever
//! prepared provider is active, and [`ConnectionRuntime`] is the handle a host
//! keeps to change it between turns. The dialogs that prepare a connection live
//! in [`crate::onboarding`]; the session commands that drive them live in
//! [`crate::terminal::SessionAccounts`].

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use polaris::models::llm::{
    AssistantBlock, DynLlmProvider, GenerationError, LlmProvider, LlmRequest, LlmResponse,
    LlmStream, Message, ModelPricing,
};
use polaris::models::{ModelRegistry, pricing::PriceStore};
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin};
use polaris::system::server::Server;

use crate::model_run::ModelRunOptions;
use crate::providers::{Provider, ProviderReport, ReadyProvider};

/// Drop reasoning blocks the destination protocol cannot replay.
///
/// A session may carry history from another connection. Signed Anthropic and
/// Bedrock reasoning, encrypted `ChatGPT` reasoning and unsigned summaries are
/// each accepted only by the protocol that produced them.
fn portable_request(mut request: LlmRequest, route: Provider) -> LlmRequest {
    request.messages.retain_mut(|message| {
        if let Message::Assistant { content, .. } = message {
            content.retain(|block| {
                let AssistantBlock::Reasoning(reasoning) = block else {
                    return true;
                };
                let signature = reasoning.signature.as_deref().filter(|s| !s.is_empty());
                let chatgpt =
                    signature.is_some_and(crate::chatgpt::responses::is_chatgpt_reasoning);
                match route {
                    Provider::Anthropic | Provider::Bedrock => signature.is_some() && !chatgpt,
                    Provider::OpenAi => {
                        signature.is_none()
                            && reasoning.id.as_deref().is_some_and(|id| !id.is_empty())
                    }
                    Provider::OpenAiChat => signature.is_none(),
                    Provider::ChatGpt => chatgpt,
                }
            });
            return !content.is_empty();
        }
        true
    });
    request
}

struct Active {
    provider: Option<Arc<dyn DynLlmProvider>>,
    report: ProviderReport,
    account_id: Option<String>,
}

/// Holds the active connection for registered model routes.
///
/// The host switches connections between turns. Requests already in progress
/// retain the provider they started with. After a plan usage limit, requests
/// stay paused until a connection is activated again, so billing never moves
/// to another source without the user choosing it.
#[derive(Clone)]
pub struct ConnectionRuntime {
    active: Arc<RwLock<Active>>,
    max_tokens: NonZeroU32,
    pricing: PriceStore,
    usage_paused: Arc<AtomicBool>,
    usage_notice: Arc<AtomicBool>,
}

impl std::fmt::Debug for ConnectionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionRuntime")
            .field("report", &self.report())
            .field("connected", &self.is_connected())
            .field("usage_paused", &self.is_usage_paused())
            .finish_non_exhaustive()
    }
}

impl ConnectionRuntime {
    /// Create a runtime with the given connection active.
    pub fn new(ready: &ReadyProvider, max_tokens: NonZeroU32, pricing: PriceStore) -> Self {
        Self {
            active: Arc::new(RwLock::new(Self::active_for(ready, max_tokens, &pricing))),
            max_tokens,
            pricing,
            usage_paused: Arc::default(),
            usage_notice: Arc::default(),
        }
    }

    fn active_for(ready: &ReadyProvider, max_tokens: NonZeroU32, pricing: &PriceStore) -> Active {
        Active {
            provider: Some(ready.implementation(max_tokens, pricing.clone())),
            report: ready.report().clone(),
            account_id: ready.chatgpt_account_id().map(str::to_string),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Active> {
        self.active.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the active connection and clear any usage-limit pause.
    /// Call between turns, after setup and validation have completed.
    /// The destination must be among the routes registered for this runtime.
    /// Use [`ConnectionPlugin::with_switching`] to share it across all built-in
    /// routes. Handles from [`ReadyProvider::install`] can replace an account
    /// or credentials only for the same [`Provider`].
    pub fn activate(&self, ready: &ReadyProvider) {
        let active = Self::active_for(ready, self.max_tokens, &self.pricing);
        *self.active.write().unwrap_or_else(|e| e.into_inner()) = active;
        self.usage_paused.store(false, Ordering::Release);
        self.usage_notice.store(false, Ordering::Release);
    }

    /// Stop serving model requests, even if environment credentials remain set.
    pub fn disconnect(&self) {
        self.active
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .provider = None;
    }

    /// Whether a connection is selected. A usage-limit pause does not disconnect it.
    pub fn is_connected(&self) -> bool {
        self.read().provider.is_some()
    }

    /// Whether requests are paused because the active plan reached a usage limit.
    pub fn is_usage_paused(&self) -> bool {
        self.usage_paused.load(Ordering::Acquire)
    }

    /// Provider details for the current connection, including sign-out state.
    pub fn report(&self) -> ProviderReport {
        let active = self.read();
        if active.provider.is_none() {
            ProviderReport::new(
                active.report.provider(),
                active.report.region().map(str::to_string),
                active.report.endpoint().map(str::to_string),
                "signed out",
            )
        } else {
            active.report.clone()
        }
    }

    /// Whether a usage limit was reached since the last call, clearing the flag.
    pub(crate) fn take_usage_notice(&self) -> bool {
        self.usage_notice.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn account_id(&self) -> Option<String> {
        self.read().account_id.clone()
    }

    fn observe(&self, result: &Result<impl Sized, GenerationError>) {
        if result
            .as_ref()
            .err()
            .is_some_and(crate::chatgpt::responses::is_plan_limit)
        {
            self.usage_paused.store(true, Ordering::Release);
            self.usage_notice.store(true, Ordering::Release);
        }
    }

    fn provider(&self, route: Provider) -> Result<Arc<dyn DynLlmProvider>, GenerationError> {
        let active = self.read();
        if active.report.provider() != route {
            return Err(GenerationError::InvalidRequest(format!(
                "model belongs to the inactive {} connection",
                route.name()
            )));
        }
        if self.is_usage_paused() {
            return Err(GenerationError::InvalidRequest(
                "requests are paused after a plan usage limit; reconnect to resume".into(),
            ));
        }
        active
            .provider
            .clone()
            .ok_or_else(|| GenerationError::Auth("no connection is active".into()))
    }
}

/// Registers model providers that share a switchable connection.
///
/// # Resources Provided
/// None. The caller retains a [`ConnectionRuntime`] handle.
///
/// # APIs Provided
/// None. Extends [`ModelRegistry`].
///
/// # Dependencies
/// Requires the [`ModelRegistry`] capability from `ModelsPlugin`.
///
/// # Lifecycle
/// Registers the initial provider at build time. [`Self::with_switching`] also
/// registers the other built-in provider names so the host can switch between them.
/// Only the active connection serves requests. Do not register these names again
/// through another plugin. Retain [`Self::runtime`] to manage the connection.
///
/// # Panics
/// Adding this plugin to a server more than once panics, including when the
/// instances name different routes: the framework requires unique plugin IDs.
/// Use [`ReadyProvider::install`] for independent providers in one server.
///
/// # Example
///
/// A session that can move to another account without losing its conversation
/// keeps the runtime handle and activates the replacement between turns.
///
/// ```
/// use std::num::NonZeroU32;
/// use polaris::models::{ModelsPlugin, pricing::PriceStore};
/// use polaris::system::server::Server;
/// use polaris_code::connection::ConnectionPlugin;
/// use polaris_code::credentials::CredentialStore;
/// use polaris_code::providers::{self, Provider};
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let env = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| "sk-ant-example".to_string());
/// let ready = providers::prepare(Provider::Anthropic, None, &env, &None::<CredentialStore>).await?;
/// let max_tokens = NonZeroU32::new(4096).ok_or("zero output limit")?;
///
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// let connections = ConnectionPlugin::new(&ready, max_tokens, PriceStore::builtin())
///     .with_switching();
/// let runtime = connections.runtime();
/// server.add_plugins(connections);
/// assert!(runtime.is_connected());
///
/// runtime.disconnect();
/// assert!(!runtime.is_connected());
/// runtime.activate(&ready);
/// assert!(runtime.is_connected());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ConnectionPlugin {
    runtime: ConnectionRuntime,
    options: ModelRunOptions,
    routes: Vec<Provider>,
}

impl ConnectionPlugin {
    /// Create a plugin for the prepared connection.
    pub fn new(ready: &ReadyProvider, max_tokens: NonZeroU32, pricing: PriceStore) -> Self {
        Self {
            runtime: ConnectionRuntime::new(ready, max_tokens, pricing),
            options: ModelRunOptions::default(),
            routes: vec![ready.report().provider()],
        }
    }

    /// Register all built-in provider names to allow connection switching.
    /// These names must not be registered by another plugin.
    #[must_use]
    pub fn with_switching(mut self) -> Self {
        self.routes = Provider::ALL.to_vec();
        self
    }

    /// Apply the same request options to the initial and replacement connections.
    #[must_use]
    pub fn with_options(mut self, options: ModelRunOptions) -> Self {
        self.options = options;
        self
    }

    /// Return a shared handle for managing the active connection.
    pub fn runtime(&self) -> ConnectionRuntime {
        self.runtime.clone()
    }

    fn register_routes(&self, registry: &mut ModelRegistry) {
        for &route in &self.routes {
            let provider: Arc<dyn DynLlmProvider> = Arc::new(Route {
                route,
                runtime: self.runtime.clone(),
            });
            crate::model_run::register(registry, provider, route.registry_name(), &self.options);
        }
    }
}

#[plugin(id = "polaris_code::connections", version = "0.1.0")]
impl Plugin for ConnectionPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        self.register_routes(&mut registry);
    }
}

// Independent providers need distinct plugin IDs to coexist in one server.
macro_rules! independent_connection_plugins {
    ($($name:ident => $id:tt),+ $(,)?) => {
        $(
            struct $name(ConnectionPlugin);

            #[plugin(id = $id, version = "0.1.0")]
            impl Plugin for $name {
                fn build(&self, mut registry: Extends<ModelRegistry>) {
                    self.0.register_routes(&mut registry);
                }
            }
        )+
    };
}

independent_connection_plugins! {
    AnthropicConnectionPlugin => "polaris_code::connections::anthropic",
    OpenAiConnectionPlugin => "polaris_code::connections::openai",
    OpenAiChatConnectionPlugin => "polaris_code::connections::openai_chat",
    BedrockConnectionPlugin => "polaris_code::connections::bedrock",
    ChatGptConnectionPlugin => "polaris_code::connections::chatgpt",
}

impl ReadyProvider {
    /// Register this provider with request options shared by every model call.
    ///
    /// The options also cover auxiliary calls and trajectory recording. The
    /// route filters reasoning from another provider's history, as in a
    /// switching session. Different prepared providers can share one server.
    /// The returned handle can disconnect or reactivate this route, including
    /// resuming requests after a plan usage limit.
    ///
    /// # Panics
    /// Panics if this provider was already installed in the server.
    pub fn install_with_options(
        &self,
        server: &mut Server,
        max_output_tokens: NonZeroU32,
        pricing: PriceStore,
        options: ModelRunOptions,
    ) -> ConnectionRuntime {
        let connection =
            ConnectionPlugin::new(self, max_output_tokens, pricing).with_options(options);
        let runtime = connection.runtime();
        match self.report().provider() {
            Provider::Anthropic => {
                server.add_plugins(AnthropicConnectionPlugin(connection));
            }
            Provider::OpenAi => {
                server.add_plugins(OpenAiConnectionPlugin(connection));
            }
            Provider::OpenAiChat => {
                server.add_plugins(OpenAiChatConnectionPlugin(connection));
            }
            Provider::Bedrock => {
                server.add_plugins(BedrockConnectionPlugin(connection));
            }
            Provider::ChatGpt => {
                server.add_plugins(ChatGptConnectionPlugin(connection));
            }
        }
        runtime
    }

    /// Register this provider with default request options.
    /// Returns its connection handle for disconnecting or reconnecting the route.
    ///
    /// # Panics
    /// Panics if this provider was already installed in the server.
    pub fn install(
        &self,
        server: &mut Server,
        max_output_tokens: NonZeroU32,
        pricing: PriceStore,
    ) -> ConnectionRuntime {
        self.install_with_options(
            server,
            max_output_tokens,
            pricing,
            ModelRunOptions::default(),
        )
    }
}

struct Route {
    route: Provider,
    runtime: ConnectionRuntime,
}

impl LlmProvider for Route {
    fn name(&self) -> &'static str {
        self.route.registry_name()
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        let result = self
            .runtime
            .provider(self.route)?
            .generate(model, portable_request(request, self.route))
            .await;
        self.runtime.observe(&result);
        result
    }

    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let result = self
            .runtime
            .provider(self.route)?
            .stream(model, portable_request(request, self.route))
            .await;
        self.runtime.observe(&result);
        let stream = result?;
        let runtime = self.runtime.clone();
        Ok(Box::pin(
            stream.inspect(move |result| runtime.observe(result)),
        ))
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.runtime.provider(self.route).ok()?.pricing(model)
    }
    fn endpoint(&self) -> Option<String> {
        self.runtime.provider(self.route).ok()?.endpoint()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use polaris::models::ModelsPlugin;
    use polaris::models::llm::ReasoningBlock;

    use super::*;
    use crate::chatgpt::responses::{PlanUsageLimit, REASONING_PREFIX};
    use crate::credentials::CredentialStore;

    /// An assistant turn holding one reasoning block signed with `signature`
    /// and one text block.
    fn history(signature: Option<String>) -> LlmRequest {
        LlmRequest {
            messages: vec![Message::Assistant {
                id: None,
                content: vec![
                    AssistantBlock::Reasoning(ReasoningBlock {
                        id: None,
                        reasoning: vec![],
                        signature,
                    }),
                    AssistantBlock::text("kept answer"),
                ],
            }],
            ..Default::default()
        }
    }

    fn assistant_blocks(request: &LlmRequest) -> &[AssistantBlock] {
        match &request.messages[0] {
            Message::Assistant { content, .. } => content,
            _ => panic!("the assistant turn stays an assistant turn"),
        }
    }

    /// `provider`, prepared from a stored key.
    async fn ready_for(dir: &tempfile::TempDir, provider: Provider) -> ReadyProvider {
        let credentials = CredentialStore::at(dir.path().join("credentials.toml"));
        credentials
            .set_api_key(provider, "private-key")
            .expect("the test key is saved");
        crate::providers::prepare(provider, None, &|_| None, &credentials)
            .await
            .expect("a stored key prepares the provider")
    }

    /// A runtime whose active connection is `provider`.
    async fn runtime_for(dir: &tempfile::TempDir, provider: Provider) -> ConnectionRuntime {
        ConnectionRuntime::new(
            &ready_for(dir, provider).await,
            NonZeroU32::new(128).expect("nonzero output limit"),
            PriceStore::builtin(),
        )
    }

    fn plan_limit() -> GenerationError {
        GenerationError::Provider {
            status: None,
            message: "plan usage limit reached".into(),
            source: Some(Box::new(PlanUsageLimit)),
        }
    }

    #[test]
    fn portable_request_keeps_only_reasoning_the_route_can_replay() {
        let chatgpt = format!("{REASONING_PREFIX}state");
        // Kept by: Anthropic, OpenAI Responses, Chat Completions, ChatGPT.
        for (signature, kept) in [
            (None, [false, false, true, false]),
            (Some(String::new()), [false, false, true, false]),
            (
                Some("anthropic-signature".into()),
                [true, false, false, false],
            ),
            (Some(chatgpt), [false, false, false, true]),
        ] {
            for (route, keep) in [
                Provider::Anthropic,
                Provider::OpenAi,
                Provider::OpenAiChat,
                Provider::ChatGpt,
            ]
            .into_iter()
            .zip(kept)
            {
                let request = portable_request(history(signature.clone()), route);
                let blocks = assistant_blocks(&request);
                assert_eq!(
                    blocks.len(),
                    if keep { 2 } else { 1 },
                    "{route:?} with signature {signature:?} keeps reasoning: {keep}"
                );
                assert!(
                    matches!(blocks.last(), Some(AssistantBlock::Text(_))),
                    "text is never dropped"
                );
            }
        }
    }

    #[test]
    fn openai_replays_unsigned_reasoning_only_with_a_nonempty_item_id() {
        for id in [None, Some(""), Some("rs_native_item")] {
            let mut request = history(None);
            let Message::Assistant { content, .. } = &mut request.messages[0] else {
                unreachable!();
            };
            let AssistantBlock::Reasoning(reasoning) = &mut content[0] else {
                unreachable!();
            };
            reasoning.id = id.map(str::to_string);
            let request = portable_request(request, Provider::OpenAi);
            assert_eq!(
                assistant_blocks(&request).len(),
                if id.is_some_and(|id| !id.is_empty()) {
                    2
                } else {
                    1
                },
                "OpenAI Responses reasoning requires its native nonempty item id"
            );
        }
    }

    #[test]
    fn portable_request_omits_reasoning_only_messages_that_cannot_be_replayed() {
        let mut request = history(Some(format!("{REASONING_PREFIX}state")));
        let Message::Assistant { content, .. } = &mut request.messages[0] else {
            unreachable!();
        };
        content.truncate(1);
        request
            .messages
            .insert(0, Message::user("Explain this project"));
        request.messages.push(Message::user("Continue"));

        let same_account = portable_request(request.clone(), Provider::ChatGpt);
        assert_eq!(same_account.messages.len(), 3);

        let switched = portable_request(request, Provider::Anthropic);
        assert_eq!(switched.messages.len(), 2);
        assert!(
            switched
                .messages
                .iter()
                .all(|message| matches!(message, Message::User { .. }))
        );
    }

    #[tokio::test]
    async fn runtime_serves_only_the_active_route_and_fails_closed_when_signed_out() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let runtime = runtime_for(&dir, Provider::OpenAi).await;
        assert!(
            runtime.provider(Provider::OpenAi).is_ok(),
            "the active route serves"
        );
        assert!(
            runtime.provider(Provider::Anthropic).is_err(),
            "an inactive route refuses"
        );

        runtime.disconnect();
        assert!(!runtime.is_connected(), "disconnect clears the connection");
        assert!(
            runtime.provider(Provider::OpenAi).is_err(),
            "a signed-out route refuses"
        );
        assert_eq!(
            runtime.report().credential_source(),
            "signed out",
            "the report says the session is signed out"
        );

        runtime.activate(&ready_for(&dir, Provider::Anthropic).await);
        assert!(
            runtime.provider(Provider::Anthropic).is_ok(),
            "the new route serves"
        );
        assert!(
            runtime.provider(Provider::OpenAi).is_err(),
            "the old route refuses"
        );
    }

    #[tokio::test]
    async fn runtime_pauses_after_a_plan_limit_until_activated_again() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let runtime = runtime_for(&dir, Provider::OpenAi).await;
        runtime.observe(&Err::<(), _>(GenerationError::InvalidRequest(
            "other".into(),
        )));
        assert!(
            !runtime.is_usage_paused(),
            "an ordinary error does not pause"
        );

        runtime.observe(&Err::<(), _>(plan_limit()));
        assert!(runtime.is_usage_paused(), "a plan limit pauses requests");
        assert!(runtime.is_connected(), "a pause does not disconnect");
        assert!(
            runtime.provider(Provider::OpenAi).is_err(),
            "paused requests are refused"
        );
        assert!(runtime.take_usage_notice(), "the pause is announced once");
        assert!(!runtime.take_usage_notice(), "the notice is taken");

        runtime.activate(&ready_for(&dir, Provider::OpenAi).await);
        assert!(
            !runtime.is_usage_paused(),
            "activating a connection resumes"
        );
        assert!(
            runtime.provider(Provider::OpenAi).is_ok(),
            "requests flow again"
        );
    }

    #[tokio::test]
    async fn independent_providers_can_share_one_server() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let anthropic = ready_for(&dir, Provider::Anthropic).await;
        let openai = ready_for(&dir, Provider::OpenAi).await;
        let mut server = Server::new();
        server.add_plugins(ModelsPlugin);
        let output_limit = NonZeroU32::new(4096).expect("nonzero output limit");
        let anthropic_runtime = anthropic.install(&mut server, output_limit, PriceStore::builtin());
        let openai_runtime = openai.install_with_options(
            &mut server,
            output_limit,
            PriceStore::builtin(),
            ModelRunOptions::default(),
        );
        server
            .finish()
            .await
            .expect("independent providers start together");

        let registry = server
            .get_global::<ModelRegistry>()
            .expect("the model registry is installed");
        for provider in [Provider::Anthropic, Provider::OpenAi] {
            assert!(
                registry.has_llm_provider(provider.registry_name()),
                "each independently installed provider remains registered"
            );
        }
        anthropic_runtime.disconnect();
        assert!(
            !anthropic_runtime.is_connected() && openai_runtime.is_connected(),
            "the returned handles independently control their installed routes"
        );
        openai_runtime.observe(&Err::<(), _>(plan_limit()));
        assert!(
            openai_runtime.is_usage_paused(),
            "the installed route observes plan limits"
        );
        openai_runtime.activate(&openai);
        assert!(
            !openai_runtime.is_usage_paused(),
            "an embedder can resume an installed route after a usage pause"
        );
    }

    #[tokio::test]
    async fn plugin_filters_foreign_reasoning_on_generate_and_stream() {
        struct Capture(Arc<Mutex<Vec<LlmRequest>>>);
        impl LlmProvider for Capture {
            fn name(&self) -> &'static str {
                "anthropic"
            }
            async fn generate(
                &self,
                _: &str,
                request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                self.0.lock().expect("capture lock").push(request);
                Err(GenerationError::UnsupportedOperation("captured".into()))
            }
            async fn stream(
                &self,
                _: &str,
                request: LlmRequest,
            ) -> Result<LlmStream, GenerationError> {
                self.0.lock().expect("capture lock").push(request);
                Err(GenerationError::UnsupportedOperation("captured".into()))
            }
        }
        let dir = tempfile::tempdir().expect("temporary directory");
        let runtime = runtime_for(&dir, Provider::Anthropic).await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        runtime.active.write().expect("active lock").provider =
            Some(Arc::new(Capture(captured.clone())));
        let connections = ConnectionPlugin {
            runtime,
            options: ModelRunOptions::default(),
            routes: vec![Provider::Anthropic],
        };
        let mut server = Server::new();
        server.add_plugins(ModelsPlugin).add_plugins(connections);
        server.finish().await.expect("the server starts");
        let registry = server
            .get_global::<ModelRegistry>()
            .expect("the model registry is installed");
        assert_eq!(
            registry.llm_provider_names(),
            vec!["anthropic"],
            "a single-route plugin registers one route"
        );
        let llm = registry
            .llm("anthropic/model")
            .expect("the route resolves a model");
        let foreign = || history(Some(format!("{REASONING_PREFIX}foreign")));
        assert!(
            llm.generate(foreign()).await.is_err(),
            "generate reaches the capture"
        );
        assert!(
            llm.stream(foreign()).await.is_err(),
            "stream reaches the capture"
        );

        let captured = captured.lock().expect("capture lock");
        assert_eq!(captured.len(), 2, "both calls are forwarded");
        for request in captured.iter() {
            assert!(
                matches!(assistant_blocks(request), [AssistantBlock::Text(_)]),
                "ChatGPT reasoning never reaches Anthropic"
            );
        }
    }
}
