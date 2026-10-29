//! Live progress reporting bridge: graph instrumentation → conversation panel.
//!
//! The agent's `ReAct` loop spends most of a turn parked on an `await` — the
//! LLM call in `act`, a tool in `execute_tools`. Those systems only emit
//! transcript traces *after* they return, so a turn that calls no tools shows
//! nothing until the final answer lands. This bridge closes that gap.
//!
//! [`ProgressMiddlewarePlugin`] registers a [`MiddlewareAPI`] system-middleware
//! that fires *around every system* of any graph. Before each step runs it asks
//! [`NodeActivity`] what a user should see while that node runs and pushes the
//! answer to [`UserIO`] as an ephemeral `io_type=status` message — which the
//! `polaris_tui` conversation panel renders as a spinner line (see that crate's
//! `ConversationPanel`). Because it rides the framework's generic
//! instrumentation seam (the same one the tracing plugin uses), it needs *zero*
//! code in the agent itself, and none in any other graph either.
//!
//! # What a user sees, and for which nodes
//!
//! [`NodeActivity`] is a registry, not a match arm. Its rule is:
//!
//! 1. a node with a registered description uses it — a working label, an
//!    explicit *idle* (clears the line), or explicit *silence*;
//! 2. every other node is described by its own identifier, humanised
//!    ([`NodeActivity::humanized`]): `predict_edit` → `Predict edit…`.
//!
//! So the default is visible, and invisibility is a decision someone recorded.
//! The alternative default — silence for anything unrecognised — is what made
//! every graph other than the agent loop unwatchable, and it fails the only
//! question a user asks of a running graph: is it moving, and where is it. A
//! developer identifier answers that; nothing does not. The panel's node
//! timeline already shows the raw node key beside the line, so a humanised
//! identifier adds no ambiguity — it is the same name, easier to read.
//!
//! A pack contributing graphs registers its own descriptions during `build`
//! (`Extends<NodeActivity>`), which is also how a node gets deliberately
//! silenced. Nothing has to be edited here for a graph added later.
//!
//! Each status also carries `node` + `seq` metadata, which the panel turns into
//! a per-turn node timeline (Ctrl-O), and the middleware opens a
//! `polaris.graph.execute_system` span around the node body so any `tracing`
//! emitted inside that node nests under it — the panel attributes those events
//! to the node by the same `(name, seq)`. The sequence resets when a graph
//! enters an *idle* node (for the agent loop, `receive_user_input`: a new turn)
//! and increments per working node, so repeated loop iterations stay distinct.
//!
//! Token usage is the one signal this can't carry: it lives on the
//! [`LlmResponse`](polaris::models::llm::LlmResponse) inside `act`, which the
//! middleware sees only by name. The agent emits a `io_type=tokens` delta there
//! directly; see [`crate::agents`].
//!
//! This bridge is deliberately app-local: it couples the generic graph
//! instrumentation to *this* app's `UserIO`, so `polaris_tui` stays a pure
//! renderer with no dependency on the graph engine.
//!
//! # Hosts that build their own executor
//!
//! A middleware only fires for an execution that was given the
//! [`MiddlewareAPI`] it was registered on. Sessions pass theirs for the agent
//! turn; a host that constructs its own [`GraphExecutor`](polaris::graph::GraphExecutor)
//! — as [`crate::cosmos::host`] does for a selected topology — has to pass it
//! too. [`GraphInstrumentation`] is how it gets there: this plugin publishes
//! the app's middleware chain as a global resource, and a host that finds one
//! runs its graphs under the same instrumentation. A host that does not (no
//! progress plugin installed) runs them exactly as before.

use polaris::graph::MiddlewareAPI;
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, UserIO};
use polaris::system::plugin;
use polaris::system::plugin::{Contract, Plugin, Version};
use polaris::system::resource::GlobalResource;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tracing::Instrument;

