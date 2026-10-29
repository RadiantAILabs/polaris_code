//! Validation-failure-driven re-extraction pass.
//!
//! Sits between [`super::shacl::validate_triples`] and
//! [`super::oxigraph_extraction::write_to_oxigraph`]. Closes the loop the
//! validator opened: a triple rejected for violating a `PredicateShape` is
//! sent back to the LLM with the shape it failed and the curated
//! predicate list, asking for a corrected re-emission or a drop.
//!
//! # Why a separate system
//!
//! Polaris wires the graph by type signature, and a repair pass has very
//! different inputs from validation: it needs the model handle, the
//! ontology summary, the trace line text, and an outbound `OntologyGap`
//! handle. Keeping it out of [`super::shacl::validate_triples`] lets the
//! validator stay pure and synchronously testable — every LLM-touching
//! concern lives in this module.
//!
//! # Repair contract
//!
//! - One LLM call per line, only when the validator flagged at least one
//!   violation. No-violation batches short-circuit straight to
//!   [`super::shacl::ValidatedTriples`].
//! - The LLM sees only the rejected triples and is asked to either fix or
//!   drop each one. It must not re-emit triples that already passed —
//!   those are folded back in unchanged after the repair.
//! - Repair candidates run through the same
//!   [`super::extraction::normalize_extracted`] filters as fresh extractions
//!   (IRI-shaped entities, degenerate s/p/o, non-curated predicates under
//!   `Strict`) and are then re-validated against the same
//!   [`super::extraction::OntologyContext::predicate_shapes`].
//! - Violations whose `(subject, object)` pair never reappears in the
//!   accepted repair set are recorded into [`OntologyGap`] — a real
//!   ontology-level signal that the LLM couldn't fit the observation to
//!   the curated shape.
//!
//! # Non-goals
//!
//! No retry loop. One round only; repair failures land in the gap. The
//! repair prompt is deliberately narrow: it doesn't try to teach the LLM
//! arbitrary new rules, it just lists the shapes that failed and trusts
//! the curated-predicate list to carry the rest of the context.

use std::collections::{HashMap, HashSet};

use polaris::models::llm::{Llm, LlmRequest, Message};
use polaris::prelude::Out;
use polaris::system::param::Res;
use polaris::system::system;

use crate::agents::extraction::{
    AgentConfig, AgentLlm, ExtractedTriples, OntologyContext, RawTriple, normalize_extracted,
};
use crate::agents::shacl::{ValidatedTriples, ValidationOutcome, ViolationReason, check};
use crate::resources::ontology_gap::OntologyGap;
use crate::resources::spend::Spend;
use crate::resources::trace_line::TraceLine;

