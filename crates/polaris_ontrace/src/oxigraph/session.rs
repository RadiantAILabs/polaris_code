//! [`OxigraphSession`] — `LocalResource` for per-session scoped graph work.
//!
//! Each session gets:
//!
//! - A **named-graph URI** (`urn:polaris:session:<id>`) it can write into.
//!   Reads can SPARQL-union it with the shared ontology and data graphs.
//! - A **staging buffer** (`Vec<Quad>`) for in-flight quads that should not yet
//!   be visible to other sessions. [`Self::commit`] promotes the buffer to the
//!   shared store atomically (per-quad), [`Self::discard`] drops it.
//!
//! Holding *both* a graph handle and a staging buffer lets agents:
//! - Stage writes during exploration, commit once they're confident, OR
//! - Bypass staging and write directly to the session graph via
//!   [`OxigraphStore::insert_quad`] with [`Self::graph`] as the quad's graph
//!   name.
//!
//! [`OxigraphStore::insert_quad`]: super::OxigraphStore::insert_quad

use oxigraph::model::{NamedNode, NamedNodeRef, Quad};
use oxigraph::store::StorageError;
use polaris::system::resource::LocalResource;

use super::store::OxigraphStore;

/// Prefix used when generating per-session named-graph IRIs.
pub const SESSION_GRAPH_PREFIX: &str = "urn:polaris:session:";

/// Per-session staging area + named-graph handle.
///
/// Inserted as a `LocalResource` at session creation; accessible from systems
/// via `ResMut<OxigraphSession>`. The buffer is private to the session — it
/// does **not** become visible to SPARQL queries until [`Self::commit`] runs.
#[derive(Debug)]
pub struct OxigraphSession {
    graph: NamedNode,
    staged: Vec<Quad>,
}

impl OxigraphSession {
    /// Build a session bound to `urn:polaris:session:<id>`.
    ///
    /// # Panics
    ///
    /// Panics if `id` contains characters that produce an invalid IRI when
    /// concatenated with [`SESSION_GRAPH_PREFIX`]. Use [`Self::try_new`] for
    /// untrusted input.
    #[must_use]
    pub fn new(id: impl AsRef<str>) -> Self {
        Self::try_new(id).expect("session id produces a valid IRI")
    }

    /// Fallible counterpart to [`Self::new`].
    ///
    /// # Errors
    ///
    /// Returns the underlying IRI parse error if the constructed graph IRI is
    /// invalid.
    pub fn try_new(id: impl AsRef<str>) -> Result<Self, oxigraph::model::IriParseError> {
        let iri = format!("{SESSION_GRAPH_PREFIX}{}", id.as_ref());
        Ok(Self {
            graph: NamedNode::new(iri)?,
            staged: Vec::new(),
        })
    }

    /// Build a session with a random UUIDv4-based graph IRI.
    #[must_use]
    pub fn random() -> Self {
        Self::new(uuid::Uuid::new_v4().to_string())
    }

    /// Build a session bound to an explicit pre-built graph IRI.
    ///
    /// Use this when the caller already has a meaningful session identity
    /// (e.g. a Polaris `SessionId`) it wants to embed in the graph URI.
    #[must_use]
    pub fn with_graph(graph: NamedNode) -> Self {
        Self {
            graph,
            staged: Vec::new(),
        }
    }

    /// IRI of the session's named graph.
    #[must_use]
    pub fn graph(&self) -> NamedNodeRef<'_> {
        self.graph.as_ref()
    }

    /// Stage a quad for later commit. The quad's `graph_name` is **not**
    /// rewritten — pass quads tagged with the session's graph (or any other
    /// target) explicitly.
    pub fn stage(&mut self, quad: Quad) {
        self.staged.push(quad);
    }

    /// All currently staged quads.
    #[must_use]
    pub fn staged(&self) -> &[Quad] {
        &self.staged
    }

    /// Number of quads waiting to be committed.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.staged.len()
    }

    /// `true` if nothing is staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.staged.is_empty()
    }

    /// Promote staged quads into `store`, draining the buffer. Returns the
    /// number of newly-inserted quads (quads already present in the store
    /// are counted but do not appear twice).
    ///
    /// Each insert is independently transactional in oxigraph; this method is
    /// **not** atomic across the whole batch — a mid-batch storage failure
    /// leaves the store partially updated and the remaining quads still in the
    /// buffer.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if a single insert fails. The buffer retains
    /// the not-yet-attempted suffix in that case.
    pub fn commit(&mut self, store: &OxigraphStore) -> Result<usize, StorageError> {
        let mut inserted = 0usize;
        while let Some(quad) = self.staged.first() {
            store.insert_quad(quad.as_ref())?;
            inserted += 1;
            self.staged.remove(0);
        }
        Ok(inserted)
    }

    /// Drop all staged quads without writing them anywhere.
    pub fn discard(&mut self) {
        self.staged.clear();
    }
}

impl LocalResource for OxigraphSession {}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{GraphName, NamedNode, Quad};

    fn q(s: &str, p: &str, o: &str, g: &NamedNode) -> Quad {
        Quad::new(
            NamedNode::new(s).unwrap(),
            NamedNode::new(p).unwrap(),
            NamedNode::new(o).unwrap(),
            GraphName::NamedNode(g.clone()),
        )
    }

    #[test]
    fn graph_iri_uses_session_prefix() {
        let s = OxigraphSession::new("abc-123");
        assert_eq!(s.graph().as_str(), "urn:polaris:session:abc-123");
    }

    #[test]
    fn random_is_unique() {
        let a = OxigraphSession::random();
        let b = OxigraphSession::random();
        assert_ne!(a.graph().as_str(), b.graph().as_str());
    }

    #[test]
    fn stage_and_commit_writes_to_store() {
        let store = OxigraphStore::in_memory().unwrap();
        let mut session = OxigraphSession::new("commit-test");
        let g = NamedNode::new(session.graph().as_str()).unwrap();

        session.stage(q("urn:alice", "urn:knows", "urn:bob", &g));
        session.stage(q("urn:alice", "urn:knows", "urn:carol", &g));

        assert_eq!(session.pending(), 2);
        let n = session.commit(&store).unwrap();
        assert_eq!(n, 2);
        assert!(session.is_empty());

        // Both quads now live in the session named graph.
        assert_eq!(store.len().unwrap(), 2);
    }

    #[test]
    fn discard_drops_buffer_without_writing() {
        let store = OxigraphStore::in_memory().unwrap();
        let mut session = OxigraphSession::new("discard-test");
        let g = NamedNode::new(session.graph().as_str()).unwrap();

        session.stage(q("urn:x", "urn:p", "urn:y", &g));
        session.discard();

        assert!(session.is_empty());
        assert_eq!(store.len().unwrap(), 0);
    }
}
