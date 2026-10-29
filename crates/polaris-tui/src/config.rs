//! [`TuiConfig`] — runtime knobs for the TUI shell.

use std::time::Duration;

/// How panels are laid out within the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutMode {
    /// One active panel fills the workspace; switch with `Tab` / digit keys.
    ///
    /// A single-view, conversation-first layout: a scrollback area plus a
    /// persistent input/status line.
    Single,
    /// All panels are visible at once in a tiled layout; one is focused.
    Tiled,
}

/// Runtime configuration for [`TuiPlugin`](crate::plugin::TuiPlugin).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TuiConfig {
    /// Initial layout mode. The user can toggle at runtime with `Ctrl-L`.
    pub layout: LayoutMode,
    /// Interval between [`TuiEvent::Tick`](crate::event::TuiEvent::Tick)
    /// emissions. 100ms gives a reactive feel without saturating panels.
    pub tick_rate: Duration,
    /// Whether mouse capture is enabled.
    pub mouse: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            layout: LayoutMode::Single,
            tick_rate: Duration::from_millis(100),
            mouse: false,
        }
    }
}

impl TuiConfig {
    /// Builder-style: set the initial layout mode.
    #[must_use]
    pub fn with_layout(mut self, layout: LayoutMode) -> Self {
        self.layout = layout;
        self
    }

    /// Builder-style: set the tick rate.
    #[must_use]
    pub fn with_tick_rate(mut self, rate: Duration) -> Self {
        self.tick_rate = rate;
        self
    }

    /// Builder-style: enable mouse capture.
    #[must_use]
    pub fn with_mouse(mut self, enabled: bool) -> Self {
        self.mouse = enabled;
        self
    }
}
