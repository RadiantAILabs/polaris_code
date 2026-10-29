//! IRI minting helpers shared by the Turtle dumper and the oxigraph agent.
//!
//! Both code paths must produce **identical** IRIs for the same `Entity` /
//! `Predicate`, otherwise round-tripping a KG through the oxigraph store and
//! the in-memory dumper would silently diverge. Centralising the scheme here
//! is the only place that contract is enforced.
//!
//! The scheme is the same flat `urn:ontrace:` namespace documented in
//! [`crate::dump`]:
//!
//! - Entities → `urn:ontrace:e/<percent-encoded-name>`
//! - Predicates → `urn:ontrace:p/<percent-encoded-name>`, except the
//!   sentinel `owl:sameAs` predicate which maps to the canonical OWL IRI.

use crate::resources::aliases::OWL_SAME_AS;
use crate::resources::kg::{Entity, Predicate};

/// Canonical IRI for `owl:sameAs`.
pub(crate) const OWL_SAME_AS_IRI: &str = "http://www.w3.org/2002/07/owl#sameAs";

/// Local-name string used internally for `rdf:type` assertions on
/// [`crate::resources::kg::Predicate`] / [`crate::agents::extraction::RawTriple`].
/// Mapped to [`RDF_TYPE_IRI`] by [`predicate_iri_str`].
pub(crate) const RDF_TYPE: &str = "rdf:type";

/// Canonical IRI for `rdf:type`.
pub(crate) const RDF_TYPE_IRI: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Mint the entity IRI for `e` (no angle brackets).
///
/// When the entity's name is already a scheme-qualified absolute IRI (e.g. a
/// curated class IRI substituted in by the type-emission path), it is
/// returned unchanged — otherwise the double-encoded
/// `urn:ontrace:e/urn%3A...` form would land in the dataset.
pub(crate) fn entity_iri_str(e: &Entity) -> String {
    if is_absolute_iri(&e.0) {
        e.0.clone()
    } else {
        format!("urn:ontrace:e/{}", percent_encode(&e.0))
    }
}

/// Mint the predicate IRI for `p` (no angle brackets).
///
/// Two sentinel local names get expanded to canonical IRIs so the RDF is
/// reasoner-friendly: `owl:sameAs` (used by [`crate::resources::aliases`])
/// and `rdf:type` (used by the type-emission path in the write systems).
///
/// A predicate whose value is already a scheme-qualified absolute IRI (e.g.
/// `urn:ontrace:onto/agent/met`, produced by
/// [`crate::agents::extraction::bridge_predicate`] when a curated predicate
/// resolves to its `TBox` declaration) is returned unchanged.
/// Otherwise the local-name predicate is minted under the flat `urn:ontrace:p/`
/// namespace.
pub(crate) fn predicate_iri_str(p: &Predicate) -> String {
    if p.0 == OWL_SAME_AS {
        OWL_SAME_AS_IRI.to_string()
    } else if p.0 == RDF_TYPE {
        RDF_TYPE_IRI.to_string()
    } else if is_absolute_iri(&p.0) {
        p.0.clone()
    } else {
        format!("urn:ontrace:p/{}", percent_encode(&p.0))
    }
}

/// Lenient RFC 3986 scheme-prefix check.
///
/// Returns `true` when `s` starts with an `ALPHA *(ALPHA / DIGIT / "+" / "-" /
/// ".")` sequence followed by `:` — i.e. it looks like an already-qualified
/// absolute IRI. The check is intentionally shallow: it does not validate the
/// authority/path that follows the scheme, only the scheme prefix itself,
/// which is enough to discriminate LLM-emitted local names (no colon) from
/// pre-resolved IRIs the bridge feeds back in.
///
/// `owl:sameAs` would also satisfy this predicate, but `predicate_iri_str`
/// intercepts that sentinel before the call.
pub(crate) fn is_absolute_iri(s: &str) -> bool {
    let Some(idx) = s.find(':') else {
        return false;
    };
    if idx == 0 {
        return false;
    }
    let scheme = &s.as_bytes()[..idx];
    scheme[0].is_ascii_alphabetic()
        && scheme
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'+' | b'-' | b'.'))
}

/// Percent-encode anything outside a conservative unreserved set.
///
/// Per RFC 3986 §2.3 the unreserved set is `A-Z a-z 0-9 - . _ ~`. We
/// additionally keep `/` so callers can carry hierarchy in entity names if
/// they want.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_iri_uses_e_namespace() {
        assert_eq!(entity_iri_str(&Entity::new("alice")), "urn:ontrace:e/alice");
    }

    #[test]
    fn predicate_iri_uses_p_namespace() {
        assert_eq!(
            predicate_iri_str(&Predicate::new("works_at")),
            "urn:ontrace:p/works_at"
        );
    }

    #[test]
    fn owl_same_as_expands_to_full_iri() {
        assert_eq!(
            predicate_iri_str(&Predicate::new(OWL_SAME_AS)),
            OWL_SAME_AS_IRI
        );
    }

    #[test]
    fn absolute_iri_predicate_passes_through() {
        let bridged = "urn:ontrace:onto/agent/met";
        assert_eq!(predicate_iri_str(&Predicate::new(bridged)), bridged);
    }

    #[test]
    fn http_iri_predicate_passes_through() {
        let iri = "http://example.org/onto#works_at";
        assert_eq!(predicate_iri_str(&Predicate::new(iri)), iri);
    }

    #[test]
    fn rdf_type_expands_to_full_iri() {
        assert_eq!(predicate_iri_str(&Predicate::new(RDF_TYPE)), RDF_TYPE_IRI,);
    }

    #[test]
    fn absolute_iri_entity_passes_through() {
        let iri = "urn:ontrace:onto/agent/Agent";
        assert_eq!(entity_iri_str(&Entity::new(iri)), iri);
    }

    #[test]
    fn special_chars_are_percent_encoded() {
        assert_eq!(percent_encode("alice smith"), "alice%20smith");
        assert_eq!(percent_encode("ACME, Inc."), "ACME%2C%20Inc.");
        assert_eq!(percent_encode("foo/bar"), "foo/bar");
    }
}
