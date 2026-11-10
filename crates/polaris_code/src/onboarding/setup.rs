use crate::config::ModelCatalog;
use polaris_tui::dialogs::{Dialog, DialogAnswer, DialogChoice, DialogContent, DialogHost};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use crate::chatgpt::{ChatGptClient, ChatGptStore};
use crate::credentials::{CredentialStore, StoredCredentials};
use crate::providers::{self, Provider, ReadyProvider};

const CONNECTIONS: [Provider; 4] = [
    Provider::ChatGpt,
    Provider::OpenAi,
    Provider::Anthropic,
    Provider::Bedrock,
];

type EnvironmentLookup = dyn Fn(&str) -> Option<String> + Send + Sync;
type ClipboardFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;
type ClipboardCopy = dyn Fn(String) -> ClipboardFuture + Send + Sync;

/// A terminal, credential storage, or provider setup error.
#[derive(Debug)]
pub struct SetupError {
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl SetupError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }
    fn caused_by(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            message: error.to_string(),
            source: Some(Box::new(error)),
        }
    }
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for SetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}
impl From<std::io::Error> for SetupError {
    fn from(error: std::io::Error) -> Self {
        Self::caused_by(error)
    }
}
impl From<crate::credentials::CredentialError> for SetupError {
    fn from(error: crate::credentials::CredentialError) -> Self {
        Self::caused_by(error)
    }
}
#[doc(hidden)]
impl From<crate::chatgpt::ChatGptError> for SetupError {
    fn from(error: crate::chatgpt::ChatGptError) -> Self {
        Self::caused_by(error)
    }
}
impl From<providers::StartupError> for SetupError {
    fn from(error: providers::StartupError) -> Self {
        Self::caused_by(error)
    }
}

/// A prepared connection and its selected defaults.
#[derive(Debug)]
pub struct SetupResult {
    /// Prepared provider and connection details.
    pub ready: ReadyProvider,
    /// Default model for this connection.
    pub model: String,
    /// Available models after applying the host's configuration.
    pub catalog: ModelCatalog,
    /// AWS region selected during setup, if any.
    pub region: Option<String>,
    /// Setup result message to show the user.
    pub notice: String,
}

impl SetupResult {
    /// The model a launch starts on: `requested` when this connection serves
    /// it, otherwise the connection's default.
    #[must_use]
    pub fn launch_model(&self, requested: Option<&str>) -> String {
        let provider = self.ready.report().provider();
        requested
            .and_then(|model| {
                let model = provider.qualify_model(model);
                provider
                    .launch_catalog(self.catalog.clone(), Some(&model))
                    .resolve_noting(&model)
                    .ok()
            })
            .map_or_else(|| self.model.clone(), |resolved| resolved.id)
    }

    /// Save the selected provider, model, and AWS region through the host's writer.
    /// Setup saves credentials separately.
    /// This writes the selected model unconditionally. Hosts that preserve
    /// existing preferences should provide their own connection writer.
    ///
    /// # Errors
    /// Returns the first writer failure; earlier writes may already be saved.
    pub fn save_defaults(
        &self,
        save: &dyn Fn(&str, &str) -> Result<(), crate::accounts::SaveDefaultError>,
    ) -> Result<(), crate::accounts::SaveDefaultError> {
        save("provider", self.ready.report().provider().flag())?;
        save("model", &self.model)?;
        if let Some(region) = &self.region {
            save("region", region)?;
        }
        Ok(())
    }
}

/// Prepares a connection through host dialogs.
///
/// The host renders dialogs and handles input while setup runs. Setup prepares
/// the provider and checks model choices before saving a replacement.
///
/// # Example
///
/// ```no_run
/// use polaris_code::onboarding::SetupService;
/// use polaris_tui::dialogs::DialogHost;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let dialogs = DialogHost::default();
/// let setup = SetupService::user(dialogs.clone())?;
/// // Render `dialogs` and forward terminal input while this future runs.
/// if let Some(connection) = setup.connect(None).await? {
///     assert!(connection.catalog.contains(&connection.model));
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SetupService {
    dialogs: DialogHost,
    credentials: CredentialStore,
    chatgpt: ChatGptStore,
    env: Arc<EnvironmentLookup>,
    catalog: Arc<dyn Fn(&ReadyProvider) -> ModelCatalog + Send + Sync>,
    clipboard: Option<Arc<ClipboardCopy>>,
}

