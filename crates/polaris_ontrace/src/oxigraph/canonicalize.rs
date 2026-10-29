//! Pre-materialize the `owl:sameAs` closure on an [`OxigraphStore`].
//!
//! `ontrace`'s extraction pipeline records identity links between surface
//! forms of the same entity by emitting `owl:sameAs` triples
//! (see [`crate::resources::aliases`]). Untouched, those alias triples sit
//! alongside the originals — `?s ?p ?o` queries return both surface forms,
//! and aggregates like `GROUP BY ?s` / `COUNT(*)` double-count any entity
//! that's been observed under more than one spelling.
//!
//! The two helpers here run one SPARQL UPDATE that rewrites every
//! non-`owl:sameAs` triple in either the default graph
//! ([`canonicalize_default_graph`]) or a single named graph
//! ([`canonicalize_named_graph`]): each subject and object is substituted
//! with the transitive `owl:sameAs+` target when one exists. Triples whose
//! subject and object are already canonical pass through unchanged.
//! `owl:sameAs` mapping triples themselves are preserved so downstream
//! queries can still inspect the alias structure.
//!
//! The UPDATE is idempotent — running it twice yields the same dataset,
//! because canonical entities have no outgoing `owl:sameAs` edge so the
//! `OPTIONAL` clause doesn't bind and `COALESCE` re-selects the original.

use crate::oxigraph::OxigraphStore;
use oxigraph::sparql::EvaluationError;

/// Rewrite every non-`owl:sameAs` triple in the default graph so its
/// subject and object are the canonical cluster IRI.
///
/// See the module-level docs for the rewrite semantics and idempotency
/// note. Use [`canonicalize_named_graph`] when the data lives in a named
/// graph instead of the default graph (e.g. the `inspect` subcommand's
/// `data` graph).
///
/// # Errors
///
/// Forwards the SPARQL evaluation error from the underlying store.
pub fn canonicalize_default_graph(store: &OxigraphStore) -> Result<(), EvaluationError> {
    let update = "PREFIX owl: <http://www.w3.org/2002/07/owl#>\n\
                  DELETE { ?s ?p ?o }\n\
                  INSERT { ?canon ?p ?ocanon }\n\
                  WHERE {\n\
                    ?s ?p ?o .\n\
                    FILTER(?p != owl:sameAs)\n\
                    OPTIONAL { ?s owl:sameAs+ ?cs }\n\
                    OPTIONAL { ?o owl:sameAs+ ?co }\n\
                    BIND(COALESCE(?cs, ?s) AS ?canon)\n\
                    BIND(COALESCE(?co, ?o) AS ?ocanon)\n\
                  }";
    store.update(update)
}

