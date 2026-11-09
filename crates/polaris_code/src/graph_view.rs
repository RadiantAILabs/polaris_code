//! The Graph tab's producer: the running graph, and what is executing in it.
//!
//! `polaris-tui` draws a [`GraphModel`] and overlays what a [`GraphFeed`] says
//! is running; it knows nothing about the graph engine. This module is the
//! adapter on this side of that line, and does two jobs:
//!
//! 1. **Shape.** [`graph_model`] converts a framework [`Graph`] into the model
//!    the tab draws, and [`GraphViewPlugin`] publishes the agent's own graph as
//!    the one shown by default. [`announce`] does the same for a graph a host
//!    executes itself — [`crate::cosmos::host`] announces the topology it
//!    selected for a turn — so the tab shows whichever graph is actually
//!    running.
//! 2. **Activity.** The activity observer registers framework middleware that
//!    reports each node as it starts and finishes. It is registered by
//!    [`ProgressMiddlewarePlugin`](crate::progress::ProgressMiddlewarePlugin),
//!    so it rides the same instrumentation seam as the status line and needs no
//!    code in the agent or in any graph.
//!
//! # Matching a running node to a drawn one
//!
//! Middleware names a running node by its framework id. A host that converts
//! the graph it then executes — a topology — gets ids that match the model
//! exactly. The agent's graph is different: a session builds its own copy, with
//! its own ids, so the model published at start-up cannot carry them. There the
//! feed falls back to the node's name, and two nodes that share a name are told
//! apart by the order they first run in. See [`GraphFeed::enter`].
//!
//! # Shape of the model
//!
//! The framework stores a graph as a flat list of nodes and edges; branch and
//! loop bodies are in the same list as the top level. The model is the way a
//! reader follows it: a chain of steps, where a decision, switch, parallel
//! group, loop or scope carries its branches. Handler edges (error and
//! timeout) are drawn as an `on error` / `on timeout` branch of the innermost
//! loop whose body they guard, or beside the main chain. An embedded scope's
//! graph is drawn inside it, to three levels, and summarised beyond that.
//! Conversion recurses once per nesting level and assumes trusted, code-defined
//! graphs.

use polaris::agent::Agent;
use polaris::graph::edge::Edge;
use polaris::graph::middleware::info::{
    DecisionInfo, DynamicInfo, LoopInfo, ParallelInfo, ScopeInfo, SwitchInfo, SystemInfo,
};
use polaris::graph::node::{CandidateSource, Node};
use polaris::graph::{Graph, MiddlewareAPI, NodeId};
use polaris::system::param::SystemContext;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version, VersionReq};
use polaris::system::server::Server;
use polaris_tui::panels::{
    Announcement, GraphBranch, GraphFeed, GraphModel, GraphNode, GraphNodeKind,
};
use std::collections::{HashMap, HashSet};

/// How many embedded graphs deep the tab draws before it only summarises.
pub(crate) const MAX_SCOPE_DEPTH: usize = 3;

/// The largest loop bound the tab names; a larger one is a "no practical limit".
const MAX_SHOWN_BOUND: usize = 10_000;

/// The name the activity middleware is registered under.
const OBSERVER: &str = "polaris_code::graph_view";

/// Converts a framework graph into the model the Graph tab draws.
///
/// `name` is what the tab's header shows. Every node's key is its framework
/// [`NodeId`], so activity reported by id lands on the node exactly.
///
/// # Example
///
/// ```
/// use polaris::graph::Graph;
/// use polaris_code::graph_view::graph_model;
///
/// async fn read_input() {}
/// async fn answer() {}
///
/// let mut graph = Graph::new();
/// graph.add_system(read_input).add_system(answer);
///
/// let model = graph_model("demo", &graph);
/// assert_eq!(model.name(), "demo");
/// assert_eq!(model.node_count(), 2);
/// assert_eq!(model.steps()[0].label(), "read_input");
/// ```
#[must_use]
pub fn graph_model(name: impl Into<String>, graph: &Graph) -> GraphModel {
    let mut converter = Converter::new(graph, 0);
    let steps = converter.main_chain();
    let mut model = GraphModel::new(name, steps);
    for branch in converter.unclaimed_handlers() {
        model = model.with_side_branch(branch);
    }
    model
}

/// An error or timeout edge, which the tab draws as a labelled branch.
struct Handler {
    source: NodeId,
    target: NodeId,
    label: &'static str,
}

/// Walks one graph's flat node and edge lists into the nested model.
struct Converter<'g> {
    graph: &'g Graph,
    nodes: HashMap<NodeId, &'g Node>,
    /// The sequential successor of each node.
    next: HashMap<NodeId, NodeId>,
    handlers: Vec<Handler>,
    /// Handler entry nodes already drawn under some loop.
    claimed: HashSet<NodeId>,
    /// Nodes already drawn, so a malformed graph cannot be drawn forever.
    seen: HashSet<NodeId>,
    /// Every node drawn so far, in order; a loop reads its body off the tail.
    trail: Vec<NodeId>,
    depth: usize,
}

impl<'g> Converter<'g> {
    fn new(graph: &'g Graph, depth: usize) -> Self {
        let nodes = graph
            .nodes()
            .iter()
            .map(|node| (node.id(), node))
            .collect::<HashMap<_, _>>();
        let mut next = HashMap::new();
        let mut handlers = Vec::new();
        for edge in graph.edges() {
            match edge {
                Edge::Sequential(edge) => {
                    next.entry(edge.from.clone()).or_insert(edge.to.clone());
                }
                Edge::Error(edge) => handlers.push(Handler {
                    source: edge.from.clone(),
                    target: edge.to.clone(),
                    label: "on error",
                }),
                Edge::Timeout(edge) => handlers.push(Handler {
                    source: edge.from.clone(),
                    target: edge.to.clone(),
                    label: "on timeout",
                }),
                _ => {}
            }
        }
        Self {
            graph,
            nodes,
            next,
            handlers,
            claimed: HashSet::new(),
            seen: HashSet::new(),
            trail: Vec::new(),
            depth,
        }
    }