/// Run the SHACL-driven repair pass.
///
/// The pipeline contract is `validate → repair → write`. When the
/// validator produced no violations this system is a passthrough — the
/// kept set lands in [`ValidatedTriples`] directly without an LLM call.
///
/// When there are violations the LLM is called once with the trace line,
/// the rejected triples + reasons, and the curated ontology summary. The
/// response is run through the same post-parse filters as a fresh
/// extraction and then re-validated against the curated shapes. Repairs
/// that survive join the kept set; original violations not addressed by a
/// successful repair are recorded as [`OntologyGap`] entries before the
/// system returns.
#[system]
pub(crate) async fn repair_triples(
    outcome: Out<ValidationOutcome>,
    ontology: Res<OntologyContext>,
    line: Res<TraceLine>,
    llm: Res<AgentLlm>,
    config: Res<AgentConfig>,
    spend: Res<Spend>,
    gap: Res<OntologyGap>,
) -> ValidatedTriples {
    let ValidationOutcome { kept, violations } = (*outcome).clone();
    if violations.is_empty() {
        return ValidatedTriples(kept);
    }

    let prompt = build_repair_prompt(&line.text, &violations, &ontology);
    let output_schema = serde_json::to_value(schemars::schema_for!(ExtractedTriples))
        .expect("ExtractedTriples schema serializes to JSON");
    let request = LlmRequest {
        system: Some(prompt),
        messages: vec![Message::user(repair_user_message(&line.text, &violations))],
        tools: None,
        tool_choice: None,
        output_schema: Some(output_schema),
        ..Default::default()
    };

    let candidates = match call_repair(&llm, request, &config, &spend).await {
        Some(c) => c,
        None => {
            // LLM call failed or returned unparseable JSON. Treat every
            // violation as unfixable.
            record_unfixed_violations(&gap, &line.text, violations.iter());
            return ValidatedTriples(kept);
        }
    };
    let repaired = apply_repairs(&kept, &candidates, &ontology);
    let fixed_keys: HashSet<(String, String)> = repaired
        .iter()
        .map(|t| (t.subject.0.clone(), t.object.0.clone()))
        .collect();
    let unfixed = violations.iter().filter(|(rejected, _)| {
        !fixed_keys.contains(&(rejected.subject.0.clone(), rejected.object.0.clone()))
    });
    record_unfixed_violations(&gap, &line.text, unfixed);

    let mut out = kept;
    out.extend(repaired);
    tracing::info!(
        violations = violations.len(),
        repaired = fixed_keys.len(),
        kept = out.len(),
        "shacl repair complete",
    );
    ValidatedTriples(out)
}

/// Call the LLM with the repair request, record token usage on `spend`,
/// and parse the response into an [`ExtractedTriples`].
///
/// Returns `None` on any failure (network error, empty body, JSON parse
/// error). Failures are logged at WARN — they're rare but should be
/// visible since the caller treats them as "no repair candidates",
/// which means every violation falls through to the gap.
async fn call_repair(
    llm: &Llm,
    request: LlmRequest,
    config: &AgentConfig,
    spend: &Spend,
) -> Option<ExtractedTriples> {
    let response = match llm.generate(request).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "repair LLM call failed");
            return None;
        }
    };
    spend.record(
        &config.model_id,
        response.usage.input_tokens,
        response.usage.output_tokens,
    );
    let text = response.text();
    if text.is_empty() {
        return None;
    }
    match serde_json::from_str::<ExtractedTriples>(&text) {
        Ok(v) => Some(v),
        Err(err) => {
            tracing::warn!(error = %err, body = %text, "repair parse failed");
            None
        }
    }
}

/// Filter every repair candidate through [`normalize_extracted`] and
/// [`check`], using the kept-batch's type evidence to drive the domain
/// check. Returns the candidates that survived both passes.
///
/// Type evidence is collected from the union of `kept` and the
/// normalised repair candidates — this lets a repair that emits a new
/// `rdf:type` triple satisfy a `domain_class` check on a different
/// candidate in the same response.
fn apply_repairs(
    kept: &[RawTriple],
    candidates: &ExtractedTriples,
    ontology: &OntologyContext,
) -> Vec<RawTriple> {
    let normalised: Vec<RawTriple> = candidates
        .triples
        .iter()
        .filter_map(|t| {
            normalize_extracted(
                crate::agents::extraction::ExtractedTriple {
                    subject: t.subject.clone(),
                    predicate: t.predicate.clone(),
                    object: t.object.clone(),
                    subject_type: t.subject_type.clone(),
                    object_type: t.object_type.clone(),
                },
                ontology,
            )
        })
        .collect();
    if normalised.is_empty() {
        return Vec::new();
    }
    let entity_types = collect_entity_types(kept.iter().chain(normalised.iter()));
    normalised
        .into_iter()
        .filter(|raw| {
            let subject_types = entity_types.get(&raw.subject.0);
            let object_types = entity_types.get(&raw.object.0);
            match check(
                raw,
                &ontology.predicate_shapes,
                &ontology.class_hierarchy,
                subject_types,
                object_types,
            ) {
                None => true,
                Some(reason) => {
                    tracing::debug!(
                        subject = %raw.subject.0,
                        predicate = %raw.predicate.0,
                        object = %raw.object.0,
                        ?reason,
                        "repair candidate still violates shape — dropping",
                    );
                    false
                }
            }
        })
        .collect()
}