impl std::fmt::Debug for SetupService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupService").finish_non_exhaustive()
    }
}

impl SetupService {
    /// Remove the saved credentials of an API-key or Bedrock connection.
    pub(crate) fn forget(&self, provider: Provider) -> Result<String, SetupError> {
        self.credentials.remove(provider)?;
        Ok("Saved credentials removed. Use /login to reconnect. Environment variables and AWS credentials remain available to other runs.".into())
    }
    /// Create a setup service using the current user's credential stores.
    ///
    /// # Errors
    /// Returns an error if the configuration directory cannot be found.
    pub fn user(dialogs: DialogHost) -> Result<Self, SetupError> {
        let credentials = CredentialStore::user().ok_or_else(|| {
            SetupError::message("no user configuration directory was found; set POLARIS_CONFIG_DIR to choose where credentials are saved")
        })?;
        Ok(Self::new(
            dialogs,
            credentials,
            ChatGptStore::user()?,
            providers::process_env,
        ))
    }

    pub(crate) fn new(
        dialogs: DialogHost,
        credentials: CredentialStore,
        chatgpt: ChatGptStore,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        let env = Arc::new(env);
        let catalog_env = env.clone();
        Self {
            dialogs,
            credentials,
            chatgpt,
            env,
            catalog: Arc::new(move |ready| default_catalog_with_env(ready, catalog_env.as_ref())),
            clipboard: None,
        }
    }

    /// Configure the model catalog used during setup.
    /// The callback must return models served by the prepared provider.
    #[must_use]
    pub fn with_catalog(
        mut self,
        catalog: impl Fn(&ReadyProvider) -> ModelCatalog + Send + Sync + 'static,
    ) -> Self {
        self.catalog = Arc::new(catalog);
        self
    }

    /// Use the host's terminal owner to request clipboard copies.
    ///
    /// In-session hosts should serialize these writes with their renderer.
    /// Completion confirms the request was written, not that the terminal accepted it.
    #[must_use]
    pub fn with_clipboard<Fut>(
        mut self,
        copy: impl Fn(String) -> Fut + Send + Sync + 'static,
    ) -> Self
    where
        Fut: Future<Output = io::Result<()>> + Send + 'static,
    {
        self.clipboard = Some(Arc::new(move |text| Box::pin(copy(text))));
        self
    }

    /// Choose, validate, and save a connection.
    /// An `initial` provider skips the first connection picker.
    /// Returns `Ok(None)` on cancellation; the active session is not changed.
    ///
    /// # Errors
    /// This method does not return setup failures: storage, authentication,
    /// and provider errors are offered for retry in the dialog. Cancelling
    /// returns `Ok(None)`.
    pub async fn connect(
        &self,
        initial: Option<Provider>,
    ) -> Result<Option<SetupResult>, SetupError> {
        self.connect_checked(initial, &|_, _, _| Ok(())).await
    }

