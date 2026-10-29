//! [`ExtractionPlugin`] — the in-memory extraction backend.
//!
//! Source-agnostic: claims the receiver from [`TraceIngestPlugin`] and runs
//! the per-trace [`ExtractionAgent`] on every message. Transports (stdin,
//! file tail, OTLP, Claude Code) push into the dispatcher; this plugin
//! drains it.
//!
//! # Pipeline
//!
//! ```text
//!  (source plugins)
//!         │  TraceIngestAPI::push(IOMessage)
//!         ▼
//!  TraceIngestPlugin (mpsc) ──► worker ──► sessions.run_oneshot(ExtractionAgent)
//!                                                  │
//!                                                  ▼
//!                                           KnowledgeGraph (global)
//! ```
//!
//! The worker drains the channel serially — one Haiku call per message.
//! Parallel fan-out via [`tokio::task::JoinSet`] is a configuration knob to
//! add once rate limits start mattering.

use crate::agents::extraction::{AgentConfig, ExtractionAgent};
use crate::agents::seed::SeedOntology;
use crate::plugins::trace_ingest::{TraceIngestRx, trace_line_from_message};
use crate::resources::aliases::Aliases;
use crate::resources::kg::KnowledgeGraph;
use crate::resources::ontology_gap::OntologyGap;
use crate::resources::sightings::Sightings;
use crate::resources::spend::{Pricing, Spend};
use polaris::sessions::SessionsAPI;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;

/// Default Polaris model id for the extractor — Haiku 4.5 is cheap, fast,
/// and follows structured-output schemas well enough for triple extraction.
pub const DEFAULT_MODEL_ID: &str = "anthropic/claude-haiku-4-5";

/// Naive in-memory extraction backend: runs the per-trace
/// [`ExtractionAgent`] on every message pulled from [`TraceIngestPlugin`],
/// accumulating triples into a shared in-memory [`KnowledgeGraph`].
///
/// Reach for this backend when you want triple extraction with zero external
/// dependencies — no SPARQL store, no on-disk database — and an in-memory KG
/// you can serialize to RDF Turtle via [`crate::dump::write_turtle`]. It is
/// the backend used by `examples/stdin_demo.rs`. For a persistent,
/// SPARQL-queryable store with SHACL conformance, register the
/// `OxigraphExtractionPlugin` (feature `oxigraph`) instead. Exactly one
/// extraction backend may be registered per server (see
/// [Lifecycle](#lifecycle)).
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`KnowledgeGraph`] | Global | Append-only in-memory temporal KG with interior mutability — the durable extraction target. |
/// | [`Aliases`] | Global | Cross-trace canonicalization table; drives `owl:sameAs` emission. Seeded via [`Self::with_aliases`]. |
/// | [`Sightings`] | Global | Per-entity first/last observation tracker for surface-form drift. |
/// | [`OntologyGap`] | Global | Accumulated catchall / uncurated-predicate signal for ontology iteration. |
/// | [`Spend`] | Global | Per-model token / USD accumulator updated after every LLM call. |
///
/// # APIs Provided
///
/// _none_ — this plugin is a terminal backend. It consumes the dispatcher's
/// receiver and writes globals; nothing downstream depends on an API it
/// exposes.
///
/// # Dependencies
///
/// Declared by capability in [`Plugin::access`]:
///
/// - requires [`TraceIngestRx`] — the receiver half of the ingest channel,
///   provided by [`TraceIngestPlugin`]. Claimed (via `take`) in
///   [`Plugin::ready`].
/// - requires [`SessionsAPI`] — provided by
///   [`SessionsPlugin`](polaris::sessions::SessionsPlugin); used to register
///   and run the per-trace agent.
///
/// In a full server [`SessionsPlugin`](polaris::sessions::SessionsPlugin) in
/// turn needs [`ModelsPlugin`](polaris::models::ModelsPlugin) +
/// [`AnthropicPlugin`](polaris::models::AnthropicPlugin) (so the agent's
/// Haiku calls resolve) and
/// [`PersistencePlugin`](polaris::plugins::PersistencePlugin); see
/// `examples/stdin_demo.rs` for the complete wiring.
///
/// # Lifecycle
///
/// In [`Plugin::build`] the plugin inserts its global resources. In
/// [`Plugin::ready`] it registers [`ExtractionAgent`], claims the ingest
/// receiver with [`TraceIngestRx::take`], and spawns a Tokio worker that
/// drains the channel serially — one [`SessionsAPI::run_oneshot`] (and thus
/// one Haiku call) per message, blocking the next line until it returns.
/// Because the receiver is single-consumer, registering a second extraction
/// backend makes the second `take` panic.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris::plugins::PersistencePlugin;
/// use polaris::sessions::SessionsPlugin;
/// use polaris::sessions::store::memory::InMemoryStore;
/// use polaris::system::server::Server;
/// use polaris_ontrace::{ExtractionPlugin, StdinIOPlugin, TraceIngestPlugin};
///
/// # async fn example() {
/// let mut server = Server::new();
/// server
///     // PersistencePlugin + ModelsPlugin/AnthropicPlugin are also needed
///     // for a runnable server — see examples/stdin_demo.rs.
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(TraceIngestPlugin::new())
///     .add_plugins(ExtractionPlugin::new())
///     .add_plugins(StdinIOPlugin::new());
/// server.finish().await.unwrap();
/// # }
/// ```
pub struct ExtractionPlugin {
    model_id: String,
    seed: Option<SeedOntology>,
    alias_seeds: Vec<(String, String)>,
    pricing: Pricing,
}

