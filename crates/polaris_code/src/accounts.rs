//! `polaris login`, `polaris logout`, `polaris login status` and `/login`.
//!
//! The way to give polaris credentials for a provider without exporting a
//! variable in every shell. `login` saves what the provider needs
//! ([`crate::credentials`]) and makes that provider the default; `logout`
//! forgets it; `login status` says which providers have credentials and where
//! they come from. Nothing here prints a key, except its last four characters
//! in `status` and in the confirmation after saving one.
//!
//! These commands run before the rest of startup, so a settings file that
//! fails to load cannot stop someone from fixing their credentials. They write
//! to standard error — they are dialogue, not output a script reads — and take
//! everything they touch as a parameter ([`Setup`], [`AccountIo`]), so each
//! step is exercised by tests without a terminal, a network or a home
//! directory.
//!
//! A saved key is *not* checked against the provider: the framework has no
//! free request to check one with, and a new HTTP client for the purpose would
//! duplicate the provider's. The first request reports a wrong key. Bedrock has
//! no key, so `login bedrock` does check — it runs the same credential check
//! every Bedrock run starts with — and saves nothing when it fails.

use std::ffi::OsString;
use std::future::Future;
use std::io::{self, BufRead as _, IsTerminal as _, Read as _, Write as _};
use std::pin::Pin;

use clap::{Args, Parser, Subcommand, ValueEnum as _};
use polaris_tui::commands::CommandSpec;

use crate::config_dir::CONFIG_DIR_VAR;
use crate::credentials::{CredentialError, CredentialStore, StoredCredentials, mask_key};
use crate::providers::{Lookup, Provider, ProviderReport, StartupError, prepare};
use crate::terminal::{CommandError, CommandOutcome, CommandRegistry};

/// What `polaris login` prints and asks, and where the answers come from.
pub trait AccountIo {
    /// Show one line to the person at the terminal.
    fn say(&mut self, text: &str);

    /// Ask a question and read the one-line answer, shown as it is typed.
    ///
    /// # Errors
    ///
    /// [`io::Error`] when there is no answer to read.
    fn ask(&mut self, prompt: &str) -> io::Result<String>;

    /// Ask for a secret and read it without showing what is typed.
    ///
    /// # Errors
    ///
    /// [`io::Error`] when there is no terminal to ask on, or the person cancels.
    fn ask_secret(&mut self, prompt: &str) -> io::Result<String>;

    /// Everything on standard input, for `--with-api-key`.
    ///
    /// # Errors
    ///
    /// [`io::Error`] when standard input cannot be read.
    fn read_stdin(&mut self) -> io::Result<String>;
}

/// The check a Bedrock login runs, given a region and an optional profile: the
/// same credential check every Bedrock run starts with.
pub type BedrockCheck<'a> =
    &'a dyn Fn(String, Option<String>) -> Pin<Box<dyn Future<Output = BedrockOutcome>>>;

/// What a [`BedrockCheck`] answers.
pub type BedrockOutcome = Result<ProviderReport, StartupError>;

/// A configuration writer's original error, retained without flattening its
/// type or source chain to text.
pub type SaveDefaultError = Box<dyn std::error::Error + Send + Sync>;

/// Everything an account command reads or writes besides the terminal.
pub struct Setup<'a> {
    /// The process environment.
    pub env: Lookup<'a>,
    /// The `.env` file in the launch folder, as names and values. It is read
    /// for `status` only; a normal run has already put it in the environment.
    pub dotenv: Lookup<'a>,
    /// The credentials file, or `None` when no user directory can be found.
    pub store: Option<&'a CredentialStore>,
    /// Save a setting in the user configuration file, as `key`, `value`.
    pub save_default: &'a dyn Fn(&str, &str) -> Result<(), SaveDefaultError>,
    /// The credential check for Bedrock.
    pub check_bedrock: BedrockCheck<'a>,
}

impl std::fmt::Debug for Setup<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Setup")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Why an account command stopped. The text is ready to print.
#[derive(Debug, Clone)]
pub struct AccountError {
    message: String,
    source: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
}

impl AccountError {
    fn message(message: String) -> Self {
        Self {
            message,
            source: None,
        }
    }

    fn caused_by(message: String, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            message,
            source: Some(std::sync::Arc::new(source)),
        }
    }
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AccountError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

impl From<CredentialError> for AccountError {
    fn from(err: CredentialError) -> Self {
        Self::caused_by(err.to_string(), err)
    }
}

impl From<io::Error> for AccountError {
    fn from(err: io::Error) -> Self {
        Self::caused_by(err.to_string(), err)
    }
}

/// The account commands.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Account {
    /// Save credentials for a provider and make it the default. With no
    /// provider, asks which. `polaris login status` shows what is set up.
    #[command(args_conflicts_with_subcommands = true)]
    Login(Login),
    /// Forget the credentials `polaris login` saved: one provider's, or all.
    /// An environment variable that holds a key is not touched.
    Logout {
        /// Provider to forget; all of them when omitted.
        #[arg(value_enum)]
        provider: Option<Provider>,
    },
}

