//! A panel wrapper that renders dialogs and handles their input.

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::{Frame, layout::Rect, widgets::Clear};

use super::DialogHost;
use crate::{EventOutcome, Notification, TuiEvent, TuiHandle, TuiPanel};

/// Adds modal dialogs to a panel.
///
/// Register this wrapper in place of the panel. It forwards the panel's lifecycle
/// and notifications while dialogs take over rendering and input.
pub struct DialogPanel<P> {
    panel: P,
    dialogs: DialogHost,
    /// Whether [`is_modal`](TuiPanel::is_modal) last reported an open dialog,
    /// so rendering follows the layout the shell chose even if the dialog
    /// closes in between.
    showing: AtomicBool,
}

impl<P: std::fmt::Debug> std::fmt::Debug for DialogPanel<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogPanel")
            .field("panel", &self.panel)
            .field("dialogs", &self.dialogs)
            .field("showing", &self.showing.load(Ordering::Relaxed))
            .finish()
    }
}

impl DialogHost {
    /// Wrap a panel so it can display this host's dialogs.
    pub fn wrap<P: TuiPanel>(&self, panel: P) -> DialogPanel<P> {
        DialogPanel {
            panel,
            dialogs: self.clone(),
            showing: AtomicBool::new(false),
        }
    }
}

impl<P: TuiPanel> TuiPanel for DialogPanel<P> {
    fn id(&self) -> &str {
        self.panel.id()
    }

    fn title(&self) -> &str {
        self.panel.title()
    }

    fn is_modal(&self) -> bool {
        let showing = self.dialogs.is_active();
        self.showing.store(showing, Ordering::Relaxed);
        showing || self.panel.is_modal()
    }

    fn attach_runtime(&mut self, handle: TuiHandle) {
        self.panel.attach_runtime(handle);
    }

    fn init(&mut self) {
        self.panel.init();
    }

    fn tick(&mut self) {
        self.panel.tick();
    }

    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
        if self.dialogs.handle_event(event) {
            EventOutcome::Consumed
        } else {
            self.panel.handle_event(event)
        }
    }

    fn take_notification(&mut self) -> Option<Notification> {
        self.panel.take_notification()
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.showing.load(Ordering::Relaxed) {
            // A dialog that closed since the shell chose the modal layout
            // leaves a blank frame rather than the panel drawn full-screen.
            if !self.dialogs.render(frame, area) {
                frame.render_widget(Clear, area);
            }
        } else {
            self.panel.render(frame, area);
        }
    }
}
