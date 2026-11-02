//! What a non-interactive run writes when its turn is over: the output
//! formats, the identity triple, and the process exit code.
//!
//! This is the *whole* of the non-interactive result emitter — the one place
//! in this crate allowed to produce stdout. It lives here, in the library, rather than in
//! `main.rs`'s front end so that the rendering of every format is covered by
//! tests instead of only by running the binary. The front end supplies the
//! sinks (stdout for [`RunReport::stdout_payload`], stderr for
//! [`RunReport::identity_line`] and [`RunReport::error_line`]) and the exit
//! code from [`RunReport::exit_code`]; it renders nothing itself.
//!
//! # Where a consumer reads the run's accounting
//!
//! Exactly one shape carries it — [`RunUsage`], schema
//! [`RUN_USAGE_SCHEMA`](crate::usage::RUN_USAGE_SCHEMA) — and each
//! machine-readable format carries that same object:
//!
//! | `--output-format` | Machine-readable? | Where the accounting is |
//! |---|---|---|
//! | `json` | yes — **the authoritative mode** | the `usage` member of the one stdout object |
//! | `stream-json` | yes | the final stdout line: the one [`IOMessage`] whose `io_type` is [`USAGE_IO_TYPE`], carrying the same object serialized as its text |
//! | `text` | no — stdout is the assistant's answer | not reported; use `json` |
//!
//! The live `io_type = "tokens"` messages that appear mid-stream are the
//! conversation panel's running tally: per-step *deltas*, input and output
//! only, no cache tiers and no cost. They are a display signal, **not** the
//! accounting — a consumer that sums them is reconstructing a number it was
//! handed. Read [`USAGE_IO_TYPE`] instead.
//!
//! # Where a consumer reads the spend ceiling
//!
//! A run the operator gave a limit also reports a
//! [`CeilingStatus`] — schema
//! [`RUN_CEILING_SCHEMA`](crate::usage::RUN_CEILING_SCHEMA) — as the
//! `ceiling` member of the `json` object, alongside `usage` rather than
//! inside it. A run that stopped because it reached that limit has status
//! `ceiling-reached`, exit code
//! [`CEILING_EXIT_CODE`](crate::run::CEILING_EXIT_CODE), and states what it
//! spent against what it was allowed on stderr through
//! [`RunReport::ceiling_line`], which every format writes. No new
//! `stream-json` message type is introduced for it.
//!
//! # The limit the run ran under
//!
//! [`RunReport::max_output_tokens`] is the `max_tokens` every request of the
//! turn carried — configuration of the run rather than a measurement of it, so
//! it sits next to the accounting rather than inside the versioned
//! [`RunUsage`] record. `json` carries it; `text` and `stream-json` do not.
//!
//! # Where a consumer reads why a run failed
//!
//! A failed run says so once, in one short sentence: the `error` member of
//! the `json` object and the [`RunReport::error_line`] on stderr carry the
//! same text, with no provider response body and no internal node ids in it.
//! `error_kind` beside it names the kind of failure — `auth`, `rate_limit`,
//! `overloaded`, `timeout`, `network`, `invalid_request` or `other` — so a
//! caller can choose between fixing a key, waiting, and retrying without
//! parsing the sentence. It is present exactly when `error` is, and a failure
//! that did not come from a model call is `other`. `rate_limit`,
//! `overloaded`, `timeout` and `network` are transient: the agent has already
//! retried them as far as its `RetryPolicy` allows by the time they are
//! reported.
//!
//! # Where a consumer reads which topology ran, and why
//!
//! A turn whose accepted coding task started a topology carries the same
//! explanation an interactive panel shows — [`TopologySelectionReport`], the
//! `topology` member of the `json` object — as data rather than prose, so a
//! machine consumer establishes which registration ran and what it beat
//! without parsing anything. `null` for an ordinary conversational turn and
//! for a turn that started no topology at all; a refused turn's own reasons
//! are already in `error`/[`RunReport::error_line`]. Carried by
//! [`OutputFormat::Json`] only, like [`RunReport::max_output_tokens`].

use std::fmt;
use std::str::FromStr;

use polaris::plugins::{IO_TYPE, IOMessage};
use polaris::sessions::{SessionId, TurnNumber};
use polaris_cosmos::contracts::{
    CandidateConsideration, CompositionChoiceOrigin, CompositionEvidence, ContentDigest,
    SelectionDecisionId, SelectionRationale,
};
use serde::Serialize;

use crate::agents::FailureKind;
use crate::cosmos::host::TopologyTurn;
use crate::cosmos::inventory::TopologyInventory;
use crate::run::{TurnOutcome, TurnReason};
use crate::usage::{CeilingStatus, RUN_USAGE_SCHEMA, RunUsage};

