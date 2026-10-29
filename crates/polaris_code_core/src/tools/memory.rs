//! Agent memory — reflective memoization of structural conclusions.
//!
//! A memory is a `(question → claim)` the agent reached by investigation, plus
//! the source spans it cited. [`MemoryTools`] reifies memories into the memory
//! named graph ([`MEMORY_GRAPH_IRI`]) and recalls them on later turns, so the
//! agent can answer a repeated structural question (`memoize` → `recall_memory`)
//! without re-running the whole investigation.
//!
//! **Invalidation is content-hashed.** Each citation stores a hash of its cited
//! span's source at record time; `recall_memory` re-hashes the current source
//! and flags a memory **stale** when any cited span has changed. So a memory
//! survives only as long as the code it rests on does — an edit (which re-indexes
//! the Code `ABox`) silently expires the conclusions that depended on it.
//!
//! Memories live in the shared [`OxigraphStore`] alongside the Code and Session
//! graphs and are reused across turns within a run; they are not persisted
//! across sessions.

use oxigraph::sparql::QueryResults;
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolRegistry, ToolsPlugin, toolset};
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};

use crate::sparql::{enc, esc};
use crate::tools::code::CodeToolsConfig;

/// IRI of the memory named graph (memoized `question → claim` conclusions).
pub const MEMORY_GRAPH_IRI: &str = "urn:polaris:graph:memory";

/// Namespace for memory-graph classes and predicates.
const MNS: &str = "urn:polaris:memory:";

/// Hard cap on serialized tool-result size, mirroring the other toolsets.
const RESULT_BYTE_LIMIT: usize = 32 * 1024;

/// FNV-1a 64-bit hash of `text`, hex-encoded.
///
/// Deterministic across runs (unlike [`std::hash::DefaultHasher`], which is
/// randomized), so a citation's stored span hash is comparable on a later turn
/// or session — the property [`recall_memory`](MemoryTools::recall_memory)
/// relies on to detect that cited code has changed.
fn span_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A parsed citation: a file path and an inclusive 1-based line span.
struct Citation {
    path: String,
    start: usize,
    end: usize,
}

/// Parse one citation in `path:start-end` form (e.g. `src/main.rs:10-42`).
///
/// Splits on the final `:` so paths keep any earlier colons; returns `None` for
/// a malformed coordinate so the caller can report it.
fn parse_citation(s: &str) -> Option<Citation> {
    let s = s.trim();
    let (path, range) = s.rsplit_once(':')?;
    let (start, end) = range.split_once('-')?;
    let start: usize = start.trim().parse().ok()?;
    let end: usize = end.trim().parse().ok()?;
    if path.is_empty() || start == 0 || end < start {
        return None;
    }
    Some(Citation {
        path: path.to_string(),
        start,
        end,
    })
}

/// Truncate `s` to `limit` bytes on a char boundary, appending a marker.
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

/// Reflective memory store: reify and recall `(question → claim)` conclusions.
#[derive(Debug, Clone)]
pub struct MemoryTools {
    store: OxigraphStore,
    /// Sandbox config used to resolve a citation's path to its current source
    /// for content-hashing.
    config: CodeToolsConfig,
}

impl MemoryTools {
    /// Wrap a store handle in the toolset. `config` sandboxes the file reads used
    /// to hash cited spans.
    #[must_use]
    pub fn new(store: OxigraphStore, config: CodeToolsConfig) -> Self {
        Self { store, config }
    }

    /// The raw source of `[start, end]` (1-based inclusive) of `path`, without
    /// line numbers — the text whose hash keys a citation's freshness. Returns
    /// `None` if the path escapes the sandbox, is unreadable, or the span is out
    /// of range.
    fn span_text(&self, path: &str, start: usize, end: usize) -> Option<String> {
        let resolved = self.config.resolve_path(path)?;
        let content = std::fs::read_to_string(&resolved).ok()?;
        let lines: Vec<&str> = content.lines().collect();
        if start == 0 || start > lines.len() {
            return None;
        }
        let end = end.min(lines.len());
        Some(lines[start - 1..end].join("\n"))
    }
}