    pub(crate) async fn connect_checked(
        &self,
        initial: Option<Provider>,
        validate: &(dyn Fn(&ReadyProvider, &str, &ModelCatalog) -> Result<(), String> + Sync),
    ) -> Result<Option<SetupResult>, SetupError> {
        let _workflow = self.dialogs.begin_workflow();
        let mut next = initial;
        loop {
            let provider = match next.take() {
                Some(provider) => provider,
                None => {
                    let rows = CONNECTIONS
                        .iter()
                        .map(|provider| {
                            let (label, description) = match provider {
                                Provider::ChatGpt => (
                                    "Continue with ChatGPT",
                                    "Use your ChatGPT plan for eligible requests",
                                ),
                                Provider::OpenAi => (
                                    "OpenAI API key",
                                    "Usage will be billed to your API account",
                                ),
                                Provider::OpenAiChat => (
                                    "OpenAI Chat Completions",
                                    "Connect to a server that supports the OpenAI Chat Completions API",
                                ),
                                Provider::Anthropic => (
                                    "Anthropic API key",
                                    "Usage will be billed to your API account",
                                ),
                                Provider::Bedrock => (
                                    "AWS Bedrock",
                                    "Connect through AWS Bedrock using your AWS credentials",
                                ),
                            };
                            DialogChoice::new(label, description)
                        })
                        .collect();
                    let Some(DialogAnswer::Choice(index)) = self
                        .dialogs
                        .present(
                            Dialog::new(
                                "Welcome to Polaris Code",
                                "Choose a connection to get started.",
                                DialogContent::Choices(rows),
                            )
                            .with_header(super::brand::header()),
                        )
                        .await
                    else {
                        return Ok(None);
                    };
                    CONNECTIONS[index]
                }
            };
            match self.configure(provider, validate).await {
                Ok(Some(result)) => return Ok(Some(result)),
                Ok(None) => {}
                Err(error) => {
                    let Some(index) = self
                        .choose(
                            "Couldn’t connect",
                            &error.to_string(),
                            vec![
                                DialogChoice::new("Try again", ""),
                                DialogChoice::new("Choose another connection", ""),
                            ],
                        )
                        .await
                    else {
                        return Ok(None);
                    };
                    if index == 0 {
                        next = Some(provider);
                    }
                }
            }
        }
    }

    fn has_existing_key(&self, provider: Provider) -> bool {
        if let Some(variable) = provider.key_var()
            && (self.env)(variable).is_some_and(|value| !value.trim().is_empty())
        {
            return true;
        }
        self.credentials.api_key(provider).ok().flatten().is_some()
    }

