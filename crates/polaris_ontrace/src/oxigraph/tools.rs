//! [`OxigraphTools`] — LLM-callable SPARQL toolset.
//!
//! Registered with the Polaris [`ToolRegistry`] by [`super::OxigraphPlugin`]
//! when `register_tools` is enabled. The toolset clones the
//! [`super::OxigraphStore`] handle (cheap — internally `Arc`-shared) so tools
//! can issue reads and writes against the same dataset that systems see via
//! [`Res<OxigraphStore>`].
//!
//! # Tools
//!
//! | Tool | Default permission | Description |
//! |------|--------------------|-------------|
//! | `sparql_query`         | `Allow`   | Read-only SELECT/ASK/CONSTRUCT/DESCRIBE |
//! | `sparql_update`        | `Confirm` | Mutating INSERT/DELETE/LOAD/CLEAR |
//! | `load_ontology_turtle` | `Confirm` | Parse Turtle into the ontology graph |
//! | `describe_resource`    | `Allow`   | `DESCRIBE <iri>` convenience |
//!
//! [`ToolRegistry`]: polaris::tools::ToolRegistry
//! [`Res<OxigraphStore>`]: polaris::system::param::Res

use super::store::OxigraphStore;
use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use oxigraph::sparql::results::QueryResultsFormat;
use polaris::tools::{ToolError, toolset};

/// Hard cap on serialized result size returned by `sparql_query`.
///
/// SPARQL results can be very large; we truncate to keep tool responses from
/// blowing past LLM context budgets. The truncation marker is appended only
/// when this cap is hit.
const RESULT_BYTE_LIMIT: usize = 32 * 1024;

/// SPARQL toolset over a shared [`OxigraphStore`] handle.
///
/// Constructed by [`super::OxigraphPlugin`]; you usually don't build this
/// yourself. Cloning is cheap and shares the underlying dataset.
#[derive(Clone)]
pub struct OxigraphTools {
    store: OxigraphStore,
}

impl OxigraphTools {
    /// Wrap a store handle in the toolset.
    #[must_use]
    pub fn new(store: OxigraphStore) -> Self {
        Self { store }
    }
}

#[toolset]
impl OxigraphTools {
    /// Execute a SPARQL 1.1 read query (SELECT / ASK / CONSTRUCT / DESCRIBE).
    ///
    /// SELECT and ASK return SPARQL JSON Results. CONSTRUCT and DESCRIBE
    /// return Turtle. Output is truncated past 32 KiB.
    #[tool]
    async fn sparql_query(
        &self,
        /// A complete SPARQL 1.1 query string.
        query: String,
    ) -> Result<String, ToolError> {
        let results = self
            .store
            .query(&query)
            .map_err(|e| ToolError::execution_error(format!("query failed: {e}")))?;

        let body = match results {
            QueryResults::Graph(_) => {
                // CONSTRUCT / DESCRIBE → Turtle.
                let buf = results
                    .write_graph(Vec::<u8>::new(), RdfFormat::Turtle)
                    .map_err(|e| {
                        ToolError::execution_error(format!("turtle serialize failed: {e}"))
                    })?;
                String::from_utf8(buf).map_err(|e| {
                    ToolError::execution_error(format!("turtle output not utf-8: {e}"))
                })?
            }
            QueryResults::Solutions(_) | QueryResults::Boolean(_) => {
                // SELECT / ASK → JSON.
                let buf = results
                    .write(Vec::<u8>::new(), QueryResultsFormat::Json)
                    .map_err(|e| {
                        ToolError::execution_error(format!("json serialize failed: {e}"))
                    })?;
                String::from_utf8(buf).map_err(|e| {
                    ToolError::execution_error(format!("json output not utf-8: {e}"))
                })?
            }
        };

        Ok(truncate(body, RESULT_BYTE_LIMIT))
    }

    /// Apply a SPARQL 1.1 UPDATE (INSERT / DELETE / LOAD / CLEAR / CREATE / DROP).
    ///
    /// Returns a short confirmation including the post-update store size.
    #[tool]
    async fn sparql_update(
        &self,
        /// A complete SPARQL 1.1 UPDATE string.
        update: String,
    ) -> Result<String, ToolError> {
        self.store
            .update(&update)
            .map_err(|e| ToolError::execution_error(format!("update failed: {e}")))?;
        let n = self
            .store
            .len()
            .map_err(|e| ToolError::execution_error(format!("len after update failed: {e}")))?;
        Ok(format!("ok; store now has {n} quads"))
    }

