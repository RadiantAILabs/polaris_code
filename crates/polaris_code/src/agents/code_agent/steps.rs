//! Startup extensions to the coding loop as ordinary graph nodes.
//!
//! Plugins fill [`CodeAgentSteps`] during `build`. After startup, the host calls
//! [`CodeAgent::with_steps`] once and registers that agent instead of the unit
//! `CodeAgent`. Pass the same agent to the graph-view plugin so contributed
//! systems appear by name in the graph.
//!
//! Each place sorts contributions by ascending `(priority, key)`, independent
//! of plugin registration order. A duplicate key at the same place is rejected.
//! Builders append systems using the ordinary [`Graph`] API; each builder must
//! append a finite chain that returns control to the agent. Their graph inputs
//! and errors have the same validation and recovery semantics as stock nodes.
//!
//! After-tools systems run once after the entire batch's results have entered
//! [`ContextManager`], including denied/error results. They can append messages
//! there and consume [`AgentConfig`](super::AgentConfig) to locate the working
//! tree. Before-finish systems run after the tool-free assistant response has
//! entered the context. Any system can [`FinishGate::hold`] the answer. All
//! systems still run; then each hold is appended as a separate user message, in
//! contribution order, after that answer. Subsequent checks therefore see the
//! same candidate answer, without earlier hold messages in the context.
//!
//! A hold continues the existing agent loop; each additional answer uses another
//! iteration and a normally metered model call. Existing turn limits, provider
//! output limits, context projection/compaction, spend admission, and executor
//! cancellation still apply. A hold grants no extra budget. Extension systems
//! that wait on I/O must race those awaits against `Res<RunCancellation>`, as
//! stock systems do, and must meter any model calls they make.
//! These are trusted host plugins: filesystem mutations must use the existing
//! permission, containment, edit-journal and index-refresh contracts, just as a
//! new tool capability must. This seam does not authorize filesystem access.

//!
//! # Example plugin and host wiring
//!
//! ```no_run
//! use std::sync::Arc;
//! use polaris::models::ModelsPlugin;
//! use polaris::models::llm::{LlmResponse, Message};
//! use polaris::plugins::PersistencePlugin;
//! use polaris::sessions::{InMemoryStore, SessionsAPI, SessionsPlugin};
//! use polaris::system::param::{Out, ResMut};
//! use polaris::system::plugin::{Plugin, PluginId, Version};
//! use polaris::system::server::Server;
//! use polaris::system::system;
//! use polaris::tools::ToolsPlugin;
//! use polaris_code::agents::{CodeAgent, CodeAgentPlugin, ContextManager};
//! use polaris_code::agents::code_agent::steps::{CodeAgentSteps, FinishGate};
//!
//! #[system]
//! async fn note_tool_round(mut context: ResMut<ContextManager>) {
//!     context.push(Message::user("The tool round has completed."));
//! }
//!
//! #[system]
//! async fn check_candidate(answer: Out<LlmResponse>, mut gate: ResMut<FinishGate>) {
//!     if answer.text().contains("work remains") {
//!         gate.hold("Please complete the remaining work before finishing.");
//!     }
//! }
//!
//! struct ExtraSteps;
//! impl Plugin for ExtraSteps {
//!     const ID: &'static str = "example::extra_steps";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!     fn dependencies(&self) -> Vec<PluginId> {
//!         vec![PluginId::of::<CodeAgentPlugin>()]
//!     }
//!     fn build(&self, server: &mut Server) {
//!         let mut steps = server.get_resource_mut::<CodeAgentSteps>().unwrap();
//!         steps.after_tools("example.note", 0, |g| {
//!             g.add_system(note_tool_round);
//!         }).unwrap();
//!         steps.before_finish("example.check", 0, |g| {
//!             g.add_system(check_candidate);
//!         }).unwrap();
//!     }
//! }
//!
//! # async fn assemble() -> Result<(), Box<dyn std::error::Error>> {
//! let mut server = Server::new();
//! server.add_plugins(ModelsPlugin)
//!     // Register your model provider as well, before creating a session.
//!     .add_plugins(ToolsPlugin)
//!     .add_plugins(PersistencePlugin)
//!     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
//!     .add_plugins(CodeAgentPlugin::new())
//!     .add_plugins(ExtraSteps);
//! // For a TUI, also install GraphPanelPlugin and
//! // GraphViewPlugin::for_code_agent_steps() before finish. It reads the
//! // completed registry in ready, after every plugin has built.
//! server.finish().await?;
//! let steps = server.get_global::<CodeAgentSteps>().unwrap();
//! let agent = CodeAgent::with_steps(&steps);
//! server.api::<SessionsAPI>().unwrap().register_agent(agent)?;
//! // Create and drive sessions normally, using CodeAgent::NAME.
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use polaris::agent::{Agent, SetupError};
use polaris::graph::Graph;
use polaris::models::llm::{LlmResponse, Message};
use polaris::plugins::UserIO;
use polaris::system::param::{Out, Res, ResMut, SystemContext};
use polaris::system::resource::{GlobalResource, LocalResource};
use polaris::system::system;

