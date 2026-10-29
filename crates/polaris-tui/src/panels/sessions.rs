//! Live list of active sessions read from [`SessionsAPI`].
//!
//! Calls [`SessionsAPI::list_session_metadata`] on every
//! [`TuiEvent::Tick`](crate::event::TuiEvent::Tick). The call is cheap (a
//! `RwLock` read over the in-memory state map) so polling at the tick rate
//! is fine; if your server tracks tens of thousands of live sessions you
//! probably want a different surface anyway.

use crate::event::TuiEvent;
use crate::panel::{EventOutcome, TuiPanel};
use crate::registry::TuiRegistry;
use crossterm::event::KeyCode;
use polaris::sessions::{SessionMetadata, SessionsAPI};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

/// Tabular live view of [`SessionsAPI`] state.
pub struct SessionsPanel {
    api: SessionsAPI,
    rows: Vec<SessionMetadata>,
    state: ListState,
}

impl SessionsPanel {
    /// Construct against a [`SessionsAPI`] clone.
    #[must_use]
    pub fn new(api: SessionsAPI) -> Self {
        Self {
            api,
            rows: Vec::new(),
            state: ListState::default(),
        }
    }

    fn refresh(&mut self) {
        self.rows = self.api.list_session_metadata();
        // Keep selection in range. If we lose the previously selected row
        // (session deleted) just drop to the end of the list.
        match self.state.selected() {
            Some(i) if i >= self.rows.len() && !self.rows.is_empty() => {
                self.state.select(Some(self.rows.len() - 1));
            }
            Some(_) | None if self.rows.is_empty() => {
                self.state.select(None);
            }
            _ => {}
        }
    }
}

impl TuiPanel for SessionsPanel {
    fn id(&self) -> &str {
        "sessions"
    }
    fn title(&self) -> &str {
        "Sessions"
    }

    fn init(&mut self) {
        self.refresh();
    }
    fn tick(&mut self) {
        self.refresh();
    }

    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
        let TuiEvent::Key(key) = event else {
            return EventOutcome::Continue;
        };
        if self.rows.is_empty() {
            return EventOutcome::Continue;
        }
        let cur = self.state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.select(Some(cur.saturating_sub(1)));
                EventOutcome::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.select(Some((cur + 1).min(self.rows.len() - 1)));
                EventOutcome::Consumed
            }
            _ => EventOutcome::Continue,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);

        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{:>3} sessions", self.rows.len()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                "j/k to navigate · refreshed every tick",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .block(Block::default().borders(Borders::BOTTOM));
        frame.render_widget(header, chunks[0]);

        if self.rows.is_empty() {
            let empty =
                Paragraph::new("No active sessions.").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(empty, chunks[1]);
            return;
        }

        let items: Vec<ListItem<'_>> = self
            .rows
            .iter()
            .map(|row| {
                let session_id = format!("{}", row.session_id);
                let agent = row.agent_type.to_string();
                let turn = row.turn_number;
                let status = format!("{:?}", row.status).to_lowercase();
                ListItem::new(Line::from(vec![
                    Span::styled(truncate(&session_id, 12), Style::default().fg(Color::Cyan)),
                    Span::raw("  "),
                    Span::styled(agent, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(format!("turn={turn}"), Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::styled(status, Style::default().fg(Color::Yellow)),
                    Span::raw("  "),
                    Span::styled(row.created_at.clone(), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let list = List::new(items).highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(list, chunks[1], &mut self.state);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Plugin that contributes a [`SessionsPanel`] reading from the server's
/// [`SessionsAPI`].
///
/// Depends on [`SessionsPlugin`] so the API is globalized before `ready()`
/// runs.
#[derive(Default, Debug)]
pub struct SessionsPanelPlugin;

impl Plugin for SessionsPanelPlugin {
    const ID: &'static str = "polaris::tui::sessions_panel";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, _server: &mut Server) {
        // Contribution is deferred to `ready()`, once `SessionsAPI` is
        // available and globalized.
    }

    async fn ready(&self, server: &mut Server) {
        let api = server
            .api::<SessionsAPI>()
            .expect("SessionsPlugin must be added before SessionsPanelPlugin")
            .clone();
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin must be added before SessionsPanelPlugin")
            .add_panel(SessionsPanel::new(api));
    }

    fn access(&self) -> PluginAccess {
        // Requires the registry it contributes to and the `SessionsAPI` it renders.
        // The resolver orders both providers (`TuiPlugin`, `SessionsPlugin`) ahead of
        // this plugin.
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
            .requires::<SessionsAPI>(VersionReq::caret(SessionsAPI::CONTRACT_VERSION))
    }
}
