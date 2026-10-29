//! Small, data-configured coding blocks.
//!
//! Besides wrapping the existing coding stages ([`super::existing`]), the
//! block catalogue includes small, configurable blocks so a plan can differ
//! per task in more than which stage runs — configuration here is data: a
//! role/instruction, a tool subset, a budget, an output kind.
//! [`ModelProposeBlock`] is that block. [`ApplyPatchIntentBlock`],
//! [`SurveyPathsBlock`], and [`ReduceAssessmentsBlock`] are the small blocks
//! alongside it: applying a proposed patch, a read-only survey over a path
//! set, and a merge/reduce step for `Fork` outputs.
//!
//! # Per-instance configuration
//!
//! [`ModelProposeBlock`] and [`SurveyPathsBlock`] decode their
//! [`ResolvedConfigurationRef`] inside [`BlockFactory::build`] and seed the
//! decoded value as a small closure-captured producer system ahead of their
//! own logic, which reads it back as an ordinary upstream `Out<T>`. Each
//! `build` call gets its own freshly decoded configuration and adds its own
//! seed node, so two instances of the same block — for example two
//! differently-instructed `code.model-propose` steps inside one `Fork` —
//! coexist in one graph without colliding, unlike an ambient `Res<T>`
//! resource shared by the whole run. See `tests` below for the executable
//! version of this claim.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use polaris::graph::{Graph, NodeId, RunCancellation};
use polaris::models::llm::{
    AssistantBlock, CacheControl, Llm, LlmRequest, Message, ToolChoice, ToolDefinition,
    ToolResultContent, UserBlock,
};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockDescription, BlockFactory, BlockWireError, CompositionCatalogue,
    CompositionCatalogueError, Effect, StablePortId, StablePortKey,
};
use polaris_cosmos::contracts::{
    Assessment, AssessmentConclusion, CompositionBlockId, CompositionBlockKey, ConfigurationEntry,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ResolvedConfiguration,
    ResolvedConfigurationRef, Status, StopReason,
};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use super::super::assess::{conclude_sites, site_assessment};
use super::super::block_cards;
use super::super::edit::{EditError, EditOutcome, apply_exact_edit, verify_exact_edit};
use super::super::payload::{AppliedEdit, CodingWorkspace, EditSite, TestRunOutcome};
use super::super::task::ExactEdit;
use super::behavior_version;
use super::twm;
use crate::agents::context::{cache_breakpoints, compact_tool_results, message_chars};
use crate::agents::policy::{DEFAULT_COMPACT_BATCH, DEFAULT_COMPACT_KEEP_RECENT};
use crate::tools::CodeToolsConfig;
use crate::tools::code::{MAX_READ_BYTES, raw_truncated, read_view, resume_footer};
use crate::tools::fileset::{Selection, WalkLimits, select_files, slashed};
use crate::usage::TokenMeter;

/// Tokens-per-char ratio [`LiveModelStep::run`] estimates its growing turn
/// history against when deciding whether to compact old tool results —
/// [`agents::context::ContextManager`](crate::agents::context::ContextManager)'s
/// own uncalibrated default (`token_ratio` before any provider sample has been
/// recorded). A model step never lives long enough across calls to calibrate
/// its own ratio the way a whole session does, so it uses the same starting
/// estimate a fresh session would.
const MODEL_STEP_TOKEN_RATIO: f64 = 0.3;
const COST_CAPPED_MAX_OUTPUT_TOKENS: u32 = 8192;

/// Estimated history size at which [`LiveModelStep::run`] starts stubbing old
/// tool results. Stubbing edits earlier messages, so the provider must
/// re-write the whole cached prefix at the cache-write price; a step capped
/// at a few dozen turns never reads enough later turns to earn that back.
/// So a step compacts only when its history nears the per-turn budget,
/// later than the main loop's `DEFAULT_COMPACT_AFTER_TOKENS`, which pays
/// off over a whole session.
const MODEL_STEP_COMPACT_AFTER_TOKENS: usize = 120_000;

/// Optional cache-aware trimming of old model-step tool output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TrimPolicy {
    /// Do not apply the batched trim policy. The legacy emergency compactor
    /// remains in place for existing plan configurations.
    #[default]
    None,
    /// Trim eligible results in batches when the estimated cache cost pays back.
    Batched,
}

impl TrimPolicy {
    /// Resolve the lean arm's per-run switch. The caller supplies its arm
    /// default; other arms never need to consult this environment variable.
    #[must_use]
    pub fn lean_env(default_enabled: bool) -> Self {
        match std::env::var("POLARIS_CODE_LEAN_TRIM").ok().as_deref() {
            Some("1") => Self::Batched,
            Some("0") => Self::None,
            _ if default_enabled => Self::Batched,
            _ => Self::None,
        }
    }
}

fn compact_prior_autotests(messages: &mut [Message]) {
    for message in messages {
        if let Message::User { content } = message {
            for block in content {
                if let UserBlock::ToolResult(result) = block
                    && let ToolResultContent::Text(body) = &mut result.content
                    && let Some(start) = body.find("\nAutotest (related subset):")
                {
                    body.truncate(start);
                    body.push_str("\n[earlier automatic test result summarized by system check]");
                }
            }
        }
    }
}

const TRIM_MIN_TOKENS: usize = 6_000;
const TRIM_MIN_TURNS_APART: u16 = 8;
const TRIM_KEEP_RESULTS: usize = 3;

fn future_cache_reads(max_turns: u16, turn: u16) -> u64 {
    // A trim on `turn` is written by that request. It can pay back only on
    // requests still available afterward, never on the current request.
    u64::from(max_turns.saturating_sub(turn.saturating_add(1)))
}

/// Mutate only old text tool results. Tool calls stay in the assistant's
/// history, giving the model the exact arguments needed to repeat a call.
/// Returns the number of results trimmed in this batch.
fn trim_old_tool_results(
    messages: &mut [Message],
    turn: u16,
    max_turns: u16,
    last_trim: &mut Option<u16>,
) -> usize {
    if turn < TRIM_MIN_TURNS_APART
        || last_trim.is_some_and(|last| turn.saturating_sub(last) < TRIM_MIN_TURNS_APART)
    {
        return 0;
    }
    let result_sites: Vec<_> = messages
        .iter()
        .enumerate()
        .flat_map(|(mi, message)| match message {
            Message::User { content } => content
                .iter()
                .enumerate()
                .filter_map(move |(bi, block)| {
                    matches!(block, UserBlock::ToolResult(_)).then_some((mi, bi))
                })
                .collect::<Vec<_>>(),
            Message::Assistant { .. } => Vec::new(),
        })
        .collect();
    let cutoff = result_sites.len().saturating_sub(TRIM_KEEP_RESULTS);
    let mut changes = Vec::new();
    for &(mi, bi) in &result_sites[..cutoff] {
        let Message::User { content } = &messages[mi] else {
            continue;
        };
        let UserBlock::ToolResult(result) = &content[bi] else {
            continue;
        };
        let ToolResultContent::Text(body) = &result.content else {
            continue;
        };
        if body.starts_with(crate::agents::context::COMPACTED_MARKER)
            || body.starts_with("[trimmed ")
        {
            continue;
        }
        let call = messages[..mi]
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant { content, .. } => {
                    content.iter().rev().find_map(|block| match block {
                        AssistantBlock::ToolCall(call) if call.id == result.id => Some(call),
                        _ => None,
                    })
                }
                Message::User { .. } => None,
            });
        let Some(call) = call else { continue };
        let autotested_edit = call.function.name == EDIT_FILE_TOOL && body.contains("\nAutotest");
        if !autotested_edit
            && !matches!(
                call.function.name.as_str(),
                "read_file" | "list_dir" | "search" | "run_python" | "run_command"
            )
        {
            continue;
        }
        let site: String = call
            .function
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("workspace")
            .chars()
            .take(80)
            .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
            .collect();
        let stub = if autotested_edit {
            format!(
                "[trimmed {} chars of edit and autotest feedback at {site}; read_file for current contents]",
                body.chars().count()
            )
        } else {
            format!(
                "[trimmed {} chars of {} output at {}; repeat the preceding {} call to retrieve it]",
                body.chars().count(),
                call.function.name,
                site,
                call.function.name
            )
        };
        if stub.len() < body.len() {
            changes.push((mi, bi, stub, body.len()));
        }
    }
    let removed_chars: usize = changes
        .iter()
        .map(|(_, _, stub, old)| old - stub.len())
        .sum();
    let saved_tokens = (removed_chars as f64 * MODEL_STEP_TOKEN_RATIO) as u64;
    if saved_tokens < TRIM_MIN_TOKENS as u64 {
        return 0;
    }
    let first = changes[0].0;
    let rewritten_chars = message_chars(&messages[first..]).saturating_sub(removed_chars);
    let rewritten_tokens = (rewritten_chars as f64 * MODEL_STEP_TOKEN_RATIO) as u64;
    // Cache creation is 12.5 reads. Compare the one rewritten suffix with
    // the reads saved over the turns still available to this model step.
    let remaining = future_cache_reads(max_turns, turn);
    if saved_tokens.saturating_mul(remaining).saturating_mul(2)
        <= rewritten_tokens.saturating_mul(25)
    {
        return 0;
    }
    let count = changes.len();
    for (mi, bi, stub, _) in changes {
        if let Message::User { content } = &mut messages[mi]
            && let UserBlock::ToolResult(result) = &mut content[bi]
        {
            result.content = ToolResultContent::Text(stub);
        }
    }
    *last_trim = Some(turn);
    count
}

fn exec(error: impl fmt::Display) -> SystemError {
    SystemError::ExecutionError(error.to_string())
}

fn describe(
    summary: &str,
    inputs: Vec<StablePortKey>,
    output: StablePortKey,
    example: &str,
) -> BlockDescription {
    BlockDescription::new(summary, inputs, output, example)
        .expect("block description is well-formed")
}

fn block_key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("block ID is a valid stable name"),
        behavior_version(),
    )
}

fn schema_key(id: &str) -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new(id).expect("schema ID is a valid stable name"),
        behavior_version(),
    )
}

/// Reads the inline configuration map a [`BlockBuildError`]-returning
/// decoder needs, refusing a content-addressed reference (this catalogue
/// never registers one) or a non-map value.
fn inline_map(config: &ResolvedConfigurationRef) -> Result<&[ConfigurationEntry], BlockBuildError> {
    match config.resolved() {
        ResolvedConfiguration::Inline(ConfigurationValue::Map(entries)) => Ok(entries),
        ResolvedConfiguration::Inline(_) => {
            Err(BlockBuildError::new("expected an inline configuration map"))
        }
        ResolvedConfiguration::ContentAddressed(_) => Err(BlockBuildError::new(
            "content-addressed configuration is not supported",
        )),
    }
}

fn field<'a>(
    entries: &'a [ConfigurationEntry],
    key: &str,
) -> Result<&'a ConfigurationValue, BlockBuildError> {
    entries
        .iter()
        .find(|entry| entry.key() == key)
        .map(ConfigurationEntry::value)
        .ok_or_else(|| BlockBuildError::new(format!("configuration is missing '{key}'")))
}

fn as_str(value: &ConfigurationValue) -> Result<&str, BlockBuildError> {
    match value {
        ConfigurationValue::String(text) => Ok(text),
        _ => Err(BlockBuildError::new("expected a string")),
    }
}

fn as_u64(value: &ConfigurationValue) -> Result<u64, BlockBuildError> {
    match value {
        ConfigurationValue::Unsigned(n) => Ok(*n),
        _ => Err(BlockBuildError::new("expected an unsigned integer")),
    }
}

fn as_string_list(value: &ConfigurationValue) -> Result<Vec<String>, BlockBuildError> {
    match value {
        ConfigurationValue::List(items) => items
            .iter()
            .map(as_str)
            .map(|r| r.map(str::to_owned))
            .collect(),
        ConfigurationValue::Null => Ok(Vec::new()),
        _ => Err(BlockBuildError::new("expected a list of strings")),
    }
}

// ---------------------------------------------------------------------------
// A read-only workspace view: what a capability declared read-only may see.
// ---------------------------------------------------------------------------

/// A read-only view of a coding workspace.
///
/// `code.model-propose` is registered [`Effect::ReadOnly`]; handing its
/// executor the full [`CodingWorkspace`] (whose `apply`/`apply_path` write
/// the tree) would let an implementation violate that declaration from the
/// inside. This type is the fix: it borrows a workspace and exposes only its
/// read-only surface.
pub struct WorkspaceView<'a>(&'a CodingWorkspace);

impl<'a> WorkspaceView<'a> {
    pub(super) fn new(workspace: &'a CodingWorkspace) -> Self {
        Self(workspace)
    }

    /// Reads any sandbox-relative file.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the sandbox or cannot be read.
    pub fn read(&self, path: &str) -> Result<String, EditError> {
        self.0.read(path)
    }

    /// Returns the sandbox configuration.
    #[must_use]
    pub fn config(&self) -> &CodeToolsConfig {
        self.0.config()
    }

    /// Returns the stated edits, which may be none.
    #[must_use]
    pub fn edits(&self) -> &[ExactEdit] {
        self.0.edits()
    }

    /// Returns the path a whole-workspace verdict is filed against.
    #[must_use]
    pub fn subject_path(&self) -> &str {
        self.0.subject_path()
    }
}

// ---------------------------------------------------------------------------
// The model step: a bounded, tool-restricted, budgeted turn producing one
// configured typed artifact.
// ---------------------------------------------------------------------------

/// Which tools a model step may reach.
///
/// Deliberately has no "every tool" case, unlike
/// [`PaletteMode`](crate::agents::PaletteMode)'s `All` default: a model
/// step's whole point is that its configuration states what it may touch, so
/// silently inheriting the full registry would defeat the reason this block
/// exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolSubset {
    /// No tools: a pure-reasoning step.
    #[default]
    None,
    /// Only the named tools.
    Only(Vec<String>),
}

/// A bound on how large *one turn's own context* may grow, in provider
/// tokens: that turn's billed input plus cache-read plus cache-creation
/// tokens, plus its output. Not a bound on the sum across every turn the step
/// takes — [`ModelStepConfig::max_turns`] is what bounds total work, the same
/// way a session's own [`RunCeiling`](crate::usage::RunCeiling) bounds total
/// spend rather than any one call.
///
/// Checked when each response comes back ([`LiveModelStep::run`]). Small
/// turns may continue as long as no single turn exceeds this bound.
/// `u32::MAX` is the nonbinding sentinel used by lean: it leaves the
/// provider's configured output limit in control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget(pub u32);

/// Which typed artifact a model step's configuration commits it to
/// producing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStepOutputKind {
    /// A plan: a sequence of intended steps in prose.
    Plan,
    /// Findings: a list of observations about the tree.
    Findings,
    /// A patch intent: proposed exact-string edits, not yet applied.
    PatchIntent,
    /// A review: a verdict plus notes about something else's output.
    Review,
    /// An answer to a stated question.
    Answer,
}

impl ModelStepOutputKind {
    const fn tag(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Findings => "findings",
            Self::PatchIntent => "patch-intent",
            Self::Review => "review",
            Self::Answer => "answer",
        }
    }

    fn parse(tag: &str) -> Result<Self, BlockBuildError> {
        match tag {
            "plan" => Ok(Self::Plan),
            "findings" => Ok(Self::Findings),
            "patch-intent" => Ok(Self::PatchIntent),
            "review" => Ok(Self::Review),
            "answer" => Ok(Self::Answer),
            other => Err(BlockBuildError::new(format!(
                "unknown model step output kind '{other}'"
            ))),
        }
    }
}

/// How many request/response turns one `propose` call may take when a
/// [`ModelStepConfig`] does not say otherwise.
///
/// Matches the fixed bound this block used before turn count became part of
/// the configuration, so a configuration built through [`ModelStepConfig::new`]
/// and never given [`ModelStepConfig::with_max_turns`] behaves exactly as it
/// always did.
const DEFAULT_MODEL_STEP_MAX_TURNS: u16 = 4;

/// Ceiling `decode` enforces on a configured turn count.
///
/// A model step is one bounded action inside a larger plan, not an
/// open-ended agent loop — this is generous enough for a real repository's
/// worth of `read_file`/`list_dir`/`search` calls ahead of a patch, while
/// still refusing a plan that asks for something unbounded.
pub(crate) const MAX_MODEL_STEP_TURNS_CEILING: u16 = 100;

/// Ceiling `decode` enforces on a configured token budget.
///
/// Unlike [`MAX_MODEL_STEP_TURNS_CEILING`] (refused only here, at build time —
/// see the `max_turns` field doc on
/// [`crate::cosmos::planner`]'s `ModelGateConfigAnswer`), a model-chosen
/// `token_budget` is refused a second time, earlier, by
/// `crate::cosmos::planner::build_step_answer`, which imports this same
/// constant rather than a second number that could drift from it: this value
/// becomes `LlmRequest.max_output_tokens` verbatim, so an unbounded one is a
/// provider bill, not just a wasted turn.
///
/// Set to the same figure the codebase's own most generous built-in
/// configuration already spends in one turn — a statement-only edit's
/// open-ended repository exploration ahead of a patch (see
/// `crate::cosmos::planner::statement_edit_model_step_config`) — rather than
/// a freshly chosen number: nothing this codebase ships needs more than that
/// in a single turn.
///
/// The one value above it `decode` accepts is the host-only `u32::MAX`
/// nonbinding sentinel [`TokenBudget`] documents, which never becomes an
/// uncapped `max_output_tokens`.
pub(crate) const MAX_MODEL_STEP_TOKEN_BUDGET: u32 = 200_000;

/// The configuration [`model_propose`] reads: a role/instruction, an allowed
/// tool subset, a token budget, a turn cap, and the typed artifact kind it
/// commits to producing.
#[derive(Debug, Clone)]
pub struct ModelStepConfig {
    /// Optional provider/model identifier for this step. `None` uses the
    /// session model and preserves older plan encodings.
    pub model_id: Option<String>,
    /// Short label for the role this step plays (for provenance/logging).
    pub role: String,
    /// The instruction given to the model for this one bounded turn.
    pub instruction: String,
    /// The tools this turn may use.
    pub tool_subset: ToolSubset,
    /// The token budget any single request/response turn of this step may
    /// spend — see [`TokenBudget`] for exactly what that bounds.
    pub budget: TokenBudget,
    /// Which artifact variant this configuration commits to producing.
    pub output: ModelStepOutputKind,
    /// How many request/response round trips this turn may take.
    pub max_turns: u16,
    /// Whether this step's live executor should fold a preceding
    /// [`TestRunOutcome`](super::super::payload::TestRunOutcome)'s captured
    /// failure into the instruction it sends — set by a repair-loop plan
    /// shape whose `model-propose` instance is wired after a test run, never
    /// by the first attempt (there is nothing prior to see yet).
    pub sees_prior_failure: bool,
    /// Whether this step's live executor should hold back its first
    /// accepted `submit_answer` call for one causal/coverage review round
    /// before letting a resubmission end the step — see
    /// [`LiveModelStep::run`]. Off by default: a plan opts a
    /// patch-producing propose step into this deliberately, never a judge
    /// step, via [`with_review_before_submit`](Self::with_review_before_submit).
    pub review_before_submit: bool,
    /// How this step consumes the task-scoped handoff from earlier model steps.
    pub context_mode: ContextMode,
    /// Whether the executor may compact old tool results; existing plans default on.
    pub compact_history: bool,
    /// Include invalid final-answer repairs inside the turn cap.
    pub strict_turn_cap: bool,
    /// Keep tool choice stable on the last request. Lean uses this to preserve
    /// the message cache across its continuation boundary.
    pub stable_tool_choice: bool,
    /// Optional batching of old tool-result output within this conversation.
    pub trim: TrimPolicy,
    /// Run one bounded related-test batch after successful writes; lean opts in.
    pub autotest: bool,
    /// Typed working memory: replaces this conversation's growing transcript
    /// with a small set of typed slots, rebuilt at a whole-prompt size limit.
    /// `None` (every existing plan) leaves the step exactly as before —
    /// see `twm`. Mutually exclusive with
    /// [`TrimPolicy`]/`compact_history`: when set, this step's own rebuild
    /// replaces both.
    pub twm: Option<twm::TwmOptions>,
}

/// How a model step reads earlier model work in the same gate-plan execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextMode {
    /// Start with only this step's instruction.
    #[default]
    Fresh,
    /// Start fresh with a bounded structured account of earlier steps.
    Brief,
    /// Resume the preceding model step's full conversation and tool results.
    Continue,
}

impl ContextMode {
    fn tag(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Brief => "brief",
            Self::Continue => "continue",
        }
    }

    fn parse(value: &str) -> Result<Self, BlockBuildError> {
        match value {
            "fresh" => Ok(Self::Fresh),
            "brief" => Ok(Self::Brief),
            "continue" => Ok(Self::Continue),
            _ => Err(BlockBuildError::new(format!(
                "unknown context mode '{value}'"
            ))),
        }
    }
}

impl ModelStepConfig {
    /// Creates a model step configuration with the default turn cap and no
    /// prior-failure visibility; override either with
    /// [`with_max_turns`](Self::with_max_turns) or
    /// [`with_prior_failure_visible`](Self::with_prior_failure_visible).
    #[must_use]
    pub const fn new(
        role: String,
        instruction: String,
        tool_subset: ToolSubset,
        budget: TokenBudget,
        output: ModelStepOutputKind,
    ) -> Self {
        Self {
            model_id: None,
            role,
            instruction,
            tool_subset,
            budget,
            output,
            max_turns: DEFAULT_MODEL_STEP_MAX_TURNS,
            sees_prior_failure: false,
            review_before_submit: false,
            context_mode: ContextMode::Fresh,
            compact_history: true,
            strict_turn_cap: false,
            stable_tool_choice: false,
            trim: TrimPolicy::None,
            autotest: false,
            twm: None,
        }
    }

    /// Overrides how many request/response round trips this turn may take.
    ///
    /// Bounded at [`decode`](Self::decode) time, not here: a caller can build
    /// an out-of-range value, but it is refused before a graph using it can
    /// materialise, the same way [`TokenBudget`]'s own zero case is.
    #[must_use]
    pub const fn with_max_turns(mut self, max_turns: u16) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Marks this step as reading a preceding test run's captured failure.
    ///
    /// Only meaningful for a [`ModelStepOutputKind::PatchIntent`] instance
    /// wired after a [`super::repair::RunProjectTestsCapturedBlock`] in the
    /// same plan; a materializer wiring this onto an instance with no such
    /// producer upstream refuses the plan before execution rather than
    /// silently ignoring the flag.
    #[must_use]
    pub const fn with_prior_failure_visible(mut self) -> Self {
        self.sees_prior_failure = true;
        self
    }

    /// Turns on the one-time pre-submit causal/coverage review described at
    /// [`ModelStepConfig::review_before_submit`].
    #[must_use]
    pub const fn with_review_before_submit(mut self) -> Self {
        self.review_before_submit = true;
        self
    }

    /// Selects this step's task-scoped context consumption policy.
    #[must_use]
    pub const fn with_context_mode(mut self, mode: ContextMode) -> Self {
        self.context_mode = mode;
        self
    }

    /// Selects a registered provider/model for this step alone.
    #[must_use]
    pub fn with_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Some(model_id.into());
        self
    }

    /// Enables a per-step old-tool-output trim policy.
    #[must_use]
    pub const fn with_trim(mut self, trim: TrimPolicy) -> Self {
        self.trim = trim;
        self
    }

    /// Encodes this configuration as a plan-facing resolved configuration
    /// value, matching what [`decode`](Self::decode) reads back.
    ///
    /// # Errors
    ///
    /// Returns an error only if the encoded value is rejected as malformed,
    /// which does not happen for a well-formed [`ModelStepConfig`].
    pub fn encode(
        &self,
    ) -> Result<ResolvedConfigurationRef, polaris_cosmos::contracts::ContractError> {
        let tool_subset = match &self.tool_subset {
            ToolSubset::None => ConfigurationValue::Null,
            ToolSubset::Only(tools) => ConfigurationValue::List(
                tools
                    .iter()
                    .cloned()
                    .map(ConfigurationValue::String)
                    .collect(),
            ),
        };
        let mut entries = vec![
            ConfigurationEntry::new("role", ConfigurationValue::String(self.role.clone()))?,
            ConfigurationEntry::new(
                "instruction",
                ConfigurationValue::String(self.instruction.clone()),
            )?,
            ConfigurationEntry::new("tool_subset", tool_subset)?,
            ConfigurationEntry::new(
                "budget",
                ConfigurationValue::Unsigned(u64::from(self.budget.0)),
            )?,
            ConfigurationEntry::new(
                "output",
                ConfigurationValue::String(self.output.tag().to_owned()),
            )?,
            ConfigurationEntry::new(
                "max_turns",
                ConfigurationValue::Unsigned(u64::from(self.max_turns)),
            )?,
            ConfigurationEntry::new(
                "sees_prior_failure",
                ConfigurationValue::Bool(self.sees_prior_failure),
            )?,
            ConfigurationEntry::new(
                "review_before_submit",
                ConfigurationValue::Bool(self.review_before_submit),
            )?,
        ];
        // Existing plans retain their exact encoded configuration and digest.
        if self.context_mode != ContextMode::Fresh {
            entries.push(ConfigurationEntry::new(
                "context_mode",
                ConfigurationValue::String(self.context_mode.tag().to_owned()),
            )?);
        }
        if !self.compact_history {
            entries.push(ConfigurationEntry::new(
                "compact_history",
                ConfigurationValue::Bool(false),
            )?);
        }
        if self.strict_turn_cap {
            entries.push(ConfigurationEntry::new(
                "strict_turn_cap",
                ConfigurationValue::Bool(true),
            )?);
        }
        if self.stable_tool_choice {
            entries.push(ConfigurationEntry::new(
                "stable_tool_choice",
                ConfigurationValue::Bool(true),
            )?);
        }
        if let Some(model_id) = &self.model_id {
            entries.push(ConfigurationEntry::new(
                "model_id",
                ConfigurationValue::String(model_id.clone()),
            )?);
        }
        if self.trim == TrimPolicy::Batched {
            entries.push(ConfigurationEntry::new(
                "trim",
                ConfigurationValue::String("batched".to_owned()),
            )?);
        }
        if self.autotest {
            entries.push(ConfigurationEntry::new(
                "autotest",
                ConfigurationValue::Bool(true),
            )?);
        }
        if let Some(twm) = &self.twm {
            entries.push(ConfigurationEntry::new(
                "twm_rebuild_tokens",
                ConfigurationValue::Unsigned(twm.rebuild_tokens as u64),
            )?);
            entries.push(ConfigurationEntry::new(
                "twm_note",
                ConfigurationValue::Bool(twm.note),
            )?);
            entries.push(ConfigurationEntry::new(
                "twm_test_status",
                ConfigurationValue::Bool(twm.test_status),
            )?);
            entries.push(ConfigurationEntry::new(
                "twm_codemap",
                ConfigurationValue::Bool(twm.codemap),
            )?);
        }
        let value = ConfigurationValue::map(entries)?;
        ResolvedConfigurationRef::inline(model_propose_schema(), value)
    }

    /// Decodes what [`encode`](Self::encode) wrote back into a configuration.
    ///
    /// Crate-visible so the repair and fine-stage blocks share this decoder,
    /// and planner tests can inspect the exact configured budgets.
    ///
    /// # Errors
    ///
    /// Returns an error if `config` was not built by [`encode`](Self::encode)
    /// or fails one of its own field bounds.
    pub(crate) fn decode(config: &ResolvedConfigurationRef) -> Result<Self, BlockBuildError> {
        let entries = inline_map(config)?;
        let role = as_str(field(entries, "role")?)?.to_owned();
        let instruction = as_str(field(entries, "instruction")?)?.to_owned();
        let tool_subset = match field(entries, "tool_subset")? {
            ConfigurationValue::Null => ToolSubset::None,
            other => ToolSubset::Only(as_string_list(other)?),
        };
        let budget = TokenBudget(
            u32::try_from(as_u64(field(entries, "budget")?)?)
                .map_err(|_| BlockBuildError::new("budget does not fit in u32"))?,
        );
        // `u32::MAX` is lean's nonbinding sentinel (see `TokenBudget`). It
        // never reaches `LlmRequest.max_output_tokens` uncapped — see
        // `LiveModelStep::run` — so the ceiling, which exists to bound that
        // field, does not apply to it. A model cannot choose the sentinel:
        // `planner::build_step_answer` refuses any budget above the ceiling
        // before it is encoded.
        let nonbinding = budget.0 == u32::MAX;
        if budget.0 == 0 || (budget.0 > MAX_MODEL_STEP_TOKEN_BUDGET && !nonbinding) {
            return Err(BlockBuildError::new(format!(
                "model step budget must be in 1..={MAX_MODEL_STEP_TOKEN_BUDGET} tokens, got {}",
                budget.0
            )));
        }
        let output = ModelStepOutputKind::parse(as_str(field(entries, "output")?)?)?;
        let max_turns = u16::try_from(as_u64(field(entries, "max_turns")?)?)
            .map_err(|_| BlockBuildError::new("max_turns does not fit in u16"))?;
        if max_turns == 0 || max_turns > MAX_MODEL_STEP_TURNS_CEILING {
            return Err(BlockBuildError::new(format!(
                "model step max_turns must be in 1..={MAX_MODEL_STEP_TURNS_CEILING}, got {max_turns}"
            )));
        }
        let sees_prior_failure = match field(entries, "sees_prior_failure")? {
            ConfigurationValue::Bool(value) => *value,
            _ => return Err(BlockBuildError::new("sees_prior_failure must be a boolean")),
        };
        let review_before_submit = match field(entries, "review_before_submit")? {
            ConfigurationValue::Bool(value) => *value,
            _ => {
                return Err(BlockBuildError::new(
                    "review_before_submit must be a boolean",
                ));
            }
        };
        let context_mode = entries
            .iter()
            .find(|entry| entry.key() == "context_mode")
            .map(|entry| ContextMode::parse(as_str(entry.value())?))
            .transpose()?
            .unwrap_or_default();
        let optional_bool = |key, default| -> Result<bool, BlockBuildError> {
            match entries
                .iter()
                .find(|entry| entry.key() == key)
                .map(|entry| entry.value())
            {
                None => Ok(default),
                Some(ConfigurationValue::Bool(value)) => Ok(*value),
                _ => Err(BlockBuildError::new(format!("{key} must be a boolean"))),
            }
        };
        let model_id = entries
            .iter()
            .find(|entry| entry.key() == "model_id")
            .map(|entry| as_str(entry.value()).map(str::to_owned))
            .transpose()?;
        if model_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || !id.contains('/'))
        {
            return Err(BlockBuildError::new("model_id must be provider/model"));
        }
        let mut decoded = Self::new(role, instruction, tool_subset, budget, output)
            .with_max_turns(max_turns)
            .with_prior_failure_visible_if(sees_prior_failure)
            .with_review_before_submit_if(review_before_submit)
            .with_context_mode(context_mode);
        decoded.compact_history = optional_bool("compact_history", true)?;
        decoded.strict_turn_cap = optional_bool("strict_turn_cap", false)?;
        decoded.stable_tool_choice = optional_bool("stable_tool_choice", false)?;
        decoded.model_id = model_id;
        decoded.trim = match entries.iter().find(|entry| entry.key() == "trim") {
            None => TrimPolicy::None,
            Some(entry) if as_str(entry.value())? == "batched" => TrimPolicy::Batched,
            Some(_) => return Err(BlockBuildError::new("unknown model step trim policy")),
        };
        decoded.autotest = optional_bool("autotest", false)?;
        decoded.twm = match entries
            .iter()
            .find(|entry| entry.key() == "twm_rebuild_tokens")
        {
            None => None,
            Some(entry) => {
                let rebuild_tokens = usize::try_from(as_u64(entry.value())?)
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        BlockBuildError::new("twm_rebuild_tokens must be positive and fit in usize")
                    })?;
                Some(twm::TwmOptions {
                    rebuild_tokens,
                    note: optional_bool("twm_note", false)?,
                    test_status: optional_bool("twm_test_status", false)?,
                    codemap: optional_bool("twm_codemap", false)?,
                })
            }
        };
        Ok(decoded)
    }
}