/// Named-graph variant of [`canonicalize_default_graph`].
///
/// Identical semantics, but scoped to the named graph at `graph_iri`.
///
/// # Errors
///
/// Forwards the SPARQL evaluation error from the underlying store.
pub fn canonicalize_named_graph(
    store: &OxigraphStore,
    graph_iri: &str,
) -> Result<(), EvaluationError> {
    let update = format!(
        "PREFIX owl: <http://www.w3.org/2002/07/owl#>\n\
         DELETE {{ GRAPH <{graph_iri}> {{ ?s ?p ?o }} }}\n\
         INSERT {{ GRAPH <{graph_iri}> {{ ?canon ?p ?ocanon }} }}\n\
         WHERE {{\n\
           GRAPH <{graph_iri}> {{ ?s ?p ?o . FILTER(?p != owl:sameAs) }}\n\
           OPTIONAL {{ GRAPH <{graph_iri}> {{ ?s owl:sameAs+ ?cs }} }}\n\
           OPTIONAL {{ GRAPH <{graph_iri}> {{ ?o owl:sameAs+ ?co }} }}\n\
           BIND(COALESCE(?cs, ?s) AS ?canon)\n\
           BIND(COALESCE(?co, ?o) AS ?ocanon)\n\
         }}"
    );
    store.update(&update)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::GraphNameRef;
    use oxigraph::sparql::QueryResults;

    /// Tiny dataset: two alias surface forms (`alice`, `Alice`) both
    /// linked by `owl:sameAs` to canonical `Alice`. `alice` and `Alice`
    /// each carry one `<met>` triple — a naive `GROUP BY ?s` sees them
    /// as two distinct subjects until canonicalization collapses them.
    fn seed_default_graph() -> OxigraphStore {
        let store = OxigraphStore::in_memory().unwrap();
        store
            .load_turtle(
                "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                 @prefix e: <urn:ontrace:e/> .\n\
                 @prefix p: <urn:ontrace:p/> .\n\
                 e:alice owl:sameAs e:Alice .\n\
                 e:Alice p:met e:bob .\n\
                 e:alice p:met e:carol .\n",
                GraphNameRef::DefaultGraph,
            )
            .unwrap();
        store
    }

    fn count(store: &OxigraphStore, sparql: &str) -> u64 {
        match store.query(sparql).unwrap() {
            QueryResults::Solutions(rows) => {
                let mut total = 0u64;
                for row in rows {
                    let row = row.unwrap();
                    if let Some(oxigraph::model::Term::Literal(lit)) = row.get("n") {
                        total = lit.value().parse().expect("integer count");
                    }
                }
                total
            }
            _ => panic!("expected Solutions"),
        }
    }

    #[test]
    fn canonicalize_collapses_alias_subjects_under_group_by() {
        let store = seed_default_graph();
        // Pre: two distinct subjects with `<met>`.
        let pre = count(
            &store,
            "PREFIX p: <urn:ontrace:p/> \
             SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s p:met ?o }",
        );
        assert_eq!(pre, 2, "before canonicalize: alice and Alice both visible");

        canonicalize_default_graph(&store).unwrap();

        let post = count(
            &store,
            "PREFIX p: <urn:ontrace:p/> \
             SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s p:met ?o }",
        );
        assert_eq!(post, 1, "after canonicalize: both rows collapse onto Alice");

        // owl:sameAs links survive — downstream queries can still inspect them.
        let same_as_rows = count(
            &store,
            "PREFIX owl: <http://www.w3.org/2002/07/owl#> \
             SELECT (COUNT(*) AS ?n) WHERE { ?s owl:sameAs ?o }",
        );
        assert_eq!(same_as_rows, 1);
    }

    #[test]
    fn canonicalize_is_idempotent() {
        let store = seed_default_graph();
        canonicalize_default_graph(&store).unwrap();
        let after_first = count(&store, "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }");
        canonicalize_default_graph(&store).unwrap();
        let after_second = count(&store, "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }");
        assert_eq!(
            after_first, after_second,
            "second canonicalize pass must not add or drop quads",
        );
    }

    #[test]
    fn canonicalize_named_graph_scoped_to_graph_iri() {
        let store = OxigraphStore::in_memory().unwrap();
        // Same fixture, but in the conventional data graph.
        store
            .load_turtle(
                "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                 @prefix e: <urn:ontrace:e/> .\n\
                 @prefix p: <urn:ontrace:p/> .\n\
                 e:alice owl:sameAs e:Alice .\n\
                 e:Alice p:met e:bob .\n\
                 e:alice p:met e:carol .\n",
                store.data_graph(),
            )
            .unwrap();

        canonicalize_named_graph(&store, store.data_graph().as_str()).unwrap();

        let n = count(
            &store,
            &format!(
                "PREFIX p: <urn:ontrace:p/> \
                 SELECT (COUNT(DISTINCT ?s) AS ?n) \
                 WHERE {{ GRAPH <{}> {{ ?s p:met ?o }} }}",
                store.data_graph().as_str(),
            ),
        );
        assert_eq!(n, 1, "canonicalize_named_graph should collapse aliases");
    }
}
