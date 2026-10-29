//! [`InspectPlugin`] — interactive SPARQL-driven KG inspector.
//!
//! Registers [`InspectAgent`] on the server's [`SessionsAPI`] and publishes
//! [`InspectAPI`] for callers (typically `src/bin/ontrace.rs` in `inspect`
//! mode) to ask one question per round-trip via [`InspectAPI::ask`].
//!
//! Unlike the extraction plugins, this one does **not** claim
//! [`crate::plugins::trace_ingest::TraceIngestRx`] — inspect mode runs over a
//! pre-loaded store, not a live trace stream. The two modes are mutually
//! exclusive: pick one when assembling the server.
//!
//! [`InspectAgent`]: crate::agents::inspect::InspectAgent

use crate::agents::extraction::AgentConfig;
use crate::agents::inspect::{InspectAgent, InspectConfig, InspectInput};
use crate::oxigraph::OxigraphStore;
use crate::resources::spend::{Pricing, Spend};
use polaris::sessions::{AgentTypeId, SessionsAPI};
use polaris::system::api::API;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use polaris::tools::ToolRegistry;

/// Default model id for the inspect agent.
///
/// Sonnet rather than Haiku — tool-using investigation benefits from the
/// stronger model, and the operator is interactive anyway so the latency
/// difference is acceptable.
pub const DEFAULT_INSPECT_MODEL_ID: &str = "anthropic/claude-sonnet-4-6";

/// Interactive SPARQL-driven knowledge-graph inspector (feature: `oxigraph`).
///
/// Register this for *inspect mode* — querying a pre-loaded
/// [`OxigraphStore`] in natural language. It registers [`InspectAgent`] on the
/// [`SessionsAPI`] and publishes [`InspectAPI`] so a caller (typically
/// `src/bin/ontrace.rs` in `inspect` mode) can ask one question per
/// round-trip. Mutually exclusive with the extraction backends: those claim
/// the live trace stream via `TraceIngestRx`, this one reads a static store.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`Spend`] | Global | Per-model token / USD accumulator. Inserted only if no extraction plugin already provided one. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`InspectAPI`] | Pre-bound `ask(question) -> answer` helper over the registered agent. |
///
/// # Dependencies
///
/// Declared by capability in [`access`](Plugin::access) — all `requires`:
///
/// - [`SessionsAPI`] (from [`SessionsPlugin`](polaris::sessions::SessionsPlugin)) — agent registration + session execution.
/// - [`ToolRegistry`] (from [`ToolsPlugin`](polaris::tools::ToolsPlugin)) — the agent reads `Res<ToolRegistry>` to drive tool calls.
/// - [`OxigraphStore`] (from [`OxigraphPlugin`](crate::oxigraph::OxigraphPlugin)) — the SPARQL tools the agent invokes are registered there; the requirement orders this plugin after it.
///
/// # Lifecycle
///
/// Feature-gated behind `oxigraph`. [`build`](Plugin::build) inserts a
/// [`Spend`] global only if none exists yet (extraction plugins usually win
/// the race). [`ready`](Plugin::ready) clones the [`SessionsAPI`], registers
/// [`InspectAgent`] (panics on graph-validation failure), looks up its
/// [`AgentTypeId`], and inserts [`InspectAPI`]. No background tasks are
/// spawned — sessions run on demand per [`InspectAPI::ask`] call.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris::plugins::PersistencePlugin;
/// use polaris::sessions::SessionsPlugin;
/// use polaris::sessions::store::memory::InMemoryStore;
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_ontrace::oxigraph::OxigraphPlugin;
/// use polaris_ontrace::InspectPlugin;
///
/// # async fn example() {
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(ToolsPlugin)
///     .add_plugins(OxigraphPlugin::in_memory().with_tools(true))
///     .add_plugins(InspectPlugin::new());
/// server.finish().await.unwrap();
/// # }
/// ```
pub struct InspectPlugin {
    model_id: String,
    pricing: Pricing,
    max_iterations: Option<usize>,
}

impl InspectPlugin {
    /// Construct with [`DEFAULT_INSPECT_MODEL_ID`] and default pricing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model_id: DEFAULT_INSPECT_MODEL_ID.to_owned(),
            pricing: Pricing::default(),
            max_iterations: None,
        }
    }

    /// Override the model the inspect agent talks to.
    #[must_use]
    pub fn with_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = model_id.into();
        self
    }

    /// Override the [`Pricing`] table used by [`Spend`] when this plugin
    /// owns the global. Ignored when an extraction plugin already provided
    /// one — that registration wins (extraction usually runs first).
    #[must_use]
    pub fn with_pricing(mut self, pricing: Pricing) -> Self {
        self.pricing = pricing;
        self
    }

    /// Override the LLM ↔ tool round-trip cap. Defaults to
    /// [`crate::agents::inspect::DEFAULT_MAX_ITERATIONS`].
    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = Some(max_iterations);
        self
    }
}