    async fn configure(
        &self,
        provider: Provider,
        validate: &(dyn Fn(&ReadyProvider, &str, &ModelCatalog) -> Result<(), String> + Sync),
    ) -> Result<Option<SetupResult>, SetupError> {
        let mut region = None;
        let mut key_candidate = None;
        let mut profile_candidate = None;
        let mut chatgpt_account = None;
        let notice = match provider {
            Provider::ChatGpt => {
                let Some(account_id) = self.chatgpt_login().await? else {
                    return Ok(None);
                };
                chatgpt_account = Some(account_id);
                "Connected to ChatGPT.".to_string()
            }
            Provider::Anthropic | Provider::OpenAi | Provider::OpenAiChat => {
                let use_existing = if !self.has_existing_key(provider) {
                    false
                } else {
                    match self
                        .choose(
                            &format!("Connect to {}", provider.name()),
                            &format!("An API key already exists for {}.", provider.name()),
                            vec![
                                DialogChoice::new("Use existing API key", ""),
                                DialogChoice::new("Enter a different API key", ""),
                            ],
                        )
                        .await
                    {
                        Some(0) => true,
                        Some(_) => false,
                        None => return Ok(None),
                    }
                };
                if !use_existing {
                    let variable = provider.key_var().expect("API key provider");
                    let override_note = if (self.env)(variable)
                        .is_some_and(|v| !v.trim().is_empty())
                    {
                        format!(
                            "\n\nThe {variable} environment variable takes priority over this key. Unset it to use the key you enter here."
                        )
                    } else {
                        String::new()
                    };
                    let body = format!("Enter your API key.{override_note}");
                    let Some(key) = self
                        .input_checked(
                            &format!("{} API key", provider.name()),
                            &body,
                            "",
                            true,
                            |value| required("API key", value),
                        )
                        .await
                    else {
                        return Ok(None);
                    };
                    key_candidate = Some(key);
                }
                "API connection configured. Access will be checked on your first request.".into()
            }
            Provider::Bedrock => {
                let known = ["POLARIS_CODE_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"]
                    .iter()
                    .filter_map(|v| (self.env)(v))
                    .find(|value| !value.trim().is_empty())
                    .unwrap_or_default();
                let Some(chosen_region) = self
                    .input_checked(
                        "AWS Bedrock region",
                        "Enter the AWS region where your models are enabled (for example, us-east-1).",
                        &known,
                        false,
                        |value| required("AWS region", value),
                    )
                    .await
                else {
                    return Ok(None);
                };
                let saved = self.credentials.aws_profile()?.unwrap_or_default();
                let Some(profile) = self.input(
                    "AWS profile",
                    "Enter an AWS profile, or leave blank to use your default AWS credentials.\nIf AWS_PROFILE is set, it takes priority.",
                    &saved, false,
                ).await else { return Ok(None); };
                profile_candidate = Some(profile.trim().to_string());
                region = Some(chosen_region.trim().to_string());
                "AWS credentials resolved successfully. Model access is checked on your first request.".into()
            }
        };
        let candidate = CandidateCredentials {
            saved: &self.credentials,
            key: key_candidate.as_deref().map(|key| (provider, key)),
            profile: profile_candidate.as_deref(),
        };
        let (title, body) = if provider == Provider::Bedrock {
            (
                "Checking AWS credentials",
                "Looking up credentials for the selected profile and region.\nThis can take a moment. No model request is sent.",
            )
        } else {
            (
                "Preparing your connection",
                "Loading connection settings and available models.",
            )
        };
        let ready = self
            .wait_for(title, body, async {
                if let Some(id) = &chatgpt_account {
                    providers::prepare_chatgpt_account(&self.chatgpt, id).await
                } else {
                    providers::prepare(provider, region.as_deref(), self.env.as_ref(), &candidate)
                        .await
                }
            })
            .await?;
        let Some(ready) = ready else {
            return Ok(None);
        };
        let mut catalog = (self.catalog)(&ready)
            .restricted_to_providers(&[ready.report().provider().registry_name()]);
        // A saved default from another connection must not prevent switching.
        // Keep invalid defaults for the selected provider as configuration errors.
        if catalog
            .default_model()
            .split_once('/')
            .is_some_and(|(prefix, _)| prefix != provider.registry_name())
            && let Some(default) = catalog.models().first()
        {
            catalog = ModelCatalog::new(default, catalog.models().iter().cloned())
                .with_display_names(
                    catalog
                        .models()
                        .iter()
                        .map(|id| (id.clone(), catalog.display_name(id).to_string())),
                );
        }
        let model = catalog
            .select_noting(None)
            .map_err(SetupError::caused_by)?
            .id;
        validate(&ready, &model, &catalog).map_err(SetupError::message)?;
        if let Some(key) = key_candidate {
            self.credentials.set_api_key(provider, &key)?;
        }
        if let Some(profile) = profile_candidate {
            if profile.is_empty() {
                self.credentials.remove(Provider::Bedrock)?;
            } else {
                self.credentials.set_aws_profile(&profile)?;
            }
        }
        if let Some(id) = chatgpt_account {
            self.chatgpt.select(&id).await?;
        }
        Ok(Some(SetupResult {
            ready,
            model,
            catalog,
            region,
            notice,
        }))
    }

