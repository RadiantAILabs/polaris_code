//! The code knowledge-graph: deterministic syntax indexers plus a SPARQL-backed
//! navigation toolset.
//!
//! [`CodeIndexer`] parses tracked Rust, Python, JavaScript, TypeScript, and TSX
//! sources and writes structural facts — files, symbols, definitions, call
//! edges, and explicit trait/interface implementations — into the
//! **Code `ABox`** named graph ([`CODE_GRAPH_IRI`]) of the shared
//! [`OxigraphStore`]. This is deterministic and runs off the agent's hot path
//! (at boot, and incrementally); it is never an LLM call.
//!
//! [`CodeGraphTools`] exposes that graph to the agent as navigation tools
//! (`find_definition`, `find_references`, `callers_of`, `callees_of`,
//! `impls_of_trait`), each a parameterized SPARQL `SELECT`. These are the
//! structural queries plain text search cannot answer: typed lookups and
//! caller/callee traversal over the call graph.
//!
//! The triple schema (subjects/predicates under [`NS`]) is private to this
//! module — every other module reaches the Code `ABox` only through the
//! tool surface, so the schema can evolve without ripple.
//!
//! # What a graph answer is allowed to claim
//!
//! An index is a claim about a tree, and a claim has to say when it stops being
//! one. Two ways of not being one are indistinguishable from an ordinary
//! negative unless the host says otherwise, and both are answered here:
//!
//! - **Readiness.** The boot index runs off the hot path, so a query can arrive
//!   before it lands. Every tool reads the index's own phase first, and while a
//!   build is in flight it returns an instruction to wait rather than an empty
//!   result — see [`CodeIndexState`].
//! - **Freshness.** `edit_file` re-indexes what it wrote, but a `git checkout`,
//!   a code generator or the agent's own `run_command` does not. The host
//!   records a `(mtime, size)` stamp beside every indexed file and reconciles
//!   on two triggers, both proportional to the answer rather than to the tree:
//!   an answer that *cites* files re-reads only the cited files whose stamp
//!   moved, before answering; an answer that cites *nothing* — the negative a
//!   model reads as "it does not exist" — stats the working tree and re-reads
//!   only what moved before the negative is allowed to stand.
//!
//! Neither trigger walks the tree on a query that resolved, and neither
//! re-parses a file whose stamp is unchanged. The accepted failure mode is
//! stated on [`FileStamp`].

use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};
use oxigraph::sparql::QueryResults;
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolRegistry, ToolsPlugin, toolset};
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sparql::{enc, esc};
use crate::tools::code::{CodeToolsConfig, cap_footer, line_view};

mod rust;
mod syntax;

/// IRI of the Code `ABox` named graph (structural facts about the repository).
pub const CODE_GRAPH_IRI: &str = "urn:polaris:graph:code";

/// IRI of the enrichment named graph: conceptual roles derived from the
/// syntactic Code `ABox` by the [`enrich`] reasoning pass.
pub const ENRICHED_GRAPH_IRI: &str = "urn:polaris:graph:code-enriched";

/// Namespace for code-graph classes and predicates.
pub const NS: &str = "urn:polaris:code:";

/// The conceptual-role vocabulary the [`enrich`] pass derives — also the
/// allow-list for the curated `find_by_role` concept query.
pub const ROLES: &[&str] = &["ToolHandler", "SystemHandler", "Plugin", "Agent"];

/// `rdf:type`.
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Hard cap on serialized tool-result size, mirroring the SPARQL toolset.
const RESULT_BYTE_LIMIT: usize = 32 * 1024;

/// Named graph the host asserts the index's *own* state into.
///
/// Separate from [`CODE_GRAPH_IRI`] on purpose: a full re-index clears the
/// facts about the repository, and the facts about the index must survive that
/// so a concurrent query can still be told a build is in flight.
const INDEX_GRAPH_IRI: &str = "urn:polaris:graph:code-index";

/// The single subject in [`INDEX_GRAPH_IRI`] carrying the index's state.
const INDEX_IRI: &str = "urn:polaris:code:index";

// ─── Index state: what the host asserts about its own index ─────────────────

/// Where the code index is in its lifecycle.
///
/// Read with [`code_index_state`]. This is the distinction a graph answer
/// depends on: only [`Ready`](Self::Ready) entitles a tool to return an empty
/// result as a negative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IndexPhase {
    /// No index has been started against this store. A host that never added
    /// [`CodeGraphPlugin`] sits here forever, which is why it is the default.
    #[default]
    Unstarted,
    /// A full index is in flight. Queries cannot be answered yet.
    Building,
    /// The last full index completed. Queries may be answered.
    Ready,
    /// The last full index failed; the graph is empty or partial.
    ///
    /// `index_tree` skips a file it cannot parse rather than failing, so today
    /// only a panicking blocking task reaches this. It exists because the
    /// alternative to naming the case is answering a query from a graph that
    /// is missing an unknown number of symbols without saying so.
    Failed,
}

/// A snapshot of what the code index says about itself.
///
/// Every field is a fact the host asserted into [`INDEX_GRAPH_IRI`], so the
/// snapshot is shared by every holder of the store rather than owned by one
/// toolset — the readiness gate, the negative-answer footer and the terminal
/// status row all read the same assertion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeIndexState {
    /// Where the index is in its lifecycle.
    pub phase: IndexPhase,
    /// Source files carrying facts as of the last completed pass.
    pub files: u64,
    /// How long the in-flight build has been running, when one is.
    pub building_for: Option<Duration>,
    /// Age of the last completed full index.
    pub indexed_ago: Option<Duration>,
    /// Age of the last completed reconciliation against the working tree.
    pub reconciled_ago: Option<Duration>,
    /// How many tree reconciliations this index has performed.
    ///
    /// The performance guard reads this: a query that resolved must not add to
    /// it (`tests/code_graph_freshness.rs`).
    pub reconciliations: u64,
    /// How many files those reconciliations re-read and re-parsed.
    pub refreshed_files: u64,
    /// Why the last build failed, when it did.
    pub failure: Option<String>,
}

/// Read the index's own state out of `store`.
///
/// Cheap: one `SELECT` over a single subject. Safe against a store that has
/// never been indexed — that reports [`IndexPhase::Unstarted`].
///
/// # Examples
///
/// ```
/// use polaris_code_core::tools::code_graph::{IndexPhase, code_index_state};
/// use polaris_ontrace::oxigraph::OxigraphStore;
///
/// let store = OxigraphStore::in_memory().unwrap();
/// assert_eq!(code_index_state(&store).phase, IndexPhase::Unstarted);
/// ```
#[must_use]
pub fn code_index_state(store: &OxigraphStore) -> CodeIndexState {
    let facts = index_facts(store);
    let age = |key: &str| {
        facts
            .get(key)
            .and_then(|v| v.parse::<u128>().ok())
            .map(since_nanos)
    };
    let count = |key: &str| facts.get(key).and_then(|v| v.parse::<u64>().ok());
    CodeIndexState {
        phase: match facts.get("phase").map(String::as_str) {
            Some("building") => IndexPhase::Building,
            Some("ready") => IndexPhase::Ready,
            Some("failed") => IndexPhase::Failed,
            _ => IndexPhase::Unstarted,
        },
        files: count("fileCount").unwrap_or(0),
        building_for: age("startedAt"),
        indexed_ago: age("indexedAt"),
        reconciled_ago: age("reconciledAt"),
        reconciliations: count("reconciliations").unwrap_or(0),
        refreshed_files: count("refreshedFiles").unwrap_or(0),
        failure: facts.get("failure").cloned(),
    }
}

/// Every `<index> ?p ?o` fact, keyed by the predicate's local name.
fn index_facts(store: &OxigraphStore) -> BTreeMap<String, String> {
    let q =
        format!("SELECT ?p ?o WHERE {{ GRAPH <{INDEX_GRAPH_IRI}> {{ <{INDEX_IRI}> ?p ?o }} }}",);
    let Ok(QueryResults::Solutions(solutions)) = store.query(&q) else {
        return BTreeMap::new();
    };
    let mut facts = BTreeMap::new();
    for solution in solutions.flatten() {
        let (Some(p), Some(o)) = (solution.get("p"), solution.get("o")) else {
            continue;
        };
        if let Some(local) = term_str(p).strip_prefix(NS) {
            facts.insert(local.to_string(), term_str(o));
        }
    }
    facts
}

/// Replace each named index fact, leaving the others alone.
fn put_index_facts(store: &OxigraphStore, facts: &[(&str, String)]) {
    let mut update = String::new();
    for (predicate, _) in facts {
        update.push_str(&format!(
            "DELETE WHERE {{ GRAPH <{INDEX_GRAPH_IRI}> {{ <{INDEX_IRI}> <{NS}{predicate}> ?o }} }};",
        ));
    }
    update.push_str(&format!("INSERT DATA {{ GRAPH <{INDEX_GRAPH_IRI}> {{ "));
    for (predicate, value) in facts {
        update.push_str(&format!(
            "<{INDEX_IRI}> <{NS}{predicate}> \"{}\" . ",
            esc(value)
        ));
    }
    update.push_str("} }");
    if let Err(error) = store.update(&update) {
        tracing::warn!("code graph: could not record index state: {error}");
    }
}

/// Drop a single index fact (used to retract a stale failure reason).
fn clear_index_fact(store: &OxigraphStore, predicate: &str) {
    let _ = store.update(&format!(
        "DELETE WHERE {{ GRAPH <{INDEX_GRAPH_IRI}> {{ <{INDEX_IRI}> <{NS}{predicate}> ?o }} }}",
    ));
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn since_nanos(then: u128) -> Duration {
    Duration::from_nanos(u64::try_from(now_nanos().saturating_sub(then)).unwrap_or(u64::MAX))
}

/// Render an age the way a sentence to a model should read it.
fn ago(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{}ms", d.subsec_millis()),
        1..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// What a file looked like when it was indexed.
///
/// **The accepted failure mode.** Modification time and size, not a content
/// digest: a digest would mean re-reading every file to check one, which is the
/// throughput trade this design refuses. A rewrite is therefore missed only if
/// it lands within the filesystem's mtime granularity *and* leaves the length
/// unchanged. On Linux (nanosecond `st_mtim`) that window is empty in practice;
/// on a filesystem with one-second timestamps, or against a tool that restores
/// mtimes (`tar -p`, `rsync --times`, `git` does not), a same-length rewrite in
/// the same tick survives until the next reconciliation notices nothing and the
/// stale facts persist. `edit_file` does not depend on this path — it re-indexes
/// what it wrote — so the exposure is limited to changes made outside the
/// toolset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FileStamp {
    mtime_nanos: u128,
    len: u64,
}

impl FileStamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        Self {
            mtime_nanos,
            len: meta.len(),
        }
    }

    fn at(path: &Path) -> Option<Self> {
        std::fs::metadata(path).ok().map(|m| Self::of(&m))
    }
}

// ─── IRI minting ──────────────────────────────────────────────────────────

fn named(iri: String) -> NamedNode {
    NamedNode::new_unchecked(iri)
}

