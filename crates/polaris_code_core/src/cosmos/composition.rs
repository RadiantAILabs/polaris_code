//! Finite, deterministic composition recipes for coding topology runs.
//!
//! The sealed topology catalog still selects and admits coding work.  This
//! module is deliberately narrower: once that selection names an approved
//! coding shape, it resolves the matching finite Cosmos-block recipe and builds
//! a *fresh* native graph from the embodiment's existing stage systems.  The
//! graph is validated before it reaches the one-start host path; it is never a
//! serialized graph or a second executor.

use std::num::NonZeroU32;

use polaris::graph::Graph;
use polaris::models::ModelRegistry;
use polaris::models::llm::{LlmRequest, Message};
use polaris::system::resource::GlobalResource;
use polaris_cosmos::contracts::{
    CompositionBlockId, CompositionBlockKey, CompositionBlockRef, CompositionCatalogId,
    CompositionCatalogKey, CompositionChoice, CompositionChoiceOrigin, CompositionDecision,
    CompositionRecipeId, CompositionRecipeKey, CompositionRecipeRef, CompositionValidation,
    CompositionValidationDisposition, ConfigurationSchemaId, ConfigurationSchemaKey,
    ConfigurationValue, ContentDigest, NativeGraphSignatureWitness, RecordedAt,
    ResolvedConfigurationRef, SelectionDecisionId, SourceKind,
};
use polaris_cosmos::topology::TopologyContractKey;

use super::graphs;
use super::topology::{
    COVERAGE_AUDIT, DIRECT, MODEL_PROPOSED, PREDICTION_ERROR, TEST_GATED, TREE_QUESTION,
    WORKLIST_SWEEP,
};
use super::{behavior_version, policy, provenance};
use crate::usage::TokenMeter;
use serde::{Deserialize, Serialize};

/// Stable identity of this embodiment's sealed finite recipe catalogue.
pub const CATALOG: &str = "polaris-code.coding-compositions";

/// Policy identity for the deterministic fact-driven recipe projection.
pub const POLICY: &str = "polaris-code.composition-registration-projection";

/// Stable identity for the separately selectable model composition policy.
pub const MODEL_POLICY: &str = "polaris-code.composition-model-chooser";

/// Stable identity for the gate-plan composition policy.
pub const GATES_POLICY: &str = "polaris-code.composition-gate-plan";

/// How many real per-turn calls the model composition policy may make before
/// [`super::host::select_and_run`] stops trying further fallback candidates
/// and ends the turn in a stated refusal.
///
/// The shipped catalog registers up to seven coding topologies, so an
/// unbounded fallback walk under [`CompositionPolicy::Model`] could make up
/// to seven real, billed model calls for one accepted task — every one of
/// them a routine "top candidate's plan was refused, try the next" turn, not
/// a wiring fault. Bounding it keeps a turn's worst-case chooser spend a
/// small, known multiple of one call regardless of catalog size, while still
/// leaving room for more than just the top-ranked candidate to be tried. The
/// eval harness reflects this bound in `evals/core/config.py`'s
/// `MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN`.
pub const MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN: usize = 3;

/// The output limit of one model-chooser request, sent only under a cost cap.
const CHOOSER_MAX_OUTPUT_TOKENS: NonZeroU32 = NonZeroU32::new(4096).unwrap();
const CONFIGURATION_SCHEMA: &str = "polaris-code.composition-configuration";
const CATALOGUE_CONTENTS: &[u8] = b"polaris-code.recipe.direct-verified:cosmos.verified-step,cosmos.resolve-residuals\npolaris-code.recipe.prediction-error:cosmos.prediction-error-loop\npolaris-code.recipe.bounded-worklist:cosmos.drain-worklist,cosmos.resolve-residuals\npolaris-code.recipe.test-gated:cosmos.verified-step,cosmos.resolve-residuals\npolaris-code.recipe.model-proposed:cosmos.verified-step,cosmos.resolve-residuals\npolaris-code.recipe.coverage-sweep:cosmos.coverage-sweep\npolaris-code.recipe.tree-question:polaris-code.tree-survey\n";

/// A statically registered coding embodiment for one finite composition.
///
/// The builder is deliberately a function pointer rather than a graph value:
/// a selected turn receives a newly built native graph, while the recipe is
/// still finite, inspectable data.  Application packs that add a coding
/// registration outside this crate can place a matching template in
/// [`CompositionTemplates`] before server assembly.
#[derive(Debug, Copy, Clone)]
pub struct CompositionTemplate {
    recipe: &'static str,
    blocks: &'static [&'static str],
    build: fn() -> Graph,
}

/// The explicitly selected policy that chooses an approved composition.
///
/// Absence of this global resource means [`Deterministic`] is used unless the
/// host's explicit `POLARIS_CODE_COMPOSITION_POLICY=model` configuration asks
/// for [`Model`]. There is no provider-triggered or post-start substitution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompositionPolicy {
    /// The default, no-model policy: [`deterministic_pick`] projects the
    /// turn's own typed [`CompositionFacts`] onto one member of the selected
    /// registration's approved recipe grammar. Same facts always resolve to
    /// the same recipe.
    #[default]
    Deterministic,
    /// Ask the configured model to choose one plan from the supplied finite
    /// grammar before any graph is admitted or started.
    Model,
    /// Build a task-specific plan from the gate-composition grammar (see
    /// [`super::planner`]) instead of picking a whole-graph recipe.
    Gates,
}

