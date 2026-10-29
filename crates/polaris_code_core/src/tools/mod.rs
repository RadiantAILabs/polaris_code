//! Tools the coding agent can call.
//!
//! Each toolset is a `#[toolset]` struct plus a thin
//! [`Plugin`](polaris::system::plugin::Plugin) that registers it with the
//! [`ToolRegistry`](polaris::tools::ToolRegistry).
//!
//! To add one: create `tools/<name>.rs` with a `#[toolset] impl MyTools` and a
//! `MyToolsPlugin` whose `build()` calls `register_toolset(..)` on the registry
//! (depend on `ToolsPlugin`), then `mod <name>;` and re-export here. See
//! [`code`] for a worked example.
//!
//! [`shell_render`] is the exception that proves the shape: it registers
//! nothing, because the tool it shapes results for (`run_command`) belongs to
//! upstream `polaris_shell`. Presenting a result is the consumer's job, so it
//! lives here rather than in the framework.

pub mod code;
pub mod code_graph;
pub mod fileset;
pub mod memory;
pub(crate) mod shell_render;
pub mod tasks;

pub use code::{CodeTools, CodeToolsConfig, CodeToolsPlugin, FILE_WRITE_TOOLS};
pub use code_graph::{
    CODE_GRAPH_IRI, CodeGraphPlugin, CodeGraphTools, CodeIndexState, CodeIndexer, ConceptTools,
    ENRICHED_GRAPH_IRI, GraphSearchMode, IndexPhase, ROLES, code_index_state, enrich,
};
pub use fileset::{PathGlob, Selection, WalkLimits, select_files};
pub use memory::{MEMORY_GRAPH_IRI, MemoryTools, MemoryToolsPlugin};
pub use tasks::{RUN_IN_BACKGROUND, STOP_TASK, TASK_OUTPUT, TaskTools, TaskToolsPlugin};

/// Whether a tool only reads state (vs. mutating files or the store).
///
/// This is the classification seam the concurrent executor uses to batch
/// independent reads, and the point at which read-only-ness could later be
/// sourced from `TBox` capability triples.
///
/// Read-only: `read_file`, `read_span`, `search`, `glob`, `list_files`, the `outline` /
/// `read_symbol` / `find_*`/`callers_of`/`callees_of`/`impls_of_trait`
/// navigation tools, the `concepts`/`find_by_role`/`resources`/`accessors_of`
/// concept queries, `recall_memory`, `task_output`, and `sparql_query`.
/// Mutating: `edit_file`, `create_file`, `run_command`, `run_in_background`,
/// `stop_task`, `memoize`, `sparql_update`, `load_ontology_turtle`.
#[must_use]
pub fn tool_is_read_only(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "read_span"
            | "search"
            | "glob"
            | "list_files"
            | "outline"
            | "read_symbol"
            | "find_definition"
            | "find_references"
            | "callers_of"
            | "callees_of"
            | "impls_of_trait"
            | "concepts"
            | "find_by_role"
            | "resources"
            | "accessors_of"
            | "recall_memory"
            | "task_output"
            | "sparql_query"
            | "describe_resource"
    )
}

#[cfg(test)]
mod tests {
    use super::tool_is_read_only;

    #[test]
    fn classifies_read_and_write_tools() {
        assert!(tool_is_read_only("read_file"));
        assert!(tool_is_read_only("search"));
        assert!(tool_is_read_only("glob"));
        assert!(tool_is_read_only("callers_of"));
        assert!(tool_is_read_only("sparql_query"));
        assert!(!tool_is_read_only("edit_file"));
        assert!(!tool_is_read_only("create_file"));
        assert!(tool_is_read_only("task_output"));
        assert!(!tool_is_read_only("run_command"));
        assert!(!tool_is_read_only("run_in_background"));
        assert!(!tool_is_read_only("stop_task"));
        assert!(!tool_is_read_only("sparql_update"));
    }
}