/// Arguments of `polaris login`.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct Login {
    /// Provider to log in to: `anthropic`, `openai` or `bedrock`.
    #[arg(value_enum)]
    pub provider: Option<Provider>,
    /// Read the API key from standard input instead of asking for it, for
    /// scripts: `printf %s "$KEY" | polaris login openai --with-api-key`.
    #[arg(long)]
    pub with_api_key: bool,
    /// What to do instead of logging in.
    #[command(subcommand)]
    pub action: Option<LoginAction>,
}

/// What `polaris login` can do other than log in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum LoginAction {
    /// Show which providers have credentials, and where each comes from.
    Status,
}

/// Parses an account command on its own, ahead of the rest of the command line.
#[derive(Debug, Parser)]
#[command(name = "polaris", bin_name = "polaris")]
struct AccountCli {
    #[command(subcommand)]
    command: Account,
}

/// Whether `argv` (program name first) is an account command: its first
/// argument is `login` or `logout`.
///
/// A working directory named `login` is reached as `./login`.
#[must_use]
pub fn is_account_command(argv: &[OsString]) -> bool {
    argv.get(1)
        .is_some_and(|first| first.as_os_str() == "login" || first.as_os_str() == "logout")
}

/// Run the account command in `argv` against the real terminal, environment and
/// credentials file, and return the process exit code: `0` on success, `1` when
/// it fails, `2` for a command line it cannot read.
///
/// `save_default` writes one setting to the user configuration file.
pub async fn run_from_args(
    argv: Vec<OsString>,
    save_default: &dyn Fn(&str, &str) -> Result<(), SaveDefaultError>,
) -> i32 {
    let account = match AccountCli::try_parse_from(argv) {
        Ok(cli) => cli.command,
        Err(err) => {
            let code = err.exit_code();
            let _ = err.print();
            return code;
        }
    };
    let store = CredentialStore::user();
    let dotenv = dotenv_lookup();
    let env = crate::providers::process_env;
    let check_bedrock = |region: String,
                         profile: Option<String>|
     -> Pin<Box<dyn Future<Output = BedrockOutcome>>> {
        Box::pin(async move {
            prepare(
                Provider::Bedrock,
                Some(&region),
                &env,
                &ProfileOnly(profile),
            )
            .await
            .map(|ready| ready.report().clone())
        })
    };
    let setup = Setup {
        env: &env,
        dotenv: &dotenv,
        store: store.as_ref(),
        save_default,
        check_bedrock: &check_bedrock,
    };
    match run(account, &mut TerminalIo, &setup).await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("polaris: {err}");
            1
        }
    }
}

/// Run one account command.
///
/// # Errors
///
/// [`AccountError`] with a message ready to print.
pub async fn run(
    account: Account,
    io: &mut dyn AccountIo,
    setup: &Setup<'_>,
) -> Result<(), AccountError> {
    match account {
        Account::Login(Login {
            action: Some(LoginAction::Status),
            ..
        }) => {
            status(io, setup);
            Ok(())
        }
        Account::Login(Login {
            provider,
            with_api_key,
            action: None,
        }) => {
            let provider = match provider {
                Some(provider) => provider,
                None => pick_provider(io)?,
            };
            match provider {
                Provider::Bedrock if with_api_key => Err(AccountError::message(
                    "Bedrock has no API key; it uses your AWS credentials. Run \
                     `polaris login bedrock` without --with-api-key."
                        .to_string(),
                )),
                Provider::Bedrock => login_bedrock(io, setup).await,
                Provider::Anthropic | Provider::OpenAi => {
                    login_with_key(provider, with_api_key, io, setup)
                }
            }
        }
        Account::Logout { provider } => logout(provider, io, setup),
    }
}

/// The store, or the reason there is none.
fn store<'a>(setup: &Setup<'a>) -> Result<&'a CredentialStore, AccountError> {
    setup.store.ok_or_else(|| {
        AccountError::message(format!(
            "cannot locate the user configuration directory: set {CONFIG_DIR_VAR}, \
             XDG_CONFIG_HOME or HOME"
        ))
    })
}

/// Ask which provider, by number or name.
fn pick_provider(io: &mut dyn AccountIo) -> Result<Provider, AccountError> {
    io.say("Which provider do you want to use?");
    for (index, provider) in Provider::ALL.into_iter().enumerate() {
        io.say(&format!("  {}) {}", index + 1, provider.name()));
    }
    let answer = io.ask(&format!(
        "Choose 1-{}, or type a name: ",
        Provider::ALL.len()
    ))?;
    let answer = answer.trim();
    if let Ok(number) = answer.parse::<usize>() {
        if let Some(provider) = number
            .checked_sub(1)
            .and_then(|index| Provider::ALL.get(index))
        {
            return Ok(*provider);
        }
    } else if let Ok(provider) = Provider::from_str(answer, true) {
        return Ok(provider);
    }
    Err(AccountError::message(format!(
        "`{answer}` is not a provider; choose 1-{} or one of: {}",
        Provider::ALL.len(),
        Provider::ALL.map(Provider::flag).join(", ")
    )))
}

