//! Shared SPARQL string-construction helpers.
//!
//! Both the reified-trajectory writer ([`crate::agents::trajectory`]) and the
//! code-graph indexer ([`crate::tools::code_graph`]) build SPARQL update text by
//! interpolating values into literals and IRIs. Untrusted input (model-chosen
//! tool names, tool output, parsed identifiers) reaches both paths, so the two
//! escaping primitives live here in one place rather than being re-derived per
//! module:
//!
//! - [`esc`] — escape a value for a double-quoted SPARQL **literal**.
//! - [`enc`] — percent-encode a value for use as an **IRI** component.

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

/// Escape a string for embedding in a SPARQL double-quoted literal.
///
/// Escapes the backslash first (so subsequent escapes are not doubled) followed
/// by the double quote and the control characters that are otherwise illegal in
/// a quoted literal (`\t`, `\n`, `\r`).
#[must_use]
pub fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Percent-encode a string for safe use as a single IRI path component.
///
/// Encodes every non-alphanumeric byte, so interpolating attacker-controlled
/// values (a model-chosen tool name, a parsed symbol) cannot inject the `>`,
/// whitespace, or other characters that would close the IRI and smuggle extra
/// triples into the update.
#[must_use]
pub fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_handles_quote_backslash_and_control_chars() {
        assert_eq!(esc(r#"a"b"#), r#"a\"b"#);
        assert_eq!(esc(r"a\b"), r"a\\b");
        assert_eq!(esc("a\tb"), r"a\tb");
        assert_eq!(esc("a\nb"), r"a\nb");
        assert_eq!(esc("a\rb"), r"a\rb");
    }

    #[test]
    fn esc_escapes_backslash_before_other_sequences() {
        // A literal backslash-n must not be mistaken for an escaped newline.
        assert_eq!(esc("a\\nb"), r"a\\nb");
    }

    #[test]
    fn enc_neutralizes_iri_breaking_characters() {
        // A tool name carrying `>` must not be able to close the IRI.
        let encoded = enc("evil> <s> <p> <o> .#");
        assert!(!encoded.contains('>'));
        assert!(!encoded.contains(' '));
        assert!(!encoded.contains('#'));
    }

    #[test]
    fn enc_preserves_alphanumerics() {
        assert_eq!(enc("read_file"), "read%5Ffile");
        assert_eq!(enc("abc123"), "abc123");
    }
}
