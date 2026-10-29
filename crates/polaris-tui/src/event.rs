//! Input events delivered to panels by the [`Runtime`](crate::runtime).
//!
//! Every panel receives the same stream of [`TuiEvent`]s; whether to consume
//! or ignore a given event is up to the panel via its
//! [`EventOutcome`](crate::panel::EventOutcome) return value. The shell
//! intercepts a small set of global chords (focus, layout toggle, quit)
//! *before* dispatch — panels never see those.

use crossterm::event::{KeyEvent, MouseEvent};

/// One frame of input or wall-clock time delivered to panels.
///
/// Concrete crossterm events are wrapped so panels can match without taking
/// a direct dependency on the crossterm version this crate ships with —
/// the wrapping is intentionally thin to keep that promise cheap.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TuiEvent {
    /// A key press the shell did not intercept.
    Key(KeyEvent),
    /// A bracketed paste arriving as one payload rather than a burst of
    /// [`Key`](Self::Key) events.
    ///
    /// The terminal brackets the paste, so a panel can insert the whole thing
    /// literally instead of replaying it through its key handler — which is
    /// what stops embedded newlines from submitting a prompt per line, and
    /// stops a pasted `/command` from dispatching.
    ///
    /// Only emitted while bracketed paste is enabled and the terminal supports
    /// it; otherwise a paste still arrives as ordinary key events.
    Paste(String),
    /// A mouse event (movement, click, scroll).
    Mouse(MouseEvent),
    /// Terminal was resized. Panels using cached layouts should invalidate.
    Resize {
        /// New width in columns.
        cols: u16,
        /// New height in rows.
        rows: u16,
    },
    /// A wall-clock tick — emitted at [`TuiConfig::tick_rate`](crate::config::TuiConfig::tick_rate).
    ///
    /// Panels use this to poll external state (e.g. session lists) without
    /// blocking the input loop.
    Tick,
    /// The shell focus moved to this panel.
    FocusGained,
    /// The shell focus moved off this panel.
    FocusLost,
}
