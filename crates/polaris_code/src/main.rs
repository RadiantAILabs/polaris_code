//! `polaris` — a terminal coding agent driven by an ontology + knowledge graph.
//!
//! Boots a Polaris [`Server`] with the ontrace RDF store as its spine: either
//! an interactive TUI conversation front end or a non-interactive `--print`
//! front end, both driving the same file + code-graph + shell + SPARQL
//! toolsets and the reflective [`CodeAgent`] through the shared run surface in
//! [`polaris_code::run`]. The store carries three named graphs — the
//! `TBox` (seeded ontology), the Code `ABox` (indexed repository), and the
//! Session `ABox` (the agent's reified trajectory).
//!
//! # Lifecycle
//!
//! `finish()` (build + ready: spawns the TUI in interactive mode, kicks code
//! indexing) → load seed ontologies → register the agent + create/resume a
//! session → drive the agent loop (looped for the TUI, once for `--print`) →
//! save the session → run bounded plugin cleanup on exit.
//!
//! # Usage
//!
//! ```bash
//! ANTHROPIC_API_KEY=… cargo run -p polaris_code --bin polaris -- [working_dir] [--session <id>]
//! ANTHROPIC_API_KEY=… cargo run -p polaris_code --bin polaris -- -p "add a README" --output-format json
//! ```

use std::io::Write;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use polaris::graph::GraphExecutor;
use polaris::models::anthropic::DEFAULT_MAX_TOKENS;
use polaris::models::pricing::PriceStore;
use polaris::models::{AnthropicPlugin, BedrockPlugin, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::checkpoint::CheckpointOrigin;
use polaris::sessions::{
    AgentTypeId, FileStore, SessionError, SessionId, SessionsAPI, SessionsPlugin,
};
use polaris::shell::{ShellConfig, ShellPlugin};
use polaris::system::param::SystemContext;
use polaris::system::server::Server;
use polaris::tools::{ToolPermission, ToolsPlugin};
use polaris_code::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, DEFAULT_MAX_TURNS, PermissionCeiling, PermissionMode,
    PolicySnapshot, UnattendedGrants, load_seed_ontologies,
};
use polaris_code::config::{DEFAULT_BEDROCK_MODEL, ModelCatalog};
use polaris_code::cosmos::composition::CompositionPolicy;
use polaris_code::cosmos::default_loop::DefaultLoopTopologyPackPlugin;
use polaris_code::cosmos::domains::DomainSelection;
use polaris_code::cosmos::host::{
    CodingTaskQueue, CodingTaskQueuePlugin, OperatorTopologyPin, auto_route,
};
use polaris_code::cosmos::inventory;
use polaris_code::cosmos::planner::GatePlannerMode;
use polaris_code::cosmos::policy as coding_policy;
use polaris_code::cosmos::topology::{CodingTopologyPackPlugin, HostCapabilities};
use polaris_code::exec::{
    ExecIOPlugin, ExecIOProvider, InventoryReport, OutputFormat, RunReport, stream_json_line,
    usage_message,
};
use polaris_code::progress::ProgressMiddlewarePlugin;
use polaris_code::run;
use polaris_code::state;
use polaris_code::terminal::{
    self, AttachmentStore, BackgroundCommands, BackgroundReporter, BackgroundTasks,
    CommandRegistry, EditJournal, RewindApplication, RewindControl, SessionAction,
    SessionNavigation, SessionReview, SessionSettings, TopologyListing, WorkspaceAttachments,
};
use polaris_code::tools::{
    CodeGraphPlugin, CodeToolsConfig, CodeToolsPlugin, GraphSearchMode, MemoryToolsPlugin,
    TaskToolsPlugin,
};
use polaris_code::usage::{RunCeiling, TaskCostLimit, TokenMeter};
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_cosmos::topology::{TopologyCatalog, TopologyCatalogPlugin};
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_ontrace::oxigraph::OxigraphStore;
use polaris_tui::panels::{ConversationPanelPlugin, HelpPanelPlugin, LogBuffer, LogPanelPlugin};
use polaris_tui::{TuiConfig, TuiHandle, TuiPlugin};
use tokio::sync::mpsc;

/// The tail of `--help`: every environment knob and every key binding in one
/// place, so the configuration surface is discoverable without reading the
/// source. Clap renders the flags above it; these two sections are the part it
/// cannot derive from the `Cli` struct.
const HELP: &str = "\
ENVIRONMENT:
    ANTHROPIC_API_KEY          Required for --provider anthropic (the
                               default). Anthropic API key. Not read for
                               --provider bedrock, which uses the AWS SDK's
                               own default credential chain instead (an
                               EC2/ECS instance role, AWS_PROFILE, or
                               AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY).
    POLARIS_CODE_MODEL         Model id to start on. Default:
                               anthropic/claude-sonnet-5. `--model` wins.
    POLARIS_CODE_MODELS        Comma-separated set of models this run can
                               reach and `/model` can switch between.
    POLARIS_CODE_GRAPH_SEARCH  Graph-search surface: curated | freeform | both.
                               Default: both.
    POLARIS_CODE_TOPOLOGY_PIN  Pins this run to one registered topology id (see
                               --list-topologies), for every accepted coding
                               task that names none of its own. Default:
                               unset (the selector picks). An unrecognised id
                               refuses at startup naming what is registered.
    POLARIS_CODE_COMPOSITION_POLICY
                               Composition policy for an accepted coding
                               task's selected topology: deterministic | model
                               | gates. Default: deterministic. An
                               unrecognised value refuses at startup naming
                               what is accepted.
    POLARIS_CODE_GATE_PLANNER  Which gate planner assembles a task-specific
                               plan when the policy above is gates: scripted |
                               model. Default: scripted. An unrecognised value
                               refuses at startup naming what is accepted.
    POLARIS_CODE_DOMAIN        Domain plan/notes selection for the gates
                               policy's statement-only edit shape: none |
                               django | sympy | sphinx. Default: none. An
                               unrecognised value refuses at startup naming
                               what is accepted.
    POLARIS_CODE_ARM           Which plan/notes treatment the domain above
                               selects: generic | generic-notes | expert |
                               experience | loop | lean | composed | fine | fine-ctx | expert2.
                               Default: generic.
                               Lean selects scripted gates by default.
                               generic-notes, expert, experience and composed
                               require a domain other than none; composed also
                               requires gates policy and model gate planner;
                               loop and lean (one conversation), fine (staged
                               proposal), and fine-ctx (staged proposal with
                               conversation handoff) require domain none.
                               An unrecognised value, or an arm paired with a
                               domain it cannot pair with, refuses at
                               startup.
    POLARIS_CODE_TESTENV       Keep the activated project runtime (PATH,
                               conda/venv vars) in shell tools (0/1, default 0).
    POLARIS_CODE_EXP_SUBMIT_GUARD  Refuse submit_answer while nothing is edited or
                               the last run_python check failed (0/1, default 0).
    POLARIS_CODE_EXP_REPO_LAYOUT  Put a short workspace listing in the first
                               message of a model step (0/1, default 0).
    POLARIS_CODE_EXP_REASONING_EFFORT  low/medium/high reasoning effort for
                               Bedrock OpenAI models (default: the model's own).
    POLARIS_CODE_LEAN_TRIM     Batched tool-output trim (0/1, default 1 for lean).
    POLARIS_CODE_LEAN_AUTOTEST Run related tests after lean edits (0/1, default 1).
    POLARIS_CODE_LEAN_LOCATE   Cheap location step (0/1, default 1 for lean).
    POLARIS_CODE_LEAN_CHECK_LOCATE  Read named lines before editing (0/1, default 1).
    POLARIS_CODE_LEAN_STABLE_TOOL_CHOICE  Keep final tool choice stable (0/1, default 1).
    POLARIS_CODE_LEAN_LOCATE_MODEL  Location model (default Claude Haiku 4.5).
    POLARIS_CODE_LEAN_CASCADE  Check after 18 turns and continue on failure (0/1, default 1).
    POLARIS_CODE_LEAN_TWM      Typed working memory: replaces the lean or loop
                               step's growing transcript with a small set of
                               typed slots, rebuilt on a trigger (0/1, default 0).
    POLARIS_CODE_LEAN_TWM_REBUILD_TOKENS  Rebuild above this whole-prompt size
                               (characters / 4, default 40000 tokens).
    POLARIS_CODE_LEAN_TWM_NOTE  Offer the model a small note slot (0/1, default 0).
    POLARIS_CODE_LEAN_TWM_TESTSTATUS  Capture test output as a typed status
                               entry (0/1, default 0).
    POLARIS_CODE_LEAN_TWM_CODEMAP  Free, no-model-call code map in the
                               unchanging first block (0/1, default 0).
    POLARIS_CODE_MAX_COST_USD  Per coding task estimated USD ceiling. Lean
                               defaults to 1.50; 0 disables it. Other arms
                               default off.
    POLARIS_CODE_PROMPT_CACHE  0/false/off disables cache-aware context and
                               provider prompt caching. Default: on.
    POLARIS_CODE_REDACT       0/false/off turns off secret redaction of tool
                               output. Default: on.
    POLARIS_CODE_ONTOLOGY_DIR  Ontology-pack directory seeded into the TBox at
                               boot. Default: unset (seeding is skipped).

A .env file in the working directory is loaded automatically.

KEYS (in the TUI):
    Tab / Shift-Tab   cycle panels (Conversation, Logs, Help)
    Alt-1 … Alt-9     jump to panel N
    Ctrl-O            expand the per-turn node timeline (Conversation)
    Ctrl-L            toggle single / tiled layout
    Ctrl-C / Ctrl-Q   quit
";

/// Destructive command patterns refused outright by the shell, regardless of
/// the per-tool confirm gate — a backstop, not the primary permission layer.
const DENIED_COMMANDS: &[&str] = &["rm -rf *", "rm -rf /", "sudo *", ":(){ :|:& };:", "mkfs *"];

/// Output-channel capacity for the non-interactive front end's `UserIO`.
const EXEC_OUTPUT_BUFFER: usize = 64;

/// The environment variable `AnthropicPlugin` reads the model key from.
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// How long a signalled stop waits for the TUI runtime to leave the alternate
/// screen before giving up and printing over it.
const TUI_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long the shutdown path waits for an interrupted turn to record itself
/// and release the session before the driver is aborted out from under it.
const TURN_STOP_GRACE: Duration = Duration::from_secs(2);

/// How much of the spend ceiling an interactive run may use before it says so
/// in the transcript — the "you are approaching the limit" point, warned once.
const CEILING_WARN_FRACTION: f64 = 0.8;

/// The environment variable that sets the per-request output token limit the
/// Anthropic provider is registered with.
const MAX_OUTPUT_TOKENS_VAR: &str = "POLARIS_CODE_MAX_OUTPUT_TOKENS";
const TASK_MAX_COST_VAR: &str = "POLARIS_CODE_MAX_COST_USD";
const TESTENV_VAR: &str = "POLARIS_CODE_TESTENV";

/// The output token limit a run uses when [`MAX_OUTPUT_TOKENS_VAR`] is unset:
/// four times the provider's [`DEFAULT_MAX_TOKENS`], so a long edit or
/// answer is not cut off at the provider's conservative default.
const DEFAULT_MAX_OUTPUT_TOKENS: NonZeroU32 =
    NonZeroU32::new(DEFAULT_MAX_TOKENS.get() * 4).unwrap();

/// Resolve the output token limit this run registers its model provider with.
///
/// Unset, or set to whitespace, keeps [`DEFAULT_MAX_OUTPUT_TOKENS`].
///
/// Anything else has to be a positive integer. The limit is load-bearing: a
/// response that reaches it is truncated, and today's agent loop does not read
/// the finish reason, so a truncated answer looks like a finished one. A typo
/// is therefore refused as a configuration error rather than falling back to
/// the default or being clamped to something the operator did not ask for.
/// The return type is [`NonZeroU32`], so zero is refused by the parse itself
/// rather than by a separate check the two could drift apart from. Values
/// above a model's ceiling are not checked here either — the provider passes
/// them through and the API rejects them by name.
fn resolve_max_output_tokens(raw: Option<&str>) -> Result<NonZeroU32, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_MAX_OUTPUT_TOKENS);
    };
    value
        .parse::<NonZeroU32>()
        .map_err(|_| format!("{MAX_OUTPUT_TOKENS_VAR} must be a positive integer, got {value:?}"))
}

