//! Reified trajectory — control decisions and tool calls as Session `ABox` nodes.
//!
//! This is the primitive that makes the agent *reflective*: every control
//! decision is written into the session graph ([`SESSION_GRAPH_IRI`]) linking
//! the choice, the options considered, and the policy consulted. This
//! **write-only** projection records provenance ("why did the agent do that?").
//! Writes are best-effort:
//! a failed record is logged, never propagated, so trajectory bookkeeping can
//! never abort a turn.

use std::sync::atomic::{AtomicU64, Ordering};

use polaris_ontrace::oxigraph::OxigraphStore;

use crate::sparql::{enc, esc};

/// IRI of the Session `ABox` named graph (reified agent trajectory).
pub const SESSION_GRAPH_IRI: &str = "urn:polaris:graph:session";

/// Namespace for session-graph classes and predicates.
const SNS: &str = "urn:polaris:session:";

/// Byte cap on a reified tool-call's output detail. Large enough to keep a
/// useful slice for provenance/auditing without bloating the session graph.
const DETAIL_CAP: usize = 2_000;
/// Byte cap on a reified tool-call's serialized arguments.
const ARGS_CAP: usize = 1_000;

/// Monotonic suffix keeping co-timed node IRIs distinct.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Borrow the leading `limit` *bytes* of `s`, walked back to the nearest UTF-8
/// char boundary so multibyte tool output never panics the truncation.
fn cap(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut cut = limit;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// A reified control decision: the choice a decision point made, with provenance.
#[derive(Debug, Clone)]
pub struct Decision {
    /// Decision point: `retrieval` | `tool_palette` | `permission` | `retry` | `termination`.
    pub kind: &'static str,
    /// IRI of the Intent this decision serves, if known.
    pub intent: Option<String>,
    /// The options that were available.
    pub options: Vec<String>,
    /// The option chosen.
    pub chosen: String,
    /// Policy keys/triples consulted (provenance).
    pub policy_refs: Vec<String>,
}

impl Decision {
    /// Convenience constructor for a decision with a chosen value and no options.
    #[must_use]
    pub fn new(kind: &'static str, chosen: impl Into<String>) -> Self {
        Self {
            kind,
            intent: None,
            options: Vec::new(),
            chosen: chosen.into(),
            policy_refs: Vec::new(),
        }
    }

    /// Attach consulted policy keys.
    #[must_use]
    pub fn with_policy(mut self, refs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.policy_refs = refs.into_iter().map(Into::into).collect();
        self
    }

    /// Write this decision into the Session `ABox`. Best-effort; logs on failure.
    pub fn record(&self, store: &OxigraphStore, turn: usize) {
        let id = format!("{SNS}decision/{turn}/{}/{}", enc(self.kind), unique());
        let mut triples = vec![
            format!("<{id}> a <{SNS}Decision> ."),
            format!("<{id}> <{SNS}kind> \"{}\" .", esc(self.kind)),
            format!("<{id}> <{SNS}atTurn> \"{turn}\" ."),
            format!("<{id}> <{SNS}chose> \"{}\" .", esc(&self.chosen)),
        ];
        if let Some(intent) = &self.intent {
            triples.push(format!("<{id}> <{SNS}forIntent> <{intent}> ."));
        }
        for o in &self.options {
            triples.push(format!("<{id}> <{SNS}option> \"{}\" .", esc(o)));
        }
        for p in &self.policy_refs {
            triples.push(format!("<{id}> <{SNS}consultedPolicy> \"{}\" .", esc(p)));
        }
        insert(store, &triples);
    }
}

/// Reify a tool invocation and its outcome into the Session `ABox`. Best-effort.
///
/// Records the invoked `args` (serialized, capped at [`ARGS_CAP`]) alongside the
/// outcome `detail` (capped at [`DETAIL_CAP`]) so the provenance captures *what
/// was asked*, not only what came back, so an audit can judge the call.
pub fn record_tool_call(
    store: &OxigraphStore,
    turn: usize,
    tool: &str,
    args: &serde_json::Value,
    status: &str,
    detail: &str,
) {
    let id = format!("{SNS}toolcall/{turn}/{}/{}", enc(tool), unique());
    let detail = cap(detail, DETAIL_CAP);
    let args_str = args.to_string();
    let args_str = cap(&args_str, ARGS_CAP);
    let triples = vec![
        format!("<{id}> a <{SNS}ToolCall> ."),
        format!("<{id}> <{SNS}tool> \"{}\" .", esc(tool)),
        format!("<{id}> <{SNS}atTurn> \"{turn}\" ."),
        format!("<{id}> <{SNS}status> \"{}\" .", esc(status)),
        format!("<{id}> <{SNS}args> \"{}\" .", esc(args_str)),
        format!("<{id}> <{SNS}detail> \"{}\" .", esc(detail)),
    ];
    insert(store, &triples);
}

fn insert(store: &OxigraphStore, triples: &[String]) {
    let update = format!(
        "INSERT DATA {{ GRAPH <{SESSION_GRAPH_IRI}> {{ {} }} }}",
        triples.join("\n")
    );
    if let Err(e) = store.update(&update) {
        tracing::warn!("trajectory: failed to record into session graph: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_is_queryable_after_record() {
        let store = OxigraphStore::in_memory().unwrap();
        Decision::new("termination", "complete")
            .with_policy(["retry.max_attempts=2"])
            .record(&store, 3);

        let ok = store
            .query(&format!(
                "ASK {{ GRAPH <{SESSION_GRAPH_IRI}> {{ \
                 ?d a <{SNS}Decision> ; <{SNS}kind> \"termination\" ; <{SNS}chose> \"complete\" }} }}"
            ))
            .unwrap();
        assert!(matches!(ok, oxigraph::sparql::QueryResults::Boolean(true)));
    }

    #[test]
    fn tool_call_is_queryable_after_record() {
        let store = OxigraphStore::in_memory().unwrap();
        record_tool_call(
            &store,
            1,
            "edit_file",
            &serde_json::json!({ "path": "a.rs" }),
            "ok",
            "Edited a.rs",
        );
        let ok = store
            .query(&format!(
                "ASK {{ GRAPH <{SESSION_GRAPH_IRI}> {{ \
                 ?c a <{SNS}ToolCall> ; <{SNS}tool> \"edit_file\" ; <{SNS}status> \"ok\" }} }}"
            ))
            .unwrap();
        assert!(matches!(ok, oxigraph::sparql::QueryResults::Boolean(true)));

        // The invoked arguments are reified too, not just the outcome.
        let has_args = store
            .query(&format!(
                "ASK {{ GRAPH <{SESSION_GRAPH_IRI}> {{ \
                 ?c <{SNS}tool> \"edit_file\" ; <{SNS}args> ?a . \
                 FILTER(CONTAINS(?a, \"a.rs\")) }} }}"
            ))
            .unwrap();
        assert!(matches!(
            has_args,
            oxigraph::sparql::QueryResults::Boolean(true)
        ));
    }

    #[test]
    fn record_tool_call_escapes_adversarial_args() {
        use oxigraph::model::Term;

        let store = OxigraphStore::in_memory().unwrap();
        // Args carrying the exact characters `esc()` exists to neutralize — a
        // double quote, a backslash, and a newline. A naive interpolation would
        // either break the SPARQL UPDATE (so nothing is stored) or corrupt the
        // value; `esc()` must keep the update well-formed *and* round-trip the
        // bytes intact.
        let args = serde_json::json!({ "command": "grep \"x\\y\" .\nls" });
        record_tool_call(&store, 1, "run_command", &args, "ok", "done");

        let results = store
            .query(&format!(
                "SELECT ?a WHERE {{ GRAPH <{SESSION_GRAPH_IRI}> {{ \
                 ?c <{SNS}tool> \"run_command\" ; <{SNS}args> ?a }} }}"
            ))
            .unwrap();
        let oxigraph::sparql::QueryResults::Solutions(solutions) = results else {
            panic!("expected SELECT solutions");
        };
        let rows: Vec<_> = solutions.map(Result::unwrap).collect();
        assert_eq!(rows.len(), 1, "exactly one tool call recorded");
        let Some(Term::Literal(lit)) = rows[0].get("a") else {
            panic!("args bound to a literal");
        };
        // The serialized args survive verbatim: esc() → store → query un-escape.
        assert_eq!(lit.value(), args.to_string());
    }
}