/// Save the key for a provider that takes one, and make it the default.
fn login_with_key(
    provider: Provider,
    from_stdin: bool,
    io: &mut dyn AccountIo,
    setup: &Setup<'_>,
) -> Result<(), AccountError> {
    let store = store(setup)?;
    let name = provider.name();
    let key = if from_stdin {
        io.read_stdin()?
    } else {
        io.ask_secret(&format!("{name} API key (the input is hidden): "))?
    };
    store.set_api_key(provider, &key)?;
    io.say(&format!(
        "Saved the {name} key ({}) in {}, readable only by you.",
        mask_key(&key),
        store.path().display()
    ));
    io.say(&format!(
        "It has not been checked with {name}; if it is wrong, the first request says so."
    ));
    if let Some(var) = provider.key_var()
        && (setup.env)(var).is_some_and(|value| !value.trim().is_empty())
    {
        io.say(&format!(
            "Note: {var} is set in this environment and is used instead of the saved key."
        ));
    }
    make_default(provider, io, setup);
    Ok(())
}

/// Ask for a region and profile, check that the AWS chain gives credentials,
/// and only then save anything.
async fn login_bedrock(io: &mut dyn AccountIo, setup: &Setup<'_>) -> Result<(), AccountError> {
    let store = store(setup)?;
    let known_region = ["POLARIS_CODE_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"]
        .into_iter()
        .find_map(|var| (setup.env)(var).filter(|value| !value.trim().is_empty()));
    let region = match &known_region {
        Some(region) => io.ask(&format!("AWS region [{}]: ", region.trim()))?,
        None => io.ask("AWS region (for example us-east-1): ")?,
    };
    let region = match (region.trim(), &known_region) {
        ("", Some(known)) => known.trim().to_string(),
        ("", None) => {
            return Err(AccountError::message(
                "a region is needed, for example us-east-1".to_string(),
            ));
        }
        (given, _) => given.to_string(),
    };
    let profile = io.ask("AWS profile name, or Enter to use the default credential chain: ")?;
    let profile = Some(profile.trim().to_string()).filter(|profile| !profile.is_empty());

    let report = (setup.check_bedrock)(region.clone(), profile.clone())
        .await
        .map_err(|err| AccountError::caused_by(format!("{err}\n\nNothing was saved."), err))?;
    io.say(&report.line());

    match &profile {
        Some(profile) => store.set_aws_profile(profile)?,
        None => {
            store.remove(Provider::Bedrock)?;
        }
    }
    if let Err(err) = (setup.save_default)("region", &region) {
        io.say(&format!("Could not save the region as your default: {err}"));
    }
    io.say(&format!(
        "Saved region {region}{} for AWS Bedrock.",
        profile
            .as_deref()
            .map_or_else(String::new, |profile| format!(" and profile `{profile}`"))
    ));
    if let Some(var) = ["AWS_PROFILE"]
        .into_iter()
        .find(|var| (setup.env)(var).is_some_and(|value| !value.trim().is_empty()))
    {
        io.say(&format!(
            "Note: {var} is set in this environment and is used instead of the saved profile."
        ));
    }
    make_default(Provider::Bedrock, io, setup);
    Ok(())
}

/// Make `provider` the default, and say how it ended either way.
fn make_default(provider: Provider, io: &mut dyn AccountIo, setup: &Setup<'_>) {
    match (setup.save_default)("provider", provider.flag()) {
        Ok(()) => io.say(&format!(
            "{} is now your default provider. Run `polaris` to start.",
            provider.name()
        )),
        Err(err) => io.say(&format!(
            "Could not make {} your default provider: {err}\nPass --provider {} to use it.",
            provider.name(),
            provider.flag()
        )),
    }
}

/// Forget stored credentials for one provider or all.
fn logout(
    provider: Option<Provider>,
    io: &mut dyn AccountIo,
    setup: &Setup<'_>,
) -> Result<(), AccountError> {
    let store = store(setup)?;
    let providers = provider.map_or_else(|| Provider::ALL.to_vec(), |provider| vec![provider]);
    for provider in providers {
        let what = if provider == Provider::Bedrock {
            "profile"
        } else {
            "key"
        };
        if store.remove(provider)? {
            io.say(&format!("Removed the stored {} {what}.", provider.name()));
        } else if provider.key_var().is_some() || provider == Provider::Bedrock {
            io.say(&format!("No stored {} {what}.", provider.name()));
        }
        if let Some(var) = provider.key_var()
            && (setup.env)(var).is_some_and(|value| !value.trim().is_empty())
        {
            io.say(&format!(
                "{var} is still set in this environment; unset it to stop using that key."
            ));
        }
    }
    Ok(())
}

/// Say which providers have credentials and where each comes from.
fn status(io: &mut dyn AccountIo, setup: &Setup<'_>) {
    match setup.store {
        Some(store) => {
            let source = if (setup.env)(CONFIG_DIR_VAR).is_some_and(|value| !value.is_empty()) {
                CONFIG_DIR_VAR
            } else if (setup.env)("XDG_CONFIG_HOME").is_some_and(|value| !value.is_empty()) {
                "XDG_CONFIG_HOME"
            } else {
                "the home directory"
            };
            io.say(&format!(
                "Credentials file: {} ({}, from {source})",
                store.path().display(),
                if store.path().exists() {
                    "exists"
                } else {
                    "not created yet"
                }
            ));
        }
        None => io.say(&format!(
            "Credentials file: none — set {CONFIG_DIR_VAR}, XDG_CONFIG_HOME or HOME to give \
             polaris a place for it"
        )),
    }
    for provider in Provider::ALL {
        let line = match provider.key_var() {
            Some(var) => key_status(provider, var, setup),
            None => bedrock_status(setup),
        };
        io.say(&line);
    }
}

