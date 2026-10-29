//! Agent loop state.

use polaris::system::resource::LocalResource;

/// Tracks whether the agent's `ReAct` loop should terminate.
#[derive(Debug, Clone, Default)]
pub struct LoopState {
    /// Whether the loop is complete.
    pub is_complete: bool,
    /// Set when the loop completed only because error recovery itself
    /// failed (the model call `recover` fell back to also errored). Carries
    /// the rendered failure text so the turn can be reported as failed
    /// instead of completed, even though the graph reached its terminal
    /// `is_complete` state cleanly.
    pub failure: Option<String>,
}

impl LocalResource for LoopState {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_not_complete() {
        assert!(!LoopState::default().is_complete);
    }
}
