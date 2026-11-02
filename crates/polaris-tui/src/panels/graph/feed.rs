//! [`GraphFeed`] — the shared state a host writes and the graph panel reads.
//!
//! The feed holds two things: the [`GraphModel`] to draw, and what is running
//! in it. A host writes both from wherever it observes its graph engine; the
//! [`GraphPanel`](super::GraphPanel) reads a [`GraphSnapshot`] of them when
//! something changed. The feed knows nothing about any graph engine.
//!
//! # Why a shared handle and not a message
//!
//! The conversation panel owns the host's [`UserIO`](polaris::plugins::UserIO)
//! and consumes every message sent through it; a second panel cannot receive
//! from it without the conversation panel forwarding. A cloneable handle over
//! shared state — the same shape as [`LogBuffer`](crate::panels::LogBuffer),
//! which already links the log subscriber to two panels — lets the producer
//! and the panel stay unaware of each other. It also coalesces by
//! construction: every write overwrites state, so the panel only ever draws
//! the latest, however many writes landed between frames.
//!
//! # Cost
//!
//! Each write takes a short lock, updates a few fields and bumps an atomic
//! version. There is no channel, no allocation proportional to the graph and
//! no wake-up of the UI task; the panel notices the version change on its next
//! tick.

use super::model::GraphModel;
use parking_lot::Mutex;
use polaris::system::plugin::{Contract, Version};
use polaris::system::resource::GlobalResource;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A node that is running right now.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunningNode {
    key: String,
    since: Instant,
}

impl RunningNode {
    /// The key of the running node.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// When the node started.
    #[must_use]
    pub fn since(&self) -> Instant {
        self.since
    }
}

/// What a node did earlier in the current run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct NodeRun {
    /// How many times the node finished.
    pub count: u32,
    /// How long its latest pass took.
    pub last: Duration,
}

/// A model with the lookups the feed needs to place activity in it.
#[derive(Debug)]
struct Indexed {
    model: Arc<GraphModel>,
    keys: HashSet<String>,
    by_label: HashMap<String, Vec<String>>,
}

impl Indexed {
    fn new(model: GraphModel) -> Arc<Self> {
        let mut keys = HashSet::new();
        let mut by_label: HashMap<String, Vec<String>> = HashMap::new();
        model.for_each_node(&mut |node| {
            keys.insert(node.key().to_string());
            by_label
                .entry(node.label().to_string())
                .or_default()
                .push(node.key().to_string());
        });
        let duplicates = model.duplicate_keys();
        if !duplicates.is_empty() {
            tracing::warn!(
                graph = model.name(),
                ?duplicates,
                "polaris-tui: a graph model repeats node keys; activity cannot tell those nodes apart"
            );
        }
        Arc::new(Self {
            model: Arc::new(model),
            keys,
            by_label,
        })
    }
}

#[derive(Debug, Default)]
struct State {
    baseline: Option<Arc<Indexed>>,
    announced: Option<(u64, Arc<Indexed>)>,
    current: Option<Arc<Indexed>>,
    /// Bumped whenever the shown model is replaced.
    generation: u64,
    next_announcement: u64,
    /// Nested runs in flight; activity resets only when the outermost starts.
    depth: usize,
    run: u64,
    running: Vec<RunningNode>,
    finished: HashMap<String, NodeRun>,
    /// Reported key → model key, for reporters whose keys are not the model's.
    aliases: HashMap<String, String>,
    claimed: HashSet<String>,
}

impl State {
    fn reset_activity(&mut self) {
        self.running.clear();
        self.finished.clear();
        self.aliases.clear();
        self.claimed.clear();
    }

    /// Maps what a reporter calls a node onto a node of the shown model.
    ///
    /// A reporter that built the model from the graph it runs reports the
    /// model's own keys, and those match exactly. A reporter that could only
    /// rebuild the graph — its keys differ — is matched by label instead: the
    /// first node with that label that no other reported key has claimed yet,
    /// which is how two nodes sharing a label stay distinct as they first run.
    fn resolve(&mut self, key: &str, label: &str) -> Option<String> {
        let current = self.current.as_ref()?;
        if current.keys.contains(key) {
            return Some(key.to_string());
        }
        if let Some(mapped) = self.aliases.get(key) {
            return Some(mapped.clone());
        }
        let candidates = current.by_label.get(label)?;
        let chosen = candidates
            .iter()
            .find(|candidate| !self.claimed.contains(*candidate))
            .or_else(|| candidates.first())?
            .clone();
        self.claimed.insert(chosen.clone());
        self.aliases.insert(key.to_string(), chosen.clone());
        Some(chosen)
    }
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    version: AtomicU64,
}

