//! Gate-plan composition: a task-specific execution graph assembled from
//! small composable blocks, rather than chosen whole from the fixed recipe
//! grammar [`super::composition`] resolves.
//!
//! A planner — [`scripted_plan`] today, [`model_plan`] when a provider is
//! configured — submits a [`GatePlan`] naming keys into the sealed coding
//! block catalogue ([`super::gate_blocks::coding_block_catalogue`]).
//! [`model_plan`]'s own bounded JSON grammar accepts either a flat list of
//! blocks (optionally ordered by a [`model_grammar`] expression tree over
//! them) or a full [`GateExpr`] tree (`Seq`/`Choose`/`Switch`/`Fork`/
//! `Loop`/`Repeat` — see [`ModelGateExprAnswer`]), plus optional typed
//! wires, naming only blocks, predicates, and discriminators
//! [`nameable_gate_blocks_for_selection`]/[`nameable_predicates`]/
//! [`nameable_discriminators`] expose from that same sealed catalogue.
//! [`model_plan_for_selection`] is the same planner under a
//! [`super::domains::DomainSelection`]: only
//! [`super::domains::Arm::Composed`] changes what it offers and says.
//! [`attempt_gate_plan`] resolves it through
//! [`polaris_cosmos::composition::materialize`], which either returns a
//! validated native graph or a list of actionable [`Refusal`]s. A refusal
//! feeds the next attempt: the planner submits a whole new plan (never a
//! patch to the refused one), up to [`MAX_GATE_PLAN_ATTEMPTS`]. Once that
//! budget is spent with nothing admitted, the caller falls back to the fixed
//! recipe grammar; a gate-plan attempt is never itself recorded as having
//! satisfied the coding contract unless one of its plans actually
//! materialised and ran.
//!
//! [`attempt_gate_plan`] returns every attempt it made, oldest first, as
//! plain data ([`GatePlanAttemptResult`]) — a choice, a gate-plan identity,
//! an attempt number, and either a materialised graph or a refusal — with no
//! evidence-store sequence number attached to any of it. Composition
//! evidence is checked against the caller's own real evidence store the
//! instant it is appended there, and only the store's owner knows the
//! position its *next* append will actually land on; a sequence minted
//! anywhere else would be a guess the store could reject. The caller
//! (today, [`super::host`]'s live selection point) is therefore the one that
//! turns this plain data into
//! [`polaris_cosmos::contracts::CompositionDecision`]/
//! [`polaris_cosmos::contracts::CompositionValidation`] pairs and appends
//! them in order — every refused attempt before the eventual admitted one,
//! or before the fallback once every attempt is refused — because only it
//! knows whether, and how, an admitted graph went on to run.

mod model_grammar;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU32;

use polaris::graph::Graph;
use polaris::models::ModelRegistry;
use polaris::models::llm::{LlmRequest, Message, ToolDefinition};
use polaris_cosmos::composition::{
    BlockDescription, BlockInstanceId, CaseKey, CompositionCatalogueSnapshot, CompositionLimits,
    ConfigId, DiscriminatorKey, Effect, FactKey, GateExpr, GatePath, GatePlan, LoopControl,
    PredicateKey, Refusal, StablePortKey, StepPlan, SwitchCase, materialize,
};
use polaris_cosmos::contracts::{
    CompositionBlockKey, CompositionBlockRef, CompositionChoice, CompositionChoiceOrigin,
    CompositionRecipeId, CompositionRecipeKey, CompositionRecipeRef, ConfigurationSchemaId,
    ConfigurationSchemaKey, ConfigurationValue, ContentDigest, ContractError, ExclusionCode,
    GateExpressionShapeKey, GatePlanRecord, MAX_COMPOSITION_BLOCKS, NativeGraphSignatureWitness,
    ResolvedConfigurationRef, RevisionAttempt,
};
use polaris_cosmos::evidence::gate_plan_record as build_gate_plan_record;
use polaris_cosmos::topology::TopologyCatalogSnapshot;
use polaris_cosmos::topology::TopologyContractKey;
use serde::Deserialize;

use super::block_cards;
use super::composition;
use super::gate_blocks::configurable::{
    ContextMode, ModelStepConfig, ModelStepOutputKind, TokenBudget, ToolSubset, TrimPolicy,
};
use super::gate_blocks::{
    configurable, existing, fine, keep_last_iteration_key, no_config_schema, predicates, repair,
    reproduce, twm,
};
use super::{behavior_version, policy};
use crate::usage::TokenMeter;

/// Lean arm options and its single-conversation planner.
pub mod lean;

/// Bound on the task statement text carried into [`PlannerFacts`] and sent
/// into a model planner's prompt.
///
/// A real problem statement (a bug report, an issue body) can run to
/// several thousand characters; a planner prompt and the Cosmos evidence it
/// is billed against both need a stated ceiling rather than an unbounded
/// string riding along as "just data".
pub const MAX_PLANNER_STATEMENT_CHARS: usize = 4_000;

/// Truncates `statement` to [`MAX_PLANNER_STATEMENT_CHARS`], on a char
/// boundary, marking that it was cut rather than silently dropping the tail.
fn bounded_statement(statement: &str) -> String {
    if statement.chars().count() <= MAX_PLANNER_STATEMENT_CHARS {
        return statement.to_owned();
    }
    let truncated: String = statement
        .chars()
        .take(MAX_PLANNER_STATEMENT_CHARS)
        .collect();
    format!("{truncated}… (truncated)")
}

/// How many whole-plan attempts one turn's revision loop may make: the
/// first plan plus at most two revisions after a refusal.
pub const MAX_GATE_PLAN_ATTEMPTS: u8 = 3;

/// Stable identity of the synthetic finite-composition recipe a gate-plan
/// decision is described under.
///
/// A gate plan has no fixed recipe name of its own — it is a tree, not a
/// grammar member — but [`polaris_cosmos::contracts::CompositionDecision`]
/// still carries an ordinary
/// [`polaris_cosmos::contracts::CompositionPlan`] envelope so a gate-plan
/// composition is reported and audited exactly like a finite-recipe one.
/// This name marks that envelope as gate-plan-sourced; the real identity is
/// the attached [`GatePlanRecord`].
pub const GATE_PLAN_RECIPE: &str = "polaris-code.recipe.gate-plan";

/// The output limit of one planner request, sent only under a cost cap.
const PLANNER_MAX_OUTPUT_TOKENS: NonZeroU32 = NonZeroU32::new(4096).unwrap();
const GATE_PLAN_CONFIGURATION_SCHEMA: &str = "polaris-code.gate-plan-composition-configuration";

/// Policy identity for the deterministic, fact-keyed gate planner.
pub const SCRIPTED_MODE: &str = "polaris-code.gate-planner-scripted";

/// Policy identity for the model-chosen gate planner.
pub const MODEL_MODE: &str = "polaris-code.gate-planner-model";

/// Which planner assembles a gate plan: a fixed function of the task's own
/// facts, or a model choosing from a bounded JSON grammar.
///
/// Orthogonal to [`composition::CompositionPolicy::Gates`]: that policy
/// value routes a turn through gate-plan composition at all; this chooses
/// which of the two gate planners builds the plan once it has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GatePlannerMode {
    /// [`scripted_plan`]: same facts always resolve to the same shape.
    #[default]
    Scripted,
    /// [`model_plan`]: a configured model chooses steps from a bounded
    /// grammar.
    Model,
}

impl polaris::system::resource::GlobalResource for GatePlannerMode {}

/// Host-overridable materialisation bounds for gate-plan composition,
/// installed as a session resource the same way [`GatePlannerMode`] is.
///
/// A newtype over [`CompositionLimits`] rather than the Cosmos type itself:
/// a resource type must be defined in this crate to implement
/// [`GlobalResource`](polaris::system::resource::GlobalResource) for it. No
/// resource means [`CompositionLimits::conservative`].
#[derive(Debug, Clone, Copy)]
pub struct GatePlanLimits(pub CompositionLimits);

impl Default for GatePlanLimits {
    fn default() -> Self {
        Self(CompositionLimits::conservative())
    }
}

impl polaris::system::resource::GlobalResource for GatePlanLimits {}

impl GatePlannerMode {
    /// Stable policy name carried in local evidence and headless reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scripted => SCRIPTED_MODE,
            Self::Model => MODEL_MODE,
        }
    }

    /// Parses the explicit host configuration spelling.
    pub fn parse(value: &str) -> Result<Self, PlannerError> {
        match value {
            "scripted" => Ok(Self::Scripted),
            "model" => Ok(Self::Model),
            _ => Err(PlannerError::InvalidConfiguration(format!(
                "unknown gate planner mode '{value}'; expected scripted or model"
            ))),
        }
    }
}

/// The typed task view a gate planner reads, deliberately excluding
/// benchmark identity and mutable graph objects — the same discipline
/// [`composition::CompositionFacts`] follows.
#[derive(Debug, Clone)]
pub struct PlannerFacts {
    /// Registration selected by the sealed topology catalog.
    pub registration: String,
    /// Caller-declared scope cardinality.
    pub scope_size: usize,
    /// Stable success-check kind.
    pub success_check: &'static str,
    /// Task objective projected from the accepted coding intent.
    pub objective: &'static str,
    /// Requester's latency limit in milliseconds, if actually stated.
    pub latency_budget_ms: Option<u64>,
    /// Whether the host declared a test runner available.
    pub test_runner_available: bool,
    /// Whether the session's shell policy would allow the reproduce script's
    /// own run command, independent of [`test_runner_available`](Self::test_runner_available).
    ///
    /// [`test_runner_available`](Self::test_runner_available) reflects a
    /// task's own stated [`SuccessCheck::Command`](super::task::SuccessCheck::Command)
    /// (or an explicit host declaration) and feeds every composition policy,
    /// including which topologies even become available — see
    /// `effective_host_capabilities` in `host.rs`. This fact is narrower on
    /// purpose: it answers only "could this plan's own fixed reproduce-run
    /// command execute," checked the same way
    /// `effective_host_capabilities` checks a stated command, and it is read
    /// only by the statement-only branch ([`statement_only_plan_body`], which
    /// [`scripted_plan`] and every domain arm built on it share) and by the
    /// model planner's fact-gated block list. Folding it into
    /// `test_runner_available` instead would also change topology
    /// availability and the finite-recipe chooser for every stated-edit and
    /// survey task in an interactive session, which is not this fact's job —
    /// see `docs/gate-planning.md`.
    pub reproduce_runner_available: bool,
    /// Whether the host declared a model provider available.
    pub model_provider_available: bool,
    /// Whether a shell policy on this host would let a bounded model step
    /// run its own `python` scripts — the self-check plan's own dependency,
    /// distinct from both [`Self::test_runner_available`] (whether the
    /// *project's* own test command can run) and
    /// [`Self::reproduce_runner_available`] (whether this crate's own fixed
    /// reproduce-run command specifically can run): a task can have any
    /// combination of the three. See
    /// [`super::host::effective_host_capabilities`] for the analogous probe
    /// `test_runner_available` is built from; this one is computed
    /// separately in `host.rs` rather than folded into
    /// [`super::host::HostCapabilities`], to keep this fact's blast radius
    /// out of topology admission. `scripted_plan` prefers this fact's own
    /// self-check shape over [`Self::reproduce_runner_available`]'s shape
    /// when both hold — see `docs/gate-planning.md`.
    pub shell_available: bool,
    /// Whether the task is a read-only question rather than a stated edit.
    pub is_question: bool,
    /// Whether the task is a read-only survey: a declared scope and a marker
    /// to look for, with no edit to apply. See
    /// [`CodingTask::is_survey`](super::task::CodingTask::is_survey).
    pub is_survey: bool,
    /// Whether the caller stated at least one exact edit.
    ///
    /// A unit of work described only by its problem statement — the normal
    /// case for a caller who has not pre-computed the change — states none,
    /// and neither [`is_question`](Self::is_question) nor
    /// [`is_survey`](Self::is_survey) covers it: it names an edit to make,
    /// not a question, and it names no scope or marker for a survey to scan.
    /// A planner that ignores this fact and applies the (empty) stated edits
    /// anyway "succeeds" without writing anything.
    pub has_stated_edits: bool,
    /// The problem statement in the requester's own words, bounded at
    /// [`MAX_PLANNER_STATEMENT_CHARS`] and sent as data — never interpolated
    /// into anything but a model prompt's own text.
    pub statement: String,
}

impl PlannerFacts {
    /// Builds planner facts from the same facts the finite-recipe chooser
    /// reads, adding the further facts neither planner needs today but a
    /// future one can key a plan on without a host-side change: whether the
    /// task is a read-only question, whether it is a read-only survey,
    /// whether it stated any edit at all, and its own problem statement.
    #[must_use]
    pub fn from_composition_facts(
        facts: composition::CompositionFacts,
        is_question: bool,
        is_survey: bool,
        has_stated_edits: bool,
        reproduce_runner_available: bool,
        statement: &str,
        shell_available: bool,
    ) -> Self {
        Self {
            registration: facts.registration,
            scope_size: facts.scope_size,
            success_check: facts.success_check,
            objective: facts.objective,
            latency_budget_ms: facts.latency_budget_ms,
            test_runner_available: facts.test_runner_available,
            reproduce_runner_available,
            model_provider_available: facts.model_provider_available,
            shell_available,
            is_question,
            is_survey,
            has_stated_edits,
            statement: bounded_statement(statement),
        }
    }
}

/// Why a gate planner could not produce a submittable plan.
#[derive(Debug)]
pub enum PlannerError {
    /// A Cosmos contract rejected a value this module built.
    Contract(ContractError),
    /// The requested planner configuration was invalid.
    InvalidConfiguration(String),
    /// The gate plan names more block instances than a composition envelope
    /// can describe.
    TooManyBlocks(usize),
    /// No configured model can serve the model planner.
    ModelUnavailable,
    /// Model transport or generation failed before any answer was billed.
    Model(String),
    /// A billed model answer failed the bounded grammar's own checks — bad
    /// JSON, an unknown block name, a duplicate instance label, or an
    /// unusable configuration. Carries the digests of the prompt and
    /// response the meter already billed for this attempt, so a refused
    /// answer stays auditable evidence rather than a silent discard of a
    /// real call.
    InvalidModelAnswer {
        /// What was wrong with the answer.
        detail: String,
        /// Digest of the prompt sent for this attempt.
        prompt_digest: ContentDigest,
        /// Digest of the raw response received for this attempt.
        response_digest: ContentDigest,
    },
}

impl fmt::Display for PlannerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => error.fmt(f),
            Self::InvalidConfiguration(detail) => {
                write!(f, "planner.invalid-configuration: {detail}")
            }
            Self::TooManyBlocks(count) => write!(
                f,
                "planner.too-many-blocks: gate plan names {count} block instances, more than the \
                 {MAX_COMPOSITION_BLOCKS} a composition envelope can describe"
            ),
            Self::ModelUnavailable => f.write_str("planner.model-unavailable"),
            Self::Model(error) => write!(f, "planner.model-error: {error}"),
            Self::InvalidModelAnswer { detail, .. } => {
                write!(f, "planner.invalid-model-answer: {detail}")
            }
        }
    }
}

impl std::error::Error for PlannerError {}

impl From<ContractError> for PlannerError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

fn no_config_id() -> Result<ConfigId, PlannerError> {
    Ok(ConfigId::new("no-config")?)
}

fn no_config_ref() -> Result<ResolvedConfigurationRef, PlannerError> {
    Ok(ResolvedConfigurationRef::inline(
        no_config_schema(),
        ConfigurationValue::Null,
    )?)
}

fn step_expr(
    instance: &str,
    block: CompositionBlockKey,
    config: ConfigId,
) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Step(StepPlan::new(
        BlockInstanceId::new(instance)?,
        block,
        config,
    )))
}

/// Plan shape for a single-target stated edit with no test runner:
/// `Seq(apply-direct, judge-stated-edits, conclude-single-pass)`.
fn direct_edit_expression(config: &ConfigId) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Seq(vec![
        step_expr("apply", existing::apply_direct_key(), config.clone())?,
        step_expr("judge", existing::judge_stated_edits_key(), config.clone())?,
        step_expr(
            "conclude",
            existing::conclude_single_pass_key(),
            config.clone(),
        )?,
    ]))
}

/// Plan shape for a multi-target stated edit with no test runner: the
/// bounded worklist sweep over every stated site, in one block, rather than
/// the single-target direct-edit sequence — the same distinction the
/// finite-recipe grammar already draws between its `direct` and
/// `bounded-worklist` recipes.
fn worklist_sweep_expression(config: &ConfigId) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Seq(vec![step_expr(
        "sweep",
        existing::drain_worklist_sweep_key(),
        config.clone(),
    )?]))
}

/// Plan shape for a read-only question, writing nothing:
/// `Seq(survey-tree, assess-findings, answer-question)`.
fn question_expression(config: &ConfigId) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Seq(vec![
        step_expr("survey", existing::survey_tree_key(), config.clone())?,
        step_expr("assess", existing::assess_findings_key(), config.clone())?,
        step_expr("answer", existing::answer_question_key(), config.clone())?,
    ]))
}

/// Plan shape for a statement-only edit task with no test runner: nothing
/// here for [`direct_edit_expression`] to apply, so the model itself proposes
/// the whole patch — using its own bounded `read_file`, `list_dir` and
/// `search` tools to locate what to change before proposing it.
/// `Seq(model-propose, apply-patch-intent, judge-patch-intent,
/// conclude-single-pass)`.
///
/// Judged by [`repair::judge_patch_intent_key`], not
/// [`existing::judge_stated_edits_key`]: the latter judges the *task's own*
/// stated edits, of which a statement-only task has none, so it would judge
/// nothing and the single pass would "succeed" at having changed nothing.
/// [`repair::JudgePatchIntentBlock`] judges the edits the model itself
/// proposed instead, and treats an empty proposal as a failure, never a
/// vacuous pass.
fn model_proposed_edit_expression(
    config: &ConfigId,
    model_config: &ConfigId,
) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Seq(vec![
        step_expr(
            "propose",
            configurable::model_propose_key(),
            model_config.clone(),
        )?,
        step_expr(
            "apply",
            configurable::apply_patch_intent_key(),
            config.clone(),
        )?,
        step_expr("judge", repair::judge_patch_intent_key(), config.clone())?,
        step_expr(
            "conclude",
            existing::conclude_single_pass_key(),
            config.clone(),
        )?,
    ]))
}

/// Plan shape for a statement-only edit task with a test runner, keyed by
/// revision attempt: the same two-level repair depth
/// [`test_gated_expression`] uses, except every propose step is a model call
/// from the start — there is no stated edit for a first pass to apply
/// directly — and a repair attempt reads the *captured* output of the test
/// run that preceded it
/// ([`repair::run_project_tests_captured_key`]/[`repair::judge_test_outcome_key`])
/// rather than only its verdict, via `model_config`'s
/// [`ModelStepConfig::sees_prior_failure`]. So a retry actually sees why the
/// previous attempt did not pass, not only that it did not.
///
/// Kept as hand-unrolled nested `Choose` gates for the same reason
/// [`test_gated_expression`] already is: a repair attempt applies a patch,
/// `Mutating { repeat_safe: false }`, which may not sit in a `Loop`/`Repeat`
/// body unless it is wired to a fresh producer inside that same body on every
/// iteration — real machinery this plan does not need to reach for at this
/// depth. See `docs/gate-planning.md` for why this remains a documented
/// simplification opportunity rather than something this shape had to solve.
fn model_test_gated_expression(
    config: &ConfigId,
    model_config: &ConfigId,
    repair_model_config: &ConfigId,
    attempt: RevisionAttempt,
) -> Result<GateExpr, PlannerError> {
    let repair_once = |propose: &str,
                       propose_config: &ConfigId,
                       apply: &str,
                       tests: &str,
                       judge: &str|
     -> Result<GateExpr, PlannerError> {
        Ok(GateExpr::Seq(vec![
            step_expr(
                propose,
                configurable::model_propose_key(),
                propose_config.clone(),
            )?,
            step_expr(
                apply,
                configurable::apply_patch_intent_key(),
                config.clone(),
            )?,
            step_expr(
                tests,
                repair::run_project_tests_captured_key(),
                config.clone(),
            )?,
            step_expr(judge, repair::judge_test_outcome_key(), config.clone())?,
        ]))
    };
    let repair_body = if attempt.get() == 1 {
        let innermost = repair_once(
            "propose-2",
            repair_model_config,
            "apply-patch-2",
            "run-tests-3",
            "judge-3",
        )?;
        let inner_choose = GateExpr::Choose {
            predicate: super::gate_blocks::predicates::tests_passed_key(),
            when_true: Box::new(step_expr(
                "affirm-2",
                configurable::affirm_assessment_key(),
                config.clone(),
            )?),
            when_false: Box::new(innermost),
        };
        let GateExpr::Seq(mut outer) = repair_once(
            "propose-1",
            repair_model_config,
            "apply-patch-1",
            "run-tests-2",
            "judge-2",
        )?
        else {
            unreachable!("repair_once always returns Seq")
        };
        outer.push(inner_choose);
        GateExpr::Seq(outer)
    } else {
        repair_once(
            "propose-1",
            repair_model_config,
            "apply-patch-1",
            "run-tests-2",
            "judge-2",
        )?
    };
    let outer_choose = GateExpr::Choose {
        predicate: super::gate_blocks::predicates::tests_passed_key(),
        when_true: Box::new(step_expr(
            "affirm-1",
            configurable::affirm_assessment_key(),
            config.clone(),
        )?),
        when_false: Box::new(repair_body),
    };
    Ok(GateExpr::Seq(vec![
        step_expr(
            "propose-0",
            configurable::model_propose_key(),
            model_config.clone(),
        )?,
        step_expr(
            "apply-0",
            configurable::apply_patch_intent_key(),
            config.clone(),
        )?,
        step_expr(
            "run-tests-1",
            repair::run_project_tests_captured_key(),
            config.clone(),
        )?,
        step_expr("judge-1", repair::judge_test_outcome_key(), config.clone())?,
        outer_choose,
        step_expr(
            "conclude",
            existing::conclude_single_pass_key(),
            config.clone(),
        )?,
    ]))
}

/// Plan shape for a statement-only edit task with *no stated test command*
/// but a shell policy that would run one: the same repair loop
/// [`model_test_gated_expression`] builds, preceded by a model step that
/// writes a small script reproducing the issue from the task's own
/// statement, and a first run of that script establishing that it actually
/// reproduces the bug before the code-fix loop begins.
///
/// `model_test_gated_expression`'s own `run-tests-1` (and any repair
/// attempt's own run) re-runs the *same* script, now against the edited
/// tree, via the [`ProjectTestRunner`](super::payload::ProjectTestRunner) the
/// host installs for this shape — see
/// [`gate_blocks::reproduce`](super::gate_blocks::reproduce). Composed rather
/// than duplicated: the four steps below are the only thing this shape adds,
/// and the reused suffix is [`model_test_gated_expression`]'s own return
/// value, unmodified — except that this shape always asks it to build its
/// *shallow*, single-repair-level tree, regardless of the caller's own
/// `attempt`.
///
/// [`MAX_COMPOSITION_BLOCKS`] bounds every gate plan at 16 block instances,
/// and `model_test_gated_expression`'s own deep, two-level tree (built for
/// `attempt.get() == 1`) is already 15 of those on its own — no room left for
/// even one more step, let alone this shape's four-step prefix. Passing a
/// fixed revision attempt of `2` asks for its shallow, one-level tree (10
/// steps) instead, unconditionally: `4 + 10 = 14`, two under the cap. A gate
/// plan carries no record of which attempt built it beyond its own shape, so
/// this costs the *reproduce* shape one level of in-graph repair depth
/// (still one retry, backed by the captured failure, same as a stated edit's
/// own shallow revision shape) — never the caller's real revision loop, which
/// [`attempt_gate_plan`] still drives across whole fresh plans exactly as it
/// does for every other shape.
fn model_reproduce_gated_expression(
    config: &ConfigId,
    model_config: &ConfigId,
    repair_model_config: &ConfigId,
    reproduce_model_config: &ConfigId,
) -> Result<GateExpr, PlannerError> {
    let mut steps = vec![
        step_expr(
            "propose-reproduce",
            configurable::model_propose_key(),
            reproduce_model_config.clone(),
        )?,
        step_expr(
            "write-reproduce",
            reproduce::write_reproduce_script_key(),
            config.clone(),
        )?,
        step_expr(
            "baseline-run",
            repair::run_project_tests_captured_key(),
            config.clone(),
        )?,
        step_expr(
            "baseline-judge",
            repair::judge_test_outcome_key(),
            config.clone(),
        )?,
    ];
    let shallow_attempt = RevisionAttempt::new(2)?;
    let GateExpr::Seq(suffix) =
        model_test_gated_expression(config, model_config, repair_model_config, shallow_attempt)?
    else {
        unreachable!("model_test_gated_expression always returns Seq")
    };
    steps.extend(suffix);
    Ok(GateExpr::Seq(steps))
}

/// Plan shape for a statement-only edit task with no project test runner but
/// a shell policy that would let a bounded model step run its own scripts:
/// the model proposes a patch *and, optionally, a `check` script that
/// reproduces the issue* in one conversation
/// ([`repair::model_propose_conversation_key`]), the patch is applied, and —
/// only when a check was actually submitted — the check runs
/// ([`repair::run_check_script_key`]) and gates a repair loop that *resumes
/// the same conversation* on a behavior failure
/// ([`repair::resume_model_propose_key`]) instead of
/// starting a fresh one that would have to re-read the code from scratch.
/// A model that submits no check falls back to the plain
/// [`model_proposed_edit_expression`] shape: judged by
/// [`repair::judge_patch_intent_key`], no self-check loop at all.
///
/// `Seq(propose-0, extract-0, apply-0, Choose(has-check:
///   Seq(check-1, judge-1, Choose(check-needs-repair: repair...; affirm-1));
///   Seq(judge-patch-intent)), conclude)`.
///
/// Kept as hand-unrolled nested `Choose` gates for the same reason
/// [`model_test_gated_expression`] already is: a repair attempt applies a
/// patch, `Mutating { repeat_safe: false }`, which may not sit in a
/// `Loop`/`Repeat` body unless wired to a fresh producer inside that same
/// body on every iteration. See `docs/gate-planning.md`.
///
/// This is the shape [`scripted_plan`] prefers over
/// [`model_reproduce_gated_expression`] when both
/// [`PlannerFacts::shell_available`] and
/// [`PlannerFacts::reproduce_runner_available`] hold: it folds proposing and
/// reproducing into the same conversation instead of a dedicated
/// write-then-propose pair, and its own repair round resumes that
/// conversation rather than starting a fresh one — see `docs/gate-planning.md`
/// for the precedence and why it is not simply the other shape's
/// replacement.
fn self_check_gated_expression(
    config: &ConfigId,
    model_config: &ConfigId,
    repair_model_config: &ConfigId,
    _attempt: RevisionAttempt,
) -> Result<GateExpr, PlannerError> {
    let repair_once = |propose: &str,
                       extract: &str,
                       apply: &str,
                       check: &str,
                       judge: &str|
     -> Result<GateExpr, PlannerError> {
        Ok(GateExpr::Seq(vec![
            step_expr(
                propose,
                repair::resume_model_propose_key(),
                repair_model_config.clone(),
            )?,
            step_expr(
                extract,
                repair::extract_patch_from_proposal_key(),
                config.clone(),
            )?,
            step_expr(
                apply,
                configurable::apply_patch_intent_key(),
                config.clone(),
            )?,
            step_expr(check, repair::run_check_script_key(), config.clone())?,
            step_expr(judge, repair::judge_test_outcome_key(), config.clone())?,
        ]))
    };
    // Two repair rounds would name 19 block instances, over the
    // composition envelope's limit, so every attempt gets one.
    let repair_body = repair_once(
        "propose-1",
        "extract-1",
        "apply-patch-1",
        "check-2",
        "judge-2",
    )?;
    let outer_choose = GateExpr::Choose {
        predicate: super::gate_blocks::predicates::check_needs_repair_key(),
        when_false: Box::new(step_expr(
            "affirm-1",
            configurable::affirm_assessment_key(),
            config.clone(),
        )?),
        when_true: Box::new(repair_body),
    };
    let with_check = GateExpr::Seq(vec![
        step_expr("check-1", repair::run_check_script_key(), config.clone())?,
        step_expr("judge-1", repair::judge_test_outcome_key(), config.clone())?,
        outer_choose,
    ]);
    let without_check = GateExpr::Seq(vec![step_expr(
        "judge-0",
        repair::judge_patch_intent_key(),
        config.clone(),
    )?]);
    let has_check_choose = GateExpr::Choose {
        predicate: super::gate_blocks::predicates::has_check_key(),
        when_true: Box::new(with_check),
        when_false: Box::new(without_check),
    };
    Ok(GateExpr::Seq(vec![
        step_expr(
            "propose-0",
            repair::model_propose_conversation_key(),
            model_config.clone(),
        )?,
        step_expr(
            "extract-0",
            repair::extract_patch_from_proposal_key(),
            config.clone(),
        )?,
        step_expr(
            "apply-0",
            configurable::apply_patch_intent_key(),
            config.clone(),
        )?,
        has_check_choose,
        step_expr(
            "conclude",
            existing::conclude_single_pass_key(),
            config.clone(),
        )?,
    ]))
}

/// Plan shape for a stated edit with a test runner, keyed by revision
/// attempt.
///
/// The first attempt is a two-level repair loop, unrolled as nested
/// `Choose` gates rather than a native `Loop` (a repair attempt applies a
/// patch, which is `Mutating { repeat_safe: false }` and so may not sit in a
/// `Loop` body): it retries a second time before giving up. Every attempt
/// after a refusal degrades to a shallower one-level shape — a single
/// repair pass with no further retry — so a limit that rejected the first
/// attempt's depth has a genuinely different, shallower shape to admit
/// instead of an identical resubmission.
fn test_gated_expression(
    config: &ConfigId,
    model_config: &ConfigId,
    attempt: RevisionAttempt,
) -> Result<GateExpr, PlannerError> {
    let repair_once = |propose: &str, apply: &str, tests: &str| -> Result<GateExpr, PlannerError> {
        Ok(GateExpr::Seq(vec![
            step_expr(
                propose,
                configurable::model_propose_key(),
                model_config.clone(),
            )?,
            step_expr(
                apply,
                configurable::apply_patch_intent_key(),
                config.clone(),
            )?,
            step_expr(tests, existing::run_project_tests_key(), config.clone())?,
        ]))
    };
    let repair_body = if attempt.get() == 1 {
        let innermost = repair_once("propose-2", "apply-patch-2", "run-tests-3")?;
        let inner_choose = GateExpr::Choose {
            predicate: super::gate_blocks::predicates::tests_passed_key(),
            when_true: Box::new(step_expr(
                "affirm-2",
                configurable::affirm_assessment_key(),
                config.clone(),
            )?),
            when_false: Box::new(innermost),
        };
        let GateExpr::Seq(mut outer) = repair_once("propose-1", "apply-patch-1", "run-tests-2")?
        else {
            unreachable!("repair_once always returns Seq")
        };
        outer.push(inner_choose);
        GateExpr::Seq(outer)
    } else {
        repair_once("propose-1", "apply-patch-1", "run-tests-2")?
    };
    let outer_choose = GateExpr::Choose {
        predicate: super::gate_blocks::predicates::tests_passed_key(),
        when_true: Box::new(step_expr(
            "affirm-1",
            configurable::affirm_assessment_key(),
            config.clone(),
        )?),
        when_false: Box::new(repair_body),
    };
    Ok(GateExpr::Seq(vec![
        step_expr("apply", existing::apply_direct_key(), config.clone())?,
        step_expr(
            "run-tests-1",
            existing::run_project_tests_key(),
            config.clone(),
        )?,
        outer_choose,
        step_expr(
            "conclude",
            existing::conclude_single_pass_key(),
            config.clone(),
        )?,
    ]))
}

