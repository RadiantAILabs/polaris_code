//! Starter ontology packs loaded from `.ttl` files at runtime.
//!
//! `SeedOntology` is a [`LocalResource`] the plugin worker inserts per
//! trace session. The naive in-memory
//! [`super::extraction::ExtractionAgent`]'s `setup` reads it and builds
//! the [`super::extraction::OntologyContext`] the extraction system
//! actually consumes; the oxigraph plugin additionally loads the same
//! Turtle source into the `TBox` graph so SPARQL queries can reason
//! over it.
//!
//! # Layered packs
//!
//! The crate ships a small layered ontology under
//! `crates/polaris_ontrace/ontologies/`:
//!
//! - `core` — abstract primitives (`Actor`, `Artifact`, `Event`,
//!   `Measurement`, `Resource`, `Failure`).
//! - `llm` — LLM substrate (providers, models, requests, responses,
//!   usage); depends on `core`.
//! - `agent` — agent loop (agents, tools, tool calls, plans,
//!   objectives); depends on `core` + `llm`.
//! - `harness` — agent-harness runtime (permission modes, checkpoints,
//!   capabilities, MCP servers); depends on `core` + `agent`.
//! - `user` — user role, preferences, intentions, environment, feedback;
//!   depends on `core` + `agent`.
//! - `coding` — coding-agent specialisation (files, modules, commits,
//!   PRs, branches, design patterns); depends on `core` + `agent`.
//! - `eval` — eval runs, datasets, judges, rubrics, metrics; depends on
//!   `core` + `agent`.
//! - `tracing` — distributed tracing (traces, spans); depends on `core`
//!   + `llm`.
//!
//! Logical packs typically merge several layers:
//!
//! - feedback-loop extractor: `core + llm + agent`
//! - coding-agent run: `core + llm + agent + coding`
//! - agent-harness traces: `core + llm + agent + harness`
//! - eval run: `core + llm + agent + eval`
//! - LLM observability: `core + llm + tracing`
//!
//! Use [`SeedOntology::merged`] to union loaded layers into one pack.
//! When the `oxigraph` feature is enabled, prefer
//! [`SeedOntology::merged_via_oxigraph`]: it parses every pack through
//! oxigraph's RDF parser into a single in-memory graph, so cross-pack
//! `@prefix` redefinitions and `owl:imports` triples are unioned at the
//! triple level rather than concatenated as Turtle source.
//!
//! # Loading
//!
//! Callers (binaries, examples, downstream crates) point the loader at
//! a directory they manage. The crate ships starter packs under
//! `crates/polaris_ontrace/ontologies/` but the path is **never** baked
//! into the library — pass it explicitly via [`SeedOntology::load`] /
//! [`SeedOntology::load_dir`], a CLI flag, or an env var.
//!
//! # Closed-world by default
//!
//! Loaded packs default to [`OntologyPolicy::Strict`] — the LLM may
//! only emit curated predicates, and a line that fits no curated
//! relation yields zero triples rather than escape-hatching to a
//! catch-all. Override per-pack with [`SeedOntology::with_policy`] when
//! an open-world sentinel is wanted.

use crate::agents::extraction::{ClassHierarchy, NodeKind, OntologyPolicy, PredicateShape};
use polaris::system::resource::LocalResource;
use std::collections::HashMap;
use std::path::Path;
use std::{fs, io};

/// File extension recognised by [`SeedOntology::load_dir`].
const TTL_EXTENSION: &str = "ttl";

/// A curated starter ontology — predicates and classes loaded from `.ttl`
/// files — that constrains and guides triple extraction.
///
/// Reach for this when you want the LLM to extract against a known vocabulary
/// rather than free-form: attach a pack via
/// [`ExtractionPlugin::with_seed`](crate::ExtractionPlugin::with_seed) (or
/// `OxigraphExtractionPlugin::with_seed` under the `oxigraph` feature) and the
/// extractor runs closed-world — the JSON-schema enum is limited to the
/// curated predicate local names, and a line that fits none yields zero
/// triples (the [`OntologyPolicy::Strict`] default). The pack holds the parsed
/// predicate/class local names that shape the prompt and schema, the
/// local-name → `TBox`-IRI bridge maps, `rdfs:domain` / `rdfs:range` shape
/// hints, the transitive subclass index, parsed `@prefix` bindings, and the
/// original Turtle source.
///
/// # Scope
///
/// **Local.** Per-session: the plugin worker clones the configured pack into
/// each per-trace session's context, where the naive
/// [`ExtractionAgent`](super::extraction::ExtractionAgent)'s `setup` reads it
/// and builds the session-local
/// [`OntologyContext`](super::extraction::OntologyContext). It is a per-session
/// snapshot of curated vocabulary, not durable shared state, so it is local
/// rather than global.
///
/// # Provided by
///
/// Consumer-supplied — there is no plugin that inserts a default. The
/// application builds a pack (via [`Self::load`] / [`Self::load_dir`] /
/// [`Self::from_string`], optionally [`Self::merged`]) and hands it to
/// [`ExtractionPlugin::with_seed`](crate::ExtractionPlugin::with_seed). The
/// plugin's ingest worker then clones it into every session by calling
/// `ctx.insert(seed)` inside `run_oneshot`, so it lands as a [`LocalResource`]
/// for that session. Without a `with_seed` call, no `SeedOntology` is inserted
/// and the extractor falls back to free-form mode (`OntologyContext::default`).
///
/// # Access
///
/// Read-only via `Res<SeedOntology>` — the naive agent's `setup` calls
/// `ctx.get_resource::<SeedOntology>()` and is rebuilt fresh per session, so
/// there is no mutation contract: a pack is constructed once by the consumer
/// and only read thereafter.
///
/// # Alternatives
///
/// The oxigraph backend does **not** consume `SeedOntology` at extraction
/// time. Instead its agent snapshots the curated vocabulary by running SPARQL
/// against the `TBox` graph already loaded into the `OxigraphStore` (the
/// Turtle is loaded into that graph at plugin `ready`). The two paths build
/// the same `OntologyContext`; only the source of the snapshot differs — a
/// [`LocalResource`] here versus a query against the shared store there. See
/// `crate::agents::oxigraph_extraction`.
///
/// # Example system
///
/// ```ignore
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::SeedOntology;
///
/// /// Log how many curated predicates the session's pack supplies.
/// #[system]
/// fn seed_summary(seed: Res<SeedOntology>) {
///     tracing::info!(
///         pack = %seed.name,
///         predicates = seed.predicates.len(),
///         classes = seed.classes.len(),
///         "curated vocabulary for this session",
///     );
/// }
/// ```
#[derive(Debug, Clone)]
pub struct SeedOntology {
    /// Short identifier (typically the file stem, e.g. `"agent_loop"`).
    pub name: String,
    /// Curated predicate local names (`snake_case`).
    pub predicates: Vec<String>,
    /// Bridge from predicate local-name to the fully-qualified `TBox` IRI
    /// that declares it. Drives [`crate::agents::extraction::bridge_predicate`]
    /// so every curated predicate lands at its `TBox` IRI in the `ABox` —
    /// LLM emits `met`, write path stores `<urn:ontrace:onto/agent/met>`.
    /// When two packs declare the same local name under different IRIs the
    /// first one loaded wins (see [`Self::merged`]).
    pub predicate_iris: HashMap<String, String>,
    /// `rdfs:domain` / `rdfs:range` hints keyed by predicate local-name.
    /// Drives the `name (Domain → Range)` annotations the extractor's
    /// prompt summary uses to steer the LLM toward the most specific
    /// predicate. Predicates without an entry render as plain names.
    pub predicate_shapes: HashMap<String, PredicateShape>,
    /// Curated class local names (`PascalCase`). Surfaced in the prompt
    /// summary; the JSON schema does not constrain entity types yet.
    pub classes: Vec<String>,
    /// Bridge from class local-name to the fully-qualified `TBox` IRI that
    /// declares it. Used by the type-emission path to anchor `rdf:type`
    /// objects to curated class IRIs (e.g. `Agent` →
    /// `urn:ontrace:onto/agent/Agent`) so the `ABox` and `TBox` share class
    /// identifiers. First IRI per local-name wins across merged packs.
    pub class_iris: HashMap<String, String>,
    /// Transitive `rdfs:subClassOf` index parsed from the pack's Turtle
    /// source. The SHACL validator consults this so abstract domains and
    /// ranges (e.g. `core:Event`) match every subclass-typed subject or
    /// object, per the standard SHACL semantics.
    pub class_hierarchy: ClassHierarchy,
    /// Policy applied when the LLM extracts a relation outside the
    /// curated predicate list. Defaults to [`OntologyPolicy::Strict`].
    pub policy: OntologyPolicy,
    /// `@prefix` declarations parsed from the pack's Turtle source,
    /// keyed by prefix name (no trailing `:`) → namespace IRI. Drives
    /// [`Self::sparql_prologue`] so `ontrace query` / `inspect` SPARQL
    /// can use the pack's compact names without redeclaring them. When
    /// two packs bind the same prefix name the first one loaded wins
    /// (see [`Self::merged`]).
    pub prefixes: HashMap<String, String>,
    /// The Turtle source the pack was parsed from. Loaded verbatim into
    /// the oxigraph `TBox` graph by
    /// [`crate::plugins::oxigraph_extraction::OxigraphExtractionPlugin`].
    pub turtle: String,
}