    fn main_chain(&mut self) -> Vec<GraphNode> {
        match self.graph.entry() {
            Some(entry) => self.chain(&entry),
            None => Vec::new(),
        }
    }

    /// Draws the steps that follow one another from `start`.
    fn chain(&mut self, start: &NodeId) -> Vec<GraphNode> {
        let mut steps = Vec::new();
        let mut cursor = Some(start.clone());
        while let Some(id) = cursor {
            if !self.seen.insert(id.clone()) {
                break;
            }
            let Some(node) = self.nodes.get(&id).copied() else {
                break;
            };
            self.trail.push(id.clone());
            steps.push(self.node(node));
            cursor = self.next.get(&id).cloned();
        }
        steps
    }

    fn branch(&mut self, label: impl Into<String>, entry: Option<&NodeId>) -> Option<GraphBranch> {
        let steps = self.chain(entry?);
        (!steps.is_empty()).then(|| GraphBranch::new(label, steps))
    }

    fn node(&mut self, node: &'g Node) -> GraphNode {
        let id = node.id();
        let key = id.as_str();
        let label = node.name();
        let label = label.as_str();
        match node {
            Node::System(_) => GraphNode::system(key, label),
            Node::Decision(decision) => {
                let mut out = GraphNode::new(key, label, GraphNodeKind::Decision);
                for (name, entry) in [
                    ("yes", &decision.true_branch),
                    ("no", &decision.false_branch),
                ] {
                    if let Some(branch) = self.branch(name, entry.as_ref()) {
                        out = out.with_branch(branch);
                    }
                }
                out
            }
            Node::Switch(switch) => {
                let mut out = GraphNode::new(key, label, GraphNodeKind::Switch);
                for (case, entry) in &switch.cases {
                    if let Some(branch) = self.branch(case.as_str(), Some(entry)) {
                        out = out.with_branch(branch);
                    }
                }
                if let Some(branch) = self.branch("default", switch.default.as_ref()) {
                    out = out.with_branch(branch);
                }
                out
            }
            Node::Parallel(parallel) => {
                let mut out = GraphNode::new(key, label, GraphNodeKind::Parallel)
                    .with_note(format!("{} at once", parallel.branches.len()));
                for (index, entry) in parallel.branches.iter().enumerate() {
                    if let Some(branch) = self.branch(format!("branch {}", index + 1), Some(entry))
                    {
                        out = out.with_branch(branch);
                    }
                }
                out
            }
            Node::Loop(looped) => {
                // A bound too large to ever be reached says nothing a reader
                // can use, so it is left out.
                let note = match looped.max_iterations {
                    Some(max) if max <= MAX_SHOWN_BOUND => format!("repeats · up to {max}"),
                    _ => "repeats".to_string(),
                };
                let mut out = GraphNode::new(key, label, GraphNodeKind::Loop).with_note(note);
                let from = self.trail.len();
                if let Some(branch) = self.branch("", looped.body_entry.as_ref()) {
                    out = out.with_branch(branch);
                }
                let body: HashSet<NodeId> = self.trail[from..].iter().cloned().collect();
                for branch in self.handlers_guarding(&body) {
                    out = out.with_branch(branch);
                }
                out
            }
            Node::Scope(scope) => {
                let mode = scope.context_policy().mode().to_string().to_lowercase();
                let inner = scope.graph();
                let mut out = GraphNode::new(key, label, GraphNodeKind::Scope)
                    .with_note(format!("scope · {mode}"));
                if self.depth < MAX_SCOPE_DEPTH {
                    for branch in embedded(inner, self.depth + 1) {
                        out = out.with_branch(branch);
                    }
                } else {
                    out = out.with_note(format!(
                        "scope · {mode} · {} nodes inside",
                        inner.node_count()
                    ));
                }
                out
            }
            Node::Dynamic(dynamic) => {
                let mut out = GraphNode::new(key, label, GraphNodeKind::Dynamic);
                match dynamic.source() {
                    CandidateSource::Inline(candidates) if self.depth < MAX_SCOPE_DEPTH => {
                        out = out.with_note(format!("one of {}", candidates.len()));
                        for (name, candidate) in candidates {
                            let steps = embedded_steps(candidate, self.depth + 1);
                            if !steps.is_empty() {
                                out = out.with_branch(GraphBranch::new(name.as_ref(), steps));
                            }
                        }
                    }
                    CandidateSource::Inline(candidates) => {
                        out = out.with_note(format!("one of {}", candidates.len()));
                    }
                    _ => out = out.with_note("chosen at run time"),
                }
                out
            }
            _ => GraphNode::system(key, label),
        }
    }