fn scripted_model_step_config() -> Result<ResolvedConfigurationRef, PlannerError> {
    Ok(ModelStepConfig::new(
        "revise".to_owned(),
        "propose the fix that makes the failing check pass".to_owned(),
        ToolSubset::Only(vec!["read_file".to_owned()]),
        TokenBudget(4_000),
        ModelStepOutputKind::PatchIntent,
    )
    .encode()?)
}

/// Shared by every arm's edit step: the model works from the checkout it was given.
const WORKSPACE_ONLY_RULE: &str = "Work only with the checkout already in the workspace: do not download, install or fetch another version of the project or its upstream changes. Installing a missing third-party dependency is fine.";

/// Turn cap [`statement_edit_model_step_config`] gives the same step.
const STATEMENT_EDIT_MAX_TURNS: u16 = 30;

/// Token budget [`statement_edit_model_step_config`] gives a statement-only
/// edit's own `model-propose` step. Exactly
/// [`MAX_MODEL_STEP_TOKEN_BUDGET`](configurable::MAX_MODEL_STEP_TOKEN_BUDGET):
/// this is the codebase's own most generous built-in case, generous enough
/// for a real repository's worth of `list_dir`/`search`/`read_file` calls
/// ahead of a patch (unlike [`scripted_model_step_config`]'s narrow
/// repair-only budget, which assumes the site is already known from a
/// failing check) — and that same constant is the ceiling a model-chosen
/// budget may not exceed either, so nothing this codebase ships ever asks
/// for more than the one number both sides share.
const STATEMENT_EDIT_TOKEN_BUDGET: u32 = configurable::MAX_MODEL_STEP_TOKEN_BUDGET;

/// Builds the `model-propose` configuration a statement-only edit task uses:
/// the bounded `read_file`/`list_dir`/`search` tool subset, a budget and turn
/// cap sized for exploring a real repository rather than one already-known
/// site, and — when `sees_prior_failure` — the flag that makes a retry read
/// the preceding attempt's captured test failure instead of being told only,
/// statically, to try again.
///
/// `submit_answer`'s own schema always accepts an optional `check` field
/// (see [`submit_answer_tool_definition`](super::gate_blocks::configurable)),
/// but only when `offer_check` is set does this also grant the `run_python`
/// tool and point the model at both: submitting a check is only worth doing
/// when the model can actually run it first to confirm it reproduces the
/// issue, which is what [`self_check_gated_expression`] needs a submitted
/// check for. The other call sites keep `offer_check` false — nothing
/// downstream of them ever reads `check`.
///
/// `notes` is domain knowledge appended verbatim after the ordinary
/// instruction: empty for the generic plan, and the selected domain's own
/// notes text otherwise. See `crate::cosmos::domains`.
fn statement_edit_model_step_config(
    statement: &str,
    sees_prior_failure: bool,
    offer_check: bool,
    notes: &str,
) -> Result<ResolvedConfigurationRef, PlannerError> {
    let mut tools = vec![
        "read_file".to_owned(),
        "list_dir".to_owned(),
        "search".to_owned(),
    ];
    let mut instruction = format!(
        "Propose the smallest behavior-changing patch that satisfies this issue:\n\
         {statement}\n\n\
         First list the concrete expected behaviors and every named operation or variant. \
         Inspect the code that produces and consumes the relevant value, using list_dir, \
         search, and read_file. Treat a search stopped by a file or output limit as \
         incomplete, and narrow the path. If a file result is truncated, read the needed \
         line range next; do not reread the same prefix. Before submitting, check each \
         proposed change against the statement's examples and edge cases. If you cannot \
         verify an item, state the uncertainty in your final reasoning."
    );
    if offer_check {
        tools.push("run_python".to_owned());
        // The submit_answer schema's own `check` field and its fail-before/
        // pass-after semantics are already explained in `output_directions`,
        // which every patch-intent step's final message carries regardless
        // of `offer_check` — this only adds what that shared text does not
        // know: that `run_python` exists to actually run one, and what
        // happens to a submitted one afterward.
        instruction.push_str(
            "\n\nrun_python lets you actually run a check script before you submit, so you \
             confirm firsthand that it fails now and passes after your patch rather than \
             assuming. A submitted check is run again for real once your patch lands; if it \
             still fails you get another attempt with its output, in this same conversation, \
             so you do not need to re-explore the code. Work only with the checkout already in \
             the workspace: do not download, install or fetch another version of the project \
             or its upstream changes. Installing a missing third-party dependency is fine.",
        );
    }
    if !notes.trim().is_empty() {
        instruction.push_str(&format!("\n\nDomain notes:\n{notes}"));
    }
    let config = ModelStepConfig::new(
        "propose-edit".to_owned(),
        instruction,
        ToolSubset::Only(tools),
        TokenBudget(STATEMENT_EDIT_TOKEN_BUDGET),
        ModelStepOutputKind::PatchIntent,
    )
    .with_max_turns(STATEMENT_EDIT_MAX_TURNS)
    // On for every statement-only-edit propose/repair step this plan
    // shape produces (never a judge — `judge_patch_intent` is a
    // deterministic check, not a model step at all): a first submit gets
    // one causal/coverage review round before it is accepted.
    .with_review_before_submit();
    let config = if sees_prior_failure {
        config.with_prior_failure_visible()
    } else {
        config
    };
    Ok(config.encode()?)
}

/// Token budget [`reproduce_script_model_step_config`] gives the reproduce
/// step: enough for a bounded look at the relevant code before writing a
/// small script, narrower than [`STATEMENT_EDIT_TOKEN_BUDGET`] because
/// reproducing a described symptom needs less exploration than locating and
/// writing its fix.
const REPRODUCE_SCRIPT_TOKEN_BUDGET: u32 = 60_000;

/// Turn cap [`reproduce_script_model_step_config`] gives the same step.
const REPRODUCE_SCRIPT_MAX_TURNS: u16 = 20;

/// Builds the `model-propose` configuration
/// [`model_reproduce_gated_expression`]'s `propose-reproduce` step uses: the
/// bounded `read_file`/`list_dir`/`search` tool subset, and an
/// [`ModelStepOutputKind::Answer`] artifact whose text is the script's own
/// source, written verbatim by [`gate_blocks::reproduce::write_reproduce_script_key`]'s
/// block — never parsed as an edit, so nothing here reaches
/// `apply_exact_edit`'s uniqueness check.
///
/// The instruction states the one property that makes the script usable as a
/// repair-loop signal: its exit status is read directly as pass or fail, so
/// it must actually depend on the bug's own behaviour rather than, say, an
/// unconditional failure. It also repeats the benchmark-honesty rule already
/// enforced by what this crate ever hands an agent: the model sees only the
/// task's own statement, never a hidden suite, so it is told to write its
/// own reproduction rather than reach for one.
fn reproduce_script_model_step_config(
    statement: &str,
) -> Result<ResolvedConfigurationRef, PlannerError> {
    let config = ModelStepConfig::new(
        "reproduce".to_owned(),
        format!(
            "Write a small, self-contained Python script that reproduces this issue:\n{statement}\
             \n\nUse list_dir, search and read_file to find the relevant code first. The script \
             must exit with a nonzero status (for example via an assertion or sys.exit(1)) when \
             the bug described above is present, and exit 0 when it is not — that exit status is \
             read directly as pass or fail, both now and again after a fix is applied, so make it \
             depend on the actual behaviour, not on which file exists. Do not read or reference the \
             project's own test suite or any hidden test file; write a standalone reproduction \
             instead. Reply with the script's source code only, as the answer text — no \
             explanation, no markdown fence."
        ),
        ToolSubset::Only(vec![
            "read_file".to_owned(),
            "list_dir".to_owned(),
            "search".to_owned(),
        ]),
        TokenBudget(REPRODUCE_SCRIPT_TOKEN_BUDGET),
        ModelStepOutputKind::Answer,
    )
    .with_max_turns(REPRODUCE_SCRIPT_MAX_TURNS);
    Ok(config.encode()?)
}

/// A statement-only edit task's own gate expression plus whatever extra
/// per-instance configurations it needs, still to be merged into the whole
/// plan's own `configurations` map by [`scripted_plan_generic`].
///
/// Shared by the generic plan (`notes` empty, called through
/// [`scripted_plan`]) and every domain variant's own statement-only plan
/// (`notes` the variant's own text) — see `crate::cosmos::domains`, whose
/// `plan_for_selection` calls this directly for the generic-plus-notes arm
/// and each domain variant's own `plan` calls it for its starting shape.
/// This is the one place `notes` reaches a model step's instruction, so the
/// `generic-notes` arm and a domain's own initial `expert`/`experience` plan
/// carry byte-identical notes text whenever they are given the same string.
///
/// Branch precedence, first match wins: `test_runner_available` gets
/// [`model_test_gated_expression`]; failing that, `shell_available` gets
/// [`self_check_gated_expression`]; failing that,
/// `reproduce_runner_available` gets [`model_reproduce_gated_expression`];
/// otherwise the bare [`model_proposed_edit_expression`]. See each
/// function's own doc, and `docs/gate-planning.md`, for why these are
/// distinct facts from `test_runner_available` (rather than a widening of
/// it) and from each other, and why `shell_available` takes precedence when
/// both it and `reproduce_runner_available` hold.
pub(crate) fn statement_only_plan_body(
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
    notes: &str,
) -> StatementPlanBody {
    let config = no_config_id()?;
    let model_config = ConfigId::new("propose-edit")?;
    let mut extra = Vec::new();
    let expression = if facts.test_runner_available {
        // A real project test runner outranks either script-based shape
        // below: it is the task's own authoritative verification, not a
        // one-off script written to convince itself.
        extra.push((
            model_config.clone(),
            statement_edit_model_step_config(&facts.statement, false, false, notes)?,
        ));
        let repair_model_config = ConfigId::new("propose-edit-repair")?;
        extra.push((
            repair_model_config.clone(),
            statement_edit_model_step_config(&facts.statement, true, false, notes)?,
        ));
        model_test_gated_expression(&config, &model_config, &repair_model_config, attempt)?
    } else if facts.shell_available {
        // No project test runner, but the shell policy would let a
        // bounded step run its own scripts: let the model see the bug
        // before it submits, and gate a same-conversation repair on
        // whether its own check passes. Takes precedence over
        // `reproduce_runner_available` below — see
        // `self_check_gated_expression`'s own doc and
        // `docs/gate-planning.md` for why the two are not the same
        // shape and why this one goes first when both hold.
        extra.push((
            model_config.clone(),
            statement_edit_model_step_config(&facts.statement, false, true, notes)?,
        ));
        let repair_model_config = ConfigId::new("propose-edit-repair")?;
        extra.push((
            repair_model_config.clone(),
            statement_edit_model_step_config(&facts.statement, true, true, notes)?,
        ));
        self_check_gated_expression(&config, &model_config, &repair_model_config, attempt)?
    } else if facts.reproduce_runner_available {
        // No stated test command and no general shell_available verdict,
        // but the shell policy would run the fixed reproduce-run command:
        // write and run a reproduce script instead of the bare
        // no-test-step shape below.
        extra.push((
            model_config.clone(),
            statement_edit_model_step_config(&facts.statement, false, false, notes)?,
        ));
        let repair_model_config = ConfigId::new("propose-edit-repair")?;
        extra.push((
            repair_model_config.clone(),
            statement_edit_model_step_config(&facts.statement, true, false, notes)?,
        ));
        let reproduce_model_config = ConfigId::new("reproduce-script")?;
        extra.push((
            reproduce_model_config.clone(),
            reproduce_script_model_step_config(&facts.statement)?,
        ));
        model_reproduce_gated_expression(
            &config,
            &model_config,
            &repair_model_config,
            &reproduce_model_config,
        )?
    } else {
        extra.push((
            model_config.clone(),
            statement_edit_model_step_config(&facts.statement, false, false, notes)?,
        ));
        model_proposed_edit_expression(&config, &model_config)?
    };
    Ok((expression, extra))
}

/// The `fine` arm replaces only the initial proposal with three bounded generic
/// stages. The original apply, test, judge, repair and conclude gates remain
/// the result of `statement_only_plan_body` above.
///
/// Built from [`statement_only_plan_body`] with
/// [`PlannerFacts::reproduce_runner_available`] cleared: this arm swaps out
/// the base plan's *first* step for its own three stages, and the
/// reproduce-script shape's first step is its `propose-reproduce` step, not
/// the edit proposal — splicing there would drop the script's author and
/// leave two `propose-0` instances. The `fine` arm carries its own `fine-reproduce`
/// stage instead, so it keeps the three shapes it was built against.
pub(crate) fn fine_statement_only_plan_body(
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
) -> StatementPlanBody {
    let base_facts = PlannerFacts {
        reproduce_runner_available: false,
        ..facts.clone()
    };
    let (expression, mut extra) = statement_only_plan_body(&base_facts, attempt, "")?;
    let locate_id = ConfigId::new("fine-locate")?;
    let reproduce_id = ConfigId::new("fine-reproduce")?;
    let propose_id = ConfigId::new("propose-edit")?;
    let can_run_python = facts.shell_available && !facts.test_runner_available;
    let read_tools = vec![
        "read_file".to_owned(),
        "list_dir".to_owned(),
        "search".to_owned(),
    ];
    let mut reproduce_tools = read_tools.clone();
    if can_run_python {
        reproduce_tools.push("run_python".to_owned());
    }
    let propose_tools = reproduce_tools.clone();
    let locate = ModelStepConfig::new(
        "fine-locate".to_owned(),
        format!("Locate the code responsible for this issue and summarize the relevant paths, symbols, and data flow in at most 8000 characters. Do not propose an edit yet.\n\nIssue:\n{}\n\n{}", facts.statement, WORKSPACE_ONLY_RULE),
        ToolSubset::Only(read_tools), TokenBudget(40_000), ModelStepOutputKind::Answer,
    ).with_max_turns(6).encode()?;
    let reproduce = ModelStepConfig::new(
        "fine-reproduce".to_owned(),
        format!("Determine the expected and current behavior for this issue. Choose a focused check; run it with run_python if available, otherwise explain how to run it. Summarize observed failure, uncertainties, and the check in at most 8000 characters. Do not edit or propose the final patch yet.\n\nIssue:\n{}\n\n{}", facts.statement, WORKSPACE_ONLY_RULE),
        ToolSubset::Only(reproduce_tools), TokenBudget(40_000), ModelStepOutputKind::Answer,
    ).with_max_turns(6).encode()?;
    let propose = ModelStepConfig::new(
        "fine-propose".to_owned(),
        format!("Propose the smallest behavior-changing exact-string patch for this issue. Use the location and reproduction findings below, inspect further if needed, and check the proposed changes against the issue's examples and edge cases. If run_python is available, run the focused check before submitting and include a check script with the patch when possible. State any unverified behavior in your reasoning.\n\nIssue:\n{}\n\n{}", facts.statement, WORKSPACE_ONLY_RULE),
        ToolSubset::Only(propose_tools), TokenBudget(120_000), ModelStepOutputKind::PatchIntent,
    ).with_max_turns(16).with_review_before_submit().encode()?;
    extra.retain(|(id, _)| id != &propose_id);
    // The generic arm's test-runner repair instruction omits the workspace rule
    // when run_python is not offered. The `fine` arm requires it in every model step.
    let repair_id = ConfigId::new("propose-edit-repair")?;
    if let Some((_, repair)) = extra.iter_mut().find(|(id, _)| id == &repair_id) {
        let mut decoded = ModelStepConfig::decode(repair)
            .map_err(|error| PlannerError::InvalidConfiguration(error.to_string()))?;
        if !decoded.instruction.contains(WORKSPACE_ONLY_RULE) {
            decoded
                .instruction
                .push_str(&format!("\n\n{WORKSPACE_ONLY_RULE}"));
            *repair = decoded.encode()?;
        }
    }
    extra.extend([
        (locate_id.clone(), locate),
        (reproduce_id.clone(), reproduce),
        (propose_id.clone(), propose),
    ]);
    let mut stages = vec![
        step_expr("fine-locate", fine::locate_key(), locate_id)?,
        step_expr("fine-reproduce", fine::reproduce_key(), reproduce_id)?,
        step_expr(
            "propose-0",
            if can_run_python {
                fine::propose_conversation_key()
            } else {
                fine::propose_key()
            },
            propose_id,
        )?,
    ];
    let GateExpr::Seq(mut tail) = expression else {
        unreachable!("statement-only plan is a sequence")
    };
    tail.remove(0);
    stages.append(&mut tail);
    Ok((GateExpr::Seq(stages), extra))
}

/// The `fine-ctx` arm keeps the `fine` arm's graph and budgets, changing only the context modes of its
/// three proposal stages. The first brief step has no prior record to read,
/// but captures the conversation that the next two steps continue.
pub(crate) fn fine_context_statement_only_plan_body(
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
) -> StatementPlanBody {
    let (expression, mut extra) = fine_statement_only_plan_body(facts, attempt)?;
    let locate_id = ConfigId::new("fine-locate")?;
    let reproduce_id = ConfigId::new("fine-reproduce")?;
    let propose_id = ConfigId::new("propose-edit")?;
    for (id, config) in &mut extra {
        let mode = if id == &locate_id {
            ContextMode::Brief
        } else if id == &reproduce_id || id == &propose_id {
            ContextMode::Continue
        } else {
            continue;
        };
        *config = ModelStepConfig::decode(config)
            .map_err(|error| PlannerError::InvalidConfiguration(error.to_string()))?
            .with_context_mode(mode)
            .encode()?;
    }
    Ok((expression, extra))
}

/// Builds the `loop` topology's own single-conversation model-step
/// configuration: the same `read_file`/`list_dir`/`search`/`run_python` tools,
/// token budget ([`STATEMENT_EDIT_TOKEN_BUDGET`]) and turn cap
/// ([`STATEMENT_EDIT_MAX_TURNS`]) the generic arm's own `propose-edit` step
/// uses — reusing the constants rather than restating them is what keeps a
/// loop-vs-generic comparison a comparison of composition alone — plus one tool
/// none of the generic arm's steps carry: `edit_file`, so this step's own turns can write the
/// fix straight to the workspace instead of proposing a patch for a
/// separate block to apply. `ModelStepOutputKind::Answer` because there is
/// no patch artifact to extract: the edit already landed, and the free-text
/// answer is only ever read by [`configurable::conclude_unverified`] to
/// confirm the step actually finished rather than errored out mid-decode.
fn loop_model_step_config(
    statement: &str,
    trim: TrimPolicy,
) -> Result<ResolvedConfigurationRef, PlannerError> {
    let mut instruction = format!(
        "Fix this issue by editing the files in the workspace directly:\n{statement}\n\n\
         Use list_dir, search and read_file to find what to change, then edit_file to make \
         the change — it writes immediately, so read the result back if you want to confirm \
         it. run_python lets you check your work before you finish. When you are done, call \
         submit_answer with a short summary of what you changed. Work only with the checkout \
         already in the workspace: do not download, install or fetch another version of the \
         project or its upstream changes. Installing a missing third-party dependency is fine."
    );
    if exp_flag("POLARIS_CODE_EXP_WORKFLOW") {
        instruction.push_str(LOOP_WORKFLOW);
    }
    let mut config = ModelStepConfig::new(
        "loop".to_owned(),
        instruction,
        ToolSubset::Only(vec![
            "read_file".to_owned(),
            "list_dir".to_owned(),
            "search".to_owned(),
            "run_python".to_owned(),
            "edit_file".to_owned(),
        ]),
        TokenBudget(
            exp_number("POLARIS_CODE_EXP_TOKEN_BUDGET").unwrap_or(STATEMENT_EDIT_TOKEN_BUDGET),
        ),
        ModelStepOutputKind::Answer,
    )
    .with_max_turns(
        exp_number("POLARIS_CODE_EXP_MAX_TURNS")
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(STATEMENT_EDIT_MAX_TURNS),
    )
    .with_trim(trim);
    config.autotest = exp_flag("POLARIS_CODE_EXP_AUTOTEST");
    config.review_before_submit = exp_flag("POLARIS_CODE_EXP_REVIEW");
    Ok(config.encode()?)
}

/// Opt-in working method appended to the loop instruction
/// (`POLARIS_CODE_EXP_WORKFLOW=1`).
const LOOP_WORKFLOW: &str = "\n\nWork in this order:\n\
1. Reproduce: write a short script and run it with run_python to show the problem. \
Keep the script; you will run it again.\n\
2. Find the root cause in the project's code, not only the place the symptom shows. \
Look at other callers and similar code paths that need the same fix.\n\
3. Make the smallest general change that fixes the cause, including edge cases the issue \
mentions or implies.\n\
4. Run your reproduction again and confirm the output is now right.\n\
5. Find the project's existing tests for the code you changed and run them from run_python \
(for example with subprocess and the project's own test runner). Fix anything you broke. \
Do not change existing tests to make them pass.\n\
6. Call submit_answer only when the reproduction and those tests pass.";

/// Whether an opt-in experiment switch is set to `1`.
fn exp_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.trim() == "1")
}

/// An opt-in numeric experiment override, if set and valid.
fn exp_number(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .as_deref()
        .and_then(parse_exp_number)
}

/// Parses an experiment override value; anything but a whole number is ignored.
fn parse_exp_number(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

/// Model for the optional cheap, read-only location step.
pub(crate) fn lean_locate_model_id() -> String {
    std::env::var("POLARIS_CODE_LEAN_LOCATE_MODEL")
        .ok()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "anthropic/claude-haiku-4-5-20251001".to_owned())
}

/// Whether the lean arm should prepend its cheap location step. An explicit
/// `0` restores the caller's original plan; unset enables the step.
#[allow(dead_code)] // shared with the lean and cascade arms
pub(crate) fn lean_locate_enabled() -> Result<bool, PlannerError> {
    match std::env::var("POLARIS_CODE_LEAN_LOCATE") {
        Ok(value) if value == "0" => Ok(false),
        Ok(value) if value == "1" => Ok(true),
        Ok(value) => Err(PlannerError::InvalidConfiguration(format!(
            "POLARIS_CODE_LEAN_LOCATE must be 1 or 0, got '{value}'"
        ))),
        Err(std::env::VarError::NotPresent) => Ok(true),
        Err(error) => Err(PlannerError::InvalidConfiguration(error.to_string())),
    }
}

/// Builds a bounded cheap search followed by any model step, passing only the
/// structured findings through a fresh brief handoff. The caller appends its
/// own check and conclusion to the returned sequence. Only lean callers use
/// this helper.
#[allow(dead_code)] // called by the cascade arm
pub(crate) fn locate_then_model_step(
    statement: &str,
    follower_key: CompositionBlockKey,
    follower: &ResolvedConfigurationRef,
    check_named_lines: bool,
    stable_tool_choice: bool,
) -> StatementPlanBody {
    let locate_config = ConfigId::new("cheap-locate")?;
    let follower_config = ConfigId::new("follow-locate")?;
    let mut locate = ModelStepConfig::new(
        "cheap-locate".to_owned(),
        format!("Locate the likely cause of this issue in this checkout: {statement}\nUse read_file, list_dir and search only. Do not edit. Submit a JSON string with keys suspects (array of path:symbol:line-range and reason), reproduction (observed symptom or steps), tests (array of existing test file:line references), open_questions (array). Keep the complete JSON under 3500 characters. State uncertainty plainly."),
        ToolSubset::Only(vec!["read_file".to_owned(), "list_dir".to_owned(), "search".to_owned()]),
        TokenBudget(10_000),
        ModelStepOutputKind::Answer,
    ).with_max_turns(3).with_model(lean_locate_model_id());
    locate.stable_tool_choice = stable_tool_choice;
    let locate = locate.encode()?;
    let mut follower = ModelStepConfig::decode(follower)
        .map_err(|error| PlannerError::InvalidConfiguration(error.to_string()))?;
    follower.context_mode = ContextMode::Brief;
    follower.instruction.push_str(if check_named_lines {
        "\n\nRead the report's named lines and confirm the symptom there before editing."
    } else {
        "\n\nThe preceding search report may be wrong. Check its claims against the checkout before editing."
    });
    let expression = GateExpr::Seq(vec![
        step_expr("cheap-locate", fine::locate_key(), locate_config.clone())?,
        step_expr("follow-locate", follower_key, follower_config.clone())?,
    ]);
    Ok((
        expression,
        vec![
            (locate_config, locate),
            (follower_config, follower.encode()?),
        ],
    ))
}

/// The loop arm's plan preceded by the optional cheap location report.
#[allow(dead_code)] // called by the lean arm
pub(crate) fn locate_then_loop_statement_only_plan_body(
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
) -> StatementPlanBody {
    // This helper is called only by the lean arm. An explicit 0 restores
    // the loop arm's original body and digest; unset defaults to the locate step.
    if !lean_locate_enabled()? {
        return loop_statement_only_plan_body(facts, attempt);
    }
    let (expression, configs) = locate_then_model_step(
        &facts.statement,
        configurable::model_direct_edit_key(),
        &loop_model_step_config(&facts.statement, TrimPolicy::None)?,
        false,
        false,
    )?;
    let GateExpr::Seq(mut steps) = expression else {
        unreachable!("locate_then_model_step returns a sequence")
    };
    steps.push(step_expr(
        "conclude",
        configurable::conclude_unverified_key(),
        no_config_id()?,
    )?);
    Ok((GateExpr::Seq(steps), configs))
}

/// The `loop` topology's statement-only plan body: one model
/// conversation — same model, tools, turn cap and token budget as the generic
/// arm's own `propose-edit` step (see [`loop_model_step_config`]) — that edits the
/// workspace directly, plus the one step every gate plan needs to close the
/// coding contract
/// ([`configurable::ConcludeUnverifiedBlock`], which makes no verification
/// claim of its own). No check gate, no judge, no repair, no review, no
/// domain notes: the only difference from the generic arm's own plan is what is
/// composed around the model conversation, which is the contrast this arm
/// exists to isolate. `attempt` is unused: unlike the repair shapes above,
/// this plan never varies with the revision loop — there is nothing in it a
/// materializer refusal could ask for a shallower version of.
///
/// `crate::cosmos::domains::plan_for_selection` calls this for
/// [`crate::cosmos::domains::Arm::Loop`], mirroring how every other
/// non-generic arm substitutes only the statement-only branch and leaves
/// the read-only-question and stated-edit shapes to [`scripted_plan_generic`]'s
/// own, unchanged handling.
pub(crate) fn loop_statement_only_plan_body(
    facts: &PlannerFacts,
    _attempt: RevisionAttempt,
) -> StatementPlanBody {
    let model_config = ConfigId::new("loop")?;
    let conclude_config = no_config_id()?;
    let extra = vec![(
        model_config.clone(),
        loop_model_step_config(&facts.statement, TrimPolicy::None)?,
    )];
    let expression = GateExpr::Seq(vec![
        step_expr("loop", configurable::model_direct_edit_key(), model_config)?,
        step_expr(
            "conclude",
            configurable::conclude_unverified_key(),
            conclude_config,
        )?,
    ]);
    Ok((expression, extra))
}

/// Loop's opt-in TWM variant. The ordinary plan is returned verbatim when off.
pub(crate) fn loop_plan_with_twm(
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
    options: Option<twm::TwmOptions>,
) -> StatementPlanBody {
    let (expression, mut configs) = loop_statement_only_plan_body(facts, attempt)?;
    if let Some(options) = options {
        let mut config = ModelStepConfig::decode(&configs[0].1)
            .map_err(|error| PlannerError::InvalidConfiguration(error.to_string()))?;
        config.twm = Some(options);
        configs[0].1 = config.encode()?;
    }
    Ok((expression, configs))
}

/// The statement-only edit shape [`scripted_plan_generic`] falls through to
/// for a task with `is_question` and `is_survey` both false and no stated
/// edits: builds the plan's own `(expression, extra configurations)` pair,
/// keyed by `facts` and `attempt` alone.
pub(crate) type StatementPlanBody =
    Result<(GateExpr, Vec<(ConfigId, ResolvedConfigurationRef)>), PlannerError>;

/// Builds a gate plan deterministically from `facts` alone, except for the
/// statement-only edit branch, which `statement_only` supplies: the same
/// facts always resolve to the same shape (modulo `attempt`'s revision-loop
/// degradation, see [`test_gated_expression`]/[`model_test_gated_expression`]).
/// Besides `is_question` and `test_runner_available`, `has_stated_edits`
/// distinguishes a statement-only edit — nothing for [`direct_edit_expression`]
/// or [`test_gated_expression`] to apply — from a task that named its own
/// edit, and a stated edit's own `scope_size` further distinguishes a
/// single-target edit from a multi-target sweep: two edits that differ only
/// in how many sites they name, or in whether any site was named at all, are
/// not the same plan.
///
/// With [`statement_only_plan_body`] as `statement_only` (what
/// [`scripted_plan`] passes), a statement-only edit with no stated test
/// command but `shell_available` gets [`self_check_gated_expression`];
/// failing that, one with `reproduce_runner_available` gets
/// [`model_reproduce_gated_expression`] instead of the bare
/// [`model_proposed_edit_expression`] — see that body's own doc, and
/// `docs/gate-planning.md`, for the precedence.
///
/// [`scripted_plan`] is this function with `statement_only` fixed to
/// [`statement_only_plan_body`] with empty notes; `crate::cosmos::domains`
/// calls this directly with its own domain variant's plan body for every
/// other arm, so only the statement-only branch itself ever varies by arm.
pub(crate) fn scripted_plan_generic(
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
    catalogue: &CompositionCatalogueSnapshot,
    target: &TopologyContractKey,
    statement_only: impl FnOnce(&PlannerFacts, RevisionAttempt) -> StatementPlanBody,
) -> Result<GatePlan, PlannerError> {
    let config = no_config_id()?;
    let mut configurations = BTreeMap::new();
    configurations.insert(config.clone(), no_config_ref()?);
    let expression = if facts.is_question {
        question_expression(&config)?
    } else if !facts.has_stated_edits && !facts.is_survey {
        // A statement-only edit task: `is_question` is false (this is an
        // edit, not a question) and `is_survey` is false (no declared scope
        // and marker to scan), so what is left is the case neither of the
        // shapes below the next branch was built for — a unit of work that
        // named no edit for `apply-direct` to apply at all.
        let (expression, extra) = statement_only(facts, attempt)?;
        for (id, resolved) in extra {
            configurations.insert(id, resolved);
        }
        expression
    } else if facts.test_runner_available {
        let model_config = ConfigId::new("propose-fix")?;
        configurations.insert(model_config.clone(), scripted_model_step_config()?);
        test_gated_expression(&config, &model_config, attempt)?
    } else if facts.scope_size > 1 {
        worklist_sweep_expression(&config)?
    } else {
        direct_edit_expression(&config)?
    };
    Ok(GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        target.clone(),
        expression,
        Vec::new(),
        configurations,
    )?)
}

