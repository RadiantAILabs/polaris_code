//! [`AppShell`] — owns panels, focus state, and the shell chrome.
//!
//! Renders a conversation-first frame: a tab bar across the top, the active
//! panel (or all panels in tiled mode) below, and a one-line status hint at
//! the bottom showing the current layout and chord cheatsheet.

use crate::config::{LayoutMode, TuiConfig};
use crate::event::TuiEvent;
use crate::layout::{Chrome, compute_chrome, compute_panels};
use crate::notify::Notification;
use crate::panel::{EventOutcome, TuiPanel};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};

/// What [`AppShell::handle_event`] wants the runtime to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellOutcome {
    /// Keep running; redraw on next frame.
    Continue,
    /// Exit the runtime cleanly.
    Quit,
    /// Apply the shell's current mouse-capture state to the terminal.
    ///
    /// The shell flips [`AppShell::mouse_captured`] itself; the runtime reads
    /// it back and emits the crossterm enable/disable command, because only the
    /// runtime owns the terminal's stdout. Releasing capture hands the mouse
    /// back to the terminal so its native drag-select + clipboard work; the
    /// trade-off is that in-app wheel scroll pauses until capture is re-grabbed.
    ToggleMouse,
}

/// Owns panels, focus, and layout. The runtime is just a wrapper that pumps
/// events and ticks into the shell and asks it to render.
pub(crate) struct AppShell {
    panels: Vec<Box<dyn TuiPanel>>,
    focused: usize,
    layout: LayoutMode,
    /// Whether the host enabled mouse capture at all. When `false` the terminal
    /// never grabbed the mouse, so native selection already works and the
    /// selection-mode toggle is a no-op (and its hint is hidden).
    mouse_enabled: bool,
    /// Whether the mouse is *currently* captured by the app. Starts `false`
    /// (selection mode) even when [`mouse_enabled`](Self::mouse_enabled) is set,
    /// so native terminal selection works on launch; the `Ctrl-S` chord flips it
    /// to grab the mouse for in-app wheel scroll and back. The runtime reads this
    /// back after a [`ShellOutcome::ToggleMouse`] to drive the terminal.
    mouse_captured: bool,
}

impl AppShell {
    pub(crate) fn new(panels: Vec<Box<dyn TuiPanel>>, config: &TuiConfig) -> Self {
        Self {
            panels,
            focused: 0,
            layout: config.layout,
            mouse_enabled: config.mouse,
            // Even when the host enables mouse capture, the shell *starts* in
            // selection mode (capture released) so the terminal's native
            // drag-select + copy work out of the box. `Ctrl-S` grabs the mouse
            // for in-app wheel scrolling when wanted.
            mouse_captured: false,
        }
    }

    /// Whether the mouse is currently captured by the app. The runtime reads
    /// this after a [`ShellOutcome::ToggleMouse`] to emit the matching
    /// crossterm enable/disable command.
    pub(crate) fn mouse_captured(&self) -> bool {
        self.mouse_captured
    }

    pub(crate) fn init_panels(&mut self) {
        for panel in &mut self.panels {
            panel.init();
        }
        // The initial focused panel gets a synthetic FocusGained so panels
        // can light up their cursor / activity state without depending on
        // the runtime sending one first.
        if let Some(panel) = self.panels.get_mut(self.focused) {
            let _ = panel.handle_event(&TuiEvent::FocusGained);
        }
    }

    pub(crate) fn tick(&mut self) {
        for panel in &mut self.panels {
            panel.tick();
        }
    }

    /// Collect the attention requests panels raised since the last call, from
    /// every panel: a background panel's approval is as urgent as the
    /// focused one's.
    pub(crate) fn take_notifications(&mut self) -> Vec<Notification> {
        self.panels
            .iter_mut()
            .filter_map(|panel| panel.take_notification())
            .collect()
    }