impl ModelStepConfig {
    const fn with_prior_failure_visible_if(self, sees_prior_failure: bool) -> Self {
        if sees_prior_failure {
            self.with_prior_failure_visible()
        } else {
            self
        }
    }

    const fn with_review_before_submit_if(self, review_before_submit: bool) -> Self {
        if review_before_submit {
            self.with_review_before_submit()
        } else {
            self
        }
    }
}

/// One artifact a model step can produce before its configured output is
/// projected into the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStepArtifact {
    /// A plan, in prose.
    Plan(String),
    /// Findings, one claim per entry.
    Findings(Vec<String>),
    /// Proposed exact-string edits, not yet applied to the tree, plus an
    /// optional check script the model wrote and (if a `run_python` tool was
    /// offered) ran to see the issue before submitting.
    PatchIntent {
        /// The proposed exact-string edits.
        edits: Vec<ExactEdit>,
        /// A short script that reproduces the issue: expected to fail or
        /// print wrong output before the edits are applied and pass after.
        /// Never applied to the tree and never part of the final patch —
        /// only run by a run-check block, from the agent's own state
        /// directory. `None` if the model gave no check, which a plan must
        /// treat as an ordinary patch intent rather than an error.
        check: Option<String>,
    },
    /// A review verdict plus explanatory notes.
    Review {
        /// The reviewer's verdict.
        verdict: Status,
        /// Why, in prose.
        notes: String,
    },
    /// An answer to a stated question.
    Answer(String),
}

/// A model-proposed exact-edit patch that may be wired into a mutating
/// application block.
///
/// Unlike the broad [`ModelStepArtifact`] result accepted from a model
/// executor, this is the graph output of a `model-propose` block configured
/// with [`ModelStepOutputKind::PatchIntent`]. Its distinct type makes an
/// attempt to wire any other configured model output into
/// [`ApplyPatchIntentBlock`] fail graph validation before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchIntent {
    /// The proposed exact-string edits.
    pub edits: Vec<ExactEdit>,
    /// See [`ModelStepArtifact::PatchIntent`]'s `check` field.
    pub check: Option<String>,
}

/// A [`PatchIntent`] paired with the conversation that produced it.
///
/// The graph output of a propose block that keeps its conversation around
/// for a possible repair round ([`super::repair::ModelProposeConversationBlock`],
/// [`super::repair::ResumeModelProposeBlock`]), as opposed to
/// [`ModelProposeBlock`], whose plain [`PatchIntent`] output has nothing left
/// to resume. A small adapter
/// ([`super::repair::ExtractPatchFromProposalBlock`]) projects this down to a
/// plain [`PatchIntent`] wherever an existing block (apply, judge) only ever
/// needed that half.
#[derive(Debug, Clone)]
pub struct ProposedPatchIntent {
    /// The proposed patch.
    pub patch: PatchIntent,
    /// The conversation that produced `patch`, resumable by a later repair
    /// round instead of starting a fresh one.
    pub conversation: ModelConversation,
}

/// Boxed future returned by [`ModelStepExecutor::propose_conversation`] and
/// its overrides — factored out purely to keep that signature readable.
type ProposeConversationFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<(ModelStepArtifact, ModelConversation), SystemError>>
            + Send
            + 'a,
    >,
>;

/// A capability that runs one bounded model turn.
///
/// Coding-specific (it reads a [`WorkspaceView`]), so it lives here rather
/// than in `polaris_cosmos`. A host installs a live, provider-backed
/// implementation ([`LiveModelStep`]) as a [`ModelStepProvider`] resource; a
/// test installs [`ScriptedModelStep`] instead, matching how
/// [`ProjectTestRunner`]'s absence versus presence already decides whether
/// [`super::existing::RunProjectTests`] can run.
///
/// `propose` is async and object-safe (a boxed future rather than an
/// `async fn`, which cannot appear in a trait object) because the graph
/// system that calls it, [`model_propose`], is itself already `async` — the
/// call site simply awaits the returned future, with no sync bridge needed.
/// A live implementation also bills `meter` as it goes, exactly as the model
/// gate planner bills the same accounting for its own calls.
///
/// [`ProjectTestRunner`]: super::super::payload::ProjectTestRunner
pub trait ModelStepExecutor: Send + Sync {
    /// Runs one bounded turn under `config` and returns its typed artifact.
    ///
    /// # Errors
    ///
    /// Returns an error if the turn could not produce a usable artifact.
    fn propose<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        workspace: &'a WorkspaceView<'a>,
        meter: &'a TokenMeter,
    ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>;

    /// Like [`propose`](Self::propose), but also returns the full turn
    /// history it produced and, given `resume`, continues an earlier step's
    /// history instead of starting a fresh one.
    ///
    /// Resuming (rather than restarting) is what lets a repair round reuse
    /// everything the model already read about the task — the code it
    /// explored, the edit it proposed — instead of paying to re-derive that
    /// context in a brand-new conversation. The default implementation
    /// ignores `resume` and ignores the history it would return, so an
    /// executor that only ever runs one-shot steps (for example
    /// [`ScriptedModelStep`] in a test that does not care about resumption)
    /// need not implement it.
    ///
    /// # Errors
    ///
    /// Returns an error if the turn could not produce a usable artifact.
    fn propose_conversation<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        workspace: &'a WorkspaceView<'a>,
        meter: &'a TokenMeter,
        resume: Option<&'a ModelConversation>,
    ) -> ProposeConversationFuture<'a> {
        let _ = resume;
        Box::pin(async move {
            let artifact = self.propose(config, workspace, meter).await?;
            Ok((artifact, ModelConversation::default()))
        })
    }
}

/// The full message history of one model step's turn, so a later step can
/// resume it instead of starting a fresh conversation.
///
/// Opaque outside this module: nothing else reads or builds its content, it
/// only ever comes from a preceding [`ModelStepExecutor::propose_conversation`]
/// call and is only ever fed into a following one.
#[derive(Debug, Clone, Default)]
pub struct ModelConversation(Vec<Message>);

impl ModelConversation {
    /// How many messages this conversation holds — a structural fact a test
    /// or a caller may check without reading message content, which stays
    /// opaque outside this module.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this conversation has no messages yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One model step's durable facts for a brief handoff, separate from the
/// conversation that a later `continue` step may resume.
#[derive(Debug, Clone, Default)]
pub struct ModelStepRecord {
    /// The step's configured role.
    pub role: String,
    /// A bounded rendering of the artifact the step submitted.
    pub result: String,
    /// Paths named by edits and file-oriented tool calls.
    pub files: Vec<String>,
    /// Commands or scripts the step ran, with their observed status.
    pub commands: Vec<String>,
    /// Other tool findings, with their observed status.
    pub observations: Vec<String>,
    /// Questions explicitly left open by the step.
    pub open_questions: Vec<String>,
    /// Whether the resulting conversation contains compacted tool output.
    pub compacted: bool,
}

/// Task-scoped context shared by model steps in one gate-plan execution.
/// The record supports `brief`; the opaque last conversation supports
/// `continue`. A plan's configurations decide which, if either, to consume.
#[derive(Debug, Default, Clone)]
pub struct ModelStepHandoff {
    inner: Arc<Mutex<ModelStepHandoffState>>,
}

#[derive(Debug, Default, Clone)]
struct ModelStepHandoffState {
    records: Vec<ModelStepRecord>,
    conversation: Option<ModelConversation>,
    last_output: Option<ModelStepOutputKind>,
    autotest: Option<AutotestEvidence>,
}

/// A passing pytest subset for a specific, unchanged workspace state.
#[derive(Debug, Clone)]
pub(super) struct AutotestEvidence {
    pub targets: Vec<String>,
    pub tree_signature: u64,
}

impl polaris::system::resource::LocalResource for ModelStepHandoff {}

impl ModelStepHandoff {
    pub(super) fn autotest(&self) -> Option<AutotestEvidence> {
        self.snapshot().autotest
    }

    fn set_autotest(&self, observation: Option<AutotestEvidence>) {
        self.inner
            .lock()
            .expect("handoff lock is not poisoned")
            .autotest = observation;
    }

    /// Completed model steps in execution order.
    #[must_use]
    pub fn records(&self) -> Vec<ModelStepRecord> {
        self.snapshot().records
    }

    /// A bounded summary, at most 4,096 characters, of the last four steps.
    #[must_use]
    pub fn brief(&self) -> String {
        Self::brief_records(&self.snapshot().records)
    }

    fn brief_records(records: &[ModelStepRecord]) -> String {
        let mut text = String::from("Earlier model steps (bounded handoff):\n");
        for record in records.iter().rev().take(4).rev() {
            let questions = if record.open_questions.is_empty() {
                "not stated".to_owned()
            } else {
                record.open_questions.join("; ")
            };
            let line = format!(
                "Role: {}; result: {}; files: {}; commands: {}; findings: {}; open questions: {}; compacted: {}\n",
                record.role,
                record.result,
                record.files.join(", "),
                record.commands.join("; "),
                record.observations.join("; "),
                questions,
                record.compacted
            );
            let limit = if record.role == "cheap-locate" {
                4_000
            } else {
                950
            };
            text.push_str(&bounded(&line, limit));
        }
        text.chars().take(4_096).collect()
    }

    fn snapshot(&self) -> ModelStepHandoffState {
        self.inner
            .lock()
            .expect("handoff lock is not poisoned")
            .clone()
    }

    pub(crate) fn record(
        &self,
        config: &ModelStepConfig,
        artifact: &ModelStepArtifact,
        conversation: ModelConversation,
        previous_messages: usize,
    ) {
        let mut record = ModelStepRecord {
            role: bounded(&config.role, 120),
            result: if config.role == "cheap-locate" {
                match artifact {
                    ModelStepArtifact::Answer(value) => bounded(value, 3_500),
                    _ => bounded(&format!("{artifact:?}"), 512),
                }
            } else {
                bounded(&format!("{artifact:?}"), 512)
            },
            ..ModelStepRecord::default()
        };
        let claims: Vec<&str> = match artifact {
            ModelStepArtifact::Plan(text) | ModelStepArtifact::Answer(text) => vec![text],
            ModelStepArtifact::Findings(items) => items.iter().map(String::as_str).collect(),
            ModelStepArtifact::Review { notes, .. } => vec![notes],
            ModelStepArtifact::PatchIntent { .. } => Vec::new(),
        };
        record.open_questions = claims
            .iter()
            .flat_map(|claim| claim.lines())
            .filter(|line| line.trim_end().ends_with('?'))
            .take(4)
            .map(|line| bounded(line, 160))
            .collect();
        if let ModelStepArtifact::PatchIntent { edits, .. } = artifact {
            record
                .files
                .extend(edits.iter().map(|edit| bounded(edit.file(), 120)));
        }
        record.compacted = conversation.0.iter().any(|message| {
            matches!(message, Message::User { content } if content.iter().any(|block| matches!(block, UserBlock::ToolResult(result) if matches!(&result.content, ToolResultContent::Text(text) if text.starts_with(crate::agents::context::COMPACTED_MARKER)))))
        });
        for message in conversation.0.iter().skip(previous_messages) {
            if config.role == "cheap-locate" {
                break; // the next step receives the report, never tool history
            }
            match message {
                Message::Assistant { content, .. } => {
                    for block in content {
                        if let AssistantBlock::ToolCall(call) = block {
                            let name = call.function.name.as_str();
                            let args = &call.function.arguments;
                            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str)
                            {
                                record.files.push(bounded(path, 120));
                            }
                            if name == RUN_PYTHON_TOOL || name == RUN_COMMAND_TOOL {
                                record.commands.push(format!(
                                    "{} {name} {}",
                                    bounded(&call.id, 64),
                                    bounded(&args.to_string(), 160)
                                ));
                            } else if name != SUBMIT_ANSWER_TOOL {
                                record.observations.push(format!(
                                    "{} {}",
                                    name,
                                    bounded(&args.to_string(), 120)
                                ));
                            }
                        }
                    }
                }
                Message::User { content } => {
                    for block in content {
                        if let UserBlock::ToolResult(result) = block
                            && let ToolResultContent::Text(text) = &result.content
                        {
                            let status = format!(
                                "{}: {:?} {}",
                                bounded(&result.id, 64),
                                result.status,
                                bounded(text, 160)
                            );
                            record.observations.push(status);
                        }
                    }
                }
            }
        }
        record.files.sort();
        record.files.dedup();
        record.files.truncate(16);
        record.commands.truncate(8);
        record.observations.truncate(12);
        let mut state = self.inner.lock().expect("handoff lock is not poisoned");
        state.records.push(record);
        state.conversation = Some(conversation);
        state.last_output = Some(config.output);
    }
}

fn bounded(value: &str, chars: usize) -> String {
    value.chars().take(chars).collect()
}

/// The installed [`ModelStepExecutor`] a running graph reads.
#[derive(Clone)]
pub struct ModelStepProvider(Arc<dyn ModelStepExecutor>);

impl fmt::Debug for ModelStepProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelStepProvider(..)")
    }
}

impl ModelStepProvider {
    /// Installs an executor as the provider a running graph will read.
    pub fn new(executor: impl ModelStepExecutor + 'static) -> Self {
        Self(Arc::new(executor))
    }

    /// Runs a fresh bounded model step for a sibling coding block.
    pub(super) fn propose<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        workspace: &'a WorkspaceView<'a>,
        meter: &'a TokenMeter,
    ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>> {
        self.0.propose(config, workspace, meter)
    }

    /// Delegates to the installed executor's
    /// [`ModelStepExecutor::propose_conversation`], for a repair-loop block
    /// ([`super::repair::ModelProposeConversationBlock`],
    /// [`super::repair::ResumeModelProposeBlock`]) that needs the returned
    /// conversation to resume later, not just the artifact
    /// [`propose`](ModelStepExecutor::propose) alone would give it.
    pub(super) fn propose_conversation<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        workspace: &'a WorkspaceView<'a>,
        meter: &'a TokenMeter,
        resume: Option<&'a ModelConversation>,
    ) -> ProposeConversationFuture<'a> {
        self.0
            .propose_conversation(config, workspace, meter, resume)
    }
}

impl polaris::system::resource::LocalResource for ModelStepProvider {}

/// A deterministic [`ModelStepExecutor`] that always returns one fixed
/// artifact, regardless of configuration or workspace state.
///
/// Composition must be testable without a live model: this is the coding
/// embodiment's scripted stand-in for a live provider-backed executor, the
/// same role a scripted planner plays for plan proposal.
#[derive(Debug, Clone)]
pub struct ScriptedModelStep {
    artifact: ModelStepArtifact,
}

impl ScriptedModelStep {
    /// Scripts a fixed artifact to return from every `propose` call.
    #[must_use]
    pub const fn new(artifact: ModelStepArtifact) -> Self {
        Self { artifact }
    }
}

impl ModelStepExecutor for ScriptedModelStep {
    fn propose<'a>(
        &'a self,
        _config: &'a ModelStepConfig,
        _workspace: &'a WorkspaceView<'a>,
        _meter: &'a TokenMeter,
    ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.artifact.clone()) })
    }

    /// Simulates continuity for tests of a resume-capable plan: each call
    /// (fresh or resumed) appends one message standing in for that turn, so a
    /// test can assert a repair step's conversation grew from its
    /// predecessor's rather than starting over.
    fn propose_conversation<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        _workspace: &'a WorkspaceView<'a>,
        _meter: &'a TokenMeter,
        resume: Option<&'a ModelConversation>,
    ) -> ProposeConversationFuture<'a> {
        let mut messages = resume.map_or_else(Vec::new, |conversation| conversation.0.clone());
        messages.push(Message::user(config.instruction.clone()));
        let artifact = self.artifact.clone();
        Box::pin(async move { Ok((artifact, ModelConversation(messages))) })
    }
}

// ---------------------------------------------------------------------------
// The live executor: a real bounded, tool-restricted, budgeted turn against
// a configured model provider.
// ---------------------------------------------------------------------------

/// The tools a live model step may call, each gated independently by
/// [`ModelStepConfig::tool_subset`].
///
/// `tool_subset` is configuration data a plan wrote, not a trusted grant by
/// itself: this executor never widens its own reach to match whatever a
/// configuration names. Naming any other string in `tool_subset` currently
/// grants nothing to call, which is what keeps a read-only role read-only
/// regardless of configuration content — [`EDIT_FILE_TOOL`] is the one
/// exception, and it is reachable only through the one block registration
/// that declares itself mutating (see its own doc comment).
const READ_FILE_TOOL: &str = "read_file";
const LIST_DIR_TOOL: &str = "list_dir";
const SEARCH_TOOL: &str = "search";
/// Lets a propose step see the bug before submitting: writes the script it
/// passes to [`RUN_PYTHON_TOOL`] to a fixed path under `.polaris/` and runs
/// it, so it can reproduce the issue first-hand instead of guessing.
const RUN_PYTHON_TOOL: &str = "run_python";
const RUN_COMMAND_TOOL: &str = "run_command";
/// Exact-string-replace direct edit, shared with the interactive `edit_file`
/// tool's own contract (unique match required, whitespace significant) via
/// [`apply_exact_edit`]. Named in a step's `tool_subset` only by the "one
/// generic loop" experiment arm's own plan
/// (`crate::cosmos::planner::loop_statement_only_plan_body`): every other
/// plan shape proposes a [`PatchIntent`] for a separate block to apply, so
/// this is the one tool an ordinary `code.model-propose` step is never
/// configured with. See [`ModelDirectEditBlock`] for why this needs its own
/// block registration rather than riding along on `code.model-propose`'s.
const EDIT_FILE_TOOL: &str = "edit_file";

/// Bound on how many immediate directory entries [`list_dir_bounded`] reports.
const MAX_LIST_DIR_ENTRIES: usize = 200;

/// Bounds on [`search_bounded`]'s walk: how many files it opens, how large a
/// file it will read, and how many matching lines it reports. `MAX_SEARCH_FILES`
/// matches the agent-facing `search` tool's own cap
/// ([`crate::tools::code`]) rather than a stricter one of its own: a lower
/// cap here produced false "no matches" results on an ordinary repository
/// walk, which a model cannot distinguish from a real absence — see
/// `docs/gate-planning.md`. The rendered result stays bounded regardless, by
/// [`MAX_SEARCH_MATCHES`] and [`MAX_SEARCH_RESULT_BYTES`] below.
const MAX_SEARCH_FILES: usize = 20_000;
const MAX_SEARCH_FILE_BYTES: u64 = 1 << 20;
const MAX_SEARCH_MATCHES: usize = 50;

/// Bound on the rendered `search` result text, independent of match count: a
/// handful of very long lines must not blow the step's own token budget any
/// more than an unbounded match count would.
const MAX_SEARCH_RESULT_BYTES: usize = 4_096;

/// Bound on `search`'s `context_lines`: enough to show the shape around a
/// match without turning one hit into another unscoped file dump. A request
/// above this is clamped rather than refused — see [`search_bounded`].
const MAX_SEARCH_CONTEXT_LINES: usize = 5;

/// Fixed, host-controlled relative path (inside the agent's own
/// [`crate::state::state_dir`], so it is git-invisible and never lands in a
/// patch) [`run_python_script`] writes a `run_python` tool call's script to
/// before running it. Fixed rather than model-chosen so the shell command
/// that runs it is always the same literal — no model-controlled text ever
/// reaches a command line. [`super::repair::CHECK_SCRIPT_PATH`] is the
/// analogous fixed name a run-check block writes a submitted `check` script
/// to; the two stay distinct so a check written post-apply never collides
/// with whatever the model last explored with `run_python`.
const PYTHON_SCRIPT_PATH: &str = "model_step_script.py";

/// Bound on the combined stdout+stderr [`run_python_script`] renders back,
/// mirroring [`MAX_SEARCH_RESULT_BYTES`] for the same reason: a script that
/// prints a lot must not blow a step's token budget, or a run-check block's
/// captured-failure text, past its bound. Independent of and smaller than
/// the real [`polaris::shell::ShellExecutor`]'s own output cap.
const MAX_PYTHON_OUTPUT_BYTES: usize = 4_096;

/// How long [`run_python_script`] lets one script run before it is treated
/// as hung rather than waited on indefinitely — model-authored code the
/// harness does not otherwise control.
const PYTHON_SCRIPT_TIMEOUT_SECS: u64 = 30;

fn run_python_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        RUN_PYTHON_TOOL,
        "Writes a Python script to a scratch file and runs it with `python`, \
         returning its exit code and output. Use this to see a bug before \
         you submit your answer — for example, run a short script that \
         reproduces the issue so you know it actually fails first. The \
         script is never applied to the tree and never part of your patch.",
        serde_json::json!({
            "type": "object",
            "properties": { "script": { "type": "string" } },
            "required": ["script"],
            "additionalProperties": false,
        }),
    )
}

fn run_command_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        RUN_COMMAND_TOOL,
        "Runs a shell command in the workspace (300s timeout); returns exit code and output head/tail.",
        serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"],
            "additionalProperties": false,
        }),
    )
}

fn edit_file_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        EDIT_FILE_TOOL,
        "Replaces an exact substring in a file and writes the change immediately. \
         old_string must occur in the file exactly once; add surrounding context if it \
         does not, or read the file first if you are not sure it is unique. Unlike a \
         proposed patch, this edit lands on disk right away — read the file back with \
         read_file if you want to confirm it before submitting your answer.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false,
        }),
    )
}

/// Applies one exact-string edit directly to the real working tree, sharing
/// [`apply_exact_edit`]'s sandboxed, unique-match contract with
/// [`ApplyPatchIntentBlock`]'s own edits — the same guarantee, called from a
/// model step's own turn instead of from a separate apply block downstream
/// of it. Returns a short outcome line rather than a full diff: the model
/// already knows what it asked for, and can `read_file` back to confirm it if
/// it needs to.
fn edit_file_bounded(
    config: &CodeToolsConfig,
    path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<(String, bool), String> {
    if old_string == new_string {
        return Err(format!(
            "edit_file '{path}': old_string and new_string are identical"
        ));
    }
    match apply_exact_edit(config, path, old_string, new_string) {
        Ok(EditOutcome::Applied { path, bytes_after }) => Ok((
            format!("Edited {path} ({bytes_after} bytes after edit)."),
            true,
        )),
        Ok(EditOutcome::AlreadySatisfied { path }) => Ok((
            format!("{path} already reads as new_string; no write was needed."),
            false,
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// Writes `script` to `script_name` under `working_dir`'s `.polaris` state
/// directory and runs it with `python`, through the real installed
/// [`polaris::shell::ShellExecutor`] — its own permission check, timeout and
/// output cap all apply exactly as they would to any other shell-backed
/// tool, on top of this function's own [`MAX_PYTHON_OUTPUT_BYTES`] render cap
/// and [`PYTHON_SCRIPT_TIMEOUT_SECS`] bound. `script_name` is always one of
/// this module's own fixed constants ([`PYTHON_SCRIPT_PATH`] or
/// [`super::repair::CHECK_SCRIPT_PATH`]), never model-chosen, so the shell
/// command run is always one of two fixed literals.
///
/// Fails closed rather than open. [`super::super::host::effective_host_capabilities`]
/// treats no installed `ShellExecutor` as "no policy to honour" because it
/// only ever answers a boolean admission-time probe; this function actually
/// executes model-authored code, a materially more consequential action, so
/// a missing executor, a denial, or a policy that would ask a human to
/// confirm (`ShellResponse::ConfirmationRequired` — there is no human to ask
/// mid-model-step) all refuse the call the same way rather than one of them
/// silently allowing it.
pub(super) async fn run_python_script(
    shell: Option<&polaris::shell::ShellExecutor>,
    working_dir: &std::path::Path,
    script_name: &str,
    script: &str,
) -> Result<PythonRunOutcome, String> {
    let Some(shell) = shell else {
        return Err("no shell is configured for this run; run_python is unavailable".to_owned());
    };
    let dir = crate::state::state_dir(working_dir)
        .map_err(|error| format!("could not prepare the state directory: {error}"))?;
    tokio::fs::write(dir.join(script_name), normalize_python_script(script))
        .await
        .map_err(|error| format!("could not write the script: {error}"))?;
    let relative = std::path::Path::new(crate::state::STATE_DIR_NAME).join(script_name);
    let command = format!("python {}", relative.display());
    if !matches!(
        shell.check_permission(&command),
        polaris::shell::ShellPermission::Allow
    ) {
        return Err(format!(
            "the shell policy does not allow '{command}' to run unattended"
        ));
    }
    let request = polaris::shell::ShellRequest::new(command)
        .with_working_dir(working_dir.to_path_buf())
        .with_timeout(PYTHON_SCRIPT_TIMEOUT_SECS);
    let response = shell
        .execute(request)
        .await
        .map_err(|error| format!("could not run the script: {error}"))?;
    match response {
        polaris::shell::ShellResponse::Executed(result) => Ok(PythonRunOutcome {
            success: result.exit_code == Some(0) && !result.timed_out,
            rendered: render_execution_result(&result),
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            stderr: result.stderr,
        }),
        polaris::shell::ShellResponse::ConfirmationRequired(_) => Err(
            "the shell policy would require confirmation to run this script; refusing rather \
             than blocking on a human that is not there"
                .to_owned(),
        ),
    }
}

/// What one [`run_python_script`] run produced: whether the process exited
/// zero within its timeout, and the rendered text a model or a repair prompt
/// reads. Split out (rather than the `run_python` tool's plain rendered
/// string) so [`super::repair::RunCheckScriptBlock`] can judge pass/fail the
/// same way [`super::super::payload::ProjectTestRunner::run`] does, without
/// re-parsing rendered text.
pub(super) struct PythonRunOutcome {
    /// Whether the script exited zero within its timeout.
    pub(super) success: bool,
    /// The rendered exit code plus captured stdout/stderr.
    pub(super) rendered: String,
    /// Exit status, absent when the process was killed.
    pub(super) exit_code: Option<i32>,
    /// Whether the shell timed out the process.
    pub(super) timed_out: bool,
    /// Raw stderr, used to classify a check failure before rendering truncates it.
    pub(super) stderr: String,
}

fn normalize_python_script(script: &str) -> String {
    if !script.contains('\n') && script.contains("\\n") {
        script.replace("\\n", "\n")
    } else {
        script.to_owned()
    }
}

/// Renders one [`polaris::shell::ExecutionResult`] as text a model can read:
/// exit code, then stdout and stderr, each capped independently so neither
/// stream alone can exceed [`MAX_PYTHON_OUTPUT_BYTES`].
fn render_execution_result(result: &polaris::shell::ExecutionResult) -> String {
    let cap = |text: &str| -> String {
        if text.len() <= MAX_PYTHON_OUTPUT_BYTES {
            text.to_owned()
        } else {
            let mut end = MAX_PYTHON_OUTPUT_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let mut capped = text[..end].to_owned();
            capped.push_str("\n… truncated");
            capped
        }
    };
    let mut rendered = match result.exit_code {
        Some(code) => format!("exit code: {code}"),
        None => "exit code: (none; the process was killed)".to_owned(),
    };
    if result.timed_out {
        rendered.push_str(&format!("\ntimed out after {PYTHON_SCRIPT_TIMEOUT_SECS}s"));
    }
    if !result.stdout.is_empty() {
        rendered.push_str(&format!("\nstdout:\n{}", cap(&result.stdout)));
    }
    if !result.stderr.is_empty() {
        rendered.push_str(&format!("\nstderr:\n{}", cap(&result.stderr)));
    }
    rendered
}

/// Characters that mean something other than themselves in a regular
/// expression. A `literal=true` search escapes every one of these before
/// matching, most surprisingly `|`: it is matched as a literal pipe
/// character, not interpreted as OR. [`search_bounded`] uses this to warn
/// against reading a `literal` search's empty result as a real absence when
/// the pattern looks like it was meant as a regular expression.
const REGEX_METACHARACTERS: [char; 10] = ['.', '^', '$', '*', '+', '?', '(', ')', '[', '|'];

fn looks_like_regex_syntax(pattern: &str) -> bool {
    pattern.contains(REGEX_METACHARACTERS.as_slice())
}

fn read_file_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        READ_FILE_TOOL,
        "Reads one sandbox-relative file. Pass start_line/max_lines to read a \
         range instead of the whole file; a ranged read is line-numbered, and \
         a result cut short states the file's total line count and the \
         start_line to pass next.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer", "minimum": 1 },
                "max_lines": { "type": "integer", "minimum": 1 },
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
    )
}

fn list_dir_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        LIST_DIR_TOOL,
        "Lists the immediate entries of one sandbox-relative directory, one \
         name per line; a directory's own name ends with '/'.",
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false,
        }),
    )
}

