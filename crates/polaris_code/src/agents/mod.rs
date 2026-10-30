//! Agent graphs the coding agent runs.
//!
//! An agent is an implementation of the [`Agent`] contract: `build(&mut Graph)`
//! wires the systems and control flow, `name()` identifies it, and `setup()`
//! seeds per-session resources. A thin
//! [`Plugin`](polaris::system::plugin::Plugin) registers the agent on the
//! [`SessionsAPI`](polaris::sessions::SessionsAPI).
//!
//! To add one: create `agents/<name>.rs` with a `struct` that `impl Agent`, add
//! a `<Name>Plugin` that registers it (and any local resources its systems
//! need), then `mod <name>;` and re-export here.
//!
//! The reflective coding agent: a `ReAct` loop whose every control decision reads
//! a [`PolicySnapshot`] and is reified into the Session `ABox` knowledge graph.

pub mod auto_review;
pub mod code_agent;
pub mod config;
pub mod context;
pub mod failure;
pub mod permission_rules;
pub mod policy;
pub mod project_context;
pub mod retry;
pub mod state;
pub mod trajectory;

pub use polaris::agent::{Agent, SetupError};

pub use code_agent::{AgentLlm, CodeAgent, CodeAgentPlugin, DEFAULT_MAX_TURNS};
pub use config::AgentConfig;
pub use context::{ContextManager, ContextStrategy, LinearWindow};
pub use failure::{FailureKind, TurnFailure};
pub use policy::{
    Approver, PaletteMode, PermissionCeiling, PermissionMode, PermissionPolicy, PolicySnapshot,
    ToolPalettePolicy, UnattendedGrants,
};
pub use project_context::{ProjectContext, load_seed_ontologies};
pub use state::LoopState;
pub use trajectory::{Decision, SESSION_GRAPH_IRI, record_tool_call};