/// Which topology ran, and why it beat the alternatives — the same facts
/// [`crate::terminal::topology::turn_report`] renders for a person, carried
/// as data instead of prose so a machine consumer establishes them without
/// scraping anything.
///
/// Present only for a turn whose [`TopologyTurn`] is
/// [`TopologyTurn::Executed`]. Straight from that variant's own
/// [`ExecutedTopology`](crate::cosmos::host::ExecutedTopology) — nothing here
/// is re-ranked, re-scored, or recomputed.
#[derive(Debug, Clone, Serialize)]
pub struct TopologySelectionReport {
    /// The registration that ran.
    pub registration_id: String,
    /// The decision that selected it.
    pub decision_id: SelectionDecisionId,
    /// Zero for the decision's primary choice, else the one-based position in
    /// its captured pre-start fallback plan.
    pub fallback_position: u64,
    /// Why this registration beat the runner-up, straight from the
    /// decision's own selected-outcome rationale.
    pub rationale: SelectionRationale,
    /// Every candidate the decision considered, winner included, in the
    /// decision's own canonical order. A consumer that wants "what else was
    /// considered" excludes `registration_id` itself from this list.
    pub candidates: Vec<CandidateConsideration>,
    /// Exact bounded Cosmos-block recipe materialised for the run, when the
    /// topology result came from the runtime assembly path.
    pub composition: Option<CompositionReport>,
    /// The task-specific plan a gate planner assembled for this run, and how
    /// many earlier attempts this turn's planner submitted and had refused
    /// before it materialised. `None` for a finite-recipe composition.
    pub gate_plan: Option<GatePlanReport>,
    /// How many gate-plan attempts a candidate's own revision loop submitted
    /// and had refused before it exhausted and fell back to the finite
    /// recipe grammar this run actually used. `None` when no gate-plan
    /// revision loop ran for the winning candidate, whether because the
    /// composition policy was not `gates` or because a gate plan admitted
    /// outright — always `None` when [`Self::gate_plan`] is `Some`.
    pub exhausted_gate_plan: Option<ExhaustedGatePlanReport>,
}

/// The admitted gate plan's own identity and the revision loop's own count,
/// read straight off the executed turn's revision trail — nothing here is
/// recomputed.
#[derive(Debug, Clone, Serialize)]
pub struct GatePlanReport {
    /// Stable identity of the planner that assembled this plan —
    /// `polaris-code.gate-planner-scripted` or `polaris-code.gate-planner-model`
    /// — read off the admitted decision's own autonomous choice origin, the
    /// same policy identity [`crate::cosmos::planner::GatePlannerMode::as_str`]
    /// names.
    pub planner: String,
    /// Canonical digest of the whole submitted gate-expression plan.
    pub plan_digest: ContentDigest,
    /// Configuration-independent digest of the plan's gate-expression shape.
    pub shape_digest: ContentDigest,
    /// Readable rendering of the whole admitted gate-expression tree — every
    /// gate's own name, nested exactly as the plan nests it, down to the
    /// catalogue block/predicate/discriminator names at its leaves (see
    /// [`crate::cosmos::planner::render_gate_expression_shape`]) — with
    /// plan-local config and instance labels omitted. [`Self::shape_digest`]
    /// remains the identity two admitted plans are grouped or compared by;
    /// this field only makes that identity legible to a reader.
    pub shape: String,
    /// One-based position of the admitted plan in this turn's revision loop.
    pub revision_attempt: u8,
    /// How many earlier attempts this turn's planner submitted and had
    /// refused before the admitted one.
    pub prior_refused_attempts: u8,
}

/// Names the policy responsible for a composition choice, whichever way it
/// was decided — the same string an evidence consumer would otherwise have
/// to match on the origin enum to get.
fn choice_policy_name(origin: &CompositionChoiceOrigin) -> String {
    match origin {
        CompositionChoiceOrigin::Autonomous { policy } => policy.id().as_str().to_owned(),
        CompositionChoiceOrigin::Forced { source } => source.id().as_str().to_owned(),
    }
}

/// How many gate-plan attempts an exhausted revision loop made, read
/// straight off the executed turn's own exhausted trail.
#[derive(Debug, Clone, Serialize)]
pub struct ExhaustedGatePlanReport {
    /// How many attempts the revision loop submitted, every one refused,
    /// before it exhausted.
    pub refused_attempts: u8,
}

/// The replay identity and resolved components of one materialised composition.
#[derive(Debug, Clone, Serialize)]
pub struct CompositionReport {
    /// Stable digest of the whole bounded composition plan.
    pub digest: ContentDigest,
    /// Versioned recipe identity.
    pub recipe: String,
    /// Versioned ordered Cosmos block identities.
    pub blocks: Vec<String>,
    /// Whether an explicit caller direction forced this choice.
    pub forced: bool,
    /// Explicitly selected policy that chose the finite plan.
    pub policy: String,
    /// The policy's stated grounds for this turn's recipe.
    pub grounds: String,
    /// Digest of the model chooser prompt, when a model chose the plan.
    pub prompt_digest: Option<ContentDigest>,
    /// Digest of the raw model chooser response, when a model chose the plan.
    pub response_digest: Option<ContentDigest>,
    /// Complete canonical composition evidence, including each component's
    /// resolved configuration and the links to ordinary admission, run, and
    /// verifier evidence.
    pub evidence: CompositionEvidence,
}