#[toolset]
impl MemoryTools {
    /// Memoize a structural conclusion for reuse: a `question`, the `claim` that
    /// answers it, and the source spans it rests on.
    ///
    /// `citations` is a comma- or semicolon-separated list of `path:start-end`
    /// coordinates (e.g. `src/main.rs:10-42, src/tools/mod.rs:5-8`). Each cited
    /// span's current source is hashed so a later `recall_memory` can tell
    /// whether the memory still holds. Memoizing the same `question` replaces the
    /// previous memory. Record only durable structural conclusions worth
    /// reusing — not transient observations.
    #[tool]
    async fn memoize(
        &self,
        /// The question this memory answers, e.g. "what enforces tool permissions".
        question: String,
        /// The claim that answers it — a concise, self-contained conclusion.
        claim: String,
        /// `path:start-end` citations, comma- or semicolon-separated.
        citations: String,
    ) -> Result<String, ToolError> {
        let question = question.trim();
        if question.is_empty() || claim.trim().is_empty() {
            return Err(ToolError::parameter_error(
                "question and claim must be non-empty",
            ));
        }
        let parsed: Vec<Citation> = citations
            .split([',', ';', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                parse_citation(s).ok_or_else(|| {
                    ToolError::parameter_error(format!(
                        "malformed citation '{s}'; expected path:start-end"
                    ))
                })
            })
            .collect::<Result<_, _>>()?;
        if parsed.is_empty() {
            return Err(ToolError::parameter_error(
                "at least one path:start-end citation is required",
            ));
        }

        // The memory IRI is deterministic from the question, so memoizing the
        // same question reuses the node. Latest-wins: drop the prior memory's
        // triples (its citation nodes first, then its own) by IRI — keying the
        // second delete on the IRI rather than the question, which the first
        // delete would already have removed.
        let id = format!("{MNS}m/{}", enc(question));
        let del = format!(
            "DELETE WHERE {{ GRAPH <{MEMORY_GRAPH_IRI}> {{ \
             <{id}> <{MNS}cites> ?c . ?c ?cp ?co . }} }};\
             DELETE WHERE {{ GRAPH <{MEMORY_GRAPH_IRI}> {{ <{id}> ?p ?o . }} }}",
        );
        self.store
            .update(&del)
            .map_err(|err| ToolError::execution_error(format!("memory delete failed: {err}")))?;
        let mut triples = vec![
            format!("<{id}> a <{MNS}Memory> ."),
            format!("<{id}> <{MNS}question> \"{}\" .", esc(question)),
            format!("<{id}> <{MNS}claim> \"{}\" .", esc(claim.trim())),
        ];
        for (i, c) in parsed.iter().enumerate() {
            let hash = self
                .span_text(&c.path, c.start, c.end)
                .map(|t| span_hash(&t))
                .unwrap_or_else(|| "missing".to_string());
            let cite = format!("{id}/cite/{i}");
            triples.push(format!("<{id}> <{MNS}cites> <{cite}> ."));
            triples.push(format!("<{cite}> <{MNS}path> \"{}\" .", esc(&c.path)));
            triples.push(format!("<{cite}> <{MNS}startLine> \"{}\" .", c.start));
            triples.push(format!("<{cite}> <{MNS}endLine> \"{}\" .", c.end));
            triples.push(format!("<{cite}> <{MNS}spanHash> \"{hash}\" ."));
        }
        let ins = format!(
            "INSERT DATA {{ GRAPH <{MEMORY_GRAPH_IRI}> {{ {} }} }}",
            triples.join("\n")
        );
        self.store
            .update(&ins)
            .map_err(|err| ToolError::execution_error(format!("memory insert failed: {err}")))?;

        Ok(format!(
            "memoized '{question}' citing {} span(s)",
            parsed.len()
        ))
    }

