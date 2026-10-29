//! Per-trace extraction agent: trace line → triples → knowledge graph.

use crate::agents::seed::SeedOntology;
use crate::iri::RDF_TYPE;
use crate::resources::aliases::{Aliases, OWL_SAME_AS, Resolution};
use crate::resources::kg::{Entity, KnowledgeGraph, Predicate, Triple};
use crate::resources::ontology_gap::{OntologyGap, record_predicate_gaps};
use crate::resources::sightings::{Sighting, Sightings};
use crate::resources::spend::Spend;
use crate::resources::trace_line::TraceLine;
use polaris::agent::{Agent, SetupError};
use polaris::graph::Graph;
use polaris::models::ModelRegistry;
use polaris::models::llm::{Llm, LlmRequest, Message};
use polaris::prelude::Out;
use polaris::system::param::{Res, SystemContext};
use polaris::system::resource::LocalResource;
use polaris::system::system;
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;

/// Configures which model the extractor calls. Inserted per-turn into the
/// session context by [`crate::ExtractionPlugin`].
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Polaris model id, e.g. `"anthropic/claude-haiku-4-5"`.
    pub model_id: String,
}

impl AgentConfig {
    /// Construct an `AgentConfig` for the given model id.
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
        }
    }
}

impl LocalResource for AgentConfig {}

/// Resolved [`Llm`] handle for the extraction agent. Created by
/// [`ExtractionAgent::setup`] from the registered [`AgentConfig`].
#[derive(Clone)]
pub struct AgentLlm(Llm);

impl AgentLlm {
    /// Wrap a resolved [`Llm`] handle. Used by agent `setup` impls that need
    /// to inject the local resource.
    #[must_use]
    pub fn new(llm: Llm) -> Self {
        Self(llm)
    }
}

impl Deref for AgentLlm {
    type Target = Llm;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl LocalResource for AgentLlm {}

/// Policy for handling extracted triples that don't fit any ontology shape.
///
/// Only consulted when [`OntologyContext::predicates`] is non-empty.
#[derive(Debug, Clone, Default)]
pub enum OntologyPolicy {
    /// Closed-world: the schema enum is exactly the ontology predicates.
    /// The LLM cannot emit a predicate outside the list; lines whose
    /// relations don't match any curated predicate yield zero triples.
    #[default]
    Strict,
    /// Open-world: the schema enum is `ontology predicates ∪ {catchall}`.
    /// The LLM uses the catchall sentinel for facts that don't fit a curated
    /// shape, so the observation isn't lost — downstream consumers can route
    /// or quarantine catchall triples by predicate.
    CatchAll {
        /// Sentinel predicate emitted when no curated predicate fits.
        /// Chosen by the caller's [`OntologyPolicy::CatchAll`] policy —
        /// there is no bundled default.
        catchall_predicate: String,
        /// Fully-qualified IRI to mint for triples whose predicate equals
        /// `catchall_predicate`. Resolved by the oxigraph backend's `TBox`
        /// snapshot when the sentinel is declared there; `None` falls back
        /// to the default `urn:ontrace:p/<sentinel>` minting in
        /// [`crate::iri`]. Bridges the LLM-emitted local name to whatever
        /// curated IRI declares it, so `ABox` triples align with `TBox`
        /// declarations.
        catchall_iri: Option<String>,
    },
}

/// Object node kind constraint for one curated predicate, harvested from a
/// SHACL `sh:nodeKind` declaration on the predicate's property shape.
///
/// Only the IRI-vs-literal distinction matters in the extraction path —
/// blank nodes never appear in the object position of an extracted triple,
/// so SHACL kinds that admit blank nodes collapse onto whichever of
/// [`Self::Iri`] / [`Self::Literal`] they also admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// Object must be an entity IRI (`sh:IRI`, `sh:IRIOrLiteral`,
    /// `sh:BlankNodeOrIRI`).
    Iri,
    /// Object must be a literal value (`sh:Literal`, `sh:BlankNodeOrLiteral`).
    Literal,
}

/// Shape hints for one curated predicate, surfaced both in the prompt
/// summary (so the LLM can pick the most specific predicate whose
/// domain / range fit the subject and object) and in SHACL
/// post-validation (so the same constraints reject violations).
///
/// Single source of truth: both the prompt rendering and
/// [`crate::agents::shacl::check`] read this struct. The four fields
/// together cover the constraint surface ontrace cares about:
///
/// - `domain_class` — `rdfs:domain` class on the predicate.
/// - `range_class` — `rdfs:range` or SHACL `sh:class`; an entity is expected.
/// - `range_datatype` — `rdfs:range` xsd-IRI or SHACL `sh:datatype`; a typed
///   literal is expected.
/// - `node_kind` — SHACL `sh:nodeKind` IRI-vs-Literal narrowing when neither
///   class nor datatype is declared but the kind of the object is fixed.
///
/// Every field is optional. Predicates declared in the `TBox` without any
/// shape produce an all-`None` entry; the prompt renders an asterisk for
/// the missing side and the validator is a pass-through for that predicate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PredicateShape {
    /// Local-name of the predicate's `rdfs:domain` class, if declared.
    pub domain_class: Option<String>,
    /// Local-name of the predicate's `rdfs:range` class (or SHACL
    /// `sh:class`) when the range is a curated class — *not* an xsd
    /// datatype.
    pub range_class: Option<String>,
    /// Local-name of an `xsd:` datatype (`integer`, `decimal`, `duration`,
    /// …) when `rdfs:range` or SHACL `sh:datatype` names one. Mutually
    /// exclusive with `range_class` in practice; carried in separate fields
    /// so the consumer doesn't have to re-classify.
    pub range_datatype: Option<String>,
    /// SHACL `sh:nodeKind` narrowing on the object position. Set even when
    /// neither `range_class` nor `range_datatype` applies — `sh:nodeKind
    /// sh:IRI` alone says "an entity, but no fixed class".
    pub node_kind: Option<NodeKind>,
}

/// Transitive `rdfs:subClassOf` index over the curated `TBox`.
///
/// Built once per session by each backend's `setup` and consulted by the
/// SHACL validator so abstract-class domains and ranges (`core:Event`,
/// `core:Actor`) match every subclass-typed subject or object — the
/// behaviour SHACL `sh:targetClass` and `sh:class` specify (see the SHACL
/// Recommendation).
///
/// Keyed by class local-name, mirroring the local-name comparisons in
/// [`crate::agents::shacl::check`]. Stores direct parents only; the
/// transitive walk happens in [`Self::is_a`].
#[derive(Debug, Clone, Default)]
pub struct ClassHierarchy {
    /// Class local-name → direct `rdfs:subClassOf` parents (local-name).
    /// Repeated declarations of the same parent are folded out.
    parents: HashMap<String, Vec<String>>,
}