impl SeedOntology {
    /// Load a single `.ttl` file into a [`SeedOntology`].
    ///
    /// The pack name is derived from the file stem (`agent.ttl` →
    /// `agent`). Default policy is [`OntologyPolicy::Strict`] — chain
    /// [`Self::with_policy`] to override.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] on read failure, or
    /// [`io::ErrorKind::InvalidInput`] if the path has no valid UTF-8
    /// file stem.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("seed path has no UTF-8 file stem: {}", path.display()),
                )
            })?
            .to_owned();
        let turtle = fs::read_to_string(path)?;
        Ok(Self::from_string(name, turtle))
    }

    /// Load every `.ttl` file in `dir`, sorted by filename for stable order.
    ///
    /// Non-`.ttl` files are silently ignored so the directory can host
    /// READMEs or related docs alongside the packs. Subdirectories are
    /// not recursed.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if `dir` cannot be read or
    /// any individual file fails to load.
    pub fn load_dir(dir: impl AsRef<Path>) -> io::Result<Vec<Self>> {
        let mut paths: Vec<_> = fs::read_dir(dir.as_ref())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case(TTL_EXTENSION))
            })
            .collect();
        paths.sort();
        paths.iter().map(Self::load).collect()
    }

    /// Construct from a pre-loaded Turtle string with an explicit name.
    ///
    /// Lets callers compose seeds programmatically (generated vocabulary,
    /// embedded test fixtures) without going through the filesystem.
    /// Default policy is [`OntologyPolicy::Strict`] — chain
    /// [`Self::with_policy`] to opt into an open-world sentinel.
    #[must_use]
    pub fn from_string(name: impl Into<String>, turtle: impl Into<String>) -> Self {
        let turtle = turtle.into();
        let parsed = parse_seed(&turtle);
        Self {
            name: name.into(),
            predicates: parsed.predicates,
            predicate_iris: parsed.predicate_iris,
            predicate_shapes: parsed.predicate_shapes,
            classes: parsed.classes,
            class_iris: parsed.class_iris,
            class_hierarchy: parsed.class_hierarchy,
            policy: OntologyPolicy::Strict,
            prefixes: parsed.prefixes,
            turtle,
        }
    }

    /// Build a SPARQL `PREFIX` prologue from this pack's `@prefix`
    /// declarations, ready to prepend to a user query.
    ///
    /// Any prefix name `query` already declares is skipped, so callers
    /// can splice the result onto an arbitrary query without risking a
    /// redefinition. Lines are emitted sorted by prefix name for stable
    /// output; the returned string ends with a newline when non-empty
    /// and is empty when the pack adds nothing.
    #[must_use]
    pub fn sparql_prologue(&self, query: &str) -> String {
        let mut pairs: Vec<(&String, &String)> = self
            .prefixes
            .iter()
            .filter(|(name, _)| !query_declares_prefix(query, name))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = String::new();
        for (name, iri) in pairs {
            out.push_str("PREFIX ");
            out.push_str(name);
            out.push_str(": <");
            out.push_str(iri);
            out.push_str(">\n");
        }
        out
    }

    /// Override the default [`OntologyPolicy::Strict`] policy.
    ///
    /// Use [`OntologyPolicy::CatchAll`] when off-ontology lines should
    /// escape-hatch to a sentinel predicate rather than yielding zero
    /// triples.
    #[must_use]
    pub fn with_policy(mut self, policy: OntologyPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Union several packs into a single seed.
    ///
    /// Predicates and classes are concatenated and deduped; Turtle
    /// sources are concatenated verbatim (so oxigraph's `TBox` loader
    /// sees every declaration, including cross-layer `owl:imports`
    /// statements). Policy is taken from the first pack with an
    /// explicit [`OntologyPolicy::CatchAll`]; if all packs are
    /// `Strict`, the merged seed is `Strict`. Chain
    /// [`Self::with_policy`] afterwards to override.
    ///
    /// Use this to assemble a logical pack from the layered files in
    /// `ontologies/` — e.g. "agent feedback loop" = `core + llm + agent`.
    pub fn merged(name: impl Into<String>, packs: impl IntoIterator<Item = Self>) -> Self {
        let name = name.into();
        let mut predicates: Vec<String> = Vec::new();
        let mut classes: Vec<String> = Vec::new();
        let mut predicate_shapes: HashMap<String, PredicateShape> = HashMap::new();
        let mut predicate_iris: HashMap<String, String> = HashMap::new();
        let mut class_iris: HashMap<String, String> = HashMap::new();
        let mut class_hierarchy = ClassHierarchy::new();
        let mut prefixes: HashMap<String, String> = HashMap::new();
        let mut turtle = String::new();
        let mut policy = OntologyPolicy::default();
        let mut policy_resolved = false;
        for pack in packs {
            predicates.extend(pack.predicates);
            classes.extend(pack.classes);
            for (k, v) in pack.predicate_shapes {
                // First declaration wins; layered packs shouldn't redeclare
                // shapes for the same local name, but if they do the lower
                // layer (loaded first) takes precedence.
                predicate_shapes.entry(k).or_insert(v);
            }
            for (k, v) in pack.predicate_iris {
                // Same rule: first pack to declare a predicate name pins
                // its IRI. Cross-pack collisions are unusual (layered packs
                // declare distinct vocabularies) but the deterministic
                // tie-breaker keeps merges reproducible.
                predicate_iris.entry(k).or_insert(v);
            }
            for (k, v) in pack.class_iris {
                class_iris.entry(k).or_insert(v);
            }
            class_hierarchy.extend(&pack.class_hierarchy);
            for (k, v) in pack.prefixes {
                // First pack to bind a prefix name pins its IRI — same
                // deterministic tie-breaker as the predicate/class maps.
                prefixes.entry(k).or_insert(v);
            }
            if !turtle.is_empty() {
                turtle.push('\n');
            }
            turtle.push_str(&pack.turtle);
            if !policy_resolved && matches!(pack.policy, OntologyPolicy::CatchAll { .. }) {
                policy = pack.policy;
                policy_resolved = true;
            }
        }
        predicates.sort();
        predicates.dedup();
        classes.sort();
        classes.dedup();
        Self {
            name,
            predicates,
            predicate_iris,
            predicate_shapes,
            classes,
            class_iris,
            class_hierarchy,
            policy,
            prefixes,
            turtle,
        }
    }
}