fn search_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        SEARCH_TOOL,
        "Searches file contents for a literal string or a regular expression, \
         returning matching lines as 'path:line: text'. Scope with `path` (a \
         file or directory) rather than searching the whole tree; a scan \
         stopped by its file or match cap, or a context_lines past the max, \
         says so in the result rather than refusing. literal=true matches \
         regex syntax like '|' as plain text, not as OR.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "literal": { "type": "boolean" },
                "case_insensitive": { "type": "boolean" },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_SEARCH_CONTEXT_LINES,
                },
                "files_only": { "type": "boolean" },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        }),
    )
}

/// Reads one sandbox-relative file for the model step's `read_file` tool.
///
/// Off the async executor rather than a blocking syscall, and capped at the
/// same [`MAX_READ_BYTES`] the agent-facing `read_file` tool uses, so a large
/// in-workspace file cannot stall a worker thread, inflate the next request
/// past the step's own token budget, or bypass the cap the model-facing
/// surface is supposed to share. A ranged read (`start_line` and/or
/// `max_lines` given) shares [`read_view`] with that same agent-facing
/// tool, so it carries the identical line numbering, truncation notice and
/// `Next: read_file(...)` resumption hint a model may already have learned
/// from the interactive surface, rather than a second, drifting rendering of
/// the same window.
async fn read_file_bounded(
    config: &CodeToolsConfig,
    path: &str,
    start_line: Option<usize>,
    max_lines: Option<usize>,
) -> Result<String, EditError> {
    let resolved = config
        .resolve_path(path)
        .ok_or_else(|| EditError::OutsideSandbox {
            path: path.to_owned(),
        })?;
    let content = tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|source| EditError::Io {
            path: path.to_owned(),
            source,
        })?;
    if start_line.is_some() || max_lines.is_some() {
        let from = start_line.unwrap_or(1);
        return Ok(
            read_view(path, &content, from, max_lines, resume_footer).unwrap_or_else(|| {
                format!(
                    "(no lines: {path} has {} lines, and the requested range starts at {from})",
                    content.lines().count()
                )
            }),
        );
    }
    if content.len() <= MAX_READ_BYTES {
        return Ok(content);
    }
    Ok(raw_truncated(path, &content))
}

/// Names one `read_file` call for the duplicate-read note
/// [`LiveModelStep::run`] returns instead of the same text again.
fn describe_read(path: &str, start_line: Option<usize>, max_lines: Option<usize>) -> String {
    match (start_line, max_lines) {
        (None, None) => path.to_owned(),
        (start, max) => format!(
            "{path} (start_line={}, max_lines={})",
            start.unwrap_or(1),
            max.map_or_else(|| "all".to_owned(), |n| n.to_string())
        ),
    }
}

/// Lists one sandbox-relative directory's immediate entries for the model
/// step's `list_dir` tool, bounded at [`MAX_LIST_DIR_ENTRIES`] and sorted so
/// two calls against the same tree answer identically.
async fn list_dir_bounded(config: &CodeToolsConfig, path: &str) -> Result<String, String> {
    let resolved = config
        .resolve_path(path)
        .ok_or_else(|| format!("path '{path}' escapes the sandbox"))?;
    let mut entries = tokio::fs::read_dir(&resolved)
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    let mut names = Vec::new();
    let mut truncated = false;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| format!("{path}: {error}"))?
    {
        let is_dir = entry.file_type().await.is_ok_and(|kind| kind.is_dir());
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if is_dir {
            name.push('/');
        }
        names.push(name);
        if names.len() >= MAX_LIST_DIR_ENTRIES {
            truncated = true;
            break;
        }
    }
    names.sort();
    if names.is_empty() {
        return Ok("(empty directory)".to_owned());
    }
    let mut out = names.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n… stopped at {MAX_LIST_DIR_ENTRIES} entries; narrow with a deeper path"
        ));
    }
    Ok(out)
}

/// Searches the sandbox tree for the model step's `search` tool: literal text
/// or a regular expression under an optional `path` scope, bounded by
/// [`MAX_SEARCH_FILES`], [`MAX_SEARCH_FILE_BYTES`], [`MAX_SEARCH_MATCHES`] and
/// [`MAX_SEARCH_RESULT_BYTES`]. Walks with [`select_files`] — the same
/// `.gitignore`-aware, `.git`-skipping, name-sorted selection the agent-facing
/// `search`/`glob` tools already share — rather than a second, hand-rolled
/// walk with its own idea of which directories to skip. Runs on a
/// blocking thread: walking the tree and running a compiled regex over every
/// line is synchronous work that must not stall the async executor.
async fn search_bounded(
    config: &CodeToolsConfig,
    pattern: &str,
    path: Option<&str>,
    literal: bool,
    case_insensitive: bool,
    context_lines: usize,
    files_only: bool,
) -> Result<String, String> {
    if pattern.is_empty() {
        return Err("pattern is empty; pass the text or regular expression to search for".into());
    }
    // A request above the cap is clamped, not refused: a refusal costs the
    // model a whole turn, and it can already see the true ceiling from the
    // tool's own JSON schema.
    let (context_lines, context_lines_clamped) = if context_lines > MAX_SEARCH_CONTEXT_LINES {
        (MAX_SEARCH_CONTEXT_LINES, true)
    } else {
        (context_lines, false)
    };
    let source = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    let matcher = RegexBuilder::new(&source)
        .case_insensitive(case_insensitive)
        .size_limit(1 << 20)
        .build()
        .map_err(|error| {
            format!(
                "'{pattern}' is not a valid regular expression: {error}\n\
                 Set literal = true to search for this text exactly as written."
            )
        })?;
    let target = path.unwrap_or(".").to_owned();
    let root = config
        .resolve_path(&target)
        .ok_or_else(|| format!("path '{target}' escapes the sandbox"))?;
    let sandbox = config
        .working_dir
        .canonicalize()
        .map_err(|error| format!("working directory is unreadable: {error}"))?;
    tokio::task::spawn_blocking(move || {
        let selection = select_files(
            &sandbox,
            &root,
            None,
            true,
            WalkLimits {
                max_files: MAX_SEARCH_FILES,
                max_file_bytes: MAX_SEARCH_FILE_BYTES,
            },
        );
        render_search_bounded(
            &sandbox,
            &selection,
            &matcher,
            context_lines,
            files_only,
            &target,
        )
    })
    .await
    .map_err(|error| format!("search was interrupted: {error}"))
    .map(|mut rendered| {
        if context_lines_clamped {
            rendered = format!(
                "(context_lines clamped to the maximum, {MAX_SEARCH_CONTEXT_LINES})\n{rendered}"
            );
        }
        // Only worth the extra sentence on an empty result: a search that
        // did find lines was not misled by literal mode, whatever the
        // pattern looks like.
        if literal && rendered.contains("no matches") && looks_like_regex_syntax(pattern) {
            rendered.push_str(
                "\nliteral=true matched every character in the pattern exactly, including \
                 regular-expression syntax such as '|' — it is not treated as OR. Drop literal \
                 to search with a regular expression, or split this into separate literal \
                 searches.",
            );
        }
        rendered
    })
}

/// Renders [`search_bounded`]'s bounded result: matching lines with
/// `context_lines` of surrounding context (or, with `files_only`, just the
/// distinct paths that matched), plus an explicit note whenever the file walk
/// or the match count stopped at its own cap — never a bare "no matches" a
/// model cannot tell apart from a real absence.
fn render_search_bounded(
    root: &std::path::Path,
    selection: &Selection,
    matcher: &Regex,
    context_lines: usize,
    files_only: bool,
    target: &str,
) -> String {
    let mut hits: Vec<String> = Vec::new();
    let mut files_scanned = 0usize;
    let mut capped = false;

    'files: for file in &selection.files {
        let Ok(content) = std::fs::read_to_string(file) else {
            continue; // non-UTF-8 content is binary as far as a line search is concerned
        };
        files_scanned += 1;
        let display = slashed(file.strip_prefix(root).unwrap_or(file));
        let lines: Vec<&str> = content.lines().collect();
        let mut file_matched = false;
        for (index, line) in lines.iter().enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            file_matched = true;
            if files_only {
                break;
            }
            let start = index.saturating_sub(context_lines);
            let end = (index + context_lines).min(lines.len() - 1);
            for (i, shown) in lines.iter().enumerate().take(end + 1).skip(start) {
                let marker = if i == index { ':' } else { '-' };
                hits.push(format!("{display}{marker}{}: {}", i + 1, shown.trim()));
            }
            if hits.len() >= MAX_SEARCH_MATCHES {
                capped = true;
                break;
            }
        }
        if files_only && file_matched {
            hits.push(display);
            if hits.len() >= MAX_SEARCH_MATCHES {
                capped = true;
            }
        }
        if capped {
            break 'files;
        }
    }

    if hits.is_empty() {
        let mut out = format!("no matches in {files_scanned} file(s) under '{target}'");
        if selection.stopped_at_limit {
            out.push_str(&format!(
                "\n… scan stopped after {MAX_SEARCH_FILES} files; narrow `path`"
            ));
        }
        return out;
    }
    let mut out = hits.join("\n");
    if capped {
        out.push_str(&format!(
            "\n… stopped at {MAX_SEARCH_MATCHES} matches — there are more; narrow \
             `path` or the pattern"
        ));
    }
    if selection.stopped_at_limit {
        out.push_str(&format!(
            "\n… scan stopped after {MAX_SEARCH_FILES} files; narrow `path`"
        ));
    }
    if out.len() > MAX_SEARCH_RESULT_BYTES {
        out.truncate(MAX_SEARCH_RESULT_BYTES);
        out.push_str("\n… truncated");
    }
    out
}

/// Runs `fut` to completion unless the run is cancelled first, in which case
/// `fut` is dropped at its own await point and this returns the same kind of
/// refusal a budget or tool-permission check would — so a provider call or a
/// file read this step is waiting on cannot outlive a turn the host has
/// already given up on.
async fn cancellable<T>(
    cancel: &RunCancellation,
    role: &str,
    fut: impl Future<Output = T>,
) -> Result<T, SystemError> {
    cancel
        .run_until_cancelled(fut)
        .await
        .ok_or_else(|| exec(format!("model step '{role}' was cancelled")))
}

fn output_directions(kind: ModelStepOutputKind) -> &'static str {
    match kind {
        ModelStepOutputKind::Plan => {
            "When you are ready to finish, call submit_answer with \
             {\"kind\":\"plan\",\"plan\":\"...\"}."
        }
        ModelStepOutputKind::Findings => {
            "When you are ready to finish, call submit_answer with \
             {\"kind\":\"findings\",\"findings\":[\"...\"]}."
        }
        ModelStepOutputKind::PatchIntent => {
            "Before submitting: state the exact code path (file and function) that produces \
             the reported behavior, and run one scoped search for sibling variants — other \
             subclasses, managers, backends, or overloads named like it — that may need the \
             same change. Submit edits only after confirming the replacement changes the \
             stated behavior, covers each named case, and every find is an exact, unique \
             substring of the file you inspected. If an earlier edit validation failed, \
             inspect that exact location again before retrying. When you are ready to finish, \
             call submit_answer with \
             {\"kind\":\"patch-intent\",\"edits\":\
             [{\"file\":\"...\",\"find\":\"...\",\"replace\":\"...\"}]}, and \
             optionally \"check\":\"...\" — a short Python script that \
             reproduces the issue: it should fail or print wrong output \
             before your edits and pass after. Only include a check you \
             actually ran."
        }
        ModelStepOutputKind::Review => {
            "When you are ready to finish, call submit_answer with \
             {\"kind\":\"review\",\
             \"verdict\":\"pass|not-applicable|indeterminate|issue\",\"notes\":\"...\"}."
        }
        ModelStepOutputKind::Answer => {
            "When you are ready to finish, call submit_answer with \
             {\"kind\":\"answer\",\"answer\":\"...\"}."
        }
    }
}

/// Whether the opt-in repo layout is on (`POLARIS_CODE_EXP_REPO_LAYOUT=1`):
/// the first message of a step that may list directories also carries a
/// short listing of the workspace, so the model starts at the project root
/// instead of spending turns finding it.
fn repo_layout_enabled() -> bool {
    std::env::var("POLARIS_CODE_EXP_REPO_LAYOUT").is_ok_and(|v| v.trim() == "1")
}

/// Whether the opt-in test-runner note is on (`POLARIS_CODE_EXP_RUNNER_HINT=1`):
/// the first message names how the project runs its own tests.
fn run_python_hint_enabled() -> bool {
    std::env::var("POLARIS_CODE_EXP_RUNNER_HINT").is_ok_and(|v| v.trim() == "1")
}

/// Most lines [`repo_layout`] renders.
const REPO_LAYOUT_MAX_LINES: usize = 120;

/// The workspace's top level and, for each top-level directory, its direct
/// children, as relative paths (directories end in `/`). Hidden entries are
/// skipped. Bounded to [`REPO_LAYOUT_MAX_LINES`] lines.
fn repo_layout(working_dir: &std::path::Path) -> String {
    fn entries(dir: &std::path::Path) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = std::fs::read_dir(dir)
            .map(|read| {
                read.filter_map(Result::ok)
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let is_dir = entry.file_type().ok()?.is_dir();
                        (!name.starts_with('.')).then_some((name, is_dir))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }
    let mut lines = Vec::new();
    for (name, is_dir) in entries(working_dir) {
        if is_dir {
            lines.push(format!("{name}/"));
            let children = entries(&working_dir.join(&name));
            let shown = children.len().min(12);
            for (child, child_dir) in &children[..shown] {
                lines.push(format!(
                    "  {name}/{child}{}",
                    if *child_dir { "/" } else { "" }
                ));
            }
            if children.len() > shown {
                lines.push(format!("  {name}/... ({} more)", children.len() - shown));
            }
        } else {
            lines.push(name);
        }
    }
    let total = lines.len();
    lines.truncate(REPO_LAYOUT_MAX_LINES);
    let mut rendered = format!(
        "\nThe workspace is the project root (paths are relative to it). Its layout:\n{}",
        lines.join("\n")
    );
    if total > REPO_LAYOUT_MAX_LINES {
        rendered.push_str(&format!(
            "\n... ({} more lines)",
            total - REPO_LAYOUT_MAX_LINES
        ));
    }
    rendered.push('\n');
    rendered
}

/// Most times one step's submit guard may send the model back to work.
const SUBMIT_GUARD_MAX_USES: u8 = 2;

/// Whether the opt-in submit guard is on (`POLARIS_CODE_EXP_SUBMIT_GUARD=1`).
///
/// The guard is a system check around `submit_answer` in an editing step: it
/// refuses a submission while the workspace has no change at all, or while
/// the step's latest `run_python` check failed, and tells the model why. It
/// fires at most [`SUBMIT_GUARD_MAX_USES`] times and never on the last turn,
/// so it can delay a step's end but not prevent it.
fn submit_guard_enabled() -> bool {
    std::env::var("POLARIS_CODE_EXP_SUBMIT_GUARD").is_ok_and(|v| v.trim() == "1")
}

/// True when no tracked file under `working_dir` differs from `HEAD`. New
/// untracked files (repro scripts, the polaris state directory) do not count:
/// a fix edits the project. An unreadable status counts as changed, so a
/// missing git never blocks a submission.
fn workspace_is_unchanged(working_dir: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--"])
        .current_dir(working_dir)
        .status()
        .is_ok_and(|status| status.code() == Some(0))
}

/// The guard's reply to a refused `submit_answer`, or `None` to accept it.
fn submit_guard_notice(unchanged: bool, last_check_failed: bool) -> Option<String> {
    if unchanged {
        Some(
            "Not submitted: the workspace has no changes yet. The fix has to be an edit to \
             the project files. Make the change with edit_file, check it, then submit."
                .to_owned(),
        )
    } else if last_check_failed {
        Some(
            "Not submitted: your most recent run_python check failed. Read its output, fix \
             the code or the check, and run it again. Submit once it passes, or explain in \
             the answer why the failure is expected."
                .to_owned(),
        )
    } else {
        None
    }
}

/// Name of the tool a live model step's turn loop always offers, regardless
/// of [`ModelStepConfig::tool_subset`]: the one way this step's own turn
/// ends. Not gated by `tool_subset` the way `read_file`/`list_dir`/`search`
/// are — a step with no exploration tools at all must still be able to
/// answer.
const SUBMIT_ANSWER_TOOL: &str = "submit_answer";

/// Builds the `submit_answer` tool definition for one configured output
/// kind: its JSON-schema parameters mirror [`ModelStepAnswer`]'s shape for
/// exactly that `kind`, `kind` itself pinned to a one-value enum so a
/// schema-conformant call can never name the wrong one.
fn submit_answer_tool_definition(kind: ModelStepOutputKind) -> ToolDefinition {
    let (description, mut properties, required): (&str, serde_json::Value, &[&str]) = match kind {
        ModelStepOutputKind::Plan => (
            "Submits this step's final answer: a plan.",
            serde_json::json!({ "plan": { "type": "string" } }),
            &["kind", "plan"],
        ),
        ModelStepOutputKind::Findings => (
            "Submits this step's final answer: a list of findings.",
            serde_json::json!({
                "findings": { "type": "array", "items": { "type": "string" } },
            }),
            &["kind", "findings"],
        ),
        ModelStepOutputKind::PatchIntent => (
            "Submits this step's final answer: proposed exact-string edits, not yet applied.",
            serde_json::json!({
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "find": { "type": "string" },
                            "replace": { "type": "string" },
                        },
                        "required": ["file", "find", "replace"],
                        "additionalProperties": false,
                    },
                },
                "check": {
                    "type": "string",
                    "description": "Optional: a short Python script that reproduces the issue — it should fail or print wrong output before your edits and pass after. Only include this if you actually ran it and confirmed both.",
                },
            }),
            &["kind", "edits"],
        ),
        ModelStepOutputKind::Review => (
            "Submits this step's final answer: a review verdict plus explanatory notes.",
            serde_json::json!({
                "verdict": {
                    "type": "string",
                    "enum": ["pass", "not-applicable", "indeterminate", "issue"],
                },
                "notes": { "type": "string" },
            }),
            &["kind", "verdict", "notes"],
        ),
        ModelStepOutputKind::Answer => (
            "Submits this step's final answer to the stated question.",
            serde_json::json!({ "answer": { "type": "string" } }),
            &["kind", "answer"],
        ),
    };
    properties["kind"] = serde_json::json!({ "type": "string", "enum": [kind.tag()] });
    ToolDefinition::new(
        SUBMIT_ANSWER_TOOL,
        description,
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    )
}

/// One decoded model step answer, tagged by `kind` the same way
/// [`ModelStepOutputKind::tag`] names it.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum ModelStepAnswer {
    Plan {
        plan: String,
    },
    Findings {
        findings: Vec<String>,
    },
    PatchIntent {
        edits: Vec<PatchEditAnswer>,
        #[serde(default)]
        check: Option<String>,
    },
    Review {
        verdict: Status,
        notes: String,
    },
    Answer {
        answer: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchEditAnswer {
    file: String,
    find: String,
    replace: String,
}

impl ModelStepAnswer {
    fn into_artifact(self) -> Result<ModelStepArtifact, SystemError> {
        match self {
            Self::Plan { plan } => Ok(ModelStepArtifact::Plan(plan)),
            Self::Findings { findings } => Ok(ModelStepArtifact::Findings(findings)),
            Self::PatchIntent { edits, check } => {
                let edits = edits
                    .into_iter()
                    .map(|edit| ExactEdit::new(edit.file, edit.find, edit.replace).map_err(exec))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ModelStepArtifact::PatchIntent { edits, check })
            }
            Self::Review { verdict, notes } => Ok(ModelStepArtifact::Review { verdict, notes }),
            Self::Answer { answer } => Ok(ModelStepArtifact::Answer(answer)),
        }
    }
}

/// Decodes an already-parsed answer value — either `submit_answer`'s own
/// tool-call arguments, or a JSON value recovered from a lenient text
/// fallback — into the artifact `expected` commits the step's configuration
/// to, distinguishing two refusal shapes: `kind` does not match what the
/// configuration promised, or its fields for that `kind` are malformed.
fn decode_model_step_answer_value(
    value: serde_json::Value,
    expected: ModelStepOutputKind,
) -> Result<ModelStepArtifact, SystemError> {
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| exec("model step answer is missing a 'kind' field"))?
        .to_owned();
    if kind != expected.tag() {
        return Err(exec(format!(
            "model step answer kind '{kind}' does not match its configured output '{}'",
            expected.tag()
        )));
    }
    let answer: ModelStepAnswer = serde_json::from_value(value)
        .map_err(|error| exec(format!("malformed '{kind}' model step answer: {error}")))?;
    answer.into_artifact()
}

/// Decodes a model's final text reply into the artifact `expected` commits
/// the step's configuration to. Only the lenient text-reply fallback path
/// reaches this — a `submit_answer` tool call is decoded straight from its
/// already-parsed arguments via [`decode_model_step_answer_value`].
fn decode_model_step_answer(
    raw: &str,
    expected: ModelStepOutputKind,
) -> Result<ModelStepArtifact, SystemError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| exec(format!("model step answer is not valid JSON: {error}")))?;
    decode_model_step_answer_value(value, expected)
}

/// Recovers a JSON object from a free-form model reply: strips a ```` ```json
/// ```` / ```` ``` ```` fence if present, then narrows to the first `{` …
/// last `}` span, which also drops any prose surrounding an unfenced object.
/// Used only by the lenient text-reply fallback in [`LiveModelStep::run`] —
/// a `submit_answer` tool call never needs this, since the provider itself
/// guarantees well-formed arguments for it.
fn extract_json_object(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|body| body.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    let start = inner.find('{')?;
    let end = inner.rfind('}')?;
    (start <= end).then(|| &inner[start..=end])
}

/// A note appended to the next outgoing turn once a step is close to its own
/// [`ModelStepConfig::max_turns`] cap, so the model is told plainly to
/// submit rather than discovering the cap only by running out of turns.
/// `turn` is the just-completed turn's zero-based index; the note (if any)
/// is for whichever turn comes next. `max_turns` itself is never changed —
/// this only makes the existing cap visible in-band.
fn turns_left_notice(max_turns: u16, turn: u16) -> Option<String> {
    let remaining = max_turns.saturating_sub(turn + 1);
    (1..=3).contains(&remaining).then(|| {
        format!("Only {remaining} turn(s) remain in this step. You must call submit_answer now.")
    })
}

/// A note appended once, at the turn halfway through a step's own
/// `max_turns` cap: a long exploration that has not converged on anything
/// gets a nudge to name a concrete finding rather than only being told to
/// wrap up once turns are almost gone. `turn` is the just-completed turn's
/// zero-based index, matching [`turns_left_notice`]'s own convention.
///
/// This is a checkpoint, not a second cap: a step that has not converged by
/// the midpoint is not stopped, only reminded. It never fires alongside
/// [`turns_left_notice`] — see [`turn_notice`], which callers use instead of
/// either function directly.
fn exploration_checkpoint_notice(max_turns: u16, turn: u16) -> Option<String> {
    let midpoint = max_turns / 2;
    (midpoint > 0 && turn + 1 == midpoint).then(|| {
        "You are halfway through this step's turns. Name the concrete failure path you've \
         found so far — the exact file and function that produce the reported behavior — or \
         call submit_answer now if you already have enough to answer."
            .to_owned()
    })
}

/// The one-line notice for turn `turn` of a step capped at `max_turns`, if
/// any applies: [`turns_left_notice`] takes priority once the step is
/// actually close to its cap, otherwise [`exploration_checkpoint_notice`]
/// may fire at the midpoint. At most one notice is ever returned.
fn turn_notice(max_turns: u16, turn: u16) -> Option<String> {
    turns_left_notice(max_turns, turn).or_else(|| exploration_checkpoint_notice(max_turns, turn))
}

/// The tool choice for turn `turn` (zero-based) of a step capped at
/// `max_turns`: the last turn must call `submit_answer`, every earlier turn
/// is left to the model.
fn final_turn_choice(max_turns: u16, turn: u16) -> Option<ToolChoice> {
    (turn >= max_turns.saturating_sub(1))
        .then(|| ToolChoice::Specific(SUBMIT_ANSWER_TOOL.to_owned()))
}

/// Produces a typed result when a model exhausts its turns. Proposed edits
/// are retained only when the normal exact-edit check says they can still be
/// applied (or are already satisfied); direct edits made by `run_python`
/// remain in the workspace and need no second application here.
fn exhausted_step_answer(
    kind: ModelStepOutputKind,
    submitted: Option<ModelStepArtifact>,
    workspace: &WorkspaceView<'_>,
) -> ModelStepArtifact {
    match (kind, submitted) {
        (ModelStepOutputKind::PatchIntent, Some(ModelStepArtifact::PatchIntent { edits, .. })) => {
            ModelStepArtifact::PatchIntent {
                edits: edits
                    .into_iter()
                    .filter(|edit| {
                        verify_exact_edit(
                            workspace.config(),
                            edit.file(),
                            edit.find(),
                            edit.replace(),
                        )
                        .is_ok()
                    })
                    .collect(),
                check: None,
            }
        }
        (ModelStepOutputKind::PatchIntent, _) => ModelStepArtifact::PatchIntent {
            edits: Vec::new(),
            check: None,
        },
        (_, Some(artifact)) => artifact,
        (ModelStepOutputKind::Plan, None) => ModelStepArtifact::Plan(
            "The model did not submit a plan within its turn limit.".to_owned(),
        ),
        (ModelStepOutputKind::Findings, None) => ModelStepArtifact::Findings(Vec::new()),
        (ModelStepOutputKind::Review, None) => ModelStepArtifact::Review {
            verdict: Status::Indeterminate,
            notes: "The model did not submit a review within its turn limit.".to_owned(),
        },
        (ModelStepOutputKind::Answer, None) => ModelStepArtifact::Answer(
            "The model did not submit an answer within its turn limit.".to_owned(),
        ),
    }
}

/// Extra turns granted on top of a step's own [`ModelStepConfig::max_turns`]
/// when [`ModelStepConfig::review_before_submit`] is on, so a step that
/// spends its one review round still has turns left to act on it and
/// resubmit — see [`LiveModelStep::run`].
pub(crate) const REVIEW_TURN_ALLOWANCE: u16 = 4;

/// The one-time notice [`LiveModelStep::run`] sends back as the tool result
/// for a step's first accepted `submit_answer` call, in place of ending the
/// step, when [`ModelStepConfig::review_before_submit`] is on. Kept short:
/// every turn resends the whole conversation. `run_python_allowed` drops the
/// last point when the step was never offered that tool, rather than asking
/// for a check it has no way to run.
///
/// Points (4) and (5) target two miss shapes distinct from an incomplete
/// producer/consumer path (already point 2/3's target): a patch that
/// silently removes or disables behavior the task never asked to change
/// (for example, replacing a live branch with `if False:`), and a
/// producer/consumer pair (a writer and its reader, or a serializer and its
/// parser) whose two sides were edited to disagree.
fn review_before_submit_notice(run_python_allowed: bool) -> String {
    let mut notice = String::from(
        "Before this is accepted, reply once more: (1) name the failing operation from the \
         task statement; (2) name the code path that produces and consumes the changed \
         value, and every named output, variant, mode, or downstream consumer (for example \
         a parser for a writer) the statement mentions; (3) say which of those your patch \
         covers, and for any it does not, read that code now and extend the patch; (4) confirm \
         the patch removes or disables no behavior the task did not ask to change; (5) if a \
         value is produced and consumed by two sides (a writer and its reader, or a serializer \
         and its parser), confirm both sides still agree.",
    );
    if run_python_allowed {
        notice.push_str(
            " (6) Run one run_python check that shows the full statement behavior after the \
             patch.",
        );
    }
    notice.push_str(" Then call submit_answer again.");
    notice
}

/// Checks a proposed artifact before it is accepted as a step's final
/// answer. Only a [`ModelStepArtifact::PatchIntent`] has anything to check:
/// each edit must resolve against the real workspace exactly the way
/// applying it later would, via [`verify_exact_edit`] — the same
/// exact-match decision [`apply_patch_intent`] itself uses, so this check
/// and the eventual apply can never disagree.
fn validate_proposed_artifact(
    artifact: &ModelStepArtifact,
    workspace: &WorkspaceView<'_>,
) -> Result<(), String> {
    let ModelStepArtifact::PatchIntent { edits, .. } = artifact else {
        return Ok(());
    };
    for edit in edits {
        verify_exact_edit(workspace.config(), edit.file(), edit.find(), edit.replace())
            .map_err(|error| format!("edit to '{}' is invalid: {error}", edit.file()))?;
    }
    Ok(())
}

/// A provider-backed [`ModelStepExecutor`]: one bounded, tool-restricted,
/// budgeted turn against a real model.
#[derive(Clone)]
pub struct LiveModelStep {
    llm: Llm,
    models: HashMap<String, Llm>,
    model_id: Option<String>,
    cancel: RunCancellation,
    transcript: Option<PathBuf>,
    shell: Option<polaris::shell::ShellExecutor>,
    navigation: Option<crate::tools::code_graph::CodeNavigation>,
    handoff: Option<ModelStepHandoff>,
}

impl LiveModelStep {
    /// Wraps an LLM handle as a live model step executor.
    ///
    /// Not interruptible on its own — [`with_cancellation`](Self::with_cancellation)
    /// attaches the run's actual signal. Without it, a provider or tool await
    /// inside [`run`](Self::run) runs to completion regardless of the host's
    /// own cancellation state.
    #[must_use]
    pub fn new(llm: Llm) -> Self {
        Self {
            llm,
            models: HashMap::new(),
            model_id: None,
            cancel: RunCancellation::never(),
            transcript: None,
            shell: None,
            navigation: None,
            handoff: None,
        }
    }

    /// Registers an alternate model handle for a step with `model_id`.
    #[must_use]
    pub fn with_model(mut self, model_id: impl Into<String>, llm: Llm) -> Self {
        self.models.insert(model_id.into(), llm);
        self
    }

