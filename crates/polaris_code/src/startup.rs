//! Starting the `polaris` agent from a program of your own.
//!
//! The `polaris` binary is [`Host::new()`](Host::new) run on the process's
//! arguments, and nothing else. A program that wants the same agent with more
//! plugins added — extra permission rules, extra steps in the coding loop, its
//! own tools — builds a [`Host`], adds what it needs, and runs it, instead of
//! copying the startup code. Everything `polaris` does stays the same: the same
//! flags, configuration files, environment variables, providers, front ends and
//! exit codes.
//!
//! # What a host writes
//!
//! ```no_run
//! use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version, VersionReq};
//! use polaris::system::server::Server;
//! use polaris_code::agents::permission_rules::{
//!     PermissionRules, PermissionRulesPlugin, RuleDecision,
//! };
//! use polaris_code::startup::{Host, clap};
//!
//! /// Refuses edits to one file, named on the command line.
//! struct Guard {
//!     protected: String,
//! }
//!
//! impl Plugin for Guard {
//!     const ID: &'static str = "my_experiments::guard";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!
//!     fn dependencies(&self) -> Vec<PluginId> {
//!         vec![PluginId::of::<PermissionRulesPlugin>()]
//!     }
//!
//!     fn access(&self) -> PluginAccess {
//!         PluginAccess::new()
//!             .extends::<PermissionRules>(VersionReq::caret(PermissionRules::CONTRACT_VERSION))
//!     }
//!
//!     fn build(&self, server: &mut Server) {
//!         let protected = self.protected.clone();
//!         server
//!             .get_resource_mut::<PermissionRules>()
//!             .expect("PermissionRulesPlugin provides the rules")
//!             .add_code_rule("my_experiments.protect", move |tool, args, _root| {
//!                 let path = args.get("path")?.as_str()?;
//!                 (tool == "edit_file" && path == protected)
//!                     .then(|| (RuleDecision::Deny, format!("{path} is read-only in this run")))
//!             })
//!             .expect("the rule name is new and the server is still building");
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let code = Host::new()
//!         // A setting of the host's own; it appears in `--help` and can be read
//!         // from the environment as well as the command line.
//!         .arg(
//!             clap::Arg::new("protect")
//!                 .long("protect")
//!                 .env("MY_EXPERIMENTS_PROTECT")
//!                 .value_name("PATH")
//!                 .help("A file the agent may not edit"),
//!         )
//!         // Added after every plugin `polaris` adds, before the server starts.
//!         .plugins(|launch, server| {
//!             if let Some(path) = launch.matches().get_one::<String>("protect") {
//!                 server.add_plugins(Guard { protected: path.clone() });
//!             }
//!             Ok(())
//!         })
//!         .run(std::env::args_os())
//!         .await;
//!     // Exit here rather than returning: the agent's HTTP pools and graph
//!     // store can hold the runtime open after the work is finished.
//!     std::process::exit(code);
//! }
//! ```
//!
//! Adding steps to the coding loop works the same way; see
//! [`CodeAgentSteps`]. The startup builds the agent from the completed step
//! registry, so a host adds nothing to the graph itself, and the Graph tab shows
//! the added steps.
//!
//! # Plugin order
//!
//! Startup adds its own plugins in this order, then calls the closures given to
//! [`Host::plugins`] in the order they were added, then inserts the run's
//! globals and calls `Server::finish`:
//!
//! 1. the model provider: `ModelsPlugin` and the plugin for `--provider`;
//! 2. tools: `ToolsPlugin`, the MCP client, the file tools, the ontrace store,
//!    the code graph, memory, the shell, shell approval and background tasks;
//! 3. [`PermissionRulesPlugin`](crate::agents::permission_rules::PermissionRulesPlugin),
//!    [`AutoReviewPlugin`](crate::agents::auto_review::AutoReviewPlugin) and the
//!    web tool;
//! 4. `PersistencePlugin`, `SessionsPlugin`, then [`CodeAgentPlugin`] with the
//!    compaction and progress plugins;
//! 5. topology selection: the evidence and task-queue plugins, the catalog and
//!    the topology packs;
//! 6. one front end: [`ExecIOPlugin`] when the run is non-interactive;
//!    otherwise the approval plugin, the terminal UI, the Graph and Help tabs and
//!    the conversation panel.
//!
//! A host's plugins are therefore added after all of these. Polaris builds
//! plugins in an order that satisfies the dependencies each one declares, so a
//! host plugin that declares a dependency on any plugin above is built after it.
//! Do not rely on the order between plugins with no dependency between them:
//!
//! | To do this | Depend on | And use |
//! |------------|-----------|---------|
//! | Add a code permission rule | `PermissionRulesPlugin` | [`PermissionRules::add_code_rule`](crate::agents::permission_rules::PermissionRules::add_code_rule) |
//! | Add a step to the coding loop | `CodeAgentPlugin` | [`CodeAgentSteps`] |
//! | Register a tool | `ToolsPlugin` | the `ToolRegistry` |
//! | Register another model provider | `ModelsPlugin` | the `ModelRegistry` |
//!
//! A plugin that does not declare the dependency can be built before the
//! resource it extends exists. The rules and the steps are fixed when the server
//! is ready, so they can only be added while plugins build.
//!
//! The globals inserted after the closures run (the token meter, the provider
//! report and the run's limits) belong to startup; a host should not insert its
//! own values of those types.
//!
//! # Exit codes and output
//!
//! [`Host::run`] returns the exit code instead of exiting, and prints exactly
//! what the binary prints. Unattended runs use `0` completed, `1` failed, `2`
//! usage or configuration error, `130` interrupted and `3` for a reached
//! spending limit, as documented for `--print`. A refusal to start prints one
//! `polaris: …` line on stderr.
//!
//! # Lifecycle
//!
//! The credential commands (`login`, `logout`) run first and need no
//! configuration. Otherwise the command line and configuration files are
//! resolved, the run's settings are validated, the server is assembled and
//! started, the agent is registered, and a session is created or resumed. A
//! non-interactive run then drives one turn and returns; an interactive run
//! serves turns until the terminal UI quits or the process is signalled.
//! Plugin cleanup runs with a short bound before the code is returned.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ArgMatches;
use polaris::graph::GraphExecutor;
use polaris::models::ModelsPlugin;
use polaris::models::pricing::PriceStore;
use polaris::plugins::{IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{
    AgentTypeId, FileStore, SessionError, SessionId, SessionsAPI, SessionsPlugin,
};
use polaris::shell::ShellPlugin;
use polaris::system::param::SystemContext;
use polaris::system::server::Server;
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogPlugin};
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_ontrace::oxigraph::OxigraphStore;
use polaris_tui::panels::{ConversationPanelPlugin, LogBuffer};
use polaris_tui::{TuiConfig, TuiHandle, TuiPlugin};
use tokio::sync::mpsc;

use crate::accounts;
use crate::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, CodeAgentSteps, PermissionCeiling, PermissionMode,
    load_seed_ontologies,
};
use crate::cosmos::default_loop::DefaultLoopTopologyPackPlugin;
use crate::cosmos::domains::Arm;
use crate::cosmos::host::{
    CodingTaskQueue, CodingTaskQueuePlugin, OperatorTopologyPin, auto_route,
};
use crate::cosmos::inventory;
use crate::cosmos::planner::GatePlannerMode;
use crate::cosmos::policy as coding_policy;
use crate::cosmos::topology::{CodingTopologyPackPlugin, HostCapabilities};
use crate::credentials::CredentialStore;
use crate::exec::{ExecIOPlugin, ExecIOProvider, InventoryReport, ModelsReport, OutputFormat};
use crate::progress::ProgressMiddlewarePlugin;
use crate::providers::{self, ModelListing, ProviderReport};
use crate::run;
use crate::state;
use crate::terminal::{
    self, AttachmentStore, BackgroundCommands, BackgroundReporter, BackgroundTasks,
    CommandRegistry, EditJournal, RewindApplication, RewindControl, SessionNavigation,
    SessionReview, SessionSettings, TerminalOutput, TopologyListing, WorkspaceAttachments,
};
use crate::tools::{
    CodeGraphPlugin, CodeToolsConfig, CodeToolsPlugin, GraphSearchMode, MemoryToolsPlugin,
    TaskToolsPlugin, WebToolsPlugin,
};
use crate::usage::{RunCeiling, TaskCostLimit, TokenMeter};
use cli::{Cli, DEFAULT_MAX_OUTPUT_TOKENS, run_max_turns, validate_context_window};
use environment::{
    ARM_VAR, COMPOSITION_POLICY_VAR, DOMAIN_VAR, GATE_PLANNER_VAR, TASK_MAX_COST_VAR, TESTENV_VAR,
    TOPOLOGY_PIN_VAR, env_flag_enabled, lean_composition_policy, ontology_dir,
    resolve_composition_policy, resolve_domain_selection, resolve_gate_planner_mode,
    resolve_topology_pin,
};
use headless::run_headless;
use interactive::Driver;
use posture::{
    ceiling_notice, configured_shell, policy_snapshot, project_shell_environment,
    startup_permissions,
};
use settings_file::{LoadError, Startup};

mod cli;
mod environment;
mod headless;
mod interactive;
mod posture;
mod settings_file;

/// The command-line parser, re-exported so a host that adds settings of its own
/// ([`Host::arg`]) builds them against the version `polaris` parses with.
pub use clap;

/// The error a host's plugin closure may return to refuse startup.
pub type HostError = Box<dyn std::error::Error + Send + Sync>;

/// One early validation/configuration closure supplied by a host.
type Configure = Box<dyn FnOnce(&Launch<'_>, &mut LaunchOptions) -> Result<(), HostError> + Send>;

/// Host choices applied before providers or plugins start.
///
/// Defaults preserve stock routing. Host validation can also return an error
/// here, before credential checks or filesystem side effects from plugins.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Recognize ordinary prompts that can be served by a coding topology.
    /// Explicit `--topology-task` and topology pins remain available.
    pub auto_route: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self { auto_route: true }
    }
}

