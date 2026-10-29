//! Interactive inspection agent: drive SPARQL tools over a loaded KG.
//!
//! Used by `ontrace inspect <ttl-or-dir>`. The agent takes one question per
//! `run_oneshot` call, loops on the LLM with the [`OxigraphTools`] toolset
//! attached, and returns the final assistant text once the model stops
//! emitting tool calls (or [`InspectConfig::max_iterations`] is reached).
//!
//! Reuses [`AgentConfig`] / [`AgentLlm`] from the extraction agent so the
//! model-resolution path is shared.
//!
//! [`OxigraphTools`]: crate::oxigraph::OxigraphTools

use crate::agents::extraction::{AgentConfig, AgentLlm, PredicateShape, render_predicate};
use crate::agents::oxigraph_extraction::snapshot_curated;
use crate::oxigraph::OxigraphStore;
use crate::resources::spend::Spend;
use polaris::agent::{Agent, SetupError};
use polaris::graph::Graph;
use polaris::models::ModelRegistry;
use polaris::models::llm::{
    AssistantBlock, LlmRequest, Message, TextBlock, ToolResultContent, UserBlock,
};
use polaris::system::param::{Res, SystemContext};
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris::tools::ToolRegistry;
use std::collections::HashMap;

/// Maximum number of LLM ↔ tool round-trips per question before the agent
/// gives up. Each iteration is one `llm.generate` call.
pub const DEFAULT_MAX_ITERATIONS: usize = 12;

/// System prompt steering the model toward summarizing-first SPARQL.
///
/// Exposed `pub(crate)` so tests can assert the strategy guidance survives
/// edits — the 32 KiB tool-result cap means raw enumeration silently loses
/// data, and the prompt is the only thing that nudges the LLM toward
/// `COUNT` / `GROUP BY` / `LIMIT` over `SELECT *`.
pub(crate) const INSPECT_SYSTEM_PROMPT: &str = "\
You inspect an RDF knowledge graph by issuing SPARQL through tools.

Named graphs:
- `urn:polaris:graph:ontology` — TBox: classes, predicates, domain/range, OWL axioms.
- `urn:polaris:graph:data` — ABox: extracted observations.
  Each fact appears twice: once as a direct quad `(s, p, o)`, and once as a
  reified `rdf:Statement` blank node carrying `urn:ontrace:observedAt` and
  (optionally) `urn:ontrace:traceId`. Filter the blank-node statements out of
  shape queries unless you need timestamps.

Tools:
- `sparql_query`         — read-only SELECT / ASK / CONSTRUCT / DESCRIBE.
- `sparql_update`        — mutating INSERT / DELETE / LOAD / CLEAR / DROP.
- `load_ontology_turtle` — append Turtle into the ontology (TBox) graph.
- `describe_resource`    — Turtle dump of all triples mentioning an IRI.

Strategy:
1. Start with shape, not enumeration. Examples:
   - predicate frequency:
     `SELECT ?p (COUNT(*) AS ?n) WHERE { GRAPH <urn:polaris:graph:data> { ?s ?p ?o . FILTER(!isBlank(?s)) } } GROUP BY ?p ORDER BY DESC(?n)`
   - top subjects: same shape, group by `?s`.
2. When you need samples, `LIMIT 20` and `FILTER(!isBlank(?s))` first.
3. Prefer `describe_resource <iri>` over hand-rolled `CONSTRUCT` for a known node.
4. Every tool response is capped at 32 KiB — prefer many narrow queries over one wide one.

Answer the user's question directly once you have enough evidence. Cite the
IRIs, counts, or samples you used so the user can verify.";

/// One user question for the inspection agent.
///
/// Inserted as a `LocalResource` by `SessionsAPI::run_oneshot`'s setup closure.
#[derive(Debug, Clone)]
pub struct InspectInput {
    /// Free-form natural-language question.
    pub question: String,
}

