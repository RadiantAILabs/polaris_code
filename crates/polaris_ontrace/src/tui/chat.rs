//! [`InspectChatPanel`] — input prompt + scrolling transcript wired to
//! [`InspectAPI::ask`](crate::InspectAPI).
//!
//! Each `Enter` press takes the input line, echoes it to the transcript, and
//! spawns a tokio task that awaits [`InspectAPI::ask`]. When the answer
//! lands (success or error), the task pushes a line into the shared
//! transcript buffer. The panel drains the buffer on every tick and re-renders.

use crate::InspectAPI;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use parking_lot::Mutex;
use polaris_tui::ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use polaris_tui::{EventOutcome, TuiEvent, TuiPanel};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

const TRANSCRIPT_CAP: usize = 1024;

/// One entry in the chat transcript.
#[derive(Debug, Clone)]
pub enum ChatLine {
    /// Question typed by the user.
    User(String),
    /// Answer returned by the inspect agent.
    Agent(String),
    /// Error surfaced by the inspect agent or the runtime.
    Error(String),
    /// Diagnostic note (e.g. "inspect API not ready yet").
    Note(String),
}

/// Conversation panel that drives [`InspectAPI::ask`] one question at a time.
pub struct InspectChatPanel {
    api: Arc<OnceLock<InspectAPI>>,
    transcript: Arc<Mutex<VecDeque<ChatLine>>>,
    pending: Arc<AtomicUsize>,
    input: String,
    cursor: usize,
    scroll: u16,
    /// Largest valid `scroll` for the most recent render — i.e. the offset
    /// at which the last line of the transcript sits on the bottom row.
    /// Recomputed every paint pass and used to clamp Down/PageDown so the
    /// view stops at the boundary instead of scrolling into empty space.
    max_scroll: u16,
    focused: bool,
}

impl InspectChatPanel {
    pub(crate) fn new(
        api: Arc<OnceLock<InspectAPI>>,
        transcript: Arc<Mutex<VecDeque<ChatLine>>>,
    ) -> Self {
        Self {
            api,
            transcript,
            pending: Arc::new(AtomicUsize::new(0)),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            max_scroll: 0,
            focused: true,
        }
    }

    fn push_line(&self, line: ChatLine) {
        let mut t = self.transcript.lock();
        t.push_back(line);
        while t.len() > TRANSCRIPT_CAP {
            t.pop_front();
        }
    }

    fn submit_input(&mut self) {
        if self.input.is_empty() {
            return;
        }
        let question = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.push_line(ChatLine::User(question.clone()));

        let Some(api) = self.api.get().cloned() else {
            self.push_line(ChatLine::Note(
                "inspect API not ready yet — try again in a moment".to_owned(),
            ));
            return;
        };

        self.pending.fetch_add(1, Ordering::SeqCst);
        let transcript = self.transcript.clone();
        let pending = self.pending.clone();
        tokio::spawn(async move {
            let entry = match api.ask(question).await {
                Ok(answer) => ChatLine::Agent(answer),
                Err(err) => ChatLine::Error(err.to_string()),
            };
            {
                let mut t = transcript.lock();
                t.push_back(entry);
                while t.len() > TRANSCRIPT_CAP {
                    t.pop_front();
                }
            }
            pending.fetch_sub(1, Ordering::SeqCst);
        });
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let snapshot = {
            let t = self.transcript.lock();
            t.iter().cloned().collect::<Vec<_>>()
        };
        let mut lines = Vec::with_capacity(snapshot.len() * 2);
        for entry in snapshot {
            let (label, color, body) = match entry {
                ChatLine::User(text) => ("you", Color::Cyan, text),
                ChatLine::Agent(text) => ("inspect", Color::Green, text),
                ChatLine::Error(text) => ("error", Color::Red, text),
                ChatLine::Note(text) => ("note", Color::Yellow, text),
            };
            // Multi-line answers: prefix the first line with the label and
            // indent continuations so the visual block reads as one reply.
            let mut parts = body.split('\n');
            if let Some(first) = parts.next() {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{label} ❯ "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(first.to_owned()),
                ]));
            }
            for rest in parts {
                lines.push(Line::from(vec![
                    Span::raw("          "),
                    Span::raw(rest.to_owned()),
                ]));
            }
        }
        lines
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventOutcome {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                EventOutcome::Consumed
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3).min(self.max_scroll);
                EventOutcome::Consumed
            }
            _ => EventOutcome::Continue,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Enter, false) => {
                self.submit_input();
                EventOutcome::Consumed
            }
            (KeyCode::Backspace, false) => {
                if self.cursor > 0 {
                    let prev = self.cursor - 1;
                    self.input.remove(prev);
                    self.cursor = prev;
                }
                EventOutcome::Consumed
            }
            (KeyCode::Left, false) => {
                self.cursor = self.cursor.saturating_sub(1);
                EventOutcome::Consumed
            }
            (KeyCode::Right, false) => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                EventOutcome::Consumed
            }
            (KeyCode::Home, _) => {
                self.cursor = 0;
                EventOutcome::Consumed
            }
            (KeyCode::End, _) => {
                self.cursor = self.input.chars().count();
                EventOutcome::Consumed
            }
            (KeyCode::Up, _) => {
                self.scroll = self.scroll.saturating_sub(1);
                EventOutcome::Consumed
            }
            (KeyCode::Down, _) => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll);
                EventOutcome::Consumed
            }
            (KeyCode::PageUp, _) => {
                self.scroll = self.scroll.saturating_sub(5);
                EventOutcome::Consumed
            }
            (KeyCode::PageDown, _) => {
                self.scroll = self.scroll.saturating_add(5).min(self.max_scroll);
                EventOutcome::Consumed
            }
            (KeyCode::Char('u'), true) => {
                self.input.clear();
                self.cursor = 0;
                EventOutcome::Consumed
            }
            (KeyCode::Char(c), false) => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
                EventOutcome::Consumed
            }
            _ => EventOutcome::Continue,
        }
    }
}