impl TopologySelectionReport {
    /// Projects the executed registration's own decision material, or `None`
    /// when this turn's topology answer was not an execution.
    fn executed(topology: &TopologyTurn) -> Option<Self> {
        let TopologyTurn::Executed(run) = topology else {
            return None;
        };
        Some(Self {
            registration_id: run.registration_id.clone(),
            decision_id: run.decision_id,
            fallback_position: run.fallback_position,
            rationale: run.rationale.clone(),
            candidates: run.candidates.clone(),
            composition: run.composition.as_ref().map(|evidence| {
                let choice = evidence.decision().choice();
                let plan = choice.plan();
                CompositionReport {
                    digest: plan.composition_digest(),
                    recipe: plan.recipe().recipe().to_string(),
                    blocks: plan
                        .blocks()
                        .iter()
                        .map(|block| block.block().to_string())
                        .collect(),
                    forced: matches!(choice.origin(), CompositionChoiceOrigin::Forced { .. }),
                    policy: run
                        .composition_request
                        .as_ref()
                        .map(|request| request.policy().as_str().to_owned())
                        .unwrap_or_else(|| "polaris-code.composition-unknown".to_owned()),
                    grounds: run
                        .composition_request
                        .as_ref()
                        .map(|request| request.grounds().to_owned())
                        .unwrap_or_default(),
                    prompt_digest: run
                        .composition_request
                        .as_ref()
                        .and_then(|request| request.prompt_digest()),
                    response_digest: run
                        .composition_request
                        .as_ref()
                        .and_then(|request| request.response_digest()),
                    evidence: evidence.clone(),
                }
            }),
            gate_plan: run.gate_plan_trail.as_ref().and_then(|trail| {
                let admitted = trail.revisions().last()?;
                let record = admitted.decision().gate_plan()?;
                Some(GatePlanReport {
                    planner: choice_policy_name(admitted.decision().choice().origin()),
                    plan_digest: record.plan_digest(),
                    shape_digest: record.shape().digest(),
                    shape: run
                        .gate_plan_shape
                        .clone()
                        .unwrap_or_else(|| "unavailable".to_owned()),
                    revision_attempt: admitted.revision_attempt().get(),
                    prior_refused_attempts: admitted.revision_attempt().get().saturating_sub(1),
                })
            }),
            exhausted_gate_plan: run.exhausted_gate_plan_trail.as_ref().map(|trail| {
                ExhaustedGatePlanReport {
                    refused_attempts: u8::try_from(trail.revisions().len()).unwrap_or(u8::MAX),
                }
            }),
        })
    }
}

/// How a non-interactive run renders its result on stdout.
///
/// Parsed from the `--output-format` argument by [`FromStr`]; the accepted
/// spellings are exactly [`OutputFormat::VALUES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// The final assistant text and nothing else.
    #[default]
    Text,
    /// One JSON object, printed after the turn completes, carrying the
    /// identity triple, the status and the final text.
    Json,
    /// The turn's existing [`IOMessage`] stream, one JSON object per line, as
    /// the messages arrive. No parallel event type is introduced: these are
    /// the same messages the sessions `SSE` handler emits.
    StreamJson,
}

impl OutputFormat {
    /// Every accepted `--output-format` spelling, in the order the help text
    /// and the documentation list them.
    pub const VALUES: [&'static str; 3] = ["text", "json", "stream-json"];

    /// This format's `--output-format` spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::StreamJson => "stream-json",
        }
    }

    /// Whether the front end must print each [`IOMessage`] as it arrives
    /// rather than only printing a result once the turn is over.
    ///
    /// A front end drains the message channel either way — an undrained
    /// bounded channel would backpressure the turn — but only a streaming
    /// format writes those messages to stdout.
    #[must_use]
    pub fn streams_messages(self) -> bool {
        matches!(self, Self::StreamJson)
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OutputFormat {
    type Err = UnknownOutputFormat;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "stream-json" => Ok(Self::StreamJson),
            other => Err(UnknownOutputFormat(other.to_string())),
        }
    }
}

/// The error [`OutputFormat::from_str`] returns for an unrecognised
/// `--output-format` value.
///
/// Reaching the argument parser, this is a *usage* error, which the
/// non-interactive contract reports with exit code `2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownOutputFormat(String);

impl fmt::Display for UnknownOutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown output format `{}`: expected one of {}",
            self.0,
            OutputFormat::VALUES.join(", ")
        )
    }
}

impl std::error::Error for UnknownOutputFormat {}

