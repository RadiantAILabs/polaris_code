//! In-session account commands for connecting, signing out, and viewing usage.

use std::sync::Arc;

use polaris::models::pricing::PriceStore;
use polaris_tui::commands::CommandSpec;
use polaris_tui::dialogs::DialogChoice;

use super::{CommandError, CommandOutcome, CommandRegistry, SessionSettings};
use crate::accounts::SaveDefaultError;
use crate::connection::ConnectionRuntime;
use crate::onboarding::prompts::{choose, open_usage};
use crate::onboarding::{SetupError, SetupResult, SetupService};
use crate::providers::Provider;
use crate::usage::TokenMeter;

type SaveDefaults = Arc<dyn Fn(&SetupResult) -> Result<(), SaveDefaultError> + Send + Sync>;

const COMMANDS: [(&str, &str, AccountCommand); 4] = [
    (
        "login",
        "Set up or change your connection",
        AccountCommand::Login,
    ),
    (
        "account",
        "Show your active connection",
        AccountCommand::Account,
    ),
    (
        "logout",
        "Sign out of the active connection",
        AccountCommand::Logout,
    ),
    (
        "usage",
        "View session usage and manage ChatGPT limits",
        AccountCommand::Usage,
    ),
];

/// One-line description of the active connection and how it is billed, for the
/// status row.
#[must_use]
pub fn connection_status(runtime: &ConnectionRuntime) -> String {
    let label = connection_label(runtime);
    if !runtime.is_connected() || runtime.is_usage_paused() {
        format!("{label} · /login")
    } else {
        label
    }
}

fn connection_label(runtime: &ConnectionRuntime) -> String {
    if !runtime.is_connected() {
        return "Signed out".into();
    }
    let report = runtime.report();
    match report.provider() {
        Provider::ChatGpt if runtime.is_usage_paused() => "ChatGPT (Usage Paused)".into(),
        Provider::ChatGpt => "ChatGPT (Account Usage)".into(),
        Provider::OpenAi | Provider::OpenAiChat if report.endpoint().is_some() => {
            format!("{} (Custom API Endpoint)", report.provider().name())
        }
        other => format!("{} (API Billing)", other.name()),
    }
}

/// Account commands for an interactive session.
///
/// Run commands between turns so connection changes finish before the next model request.
///
/// # Example
///
/// ```
/// use polaris_code::terminal::{CommandRegistry, SessionAccounts};
/// # fn example(accounts: &SessionAccounts) -> Result<(), Box<dyn std::error::Error>> {
/// let commands = CommandRegistry::new();
/// accounts.register(&commands)?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SessionAccounts {
    setup: SetupService,
    runtime: ConnectionRuntime,
    settings: SessionSettings,
    meter: TokenMeter,
    pricing: PriceStore,
    require_price: bool,
    save: SaveDefaults,
}

#[derive(Clone, Copy)]
enum AccountCommand {
    Login,
    Account,
    Logout,
    Usage,
}

impl std::fmt::Debug for SessionAccounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAccounts")
            .field("provider", &self.runtime.report().provider())
            .finish_non_exhaustive()
    }
}

impl SessionAccounts {
    /// Create account controls for the session's connection, settings, and usage meter.
    ///
    /// By default no dollar spend guard is enforced, so a connection with
    /// unpriced models is accepted, and no defaults are saved after a change.
    /// Use [`with_spend_guard`](Self::with_spend_guard) and
    /// [`with_defaults`](Self::with_defaults) to change either.
    pub fn new(
        setup: SetupService,
        runtime: ConnectionRuntime,
        settings: SessionSettings,
        meter: TokenMeter,
    ) -> Self {
        Self {
            setup,
            runtime,
            settings,
            meter,
            pricing: PriceStore::builtin(),
            require_price: false,
            save: Arc::new(|_| Ok(())),
        }
    }