/// Predicate/class IRI for a local name in [`NS`].
fn term(local: &str) -> NamedNode {
    named(format!("{NS}{local}"))
}

fn file_iri(rel: &str) -> NamedNode {
    named(format!("{NS}file/{}", enc(rel)))
}

fn sym_iri(rel: &str, class: &str, name: &str, line: usize) -> NamedNode {
    named(format!("{NS}sym/{}/{class}/{}/{line}", enc(rel), enc(name)))
}

// ─── Indexer ────────────────────────────────────────────────────────────────

/// Deterministic source → triples indexer for the Code `ABox`.
pub struct CodeIndexer;

impl CodeIndexer {
    /// Parse every supported source file under `root` into the Code `ABox`,
    /// replacing any prior contents of the graph. Returns the number of quads
    /// written.
    ///
    /// The walk + parse runs on a blocking thread; the store handle is cheap to
    /// clone and shares the underlying dataset.
    ///
    /// # Errors
    ///
    /// Returns an error if the blocking task panics or store IO fails.
    /// The index announces itself: [`IndexPhase::Building`] is asserted before
    /// the walk starts and [`IndexPhase::Ready`] (or [`IndexPhase::Failed`])
    /// after it finishes, so a query racing a cold index is told to wait
    /// instead of being handed an empty result.
    pub async fn index_repo(store: &OxigraphStore, root: &Path) -> anyhow::Result<usize> {
        // Replace the whole graph for a full re-index. The index's own state
        // lives in a different named graph and deliberately survives this.
        let _ = store.update(&format!("CLEAR GRAPH <{CODE_GRAPH_IRI}>"));
        put_index_facts(
            store,
            &[
                ("phase", "building".to_string()),
                ("startedAt", now_nanos().to_string()),
            ],
        );
        clear_index_fact(store, "failure");
        let worker = store.clone();
        let root = root.to_path_buf();
        let outcome = tokio::task::spawn_blocking(move || index_tree(&worker, &root))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|inner| inner);
        match outcome {
            Ok(written) => {
                // Derive the conceptual roles before announcing readiness, so
                // `ready` means both graphs are populated: a concept query that
                // sees `ready` must not read an enrichment graph that is still
                // empty and report it as "this codebase has no plugins".
                if let Err(err) = enrich(store) {
                    tracing::warn!("code graph: enrichment failed: {err}");
                }
                let now = now_nanos().to_string();
                put_index_facts(
                    store,
                    &[
                        ("phase", "ready".to_string()),
                        ("fileCount", indexed_file_count(store).to_string()),
                        ("indexedAt", now.clone()),
                        // A full index *is* a reconciliation with the tree.
                        ("reconciledAt", now),
                    ],
                );
                Ok(written)
            }
            Err(error) => {
                put_index_facts(
                    store,
                    &[
                        ("phase", "failed".to_string()),
                        ("failure", error.to_string()),
                    ],
                );
                Err(error)
            }
        }
    }

    /// Re-index a specific set of files (incremental). Each file's prior facts
    /// are deleted before its fresh facts are written.
    ///
    /// # Errors
    ///
    /// Returns an error if the blocking task panics or store IO fails.
    pub async fn index_paths(
        store: &OxigraphStore,
        root: &Path,
        paths: &[std::path::PathBuf],
    ) -> anyhow::Result<usize> {
        let store = store.clone();
        let root = root.to_path_buf();
        let paths = paths.to_vec();
        let written = tokio::task::spawn_blocking(move || {
            let mut total = 0;
            for abs in &paths {
                let Ok(rel) = abs.strip_prefix(&root) else {
                    continue;
                };
                let rel = rel.to_string_lossy().to_string();
                clear_file(&store, &rel);
                match index_file(&store, abs, &rel) {
                    Ok(written) => total += written,
                    Err(error) => tracing::warn!(
                        path = %abs.display(),
                        "code graph: skipped file after indexing failure: {error}"
                    ),
                }
            }
            anyhow::Ok(total)
        })
        .await??;
        Ok(written)
    }
}

/// Delete every triple about one file (the file node and its symbols).
fn clear_file(store: &OxigraphStore, rel: &str) {
    let f = file_iri(rel);
    let q = format!(
        "DELETE WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
         ?s <{NS}definedIn> <{iri}> ; ?p ?o . }} }};\
         DELETE WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ <{iri}> ?p ?o . }} }}",
        iri = f.as_str(),
    );
    let _ = store.update(&q);
}

/// Every supported source file under `root`, keyed by working-directory-relative
/// path, with the stamp `readdir` already had to produce.
///
/// The one place the tree is walked. Indexing walks it to decide what to parse;
/// reconciliation walks it to decide what *not* to.
fn source_stamps(root: &Path) -> BTreeMap<String, FileStamp> {
    let mut stamps = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            // Never prune the walk root itself (its name may legitimately start
            // with '.', e.g. a tempdir); only skip generated/dependency/cache
            // subentries with unambiguous conventional names.
            let name = e.file_name().to_string_lossy();
            e.depth() == 0
                || !(name.starts_with('.')
                    || matches!(
                        name.as_ref(),
                        "target" | "node_modules" | "__pycache__" | "site-packages" | "venv"
                    ))
        })
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !entry.file_type().is_file() || SourceLanguage::for_path(path).is_none() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        stamps.insert(rel.to_string_lossy().to_string(), FileStamp::of(&meta));
    }
    stamps
}

fn index_tree(store: &OxigraphStore, root: &Path) -> anyhow::Result<usize> {
    let mut total = 0;
    for rel in source_stamps(root).keys() {
        let path = root.join(rel);
        match index_file(store, &path, rel) {
            Ok(written) => total += written,
            Err(error) => tracing::warn!(
                path = %path.display(),
                "code graph: skipped file after indexing failure: {error}"
            ),
        }
    }
    Ok(total)
}

/// Parse one supported file and insert its facts. Parse failures and unsupported
/// extensions are skipped (return 0).
fn index_file(store: &OxigraphStore, abs: &Path, rel: &str) -> anyhow::Result<usize> {
    // Stamp before reading, never after: a stamp that is older than the bytes
    // we parsed costs one redundant re-read later, whereas a stamp that is
    // newer would hide a change forever.
    let stamp = FileStamp::at(abs).unwrap_or_default();
    let src = std::fs::read_to_string(abs)?;
    let Some(language) = SourceLanguage::for_path(abs) else {
        return Ok(0);
    };
    let mut facts = FactEmitter::new(rel, language.id(), stamp);
    match language {
        SourceLanguage::Rust if !rust::extract(&src, &mut facts) => return Ok(0),
        SourceLanguage::Rust => {}
        SourceLanguage::Python
        | SourceLanguage::JavaScript
        | SourceLanguage::TypeScript
        | SourceLanguage::Tsx => syntax::extract(language, &src, &mut facts)?,
    }
    let n = facts.quads.len();
    for q in &facts.quads {
        store.insert_quad(q)?;
    }
    Ok(n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
}

impl SourceLanguage {
    fn for_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        if name.ends_with(".min.js") || name.ends_with(".min.mjs") || name.ends_with(".min.cjs") {
            return None;
        }
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript | Self::Tsx => "typescript",
        }
    }
}

/// Private normalized fact emitter shared by the `syn` and tree-sitter paths.
struct FactEmitter {
    quads: Vec<Quad>,
    graph: GraphName,
    rel: String,
    file: NamedNode,
    language: &'static str,
}

impl FactEmitter {
    fn new(rel: &str, language: &'static str, stamp: FileStamp) -> Self {
        let file = file_iri(rel);
        let mut emitter = Self {
            quads: Vec::new(),
            graph: GraphName::NamedNode(named(CODE_GRAPH_IRI.to_string())),
            rel: rel.to_string(),
            file: file.clone(),
            language,
        };
        emitter.push(
            file.clone(),
            term("path"),
            Literal::new_simple_literal(rel).into(),
        );
        emitter.push(file.clone(), named(RDF_TYPE.into()), term("File").into());
        emitter.push(
            file.clone(),
            term("language"),
            Literal::new_simple_literal(language).into(),
        );
        // What the file looked like when these facts were derived. Every
        // freshness decision this module makes is a comparison against these
        // two literals; nothing else is retained about the file's bytes.
        emitter.push(
            file.clone(),
            term("sourceMtime"),
            Literal::new_simple_literal(stamp.mtime_nanos.to_string()).into(),
        );
        emitter.push(
            file,
            term("sourceSize"),
            Literal::new_simple_literal(stamp.len.to_string()).into(),
        );
        emitter
    }

    fn push(&mut self, s: NamedNode, p: NamedNode, o: Term) {
        self.quads.push(Quad::new(s, p, o, self.graph.clone()));
    }

    /// Record a declaration and return its symbol IRI.
    ///
    /// `ident_line` is the identifier line (kept as `atLine` for back-compat and
    /// IRI identity); `start`/`end` are the *item's* full span — including
    /// attributes and doc comments — so a `read_symbol` of the span comes back
    /// clean and compilable.
    fn decl(
        &mut self,
        class: &str,
        name: &str,
        ident_line: usize,
        span: (usize, usize),
    ) -> NamedNode {
        let s = sym_iri(&self.rel, class, name, ident_line);
        self.push(s.clone(), named(RDF_TYPE.into()), term(class).into());
        self.push(
            s.clone(),
            term("name"),
            Literal::new_simple_literal(name).into(),
        );
        self.push(s.clone(), term("definedIn"), self.file.clone().into());
        self.push(
            s.clone(),
            term("atLine"),
            Literal::new_simple_literal(ident_line.to_string()).into(),
        );
        self.push(
            s.clone(),
            term("startLine"),
            Literal::new_simple_literal(span.0.to_string()).into(),
        );
        self.push(
            s.clone(),
            term("endLine"),
            Literal::new_simple_literal(span.1.to_string()).into(),
        );
        self.push(
            s.clone(),
            term("language"),
            Literal::new_simple_literal(self.language).into(),
        );
        s
    }

    fn call(&mut self, caller: &NamedNode, callee: &str) {
        self.push(
            caller.clone(),
            term("callsFn"),
            Literal::new_simple_literal(callee).into(),
        );
    }
}

// ─── Freshness: keeping an answer honest about the tree it came from ────────

/// How many source files currently carry facts.
fn indexed_file_count(store: &OxigraphStore) -> u64 {
    let q = format!(
        "SELECT (COUNT(DISTINCT ?f) AS ?n) WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
         ?f <{NS}path> ?path }} }}",
    );
    let Ok(QueryResults::Solutions(mut solutions)) = store.query(&q) else {
        return 0;
    };
    solutions
        .next()
        .and_then(Result::ok)
        .and_then(|row| row.get("n").map(term_str))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The stamp recorded for each of `paths`, in one query.