/// The environment variable that sets how many times the agent loop may go
/// round in one turn.
const MAX_TURNS_VAR: &str = "POLARIS_CODE_MAX_TURNS";

/// Resolve the agent loop's turn limit for this run.
///
/// Unset, or set to whitespace, keeps [`DEFAULT_MAX_TURNS`]. Anything else has
/// to be a positive integer; a typo is refused as a configuration error rather
/// than silently falling back to the default.
fn resolve_max_turns(raw: Option<&str>) -> Result<usize, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_MAX_TURNS);
    };
    value
        .parse::<NonZeroUsize>()
        .map(NonZeroUsize::get)
        .map_err(|_| format!("{MAX_TURNS_VAR} must be a positive integer, got {value:?}"))
}

/// The environment variable that pins this whole run to one registered
/// topology, for every accepted coding task that names none of its own.
const TOPOLOGY_PIN_VAR: &str = "POLARIS_CODE_TOPOLOGY_PIN";

/// Resolve the operator's run-wide topology pin against the sealed catalog
/// this run just built, from outside the binary and with no rebuild —
/// `POLARIS_CODE_TOPOLOGY_PIN` read the same way `POLARIS_CODE_GRAPH_SEARCH`
/// already is (see `graph_search` below).
///
/// Unset, or set to whitespace, resolves to `Ok(None)` — a run that
/// configures nothing selects exactly as it always has, byte for byte.
///
/// Anything else has to name one of `known`, the sealed catalog's own
/// registration ids (the same set `--list-topologies` and `/topologies`
/// show). An unrecognised name is refused here, at startup, before any turn
/// runs — never accepted and silently left for a later turn's selection point
/// to fail on, and never quietly substituted with the selector's own default,
/// which would make a whole benchmark sweep measure the same unpinned run
/// three times and report it as three.
fn resolve_topology_pin(raw: Option<&str>, known: &[String]) -> Result<Option<String>, String> {
    let Some(name) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if known.iter().any(|candidate| candidate == name) {
        Ok(Some(name.to_owned()))
    } else {
        Err(format!(
            "{TOPOLOGY_PIN_VAR} `{name}` is not a registered topology; known topologies are {} \
             (see --list-topologies)",
            known.join(", ")
        ))
    }
}

/// The environment variable that selects the finite-composition policy every
/// accepted coding task's selected topology composes under.
const COMPOSITION_POLICY_VAR: &str = "POLARIS_CODE_COMPOSITION_POLICY";

/// Resolve the operator's run-wide composition policy from outside the
/// binary, read and validated once here — the same pattern
/// [`resolve_topology_pin`] follows for its own env var, and for the same
/// reason: a typo must refuse at startup, naming the values it accepts,
/// rather than being discovered deep inside the first accepted coding task's
/// own selection point
/// ([`composition::CompositionPolicyError::InvalidAnswer`]), which ends the
/// whole session (`DriverStep::Stop`) instead of the process never starting
/// one.
///
/// Unset, or set to whitespace, resolves to `Ok(None)` — a run that
/// configures nothing selects the deterministic policy exactly as it always
/// has. Anything else has to be one of [`CompositionPolicy::parse`]'s two
/// accepted spellings, `deterministic` or `model`.
fn resolve_composition_policy(raw: Option<&str>) -> Result<Option<CompositionPolicy>, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    CompositionPolicy::parse(value)
        .map(Some)
        .map_err(|error| format!("{COMPOSITION_POLICY_VAR} {error}"))
}

/// The environment variable that selects which gate planner assembles a
/// task-specific plan when the composition policy above is `gates`.
const GATE_PLANNER_VAR: &str = "POLARIS_CODE_GATE_PLANNER";

/// Resolve the operator's gate planner mode the same way
/// [`resolve_composition_policy`] resolves its own env var: validated once
/// here, before any turn, rather than discovered deep inside the first
/// accepted coding task's gate-plan revision loop.
///
/// Unset, or set to whitespace, resolves to `Ok(None)` — a run that
/// configures nothing uses the scripted planner exactly as it always has.
/// Anything else has to be one of [`GatePlannerMode::parse`]'s two accepted
/// spellings, `scripted` or `model`. Meaningless unless the composition
/// policy is also `gates`, but validated unconditionally: a typo refuses at
/// startup regardless of which policy is active, rather than only surfacing
/// once an operator also happens to set the other variable.
fn resolve_gate_planner_mode(raw: Option<&str>) -> Result<Option<GatePlannerMode>, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    GatePlannerMode::parse(value)
        .map(Some)
        .map_err(|error| format!("{GATE_PLANNER_VAR} {error}"))
}

/// Select the scripted gates path when lean is named without a separate
/// composition setting. The harness sends its deterministic baseline even
/// for a lean run, so the arm also overrides that baseline to gates.
fn lean_composition_policy(
    selection: Option<DomainSelection>,
    policy: Option<CompositionPolicy>,
    planner: Option<GatePlannerMode>,
) -> Result<Option<CompositionPolicy>, String> {
    if !selection.is_some_and(|selection| selection.arm == polaris_code::cosmos::domains::Arm::Lean)
    {
        return Ok(policy);
    }
    if policy == Some(CompositionPolicy::Model) {
        return Err("arm lean requires scripted gates, not model composition".to_owned());
    }
    if planner == Some(GatePlannerMode::Model) {
        return Err("arm lean requires POLARIS_CODE_GATE_PLANNER=scripted".to_owned());
    }
    Ok(Some(CompositionPolicy::Gates))
}

/// The environment variable that selects the domain plan/notes a gate-plan
/// turn's statement-only edit shape uses.
const DOMAIN_VAR: &str = "POLARIS_CODE_DOMAIN";

/// The environment variable that selects which arm (plan/notes treatment)
/// the domain above uses.
const ARM_VAR: &str = "POLARIS_CODE_ARM";

/// Resolve the operator's domain/arm selection from outside the binary,
/// following the same startup-validation discipline
/// [`resolve_composition_policy`] does: a typo, or an arm requested without a
/// domain, refuses at startup naming what is accepted, rather than being
/// discovered per turn deep inside gate-plan composition.
///
/// Unset for both, resolves to `Ok(None)` — a run that configures neither
/// installs no global, and every turn runs the unmodified generic plan
/// exactly as it did before domain plans existed. Setting only one of the
/// two still validates the pair, defaulting the other to its own baseline
/// (`none`/`generic`) first: `POLARIS_CODE_ARM=expert` alone, with no domain
/// named, is exactly as much a configuration error as naming an unknown
/// domain would be.
fn resolve_domain_selection(
    domain_raw: Option<&str>,
    arm_raw: Option<&str>,
) -> Result<Option<DomainSelection>, String> {
    let domain = domain_raw.map(str::trim).filter(|value| !value.is_empty());
    let arm = arm_raw.map(str::trim).filter(|value| !value.is_empty());
    if domain.is_none() && arm.is_none() {
        return Ok(None);
    }
    DomainSelection::resolve(domain.unwrap_or("none"), arm.unwrap_or("generic"))
        .map(Some)
        .map_err(|error| format!("{DOMAIN_VAR}/{ARM_VAR} {error}"))
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

/// A terminal coding agent built on Polaris. Mirrors Claude Code's CLI shape:
/// no positional subcommand, `-p`/`--print` for one non-interactive turn.
#[derive(Parser)]
#[command(name = "polaris", version)]
// `--print` and `--topology-task` are the two non-interactive entry points and
// they are exclusive: one drives a conversational turn, the other a topology
// turn, and a single run is one turn. The group also gives `--allow-edit` and
// `--allow-command` one name to require, since both grants apply to either.
#[command(group = clap::ArgGroup::new("unattended").args(["print", "topology_task"]))]
// Clap lists the flags; `HELP` adds the two sections it cannot derive — every
// environment knob and the TUI keys — so one `--help` shows the whole surface.
#[command(after_help = HELP)]
struct Cli {
    /// Working directory (defaults to the current directory).
    working_dir: Option<PathBuf>,
    /// Run one turn non-interactively with this prompt, then exit, instead of
    /// starting the TUI.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,
    /// Run one topology turn non-interactively over this coding-work spec,
    /// then exit. The spec is the same `key=value` text `/topology` takes:
    /// `file=<path> find=<text> replace=<text> [file2=… find2=… replace2=…
    /// ...] [scope=<path>[,<path>...]] [command=<program>
    /// [command-args=<arg>[,<arg>...]]] [stub=…] [intent=…] [verifier=…]
    /// [ref=…] [budget-ms=…]`. Needs `--allow-edit`, because a topology
    /// edits files and cannot stop to ask.
    #[arg(long, value_name = "SPEC")]
    topology_task: Option<String>,
    /// Resume this session id. A non-interactive run generates a fresh id
    /// when absent, rather than the interactive default `"default"` — two
    /// concurrent `--print` runs must not contend on one session.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Model to run on, as `provider/model` or an unambiguous model name.
    /// Wins over `POLARIS_CODE_MODEL`. Must name one of the models this run
    /// can reach (`POLARIS_CODE_MODELS` declares the set); an unknown name is
    /// refused at startup, naming what was available.
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
    /// Model provider to run on. `bedrock` uses the AWS Bedrock Converse API
    /// through the default AWS credential chain (an EC2/ECS instance role,
    /// `AWS_PROFILE`, or `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`) instead
    /// of `ANTHROPIC_API_KEY`. With no `--model`/`POLARIS_CODE_MODEL`, a
    /// `bedrock` run starts on `DEFAULT_BEDROCK_MODEL`.
    #[arg(
        long,
        env = "POLARIS_CODE_PROVIDER",
        value_enum,
        default_value = "anthropic",
        value_name = "PROVIDER"
    )]
    provider: Provider,
    /// AWS region for `--provider bedrock`. Unset leaves region resolution to
    /// the AWS SDK's own default chain (`AWS_REGION`, `AWS_DEFAULT_REGION`, or
    /// the local AWS config profile). Ignored for `--provider anthropic`.
    #[arg(long, value_name = "REGION")]
    region: Option<String>,
    /// Plan arm, overriding `POLARIS_CODE_ARM` (including loop and lean).
    #[arg(long, value_name = "ARM")]
    arm: Option<String>,
    /// Output format for `--print` — `text`, `json` or `stream-json`;
    /// ignored in interactive mode.
    #[arg(long, default_value = "text", value_name = "FORMAT")]
    output_format: OutputFormat,
    /// Interactive permission posture. Non-interactive runs always use an
    /// immutable unattended grant instead.
    #[arg(long, value_enum, value_name = "MODE")]
    permission_mode: Option<InteractivePermissionMode>,
    /// Highest permission posture this process may ever reach. Nothing inside
    /// the session — a command, a setting, a prompt — can exceed it.
    /// Interactive-only, like `--permission-mode`.
    #[arg(long, value_enum, value_name = "MODE")]
    permission_ceiling: Option<InteractivePermissionMode>,
    /// Allow `edit_file` during this unattended run.
    #[arg(long, requires = "unattended")]
    allow_edit: bool,
    /// Allow shell commands matching this existing shell-policy glob during
    /// this unattended run. May be repeated.
    #[arg(long, requires = "unattended", value_name = "GLOB")]
    allow_command: Vec<String>,
    /// List the topologies this build registered, and whether each can run
    /// here, then exit. Honours `--output-format`; starts no agent turn.
    #[arg(long, conflicts_with = "print")]
    list_topologies: bool,
    /// Stop the run once its estimated spend reaches this many US dollars.
    /// Refused at startup if the model in use has no price, because an
    /// unpriced model must not become an unlimited one.
    #[arg(long, value_name = "USD")]
    max_cost_usd: Option<f64>,
    /// Stop the run once it has billed this many tokens, counting every tier.
    /// Enforceable for any model, priced or not.
    #[arg(long, value_name = "TOKENS")]
    max_tokens: Option<u64>,
}