/// One closure a host gave to [`Host::plugins`].
type AddPlugins = Box<dyn FnOnce(&Launch<'_>, &mut Server) -> Result<(), HostError> + Send>;

/// Output-channel capacity for the non-interactive front end's `UserIO`.
const EXEC_OUTPUT_BUFFER: usize = 64;

/// How long a signalled stop waits for the TUI runtime to leave the alternate
/// screen before giving up and printing over it.
const TUI_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long the shutdown path waits for an interrupted turn to record itself
/// and release the session before the driver is aborted out from under it.
const TURN_STOP_GRACE: Duration = Duration::from_secs(2);

/// The `polaris` agent's startup, with the additions a host program makes.
///
/// `Host::new()` is the whole of what the `polaris` binary does. A host adds
/// [settings of its own](Self::arg) and [plugins](Self::plugins), then
/// [runs](Self::run) it on a command line. The module documentation shows a
/// complete host, and says where its plugins stand relative to `polaris`'s own.
///
/// # Surface
///
/// | Method | Purpose |
/// |--------|---------|
/// | [`new`](Self::new) | The stock startup, with nothing added. |
/// | [`arg`](Self::arg) | Add a command-line setting of the host's own. |
/// | [`configure`](Self::configure) | Validate host inputs and choose routing before provider setup. |
/// | [`plugins`](Self::plugins) | Add plugins, with the parsed command line in view. |
/// | [`run`](Self::run) | Parse, start, run, and return the exit code. |
///
/// # Lifecycle
///
/// Nothing runs until [`run`](Self::run). The closures given to
/// [`plugins`](Self::plugins) are called once, during startup, after the
/// command line and configuration files are resolved and before the server
/// starts. A closure can only add what a plugin can add while the server is
/// being built.
///
/// # Composition
///
/// Open extension: any number of settings and closures, in any combination.
/// Adding none gives the stock `polaris`.
///
/// # Example
///
/// ```no_run
/// use polaris_code::startup::Host;
///
/// # async fn start() {
/// let code = Host::new().run(["polaris", "--print", "list the TODOs"]).await;
/// std::process::exit(code);
/// # }
/// ```
#[must_use = "a Host does nothing until it is run"]
#[derive(Default)]
pub struct Host {
    args: Vec<clap::Arg>,
    plugins: Vec<AddPlugins>,
    configure: Vec<Configure>,
}

impl Host {
    /// The stock `polaris` startup.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a command-line setting of the host's own.
    ///
    /// It is parsed together with `polaris`'s flags, listed in `--help`, and
    /// read from the [`Launch`] a [`plugins`](Self::plugins) closure receives.
    /// Give it [`env`](clap::Arg::env) to read it from an environment variable
    /// as well. The id and names must not collide with `polaris`'s own
    /// (`polaris --help` lists them); a collision is a programming error that
    /// clap reports when it builds the parser.
    ///
    /// Host settings are not configuration-file keys: a file does not supply
    /// their defaults.
    pub fn arg(mut self, arg: clap::Arg) -> Self {
        self.args.push(arg);
        self
    }

    /// Validate host settings and select launch options before provider setup.
    ///
    /// Called after parsing, working-directory resolution and listing commands,
    /// before any provider is prepared. Errors use the same exit codes as
    /// [`Self::plugins`]. Callbacks run in registration order. Use this to read
    /// inputs or prepare immutable state shared with a later plugin closure.
    ///
    /// ```no_run
    /// use polaris_code::startup::Host;
    /// let host = Host::new().configure(|launch, options| {
    ///     if launch.is_headless() { options.auto_route = false; }
    ///     Ok(())
    /// });
    /// # let _ = host;
    /// ```
    pub fn configure(
        mut self,
        configure: impl FnOnce(&Launch<'_>, &mut LaunchOptions) -> Result<(), HostError>
        + Send
        + 'static,
    ) -> Self {
        self.configure.push(Box::new(configure));
        self
    }

    /// Add plugins, deciding which from the command line.
    ///
    /// The closure is called once, with the [`Launch`] and the server being
    /// assembled, after every plugin `polaris` adds itself and before the
    /// server starts. Closures run in the order they were added. Returning an
    /// error refuses the run: its text is printed as `polaris: …`, and the exit
    /// code is `2` for a non-interactive run and `1` for an interactive one,
    /// as for any other startup refusal.
    ///
    /// A closure that only adds plugins ignores the [`Launch`]:
    ///
    /// ```no_run
    /// # use polaris_code::startup::Host;
    /// # use polaris::system::server::Server;
    /// # struct Probe;
    /// # impl polaris::system::plugin::Plugin for Probe {
    /// #     const ID: &'static str = "doc::probe";
    /// #     const VERSION: polaris::system::plugin::Version = polaris::system::plugin::Version::new(0, 1, 0);
    /// #     fn build(&self, _: &mut Server) {}
    /// # }
    /// let host = Host::new().plugins(|_launch, server| {
    ///     server.add_plugins(Probe);
    ///     Ok(())
    /// });
    /// # let _ = host;
    /// ```
    pub fn plugins(
        mut self,
        add: impl FnOnce(&Launch<'_>, &mut Server) -> Result<(), HostError> + Send + 'static,
    ) -> Self {
        self.plugins.push(Box::new(add));
        self
    }

    /// Parse `args`, start the agent, run it, and return the process exit code.
    ///
    /// `args` is the whole command line with the program name first, as
    /// [`std::env::args_os`] yields it. The flags are `polaris`'s own plus any
    /// added with [`arg`](Self::arg); `--help` and `--version` print and return
    /// `0`, and a command line that does not parse prints clap's message and
    /// returns `2`.
    ///
    /// The result is the exit code the binary would exit with. This function
    /// does not exit the process, except that a second interrupt signal during
    /// an unattended turn ends it at once, as it does for the binary. The caller
    /// should call `std::process::exit` with the code, as `polaris` does, because
    /// the agent's background workers can keep the async runtime from draining.
    ///
    /// Without `--print` or `--topology-task` the run is interactive and takes
    /// over the terminal until the user quits.
    pub async fn run<I, T>(self, args: I) -> i32
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let argv: Vec<OsString> = args.into_iter().map(Into::into).collect();
        // Credential commands run before any configuration is read, so a
        // settings file that fails to load cannot stand in the way of fixing
        // credentials.
        if accounts::is_account_command(&argv) {
            let save = |key: &str, value: &str| {
                settings_file::set_user_value(key, value).map_err(Into::into)
            };
            return accounts::run_from_args(argv, &save).await;
        }
        let (cli, startup, matches) = match settings_file::load(argv, &self.args) {
            Ok(loaded) => loaded,
            Err(LoadError::Usage(error)) => {
                // Help and version go to stdout and succeed; a bad command line
                // goes to stderr and exits `2`.
                let _ = error.print();
                return error.exit_code();
            }
            Err(LoadError::Config(message)) => {
                eprintln!("Configuration error: {message}");
                return 2;
            }
        };
        launch(self, cli, startup, matches)
            .await
            .unwrap_or_else(Refusal::report)
    }
}

/// What a [`Host::plugins`] closure can read about the run being started.
///
/// It describes the command line as the operator gave it, with configuration
/// files and environment variables already applied. A closure that adds
/// plugins only needs it to decide which.
pub struct Launch<'a> {
    matches: &'a ArgMatches,
    working_dir: &'a Path,
    headless: bool,
    options: LaunchOptions,
}

impl Launch<'_> {
    /// The parsed command line: `polaris`'s flags and the host's own.
    ///
    /// Read a host setting with `matches().get_one::<T>("id")`, using the id
    /// given to [`Host::arg`].
    #[must_use]
    pub fn matches(&self) -> &ArgMatches {
        self.matches
    }

    /// The working directory the agent runs in, resolved to an absolute path.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        self.working_dir
    }

    /// Whether this run is non-interactive (`--print` or `--topology-task`)
    /// rather than the terminal UI.
    #[must_use]
    pub fn is_headless(&self) -> bool {
        self.headless
    }
}

/// A startup failure: what the operator reads and the exit code it ends with.
struct Refusal {
    message: String,
    code: i32,
}

/// Refuse to start, for `message`.
///
/// A non-interactive run exits `2` — the contract's "usage or configuration
/// error", kept distinct from `1`, which means the turn itself ran and failed.
/// The interactive front end keeps the plain failure code it has always exited
/// with, so the TUI path's behaviour is unchanged.
fn refusal(headless: bool, message: impl Display) -> Refusal {
    Refusal {
        message: message.to_string(),
        code: if headless { 2 } else { 1 },
    }
}

impl Refusal {
    /// Say why on stderr, and return the exit code.
    fn report(self) -> i32 {
        eprintln!("polaris: {}", self.message);
        self.code
    }
}

/// Give plugins a bounded cleanup opportunity, preserving the selected exit code.
async fn finish_host(server: &mut Server, exit_code: i32) -> i32 {
    let timeout = if cfg!(test) {
        Duration::from_millis(50)
    } else {
        Duration::from_secs(5)
    };
    match tokio::time::timeout(timeout, server.cleanup()).await {
        Ok(()) => {}
        Err(_) => tracing::warn!(
            timeout_secs = timeout.as_secs_f32(),
            "server cleanup timed out on exit"
        ),
    }
    exit_code
}

