//! Recognize when an extracted object string is actually a scalar value
//! (integer, decimal, numeric duration) rather than a graph entity.
//!
//! The LLM treats every `subject` / `predicate` / `object` as a string, so
//! literal values like `"1.63s"` or `"42"` round-trip as
//! `urn:ontrace:e/1.63s` entity IRIs. That's incorrect — they're values,
//! not entities — but the in-memory KG can't easily store literals
//! alongside entities. This module is the bridge: dump-time and write-time
//! callers ask "does this object look like a scalar?" and emit it as an
//! RDF literal when yes.
//!
//! The classification here is cosmetic. Datatypes are not checked when
//! triples are written.

/// What kind of scalar a string looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarKind {
    /// `-?\d+` — emit with `xsd:integer` datatype.
    Integer,
    /// `-?\d+\.\d+` — emit with `xsd:decimal` datatype.
    Decimal,
    /// `\d+(\.\d+)?` followed by a recognized time-unit suffix
    /// (`ms`, `us`, `ns`, `μs`, `s`, `m`, `h`). Emit as a plain string
    /// literal that preserves the unit verbatim — the
    /// `xsd:duration` lexical form (`P…`) is too ad-hoc to map onto
    /// real-world latency strings.
    Duration,
}

/// Classify `s` as a known scalar form, or return `None` to keep it as an
/// entity IRI.
pub(crate) fn classify(s: &str) -> Option<ScalarKind> {
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = strip_duration_suffix(s)
        && is_unsigned_number(rest)
    {
        return Some(ScalarKind::Duration);
    }
    let trimmed = s.strip_prefix('-').unwrap_or(s);
    if is_integer(trimmed) {
        return Some(ScalarKind::Integer);
    }
    if is_decimal(trimmed) {
        return Some(ScalarKind::Decimal);
    }
    None
}

fn strip_duration_suffix(s: &str) -> Option<&str> {
    for unit in ["ms", "us", "ns", "μs", "s", "m", "h"] {
        if let Some(rest) = s.strip_suffix(unit)
            && !rest.is_empty()
        {
            return Some(rest);
        }
    }
    None
}

fn is_integer(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_unsigned_number(s: &str) -> bool {
    is_integer(s) || is_decimal(s)
}

fn is_decimal(s: &str) -> bool {
    let Some(dot) = s.find('.') else {
        return false;
    };
    let (a, b) = s.split_at(dot);
    let b = &b[1..];
    !a.is_empty()
        && !b.is_empty()
        && a.bytes().all(|x| x.is_ascii_digit())
        && b.bytes().all(|x| x.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers() {
        assert_eq!(classify("42"), Some(ScalarKind::Integer));
        assert_eq!(classify("-7"), Some(ScalarKind::Integer));
        assert_eq!(classify("0"), Some(ScalarKind::Integer));
    }

    #[test]
    fn decimals() {
        assert_eq!(classify("3.14"), Some(ScalarKind::Decimal));
        assert_eq!(classify("-0.5"), Some(ScalarKind::Decimal));
    }

    #[test]
    fn durations() {
        assert_eq!(classify("1.63s"), Some(ScalarKind::Duration));
        assert_eq!(classify("100ms"), Some(ScalarKind::Duration));
        assert_eq!(classify("500us"), Some(ScalarKind::Duration));
        assert_eq!(classify("30ns"), Some(ScalarKind::Duration));
        assert_eq!(classify("2h"), Some(ScalarKind::Duration));
    }

    #[test]
    fn non_scalars_return_none() {
        assert_eq!(classify(""), None);
        assert_eq!(classify("alice"), None);
        assert_eq!(classify("v0.1.0"), None);
        assert_eq!(classify("s"), None);
        assert_eq!(classify("ms"), None);
        assert_eq!(classify("."), None);
        assert_eq!(classify("1."), None);
        assert_eq!(classify(".5"), None);
        assert_eq!(classify("12-34"), None);
    }
}