use super::{CodeAgent, ContextManager, LoopState, PreparedContext, build_graph, finalize};
use crate::agents::trajectory::Decision;
use crate::terminal::transcript::{LiveTranscript, OUTCOME_COMPLETE};
use polaris_ontrace::oxigraph::OxigraphStore;

type Builder = Arc<dyn Fn(&mut Graph) + Send + Sync>;
type Contributions = BTreeMap<(i32, String), Builder>;

/// Startup registry for plugins that add graph steps to the coding agent.
///
/// # Scope
///
/// **Global.** Contributions are host configuration shared by every session;
/// each [`SteppedCodeAgent`] snapshots them before its graph is built.
///
/// # Provided By
///
/// [`CodeAgentPlugin`](super::CodeAgentPlugin) installs a mutable startup registry
/// in `build` and publishes a frozen global snapshot in `ready`.
/// Contributing plugins must depend on it and mutate the registry in `build`.
///
/// # Access
///
/// Use `server.get_resource_mut::<CodeAgentSteps>()` during plugin `build`.
/// After `Server::finish`, read the frozen snapshot with `server.get_global`.
/// The startup resource is removed when frozen. Runtime systems do not read
/// this registry. There is no interior mutability: changing a standalone
/// registry after an agent snapshots it cannot change that agent's graph.
///
/// # Alternatives
///
/// Use the unit [`CodeAgent`] for the stock graph, or Polaris's dynamic slots
/// when subgraphs must change at runtime instead of at startup.
///
/// # Example
///
/// ```
/// use polaris::system::{param::ResMut, system};
/// use polaris_code::agents::ContextManager;
/// use polaris_code::agents::code_agent::steps::{CodeAgentSteps, FinishGate};
///
/// #[system]
/// async fn observe(mut context: ResMut<ContextManager>) {
///     context.push(polaris::models::llm::Message::user("Tool round observed."));
/// }
/// #[system]
/// async fn check_answer(mut gate: ResMut<FinishGate>) {
///     gate.hold("Please verify the result before finishing.");
/// }
/// let mut steps = CodeAgentSteps::default();
/// steps.after_tools("example.observe", 0, |g| { g.add_system(observe); }).unwrap();
/// steps.before_finish("example.check", 0, |g| { g.add_system(check_answer); }).unwrap();
/// ```
#[derive(Clone, Default)]
pub struct CodeAgentSteps {
    after_tools: Contributions,
    before_finish: Contributions,
}

impl GlobalResource for CodeAgentSteps {}

/// A key already registered at one of the agent's extension places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateStep {
    key: String,
    place: &'static str,
}

impl std::fmt::Display for DuplicateStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "duplicate {} step key: {}", self.place, self.key)
    }
}

impl std::error::Error for DuplicateStep {}

impl CodeAgentSteps {
    /// Append a graph builder after each complete tool round.
    ///
    /// Builders run by ascending priority, then key; the builder's own system
    /// order is preserved. Use a namespaced key stable across plugin versions.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicateStep`] without changing the registry if this place
    /// already contains `key`, even at a different priority.
    pub fn after_tools(
        &mut self,
        key: impl Into<String>,
        priority: i32,
        build: impl Fn(&mut Graph) + Send + Sync + 'static,
    ) -> Result<(), DuplicateStep> {
        Self::insert(
            &mut self.after_tools,
            "after-tools",
            key.into(),
            priority,
            build,
        )
    }

    /// Append a graph builder before accepting each tool-free answer.
    ///
    /// Systems may consume `ResMut<FinishGate>` and call [`FinishGate::hold`].
    /// No hold means the stock `finalize` runs. Ordering is as in [`Self::after_tools`].
    ///
    /// # Errors
    ///
    /// Returns [`DuplicateStep`] without changing the registry if this place
    /// already contains `key`, even at a different priority.
    pub fn before_finish(
        &mut self,
        key: impl Into<String>,
        priority: i32,
        build: impl Fn(&mut Graph) + Send + Sync + 'static,
    ) -> Result<(), DuplicateStep> {
        Self::insert(
            &mut self.before_finish,
            "before-finish",
            key.into(),
            priority,
            build,
        )
    }

    fn insert(
        contributions: &mut Contributions,
        place: &'static str,
        key: String,
        priority: i32,
        build: impl Fn(&mut Graph) + Send + Sync + 'static,
    ) -> Result<(), DuplicateStep> {
        if contributions.keys().any(|(_, existing)| existing == &key) {
            return Err(DuplicateStep { key, place });
        }
        contributions.insert((priority, key), Arc::new(build));
        Ok(())
    }