/// The one place this build names the topology packs it ships.
///
/// Both assemblies below go through here — the full agent server and the
/// listing-only server `--list-topologies` seals — so what a user is shown can
/// never be a different set from what a turn can select. Enabling another pack
/// stays one `add_plugins` line, in one place.
fn add_topology_packs(server: &mut Server) {
    server
        .add_plugins(TopologyCatalogPlugin)
        .add_plugins(CodingTopologyPackPlugin)
        // The default-loop pack publishes the conversational turn itself as a
        // registration, under its own contract, so the catalog can name and
        // describe the path that actually serves turns. It is publication and
        // not activation: nothing asks for that contract, the conversational
        // turn never reads the catalog, and the registration carries no author
        // priors (`cosmos::default_loop`).
        .add_plugins(DefaultLoopTopologyPackPlugin);
}

/// Seals a catalog holding only the topology packs, with no model provider,
/// no session store, and no agent — a listing must not need an API key and
/// must not be able to start anything.
async fn list_topologies(working_dir: &Path, format: OutputFormat) -> Result<i32, Refusal> {
    let mut server = Server::new();
    add_topology_packs(&mut server);
    if let Err(err) = server.finish().await {
        return Err(refusal(
            true,
            format!("failed to seal the topology catalog: {err}"),
        ));
    }
    let Some(catalog) = server.get_global::<TopologyCatalog>() else {
        return Err(refusal(true, "TopologyCatalog missing"));
    };
    let snapshot = catalog
        .snapshot()
        .map_err(|err| refusal(true, format!("the topology catalog is not sealed: {err}")))?;
    let observed_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default();
    let listed = inventory::observe(
        &snapshot,
        working_dir,
        HostCapabilities::hermetic(),
        observed_at_ms,
    )
    .map_err(|err| refusal(true, format!("failed to read the topology catalog: {err}")))?;
    let report = InventoryReport::new(listed);
    // The third print site in this module, and like the other two every byte of
    // it is rendered inside `exec`.
    println!("{}", report.stdout_payload(format));
    Ok(report.exit_code())
}

/// Lists what each provider offers from the environment and the built-in
/// price table alone: no key, no network, no server.
fn list_models(format: OutputFormat) -> i32 {
    let listing = ModelListing::observe(
        &providers::process_env,
        &CredentialStore::user(),
        &PriceStore::builtin(),
    );
    // The fourth print site: rendered inside `exec`, like the other three.
    let report = ModelsReport::new(listing);
    println!("{}", report.stdout_payload(format));
    report.exit_code()
}

/// Which stage of [`resume_or_create_session`] failed, so the caller can
/// report the message each stage was written for.
#[derive(Debug)]
enum SessionStartupError {
    /// Resuming failed for a reason other than "never seen this id".
    Resume(SessionError),
    /// The id was never seen before, and creating it fresh failed too.
    Create(SessionError),
}