/// Metadata marking an ephemeral status message. Reuses polaris core's
/// [`IO_TYPE`] key (`"io_type"`) — the same one the `polaris_tui` conversation
/// panel matches on — so the two never drift.
const META_TYPE: &str = IO_TYPE;
/// Metadata value for [`META_TYPE`] on a status message.
const TYPE_STATUS: &str = "status";
/// Metadata key naming the graph node a status belongs to — drives the panel's
/// Ctrl-O node timeline.
const META_NODE: &str = "node";
/// Metadata key carrying the per-turn sequence index of a node.
const META_SEQ: &str = "seq";

/// What a user sees while a node runs, when the node's own identifier is all
/// there is to go on.
const FALLBACK_LABEL: &str = "Working…";

/// The registry of node descriptions the status line is drawn from.
///
/// Cloning shares one registry: [`ProgressMiddlewarePlugin`] keeps a handle in
/// its middleware and publishes the same handle as a build-phase resource, so a
/// plugin that contributes graphs can describe their nodes with
/// `Extends<NodeActivity>` during `build` and be read by the middleware at run
/// time.
///
/// An unregistered node is *not* silent — see the [module docs](self) for the
/// rule and why it is this way round.
///
/// # Example
///
/// ```
/// use polaris_code_core::progress::NodeActivity;
///
/// let activities = NodeActivity::new();
/// activities.describe("run_migrations", "Migrating…");
/// activities.silence("bind_scope");
///
/// assert_eq!(activities.activity("run_migrations").as_deref(), Some("Migrating…"));
/// assert_eq!(activities.activity("bind_scope"), None);
/// // Nobody described this one, so it is described by its own name.
/// assert_eq!(activities.activity("predict_edit").as_deref(), Some("Predict edit…"));
/// ```
#[derive(Clone, Default)]
pub struct NodeActivity {
    /// `None` is registered silence; `Some("")` clears the line to idle;
    /// `Some(label)` is a working label.
    described: Arc<RwLock<HashMap<String, Option<String>>>>,
}

impl Contract for NodeActivity {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl std::fmt::Debug for NodeActivity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let described = self.described.read().map(|map| map.len()).unwrap_or(0);
        formatter
            .debug_struct("NodeActivity")
            .field("described", &described)
            .finish()
    }
}

impl NodeActivity {
    /// Creates a registry in which every node is described by its own name.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the registry the shipped agent loop is described by.
    ///
    /// The agent loop's own six nodes read exactly as they always have; its two
    /// structural nodes stay silent, because "entering the loop" is not
    /// something a user is waiting on.
    #[must_use]
    pub fn with_agent_loop() -> Self {
        let activities = Self::new();
        // Parked waiting for the next user message → idle/ready, and a new turn.
        activities.idle("receive_user_input");
        activities.describe("prepare_context", "Preparing context…");
        activities.describe("act", "Thinking…");
        activities.describe("execute_tools", "Running tools…");
        activities.describe("finalize", "Finalizing…");
        activities.describe("recover", "Recovering…");
        // Structural: they neither wait nor work, so they say nothing.
        activities.silence("init_loop");
        activities.silence("apply_steering");
        activities
    }

    /// Describes `node` with the label shown while it runs.
    pub fn describe(&self, node: &str, label: impl Into<String>) {
        self.record(node, Some(label.into()));
    }

    /// Declares `node` an idle point: it clears the status line, and starts a
    /// new sequence for the nodes that follow.
    pub fn idle(&self, node: &str) {
        self.record(node, Some(String::new()));
    }

    /// Declares `node` deliberately unreported: no status is sent and the
    /// current line is left alone.
    pub fn silence(&self, node: &str) {
        self.record(node, None);
    }

    /// Returns the status text for `node`, or `None` if `node` is silenced.
    ///
    /// An empty string is the idle line. An undescribed node yields
    /// [`humanized`](Self::humanized).
    #[must_use]
    pub fn activity(&self, node: &str) -> Option<String> {
        match self.described.read() {
            Ok(described) => match described.get(node) {
                Some(entry) => entry.clone(),
                None => Some(Self::humanized(node)),
            },
            // A poisoned registry must not silence a running graph.
            Err(_) => Some(Self::humanized(node)),
        }
    }

    /// Returns `true` if `node` has been described (including as silent).
    #[must_use]
    pub fn is_described(&self, node: &str) -> bool {
        self.described
            .read()
            .is_ok_and(|described| described.contains_key(node))
    }