fn recorded_stamps(store: &OxigraphStore, paths: &[String]) -> BTreeMap<String, FileStamp> {
    if paths.is_empty() {
        return BTreeMap::new();
    }
    let values = paths
        .iter()
        .map(|p| format!("\"{}\"", esc(p)))
        .collect::<Vec<_>>()
        .join(" ");
    let q = format!(
        "SELECT ?path ?mtime ?size WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
         VALUES ?path {{ {values} }} \
         ?f <{NS}path> ?path ; <{NS}sourceMtime> ?mtime ; <{NS}sourceSize> ?size . }} }}",
    );
    let Ok(QueryResults::Solutions(solutions)) = store.query(&q) else {
        return BTreeMap::new();
    };
    let mut stamps = BTreeMap::new();
    for row in solutions.flatten() {
        let (Some(path), Some(mtime), Some(size)) =
            (row.get("path"), row.get("mtime"), row.get("size"))
        else {
            continue;
        };
        stamps.insert(
            term_str(path),
            FileStamp {
                mtime_nanos: term_str(mtime).parse().unwrap_or_default(),
                len: term_str(size).parse().unwrap_or_default(),
            },
        );
    }
    stamps
}

/// Every file the graph has facts about, with its recorded stamp.
fn all_recorded_stamps(store: &OxigraphStore) -> BTreeMap<String, FileStamp> {
    let q = format!(
        "SELECT ?path ?mtime ?size WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
         ?f <{NS}path> ?path ; <{NS}sourceMtime> ?mtime ; <{NS}sourceSize> ?size . }} }}",
    );
    let Ok(QueryResults::Solutions(solutions)) = store.query(&q) else {
        return BTreeMap::new();
    };
    let mut stamps = BTreeMap::new();
    for row in solutions.flatten() {
        let (Some(path), Some(mtime), Some(size)) =
            (row.get("path"), row.get("mtime"), row.get("size"))
        else {
            continue;
        };
        stamps.insert(
            term_str(path),
            FileStamp {
                mtime_nanos: term_str(mtime).parse().unwrap_or_default(),
                len: term_str(size).parse().unwrap_or_default(),
            },
        );
    }
    stamps
}

/// Which of the files an answer cites no longer match what was indexed.
///
/// One `stat` per distinct cited path and one `SELECT` — proportional to the
/// answer, never to the tree.
fn drifted_citations(store: &OxigraphStore, root: &Path, paths: &[String]) -> Vec<String> {
    let mut distinct: Vec<String> = paths.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.is_empty() {
        return Vec::new();
    }
    let recorded = recorded_stamps(store, &distinct);
    distinct
        .into_iter()
        .filter(|rel| {
            let Some(was) = recorded.get(rel) else {
                // No stamp: pre-stamp facts, or a node with no file. Nothing to
                // compare against, so nothing to claim.
                return false;
            };
            FileStamp::at(&root.join(rel)).as_ref() != Some(was)
        })
        .collect()
}

/// Re-read and re-parse exactly `rels`, then re-derive the concept layer.
async fn refresh_files(store: &OxigraphStore, root: &Path, rels: &[String]) {
    let absolute: Vec<PathBuf> = rels.iter().map(|rel| root.join(rel)).collect();
    if let Err(error) = CodeIndexer::index_paths(store, root, &absolute).await {
        tracing::warn!(
            "code graph: refreshing {} file(s) failed: {error}",
            rels.len()
        );
    }
    if let Err(error) = enrich(store) {
        tracing::warn!("code graph: enrichment after refresh failed: {error}");
    }
    let state = code_index_state(store);
    put_index_facts(
        store,
        &[
            (
                "refreshedFiles",
                (state.refreshed_files + rels.len() as u64).to_string(),
            ),
            ("fileCount", indexed_file_count(store).to_string()),
        ],
    );
}

/// Bring the graph back in line with the working tree before a negative answer
/// is allowed to stand.
///
/// Stats every source file and re-parses only those whose stamp moved, plus
/// files that appeared, minus files that are gone. This is the only path that
/// touches the whole tree, and only a query that resolved nothing can reach it:
/// an answer that found something is checked against its own citations instead.
///
/// Returns how many files were re-read, added, or dropped.
///
/// Deliberately **not** debounced on wall-clock time. A time window cannot tell
/// "we reconciled 2ms ago and nothing has changed" from "we reconciled 2ms ago
/// and then a generator wrote a file", so any window is a window in which a
/// fabricated negative gets through. The cost is bounded the other way instead:
/// the walk only stats, only a query that already found nothing reaches it, and
/// parsing is proportional to what actually moved.
async fn reconcile_tree(store: &OxigraphStore, root: &Path) -> usize {
    let worker = store.clone();
    let scan_root = root.to_path_buf();
    let refreshed = tokio::task::spawn_blocking(move || reconcile_blocking(&worker, &scan_root))
        .await
        .unwrap_or_else(|error| {
            tracing::warn!("code graph: tree reconciliation panicked: {error}");
            0
        });
    if refreshed > 0
        && let Err(error) = enrich(store)
    {
        tracing::warn!("code graph: enrichment after reconciliation failed: {error}");
    }
    let state = code_index_state(store);
    put_index_facts(
        store,
        &[
            ("reconciledAt", now_nanos().to_string()),
            ("reconciliations", (state.reconciliations + 1).to_string()),
            (
                "refreshedFiles",
                (state.refreshed_files + refreshed as u64).to_string(),
            ),
            ("fileCount", indexed_file_count(store).to_string()),
        ],
    );
    refreshed
}

/// The blocking half of [`reconcile_tree`]: stat the tree, parse only the diff.
fn reconcile_blocking(store: &OxigraphStore, root: &Path) -> usize {
    let on_disk = source_stamps(root);
    let recorded = all_recorded_stamps(store);
    let mut refreshed = 0;
    for (rel, stamp) in &on_disk {
        if recorded.get(rel) == Some(stamp) {
            continue;
        }
        clear_file(store, rel);
        if let Err(error) = index_file(store, &root.join(rel), rel) {
            tracing::warn!(path = %rel, "code graph: skipped file during reconciliation: {error}");
        }
        refreshed += 1;
    }
    for rel in recorded.keys() {
        if !on_disk.contains_key(rel) {
            clear_file(store, rel);
            refreshed += 1;
        }
    }
    refreshed
}

/// One query's result, plus what it depends on.
struct Answer {
    text: String,
    /// Working-directory-relative paths the answer cites, if any.
    paths: Vec<String>,
    /// Whether the query resolved anything at all. A `false` here is what a
    /// model reads as "it does not exist", so it is the claim that has to be
    /// earned.
    found: bool,
}

impl Answer {
    /// A formatted result, falling back to `negative` when it is empty.
    fn new(text: String, paths: Vec<String>, negative: &str) -> Self {
        if text.trim().is_empty() {
            Self {
                text: negative.to_string(),
                paths: Vec::new(),
                found: false,
            }
        } else {
            Self {
                text,
                paths,
                found: true,
            }
        }
    }

    /// A result whose own formatting already decided whether it found anything.
    fn stated(text: String, paths: Vec<String>, found: bool) -> Self {
        Self { text, paths, found }
    }
}

/// Answer `run` under the readiness gate and the freshness contract.
///
/// `subject` is what the caller asked about, used to make the not-ready text an
/// instruction about *this* question rather than a generic banner. `run` may be
/// invoked twice: once to see what the answer depends on, and once more after
/// the index has caught up on it.
async fn guarded<F>(
    store: &OxigraphStore,
    root: &Path,
    tool: &str,
    subject: Option<&str>,
    run: F,
) -> Result<String, ToolError>
where
    F: Fn() -> Result<Answer, ToolError>,
{
    let state = code_index_state(store);
    match state.phase {
        IndexPhase::Unstarted => return Ok(not_built(tool, subject)),
        IndexPhase::Building => return Ok(still_building(tool, subject, state.building_for)),
        IndexPhase::Ready | IndexPhase::Failed => {}
    }

    let mut notes = Vec::new();
    if let Some(reason) = &state.failure {
        notes.push(format!(
            "CODE INDEX INCOMPLETE — the last index pass failed ({reason}), so the graph is \
             missing an unknown number of symbols. Do not read anything below as proof that a \
             symbol is absent; confirm with `search` before acting on it."
        ));
    }

    let mut answer = run()?;
    if answer.found {
        let drifted = drifted_citations(store, root, &answer.paths);
        if !drifted.is_empty() {
            refresh_files(store, root, &drifted).await;
            answer = run()?;
            notes.push(format!(
                "(index note: {} file(s) this answer depends on had changed on disk since they \
                 were indexed; they were re-read before answering.)",
                drifted.len()
            ));
        }
    } else {
        // The dangerous answer. Reconcile with the tree before letting a model
        // conclude that something does not exist.
        let refreshed = reconcile_tree(store, root).await;
        if refreshed > 0 {
            answer = run()?;
        }
        if answer.found {
            notes.push(format!(
                "(index note: the working tree had moved on since it was indexed; \
                 {refreshed} file(s) were re-read before answering.)"
            ));
        } else {
            notes.push(
                "(index note: the index was reconciled with the working tree before answering, \
                 so this negative is about the tree as it is now.)"
                    .to_string(),
            );
        }
    }

    if notes.is_empty() {
        return Ok(answer.text);
    }
    notes.push(answer.text);
    Ok(truncate(notes.join("\n"), RESULT_BYTE_LIMIT))
}

/// How the question reads back when there is no index at all.
fn not_built(tool: &str, subject: Option<&str>) -> String {
    format!(
        "CODE INDEX NOT BUILT — this working directory has no code index, so `{tool}` cannot \
         answer{}. This is not a \"not found\" result and says nothing about whether it exists. \
         Use `search` or `read_file` instead; neither depends on the index.",
        about(subject)
    )
}

/// How the question reads back while the boot index is still running.
fn still_building(tool: &str, subject: Option<&str>, elapsed: Option<Duration>) -> String {
    let started = elapsed.map_or(String::new(), |d| format!(" (started {} ago)", ago(d)));
    format!(
        "CODE INDEX NOT READY — the code graph is still being built{started}, so `{tool}` cannot \
         answer{} yet. This is not a \"not found\" result: do not conclude the symbol is missing \
         and do not write a new definition for it. Either call `{tool}` again in a few seconds, \
         or use `search` / `read_file` now — neither depends on the index.",
        about(subject)
    )
}

fn about(subject: Option<&str>) -> String {
    subject.map_or(String::new(), |s| format!(" about '{s}'"))
}

// ─── Navigation toolset ──────────────────────────────────────────────────────

/// SPARQL-backed code navigation toolset over the Code `ABox`.
#[derive(Debug, Clone)]
pub struct CodeGraphTools {
    store: OxigraphStore,
    /// Sandbox config used by `read_symbol` to resolve a span to file bytes.
    config: CodeToolsConfig,
}

impl CodeGraphTools {
    /// Wrap a store handle in the toolset. `config` sandboxes the file reads
    /// `read_symbol` performs once it has resolved a symbol's span.
    #[must_use]
    pub fn new(store: OxigraphStore, config: CodeToolsConfig) -> Self {
        Self { store, config }
    }

    /// Run a SELECT and format the chosen variables, one solution per line.
    fn select(&self, query: &str, vars: &[&str]) -> Result<(String, Vec<String>), ToolError> {
        run_select(&self.store, query, vars)
    }

