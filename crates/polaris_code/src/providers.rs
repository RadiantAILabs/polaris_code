//! Which model provider a run talks to, and whether it can start.
//!
//! [`Provider`] is the operator's choice (`--provider`). Everything that varies
//! by provider is answered in this module, so adding one is one variant and one
//! arm in each `match` below rather than an edit scattered through the
//! composition root:
//!
//! | Question | Answered by |
//! |----------|-------------|
//! | Which models can it serve? | [`Provider::catalog`] |
//! | Can it start — credentials, region, endpoint? | [`prepare`], giving a [`ReadyProvider`] or a [`StartupError`] |
//! | Register it with the server | [`ReadyProvider::install`] |
//! | What is it running as? | [`ProviderReport`], one line and a [`GlobalResource`] |
//!
//! The check runs before any front end starts, for the interactive and the
//! non-interactive path alike, and every failure says what to do next. No
//! value it reads or prints is a secret: it reports *where* a credential came
//! from, never the credential.
//!
//! No check here makes a model call. A key that is present but wrong is only
//! found out by the provider, at the first request.
//!
//! # Where a key comes from
//!
//! In this order, the first that exists wins:
//!
//! 1. the provider's environment variable (`ANTHROPIC_API_KEY`,
//!    `OPENAI_API_KEY`), which includes a `.env` file in the launch folder;
//! 2. the key `polaris login` stored ([`crate::credentials`]).
//!
//! Bedrock has no key: it uses the AWS SDK's chain, and `AWS_PROFILE`, when
//! unset, falls back to the profile `polaris login bedrock` stored.
//!
//! # Endpoints
//!
//! `OpenAiPlugin` takes its endpoint from `OPENAI_BASE_URL`, which the
//! underlying client reads when the provider is built, so
//! `--provider openai` reaches any server that speaks the same Responses API
//! (Ollama, LM Studio, vLLM, a gateway). The plugin has no setter of its own.
//! `AnthropicPlugin` has neither a setter nor a variable, so a custom
//! `ANTHROPIC_BASE_URL` is refused rather than ignored: ignoring it would send
//! the key to Anthropic instead of the gateway the operator named.

use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use aws_credential_types::credential_feature::AwsCredentialFeature;
use aws_credential_types::provider::ProvideCredentials;
use clap::ValueEnum;
use polaris::models::openai::OpenAiProvider;
use polaris::models::pricing::PriceStore;
use polaris::models::{AnthropicPlugin, BedrockPlugin, ModelRegistry, OpenAiPlugin};
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin};
use polaris::system::resource::GlobalResource;
use polaris::system::server::Server;
use serde::Serialize;

use crate::config::{
    BUILTIN_MODELS, BUILTIN_OPENAI_MODELS, DEFAULT_BEDROCK_MODEL, DEFAULT_MODEL,
    DEFAULT_OPENAI_MODEL, ModelCatalog,
};
use crate::credentials::StoredCredentials;
use crate::usage::{Price, price_for};

/// The variable `AnthropicPlugin` reads the model key from.
const ANTHROPIC_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// The variable `OpenAiPlugin` reads the model key from.
const OPENAI_KEY_VAR: &str = "OPENAI_API_KEY";

/// The variable the `OpenAI` client reads its endpoint from.
const OPENAI_BASE_URL_VAR: &str = "OPENAI_BASE_URL";

/// The variable other Anthropic tools read a gateway address from. Nothing in
/// this build can honour it, so a value is refused.
const ANTHROPIC_BASE_URL_VAR: &str = "ANTHROPIC_BASE_URL";

/// The reasoning effort (`low`, `medium`, `high`) a Bedrock `OpenAI` model is
/// asked for on every request.
const REASONING_EFFORT_VAR: &str = "POLARIS_CODE_EXP_REASONING_EFFORT";

/// The profile the AWS SDK's shared-config providers read.
const AWS_PROFILE_VAR: &str = "AWS_PROFILE";

/// How long resolving AWS credentials may take before the check gives up. The
/// SDK's own providers have shorter timeouts; this bounds the chain as a whole.
const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(20);

/// A source of named values, so a check can be run against a stand-in for the
/// process environment.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment as a [`Lookup`].
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The value, unless it is absent or only whitespace.
fn present(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Model provider a run can select, via `--provider`.
///
/// Adding a provider means adding a variant here and an arm to each `match`
/// in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Provider {
    /// The Anthropic API, with `ANTHROPIC_API_KEY`.
    Anthropic,
    /// The `OpenAI` Responses API, or a server that speaks it, with
    /// `OPENAI_API_KEY` and optionally `OPENAI_BASE_URL`.
    #[value(name = "openai")]
    OpenAi,
    /// AWS Bedrock, through the AWS SDK's default credential chain.
    Bedrock,
}

impl Provider {
    /// Every provider, in the order listings show them.
    pub const ALL: [Self; 3] = [Self::Anthropic, Self::OpenAi, Self::Bedrock];

    /// The name `--provider` takes.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Bedrock => "bedrock",
        }
    }

    /// The provider's display name for account and failure messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::Bedrock => "Amazon Bedrock",
        }
    }

    /// The provider serving a fully qualified model id, if known.
    #[must_use]
    pub fn of_model(model_id: &str) -> Option<Self> {
        let (prefix, _) = model_id.split_once('/')?;
        Self::ALL
            .into_iter()
            .find(|provider| provider.registry_name() == prefix)
    }

    /// What this provider authenticates with, for user-facing diagnostics.
    #[must_use]
    pub const fn credential(self) -> &'static str {
        match self.key_var() {
            Some(_) => "API key",
            None => "AWS credentials",
        }
    }

    /// What to check after this provider rejects authentication.
    #[must_use]
    pub fn credential_hint(self) -> String {
        match self.key_var() {
            Some(var) => format!(
                "Check the key saved by `polaris login {}` or supplied through {var}.",
                self.flag()
            ),
            None => "Check your AWS credentials and the model access granted to them.".to_string(),
        }
    }

    /// What to check when this provider cannot be reached.
    #[must_use]
    pub const fn network_hint(self) -> &'static str {
        match self {
            Self::Bedrock => "Check your AWS credentials, region and network connection.",
            _ => "Check your network connection and proxy settings.",
        }
    }

    /// The prefix this provider's models carry in a `provider/model` id: the
    /// name its `LlmProvider` registers under.
    #[must_use]
    pub const fn registry_name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Bedrock => "aws.bedrock",
        }
    }

    /// The environment variable holding this provider's key, or `None` when it
    /// authenticates some other way.
    #[must_use]
    pub const fn key_var(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some(ANTHROPIC_KEY_VAR),
            Self::OpenAi => Some(OPENAI_KEY_VAR),
            Self::Bedrock => None,
        }
    }

    /// The models a run on this provider may select between, and the one it
    /// starts on: the provider's own defaults, overridden by
    /// `POLARIS_CODE_MODEL` / `POLARIS_CODE_MODELS` read through `env`.
    ///
    /// Bedrock has no built-in set: regional inference-profile ids vary by
    /// account and region (see [`DEFAULT_BEDROCK_MODEL`]).
    #[must_use]
    pub fn catalog(self, env: Lookup<'_>) -> ModelCatalog {
        match self {
            Self::Anthropic => ModelCatalog::from_lookup(DEFAULT_MODEL, BUILTIN_MODELS, env),
            Self::OpenAi => {
                ModelCatalog::from_lookup(DEFAULT_OPENAI_MODEL, BUILTIN_OPENAI_MODELS, env)
            }
            Self::Bedrock => ModelCatalog::from_lookup(DEFAULT_BEDROCK_MODEL, &[], env),
        }
    }
}