fn key_status(provider: Provider, var: &str, setup: &Setup<'_>) -> String {
    let name = provider.flag();
    let found = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
    if let Some(value) = found((setup.env)(var)) {
        return format!(
            "{name:<10} key from {var} in the environment, ending {}",
            mask_key(&value)
        );
    }
    if let Some(value) = found((setup.dotenv)(var)) {
        return format!(
            "{name:<10} key from {var} in .env, ending {}",
            mask_key(&value)
        );
    }
    match setup.store.map(|store| store.api_key(provider)) {
        Some(Ok(Some(value))) => format!("{name:<10} stored key, ending {}", mask_key(&value)),
        Some(Err(err)) => format!("{name:<10} the credentials file cannot be used: {err}"),
        _ => format!("{name:<10} no key; run `polaris login {name}` or set {var}"),
    }
}

fn bedrock_status(setup: &Setup<'_>) -> String {
    let region = ["POLARIS_CODE_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"]
        .into_iter()
        .find_map(|var| {
            (setup.env)(var)
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("{} (from {var})", value.trim()))
        })
        .unwrap_or_else(|| "not set".to_string());
    let profile = match (setup.env)("AWS_PROFILE").filter(|value| !value.trim().is_empty()) {
        Some(profile) => format!("{} (from AWS_PROFILE)", profile.trim()),
        None => match setup.store.map(StoredCredentials::aws_profile) {
            Some(Ok(Some(profile))) => format!("{profile} (stored)"),
            Some(Err(err)) => format!("unreadable ({err})"),
            _ => "none".to_string(),
        },
    };
    format!(
        "{:<10} no key; uses the AWS credential chain. Region: {region}. Profile: {profile}. \
         `polaris login bedrock` checks them.",
        Provider::Bedrock.flag()
    )
}

/// A [`StoredCredentials`] that holds only a profile, for checking Bedrock
/// with a profile that is not saved yet.
struct ProfileOnly(Option<String>);

impl StoredCredentials for ProfileOnly {
    fn api_key(&self, _provider: Provider) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }

    fn aws_profile(&self) -> Result<Option<String>, CredentialError> {
        Ok(self.0.clone())
    }
}

/// The names and values in `./.env`, without putting them in the environment.
fn dotenv_lookup() -> impl Fn(&str) -> Option<String> {
    let entries: std::collections::BTreeMap<String, String> = std::env::current_dir()
        .ok()
        .and_then(|dir| dotenvy::from_path_iter(dir.join(".env")).ok())
        .map(|entries| entries.flatten().collect())
        .unwrap_or_default();
    move |name| entries.get(name).cloned()
}

/// The `/login` command: where to go to change credentials.
///
/// # Errors
///
/// [`CommandError`] when `/login` is already registered.
pub fn register_login_command(commands: &CommandRegistry) -> Result<(), CommandError> {
    commands.register_sync(
        CommandSpec::new("login", "How to set up or change provider credentials"),
        |_, _| {
            CommandOutcome::Reply(
                "Credentials are set outside a session. Quit, run `polaris login` (or \
                 `polaris login anthropic|openai|bedrock`), then start polaris again. \
                 `polaris login status` shows which providers have credentials and where \
                 they come from. This session keeps the credentials it started with."
                    .to_string(),
            )
        },
    )
}

/// The real terminal: prompts and output on standard error, answers from
/// standard input, secrets read without echo.
#[derive(Debug)]
pub struct TerminalIo;

impl AccountIo for TerminalIo {
    fn say(&mut self, text: &str) {
        eprintln!("{text}");
    }

    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        eprint!("{prompt}");
        io::stderr().flush()?;
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "no answer on standard input",
            ));
        }
        Ok(line)
    }

    fn ask_secret(&mut self, prompt: &str) -> io::Result<String> {
        read_hidden(prompt)
    }

    fn read_stdin(&mut self) -> io::Result<String> {
        let mut text = String::new();
        io::stdin().lock().read_to_string(&mut text)?;
        Ok(text)
    }
}