    /// Dispatch an input event. Global chords are handled here; the rest is
    /// routed by event kind, not by layout: `Key`/`Mouse` go only to the
    /// focused panel (a background panel must not see keystrokes meant for
    /// the active one), while `Tick`/`Resize` broadcast so every panel can
    /// keep its clock and layout state in sync.
    pub(crate) fn handle_event(&mut self, event: TuiEvent) -> ShellOutcome {
        // Inviolable chords are intercepted before panels see them. Everything
        // else the shell binds is *offered* to the focused panel first and only
        // applied if the panel declines it — see `handle_fallback_key`.
        if let TuiEvent::Key(key) = &event
            && let Some(outcome) = self.handle_inviolable_key(*key)
        {
            return outcome;
        }

        let outcome = if is_focused_only(&event) {
            if let Some(panel) = self.panels.get_mut(self.focused) {
                panel.handle_event(&event)
            } else {
                EventOutcome::Continue
            }
        } else {
            let mut focused_outcome = EventOutcome::Continue;
            for (i, panel) in self.panels.iter_mut().enumerate() {
                let res = panel.handle_event(&event);
                if i == self.focused {
                    focused_outcome = res;
                }
            }
            focused_outcome
        };

        // A panel that handled the event stops here. One that returns
        // `Continue` has declined it, so the shell applies its own fallback
        // chords — the fall-through `EventOutcome::Continue` documents.
        if let (EventOutcome::Continue, TuiEvent::Key(key)) = (outcome, &event)
            && let Some(shell_outcome) = self.handle_fallback_key(*key)
        {
            return shell_outcome;
        }

        match outcome {
            EventOutcome::Quit => ShellOutcome::Quit,
            _ => ShellOutcome::Continue,
        }
    }

