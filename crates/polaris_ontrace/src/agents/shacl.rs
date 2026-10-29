//! SHACL-style post-validation: drop extracted triples that violate the
//! curated `TBox`'s shape constraints before they reach the store.
//!
//! Sits between `extract_triples` and `repair_triples` in the
//! [`OxigraphExtractionAgent`](super::oxigraph_extraction::OxigraphExtractionAgent)
//! graph. The LLM's JSON-schema enum only constrains the *predicate name* —
//! it can't express "the object of `has_metric` is a `Metric`, not a bare
//! number". This module closes that gap with a narrow, in-process validator.
//!
//! # Single source of truth
//!
//! Per-predicate constraints come from [`OntologyContext::predicate_shapes`],
//! the same [`PredicateShape`] map that drives the prompt. `rdfs:domain` /
//! `rdfs:range` and SHACL property shapes (`sh:class`, `sh:datatype`,
//! `sh:nodeKind`) are both folded in by the loaders ahead of time, so the
//! validator and the prompt agree by construction — no separate cache to
//! drift out of sync.
//!
//! # What it checks
//!
//! [`check`] looks up the predicate in `predicate_shapes` and applies, in
//! order:
//!
//! - **`node_kind`.** `Iri` rejects scalar-shaped objects; `Literal`
//!   rejects entity-shaped ones.
//! - **`range_datatype`.** The object's lexical form must classify (see
//!   [`crate::scalar`]) as that datatype family — an `xsd:integer` range
//!   rejects `1.63s`.
//! - **`range_class`.** A class-ranged predicate expects an entity; a
//!   scalar-shaped object is rejected.
//! - **`domain_class`.** When the batch carries `rdf:type` evidence for
//!   the subject (LLM-supplied or shape-inferred) and that evidence is
//!   disjoint from the declared domain, the triple is rejected.
//!
//! # On violation
//!
//! [`validate_triples`] returns a [`ValidationOutcome`] carrying both the
//! kept triples and the per-triple violations. The downstream
//! `repair_triples` system uses the violations to drive a re-extraction
//! pass before anything reaches the store. Triples that fail the repair
//! pass are recorded into [`OntologyGap`] there, not here — this keeps
//! the validator a pure function of inputs.

use std::collections::{HashMap, HashSet};

use polaris::prelude::Out;
use polaris::system::param::Res;
use polaris::system::resource::LocalResource;
use polaris::system::system;

use crate::agents::extraction::{
    ClassHierarchy, NodeKind, OntologyContext, PredicateShape, RawTriple,
};
use crate::scalar::{self, ScalarKind};

/// Triples that survived the full validate-then-repair pipeline. Carried
/// as a distinct newtype so the Polaris graph wires
/// `extract → validate → repair → write_to_oxigraph` purely by type
/// signature — `repair_triples` produces this, `write_to_oxigraph`
/// consumes `Out<ValidatedTriples>`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ValidatedTriples(pub(crate) Vec<RawTriple>);

impl LocalResource for ValidatedTriples {}

/// Output of [`validate_triples`] — the kept triples plus the violations
/// the validator flagged. Consumed by `repair_triples`, which either
/// attempts an LLM repair on each violation or records it as a gap when
/// no salvage is possible.
#[derive(Debug, Clone, Default)]
pub(crate) struct ValidationOutcome {
    /// Triples that passed every applicable shape check.
    pub(crate) kept: Vec<RawTriple>,
    /// Triples that failed, with the reason for inspection downstream.
    pub(crate) violations: Vec<(RawTriple, ViolationReason)>,
}

impl LocalResource for ValidationOutcome {}