/// The generic scripted plan: [`scripted_plan_generic`] with
/// [`statement_only_plan_body`] and
/// empty notes. `crate::cosmos::domains::plan_for_selection` calls this
/// directly, and only this, for [`crate::cosmos::domains::Arm::Generic`] —
/// which is what keeps the generic arm's plan unchanged regardless of the
/// selected domain.
pub fn scripted_plan(
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
    catalogue: &CompositionCatalogueSnapshot,
    target: &TopologyContractKey,
) -> Result<GatePlan, PlannerError> {
    scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
        statement_only_plan_body(f, a, "")
    })
}

// ---------------------------------------------------------------------------
// Model planner: a bounded JSON grammar over the same catalogue, never an
// open-ended graph-generation interface.
// ---------------------------------------------------------------------------

/// Strips the `"code."` registration-namespace prefix any of this
/// embodiment's catalogue identities carries, for the short names a model
/// answer, a prompt, or a rendered plan shape names a catalogue key by
/// (`"apply-direct"`, not `"code.apply-direct"`).
fn short_id(id: &str) -> &str {
    id.strip_prefix("code.").unwrap_or(id)
}

/// Strips a block key's namespace prefix (see [`short_id`]).
fn short_block_name(key: &CompositionBlockKey) -> &str {
    short_id(key.id().as_str())
}

/// Strips a stable port key's namespace prefix (see [`short_id`]), for the
/// port names sent in a block's prompt description (`"applied-edit"`, not
/// `"code.applied-edit"`).
fn short_port_name(port: &StablePortKey) -> &str {
    short_id(port.id().as_str())
}

/// Renders a loop/repeat gate's termination control the same way
/// [`render_gate_expression_shape`] renders everything else: a short,
/// catalogue-derived name, never a plan-local label.
fn render_loop_control(control: &LoopControl) -> String {
    match control {
        LoopControl::Until(predicate) => format!("until={}", short_id(predicate.id().as_str())),
        LoopControl::Count(count) => format!("count={count}"),
    }
}

/// Renders one gate-expression tree into the readable shape label carried in
/// reports and plan-history evidence: every gate's own name, nested exactly
/// as the plan nests it, down to the catalogue block/predicate/discriminator
/// names at its leaves — e.g.
/// `Seq(model-propose, apply-patch-intent, Loop(until=tests-passed, run-project-tests-captured))`.
///
/// This is a projection of the plan actually admitted, not a re-derivation:
/// two plans that use the same blocks in a different tree shape render
/// differently, which a flat, unordered block list cannot show. The plan's
/// own [`polaris_cosmos::contracts::GateExpressionShapeKey`] digest — read
/// separately, off [`polaris_cosmos::contracts::GatePlanRecord::shape`] —
/// remains the canonical grouping identity this label only makes legible for
/// a reader, never replaces: two renderings that happen to collide in text
/// are not thereby the same plan, and nothing here is compared for equality
/// anywhere the digest already is.
pub fn render_gate_expression_shape(expr: &GateExpr) -> String {
    match expr {
        GateExpr::Step(step) => short_block_name(step.block()).to_owned(),
        GateExpr::Seq(children) => {
            let rendered = children
                .iter()
                .map(render_gate_expression_shape)
                .collect::<Vec<_>>()
                .join(", ");
            format!("Seq({rendered})")
        }
        GateExpr::Choose {
            predicate,
            when_true,
            when_false,
        } => format!(
            "Choose({}, {}, {})",
            short_id(predicate.id().as_str()),
            render_gate_expression_shape(when_true),
            render_gate_expression_shape(when_false),
        ),
        GateExpr::Switch {
            discriminator,
            cases,
            default,
        } => {
            let mut parts = vec![short_id(discriminator.id().as_str()).to_owned()];
            for case in cases {
                parts.push(format!(
                    "{}: {}",
                    case.key().as_str(),
                    render_gate_expression_shape(case.body())
                ));
            }
            if let Some(default) = default {
                parts.push(format!(
                    "default: {}",
                    render_gate_expression_shape(default)
                ));
            }
            format!("Switch({})", parts.join(", "))
        }
        GateExpr::Fork { arms, reducer } => {
            let rendered_arms = arms
                .iter()
                .map(render_gate_expression_shape)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Fork([{rendered_arms}], reduce={})",
                short_block_name(reducer.block())
            )
        }
        GateExpr::Loop { body, control, .. } => format!(
            "Loop({}, {})",
            render_loop_control(control),
            render_gate_expression_shape(body)
        ),
        GateExpr::Repeat { body, control, .. } => format!(
            "Repeat({}, {})",
            render_loop_control(control),
            render_gate_expression_shape(body)
        ),
        GateExpr::Guard { step, on_error, .. } => format!(
            "Guard({}, on_error={})",
            short_block_name(step.block()),
            short_id(on_error.id().as_str())
        ),
        GateExpr::Seal { body, .. } => format!("Seal({})", render_gate_expression_shape(body)),
        GateExpr::Slot { candidates, .. } => {
            let rendered = candidates
                .iter()
                .map(render_gate_expression_shape)
                .collect::<Vec<_>>()
                .join(", ");
            format!("Slot({rendered})")
        }
    }
}

/// Renders one block's catalogue [`BlockDescription`] into the line naming it
/// in the model planner's prompt: its summary, what it reads and produces,
/// its declared effect, one example configured use, and the stable fact keys
/// (or "requires: none") a host must already satisfy for it to be worth
/// naming — the full picture [`super::block_cards`] carries for it, not a
/// second, hand-written line that could drift out of sync with what the
/// catalogue actually says.
fn describe_for_prompt(
    description: &BlockDescription,
    effect: Effect,
    calls_model: bool,
) -> String {
    let effect_text = if calls_model {
        "calls a model"
    } else {
        match effect {
            Effect::ReadOnly => "read-only",
            Effect::Mutating { repeat_safe: true } => "changes files, repeat-safe",
            Effect::Mutating { repeat_safe: false } => "changes files",
        }
    };
    let output = short_port_name(description.output());
    let mut text = format!(
        "{} [{effect_text}, produces {output}]",
        description.summary()
    );
    if !description.inputs().is_empty() {
        let inputs = description
            .inputs()
            .iter()
            .map(short_port_name)
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(" (reads {inputs})"));
    }
    text.push_str(&format!(" — e.g. {}", description.example()));
    if description.required_facts().is_empty() {
        text.push_str(" (requires: none)");
    } else {
        let facts = description
            .required_facts()
            .iter()
            .map(FactKey::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(" (requires: {facts})"));
    }
    text
}

/// Prompt text for one of the selected domain's own expert blocks.
///
/// Every other block [`nameable_gate_blocks_for_selection`] offers is
/// described by its own catalogue [`BlockDescription`] (see
/// [`describe_for_prompt`]); the domain expert blocks `crate::cosmos::domains`
/// registers carry none, so the catalogue's fact-gated listing
/// ([`block_cards::admissible_gate_blocks`]) never returns them. They are
/// only ever offered under [`super::domains::Arm::Composed`], for the one
/// selected domain, and this is the one line describing each there.
fn expert_block_meaning(name: &str) -> &'static str {
    match name {
        "django.verify-regression" => {
            "runs changed Django tests and the proposed check after applying a patch"
        }
        "django.verify-regression-repair" => {
            "verifies a Django repair, allowing a checked zero-edit repair"
        }
        "sympy.run-scoped-verification" => {
            "runs scoped SymPy tests, doctests, and the proposed check"
        }
        "sphinx.find-mirrored-tests" => {
            "finds Sphinx tests that mirror the proposed patch's modules"
        }
        "sphinx.run-check-and-mirrored-tests" => {
            "runs the proposed check and located Sphinx tests after applying a patch"
        }
        _ => "an expert verification block for the selected domain",
    }
}

/// One block the model planner may name: its short name, its real catalogue
/// key, and the prompt text describing it (see [`describe_for_prompt`]).
struct NameableBlock {
    name: String,
    key: CompositionBlockKey,
    meaning: String,
}

/// [`nameable_gate_blocks_for_selection`] under the default (generic)
/// domain selection: what [`model_plan`] itself offers.
#[cfg(test)]
fn nameable_gate_blocks(
    catalogue: &CompositionCatalogueSnapshot,
    facts: &PlannerFacts,
) -> Vec<NameableBlock> {
    nameable_gate_blocks_for_selection(catalogue, facts, super::domains::DomainSelection::default())
}

/// Every block the model planner's bounded JSON grammar may name for one
/// task, derived from the sealed catalogue's own [`BlockDescription`]s
/// ([`block_cards::admissible_gate_blocks`]) rather than a hand-maintained
/// subset or a hand-maintained hidden-block list: a block whose required
/// facts `facts` does not meet (a statement-only task's `apply-direct`, a
/// host with no test runner's `run-project-tests`, and so on) is hidden here,
/// and [`block_by_name_for_selection`] cannot resolve it either — a model
/// naming it anyway gets the ordinary "unknown block" refusal and must
/// revise, the same as naming any other block outside the grammar.
///
/// A block registered under its own strict configuration schema (a marker
/// scan's `marker`/`max_sites`/scope, a survey's path list, a new file's
/// path/content, a classifier's dispositions) is offered too: a step naming
/// it configures it as `{"registered": value}` ([`ModelGateConfigAnswer::Registered`]),
/// carried under the block's own registered schema key and validated by the
/// block's own factory during Cosmos materialization.
///
/// Two further filters are independent of `facts`:
///
/// - `code.model-direct-edit` and `code.conclude-unverified` are never
///   offered. `model-direct-edit` shares `model_propose`'s configuration
///   schema (a plan names the same `role`/`instruction`/`tools`/`token_budget`
///   shape for both), but it is registered `Effect::Mutating` because
///   naming `edit_file` in its tool subset really does let its own turns
///   write to the tree — unlike every other block this grammar offers,
///   which are read-only or apply an already-separately-judged artifact.
///   Excluded here so the free-form model planner can never acquire that
///   capability through the same JSON grammar an ordinary read-only propose
///   step accepts; only the `loop` topology's
///   hand-written plan (`loop_statement_only_plan_body`) ever names it, and
///   `conclude-unverified` is that plan's own closing step.
/// - Domain expert blocks (`crate::cosmos::domains`) are offered only under
///   [`super::domains::Arm::Composed`], and then only the selected domain's
///   own ([`super::domains::expert_keys`]) — a generic runtime plan carries
///   no domain knowledge. They register no catalogue description, so they
///   are appended after the fact-gated listing and described by
///   [`expert_block_meaning`] instead.
fn nameable_gate_blocks_for_selection(
    catalogue: &CompositionCatalogueSnapshot,
    facts: &PlannerFacts,
    selection: super::domains::DomainSelection,
) -> Vec<NameableBlock> {
    let domain_keys = super::domains::keys();
    let selected_expert_keys = if selection.arm == super::domains::Arm::Composed {
        super::domains::expert_keys(selection.domain)
    } else {
        Vec::new()
    };
    let offered = |key: &CompositionBlockKey| {
        *key != configurable::model_direct_edit_key()
            && *key != configurable::conclude_unverified_key()
            && (!domain_keys.contains(key) || selected_expert_keys.contains(key))
    };
    let model_propose = configurable::model_propose_schema();
    let mut blocks: Vec<NameableBlock> = block_cards::admissible_gate_blocks(catalogue, facts)
        .into_iter()
        .filter(|(key, _)| offered(key))
        .filter_map(|(key, description)| {
            let (_, schema, effect) = catalogue.block(key)?;
            let calls_model = *schema == model_propose;
            Some(NameableBlock {
                name: short_block_name(key).to_owned(),
                key: key.clone(),
                meaning: describe_for_prompt(description, effect, calls_model),
            })
        })
        .collect();
    for key in &selected_expert_keys {
        // A described expert block already went through the fact-gated
        // listing above, admitted or not; only an undescribed one is added
        // here.
        if catalogue.block(key).is_none()
            || catalogue.block_description(key).is_some()
            || blocks.iter().any(|block| block.key == *key)
        {
            continue;
        }
        let name = short_block_name(key).to_owned();
        let meaning = expert_block_meaning(&name).to_owned();
        blocks.push(NameableBlock {
            name,
            key: key.clone(),
            meaning,
        });
    }
    blocks
}

fn block_by_name_for_selection(
    catalogue: &CompositionCatalogueSnapshot,
    facts: &PlannerFacts,
    name: &str,
    selection: super::domains::DomainSelection,
) -> Option<CompositionBlockKey> {
    nameable_gate_blocks_for_selection(catalogue, facts, selection)
        .into_iter()
        .find(|block| block.name == name)
        .map(|block| block.key)
}

/// The blocks that only judge or apply edits the task's own caller stated,
/// rather than anything a model step proposed.
fn stated_edit_only_blocks() -> [CompositionBlockKey; 4] {
    [
        existing::apply_direct_key(),
        existing::judge_stated_edits_key(),
        existing::predict_edit_loop_key(),
        existing::conclude_prediction_key(),
    ]
}

/// Whether a block only judges or applies edits the task's own caller
/// stated, rather than anything a model step proposed.
///
/// A statement-only task states none, so a model-chosen plan naming one of
/// these would pass without writing anything. [`direct_edit_expression`]
/// and [`model_proposed_edit_expression`]'s own doc comments record the same
/// fact for the *scripted* planner's fixed shapes. For the free-form plan a
/// model chooses, the catalogue's own required facts already hide these
/// blocks from a task that stated no edits
/// ([`nameable_gate_blocks_for_selection`]); [`build_step_answer`] still
/// checks this after resolving a name, and names the reason when a hidden
/// one is named anyway (see [`unknown_block_detail`]).
fn requires_stated_edits(block: &CompositionBlockKey) -> bool {
    stated_edit_only_blocks().contains(block)
}

/// Why `name` did not resolve to an offered block: always an "unknown
/// block" refusal, plus — when it names a stated-edit-only block for a task
/// that stated none — the reason it is hidden and the shape to use instead,
/// so a revision prompt carries an actionable hint rather than only the
/// refusal.
fn unknown_block_detail(facts: &PlannerFacts, name: &str) -> String {
    let stated_edit_only = stated_edit_only_blocks()
        .iter()
        .any(|key| short_block_name(key) == name);
    if stated_edit_only && !facts.has_stated_edits {
        format!(
            "unknown block '{name}': {}",
            stated_edits_refusal_detail(name)
        )
    } else {
        format!("unknown block '{name}'")
    }
}

fn stated_edits_refusal_detail(name: &str) -> String {
    format!(
        "'{name}' only judges or applies edits the task itself stated, and this task \
         stated none; choose model-propose + apply-patch-intent + judge-patch-intent \
         instead"
    )
}

/// One predicate a model-submitted `Choose`/`Loop`/`Repeat` gate may name: its
/// short name, its real catalogue key, and the one-line meaning sent in the
/// prompt.
struct NameablePredicate {
    name: &'static str,
    key: PredicateKey,
    meaning: &'static str,
}

/// Every predicate the model planner's bounded grammar may name.
///
/// Hand-maintained, unlike [`nameable_gate_blocks_for_selection`]: [`predicates`] registers
/// no catalogue prose of its own, only the predicates themselves.
fn nameable_predicates() -> Vec<NameablePredicate> {
    vec![
        NameablePredicate {
            name: "tests-passed",
            key: predicates::tests_passed_key(),
            meaning: "the most recently produced edit-site assessment judged Pass",
        },
        NameablePredicate {
            name: "review-approved",
            key: predicates::review_approved_key(),
            meaning: "the most recently produced model review artifact approved what it reviewed",
        },
        NameablePredicate {
            name: "budget-left",
            key: predicates::budget_left_key(),
            meaning: "a worklist sweep still has iteration budget left",
        },
        NameablePredicate {
            name: "has-check",
            key: predicates::has_check_key(),
            meaning: "the most recently produced model proposal included a check script",
        },
        NameablePredicate {
            name: "check-needs-repair",
            key: predicates::check_needs_repair_key(),
            meaning: "the most recently run check script failed",
        },
    ]
}

fn predicate_by_name(name: &str) -> Option<PredicateKey> {
    nameable_predicates()
        .into_iter()
        .find(|predicate| predicate.name == name)
        .map(|predicate| predicate.key)
}

/// One discriminator a model-submitted `Switch` gate may name: its short
/// name, its real catalogue key, its one-line meaning, and the closed set of
/// case labels it may route on.
struct NameableDiscriminator {
    name: &'static str,
    key: DiscriminatorKey,
    meaning: &'static str,
    cases: BTreeSet<CaseKey>,
}

/// Every discriminator the model planner's bounded grammar may name.
///
/// Empty today: [`predicates`] registers two, and neither is safely usable
/// through a bare submitted gate plan against the coding target contract,
/// which declares no `requires`/`requires_outputs` at all (only
/// `produces::<AssessmentConclusion>()`, see
/// [`super::topology::contract`]) — so a graph whose derived signature ends
/// up with *anything* in `requires_outputs` is refused with
/// `plan.target-contract-mismatch`, regardless of the type.
/// `task-kind`'s own discriminator reads
/// [`crate::cosmos::task::CodingTask`] directly, which no coding block ever
/// produces as an ordinary graph value (it is only ever ambient `Res`), so
/// naming it always lands in exactly that free-read state — confirmed by
/// this module's own (materialize-refused) test. `task-disposition` routes on
/// a real graph-produced value ([`clarification::classify_request_key`]'s own
/// `Out<TaskDisposition>`, proven by
/// `tests/gate_block_catalogue.rs`'s `clarification_disposition_switch_materialises_and_answers`),
/// and although that block's own configuration schema
/// (`allowed_dispositions`) is now decodable through the
/// `{"registered": value}` config form ([`ModelGateConfigAnswer::Registered`]),
/// a switch on it is offered only by the flat grammar's optional
/// `expression` tree ([`model_grammar`], which names its own discriminator
/// set), not by this `plan` tree. The first gap is closable by a coding
/// block that actually produces a task-derived value for a plan to switch
/// on, and naming `task-disposition` here is a separate decision — but
/// neither is to be opened silently here: the `switch` node shape itself
/// still parses and resolves against whatever this returns, so a future
/// addition here needs no further grammar change.
fn nameable_discriminators() -> Vec<NameableDiscriminator> {
    Vec::new()
}

fn discriminator_by_name(name: &str) -> Option<(DiscriminatorKey, BTreeSet<CaseKey>)> {
    nameable_discriminators()
        .into_iter()
        .find(|discriminator| discriminator.name == name)
        .map(|discriminator| (discriminator.key, discriminator.cases))
}

/// The model-answer subset accepted by the gate planner. An unknown field
/// at any level — the answer itself, a step, or a step's config — is a
/// refused answer, not a silently ignored extra.
///
/// Exactly one of `steps` (the original flat grammar: an implicit `Seq` of
/// leaf steps, or the order an optional `expression` tree over them gives)
/// or `plan` (a full [`ModelGateExprAnswer`] tree) must be present, so an
/// old-style answer from a model still following the flat grammar keeps
/// working unchanged. `wires` may accompany either.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelGatePlanAnswer {
    #[serde(default)]
    steps: Option<Vec<ModelGateStepAnswer>>,
    #[serde(default)]
    plan: Option<ModelGateExprAnswer>,
    /// Optional tree over the flat `steps`, naming each declared instance
    /// exactly once (see [`model_grammar::Expression`]). Accompanies `steps`
    /// only: a `plan` tree already carries its own shape.
    #[serde(default)]
    expression: Option<model_grammar::Expression>,
    /// Optional typed wires from a producer instance to a reader instance,
    /// under either shape (see [`model_grammar::Wire`]).
    #[serde(default)]
    wires: Vec<model_grammar::Wire>,
    grounds: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelGateStepAnswer {
    instance: String,
    block: String,
    #[serde(default)]
    config: Option<ModelGateConfigAnswer>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
enum ModelGateConfigAnswer {
    Registered(serde_json::Value),
    ModelPropose {
        role: String,
        instruction: String,
        #[serde(default)]
        tools: Vec<String>,
        /// Refused by [`build_step_answer`] itself when outside
        /// `1..=`[`MAX_MODEL_STEP_TOKEN_BUDGET`](configurable::MAX_MODEL_STEP_TOKEN_BUDGET),
        /// and again, redundantly, by [`ModelStepConfig::decode`] once this
        /// step is later built — unlike `max_turns` below, refused here first
        /// because this value becomes a real request's `max_output_tokens`.
        token_budget: u32,
        /// Which artifact this instance commits to producing: `"plan"`,
        /// `"findings"`, `"patch-intent"`, `"review"`, or `"answer"`, so a
        /// downstream block that needs a patch (`apply-patch-intent`) only
        /// gets one when the plan actually asked for one. Omitted means
        /// `"patch-intent"`, the one kind the flat grammar always produced.
        #[serde(default)]
        output: Option<String>,
        /// Round-trip cap for this one bounded turn; omitted gives the
        /// scripted statement-edit step's own cap
        /// ([`STATEMENT_EDIT_MAX_TURNS`]). Out-of-range values are refused
        /// by [`ModelStepConfig::decode`] when this step is later built, with
        /// that decoder's own precise bound in the message — not
        /// re-validated here, so the two bounds cannot drift apart.
        #[serde(default)]
        max_turns: Option<u16>,
        /// Whether this instance reads a preceding captured test failure.
        /// Meaningful only wired after a `run-project-tests-captured` step in
        /// the same plan; a plan setting it without one is refused by native
        /// graph validation (nothing produces the input it would read),
        /// exactly like any other missing producer.
        #[serde(default)]
        sees_prior_failure: bool,
        /// How this step's conversation starts: `"fresh"` (the default),
        /// `"brief"`, or `"continue"` (see [`ContextMode`]).
        #[serde(default)]
        context_mode: Option<String>,
    },
}

/// Maps a model-propose config answer's `output` tag to the typed kind it
/// selects, independent of [`ModelStepOutputKind`]'s own (module-private)
/// parser: the tags are the same stable strings
/// [`ModelStepConfig::encode`]/`decode` already round-trip.
fn model_step_output_kind(tag: &str) -> Option<ModelStepOutputKind> {
    match tag {
        "plan" => Some(ModelStepOutputKind::Plan),
        "findings" => Some(ModelStepOutputKind::Findings),
        "patch-intent" => Some(ModelStepOutputKind::PatchIntent),
        "review" => Some(ModelStepOutputKind::Review),
        "answer" => Some(ModelStepOutputKind::Answer),
        _ => None,
    }
}

/// One case arm of a model-submitted `Switch` gate: a discriminator case
/// label and the sub-expression it selects.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSwitchCaseAnswer {
    case: String,
    body: ModelGateExprAnswer,
}

/// One node of a model-submitted gate-expression tree, tagged by `kind`.
///
/// Covers every [`GateExpr`] shape the coding gate-block catalogue can
/// actually back: `Seq`, `Choose`, `Switch`, `Fork`, `Loop`, and `Repeat`.
/// `Guard`, `Seal`, and `Slot` are deliberately absent — the catalogue
/// registers no handler, timeout-policy, context-policy, or selector key at
/// all (only a block factory, a predicate/discriminator set, and one loop
/// retention policy), so nothing could ever admit one; a model naming
/// `"guard"`, `"seal"`, or `"slot"` gets the same unknown-shape refusal as any
/// other malformed answer, through the same revision loop, rather than a
/// grammar that only ever refuses at materialization time.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum ModelGateExprAnswer {
    /// Runs one registered block. Carries the same shape a legacy `steps`
    /// entry does, so a leaf can be written identically in either grammar.
    Step(ModelGateStepAnswer),
    /// Runs an ordered list of nodes in sequence.
    Seq {
        /// Child nodes, in run order. Must not be empty.
        steps: Vec<ModelGateExprAnswer>,
    },
    /// Runs `when_true` if `predicate` holds on the most recently produced
    /// value it reads, `when_false` otherwise.
    Choose {
        predicate: String,
        when_true: Box<ModelGateExprAnswer>,
        when_false: Box<ModelGateExprAnswer>,
    },
    /// Runs the case body matching `discriminator`'s output, or `default` if
    /// no case matches (and `default` is given).
    Switch {
        discriminator: String,
        cases: Vec<ModelSwitchCaseAnswer>,
        #[serde(default)]
        default: Option<Box<ModelGateExprAnswer>>,
    },
    /// Runs every arm concurrently, then reduces their outputs through one
    /// registered block.
    Fork {
        /// Concurrent arms. Must not be empty.
        arms: Vec<ModelGateExprAnswer>,
        reducer: ModelGateStepAnswer,
    },
    /// Repeats `body` while `until` holds (checked before every iteration,
    /// including the first, so it may run zero times), or exactly `count`
    /// times. Every loop this grammar builds keeps only its most recent
    /// completed iteration — the coding catalogue registers no other
    /// retention policy to choose instead.
    Loop {
        body: Box<ModelGateExprAnswer>,
        #[serde(default)]
        until: Option<String>,
        #[serde(default)]
        count: Option<u16>,
    },
    /// Like `loop`, but `body` always runs once before the same repeat check,
    /// so its guaranteed production is `body`'s own rather than always empty.
    Repeat {
        body: Box<ModelGateExprAnswer>,
        #[serde(default)]
        until: Option<String>,
        #[serde(default)]
        count: Option<u16>,
    },
}

/// Defensive cap on [`build_expr_answer`]'s own recursion, independent of any
/// host [`CompositionLimits::max_depth`]: a pathological answer must fail
/// cleanly here rather than risk overflowing the stack before the host's own,
/// more precise, path-scoped depth check ever runs.
const MAX_MODEL_PLAN_TREE_DEPTH: u32 = 48;

/// Resolves one leaf step's block and configuration, shared by the flat
/// `steps` grammar and every `Step`/`Fork`-reducer leaf in the `plan` tree
/// grammar.
///
/// A `model-propose` configuration is accepted by `model-propose` itself and
/// by the two conversation-preserving propose blocks
/// (`model-propose-conversation`, `resume-model-propose`). It never grants
/// `edit_file` — every block this grammar offers under that configuration is
/// read-only — and its instruction always gains [`WORKSPACE_ONLY_RULE`], the
/// same rule every scripted edit step carries. The planner chooses the
/// validated `token_budget`, the `output` kind (default `patch-intent`),
/// `max_turns` (default [`STATEMENT_EDIT_MAX_TURNS`], the scripted
/// statement-edit step's own cap), `sees_prior_failure`, and `context_mode`
/// per instance.
#[expect(
    clippy::too_many_arguments,
    reason = "one leaf builder threading the plan-wide config table, instance set, and config counter is clearer than a struct that exists only to satisfy this lint"
)]
fn build_step_answer(
    step: &ModelGateStepAnswer,
    facts: &PlannerFacts,
    catalogue: &CompositionCatalogueSnapshot,
    selection: super::domains::DomainSelection,
    configurations: &mut BTreeMap<ConfigId, ResolvedConfigurationRef>,
    seen_instances: &mut BTreeSet<String>,
    next_config: &mut usize,
    invalid: &impl Fn(String) -> PlannerError,
) -> Result<StepPlan, PlannerError> {
    let block = block_by_name_for_selection(catalogue, facts, &step.block, selection)
        .ok_or_else(|| invalid(unknown_block_detail(facts, &step.block)))?;
    if !facts.has_stated_edits && requires_stated_edits(&block) {
        return Err(invalid(stated_edits_refusal_detail(&step.block)));
    }
    if !seen_instances.insert(step.instance.clone()) {
        return Err(invalid(format!(
            "duplicate instance label '{}'",
            step.instance
        )));
    }
    let instance =
        BlockInstanceId::new(step.instance.clone()).map_err(|error| invalid(error.to_string()))?;
    let index = *next_config;
    *next_config += 1;
    let config_id =
        ConfigId::new(format!("model-step-{index}")).map_err(|error| invalid(error.to_string()))?;
    let config_ref = match &step.config {
        None => no_config_ref().map_err(|error| invalid(error.to_string()))?,
        Some(ModelGateConfigAnswer::Registered(value)) => {
            let (_, schema, _) = catalogue
                .block(&block)
                .expect("nameable block is registered");
            if *schema == no_config_schema() || *schema == configurable::model_propose_schema() {
                return Err(invalid(format!(
                    "'{}' requires null or model-propose config",
                    step.block
                )));
            }
            // This older factory ignores surplus keys; reject them at the
            // model boundary, like the other factories already do.
            if block == configurable::create_file_key()
                && !value.as_object().is_some_and(|map| {
                    map.len() == 2 && map.contains_key("path") && map.contains_key("content")
                })
            {
                return Err(invalid(
                    "create-file config requires exactly path and content".to_owned(),
                ));
            }
            ResolvedConfigurationRef::inline(
                schema.clone(),
                model_grammar::configuration(value).map_err(invalid)?,
            )
            .map_err(|error| invalid(error.to_string()))?
        }
        Some(ModelGateConfigAnswer::ModelPropose {
            role,
            instruction,
            tools,
            token_budget,
            output,
            max_turns,
            sees_prior_failure,
            context_mode,
        }) => {
            if block != configurable::model_propose_key()
                && block != repair::model_propose_conversation_key()
                && block != repair::resume_model_propose_key()
            {
                return Err(invalid(format!(
                    "'{}' does not take a model-propose configuration",
                    step.block
                )));
            }
            if tools.iter().any(|tool| tool == "edit_file") {
                return Err(invalid(
                    "model-propose is read-only and cannot offer edit_file".to_owned(),
                ));
            }
            // Unlike `max_turns` (refused only once this step is later built,
            // see that field's own doc above), an out-of-range `token_budget`
            // is refused here too, before it is even encoded: this value
            // becomes `LlmRequest.max_output_tokens` verbatim, so an
            // unbounded one is a provider bill, not just a wasted turn. Reuses
            // `configurable`'s own ceiling rather than a second number that
            // could drift from the one `ModelStepConfig::decode` enforces.
            if *token_budget == 0 || *token_budget > configurable::MAX_MODEL_STEP_TOKEN_BUDGET {
                return Err(invalid(format!(
                    "model-propose token_budget must be in 1..={}, got {token_budget}",
                    configurable::MAX_MODEL_STEP_TOKEN_BUDGET
                )));
            }
            let output_kind = match output {
                Some(output) => model_step_output_kind(output).ok_or_else(|| {
                    invalid(format!("unknown model-propose output kind '{output}'"))
                })?,
                None => ModelStepOutputKind::PatchIntent,
            };
            let mode = match context_mode.as_deref().unwrap_or("fresh") {
                "fresh" => ContextMode::Fresh,
                "brief" => ContextMode::Brief,
                "continue" => ContextMode::Continue,
                other => return Err(invalid(format!("unknown context mode '{other}'"))),
            };
            // The planner names the step and writes its instruction; the
            // workspace rule every scripted edit step carries is appended so
            // a runtime plan cannot talk a step into fetching another
            // version of the project.
            let mut config = ModelStepConfig::new(
                role.clone(),
                format!("{instruction}\n\n{WORKSPACE_ONLY_RULE}"),
                ToolSubset::Only(tools.clone()),
                TokenBudget(*token_budget),
                output_kind,
            )
            .with_max_turns(max_turns.unwrap_or(STATEMENT_EDIT_MAX_TURNS))
            .with_context_mode(mode);
            if *sees_prior_failure {
                config = config.with_prior_failure_visible();
            }
            config
                .encode()
                .map_err(|error| invalid(error.to_string()))?
        }
    };
    configurations.insert(config_id.clone(), config_ref);
    Ok(StepPlan::new(instance, block, config_id))
}

