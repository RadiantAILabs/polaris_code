//! Per-trace extraction agent that writes into an [`OxigraphStore`].
//!
//! Parallel to [`super::extraction::ExtractionAgent`]: same LLM call, same
//! prompt, same alias canonicalization — but emits RDF quads into the
//! `ABox` named graph ([`OxigraphStore::data_graph`]) instead of the
//! in-memory `KnowledgeGraph`.
//!
//! The two agents are wired through separate plugins
//! ([`crate::ExtractionPlugin`] vs [`crate::OxigraphExtractionPlugin`]);
//! pick one when constructing the server. There is no shared abstraction —
//! they're independent backends that happen to reuse the LLM extraction
//! step ([`super::extraction::extract_triples`]).

use crate::agents::extraction::{
    AgentConfig, AgentLlm, ClassHierarchy, NodeKind, OntologyContext, OntologyPolicy,
    PredicateShape, RawTriple, bridge_predicate, derive_type_triples, extract_triples,
};
use crate::agents::repair::repair_triples;
use crate::agents::shacl::{ValidatedTriples, validate_triples};
use crate::iri::{OWL_SAME_AS_IRI, entity_iri_str, predicate_iri_str};
use crate::oxigraph::OxigraphStore;
use crate::resources::aliases::{Aliases, Resolution};
use crate::resources::kg::Entity;
use crate::resources::ontology_gap::{OntologyGap, record_predicate_gaps};
use crate::resources::sightings::{Sighting, Sightings};
use crate::resources::trace_line::TraceLine;
use crate::scalar::{self, ScalarKind};
use oxigraph::model::{
    BlankNode, Literal, NamedNode, NamedNodeRef, Quad, Subject, Term,
    vocab::{rdf, xsd},
};
use oxigraph::sparql::QueryResults;
use polaris::agent::{Agent, SetupError};
use polaris::graph::Graph;
use polaris::models::ModelRegistry;
use polaris::prelude::Out;
use polaris::system::param::{Res, SystemContext};
use polaris::system::resource::LocalResource;
use polaris::system::system;
use std::collections::HashMap;

/// Predicate IRI for the observation timestamp on a reified statement.
const ONT_OBSERVED_AT: &str = "urn:ontrace:observedAt";

/// Predicate IRI for the upstream trace identifier on a reified statement.
const ONT_TRACE_ID: &str = "urn:ontrace:traceId";

/// Per-session policy knob inserted by [`crate::OxigraphExtractionPlugin`].
///
/// Held as a `LocalResource` so each session sees the plugin-wide setting
/// without coupling it to [`AgentConfig`] (which the naive in-memory agent
/// also reads).
#[derive(Debug, Clone, Default)]
pub struct OntologyConfig {
    /// Behavior when an extracted relation doesn't match a curated predicate.
    pub policy: OntologyPolicy,
}

impl OntologyConfig {
    /// Construct with `policy`.
    #[must_use]
    pub fn new(policy: OntologyPolicy) -> Self {
        Self { policy }
    }
}

impl LocalResource for OntologyConfig {}

/// One-shot agent that converts a single [`TraceLine`] into quads and writes
/// them into the shared [`OxigraphStore`]'s data (`ABox`) named graph.
///
/// Stateless across traces — durable state lives in [`OxigraphStore`] (the
/// RDF dataset itself) and [`Aliases`] (canonicalization table).
#[derive(Debug, Clone, Default)]
pub struct OxigraphExtractionAgent;

impl OxigraphExtractionAgent {
    /// Stable agent name used to look up the registered `AgentTypeId`.
    pub const NAME: &'static str = "ontrace::OxigraphExtractionAgent";
}

impl Agent for OxigraphExtractionAgent {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn build(&self, graph: &mut Graph) {
        graph.add_system(extract_triples);
        graph.add_system(validate_triples);
        graph.add_system(repair_triples);
        graph.add_system(write_to_oxigraph);
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
        // Policy is set by the plugin worker; fall back to `Strict` if absent
        // (e.g. tests that call `run_oneshot` directly without the plugin).
        let policy = ctx
            .get_resource::<OntologyConfig>()
            .map(|c| c.policy.clone())
            .unwrap_or_default();
        let store = ctx
            .get_resource::<OxigraphStore>()
            .map_err(SetupError::new)?
            .clone();
        let ontology = snapshot_ontology(&store, policy).map_err(SetupError::new)?;
        ctx.insert(AgentLlm::new(llm));
        ctx.insert(ontology);
        Ok(())
    }
}

