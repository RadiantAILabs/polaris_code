//! Entity canonicalization via an alias table.
//!
//! Implements the semantic-web "link, don't merge" approach: observed
//! entities keep their original surface form in the KG, and an
//! `owl:sameAs` triple is emitted when two surface forms normalize to the
//! same key. Downstream consumers (or a SPARQL reasoner) close the cluster
//! transitively without losing observational provenance.
//!
//! [`Aliases`] holds the cross-trace table; [`normalize`] is the matching
//! rule. The predicate constant used for identity links is [`OWL_SAME_AS`].

use crate::resources::kg::Entity;
use parking_lot::RwLock;
use polaris::system::resource::GlobalResource;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::{fs, io};
use unicode_normalization::UnicodeNormalization;

/// Predicate string ontrace uses when emitting an identity link.
///
/// Recognised by [`crate::dump::write_turtle`] and serialised against the
/// OWL namespace `<http://www.w3.org/2002/07/owl#sameAs>`.
pub const OWL_SAME_AS: &str = "owl:sameAs";

/// Outcome of resolving an observed entity against the alias table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Nothing to emit. Either this is the canonical for its normalized
    /// key, or the alias has already been emitted on an earlier observation.
    None,
    /// Emit `entity owl:sameAs canonical` to record the identity link.
    EmitSameAs {
        /// Canonical entity for the cluster.
        canonical: Entity,
    },
}

#[derive(Default)]
struct AliasesInner {
    canonical: HashMap<String, Entity>,
    emitted: HashSet<(Entity, Entity)>,
}

/// Cross-trace entity-canonicalization table.
///
/// Use this when a system needs to decide whether a freshly observed entity
/// is a new surface form of one already seen and, if so, emit the identity
/// link. Call [`Aliases::resolve`] on each extracted entity: it registers the
/// entity on first sight and returns [`Resolution::EmitSameAs`] exactly once
/// per `(observed, canonical)` pair so the write path can record an
/// `owl:sameAs` triple without collapsing nodes ("link, don't merge").
///
/// # Scope
///
/// `Global` — canonicalization must hold across every per-trace session: the
/// first surface form seen in one session becomes the canonical that later
/// sessions alias to, so the table is shared, not per-session.
///
/// # Provided by
///
/// [`crate::ExtractionPlugin`] and
/// [`crate::plugins::oxigraph_extraction::OxigraphExtractionPlugin`] each
/// insert it in `build()` — `Aliases::new()` by default, or
/// `Aliases::with_seeds(...)` when hand-curated `(alias, target)` pairs were
/// supplied via the plugin's `with_aliases` builder.
///
/// # Access
///
/// Consume as `Res<Aliases>`. `ResMut<T>` is not used for globals in this
/// crate; [`Aliases::resolve`] mutates through the interior
/// [`parking_lot::RwLock`] and so takes `&self`.
///
/// Mutation contract: `resolve` is **monotonic and idempotent per pair** — it
/// registers a normalized key the first time it is seen and emits each
/// `owl:sameAs` link at most once; repeated observations of the same alias
/// return [`Resolution::None`] to avoid flooding the KG.
///
/// # Alternatives
///
/// | Alternative | When |
/// | --- | --- |
/// | _none_ | This is the only canonicalization resource in the crate. |
///
/// # Example system
///
/// ```no_run
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::{Aliases, Resolution, Entity};
///
/// /// Resolve an entity and log when a new identity link should be emitted.
/// #[system]
/// async fn note_aliases(aliases: Res<Aliases>) {
///     if let Resolution::EmitSameAs { canonical } = aliases.resolve(&Entity::new("MSFT")) {
///         tracing::info!(?canonical, "emit owl:sameAs");
///     }
/// }
/// ```
#[derive(Default)]
pub struct Aliases {
    inner: RwLock<AliasesInner>,
}

