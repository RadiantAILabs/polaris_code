//! Guard against one-key answers typed before a prompt was seen.
//!
//! A single key press answers the approval picker and dialogs, so a word
//! being typed when one opens could answer it. A key that can answer is ignored
//! while another key was pressed less than [`QUIET`] earlier; the ignored key
//! restarts the window.

use std::time::{Duration, Instant};

/// How long a key that could answer is ignored after another key press.
pub(crate) const QUIET: Duration = Duration::from_millis(400);

/// Tracks the last key press an answer must wait out.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StrayGuard {
    last: Option<Instant>,
}

impl StrayGuard {
    /// Start a guard for a prompt opened after a key pressed at `last_key`.
    pub(crate) fn new(last_key: Option<Instant>) -> Self {
        Self { last: last_key }
    }

    /// Record a key pressed at `now` that did not answer.
    pub(crate) fn stray(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// Whether an answer key pressed at `now` may answer. A refused key counts
    /// as stray.
    pub(crate) fn admits(&mut self, now: Instant) -> bool {
        let quiet = self
            .last
            .is_none_or(|at| now.saturating_duration_since(at) >= QUIET);
        if !quiet {
            self.stray(now);
        }
        quiet
    }
}