/// Use a `GraphFeed` to show a graph in the Graph tab and light up the node that
/// is running. It is a cloneable handle to the graph being shown and what is
/// running in it, and is available with the `panels` feature.
///
/// A host calls [`publish`](Self::publish) once for the graph it normally runs,
/// [`announce`](Self::announce) before running a different one, and reports
/// each node with [`enter`](Self::enter) as it starts. A run is bracketed by
/// [`begin_run`](Self::begin_run). Each call is one short lock and an atomic
/// increment, so a producer on the agent's path pays nothing worth measuring.
///
/// Guards report completion when dropped, including when the future that held
/// them is cancelled, so an interrupted turn does not leave a node shown as
/// running.
///
/// # Scope
///
/// Global, because a shell shows one graph: every session in the process
/// reports to the same tab. The tab shows the outermost run that began last;
/// runs that overlap one in flight merge into it (see
/// [`announce`](Self::announce)).
///
/// # Provided By
///
/// [`GraphPanelPlugin`](super::GraphPanelPlugin), which inserts the feed during
/// `build()`. A host that wires the panel by hand shares a clone instead. A
/// system reading it must tolerate its absence: a headless host installs none.
///
/// # Access
///
/// `Res<GraphFeed>` only. The feed is mutated through its own `&self` methods,
/// which take an internal lock; there is no `ResMut` form and none is needed.
///
/// # Alternatives
///
/// [`LogBuffer`](crate::panels::LogBuffer) is the same shape for log lines: a
/// shared handle a producer writes and a panel reads. A host that only wants
/// node names in the transcript's activity line uses the `status` messages the
/// conversation panel already reads, which this does not replace.
///
/// # Example
///
/// ```
/// use polaris::system::param::Res;
/// use polaris::system::system;
/// use polaris_tui::panels::{GraphFeed, GraphModel, GraphNode};
///
/// #[system]
/// async fn show_the_pipeline(feed: Res<GraphFeed>) {
///     feed.publish(GraphModel::new(
///         "pipeline",
///         vec![GraphNode::system("a", "fetch"), GraphNode::system("b", "parse")],
///     ));
/// }
///
/// // What a producer does as the graph runs:
/// let feed = GraphFeed::new();
/// feed.publish(GraphModel::new("demo", vec![GraphNode::system("a", "fetch")]));
/// let _run = feed.begin_run();
/// let span = feed.enter("a", "fetch");
/// assert_eq!(feed.snapshot().running().len(), 1);
/// drop(span);
/// assert!(feed.snapshot().running().is_empty());
/// assert_eq!(feed.snapshot().finished("a").map(|run| run.count), Some(1));
/// ```
#[derive(Debug, Clone, Default)]
pub struct GraphFeed {
    inner: Arc<Inner>,
}

impl GlobalResource for GraphFeed {}

impl Contract for GraphFeed {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl GraphFeed {
    /// Creates a feed that shows nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self) {
        self.inner.version.fetch_add(1, Ordering::Release);
    }

