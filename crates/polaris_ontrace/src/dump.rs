//! Serialize a [`KnowledgeGraph`] to RDF Turtle.
//!
//! Each observation is emitted as **two** things:
//!
//! 1. The bare `(S, P, O)` triple — for direct SPARQL queries on relationships.
//!    RDF semantics deduplicate, so repeated observations of the same triple
//!    collapse to one edge in the graph (intentional).
//! 2. An RDF-star annotation `<< s p o >> ont:observedAt "…" .` carrying
//!    the timestamp and optional `trace_id` — preserves the full temporal
//!    history. Default Turtle-star semantics is occurrence-based (the quoted
//!    triple is a *reference*, not an assertion), so the bare triple above
//!    is still required to assert the relationship.
//!
//! IRIs use a flat `urn:ontrace:` scheme; entity / predicate strings are
//! percent-encoded into a conservative safe set so arbitrary user input
//! (spaces, punctuation) round-trips cleanly. When the encoded local name
//! is a valid Turtle `PN_LOCAL` we emit the compact prefixed form
//! (`e:foo`, `p:met`); otherwise we fall back to the full angle-bracketed
//! IRI.
//!
//! Object positions whose name parses as a recognized scalar (integer,
//! decimal, or numeric-duration like `1.63s`) are rendered as RDF literals
//! rather than entity IRIs. This is dump-time cosmetics — the underlying
//! `KnowledgeGraph` still stores the value as an `Entity`. Scalar detection
//! at write time is not implemented.

use crate::iri::{RDF_TYPE, entity_iri_str, is_absolute_iri, percent_encode, predicate_iri_str};
use crate::resources::aliases::OWL_SAME_AS;
use crate::resources::kg::{Entity, KnowledgeGraph, Predicate, Triple};
use crate::resources::sightings::{Sighting, Sightings};
use crate::scalar::{self, ScalarKind};
use std::io::{self, Write};

const PROLOGUE: &str = "\
@prefix e:   <urn:ontrace:e/> .
@prefix p:   <urn:ontrace:p/> .
@prefix ont: <urn:ontrace:> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

";

/// Write the full KG as Turtle (with RDF-star annotations) to `out`.
///
/// The output is grouped: a `# Direct triples` block followed by a
/// `# Timed observations (RDF-star)` block. Suitable for loading into
/// oxigraph, Apache Jena, or any RDF 1.2 / Turtle-star aware store.
///
/// # Errors
///
/// Forwards any IO error from `out`.
pub fn write_turtle(kg: &KnowledgeGraph, out: &mut impl Write) -> io::Result<()> {
    let triples = kg.snapshot();
    out.write_all(PROLOGUE.as_bytes())?;

    out.write_all(b"# Direct triples\n")?;
    // Dedupe (S,P,O) for the direct section — the timed observations below
    // keep every individual sighting.
    let mut seen = std::collections::HashSet::new();
    for t in &triples {
        let key = (t.subject.clone(), t.predicate.clone(), t.object.clone());
        if seen.insert(key) {
            writeln!(
                out,
                "{} {} {} .",
                subject_term(&t.subject),
                predicate_term(&t.predicate),
                object_term(&t.object),
            )?;
        }
    }

    out.write_all(b"\n# Timed observations (RDF-star)\n")?;
    for t in &triples {
        write_observation(out, t)?;
    }

    Ok(())
}

fn write_observation(out: &mut impl Write, t: &Triple) -> io::Result<()> {
    write!(
        out,
        "<< {} {} {} >> ont:observedAt \"{}\"^^xsd:dateTime",
        subject_term(&t.subject),
        predicate_term(&t.predicate),
        object_term(&t.object),
        t.observed_at.to_rfc3339(),
    )?;
    if let Some(tid) = &t.trace_id {
        write!(out, " ; ont:traceId {}", quote_string(tid))?;
    }
    writeln!(out, " .")?;
    Ok(())
}