impl Default for InspectPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for InspectPlugin {
    const ID: &'static str = "ontrace::inspect";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Requires the sessions runtime (agent registration + execution), the
        // tool registry the inspect agent drives, and the `OxigraphStore`. The
        // store requirement orders this plugin after `OxigraphPlugin`, which
        // registers the SPARQL toolset the agent calls.
        PluginAccess::new()
            .requires::<SessionsAPI>(VersionReq::caret(SessionsAPI::CONTRACT_VERSION))
            .requires::<ToolRegistry>(VersionReq::caret(ToolRegistry::CONTRACT_VERSION))
            .requires::<OxigraphStore>(VersionReq::caret(OxigraphStore::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        // Only own `Spend` if nobody else does — extraction plugins typically
        // beat us to it, and we want to keep recording into the same global.
        if server.get_global::<Spend>().is_none() {
            server.insert_global(Spend::with_pricing(self.pricing.clone()));
        }
    }

    async fn ready(&self, server: &mut Server) {
        let sessions = server
            .api::<SessionsAPI>()
            .expect("InspectPlugin requires SessionsPlugin")
            .clone();

        sessions
            .register_agent(InspectAgent)
            .expect("InspectAgent failed graph validation");

        let agent_type = sessions
            .find_agent_type(InspectAgent::NAME)
            .expect("just-registered agent must be findable");

        server.insert_api(InspectAPI {
            sessions,
            agent_type,
            model_id: self.model_id.clone(),
            max_iterations: self.max_iterations,
        });
    }
}

/// Pre-bound `ask(question) -> answer` helper over the registered
/// [`InspectAgent`].
///
/// Fetch it to query a loaded knowledge graph in natural language — one
/// question per round-trip. Each [`Self::ask`] spawns a fresh ephemeral
/// session, so questions don't share conversation history; cross-question
/// memory would belong in a `GlobalResource`, not on the conversation thread.
///
/// # Provided by
///
/// [`InspectPlugin`].
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`ask`](Self::ask) | `async`; pose one question, run the agent's tool loop, and return the final answer text. |
///
/// # Lifecycle
///
/// Available after [`InspectPlugin::ready`] has registered the agent and
/// inserted this API — i.e. after [`Server::finish`]. Calling `ask` before
/// then is impossible (the API is not yet fetchable). The bound
/// [`AgentTypeId`] stays valid for the server's lifetime.
///
/// # Composition
///
/// Provider-scoped. One [`InspectPlugin`] inserts one [`InspectAPI`]; the
/// handle is [`Clone`], so multiple callers may hold it and ask concurrently,
/// each getting an independent ephemeral session.
///
/// # Example consumers
///
/// The `ontrace inspect` CLI subcommand (`src/bin/ontrace.rs`) fetches this
/// API and calls [`ask`](Self::ask) — once for `--ask <q>` one-shot mode, or
/// repeatedly from the TUI chat panel.
///
/// # Example
///
/// Provider side ([`InspectPlugin::ready`] inserts the API):
///
/// ```ignore
/// server.insert_api(InspectAPI { sessions, agent_type, model_id, max_iterations });
/// ```
///
/// Consumer side:
///
/// ```no_run
/// # use polaris::system::server::Server;
/// # use polaris_ontrace::InspectAPI;
/// # async fn example(server: &Server) {
/// let api = server.api::<InspectAPI>().expect("InspectPlugin not registered");
/// let answer = api.ask("which agents called the bash tool?").await.unwrap();
/// println!("{answer}");
/// # }
/// ```
#[derive(Clone)]
pub struct InspectAPI {
    sessions: SessionsAPI,
    agent_type: AgentTypeId,
    model_id: String,
    max_iterations: Option<usize>,
}

impl API for InspectAPI {}

impl InspectAPI {
    /// Pose one question to the inspect agent and wait for the final answer.
    ///
    /// Returns whatever text the LLM produced once it stopped emitting tool
    /// calls (or hit the iteration cap). Tool failures are folded into the
    /// conversation as `ToolResult` error blocks, not propagated — the LLM
    /// gets a chance to recover.
    ///
    /// # Errors
    ///
    /// Returns the underlying `SessionsAPI::run_oneshot` error if the session
    /// itself fails to execute (e.g. graph wiring error or system panic).
    pub async fn ask(
        &self,
        question: impl Into<String>,
    ) -> Result<String, polaris::sessions::SessionError> {
        let question = question.into();
        let model_id = self.model_id.clone();
        let max_iterations = self.max_iterations;
        self.sessions
            .run_oneshot::<String>(&self.agent_type, move |ctx| {
                ctx.insert(InspectInput::new(question));
                ctx.insert(AgentConfig::new(model_id));
                if let Some(n) = max_iterations {
                    ctx.insert(InspectConfig::new(n));
                }
            })
            .await
    }
}