impl ClassHierarchy {
    /// Construct an empty hierarchy — equivalent to
    /// [`ClassHierarchy::default`], spelled out for call-site clarity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a `subclass rdfs:subClassOf parent` edge.
    ///
    /// Duplicate parents for the same subclass are coalesced — the
    /// downstream walk only cares about the set, not the multiplicity.
    pub fn add_subclass(&mut self, subclass: impl Into<String>, parent: impl Into<String>) {
        let parent = parent.into();
        let entry = self.parents.entry(subclass.into()).or_default();
        if !entry.iter().any(|p| p == &parent) {
            entry.push(parent);
        }
    }

    /// `true` when `actual` is `expected` or transitively a subclass of it.
    ///
    /// Reflexive: `is_a(x, x)` is `true` even when `x` has no entry.
    /// Cycle-safe via an explicit visited set, so a malformed `TBox` that
    /// declares `A subClassOf B` and `B subClassOf A` won't loop.
    #[must_use]
    pub fn is_a(&self, actual: &str, expected: &str) -> bool {
        if actual == expected {
            return true;
        }
        let mut visited: HashSet<&str> = HashSet::new();
        visited.insert(actual);
        let mut stack: Vec<&str> = vec![actual];
        while let Some(node) = stack.pop() {
            let Some(parents) = self.parents.get(node) else {
                continue;
            };
            for p in parents {
                if p == expected {
                    return true;
                }
                if visited.insert(p.as_str()) {
                    stack.push(p.as_str());
                }
            }
        }
        false
    }

    /// `true` when the hierarchy has no edges. Equivalent to
    /// [`Self::default`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// Fold every edge in `other` into `self`. Used by [`SeedOntology::merged`]
    /// to union per-pack hierarchies into the merged seed without exposing
    /// the internal map.
    ///
    /// [`SeedOntology::merged`]: crate::agents::seed::SeedOntology::merged
    pub fn extend(&mut self, other: &Self) {
        for (subclass, parents) in &other.parents {
            for parent in parents {
                self.add_subclass(subclass.clone(), parent.clone());
            }
        }
    }
}

/// Per-session snapshot of the curated ontology.
///
/// Built once per agent session by each agent's `setup`:
///
/// - The naive in-memory [`ExtractionAgent`] reads a [`SeedOntology`]
///   from the session context (if the plugin inserted one) and calls
///   [`OntologyContext::from_curated`].
/// - The oxigraph agent queries the `TBox` graph for curated declarations
///   and calls the same constructor.
///
/// When `predicates` is empty, the ontology guidance is skipped entirely
/// and the extractor falls back to the free-form prompt and schema.
#[derive(Debug, Clone, Default)]
pub struct OntologyContext {
    /// Local-name list of predicates curated in the ontology. Used both for the
    /// JSON-schema `enum` constraint and in the prompt summary.
    pub predicates: Vec<String>,
    /// Local-name list of curated classes. Surfaced in the prompt only;
    /// the schema does not currently constrain entity types.
    pub classes: Vec<String>,
    /// Pre-rendered, prompt-ready summary of the ontology. Empty when
    /// `predicates` and `classes` are both empty.
    pub summary: String,
    /// Behavior when an extracted relation doesn't match any curated predicate.
    pub policy: OntologyPolicy,
    /// Bridge from curated predicate local-name (what the LLM emits) to the
    /// fully-qualified IRI declared in the `TBox`. The write paths consult
    /// this in [`bridge_predicate`] so every curated predicate lands in the
    /// `ABox` at the same IRI the `TBox` uses — `?s <tbox-iri> ?o` joins
    /// then work without any per-query bridging. Predicates absent from the
    /// map fall back to the default `urn:ontrace:p/<name>` minting.
    pub predicate_iris: HashMap<String, String>,
    /// `rdfs:domain` / `rdfs:range` hints keyed by predicate local-name —
    /// what `extract_triples` consults to infer `rdf:type` triples when the
    /// LLM didn't supply explicit `subject_type` / `object_type` fields.
    /// Predicates without an entry produce no inferred type.
    pub predicate_shapes: HashMap<String, PredicateShape>,
    /// Bridge from curated class local-name to the fully-qualified IRI
    /// declared in the `TBox`. Used by the type-emission path so the object
    /// of an `rdf:type` triple is the curated IRI (`urn:ontrace:onto/agent/Agent`)
    /// rather than the entity-mint fallback (`urn:ontrace:e/Agent`). Classes
    /// absent from the map render as bare entity names.
    pub class_iris: HashMap<String, String>,
    /// Transitive `rdfs:subClassOf` index over the curated `TBox`. The SHACL
    /// validator queries this so a triple whose subject is typed as a
    /// subclass of the declared `domain_class` (or whose object is typed as
    /// a subclass of the declared `range_class`) is accepted — matching
    /// the standard SHACL semantics (`sh:targetClass` and `sh:class` admit
    /// every transitive subclass).
    pub class_hierarchy: ClassHierarchy,
}

impl OntologyContext {
    /// `true` when no ontology is configured. The extractor falls back to
    /// the free-form prompt and schema in that case.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty() && self.classes.is_empty()
    }

    /// Build a curated context from explicit predicate + class lists, with
    /// optional `rdfs:domain` / `rdfs:range` hints per predicate and a
    /// local-name → curated-IRI bridge for `ABox`/`TBox` alignment.
    ///
    /// Renders the prompt-ready `summary` in the canonical format
    /// consumed by [`build_prompt`]. Returns a default-empty context
    /// when both predicate and class lists are empty so callers can
    /// pipe through empty seeds without a branch. `shapes` is keyed by
    /// predicate local-name; predicates with no entry render as a bare
    /// name in the summary. `predicate_iris` may be empty — the catchall
    /// `OntologyPolicy::CatchAll::catchall_iri` field still works as a
    /// fallback bridge for the sentinel predicate alone.
    #[must_use]
    pub fn from_curated(
        predicates: Vec<String>,
        classes: Vec<String>,
        shapes: HashMap<String, PredicateShape>,
        predicate_iris: HashMap<String, String>,
        class_iris: HashMap<String, String>,
        class_hierarchy: ClassHierarchy,
        policy: OntologyPolicy,
    ) -> Self {
        let summary = render_summary(&predicates, &classes, &shapes, &policy);
        Self {
            predicates,
            classes,
            summary,
            policy,
            predicate_iris,
            predicate_shapes: shapes,
            class_iris,
            class_hierarchy,
        }
    }
}

impl LocalResource for OntologyContext {}