    /// Turns a node identifier into the line shown for it: separators become
    /// spaces, the first letter is capitalised, and an ellipsis marks it as
    /// in-progress. `predict_edit` → `Predict edit…`.
    ///
    /// A name with nothing readable in it falls back to a plain working line
    /// rather than to an empty one, which would read as idle.
    #[must_use]
    pub fn humanized(node: &str) -> String {
        let words = node.replace(['_', '-', '.', ':'], " ");
        let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut chars = words.chars();
        match chars.next() {
            None => FALLBACK_LABEL.to_string(),
            Some(first) => format!("{}{}…", first.to_uppercase(), chars.as_str()),
        }
    }

    fn record(&self, node: &str, entry: Option<String>) {
        if let Ok(mut described) = self.described.write() {
            described.insert(node.to_string(), entry);
        }
    }
}

/// The app's graph middleware chain, published so a host that builds its own
/// [`GraphExecutor`](polaris::graph::GraphExecutor) can run graphs under the
/// same instrumentation the agent's own turn runs under.
///
/// Cloning shares one chain. Absence is meaningful: a host that finds no
/// `GraphInstrumentation` is a host where nobody asked for instrumentation, and
/// it must execute exactly as it would have without this module.
#[derive(Clone, Debug)]
pub struct GraphInstrumentation {
    middleware: MiddlewareAPI,
}

impl GlobalResource for GraphInstrumentation {}

impl Contract for GraphInstrumentation {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl GraphInstrumentation {
    /// Wraps a middleware chain for publication.
    #[must_use]
    pub const fn new(middleware: MiddlewareAPI) -> Self {
        Self { middleware }
    }