/// Permission modes a human may choose when starting an interactive session.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum InteractivePermissionMode {
    Confirm,
    AutoEdit,
    Plan,
}

impl From<InteractivePermissionMode> for PermissionMode {
    fn from(mode: InteractivePermissionMode) -> Self {
        match mode {
            InteractivePermissionMode::Confirm => Self::Confirm,
            InteractivePermissionMode::AutoEdit => Self::AutoEdit,
            InteractivePermissionMode::Plan => Self::Plan,
        }
    }
}

/// Model provider a run can select, via `--provider`.
///
/// Mirrors `AnthropicPlugin` / `BedrockPlugin` registration one-for-one — see
/// the `match` in `main()` that builds the server. Adding a third provider
/// means adding a variant here and one more arm there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Provider {
    Anthropic,
    Bedrock,
}

/// Resolve the posture this process starts in, and the ceiling it may never
/// exceed, from the operator's flags.
///
/// Three rules, in the order they apply:
///
/// * **`--print` takes neither flag.** It already refuses `--permission-mode`;
///   it refuses `--permission-ceiling` for that reason and one more. An
///   unattended posture is incomparable with the attended ladder a ceiling is
///   defined over (`PermissionMode::attended_rung`), so a ceiling over a
///   `--print` run could only do nothing or silently redefine the run's grants
///   — and the second is precisely "the ceiling became a way to express a
///   `--print` permission mode". The posture of a `--print` run is already
///   immutable and already fixed by the same operator on the same command
///   line, through `--allow-edit` / `--allow-command`.
/// * **An explicit `--permission-mode` above the ceiling is refused**, not
///   clamped: the operator wrote both flags, and a contradiction between them
///   is worth a line of output rather than a silent reinterpretation.
/// * **The implicit default is clamped into the ceiling.** `confirm` is the
///   default posture, so `--permission-ceiling plan` on its own starts the
///   session in `plan`. Clamping only ever lowers.
fn startup_permissions(
    cli: &Cli,
    headless: bool,
) -> Result<(PermissionMode, Option<PermissionCeiling>), String> {
    if headless {
        if cli.permission_mode.is_some() {
            return Err(
                "--permission-mode is interactive-only; a non-interactive run uses unattended \
                 grants"
                    .into(),
            );
        }
        if cli.permission_ceiling.is_some() {
            return Err(
                "--permission-ceiling is interactive-only; a --print run is already \
                 unattended and its posture is fixed by --allow-edit/--allow-command"
                    .into(),
            );
        }
        return Ok((
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: cli.allow_edit,
                run_command_patterns: cli.allow_command.clone(),
            }),
            None,
        ));
    }

    let ceiling = match cli.permission_ceiling.map(PermissionMode::from) {
        // `InteractivePermissionMode` has no unattended variant, so this is
        // unreachable today; it stays a refusal rather than an `expect` so the
        // one posture that cannot be a ceiling can never become one by a later
        // edit to that enum.
        Some(mode) => Some(
            PermissionCeiling::new(mode.clone())
                .ok_or_else(|| format!("`{}` cannot be a permission ceiling", mode.label()))?,
        ),
        None => None,
    };
    let mode = match (&ceiling, cli.permission_mode.map(PermissionMode::from)) {
        (Some(ceiling), Some(requested)) if !ceiling.admits(&requested) => {
            return Err(format!(
                "{}; choose a mode at or below the ceiling",
                ceiling.refusal(requested.label())
            ));
        }
        (_, Some(requested)) => requested,
        (Some(ceiling), None) => ceiling.clamp(PermissionMode::default()),
        (None, None) => PermissionMode::default(),
    };
    Ok((mode, ceiling))
}

/// The one line an interactive session opens with when a ceiling is in force.
///
/// A posture the user cannot raise has to be readable without asking for it, so
/// the session says so up front; `/permission` then advertises only the modes
/// the ceiling admits and names the ceiling in every refusal.
fn ceiling_notice(ceiling: &PermissionCeiling, active: &PermissionMode) -> String {
    format!(
        "Permission ceiling `{}` is in force for this process. This session starts in `{}`, \
         and nothing inside it — a command, a setting or a prompt — can raise the posture \
         above the ceiling.",
        ceiling.label(),
        active.label()
    )
}

fn shell_config(working_dir: &std::path::Path, mode: &PermissionMode) -> ShellConfig {
    let allowed_commands = match mode {
        PermissionMode::Unattended(grants) => grants.run_command_patterns.clone(),
        PermissionMode::Confirm | PermissionMode::AutoEdit | PermissionMode::Plan => {
            vec!["*".to_string()]
        }
    };
    ShellConfig::new()
        .with_working_dir(working_dir)
        .with_timeout(120)
        .with_max_output_bytes(64 * 1024)
        .with_allowed_dirs(vec![working_dir.to_path_buf()])
        .with_allowed_commands(allowed_commands)
        .with_denied_commands(DENIED_COMMANDS.iter().map(|s| (*s).to_string()).collect())
}