/// Build an [`OntologyContext`] by querying `store`'s ontology named graph.
///
/// Extracts predicates declared `rdf:Property`, `owl:ObjectProperty`, or
/// `owl:DatatypeProperty`, and classes declared `rdfs:Class` or `owl:Class`.
/// Local names are derived by stripping the IRI past the last `#` or `/`.
/// The prompt-ready `summary` is pre-rendered so the per-line extraction
/// path doesn't re-format on every call.
///
/// Predicates also carry `rdfs:domain` / `rdfs:range` hints when declared,
/// driving the `name (Domain → Range)` annotations in the prompt summary.
///
/// When `policy` is [`OntologyPolicy::CatchAll`] with no explicit IRI, the
/// sentinel's declared `TBox` IRI is looked up and attached to the policy so
/// catchall triples write to the curated IRI instead of the default
/// `urn:ontrace:p/<sentinel>` namespace.
///
/// # Errors
///
/// Returns the underlying SPARQL evaluation error if any query fails. An
/// empty ontology (no curated predicates) returns a default-constructed
/// [`OntologyContext`] — the extractor falls back to free-form mode in
/// that case.
pub fn snapshot_ontology(
    store: &OxigraphStore,
    policy: OntologyPolicy,
) -> Result<OntologyContext, oxigraph::sparql::EvaluationError> {
    let graph_iri = store.ontology_graph().as_str();
    let (predicates, classes, predicate_shapes, predicate_iris, class_iris, class_hierarchy) =
        snapshot_curated(store)?;

    let policy = match policy {
        OntologyPolicy::CatchAll {
            catchall_predicate,
            catchall_iri: None,
        } => {
            // The full IRI map already covers the sentinel — but the
            // `catchall_iri` field is the fallback consulted when the map
            // lacks it, so populate it too.
            let catchall_iri = predicate_iris
                .get(&catchall_predicate)
                .cloned()
                .or(query_predicate_iri(store, graph_iri, &catchall_predicate)?);
            OntologyPolicy::CatchAll {
                catchall_predicate,
                catchall_iri,
            }
        }
        other => other,
    };

    Ok(OntologyContext::from_curated(
        predicates,
        classes,
        predicate_shapes,
        predicate_iris,
        class_iris,
        class_hierarchy,
        policy,
    ))
}

/// Raw `TBox` snapshot tuple: predicates, classes, per-predicate domain /
/// range hints, per-predicate local-name → IRI, per-class local-name → IRI,
/// and the transitive `rdfs:subClassOf` index.
pub(crate) type CuratedSnapshot = (
    Vec<String>,
    Vec<String>,
    HashMap<String, PredicateShape>,
    HashMap<String, String>,
    HashMap<String, String>,
    ClassHierarchy,
);

/// Raw `TBox` snapshot: predicates, classes, per-predicate domain/range
/// hints, the local-name → IRI bridges for predicates and classes, and the
/// `rdfs:subClassOf` hierarchy — with no policy / catchall-IRI logic mixed
/// in.
///
/// The extraction agent wraps this with policy-aware bridging via
/// [`snapshot_ontology`]; the inspect agent reuses it to feed the curated
/// schema into its system prompt without dragging the extraction-shaped
/// policy guidance along.
///
/// # Errors
///
/// Propagates SPARQL evaluation errors from the queries against the
/// ontology graph. An empty `TBox` returns empty collections across the board.
pub(crate) fn snapshot_curated(
    store: &OxigraphStore,
) -> Result<CuratedSnapshot, oxigraph::sparql::EvaluationError> {
    let graph_iri = store.ontology_graph().as_str();
    let predicate_iris = query_predicate_iris(store, graph_iri)?;
    let mut predicates: Vec<String> = predicate_iris.keys().cloned().collect();
    predicates.sort();
    predicates.dedup();
    let class_iris = query_class_iris(store, graph_iri)?;
    let mut classes: Vec<String> = class_iris.keys().cloned().collect();
    classes.sort();
    classes.dedup();
    let predicate_shapes = query_predicate_shapes(store, graph_iri)?;
    let class_hierarchy = query_class_hierarchy(store, graph_iri)?;
    Ok((
        predicates,
        classes,
        predicate_shapes,
        predicate_iris,
        class_iris,
        class_hierarchy,
    ))
}

/// SPARQL the `TBox` for every declared property, returning a
/// `local-name → IRI` map.
///
/// First IRI per local-name wins. Powers the `ABox` ↔ `TBox` predicate
/// bridge consumed by [`crate::agents::extraction::bridge_predicate`].
fn query_predicate_iris(
    store: &OxigraphStore,
    graph_iri: &str,
) -> Result<HashMap<String, String>, oxigraph::sparql::EvaluationError> {
    use oxigraph::model::Term;
    use oxigraph::sparql::QueryResults;

    let sparql = format!(
        "SELECT DISTINCT ?p WHERE {{ GRAPH <{graph_iri}> {{ \
         ?p a ?t . FILTER(?t IN (\
         <http://www.w3.org/1999/02/22-rdf-syntax-ns#Property>, \
         <http://www.w3.org/2002/07/owl#ObjectProperty>, \
         <http://www.w3.org/2002/07/owl#DatatypeProperty>)) }} }}"
    );
    let results = store.query(&sparql)?;
    let mut out: HashMap<String, String> = HashMap::new();
    if let QueryResults::Solutions(solutions) = results {
        for sol in solutions {
            let sol = sol?;
            let Some(Term::NamedNode(node)) = sol.get("p") else {
                continue;
            };
            let iri = node.as_str();
            let name = local_name(iri);
            out.entry(name).or_insert_with(|| iri.to_owned());
        }
    }
    Ok(out)
}