    /// Register the account commands so each explains why setup is unavailable.
    ///
    /// Use when [`SetupService::user`] fails, so commands explain the failure
    /// instead of appearing unknown.
    ///
    /// # Errors
    ///
    /// [`CommandError`] when one of the commands is already registered.
    pub fn register_unavailable(
        commands: &CommandRegistry,
        reason: &SetupError,
    ) -> Result<(), CommandError> {
        for (name, _, _) in COMMANDS {
            let message = format!("account setup is unavailable: {reason}");
            commands.register_sync(
                CommandSpec::new(name, "Account setup unavailable"),
                move |_, _| CommandOutcome::Error(message.clone()),
            )?;
        }
        Ok(())
    }

    /// Show a pending usage-limit dialog. Call after saving the failed turn.
    pub async fn show_usage_limit_if_needed(&self) -> Option<CommandOutcome> {
        if !self.runtime.take_usage_notice() {
            return None;
        }
        loop {
            match choose(
                self.setup.dialogs(),
                "ChatGPT usage paused",
                "Your conversation is saved. A ChatGPT plan or app limit was reached.\nReview the limits in ChatGPT Settings → Usage.\nAfter changing a limit, use /login to reconnect. Billing will not switch automatically.",
                vec![
                    DialogChoice::new("Manage usage", ""),
                    DialogChoice::new("Change connection", ""),
                ],
            )
            .await
            {
                Some(0) => open_usage(self.setup.dialogs()).await,
                Some(1) => return Some(self.connect().await),
                _ => break,
            }
        }
        None
    }

    /// Set the price table and whether connection changes require priced models.
    /// When `require_price` is true, reject unpriced models before saving or activation.
    #[must_use]
    pub fn with_spend_guard(mut self, pricing: PriceStore, require_price: bool) -> Self {
        self.pricing = pricing;
        self.require_price = require_price;
        self
    }

    /// Set the callback that saves defaults after a successful connection change.
    /// A failure is reported while the new connection stays active.
    /// The callback may return [`SaveDefaultError`] or any type that converts to it,
    /// such as a `String`.
    #[must_use]
    pub fn with_defaults<E: Into<SaveDefaultError>>(
        mut self,
        save: impl Fn(&SetupResult) -> Result<(), E> + Send + Sync + 'static,
    ) -> Self {
        self.save = Arc::new(move |result| save(result).map_err(Into::into));
        self
    }

    /// Register `/login`, `/account`, `/logout`, and `/usage`.
    ///
    /// # Errors
    ///
    /// [`CommandError`] when one of the commands is already registered.
    pub fn register(&self, commands: &CommandRegistry) -> Result<(), CommandError> {
        for (name, description, command) in COMMANDS {
            let accounts = self.clone();
            commands.register(
                CommandSpec::new(name, description),
                Arc::new(move |_| {
                    let accounts = accounts.clone();
                    Box::pin(async move { accounts.command(command).await })
                }),
            )?;
        }
        Ok(())
    }

    async fn command(&self, command: AccountCommand) -> CommandOutcome {
        match command {
            AccountCommand::Login => self.connect().await,
            AccountCommand::Account => self.account(),
            AccountCommand::Logout => self.logout().await,
            AccountCommand::Usage => self.usage(),
        }
    }

    fn account(&self) -> CommandOutcome {
        if !self.runtime.is_connected() {
            return CommandOutcome::Reply("You’re signed out.\n\nUse /login to connect.".into());
        }
        let status = if self.runtime.is_usage_paused() {
            "Usage paused"
        } else {
            "Connected"
        };
        let provider = connection_label(&self.runtime);
        CommandOutcome::Reply(format!(
            "Provider: {provider}\nModel:    {}\nStatus:   {status}\n\nUse /login to change connection or /logout to sign out.",
            self.settings.snapshot().model_id
        ))
    }

    fn usage(&self) -> CommandOutcome {
        let usage = self.meter.snapshot(&self.settings.snapshot().model_id);
        let body = format!(
            "{}\n\nThis session: {} input tokens · {} output tokens · {} total.\nToken counts do not show your remaining plan or account balance.",
            connection_status(&self.runtime),
            usage.input_tokens,
            usage.output_tokens,
            usage.total_tokens
        );
        if self.runtime.report().provider() != Provider::ChatGpt {
            return CommandOutcome::Reply(body);
        }
        CommandOutcome::Reply(format!(
            "{body}\nManage usage: {}",
            crate::chatgpt::USAGE_URL
        ))
    }