/// Resolves a loop/repeat node's termination control: exactly one of `until`
/// (a named predicate) or `count` (a fixed iteration count) must be given.
fn loop_control_from_answer(
    until: &Option<String>,
    count: &Option<u16>,
    invalid: &impl Fn(String) -> PlannerError,
) -> Result<LoopControl, PlannerError> {
    match (until, count) {
        (Some(_), Some(_)) => Err(invalid(
            "a loop/repeat node names both 'until' and 'count'; name exactly one".to_owned(),
        )),
        (None, None) => Err(invalid(
            "a loop/repeat node names neither 'until' nor 'count'; name exactly one".to_owned(),
        )),
        (Some(name), None) => {
            let key = predicate_by_name(name)
                .ok_or_else(|| invalid(format!("unknown predicate '{name}'")))?;
            Ok(LoopControl::Until(key))
        }
        (None, Some(count)) => Ok(LoopControl::Count(*count)),
    }
}

/// Recursively converts one model-submitted tree node into a [`GateExpr`],
/// resolving every block/predicate/discriminator name against the sealed
/// catalogue as it goes and allocating a fresh [`ConfigId`] per leaf step.
///
/// Depth-limited by [`MAX_MODEL_PLAN_TREE_DEPTH`] independently of shape
/// validation: [`GateExpr::validate_shape`] and the host's own
/// [`CompositionLimits`] still decide whether the resulting tree is
/// admissible; this only stops recursion from a pathological answer before it
/// could ever reach either.
#[expect(
    clippy::too_many_arguments,
    reason = "one recursive builder threading the plan-wide config table, instance set, and config counter is clearer than a struct that exists only to satisfy this lint"
)]
fn build_expr_answer(
    expr: &ModelGateExprAnswer,
    facts: &PlannerFacts,
    catalogue: &CompositionCatalogueSnapshot,
    selection: super::domains::DomainSelection,
    configurations: &mut BTreeMap<ConfigId, ResolvedConfigurationRef>,
    seen_instances: &mut BTreeSet<String>,
    next_config: &mut usize,
    depth: u32,
    invalid: &impl Fn(String) -> PlannerError,
) -> Result<GateExpr, PlannerError> {
    if depth > MAX_MODEL_PLAN_TREE_DEPTH {
        return Err(invalid(format!(
            "gate tree nests deeper than {MAX_MODEL_PLAN_TREE_DEPTH} levels"
        )));
    }
    match expr {
        ModelGateExprAnswer::Step(step) => Ok(GateExpr::Step(build_step_answer(
            step,
            facts,
            catalogue,
            selection,
            configurations,
            seen_instances,
            next_config,
            invalid,
        )?)),
        ModelGateExprAnswer::Seq { steps } => {
            if steps.is_empty() {
                return Err(invalid("a seq node names no steps".to_owned()));
            }
            let children = steps
                .iter()
                .map(|child| {
                    build_expr_answer(
                        child,
                        facts,
                        catalogue,
                        selection,
                        configurations,
                        seen_instances,
                        next_config,
                        depth + 1,
                        invalid,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(GateExpr::Seq(children))
        }
        ModelGateExprAnswer::Choose {
            predicate,
            when_true,
            when_false,
        } => {
            let predicate_key = predicate_by_name(predicate)
                .ok_or_else(|| invalid(format!("unknown predicate '{predicate}'")))?;
            let when_true = build_expr_answer(
                when_true,
                facts,
                catalogue,
                selection,
                configurations,
                seen_instances,
                next_config,
                depth + 1,
                invalid,
            )?;
            let when_false = build_expr_answer(
                when_false,
                facts,
                catalogue,
                selection,
                configurations,
                seen_instances,
                next_config,
                depth + 1,
                invalid,
            )?;
            Ok(GateExpr::Choose {
                predicate: predicate_key,
                when_true: Box::new(when_true),
                when_false: Box::new(when_false),
            })
        }
        ModelGateExprAnswer::Switch {
            discriminator,
            cases,
            default,
        } => {
            let (discriminator_key, valid_cases) = discriminator_by_name(discriminator)
                .ok_or_else(|| invalid(format!("unknown discriminator '{discriminator}'")))?;
            if cases.is_empty() && default.is_none() {
                return Err(invalid(
                    "a switch node names no cases and no default".to_owned(),
                ));
            }
            let mut built_cases = Vec::with_capacity(cases.len());
            let mut seen_cases = BTreeSet::new();
            for case in cases {
                if !valid_cases.iter().any(|valid| valid.as_str() == case.case) {
                    let known = valid_cases
                        .iter()
                        .map(CaseKey::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(invalid(format!(
                        "unknown case '{}' for discriminator '{discriminator}'; valid cases: {known}",
                        case.case
                    )));
                }
                if !seen_cases.insert(case.case.clone()) {
                    return Err(invalid(format!("duplicate case '{}'", case.case)));
                }
                let body = build_expr_answer(
                    &case.body,
                    facts,
                    catalogue,
                    selection,
                    configurations,
                    seen_instances,
                    next_config,
                    depth + 1,
                    invalid,
                )?;
                let case_key =
                    CaseKey::new(case.case.clone()).map_err(|error| invalid(error.to_string()))?;
                built_cases.push(SwitchCase::new(case_key, body));
            }
            let default_expr = match default {
                Some(body) => Some(Box::new(build_expr_answer(
                    body,
                    facts,
                    catalogue,
                    selection,
                    configurations,
                    seen_instances,
                    next_config,
                    depth + 1,
                    invalid,
                )?)),
                None => None,
            };
            Ok(GateExpr::Switch {
                discriminator: discriminator_key,
                cases: built_cases,
                default: default_expr,
            })
        }
        ModelGateExprAnswer::Fork { arms, reducer } => {
            if arms.is_empty() {
                return Err(invalid("a fork node names no arms".to_owned()));
            }
            let built_arms = arms
                .iter()
                .map(|arm| {
                    build_expr_answer(
                        arm,
                        facts,
                        catalogue,
                        selection,
                        configurations,
                        seen_instances,
                        next_config,
                        depth + 1,
                        invalid,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            let reducer_plan = build_step_answer(
                reducer,
                facts,
                catalogue,
                selection,
                configurations,
                seen_instances,
                next_config,
                invalid,
            )?;
            Ok(GateExpr::Fork {
                arms: built_arms,
                reducer: reducer_plan,
            })
        }
        ModelGateExprAnswer::Loop { body, until, count } => {
            let control = loop_control_from_answer(until, count, invalid)?;
            let body_expr = build_expr_answer(
                body,
                facts,
                catalogue,
                selection,
                configurations,
                seen_instances,
                next_config,
                depth + 1,
                invalid,
            )?;
            Ok(GateExpr::Loop {
                body: Box::new(body_expr),
                control,
                retention: keep_last_iteration_key(),
            })
        }
        ModelGateExprAnswer::Repeat { body, until, count } => {
            let control = loop_control_from_answer(until, count, invalid)?;
            let body_expr = build_expr_answer(
                body,
                facts,
                catalogue,
                selection,
                configurations,
                seen_instances,
                next_config,
                depth + 1,
                invalid,
            )?;
            Ok(GateExpr::Repeat {
                body: Box::new(body_expr),
                control,
                retention: keep_last_iteration_key(),
            })
        }
    }
}

/// The gate kinds described in [`model_plan`]'s prompt, in plain words, once
/// each — independent of any specific task's facts, except for `switch`: see
/// [`gate_kind_descriptions`].
const GATE_KIND_DESCRIPTIONS_BASE: &str = "\
Gate kinds: \
step runs one named block. \
seq runs an ordered list of nodes, one after another. \
choose runs when_true if a named predicate holds on the most recently produced value it reads, else when_false.";

/// `switch`'s own gate-kind sentence, appended by [`gate_kind_descriptions`]
/// only when at least one discriminator is nameable.
const SWITCH_KIND_DESCRIPTION: &str = " switch runs the case body matching a named discriminator's output, or default if no case matches.";

const GATE_KIND_DESCRIPTIONS_TAIL: &str = " \
fork runs every arm concurrently, then reduces all their outputs through one named reducer block. \
loop repeats body while a named predicate holds, checked before every iteration including the first (so it may run zero times), or a fixed number of times; every loop keeps only its most recent completed iteration. \
repeat is like loop, but always runs body once before that same check, so it is guaranteed to run at least once.";

/// Builds the gate-kinds prose for [`model_plan`]'s prompt: `switch` is
/// offered only once a discriminator actually exists to name
/// ([`nameable_discriminators`] non-empty) — offering a shape every
/// submission of it is refused is worse than not offering it at all. Nothing
/// else about the prompt has to change the day a discriminator is
/// registered: this sentence, and the matching `<node>` clause
/// [`model_plan`] splices in beside it, both come back on their own.
fn gate_kind_descriptions(switch_available: bool) -> String {
    let switch = if switch_available {
        SWITCH_KIND_DESCRIPTION
    } else {
        ""
    };
    format!("{GATE_KIND_DESCRIPTIONS_BASE}{switch}{GATE_KIND_DESCRIPTIONS_TAIL}")
}

/// Worked example 1: a stated edit, no test runner, in the flat legacy
/// grammar — `Seq(apply-direct, judge-stated-edits, conclude-single-pass)`,
/// the same shape [`direct_edit_expression`] builds. Proven to parse and
/// materialize by `worked_example_direct_edit_parses_and_materialises`.
const WORKED_EXAMPLE_DIRECT_EDIT: &str = "\
{\"steps\":[{\"instance\":\"apply\",\"block\":\"apply-direct\"},{\"instance\":\"judge\",\"block\":\"judge-stated-edits\"},{\"instance\":\"conclude\",\"block\":\"conclude-single-pass\"}],\"grounds\":\"one stated edit, no test runner: apply then judge then conclude\"}";

/// Worked example 2: a statement-only edit with a test runner — propose,
/// apply, test; on failure, re-propose once more seeing the captured
/// failure, then conclude either way. The one-level-deep shape
/// [`model_test_gated_expression`] degrades to after a revision, written as
/// an explicit `choose` tree rather than [`GateExpr::Loop`]/`Repeat` (the
/// repair arm applies a patch, which is `Mutating { repeat_safe: false }` and
/// this grammar exposes no wire for a fresh per-iteration producer). Proven
/// to parse, materialize, and run to a concluded coding contract by
/// `worked_example_test_gated_repair_parses_materialises_and_runs`.
const WORKED_EXAMPLE_TEST_GATED_REPAIR: &str = "\
{\"plan\":{\"kind\":\"seq\",\"steps\":[\
{\"kind\":\"step\",\"instance\":\"propose-0\",\"block\":\"model-propose\",\"config\":{\"model-propose\":{\"role\":\"propose-edit\",\"instruction\":\"propose a minimal patch for the stated issue\",\"tools\":[\"read_file\",\"list_dir\",\"search\"],\"token_budget\":50000,\"output\":\"patch-intent\"}}},\
{\"kind\":\"step\",\"instance\":\"apply-0\",\"block\":\"apply-patch-intent\"},\
{\"kind\":\"step\",\"instance\":\"run-tests-1\",\"block\":\"run-project-tests-captured\"},\
{\"kind\":\"step\",\"instance\":\"judge-1\",\"block\":\"judge-test-outcome\"},\
{\"kind\":\"choose\",\"predicate\":\"tests-passed\",\
\"when_true\":{\"kind\":\"step\",\"instance\":\"affirm-1\",\"block\":\"affirm-assessment\"},\
\"when_false\":{\"kind\":\"seq\",\"steps\":[\
{\"kind\":\"step\",\"instance\":\"propose-1\",\"block\":\"model-propose\",\"config\":{\"model-propose\":{\"role\":\"propose-edit-repair\",\"instruction\":\"the previous patch did not make the tests pass; propose a corrected minimal patch\",\"tools\":[\"read_file\",\"list_dir\",\"search\"],\"token_budget\":50000,\"output\":\"patch-intent\",\"sees_prior_failure\":true}}},\
{\"kind\":\"step\",\"instance\":\"apply-patch-1\",\"block\":\"apply-patch-intent\"},\
{\"kind\":\"step\",\"instance\":\"run-tests-2\",\"block\":\"run-project-tests-captured\"},\
{\"kind\":\"step\",\"instance\":\"judge-2\",\"block\":\"judge-test-outcome\"}\
]}},\
{\"kind\":\"step\",\"instance\":\"conclude\",\"block\":\"conclude-single-pass\"}\
]},\"grounds\":\"statement-only edit with a test runner: propose, apply, test, retry once on failure with the captured output, then conclude\"}";

/// Worked example 3: a statement-only edit, no test runner — the model
/// investigates with its own bounded tools, proposes a patch, it is applied
/// and judged against what actually landed. The same shape
/// [`model_proposed_edit_expression`] builds. Proven to parse, materialize,
/// and run to a concluded coding contract by
/// `worked_example_investigate_then_patch_parses_materialises_and_runs`.
const WORKED_EXAMPLE_INVESTIGATE_THEN_PATCH: &str = "\
{\"plan\":{\"kind\":\"seq\",\"steps\":[\
{\"kind\":\"step\",\"instance\":\"propose\",\"block\":\"model-propose\",\"config\":{\"model-propose\":{\"role\":\"propose-edit\",\"instruction\":\"use list_dir and search to find the relevant code, read_file to inspect it, then propose an exact-string patch\",\"tools\":[\"read_file\",\"list_dir\",\"search\"],\"token_budget\":50000,\"output\":\"patch-intent\"}}},\
{\"kind\":\"step\",\"instance\":\"apply\",\"block\":\"apply-patch-intent\"},\
{\"kind\":\"step\",\"instance\":\"judge\",\"block\":\"judge-patch-intent\"},\
{\"kind\":\"step\",\"instance\":\"conclude\",\"block\":\"conclude-single-pass\"}\
]},\"grounds\":\"statement-only edit with no test runner: investigate, propose, apply, judge what landed, conclude\"}";

/// Worked example 4: a read-only question — survey the tree, assess
/// findings, answer; nothing is written. The same shape
/// [`question_expression`] builds. Proven to parse, materialize, and run to
/// a concluded coding contract by
/// `worked_example_question_parses_materialises_and_runs`.
const WORKED_EXAMPLE_QUESTION: &str = "\
{\"plan\":{\"kind\":\"seq\",\"steps\":[\
{\"kind\":\"step\",\"instance\":\"survey\",\"block\":\"survey-tree\"},\
{\"kind\":\"step\",\"instance\":\"assess\",\"block\":\"assess-findings\"},\
{\"kind\":\"step\",\"instance\":\"answer\",\"block\":\"answer-question\"}\
]},\"grounds\":\"read-only question: survey, assess, answer\"}";

/// Renders the four worked examples above into the prompt text, each
/// captioned in plain words.
fn worked_examples_text() -> String {
    format!(
        "Worked examples (each is a complete, valid answer on its own):\n\
         1) A stated edit, no test runner — the flat legacy grammar still works: \
         {WORKED_EXAMPLE_DIRECT_EDIT}\n\
         2) A statement-only edit with a test runner — propose, apply, test; on failure, \
         re-propose once more seeing the captured failure, then conclude either way: \
         {WORKED_EXAMPLE_TEST_GATED_REPAIR}\n\
         3) A statement-only edit, no test runner — the model investigates with its own \
         bounded tools, proposes a patch, it is applied and judged against what actually \
         landed: {WORKED_EXAMPLE_INVESTIGATE_THEN_PATCH}\n\
         4) A read-only question — survey the tree, assess findings, answer; nothing is \
         written: {WORKED_EXAMPLE_QUESTION}"
    )
}

/// Name of the tool [`model_plan`] offers so the model can submit its chosen
/// plan as structured arguments instead of free text — the same move
/// `gate_blocks::configurable`'s `submit_answer` makes for a model-propose
/// step, applied here to the planner's own single-shot answer.
const SUBMIT_PLAN_TOOL: &str = "submit_plan";

/// The extra prompt text [`model_plan_for_selection`] appends under
/// [`super::domains::Arm::Composed`]: the selected domain's own expert notes
/// and two worked examples side by side — the generic scripted plan and
/// that domain's own expert plan for the same facts — each built for real
/// ([`scripted_plan`]/[`super::domains::plan_for_selection`]) and rendered
/// by [`render_gate_expression_shape`].
fn composed_context(
    selection: super::domains::DomainSelection,
    facts: &PlannerFacts,
    catalogue: &CompositionCatalogueSnapshot,
    target: &TopologyContractKey,
) -> Result<String, PlannerError> {
    let attempt = RevisionAttempt::new(1)?;
    let generic = scripted_plan(facts, attempt, catalogue, target)?;
    let expert = super::domains::plan_for_selection(
        super::domains::DomainSelection {
            arm: super::domains::Arm::Expert,
            ..selection
        },
        facts,
        attempt,
        catalogue,
        target,
    )?;
    Ok(format!(
        " Domain expert notes for {}:\n{}\nWorked example, generic plan: {}. \
         Worked example, {} expert plan: {}. These examples show useful ordering \
         and checks; branches can be expressed with the plan tree or the optional \
         expression tree. Submit available blocks with prerequisites before consumers. \
         A conversation-preserving proposal needs model-propose-conversation with the same \
         model-propose config, followed by extract-patch-from-proposal before apply-patch-intent.",
        selection.domain,
        super::domains::expert_notes(selection.domain),
        render_gate_expression_shape(generic.expression()),
        selection.domain,
        render_gate_expression_shape(expert.expression()),
    ))
}

/// Shared JSON-schema fragment for a model-propose config answer, reused by
/// both a flat leaf step and a tree `step` node: keeps `output`, `max_turns`,
/// `sees_prior_failure`, and `context_mode` in the schema so a `submit_plan`
/// tool call can drive the same settings the text grammar already does, not
/// just `role`/`instruction`/`tools`/`token_budget`.
fn model_propose_config_schema() -> serde_json::Value {
    serde_json::json!({
        "anyOf": [
            { "type": "null" },
            {
                "type": "object",
                "properties": {
                    "model-propose": {
                        "type": "object",
                        "properties": {
                            "role": { "type": "string" },
                            "instruction": { "type": "string" },
                            "tools": {
                                "type": "array",
                                "items": { "type": "string" },
                            },
                            "token_budget": { "type": "integer" },
                            "output": {
                                "type": "string",
                                "enum": ["plan", "findings", "patch-intent", "review", "answer"],
                            },
                            "max_turns": { "type": ["integer", "null"] },
                            "sees_prior_failure": { "type": "boolean" },
                            "context_mode": {
                                "type": "string",
                                "enum": ["fresh", "brief", "continue"],
                            },
                        },
                        "required": ["role", "instruction", "token_budget"],
                        "additionalProperties": false,
                    },
                },
                "required": ["model-propose"],
                "additionalProperties": false,
            },
        ],
    })
}

/// [`model_propose_config_schema`] plus the `{"registered": value}` form a
/// block registered under its own configuration schema takes (see
/// [`ModelGateConfigAnswer::Registered`]), for a tree `step` node. A flat
/// `steps` entry gains the same form from [`model_grammar::extend_schema`].
fn step_config_schema() -> serde_json::Value {
    let mut schema = model_propose_config_schema();
    schema["anyOf"]
        .as_array_mut()
        .expect("model_propose_config_schema is an anyOf list")
        .push(serde_json::json!({
            "type": "object",
            "properties": { "registered": {} },
            "required": ["registered"],
            "additionalProperties": false,
        }));
    schema
}

/// Shared JSON-schema fragment for a bare leaf step (no `kind` tag): the flat
/// `steps` grammar's own entry shape, and a `fork` node's `reducer`.
fn step_leaf_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "block": { "type": "string" },
            "config": model_propose_config_schema(),
        },
        "required": ["instance", "block"],
        "additionalProperties": false,
    })
}

/// Builds the `submit_plan` tool definition: its JSON-schema parameters
/// mirror [`ModelGatePlanAnswer`]'s own shape — either the flat legacy
/// `steps` grammar (optionally ordered by an `expression` tree) or the full
/// `plan` gate-expression tree, plus optional `wires` under either — so a
/// schema-conformant call decodes with the same [`serde_json::from_value`]
/// the text fallback below uses. [`model_grammar::extend_schema`] adds the
/// `expression`/`wires` properties, the flat steps' `registered` config
/// form, and `$defs.expression`; `$defs.node` is the `plan` tree's own node.
/// Registered non-strict: both trees are genuinely recursive, `wires` and
/// `expression` are optional, and a `switch` expression's case keys form a
/// dynamic map — Polaris strict-mode normalization would require every
/// property and close every map, and constrained decoding cannot enforce
/// these shapes any more precisely than this schema already states. The
/// real exhaustive checks are [`build_expr_answer`] and
/// [`model_grammar::Expression`]'s resolver walking the decoded answer
/// against the sealed catalogue, and Cosmos after them.
fn submit_plan_tool_definition() -> ToolDefinition {
    let node = serde_json::json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "kind": { "const": "step" },
                    "instance": { "type": "string" },
                    "block": { "type": "string" },
                    "config": step_config_schema(),
                },
                "required": ["kind", "instance", "block"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "const": "seq" },
                    "steps": { "type": "array", "items": { "$ref": "#/$defs/node" } },
                },
                "required": ["kind", "steps"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "const": "choose" },
                    "predicate": { "type": "string" },
                    "when_true": { "$ref": "#/$defs/node" },
                    "when_false": { "$ref": "#/$defs/node" },
                },
                "required": ["kind", "predicate", "when_true", "when_false"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "const": "switch" },
                    "discriminator": { "type": "string" },
                    "cases": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "case": { "type": "string" },
                                "body": { "$ref": "#/$defs/node" },
                            },
                            "required": ["case", "body"],
                            "additionalProperties": false,
                        },
                    },
                    "default": { "anyOf": [{ "type": "null" }, { "$ref": "#/$defs/node" }] },
                },
                "required": ["kind", "discriminator", "cases"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "const": "fork" },
                    "arms": { "type": "array", "items": { "$ref": "#/$defs/node" } },
                    "reducer": step_leaf_schema(),
                },
                "required": ["kind", "arms", "reducer"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "const": "loop" },
                    "body": { "$ref": "#/$defs/node" },
                    "until": { "type": ["string", "null"] },
                    "count": { "type": ["integer", "null"] },
                },
                "required": ["kind", "body"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "const": "repeat" },
                    "body": { "$ref": "#/$defs/node" },
                    "until": { "type": ["string", "null"] },
                    "count": { "type": ["integer", "null"] },
                },
                "required": ["kind", "body"],
                "additionalProperties": false,
            },
        ],
    });
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "steps": { "type": ["array", "null"], "items": step_leaf_schema() },
            "plan": { "anyOf": [{ "type": "null" }, { "$ref": "#/$defs/node" }] },
            "grounds": { "type": "string" },
        },
        "required": ["grounds"],
        "additionalProperties": false,
    });
    model_grammar::extend_schema(&mut schema);
    // `extend_schema` replaces `$defs` wholesale with its own `expression`
    // definition, so the `plan` tree's node definition is added after it.
    schema["$defs"]["node"] = node;
    ToolDefinition::new(
        SUBMIT_PLAN_TOOL,
        "Submits the chosen gate plan for this coding task: exactly one of a \
         flat ordered list of blocks (\"steps\", optionally ordered by an \
         \"expression\" tree) or a full gate-expression tree (\"plan\"), never \
         both, with optional typed \"wires\".",
        schema,
    )
    .with_strict(false)
}

/// Recovers a JSON object from a free-form model reply: strips a ```` ```json
/// ```` / ```` ``` ```` fence if present, then narrows to the first `{` …
/// last `}` span, which also drops any prose surrounding an unfenced object.
/// Only the text fallback in [`model_plan`] needs this — a `submit_plan`
/// tool call decodes straight from its already-parsed arguments.
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

/// Asks the configured model to submit a gate plan for one coding task, via a
/// [`SUBMIT_PLAN_TOOL`] tool call (or, for a plain-text reply, the same
/// lenient fence/prose-stripped fallback `gate_blocks::configurable`'s
/// `submit_answer` path uses), from the bounded grammar
/// [`ModelGatePlanAnswer`] derives over the blocks/predicates/discriminators
/// [`nameable_gate_blocks_for_selection`]/[`nameable_predicates`]/[`nameable_discriminators`]
/// expose from the sealed catalogue.
///
/// Mirrors [`composition::choose`]'s model branch: the prompt enumerates
/// every name the answer may use, the parsed result is checked again before
/// any catalogue key is trusted, and the meter is billed unconditionally
/// before validation, so a refused answer is still the auditable record of
/// a real call.
///
/// `history` is one optional hook for a caller that has past-outcome text to
/// share (see `cosmos::plan_history`): when
/// present, it is included in the prompt under its own short heading;
/// otherwise the prompt says nothing about history at all. Nothing here reads
/// or writes it beyond that.
///
/// This is [`model_plan_for_selection`] under the default (generic) domain
/// selection, whose prompt carries no domain knowledge at all.
#[expect(
    clippy::too_many_arguments,
    reason = "the model planner joins the plan grammar, revision context, catalogue, target contract, and optional history hook"
)]
pub async fn model_plan(
    facts: &PlannerFacts,
    prior_refusals: Option<&[Refusal]>,
    catalogue: &CompositionCatalogueSnapshot,
    target: &TopologyContractKey,
    registry: Option<&ModelRegistry>,
    model_id: Option<&str>,
    meter: &TokenMeter,
    history: Option<&str>,
) -> Result<GatePlan, PlannerError> {
    model_plan_for_selection(
        super::domains::DomainSelection::default(),
        facts,
        prior_refusals,
        catalogue,
        target,
        registry,
        model_id,
        meter,
        history,
    )
    .await
}