/// Collect every per-predicate shape constraint the `TBox` carries, from
/// both `rdfs:domain` / `rdfs:range` and SHACL property shapes.
///
/// Runs two SPARQL passes against the ontology named graph and merges them
/// into a single `local-name → PredicateShape` map. First declaration wins
/// per field — the validator and the prompt then read identical data, with
/// no separate `ShapeIndex` cache to drift out of sync.
///
/// Pass 1 — `rdfs:domain` / `rdfs:range`:
///
/// - `rdfs:domain` populates [`PredicateShape::domain_class`].
/// - `rdfs:range` populates [`PredicateShape::range_datatype`] when the
///   range IRI sits under the `xsd:` namespace, otherwise
///   [`PredicateShape::range_class`].
///
/// Pass 2 — SHACL property shapes attached to any `sh:NodeShape` (the
/// shape's `sh:targetClass`, when declared, refines `domain_class` only
/// when the predicate is still domain-less from pass 1; SHACL fills the
/// gaps left by `rdfs:*`, never overrides):
///
/// - `sh:datatype` → `range_datatype`.
/// - `sh:class`    → `range_class`.
/// - `sh:nodeKind` → `node_kind`.
///
/// Predicates that produce no useful field after both passes are omitted —
/// the prompt renders them as bare names and the validator is a
/// pass-through for them.
fn query_predicate_shapes(
    store: &OxigraphStore,
    graph_iri: &str,
) -> Result<HashMap<String, PredicateShape>, oxigraph::sparql::EvaluationError> {
    let mut out: HashMap<String, PredicateShape> = HashMap::new();

    let rdfs_pass = format!(
        "SELECT ?p ?d ?r WHERE {{ GRAPH <{graph_iri}> {{ \
         ?p a ?t . FILTER(?t IN (\
         <http://www.w3.org/2002/07/owl#ObjectProperty>, \
         <http://www.w3.org/2002/07/owl#DatatypeProperty>, \
         <http://www.w3.org/1999/02/22-rdf-syntax-ns#Property>)) . \
         OPTIONAL {{ ?p <http://www.w3.org/2000/01/rdf-schema#domain> ?d }} \
         OPTIONAL {{ ?p <http://www.w3.org/2000/01/rdf-schema#range> ?r }} }} }}"
    );
    if let QueryResults::Solutions(solutions) = store.query(&rdfs_pass)? {
        for sol in solutions {
            let sol = sol?;
            let Some(Term::NamedNode(p_node)) = sol.get("p") else {
                continue;
            };
            let p_name = local_name(p_node.as_str());
            let domain = sol.get("d").and_then(|t| match t {
                Term::NamedNode(n) => Some(local_name(n.as_str())),
                _ => None,
            });
            let (range_class, range_datatype) = sol
                .get("r")
                .map(|t| match t {
                    Term::NamedNode(n) => split_range(n.as_str()),
                    _ => (None, None),
                })
                .unwrap_or((None, None));
            if domain.is_none() && range_class.is_none() && range_datatype.is_none() {
                continue;
            }
            let entry = out.entry(p_name).or_default();
            if entry.domain_class.is_none() {
                entry.domain_class = domain;
            }
            if entry.range_class.is_none() {
                entry.range_class = range_class;
            }
            if entry.range_datatype.is_none() {
                entry.range_datatype = range_datatype;
            }
        }
    }

    let shacl_pass = format!(
        "SELECT ?p ?dt ?cls ?nk ?target WHERE {{ GRAPH <{graph_iri}> {{ \
         ?shape a <http://www.w3.org/ns/shacl#NodeShape> ; \
                <http://www.w3.org/ns/shacl#property> ?prop . \
         ?prop <http://www.w3.org/ns/shacl#path> ?p . \
         OPTIONAL {{ ?shape <http://www.w3.org/ns/shacl#targetClass> ?target }} \
         OPTIONAL {{ ?prop <http://www.w3.org/ns/shacl#datatype> ?dt }} \
         OPTIONAL {{ ?prop <http://www.w3.org/ns/shacl#class> ?cls }} \
         OPTIONAL {{ ?prop <http://www.w3.org/ns/shacl#nodeKind> ?nk }} }} }}"
    );
    if let QueryResults::Solutions(solutions) = store.query(&shacl_pass)? {
        for sol in solutions {
            let sol = sol?;
            let Some(Term::NamedNode(p_node)) = sol.get("p") else {
                continue;
            };
            let entry = out.entry(local_name(p_node.as_str())).or_default();
            if let Some(Term::NamedNode(target)) = sol.get("target")
                && entry.domain_class.is_none()
            {
                entry.domain_class = Some(local_name(target.as_str()));
            }
            if let Some(Term::NamedNode(dt)) = sol.get("dt")
                && entry.range_datatype.is_none()
                && let Some(local) = dt.as_str().strip_prefix(XSD_NS)
            {
                entry.range_datatype = Some(local.to_owned());
            }
            if let Some(Term::NamedNode(cls)) = sol.get("cls")
                && entry.range_class.is_none()
            {
                entry.range_class = Some(local_name(cls.as_str()));
            }
            if let Some(Term::NamedNode(nk)) = sol.get("nk")
                && entry.node_kind.is_none()
            {
                entry.node_kind = parse_node_kind(nk.as_str());
            }
        }
    }

    // Drop entries where every field is None — the SHACL pass may have
    // inserted a default entry for a predicate that turned out to carry
    // no usable constraints.
    out.retain(|_, s| {
        s.domain_class.is_some()
            || s.range_class.is_some()
            || s.range_datatype.is_some()
            || s.node_kind.is_some()
    });
    Ok(out)
}