/// Why a triple failed validation. Carried in the [`ValidationOutcome`]
/// and rendered into the repair prompt by `repair_triples`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ViolationReason {
    /// The predicate's `range_class` is a curated class, but the object's
    /// lexical form classifies as a scalar value. A class-ranged predicate
    /// expects an entity, not a number / duration.
    RangeClassButObjectIsScalar {
        /// Local-name of the declared range class.
        range_class: String,
        /// How the object classified (see [`crate::scalar`]).
        scalar_kind: ScalarKind,
    },
    /// The predicate's `range_datatype` is an `xsd:` datatype, but the
    /// object's lexical form doesn't classify as that family.
    DatatypeMismatch {
        /// Local-name of the expected `xsd:` datatype.
        expected: String,
        /// How the object classified, if at all.
        got: Option<ScalarKind>,
    },
    /// SHACL `sh:nodeKind` violation — the object is the wrong kind of node.
    NodeKindMismatch {
        /// The node kind the shape requires.
        expected: NodeKind,
        /// The node kind the object's lexical form implies.
        got: NodeKind,
    },
    /// The predicate's `domain_class` requires a subject of `expected`, but
    /// every recorded `rdf:type` for this subject in the current batch is
    /// disjoint from it (modulo `rdfs:subClassOf*` inference). Vacuous when
    /// the subject carries no recorded type.
    DomainMismatch {
        /// Local-name of the declared domain class.
        expected: String,
        /// Local-names of the subject's recorded types in this batch.
        got: Vec<String>,
    },
    /// The predicate's `range_class` requires an object of `expected`, but
    /// every recorded `rdf:type` for the (IRI) object in the current batch
    /// is disjoint from it (modulo `rdfs:subClassOf*` inference). Vacuous
    /// when the object carries no recorded type or is a literal (the
    /// scalar-vs-class case is reported as
    /// [`Self::RangeClassButObjectIsScalar`] instead).
    RangeMismatch {
        /// Local-name of the declared range class.
        expected: String,
        /// Local-names of the object's recorded types in this batch.
        got: Vec<String>,
    },
}

impl ViolationReason {
    /// Render the violation as a short human-readable phrase for the
    /// repair prompt and the [`OntologyGap`] record. Stable across
    /// versions — `repair_triples` includes it verbatim in the LLM
    /// prompt, so changes here affect prompt behavior.
    pub(crate) fn as_hint(&self) -> String {
        match self {
            Self::RangeClassButObjectIsScalar {
                range_class,
                scalar_kind,
            } => format!(
                "expected the object to be a `{range_class}` entity, got a {scalar_kind:?} value"
            ),
            Self::DatatypeMismatch { expected, got } => match got {
                Some(k) => format!("expected an xsd:{expected} literal, got {k:?}"),
                None => format!(
                    "expected an xsd:{expected} literal; the object isn't a recognised scalar"
                ),
            },
            Self::NodeKindMismatch { expected, got } => {
                format!("expected the object to be {expected:?}, got {got:?}")
            }
            Self::DomainMismatch { expected, got } => {
                format!("subject is typed as {got:?}, but the predicate's domain is `{expected}`")
            }
            Self::RangeMismatch { expected, got } => {
                format!("object is typed as {got:?}, but the predicate's range is `{expected}`")
            }
        }
    }
}