    /// Answer under the readiness gate and the freshness contract.
    async fn guarded<F>(
        &self,
        tool: &str,
        subject: Option<&str>,
        run: F,
    ) -> Result<String, ToolError>
    where
        F: Fn() -> Result<Answer, ToolError>,
    {
        guarded(&self.store, &self.config.working_dir, tool, subject, run).await
    }
}

/// Root-bound access to the already running index for lean model steps.
#[derive(Clone)]
pub(crate) struct CodeNavigation {
    store: OxigraphStore,
    root: std::path::PathBuf,
}

impl polaris::system::resource::GlobalResource for CodeNavigation {}

impl CodeNavigation {
    pub(crate) fn new(store: OxigraphStore, root: PathBuf) -> Self {
        Self { store, root }
    }

    pub(crate) fn available_for(&self, root: &Path) -> bool {
        self.root
            .canonicalize()
            .ok()
            .zip(root.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
            && code_index_state(&self.store).phase == IndexPhase::Ready
    }

    pub(crate) async fn query(&self, tool: &str, name: String) -> Result<String, String> {
        let tools = CodeGraphTools::new(self.store.clone(), CodeToolsConfig::new(&self.root));
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            match tool {
                "find_definition" => tools.find_definition(name).await,
                _ => tools.find_references(name).await,
            }
        })
        .await
        .map_err(|_| "Code navigation timed out; use search.".to_owned())?
        .map_err(|e| e.to_string())?;
        let mut text: String = result.chars().take(1400).collect();
        if result.chars().count() > 1400 {
            text.push_str("\n[clipped; use search to narrow]");
        }
        Ok(text)
    }
}

/// Run a SELECT against `store` and format the chosen variables tab-separated,
/// one solution per line, capped at [`RESULT_BYTE_LIMIT`]. Shared by the
/// navigation and concept toolsets.
///
/// Also returns every `?path` the solutions bound: the files the formatted
/// answer cites, which is what the freshness check is proportional to.
fn run_select(
    store: &OxigraphStore,
    query: &str,
    vars: &[&str],
) -> Result<(String, Vec<String>), ToolError> {
    let results = store
        .query(query)
        .map_err(|err| ToolError::execution_error(format!("query failed: {err}")))?;
    let QueryResults::Solutions(solutions) = results else {
        return Ok((String::new(), Vec::new()));
    };
    let mut lines = Vec::new();
    let mut paths = Vec::new();
    for solution in solutions {
        let solution =
            solution.map_err(|err| ToolError::execution_error(format!("row failed: {err}")))?;
        if let Some(path) = solution.get("path") {
            paths.push(term_str(path));
        }
        let row: Vec<String> = vars
            .iter()
            .map(|v| solution.get(*v).map(term_str).unwrap_or_default())
            .collect();
        lines.push(row.join("\t"));
    }
    Ok((truncate(lines.join("\n"), RESULT_BYTE_LIMIT), paths))
}

/// Re-derive the enrichment graph ([`ENRICHED_GRAPH_IRI`]) from the syntactic
/// Code `ABox`.
///
/// A separate reasoning pass — not baked into the syntax extractors — expressed as
/// SPARQL `INSERT … WHERE` rules that map syntactic facts to the conceptual
/// roles in [`ROLES`]:
///
/// - `#[tool]` (`hasAttr "tool"`) → `:ToolHandler`
/// - `#[system]` (`hasAttr "system"`) → `:SystemHandler`
/// - `impl Plugin for T` (`implementsTrait "Plugin"`) → `:Plugin`
/// - `impl Agent for T` (`implementsTrait "Agent"`) → `:Agent`
/// - `Res<X>` param (`paramRes "X"`) → `:reads "X"` (shared access)
/// - `ResMut<X>` param (`paramResMut "X"`) → `:writes "X"` (exclusive access)
///
/// Idempotent: clears and rebuilds the enrichment graph, so it can run after a
/// full or incremental index. The role IRIs live under [`NS`]; this layers on
/// top of the seed `TBox` without mutating either the indexer or the Code `ABox`.
///
/// # Errors
///
/// Returns an error if any underlying SPARQL update fails.
///
/// # Examples
///
/// ```no_run
/// use polaris_code_core::tools::enrich;
/// use polaris_ontrace::oxigraph::OxigraphStore;
///
/// let store = OxigraphStore::in_memory().unwrap();
/// // Once source has been indexed into the Code `ABox`, derive the
/// // conceptual-role layer the concept queries read from:
/// enrich(&store).unwrap();
/// ```
pub fn enrich(store: &OxigraphStore) -> anyhow::Result<()> {
    // SILENT: the enrichment graph may not exist yet on the first pass.
    store.update(&format!("CLEAR SILENT GRAPH <{ENRICHED_GRAPH_IRI}>"))?;
    insert_role_by_attr(store, "tool", "ToolHandler")?;
    insert_role_by_attr(store, "system", "SystemHandler")?;
    insert_role_by_trait(store, "Plugin", "Plugin")?;
    insert_role_by_trait(store, "Agent", "Agent")?;
    insert_access(store, "paramRes", "reads")?;
    insert_access(store, "paramResMut", "writes")?;
    Ok(())
}

/// Tag every symbol carrying attribute `attr` with conceptual `role`.
fn insert_role_by_attr(store: &OxigraphStore, attr: &str, role: &str) -> anyhow::Result<()> {
    let q = format!(
        "INSERT {{ GRAPH <{ENRICHED_GRAPH_IRI}> {{ ?s a <{NS}{role}> }} }} \
         WHERE  {{ GRAPH <{CODE_GRAPH_IRI}> {{ ?s <{NS}hasAttr> \"{attr}\" ; \
           <{NS}language> \"rust\" }} }}",
    );
    store.update(&q)?;
    Ok(())
}

/// Tag every impl of trait `trait_name` (and carry through its `onType`) with
/// conceptual `role`.
fn insert_role_by_trait(store: &OxigraphStore, trait_name: &str, role: &str) -> anyhow::Result<()> {
    let q = format!(
        "INSERT {{ GRAPH <{ENRICHED_GRAPH_IRI}> {{ ?s a <{NS}{role}> ; <{NS}onType> ?onType }} }} \
         WHERE  {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
           ?s <{NS}implementsTrait> \"{trait_name}\" ; <{NS}onType> ?onType ; \
             <{NS}language> \"rust\" }} }}",
    );
    store.update(&q)?;
    Ok(())
}

/// Map a syntactic resource-parameter fact (`syn_pred`, e.g. `paramRes`) to a
/// conceptual access edge (`access_pred`, e.g. `reads`) carrying the resource
/// type name through into the enrichment graph.
fn insert_access(store: &OxigraphStore, syn_pred: &str, access_pred: &str) -> anyhow::Result<()> {
    let q = format!(
        "INSERT {{ GRAPH <{ENRICHED_GRAPH_IRI}> {{ ?s <{NS}{access_pred}> ?x }} }} \
         WHERE  {{ GRAPH <{CODE_GRAPH_IRI}> {{ ?s <{NS}{syn_pred}> ?x ; \
           <{NS}language> \"rust\" }} }}",
    );
    store.update(&q)?;
    Ok(())
}

fn term_str(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => n.as_str().to_string(),
        Term::Literal(l) => l.value().to_string(),
        Term::BlankNode(b) => b.as_str().to_string(),
        other => other.to_string(),
    }
}

#[toolset]
impl CodeGraphTools {
    /// Locate where a symbol (function, struct, enum, trait, impl) is defined.
    ///
    /// Returns `path<TAB>line<TAB>kind` rows. Prefer this over text search for
    /// "where is X defined" — it resolves by symbol, not substring.
    #[tool]
    async fn find_definition(
        &self,
        /// The unqualified symbol name, e.g. `CodeIndexer` or `index_repo`.
        name: String,
    ) -> Result<String, ToolError> {
        let q = format!(
            "SELECT ?path ?line ?kind WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
             ?s <{NS}name> \"{name}\" ; a ?kind ; <{NS}definedIn> ?f ; <{NS}atLine> ?line . \
             ?f <{NS}path> ?path . }} }} ORDER BY ?path LIMIT 200",
            name = esc(&name),
        );
        let negative = format!("no definition found for '{name}'");
        self.guarded("find_definition", Some(&name), || {
            let (out, paths) = self.select(&q, &["path", "line", "kind"])?;
            Ok(Answer::new(out, paths, &negative))
        })
        .await
    }

    /// The table of contents of a file: `kind<TAB>name<TAB>start-end` per symbol,
    /// ordered by position.
    ///
    /// Read this ~30-line map instead of the whole file, then pull only the spans
    /// you need with `read_symbol` / `read_span`. The default for understanding a
    /// supported source file: outline first, read one symbol second.
    #[tool]
    async fn outline(
        &self,
        /// File path relative to the working directory, e.g. `src/main.rs`.
        path: String,
    ) -> Result<String, ToolError> {
        let q = format!(
            "SELECT ?kind ?name ?start ?end WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
             ?f <{NS}path> \"{path}\" . \
             ?s <{NS}definedIn> ?f ; a ?kind ; <{NS}name> ?name ; \
             <{NS}startLine> ?start ; <{NS}endLine> ?end . }} }} \
             ORDER BY (<http://www.w3.org/2001/XMLSchema#integer>(?start)) LIMIT 2000",
            path = esc(&path),
        );
        let negative = format!("no symbols indexed for '{path}'");
        // The file named in the request is what this answer depends on, found
        // or not: an outline of a file that has just been rewritten is exactly
        // as misleading as an outline of one that was never indexed.
        let cited = vec![path.clone()];
        self.guarded("outline", Some(&path), || {
            let results = self
                .store
                .query(&q)
                .map_err(|err| ToolError::execution_error(format!("query failed: {err}")))?;
            let QueryResults::Solutions(solutions) = results else {
                return Ok(Answer::new(String::new(), cited.clone(), &negative));
            };
            let mut lines = Vec::new();
            for solution in solutions {
                let solution = solution
                    .map_err(|err| ToolError::execution_error(format!("row failed: {err}")))?;
                let kind = solution.get("kind").map(term_str).unwrap_or_default();
                let name = solution.get("name").map(term_str).unwrap_or_default();
                let start = solution.get("start").map(term_str).unwrap_or_default();
                let end = solution.get("end").map(term_str).unwrap_or_default();
                lines.push(format!("{}\t{name}\t{start}-{end}", short_class(&kind)));
            }
            Ok(Answer::new(
                truncate(lines.join("\n"), RESULT_BYTE_LIMIT),
                cited.clone(),
                &negative,
            ))
        })
        .await
    }