    /// A number that changes whenever the snapshot would.
    ///
    /// Reading it is one atomic load, so a panel can poll it every tick and
    /// take a [`snapshot`](Self::snapshot) only when it moved.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire)
    }

    /// Sets the graph shown whenever a run does not announce another.
    ///
    /// The graph is shown at once unless a run is in flight or another graph is
    /// announced, so the tab is not empty before the first turn. In those two
    /// cases it is shown from the next run that announces nothing.
    pub fn publish(&self, model: GraphModel) {
        let indexed = Indexed::new(model);
        {
            let mut state = self.inner.state.lock();
            if state.current.is_none() || (state.depth == 0 && state.announced.is_none()) {
                state.current = Some(indexed.clone());
                state.generation += 1;
                state.reset_activity();
            }
            state.baseline = Some(indexed);
        }
        self.bump();
    }

    /// Names the graph the next [`begin_run`](Self::begin_run) executes.
    ///
    /// The returned guard keeps the announcement open. Dropping it withdraws an
    /// announcement no run has taken up, so a host that failed before running
    /// does not leave the next, unrelated run drawn as this graph.
    ///
    /// Only an outermost run takes an announcement up. One made while a run is
    /// already in flight is not taken by a nested [`begin_run`](Self::begin_run);
    /// it waits for the next outermost run. Sessions that run at the same time
    /// all merge into the first outermost run, so the graph shown is the one
    /// that run began with.
    #[must_use = "the announcement is withdrawn when the guard is dropped"]
    pub fn announce(&self, model: GraphModel) -> Announcement {
        let indexed = Indexed::new(model);
        let token = {
            let mut state = self.inner.state.lock();
            state.next_announcement += 1;
            let token = state.next_announcement;
            state.announced = Some((token, indexed));
            token
        };
        Announcement {
            feed: self.clone(),
            token,
        }
    }

    /// Starts a run: shows the announced graph, or the published one, and
    /// forgets what the previous run did.
    ///
    /// Runs may nest — a host executing a graph from inside a graph — and only
    /// the outermost one resets anything. The returned guard ends the run when
    /// dropped.
    #[must_use = "the run ends when the guard is dropped"]
    pub fn begin_run(&self) -> RunSpan {
        {
            let mut state = self.inner.state.lock();
            if state.depth == 0 {
                let next = match state.announced.take() {
                    Some((_, announced)) => Some(announced),
                    None => state.baseline.clone(),
                };
                // A run of the graph already on screen keeps its layout and
                // the reader's place in it; only a different graph replaces it.
                if let Some(next) = next
                    && !state
                        .current
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &next))
                {
                    state.current = Some(next);
                    state.generation += 1;
                }
                state.reset_activity();
                state.run += 1;
            }
            state.depth += 1;
        }
        self.bump();
        RunSpan { feed: self.clone() }
    }

    /// Reports that the node `key`, called `label`, has started.
    ///
    /// `key` is the node's own identity. A reporter that built the model from
    /// the graph it runs reports the model's keys, and those match exactly.
    /// A reporter that could only rebuild the graph reports keys the model does
    /// not have; the node is then placed by `label`: the first node of that
    /// label no other reported key has claimed in this run, so two nodes that
    /// share a label stay distinct as they first run. A node the model does not
    /// contain is ignored. The node counts as running until the returned guard
    /// is dropped.
    #[must_use = "the node is reported finished when the guard is dropped"]
    pub fn enter(&self, key: &str, label: &str) -> NodeSpan {
        let resolved = {
            let mut state = self.inner.state.lock();
            let resolved = state.resolve(key, label);
            if let Some(resolved) = &resolved {
                state.running.push(RunningNode {
                    key: resolved.clone(),
                    since: Instant::now(),
                });
            }
            resolved
        };
        if resolved.is_some() {
            self.bump();
        }
        NodeSpan {
            feed: self.clone(),
            key: resolved,
        }
    }

    fn leave(&self, key: &str) {
        {
            let mut state = self.inner.state.lock();
            let Some(position) = state.running.iter().rposition(|node| node.key == key) else {
                return;
            };
            let node = state.running.remove(position);
            let took = node.since.elapsed();
            let entry = state.finished.entry(node.key).or_insert(NodeRun {
                count: 0,
                last: Duration::ZERO,
            });
            entry.count = entry.count.saturating_add(1);
            entry.last = took;
        }
        self.bump();
    }

    fn end_run(&self) {
        {
            let mut state = self.inner.state.lock();
            state.depth = state.depth.saturating_sub(1);
            if state.depth == 0 {
                state.running.clear();
            }
        }
        self.bump();
    }

    fn withdraw(&self, token: u64) {
        let mut state = self.inner.state.lock();
        if state
            .announced
            .as_ref()
            .is_some_and(|(held, _)| *held == token)
        {
            state.announced = None;
        }
    }

    /// Reads the shown model and what has run in it.
    #[must_use]
    pub fn snapshot(&self) -> GraphSnapshot {
        let state = self.inner.state.lock();
        GraphSnapshot {
            model: state.current.as_ref().map(|current| current.model.clone()),
            generation: state.generation,
            run: state.run,
            in_run: state.depth > 0,
            running: state.running.clone(),
            finished: state.finished.clone(),
        }
    }
}

/// Keeps an announced graph open. See [`GraphFeed::announce`].
#[derive(Debug)]
pub struct Announcement {
    feed: GraphFeed,
    token: u64,
}

impl Drop for Announcement {
    fn drop(&mut self) {
        self.feed.withdraw(self.token);
    }
}

/// Marks a run in flight. See [`GraphFeed::begin_run`].
#[derive(Debug)]
pub struct RunSpan {
    feed: GraphFeed,
}