/// Keep the host's activated project runtime (conda env, virtualenv) in
/// shell children when `POLARIS_CODE_TESTENV=1`. Only runtime-setup names
/// pass; full inheritance would also hand provider credentials to
/// model-authored scripts. The benchmark harness activates the image's
/// environment before it starts this process.
fn project_shell_environment(config: ShellConfig, enabled: bool) -> ShellConfig {
    if !enabled {
        return config;
    }
    config.with_env_allowlist(
        [
            "PATH",
            "CONDA_PREFIX",
            "CONDA_DEFAULT_ENV",
            "CONDA_SHLVL",
            "VIRTUAL_ENV",
            "PYTHONPATH",
            "LD_LIBRARY_PATH",
            "LANG",
            "LC_ALL",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
}

fn policy_snapshot(mode: &PermissionMode, graph_search: GraphSearchMode) -> PolicySnapshot {
    let mut policy = PolicySnapshot::defaults();
    policy.permission = mode.permission_policy();
    if !graph_search.freeform_enabled() {
        policy.tool_palette.denied.push("sparql_query".to_string());
        policy
            .permission
            .overrides
            .insert("sparql_query".to_string(), ToolPermission::Deny);
    }
    policy
}

/// Report a startup failure and end the process.
///
/// A non-interactive run exits `2` — the contract's "usage or configuration
/// error", kept distinct from `1`, which means the turn itself
/// ran and failed. The interactive front end keeps the plain failure code it
/// has always exited with, so the TUI path's behaviour is unchanged.
fn fatal(headless: bool, message: &str) -> ! {
    eprintln!("polaris: {message}");
    std::process::exit(if headless { 2 } else { 1 });
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
async fn list_topologies(working_dir: &std::path::Path, format: OutputFormat) -> ! {
    let mut server = Server::new();
    add_topology_packs(&mut server);
    if let Err(err) = server.finish().await {
        fatal(true, &format!("failed to seal the topology catalog: {err}"));
    }
    let Some(catalog) = server.get_global::<TopologyCatalog>() else {
        fatal(true, "TopologyCatalog missing");
    };
    let snapshot = match catalog.snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => fatal(true, &format!("the topology catalog is not sealed: {err}")),
    };
    let observed_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default();
    let listed = match inventory::observe(
        &snapshot,
        working_dir,
        HostCapabilities::hermetic(),
        observed_at_ms,
    ) {
        Ok(listed) => listed,
        Err(err) => fatal(true, &format!("failed to read the topology catalog: {err}")),
    };
    let report = InventoryReport::new(listed);
    // The third and last print site in this file, and like the other two every
    // byte of it is rendered inside `exec::report`.
    println!("{}", report.stdout_payload(format));
    std::process::exit(report.exit_code());
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

/// Builds the `BedrockPlugin` for `--provider bedrock`.
///
/// With no `--region`, defers entirely to `BedrockPlugin::from_env()`, which
/// resolves both credentials and region through the AWS SDK's own default
/// chain. With `--region`, loads that chain once here — the same credential
/// resolution, with the region pinned — since `BedrockPlugin::from_env()`
/// has no parameter to override it.
async fn bedrock_plugin(
    region: Option<String>,
    max_output_tokens: NonZeroU32,
    pricing_store: PriceStore,
) -> BedrockPlugin {
    let plugin = match region {
        Some(region) => {
            let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new(region))
                .load()
                .await;
            BedrockPlugin::from_sdk_config(sdk_config)
        }
        None => BedrockPlugin::from_env(),
    };
    let plugin = plugin
        .with_max_tokens(max_output_tokens)
        .with_pricing_store(pricing_store);
    match reasoning_effort_fields(std::env::var(REASONING_EFFORT_VAR).ok().as_deref()) {
        Some(fields) => plugin.with_additional_model_request_fields(fields),
        None => plugin,
    }
}

/// `POLARIS_CODE_EXP_REASONING_EFFORT`: the reasoning effort (`low`,
/// `medium`, `high`) a Bedrock `OpenAI` model is asked for on every request.
const REASONING_EFFORT_VAR: &str = "POLARIS_CODE_EXP_REASONING_EFFORT";

/// The Converse request fields for a reasoning effort, or `None` when unset,
/// blank, or not one of `low`/`medium`/`high` (the model's default applies).
fn reasoning_effort_fields(value: Option<&str>) -> Option<serde_json::Value> {
    let effort = value?.trim();
    matches!(effort, "low" | "medium" | "high")
        .then(|| serde_json::json!({"reasoning": {"effort": effort}}))
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    let headless = cli.print.is_some() || cli.topology_task.is_some();
    let (permission_mode, permission_ceiling) =
        startup_permissions(&cli, headless).unwrap_or_else(|message| fatal(headless, &message));

    let working_dir = cli
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let working_dir = match working_dir.canonicalize() {
        Ok(dir) => dir,
        Err(err) => fatal(headless, &format!("invalid working directory: {err}")),
    };

    // Listing is not a run: it seals the same packs, reads the sealed catalog
    // and exits before anything else is assembled.
    if cli.list_topologies {
        list_topologies(&working_dir, cli.output_format).await;
    }

    // Which models this run can reach, and which one it starts on. Resolved
    // here — before the server is built, before a session exists and before a
    // provider is ever called — so a name nothing can serve ends the process
    // with the set that would have worked, not with a provider error at the
    // first token. The set is narrowed to the registered providers once the
    // registry exists, further down.
    let declared_models = match cli.provider {
        Provider::Anthropic => ModelCatalog::from_env(),
        Provider::Bedrock => ModelCatalog::from_env_with_default(DEFAULT_BEDROCK_MODEL, &[]),
    };
    let model_id = declared_models
        .select(cli.model.as_deref())
        .unwrap_or_else(|err| fatal(headless, &err.to_string()));

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
        .unwrap_or_else(|message| fatal(headless, &message));
    // `POLARIS_CODE_GATE_PLANNER` selects which gate planner builds a
    // task-specific plan when the policy above is `gates`. Validated
    // unconditionally and at the same point, for the same reason.
    let gate_planner_raw = std::env::var(GATE_PLANNER_VAR).ok();
    let gate_planner_mode = resolve_gate_planner_mode(gate_planner_raw.as_deref())
        .unwrap_or_else(|message| fatal(headless, &message));
    // `POLARIS_CODE_DOMAIN`/`POLARIS_CODE_ARM` select the domain plan/notes a
    // `gates`-policy turn's statement-only edit shape uses. Validated here,
    // at the same point and for the same reason as the two variables above.
    let domain_raw = std::env::var(DOMAIN_VAR).ok();
    let arm_raw = std::env::var(ARM_VAR).ok();
    let domain_selection = resolve_domain_selection(
        domain_raw.as_deref(),
        cli.arm.as_deref().or(arm_raw.as_deref()),
    )
    .unwrap_or_else(|message| fatal(headless, &message));
    let composition_policy =
        lean_composition_policy(domain_selection, composition_policy, gate_planner_mode)
            .unwrap_or_else(|message| fatal(headless, &message));
    let lean_options = domain_selection
        .is_some_and(|s| s.arm == polaris_code::cosmos::domains::Arm::Lean)
        .then(|| {
            polaris_code::cosmos::planner::lean::LeanOptions::from_env()
                .unwrap_or_else(|message| fatal(headless, &message))
        });
    if domain_selection
        .is_some_and(|selection| selection.arm == polaris_code::cosmos::domains::Arm::Composed)
        && (composition_policy != Some(polaris_code::cosmos::composition::CompositionPolicy::Gates)
            || gate_planner_mode != Some(GatePlannerMode::Model))
    {
        fatal(
            headless,
            "arm composed requires POLARIS_CODE_COMPOSITION_POLICY=gates and POLARIS_CODE_GATE_PLANNER=model",
        );
    }

    // The operator's spend ceiling, validated before anything starts. Two
    // ways to get exit 2 here rather than a run that quietly ignores a limit:
    // a limit that cannot bound a run (zero, negative, not a number), and a
    // money limit over a model with no price — "unpriced" must never mean
    // "unlimited", so the run refuses rather than proceeds unbounded.
    let run_ceiling = match RunCeiling::from_limits(cli.max_cost_usd, cli.max_tokens) {
        Ok(ceiling) => ceiling,
        Err(err) => fatal(headless, &err.to_string()),
    };
    let configured_task_cost = if let Some(options) = &lean_options {
        options.max_cost_usd
    } else {
        std::env::var(TASK_MAX_COST_VAR).ok().and_then(|raw| {
            let limit = raw.trim().parse::<f64>().unwrap_or_else(|_| {
                fatal(
                    headless,
                    "POLARIS_CODE_MAX_COST_USD must be finite and nonnegative",
                )
            });
            if limit == 0.0 { None } else { Some(limit) }
        })
    };
    let pricing_store = PriceStore::live();
    let task_cost_limit = configured_task_cost.map(|limit| {
        let limit = TaskCostLimit::new(limit).unwrap_or_else(|_| {
            fatal(
                headless,
                "POLARIS_CODE_MAX_COST_USD must be a positive finite number",
            )
        });
        if polaris_code::usage::price_for(&pricing_store, &model_id).is_none() {
            fatal(
                headless,
                "POLARIS_CODE_MAX_COST_USD requires a priced model",
            );
        }
        limit
    });
    if let Some(ceiling) = run_ceiling
        && let Some(why) = ceiling.unenforceable_for(&pricing_store, &model_id)
    {
        fatal(headless, &why);
    }

    // `POLARIS_CODE_MAX_OUTPUT_TOKENS` sets the `max_tokens` every request
    // carries. Resolved once here and handed to two places that must agree:
    // `AnthropicPlugin`, which sends it, and the non-interactive report, which
    // states it. One local, so they cannot drift.
    let configured_max_output_tokens = std::env::var(MAX_OUTPUT_TOKENS_VAR).ok();
    let max_output_tokens = resolve_max_output_tokens(configured_max_output_tokens.as_deref())
        .unwrap_or_else(|message| fatal(headless, &message));
    let max_turns = resolve_max_turns(std::env::var(MAX_TURNS_VAR).ok().as_deref())
        .unwrap_or_else(|message| fatal(headless, &message));
    let agent_config = AgentConfig::new(model_id, working_dir.clone())
        .with_graph_search(graph_search)
        .with_prompt_cache(prompt_cache);
    // Everything this run writes that is not the user's work goes under one
    // directory, and that directory is invisible to git. Otherwise the agent's
    // own transcript rides along in whatever is built from the working tree —
    // a prediction patch, a `git commit -a`, a CI diff. See `state`'s module
    // docs for why the state stays repository-local rather than moving out.
    let state_dir = state::state_dir(&working_dir)
        .unwrap_or_else(|err| fatal(headless, &format!("failed to open state directory: {err}")));
    let edit_journal = EditJournal::open(&working_dir)
        .unwrap_or_else(|err| fatal(headless, &format!("failed to open rewind journal: {err}")));

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
    // destructive patterns stay authoritative: they are refused even when the
    // user has already approved the call.
    let shell_config = project_shell_environment(
        shell_config(&working_dir, &permission_mode),
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
    let attachment_settings = Arc::new(std::sync::OnceLock::<SessionSettings>::new());

    // Managed shell runs pushed out of a turn, by the `Ctrl-T` chord or by the
    // model's own `run_in_background` call. The table is shared:
    // `execute_tools` fills it through the sink it installs, the chord, the
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
    let background_commands = Arc::new(std::sync::OnceLock::<BackgroundCommands>::new());

    let history = Arc::new(terminal::history::CanonicalHistory::default());
    let session_id = match cli.session.clone() {
        Some(id) => SessionId::from_string(id),
        None if headless => SessionId::new(),
        None => SessionId::from_string("default"),
    };
    let interrupt_api = Arc::new(std::sync::OnceLock::<SessionsAPI>::new());
    let interrupt_navigation = Arc::new(std::sync::OnceLock::<SessionNavigation>::new());
    let status_emitter = Arc::new(std::sync::OnceLock::<run::StatusEmitter>::new());
    let turn_steering = if headless {
        terminal::TurnSteering::default()
    } else {
        terminal::TurnSteering::enabled()
    };

    // `AnthropicPlugin::from_env` panics when the key is missing, which exits
    // `101` through a backtrace. Neither front end should meet that: the
    // non-interactive contract owes its caller exit code `2` and one line of
    // explanation for a configuration error, and an interactive
    // run owes a person the same fact before the TUI takes the screen. A key
    // set to whitespace is missing too — the provider would reject it on the
    // first turn, long after the message would have helped. `--provider
    // bedrock` has no equivalent gate: `BedrockPlugin::from_env` resolves
    // credentials through the AWS SDK's own default chain (instance role,
    // profile, or explicit keys), which has no single "is it set" env var to
    // check up front — a missing/invalid credential surfaces from the SDK
    // itself when the plugin builds.
    if cli.provider == Provider::Anthropic && api_key_missing(std::env::var(API_KEY_VAR)) {
        if headless {
            fatal(
                headless,
                &format!("{API_KEY_VAR} is not set; a non-interactive run needs a model key"),
            );
        }
        eprintln!(
            "polaris: {API_KEY_VAR} is not set.\n\
             Set it in your environment or a .env file in the working directory, e.g.:\n\
             \n    export {API_KEY_VAR}=sk-ant-...\n\n\
             Run `polaris --help` for all options."
        );
        std::process::exit(1);
    }

    // Built once and shared: the session plugin persists through it, and
    // `/sessions` reads the saved records back through the same store so a
    // picker row can say more than an identifier. It lives under the run's
    // state directory, so a saved transcript stays out of the working tree.
    let session_store: Arc<dyn polaris::sessions::SessionStore> =
        Arc::new(FileStore::new(state_dir.join("sessions")));

    let mut server = Server::new();
    server.add_plugins(ModelsPlugin);
    match cli.provider {
        Provider::Anthropic => {
            server.add_plugins(
                AnthropicPlugin::from_env(API_KEY_VAR)
                    .with_max_tokens(max_output_tokens)
                    .with_pricing_store(pricing_store.clone()),
            );
        }
        Provider::Bedrock => {
            server.add_plugins(
                bedrock_plugin(cli.region.clone(), max_output_tokens, pricing_store.clone()).await,
            );
        }
    }
    server
        // Tools: file primitives, the SPARQL store + tools, the code graph, shell.
        .add_plugins(ToolsPlugin)
        .add_plugins(CodeToolsPlugin::new(
            CodeToolsConfig::new(&working_dir).with_edit_journal(edit_journal.clone()),
        ))
        .add_plugins(OxigraphPlugin::in_memory().with_tools(true))
        .add_plugins(CodeGraphPlugin::new(&working_dir).with_search_mode(graph_search))
        // Reflective memory: memoize/recall structural conclusions,
        // content-hash-invalidated against the cited spans.
        .add_plugins(MemoryToolsPlugin::new(&working_dir))
        .add_plugins(ShellPlugin::new(shell_config))
        // The model's own way to push a command out of a turn and read it
        // back. Over the executor `ShellPlugin` just published and the table
        // above — no second process registry.
        .add_plugins(TaskToolsPlugin::new(background_tasks.clone()))
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
        // One shared log buffer feeds both the Logs panel and the conversation
        // panel's Ctrl-O node timeline (which buckets entries by their
        // enclosing node span). The log panel installs the global tracing
        // subscriber over it.
        let log_buffer = LogBuffer::with_capacity(2_000);
        let callback_api = interrupt_api.clone();
        let callback_navigation = interrupt_navigation.clone();
        let callback_background = background_commands.clone();
        server
            // Terminal UI + panels. The conversation panel publishes the
            // `UserIO` the agent's `receive_user_input`/`confirm` flow binds to.
            // Enable mouse capture so the wheel scrolls the conversation
            // transcript and logs (hold Shift/Option for native
            // text-selection in most terminals).
            .add_plugins(TuiPlugin::new(TuiConfig::default().with_mouse(true)))
            // Only buffer `polaris*` traces — drop the noise from transitive
            // deps (hyper, reqwest, oxigraph internals, …) so the Logs panel
            // and the conversation timeline show the agent's own activity.
            .add_plugins(
                LogPanelPlugin::with_buffer(log_buffer.clone()).with_target_prefixes(["polaris"]),
            )
            .add_plugins(HelpPanelPlugin);
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

    // Register the token meter as a global so the agent's `act` system can read
    // it via `Res<TokenMeter>`. Must precede any `create_context()` call.
    server.insert_global(token_meter.clone());
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
        fatal(headless, &format!("failed to start the server: {err}"));
    }

    // The reachable catalog: the declared set, less every model whose provider
    // this run did not register. Re-resolving the startup model against it
    // turns "the plugin that serves this model is not installed" into the same
    // startup refusal as an unknown name, rather than a first-token failure.
    let catalog = {
        let Some(models) = server.get_global::<polaris::models::ModelRegistry>() else {
            fatal(headless, "ModelRegistry missing");
        };
        declared_models.restricted_to_providers(&models.llm_provider_names())
    };
    if let Err(err) = catalog.resolve(&agent_config.model_id) {
        fatal(headless, &err.to_string());
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
        fatal(
            headless,
            &format!("failed to register settings commands: {err}"),
        );
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
        Some(Err(err)) => fatal(
            headless,
            &format!("the topology catalog is not sealed: {err}"),
        ),
        None => fatal(headless, "TopologyCatalog missing"),
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
        fatal(headless, &message);
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
        fatal(
            headless,
            &format!("failed to register topology command: {err}"),
        );
    }

    // `/topologies`, interactive only — the same snapshot above.
    if !headless {
        let listing = TopologyListing::new(
            topology_catalog,
            working_dir.clone(),
            HostCapabilities::hermetic(),
        );
        if let Err(err) = listing.register(&commands) {
            fatal(headless, &format!("failed to register /topologies: {err}"));
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
        fatal(headless, "PersistenceAPI missing");
    };
    let Some(sessions) = server.api::<SessionsAPI>().cloned() else {
        fatal(headless, "SessionsAPI missing");
    };
    let _ = interrupt_api.set(sessions.clone());
    sessions.set_serializers(persistence.serializers());
    if let Err(err) = sessions.register_agent(CodeAgent) {
        fatal(headless, &format!("failed to register CodeAgent: {err}"));
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
        &server,
        &sessions,
        &session_id,
        &agent_type,
        executor,
        init_context,
    )
    .await
    {
        Ok(resumed) => resumed,
        Err(SessionStartupError::Resume(err)) => fatal(
            headless,
            &format!("failed to resume session `{}`: {err}", session_id.as_str()),
        ),
        Err(SessionStartupError::Create(err)) => {
            fatal(headless, &format!("failed to create session: {err}"))
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
    if !headless && let Err(err) = navigation.register(&commands) {
        fatal(
            headless,
            &format!("failed to register session commands: {err}"),
        );
    }
    let rewind = RewindControl::new(sessions.clone(), edit_journal.clone(), {
        let navigation = navigation.clone();
        move || navigation.current()
    });
    if !headless && let Err(err) = rewind.register(&commands) {
        fatal(
            headless,
            &format!("failed to register rewind command: {err}"),
        );
    }

    // `/diff` reads the same journal `/rewind` restores from, and nothing
    // else — reviewing must not be able to write through this seam.
    let review = SessionReview::new(edit_journal.clone(), {
        let navigation = navigation.clone();
        move || navigation.current()
    });
    if !headless && let Err(err) = review.register(&commands) {
        fatal(headless, &format!("failed to register diff command: {err}"));
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
            fatal(headless, "ShellExecutor missing");
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
            fatal(
                headless,
                &format!("failed to register background task commands: {err}"),
            );
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
    // decisions actually read (`execute_tools` resolves
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
        fatal(headless, &format!("failed to apply tool policy: {err}"));
    }

    if let Some(exec_io) = exec_io {
        // Accept before the turn starts, so the selection point the turn
        // already runs finds the work. A refusal here is a configuration
        // error — exit `2`, nothing queued, no model call, no turn.
        if let Some(spec) = cli.topology_task.as_deref() {
            match topology_work.accept_spec(spec) {
                Ok(accepted) => tracing::info!("{}", accepted.summary()),
                Err(refused) => fatal(headless, &refused.to_string()),
            }
        } else if let Some(prompt) = cli.print.as_deref()
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
        let exit_code = finish_host(&mut server, exit_code).await;
        std::process::exit(exit_code);
    }

    if let Err(err) = history.refresh(&sessions, &session_id).await {
        tracing::warn!("canonical transcript history unavailable: {err}");
    }

    // Spawn the turn-driver: one ReAct turn per user submission. Each turn begins
    // by blocking on `UserIO::receive()` inside the graph, so this loop is paced
    // by the user. It outlives nothing — the process exits when the TUI quits.
    let driver_sessions = sessions.clone();
    let driver_status = status_emitter.clone();
    let driver_history = history.clone();
    let driver_navigation = navigation.clone();
    let driver_output = terminal_output.expect("interactive mode has terminal output");
    // Open with the ceiling, before the first prompt: a user who cannot raise
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
    let driver_config = agent_config.clone();
    let driver_settings = settings.clone();
    let driver_steering = turn_steering.clone();
    let driver_attachments = attachment_store.clone();
    let driver_tasks = background_tasks.clone();
    let driver_rewind = rewind.clone();
    let driver_journal = edit_journal.clone();
    let driver_store = server
        .get_global::<OxigraphStore>()
        .map(|store| OxigraphStore::clone(&store));
    let driver_working_dir = working_dir.clone();
    let driver_policy = policy_snapshot(&permission_mode, graph_search);
    let rewind_application = RewindApplication::new(
        driver_sessions.clone(),
        driver_journal.clone(),
        driver_config.clone(),
        driver_settings.clone(),
        driver_steering.clone(),
        driver_attachments.clone(),
        driver_tasks.clone(),
        driver_policy.clone(),
        driver_store.clone(),
        driver_working_dir.clone(),
    );
    let driver_agent_type = agent_type;
    let driver = tokio::spawn(async move {
        // One-shot latch for the "approaching the ceiling" notice below.
        let mut ceiling_warned = false;
        // `run::drive_until`'s loop, reproduced here only so the canonical
        // history the conversation panel browses is refreshed after each turn
        // — including a failed one, whose partial transcript stays browsable.
        // The durability point, the failure classification and the ceiling
        // are `drive_until`'s, called rather than restated.
        let mut budget = run::FailureBudget::new();
        loop {
            let driver_id = driver_navigation.current();
            // A rewind point is a before-input boundary. Capturing here happens
            // before the graph accepts the next prompt, including the first.
            match driver_sessions
                .checkpoints()
                .capture(&driver_id, CheckpointOrigin::Manual)
                .await
            {
                Ok(checkpoint) => {
                    if let Ok(revision) = driver_sessions.checkpoints().revision(&driver_id) {
                        let _ = driver_journal.add_point(
                            &driver_id,
                            revision,
                            checkpoint.id,
                            format!("before turn {}", checkpoint.resume_turn),
                        );
                    }
                }
                Err(error) => tracing::warn!("rewind boundary unavailable: {error}"),
            }
            let (outcome, step) =
                run::drive_turn_step(&driver_sessions, &driver_id, driver_status.get()).await;
            if let Err(err) = driver_history.refresh(&driver_sessions, &driver_id).await {
                tracing::warn!("canonical transcript history unavailable: {err}");
            }
            // A turn that answered submitted coding work owes its submitter
            // which of the three answers came back — a topology ran, none was
            // eligible, or none started. Display-only, like the rewind report
            // below: the canonical transcript belongs to the conversation, and
            // a topology turn never wrote to it.
            if let Some(report) = terminal::topology::turn_report(&outcome) {
                let _ = driver_output.send(report).await;
            }
            // The durability point. A checkpoint capture is in-memory, so
            // until this call returns, everything since launch lives only in
            // this process. It runs between turns, where the session's
            // context lock is free, and it names `driver_id` — the session
            // this iteration actually drove, which after a `/new` or
            // `/resume` is not the one the process launched with.
            if let Err(message) = run::persist_turn(&driver_sessions, &driver_id).await {
                tracing::error!("{message}");
                let _ = driver_output
                    .send(
                        IOMessage::system_text(message)
                            .with_metadata(polaris::plugins::IO_TYPE, "error"),
                    )
                    .await;
            }
            // Whatever ended the turn — completion, failure, or the user's
            // Ctrl-C — a task that outlived it must not become invisible. The
            // count follows the turn's own last row, including `cancelled`,
            // which is exactly where "I interrupted it and it kept running" is
            // otherwise correct but surprising.
            let _ = driver_output
                .send(polaris_tui::commands::report_status(
                    &polaris_tui::commands::StatusReport::default()
                        .with_tasks_running(driver_tasks.running()),
                ))
                .await;
            if let Some(request) = driver_rewind.take_pending() {
                let outcome = rewind_application.apply(&driver_id, &request).await;
                let report = RewindApplication::report(&request, &outcome);
                let _ = driver_output.send(report.into_message()).await;
                let _ = driver_history.refresh(&driver_sessions, &driver_id).await;
            }
            if let Some(action) = driver_navigation.take_pending() {
                let target = match &action {
                    SessionAction::New(id) | SessionAction::Resume(id) => id.clone(),
                };
                if let Err(err) = driver_sessions.save_session(&driver_id).await {
                    let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session switch cancelled: failed to save `{}`: {err}. Fix storage and retry.",
                            driver_id.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                    continue;
                }
                let transition = match action {
                    SessionAction::Resume(_) => {
                        driver_sessions
                            .resume_session_with_executor(
                                driver_sessions.create_context(),
                                &target,
                                executor(),
                                {
                                    let cfg = driver_config.clone();
                                    let settings = driver_settings.clone();
                                    let steering = driver_steering.clone();
                                    let attachments = driver_attachments.clone();
                                    let tasks = driver_tasks.clone();
                                    move |ctx| {
                                        ctx.insert(cfg.clone());
                                        ctx.insert(settings.clone());
                                        ctx.insert(steering.clone());
                                        ctx.insert(attachments.clone());
                                        ctx.insert(tasks.clone());
                                    }
                                },
                            )
                            .await
                    }
                    SessionAction::New(_) => driver_sessions.create_session_with_executor(
                        driver_sessions.create_context(),
                        &target,
                        &driver_agent_type,
                        executor(),
                        {
                            let cfg = driver_config.clone();
                            let settings = driver_settings.clone();
                            let steering = driver_steering.clone();
                            let attachments = driver_attachments.clone();
                            let tasks = driver_tasks.clone();
                            move |ctx| {
                                ctx.insert(cfg.clone());
                                ctx.insert(settings.clone());
                                ctx.insert(steering.clone());
                                ctx.insert(attachments.clone());
                                ctx.insert(tasks.clone());
                            }
                        },
                    ),
                };
                if let Err(err) = transition {
                    let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session switch failed for `{}`: {err}. The previous session remains active; fix the id or storage and retry.",
                            target.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                    continue;
                }
                // Modes are deliberately not persisted. Every session entered
                // through `/new` or `/resume` starts in fail-closed Confirm,
                // regardless of the posture of the session being left.
                let fresh_policy = policy_snapshot(&PermissionMode::Confirm, graph_search);
                driver_settings.reset(driver_config.model_id.clone());
                if let Err(err) = driver_sessions
                    .with_context(&target, move |ctx| {
                        ctx.insert(fresh_policy);
                    })
                    .await
                {
                    let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session `{}` opened but its default permission mode could not be applied: {err}",
                            target.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                    continue;
                }
                if let Err(err) = driver_history.refresh(&driver_sessions, &target).await {
                    let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session `{}` resumed but its canonical transcript could not be replayed: {err}",
                            target.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                    continue;
                }
                let _ = driver_output
                    .send(polaris_tui::commands::session_replay_start(target.as_str()))
                    .await;
                if let Ok(messages) = driver_history.replay() {
                    for message in messages {
                        let _ = driver_output.send(message).await;
                    }
                }
                let _ = driver_output
                    .send(polaris_tui::commands::session_replay_end(target.as_str()))
                    .await;
                driver_navigation.set_current(target);
                continue;
            }
            // The spend ceiling, where a human can see it. The live cost row
            // already shows what the run has spent; this adds the two things
            // the row cannot say — that a limit exists and is close, and that
            // it has been reached — into the transcript the user is reading.
            if let Some(ceiling) = &outcome.ceiling {
                if ceiling.stopped {
                    let _ = driver_output
                        .send(
                            IOMessage::system_text(format!(
                                "Spend ceiling reached: {}. No further turn will start. \
                                 Raise --max-cost-usd / --max-tokens and restart to continue.",
                                ceiling.statement()
                            ))
                            .with_metadata(polaris::plugins::IO_TYPE, "error"),
                        )
                        .await;
                    // The composer must stop looking ready: this session is
                    // taking no more work, whatever the user types.
                    let _ = driver_output
                        .send(polaris_tui::commands::report_unavailable(format!(
                            "spend ceiling reached — {}",
                            ceiling.statement()
                        )))
                        .await;
                } else if !ceiling_warned
                    && ceiling
                        .used_fraction()
                        .is_some_and(|used| used >= CEILING_WARN_FRACTION)
                {
                    // Once, at the crossing. Repeating it every turn would
                    // train the user to ignore the line that matters.
                    ceiling_warned = true;
                    let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Approaching the spend ceiling: {}.",
                            ceiling.statement()
                        )))
                        .await;
                }
            }
            // A turn failure is the user's news, not the Logs panel's. It
            // lands in the transcript either way; whether the driver keeps
            // serving turns afterwards is `drive_turn_step`'s judgement,
            // bounded by the consecutive-failure ceiling. A ceiling stop
            // carries a sentence too, but the block above has already put it
            // on screen, and it is not a failure — so it is neither repeated
            // nor logged as one.
            let step = budget.absorb(step);
            if outcome.stopped_at_ceiling() {
                tracing::info!("spend ceiling reached, stopping driver");
            } else if let Some(message) = step.message() {
                tracing::error!("{message}");
                let _ = driver_output
                    .send(
                        IOMessage::system_text(message)
                            .with_metadata(polaris::plugins::IO_TYPE, "error"),
                    )
                    .await;
            }
            if !step.continues() {
                return;
            }
        }
    });

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
    let exit_code = finish_host(&mut server, stop.map_or(0, run::StopSignal::exit_code)).await;

    // The agent loop holds long-lived clients (HTTP connection pools, the
    // oxigraph store) whose background workers can keep the tokio runtime from
    // draining promptly when it's dropped at the end of `main`. Plugins have
    // had their bounded cleanup opportunity, so exit directly rather than wait
    // indefinitely for the runtime itself to drain.
    std::process::exit(exit_code);
}

/// Drive the one non-interactive turn: enqueue the prompt if there is one,
/// run it, drain and emit the result per `format`, save the session, and
/// return the process exit code.
///
/// `prompt` is `None` for a `--topology-task` run, which has nothing to say to
/// the model: the work is already in the queue, so the selection point at the
/// top of the turn answers it and returns before `process_turn_with` is
/// reached.
///
/// This is the front end, not the emitter: every byte it writes is rendered by
/// [`RunReport`] / [`stream_json_line`], which are library-side and tested.
/// Stdout carries exactly the result (`text`/`json`) or exactly the message
/// stream (`stream-json`, printed by the concurrent drain below); the identity
/// triple and every diagnostic go to stderr.
///
/// Both machine-readable formats carry the run's token and cost accounting:
/// `json` as the `usage` member of its object, `stream-json` as the final
/// message on the stream (`io_type = "usage"`). `text` reserves stdout for the
/// assistant's answer, so a consumer that needs accounting uses `json`.
///
/// A run given `--max-cost-usd` / `--max-tokens` that reached its limit
/// returns [`CEILING_EXIT_CODE`](run::CEILING_EXIT_CODE) and states what it
/// spent against what it was allowed on stderr; `json` also carries the same
/// facts as its `ceiling` member. The session is saved first, exactly as for
/// any other ending.
async fn run_headless(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    prompt: Option<String>,
    exec_io: (
        Arc<ExecIOProvider>,
        mpsc::Sender<IOMessage>,
        mpsc::Receiver<IOMessage>,
    ),
    format: OutputFormat,
    max_output_tokens: u32,
) -> i32 {
    let (provider, input_tx, mut output_rx) = exec_io;
    let status = run::StatusEmitter::new(provider.clone());

    // Enqueue before the turn starts: `receive_user_input` swallows a closed
    // channel rather than erroring, so a send racing the turn could hang.
    if let Some(prompt) = prompt {
        let _ = input_tx
            .send(IOMessage::from_external("cli", IOContent::Text(prompt)))
            .await;
    }
    drop(input_tx);

    // Drain concurrently so `stream-json` prints as messages arrive rather
    // than after the turn completes. `text`/`json` need nothing from this
    // stream but must still drain it so the bounded output channel never
    // backpressures the turn.
    let drain = tokio::spawn(async move {
        while let Some(message) = output_rx.recv().await {
            if format.streams_messages() {
                match stream_json_line(&message) {
                    Ok(line) => println!("{line}"),
                    Err(err) => eprintln!("polaris: failed to serialize a message: {err}"),
                }
            }
        }
    });

    let turn = run::drive_turn_with_status(sessions, session_id, Some(&status));
    tokio::pin!(turn);
    let mut interrupted = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut turn => break outcome,
            signal = run::stop_signal() => {
                if interrupted {
                    // Deliberate hard escape: unlike the cooperative first
                    // signal, a second one does not wait or save.
                    std::process::exit(signal.exit_code());
                }
                // A supervisor's SIGTERM is an orderly stop request, so it
                // takes the same cooperative route as SIGINT: cancel the
                // turn, then fall through to the save and the report below.
                interrupted = run::request_interrupt(sessions, session_id);
            }
        }
    };

    // `stream-json` closes with the run's own accounting, so a streaming
    // consumer reads exactly the versioned object `json` embeds instead of
    // summing the live `io_type=tokens` display deltas. Sent through the
    // provider rather than printed here, so the drain renders it with the same
    // line renderer as every other message and it lands last, in stream order.
    if format.streams_messages()
        && let Some(usage) = outcome.usage.as_ref()
    {
        let _ = provider.send(usage_message(usage)).await;
    }

    // Let the drain task see end-of-stream, then wait for every stream-json
    // line to be flushed before printing the result / returning.
    provider.close().await;
    let _ = drain.await;

    if let Err(message) = run::persist_turn(sessions, session_id).await {
        eprintln!("polaris: {message}");
    }

    let report = RunReport::new(session_id, &outcome, max_output_tokens);
    // Identity on stderr for every format, so `text` and `stream-json` stdout
    // stay exactly the result / the message stream; `json` embeds the same
    // three identifiers in its object, since its stdout *is* the result.
    eprintln!("{}", report.identity_line());
    if let Some(line) = report.model_line() {
        eprintln!("{line}");
    }
    if let Some(line) = report.error_line() {
        eprintln!("{line}");
    }
    // What was spent against what was allowed, for every format: `text` and
    // `stream-json` have no structured place to carry it, and a run that
    // stopped early must say why even to a caller reading only stderr.
    if let Some(line) = report.ceiling_line() {
        eprintln!("{line}");
    }
    if let Some(payload) = report.stdout_payload(format) {
        println!("{payload}");
    }
    let _ = std::io::stdout().flush();

    report.exit_code()
}