    /// Recall prior memories whose question matches `question` (case-insensitive,
    /// either direction of substring), with each cited span re-checked for
    /// freshness.
    ///
    /// Returns each memory's claim and citations; a memory whose cited code has
    /// changed since it was recorded is flagged `[STALE]`, so a stale conclusion
    /// is surfaced for re-verification rather than silently trusted. Call this
    /// before a structural investigation to reuse a prior conclusion.
    #[tool]
    async fn recall_memory(
        &self,
        /// The question to look up, e.g. "what enforces tool permissions".
        question: String,
    ) -> Result<String, ToolError> {
        let question = question.trim();
        if question.is_empty() {
            return Err(ToolError::parameter_error("question must be non-empty"));
        }
        let q = format!(
            "SELECT ?m ?question ?claim WHERE {{ GRAPH <{MEMORY_GRAPH_IRI}> {{ \
             ?m a <{MNS}Memory> ; <{MNS}question> ?question ; <{MNS}claim> ?claim . \
             FILTER(CONTAINS(LCASE(?question), LCASE(\"{needle}\")) || \
                    CONTAINS(LCASE(\"{needle}\"), LCASE(?question))) }} }} LIMIT 20",
            needle = esc(question),
        );
        let results = self
            .store
            .query(&q)
            .map_err(|err| ToolError::execution_error(format!("query failed: {err}")))?;
        let QueryResults::Solutions(solutions) = results else {
            return Ok(format!("no memory for '{question}'"));
        };

        let mut memories = Vec::new();
        for solution in solutions {
            let solution =
                solution.map_err(|err| ToolError::execution_error(format!("row failed: {err}")))?;
            let m = solution.get("m").map(term_str).unwrap_or_default();
            let recorded_q = solution.get("question").map(term_str).unwrap_or_default();
            let claim = solution.get("claim").map(term_str).unwrap_or_default();
            memories.push((m, recorded_q, claim));
        }
        if memories.is_empty() {
            return Ok(format!("no memory for '{question}'"));
        }

        let mut out = String::new();
        for (m, recorded_q, claim) in memories {
            let cites = self.citations_of(&m)?;
            let stale = cites.iter().any(|(_, _, _, fresh)| !fresh);
            let marker = if stale {
                " [STALE — cited code changed]"
            } else {
                ""
            };
            out.push_str(&format!(
                "Q: {recorded_q}{marker}\nA: {claim}\ncitations:\n"
            ));
            for (path, start, end, fresh) in &cites {
                let flag = if *fresh { "" } else { "  (changed)" };
                out.push_str(&format!("  {path}:{start}-{end}{flag}\n"));
            }
            out.push('\n');
        }
        Ok(truncate(out.trim_end().to_string(), RESULT_BYTE_LIMIT))
    }
}

impl MemoryTools {
    /// Citations of memory `m` as `(path, start, end, fresh)`, where `fresh` is
    /// whether the cited span's current hash still matches the recorded one.
    fn citations_of(&self, m: &str) -> Result<Vec<(String, usize, usize, bool)>, ToolError> {
        let q = format!(
            "SELECT ?path ?start ?end ?hash WHERE {{ GRAPH <{MEMORY_GRAPH_IRI}> {{ \
             <{m}> <{MNS}cites> ?c . ?c <{MNS}path> ?path ; <{MNS}startLine> ?start ; \
             <{MNS}endLine> ?end ; <{MNS}spanHash> ?hash . }} }} ORDER BY ?path",
        );
        let results = self
            .store
            .query(&q)
            .map_err(|err| ToolError::execution_error(format!("query failed: {err}")))?;
        let QueryResults::Solutions(solutions) = results else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        for solution in solutions {
            let solution =
                solution.map_err(|err| ToolError::execution_error(format!("row failed: {err}")))?;
            let path = solution.get("path").map(term_str).unwrap_or_default();
            let start = solution
                .get("start")
                .map(term_str)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let end = solution
                .get("end")
                .map(term_str)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let recorded = solution.get("hash").map(term_str).unwrap_or_default();
            let current = self
                .span_text(&path, start, end)
                .map(|t| span_hash(&t))
                .unwrap_or_else(|| "missing".to_string());
            rows.push((path, start, end, current == recorded));
        }
        Ok(rows)
    }
}

fn term_str(t: &oxigraph::model::Term) -> String {
    match t {
        oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
        oxigraph::model::Term::Literal(l) => l.value().to_string(),
        oxigraph::model::Term::BlankNode(b) => b.as_str().to_string(),
        other => other.to_string(),
    }
}

/// Plugin that registers [`MemoryTools`] over the shared [`OxigraphStore`].
///
/// # Tools Provided
///
/// | Tool | Description |
/// |------|-------------|
/// | `memoize` | Record a `(question → claim)` conclusion with the source spans it cites. |
/// | `recall_memory` | Recall a prior conclusion, flagging it `[STALE]` if any cited span has changed. |
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | (none)   | —     | Registers tools on the existing `ToolRegistry`; provides no new resource. |
///
/// # Dependencies
///
/// - [`ToolsPlugin`] — the [`ToolRegistry`] this plugin populates.
/// - [`OxigraphPlugin`] — the [`OxigraphStore`] global the memory graph lives in.
pub struct MemoryToolsPlugin {
    working_dir: std::path::PathBuf,
}

impl MemoryToolsPlugin {
    /// Create a plugin whose citation reads are sandboxed to `working_dir`.
    #[must_use]
    pub fn new(working_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
        }
    }
}

