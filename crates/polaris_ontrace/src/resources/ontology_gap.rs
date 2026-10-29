//! Accumulated signal about where the curated ontology is wrong or
//! incomplete — the feedback channel that closes the loop between generated
//! KGs and ontology iteration.
//!
//! The extraction pipeline already *generates* this signal and throws it
//! away: catch-all sentinel uses, predicates the LLM coins outside the
//! curated set, and triples dropped by SHACL post-validation. Each of those
//! is evidence that the `TBox` doesn't yet match the traces being ingested.
//! [`OntologyGap`] is where that evidence lands so a human reviewer can rank it
//! and propose `TBox` changes.
//!
//! Held as a [`GlobalResource`] so a single accumulator survives across the
//! per-trace ephemeral sessions — identical lifecycle and concurrency story
//! to [`crate::resources::kg::KnowledgeGraph`] and
//! [`crate::resources::sightings::Sightings`]. Interior mutability is a
//! [`RwLock`]; recording is synchronous (no `.await` under the guard), so
//! concurrent extraction sessions can all record into it safely.
//!
//! Append-only and unbounded in `count`; per-key example lists are capped at
//! [`EXAMPLE_CAP`] so memory stays bounded even under heavy traffic.

use crate::agents::extraction::{OntologyContext, OntologyPolicy, RawTriple};
use parking_lot::RwLock;
use polaris::system::resource::GlobalResource;
use std::collections::HashMap;

/// Maximum number of example trace lines retained per gap key. The `count`
/// keeps growing past this; only the stored examples are capped.
pub const EXAMPLE_CAP: usize = 8;

/// Frequency plus a capped sample of trace lines for one gap key.
#[derive(Default, Debug, Clone)]
pub struct GapStat {
    /// Total times this gap was observed — unbounded.
    pub count: usize,
    /// Up to [`EXAMPLE_CAP`] example detail strings (trace lines, or
    /// `subject predicate object — reason` for SHACL violations).
    pub examples: Vec<String>,
}

impl GapStat {
    /// Bump `count` and retain `example` if the sample list isn't full yet.
    fn record(&mut self, example: &str) {
        self.count += 1;
        if self.examples.len() < EXAMPLE_CAP {
            self.examples.push(example.to_owned());
        }
    }
}

/// A point-in-time clone of every recorded gap, returned by
/// [`OntologyGap::snapshot`]. Consumed by inspection tooling.
#[derive(Default, Debug, Clone)]
pub struct GapReport {
    /// Predicates the LLM emitted that aren't in the curated set, keyed by
    /// predicate local-name. A frequent entry is a candidate new `TBox`
    /// property.
    pub uncurated_predicates: HashMap<String, GapStat>,
    /// Uses of the [`OntologyPolicy::CatchAll`] sentinel — a relation the
    /// LLM saw but no curated predicate fit. The example lines show what
    /// kinds of relations keep falling through.
    pub catchall_uses: GapStat,
    /// Triples dropped by SHACL post-validation, keyed by predicate
    /// local-name. A frequent entry suggests the predicate's declared
    /// `rdfs:domain` / `rdfs:range` (or SHACL shape) is too tight or wrong.
    pub shacl_violations: HashMap<String, GapStat>,
}

/// Cross-session accumulator of ontology-improvement signal.
///
/// Use this when a write path discovers evidence that the curated `TBox`
/// doesn't match the traces being ingested: a predicate the LLM coined
/// outside the curated set ([`OntologyGap::record_uncurated_predicate`]), a
/// use of the catch-all sentinel ([`OntologyGap::record_catchall`]), or a
/// triple dropped by SHACL validation ([`OntologyGap::record_violation`]).
/// Read [`OntologyGap::snapshot`] to rank the gaps and propose `TBox` changes
/// — this is the feedback channel that closes the loop between generated KGs
/// and ontology iteration.
///
/// # Scope
///
/// `Global` — the signal must accumulate across every per-trace ephemeral
/// session so a reviewer sees the aggregate, not one
/// session's slice.
///
/// # Provided by
///
/// [`crate::ExtractionPlugin`] and
/// [`crate::plugins::oxigraph_extraction::OxigraphExtractionPlugin`] each
/// insert `OntologyGap::new()` in `build()`. (Only the oxigraph backend
/// records SHACL violations, since the naive backend has no validation step.)
///
/// # Access
///
/// Consume as `Res<OntologyGap>`. `ResMut<T>` is not used for globals in this
/// crate; every recording method mutates through the interior [`RwLock`] and
/// so takes `&self`. Each takes the write lock briefly and releases it before
/// returning — never hold a reference across an `.await` (the `#[system]`
/// call sites don't).
///
/// Mutation contract: **append-only**, with `count` unbounded but per-key
/// example lists capped at [`EXAMPLE_CAP`], so memory stays bounded even under
/// heavy traffic.
///
/// # Alternatives
///
/// | Alternative | When |
/// | --- | --- |
/// | _none_ | This is the only ontology-gap accumulator in the crate. |
///
/// # Example system
///
/// ```no_run
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::OntologyGap;
///
/// /// Surface the most frequent uncurated predicate as a `TBox` candidate.
/// #[system]
/// async fn top_gap(gap: Res<OntologyGap>) {
///     let report = gap.snapshot();
///     if let Some((pred, stat)) = report.uncurated_predicates.iter().max_by_key(|(_, s)| s.count) {
///         tracing::info!(predicate = %pred, count = stat.count, "candidate TBox property");
///     }
/// }
/// ```
#[derive(Default, Debug)]
pub struct OntologyGap {
    inner: RwLock<GapReport>,
}

