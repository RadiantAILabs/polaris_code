//! The command line `polaris` accepts: the flags, the environment variables
//! that stand in for some of them, and the checks on the values they carry.
//!
//! Resolving a value against configuration files happens in
//! [`settings_file`](super::settings_file), which feeds the files' defaults to
//! this parser. What the values mean for a run is decided by the rest of
//! [`startup`](super).

use std::num::NonZeroU32;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use polaris::models::anthropic::DEFAULT_MAX_TOKENS;

use crate::accounts::Account;
use crate::agents::{DEFAULT_MAX_TURNS, PermissionMode};
use crate::exec::OutputFormat;
use crate::providers::{self, Provider};
use crate::terminal::notify::NotifyMode;

/// The tail of `--help`: every environment knob and every key binding in one
/// place, so the configuration surface is discoverable without reading the
/// source. Clap renders the flags above it; these two sections are the part it
/// cannot derive from the `Cli` struct.
const HELP: &str = "\
ENVIRONMENT:
    POLARIS_CONFIG_DIR         Directory for user configuration, credentials,
                               permission rules, and MCP definitions. Fallback:
                               $XDG_CONFIG_HOME/polaris, then ~/.config/polaris.
    ANTHROPIC_API_KEY          Required for --provider anthropic (the
                               default). Anthropic API key. Not read for
                               --provider bedrock, which uses the AWS SDK's
                               own default credential chain instead (an
                               EC2/ECS instance role, AWS_PROFILE, or
                               AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY).
    OPENAI_API_KEY             Required for --provider openai. Any non-empty
                               value works for a server that checks no key.
    OPENAI_BASE_URL            Address of an OpenAI-compatible server (Ollama,
                               LM Studio, vLLM, a gateway) for --provider
                               openai. Default: api.openai.com.
    POLARIS_CODE_MODEL         Model id to start on. Default: the provider's
                               newest Sonnet-class or flagship model, shown
                               by --list-models. `--model` wins; a bare name
                               such as `sonnet` picks the newest match.
    POLARIS_CODE_MODELS        Comma-separated set of models this run can
                               reach and `/model` can switch between.
                               Declare a local server's models here as
                               openai/<id>.
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
    POLARIS_CODE_MAX_COST_USD  Per coding task estimated USD ceiling. Lean
                               defaults to 1.50; 0 disables it. Other arms
                               default off.
    POLARIS_CODE_PROMPT_CACHE  0/false/off disables cache-aware context and
                               provider prompt caching. Default: on.
    POLARIS_CODE_REDACT       0/false/off turns off secret redaction of tool
                               output. Default: on.
    POLARIS_CODE_NOTIFY       When the terminal bell and window title ask for
                               attention: off, approval (a pending approval,
                               default), or all (also when a turn ends).
    POLARIS_CODE_ONTOLOGY_DIR  Ontology-pack directory seeded into the TBox at
                               boot. Default: unset (seeding is skipped).

    POLARIS_CODE_PROVIDER      Provider to start on; `--provider` wins.
    POLARIS_CODE_REGION        AWS region; `--region` wins. The AWS SDK's
                               own region chain is the fallback.
    POLARIS_CODE_PERMISSION_MODE
                               Interactive posture; `--permission-mode` wins.
    POLARIS_CODE_PERMISSION_CEILING
                               Interactive ceiling; `--permission-ceiling` wins.
    POLARIS_CODE_MAX_TOKENS    Whole-run billed-token ceiling; `--max-tokens` wins.
    POLARIS_CODE_MAX_OUTPUT_TOKENS
                               Positive per-request output-token limit;
                               `--max-output-tokens` wins.
    POLARIS_CODE_MAX_TURNS     Agent iteration limit per turn; 0 disables it;
                               `--max-turns` wins. Default: 100, except the
                               explicit unrestricted container mode.
    POLARIS_CODE_CONTEXT_WINDOW
                               Explicit model input-plus-output capacity.
    POLARIS_CODE_REASONING_EFFORT
                               Effort pinned on every model call; unset keeps
                               provider/model defaults.