    async fn chatgpt_login(&self) -> Result<Option<String>, SetupError> {
        let client = ChatGptClient::new(self.chatgpt.clone())?;
        let accounts = self.chatgpt.accounts_async().await?;
        let mut selected = None;
        if !accounts.is_empty() {
            let mut rows: Vec<_> = accounts
                .iter()
                .map(|account| {
                    let label = if account.signed_in {
                        account.label.clone()
                    } else {
                        format!("{} (Signed out)", account.label)
                    };
                    DialogChoice::new(label, "")
                })
                .collect();
            rows.push(DialogChoice::new("Add another account or workspace", ""));
            let Some(index) = self
                .choose(
                    "ChatGPT account",
                    "Choose the account that will supply usage to Polaris Code.",
                    rows,
                )
                .await
            else {
                return Ok(None);
            };
            if let Some(account) = accounts.get(index) {
                selected = Some(account.clone());
            }
        }
        let account = if let Some(account) =
            selected.as_ref().filter(|a| a.signed_in && a.plan_enabled)
        {
            account.clone()
        } else {
            let mut consent = selected
                .as_ref()
                .is_some_and(|a| a.signed_in && !a.plan_enabled);
            let mut retry_client_id = None;
            loop {
                let begin = async {
                    match retry_client_id.as_deref() {
                        Some(client_id) => {
                            client
                                .begin_with_client(
                                    selected.as_ref().map(|a| a.id.as_str()),
                                    consent,
                                    Some(client_id),
                                )
                                .await
                        }
                        None => {
                            client
                                .begin(selected.as_ref().map(|a| a.id.as_str()), consent)
                                .await
                        }
                    }
                };
                let Some(pending) = self
                    .wait_for(
                        "Preparing sign-in",
                        "Preparing your sign-in link. You can cancel and choose another connection.",
                        begin,
                    )
                    .await?
                else {
                    return Ok(None);
                };
                let url = pending.authorization_url().to_string();
                let Some(_) = self.open_sign_in_browser(&url).await? else {
                    return Ok(None);
                };
                let mut feedback = String::new();
                let completion = client.complete(pending);
                let mut completion = std::pin::pin!(completion);
                let result = loop {
                    let mut body = "Sign in to ChatGPT in your browser.".to_string();
                    if !feedback.is_empty() {
                        body.push_str("\n\n");
                        body.push_str(&feedback);
                    }
                    let waiting = self.sign_in_action(&body, &url);
                    tokio::select! {
                        result = &mut completion => break result,
                        action = waiting => match action {
                            Some(0) => {
                                let Some(_) = self.open_sign_in_browser(&url).await? else {
                                    return Ok(None);
                                };
                                feedback.clear();
                            }
                            Some(1) => {
                                let Some(copied) = self.wait_for(
                                    "Copying sign-in link",
                                    "Copying the link to your clipboard. You can cancel and choose another connection.",
                                    async { Ok::<_, SetupError>(self.copy_sign_in_link(&url).await) },
                                ).await? else {
                                    return Ok(None);
                                };
                                feedback = match copied {
                                    Ok(super::browser::CopyOutcome::Copied) => "Sign-in link copied.",
                                    Ok(super::browser::CopyOutcome::TerminalRequested) =>
                                        "Copy requested. Paste into your browser; allow clipboard access if prompted.",
                                    Err(_) => "Couldn’t copy the link. Try opening it with Open browser.",
                                }.into();
                            }
                            _ => return Ok(None),
                        }
                    }
                };
                let account = match result {
                    Ok(account) => account,
                    Err(error) => {
                        if retry_client_id.is_none()
                            && let Some(client_id) = error.retry_client_id()
                        {
                            retry_client_id = Some(client_id.to_string());
                            continue;
                        }
                        return Err(error.into());
                    }
                };
                if account.plan_enabled {
                    break account;
                }
                selected = Some(account);
                let action = self
                    .choose(
                        "ChatGPT signed in · Plan usage off",
                        "You’re signed in, but ChatGPT plan usage has not been authorized.\nEnable it to send requests, or choose another connection.",
                        vec![
                            DialogChoice::new("Enable plan usage", ""),
                            DialogChoice::new("Choose another connection", ""),
                        ],
                    )
                    .await;
                if action != Some(0) {
                    return Ok(None);
                }
                consent = true;
            }
        };
        if self
            .chatgpt
            .needs_usage_acknowledgement_async(&account.id)
            .await?
        {
            loop {
                match self.choose(
                    "ChatGPT connected",
                    "Eligible requests will use your ChatGPT plan and count toward its limits.\nCredits may be used if you’ve enabled that in ChatGPT.\nManage the Polaris Code limit in ChatGPT Settings → Usage.",
                    vec![DialogChoice::new("Continue", ""), DialogChoice::new("Manage usage", "")],
                ).await {
                    Some(0) => { self.chatgpt.acknowledge_usage(&account.id).await?; break; }
                    Some(_) => { self.open_usage().await; }
                    None => return Ok(None),
                }
            }
        }
        Ok(Some(account.id))
    }