/// Read one line from the terminal without showing it.
fn read_hidden(prompt: &str) -> io::Result<String> {
    use polaris_tui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use polaris_tui::crossterm::terminal;

    /// Puts the terminal back however the loop below ends.
    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }

    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "standard input is not a terminal, so the key cannot be asked for; pass it on \
             standard input with --with-api-key",
        ));
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut secret = String::new();
    let outcome = {
        terminal::enable_raw_mode()?;
        let _restore = RawMode;
        loop {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Char('c' | 'd') if control => {
                    break Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                KeyCode::Backspace => {
                    secret.pop();
                }
                KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                    secret.push(c);
                }
                _ => {}
            }
        }
    };
    eprintln!();
    outcome.map(|()| secret)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    use super::*;

    /// A scripted terminal: queued answers in, everything said out.
    #[derive(Default)]
    struct Script {
        answers: VecDeque<String>,
        secrets: VecDeque<String>,
        stdin: String,
        said: Vec<String>,
    }

    impl Script {
        /// A terminal whose standard input holds `text`.
        fn piping(text: &str) -> Self {
            Self {
                stdin: text.to_string(),
                ..Self::default()
            }
        }

        fn said(&self) -> String {
            self.said.join("\n")
        }
    }

    impl AccountIo for Script {
        fn say(&mut self, text: &str) {
            self.said.push(text.to_string());
        }

        fn ask(&mut self, prompt: &str) -> io::Result<String> {
            self.said.push(format!("? {prompt}"));
            self.answers
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no answer"))
        }

        fn ask_secret(&mut self, prompt: &str) -> io::Result<String> {
            self.said.push(format!("? {prompt}"));
            self.secrets
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no terminal"))
        }

        fn read_stdin(&mut self) -> io::Result<String> {
            Ok(self.stdin.clone())
        }
    }

    #[tokio::test]
    async fn invalid_account_arguments_return_an_exit_code() {
        let save = |_: &str, _: &str| -> Result<(), SaveDefaultError> {
            panic!("parsing must fail first")
        };
        assert_eq!(
            run_from_args(
                vec!["polaris".into(), "login".into(), "--invalid-option".into()],
                &save
            )
            .await,
            2
        );
        assert_eq!(
            run_from_args(
                vec!["polaris".into(), "login".into(), "--help".into()],
                &save
            )
            .await,
            0
        );
    }

    #[test]
    fn account_errors_retain_typed_causes() {
        use std::error::Error as _;
        let error = AccountError::from(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let error = AccountError::from(CredentialError::InvalidValue {
            what: "API key",
            reason: "is blank",
        });
        assert!(error.source().unwrap().is::<CredentialError>());
    }

    /// The outside world of one test: environment, `.env`, a credentials file
    /// in a scratch directory, and a record of what was saved as a default.
    struct World {
        dir: tempfile::TempDir,
        env: BTreeMap<String, String>,
        dotenv: BTreeMap<String, String>,
        saved: RefCell<Vec<(String, String)>>,
        save_fails: bool,
        bedrock: RefCell<Vec<(String, Option<String>)>>,
        bedrock_result: Result<ProviderReport, StartupError>,
        has_store: bool,
    }

    impl World {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("a scratch directory"),
                env: BTreeMap::new(),
                dotenv: BTreeMap::new(),
                saved: RefCell::new(Vec::new()),
                save_fails: false,
                bedrock: RefCell::new(Vec::new()),
                bedrock_result: Err(StartupError::NoRegion),
                has_store: true,
            }
        }

        fn store(&self) -> CredentialStore {
            CredentialStore::at(self.dir.path().join("polaris").join("credentials.toml"))
        }

        async fn run(&self, account: Account, io: &mut Script) -> Result<(), AccountError> {
            let store = self.store();
            let env = |name: &str| self.env.get(name).cloned();
            let dotenv = |name: &str| self.dotenv.get(name).cloned();
            let save = |key: &str, value: &str| {
                if self.save_fails {
                    return Err(io::Error::other("the configuration file is not valid").into());
                }
                self.saved
                    .borrow_mut()
                    .push((key.to_string(), value.to_string()));
                Ok(())
            };
            let check = |region: String,
                         profile: Option<String>|
             -> Pin<Box<dyn Future<Output = BedrockOutcome>>> {
                self.bedrock.borrow_mut().push((region, profile));
                let result = self.bedrock_result.clone();
                Box::pin(async move { result })
            };
            let setup = Setup {
                env: &env,
                dotenv: &dotenv,
                store: self.has_store.then_some(&store),
                save_default: &save,
                check_bedrock: &check,
            };
            run(account, io, &setup).await
        }

        fn saved(&self) -> Vec<(String, String)> {
            self.saved.borrow().clone()
        }
    }

    fn login(provider: Option<Provider>, with_api_key: bool) -> Account {
        Account::Login(Login {
            provider,
            with_api_key,
            action: None,
        })
    }

    fn parse(args: &[&str]) -> Result<Account, clap::Error> {
        let mut argv = vec!["polaris"];
        argv.extend(args);
        AccountCli::try_parse_from(argv).map(|cli| cli.command)
    }

    #[tokio::test]
    async fn login_saves_the_hidden_key_and_makes_the_provider_the_default() {
        let world = World::new();
        let mut io = Script::default();
        io.secrets
            .push_back("sk-ant-api03-abcdefgh1234".to_string());

        world
            .run(login(Some(Provider::Anthropic), false), &mut io)
            .await
            .unwrap();

        assert_eq!(
            world
                .store()
                .api_key(Provider::Anthropic)
                .unwrap()
                .as_deref(),
            Some("sk-ant-api03-abcdefgh1234")
        );
        assert_eq!(
            world.saved(),
            [("provider".to_string(), "anthropic".to_string())]
        );
        let said = io.said();
        assert!(said.contains("…1234"), "{said}");
        assert!(
            !said.contains("sk-ant-api03"),
            "the key is never shown: {said}"
        );
        assert!(said.contains("has not been checked"), "{said}");
        assert!(said.contains("now your default provider"), "{said}");
    }

    #[tokio::test]
    async fn with_api_key_reads_standard_input_and_never_asks() {
        let world = World::new();
        let mut io = Script::piping("sk-openai-from-a-pipe-9999\n");

        world
            .run(login(Some(Provider::OpenAi), true), &mut io)
            .await
            .unwrap();

        assert_eq!(
            world.store().api_key(Provider::OpenAi).unwrap().as_deref(),
            Some("sk-openai-from-a-pipe-9999")
        );
        assert!(
            io.said.iter().all(|line| !line.starts_with("? ")),
            "{}",
            io.said()
        );
        assert_eq!(
            world.saved(),
            [("provider".to_string(), "openai".to_string())]
        );
    }

    #[tokio::test]
    async fn an_unusable_key_is_refused_and_nothing_is_saved() {
        let world = World::new();
        for bad in ["", "   \n", "two words", "line one\nline two"] {
            let mut io = Script::piping(bad);
            let error = world
                .run(login(Some(Provider::OpenAi), true), &mut io)
                .await
                .expect_err(bad);
            assert!(error.to_string().contains("API key"), "{error}");
        }
        assert!(!world.store().path().exists());
        assert!(world.saved().is_empty());
    }

    #[tokio::test]
    async fn a_missing_terminal_for_the_hidden_prompt_is_an_error_that_names_the_way_out() {
        let world = World::new();
        let mut io = Script::default();
        let error = world
            .run(login(Some(Provider::Anthropic), false), &mut io)
            .await
            .expect_err("no secret could be read");
        assert!(error.to_string().contains("no terminal"), "{error}");
    }

    #[tokio::test]
    async fn a_key_already_in_the_environment_is_called_out() {
        let mut world = World::new();
        world
            .env
            .insert("ANTHROPIC_API_KEY".into(), "sk-ant-from-env".into());
        let mut io = Script::default();
        io.secrets.push_back("sk-ant-saved-key-0001".to_string());

        world
            .run(login(Some(Provider::Anthropic), false), &mut io)
            .await
            .unwrap();

        let said = io.said();
        assert!(said.contains("ANTHROPIC_API_KEY is set"), "{said}");
        assert!(said.contains("used instead of the saved key"), "{said}");
        assert!(!said.contains("sk-ant-from-env"), "{said}");
    }

    #[tokio::test]
    async fn a_saved_key_survives_a_failure_to_make_the_provider_the_default() {
        let mut world = World::new();
        world.save_fails = true;
        let mut io = Script::default();
        io.secrets
            .push_back("sk-ant-api03-abcdefgh1234".to_string());

        world
            .run(login(Some(Provider::Anthropic), false), &mut io)
            .await
            .unwrap();

        assert!(
            world
                .store()
                .api_key(Provider::Anthropic)
                .unwrap()
                .is_some()
        );
        let said = io.said();
        assert!(
            said.contains("Could not make Anthropic your default"),
            "{said}"
        );
        assert!(said.contains("--provider anthropic"), "{said}");
    }

    #[tokio::test]
    async fn without_a_user_directory_login_says_what_to_set() {
        let mut world = World::new();
        world.has_store = false;
        let mut io = Script::piping("sk-openai-from-a-pipe-9999");
        let error = world
            .run(login(Some(Provider::OpenAi), true), &mut io)
            .await
            .expect_err("nowhere to save");
        assert!(error.to_string().contains("POLARIS_CONFIG_DIR"), "{error}");
    }

    #[tokio::test]
    async fn no_provider_asks_with_a_numbered_picker() {
        for (answer, expected) in [
            ("2", Provider::OpenAi),
            (" 1 \n", Provider::Anthropic),
            ("bedrock", Provider::Bedrock),
            ("OpenAI", Provider::OpenAi),
        ] {
            let mut io = Script::default();
            io.answers.push_back(answer.to_string());
            assert_eq!(pick_provider(&mut io).unwrap(), expected, "{answer:?}");
            let said = io.said();
            assert!(said.contains("1) Anthropic"), "{said}");
            assert!(said.contains("2) OpenAI"), "{said}");
            assert!(said.contains("3) Amazon Bedrock"), "{said}");
        }
        for bad in ["0", "4", "gemini", ""] {
            let mut io = Script::default();
            io.answers.push_back(bad.to_string());
            let error = pick_provider(&mut io).expect_err(bad);
            assert!(
                error.to_string().contains("anthropic, openai, bedrock"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn login_without_a_provider_continues_with_the_one_picked() {
        let world = World::new();
        let mut io = Script::default();
        io.answers.push_back("2".to_string());
        io.secrets
            .push_back("sk-openai-picked-key-4321".to_string());

        world.run(login(None, false), &mut io).await.unwrap();

        assert!(world.store().api_key(Provider::OpenAi).unwrap().is_some());
        assert_eq!(
            world.saved(),
            [("provider".to_string(), "openai".to_string())]
        );
    }

    fn bedrock_report() -> ProviderReport {
        ProviderReport::new(
            Provider::Bedrock,
            Some("us-east-1".to_string()),
            None,
            "EC2 instance role",
        )
    }

    #[tokio::test]
    async fn bedrock_login_checks_first_then_saves_region_profile_and_default() {
        let mut world = World::new();
        world.bedrock_result = Ok(bedrock_report());
        let mut io = Script::default();
        io.answers.push_back("us-west-2".to_string());
        io.answers.push_back("work".to_string());

        world
            .run(login(Some(Provider::Bedrock), false), &mut io)
            .await
            .unwrap();

        assert_eq!(
            world.bedrock.borrow().as_slice(),
            [("us-west-2".to_string(), Some("work".to_string()))]
        );
        assert_eq!(
            world.store().aws_profile().unwrap().as_deref(),
            Some("work")
        );
        assert_eq!(
            world.saved(),
            [
                ("region".to_string(), "us-west-2".to_string()),
                ("provider".to_string(), "bedrock".to_string())
            ]
        );
        assert!(
            io.said().contains("bedrock · region us-east-1"),
            "{}",
            io.said()
        );
    }

    #[tokio::test]
    async fn bedrock_login_offers_the_region_it_already_knows_and_a_blank_profile_clears_one() {
        let mut world = World::new();
        world.env.insert("AWS_REGION".into(), "eu-west-1".into());
        world.bedrock_result = Ok(bedrock_report());
        world.store().set_aws_profile("old").unwrap();
        let mut io = Script::default();
        io.answers.push_back("\n".to_string());
        io.answers.push_back("\n".to_string());

        world
            .run(login(Some(Provider::Bedrock), false), &mut io)
            .await
            .unwrap();

        assert!(
            io.said().contains("AWS region [eu-west-1]"),
            "{}",
            io.said()
        );
        assert_eq!(world.bedrock.borrow()[0], ("eu-west-1".to_string(), None));
        assert_eq!(
            world.store().aws_profile().unwrap(),
            None,
            "a blank profile forgets the old one"
        );
    }

    #[tokio::test]
    async fn bedrock_login_that_cannot_resolve_credentials_saves_nothing() {
        let mut world = World::new();
        world.bedrock_result = Err(StartupError::NoCredentials {
            detail: "no providers in the chain".to_string(),
            source: None,
        });
        let mut io = Script::default();
        io.answers.push_back("us-east-1".to_string());
        io.answers.push_back("work".to_string());

        let error = world
            .run(login(Some(Provider::Bedrock), false), &mut io)
            .await
            .expect_err("no credentials");

        assert!(error.to_string().contains("aws sso login"), "{error}");
        assert!(error.to_string().contains("Nothing was saved"), "{error}");
        assert!(
            std::error::Error::source(&error).is_some_and(|source| source.is::<StartupError>())
        );
        assert!(world.saved().is_empty());
        assert!(!world.store().path().exists());
    }

    #[tokio::test]
    async fn bedrock_login_needs_a_region_and_takes_no_api_key() {
        let world = World::new();
        let mut io = Script::default();
        io.answers.push_back("  ".to_string());
        let error = world
            .run(login(Some(Provider::Bedrock), false), &mut io)
            .await
            .expect_err("no region given or known");
        assert!(error.to_string().contains("region"), "{error}");

        let error = world
            .run(login(Some(Provider::Bedrock), true), &mut Script::default())
            .await
            .expect_err("Bedrock has no key");
        assert!(error.to_string().contains("no API key"), "{error}");
    }

    #[tokio::test]
    async fn logout_forgets_one_provider_and_says_what_the_environment_still_holds() {
        let mut world = World::new();
        world
            .env
            .insert("OPENAI_API_KEY".into(), "sk-openai-env".into());
        world
            .store()
            .set_api_key(Provider::OpenAi, "sk-openai-stored-1")
            .unwrap();
        world
            .store()
            .set_api_key(Provider::Anthropic, "sk-ant-stored-1")
            .unwrap();
        let mut io = Script::default();

        world
            .run(
                Account::Logout {
                    provider: Some(Provider::OpenAi),
                },
                &mut io,
            )
            .await
            .unwrap();

        assert_eq!(world.store().api_key(Provider::OpenAi).unwrap(), None);
        assert!(
            world
                .store()
                .api_key(Provider::Anthropic)
                .unwrap()
                .is_some()
        );
        let said = io.said();
        assert!(said.contains("Removed the stored OpenAI key"), "{said}");
        assert!(said.contains("OPENAI_API_KEY is still set"), "{said}");
        assert!(!said.contains("sk-openai"), "{said}");
    }

    #[tokio::test]
    async fn logout_with_no_provider_forgets_everything() {
        let world = World::new();
        world
            .store()
            .set_api_key(Provider::OpenAi, "sk-openai-stored-1")
            .unwrap();
        world.store().set_aws_profile("work").unwrap();
        let mut io = Script::default();

        world
            .run(Account::Logout { provider: None }, &mut io)
            .await
            .unwrap();

        assert!(!world.store().path().exists());
        let said = io.said();
        assert!(said.contains("Removed the stored OpenAI key"), "{said}");
        assert!(
            said.contains("Removed the stored Amazon Bedrock profile"),
            "{said}"
        );
        assert!(said.contains("No stored Anthropic key"), "{said}");
    }

    #[tokio::test]
    async fn status_names_the_source_of_each_key_in_the_order_a_run_uses() {
        let mut world = World::new();
        world.env.insert(
            "ANTHROPIC_API_KEY".into(),
            "sk-ant-from-the-shell-1111".into(),
        );
        world
            .dotenv
            .insert("ANTHROPIC_API_KEY".into(), "sk-ant-from-dotenv-2222".into());
        world
            .dotenv
            .insert("OPENAI_API_KEY".into(), "sk-openai-from-dotenv-3333".into());
        world
            .store()
            .set_api_key(Provider::OpenAi, "sk-openai-stored-4444")
            .unwrap();
        world
            .env
            .insert("XDG_CONFIG_HOME".into(), "/home/u/.config".into());
        let mut io = Script::default();

        world
            .run(
                Account::Login(Login {
                    provider: None,
                    with_api_key: false,
                    action: Some(LoginAction::Status),
                }),
                &mut io,
            )
            .await
            .unwrap();

        let said = io.said();
        assert!(
            said.contains("anthropic  key from ANTHROPIC_API_KEY in the environment, ending …1111"),
            "{said}"
        );
        assert!(
            said.contains("openai     key from OPENAI_API_KEY in .env, ending …3333"),
            "{said}"
        );
        assert!(said.contains("from XDG_CONFIG_HOME"), "{said}");
        for secret in ["from-the-shell", "from-dotenv", "stored-4444"] {
            assert!(!said.contains(secret), "{secret} leaked: {said}");
        }
    }

    #[tokio::test]
    async fn status_reports_a_stored_key_a_missing_key_and_bedrock_settings() {
        let mut world = World::new();
        world
            .store()
            .set_api_key(Provider::Anthropic, "sk-ant-stored-5555-6666")
            .unwrap();
        world.store().set_aws_profile("work").unwrap();
        world.env.insert("AWS_REGION".into(), "us-east-1".into());
        let mut io = Script::default();

        world
            .run(
                Account::Login(Login {
                    provider: None,
                    with_api_key: false,
                    action: Some(LoginAction::Status),
                }),
                &mut io,
            )
            .await
            .unwrap();

        let said = io.said();
        assert!(
            said.contains("anthropic  stored key, ending …6666"),
            "{said}"
        );
        assert!(
            said.contains("no key; run `polaris login openai` or set OPENAI_API_KEY"),
            "{said}"
        );
        assert!(
            said.contains("Region: us-east-1 (from AWS_REGION)"),
            "{said}"
        );
        assert!(said.contains("Profile: work (stored)"), "{said}");
        assert!(said.contains("exists"), "{said}");
    }

    #[tokio::test]
    async fn status_says_so_when_there_is_no_place_for_a_credentials_file() {
        let mut world = World::new();
        world.has_store = false;
        let mut io = Script::default();
        world
            .run(
                Account::Login(Login {
                    provider: None,
                    with_api_key: false,
                    action: Some(LoginAction::Status),
                }),
                &mut io,
            )
            .await
            .unwrap();
        assert!(
            io.said().contains("Credentials file: none"),
            "{}",
            io.said()
        );
    }

    #[test]
    fn the_subcommands_parse_with_their_arguments() {
        assert_eq!(parse(&["login"]).unwrap(), login(None, false));
        assert_eq!(
            parse(&["login", "openai"]).unwrap(),
            login(Some(Provider::OpenAi), false)
        );
        assert_eq!(
            parse(&["login", "anthropic", "--with-api-key"]).unwrap(),
            login(Some(Provider::Anthropic), true)
        );
        assert_eq!(
            parse(&["login", "status"]).unwrap(),
            Account::Login(Login {
                provider: None,
                with_api_key: false,
                action: Some(LoginAction::Status)
            })
        );
        assert_eq!(
            parse(&["logout"]).unwrap(),
            Account::Logout { provider: None }
        );
        assert_eq!(
            parse(&["logout", "bedrock"]).unwrap(),
            Account::Logout {
                provider: Some(Provider::Bedrock)
            }
        );
        assert!(parse(&["login", "gemini"]).is_err());
        assert!(parse(&["login", "status", "openai"]).is_err());
    }

    #[test]
    fn only_login_and_logout_as_the_first_argument_are_account_commands() {
        let argv = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(is_account_command(&argv(&["polaris", "login"])));
        assert!(is_account_command(&argv(&["polaris", "logout", "openai"])));
        assert!(!is_account_command(&argv(&["polaris"])));
        assert!(!is_account_command(&argv(&["polaris", "./login"])));
        assert!(!is_account_command(&argv(&[
            "polaris",
            "--provider",
            "openai",
            "login"
        ])));
        assert!(!is_account_command(&argv(&["polaris", "-p", "login"])));
    }

    #[test]
    fn the_login_command_points_at_the_terminal_command() {
        let commands = CommandRegistry::with_builtins();
        register_login_command(&commands).unwrap();
        assert!(commands.specs().iter().any(|spec| spec.name == "login"));
        assert!(
            register_login_command(&commands).is_err(),
            "a second registration is refused rather than shadowing the first"
        );
    }

    #[test]
    fn the_dotenv_lookup_reads_names_without_touching_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "POLARIS_TEST_ONLY_KEY=value-1\n").unwrap();
        let entries: BTreeMap<String, String> =
            dotenvy::from_path_iter(&path).unwrap().flatten().collect();
        assert_eq!(
            entries.get("POLARIS_TEST_ONLY_KEY").map(String::as_str),
            Some("value-1")
        );
        assert!(std::env::var("POLARIS_TEST_ONLY_KEY").is_err());
    }
}