/// One non-interactive run's reportable result: the identity triple every
/// format carries, the status, and the
/// final text or failure message.
///
/// Serialized as-is for [`OutputFormat::Json`]; the `reason` it was built from
/// is kept, unserialized, as the single source of the exit code.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// The session the turn ran in — the first of the three identifiers a run
    /// reports so it can be found afterwards in
    /// `<working_dir>/.polaris/sessions`, the trajectory graph and any `OTel`
    /// backend.
    pub session_id: String,
    /// The session's turn number after the turn: incremented on success, left
    /// where it was on failure.
    pub turn_number: TurnNumber,
    /// The executor's run id — the trace join key into graph events. `None`
    /// when the turn never reached the executor, because no id was minted.
    pub run_id: Option<String>,
    /// `completed`, `failed`, `interrupted`, `no-topology-started` or
    /// `ceiling-reached`.
    pub status: &'static str,
    /// The final assistant text, when the turn produced one.
    pub result: Option<String>,
    /// The failure message, when the turn failed.
    pub error: Option<String>,
    /// What kind of failure `error` describes — `auth`, `rate_limit`,
    /// `overloaded`, `timeout`, `network`, `invalid_request` or `other` — so a
    /// caller can decide what to do without reading the message. Present
    /// exactly when `error` is; a failure that is not a model-call failure,
    /// such as a busy session or a coding task that started no topology, is
    /// `other`.
    pub error_kind: Option<FailureKind>,
    /// The run's own token and cost accounting — the `usage` member of the
    /// `json` object, and the authoritative reading for any consumer that
    /// needs what the run cost. Taken from
    /// [`TurnOutcome::usage`](crate::run::TurnOutcome::usage) rather than
    /// recomputed here.
    ///
    /// Serialized as `null` when the run was unmeasured (no
    /// [`TokenMeter`](crate::usage::TokenMeter) installed). `null` is not
    /// zero: a consumer must treat it as "this run reports no accounting",
    /// never as "this run was free".
    pub usage: Option<RunUsage>,
    /// Where the run stands against the operator's spend ceiling — the
    /// `ceiling` member of the `json` object, schema
    /// [`RUN_CEILING_SCHEMA`](crate::usage::RUN_CEILING_SCHEMA).
    ///
    /// Its own member, and its own schema, rather than a field added to
    /// `usage`: what a run *spent* and what it was *allowed* are separate
    /// facts, and
    /// [`RUN_USAGE_SCHEMA`](crate::usage::RUN_USAGE_SCHEMA) keeps exactly the
    /// meaning every consumer already parses.
    ///
    /// Serialized as `null` when the operator set no limit. `null` here means
    /// "no ceiling was set", never "the ceiling was not reached".
    pub ceiling: Option<CeilingStatus>,
    /// The per-request output token limit this run registered its model
    /// provider with — `max_tokens` on every Anthropic request the turn made.
    ///
    /// Reported because it bounds the run: a response that reaches it comes
    /// back truncated, so a benchmark number derived from this run is only
    /// interpretable alongside the limit it ran under. It is configuration,
    /// not accounting, so it sits beside [`RunUsage`] rather than inside it —
    /// and it is deliberately outside the versioned `usage` record, which
    /// consumers pin by schema.
    ///
    /// Carried by [`OutputFormat::Json`] only. `text` reserves stdout for the
    /// assistant's answer and `stream-json` for [`IOMessage`]s.
    pub max_output_tokens: u32,
    /// Which topology ran and why, when this turn's accepted coding task
    /// started one. See "Where a consumer reads which topology ran, and why"
    /// above.
    pub topology: Option<TopologySelectionReport>,
    /// The terminal topology-path answer, including selection outcomes that
    /// never started a registration. `None` is an ordinary conversational
    /// turn, so an external consumer need not infer a path from the exit code.
    pub topology_terminal: Option<TopologyTerminalReport>,
    /// The outcome this report projects. Skipped by serialization: `status`
    /// and `error` are its serialized projection, and it is kept only so
    /// [`RunReport::exit_code`] has one source of truth rather than
    /// re-deriving a code from `status`.
    #[serde(skip)]
    reason: TurnReason,
}

/// What to report when the turn started no topology and carried no decision
/// to explain it — a shape the run surface does not produce, kept so the
/// emitter has no unwrap.
fn no_topology_fallback() -> String {
    "no topology started for the accepted coding task".to_string()
}

impl RunReport {
    /// Project one [`TurnOutcome`] into the result a non-interactive run
    /// reports for `session_id`, under the output token limit
    /// `max_output_tokens` that the run registered its model provider with.
    ///
    /// The limit is a required argument rather than an optional one: every
    /// reporter has to state the bound its turn ran under, so none can forget
    /// to and quietly report an unbounded-looking result.
    #[must_use]
    pub fn new(session_id: &SessionId, outcome: &TurnOutcome, max_output_tokens: u32) -> Self {
        let (status, error) = match &outcome.reason {
            TurnReason::Completed => ("completed", None),
            TurnReason::Failed(failure) => ("failed", Some(failure.message.clone())),
            TurnReason::Interrupted => ("interrupted", None),
            // The answer *is* the explanation for this outcome, so `error`
            // carries the turn's own answer rather than a second rendering of
            // it. `result` carries the same string, because a no-start answer
            // is still what this turn had to say.
            TurnReason::NoTopologyStarted => (
                "no-topology-started",
                Some(
                    outcome
                        .topology
                        .as_ref()
                        .map_or_else(no_topology_fallback, TopologyTurn::answer),
                ),
            ),
            // Not an error: the run did what it was allowed to do and
            // stopped. What was spent against what was allowed is reported
            // by `ceiling` and by `ceiling_line`, so `error` stays `None`
            // and a consumer keying off it still sees no failure.
            TurnReason::CeilingReached => ("ceiling-reached", None),
            TurnReason::CostCapped => ("cost-cap", None),
        };
        Self {
            session_id: session_id.as_str().to_string(),
            turn_number: outcome.turn_number,
            run_id: outcome.run_id.as_ref().map(|id| id.as_str().to_string()),
            status,
            result: outcome.final_text.clone(),
            error_kind: error.as_ref().map(|_| match &outcome.reason {
                TurnReason::Failed(failure) => failure.kind,
                _ => FailureKind::Other,
            }),
            error,
            usage: outcome.usage.clone(),
            ceiling: outcome.ceiling.clone(),
            max_output_tokens,
            topology: outcome
                .topology
                .as_ref()
                .and_then(TopologySelectionReport::executed),
            topology_terminal: outcome.topology.as_ref().map(TopologyTerminalReport::from),
            reason: outcome.reason.clone(),
        }
    }