impl GlobalResource for CompositionPolicy {}

impl CompositionPolicy {
    /// Stable policy name carried in local evidence and headless reports.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Deterministic => POLICY,
            Self::Model => MODEL_POLICY,
            Self::Gates => GATES_POLICY,
        }
    }

    /// Parses the explicit host configuration spelling.
    pub fn parse(value: &str) -> Result<Self, CompositionPolicyError> {
        match value {
            "deterministic" => Ok(Self::Deterministic),
            "model" => Ok(Self::Model),
            "gates" => Ok(Self::Gates),
            _ => Err(CompositionPolicyError::InvalidAnswer(format!(
                "unknown composition policy '{value}'; expected deterministic, model, or gates"
            ))),
        }
    }
}

/// Facts exposed to a composition policy, deliberately excluding benchmark
/// identity and mutable graph objects.
#[derive(Debug, Clone)]
pub struct CompositionFacts {
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
    /// Whether the host declared a model provider available.
    pub model_provider_available: bool,
}

/// A typed finite-plan request made before materialisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionRequest {
    recipe: String,
    worklist_bound: Option<usize>,
    grounds: String,
    policy: CompositionPolicy,
    prompt_digest: Option<ContentDigest>,
    response_digest: Option<ContentDigest>,
}

impl CompositionRequest {
    /// Returns the chosen approved recipe name.
    #[must_use]
    pub fn recipe(&self) -> &str {
        &self.recipe
    }

    /// Returns the selected bounded-worklist limit, where applicable.
    #[must_use]
    pub const fn worklist_bound(&self) -> Option<usize> {
        self.worklist_bound
    }

    /// Returns the policy's stated grounds.
    #[must_use]
    pub fn grounds(&self) -> &str {
        &self.grounds
    }

    /// Returns the selected policy.
    #[must_use]
    pub const fn policy(&self) -> &CompositionPolicy {
        &self.policy
    }

    /// Returns the chooser prompt digest when a model made this choice.
    #[must_use]
    pub const fn prompt_digest(&self) -> Option<ContentDigest> {
        self.prompt_digest
    }

    /// Returns the raw chooser-response digest when a model made this choice.
    #[must_use]
    pub const fn response_digest(&self) -> Option<ContentDigest> {
        self.response_digest
    }
}

/// The model-answer subset accepted by the chooser. Unknown JSON fields are
/// ignored, but every semantic field is checked against the finite grammar.
#[derive(Debug, Deserialize)]
struct ModelAnswer {
    recipe: String,
    #[serde(default)]
    worklist_bound: Option<usize>,
    grounds: String,
}

/// A refusal before start from composition policy selection.
#[derive(Debug)]
pub enum CompositionPolicyError {
    /// No configured model can serve an explicitly selected chooser.
    ModelUnavailable,
    /// Model transport or generation failed before any answer was billed.
    Model(String),
    /// The requested policy configuration itself was invalid, before any
    /// model call could have been made (today, only
    /// [`CompositionPolicy::parse`] reaches this).
    InvalidAnswer(String),
    /// A billed model answer failed the finite grammar's own checks — bad
    /// JSON, an unapproved recipe, an out-of-bounds worklist bound, or empty
    /// grounds. Carries the digests of the prompt and response the meter
    /// already billed for this attempt, so a refused candidate stays
    /// auditable evidence rather than a silent discard of a real call.
    InvalidModelAnswer {
        /// What was wrong with the answer.
        detail: String,
        /// Digest of the prompt sent for this attempt.
        prompt_digest: ContentDigest,
        /// Digest of the raw response received for this attempt.
        response_digest: ContentDigest,
    },
}

impl std::fmt::Display for CompositionPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelUnavailable => f.write_str("composition.model-unavailable"),
            Self::Model(error) => write!(f, "composition.model-error: {error}"),
            Self::InvalidAnswer(error) => write!(f, "composition.invalid-model-answer: {error}"),
            Self::InvalidModelAnswer { detail, .. } => {
                write!(f, "composition.invalid-model-answer: {detail}")
            }
        }
    }
}

impl std::error::Error for CompositionPolicyError {}