    /// Read a symbol's definition source by name, resolving its span via the
    /// graph, which costs far fewer tokens than reading the whole file.
    ///
    /// Returns the definition (including attributes and doc comments) with line
    /// numbers. If the name is ambiguous, lists the candidate spans so you can
    /// pick one with `read_span`.
    #[tool]
    async fn read_symbol(
        &self,
        /// The unqualified symbol name, e.g. `CodeIndexer` or `index_repo`.
        name: String,
    ) -> Result<String, ToolError> {
        let q = format!(
            "SELECT ?path ?start ?end ?kind WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
             ?s <{NS}name> \"{name}\" ; a ?kind ; <{NS}definedIn> ?f ; \
             <{NS}startLine> ?start ; <{NS}endLine> ?end . \
             ?f <{NS}path> ?path . }} }} ORDER BY ?path LIMIT 200",
            name = esc(&name),
        );
        // The span resolution and the read of live bytes are both inside the
        // closure, so a re-run after a refresh re-resolves before it re-reads:
        // `read_symbol` can never hand back a span the index no longer holds.
        self.guarded("read_symbol", Some(&name), || {
            let results = self
                .store
                .query(&q)
                .map_err(|err| ToolError::execution_error(format!("query failed: {err}")))?;
            let QueryResults::Solutions(solutions) = results else {
                return Ok(Answer::stated(
                    format!("no symbol named '{name}'"),
                    Vec::new(),
                    false,
                ));
            };
            let mut rows = Vec::new();
            for solution in solutions {
                let solution = solution
                    .map_err(|err| ToolError::execution_error(format!("row failed: {err}")))?;
                let path = solution.get("path").map(term_str).unwrap_or_default();
                let start = solution
                    .get("start")
                    .map(term_str)
                    .and_then(|s| s.parse::<usize>().ok());
                let end = solution
                    .get("end")
                    .map(term_str)
                    .and_then(|s| s.parse::<usize>().ok());
                let kind = solution.get("kind").map(term_str).unwrap_or_default();
                if let (Some(start), Some(end)) = (start, end) {
                    rows.push((path, start, end, short_class(&kind).to_string()));
                }
            }
            let cited: Vec<String> = rows.iter().map(|(path, ..)| path.clone()).collect();

            match rows.as_slice() {
                [] => Ok(Answer::stated(
                    format!("no symbol named '{name}'; try find_definition or read_file"),
                    Vec::new(),
                    false,
                )),
                [(path, start, end, _)] => Ok(Answer::stated(
                    self.read_span_source(path, *start, *end)?,
                    cited,
                    true,
                )),
                many => {
                    let mut out = format!(
                        "'{name}' is ambiguous ({} definitions); read_span the one you want:\n",
                        many.len()
                    );
                    for (path, start, end, kind) in many {
                        out.push_str(&format!("{kind}\t{path}\t{start}-{end}\n"));
                    }
                    Ok(Answer::stated(out, cited, true))
                }
            }
        })
        .await
    }

    /// Find functions that reference (call) a given function.
    ///
    /// Returns `path<TAB>line<TAB>caller` rows — the call sites in the call graph.
    #[tool]
    async fn find_references(
        &self,
        /// The function name being referenced.
        name: String,
    ) -> Result<String, ToolError> {
        let negative = format!("no references found for '{name}'");
        self.guarded("find_references", Some(&name), || {
            let (out, paths) = self.callers_query(&name)?;
            Ok(Answer::new(out, paths, &negative))
        })
        .await
    }

    /// List the functions that call `name` (its callers).
    #[tool]
    async fn callers_of(
        &self,
        /// The callee function name.
        name: String,
    ) -> Result<String, ToolError> {
        let negative = format!("no callers found for '{name}'");
        self.guarded("callers_of", Some(&name), || {
            let (out, paths) = self.callers_query(&name)?;
            Ok(Answer::new(out, paths, &negative))
        })
        .await
    }

    /// List the functions called by `name` (its callees).
    #[tool]
    async fn callees_of(
        &self,
        /// The caller function name.
        name: String,
    ) -> Result<String, ToolError> {
        let q = format!(
            "SELECT DISTINCT ?callee WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
             ?c <{NS}name> \"{name}\" ; <{NS}callsFn> ?callee . }} }} ORDER BY ?callee LIMIT 200",
            name = esc(&name),
        );
        let negative = format!("no callees found for '{name}'");
        // Callee names cite no file, so this answer carries nothing to
        // stat-check; a negative still reconciles the tree before it stands.
        self.guarded("callees_of", Some(&name), || {
            let (out, _) = self.select(&q, &["callee"])?;
            Ok(Answer::new(out, Vec::new(), &negative))
        })
        .await
    }

    /// List the types that implement a given trait.
    ///
    /// Returns `path<TAB>line<TAB>type` rows.
    #[tool]
    async fn impls_of_trait(
        &self,
        /// The trait name, e.g. `Plugin` or `Agent`.
        name: String,
    ) -> Result<String, ToolError> {
        let q = format!(
            "SELECT ?path ?line ?onType WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
             ?s <{NS}implementsTrait> \"{name}\" ; <{NS}onType> ?onType ; \
             <{NS}definedIn> ?f ; <{NS}atLine> ?line . \
             ?f <{NS}path> ?path . }} }} ORDER BY ?path LIMIT 200",
            name = esc(&name),
        );
        let negative = format!("no impls found for trait '{name}'");
        self.guarded("impls_of_trait", Some(&name), || {
            let (out, paths) = self.select(&q, &["path", "line", "onType"])?;
            Ok(Answer::new(out, paths, &negative))
        })
        .await
    }
}

impl CodeGraphTools {
    /// Read `[start, end]` of `path` (sandboxed), returned with line numbers.
    fn read_span_source(&self, path: &str, start: usize, end: usize) -> Result<String, ToolError> {
        let resolved = self
            .config
            .resolve_path(path)
            .ok_or_else(|| ToolError::execution_error(format!("path '{path}' escapes sandbox")))?;
        let content = std::fs::read_to_string(&resolved).map_err(|err| {
            ToolError::execution_error(format!("read_symbol '{path}' failed: {err}"))
        })?;
        let header = format!("{path}:{start}-{end}\n");
        let view = line_view(
            &content,
            start,
            Some(end.saturating_sub(start) + 1),
            RESULT_BYTE_LIMIT.saturating_sub(header.len()),
        );
        // A symbol body cut by the cap gets the same resumption notice a cut
        // `read_file` gets, pointing at the same `offset` mechanism.
        Ok(match cap_footer(path, &view) {
            Some(footer) => format!("{header}{}{footer}", view.body),
            None => format!("{header}{}", view.body),
        })
    }

    fn callers_query(&self, name: &str) -> Result<(String, Vec<String>), ToolError> {
        let q = format!(
            "SELECT ?path ?line ?caller WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ \
             ?c <{NS}callsFn> \"{name}\" ; <{NS}name> ?caller ; \
             <{NS}definedIn> ?f ; <{NS}atLine> ?line . \
             ?f <{NS}path> ?path . }} }} ORDER BY ?path LIMIT 200",
            name = esc(name),
        );
        self.select(&q, &["path", "line", "caller"])
    }
}

/// Shorten a class IRI (`urn:polaris:code:Function`) to its local name
/// (`Function`) for legible outline rows.
fn short_class(iri: &str) -> &str {
    iri.rsplit(['/', ':', '#']).next().unwrap_or(iri)
}

fn truncate(mut s: String, limit: usize) -> String {
    if s.len() <= limit {
        return s;
    }
    let mut cut = limit;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("\n… (truncated)");
    s
}

// ─── Curated concept queries ─────────────────────────────────────────────────

/// Which graph-search surface the agent is offered, the config gate for
/// comparing the two retrieval styles.
///
/// - [`Curated`](Self::Curated): only the parameterized [`ConceptTools`]; the
///   freeform `sparql_query` should be withheld by the host.
/// - [`Freeform`](Self::Freeform): only freeform SPARQL (ontrace's
///   `sparql_query`); the concept tools are not registered.
/// - [`Both`](Self::Both): expose both (default; the prompt nudges freeform).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphSearchMode {
    /// Curated parameterized concept queries only.
    Curated,
    /// Freeform SPARQL only.
    Freeform,
    /// Both surfaces exposed.
    #[default]
    Both,
}

impl GraphSearchMode {
    /// Whether the curated [`ConceptTools`] should be registered.
    #[must_use]
    pub fn curated_enabled(self) -> bool {
        matches!(self, Self::Curated | Self::Both)
    }

    /// Whether freeform SPARQL should remain available to the agent.
    #[must_use]
    pub fn freeform_enabled(self) -> bool {
        matches!(self, Self::Freeform | Self::Both)
    }

    /// Parse from a config string (`curated` / `freeform` / `both`),
    /// case-insensitively. Unknown values fall back to the default.
    #[must_use]
    pub fn from_str_or_default(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "curated" => Self::Curated,
            "freeform" => Self::Freeform,
            _ => Self::Both,
        }
    }
}

/// Curated, parameterized concept-query toolset over the enrichment graph — the
/// safe, legible "query the codebase by role" path.
#[derive(Debug, Clone)]
pub struct ConceptTools {
    store: OxigraphStore,
    /// Sandbox config: its `working_dir` is the tree these answers are checked
    /// against, exactly as for [`CodeGraphTools`].
    config: CodeToolsConfig,
}

impl ConceptTools {
    /// Wrap a store handle in the toolset. `config` names the tree the
    /// readiness gate and the freshness check are about.
    #[must_use]
    pub fn new(store: OxigraphStore, config: CodeToolsConfig) -> Self {
        Self { store, config }
    }

    /// Answer under the readiness gate and the freshness contract.
    async fn guarded<F>(
        &self,
        tool: &str,
        subject: Option<&str>,
        run: F,
    ) -> Result<String, ToolError>
    where
        F: Fn() -> Result<Answer, ToolError>,
    {
        guarded(&self.store, &self.config.working_dir, tool, subject, run).await
    }
}

#[toolset]
impl ConceptTools {
    /// List the conceptual roles the code graph understands and how many
    /// symbols hold each — the vocabulary for `find_by_role`.
    ///
    /// Roles are Rust/Polaris concepts derived only from Rust syntactic facts
    /// (e.g. `#[tool]` → `ToolHandler`, `impl Plugin` → `Plugin`), so this
    /// answers "what enforces tool permissions" or "what are the Polaris
    /// plugins/agents" without guessing equivalent roles in other languages.
    #[tool]
    async fn concepts(&self) -> Result<String, ToolError> {
        // An all-zero table is the shape of both "this codebase holds no
        // Polaris roles" and "nothing has been indexed yet", so it counts as a
        // negative answer and goes through the same gate.
        self.guarded("concepts", None, || {
            let mut lines = vec!["role\tcount".to_string()];
            let mut any = false;
            for role in ROLES {
                let q = format!(
                    "SELECT (COUNT(?s) AS ?n) WHERE {{ GRAPH <{ENRICHED_GRAPH_IRI}> {{ \
                     ?s a <{NS}{role}> }} }}",
                );
                let (n, _) = run_select(&self.store, &q, &["n"])?;
                let n = n.trim();
                any |= n.parse::<u64>().is_ok_and(|n| n > 0);
                lines.push(format!("{role}\t{n}"));
            }
            Ok(Answer::stated(lines.join("\n"), Vec::new(), any))
        })
        .await
    }

