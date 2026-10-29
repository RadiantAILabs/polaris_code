//! [`SparqlPanel`] — multi-line SPARQL editor + result pane.
//!
//! Each submitted query runs on `tokio::task::spawn_blocking` against a
//! clone of the [`OxigraphStore`] global. `CONSTRUCT` / `DESCRIBE` results
//! serialize as Turtle; `SELECT` / `ASK` results serialize as SPARQL Results
//! JSON — the same forms the inspect agent's `sparql_query` tool returns, so
//! queries prototyped in the panel transfer straight over.

use crate::oxigraph::OxigraphStore;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use oxigraph::sparql::results::QueryResultsFormat;
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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const TRANSCRIPT_CAP: usize = 256;

/// One entry in the SPARQL panel's transcript.
#[derive(Debug, Clone)]
enum SparqlEntry {
    Query(String),
    Result(String),
    Error(String),
}

/// A multi-line-aware SPARQL prompt + result transcript.
///
/// `Enter` submits the buffered query. `Shift-Enter` and `Ctrl-J` insert a
/// newline so multi-line queries are practical without leaving the panel.
pub struct SparqlPanel {
    store: OxigraphStore,
    transcript: Arc<Mutex<VecDeque<SparqlEntry>>>,
    pending: Arc<AtomicUsize>,
    input: String,
    cursor: usize,
    scroll: u16,
    /// Largest valid `scroll` for the most recent render — recomputed every
    /// paint pass so Down/PageDown can stop at the boundary.
    max_scroll: u16,
    focused: bool,
}

impl SparqlPanel {
    pub(crate) fn new(store: OxigraphStore) -> Self {
        Self {
            store,
            transcript: Arc::new(Mutex::new(VecDeque::new())),
            pending: Arc::new(AtomicUsize::new(0)),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            max_scroll: 0,
            focused: false,
        }
    }

    fn push_entry(t: &Mutex<VecDeque<SparqlEntry>>, entry: SparqlEntry) {
        let mut t = t.lock();
        t.push_back(entry);
        while t.len() > TRANSCRIPT_CAP {
            t.pop_front();
        }
    }

    fn submit_input(&mut self) {
        let query = std::mem::take(&mut self.input);
        self.cursor = 0;
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return;
        }
        Self::push_entry(&self.transcript, SparqlEntry::Query(trimmed.to_owned()));
        let store = self.store.clone();
        let transcript = self.transcript.clone();
        let pending = self.pending.clone();
        pending.fetch_add(1, Ordering::SeqCst);
        let sparql = trimmed.to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = match run_one_query(&store, &sparql) {
                Ok(out) => SparqlEntry::Result(out),
                Err(err) => SparqlEntry::Error(err),
            };
            Self::push_entry(&transcript, entry);
            pending.fetch_sub(1, Ordering::SeqCst);
        });
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let snapshot = {
            let t = self.transcript.lock();
            t.iter().cloned().collect::<Vec<_>>()
        };
        let mut lines = Vec::with_capacity(snapshot.len() * 4);
        for entry in snapshot {
            match entry {
                SparqlEntry::Query(text) => {
                    let mut parts = text.split('\n');
                    if let Some(first) = parts.next() {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "sparql ❯ ",
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(first.to_owned()),
                        ]));
                    }
                    for rest in parts {
                        lines.push(Line::from(vec![
                            Span::raw("         "),
                            Span::raw(rest.to_owned()),
                        ]));
                    }
                }
                SparqlEntry::Result(text) => {
                    for line in text.lines() {
                        lines.push(Line::from(Span::styled(
                            line.to_owned(),
                            Style::default().fg(Color::Gray),
                        )));
                    }
                    lines.push(Line::from(""));
                }
                SparqlEntry::Error(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("error: {text}"),
                        Style::default().fg(Color::Red),
                    )));
                    lines.push(Line::from(""));
                }
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
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match (key.code, ctrl, shift) {
            (KeyCode::Enter, false, false) => {
                self.submit_input();
                EventOutcome::Consumed
            }
            // Shift-Enter or Ctrl-J inserts a newline so multi-line queries
            // are editable in-panel.
            (KeyCode::Enter, false, true) | (KeyCode::Char('j'), true, _) => {
                self.input.insert(self.cursor, '\n');
                self.cursor += 1;
                EventOutcome::Consumed
            }
            (KeyCode::Backspace, false, _) => {
                if self.cursor > 0 {
                    let prev = self.cursor - 1;
                    self.input.remove(prev);
                    self.cursor = prev;
                }
                EventOutcome::Consumed
            }
            (KeyCode::Left, false, _) => {
                self.cursor = self.cursor.saturating_sub(1);
                EventOutcome::Consumed
            }
            (KeyCode::Right, false, _) => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                EventOutcome::Consumed
            }
            (KeyCode::Home, _, _) => {
                self.cursor = 0;
                EventOutcome::Consumed
            }
            (KeyCode::End, _, _) => {
                self.cursor = self.input.chars().count();
                EventOutcome::Consumed
            }
            (KeyCode::PageUp, _, _) => {
                self.scroll = self.scroll.saturating_sub(5);
                EventOutcome::Consumed
            }
            (KeyCode::PageDown, _, _) => {
                self.scroll = self.scroll.saturating_add(5).min(self.max_scroll);
                EventOutcome::Consumed
            }
            (KeyCode::Char('u'), true, _) => {
                self.input.clear();
                self.cursor = 0;
                EventOutcome::Consumed
            }
            (KeyCode::Char(c), false, _) => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
                EventOutcome::Consumed
            }
            _ => EventOutcome::Continue,
        }
    }
}

