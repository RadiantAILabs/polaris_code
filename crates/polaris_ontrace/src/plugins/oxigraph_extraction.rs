//! [`OxigraphExtractionPlugin`] — oxigraph-backed counterpart to
//! [`crate::ExtractionPlugin`].
//!
//! Wires the [`OxigraphExtractionAgent`] into a Polaris server and runs the
//! same per-trace worker loop as the naive in-memory plugin — but every
//! emitted triple lands in an [`OxigraphStore`]'s data (`ABox`) named graph
//! rather than the in-memory [`KnowledgeGraph`].
//!
//! Pick one or the other when assembling the server. Both backends consume
//! the same [`TraceIngestPlugin`] channel, so registering two would race for
//! the receiver — [`crate::plugins::trace_ingest::TraceIngestRx::take`]
//! panics on the second claim.
//!
//! # Dependencies
//!
//! - [`TraceIngestPlugin`] — provides the input channel.
//! - [`OxigraphPlugin`] — must be added first so the [`OxigraphStore`]
//!   global resource is present.
//! - [`SessionsPlugin`] — used to register and run the per-trace agent.
//!
//! [`KnowledgeGraph`]: crate::resources::kg::KnowledgeGraph

use crate::agents::extraction::{AgentConfig, OntologyPolicy};
use crate::agents::oxigraph_extraction::{OntologyConfig, OxigraphExtractionAgent};
use crate::agents::seed::SeedOntology;
use crate::oxigraph::OxigraphStore;
use crate::plugins::extraction::DEFAULT_MODEL_ID;
use crate::plugins::trace_ingest::{TraceIngestRx, trace_line_from_message};
use crate::resources::aliases::Aliases;
use crate::resources::ontology_gap::OntologyGap;
use crate::resources::sightings::Sightings;
use crate::resources::spend::{Pricing, Spend};
use polaris::sessions::SessionsAPI;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;