/// [`model_plan`] for one domain selection: the one real model-planner entry
/// point.
///
/// `selection` changes the prompt and the grammar only under
/// [`super::domains::Arm::Composed`]: the selected domain's own expert
/// blocks become nameable ([`nameable_gate_blocks_for_selection`]), its own
/// predicates and discriminators become nameable in the optional
/// `expression` tree ([`model_grammar`]), and [`composed_context`]'s notes and
/// worked examples are appended to the prompt. Every other arm leaves the
/// prompt byte-identical to [`model_plan`]'s.
#[expect(
    clippy::too_many_arguments,
    reason = "the selected model planner needs the same request context as the generic entry point"
)]
async fn model_plan_for_selection(
    selection: super::domains::DomainSelection,
    facts: &PlannerFacts,
    prior_refusals: Option<&[Refusal]>,
    catalogue: &CompositionCatalogueSnapshot,
    target: &TopologyContractKey,
    registry: Option<&ModelRegistry>,
    model_id: Option<&str>,
    meter: &TokenMeter,
    history: Option<&str>,
) -> Result<GatePlan, PlannerError> {
    let registry = registry.ok_or(PlannerError::ModelUnavailable)?;
    let model_id = model_id.ok_or(PlannerError::ModelUnavailable)?;
    let block_names = nameable_gate_blocks_for_selection(catalogue, facts, selection)
        .iter()
        .map(|block| format!("{} ({})", block.name, block.meaning))
        .collect::<Vec<_>>()
        .join("; ");
    let predicate_names = nameable_predicates()
        .iter()
        .map(|predicate| format!("{} ({})", predicate.name, predicate.meaning))
        .collect::<Vec<_>>()
        .join("; ");
    let discriminators = nameable_discriminators();
    let switch_available = !discriminators.is_empty();
    let switch_clause = if switch_available {
        " {\"kind\":\"switch\",\"discriminator\":\"...\",\"cases\":[{\"case\":\"...\",\"body\":<node>},...],\"default\":<node-or-null>};"
    } else {
        ""
    };
    let gate_kinds = gate_kind_descriptions(switch_available);
    let max_token_budget = configurable::MAX_MODEL_STEP_TOKEN_BUDGET;
    let discriminator_section = if discriminators.is_empty() {
        String::new()
    } else {
        let discriminator_names = discriminators
            .iter()
            .map(|discriminator| {
                format!(
                    "{} ({}; cases: {})",
                    discriminator.name,
                    discriminator.meaning,
                    discriminator
                        .cases
                        .iter()
                        .map(CaseKey::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("\nAvailable discriminators: {discriminator_names}.")
    };
    let revision_note = match prior_refusals {
        Some(refusals) if !refusals.is_empty() => {
            let details = refusals
                .iter()
                .map(|refusal| {
                    format!(
                        "{} at {} (expected {}, actual {}): {}",
                        refusal.code(),
                        refusal.path(),
                        refusal.expected(),
                        refusal.actual(),
                        refusal.repair_hint()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                " The previous plan was refused: {details}. Submit a whole new plan that avoids \
                 these problems, not a patch to the refused one."
            )
        }
        _ => String::new(),
    };
    let statement_only_hint = if facts.has_stated_edits {
        ""
    } else {
        " This task states no edits. Blocks that only apply or judge stated edits cannot fix it. \
          To change the workspace, use model-propose, apply-patch-intent, judge-patch-intent, \
          then conclude-single-pass."
    };
    let worked_examples = worked_examples_text();
    let history_section = match history {
        Some(text) if !text.trim().is_empty() => {
            format!("\n\nPast plan shapes (use count, resolved count):\n{text}")
        }
        _ => String::new(),
    };
    let mut prompt = format!(
        "Submit a gate plan for one coding task, via the submit_plan tool call (or, failing that, as \
         JSON text), in one of two shapes.\n\
         Shape A (a flat list of blocks, run in order unless an optional \"expression\" tree over \
         them is given, see below): \
         {{\"steps\":[{{\"instance\":\"...\",\"block\":\"...\",\"config\":null}}],\"grounds\":\"...\"}}.\n\
         Shape B (a full gate-expression tree): {{\"plan\":<node>,\"grounds\":\"...\"}}, where <node> is one \
         of: {{\"kind\":\"step\",\"instance\":\"...\",\"block\":\"...\",\"config\":null}}; \
         {{\"kind\":\"seq\",\"steps\":[<node>,...]}}; \
         {{\"kind\":\"choose\",\"predicate\":\"...\",\"when_true\":<node>,\"when_false\":<node>}};{switch_clause} \
         {{\"kind\":\"fork\",\"arms\":[<node>,...],\"reducer\":{{\"instance\":\"...\",\"block\":\"...\",\"config\":null}}}}; \
         {{\"kind\":\"loop\",\"body\":<node>,\"until\":\"<predicate>\"}} or {{\"kind\":\"loop\",\"body\":<node>,\"count\":<N>}}; \
         {{\"kind\":\"repeat\",\"body\":<node>,\"until\":\"<predicate>\"}} or {{\"kind\":\"repeat\",\"body\":<node>,\"count\":<N>}}.\n\
         Submit exactly one of \"steps\" or \"plan\", never both.\n\
         A step naming \"model-propose\" takes \
         {{\"model-propose\":{{\"role\":\"...\",\"instruction\":\"...\",\"tools\":[...],\
         \"token_budget\":number-up-to-{max_token_budget},\
         \"output\":\"plan\"|\"findings\"|\"patch-intent\"|\"review\"|\"answer\"-optional-default-patch-intent,\
         \"max_turns\":number-optional,\"sees_prior_failure\":boolean-optional,\
         \"context_mode\":\"fresh\"|\"brief\"|\"continue\"-optional}}}} as its config; choose \"output\" so the \
         very next step can use it (for example \"apply-patch-intent\" needs \"patch-intent\").\n\
         {gate_kinds}\n\
         Available blocks: {block_names}.\n\
         Available predicates: {predicate_names}.{discriminator_section}\n\
         {worked_examples}\n\
         Task statement: {}. Facts: registration={}, scope_size={}, success_check={}, objective={}, \
         latency_budget_ms={:?}, test_runner_available={}, reproduce_runner_available={}, \
         model_provider_available={}, is_question={}, is_survey={}, has_stated_edits={}.\
         {statement_only_hint}{revision_note}{history_section}\n\
         Every instance label must be unique across the whole plan. Name only the blocks, predicates, \
         and discriminators listed above.",
        facts.statement,
        facts.registration,
        facts.scope_size,
        facts.success_check,
        facts.objective,
        facts.latency_budget_ms,
        facts.test_runner_available,
        facts.reproduce_runner_available,
        facts.model_provider_available,
        facts.is_question,
        facts.is_survey,
        facts.has_stated_edits,
    );
    // A model-propose step gets only the tools its config names, so the
    // planner has to be told which names exist; without this list it guessed
    // read-only tools and its steps could never run a check.
    prompt.push_str(if facts.shell_available && !facts.test_runner_available {
        " model-propose tools: read_file, list_dir, search, run_python (runs a Python script \
          in the workspace, e.g. to reproduce the issue or check a fix)."
    } else {
        " model-propose tools: read_file, list_dir, search."
    });
    prompt.push_str(&model_grammar::prompt(catalogue, selection));
    if selection.arm == super::domains::Arm::Composed {
        prompt.push_str(&composed_context(selection, facts, catalogue, target)?);
    }
    let request = LlmRequest {
        messages: vec![Message::user(prompt.clone())],
        tools: Some(vec![submit_plan_tool_definition()]),
        max_output_tokens: meter
            .cost_cap_enabled()
            .then_some(PLANNER_MAX_OUTPUT_TOKENS),
        ..Default::default()
    };
    let permit = meter
        .reserve_request(model_id, &request, PLANNER_MAX_OUTPUT_TOKENS.get())
        .map_err(PlannerError::Model)?;
    meter.record_model_turn();
    let response = registry
        .llm(model_id)
        .map_err(|error| PlannerError::Model(error.to_string()))?
        .generate(request)
        .await
        .map_err(|error| PlannerError::Model(error.to_string()))?;
    if let Some(permit) = permit {
        permit.settle(&response.usage);
    }
    meter.add(
        response.usage.input_tokens.unwrap_or_default(),
        response.usage.output_tokens.unwrap_or_default(),
    );
    meter.add_cache(
        response.usage.cache_read_tokens.unwrap_or_default(),
        response.usage.cache_creation_tokens.unwrap_or_default(),
    );
    let raw = response.text();
    // Billed above, unconditionally: every check from here down carries this
    // exact attempt's digests, so a refusal is still the auditable record of
    // a real call rather than a discard of one the meter already charged.
    let prompt_digest = ContentDigest::sha256(prompt.as_bytes());
    // Text alone can be empty for a malformed submit_plan tool call. Hash
    // every content block so two distinct refused tool arguments retain
    // distinct submission identities in the revision trail.
    let response_bytes = serde_json::to_vec(&response.content)
        .expect("assistant content blocks are serializable by their provider contract");
    let response_digest = ContentDigest::sha256(&response_bytes);
    let invalid = |detail: String| PlannerError::InvalidModelAnswer {
        detail,
        prompt_digest,
        response_digest,
    };
    // A `submit_plan` call decodes straight from its own already-parsed
    // arguments; only a plain-text reply falls back to the lenient
    // fence/prose-stripped parse a revision prompt (fed by this same
    // `invalid` refusal, one level up) could otherwise keep tripping over.
    let answer: ModelGatePlanAnswer = match response
        .tool_calls()
        .into_iter()
        .find(|call| call.function.name == SUBMIT_PLAN_TOOL)
    {
        Some(tool_call) => serde_json::from_value(tool_call.function.arguments.clone())
            .map_err(|error| invalid(error.to_string()))?,
        None => {
            let text = extract_json_object(&raw).unwrap_or(raw.as_str());
            serde_json::from_str(text).map_err(|error| invalid(error.to_string()))?
        }
    };
    if answer.grounds.trim().is_empty() {
        return Err(invalid("grounds are empty".to_owned()));
    }
    if answer.plan.is_some() && answer.expression.is_some() {
        return Err(invalid(
            "an 'expression' tree orders the declared 'steps'; it cannot accompany 'plan', which \
             is already a tree"
                .to_owned(),
        ));
    }
    let mut seen_instances = BTreeSet::new();
    let mut configurations = BTreeMap::new();
    let mut next_config = 0usize;
    let expression = match (answer.steps, answer.plan) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "answer names both 'steps' and 'plan'; submit exactly one".to_owned(),
            ));
        }
        (None, None) => {
            return Err(invalid(
                "answer names neither 'steps' nor 'plan'; submit exactly one".to_owned(),
            ));
        }
        (Some(steps), None) => {
            if steps.is_empty() {
                return Err(invalid("plan names no steps".to_owned()));
            }
            if steps.len() > CompositionLimits::conservative().max_instances as usize {
                return Err(invalid("plan exceeds host instance limit".to_owned()));
            }
            let built = steps
                .iter()
                .map(|step| {
                    build_step_answer(
                        step,
                        facts,
                        catalogue,
                        selection,
                        &mut configurations,
                        &mut seen_instances,
                        &mut next_config,
                        &invalid,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            match answer.expression {
                Some(expression) => {
                    let mut declared = built
                        .into_iter()
                        .map(|step| (step.instance().as_str().to_owned(), step))
                        .collect::<BTreeMap<_, _>>();
                    let expression = expression
                        .resolve(&mut declared, catalogue, selection, 1)
                        .map_err(&invalid)?;
                    if !declared.is_empty() {
                        return Err(invalid(
                            "expression must use every declared step".to_owned(),
                        ));
                    }
                    expression
                }
                None => {
                    let mut built = built.into_iter().map(GateExpr::Step).collect::<Vec<_>>();
                    if built.len() == 1 {
                        built.remove(0)
                    } else {
                        GateExpr::Seq(built)
                    }
                }
            }
        }
        (None, Some(plan)) => build_expr_answer(
            &plan,
            facts,
            catalogue,
            selection,
            &mut configurations,
            &mut seen_instances,
            &mut next_config,
            0,
            &invalid,
        )?,
    };
    let wires = answer
        .wires
        .iter()
        .map(model_grammar::Wire::resolve)
        .collect::<Result<Vec<_>, _>>()
        .map_err(&invalid)?;
    GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        target.clone(),
        expression,
        wires,
        configurations,
    )
    .map_err(|error| invalid(error.to_string()))
}

// ---------------------------------------------------------------------------
// The synthetic finite-composition envelope every gate-plan decision still
// carries, and the revision-loop orchestration built on top of it.
// ---------------------------------------------------------------------------

fn envelope_configuration(value: &str) -> Result<ResolvedConfigurationRef, PlannerError> {
    Ok(ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new(GATE_PLAN_CONFIGURATION_SCHEMA)?,
            behavior_version(),
        ),
        ConfigurationValue::String(value.to_owned()),
    )?)
}

fn collect_step_refs(expression: &GateExpr, steps: &mut Vec<StepPlan>) {
    match expression {
        GateExpr::Step(step) => steps.push(step.clone()),
        GateExpr::Seq(children) => children
            .iter()
            .for_each(|child| collect_step_refs(child, steps)),
        GateExpr::Choose {
            when_true,
            when_false,
            ..
        } => {
            collect_step_refs(when_true, steps);
            collect_step_refs(when_false, steps);
        }
        GateExpr::Switch { cases, default, .. } => {
            cases
                .iter()
                .for_each(|case| collect_step_refs(case.body(), steps));
            if let Some(default) = default {
                collect_step_refs(default, steps);
            }
        }
        GateExpr::Fork { arms, reducer } => {
            arms.iter().for_each(|arm| collect_step_refs(arm, steps));
            steps.push(reducer.clone());
        }
        GateExpr::Loop { body, .. } | GateExpr::Repeat { body, .. } => {
            collect_step_refs(body, steps);
        }
        GateExpr::Guard { step, .. } => steps.push(step.clone()),
        GateExpr::Seal { body, .. } => collect_step_refs(body, steps),
        GateExpr::Slot { candidates, .. } => candidates
            .iter()
            .for_each(|candidate| collect_step_refs(candidate, steps)),
    }
}

/// Describes every block instance a gate plan's tree names, bounded at
/// [`MAX_COMPOSITION_BLOCKS`].
///
/// This is a coarse audit description, exactly like the finite-recipe
/// world's own [`CompositionBlockRef`] list: the real, per-instance
/// resolved configuration lives in the [`GatePlan`] itself, not here.
fn gate_plan_blocks(expression: &GateExpr) -> Result<Vec<CompositionBlockRef>, PlannerError> {
    let mut steps = Vec::new();
    collect_step_refs(expression, &mut steps);
    if steps.len() > MAX_COMPOSITION_BLOCKS {
        return Err(PlannerError::TooManyBlocks(steps.len()));
    }
    steps
        .into_iter()
        .map(|step| {
            Ok(CompositionBlockRef::new(
                step.block().clone(),
                envelope_configuration(step.instance().as_str())?,
            )?)
        })
        .collect()
}

fn gate_plan_choice(
    plan: &GatePlan,
    record: GatePlanRecord,
    mode: GatePlannerMode,
) -> Result<CompositionChoice, PlannerError> {
    let blocks = gate_plan_blocks(plan.expression())?;
    let recipe_config = envelope_configuration(&format!(
        "plan={};shape={}",
        record.plan_digest(),
        record.shape().digest()
    ))?;
    let recipe = CompositionRecipeRef::new(
        CompositionRecipeKey::new(
            CompositionRecipeId::new(GATE_PLAN_RECIPE)?,
            behavior_version(),
        ),
        recipe_config,
    )?;
    let composition_plan = polaris_cosmos::contracts::CompositionPlan::new(
        plan.catalogue().clone(),
        plan.catalogue_digest(),
        recipe,
        blocks,
    )?;
    Ok(CompositionChoice::new(
        composition_plan,
        CompositionChoiceOrigin::Autonomous {
            policy: policy(mode.as_str()),
        },
    )?)
}

/// The stable refusal code for a billed model answer that failed the
/// bounded grammar's own checks before any [`GatePlan`] could be built.
const INVALID_MODEL_ANSWER_CODE: &str = "planner.invalid-model-answer";

/// Builds the [`Refusal`] fed into the next revision's prompt when a billed
/// model answer could not become a [`GatePlan`] at all — bad JSON, an
/// unknown block, or any other check in [`model_plan`]'s own grammar.
///
/// The path is always root: the answer never resolved into a plan tree, so
/// there is no node in one to point at.
fn invalid_model_answer_refusal(detail: &str) -> Refusal {
    Refusal::new(
        INVALID_MODEL_ANSWER_CODE,
        GatePath::root(),
        None,
        "a JSON gate-plan answer naming only catalogue blocks and instances",
        detail,
        "submit a whole new plan as valid JSON, naming only the listed blocks",
    )
}

/// Builds the [`CompositionChoice`] that stands in for an attempt whose
/// model answer never became a [`GatePlan`].
///
/// [`polaris_cosmos::contracts::CompositionPlan::new`] requires at least one
/// block, so this attempt still needs one: a single sentinel reference to
/// an always-registered catalogue block, never itself run (this choice's
/// decision is retained only as a refused revision, and a refused
/// revision's plan is never materialised or executed). Its configuration
/// carries the prompt/response digests the meter already billed for, so the
/// refusal stays traceable to the exact call it came from.
fn invalid_model_answer_choice(
    mode: GatePlannerMode,
    prompt_digest: ContentDigest,
    response_digest: ContentDigest,
    catalogue: &CompositionCatalogueSnapshot,
) -> Result<CompositionChoice, PlannerError> {
    let recipe_config = envelope_configuration(&format!(
        "invalid-model-answer;prompt={prompt_digest};response={response_digest}"
    ))?;
    let recipe = CompositionRecipeRef::new(
        CompositionRecipeKey::new(
            CompositionRecipeId::new(GATE_PLAN_RECIPE)?,
            behavior_version(),
        ),
        recipe_config,
    )?;
    let blocks = vec![CompositionBlockRef::new(
        existing::conclude_single_pass_key(),
        envelope_configuration("invalid-model-answer")?,
    )?];
    let composition_plan = polaris_cosmos::contracts::CompositionPlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        recipe,
        blocks,
    )?;
    Ok(CompositionChoice::new(
        composition_plan,
        CompositionChoiceOrigin::Autonomous {
            policy: policy(mode.as_str()),
        },
    )?)
}

/// Identity for a refused submission that never produced a gate expression.
///
/// Cosmos requires a gate-plan identity on every revision. This reserved
/// shape cannot be confused with a real plan shape, and the digest identifies
/// the billed submission rather than claiming a `GatePlan` was built. The
/// validation's `planner.invalid-model-answer` refusal states why it has no
/// executable plan.
fn invalid_model_answer_record(
    prompt_digest: ContentDigest,
    response_digest: ContentDigest,
    catalogue: &CompositionCatalogueSnapshot,
) -> GatePlanRecord {
    GatePlanRecord::new(
        ContentDigest::sha256(
            format!("invalid-model-answer;prompt={prompt_digest};response={response_digest}")
                .as_bytes(),
        ),
        catalogue.digest(),
        GateExpressionShapeKey::new(ContentDigest::sha256(b"invalid-model-answer;no-gate-plan")),
    )
}

/// One gate-plan revision attempt's outcome, exactly as the planner produced
/// it: no evidence-store sequence number, since only the caller — reading
/// its own store at the moment it appends this attempt's evidence — can
/// assign one.
#[derive(Debug)]
pub enum GatePlanAttemptResult {
    /// A plan materialised into a validated native graph, ready for the
    /// caller's own live admission and execution — the same path a
    /// finite-recipe [`super::composition::MaterializedComposition`] takes.
    ///
    /// This is not yet a live-admitted, running graph: the caller still owns
    /// live admission and execution, and only it knows whether live
    /// admission itself admitted or refused this composition, and later,
    /// whether a run and outcome followed.
    Materialized {
        /// The autonomous choice this attempt's decision carries.
        choice: CompositionChoice,
        /// The real gate-plan identity: shape, digest, catalogue.
        record: GatePlanRecord,
        /// This attempt's one-based position in the revision loop.
        attempt: RevisionAttempt,
        /// The validated native graph, boxed so a refused attempt's much
        /// smaller variant does not pay for its size.
        graph: Box<Graph>,
        /// The graph's own audit witness.
        native_signature: NativeGraphSignatureWitness,
        /// The submitted plan's own gate-expression tree, rendered by
        /// [`render_gate_expression_shape`] while the plan itself is still in
        /// scope — the caller keeps only this label and [`GatePlanRecord`]'s
        /// digest identity, never the [`GateExpr`] tree itself.
        shape_label: String,
    },
    /// The attempt was refused before a graph could run — either
    /// materialisation rejected the plan, or (model mode) a billed answer
    /// never became a plan at all.
    Refused {
        /// The autonomous choice this attempt's decision carries.
        choice: CompositionChoice,
        /// The gate-plan identity, or the reserved invalid-submission
        /// identity when no gate plan could be resolved.
        record: GatePlanRecord,
        /// This attempt's one-based position in the revision loop.
        attempt: RevisionAttempt,
        /// The stable stage this refusal happened at, matching
        /// [`polaris_cosmos::contracts::CompositionValidation::refused`]'s
        /// own `stage` argument.
        stage: ExclusionCode,
        /// Every actionable refusal, fed into the next attempt's prompt.
        refusals: Vec<Refusal>,
    },
}

/// What one revision attempt submitted, before it is turned into a
/// [`GatePlanAttemptResult`] by either materialising or refusing directly.
///
/// A model attempt that never became a [`GatePlan`] at all — bad JSON, an
/// unknown block, any other check [`model_plan`]'s own grammar makes — is
/// still a billed, auditable attempt, not a reason to stop the turn; this
/// variant carries the refusal it becomes directly, since there is no plan
/// left to try materialising.
enum AttemptSubmission {
    /// A well-formed plan, ready to try materialising. Boxed so this variant
    /// stays close in size to [`AttemptSubmission::InvalidAnswer`], which
    /// carries no plan at all.
    Plan {
        plan: Box<GatePlan>,
        record: GatePlanRecord,
        choice: CompositionChoice,
    },
    /// A billed answer that never resolved into a plan.
    InvalidAnswer {
        refusal: Refusal,
        choice: CompositionChoice,
        record: GatePlanRecord,
    },
}

/// Runs one turn's whole gate-plan revision loop: builds a plan, tries to
/// materialise it, and on refusal submits a whole new plan — never a patch
/// to the refused one — up to [`MAX_GATE_PLAN_ATTEMPTS`] total attempts.
///
/// Returns every attempt made, oldest first, as plain
/// [`GatePlanAttemptResult`]s — no sequence numbers, since nothing built here
/// is appended to an evidence store. All but the last are always
/// [`GatePlanAttemptResult::Refused`]: the loop returns immediately once an
/// attempt materialises. The last is
/// [`GatePlanAttemptResult::Materialized`] when some attempt succeeded, or
/// itself `Refused` when every attempt was exhausted — the caller then falls
/// back to the finite recipe grammar. A budget already spent before the
/// first attempt returns an empty `Vec`.
///
/// `chooser_budget` is the turn's own shared count of real model requests
/// billed so far, across every fallback candidate this turn tries. A
/// [`GatePlannerMode::Model`] attempt debits it immediately before each
/// request, including revisions, and the loop stops rather than issue a
/// request once [`composition::MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN`] is
/// already spent.
///
/// `domain_selection` dispatches scripted domain plans
/// ([`super::domains::plan_for_selection`]) or, for
/// [`super::domains::Arm::Composed`], adds the selected expert context and
/// wireable blocks to the model planner ([`model_plan_for_selection`]); it
/// is refused outright, before any attempt, when `Composed` is paired with
/// [`GatePlannerMode::Scripted`]. The default selection preserves the
/// generic scripted plan and model prompt.
///
/// `history` is passed straight through to every model-planner call this
/// loop makes; see [`model_plan`]'s own doc for what it does.
#[expect(
    clippy::too_many_arguments,
    reason = "the revision loop joins the plan grammar, materialisation limits, target contract, and optional history hook"
)]
pub async fn attempt_gate_plan(
    mode: GatePlannerMode,
    domain_selection: super::domains::DomainSelection,
    facts: &PlannerFacts,
    catalogue: &CompositionCatalogueSnapshot,
    limits: &CompositionLimits,
    contracts: &TopologyCatalogSnapshot,
    target: &TopologyContractKey,
    registry: Option<&ModelRegistry>,
    model_id: Option<&str>,
    meter: &TokenMeter,
    chooser_budget: &mut usize,
    history: Option<&str>,
) -> Result<Vec<GatePlanAttemptResult>, PlannerError> {
    if domain_selection.arm == super::domains::Arm::Composed && mode != GatePlannerMode::Model {
        return Err(PlannerError::InvalidConfiguration(
            "arm composed requires gate planner model".to_owned(),
        ));
    }
    let mut attempts = Vec::new();
    let mut prior_refusals: Option<Vec<Refusal>> = None;
    for attempt_number in 1..=MAX_GATE_PLAN_ATTEMPTS {
        let attempt = RevisionAttempt::new(attempt_number)?;

        if matches!(mode, GatePlannerMode::Model)
            && *chooser_budget >= composition::MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN
        {
            break;
        }

        let submission = match mode {
            GatePlannerMode::Scripted => {
                let plan = super::domains::plan_for_selection(
                    domain_selection,
                    facts,
                    attempt,
                    catalogue,
                    target,
                )?;
                let record = build_gate_plan_record(&plan);
                let choice = gate_plan_choice(&plan, record, mode)?;
                AttemptSubmission::Plan {
                    plan: Box::new(plan),
                    record,
                    choice,
                }
            }
            GatePlannerMode::Model => {
                *chooser_budget += 1;
                match model_plan_for_selection(
                    domain_selection,
                    facts,
                    prior_refusals.as_deref(),
                    catalogue,
                    target,
                    registry,
                    model_id,
                    meter,
                    history,
                )
                .await
                {
                    Ok(plan) => {
                        let record = build_gate_plan_record(&plan);
                        let choice = gate_plan_choice(&plan, record, mode)?;
                        AttemptSubmission::Plan {
                            plan: Box::new(plan),
                            record,
                            choice,
                        }
                    }
                    Err(PlannerError::InvalidModelAnswer {
                        detail,
                        prompt_digest,
                        response_digest,
                    }) => AttemptSubmission::InvalidAnswer {
                        refusal: invalid_model_answer_refusal(&detail),
                        record: invalid_model_answer_record(
                            prompt_digest,
                            response_digest,
                            catalogue,
                        ),
                        choice: invalid_model_answer_choice(
                            mode,
                            prompt_digest,
                            response_digest,
                            catalogue,
                        )?,
                    },
                    Err(other) => return Err(other),
                }
            }
        };

        match submission {
            AttemptSubmission::Plan {
                plan,
                record,
                choice,
            } => match materialize(&plan, catalogue, limits, contracts) {
                Ok(graph) => {
                    let signature = graph.signature();
                    let native_signature = NativeGraphSignatureWitness::new(
                        target.clone(),
                        ContentDigest::sha256(format!("{signature}").as_bytes()),
                    );
                    let shape_label = render_gate_expression_shape(plan.expression());
                    attempts.push(GatePlanAttemptResult::Materialized {
                        choice,
                        record,
                        attempt,
                        graph: Box::new(graph),
                        shape_label,
                        native_signature,
                    });
                    return Ok(attempts);
                }
                Err(refusals) => {
                    let stage = ExclusionCode::new("composition.gate-plan-materialize")?;
                    attempts.push(GatePlanAttemptResult::Refused {
                        choice,
                        record,
                        attempt,
                        stage,
                        refusals: refusals.clone(),
                    });
                    // Diagnostic only: the episode record keeps just the refusal count.
                    eprintln!("gate-plan attempt {attempt_number} refused: {refusals:?}");
                    prior_refusals = Some(refusals);
                }
            },
            AttemptSubmission::InvalidAnswer {
                refusal,
                choice,
                record,
            } => {
                let stage = ExclusionCode::new(INVALID_MODEL_ANSWER_CODE)?;
                attempts.push(GatePlanAttemptResult::Refused {
                    choice,
                    record,
                    attempt,
                    stage,
                    refusals: vec![refusal.clone()],
                });
                eprintln!(
                    "gate-plan attempt {attempt_number} refused (invalid answer): {refusal:?}"
                );
                prior_refusals = Some(vec![refusal]);
            }
        }
    }
    Ok(attempts)
}

#[cfg(test)]
mod tests {
    #[test]
    fn experiment_overrides_parse_only_valid_values() {
        assert_eq!(parse_exp_number(" 60 "), Some(60));
        assert_eq!(parse_exp_number("sixty"), None);
        assert_eq!(exp_number("POLARIS_CODE_EXP_TEST_NUMBER_UNSET"), None);
        assert!(LOOP_WORKFLOW.contains("existing tests"));
    }

    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use polaris::graph::GraphExecutor;
    use polaris::models::ModelRegistry;
    use polaris::models::llm::{
        AssistantBlock, GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream,
        StopReason as LlmStopReason, ToolCall, Usage, UserBlock,
    };
    use polaris::system::param::SystemContext;
    use polaris_cosmos::contracts::AssessmentConclusion;
    use polaris_cosmos::contracts::StopReason;

    use super::*;
    use crate::cosmos::gate_blocks::{
        coding_block_catalogue, coding_block_catalogue_for_lean, coding_target_contracts,
    };
    use crate::cosmos::payload::CodingWorkspace;
    use crate::cosmos::task::{CodingIntent, CodingTask, SuccessCheck};
    use crate::cosmos::topology;

    fn target_contract() -> TopologyContractKey {
        topology::contract()
            .expect("the coding contract registers cleanly")
            .key()
            .clone()
    }

    fn edit_facts(test_runner_available: bool) -> PlannerFacts {
        PlannerFacts {
            registration: "test".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available,
            reproduce_runner_available: false,
            // A model provider actually configured is exactly what lets
            // `model_plan` itself run, so every fixture in this module
            // declares one available — a fixture that wants to prove
            // `model-propose` is hidden overrides this field itself (see
            // `nameable_gate_blocks_hides_blocks_whose_other_required_facts_are_unmet`).
            model_provider_available: true,
            shell_available: false,
            is_question: false,
            is_survey: false,
            has_stated_edits: true,
            statement: "apply the stated edit".to_owned(),
        }
    }

    fn question_facts() -> PlannerFacts {
        PlannerFacts {
            is_question: true,
            ..edit_facts(false)
        }
    }

    fn statement_only_facts(test_runner_available: bool) -> PlannerFacts {
        PlannerFacts {
            has_stated_edits: false,
            statement: "add() subtracts instead of adding: fix the implementation".to_owned(),
            ..edit_facts(test_runner_available)
        }
    }

    fn statement_only_facts_with_shell() -> PlannerFacts {
        PlannerFacts {
            shell_available: true,
            ..statement_only_facts(false)
        }
    }

    /// A statement-only task with no stated test command, but a shell policy
    /// that would run the reproduce script's own command — the fact
    /// `terminal::topology`'s accept path can set even though `command=` was
    /// never given, see `docs/gate-planning.md`.
    fn statement_only_reproduce_facts() -> PlannerFacts {
        PlannerFacts {
            reproduce_runner_available: true,
            ..statement_only_facts(false)
        }
    }

    #[test]
    fn lean_cascade_materializes_with_warm_continuation_and_check_branch() {
        let catalogue = coding_block_catalogue_for_lean();
        let contracts = coding_target_contracts().unwrap();
        let mut options = lean::LeanOptions {
            cascade: true,
            ..Default::default()
        };
        for locate in [false, true] {
            options.locate = locate;
            let plan = scripted_plan_generic(
                &statement_only_facts_with_shell(),
                first_attempt(),
                &catalogue,
                &target_contract(),
                |facts, attempt| lean::lean_plan_body(facts, attempt, &options),
            )
            .unwrap();
            materialize(
                &plan,
                &catalogue,
                &CompositionLimits::conservative(),
                &contracts,
            )
            .expect("cascade gate plan must be structurally valid");
        }
    }