impl Aliases {
    /// Empty alias table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with hand-curated `(alias, target)` pairs.
    ///
    /// Both the alias and the target are pre-registered under a single
    /// canonical entity (the target), so any later observation whose
    /// normalized form matches either side triggers an `owl:sameAs` to the
    /// target unless its raw string equals the target verbatim.
    #[must_use]
    pub fn with_seeds<I, A, T>(seeds: I) -> Self
    where
        I: IntoIterator<Item = (A, T)>,
        A: AsRef<str>,
        T: Into<String>,
    {
        let mut canonical = HashMap::new();
        for (alias, target) in seeds {
            let target = target.into();
            let target_entity = Entity::new(target.clone());
            canonical.insert(normalize(alias.as_ref()), target_entity.clone());
            canonical.insert(normalize(&target), target_entity);
        }
        Self {
            inner: RwLock::new(AliasesInner {
                canonical,
                emitted: HashSet::new(),
            }),
        }
    }

    /// Resolve `entity` against the table, registering it on first sight.
    ///
    /// Returns [`Resolution::EmitSameAs`] exactly once per
    /// `(observed, canonical)` pair — repeated observations of the same
    /// alias return [`Resolution::None`] to avoid flooding the KG.
    pub fn resolve(&self, entity: &Entity) -> Resolution {
        let key = normalize(&entity.0);
        let mut inner = self.inner.write();
        match inner.canonical.get(&key) {
            None => {
                inner.canonical.insert(key, entity.clone());
                Resolution::None
            }
            Some(canonical) if canonical == entity => Resolution::None,
            Some(canonical) => {
                let canonical = canonical.clone();
                let pair = (entity.clone(), canonical.clone());
                if inner.emitted.insert(pair) {
                    Resolution::EmitSameAs { canonical }
                } else {
                    Resolution::None
                }
            }
        }
    }
}

impl GlobalResource for Aliases {}

/// Parse a `(alias, target)` pair list from `path`.
///
/// File format is intentionally minimal so curation stays low-friction:
///
/// - One pair per line, `alias|target`.
/// - Lines starting with `#` (after optional leading whitespace) are
///   comments and skipped.
/// - Blank lines are skipped.
/// - Both sides are trimmed; empty sides on a non-comment line are an error.
///
/// Consumers typically feed the result to [`Aliases::with_seeds`], either
/// directly or after layering with additional in-memory pairs.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] on read failure, or
/// [`io::ErrorKind::InvalidData`] when a non-comment, non-blank line is
/// malformed (missing `|` separator, empty alias or target).
pub fn load_alias_seeds(path: impl AsRef<Path>) -> io::Result<Vec<(String, String)>> {
    let body = fs::read_to_string(path.as_ref())?;
    parse_alias_seeds(&body).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {err}", path.as_ref().display()),
        )
    })
}

/// Parse the same line-based format as [`load_alias_seeds`] from an
/// in-memory string. Exposed so callers can embed alias fixtures in tests
/// without going through the filesystem.
///
/// # Errors
///
/// Returns a short message naming the offending line number for any
/// malformed entry (missing `|`, empty alias or target).
pub fn parse_alias_seeds(body: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for (lineno, raw) in body.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((alias, target)) = line.split_once('|') else {
            return Err(format!(
                "line {}: expected `alias|target`, got `{raw}`",
                lineno + 1
            ));
        };
        let alias = alias.trim();
        let target = target.trim();
        if alias.is_empty() || target.is_empty() {
            return Err(format!(
                "line {}: alias and target must both be non-empty",
                lineno + 1
            ));
        }
        pairs.push((alias.to_owned(), target.to_owned()));
    }
    Ok(pairs)
}