impl TuiPanel for SparqlPanel {
    fn id(&self) -> &str {
        "sparql"
    }

    fn title(&self) -> &str {
        "SPARQL"
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
        // Reserve roughly a quarter of the panel for the editor (capped at 8
        // lines + border) so multi-line queries are practical but the
        // transcript still dominates.
        let editor_lines = (self.input.lines().count().max(1) as u16).min(8) + 2;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(editor_lines)])
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
            format!(" sparql › running ({pending})… ")
        } else if self.focused {
            " sparql › ".to_owned()
        } else {
            " sparql ".to_owned()
        };
        let editor_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if self.focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let editor_inner = editor_block.inner(chunks[1]);
        frame.render_widget(editor_block, chunks[1]);

        let editor = Paragraph::new(
            self.input
                .split('\n')
                .map(|line| Line::from(line.to_owned()))
                .collect::<Vec<_>>(),
        )
        .wrap(Wrap { trim: false });
        frame.render_widget(editor, editor_inner);

        if self.focused && editor_inner.width > 0 && editor_inner.height > 0 {
            // Place the cursor at the byte-position within the current
            // logical line. We don't bother with grapheme clusters here —
            // SPARQL is ASCII-leaning and the panel is for human editing.
            let (row_idx, col_idx) = byte_cursor_to_row_col(&self.input, self.cursor);
            let row = (row_idx as u16).min(editor_inner.height.saturating_sub(1));
            let col_offset = u16::try_from(col_idx).unwrap_or(u16::MAX);
            let col = editor_inner.x + col_offset.min(editor_inner.width.saturating_sub(1));
            frame.set_cursor_position((col, editor_inner.y + row));
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

/// Translate a byte cursor in `text` into (row, col) for cursor placement.
fn byte_cursor_to_row_col(text: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(text.len());
    let mut row = 0;
    let mut last_nl = 0;
    for (idx, ch) in text.char_indices().take_while(|(i, _)| *i < cursor) {
        if ch == '\n' {
            row += 1;
            last_nl = idx + 1;
        }
    }
    (row, cursor - last_nl)
}

/// Execute one SPARQL query against `store` and render the result.
///
/// `CONSTRUCT` / `DESCRIBE` serialize as Turtle; `SELECT` / `ASK` serialize
/// as SPARQL Results JSON.
fn run_one_query(store: &OxigraphStore, sparql: &str) -> Result<String, String> {
    let results = store.query(sparql).map_err(|e| e.to_string())?;
    let bytes = match results {
        QueryResults::Graph(_) => results
            .write_graph(Vec::<u8>::new(), RdfFormat::Turtle)
            .map_err(|e| e.to_string())?,
        QueryResults::Solutions(_) | QueryResults::Boolean(_) => results
            .write(Vec::<u8>::new(), QueryResultsFormat::Json)
            .map_err(|e| e.to_string())?,
    };
    String::from_utf8(bytes).map_err(|e| e.to_string())
}