/// Why a provider cannot start, with what to do about it.
///
/// [`Display`](fmt::Display) renders the whole message, ready to print once
/// before exiting: it names the missing piece and the ways to supply it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StartupError {
    /// The provider's key variable is unset or blank.
    MissingKey {
        /// The provider that needs it.
        provider: Provider,
        /// The variable that was read.
        var: &'static str,
    },
    /// `OPENAI_BASE_URL` is set but is not an `http` or `https` address.
    BadBaseUrl {
        /// The variable that was read.
        var: &'static str,
        /// What it held, with any credentials in it removed.
        value: String,
    },
    /// `ANTHROPIC_BASE_URL` is set, and this build cannot send Anthropic
    /// requests anywhere else.
    UnsupportedBaseUrl {
        /// The variable that was read.
        var: &'static str,
        /// What it held, with any credentials in it removed.
        value: String,
    },
    /// Neither `--region` nor the AWS configuration names a region.
    NoRegion,
    /// The AWS credential chain produced no credentials.
    NoCredentials {
        /// What the SDK said, for the operator to act on.
        detail: String,
        /// The SDK or timeout error, when a lookup was attempted.
        source: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
    },
    /// The credentials file `polaris login` writes exists but cannot be used.
    StoredCredentials {
        /// Why, and how to fix it, retaining the original safe diagnostic.
        source: std::sync::Arc<crate::credentials::CredentialError>,
    },
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingKey { provider, var } => {
                let flag = provider.flag();
                let (name, where_to_get_one, example, more) = match provider {
                    Provider::Anthropic => (
                        "Anthropic",
                        "https://console.anthropic.com/settings/keys",
                        "sk-ant-...",
                        String::new(),
                    ),
                    Provider::OpenAi => (
                        "OpenAI",
                        "https://platform.openai.com/api-keys",
                        "sk-...",
                        format!(
                            "For an OpenAI-compatible server (Ollama, LM Studio, vLLM, a \
                             gateway), also set {OPENAI_BASE_URL_VAR} to its address, for \
                             example http://localhost:11434/v1. A server that checks no key \
                             accepts any non-empty {var}.\n\n"
                        ),
                    ),
                    Provider::Bedrock => ("AWS", "", "", String::new()),
                };
                write!(
                    f,
                    "{var} is not set and no key is stored, so polaris cannot reach {name}.\n\n\
                     Save a key (create one at {where_to_get_one}):\n\n    \
                     polaris login {flag}\n\n\
                     or set it for this shell:\n\n    export {var}={example}\n\n\
                     {more}\
                     You can also put that line in a .env file in the folder you run polaris \
                     from.\n\
                     To use another provider, pass --provider {} (set POLARIS_CODE_PROVIDER, or \
                     `provider` in the config file, to make it the default).\n\
                     Run `polaris --help` for all options.",
                    other_providers(*provider)
                )
            }
            Self::BadBaseUrl { var, value } => write!(
                f,
                "{var} is set to `{value}`, which is not an http or https address.\n\
                 Set it to the server's address, for example http://localhost:11434/v1, \
                 or unset it to use api.openai.com."
            ),
            Self::UnsupportedBaseUrl { var, value } => write!(
                f,
                "{var} is set to `{value}`, but this build of polaris cannot send Anthropic \
                 requests to another address, and would send your key to api.anthropic.com \
                 instead.\n\
                 Unset {var} to use Anthropic directly, or use --provider openai with an \
                 OpenAI-compatible gateway (set OPENAI_BASE_URL)."
            ),
            Self::NoRegion => write!(
                f,
                "No AWS region is configured for --provider bedrock.\n\
                 Pass --region <region> (for example --region us-east-1), set AWS_REGION, add \
                 a region to your AWS profile, or run `polaris login bedrock`."
            ),
            Self::NoCredentials { detail, .. } => write!(
                f,
                "No AWS credentials were found for --provider bedrock ({detail}).\n\
                 The AWS SDK looked at environment variables, the shared profile (AWS_PROFILE), \
                 SSO, container credentials and the EC2 instance role.\n\
                 Set AWS_PROFILE, run `aws sso login`, or export AWS_ACCESS_KEY_ID and \
                 AWS_SECRET_ACCESS_KEY; `polaris login bedrock` checks them and saves your \
                 region and profile."
            ),
            Self::StoredCredentials { source } => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StoredCredentials { source } => Some(source.as_ref()),
            Self::NoCredentials { source, .. } => source.as_deref().map(|source| source as _),
            _ => None,
        }
    }
}

/// The other providers' flag spellings, as `a or b`.
fn other_providers(provider: Provider) -> String {
    Provider::ALL
        .into_iter()
        .filter(|other| *other != provider)
        .map(Provider::flag)
        .collect::<Vec<_>>()
        .join(" or ")
}

/// What a provider that passed its check is running as.
///
/// One line for the person starting a session ([`Self::line`]) and a resource
/// other parts of the host can read — a status command, for one — without
/// asking the provider again. It carries no secret: a credential is named by
/// the variable or AWS source it came from, an endpoint has any user name,
/// password or query removed.
///
/// # Scope
///
/// Global, because one provider serves the whole process: every session the
/// host opens runs on the provider that passed the check at launch.
///
/// # Provided By
///
/// The binary's composition root, from [`ReadyProvider::report`], before the
/// server is finished. An embedder that builds its own server installs none,
/// so a system reading it must tolerate its absence.
///
/// # Access
///
/// `Res<ProviderReport>` only. It is a value fixed at launch with no interior
/// mutability.
///
/// # Alternatives
///
/// _none_ — the model registry says which providers are registered, not which
/// credentials they were started with.
///
/// # Example
///
/// ```
/// use polaris::system::param::Res;
/// use polaris::system::system;
/// use polaris_code::providers::ProviderReport;
///
/// #[system]
/// async fn log_provider(report: Res<ProviderReport>) {
///     tracing::info!("running on {}", report.line());
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReport {
    provider: Provider,
    region: Option<String>,
    endpoint: Option<String>,
    credential_source: String,
}

impl GlobalResource for ProviderReport {}

impl ProviderReport {
    /// A report for `provider`, as a host that checks credentials itself would
    /// build one. `credential_source` names where they came from, never a
    /// credential.
    #[must_use]
    pub fn new(
        provider: Provider,
        region: Option<String>,
        endpoint: Option<String>,
        credential_source: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            region,
            endpoint,
            credential_source: credential_source.into(),
        }
    }

    /// The provider this run is using.
    #[must_use]
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// The AWS region, for a provider that has one.
    #[must_use]
    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// The endpoint, when the operator pointed the provider somewhere other
    /// than its default. Any user name or password in it is removed.
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Where the credentials came from: a variable name, or the AWS source
    /// that answered. Never the credential itself.
    #[must_use]
    pub fn credential_source(&self) -> &str {
        &self.credential_source
    }

    /// The report as one line, for example
    /// `bedrock · region us-east-1 · credentials from EC2 instance role`.
    #[must_use]
    pub fn line(&self) -> String {
        let mut parts = vec![self.provider.flag().to_string()];
        if let Some(region) = &self.region {
            parts.push(format!("region {region}"));
        }
        if let Some(endpoint) = &self.endpoint {
            parts.push(format!("endpoint {endpoint}"));
        }
        parts.push(format!("credentials from {}", self.credential_source));
        parts.join(" · ")
    }
}

/// A provider that passed its check and can be registered.
#[derive(Clone)]
pub struct ReadyProvider {
    report: ProviderReport,
    sdk_config: Option<aws_config::SdkConfig>,
    reasoning_fields: Option<serde_json::Value>,
    /// The key `polaris login` stored, when no environment variable held one.
    stored_key: Option<String>,
}

// By hand, so what a debug print shows is the report and never a stored key or
// whatever the SDK config's credential provider chooses to print about itself.
impl fmt::Debug for ReadyProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyProvider")
            .field("report", &self.report)
            .field("reasoning_fields", &self.reasoning_fields)
            .finish_non_exhaustive()
    }
}