/// Chooses a member of the finite recipe grammar before materialisation.
///
/// The deterministic policy calls no model: [`deterministic_pick`] projects
/// `facts` onto the selected registration's approved grammar, entirely inside
/// this call. Model policy is intentionally a parser around a small JSON
/// answer, not a graph-generation interface. Its prompt enumerates the only
/// recipe names it may return; the parsed result is checked again by
/// [`select_and_materialize_request`] before native validation and admission.
pub async fn choose(
    selected_policy: CompositionPolicy,
    facts: &CompositionFacts,
    extension: Option<CompositionTemplate>,
    registry: Option<&ModelRegistry>,
    model_id: Option<&str>,
    meter: &TokenMeter,
) -> Result<CompositionRequest, CompositionPolicyError> {
    if matches!(selected_policy, CompositionPolicy::Deterministic) {
        let (recipe, grounds) = match extension {
            Some(template) => (
                template.recipe.to_owned(),
                "application-owned composition template names exactly one recipe".to_owned(),
            ),
            None => deterministic_pick(facts)
                .map_err(|error| CompositionPolicyError::InvalidAnswer(error.to_string()))?,
        };
        return Ok(CompositionRequest {
            worklist_bound: default_worklist_bound(&recipe),
            recipe,
            grounds,
            policy: CompositionPolicy::Deterministic,
            prompt_digest: None,
            response_digest: None,
        });
    }

    let registry = registry.ok_or(CompositionPolicyError::ModelUnavailable)?;
    let model_id = model_id.ok_or(CompositionPolicyError::ModelUnavailable)?;
    let allowed = allowed_recipes(facts.registration.as_str(), extension)
        .map_err(|error| CompositionPolicyError::InvalidAnswer(error.to_string()))?;
    let prompt = format!(
        "Choose exactly one approved coding composition. Return JSON only: \
         {{\"recipe\":\"...\",\"worklist_bound\":number-or-null,\"grounds\":\"...\"}}. \
         Approved recipes: {}. Facts: registration={}, scope_size={}, success_check={}, objective={}, \
         latency_budget_ms={:?}, test_runner_available={}, model_provider_available={}. A worklist_bound is legal only \
         for polaris-code.recipe.bounded-worklist and must be 1..={}. Do not propose a graph.",
        allowed.join(", "),
        facts.registration,
        facts.scope_size,
        facts.success_check,
        facts.objective,
        facts.latency_budget_ms,
        facts.test_runner_available,
        facts.model_provider_available,
        graphs::SWEEP_BUDGET,
    );
    let request = LlmRequest {
        messages: vec![Message::user(prompt.clone())],
        max_output_tokens: meter
            .cost_cap_enabled()
            .then_some(CHOOSER_MAX_OUTPUT_TOKENS),
        ..Default::default()
    };
    let permit = meter
        .reserve_request(model_id, &request, CHOOSER_MAX_OUTPUT_TOKENS.get())
        .map_err(CompositionPolicyError::Model)?;
    meter.record_model_turn();
    let response = registry
        .llm(model_id)
        .map_err(|error| CompositionPolicyError::Model(error.to_string()))?
        .generate(request)
        .await
        .map_err(|error| CompositionPolicyError::Model(error.to_string()))?;
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
    let response_digest = ContentDigest::sha256(raw.as_bytes());
    let invalid = |detail: String| CompositionPolicyError::InvalidModelAnswer {
        detail,
        prompt_digest,
        response_digest,
    };
    let answer: ModelAnswer =
        serde_json::from_str(&raw).map_err(|error| invalid(error.to_string()))?;
    if !allowed.iter().any(|recipe| recipe == &answer.recipe) {
        return Err(invalid(format!(
            "recipe '{}' is not approved for '{}'",
            answer.recipe, facts.registration
        )));
    }
    validate_worklist_bound(&answer.recipe, answer.worklist_bound).map_err(invalid)?;
    if answer.grounds.trim().is_empty() {
        return Err(invalid("grounds are empty".to_owned()));
    }
    Ok(CompositionRequest {
        recipe: answer.recipe,
        worklist_bound: answer.worklist_bound,
        grounds: answer.grounds,
        policy: CompositionPolicy::Model,
        prompt_digest: Some(prompt_digest),
        response_digest: Some(response_digest),
    })
}

fn validate_worklist_bound(recipe: &str, bound: Option<usize>) -> Result<(), String> {
    match (recipe, bound) {
        ("polaris-code.recipe.bounded-worklist", Some(bound))
            if (1..=graphs::SWEEP_BUDGET).contains(&bound) =>
        {
            Ok(())
        }
        ("polaris-code.recipe.bounded-worklist", _) => Err(format!(
            "bounded-worklist requires worklist_bound in 1..={}",
            graphs::SWEEP_BUDGET
        )),
        (_, Some(_)) => Err("worklist_bound is only legal for bounded-worklist".to_owned()),
        (_, None) => Ok(()),
    }
}

fn default_worklist_bound(recipe: &str) -> Option<usize> {
    (recipe == "polaris-code.recipe.bounded-worklist").then_some(graphs::SWEEP_BUDGET)
}

fn allowed_recipes(
    registration: &str,
    extension: Option<CompositionTemplate>,
) -> Result<Vec<String>, CompositionError> {
    if let Some(template) = extension {
        return Ok(vec![template.recipe.to_owned()]);
    }
    let recipes = match registration {
        DIRECT | MODEL_PROPOSED => vec![
            "polaris-code.recipe.direct-verified",
            "polaris-code.recipe.prediction-error",
        ],
        PREDICTION_ERROR => vec!["polaris-code.recipe.prediction-error"],
        WORKLIST_SWEEP => vec!["polaris-code.recipe.bounded-worklist"],
        TEST_GATED => vec!["polaris-code.recipe.test-gated"],
        COVERAGE_AUDIT => vec!["polaris-code.recipe.coverage-sweep"],
        TREE_QUESTION => vec!["polaris-code.recipe.tree-question"],
        _ => {
            return Err(CompositionError::UnknownRegistration(
                registration.to_owned(),
            ));
        }
    };
    Ok(recipes.into_iter().map(str::to_owned).collect())
}

/// Latency ceiling, in milliseconds, at or under which [`deterministic_pick`]
/// rules out a recipe that pays for a second full predict/verify round.
///
/// [`graphs::prediction_error_edit_graph`] commits a full predicted file
/// body before writing, then re-reads the disk and compares — materially more
/// work than [`graphs::direct_edit_graph`]'s single apply-then-verify pass.
const TIGHT_LATENCY_BUDGET_MS: u64 = 5_000;