/// Render one predicate line: `name (Domain → Range)` when at least one
/// side of the shape is known, plain `name` otherwise.
///
/// `Range` resolves to the richest known constraint, in this order:
/// `range_class` (an entity class), `range_datatype` (an `xsd:` literal
/// datatype, rendered with the `xsd:` prefix), then a kind-only hint
/// (`IRI` or `Literal`) when only `node_kind` is set. Missing sides
/// render as `*` so a half-declared predicate (e.g. only `rdfs:range`)
/// still surfaces the known side.
///
/// Exposed `pub(crate)` so backend-specific agents (e.g. the inspect agent)
/// can reuse the same per-predicate rendering without re-implementing the
/// shape-vs-no-shape branching.
pub(crate) fn render_predicate(name: &str, shape: Option<&PredicateShape>) -> String {
    let Some(s) = shape else {
        return name.to_owned();
    };
    let range = s
        .range_class
        .clone()
        .or_else(|| s.range_datatype.as_ref().map(|dt| format!("xsd:{dt}")))
        .or_else(|| {
            s.node_kind.map(|k| match k {
                NodeKind::Iri => "IRI".to_owned(),
                NodeKind::Literal => "Literal".to_owned(),
            })
        });
    if s.domain_class.is_none() && range.is_none() {
        return name.to_owned();
    }
    let d = s.domain_class.as_deref().unwrap_or("*");
    let r = range.as_deref().unwrap_or("*");
    format!("{name} ({d} → {r})")
}

/// Render the prompt-ready summary block for a curated ontology.
///
/// Shared by the naive and oxigraph agents so the LLM sees identical
/// guidance regardless of backend. The summary is appended after the
/// base extraction prompt by [`build_prompt`].
///
/// Predicates are rendered one-per-line with their `rdfs:domain` /
/// `rdfs:range` hint when known — this is what gives the LLM enough
/// signal to pick e.g. `read_file (Agent → File)` over the catchall
/// when the subject is an agent and the object is a file path.
fn render_summary(
    predicates: &[String],
    classes: &[String],
    shapes: &HashMap<String, PredicateShape>,
    policy: &OntologyPolicy,
) -> String {
    if predicates.is_empty() && classes.is_empty() {
        return String::new();
    }
    use std::fmt::Write;
    let mut s = String::from("Curated ontology:\n");
    if !predicates.is_empty() {
        s.push_str("- predicates (Domain → Range; `*` = unconstrained):\n");
        for p in predicates {
            let _ = writeln!(s, "    - {}", render_predicate(p, shapes.get(p)));
        }
    }
    if !classes.is_empty() {
        s.push_str("- classes: ");
        s.push_str(&classes.join(", "));
        s.push('\n');
    }
    match policy {
        OntologyPolicy::Strict => {
            s.push_str(
                "Only emit triples whose predicate is in the curated list above. \
                 If the input contains no such facts, return an empty `triples` array.",
            );
        }
        OntologyPolicy::CatchAll {
            catchall_predicate, ..
        } => {
            let _ = write!(
                s,
                "Pick the most specific curated predicate whose Domain → Range fits \
                 the subject and object you've identified. Use `{catchall_predicate}` \
                 only as a last resort, after considering and rejecting every other \
                 curated predicate; before emitting it, re-read the list and check \
                 whether any listed predicate's shape matches. Return an empty \
                 `triples` array if the input carries no factual relations at all."
            );
        }
    }
    s
}

/// A triple as produced by the extractor — pre-canonicalization.
///
/// The agent's terminal system stamps these with the trace's `observed_at`
/// and `trace_id` to produce the canonical [`crate::Triple`] that lands in
/// the knowledge graph.
///
/// `subject_type` and `object_type` carry an optional curated-class
/// local-name the LLM picked for either position. The write paths fold
/// these into `rdf:type` triples (and fall back to the predicate's declared
/// `rdfs:domain` / `rdfs:range` when the LLM didn't supply one).
#[derive(Debug, Clone)]
pub struct RawTriple {
    /// Subject entity.
    pub subject: Entity,
    /// Predicate / relation.
    pub predicate: Predicate,
    /// Object entity.
    pub object: Entity,
    /// Optional curated class local-name for the subject, supplied by the
    /// LLM. Drives `<subject> rdf:type <class>` emission in the write paths.
    pub subject_type: Option<String>,
    /// Optional curated class local-name for the object, supplied by the
    /// LLM. Drives `<object> rdf:type <class>` emission in the write paths.
    pub object_type: Option<String>,
}

/// One-shot agent that converts a single [`TraceLine`] into triples and writes
/// them into the shared [`KnowledgeGraph`].
///
/// The agent is stateless across traces — a fresh ephemeral session is spawned
/// per trace by [`crate::ExtractionPlugin`].
#[derive(Debug, Clone, Default)]
pub struct ExtractionAgent;

impl ExtractionAgent {
    /// Stable agent name, used to look up the [`polaris::sessions::AgentTypeId`].
    pub const NAME: &'static str = "ontrace::ExtractionAgent";
}

impl Agent for ExtractionAgent {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn build(&self, graph: &mut Graph) {
        graph.add_system(extract_triples);
        graph.add_system(write_to_kg);
    }

    fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        let model_id = ctx
            .get_resource::<AgentConfig>()
            .map_err(SetupError::new)?
            .model_id
            .clone();
        let llm = ctx
            .get_resource::<ModelRegistry>()
            .map_err(SetupError::new)?
            .llm(&model_id)
            .map_err(SetupError::new)?;
        ctx.insert(AgentLlm(llm));
        // Build the ontology context from a SeedOntology if the plugin
        // inserted one for this session; fall back to an empty default
        // so the shared `extract_triples` system runs in free-form mode.
        let ontology = match ctx.get_resource::<SeedOntology>() {
            Ok(seed) => OntologyContext::from_curated(
                seed.predicates.clone(),
                seed.classes.clone(),
                seed.predicate_shapes.clone(),
                seed.predicate_iris.clone(),
                seed.class_iris.clone(),
                seed.class_hierarchy.clone(),
                seed.policy.clone(),
            ),
            Err(_) => OntologyContext::default(),
        };
        ctx.insert(ontology);
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LLM I/O types
// ─────────────────────────────────────────────────────────────────────────────

/// One triple as the model returns it. `#[derive(JsonSchema)]` is the
/// extraction output contract: the derived schema is handed to the LLM via
/// `LlmRequest::output_schema`, and the reply deserializes straight back
/// into this type.
///
/// The schema is *static* — it cannot encode the per-session curated
/// predicate / class enums. Those constraints are carried in the prompt
/// (via [`OntologyContext::summary`]) and re-applied after parsing by
/// [`normalize_predicate`] / [`curated_class`].
///
/// Exposed `pub(crate)` so the [`crate::agents::repair`] module can reuse
/// the same schema for the repair call without redeclaring the contract.
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ExtractedTriple {
    /// Lowercase canonical entity name for the subject.
    pub(crate) subject: String,
    /// Short `snake_case` verb describing the relation.
    pub(crate) predicate: String,
    /// Lowercase canonical entity name for the object.
    pub(crate) object: String,
    /// Class local-name for the subject; `null` when uncertain.
    #[serde(default)]
    pub(crate) subject_type: Option<String>,
    /// Class local-name for the object; `null` when uncertain.
    #[serde(default)]
    pub(crate) object_type: Option<String>,
}

/// Top-level extraction output: all factual relations present in the line,
/// empty when none.
///
/// Exposed `pub(crate)` for the repair pass — see [`ExtractedTriple`].
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ExtractedTriples {
    /// Every factual relation present in the line; empty if none.
    pub(crate) triples: Vec<ExtractedTriple>,
}

const BASE_EXTRACTION_PROMPT: &str = "\
You extract subject-predicate-object triples from a single line of text.
Each triple captures one factual relation between two named entities.

Rules:
- Use lowercase canonical entity names (e.g. \"alice\" not \"Alice Smith\").
- Prefer snake_case for multi-word entity names (\"request_context\", not \
  \"request-context\" or \"Request Context\").
- For filenames, prefer the basename when unambiguous (\"extraction.rs\", not \
  \"src/agents/extraction.rs\") so the same file references collapse on \
  later observations.
- If a previously-mentioned entity in the input fits, reuse that exact name \
  rather than coining a near-duplicate.
- Predicates are short snake_case verbs (e.g. \"met\", \"works_at\").
- Only extract relations explicitly stated in the input.
- If the line carries no factual relations, return an empty `triples` array.
- Never invent facts that are not present in the input.
- For each triple, fill `subject_type` / `object_type` with a class \
  local-name from the curated list when one clearly applies; omit when \
  uncertain. These drive `rdf:type` assertions in the output.";

/// Re-apply the curated-predicate constraint after parsing.
///
/// The output schema is static (derived from [`ExtractedTriples`]), so the
/// per-session predicate enum is enforced here instead of by the LLM:
///
/// - Free-form mode (`ctx.predicates` empty) — pass through unchanged.
/// - Curated predicate — pass through unchanged.
/// - Non-curated predicate under [`OntologyPolicy::Strict`] — drop the
///   triple (`None`), matching the closed-world guarantee.
/// - Non-curated predicate under [`OntologyPolicy::CatchAll`] — rewrite to
///   the catchall sentinel, so the observation isn't lost.
fn normalize_predicate(predicate: String, ctx: &OntologyContext) -> Option<String> {
    if ctx.predicates.is_empty() || ctx.predicates.iter().any(|p| p == &predicate) {
        return Some(predicate);
    }
    match &ctx.policy {
        OntologyPolicy::Strict => None,
        OntologyPolicy::CatchAll {
            catchall_predicate, ..
        } => Some(catchall_predicate.clone()),
    }
}

/// Re-apply the curated-class constraint to an LLM-supplied
/// `subject_type` / `object_type` after parsing.
///
/// When the ontology curates a class list, a type the LLM coined outside
/// that list is dropped to `None` rather than minting a bare-name
/// `rdf:type` object. Free-form mode (no curated classes) passes through.
fn curated_class(class: Option<String>, ctx: &OntologyContext) -> Option<String> {
    let class = class?;
    if ctx.classes.is_empty() || ctx.classes.iter().any(|c| c == &class) {
        Some(class)
    } else {
        None
    }
}

/// Concatenate the base prompt with `ctx.summary` when one is present.
fn build_prompt(ctx: &OntologyContext) -> String {
    if ctx.summary.is_empty() {
        BASE_EXTRACTION_PROMPT.to_owned()
    } else {
        format!("{BASE_EXTRACTION_PROMPT}\n\n{}", ctx.summary)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Systems
// ─────────────────────────────────────────────────────────────────────────────

/// Call the LLM, parse structured JSON output into [`RawTriple`]s.
///
/// The output schema is derived statically from [`ExtractedTriples`] via
/// `schemars` and handed to the LLM through `LlmRequest::output_schema`.
/// The per-session [`OntologyContext`] shapes the *prompt* (a non-empty
/// context appends a curated-predicate summary) but not the schema — the
/// curated predicate / class constraints are re-applied after parsing by
/// [`normalize_predicate`] and [`curated_class`]. The naive in-memory agent
/// inserts a default-empty context, so the behavior collapses to free-form
/// extraction.
///
/// Errors are caught and logged — a single failed extraction returns an
/// empty Vec rather than aborting the per-trace session.
///
/// Exposed `pub(crate)` so backend-specific agents (e.g. the oxigraph one)
/// can wire the same extraction step into their own `Graph` without
/// duplicating the prompt and schema.
#[system]
pub(crate) async fn extract_triples(
    line: Res<TraceLine>,
    llm: Res<AgentLlm>,
    ontology: Res<OntologyContext>,
    config: Res<AgentConfig>,
    spend: Res<Spend>,
) -> Vec<RawTriple> {
    let output_schema = serde_json::to_value(schemars::schema_for!(ExtractedTriples))
        .expect("ExtractedTriples schema serializes to JSON");
    let request = LlmRequest {
        system: Some(build_prompt(&ontology)),
        messages: vec![Message::user(line.text.clone())],
        tools: None,
        tool_choice: None,
        output_schema: Some(output_schema),
        ..Default::default()
    };

    let response = match llm.generate(request).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, trace = %line.text, "extraction LLM call failed");
            return Vec::new();
        }
    };

    spend.record(
        &config.model_id,
        response.usage.input_tokens,
        response.usage.output_tokens,
    );

    let text = response.text();
    if text.is_empty() {
        return Vec::new();
    }
    let parsed: ExtractedTriples = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, trace = %line.text, body = %text, "extraction parse failed");
            return Vec::new();
        }
    };

    parsed
        .triples
        .into_iter()
        .filter_map(|t| normalize_extracted(t, &ontology))
        .collect()
}

/// Run the same post-parse filters and bridging that
/// [`extract_triples`] applies, on one [`ExtractedTriple`] returned by
/// any LLM call. Used by both the extractor and the repair pass so the
/// curated-predicate / curated-class / IRI / degenerate-triple guards
/// stay consistent across them.
///
/// Returns `None` when the candidate is dropped (IRI-string entity,
/// degenerate s/p/o, or predicate normalised away under
/// [`OntologyPolicy::Strict`]).
pub(crate) fn normalize_extracted(
    t: ExtractedTriple,
    ontology: &OntologyContext,
) -> Option<RawTriple> {
    if looks_like_iri(&t.subject) || looks_like_iri(&t.object) {
        tracing::debug!(
            subject = %t.subject,
            predicate = %t.predicate,
            object = %t.object,
            "dropping triple with IRI-string entity",
        );
        return None;
    }
    let predicate = normalize_predicate(t.predicate, ontology)?;
    if !is_pairwise_distinct(&t.subject, &predicate, &t.object) {
        tracing::debug!(
            subject = %t.subject,
            predicate = %predicate,
            object = %t.object,
            "dropping degenerate triple (subject/predicate/object not pairwise distinct)",
        );
        return None;
    }
    Some(RawTriple {
        subject: Entity::new(t.subject),
        predicate: Predicate::new(predicate),
        object: Entity::new(t.object),
        subject_type: curated_class(t.subject_type, ontology).filter(|s| !s.trim().is_empty()),
        object_type: curated_class(t.object_type, ontology).filter(|s| !s.trim().is_empty()),
    })
}

/// `true` when `subject`, `predicate`, and `object` are all different strings
/// after lowercase/trim normalization.
///
/// Catches the degenerate shapes the LLM occasionally produces on
/// metadata-only lines: self-loops (`request_context.rs read_file
/// request_context.rs`), tautologies (`claude used_model claude`), and
/// predicate-as-entity confusions (`grep called_tool grep`). All three
/// carry no factual content; dropping them upstream of SHACL means the
/// naive backend benefits even without a `TBox`.
fn is_pairwise_distinct(subject: &str, predicate: &str, object: &str) -> bool {
    let s = subject.trim().to_ascii_lowercase();
    let p = predicate.trim().to_ascii_lowercase();
    let o = object.trim().to_ascii_lowercase();
    s != p && s != o && p != o
}

/// `true` when `name` looks like an IRI rather than a lowercase canonical
/// entity name — the LLM occasionally pastes a fully-qualified IRI it saw
/// in the source text as an entity, and those round-trip badly through the
/// `urn:ontrace:e/<name>` minting in [`crate::iri`] (the result is a nested
/// URN like `urn:ontrace:e/urn%3Aontrace%3Ap/related_to`).
///
/// Conservative on purpose: only flags strings whose shape no real entity
/// name would ever take — a scheme prefix or a percent-escape sequence.
fn looks_like_iri(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("urn:")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || name.contains('%')
}

/// Derive the `rdf:type` assertions a single [`RawTriple`] implies, given
/// the per-session [`OntologyContext`].
///
/// Two sources combine, LLM first:
///
/// 1. `raw.subject_type` / `raw.object_type` — class local-names the LLM
///    supplied directly on the triple.
/// 2. Predicate shape fallback — when the LLM didn't fill the corresponding
///    type field, but the predicate has a declared `rdfs:domain` /
///    `rdfs:range`, the shape side stands in.
///
/// Each class local-name is bridged through [`OntologyContext::class_iris`]
/// so the rendered object IRI matches the `TBox` declaration; classes absent
/// from the bridge render as bare entity names. Returns an empty `Vec` when
/// nothing applies — no type field, no shape, no curated class.
///
/// Exposed `pub(crate)` so both backend write paths share one implementation.
pub(crate) fn derive_type_triples(raw: &RawTriple, ontology: &OntologyContext) -> Vec<RawTriple> {
    let shape = ontology.predicate_shapes.get(&raw.predicate.0);
    let subject_class = raw
        .subject_type
        .clone()
        .or_else(|| shape.and_then(|s| s.domain_class.clone()));
    let object_class = raw
        .object_type
        .clone()
        .or_else(|| shape.and_then(|s| s.range_class.clone()));
    let mut out = Vec::new();
    if let Some(class) = subject_class {
        out.push(type_triple(&raw.subject, &class, ontology));
    }
    if let Some(class) = object_class {
        out.push(type_triple(&raw.object, &class, ontology));
    }
    out
}

/// Build one `<entity> rdf:type <class>` [`RawTriple`], substituting the
/// curated class IRI when one is registered. The class name is left as a
/// plain local-name (rendering as `urn:ontrace:e/<name>`) when no bridge
/// entry exists.
fn type_triple(entity: &Entity, class: &str, ontology: &OntologyContext) -> RawTriple {
    let object = match ontology.class_iris.get(class) {
        Some(iri) => Entity::new(iri.clone()),
        None => Entity::new(class.to_owned()),
    };
    RawTriple {
        subject: entity.clone(),
        predicate: Predicate::new(RDF_TYPE),
        object,
        subject_type: None,
        object_type: None,
    }
}

/// Rewrite the predicate to its curated `TBox` IRI when one is registered.
///
/// Used by both write paths so any LLM-emitted local name (`met`, `works_at`,
/// `related_to`, …) lands in the `ABox` at the IRI the `TBox` declares it
/// under (`urn:ontrace:onto/agent/met` etc.), making `?s <tbox-iri> ?o`
/// SPARQL joins work without per-query bridging.
///
/// Lookup order:
/// 1. [`OntologyContext::predicate_iris`] — the full local-name → IRI map
///    built by the seed loader / `TBox` snapshot. Covers every curated
///    predicate including the catchall.
/// 2. The catchall fallback: [`OntologyPolicy::CatchAll::catchall_iri`]
///    is honoured for the sentinel predicate alone, in case a seed was loaded
///    without a full map but still resolved the sentinel.
/// 3. Pass through unchanged — the write path then mints the default
///    `urn:ontrace:p/<name>` via [`crate::iri::predicate_iri_str`] (correct
///    for predicates the LLM invented outside the curated set).
pub(crate) fn bridge_predicate(p: &Predicate, ontology: &OntologyContext) -> Predicate {
    if let Some(iri) = ontology.predicate_iris.get(&p.0) {
        return Predicate::new(iri.clone());
    }
    if let OntologyPolicy::CatchAll {
        catchall_predicate,
        catchall_iri: Some(iri),
    } = &ontology.policy
        && &p.0 == catchall_predicate
    {
        return Predicate::new(iri.clone());
    }
    p.clone()
}

/// Write extracted triples into the global [`KnowledgeGraph`], stamping each
/// with the trace's observation time and id.
///
/// Before each factual write, consults [`Aliases`] for the subject and
/// object: a first-detection alias triggers an extra `owl:sameAs` triple,
/// recording the identity link without merging the surface forms.
///
/// The predicate is run through [`bridge_predicate`] so catchall observations
/// land at the curated IRI declared in the `TBox` (when one is registered).
/// Sightings retain the raw LLM-emitted predicate so the markdown report
/// still reads in local-name form.
///
/// Each raw triple is also classified into [`OntologyGap`] before bridging —
/// a catchall use or an uncurated predicate is feedback signal for ontology
/// iteration, captured here while the originating trace line is in scope.
#[system]
async fn write_to_kg(
    triples: Out<Vec<RawTriple>>,
    line: Res<TraceLine>,
    kg: Res<KnowledgeGraph>,
    aliases: Res<Aliases>,
    sightings: Res<Sightings>,
    ontology: Res<OntologyContext>,
    gap: Res<OntologyGap>,
) -> usize {
    let mut count = 0;
    for raw in triples.iter() {
        record_predicate_gaps(&gap, &ontology, raw, &line.text);
        for entity in [&raw.subject, &raw.object] {
            if let Resolution::EmitSameAs { canonical } = aliases.resolve(entity) {
                kg.insert(Triple {
                    subject: entity.clone(),
                    predicate: Predicate::new(OWL_SAME_AS),
                    object: canonical,
                    observed_at: line.observed_at,
                    trace_id: line.trace_id.clone(),
                });
            }
        }
        kg.insert(Triple {
            subject: raw.subject.clone(),
            predicate: bridge_predicate(&raw.predicate, &ontology),
            object: raw.object.clone(),
            observed_at: line.observed_at,
            trace_id: line.trace_id.clone(),
        });
        for type_t in derive_type_triples(raw, &ontology) {
            kg.insert(Triple {
                subject: type_t.subject,
                predicate: type_t.predicate,
                object: type_t.object,
                observed_at: line.observed_at,
                trace_id: line.trace_id.clone(),
            });
        }
        count += 1;
    }
    sightings.push(Sighting {
        line: line.text.clone(),
        observed_at: line.observed_at,
        trace_id: line.trace_id.clone(),
        triples: triples.to_vec(),
    });
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_hierarchy_is_a_is_reflexive() {
        let h = ClassHierarchy::default();
        assert!(h.is_a("Foo", "Foo"));
        // Reflexivity holds even when the class has no declared parent.
        assert!(h.is_a("Disconnected", "Disconnected"));
    }

    #[test]
    fn class_hierarchy_is_a_walks_direct_parent() {
        let mut h = ClassHierarchy::new();
        h.add_subclass("ToolCall", "Event");
        assert!(h.is_a("ToolCall", "Event"));
        assert!(!h.is_a("Event", "ToolCall"));
    }

    #[test]
    fn class_hierarchy_is_a_walks_transitive_chain() {
        let mut h = ClassHierarchy::new();
        h.add_subclass("ToolCall", "Span");
        h.add_subclass("Span", "Event");
        h.add_subclass("Event", "Actor");
        assert!(h.is_a("ToolCall", "Event"));
        assert!(h.is_a("ToolCall", "Actor"));
        assert!(h.is_a("Span", "Actor"));
    }

    #[test]
    fn class_hierarchy_is_a_returns_false_for_disjoint_classes() {
        let mut h = ClassHierarchy::new();
        h.add_subclass("ToolCall", "Event");
        h.add_subclass("Metric", "Measurement");
        assert!(!h.is_a("ToolCall", "Measurement"));
        assert!(!h.is_a("Metric", "Event"));
        assert!(!h.is_a("Unknown", "Event"));
    }

    #[test]
    fn class_hierarchy_is_a_is_cycle_safe() {
        // Malformed TBox: A subClassOf B and B subClassOf A. The walk
        // must terminate and conclude `is_a` against a non-ancestor.
        let mut h = ClassHierarchy::new();
        h.add_subclass("A", "B");
        h.add_subclass("B", "A");
        assert!(h.is_a("A", "B"));
        assert!(h.is_a("B", "A"));
        assert!(!h.is_a("A", "C"));
    }

    #[test]
    fn class_hierarchy_add_subclass_is_idempotent() {
        let mut h = ClassHierarchy::new();
        h.add_subclass("ToolCall", "Event");
        h.add_subclass("ToolCall", "Event");
        // Duplicate parent declaration must not change reachability.
        assert!(h.is_a("ToolCall", "Event"));
    }

    #[test]
    fn class_hierarchy_extend_unions_edges() {
        let mut a = ClassHierarchy::new();
        a.add_subclass("ToolCall", "Event");
        let mut b = ClassHierarchy::new();
        b.add_subclass("Metric", "Measurement");
        a.extend(&b);
        assert!(a.is_a("ToolCall", "Event"));
        assert!(a.is_a("Metric", "Measurement"));
    }

    fn ctx(predicates: &[&str], policy: OntologyPolicy) -> OntologyContext {
        OntologyContext {
            predicates: predicates.iter().map(|s| (*s).to_owned()).collect(),
            classes: Vec::new(),
            summary: if predicates.is_empty() {
                String::new()
            } else {
                format!(
                    "Curated ontology:\n- predicates: {}\n",
                    predicates.join(", ")
                )
            },
            policy,
            predicate_iris: HashMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn normalize_predicate_passes_through_in_free_form_mode() {
        let c = ctx(&[], OntologyPolicy::Strict);
        assert_eq!(
            normalize_predicate("invented".to_owned(), &c),
            Some("invented".to_owned()),
        );
    }

    #[test]
    fn normalize_predicate_passes_through_curated_predicate() {
        let c = ctx(&["met", "works_at"], OntologyPolicy::Strict);
        assert_eq!(
            normalize_predicate("met".to_owned(), &c),
            Some("met".to_owned()),
        );
    }

    #[test]
    fn normalize_predicate_drops_non_curated_under_strict() {
        let c = ctx(&["met"], OntologyPolicy::Strict);
        assert_eq!(normalize_predicate("deployed_to".to_owned(), &c), None);
    }

    #[test]
    fn normalize_predicate_rewrites_non_curated_to_sentinel_under_catchall() {
        let c = ctx(
            &["met"],
            OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: None,
            },
        );
        assert_eq!(
            normalize_predicate("deployed_to".to_owned(), &c),
            Some("related_to".to_owned()),
        );
    }

    #[test]
    fn normalize_predicate_keeps_sentinel_emitted_directly() {
        // The sentinel is not in the curated list, but emitting it directly
        // must round-trip — it's the catchall, not a gap.
        let c = ctx(
            &["met"],
            OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: None,
            },
        );
        assert_eq!(
            normalize_predicate("related_to".to_owned(), &c),
            Some("related_to".to_owned()),
        );
    }

    #[test]
    fn curated_class_passes_through_in_free_form_mode() {
        let c = OntologyContext::default();
        assert_eq!(
            curated_class(Some("Person".to_owned()), &c),
            Some("Person".to_owned()),
        );
    }

    #[test]
    fn curated_class_passes_through_curated_class() {
        let c = OntologyContext {
            classes: vec!["Person".to_owned(), "File".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            curated_class(Some("File".to_owned()), &c),
            Some("File".to_owned()),
        );
    }

    #[test]
    fn curated_class_drops_non_curated_class() {
        let c = OntologyContext {
            classes: vec!["Person".to_owned()],
            ..Default::default()
        };
        assert_eq!(curated_class(Some("Spaceship".to_owned()), &c), None);
        assert_eq!(curated_class(None, &c), None);
    }

    #[test]
    fn bridge_predicate_substitutes_catchall_to_iri() {
        let ontology = OntologyContext {
            predicates: vec!["related_to".into()],
            classes: Vec::new(),
            summary: String::new(),
            predicate_iris: HashMap::new(),
            policy: OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: Some("urn:ontrace:onto/core/related_to".into()),
            },
            ..Default::default()
        };
        let bridged = bridge_predicate(&Predicate::new("related_to"), &ontology);
        assert_eq!(bridged.0, "urn:ontrace:onto/core/related_to");
    }

    #[test]
    fn bridge_predicate_passes_uncurated_predicate_through() {
        // No bridge entry for `met`, catchall handles `related_to` only —
        // `met` passes through and the write path will mint
        // `urn:ontrace:p/met`.
        let ontology = OntologyContext {
            predicates: vec!["met".into(), "related_to".into()],
            classes: Vec::new(),
            summary: String::new(),
            predicate_iris: HashMap::new(),
            policy: OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: Some("urn:ontrace:onto/core/related_to".into()),
            },
            ..Default::default()
        };
        let bridged = bridge_predicate(&Predicate::new("met"), &ontology);
        assert_eq!(bridged.0, "met", "uncurated predicate must be untouched");
    }