EXPERIMENT SWITCHES (not configuration-file keys):
    POLARIS_CODE_EXP_SUBMIT_GUARD  Refuse submit_answer while nothing is edited or
                               the last run_python check failed (0/1, default 0).
    POLARIS_CODE_EXP_REPO_LAYOUT  Put a short workspace listing in the first
                               message of a model step (0/1, default 0).
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

CONFIGURATION FILES:
    User: <POLARIS_CONFIG_DIR>/config.toml (or the fallback directory above).
    Project: <working directory>/polaris.toml. Select an overlay with --profile.
    Flags > environment > project file > user file > defaults.
    POLARIS_CODE_MAX_COST_USD is a separate per-task limit; the file key
    max_cost_usd sets the whole-run --max-cost-usd limit.

Only .env in the launch folder is loaded automatically; parent folders and
an explicit working directory are not searched. A loaded file is reported
on stderr. Existing environment values take precedence.

CREDENTIALS:
    A provider's key is read from its environment variable (a .env file in
    the launch folder counts), and otherwise from the key `polaris login`
    stored in credentials.toml in the user configuration directory. Select that
    directory with POLARIS_CONFIG_DIR. `polaris login status` shows which
    providers have credentials and where each comes from.

KEYS (in the TUI):
    Tab               cycle panels (Conversation, Graph, Help)
    Shift-Tab         step to the next permission mode (see /permission)
    Alt-1 … Alt-9     jump to panel N
    Esc               interrupt a running turn (a popup, search or pending
                      approval takes Esc first); never quits
    Ctrl-G            edit the draft in $VISUAL / $EDITOR
    Ctrl-N            browse and search the transcript
    Ctrl-O            expand the per-turn node timeline (Conversation)
    Ctrl-L            toggle single / tiled layout
    Ctrl-C / Ctrl-Q   quit (Ctrl-C first returns a scrolled-up transcript to
                      the live bottom, then clears the draft or interrupts a
                      running turn)

KEYS (Graph panel):
    Up / k, Down / j  scroll one row
    PageUp / PageDown scroll one page
    Home / g, End / G jump to the top or bottom
    f                 follow the running node again (scrolling stops
                      following)
    Mouse wheel       scroll three rows
";

/// The environment variable that sets the per-request output token limit the
/// Anthropic provider is registered with.
const MAX_OUTPUT_TOKENS_VAR: &str = "POLARIS_CODE_MAX_OUTPUT_TOKENS";

/// The output token limit a run uses when [`MAX_OUTPUT_TOKENS_VAR`] is unset:
/// four times the provider's [`DEFAULT_MAX_TOKENS`], so a long edit or
/// answer is not cut off at the provider's conservative default.
pub(super) const DEFAULT_MAX_OUTPUT_TOKENS: NonZeroU32 =
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
pub(super) fn resolve_max_output_tokens(raw: Option<&str>) -> Result<NonZeroU32, String> {
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
/// to be a non-negative integer (zero disables the cap); a typo is refused as a
/// configuration error rather than silently falling back to the default.
pub(super) fn resolve_max_turns(raw: Option<&str>) -> Result<usize, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_MAX_TURNS);
    };
    value
        .parse::<usize>()
        .map(|value| if value == 0 { usize::MAX } else { value })
        .map_err(|_| {
            format!(
                "{MAX_TURNS_VAR} must be a non-negative integer (0 disables the cap), got {value:?}"
            )
        })
}