/// XSD namespace IRI prefix — a range or `sh:datatype` IRI under this
/// namespace is treated as a literal datatype constraint, not a class.
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";

/// SHACL namespace IRI prefix — used to recognise the `sh:nodeKind`
/// value IRIs (`sh:IRI`, `sh:Literal`, …).
const SH_NS: &str = "http://www.w3.org/ns/shacl#";

/// Split an `rdfs:range` IRI into `(range_class, range_datatype)`.
///
/// Returns the local name in `range_datatype` slot when the IRI sits
/// under the `xsd:` namespace, in `range_class` slot otherwise. Exactly
/// one side is `Some` for any input — the caller writes it into the
/// matching field of [`PredicateShape`].
fn split_range(iri: &str) -> (Option<String>, Option<String>) {
    if let Some(local) = iri.strip_prefix(XSD_NS) {
        (None, Some(local.to_owned()))
    } else {
        (Some(local_name(iri)), None)
    }
}

/// Map a SHACL `sh:nodeKind` value IRI onto [`NodeKind`]. Kinds that
/// admit blank nodes collapse onto their IRI-or-Literal half since the
/// extractor never produces blank-node objects; `sh:BlankNode` and
/// anything unrecognised yield `None`.
fn parse_node_kind(iri: &str) -> Option<NodeKind> {
    let local = iri.strip_prefix(SH_NS).unwrap_or(iri);
    match local {
        "IRI" | "IRIOrLiteral" | "BlankNodeOrIRI" => Some(NodeKind::Iri),
        "Literal" | "BlankNodeOrLiteral" => Some(NodeKind::Literal),
        _ => None,
    }
}

/// SPARQL the `TBox` for a property whose local name equals `local_target`,
/// returning the first matching subject IRI.
///
/// Powers the catchall-IRI bridge: a sentinel local name (for example
/// `related_to`) becomes the IRI the `TBox` declares for it. Returns
/// `None` when no matching declaration exists — the bridge stays inactive
/// and the write path falls back to the default `urn:ontrace:p/<sentinel>`
/// minting.
fn query_predicate_iri(
    store: &OxigraphStore,
    graph_iri: &str,
    local_target: &str,
) -> Result<Option<String>, oxigraph::sparql::EvaluationError> {
    let sparql = format!(
        "SELECT ?p WHERE {{ GRAPH <{graph_iri}> {{ \
         ?p a ?t . FILTER(?t IN (\
         <http://www.w3.org/2002/07/owl#ObjectProperty>, \
         <http://www.w3.org/2002/07/owl#DatatypeProperty>, \
         <http://www.w3.org/1999/02/22-rdf-syntax-ns#Property>)) }} }}"
    );
    let results = store.query(&sparql)?;
    if let QueryResults::Solutions(solutions) = results {
        for sol in solutions {
            let sol = sol?;
            let Some(Term::NamedNode(node)) = sol.get("p") else {
                continue;
            };
            let iri = node.as_str();
            if local_name(iri) == local_target {
                return Ok(Some(iri.to_owned()));
            }
        }
    }
    Ok(None)
}

/// SPARQL the `TBox` for every declared class, returning a `local-name → IRI`
/// map. First IRI per local-name wins. Powers the `ABox` ↔ `TBox` class IRI
/// bridge consumed by the type-emission path.
fn query_class_iris(
    store: &OxigraphStore,
    graph_iri: &str,
) -> Result<HashMap<String, String>, oxigraph::sparql::EvaluationError> {
    let sparql = format!(
        "SELECT DISTINCT ?c WHERE {{ GRAPH <{graph_iri}> {{ \
         ?c a ?t . FILTER(?t IN (\
         <http://www.w3.org/2000/01/rdf-schema#Class>, \
         <http://www.w3.org/2002/07/owl#Class>)) }} }}"
    );
    let results = store.query(&sparql)?;
    let mut out: HashMap<String, String> = HashMap::new();
    if let QueryResults::Solutions(solutions) = results {
        for sol in solutions {
            let sol = sol?;
            let Some(Term::NamedNode(node)) = sol.get("c") else {
                continue;
            };
            let iri = node.as_str();
            out.entry(local_name(iri)).or_insert_with(|| iri.to_owned());
        }
    }
    Ok(out)
}