    #[test]
    fn bridge_predicate_noop_when_iri_unset() {
        let ontology = OntologyContext {
            predicates: vec!["related_to".into()],
            classes: Vec::new(),
            summary: String::new(),
            predicate_iris: HashMap::new(),
            policy: OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: None,
            },
            ..Default::default()
        };
        let bridged = bridge_predicate(&Predicate::new("related_to"), &ontology);
        assert_eq!(bridged.0, "related_to");
    }

    #[test]
    fn bridge_predicate_noop_under_strict_policy() {
        let ontology = OntologyContext {
            predicates: vec!["related_to".into()],
            classes: Vec::new(),
            summary: String::new(),
            predicate_iris: HashMap::new(),
            policy: OntologyPolicy::Strict,
            ..Default::default()
        };
        let bridged = bridge_predicate(&Predicate::new("related_to"), &ontology);
        assert_eq!(bridged.0, "related_to");
    }

    #[test]
    fn prompt_with_empty_ontology_is_base_prompt_only() {
        let s = build_prompt(&OntologyContext::default());
        assert_eq!(s, BASE_EXTRACTION_PROMPT);
    }

    #[test]
    fn prompt_appends_summary_when_present() {
        let c = ctx(&["met"], OntologyPolicy::Strict);
        let s = build_prompt(&c);
        assert!(s.starts_with(BASE_EXTRACTION_PROMPT));
        assert!(s.contains("Curated ontology"));
    }

    #[test]
    fn looks_like_iri_flags_urn_and_http_and_percent_strings() {
        assert!(looks_like_iri("urn:ontrace:p/related_to"));
        assert!(looks_like_iri("http://example.org/foo"));
        assert!(looks_like_iri("https://example.org/foo"));
        assert!(looks_like_iri("URN:ontrace:p/x")); // case-insensitive scheme
        assert!(looks_like_iri("alice%20smith")); // percent-encoded
    }

    #[test]
    fn looks_like_iri_passes_canonical_names() {
        assert!(!looks_like_iri("alice"));
        assert!(!looks_like_iri("47b6bdf"));
        assert!(!looks_like_iri("commit message style"));
        assert!(!looks_like_iri("claude-opus-4-7"));
        assert!(!looks_like_iri("src/agents/extraction.rs"));
    }

    #[test]
    fn pairwise_distinct_rejects_self_loops_and_tautologies() {
        assert!(!is_pairwise_distinct("foo.rs", "read_file", "foo.rs"));
        assert!(!is_pairwise_distinct("claude", "used_model", "claude"));
        assert!(!is_pairwise_distinct("grep", "called_tool", "grep"));
        assert!(!is_pairwise_distinct("read", "called_tool", "read"));
    }

    #[test]
    fn pairwise_distinct_is_case_and_whitespace_tolerant() {
        // Case folding + trim, because we drop before alias normalization.
        assert!(!is_pairwise_distinct(" Alice ", "knows", "alice"));
    }

    #[test]
    fn pairwise_distinct_accepts_well_formed_triples() {
        assert!(is_pairwise_distinct("alice", "met", "bob"));
        assert!(is_pairwise_distinct("agent", "called_tool", "grep"));
    }

    #[test]
    fn prompt_warns_against_catchall_overuse() {
        let c = OntologyContext::from_curated(
            vec!["met".to_owned()],
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ClassHierarchy::default(),
            OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: None,
            },
        );
        // The catchall guidance must steer the LLM toward curated predicates
        // rather than encouraging fallback at the first hesitation.
        assert!(
            c.summary.contains("last resort"),
            "summary should discourage catchall as default; got: {}",
            c.summary
        );
    }

    #[test]
    fn render_predicate_falls_back_to_datatype_then_node_kind() {
        // range_datatype renders with xsd: prefix.
        let latency = PredicateShape {
            domain_class: Some("Event".to_owned()),
            range_datatype: Some("decimal".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            render_predicate("has_latency", Some(&latency)),
            "has_latency (Event → xsd:decimal)",
        );
        // node_kind alone renders as IRI / Literal hint.
        let produced = PredicateShape {
            domain_class: Some("Agent".to_owned()),
            node_kind: Some(NodeKind::Iri),
            ..Default::default()
        };
        assert_eq!(
            render_predicate("produced", Some(&produced)),
            "produced (Agent → IRI)",
        );
        // range_class wins over node_kind when both are set.
        let read = PredicateShape {
            domain_class: Some("Agent".to_owned()),
            range_class: Some("File".to_owned()),
            node_kind: Some(NodeKind::Iri),
            ..Default::default()
        };
        assert_eq!(
            render_predicate("read_file", Some(&read)),
            "read_file (Agent → File)",
        );
    }

    #[test]
    fn render_predicate_with_only_node_kind_no_domain_renders_kind_only() {
        let shape = PredicateShape {
            node_kind: Some(NodeKind::Literal),
            ..Default::default()
        };
        assert_eq!(render_predicate("note", Some(&shape)), "note (* → Literal)",);
    }

    #[test]
    fn summary_renders_predicate_shape_when_known() {
        let mut shapes = HashMap::new();
        shapes.insert(
            "read_file".to_owned(),
            PredicateShape {
                domain_class: Some("Agent".to_owned()),
                range_class: Some("File".to_owned()),
                ..Default::default()
            },
        );
        let ctx = OntologyContext::from_curated(
            vec!["read_file".to_owned(), "met".to_owned()],
            vec!["Agent".to_owned(), "File".to_owned()],
            shapes,
            HashMap::new(),
            HashMap::new(),
            ClassHierarchy::default(),
            OntologyPolicy::Strict,
        );
        assert!(ctx.summary.contains("read_file (Agent → File)"));
        // Predicates with no shape still appear, as a bare name.
        assert!(ctx.summary.contains("- met"));
    }

    #[test]
    fn bridge_uses_predicate_iri_map_for_any_curated_predicate() {
        let mut iris = HashMap::new();
        iris.insert("met".to_owned(), "urn:ontrace:onto/agent/met".to_owned());
        let ontology = OntologyContext {
            predicates: vec!["met".into(), "related_to".into()],
            classes: Vec::new(),
            summary: String::new(),
            predicate_iris: iris,
            policy: OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: Some("urn:ontrace:onto/core/related_to".into()),
            },
            ..Default::default()
        };
        let bridged = bridge_predicate(&Predicate::new("met"), &ontology);
        assert_eq!(bridged.0, "urn:ontrace:onto/agent/met");
    }

    #[test]
    fn bridge_predicate_iri_map_wins_over_catchall_fallback() {
        // If the map carries the catchall too, the map wins — there's no
        // ambiguity, just one bridging mechanism.
        let mut iris = HashMap::new();
        iris.insert(
            "related_to".to_owned(),
            "urn:ontrace:onto/core/related_to".to_owned(),
        );
        let ontology = OntologyContext {
            predicates: vec!["related_to".into()],
            classes: Vec::new(),
            summary: String::new(),
            predicate_iris: iris,
            policy: OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: Some("urn:legacy/related_to".into()),
            },
            ..Default::default()
        };
        let bridged = bridge_predicate(&Predicate::new("related_to"), &ontology);
        assert_eq!(bridged.0, "urn:ontrace:onto/core/related_to");
    }

    fn raw_t(s: &str, p: &str, o: &str) -> RawTriple {
        RawTriple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            subject_type: None,
            object_type: None,
        }
    }

    fn shape_ctx(p: &str, domain: Option<&str>, range: Option<&str>) -> OntologyContext {
        let mut shapes = HashMap::new();
        shapes.insert(
            p.to_owned(),
            PredicateShape {
                domain_class: domain.map(str::to_owned),
                range_class: range.map(str::to_owned),
                ..Default::default()
            },
        );
        OntologyContext {
            predicates: vec![p.to_owned()],
            predicate_shapes: shapes,
            ..Default::default()
        }
    }

    #[test]
    fn derive_type_triples_from_predicate_shape_when_llm_silent() {
        let ontology = shape_ctx("works_at", Some("Person"), Some("Organization"));
        let raw = raw_t("alice", "works_at", "acme");
        let types = derive_type_triples(&raw, &ontology);
        assert_eq!(types.len(), 2);
        assert!(types.iter().all(|t| t.predicate.0 == crate::iri::RDF_TYPE));
        let mut classes: Vec<String> = types.iter().map(|t| t.object.0.clone()).collect();
        classes.sort();
        assert_eq!(
            classes,
            vec!["Organization".to_string(), "Person".to_string()]
        );
    }

    #[test]
    fn derive_type_triples_prefers_llm_provided_types() {
        let ontology = shape_ctx("works_at", Some("Person"), Some("Organization"));
        let mut raw = raw_t("alice", "works_at", "acme");
        raw.subject_type = Some("Engineer".to_owned());
        let types = derive_type_triples(&raw, &ontology);
        // LLM-supplied "Engineer" wins over shape-derived "Person" for the subject;
        // object falls back to range "Organization".
        let subject_class = types
            .iter()
            .find(|t| t.subject == raw.subject)
            .expect("subject type present");
        assert_eq!(subject_class.object.0, "Engineer");
        let object_class = types
            .iter()
            .find(|t| t.subject == raw.object)
            .expect("object type present");
        assert_eq!(object_class.object.0, "Organization");
    }

    #[test]
    fn derive_type_triples_skips_missing_shape_sides() {
        // Only range declared — only the object gets a rdf:type emission.
        let ontology = shape_ctx("produced", None, Some("Artifact"));
        let raw = raw_t("agent42", "produced", "report.md");
        let types = derive_type_triples(&raw, &ontology);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].subject.0, "report.md");
        assert_eq!(types[0].object.0, "Artifact");
    }

    #[test]
    fn derive_type_triples_bridges_class_to_curated_iri() {
        let mut ontology = shape_ctx("works_at", Some("Person"), None);
        ontology.class_iris.insert(
            "Person".to_owned(),
            "urn:ontrace:onto/agent/Person".to_owned(),
        );
        let raw = raw_t("alice", "works_at", "acme");
        let types = derive_type_triples(&raw, &ontology);
        assert_eq!(types.len(), 1);
        // Object position carries the curated IRI directly so the write path
        // renders it as `<urn:ontrace:onto/agent/Person>`, not the entity-mint
        // fallback `<urn:ontrace:e/Person>`.
        assert_eq!(types[0].object.0, "urn:ontrace:onto/agent/Person");
    }

    #[test]
    fn derive_type_triples_empty_when_no_shape_and_no_llm_types() {
        let ontology = OntologyContext::default();
        let raw = raw_t("alice", "met", "bob");
        assert!(derive_type_triples(&raw, &ontology).is_empty());
    }

    #[test]
    fn bridge_falls_back_to_default_for_uncurated_predicate() {
        let mut iris = HashMap::new();
        iris.insert("met".to_owned(), "urn:ontrace:onto/agent/met".to_owned());
        let ontology = OntologyContext {
            predicates: vec!["met".into()],
            classes: Vec::new(),
            summary: String::new(),
            predicate_iris: iris,
            policy: OntologyPolicy::Strict,
            ..Default::default()
        };
        // An LLM-emitted local name outside the curated set passes through
        // unchanged — the write path will mint `urn:ontrace:p/<name>`.
        let bridged = bridge_predicate(&Predicate::new("invented_by_llm"), &ontology);
        assert_eq!(bridged.0, "invented_by_llm");
    }
}