    async fn choose(&self, title: &str, body: &str, choices: Vec<DialogChoice>) -> Option<usize> {
        super::prompts::choose(&self.dialogs, title, body, choices).await
    }

    async fn open_sign_in_browser(&self, url: &str) -> Result<Option<()>, SetupError> {
        self.wait_for(
            "Opening browser",
            "Opening the sign-in link. You can cancel and choose another connection.",
            async {
                let _ = super::browser::open(url).await;
                Ok::<_, SetupError>(())
            },
        )
        .await
    }

    async fn copy_sign_in_link(&self, text: &str) -> io::Result<super::browser::CopyOutcome> {
        if let Some(copy) = &self.clipboard {
            copy(text.to_string()).await?;
            Ok(super::browser::CopyOutcome::TerminalRequested)
        } else {
            super::browser::copy(text).await
        }
    }

    pub(crate) fn dialogs(&self) -> &DialogHost {
        &self.dialogs
    }

    async fn sign_in_action(&self, body: &str, url: &str) -> Option<usize> {
        match self
            .dialogs
            .present(
                Dialog::new(
                    "Continue in your browser",
                    body,
                    DialogContent::Choices(vec![
                        DialogChoice::new("Open browser", "").with_shortcut('o'),
                        DialogChoice::new("Copy sign-in link", "").with_shortcut('c'),
                    ]),
                )
                .with_detail("Sign-in link", url)
                .with_detail_choice(1),
            )
            .await
        {
            Some(DialogAnswer::Choice(index)) => Some(index),
            _ => None,
        }
    }

    async fn wait_for<T, E: Into<SetupError>>(
        &self,
        title: &str,
        body: &str,
        work: impl std::future::Future<Output = Result<T, E>>,
    ) -> Result<Option<T>, SetupError> {
        tokio::select! {
            result = work => result.map(Some).map_err(Into::into),
            _ = self.choose(title, body, vec![]) => Ok(None),
        }
    }

    async fn input(&self, title: &str, body: &str, initial: &str, secret: bool) -> Option<String> {
        self.input_with_error(title, body, initial, secret, None)
            .await
    }

    async fn input_with_error(
        &self,
        title: &str,
        body: &str,
        initial: &str,
        secret: bool,
        error: Option<String>,
    ) -> Option<String> {
        match self
            .dialogs
            .present(Dialog::new(
                title,
                body,
                DialogContent::Input {
                    initial: initial.into(),
                    secret,
                    error,
                },
            ))
            .await
        {
            Some(DialogAnswer::Input(value)) => Some(value),
            _ => None,
        }
    }

    async fn input_checked(
        &self,
        title: &str,
        body: &str,
        initial: &str,
        secret: bool,
        check: impl Fn(&str) -> Result<(), String> + Sync,
    ) -> Option<String> {
        let mut value = initial.to_string();
        let mut error = None;
        loop {
            value = self
                .input_with_error(title, body, &value, secret, error.take())
                .await?;
            match check(&value) {
                Ok(()) => return Some(value),
                Err(message) => error = Some(message),
            }
        }
    }

    async fn open_usage(&self) {
        super::prompts::open_usage(&self.dialogs).await;
    }

    /// Sign out of one `ChatGPT` account and say whether revocation was confirmed.
    pub(crate) async fn sign_out_account(&self, id: &str) -> Result<String, SetupError> {
        let label = self
            .chatgpt
            .accounts_async()
            .await?
            .into_iter()
            .find(|account| account.id == id)
            .map_or_else(
                || Provider::ChatGpt.name().to_string(),
                |account| account.label,
            );
        let confirmed = ChatGptClient::new(self.chatgpt.clone())?
            .sign_out(id)
            .await?;
        Ok(super::prompts::sign_out_notice(&label, confirmed))
    }
}

/// The models a prepared connection offers with no host configuration applied.
pub(super) fn default_catalog(ready: &ReadyProvider) -> ModelCatalog {
    default_catalog_with_env(ready, &providers::process_env)
}

fn default_catalog_with_env(ready: &ReadyProvider, env: providers::Lookup<'_>) -> ModelCatalog {
    let provider = ready.report().provider();
    provider
        .launch_catalog(ready.model_catalog(env), None)
        .restricted_to_providers(&[provider.registry_name()])
}

