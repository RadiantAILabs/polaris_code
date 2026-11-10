//! In-session account commands for connecting, signing out, and viewing usage.

use std::sync::Arc;

use polaris::models::pricing::PriceStore;
use polaris_tui::commands::CommandSpec;
use polaris_tui::dialogs::DialogChoice;

use super::{CommandError, CommandOutcome, CommandRegistry, SessionSettings};
use crate::connection::ConnectionRuntime;
use crate::onboarding::prompts::{choose, open_usage};
use crate::onboarding::{SetupError, SetupResult, SetupService};
use crate::providers::Provider;
use crate::usage::TokenMeter;

type SaveDefaults = Arc<dyn Fn(&SetupResult) -> Result<(), String> + Send + Sync>;

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
    #[must_use]
    pub fn with_defaults(
        mut self,
        save: impl Fn(&SetupResult) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.save = Arc::new(save);
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
        let validate = |_ready: &crate::providers::ReadyProvider,
                        model: &str,
                        catalog: &crate::config::ModelCatalog| {
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
                if let Err(error) = self
                    .settings
                    .set_connection(result.model.clone(), result.catalog.clone())
                {
                    return CommandOutcome::Error(error.to_string());
                }
                self.runtime.activate(&result.ready);
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