    /// Draws each handler that has not been drawn yet and whose every guarded
    /// node is in `body`.
    ///
    /// A handler added to a graph guards the fallible nodes of that graph,
    /// including those of the loops and branches absorbed into it, so a handler
    /// belongs to the smallest container that holds all of them: an inner loop
    /// that holds only some leaves it for the loop around it.
    fn handlers_guarding(&mut self, body: &HashSet<NodeId>) -> Vec<GraphBranch> {
        let mut entries: Vec<(NodeId, &'static str)> = Vec::new();
        for handler in &self.handlers {
            let unseen = !self.claimed.contains(&handler.target)
                && !entries.iter().any(|(target, _)| *target == handler.target);
            let all_inside = self
                .handlers
                .iter()
                .filter(|other| other.target == handler.target && other.label == handler.label)
                .all(|other| body.contains(&other.source));
            if unseen && all_inside {
                entries.push((handler.target.clone(), handler.label));
            }
        }
        let mut branches = Vec::new();
        for (target, label) in entries {
            self.claimed.insert(target.clone());
            if let Some(branch) = self.branch(label, Some(&target)) {
                branches.push(branch);
            }
        }
        branches
    }

    /// Draws the handlers no loop claimed: those guarding the main chain.
    fn unclaimed_handlers(&mut self) -> Vec<GraphBranch> {
        let everything: HashSet<NodeId> = self.nodes.keys().cloned().collect();
        self.handlers_guarding(&everything)
    }
}

/// The branches of an embedded graph: its main chain, then any handlers.
fn embedded(graph: &Graph, depth: usize) -> Vec<GraphBranch> {
    let mut converter = Converter::new(graph, depth);
    let steps = converter.main_chain();
    let mut branches = Vec::new();
    if !steps.is_empty() {
        branches.push(GraphBranch::new("", steps));
    }
    branches.extend(converter.unclaimed_handlers());
    branches
}

/// The main chain of an embedded graph, for a candidate drawn as one branch.
fn embedded_steps(graph: &Graph, depth: usize) -> Vec<GraphNode> {
    Converter::new(graph, depth).main_chain()
}

/// Publishes the graph an agent runs as the one the Graph tab shows by default.
///
/// Add this plugin to give the Graph tab an initial model before a run starts.
/// The tab draws this graph until a host announces another with [`announce`].
/// A session builds its own copy of the agent's graph, so node ids differ from
/// the published model and the tab matches by name; see the
/// [module docs](self).
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Publishes a model into the existing [`GraphFeed`]; provides no resource of its own. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Publishes through the feed rather than registering an API. |
///
/// # Dependencies
///
/// - [`GraphPanelPlugin`](polaris_tui::panels::GraphPanelPlugin) — provides the
///   global [`GraphFeed`] this plugin requires.
/// - [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin) when constructed with
///   [`Self::for_code_agent_steps`], to freeze the registry before publication.
///
/// # Lifecycle
///
/// `build()` publishes the prepared model. The declared `GraphFeed` requirement
/// makes the resolver build the panel plugin first, regardless of add order.
/// With [`Self::for_code_agent_steps`], `ready()` republishes after all plugin
/// builds have filled the coding agent step registry.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::CodeAgent;
/// use polaris_code::graph_view::GraphViewPlugin;
/// use polaris_tui::panels::GraphPanelPlugin;
/// use polaris_tui::{TuiConfig, TuiPlugin};
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(TuiPlugin::new(TuiConfig::default()))
///     .add_plugins(GraphPanelPlugin::default())
///     .add_plugins(GraphViewPlugin::for_agent(&CodeAgent));
/// server.run().await;
/// # }
/// ```
#[derive(Debug)]
pub struct GraphViewPlugin {
    model: GraphModel,
    code_agent_steps: bool,
}

impl GraphViewPlugin {
    /// Prepares to publish the graph `agent` builds, under the agent's name.
    #[must_use]
    pub fn for_agent(agent: &impl Agent) -> Self {
        Self {
            model: graph_model(agent.name(), &agent.to_graph()),
            code_agent_steps: false,
        }
    }

    /// Publish the coding agent with all startup step contributions.
    ///
    /// Add this before `Server::finish`; `ready` reads the completed registry,
    /// so contributing plugins can be added in any order. After startup, register
    /// `CodeAgent::with_steps` from the same registry with `SessionsAPI`.
    /// Requires `CodeAgentPlugin` as well as the graph panel.
    #[must_use]
    pub fn for_code_agent_steps() -> Self {
        let mut plugin = Self::for_agent(&crate::agents::CodeAgent);
        plugin.code_agent_steps = true;
        plugin
    }

    /// The initial model this plugin publishes.
    /// With `for_code_agent_steps`, `ready` replaces the feed with the completed
    /// graph; this accessor still returns the initial stock model.
    #[must_use]
    pub fn model(&self) -> &GraphModel {
        &self.model
    }
}

impl Plugin for GraphViewPlugin {
    const ID: &'static str = "polaris_code::graph_view";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        if self.code_agent_steps {
            vec![PluginId::of::<crate::agents::CodeAgentPlugin>()]
        } else {
            Vec::new()
        }
    }

    async fn ready(&self, server: &mut Server) {
        if self.code_agent_steps {
            let steps = server
                .get_global::<crate::agents::code_agent::steps::CodeAgentSteps>()
                .expect("CodeAgentPlugin provides the step registry");
            let agent = crate::agents::CodeAgent::with_steps(&steps);
            server
                .get_global::<GraphFeed>()
                .expect("GraphPanelPlugin provides GraphFeed")
                .publish(graph_model(agent.name(), &agent.to_graph()));
        }
    }

    fn build(&self, server: &mut Server) {
        server
            .get_global::<GraphFeed>()
            .expect("GraphPanelPlugin provides the GraphFeed this plugin requires")
            .publish(self.model.clone());
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new().requires::<GraphFeed>(VersionReq::caret(GraphFeed::CONTRACT_VERSION))
    }
}

/// Tells the Graph tab which graph the next execution runs.
///
/// A host that builds its own executor — as [`crate::cosmos::host`] does for a
/// selected topology — calls this just before executing, with the graph it is
/// about to execute, and keeps the guard until the execution returns. The tab
/// then draws that graph, with node ids that match the run exactly, until the
/// next run starts; a guard dropped before any run starts withdraws the
/// announcement.
///
/// Returns `None`, and does nothing, when `ctx` has no [`GraphFeed`] — a host
/// with no Graph tab.
///
/// # Example
///
/// ```no_run
/// use polaris::graph::{Graph, GraphExecutor};
/// use polaris::system::param::SystemContext;
/// use polaris_code::graph_view::announce;
///
/// # async fn run(ctx: &mut SystemContext<'_>, graph: &Graph) {
/// let _announcement = announce(ctx, "selected topology", graph);
/// let result = GraphExecutor::new().execute(graph, ctx, None, None).await;
/// assert!(result.is_ok());
/// # }
/// ```
#[must_use = "the announcement is withdrawn when the guard is dropped"]
pub fn announce(ctx: &SystemContext<'_>, name: &str, graph: &Graph) -> Option<Announcement> {
    feed_of(ctx).map(|feed| feed.announce(graph_model(name, graph)))
}