fn required(label: &'static str, value: &str) -> Result<(), String> {
    crate::credentials::checked(label, value)
        .map(|_| ())
        .map_err(|error| {
            if value.trim().is_empty() {
                format!("Enter an {label} to continue.")
            } else {
                format!("Check the {label}: {error}")
            }
        })
}

struct CandidateCredentials<'a> {
    saved: &'a CredentialStore,
    key: Option<(Provider, &'a str)>,
    profile: Option<&'a str>,
}

impl StoredCredentials for CandidateCredentials<'_> {
    fn api_key(
        &self,
        provider: Provider,
    ) -> Result<Option<String>, crate::credentials::CredentialError> {
        match self.key {
            Some((candidate_provider, key)) if candidate_provider == provider => {
                Ok(Some(key.to_string()))
            }
            _ => self.saved.api_key(provider),
        }
    }
    fn aws_profile(&self) -> Result<Option<String>, crate::credentials::CredentialError> {
        match self.profile {
            Some(profile) => Ok((!profile.is_empty()).then(|| profile.to_string())),
            None => self.saved.aws_profile(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clipboard_requests_use_the_host_and_preserve_its_failure() {
        let dir = tempfile::tempdir().expect("temporary credential directory");
        let copied = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = copied.clone();
        let setup = SetupService::new(
            DialogHost::default(),
            CredentialStore::at(dir.path().join("credentials.toml")),
            ChatGptStore::at(dir.path().join("chatgpt.json")),
            |_| None,
        )
        .with_clipboard(move |text| {
            captured
                .lock()
                .expect("record clipboard request")
                .push(text);
            async { Ok(()) }
        });
        let text = format!("https://example.com/authorize?state={}", "a".repeat(500));
        assert!(
            matches!(
                setup.copy_sign_in_link(&text).await,
                Ok(super::super::browser::CopyOutcome::TerminalRequested)
            ),
            "the host confirms only that its terminal request was sent"
        );
        assert_eq!(
            *copied.lock().expect("read requests"),
            [text],
            "the host receives the full original URL"
        );

        let setup = setup.with_clipboard(|_| async {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "runtime stopped"))
        });
        let error = setup
            .copy_sign_in_link("https://example.com/")
            .await
            .err()
            .expect("host failure is returned");
        assert_eq!(
            error.kind(),
            io::ErrorKind::BrokenPipe,
            "a failed host request must not fall back to an independent terminal writer"
        );
    }

    #[test]
    fn candidate_credentials_overlay_saved() {
        let dir = tempfile::tempdir().expect("temporary directory is created");
        let saved = CredentialStore::at(dir.path().join("credentials.toml"));
        saved
            .set_api_key(Provider::OpenAi, "original-key")
            .expect("original key is saved");
        saved
            .set_aws_profile("original-profile")
            .expect("original profile is saved");
        for profile in ["replacement-profile", ""] {
            let candidate = CandidateCredentials {
                saved: &saved,
                key: Some((Provider::OpenAi, "replacement-key")),
                profile: Some(profile),
            };
            assert_eq!(
                candidate
                    .api_key(Provider::OpenAi)
                    .expect("candidate key is readable")
                    .as_deref(),
                Some("replacement-key"),
                "the candidate key overrides the saved key"
            );
            assert_eq!(
                candidate
                    .aws_profile()
                    .expect("candidate profile is readable")
                    .as_deref(),
                (!profile.is_empty()).then_some(profile),
                "the candidate profile {profile:?} overrides the saved profile"
            );
            assert_eq!(
                saved
                    .api_key(Provider::OpenAi)
                    .expect("saved key is readable")
                    .as_deref(),
                Some("original-key"),
                "the saved key is untouched until accepted"
            );
            assert_eq!(
                saved
                    .aws_profile()
                    .expect("saved profile is readable")
                    .as_deref(),
                Some("original-profile"),
                "the saved profile is untouched until accepted"
            );
        }
    }
}