/// Oxigraph-backed extraction backend (feature `oxigraph`): runs the
/// per-trace [`OxigraphExtractionAgent`] on every message pulled from
/// [`TraceIngestPlugin`], accumulating quads in the [`OxigraphStore`]'s data
/// (`ABox`) named graph.
///
/// Reach for this backend when you want extracted triples in a persistent,
/// SPARQL-queryable RDF store with `TBox`/SHACL conformance — and the ability
/// to drive the `ontrace inspect` shell over the same store. For a
/// dependency-free in-memory KG instead, register
/// [`ExtractionPlugin`](crate::ExtractionPlugin). Exactly one extraction
/// backend may be registered per server (see [Lifecycle](#lifecycle)).
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`Aliases`] | Global | Cross-trace canonicalization table; drives `owl:sameAs` emission. Seeded via [`Self::with_aliases`]. |
/// | [`Sightings`] | Global | Per-entity first/last observation tracker for surface-form drift. |
/// | [`OntologyGap`] | Global | Accumulated catchall / uncurated-predicate / SHACL-violation signal for ontology iteration. |
/// | [`Spend`] | Global | Per-model token / USD accumulator updated after every LLM call. |
///
/// The extraction target itself — [`OxigraphStore`] — is **not** inserted
/// here; it is provided by [`OxigraphPlugin`](crate::oxigraph::OxigraphPlugin),
/// a required dependency (see below). This plugin only writes into it.
///
/// # APIs Provided
///
/// _none_ — this plugin is a terminal backend. It consumes the dispatcher's
/// receiver and writes the shared store and globals; nothing downstream
/// depends on an API it exposes.
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
/// - requires [`OxigraphStore`] — the global it writes quads into, provided
///   by [`OxigraphPlugin`](crate::oxigraph::OxigraphPlugin), which must be
///   added first. The requirement orders this backend after the store's
///   provider so the global is present both for the [`Self::with_seed`] `TBox`
///   load and for the worker's per-trace writes.
///
/// In a full server [`SessionsPlugin`](polaris::sessions::SessionsPlugin) in
/// turn needs [`ModelsPlugin`](polaris::models::ModelsPlugin) +
/// [`AnthropicPlugin`](polaris::models::AnthropicPlugin) (so the agent's
/// Haiku calls resolve) and
/// [`PersistencePlugin`](polaris::plugins::PersistencePlugin).
///
/// # Lifecycle
///
/// This whole plugin is gated behind the `oxigraph` cargo feature. In
/// [`Plugin::build`] it inserts its global resources (but not the store). In
/// [`Plugin::ready`] it: (1) if [`Self::with_seed`] was called, loads the
/// seed pack's Turtle into the [`OxigraphStore`]'s ontology (`TBox`) named
/// graph so the per-session
/// [`snapshot_ontology`](crate::agents::oxigraph_extraction::snapshot_ontology)
/// sees the curated shapes; (2) registers [`OxigraphExtractionAgent`];
/// (3) claims the ingest receiver with [`TraceIngestRx::take`]; and
/// (4) spawns a Tokio worker that drains the channel serially — one
/// [`SessionsAPI::run_oneshot`] (one Haiku call) per message, blocking the
/// next line until it returns. Because the receiver is single-consumer,
/// registering a second extraction backend makes the second `take` panic.
///
/// The default [`OntologyPolicy`] is [`OntologyPolicy::Strict`] — lines that
/// fit no curated predicate yield zero quads. [`Self::with_seed`] adopts the
/// pack's policy; chain [`Self::with_policy`] afterwards to override (e.g.
/// to an open-world [`OntologyPolicy::CatchAll`] sentinel).
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris::plugins::PersistencePlugin;
/// use polaris::sessions::SessionsPlugin;
/// use polaris::sessions::store::memory::InMemoryStore;
/// use polaris::system::server::Server;
/// use polaris_ontrace::oxigraph::OxigraphPlugin;
/// use polaris_ontrace::{OxigraphExtractionPlugin, StdinIOPlugin, TraceIngestPlugin};
///
/// # async fn example() {
/// let mut server = Server::new();
/// server
///     // PersistencePlugin + ModelsPlugin/AnthropicPlugin are also needed
///     // for a runnable server — see examples/stdin_demo.rs.
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(OxigraphPlugin::in_memory())
///     .add_plugins(TraceIngestPlugin::new())
///     .add_plugins(OxigraphExtractionPlugin::new())
///     .add_plugins(StdinIOPlugin::new());
/// server.finish().await.unwrap();
/// # }
/// ```
///
/// [`OxigraphStore`]: crate::oxigraph::OxigraphStore
pub struct OxigraphExtractionPlugin {
    model_id: String,
    policy: OntologyPolicy,
    seed_turtle: Option<String>,
    alias_seeds: Vec<(String, String)>,
    pricing: Pricing,
}