/// A terminal coding agent built on Polaris. Mirrors Claude Code's CLI shape:
/// `-p`/`--print` for one non-interactive turn, and `login`/`logout` to manage
/// provider credentials. A working directory named `login` or `logout` is
/// reached as `./login`.
#[derive(Parser)]
#[command(name = "polaris", version, args_conflicts_with_subcommands = true)]
// `--print` and `--topology-task` are the two non-interactive entry points and
// they are exclusive: one drives a conversational turn, the other a topology
// turn, and a single run is one turn. The group also gives `--allow-edit` and
// `--allow-command` one name to require, since both grants apply to either.
#[command(group = clap::ArgGroup::new("unattended").args(["print", "topology_task"]))]
// Clap lists the flags; `HELP` adds the two sections it cannot derive — every
// environment knob and the TUI keys — so one `--help` shows the whole surface.
#[command(after_help = HELP)]
pub(super) struct Cli {
    /// Credential commands. `main` runs them before it loads any configuration
    /// (see `accounts::is_account_command`); the field is what lists them in
    /// `--help` and keeps `polaris login` from being read as a directory.
    #[command(subcommand)]
    pub(super) account: Option<Account>,
    /// Working directory (defaults to the current directory).
    pub(super) working_dir: Option<PathBuf>,
    /// Run one turn non-interactively with this prompt, then exit, instead of
    /// starting the TUI.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub(super) print: Option<String>,
    /// Run one topology turn non-interactively over this coding-work spec,
    /// then exit. The spec is the same `key=value` text `/topology` takes:
    /// `file=<path> find=<text> replace=<text> [file2=… find2=… replace2=…
    /// ...] [scope=<path>[,<path>...]] [command=<program>
    /// [command-args=<arg>[,<arg>...]]] [stub=…] [intent=…] [verifier=…]
    /// [ref=…] [budget-ms=…]`. Needs `--allow-edit`, because a topology
    /// edits files and cannot stop to ask.
    #[arg(long, value_name = "SPEC")]
    pub(super) topology_task: Option<String>,
    /// Resume this session id. A non-interactive run generates a fresh id
    /// when absent, rather than the interactive default `"default"` — two
    /// concurrent `--print` runs must not contend on one session.
    #[arg(long, value_name = "ID")]
    pub(super) session: Option<String>,
    /// Model to run on, as `provider/model` or an unambiguous model name.
    /// Wins over `POLARIS_CODE_MODEL`. Must name one of the models this run
    /// can reach (`POLARIS_CODE_MODELS` declares the set); an unknown name is
    /// refused at startup, naming what was available. `openai-chat` also accepts
    /// arbitrary remote ids, with or without its provider prefix.
    #[arg(long, value_name = "MODEL")]
    pub(super) model: Option<String>,
    /// Model provider to run on. `anthropic` (the default) needs
    /// `ANTHROPIC_API_KEY`. `openai` needs `OPENAI_API_KEY` and also reaches
    /// any server that speaks the same API (Ollama, LM Studio, vLLM, a
    /// gateway) when `OPENAI_BASE_URL` names it; declare that server's models
    /// with `POLARIS_CODE_MODELS`. `openai-chat` uses Chat Completions at the
    /// same key/base-URL settings and accepts arbitrary model ids. `bedrock` uses the AWS Bedrock Converse API
    /// through the default AWS credential chain (an EC2/ECS instance role,
    /// `AWS_PROFILE`, or `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`) and
    /// needs a region. A run checks its provider's credentials before
    /// starting and says what is missing. `--list-models` shows what each
    /// provider offers.
    #[arg(
        long,
        env = "POLARIS_CODE_PROVIDER",
        value_enum,
        default_value = "anthropic",
        value_name = "PROVIDER"
    )]
    pub(super) provider: Provider,
    /// AWS region for `--provider bedrock`. Unset leaves region resolution to
    /// the AWS SDK's own default chain (`AWS_REGION`, `AWS_DEFAULT_REGION`, or
    /// the local AWS config profile); a run with no region from either place
    /// stops at startup. Ignored for the other providers.
    #[arg(long, env = "POLARIS_CODE_REGION", value_name = "REGION")]
    pub(super) region: Option<String>,
    /// Plan arm, overriding `POLARIS_CODE_ARM` (including loop and lean).
    #[arg(long, value_name = "ARM")]
    pub(super) arm: Option<String>,
    /// Output format for `--print` — `text`, `json` or `stream-json`;
    /// ignored in interactive mode.
    #[arg(long, default_value = "text", value_name = "FORMAT")]
    pub(super) output_format: OutputFormat,
    /// Interactive permission posture. Non-interactive runs always use an
    /// immutable unattended grant instead.
    #[arg(long, value_enum, value_name = "MODE")]
    pub(super) permission_mode: Option<InteractivePermissionMode>,
    /// Reviewer model in provider/model form; defaults to the session model.
    #[arg(long, env = "POLARIS_CODE_REVIEW_MODEL", value_name = "MODEL")]
    pub(super) review_model: Option<String>,
    /// Highest permission posture this process may ever reach. Nothing inside
    /// the session — a command, a setting, a prompt — can exceed it.
    /// Interactive-only, like `--permission-mode`.
    #[arg(long, value_enum, value_name = "MODE")]
    pub(super) permission_ceiling: Option<InteractivePermissionMode>,
    /// When the terminal bell and window title ask for attention: `off`,
    /// `approval` (an approval is waiting), or `all` (also when a turn ends).
    /// Ignored by non-interactive runs.
    #[arg(
        long,
        env = "POLARIS_CODE_NOTIFY",
        value_enum,
        default_value = "approval",
        value_name = "MODE"
    )]
    pub(super) notify: NotifyMode,
    /// Allow `edit_file` during this unattended run.
    #[arg(long, requires = "unattended")]
    pub(super) allow_edit: bool,
    /// Allow file edits and arbitrary shell commands without prompts. Only for
    /// disposable containers: shell commands can access the process filesystem.
    /// Disables shell command denials, syntax checks and file-tool path containment.
    #[arg(long, requires = "print", conflicts_with_all = ["topology_task", "allow_edit", "allow_command"])]
    pub(super) dangerously_skip_permissions: bool,
    /// Allow shell commands matching this existing shell-policy glob during
    /// this unattended run. May be repeated.
    #[arg(long, requires = "unattended", value_name = "GLOB")]
    pub(super) allow_command: Vec<String>,
    /// Grant an additional tool by exact name or trailing-star prefix during
    /// this unattended run. MCP tools require exact names. May be repeated.
    #[arg(long, requires = "unattended", value_name = "NAME|PATTERN")]
    pub(super) allow_tool: Vec<String>,
    /// List the topologies this build registered, and whether each can run
    /// here, then exit. Honours `--output-format`; starts no agent turn.
    #[arg(long, conflicts_with = "print")]
    pub(super) list_topologies: bool,
    /// List the models each provider offers, which have a price, and whether
    /// its credentials are present, then exit. Honours `--output-format`;
    /// needs no key and starts no agent turn.
    #[arg(long, conflicts_with_all = ["print", "list_topologies"])]
    pub(super) list_models: bool,
    /// Stop the run once its estimated spend reaches this many US dollars.
    /// Refused at startup if the model in use has no price, because an
    /// unpriced model must not become an unlimited one.
    #[arg(long, value_name = "USD")]
    pub(super) max_cost_usd: Option<f64>,
    /// Stop the run once it has billed this many tokens, counting every tier.
    /// Enforceable for any model, priced or not.
    #[arg(long, value_name = "TOKENS")]
    pub(super) max_tokens: Option<u64>,
    /// Maximum agent iterations per turn; zero disables the cap. Defaults to 100,
    /// except --dangerously-skip-permissions, which has no practical turn cap.
    #[arg(
        long,
        env = "POLARIS_CODE_MAX_TURNS",
        value_name = "TURNS",
        value_parser = |raw: &str| resolve_max_turns(Some(raw))
    )]
    pub(super) max_turns: Option<usize>,
    /// Maximum output tokens per model request; must be positive.
    #[arg(
        long,
        env = "POLARIS_CODE_MAX_OUTPUT_TOKENS",
        value_name = "TOKENS",
        value_parser = |raw: &str| resolve_max_output_tokens(Some(raw))
    )]
    pub(super) max_output_tokens: Option<NonZeroU32>,
    /// Model context window, including input and output tokens. Reserves output
    /// and prompt overhead before sizing context projection and compaction.
    #[arg(long, env = "POLARIS_CODE_CONTEXT_WINDOW", value_name = "TOKENS")]
    pub(super) context_window: Option<NonZeroU32>,
    /// Pin reasoning effort on every model call, including summaries and
    /// reviewers. Known levels: none, minimal, low, medium, high, xhigh, max.
    /// Custom non-empty values pass through; unsupported values fail visibly.
    #[arg(long, env = "POLARIS_CODE_REASONING_EFFORT", value_name = "EFFORT")]
    pub(super) reasoning_effort: Option<polaris::models::llm::ReasoningEffort>,
    /// Longest one Bedrock transport attempt may take, in whole seconds (1-86400).
    /// Covers Converse and Responses; unset preserves provider defaults.
    #[arg(
        long,
        env = "POLARIS_CODE_MODEL_REQUEST_TIMEOUT_SECS",
        value_name = "SECONDS"
    )]
    pub(super) model_request_timeout_secs: Option<providers::ModelRequestTimeout>,
    /// Write native ATIF JSON to a fresh file during this unattended run.
    /// Parent directories must exist. Contains full prompts and model outputs.
    #[arg(
        long,
        requires = "print",
        conflicts_with = "topology_task",
        value_name = "PATH"
    )]
    pub(super) trajectory_path: Option<PathBuf>,
}