    /// Attach the full provider/model identity used for per-task pricing.
    #[must_use]
    pub fn with_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Some(model_id.into());
        self
    }

    /// Attaches the host's real shell executor, enabling the `run_python`
    /// tool for a step whose [`ModelStepConfig::tool_subset`] names it.
    /// Without this, `run_python` is refused as unavailable rather than
    /// silently granted through some other path — see
    /// [`run_python_script`]'s fail-closed rationale.
    #[must_use]
    pub fn with_shell(mut self, shell: polaris::shell::ShellExecutor) -> Self {
        self.shell = Some(shell);
        self
    }

    pub(crate) fn with_handoff(mut self, handoff: ModelStepHandoff) -> Self {
        self.handoff = Some(handoff);
        self
    }

    pub(crate) fn with_navigation(
        mut self,
        navigation: crate::tools::code_graph::CodeNavigation,
    ) -> Self {
        self.navigation = Some(navigation);
        self
    }

    /// Appends one JSON line per model call to `path`: the step's role, the
    /// turn within the step, the messages sent, the reply and its usage.
    ///
    /// Written as each call returns, not when the step ends, so a run that is
    /// killed or times out still leaves every call it made on disk. A write
    /// that fails is logged and the step carries on: the transcript is a
    /// record of the work, never a reason to stop it.
    #[must_use]
    pub fn with_transcript(mut self, path: impl Into<PathBuf>) -> Self {
        self.transcript = Some(path.into());
        self
    }

    fn record_call(
        &self,
        role: &str,
        turn: u16,
        sent: &[Message],
        response: &polaris::models::llm::LlmResponse,
    ) {
        let Some(path) = &self.transcript else {
            return;
        };
        let line = serde_json::json!({
            "role": role,
            "turn": turn,
            "sent": sent,
            "reply": response.content,
            "usage": {
                "input_tokens": response.usage.input_tokens,
                "output_tokens": response.usage.output_tokens,
                "cache_read_tokens": response.usage.cache_read_tokens,
                "cache_creation_tokens": response.usage.cache_creation_tokens,
            },
        });
        let written = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{line}")
        })();
        if let Err(error) = written {
            tracing::warn!(path = %path.display(), %error, "could not record a model step call");
        }
    }

    /// Attaches the run's cancellation signal, so a provider call or tool
    /// read this step is waiting on stops promptly instead of running to
    /// completion after the host has already given up on the turn.
    #[must_use]
    pub fn with_cancellation(mut self, cancel: RunCancellation) -> Self {
        self.cancel = cancel;
        self
    }

    /// `turn`, `seen_reads` and `review_used` are always local to this one
    /// call: a resumed (repair) call gets its own fresh `0..effective_max_turns`
    /// budget, its own empty dedup set and its own unused review, exactly as
    /// a first call does, so [`turn_notice`]'s closing and midpoint notices —
    /// and the last-turn `submit_answer` requirement — apply to a repair
    /// round's own turns, not the cumulative count across every round
    /// resumed so far. When [`ModelStepConfig::review_before_submit`] is on,
    /// `effective_max_turns` adds [`REVIEW_TURN_ALLOWANCE`] to
    /// `config.max_turns` and the step's first accepted `submit_answer` call
    /// is answered with [`review_before_submit_notice`] instead of ending
    /// the step, unless that call already landed on the true last turn, in
    /// which case there is no turn left to spend on a review and it is
    /// accepted directly. An invalid submission on that last turn gets one
    /// extra forced submission with its error in the conversation. Exhaustion
    /// returns a typed fallback, retaining any usable submitted edits while
    /// direct workspace edits remain in place.
    async fn run(
        &self,
        config: &ModelStepConfig,
        workspace: &WorkspaceView<'_>,
        meter: &TokenMeter,
        resume: Option<&ModelConversation>,
    ) -> Result<(ModelStepArtifact, ModelConversation), SystemError> {
        let allowed = |name: &str| {
            matches!(
                &config.tool_subset,
                ToolSubset::Only(names) if names.iter().any(|allowed| allowed == name)
            )
        };
        let read_file_allowed = allowed(READ_FILE_TOOL);
        let list_dir_allowed = allowed(LIST_DIR_TOOL);
        let search_allowed = allowed(SEARCH_TOOL);
        let run_python_allowed = allowed(RUN_PYTHON_TOOL);
        let edit_file_allowed = allowed(EDIT_FILE_TOOL);
        let run_command_allowed =
            allowed(RUN_COMMAND_TOOL) && edit_file_allowed && self.shell.is_some();
        let mut tool_definitions = Vec::new();
        if read_file_allowed {
            tool_definitions.push(read_file_tool_definition());
        }
        if list_dir_allowed {
            tool_definitions.push(list_dir_tool_definition());
        }
        if search_allowed {
            tool_definitions.push(search_tool_definition());
        }
        if run_python_allowed {
            tool_definitions.push(run_python_tool_definition());
        }
        if run_command_allowed {
            tool_definitions.push(run_command_tool_definition());
        }
        if edit_file_allowed {
            tool_definitions.push(edit_file_tool_definition());
        }
        let navigation = self
            .navigation
            .as_ref()
            .filter(|_| allowed("find_definition") || allowed("find_references"))
            .filter(|n| n.available_for(&workspace.config().working_dir));
        for (name, description) in [
            (
                "find_definition",
                "Definition locations by unqualified symbol name (syntactic index).",
            ),
            (
                "find_references",
                "Syntactic callers by function name; incomplete for dynamic dispatch or non-call uses. Use search for those.",
            ),
        ] {
            if allowed(name) && navigation.is_some() {
                tool_definitions.push(ToolDefinition::new(
                    name,
                    description,
                    serde_json::json!({
                        "type": "object", "properties": {"name": {"type": "string"}},
                        "required": ["name"], "additionalProperties": false,
                    }),
                ));
            }
        }
        // Always offered, independent of `tool_subset`: this is the one way
        // a turn ends, so even a step with no exploration tools at all must
        // be able to call it.
        tool_definitions.push(submit_answer_tool_definition(config.output));
        let twm_note_allowed = config.twm.as_ref().is_some_and(|options| options.note);
        if twm_note_allowed {
            tool_definitions.push(twm::note_tool_definition());
        }
        // Named back to the model when it calls a tool this step does not
        // offer, so it can retry with a real one instead of ending the run.
        let offered_tools = tool_definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let tools = Some(tool_definitions);
        let resume_boundary = resume.map_or(0, |conversation| conversation.0.len());
        let mut messages = match resume {
            Some(conversation) => {
                let mut messages = conversation.0.clone();
                if config.autotest {
                    compact_prior_autotests(&mut messages);
                }
                messages.push(Message::user(config.instruction.clone()));
                messages
            }
            None => {
                let mut first = format!(
                    "Role: {}\nInstruction: {}\n{}",
                    config.role,
                    config.instruction,
                    output_directions(config.output)
                );
                if list_dir_allowed && repo_layout_enabled() {
                    first.push_str(&repo_layout(&workspace.config().working_dir));
                }
                if run_python_hint_enabled()
                    && let Some(hint) =
                        super::autotest::runner_hint(&workspace.config().working_dir)
                {
                    first.push_str(&hint);
                }
                vec![Message::user(first)]
            }
        };
        // Every exact-duplicate `read_file(path, start_line, max_lines)` call
        // this step has already answered — a rerun of the same call gets a
        // short note instead of the same file text again, since a read-only
        // step's own workspace cannot have changed underneath it between two
        // of its own turns.
        let mut seen_reads: HashSet<(String, Option<usize>, Option<usize>)> = HashSet::new();
        // Typed working memory (see `twm`):
        // replaces this step's growing transcript with a small set of typed
        // slots, rebuilt at a whole-prompt size limit instead of trimmed in place.
        // `None` for every plan that does not opt in — see `ModelStepConfig::twm`.
        let mut twm_state = config.twm.map(|options| {
            let codemap = options
                .codemap
                .then(|| twm::build_codemap(&workspace.config().working_dir));
            twm::TwmState::new(options, &config.instruction, codemap)
        });
        let effective_max_turns = if config.review_before_submit {
            config.max_turns.saturating_add(REVIEW_TURN_ALLOWANCE)
        } else {
            config.max_turns
        };
        let mut review_used = false;
        let submit_guard = submit_guard_enabled();
        let mut guard_uses: u8 = 0;
        let mut last_check_failed = false;
        let mut repair_last_submission = false;
        let mut last_submitted = None;
        let mut last_trim = None;
        for turn in 0..=effective_max_turns {
            if turn == effective_max_turns && (config.strict_turn_cap || !repair_last_submission) {
                break;
            }
            // Relieve routine token pressure by stubbing the oldest tool
            // results in place, the same jump-compaction the main agent loop
            // applies to its own cache-aware window — reused directly rather
            // than reimplemented, so a step whose exploration runs long keeps
            // a stable, cacheable prefix instead of resending an ever-growing
            // history at full price every turn.
            // A resumed step's old transcript sits before its new instruction.
            // Keep the current step plus the prior submit result intact, while
            // letting older results compact even when the handoff has fewer
            // than the usual twelve recent messages.
            let keep_recent = if resume_boundary == 0 {
                DEFAULT_COMPACT_KEEP_RECENT
            } else {
                messages
                    .len()
                    .saturating_sub(resume_boundary)
                    .saturating_add(1)
                    .min(DEFAULT_COMPACT_KEEP_RECENT)
            };
            if let Some(twm) = twm_state.as_mut() {
                // Typed working memory replaces both the jump-compactor and
                // the batched trim for this step: it observes new tool
                // calls/results since the last turn, then rebuilds the whole
                // prompt from typed slots (never edits old text in place)
                // once the whole prompt passes the size limit. Between rebuilds the
                // conversation stays exactly as append-only as it is today.
                twm.observe(&messages, turn, effective_max_turns);
                twm.age_out(turn);
            } else {
                if config.compact_history && config.trim == TrimPolicy::None {
                    compact_tool_results(
                        &mut messages,
                        Some(MODEL_STEP_COMPACT_AFTER_TOKENS.min(config.budget.0 as usize)),
                        DEFAULT_COMPACT_BATCH,
                        keep_recent,
                        MODEL_STEP_TOKEN_RATIO,
                    );
                }
                if config.trim == TrimPolicy::Batched
                    && trim_old_tool_results(
                        &mut messages,
                        turn,
                        effective_max_turns,
                        &mut last_trim,
                    ) > 0
                {
                    seen_reads.clear();
                }
            }
            let llm = match &config.model_id {
                Some(id) => self.models.get(id).ok_or_else(|| {
                    exec(format!(
                        "model step '{}' has no registered model '{id}'",
                        config.role
                    ))
                })?,
                None => &self.llm,
            };
            let cost_capped = meter.cost_cap_enabled();
            if cost_capped && meter.take_cost_notice() {
                messages.push(Message::user("Budget nearly used: finish and submit."));
            }
            let max_output = if cost_capped {
                config.budget.0.min(COST_CAPPED_MAX_OUTPUT_TOKENS)
            } else {
                config.budget.0
            };
            let mut request = LlmRequest {
                messages: messages.clone(),
                tools: tools.clone(),
                // Validation rejects a zero budget, so `new` never returns
                // `None`; the filter drops the limit of an unbounded, uncapped step.
                max_output_tokens: NonZeroU32::new(max_output)
                    .filter(|_| cost_capped || config.budget.0 != u32::MAX),
                tool_choice: if config.stable_tool_choice {
                    None
                } else {
                    final_turn_choice(effective_max_turns, turn)
                },
                cache: CacheControl::prefix().with_breakpoints(cache_breakpoints(&messages)),
                ..Default::default()
            };
            if let Some(twm) = twm_state.as_mut()
                && twm.should_rebuild(&request)
            {
                tracing::debug!(
                    turn,
                    messages_before = messages.len(),
                    "Rebuilding typed working memory at prompt size limit"
                );
                messages = twm.rebuild(&messages);
                request.messages = messages.clone();
                request.cache =
                    CacheControl::prefix().with_breakpoints(cache_breakpoints(&messages));
                seen_reads.clear();
            }
            let permit = if cost_capped {
                let model_id = config
                    .model_id
                    .as_ref()
                    .or(self.model_id.as_ref())
                    .ok_or_else(|| exec("cost-cap: model identity unavailable"))?;
                meter
                    .reserve_request(model_id, &request, max_output)
                    .map_err(exec)?
            } else {
                None
            };
            meter.record_model_turn();
            let response = cancellable(&self.cancel, &config.role, llm.generate(request))
                .await?
                .map_err(exec)?;
            if let Some(permit) = permit {
                permit.settle(&response.usage);
            }
            self.record_call(&config.role, turn, &messages, &response);
            let input = response.usage.input_tokens.unwrap_or_default();
            let output = response.usage.output_tokens.unwrap_or_default();
            let cache_read = response.usage.cache_read_tokens.unwrap_or_default();
            let cache_creation = response.usage.cache_creation_tokens.unwrap_or_default();
            // Billed unconditionally, before the budget check below runs: a
            // refusal is still the auditable record of a real call rather
            // than a discard of one the session ledger already charged.
            if let Some(id) = &config.model_id {
                meter.add_for_model(id, input, output, cache_read, cache_creation);
            } else {
                meter.add(input, output);
                meter.add_cache(cache_read, cache_creation);
            }
            // This turn's own full context — everything the provider billed
            // for it, prompt and cache tiers alike, plus its output — is what
            // `TokenBudget` bounds; a step whose earlier turns were each
            // small stays inside budget no matter how many of them it takes,
            // since `max_turns` is what bounds total work.
            let turn_total = input + output + cache_read + cache_creation;
            if turn_total > u64::from(config.budget.0) {
                return Err(exec(format!(
                    "model step '{}' exceeded its token budget ({turn_total} > {})",
                    config.role, config.budget.0
                )));
            }
            messages.push(Message::Assistant {
                id: None,
                content: response.content.clone(),
            });
            if response.has_tool_calls() {
                let mut results = Vec::new();
                let mut edited_paths = Vec::new();
                let mut last_write_result = None;
                let mut finished: Option<ModelStepArtifact> = None;
                let mut review_requested_this_turn = false;
                let mut invalid_submission = false;
                let is_last_turn = turn >= effective_max_turns.saturating_sub(1);
                for tool_call in response.tool_calls() {
                    let tracks_shell = config.autotest
                        && edit_file_allowed
                        && matches!(
                            tool_call.function.name.as_str(),
                            RUN_COMMAND_TOOL | RUN_PYTHON_TOOL
                        );
                    let before = if tracks_shell {
                        super::autotest::snapshot(&workspace.config().working_dir).await
                    } else {
                        None
                    };
                    let outcome: Result<String, String> = match tool_call.function.name.as_str() {
                        SUBMIT_ANSWER_TOOL => {
                            let decoded = decode_model_step_answer_value(
                                tool_call.function.arguments.clone(),
                                config.output,
                            )
                            .map_err(|error| error.to_string());
                            let validated = decoded.and_then(|artifact| {
                                last_submitted = Some(artifact.clone());
                                validate_proposed_artifact(&artifact, workspace).map(|()| artifact)
                            });
                            if validated.is_err() {
                                invalid_submission = true;
                            }
                            let guard_notice = (submit_guard
                                && validated.is_ok()
                                && edit_file_allowed
                                && !is_last_turn
                                && guard_uses < SUBMIT_GUARD_MAX_USES)
                                .then(|| {
                                    submit_guard_notice(
                                        workspace_is_unchanged(&workspace.config().working_dir),
                                        last_check_failed,
                                    )
                                })
                                .flatten();
                            if guard_notice.is_some() {
                                guard_uses += 1;
                                review_requested_this_turn = true;
                            }
                            validated.map(|artifact| {
                                if let Some(notice) = guard_notice {
                                    notice
                                } else if config.review_before_submit
                                    && !review_used
                                    && !is_last_turn
                                {
                                    review_used = true;
                                    review_requested_this_turn = true;
                                    review_before_submit_notice(run_python_allowed)
                                } else {
                                    finished = Some(artifact);
                                    "answer accepted".to_owned()
                                }
                            })
                        }
                        READ_FILE_TOOL if read_file_allowed => {
                            match tool_call
                                .function
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                            {
                                Some(path) => {
                                    let start_line = tool_call
                                        .function
                                        .arguments
                                        .get("start_line")
                                        .and_then(serde_json::Value::as_u64)
                                        .map(|n| n as usize);
                                    let max_lines = tool_call
                                        .function
                                        .arguments
                                        .get("max_lines")
                                        .and_then(serde_json::Value::as_u64)
                                        .map(|n| n as usize);
                                    let key = (path.to_owned(), start_line, max_lines);
                                    if seen_reads.contains(&key) {
                                        Ok(format!(
                                            "Already read {} in this step; reuse that earlier \
                                             result instead of re-reading it.",
                                            describe_read(path, start_line, max_lines)
                                        ))
                                    } else {
                                        let outcome = cancellable(
                                            &self.cancel,
                                            &config.role,
                                            read_file_bounded(
                                                workspace.config(),
                                                path,
                                                start_line,
                                                max_lines,
                                            ),
                                        )
                                        .await?
                                        .map_err(|error| error.to_string());
                                        if outcome.is_ok() {
                                            seen_reads.insert(key);
                                        }
                                        outcome
                                    }
                                }
                                None => {
                                    Err("read_file requires a 'path' string argument".to_owned())
                                }
                            }
                        }
                        LIST_DIR_TOOL if list_dir_allowed => {
                            match tool_call
                                .function
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                            {
                                Some(path) => {
                                    cancellable(
                                        &self.cancel,
                                        &config.role,
                                        list_dir_bounded(workspace.config(), path),
                                    )
                                    .await?
                                }
                                None => {
                                    Err("list_dir requires a 'path' string argument".to_owned())
                                }
                            }
                        }
                        SEARCH_TOOL if search_allowed => {
                            match tool_call
                                .function
                                .arguments
                                .get("pattern")
                                .and_then(serde_json::Value::as_str)
                            {
                                Some(pattern) => {
                                    let path = tool_call
                                        .function
                                        .arguments
                                        .get("path")
                                        .and_then(serde_json::Value::as_str);
                                    let literal = tool_call
                                        .function
                                        .arguments
                                        .get("literal")
                                        .and_then(serde_json::Value::as_bool)
                                        .unwrap_or(false);
                                    let case_insensitive = tool_call
                                        .function
                                        .arguments
                                        .get("case_insensitive")
                                        .and_then(serde_json::Value::as_bool)
                                        .unwrap_or(false);
                                    let context_lines = tool_call
                                        .function
                                        .arguments
                                        .get("context_lines")
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0)
                                        as usize;
                                    let files_only = tool_call
                                        .function
                                        .arguments
                                        .get("files_only")
                                        .and_then(serde_json::Value::as_bool)
                                        .unwrap_or(false);
                                    cancellable(
                                        &self.cancel,
                                        &config.role,
                                        search_bounded(
                                            workspace.config(),
                                            pattern,
                                            path,
                                            literal,
                                            case_insensitive,
                                            context_lines,
                                            files_only,
                                        ),
                                    )
                                    .await?
                                }
                                None => {
                                    Err("search requires a 'pattern' string argument".to_owned())
                                }
                            }
                        }
                        RUN_COMMAND_TOOL if run_command_allowed => {
                            // Shell commands can change any file, invalidating read dedup.
                            seen_reads.clear();
                            match tool_call
                                .function
                                .arguments
                                .get("command")
                                .and_then(serde_json::Value::as_str)
                            {
                                Some(command) => {
                                    cancellable(
                                        &self.cancel,
                                        &config.role,
                                        super::command::run_workspace_command(
                                            self.shell.as_ref(),
                                            workspace.config().working_dir.as_path(),
                                            command,
                                        ),
                                    )
                                    .await?
                                }
                                None => {
                                    Err("run_command requires a 'command' string argument"
                                        .to_owned())
                                }
                            }
                        }
                        RUN_PYTHON_TOOL if run_python_allowed => {
                            match tool_call
                                .function
                                .arguments
                                .get("script")
                                .and_then(serde_json::Value::as_str)
                            {
                                Some(script) => cancellable(
                                    &self.cancel,
                                    &config.role,
                                    run_python_script(
                                        self.shell.as_ref(),
                                        workspace.config().working_dir.as_path(),
                                        PYTHON_SCRIPT_PATH,
                                        script,
                                    ),
                                )
                                .await?
                                .map(|outcome| {
                                    last_check_failed = !outcome.success;
                                    outcome.rendered
                                }),
                                None => {
                                    Err("run_python requires a 'script' string argument".to_owned())
                                }
                            }
                        }
                        name @ ("find_definition" | "find_references")
                            if allowed(name) && navigation.is_some() =>
                        {
                            match tool_call
                                .function
                                .arguments
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                            {
                                Some(symbol) => {
                                    cancellable(
                                        &self.cancel,
                                        &config.role,
                                        navigation
                                            .expect("guarded above")
                                            .query(name, symbol.to_owned()),
                                    )
                                    .await?
                                }
                                None => Err("navigation requires a name string".to_owned()),
                            }
                        }
                        EDIT_FILE_TOOL if edit_file_allowed => {
                            match (
                                tool_call
                                    .function
                                    .arguments
                                    .get("path")
                                    .and_then(serde_json::Value::as_str),
                                tool_call
                                    .function
                                    .arguments
                                    .get("old_string")
                                    .and_then(serde_json::Value::as_str),
                                tool_call
                                    .function
                                    .arguments
                                    .get("new_string")
                                    .and_then(serde_json::Value::as_str),
                            ) {
                                (Some(path), Some(old_string), Some(new_string)) => {
                                    let outcome = edit_file_bounded(
                                        workspace.config(),
                                        path,
                                        old_string,
                                        new_string,
                                    );
                                    outcome.map(|(content, wrote)| {
                                        if config.autotest && wrote {
                                            edited_paths.push(path.to_owned());
                                            last_write_result = Some(results.len());
                                            seen_reads.clear();
                                        }
                                        content
                                    })
                                }
                                _ => Err("edit_file requires 'path', 'old_string' and \
                                          'new_string' string arguments"
                                    .to_owned()),
                            }
                        }
                        twm::NOTE_TOOL if twm_note_allowed => {
                            match tool_call
                                .function
                                .arguments
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                            {
                                Some(_) => Ok("noted".to_owned()),
                                None => Err("note requires a 'text' string argument".to_owned()),
                            }
                        }
                        // Refused but not fatal: models trained on other harnesses
                        // sometimes call their own tool names (`bash`,
                        // `str_replace_based_edit_tool`). The call is not run; the
                        // model is told which tools exist and keeps its turn budget.
                        other => Err(format!(
                            "disallowed tool '{other}': this step offers only {offered_tools}"
                        )),
                    };
                    if outcome.is_ok()
                        && let Some(before) = before
                        && let Some(after) =
                            super::autotest::snapshot(&workspace.config().working_dir).await
                    {
                        let changed: Vec<_> = after
                            .into_iter()
                            .filter(|(p, stamp)| before.get(p) != Some(stamp))
                            .map(|(p, _)| p)
                            .collect();
                        if !changed.is_empty() {
                            edited_paths.extend(changed);
                            last_write_result = Some(results.len());
                        }
                    }
                    results.push(match outcome {
                        Ok(content) => UserBlock::tool_result(
                            tool_call.id.clone(),
                            ToolResultContent::Text(content),
                        ),
                        Err(error) => UserBlock::tool_error(
                            tool_call.id.clone(),
                            ToolResultContent::Text(error),
                        ),
                    });
                }
                if let Some(index) = last_write_result {
                    let cancel = self.cancel.clone();
                    let shell_cancel = polaris::shell::ShellCancel::from_fn(move || {
                        let cancel = cancel.clone();
                        async move { cancel.cancelled().await }
                    });
                    let before_signature =
                        super::cascade::tree_signature(&workspace.config().working_dir).ok();
                    if let Some(autotest) = cancellable(
                        &self.cancel,
                        &config.role,
                        super::autotest::run(
                            self.shell.as_ref(),
                            &workspace.config().working_dir,
                            &edited_paths,
                            shell_cancel,
                        ),
                    )
                    .await?
                    {
                        let after_signature =
                            super::cascade::tree_signature(&workspace.config().working_dir).ok();
                        if let Some(handoff) = &self.handoff {
                            handoff.set_autotest(match (before_signature, after_signature) {
                                (Some(before), Some(after))
                                    if before == after && autotest.passed && autotest.pytest =>
                                {
                                    Some(AutotestEvidence {
                                        targets: autotest.targets,
                                        tree_signature: after,
                                    })
                                }
                                _ => None,
                            });
                        }
                        if let UserBlock::ToolResult(result) = &mut results[index]
                            && let ToolResultContent::Text(content) = &mut result.content
                        {
                            content.push_str(&autotest.summary);
                            seen_reads.clear();
                        }
                    } else if let Some(handoff) = &self.handoff {
                        handoff.set_autotest(None);
                    }
                }
                if let Some(artifact) = finished {
                    // Keep the tool result answering this submit_answer call:
                    // a tool call with no matching result is not a
                    // conversation a later step can validly resume.
                    messages.push(Message::User { content: results });
                    return Ok((artifact, ModelConversation(messages)));
                }
                if is_last_turn && turn < effective_max_turns && invalid_submission {
                    repair_last_submission = true;
                }
                // Skip the ordinary turn notice on the one turn the review
                // notice fired: the review already tells the model exactly
                // what to do next, and a "turns remain, submit now" notice
                // stacked onto it would contradict its ask to keep working.
                if turn < effective_max_turns
                    && !review_requested_this_turn
                    && let Some(notice) = turn_notice(effective_max_turns, turn)
                {
                    results.push(UserBlock::text(notice));
                }
                messages.push(Message::User { content: results });
                continue;
            }
            // No tool call at all: a lenient fallback, not the primary path.
            // Accepted only if the reply, once a code fence and any
            // surrounding prose are stripped, still decodes as a valid
            // answer for this step's configured kind — a patch-intent
            // answer is checked exactly as a `submit_answer` one is, via
            // `validate_proposed_artifact`. Anything else is a reminder to
            // call `submit_answer`, not a refusal: it costs a turn, not the
            // whole step.
            let raw = response.text();
            if let Some(artifact) = extract_json_object(&raw)
                .and_then(|json| decode_model_step_answer(json, config.output).ok())
            {
                last_submitted = Some(artifact.clone());
                match validate_proposed_artifact(&artifact, workspace) {
                    Ok(()) => return Ok((artifact, ModelConversation(messages))),
                    Err(reason) => {
                        let mut reminder =
                            format!("{reason} Call submit_answer with a corrected answer.");
                        if turn < effective_max_turns
                            && let Some(notice) = turn_notice(effective_max_turns, turn)
                        {
                            reminder.push(' ');
                            reminder.push_str(&notice);
                        }
                        messages.push(Message::user(reminder));
                        continue;
                    }
                }
            }
            let mut reminder = format!(
                "That reply was not a submit_answer call. {}",
                output_directions(config.output)
            );
            if turn < effective_max_turns
                && let Some(notice) = turn_notice(effective_max_turns, turn)
            {
                reminder.push(' ');
                reminder.push_str(&notice);
            }
            messages.push(Message::user(reminder));
        }
        tracing::warn!(role = %config.role, turns = effective_max_turns, "model step exhausted its turns; returning a fallback answer");
        Ok((
            exhausted_step_answer(config.output, last_submitted, workspace),
            ModelConversation(messages),
        ))
    }
}

impl ModelStepExecutor for LiveModelStep {
    fn propose<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        workspace: &'a WorkspaceView<'a>,
        meter: &'a TokenMeter,
    ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>> {
        Box::pin(async move {
            self.run(config, workspace, meter, None)
                .await
                .map(|(artifact, _)| artifact)
        })
    }

    fn propose_conversation<'a>(
        &'a self,
        config: &'a ModelStepConfig,
        workspace: &'a WorkspaceView<'a>,
        meter: &'a TokenMeter,
        resume: Option<&'a ModelConversation>,
    ) -> ProposeConversationFuture<'a> {
        Box::pin(async move { self.run(config, workspace, meter, resume).await })
    }
}

pub(super) async fn run_with_handoff_conversation(
    config: &ModelStepConfig,
    provider: &ModelStepProvider,
    workspace: &WorkspaceView<'_>,
    meter: &TokenMeter,
    handoff: &ModelStepHandoff,
) -> Result<(ModelStepArtifact, ModelConversation), SystemError> {
    let mut instruction = config.clone();
    let prior = handoff.snapshot();
    let resume = match config.context_mode {
        ContextMode::Fresh => None,
        ContextMode::Brief => {
            if !prior.records.is_empty() {
                instruction.instruction = format!(
                    "{}\n\n{}",
                    instruction.instruction,
                    ModelStepHandoff::brief_records(&prior.records)
                );
            }
            None
        }
        ContextMode::Continue => prior.conversation.as_ref(),
    };
    let previous_messages = resume.map_or(0, ModelConversation::len);
    if config.context_mode == ContextMode::Continue
        && prior.last_output.is_some_and(|kind| kind != config.output)
    {
        instruction.instruction.push_str("\n\n");
        instruction
            .instruction
            .push_str(output_directions(config.output));
    }
    let (artifact, conversation) = provider
        .propose_conversation(&instruction, workspace, meter, resume)
        .await?;
    handoff.record(config, &artifact, conversation.clone(), previous_messages);
    Ok((artifact, conversation))
}

async fn run_with_handoff(
    config: &ModelStepConfig,
    provider: &ModelStepProvider,
    workspace: &WorkspaceView<'_>,
    meter: &TokenMeter,
    handoff: &ModelStepHandoff,
) -> Result<ModelStepArtifact, SystemError> {
    Ok(
        run_with_handoff_conversation(config, provider, workspace, meter, handoff)
            .await?
            .0,
    )
}

