//! Drift guard between the coding agent's guidance text and the languages the
//! code index actually covers.
//!
//! The agent's `SYSTEM_PROMPT` in `src/agents/code_agent.rs` tells the model
//! which files `outline`/`read_symbol` can map, and that curated concept
//! queries describe Rust/Polaris roles only. Both claims are true exactly while
//! they agree with `src/tools/code_graph.rs`: the language set reported by
//! `SourceLanguage::id`, and the `language "rust"` scope every enrichment rule
//! carries. Prompt and indexer are private, so this guard reads the two sources
//! and compares them. Adding a language to the indexer without updating the
//! guidance, claiming a language the indexer does not index, or unscoping an
//! enrichment rule while the guidance still promises Rust-only roles all fail
//! here.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

/// Source holding the agent's guidance text.
const AGENT_SOURCE: &str = "src/agents/code_agent.rs";
/// Source holding the indexer's language dispatch and enrichment rules.
const INDEXER_SOURCE: &str = "src/tools/code_graph.rs";
/// Marker introducing the guidance sentence that enumerates language coverage.
const COVERAGE_PREFIX: &str = "The code index covers ";
/// Marker introducing the indexer's language-id mapping.
const LANGUAGE_ID_FN: &str = "fn id(self) -> &'static str {";
/// Start of every enrichment rule helper, anchored at column zero.
const RULE_PREFIX: &str = "\nfn insert_";
/// The triple pattern that keeps an enrichment rule Rust-only, as written in
/// the SPARQL format strings.
const RUST_SCOPE: &str = r#"<{NS}language> \"rust\""#;

#[test]
fn guidance_names_exactly_the_indexed_languages() {
    let claimed = claimed_languages(&system_prompt());
    let indexed = indexed_languages();

    assert_eq!(
        claimed, indexed,
        "the agent guidance in {AGENT_SOURCE} claims the code index covers {claimed:?}, but \
         `SourceLanguage::id` in {INDEXER_SOURCE} reports {indexed:?}.\n\
         the implementation wins: update the \"{COVERAGE_PREFIX}...\" sentence so it names \
         exactly the languages the index covers."
    );
}

#[test]
fn guidance_keeps_the_concept_queries_rust_only() {
    let indexer = read_source(INDEXER_SOURCE);
    let rules = enrichment_rules(&indexer);
    assert!(
        !rules.is_empty(),
        "no `fn insert_...` enrichment rule found in {INDEXER_SOURCE}; this guard no longer \
         reads the rules it is supposed to check"
    );

    let unscoped: Vec<&str> = rules
        .iter()
        .filter(|(_, body)| !body.contains(RUST_SCOPE))
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        unscoped.is_empty(),
        "enrichment rules {unscoped:?} in {INDEXER_SOURCE} no longer carry the `{RUST_SCOPE}` \
         scope, so curated roles can now be derived from non-Rust syntax.\n\
         either restore the scope or update the agent guidance in {AGENT_SOURCE}, which \
         currently promises the concept queries describe Rust/Polaris roles only."
    );

    let guidance = system_prompt();
    assert!(
        guidance.contains("Rust/Polaris"),
        "every enrichment rule in {INDEXER_SOURCE} is scoped to Rust, but the agent guidance \
         in {AGENT_SOURCE} no longer says the concept queries describe Rust/Polaris roles \
         only; the model will read role gaps in another indexed language as real absences"
    );
}

/// The agent's guidance text, read out of its private `SYSTEM_PROMPT` const.
fn system_prompt() -> String {
    let source = read_source(AGENT_SOURCE);
    let declaration = "const SYSTEM_PROMPT: &str = ";
    let at = source
        .find(declaration)
        .unwrap_or_else(|| panic!("no `{declaration}` in {AGENT_SOURCE}"));
    let open = at
        + declaration.len()
        + source[at + declaration.len()..]
            .find('"')
            .unwrap_or_else(|| panic!("`SYSTEM_PROMPT` in {AGENT_SOURCE} is not a string literal"));
    string_literal(&source, open).0
}

/// The languages the guidance claims the index covers, lowercased so they
/// compare against the indexer's own language ids.
fn claimed_languages(guidance: &str) -> BTreeSet<String> {
    let at = guidance
        .find(COVERAGE_PREFIX)
        .unwrap_or_else(|| panic!("no \"{COVERAGE_PREFIX}...\" sentence in the agent guidance"));
    let rest = &guidance[at + COVERAGE_PREFIX.len()..];
    let end = rest.find('.').unwrap_or_else(|| {
        panic!("the \"{COVERAGE_PREFIX}...\" sentence in the agent guidance never ends")
    });
    rest[..end]
        .replace(" and ", ",")
        .split(',')
        .map(|item| {
            item.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        })
        .filter(|item| !item.is_empty())
        .collect()
}

/// The language ids the indexer reports, one per indexed language. Grammar
/// variants that share an id (TSX under `typescript`) collapse into it, so this
/// is the set the guidance should describe.
fn indexed_languages() -> BTreeSet<String> {
    let source = read_source(INDEXER_SOURCE);
    string_literals(block_after(&source, LANGUAGE_ID_FN))
}

/// Every enrichment rule helper as `(name, body)`.
fn enrichment_rules(source: &str) -> Vec<(String, String)> {
    let mut rules = Vec::new();
    let mut from = 0;
    while let Some(hit) = source[from..].find(RULE_PREFIX) {
        let at = from + hit + 1;
        let rest = &source[at..];
        let signature_end = rest.find('{').unwrap_or_else(|| {
            panic!("enrichment rule at byte {at} of {INDEXER_SOURCE} has no body")
        });
        let name_end = rest.find('(').unwrap_or_else(|| {
            panic!("enrichment rule at byte {at} of {INDEXER_SOURCE} has no parameter list")
        });
        let name = rest["fn ".len()..name_end].to_string();
        let open = at + signature_end;
        let body = block_from(source, open);
        from = open + 1 + body.len();
        rules.push((name, body.to_string()));
    }
    rules
}

/// Contents of the block opened by the brace that ends `marker`.
fn block_after<'a>(source: &'a str, marker: &str) -> &'a str {
    let at = source
        .find(marker)
        .unwrap_or_else(|| panic!("no `{marker}` in {INDEXER_SOURCE}"));
    block_from(source, at + marker.len() - 1)
}

/// Contents of the block whose opening brace is at `open`.
///
/// String literals are skipped so SPARQL format strings, which are full of
/// `{{`/`}}`, do not unbalance the count. Braces inside comments or character
/// literals would, and no scanned body contains either.
fn block_from(source: &str, open: usize) -> &str {
    let bytes = source.as_bytes();
    assert_eq!(bytes[open], b'{', "expected a block to open at byte {open}");
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = string_literal(source, i).1;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[open + 1..i];
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced braces from byte {open}");
}

/// Every string literal in `source`, lowercased and deduplicated.
fn string_literals(source: &str) -> BTreeSet<String> {
    let bytes = source.as_bytes();
    let mut found = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let (literal, next) = string_literal(source, i);
            found.insert(literal.to_lowercase());
            i = next;
        } else {
            i += 1;
        }
    }
    found
}

/// The contents of the string literal opening at `open`, and the byte index
/// just past its closing quote. Escapes are left exactly as written.
fn string_literal(source: &str, open: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    assert_eq!(
        bytes[open], b'"',
        "expected a string literal to open at byte {open}"
    );
    let start = open + 1;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return (source[start..i].to_string(), i + 1),
            _ => i += 1,
        }
    }
    panic!("unterminated string literal at byte {open}");
}

/// Read one crate source file by its path relative to the manifest directory.
fn read_source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}