/// Resume `session_id` from the store, or create it fresh when the store has
/// never seen the id before. Returns whether the session was resumed
/// (`true`) or created (`false`).
///
/// [`SessionError::SessionNotFound`] is the one resume failure this falls
/// through on: the store's own way of saying "never seen this id", whether
/// the id came from an explicit `--session` or was generated for a default
/// run. Every other variant — a store that exists but is corrupt or
/// unreadable, a serializer mismatch, a missing agent, ... — is returned to
/// the caller instead, distinguishably, so it can still be treated as fatal.
async fn resume_or_create_session(
    server: &Server,
    sessions: &SessionsAPI,
    session_id: &SessionId,
    agent_type: &AgentTypeId,
    executor: impl Fn() -> GraphExecutor,
    init: impl Fn(&mut SystemContext<'static>),
) -> Result<bool, SessionStartupError> {
    match sessions
        .resume_session_with_executor(server.create_context(), session_id, executor(), |ctx| {
            init(ctx)
        })
        .await
    {
        Ok(()) => Ok(true),
        Err(SessionError::SessionNotFound(_)) => sessions
            .create_session_with_executor(
                server.create_context(),
                session_id,
                agent_type,
                executor(),
                |ctx| init(ctx),
            )
            .map(|()| false)
            .map_err(SessionStartupError::Create),
        Err(err) => Err(SessionStartupError::Resume(err)),
    }
}

/// Run what the command line asked for, once it has parsed.
async fn launch(
    mut host: Host,
    cli: Cli,
    startup: Startup,
    matches: ArgMatches,
) -> Result<i32, Refusal> {
    if cli.account.is_some() {
        return Err(refusal(false, "`login` and `logout` take no other options"));
    }
    let headless = cli.print.is_some() || cli.topology_task.is_some();
    let permissions =
        startup_permissions(&cli, headless).map_err(|message| refusal(headless, message))?;

    let working_dir = cli
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let working_dir = match working_dir.canonicalize() {
        Ok(dir) => dir,
        Err(err) => {
            let hint = if working_dir == Path::new("login") || working_dir == Path::new("logout") {
                "; credential commands are `polaris login` and `polaris logout`, with no \
                 option before them"
            } else {
                ""
            };
            return Err(refusal(
                headless,
                format!("invalid working directory: {err}{hint}"),
            ));
        }
    };

    // Listing is not a run: it seals the same packs, reads the sealed catalog
    // and exits before anything else is assembled.
    if cli.list_topologies {
        return list_topologies(&working_dir, cli.output_format).await;
    }
    if cli.list_models {
        return Ok(list_models(cli.output_format));
    }

    let mut options = LaunchOptions::default();
    let mut launch = Launch {
        matches: &matches,
        working_dir: &working_dir,
        headless,
        options: options.clone(),
    };
    for configure in std::mem::take(&mut host.configure) {
        configure(&launch, &mut options).map_err(|error| refusal(headless, error))?;
    }
    launch.options = options;
    let (mut server, run) = prepare(host, cli, startup, &launch, permissions).await?;
    match serve(&mut server, run).await {
        Ok(code) => Ok(code),
        // The server is already running, so give its plugins their cleanup
        // before the process ends.
        Err(refusal) => {
            let code = refusal.report();
            Ok(finish_host(&mut server, code).await)
        }
    }
}

/// What startup carries from assembling the server to running the agent.
struct Run {
    auto_route: bool,
    headless: bool,
    cli: Cli,
    startup: Startup,
    working_dir: PathBuf,
    agent_config: AgentConfig,
    declared_models: crate::config::ModelCatalog,
    startup_notes: Vec<String>,
    permission_mode: PermissionMode,
    permission_ceiling: Option<PermissionCeiling>,
    graph_search: GraphSearchMode,
    commands: Arc<CommandRegistry>,
    attachment_store: AttachmentStore,
    attachment_settings: Arc<OnceLock<SessionSettings>>,
    background_tasks: BackgroundTasks,
    backgrounded: Option<terminal::background::Backgrounded>,
    background_commands: Arc<OnceLock<BackgroundCommands>>,
    history: Arc<terminal::history::CanonicalHistory>,
    session_id: SessionId,
    interrupt_api: Arc<OnceLock<SessionsAPI>>,
    interrupt_navigation: Arc<OnceLock<SessionNavigation>>,
    status_emitter: Arc<OnceLock<run::StatusEmitter>>,
    turn_steering: terminal::TurnSteering,
    provider_report: ProviderReport,
    trajectory: Option<crate::trajectory::RunTrajectory>,
    session_store: Arc<dyn polaris::sessions::SessionStore>,
    edit_journal: EditJournal,
    token_meter: TokenMeter,
    run_ceiling: Option<RunCeiling>,
    max_output_tokens: NonZeroU32,
    max_turns: usize,
    coding_tasks: CodingTaskQueue,
    topology_pin_raw: Option<String>,
    terminal_output: Option<TerminalOutput>,
    exec_io: Option<ExecIo>,
}

/// The non-interactive front end's channel ends.
type ExecIo = (
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
    mpsc::Receiver<IOMessage>,
);

/// Validate the run's settings, assemble the server with the host's plugins, and
/// start it.
async fn prepare(
    host: Host,
    cli: Cli,
    startup: Startup,
    launch: &Launch<'_>,
    permissions: (PermissionMode, Option<PermissionCeiling>),
) -> Result<(Server, Run), Refusal> {
    let headless = launch.headless;
    let working_dir = launch.working_dir.to_path_buf();
    let (permission_mode, permission_ceiling) = permissions;

    // Which models this run can reach, and which one it starts on. Resolved
    // here — before the server is built, before a session exists and before a
    // provider is ever called — so a name nothing can serve ends the process
    // with the set that would have worked, not with a provider error at the
    // first token. The set is narrowed to the registered providers once the
    // registry exists, further down.
    let declared_models = startup.catalog(cli.provider.catalog(&providers::process_env));
    let requested_model = cli
        .model
        .as_deref()
        .map(|id| cli.provider.qualify_model(id));
    let declared_models = cli
        .provider
        .launch_catalog(declared_models, requested_model.as_deref());
    let selected_model = declared_models
        .select_noting(requested_model.as_deref())
        .map_err(|err| refusal(headless, err))?;
    let model_id = selected_model.id.clone();
    // What startup has to tell the person running it, said once the front end
    // that shows it exists: stderr for a `--print` run, the transcript for a
    // session.
    let mut startup_notes: Vec<String> = selected_model.note().into_iter().collect();
    startup_notes.extend(startup.notices().iter().cloned());

    // Config gate: `POLARIS_CODE_GRAPH_SEARCH` selects which
    // graph-search surface the agent sees — `curated`, `freeform`, or `both`
    // (default). Curated registers the concept tools; freeform leaves ontrace's
    // `sparql_query` enabled. A curated-only run withholds `sparql_query`.
    let graph_search = std::env::var("POLARIS_CODE_GRAPH_SEARCH")
        .map(|s| GraphSearchMode::from_str_or_default(&s))
        .unwrap_or_default();
    // Config gate: `POLARIS_CODE_PROMPT_CACHE=0|false|off` opts out of the
    // cache-aware context strategy (and provider prompt caching), reverting to the
    // sliding `LinearWindow`. Enabled by default.
    let prompt_cache = env_flag_enabled(std::env::var("POLARIS_CODE_PROMPT_CACHE"));
    // Safety config gate: `POLARIS_CODE_REDACT=0|false|off` turns off secret
    // redaction of tool output. On by default — a host has to opt out — and
    // the escape hatch exists because a rule that eats a tool result the model
    // needs is worse than no rule at all. Redaction is accident prevention,
    // never a boundary: see `docs/secret-redaction.md`.
    let redaction = env_flag_enabled(std::env::var("POLARIS_CODE_REDACT"));
    // `POLARIS_CODE_COMPOSITION_POLICY` selects the finite-composition policy
    // an accepted coding task's selected topology composes under. Validated
    // here, before the server exists, the same "unknown" ground `--model`
    // and `POLARIS_CODE_TOPOLOGY_PIN` already refuse on, rather than being
    // discovered per turn deep inside `cosmos::host::select_and_run`. Unset
    // installs no global, leaving `read_composition_policy` at its own
    // deterministic default.
    let composition_policy_raw = std::env::var(COMPOSITION_POLICY_VAR).ok();
    let composition_policy = resolve_composition_policy(composition_policy_raw.as_deref())
        .map_err(|message| refusal(headless, message))?;
    // `POLARIS_CODE_GATE_PLANNER` selects which gate planner builds a
    // task-specific plan when the policy above is `gates`. Validated
    // unconditionally and at the same point, for the same reason.
    let gate_planner_raw = std::env::var(GATE_PLANNER_VAR).ok();
    let gate_planner_mode = resolve_gate_planner_mode(gate_planner_raw.as_deref())
        .map_err(|message| refusal(headless, message))?;
    // `POLARIS_CODE_DOMAIN`/`POLARIS_CODE_ARM` select the domain plan/notes a
    // `gates`-policy turn's statement-only edit shape uses. Validated here,
    // at the same point and for the same reason as the two variables above.
    let domain_raw = std::env::var(DOMAIN_VAR).ok();
    let arm_raw = std::env::var(ARM_VAR).ok();
    let domain_selection = resolve_domain_selection(
        domain_raw.as_deref(),
        cli.arm.as_deref().or(arm_raw.as_deref()),
    )
    .map_err(|message| refusal(headless, message))?;
    let composition_policy =
        lean_composition_policy(domain_selection, composition_policy, gate_planner_mode)
            .map_err(|message| refusal(headless, message))?;
    let lean_options = if domain_selection.is_some_and(|s| s.arm == Arm::Lean) {
        Some(
            crate::cosmos::planner::lean::LeanOptions::from_env()
                .map_err(|message| refusal(headless, message))?,
        )
    } else {
        None
    };
    if domain_selection.is_some_and(|selection| selection.arm == Arm::Composed)
        && (composition_policy != Some(crate::cosmos::composition::CompositionPolicy::Gates)
            || gate_planner_mode != Some(GatePlannerMode::Model))
    {
        return Err(refusal(
            headless,
            "arm composed requires POLARIS_CODE_COMPOSITION_POLICY=gates and POLARIS_CODE_GATE_PLANNER=model",
        ));
    }

    // The operator's spend ceiling, validated before anything starts. Two
    // ways to get exit 2 here rather than a run that quietly ignores a limit:
    // a limit that cannot bound a run (zero, negative, not a number), and a
    // money limit over a model with no price — "unpriced" must never mean
    // "unlimited", so the run refuses rather than proceeds unbounded.
    let run_ceiling = RunCeiling::from_limits(cli.max_cost_usd, cli.max_tokens)
        .map_err(|err| refusal(headless, err))?;
    let configured_task_cost = if let Some(options) = &lean_options {
        options.max_cost_usd
    } else {
        match std::env::var(TASK_MAX_COST_VAR).ok() {
            Some(raw) => {
                let limit = raw.trim().parse::<f64>().map_err(|_| {
                    refusal(
                        headless,
                        "POLARIS_CODE_MAX_COST_USD must be finite and nonnegative",
                    )
                })?;
                if limit == 0.0 { None } else { Some(limit) }
            }
            None => None,
        }
    };
    let pricing_store = PriceStore::live();
    let task_cost_limit = configured_task_cost
        .map(|limit| {
            let limit = TaskCostLimit::new(limit).map_err(|_| {
                refusal(
                    headless,
                    "POLARIS_CODE_MAX_COST_USD must be a positive finite number",
                )
            })?;
            if crate::usage::price_for(&pricing_store, &model_id).is_none() {
                return Err(refusal(
                    headless,
                    "POLARIS_CODE_MAX_COST_USD requires a priced model",
                ));
            }
            Ok(limit)
        })
        .transpose()?;
    if let Some(ceiling) = run_ceiling
        && let Some(why) = ceiling.unenforceable_for(&pricing_store, &model_id)
    {
        return Err(refusal(headless, why));
    }

    // `POLARIS_CODE_MAX_OUTPUT_TOKENS` sets the `max_tokens` every request
    // carries. Resolved once here for the provider, the agent reservation,
    // and the non-interactive report, so they agree on the output allowance.
    let max_output_tokens = cli.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let max_turns = run_max_turns(&cli);
    if let Err(why) = validate_context_window(cli.context_window, max_output_tokens) {
        return Err(refusal(headless, why));
    }
    let mut agent_config = AgentConfig::new(model_id, working_dir.clone())
        .with_graph_search(graph_search)
        .with_unrestricted_file_paths(cli.dangerously_skip_permissions)
        .with_prompt_cache(prompt_cache)
        .with_max_output_tokens(max_output_tokens);
    agent_config.context_window = cli.context_window;
    // Everything this run writes that is not the user's work goes under one
    // directory, and that directory is invisible to git. Otherwise the agent's
    // own transcript rides along in whatever is built from the working tree —
    // a prediction patch, a `git commit -a`, a CI diff. See `state`'s module
    // docs for why the state stays repository-local rather than moving out.
    let state_dir = state::state_dir(&working_dir)
        .map_err(|err| refusal(headless, format!("failed to open state directory: {err}")))?;
    let edit_journal = EditJournal::open(&working_dir)
        .map_err(|err| refusal(headless, format!("failed to open rewind journal: {err}")))?;

    // Tool permission is resolved by the active coding mode further down. The
    // shell keeps its independent directory and command-pattern boundaries:
    //
    // * sandboxing is on by default and an empty `allowed_dirs` denies *every*
    //   directory, so the working dir has to be granted explicitly or an
    //   approved command is rejected before it can spawn; and
    // * attended modes allow `*` inside only after the outer mode has allowed or
    //   confirmed the tool call; unattended mode installs exactly its immutable
    //   operator-provided patterns, and an unmatched command fails closed.
    //
    // `denied_commands` is still evaluated first, per subcommand, so the
    // destructive patterns stay authoritative unless the operator explicitly
    // selected unrestricted container execution.
    let shell_config = project_shell_environment(
        configured_shell(
            &working_dir,
            &permission_mode,
            cli.dangerously_skip_permissions,
        ),
        std::env::var(TESTENV_VAR).is_ok_and(|v| v.trim() == "1"),
    );

    // Session token meter: the agent's `act` system folds streaming usage into
    // it regardless of front end; only the interactive path prints it — the
    // non-interactive result emitter's stdout is reserved for the result.
    let token_meter = TokenMeter::new();

    // Accepted coding tasks. The host keeps this handle and
    // `CodingTaskQueuePlugin` publishes the same one, so whatever accepts a
    // coding task and the run surface's selection point share one queue. It
    // starts empty, and three things put work into it: `/topology`
    // interactively and `--topology-task` for one unattended run, both
    // through the `TopologyWork` handle built below and both an explicit,
    // named accept; and, for a `--print` run whose prompt `auto_route`
    // recognises as a light-effort question, this binding directly, below,
    // marked `TaskOrigin::AutoRouted` rather than explicit — the one
    // difference being that an inconclusive answer there falls through to
    // the conversational turn instead of ending it. Until one of the three is
    // used, every turn is the conversational one and selection stays inert.
    let coding_tasks = CodingTaskQueue::new();

    // Terminal `/commands` run locally, never as model requests. `help` is
    // built in; features that add commands (sessions, settings) register
    // theirs on this registry. `@` file mentions complete over `working_dir`
    // and are refused if they resolve outside it. Both are consumed by the
    // interactive front end below; a `--print` run installs no panels.
    let commands = Arc::new(CommandRegistry::with_builtins());

    // Composer attachments. The store is shared: the resolver fills it when
    // the user attaches, and the agent's `receive_user_input` empties it when
    // the prompt naming a handle is taken. Settings arrive later — the model
    // registry does not exist until the server is finished — so the capability
    // check reads them through a cell and falls back to the configured model
    // until it is set.
    let attachment_store = AttachmentStore::new();
    let attachment_settings = Arc::new(OnceLock::<SessionSettings>::new());

    // Managed shell runs pushed out of a turn, by the `Ctrl-T` chord or by the
    // model's own `run_in_background` call. The table is shared:
    // `execute_call` fills it through the sink it installs, the chord, the
    // four `/commands` and the task toolset read it, and the reporter below
    // closes each run's block when it ends. `Backgrounded` is the one-way
    // announcement that a run has left its turn.
    //
    // `--print` takes the detached table on purpose. Exactly one thing may
    // wait on a run — `ShellExecutor::wait_for` consumes the terminal result —
    // and headless wires no reporter, so `report_backgrounded` has to answer
    // *false* there for `run_in_background` to know it must collect the result
    // itself. A live channel nobody drains would answer true and lose the exit
    // code.
    let default_deadline = Duration::from_secs(shell_config.default_timeout_secs);
    let (background_tasks, backgrounded) = if headless {
        (BackgroundTasks::detached(default_deadline), None)
    } else {
        let (tasks, stream) = BackgroundTasks::new(default_deadline);
        (tasks, Some(stream))
    };
    let background_commands = Arc::new(OnceLock::<BackgroundCommands>::new());

    let history = Arc::new(terminal::history::CanonicalHistory::default());
    let session_id = match cli.session.clone() {
        Some(id) => SessionId::from_string(id),
        None if headless => SessionId::new(),
        None => SessionId::from_string("default"),
    };
    let interrupt_api = Arc::new(OnceLock::<SessionsAPI>::new());
    let interrupt_navigation = Arc::new(OnceLock::<SessionNavigation>::new());
    let status_emitter = Arc::new(OnceLock::<run::StatusEmitter>::new());
    let turn_steering = if headless {
        terminal::TurnSteering::default()
    } else {
        terminal::TurnSteering::enabled()
    };

    // The provider's plugins panic when their key is missing, which exits `101`
    // through a backtrace, and a Bedrock run with no region or credentials only
    // fails at the first request. Neither front end should meet that: the
    // non-interactive contract owes its caller exit code `2` and an explanation
    // for a configuration error, and an interactive run owes a person the same
    // fact before the TUI takes the screen. One check serves both, and what it
    // finds out — which region, which credential source — is reported, never
    // the credential itself.
    if cli.model_request_timeout_secs.is_some() && cli.provider != providers::Provider::Bedrock {
        return Err(refusal(
            headless,
            providers::StartupError::UnsupportedRequestTimeout {
                provider: cli.provider,
            },
        ));
    }
    let ready_provider = providers::prepare(
        cli.provider,
        cli.region.as_deref(),
        &providers::process_env,
        &CredentialStore::user(),
    )
    .await
    .map_err(|err| refusal(headless, err))?;
    startup_notes.push(ready_provider.report().line());
    let ready_provider = match cli.model_request_timeout_secs {
        Some(timeout) => ready_provider
            .with_request_timeout(timeout)
            .map_err(|error| refusal(headless, error))?,
        None => ready_provider,
    };
    let trajectory = cli
        .trajectory_path
        .as_ref()
        .map(|path| {
            crate::trajectory::RunTrajectory::create(
                path,
                &session_id.to_string(),
                &agent_config.model_id,
                cli.print.as_deref().expect("trajectory requires --print"),
            )
            .map_err(|error| refusal(headless, format!("cannot create trajectory: {error}")))
        })
        .transpose()?;

    // Built once and shared: the session plugin persists through it, and
    // `/sessions` reads the saved records back through the same store so a
    // picker row can say more than an identifier. It lives under the run's
    // state directory, so a saved transcript stays out of the working tree.
    let session_store: Arc<dyn polaris::sessions::SessionStore> =
        Arc::new(FileStore::new(state_dir.join("sessions")));

    // Servers a project declares start only once the user has trusted them.
    let mcp = crate::tools::mcp::McpClientPlugin::from_working_dir(
        &working_dir,
        commands.clone(),
        startup.trust().clone(),
    )
    .await;
    startup_notes.extend(mcp.startup_notes());
    // Indexing `~` (or any folder that is not a project) walks everything
    // under it before the first answer; skip the boot pass there and say how
    // to get it. Interactive only: a headless run (evals, scripts) is pointed
    // at its directory on purpose and keeps the index it always had.
    let boot_index = crate::tools::boot_index_wanted(headless, &working_dir);
    if !boot_index {
        startup_notes.push(format!(
            "{} doesn't look like a project, so the code index was skipped. \
             Start polaris inside a repository (e.g. `cd ~/my-repo && polaris`) \
             for code navigation.",
            working_dir.display()
        ));
    }
    let tool_grants = crate::agents::permission_rules::RunToolGrants::new(cli.allow_tool.clone())
        .map_err(|error| refusal(headless, error))?;

    let mut server = Server::new();
    server.add_plugins(ModelsPlugin);
    ready_provider.install_with_options(
        &mut server,
        max_output_tokens,
        pricing_store.clone(),
        crate::model_run::ModelRunOptions {
            max_output_tokens: Some(max_output_tokens),
            reasoning_effort: cli.reasoning_effort.clone(),
            trajectory: trajectory.clone(),
        },
    );
    if let Some(trace) = &trajectory {
        server.insert_global(trace.clone());
    }
    server
        // Tools: file primitives, the SPARQL store + tools, the code graph, shell.
        .add_plugins(polaris::tools::ToolsPlugin)
        .add_plugins(mcp)
        .add_plugins(CodeToolsPlugin::new(
            CodeToolsConfig::new(&working_dir)
                .with_edit_journal(edit_journal.clone())
                .with_unrestricted_paths(cli.dangerously_skip_permissions),
        ))
        .add_plugins(OxigraphPlugin::in_memory().with_tools(true))
        .add_plugins(
            CodeGraphPlugin::new(&working_dir)
                .with_search_mode(graph_search)
                .with_boot_index(boot_index),
        )
        // Reflective memory: memoize/recall structural conclusions,
        // content-hash-invalidated against the cited spans.
        .add_plugins(MemoryToolsPlugin::new(&working_dir))
        .add_plugins(ShellPlugin::new(shell_config))
        .add_plugins(crate::tools::approval::ApprovalShellPlugin)
        // The model's own way to push a command out of a turn and read it
        // back. Over the executor `ShellPlugin` just published and the table
        // above — no second process registry.
        .add_plugins(TaskToolsPlugin::new(background_tasks.clone()))
        .add_plugins(
            crate::agents::permission_rules::PermissionRulesPlugin::new(&working_dir)
                .with_unrestricted_shell(cli.dangerously_skip_permissions)
                .with_project_trust(startup.trust().clone())
                .with_commands(commands.clone(), attachment_settings.clone())
                .with_tool_grants(tool_grants),
        )
        .add_plugins(crate::agents::auto_review::AutoReviewPlugin::new(
            crate::agents::auto_review::ReviewPolicy {
                model: cli.review_model.clone(),
                ..Default::default()
            },
        ))
        // Reading a public web page; registered `Confirm`, so it asks first.
        .add_plugins(WebToolsPlugin::default())
        // Sessions + persistence.
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(session_store.clone()))
        // The reflective coding agent, carrying the one safety control that
        // sits between a tool result and everyone who reads it.
        .add_plugins(
            CodeAgentPlugin::new()
                .with_prompt_cache(prompt_cache)
                .with_redaction(redaction),
        )
        // `/compact`, offered because the agent above reads it.
        .add_plugins(crate::agents::context::CompactionPlugin::new(
            commands.clone(),
        ))
        // Live per-step progress → whichever UserIO the front end installs.
        .add_plugins(ProgressMiddlewarePlugin)
        // Topology selection. One process-wide catalog provider, the coding
        // pack that contributes every coding registration to it, one evidence
        // capability the host appends decisions and runs to, and the queue
        // the run surface's selection point drains. Enabling another pack is
        // one more `add_plugins` line and nothing else.
        //
        // These five are inert on their own: with nothing accepted into
        // `coding_tasks`, every turn is the conversational one below. The packs
        // themselves are named once, in `add_topology_packs` below.
        .add_plugins(TopologyEvidencePlugin::new(coding_policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(coding_tasks.clone()));
    add_topology_packs(&mut server);

    // Front-end IO: exactly one of these two, never both. The non-interactive
    // plugin is installed *instead of* the TUI panels, never alongside them.
    let mut terminal_output = None;
    let exec_io = if headless {
        let (provider, input_tx, output_rx) = ExecIOProvider::new(EXEC_OUTPUT_BUFFER);
        let provider = Arc::new(provider);
        server.add_plugins(ExecIOPlugin::new(provider.clone()));
        Some((provider, input_tx, output_rx))
    } else {
        server.add_plugins(crate::agents::InteractiveApprovalPlugin);
        // One shared log buffer feeds the conversation panel's Ctrl-O node
        // timeline (which buckets entries by their enclosing node span). The
        // log plugin installs the global tracing subscriber over it, with no
        // Logs tab of its own.
        let log_buffer = LogBuffer::with_capacity(2_000);
        let callback_api = interrupt_api.clone();
        let callback_navigation = interrupt_navigation.clone();
        let callback_background = background_commands.clone();
        // Terminal UI + panels. The conversation panel publishes the
        // `UserIO` the agent's `receive_user_input`/`confirm` flow binds to.
        // Enable mouse capture so the wheel scrolls the conversation
        // transcript and the graph (hold Shift/Option for native
        // text-selection in most terminals).
        server.add_plugins(TuiPlugin::new(TuiConfig::default().with_mouse(true)));
        // The other tabs — Graph and Help — and the log capture behind the
        // conversation's node timeline. The Graph tab draws the coding agent
        // with whatever steps plugins added.
        crate::tabs::add_code_agent_shell_tabs(&mut server, log_buffer.clone());
        // The conversation panel's `UserIO` is wrapped so commands are
        // executed and answered before the agent's `receive` ever returns.
        // One call yields both host handles the front end needs: the replay
        // output used by session navigation, and the status sink the
        // cancellation path emits `interrupting`/`cancelled` through.
        let (conversation, output) = terminal::attach_with_status(
            ConversationPanelPlugin::default()
                .with_log_buffer(log_buffer.clone())
                .with_history_source(history.clone())
                .with_attachment_source(Arc::new(
                    WorkspaceAttachments::new(
                        &working_dir,
                        attachment_store.clone(),
                        agent_config.model_id.clone(),
                    )
                    .with_settings(attachment_settings.clone()),
                ))
                .with_turn_input_handle(Arc::new(turn_steering.clone()))
                .with_placeholder("Ask Polaris…")
                .with_notifications(cli.notify.config())
                .with_interrupt_handle(move || {
                    let Some(sessions) = callback_api.get().cloned() else {
                        return;
                    };
                    let Some(navigation) = callback_navigation.get() else {
                        return;
                    };
                    run::request_interrupt(&sessions, &navigation.current());
                })
                // Ctrl-T: ask to background whatever the turn is waiting on.
                // The callback runs on the UI task, so it hands the question to
                // the host and returns — the receipt comes from the invocation
                // that owns the tool block, and a refusal from the commands.
                .with_background_handle(move || {
                    let Some(commands) = callback_background.get().cloned() else {
                        return;
                    };
                    tokio::spawn(async move { commands.request_background().await });
                }),
            commands.clone(),
            &working_dir,
            status_emitter.clone(),
        );
        terminal_output = Some(output);
        server.add_plugins(conversation);
        None
    };

    // The host's own plugins, after everything above so they can depend on any
    // of it. They run before the globals below, so a host that inserts a global
    // of its own does it here.
    for add in host.plugins {
        add(launch, &mut server).map_err(|error| refusal(headless, error))?;
    }

    // Register the token meter as a global so the agent's `act` system can read
    // it via `Res<TokenMeter>`. Must precede any `create_context()` call.
    server.insert_global(token_meter.clone());
    server.insert_global(ready_provider.report().clone());
    if let Some(limit) = task_cost_limit {
        server.insert_global(limit);
    }

    // The operator's limit rides alongside the meter it bounds, and only when
    // one was set: `run::drive_turn` reading no `RunCeiling` is what "this run
    // has no limit" means. Same ordering requirement as the meter.
    if let Some(ceiling) = run_ceiling {
        server.insert_global(ceiling);
    }

    // The operator's run-wide topology pin, read raw
    // and installed here, before the sealed catalog even exists to validate
    // it against: `Server::insert_global` panics once a system context has
    // been created, and `finish()` below creates one (it kicks off background
    // code indexing). Trimmed and filtered for blank exactly as
    // `resolve_topology_pin` does; *validated* against the sealed catalog
    // further down, once one exists, and the process exits before a session
    // or a turn if it is not one of the registered ids — an unvalidated value
    // sits in the global for that brief window, but nothing ever reads it
    // before the validation below would already have exited the process.
    let topology_pin_raw = std::env::var(TOPOLOGY_PIN_VAR).ok();
    if let Some(registration_id) = topology_pin_raw
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        server.insert_global(OperatorTopologyPin::new(registration_id));
    }

    // The operator's composition policy, already fully validated above
    // (unlike the pin, it names no catalog member, so nothing here waits on
    // the sealed catalog). Unset installs no global at all, so
    // `read_composition_policy` falls through to its own deterministic
    // default exactly as it did before this variable existed.
    if let Some(policy) = composition_policy {
        server.insert_global(policy);
    }
    // Same story for which gate planner a `gates` policy uses: unset
    // installs no global, so `read_gate_planner_mode` falls through to
    // `GatePlannerMode::Scripted`.
    if let Some(mode) = gate_planner_mode {
        server.insert_global(mode);
    }
    // Same story for the domain/arm selection: unset installs no global, so
    // `read_domain_selection` falls through to `DomainSelection::default()`
    // (no domain, the generic arm).
    if let Some(selection) = domain_selection {
        server.insert_global(selection);
    }

    // build + ready: spawns the TUI task (interactive mode) and kicks
    // background code indexing.
    if let Err(err) = server.finish().await {
        return Err(refusal(
            headless,
            format!("failed to start the server: {err}"),
        ));
    }

    let provider_report = ready_provider.report().clone();
    Ok((
        server,
        Run {
            auto_route: launch.options.auto_route,
            headless,
            cli,
            startup,
            working_dir,
            agent_config,
            declared_models,
            startup_notes,
            permission_mode,
            permission_ceiling,
            graph_search,
            commands,
            attachment_store,
            attachment_settings,
            background_tasks,
            backgrounded,
            background_commands,
            history,
            session_id,
            interrupt_api,
            interrupt_navigation,
            status_emitter,
            turn_steering,
            provider_report,
            trajectory,
            session_store,
            edit_journal,
            token_meter,
            run_ceiling,
            max_output_tokens,
            max_turns,
            coding_tasks,
            topology_pin_raw,
            terminal_output,
            exec_io,
        },
    ))
}