#[system]
async fn model_propose(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<ModelStepArtifact, SystemError> {
    let view = WorkspaceView::new(&workspace);
    run_with_handoff(&config, &provider, &view, &meter, &handoff).await
}

#[system]
async fn model_propose_patch_intent(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<PatchIntent, SystemError> {
    let view = WorkspaceView::new(&workspace);
    let artifact = run_with_handoff(&config, &provider, &view, &meter, &handoff).await?;
    let ModelStepArtifact::PatchIntent { edits, check } = artifact else {
        return Err(exec(
            "a model-propose block configured for patch-intent returned another artifact kind",
        ));
    };
    Ok(PatchIntent { edits, check })
}

/// Folds a preceding test run's captured failure into a model step's own
/// instruction, so a retry actually sees why the last attempt did not pass
/// rather than being told only, statically, to try again.
pub(super) fn with_prior_failure(
    config: &ModelStepConfig,
    outcome: &TestRunOutcome,
) -> ModelStepConfig {
    let mut augmented = config.clone();
    augmented.instruction = if outcome.output.trim().is_empty() {
        format!(
            "{}\n\nThe previous attempt's test run did not pass (status {:?}); no output was \
             captured.",
            augmented.instruction, outcome.status
        )
    } else {
        format!(
            "{}\n\nThe previous attempt's test run did not pass (status {:?}) and produced this \
             output:\n{}",
            augmented.instruction, outcome.status, outcome.output
        )
    };
    augmented.instruction.push_str(
        "\nThe current patch's edits are already applied; submit no edits to keep the current patch.",
    );
    augmented
}

/// Like [`model_propose_patch_intent`], but also reads a preceding
/// [`TestRunOutcome`] and folds its captured failure into the instruction
/// before calling the provider — what a repair loop's own retry steps use
/// ([`ModelStepConfig::with_prior_failure_visible`]), so the model sees why
/// the last attempt did not pass rather than being told only, statically, to
/// try again.
#[system]
async fn model_propose_patch_intent_with_feedback(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    outcome: Out<TestRunOutcome>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<PatchIntent, SystemError> {
    let augmented = with_prior_failure(&config, &outcome);
    let view = WorkspaceView::new(&workspace);
    let artifact = run_with_handoff(&augmented, &provider, &view, &meter, &handoff).await?;
    let ModelStepArtifact::PatchIntent { edits, check } = artifact else {
        return Err(exec(
            "a model-propose block configured for patch-intent returned another artifact kind",
        ));
    };
    Ok(PatchIntent { edits, check })
}

/// Runs one bounded model turn and returns its configured typed artifact.
///
/// A `patch-intent` configuration returns [`PatchIntent`]; every other
/// [`ModelStepOutputKind`] returns [`ModelStepArtifact`]. This output split
/// makes patch-application wires structurally typed. The decoded
/// [`ModelStepConfig`] remains seeded per instance. Proposing is read-only,
/// so the block may sit inside a `Fork` (e.g. a vote over several proposals).
///
/// A `patch-intent` configuration with
/// [`ModelStepConfig::sees_prior_failure`] set also reads an upstream
/// [`TestRunOutcome`], so the materializer refuses a plan naming that flag
/// with nothing producing one in scope, before execution rather than at run
/// time.
pub struct ModelProposeBlock;

impl BlockFactory for ModelProposeBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let decoded = ModelStepConfig::decode(config)?;
        if matches!(&decoded.tool_subset, ToolSubset::Only(tools) if tools.iter().any(|tool| tool == EDIT_FILE_TOOL || tool == RUN_COMMAND_TOOL))
        {
            return Err(BlockBuildError::new(
                "code.model-propose is read-only and cannot offer edit_file or run_command",
            ));
        }
        build_model_step(graph, decoded)
    }
}

fn build_model_step(
    graph: &mut Graph,
    decoded: ModelStepConfig,
) -> Result<NodeId, BlockBuildError> {
    let output = decoded.output;
    let sees_prior_failure = decoded.sees_prior_failure;
    graph.add_system_node(move || {
        let decoded = decoded.clone();
        async move { decoded }
    });
    Ok(match (output, sees_prior_failure) {
        (ModelStepOutputKind::PatchIntent, true) => {
            graph.add_system_node(model_propose_patch_intent_with_feedback)
        }
        (ModelStepOutputKind::PatchIntent, false) => {
            graph.add_system_node(model_propose_patch_intent)
        }
        (
            ModelStepOutputKind::Plan
            | ModelStepOutputKind::Findings
            | ModelStepOutputKind::Review
            | ModelStepOutputKind::Answer,
            _,
        ) => graph.add_system_node(model_propose),
    })
}

/// Returns [`ModelProposeBlock`]'s registered key.
#[must_use]
pub fn model_propose_key() -> CompositionBlockKey {
    block_key("code.model-propose")
}

/// Returns the configuration schema [`ModelProposeBlock`] declares.
#[must_use]
pub fn model_propose_schema() -> ConfigurationSchemaKey {
    schema_key("polaris-code.gate-blocks.model-propose-config")
}

/// A model step whose own turns may edit the working tree directly, through
/// [`EDIT_FILE_TOOL`], instead of only proposing a [`PatchIntent`] for a
/// separate block to apply afterward.
///
/// Uses the same model-step graph builder as [`ModelProposeBlock`], while
/// permitting `edit_file` only under this mutating registration. A plan
/// plan names this block's own `role`/`instruction`/`tool_subset`/`token_budget`
/// through the identical [`model_propose_schema`] configuration. The two
/// differ only in what the catalogue *registers*: `code.model-propose` is
/// [`Effect::ReadOnly`] — true of every plan that uses it today, all of which
/// only ever propose — while this block is registered
/// [`Effect::Mutating`], honestly, because naming [`EDIT_FILE_TOOL`] in its
/// `tool_subset` really does let its own turns write to the tree. Reusing
/// `code.model-propose`'s *key* for both roles would either lie about the
/// read-only ones or wall off the mutating one from `Fork`/`Repeat`
/// reasoning that assumes read-only means safe to run more than once in the
/// same shape — a second registration costs one key and keeps both claims
/// true.
///
/// Built for the `loop` topology, a single model conversation
/// ([`crate::cosmos::planner::loop_statement_only_plan_body`]): one model
/// conversation that edits the workspace itself, with nothing downstream to
/// judge or repair it. Not offered to the free-form model planner
/// ([`super::super::planner::nameable_gate_blocks`] excludes it by key): a
/// plan that could grant a real edit tool through the same JSON grammar an
/// ordinary read-only propose step accepts would let *any* gates-model turn
/// quietly acquire that capability, not just this one arm's own
/// hand-written plan.
pub struct ModelDirectEditBlock;

impl BlockFactory for ModelDirectEditBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let decoded = ModelStepConfig::decode(config)?;
        if !matches!(&decoded.tool_subset, ToolSubset::Only(tools) if tools.iter().any(|tool| tool == EDIT_FILE_TOOL))
        {
            return Err(BlockBuildError::new(
                "code.model-direct-edit requires edit_file in its tool subset",
            ));
        }
        build_model_step(graph, decoded)
    }
}

/// Returns [`ModelDirectEditBlock`]'s registered key.
#[must_use]
pub fn model_direct_edit_key() -> CompositionBlockKey {
    block_key("code.model-direct-edit")
}

// ---------------------------------------------------------------------------
// Applying a model-proposed patch.
// ---------------------------------------------------------------------------

#[system]
async fn apply_patch_intent(
    workspace: Res<CodingWorkspace>,
    patch: Out<PatchIntent>,
) -> Result<AppliedEdit, SystemError> {
    let mut outcomes = Vec::with_capacity(patch.edits.len());
    for edit in &patch.edits {
        outcomes.push(
            apply_exact_edit(workspace.config(), edit.file(), edit.find(), edit.replace())
                .map_err(exec)?,
        );
    }
    Ok(AppliedEdit { outcomes })
}

/// Applies a model-proposed patch intent's exact-string edits to the real
/// working tree.
///
/// Wraps a direct call to the same exact-edit applier
/// [`CodingWorkspace::apply`](super::super::payload::CodingWorkspace::apply)
/// uses: `CodingWorkspace, Out<PatchIntent> -> AppliedEdit`. Mutating
/// and not repeatable — though `apply_exact_edit` is itself idempotent (a
/// landed edit re-applies as [`EditOutcome::AlreadySatisfied`](super::super::edit::EditOutcome::AlreadySatisfied)),
/// so this block is declared conservatively, not as a positive repeatability
/// claim it has not been proven against every edit shape. A materializer
/// wiring it after a model step configured for a different
/// [`ModelStepOutputKind`] is refused by the graph's typed signature before
/// execution rather than becoming a run-time variant error.
pub struct ApplyPatchIntentBlock;

impl BlockFactory for ApplyPatchIntentBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(apply_patch_intent))
    }

    fn wire_input(
        &self,
        graph: &mut Graph,
        reader: NodeId,
        producer: NodeId,
        port: &StablePortKey,
    ) -> Result<(), BlockWireError> {
        if *port == patch_intent_port_key() {
            graph.read_from::<PatchIntent>(reader, producer);
            Ok(())
        } else {
            Err(BlockWireError::unsupported(port))
        }
    }
}

/// Returns [`ApplyPatchIntentBlock`]'s registered key.
#[must_use]
pub fn apply_patch_intent_key() -> CompositionBlockKey {
    block_key("code.apply-patch-intent")
}

/// Returns the stable input port that supplies a model patch intent to
/// [`ApplyPatchIntentBlock`].
///
/// # Panics
///
/// Never: the port ID is a compile-time valid stable name.
#[must_use]
pub fn patch_intent_port_key() -> StablePortKey {
    StablePortKey::new(
        StablePortId::new("code.patch-intent").expect("patch intent port ID is valid"),
        behavior_version(),
    )
}

// ---------------------------------------------------------------------------
// Creating a new file — the capability "add a test" needs that no existing
// block has: every other write here is an exact-string replacement of
// content that already exists.
// ---------------------------------------------------------------------------

/// What creating a new file produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedFile {
    /// The sandbox-relative path that was created.
    pub path: String,
}

/// The path and content one [`CreateFileBlock`] instance was configured with.
#[derive(Debug, Clone)]
struct NewFile {
    path: String,
    content: String,
}

#[system]
async fn create_new_file(
    workspace: Res<CodingWorkspace>,
    new_file: Out<NewFile>,
) -> Result<CreatedFile, SystemError> {
    super::super::edit::create_file(workspace.config(), &new_file.path, &new_file.content)
        .map_err(exec)?;
    Ok(CreatedFile {
        path: new_file.path.clone(),
    })
}

/// Writes a new sandbox-relative file, seeded per instance by its own
/// decoded path and content.
///
/// Wraps [`super::super::edit::create_file`]: `CodingWorkspace ->
/// CreatedFile`. Mutating and not repeatable — running the same instance
/// twice would find the path already there and refuse
/// ([`EditError::AlreadyExists`](super::super::edit::EditError::AlreadyExists))
/// rather than silently overwrite it, which is the opposite of repeat-safe.
pub struct CreateFileBlock;

impl BlockFactory for CreateFileBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let entries = inline_map(config)?;
        let path = as_str(field(entries, "path")?)?.to_owned();
        let content = as_str(field(entries, "content")?)?.to_owned();
        graph.add_system_node(move || {
            let new_file = NewFile {
                path: path.clone(),
                content: content.clone(),
            };
            async move { new_file }
        });
        Ok(graph.add_system_node(create_new_file))
    }
}

/// Returns [`CreateFileBlock`]'s registered key.
#[must_use]
pub fn create_file_key() -> CompositionBlockKey {
    block_key("code.create-file")
}

/// Returns the configuration schema [`CreateFileBlock`] declares: a map with
/// `path` and `content` string fields.
#[must_use]
pub fn create_file_schema() -> ConfigurationSchemaKey {
    schema_key("polaris-code.gate-blocks.create-file-config")
}

/// Encodes a `code.create-file` configuration value for a plan to reference.
///
/// # Errors
///
/// Returns an error only if the encoded value is rejected as malformed,
/// which does not happen for well-formed `path`/`content` strings.
pub fn create_file_config(
    path: impl Into<String>,
    content: impl Into<String>,
) -> Result<ResolvedConfigurationRef, polaris_cosmos::contracts::ContractError> {
    let value = ConfigurationValue::map(vec![
        ConfigurationEntry::new("path", ConfigurationValue::String(path.into()))?,
        ConfigurationEntry::new("content", ConfigurationValue::String(content.into()))?,
    ])?;
    ResolvedConfigurationRef::inline(create_file_schema(), value)
}

// ---------------------------------------------------------------------------
// A generic read-only survey over a configured path set.
// ---------------------------------------------------------------------------

/// The sandbox-relative paths [`survey_paths`] reads.
#[derive(Debug, Clone, Default)]
struct SurveyPaths(Vec<String>);

/// What [`survey_paths`] observed about one configured path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSurveyFinding {
    /// The path that was read, relative to the sandbox root.
    pub path: String,
    /// Bytes read, or zero if the path could not be read.
    pub bytes: usize,
    /// Lines read, or zero if the path could not be read.
    pub lines: usize,
    /// Whether the path was readable at all.
    pub readable: bool,
}

#[system]
async fn survey_paths(
    workspace: Res<CodingWorkspace>,
    paths: Out<SurveyPaths>,
) -> Vec<PathSurveyFinding> {
    paths
        .0
        .iter()
        .map(|path| match workspace.read(path) {
            Ok(content) => PathSurveyFinding {
                path: path.clone(),
                bytes: content.len(),
                lines: content.lines().count(),
                readable: true,
            },
            Err(_) => PathSurveyFinding {
                path: path.clone(),
                bytes: 0,
                lines: 0,
                readable: false,
            },
        })
        .collect()
}

/// Reads a configured set of sandbox-relative paths and reports their shape.
///
/// Wraps [`survey_paths`], seeded per instance by its own decoded path list:
/// `CodingWorkspace -> Vec<PathSurveyFinding>`. Read-only, and never fails: a
/// path that cannot be read is a finding (`readable: false`), not a refusal,
/// matching how [`super::existing::SurveyTree`] treats an unreadable
/// [`CodingQuestion::Inspect`](super::super::payload::CodingQuestion::Inspect)
/// target.
pub struct SurveyPathsBlock;

impl BlockFactory for SurveyPathsBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let paths = match config.resolved() {
            ResolvedConfiguration::Inline(value) => SurveyPaths(as_string_list(value)?),
            ResolvedConfiguration::ContentAddressed(_) => {
                return Err(BlockBuildError::new(
                    "content-addressed configuration is not supported",
                ));
            }
        };
        graph.add_system_node(move || {
            let paths = SurveyPaths(paths.0.clone());
            async move { paths }
        });
        Ok(graph.add_system_node(survey_paths))
    }
}

/// Returns [`SurveyPathsBlock`]'s registered key.
#[must_use]
pub fn survey_paths_key() -> CompositionBlockKey {
    block_key("code.survey-paths")
}

/// Returns the configuration schema [`SurveyPathsBlock`] declares: a plain
/// list of sandbox-relative path strings.
#[must_use]
pub fn survey_paths_schema() -> ConfigurationSchemaKey {
    schema_key("polaris-code.gate-blocks.survey-paths-config")
}

// ---------------------------------------------------------------------------
// A merge/reduce step for `Fork` outputs.
// ---------------------------------------------------------------------------

const fn severity(status: Status) -> u8 {
    match status {
        Status::Pass => 0,
        Status::NotApplicable => 1,
        Status::Indeterminate => 2,
        Status::Issue => 3,
    }
}

#[system]
async fn reduce_assessments(
    all: polaris::system::param::OutAll<Assessment<EditSite>>,
) -> Result<Assessment<EditSite>, SystemError> {
    all.iter()
        .max_by_key(|entry| severity(entry.value().status()))
        .map(|entry| entry.value().clone())
        .ok_or_else(|| exec("reduce-assessments requires at least one branch assessment"))
}

/// Reduces every `Fork` branch's edit-site assessment to the single worst
/// verdict.
///
/// Wraps [`reduce_assessments`]: `OutAll<Assessment<EditSite>> ->
/// Assessment<EditSite>`. Read-only, so it may itself sit inside a `Fork`'s
/// reducer position without becoming a second mutation. "Worst" ranks
/// [`Status::Issue`] above [`Status::Indeterminate`] above
/// [`Status::NotApplicable`] above [`Status::Pass`] — a local ranking this
/// block chooses, distinct from `Status`'s own derived `Ord`.
pub struct ReduceAssessmentsBlock;

impl BlockFactory for ReduceAssessmentsBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(reduce_assessments))
    }
}

/// Returns [`ReduceAssessmentsBlock`]'s registered key.
#[must_use]
pub fn reduce_assessments_key() -> CompositionBlockKey {
    block_key("code.reduce-assessments")
}

// ---------------------------------------------------------------------------
// A pass-through step for a `Choose`/`Switch` arm with nothing new to do.
// ---------------------------------------------------------------------------

#[system]
async fn affirm_assessment(
    assessment: Out<Assessment<EditSite>>,
) -> Result<Assessment<EditSite>, SystemError> {
    Ok(assessment.clone())
}

/// Forwards an already-produced edit-site assessment unchanged.
///
/// Every [`polaris_cosmos::composition::GateExpr::Step`] names a real
/// registered block, so a `Choose`/`Switch` arm that has nothing further to
/// do (for example: tests already passed, no repair needed) still needs one
/// to occupy that arm. Re-affirming the assessment already on hand — rather
/// than producing nothing — keeps a step downstream of the branch (one that
/// reads `Out<Assessment<EditSite>>`, such as
/// [`super::existing::ConcludeSinglePass`]) guaranteed on both arms, not just
/// the one that did more work. Read-only: it inspects and clones, never
/// writes.
pub struct AffirmAssessmentBlock;

impl BlockFactory for AffirmAssessmentBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(affirm_assessment))
    }
}

/// Returns [`AffirmAssessmentBlock`]'s registered key.
#[must_use]
pub fn affirm_assessment_key() -> CompositionBlockKey {
    block_key("code.affirm-assessment")
}

// ---------------------------------------------------------------------------
// Closing the contract without judging anything.
// ---------------------------------------------------------------------------

#[system]
async fn conclude_unverified(
    answer: Out<ModelStepArtifact>,
) -> Result<AssessmentConclusion, SystemError> {
    if !matches!(&*answer, ModelStepArtifact::Answer(_)) {
        return Err(exec(
            "code.conclude-unverified requires an upstream step configured for an answer",
        ));
    }
    let site = site_assessment(EditSite::new(
        "(workspace)".to_owned(),
        0,
        Status::Indeterminate,
    ))
    .map_err(exec)?;
    conclude_sites(vec![site], Vec::new(), StopReason::PredicateSatisfied).map_err(exec)
}

/// Closes the coding contract from an upstream model step's own free-text
/// answer, without computing any verdict about what it did.
///
/// Every other conclude block in this catalogue reads an already-judged
/// [`Assessment<EditSite>`] — `code.judge-stated-edits`,
/// `code.judge-patch-intent` and friends decide `Pass`/`Issue` first, and a
/// conclude block only closes the contract over that decision. This block
/// has no judge upstream of it by design (the `loop` topology
/// this is built for wires nothing between its own model step and this
/// one), so it
/// reports [`Status::Indeterminate`] unconditionally: reporting `Pass` here
/// would be exactly the vacuous pass `code.judge-patch-intent`'s own doc
/// comment warns a statement-only plan into, just reached by never judging
/// at all rather than by judging an empty edit. Whether the change the
/// upstream step made is correct is left to the caller, for example by
/// reading the workspace's own `git diff`.
pub struct ConcludeUnverifiedBlock;

impl BlockFactory for ConcludeUnverifiedBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(conclude_unverified))
    }
}

/// Returns [`ConcludeUnverifiedBlock`]'s registered key.
#[must_use]
pub fn conclude_unverified_key() -> CompositionBlockKey {
    block_key("code.conclude-unverified")
}

/// Every block key this module registers.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![
        model_propose_key(),
        model_direct_edit_key(),
        apply_patch_intent_key(),
        create_file_key(),
        survey_paths_key(),
        reduce_assessments_key(),
        affirm_assessment_key(),
        conclude_unverified_key(),
    ]
}