/// Lexical normalization key for alias matching.
///
/// Applies Unicode NFKC, lowercases, trims, and collapses runs of any
/// separator-equivalent character — ASCII whitespace, `-`, and `_` —
/// down to a single `_`. So `request-context`, `request_context`, and
/// `request context` all normalize equal, letting the same concept
/// reach the alias table under one key regardless of which casing the
/// extractor or LLM emitted on a given line. CamelCase is *not* split
/// (`requestContext` ≠ `request_context`) — splitting would be too
/// aggressive; rely on prompt rules or a seed alias for those.
/// Diacritics are not stripped; an NFKD + combining-mark filter step would
/// add that.
#[must_use]
pub fn normalize(s: &str) -> String {
    let composed: String = s.nfkc().collect();
    let lowered = composed.to_lowercase();
    let trimmed = lowered.trim_matches(|c: char| c.is_whitespace() || c == '-' || c == '_');

    let mut out = String::with_capacity(trimmed.len());
    let mut in_sep = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() || ch == '-' || ch == '_' {
            if !in_sep {
                out.push('_');
                in_sep = true;
            }
        } else {
            out.push(ch);
            in_sep = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_case_trim_and_whitespace() {
        assert_eq!(normalize("  Alice  "), "alice");
        assert_eq!(normalize("Alice Smith"), "alice_smith");
        assert_eq!(normalize("Alice\t  Smith"), "alice_smith");
        assert_eq!(normalize("ACME, Inc."), "acme,_inc.");
    }

    #[test]
    fn normalize_treats_hyphen_underscore_and_space_as_equivalent() {
        // The three surface forms collide under one alias key.
        assert_eq!(normalize("request-context"), "request_context");
        assert_eq!(normalize("request_context"), "request_context");
        assert_eq!(normalize("request context"), "request_context");
        assert_eq!(normalize("Request-Context"), "request_context");
    }

    #[test]
    fn normalize_collapses_mixed_and_repeated_separators() {
        // Repeated mixed separators collapse to a single underscore.
        assert_eq!(normalize("foo - _ bar"), "foo_bar");
        assert_eq!(normalize("__leading"), "leading");
        assert_eq!(normalize("trailing--"), "trailing");
    }

    #[test]
    fn normalize_does_not_split_camel_case() {
        // CamelCase is intentionally untouched — same surface form, same key.
        assert_eq!(normalize("requestContext"), "requestcontext");
    }

    #[test]
    fn case_folding_emits_same_as() {
        let aliases = Aliases::new();
        assert_eq!(aliases.resolve(&Entity::new("Alice")), Resolution::None);

        match aliases.resolve(&Entity::new("alice")) {
            Resolution::EmitSameAs { canonical } => {
                assert_eq!(canonical, Entity::new("Alice"));
            }
            other => panic!("expected EmitSameAs, got {other:?}"),
        }
    }

    #[test]
    fn seeded_alias_resolves_against_target() {
        let aliases = Aliases::with_seeds([("msft", "microsoft")]);
        match aliases.resolve(&Entity::new("MSFT")) {
            Resolution::EmitSameAs { canonical } => {
                assert_eq!(canonical, Entity::new("microsoft"));
            }
            other => panic!("expected EmitSameAs, got {other:?}"),
        }
    }

    #[test]
    fn repeat_alias_observation_is_silent() {
        let aliases = Aliases::new();
        aliases.resolve(&Entity::new("Alice"));
        let first = aliases.resolve(&Entity::new("alice"));
        let second = aliases.resolve(&Entity::new("alice"));
        assert!(matches!(first, Resolution::EmitSameAs { .. }));
        assert_eq!(second, Resolution::None);
    }

    #[test]
    fn canonical_self_resolves_silently() {
        let aliases = Aliases::new();
        aliases.resolve(&Entity::new("alice"));
        assert_eq!(aliases.resolve(&Entity::new("alice")), Resolution::None);
    }

    #[test]
    fn parse_alias_seeds_handles_comments_and_blanks() {
        let body = "# comment\n\nassistant|claude\n   # indented comment\nmsft|microsoft\n";
        let pairs = parse_alias_seeds(body).unwrap();
        assert_eq!(
            pairs,
            vec![
                ("assistant".to_owned(), "claude".to_owned()),
                ("msft".to_owned(), "microsoft".to_owned()),
            ],
        );
    }

    #[test]
    fn parse_alias_seeds_rejects_malformed_lines() {
        assert!(parse_alias_seeds("alice\n").is_err());
        assert!(parse_alias_seeds("|target\n").is_err());
        assert!(parse_alias_seeds("alias|\n").is_err());
    }
}