    /// Parse a Turtle fragment and load it into the ontology (`TBox`) named graph.
    ///
    /// Use this to seed class hierarchies, predicate definitions, OWL axioms,
    /// and other curated schema content separately from runtime data.
    #[tool]
    async fn load_ontology_turtle(
        &self,
        /// A complete Turtle document. Prefixes must be declared inline.
        turtle: String,
    ) -> Result<String, ToolError> {
        self.store
            .load_turtle(&turtle, self.store.ontology_graph())
            .map_err(|e| ToolError::execution_error(format!("load failed: {e}")))?;
        let n = self
            .store
            .len()
            .map_err(|e| ToolError::execution_error(format!("len after load failed: {e}")))?;
        Ok(format!(
            "loaded into ontology graph; store now has {n} quads"
        ))
    }

    /// Return all triples mentioning `iri` as subject or object, as Turtle.
    ///
    /// Shorthand for `DESCRIBE <iri>` without requiring the LLM to escape the
    /// angle brackets.
    #[tool]
    async fn describe_resource(
        &self,
        /// The IRI to describe (without angle brackets, e.g. `urn:alice`).
        iri: String,
    ) -> Result<String, ToolError> {
        // Validate the IRI before splicing it into a query string.
        let validated = oxigraph::model::NamedNode::new(&iri)
            .map_err(|e| ToolError::parameter_error(format!("invalid IRI '{iri}': {e}")))?;
        let q = format!("DESCRIBE <{}>", validated.as_str());
        let results = self
            .store
            .query(&q)
            .map_err(|e| ToolError::execution_error(format!("describe failed: {e}")))?;
        let buf = results
            .write_graph(Vec::<u8>::new(), RdfFormat::Turtle)
            .map_err(|e| ToolError::execution_error(format!("turtle serialize failed: {e}")))?;
        let body = String::from_utf8(buf)
            .map_err(|e| ToolError::execution_error(format!("turtle output not utf-8: {e}")))?;
        Ok(truncate(body, RESULT_BYTE_LIMIT))
    }
}

fn truncate(mut s: String, limit: usize) -> String {
    if s.len() <= limit {
        return s;
    }
    // Truncate on a char boundary.
    let mut cut = limit;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("\n... [truncated]");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::tools::Toolset;

    #[tokio::test]
    async fn round_trip_via_tools() {
        let store = OxigraphStore::in_memory().unwrap();
        let tools = OxigraphTools::new(store.clone());

        tools
            .sparql_update("INSERT DATA { <urn:alice> <urn:knows> <urn:bob> }".into())
            .await
            .unwrap();

        let answer = tools
            .sparql_query("SELECT ?o WHERE { <urn:alice> <urn:knows> ?o }".into())
            .await
            .unwrap();
        assert!(answer.contains("urn:bob"), "got: {answer}");
    }

    #[tokio::test]
    async fn load_ontology_then_query() {
        let store = OxigraphStore::in_memory().unwrap();
        let tools = OxigraphTools::new(store);

        tools
            .load_ontology_turtle(
                "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                 <urn:ex:Person> a owl:Class ."
                    .into(),
            )
            .await
            .unwrap();

        let ans = tools
            .sparql_query(
                "ASK { GRAPH <urn:polaris:graph:ontology> {\
                 <urn:ex:Person> a <http://www.w3.org/2002/07/owl#Class>\
                 } }"
                .into(),
            )
            .await
            .unwrap();
        assert!(ans.contains("true"), "got: {ans}");
    }

    #[tokio::test]
    async fn describe_resource_returns_turtle() {
        let store = OxigraphStore::in_memory().unwrap();
        let tools = OxigraphTools::new(store);
        tools
            .sparql_update("INSERT DATA { <urn:alice> <urn:knows> <urn:bob> }".into())
            .await
            .unwrap();

        let ttl = tools.describe_resource("urn:alice".into()).await.unwrap();
        assert!(ttl.contains("urn:bob"), "got: {ttl}");
    }

    #[test]
    fn toolset_registers_four_tools() {
        let tools = OxigraphTools::new(OxigraphStore::in_memory().unwrap());
        let names: Vec<String> = tools
            .tools()
            .into_iter()
            .map(|t| t.definition().name)
            .collect();
        let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(names_ref.contains(&"sparql_query"), "names: {names_ref:?}");
        assert!(names_ref.contains(&"sparql_update"));
        assert!(names_ref.contains(&"load_ontology_turtle"));
        assert!(names_ref.contains(&"describe_resource"));
    }
}