/// Picks one member of `registration`'s approved recipe grammar from the
/// turn's own typed facts — never a model call, never a fixed
/// one-recipe-per-registration answer. The same facts always resolve to the
/// same recipe, which is what makes [`CompositionPolicy::Deterministic`]
/// reproducible.
///
/// A registration with exactly one approved recipe has nothing to decide; the
/// rule only has visible effect where the grammar actually branches (today,
/// `DIRECT` and `MODEL_PROPOSED`, both choosing between the single-pass
/// direct edit and the predict/act/verify/compare loop). Checked in priority
/// order, each rule returning the recipe together with the fact and reasoning
/// that picked it:
///
/// 1. A stated latency budget at or under [`TIGHT_LATENCY_BUDGET_MS`] rules
///    out paying for prediction-error's extra round: direct-verified.
/// 2. A scope spanning more than one declared path favours the recipe whose
///    compare step can catch drift between what was predicted and what
///    actually landed, which a single unexamined pass would not re-check:
///    prediction-error.
/// 3. A success check with no judge outside this graph (`stated-edits`, where
///    this graph's own verify is the only check the work will ever get) also
///    favours that falsifiable compare: prediction-error.
/// 4. Otherwise — single-site scope, no tight budget, and an external command
///    or delegated judge already in place — the cheapest approved recipe is
///    enough: direct-verified.
fn deterministic_pick(facts: &CompositionFacts) -> Result<(String, String), CompositionError> {
    let allowed = allowed_recipes(&facts.registration, None)?;
    if allowed.len() <= 1 {
        let recipe = allowed
            .into_iter()
            .next()
            .ok_or_else(|| CompositionError::UnknownRegistration(facts.registration.clone()))?;
        return Ok((
            recipe,
            format!(
                "registration '{}' approves exactly one recipe; nothing to choose",
                facts.registration
            ),
        ));
    }
    let approves = |recipe: &str| allowed.iter().any(|approved| approved == recipe);

    if let Some(budget_ms) = facts.latency_budget_ms
        && budget_ms <= TIGHT_LATENCY_BUDGET_MS
        && approves("polaris-code.recipe.direct-verified")
    {
        return Ok((
            "polaris-code.recipe.direct-verified".to_owned(),
            format!(
                "latency_budget_ms={budget_ms} is at or under the \
                 {TIGHT_LATENCY_BUDGET_MS}ms tight-budget threshold: too little room for \
                 prediction-error's extra predict/compare round"
            ),
        ));
    }
    if facts.scope_size > 1 && approves("polaris-code.recipe.prediction-error") {
        return Ok((
            "polaris-code.recipe.prediction-error".to_owned(),
            format!(
                "scope_size={} spans more than one declared path: prediction-error's \
                 predict/verify compare catches drift across a wider surface that a single \
                 unexamined pass would not re-check",
                facts.scope_size
            ),
        ));
    }
    if facts.success_check == "stated-edits" && approves("polaris-code.recipe.prediction-error") {
        return Ok((
            "polaris-code.recipe.prediction-error".to_owned(),
            "success_check=stated-edits: this graph's own verify is the only check the work \
             will ever get, so the extra falsifiable compare earns its cost"
                .to_owned(),
        ));
    }
    Ok((
        allowed[0].clone(),
        format!(
            "scope_size={}, latency_budget_ms={:?}, success_check={}: no escalation rule \
             applies, so the cheapest approved recipe is used",
            facts.scope_size, facts.latency_budget_ms, facts.success_check
        ),
    ))
}

impl CompositionTemplate {
    /// Names the recipe, its ordered blocks, and the native graph builder.
    #[must_use]
    pub const fn new(
        recipe: &'static str,
        blocks: &'static [&'static str],
        build: fn() -> Graph,
    ) -> Self {
        Self {
            recipe,
            blocks,
            build,
        }
    }
}