    /// List the symbols holding a conceptual `role`, as `path<TAB>start-end<TAB>name`.
    ///
    /// `role` must be one of the values reported by `concepts` (e.g.
    /// `ToolHandler`, `SystemHandler`, `Plugin`, `Agent`). Resolve a span here,
    /// then `read_symbol` / `read_span` only what you need.
    #[tool]
    async fn find_by_role(
        &self,
        /// A conceptual role from `concepts`, e.g. `Plugin` or `ToolHandler`.
        role: String,
    ) -> Result<String, ToolError> {
        if !ROLES.contains(&role.as_str()) {
            return Err(ToolError::parameter_error(format!(
                "unknown role '{role}'; known roles: {}",
                ROLES.join(", ")
            )));
        }
        // `role` is validated against the static allow-list, so it is safe to
        // interpolate directly into the role IRI.
        let q = format!(
            "SELECT ?path ?start ?end ?name WHERE {{ \
             GRAPH <{ENRICHED_GRAPH_IRI}> {{ ?s a <{NS}{role}> . }} \
             GRAPH <{CODE_GRAPH_IRI}> {{ ?s <{NS}name> ?name ; <{NS}definedIn> ?f ; \
             <{NS}startLine> ?start ; <{NS}endLine> ?end . ?f <{NS}path> ?path . }} }} \
             ORDER BY ?path LIMIT 200",
        );
        let negative = format!("no symbols with role '{role}'");
        self.guarded("find_by_role", Some(&role), || {
            let (out, paths) = run_select(&self.store, &q, &["path", "start", "end", "name"])?;
            Ok(Answer::new(out, paths, &negative))
        })
        .await
    }

    /// List the resources systems read or write, as `resource<TAB>readers<TAB>writers`.
    ///
    /// Derived from system-handler `Res<X>` / `ResMut<X>` parameters: a high
    /// writer count flags central mutable state (e.g. the conversation context),
    /// a high reader count flags shared config/registries. The vocabulary for
    /// `accessors_of` — answers "what writes X", "what reads X" without source.
    #[tool]
    async fn resources(&self) -> Result<String, ToolError> {
        self.guarded("resources", None, || {
            let readers = self.count_accessors("reads")?;
            let writers = self.count_accessors("writes")?;
            let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            names.extend(readers.keys().cloned());
            names.extend(writers.keys().cloned());
            if names.is_empty() {
                return Ok(Answer::stated(
                    "no resource access recorded".to_string(),
                    Vec::new(),
                    false,
                ));
            }
            let mut lines = vec!["resource\treaders\twriters".to_string()];
            for name in names {
                lines.push(format!(
                    "{name}\t{}\t{}",
                    readers.get(&name).copied().unwrap_or(0),
                    writers.get(&name).copied().unwrap_or(0),
                ));
            }
            Ok(Answer::stated(
                truncate(lines.join("\n"), RESULT_BYTE_LIMIT),
                Vec::new(),
                true,
            ))
        })
        .await
    }

    /// List the systems that read or write a given resource, as
    /// `access<TAB>path<TAB>start-end<TAB>name` (`access` is `reads` or `writes`).
    ///
    /// `resource` is a type name from `resources` (e.g. `ContextManager`).
    /// Resolve a span here, then `read_symbol` / `read_span` only what you need.
    #[tool]
    async fn accessors_of(
        &self,
        /// A resource type name from `resources`, e.g. `ContextManager`.
        resource: String,
    ) -> Result<String, ToolError> {
        let negative = format!("no systems read or write '{resource}'");
        self.guarded("accessors_of", Some(&resource), || {
            let (mut lines, mut paths) = self.access_rows("reads", &resource)?;
            let (writes, write_paths) = self.access_rows("writes", &resource)?;
            lines.extend(writes);
            paths.extend(write_paths);
            if lines.is_empty() {
                return Ok(Answer::stated(negative.clone(), Vec::new(), false));
            }
            Ok(Answer::stated(
                truncate(lines.join("\n"), RESULT_BYTE_LIMIT),
                paths,
                true,
            ))
        })
        .await
    }
}

impl ConceptTools {
    /// Count, per resource type, the systems holding `access_pred` access
    /// (`reads` / `writes`) over the enrichment graph.
    fn count_accessors(
        &self,
        access_pred: &str,
    ) -> Result<std::collections::BTreeMap<String, usize>, ToolError> {
        let q = format!(
            "SELECT ?x (COUNT(DISTINCT ?s) AS ?n) WHERE {{ GRAPH <{ENRICHED_GRAPH_IRI}> {{ \
             ?s <{NS}{access_pred}> ?x }} }} GROUP BY ?x",
        );
        let results = self
            .store
            .query(&q)
            .map_err(|err| ToolError::execution_error(format!("query failed: {err}")))?;
        let QueryResults::Solutions(solutions) = results else {
            return Ok(std::collections::BTreeMap::new());
        };
        let mut counts = std::collections::BTreeMap::new();
        for solution in solutions {
            let solution =
                solution.map_err(|err| ToolError::execution_error(format!("row failed: {err}")))?;
            let x = solution.get("x").map(term_str).unwrap_or_default();
            let n = solution
                .get("n")
                .map(term_str)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            if !x.is_empty() {
                counts.insert(x, n);
            }
        }
        Ok(counts)
    }

    /// Rows of systems with `access_pred` (`reads` / `writes`) access to
    /// `resource`, each formatted `access<TAB>path<TAB>start-end<TAB>name`,
    /// plus the files those rows cite.
    fn access_rows(
        &self,
        access_pred: &str,
        resource: &str,
    ) -> Result<(Vec<String>, Vec<String>), ToolError> {
        let q = format!(
            "SELECT ?path ?start ?end ?name WHERE {{ \
             GRAPH <{ENRICHED_GRAPH_IRI}> {{ ?s <{NS}{access_pred}> \"{res}\" . }} \
             GRAPH <{CODE_GRAPH_IRI}> {{ ?s <{NS}name> ?name ; <{NS}definedIn> ?f ; \
             <{NS}startLine> ?start ; <{NS}endLine> ?end . ?f <{NS}path> ?path . }} }} \
             ORDER BY ?path LIMIT 200",
            res = esc(resource),
        );
        let results = self
            .store
            .query(&q)
            .map_err(|err| ToolError::execution_error(format!("query failed: {err}")))?;
        let QueryResults::Solutions(solutions) = results else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut rows = Vec::new();
        let mut paths = Vec::new();
        for solution in solutions {
            let solution =
                solution.map_err(|err| ToolError::execution_error(format!("row failed: {err}")))?;
            let path = solution.get("path").map(term_str).unwrap_or_default();
            let start = solution.get("start").map(term_str).unwrap_or_default();
            let end = solution.get("end").map(term_str).unwrap_or_default();
            let name = solution.get("name").map(term_str).unwrap_or_default();
            rows.push(format!("{access_pred}\t{path}\t{start}-{end}\t{name}"));
            paths.push(path);
        }
        Ok((rows, paths))
    }
}

/// Plugin that registers [`CodeGraphTools`] and kicks off background indexing.
/// Add it when an agent needs zero-setup structural navigation over Rust,
/// Python, JavaScript, TypeScript, and TSX repositories.
///
/// Depends on [`ToolsPlugin`] (for the registry) and [`OxigraphPlugin`] (for the
/// [`OxigraphStore`] global). On [`Plugin::ready`] it spawns a background task
/// that indexes `working_dir` into the Code `ABox`, so the agent can begin a turn
/// even while indexing is still in flight.
///
/// The curated [`ConceptTools`] (role/resource queries) are registered only when
/// the configured [`GraphSearchMode`] enables them — see
/// [`with_search_mode`](Self::with_search_mode); the always-on [`CodeGraphTools`]
/// cover symbol/def/ref navigation regardless.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | (none)   | —     | Registers tools on the existing `ToolRegistry`; provides no new resource. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | (none) | Uses the existing store and tool registry without publishing a new API. |
///
/// # Tools Provided
///
/// | Tool | Permission | Description |
/// |------|------------|-------------|
/// | `find_definition`, `outline`, `read_symbol` | Allow | Resolve indexed symbols and their source spans. |
/// | `find_references`, `callers_of`, `callees_of` | Allow | Query best-effort syntactic call relationships. |
/// | `impls_of_trait` | Allow | Query explicit Rust trait or TypeScript interface implementations. |
/// | `concepts`, `find_by_role`, `resources`, `accessors_of` | Allow | Query Rust/Polaris-only derived concepts when curated search is enabled. |
///
/// # Permissions
///
/// Every tool is read-only. Source reads performed by `read_symbol` remain
/// sandboxed to the configured working directory.
///
/// # Dependencies
///
/// - [`ToolsPlugin`] — the [`ToolRegistry`] this plugin populates.
/// - [`OxigraphPlugin`] — the [`OxigraphStore`] global this plugin reads/writes.
///
/// # Lifecycle
///
/// `ready` starts best-effort background indexing, then derives the Rust-only
/// concept graph. Unsupported, malformed, or individually failing files are
/// skipped and logged without delaying agent startup.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_code_core::CodeGraphPlugin;
/// use polaris_ontrace::oxigraph::OxigraphPlugin;
///
/// let mut server = Server::new();
/// server
///     .add_plugins(ToolsPlugin)
///     .add_plugins(OxigraphPlugin::in_memory())
///     .add_plugins(CodeGraphPlugin::new("."));
/// ```
pub struct CodeGraphPlugin {
    working_dir: std::path::PathBuf,
    search_mode: GraphSearchMode,
}

impl CodeGraphPlugin {
    /// Create a plugin that indexes (and serves navigation over) `working_dir`.
    #[must_use]
    pub fn new(working_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            search_mode: GraphSearchMode::default(),
        }
    }

    /// Set the graph-search surface (config gate). Controls whether
    /// the curated [`ConceptTools`] are registered; the host gates freeform
    /// SPARQL separately (see [`GraphSearchMode::freeform_enabled`]).
    #[must_use]
    pub fn with_search_mode(mut self, mode: GraphSearchMode) -> Self {
        self.search_mode = mode;
        self
    }
}