/// SPARQL the `TBox` for every `rdfs:subClassOf` edge and fold them into
/// a [`ClassHierarchy`].
///
/// Mirrors the seed-loader path so the validator sees the same transitive
/// index regardless of whether the ontology entered via Turtle parsing
/// (`SeedOntology`) or arrived directly in the `TBox` graph.
fn query_class_hierarchy(
    store: &OxigraphStore,
    graph_iri: &str,
) -> Result<ClassHierarchy, oxigraph::sparql::EvaluationError> {
    let sparql = format!(
        "SELECT ?c ?p WHERE {{ GRAPH <{graph_iri}> {{ \
         ?c <http://www.w3.org/2000/01/rdf-schema#subClassOf> ?p . \
         FILTER(isIRI(?c) && isIRI(?p)) }} }}"
    );
    let results = store.query(&sparql)?;
    let mut hierarchy = ClassHierarchy::new();
    if let QueryResults::Solutions(solutions) = results {
        for sol in solutions {
            let sol = sol?;
            let Some(Term::NamedNode(c)) = sol.get("c") else {
                continue;
            };
            let Some(Term::NamedNode(p)) = sol.get("p") else {
                continue;
            };
            hierarchy.add_subclass(local_name(c.as_str()), local_name(p.as_str()));
        }
    }
    Ok(hierarchy)
}

/// Local-name of an IRI: the substring after the last `#` or `/`.
/// Falls back to the full IRI if neither separator is present.
fn local_name(iri: &str) -> String {
    let cut = iri
        .rfind('#')
        .or_else(|| iri.rfind('/'))
        .map_or(0, |i| i + 1);
    iri[cut..].to_owned()
}

/// Write validated triples as RDF quads into the data (`ABox`) named graph.
///
/// Consumes [`ValidatedTriples`] — the subset of the extractor's output
/// that survived [`validate_triples`]. Each `RawTriple` becomes:
///
/// 1. One **direct** quad `(s, p, o, data_graph)` — supports
///    relationship-shaped SPARQL queries.
/// 2. One **reified** observation: a fresh blank node typed as
///    [`rdf:Statement`], carrying `rdf:subject` / `rdf:predicate` /
///    `rdf:object` plus the trace's `observed_at` and optional `trace_id`.
///    Preserves the full temporal history.
///
/// Aliases drive `owl:sameAs` emission identically to the naive in-memory agent.
/// The predicate is run through [`bridge_predicate`] so catchall triples
/// align with the curated `TBox` IRI when one is registered; sightings keep
/// the raw LLM output so the markdown report stays readable.
///
/// Each surviving triple is classified into [`OntologyGap`] — catchall uses
/// and uncurated predicates are feedback signal for ontology iteration.
/// SHACL-dropped triples never reach here; they're recorded upstream in
/// [`validate_triples`].
///
/// Errors from `Aliases::resolve` are not possible (infallible); errors from
/// `OxigraphStore::insert_quad` are logged and skipped so a single failing
/// quad doesn't abort the per-trace session.
///
/// [`rdf:Statement`]: http://www.w3.org/1999/02/22-rdf-syntax-ns#Statement
#[system]
pub(crate) async fn write_to_oxigraph(
    triples: Out<ValidatedTriples>,
    line: Res<TraceLine>,
    store: Res<OxigraphStore>,
    aliases: Res<Aliases>,
    sightings: Res<Sightings>,
    ontology: Res<OntologyContext>,
    gap: Res<OntologyGap>,
) -> usize {
    for raw in &triples.0 {
        record_predicate_gaps(&gap, &ontology, raw, &line.text);
    }
    let quads = build_quads(
        triples.0.iter(),
        &line,
        &aliases,
        &ontology,
        store.data_graph(),
    );
    let mut written = 0usize;
    for quad in &quads {
        match store.insert_quad(quad.as_ref()) {
            Ok(_) => written += 1,
            Err(err) => {
                tracing::warn!(error = %err, quad = %quad, "oxigraph insert failed");
            }
        }
    }
    sightings.push(Sighting {
        line: line.text.clone(),
        observed_at: line.observed_at,
        trace_id: line.trace_id.clone(),
        triples: triples.0.clone(),
    });
    written
}