/// The feed in scope, if the host has a Graph tab.
fn feed_of(ctx: &SystemContext<'_>) -> Option<GraphFeed> {
    ctx.get_resource::<GraphFeed>()
        .ok()
        .map(|feed| GraphFeed::clone(&feed))
}

/// Registers one node kind's observer: enter on the way in, leave when the
/// guard drops on the way out.
macro_rules! observe_nodes {
    ($middleware:expr, $register:ident, $info:ty) => {
        $middleware.$register(OBSERVER, |info: $info, ctx, next| {
            Box::pin(async move {
                let _node = feed_of(ctx)
                    .map(|feed| feed.enter(info.node_id.as_str(), info.node_name.as_str()));
                next.run(ctx).await
            })
        })
    };
}

/// Registers the middleware that reports each node's start and finish.
///
/// Called by [`ProgressMiddlewarePlugin`](crate::progress::ProgressMiddlewarePlugin)
/// on the chain it publishes. Every node kind is reported, because the framework
/// wraps a decision, loop, parallel group or scope around the nodes it runs: the
/// tab shows the innermost as running and the ones around it as holding it.
///
/// A context with no [`GraphFeed`] — headless, or a host with no Graph tab —
/// costs one failed lookup per node and changes nothing.
pub(crate) fn observe(middleware: &MiddlewareAPI) {
    middleware.register_graph_execution(OBSERVER, |_, ctx, next| {
        Box::pin(async move {
            // Held across the run so the tab knows when it ends, and dropped
            // with the future if the turn is cancelled.
            let _run = feed_of(ctx).map(|feed| feed.begin_run());
            next.run(ctx).await
        })
    });
    observe_nodes!(middleware, register_system, SystemInfo);
    observe_nodes!(middleware, register_decision, DecisionInfo);
    observe_nodes!(middleware, register_switch, SwitchInfo);
    observe_nodes!(middleware, register_loop, LoopInfo);
    observe_nodes!(middleware, register_parallel, ParallelInfo);
    observe_nodes!(middleware, register_scope, ScopeInfo);
    observe_nodes!(middleware, register_dynamic, DynamicInfo);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::CodeAgent;
    use polaris::graph::node::ContextPolicy;
    use polaris::graph::{CaughtError, ErrorKind, ExecutionError, GraphExecutor};
    use polaris::system::param::{ErrOut, Res};
    use polaris::system::resource::GlobalResource;
    use polaris::system::system;
    use polaris::system::system::SystemError;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::Notify;

    async fn alpha() -> i32 {
        1
    }
    async fn beta() -> i32 {
        2
    }
    async fn gamma() -> i32 {
        3
    }
    async fn delta() -> i32 {
        4
    }

    /// The first node of `graph` called `label`.
    fn id_of(graph: &Graph, label: &str) -> String {
        graph
            .nodes()
            .iter()
            .find(|node| node.name() == label)
            .unwrap_or_else(|| panic!("no node called {label}"))
            .id()
            .as_str()
            .to_string()
    }

    /// Every node of `steps` and everything nested in it, in drawing order.
    fn nodes_of<'a>(steps: &'a [GraphNode], out: &mut Vec<&'a GraphNode>) {
        for node in steps {
            out.push(node);
            for branch in node.branches() {
                nodes_of(branch.steps(), out);
            }
        }
    }

    fn all_nodes(model: &GraphModel) -> Vec<&GraphNode> {
        let mut out = Vec::new();
        nodes_of(model.steps(), &mut out);
        for branch in model.side_branches() {
            nodes_of(branch.steps(), &mut out);
        }
        out
    }

    fn find<'a>(model: &'a GraphModel, label: &str) -> &'a GraphNode {
        all_nodes(model)
            .into_iter()
            .find(|node| node.label() == label)
            .unwrap_or_else(|| panic!("the model has no node called {label}"))
    }

    fn branch<'a>(node: &'a GraphNode, label: &str) -> &'a GraphBranch {
        node.branches()
            .iter()
            .find(|branch| branch.label() == label)
            .unwrap_or_else(|| panic!("{} has no branch {label:?}", node.label()))
    }

    fn labels(steps: &[GraphNode]) -> Vec<&str> {
        steps.iter().map(GraphNode::label).collect()
    }

    #[test]
    fn a_chain_converts_in_order_and_keys_are_the_framework_ids() {
        let mut graph = Graph::new();
        graph.add_system(alpha).add_system(beta).add_system(gamma);

        let model = graph_model("chain", &graph);

        assert_eq!(model.name(), "chain");
        assert_eq!(labels(model.steps()), ["alpha", "beta", "gamma"]);
        assert_eq!(model.steps()[1].key(), id_of(&graph, "beta"));
        assert!(
            model
                .steps()
                .iter()
                .all(|n| n.kind() == GraphNodeKind::System),
            "plain systems are drawn as systems"
        );
    }

    #[test]
    fn a_decision_carries_a_yes_branch_and_a_no_branch_and_the_chain_resumes_after_it() {
        let mut graph = Graph::new();
        graph.add_system(alpha);
        graph.add_conditional_branch::<i32, _, _, _>(
            "choose",
            |n| *n > 0,
            |yes| {
                yes.add_system(beta).add_system(gamma);
            },
            |no| {
                no.add_system(delta);
            },
        );
        graph.add_system(alpha);

        let model = graph_model("branching", &graph);

        assert_eq!(labels(model.steps()), ["alpha", "choose", "alpha"]);
        let choose = find(&model, "choose");
        assert_eq!(choose.kind(), GraphNodeKind::Decision);
        assert_eq!(labels(branch(choose, "yes").steps()), ["beta", "gamma"]);
        assert_eq!(labels(branch(choose, "no").steps()), ["delta"]);
    }

    #[test]
    fn a_loop_carries_its_body_and_bound_and_draws_the_handler_that_guards_it() {
        let mut graph = Graph::new();
        graph.add_system(alpha);
        graph.add_loop_n("work", 5, |body| {
            body.add_system(beta).add_system(gamma);
        });
        let guarded = id_of(&graph, "beta");
        graph.add_error_handler_for([NodeId::from_string(guarded)], |handler| {
            handler.add_system(delta);
        });

        let model = graph_model("looping", &graph);

        let looped = find(&model, "work");
        assert_eq!(looped.kind(), GraphNodeKind::Loop);
        assert_eq!(looped.note(), Some("repeats · up to 5"));
        assert_eq!(labels(branch(looped, "").steps()), ["beta", "gamma"]);
        assert_eq!(
            labels(branch(looped, "on error").steps()),
            ["delta"],
            "the handler sits under the loop whose body it guards"
        );
        assert!(
            model.side_branches().is_empty(),
            "and is not repeated beside the chain"
        );
        assert_eq!(model.node_count(), graph.node_count());
    }

    #[test]
    fn a_handler_for_the_main_chain_sits_beside_it() {
        let mut graph = Graph::new();
        graph.add_system(alpha).add_system(beta);
        let guarded = id_of(&graph, "alpha");
        graph.add_error_handler_for([NodeId::from_string(guarded)], |handler| {
            handler.add_system(gamma);
        });

        let model = graph_model("guarded", &graph);

        assert_eq!(labels(model.steps()), ["alpha", "beta"]);
        assert_eq!(model.side_branches().len(), 1);
        assert_eq!(model.side_branches()[0].label(), "on error");
        assert_eq!(labels(model.side_branches()[0].steps()), ["gamma"]);
    }

    #[test]
    fn a_parallel_node_carries_one_branch_per_path() {
        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            [
                Box::new(|g: &mut Graph| {
                    g.add_system(alpha);
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|g: &mut Graph| {
                    g.add_system(beta).add_system(gamma);
                }),
            ],
        );
        graph.add_system(delta);

        let model = graph_model("parallel", &graph);

        let fan = find(&model, "fan_out");
        assert_eq!(fan.kind(), GraphNodeKind::Parallel);
        assert_eq!(fan.note(), Some("2 at once"));
        assert_eq!(labels(branch(fan, "branch 1").steps()), ["alpha"]);
        assert_eq!(labels(branch(fan, "branch 2").steps()), ["beta", "gamma"]);
        assert_eq!(labels(model.steps()), ["fan_out", "delta"]);
    }

    #[test]
    fn a_scope_draws_its_embedded_graph_inside_it() {
        let mut inner = Graph::new();
        inner.add_system(beta).add_system(gamma);
        let mut graph = Graph::new();
        graph.add_system(alpha);
        graph.add_scope("isolated_work", inner, ContextPolicy::shared());

        let model = graph_model("nested", &graph);

        let scope = find(&model, "isolated_work");
        assert_eq!(scope.kind(), GraphNodeKind::Scope);
        assert_eq!(scope.note(), Some("scope · shared"));
        assert_eq!(labels(branch(scope, "").steps()), ["beta", "gamma"]);
    }

    #[test]
    fn scopes_nested_past_the_depth_cap_are_summarised_not_drawn() {
        fn nested(levels: usize) -> Graph {
            let mut graph = Graph::new();
            if levels == 0 {
                graph.add_system(alpha).add_system(beta);
            } else {
                graph.add_scope(
                    format!("level_{levels}"),
                    nested(levels - 1),
                    ContextPolicy::shared(),
                );
            }
            graph
        }
        let model = graph_model("deep", &nested(MAX_SCOPE_DEPTH + 2));

        let cut = all_nodes(&model)
            .into_iter()
            .filter(|node| node.kind() == GraphNodeKind::Scope)
            .find(|node| node.branches().is_empty())
            .expect("some scope is summarised");
        let note = cut.note().expect("a summarised scope says what it holds");
        assert!(note.ends_with("nodes inside"), "{note}");
        assert_eq!(
            all_nodes(&model).len(),
            MAX_SCOPE_DEPTH + 1,
            "levels beyond the cap are not drawn"
        );
    }

    #[test]
    fn an_empty_graph_converts_to_an_empty_model() {
        assert!(graph_model("nothing", &Graph::new()).is_empty());
    }

    #[test]
    fn the_agent_loop_is_drawn_whole_with_each_node_once() {
        let graph = CodeAgent.to_graph();

        let model = graph_model(CodeAgent::NAME, &graph);

        assert_eq!(
            model.node_count(),
            graph.node_count(),
            "no node of the agent's graph is lost or drawn twice"
        );
        assert!(model.duplicate_keys().is_empty(), "keys name one node each");
        let ids: HashSet<String> = graph
            .nodes()
            .iter()
            .map(|node| node.id().as_str().to_string())
            .collect();
        let keys: HashSet<String> = all_nodes(&model)
            .iter()
            .map(|node| node.key().to_string())
            .collect();
        assert_eq!(keys, ids);

        // Two nodes share the name `act`: the first attempt and the retry.
        let acts: Vec<_> = all_nodes(&model)
            .into_iter()
            .filter(|node| node.label() == "act")
            .collect();
        assert_eq!(acts.len(), 2);
        assert_ne!(acts[0].key(), acts[1].key());

        let agent_loop = find(&model, "agent_loop");
        assert_eq!(agent_loop.kind(), GraphNodeKind::Loop);
        let mut inside = Vec::new();
        nodes_of(branch(agent_loop, "").steps(), &mut inside);
        assert!(
            inside.iter().any(|node| node.label() == "model_retries"),
            "the retry loop is nested in the agent loop"
        );
        assert_eq!(
            labels(branch(agent_loop, "on error").steps()),
            ["recover"],
            "the loop's error handler hangs off the loop"
        );
        assert!(
            find(&model, "model_retries")
                .branches()
                .iter()
                .all(|branch| branch.label() != "on error"),
            "the retry loop guards only some of the handler's nodes, so the outer loop owns it"
        );
        let tools = find(&model, "has_tool_calls");
        assert_eq!(
            labels(branch(tools, "yes").steps()),
            [
                "review_tools",
                "prepare_calls",
                "dispatch_read_only_batch",
                "tool_calls",
                "finish_calls"
            ]
        );
        assert_eq!(
            labels(branch(tools, "no").steps()),
            ["decide_continuation", "continues"]
        );
        let continues = find(&model, "continues");
        assert_eq!(labels(branch(continues, "yes").steps()), ["continue_turn"]);
        assert_eq!(labels(branch(continues, "no").steps()), ["finalize"]);
        let calls = find(&model, "tool_calls");
        assert_eq!(calls.kind(), GraphNodeKind::Loop);
        assert_eq!(
            labels(branch(calls, "").steps()),
            [
                "authorize_call",
                "needs_human",
                "call_allowed",
                "advance_call"
            ]
        );
        assert_eq!(
            labels(branch(find(&model, "needs_human"), "yes").steps()),
            ["ask_human"]
        );
        assert_eq!(
            labels(branch(find(&model, "call_allowed"), "yes").steps()),
            ["execute_call"]
        );
        assert_eq!(
            labels(branch(find(&model, "call_allowed"), "no").steps()),
            ["record_denial"]
        );
    }

    /// Draws `panel` into a terminal of the given size and returns its rows.
    fn screen(panel: &mut polaris_tui::panels::GraphPanel, width: u16, height: u16) -> Vec<String> {
        use polaris_tui::TuiPanel as _;
        use polaris_tui::ratatui::Terminal;
        use polaris_tui::ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn the_agents_whole_graph_reads_in_80_by_24_and_scrolls_to_its_end() {
        use polaris_tui::TuiEvent;
        use polaris_tui::TuiPanel as _;
        use polaris_tui::crossterm::event::{
            KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };
        let graph = CodeAgent.to_graph();
        let feed = GraphFeed::new();
        feed.publish(graph_model(CodeAgent::NAME, &graph));
        let mut panel = polaris_tui::panels::GraphPanel::new(feed);

        let first = screen(&mut panel, 80, 24);
        assert!(
            first[0].starts_with(&format!(" CodeAgent · {} nodes", graph.node_count())),
            "{:?}",
            first[0]
        );
        assert!(
            first[0].contains("1-23/"),
            "more rows than fit, so the header says which are shown: {:?}",
            first[0]
        );
        let text = first.join("\n");
        let row_of = |needle: &str| {
            first
                .iter()
                .find(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("no row mentions {needle:?}:\n{text}"))
        };
        assert!(row_of("↻ agent_loop").contains("agent_loop  repeats"));
        assert!(
            !row_of("↻ model_retries").contains("up to"),
            "a bound nobody can reach is not printed:\n{text}"
        );
        assert!(text.contains("? manual_compaction"), "{text}");
        assert!(
            !text.contains("recover"),
            "the end of the graph is below the fold:\n{text}"
        );

        panel.handle_event(&TuiEvent::Key(KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }));
        let last = screen(&mut panel, 80, 24).join("\n");
        assert!(last.contains("on error"), "{last}");
        assert!(
            last.contains("recover"),
            "scrolling reaches the handler at the end:\n{last}"
        );
    }

    /// What one system saw running when it ran: its name, then the keys.
    type Seen = (&'static str, Vec<String>);

    /// What systems observed about the feed while they ran.
    #[derive(Clone, Default)]
    struct Probe(Arc<Mutex<Vec<Seen>>>);

    impl GlobalResource for Probe {}

    impl Probe {
        fn take(&self) -> Vec<Seen> {
            std::mem::take(&mut *self.0.lock().expect("probe lock"))
        }
    }

    fn observed(name: &'static str, feed: &GraphFeed, probe: &Probe) {
        let running = feed
            .snapshot()
            .running()
            .iter()
            .map(|node| node.key().to_string())
            .collect();
        probe.0.lock().expect("probe lock").push((name, running));
    }

    #[system]
    async fn watched_alpha(feed: Res<GraphFeed>, probe: Res<Probe>) -> i32 {
        observed("alpha", &feed, &probe);
        1
    }

    #[system]
    async fn watched_beta(feed: Res<GraphFeed>, probe: Res<Probe>) -> i32 {
        observed("beta", &feed, &probe);
        2
    }

    #[system]
    async fn watched_gamma(feed: Res<GraphFeed>, probe: Res<Probe>) -> i32 {
        observed("gamma", &feed, &probe);
        3
    }

    #[system]
    async fn watched_failure(feed: Res<GraphFeed>, probe: Res<Probe>) -> Result<(), SystemError> {
        observed("failure", &feed, &probe);
        Err(SystemError::ExecutionError(
            "the watched step failed".into(),
        ))
    }

    #[system]
    async fn watched_handler(
        feed: Res<GraphFeed>,
        probe: Res<Probe>,
        error: ErrOut<CaughtError>,
    ) -> ErrorKind {
        observed("handler", &feed, &probe);
        assert_eq!(error.system_name, "watched_failure");
        assert!(error.message.contains("the watched step failed"));
        error.kind
    }

    #[derive(Clone, Default)]
    struct StallSignal(Arc<Notify>);

    impl GlobalResource for StallSignal {}

    #[system]
    async fn watched_pending(feed: Res<GraphFeed>, probe: Res<Probe>, signal: Res<StallSignal>) {
        observed("pending", &feed, &probe);
        signal.0.notify_one();
        std::future::pending::<()>().await;
    }

    /// `alpha`, then a choice between `beta` and `gamma`, then `alpha` again.
    fn watched_pipeline(choose_yes: bool) -> Graph {
        let mut graph = Graph::new();
        graph.add_system(watched_alpha);
        graph.add_conditional_branch::<i32, _, _, _>(
            "choose",
            move |n| (*n > 0) == choose_yes,
            |yes| {
                yes.add_system(watched_beta);
            },
            |no| {
                no.add_system(watched_gamma);
            },
        );
        graph.add_system(watched_alpha);
        graph
    }

    async fn observer_server(feed: &GraphFeed) -> (Server, Probe, MiddlewareAPI) {
        let probe = Probe::default();
        let mut server = Server::new();
        server.insert_global(feed.clone());
        server.insert_global(probe.clone());
        server.insert_global(StallSignal::default());
        server.finish().await.expect("the server assembles");
        let middleware = MiddlewareAPI::new();
        observe(&middleware);
        (server, probe, middleware)
    }

    fn assert_graph_valid(graph: &Graph) {
        let validation = graph.validate();
        assert!(validation.is_ok(), "{validation}");
    }

    /// Runs `graph` under the activity middleware with `feed` in scope, and
    /// returns what its systems saw.
    async fn run_observed(graph: &Graph, feed: &GraphFeed) -> Vec<Seen> {
        assert_graph_valid(graph);
        let (server, probe, middleware) = observer_server(feed).await;
        let mut ctx = server.create_context();
        GraphExecutor::new()
            .execute(graph, &mut ctx, None, Some(&middleware))
            .await
            .expect("the graph runs");
        probe.take()
    }

    #[tokio::test]
    async fn the_middleware_lights_each_node_as_it_runs_and_the_ones_around_it() {
        let graph = watched_pipeline(true);
        let feed = GraphFeed::new();
        feed.publish(graph_model("pipeline", &graph));

        let seen = run_observed(&graph, &feed).await;

        let (alpha_key, choose_key, beta_key) = (
            id_of(&graph, "watched_alpha"),
            id_of(&graph, "choose"),
            id_of(&graph, "watched_beta"),
        );
        assert_eq!(
            seen[0],
            ("alpha", vec![alpha_key.clone()]),
            "a system runs alone at the top level"
        );
        assert_eq!(
            seen[1],
            ("beta", vec![choose_key.clone(), beta_key.clone()]),
            "the decision holds the system it chose, which is the innermost running node"
        );
        assert_eq!(seen.len(), 3, "the other branch did not run");

        let after = feed.snapshot();
        assert!(after.running().is_empty(), "nothing is running afterwards");
        assert!(!after.in_run());
        assert_eq!(after.run(), 1);
        assert!(after.finished(&choose_key).is_some());
        assert!(after.finished(&beta_key).is_some());
        assert_eq!(
            after.finished(&id_of(&graph, "watched_gamma")),
            None,
            "a node that did not run is not marked as having run"
        );
    }

    #[tokio::test]
    async fn the_false_branch_lights_gamma_and_never_marks_beta() {
        let graph = watched_pipeline(false);
        let feed = GraphFeed::new();
        feed.publish(graph_model("pipeline", &graph));

        let seen = run_observed(&graph, &feed).await;
        let choose_key = id_of(&graph, "choose");
        let gamma_key = id_of(&graph, "watched_gamma");
        let beta_key = id_of(&graph, "watched_beta");
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[1],
            ("gamma", vec![choose_key.clone(), gamma_key.clone()])
        );
        assert!(seen.iter().all(|(_, keys)| !keys.contains(&beta_key)));

        let after = feed.snapshot();
        assert!(after.running().is_empty());
        assert!(!after.in_run());
        assert!(after.finished(&choose_key).is_some());
        assert!(after.finished(&gamma_key).is_some());
        assert_eq!(after.finished(&beta_key), None);
    }

    #[tokio::test]
    async fn a_copy_of_the_graph_with_other_ids_is_matched_by_name() {
        // A session builds its own copy of the agent's graph, so its node ids
        // are not those of the model published at start-up.
        let published = watched_pipeline(true);
        let executed = watched_pipeline(true);
        assert_ne!(
            id_of(&published, "watched_beta"),
            id_of(&executed, "watched_beta"),
            "the copies really do differ in id"
        );
        let feed = GraphFeed::new();
        feed.publish(graph_model("pipeline", &published));

        let seen = run_observed(&executed, &feed).await;

        assert_eq!(
            seen[1].1,
            vec![
                id_of(&published, "choose"),
                id_of(&published, "watched_beta")
            ],
            "the running nodes are the published model's"
        );
        let after = feed.snapshot();
        assert!(after.finished(&id_of(&published, "watched_beta")).is_some());
    }

    #[tokio::test]
    async fn two_nodes_with_one_name_light_in_the_order_they_first_run() {
        // `watched_alpha` is both the first and the last node of the pipeline.
        let published = watched_pipeline(true);
        let executed = watched_pipeline(true);
        let feed = GraphFeed::new();
        feed.publish(graph_model("pipeline", &published));
        let first = published.entry().expect("an entry").as_str().to_string();
        let last = published
            .nodes()
            .iter()
            .filter(|node| node.name() == "watched_alpha")
            .map(|node| node.id().as_str().to_string())
            .find(|id| *id != first)
            .expect("a second node with the name");

        let seen = run_observed(&executed, &feed).await;

        assert_eq!(seen[0].1, vec![first.clone()]);
        assert_eq!(
            seen[2].1,
            vec![last.clone()],
            "the second node with the name is lit, not the first again"
        );
        let after = feed.snapshot();
        assert_eq!(after.finished(&first).map(|run| run.count), Some(1));
        assert_eq!(after.finished(&last).map(|run| run.count), Some(1));
    }

    /// A later run on the same context starts with no leftover activity.
    async fn assert_clean_next_run(
        ctx: &mut SystemContext<'_>,
        middleware: &MiddlewareAPI,
        feed: &GraphFeed,
        probe: &Probe,
    ) {
        let after = feed.snapshot();
        assert!(after.running().is_empty(), "no node remains running");
        assert!(!after.in_run(), "the run guard was dropped");
        probe.take();

        let mut next = Graph::new();
        next.add_system(watched_beta);
        assert_graph_valid(&next);
        let key = id_of(&next, "watched_beta");
        feed.publish(graph_model("next run", &next));
        let result = GraphExecutor::new()
            .execute(&next, ctx, None, Some(middleware))
            .await
            .expect("the next run succeeds");
        assert_eq!(result.output::<i32>(), Some(&2));
        assert_eq!(probe.take(), [("beta", vec![key.clone()])]);
        let next = feed.snapshot();
        assert!(next.running().is_empty());
        assert!(!next.in_run());
        assert_eq!(next.run(), after.run() + 1);
        assert_eq!(next.finished(&key).map(|run| run.count), Some(1));
    }

    #[tokio::test]
    async fn a_failed_execution_clears_activity_and_allows_a_clean_next_run() {
        let mut graph = Graph::new();
        graph.add_system(watched_failure);
        assert_graph_valid(&graph);
        let feed = GraphFeed::new();
        feed.publish(graph_model("failure", &graph));
        let (server, probe, middleware) = observer_server(&feed).await;
        let mut ctx = server.create_context();

        let result = GraphExecutor::new()
            .execute(&graph, &mut ctx, None, Some(&middleware))
            .await;
        match result {
            Err(ExecutionError::SystemError(message)) => {
                assert!(message.contains("the watched step failed"));
            }
            other => panic!("expected the original system error, got {other:?}"),
        }
        assert_eq!(
            probe.take(),
            [("failure", vec![id_of(&graph, "watched_failure")])]
        );
        assert_clean_next_run(&mut ctx, &middleware, &feed, &probe).await;
    }

    #[tokio::test]
    async fn an_error_handler_is_observed_and_keeps_the_caught_error_kind() {
        let mut graph = Graph::new();
        graph
            .add_system(watched_failure)
            .add_error_handler(|handler| {
                handler.add_system(watched_handler);
            });
        assert_graph_valid(&graph);
        let feed = GraphFeed::new();
        feed.publish(graph_model("handled failure", &graph));
        let (server, probe, middleware) = observer_server(&feed).await;
        let mut ctx = server.create_context();

        let result = GraphExecutor::new()
            .execute(&graph, &mut ctx, None, Some(&middleware))
            .await
            .expect("the handler recovers");
        assert_eq!(result.output::<ErrorKind>(), Some(&ErrorKind::Execution));
        let handler = id_of(&graph, "watched_handler");
        assert_eq!(
            probe.take(),
            [
                ("failure", vec![id_of(&graph, "watched_failure")]),
                ("handler", vec![handler.clone()]),
            ]
        );
        assert!(feed.snapshot().finished(&handler).is_some());
        assert_clean_next_run(&mut ctx, &middleware, &feed, &probe).await;
    }

    #[tokio::test]
    async fn a_timed_out_execution_clears_activity_and_allows_a_clean_next_run() {
        let mut graph = Graph::new();
        let pending = graph.system(watched_pending).id();
        let timeout = Duration::from_millis(10);
        graph.set_timeout(pending.clone(), timeout);
        assert_graph_valid(&graph);
        let feed = GraphFeed::new();
        feed.publish(graph_model("timeout", &graph));
        let (server, probe, middleware) = observer_server(&feed).await;
        let mut ctx = server.create_context();

        let result = GraphExecutor::new()
            .execute(&graph, &mut ctx, None, Some(&middleware))
            .await;
        match result {
            Err(ExecutionError::Timeout {
                node,
                timeout: actual,
            }) => {
                assert_eq!(node, pending);
                assert_eq!(actual, timeout);
            }
            other => panic!("expected the original timeout, got {other:?}"),
        }
        assert_eq!(
            probe.take(),
            [("pending", vec![pending.as_str().to_string()])]
        );
        assert_clean_next_run(&mut ctx, &middleware, &feed, &probe).await;
    }

    #[tokio::test]
    async fn dropping_a_pending_execution_clears_activity_and_allows_a_clean_next_run() {
        let mut graph = Graph::new();
        graph.add_system(watched_pending);
        assert_graph_valid(&graph);
        let feed = GraphFeed::new();
        feed.publish(graph_model("cancelled", &graph));
        let (server, probe, middleware) = observer_server(&feed).await;
        let signal = server
            .get_global::<StallSignal>()
            .expect("stall signal")
            .clone();
        let mut ctx = server.create_context();
        let key = id_of(&graph, "watched_pending");
        let executor = GraphExecutor::new();
        {
            let run = executor.execute(&graph, &mut ctx, None, Some(&middleware));
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => panic!("the pending system finished: {result:?}"),
                () = signal.0.notified() => {}
            }
            let during = feed.snapshot();
            assert!(during.in_run());
            assert!(during.is_running(&key));
            assert_eq!(probe.take(), [("pending", vec![key])]);
        }
        assert_clean_next_run(&mut ctx, &middleware, &feed, &probe).await;
    }

    #[tokio::test]
    async fn a_host_with_no_graph_tab_runs_the_same_graph_to_the_same_end() {
        let mut graph = Graph::new();
        graph.add_system(alpha);
        graph.add_conditional_branch::<i32, _, _, _>(
            "choose",
            |n| *n > 0,
            |yes| {
                yes.add_system(beta);
            },
            |no| {
                no.add_system(gamma);
            },
        );
        graph.add_system(delta);

        assert_graph_valid(&graph);

        // No `GraphFeed` anywhere in scope: the observer's lookup finds nothing.
        let mut server = Server::new();
        server.finish().await.expect("the server assembles");

        let mut plain = server.create_context();
        let baseline = GraphExecutor::new()
            .execute(&graph, &mut plain, None, None)
            .await
            .expect("the graph runs without middleware");

        let middleware = MiddlewareAPI::new();
        observe(&middleware);
        let mut observed = server.create_context();
        let result = GraphExecutor::new()
            .execute(&graph, &mut observed, None, Some(&middleware))
            .await
            .expect("the graph runs under the observer");

        assert_eq!(result.nodes_executed(), baseline.nodes_executed());
        assert_eq!(baseline.output::<i32>(), Some(&4));
        assert_eq!(result.output::<i32>(), Some(&4));
    }
}