/// Subject position — always an entity (RDF 1.1 forbids literal subjects).
fn subject_term(e: &Entity) -> String {
    entity_term(e)
}

/// Object position — render as a typed literal when the name parses as a
/// recognized scalar, otherwise as an entity term.
fn object_term(e: &Entity) -> String {
    classify_literal(&e.0).unwrap_or_else(|| entity_term(e))
}

/// Render an entity as either the compact prefixed form (`e:foo`) or the
/// full angle-bracketed IRI, depending on whether the percent-encoded local
/// name is valid as a Turtle `PN_LOCAL`. Entities whose name is already a
/// scheme-qualified absolute IRI (e.g. a curated class IRI from the
/// type-emission path) are rendered as `<iri>` directly so they don't get
/// double-encoded into the `e:` namespace.
fn entity_term(e: &Entity) -> String {
    if is_absolute_iri(&e.0) {
        return format!("<{}>", entity_iri_str(e));
    }
    let local = percent_encode(&e.0);
    if is_pn_local_safe(&local) {
        format!("e:{local}")
    } else {
        format!("<{}>", entity_iri_str(e))
    }
}

/// Render a predicate as `p:foo`, a known prefixed sentinel (`owl:sameAs`,
/// `rdf:type`), or the full IRI form.
fn predicate_term(p: &Predicate) -> String {
    if p.0 == OWL_SAME_AS {
        // Already a prefixed name thanks to the prologue.
        OWL_SAME_AS.to_string()
    } else if p.0 == RDF_TYPE {
        RDF_TYPE.to_string()
    } else {
        let local = percent_encode(&p.0);
        if is_pn_local_safe(&local) {
            format!("p:{local}")
        } else {
            format!("<{}>", predicate_iri_str(p))
        }
    }
}

