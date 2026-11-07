//! Agent configuration.

use std::num::NonZeroU32;
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
#[non_exhaustive]
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
    /// The role text that replaces the built-in system prompt, when set. The
    /// project grounding block is still appended to it. `None` keeps the
    /// built-in prompt, so a session that never sets it sends the same bytes
    /// as before.
    pub system_prompt: Option<String>,
    /// The most one model reply may produce. It sizes the worst-case cost
    /// reserved for each request when the session's meter carries a spend cap
    /// (see [`crate::usage::TokenMeter::set_task_cost_cap`]), and it is passed
    /// to the provider as the request's output limit. `None` reserves the
    /// Anthropic fallback allowance (4096 tokens), regardless of provider,
    /// and leaves the request's limit to the provider.
    ///
    /// Set it to the limit the model provider was registered with: a provider
    /// registered with a larger ceiling than the value reserved here can
    /// produce more than was reserved.
    pub max_output_tokens: Option<NonZeroU32>,
    /// Explicit model context window, including the reserved output allowance.
    /// `None` keeps the built-in retrieval policy. Applies to this session.
    pub context_window: Option<NonZeroU32>,
    /// The shell commands this session may run, as command patterns (`*`
    /// matches any text, `?` one character). `None` leaves shell commands to
    /// the permission mode and the server's shell policy alone, as they always
    /// were. `Some` narrows this session further: a shell command is refused
    /// unless every command in it matches one of the patterns, and an empty
    /// list means the session has no shell at all, so the shell tools are not
    /// offered to the model either.
    ///
    /// The check is made per call, when the call is authorized, so two
    /// sessions on one server can hold different lists.
    pub command_globs: Option<Vec<String>>,
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
            system_prompt: None,
            max_output_tokens: None,
            context_window: None,
            command_globs: None,
        }
    }

    /// Use `prompt` as the system prompt instead of the built-in one. The
    /// project grounding block is still appended to it.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Limit this session's shell commands to those matching `patterns`; an
    /// empty list removes the shell from the session. See
    /// [`command_globs`](Self::command_globs).
    #[must_use]
    pub fn with_command_globs(mut self, patterns: Vec<String>) -> Self {
        self.command_globs = Some(patterns);
        self
    }

    /// Bound a single model reply to `tokens` output tokens, for the request
    /// itself and for the cost reserved before it is sent.
    #[must_use]
    pub fn with_max_output_tokens(mut self, tokens: NonZeroU32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    /// Declare this session's model input-plus-output capacity. The host must
    /// reserve a smaller output allowance before context projection can fit.
    #[must_use]
    pub fn with_context_window(mut self, tokens: NonZeroU32) -> Self {
        self.context_window = Some(tokens);
        self
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