    /// The identity triple on one line, which the front end writes to stderr
    /// for *every* format.
    ///
    /// All three identifiers are required in all three formats;
    /// [`OutputFormat::Json`] also embeds them in its stdout object, but
    /// `text` and `stream-json` reserve stdout for exactly the final text and
    /// exactly the [`IOMessage`] stream, so their copy goes to stderr.
    #[must_use]
    pub fn identity_line(&self) -> String {
        format!(
            "session={} turn={} run_id={}",
            self.session_id,
            self.turn_number,
            self.run_id.as_deref().unwrap_or("-")
        )
    }

    /// The model the run actually served its turn on, as one stderr line, or
    /// `None` when the run was unmeasured and so names no model.
    ///
    /// A separate line rather than a fourth field on the identity triple: that
    /// triple is fixed, and this is a different fact — not who the run was,
    /// but what answered it. `json` already embeds the same id inside
    /// `usage.model_id`; this is how `text` and `stream-json` state it without
    /// spending their stdout, so an operator who passed `--model` can see it
    /// took effect without asking.
    #[must_use]
    pub fn model_line(&self) -> Option<String> {
        let usage = self.usage.as_ref()?;
        Some(format!("polaris: model={}", usage.model_id))
    }

    /// The stderr diagnostic for a turn that produced no result — a failed
    /// turn, or one whose accepted coding task started no topology — or
    /// `None` when there is nothing to explain. Diagnostics never go to
    /// stdout.
    #[must_use]
    pub fn error_line(&self) -> Option<String> {
        let message = self.error.as_ref()?;
        Some(match self.reason {
            TurnReason::NoTopologyStarted => {
                format!("polaris: no topology started: {message}")
            }
            _ => format!("polaris: turn failed: {message}"),
        })
    }

    /// The stderr line stating what the run spent against what it was
    /// allowed, or `None` when the run did not stop at its ceiling.
    ///
    /// Written for every format, like [`identity_line`](Self::identity_line):
    /// `text` reserves stdout for the assistant's answer and `stream-json`
    /// for the message stream, and a run that stopped early must say so even
    /// in the format that carries no structured result. A `json` consumer can
    /// read the same facts from the `ceiling` member instead of parsing this.
    #[must_use]
    pub fn ceiling_line(&self) -> Option<String> {
        let ceiling = self.ceiling.as_ref()?;
        if !ceiling.stopped {
            return None;
        }
        Some(format!(
            "polaris: spend ceiling reached, stopping: {}",
            ceiling.statement()
        ))
    }

    /// What the front end writes to stdout for `format`, or `None` when the
    /// format has nothing left to print — [`OutputFormat::StreamJson`] has
    /// already printed its lines, and [`OutputFormat::Text`] prints nothing
    /// for a turn that produced no assistant text.
    #[must_use]
    pub fn stdout_payload(&self, format: OutputFormat) -> Option<String> {
        match format {
            OutputFormat::Text => self.result.clone(),
            // Infallible in practice — every field is a string, a `u32` or an
            // `Option` of one — but rendered through a fallback object rather
            // than a panic so a non-interactive caller always gets parseable
            // JSON on stdout.
            OutputFormat::Json => Some(serde_json::to_string(self).unwrap_or_else(|err| {
                format!(r#"{{"status":"failed","error":"failed to serialize result: {err}"}}"#)
            })),
            OutputFormat::StreamJson => None,
        }
    }

    /// The process exit code for this run: `0` completed, `1` turn failed,
    /// [`CEILING_EXIT_CODE`](crate::run::CEILING_EXIT_CODE) stopped at the
    /// spend ceiling, `130` interrupted.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.reason.exit_code()
    }
}

/// The terminal answer reached by topology selection.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TopologyTerminalReport {
    /// `executed`, `no-eligible-candidate`, `not-started`, or `declined`.
    pub outcome: &'static str,
    /// The registration that started, if selection started one.
    pub registration_id: Option<String>,
}

impl From<&TopologyTurn> for TopologyTerminalReport {
    fn from(topology: &TopologyTurn) -> Self {
        match topology {
            TopologyTurn::Executed(run) => Self {
                outcome: "executed",
                registration_id: Some(run.registration_id.clone()),
            },
            TopologyTurn::CostCapped(run) => Self {
                outcome: "cost-cap",
                registration_id: Some(run.registration_id.clone()),
            },
            TopologyTurn::NoEligibleCandidate(_) => Self {
                outcome: "no-eligible-candidate",
                registration_id: None,
            },
            TopologyTurn::NotStarted(_) => Self {
                outcome: "not-started",
                registration_id: None,
            },
            TopologyTurn::Declined(_) => Self {
                outcome: "declined",
                registration_id: None,
            },
        }
    }
}