/// Optional startup registrations for coding topologies owned by another pack.
///
/// The shipped coding pack uses this module's sealed built-in mapping.  This
/// small resource is solely the extension seam for another pack that registers
/// its own coding graph: it must provide the finite recipe and a builder for
/// the equivalent graph, rather than silently falling back to an unrecorded
/// catalog graph.
#[derive(Debug, Default, Clone)]
pub struct CompositionTemplates(Vec<(&'static str, CompositionTemplate)>);

impl GlobalResource for CompositionTemplates {}

impl CompositionTemplates {
    /// Creates an empty extension set.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Adds one registration-to-composition binding before server assembly.
    ///
    /// Later bindings with the same registration replace earlier ones, making
    /// the application pack that installs last the explicit owner of that
    /// registration's embodiment.
    pub fn register(&mut self, registration: &'static str, template: CompositionTemplate) {
        if let Some((_, existing)) = self
            .0
            .iter_mut()
            .find(|(registered, _)| *registered == registration)
        {
            *existing = template;
        } else {
            self.0.push((registration, template));
        }
    }

    /// Looks up the finite template declared for `registration`.
    #[must_use]
    pub fn template(&self, registration: &str) -> Option<CompositionTemplate> {
        self.0
            .iter()
            .find_map(|(registered, template)| (*registered == registration).then_some(*template))
    }
}

/// A selected recipe together with its freshly materialised native graph.
#[derive(Debug)]
pub struct MaterializedComposition {
    decision: CompositionDecision,
    graph: Graph,
    native_signature: NativeGraphSignatureWitness,
    request: CompositionRequest,
}

impl MaterializedComposition {
    /// Wraps a gate-plan materialisation as an ordinary composition result.
    ///
    /// [`super::planner`] builds `decision`, `graph`, and `native_signature`
    /// from a submitted [`polaris_cosmos::composition::GatePlan`] rather than
    /// a fixed recipe, but from here on a gate-plan composition is handled
    /// identically to a finite-recipe one: the same live admission,
    /// [`execute_permitted`](super::host), and reporting all read this same
    /// type.
    #[must_use]
    pub(crate) const fn from_gate_plan(
        decision: CompositionDecision,
        graph: Graph,
        native_signature: NativeGraphSignatureWitness,
        recipe: String,
        grounds: String,
        prompt_digest: Option<ContentDigest>,
        response_digest: Option<ContentDigest>,
    ) -> Self {
        Self {
            decision,
            graph,
            native_signature,
            request: CompositionRequest {
                recipe,
                worklist_bound: None,
                grounds,
                policy: CompositionPolicy::Gates,
                prompt_digest,
                response_digest,
            },
        }
    }

    /// Returns the recorded finite composition decision.
    #[must_use]
    pub const fn decision(&self) -> &CompositionDecision {
        &self.decision
    }

    /// Returns the native graph built for this turn alone.
    #[must_use]
    pub const fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Returns the audit witness derived from the native graph signature.
    #[must_use]
    pub const fn native_signature(&self) -> &NativeGraphSignatureWitness {
        &self.native_signature
    }

    /// Returns the pre-start policy request that materialised this graph.
    #[must_use]
    pub const fn request(&self) -> &CompositionRequest {
        &self.request
    }

    /// Records successful native validation and live admission.
    pub fn admitted_validation(
        &self,
        attempt_id: polaris_cosmos::contracts::AdmissionAttemptId,
        recorded_at: RecordedAt,
    ) -> Result<CompositionValidation, CompositionError> {
        Ok(CompositionValidation::new(
            attempt_id,
            &self.decision,
            recorded_at,
            self.native_signature.clone(),
            CompositionValidationDisposition::Admitted,
        )?)
    }
}

/// Why an approved recipe could not become a native graph.
#[derive(Debug)]
pub enum CompositionError {
    /// Cosmos rejected a bounded identity or evidence record.
    Contract(polaris_cosmos::contracts::ContractError),
    /// The selected catalog registration has no approved recipe.
    UnknownRegistration(String),
    /// The fresh native graph did not satisfy the coding graph contract.
    NativeValidation(String),
}

impl std::fmt::Display for CompositionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => error.fmt(formatter),
            Self::UnknownRegistration(registration) => {
                write!(
                    formatter,
                    "no approved composition recipe for '{registration}'"
                )
            }
            Self::NativeValidation(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for CompositionError {}

impl From<polaris_cosmos::contracts::ContractError> for CompositionError {
    fn from(error: polaris_cosmos::contracts::ContractError) -> Self {
        Self::Contract(error)
    }
}

/// Resolves and materialises `registration`'s first approved recipe, with no
/// [`CompositionFacts`] to consider.
///
/// This is a facts-free convenience for a caller with no turn to describe
/// (used by this module's own tests); the live host path calls [`choose`]
/// with real facts and reaches [`select_and_materialize_request`] instead, so
/// a registration with more than one approved recipe is actually decided by
/// [`deterministic_pick`] rather than defaulting here.
///
/// `forced` records a caller-directed topology choice distinctly from the
/// deterministic autonomous policy.  It only describes provenance: the caller
/// has already been narrowed by normal catalog selection and can never use it
/// to bypass preflight or live admission.
pub fn select_and_materialize(
    registration: &str,
    extension: Option<CompositionTemplate>,
    decision_id: SelectionDecisionId,
    recorded_at: RecordedAt,
    forced: bool,
    contract: &TopologyContractKey,
) -> Result<MaterializedComposition, CompositionError> {
    let recipe = match extension {
        Some(template) => template.recipe.to_owned(),
        None => recipe(registration)?.0.to_owned(),
    };
    select_and_materialize_request(
        registration,
        extension,
        CompositionRequest {
            worklist_bound: default_worklist_bound(&recipe),
            recipe,
            grounds: "deterministic registration-to-recipe projection".to_owned(),
            policy: CompositionPolicy::Deterministic,
            prompt_digest: None,
            response_digest: None,
        },
        decision_id,
        recorded_at,
        forced,
        contract,
    )
}

/// Materialises a policy-selected request after checking it is a member of
/// this registration's finite grammar.  This is deliberately synchronous:
/// all model interaction occurs in [`choose`] before this native validation
/// boundary.
pub fn select_and_materialize_request(
    registration: &str,
    extension: Option<CompositionTemplate>,
    request: CompositionRequest,
    decision_id: SelectionDecisionId,
    recorded_at: RecordedAt,
    forced: bool,
    contract: &TopologyContractKey,
) -> Result<MaterializedComposition, CompositionError> {
    let (recipe, blocks, graph, extension_contents) = match extension {
        Some(template) if request.recipe == template.recipe => (
            template.recipe,
            template.blocks.to_vec(),
            (template.build)(),
            Some(template),
        ),
        Some(_) => {
            return Err(CompositionError::UnknownRegistration(format!(
                "{registration} does not approve recipe '{}'",
                request.recipe
            )));
        }
        None => {
            let (recipe, blocks, graph) = recipe_by_name(registration, &request.recipe)?;
            (recipe, blocks, graph, None)
        }
    };
    let plan = polaris_cosmos::contracts::CompositionPlan::new(
        CompositionCatalogKey::new(CompositionCatalogId::new(CATALOG)?, behavior_version()),
        catalogue_digest(registration, extension_contents),
        recipe_ref(recipe, request.worklist_bound)?,
        blocks
            .into_iter()
            .map(|block| block_ref(block, request.worklist_bound))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    // A caller-directed *topology* remains forced under the deterministic
    // projection. A model-selected composition is still model-chosen: the
    // directive narrowed the catalog candidate, not the recipe the policy
    // picked from its approved grammar.
    let origin = if forced && matches!(request.policy, CompositionPolicy::Deterministic) {
        CompositionChoiceOrigin::Forced {
            source: super::source_key(),
        }
    } else {
        CompositionChoiceOrigin::Autonomous {
            policy: policy(match request.policy {
                CompositionPolicy::Deterministic => POLICY,
                CompositionPolicy::Model => MODEL_POLICY,
                CompositionPolicy::Gates => GATES_POLICY,
            }),
        }
    };
    let choice = CompositionChoice::new(plan, origin)?;
    let decision = CompositionDecision::new(
        decision_id,
        recorded_at,
        choice,
        vec![provenance(
            SourceKind::CosmosPolicy,
            polaris_cosmos::contracts::EpistemicBasis::Verified,
        )],
    )?;

    let validation = graph.validate();
    if validation.is_err() {
        return Err(CompositionError::NativeValidation(format!(
            "materialised composition '{registration}' failed native validation: {validation}"
        )));
    }
    let signature = graph.signature();
    if !signature.satisfies(
        &polaris::graph::GraphSignature::new()
            .produce::<polaris_cosmos::contracts::AssessmentConclusion>(),
    ) {
        return Err(CompositionError::NativeValidation(format!(
            "materialised composition '{registration}' does not satisfy coding contract {contract}"
        )));
    }
    let witness = NativeGraphSignatureWitness::new(
        contract.clone(),
        ContentDigest::sha256(format!("{signature}").as_bytes()),
    );
    Ok(MaterializedComposition {
        decision,
        graph,
        native_signature: witness,
        request,
    })
}

fn catalogue_digest(registration: &str, extension: Option<CompositionTemplate>) -> ContentDigest {
    let Some(template) = extension else {
        return ContentDigest::sha256(CATALOGUE_CONTENTS);
    };
    let mut contents = CATALOGUE_CONTENTS.to_vec();
    contents.extend_from_slice(registration.as_bytes());
    contents.push(b':');
    contents.extend_from_slice(template.recipe.as_bytes());
    for block in template.blocks {
        contents.push(b',');
        contents.extend_from_slice(block.as_bytes());
    }
    ContentDigest::sha256(&contents)
}

fn configuration(
    value: &str,
    worklist_bound: Option<usize>,
) -> Result<ResolvedConfigurationRef, CompositionError> {
    let value = match worklist_bound {
        Some(bound) => format!("{value};worklist-bound={bound}"),
        None => value.to_owned(),
    };
    Ok(ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new(CONFIGURATION_SCHEMA)?,
            behavior_version(),
        ),
        ConfigurationValue::String(value),
    )?)
}

fn recipe_ref(
    recipe: &str,
    worklist_bound: Option<usize>,
) -> Result<CompositionRecipeRef, CompositionError> {
    Ok(CompositionRecipeRef::new(
        CompositionRecipeKey::new(CompositionRecipeId::new(recipe)?, behavior_version()),
        configuration(recipe, worklist_bound)?,
    )?)
}

fn block_ref(
    block: &str,
    worklist_bound: Option<usize>,
) -> Result<CompositionBlockRef, CompositionError> {
    Ok(CompositionBlockRef::new(
        CompositionBlockKey::new(CompositionBlockId::new(block)?, behavior_version()),
        configuration(block, worklist_bound)?,
    )?)
}

fn recipe(
    registration: &str,
) -> Result<(&'static str, Vec<&'static str>, Graph), CompositionError> {
    let result = match registration {
        DIRECT => (
            "polaris-code.recipe.direct-verified",
            vec!["cosmos.verified-step", "cosmos.resolve-residuals"],
            graphs::direct_edit_graph(),
        ),
        PREDICTION_ERROR => (
            "polaris-code.recipe.prediction-error",
            vec!["cosmos.prediction-error-loop"],
            graphs::prediction_error_edit_graph(),
        ),
        WORKLIST_SWEEP => (
            "polaris-code.recipe.bounded-worklist",
            vec!["cosmos.drain-worklist", "cosmos.resolve-residuals"],
            graphs::worklist_sweep_graph(),
        ),
        TEST_GATED => (
            "polaris-code.recipe.test-gated",
            vec!["cosmos.verified-step", "cosmos.resolve-residuals"],
            graphs::test_gated_edit_graph(),
        ),
        MODEL_PROPOSED => (
            "polaris-code.recipe.model-proposed",
            vec!["cosmos.verified-step", "cosmos.resolve-residuals"],
            graphs::model_proposed_edit_graph(),
        ),
        COVERAGE_AUDIT => (
            "polaris-code.recipe.coverage-sweep",
            vec!["cosmos.coverage-sweep"],
            graphs::coverage_audit_graph(),
        ),
        TREE_QUESTION => (
            "polaris-code.recipe.tree-question",
            vec!["polaris-code.tree-survey"],
            graphs::tree_question_graph(),
        ),
        _ => {
            return Err(CompositionError::UnknownRegistration(
                registration.to_owned(),
            ));
        }
    };
    Ok(result)
}

/// Resolves one recipe that the selected registration is permitted to use.
///
/// The alternatives are deliberately small and graph-compatible. A direct or
/// model-proposed edit may use the prediction-error variant when a policy sees
/// a multi-file scope; no option invents a graph or bypasses the same contract
/// and admission path.
fn recipe_by_name(
    registration: &str,
    selected: &str,
) -> Result<(&'static str, Vec<&'static str>, Graph), CompositionError> {
    let permitted = match registration {
        DIRECT | MODEL_PROPOSED => matches!(
            selected,
            "polaris-code.recipe.direct-verified" | "polaris-code.recipe.prediction-error"
        ),
        PREDICTION_ERROR => selected == "polaris-code.recipe.prediction-error",
        WORKLIST_SWEEP => selected == "polaris-code.recipe.bounded-worklist",
        TEST_GATED => selected == "polaris-code.recipe.test-gated",
        COVERAGE_AUDIT => selected == "polaris-code.recipe.coverage-sweep",
        TREE_QUESTION => selected == "polaris-code.recipe.tree-question",
        _ => false,
    };
    if !permitted {
        return Err(CompositionError::UnknownRegistration(format!(
            "{registration} does not approve recipe '{selected}'"
        )));
    }
    match selected {
        "polaris-code.recipe.direct-verified" => Ok((
            "polaris-code.recipe.direct-verified",
            vec!["cosmos.verified-step", "cosmos.resolve-residuals"],
            graphs::direct_edit_graph(),
        )),
        "polaris-code.recipe.prediction-error" => Ok((
            "polaris-code.recipe.prediction-error",
            vec!["cosmos.prediction-error-loop"],
            graphs::prediction_error_edit_graph(),
        )),
        "polaris-code.recipe.bounded-worklist" => Ok((
            "polaris-code.recipe.bounded-worklist",
            vec!["cosmos.drain-worklist", "cosmos.resolve-residuals"],
            graphs::worklist_sweep_graph(),
        )),
        "polaris-code.recipe.test-gated" => Ok((
            "polaris-code.recipe.test-gated",
            vec!["cosmos.verified-step", "cosmos.resolve-residuals"],
            graphs::test_gated_edit_graph(),
        )),
        "polaris-code.recipe.coverage-sweep" => Ok((
            "polaris-code.recipe.coverage-sweep",
            vec!["cosmos.coverage-sweep"],
            graphs::coverage_audit_graph(),
        )),
        "polaris-code.recipe.tree-question" => Ok((
            "polaris-code.recipe.tree-question",
            vec!["polaris-code.tree-survey"],
            graphs::tree_question_graph(),
        )),
        _ => Err(CompositionError::UnknownRegistration(selected.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use polaris_cosmos::contracts::{LogicalSequence, RecordedAt, SelectionDecisionId};

    use super::*;

    #[test]
    fn direct_and_bounded_worklist_materialise_distinct_validated_shapes() {
        let contract = super::super::topology::contract().expect("coding contract");
        let direct = select_and_materialize(
            DIRECT,
            None,
            SelectionDecisionId::from_bytes([
                0, 1, 2, 3, 4, 5, 0x40, 7, 0x80, 9, 10, 11, 12, 13, 14, 15,
            ])
            .expect("valid ID"),
            RecordedAt::new(LogicalSequence::new(1), None),
            false,
            contract.key(),
        )
        .expect("direct composition materialises");
        let worklist = select_and_materialize(
            WORKLIST_SWEEP,
            None,
            SelectionDecisionId::from_bytes([
                1, 2, 3, 4, 5, 6, 0x40, 8, 0x80, 10, 11, 12, 13, 14, 15, 16,
            ])
            .expect("valid ID"),
            RecordedAt::new(LogicalSequence::new(1), None),
            true,
            contract.key(),
        )
        .expect("worklist composition materialises");

        assert!(direct.graph().validate().is_ok());
        assert!(worklist.graph().validate().is_ok());
        assert_ne!(
            direct.decision().composition_digest(),
            worklist.decision().composition_digest()
        );
        assert_ne!(
            direct.native_signature().signature_digest(),
            worklist.native_signature().signature_digest()
        );
    }

    #[test]
    fn direct_registration_has_two_native_validated_policy_choices() {
        let choices = allowed_recipes(DIRECT, None).expect("direct grammar exists");
        assert_eq!(
            choices,
            vec![
                "polaris-code.recipe.direct-verified",
                "polaris-code.recipe.prediction-error"
            ]
        );
        let (_, _, direct) = recipe_by_name(DIRECT, &choices[0]).expect("direct is approved");
        let (_, _, prediction) =
            recipe_by_name(DIRECT, &choices[1]).expect("prediction error is approved");
        assert!(direct.validate().is_ok());
        assert!(prediction.validate().is_ok());
        assert_ne!(direct.signature(), prediction.signature());
    }

    fn base_facts() -> CompositionFacts {
        CompositionFacts {
            registration: DIRECT.to_owned(),
            scope_size: 1,
            success_check: "command",
            objective: "fast",
            latency_budget_ms: None,
            test_runner_available: false,
            model_provider_available: false,
        }
    }

    #[test]
    fn tight_latency_budget_picks_direct_verified_regardless_of_scope() {
        let facts = CompositionFacts {
            scope_size: 4,
            latency_budget_ms: Some(1_000),
            ..base_facts()
        };
        let (recipe, grounds) = deterministic_pick(&facts).expect("direct grammar exists");
        assert_eq!(recipe, "polaris-code.recipe.direct-verified");
        assert!(grounds.contains("latency_budget_ms=1000"));
    }

    #[test]
    fn multi_site_scope_picks_prediction_error_without_a_tight_budget() {
        let facts = CompositionFacts {
            scope_size: 2,
            ..base_facts()
        };
        let (recipe, grounds) = deterministic_pick(&facts).expect("direct grammar exists");
        assert_eq!(recipe, "polaris-code.recipe.prediction-error");
        assert!(grounds.contains("scope_size=2"));
    }

    #[test]
    fn self_judged_success_check_picks_prediction_error_for_a_single_site() {
        let facts = CompositionFacts {
            success_check: "stated-edits",
            ..base_facts()
        };
        let (recipe, grounds) = deterministic_pick(&facts).expect("direct grammar exists");
        assert_eq!(recipe, "polaris-code.recipe.prediction-error");
        assert!(grounds.contains("success_check=stated-edits"));
    }

    #[test]
    fn single_site_command_checked_scope_defaults_to_direct_verified() {
        let facts = base_facts();
        let (recipe, grounds) = deterministic_pick(&facts).expect("direct grammar exists");
        assert_eq!(recipe, "polaris-code.recipe.direct-verified");
        assert!(grounds.contains("no escalation rule applies"));
    }

    #[test]
    fn single_recipe_registration_has_nothing_to_choose() {
        let facts = CompositionFacts {
            registration: WORKLIST_SWEEP.to_owned(),
            scope_size: 9,
            ..base_facts()
        };
        let (recipe, grounds) = deterministic_pick(&facts).expect("worklist grammar exists");
        assert_eq!(recipe, "polaris-code.recipe.bounded-worklist");
        assert!(grounds.contains("approves exactly one recipe"));
    }

    #[tokio::test]
    async fn deterministic_policy_materialises_different_recipes_from_different_facts() {
        let contract = super::super::topology::contract().expect("coding contract");
        let meter = TokenMeter::default();
        let single_site = base_facts();
        let multi_site = CompositionFacts {
            scope_size: 2,
            ..base_facts()
        };

        let single_request = choose(
            CompositionPolicy::Deterministic,
            &single_site,
            None,
            None,
            None,
            &meter,
        )
        .await
        .expect("deterministic policy never calls a model");
        let multi_request = choose(
            CompositionPolicy::Deterministic,
            &multi_site,
            None,
            None,
            None,
            &meter,
        )
        .await
        .expect("deterministic policy never calls a model");
        assert_eq!(
            single_request.recipe(),
            "polaris-code.recipe.direct-verified"
        );
        assert_eq!(
            multi_request.recipe(),
            "polaris-code.recipe.prediction-error"
        );

        let decision_id_a = SelectionDecisionId::from_bytes([
            2, 3, 4, 5, 6, 7, 0x40, 9, 0x80, 11, 12, 13, 14, 15, 16, 17,
        ])
        .expect("valid ID");
        let decision_id_b = SelectionDecisionId::from_bytes([
            3, 4, 5, 6, 7, 8, 0x40, 10, 0x80, 12, 13, 14, 15, 16, 17, 18,
        ])
        .expect("valid ID");
        let recorded_at = RecordedAt::new(LogicalSequence::new(1), None);

        let single = select_and_materialize_request(
            DIRECT,
            None,
            single_request,
            decision_id_a,
            recorded_at,
            false,
            contract.key(),
        )
        .expect("single-site composition materialises");
        let multi = select_and_materialize_request(
            DIRECT,
            None,
            multi_request,
            decision_id_b,
            recorded_at,
            false,
            contract.key(),
        )
        .expect("multi-site composition materialises");

        assert!(single.graph().validate().is_ok());
        assert!(multi.graph().validate().is_ok());
        assert_ne!(
            single.decision().composition_digest(),
            multi.decision().composition_digest()
        );
    }

    #[tokio::test]
    async fn deterministic_policy_is_reproducible_for_the_same_facts() {
        let meter = TokenMeter::default();
        let facts = base_facts();

        let first = choose(
            CompositionPolicy::Deterministic,
            &facts,
            None,
            None,
            None,
            &meter,
        )
        .await
        .expect("deterministic policy never calls a model");
        let second = choose(
            CompositionPolicy::Deterministic,
            &facts,
            None,
            None,
            None,
            &meter,
        )
        .await
        .expect("deterministic policy never calls a model");

        assert_eq!(first.recipe(), second.recipe());
        assert_eq!(first.grounds(), second.grounds());
        assert_eq!(first.worklist_bound(), second.worklist_bound());
    }
}