    async fn connect(&self) -> CommandOutcome {
        let validate = |ready: &crate::providers::ReadyProvider,
                        model: &str,
                        catalog: &crate::config::ModelCatalog| {
            let provider = ready.report().provider();
            if !self.runtime.has_route(provider) {
                return Err(format!(
                    "{} cannot be used in this session because no model route is registered for it; the active connection was kept",
                    provider.name()
                ));
            }
            catalog
                .resolve_noting(model)
                .map_err(|error| error.to_string())?;
            if self.require_price
                && catalog
                    .models()
                    .iter()
                    .any(|model| crate::usage::price_for(&self.pricing, model).is_none())
            {
                return Err("this connection includes unpriced models, so the dollar spend guard cannot be enforced; the active connection was kept (start a separate run with a token limit to use it)".into());
            }
            Ok(())
        };
        match self.setup.connect_checked(None, &validate).await {
            Ok(Some(result)) => {
                // Activate first: it is the step that can refuse, and the model
                // and catalog were already checked by `validate`.
                if let Err(error) = self.runtime.activate(&result.ready) {
                    return CommandOutcome::Error(format!(
                        "{error}; the active connection was kept"
                    ));
                }
                if let Err(error) = self
                    .settings
                    .set_connection(result.model.clone(), result.catalog.clone())
                {
                    return CommandOutcome::Error(error.to_string());
                }
                let mut message = format!(
                    "{}\n{}\nModel: {}. You can now continue your conversation.",
                    result.notice,
                    result.ready.report().line(),
                    result.model
                );
                if let Err(error) = (self.save)(&result) {
                    message.push_str(&format!(
                        "\nConnection active for this session; defaults could not be saved: {error}"
                    ));
                }
                CommandOutcome::Reply(message)
            }
            Ok(None) => CommandOutcome::Reply(
                "Connection setup cancelled. Your session is unchanged.".into(),
            ),
            Err(error) => CommandOutcome::Error(error.to_string()),
        }
    }

    async fn logout(&self) -> CommandOutcome {
        if !self.runtime.is_connected() {
            return CommandOutcome::Reply(
                "You’re already signed out. Use /login to connect an account.".into(),
            );
        }
        let provider = self.runtime.report().provider();
        let result = match self.runtime.account_id() {
            Some(id) => self.setup.sign_out_account(&id).await,
            None => self.setup.forget(provider),
        };
        match result {
            Ok(message) => {
                self.runtime.disconnect();
                CommandOutcome::Reply(message)
            }
            Err(error) => CommandOutcome::Error(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use polaris::models::llm::{
        DynLlmProvider, GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream, Message,
    };
    use polaris::models::{ModelRegistry, ModelsPlugin};
    use polaris::system::server::Server;
    use polaris_tui::TuiEvent;
    use polaris_tui::commands::CommandInvocation;
    use polaris_tui::dialogs::DialogHost;
    use polaris_tui::ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    };

    use super::*;
    use crate::agents::PermissionMode;
    use crate::chatgpt::ChatGptStore;
    use crate::config::ModelCatalog;
    use crate::connection::ConnectionPlugin;
    use crate::credentials::{CredentialStore, StoredCredentials};
    use crate::terminal::SettingsSnapshot;

    /// The models every test connection offers, named after its provider.
    fn catalog_for(provider: Provider) -> ModelCatalog {
        let name = provider.registry_name();
        ModelCatalog::new(format!("{name}/m1"), [format!("{name}/m2")])
    }

    /// Records which connection served each request, then refuses it.
    struct Served {
        by: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl LlmProvider for Served {
        fn name(&self) -> &'static str {
            "served"
        }
        async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
            self.log.lock().expect("log lock").push(self.by);
            Err(GenerationError::UnsupportedOperation("recorded".into()))
        }
        async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
            self.log.lock().expect("log lock").push(self.by);
            Err(GenerationError::UnsupportedOperation("recorded".into()))
        }
    }

    #[derive(Default)]
    struct Options {
        require_price: bool,
        single_route: bool,
        save_error: Option<&'static str>,
    }