impl ReadyProvider {
    /// What the provider is running as.
    #[must_use]
    pub fn report(&self) -> &ProviderReport {
        &self.report
    }

    /// Register the provider's plugin with `server`.
    ///
    /// `max_output_tokens` is the per-request output limit for the providers
    /// that take one at registration — Anthropic and Bedrock. `OpenAiPlugin`
    /// has no such setting, so it is not applied there.
    ///
    /// # Panics
    ///
    /// Panics if an internal invariant is broken and a prepared Bedrock
    /// provider has no checked SDK configuration.
    pub fn install(&self, server: &mut Server, max_output_tokens: NonZeroU32, pricing: PriceStore) {
        match (self.report.provider, &self.stored_key) {
            (Provider::Anthropic, None) => {
                server.add_plugins(
                    AnthropicPlugin::from_env(ANTHROPIC_KEY_VAR)
                        .with_max_tokens(max_output_tokens)
                        .with_pricing_store(pricing),
                );
            }
            (Provider::Anthropic, Some(key)) => {
                server.add_plugins(
                    AnthropicPlugin::new(key.clone())
                        .with_max_tokens(max_output_tokens)
                        .with_pricing_store(pricing),
                );
            }
            (Provider::OpenAi, None) => {
                server.add_plugins(
                    OpenAiPlugin::from_env(OPENAI_KEY_VAR).with_pricing_store(pricing),
                );
            }
            (Provider::OpenAi, Some(key)) => {
                server.add_plugins(StoredKeyOpenAiPlugin::new(key.clone(), pricing));
            }
            (Provider::Bedrock, _) => {
                // The SDK config was loaded and checked in `prepare`; handing
                // it over means the plugin does not load it a second time.
                let sdk_config = self
                    .sdk_config
                    .clone()
                    .expect("a Bedrock provider is prepared with its SDK config");
                let plugin = BedrockPlugin::from_sdk_config(sdk_config)
                    .with_max_tokens(max_output_tokens)
                    .with_pricing_store(pricing);
                server.add_plugins(match self.reasoning_fields.clone() {
                    Some(fields) => plugin.with_additional_model_request_fields(fields),
                    None => plugin,
                });
            }
        }
    }
}

/// Check that `provider` can start, reading credentials through `env` and,
/// when the environment has none, from `stored`.
///
/// `region` is `--region`; only Bedrock uses it. The same function serves the
/// interactive and the non-interactive front end, so both refuse to start on
/// the same grounds and say the same thing.
///
/// # Errors
///
/// [`StartupError`] naming what is missing and how to supply it.
pub async fn prepare(
    provider: Provider,
    region: Option<&str>,
    env: Lookup<'_>,
    stored: &dyn StoredCredentials,
) -> Result<ReadyProvider, StartupError> {
    match provider {
        Provider::Anthropic => {
            let origin = resolve_key(provider, env, stored)?;
            if let Some(url) = present(env(ANTHROPIC_BASE_URL_VAR)) {
                return Err(StartupError::UnsupportedBaseUrl {
                    var: ANTHROPIC_BASE_URL_VAR,
                    value: redact_url(&url),
                });
            }
            Ok(ready_with_key(provider, origin, None))
        }
        Provider::OpenAi => {
            let origin = resolve_key(provider, env, stored)?;
            let endpoint = match present(env(OPENAI_BASE_URL_VAR)) {
                Some(url) if is_http_url(&url) => Some(redact_url(&url)),
                Some(url) => {
                    return Err(StartupError::BadBaseUrl {
                        var: OPENAI_BASE_URL_VAR,
                        value: redact_url(&url),
                    });
                }
                None => None,
            };
            Ok(ready_with_key(provider, origin, endpoint))
        }
        Provider::Bedrock => {
            // `AWS_PROFILE` wins, as it does for every AWS tool; the stored
            // profile only fills the gap when it is unset.
            let stored_profile = match present(env(AWS_PROFILE_VAR)) {
                Some(_) => None,
                None => stored
                    .aws_profile()
                    .map_err(|err| StartupError::StoredCredentials {
                        source: std::sync::Arc::new(err),
                    })?,
            };
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
            if let Some(region) = present(region.map(str::to_string)) {
                loader = loader.region(aws_config::Region::new(region));
            }
            if let Some(profile) = &stored_profile {
                loader = loader.profile_name(profile);
            }
            check_bedrock(
                loader.load().await,
                stored_profile.as_deref(),
                env,
                CREDENTIAL_TIMEOUT,
            )
            .await
        }
    }
}

/// Where a provider's key was found.
enum KeyOrigin {
    /// In this environment variable, `.env` included.
    Variable(&'static str),
    /// In the credentials file, because no variable held one.
    Stored(String),
}

/// What the report says for a key `polaris login` stored.
const STORED_KEY_SOURCE: &str = "stored key (polaris login)";

/// Find `provider`'s key: its variable first, then the credentials file.
fn resolve_key(
    provider: Provider,
    env: Lookup<'_>,
    stored: &dyn StoredCredentials,
) -> Result<KeyOrigin, StartupError> {
    let var = provider
        .key_var()
        .expect("this provider authenticates with a key");
    if present(env(var)).is_some() {
        return Ok(KeyOrigin::Variable(var));
    }
    match stored.api_key(provider) {
        Ok(Some(key)) => Ok(KeyOrigin::Stored(key)),
        Ok(None) => Err(StartupError::MissingKey { provider, var }),
        Err(err) => Err(StartupError::StoredCredentials {
            source: std::sync::Arc::new(err),
        }),
    }
}

fn ready_with_key(
    provider: Provider,
    origin: KeyOrigin,
    endpoint: Option<String>,
) -> ReadyProvider {
    let (credential_source, stored_key) = match origin {
        KeyOrigin::Variable(var) => (var.to_string(), None),
        KeyOrigin::Stored(key) => (STORED_KEY_SOURCE.to_string(), Some(key)),
    };
    ReadyProvider {
        report: ProviderReport {
            provider,
            region: None,
            endpoint,
            credential_source,
        },
        sdk_config: None,
        reasoning_fields: None,
        stored_key,
    }
}

/// Registers the `OpenAI` provider with a key that `polaris login` stored —
/// add it instead of [`OpenAiPlugin`] when the key is not in the environment.
///
/// [`OpenAiPlugin`] can only read its key from an environment variable, and this
/// process never writes one. This plugin does what that plugin's `build` does
/// — register an [`OpenAiProvider`] under `openai/<model>` — with the key it
/// was given, using only the framework's public provider type and registry.
/// The provider reads `OPENAI_BASE_URL` itself, exactly as it does under
/// `OpenAiPlugin`.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Registers a provider with the [`ModelRegistry`] that [`ModelsPlugin`](polaris::models::ModelsPlugin) owns. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Contributes a provider to [`ModelRegistry`] rather than publishing an API. |
///
/// # Dependencies
///
/// [`ModelsPlugin`](polaris::models::ModelsPlugin), which owns the [`ModelRegistry`] this plugin extends. Do not
/// add it together with [`OpenAiPlugin`]: both register the `openai` provider.
///
/// # Example
///
/// ```no_run
/// # use polaris::models::ModelsPlugin;
/// # use polaris::models::pricing::PriceStore;
/// # use polaris::system::server::Server;
/// # use polaris_code::providers::StoredKeyOpenAiPlugin;
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(StoredKeyOpenAiPlugin::new("sk-...", PriceStore::builtin()));
/// ```
pub struct StoredKeyOpenAiPlugin {
    key: String,
    pricing: PriceStore,
}

impl StoredKeyOpenAiPlugin {
    /// A plugin that registers the provider with `key`, pricing from `pricing`.
    #[must_use]
    pub fn new(key: impl Into<String>, pricing: PriceStore) -> Self {
        Self {
            key: key.into(),
            pricing,
        }
    }
}

// By hand, so a debug print never shows the key.
impl fmt::Debug for StoredKeyOpenAiPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredKeyOpenAiPlugin")
            .field("key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[plugin(id = "polaris_code::openai_stored_key", version = "0.1.0")]
impl Plugin for StoredKeyOpenAiPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        registry.register_llm_provider(
            OpenAiProvider::new(self.key.clone()).with_pricing_store(self.pricing.clone()),
        );
    }
}