#[cfg(feature = "oxigraph")]
impl SeedOntology {
    /// Merge several packs by parsing each Turtle source through oxigraph
    /// into a unified RDF graph, then re-serialising it as one canonical
    /// Turtle document.
    ///
    /// Unlike [`Self::merged`] (verbatim string concatenation), this path
    /// is robust to cross-pack prefix conflicts: each pack is parsed in
    /// its own prefix context, so two packs that both bind `@prefix ont:`
    /// to *different* IRIs (e.g. `urn:ontrace:onto/core/` vs
    /// `urn:ontrace:onto/agent/`) union cleanly at the triple level.
    /// `owl:imports` triples are preserved verbatim — oxigraph does not
    /// dereference them, but they survive the round-trip and SPARQL
    /// reasoners downstream can see them.
    ///
    /// Predicate and class local names are derived from the merged
    /// graph via SPARQL because oxigraph's Turtle serializer drops the
    /// `a` shorthand and prefix abbreviations that the naive
    /// [`Self::merged`] depends on.
    ///
    /// The result's policy follows the same rule as [`Self::merged`]:
    /// the first pack with an explicit [`OntologyPolicy::CatchAll`]
    /// wins, otherwise the default ([`OntologyPolicy::Strict`]) stands.
    /// Chain [`Self::with_policy`] to override.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidData`] if any pack's Turtle
    /// fails to parse, or [`io::ErrorKind::Other`] for oxigraph storage
    /// or SPARQL evaluation failures.
    pub fn merged_via_oxigraph(
        name: impl Into<String>,
        packs: impl IntoIterator<Item = Self>,
    ) -> io::Result<Self> {
        use oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
        use oxigraph::model::GraphNameRef;
        use oxigraph::store::Store;

        let packs: Vec<Self> = packs.into_iter().collect();

        let store =
            Store::new().map_err(|e| io::Error::other(format!("oxigraph store init: {e}")))?;

        for pack in &packs {
            let parser = RdfParser::from_format(RdfFormat::Turtle);
            store
                .load_from_reader(parser, pack.turtle.as_bytes())
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("parse pack {:?}: {e}", pack.name),
                    )
                })?;
        }

        let buf = store
            .dump_graph_to_writer(
                GraphNameRef::DefaultGraph,
                RdfSerializer::from_format(RdfFormat::Turtle),
                Vec::<u8>::new(),
            )
            .map_err(|e| io::Error::other(format!("oxigraph dump: {e}")))?;
        let turtle = String::from_utf8(buf).expect("oxigraph emits utf-8 turtle");

        let predicate_iris = sparql_predicate_iris(&store)?;
        let mut predicates: Vec<String> = predicate_iris.keys().cloned().collect();
        predicates.sort();
        predicates.dedup();
        let class_iris = sparql_class_iris(&store)?;
        let mut classes: Vec<String> = class_iris.keys().cloned().collect();
        classes.sort();
        classes.dedup();
        let predicate_shapes = sparql_predicate_shapes(&store)?;
        let class_hierarchy = sparql_class_hierarchy(&store)?;

        let policy = packs
            .iter()
            .find(|p| matches!(p.policy, OntologyPolicy::CatchAll { .. }))
            .map_or(OntologyPolicy::default(), |p| p.policy.clone());

        // Union the per-pack `@prefix` maps directly — first pack to
        // bind a name wins, matching the verbatim [`Self::merged`] path.
        let mut prefixes: HashMap<String, String> = HashMap::new();
        for pack in &packs {
            for (k, v) in &pack.prefixes {
                prefixes.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        Ok(Self {
            name: name.into(),
            predicates,
            predicate_iris,
            predicate_shapes,
            classes,
            class_iris,
            class_hierarchy,
            policy,
            prefixes,
            turtle,
        })
    }
}

/// Collect `local-name → IRI` for every declared property in `store`'s
/// default graph.
///
/// Used by [`SeedOntology::merged_via_oxigraph`] to build the bridge map
/// that anchors `ABox` predicates to the same IRI the `TBox` declares —
/// no per-query rewriting needed in SPARQL against the resulting store.
///
/// First IRI per local-name wins. Cross-pack collisions are unusual
/// (layered packs use distinct namespaces) but the deterministic
/// tie-breaker keeps merges reproducible.
#[cfg(feature = "oxigraph")]
fn sparql_predicate_iris(store: &oxigraph::store::Store) -> io::Result<HashMap<String, String>> {
    use oxigraph::model::Term;
    use oxigraph::sparql::QueryResults;

    let sparql = "SELECT DISTINCT ?s WHERE { \
                  ?s a ?t . FILTER(?t IN (\
                  <http://www.w3.org/2002/07/owl#ObjectProperty>, \
                  <http://www.w3.org/2002/07/owl#DatatypeProperty>, \
                  <http://www.w3.org/1999/02/22-rdf-syntax-ns#Property>)) }";
    let results = store
        .query(sparql)
        .map_err(|e| io::Error::other(format!("sparql predicate iris query: {e}")))?;
    let mut out: HashMap<String, String> = HashMap::new();
    if let QueryResults::Solutions(solutions) = results {
        for sol in solutions {
            let sol = sol.map_err(|e| io::Error::other(format!("sparql row: {e}")))?;
            let Some(term) = sol.get("s") else { continue };
            if let Term::NamedNode(node) = term
                && let Some(name) = local_name(node.as_str())
            {
                out.entry(name).or_insert_with(|| node.as_str().to_owned());
            }
        }
    }
    Ok(out)
}