/// What `--list-topologies` writes to stdout.
///
/// A listing is not a turn: it starts nothing, spends nothing, and produces no
/// assistant text, so it is its own report rather than a [`RunReport`] with
/// empty fields. It lives here for the same reason `RunReport` does — every
/// byte the non-interactive front end prints is rendered in this module, so
/// the rendering is covered by tests rather than only by running the binary.
///
/// # Example
///
/// ```
/// # use polaris_code::cosmos::inventory;
/// # use polaris_code::exec::{InventoryReport, OutputFormat};
/// # fn render(listed: inventory::TopologyInventory) -> String {
/// let report = InventoryReport::new(listed);
/// report.stdout_payload(OutputFormat::Text)
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct InventoryReport {
    inventory: TopologyInventory,
}

impl InventoryReport {
    /// Captures a derived inventory for rendering.
    #[must_use]
    pub const fn new(inventory: TopologyInventory) -> Self {
        Self { inventory }
    }

    /// What the front end writes to stdout for `format`.
    ///
    /// [`OutputFormat::StreamJson`] renders the same single object as
    /// [`OutputFormat::Json`]: a listing is one answer, not a message stream,
    /// and a scripted consumer that asked for line-delimited JSON still gets
    /// one parseable line.
    #[must_use]
    pub fn stdout_payload(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.inventory.to_text(),
            OutputFormat::Json | OutputFormat::StreamJson => serde_json::to_string(&self.inventory)
                .unwrap_or_else(|err| {
                    format!(r#"{{"error":"failed to serialize the topology inventory: {err}"}}"#)
                }),
        }
    }

    /// The process exit code for a listing: `0` — reading the sealed catalog
    /// is not a turn and cannot fail a turn.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        0
    }
}

/// The `io_type` metadata value marking the one `stream-json` message that
/// carries the run's accounting.
///
/// A `stream-json` consumer selects on this and parses that message's text as
/// a [`RunUsage`]; it never has to interpret the live `io_type = "tokens"`
/// deltas, which are a display signal rather than the accounting.
pub const USAGE_IO_TYPE: &str = "usage";