impl ExtractionPlugin {
    /// Construct a plugin with the default model ([`DEFAULT_MODEL_ID`]) and
    /// the default Anthropic [`Pricing`] table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model_id: DEFAULT_MODEL_ID.to_owned(),
            seed: None,
            alias_seeds: Vec::new(),
            pricing: Pricing::default(),
        }
    }

    /// Override the model used by the extraction agent.
    ///
    /// Accepts any Polaris model id whose provider is registered on the
    /// server — e.g. `"anthropic/claude-haiku-4-5"`,
    /// `"anthropic/claude-sonnet-4-6"`.
    #[must_use]
    pub fn with_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = model_id.into();
        self
    }

    /// Attach a curated [`SeedOntology`] to every per-trace session.
    ///
    /// The worker clones the seed into each session's context, where
    /// [`ExtractionAgent`]'s `setup` reads it and builds the curated
    /// [`crate::agents::extraction::OntologyContext`] used by the LLM
    /// prompt and JSON-schema enum. Without a seed the extractor runs in
    /// free-form mode.
    #[must_use]
    pub fn with_seed(mut self, seed: SeedOntology) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Seed the [`Aliases`] table with hand-curated `(alias, target)` pairs.
    ///
    /// Each pair pre-registers the alias side under the target as canonical,
    /// so any later observation whose normalized form matches the alias
    /// triggers a one-shot `owl:sameAs` emission to the target — collapsing
    /// surface-form drift (`assistant` ↔ `claude`, tool-name casings,
    /// shorthand) without losing observational provenance. Pairs accumulate
    /// across calls. Typically populated from
    /// [`crate::resources::aliases::load_alias_seeds`].
    #[must_use]
    pub fn with_aliases<I, A, T>(mut self, seeds: I) -> Self
    where
        I: IntoIterator<Item = (A, T)>,
        A: Into<String>,
        T: Into<String>,
    {
        self.alias_seeds
            .extend(seeds.into_iter().map(|(a, t)| (a.into(), t.into())));
        self
    }

    /// Replace the [`Pricing`] table used by the registered [`Spend`].
    ///
    /// The default is [`Pricing::anthropic_defaults`] — override when you
    /// run a model whose id isn't in that table, or when you want to track
    /// a negotiated rate. Has no effect once `build` has run.
    #[must_use]
    pub fn with_pricing(mut self, pricing: Pricing) -> Self {
        self.pricing = pricing;
        self
    }
}

impl Default for ExtractionPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ExtractionPlugin {
    const ID: &'static str = "ontrace::extraction";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Claims the trace-ingest receiver in `ready()` and runs the per-trace
        // agent through `SessionsAPI`. Both are required capabilities, so the
        // resolver orders this backend after `TraceIngestPlugin` and
        // `SessionsPlugin` (build *and* ready) and errors at `finish()` if
        // either provider is missing — replacing the old runtime `.expect()`.
        PluginAccess::new()
            .requires::<TraceIngestRx>(VersionReq::caret(TraceIngestRx::CONTRACT_VERSION))
            .requires::<SessionsAPI>(VersionReq::caret(SessionsAPI::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        server.insert_global(KnowledgeGraph::new());
        if self.alias_seeds.is_empty() {
            server.insert_global(Aliases::new());
        } else {
            server.insert_global(Aliases::with_seeds(self.alias_seeds.clone()));
        }
        server.insert_global(Sightings::new());
        server.insert_global(OntologyGap::new());
        server.insert_global(Spend::with_pricing(self.pricing.clone()));
    }

    async fn ready(&self, server: &mut Server) {
        let sessions = server
            .api::<SessionsAPI>()
            .expect("ExtractionPlugin requires SessionsPlugin")
            .clone();

        sessions
            .register_agent(ExtractionAgent)
            .expect("ExtractionAgent failed graph validation");

        let agent_type = sessions
            .find_agent_type(ExtractionAgent::NAME)
            .expect("just-registered agent must be findable");

        let mut rx = server
            .api::<TraceIngestRx>()
            .expect("ExtractionPlugin requires TraceIngestPlugin")
            .take();
        let model_id = self.model_id.clone();
        let seed = self.seed.clone();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let Some(line) = trace_line_from_message(&message) else {
                    continue;
                };
                let model_id = model_id.clone();
                let seed = seed.clone();
                let result: Result<usize, _> = sessions
                    .run_oneshot(&agent_type, move |ctx| {
                        ctx.insert(line);
                        ctx.insert(AgentConfig::new(model_id));
                        if let Some(s) = seed {
                            ctx.insert(s);
                        }
                    })
                    .await;
                match result {
                    Ok(count) => tracing::debug!(triples = count, "extracted"),
                    Err(err) => tracing::warn!(error = %err, "extraction failed"),
                }
            }
            tracing::info!("trace channel closed; extraction worker exiting");
        });
    }
}