/// True when `s` is a valid Turtle `PN_LOCAL` under our percent-encoding.
///
/// `PN_LOCAL` allows letters, digits, `-`, `_`, `.` (not first/last), `:`,
/// and percent-escapes `%HH`. It disallows raw `/` and `~`, which our
/// encoder leaves unescaped — so those characters force the angle-bracketed
/// IRI form.
fn is_pn_local_safe(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    if chars[0] == '.' || chars[0] == '-' {
        return false;
    }
    if chars.last() == Some(&'.') {
        return false;
    }
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ':' {
            i += 1;
        } else if c == '%' {
            if i + 2 < chars.len()
                && chars[i + 1].is_ascii_hexdigit()
                && chars[i + 2].is_ascii_hexdigit()
            {
                i += 3;
            } else {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

/// Try to render `s` as a typed RDF literal. Returns `None` if it doesn't
/// look like a recognized scalar (see [`crate::scalar::classify`]).
fn classify_literal(s: &str) -> Option<String> {
    match scalar::classify(s)? {
        ScalarKind::Duration => Some(quote_string(s)),
        ScalarKind::Integer => Some(format!("\"{s}\"^^xsd:integer")),
        ScalarKind::Decimal => Some(format!("\"{s}\"^^xsd:decimal")),
    }
}

/// Quote a Turtle string literal: wrap in `"..."` and escape `\`, `"`, and
/// the standard control chars.
fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Pretty markdown report
// ─────────────────────────────────────────────────────────────────────────────

/// Write a markdown digest of [`Sightings`] to `out`.
///
/// Each block shows one extracted line — the raw input, the timestamp, the
/// optional `trace_id`, and a bulleted list of the triples the LLM
/// returned. Entity / predicate names are decoded for display (no
/// percent-encoding) and rendered as plain text rather than IRIs, since
/// the audience is humans inspecting extraction quality.
///
/// Sightings are grouped by `trace_id` when one is present (with `(no
/// trace)` last) so the file reads like a per-session activity log.
///
/// Companion to [`write_turtle`]: the same snapshot tick that produces
/// `<stamp>.ttl` for SPARQL also produces `<stamp>.md` for eyeballing.
///
/// # Errors
///
/// Forwards any IO error from `out`.
pub fn write_pretty(sightings: &Sightings, out: &mut impl Write) -> io::Result<()> {
    let mut groups: std::collections::BTreeMap<Option<String>, Vec<Sighting>> =
        std::collections::BTreeMap::new();
    for s in sightings.snapshot() {
        groups.entry(s.trace_id.clone()).or_default().push(s);
    }
    let total = groups.values().map(Vec::len).sum::<usize>();
    let extracted = groups
        .values()
        .flatten()
        .filter(|s| !s.triples.is_empty())
        .count();
    writeln!(out, "# Extraction report")?;
    writeln!(out)?;
    writeln!(out, "{total} line(s) seen, {extracted} produced triples.",)?;
    writeln!(out)?;

    let mut keys: Vec<_> = groups.keys().cloned().collect();
    // None last so `(no trace)` sinks to the bottom.
    keys.sort_by(|a, b| match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    });
    for key in keys {
        let label = key.clone().unwrap_or_else(|| "(no trace)".to_owned());
        writeln!(out, "## {label}")?;
        writeln!(out)?;
        let mut entries = groups.remove(&key).unwrap_or_default();
        entries.sort_by_key(|s| s.observed_at);
        for s in entries {
            write_sighting(out, &s)?;
        }
    }
    Ok(())
}

fn write_sighting(out: &mut impl Write, s: &Sighting) -> io::Result<()> {
    writeln!(out, "- **{}**", s.observed_at.to_rfc3339())?;
    writeln!(out, "  - input: `{}`", inline_code_escape(&s.line))?;
    if s.triples.is_empty() {
        writeln!(out, "  - _(no triples extracted)_")?;
    } else {
        writeln!(out, "  - triples:")?;
        for t in &s.triples {
            writeln!(
                out,
                "    - {} — **{}** → {}",
                pretty_entity(&t.subject),
                pretty_predicate(&t.predicate),
                pretty_object(&t.object),
            )?;
        }
    }
    Ok(())
}

fn pretty_entity(e: &Entity) -> String {
    e.0.clone()
}

fn pretty_predicate(p: &Predicate) -> String {
    p.0.clone()
}

/// Render the object for a markdown line, surfacing the literal annotation
/// when the value is a recognized scalar.
fn pretty_object(e: &Entity) -> String {
    match scalar::classify(&e.0) {
        Some(ScalarKind::Integer) => format!("`{}` (int)", e.0),
        Some(ScalarKind::Decimal) => format!("`{}` (decimal)", e.0),
        Some(ScalarKind::Duration) => format!("`{}` (duration)", e.0),
        None => e.0.clone(),
    }
}

/// Escape backticks in inline-code spans by switching to a longer fence
/// when the content itself contains backticks. Keeps the report readable
/// when trace lines embed shell snippets.
fn inline_code_escape(s: &str) -> String {
    // Collapse newlines so each input line stays on one row.
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.contains('`') {
        // Markdown allows `` ` `` for inline code by surrounding with `` `` ``.
        // We escape by replacing internal backticks with their HTML entity —
        // simpler than tracking fence length.
        flat.replace('`', "&#96;")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::extraction::RawTriple;
    use crate::resources::kg::{Entity, Predicate};
    use chrono::{TimeZone, Utc};

    fn fixed_triple(s: &str, p: &str, o: &str) -> Triple {
        Triple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 12, 9, 54, 4).unwrap(),
            trace_id: None,
        }
    }

    fn dump(kg: &KnowledgeGraph) -> String {
        let mut buf = Vec::new();
        write_turtle(kg, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn empty_kg_emits_prologue_only() {
        let kg = KnowledgeGraph::new();
        let out = dump(&kg);
        assert!(out.contains("@prefix e:"));
        assert!(out.contains("@prefix p:"));
        assert!(out.contains("@prefix ont:"));
        assert!(out.contains("# Direct triples"));
        assert!(out.contains("# Timed observations (RDF-star)"));
    }

    #[test]
    fn one_triple_uses_prefixed_names() {
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("alice", "met", "bob"));
        let out = dump(&kg);
        assert!(out.contains("e:alice p:met e:bob ."));
        assert!(out.contains("<< e:alice p:met e:bob >> ont:observedAt"));
        assert!(out.contains("\"2026-05-12T09:54:04+00:00\"^^xsd:dateTime"));
    }

    #[test]
    fn direct_section_dedupes_repeated_observations() {
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("alice", "met", "bob"));
        kg.insert(fixed_triple("alice", "met", "bob"));
        let out = dump(&kg);
        let direct = out.matches("e:alice p:met e:bob .").count();
        // Direct section keeps one; RDF-star annotation header keeps two.
        // Both sections contain "e:alice p:met e:bob" — the direct form is
        // followed by " .", the quoted form by " >>". Count only the bare
        // direct form by anchoring to the trailing " ." not followed by ">".
        assert_eq!(direct, 1, "direct triple deduped");

        let annotations = out.matches("<< e:alice p:met e:bob >>").count();
        assert_eq!(annotations, 2, "both observations kept as RDF-star");
    }

    #[test]
    fn entity_names_with_spaces_are_percent_encoded_and_prefixed() {
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("alice smith", "works at", "acme corp"));
        let out = dump(&kg);
        // %20 is valid in PN_LOCAL so prefixed names still work.
        assert!(out.contains("e:alice%20smith"));
        assert!(out.contains("p:works%20at"));
        assert!(out.contains("e:acme%20corp"));
    }

    #[test]
    fn entity_names_with_slashes_fall_back_to_full_iri() {
        // PN_LOCAL forbids raw `/`, so a path-like entity must use the
        // angle-bracketed form.
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("alice", "works_in", "/Users/alice/work"));
        let out = dump(&kg);
        assert!(out.contains("<urn:ontrace:e//Users/alice/work>"));
    }

    #[test]
    fn owl_same_as_uses_owl_namespace() {
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("Alice", OWL_SAME_AS, "alice"));
        let out = dump(&kg);
        assert!(out.contains("@prefix owl: <http://www.w3.org/2002/07/owl#>"));
        assert!(out.contains("e:Alice owl:sameAs e:alice ."));
        assert!(out.contains("<< e:Alice owl:sameAs e:alice >> ont:observedAt"));
    }

    #[test]
    fn trace_id_emitted_on_annotation() {
        let kg = KnowledgeGraph::new();
        let t = Triple {
            trace_id: Some("session_abc\"123".to_string()),
            ..fixed_triple("a", "b", "c")
        };
        kg.insert(t);
        let out = dump(&kg);
        assert!(out.contains("ont:traceId \"session_abc\\\"123\""));
    }

    #[test]
    fn numeric_duration_object_renders_as_string_literal() {
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("build_job", "has_latency", "1.63s"));
        let out = dump(&kg);
        assert!(out.contains("e:build_job p:has_latency \"1.63s\" ."));
        assert!(out.contains("<< e:build_job p:has_latency \"1.63s\" >>"));
    }

    #[test]
    fn integer_object_renders_as_xsd_integer() {
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("req_42", "has_token_usage", "1024"));
        let out = dump(&kg);
        assert!(out.contains("\"1024\"^^xsd:integer"));
    }

    #[test]
    fn decimal_object_renders_as_xsd_decimal() {
        let kg = KnowledgeGraph::new();
        kg.insert(fixed_triple("run_3", "has_score", "0.87"));
        let out = dump(&kg);
        assert!(out.contains("\"0.87\"^^xsd:decimal"));
    }

    #[test]
    fn pn_local_safety_classifications() {
        assert!(is_pn_local_safe("alice"));
        assert!(is_pn_local_safe("alice%20smith"));
        assert!(is_pn_local_safe("claude-opus-4-7"));
        assert!(is_pn_local_safe("a.b.c"));
        assert!(is_pn_local_safe("1foo"));
        assert!(!is_pn_local_safe(""));
        assert!(!is_pn_local_safe("/Users/x"));
        assert!(!is_pn_local_safe("a~b"));
        assert!(!is_pn_local_safe(".leading"));
        assert!(!is_pn_local_safe("trailing."));
        assert!(!is_pn_local_safe("-dash"));
        assert!(!is_pn_local_safe("a%2"));
    }

    fn raw_t(s: &str, p: &str, o: &str) -> RawTriple {
        RawTriple {
            subject: Entity::new(s),
            predicate: Predicate::new(p),
            object: Entity::new(o),
            subject_type: None,
            object_type: None,
        }
    }

    fn pretty(sightings: &Sightings) -> String {
        let mut buf = Vec::new();
        write_pretty(sightings, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn pretty_groups_by_trace_id_with_no_trace_last() {
        let s = Sightings::new();
        s.push(Sighting {
            line: "alice met bob".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            trace_id: Some("session-2".into()),
            triples: vec![raw_t("alice", "met", "bob")],
        });
        s.push(Sighting {
            line: "x talks to y".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 1).unwrap(),
            trace_id: None,
            triples: vec![],
        });
        s.push(Sighting {
            line: "p does q".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 2).unwrap(),
            trace_id: Some("session-1".into()),
            triples: vec![raw_t("p", "does", "q")],
        });
        let out = pretty(&s);
        let i_s1 = out.find("## session-1").expect("session-1 heading");
        let i_s2 = out.find("## session-2").expect("session-2 heading");
        let i_no = out.find("## (no trace)").expect("no-trace heading");
        assert!(i_s1 < i_s2, "session-1 before session-2");
        assert!(i_s2 < i_no, "(no trace) sinks last");
        assert!(out.contains("3 line(s) seen, 2 produced triples"));
    }

    #[test]
    fn pretty_shows_decoded_names_and_scalar_annotation() {
        let s = Sightings::new();
        s.push(Sighting {
            line: "build_job took 1.63s".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            trace_id: None,
            triples: vec![raw_t("build_job", "has_latency", "1.63s")],
        });
        let out = pretty(&s);
        assert!(out.contains("build_job — **has_latency** → `1.63s` (duration)"));
        assert!(!out.contains("urn:ontrace"));
    }

    #[test]
    fn pretty_empty_triples_show_no_extraction_note() {
        let s = Sightings::new();
        s.push(Sighting {
            line: "noise that did not extract".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            trace_id: None,
            triples: vec![],
        });
        let out = pretty(&s);
        assert!(out.contains("_(no triples extracted)_"));
    }

    #[test]
    fn pretty_escapes_backticks_in_input_line() {
        let s = Sightings::new();
        s.push(Sighting {
            line: "ran `cargo test` cleanly".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            trace_id: None,
            triples: vec![],
        });
        let out = pretty(&s);
        assert!(out.contains("&#96;cargo test&#96;"));
    }

    #[test]
    fn classify_literal_recognizes_scalars() {
        assert_eq!(classify_literal("42"), Some("\"42\"^^xsd:integer".into()));
        assert_eq!(classify_literal("-7"), Some("\"-7\"^^xsd:integer".into()));
        assert_eq!(
            classify_literal("3.14"),
            Some("\"3.14\"^^xsd:decimal".into())
        );
        assert_eq!(classify_literal("1.63s"), Some("\"1.63s\"".into()));
        assert_eq!(classify_literal("100ms"), Some("\"100ms\"".into()));
        assert_eq!(classify_literal("alice"), None);
        assert_eq!(classify_literal("v0.1.0"), None);
        assert_eq!(classify_literal("s"), None);
        assert_eq!(classify_literal(""), None);
    }
}
