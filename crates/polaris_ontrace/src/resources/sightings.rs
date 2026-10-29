//! Diagnostic log of `trace line → extracted triples` pairings.
//!
//! Both extraction backends accumulate triples into the KG, which is great
//! for SPARQL but tells you nothing about *which* trace line produced
//! *which* triples — a crucial view when you're trying to spot
//! hallucinations or schema-talk extractions.
//!
//! [`Sightings`] is the missing log: each [`Sighting`] is one LLM call's
//! input line paired with the triples it produced (or didn't). Held as a
//! [`GlobalResource`] so a single accumulator survives across sessions,
//! and consumed by [`crate::dump::write_pretty`] to render the
//! markdown sidecar that ships alongside the Turtle snapshot.
//!
//! Append-only and unbounded — same lifecycle as
//! [`crate::resources::kg::KnowledgeGraph`]. Memory grows with extraction
//! traffic; trim or roll if that becomes load-bearing.

use crate::agents::extraction::RawTriple;
use crate::resources::kg::Timestamp;
use parking_lot::RwLock;
use polaris::system::resource::GlobalResource;

/// One LLM extraction call: the raw input line plus the triples returned.
#[derive(Debug, Clone)]
pub struct Sighting {
    /// Original text the extractor saw.
    pub line: String,
    /// When the line was observed (carried over from [`crate::TraceLine`]).
    pub observed_at: Timestamp,
    /// Upstream trace identifier, if the source surfaced one.
    pub trace_id: Option<String>,
    /// Triples the extractor returned for this line. Empty when the LLM
    /// declined to extract anything (e.g. under `OntologyPolicy::Strict`
    /// when no curated predicate matched).
    pub triples: Vec<RawTriple>,
}

/// Diagnostic log pairing each trace line with the triples it produced.
///
/// Use this when a system needs to answer "*which* trace line produced
/// *which* triples" — the KG alone can't, because it accumulates triples
/// without provenance back to the originating line. Push a [`Sighting`] after
/// each extraction call (line + the triples returned, possibly empty) to make
/// hallucinations and schema-talk extractions auditable; consumed by
/// [`crate::dump::write_pretty`] for the markdown sidecar alongside the
/// Turtle snapshot.
///
/// # Scope
///
/// `Global` — extraction is one ephemeral session per line, but the log must
/// accumulate across all of them, so a single shared accumulator outlives any
/// one session.
///
/// # Provided by
///
/// [`crate::ExtractionPlugin`] and
/// [`crate::plugins::oxigraph_extraction::OxigraphExtractionPlugin`] each
/// insert `Sightings::new()` in `build()`.
///
/// # Access
///
/// Consume as `Res<Sightings>`. `ResMut<T>` is not used for globals in this
/// crate; [`Sightings::push`] mutates through the interior [`RwLock`] and so
/// takes `&self`.
///
/// Mutation contract: **append-only and unbounded** — every [`Sightings::push`]
/// adds one record and nothing is ever removed, so memory grows with
/// extraction traffic (same profile as the in-memory [`crate::KnowledgeGraph`];
/// trim or roll if that becomes load-bearing).
///
/// # Alternatives
///
/// | Alternative | When |
/// | --- | --- |
/// | _none_ | This is the only line-to-triples provenance log in the crate. |
///
/// # Example system
///
/// ```no_run
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::Sightings;
///
/// /// Count trace lines that yielded no triples (candidate dropped lines).
/// #[system]
/// async fn count_empty(sightings: Res<Sightings>) {
///     let empty = sightings.snapshot().iter().filter(|s| s.triples.is_empty()).count();
///     tracing::info!(empty, "lines that extracted nothing");
/// }
/// ```
#[derive(Default, Debug)]
pub struct Sightings {
    inner: RwLock<Vec<Sighting>>,
}

impl Sightings {
    /// Construct an empty sightings log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `sighting` to the log.
    pub fn push(&self, sighting: Sighting) {
        self.inner.write().push(sighting);
    }

    /// Total number of sightings recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// `true` when no sightings have been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot every recorded sighting (clones the storage — use sparingly).
    #[must_use]
    pub fn snapshot(&self) -> Vec<Sighting> {
        self.inner.read().clone()
    }
}

impl GlobalResource for Sightings {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::kg::{Entity, Predicate};
    use chrono::Utc;

    fn raw(s: &str, p: &str, o: &str) -> RawTriple {
        RawTriple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            subject_type: None,
            object_type: None,
        }
    }

    #[test]
    fn append_and_snapshot() {
        let s = Sightings::new();
        assert!(s.is_empty());
        s.push(Sighting {
            line: "alice met bob".into(),
            observed_at: Utc::now(),
            trace_id: None,
            triples: vec![raw("alice", "met", "bob")],
        });
        s.push(Sighting {
            line: "noise line".into(),
            observed_at: Utc::now(),
            trace_id: Some("t-1".into()),
            triples: vec![],
        });
        assert_eq!(s.len(), 2);
        let snap = s.snapshot();
        assert_eq!(snap[0].triples.len(), 1);
        assert!(snap[1].triples.is_empty());
        assert_eq!(snap[1].trace_id.as_deref(), Some("t-1"));
    }
}