    #[tokio::test]
    async fn default_lean_plan_carries_locate_trim_cascade_and_one_cost_meter() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelConversation, ModelStepArtifact, ModelStepExecutor, ModelStepProvider,
            WorkspaceView,
        };
        use polaris::system::system::SystemError;
        use std::future::Future;
        use std::pin::Pin;

        type Calls = Arc<Mutex<Vec<(String, bool, TrimPolicy, bool, String)>>>;
        struct Scripted(Calls);
        impl Scripted {
            fn answer(
                &self,
                config: &ModelStepConfig,
                meter: &TokenMeter,
                resumed: bool,
            ) -> ModelStepArtifact {
                let model = config
                    .model_id
                    .as_deref()
                    .unwrap_or("anthropic/claude-sonnet-5");
                let permit = meter.reserve_model_call(model, 1, 10_000).unwrap().unwrap();
                permit.settle(&Usage {
                    input_tokens: Some(0),
                    output_tokens: Some(10_000),
                    ..Usage::default()
                });
                meter.record_model_turn();
                self.0.lock().unwrap().push((
                    config.role.clone(),
                    resumed,
                    config.trim,
                    meter.cost_cap_enabled(),
                    config.instruction.clone(),
                ));
                if config.role == "cheap-locate" {
                    ModelStepArtifact::Answer(
                        r#"{"suspects":["calc.py:add:1-2"],"reproduction":"wrong sum","tests":[],"open_questions":[]}"#
                            .to_owned(),
                    )
                } else {
                    ModelStepArtifact::Answer("done".to_owned())
                }
            }
        }
        impl ModelStepExecutor for Scripted {
            fn propose<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                Box::pin(async move { Ok(self.answer(config, meter, false)) })
            }

            fn propose_conversation<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                meter: &'a TokenMeter,
                resume: Option<&'a ModelConversation>,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<(ModelStepArtifact, ModelConversation), SystemError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    Ok((
                        self.answer(config, meter, resume.is_some()),
                        ModelConversation::default(),
                    ))
                })
            }
        }

        let options = lean::LeanOptions::from_lookup(|_| None).unwrap();
        assert!(options.trim && options.autotest && options.locate && options.cascade);
        assert_eq!(options.max_cost_usd, Some(1.50));
        let dir = tempfile::tempdir().unwrap();
        let task = CodingTask::from_statement(
            "default-lean",
            dir.path(),
            "fix add",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .unwrap();
        let catalogue = coding_block_catalogue_for_lean();
        let plan = scripted_plan_generic(
            &statement_only_facts_with_shell(),
            first_attempt(),
            &catalogue,
            &target_contract(),
            |facts, attempt| lean::lean_plan_body(facts, attempt, &options),
        )
        .unwrap();
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &coding_target_contracts().unwrap(),
        )
        .unwrap();
        let meter = TokenMeter::new();
        meter
            .set_task_cost_cap(options.max_cost_usd.unwrap())
            .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut globals = polaris::system::resource::Resources::new();
        globals.insert(polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_owned()])
                .with_allowed_commands(vec!["python *".to_owned()])
                .disable_cache(),
        ));
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(CodingWorkspace::for_task(&task));
        context.insert(meter.clone());
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(Scripted(calls.clone())));
        GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "cheap-locate");
        assert_eq!(calls[0].2, TrimPolicy::None);
        assert_eq!(calls[1].0, "lean");
        assert!(calls[1].4.contains("calc.py:add:1-2"));
        assert_eq!(calls[1].2, TrimPolicy::Batched);
        assert_eq!(calls[2].0, "lean");
        assert!(calls[2].1, "tier two must resume tier one's conversation");
        assert_eq!(calls[2].2, TrimPolicy::Batched);
        assert!(calls.iter().all(|call| call.3));
        // The three scripted bills share one cap: Haiku ($0.05) plus two
        // Sonnet tiers ($0.10 each) leave less than a $1.26 Sonnet request.
        assert!(
            meter
                .reserve_model_call("anthropic/claude-sonnet-5", 1, 126_000)
                .is_err()
        );
    }

    #[tokio::test]
    async fn lean_cascade_failure_resumes_the_same_model_conversation() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelConversation, ModelStepArtifact, ModelStepExecutor, ModelStepProvider,
            WorkspaceView,
        };
        use polaris::system::system::SystemError;
        use std::future::Future;
        use std::pin::Pin;

        struct Scripted(Arc<Mutex<Vec<(bool, String, u16)>>>);
        impl ModelStepExecutor for Scripted {
            fn propose<'a>(
                &'a self,
                _config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                _meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                Box::pin(async { Ok(ModelStepArtifact::Answer("done".to_owned())) })
            }

            fn propose_conversation<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                meter: &'a TokenMeter,
                resume: Option<&'a ModelConversation>,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<(ModelStepArtifact, ModelConversation), SystemError>>
                        + Send
                        + 'a,
                >,
            > {
                self.0.lock().unwrap().push((
                    resume.is_some(),
                    config.instruction.clone(),
                    config.max_turns,
                ));
                meter.record_model_turn();
                Box::pin(async {
                    Ok((
                        ModelStepArtifact::Answer("done".to_owned()),
                        ModelConversation::default(),
                    ))
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let task = CodingTask::from_statement(
            "cascade-resume",
            dir.path(),
            "fix it",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .unwrap();
        let catalogue = coding_block_catalogue_for_lean();
        let contracts = coding_target_contracts().unwrap();
        let options = lean::LeanOptions {
            locate: false,
            cascade: true,
            ..Default::default()
        };
        let plan = scripted_plan_generic(
            &statement_only_facts_with_shell(),
            first_attempt(),
            &catalogue,
            &target_contract(),
            |facts, attempt| lean::lean_plan_body(facts, attempt, &options),
        )
        .unwrap();
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut globals = polaris::system::resource::Resources::new();
        globals.insert(polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_owned()])
                .with_allowed_commands(vec!["python *".to_owned()])
                .disable_cache(),
        ));
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(CodingWorkspace::for_task(&task));
        context.insert(TokenMeter::new());
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(Scripted(calls.clone())));
        GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].0);
        assert!(calls[1].0, "second tier must resume the first conversation");
        assert!(
            calls[1]
                .1
                .contains("System check: Initial tree could not be recorded"),
            "failed system check must reach tier two: {}",
            calls[1].1
        );
        assert_eq!(calls[0].2, 18);
        assert_eq!(calls[1].2, 99);
    }

    fn first_attempt() -> RevisionAttempt {
        RevisionAttempt::new(1).expect("one is a valid attempt number")
    }

    // -----------------------------------------------------------------
    // render_gate_expression_shape: the readable label renders the whole
    // tree, not a flat block list, so two different shapes built from the
    // same blocks are never rendered identically.
    // -----------------------------------------------------------------

    #[test]
    fn render_gate_expression_shape_renders_nested_gate_names() {
        let config = no_config_id().expect("no-config id");
        let seq = direct_edit_expression(&config).expect("direct-edit shape builds");
        assert_eq!(
            render_gate_expression_shape(&seq),
            "Seq(apply-direct, judge-stated-edits, conclude-single-pass)"
        );
    }

    #[test]
    fn render_gate_expression_shape_distinguishes_a_seq_from_a_choose_over_the_same_two_blocks() {
        let config = no_config_id().expect("no-config id");
        let a = step_expr("a", existing::apply_direct_key(), config.clone()).expect("step a");
        let b = step_expr("b", existing::judge_stated_edits_key(), config.clone()).expect("step b");

        let seq = GateExpr::Seq(vec![a.clone(), b.clone()]);
        let choose = GateExpr::Choose {
            predicate: predicates::tests_passed_key(),
            when_true: Box::new(a),
            when_false: Box::new(b),
        };

        let seq_label = render_gate_expression_shape(&seq);
        let choose_label = render_gate_expression_shape(&choose);
        assert_eq!(seq_label, "Seq(apply-direct, judge-stated-edits)");
        assert_eq!(
            choose_label,
            "Choose(tests-passed, apply-direct, judge-stated-edits)"
        );
        assert_ne!(
            seq_label, choose_label,
            "two shapes built from the same blocks must render differently"
        );
    }

    #[test]
    fn render_gate_expression_shape_renders_loop_control_and_fork_reducer() {
        let config = no_config_id().expect("no-config id");
        let loop_expr = GateExpr::Loop {
            body: Box::new(
                step_expr(
                    "run",
                    repair::run_project_tests_captured_key(),
                    config.clone(),
                )
                .expect("loop body step"),
            ),
            control: LoopControl::Until(predicates::tests_passed_key()),
            retention: keep_last_iteration_key(),
        };
        assert_eq!(
            render_gate_expression_shape(&loop_expr),
            "Loop(until=tests-passed, run-project-tests-captured)"
        );

        let fork_expr = GateExpr::Fork {
            arms: vec![
                step_expr("a", existing::apply_direct_key(), config.clone()).expect("arm a"),
                step_expr("b", existing::apply_direct_key(), config).expect("arm b"),
            ],
            reducer: StepPlan::new(
                BlockInstanceId::new("reduce").expect("instance"),
                configurable::reduce_assessments_key(),
                no_config_id().expect("no-config id"),
            ),
        };
        assert_eq!(
            render_gate_expression_shape(&fork_expr),
            "Fork([apply-direct, apply-direct], reduce=reduce-assessments)"
        );
    }

    // -----------------------------------------------------------------
    // (a) three distinct scripted shapes, each materialising, validating,
    // and running to a concluded coding contract.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn scripted_direct_edit_shape_materialises_and_runs() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::new(
            "planner-fixture",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1 + 1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(&edit_facts(false), first_attempt(), &catalogue, &target)
            .expect("direct-edit plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("direct-edit plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("direct-edit plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert_eq!(workspace.read("lib.rs").expect("read back"), "1 + 1");
    }

    #[tokio::test]
    async fn scripted_test_gated_shape_materialises_and_runs() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepProvider, ScriptedModelStep,
        };
        use crate::cosmos::payload::ProjectTestRunner;
        use crate::cosmos::task::ExactEdit;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        // The stated edit lands somewhere that does not itself satisfy the
        // success check, so the initial test run genuinely fails and the
        // repair arm is the one exercised.
        std::fs::write(dir.path().join("lib.rs"), "todo!()\n// note").expect("seed file");
        let task = CodingTask::new(
            "planner-fixture",
            dir.path(),
            "lib.rs",
            "// note",
            "// noted",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(&edit_facts(true), first_attempt(), &catalogue, &target)
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
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(ProjectTestRunner::command(
            "sh",
            ["-c", "grep -q '1 + 1' lib.rs"],
        ));
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new("lib.rs", "todo!()", "1 + 1").expect("well-formed edit"),
                ],
                check: None,
            },
        )));
        context.insert(TokenMeter::default());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("test-gated plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("1 + 1"),
            "the repair arm's proposed patch must have landed"
        );
    }

    #[tokio::test]
    async fn scripted_statement_only_edit_shape_materialises_and_runs() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepProvider, ScriptedModelStep,
        };
        use crate::cosmos::task::ExactEdit;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        let task = CodingTask::from_statement(
            "planner-statement-fixture",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(
            &statement_only_facts(false),
            first_attempt(),
            &catalogue,
            &target,
        )
        .expect("statement-only edit plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("statement-only edit plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit")],
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("statement-only edit plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert_eq!(
            conclusion.conclusion_kind(),
            polaris_cosmos::contracts::ConclusionKind::Resolved
        );
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("a + b"),
            "the model's proposed patch must have landed"
        );
    }

    // -----------------------------------------------------------------
    // (a2) the self-check-gated shape: a statement-only edit with no test
    // runner but a shell policy that would allow a model-authored script.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn scripted_statement_only_edit_with_shell_and_no_check_falls_back_to_the_plain_judge() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepProvider, ScriptedModelStep,
        };
        use crate::cosmos::task::ExactEdit;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        let task = CodingTask::from_statement(
            "planner-self-check-fallback-fixture",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(
            &statement_only_facts_with_shell(),
            first_attempt(),
            &catalogue,
            &target,
        )
        .expect("self-check-gated plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("self-check-gated plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        // No `ShellExecutor` in context: the model submitted no check, so the
        // `has-check` gate must route through the plain judge rather than
        // ever reaching `run-check-script`.
        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit")],
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("self-check-gated plan executes with no check submitted");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("a + b"),
            "the model's proposed patch must have landed even with no check submitted"
        );
    }

    #[tokio::test]
    async fn scripted_statement_only_edit_with_shell_self_check_passes_on_first_try() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;

        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepProvider, ScriptedModelStep,
        };
        use crate::cosmos::task::ExactEdit;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        let task = CodingTask::from_statement(
            "planner-self-check-pass-fixture",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);
        let working_dir = workspace.config().working_dir.clone();

        // This sandbox may only have `python3` on its real PATH, but
        // `run_python_script` always shells out to the literal `python`; a
        // shim standing in for it keeps this test independent of the host's
        // own installed interpreters.
        let bin_dir = tempfile::tempdir().expect("temp bin dir");
        let python_shim = bin_dir.path().join("python");
        std::fs::write(&python_shim, "#!/bin/sh\nexec python3 \"$@\"\n").expect("write shim");
        let mut perms = std::fs::metadata(&python_shim)
            .expect("stat shim")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&python_shim, perms).expect("chmod shim");

        let shell = polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(&working_dir)
                .with_allowed_dirs(vec![working_dir.clone()])
                .with_allowed_commands(vec!["python *".to_owned()])
                .with_path(format!("{}:/usr/bin:/bin", bin_dir.path().display())),
        );

        let plan = scripted_plan(
            &statement_only_facts_with_shell(),
            first_attempt(),
            &catalogue,
            &target,
        )
        .expect("self-check-gated plan builds");
        gate_plan_blocks(plan.expression())
            .expect("self-check-gated plan fits the composition envelope");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("self-check-gated plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut globals = polaris::system::resource::Resources::new();
        globals.insert(shell);
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit")],
                check: Some("print('ok')".to_owned()),
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("self-check-gated plan executes when the check passes on the first try");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("a + b"),
            "the model's proposed patch must have landed"
        );
    }

    #[tokio::test]
    async fn scripted_statement_only_edit_shape_searches_reads_then_patches_a_python_fixture() {
        use std::collections::VecDeque;

        use polaris::models::ModelRegistry;
        use polaris::models::llm::{
            AssistantBlock, GenerationError, LlmProvider, LlmResponse, LlmStream,
            StopReason as LlmStopReason, ToolCall, ToolFunction, Usage,
        };

        use crate::cosmos::gate_blocks::configurable::{LiveModelStep, ModelStepProvider};

        /// Answers one queued response per `generate` call: a `search` tool
        /// call, then a `read_file` tool call, then the final patch-intent
        /// JSON — exactly the sequence a real model would take to locate a
        /// bug from an issue statement alone before proposing a fix.
        struct ScriptedSearchThenReadProvider {
            responses: Mutex<VecDeque<LlmResponse>>,
        }

        impl LlmProvider for ScriptedSearchThenReadProvider {
            fn name(&self) -> &'static str {
                "scripted-search-then-read"
            }

            async fn generate(
                &self,
                _model: &str,
                _request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                self.responses
                    .lock()
                    .expect("mutex is never poisoned")
                    .pop_front()
                    .ok_or_else(|| {
                        GenerationError::UnsupportedOperation(
                            "no more scripted responses queued".to_owned(),
                        )
                    })
            }

            async fn stream(
                &self,
                _model: &str,
                _request: LlmRequest,
            ) -> Result<LlmStream, GenerationError> {
                unreachable!("this test never streams")
            }
        }

        fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> LlmResponse {
            LlmResponse {
                content: vec![AssistantBlock::tool_call(ToolCall {
                    id: id.to_owned(),
                    call_id: None,
                    function: ToolFunction {
                        name: name.to_owned(),
                        arguments,
                    },
                    signature: None,
                    additional_params: None,
                })],
                usage: Usage {
                    input_tokens: Some(50),
                    output_tokens: Some(20),
                    ..Usage::default()
                },
                stop_reason: LlmStopReason::ToolUse,
                id: None,
                model: None,
            }
        }

        fn answer(text: impl Into<String>) -> LlmResponse {
            LlmResponse {
                content: vec![AssistantBlock::text(text)],
                usage: Usage {
                    input_tokens: Some(80),
                    output_tokens: Some(40),
                    ..Usage::default()
                },
                stop_reason: LlmStopReason::EndTurn,
                id: None,
                model: None,
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        // A minimal fixture "Python repo": one buggy function, found by
        // searching the tree rather than being told the file up front.
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("calc.py"),
            "def add(a, b):\n    return a - b\n",
        )
        .expect("seed fixture file");
        let task = CodingTask::from_statement(
            "planner-python-fixture",
            dir.path(),
            "calc.py's add(a, b) returns a - b but it should return a + b",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(
            &statement_only_facts(false),
            first_attempt(),
            &catalogue,
            &target,
        )
        .expect("statement-only edit plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("statement-only edit plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(ScriptedSearchThenReadProvider {
            responses: Mutex::new(VecDeque::from(vec![
                tool_call(
                    "call-1",
                    "search",
                    serde_json::json!({ "pattern": "def add", "literal": true }),
                ),
                tool_call(
                    "call-2",
                    "read_file",
                    serde_json::json!({ "path": "calc.py" }),
                ),
                answer(
                    r#"{"kind":"patch-intent","edits":[{"file":"calc.py","find":"return a - b","replace":"return a + b"}]}"#,
                ),
            ])),
        });
        let llm = registry
            .llm("scripted-search-then-read/mock")
            .expect("the just-registered provider resolves");

        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepProvider::new(LiveModelStep::new(llm)));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("statement-only edit plan executes against the live executor");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert_eq!(
            conclusion.conclusion_kind(),
            polaris_cosmos::contracts::ConclusionKind::Resolved
        );
        assert_eq!(
            workspace.read("calc.py").expect("read back"),
            "def add(a, b):\n    return a + b\n",
            "the model must have searched, read, then proposed the exact fix"
        );
    }

    /// The cheap location step passes a typed report to a fresh `loop` arm loop; the
    /// follower sees no raw search transcript and retains direct-edit tools.
    #[tokio::test]
    async fn cheap_locate_precedes_loop_with_report_only() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepExecutor, ModelStepProvider, WorkspaceView,
        };
        use polaris::system::system::SystemError;
        use std::future::Future;
        use std::pin::Pin;

        type ReportCalls = Arc<Mutex<Vec<(String, String, Option<String>)>>>;
        struct ReportExecutor(ReportCalls);
        impl ModelStepExecutor for ReportExecutor {
            fn propose<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                _meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                Box::pin(async move {
                    self.0.lock().unwrap().push((
                        config.role.clone(),
                        config.instruction.clone(),
                        config.model_id.clone(),
                    ));
                    if config.role == "cheap-locate" {
                        Ok(ModelStepArtifact::Answer(r#"{"suspects":["calc.py:add:1-2 returns subtraction"],"reproduction":"add(1,2) returns -1","tests":["tests/test_calc.py:10"],"open_questions":["Is there another caller?"]}"#.to_owned()))
                    } else {
                        Ok(ModelStepArtifact::Answer(
                            "No edit in scripted test".to_owned(),
                        ))
                    }
                })
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("calc.py"),
            "def add(a, b):\n    return a - b\n",
        )
        .unwrap();
        let task = CodingTask::from_statement(
            "cheap-locate-fixture",
            dir.path(),
            "add subtracts",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .unwrap();
        let workspace = CodingWorkspace::for_task(&task);
        let plan = scripted_plan_generic(
            &statement_only_facts(false),
            first_attempt(),
            &catalogue,
            &target_contract(),
            locate_then_loop_statement_only_plan_body,
        )
        .unwrap();
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut context = SystemContext::new();
        context.insert(workspace);
        context.insert(TokenMeter::new());
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ReportExecutor(calls.clone())));
        GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].2.as_deref(),
            Some("anthropic/claude-haiku-4-5-20251001")
        );
        assert!(calls[1].1.contains("calc.py:add:1-2"));
        assert!(calls[1].1.contains("report may be wrong"));
        assert!(!calls[1].1.contains("ToolResult"));
        assert_eq!(calls[1].2, None);
    }

    /// The `loop` topology: the model step's own `edit_file` tool call changes the
    /// file directly, with no `apply-patch-intent`/judge step in the plan at
    /// all, and the conclude step makes no verification claim about it.
    #[tokio::test]
    async fn loop_statement_only_shape_edits_the_file_directly_and_concludes_unverified() {
        use polaris::models::llm::ToolCall;

        // A `Mutex<VecDeque<_>>` queue, matching the other scripted-provider
        // tests above: `generate` is `&self`, so the queue needs interior
        // mutability to pop from across calls.
        struct QueueProvider {
            responses: Mutex<std::collections::VecDeque<LlmResponse>>,
        }

        impl LlmProvider for QueueProvider {
            fn name(&self) -> &'static str {
                "scripted-loop"
            }

            async fn generate(
                &self,
                _model: &str,
                _request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                self.responses
                    .lock()
                    .expect("mutex is never poisoned")
                    .pop_front()
                    .ok_or_else(|| {
                        GenerationError::UnsupportedOperation(
                            "no more scripted responses queued".to_owned(),
                        )
                    })
            }

            async fn stream(
                &self,
                _model: &str,
                _request: LlmRequest,
            ) -> Result<LlmStream, GenerationError> {
                unreachable!("this test never streams")
            }
        }

        fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> LlmResponse {
            LlmResponse {
                content: vec![AssistantBlock::tool_call(ToolCall::new(
                    id, name, arguments,
                ))],
                usage: Usage {
                    input_tokens: Some(50),
                    output_tokens: Some(20),
                    ..Usage::default()
                },
                stop_reason: LlmStopReason::ToolUse,
                id: None,
                model: None,
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("calc.py"),
            "def add(a, b):\n    return a - b\n",
        )
        .expect("seed fixture file");
        let task = CodingTask::from_statement(
            "planner-loop-fixture",
            dir.path(),
            "calc.py's add(a, b) returns a - b but it should return a + b",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan_generic(
            &statement_only_facts(false),
            first_attempt(),
            &catalogue,
            &target,
            loop_statement_only_plan_body,
        )
        .expect("the loop arm's one-step plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the loop arm's plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(QueueProvider {
            responses: Mutex::new(std::collections::VecDeque::from(vec![
                tool_call(
                    "call-1",
                    "read_file",
                    serde_json::json!({ "path": "calc.py" }),
                ),
                tool_call(
                    "call-2",
                    "edit_file",
                    serde_json::json!({
                        "path": "calc.py",
                        "old_string": "return a - b",
                        "new_string": "return a + b",
                    }),
                ),
                tool_call(
                    "call-3",
                    "submit_answer",
                    serde_json::json!({
                        "kind": "answer",
                        "answer": "Fixed add() to return a + b",
                    }),
                ),
            ])),
        });
        let llm = registry
            .llm("scripted-loop/mock")
            .expect("the just-registered provider resolves");

        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(configurable::ModelStepProvider::new(
            configurable::LiveModelStep::new(llm),
        ));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the loop arm's plan executes: read, then a direct edit, then submit_answer");

        assert_eq!(
            workspace.read("calc.py").expect("read back"),
            "def add(a, b):\n    return a + b\n",
            "edit_file must have written the fix directly, with no apply-patch-intent block \
             in the plan at all"
        );
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(
            conclusion.stop_reason(),
            StopReason::PredicateSatisfied,
            "the model's own submit_answer call is what stopped the step"
        );
        assert_eq!(
            conclusion.conclusion_kind(),
            polaris_cosmos::contracts::ConclusionKind::Unresolved,
            "an unverified conclusion must never present itself as Resolved: nothing here \
             judged whether the edit is correct"
        );
    }

    #[tokio::test]
    async fn an_empty_proposed_patch_is_never_recorded_as_a_pass() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepProvider, ScriptedModelStep,
        };

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        let original = "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n";
        std::fs::write(dir.path().join("lib.rs"), original).expect("seed file");
        let task = CodingTask::from_statement(
            "planner-empty-patch-fixture",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(
            &statement_only_facts(false),
            first_attempt(),
            &catalogue,
            &target,
        )
        .expect("statement-only edit plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("statement-only edit plan materialises");

        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: Vec::new(),
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the plan still runs to completion even when the model proposes nothing");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_ne!(
            conclusion.conclusion_kind(),
            polaris_cosmos::contracts::ConclusionKind::Resolved,
            "an empty proposed patch must never be recorded as a resolved success"
        );
        assert_eq!(
            workspace.read("lib.rs").expect("read back"),
            original,
            "nothing should have been written to the tree"
        );
    }

    #[tokio::test]
    async fn scripted_statement_only_test_gated_shape_repairs_using_the_captured_failure() {
        use std::collections::VecDeque;
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use polaris::system::system::SystemError;

        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepExecutor, ModelStepProvider, WorkspaceView,
        };
        use crate::cosmos::payload::ProjectTestRunner;
        use crate::cosmos::task::ExactEdit;

        struct RecordingModelStep {
            responses: Mutex<VecDeque<ModelStepArtifact>>,
            calls: AtomicUsize,
        }

        impl ModelStepExecutor for RecordingModelStep {
            fn propose<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                _meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                Box::pin(async move {
                    let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
                    if call_index == 1 {
                        // The retry step: it must see why the first attempt
                        // did not pass, not only that it did not.
                        assert!(
                            config.instruction.contains("MISSING_THE_FIX"),
                            "the retry must read the previous attempt's captured test \
                             output: {}",
                            config.instruction
                        );
                    }
                    Ok(self
                        .responses
                        .lock()
                        .expect("mutex is never poisoned")
                        .pop_front()
                        .expect("a scripted response is queued for every call"))
                })
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        let task = CodingTask::from_statement(
            "planner-statement-repair-fixture",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(
            &statement_only_facts(true),
            first_attempt(),
            &catalogue,
            &target,
        )
        .expect("statement-only test-gated plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("statement-only test-gated plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let executor = RecordingModelStep {
            responses: Mutex::new(VecDeque::from(vec![
                // First attempt: a harmless, insufficient edit — the test
                // still fails, and the command's own stderr names why.
                ModelStepArtifact::PatchIntent {
                    edits: vec![
                        ExactEdit::new("lib.rs", "fn add", "fn add /* attempt 1 */")
                            .expect("well-formed edit"),
                    ],
                    check: None,
                },
                // Retry, informed by the captured failure: the actual fix.
                ModelStepArtifact::PatchIntent {
                    edits: vec![
                        ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit"),
                    ],
                    check: None,
                },
            ])),
            calls: AtomicUsize::new(0),
        };

        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ProjectTestRunner::command(
            "sh",
            [
                "-c",
                "grep -q 'a + b' lib.rs || { echo MISSING_THE_FIX >&2; exit 1; }",
            ],
        ));
        context.insert(ModelStepProvider::new(executor));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("statement-only test-gated plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("a + b"),
            "the repair arm's proposed patch must have landed"
        );
    }

    #[tokio::test]
    async fn scripted_statement_only_reproduce_shape_writes_runs_and_stays_out_of_the_diff() {
        use std::future::Future;
        use std::pin::Pin;

        use polaris::system::system::SystemError;

        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepExecutor, ModelStepProvider, WorkspaceView,
        };
        use crate::cosmos::gate_blocks::reproduce::REPRODUCE_SCRIPT_PATH;
        use crate::cosmos::payload::ProjectTestRunner;
        use crate::cosmos::task::ExactEdit;

        /// Answers by the model-propose step's own configured role: the
        /// reproduce step never sees the code-fix instruction and vice versa,
        /// the same distinction `PlannerFacts::reproduce_runner_available`'s
        /// own doc draws between the two model calls this shape makes.
        struct RoleScriptedModel;

        impl ModelStepExecutor for RoleScriptedModel {
            fn propose<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                _meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                let artifact = match config.role.as_str() {
                    "reproduce" => ModelStepArtifact::Answer(
                        "print('reproducing add() subtracting instead of adding')\n".to_owned(),
                    ),
                    "propose-edit" => ModelStepArtifact::PatchIntent {
                        edits: vec![
                            ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit"),
                        ],
                        check: None,
                    },
                    other => panic!("unexpected model role {other}"),
                };
                Box::pin(async move { Ok(artifact) })
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        // A real git repository, the same convention the eval harness's own
        // `git add -A && git diff --cached` patch collection uses, so this
        // test proves the reproduce script's absence from the diff the same
        // way the harness would observe it — not merely that it landed under
        // `.polaris`.
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["add", "-A"]);
        run_git(
            dir.path(),
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        );

        let task = CodingTask::from_statement(
            "planner-statement-reproduce-fixture",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(
            &statement_only_reproduce_facts(),
            first_attempt(),
            &catalogue,
            &target,
        )
        .expect("statement-only reproduce plan builds");
        let blocks = gate_plan_blocks(plan.expression()).expect("plan tree describes cleanly");
        let block_names: Vec<&str> = blocks
            .iter()
            .map(|step| step.block().id().as_str())
            .collect();
        assert!(
            block_names.contains(&"code.write-reproduce-script"),
            "the reproduce shape must name the write-reproduce-script block: {block_names:?}"
        );
        assert!(
            block_names.contains(&"code.run-project-tests-captured"),
            "the reproduce shape must run the captured test block: {block_names:?}"
        );

        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("statement-only reproduce plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(configurable::ModelStepHandoff::default());
        // A fixed fake runner, exactly like `run_project_tests_captured`'s own
        // unit tests use — the plan wires it identically whether the real
        // command is a stated one or (as here) the reproduce script's own
        // fixed path; this test's own point is the plan's shape and the
        // write's effect on the tree, not a real Python interpreter.
        context.insert(ProjectTestRunner::command(
            "sh",
            ["-c", "grep -q 'a + b' lib.rs || exit 1"],
        ));
        context.insert(ModelStepProvider::new(RoleScriptedModel));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("statement-only reproduce plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("a + b"),
            "the code-fix model-propose's patch must have landed"
        );
        assert!(
            dir.path().join(REPRODUCE_SCRIPT_PATH).is_file(),
            "the reproduce script must have been written to its fixed path"
        );

        run_git(dir.path(), &["add", "-A"]);
        let diff = run_git(dir.path(), &["diff", "--cached", "--name-only"]);
        assert!(
            !diff.contains("reproduce"),
            "the reproduce script must never reach a git add -A/diff patch: {diff:?}"
        );
        assert!(
            diff.contains("lib.rs"),
            "the real edit must still reach the patch: {diff:?}"
        );
    }

    /// Runs `git` in `dir`, for the reproduce shape's own diff-exclusion test
    /// only — not a general test helper.
    fn run_git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[tokio::test]
    async fn scripted_question_shape_materialises_and_runs() {
        use crate::cosmos::payload::CodingQuestion;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::from_statement(
            "planner-question-fixture",
            dir.path(),
            "where does todo!() occur",
            CodingIntent::Answer,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed question task")
        .with_question("where is todo!()");
        let workspace = CodingWorkspace::for_task(&task);

        let plan = scripted_plan(&question_facts(), first_attempt(), &catalogue, &target)
            .expect("question plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("question plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(CodingQuestion::locate("todo!()"));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("question plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        // Writing nothing is the whole point of the read-only shape.
        assert_eq!(workspace.read("lib.rs").expect("read back"), "todo!()");
    }

    #[test]
    fn loop_twm_is_opt_in_and_preserves_the_unset_plan_digest() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let facts = statement_only_facts(false);
        let original = scripted_plan_generic(
            &facts,
            first_attempt(),
            &catalogue,
            &target,
            loop_statement_only_plan_body,
        )
        .unwrap();
        for value in [None, Some("0")] {
            let options = twm::TwmOptions::from_lookup(|key| {
                (key == "POLARIS_CODE_LEAN_TWM")
                    .then(|| value.map(str::to_owned))
                    .flatten()
            })
            .unwrap();
            let off =
                scripted_plan_generic(&facts, first_attempt(), &catalogue, &target, |f, a| {
                    loop_plan_with_twm(f, a, options)
                })
                .unwrap();
            assert_eq!(original.plan_digest(), off.plan_digest());
        }
        let options = twm::TwmOptions {
            note: true,
            test_status: true,
            codemap: true,
            ..Default::default()
        };
        let (_, configs) = loop_plan_with_twm(&facts, first_attempt(), Some(options)).unwrap();
        assert_eq!(
            ModelStepConfig::decode(&configs[0].1).unwrap().twm,
            Some(options)
        );
        let on = scripted_plan_generic(&facts, first_attempt(), &catalogue, &target, |f, a| {
            loop_plan_with_twm(f, a, Some(options))
        })
        .unwrap();
        assert_ne!(original.plan_digest(), on.plan_digest());
    }

    #[test]
    fn the_three_scripted_shapes_derive_different_plan_digests() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let attempt = first_attempt();

        let direct = scripted_plan(&edit_facts(false), attempt, &catalogue, &target)
            .expect("direct-edit plan builds");
        let test_gated = scripted_plan(&edit_facts(true), attempt, &catalogue, &target)
            .expect("test-gated plan builds");
        let question = scripted_plan(&question_facts(), attempt, &catalogue, &target)
            .expect("question plan builds");

        assert_ne!(direct.plan_digest(), test_gated.plan_digest());
        assert_ne!(direct.plan_digest(), question.plan_digest());
        assert_ne!(test_gated.plan_digest(), question.plan_digest());
    }

    #[test]
    fn a_statement_only_edit_gets_its_own_shape_and_digest() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let attempt = first_attempt();

        let stated_direct = scripted_plan(&edit_facts(false), attempt, &catalogue, &target)
            .expect("stated-edit plan builds");
        let stated_test_gated = scripted_plan(&edit_facts(true), attempt, &catalogue, &target)
            .expect("stated-edit test-gated plan builds");
        let statement_no_runner =
            scripted_plan(&statement_only_facts(false), attempt, &catalogue, &target)
                .expect("statement-only plan builds");
        let statement_with_runner =
            scripted_plan(&statement_only_facts(true), attempt, &catalogue, &target)
                .expect("statement-only test-gated plan builds");

        assert_ne!(
            stated_direct.plan_digest(),
            statement_no_runner.plan_digest(),
            "a task with no stated edits must not resolve to the same plan as one that named its own"
        );
        assert_ne!(
            stated_test_gated.plan_digest(),
            statement_with_runner.plan_digest()
        );
        assert_ne!(
            statement_no_runner.plan_digest(),
            statement_with_runner.plan_digest(),
            "whether a test runner is installed must still distinguish the statement-only shape"
        );
    }

    #[test]
    fn a_multi_target_edit_gets_a_different_shape_and_digest_than_a_single_target_one() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let attempt = first_attempt();

        // Same registration and runner availability (none): two stated edits
        // that differ only in how many sites they name must not resolve to the
        // identical plan.
        let single = scripted_plan(&edit_facts(false), attempt, &catalogue, &target)
            .expect("single-target plan builds");
        let multi = scripted_plan(
            &PlannerFacts {
                scope_size: 3,
                ..edit_facts(false)
            },
            attempt,
            &catalogue,
            &target,
        )
        .expect("multi-target plan builds");

        assert_ne!(
            single.plan_digest(),
            multi.plan_digest(),
            "a differently scoped edit is a differently shaped plan, not a coincidence of digest"
        );

        let single_graph = materialize(
            &single,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the single-target plan materialises");
        let multi_graph = materialize(
            &multi,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the multi-target plan materialises");
        assert!(
            single_graph.validate().is_ok(),
            "{:?}",
            single_graph.validate().errors()
        );
        assert!(
            multi_graph.validate().is_ok(),
            "{:?}",
            multi_graph.validate().errors()
        );
    }

    // -----------------------------------------------------------------
    // (b) a refusal feeds a whole new, genuinely shallower plan, correctly
    // ordered and evidenced.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn a_depth_refusal_revises_into_a_shallower_admitted_plan() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        // The first attempt's two-level repair loop nests six deep; the
        // degraded one-level shape a refusal revises into nests four deep
        // (see `test_gated_expression`). A max depth of four rejects only
        // the first attempt.
        let limits = CompositionLimits {
            max_depth: 4,
            ..CompositionLimits::conservative()
        };

        let attempts = attempt_gate_plan(
            GatePlannerMode::Scripted,
            crate::cosmos::domains::DomainSelection::default(),
            &edit_facts(true),
            &catalogue,
            &limits,
            &contracts,
            &target,
            None,
            None,
            &TokenMeter::default(),
            &mut 0,
            None,
        )
        .await
        .expect("the revision loop runs");

        assert_eq!(
            attempts.len(),
            2,
            "one refusal, then the shallower admitted plan"
        );

        let GatePlanAttemptResult::Refused {
            attempt, refusals, ..
        } = &attempts[0]
        else {
            panic!("the first attempt must be refused for exceeding the depth limit");
        };
        assert_eq!(attempt.get(), 1);
        assert!(
            refusals
                .iter()
                .any(|refusal| refusal.code() == "plan.limit-exceeded"),
            "the first attempt must be refused for exceeding the depth limit"
        );

        let GatePlanAttemptResult::Materialized { attempt, graph, .. } = &attempts[1] else {
            panic!("the shallower plan must have materialised");
        };
        assert_eq!(attempt.get(), 2, "the admitted plan is the second attempt");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    // -----------------------------------------------------------------
    // (c) exhausting every attempt records every refusal and starts
    // nothing.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn an_unsatisfiable_limit_exhausts_every_attempt() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        // Neither the two-level (depth six) nor the degraded one-level
        // (depth four) shape fits under a limit this tight.
        let limits = CompositionLimits {
            max_depth: 3,
            ..CompositionLimits::conservative()
        };

        let attempts = attempt_gate_plan(
            GatePlannerMode::Scripted,
            crate::cosmos::domains::DomainSelection::default(),
            &edit_facts(true),
            &catalogue,
            &limits,
            &contracts,
            &target,
            None,
            None,
            &TokenMeter::default(),
            &mut 0,
            None,
        )
        .await
        .expect("the revision loop runs");

        assert_eq!(attempts.len(), usize::from(MAX_GATE_PLAN_ATTEMPTS));
        for (index, result) in attempts.iter().enumerate() {
            let GatePlanAttemptResult::Refused { attempt, .. } = result else {
                panic!("no plan should fit under this limit");
            };
            assert_eq!(attempt.get(), (index as u8) + 1);
        }
    }

    // -----------------------------------------------------------------
    // (d) model planner with a mock provider: valid, invalid JSON, and an
    // unknown block name.
    // -----------------------------------------------------------------

    struct ScriptedAnswerProvider {
        answer: String,
        calls: Arc<AtomicUsize>,
        /// The exact user prompt sent with each request, in call order.
        prompts: Arc<Mutex<Vec<String>>>,
    }

    impl LlmProvider for ScriptedAnswerProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        async fn generate(
            &self,
            _model: &str,
            request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let [Message::User { content }] = request.messages.as_slice() else {
                panic!("gate planner sends one user message");
            };
            let [UserBlock::Text(block)] = content.as_slice() else {
                panic!("gate planner sends one text block");
            };
            self.prompts
                .lock()
                .expect("mutex is never poisoned")
                .push(block.text.clone());
            Ok(LlmResponse {
                content: vec![AssistantBlock::text(self.answer.clone())],
                usage: Usage {
                    input_tokens: Some(20),
                    output_tokens: Some(10),
                    total_tokens: Some(30),
                    ..Default::default()
                },
                stop_reason: LlmStopReason::EndTurn,
                id: None,
                model: None,
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmStream, GenerationError> {
            unreachable!("model_plan tests never stream")
        }
    }

    fn registry_with(answer: &str) -> ModelRegistry {
        registry_with_counter(answer, Arc::new(AtomicUsize::new(0)))
    }

    fn registry_with_counter(answer: &str, calls: Arc<AtomicUsize>) -> ModelRegistry {
        registry_with_recorder(answer, calls, Arc::new(Mutex::new(Vec::new())))
    }

    fn registry_with_recorder(
        answer: &str,
        calls: Arc<AtomicUsize>,
        prompts: Arc<Mutex<Vec<String>>>,
    ) -> ModelRegistry {
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(ScriptedAnswerProvider {
            answer: answer.to_owned(),
            calls,
            prompts,
        });
        registry
    }

    /// A provider that always answers by calling `submit_plan`, never with
    /// text — proves [`model_plan`] decodes a tool call's own arguments
    /// directly rather than requiring the text fallback.
    struct ScriptedToolCallProvider {
        arguments: serde_json::Value,
    }

    impl LlmProvider for ScriptedToolCallProvider {
        fn name(&self) -> &'static str {
            "scripted-tool-call"
        }

        async fn generate(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            Ok(LlmResponse {
                content: vec![AssistantBlock::tool_call(ToolCall::new(
                    "call-1",
                    SUBMIT_PLAN_TOOL,
                    self.arguments.clone(),
                ))],
                usage: Usage {
                    input_tokens: Some(20),
                    output_tokens: Some(10),
                    total_tokens: Some(30),
                    ..Default::default()
                },
                stop_reason: LlmStopReason::ToolUse,
                id: None,
                model: None,
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmStream, GenerationError> {
            unreachable!("model_plan tests never stream")
        }
    }

    fn registry_with_tool_call(arguments: serde_json::Value) -> ModelRegistry {
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(ScriptedToolCallProvider { arguments });
        registry
    }

    #[tokio::test]
    async fn model_planner_accepts_a_submit_plan_tool_call() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with_tool_call(serde_json::json!({
            "steps": [
                {"instance": "apply", "block": "apply-direct"},
                {"instance": "judge", "block": "judge-stated-edits"},
                {"instance": "conclude", "block": "conclude-single-pass"},
            ],
            "grounds": "model chose the direct shape",
        }));
        let meter = TokenMeter::default();

        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted-tool-call/model"),
            &meter,
            None,
        )
        .await
        .expect("a well-formed submit_plan call is accepted");
        assert_eq!(meter.input_tokens(), 20);

        let contracts = coding_target_contracts().expect("coding contract catalog");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the model-chosen plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    #[tokio::test]
    async fn model_planner_accepts_a_well_formed_answer() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                {"instance":"judge","block":"judge-stated-edits"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"model chose the direct shape"}"#,
        );
        let meter = TokenMeter::default();

        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a well-formed answer is accepted");
        assert_eq!(meter.input_tokens(), 20);

        let contracts = coding_target_contracts().expect("coding contract catalog");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the model-chosen plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    #[tokio::test]
    async fn model_planner_can_materialise_a_statement_only_patch_path() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[
                {"instance":"propose","block":"model-propose",
                 "config":{"model-propose":{"role":"propose","instruction":"fix the issue",
                  "tools":["read_file","search","run_python"],"token_budget":200000}}},
                {"instance":"apply","block":"apply-patch-intent"},
                {"instance":"judge","block":"judge-patch-intent"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"propose and apply a patch for the issue"}"#,
        );
        let plan = model_plan(
            &statement_only_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &TokenMeter::default(),
            None,
        )
        .await
        .expect("statement-only issue has a valid model-composed patch path");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the model-composed path materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    #[tokio::test]
    async fn model_planner_accepts_per_step_context_mode() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[
                {"instance":"propose","block":"model-propose",
                 "config":{"model-propose":{"role":"propose","instruction":"fix the issue",
                  "tools":["read_file"],"token_budget":200000,"context_mode":"brief"}}},
                {"instance":"apply","block":"apply-patch-intent"},
                {"instance":"judge","block":"judge-patch-intent"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"use a brief from earlier model steps"}"#,
        );
        let plan = model_plan(
            &statement_only_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &TokenMeter::default(),
            None,
        )
        .await
        .expect("context mode is accepted");
        assert!(format!("{:?}", plan.configurations()).contains("context_mode"));
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("context-aware model step materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    #[tokio::test]
    async fn model_planner_prompt_carries_the_statement_and_a_catalogue_only_block() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with_recorder(
            r#"{"steps":[{"instance":"propose","block":"model-propose",
                "config":{"model-propose":{"role":"revise","instruction":"fix it",
                "tools":["read_file"],"token_budget":1000,"output":"patch-intent"}}},
                {"instance":"apply","block":"apply-patch-intent"},
                {"instance":"judge","block":"judge-patch-intent"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"model chose the statement-driven shape"}"#,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&prompts),
        );
        let meter = TokenMeter::default();
        let facts = statement_only_facts(false);

        model_plan(
            &facts,
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a well-formed answer naming a catalogue-only block is accepted");

        let sent = prompts
            .lock()
            .expect("mutex is never poisoned")
            .first()
            .cloned()
            .expect("one request was sent");
        assert!(
            sent.contains(&facts.statement),
            "the prompt must carry the task's own statement: {sent}"
        );
        assert!(
            sent.contains("judge-patch-intent"),
            "the prompt must name a block this run only just added, proving the list came \
             from the sealed catalogue rather than a hand-typed subset: {sent}"
        );
    }

    /// Each block's own prompt line names its stable required-fact keys (or
    /// says it needs none), not just the separate flat fact dump: proves
    /// [`describe_for_prompt`] actually renders
    /// [`BlockDescription::required_facts`] rather than only using it to
    /// filter the menu.
    #[tokio::test]
    async fn model_planner_prompt_shows_each_blocks_required_facts() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with_recorder(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                {"instance":"judge","block":"judge-stated-edits"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"model chose the direct shape"}"#,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&prompts),
        );
        let meter = TokenMeter::default();

        model_plan(
            &edit_facts(true),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a well-formed answer is accepted");

        let sent = prompts
            .lock()
            .expect("mutex is never poisoned")
            .first()
            .cloned()
            .expect("one request was sent");
        assert!(
            sent.contains("run-project-tests-captured")
                && sent.contains("code.test-runner-available"),
            "a fact-gated block's line must name its required fact key: {sent}"
        );
        assert!(
            sent.contains("judge-test-outcome") && sent.contains("requires: none"),
            "a block with no required facts must say so explicitly: {sent}"
        );
    }

    /// `switch` names a real, parseable [`GateExpr::Switch`] shape (see
    /// `model_planner_refuses_a_switch_naming_any_discriminator_today` below),
    /// but every submission of it is refused today because
    /// [`nameable_discriminators`] is empty — so the prompt must not offer
    /// the `switch` node shape or describe it as a gate kind. Every other
    /// kind the grammar advertises must still be present.
    #[tokio::test]
    async fn model_planner_prompt_omits_switch_while_no_discriminator_is_registered() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with_recorder(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                {"instance":"judge","block":"judge-stated-edits"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"model chose the direct shape"}"#,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&prompts),
        );
        let meter = TokenMeter::default();

        model_plan(
            &edit_facts(true),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a well-formed answer is accepted");

        let sent = prompts
            .lock()
            .expect("mutex is never poisoned")
            .first()
            .cloned()
            .expect("one request was sent");
        // Checks the grammar clause and the gate-kind sentence specifically,
        // not the whole prompt for the bare word "switch": an unrelated
        // block's own catalogue example may mention "Switch" as a general
        // graph concept (e.g. "occupies a Choose/Switch arm"), which is not
        // the always-refused shape this finding is about.
        assert!(
            !sent.contains("\\\"kind\\\":\\\"switch\\\""),
            "the prompt must not offer the switch node shape: {sent}"
        );
        assert!(
            !sent.contains("switch runs the case body"),
            "the prompt must not describe the switch gate kind: {sent}"
        );
        for kind in [
            "seq runs",
            "choose runs",
            "fork runs",
            "loop repeats",
            "repeat is like loop",
        ] {
            assert!(
                sent.contains(kind),
                "expected {kind:?} still offered: {sent}"
            );
        }
    }

    #[tokio::test]
    async fn model_planner_refuses_unparseable_json() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with("not json at all");
        let meter = TokenMeter::default();

        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("unparseable JSON must be refused");
        assert!(matches!(error, PlannerError::InvalidModelAnswer { .. }));
        // Billed unconditionally, even though the answer was refused.
        assert_eq!(meter.input_tokens(), 20);
    }

    #[tokio::test]
    async fn model_planner_refuses_an_unknown_block_name() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"apply","block":"delete-the-repository"}],
                "grounds":"an answer naming a block outside the grammar"}"#,
        );
        let meter = TokenMeter::default();

        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("an unknown block name must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("unknown block"));
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    /// `code.model-direct-edit` is registered (the loop arm's own hand-written plan
    /// names it) but must never be reachable through the free-form model
    /// planner's own JSON grammar, which offers no way to distinguish a
    /// step that only proposes from one that can really write to the tree.
    /// Refused exactly like a name outside the catalogue altogether.
    #[tokio::test]
    async fn model_planner_refuses_the_direct_edit_block_by_name() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"loop","block":"model-direct-edit"}],
                "grounds":"an answer naming the mutating block directly"}"#,
        );
        let meter = TokenMeter::default();

        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("model-direct-edit must not be nameable by the model planner");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("unknown block"));
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_planner_refuses_a_missing_conversation_at_materialization() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"repair","block":"resume-model-propose"}],
                "grounds":"try a conversation block without a conversation"}"#,
        );
        // `resume-model-propose` declares a shell as a required fact, so it
        // is only nameable once one is declared available.
        let plan = model_plan(
            &statement_only_facts_with_shell(),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &TokenMeter::default(),
            None,
        )
        .await
        .expect("the grammar can name a registered conversation block");
        assert!(
            materialize(
                &plan,
                &catalogue,
                &CompositionLimits::conservative(),
                &coding_target_contracts().unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn generic_model_planner_does_not_offer_domain_expert_blocks() {
        let offered = nameable_gate_blocks(
            &coding_block_catalogue(),
            &statement_only_facts_with_shell(),
        );
        for domain_key in super::super::domains::keys() {
            assert!(
                !offered.iter().any(|block| block.key == domain_key),
                "a generic runtime plan must not carry domain knowledge: {domain_key}"
            );
        }
    }

    #[test]
    fn composed_offers_only_the_selected_domains_wireable_expert_blocks() {
        use crate::cosmos::domains::{Arm, Domain, DomainSelection, expert_keys};

        let catalogue = coding_block_catalogue();
        // The conversation-preserving propose blocks declare a shell as a
        // required fact, so the fixture declares one available.
        let facts = statement_only_facts_with_shell();
        for domain in [Domain::Django, Domain::Sympy, Domain::Sphinx] {
            let offered = nameable_gate_blocks_for_selection(
                &catalogue,
                &facts,
                DomainSelection {
                    domain,
                    arm: Arm::Composed,
                },
            );
            assert!(
                offered
                    .iter()
                    .any(|block| block.key == repair::model_propose_conversation_key())
            );
            assert!(
                offered
                    .iter()
                    .any(|block| block.key == repair::extract_patch_from_proposal_key())
            );
            for selected in expert_keys(domain) {
                assert!(
                    offered.iter().any(|block| block.key == selected),
                    "missing {selected}"
                );
            }
            for other in [Domain::Django, Domain::Sympy, Domain::Sphinx]
                .into_iter()
                .filter(|other| *other != domain)
            {
                for excluded in expert_keys(other) {
                    assert!(
                        !offered.iter().any(|block| block.key == excluded),
                        "offered {excluded} to {domain}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn composed_prompt_adds_only_its_selected_expert_notes_and_two_examples() {
        use crate::cosmos::domains::{Arm, Domain, DomainSelection, expert_notes};

        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let answer = r#"{"steps":[{"instance":"conclude","block":"conclude-single-pass"}],"grounds":"example"}"#;
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry =
            registry_with_recorder(answer, Arc::new(AtomicUsize::new(0)), Arc::clone(&prompts));
        let facts = statement_only_facts(false);
        for selection in [
            DomainSelection::default(),
            DomainSelection {
                domain: Domain::Django,
                arm: Arm::Generic,
            },
            DomainSelection {
                domain: Domain::Django,
                arm: Arm::Composed,
            },
        ] {
            model_plan_for_selection(
                selection,
                &facts,
                None,
                &catalogue,
                &target,
                Some(&registry),
                Some("scripted/model"),
                &TokenMeter::default(),
                None,
            )
            .await
            .expect("planner accepts the answer");
        }
        let sent = prompts.lock().expect("mutex is never poisoned");
        assert_eq!(
            sent[0], sent[1],
            "generic domain selection must leave the model planner's prompt byte-identical"
        );
        assert!(!sent[0].contains("Domain expert notes"));
        assert!(!sent[0].contains("django.verify-regression"));
        assert!(sent[2].contains(expert_notes(Domain::Django)));
        assert!(sent[2].contains("Worked example, generic plan:"));
        assert!(sent[2].contains("Worked example, django expert plan:"));
        assert!(sent[2].contains("django.verify-regression"));
        assert!(!sent[2].contains(expert_notes(Domain::Sympy)));
    }

    #[tokio::test]
    async fn a_flat_composed_sequence_wires_each_domains_expert_verifier() {
        use crate::cosmos::domains::{Arm, Domain, DomainSelection};
        use crate::cosmos::gate_blocks::coding_target_contracts;

        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let contracts = coding_target_contracts().expect("coding contracts");
        let proposal = r#"{"instance":"propose","block":"model-propose-conversation","config":{"model-propose":{"role":"fix","instruction":"propose a patch and a check","tools":["read_file"],"token_budget":1000}}}"#;
        for (domain, expert_steps) in [
            (
                Domain::Django,
                r#"{"instance":"verify","block":"django.verify-regression"}"#,
            ),
            (
                Domain::Sympy,
                r#"{"instance":"verify","block":"sympy.run-scoped-verification"}"#,
            ),
            (
                Domain::Sphinx,
                r#"{"instance":"find","block":"sphinx.find-mirrored-tests"},{"instance":"verify","block":"sphinx.run-check-and-mirrored-tests"}"#,
            ),
        ] {
            let answer = format!(
                "{{\"steps\":[{proposal},{{\"instance\":\"extract\",\"block\":\"extract-patch-from-proposal\"}},{{\"instance\":\"apply\",\"block\":\"apply-patch-intent\"}},{expert_steps},{{\"instance\":\"judge\",\"block\":\"judge-test-outcome\"}},{{\"instance\":\"conclude\",\"block\":\"conclude-single-pass\"}}],\"grounds\":\"domain check\"}}"
            );
            let registry = registry_with(&answer);
            // `model-propose-conversation` declares a shell as a required
            // fact, so it is only nameable once one is declared available.
            let mut facts = statement_only_facts_with_shell();
            facts.model_provider_available = true;
            let plan = model_plan_for_selection(
                DomainSelection {
                    domain,
                    arm: Arm::Composed,
                },
                &facts,
                None,
                &catalogue,
                &target,
                Some(&registry),
                Some("scripted/model"),
                &TokenMeter::default(),
                None,
            )
            .await
            .expect("flat expert plan parses");
            let graph = materialize(
                &plan,
                &catalogue,
                &CompositionLimits::conservative(),
                &contracts,
            )
            .expect("flat expert plan materializes");
            assert!(
                graph.validate().is_ok(),
                "{domain}: {:?}",
                graph.validate().errors()
            );
        }
    }

    #[tokio::test]
    async fn model_planner_cannot_put_edit_file_on_a_read_only_propose_step() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"propose","block":"model-propose",
                "config":{"model-propose":{"role":"propose","instruction":"fix it",
                "tools":["edit_file"],"token_budget":200000}}}],
                "grounds":"try to edit under the read-only block"}"#,
        );
        let error = model_plan(
            &statement_only_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &TokenMeter::default(),
            None,
        )
        .await
        .expect_err("a read-only block cannot grant the edit tool");
        assert!(
            error.to_string().contains("cannot offer edit_file"),
            "{error}"
        );
    }

    /// A statement-only task states no edits, so a model-chosen plan naming
    /// one of these would pass without writing anything. The same shape reachable
    /// through `apply-direct`/`judge-stated-edits` is covered too, since both
    /// pairs read the same nonexistent edits.
    #[tokio::test]
    async fn model_planner_refuses_a_stated_edit_block_for_a_statement_only_task() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let meter = TokenMeter::default();

        for answer in [
            r#"{"steps":[{"instance":"predict","block":"predict-edit-loop"},
                {"instance":"conclude","block":"conclude-prediction"}],
                "grounds":"model chose the prediction-error shape"}"#,
            r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                {"instance":"judge","block":"judge-stated-edits"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"model chose the direct shape"}"#,
        ] {
            let registry = registry_with(answer);
            let error = model_plan(
                &statement_only_facts(false),
                None,
                &catalogue,
                &target,
                Some(&registry),
                Some("scripted/model"),
                &meter,
                None,
            )
            .await
            .expect_err("a stated-edit-only block must be refused when none was stated");
            match error {
                PlannerError::InvalidModelAnswer { detail, .. } => {
                    assert!(
                        detail.contains("stated none"),
                        "detail should name the missing stated edits: {detail}"
                    );
                }
                other => panic!("expected an invalid-answer refusal, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn model_planner_refuses_an_unknown_top_level_field() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"}],
                "grounds":"otherwise well formed",
                "priority":"high"}"#,
        );
        let meter = TokenMeter::default();

        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("an unknown top-level field must be refused, not silently dropped");
        assert!(matches!(error, PlannerError::InvalidModelAnswer { .. }));
    }

    #[tokio::test]
    async fn model_planner_refuses_an_unknown_step_level_field() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"apply","block":"apply-direct","typo":true}],
                "grounds":"otherwise well formed"}"#,
        );
        let meter = TokenMeter::default();

        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("an unknown step-level field must be refused, not silently dropped");
        assert!(matches!(error, PlannerError::InvalidModelAnswer { .. }));
    }

    #[tokio::test]
    async fn model_planner_refuses_an_unknown_config_level_field() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"propose","block":"model-propose","config":
                {"model-propose":{"role":"revise","instruction":"fix it","tools":[],
                "token_budget":4000,"output":"patch-intent","extra":1}}}],
                "grounds":"otherwise well formed"}"#,
        );
        let meter = TokenMeter::default();

        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("an unknown config-level field must be refused, not silently dropped");
        assert!(matches!(error, PlannerError::InvalidModelAnswer { .. }));
    }

    /// Returns one answer per call, oldest first, so a test can script a
    /// model that revises its own earlier mistake.
    struct SequencedAnswerProvider {
        answers: std::sync::Mutex<std::collections::VecDeque<String>>,
        calls: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl LlmProvider for SequencedAnswerProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        async fn generate(
            &self,
            _model: &str,
            request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests
                .lock()
                .expect("test mutex")
                .push(format!("{request:?}"));
            let answer = self
                .answers
                .lock()
                .expect("test mutex")
                .pop_front()
                .expect("as many scripted answers as calls made");
            Ok(LlmResponse {
                content: vec![AssistantBlock::text(answer)],
                usage: Usage {
                    input_tokens: Some(20),
                    output_tokens: Some(10),
                    total_tokens: Some(30),
                    ..Default::default()
                },
                stop_reason: LlmStopReason::EndTurn,
                id: None,
                model: None,
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmStream, GenerationError> {
            unreachable!("model_plan tests never stream")
        }
    }

    // -----------------------------------------------------------------
    // (d.2) a billed but invalid model answer is a refusal that feeds the
    // revision loop, not a stop to the whole turn.
    // -----------------------------------------------------------------

    fn assert_refused_attempts_form_a_revision_trail(
        attempts: &[GatePlanAttemptResult],
        target: &TopologyContractKey,
    ) {
        use polaris_cosmos::contracts::{
            AdmissionAttemptId, CompositionDecision, CompositionRevisionEvidence,
            CompositionRevisionTrail, CompositionValidation, EpistemicBasis, LogicalSequence,
            RecordedAt, SelectionDecisionId, SourceKind,
        };

        let decision_id = SelectionDecisionId::from_bytes([
            0, 1, 2, 3, 4, 5, 0x40, 7, 0x80, 9, 10, 11, 12, 13, 14, 15,
        ])
        .expect("valid decision ID");
        let mut revisions = Vec::new();
        for (index, result) in attempts.iter().enumerate() {
            let GatePlanAttemptResult::Refused {
                choice,
                record,
                attempt,
                stage,
                refusals,
            } = result
            else {
                break;
            };
            let sequence = (index as u64) * 2 + 1;
            let decision = CompositionDecision::new(
                decision_id,
                RecordedAt::new(LogicalSequence::new(sequence), None),
                choice.clone(),
                vec![super::super::provenance(
                    SourceKind::CosmosPolicy,
                    EpistemicBasis::Verified,
                )],
            )
            .expect("composition decision")
            .with_gate_plan(*record, *attempt)
            .expect("gate-plan decision");
            let attempt_id = AdmissionAttemptId::from_bytes([
                1, 2, 3, 4, 5, 6, 0x40, 8, 0x80, 10, 11, 12, 13, 14, 15, 16,
            ])
            .expect("valid attempt ID");
            let validation = CompositionValidation::refused(
                attempt_id,
                &decision,
                RecordedAt::new(LogicalSequence::new(sequence + 1), None),
                NativeGraphSignatureWitness::new(target.clone(), ContentDigest::sha256(&[])),
                stage.clone(),
                ExclusionCode::new(refusals[0].code()).expect("valid refusal code"),
                refusals.clone(),
            )
            .expect("refused validation");
            revisions.push(
                CompositionRevisionEvidence::new(decision, validation)
                    .expect("refused revision is valid evidence"),
            );
        }
        CompositionRevisionTrail::new(revisions, None, None)
            .expect("refused attempts form a valid revision trail");
    }

    #[tokio::test]
    async fn an_invalid_model_answer_revises_into_an_admitted_plan() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(SequencedAnswerProvider {
            answers: std::sync::Mutex::new(
                vec![
                    "not json at all".to_owned(),
                    r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                        {"instance":"judge","block":"judge-stated-edits"},
                        {"instance":"conclude","block":"conclude-single-pass"}],
                        "grounds":"revised after the invalid first answer"}"#
                        .to_owned(),
                ]
                .into(),
            ),
            calls: Arc::clone(&calls),
            requests: Arc::clone(&requests),
        });
        let meter = TokenMeter::default();
        let mut chooser_budget = 0;

        let attempts = attempt_gate_plan(
            GatePlannerMode::Model,
            crate::cosmos::domains::DomainSelection::default(),
            &edit_facts(false),
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            &mut chooser_budget,
            None,
        )
        .await
        .expect("the revision loop runs");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the invalid answer costs one real call, the revision a second"
        );
        assert_eq!(
            attempts.len(),
            2,
            "one refused invalid answer, then the admitted revision"
        );

        let GatePlanAttemptResult::Refused {
            attempt,
            record,
            refusals,
            ..
        } = &attempts[0]
        else {
            panic!("an unparseable first answer must be refused, not stop the whole turn");
        };
        assert_eq!(attempt.get(), 1);
        assert_eq!(record.catalogue_digest(), catalogue.digest());
        assert_eq!(
            record.shape(),
            GateExpressionShapeKey::new(ContentDigest::sha256(
                b"invalid-model-answer;no-gate-plan"
            )),
            "an answer that never became a plan carries the reserved refusal shape"
        );
        assert!(
            refusals
                .iter()
                .any(|refusal| refusal.code() == "planner.invalid-model-answer"),
            "the refusal names the invalid-model-answer code"
        );

        let GatePlanAttemptResult::Materialized { attempt, graph, .. } = &attempts[1] else {
            panic!("the revised, well-formed plan must have materialised");
        };
        assert_eq!(
            attempt.get(),
            2,
            "the admitted plan is the second, revised attempt"
        );
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        assert!(
            requests.lock().expect("test mutex")[1].contains(INVALID_MODEL_ANSWER_CODE),
            "the second request receives the first refusal"
        );
        assert_refused_attempts_form_a_revision_trail(&attempts, &target);
    }

    #[tokio::test]
    async fn every_unresolvable_model_answer_exhausts_the_revision_loop_with_valid_evidence() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(SequencedAnswerProvider {
            answers: Mutex::new(
                vec![
                    "not json at all".to_owned(),
                    r#"{"steps":[],"grounds":"empty"}"#.to_owned(),
                    r#"{"steps":[{"instance":"x","block":"unknown-block"}],"grounds":"unknown"}"#
                        .to_owned(),
                ]
                .into(),
            ),
            calls: Arc::clone(&calls),
            requests: Arc::clone(&requests),
        });
        let mut chooser_budget = 0;
        let attempts = attempt_gate_plan(
            GatePlannerMode::Model,
            crate::cosmos::domains::DomainSelection::default(),
            &edit_facts(false),
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &TokenMeter::default(),
            &mut chooser_budget,
            None,
        )
        .await
        .expect("invalid answers must exhaust normally, leaving the caller to fall back");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_GATE_PLAN_ATTEMPTS as usize
        );
        assert_eq!(attempts.len(), MAX_GATE_PLAN_ATTEMPTS as usize);
        assert!(
            attempts
                .iter()
                .all(|result| matches!(result, GatePlanAttemptResult::Refused { .. }))
        );
        let requests = requests.lock().expect("test mutex");
        assert!(requests[1].contains(INVALID_MODEL_ANSWER_CODE));
        assert!(requests[2].contains("plan names no steps"));
        assert_refused_attempts_form_a_revision_trail(&attempts, &target);
    }

    // -----------------------------------------------------------------
    // (d.1) the chooser-call budget is shared and debited before every
    // real model request, revisions included.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn the_shared_chooser_budget_stops_the_revision_loop_mid_turn() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        // A single `apply-direct` step parses into a well-formed plan but
        // never produces the target contract's `AssessmentConclusion`, so
        // it is refused by `materialize` itself, not by `model_plan` — a
        // materializer refusal, distinct from an invalid model answer.
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = registry_with_counter(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"}],
                "grounds":"a plan with no concluding step"}"#,
            Arc::clone(&calls),
        );
        // Simulates a budget already spent down to its last remaining call
        // by an earlier candidate this same turn.
        let mut chooser_budget = composition::MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN - 1;

        let attempts = attempt_gate_plan(
            GatePlannerMode::Model,
            crate::cosmos::domains::DomainSelection::default(),
            &edit_facts(false),
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &TokenMeter::default(),
            &mut chooser_budget,
            None,
        )
        .await
        .expect("the revision loop runs");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the one call the budget still affords is ever made"
        );
        assert_eq!(
            chooser_budget,
            composition::MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN,
            "the shared budget is debited to exactly its ceiling, never past it"
        );

        assert_eq!(
            attempts.len(),
            1,
            "only the one affordable attempt is recorded"
        );
        assert!(
            matches!(attempts[0], GatePlanAttemptResult::Refused { .. }),
            "a plan with no concluding step must never materialise"
        );
    }

    // -----------------------------------------------------------------
    // A task's menu hides every block whose catalogue-declared
    // required facts it does not meet, and naming one anyway is refused like
    // any other unknown block.
    // -----------------------------------------------------------------

    #[test]
    fn nameable_gate_blocks_hides_stated_edit_only_blocks_when_no_edits_are_stated() {
        let catalogue = coding_block_catalogue();
        let with_edits = nameable_gate_blocks(&catalogue, &edit_facts(false));
        let without_edits = nameable_gate_blocks(&catalogue, &statement_only_facts(false));
        for hidden in [
            "apply-direct",
            "judge-stated-edits",
            "predict-edit-loop",
            "conclude-prediction",
        ] {
            assert!(
                with_edits.iter().any(|block| block.name == hidden),
                "{hidden} must be nameable when the task states its own edits"
            );
            assert!(
                !without_edits.iter().any(|block| block.name == hidden),
                "{hidden} must be hidden when the task states no edits"
            );
        }
    }

    #[test]
    fn nameable_gate_blocks_hides_blocks_whose_other_required_facts_are_unmet() {
        let catalogue = coding_block_catalogue();

        let mut no_model = edit_facts(false);
        no_model.model_provider_available = false;
        assert!(
            !nameable_gate_blocks(&catalogue, &no_model)
                .iter()
                .any(|block| block.name == "model-propose"),
            "model-propose must be hidden when no model provider is declared available"
        );
        let mut with_model = no_model.clone();
        with_model.model_provider_available = true;
        assert!(
            nameable_gate_blocks(&catalogue, &with_model)
                .iter()
                .any(|block| block.name == "model-propose"),
            "model-propose must be nameable once a model provider is declared available"
        );

        assert!(
            !nameable_gate_blocks(&catalogue, &edit_facts(false))
                .iter()
                .any(|block| block.name == "run-project-tests-captured"),
            "run-project-tests-captured must be hidden when no test runner is declared available"
        );
        assert!(
            nameable_gate_blocks(&catalogue, &edit_facts(true))
                .iter()
                .any(|block| block.name == "run-project-tests-captured"),
            "run-project-tests-captured must be nameable once a test runner is declared available"
        );
    }

    #[tokio::test]
    async fn model_planner_refuses_a_hidden_block_named_when_no_edits_are_stated() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"}],
                "grounds":"a model that ignored has_stated_edits"}"#,
        );
        let meter = TokenMeter::default();

        let error = model_plan(
            &statement_only_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("apply-direct must be refused for a task that stated no edits");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("unknown block"), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // The four worked examples in the model prompt each parse
    // and materialize against the real coding catalogue, and run to a
    // concluded coding contract.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn worked_example_direct_edit_parses_materialises_and_runs() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::new(
            "worked-example-direct-edit",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1 + 1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let registry = registry_with(WORKED_EXAMPLE_DIRECT_EDIT);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the worked direct-edit example is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the worked direct-edit example materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the worked direct-edit example executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert_eq!(workspace.read("lib.rs").expect("read back"), "1 + 1");
    }

    #[tokio::test]
    async fn worked_example_test_gated_repair_parses_materialises_and_runs() {
        use std::collections::VecDeque;
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use polaris::system::system::SystemError;

        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepExecutor, ModelStepProvider, WorkspaceView,
        };
        use crate::cosmos::payload::ProjectTestRunner;
        use crate::cosmos::task::ExactEdit;

        struct RecordingModelStep {
            responses: Mutex<VecDeque<ModelStepArtifact>>,
            calls: AtomicUsize,
        }

        impl ModelStepExecutor for RecordingModelStep {
            fn propose<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                _meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                Box::pin(async move {
                    let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
                    if call_index == 1 {
                        assert!(
                            config.instruction.contains("MISSING_THE_FIX"),
                            "the retry must read the previous attempt's captured test \
                             output: {}",
                            config.instruction
                        );
                    }
                    Ok(self
                        .responses
                        .lock()
                        .expect("mutex is never poisoned")
                        .pop_front()
                        .expect("a scripted response is queued for every call"))
                })
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        let task = CodingTask::from_statement(
            "worked-example-test-gated-repair",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let registry = registry_with(WORKED_EXAMPLE_TEST_GATED_REPAIR);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &statement_only_facts(true),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the worked test-gated repair example is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the worked test-gated repair example materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let executor = RecordingModelStep {
            responses: Mutex::new(VecDeque::from(vec![
                ModelStepArtifact::PatchIntent {
                    edits: vec![
                        ExactEdit::new("lib.rs", "fn add", "fn add /* attempt 1 */")
                            .expect("well-formed edit"),
                    ],
                    check: None,
                },
                ModelStepArtifact::PatchIntent {
                    edits: vec![
                        ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit"),
                    ],
                    check: None,
                },
            ])),
            calls: AtomicUsize::new(0),
        };

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(ProjectTestRunner::command(
            "sh",
            [
                "-c",
                "grep -q 'a + b' lib.rs || { echo MISSING_THE_FIX >&2; exit 1; }",
            ],
        ));
        context.insert(ModelStepProvider::new(executor));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the worked test-gated repair example executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("a + b"),
            "the repair arm's proposed patch must have landed"
        );
    }

    // -----------------------------------------------------------------
    // Integration smoke test: a bug-fix task with no stated edits
    // and a real test runner available, driven end to end by a stub LLM —
    // no network, no `ANTHROPIC_API_KEY` use anywhere. Prints the full
    // prompt sent to the stub model and the accepted plan's rendered shape,
    // so both can be captured for the record.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn unstated_edit_smoke_test_prints_the_sent_prompt_and_accepted_plan() {
        use std::collections::VecDeque;
        use std::future::Future;
        use std::pin::Pin;

        use polaris::system::system::SystemError;

        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepExecutor, ModelStepProvider, WorkspaceView,
        };
        use crate::cosmos::payload::ProjectTestRunner;
        use crate::cosmos::task::ExactEdit;

        struct RecordingModelStep {
            responses: Mutex<VecDeque<ModelStepArtifact>>,
            calls: AtomicUsize,
        }

        impl ModelStepExecutor for RecordingModelStep {
            fn propose<'a>(
                &'a self,
                _config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                _meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                Box::pin(async move {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    Ok(self
                        .responses
                        .lock()
                        .expect("mutex is never poisoned")
                        .pop_front()
                        .expect("a scripted response is queued for every call"))
                })
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        let task = CodingTask::from_statement(
            "unstated-edit-smoke",
            dir.path(),
            "tests/test_arithmetic.py::test_add fails: add(2, 3) returns -1 instead of 5 — \
             fix add() so the test passes",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let facts = PlannerFacts {
            statement: "tests/test_arithmetic.py::test_add fails: add(2, 3) returns -1 \
                        instead of 5 — fix add() so the test passes"
                .to_owned(),
            ..statement_only_facts(true)
        };
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with_recorder(
            WORKED_EXAMPLE_TEST_GATED_REPAIR,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&prompts),
        );
        let meter = TokenMeter::default();
        let plan = model_plan(
            &facts,
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the stub model's plan is accepted");

        let sent_prompt = prompts
            .lock()
            .expect("mutex is never poisoned")
            .first()
            .cloned()
            .expect("one request was sent");
        let shape = render_gate_expression_shape(plan.expression());

        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the accepted plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let executor = RecordingModelStep {
            responses: Mutex::new(VecDeque::from(vec![
                ModelStepArtifact::PatchIntent {
                    edits: vec![
                        ExactEdit::new("lib.rs", "fn add", "fn add /* attempt 1 */")
                            .expect("well-formed edit"),
                    ],
                    check: None,
                },
                ModelStepArtifact::PatchIntent {
                    edits: vec![
                        ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit"),
                    ],
                    check: None,
                },
            ])),
            calls: AtomicUsize::new(0),
        };

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(ProjectTestRunner::command(
            "sh",
            [
                "-c",
                "grep -q 'a + b' lib.rs || { echo MISSING_THE_FIX >&2; exit 1; }",
            ],
        ));
        context.insert(ModelStepProvider::new(executor));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the accepted plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert!(
            workspace
                .read("lib.rs")
                .expect("read back")
                .contains("a + b"),
            "the repair arm's proposed patch must have landed"
        );

        println!(
            "=== integration smoke test: accepted plan shape ===\n{shape}\n\n\
             === integration smoke test: full prompt sent to the stub model ===\n{sent_prompt}"
        );
    }

    #[tokio::test]
    async fn worked_example_investigate_then_patch_parses_materialises_and_runs() {
        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepProvider, ScriptedModelStep,
        };
        use crate::cosmos::task::ExactEdit;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("seed file");
        let task = CodingTask::from_statement(
            "worked-example-investigate-then-patch",
            dir.path(),
            "add() subtracts instead of adding",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let registry = registry_with(WORKED_EXAMPLE_INVESTIGATE_THEN_PATCH);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &statement_only_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the worked investigate-then-patch example is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the worked investigate-then-patch example materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![ExactEdit::new("lib.rs", "a - b", "a + b").expect("well-formed edit")],
                check: None,
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the worked investigate-then-patch example executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert_eq!(
            workspace.read("lib.rs").expect("read back"),
            "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"
        );
    }

    #[tokio::test]
    async fn worked_example_question_parses_materialises_and_runs() {
        use crate::cosmos::payload::CodingQuestion;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::from_statement(
            "worked-example-question",
            dir.path(),
            "where does todo!() occur",
            CodingIntent::Answer,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed question task")
        .with_question("where is todo!()");
        let workspace = CodingWorkspace::for_task(&task);

        let registry = registry_with(WORKED_EXAMPLE_QUESTION);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &question_facts(),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the worked question example is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the worked question example materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(CodingQuestion::locate("todo!()"));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the worked question example executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
        assert_eq!(workspace.read("lib.rs").expect("read back"), "todo!()");
    }

    // -----------------------------------------------------------------
    // Fork, Loop, and Repeat each parse and materialize against
    // the real coding catalogue beyond the worked examples above, which only
    // exercise Seq and Choose; Switch parses correctly too, but nothing it
    // could name is usable yet (see the test and doc below).
    // -----------------------------------------------------------------

    /// `switch` parses correctly (a real [`GateExpr::Switch`] structural
    /// test), but every discriminator name is refused today: see
    /// [`nameable_discriminators`]'s own doc for why neither of the two
    /// [`predicates`] registers can currently back one. This proves the
    /// refusal is exactly that clean "unknown discriminator" message, not a
    /// parse failure or a mysterious materializer error, for a name a model
    /// might plausibly guess exists.
    #[tokio::test]
    async fn model_planner_refuses_a_switch_naming_any_discriminator_today() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let answer = r#"{"plan":{"kind":"switch","discriminator":"task-kind","cases":[
            {"case":"question","body":{"kind":"step","instance":"a","block":"survey-tree"}}
        ],"default":{"kind":"step","instance":"b","block":"apply-direct"}},
        "grounds":"route on task kind"}"#;
        let registry = registry_with(answer);
        let meter = TokenMeter::default();
        let error = model_plan(
            &question_facts(),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("no discriminator is currently exposed, so this must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("unknown discriminator"), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    /// Each `Fork` arm is its own self-contained subgraph: unlike `Seq`,
    /// `Choose`, `Switch`, and `Loop` (which the native builder flattens
    /// together with the rest of the plan, see
    /// [`polaris_cosmos::composition::materializer`]'s own
    /// `recursive_node_count` doc), a value produced *outside* a `Fork` is
    /// not visible to a read *inside* one of its arms, so each arm here
    /// produces its own input. Every arm runs the project's
    /// tests and judges the outcome itself, entirely read-only, so running
    /// two arms concurrently is safe and needs nothing from outside the
    /// `Fork`.
    #[tokio::test]
    async fn model_tree_fork_reduces_two_judged_test_outcomes() {
        use crate::cosmos::payload::ProjectTestRunner;

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::from_statement(
            "model-tree-fork",
            dir.path(),
            "fix the bug",
            CodingIntent::Careful,
            SuccessCheck::command("true", Vec::<String>::new()).expect("well-formed command"),
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let answer = r#"{"plan":{"kind":"seq","steps":[
            {"kind":"fork","arms":[
                {"kind":"seq","steps":[
                    {"kind":"step","instance":"run-a","block":"run-project-tests-captured"},
                    {"kind":"step","instance":"judge-a","block":"judge-test-outcome"}
                ]},
                {"kind":"seq","steps":[
                    {"kind":"step","instance":"run-b","block":"run-project-tests-captured"},
                    {"kind":"step","instance":"judge-b","block":"judge-test-outcome"}
                ]}
            ],"reducer":{"instance":"reduce","block":"reduce-assessments"}},
            {"kind":"step","instance":"conclude","block":"conclude-single-pass"}
        ]},"grounds":"run the tests twice, independently, reduced to the worst verdict"}"#;
        let registry = registry_with(answer);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(true),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a fork with a reduce-assessments reducer is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the fork plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(ProjectTestRunner::command("true", Vec::<String>::new()));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the fork plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    }

    #[tokio::test]
    async fn model_tree_loop_with_fixed_count_runs_its_body() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::new(
            "model-tree-loop-count",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1 + 1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let answer = r#"{"plan":{"kind":"seq","steps":[
            {"kind":"step","instance":"apply","block":"apply-direct"},
            {"kind":"step","instance":"judge","block":"judge-stated-edits"},
            {"kind":"loop","body":{"kind":"step","instance":"affirm","block":"affirm-assessment"},"count":1},
            {"kind":"step","instance":"conclude","block":"conclude-single-pass"}
        ]},"grounds":"re-affirm the judged assessment once, then conclude"}"#;
        let registry = registry_with(answer);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a fixed-count loop is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the fixed-count loop plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the fixed-count loop plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    }

    #[tokio::test]
    async fn model_tree_loop_until_tests_passed_runs_zero_iterations_when_already_satisfied() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::new(
            "model-tree-loop-until",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1 + 1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let answer = r#"{"plan":{"kind":"seq","steps":[
            {"kind":"step","instance":"apply","block":"apply-direct"},
            {"kind":"step","instance":"judge","block":"judge-stated-edits"},
            {"kind":"loop","body":{"kind":"step","instance":"affirm","block":"affirm-assessment"},"until":"tests-passed"},
            {"kind":"step","instance":"conclude","block":"conclude-single-pass"}
        ]},"grounds":"loop until the judged assessment passes; it already does, so the body need not run"}"#;
        let registry = registry_with(answer);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("an until-predicate loop is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the until-predicate loop plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the until-predicate loop plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    }

    #[tokio::test]
    async fn model_tree_repeat_with_count_zero_runs_its_body_exactly_once() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::new(
            "model-tree-repeat",
            dir.path(),
            "lib.rs",
            "todo!()",
            "1 + 1",
            "todo!()",
            CodingIntent::Careful,
        )
        .expect("well-formed task");
        let workspace = CodingWorkspace::for_task(&task);

        let answer = r#"{"plan":{"kind":"seq","steps":[
            {"kind":"step","instance":"apply","block":"apply-direct"},
            {"kind":"step","instance":"judge","block":"judge-stated-edits"},
            {"kind":"repeat","body":{"kind":"step","instance":"affirm","block":"affirm-assessment"},"count":0},
            {"kind":"step","instance":"conclude","block":"conclude-single-pass"}
        ]},"grounds":"repeat guarantees the affirm step runs exactly once"}"#;
        let registry = registry_with(answer);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a repeat with count zero is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the repeat plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the repeat plan executes");
        let conclusion = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
        assert_eq!(conclusion.stop_reason(), StopReason::GoalMet);
    }

    // -----------------------------------------------------------------
    // Structural mistakes in the tree grammar come back
    // through the same invalid-model-answer refusal as any flat-grammar
    // mistake, with a precise message.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn model_planner_refuses_an_answer_naming_both_steps_and_plan() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"}],
                "plan":{"kind":"step","instance":"apply","block":"apply-direct"},
                "grounds":"names both shapes"}"#,
        );
        let meter = TokenMeter::default();
        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("naming both steps and plan must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("both 'steps' and 'plan'"), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_planner_refuses_an_answer_naming_neither_steps_nor_plan() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(r#"{"grounds":"names neither shape"}"#);
        let meter = TokenMeter::default();
        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("naming neither steps nor plan must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("neither 'steps' nor 'plan'"), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_planner_refuses_a_choose_naming_an_unknown_predicate() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"plan":{"kind":"choose","predicate":"made-up-predicate",
                "when_true":{"kind":"step","instance":"a","block":"apply-direct"},
                "when_false":{"kind":"step","instance":"b","block":"apply-direct"}},
                "grounds":"names an unregistered predicate"}"#,
        );
        let meter = TokenMeter::default();
        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("an unknown predicate must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("unknown predicate"), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    // An "unknown case" test (a case label outside a named discriminator's
    // own closed set) is not constructible right now: every discriminator
    // name is refused before case validation ever runs, since
    // `nameable_discriminators` is empty (see its own doc for why). The
    // `build_expr_answer` check itself is exercised by ordinary code review
    // rather than an end-to-end test until a real discriminator is exposed.

    #[tokio::test]
    async fn model_planner_refuses_a_loop_naming_both_until_and_count() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"plan":{"kind":"loop","body":{"kind":"step","instance":"a","block":"apply-direct"},
                "until":"tests-passed","count":1},
                "grounds":"names both loop controls"}"#,
        );
        let meter = TokenMeter::default();
        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("naming both until and count must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("both 'until' and 'count'"), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_planner_refuses_a_loop_naming_neither_until_nor_count() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"plan":{"kind":"loop","body":{"kind":"step","instance":"a","block":"apply-direct"}},
                "grounds":"names neither loop control"}"#,
        );
        let meter = TokenMeter::default();
        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("naming neither until nor count must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("neither 'until' nor 'count'"), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Model-propose's settings (output kind, tool subset,
    // role/instruction, max_turns, sees_prior_failure) are chosen per
    // instance by the model, reach the runtime configuration unchanged, and
    // a downstream block that cannot consume the chosen output kind is
    // refused clearly through the unchanged materializer/native-validation
    // path.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn model_planner_lets_the_model_set_model_propose_settings_per_instance() {
        use std::future::Future;
        use std::pin::Pin;

        use polaris::system::system::SystemError;

        use crate::cosmos::gate_blocks::configurable::{
            ModelStepArtifact, ModelStepExecutor, ModelStepProvider, WorkspaceView,
        };
        use crate::cosmos::payload::ProjectTestRunner;

        struct AssertingModelStep;

        impl ModelStepExecutor for AssertingModelStep {
            fn propose<'a>(
                &'a self,
                config: &'a ModelStepConfig,
                _workspace: &'a WorkspaceView<'a>,
                _meter: &'a TokenMeter,
            ) -> Pin<Box<dyn Future<Output = Result<ModelStepArtifact, SystemError>> + Send + 'a>>
            {
                Box::pin(async move {
                    assert_eq!(config.role, "custom-role");
                    assert!(
                        config.instruction.contains("do the thing"),
                        "{}",
                        config.instruction
                    );
                    assert_eq!(config.max_turns, 7);
                    assert!(config.sees_prior_failure);
                    assert!(matches!(
                        &config.tool_subset,
                        ToolSubset::Only(tools) if tools == &vec!["read_file".to_owned()]
                    ));
                    Ok(ModelStepArtifact::PatchIntent {
                        edits: Vec::new(),
                        check: None,
                    })
                })
            }
        }

        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let dir = tempfile::tempdir().expect("temp project");
        std::fs::write(dir.path().join("lib.rs"), "todo!()").expect("seed file");
        let task = CodingTask::from_statement(
            "model-propose-settings",
            dir.path(),
            "fix the bug",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("well-formed statement-only task");
        let workspace = CodingWorkspace::for_task(&task);

        let answer = r#"{"steps":[
            {"instance":"run-tests","block":"run-project-tests-captured"},
            {"instance":"propose","block":"model-propose","config":{"model-propose":{
                "role":"custom-role","instruction":"do the thing","tools":["read_file"],
                "token_budget":1234,"output":"patch-intent","max_turns":7,
                "sees_prior_failure":true}}},
            {"instance":"apply","block":"apply-patch-intent"},
            {"instance":"judge","block":"judge-patch-intent"},
            {"instance":"conclude","block":"conclude-single-pass"}
        ],"grounds":"custom model-propose settings"}"#;
        let registry = registry_with(answer);
        let meter = TokenMeter::default();
        let plan = model_plan(
            &statement_only_facts(true),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("custom model-propose settings are accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the plan materialises");

        let mut context = SystemContext::new();
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(configurable::ModelStepHandoff::default());
        context.insert(ProjectTestRunner::command("true", Vec::<String>::new()));
        context.insert(ModelStepProvider::new(AssertingModelStep));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("the plan executes, running the asserting executor");
        let _ = execution
            .output::<AssessmentConclusion>()
            .expect("concludes the coding contract");
    }

    /// The maximum token budget itself is accepted and materialises — the
    /// ceiling refuses anything *over* it, not the boundary value.
    #[tokio::test]
    async fn model_planner_accepts_the_maximum_token_budget() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let registry = registry_with(&format!(
            r#"{{"steps":[{{"instance":"propose","block":"model-propose","config":
                {{"model-propose":{{"role":"revise","instruction":"fix it","tools":[],
                "token_budget":{},"output":"patch-intent"}}}}}},
                {{"instance":"apply","block":"apply-patch-intent"}},
                {{"instance":"judge","block":"judge-patch-intent"}},
                {{"instance":"conclude","block":"conclude-single-pass"}}],
                "grounds":"the maximum token budget itself"}}"#,
            configurable::MAX_MODEL_STEP_TOKEN_BUDGET
        ));
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the maximum token budget is accepted");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("a plan at the maximum token budget materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    /// One token over the maximum is refused by the planner itself, before
    /// materialization ever runs — unlike `max_turns`, whose out-of-range
    /// value is only caught once the block is later built (see
    /// `model_planner_out_of_range_max_turns_is_refused_by_materialize`
    /// below), because an unbounded `token_budget` is a provider bill, not
    /// just a wasted turn.
    #[tokio::test]
    async fn model_planner_refuses_a_token_budget_over_the_maximum() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(&format!(
            r#"{{"steps":[{{"instance":"propose","block":"model-propose","config":
                {{"model-propose":{{"role":"revise","instruction":"fix it","tools":[],
                "token_budget":{},"output":"patch-intent"}}}}}}],
                "grounds":"one token over the maximum"}}"#,
            configurable::MAX_MODEL_STEP_TOKEN_BUDGET + 1
        ));
        let meter = TokenMeter::default();
        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("a token_budget over the maximum must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(detail.contains("token_budget must be in 1..="), "{detail}");
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_planner_refuses_an_unknown_model_propose_output_kind() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[{"instance":"propose","block":"model-propose","config":
                {"model-propose":{"role":"revise","instruction":"fix it","tools":[],
                "token_budget":4000,"output":"bogus-kind"}}}],
                "grounds":"otherwise well formed"}"#,
        );
        let meter = TokenMeter::default();
        let error = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect_err("an unknown output kind must be refused");
        match error {
            PlannerError::InvalidModelAnswer { detail, .. } => {
                assert!(
                    detail.contains("unknown model-propose output kind"),
                    "{detail}"
                );
            }
            other => panic!("expected an invalid-answer refusal, got {other:?}"),
        }
    }

    /// `apply-patch-intent` needs a `patch-intent` output; a model that
    /// instead picks `review` for the preceding `model-propose` step gets a
    /// clear refusal, not silent misbehaviour. The effects/materializer path
    /// decides this the same way it always has — nothing here bypasses or
    /// duplicates that check (see [`build_step_answer`]'s own doc) — so the
    /// exact refusal code this lands on is whichever native graph validation
    /// already gives an absent/mismatched `Out<PatchIntent>` read; this test
    /// only pins that *some* refusal fires and that it names `PatchIntent`,
    /// not a specific code, since that mapping is `polaris_cosmos`'s to
    /// choose.
    #[tokio::test]
    async fn model_planner_output_kind_mismatch_is_refused_by_materialize() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        let registry = registry_with(
            r#"{"steps":[
                {"instance":"propose","block":"model-propose","config":{"model-propose":{
                    "role":"revise","instruction":"review something","tools":[],
                    "token_budget":500,"output":"review"}}},
                {"instance":"apply","block":"apply-patch-intent"},
                {"instance":"judge","block":"judge-patch-intent"},
                {"instance":"conclude","block":"conclude-single-pass"}
            ],"grounds":"a model-propose output the next step cannot consume"}"#,
        );
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the answer is well-formed JSON even though it will not materialise");
        let refusals = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect_err("apply-patch-intent must refuse a model-propose output kind it cannot consume");
        assert!(
            refusals
                .iter()
                .any(|refusal| refusal.actual().contains("PatchIntent")
                    || refusal.expected().contains("PatchIntent")),
            "{refusals:?}"
        );
    }

    #[tokio::test]
    async fn model_planner_out_of_range_max_turns_is_refused_by_materialize() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract catalog");
        let target = target_contract();
        // One over the block's own ceiling, derived rather than spelled out
        // so the fixture cannot fall back inside a raised ceiling.
        let registry = registry_with(&format!(
            r#"{{"steps":[
                {{"instance":"propose","block":"model-propose","config":{{"model-propose":{{
                    "role":"revise","instruction":"fix it","tools":[],
                    "token_budget":500,"output":"patch-intent","max_turns":{}}}}}}}
            ],"grounds":"a max_turns value above the block's own ceiling"}}"#,
            configurable::MAX_MODEL_STEP_TURNS_CEILING + 1
        ));
        let meter = TokenMeter::default();
        let plan = model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("the answer encodes even though the value is out of range");
        let refusals = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect_err("an out-of-range max_turns must be refused when the step is built");
        assert!(
            refusals.iter().any(|refusal| {
                refusal.code() == polaris_cosmos::composition::refusal::codes::BAD_CONFIG
            }),
            "{refusals:?}"
        );
    }

    // -----------------------------------------------------------------
    // The optional history hook.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn model_planner_prompt_includes_history_when_given() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with_recorder(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                {"instance":"judge","block":"judge-stated-edits"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"g"}"#,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&prompts),
        );
        let meter = TokenMeter::default();

        model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            Some("direct-edit: used 12 times, 3 resolved"),
        )
        .await
        .expect("a well-formed answer is accepted");

        let sent = prompts
            .lock()
            .expect("mutex is never poisoned")
            .first()
            .cloned()
            .expect("one request was sent");
        assert!(
            sent.contains("direct-edit: used 12 times, 3 resolved"),
            "the prompt must carry the given history text: {sent}"
        );
    }

    #[tokio::test]
    async fn model_planner_prompt_omits_history_section_when_absent() {
        let catalogue = coding_block_catalogue();
        let target = target_contract();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with_recorder(
            r#"{"steps":[{"instance":"apply","block":"apply-direct"},
                {"instance":"judge","block":"judge-stated-edits"},
                {"instance":"conclude","block":"conclude-single-pass"}],
                "grounds":"g"}"#,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&prompts),
        );
        let meter = TokenMeter::default();

        model_plan(
            &edit_facts(false),
            None,
            &catalogue,
            &target,
            Some(&registry),
            Some("scripted/model"),
            &meter,
            None,
        )
        .await
        .expect("a well-formed answer is accepted");

        let sent = prompts
            .lock()
            .expect("mutex is never poisoned")
            .first()
            .cloned()
            .expect("one request was sent");
        assert!(
            !sent.contains("Past plan shapes"),
            "no history section should appear when none is given: {sent}"
        );
    }

    // -----------------------------------------------------------------
    // (e) policy parsing.
    // -----------------------------------------------------------------

    #[test]
    fn gate_planner_mode_parses_its_two_spellings_and_rejects_others() {
        assert!(matches!(
            GatePlannerMode::parse("scripted"),
            Ok(GatePlannerMode::Scripted)
        ));
        assert!(matches!(
            GatePlannerMode::parse("model"),
            Ok(GatePlannerMode::Model)
        ));
        assert!(GatePlannerMode::parse("chaotic-random").is_err());
    }

    #[test]
    fn composition_policy_parses_the_gates_spelling() {
        assert!(matches!(
            composition::CompositionPolicy::parse("gates"),
            Ok(composition::CompositionPolicy::Gates)
        ));
        assert_eq!(
            composition::CompositionPolicy::Gates.as_str(),
            composition::GATES_POLICY
        );
    }
    include!("planner/model_grammar_tests.rs");
}
