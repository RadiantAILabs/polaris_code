//! Agent loop state.

use polaris::system::resource::LocalResource;

use super::failure::TurnFailure;

/// Tracks whether the agent's `ReAct` loop should terminate.
#[derive(Debug, Clone, Default)]
pub struct LoopState {
    /// Whether the loop is complete.
    pub is_complete: bool,
    /// Set when the turn failed in a way the loop could only report, not
    /// work around: the model call gave up after its retries, or the model
    /// call `recover` fell back to also errored. Carries the kind and the
    /// final message so the turn can be reported as failed instead of
    /// completed, even though the graph reached its terminal `is_complete`
    /// state cleanly.
    ///
    /// `act` records a model-call failure here before it errors, which is how
    /// `recover` knows the failure is already classified and the provider has
    /// had its full retry budget: it reports that failure once and does not
    /// ask the model again.
    pub failure: Option<TurnFailure>,
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