impl InspectInput {
    /// Construct an input from a single question string.
    #[must_use]
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
        }
    }
}

impl LocalResource for InspectInput {}

/// Per-session knobs that aren't part of the shared [`AgentConfig`].
#[derive(Debug, Clone)]
pub struct InspectConfig {
    /// Hard cap on LLM ↔ tool round-trips. Prevents a runaway model from
    /// burning tokens forever on a malformed query.
    pub max_iterations: usize,
}

impl InspectConfig {
    /// Construct with an explicit iteration cap.
    #[must_use]
    pub fn new(max_iterations: usize) -> Self {
        Self { max_iterations }
    }
}

impl Default for InspectConfig {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
        }
    }
}

impl LocalResource for InspectConfig {}

/// Curated `TBox` overview rendered into the system prompt when the ontology
/// graph carries declarations.
///
/// Empty when the ontology graph is empty — the agent then runs in
/// discovery mode (it can still SPARQL the `TBox` itself, but it isn't
/// pre-briefed).
#[derive(Debug, Clone, Default)]
pub struct InspectOntologyOverview {
    /// Prompt-ready block. Empty string means "nothing to append".
    pub block: String,
}

impl InspectOntologyOverview {
    /// Build an overview from the `TBox` snapshot. Returns an empty overview
    /// when there are no curated predicates *and* no curated classes — the
    /// agent then runs without ontology guidance.
    #[must_use]
    pub fn from_snapshot(
        predicates: &[String],
        classes: &[String],
        shapes: &HashMap<String, PredicateShape>,
    ) -> Self {
        if predicates.is_empty() && classes.is_empty() {
            return Self::default();
        }
        use std::fmt::Write;
        let mut s = String::from("Curated TBox loaded in <urn:polaris:graph:ontology>:\n");
        if !predicates.is_empty() {
            s.push_str("- predicates (Domain → Range; `*` = unconstrained):\n");
            for p in predicates {
                let _ = writeln!(s, "    - {}", render_predicate(p, shapes.get(p)));
            }
        }
        if !classes.is_empty() {
            s.push_str("- classes: ");
            s.push_str(&classes.join(", "));
            s.push('\n');
        }
        s.push_str(
            "Use these local names as a vocabulary when planning queries. \
             Curated predicates land in the data graph at the same IRIs the TBox \
             declares them under, so `?s <tbox-iri> ?o` joins work directly. \
             Predicates the LLM coined outside the curated set still mint as \
             `urn:ontrace:p/<local-name>` — check both forms if a query returns \
             nothing.",
        );
        Self { block: s }
    }

    /// `true` when no curated guidance is available.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.block.is_empty()
    }
}

impl LocalResource for InspectOntologyOverview {}

/// One-shot agent that answers a single question about the loaded KG by
/// looping the LLM against the [`OxigraphTools`] toolset.
///
/// Stateless across questions — each `run_oneshot` call gets a fresh
/// conversation. Cross-session memory would belong in a `GlobalResource`,
/// not on the agent struct.
///
/// [`OxigraphTools`]: crate::oxigraph::OxigraphTools
#[derive(Debug, Clone, Default)]
pub struct InspectAgent;

impl InspectAgent {
    /// Stable agent name used to look up the [`polaris::sessions::AgentTypeId`].
    pub const NAME: &'static str = "ontrace::InspectAgent";
}

impl Agent for InspectAgent {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn build(&self, graph: &mut Graph) {
        graph.add_system(answer_question);
    }

    fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        let model_id = ctx
            .get_resource::<AgentConfig>()
            .map_err(SetupError::new)?
            .model_id
            .clone();
        let llm = ctx
            .get_resource::<ModelRegistry>()
            .map_err(SetupError::new)?
            .llm(&model_id)
            .map_err(SetupError::new)?;
        ctx.insert(AgentLlm::new(llm));
        if ctx.get_resource::<InspectConfig>().is_err() {
            ctx.insert(InspectConfig::default());
        }
        // Snapshot the curated TBox once per session — the result is
        // appended to the system prompt so the LLM sees the schema
        // before its first turn. Empty store → empty overview, agent
        // runs in pure discovery mode.
        let overview = match ctx.get_resource::<OxigraphStore>() {
            Ok(store) => {
                let (predicates, classes, shapes, _piris, _ciris, _hierarchy) =
                    snapshot_curated(&store).map_err(SetupError::new)?;
                InspectOntologyOverview::from_snapshot(&predicates, &classes, &shapes)
            }
            Err(_) => InspectOntologyOverview::default(),
        };
        ctx.insert(overview);
        Ok(())
    }
}

/// Tool-result content shaped for an LLM follow-up turn.
///
/// `ToolRegistry::execute` returns `serde_json::Value`. The `#[tool]` macro
/// wraps a `String` return as `Value::String`, which we want to surface
/// verbatim; structured returns get re-serialized. Either way the LLM sees
/// a flat string back.
fn render_tool_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Drive the LLM ↔ tool loop until the model stops asking for tools or we
/// hit [`InspectConfig::max_iterations`].
///
/// Each round-trip:
/// 1. `llm.generate(request)` — sends conversation + tool catalog.
/// 2. If the response has no [`AssistantBlock::ToolCall`] blocks, return its
///    text as the final answer.
/// 3. Otherwise record the assistant turn, dispatch each call via
///    [`ToolRegistry::execute`], and append the results as `UserBlock::ToolResult`
///    so the next iteration sees them in conversation history.
///
/// Provider/registry errors are converted into a `ToolResult` with
/// `ToolResultStatus::Error` rather than propagated — a single bad query
/// shouldn't abort a long investigation.
#[system]
async fn answer_question(
    input: Res<InspectInput>,
    llm: Res<AgentLlm>,
    config: Res<AgentConfig>,
    inspect: Res<InspectConfig>,
    overview: Res<InspectOntologyOverview>,
    tools: Res<ToolRegistry>,
    spend: Res<Spend>,
) -> String {
    let definitions = tools.definitions();
    let system_prompt = if overview.is_empty() {
        INSPECT_SYSTEM_PROMPT.to_owned()
    } else {
        format!("{INSPECT_SYSTEM_PROMPT}\n\n{}", overview.block)
    };
    let mut request = LlmRequest {
        system: Some(system_prompt),
        messages: vec![Message::user(input.question.clone())],
        tools: if definitions.is_empty() {
            None
        } else {
            Some(definitions)
        },
        tool_choice: None,
        output_schema: None,
        ..Default::default()
    };

    for iteration in 0..inspect.max_iterations {
        tracing::info!(iteration, model = %config.model_id, "inspect: LLM call");
        let response = match llm.generate(request.clone()).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, iteration, "inspect LLM call failed");
                return format!("inspection failed: {err}");
            }
        };
        spend.record(
            &config.model_id,
            response.usage.input_tokens,
            response.usage.output_tokens,
        );

        if !response.has_tool_calls() {
            tracing::info!(
                iteration,
                input_tokens = response.usage.input_tokens,
                output_tokens = response.usage.output_tokens,
                "inspect: final answer",
            );
            return response.text();
        }

        let calls: Vec<_> = response.tool_calls().into_iter().cloned().collect();
        tracing::info!(
            iteration,
            tool_calls = calls.len(),
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "inspect: tool round",
        );
        request.messages.push(Message::Assistant {
            id: None,
            content: response.content.clone(),
        });

        let mut tool_results: Vec<UserBlock> = Vec::with_capacity(calls.len());
        for call in calls {
            tracing::info!(
                tool = %call.function.name,
                args = %call.function.arguments,
                "inspect: tool call",
            );
            let block = match tools
                .execute(&call.function.name, &call.function.arguments)
                .await
            {
                Ok(v) => UserBlock::tool_result(
                    call.id.clone(),
                    ToolResultContent::Text(render_tool_value(&v)),
                ),
                Err(err) => {
                    UserBlock::tool_error(call.id.clone(), ToolResultContent::Text(err.to_string()))
                }
            };
            tool_results.push(block);
        }
        request.messages.push(Message::User {
            content: tool_results,
        });
    }

    tracing::warn!(
        max = inspect.max_iterations,
        "inspect agent hit iteration cap without producing a final answer",
    );
    // Best-effort: surface any text from the last assistant turn, if there
    // was one, so the user isn't left with a bare error.
    let tail = request.messages.iter().rev().find_map(|msg| match msg {
        Message::Assistant { content, .. } => {
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    AssistantBlock::Text(TextBlock { text }) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if text.is_empty() { None } else { Some(text) }
        }
        Message::User { .. } => None,
    });
    match tail {
        Some(t) => format!(
            "(inspect hit iteration cap; partial answer below)\n{t}",
            t = t,
        ),
        None => "inspection hit max iterations without producing a final answer".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_mentions_both_named_graphs() {
        assert!(INSPECT_SYSTEM_PROMPT.contains("urn:polaris:graph:ontology"));
        assert!(INSPECT_SYSTEM_PROMPT.contains("urn:polaris:graph:data"));
    }

    #[test]
    fn system_prompt_steers_toward_summarizing_queries() {
        // The 32 KiB cap on tool results is enforced by OxigraphTools — the
        // prompt is the only place we tell the LLM not to enumerate.
        assert!(INSPECT_SYSTEM_PROMPT.contains("COUNT"));
        assert!(INSPECT_SYSTEM_PROMPT.contains("LIMIT"));
        assert!(INSPECT_SYSTEM_PROMPT.contains("32 KiB"));
    }

    #[test]
    fn render_tool_value_unwraps_strings() {
        let v = serde_json::Value::String("hello".into());
        assert_eq!(render_tool_value(&v), "hello");
    }

    #[test]
    fn render_tool_value_serializes_structured_results() {
        let v = serde_json::json!({"ok": true, "n": 3});
        let rendered = render_tool_value(&v);
        assert!(rendered.contains("\"ok\""));
        assert!(rendered.contains("3"));
    }

    #[test]
    fn inspect_config_default_has_a_finite_cap() {
        let c = InspectConfig::default();
        assert!(c.max_iterations > 0);
        assert!(c.max_iterations < 1000, "cap should be reasonable");
    }

    #[test]
    fn overview_empty_when_tbox_carries_nothing() {
        let overview = InspectOntologyOverview::from_snapshot(&[], &[], &HashMap::new());
        assert!(overview.is_empty());
    }

    #[test]
    fn overview_renders_predicates_with_shapes_and_classes() {
        let mut shapes = HashMap::new();
        shapes.insert(
            "works_at".to_owned(),
            PredicateShape {
                domain_class: Some("Person".to_owned()),
                range_class: Some("Organization".to_owned()),
                ..Default::default()
            },
        );
        let overview = InspectOntologyOverview::from_snapshot(
            &["met".to_owned(), "works_at".to_owned()],
            &["Person".to_owned(), "Organization".to_owned()],
            &shapes,
        );
        assert!(!overview.is_empty());
        assert!(overview.block.contains("works_at (Person → Organization)"));
        assert!(overview.block.contains("- met"));
        assert!(overview.block.contains("Person, Organization"));
        // Reminds the LLM that ABox IRI minting can differ from TBox IRIs.
        assert!(overview.block.contains("urn:ontrace:p/"));
    }

    #[test]
    fn overview_renders_classes_only_when_no_predicates() {
        let overview =
            InspectOntologyOverview::from_snapshot(&[], &["Event".to_owned()], &HashMap::new());
        assert!(!overview.is_empty());
        assert!(overview.block.contains("classes: Event"));
        assert!(!overview.block.contains("- predicates"));
    }
}