impl Plugin for MemoryToolsPlugin {
    const ID: &'static str = "polaris_code::memory";
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
                .expect("OxigraphPlugin must be added before MemoryToolsPlugin");
            OxigraphStore::clone(&g)
        };
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before MemoryToolsPlugin");
        registry.register_toolset(MemoryTools::new(
            store,
            CodeToolsConfig::new(self.working_dir.clone()),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build memory tools over an in-memory store with `src` written as the file
    /// citations resolve against, returning the tempdir kept alive for reads.
    fn tools_with_file(name: &str, src: &str) -> (MemoryTools, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), src).unwrap();
        let store = OxigraphStore::in_memory().unwrap();
        let tools = MemoryTools::new(store, CodeToolsConfig::new(dir.path()));
        (tools, dir)
    }

    #[test]
    fn parse_citation_handles_well_formed_and_rejects_garbage() {
        let c = parse_citation("src/main.rs:10-42").unwrap();
        assert_eq!((c.path.as_str(), c.start, c.end), ("src/main.rs", 10, 42));
        assert!(parse_citation("no-range.rs").is_none());
        assert!(parse_citation("src/x.rs:5-2").is_none(), "inverted range");
        assert!(parse_citation("src/x.rs:0-3").is_none(), "zero start");
    }

    #[tokio::test]
    async fn memoize_then_recall_returns_claim() {
        let (tools, _dir) = tools_with_file("a.rs", "fn one() {}\nfn two() {}\n");
        tools
            .memoize(
                "where is one defined".into(),
                "in a.rs at the top".into(),
                "a.rs:1-1".into(),
            )
            .await
            .unwrap();

        let out = tools
            .recall_memory("where is one defined".into())
            .await
            .unwrap();
        assert!(out.contains("in a.rs at the top"), "got: {out}");
        assert!(out.contains("a.rs:1-1"), "got: {out}");
        assert!(
            !out.contains("STALE"),
            "fresh memory must not be stale: {out}"
        );
    }

    #[tokio::test]
    async fn recall_flags_stale_when_cited_span_changes() {
        let (tools, dir) = tools_with_file("a.rs", "fn one() {}\nfn two() {}\n");
        tools
            .memoize(
                "shape of one".into(),
                "one is a no-op fn".into(),
                "a.rs:1-1".into(),
            )
            .await
            .unwrap();
        // Mutate the cited line so its hash no longer matches.
        std::fs::write(
            dir.path().join("a.rs"),
            "fn one() { todo!() }\nfn two() {}\n",
        )
        .unwrap();

        let out = tools.recall_memory("shape of one".into()).await.unwrap();
        assert!(out.contains("STALE"), "got: {out}");
        assert!(out.contains("(changed)"), "got: {out}");
    }

    #[tokio::test]
    async fn recall_misses_report_cleanly() {
        let (tools, _dir) = tools_with_file("a.rs", "fn one() {}\n");
        let out = tools
            .recall_memory("unrelated question".into())
            .await
            .unwrap();
        assert!(out.contains("no memory"), "got: {out}");
    }

    #[tokio::test]
    async fn memoizing_same_question_replaces_prior() {
        let (tools, _dir) = tools_with_file("a.rs", "fn one() {}\nfn two() {}\n");
        tools
            .memoize("q".into(), "first answer".into(), "a.rs:1-1".into())
            .await
            .unwrap();
        tools
            .memoize("q".into(), "second answer".into(), "a.rs:2-2".into())
            .await
            .unwrap();

        let out = tools.recall_memory("q".into()).await.unwrap();
        assert!(out.contains("second answer"), "got: {out}");
        assert!(
            !out.contains("first answer"),
            "prior memory must be replaced: {out}"
        );
    }

    #[tokio::test]
    async fn memoize_records_every_citation_across_mixed_separators() {
        let (tools, _dir) = tools_with_file("a.rs", "fn one() {}\nfn two() {}\nfn three() {}\n");
        // Comma, semicolon, and newline separators in one list — each span must
        // be parsed and reified.
        tools
            .memoize(
                "the three fns".into(),
                "one, two, three".into(),
                "a.rs:1-1, a.rs:2-2; a.rs:3-3".into(),
            )
            .await
            .unwrap();

        let out = tools.recall_memory("the three fns".into()).await.unwrap();
        assert!(out.contains("a.rs:1-1"), "got: {out}");
        assert!(out.contains("a.rs:2-2"), "got: {out}");
        assert!(out.contains("a.rs:3-3"), "got: {out}");
        assert!(!out.contains("STALE"), "all spans fresh: {out}");
    }

    #[tokio::test]
    async fn memoize_rejects_malformed_citation() {
        let (tools, _dir) = tools_with_file("a.rs", "fn one() {}\n");
        let err = tools
            .memoize("q".into(), "a".into(), "not-a-citation".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("malformed citation"), "got: {err}");
    }

    #[test]
    fn span_hash_is_deterministic_and_sensitive() {
        assert_eq!(span_hash("hello"), span_hash("hello"));
        assert_ne!(span_hash("hello"), span_hash("hellp"));
    }
}