/// Wire the started server to a session and drive it: one turn for a
/// non-interactive run, the turn loop for the terminal UI. Returns the exit
/// code, with plugin cleanup already done on every path that completes.
async fn serve(server: &mut Server, run: Run) -> Result<i32, Refusal> {
    let Run {
        auto_route: allow_auto_route,
        headless,
        cli,
        startup,
        working_dir,
        agent_config,
        declared_models,
        startup_notes,
        permission_mode,
        permission_ceiling,
        graph_search,
        commands,
        attachment_store,
        attachment_settings,
        background_tasks,
        backgrounded,
        background_commands,
        history,
        session_id,
        interrupt_api,
        interrupt_navigation,
        status_emitter,
        turn_steering,
        provider_report,
        trajectory,
        session_store,
        edit_journal,
        token_meter,
        run_ceiling,
        max_output_tokens,
        max_turns,
        coding_tasks,
        topology_pin_raw,
        terminal_output,
        exec_io,
    } = run;

    // The reachable catalog: the declared set, less every model whose provider
    // this run did not register. Re-resolving the startup model against it
    // turns "the plugin that serves this model is not installed" into the same
    // startup refusal as an unknown name, rather than a first-token failure.
    let catalog = {
        let Some(models) = server.get_global::<polaris::models::ModelRegistry>() else {
            return Err(refusal(headless, "ModelRegistry missing"));
        };
        declared_models.restricted_to_providers(&models.llm_provider_names())
    };
    if let Err(err) = catalog.resolve(&agent_config.model_id) {
        return Err(refusal(headless, err));
    }
    // The ceiling is installed on the settings rather than checked at each
    // surface: these settings are the only mutable holder of the session's
    // mode, and `prepare_context` reads exactly this snapshot back to build the
    // policy the turn resolves every tool call against.
    let settings = {
        let settings = SessionSettings::new(
            agent_config.model_id.clone(),
            permission_mode.clone(),
            &catalog,
        );
        match permission_ceiling.clone() {
            Some(ceiling) => settings.with_permission_ceiling(ceiling),
            None => settings,
        }
    };
    let _ = attachment_settings.set(settings.clone());
    if !headless && let Err(err) = settings.register(&commands) {
        return Err(refusal(
            headless,
            format!("failed to register settings commands: {err}"),
        ));
    }
    if !headless && let Err(err) = accounts::register_login_command(&commands) {
        return Err(refusal(
            headless,
            format!("failed to register /login: {err}"),
        ));
    }

    // The sealed catalog this server just built, read back once,
    // unconditionally: `TopologyWork` needs it whether or not this run is
    // headless (a named topology is validated against it at acceptance, the
    // same "unknown" ground `--topology-task` and `/topology` share), and
    // interactively `/topologies` asks it the same question `--list-topologies`
    // does. Sealing is final, so one read cannot drift from what is registered.
    let topology_catalog = match server
        .get_global::<TopologyCatalog>()
        .map(|catalog| catalog.snapshot())
    {
        Some(Ok(snapshot)) => snapshot,
        Some(Err(err)) => {
            return Err(refusal(
                headless,
                format!("the topology catalog is not sealed: {err}"),
            ));
        }
        None => return Err(refusal(headless, "TopologyCatalog missing")),
    };

    // The operator's run-wide topology pin, installed as a global further up
    // (before this line, since it had to precede `finish()`) — validated only
    // here, now that the sealed catalog it has to name a member of actually
    // exists. An unrecognised id is refused here, at startup, the same
    // "unknown" ground `--model` already refuses on, never left for a later
    // turn to fail on and never silently substituted with the selector's own
    // default — so trying a different registration against the same accepted
    // work is a different process launch, never a rebuild. Unset leaves
    // `select_and_run` exactly as it was.
    let known_topologies: Vec<String> = topology_catalog
        .candidates()
        .iter()
        .map(|candidate| candidate.registration_id().as_str().to_owned())
        .collect();
    if let Err(message) = resolve_topology_pin(topology_pin_raw.as_deref(), &known_topologies) {
        return Err(refusal(headless, message));
    }

    // The one writer of `coding_tasks`. It reads the live posture from
    // `settings` rather than from the session context, deliberately: a
    // `/topology` handler runs inside `CommandDispatch::receive`, which is
    // itself inside the turn that holds the session context, so asking the
    // session for its policy there would deadlock. `SessionSettings` is the
    // same snapshot `/permission` writes, so the answer is still the live one.
    let topology_work = terminal::TopologyWork::new(
        coding_tasks.clone(),
        working_dir.clone(),
        topology_catalog.clone(),
        {
            let settings = settings.clone();
            move || settings.snapshot().permission_mode
        },
    );
    if !headless && let Err(err) = topology_work.register(&commands) {
        return Err(refusal(
            headless,
            format!("failed to register topology command: {err}"),
        ));
    }

    // `/topologies`, interactive only — the same snapshot above.
    if !headless {
        let listing = TopologyListing::new(
            topology_catalog,
            working_dir.clone(),
            HostCapabilities::hermetic(),
        );
        if let Err(err) = listing.register(&commands) {
            return Err(refusal(
                headless,
                format!("failed to register /topologies: {err}"),
            ));
        }
    }

    // Seed the `TBox` from the ontology packs, if a pack directory is configured.
    if let Some(dir) = ontology_dir() {
        let store = server
            .get_global::<OxigraphStore>()
            .map(|g| OxigraphStore::clone(&g));
        if let Some(store) = store {
            match load_seed_ontologies(&store, &dir).await {
                Ok(n) => tracing::info!("seeded {n} ontology pack(s) from {}", dir.display()),
                Err(e) => tracing::warn!("ontology seeding skipped: {e}"),
            }
        }
    }

    // Wire sessions: serializers and the agent. The tool permission policy is
    // applied per session further down, once the session context exists.
    let Some(persistence) = server.api::<PersistenceAPI>() else {
        return Err(refusal(headless, "PersistenceAPI missing"));
    };
    let Some(sessions) = server.api::<SessionsAPI>().cloned() else {
        return Err(refusal(headless, "SessionsAPI missing"));
    };
    let _ = interrupt_api.set(sessions.clone());
    sessions.set_serializers(persistence.serializers());
    // The agent is built from the step registry the plugins filled while they
    // built. With no steps added it is the stock coding agent, node for node.
    let agent = {
        let Some(steps) = server.get_global::<CodeAgentSteps>() else {
            return Err(refusal(headless, "CodeAgentSteps missing"));
        };
        CodeAgent::with_steps(&steps)
    };
    if let Err(err) = sessions.register_agent(agent) {
        return Err(refusal(
            headless,
            format!("failed to register CodeAgent: {err}"),
        ));
    }

    // Create or resume the session. A non-interactive run defaults to a fresh
    // id rather than the interactive `"default"`: two concurrent `--print`
    // invocations must not contend on one session.
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let executor = move || GraphExecutor::new().with_default_max_iterations(max_turns);
    // `insert` returns the displaced value; the seeder wants `()`.
    let init_context = |ctx: &mut SystemContext<'static>| {
        ctx.insert(agent_config.clone());
        ctx.insert(settings.clone());
        ctx.insert(turn_steering.clone());
        ctx.insert(attachment_store.clone());
        ctx.insert(background_tasks.clone());
    };

    let resumed = match resume_or_create_session(
        server,
        &sessions,
        &session_id,
        &agent_type,
        executor,
        init_context,
    )
    .await
    {
        Ok(resumed) => resumed,
        Err(SessionStartupError::Resume(err)) => {
            return Err(refusal(
                headless,
                format!("failed to resume session `{}`: {err}", session_id.as_str()),
            ));
        }
        Err(SessionStartupError::Create(err)) => {
            return Err(refusal(
                headless,
                format!("failed to create session: {err}"),
            ));
        }
    };
    tracing::info!(
        "session {} ({}) in {}",
        session_id.as_str(),
        if resumed { "resumed" } else { "new" },
        working_dir.display()
    );

    let navigation = SessionNavigation::new(sessions.clone(), session_id.clone())
        .with_store(session_store.clone());
    let _ = interrupt_navigation.set(navigation.clone());
    if !headless
        && let Err(err) = startup.register_status(
            &commands,
            &settings,
            &navigation,
            &working_dir,
            provider_report,
        )
    {
        return Err(refusal(
            headless,
            format!("failed to register /status: {err}"),
        ));
    }
    if !headless && let Err(err) = navigation.register(&commands) {
        return Err(refusal(
            headless,
            format!("failed to register session commands: {err}"),
        ));
    }
    let rewind = RewindControl::new(sessions.clone(), edit_journal.clone(), {
        let navigation = navigation.clone();
        move || navigation.current()
    });
    if !headless && let Err(err) = rewind.register(&commands) {
        return Err(refusal(
            headless,
            format!("failed to register rewind command: {err}"),
        ));
    }

    // `/diff` reads the same journal `/rewind` restores from, and nothing
    // else — reviewing must not be able to write through this seam.
    let review = SessionReview::new(edit_journal.clone(), {
        let navigation = navigation.clone();
        move || navigation.current()
    });
    if !headless && let Err(err) = review.register(&commands) {
        return Err(refusal(
            headless,
            format!("failed to register diff command: {err}"),
        ));
    }

    // `/tasks`, `/task`, `/fg` and `/stop`, plus the reporter that closes a
    // backgrounded run's block when it ends. Both read the executor the shell
    // plugin already published: the commands to list and stop a process, the
    // reporter to wait on a run the model started itself — a run the turn
    // gave up instead hands the reporter its own wait. Neither owns a process.
    // Interactive only — `--print` has no transcript to report into and no chord to press,
    // and a run it backgrounds has nobody waiting on it by construction.
    if !headless {
        let Some(executor) = server
            .get_global::<polaris::shell::ShellExecutor>()
            .map(|executor| polaris::shell::ShellExecutor::clone(&executor))
        else {
            return Err(refusal(headless, "ShellExecutor missing"));
        };
        let output = terminal_output
            .clone()
            .expect("interactive mode has terminal output");
        let task_navigation = navigation.clone();
        let reporter_executor = executor.clone();
        let task_commands = BackgroundCommands::new(
            executor,
            background_tasks.clone(),
            output.clone(),
            move || task_navigation.current(),
        );
        if let Err(err) = task_commands.register(&commands) {
            return Err(refusal(
                headless,
                format!("failed to register background task commands: {err}"),
            ));
        }
        let _ = background_commands.set(task_commands);

        let reporter_navigation = navigation.clone();
        let reporter = BackgroundReporter::new(
            reporter_executor,
            output,
            turn_steering.clone(),
            background_tasks.clone(),
            move || reporter_navigation.current(),
        );
        let mut backgrounded = backgrounded.expect("interactive mode takes the live table");
        tokio::spawn(async move {
            // One watcher per backgrounded run. Each closes exactly one block
            // and reconciles exactly one completion; none of them can start a
            // turn, because the only route into history is the mid-turn
            // steering boundary and that route queues nothing when idle.
            while let Some(number) = backgrounded.recv().await {
                let reporter = reporter.clone();
                tokio::spawn(async move {
                    reporter.watch(number).await;
                });
            }
        });
    }

    // Coding permission policy, injected into the session context the agent's
    // decisions actually read (`authorize_call` resolves
    // `PolicySnapshot::permission` over the registry's default).
    //
    // It cannot be a `ToolRegistry::set_permission` call from here: `ToolsPlugin`
    // moves the registry out of the resource table and into a *global* during
    // its `ready()` phase, so by the time `Server::finish()` has returned there
    // is no mutable handle left and the call would silently do nothing —
    // leaving `run_command` on the `#[tool]` default, `Allow`, and running
    // commands unprompted. `CodeAgent::setup` seeds the defaults while creating
    // or resuming the session, so this has to run after that and it overwrites
    // the snapshot wholesale.
    let policy = policy_snapshot(&permission_mode, graph_search);
    if let Err(err) = sessions
        .with_context(&session_id, move |ctx| {
            ctx.insert(policy);
        })
        .await
    {
        return Err(refusal(
            headless,
            format!("failed to apply tool policy: {err}"),
        ));
    }

    if let Some(exec_io) = exec_io {
        for note in &startup_notes {
            eprintln!("polaris: {note}");
        }
        // Accept before the turn starts, so the selection point the turn
        // already runs finds the work. A refusal here is a configuration
        // error — exit `2`, nothing queued, no model call, no turn.
        if let Some(spec) = cli.topology_task.as_deref() {
            match topology_work.accept_spec(spec) {
                Ok(accepted) => tracing::info!("{}", accepted.summary()),
                Err(refused) => return Err(refusal(headless, refused)),
            }
        } else if allow_auto_route
            && let Some(prompt) = cli.print.as_deref()
            && let Some(auto) = auto_route(&working_dir, prompt)
        {
            // A third, quiet acceptance path alongside `/topology` and
            // `--topology-task`: a `--print` prompt this host recognises as
            // one of the light-effort question shapes is accepted the same
            // way, marked `TaskOrigin::AutoRouted` so a routing guess that
            // turns out wrong falls through to the conversational turn
            // `run_headless` runs below rather than answering badly. The
            // *same* prompt is still sent into `exec_io`'s channel
            // unconditionally right after this, so that fallthrough costs
            // nothing extra to wire up.
            tracing::info!("auto-routed the prompt to the light-effort coding topology");
            coding_tasks.accept(auto);
        }
        let exit_code = run_headless(
            &sessions,
            &session_id,
            cli.print.clone(),
            exec_io,
            cli.output_format,
            max_output_tokens.get(),
        )
        .await;
        let mut exit_code = finish_host(server, exit_code).await;
        if let Some(trace) = &trajectory
            && let Err(error) = trace.finish(exit_code)
        {
            eprintln!("polaris: trajectory export failed: {error}");
            exit_code = 1;
        }
        return Ok(exit_code);
    }

    if let Err(err) = history.refresh(&sessions, &session_id).await {
        tracing::warn!("canonical transcript history unavailable: {err}");
    }

    // Spawn the turn-driver: one ReAct turn per user submission. Each turn begins
    // by blocking on `UserIO::receive()` inside the graph, so this loop is paced
    // by the user. It outlives nothing — the process exits when the TUI quits.
    let driver_output = terminal_output.expect("interactive mode has terminal output");
    // Open with what startup found out — which provider and credentials the
    // session is on, and any model name it had to choose between.
    for note in startup_notes {
        let _ = driver_output.send(IOMessage::system_text(note)).await;
    }
    // Then the ceiling, before the first prompt: a user who cannot raise
    // their permissions should read why from the ordinary session surface
    // rather than discover it by being refused.
    if let Some(ceiling) = settings.permission_ceiling() {
        let _ = driver_output
            .send(IOMessage::system_text(ceiling_notice(
                ceiling,
                &settings.snapshot().permission_mode,
            )))
            .await;
    }
    let driver_store = server
        .get_global::<OxigraphStore>()
        .map(|store| OxigraphStore::clone(&store));
    let rewind_application = RewindApplication::new(
        sessions.clone(),
        edit_journal.clone(),
        agent_config.clone(),
        settings.clone(),
        turn_steering.clone(),
        attachment_store.clone(),
        background_tasks.clone(),
        policy_snapshot(&permission_mode, graph_search),
        driver_store,
        working_dir.clone(),
    );
    let driver = Driver {
        sessions: sessions.clone(),
        status: status_emitter.clone(),
        history: history.clone(),
        navigation: navigation.clone(),
        output: driver_output,
        config: agent_config.clone(),
        settings: settings.clone(),
        steering: turn_steering.clone(),
        attachments: attachment_store.clone(),
        tasks: background_tasks.clone(),
        rewind: rewind.clone(),
        journal: edit_journal.clone(),
        rewind_application,
        agent_type,
        graph_search,
        max_turns,
    }
    .spawn();

    // Block until the user quits the TUI, or the host is asked to stop from
    // outside. `wait()` returns once the runtime task has exited and restored
    // the terminal from the alternate screen, so anything we print past this
    // point lands on the normal screen — and a signalled stop takes the same
    // route through `shutdown()` rather than dying in the alternate screen
    // with the terminal still in raw mode.
    let stop = match server.api::<TuiHandle>().cloned() {
        Some(handle) => {
            let waiter = handle.wait();
            tokio::pin!(waiter);
            let stop = tokio::select! {
                () = &mut waiter => None,
                signal = run::stop_signal() => Some(signal),
            };
            if let Some(signal) = stop {
                tracing::info!("{} received; stopping", signal.name());
                handle.shutdown();
                // `wait()` already took the join handle, so the server's own
                // cleanup phase cannot join it for us; finish the wait here.
                if tokio::time::timeout(TUI_SHUTDOWN_GRACE, waiter)
                    .await
                    .is_err()
                {
                    tracing::warn!("the TUI did not exit within its shutdown grace");
                }
            }
            stop
        }
        None => None,
    };

    // Persist the session that is actually in force — after a `/new` or a
    // `/resume` that is not `session_id`, which was bound before the TUI ever
    // ran. `shutdown_driver` quiesces a live turn, stops the loop so it
    // cannot start one that races the save, and then saves under a bound.
    let active = navigation.current();
    if let Err(message) = run::shutdown_driver(&sessions, &active, &driver, TURN_STOP_GRACE).await {
        tracing::error!("{message}");
        eprintln!("polaris: {message}");
    }

    // Print the session token + cost summary on the restored screen, priced
    // for the model the last turn actually ran on rather than the one the
    // process started with — `/model` may have moved it — and, when the
    // operator set one, what that spend was against. Still the one interactive
    // print site: the ceiling is a line of the same summary, not a second
    // emitter.
    if token_meter.total_tokens() > 0 {
        let served = settings
            .served_model()
            .unwrap_or_else(|| agent_config.model_id.clone());
        let mut summary = token_meter.summary(&served, max_output_tokens.get());
        if let Some(ceiling) = run_ceiling {
            let status = ceiling.evaluate(&token_meter.snapshot(&served));
            summary.push_str(&format!("\n  ceiling:  {}", status.statement()));
            if status.stopped {
                summary.push_str(" — reached, run stopped");
            }
        }
        println!("{summary}");
        let _ = std::io::stdout().flush();
    }

    // A signalled stop reports the Unix convention (`128 + signum`); a user
    // quitting the TUI still exits `0`.
    Ok(finish_host(server, stop.map_or(0, run::StopSignal::exit_code)).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::ModelRegistry;
    use polaris::models::llm::{
        AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
        LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
    };
    use polaris::plugins::{IOContent, IOProvider};
    use polaris::system::plugin::{Plugin, PluginId, Version};
    use polaris::tools::ToolsPlugin;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CleanupProbe(Arc<AtomicBool>);

    struct HangingCleanup;

    impl Plugin for CleanupProbe {
        const ID: &'static str = "polaris-code::cleanup-probe";
        const VERSION: Version = Version::new(0, 0, 1);

        fn build(&self, _server: &mut Server) {}

        async fn cleanup(&self, _server: &mut Server) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl Plugin for HangingCleanup {
        const ID: &'static str = "polaris-code::hanging-cleanup";
        const VERSION: Version = Version::new(0, 0, 1);

        fn build(&self, _server: &mut Server) {}

        async fn cleanup(&self, _server: &mut Server) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn host_exit_runs_plugin_cleanup() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let mut server = Server::new();
        server.add_plugins(CleanupProbe(cleaned.clone()));
        server.finish().await.expect("server should finish");

        let exit_code = finish_host(&mut server, 1).await;

        assert!(cleaned.load(Ordering::SeqCst), "plugin cleanup did not run");
        assert_eq!(exit_code, 1, "cleanup replaced the host exit status");
    }

    #[tokio::test]
    async fn host_exit_bounds_hung_plugin_cleanup() {
        let mut server = Server::new();
        server.add_plugins(HangingCleanup);
        server.finish().await.expect("server should finish");

        let exit_code = tokio::time::timeout(Duration::from_secs(1), finish_host(&mut server, 130))
            .await
            .expect("host cleanup exceeded its bound");
        assert_eq!(exit_code, 130, "timeout replaced the host exit status");
    }

    /// An `LlmProvider` that streams one fixed text answer and no tool
    /// calls, standing in for `AnthropicPlugin` the same way
    /// `tests/headless_exec.rs`'s own `ScriptedProvider` does: no
    /// `ANTHROPIC_API_KEY`, no network, so a session-creation test can prove
    /// a turn actually completes without spending on a live model call.
    struct ScriptedProvider;

    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        async fn generate(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            Ok(LlmResponse {
                content: vec![AssistantBlock::text("scripted answer")],
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
                id: None,
                model: None,
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmStream, GenerationError> {
            use futures_util::StreamExt as _;
            let events = vec![
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("scripted answer".to_string()),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        total_tokens: Some(15),
                        ..Default::default()
                    },
                    id: None,
                    model: None,
                }),
            ];
            Ok(futures_util::stream::iter(events).boxed())
        }
    }

    /// Registers [`ScriptedProvider`] once `ModelsPlugin` has built, the same
    /// seam `AnthropicPlugin` uses in production.
    struct ScriptedModelPlugin;

    impl Plugin for ScriptedModelPlugin {
        const ID: &'static str = "polaris-code::test-scripted-model";
        const VERSION: Version = Version::new(0, 1, 0);

        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<ModelsPlugin>()]
        }

        fn build(&self, server: &mut Server) {
            server
                .get_resource_mut::<ModelRegistry>()
                .expect("ModelsPlugin must build before ScriptedModelPlugin")
                .register_llm_provider(ScriptedProvider);
        }
    }

    /// The regression this test guards: "`--session <id>` cannot start a new
    /// session" — every eval episode passes a fresh, never-seen
    /// `--session <id>` (`evals/core/episode.py`), and a guard that treated any
    /// explicit-id resume failure as fatal (`SessionNotFound` included) would
    /// make `resume_or_create_session` fatal on it before a turn is ever
    /// attempted. Passes only while `SessionNotFound` alone falls through to
    /// `create_session_with_executor` regardless of how the id arrived.
    #[tokio::test]
    async fn a_never_seen_explicit_session_id_is_created_and_completes_a_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();

        let (provider_io, input_tx, mut output_rx) = ExecIOProvider::new(64);
        let provider_io = Arc::new(provider_io);

        let mut server = Server::new();
        server.insert_global(TokenMeter::new());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin)
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                data_dir.path().to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ExecIOPlugin::new(provider_io.clone()));
        server.finish().await.unwrap();

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        // Exactly the shape `episode.py::session_id` guarantees: an id this
        // store has never seen, because every episode's id is unique by
        // construction.
        let session_id = SessionId::from_string("eval-run-9-mode-0-1-instance-3");
        let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
        let agent_config = AgentConfig::new("scripted/test-model", workspace.path().to_path_buf());

        let resumed = resume_or_create_session(
            &server,
            sessions,
            &session_id,
            &agent_type,
            || GraphExecutor::new().with_default_max_iterations(25),
            |ctx| {
                ctx.insert(agent_config.clone());
            },
        )
        .await
        .expect("an id the store has never seen must be created, not fatal");
        assert!(!resumed, "a never-seen id is created, not resumed");

        input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text("say hi".to_string()),
            ))
            .await
            .unwrap();
        drop(input_tx);
        let drain = tokio::spawn(async move { while output_rx.recv().await.is_some() {} });

        let outcome = run::drive_turn(sessions, &session_id).await;
        provider_io.close().await;
        drain.await.unwrap();

        assert_eq!(outcome.reason, run::TurnReason::Completed);
        assert_eq!(outcome.reason.exit_code(), 0);
        assert_eq!(outcome.turn_number, 1, "the session's first turn completed");
    }

    /// The fatal path that must stay: a store entry that exists
    /// but fails to deserialize is not `SessionNotFound`, so it must not
    /// fall through to `create_session_with_executor` — it stays a `Resume`
    /// error the caller still reports as fatal.
    #[tokio::test]
    async fn a_corrupt_session_store_entry_is_distinguishable_from_never_seen() {
        let corrupt_dir = tempfile::tempdir().unwrap();
        tokio::fs::write(corrupt_dir.path().join("broken.json"), b"not-json")
            .await
            .unwrap();

        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                corrupt_dir.path().to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new());
        server.finish().await.unwrap();

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        let broken = SessionId::from_string("broken");
        let agent_type = AgentTypeId::from_name(CodeAgent::NAME);

        let outcome = resume_or_create_session(
            &server,
            sessions,
            &broken,
            &agent_type,
            || GraphExecutor::new().with_default_max_iterations(25),
            |_ctx| {},
        )
        .await;

        match outcome {
            Err(SessionStartupError::Resume(err)) => {
                assert!(
                    !matches!(err, SessionError::SessionNotFound(_)),
                    "a corrupt store entry must not be reported as SessionNotFound: {err}"
                );
            }
            other => panic!(
                "a corrupt store entry must fail the resume stage distinguishably from \
                 SessionNotFound, got {other:?}"
            ),
        }
        assert!(
            !sessions.list_live_sessions().contains(&broken),
            "a corrupt entry must not have silently gained a fresh live session"
        );
    }
}