/// Validate a single triple against the per-session [`PredicateShape`]s.
///
/// Returns `None` when the triple is valid (or its predicate carries no
/// constraints), or `Some(reason)` describing the first violation found.
///
/// Pure and store-free — all shape data lives in `shapes`, the
/// `rdfs:subClassOf*` index in `hierarchy`, both populated by the agent's
/// `setup` from the `TBox` once per session. Checks run in order:
/// `node_kind`, then `range_datatype`, then `range_class`, then
/// `domain_class`.
///
/// Class membership against `domain_class` / `range_class` is checked
/// via [`ClassHierarchy::is_a`], so an abstract class (e.g. `core:Event`)
/// admits every subclass-typed subject or object — matching standard
/// SHACL semantics for `sh:targetClass` and `sh:class`.
#[must_use]
pub(crate) fn check(
    triple: &RawTriple,
    shapes: &HashMap<String, PredicateShape>,
    hierarchy: &ClassHierarchy,
    subject_types: Option<&HashSet<String>>,
    object_types: Option<&HashSet<String>>,
) -> Option<ViolationReason> {
    let shape = shapes.get(&triple.predicate.0)?;
    let object_scalar = scalar::classify(&triple.object.0);

    if let Some(expected) = shape.node_kind {
        let got = match object_scalar {
            Some(_) => NodeKind::Literal,
            None => NodeKind::Iri,
        };
        if got != expected {
            return Some(ViolationReason::NodeKindMismatch { expected, got });
        }
    }

    if let Some(datatype) = &shape.range_datatype
        && !datatype_admits(datatype, object_scalar)
    {
        return Some(ViolationReason::DatatypeMismatch {
            expected: datatype.clone(),
            got: object_scalar,
        });
    }

    if let Some(range_class) = &shape.range_class {
        if let Some(scalar_kind) = object_scalar {
            return Some(ViolationReason::RangeClassButObjectIsScalar {
                range_class: range_class.clone(),
                scalar_kind,
            });
        }
        // Object is an IRI — when the batch carries `rdf:type` evidence for
        // it (LLM-supplied or shape-inferred), every type must match the
        // declared range class via the subclass hierarchy. Vacuous when no
        // type is recorded for the object.
        if let Some(types) = object_types
            && !types.is_empty()
            && !types.iter().any(|t| hierarchy.is_a(t, range_class))
        {
            let mut got: Vec<String> = types.iter().cloned().collect();
            got.sort();
            return Some(ViolationReason::RangeMismatch {
                expected: range_class.clone(),
                got,
            });
        }
    }

    // Domain check is vacuous when the subject carries no recorded type.
    // The first pass in `validate_triples` collects every `rdf:type` triple
    // in the batch into the types map; predicates whose subjects carry
    // no type evidence pass this branch unchanged.
    if let Some(expected) = &shape.domain_class
        && let Some(types) = subject_types
        && !types.is_empty()
        && !types.iter().any(|t| hierarchy.is_a(t, expected))
    {
        let mut got: Vec<String> = types.iter().cloned().collect();
        got.sort();
        return Some(ViolationReason::DomainMismatch {
            expected: expected.clone(),
            got,
        });
    }

    None
}

/// Whether an `xsd:` datatype local-name admits an object that classified
/// as `scalar`.
///
/// Numeric and duration families require a matching [`ScalarKind`].
/// String-like and temporal datatypes (`xsd:string`, `xsd:dateTime`, …) and
/// any unrecognized datatype are permissive — the [`crate::scalar`]
/// classifier doesn't model them, so constraining against it would produce
/// false positives.
fn datatype_admits(xsd_local: &str, scalar: Option<ScalarKind>) -> bool {
    match xsd_local {
        "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "negativeInteger" | "nonPositiveInteger" | "unsignedInt"
        | "unsignedLong" => matches!(scalar, Some(ScalarKind::Integer)),
        "decimal" | "double" | "float" => {
            matches!(
                scalar,
                Some(ScalarKind::Integer) | Some(ScalarKind::Decimal)
            )
        }
        "duration" | "dayTimeDuration" => matches!(scalar, Some(ScalarKind::Duration)),
        _ => true,
    }
}

/// Partition extracted triples into kept + violations, against the
/// per-session [`OntologyContext::predicate_shapes`].
///
/// Runs between `extract_triples` and `repair_triples` in the
/// [`OxigraphExtractionAgent`](super::oxigraph_extraction::OxigraphExtractionAgent)
/// graph. Each per-triple decision is independent; the only cross-triple
/// state is the `rdf:type` evidence collected up front (LLM-supplied
/// `subject_type` / `object_type` plus any inline `rdf:type` triples in
/// the batch), which powers the `domain_class` check.
///
/// When the curated ontology declares no shapes the predicate-shapes map
/// is empty and every triple passes — free-form extraction.
///
/// Gap recording (`OntologyGap::record_violation`) happens downstream in
/// `repair_triples` so only violations the repair pass *fails* to fix
/// are recorded as ontology signal. A repaired triple is no longer a
/// gap — it was bad LLM output, not a missing constraint.
#[system]
pub(crate) async fn validate_triples(
    triples: Out<Vec<RawTriple>>,
    ontology: Res<OntologyContext>,
) -> ValidationOutcome {
    let shapes = &ontology.predicate_shapes;
    let hierarchy = &ontology.class_hierarchy;
    let entity_types = collect_entity_types(triples.iter());

    let mut kept = Vec::with_capacity(triples.len());
    let mut violations = Vec::new();
    for triple in triples.iter() {
        let subject_types = entity_types.get(&triple.subject.0);
        let object_types = entity_types.get(&triple.object.0);
        if let Some(reason) = check(triple, shapes, hierarchy, subject_types, object_types) {
            tracing::debug!(
                subject = %triple.subject.0,
                predicate = %triple.predicate.0,
                object = %triple.object.0,
                ?reason,
                "shacl: triple violates TBox shape, deferring to repair pass",
            );
            violations.push((triple.clone(), reason));
        } else {
            kept.push(triple.clone());
        }
    }
    if !violations.is_empty() {
        tracing::info!(
            kept = kept.len(),
            violating = violations.len(),
            "shacl: validated batch",
        );
    }
    ValidationOutcome { kept, violations }
}