/// Record every violation in `violations` as an [`OntologyGap`] entry.
/// The detail includes the trace line so a reviewer can see why the
/// LLM picked the wrong predicate.
fn record_unfixed_violations<'a, I>(gap: &OntologyGap, trace_line: &str, violations: I)
where
    I: IntoIterator<Item = &'a (RawTriple, ViolationReason)>,
{
    for (triple, reason) in violations {
        gap.record_violation(
            &triple.predicate.0,
            &format!(
                "{} {} {} — {} (line: {trace_line})",
                triple.subject.0,
                triple.predicate.0,
                triple.object.0,
                reason.as_hint(),
            ),
        );
    }
}

/// Walk `triples` and collect `entity local-name -> {class local-names}`
/// from any `rdf:type` assertions (both LLM-supplied via
/// `subject_type` / `object_type` fields and raw `rdf:type` triples). Both
/// subject and object positions are indexed, mirroring the helper in
/// [`crate::agents::shacl`]; duplicated here to keep `shacl` free of any
/// repair-pass coupling.
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

/// Reduce a class name to its bare local-name for matching against
/// `domain_class` (also a local-name). Pass-through when no separator.
fn type_local_name(class: &str) -> String {
    let cut = class
        .rfind('#')
        .or_else(|| class.rfind('/'))
        .map_or(0, |i| i + 1);
    class[cut..].to_owned()
}

/// System prompt for the repair call. Carries:
///
/// - The instruction shape (re-emit only the rejected triples).
/// - The curated ontology summary, so the model has the same shape
///   guidance the original extraction had.
///
/// The trace line and rejected list live in the user message
/// ([`repair_user_message`]) so they don't get cached with the system
/// prompt across distinct lines.
pub(crate) fn build_repair_prompt(
    _line: &str,
    _violations: &[(RawTriple, ViolationReason)],
    ontology: &OntologyContext,
) -> String {
    let mut s = String::from(
        "You correct subject-predicate-object triples that violated ontology shape \
         constraints during a prior extraction pass.\n\n\
         For EACH rejected triple in the user message:\n\
         - Either re-emit it with a corrected predicate from the curated list whose \
           Domain → Range shape fits the subject and object, OR\n\
         - Drop it entirely by omitting it from your response.\n\n\
         Rules:\n\
         - Use lowercase canonical entity names; preserve the subject and object you \
           were given unless that's the cause of the violation.\n\
         - The corrected predicate MUST be one of the curated predicates listed below.\n\
         - Do NOT invent triples that weren't in the rejected list.\n\
         - Do NOT re-emit triples that aren't in the rejected list — those already passed.\n\
         - Return an empty `triples` array if no rejection can be salvaged.\n",
    );
    if !ontology.summary.is_empty() {
        s.push('\n');
        s.push_str(&ontology.summary);
    }
    s
}