impl OntologyGap {
    /// Construct an empty gap accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the LLM emitted `predicate`, which isn't in the curated
    /// set, while extracting `trace_line`.
    pub fn record_uncurated_predicate(&self, predicate: &str, trace_line: &str) {
        self.inner
            .write()
            .uncurated_predicates
            .entry(predicate.to_owned())
            .or_default()
            .record(trace_line);
    }

    /// Record one use of the catch-all sentinel predicate while extracting
    /// `trace_line` — a relation that fit no curated predicate.
    pub fn record_catchall(&self, trace_line: &str) {
        self.inner.write().catchall_uses.record(trace_line);
    }

    /// Record that a triple using `predicate` was dropped by SHACL
    /// post-validation. `detail` should carry the offending triple and the
    /// violation reason.
    pub fn record_violation(&self, predicate: &str, detail: &str) {
        self.inner
            .write()
            .shacl_violations
            .entry(predicate.to_owned())
            .or_default()
            .record(detail);
    }

    /// `true` when nothing has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let g = self.inner.read();
        g.uncurated_predicates.is_empty()
            && g.shacl_violations.is_empty()
            && g.catchall_uses.count == 0
    }

    /// Clone the full set of recorded gaps for inspection or reporting.
    #[must_use]
    pub fn snapshot(&self) -> GapReport {
        self.inner.read().clone()
    }
}

impl GlobalResource for OntologyGap {}

/// Record any predicate-level gap a single extracted triple reveals, given
/// the session's [`OntologyContext`].
///
/// Shared by both write paths (`write_to_kg` and `write_to_oxigraph`) so the
/// catch-all / uncurated-predicate classification stays identical across
/// backends. In free-form mode (no curated ontology) every predicate would
/// trivially be "uncurated", which is noise rather than signal — so the
/// function is a no-op when [`OntologyContext::is_empty`].
///
/// SHACL violations are *not* recorded here — they're caught upstream in
/// `validate_triples`, before this triple would ever reach a write path.
pub(crate) fn record_predicate_gaps(
    gap: &OntologyGap,
    ontology: &OntologyContext,
    raw: &RawTriple,
    trace_line: &str,
) {
    if ontology.is_empty() {
        return;
    }
    let predicate = &raw.predicate.0;
    if let OntologyPolicy::CatchAll {
        catchall_predicate, ..
    } = &ontology.policy
        && predicate == catchall_predicate
    {
        gap.record_catchall(trace_line);
        return;
    }
    let curated = ontology.predicate_iris.contains_key(predicate)
        || ontology.predicates.iter().any(|p| p == predicate);
    if !curated {
        gap.record_uncurated_predicate(predicate, trace_line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::kg::{Entity, Predicate};

    fn raw(s: &str, p: &str, o: &str) -> RawTriple {
        RawTriple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            subject_type: None,
            object_type: None,
        }
    }

    fn curated(predicates: &[&str], policy: OntologyPolicy) -> OntologyContext {
        OntologyContext {
            predicates: predicates.iter().map(|s| (*s).to_owned()).collect(),
            classes: Vec::new(),
            summary: "Curated ontology:\n".to_owned(),
            policy,
            predicate_iris: HashMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_until_recorded() {
        let gap = OntologyGap::new();
        assert!(gap.is_empty());
        gap.record_catchall("alice waved at bob");
        assert!(!gap.is_empty());
    }

    #[test]
    fn examples_are_capped_but_count_is_not() {
        let gap = OntologyGap::new();
        for i in 0..(EXAMPLE_CAP + 5) {
            gap.record_uncurated_predicate("deployed_to", &format!("line {i}"));
        }
        let report = gap.snapshot();
        let stat = report.uncurated_predicates.get("deployed_to").unwrap();
        assert_eq!(stat.count, EXAMPLE_CAP + 5);
        assert_eq!(stat.examples.len(), EXAMPLE_CAP);
    }

    #[test]
    fn record_predicate_gaps_noop_in_free_form_mode() {
        let gap = OntologyGap::new();
        record_predicate_gaps(
            &gap,
            &OntologyContext::default(),
            &raw("alice", "met", "bob"),
            "alice met bob",
        );
        assert!(gap.is_empty());
    }

    #[test]
    fn record_predicate_gaps_flags_uncurated_predicate() {
        let gap = OntologyGap::new();
        let ontology = curated(&["met"], OntologyPolicy::Strict);
        record_predicate_gaps(
            &gap,
            &ontology,
            &raw("svc", "deployed_to", "prod"),
            "svc deployed to prod",
        );
        let report = gap.snapshot();
        assert_eq!(
            report
                .uncurated_predicates
                .get("deployed_to")
                .unwrap()
                .count,
            1
        );
        assert_eq!(report.catchall_uses.count, 0);
    }

    #[test]
    fn record_predicate_gaps_flags_catchall_use() {
        let gap = OntologyGap::new();
        let ontology = curated(
            &["met", "related_to"],
            OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".to_owned(),
                catchall_iri: None,
            },
        );
        record_predicate_gaps(
            &gap,
            &ontology,
            &raw("alice", "related_to", "the report"),
            "alice mentioned the report",
        );
        let report = gap.snapshot();
        assert_eq!(report.catchall_uses.count, 1);
        assert!(report.uncurated_predicates.is_empty());
    }

    #[test]
    fn record_predicate_gaps_ignores_curated_predicate() {
        let gap = OntologyGap::new();
        let ontology = curated(&["met"], OntologyPolicy::Strict);
        record_predicate_gaps(
            &gap,
            &ontology,
            &raw("alice", "met", "bob"),
            "alice met bob",
        );
        assert!(gap.is_empty());
    }
}