/// Walk `triples` and collect `entity local-name -> {class local-names}`
/// from any `rdf:type` assertions present in the batch (both LLM-supplied
/// via the `subject_type` / `object_type` fields and raw `rdf:type` triples
/// passed through directly). Both subject and object positions are
/// indexed, so [`check`] can look up either side via the same map.
///
/// Class local-names registered through the `class_iris` bridge land here
/// as their full IRI; the local-name is stripped so the comparison in
/// [`check`] matches `domain_class` / `range_class` (also local-names).
fn collect_entity_types<'a, I>(triples: I) -> HashMap<String, HashSet<String>>
where
    I: IntoIterator<Item = &'a RawTriple>,
{
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    let mut record = |subject: &str, class: &str| {
        out.entry(subject.to_owned())
            .or_default()
            .insert(type_local_name(class));
    };
    for t in triples {
        if t.predicate.0 == crate::iri::RDF_TYPE {
            record(&t.subject.0, &t.object.0);
        }
        if let Some(class) = &t.subject_type {
            record(&t.subject.0, class);
        }
        if let Some(class) = &t.object_type {
            record(&t.object.0, class);
        }
    }
    out
}

/// Reduce a class name to its bare local-name so the [`check`] comparison
/// against `domain_class` (also a local-name) matches. Pass through
/// anything without a `#` or `/` separator unchanged.
fn type_local_name(class: &str) -> String {
    let cut = class
        .rfind('#')
        .or_else(|| class.rfind('/'))
        .map_or(0, |i| i + 1);
    class[cut..].to_owned()
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

    fn shapes_with(predicate: &str, shape: PredicateShape) -> HashMap<String, PredicateShape> {
        let mut by = HashMap::new();
        by.insert(predicate.to_owned(), shape);
        by
    }

    /// Thin wrapper that defaults the hierarchy and object-types arguments
    /// so the bulk of the existing tests (which only exercise the
    /// scalar / datatype / nodeKind / subject-type axes) stay readable.
    /// Hierarchy-aware tests call [`check`] directly.
    fn check_with_subject_types(
        triple: &RawTriple,
        shapes: &HashMap<String, PredicateShape>,
        subject_types: Option<&HashSet<String>>,
    ) -> Option<ViolationReason> {
        let hierarchy = ClassHierarchy::default();
        check(triple, shapes, &hierarchy, subject_types, None)
    }

    #[test]
    fn unconstrained_predicate_passes() {
        let shapes = shapes_with("met", PredicateShape::default());
        assert_eq!(
            check_with_subject_types(&raw("alice", "met", "bob"), &shapes, None),
            None
        );
    }

    #[test]
    fn predicate_absent_from_index_passes() {
        let shapes: HashMap<String, PredicateShape> = HashMap::new();
        assert_eq!(
            check_with_subject_types(&raw("alice", "invented", "bob"), &shapes, None),
            None,
        );
    }

    #[test]
    fn class_range_rejects_scalar_object() {
        let shapes = shapes_with(
            "has_metric",
            PredicateShape {
                range_class: Some("Metric".to_owned()),
                ..Default::default()
            },
        );
        let reason =
            check_with_subject_types(&raw("eval_run", "has_metric", "0.92"), &shapes, None);
        assert_eq!(
            reason,
            Some(ViolationReason::RangeClassButObjectIsScalar {
                range_class: "Metric".to_owned(),
                scalar_kind: ScalarKind::Decimal,
            }),
        );
    }

    #[test]
    fn class_range_accepts_entity_object() {
        let shapes = shapes_with(
            "has_metric",
            PredicateShape {
                range_class: Some("Metric".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            check_with_subject_types(&raw("eval_run", "has_metric", "accuracy"), &shapes, None),
            None,
        );
    }

    #[test]
    fn integer_datatype_rejects_duration() {
        let shapes = shapes_with(
            "token_count",
            PredicateShape {
                range_datatype: Some("integer".to_owned()),
                ..Default::default()
            },
        );
        let reason =
            check_with_subject_types(&raw("eval_run", "token_count", "1.63s"), &shapes, None);
        assert_eq!(
            reason,
            Some(ViolationReason::DatatypeMismatch {
                expected: "integer".to_owned(),
                got: Some(ScalarKind::Duration),
            }),
        );
    }

    #[test]
    fn integer_datatype_rejects_non_scalar() {
        let shapes = shapes_with(
            "token_count",
            PredicateShape {
                range_datatype: Some("integer".to_owned()),
                ..Default::default()
            },
        );
        let reason =
            check_with_subject_types(&raw("eval_run", "token_count", "lots"), &shapes, None);
        assert_eq!(
            reason,
            Some(ViolationReason::DatatypeMismatch {
                expected: "integer".to_owned(),
                got: None,
            }),
        );
    }

    #[test]
    fn integer_datatype_accepts_integer() {
        let shapes = shapes_with(
            "token_count",
            PredicateShape {
                range_datatype: Some("integer".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            check_with_subject_types(&raw("eval_run", "token_count", "1024"), &shapes, None),
            None,
        );
    }

    #[test]
    fn decimal_datatype_accepts_integer_and_decimal() {
        let shapes = shapes_with(
            "score",
            PredicateShape {
                range_datatype: Some("decimal".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            check_with_subject_types(&raw("run", "score", "1"), &shapes, None),
            None
        );
        assert_eq!(
            check_with_subject_types(&raw("run", "score", "0.5"), &shapes, None),
            None
        );
    }

    #[test]
    fn unmodeled_datatype_is_permissive() {
        let shapes = shapes_with(
            "started_at",
            PredicateShape {
                range_datatype: Some("dateTime".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            check_with_subject_types(
                &raw("run", "started_at", "2026-05-14t12:00:00"),
                &shapes,
                None
            ),
            None,
        );
    }

    #[test]
    fn node_kind_iri_rejects_scalar() {
        let shapes = shapes_with(
            "produced",
            PredicateShape {
                node_kind: Some(NodeKind::Iri),
                ..Default::default()
            },
        );
        let reason = check_with_subject_types(&raw("agent", "produced", "42"), &shapes, None);
        assert_eq!(
            reason,
            Some(ViolationReason::NodeKindMismatch {
                expected: NodeKind::Iri,
                got: NodeKind::Literal,
            }),
        );
    }

    #[test]
    fn node_kind_literal_rejects_entity() {
        let shapes = shapes_with(
            "has_value",
            PredicateShape {
                node_kind: Some(NodeKind::Literal),
                ..Default::default()
            },
        );
        let reason = check_with_subject_types(&raw("metric", "has_value", "high"), &shapes, None);
        assert_eq!(
            reason,
            Some(ViolationReason::NodeKindMismatch {
                expected: NodeKind::Literal,
                got: NodeKind::Iri,
            }),
        );
    }

    #[test]
    fn node_kind_iri_accepts_entity() {
        let shapes = shapes_with(
            "produced",
            PredicateShape {
                node_kind: Some(NodeKind::Iri),
                ..Default::default()
            },
        );
        assert_eq!(
            check_with_subject_types(&raw("agent", "produced", "report"), &shapes, None),
            None
        );
    }

    fn types_of(values: &[&str]) -> HashSet<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn domain_check_passes_when_subject_has_matching_type() {
        let shapes = shapes_with(
            "has_metric",
            PredicateShape {
                domain_class: Some("EvalRun".to_owned()),
                ..Default::default()
            },
        );
        let types = types_of(&["EvalRun"]);
        assert_eq!(
            check_with_subject_types(
                &raw("run42", "has_metric", "accuracy"),
                &shapes,
                Some(&types),
            ),
            None,
        );
    }

    #[test]
    fn domain_check_rejects_when_subject_types_are_disjoint() {
        let shapes = shapes_with(
            "has_metric",
            PredicateShape {
                domain_class: Some("EvalRun".to_owned()),
                ..Default::default()
            },
        );
        let types = types_of(&["Agent"]);
        let reason = check_with_subject_types(
            &raw("run42", "has_metric", "accuracy"),
            &shapes,
            Some(&types),
        );
        assert_eq!(
            reason,
            Some(ViolationReason::DomainMismatch {
                expected: "EvalRun".to_owned(),
                got: vec!["Agent".to_owned()],
            }),
        );
    }

    #[test]
    fn domain_check_is_vacuous_when_subject_has_no_recorded_type() {
        let shapes = shapes_with(
            "has_metric",
            PredicateShape {
                domain_class: Some("EvalRun".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            check_with_subject_types(&raw("run42", "has_metric", "accuracy"), &shapes, None),
            None,
        );
        let empty = types_of(&[]);
        assert_eq!(
            check_with_subject_types(
                &raw("run42", "has_metric", "accuracy"),
                &shapes,
                Some(&empty),
            ),
            None,
        );
    }

    #[test]
    fn collect_entity_types_reads_inline_type_fields_and_rdf_type_triples() {
        let with_type = RawTriple {
            subject: Entity::new("alice"),
            predicate: Predicate::new("met"),
            object: Entity::new("bob"),
            subject_type: Some("Person".to_owned()),
            object_type: Some("Person".to_owned()),
        };
        let explicit_type = RawTriple {
            subject: Entity::new("alice"),
            predicate: Predicate::new(crate::iri::RDF_TYPE),
            object: Entity::new("urn:ontrace:onto/agent/Agent"),
            subject_type: None,
            object_type: None,
        };
        let map = collect_entity_types([&with_type, &explicit_type]);
        let alice = map.get("alice").unwrap();
        assert!(alice.contains("Person"), "got: {alice:?}");
        assert!(alice.contains("Agent"), "got: {alice:?}");
        let bob = map.get("bob").unwrap();
        assert!(bob.contains("Person"), "got: {bob:?}");
    }

    #[test]
    fn violation_hint_is_human_readable() {
        let r = ViolationReason::DatatypeMismatch {
            expected: "integer".to_owned(),
            got: Some(ScalarKind::Decimal),
        };
        let s = r.as_hint();
        assert!(s.contains("xsd:integer"), "hint: {s}");
        assert!(s.contains("Decimal"), "hint: {s}");
    }

    fn hierarchy_with(edges: &[(&str, &str)]) -> ClassHierarchy {
        let mut h = ClassHierarchy::new();
        for (sub, sup) in edges {
            h.add_subclass((*sub).to_owned(), (*sup).to_owned());
        }
        h
    }

    #[test]
    fn domain_check_accepts_subclass_typed_subject_via_hierarchy() {
        // `has_metric` requires the subject to be an `Event`; the LLM gave
        // it as `ToolCall`, which is a subclass of `Event`. Without the
        // hierarchy walk this would falsely reject; with it, it passes.
        let shapes = shapes_with(
            "has_metric",
            PredicateShape {
                domain_class: Some("Event".to_owned()),
                ..Default::default()
            },
        );
        let hierarchy = hierarchy_with(&[("ToolCall", "Event")]);
        let types = types_of(&["ToolCall"]);
        assert_eq!(
            check(
                &raw("tool_call_1", "has_metric", "accuracy"),
                &shapes,
                &hierarchy,
                Some(&types),
                None,
            ),
            None,
        );
    }

    #[test]
    fn domain_check_accepts_transitively_subclass_typed_subject() {
        // Span → Event → Actor. A predicate requiring `Actor` admits
        // every transitive subclass.
        let shapes = shapes_with(
            "performed",
            PredicateShape {
                domain_class: Some("Actor".to_owned()),
                ..Default::default()
            },
        );
        let hierarchy = hierarchy_with(&[("Span", "Event"), ("Event", "Actor")]);
        let types = types_of(&["Span"]);
        assert_eq!(
            check(
                &raw("span_1", "performed", "lookup"),
                &shapes,
                &hierarchy,
                Some(&types),
                None,
            ),
            None,
        );
    }

    #[test]
    fn domain_check_rejects_sibling_class_in_same_hierarchy() {
        // ToolCall and Measurement both descend from Event, but Measurement
        // is not a subclass of Actor. A predicate with `domain_class: Actor`
        // must reject a Measurement-typed subject.
        let shapes = shapes_with(
            "performed",
            PredicateShape {
                domain_class: Some("Actor".to_owned()),
                ..Default::default()
            },
        );
        let hierarchy = hierarchy_with(&[
            ("ToolCall", "Event"),
            ("Measurement", "Event"),
            ("ToolCall", "Actor"),
        ]);
        let types = types_of(&["Measurement"]);
        let reason = check(
            &raw("m1", "performed", "lookup"),
            &shapes,
            &hierarchy,
            Some(&types),
            None,
        );
        assert_eq!(
            reason,
            Some(ViolationReason::DomainMismatch {
                expected: "Actor".to_owned(),
                got: vec!["Measurement".to_owned()],
            }),
        );
    }

    #[test]
    fn range_check_accepts_subclass_typed_object_iri() {
        // `produced` declares `range_class: Artifact`; the object is typed
        // as `Commit`, a subclass. Range check via hierarchy must accept.
        let shapes = shapes_with(
            "produced",
            PredicateShape {
                range_class: Some("Artifact".to_owned()),
                ..Default::default()
            },
        );
        let hierarchy = hierarchy_with(&[("Commit", "Artifact")]);
        let object_types = types_of(&["Commit"]);
        assert_eq!(
            check(
                &raw("alice", "produced", "abc123"),
                &shapes,
                &hierarchy,
                None,
                Some(&object_types),
            ),
            None,
        );
    }

    #[test]
    fn range_check_rejects_object_iri_typed_outside_range_subtree() {
        // `produced` declares `range_class: Artifact`; the object is typed
        // as `Agent`, which is not a subclass of `Artifact`. Reject.
        let shapes = shapes_with(
            "produced",
            PredicateShape {
                range_class: Some("Artifact".to_owned()),
                ..Default::default()
            },
        );
        let hierarchy = hierarchy_with(&[("Commit", "Artifact"), ("Agent", "Actor")]);
        let object_types = types_of(&["Agent"]);
        let reason = check(
            &raw("alice", "produced", "bob"),
            &shapes,
            &hierarchy,
            None,
            Some(&object_types),
        );
        assert_eq!(
            reason,
            Some(ViolationReason::RangeMismatch {
                expected: "Artifact".to_owned(),
                got: vec!["Agent".to_owned()],
            }),
        );
    }

    #[test]
    fn range_check_is_vacuous_when_object_iri_has_no_recorded_type() {
        // Without type evidence for the object, the range check must
        // remain a pass-through — type-less observation still gets
        // through (open-world behaviour).
        let shapes = shapes_with(
            "produced",
            PredicateShape {
                range_class: Some("Artifact".to_owned()),
                ..Default::default()
            },
        );
        let hierarchy = ClassHierarchy::default();
        assert_eq!(
            check(
                &raw("alice", "produced", "report"),
                &shapes,
                &hierarchy,
                None,
                None,
            ),
            None,
        );
    }

    #[test]
    fn range_check_still_flags_scalar_when_class_is_declared() {
        // Even with the hierarchy-aware range check, a scalar object
        // against a class-ranged predicate is still surfaced as
        // RangeClassButObjectIsScalar — the scalar branch wins.
        let shapes = shapes_with(
            "produced",
            PredicateShape {
                range_class: Some("Artifact".to_owned()),
                ..Default::default()
            },
        );
        let hierarchy = ClassHierarchy::default();
        let reason = check(
            &raw("alice", "produced", "1.63"),
            &shapes,
            &hierarchy,
            None,
            None,
        );
        assert!(
            matches!(
                reason,
                Some(ViolationReason::RangeClassButObjectIsScalar { .. })
            ),
            "got: {reason:?}",
        );
    }

    #[test]
    fn range_mismatch_hint_is_human_readable() {
        let r = ViolationReason::RangeMismatch {
            expected: "Artifact".to_owned(),
            got: vec!["Agent".to_owned()],
        };
        let s = r.as_hint();
        assert!(s.contains("Artifact"), "hint: {s}");
        assert!(s.contains("Agent"), "hint: {s}");
    }
}