    /// The chain to hand to
    /// [`GraphExecutor::execute`](polaris::graph::GraphExecutor::execute).
    #[must_use]
    pub const fn middleware(&self) -> &MiddlewareAPI {
        &self.middleware
    }
}

/// Plugin that reports live graph progress to the conversation panel.
///
/// Registers a system-middleware on the shared [`MiddlewareAPI`] (creating it
/// if no other plugin has) and publishes [`NodeActivity`] for graph-contributing
/// plugins to extend and [`GraphInstrumentation`] for hosts that execute graphs
/// themselves. Requires a [`UserIO`] to be resolvable in the executing scope —
/// provided by `ConversationPanelPlugin` interactively, [`crate::exec`]
/// headless. When `UserIO` is absent the middleware degrades silently to a
/// no-op.
pub struct ProgressMiddlewarePlugin;

#[plugin(
    id = "polaris_code::progress_middleware",
    version = "0.1.0",
    provides(NodeActivity)
)]
impl Plugin for ProgressMiddlewarePlugin {
    fn build(&self, server: &mut Server) {
        if !server.contains_api::<MiddlewareAPI>() {
            server.insert_api(MiddlewareAPI::new());
        }
        let mw = server
            .api::<MiddlewareAPI>()
            .expect("MiddlewareAPI present after insertion")
            .clone();

        // Published for hosts that construct their own executor: without the
        // chain in hand they cannot run a graph under this middleware.
        assert!(
            server
                .insert_global(GraphInstrumentation::new(mw.clone()))
                .is_none(),
            "GraphInstrumentation already has a global resource"
        );

        // Published as a build-phase resource so a plugin contributing graphs
        // can describe their nodes with `Extends<NodeActivity>`.
        let activities = NodeActivity::with_agent_loop();
        assert!(
            server.insert_resource(activities.clone()).is_none(),
            "NodeActivity capability already has a build-phase resource"
        );

        // Per-turn sequence counter. A turn begins when the graph enters an
        // idle node (`receive_user_input` in the agent loop), which resets it,
        // so each working node gets a monotonic, iteration-distinct index the
        // panel uses to bucket output.
        let counter = Arc::new(AtomicU64::new(0));

        mw.register_system("polaris_code::progress", move |info, ctx, next| {
            let node_name = info.node_name.clone();
            let activity = activities.activity(&node_name);
            let working = matches!(activity, Some(ref label) if !label.is_empty());
            let seq = if matches!(activity, Some(ref label) if label.is_empty()) {
                counter.store(0, Ordering::Relaxed);
                0
            } else if working {
                counter.fetch_add(1, Ordering::Relaxed) + 1
            } else {
                0
            };
            Box::pin(async move {
                if let Some(activity) = activity
                    && let Ok(user_io) = ctx.get_resource::<UserIO>()
                {
                    // The `user_io` read-ref borrows `ctx`; it must drop before
                    // `next.run` reborrows mutably. UserIO isn't `Clone`, so we
                    // send while holding it — safe under the sequential executor
                    // (no other system runs concurrently with this middleware).
                    let msg = IOMessage::from_agent("code", IOContent::Text(activity))
                        .with_metadata(META_TYPE, TYPE_STATUS)
                        .with_metadata(META_NODE, node_name.to_string())
                        .with_metadata(META_SEQ, seq.to_string());
                    let _ = user_io.send(msg).await;
                }
                if working {
                    // Open a node span so in-node `tracing` events (LLM retries,
                    // tool internals, the agent's own narration) nest under it
                    // and the conversation panel can attribute them by node.
                    // `TracingPlugin`'s graph instrumentation isn't installed in
                    // this app (the TUI's `LogPanelPlugin` owns the global
                    // subscriber), so the span is opened here.
                    let span = tracing::info_span!(
                        "polaris.graph.execute_system",
                        polaris.graph.system.name = %node_name,
                        polaris.graph.system.seq = seq,
                    );
                    next.run(ctx).instrument(span).await
                } else {
                    next.run(ctx).await
                }
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_loop_reads_exactly_as_before() {
        let activities = NodeActivity::with_agent_loop();
        assert_eq!(activities.activity("act").as_deref(), Some("Thinking…"));
        assert_eq!(
            activities.activity("execute_tools").as_deref(),
            Some("Running tools…")
        );
        assert_eq!(
            activities.activity("prepare_context").as_deref(),
            Some("Preparing context…")
        );
        assert_eq!(
            activities.activity("finalize").as_deref(),
            Some("Finalizing…")
        );
        assert_eq!(
            activities.activity("recover").as_deref(),
            Some("Recovering…")
        );
        // Blocking-on-input clears to idle.
        assert_eq!(
            activities.activity("receive_user_input").as_deref(),
            Some("")
        );
        // Structural systems leave the status untouched.
        assert_eq!(activities.activity("init_loop"), None);
    }

    #[test]
    fn an_undescribed_node_is_described_by_its_own_name() {
        let activities = NodeActivity::with_agent_loop();
        assert_eq!(
            activities.activity("predict_edit").as_deref(),
            Some("Predict edit…")
        );
        assert_eq!(
            activities.activity("conclude_prediction").as_deref(),
            Some("Conclude prediction…")
        );
        assert!(!activities.is_described("predict_edit"));
    }

    #[test]
    fn a_pack_can_describe_and_silence_its_own_nodes() {
        let activities = NodeActivity::with_agent_loop();
        activities.describe("predict_edit", "Predicting the edit…");
        activities.silence("bind_scope");

        assert_eq!(
            activities.activity("predict_edit").as_deref(),
            Some("Predicting the edit…")
        );
        assert_eq!(activities.activity("bind_scope"), None);
        // Clones share one registry, which is how the middleware sees a
        // description registered after it was built.
        assert_eq!(activities.clone().activity("bind_scope"), None);
    }

    #[test]
    fn humanizing_a_name_never_yields_an_idle_line() {
        assert_eq!(NodeActivity::humanized("act_edit"), "Act edit…");
        assert_eq!(NodeActivity::humanized("verify-edit"), "Verify edit…");
        assert_eq!(NodeActivity::humanized("scope.enter"), "Scope enter…");
        // An empty or separator-only name would otherwise read as "idle".
        assert_eq!(NodeActivity::humanized(""), FALLBACK_LABEL);
        assert_eq!(NodeActivity::humanized("__"), FALLBACK_LABEL);
    }
}