    /// Chords the shell always handles itself, before any panel sees them.
    ///
    /// These are the guarantees a panel must never be able to revoke: an exit
    /// (`Ctrl-Q`), a way out of a focused panel (`Alt+1..9`), and the terminal's
    /// own selection (`Ctrl-S`). A panel that consumed every event would
    /// otherwise trap the user in the process with no way to quit or switch
    /// away, so these are deliberately not offered to panels at all.
    ///
    /// Returns `Some` if the chord was intercepted (panels must not see it).
    fn handle_inviolable_key(&mut self, key: KeyEvent) -> Option<ShellOutcome> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('q'), true) => Some(ShellOutcome::Quit),
            // Ctrl-S toggles "selection mode": release mouse capture so the
            // terminal's own drag-select + copy take over, then re-grab it.
            // Only meaningful when the host enabled mouse capture — otherwise
            // the mouse was never grabbed and native selection always works, so
            // the chord falls through (no interception).
            (KeyCode::Char('s'), true) if self.mouse_enabled => {
                self.mouse_captured = !self.mouse_captured;
                Some(ShellOutcome::ToggleMouse)
            }
            (KeyCode::Char('l'), true) => {
                self.layout = match self.layout {
                    LayoutMode::Single => LayoutMode::Tiled,
                    LayoutMode::Tiled => LayoutMode::Single,
                };
                Some(ShellOutcome::Continue)
            }
            // Alt+1..=9 jumps to panel N. Plain digits go to panels (e.g. for
            // text input). Holding Alt is the "I want chrome, not text"
            // affordance.
            (KeyCode::Char(c), _)
                if key.modifiers.contains(KeyModifiers::ALT) && c.is_ascii_digit() =>
            {
                let idx = (c as u8 - b'0') as usize;
                if idx > 0 && idx <= self.panels.len() {
                    self.set_focus(idx - 1);
                }
                Some(ShellOutcome::Continue)
            }
            _ => None,
        }
    }

    /// Chords the shell handles only when the focused panel declined them.
    ///
    /// These have a real meaning inside a panel — `Tab` accepts a completion,
    /// `Ctrl-C` denies a pending approval or clears a draft — and a panel that
    /// is in such a state consumes them. A panel that is not returns
    /// `EventOutcome::Continue` and gets the shell behaviour unchanged: focus
    /// cycling, and quit.
    ///
    /// Returns `Some` if the shell acted on the chord.
    fn handle_fallback_key(&mut self, key: KeyEvent) -> Option<ShellOutcome> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => Some(ShellOutcome::Quit),
            (KeyCode::Tab, false) => {
                self.cycle_focus(1);
                Some(ShellOutcome::Continue)
            }
            (KeyCode::BackTab, _) => {
                self.cycle_focus(-1);
                Some(ShellOutcome::Continue)
            }
            _ => None,
        }
    }

    fn cycle_focus(&mut self, delta: isize) {
        if self.panels.is_empty() {
            return;
        }
        let len = self.panels.len() as isize;
        let next = (self.focused as isize + delta).rem_euclid(len) as usize;
        self.set_focus(next);
    }

    fn set_focus(&mut self, new: usize) {
        if new == self.focused || new >= self.panels.len() {
            return;
        }
        if let Some(prev) = self.panels.get_mut(self.focused) {
            let _ = prev.handle_event(&TuiEvent::FocusLost);
        }
        self.focused = new;
        if let Some(next) = self.panels.get_mut(self.focused) {
            let _ = next.handle_event(&TuiEvent::FocusGained);
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>) {
        let Chrome {
            tabs,
            workspace,
            status,
        } = compute_chrome(frame.area());

        self.render_tab_bar(frame, tabs);
        self.render_panels(frame, workspace);
        self.render_status_line(frame, status);
    }

    fn render_tab_bar(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.panels.is_empty() {
            let placeholder =
                Paragraph::new(" polaris-tui — no panels contributed ").style(dim_style());
            frame.render_widget(placeholder, area);
            return;
        }
        let titles: Vec<Line<'_>> = self
            .panels
            .iter()
            .enumerate()
            .map(|(i, p)| {
                Line::from(vec![
                    Span::styled(format!(" {} ", i + 1), Style::default().fg(Color::DarkGray)),
                    Span::raw(p.title().to_string()),
                    Span::raw(" "),
                ])
            })
            .collect();
        let tabs = Tabs::new(titles)
            .select(self.focused)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .divider(Span::raw(""));
        frame.render_widget(tabs, area);
    }

    fn render_panels(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.panels.is_empty() {
            let hint = Paragraph::new(
                "No TuiPanel contributions registered.\n\n\
                 Add a panel from a plugin's build() with:\n\
                 server.api::<TuiRegistry>().add_panel(MyPanel::new())",
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" polaris-tui "),
            )
            .style(dim_style());
            frame.render_widget(hint, area);
            return;
        }
        let rects = compute_panels(area, self.panels.len(), self.focused, self.layout);
        let focused = self.focused;
        for (i, (panel, rect)) in self.panels.iter_mut().zip(rects).enumerate() {
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let inner = match self.layout {
                LayoutMode::Single => rect,
                LayoutMode::Tiled => {
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ", panel.title()))
                        .border_style(if i == focused {
                            Style::default().fg(Color::Cyan)
                        } else {
                            dim_style()
                        });
                    let inner = block.inner(rect);
                    frame.render_widget(block, rect);
                    inner
                }
            };
            panel.render(frame, inner);
        }
    }

    fn render_status_line(&self, frame: &mut Frame<'_>, area: Rect) {
        let mode = match self.layout {
            LayoutMode::Single => "single",
            LayoutMode::Tiled => "tiled",
        };
        let mut hint =
            format!(" {mode} · tab/shift-tab switch · alt-1..9 jump · ctrl-l layout · ctrl-c quit");
        // Selection-mode affordance, shown only when the host enabled mouse
        // capture (otherwise native selection is always available).
        if self.mouse_enabled {
            if self.mouse_captured {
                hint.push_str(" · ctrl-s select");
            } else {
                hint.push_str(" · SELECT MODE: drag to copy · ctrl-s resume");
            }
        }
        hint.push(' ');
        let line = Paragraph::new(hint).style(dim_style());
        frame.render_widget(line, area);
    }
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// True for events that target the focused panel exclusively. Anything else
/// is fan-out (ticks, resizes, focus transitions a panel may need to observe).
fn is_focused_only(event: &TuiEvent) -> bool {
    matches!(event, TuiEvent::Key(_) | TuiEvent::Mouse(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counts {
        keys: AtomicUsize,
        mouse: AtomicUsize,
        ticks: AtomicUsize,
        resizes: AtomicUsize,
    }

    struct RecordingPanel {
        name: &'static str,
        counts: Arc<Counts>,
        /// What to return for `Key` events. `Continue` (the default) models a
        /// panel that declines the shell's fallback chords; `Consumed` models
        /// one that claims them — a completion popup, or an open approval.
        key_outcome: EventOutcome,
    }

    impl TuiPanel for RecordingPanel {
        fn id(&self) -> &str {
            self.name
        }
        fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
            match event {
                TuiEvent::Key(_) => {
                    self.counts.keys.fetch_add(1, Ordering::Relaxed);
                    return self.key_outcome;
                }
                TuiEvent::Mouse(_) => self.counts.mouse.fetch_add(1, Ordering::Relaxed),
                TuiEvent::Tick => self.counts.ticks.fetch_add(1, Ordering::Relaxed),
                TuiEvent::Resize { .. } => self.counts.resizes.fetch_add(1, Ordering::Relaxed),
                _ => 0,
            };
            EventOutcome::Continue
        }
        fn render(&mut self, _: &mut ratatui::Frame<'_>, _: Rect) {}
    }

    fn shell_with_two_panels(layout: LayoutMode) -> (AppShell, Arc<Counts>, Arc<Counts>) {
        shell_with_config(layout, false)
    }

    fn shell_with_config(layout: LayoutMode, mouse: bool) -> (AppShell, Arc<Counts>, Arc<Counts>) {
        shell_with_outcome(layout, mouse, EventOutcome::Continue)
    }

    /// Two panels whose `Key` handling returns `key_outcome`, so a test can
    /// model both sides of the fallback contract.
    fn shell_with_outcome(
        layout: LayoutMode,
        mouse: bool,
        key_outcome: EventOutcome,
    ) -> (AppShell, Arc<Counts>, Arc<Counts>) {
        let a = Arc::new(Counts::default());
        let b = Arc::new(Counts::default());
        let panels: Vec<Box<dyn TuiPanel>> = vec![
            Box::new(RecordingPanel {
                name: "a",
                counts: a.clone(),
                key_outcome,
            }),
            Box::new(RecordingPanel {
                name: "b",
                counts: b.clone(),
                key_outcome,
            }),
        ];
        let config = TuiConfig {
            layout,
            mouse,
            ..TuiConfig::default()
        };
        (AppShell::new(panels, &config), a, b)
    }

    fn plain_key(c: char) -> TuiEvent {
        TuiEvent::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn click() -> TuiEvent {
        TuiEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn chord(code: KeyCode, modifiers: KeyModifiers) -> TuiEvent {
        TuiEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn key_and_mouse_route_to_focused_panel_only_in_tiled() {
        // Tiled layout must not broadcast Key/Mouse: a background panel would
        // echo keystrokes meant for the active one.
        let (mut shell, a, b) = shell_with_two_panels(LayoutMode::Tiled);
        assert_eq!(shell.handle_event(plain_key('x')), ShellOutcome::Continue);
        assert_eq!(shell.handle_event(click()), ShellOutcome::Continue);
        assert_eq!(a.keys.load(Ordering::Relaxed), 1);
        assert_eq!(a.mouse.load(Ordering::Relaxed), 1);
        assert_eq!(b.keys.load(Ordering::Relaxed), 0);
        assert_eq!(b.mouse.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tick_and_resize_broadcast_in_both_layouts() {
        for layout in [LayoutMode::Single, LayoutMode::Tiled] {
            let (mut shell, a, b) = shell_with_two_panels(layout);
            shell.handle_event(TuiEvent::Tick);
            shell.handle_event(TuiEvent::Resize { cols: 80, rows: 24 });
            assert_eq!(a.ticks.load(Ordering::Relaxed), 1, "{layout:?}");
            assert_eq!(b.ticks.load(Ordering::Relaxed), 1, "{layout:?}");
            assert_eq!(a.resizes.load(Ordering::Relaxed), 1, "{layout:?}");
            assert_eq!(b.resizes.load(Ordering::Relaxed), 1, "{layout:?}");
        }
    }

    #[test]
    fn tab_and_backtab_move_focus_and_route_keys() {
        // Key events go only to the focused panel, so the keystroke count is a
        // proxy for "who has focus."
        let (mut shell, a, b) = shell_with_two_panels(LayoutMode::Single);
        // Initial focus is panel a.
        shell.handle_event(plain_key('x'));
        assert_eq!(a.keys.load(Ordering::Relaxed), 1);
        assert_eq!(b.keys.load(Ordering::Relaxed), 0);

        // Tab → focus b. Tab is a *fallback* chord, so the focused panel is
        // offered it first and its key count includes it; declining leaves the
        // shell behaviour (focus cycling).
        shell.handle_event(chord(KeyCode::Tab, KeyModifiers::NONE));
        shell.handle_event(plain_key('y'));
        assert_eq!(a.keys.load(Ordering::Relaxed), 2, "a saw 'x' and the Tab");
        assert_eq!(b.keys.load(Ordering::Relaxed), 1, "focus moved to b");

        // BackTab wraps back to a.
        shell.handle_event(chord(KeyCode::BackTab, KeyModifiers::NONE));
        shell.handle_event(plain_key('z'));
        assert_eq!(a.keys.load(Ordering::Relaxed), 3);
        assert_eq!(b.keys.load(Ordering::Relaxed), 2, "b saw the BackTab");
    }

    #[test]
    fn alt_digit_jumps_to_panel_and_out_of_range_is_ignored() {
        let (mut shell, a, b) = shell_with_two_panels(LayoutMode::Single);
        // Alt-2 → panel b (1-based).
        shell.handle_event(chord(KeyCode::Char('2'), KeyModifiers::ALT));
        shell.handle_event(plain_key('x'));
        assert_eq!(b.keys.load(Ordering::Relaxed), 1);

        // Alt-9 is out of range with two panels — focus stays on b.
        shell.handle_event(chord(KeyCode::Char('9'), KeyModifiers::ALT));
        shell.handle_event(plain_key('y'));
        assert_eq!(a.keys.load(Ordering::Relaxed), 0);
        assert_eq!(b.keys.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn ctrl_s_toggles_mouse_capture_when_enabled() {
        // With the mouse enabled the shell starts in selection mode (capture
        // released) so native selection works on launch. Ctrl-S grabs the
        // mouse, then releases it again — each toggle asks the runtime to
        // mirror the new state, without leaking the chord to panels.
        let (mut shell, a, b) = shell_with_config(LayoutMode::Single, true);
        assert!(
            !shell.mouse_captured(),
            "starts in selection mode even when mouse enabled"
        );

        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            ShellOutcome::ToggleMouse
        );
        assert!(shell.mouse_captured(), "Ctrl-S grabs capture");

        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            ShellOutcome::ToggleMouse
        );
        assert!(!shell.mouse_captured(), "Ctrl-S again releases capture");

        // The chord is intercepted globally — panels never see it.
        assert_eq!(a.keys.load(Ordering::Relaxed), 0);
        assert_eq!(b.keys.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ctrl_s_is_inert_when_mouse_disabled() {
        // No capture means native selection already works, so the chord is not
        // intercepted (it falls through to the focused panel, harmlessly).
        let (mut shell, a, _b) = shell_with_config(LayoutMode::Single, false);
        assert!(!shell.mouse_captured());
        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            ShellOutcome::Continue
        );
        assert!(!shell.mouse_captured(), "state is untouched");
        // Not intercepted → the focused panel saw the key.
        assert_eq!(a.keys.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn inviolable_chords_are_not_forwarded_to_panels() {
        // These are the guarantees a panel must never be able to revoke, so
        // they are still intercepted before any panel sees them.
        let (mut shell, a, b) = shell_with_two_panels(LayoutMode::Tiled);
        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            ShellOutcome::Continue
        );
        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('2'), KeyModifiers::ALT)),
            ShellOutcome::Continue
        );
        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ShellOutcome::Quit
        );
        assert_eq!(a.keys.load(Ordering::Relaxed), 0);
        assert_eq!(b.keys.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ctrl_c_quits_when_the_focused_panel_declines() {
        // Ctrl-C is a fallback chord so a panel can claim it. A panel that
        // returns `Continue` leaves the shell to quit.
        let (mut shell, a, b) = shell_with_two_panels(LayoutMode::Tiled);
        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ShellOutcome::Quit
        );
        assert_eq!(a.keys.load(Ordering::Relaxed), 1, "it was offered first");
        assert_eq!(b.keys.load(Ordering::Relaxed), 0, "only the focused panel");
    }

    #[test]
    fn a_consuming_panel_blocks_fallback_chords() {
        let (mut shell, a, _b) =
            shell_with_outcome(LayoutMode::Single, false, EventOutcome::Consumed);
        // Ctrl-C consumed → no quit.
        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ShellOutcome::Continue
        );
        // Tab consumed → focus does not move, so the same panel keeps counting.
        shell.handle_event(chord(KeyCode::Tab, KeyModifiers::NONE));
        shell.handle_event(plain_key('x'));
        assert_eq!(
            a.keys.load(Ordering::Relaxed),
            3,
            "focus stayed on a: it saw Ctrl-C, Tab and 'x'"
        );
    }

    #[test]
    fn a_consuming_panel_cannot_trap_the_user() {
        // The property the inviolable class exists for: even a panel that
        // claims every key cannot prevent quitting or defocusing.
        let (mut shell, a, b) =
            shell_with_outcome(LayoutMode::Single, false, EventOutcome::Consumed);
        // Alt+2 still moves focus off the greedy panel.
        shell.handle_event(chord(KeyCode::Char('2'), KeyModifiers::ALT));
        shell.handle_event(plain_key('x'));
        assert_eq!(b.keys.load(Ordering::Relaxed), 1, "focus escaped to b");
        assert_eq!(a.keys.load(Ordering::Relaxed), 0);
        // Ctrl-Q still exits.
        assert_eq!(
            shell.handle_event(chord(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ShellOutcome::Quit
        );
    }

    /// A panel that raises one notification per `ping`, to check the shell
    /// gathers requests from every panel and hands each over once.
    struct PingPanel {
        name: &'static str,
        pending: Option<Notification>,
    }

    impl TuiPanel for PingPanel {
        fn id(&self) -> &str {
            self.name
        }
        fn take_notification(&mut self) -> Option<Notification> {
            self.pending.take()
        }
        fn render(&mut self, _: &mut ratatui::Frame<'_>, _: Rect) {}
    }

    #[test]
    fn notifications_come_from_every_panel_and_are_handed_over_once() {
        let ping = |name, message: &str| -> Box<dyn TuiPanel> {
            Box::new(PingPanel {
                name,
                pending: Some(Notification::new(message)),
            })
        };
        let quiet = Box::new(PingPanel {
            name: "quiet",
            pending: None,
        });
        let mut shell = AppShell::new(
            vec![ping("a", "first"), quiet, ping("c", "background")],
            &TuiConfig::default(),
        );
        // Focus is on the first panel; the third is a background panel.
        let messages: Vec<String> = shell
            .take_notifications()
            .iter()
            .map(|notification| notification.message().to_owned())
            .collect();
        assert_eq!(messages, ["first", "background"]);
        assert!(shell.take_notifications().is_empty());
    }

    #[test]
    fn a_panel_that_never_notifies_costs_nothing() {
        let (mut shell, _, _) = shell_with_two_panels(LayoutMode::Single);
        assert!(shell.take_notifications().is_empty());
    }
}