/// Permission modes a human may choose when starting an interactive session.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum InteractivePermissionMode {
    Confirm,
    AutoEdit,
    Auto,
    Plan,
}

impl From<InteractivePermissionMode> for PermissionMode {
    fn from(mode: InteractivePermissionMode) -> Self {
        match mode {
            InteractivePermissionMode::Confirm => Self::Confirm,
            InteractivePermissionMode::AutoEdit => Self::AutoEdit,
            InteractivePermissionMode::Auto => Self::Auto,
            InteractivePermissionMode::Plan => Self::Plan,
        }
    }
}

// The executor requires a finite usize bound even for a predicate-controlled
// loop. The largest representable value leaves unrestricted container runs
// governed by task completion, cancellation and explicit spend/time limits.
pub(super) fn run_max_turns(cli: &Cli) -> usize {
    cli.max_turns
        .unwrap_or(if cli.dangerously_skip_permissions {
            usize::MAX
        } else {
            DEFAULT_MAX_TURNS
        })
}

pub(super) fn validate_context_window(
    window: Option<NonZeroU32>,
    max_output_tokens: NonZeroU32,
) -> Result<(), &'static str> {
    if let Some(window) = window
        && window.get().saturating_sub(max_output_tokens.get()) < 2
    {
        return Err("--context-window must exceed --max-output-tokens by at least two tokens");
    }
    Ok(())
}

