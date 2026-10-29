//! The [`TuiPanel`] trait — the contribution surface for plugins.
//!
//! A panel is a self-contained ratatui widget plus an event handler. Plugins
//! clone whatever Polaris APIs they need (sessions, model registry, custom
//! buses) into the panel struct in their `build()` phase, then register it
//! with [`TuiRegistry::add_panel`](crate::registry::TuiRegistry::add_panel).
//! The registry hands the panel to the runtime in
//! [`TuiPlugin::ready`](crate::plugin::TuiPlugin); from that point on the
//! panel owns its own state.

use crate::event::TuiEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

/// What the runtime should do after dispatching an event to a panel.
///
/// The default — [`Continue`](Self::Continue) — lets the shell keep
/// processing other listeners (none today) and fall through to its own
/// chord table. Returning [`Consumed`](Self::Consumed) tells the shell the
/// panel handled the input and the shell should not interpret it further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventOutcome {
    /// Panel did not act on the event; shell may fall through to its own
    /// handling.
    Continue,
    /// Panel handled the event; shell should not interpret it.
    Consumed,
    /// Panel wants the shell to quit the TUI cleanly.
    Quit,
}

/// A contributable terminal panel.
///
/// Panels are `Send` (the runtime owns them on the TUI task) but not `Sync`
/// — only the runtime accesses a given panel at a time. State that needs to
/// be shared with the rest of the server should live behind an `Arc`/`API`
/// that the panel clones in at construction time.
///
/// # Lifecycle
///
/// 1. Plugin constructs the panel during `build()` / `ready()` and calls
///    `registry.add_panel(panel)`.
/// 2. The provider calls [`attach_runtime`](Self::attach_runtime), then the
///    runtime calls [`init`](Self::init) once, after the terminal is set
///    up, so the panel can spawn background tasks tied to its lifetime.
/// 3. [`tick`](Self::tick) runs on every wall-clock tick; use it to poll
///    cheap external state. Expensive work should happen on background
///    tasks the panel spawned in [`init`](Self::init).
/// 4. [`handle_event`](Self::handle_event) runs for input events the shell
///    routes here. `Key` and `Mouse` events only reach the focused panel;
///    `Tick`, `Resize`, and focus transitions are broadcast to every panel
///    regardless of layout. Panels therefore do not need their own
///    "am-I-focused" gate around keystroke handling.
/// 5. [`render`](Self::render) runs every paint pass — keep it pure.
pub trait TuiPanel: Send + 'static {
    /// Stable identifier — also the tab label by default.
    fn id(&self) -> &str;

    /// Human-readable title rendered in tab bars and panel borders.
    fn title(&self) -> &str {
        self.id()
    }

    /// Attach this shell's runtime handle before [`init`](Self::init).
    ///
    /// Panels that need an interactive terminal round trip may retain a clone
    /// and call [`TuiHandle::run_in_terminal`](crate::TuiHandle::run_in_terminal)
    /// from a background task. Default: no-op.
    fn attach_runtime(&mut self, _handle: crate::TuiHandle) {}

    /// Called once before the first render. Default: no-op.
    fn init(&mut self) {}

    /// Called once per [`tick`](crate::event::TuiEvent::Tick). Default: no-op.
    fn tick(&mut self) {}

    /// Handle an input event. Default: ignore.
    fn handle_event(&mut self, _event: &TuiEvent) -> EventOutcome {
        EventOutcome::Continue
    }

    /// Render this panel into `area` of the current frame.
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect);
}
