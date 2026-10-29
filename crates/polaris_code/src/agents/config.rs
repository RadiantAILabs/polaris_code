//! Agent configuration.

use std::path::PathBuf;

use polaris::system::resource::LocalResource;

use crate::tools::GraphSearchMode;

/// Per-session configuration for [`super::CodeAgent`].
///
/// Carries the model id and the sandbox/working directory. The working dir is
/// the single coordination point with the binary (the binary sets it; the agent's
/// `setup` reads it to build [`super::ProjectContext`] and to scope path-based
/// grounding).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Model identifier (e.g. `anthropic/claude-sonnet-5`).
    pub model_id: String,
    /// Root directory the agent operates within.
    pub working_dir: PathBuf,
    /// Which graph-search surface to expose. The binary mirrors
    /// this onto [`crate::tools::CodeGraphPlugin`] and the freeform-tool
    /// permission gate; it is carried here so the gate has a single source.
    pub graph_search: GraphSearchMode,
    /// Whether to use the cache-aware context strategy. When `true`,
    /// the agent projects a byte-stable prefix and requests provider prompt
    /// caching; when `false` it falls back to the sliding [`LinearWindow`]. The
    /// binary mirrors this onto [`super::CodeAgentPlugin`].
    ///
    /// [`LinearWindow`]: crate::agents::context::LinearWindow
    pub prompt_cache: bool,
}

impl AgentConfig {
    /// Create a new agent configuration with the default graph-search mode and
    /// prompt caching enabled.
    #[must_use]
    pub fn new(model_id: impl Into<String>, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            model_id: model_id.into(),
            working_dir: working_dir.into(),
            graph_search: GraphSearchMode::default(),
            prompt_cache: true,
        }
    }

    /// Set the graph-search surface (config gate).
    #[must_use]
    pub fn with_graph_search(mut self, mode: GraphSearchMode) -> Self {
        self.graph_search = mode;
        self
    }

    /// Enable or disable the cache-aware context strategy + prompt caching.
    #[must_use]
    pub fn with_prompt_cache(mut self, on: bool) -> Self {
        self.prompt_cache = on;
        self
    }
}

impl LocalResource for AgentConfig {}