/// Parse a command line exactly as the operator would type it, so tests
/// exercise the flags rather than a hand-built `Cli`.
#[cfg(test)]
pub(super) fn parse(args: &[&str]) -> Cli {
    let mut argv = vec!["polaris"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).expect("the command line parses")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts;

    fn cli(args: &[&str]) -> Cli {
        parse(args)
    }

    #[test]
    fn notify_defaults_to_approvals_and_takes_a_word() {
        assert_eq!(cli(&[]).notify, NotifyMode::Approval);
        for (word, mode) in [
            ("off", NotifyMode::Off),
            ("approval", NotifyMode::Approval),
            ("all", NotifyMode::All),
        ] {
            assert_eq!(cli(&["--notify", word]).notify, mode, "{word}");
        }
    }

    #[test]
    fn notify_reads_its_variable_and_refuses_an_unknown_word() {
        use clap::CommandFactory;
        let command = Cli::command();
        let notify = command
            .get_arguments()
            .find(|arg| arg.get_id() == "notify")
            .expect("--notify exists");
        assert_eq!(
            notify.get_env().and_then(|name| name.to_str()),
            Some("POLARIS_CODE_NOTIFY")
        );
        let Err(refusal) = Cli::try_parse_from(["polaris", "--notify", "sometimes"]) else {
            panic!("an unknown word must be refused");
        };
        let refusal = refusal.to_string();
        for word in ["off", "approval", "all"] {
            assert!(refusal.contains(word), "{refusal}");
        }
    }

    #[test]
    fn unattended_flags_reject_conflicting_grants_and_topology_tasks() {
        for flags in [
            vec!["--dangerously-skip-permissions", "--topology-task", "x"],
            vec!["--dangerously-skip-permissions", "--allow-edit"],
            vec![
                "--dangerously-skip-permissions",
                "--allow-command",
                "cargo *",
            ],
            vec![
                "--trajectory-path",
                "/tmp/trace.json",
                "--topology-task",
                "x",
            ],
        ] {
            // Check this flag before clap reaches the print/topology group conflict.
            let args = [vec!["polaris"], flags.clone(), vec!["-p", "hello"]].concat();
            let error = Cli::try_parse_from(args).err().expect("flags conflict");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{flags:?}: {error}"
            );
            for flag in flags.iter().filter(|flag| flag.starts_with("--")) {
                assert!(error.to_string().contains(flag), "{flags:?}: {error}");
            }
        }
    }

    #[test]
    fn effort_accepts_known_and_custom_levels_but_refuses_empty_values() {
        for level in [
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ] {
            assert_eq!(
                cli(&["--reasoning-effort", level])
                    .reasoning_effort
                    .unwrap()
                    .to_string(),
                level
            );
        }
        assert_eq!(
            Cli::try_parse_from(["polaris", "--reasoning-effort", ""])
                .err()
                .expect("empty effort")
                .kind(),
            clap::error::ErrorKind::ValueValidation
        );
        assert_eq!(
            Cli::try_parse_from(["polaris", "--trajectory-path", "/tmp/trace.json"])
                .err()
                .expect("print is required")
                .kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn context_window_is_positive_and_explicit() {
        assert_eq!(
            Cli::try_parse_from(["polaris", "--context-window", "0"])
                .err()
                .expect("zero window")
                .kind(),
            clap::error::ErrorKind::ValueValidation
        );
        let parsed = cli(&["--context-window", "32768", "--max-output-tokens", "4096"]);
        assert_eq!(parsed.context_window.unwrap().get(), 32768);
        assert_eq!(parsed.max_output_tokens.unwrap().get(), 4096);
    }

    #[test]
    fn context_window_reserves_at_least_two_tokens_beyond_output() {
        let output = nz(4096);
        assert_eq!(validate_context_window(None, output), Ok(()));
        for window in [4095, 4096, 4097] {
            assert_eq!(
                validate_context_window(Some(nz(window)), output),
                Err("--context-window must exceed --max-output-tokens by at least two tokens"),
                "window {window}"
            );
        }
        assert_eq!(validate_context_window(Some(nz(4098)), output), Ok(()));
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
        assert_eq!(resolve_max_turns(Some("0")), Ok(usize::MAX));
        assert_eq!(
            run_max_turns(&cli(&["-p", "hi", "--max-turns", "0"])),
            usize::MAX
        );
    }

    /// A negative, a float and a typo are refused as a turn limit.
    #[test]
    fn an_invalid_turn_limit_is_refused() {
        for raw in ["-1", "1.5", "lots"] {
            let error = resolve_max_turns(Some(raw))
                .expect_err("an invalid turn limit must be refused, not defaulted");
            assert!(
                error.contains(MAX_TURNS_VAR) && error.contains("non-negative integer"),
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
            "POLARIS_CODE_NOTIFY",
            "POLARIS_CODE_ONTOLOGY_DIR",
        ] {
            assert!(rendered.contains(knob), "--help omits {knob}");
        }
    }

    #[test]
    fn every_provider_is_selectable_by_its_flag_spelling() {
        for provider in Provider::ALL {
            assert_eq!(cli(&["--provider", provider.flag()]).provider, provider);
        }
        assert_eq!(cli(&[]).provider, Provider::Anthropic);
        assert!(Cli::try_parse_from(["polaris", "--provider", "gemini"]).is_err());
    }

    #[test]
    fn a_region_may_be_given_by_flag() {
        assert_eq!(
            cli(&["--provider", "bedrock", "--region", "eu-west-1"])
                .region
                .as_deref(),
            Some("eu-west-1")
        );
    }

    /// `polaris login` is a command, `polaris ./login` is a directory, and a
    /// run's own options never combine with one.
    #[test]
    fn login_is_a_command_and_a_directory_named_login_is_reached_by_path() {
        assert!(matches!(
            cli(&["login"]).account,
            Some(Account::Login(accounts::Login {
                provider: None,
                with_api_key: false,
                action: None
            }))
        ));
        assert!(matches!(
            cli(&["login", "openai", "--with-api-key"]).account,
            Some(Account::Login(accounts::Login {
                provider: Some(Provider::OpenAi),
                with_api_key: true,
                ..
            }))
        ));
        assert!(matches!(
            cli(&["logout", "bedrock"]).account,
            Some(Account::Logout {
                provider: Some(Provider::Bedrock)
            })
        ));
        assert!(matches!(
            cli(&["login", "status"]).account,
            Some(Account::Login(accounts::Login {
                action: Some(accounts::LoginAction::Status),
                ..
            }))
        ));

        let directory = cli(&["./login"]);
        assert!(directory.account.is_none());
        assert_eq!(directory.working_dir, Some(PathBuf::from("./login")));
        let plain = cli(&["."]);
        assert!(plain.account.is_none());
        assert_eq!(plain.working_dir, Some(PathBuf::from(".")));

        // After an option, `login` is no longer in command position: it is the
        // working directory, and `main` says how to reach the command instead.
        let after_option = cli(&["--provider", "openai", "login"]);
        assert!(after_option.account.is_none());
        assert_eq!(after_option.working_dir, Some(PathBuf::from("login")));

        for conflicting in [vec!["login", "--model", "sonnet"], vec![".", "login"]] {
            let mut argv = vec!["polaris"];
            argv.extend(&conflicting);
            assert!(Cli::try_parse_from(argv).is_err(), "{conflicting:?}");
        }
    }

    #[test]
    fn help_lists_the_credential_commands() {
        use clap::CommandFactory;
        let rendered = Cli::command().render_long_help().to_string();
        for needle in ["login", "logout", "POLARIS_CONFIG_DIR", "credentials.toml"] {
            assert!(rendered.contains(needle), "--help omits {needle}");
        }
    }

    #[test]
    fn listing_models_is_a_listing_not_a_turn() {
        assert!(cli(&["--list-models"]).list_models);
        assert!(!cli(&[]).list_models);
        for conflicting in [
            ["--list-models", "--print"],
            ["--list-models", "--list-topologies"],
        ] {
            let mut argv = vec!["polaris"];
            argv.extend(conflicting);
            // `--print` takes a value, so give it one.
            if conflicting.contains(&"--print") {
                argv.push("hi");
            }
            assert!(Cli::try_parse_from(argv).is_err(), "{conflicting:?}");
        }
    }

    /// The keys block names the panels the default shell shows (see
    /// `tabs.rs`) and the Graph panel's keys, and does not name a Logs tab.
    #[test]
    fn help_names_the_default_panels_and_the_graph_keys() {
        use clap::CommandFactory;
        let rendered = Cli::command().render_long_help().to_string();
        assert!(
            rendered.contains("cycle panels (Conversation, Graph, Help)"),
            "--help does not list the default panels"
        );
        assert!(!rendered.contains("Logs"), "--help still names a Logs tab");
        for needle in [
            "KEYS (Graph panel)",
            "follow the running node",
            "live bottom",
        ] {
            assert!(rendered.contains(needle), "--help omits {needle}");
        }
    }

    #[test]
    fn help_tells_the_operator_how_to_reach_each_provider() {
        use clap::CommandFactory;
        let rendered = Cli::command().render_long_help().to_string();
        for needle in [
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "POLARIS_CODE_REGION",
            "--list-models",
            "openai",
            "bedrock",
        ] {
            assert!(rendered.contains(needle), "--help omits {needle}");
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
}
