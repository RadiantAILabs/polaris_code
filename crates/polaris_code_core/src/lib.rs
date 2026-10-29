//! polaris-code — a terminal coding agent built on the Polaris stack.
//!
//! This crate composes the Polaris framework with three companion crates from
//! this workspace into a single coding agent:
//!
//! | Dependency | Role |
//! |------------|------|
//! | [`polaris`] | Agent runtime — `Server`, plugins, systems, `SessionsAPI`, LLM providers, the `#[tool]`/`#[toolset]` macros. |
//! | [`polaris_tui`] | Composable ratatui terminal UI — panels contributed to a shared registry. |
//! | [`polaris_ontrace`] | Ontology-driven tools/resources/agents — an oxigraph RDF store plus a SPARQL toolset, for modelling the codebase as a queryable knowledge graph. |
//! | [`polaris_cosmos`] | Domain-neutral cognition — the topology catalog and admission boundary, selection policy points, the evidence contract, and the shared value vocabulary. |
//!
//! See `docs/philosophy.md` for the design principles every component follows.
//!
//! # Layout
//!
//! - [`tools`] — toolsets the agent can call, each registered with the
//!   [`ToolRegistry`](polaris::tools::ToolRegistry) by its own plugin. Ships
//!   [`tools::CodeTools`] (`read_file`, `edit_file`, `search`, `list_files`) and
//!   [`tools::CodeGraphTools`] (`find_definition`, `callers_of`, … over the Code
//!   `ABox`).
//! - [`agents`] — the reflective [`CodeAgent`](agents::CodeAgent): a `ReAct` loop
//!   whose control decisions read a per-turn policy snapshot and are reified
//!   into the Session `ABox` knowledge graph.
//! - [`config`] — host configuration resolved at launch: [`ModelCatalog`], the
//!   `provider/model` ids one run may select between, the default it starts
//!   on, and the resolver behind `--model` and `/model`.
//! - [`sparql`] — shared SPARQL string helpers ([`esc`](sparql::esc) for literals,
//!   [`enc`](sparql::enc) for IRI components) used by both graph writers.
//! - [`usage`] — session token accounting + cost estimation ([`TokenMeter`]),
//!   and [`RunUsage`], the versioned accounting a non-interactive run emits.
//! - [`progress`] — graph instrumentation bridged to the conversation panel
//!   ([`ProgressMiddlewarePlugin`]).
//! - [`terminal`] — the coding-side policy behind the `polaris-tui`
//!   conversation panel: the `/command` registry and its dispatch decorator
//!   ([`terminal::CommandRegistry`], [`terminal::CommandDispatch`]), which run
//!   commands locally and never as model requests, plus the `@` file-mention
//!   source over the working directory ([`terminal::WorkspaceFiles`]).
//! - [`cosmos`] — the *coding embodiment* of the `polaris_cosmos` contracts:
//!   coding task profiling ([`cosmos::task`]), real edit actions and verifiers
//!   ([`cosmos::edit`], [`cosmos::verify`]), seven coding topologies
//!   ([`cosmos::graphs`]) registered against one contract
//!   ([`cosmos::topology`]), the author priors selection ranks
//!   ([`cosmos::policy`]), the offline end-to-end driver ([`cosmos::proof`]),
//!   and the live one ([`cosmos::host`]) the run surface calls at its
//!   selection point. Cosmos supplies contracts; this crate supplies the
//!   payload, the sandbox, the verifiers, and graph execution.
//! - [`run`] — the run surface: [`run::drive_turn`] runs one turn,
//!   [`run::drive_until`] loops it. The interactive and non-interactive front
//!   ends in `main.rs` are two thin callers over these same functions; there
//!   is exactly one agent loop. Its first act each turn is the
//!   topology-selection point ([`cosmos::host::select_and_run`]), inert unless
//!   the host accepted a coding task — see `docs/topology-selection.md`. The
//!   same module supplies the resilience pieces a front end composes around
//!   that loop and nothing owns for it: [`run::persist_turn`] (save after
//!   each turn, under the session that turn ran in),
//!   [`run::stop_signal`]/[`run::shutdown_driver`] (SIGINT and SIGTERM, then
//!   the one safe quiesce/abort/save order), and
//!   [`run::DriverStep`]/[`run::FailureBudget`] (whether a failed turn goes
//!   back to the prompt, and when to stop trying).
//! - [`state`] — the agent's own state directory inside a workspace
//!   ([`state::state_dir`]): where saved sessions and the rewind journal live,
//!   and the self-ignore marker that keeps them out of anything git builds from
//!   the working tree.
//! - [`docs::embedding`] — the entry point for a program that is **not** the
//!   `polaris-code` binary: what an embedder must supply, what a turn hands
//!   back, and what the binary's composition root keeps to itself. Proved by
//!   `tests/embedding_entry_point.rs` and `examples/embed_turn.rs`.
//! - [`exec`] — the non-interactive front end: [`exec::ExecIOPlugin`] installs
//!   a channel-backed `UserIO` instead of the TUI panels, and
//!   [`exec::report`] renders what the run prints — the `--output-format`
//!   vocabulary, the identity triple, the run's token and cost accounting,
//!   and the exit code.

pub mod agents;
pub mod config;
pub mod cosmos;
pub mod exec;
pub mod progress;
pub mod run;
pub mod sparql;
pub mod state;
pub mod terminal;
pub mod tools;
pub mod usage;

/// Discoverability catalogs — indexes of every plugin, API, and
/// consumer-facing resource this crate ships, each linking to the item's
/// rustdoc — plus the embedding guide for callers outside this crate.
///
/// These are doc-only modules. The catalogs are authored in
/// `src/docs/{plugins,apis,resources}.md` and kept in sync with the source by
/// the drift guards in `tests/{plugin,api,resource}_catalog.rs`; the embedding
/// guide is authored in `src/docs/embedding.md` and its code blocks are
/// compiled as doctests. See the reference standards under `docs/reference/`
/// for what each item's rustdoc must contain.
pub mod docs {
    #[doc = include_str!("docs/embedding.md")]
    pub mod embedding {}

    #[doc = include_str!("docs/plugins.md")]
    pub mod plugins {}

    #[doc = include_str!("docs/apis.md")]
    pub mod apis {}

    #[doc = include_str!("docs/resources.md")]
    pub mod resources {}
}

pub use agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager, Decision, LoopState, PolicySnapshot,
    ProjectContext,
};
pub use config::{ModelCatalog, ModelSelectionError};
pub use exec::ExecIOPlugin;
pub use progress::ProgressMiddlewarePlugin;
pub use run::{CEILING_EXIT_CODE, TurnOutcome, TurnReason};
pub use tools::{
    CodeGraphPlugin, CodeGraphTools, CodeIndexState, CodeIndexer, CodeTools, CodeToolsConfig,
    CodeToolsPlugin, IndexPhase, code_index_state, tool_is_read_only,
};
pub use usage::{
    CeilingStatus, InvalidCeiling, Price, RunCeiling, RunUsage, TokenMeter, price_for,
};