    /// A signed-in session on `OpenAI` whose connections are recorded, not real.
    struct Session {
        _dir: tempfile::TempDir,
        server: Server,
        host: DialogHost,
        commands: CommandRegistry,
        credentials: CredentialStore,
        runtime: ConnectionRuntime,
        settings: SessionSettings,
        served: Arc<Mutex<Vec<&'static str>>>,
        saved: Arc<Mutex<Vec<String>>>,
    }

    impl Session {
        async fn start(options: Options) -> Self {
            let dir = tempfile::tempdir().expect("temporary directory");
            let credentials = CredentialStore::at(dir.path().join("credentials.toml"));
            credentials
                .set_api_key(Provider::OpenAi, "original-key")
                .expect("the original key is saved");
            let ready = crate::providers::prepare(Provider::OpenAi, None, &|_| None, &credentials)
                .await
                .expect("a stored key prepares the provider");

            let served = Arc::new(Mutex::new(Vec::new()));
            let log = served.clone();
            let plugin = ConnectionPlugin::with_test_factory(&ready, move |ready| {
                let provider: Arc<dyn DynLlmProvider> = Arc::new(Served {
                    by: ready.report().provider().registry_name(),
                    log: log.clone(),
                });
                provider
            });
            let plugin = if options.single_route {
                plugin
            } else {
                plugin.with_switching()
            };
            let runtime = plugin.runtime();
            let mut server = Server::new();
            server.add_plugins(ModelsPlugin).add_plugins(plugin);
            server.finish().await.expect("the server starts");

            let host = DialogHost::default();
            let setup = SetupService::new(
                host.clone(),
                credentials.clone(),
                ChatGptStore::at(dir.path().join("chatgpt.json")),
                |_| None,
            )
            .with_catalog(|ready| catalog_for(ready.report().provider()));
            let settings = SessionSettings::new(
                "openai/m1",
                PermissionMode::AutoEdit,
                &catalog_for(Provider::OpenAi),
            );
            let saved = Arc::new(Mutex::new(Vec::new()));
            let record = saved.clone();
            let save_error = options.save_error;
            let accounts = SessionAccounts::new(
                setup,
                runtime.clone(),
                settings.clone(),
                TokenMeter::new().with_pricing_store(PriceStore::builtin()),
            )
            .with_spend_guard(PriceStore::builtin(), options.require_price)
            .with_defaults(move |result| {
                record
                    .lock()
                    .expect("saved lock")
                    .push(result.model.clone());
                match save_error {
                    Some(message) => Err(SaveDefaultError::from(message)),
                    None => Ok(()),
                }
            });
            let commands = CommandRegistry::new();
            accounts.register(&commands).expect("commands register");
            Self {
                _dir: dir,
                server,
                host,
                commands,
                credentials,
                runtime,
                settings,
                served,
                saved,
            }
        }

        /// Run `/name` while `answer` plays the user's part in its dialogs.
        async fn run(&self, name: &str, answer: impl Future<Output = ()>) -> CommandOutcome {
            let invocation = CommandInvocation::new(name, "");
            let (outcome, ()) = tokio::join!(self.commands.dispatch(&invocation), answer);
            outcome
        }

        /// Send a request for `model` and return why it did not succeed.
        async fn request(&self, model: &str) -> GenerationError {
            let request = LlmRequest {
                messages: vec![Message::user("hello")],
                ..Default::default()
            };
            self.server
                .get_global::<ModelRegistry>()
                .expect("the model registry is installed")
                .llm(model)
                .expect("the model resolves")
                .generate(request)
                .await
                .err()
                .expect("the test connections never succeed")
        }

        fn served(&self) -> Vec<&'static str> {
            self.served.lock().expect("log lock").clone()
        }

        fn saved(&self) -> Vec<String> {
            self.saved.lock().expect("saved lock").clone()
        }

        /// Pick Anthropic in the welcome dialog and enter `key`.
        async fn choose_anthropic(&self, key: &str) {
            wait_for(&self.host, "Welcome to Polaris Code").await;
            press(&self.host, KeyCode::Char('3'));
            wait_for(&self.host, "Anthropic API key").await;
            enter(&self.host, key).await;
        }