    pub(super) fn build_after_tools(&self, graph: &mut Graph) {
        for build in self.after_tools.values() {
            build(graph);
        }
    }

    pub(super) fn build_before_finish(&self, graph: &mut Graph) {
        if self.before_finish.is_empty() {
            graph.add_system(finalize);
            return;
        }
        graph.add_system(begin_finish_checks);
        for build in self.before_finish.values() {
            build(graph);
        }
        graph.add_system(collect_finish_holds);
        graph.add_conditional_branch::<FinishStatus, _, _, _>(
            "finish_or_continue",
            |status| status.held,
            |held| {
                held.add_system(continue_after_hold);
            },
            |accepted| {
                accepted.add_system(finalize);
            },
        );
    }
}

/// Coding agent with a fixed snapshot of plugin-contributed graph nodes.
///
/// Construct with [`CodeAgent::with_steps`] after all plugins build, then use
/// wherever an [`Agent`] is accepted, including `SessionsAPI::register_agent`
/// and `GraphViewPlugin::for_agent`. Name and setup match the stock agent.
/// An empty snapshot builds exactly the stock graph, with no extra slot nodes.
#[derive(Clone)]
pub struct SteppedCodeAgent {
    steps: CodeAgentSteps,
}

impl SteppedCodeAgent {
    pub(super) fn new(steps: CodeAgentSteps) -> Self {
        Self { steps }
    }
}

impl Agent for SteppedCodeAgent {
    fn build(&self, graph: &mut Graph) {
        build_graph(graph, &self.steps);
    }

    fn name(&self) -> &'static str {
        CodeAgent::NAME
    }

    fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        CodeAgent.setup(ctx)?;
        if !self.steps.before_finish.is_empty() {
            ctx.insert(FinishGate {
                messages: Vec::new(),
            });
        }
        Ok(())
    }
}

/// Accumulates messages that hold the current candidate answer open.
///
/// # Scope
///
/// **Local.** Holds belong to one session's current tool-free model answer.
/// The agent clears them immediately before every before-finish chain.
///
/// # Provided By
///
/// [`SteppedCodeAgent::setup`] when at least one before-finish builder is present.
///
/// # Access
///
/// `ResMut<FinishGate>` calls [`Self::hold`]; `Res<FinishGate>` may inspect
/// [`Self::is_held`]. All checks run even after a hold. Only the agent can clear
/// holds, so a later plugin cannot undo an earlier plugin's veto.
///
/// # Alternatives
///
/// After-tools steps append directly to [`ContextManager`]; no finish gate is
/// needed there. Leaving this resource untouched accepts the candidate answer.
///
/// # Example
///
/// ```
/// use polaris::system::{param::ResMut, system};
/// use polaris_code::agents::code_agent::steps::FinishGate;
/// #[system]
/// async fn require_verification(mut gate: ResMut<FinishGate>) {
///     gate.hold("Run the requested verification and report its result.");
/// }
/// ```
pub struct FinishGate {
    messages: Vec<String>,
}

impl LocalResource for FinishGate {}

impl FinishGate {
    /// Hold this answer and append this message after it before calling the model again.
    /// Multiple calls keep every message in call order, including identical messages.
    pub fn hold(&mut self, message: impl Into<String>) {
        self.messages.push(message.into());
    }

    /// Whether any check has held the current answer.
    #[must_use]
    pub fn is_held(&self) -> bool {
        !self.messages.is_empty()
    }
}

#[derive(Clone)]
struct FinishStatus {
    held: bool,
}

#[system]
async fn begin_finish_checks(mut gate: ResMut<FinishGate>) {
    gate.messages.clear();
}

#[system]
async fn collect_finish_holds(
    gate: Res<FinishGate>,
    mut context: ResMut<ContextManager>,
) -> FinishStatus {
    for message in &gate.messages {
        context.push(Message::user(message.clone()));
    }
    FinishStatus {
        held: gate.is_held(),
    }
}

#[system]
async fn continue_after_hold(
    answer: Out<LlmResponse>,
    prepared: Out<PreparedContext>,
    user_io: Res<UserIO>,
    store: Res<OxigraphStore>,
    mut live: ResMut<LiveTranscript>,
) -> LoopState {
    live.close(&user_io, &answer.text(), OUTCOME_COMPLETE).await;
    Decision {
        kind: "termination",
        intent: None,
        options: vec!["complete".into(), "continue".into()],
        chosen: "continue".into(),
        policy_refs: vec!["before_finish.held".into()],
    }
    .record(&store, prepared.turn);
    LoopState {
        is_complete: false,
        failure: None,
    }
}

#[cfg(test)]
mod tests;