/// Registers every block in this module into `catalogue`.
///
/// # Errors
///
/// Returns [`CompositionCatalogueError::DuplicateBlock`] if a key here
/// collides with one already registered.
pub(super) fn register_all(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    catalogue.register_block(
        model_propose_key(),
        Arc::new(ModelProposeBlock),
        model_propose_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &model_propose_key(),
        describe(
            "Runs one bounded, tool-restricted, budgeted model turn and returns its configured \
             typed artifact.",
            Vec::new(),
            block_cards::model_step_artifact_port(),
            "configured with output kind patch-intent, its PatchIntent artifact wires into \
             code.apply-patch-intent via that block's own port",
        )
        .requires(block_cards::model_provider_available()),
    )?;
    catalogue.register_block(
        model_direct_edit_key(),
        Arc::new(ModelDirectEditBlock),
        model_propose_schema(),
        Effect::Mutating { repeat_safe: false },
    )?;
    // Described like every other block here, though the model planner never
    // offers it: `planner::nameable_gate_blocks_for_selection` excludes it
    // by key, whatever its description or facts say.
    catalogue.describe_block(
        &model_direct_edit_key(),
        describe(
            "Runs one bounded, budgeted model conversation whose own edit_file calls write to \
             the working tree directly, and returns its configured typed artifact.",
            Vec::new(),
            block_cards::model_step_artifact_port(),
            "the one generic loop arm's single model conversation, closed by \
             code.conclude-unverified",
        )
        .requires(block_cards::model_provider_available()),
    )?;
    catalogue.register_block(
        apply_patch_intent_key(),
        Arc::new(ApplyPatchIntentBlock),
        super::no_config_schema(),
        Effect::Mutating { repeat_safe: false },
    )?;
    catalogue.describe_block(
        &apply_patch_intent_key(),
        describe(
            "Applies a model-proposed patch intent's exact-string edits to the working tree.",
            vec![patch_intent_port_key()],
            block_cards::applied_edit_port(),
            "wired downstream of a code.model-propose block configured for patch-intent output",
        ),
    )?;
    catalogue.register_block(
        create_file_key(),
        Arc::new(CreateFileBlock),
        create_file_schema(),
        Effect::Mutating { repeat_safe: false },
    )?;
    catalogue.describe_block(
        &create_file_key(),
        describe(
            "Writes a new sandbox-relative file with configured path and content.",
            Vec::new(),
            block_cards::created_file_port(),
            "adds a new test file the stated edits alone cannot create",
        ),
    )?;
    catalogue.register_block(
        survey_paths_key(),
        Arc::new(SurveyPathsBlock),
        survey_paths_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &survey_paths_key(),
        describe(
            "Reads a configured set of sandbox-relative paths and reports their shape.",
            Vec::new(),
            block_cards::path_survey_findings_port(),
            "a read-only survey step over a task-specific path list",
        ),
    )?;
    catalogue.register_block(
        reduce_assessments_key(),
        Arc::new(ReduceAssessmentsBlock),
        super::no_config_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &reduce_assessments_key(),
        describe(
            "Reduces every Fork branch's edit-site assessment to the single worst verdict.",
            Vec::new(),
            block_cards::edit_site_assessment_port(),
            "the reducer position of a Fork voting over several edit-site assessments",
        ),
    )?;
    catalogue.register_block(
        affirm_assessment_key(),
        Arc::new(AffirmAssessmentBlock),
        super::no_config_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.describe_block(
        &affirm_assessment_key(),
        describe(
            "Forwards an already-produced edit-site assessment unchanged.",
            Vec::new(),
            block_cards::edit_site_assessment_port(),
            "occupies a Choose/Switch arm that has nothing further to do",
        ),
    )?;
    catalogue.register_block(
        conclude_unverified_key(),
        Arc::new(ConcludeUnverifiedBlock),
        super::no_config_schema(),
        Effect::ReadOnly,
    )?;
    // Never offered to the model planner either; see `model_direct_edit_key`
    // above.
    catalogue.describe_block(
        &conclude_unverified_key(),
        describe(
            "Closes the coding contract from an upstream model step's answer with an \
             Indeterminate verdict, judging nothing.",
            vec![block_cards::model_step_artifact_port()],
            block_cards::assessment_conclusion_port(),
            "the closing step after a code.model-direct-edit step configured for an answer",
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use polaris::graph::{CancellationSource, GraphExecutor};
    use polaris::system::param::{Access, SystemContext};

    use super::*;
    use crate::cosmos::assess::site_assessment;
    use crate::cosmos::task::{CodingIntent, CodingTask};

    fn guarantees(graph: &Graph, type_id: TypeId) -> bool {
        graph
            .signature()
            .guaranteed_produces()
            .iter()
            .any(|access: &Access| access.type_id == type_id)
    }

    fn workspace(lib_rs: &str) -> (tempfile::TempDir, CodingWorkspace) {
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), lib_rs).expect("seed file");
        let task = CodingTask::new(
            "gate-block-configurable",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1 + 1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        (dir, CodingWorkspace::for_task(&task))
    }

    fn patch_intent_config() -> ResolvedConfigurationRef {
        ModelStepConfig::new(
            "revise".to_owned(),
            "propose a patch that fixes the failing test".to_owned(),
            ToolSubset::Only(vec!["read_file".to_owned()]),
            TokenBudget(2_000),
            ModelStepOutputKind::PatchIntent,
        )
        .encode()
        .expect("well-formed configuration encodes")
    }

    fn answer_config() -> ResolvedConfigurationRef {
        ModelStepConfig::new(
            "answer".to_owned(),
            "answer the question".to_owned(),
            ToolSubset::None,
            TokenBudget(500),
            ModelStepOutputKind::Answer,
        )
        .encode()
        .expect("well-formed configuration encodes")
    }

    fn plan_config() -> ResolvedConfigurationRef {
        ModelStepConfig::new(
            "plan".to_owned(),
            "describe the next steps".to_owned(),
            ToolSubset::None,
            TokenBudget(500),
            ModelStepOutputKind::Plan,
        )
        .encode()
        .expect("well-formed configuration encodes")
    }

    fn survey_config(paths: &[&str]) -> ResolvedConfigurationRef {
        ResolvedConfigurationRef::inline(
            survey_paths_schema(),
            ConfigurationValue::List(
                paths
                    .iter()
                    .map(|path| ConfigurationValue::String((*path).to_owned()))
                    .collect(),
            ),
        )
        .expect("well-formed path list encodes")
    }

    #[tokio::test]
    async fn model_propose_returns_its_scripted_artifact() {
        let (_dir, workspace) = workspace("todo!()");
        let mut graph = Graph::new();
        ModelProposeBlock
            .build(&mut graph, &patch_intent_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<PatchIntent>()));

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit"),
                ],
                check: None,
            },
        )));

        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("model-propose runs standalone");
        let artifact = execution
            .output::<PatchIntent>()
            .expect("produces a PatchIntent");
        assert_eq!(artifact.edits.len(), 1);
    }

    #[tokio::test]
    async fn two_differently_configured_model_propose_instances_coexist_in_one_graph() {
        // The known limitation an ambient Res<ModelStepConfig> resource would
        // hit: two instances, two different roles, one graph, one Fork.
        let (_dir, workspace) = workspace("todo!()");
        let mut graph = Graph::new();
        graph.add_parallel(
            "vote",
            [
                Box::new(|branch: &mut Graph| {
                    ModelProposeBlock
                        .build(branch, &plan_config())
                        .expect("first instance builds");
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|branch: &mut Graph| {
                    ModelProposeBlock
                        .build(branch, &answer_config())
                        .expect("second instance builds");
                }),
            ],
        );
        graph.add_system(collect_roles);
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(RoleEchoingModelStep));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("both instances run with their own configuration");
        let roles = execution
            .output::<std::collections::BTreeSet<String>>()
            .expect("collects a role from each instance");
        assert_eq!(
            *roles,
            std::collections::BTreeSet::from(["plan".to_owned(), "answer".to_owned()]),
            "each instance saw its own decoded role, not a shared ambient one"
        );
    }

    #[system]
    async fn collect_roles(
        all: polaris::system::param::OutAll<ModelStepArtifact>,
    ) -> std::collections::BTreeSet<String> {
        all.iter()
            .filter_map(|entry| match entry.value() {
                ModelStepArtifact::Plan(role) => Some(role.clone()),
                _ => None,
            })
            .collect()
    }

    struct RoleEchoingModelStep;
    impl ModelStepExecutor for RoleEchoingModelStep {
        fn propose<'a>(
            &'a self,
            config: &'a ModelStepConfig,
            _workspace: &'a WorkspaceView<'a>,
            _meter: &'a TokenMeter,
        ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
        {
            Box::pin(async move { Ok(ModelStepArtifact::Plan(config.role.clone())) })
        }
    }

    #[tokio::test]
    async fn apply_patch_intent_writes_the_proposed_edit() {
        let (_dir, workspace) = workspace("todo!()");
        let mut graph = Graph::new();
        ModelProposeBlock
            .build(&mut graph, &patch_intent_config())
            .expect("builds");
        ApplyPatchIntentBlock
            .build(&mut graph, &super_no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<AppliedEdit>()));

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit"),
                ],
                check: None,
            },
        )));

        GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("propose then apply runs");
        assert_eq!(workspace.read("lib.rs").expect("read back"), "1 + 1");
    }

    #[test]
    fn apply_patch_intent_rejects_a_non_patch_producer_before_execution() {
        let (_dir, workspace) = workspace("todo!()");
        let mut graph = Graph::new();
        let producer = ModelProposeBlock
            .build(&mut graph, &answer_config())
            .expect("builds");
        let reader = ApplyPatchIntentBlock
            .build(&mut graph, &super_no_config())
            .expect("builds");
        ApplyPatchIntentBlock
            .wire_input(&mut graph, reader, producer, &patch_intent_port_key())
            .expect("patch port is recognized");

        let _ = workspace;
        assert!(
            graph.validate().is_err(),
            "an Answer producer cannot satisfy an apply-patch-intent input"
        );
    }

    #[tokio::test]
    async fn create_file_writes_a_new_path() {
        let (_dir, workspace) = workspace("todo!()");
        let mut graph = Graph::new();
        CreateFileBlock
            .build(
                &mut graph,
                &create_file_config("tests/new_test.rs", "// a new test\n")
                    .expect("well-formed configuration encodes"),
            )
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<CreatedFile>()));

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("create-file runs standalone");
        let created = execution
            .output::<CreatedFile>()
            .expect("produces CreatedFile");
        assert_eq!(created.path, "tests/new_test.rs");
        assert_eq!(
            workspace.read("tests/new_test.rs").expect("read back"),
            "// a new test\n"
        );
    }

    #[tokio::test]
    async fn create_file_refuses_to_overwrite_an_existing_path() {
        let (_dir, workspace) = workspace("todo!()");
        let mut graph = Graph::new();
        CreateFileBlock
            .build(
                &mut graph,
                &create_file_config("lib.rs", "overwritten")
                    .expect("well-formed configuration encodes"),
            )
            .expect("builds");

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let outcome = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await;
        assert!(outcome.is_err(), "lib.rs already exists");
        assert_eq!(workspace.read("lib.rs").expect("unchanged"), "todo!()");
    }

    #[tokio::test]
    async fn survey_paths_reports_readable_and_unreadable_paths() {
        let (_dir, workspace) = workspace("hello\nworld\n");
        let mut graph = Graph::new();
        SurveyPathsBlock
            .build(&mut graph, &survey_config(&["lib.rs", "missing.rs"]))
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<Vec<PathSurveyFinding>>()));

        let mut context = SystemContext::new();
        context.insert(workspace);
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("survey-paths runs standalone");
        let findings = execution
            .output::<Vec<PathSurveyFinding>>()
            .expect("produces findings");
        assert_eq!(findings.len(), 2);
        assert!(findings[0].readable);
        assert_eq!(findings[0].lines, 2);
        assert!(!findings[1].readable);
    }

    #[tokio::test]
    async fn reduce_assessments_keeps_the_worst_branch_verdict() {
        let mut graph = Graph::new();
        graph.add_parallel(
            "vote",
            [
                Box::new(|branch: &mut Graph| {
                    branch.add_system(pass_assessment);
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|branch: &mut Graph| {
                    branch.add_system(issue_assessment);
                }),
            ],
        );
        ReduceAssessmentsBlock
            .build(&mut graph, &super_no_config())
            .expect("builds");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(guarantees(&graph, TypeId::of::<Assessment<EditSite>>()));

        let mut context = SystemContext::new();
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("fork then reduce runs");
        let combined = execution
            .output::<Assessment<EditSite>>()
            .expect("produces the reduced assessment");
        assert_eq!(combined.status(), Status::Issue);
    }

    fn super_no_config() -> ResolvedConfigurationRef {
        ResolvedConfigurationRef::inline(super::super::no_config_schema(), ConfigurationValue::Null)
            .expect("a null value satisfies the no-config schema")
    }

    #[system]
    async fn pass_assessment() -> Result<Assessment<EditSite>, SystemError> {
        site_assessment(EditSite::new("a.rs".to_owned(), 0, Status::Pass)).map_err(exec)
    }

    #[system]
    async fn issue_assessment() -> Result<Assessment<EditSite>, SystemError> {
        site_assessment(EditSite::new("b.rs".to_owned(), 0, Status::Issue)).map_err(exec)
    }

    // -----------------------------------------------------------------
    // LiveModelStep: a mock LlmProvider stands in for a real model, never
    // touching the network. It answers one queued response per `generate`
    // call, so a multi-turn tool round trip scripts as a short response
    // sequence.
    // -----------------------------------------------------------------

    use polaris::models::ModelRegistry;
    use polaris::models::llm::{
        AssistantBlock, GenerationError, LlmProvider, LlmResponse, StopReason as LlmStopReason,
        ToolCall, ToolFunction, Usage,
    };

    struct ScriptedTurnProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<LlmResponse>>,
        /// The `max_output_tokens` of every request received, in order.
        output_caps: std::sync::Arc<std::sync::Mutex<Vec<Option<u32>>>>,
        /// Every request received, in order — so a test can inspect what
        /// cache markers and message history a turn actually sent.
        requests: std::sync::Arc<std::sync::Mutex<Vec<LlmRequest>>>,
    }

    impl ScriptedTurnProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into_iter().collect()),
                output_caps: std::sync::Arc::default(),
                requests: std::sync::Arc::default(),
            }
        }
    }

    impl LlmProvider for ScriptedTurnProvider {
        fn name(&self) -> &'static str {
            "scripted-turn"
        }

        async fn generate(
            &self,
            _model: &str,
            request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            self.output_caps
                .lock()
                .expect("mock provider mutex is never poisoned")
                .push(request.max_output_tokens.map(std::num::NonZeroU32::get));
            self.requests
                .lock()
                .expect("mock provider mutex is never poisoned")
                .push(request.clone());
            self.responses
                .lock()
                .expect("mock provider mutex is never poisoned")
                .pop_front()
                .ok_or_else(|| {
                    GenerationError::UnsupportedOperation(
                        "scripted turn provider has no more queued responses".to_owned(),
                    )
                })
        }
    }

    /// Registers a fresh [`ScriptedTurnProvider`] and returns the handle to
    /// it, mirroring how a real provider plugin registers with the model
    /// registry — never a live call.
    fn live_llm(responses: Vec<LlmResponse>) -> Llm {
        live_llm_recording(responses).0
    }

    /// Like [`live_llm`], plus the output cap each request carried.
    fn live_llm_recording(
        responses: Vec<LlmResponse>,
    ) -> (Llm, std::sync::Arc<std::sync::Mutex<Vec<Option<u32>>>>) {
        let provider = ScriptedTurnProvider::new(responses);
        let output_caps = std::sync::Arc::clone(&provider.output_caps);
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(provider);
        let llm = registry
            .llm("scripted-turn/mock")
            .expect("the just-registered provider resolves");
        (llm, output_caps)
    }

    /// Like [`live_llm`], plus every full request the mock provider received —
    /// for asserting on the cache markers and message history a turn sent.
    fn live_llm_capturing(
        responses: Vec<LlmResponse>,
    ) -> (Llm, std::sync::Arc<std::sync::Mutex<Vec<LlmRequest>>>) {
        let provider = ScriptedTurnProvider::new(responses);
        let requests = std::sync::Arc::clone(&provider.requests);
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(provider);
        let llm = registry
            .llm("scripted-turn/mock")
            .expect("the just-registered provider resolves");
        (llm, requests)
    }

    /// A provider whose `generate` never resolves, for proving that a
    /// cancelled step returns instead of waiting on it forever.
    struct PendingProvider;

    impl LlmProvider for PendingProvider {
        fn name(&self) -> &'static str {
            "pending"
        }

        async fn generate(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            std::future::pending().await
        }
    }

    fn pending_llm() -> Llm {
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(PendingProvider);
        registry
            .llm("pending/mock")
            .expect("the just-registered provider resolves")
    }

    fn text_turn(text: impl Into<String>, input_tokens: u64, output_tokens: u64) -> LlmResponse {
        LlmResponse {
            content: vec![AssistantBlock::text(text)],
            usage: Usage {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                ..Usage::default()
            },
            stop_reason: LlmStopReason::EndTurn,
            id: None,
            model: None,
        }
    }

    fn tool_call_turn(name: &str, arguments: serde_json::Value) -> LlmResponse {
        tool_call_turn_with_usage(name, arguments, 5, 5)
    }

    /// Like [`tool_call_turn`], with a caller-chosen usage instead of the
    /// fixed 5-in/5-out default — for scripting a turn whose own cost matters
    /// to the assertion (a budget check, or a cumulative sum across turns).
    fn tool_call_turn_with_usage(
        name: &str,
        arguments: serde_json::Value,
        input_tokens: u64,
        output_tokens: u64,
    ) -> LlmResponse {
        LlmResponse {
            content: vec![AssistantBlock::tool_call(ToolCall {
                id: "call-1".to_owned(),
                call_id: None,
                function: ToolFunction {
                    name: name.to_owned(),
                    arguments,
                },
                signature: None,
                additional_params: None,
            })],
            usage: Usage {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                ..Usage::default()
            },
            stop_reason: LlmStopReason::ToolUse,
            id: None,
            model: None,
        }
    }

    fn live_config(
        output: ModelStepOutputKind,
        tool_subset: ToolSubset,
        budget: u32,
    ) -> ModelStepConfig {
        ModelStepConfig::new(
            "revise".to_owned(),
            "do the configured thing".to_owned(),
            tool_subset,
            TokenBudget(budget),
            output,
        )
    }

    /// Whether every `AssistantBlock::ToolCall` in `messages` is answered by
    /// a matching `UserBlock::ToolResult` in the very next message — the
    /// shape a later step must find if it is to resume this conversation,
    /// since a provider replays the same history back to itself and refuses
    /// a tool call with no result.
    fn every_tool_call_is_answered(messages: &[Message]) -> bool {
        messages.windows(2).all(|pair| {
            let Message::Assistant { content, .. } = &pair[0] else {
                return true;
            };
            let call_ids: Vec<&str> = content
                .iter()
                .filter_map(|block| match block {
                    AssistantBlock::ToolCall(call) => Some(call.id.as_str()),
                    _ => None,
                })
                .collect();
            if call_ids.is_empty() {
                return true;
            }
            let Message::User { content } = &pair[1] else {
                return false;
            };
            call_ids.iter().all(|id| {
                content
                    .iter()
                    .any(|block| matches!(block, UserBlock::ToolResult(result) if result.id == *id))
            })
        })
    }

    #[tokio::test]
    async fn live_step_decodes_a_plan_artifact() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"plan","plan":"read lib.rs, then patch it"}"#,
            10,
            20,
        )]));
        let config = live_config(ModelStepOutputKind::Plan, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a well-formed plan answer decodes");
        assert_eq!(
            artifact,
            ModelStepArtifact::Plan("read lib.rs, then patch it".to_owned())
        );
    }

    #[tokio::test]
    async fn live_step_records_each_call_as_one_json_line() {
        let (dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let log = dir.path().join(".polaris/model-steps/s.jsonl");
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"plan","plan":"read lib.rs"}"#,
            10,
            20,
        )]))
        .with_transcript(&log);
        let config = live_config(ModelStepOutputKind::Plan, ToolSubset::None, 1_000);
        executor
            .propose(&config, &view, &meter)
            .await
            .expect("a well-formed plan answer decodes");
        let text = std::fs::read_to_string(&log).expect("the call was recorded");
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["role"], config.role.as_str());
        assert_eq!(lines[0]["turn"], 0);
        assert!(lines[0]["sent"][0].to_string().contains("Instruction"));
        assert!(lines[0]["reply"].to_string().contains("read lib.rs"));
        assert_eq!(lines[0]["usage"]["input_tokens"], 10);
    }

    #[tokio::test]
    async fn live_step_decodes_a_findings_artifact() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"findings","findings":["a","b"]}"#,
            10,
            20,
        )]));
        let config = live_config(ModelStepOutputKind::Findings, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a well-formed findings answer decodes");
        assert_eq!(
            artifact,
            ModelStepArtifact::Findings(vec!["a".to_owned(), "b".to_owned()])
        );
    }

    #[tokio::test]
    async fn live_step_decodes_a_patch_intent_artifact() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"patch-intent","edits":[{"file":"lib.rs","find":"todo!()","replace":"1 + 1"}]}"#,
            10,
            20,
        )]));
        let config = live_config(ModelStepOutputKind::PatchIntent, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a well-formed patch-intent answer decodes");
        assert_eq!(
            artifact,
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit")
                ],
                check: None,
            }
        );
    }

    #[tokio::test]
    async fn live_step_decodes_a_review_artifact() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"review","verdict":"pass","notes":"looks fine"}"#,
            10,
            20,
        )]));
        let config = live_config(ModelStepOutputKind::Review, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a well-formed review answer decodes");
        assert_eq!(
            artifact,
            ModelStepArtifact::Review {
                verdict: Status::Pass,
                notes: "looks fine".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn live_step_decodes_an_answer_artifact() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"answer","answer":"42"}"#,
            10,
            20,
        )]));
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a well-formed answer decodes");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
    }

    // `decode_model_step_answer` is the pure decoder both the `submit_answer`
    // tool-call path and the lenient text fallback resolve to. Its own
    // refusal shapes are exercised directly here, since the live turn loop
    // no longer surfaces them as a hard failure on the first bad reply — see
    // the `live_step_*` retry tests below for that behaviour.
    #[test]
    fn decode_model_step_answer_refuses_a_non_json_reply() {
        let error = decode_model_step_answer("not json at all", ModelStepOutputKind::Answer)
            .expect_err("non-JSON text is not a usable answer");
        assert!(
            matches!(&error, SystemError::ExecutionError(message) if message.contains("not valid JSON")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn decode_model_step_answer_refuses_a_kind_mismatch() {
        // Configured for Answer, but the model replied with a Plan.
        let error = decode_model_step_answer(
            r#"{"kind":"plan","plan":"do it"}"#,
            ModelStepOutputKind::Answer,
        )
        .expect_err("a plan answer does not satisfy an answer-configured step");
        assert!(
            matches!(&error, SystemError::ExecutionError(message) if message.contains("does not match its configured output")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn decode_model_step_answer_refuses_an_unknown_top_level_field() {
        let error = decode_model_step_answer(
            r#"{"kind":"answer","answer":"42","extra":"surprise"}"#,
            ModelStepOutputKind::Answer,
        )
        .expect_err("an unrecognized top-level field must be refused, not ignored");
        assert!(
            matches!(&error, SystemError::ExecutionError(message) if message.contains("malformed")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn decode_model_step_answer_refuses_a_patch_intent_edit_with_an_unknown_field() {
        let error = decode_model_step_answer(
            r#"{"kind":"patch-intent","edits":[{"file":"lib.rs","find":"todo!()","replace":"1 + 1","note":"surprise"}]}"#,
            ModelStepOutputKind::PatchIntent,
        )
        .expect_err("an unrecognized field inside an edit must be refused, not ignored");
        assert!(
            matches!(&error, SystemError::ExecutionError(message) if message.contains("malformed")),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn lean_strict_cap_counts_one_hundred_calls_even_with_invalid_final_submission() {
        let (_dir, workspace) = workspace("unchanged");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let mut turns = (0..99)
            .map(|_| text_turn("still working", 300_000, 10))
            .collect::<Vec<_>>();
        turns.push(tool_call_turn(
            SUBMIT_ANSWER_TOOL,
            serde_json::json!({"kind":"answer"}),
        ));
        let (llm, requests) = live_llm_capturing(turns);
        let executor = LiveModelStep::new(llm);
        let mut config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, u32::MAX)
            .with_max_turns(100);
        config.strict_turn_cap = true;
        config.compact_history = false;
        executor.propose(&config, &view, &meter).await.unwrap();
        assert_eq!(meter.snapshot("mock").num_turns, 100);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 100);
        assert!(requests.iter().all(|r| r.max_output_tokens.is_none()));
        assert!(requests.last().unwrap().tool_choice.is_some());
    }

    #[tokio::test]
    async fn lean_navigation_is_offered_and_answers_through_the_existing_index() {
        let (dir, workspace) = workspace("fn helper() {}\nfn caller() { helper(); }\n");
        let store = polaris_ontrace::oxigraph::OxigraphStore::in_memory().unwrap();
        crate::tools::code_graph::CodeIndexer::index_repo(&store, dir.path())
            .await
            .unwrap();
        let navigation =
            crate::tools::code_graph::CodeNavigation::new(store, dir.path().to_owned());
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn("find_definition", serde_json::json!({"name":"helper"})),
            tool_call_turn("find_references", serde_json::json!({"name":"helper"})),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind":"answer","answer":"done"}),
            ),
        ]);
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec!["find_definition".into(), "find_references".into()]),
            u32::MAX,
        );
        LiveModelStep::new(llm)
            .with_navigation(navigation)
            .propose(
                &config,
                &WorkspaceView::new(&workspace),
                &TokenMeter::default(),
            )
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        assert!(format!("{:?}", requests[0].tools).contains("find_definition"));
        assert!(format!("{:?}", requests[1].messages).contains("lib.rs"));
        assert!(format!("{:?}", requests[2].messages).contains("caller"));
    }

    #[test]
    fn warm_continuation_keeps_old_autotest_feedback_short() {
        let mut messages = vec![Message::User {
            content: vec![UserBlock::tool_result(
                "edit-1".to_owned(),
                ToolResultContent::Text(format!(
                    "edited\nAutotest (related subset): {}",
                    "x".repeat(900)
                )),
            )],
        }];
        compact_prior_autotests(&mut messages);
        let rendered = format!("{:?}", messages);
        assert!(rendered.contains("summarized by system check"));
        assert!(!rendered.contains(&"x".repeat(100)));
    }

    #[test]
    fn warm_continuation_preserves_an_autotest_not_run_message() {
        let mut messages = vec![Message::User {
            content: vec![UserBlock::tool_result(
                "edit-1".to_owned(),
                ToolResultContent::Text("edited\nAutotest not run: no allowed runner".to_owned()),
            )],
        }];
        let original = messages.clone();
        compact_prior_autotests(&mut messages);
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }

    #[test]
    fn trim_counts_only_requests_after_the_rewrite() {
        assert_eq!(future_cache_reads(18, 0), 17);
        assert_eq!(future_cache_reads(18, 8), 9);
        assert_eq!(future_cache_reads(18, 17), 0);
        assert_eq!(future_cache_reads(18, 18), 0);
    }

    #[tokio::test]
    async fn lean_last_turn_keeps_the_message_cache_lineage() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind":"answer","answer":"done"}),
            ),
        ]);
        let mut config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        )
        .with_max_turns(2);
        config.stable_tool_choice = true;
        config.strict_turn_cap = true;
        config.compact_history = false;
        let (_, conversation) = LiveModelStep::new(llm)
            .run(&config, &view, &TokenMeter::new(), None)
            .await
            .unwrap();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests.iter().all(|request| request.tool_choice.is_none()));
            assert!(requests.iter().all(|request| request.cache.prefix));
            assert_eq!(
                serde_json::to_value(&requests[0].messages).unwrap(),
                serde_json::to_value(&conversation.0[..1]).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&requests[1].messages).unwrap(),
                serde_json::to_value(&conversation.0[..3]).unwrap()
            );
        }
        let (llm, resumed_requests) = live_llm_capturing(vec![tool_call_turn(
            SUBMIT_ANSWER_TOOL,
            serde_json::json!({"kind":"answer","answer":"fixed"}),
        )]);
        config.instruction = "System check failed; continue.".to_owned();
        LiveModelStep::new(llm)
            .run(&config, &view, &TokenMeter::new(), Some(&conversation))
            .await
            .unwrap();
        let resumed = resumed_requests.lock().unwrap();
        assert_eq!(resumed.len(), 1);
        assert!(resumed[0].tool_choice.is_none());
        assert_eq!(
            serde_json::to_value(&resumed[0].messages[..conversation.0.len()]).unwrap(),
            serde_json::to_value(&conversation.0).unwrap()
        );
    }

    #[tokio::test]
    async fn lean_autotest_batches_edits_tracks_shell_creations_and_is_opt_in() {
        use std::os::unix::fs::PermissionsExt;
        for enabled in [false, true] {
            let (dir, workspace) = workspace("before");
            for path in ["bin", "tests"] {
                std::fs::create_dir(dir.path().join(path)).unwrap();
            }
            std::fs::write(dir.path().join("tests/test_widget.py"), "before").unwrap();
            std::fs::write(dir.path().join("tests/test_other.py"), "before").unwrap();
            let runner = dir.path().join("bin/test");
            std::fs::write(&runner, "#!/bin/sh\nprintf 'run\\n' >> runs\ntest \"$(cat tests/test_widget.py)\" = after || exit 9\ntest \"$(cat tests/test_other.py)\" = after || exit 9\necho 'E AssertionError: fixture failure'\necho '1 failed, 1 passed in 0.01s'\nexit 1\n").unwrap();
            std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
            let edit = |path, old| {
                tool_call_turn(
                    EDIT_FILE_TOOL,
                    serde_json::json!({"path":path,"old_string":old,"new_string":"after"}),
                )
            };
            let mut batch = edit("tests/test_widget.py", "before");
            let mut second = edit("tests/test_other.py", "before");
            if let AssistantBlock::ToolCall(call) = &mut second.content[0] {
                call.id = "call-2".into();
            }
            batch.content.extend(second.content);
            let (llm, requests) = live_llm_capturing(vec![
                batch,
                tool_call_turn(
                    EDIT_FILE_TOOL,
                    serde_json::json!({"path":"tests/test_widget.py", "old_string":"not present", "new_string":"also absent"}),
                ),
                tool_call_turn(
                    RUN_COMMAND_TOOL,
                    serde_json::json!({"command":"touch tests/test_new.py"}),
                ),
                tool_call_turn(
                    SUBMIT_ANSWER_TOOL,
                    serde_json::json!({"kind":"answer","answer":"done"}),
                ),
            ]);
            let shell = polaris::shell::ShellExecutor::new(
                polaris::shell::ShellConfig::new()
                    .with_working_dir(dir.path())
                    .with_allowed_dirs(vec![dir.path().to_owned()])
                    .with_allowed_commands(vec!["bin/test *".into(), "touch *".into()])
                    .disable_cache(),
            );
            let mut config = live_config(
                ModelStepOutputKind::Answer,
                ToolSubset::Only(vec![EDIT_FILE_TOOL.into(), RUN_COMMAND_TOOL.into()]),
                u32::MAX,
            );
            config.autotest = enabled;
            let decoded = ModelStepConfig::decode(&config.encode().unwrap()).unwrap();
            assert_eq!(decoded.autotest, enabled);
            let meter = TokenMeter::default();
            LiveModelStep::new(llm)
                .with_shell(shell)
                .propose(&config, &WorkspaceView::new(&workspace), &meter)
                .await
                .unwrap();
            assert_eq!(
                meter.snapshot("mock").num_turns,
                4,
                "autotests make no model calls"
            );
            let requests = requests.lock().unwrap();
            let feedback = format!("{:?}", requests[1].messages);
            if enabled {
                assert_eq!(
                    std::fs::read_to_string(dir.path().join("runs")).unwrap(),
                    "run\nrun\n"
                );
                assert_eq!(feedback.matches("Autotest (related subset)").count(), 1);
                assert!(feedback.contains("1 failed, 1 passed"), "{feedback}");
                assert!(feedback.contains("fixture failure"));
                assert_eq!(
                    format!("{:?}", requests[2].messages)
                        .matches("Autotest (related subset)")
                        .count(),
                    1,
                    "failed edit must not run tests"
                );
                assert_eq!(
                    format!("{:?}", requests[3].messages)
                        .matches("Autotest (related subset)")
                        .count(),
                    2,
                    "new shell-created test is covered"
                );
            } else {
                assert!(!dir.path().join("runs").exists());
                assert!(!feedback.contains("Autotest"));
            }
        }
    }

    #[tokio::test]
    async fn lean_command_is_host_gated_and_can_run_a_project_test() {
        let (dir, workspace) = workspace("before");
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            dir.path().join("check.py"),
            "from pathlib import Path\nassert Path('lib.rs').is_file()\nprint('project-test-passed')",
        )
        .unwrap();
        let bin = tempfile::tempdir().unwrap();
        for (name, script) in [
            ("python", "#!/bin/sh\nexec python3 \"$@\"\n"),
            ("pytest", "#!/bin/sh\nexec python3 check.py\n"),
        ] {
            let path = bin.path().join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let view = WorkspaceView::new(&workspace);
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![EDIT_FILE_TOOL.into(), RUN_COMMAND_TOOL.into()]),
            u32::MAX,
        );
        let submit = || {
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind":"answer","answer":"done"}),
            )
        };
        let (llm, requests) = live_llm_capturing(vec![submit()]);
        LiveModelStep::new(llm)
            .propose(&config, &view, &TokenMeter::default())
            .await
            .unwrap();
        assert!(!format!("{:?}", requests.lock().unwrap()[0].tools).contains(RUN_COMMAND_TOOL));
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(
                RUN_COMMAND_TOOL,
                serde_json::json!({"command":"python check.py"}),
            ),
            tool_call_turn(RUN_COMMAND_TOOL, serde_json::json!({"command":"pytest -q"})),
            tool_call_turn(
                RUN_COMMAND_TOOL,
                serde_json::json!({"command":"touch refused"}),
            ),
            submit(),
        ]);
        let shell = polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_owned()])
                .with_allowed_commands(vec!["python *".to_owned(), "pytest *".to_owned()])
                .with_path(format!("{}:/usr/bin:/bin", bin.path().display())),
        );
        let meter = TokenMeter::default();
        LiveModelStep::new(llm)
            .with_shell(shell)
            .propose(&config, &view, &meter)
            .await
            .unwrap();
        assert_eq!(meter.snapshot("mock").num_turns, 4);
        let requests = requests.lock().unwrap();
        assert!(format!("{:?}", requests[0].tools).contains(RUN_COMMAND_TOOL));
        for request in &requests[1..=2] {
            let feedback = format!("{:?}", request.messages);
            assert!(feedback.contains("project-test-passed"), "{feedback}");
            assert!(feedback.contains("exit code: 0"));
        }
        assert!(format!("{:?}", requests[3].messages).contains("does not allow"));
        assert!(!dir.path().join("refused").exists());
        let mut graph = Graph::new();
        assert!(
            ModelProposeBlock
                .build(&mut graph, &config.encode().unwrap())
                .is_err()
        );
    }

    #[tokio::test]
    async fn live_step_ends_when_submit_answer_is_called_with_a_valid_answer() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![tool_call_turn(
            SUBMIT_ANSWER_TOOL,
            serde_json::json!({"kind": "answer", "answer": "42"}),
        )]));
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a valid submit_answer call ends the step");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
    }

    #[tokio::test]
    async fn live_step_resumes_a_submitted_conversation_with_its_tool_result_answered() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let submit = || {
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            )
        };
        let (llm, requests) = live_llm_capturing(vec![submit(), submit()]);
        let executor = LiveModelStep::new(llm);
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        let (_, conversation) = executor
            .run(&config, &view, &meter, None)
            .await
            .expect("the first call submits");
        executor
            .run(&config, &view, &meter, Some(&conversation))
            .await
            .expect("the resumed call submits");

        let requests = requests.lock().expect("mutex is never poisoned");
        let resumed = &requests.last().expect("two requests were sent").messages;
        let answered = resumed.iter().any(|message| {
            matches!(message, Message::User { content } if content
                .iter()
                .any(|block| matches!(block, UserBlock::ToolResult(_))))
        });
        assert!(
            answered,
            "the resumed request answers the first submit_answer call"
        );
    }

    #[tokio::test]
    async fn handoff_continue_preserves_prior_tool_results_across_artifact_kinds() {
        let (_dir, workspace) = workspace("earlier finding");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({"path": "lib.rs"})),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "findings", "findings": ["found it"]}),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "done"}),
            ),
        ]);
        let provider = ModelStepProvider::new(LiveModelStep::new(llm));
        let handoff = ModelStepHandoff::default();
        let first = live_config(
            ModelStepOutputKind::Findings,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        );
        run_with_handoff(&first, &provider, &view, &meter, &handoff)
            .await
            .expect("first step");
        let second = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 10_000)
            .with_context_mode(ContextMode::Continue);
        run_with_handoff(&second, &provider, &view, &meter, &handoff)
            .await
            .expect("continued step");
        let requests = requests.lock().expect("requests lock");
        let resumed = &requests[2].messages;
        let resumed_text = format!("{resumed:?}");
        assert!(resumed_text.contains("earlier finding"));
        assert!(resumed_text.contains("answer"));
        assert_eq!(handoff.records().len(), 2);
    }

    #[tokio::test]
    async fn handoff_brief_is_bounded_and_fresh_does_not_receive_it() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "first"}),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "brief"}),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "fresh"}),
            ),
        ]);
        let provider = ModelStepProvider::new(LiveModelStep::new(llm));
        let handoff = ModelStepHandoff::default();
        let first = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 10_000);
        run_with_handoff(&first, &provider, &view, &meter, &handoff)
            .await
            .expect("first step");
        handoff.inner.lock().expect("handoff lock").records[0]
            .observations
            .push("x".repeat(10_000));
        let brief = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 10_000)
            .with_context_mode(ContextMode::Brief);
        run_with_handoff(&brief, &provider, &view, &meter, &handoff)
            .await
            .expect("brief step");
        assert!(handoff.brief().chars().count() <= 4_096);
        let fresh = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 10_000);
        run_with_handoff(&fresh, &provider, &view, &meter, &handoff)
            .await
            .expect("fresh step");
        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests[1].messages.len(), 1);
        assert!(format!("{:?}", requests[1].messages[0]).contains("Earlier model steps"));
        assert_eq!(requests[2].messages.len(), 1);
        assert!(!format!("{:?}", requests[2].messages[0]).contains("Earlier model steps"));
    }

    #[test]
    fn brief_names_files_commands_outcomes_and_open_questions() {
        let config = live_config(ModelStepOutputKind::Findings, ToolSubset::None, 1_000);
        let handoff = ModelStepHandoff::default();
        let conversation = ModelConversation(vec![
            Message::Assistant {
                id: None,
                content: vec![AssistantBlock::tool_call(ToolCall::new(
                    "run-1",
                    RUN_PYTHON_TOOL,
                    serde_json::json!({"script": "print(1)"}),
                ))],
            },
            Message::User {
                content: vec![UserBlock::tool_result(
                    "run-1",
                    ToolResultContent::Text("exit 0: 1".to_owned()),
                )],
            },
            Message::Assistant {
                id: None,
                content: vec![AssistantBlock::tool_call(ToolCall::new(
                    "read-1",
                    READ_FILE_TOOL,
                    serde_json::json!({"path": "lib.rs"}),
                ))],
            },
        ]);
        handoff.record(
            &config,
            &ModelStepArtifact::Findings(vec!["Why does this branch fail?".to_owned()]),
            conversation,
            0,
        );
        let brief = handoff.brief();
        assert!(brief.contains("lib.rs"));
        assert!(brief.contains("run_python"));
        assert!(brief.contains("exit 0"));
        assert!(brief.contains("Why does this branch fail?"));
        assert!(brief.chars().count() <= 4_096);
    }

    #[tokio::test]
    async fn continued_handoff_compacts_when_the_next_steps_budget_is_smaller() {
        let (_dir, workspace) = workspace(&"x".repeat(60_000));
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({"path": "lib.rs"})),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "first"}),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "second"}),
            ),
        ]);
        let provider = ModelStepProvider::new(LiveModelStep::new(llm));
        let handoff = ModelStepHandoff::default();
        let first = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            100_000,
        );
        run_with_handoff(&first, &provider, &view, &meter, &handoff)
            .await
            .expect("first step");
        let second = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000)
            .with_context_mode(ContextMode::Continue);
        run_with_handoff(&second, &provider, &view, &meter, &handoff)
            .await
            .expect("continued step");
        assert!(handoff.records()[1].compacted);
        let requests = requests.lock().expect("requests lock");
        assert!(
            format!("{:?}", requests[2].messages)
                .contains(crate::agents::context::COMPACTED_MARKER)
        );
    }

    #[tokio::test]
    async fn live_step_reviews_the_first_submit_answer_call_before_accepting_the_second() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let submit = || {
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            )
        };
        let (llm, requests) = live_llm_capturing(vec![submit(), submit()]);
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000)
            .with_max_turns(2)
            .with_review_before_submit();
        let artifact = LiveModelStep::new(llm)
            .propose(&config, &view, &meter)
            .await
            .expect("the second submit_answer call ends the step");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));

        let sent = requests.lock().expect("mutex is never poisoned");
        assert_eq!(
            sent.len(),
            2,
            "the review costs exactly one extra turn, not an open-ended retry"
        );
        let first_reply = format!("{:?}", sent[1].messages);
        assert!(
            first_reply.contains("name the failing operation")
                && first_reply.contains("call submit_answer again"),
            "the first submit_answer call must be answered with the review notice, not accepted: \
             {first_reply}"
        );
    }

    #[tokio::test]
    async fn live_step_accepts_a_first_submit_answer_directly_on_the_true_last_turn() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let read_call = || tool_call_turn(READ_FILE_TOOL, serde_json::json!({"path": "lib.rs"}));
        let submit = || {
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            )
        };
        let (llm, requests) = live_llm_capturing(vec![
            read_call(),
            read_call(),
            read_call(),
            read_call(),
            submit(),
        ]);
        // max_turns = 1 plus the review's own 4-turn allowance gives an
        // effective cap of 5; spending the first four turns reading reaches
        // the true last turn (index 4) as the model's first submit_answer
        // call — with no turn left to spend on a review, it must be
        // accepted directly rather than asked to redo its analysis.
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        )
        .with_max_turns(1)
        .with_review_before_submit();
        let artifact = LiveModelStep::new(llm)
            .propose(&config, &view, &meter)
            .await
            .expect("the last-turn submit_answer call ends the step directly");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
        assert_eq!(
            requests.lock().expect("mutex is never poisoned").len(),
            5,
            "no extra review turn is spent once the true last turn is reached"
        );
    }

    #[tokio::test]
    async fn live_step_keeps_a_reviewed_conversation_resumable() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let submit = || {
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            )
        };
        let executor = LiveModelStep::new(live_llm(vec![submit(), submit()]));
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000)
            .with_max_turns(2)
            .with_review_before_submit();
        let (_, conversation) = executor
            .run(&config, &view, &meter, None)
            .await
            .expect("the second submit_answer call ends the step");
        assert!(
            every_tool_call_is_answered(&conversation.0),
            "every submit_answer tool call, including the reviewed first one, must carry its \
             own tool_result so a later step can resume this conversation"
        );
    }

    #[tokio::test]
    async fn live_step_retries_after_an_invalid_submit_answer_call() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![
            // Configured for Answer, but the first call submits a Plan.
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "plan", "plan": "do it"}),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            ),
        ]));
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("the second, valid submit_answer call ends the step");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
    }

    #[tokio::test]
    async fn live_step_retries_a_patch_intent_edit_whose_find_text_is_missing() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({
                    "kind": "patch-intent",
                    "edits": [{"file": "lib.rs", "find": "not in the file", "replace": "1 + 1"}],
                }),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({
                    "kind": "patch-intent",
                    "edits": [{"file": "lib.rs", "find": "todo!()", "replace": "1 + 1"}],
                }),
            ),
        ]));
        let config = live_config(ModelStepOutputKind::PatchIntent, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("the second, valid edit ends the step");
        assert_eq!(
            artifact,
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit")
                ],
                check: None,
            }
        );
    }

    #[tokio::test]
    async fn live_step_repairs_a_last_turn_submission_after_a_direct_edit_changed_the_file() {
        let (_dir, workspace) = workspace("old");
        apply_exact_edit(workspace.config(), "lib.rs", "old", "applied")
            .expect("a prior tool changed the workspace");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind":"patch-intent", "edits":[{
                    "file":"lib.rs", "find":"old", "replace":"different"
                }]}),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind":"patch-intent", "edits":[]}),
            ),
        ]);
        let config = live_config(ModelStepOutputKind::PatchIntent, ToolSubset::None, 1_000)
            .with_max_turns(1);
        let (artifact, conversation) = LiveModelStep::new(llm)
            .run(&config, &view, &meter, None)
            .await
            .expect("one forced repair turn accepts the already applied edit");
        assert_eq!(
            artifact,
            ModelStepArtifact::PatchIntent {
                edits: Vec::new(),
                check: None,
            }
        );
        assert_eq!(workspace.read("lib.rs").expect("read"), "applied");
        assert!(every_tool_call_is_answered(&conversation.0));
        let requests = requests.lock().expect("mutex is never poisoned");
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| matches!(
            &request.tool_choice,
            Some(ToolChoice::Specific(name)) if name == SUBMIT_ANSWER_TOOL
        )));
        assert!(format!("{:?}", requests[1].messages).contains("invalid"));
    }

    #[tokio::test]
    async fn live_step_falls_back_to_valid_submitted_edits_after_one_failed_repair() {
        let (_dir, workspace) = workspace("other");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let invalid = || {
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind":"patch-intent", "edits":[
                    {"file":"lib.rs", "find":"missing", "replace":"absent"},
                    {"file":"lib.rs", "find":"other", "replace":"changed"}
                ]}),
            )
        };
        let (llm, requests) = live_llm_capturing(vec![invalid(), invalid()]);
        let config = live_config(ModelStepOutputKind::PatchIntent, ToolSubset::None, 1_000)
            .with_max_turns(1);
        let artifact = LiveModelStep::new(llm)
            .propose(&config, &view, &meter)
            .await
            .expect("exhaustion returns usable submitted edits");
        assert_eq!(
            artifact,
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "other", "changed").expect("well-formed edit")
                ],
                check: None,
            }
        );
        assert_eq!(requests.lock().expect("mutex is never poisoned").len(), 2);
    }

    #[tokio::test]
    async fn live_step_accepts_a_fenced_json_text_reply() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            "```json\n{\"kind\":\"answer\",\"answer\":\"42\"}\n```",
            10,
            20,
        )]));
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a fenced JSON reply is accepted without calling submit_answer");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
    }

    #[tokio::test]
    async fn live_step_sends_a_reminder_for_a_plain_prose_reply_then_succeeds() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            text_turn("Sure, let me think about that for a moment.", 10, 20),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            ),
        ]);
        let executor = LiveModelStep::new(llm);
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("the step recovers once submit_answer is called");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
        let sent = requests.lock().expect("mutex is never poisoned");
        let second_request = sent.get(1).expect("a second turn was requested");
        let rendered = format!("{:?}", second_request.messages);
        assert!(
            rendered.contains("submit_answer"),
            "the reminder must name the tool to call: {rendered}"
        );
    }

    #[tokio::test]
    async fn live_step_notes_how_many_turns_remain_as_the_cap_approaches() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({"path": "lib.rs"})),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            ),
        ]);
        let executor = LiveModelStep::new(llm);
        // Three total turns, one already spent: two remain, inside the
        // notice window.
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            1_000,
        )
        .with_max_turns(3);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("submit_answer on the second turn ends the step");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
        let sent = requests.lock().expect("mutex is never poisoned");
        let second_request = sent.get(1).expect("a second turn was requested");
        let rendered = format!("{:?}", second_request.messages);
        assert!(
            rendered.contains("turn(s) remain"),
            "the turns-left notice must reach the model: {rendered}"
        );
    }

    #[test]
    fn exploration_checkpoint_notice_fires_once_at_the_midpoint() {
        assert!(exploration_checkpoint_notice(10, 4).is_some());
        assert!(exploration_checkpoint_notice(10, 3).is_none());
        assert!(exploration_checkpoint_notice(10, 5).is_none());
    }

    #[test]
    fn exploration_checkpoint_notice_never_fires_for_a_turn_cap_that_has_no_midpoint() {
        assert!(exploration_checkpoint_notice(1, 0).is_none());
        assert!(exploration_checkpoint_notice(0, 0).is_none());
    }

    #[test]
    fn turn_notice_prefers_the_closing_notice_over_the_checkpoint() {
        // max_turns = 2: the midpoint (turn 0) is also inside the closing
        // window, so only the more urgent notice should ever be returned.
        let notice = turn_notice(2, 0).expect("a notice fires");
        assert!(notice.contains("turn(s) remain"), "got: {notice}");
    }

    #[test]
    fn review_before_submit_notice_covers_every_miss_shape_it_targets() {
        let with_check = review_before_submit_notice(true);
        assert!(
            with_check.contains("removes or disables no behavior"),
            "must ask the model to confirm it did not silently drop unrelated behavior: \
             {with_check}"
        );
        assert!(
            with_check.contains("a writer and its reader")
                && with_check.contains("a serializer and its parser"),
            "must ask the model to confirm a producer/consumer pair still agrees: {with_check}"
        );
        assert!(
            with_check.contains("run_python check"),
            "the run_python point must appear when the tool is offered: {with_check}"
        );

        let without_check = review_before_submit_notice(false);
        assert!(
            !without_check.contains("run_python check"),
            "the run_python point must be dropped when the tool was never offered: \
             {without_check}"
        );
    }

    #[tokio::test]
    async fn live_step_notes_an_exploration_checkpoint_at_the_turn_cap_midpoint() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let read_call = || tool_call_turn(READ_FILE_TOOL, serde_json::json!({"path": "lib.rs"}));
        let (llm, requests) = live_llm_capturing(vec![
            read_call(),
            read_call(),
            read_call(),
            read_call(),
            read_call(),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            ),
        ]);
        let executor = LiveModelStep::new(llm);
        // max_turns = 10: the midpoint falls on turn index 4 (the fifth
        // turn), well short of the closing turns-left window.
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            1_000,
        )
        .with_max_turns(10);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("submit_answer on the sixth turn ends the step");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
        let sent = requests.lock().expect("mutex is never poisoned");
        let sixth_request = sent.get(5).expect("a sixth turn was requested");
        let rendered = format!("{:?}", sixth_request.messages);
        assert!(
            rendered.contains("halfway"),
            "the midpoint checkpoint notice must reach the model: {rendered}"
        );
        assert!(
            !rendered.contains("turn(s) remain"),
            "only one notice should fire per turn: {rendered}"
        );
    }

    #[tokio::test]
    async fn live_step_notes_an_exploration_checkpoint_again_on_a_resumed_conversation() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let read_call = || tool_call_turn(READ_FILE_TOOL, serde_json::json!({"path": "lib.rs"}));
        let submit = || {
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "42"}),
            )
        };
        // One response for the round to resume (a single submit_answer
        // call), then the same five-reads-then-submit shape the fresh-call
        // checkpoint test uses, for the resumed round.
        let (llm, requests) = live_llm_capturing(vec![
            submit(),
            read_call(),
            read_call(),
            read_call(),
            read_call(),
            read_call(),
            submit(),
        ]);
        let executor = LiveModelStep::new(llm);
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            1_000,
        )
        .with_max_turns(10);
        let (_, conversation) = executor
            .run(&config, &view, &meter, None)
            .await
            .expect("the first round submits on its only turn");
        let (artifact, _) = executor
            .run(&config, &view, &meter, Some(&conversation))
            .await
            .expect("the resumed round submits on its sixth turn");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
        let sent = requests.lock().expect("mutex is never poisoned");
        // Request 0 is the first round's only turn; the resumed round's own
        // turns start at request 1, so its sixth turn (index 5) is overall
        // request 6 — the same round-local position the fresh-call test
        // checks, proving the checkpoint counts from the resumed call's own
        // turn 0, not from turns spent in the round it resumes.
        let resumed_sixth_request = sent
            .get(6)
            .expect("the resumed round's sixth turn was sent");
        let rendered = format!("{:?}", resumed_sixth_request.messages);
        assert!(
            rendered.contains("halfway"),
            "a resumed round gets its own midpoint checkpoint: {rendered}"
        );
        assert!(
            !rendered.contains("turn(s) remain"),
            "only one notice should fire per turn: {rendered}"
        );
    }

    #[tokio::test]
    async fn live_step_refuses_when_the_turn_exceeds_its_token_budget() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"answer","answer":"42"}"#,
            20,
            20,
        )]));
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 10);
        let error = executor
            .propose(&config, &view, &meter)
            .await
            .expect_err("this one turn's own 40 tokens exceeds a budget of 10");
        assert!(
            matches!(&error, SystemError::ExecutionError(message) if message.contains("exceeded its token budget")),
            "unexpected error: {error:?}"
        );
        // The call that overspent is still billed: a refusal is an auditable
        // record of a real call, not a silent discard of one.
        assert_eq!(meter.input_tokens(), 20);
        assert_eq!(meter.output_tokens(), 20);
    }

    #[tokio::test]
    async fn live_step_refuses_a_tool_round_trip_turn_that_exceeds_its_own_budget() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        // Only one response is queued: if the executor still attempted
        // another request after this turn's own context already exceeded
        // budget, the mock provider would refuse for an unrelated reason ("no
        // more queued responses") instead of the budget refusal this asserts
        // on — proving the refusal happens on this turn, not a later one.
        let executor = LiveModelStep::new(live_llm(vec![tool_call_turn_with_usage(
            READ_FILE_TOOL,
            serde_json::json!({ "path": "lib.rs" }),
            10,
            10,
        )]));
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            15, // this turn alone bills 10 + 10 = 20, over a 15 budget
        );
        let error = executor
            .propose(&config, &view, &meter)
            .await
            .expect_err("a single turn spending more than its own budget must be refused");
        assert!(
            matches!(&error, SystemError::ExecutionError(message) if message.contains("exceeded its token budget")),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn live_step_succeeds_when_turns_individually_fit_but_their_sum_would_not() {
        // Five turns at 20k in + 20k out each (40k/turn, under a 50k budget)
        // sum to 200k — well past that same 50k budget. A cumulative-spend
        // check would refuse this; bounding each turn's own context instead
        // lets a step finish exploration that simply took several small turns.
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let responses = vec![
            tool_call_turn_with_usage(
                READ_FILE_TOOL,
                serde_json::json!({ "path": "lib.rs" }),
                20_000,
                20_000,
            ),
            tool_call_turn_with_usage(
                READ_FILE_TOOL,
                serde_json::json!({ "path": "lib.rs" }),
                20_000,
                20_000,
            ),
            tool_call_turn_with_usage(
                READ_FILE_TOOL,
                serde_json::json!({ "path": "lib.rs" }),
                20_000,
                20_000,
            ),
            tool_call_turn_with_usage(
                READ_FILE_TOOL,
                serde_json::json!({ "path": "lib.rs" }),
                20_000,
                20_000,
            ),
            text_turn(r#"{"kind":"answer","answer":"42"}"#, 20_000, 20_000),
        ];
        let executor = LiveModelStep::new(live_llm(responses));
        let config = ModelStepConfig::new(
            "revise".to_owned(),
            "do the configured thing".to_owned(),
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            TokenBudget(50_000),
            ModelStepOutputKind::Answer,
        )
        .with_max_turns(5);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("no single turn exceeds the budget, even though their sum does");
        assert_eq!(artifact, ModelStepArtifact::Answer("42".to_owned()));
        // The session ledger still records everything billed across all 5
        // turns — only the step's own refusal check stopped summing.
        assert_eq!(meter.input_tokens(), 100_000);
        assert_eq!(meter.output_tokens(), 100_000);
    }

    #[tokio::test]
    async fn live_step_caps_every_request_output_at_the_turn_budget() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        // Both requests are capped at the same 100: the bound is per turn,
        // not a shrinking remainder of a cumulative spend.
        let (llm, output_caps) = live_llm_recording(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            text_turn(r#"{"kind":"answer","answer":"42"}"#, 5, 5),
        ]);
        let executor = LiveModelStep::new(llm);
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            100,
        );
        executor
            .propose(&config, &view, &meter)
            .await
            .expect("each turn's own 10 tokens fit a budget of 100");
        assert_eq!(
            *output_caps.lock().expect("mutex is never poisoned"),
            vec![Some(100), Some(100)]
        );
    }

    #[tokio::test]
    async fn live_step_requests_cache_prefix_and_a_stable_breakpoint_across_turns() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            text_turn(r#"{"kind":"answer","answer":"42"}"#, 5, 5),
        ]);
        let executor = LiveModelStep::new(llm);
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            1_000,
        );
        executor
            .propose(&config, &view, &meter)
            .await
            .expect("a two-turn tool round trip succeeds");
        let requests = requests.lock().expect("mutex is never poisoned");
        assert_eq!(requests.len(), 2, "one request per turn");
        for request in requests.iter() {
            assert!(
                request.cache.prefix,
                "every request caches the stable tool-definition prefix"
            );
        }
        // The stable breakpoint (this step's own turn start, index 0) is the
        // same on both calls; only the rolling breakpoint (the latest
        // message) advances as the round trip's history grows — the same
        // byte-stable-prefix guarantee `CacheAwareWindow` gives the main loop.
        assert_eq!(requests[0].cache.breakpoints, vec![0]);
        assert_eq!(requests[1].cache.breakpoints, vec![0, 2]);
    }

    #[tokio::test]
    async fn batched_trim_reduces_scripted_prompt_cost_and_preserves_recent_results() {
        fn estimate(requests: &[LlmRequest]) -> (Vec<usize>, f64) {
            let mut previous: Option<&LlmRequest> = None;
            let mut cost = 0.0;
            let mut sizes = Vec::new();
            for request in requests {
                let total = message_chars(&request.messages);
                let mut cached = 0;
                if let Some(prior) = previous {
                    for (index, (old, new)) in
                        prior.messages.iter().zip(&request.messages).enumerate()
                    {
                        if serde_json::to_value(old).expect("message serializes")
                            != serde_json::to_value(new).expect("message serializes")
                        {
                            break;
                        }
                        // Count only prefixes that ended at a breakpoint on
                        // the preceding request. The provider may read less;
                        // this avoids assuming arbitrary prefixes are cached.
                        if prior.cache.breakpoints.contains(&index) {
                            cached = message_chars(&request.messages[..=index]);
                        }
                    }
                }
                // Sonnet 5 list prices: $0.2/M cache read, $2.5/M write.
                let read = cached as f64 * MODEL_STEP_TOKEN_RATIO;
                let write = (total - cached) as f64 * MODEL_STEP_TOKEN_RATIO;
                cost += (read * 0.2 + write * 2.5) / 1_000_000.0;
                sizes.push((total as f64 * MODEL_STEP_TOKEN_RATIO) as usize);
                previous = Some(request);
            }
            (sizes, cost)
        }

        async fn run(trim: TrimPolicy) -> Vec<LlmRequest> {
            let (dir, workspace) = workspace("todo!()");
            let view = WorkspaceView::new(&workspace);
            let meter = TokenMeter::default();
            let large = "x".repeat(32 * 1024);
            let mut responses = Vec::new();
            for index in 0..20 {
                let name = format!("read{index}.rs");
                std::fs::write(dir.path().join(&name), &large).expect("fixture file");
                responses.push(tool_call_turn(
                    READ_FILE_TOOL,
                    serde_json::json!({ "path": name }),
                ));
            }
            responses.push(text_turn(r#"{"kind":"answer","answer":"done"}"#, 5, 5));
            let (llm, requests) = live_llm_capturing(responses);
            let config = ModelStepConfig::new(
                "loop".to_owned(),
                "solve the task".to_owned(),
                ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
                TokenBudget(1_000_000),
                ModelStepOutputKind::Answer,
            )
            .with_max_turns(30)
            .with_trim(trim);
            LiveModelStep::new(llm)
                .run(&config, &view, &meter, None)
                .await
                .expect("scripted conversation succeeds");
            requests.lock().expect("mutex is never poisoned").clone()
        }

        let plain = run(TrimPolicy::None).await;
        let trimmed = run(TrimPolicy::Batched).await;
        let (plain_sizes, plain_cost) = estimate(&plain);
        let (trimmed_sizes, trimmed_cost) = estimate(&trimmed);
        eprintln!(
            "scripted prompt tokens/turn: plain={plain_sizes:?} trimmed={trimmed_sizes:?}; cache input cost: plain=${plain_cost:.4} trimmed=${trimmed_cost:.4}"
        );
        assert_eq!(plain_sizes.len(), 21);
        assert_eq!(trimmed_sizes.len(), 21);
        assert!(trimmed_sizes.iter().sum::<usize>() < plain_sizes.iter().sum::<usize>());
        assert!(
            trimmed_cost < plain_cost,
            "batched trim must repay the cache write"
        );
        assert_eq!(
            serde_json::to_value(&trimmed[0].messages[0]).expect("message serializes"),
            serde_json::to_value(&plain[0].messages[0]).expect("message serializes")
        );
        assert!(trimmed.last().expect("last request").cache.prefix);
        let last = &trimmed.last().expect("last request").messages;
        let results: Vec<_> = last
            .iter()
            .flat_map(|message| match message {
                Message::User { content } => content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::ToolResult(result) => Some(result),
                        _ => None,
                    })
                    .collect(),
                Message::Assistant { .. } => Vec::new(),
            })
            .collect();
        assert!(results.iter().take(3).any(|result| matches!(&result.content, ToolResultContent::Text(text) if text.starts_with("[trimmed "))));
        assert!(results.iter().rev().take(TRIM_KEEP_RESULTS).all(|result| matches!(&result.content, ToolResultContent::Text(text) if !text.starts_with("[trimmed "))));
    }

    #[tokio::test]
    async fn a_trimmed_file_read_can_be_retrieved_again() {
        let (dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let mut responses = Vec::new();
        for index in 0..8 {
            let name = format!("read{index}.rs");
            std::fs::write(dir.path().join(&name), "x".repeat(32 * 1024)).expect("fixture file");
            responses.push(tool_call_turn(
                READ_FILE_TOOL,
                serde_json::json!({ "path": name }),
            ));
        }
        responses.push(tool_call_turn(
            READ_FILE_TOOL,
            serde_json::json!({ "path": "read0.rs" }),
        ));
        responses.push(text_turn(r#"{"kind":"answer","answer":"done"}"#, 5, 5));
        let (llm, requests) = live_llm_capturing(responses);
        let config = ModelStepConfig::new(
            "loop".to_owned(),
            "solve".to_owned(),
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            TokenBudget(1_000_000),
            ModelStepOutputKind::Answer,
        )
        .with_max_turns(30)
        .with_trim(TrimPolicy::Batched);
        LiveModelStep::new(llm)
            .run(&config, &view, &meter, None)
            .await
            .expect("scripted run");
        let requests = requests.lock().expect("mutex is never poisoned");
        let messages = &requests
            .last()
            .expect("request after repeated read")
            .messages;
        let outputs: Vec<_> = messages
            .iter()
            .flat_map(|message| match message {
                Message::User { content } => content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::ToolResult(result) => match &result.content {
                            ToolResultContent::Text(text) => Some(text.as_str()),
                            ToolResultContent::Image(_) => None,
                        },
                        _ => None,
                    })
                    .collect(),
                Message::Assistant { .. } => Vec::new(),
            })
            .collect();
        assert!(outputs[0].starts_with("[trimmed "));
        assert!(
            outputs
                .last()
                .expect("re-read result")
                .contains(&"x".repeat(1_000))
        );
    }

    #[tokio::test]
    async fn resumed_lean_conversation_trims_first_tier_results() {
        let (dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::new();
        let config = ModelStepConfig::new(
            "lean".to_owned(),
            "solve".to_owned(),
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            TokenBudget(1_000_000),
            ModelStepOutputKind::Answer,
        )
        .with_max_turns(30)
        .with_trim(TrimPolicy::Batched);
        let mut first = Vec::new();
        for index in 0..7 {
            let path = format!("before{index}.rs");
            std::fs::write(dir.path().join(&path), "x".repeat(32 * 1024)).unwrap();
            first.push(tool_call_turn(
                READ_FILE_TOOL,
                serde_json::json!({ "path": path }),
            ));
        }
        first.push(text_turn(r#"{"kind":"answer","answer":"continue"}"#, 5, 5));
        let (first_llm, _) = live_llm_capturing(first);
        let (_, conversation) = LiveModelStep::new(first_llm)
            .run(&config, &view, &meter, None)
            .await
            .unwrap();
        let mut second = Vec::new();
        for index in 0..10 {
            let path = format!("after{index}.rs");
            std::fs::write(dir.path().join(&path), "y".repeat(32 * 1024)).unwrap();
            second.push(tool_call_turn(
                READ_FILE_TOOL,
                serde_json::json!({ "path": path }),
            ));
        }
        second.push(text_turn(r#"{"kind":"answer","answer":"done"}"#, 5, 5));
        let (second_llm, requests) = live_llm_capturing(second);
        LiveModelStep::new(second_llm)
            .run(&config, &view, &meter, Some(&conversation))
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        let last = &requests.last().unwrap().messages;
        assert!(last.iter().any(|message| match message {
            Message::User { content } => content.iter().any(|block| match block {
                UserBlock::ToolResult(result) => matches!(
                    &result.content,
                    ToolResultContent::Text(text)
                        if text.starts_with("[trimmed ") && text.contains("before0.rs")
                ),
                _ => false,
            }),
            Message::Assistant { .. } => false,
        }));
    }

    #[tokio::test]
    async fn live_step_compacts_old_tool_results_once_the_window_grows_past_the_threshold() {
        // Each file is bigger than one `read_file` call's own cap, so fourteen
        // read_file round trips (~32KB of tool-result text apiece) push the
        // growing turn history past `MODEL_STEP_COMPACT_AFTER_TOKENS` at the
        // step's uncalibrated 0.3 ratio, the same trigger `CacheAwareWindow`
        // compacts against. Each read targets its own distinct file, rather
        // than rereading the same one, so the duplicate-read note (see
        // `live_step_returns_a_short_note_for_a_repeated_identical_read`)
        // does not collapse them into cheap repeats before compaction has
        // anything to do.
        let (dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let large = "x".repeat(64 * 1024);
        let mut responses: Vec<LlmResponse> = (0..14)
            .map(|index| {
                let name = format!("big{index}.rs");
                std::fs::write(dir.path().join(&name), &large).expect("fixture file");
                tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": name }))
            })
            .collect();
        responses.push(text_turn(r#"{"kind":"answer","answer":"done"}"#, 10, 10));
        let (llm, requests) = live_llm_capturing(responses);
        let executor = LiveModelStep::new(llm);
        let config = ModelStepConfig::new(
            "revise".to_owned(),
            "do the configured thing".to_owned(),
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            TokenBudget(1_000_000),
            ModelStepOutputKind::Answer,
        )
        .with_max_turns(20);
        let provider = ModelStepProvider::new(executor);
        let handoff = ModelStepHandoff::default();
        let artifact = run_with_handoff(&config, &provider, &view, &meter, &handoff)
            .await
            .expect("compaction keeps the growing history from ballooning past budget");
        assert_eq!(artifact, ModelStepArtifact::Answer("done".to_owned()));
        assert!(
            handoff.records()[0].compacted,
            "the handoff records compaction"
        );

        let requests = requests.lock().expect("mutex is never poisoned");
        let last_sent = &requests
            .last()
            .expect("at least one request was sent")
            .messages;
        let stubbed = last_sent
            .iter()
            .filter(|message| {
                matches!(message, Message::User { content } if content.iter().any(|block| matches!(
                    block,
                    UserBlock::ToolResult(result) if matches!(
                        &result.content,
                        ToolResultContent::Text(text) if text.starts_with(crate::agents::context::COMPACTED_MARKER)
                    )
                )))
            })
            .count();
        assert!(
            stubbed > 0,
            "some of the oldest tool results were stubbed once the window grew"
        );
        // The most recent messages (within `DEFAULT_COMPACT_KEEP_RECENT`) are
        // never touched, so the model's latest observation stays readable.
        let tail = &last_sent[last_sent.len().saturating_sub(2)..];
        assert!(
            tail.iter().all(|message| match message {
                Message::User { content } => content.iter().all(|block| !matches!(
                    block,
                    UserBlock::ToolResult(result) if matches!(
                        &result.content,
                        ToolResultContent::Text(text) if text.starts_with(crate::agents::context::COMPACTED_MARKER)
                    )
                )),
                Message::Assistant { .. } => true,
            }),
            "the most recent tool results stay intact"
        );
    }

    #[test]
    fn decode_refuses_a_budget_outside_its_bounds() {
        let zero = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 0)
            .encode()
            .expect("a well-formed configuration encodes");
        let error = ModelStepConfig::decode(&zero).expect_err("a zero budget must be refused");
        assert!(
            error.to_string().contains("budget must be in 1..="),
            "unexpected error: {error}"
        );

        let too_many = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::None,
            MAX_MODEL_STEP_TOKEN_BUDGET + 1,
        )
        .encode()
        .expect("a well-formed configuration encodes");
        let error =
            ModelStepConfig::decode(&too_many).expect_err("an over-ceiling budget is refused");
        assert!(
            error.to_string().contains("budget must be in 1..="),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn decode_accepts_the_maximum_budget() {
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::None,
            MAX_MODEL_STEP_TOKEN_BUDGET,
        )
        .encode()
        .expect("a well-formed configuration encodes");
        let decoded =
            ModelStepConfig::decode(&config).expect("the maximum budget itself is accepted");
        assert_eq!(decoded.budget.0, MAX_MODEL_STEP_TOKEN_BUDGET);
    }

    #[test]
    fn decode_refuses_a_turn_cap_outside_its_bounds() {
        let too_many = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000)
            .with_max_turns(MAX_MODEL_STEP_TURNS_CEILING + 1)
            .encode()
            .expect("a well-formed configuration encodes");
        let error =
            ModelStepConfig::decode(&too_many).expect_err("an over-ceiling turn cap is refused");
        assert!(
            error.to_string().contains("max_turns must be in"),
            "unexpected error: {error}"
        );

        let zero = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000)
            .with_max_turns(0)
            .encode()
            .expect("a well-formed configuration encodes");
        assert!(
            ModelStepConfig::decode(&zero).is_err(),
            "zero turns is refused"
        );
    }

    #[test]
    fn encode_then_decode_round_trips_the_turn_cap_and_prior_failure_flag() {
        let original = live_config(ModelStepOutputKind::PatchIntent, ToolSubset::None, 1_000)
            .with_max_turns(30)
            .with_prior_failure_visible()
            .with_review_before_submit();
        let decoded = ModelStepConfig::decode(
            &original
                .encode()
                .expect("a well-formed configuration encodes"),
        )
        .expect("a well-formed configuration decodes");
        assert_eq!(decoded.max_turns, 30);
        assert!(decoded.sees_prior_failure);
        assert!(decoded.review_before_submit);
    }

    #[tokio::test]
    async fn twm_short_thirty_turn_step_never_rebuilds() {
        let (_dir, workspace) = workspace("todo!()");
        let mut config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec!["read_file".to_owned()]),
            u32::MAX,
        )
        .with_max_turns(30);
        config.twm = Some(twm::TwmOptions::default());
        let mut replies = (0..29)
            .map(|line| {
                tool_call_turn(
                    "read_file",
                    serde_json::json!({"path": "lib.rs", "start_line": line}),
                )
            })
            .collect::<Vec<_>>();
        replies.push(tool_call_turn(
            SUBMIT_ANSWER_TOOL,
            serde_json::json!({"kind": "answer", "answer": "done"}),
        ));
        let (llm, requests) = live_llm_capturing(replies);
        LiveModelStep::new(llm)
            .propose(
                &config,
                &WorkspaceView::new(&workspace),
                &TokenMeter::default(),
            )
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 30);
        for (turn, request) in requests.iter().enumerate() {
            assert_eq!(request.messages.len(), 1 + 2 * turn);
            assert_eq!(
                serde_json::to_value(&request.messages[0]).unwrap(),
                serde_json::to_value(&requests[0].messages[0]).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn twm_stays_append_only_between_rebuilds_and_rebuilds_only_at_the_trigger() {
        // The mixed approach end to end, through the real live turn loop:
        // between triggers the transcript only ever grows (the first message
        // is byte-identical across turns), and a rebuild happens exactly
        // once, exactly when the whole prompt passes the size limit — never every
        // turn — and shrinks history rather than keeping all of it. See
        // `twm`.
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let mut config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec!["read_file".to_owned()]),
            u32::MAX,
        )
        .with_max_turns(6);
        config.twm = Some(twm::TwmOptions::default());
        let read_at = |line: u64| {
            let mut response = tool_call_turn(
                "read_file",
                serde_json::json!({"path": "lib.rs", "start_line": line}),
            );
            response
                .content
                .insert(0, AssistantBlock::text("r".repeat(41_000)));
            response
        };
        let (llm, requests) = live_llm_capturing(vec![
            read_at(0),
            read_at(1),
            read_at(2),
            read_at(3),
            tool_call_turn(
                "read_file",
                serde_json::json!({"path": "lib.rs", "start_line": 4}),
            ),
            tool_call_turn(
                SUBMIT_ANSWER_TOOL,
                serde_json::json!({"kind": "answer", "answer": "done"}),
            ),
        ]);
        LiveModelStep::new(llm)
            .propose(&config, &view, &TokenMeter::default())
            .await
            .expect("the scripted turns end the step");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert_eq!(
            serde_json::to_value(&requests[4].messages[0]).unwrap(),
            serde_json::to_value(&requests[5].messages[0]).unwrap(),
            "the next short turn must not rebuild"
        );
        assert_eq!(
            requests[0].messages.len(),
            1,
            "turn 0: just the instruction"
        );
        for turn in 1..4 {
            assert_eq!(
                format!("{:?}", requests[turn].messages[0]),
                format!("{:?}", requests[0].messages[0]),
                "append-only below the 40,000-token size limit: the first message stays untouched on turn {turn}"
            );
        }
        assert_eq!(
            requests[3].messages.len(),
            7,
            "turn 3: appended three times over, not yet rebuilt"
        );
        let rebuilt_context = format!("{:?}", requests[4].messages[0]);
        assert_ne!(
            rebuilt_context,
            format!("{:?}", requests[3].messages[0]),
            "turn 4 crosses the 40,000-token size limit: the context message changes"
        );
        assert!(
            rebuilt_context.contains("Working set"),
            "the rebuilt context is the typed render, not raw chat: {rebuilt_context}"
        );
        assert!(
            requests[4].messages.len() < requests[3].messages.len(),
            "a rebuild must shrink history ({} was not < {})",
            requests[4].messages.len(),
            requests[3].messages.len()
        );
        let kept_tail = format!("{:?}", &requests[4].messages[1..]);
        assert!(
            !kept_tail.contains("Number(0)") && !kept_tail.contains("Number(1)"),
            "the two oldest reads must have been dropped, not kept forever: {kept_tail}"
        );
        assert!(
            kept_tail.contains("Number(2)") && kept_tail.contains("Number(3)"),
            "the two most recent reads must survive the rebuild: {kept_tail}"
        );
        assert!(
            matches!(requests[4].messages[1], Message::Assistant { .. }),
            "the kept tail must start at an assistant message, never an orphaned tool result"
        );
    }

    #[test]
    fn trim_is_opt_in_and_only_enabled_steps_change_the_plan_configuration() {
        let plain = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        let encoded_plain = plain.encode().expect("plain configuration encodes");
        assert_eq!(
            ModelStepConfig::decode(&encoded_plain)
                .expect("decodes")
                .trim,
            TrimPolicy::None
        );
        let trimmed = plain.with_trim(TrimPolicy::Batched);
        let encoded_trimmed = trimmed.encode().expect("trimmed configuration encodes");
        assert_ne!(encoded_plain, encoded_trimmed);
        assert_eq!(
            ModelStepConfig::decode(&encoded_trimmed)
                .expect("decodes")
                .trim,
            TrimPolicy::Batched
        );
    }

    #[test]
    fn with_prior_failure_folds_the_captured_output_into_the_instruction() {
        let config = live_config(ModelStepOutputKind::PatchIntent, ToolSubset::None, 1_000);
        let outcome = TestRunOutcome {
            status: Status::Issue,
            output: "AssertionError: expected 5, got -1".to_owned(),
            check_failure: None,
        };
        let augmented = with_prior_failure(&config, &outcome);
        assert!(
            augmented
                .instruction
                .contains("AssertionError: expected 5, got -1"),
            "the captured failure text must reach the instruction: {}",
            augmented.instruction
        );
        assert!(augmented.instruction.starts_with(&config.instruction));
    }

    #[test]
    fn with_prior_failure_still_says_something_when_nothing_was_captured() {
        let config = live_config(ModelStepOutputKind::PatchIntent, ToolSubset::None, 1_000);
        let outcome = TestRunOutcome {
            status: Status::Issue,
            output: String::new(),
            check_failure: None,
        };
        let augmented = with_prior_failure(&config, &outcome);
        assert!(
            augmented.instruction.contains("did not pass"),
            "an empty capture must still say the attempt failed: {}",
            augmented.instruction
        );
    }

    #[test]
    fn python_script_normalizes_only_fully_escaped_lines() {
        assert_eq!(
            normalize_python_script("import os\\nassert os.name"),
            "import os\nassert os.name"
        );
        assert_eq!(
            normalize_python_script("print('a\\nb')\nprint('ok')"),
            "print('a\\nb')\nprint('ok')"
        );
    }

    #[tokio::test]
    async fn read_file_bounded_truncates_a_file_larger_than_the_cap() {
        let (dir, workspace) = workspace("todo!()");
        let large = "line\n".repeat(20_000);
        std::fs::write(dir.path().join("lib.rs"), &large).expect("overwrite with a large file");
        let content = read_file_bounded(workspace.config(), "lib.rs", None, None)
            .await
            .expect("a large in-sandbox file still reads");
        assert!(
            content.len() <= MAX_READ_BYTES + 200,
            "response was not capped: {} bytes",
            content.len()
        );
        assert!(
            content.contains("truncated at the output cap"),
            "missing truncation marker: {content}"
        );
    }

    #[tokio::test]
    async fn read_file_bounded_refuses_a_path_that_escapes_the_sandbox() {
        let (_dir, workspace) = workspace("todo!()");
        let error = read_file_bounded(workspace.config(), "../../etc/passwd", None, None)
            .await
            .expect_err("a path outside the sandbox must be refused");
        assert!(
            matches!(error, EditError::OutsideSandbox { .. }),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn read_file_bounded_returns_a_numbered_range_with_a_resume_footer() {
        let (dir, workspace) = workspace("todo!()");
        let body: String = (1..=20).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("lib.rs"), body).expect("fixture file");

        let page = read_file_bounded(workspace.config(), "lib.rs", Some(5), Some(3))
            .await
            .expect("a ranged read succeeds");
        assert!(page.contains("5\tline5"), "got: {page}");
        assert!(page.contains("7\tline7"), "got: {page}");
        assert!(
            !page.contains("line4") && !page.contains("\tline8"),
            "got: {page}"
        );
        assert!(
            page.contains("of 20") && page.contains("read_file(path=\"lib.rs\", offset=8)"),
            "a range short of the end must say the total and how to continue: {page}"
        );

        let past_eof = read_file_bounded(workspace.config(), "lib.rs", Some(100), None)
            .await
            .expect("a range past the end still answers");
        assert!(
            past_eof.contains("has 20 lines"),
            "an out-of-range start must say how long the file actually is: {past_eof}"
        );
    }

    #[tokio::test]
    async fn list_dir_bounded_stops_at_the_entry_cap() {
        let (dir, workspace) = workspace("todo!()");
        for index in 0..MAX_LIST_DIR_ENTRIES + 5 {
            std::fs::write(dir.path().join(format!("file-{index:04}.txt")), "x")
                .expect("fixture file");
        }
        let listing = list_dir_bounded(workspace.config(), ".")
            .await
            .expect("directory reads");
        assert_eq!(
            listing
                .lines()
                .filter(|line| !line.starts_with('…'))
                .count(),
            MAX_LIST_DIR_ENTRIES,
            "more entries were listed than the cap allows"
        );
        assert!(
            listing.contains("stopped at"),
            "missing truncation marker: {listing}"
        );
    }

    #[tokio::test]
    async fn list_dir_bounded_refuses_a_path_that_escapes_the_sandbox() {
        let (_dir, workspace) = workspace("todo!()");
        let error = list_dir_bounded(workspace.config(), "../../etc")
            .await
            .expect_err("a path outside the sandbox must be refused");
        assert!(
            error.contains("escapes the sandbox"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn search_bounded_stops_at_the_match_cap() {
        let (dir, workspace) = workspace("todo!()");
        let haystack = "needle\n".repeat(MAX_SEARCH_MATCHES + 10);
        std::fs::write(dir.path().join("haystack.txt"), haystack).expect("fixture file");
        let result = search_bounded(workspace.config(), "needle", None, true, false, 0, false)
            .await
            .expect("search runs");
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(
            lines.iter().filter(|line| !line.starts_with('…')).count(),
            MAX_SEARCH_MATCHES,
            "more matches were reported than the cap allows: {result}"
        );
        let last = *lines.last().expect("a capped result still has a footer");
        assert!(
            last.contains("stopped at") && last.contains("narrow"),
            "a match cap must say so explicitly, not a bare list: {result}"
        );
    }

    #[tokio::test]
    async fn search_bounded_caps_the_rendered_result_bytes() {
        let (dir, workspace) = workspace("todo!()");
        let long_line = "x".repeat(500);
        let haystack = format!("{long_line}\n").repeat(50);
        std::fs::write(dir.path().join("long.txt"), haystack).expect("fixture file");
        let result = search_bounded(workspace.config(), "x+", None, false, false, 0, false)
            .await
            .expect("search runs");
        assert!(
            result.len() <= MAX_SEARCH_RESULT_BYTES + 32,
            "response was not capped: {} bytes",
            result.len()
        );
        assert!(
            result.contains("truncated"),
            "missing truncation marker: {result}"
        );
    }

    #[tokio::test]
    async fn search_bounded_refuses_an_invalid_pattern_naming_the_fault() {
        let (_dir, workspace) = workspace("todo!()");
        let error = search_bounded(
            workspace.config(),
            "(unclosed",
            None,
            false,
            false,
            0,
            false,
        )
        .await
        .expect_err("an invalid regular expression must be refused");
        assert!(
            error.contains("not a valid regular expression"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn search_bounded_scopes_to_a_path() {
        let (dir, workspace) = workspace("todo!()");
        std::fs::create_dir(dir.path().join("sub")).expect("fixture dir");
        std::fs::write(dir.path().join("sub/needle.txt"), "needle here\n").expect("fixture file");
        std::fs::write(dir.path().join("needle.txt"), "needle here too\n").expect("fixture file");

        let scoped = search_bounded(
            workspace.config(),
            "needle",
            Some("sub"),
            true,
            false,
            0,
            false,
        )
        .await
        .expect("a scoped search runs");
        assert!(scoped.contains("sub/needle.txt"), "got: {scoped}");
        assert!(
            !scoped.contains("\nneedle.txt") && !scoped.starts_with("needle.txt"),
            "a path-scoped search must not report a match outside that path: {scoped}"
        );
    }

    #[tokio::test]
    async fn search_bounded_refuses_a_path_that_escapes_the_sandbox() {
        let (_dir, workspace) = workspace("todo!()");
        let error = search_bounded(
            workspace.config(),
            "needle",
            Some("../../etc"),
            true,
            false,
            0,
            false,
        )
        .await
        .expect_err("a path outside the sandbox must be refused");
        assert!(error.contains("escapes the sandbox"), "got: {error}");
    }

    #[tokio::test]
    async fn search_bounded_includes_the_requested_context_lines() {
        let (dir, workspace) = workspace("todo!()");
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("context.txt"), body).expect("fixture file");

        let result = search_bounded(workspace.config(), "line5", None, true, false, 2, false)
            .await
            .expect("search runs");
        // The match line itself is marked with ':', context lines with '-',
        // mirroring the ordinary grep convention.
        assert!(result.contains("context.txt:5: line5"), "got: {result}");
        assert!(result.contains("context.txt-3: line3"), "got: {result}");
        assert!(result.contains("context.txt-7: line7"), "got: {result}");
        assert!(!result.contains("line2"), "got: {result}");
        assert!(!result.contains("line8"), "got: {result}");
    }

    #[tokio::test]
    async fn search_bounded_clamps_context_lines_above_the_cap() {
        let (dir, workspace) = workspace("todo!()");
        let body: String = (1..=30).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("context.txt"), body).expect("fixture file");

        let result = search_bounded(
            workspace.config(),
            "line15",
            None,
            true,
            false,
            MAX_SEARCH_CONTEXT_LINES + 10,
            false,
        )
        .await
        .expect("an out-of-range context_lines is clamped, not refused");
        assert!(
            result.contains("clamped"),
            "a clamped context_lines must say so: {result}"
        );
        assert!(result.contains("context.txt:15: line15"), "got: {result}");
        assert!(result.contains("context.txt-10: line10"), "got: {result}");
        assert!(result.contains("context.txt-20: line20"), "got: {result}");
        assert!(
            !result.contains("line9") && !result.contains("line21"),
            "context_lines must clamp to the max ({MAX_SEARCH_CONTEXT_LINES}), not the \
             requested value: {result}"
        );
    }

    #[tokio::test]
    async fn search_bounded_notes_that_literal_mode_does_not_treat_pipe_as_or() {
        let (_dir, workspace) = workspace("todo!()");
        let result = search_bounded(
            workspace.config(),
            "alpha|beta",
            None,
            true,
            false,
            0,
            false,
        )
        .await
        .expect("search runs");
        assert!(result.contains("no matches"), "got: {result}");
        assert!(
            result.contains("not treated as OR"),
            "a literal search with regex-looking syntax and no matches should explain that \
             literal mode does not interpret it: {result}"
        );
    }

    #[tokio::test]
    async fn search_bounded_does_not_warn_about_literal_syntax_when_matches_are_found() {
        let (dir, workspace) = workspace("todo!()");
        std::fs::write(dir.path().join("pipe.txt"), "alpha|beta\n").expect("fixture file");
        let result = search_bounded(
            workspace.config(),
            "alpha|beta",
            None,
            true,
            false,
            0,
            false,
        )
        .await
        .expect("search runs");
        assert!(result.contains("pipe.txt"), "got: {result}");
        assert!(
            !result.contains("not treated as OR"),
            "a real match needs no warning about literal mode: {result}"
        );
    }

    #[tokio::test]
    async fn search_bounded_files_only_lists_matching_paths_without_lines() {
        let (dir, workspace) = workspace("needle in lib.rs too\n");
        std::fs::write(dir.path().join("a.txt"), "needle\nneedle\nneedle\n").expect("fixture file");
        std::fs::write(dir.path().join("b.txt"), "nothing here\n").expect("fixture file");

        let result = search_bounded(workspace.config(), "needle", None, true, false, 0, true)
            .await
            .expect("search runs");
        let paths: std::collections::BTreeSet<&str> = result
            .lines()
            .filter(|line| !line.starts_with('…'))
            .collect();
        assert_eq!(
            paths,
            std::collections::BTreeSet::from(["a.txt", "lib.rs"]),
            "files_only must list each matching path once, with no line numbers: {result}"
        );
    }

    #[tokio::test]
    async fn search_bounded_reports_no_matches_by_scanned_file_count() {
        let (_dir, workspace) = workspace("todo!()");
        let result = search_bounded(workspace.config(), "absent", None, true, false, 0, false)
            .await
            .expect("search runs");
        assert!(
            result.contains("no matches in 1 file(s)"),
            "an honest empty result names how many files it actually looked in: {result}"
        );
    }

    #[tokio::test]
    async fn live_step_returns_a_truncated_tool_result_for_a_large_file() {
        let large = "line\n".repeat(20_000);
        let (_dir, workspace) = workspace(&large);
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            text_turn(r#"{"kind":"answer","answer":"done"}"#, 10, 10),
        ]));
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        );
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a capped tool result still lets the turn finish");
        assert_eq!(artifact, ModelStepArtifact::Answer("done".to_owned()));
    }

    #[tokio::test]
    async fn live_step_refuses_a_disallowed_tool_call() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            text_turn(r#"{"kind":"answer","answer":"done"}"#, 10, 10),
        ]);
        // No tools in the configured subset, so the read-only tool the model
        // called anyway is outside its granted reach.
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 10_000);
        let artifact = LiveModelStep::new(llm)
            .propose(&config, &view, &meter)
            .await
            .expect("a refused tool call is reported to the model, not fatal");
        assert_eq!(artifact, ModelStepArtifact::Answer("done".to_owned()));
        let sent = format!("{:?}", requests.lock().unwrap()[1].messages);
        assert!(
            sent.contains("disallowed tool 'read_file'") && !sent.contains("todo!()"),
            "the model is told the call was refused and the file is not read: {sent}"
        );
    }

    #[tokio::test]
    async fn live_step_permits_an_allowed_tool_call_then_answers() {
        let (_dir, workspace) = workspace("hello world");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            text_turn(r#"{"kind":"answer","answer":"hello world"}"#, 10, 10),
        ]));
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        );
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("the allowed tool round-trips, then the model answers");
        assert_eq!(
            artifact,
            ModelStepArtifact::Answer("hello world".to_owned())
        );
    }

    #[tokio::test]
    async fn live_step_returns_a_short_note_for_a_repeated_identical_read() {
        let (_dir, workspace) = workspace("hello world");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            text_turn(r#"{"kind":"answer","answer":"done"}"#, 10, 10),
        ]);
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        )
        .with_max_turns(3);
        let artifact = LiveModelStep::new(llm)
            .propose(&config, &view, &meter)
            .await
            .expect("a repeated identical read still lets the turn finish");
        assert_eq!(artifact, ModelStepArtifact::Answer("done".to_owned()));

        // The third request's own message history carries both earlier tool
        // results: the first read's real content, then the second (identical)
        // read's result, which must be the short duplicate note instead of
        // the file text again.
        let requests = requests
            .lock()
            .expect("mock provider mutex is never poisoned");
        let mut tool_results = requests[2]
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => Some(content.iter()),
                Message::Assistant { .. } => None,
            })
            .flatten()
            .filter_map(|block| match block {
                UserBlock::ToolResult(result) => match &result.content {
                    ToolResultContent::Text(text) => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            });
        let first_read = tool_results.next().expect("the first read's own result");
        assert!(first_read.contains("hello world"), "got: {first_read}");
        let second_read = tool_results.next().expect("the repeated read's own result");
        assert!(
            second_read.contains("Already read") && !second_read.contains("hello world"),
            "a repeated identical read must get a short note, not the file text again: {second_read}"
        );
    }

    #[tokio::test]
    async fn live_step_permits_list_dir_and_search_when_granted() {
        let (dir, workspace) = workspace("def add(a, b):\n    return a - b\n");
        std::fs::write(dir.path().join("readme.txt"), "notes").expect("fixture file");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![
            tool_call_turn(LIST_DIR_TOOL, serde_json::json!({ "path": "." })),
            tool_call_turn(
                SEARCH_TOOL,
                serde_json::json!({ "pattern": "def add", "literal": true }),
            ),
            text_turn(r#"{"kind":"answer","answer":"found it"}"#, 10, 10),
        ]));
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![LIST_DIR_TOOL.to_owned(), SEARCH_TOOL.to_owned()]),
            10_000,
        );
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("both granted tools round-trip, then the model answers");
        assert_eq!(artifact, ModelStepArtifact::Answer("found it".to_owned()));
    }

    #[tokio::test]
    async fn live_step_refuses_list_dir_or_search_when_not_granted() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(SEARCH_TOOL, serde_json::json!({ "pattern": "todo" })),
            text_turn(r#"{"kind":"answer","answer":"done"}"#, 10, 10),
        ]);
        // Only read_file is granted, so the search call the model made anyway
        // is outside its reach even though search is a real tool this step
        // knows about.
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        );
        LiveModelStep::new(llm)
            .propose(&config, &view, &meter)
            .await
            .expect("a refused tool call is reported to the model, not fatal");
        let sent = format!("{:?}", requests.lock().unwrap()[1].messages);
        assert!(
            sent.contains("disallowed tool 'search'") && sent.contains("offers only read_file"),
            "the refusal names the tools the step does offer: {sent}"
        );
    }

    #[tokio::test]
    async fn live_step_honors_a_configured_turn_cap_below_the_default() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        // A configured cap of one turn must not consume the queued second
        // reply merely because the first reply explored instead of answered.
        let executor = LiveModelStep::new(live_llm(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            text_turn(r#"{"kind":"answer","answer":"done"}"#, 10, 10),
        ]));
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        )
        .with_max_turns(1);
        let artifact = executor
            .propose(&config, &view, &meter)
            .await
            .expect("a non-answer at the cap yields a typed fallback");
        assert!(
            matches!(&artifact, ModelStepArtifact::Answer(answer) if answer.contains("did not submit an answer")),
            "unexpected artifact: {artifact:?}"
        );
    }

    #[tokio::test]
    async fn live_step_forces_submit_answer_only_on_its_last_turn() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let (llm, requests) = live_llm_capturing(vec![
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
            tool_call_turn(READ_FILE_TOOL, serde_json::json!({ "path": "lib.rs" })),
        ]);
        let config = live_config(
            ModelStepOutputKind::Answer,
            ToolSubset::Only(vec![READ_FILE_TOOL.to_owned()]),
            10_000,
        )
        .with_max_turns(3);
        let _ = LiveModelStep::new(llm)
            .propose(&config, &view, &meter)
            .await;
        let choices: Vec<Option<ToolChoice>> = requests
            .lock()
            .expect("mock provider mutex is never poisoned")
            .iter()
            .map(|request| request.tool_choice.clone())
            .collect();
        assert_eq!(choices.len(), 3);
        assert!(choices[0].is_none() && choices[1].is_none());
        assert!(
            matches!(&choices[2], Some(ToolChoice::Specific(name)) if name == SUBMIT_ANSWER_TOOL),
            "last turn must force submit_answer, got {:?}",
            choices[2]
        );
    }

    #[tokio::test]
    async fn live_step_records_usage_on_the_meter() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let executor = LiveModelStep::new(live_llm(vec![text_turn(
            r#"{"kind":"answer","answer":"42"}"#,
            7,
            13,
        )]));
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        executor
            .propose(&config, &view, &meter)
            .await
            .expect("decodes");
        assert_eq!(meter.input_tokens(), 7);
        assert_eq!(meter.output_tokens(), 13);
    }

    #[tokio::test]
    async fn cost_cap_refuses_a_model_step_before_the_provider_is_called() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::new();
        meter.set_task_cost_cap(0.001).unwrap();
        let (llm, requests) = live_llm_recording(vec![text_turn("unused", 1, 1)]);
        let executor = LiveModelStep::new(llm).with_model_id("anthropic/claude-sonnet-5");
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1000);
        let error = executor.propose(&config, &view, &meter).await.unwrap_err();
        assert!(error.to_string().contains("cost-cap"));
        assert!(requests.lock().unwrap().is_empty());
        assert!(meter.cost_cap_stopped());
    }

    #[tokio::test]
    async fn cost_cap_prices_the_locate_model_and_following_edit_tier_on_one_meter() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::new();
        meter.set_task_cost_cap(0.151).unwrap();
        let answer = || text_turn(r#"{"kind":"answer","answer":"done"}"#, 0, 8192);
        let (sonnet, sonnet_requests) = live_llm_capturing(vec![answer()]);
        let (haiku, haiku_requests) = live_llm_capturing(vec![answer()]);
        let executor = LiveModelStep::new(sonnet)
            .with_model_id("anthropic/claude-sonnet-5")
            .with_model("anthropic/claude-haiku-4-5-20251001", haiku);
        let config =
            live_config(ModelStepOutputKind::Answer, ToolSubset::None, 100_000).with_max_turns(1);
        executor
            .propose(
                &config
                    .clone()
                    .with_model("anthropic/claude-haiku-4-5-20251001"),
                &view,
                &meter,
            )
            .await
            .unwrap();
        executor.propose(&config, &view, &meter).await.unwrap();
        assert_eq!(haiku_requests.lock().unwrap().len(), 1);
        assert_eq!(sonnet_requests.lock().unwrap().len(), 1);
        assert_eq!(meter.input_tokens(), 0);
        assert_eq!(meter.output_tokens(), 16_384);
        // Haiku's $0.04096 plus Sonnet's $0.08192 crosses 80% of $0.151.
        // Pricing either call as the other model would not yield this result.
        assert!(meter.take_cost_notice());
    }

    #[tokio::test]
    async fn live_step_stops_on_a_cancelled_run_instead_of_hanging_on_the_provider() {
        let (_dir, workspace) = workspace("todo!()");
        let view = WorkspaceView::new(&workspace);
        let meter = TokenMeter::default();
        let source = CancellationSource::new();
        // Never answers on its own: if the executor were not racing the
        // provider call against the cancellation signal, this would hang
        // until the outer timeout instead of returning promptly.
        let executor = LiveModelStep::new(pending_llm()).with_cancellation(source.token());
        let config = live_config(ModelStepOutputKind::Answer, ToolSubset::None, 1_000);
        source.cancel();

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            executor.propose(&config, &view, &meter),
        )
        .await
        .expect("a cancelled step must return rather than wait on a provider that never answers")
        .expect_err("a cancelled step has no artifact to return");
        assert!(
            matches!(&error, SystemError::ExecutionError(message) if message.contains("cancelled")),
            "unexpected error: {error:?}"
        );
    }

    /// End-to-end check: a planned graph — built the same way the
    /// host's scripted gate planner builds one, for a task with a declared
    /// test runner — actually runs its repair arm through [`LiveModelStep`]
    /// backed by a mock provider, and the proposed patch lands on disk.
    ///
    /// A hermetic host never installs a real [`ProjectTestRunner`]
    /// (`src/docs/resources.md`'s own entry for it says so: that absence is what
    /// makes `code.test-gated-edit`'s exclusion a real observation), so no
    /// test — this one included — can additionally drive this shape through
    /// the full `SessionsAPI`/`drive_turn` loop `gate_plan_selection_host_loop.rs`
    /// exercises for the test-runner-free shapes. Building the plan with
    /// [`scripted_plan`] and resolving it with [`materialize`] is the same
    /// two calls the host's own live selection point makes; this test adds
    /// only the fixture, the mock model, and the test-runner stand-in a
    /// hermetic host would not have.
    #[tokio::test]
    async fn live_model_step_repairs_a_failing_test_gated_plan_end_to_end() {
        use polaris_cosmos::composition::{CompositionLimits, materialize};
        use polaris_cosmos::contracts::StopReason as CodingStopReason;
        use polaris_cosmos::contracts::{AssessmentConclusion, RevisionAttempt};

        use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};
        use crate::cosmos::payload::ProjectTestRunner;
        use crate::cosmos::planner::{PlannerFacts, scripted_plan};
        use crate::cosmos::topology;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = topology::contract()
            .expect("the coding contract registers cleanly")
            .key()
            .clone();
        let dir = tempfile::tempdir().expect("temp project");
        // The stated edit lands somewhere that does not itself satisfy the
        // success check, so the initial direct-apply run genuinely fails and
        // the repair arm — the one that calls the live executor — is the one
        // exercised.
        std::fs::write(dir.path().join("lib.rs"), "todo!()\n// note").expect("seed file");
        let task = CodingTask::new(
            "gate-block-configurable-live",
            dir.path(),
            "lib.rs",
            "// note",
            "// noted",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let facts = PlannerFacts {
            registration: "test".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available: true,
            reproduce_runner_available: false,
            model_provider_available: true,
            is_question: false,
            is_survey: false,
            has_stated_edits: true,
            statement: task.statement().to_owned(),
            shell_available: false,
        };
        let plan = scripted_plan(
            &facts,
            RevisionAttempt::new(1).expect("one is a valid attempt number"),
            &catalogue,
            &target,
        )
        .expect("test-gated plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("test-gated plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(ProjectTestRunner::command(
            "sh",
            ["-c", "grep -q '1 + 1' lib.rs"],
        ));
        context.insert(ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(LiveModelStep::new(live_llm(vec![
            text_turn(
                r#"{"kind":"patch-intent","edits":[{"file":"lib.rs","find":"todo!()","replace":"1 + 1"}]}"#,
                12,
                18,
            ),
        ]))));
        context.insert(TokenMeter::default());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("test-gated plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), CodingStopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("1 + 1"),
            "the live executor's proposed patch actually landed through the repair arm"
        );
    }

    #[test]
    fn submit_guard_notice_refuses_empty_and_failing_submissions() {
        assert!(
            submit_guard_notice(true, false)
                .unwrap()
                .contains("no changes")
        );
        assert!(
            submit_guard_notice(false, true)
                .unwrap()
                .contains("check failed")
        );
        assert!(
            submit_guard_notice(true, true)
                .unwrap()
                .contains("no changes")
        );
        assert!(submit_guard_notice(false, false).is_none());
    }

    #[test]
    fn workspace_is_unchanged_ignores_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        git(&["add", "a.py"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ]);
        assert!(workspace_is_unchanged(dir.path()));
        std::fs::create_dir_all(dir.path().join(crate::state::STATE_DIR_NAME)).unwrap();
        std::fs::write(
            dir.path().join(crate::state::STATE_DIR_NAME).join("s.py"),
            "print(1)\n",
        )
        .unwrap();
        assert!(workspace_is_unchanged(dir.path()));
        std::fs::write(dir.path().join("a.py"), "x = 2\n").unwrap();
        assert!(!workspace_is_unchanged(dir.path()));
    }

    #[test]
    fn repo_layout_lists_top_level_and_children_without_hidden_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg/sub")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("pkg/mod.py"), "").unwrap();
        std::fs::write(dir.path().join("setup.py"), "").unwrap();
        let layout = repo_layout(dir.path());
        assert!(layout.contains("pkg/\n  pkg/mod.py\n  pkg/sub/"));
        assert!(layout.contains("setup.py"));
        assert!(!layout.contains(".git"));
    }
}