/// Render `usage` as the final `stream-json` message.
///
/// Deliberately an [`IOMessage`] and not a parallel event type: the
/// `stream-json` stream stays one vocabulary, and the front end sends this through the same provider and the
/// same [`stream_json_line`] renderer as every other message, so it arrives
/// last and in stream order rather than being printed around the stream.
///
/// The text is exactly the object [`RunReport::stdout_payload`] embeds under
/// `usage` for [`OutputFormat::Json`], so the two formats agree byte for byte
/// and a consumer needs one parser, not two.
///
/// # Example
///
/// ```
/// use polaris_code::exec::{USAGE_IO_TYPE, usage_message};
/// use polaris_code::usage::TokenMeter;
///
/// let meter = TokenMeter::new();
/// meter.add(90, 10);
/// let message = usage_message(&meter.snapshot("anthropic/claude-sonnet-4-6"));
///
/// assert_eq!(message.metadata.get("io_type").map(String::as_str), Some(USAGE_IO_TYPE));
/// ```
#[must_use]
pub fn usage_message(usage: &RunUsage) -> IOMessage {
    // Infallible in practice — every field is a string, a `u64`, or an
    // `Option` of one of those — but rendered through a fallback rather than a
    // panic so a streaming consumer always gets a parseable last line that
    // still names the schema it failed under.
    let payload = serde_json::to_string(usage).unwrap_or_else(|err| {
        format!(r#"{{"schema":"{RUN_USAGE_SCHEMA}","error":"failed to serialize usage: {err}"}}"#)
    });
    IOMessage::system_text(payload).with_metadata(IO_TYPE, USAGE_IO_TYPE)
}

/// One `stream-json` line: `message` serialized as a single-line JSON object.
///
/// Shared by the front end and its tests so what a test asserts is exactly
/// what the binary prints.
///
/// # Errors
///
/// Returns the `serde_json` error if the message cannot be serialized, which
/// the front end reports as a diagnostic on stderr rather than emitting a
/// malformed line.
pub fn stream_json_line(message: &IOMessage) -> Result<String, serde_json::Error> {
    serde_json::to_string(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::TurnFailure;
    use crate::usage::TokenMeter;
    use polaris::graph::RunId;
    use polaris::models::anthropic::DEFAULT_MAX_TOKENS;
    use std::time::Duration;

    /// A non-default limit, so a test that asserts on it cannot pass by
    /// accidentally matching [`DEFAULT_MAX_TOKENS`].
    const CONFIGURED_LIMIT: u32 = 32_000;

    fn outcome(reason: TurnReason, final_text: Option<&str>, run_id: bool) -> TurnOutcome {
        outcome_with_topology(reason, final_text, run_id, None)
    }

    fn outcome_with_topology(
        reason: TurnReason,
        final_text: Option<&str>,
        run_id: bool,
        topology: Option<TopologyTurn>,
    ) -> TurnOutcome {
        TurnOutcome {
            final_text: final_text.map(str::to_string),
            run_id: run_id.then(RunId::new),
            nodes_executed: 4,
            duration: Duration::from_millis(7),
            turn_number: 3,
            reason,
            usage: Some(TokenMeter::new().snapshot("anthropic/claude-sonnet-4-6")),
            topology,
            ceiling: None,
        }
    }

    fn no_topology_started(attempted: Vec<String>) -> TopologyTurn {
        let unstarted = Box::new(crate::cosmos::host::NoTopologyStarted {
            decision_id: "b1c2d3e4-0000-4000-8000-000000000042"
                .parse()
                .expect("a UUIDv4 decision ID"),
            task_id: polaris_cosmos::contracts::TaskId::new("polaris-code", "implement-add")
                .expect("a valid host task reference"),
            reasons: vec!["verifier.capability-missing".to_string()],
            attempted: attempted.clone(),
            requested: None,
            requested_by_pin: false,
            origin: crate::cosmos::host::TaskOrigin::Explicit,
        });
        if attempted.is_empty() {
            TopologyTurn::NoEligibleCandidate(unstarted)
        } else {
            TopologyTurn::NotStarted(unstarted)
        }
    }

    #[test]
    fn a_no_topology_turn_reports_its_own_status_and_the_selector_codes() {
        let report = RunReport::new(
            &SessionId::from_string("s1"),
            &outcome_with_topology(
                TurnReason::NoTopologyStarted,
                None,
                false,
                Some(no_topology_started(Vec::new())),
            ),
            CONFIGURED_LIMIT,
        );
        assert_eq!(report.status, "no-topology-started");
        assert_eq!(report.exit_code(), 1);

        // Not "turn failed": nothing failed, the answer was that nothing was
        // eligible, and the diagnostic says so with the selector's own code.
        let line = report.error_line().expect("the answer explains itself");
        assert_eq!(
            line,
            "polaris: no topology started: no eligible candidate \
             (verifier.capability-missing)"
                .replace("\\\n             ", "")
        );

        // A refused live readmission names what it tried.
        let attempted = RunReport::new(
            &SessionId::from_string("s1"),
            &outcome_with_topology(
                TurnReason::NoTopologyStarted,
                None,
                false,
                Some(no_topology_started(vec!["code.direct-edit".to_string()])),
            ),
            CONFIGURED_LIMIT,
        );
        let line = attempted.error_line().expect("the answer explains itself");
        assert!(
            line.contains("no candidate survived live readmission")
                && line.contains("attempted code.direct-edit"),
            "unexpected diagnostic: {line}"
        );
    }

    #[test]
    fn every_accepted_spelling_round_trips_and_others_are_rejected() {
        for value in OutputFormat::VALUES {
            let parsed: OutputFormat = value.parse().expect("VALUES must parse");
            assert_eq!(parsed.as_str(), value);
            assert_eq!(parsed.to_string(), value);
        }
        let err = "streamjson".parse::<OutputFormat>().unwrap_err();
        assert!(
            err.to_string().contains("stream-json"),
            "the error should name the accepted spellings: {err}"
        );
        assert!("".parse::<OutputFormat>().is_err());
        assert!("Text".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn only_stream_json_prints_messages_as_they_arrive() {
        assert!(OutputFormat::StreamJson.streams_messages());
        assert!(!OutputFormat::Text.streams_messages());
        assert!(!OutputFormat::Json.streams_messages());
    }

    /// The `json` shape is a contract other tooling parses: adding
    /// `topology` must not cost it a single pre-existing member, and an
    /// ordinary turn — no accepted coding task, nothing selected — must
    /// report it as `null` rather than gaining unprompted output.
    #[test]
    fn the_json_shape_gained_a_topology_member_and_lost_nothing() {
        let session_id = SessionId::from_string("s-topology-shape");
        let report = RunReport::new(
            &session_id,
            &outcome(TurnReason::Completed, Some("ok"), true),
            CONFIGURED_LIMIT,
        );
        let json = report
            .stdout_payload(OutputFormat::Json)
            .expect("json always prints an object");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
        for field in [
            "session_id",
            "turn_number",
            "run_id",
            "status",
            "result",
            "error",
            "usage",
            "ceiling",
            "max_output_tokens",
        ] {
            assert!(
                parsed.get(field).is_some(),
                "missing pre-existing field {field:?}: {json}"
            );
        }
        assert!(
            parsed["topology"].is_null(),
            "a conversational turn started no topology: {json}"
        );
        assert!(report.topology.is_none());
        assert!(report.topology_terminal.is_none());
    }

    #[test]
    fn identity_is_reported_in_all_three_formats() {
        let session_id = SessionId::from_string("s-1");
        let report = RunReport::new(
            &session_id,
            &outcome(TurnReason::Completed, Some("hi"), true),
            CONFIGURED_LIMIT,
        );
        let run_id = report.run_id.clone().expect("the outcome carried a run id");

        // stderr copy: identical for every format.
        let identity = report.identity_line();
        for part in [
            "session=s-1".to_string(),
            "turn=3".to_string(),
            format!("run_id={run_id}"),
        ] {
            assert!(
                identity.contains(&part),
                "identity line {identity:?} is missing {part:?}"
            );
        }

        // stdout copy: `json` embeds the same three identifiers.
        let json = report
            .stdout_payload(OutputFormat::Json)
            .expect("json always prints an object");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
        assert_eq!(parsed["session_id"], "s-1");
        assert_eq!(parsed["turn_number"], 3);
        assert_eq!(parsed["run_id"], run_id.as_str());
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["result"], "hi");
        assert!(parsed["error"].is_null());
        assert!(
            parsed.get("reason").is_none(),
            "the private reason must not leak into the JSON result: {json}"
        );
    }

    #[test]
    fn text_prints_the_answer_and_stream_json_prints_nothing_extra() {
        let session_id = SessionId::from_string("s-2");
        let report = RunReport::new(
            &session_id,
            &outcome(TurnReason::Completed, Some("the answer"), true),
            CONFIGURED_LIMIT,
        );
        assert_eq!(
            report.stdout_payload(OutputFormat::Text).as_deref(),
            Some("the answer")
        );
        assert_eq!(report.stdout_payload(OutputFormat::StreamJson), None);

        let silent = RunReport::new(
            &session_id,
            &outcome(TurnReason::Completed, None, true),
            CONFIGURED_LIMIT,
        );
        assert_eq!(silent.stdout_payload(OutputFormat::Text), None);
    }

    #[test]
    fn a_failed_turn_reports_its_message_on_stderr_and_in_json_and_exits_one() {
        let session_id = SessionId::from_string("s-3");
        let report = RunReport::new(
            &session_id,
            &outcome(TurnReason::Failed("session busy".into()), None, false),
            CONFIGURED_LIMIT,
        );

        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.status, "failed");
        assert!(
            report
                .error_line()
                .expect("a failed turn has an error line")
                .contains("session busy")
        );
        // No run id was minted, so every format reports the identity with the
        // placeholder rather than inventing one.
        assert!(report.identity_line().contains("run_id=-"));

        let json = report
            .stdout_payload(OutputFormat::Json)
            .expect("json always prints an object");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
        assert_eq!(parsed["status"], "failed");
        assert_eq!(parsed["error"], "session busy");
        assert!(parsed["run_id"].is_null());
        // `text` has nothing to print for a failed turn; the reason is the
        // stderr diagnostic and the exit code.
        assert_eq!(report.stdout_payload(OutputFormat::Text), None);
    }

    /// `error_kind` sits next to `error` and is present exactly when it is, so
    /// a caller can branch on the kind of failure without reading the message.
    #[test]
    fn json_reports_the_kind_of_failure_beside_its_message() {
        let session_id = SessionId::from_string("s-kind");
        let parsed = |reason: TurnReason| -> serde_json::Value {
            let report =
                RunReport::new(&session_id, &outcome(reason, None, true), CONFIGURED_LIMIT);
            let json = report
                .stdout_payload(OutputFormat::Json)
                .expect("json always prints an object");
            serde_json::from_str(&json).expect("json is parseable")
        };

        for kind in [
            FailureKind::Auth,
            FailureKind::RateLimit,
            FailureKind::Overloaded,
            FailureKind::Timeout,
            FailureKind::Network,
            FailureKind::InvalidRequest,
            FailureKind::Other,
        ] {
            let object = parsed(TurnReason::Failed(TurnFailure::new(kind, "it failed")));
            assert_eq!(object["error"], "it failed");
            assert_eq!(object["error_kind"], kind.label());
        }

        // A failure that is not a model-call failure is `other`.
        let object = parsed(TurnReason::Failed("session busy".into()));
        assert_eq!(object["error_kind"], "other");

        // No error, no kind: a consumer keying off either sees the same thing.
        for reason in [
            TurnReason::Completed,
            TurnReason::Interrupted,
            TurnReason::CeilingReached,
        ] {
            let object = parsed(reason);
            assert!(object["error"].is_null());
            assert!(object["error_kind"].is_null());
        }
    }

    /// The `json` object states the output token limit the turn ran under, so
    /// a benchmark number taken from it can say what bounded the run.
    #[test]
    fn json_reports_the_output_token_limit_the_run_ran_under() {
        let session_id = SessionId::from_string("s-limit");
        let json = RunReport::new(
            &session_id,
            &outcome(TurnReason::Completed, Some("ok"), true),
            CONFIGURED_LIMIT,
        )
        .stdout_payload(OutputFormat::Json)
        .expect("json always prints an object");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
        assert_eq!(parsed["max_output_tokens"], CONFIGURED_LIMIT);

        // A run that configured nothing reports the provider's own default,
        // not a zero or an absent field.
        let json = RunReport::new(
            &session_id,
            &outcome(TurnReason::Completed, Some("ok"), true),
            DEFAULT_MAX_TOKENS.get(),
        )
        .stdout_payload(OutputFormat::Json)
        .expect("json always prints an object");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is parseable");
        assert_eq!(parsed["max_output_tokens"], 4096);
        assert_eq!(parsed["max_output_tokens"], DEFAULT_MAX_TOKENS.get());
    }

    #[test]
    fn a_completed_turn_exits_zero_and_an_interrupted_turn_exits_130() {
        let session_id = SessionId::from_string("s-4");
        assert_eq!(
            RunReport::new(
                &session_id,
                &outcome(TurnReason::Completed, Some("ok"), true),
                CONFIGURED_LIMIT,
            )
            .exit_code(),
            0
        );
        let interrupted = RunReport::new(
            &session_id,
            &outcome(TurnReason::Interrupted, None, true),
            CONFIGURED_LIMIT,
        );
        assert_eq!(interrupted.exit_code(), 130);
        assert_eq!(interrupted.status, "interrupted");
        assert!(interrupted.error_line().is_none());
    }
}