/// Resolve the region and one credential from `sdk_config`, the config the
/// Bedrock provider will itself be built from, and report where they came
/// from.
///
/// The credential is fetched and dropped: its only use here is to prove the
/// chain produces one, and to read which source answered.
async fn check_bedrock(
    sdk_config: aws_config::SdkConfig,
    stored_profile: Option<&str>,
    env: Lookup<'_>,
    timeout: Duration,
) -> Result<ReadyProvider, StartupError> {
    let region = sdk_config
        .region()
        .map(|region| region.as_ref().to_string())
        .ok_or(StartupError::NoRegion)?;
    let Some(credentials) = sdk_config.credentials_provider() else {
        return Err(StartupError::NoCredentials {
            detail: "no credential provider is configured".to_string(),
            source: None,
        });
    };
    let credentials = match tokio::time::timeout(timeout, credentials.provide_credentials()).await {
        Ok(Ok(credentials)) => credentials,
        Ok(Err(err)) => {
            return Err(StartupError::NoCredentials {
                detail: error_chain(&err),
                source: Some(std::sync::Arc::new(err)),
            });
        }
        Err(err) => {
            return Err(StartupError::NoCredentials {
                detail: format!("lookup timed out after {}s", timeout.as_secs()),
                source: Some(std::sync::Arc::new(err)),
            });
        }
    };
    let features = credentials
        .get_property::<Vec<AwsCredentialFeature>>()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let profile = present(env(AWS_PROFILE_VAR)).or_else(|| stored_profile.map(str::to_string));
    Ok(ReadyProvider {
        report: ProviderReport {
            provider: Provider::Bedrock,
            region: Some(region),
            endpoint: None,
            credential_source: credential_source(features, profile.as_deref()),
        },
        sdk_config: Some(sdk_config),
        reasoning_fields: reasoning_effort_fields(env(REASONING_EFFORT_VAR).as_deref()),
        stored_key: None,
    })
}

/// An error and its causes as one line. The SDK's credential errors put what
/// went wrong in the cause, and their own text only says which kind it was.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut cause = err.source();
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    text
}

/// Name the source of a credential from the features the SDK tagged it with,
/// innermost source first (an SSO profile is tagged SSO, then profile).
fn credential_source(features: &[AwsCredentialFeature], profile: Option<&str>) -> String {
    use AwsCredentialFeature as Feature;
    let profile_suffix = profile.map_or_else(String::new, |name| format!(" (profile `{name}`)"));
    features
        .iter()
        .find_map(|feature| {
            Some(match feature {
                Feature::CredentialsEnvVars => {
                    "environment variables (AWS_ACCESS_KEY_ID)".to_string()
                }
                Feature::CredentialsSso | Feature::CredentialsProfileSso => {
                    format!("AWS SSO{profile_suffix}")
                }
                Feature::CredentialsImds => "EC2 instance role".to_string(),
                Feature::CredentialsHttp => "container credentials endpoint".to_string(),
                Feature::CredentialsProcess | Feature::CredentialsProfileProcess => {
                    format!("a credential process{profile_suffix}")
                }
                Feature::CredentialsEnvVarsStsWebIdToken
                | Feature::CredentialsStsAssumeRoleWebId
                | Feature::CredentialsProfileStsWebIdToken => "a web identity token".to_string(),
                Feature::CredentialsStsAssumeRole => format!("an assumed role{profile_suffix}"),
                Feature::CredentialsProfile
                | Feature::CredentialsProfileSourceProfile
                | Feature::CredentialsProfileNamedProvider => {
                    format!(
                        "shared AWS profile{}",
                        profile
                            .map_or_else(|| " (default)".to_string(), |name| format!(" `{name}`"),)
                    )
                }
                Feature::CredentialsCode => "the AWS SDK configuration".to_string(),
                _ => return None,
            })
        })
        .unwrap_or_else(|| "the AWS default credential chain".to_string())
}

/// The Converse request fields for a reasoning effort, or `None` when unset,
/// blank, or not one of `low`/`medium`/`high` (the model's default applies).
fn reasoning_effort_fields(value: Option<&str>) -> Option<serde_json::Value> {
    let effort = value?.trim();
    matches!(effort, "low" | "medium" | "high")
        .then(|| serde_json::json!({"reasoning": {"effort": effort}}))
}

/// Whether `url` is an `http` or `https` address with a host.
fn is_http_url(url: &str) -> bool {
    let shown = redact_url(url);
    shown.split_once("://").is_some_and(|(scheme, rest)| {
        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            && !rest.is_empty()
            && !rest.starts_with('/')
    })
}

/// `url` without a user name, password, query or fragment, so it can be shown.
fn redact_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority.rsplit('@').next().unwrap_or_default();
    if scheme.is_empty() {
        format!("{host}{path}")
    } else {
        format!("{scheme}://{host}{path}")
    }
}

/// Whether the provider's credentials are known to be present, for a listing
/// that must not start anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CredentialStatus {
    /// The key variable holds a value.
    Found {
        /// The variable.
        source: &'static str,
    },
    /// The key variable is unset or blank.
    Missing {
        /// The variable to set.
        variable: &'static str,
    },
    /// Resolving them means running the credential chain, which a listing
    /// does not do.
    Unchecked {
        /// Why, and when the check happens instead.
        note: &'static str,
    },
}

/// A model one provider can serve, as a listing shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListedModel {
    /// The `provider/model` id `--model` takes.
    pub id: String,
    /// Whether a run starts on this model when it names none.
    pub default: bool,
    /// List price per million tokens from the built-in table, or `None` when
    /// it has no row — a spend ceiling in dollars cannot be enforced then.
    pub price: Option<Price>,
}

/// One provider's models and whether it could start.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderListing {
    /// The name `--provider` takes.
    pub provider: Provider,
    /// Whether its credentials are present.
    pub credentials: CredentialStatus,
    /// Its selectable models, the default first.
    pub models: Vec<ListedModel>,
}

/// Every provider's selectable models, for `--list-models`.
///
/// Derived, not authored: the sets come from [`Provider::catalog`], so a
/// `POLARIS_CODE_MODELS` declaration shows here exactly as a run would see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelListing {
    /// One entry per provider, in [`Provider::ALL`] order.
    pub providers: Vec<ProviderListing>,
}

impl ModelListing {
    /// Read the listing through `env` and `stored`, pricing models from
    /// `prices`. Starts nothing and needs no credential.
    #[must_use]
    pub fn observe(env: Lookup<'_>, stored: &dyn StoredCredentials, prices: &PriceStore) -> Self {
        let providers = Provider::ALL
            .into_iter()
            .map(|provider| {
                let catalog = provider
                    .catalog(env)
                    .restricted_to_providers(&[provider.registry_name()]);
                let credentials = match provider.key_var() {
                    Some(var) if present(env(var)).is_some() => {
                        CredentialStatus::Found { source: var }
                    }
                    Some(_) if matches!(stored.api_key(provider), Ok(Some(_))) => {
                        CredentialStatus::Found {
                            source: STORED_KEY_SOURCE,
                        }
                    }
                    Some(variable) => CredentialStatus::Missing { variable },
                    None => CredentialStatus::Unchecked {
                        note: "resolved at startup from the AWS default credential chain",
                    },
                };
                let models = catalog
                    .models()
                    .iter()
                    .map(|id| ListedModel {
                        id: id.clone(),
                        default: id == catalog.default_model(),
                        price: price_for(prices, id),
                    })
                    .collect();
                ProviderListing {
                    provider,
                    credentials,
                    models,
                }
            })
            .collect();
        Self { providers }
    }

