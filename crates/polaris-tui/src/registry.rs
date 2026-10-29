//! [`TuiRegistry`] — the cross-plugin contribution surface for TUI panels.
//!
//! Plugins call `add_panel` during their `build()` phase, then
//! [`TuiPlugin`](crate::plugin::TuiPlugin) drains the registry once in
//! `ready()` to hand panels to the runtime.

use crate::panel::TuiPanel;
use parking_lot::Mutex;
use polaris::system::api::API;
use polaris::system::plugin::{Contract, Version};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cross-plugin contribution registry for [`TuiPanel`]s.
///
/// `TuiRegistry` is cheaply cloneable (`Arc`-backed) so the API surface
/// `server.api::<TuiRegistry>()` returns `&TuiRegistry` and contributors
/// can call `&self` methods.
///
/// # Lifecycle
///
/// 1. [`TuiPlugin::build`](crate::plugin::TuiPlugin) inserts an empty
///    registry.
/// 2. Other plugins call [`add_panel`](Self::add_panel) in their own
///    `build()` (or `ready()` if they depend on other plugins' state).
/// 3. [`TuiPlugin::ready`](crate::plugin::TuiPlugin) calls
///    [`drain`](Self::drain) once and hands the panels to the runtime.
///
/// After `drain` the registry is empty and marked drained; a later
/// [`add_panel`](Self::add_panel) is still accepted but will not appear in
/// the running shell, so it emits a `tracing::warn!` to surface the
/// misordering rather than dropping the panel silently. Contribute panels in
/// `build()` (or an early `ready()`) before [`TuiPlugin`](crate::plugin::TuiPlugin)
/// drains the registry.
#[derive(Clone, Default)]
pub struct TuiRegistry {
    state: Arc<Mutex<Vec<Box<dyn TuiPanel>>>>,
    drained: Arc<AtomicBool>,
}

impl std::fmt::Debug for TuiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiRegistry")
            .field("panels", &self.state.lock().len())
            .finish()
    }
}

impl API for TuiRegistry {}

/// The `TuiRegistry` capability contract. [`TuiPlugin`](crate::plugin::TuiPlugin)
/// `provides` it; every panel plugin `requires` it, so the server resolver orders
/// `TuiPlugin` first and guarantees the registry is present before any panel's
/// `build()`/`ready()` runs.
impl Contract for TuiRegistry {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl TuiRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Contributes a panel. Chained-builder.
    ///
    /// `&self` rather than `&mut self` so `server.api::<TuiRegistry>()`
    /// callers can chain without acquiring exclusive access.
    ///
    /// Contributing after [`drain`](Self::drain) has run (i.e. after
    /// [`TuiPlugin`](crate::plugin::TuiPlugin) has handed panels to the
    /// runtime) cannot reach the running shell, so it logs a warning instead
    /// of silently no-op'ing.
    pub fn add_panel<P: TuiPanel>(&self, panel: P) -> &Self {
        if self.drained.load(Ordering::Acquire) {
            tracing::warn!(
                panel = panel.id(),
                "TuiRegistry::add_panel called after the registry was drained; \
                 this panel will not appear. Contribute panels in build() before \
                 TuiPlugin drains them in ready()."
            );
        }
        self.state.lock().push(Box::new(panel));
        self
    }

    /// Number of contributions accumulated so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.lock().len()
    }

    /// True if no contributions have been made yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.lock().is_empty()
    }

    /// Drains the accumulated contributions and marks the registry drained.
    /// Called once by [`TuiPlugin::ready`](crate::plugin::TuiPlugin).
    pub fn drain(&self) -> Vec<Box<dyn TuiPanel>> {
        self.drained.store(true, Ordering::Release);
        std::mem::take(&mut *self.state.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TuiEvent;
    use crate::panel::EventOutcome;
    use ratatui::Frame;
    use ratatui::layout::Rect;

    struct DummyPanel(&'static str);

    impl TuiPanel for DummyPanel {
        fn id(&self) -> &str {
            self.0
        }
        fn handle_event(&mut self, _: &TuiEvent) -> EventOutcome {
            EventOutcome::Continue
        }
        fn render(&mut self, _: &mut Frame<'_>, _: Rect) {}
    }

    #[test]
    fn add_panel_accumulates_and_drains() {
        let registry = TuiRegistry::new();
        registry
            .add_panel(DummyPanel("a"))
            .add_panel(DummyPanel("b"));
        assert_eq!(registry.len(), 2);
        let panels = registry.drain();
        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0].id(), "a");
        assert!(registry.is_empty());
    }

    #[test]
    fn drain_marks_registry_drained() {
        let registry = TuiRegistry::new();
        assert!(!registry.drained.load(Ordering::Acquire));
        registry.drain();
        assert!(registry.drained.load(Ordering::Acquire));
        // A post-drain contribution is still recorded (and warns), but the
        // shell has already taken its panels.
        registry.add_panel(DummyPanel("late"));
        assert_eq!(registry.len(), 1);
    }
}