/// Pure helper: turn a stream of `RawTriple`s into the full set of quads
/// (direct + reified + alias `owl:sameAs`) that should land in `data_graph`.
///
/// Factored out for unit testing — exercising the polaris `#[system]`
/// machinery in tests is much heavier than calling this directly.
fn build_quads<'a>(
    raw: impl IntoIterator<Item = &'a RawTriple>,
    line: &TraceLine,
    aliases: &Aliases,
    ontology: &OntologyContext,
    data_graph: NamedNodeRef<'_>,
) -> Vec<Quad> {
    let mut quads = Vec::new();
    let observed_at = Literal::new_typed_literal(line.observed_at.to_rfc3339(), xsd::DATE_TIME);
    let observed_at_pred = named(ONT_OBSERVED_AT);
    let trace_id_pred = named(ONT_TRACE_ID);
    let same_as = named(OWL_SAME_AS_IRI);

    for r in raw {
        for entity in [&r.subject, &r.object] {
            if let Resolution::EmitSameAs { canonical } = aliases.resolve(entity) {
                let from = named(&entity_iri_str(entity));
                let to = named(&entity_iri_str(&canonical));
                quads.push(Quad::new(
                    from,
                    same_as.clone(),
                    to,
                    data_graph.into_owned(),
                ));
            }
        }

        let s = named(&entity_iri_str(&r.subject));
        let bridged = bridge_predicate(&r.predicate, ontology);
        let p = named(&predicate_iri_str(&bridged));
        let o: Term = object_term(&r.object);

        // Direct (s, p, o) quad.
        quads.push(Quad::new(
            s.clone(),
            p.clone(),
            o.clone(),
            data_graph.into_owned(),
        ));

        // Inferred `rdf:type` quads (no reification — type assertions are
        // schema-level, not per-observation events).
        for type_t in derive_type_triples(r, ontology) {
            let t_subject = named(&entity_iri_str(&type_t.subject));
            let t_pred = named(&predicate_iri_str(&type_t.predicate));
            let t_object: Term = Term::NamedNode(named(&entity_iri_str(&type_t.object)));
            quads.push(Quad::new(
                t_subject,
                t_pred,
                t_object,
                data_graph.into_owned(),
            ));
        }

        // Reified observation carrying timestamp + trace_id.
        let obs: Subject = BlankNode::default().into();
        quads.push(Quad::new(
            obs.clone(),
            rdf::TYPE.into_owned(),
            Term::NamedNode(rdf::STATEMENT.into_owned()),
            data_graph.into_owned(),
        ));
        quads.push(Quad::new(
            obs.clone(),
            rdf::SUBJECT.into_owned(),
            Term::NamedNode(s),
            data_graph.into_owned(),
        ));
        quads.push(Quad::new(
            obs.clone(),
            rdf::PREDICATE.into_owned(),
            Term::NamedNode(p),
            data_graph.into_owned(),
        ));
        quads.push(Quad::new(
            obs.clone(),
            rdf::OBJECT.into_owned(),
            o,
            data_graph.into_owned(),
        ));
        quads.push(Quad::new(
            obs.clone(),
            observed_at_pred.clone(),
            Term::Literal(observed_at.clone()),
            data_graph.into_owned(),
        ));
        if let Some(tid) = &line.trace_id {
            quads.push(Quad::new(
                obs,
                trace_id_pred.clone(),
                Term::Literal(Literal::new_simple_literal(tid)),
                data_graph.into_owned(),
            ));
        }
    }
    quads
}

/// Build a `NamedNode` from a known-good IRI string.
///
/// Panics if the IRI is invalid — every caller passes either a percent-
/// encoded value from [`crate::iri`] or a hard-coded constant, both of which
/// are valid by construction.
fn named(iri: &str) -> NamedNode {
    NamedNode::new(iri).expect("IRI built from validated source")
}

