//! In-memory temporal knowledge graph.

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use polaris::system::resource::GlobalResource;
use std::collections::HashMap;

/// Observation timestamp for a triple.
pub type Timestamp = DateTime<Utc>;

/// A canonical entity. This type holds the raw extracted string;
/// canonicalization (e.g. "Alice" ≡ "alice") is not applied by `Entity`
/// itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entity(pub String);

impl Entity {
    /// Construct from any displayable value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// A canonical predicate (the verb / relation type).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Predicate(pub String);

impl Predicate {
    /// Construct from any displayable value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// A single (subject, predicate, object) observation with a timestamp.
#[derive(Clone, Debug)]
pub struct Triple {
    /// Subject entity.
    pub subject: Entity,
    /// Predicate / relation.
    pub predicate: Predicate,
    /// Object entity.
    pub object: Entity,
    /// When this triple was observed (typically the trace line's arrival time).
    pub observed_at: Timestamp,
    /// Optional upstream trace identifier (e.g. an `OTel` trace id), if known.
    pub trace_id: Option<String>,
}

#[derive(Default, Debug)]
struct KgInner {
    triples: Vec<Triple>,
    by_subject: HashMap<Entity, Vec<usize>>,
    by_predicate: HashMap<Predicate, Vec<usize>>,
}

/// In-memory temporal knowledge graph — the durable extraction target.
///
/// Use this when a system needs to **write** extracted facts (via
/// [`KnowledgeGraph::insert`]) or **read back** the accumulated graph for
/// reporting, dumping, or per-subject/per-predicate lookup. It is the shared
/// sink the extraction pipeline writes into and the source every downstream
/// consumer (Turtle dump, KG-size ticker, inspection) reads from.
///
/// # Scope
///
/// `Global` — extraction runs as one ephemeral session per trace line, but
/// the facts must accumulate across all of them, so a single shared graph
/// outlives any one session.
///
/// # Provided by
///
/// [`crate::ExtractionPlugin`] inserts it in `build()` via
/// `server.insert_global(KnowledgeGraph::new())`. The oxigraph backend does
/// **not** register it — it writes to `OxigraphStore` instead (see
/// *Alternatives*).
///
/// # Access
///
/// Consume as `Res<KnowledgeGraph>`. `ResMut<T>` is not used for globals in
/// this crate; mutation goes through the interior [`parking_lot::RwLock`], so
/// even the write path ([`KnowledgeGraph::insert`]) takes `&self`.
///
/// The mutation contract is **append-only with timestamps**: every
/// [`KnowledgeGraph::insert`] adds a row, and repeated `(S, P, O)`
/// observations are intentionally kept (each carries its own `observed_at`)
/// rather than deduplicated, building a temporal history.
///
/// # Alternatives
///
/// | Alternative | When |
/// | --- | --- |
/// | `OxigraphStore` (`oxigraph` feature) | RDF-backed backend registered by `OxigraphExtractionPlugin`; choose it for SPARQL queries, SHACL validation, and a durable on-disk graph instead of this in-memory store. The two backends are mutually exclusive — exactly one extraction backend is registered. |
///
/// # Example system
///
/// ```no_run
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::{KnowledgeGraph, Predicate};
///
/// /// Count how many `met` relations have been observed so far.
/// #[system]
/// async fn count_meetings(kg: Res<KnowledgeGraph>) {
///     let met = kg.by_predicate(&Predicate::new("met"));
///     tracing::info!(count = met.len(), "met observations");
/// }
/// ```
#[derive(Default, Debug)]
pub struct KnowledgeGraph {
    inner: RwLock<KgInner>,
}

impl KnowledgeGraph {
    /// Create an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a new triple. Indices on subject and predicate are updated.
    pub fn insert(&self, triple: Triple) {
        let mut g = self.inner.write();
        let idx = g.triples.len();
        g.by_subject
            .entry(triple.subject.clone())
            .or_default()
            .push(idx);
        g.by_predicate
            .entry(triple.predicate.clone())
            .or_default()
            .push(idx);
        g.triples.push(triple);
    }

    /// Total number of triples observed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().triples.len()
    }

    /// True if no triples have been observed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot all triples (clones the storage — use sparingly).
    #[must_use]
    pub fn snapshot(&self) -> Vec<Triple> {
        self.inner.read().triples.clone()
    }

    /// All triples whose subject matches `entity`.
    #[must_use]
    pub fn by_subject(&self, entity: &Entity) -> Vec<Triple> {
        let g = self.inner.read();
        g.by_subject
            .get(entity)
            .map(|idxs| idxs.iter().map(|&i| g.triples[i].clone()).collect())
            .unwrap_or_default()
    }

    /// All triples whose predicate matches `predicate`.
    #[must_use]
    pub fn by_predicate(&self, predicate: &Predicate) -> Vec<Triple> {
        let g = self.inner.read();
        g.by_predicate
            .get(predicate)
            .map(|idxs| idxs.iter().map(|&i| g.triples[i].clone()).collect())
            .unwrap_or_default()
    }
}

impl GlobalResource for KnowledgeGraph {}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str, p: &str, o: &str) -> Triple {
        Triple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            observed_at: Utc::now(),
            trace_id: None,
        }
    }

    #[test]
    fn insert_and_query() {
        let kg = KnowledgeGraph::new();
        assert!(kg.is_empty());

        kg.insert(t("alice", "met", "bob"));
        kg.insert(t("alice", "works_at", "acme"));
        kg.insert(t("bob", "works_at", "acme"));

        assert_eq!(kg.len(), 3);
        assert_eq!(kg.by_subject(&Entity::new("alice")).len(), 2);
        assert_eq!(kg.by_predicate(&Predicate::new("works_at")).len(), 2);
    }

    #[test]
    fn temporal_duplicates_are_kept() {
        let kg = KnowledgeGraph::new();
        kg.insert(t("alice", "located_in", "nyc"));
        kg.insert(t("alice", "located_in", "nyc"));
        assert_eq!(kg.len(), 2, "duplicates kept for temporal history");
    }
}
