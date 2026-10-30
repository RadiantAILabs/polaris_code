//! Every tool this binary registers must be classified read-only or mutating.
//!
//! `agents::code_agent` runs a batch concurrently only when every call in it is
//! read-only, and it asks [`tool_is_read_only`] which ones those are. That
//! predicate is a `matches!` over names, so it cannot notice a tool nobody
//! taught it about, so a plain name list would drift the first time someone
//! adds a tool.
//!
//! Two things keep the rot cheap and loud.
//!
//! *Cheap*: the predicate is fail-closed. An unclassified name answers `false`,
//! the batch containing it takes the sequential lane, and the only thing lost
//! is the latency this change saves. A new tool is never wrongly raced.
//!
//! *Loud*: this test. It boots the same tool plugins `main.rs` does, asks the
//! [`ToolRegistry`] what is actually registered, and compares that against an
//! explicit table below. Add a tool and forget to classify it and this fails
//! by name; classify it wrongly relative to the table and this fails on the
//! verdict.
//!
//! The table is deliberately written out here rather than derived from
//! [`tool_is_read_only`] — a table derived from the thing it checks would agree
//! with it unconditionally and prove nothing. What it is really asserting is
//! that a human looked at each registered tool once and said which it is.
//!
//! The upstream `Tool` trait has no slot for this property (it exposes
//! `definition`, `permission`, `execute` and nothing else), so the declaration
//! lives in `tools::tool_is_read_only`, sited with the toolsets rather than in
//! the agent loop. Moving it onto the tool itself is a framework change.

use polaris::plugins::persistence::PersistencePlugin;
use polaris::shell::{ShellConfig, ShellPlugin};
use polaris::tools::{ToolRegistry, ToolsPlugin};
use polaris_code::terminal::BackgroundTasks;
use polaris_code::tools::{
    CodeGraphPlugin, CodeToolsConfig, CodeToolsPlugin, GraphSearchMode, MemoryToolsPlugin,
    TaskToolsPlugin, WebToolsPlugin, tool_is_read_only,
};
use polaris_ontrace::oxigraph::OxigraphPlugin;

use polaris::system::server::Server;

/// What each shipped tool is, asserted by hand.
///
/// `true` means the tool only reads: two of them in one batch may run at the
/// same time without either one changing what the other sees. `false` means it
/// can change a file, the store, or the machine.
const CLASSIFICATION: &[(&str, bool)] = &[
    // `CodeTools` — the file primitives.
    ("read_file", true),
    ("read_span", true),
    ("search", true),
    ("glob", true),
    ("list_files", true),
    ("edit_file", false),
    ("create_file", false),
    // `CodeGraphTools` — navigation over the Code ABox.
    ("outline", true),
    ("read_symbol", true),
    ("find_definition", true),
    ("find_references", true),
    ("callers_of", true),
    ("callees_of", true),
    ("impls_of_trait", true),
    // `ConceptTools` — the curated conceptual-role queries.
    ("concepts", true),
    ("find_by_role", true),
    ("resources", true),
    ("accessors_of", true),
    // `MemoryTools` — reflective memoization.
    ("recall_memory", true),
    ("memoize", false),
    // `TaskTools` — the model's own background runs.
    ("run_in_background", false),
    ("task_output", true),
    ("stop_task", false),
    // `WebTools` — reads a public page; changes nothing on this machine.
    ("web_fetch", true),
    // ontrace's SPARQL toolset.
    ("sparql_query", true),
    ("describe_resource", true),
    ("sparql_update", false),
    ("load_ontology_turtle", false),
    // The shell.
    ("run_command", false),
];

/// Boots the tool plugins `main.rs` boots, and returns the registered names.
async fn registered_tool_names() -> Vec<String> {
    // A directory that exists and holds nothing: the toolsets only need a
    // sandbox root to close over, and registration never touches it.
    let working_dir = std::env::temp_dir();

    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(&working_dir)))
        .add_plugins(OxigraphPlugin::in_memory().with_tools(true))
        // `Both`, because the widest surface is the one that has to be fully
        // classified: a mode that hides a tool still leaves it registered, and
        // a later config change can expose it again.
        .add_plugins(CodeGraphPlugin::new(&working_dir).with_search_mode(GraphSearchMode::Both))
        .add_plugins(MemoryToolsPlugin::new(&working_dir))
        .add_plugins(ShellPlugin::new(
            ShellConfig::new().with_working_dir(&working_dir),
        ))
        .add_plugins(TaskToolsPlugin::new(
            BackgroundTasks::new(std::time::Duration::from_secs(120)).0,
        ))
        .add_plugins(WebToolsPlugin::default())
        .add_plugins(PersistencePlugin);
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = ctx.get_resource::<ToolRegistry>().unwrap();
    let mut names: Vec<String> = registry.names().into_iter().map(String::from).collect();
    names.sort_unstable();
    names
}

#[tokio::test]
async fn every_registered_tool_declares_whether_it_only_reads() {
    let registered = registered_tool_names().await;

    let mut classified: Vec<String> = CLASSIFICATION
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    classified.sort_unstable();

    assert_eq!(
        registered, classified,
        "a registered tool is missing from the read-only classification (or the \
         classification names a tool that is no longer registered). Add it to \
         CLASSIFICATION here and to tools::tool_is_read_only; until then the \
         agent treats it as mutating and any batch containing it runs \
         sequentially."
    );
}

#[tokio::test]
async fn the_classification_agrees_with_the_predicate_the_agent_reads() {
    for (name, expected_read_only) in CLASSIFICATION {
        assert_eq!(
            tool_is_read_only(name),
            *expected_read_only,
            "tools::tool_is_read_only disagrees with this test's classification \
             of `{name}`; one of the two is wrong, and if the predicate is the \
             wrong one the agent may be racing a tool that mutates"
        );
    }
}

#[test]
fn an_unclassified_tool_is_treated_as_mutating() {
    // The fail-closed property the concurrent lane leans on: forgetting to
    // classify a new tool costs latency, never safety.
    assert!(
        !tool_is_read_only("a_tool_that_has_not_been_written_yet"),
        "tool_is_read_only must default to `false`, so an unclassified tool \
         falls back to the sequential lane"
    );
}