/// Render the rejected triples + reasons as the user message for the
/// repair call.
pub(crate) fn repair_user_message(
    line: &str,
    violations: &[(RawTriple, ViolationReason)],
) -> String {
    use std::fmt::Write;
    let mut s = format!("Original line:\n{line}\n\nRejected triples:\n");
    for (t, r) in violations {
        let _ = writeln!(
            s,
            "- ({subject}, {predicate}, {object}) — {hint}",
            subject = t.subject.0,
            predicate = t.predicate.0,
            object = t.object.0,
            hint = r.as_hint(),
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::extraction::{OntologyPolicy, PredicateShape};
    use crate::resources::kg::{Entity, Predicate};
    use crate::scalar::ScalarKind;

    fn raw(s: &str, p: &str, o: &str) -> RawTriple {
        RawTriple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            subject_type: None,
            object_type: None,
        }
    }

    fn ontology() -> OntologyContext {
        let mut shapes = HashMap::new();
        shapes.insert(
            "has_metric".to_owned(),
            PredicateShape {
                range_class: Some("Metric".to_owned()),
                ..Default::default()
            },
        );
        OntologyContext {
            predicates: vec!["has_metric".into(), "has_score".into()],
            predicate_shapes: shapes,
            policy: OntologyPolicy::CatchAll {
                catchall_predicate: "related_to".into(),
                catchall_iri: None,
            },
            ..Default::default()
        }
    }

    #[test]
    fn repair_prompt_includes_curated_summary() {
        let mut o = ontology();
        o.summary = "Curated ontology:\n- predicates: has_metric (EvalRun → Metric)".into();
        let prompt = build_repair_prompt("ignored", &[], &o);
        assert!(prompt.contains("rejected"));
        assert!(prompt.contains("Curated ontology"));
        assert!(prompt.contains("has_metric"));
    }

    #[test]
    fn repair_user_message_lists_violations_with_hints() {
        let v = [(
            raw("run42", "has_metric", "0.92"),
            ViolationReason::RangeClassButObjectIsScalar {
                range_class: "Metric".into(),
                scalar_kind: ScalarKind::Decimal,
            },
        )];
        let msg = repair_user_message("eval run 42 hit 0.92 accuracy", &v);
        assert!(msg.contains("Original line"));
        assert!(msg.contains("run42"));
        assert!(msg.contains("0.92"));
        assert!(msg.contains("Metric"));
    }

    #[test]
    fn apply_repairs_keeps_candidate_that_now_passes() {
        let o = ontology();
        let candidates = ExtractedTriples {
            triples: vec![crate::agents::extraction::ExtractedTriple {
                subject: "run42".into(),
                predicate: "has_score".into(),
                object: "0.92".into(),
                subject_type: None,
                object_type: None,
            }],
        };
        let repaired = apply_repairs(&[], &candidates, &o);
        assert_eq!(repaired.len(), 1);
        assert_eq!(repaired[0].predicate.0, "has_score");
    }

    #[test]
    fn apply_repairs_drops_candidate_that_still_violates() {
        // Candidate keeps using has_metric with a scalar object — same
        // shape failure as the original. Must drop.
        let o = ontology();
        let candidates = ExtractedTriples {
            triples: vec![crate::agents::extraction::ExtractedTriple {
                subject: "run42".into(),
                predicate: "has_metric".into(),
                object: "0.92".into(),
                subject_type: None,
                object_type: None,
            }],
        };
        let repaired = apply_repairs(&[], &candidates, &o);
        assert!(repaired.is_empty());
    }

    #[test]
    fn apply_repairs_drops_candidates_with_non_curated_predicate_under_strict() {
        let mut o = ontology();
        o.policy = OntologyPolicy::Strict;
        let candidates = ExtractedTriples {
            triples: vec![crate::agents::extraction::ExtractedTriple {
                subject: "run42".into(),
                predicate: "invented_by_llm".into(),
                object: "accuracy".into(),
                subject_type: None,
                object_type: None,
            }],
        };
        let repaired = apply_repairs(&[], &candidates, &o);
        assert!(repaired.is_empty());
    }

    #[test]
    fn record_unfixed_violations_writes_to_gap() {
        let gap = OntologyGap::new();
        let v = [(
            raw("run42", "has_metric", "0.92"),
            ViolationReason::RangeClassButObjectIsScalar {
                range_class: "Metric".into(),
                scalar_kind: ScalarKind::Decimal,
            },
        )];
        record_unfixed_violations(&gap, "trace line text", v.iter());
        let snapshot = gap.snapshot();
        let stat = snapshot
            .shacl_violations
            .get("has_metric")
            .expect("predicate-keyed violation");
        assert_eq!(stat.count, 1);
        assert!(stat.examples[0].contains("Metric"));
        assert!(stat.examples[0].contains("trace line text"));
    }
}