    /// The listing as text: a block per provider, one model per line.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for listing in &self.providers {
            let credentials = match &listing.credentials {
                CredentialStatus::Found { source } => format!("found ({source})"),
                CredentialStatus::Missing { variable } => format!("missing (set {variable})"),
                CredentialStatus::Unchecked { note } => (*note).to_string(),
            };
            out.push_str(&format!(
                "--provider {} — credentials: {credentials}\n",
                listing.provider.flag()
            ));
            if listing.models.is_empty() {
                out.push_str("  (no models declared; set POLARIS_CODE_MODELS)\n");
            }
            for model in &listing.models {
                let price = model.price.map_or_else(
                    || "price unknown".to_string(),
                    |price| {
                        format!(
                            "${:.2} in / ${:.2} out per MTok",
                            price.input_per_mtok, price.output_per_mtok
                        )
                    },
                );
                let marker = if model.default { "  (default)" } else { "" };
                out.push_str(&format!("  {:<44} {price}{marker}\n", model.id));
            }
            out.push('\n');
        }
        out.push_str(
            "Pass --model <id>, or a bare name such as `sonnet` for the newest match. \
             POLARIS_CODE_MODELS declares other models.\n\
             Prices are the built-in table; a run may use newer ones.",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use aws_credential_types::Credentials;
    use aws_credential_types::provider::error::CredentialsError;
    use aws_credential_types::provider::{SharedCredentialsProvider, future};

    use super::*;
    use crate::credentials::CredentialStore;

    /// What a run sees when nothing was saved with `polaris login`.
    const NOTHING_STORED: Option<CredentialStore> = None;

    fn env_of<const N: usize>(
        pairs: [(&'static str, &'static str); N],
    ) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<&str, &str> = pairs.into_iter().collect();
        move |name| map.get(name).map(|value| (*value).to_string())
    }

    async fn prepare_with(
        provider: Provider,
        pairs: &[(&'static str, &'static str)],
    ) -> Result<ReadyProvider, StartupError> {
        prepare_stored(provider, pairs, &NOTHING_STORED).await
    }

    async fn prepare_stored(
        provider: Provider,
        pairs: &[(&'static str, &'static str)],
        stored: &dyn StoredCredentials,
    ) -> Result<ReadyProvider, StartupError> {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        let env = move |name: &str| map.get(name).map(|value| (*value).to_string());
        prepare(provider, None, &env, stored).await
    }

    /// A credentials file in a scratch directory, holding what `entries` say.
    fn stored_in(
        dir: &tempfile::TempDir,
        keys: &[(Provider, &str)],
        profile: Option<&str>,
    ) -> CredentialStore {
        let store = CredentialStore::at(dir.path().join(crate::credentials::CREDENTIALS_FILE));
        for (provider, key) in keys {
            store.set_api_key(*provider, key).unwrap();
        }
        if let Some(profile) = profile {
            store.set_aws_profile(profile).unwrap();
        }
        store
    }

    #[test]
    fn the_flag_names_are_the_ones_clap_accepts() {
        for provider in Provider::ALL {
            let possible = provider
                .to_possible_value()
                .expect("every variant is selectable");
            assert_eq!(possible.get_name(), provider.flag());
            assert_eq!(Provider::from_str(provider.flag(), false), Ok(provider));
        }
        assert_eq!(Provider::from_str("openai", false), Ok(Provider::OpenAi));
        assert!(Provider::from_str("open-ai", false).is_err());
    }

    #[test]
    fn each_provider_owns_a_registry_name_and_a_catalog_of_its_own_models() {
        let none = |_: &str| None;
        for provider in Provider::ALL {
            let catalog = provider.catalog(&none);
            let reachable = catalog.restricted_to_providers(&[provider.registry_name()]);
            assert_eq!(
                reachable.select(None).as_deref(),
                Ok(catalog.default_model()),
                "{}: its default is one of its own models",
                provider.flag()
            );
        }
        assert_eq!(
            Provider::OpenAi.catalog(&none).default_model(),
            "openai/gpt-5.5"
        );
        assert_eq!(
            Provider::Anthropic.catalog(&none).default_model(),
            "anthropic/claude-sonnet-5-5"
        );
        assert_eq!(
            Provider::Bedrock.catalog(&none).default_model(),
            DEFAULT_BEDROCK_MODEL
        );
    }

    /// The declared set is one list for the whole run; each provider keeps the
    /// ids that carry its own prefix, and its own default stays selectable.
    #[test]
    fn a_declared_set_applies_to_whichever_provider_is_chosen() {
        let env = env_of([(
            "POLARIS_CODE_MODELS",
            "openai/llama3, anthropic/claude-opus-5-5",
        )]);
        let openai = Provider::OpenAi
            .catalog(&env)
            .restricted_to_providers(&["openai"]);
        assert_eq!(openai.models(), ["openai/gpt-5.5", "openai/llama3"]);
        let anthropic = Provider::Anthropic
            .catalog(&env)
            .restricted_to_providers(&["anthropic"]);
        assert_eq!(
            anthropic.models(),
            ["anthropic/claude-sonnet-5-5", "anthropic/claude-opus-5-5"]
        );
    }

    /// Pointing `OpenAI` at a local server takes both variables: the declared
    /// set names what the server has, and the default names what to start on.
    #[test]
    fn a_local_server_is_reached_by_declaring_its_models_and_a_default() {
        let env = env_of([
            ("POLARIS_CODE_MODEL", "openai/qwen3"),
            ("POLARIS_CODE_MODELS", "openai/qwen3, openai/llama3"),
        ]);
        let catalog = Provider::OpenAi
            .catalog(&env)
            .restricted_to_providers(&["openai"]);
        assert_eq!(catalog.select(None).unwrap(), "openai/qwen3");
        assert_eq!(catalog.resolve("llama3").unwrap(), "openai/llama3");
    }

    #[tokio::test]
    async fn a_missing_or_blank_key_stops_the_provider_and_says_what_to_do() {
        for (provider, var) in [
            (Provider::Anthropic, "ANTHROPIC_API_KEY"),
            (Provider::OpenAi, "OPENAI_API_KEY"),
        ] {
            for pairs in [vec![], vec![(var, "")], vec![(var, "   ")]] {
                let err = prepare_with(provider, &pairs)
                    .await
                    .expect_err("no key, no start");
                assert!(
                    matches!(err, StartupError::MissingKey { provider: actual_provider, var: actual_var } if actual_provider == provider && actual_var == var)
                );
                let message = err.to_string();
                assert!(message.contains(&format!("{var} is not set")), "{message}");
                assert!(message.contains("export"), "{message}");
                assert!(message.contains(".env"), "{message}");
                assert!(message.contains("--provider"), "{message}");
                assert!(message.contains("POLARIS_CODE_PROVIDER"), "{message}");
            }
        }
    }

    #[tokio::test]
    async fn the_missing_key_message_points_at_where_to_get_one_and_at_the_alternatives() {
        let anthropic = StartupError::MissingKey {
            provider: Provider::Anthropic,
            var: "ANTHROPIC_API_KEY",
        }
        .to_string();
        assert!(anthropic.contains("console.anthropic.com"), "{anthropic}");
        assert!(
            anthropic.contains("--provider openai or bedrock"),
            "{anthropic}"
        );

        let openai = StartupError::MissingKey {
            provider: Provider::OpenAi,
            var: "OPENAI_API_KEY",
        }
        .to_string();
        assert!(openai.contains("platform.openai.com"), "{openai}");
        assert!(openai.contains("OPENAI_BASE_URL"), "{openai}");
        assert!(
            openai.contains("--provider anthropic or bedrock"),
            "{openai}"
        );
    }

    #[tokio::test]
    async fn a_key_is_reported_by_variable_name_and_never_by_value() {
        let secret = "sk-ant-SECRET-VALUE";
        let ready = prepare_with(Provider::Anthropic, &[("ANTHROPIC_API_KEY", secret)])
            .await
            .expect("a key is enough");
        let line = ready.report().line();
        assert_eq!(line, "anthropic · credentials from ANTHROPIC_API_KEY");
        assert!(!format!("{ready:?}").contains(secret));
    }

    #[tokio::test]
    async fn openai_reports_a_custom_endpoint_without_credentials_in_it() {
        let ready = prepare_with(
            Provider::OpenAi,
            &[
                ("OPENAI_API_KEY", "local"),
                (
                    "OPENAI_BASE_URL",
                    "http://user:hunter2@localhost:11434/v1?token=abc#frag",
                ),
            ],
        )
        .await
        .expect("a key and a base url are enough");
        assert_eq!(
            ready.report().line(),
            "openai · endpoint http://localhost:11434/v1 · credentials from OPENAI_API_KEY"
        );
        assert_eq!(ready.report().endpoint(), Some("http://localhost:11434/v1"));
        let rendered = format!("{ready:?}");
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("token=abc"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn openai_without_a_base_url_names_no_endpoint() {
        let ready = prepare_with(Provider::OpenAi, &[("OPENAI_API_KEY", "sk-x")])
            .await
            .unwrap();
        assert_eq!(ready.report().endpoint(), None);
        assert_eq!(
            ready.report().line(),
            "openai · credentials from OPENAI_API_KEY"
        );
    }

    #[tokio::test]
    async fn a_base_url_that_is_not_http_is_refused_without_echoing_credentials() {
        let err = prepare_with(
            Provider::OpenAi,
            &[
                ("OPENAI_API_KEY", "k"),
                ("OPENAI_BASE_URL", "ftp://user:hunter2@host/v1"),
            ],
        )
        .await
        .expect_err("only http and https are addresses");
        let message = err.to_string();
        assert!(message.contains("OPENAI_BASE_URL"), "{message}");
        assert!(message.contains("ftp://host/v1"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");

        for bad in ["localhost:11434", "http://", "https:///"] {
            assert!(
                prepare_with(
                    Provider::OpenAi,
                    &[("OPENAI_API_KEY", "k"), ("OPENAI_BASE_URL", bad)]
                )
                .await
                .is_err(),
                "{bad}"
            );
        }
    }

    /// A gateway address nothing can honour must stop the run, not be skipped:
    /// skipping it would send the key to a different party than the one named.
    #[tokio::test]
    async fn an_anthropic_base_url_is_refused_because_it_cannot_be_honoured() {
        let err = prepare_with(
            Provider::Anthropic,
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-x"),
                (
                    "ANTHROPIC_BASE_URL",
                    "https://user:pw@gateway.example.com/anthropic",
                ),
            ],
        )
        .await
        .expect_err("the endpoint cannot be used");
        let message = err.to_string();
        assert!(message.contains("ANTHROPIC_BASE_URL"), "{message}");
        assert!(
            message.contains("https://gateway.example.com/anthropic"),
            "{message}"
        );
        assert!(!message.contains("pw@"), "{message}");
        assert!(message.contains("api.anthropic.com"), "{message}");
        assert!(message.contains("--provider openai"), "{message}");
    }

    #[tokio::test]
    async fn a_blank_anthropic_base_url_is_not_a_gateway() {
        prepare_with(
            Provider::Anthropic,
            &[("ANTHROPIC_API_KEY", "k"), ("ANTHROPIC_BASE_URL", "  ")],
        )
        .await
        .expect("a blank value names nothing");
    }

    #[test]
    fn urls_are_shown_without_userinfo_query_or_fragment() {
        for (raw, shown) in [
            ("https://api.example.com/v1", "https://api.example.com/v1"),
            ("https://u:p@api.example.com", "https://api.example.com"),
            ("http://u@h:8080/v1/?k=v#x", "http://h:8080/v1/"),
            ("HTTP://Host/v1", "HTTP://Host/v1"),
            ("no-scheme.example.com/v1", "no-scheme.example.com/v1"),
        ] {
            assert_eq!(redact_url(raw), shown, "{raw}");
        }
    }

    fn sdk_config(
        region: Option<&'static str>,
        credentials: Option<SharedCredentialsProvider>,
    ) -> aws_config::SdkConfig {
        let mut builder = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest());
        builder.set_region(region.map(aws_config::Region::new));
        builder.set_credentials_provider(credentials);
        builder.build()
    }

    fn credentials_tagged(features: Vec<AwsCredentialFeature>) -> SharedCredentialsProvider {
        let mut credentials = Credentials::new("AKIDEXAMPLE", "SECRETEXAMPLE", None, None, "test");
        credentials.set_property(features);
        SharedCredentialsProvider::new(credentials)
    }

    #[derive(Debug)]
    struct Failing(&'static str);

    impl ProvideCredentials for Failing {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::ready(Err(CredentialsError::not_loaded(self.0)))
        }
    }

    #[derive(Debug)]
    struct Hanging;

    impl ProvideCredentials for Hanging {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::new(std::future::pending())
        }
    }

    async fn check(
        config: aws_config::SdkConfig,
        pairs: &[(&'static str, &'static str)],
    ) -> Result<ReadyProvider, StartupError> {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        let env = move |name: &str| map.get(name).map(|value| (*value).to_string());
        check_bedrock(config, None, &env, Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn bedrock_reports_the_region_and_where_the_credentials_came_from() {
        for (features, expected) in [
            (
                vec![AwsCredentialFeature::CredentialsImds],
                "bedrock · region us-east-1 · credentials from EC2 instance role",
            ),
            (
                vec![AwsCredentialFeature::CredentialsEnvVars],
                "bedrock · region us-east-1 · credentials from environment variables (AWS_ACCESS_KEY_ID)",
            ),
            (
                vec![
                    AwsCredentialFeature::CredentialsSso,
                    AwsCredentialFeature::CredentialsProfile,
                ],
                "bedrock · region us-east-1 · credentials from AWS SSO (profile `work`)",
            ),
            (
                vec![AwsCredentialFeature::CredentialsProfile],
                "bedrock · region us-east-1 · credentials from shared AWS profile `work`",
            ),
            (
                vec![AwsCredentialFeature::CredentialsHttp],
                "bedrock · region us-east-1 · credentials from container credentials endpoint",
            ),
            (
                vec![],
                "bedrock · region us-east-1 · credentials from the AWS default credential chain",
            ),
        ] {
            let config = sdk_config(
                Some("us-east-1"),
                Some(credentials_tagged(features.clone())),
            );
            let ready = check(config, &[("AWS_PROFILE", "work")])
                .await
                .expect("a region and a credential are enough");
            assert_eq!(ready.report().line(), expected, "{features:?}");
            assert_eq!(ready.report().region(), Some("us-east-1"));
            let rendered = format!("{ready:?}");
            assert!(
                !rendered.contains("SECRETEXAMPLE") && !rendered.contains("AKIDEXAMPLE"),
                "the report must not carry the credential: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn the_default_profile_is_named_as_such_when_no_profile_is_set() {
        let config = sdk_config(
            Some("eu-west-1"),
            Some(credentials_tagged(vec![
                AwsCredentialFeature::CredentialsProfile,
            ])),
        );
        let ready = check(config, &[]).await.unwrap();
        assert_eq!(
            ready.report().credential_source(),
            "shared AWS profile (default)"
        );
    }

    #[tokio::test]
    async fn bedrock_without_a_region_says_how_to_set_one() {
        let config = sdk_config(None, Some(credentials_tagged(vec![])));
        let err = check(config, &[]).await.expect_err("no region");
        assert!(matches!(err, StartupError::NoRegion));
        let message = err.to_string();
        assert!(message.contains("--region"), "{message}");
        assert!(message.contains("AWS_REGION"), "{message}");
    }

    #[tokio::test]
    async fn bedrock_without_credentials_says_what_the_sdk_tried_and_how_to_fix_it() {
        let failing = SharedCredentialsProvider::new(Failing("no providers in the chain"));
        let err = check(sdk_config(Some("us-east-1"), Some(failing)), &[])
            .await
            .expect_err("the chain produced nothing");
        let message = err.to_string();
        assert!(
            matches!(err, StartupError::NoCredentials { .. }),
            "{message}"
        );
        assert!(std::error::Error::source(&err).is_some_and(|source| {
            source.is::<aws_credential_types::provider::error::CredentialsError>()
        }));
        assert!(message.contains("no providers in the chain"), "{message}");
        assert!(message.contains("AWS_PROFILE"), "{message}");
        assert!(message.contains("aws sso login"), "{message}");
        assert!(message.contains("AWS_ACCESS_KEY_ID"), "{message}");

        let none = check(sdk_config(Some("us-east-1"), None), &[])
            .await
            .expect_err("no provider at all");
        assert!(matches!(none, StartupError::NoCredentials { .. }));
    }

    #[tokio::test]
    async fn a_credential_lookup_that_never_answers_is_bounded() {
        let hanging = SharedCredentialsProvider::new(Hanging);
        let env = |_: &str| None;
        let err = check_bedrock(
            sdk_config(Some("us-east-1"), Some(hanging)),
            None,
            &env,
            Duration::from_millis(50),
        )
        .await
        .expect_err("a lookup that hangs is a failure");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn bedrock_keeps_the_reasoning_effort_the_operator_asked_for() {
        let config = sdk_config(Some("us-east-1"), Some(credentials_tagged(vec![])));
        let ready = check(config, &[("POLARIS_CODE_EXP_REASONING_EFFORT", " high ")])
            .await
            .unwrap();
        assert_eq!(
            ready.reasoning_fields,
            Some(serde_json::json!({"reasoning": {"effort": "high"}}))
        );
    }

    #[test]
    fn reasoning_effort_accepts_only_known_levels() {
        assert_eq!(
            reasoning_effort_fields(Some(" high ")),
            Some(serde_json::json!({"reasoning": {"effort": "high"}}))
        );
        for value in [None, Some(""), Some("max"), Some("HIGH")] {
            assert_eq!(reasoning_effort_fields(value), None);
        }
    }

    /// `registry_name` is what a `provider/model` id is matched against, so it
    /// must be the name the framework's own provider reports.
    #[test]
    fn the_registry_names_are_the_ones_the_framework_providers_report() {
        use polaris::models::anthropic::AnthropicProvider;
        use polaris::models::llm::LlmProvider;
        use polaris::models::openai::OpenAiProvider;

        assert_eq!(
            AnthropicProvider::new("k").name(),
            Provider::Anthropic.registry_name()
        );
        assert_eq!(
            OpenAiProvider::new("k").name(),
            Provider::OpenAi.registry_name()
        );
    }

    /// Bedrock registers from the config the check loaded, so this reaches no
    /// network. The other two read their key from the process environment
    /// themselves, which a test must not set.
    #[tokio::test]
    async fn bedrock_registers_from_the_config_the_check_loaded() {
        use polaris::models::{ModelRegistry, ModelsPlugin};

        let config = sdk_config(Some("us-east-1"), Some(credentials_tagged(vec![])));
        let ready = check(config, &[]).await.unwrap();
        let mut server = Server::new();
        server.add_plugins(ModelsPlugin);
        ready.install(
            &mut server,
            NonZeroU32::new(1024).unwrap(),
            PriceStore::builtin(),
        );
        server.finish().await.expect("the server builds");
        let registry = server.get_global::<ModelRegistry>().expect("registry");
        assert!(registry.has_llm_provider(Provider::Bedrock.registry_name()));
        assert_eq!(registry.llm_provider_names().len(), 1);
    }

    #[tokio::test]
    async fn a_stored_key_is_used_when_no_variable_holds_one() {
        let dir = tempfile::tempdir().unwrap();
        let stored = stored_in(
            &dir,
            &[
                (Provider::Anthropic, "sk-ant-stored-key-1234"),
                (Provider::OpenAi, "sk-openai-stored-key-5678"),
            ],
            None,
        );
        for (provider, source) in [
            (
                Provider::Anthropic,
                "anthropic · credentials from stored key (polaris login)",
            ),
            (
                Provider::OpenAi,
                "openai · credentials from stored key (polaris login)",
            ),
        ] {
            let ready = prepare_stored(provider, &[], &stored).await.unwrap();
            assert_eq!(ready.report().line(), source);
            assert!(ready.stored_key.is_some());
            let shown = format!("{ready:?} {}", ready.report().line());
            assert!(
                !shown.contains("stored-key"),
                "a key is never shown: {shown}"
            );
        }
    }

    #[tokio::test]
    async fn a_variable_wins_over_a_stored_key() {
        let dir = tempfile::tempdir().unwrap();
        let stored = stored_in(
            &dir,
            &[(Provider::Anthropic, "sk-ant-stored-key-1234")],
            None,
        );
        let ready = prepare_stored(
            Provider::Anthropic,
            &[("ANTHROPIC_API_KEY", "sk-ant-from-env")],
            &stored,
        )
        .await
        .unwrap();
        assert_eq!(ready.report().credential_source(), "ANTHROPIC_API_KEY");
        assert!(
            ready.stored_key.is_none(),
            "the framework reads the variable itself"
        );
    }

    #[tokio::test]
    async fn a_blank_variable_falls_through_to_the_stored_key() {
        let dir = tempfile::tempdir().unwrap();
        let stored = stored_in(
            &dir,
            &[(Provider::OpenAi, "sk-openai-stored-key-5678")],
            None,
        );
        let ready = prepare_stored(Provider::OpenAi, &[("OPENAI_API_KEY", "  ")], &stored)
            .await
            .unwrap();
        assert_eq!(ready.report().credential_source(), STORED_KEY_SOURCE);
    }

    #[tokio::test]
    async fn the_stored_key_of_one_provider_does_not_serve_another() {
        let dir = tempfile::tempdir().unwrap();
        let stored = stored_in(
            &dir,
            &[(Provider::OpenAi, "sk-openai-stored-key-5678")],
            None,
        );
        let err = prepare_stored(Provider::Anthropic, &[], &stored)
            .await
            .expect_err("only an OpenAI key is stored");
        assert!(matches!(err, StartupError::MissingKey { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_run_with_neither_variable_nor_stored_key_is_pointed_at_login_first() {
        let message = prepare_with(Provider::OpenAi, &[])
            .await
            .unwrap_err()
            .to_string();
        let login = message
            .find("polaris login openai")
            .expect("names the login command");
        let export = message
            .find("export OPENAI_API_KEY")
            .expect("names the variable");
        assert!(login < export, "the guided route comes first: {message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_exposed_credentials_file_stops_a_run_that_needs_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let stored = stored_in(
            &dir,
            &[(Provider::Anthropic, "sk-ant-stored-key-1234")],
            None,
        );
        std::fs::set_permissions(stored.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = prepare_stored(Provider::Anthropic, &[], &stored)
            .await
            .expect_err("a key others can read is not used silently");
        let message = err.to_string();
        assert!(
            matches!(err, StartupError::StoredCredentials { .. }),
            "{message}"
        );
        assert!(
            std::error::Error::source(&err)
                .is_some_and(|source| source.is::<crate::credentials::CredentialError>())
        );
        assert!(message.contains("chmod 600"), "{message}");
        assert!(!message.contains("stored-key"), "{message}");

        // The file is not consulted at all when a variable already answers.
        prepare_stored(Provider::Anthropic, &[("ANTHROPIC_API_KEY", "k")], &stored)
            .await
            .expect("the variable wins before the file is read");
    }

    #[tokio::test]
    async fn stored_keys_register_the_providers_without_touching_the_environment() {
        use polaris::models::ModelsPlugin;

        let dir = tempfile::tempdir().unwrap();
        let stored = stored_in(
            &dir,
            &[
                (Provider::Anthropic, "sk-ant-stored-key-1234"),
                (Provider::OpenAi, "sk-openai-stored-key-5678"),
            ],
            None,
        );
        for provider in [Provider::Anthropic, Provider::OpenAi] {
            let ready = prepare_stored(provider, &[], &stored).await.unwrap();
            let mut server = Server::new();
            server.add_plugins(ModelsPlugin);
            ready.install(
                &mut server,
                NonZeroU32::new(1024).unwrap(),
                PriceStore::builtin(),
            );
            server.finish().await.expect("the server builds");
            let registry = server.get_global::<ModelRegistry>().expect("registry");
            assert!(
                registry.has_llm_provider(provider.registry_name()),
                "{} did not register",
                provider.flag()
            );
        }
    }

    #[test]
    fn the_stored_key_plugin_never_prints_its_key() {
        let plugin = StoredKeyOpenAiPlugin::new("sk-secret-value", PriceStore::builtin());
        assert!(!format!("{plugin:?}").contains("sk-secret-value"));
    }

    #[tokio::test]
    async fn a_stored_aws_profile_names_the_source_unless_the_variable_overrides_it() {
        let config = || {
            sdk_config(
                Some("us-east-1"),
                Some(credentials_tagged(vec![
                    AwsCredentialFeature::CredentialsProfile,
                ])),
            )
        };
        let env = |_: &str| None;
        let ready = check_bedrock(config(), Some("saved"), &env, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            ready.report().credential_source(),
            "shared AWS profile `saved`"
        );

        let env = env_of([("AWS_PROFILE", "from-env")]);
        let ready = check_bedrock(config(), Some("saved"), &env, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            ready.report().credential_source(),
            "shared AWS profile `from-env`"
        );
    }

    #[test]
    fn the_listing_counts_a_stored_key_as_found() {
        let dir = tempfile::tempdir().unwrap();
        let stored = stored_in(
            &dir,
            &[(Provider::Anthropic, "sk-ant-stored-key-1234")],
            None,
        );
        let none = |_: &str| None;
        let listing = ModelListing::observe(&none, &stored, &PriceStore::builtin());
        assert_eq!(
            listing.providers[0].credentials,
            CredentialStatus::Found {
                source: STORED_KEY_SOURCE
            }
        );
        assert_eq!(
            listing.providers[1].credentials,
            CredentialStatus::Missing {
                variable: "OPENAI_API_KEY"
            }
        );
        assert!(
            listing
                .to_text()
                .contains("found (stored key (polaris login))")
        );
    }

    #[test]
    fn the_listing_covers_every_provider_without_a_key() {
        let env = env_of([("OPENAI_API_KEY", "k")]);
        let listing = ModelListing::observe(&env, &NOTHING_STORED, &PriceStore::builtin());
        let names: Vec<_> = listing.providers.iter().map(|p| p.provider).collect();
        assert_eq!(names, Provider::ALL);

        assert_eq!(
            listing.providers[0].credentials,
            CredentialStatus::Missing {
                variable: "ANTHROPIC_API_KEY"
            }
        );
        assert_eq!(
            listing.providers[1].credentials,
            CredentialStatus::Found {
                source: "OPENAI_API_KEY"
            }
        );
        assert!(matches!(
            listing.providers[2].credentials,
            CredentialStatus::Unchecked { .. }
        ));

        let anthropic = &listing.providers[0].models;
        assert_eq!(anthropic[0].id, "anthropic/claude-sonnet-5-5");
        assert!(anthropic[0].default);
        assert_eq!(anthropic.iter().filter(|m| m.default).count(), 1);
        assert!(anthropic.iter().all(|m| m.id.starts_with("anthropic/")));

        let bedrock = &listing.providers[2].models;
        assert_eq!(bedrock.len(), 1);
        assert_eq!(bedrock[0].id, DEFAULT_BEDROCK_MODEL);
    }

    #[test]
    fn the_listing_prices_what_the_table_knows_and_says_so_for_the_rest() {
        let none = |_: &str| None;
        let listing = ModelListing::observe(&none, &NOTHING_STORED, &PriceStore::builtin());
        let price_of = |id: &str| {
            listing
                .providers
                .iter()
                .flat_map(|p| &p.models)
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{id} is not listed"))
                .price
        };
        assert!(price_of("anthropic/claude-sonnet-5-5").is_some());
        assert!(price_of("openai/gpt-5.5").is_some());
        assert_eq!(price_of("anthropic/claude-haiku-5-5"), None);
        assert!(listing.to_text().contains("price unknown"));
    }

    #[test]
    fn a_declared_set_replaces_the_builtin_models_in_the_listing() {
        let env = env_of([
            ("POLARIS_CODE_MODEL", "openai/llama3"),
            ("POLARIS_CODE_MODELS", "openai/llama3, openai/qwen3"),
        ]);
        let listing = ModelListing::observe(&env, &NOTHING_STORED, &PriceStore::builtin());
        let ids = |provider: &ProviderListing| {
            provider
                .models
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&listing.providers[1]),
            ["openai/llama3", "openai/qwen3"]
        );
        assert!(listing.providers[1].models[0].default);
        // Another provider has none of those ids and nothing of its own to
        // start on, so its block says so rather than listing a stranger's models.
        assert!(listing.providers[0].models.is_empty());
        assert!(listing.to_text().contains("set POLARIS_CODE_MODELS"));
    }

    #[test]
    fn the_listing_text_names_each_provider_and_its_credential_state() {
        let none = |_: &str| None;
        let text = ModelListing::observe(&none, &NOTHING_STORED, &PriceStore::builtin()).to_text();
        assert!(
            text.contains("--provider anthropic — credentials: missing (set ANTHROPIC_API_KEY)"),
            "{text}"
        );
        assert!(
            text.contains("--provider openai — credentials: missing (set OPENAI_API_KEY)"),
            "{text}"
        );
        assert!(
            text.contains("--provider bedrock — credentials: resolved at startup"),
            "{text}"
        );
        assert!(text.contains("anthropic/claude-sonnet-5-5"), "{text}");
        assert!(text.contains("(default)"), "{text}");
        assert!(text.contains("$2.00 in / $10.00 out per MTok"), "{text}");
    }

    #[test]
    fn the_listing_serialises_as_one_object() {
        let none = |_: &str| None;
        let value = serde_json::to_value(ModelListing::observe(
            &none,
            &NOTHING_STORED,
            &PriceStore::builtin(),
        ))
        .expect("the listing serialises");
        let anthropic = &value["providers"][0];
        assert_eq!(anthropic["provider"], "anthropic");
        assert_eq!(anthropic["credentials"]["status"], "missing");
        assert_eq!(anthropic["credentials"]["variable"], "ANTHROPIC_API_KEY");
        assert_eq!(anthropic["models"][0]["id"], "anthropic/claude-sonnet-5-5");
        assert_eq!(anthropic["models"][0]["default"], true);
        assert_eq!(value["providers"][2]["credentials"]["status"], "unchecked");
    }
}