/// Resolve the ontology-pack directory: `POLARIS_CODE_ONTOLOGY_DIR` if it names
/// an existing directory, else `None`.
fn ontology_dir() -> Option<PathBuf> {
    let dir = std::env::var("POLARIS_CODE_ONTOLOGY_DIR").ok()?;
    let p = PathBuf::from(dir);
    p.is_dir().then_some(p)
}

/// Whether `ANTHROPIC_API_KEY` is effectively absent: unset, or set but blank.
/// Takes the `env::var` result so the predicate is pure and unit-testable.
fn api_key_missing(var: Result<String, std::env::VarError>) -> bool {
    var.map(|v| v.trim().is_empty()).unwrap_or(true)
}

/// Resolve an on-by-default boolean env gate: `0`/`false`/`off` (case- and
/// whitespace-insensitive) disable it; unset or anything else leaves it on.
/// Takes the `env::var` result so the predicate is pure and unit-testable.
fn env_flag_enabled(var: Result<String, std::env::VarError>) -> bool {
    var.map(|s| {
        !matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        )
    })
    .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::ModelRegistry;
    use polaris::models::llm::{
        AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
        LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
    };
    use polaris::shell::{ShellExecutor, ShellPermission};
    use polaris::system::plugin::{Plugin, PluginId, Version};
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

    /// Parse a command line exactly as the operator would type it, so these
    /// tests exercise the flags rather than a hand-built `Cli`.
    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["polaris"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("the command line parses")
    }

    fn resolve(args: &[&str]) -> Result<(PermissionMode, Option<PermissionCeiling>), String> {
        let parsed = cli(args);
        let headless = parsed.print.is_some();
        startup_permissions(&parsed, headless)
    }

    #[test]
    fn a_startup_mode_above_the_ceiling_is_refused_and_names_it() {
        let refusal = resolve(&[
            "--permission-ceiling",
            "confirm",
            "--permission-mode",
            "auto-edit",
        ])
        .expect_err("`auto-edit` exceeds a `confirm` ceiling");
        assert!(refusal.contains("ceiling"), "{refusal}");
        assert!(refusal.contains("confirm"), "{refusal}");
        assert!(refusal.contains("auto-edit"), "{refusal}");
    }

    #[test]
    fn a_startup_mode_at_or_below_the_ceiling_is_kept() {
        for requested in ["plan", "confirm"] {
            let (mode, ceiling) = resolve(&[
                "--permission-ceiling",
                "confirm",
                "--permission-mode",
                requested,
            ])
            .expect("a mode at or below the ceiling starts");
            assert_eq!(mode.label(), requested);
            assert_eq!(ceiling.expect("the ceiling is in force").label(), "confirm");
        }
    }

    /// The default posture is `confirm`, which a stricter ceiling lowers rather
    /// than being bypassed by.
    #[test]
    fn the_default_posture_is_clamped_into_the_ceiling() {
        let (mode, ceiling) =
            resolve(&["--permission-ceiling", "plan"]).expect("a ceiling alone starts a session");
        assert_eq!(mode, PermissionMode::Plan);
        assert_eq!(ceiling.expect("the ceiling is in force").label(), "plan");

        let (mode, _) = resolve(&["--permission-ceiling", "auto-edit"])
            .expect("a ceiling above the default starts a session");
        assert_eq!(
            mode,
            PermissionMode::Confirm,
            "a ceiling above the default must not raise it"
        );
    }

    #[test]
    fn an_unconstrained_start_is_unchanged() {
        let (mode, ceiling) = resolve(&[]).expect("the plain interactive start");
        assert_eq!(mode, PermissionMode::Confirm);
        assert!(ceiling.is_none());

        let (mode, ceiling) =
            resolve(&["--permission-mode", "auto-edit"]).expect("an explicit interactive posture");
        assert_eq!(mode, PermissionMode::AutoEdit);
        assert!(ceiling.is_none());
    }

    /// `--print` stays fail-closed unattended and takes neither flag: its
    /// posture is fixed by `--allow-edit`/`--allow-command` on the same command
    /// line, and a ceiling must not become a second way to express one.
    #[test]
    fn print_remains_fail_closed_and_takes_neither_permission_flag() {
        assert!(
            resolve(&["--print", "hi", "--permission-mode", "plan"])
                .expect_err("--permission-mode is interactive-only")
                .contains("--permission-mode")
        );
        let refusal = resolve(&["--print", "hi", "--permission-ceiling", "plan"])
            .expect_err("--permission-ceiling is interactive-only");
        assert!(refusal.contains("--permission-ceiling"), "{refusal}");

        let (mode, ceiling) = resolve(&["--print", "hi"]).expect("a plain unattended run");
        assert_eq!(
            mode,
            PermissionMode::Unattended(UnattendedGrants::default()),
            "an ungranted --print run stays fail-closed"
        );
        assert!(ceiling.is_none(), "a --print run carries no ceiling");

        let (mode, _) = resolve(&[
            "--print",
            "hi",
            "--allow-edit",
            "--allow-command",
            "cargo *",
        ])
        .expect("a granted unattended run");
        assert_eq!(
            mode,
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: true,
                run_command_patterns: vec!["cargo *".into()],
            })
        );
    }

    /// The session opens by saying what the user cannot do, in both labels.
    #[test]
    fn the_opening_notice_names_the_ceiling_and_the_starting_posture() {
        let notice = ceiling_notice(
            &PermissionCeiling::new(PermissionMode::Confirm).expect("confirm is a ceiling"),
            &PermissionMode::Plan,
        );
        assert!(notice.contains("ceiling"), "{notice}");
        assert!(notice.contains("`confirm`"), "{notice}");
        assert!(notice.contains("`plan`"), "{notice}");
    }

    /// Shorthand for a known-nonzero literal in a test assertion.
    fn nz(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("test literal must be nonzero")
    }

    /// A run that configures nothing gets four times the provider's default.
    #[test]
    fn an_unset_output_limit_uses_the_binary_default() {
        assert_eq!(
            resolve_max_output_tokens(None),
            Ok(DEFAULT_MAX_OUTPUT_TOKENS)
        );
        assert_eq!(
            resolve_max_output_tokens(Some("")),
            Ok(DEFAULT_MAX_OUTPUT_TOKENS)
        );
        assert_eq!(
            resolve_max_output_tokens(Some("  ")),
            Ok(DEFAULT_MAX_OUTPUT_TOKENS)
        );
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS.get(), 16384);
    }

    /// A configured limit is taken as written — not clamped to any ceiling.
    #[test]
    fn a_configured_output_limit_is_taken_as_written() {
        assert_eq!(resolve_max_output_tokens(Some("32000")), Ok(nz(32_000)));
        assert_eq!(resolve_max_output_tokens(Some(" 64000 ")), Ok(nz(64_000)));
        assert_eq!(resolve_max_output_tokens(Some("1")), Ok(nz(1)));
        assert_eq!(
            resolve_max_output_tokens(Some("10000000")),
            Ok(nz(10_000_000)),
            "an over-ceiling value is passed through for the API to reject"
        );
    }

    /// An unset or blank turn limit keeps the default; a positive integer is used.
    #[test]
    fn the_turn_limit_defaults_and_accepts_a_positive_integer() {
        assert_eq!(resolve_max_turns(None), Ok(DEFAULT_MAX_TURNS));
        assert_eq!(resolve_max_turns(Some("  ")), Ok(DEFAULT_MAX_TURNS));
        assert_eq!(resolve_max_turns(Some(" 250 ")), Ok(250));
    }

    /// Zero, a negative, a float and a typo are refused as a turn limit.
    #[test]
    fn an_invalid_turn_limit_is_refused() {
        for raw in ["0", "-1", "1.5", "lots"] {
            let error = resolve_max_turns(Some(raw))
                .expect_err("an invalid turn limit must be refused, not defaulted");
            assert!(
                error.contains(MAX_TURNS_VAR) && error.contains("positive integer"),
                "unhelpful error for {raw:?}: {error}"
            );
        }
    }

    /// Zero, a negative, a float and a typo are refused, not defaulted.
    #[test]
    fn an_invalid_output_limit_is_refused() {
        for raw in ["0", "-1", "1.5", "lots", "4096tok"] {
            let error = resolve_max_output_tokens(Some(raw))
                .expect_err("an invalid limit must be refused, not defaulted");
            assert!(
                error.contains(MAX_OUTPUT_TOKENS_VAR) && error.contains("positive integer"),
                "unhelpful error for {raw:?}: {error}"
            );
        }
    }

    /// A run that configures nothing pins nothing — `select_and_run` sees no
    /// `OperatorTopologyPin` at all, byte for byte as it did before this
    /// variable existed.
    #[test]
    fn an_unset_topology_pin_resolves_to_no_pin() {
        let known = vec!["code.direct-edit".to_owned()];
        assert_eq!(resolve_topology_pin(None, &known), Ok(None));
        assert_eq!(resolve_topology_pin(Some(""), &known), Ok(None));
        assert_eq!(resolve_topology_pin(Some("   "), &known), Ok(None));
    }

    /// A name the sealed catalog actually holds is accepted, trimmed of
    /// incidental whitespace.
    #[test]
    fn a_known_topology_pin_is_accepted() {
        let known = vec![
            "code.direct-edit".to_owned(),
            "code.test-gated-edit".to_owned(),
        ];
        assert_eq!(
            resolve_topology_pin(Some("code.direct-edit"), &known),
            Ok(Some("code.direct-edit".to_owned()))
        );
        assert_eq!(
            resolve_topology_pin(Some(" code.test-gated-edit "), &known),
            Ok(Some("code.test-gated-edit".to_owned())),
        );
    }

    /// An unrecognised id is refused, naming what is registered — never
    /// silently substituted with the selector's own default, which would make
    /// a benchmark sweep measure the same unpinned run under two names.
    #[test]
    fn an_unknown_topology_pin_is_refused_and_names_what_is_registered() {
        let known = vec![
            "code.direct-edit".to_owned(),
            "code.test-gated-edit".to_owned(),
        ];
        let error = resolve_topology_pin(Some("code.no-such-thing"), &known)
            .expect_err("an unknown pin must be refused, not defaulted");
        assert!(error.contains(TOPOLOGY_PIN_VAR), "{error}");
        assert!(error.contains("code.no-such-thing"), "{error}");
        assert!(error.contains("code.direct-edit"), "{error}");
        assert!(error.contains("code.test-gated-edit"), "{error}");
    }

    /// A run that configures nothing installs no `CompositionPolicy` global —
    /// `read_composition_policy` sees no resource at all, byte for byte as it
    /// did before this variable existed.
    #[test]
    fn an_unset_composition_policy_resolves_to_no_override() {
        assert_eq!(resolve_composition_policy(None), Ok(None));
        assert_eq!(resolve_composition_policy(Some("")), Ok(None));
        assert_eq!(resolve_composition_policy(Some("   ")), Ok(None));
    }

    #[test]
    fn a_known_composition_policy_is_accepted() {
        assert_eq!(
            resolve_composition_policy(Some("deterministic")),
            Ok(Some(CompositionPolicy::Deterministic))
        );
        assert_eq!(
            resolve_composition_policy(Some(" model ")),
            Ok(Some(CompositionPolicy::Model))
        );
    }

    /// An unrecognised value is refused, naming the accepted values — never
    /// silently defaulted to the deterministic policy, which would make a
    /// benchmark sweep measure the same unintended policy under two names.
    #[test]
    fn an_unknown_composition_policy_is_refused_and_names_what_is_accepted() {
        let error = resolve_composition_policy(Some("Model"))
            .expect_err("an unknown policy must be refused, not defaulted");
        assert!(error.contains(COMPOSITION_POLICY_VAR), "{error}");
        assert!(error.contains("Model"), "{error}");
        assert!(error.contains("deterministic"), "{error}");
        assert!(error.contains("model"), "{error}");
    }

    /// A run that configures nothing installs no `GatePlannerMode` global —
    /// `read_gate_planner_mode` sees no resource at all, and the scripted
    /// planner runs exactly as it did before this variable existed.
    #[test]
    fn an_unset_gate_planner_mode_resolves_to_no_override() {
        assert_eq!(resolve_gate_planner_mode(None), Ok(None));
        assert_eq!(resolve_gate_planner_mode(Some("")), Ok(None));
        assert_eq!(resolve_gate_planner_mode(Some("   ")), Ok(None));
    }

    #[test]
    fn a_known_gate_planner_mode_is_accepted() {
        assert_eq!(
            resolve_gate_planner_mode(Some("scripted")),
            Ok(Some(GatePlannerMode::Scripted))
        );
        assert_eq!(
            resolve_gate_planner_mode(Some(" model ")),
            Ok(Some(GatePlannerMode::Model))
        );
    }

    /// An unrecognised value is refused, naming the accepted values — never
    /// silently defaulted to the scripted planner, which would make a
    /// benchmark sweep measure the same unintended planner under two names.
    #[test]
    fn an_unknown_gate_planner_mode_is_refused_and_names_what_is_accepted() {
        let error = resolve_gate_planner_mode(Some("Model"))
            .expect_err("an unknown mode must be refused, not defaulted");
        assert!(error.contains(GATE_PLANNER_VAR), "{error}");
        assert!(error.contains("Model"), "{error}");
        assert!(error.contains("scripted"), "{error}");
        assert!(error.contains("model"), "{error}");
    }

    /// A run that configures neither variable installs no `DomainSelection`
    /// global — `read_domain_selection` sees no resource at all, and every
    /// turn runs the unmodified generic plan exactly as it did before domain
    /// plans existed.
    #[test]
    fn an_unset_domain_selection_resolves_to_no_override() {
        assert_eq!(resolve_domain_selection(None, None), Ok(None));
        assert_eq!(resolve_domain_selection(Some(""), Some("  ")), Ok(None));
    }

    #[test]
    fn a_known_domain_and_arm_are_accepted() {
        assert_eq!(
            resolve_domain_selection(Some("django"), Some("expert")),
            Ok(Some(DomainSelection {
                domain: polaris_code::cosmos::domains::Domain::Django,
                arm: polaris_code::cosmos::domains::Arm::Expert,
            }))
        );
    }

    #[test]
    fn lean_selects_scripted_gates_without_extra_environment() {
        let lean = resolve_domain_selection(None, Some("lean")).unwrap();
        assert_eq!(
            lean_composition_policy(lean, None, None),
            Ok(Some(CompositionPolicy::Gates))
        );
        assert_eq!(
            lean_composition_policy(lean, Some(CompositionPolicy::Deterministic), None),
            Ok(Some(CompositionPolicy::Gates))
        );
        assert!(lean_composition_policy(lean, Some(CompositionPolicy::Model), None).is_err());
        assert!(lean_composition_policy(lean, None, Some(GatePlannerMode::Model)).is_err());
        assert_eq!(lean_composition_policy(None, None, None), Ok(None));
    }

    /// Setting only the arm still validates the pair against the domain's
    /// own default (`none`), which refuses exactly as an unknown value would.
    #[test]
    fn an_arm_without_a_domain_is_refused_and_names_the_problem() {
        let error = resolve_domain_selection(None, Some("expert"))
            .expect_err("expert needs a domain other than none");
        assert!(error.contains(DOMAIN_VAR), "{error}");
        assert!(error.contains(ARM_VAR), "{error}");
        assert!(error.contains("expert"), "{error}");
    }

    #[test]
    fn an_unknown_domain_is_refused_and_names_what_is_accepted() {
        let error = resolve_domain_selection(Some("djangoo"), None)
            .expect_err("an unknown domain must be refused, not defaulted");
        assert!(error.contains("djangoo"), "{error}");
        assert!(error.contains("django"), "{error}");
    }

    #[test]
    fn lean_is_accepted_from_cli_and_env_resolution() {
        let args = cli(&["--arm", "lean"]);
        assert_eq!(args.arm.as_deref(), Some("lean"));
        assert_eq!(
            resolve_domain_selection(None, args.arm.as_deref())
                .unwrap()
                .unwrap()
                .arm,
            polaris_code::cosmos::domains::Arm::Lean
        );
        assert_eq!(
            resolve_domain_selection(None, Some("lean"))
                .unwrap()
                .unwrap()
                .arm,
            polaris_code::cosmos::domains::Arm::Lean
        );
        assert!(resolve_domain_selection(Some("django"), Some("lean")).is_err());
    }

    /// The `loop` topology is a single model conversation and is the mirror image of `expert`/
    /// `experience`/`generic-notes`: it needs *no* domain, so it is accepted
    /// with `none` (including the unset default) and refused with a real one.
    #[test]
    fn the_loop_arm_requires_domain_none() {
        assert_eq!(
            resolve_domain_selection(Some("none"), Some("loop")),
            Ok(Some(DomainSelection {
                domain: polaris_code::cosmos::domains::Domain::None,
                arm: polaris_code::cosmos::domains::Arm::Loop,
            }))
        );
        assert_eq!(
            resolve_domain_selection(None, Some("loop")),
            Ok(Some(DomainSelection {
                domain: polaris_code::cosmos::domains::Domain::None,
                arm: polaris_code::cosmos::domains::Arm::Loop,
            }))
        );
        let error = resolve_domain_selection(Some("django"), Some("loop"))
            .expect_err("loop must be refused alongside a real domain");
        assert!(error.contains("loop"), "{error}");
    }

    #[test]
    fn the_fine_arm_requires_domain_none() {
        assert_eq!(
            resolve_domain_selection(Some("none"), Some("fine")),
            Ok(Some(DomainSelection {
                domain: polaris_code::cosmos::domains::Domain::None,
                arm: polaris_code::cosmos::domains::Arm::Fine,
            }))
        );
        let error = resolve_domain_selection(Some("django"), Some("fine"))
            .expect_err("fine must be refused alongside a real domain");
        assert!(error.contains("fine"), "{error}");
    }

    #[test]
    fn attended_shell_policy_keeps_the_deny_list_and_directory_sandbox() {
        let working_dir = std::env::current_dir().expect("current dir");
        for mode in [
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
            PermissionMode::Plan,
        ] {
            let executor = ShellExecutor::new(shell_config(&working_dir, &mode));
            assert_eq!(executor.check_permission("echo ok"), ShellPermission::Allow);
            assert_eq!(
                executor.check_permission("rm -rf build"),
                ShellPermission::Deny
            );
            assert!(executor.config().sandbox);
            assert_eq!(executor.config().allowed_dirs, vec![working_dir.clone()]);
        }
    }

    #[test]
    fn unattended_shell_policy_allows_only_declared_patterns() {
        let working_dir = std::env::current_dir().expect("current dir");
        let mode = PermissionMode::Unattended(UnattendedGrants {
            edit_file: false,
            run_command_patterns: vec!["cargo *".into()],
        });
        let executor = ShellExecutor::new(shell_config(&working_dir, &mode));
        assert_eq!(
            executor.check_permission("cargo test --workspace"),
            ShellPermission::Allow
        );
        assert_eq!(
            executor.check_permission("echo ok"),
            ShellPermission::Confirm
        );
        assert_eq!(
            executor.check_permission("rm -rf build"),
            ShellPermission::Deny
        );
        assert_eq!(executor.config().allowed_dirs, vec![working_dir]);
    }

    #[test]
    fn project_environment_keeps_runtime_names_only_when_enabled() {
        let dir = std::env::current_dir().unwrap();
        let mode = PermissionMode::Unattended(UnattendedGrants {
            edit_file: false,
            run_command_patterns: vec!["python *".into()],
        });
        let off = project_shell_environment(shell_config(&dir, &mode), false);
        assert!(off.env_allowlist.is_empty());
        let on = project_shell_environment(shell_config(&dir, &mode), true);
        assert!(!on.inherit_env);
        assert!(on.env_allowlist.contains(&"PATH".to_string()));
        assert!(on.env_allowlist.contains(&"CONDA_PREFIX".to_string()));
        for secret in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!on.env_allowlist.contains(&secret.to_string()));
        }
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

    #[test]
    fn help_is_reachable_and_lists_every_flag_and_env_knob() {
        // Clap owns `-h`/`--help`; `HELP` is the tail it cannot derive. This
        // asserts the two halves are actually joined, so a flag added without
        // a doc comment or a knob added without a HELP line is caught here.
        use clap::CommandFactory;
        let rendered = Cli::command().render_long_help().to_string();
        for flag in ["--model", "--print", "--output-format", "--max-cost-usd"] {
            assert!(rendered.contains(flag), "--help omits {flag}");
        }
        for knob in [
            "ANTHROPIC_API_KEY",
            "POLARIS_CODE_MODEL",
            "POLARIS_CODE_MODELS",
            "POLARIS_CODE_GRAPH_SEARCH",
            "POLARIS_CODE_PROMPT_CACHE",
            "POLARIS_CODE_REDACT",
            "POLARIS_CODE_ONTOLOGY_DIR",
        ] {
            assert!(rendered.contains(knob), "--help omits {knob}");
        }
    }

    #[test]
    fn api_key_missing_treats_unset_and_blank_as_missing() {
        use std::env::VarError;
        assert!(api_key_missing(Err(VarError::NotPresent)));
        assert!(api_key_missing(Ok(String::new())));
        assert!(api_key_missing(Ok("   ".into())));
        assert!(!api_key_missing(Ok("sk-ant-123".into())));
    }

    #[test]
    fn env_flag_enabled_defaults_on_and_honors_disable_words() {
        use std::env::VarError;
        // Unset → on.
        assert!(env_flag_enabled(Err(VarError::NotPresent)));
        // Disable words, case- and whitespace-insensitive.
        for off in ["0", "false", "off", "OFF", " Off "] {
            assert!(!env_flag_enabled(Ok(off.into())), "{off:?} should disable");
        }
        // Anything else → on.
        for on in ["1", "true", "on", "yes", "anything"] {
            assert!(env_flag_enabled(Ok(on.into())), "{on:?} should enable");
        }
    }

    #[test]
    fn the_eval_episode_argv_shape_parses_into_an_explicit_fresh_session() {
        // Mirrors `evals/core/episode.py::agent_argv`: working_dir, -p, text,
        // --output-format, json, --session, id. `cli()` already prepends the
        // binary name the way an operator's shell would.
        let parsed = cli(&[
            ".",
            "-p",
            "fix the failing test",
            "--output-format",
            "json",
            "--session",
            "eval-run-42-mode-0-instance-7",
        ]);
        assert_eq!(parsed.print.as_deref(), Some("fix the failing test"));
        assert_eq!(
            parsed.session.as_deref(),
            Some("eval-run-42-mode-0-instance-7")
        );
        assert_eq!(parsed.output_format, OutputFormat::Json);
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