        /// Read the failure screen, then dismiss it and the dialog behind it.
        async fn abandon_after_failure(&self) -> String {
            let screen = wait_for(&self.host, "Couldn’t connect").await;
            press(&self.host, KeyCode::Esc);
            screen
        }

        /// Assert the session still runs on its original `OpenAI` connection.
        async fn assert_unchanged(&self) {
            assert_eq!(self.runtime.report().provider(), Provider::OpenAi);
            assert_eq!(
                self.settings.snapshot(),
                SettingsSnapshot {
                    model_id: "openai/m1".into(),
                    permission_mode: PermissionMode::AutoEdit,
                },
                "the model and permissions are untouched"
            );
            assert_eq!(
                self.settings.catalog().models(),
                catalog_for(Provider::OpenAi).models(),
                "the catalog is untouched"
            );
            assert!(self.saved().is_empty(), "no defaults were saved");
            assert!(
                self.credentials
                    .api_key(Provider::Anthropic)
                    .expect("credentials are readable")
                    .is_none(),
                "no rejected key was saved"
            );
            let before = self.served().len();
            assert!(matches!(
                self.request("openai/m1").await,
                GenerationError::UnsupportedOperation(_)
            ));
            assert_eq!(
                self.served()[before..],
                ["openai"],
                "the next request still goes to the original connection"
            );
        }
    }

    fn render(host: &DialogHost) -> String {
        let (width, height) = (90, 25);
        let mut terminal = Terminal::new(TestBackend::new(width, height))
            .expect("test backend creates a terminal");
        terminal
            .draw(|frame| {
                host.render(frame, frame.area());
            })
            .expect("dialog host draws into the test backend");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn wait_for(host: &DialogHost, heading: &str) -> String {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let screen = render(host);
                if screen.lines().any(|line| line.trim() == heading) {
                    return screen;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the dialog titled {heading:?} never appeared"))
    }

    fn press(host: &DialogHost, code: KeyCode) {
        host.handle_event(&TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    async fn enter(host: &DialogHost, text: &str) {
        host.handle_event(&TuiEvent::Paste(text.into()));
        // A preceding dialog answer guards confirmation until typing has paused.
        tokio::time::sleep(Duration::from_millis(450)).await;
        press(host, KeyCode::Enter);
    }

    #[tokio::test]
    async fn login_sends_the_next_request_to_the_new_connection_and_keeps_session_settings() {
        let session = Session::start(Options::default()).await;
        assert!(matches!(
            session.request("openai/m1").await,
            GenerationError::UnsupportedOperation(_)
        ));
        assert_eq!(session.served(), ["openai"], "the first request is served");

        let outcome = session
            .run("login", session.choose_anthropic("new-key"))
            .await;

        let CommandOutcome::Reply(reply) = outcome else {
            panic!("a completed login replies: {outcome:?}");
        };
        assert!(reply.contains("Model: anthropic/m1"), "{reply}");
        assert!(!reply.contains("could not be saved"), "{reply}");
        assert_eq!(session.runtime.report().provider(), Provider::Anthropic);
        assert_eq!(
            session.settings.snapshot(),
            SettingsSnapshot {
                model_id: "anthropic/m1".into(),
                permission_mode: PermissionMode::AutoEdit,
            },
            "the model follows the connection; the permission mode stays"
        );
        assert_eq!(
            session.settings.catalog().models(),
            catalog_for(Provider::Anthropic).models(),
            "the model chooser now offers the new connection's models"
        );
        assert_eq!(session.saved(), ["anthropic/m1"], "defaults are saved once");
        assert_eq!(
            session
                .credentials
                .api_key(Provider::Anthropic)
                .expect("credentials are readable")
                .as_deref(),
            Some("new-key"),
            "the entered key is saved"
        );

        assert!(matches!(
            session.request("anthropic/m1").await,
            GenerationError::UnsupportedOperation(_)
        ));
        assert_eq!(
            session.served(),
            ["openai", "anthropic"],
            "the next request goes to the new connection"
        );
        assert!(
            matches!(
                session.request("openai/m1").await,
                GenerationError::InvalidRequest(message) if message.contains("inactive")
            ),
            "the old connection no longer serves"
        );
        assert_eq!(session.served().len(), 2, "the refusal reached no provider");
    }

    #[tokio::test]
    async fn cancelled_login_keeps_the_connection() {
        let session = Session::start(Options::default()).await;
        let outcome = session
            .run("login", async {
                wait_for(&session.host, "Welcome to Polaris Code").await;
                press(&session.host, KeyCode::Esc);
            })
            .await;

        assert_eq!(
            outcome,
            CommandOutcome::Reply("Connection setup cancelled. Your session is unchanged.".into())
        );
        session.assert_unchanged().await;
    }

    #[tokio::test]
    async fn login_refused_by_the_spend_guard_keeps_the_connection() {
        let session = Session::start(Options {
            require_price: true,
            ..Options::default()
        })
        .await;
        let outcome = session
            .run("login", async {
                session.choose_anthropic("rejected-key").await;
                let screen = session.abandon_after_failure().await;
                assert!(screen.contains("unpriced"), "the refusal is explained");
            })
            .await;

        assert!(
            matches!(&outcome, CommandOutcome::Reply(reply) if reply.contains("cancelled")),
            "abandoning the retry dialog cancels: {outcome:?}"
        );
        session.assert_unchanged().await;
    }

    #[tokio::test]
    async fn login_to_a_provider_without_a_route_keeps_the_connection() {
        let session = Session::start(Options {
            single_route: true,
            ..Options::default()
        })
        .await;
        let outcome = session
            .run("login", async {
                session.choose_anthropic("rejected-key").await;
                let screen = session.abandon_after_failure().await;
                assert!(
                    screen.contains("no model route"),
                    "the refusal names the cause"
                );
            })
            .await;

        assert!(
            matches!(&outcome, CommandOutcome::Reply(reply) if reply.contains("cancelled")),
            "abandoning the retry dialog cancels: {outcome:?}"
        );
        session.assert_unchanged().await;
    }

    #[tokio::test]
    async fn failed_defaults_save_is_reported_and_the_new_connection_stays_active() {
        let session = Session::start(Options {
            save_error: Some("disk full"),
            ..Options::default()
        })
        .await;
        let outcome = session
            .run("login", session.choose_anthropic("new-key"))
            .await;

        let CommandOutcome::Reply(reply) = outcome else {
            panic!("a save failure is reported in a reply: {outcome:?}");
        };
        assert!(
            reply.contains(
                "Connection active for this session; defaults could not be saved: disk full"
            ),
            "{reply}"
        );
        assert_eq!(session.saved(), ["anthropic/m1"], "the save was attempted");
        assert_eq!(session.runtime.report().provider(), Provider::Anthropic);
        assert_eq!(session.settings.snapshot().model_id, "anthropic/m1");
        assert!(matches!(
            session.request("anthropic/m1").await,
            GenerationError::UnsupportedOperation(_)
        ));
        assert_eq!(session.served(), ["anthropic"]);
    }

    #[tokio::test]
    async fn logout_disconnects_and_forgets_the_saved_key() {
        let session = Session::start(Options::default()).await;

        let outcome = session.run("logout", async {}).await;

        let CommandOutcome::Reply(reply) = outcome else {
            panic!("logout replies: {outcome:?}");
        };
        assert!(reply.contains("Saved credentials removed"), "{reply}");
        assert!(!session.runtime.is_connected(), "the session is signed out");
        assert!(
            session
                .credentials
                .api_key(Provider::OpenAi)
                .expect("credentials are readable")
                .is_none(),
            "the saved key is removed"
        );
        assert!(
            matches!(
                session.request("openai/m1").await,
                GenerationError::Auth(message) if message.contains("no connection")
            ),
            "requests fail closed after logout"
        );
        assert!(session.served().is_empty(), "no request reached a provider");
        assert_eq!(
            connection_status(&session.runtime),
            "Signed out · /login",
            "the status row points at /login"
        );

        let again = session.run("logout", async {}).await;
        assert!(
            matches!(&again, CommandOutcome::Reply(reply) if reply.contains("already signed out")),
            "a second logout says so: {again:?}"
        );
    }
}