impl TuiPanel for InspectChatPanel {
    fn id(&self) -> &str {
        "inspect"
    }

    fn title(&self) -> &str {
        "Inspect"
    }

    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
        match event {
            TuiEvent::FocusGained => {
                self.focused = true;
                EventOutcome::Continue
            }
            TuiEvent::FocusLost => {
                self.focused = false;
                EventOutcome::Continue
            }
            TuiEvent::Key(key) if self.focused => self.handle_key(*key),
            TuiEvent::Mouse(mouse) if self.focused => self.handle_mouse(*mouse),
            _ => EventOutcome::Continue,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(area);

        let lines = self.transcript_lines();
        let total_lines =
            u16::try_from(wrapped_row_count(&lines, chunks[0].width)).unwrap_or(u16::MAX);
        self.max_scroll = total_lines.saturating_sub(chunks[0].height);
        self.scroll = self.scroll.min(self.max_scroll);
        let transcript = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(transcript, chunks[0]);

        let pending = self.pending.load(Ordering::SeqCst);
        let title = if pending > 0 {
            format!(" › thinking ({pending})… ")
        } else if self.focused {
            " › ".to_owned()
        } else {
            " input ".to_owned()
        };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if self.focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let input_inner = input_block.inner(chunks[1]);
        frame.render_widget(input_block, chunks[1]);

        let input_line = Paragraph::new(Line::from(self.input.as_str()));
        frame.render_widget(input_line, input_inner);

        if self.focused && input_inner.width > 0 {
            let col_offset = u16::try_from(self.cursor).unwrap_or(u16::MAX);
            let x = input_inner.x + col_offset.min(input_inner.width.saturating_sub(1));
            frame.set_cursor_position((x, input_inner.y));
        }
    }
}

/// Estimate how many terminal rows `lines` occupy when wrapped at `width`.
///
/// `Paragraph::line_count` is gated behind ratatui's unstable
/// `rendered-line-info` feature, so we approximate by dividing each logical
/// line's char count by the available width. Word-boundary wrapping can
/// nudge the real count a row or two higher than this, but the estimate is
/// tight enough to keep Down/PageDown from running off the end.
fn wrapped_row_count(lines: &[Line<'_>], width: u16) -> usize {
    if width == 0 {
        return lines.len();
    }
    let w = usize::from(width);
    lines
        .iter()
        .map(|line| {
            let chars: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            chars.div_ceil(w).max(1)
        })
        .sum()
}