/// Collect `(predicate-local-name, PredicateShape)` pairs from the merged
/// oxigraph store by SPARQL-querying both `rdfs:domain` / `rdfs:range` and
/// every SHACL property shape attached to a `sh:NodeShape`.
///
/// Mirrors [`parse_seed`]'s `predicate_shapes` output for the oxigraph
/// path so [`SeedOntology::merged_via_oxigraph`] surfaces the same prompt
/// hints as the naive `merged` path. Predicates that produce no usable
/// field after both passes are omitted — they render as bare names in the
/// prompt and pass validation transparently.
#[cfg(feature = "oxigraph")]
fn sparql_predicate_shapes(
    store: &oxigraph::store::Store,
) -> io::Result<HashMap<String, PredicateShape>> {
    use oxigraph::model::Term;
    use oxigraph::sparql::QueryResults;

    let mut out: HashMap<String, PredicateShape> = HashMap::new();

    let rdfs_pass = "SELECT ?p ?d ?r WHERE { \
                     ?p a ?t . \
                     FILTER(?t IN (\
                     <http://www.w3.org/2002/07/owl#ObjectProperty>, \
                     <http://www.w3.org/2002/07/owl#DatatypeProperty>, \
                     <http://www.w3.org/1999/02/22-rdf-syntax-ns#Property>)) . \
                     OPTIONAL { ?p <http://www.w3.org/2000/01/rdf-schema#domain> ?d } \
                     OPTIONAL { ?p <http://www.w3.org/2000/01/rdf-schema#range> ?r } }";
    if let QueryResults::Solutions(solutions) = store
        .query(rdfs_pass)
        .map_err(|e| io::Error::other(format!("sparql shapes query: {e}")))?
    {
        for sol in solutions {
            let sol = sol.map_err(|e| io::Error::other(format!("sparql row: {e}")))?;
            let Some(Term::NamedNode(p_node)) = sol.get("p") else {
                continue;
            };
            let Some(p_name) = local_name(p_node.as_str()) else {
                continue;
            };
            let domain = sol.get("d").and_then(|t| match t {
                Term::NamedNode(n) => local_name(n.as_str()),
                _ => None,
            });
            let (range_class, range_datatype) = sol
                .get("r")
                .map(|t| match t {
                    Term::NamedNode(n) => split_range_iri(n.as_str()),
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

    let shacl_pass = "SELECT ?p ?dt ?cls ?nk ?target WHERE { \
                      ?shape a <http://www.w3.org/ns/shacl#NodeShape> ; \
                             <http://www.w3.org/ns/shacl#property> ?prop . \
                      ?prop <http://www.w3.org/ns/shacl#path> ?p . \
                      OPTIONAL { ?shape <http://www.w3.org/ns/shacl#targetClass> ?target } \
                      OPTIONAL { ?prop <http://www.w3.org/ns/shacl#datatype> ?dt } \
                      OPTIONAL { ?prop <http://www.w3.org/ns/shacl#class> ?cls } \
                      OPTIONAL { ?prop <http://www.w3.org/ns/shacl#nodeKind> ?nk } }";
    if let QueryResults::Solutions(solutions) = store
        .query(shacl_pass)
        .map_err(|e| io::Error::other(format!("sparql shacl shapes query: {e}")))?
    {
        for sol in solutions {
            let sol = sol.map_err(|e| io::Error::other(format!("sparql row: {e}")))?;
            let Some(Term::NamedNode(p_node)) = sol.get("p") else {
                continue;
            };
            let Some(p_name) = local_name(p_node.as_str()) else {
                continue;
            };
            let entry = out.entry(p_name).or_default();
            if let Some(Term::NamedNode(target)) = sol.get("target")
                && entry.domain_class.is_none()
                && let Some(name) = local_name(target.as_str())
            {
                entry.domain_class = Some(name);
            }
            if let Some(Term::NamedNode(dt)) = sol.get("dt")
                && entry.range_datatype.is_none()
                && let Some(local) = dt.as_str().strip_prefix(XSD_NS)
            {
                entry.range_datatype = Some(local.to_owned());
            }
            if let Some(Term::NamedNode(cls)) = sol.get("cls")
                && entry.range_class.is_none()
                && let Some(name) = local_name(cls.as_str())
            {
                entry.range_class = Some(name);
            }
            if let Some(Term::NamedNode(nk)) = sol.get("nk")
                && entry.node_kind.is_none()
            {
                entry.node_kind = parse_node_kind_iri(nk.as_str());
            }
        }
    }

    out.retain(|_, s| {
        s.domain_class.is_some()
            || s.range_class.is_some()
            || s.range_datatype.is_some()
            || s.node_kind.is_some()
    });
    Ok(out)
}

/// Collect `subclass → parent` `rdfs:subClassOf` edges from the merged
/// oxigraph store and fold them into a [`ClassHierarchy`].
///
/// Mirrors [`parse_seed`]'s `class_hierarchy` output for the oxigraph
/// merge path so [`SeedOntology::merged_via_oxigraph`] produces the same
/// transitive index as the naive [`SeedOntology::merged`] path.
#[cfg(feature = "oxigraph")]
fn sparql_class_hierarchy(store: &oxigraph::store::Store) -> io::Result<ClassHierarchy> {
    use oxigraph::model::Term;
    use oxigraph::sparql::QueryResults;

    let sparql = "SELECT ?c ?p WHERE { \
                  ?c <http://www.w3.org/2000/01/rdf-schema#subClassOf> ?p . \
                  FILTER(isIRI(?c) && isIRI(?p)) }";
    let results = store
        .query(sparql)
        .map_err(|e| io::Error::other(format!("sparql class hierarchy query: {e}")))?;
    let mut hierarchy = ClassHierarchy::new();
    if let QueryResults::Solutions(solutions) = results {
        for sol in solutions {
            let sol = sol.map_err(|e| io::Error::other(format!("sparql row: {e}")))?;
            let Some(Term::NamedNode(c)) = sol.get("c") else {
                continue;
            };
            let Some(Term::NamedNode(p)) = sol.get("p") else {
                continue;
            };
            if let (Some(c_name), Some(p_name)) = (local_name(c.as_str()), local_name(p.as_str())) {
                hierarchy.add_subclass(c_name, p_name);
            }
        }
    }
    Ok(hierarchy)
}

/// Collect `local-name → IRI` for every declared class in `store`'s default
/// graph.
///
/// Mirrors [`sparql_predicate_iris`] for the class side, feeding the class
/// IRI bridge consumed by the type-emission path in the extraction agents.
/// First IRI per local-name wins.
#[cfg(feature = "oxigraph")]
fn sparql_class_iris(store: &oxigraph::store::Store) -> io::Result<HashMap<String, String>> {
    use oxigraph::model::Term;
    use oxigraph::sparql::QueryResults;

    let sparql = "SELECT DISTINCT ?s WHERE { \
                  ?s a ?t . FILTER(?t IN (\
                  <http://www.w3.org/2002/07/owl#Class>, \
                  <http://www.w3.org/2000/01/rdf-schema#Class>)) }";
    let results = store
        .query(sparql)
        .map_err(|e| io::Error::other(format!("sparql class iris query: {e}")))?;
    let mut out: HashMap<String, String> = HashMap::new();
    if let QueryResults::Solutions(solutions) = results {
        for sol in solutions {
            let sol = sol.map_err(|e| io::Error::other(format!("sparql row: {e}")))?;
            let Some(Term::NamedNode(node)) = sol.get("s") else {
                continue;
            };
            if let Some(name) = local_name(node.as_str()) {
                out.entry(name).or_insert_with(|| node.as_str().to_owned());
            }
        }
    }
    Ok(out)
}

impl LocalResource for SeedOntology {}

/// IRI for `rdf:type` — the `a` shorthand resolves here.
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
/// IRI for `rdfs:domain`.
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
/// IRI for `rdfs:range`.
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
/// IRI for `rdfs:subClassOf` — class hierarchy edges.
const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";

/// XSD namespace IRI prefix — `rdfs:range` / `sh:datatype` IRIs under this
/// namespace are treated as datatype constraints, not as classes.
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";

/// SHACL namespace IRI prefix.
const SH_NS: &str = "http://www.w3.org/ns/shacl#";

/// SHACL `sh:path` — names the predicate a property shape constrains.
const SH_PATH: &str = "http://www.w3.org/ns/shacl#path";
/// SHACL `sh:datatype` — literal datatype on a property shape.
const SH_DATATYPE: &str = "http://www.w3.org/ns/shacl#datatype";
/// SHACL `sh:class` — class the object of a property must belong to.
const SH_CLASS: &str = "http://www.w3.org/ns/shacl#class";
/// SHACL `sh:nodeKind` — IRI-vs-Literal narrowing on a property shape.
const SH_NODE_KIND: &str = "http://www.w3.org/ns/shacl#nodeKind";
/// SHACL `sh:property` — links a node shape to one of its property shapes.
const SH_PROPERTY: &str = "http://www.w3.org/ns/shacl#property";
/// SHACL `sh:targetClass` — declares the class a node shape applies to.
const SH_TARGET_CLASS: &str = "http://www.w3.org/ns/shacl#targetClass";

/// IRIs we treat as "this subject is a property".
const PROPERTY_TYPE_IRIS: [&str; 3] = [
    "http://www.w3.org/2002/07/owl#ObjectProperty",
    "http://www.w3.org/2002/07/owl#DatatypeProperty",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#Property",
];

/// IRIs we treat as "this subject is a class".
const CLASS_TYPE_IRIS: [&str; 2] = [
    "http://www.w3.org/2002/07/owl#Class",
    "http://www.w3.org/2000/01/rdf-schema#Class",
];

/// Split an `rdfs:range` IRI into `(range_class, range_datatype)`. Exactly
/// one side is `Some` for any input — the caller writes it into the
/// matching field of [`PredicateShape`].
fn split_range_iri(iri: &str) -> (Option<String>, Option<String>) {
    if let Some(local) = iri.strip_prefix(XSD_NS) {
        (None, Some(local.to_owned()))
    } else {
        (local_name(iri), None)
    }
}

/// Map a SHACL `sh:nodeKind` value IRI onto [`NodeKind`]. Kinds that
/// admit blank nodes collapse onto their IRI-or-Literal half since the
/// extractor never produces blank-node objects.
fn parse_node_kind_iri(iri: &str) -> Option<NodeKind> {
    let local = iri.strip_prefix(SH_NS).unwrap_or(iri);
    match local {
        "IRI" | "IRIOrLiteral" | "BlankNodeOrIRI" => Some(NodeKind::Iri),
        "Literal" | "BlankNodeOrLiteral" => Some(NodeKind::Literal),
        _ => None,
    }
}

/// Result of a single oxttl pass over a Turtle source.
struct SeedParse {
    /// Property local names, sorted + deduped.
    predicates: Vec<String>,
    /// Local-name → fully-qualified `TBox` IRI for every declared
    /// property. First declaration of a given local-name wins.
    predicate_iris: HashMap<String, String>,
    /// Class local names, sorted + deduped.
    classes: Vec<String>,
    /// Local-name → fully-qualified `TBox` IRI for every declared class.
    /// First declaration of a given local-name wins.
    class_iris: HashMap<String, String>,
    /// `rdfs:domain` / `rdfs:range` hints keyed by predicate local-name.
    predicate_shapes: HashMap<String, PredicateShape>,
    /// Transitive `rdfs:subClassOf` index built from every
    /// `?c rdfs:subClassOf ?p` triple in the source.
    class_hierarchy: ClassHierarchy,
    /// `@prefix` declarations seen in the source, keyed by prefix name
    /// (no trailing `:`) → namespace IRI.
    prefixes: HashMap<String, String>,
}

/// Single oxttl pass that extracts every signal `from_string` needs:
/// property + class local names and `rdfs:domain` / `rdfs:range` shapes.
///
/// A real Turtle parser avoids the brittleness of a regex-style scanner —
/// declarations whose comments contain literal `.` characters (e.g.
/// `e.g.`) or whose adjacent statements use `#`-suffixed IRIs
/// (`<…#type>`) would silently drop the closing statement period and
/// merge the following declaration's subject. oxttl tracks string and IRI
/// context, so those cases parse correctly.
fn parse_seed(turtle: &str) -> SeedParse {
    let parser = oxttl::TurtleParser::new();
    let mut reader = parser.for_slice(turtle.as_bytes());

    let mut property_iris: Vec<String> = Vec::new();
    let mut class_iris: Vec<String> = Vec::new();
    // Predicate-keyed (by local name) shape signals from `rdfs:*`. SHACL
    // signals are collected by *shape subject* below and joined after the
    // pass — property shapes are often blank nodes, so we can't key on
    // predicate name until we've seen the `sh:path` triple.
    let mut domains: HashMap<String, String> = HashMap::new();
    let mut range_classes: HashMap<String, String> = HashMap::new();
    let mut range_datatypes: HashMap<String, String> = HashMap::new();
    let mut class_hierarchy = ClassHierarchy::new();
    // SHACL property-shape state, keyed by the shape subject (NamedNode IRI
    // or blank-node id, rendered as a string for hashing).
    let mut sh_path: HashMap<String, String> = HashMap::new();
    let mut sh_datatype: HashMap<String, String> = HashMap::new();
    let mut sh_class: HashMap<String, String> = HashMap::new();
    let mut sh_node_kind: HashMap<String, NodeKind> = HashMap::new();
    // Node-shape → property-shape edges + targetClass declarations.
    let mut sh_property_edges: Vec<(String, String)> = Vec::new();
    let mut sh_target_class: HashMap<String, String> = HashMap::new();

    let subject_key = |subj: &oxrdf::Subject| -> String {
        // oxrdf::Subject gains a TripleSubject variant under the
        // `oxigraph` feature; match on the two we care about and fall
        // through to empty otherwise — `if let` keeps the no-feature
        // build clean (no unreachable_patterns), and the feature build
        // gets the implicit catch-all it needs.
        if let oxrdf::Subject::NamedNode(n) = subj {
            n.as_str().to_owned()
        } else if let oxrdf::Subject::BlankNode(b) = subj {
            format!("_:{}", b.as_str())
        } else {
            String::new()
        }
    };
    let term_key = |term: &oxrdf::Term| -> Option<String> {
        if let oxrdf::Term::NamedNode(n) = term {
            Some(n.as_str().to_owned())
        } else if let oxrdf::Term::BlankNode(b) = term {
            Some(format!("_:{}", b.as_str()))
        } else {
            None
        }
    };

    for result in reader.by_ref() {
        let triple = match result {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(error = %err, "parse_seed: turtle parse error");
                continue;
            }
        };
        let subj_key = subject_key(&triple.subject);
        if subj_key.is_empty() {
            continue;
        }
        match triple.predicate.as_str() {
            RDF_TYPE => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                let t_iri = t.as_str();
                let oxrdf::Subject::NamedNode(subj) = &triple.subject else {
                    continue;
                };
                if PROPERTY_TYPE_IRIS.contains(&t_iri) {
                    property_iris.push(subj.as_str().to_owned());
                } else if CLASS_TYPE_IRIS.contains(&t_iri) {
                    class_iris.push(subj.as_str().to_owned());
                }
            }
            RDFS_DOMAIN => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                let oxrdf::Subject::NamedNode(subj) = &triple.subject else {
                    continue;
                };
                if let (Some(s_name), Some(t_name)) =
                    (local_name(subj.as_str()), local_name(t.as_str()))
                {
                    domains.entry(s_name).or_insert(t_name);
                }
            }
            RDFS_RANGE => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                let oxrdf::Subject::NamedNode(subj) = &triple.subject else {
                    continue;
                };
                let Some(s_name) = local_name(subj.as_str()) else {
                    continue;
                };
                let (class, datatype) = split_range_iri(t.as_str());
                if let Some(c) = class {
                    range_classes.entry(s_name.clone()).or_insert(c);
                }
                if let Some(d) = datatype {
                    range_datatypes.entry(s_name).or_insert(d);
                }
            }
            RDFS_SUBCLASS_OF => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                let oxrdf::Subject::NamedNode(subj) = &triple.subject else {
                    continue;
                };
                if let (Some(s_name), Some(p_name)) =
                    (local_name(subj.as_str()), local_name(t.as_str()))
                {
                    class_hierarchy.add_subclass(s_name, p_name);
                }
            }
            SH_PATH => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                if let Some(p_name) = local_name(t.as_str()) {
                    sh_path.entry(subj_key.clone()).or_insert(p_name);
                }
            }
            SH_DATATYPE => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                if let Some(local) = t.as_str().strip_prefix(XSD_NS) {
                    sh_datatype
                        .entry(subj_key.clone())
                        .or_insert_with(|| local.to_owned());
                }
            }
            SH_CLASS => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                if let Some(name) = local_name(t.as_str()) {
                    sh_class.entry(subj_key.clone()).or_insert(name);
                }
            }
            SH_NODE_KIND => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                if let Some(kind) = parse_node_kind_iri(t.as_str())
                    && !sh_node_kind.contains_key(&subj_key)
                {
                    sh_node_kind.insert(subj_key.clone(), kind);
                }
            }
            SH_PROPERTY => {
                if let Some(obj_key) = term_key(&triple.object) {
                    sh_property_edges.push((subj_key.clone(), obj_key));
                }
            }
            SH_TARGET_CLASS => {
                let oxrdf::Term::NamedNode(t) = &triple.object else {
                    continue;
                };
                if let Some(name) = local_name(t.as_str()) {
                    sh_target_class.entry(subj_key.clone()).or_insert(name);
                }
            }
            _ => {}
        }
    }

    // The reader is fully drained, so `prefixes()` now reports every
    // `@prefix` declaration in the source.
    let prefixes: HashMap<String, String> = reader
        .prefixes()
        .map(|(name, iri)| (name.to_owned(), iri.to_owned()))
        .collect();

    // First-declared-wins: build the local-name → IRI map by iterating in
    // declaration order. Layered packs can override later with `merged`.
    let mut predicate_iris: HashMap<String, String> = HashMap::new();
    for iri in &property_iris {
        if let Some(name) = local_name(iri) {
            predicate_iris.entry(name).or_insert_with(|| iri.clone());
        }
    }

    let mut predicates: Vec<String> = property_iris.iter().filter_map(|i| local_name(i)).collect();
    predicates.sort();
    predicates.dedup();

    let mut class_iri_map: HashMap<String, String> = HashMap::new();
    for iri in &class_iris {
        if let Some(name) = local_name(iri) {
            class_iri_map.entry(name).or_insert_with(|| iri.clone());
        }
    }

    let mut classes: Vec<String> = class_iris.iter().filter_map(|i| local_name(i)).collect();
    classes.sort();
    classes.dedup();

    let mut predicate_shapes: HashMap<String, PredicateShape> = HashMap::new();
    for (k, v) in domains {
        predicate_shapes.entry(k).or_default().domain_class = Some(v);
    }
    for (k, v) in range_classes {
        predicate_shapes.entry(k).or_default().range_class = Some(v);
    }
    for (k, v) in range_datatypes {
        predicate_shapes.entry(k).or_default().range_datatype = Some(v);
    }

    // SHACL fold: for each property shape, look up its predicate name via
    // `sh:path` and fold the shape's `sh:datatype` / `sh:class` /
    // `sh:nodeKind` onto the matching predicate. First-declared wins per
    // field — `rdfs:*` ran first and gets to set the canonical hint.
    for (shape_key, pred_name) in &sh_path {
        let entry = predicate_shapes.entry(pred_name.clone()).or_default();
        if entry.range_datatype.is_none()
            && let Some(dt) = sh_datatype.get(shape_key)
        {
            entry.range_datatype = Some(dt.clone());
        }
        if entry.range_class.is_none()
            && let Some(c) = sh_class.get(shape_key)
        {
            entry.range_class = Some(c.clone());
        }
        if entry.node_kind.is_none()
            && let Some(kind) = sh_node_kind.get(shape_key)
        {
            entry.node_kind = Some(*kind);
        }
    }
    // `sh:targetClass` lives on the parent node shape, not on the property
    // shape, so we have to walk `sh:property` edges from each node shape
    // back down to its property shapes and bridge the target onto every
    // predicate the node shape constrains.
    for (node_shape_key, prop_shape_key) in &sh_property_edges {
        let Some(target) = sh_target_class.get(node_shape_key) else {
            continue;
        };
        let Some(pred_name) = sh_path.get(prop_shape_key) else {
            continue;
        };
        let entry = predicate_shapes.entry(pred_name.clone()).or_default();
        if entry.domain_class.is_none() {
            entry.domain_class = Some(target.clone());
        }
    }
    // Drop any all-None entries we created defensively above.
    predicate_shapes.retain(|_, s| {
        s.domain_class.is_some()
            || s.range_class.is_some()
            || s.range_datatype.is_some()
            || s.node_kind.is_some()
    });

    SeedParse {
        predicates,
        predicate_iris,
        classes,
        class_iris: class_iri_map,
        predicate_shapes,
        class_hierarchy,
        prefixes,
    }
}

