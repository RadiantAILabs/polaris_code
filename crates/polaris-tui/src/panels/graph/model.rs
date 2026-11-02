//! The graph shape the [`GraphPanel`](super::GraphPanel) draws.
//!
//! These types describe a graph the way a reader follows it — a chain of
//! steps, some of which branch — and nothing about how it executes. A host
//! converts whatever graph engine it runs into a [`GraphModel`]; this crate
//! never sees the engine's own types.

use std::collections::HashSet;

/// What a [`GraphNode`] is, which decides its glyph and whether it nests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GraphNodeKind {
    /// A unit of work that runs and returns.
    System,
    /// A two-way choice; its branches are labelled by outcome.
    Decision,
    /// A many-way choice; its branches are labelled by case.
    Switch,
    /// A body that repeats until it is done.
    Loop,
    /// Branches that run at the same time.
    Parallel,
    /// An embedded graph that runs behind a context boundary.
    Scope,
    /// A graph chosen at run time from a set of candidates.
    Dynamic,
}

impl GraphNodeKind {
    /// The one-cell glyph that marks this kind of node in the panel.
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::System => '•',
            Self::Decision => '?',
            Self::Switch => '⑂',
            Self::Loop => '↻',
            Self::Parallel => '≡',
            Self::Scope => '▣',
            Self::Dynamic => '◇',
        }
    }

    /// Whether a node of this kind runs other nodes while it is running.
    #[must_use]
    pub const fn is_container(self) -> bool {
        !matches!(self, Self::System)
    }
}

/// One step of a graph.
///
/// A node is identified by its [`key`](Self::key): a host reports activity
/// against keys, so the key must be unique within a [`GraphModel`] and stable
/// for as long as the model is shown.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphNode {
    key: String,
    label: String,
    kind: GraphNodeKind,
    note: Option<String>,
    branches: Vec<GraphBranch>,
}

impl GraphNode {
    /// Creates a node of `kind`, with no note and no branches.
    #[must_use]
    pub fn new(key: impl Into<String>, label: impl Into<String>, kind: GraphNodeKind) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind,
            note: None,
            branches: Vec::new(),
        }
    }

    /// Creates a [`GraphNodeKind::System`] node.
    #[must_use]
    pub fn system(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self::new(key, label, GraphNodeKind::System)
    }

    /// Attaches a short note shown after the label, such as `≤ 50 iterations`.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Adds a branch, drawn nested under this node.
    #[must_use]
    pub fn with_branch(mut self, branch: GraphBranch) -> Self {
        self.branches.push(branch);
        self
    }

    /// The identity activity is reported against.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The text drawn for this node.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// What kind of node this is.
    #[must_use]
    pub const fn kind(&self) -> GraphNodeKind {
        self.kind
    }

    /// The short note drawn after the label, if any.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The branches nested under this node, in drawing order.
    #[must_use]
    pub fn branches(&self) -> &[GraphBranch] {
        &self.branches
    }
}

/// A labelled chain of steps nested under a [`GraphNode`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphBranch {
    label: String,
    steps: Vec<GraphNode>,
}

impl GraphBranch {
    /// Creates a branch named `label` that runs `steps` in order.
    #[must_use]
    pub fn new(label: impl Into<String>, steps: Vec<GraphNode>) -> Self {
        Self {
            label: label.into(),
            steps,
        }
    }

    /// The branch's name: an outcome, a case, or a role such as `on error`.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The steps the branch runs, in order.
    #[must_use]
    pub fn steps(&self) -> &[GraphNode] {
        &self.steps
    }
}

/// A whole graph: a name, the chain of steps that starts it, and the branches
/// that sit beside the chain rather than in it, such as an error handler.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct GraphModel {
    name: String,
    steps: Vec<GraphNode>,
    side_branches: Vec<GraphBranch>,
}

impl GraphModel {
    /// Creates a graph named `name` that runs `steps` in order.
    #[must_use]
    pub fn new(name: impl Into<String>, steps: Vec<GraphNode>) -> Self {
        Self {
            name: name.into(),
            steps,
            side_branches: Vec::new(),
        }
    }

    /// Adds a branch that is not part of the main chain, drawn after it.
    #[must_use]
    pub fn with_side_branch(mut self, branch: GraphBranch) -> Self {
        self.side_branches.push(branch);
        self
    }

    /// The graph's name, shown in the panel header.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The main chain, in running order.
    #[must_use]
    pub fn steps(&self) -> &[GraphNode] {
        &self.steps
    }

    /// The branches drawn after the main chain.
    #[must_use]
    pub fn side_branches(&self) -> &[GraphBranch] {
        &self.side_branches
    }

    /// How many nodes the graph has, counting every nested one.
    #[must_use]
    pub fn node_count(&self) -> usize {
        let mut count = 0;
        self.for_each_node(&mut |_| count += 1);
        count
    }

    /// Whether the graph has no nodes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.side_branches.iter().all(|b| b.steps.is_empty())
    }

    /// Whether some node of the graph has `key`.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        let mut found = false;
        self.for_each_node(&mut |node| found |= node.key == key);
        found
    }

    /// The keys that occur more than once, in first-seen order.
    ///
    /// A key must name one node; a duplicate would make activity ambiguous, so
    /// the feed treats a model with duplicates as a host mistake and says so.
    #[must_use]
    pub fn duplicate_keys(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut duplicates: Vec<String> = Vec::new();
        self.for_each_node(&mut |node| {
            if !seen.insert(node.key.clone()) && !duplicates.contains(&node.key) {
                duplicates.push(node.key.clone());
            }
        });
        duplicates
    }

    /// Visits every node in drawing order, depth first.
    pub(crate) fn for_each_node(&self, visit: &mut impl FnMut(&GraphNode)) {
        fn walk(steps: &[GraphNode], visit: &mut impl FnMut(&GraphNode)) {
            for node in steps {
                visit(node);
                for branch in &node.branches {
                    walk(&branch.steps, visit);
                }
            }
        }
        walk(&self.steps, visit);
        for branch in &self.side_branches {
            walk(&branch.steps, visit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphModel {
        GraphModel::new(
            "demo",
            vec![
                GraphNode::system("a", "first"),
                GraphNode::new("d", "ready?", GraphNodeKind::Decision)
                    .with_branch(GraphBranch::new("yes", vec![GraphNode::system("b", "go")]))
                    .with_branch(GraphBranch::new("no", vec![GraphNode::system("c", "stop")])),
            ],
        )
        .with_side_branch(GraphBranch::new(
            "on error",
            vec![GraphNode::system("e", "recover")],
        ))
    }

    #[test]
    fn a_model_counts_every_nested_node_and_side_branch() {
        assert_eq!(sample().node_count(), 5);
        assert!(!sample().is_empty());
        assert!(GraphModel::default().is_empty());
    }

    #[test]
    fn keys_are_found_at_any_depth_and_duplicates_are_reported() {
        let model = sample();
        assert!(model.contains_key("c"));
        assert!(model.contains_key("e"));
        assert!(!model.contains_key("zzz"));
        assert!(model.duplicate_keys().is_empty());

        let doubled = GraphModel::new(
            "dup",
            vec![GraphNode::system("x", "one"), GraphNode::system("x", "two")],
        );
        assert_eq!(doubled.duplicate_keys(), vec!["x".to_string()]);
    }
}