impl Drop for RunSpan {
    fn drop(&mut self) {
        self.feed.end_run();
    }
}

/// Marks a node running. See [`GraphFeed::enter`].
#[derive(Debug)]
pub struct NodeSpan {
    feed: GraphFeed,
    key: Option<String>,
}

impl Drop for NodeSpan {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.feed.leave(&key);
        }
    }
}

/// A read of a [`GraphFeed`]: the model to draw and what has run in it.
#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    model: Option<Arc<GraphModel>>,
    generation: u64,
    run: u64,
    in_run: bool,
    running: Vec<RunningNode>,
    finished: HashMap<String, NodeRun>,
}

impl GraphSnapshot {
    /// The graph to draw, if any has been published.
    #[must_use]
    pub fn model(&self) -> Option<&Arc<GraphModel>> {
        self.model.as_ref()
    }

    /// Changes whenever the model is replaced, so a drawn layout can be reused
    /// until it does.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// How many runs have started.
    #[must_use]
    pub fn run(&self) -> u64 {
        self.run
    }

    /// Whether a run is in flight.
    #[must_use]
    pub fn in_run(&self) -> bool {
        self.in_run
    }

    /// The nodes running now, oldest first.
    #[must_use]
    pub fn running(&self) -> &[RunningNode] {
        &self.running
    }

    /// Whether `key` is running now.
    #[must_use]
    pub fn is_running(&self, key: &str) -> bool {
        self.running.iter().any(|node| node.key == key)
    }

    /// When `key` last started running, if it is running now.
    #[must_use]
    pub fn running_since(&self, key: &str) -> Option<Instant> {
        self.running
            .iter()
            .rev()
            .find(|node| node.key == key)
            .map(|node| node.since)
    }

    /// What `key` did earlier in this run, if it ran.
    #[must_use]
    pub fn finished(&self, key: &str) -> Option<NodeRun> {
        self.finished.get(key).copied()
    }

    /// How many distinct nodes have finished in this run.
    #[must_use]
    pub fn finished_count(&self) -> usize {
        self.finished.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panels::graph::model::GraphNode;

    fn model(name: &str, keys: &[&str]) -> GraphModel {
        GraphModel::new(
            name,
            keys.iter()
                .map(|key| GraphNode::system(*key, format!("{key}-label")))
                .collect(),
        )
    }

    #[test]
    fn a_published_graph_is_shown_before_any_run() {
        let feed = GraphFeed::new();
        assert!(feed.snapshot().model().is_none());
        feed.publish(model("agent", &["a", "b"]));
        let snapshot = feed.snapshot();
        assert_eq!(snapshot.model().map(|m| m.name()), Some("agent"));
        assert!(!snapshot.in_run());
    }

    #[test]
    fn entering_a_node_marks_it_running_and_leaving_marks_it_finished() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a", "b"]));
        let _run = feed.begin_run();
        let span = feed.enter("a", "a-label");
        assert!(feed.snapshot().is_running("a"));
        assert!(feed.snapshot().running_since("a").is_some());
        drop(span);
        let snapshot = feed.snapshot();
        assert!(!snapshot.is_running("a"));
        assert_eq!(snapshot.finished("a").map(|run| run.count), Some(1));
        assert_eq!(snapshot.finished("b"), None);
    }