/// Render the object position of a [`RawTriple`] as either an RDF literal
/// (when the value parses as a recognized scalar — see [`crate::scalar`])
/// or a named-node entity IRI.
///
/// Keeping the choice in one place ensures the direct quad and the reified
/// `rdf:object` term stay in sync, and matches the dump-side rendering in
/// [`crate::dump`] so the two backends produce comparable RDF.
fn object_term(e: &Entity) -> Term {
    match scalar::classify(&e.0) {
        Some(ScalarKind::Integer) => {
            Term::Literal(Literal::new_typed_literal(e.0.as_str(), xsd::INTEGER))
        }
        Some(ScalarKind::Decimal) => {
            Term::Literal(Literal::new_typed_literal(e.0.as_str(), xsd::DECIMAL))
        }
        Some(ScalarKind::Duration) => Term::Literal(Literal::new_simple_literal(e.0.as_str())),
        None => Term::NamedNode(named(&entity_iri_str(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::extraction::RawTriple;
    use crate::resources::kg::{Entity, Predicate};
    use chrono::{TimeZone, Utc};
    use oxigraph::model::NamedNode;

    fn line_at(text: &str, trace_id: Option<&str>) -> TraceLine {
        TraceLine {
            text: text.to_string(),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            trace_id: trace_id.map(str::to_string),
        }
    }

    fn raw(s: &str, p: &str, o: &str) -> RawTriple {
        RawTriple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            subject_type: None,
            object_type: None,
        }
    }

    fn data_graph() -> NamedNode {
        NamedNode::new(crate::oxigraph::DEFAULT_DATA_IRI).unwrap()
    }

    #[test]
    fn single_triple_emits_direct_plus_reified() {
        let aliases = Aliases::new();
        let g = data_graph();
        let line = line_at("alice met bob", None);
        let quads = build_quads(
            &[raw("alice", "met", "bob")],
            &line,
            &aliases,
            &OntologyContext::default(),
            g.as_ref(),
        );

        // 1 direct + 5 reified (type, subject, predicate, object, observedAt)
        // and 0 sameAs (fresh aliases, no clashes).
        assert_eq!(quads.len(), 6, "got: {quads:#?}");

        let direct = quads
            .iter()
            .find(|q| q.predicate.as_str() == "urn:ontrace:p/met")
            .expect("direct quad present");
        assert_eq!(direct.subject.to_string(), "<urn:ontrace:e/alice>");
        assert_eq!(direct.object.to_string(), "<urn:ontrace:e/bob>");

        let has_observed = quads
            .iter()
            .any(|q| q.predicate.as_str() == ONT_OBSERVED_AT);
        assert!(has_observed, "reified observedAt missing");
    }

    #[test]
    fn alias_emits_owl_same_as_quad() {
        let aliases = Aliases::new();
        aliases.resolve(&Entity::new("Alice")); // seed canonical
        let g = data_graph();
        let line = line_at("alice met bob", None);

        // "alice" should now alias to "Alice".
        let quads = build_quads(
            &[raw("alice", "met", "bob")],
            &line,
            &aliases,
            &OntologyContext::default(),
            g.as_ref(),
        );

        let same_as = quads
            .iter()
            .find(|q| q.predicate.as_str() == OWL_SAME_AS_IRI)
            .expect("expected owl:sameAs quad");
        assert_eq!(same_as.subject.to_string(), "<urn:ontrace:e/alice>");
        assert_eq!(same_as.object.to_string(), "<urn:ontrace:e/Alice>");
    }

    #[test]
    fn trace_id_emits_extra_reified_quad() {
        let aliases = Aliases::new();
        let g = data_graph();
        let line = line_at("alice met bob", Some("trace-42"));
        let quads = build_quads(
            &[raw("alice", "met", "bob")],
            &line,
            &aliases,
            &OntologyContext::default(),
            g.as_ref(),
        );

        let has_trace = quads.iter().any(|q| q.predicate.as_str() == ONT_TRACE_ID);
        assert!(has_trace, "expected trace_id reified quad");
        // With trace_id we get 7 quads (6 base + trace_id).
        assert_eq!(quads.len(), 7);
    }

    #[test]
    fn quads_land_in_data_graph() {
        let aliases = Aliases::new();
        let g = data_graph();
        let line = line_at("alice met bob", None);
        let quads = build_quads(
            &[raw("alice", "met", "bob")],
            &line,
            &aliases,
            &OntologyContext::default(),
            g.as_ref(),
        );

        for q in &quads {
            assert_eq!(
                q.graph_name.to_string(),
                format!("<{}>", crate::oxigraph::DEFAULT_DATA_IRI),
                "quad {q:?} not in data graph"
            );
        }
    }

    #[test]
    fn scalar_object_renders_as_typed_literal() {
        let aliases = Aliases::new();
        let g = data_graph();
        let line = line_at("build_job took 1.63s", None);
        let quads = build_quads(
            &[raw("build_job", "has_latency", "1.63s")],
            &line,
            &aliases,
            &OntologyContext::default(),
            g.as_ref(),
        );
        let direct = quads
            .iter()
            .find(|q| q.predicate.as_str() == "urn:ontrace:p/has_latency")
            .expect("direct latency quad present");
        // Plain string literal preserving the unit; no datatype IRI.
        assert_eq!(direct.object.to_string(), "\"1.63s\"");
    }

    #[test]
    fn integer_object_renders_with_xsd_integer_datatype() {
        let aliases = Aliases::new();
        let g = data_graph();
        let line = line_at("req has 1024 tokens", None);
        let quads = build_quads(
            &[raw("req_42", "has_token_usage", "1024")],
            &line,
            &aliases,
            &OntologyContext::default(),
            g.as_ref(),
        );
        let direct = quads
            .iter()
            .find(|q| q.predicate.as_str() == "urn:ontrace:p/has_token_usage")
            .expect("direct quad present");
        assert!(
            direct.object.to_string().contains("xsd:integer")
                || direct
                    .object
                    .to_string()
                    .contains("http://www.w3.org/2001/XMLSchema#integer"),
            "expected xsd:integer datatype, got: {}",
            direct.object,
        );
    }

    #[test]
    fn local_name_strips_iri() {
        assert_eq!(local_name("http://example.org/onto#works_at"), "works_at");
        assert_eq!(local_name("urn:ontrace:p/met"), "met");
        assert_eq!(local_name("works_at"), "works_at");
    }

    fn seeded_store() -> OxigraphStore {
        let store = OxigraphStore::in_memory().unwrap();
        store
            .load_turtle(
                "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                 @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                 @prefix ex: <http://example.org/onto#> .\n\
                 ex:Person a owl:Class .\n\
                 ex:Organization a owl:Class .\n\
                 ex:works_at a owl:ObjectProperty ;\n\
                     rdfs:domain ex:Person ; rdfs:range ex:Organization .\n\
                 ex:met a owl:ObjectProperty .\n",
                store.ontology_graph(),
            )
            .unwrap();
        store
    }

    #[test]
    fn snapshot_collects_predicates_and_classes() {
        let store = seeded_store();
        let ctx = snapshot_ontology(&store, OntologyPolicy::Strict).unwrap();
        assert_eq!(ctx.predicates, vec!["met", "works_at"]);
        assert_eq!(ctx.classes, vec!["Organization", "Person"]);
        assert!(ctx.summary.contains("works_at"));
        assert!(ctx.summary.contains("Person"));
        assert!(matches!(ctx.policy, OntologyPolicy::Strict));
    }

    #[test]
    fn snapshot_empty_ontology_yields_empty_context() {
        let store = OxigraphStore::in_memory().unwrap();
        let ctx = snapshot_ontology(&store, OntologyPolicy::Strict).unwrap();
        assert!(ctx.is_empty());
        assert!(ctx.summary.is_empty());
    }

    #[test]
    fn snapshot_carries_class_hierarchy_from_tbox() {
        // `rdfs:subClassOf` edges in the TBox graph must reach the
        // snapshot's ClassHierarchy so the validator can match abstract
        // domains and ranges via transitive inference.
        let store = OxigraphStore::in_memory().unwrap();
        store
            .load_turtle(
                "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                 @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                 @prefix ex: <http://example.org/onto#> .\n\
                 ex:Event a owl:Class .\n\
                 ex:Span a owl:Class ; rdfs:subClassOf ex:Event .\n\
                 ex:ToolCall a owl:Class ; rdfs:subClassOf ex:Span .\n",
                store.ontology_graph(),
            )
            .unwrap();
        let ctx = snapshot_ontology(&store, OntologyPolicy::Strict).unwrap();
        assert!(ctx.class_hierarchy.is_a("Span", "Event"));
        assert!(ctx.class_hierarchy.is_a("ToolCall", "Event"));
        assert!(!ctx.class_hierarchy.is_a("Event", "ToolCall"));
    }

    #[test]
    fn catchall_policy_mentions_sentinel_in_summary() {
        let store = seeded_store();
        let policy = OntologyPolicy::CatchAll {
            catchall_predicate: "related_to".into(),
            catchall_iri: None,
        };
        let ctx = snapshot_ontology(&store, policy).unwrap();
        assert!(
            ctx.summary.contains("related_to"),
            "summary: {}",
            ctx.summary
        );
    }

    #[test]
    fn snapshot_carries_local_name_to_iri_bridge() {
        let store = seeded_store();
        let ctx = snapshot_ontology(&store, OntologyPolicy::Strict).unwrap();
        // Every curated local name resolves to its TBox IRI via the bridge.
        assert_eq!(
            ctx.predicate_iris.get("works_at").map(String::as_str),
            Some("http://example.org/onto#works_at"),
        );
        assert_eq!(
            ctx.predicate_iris.get("met").map(String::as_str),
            Some("http://example.org/onto#met"),
        );
        // Bridge keys cover every predicate in the snapshot.
        for name in &ctx.predicates {
            assert!(ctx.predicate_iris.contains_key(name), "{name:?} missing");
        }
    }

    #[test]
    fn bridge_drives_data_graph_quad_to_tbox_iri() {
        // End-to-end check: build a curated ontology with `met` declared in
        // the TBox at a non-default IRI and confirm an extracted triple
        // lands at that IRI in the data graph, not the default
        // `urn:ontrace:p/met`.
        use crate::iri::predicate_iri_str;
        use crate::resources::aliases::Aliases;
        use crate::resources::kg::{Entity, Predicate};

        let store = seeded_store();
        let ontology = snapshot_ontology(&store, OntologyPolicy::Strict).unwrap();
        let aliases = Aliases::new();
        let g = data_graph();
        let line = line_at("alice met bob", None);
        let quads = build_quads(
            &[RawTriple {
                subject: Entity::new("alice"),
                predicate: Predicate::new("met"),
                object: Entity::new("bob"),
                subject_type: None,
                object_type: None,
            }],
            &line,
            &aliases,
            &ontology,
            g.as_ref(),
        );

        let direct = quads
            .iter()
            .find(|q| q.predicate.as_str() == "http://example.org/onto#met")
            .expect("direct quad should use TBox IRI for curated predicate");
        assert_eq!(direct.subject.to_string(), "<urn:ontrace:e/alice>");
        // The default `urn:ontrace:p/met` minting must not appear for a
        // curated predicate — that's the whole point of the bridge.
        assert!(
            !quads
                .iter()
                .any(|q| q.predicate.as_str() == predicate_iri_str(&Predicate::new("met"))),
            "default `urn:ontrace:p/met` IRI should not appear when TBox declares the predicate",
        );
    }
}