impl OxigraphExtractionPlugin {
    /// Construct with the default model ([`DEFAULT_MODEL_ID`]) and the
    /// default Anthropic [`Pricing`] table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model_id: DEFAULT_MODEL_ID.to_owned(),
            policy: OntologyPolicy::default(),
            seed_turtle: None,
            alias_seeds: Vec::new(),
            pricing: Pricing::default(),
        }
    }

    /// Override the model used by the extraction agent.
    #[must_use]
    pub fn with_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = model_id.into();
        self
    }

    /// Set the ontology-conformance policy used by the extractor.
    ///
    /// Default: [`OntologyPolicy::Strict`] — the LLM may only emit predicates
    /// from the curated ontology, and irrelevant lines yield zero triples.
    /// Use [`OntologyPolicy::CatchAll`] to allow a sentinel predicate for
    /// facts that don't fit a curated shape.
    ///
    /// Note: [`Self::with_seed`] already sets the policy from
    /// [`SeedOntology::policy`]; call this *after* `with_seed` if you want
    /// to override a pack's default policy.
    #[must_use]
    pub fn with_policy(mut self, policy: OntologyPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Attach a curated [`SeedOntology`] pack.
    ///
    /// Two things happen at startup:
    ///
    /// 1. The seed's Turtle source is loaded into the [`OxigraphStore`]'s
    ///    ontology (`TBox`) named graph during [`Plugin::ready`], so the
    ///    per-session
    ///    [`crate::agents::oxigraph_extraction::snapshot_ontology`] call
    ///    sees the curated predicates and classes.
    /// 2. The plugin's policy is set to [`SeedOntology::policy`] (chain
    ///    [`Self::with_policy`] afterwards to override).
    ///
    /// Equivalent to calling
    /// [`crate::oxigraph::OxigraphPlugin::with_ontology_file`] with the
    /// same pack — pick whichever side fits your wiring. Calling both is
    /// safe; oxigraph's loader is idempotent under repeated declarations.
    #[must_use]
    pub fn with_seed(mut self, seed: SeedOntology) -> Self {
        self.policy = seed.policy.clone();
        self.seed_turtle = Some(seed.turtle);
        self
    }

    /// Seed the [`Aliases`] table with hand-curated `(alias, target)` pairs.
    ///
    /// Mirror of [`crate::ExtractionPlugin::with_aliases`] on the oxigraph
    /// backend — the same [`Aliases`] global is registered either way.
    /// Pairs accumulate across calls. Typically populated from
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

impl Default for OxigraphExtractionPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for OxigraphExtractionPlugin {
    const ID: &'static str = "ontrace::oxigraph_extraction";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Requires the trace-ingest receiver, the sessions runtime, and the
        // `OxigraphStore` it writes quads into. The store requirement orders
        // this backend after `OxigraphPlugin` (the provider), so the global is
        // present both for the `with_seed` TBox load in `ready()` and for the
        // worker's per-trace writes.
        PluginAccess::new()
            .requires::<TraceIngestRx>(VersionReq::caret(TraceIngestRx::CONTRACT_VERSION))
            .requires::<SessionsAPI>(VersionReq::caret(SessionsAPI::CONTRACT_VERSION))
            .requires::<OxigraphStore>(VersionReq::caret(OxigraphStore::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
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
        if let Some(turtle) = &self.seed_turtle {
            let store = server
                .get_global::<OxigraphStore>()
                .expect("OxigraphExtractionPlugin::with_seed requires OxigraphPlugin")
                .clone();
            store
                .load_turtle(turtle, store.ontology_graph())
                .expect("seed ontology Turtle failed to load into TBox");
        }

        let sessions = server
            .api::<SessionsAPI>()
            .expect("OxigraphExtractionPlugin requires SessionsPlugin")
            .clone();

        sessions
            .register_agent(OxigraphExtractionAgent)
            .expect("OxigraphExtractionAgent failed graph validation");

        let agent_type = sessions
            .find_agent_type(OxigraphExtractionAgent::NAME)
            .expect("just-registered agent must be findable");

        let mut rx = server
            .api::<TraceIngestRx>()
            .expect("OxigraphExtractionPlugin requires TraceIngestPlugin")
            .take();
        let model_id = self.model_id.clone();
        let policy = self.policy.clone();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let Some(line) = trace_line_from_message(&message) else {
                    continue;
                };
                let model_id = model_id.clone();
                let policy = policy.clone();
                let result: Result<usize, _> = sessions
                    .run_oneshot(&agent_type, |ctx| {
                        ctx.insert(line);
                        ctx.insert(AgentConfig::new(model_id));
                        ctx.insert(OntologyConfig::new(policy));
                    })
                    .await;
                match result {
                    Ok(count) => tracing::debug!(quads = count, "extracted"),
                    Err(err) => tracing::warn!(error = %err, "extraction failed"),
                }
            }
            tracing::info!("trace channel closed; oxigraph extraction worker exiting");
        });
    }
}