    #[test]
    fn every_change_moves_the_version_and_a_read_does_not() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a"]));
        let before = feed.version();
        let _ = feed.snapshot();
        assert_eq!(
            feed.version(),
            before,
            "reading must not look like a change"
        );
        let _run = feed.begin_run();
        let after_begin = feed.version();
        assert!(after_begin > before);
        let span = feed.enter("a", "a-label");
        assert!(feed.version() > after_begin);
        let entered = feed.version();
        drop(span);
        assert!(feed.version() > entered);
    }

    #[test]
    fn a_node_the_model_does_not_contain_is_ignored() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a"]));
        let _run = feed.begin_run();
        let before = feed.version();
        let span = feed.enter("stranger", "unknown");
        assert!(feed.snapshot().running().is_empty());
        assert_eq!(feed.version(), before, "an ignored node is not a change");
        drop(span);
        assert_eq!(feed.snapshot().finished_count(), 0);
    }

    #[test]
    fn a_new_run_forgets_what_the_last_one_did() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a"]));
        {
            let _run = feed.begin_run();
            drop(feed.enter("a", "a-label"));
            assert_eq!(feed.snapshot().finished_count(), 1);
        }
        // Still readable after the run ends, so the tab keeps showing it.
        assert_eq!(feed.snapshot().finished_count(), 1);
        let _run = feed.begin_run();
        assert_eq!(feed.snapshot().finished_count(), 0);
        assert_eq!(feed.snapshot().run(), 2);
    }

    #[test]
    fn a_run_of_the_graph_already_shown_keeps_its_generation() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a"]));
        let shown = feed.snapshot().generation();
        drop(feed.begin_run());
        drop(feed.begin_run());
        assert_eq!(
            feed.snapshot().generation(),
            shown,
            "a panel keys its layout and scroll position on this"
        );
        drop(feed.announce(model("other", &["x"])));
        let _run = feed.begin_run();
        assert_eq!(
            feed.snapshot().generation(),
            shown,
            "withdrawn: still the same graph"
        );
    }

    #[test]
    fn an_announced_graph_replaces_the_published_one_for_one_run() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a"]));

        let announcement = feed.announce(model("topology", &["x", "y"]));
        let run = feed.begin_run();
        assert_eq!(feed.snapshot().model().map(|m| m.name()), Some("topology"));
        drop(feed.enter("x", "x-label"));
        drop(run);
        drop(announcement);
        // The finished topology stays on screen until another run starts.
        assert_eq!(feed.snapshot().model().map(|m| m.name()), Some("topology"));
        assert_eq!(feed.snapshot().finished("x").map(|run| run.count), Some(1));

        let _next = feed.begin_run();
        assert_eq!(feed.snapshot().model().map(|m| m.name()), Some("agent"));
        assert_eq!(feed.snapshot().finished_count(), 0);
    }

    #[test]
    fn an_announcement_no_run_took_up_is_withdrawn_when_dropped() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a"]));
        drop(feed.announce(model("topology", &["x"])));
        let _run = feed.begin_run();
        assert_eq!(
            feed.snapshot().model().map(|m| m.name()),
            Some("agent"),
            "a withdrawn announcement must not describe the next run"
        );
    }

    #[test]
    fn a_nested_run_does_not_reset_the_outer_one() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a", "b"]));
        let outer = feed.begin_run();
        drop(feed.enter("a", "a-label"));
        let inner = feed.begin_run();
        assert_eq!(feed.snapshot().finished_count(), 1, "inner run kept it");
        drop(inner);
        assert!(feed.snapshot().in_run(), "the outer run is still going");
        drop(outer);
        assert!(!feed.snapshot().in_run());
    }

    #[test]
    fn a_dropped_guard_clears_a_node_a_cancelled_turn_left_running() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a"]));
        let run = feed.begin_run();
        let span = feed.enter("a", "a-label");
        assert!(feed.snapshot().is_running("a"));
        // A cancelled future drops its guards without running to completion.
        drop(span);
        drop(run);
        assert!(feed.snapshot().running().is_empty());
    }

    #[test]
    fn a_reporter_with_other_keys_is_matched_by_label_in_first_run_order() {
        // Two nodes share a label, as a retry of a model call does. The
        // reporter's ids are not the model's, so label decides, and the second
        // distinct id to arrive takes the second node.
        let model = GraphModel::new(
            "agent",
            vec![
                GraphNode::system("m1", "act"),
                GraphNode::system("m2", "wait"),
                GraphNode::system("m3", "act"),
            ],
        );
        let feed = GraphFeed::new();
        feed.publish(model);
        let _run = feed.begin_run();

        let first = feed.enter("other-1", "act");
        assert!(feed.snapshot().is_running("m1"));
        drop(first);
        let again = feed.enter("other-1", "act");
        assert!(
            feed.snapshot().is_running("m1"),
            "the same id is the same node"
        );
        drop(again);
        let retry = feed.enter("other-2", "act");
        assert!(feed.snapshot().is_running("m3"));
        assert!(!feed.snapshot().is_running("m1"));
        drop(retry);
        assert_eq!(feed.snapshot().finished("m1").map(|run| run.count), Some(2));
        assert_eq!(feed.snapshot().finished("m3").map(|run| run.count), Some(1));
    }

    #[test]
    fn parallel_nodes_are_running_together() {
        let feed = GraphFeed::new();
        feed.publish(model("agent", &["a", "b"]));
        let _run = feed.begin_run();
        let a = feed.enter("a", "a-label");
        let b = feed.enter("b", "b-label");
        assert_eq!(feed.snapshot().running().len(), 2);
        drop(a);
        assert!(feed.snapshot().is_running("b"));
        drop(b);
    }
}