impl Plugin for CodeGraphPlugin {
    const ID: &'static str = "polaris_code::code_graph";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<ToolsPlugin>(),
            PluginId::of::<OxigraphPlugin>(),
        ]
    }

    fn build(&self, server: &mut Server) {
        let store = {
            let g = server
                .get_global::<OxigraphStore>()
                .expect("OxigraphPlugin must be added before CodeGraphPlugin");
            OxigraphStore::clone(&g)
        };
        server.insert_global(CodeNavigation::new(store.clone(), self.working_dir.clone()));
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before CodeGraphPlugin");
        // `ready` spawns the boot index, but the tools are callable the moment
        // they are registered. Announce the pending index here, synchronously,
        // so a query that arrives in between is told the index is on its way
        // rather than that there is no index at all.
        put_index_facts(
            &store,
            &[
                ("phase", "building".to_string()),
                ("startedAt", now_nanos().to_string()),
            ],
        );
        let config = CodeToolsConfig::new(self.working_dir.clone());
        registry.register_toolset(CodeGraphTools::new(store.clone(), config.clone()));
        // The curated concept queries are the config-gated half of the search gate;
        // freeform SPARQL (ontrace's `sparql_query`) is gated by the host.
        if self.search_mode.curated_enabled() {
            registry.register_toolset(ConceptTools::new(store, config));
        }
    }

    async fn ready(&self, server: &mut Server) {
        let store = {
            let g = server
                .get_global::<OxigraphStore>()
                .expect("OxigraphStore global present after build");
            OxigraphStore::clone(&g)
        };
        let working_dir = self.working_dir.clone();
        tokio::spawn(async move {
            match CodeIndexer::index_repo(&store, &working_dir).await {
                Ok(n) => tracing::info!(
                    "code graph: indexed {n} facts from {}",
                    working_dir.display()
                ),
                Err(err) => tracing::warn!("code graph: indexing failed: {err}"),
            }
            // `index_repo` derives the conceptual-role enrichment graph itself,
            // before it announces readiness.
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Index `src` as `lib.rs` and return the tools plus the tempdir they read
    /// from (kept alive so sandboxed file reads in `read_symbol` succeed).
    async fn indexed_tools(src: &str) -> (CodeGraphTools, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), src).unwrap();
        let store = OxigraphStore::in_memory().unwrap();
        CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
        let tools = CodeGraphTools::new(store, CodeToolsConfig::new(dir.path()));
        (tools, dir)
    }

    async fn indexed_fixture(
        files: &[(&str, &str)],
    ) -> (
        CodeGraphTools,
        ConceptTools,
        OxigraphStore,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        for (path, src) in files {
            let path = dir.path().join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, src).unwrap();
        }
        let store = OxigraphStore::in_memory().unwrap();
        CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
        enrich(&store).unwrap();
        (
            CodeGraphTools::new(store.clone(), CodeToolsConfig::new(dir.path())),
            ConceptTools::new(store.clone(), CodeToolsConfig::new(dir.path())),
            store,
            dir,
        )
    }

    fn select_values(store: &OxigraphStore, query: &str, variable: &str) -> Vec<String> {
        let QueryResults::Solutions(rows) = store.query(query).unwrap() else {
            panic!("expected SELECT results");
        };
        rows.map(|row| {
            let row = row.unwrap();
            row.get(variable).map(term_str).unwrap_or_default()
        })
        .collect()
    }

    #[tokio::test]
    async fn lean_navigation_is_root_bound_short_and_refreshes_edits() {
        let (_, _, store, dir) = indexed_fixture(&[(
            "widget.py",
            "def helper():\n    return 1\ndef caller():\n    return helper()\n",
        )])
        .await;
        let navigation = CodeNavigation {
            store,
            root: dir.path().to_owned(),
        };
        assert!(navigation.available_for(dir.path()));
        let other = tempfile::tempdir().unwrap();
        assert!(!navigation.available_for(other.path()));
        for tool in ["find_definition", "find_references"] {
            let result = navigation.query(tool, "helper".into()).await.unwrap();
            assert!(result.contains("widget.py"), "{result}");
            assert!(result.chars().count() <= 1500);
        }
        std::fs::write(
            dir.path().join("widget.py"),
            "\n\ndef helper():\n    return 200\n",
        )
        .unwrap();
        let result = navigation
            .query("find_definition", "helper".into())
            .await
            .unwrap();
        assert!(result.contains("widget.py\t3"), "{result}");
        let empty = CodeNavigation {
            store: OxigraphStore::in_memory().unwrap(),
            root: dir.path().to_owned(),
        };
        assert!(!empty.available_for(dir.path()));
    }

    #[tokio::test]
    async fn rust_facts_are_the_base_set_plus_one_language_fact_per_node() {
        let (tools, _, store, _dir) = indexed_fixture(&[(
            "lib.rs",
            "fn helper() -> u32 { 1 }\nfn caller() -> u32 { helper() }\n",
        )])
        .await;
        let facts = select_values(
            &store,
            &format!("SELECT ?p WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ ?s ?p ?o }} }}"),
            "p",
        );
        assert_eq!(
            facts.len(),
            20,
            "base 15 facts, three language facts, and the file's freshness stamp"
        );
        assert_eq!(
            facts
                .iter()
                .filter(|predicate| {
                    **predicate == format!("{NS}sourceMtime")
                        || **predicate == format!("{NS}sourceSize")
                })
                .count(),
            2,
            "every indexed file carries the stamp the freshness check compares against"
        );
        assert_eq!(
            facts
                .iter()
                .filter(|predicate| predicate.as_str() == format!("{NS}language"))
                .count(),
            3
        );
        assert_eq!(
            tools.find_definition("helper".into()).await.unwrap(),
            "lib.rs\t1\turn:polaris:code:Function"
        );
        let outline = tools.outline("lib.rs".into()).await.unwrap();
        assert!(!outline.contains("Class"));
        assert!(!outline.contains("Interface"));
        assert!(!outline.contains("TypeAlias"));
    }

    #[tokio::test]
    async fn malformed_rust_keeps_the_base_all_or_nothing_behavior() {
        let (tools, _, store, _dir) = indexed_fixture(&[("broken.rs", "fn broken(")]).await;
        assert!(
            tools
                .outline("broken.rs".into())
                .await
                .unwrap()
                .contains("no symbols indexed")
        );
        let facts = select_values(
            &store,
            &format!("SELECT ?p WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ ?s ?p ?o }} }}"),
            "p",
        );
        assert!(facts.is_empty());
    }

    #[test]
    fn extension_dispatch_matches_the_approved_closed_set() {
        assert_eq!(
            SourceLanguage::for_path(Path::new("x.rs")),
            Some(SourceLanguage::Rust)
        );
        for path in ["x.py", "x.pyi"] {
            assert_eq!(
                SourceLanguage::for_path(Path::new(path)),
                Some(SourceLanguage::Python)
            );
        }
        for path in ["x.js", "x.jsx", "x.mjs", "x.cjs"] {
            assert_eq!(
                SourceLanguage::for_path(Path::new(path)),
                Some(SourceLanguage::JavaScript)
            );
        }
        for path in ["x.ts", "x.mts", "x.cts", "x.d.ts"] {
            assert_eq!(
                SourceLanguage::for_path(Path::new(path)),
                Some(SourceLanguage::TypeScript)
            );
        }
        assert_eq!(
            SourceLanguage::for_path(Path::new("x.tsx")),
            Some(SourceLanguage::Tsx)
        );
        for path in ["x.go", "x.java", "x.min.js", "x.min.mjs", "x.min.cjs"] {
            assert_eq!(SourceLanguage::for_path(Path::new(path)), None);
        }
    }

    #[test]
    fn graph_tool_names_remain_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let store = OxigraphStore::in_memory().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register_toolset(CodeGraphTools::new(
            store.clone(),
            CodeToolsConfig::new(dir.path()),
        ));
        registry.register_toolset(ConceptTools::new(store, CodeToolsConfig::new(dir.path())));
        let names: std::collections::BTreeSet<String> = registry
            .definitions()
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        assert_eq!(
            names,
            [
                "accessors_of",
                "callees_of",
                "callers_of",
                "concepts",
                "find_by_role",
                "find_definition",
                "find_references",
                "impls_of_trait",
                "outline",
                "read_symbol",
                "resources",
            ]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
        );
    }

    #[tokio::test]
    async fn python_navigation_includes_decorated_spans_and_calls() {
        let (tools, concepts, _, _dir) = indexed_fixture(&[(
            "service.py",
            "@tool\ndef helper():\n    return 1\n\nclass Plugin:\n    pass\n\nclass Service:\n    def run(self):\n        return helper()\n",
        )])
        .await;

        assert_eq!(
            tools.find_definition("helper".into()).await.unwrap(),
            "service.py\t2\turn:polaris:code:Function"
        );
        let outline = tools.outline("service.py".into()).await.unwrap();
        assert!(outline.contains("Function\thelper\t1-3"), "{outline}");
        assert!(outline.contains("Class\tPlugin\t5-6"), "{outline}");
        assert!(outline.contains("Class\tService\t8-10"), "{outline}");
        assert!(outline.contains("Function\trun\t9-10"), "{outline}");
        assert!(
            tools
                .callers_of("helper".into())
                .await
                .unwrap()
                .contains("service.py\t9\trun")
        );
        assert!(
            tools
                .callees_of("run".into())
                .await
                .unwrap()
                .contains("helper")
        );
        assert!(
            concepts
                .find_by_role("ToolHandler".into())
                .await
                .unwrap()
                .contains("no symbols")
        );
        assert!(concepts.concepts().await.unwrap().contains("Plugin\t0"));
    }

    #[tokio::test]
    async fn javascript_indexes_classes_arrows_methods_and_new_calls() {
        let (tools, _, _, _dir) = indexed_fixture(&[(
            "app.jsx",
            "class Widget {\n  run() { return helper(); }\n  make = () => new Widget();\n}\nconst helper = () => 1;\n",
        )])
        .await;

        let outline = tools.outline("app.jsx".into()).await.unwrap();
        for expected in [
            "Class\tWidget\t1-4",
            "Function\trun\t2-2",
            "Function\tmake\t3-3",
            "Function\thelper\t5-5",
        ] {
            assert!(outline.contains(expected), "missing {expected}: {outline}");
        }
        assert_eq!(tools.callees_of("run".into()).await.unwrap(), "helper");
        assert_eq!(tools.callees_of("make".into()).await.unwrap(), "Widget");
    }

    #[tokio::test]
    async fn typescript_implements_annotates_one_class_without_minting_roles() {
        let (tools, concepts, _, _dir) = indexed_fixture(&[(
            "widget.ts",
            "export interface Plugin<T> { id(): T; }\nexport class Widget implements Plugin<string> {\n  id(): string { return helper(); }\n}\ntype Label = string;\nenum Mode { A }\nnamespace Space { export function helper() { return 'x'; } }\n",
        )])
        .await;

        assert_eq!(
            tools.find_definition("Widget".into()).await.unwrap(),
            "widget.ts\t2\turn:polaris:code:Class"
        );
        assert_eq!(
            tools.impls_of_trait("Plugin".into()).await.unwrap(),
            "widget.ts\t2\tWidget"
        );
        let outline = tools.outline("widget.ts".into()).await.unwrap();
        for expected in [
            "Interface\tPlugin\t1-1",
            "Class\tWidget\t2-4",
            "TypeAlias\tLabel\t5-5",
            "Enum\tMode\t6-6",
            "Module\tSpace\t7-7",
        ] {
            assert!(outline.contains(expected), "missing {expected}: {outline}");
        }
        assert!(concepts.concepts().await.unwrap().contains("Plugin\t0"));
        assert!(
            concepts
                .find_by_role("Plugin".into())
                .await
                .unwrap()
                .contains("no symbols")
        );
    }

    #[tokio::test]
    async fn tsx_and_mixed_language_facts_share_one_graph() {
        let (tools, _, store, _dir) = indexed_fixture(&[
            ("lib.rs", "fn rust_fn() {}\n"),
            ("worker.py", "def python_fn():\n    return 1\n"),
            (
                "view.tsx",
                "export const View = () => <button onClick={() => helper()}>ok</button>;\nfunction helper() {}\n",
            ),
        ])
        .await;

        for name in ["rust_fn", "python_fn", "View", "helper"] {
            assert!(
                !tools
                    .find_definition(name.into())
                    .await
                    .unwrap()
                    .contains("no definition"),
                "missing {name}"
            );
        }
        let languages = select_values(
            &store,
            &format!(
                "SELECT DISTINCT ?language WHERE {{ GRAPH <{CODE_GRAPH_IRI}> {{ ?s <{NS}language> ?language }} }} ORDER BY ?language"
            ),
            "language",
        );
        assert_eq!(languages, ["python", "rust", "typescript"]);
    }

    #[tokio::test]
    async fn incremental_python_and_typescript_edits_replace_stale_facts() {
        let (tools, _, store, dir) = indexed_fixture(&[
            ("edit.py", "def old_python():\n    return 1\n"),
            ("edit.ts", "export function old_ts() { return 1; }\n"),
        ])
        .await;
        std::fs::write(
            dir.path().join("edit.py"),
            "def new_python():\n    return 2\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("edit.ts"),
            "export function new_ts() { return 2; }\n",
        )
        .unwrap();
        CodeIndexer::index_paths(
            &store,
            dir.path(),
            &[dir.path().join("edit.py"), dir.path().join("edit.ts")],
        )
        .await
        .unwrap();
        enrich(&store).unwrap();

        for old in ["old_python", "old_ts"] {
            assert!(
                tools
                    .find_definition(old.into())
                    .await
                    .unwrap()
                    .contains("no definition")
            );
        }
        for new in ["new_python", "new_ts"] {
            assert!(
                !tools
                    .find_definition(new.into())
                    .await
                    .unwrap()
                    .contains("no definition")
            );
        }
    }

    #[tokio::test]
    async fn malformed_files_keep_good_declarations_and_generated_inputs_are_pruned() {
        let (tools, _, _, _dir) = indexed_fixture(&[
            (
                "broken.py",
                "def good():\n    return 1\n\ndef broken(\n\ndef later():\n    return good()\n",
            ),
            ("ignored.min.js", "function minified(){}"),
            ("node_modules/noise.js", "function dependency(){}"),
            ("__pycache__/noise.py", "def cached(): pass"),
            ("site-packages/noise.py", "def installed(): pass"),
            ("venv/noise.py", "def virtualized(): pass"),
            ("unsupported.go", "func unsupported() {}"),
        ])
        .await;

        assert!(
            !tools
                .find_definition("good".into())
                .await
                .unwrap()
                .contains("no definition")
        );
        assert!(
            !tools
                .find_definition("later".into())
                .await
                .unwrap()
                .contains("no definition")
        );
        for omitted in [
            "minified",
            "dependency",
            "cached",
            "installed",
            "virtualized",
            "unsupported",
        ] {
            assert!(
                tools
                    .find_definition(omitted.into())
                    .await
                    .unwrap()
                    .contains("no definition"),
                "unexpectedly indexed {omitted}"
            );
        }
    }

    #[tokio::test]
    async fn finds_definition_and_callers() {
        let (tools, _dir) = indexed_tools(
            "fn helper() -> u32 { 1 }\n\
             fn caller() -> u32 { helper() }\n",
        )
        .await;

        let def = tools.find_definition("helper".into()).await.unwrap();
        assert!(def.contains("lib.rs"), "def was: {def}");
        assert!(def.contains("Function"), "def was: {def}");

        let callers = tools.callers_of("helper".into()).await.unwrap();
        assert!(callers.contains("caller"), "callers were: {callers}");

        let callees = tools.callees_of("caller".into()).await.unwrap();
        assert!(callees.contains("helper"), "callees were: {callees}");
    }

    #[tokio::test]
    async fn finds_trait_impls() {
        let (tools, _dir) = indexed_tools(
            "struct Widget;\n\
             trait Render { fn render(&self); }\n\
             impl Render for Widget { fn render(&self) {} }\n",
        )
        .await;

        let impls = tools.impls_of_trait("Render".into()).await.unwrap();
        assert!(impls.contains("Widget"), "impls were: {impls}");
    }

    #[tokio::test]
    async fn missing_symbol_reports_cleanly() {
        let (tools, _dir) = indexed_tools("fn only() {}\n").await;
        let def = tools.find_definition("nonexistent".into()).await.unwrap();
        assert!(def.contains("no definition found"), "got: {def}");
    }

    #[tokio::test]
    async fn outline_lists_symbols_in_position_order() {
        // Line numbers (1-based): blank-ish layout so spans are unambiguous.
        let (tools, _dir) = indexed_tools(
            "struct First;\n\
             fn second() {}\n\
             enum Third { A }\n",
        )
        .await;
        let outline = tools.outline("lib.rs".into()).await.unwrap();
        let lines: Vec<&str> = outline.lines().collect();
        // Struct (line 1) before Function (line 2) before Enum (line 3).
        let pos = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        assert!(outline.contains("Struct\tFirst"), "got: {outline}");
        assert!(
            pos("First") < pos("second"),
            "ordered by start line: {outline}"
        );
        assert!(
            pos("second") < pos("Third"),
            "ordered by start line: {outline}"
        );
    }

    #[tokio::test]
    async fn read_symbol_returns_full_span_with_doc_comments() {
        let (tools, _dir) = indexed_tools(
            "fn before() {}\n\
             /// docs for target\n\
             #[inline]\n\
             fn target() {\n\
             let x = 1;\n\
             }\n\
             fn after() {}\n",
        )
        .await;
        let src = tools.read_symbol("target".into()).await.unwrap();
        // Span starts at the doc comment / attribute, not the `fn` line.
        assert!(src.contains("docs for target"), "got: {src}");
        assert!(src.contains("#[inline]"), "got: {src}");
        assert!(src.contains("fn target()"), "got: {src}");
        // Neighbors must not bleed in.
        assert!(!src.contains("before"), "got: {src}");
        assert!(!src.contains("after"), "got: {src}");
    }

    #[tokio::test]
    async fn read_symbol_reports_missing_cleanly() {
        let (tools, _dir) = indexed_tools("fn only() {}\n").await;
        let out = tools.read_symbol("nope".into()).await.unwrap();
        assert!(out.contains("no symbol named 'nope'"), "got: {out}");
    }

    /// Index `src`, run the enrichment pass, and return concept tools over the
    /// derived graph.
    async fn enriched_concept_tools(src: &str) -> (ConceptTools, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), src).unwrap();
        let store = OxigraphStore::in_memory().unwrap();
        CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
        enrich(&store).unwrap();
        let config = CodeToolsConfig::new(dir.path());
        (ConceptTools::new(store, config), dir)
    }

    #[tokio::test]
    async fn enrichment_maps_attributes_and_trait_impls_to_roles() {
        let (tools, _dir) = enriched_concept_tools(
            "struct MyPlug;\n\
             impl Plugin for MyPlug { fn id() {} }\n\
             struct Tools;\n\
             impl Tools {\n\
             #[tool]\n\
             fn do_thing(&self) {}\n\
             }\n",
        )
        .await;

        let handlers = tools.find_by_role("ToolHandler".into()).await.unwrap();
        assert!(handlers.contains("do_thing"), "got: {handlers}");

        let plugins = tools.find_by_role("Plugin".into()).await.unwrap();
        assert!(plugins.contains("MyPlug"), "got: {plugins}");
    }

    #[tokio::test]
    async fn concepts_reports_role_counts() {
        let (tools, _dir) = enriched_concept_tools(
            "struct P;\n\
             impl Plugin for P { fn id() {} }\n",
        )
        .await;
        let concepts = tools.concepts().await.unwrap();
        assert!(concepts.contains("Plugin\t1"), "got: {concepts}");
        // A role with no members is still listed (count 0).
        assert!(concepts.contains("Agent\t0"), "got: {concepts}");
    }

    #[tokio::test]
    async fn enrichment_maps_res_params_to_access_edges() {
        let (tools, _dir) = enriched_concept_tools(
            "struct Ctx;\n\
             struct Cfg;\n\
             #[system]\n\
             async fn step(mut ctx: ResMut<Ctx>, cfg: Res<Cfg>) {}\n",
        )
        .await;

        let resources = tools.resources().await.unwrap();
        // Ctx is written once; Cfg is read once.
        assert!(resources.contains("Ctx\t0\t1"), "got: {resources}");
        assert!(resources.contains("Cfg\t1\t0"), "got: {resources}");

        let writers = tools.accessors_of("Ctx".into()).await.unwrap();
        assert!(writers.contains("writes"), "got: {writers}");
        assert!(writers.contains("step"), "got: {writers}");

        let readers = tools.accessors_of("Cfg".into()).await.unwrap();
        assert!(readers.contains("reads"), "got: {readers}");
        assert!(readers.contains("step"), "got: {readers}");
    }

    #[tokio::test]
    async fn accessors_of_unknown_resource_reports_cleanly() {
        let (tools, _dir) = enriched_concept_tools("fn x() {}\n").await;
        let out = tools.accessors_of("Nope".into()).await.unwrap();
        assert!(out.contains("no systems read or write"), "got: {out}");
    }

    #[tokio::test]
    async fn find_by_role_rejects_unknown_role() {
        let (tools, _dir) = enriched_concept_tools("fn x() {}\n").await;
        let err = tools.find_by_role("Bogus".into()).await.unwrap_err();
        assert!(err.to_string().contains("unknown role"), "got: {err}");
    }

    #[test]
    fn graph_search_mode_parses_and_gates() {
        assert_eq!(
            GraphSearchMode::from_str_or_default("curated"),
            GraphSearchMode::Curated
        );
        assert_eq!(
            GraphSearchMode::from_str_or_default("FREEFORM"),
            GraphSearchMode::Freeform
        );
        assert_eq!(
            GraphSearchMode::from_str_or_default("nonsense"),
            GraphSearchMode::Both
        );
        assert!(GraphSearchMode::Curated.curated_enabled());
        assert!(!GraphSearchMode::Curated.freeform_enabled());
        assert!(GraphSearchMode::Freeform.freeform_enabled());
        assert!(!GraphSearchMode::Freeform.curated_enabled());
        assert!(
            GraphSearchMode::Both.curated_enabled() && GraphSearchMode::Both.freeform_enabled()
        );
    }

    #[tokio::test]
    async fn read_symbol_disambiguates_collisions() {
        // Same name in two impls → ambiguous; list candidates rather than guess.
        let (tools, _dir) = indexed_tools(
            "struct A;\n\
             struct B;\n\
             impl A { fn run(&self) {} }\n\
             impl B { fn run(&self) {} }\n",
        )
        .await;
        let out = tools.read_symbol("run".into()).await.unwrap();
        assert!(out.contains("ambiguous"), "got: {out}");
        assert!(out.contains("read_span"), "got: {out}");
    }
}