/// True if `query` already declares a SPARQL `PREFIX` for `name`.
///
/// A deliberately loose scan — it looks for the `prefix` keyword
/// followed by `name:` (case-insensitive, whitespace-tolerant), which
/// is enough to keep [`SeedOntology::sparql_prologue`] from emitting a
/// redefinition. It does not parse the query, so a prefix name buried
/// in a string literal could yield a false positive; the cost is only
/// a missing convenience prefix, never a malformed query.
fn query_declares_prefix(query: &str, name: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    let needle = format!("{}:", name.to_ascii_lowercase());
    lower.match_indices("prefix").any(|(i, _)| {
        lower[i + "prefix".len()..]
            .trim_start()
            .starts_with(&needle)
    })
}

/// Local-name of an IRI: the substring after the last `#`, `/`, or `:`.
fn local_name(iri: &str) -> Option<String> {
    let s = iri.trim_matches(|c| c == '<' || c == '>');
    let cut = s
        .rfind('#')
        .or_else(|| s.rfind('/'))
        .or_else(|| s.rfind(':'))
        .map_or(0, |i| i + 1);
    let name = &s[cut..];
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

#[doc(hidden)]
/// Test-only entry point returning `(predicates, classes)` local names.
/// Production code calls [`parse_seed`] directly.
#[cfg(test)]
fn extract_terms(turtle: &str) -> (Vec<String>, Vec<String>) {
    let parsed = parse_seed(turtle);
    (parsed.predicates, parsed.classes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Path to the crate's `ontologies/` directory, resolved via
    /// `CARGO_MANIFEST_DIR` so tests work regardless of the caller's CWD.
    fn ontologies_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ontologies")
    }

    fn load_pack(name: &str) -> SeedOntology {
        SeedOntology::load(ontologies_dir().join(format!("{name}.ttl")))
            .unwrap_or_else(|e| panic!("failed to load {name}: {e}"))
    }

    #[test]
    fn parse_seed_records_subclass_of_edges() {
        // `rdfs:subClassOf` triples in the source should land in the
        // `ClassHierarchy`, indexed by local name so the validator can
        // match abstract domains via transitive subclass inference.
        let turtle = "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                      @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                      @prefix ex: <http://example.org/onto#> .\n\
                      ex:Event a owl:Class .\n\
                      ex:Span a owl:Class ; rdfs:subClassOf ex:Event .\n\
                      ex:ToolCall a owl:Class ; rdfs:subClassOf ex:Span .\n";
        let parsed = parse_seed(turtle);
        assert!(parsed.class_hierarchy.is_a("Span", "Event"));
        assert!(parsed.class_hierarchy.is_a("ToolCall", "Event"));
        assert!(!parsed.class_hierarchy.is_a("Event", "Span"));
    }

    #[test]
    fn merged_unions_class_hierarchies() {
        // Edges declared in one pack survive a merge with another pack
        // (which adds its own edges).
        let first = SeedOntology::from_string(
            "first",
            "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             @prefix ex: <http://example.org/onto#> .\n\
             ex:ToolCall a owl:Class ; rdfs:subClassOf ex:Event .\n",
        );
        let second = SeedOntology::from_string(
            "second",
            "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             @prefix ex: <http://example.org/onto#> .\n\
             ex:Commit a owl:Class ; rdfs:subClassOf ex:Artifact .\n",
        );
        let merged = SeedOntology::merged("m", [first, second]);
        assert!(merged.class_hierarchy.is_a("ToolCall", "Event"));
        assert!(merged.class_hierarchy.is_a("Commit", "Artifact"));
    }

    #[test]
    fn parse_seed_picks_up_shacl_property_shape() {
        // SHACL property shape attached to a node shape with a
        // targetClass — exercises the full SHACL fold (sh:targetClass,
        // sh:path, sh:datatype, sh:class, sh:nodeKind).
        let turtle = "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                      @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                      @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                      @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
                      @prefix ex: <http://example.org/onto#> .\n\
                      ex:Event a owl:Class .\n\
                      ex:Latency a owl:Class .\n\
                      ex:has_latency a owl:DatatypeProperty .\n\
                      ex:produced a owl:ObjectProperty .\n\
                      ex:EventShape a sh:NodeShape ;\n\
                          sh:targetClass ex:Event ;\n\
                          sh:property [ sh:path ex:has_latency ;\n\
                                        sh:datatype xsd:decimal ;\n\
                                        sh:nodeKind sh:Literal ] ;\n\
                          sh:property [ sh:path ex:produced ;\n\
                                        sh:class ex:Latency ;\n\
                                        sh:nodeKind sh:IRI ] .\n";
        let parsed = parse_seed(turtle);
        let lat = parsed.predicate_shapes.get("has_latency").unwrap();
        // sh:targetClass on the parent node shape becomes domain_class.
        assert_eq!(lat.domain_class.as_deref(), Some("Event"));
        // sh:datatype xsd:decimal becomes range_datatype.
        assert_eq!(lat.range_datatype.as_deref(), Some("decimal"));
        assert_eq!(lat.node_kind, Some(NodeKind::Literal));
        let prod = parsed.predicate_shapes.get("produced").unwrap();
        assert_eq!(prod.domain_class.as_deref(), Some("Event"));
        assert_eq!(prod.range_class.as_deref(), Some("Latency"));
        assert_eq!(prod.node_kind, Some(NodeKind::Iri));
    }

    #[test]
    fn parse_seed_splits_xsd_range_into_datatype_slot() {
        // `rdfs:range xsd:integer` should land in range_datatype, never
        // range_class — split_range_iri is the single point of truth.
        let turtle = "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                      @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                      @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
                      @prefix ex: <http://example.org/onto#> .\n\
                      ex:token_count a owl:DatatypeProperty ;\n\
                          rdfs:range xsd:integer .\n";
        let parsed = parse_seed(turtle);
        let tc = parsed.predicate_shapes.get("token_count").unwrap();
        assert_eq!(tc.range_datatype.as_deref(), Some("integer"));
        assert_eq!(tc.range_class, None);
    }

    #[test]
    fn core_pack_carries_primitives() {
        let s = load_pack("core");
        assert_eq!(s.name, "core");
        assert!(s.predicates.contains(&"produced".to_owned()));
        assert!(s.predicates.contains(&"consumed".to_owned()));
        assert!(s.predicates.contains(&"errored_with".to_owned()));
        assert!(s.predicates.contains(&"resolved_by".to_owned()));
        assert!(s.predicates.contains(&"superseded_by".to_owned()));
        assert!(s.classes.contains(&"Actor".to_owned()));
        assert!(s.classes.contains(&"Artifact".to_owned()));
        assert!(s.classes.contains(&"Event".to_owned()));
        assert!(s.classes.contains(&"Failure".to_owned()));
        assert!(s.classes.contains(&"Resolution".to_owned()));
    }

    #[test]
    fn llm_pack_has_provider_and_usage_terms() {
        let s = load_pack("llm");
        assert!(s.predicates.contains(&"used_model".to_owned()));
        assert!(s.predicates.contains(&"hit_provider".to_owned()));
        assert!(s.predicates.contains(&"has_token_usage".to_owned()));
        assert!(s.classes.contains(&"Provider".to_owned()));
        assert!(s.classes.contains(&"TokenUsage".to_owned()));
        assert!(s.classes.contains(&"RateLimit".to_owned()));
    }

    #[test]
    fn core_pack_records_curated_predicate_iris() {
        let s = load_pack("core");
        // Every predicate the pack declares should appear in the bridge
        // map at its full TBox IRI — that's what makes ABox/TBox align.
        assert_eq!(
            s.predicate_iris.get("errored_with").map(String::as_str),
            Some("urn:ontrace:onto/core/errored_with"),
        );
        assert_eq!(
            s.predicate_iris.get("produced").map(String::as_str),
            Some("urn:ontrace:onto/core/produced"),
        );
        // Every local name in `predicates` has a corresponding IRI entry.
        for name in &s.predicates {
            assert!(
                s.predicate_iris.contains_key(name),
                "predicate {name:?} missing from iri bridge: {:?}",
                s.predicate_iris,
            );
        }
    }

    #[test]
    fn merged_unions_predicate_iri_maps() {
        let core = load_pack("core");
        let agent = load_pack("agent");
        let merged = SeedOntology::merged("core_agent", [core, agent]);
        // Predicate from core.
        assert!(merged.predicate_iris.contains_key("produced"));
        // Predicate from agent.
        assert!(merged.predicate_iris.contains_key("invoked"));
        // Every predicate name has an IRI entry.
        for name in &merged.predicates {
            assert!(
                merged.predicate_iris.contains_key(name),
                "{name:?} missing from merged bridge",
            );
        }
    }

    #[test]
    fn agent_pack_has_loop_terms() {
        let s = load_pack("agent");
        assert!(s.predicates.contains(&"invoked".to_owned()));
        assert!(s.predicates.contains(&"called_tool".to_owned()));
        assert!(s.predicates.contains(&"pursues".to_owned()));
        assert!(s.classes.contains(&"Agent".to_owned()));
        assert!(s.classes.contains(&"ToolCall".to_owned()));
        assert!(s.classes.contains(&"Objective".to_owned()));
    }

    #[test]
    fn coding_pack_has_workflow_terms() {
        let s = load_pack("coding");
        assert!(s.predicates.contains(&"edited_file".to_owned()));
        assert!(s.predicates.contains(&"ran_test".to_owned()));
        assert!(s.predicates.contains(&"applied_pattern".to_owned()));
        assert!(s.classes.contains(&"PullRequest".to_owned()));
        assert!(s.classes.contains(&"DesignPattern".to_owned()));
    }

    #[test]
    fn eval_pack_has_scoring_terms() {
        let s = load_pack("eval");
        assert!(s.predicates.contains(&"used_dataset".to_owned()));
        assert!(s.predicates.contains(&"judged_by".to_owned()));
        assert!(s.predicates.contains(&"regressed_on".to_owned()));
        assert!(s.classes.contains(&"EvalRun".to_owned()));
        assert!(s.classes.contains(&"Rubric".to_owned()));
    }

    #[test]
    fn tracing_pack_has_span_terms() {
        let s = load_pack("tracing");
        assert!(s.predicates.contains(&"belongs_to_trace".to_owned()));
        assert!(s.classes.contains(&"Span".to_owned()));
        assert!(s.classes.contains(&"Trace".to_owned()));
        // child_of is the generic DAG edge — lifted into core, shared by
        // Spans and conversation Messages.
        assert!(
            load_pack("core")
                .predicates
                .contains(&"child_of".to_owned())
        );
    }

    #[test]
    fn harness_pack_has_runtime_terms() {
        let s = load_pack("harness");
        assert!(s.classes.contains(&"Capability".to_owned()));
        assert!(s.classes.contains(&"MCPServer".to_owned()));
        assert!(s.classes.contains(&"PermissionMode".to_owned()));
        assert!(s.predicates.contains(&"has_capability".to_owned()));
        assert!(s.predicates.contains(&"runs_as".to_owned()));
    }

    #[test]
    fn user_pack_has_profile_intent_and_environment_terms() {
        let s = load_pack("user");
        assert!(s.predicates.contains(&"has_role".to_owned()));
        assert!(s.predicates.contains(&"has_preference".to_owned()));
        assert!(s.predicates.contains(&"has_intent".to_owned()));
        assert!(s.predicates.contains(&"imposed_constraint".to_owned()));
        assert!(s.predicates.contains(&"gave_feedback".to_owned()));
        assert!(s.predicates.contains(&"works_in".to_owned()));
        assert!(s.predicates.contains(&"in_session".to_owned()));
        assert!(s.classes.contains(&"Role".to_owned()));
        assert!(s.classes.contains(&"Intent".to_owned()));
        assert!(s.classes.contains(&"Constraint".to_owned()));
        assert!(s.classes.contains(&"Environment".to_owned()));
        assert!(s.classes.contains(&"Session".to_owned()));
        assert!(s.classes.contains(&"Feedback".to_owned()));
    }

    #[test]
    fn core_pack_default_policy_is_strict() {
        let s = load_pack("core");
        assert!(
            matches!(s.policy, OntologyPolicy::Strict),
            "loaded packs default to Strict; got {:?}",
            s.policy,
        );
    }

    #[test]
    fn with_policy_overrides_default() {
        let s = SeedOntology::from_string("inline", "").with_policy(OntologyPolicy::CatchAll {
            catchall_predicate: "related_to".to_owned(),
            catchall_iri: None,
        });
        assert!(matches!(s.policy, OntologyPolicy::CatchAll { .. }));
    }

    #[test]
    fn load_dir_picks_up_all_packs_sorted() {
        let packs = SeedOntology::load_dir(ontologies_dir()).unwrap();
        let names: Vec<_> = packs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "agent", "coding", "core", "eval", "harness", "llm", "tracing", "user"
            ]
        );
    }

    #[test]
    fn merged_unions_layered_packs() {
        let core = load_pack("core");
        let llm = load_pack("llm");
        let agent = load_pack("agent");
        let merged = SeedOntology::merged("agent_loop", [core, llm, agent]);
        assert_eq!(merged.name, "agent_loop");
        // LLM substrate present.
        assert!(merged.predicates.contains(&"used_model".to_owned()));
        // Agent loop present.
        assert!(merged.predicates.contains(&"invoked".to_owned()));
        // Sorted + deduped.
        let mut sorted = merged.predicates.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(merged.predicates, sorted);
        // Turtle source is concatenated (Oxigraph TBox loader sees everything).
        assert!(merged.turtle.contains("ontrace core"));
        assert!(merged.turtle.contains("ontrace llm"));
        assert!(merged.turtle.contains("ontrace agent"));
        // All packs default to Strict, so the merge stays Strict.
        assert!(matches!(merged.policy, OntologyPolicy::Strict));
    }

    #[cfg(feature = "oxigraph")]
    #[test]
    fn merged_via_oxigraph_unions_packs_with_conflicting_prefixes() {
        // `core` and `agent` both declare `@prefix ont:` to different IRIs.
        // Verbatim concat (the `merged` path) works only because each file
        // redeclares all its prefixes; the oxigraph path is robust by
        // construction — each pack is parsed in isolation.
        let core = load_pack("core");
        let agent = load_pack("agent");
        let merged = SeedOntology::merged_via_oxigraph("agent_loop", [core, agent]).unwrap();
        assert_eq!(merged.name, "agent_loop");
        // Predicate from `core`.
        assert!(merged.predicates.contains(&"produced".to_owned()));
        // Predicate from `agent`.
        assert!(merged.predicates.contains(&"invoked".to_owned()));
        // Class from each pack.
        assert!(merged.classes.contains(&"Actor".to_owned()));
        assert!(merged.classes.contains(&"Agent".to_owned()));
        // All packs default to Strict, so the merge stays Strict.
        assert!(matches!(merged.policy, OntologyPolicy::Strict));
        // The dumped Turtle is parseable by oxigraph again (round-trip safe).
        let store = oxigraph::store::Store::new().unwrap();
        store
            .load_from_reader(
                oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::Turtle),
                merged.turtle.as_bytes(),
            )
            .expect("merged turtle must re-parse");
    }

    #[cfg(feature = "oxigraph")]
    #[test]
    fn merged_via_oxigraph_handles_zero_packs() {
        let empty = SeedOntology::merged_via_oxigraph("none", std::iter::empty()).unwrap();
        assert!(empty.predicates.is_empty());
        assert!(empty.classes.is_empty());
        assert!(matches!(empty.policy, OntologyPolicy::Strict));
    }

    #[test]
    fn from_string_parses_inline_turtle() {
        let ttl = "\
            @prefix ex: <urn:x/> .\n\
            @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
            @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
            ex:Foo a owl:Class ;\n\
                rdfs:label \"Foo\" .\n\
            ex:bar a owl:ObjectProperty ;\n\
                rdfs:domain ex:Foo ;\n\
                rdfs:range ex:Foo .\n";
        let s = SeedOntology::from_string("inline", ttl);
        assert_eq!(s.name, "inline");
        assert_eq!(s.predicates, vec!["bar".to_owned()]);
        assert_eq!(s.classes, vec!["Foo".to_owned()]);
    }

    #[test]
    fn extract_terms_ignores_unrecognised_types() {
        let ttl = "@prefix ex: <urn:x/> .\n\
                   ex:foo a ex:Whatever .\n";
        let (preds, classes) = extract_terms(ttl);
        assert!(preds.is_empty());
        assert!(classes.is_empty());
    }

    #[test]
    fn extract_terms_strips_line_comments() {
        let ttl = "# top-level comment\n\
                   @prefix ex: <urn:x/> .\n\
                   @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                   ex:Foo a owl:Class . # trailing comment\n\
                   # ex:bar a owl:ObjectProperty .\n";
        let (preds, classes) = extract_terms(ttl);
        assert!(preds.is_empty(), "commented-out predicate must not parse");
        assert_eq!(classes, vec!["Foo".to_owned()]);
    }

    #[test]
    fn parse_handles_dot_in_comment_followed_by_hash_iri() {
        // A regex scanner would drop the closing `.` of a statement when
        // the next line carries an IRI with a `#`, which merges the next
        // declaration's subject into the previous one. The bundled
        // `core.ttl` `instance_of` declaration has exactly that shape; this
        // is the minimal case.
        let ttl = "@prefix ex: <urn:x/> .\n\
                   @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                   ex:before a owl:ObjectProperty ;\n\
                       rdfs:comment \"a.b. mention with periods\" ;\n\
                       owl:equivalentProperty <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> .\n\
                   ex:after a owl:ObjectProperty .\n";
        let (preds, _) = extract_terms(ttl);
        assert!(preds.contains(&"before".to_owned()), "got: {preds:?}");
        assert!(preds.contains(&"after".to_owned()), "got: {preds:?}");
    }

    #[test]
    fn from_string_collects_prefix_declarations() {
        let ttl = "\
            @prefix ex: <urn:x/> .\n\
            @prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
            ex:Foo a owl:Class .\n";
        let s = SeedOntology::from_string("inline", ttl);
        assert_eq!(s.prefixes.get("ex").map(String::as_str), Some("urn:x/"));
        assert_eq!(
            s.prefixes.get("owl").map(String::as_str),
            Some("http://www.w3.org/2002/07/owl#"),
        );
    }

    #[test]
    fn core_pack_carries_its_namespace_prefix() {
        let s = load_pack("core");
        // Every bundled pack binds at least its own `ont:`-style prefix;
        // the prologue helper needs them to be parsed off the source.
        assert!(
            !s.prefixes.is_empty(),
            "core pack should expose @prefix declarations: {:?}",
            s.prefixes,
        );
    }

    #[test]
    fn sparql_prologue_emits_sorted_unused_prefixes() {
        let ttl = "\
            @prefix b: <urn:b/> .\n\
            @prefix a: <urn:a/> .\n\
            a:Thing a <http://www.w3.org/2002/07/owl#Class> .\n";
        let s = SeedOntology::from_string("inline", ttl);
        let prologue = s.sparql_prologue("SELECT * WHERE { ?s ?p ?o }");
        assert_eq!(
            prologue, "PREFIX a: <urn:a/>\nPREFIX b: <urn:b/>\n",
            "prologue must be sorted by prefix name",
        );
    }

    #[test]
    fn sparql_prologue_skips_prefixes_the_query_already_declares() {
        let ttl = "\
            @prefix a: <urn:a/> .\n\
            @prefix b: <urn:b/> .\n\
            a:Thing a <http://www.w3.org/2002/07/owl#Class> .\n";
        let s = SeedOntology::from_string("inline", ttl);
        let prologue = s.sparql_prologue("PREFIX a: <urn:other/>\nSELECT * WHERE { ?s ?p ?o }");
        assert_eq!(
            prologue, "PREFIX b: <urn:b/>\n",
            "a prefix already declared by the query must not be re-emitted",
        );
    }

    #[test]
    fn merged_unions_prefix_maps_first_wins() {
        let first = SeedOntology::from_string(
            "first",
            "@prefix shared: <urn:first/> .\n@prefix only_a: <urn:a/> .\n",
        );
        let second = SeedOntology::from_string(
            "second",
            "@prefix shared: <urn:second/> .\n@prefix only_b: <urn:b/> .\n",
        );
        let merged = SeedOntology::merged("m", [first, second]);
        assert_eq!(
            merged.prefixes.get("shared").map(String::as_str),
            Some("urn:first/"),
            "first pack to bind a prefix name wins",
        );
        assert!(merged.prefixes.contains_key("only_a"));
        assert!(merged.prefixes.contains_key("only_b"));
    }

    #[test]
    fn local_name_handles_iri_brackets_and_separators() {
        assert_eq!(
            local_name("<urn:ontrace:onto/agent/Agent>").as_deref(),
            Some("Agent")
        );
        assert_eq!(local_name("ex:works_at").as_deref(), Some("works_at"));
        assert_eq!(
            local_name("<http://example.org/onto#works_at>").as_deref(),
            Some("works_at")
        );
    }
}
