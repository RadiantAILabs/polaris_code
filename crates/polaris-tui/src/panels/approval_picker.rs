//! The approval picker: a selectable list drawn in place of the composer while
//! an approval is pending.
//!
//! The picker owns everything about *looking at and answering* one approval:
//! the selected item, the command and how much of it has been seen, the
//! optional note, the refusal of an answer, and the drawing. It does not send
//! anything. Key handling returns an [`Outcome`] and the conversation panel
//! turns an answer into the same message a typed answer would have produced.
//!
//! # Layout
//!
//! ```text
//! ┌ approve? · run_command in ~/proj ────────────────────────────┐
//! │ cargo test -p polaris_code --test permission_modes            │
//! ├───────────────────────────────────────────────────────────────┤
//! │ ❯ 1. allow once                                               │
//! │   2. allow for session                                        │
//! │   3. deny                                                     │
//! └ ↑↓ move · 1-3 pick · Enter confirm · Tab add note · Esc deny ─┘
//! ```
//!
//! Rows are given out in a fixed order, so that nothing needed to answer
//! safely is pushed off screen. Inside the borders:
//!
//! 1. the note row, while a note is open;
//! 2. one row of the answer list (it follows the highlighted item);
//! 3. one row of the command (the *subject*);
//! 4. the notice row, if there is a notice to show;
//! 5. the rest of the answer list;
//! 6. whatever is left goes to the command.
//!
//! A command taller than its rows ends with a `… N more rows` line. `N` counts
//! wrapped rows, not newlines, so a single very long line is counted too. The
//! command scrolls with `PgUp`/`PgDn` (or `Shift+↑`/`Shift+↓`). `?` adds the
//! full prompt below the command; the same scroll covers both.
//!
//! The box never takes more rows than it is given. Under three rows it has no
//! room for borders and a list row, so it becomes a bar of one line (the
//! refusal if the last answer was refused, else the note field when one is
//! open, else the highlighted item) or two (that line, then the refusal or
//! the key hint). No part of the command is drawn in a bar. A notice with no
//! row of its own takes the place of the key hint on the bottom border.
//!
//! # The command must be seen first
//!
//! The command is what a confirming answer runs, so a confirming answer — an
//! item the host marked `confirmed`, picked by digit, alias or `Enter` — is
//! refused, with the reason on screen, until the command's last row has been
//! *drawn* on a frame. Rows are counted by grapheme cluster, the way ratatui
//! draws them; an `… N more rows` marker being visible is not enough. This
//! only applies to a command the host named ([`APPROVAL_SUBJECT`]); the first
//! line of the prompt that stands in for a missing one is not gated.
//!
//! - A scroll moves at most one page past the last frame drawn, so a burst of
//!   input between two frames cannot skip rows. A new width starts the read
//!   again from the top.
//! - A cluster wider than the box can only be drawn as a `�`. A confirming
//!   answer is refused until the terminal is wide enough to show it.
//! - A box with no room to draw even one row of the command refuses every
//!   confirming answer and says to enlarge the terminal.
//! - A command over 64 KiB cannot be shown in full and can only be denied.
//!
//! Denying is never refused: `Esc` and every item that is not `confirmed`
//! always work. A refusal leaves the picker open with any note kept, and goes
//! away on the next key press.
//!
//! # Host text
//!
//! The command and the full prompt are cut to 64 KiB before they are
//! sanitised, sanitised once, and wrapped once per width. A frame only copies
//! the rows it shows. Text drawn on every frame (labels, the context, the
//! first line of the prompt) is cut to 2 KiB on arrival, and a cut one ends in
//! `…`. Control characters and zero-width characters are dropped from all of
//! it.
//!
//! # Stray keys
//!
//! One key press answers, so a word typed into the composer when the picker
//! opens could approve something. A key that can answer (Enter, a digit or an
//! alias such as `y`) is ignored while other keys were pressed less than
//! [`QUIET`] earlier. Moving, scrolling, `Tab` and `Esc` are never ignored.
//!
//! [`APPROVAL_SUBJECT`]: super::conversation::APPROVAL_SUBJECT

use super::conversation::{
    ApprovalChoice, bounded_label, bounded_safe_text, cell_width, hard_wrap,
};
use crate::composer::Composer;
use crate::transcript::safe_text;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

/// Most bytes of the command or the full prompt that are looked at. The rest
/// is dropped before any sanitising.
pub(super) const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Most bytes of a note.
const MAX_NOTE_BYTES: usize = 1024;
/// How long a key that could answer is ignored after another key press.
const QUIET: Duration = Duration::from_millis(400);
/// Columns the list draws before a label: the pointer, the digit and spaces.
const LABEL_INDENT: usize = 5;
/// Rows that show the borders, one answer and one row of the command: the
/// least the panel should try to give the picker.
pub(crate) const COMFORTABLE_ROWS: u16 = 4;

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    /// Whether the chosen item authorizes the action.
    pub confirmed: bool,
    /// The host's id for the item, or `None` for the built-in allow/deny pair.
    pub id: Option<String>,
    /// The first alias of the item: the text a typed answer would have used.
    pub key: String,
    /// The item's label with terminal controls removed, for the transcript.
    pub label: String,
    /// The note, without control characters and trimmed. `None` when empty.
    pub note: Option<String>,
}

/// What a key press did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// An item was chosen.
    Answer(Answer),
    /// The user pressed `Esc`: deny without choosing an item.
    Deny,
    /// The picker used the key, or dropped it on purpose.
    Consumed,
    /// The picker has no use for the key; the panel may.
    NotMine,
}

/// Why a confirming answer is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// The command is too long to show, so only a denial is accepted.
    TooLong,
    /// A cluster of the command is wider than the box.
    Narrow,
    /// The box has no room to draw any of the command.
    Cramped,
    /// The last row of the command has not been drawn yet.
    Unseen,
}

impl Gate {
    /// The reason in full, and a shorter one for a narrow line. Each names the
    /// key or the action that helps, and the way to deny, which is always open.
    fn texts(self) -> [&'static str; 2] {
        match self {
            Self::TooLong => [
                "too long to review here, so it can only be denied (Esc)",
                "too long: deny only (Esc)",
            ],
            Self::Narrow => [
                "too narrow to show the command: widen the terminal, or deny (Esc)",
                "too narrow: widen it or deny (Esc)",
            ],
            Self::Cramped => [
                "too small to show the command: enlarge the terminal, or deny (Esc)",
                "too small: enlarge it or deny (Esc)",
            ],
            Self::Unseen => [
                "scroll to the end of the command (PgDn or Shift+↓) before approving it, or deny (Esc)",
                "scroll to the end (PgDn) or deny (Esc)",
            ],
        }
    }

    /// The longest reason that fits `room` columns; the short one is cut with
    /// `…` when even it does not.
    fn fit(self, room: usize) -> String {
        let [full, short] = self.texts();
        if clip(full, room) == full {
            full.to_string()
        } else {
            clip(short, room)
        }
    }
}

/// The command (and under `?`, the full prompt after it) wrapped to one width.
struct Body {
    width: usize,
    details: bool,
    rows: Vec<Line<'static>>,
    /// The row just past the command and its omission note: once the row
    /// before this one has been drawn, all of the command has.
    subject_end: usize,
    /// Whether every cluster of the command fits `width`, so that no row holds
    /// a stand-in for one (see [`hard_wrap`]).
    faithful: bool,
}

/// One line of the answer list.
struct Item {
    id: Option<String>,
    text: String,
    keys: Vec<String>,
    confirmed: bool,
    /// `text` wrapped to `width`.
    width: usize,
    rows: Vec<String>,
}

impl Item {
    fn new(id: Option<String>, label: &str, keys: &[&str], confirmed: bool) -> Self {
        Self {
            id,
            text: bounded_label(&label.replace(['\n', '\r'], " ")),
            keys: keys.iter().map(|key| (*key).to_string()).collect(),
            confirmed,
            width: 0,
            rows: Vec::new(),
        }
    }

    /// The label's rows at `width`, wrapped on first use and again only when
    /// the width changes.
    fn rows(&mut self, width: usize) -> &[String] {
        let width = width.max(1);
        if self.width != width {
            self.rows = hard_wrap(&self.text, width);
            self.width = width;
        }
        &self.rows
    }
}

/// How the rows of the box are shared out at one size.
struct Plan {
    /// Whether the box is a bar with no borders.
    bar: bool,
    height: usize,
    /// Whether the note field has its row.
    note: bool,
    /// Rows of the answer list drawn.
    list: usize,
    /// Whether the panel's notice, or a refusal, has a row of its own.
    notice_row: bool,
    /// Rows of the command drawn.
    content: usize,
    /// Whether the command ends with the `… N more rows` line.
    marker: bool,
    separator: bool,
}

/// Take up to `want` rows out of `room`.
fn take(room: &mut usize, want: usize) -> usize {
    let got = want.min(*room);
    *room -= got;
    got
}

/// The selectable approval list.
pub(crate) struct ApprovalPicker {
    items: Vec<Item>,
    selected: usize,
    context: Option<String>,
    /// What the body shows first: the command, or the prompt's first line.
    text: Option<String>,
    /// Bytes of the command past the 64 KiB bound.
    omitted: usize,
    /// Whether `text` is a command the host named, which gates confirming
    /// answers.
    gated: bool,
    /// The full prompt, shown below the command on `?`.
    details: Option<String>,
    show_details: bool,
    body: Option<Body>,
    /// First body row to draw next.
    scroll: usize,
    /// First body row the last frame drew. Input is handled in batches
    /// between frames, so `scroll` may move many times before the next draw;
    /// it is never moved more than a page past this, so the rows in between
    /// are drawn on the way down rather than skipped.
    shown_scroll: usize,
    /// Body rows the last frame had room for.
    viewport: usize,
    /// Whether the command's last row has been drawn.
    seen_end: bool,
    /// Whether the last frame had no room to draw any of the command.
    cramped: bool,
    /// Whether the last confirming answer was refused and the reason is still
    /// to be shown.
    refused: bool,
    /// The note field while it is open.
    note: Option<Composer>,
    /// When the last key that could not answer was pressed.
    stray: Option<Instant>,
}

impl ApprovalPicker {
    /// Build a picker for one approval.
    ///
    /// `choices` are the host's answers; with none, the picker offers allow and
    /// deny. `subject` is the command the host named, if it did: a confirming
    /// answer waits until all of it has been drawn. `summary` stands in for it
    /// when the host named nothing, and gates nothing. `details` is the full
    /// prompt, shown below on `?`. `last_key` is when the panel last saw a key
    /// press, so keys typed just before the picker opened cannot answer it.
    ///
    /// Everything is bounded and sanitised here, once.
    pub(crate) fn new(
        choices: &[ApprovalChoice],
        subject: Option<&str>,
        summary: &str,
        context: Option<&str>,
        details: &str,
        last_key: Option<Instant>,
    ) -> Self {
        let items = if choices.is_empty() {
            vec![
                Item::new(None, "allow", &["y", "yes"], true),
                Item::new(None, "deny", &["n", "no"], false),
            ]
        } else {
            choices
                .iter()
                .map(|choice| {
                    let keys = choice.keys.iter().map(String::as_str).collect::<Vec<_>>();
                    Item::new(
                        Some(choice.id.clone()),
                        &choice.label,
                        &keys,
                        choice.confirmed,
                    )
                })
                .collect()
        };
        let shown = subject.unwrap_or(summary);
        let (text, omitted) = match subject {
            Some(subject) => {
                let (mut text, omitted) = bounded_safe_text(subject, MAX_TEXT_BYTES);
                text.truncate(text.trim_end_matches('\n').len());
                (Some(text), omitted)
            }
            None => (
                (!summary.trim().is_empty()).then(|| bounded_label(summary)),
                0,
            ),
        };
        let details = (!details.trim().is_empty() && details.trim() != shown.trim()).then(|| {
            let (mut text, cut) = bounded_safe_text(details, MAX_TEXT_BYTES);
            text.truncate(text.trim_end_matches('\n').len());
            if cut > 0 {
                text.push_str(&format!(
                    "\n… {cut} more bytes not shown; the request is retained in full"
                ));
            }
            text
        });
        Self {
            items,
            selected: 0,
            context: context.map(|text| bounded_label(text).replace('\n', " ")),
            text,
            omitted,
            gated: subject.is_some(),
            details,
            show_details: false,
            body: None,
            scroll: 0,
            shown_scroll: 0,
            viewport: 0,
            seen_end: false,
            cramped: false,
            refused: false,
            note: None,
            stray: last_key,
        }
    }

    /// Index of the highlighted item.
    #[cfg(test)]
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// Accept answer keys now, as if nothing had been typed before.
    #[cfg(test)]
    pub(crate) fn settle(&mut self) {
        self.stray = None;
    }

    /// Whether the note field is open.
    #[cfg(test)]
    pub(crate) fn note_open(&self) -> bool {
        self.note.is_some()
    }

    /// The first body row the next frame draws.
    #[cfg(test)]
    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    /// Why a confirming answer is refused right now, or `None` once the whole
    /// command has been on screen.
    fn blocked(&self) -> Option<Gate> {
        if !self.gated {
            return None;
        }
        if self.omitted > 0 {
            return Some(Gate::TooLong);
        }
        // A stand-in hides what the command says, so nothing is approved
        // until a wider terminal shows it as it is.
        if self.body.as_ref().is_some_and(|body| !body.faithful) {
            return Some(Gate::Narrow);
        }
        (!self.seen_end).then_some(if self.cramped {
            Gate::Cramped
        } else {
            Gate::Unseen
        })
    }

    // — Keys ———————————————————————————————————————————————————

    /// Handle one key press at time `now`.
    pub(crate) fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Outcome {
        self.refused = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
        if self.note.is_some() {
            return self.note_key(key, ctrl, alt);
        }
        if ctrl || alt {
            return Outcome::NotMine;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => Outcome::Deny,
            KeyCode::Up if shift => self.scroll_by(-1),
            KeyCode::Down if shift => self.scroll_by(1),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp => self.page(-1),
            KeyCode::PageDown => self.page(1),
            KeyCode::Tab => {
                self.note = Some(Composer::new());
                Outcome::Consumed
            }
            KeyCode::Enter => self.answer(self.selected, None, Some(now)),
            KeyCode::Char('?') => {
                if self.details.is_some() {
                    self.show_details = !self.show_details;
                }
                Outcome::Consumed
            }
            KeyCode::Char(c) => self.char_key(c, now),
            _ => Outcome::NotMine,
        }
    }

    /// A plain character while the list has focus: a digit, an alias, or a
    /// vim-style move. Anything else is a stray key.
    fn char_key(&mut self, c: char, now: Instant) -> Outcome {
        if let Some(digit) = c.to_digit(10).filter(|digit| *digit >= 1) {
            let index = digit as usize - 1;
            return if index < self.items.len() {
                self.answer(index, None, Some(now))
            } else {
                self.stray(now)
            };
        }
        let mut buffer = [0; 4];
        let typed = &*c.encode_utf8(&mut buffer);
        if let Some(index) = self
            .items
            .iter()
            .position(|item| item.keys.iter().any(|key| key.eq_ignore_ascii_case(typed)))
        {
            return self.answer(index, None, Some(now));
        }
        match c {
            'k' => self.step(-1),
            'j' => self.step(1),
            _ => self.stray(now),
        }
    }

    /// Keys while the note field is open. Nothing here is guarded against
    /// stray keys: the user opened the field on purpose.
    fn note_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool) -> Outcome {
        match (key.code, ctrl, alt) {
            (KeyCode::Esc | KeyCode::Tab, false, false) => {
                self.note = None;
                Outcome::Consumed
            }
            (KeyCode::Enter, false, false) => {
                let note = self.note.as_ref().map(|note| note.text().to_string());
                self.answer(self.selected, note.as_deref(), None)
            }
            (KeyCode::Up, false, false) => self.step(-1),
            (KeyCode::Down, false, false) => self.step(1),
            (KeyCode::PageUp, false, false) => self.page(-1),
            (KeyCode::PageDown, false, false) => self.page(1),
            (code, ..) => {
                let Some(note) = self.note.as_mut() else {
                    return Outcome::NotMine;
                };
                match (code, ctrl, alt) {
                    (KeyCode::Left, false, false) => note.move_left(),
                    (KeyCode::Right, false, false) => note.move_right(),
                    (KeyCode::Home, false, false) | (KeyCode::Char('a'), true, false) => {
                        note.move_line_start();
                    }
                    (KeyCode::End, false, false) | (KeyCode::Char('e'), true, false) => {
                        note.move_line_end();
                    }
                    (KeyCode::Backspace, false, false) => note.backspace(),
                    (KeyCode::Delete, false, false) => note.delete_forward(),
                    (KeyCode::Char('u'), true, false) => {
                        let at = note.cursor();
                        let start = note.line_start(at);
                        note.delete_range(start, at);
                    }
                    (KeyCode::Char('w'), true, false) => {
                        let (from, to) = (note.prev_word(), note.cursor());
                        note.delete_range(from, to);
                    }
                    (KeyCode::Char(c), false, false) => {
                        if !c.is_control() && note.text().len() + c.len_utf8() <= MAX_NOTE_BYTES {
                            note.insert_char(c);
                        }
                    }
                    _ => return Outcome::NotMine,
                }
                Outcome::Consumed
            }
        }
    }

    /// Insert pasted text into the note field. Without an open field the
    /// paste is dropped: the composer is hidden, so it must not land there.
    pub(crate) fn paste(&mut self, text: &str) {
        let Some(note) = self.note.as_mut() else {
            return;
        };
        let room = MAX_NOTE_BYTES.saturating_sub(note.text().len());
        let cut = text.floor_char_boundary(room.min(text.len()));
        let clean = safe_text(&text[..cut]).replace('\n', " ");
        note.insert_paste(&clean);
    }

    /// Move the highlight, wrapping at both ends.
    fn step(&mut self, by: isize) -> Outcome {
        let count = self.items.len() as isize;
        self.selected = (self.selected as isize + by).rem_euclid(count) as usize;
        Outcome::Consumed
    }

    /// Scroll the body by `by` rows (negative is up), within what the last
    /// frame measured. Without anything to scroll the key is not the picker's.
    fn scroll_by(&mut self, by: isize) -> Outcome {
        let total = self.body.as_ref().map_or(0, |body| body.rows.len());
        if self.viewport == 0 || total <= self.viewport {
            return Outcome::NotMine;
        }
        let last = total - self.viewport;
        // Down to the next page at most: the rows between were never drawn.
        let reach = self.shown_scroll + self.viewport;
        self.scroll = self.scroll.saturating_add_signed(by).min(last).min(reach);
        Outcome::Consumed
    }

    fn page(&mut self, direction: isize) -> Outcome {
        let rows = self.viewport.saturating_sub(1).max(1) as isize;
        self.scroll_by(direction * rows)
    }

    /// Record a key that was not an answer, so the next answer key waits.
    fn stray(&mut self, now: Instant) -> Outcome {
        self.stray = Some(now);
        Outcome::Consumed
    }

    /// Answer with item `index`. `guard` is the time of an answer key that
    /// must wait out [`QUIET`]; the note field passes `None`.
    ///
    /// A confirming answer is refused, and the picker left as it is, while the
    /// command has not been seen. Denying is never refused.
    fn answer(&mut self, index: usize, note: Option<&str>, guard: Option<Instant>) -> Outcome {
        if let Some(now) = guard {
            let quiet = self
                .stray
                .is_none_or(|at| now.saturating_duration_since(at) >= QUIET);
            if !quiet {
                return self.stray(now);
            }
        }
        let item = &self.items[index];
        if item.confirmed && self.blocked().is_some() {
            self.refused = true;
            return Outcome::Consumed;
        }
        let note = note
            .map(|note| {
                let cut = note.floor_char_boundary(MAX_NOTE_BYTES.min(note.len()));
                safe_text(&note[..cut])
                    .replace('\n', " ")
                    .trim()
                    .to_string()
            })
            .filter(|note| !note.is_empty());
        Outcome::Answer(Answer {
            confirmed: item.confirmed,
            id: item.id.clone(),
            key: item
                .keys
                .first()
                .cloned()
                .unwrap_or_else(|| if item.confirmed { "yes" } else { "no" }.to_string()),
            label: item.text.clone(),
            note,
        })
    }

    // — Layout —————————————————————————————————————————————————

    /// The rows of the whole list with labels `inner` columns wide.
    fn list_rows(&mut self, inner: usize) -> usize {
        let width = inner.saturating_sub(LABEL_INDENT);
        self.items
            .iter_mut()
            .map(|item| item.rows(width).len())
            .sum()
    }

    /// Wrap the body to `width` and return its rows, reusing the last wrap
    /// when neither the width nor the `?` state changed.
    fn ensure_body(&mut self, width: usize) -> usize {
        let width = width.max(1);
        let fresh = self
            .body
            .as_ref()
            .is_some_and(|body| body.width == width && body.details == self.show_details);
        if !fresh {
            // A reflow moves the rows, so the offset no longer points at what
            // was read: start again from the top.
            if self.body.as_ref().is_some_and(|body| body.width != width) {
                self.scroll = 0;
                self.shown_scroll = 0;
            }
            self.body = self.wrap(width);
        }
        self.body.as_ref().map_or(0, |body| body.rows.len())
    }

    /// Hard-wrap the command to `width`, with the full prompt after it under
    /// `?`.
    fn wrap(&self, width: usize) -> Option<Body> {
        let text = self.text.as_ref()?;
        let strong = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let mut rows = hard_wrap(text, width)
            .into_iter()
            .map(|row| Line::styled(row, strong))
            .collect::<Vec<_>>();
        if self.omitted > 0 {
            rows.extend(
                hard_wrap(
                    &format!(
                        "… {} more bytes not shown: too long to review here, so only a denial is accepted",
                        self.omitted
                    ),
                    width,
                )
                .into_iter()
                .map(|row| Line::styled(row, Style::default().fg(Color::Yellow))),
            );
        }
        let subject_end = rows.len();
        if self.show_details
            && let Some(details) = &self.details
        {
            rows.push(Line::default());
            rows.extend(
                hard_wrap(details, width)
                    .into_iter()
                    .map(|row| Line::styled(row, dim)),
            );
        }
        let faithful = !text
            .graphemes(true)
            .any(|grapheme| cell_width(grapheme) > width);
        Some(Body {
            width,
            details: self.show_details,
            rows,
            subject_end,
            faithful,
        })
    }

    /// Share `max` rows out for a box `width` columns wide.
    fn plan(&mut self, width: u16, max: u16, notice: bool) -> Plan {
        let max = usize::from(max).max(1);
        let notice = notice || self.refused;
        if max < 3 {
            // No room for two borders and a row: a bar. Nothing of the
            // command is drawn, so there is nothing to scroll.
            self.body = None;
            self.scroll = 0;
            self.shown_scroll = 0;
            return Plan {
                bar: true,
                height: max,
                note: self.note.is_some(),
                list: 1,
                notice_row: false,
                content: 0,
                marker: false,
                separator: false,
            };
        }
        let inner = usize::from(width.saturating_sub(2));
        let wrap_width = inner.max(1);
        let list_total = self.list_rows(wrap_width);
        let body = self.ensure_body(wrap_width);
        let mut room = max - 2;
        let note = self.note.is_some();
        let note_row = take(&mut room, usize::from(note)) == 1;
        let first = take(&mut room, 1);
        let body_min = take(&mut room, usize::from(body > 0));
        // A command of more than one row may be cut, and a cut one is marked.
        let marker_min = take(&mut room, usize::from(body > 1));
        let notice_row = notice && take(&mut room, 1) == 1;
        let more = take(&mut room, list_total.saturating_sub(first));
        let list = first + more;
        let left = body_min + marker_min + room;
        let (content, marker, separator) = match (body, left) {
            (0, _) | (_, 0) => (0, false, false),
            (body, left) if left > body => (body, false, true),
            (body, left) if left == body => (body, false, false),
            (_, 1) => (1, false, false),
            (_, left) => (left - 1, true, false),
        };
        let used = content + usize::from(marker) + usize::from(separator);
        Plan {
            bar: false,
            height: 2 + usize::from(note_row) + list + usize::from(notice_row) + used,
            note: note_row,
            list,
            notice_row,
            content,
            marker,
            separator,
        }
    }

    /// The height the box wants at `width`, never above `max`.
    pub(crate) fn height(&mut self, width: u16, max: u16, notice: bool) -> u16 {
        u16::try_from(self.plan(width, max, notice).height).unwrap_or(max)
    }

    // — Drawing ————————————————————————————————————————————————

    /// Draw the box at the bottom of `area`. `notice` is a line from the panel
    /// to show when there is no refusal to show. Returns where the caret goes
    /// while the note field is open.
    pub(crate) fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        notice: Option<&str>,
    ) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let plan = self.plan(area.width, area.height, notice.is_some());
        let height = u16::try_from(plan.height)
            .unwrap_or(area.height)
            .min(area.height);
        let area = Rect {
            y: area.bottom() - height,
            height,
            ..area
        };
        if plan.bar {
            return self.render_bar(frame, area, notice);
        }
        let border = Style::default().fg(Color::Yellow);
        let dim = Style::default().fg(Color::DarkGray);
        let warn = Style::default().fg(Color::Yellow);
        let red = Style::default().fg(Color::Red);
        let inner = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        // What this frame draws is what has been seen: nothing is drawn into
        // a box with no columns or no body rows.
        let drawn = plan.content > 0 && inner.width > 0;
        self.viewport = if drawn { plan.content } else { 0 };
        self.cramped = !drawn;
        let total = self.body.as_ref().map_or(0, |body| body.rows.len());
        self.scroll = self.scroll.min(total.saturating_sub(self.viewport));
        self.shown_scroll = self.scroll;
        let scroll = self.scroll;
        if drawn
            && let Some(body) = &self.body
            && body.faithful
            && scroll < body.subject_end
            && scroll + plan.content >= body.subject_end
        {
            self.seen_end = true;
        }
        // The refusal asked for the rest of the command, or a wider box; once
        // that has happened the advice is stale.
        if self.refused && self.blocked().is_none() {
            self.refused = false;
        }
        let refusal = self.refused.then(|| self.blocked()).flatten();
        let notice = refusal
            .map(|gate| gate.fit(inner.width.into()))
            .or_else(|| {
                notice.map(|text| clip(&safe_text(text).replace('\n', " "), inner.width.into()))
            });

        let width = usize::from(inner.width);
        let mut rows: Vec<Line<'static>> = Vec::with_capacity(plan.height);
        if let Some(body) = &self.body {
            rows.extend(body.rows.iter().skip(scroll).take(plan.content).cloned());
            if plan.marker {
                let above = scroll;
                let below = total.saturating_sub(scroll + plan.content);
                rows.push(Line::styled(clip(&marker_text(below, above), width), warn));
            }
        }
        if plan.separator {
            rows.push(Line::styled("─".repeat(width), border));
        }
        let region = rows.len();
        rows.extend(self.list_lines(width, plan.list));
        let mut caret = None;
        if plan.note
            && let Some((line, column)) = self.note_line(width)
        {
            caret = Some(column);
            rows.push(line);
        }
        if plan.notice_row {
            rows.push(match &notice {
                Some(text) => Line::styled(text.clone(), red),
                None => Line::default(),
            });
        }

        let title = match &self.context {
            Some(context) => format!(" approve? · {context} "),
            None => " approve? ".to_string(),
        };
        let bottom = match (&notice, plan.notice_row) {
            (Some(text), false) => {
                Line::styled(format!(" {} ", clip(text, width.saturating_sub(2))), red)
            }
            _ => Line::styled(
                self.hint(width, total > self.viewport && self.viewport > 0),
                dim,
            ),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(Span::styled(
                clip(&title, width),
                border.add_modifier(Modifier::BOLD),
            ))
            .title_bottom(bottom);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(rows), inner);
        if plan.separator {
            let y = inner.y + u16::try_from(plan.content).unwrap_or(0);
            if y < inner.bottom() {
                let buffer = frame.buffer_mut();
                buffer.set_string(area.x, y, "├", border);
                buffer.set_string(area.right().saturating_sub(1), y, "┤", border);
            }
        }
        let note_y = inner.y + u16::try_from(region + plan.list).unwrap_or(0);
        caret.and_then(|column| {
            let x = inner.x + u16::try_from(column).unwrap_or(0);
            (note_y < inner.bottom() && x < inner.right()).then_some((x, note_y))
        })
    }

    /// Draw the bar used when there are fewer than three rows: no borders and
    /// no part of the command. One row shows the refusal if there is one, else
    /// the note field, else the highlighted item. A second row, if there is
    /// one, carries the refusal, a notice from the panel, or the key hint.
    fn render_bar(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        notice: Option<&str>,
    ) -> Option<(u16, u16)> {
        let dim = Style::default().fg(Color::DarkGray);
        let red = Style::default().fg(Color::Red);
        self.viewport = 0;
        self.cramped = true;
        if self.refused && self.blocked().is_none() {
            self.refused = false;
        }
        let width = usize::from(area.width);
        let refusal = self
            .refused
            .then(|| self.blocked())
            .flatten()
            .map(|gate| Line::styled(gate.fit(width), red));
        let content = match self.note_line(width) {
            Some((line, column)) => (line, Some(column)),
            None => (self.item_line(width), None),
        };
        let mut lines = Vec::with_capacity(2);
        let mut caret = None;
        if area.height == 1 {
            match refusal {
                Some(line) => lines.push(line),
                None => {
                    caret = content.1;
                    lines.push(content.0);
                }
            }
        } else {
            caret = content.1;
            lines.push(content.0);
            lines.push(refusal.unwrap_or_else(|| match notice {
                Some(text) => Line::styled(clip(&safe_text(text).replace('\n', " "), width), red),
                None => Line::styled(self.hint(width, false), dim),
            }));
        }
        frame.render_widget(Paragraph::new(lines), area);
        caret.and_then(|column| {
            let x = area.x + u16::try_from(column).unwrap_or(0);
            (x < area.right()).then_some((x, area.y))
        })
    }

    /// The visible rows of the list, windowed so the selected item shows.
    fn list_lines(&mut self, width: usize, rows: usize) -> Vec<Line<'static>> {
        let label_width = width.saturating_sub(LABEL_INDENT);
        let selected = self.selected;
        let mut lines = Vec::new();
        let mut selected_rows = 0..0;
        for (index, item) in self.items.iter_mut().enumerate() {
            let item_rows = item.rows(label_width);
            let first = lines.len();
            let style = if index == selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            for (n, row) in item_rows.iter().enumerate() {
                let lead = if n == 0 {
                    lead(index, index == selected)
                } else {
                    " ".repeat(LABEL_INDENT)
                };
                lines.push(Line::from(vec![
                    Span::styled(lead, style),
                    Span::styled(row.clone(), style),
                ]));
            }
            if index == selected {
                selected_rows = first..lines.len();
            }
        }
        let skip = selected_rows
            .end
            .saturating_sub(rows)
            .min(selected_rows.start);
        lines.into_iter().skip(skip).take(rows).collect()
    }

    /// The highlighted item on one line, cut to `width`.
    fn item_line(&self, width: usize) -> Line<'static> {
        let text = self.items.get(self.selected).map_or("", |i| &i.text);
        Line::styled(
            clip(&format!("{}{text}", lead(self.selected, true)), width),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    }

    /// The note row and the caret column in it, while the note is open.
    fn note_line(&self, width: usize) -> Option<(Line<'static>, usize)> {
        let note = self.note.as_ref()?;
        let deny = self.items.get(self.selected).is_some_and(|i| !i.confirmed);
        let lead = if deny { "  reason: " } else { "  note: " };
        let lead = clip(lead, width);
        let lead_width = lead.chars().count();
        let room = width.saturating_sub(lead_width).max(1);
        let (text, column) = note_window(note.text(), note.cursor(), room);
        let body = if note.text().is_empty() {
            Span::styled(
                clip("(optional) type, then Enter", room),
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Span::raw(text)
        };
        Some((
            Line::from(vec![
                Span::styled(lead, Style::default().fg(Color::Yellow)),
                body,
            ]),
            lead_width + column,
        ))
    }

    /// The keys that fit on the bottom border. Whole hints are dropped, lowest
    /// priority first, rather than cut in the middle.
    fn hint(&self, inner: usize, scrollable: bool) -> String {
        let count = self.items.len().min(9);
        let mut hints: Vec<(u8, String)> = if self.note.is_some() {
            vec![
                (1, "↑↓ move".to_string()),
                (0, "Enter send".to_string()),
                (0, "Esc cancel note".to_string()),
            ]
        } else {
            let pick = if count == 1 {
                "1 pick".to_string()
            } else {
                format!("1-{count} pick")
            };
            let mut hints = vec![
                (2, "↑↓ move".to_string()),
                (3, pick),
                (1, "Enter confirm".to_string()),
                (4, "Tab add note".to_string()),
            ];
            if scrollable {
                hints.push((2, "PgUp/PgDn scroll".to_string()));
            }
            if self.details.is_some() {
                hints.push((
                    5,
                    if self.show_details {
                        "? hide".to_string()
                    } else {
                        "? details".to_string()
                    },
                ));
            }
            hints.push((0, "Esc deny".to_string()));
            hints
        };
        let mut by_priority = (0..hints.len()).collect::<Vec<_>>();
        by_priority.sort_by_key(|index| hints[*index].0);
        let mut keep = vec![false; hints.len()];
        // A leading and a trailing space, and " · " between hints.
        let mut used = 2;
        for index in by_priority {
            let cost = hints[index].1.chars().count() + if used > 2 { 3 } else { 0 };
            if used + cost <= inner {
                used += cost;
                keep[index] = true;
            }
        }
        let text = hints
            .drain(..)
            .zip(keep)
            .filter_map(|((_, text), keep)| keep.then_some(text))
            .collect::<Vec<_>>()
            .join(" · ");
        format!(" {text} ")
    }
}

/// The start of an item's first row: the pointer and the number.
fn lead(index: usize, selected: bool) -> String {
    match (selected, index < 9) {
        (true, true) => format!("❯ {}. ", index + 1),
        (false, true) => format!("  {}. ", index + 1),
        (true, false) => "❯    ".to_string(),
        (false, false) => " ".repeat(LABEL_INDENT),
    }
}

/// Cut `text` to `width` columns, ending with `…` when something was cut.
fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for grapheme in text.graphemes(true) {
        let columns = cell_width(grapheme);
        if used + columns > width {
            if width > 0 {
                while used + 1 > width {
                    let Some(last) = out.graphemes(true).next_back().map(str::to_owned) else {
                        break;
                    };
                    out.truncate(out.len() - last.len());
                    used -= cell_width(&last);
                }
                out.push('…');
            }
            return out;
        }
        out.push_str(grapheme);
        used += columns;
    }
    out
}

/// The line under a command that does not fit.
fn marker_text(below: usize, above: usize) -> String {
    let rows = |n: usize| if n == 1 { "row" } else { "rows" };
    if below == 0 {
        format!(
            "end of text · {above} {} above · PgUp/PgDn scroll",
            rows(above)
        )
    } else if above == 0 {
        format!("… {below} more {} · PgUp/PgDn scroll", rows(below))
    } else {
        format!(
            "… {below} more {} · {above} above · PgUp/PgDn scroll",
            rows(below)
        )
    }
}

/// The part of the note that fits `room` columns with the caret in view, and
/// the caret's column in it.
fn note_window(text: &str, cursor: usize, room: usize) -> (String, usize) {
    let before = text[..cursor].graphemes(true).collect::<Vec<_>>();
    let mut start = before.len();
    let mut used = 0;
    while start > 0 {
        let columns = cell_width(before[start - 1]);
        if used + columns > room.saturating_sub(1) {
            break;
        }
        used += columns;
        start -= 1;
    }
    let mut shown = before[start..].concat();
    let mut column = used;
    for grapheme in text[cursor..].graphemes(true) {
        let columns = cell_width(grapheme);
        if column + columns > room {
            break;
        }
        shown.push_str(grapheme);
        column += columns;
    }
    (shown, used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        with(code, KeyModifiers::NONE)
    }

    fn choices() -> Vec<ApprovalChoice> {
        vec![
            ApprovalChoice::new("once", "allow once", &["y"], true),
            ApprovalChoice::new("session", "allow for this session", &["s"], true),
            ApprovalChoice::new("deny", "deny", &["n"], false),
        ]
    }

    /// A picker for a command the host named, with the full prompt behind `?`.
    fn picker(subject: &str) -> ApprovalPicker {
        ApprovalPicker::new(
            &choices(),
            Some(subject),
            "Run command:",
            Some("run_command in ~/demo"),
            &format!("Run command:\n  {subject}\n\nArguments: none"),
            None,
        )
    }

    /// A picker with no named command and no host choices.
    fn plain() -> ApprovalPicker {
        ApprovalPicker::new(&[], None, "Run it?", None, "Run it?", None)
    }

    fn lines(count: usize) -> String {
        (1..=count)
            .map(|row| format!("echo row{row:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw(
        picker: &mut ApprovalPicker,
        width: u16,
        height: u16,
        notice: Option<&str>,
    ) -> (String, Option<(u16, u16)>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut caret = None;
        terminal
            .draw(|frame| caret = picker.render(frame, frame.area(), notice))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        (rows.join("\n"), caret)
    }

    fn press(picker: &mut ApprovalPicker, code: KeyCode) -> Outcome {
        picker.handle_key(key(code), Instant::now())
    }

    fn type_str(picker: &mut ApprovalPicker, text: &str) {
        for c in text.chars() {
            press(picker, KeyCode::Char(c));
        }
    }

    fn answer(outcome: Outcome) -> Answer {
        match outcome {
            Outcome::Answer(answer) => answer,
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    /// Page down, drawing after every key, until the gate opens. Returns the
    /// frames it took.
    fn read_to_the_end(picker: &mut ApprovalPicker, width: u16, height: u16) -> usize {
        for frame in 0..2000 {
            draw(picker, width, height, None);
            if picker.blocked().is_none() {
                return frame;
            }
            press(picker, KeyCode::PageDown);
        }
        panic!("the end of the command was never drawn");
    }

    // — Answering ————————————————————————————————————————————————

    #[test]
    fn with_no_choices_it_offers_allow_and_deny() {
        let mut picker = plain();
        let allow = answer(press(&mut picker, KeyCode::Char('1')));
        assert!(allow.confirmed);
        assert_eq!((allow.id, allow.key.as_str()), (None, "y"));
        assert_eq!(allow.label, "allow");
        let deny = answer(press(&mut picker, KeyCode::Char('n')));
        assert!(!deny.confirmed);
        assert_eq!((deny.id, deny.key.as_str()), (None, "n"));
    }

    #[test]
    fn digits_aliases_and_enter_pick_host_choices() {
        let mut picker = picker("ls");
        read_to_the_end(&mut picker, 80, 24);
        assert_eq!(
            answer(press(&mut picker, KeyCode::Char('2'))).id.as_deref(),
            Some("session")
        );
        // Aliases ignore case.
        assert_eq!(
            answer(press(&mut picker, KeyCode::Char('S'))).id.as_deref(),
            Some("session")
        );
        press(&mut picker, KeyCode::Down);
        let enter = answer(press(&mut picker, KeyCode::Enter));
        assert_eq!(enter.id.as_deref(), Some("session"));
        assert_eq!(enter.key, "s");
        assert_eq!(press(&mut picker, KeyCode::Esc), Outcome::Deny);
    }

    #[test]
    fn arrows_and_vim_keys_move_and_wrap() {
        let mut picker = plain();
        assert_eq!(picker.selected(), 0);
        press(&mut picker, KeyCode::Up);
        assert_eq!(picker.selected(), 1, "up from the first wraps to the last");
        press(&mut picker, KeyCode::Down);
        assert_eq!(picker.selected(), 0);
        type_str(&mut picker, "j");
        assert_eq!(picker.selected(), 1);
        type_str(&mut picker, "k");
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn only_the_first_nine_items_have_digits() {
        let many = (1..=11)
            .map(|n| ApprovalChoice::new(&format!("c{n}"), format!("choice {n}"), &[], false))
            .collect::<Vec<_>>();
        let mut picker = ApprovalPicker::new(&many, None, "Run it?", None, "Run it?", None);
        assert_eq!(
            answer(press(&mut picker, KeyCode::Char('9'))).id.as_deref(),
            Some("c9")
        );
        let (screen, _) = draw(&mut picker, 40, 30, None);
        assert!(screen.contains("9. choice 9"), "{screen}");
        assert!(!screen.contains("10. choice 10"), "{screen}");
        assert!(screen.contains("choice 10"), "{screen}");
        // A key with no alias answers with `yes` / `no` by what it confirms.
        assert_eq!(answer(press(&mut picker, KeyCode::Enter)).key, "no");
    }

    #[test]
    fn a_digit_past_the_list_is_a_stray_key() {
        let t0 = Instant::now();
        let mut picker = plain();
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('9')), t0),
            Outcome::Consumed
        );
        // It counts as a stray key: an answer right after it waits.
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('1')), t0 + Duration::from_millis(50)),
            Outcome::Consumed
        );
        assert!(matches!(
            picker.handle_key(key(KeyCode::Char('1')), t0 + Duration::from_millis(600)),
            Outcome::Answer(_)
        ));
    }

    #[test]
    fn answer_keys_wait_out_the_quiet_window_but_moves_and_esc_do_not() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut picker = ApprovalPicker::new(&[], None, "Run it?", None, "Run it?", Some(t0));
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('y')), at(100)),
            Outcome::Consumed,
            "typed just before the picker opened"
        );
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter), at(300)),
            Outcome::Consumed,
            "the ignored key restarted the window"
        );
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('1')), at(600)),
            Outcome::Consumed
        );
        assert!(matches!(
            picker.handle_key(key(KeyCode::Char('1')), at(1100)),
            Outcome::Answer(_)
        ));

        let mut picker = ApprovalPicker::new(&[], None, "Run it?", None, "Run it?", Some(t0));
        picker.handle_key(key(KeyCode::Down), at(1));
        assert_eq!(picker.selected(), 1, "moving is never ignored");
        picker.handle_key(key(KeyCode::Tab), at(2));
        assert!(picker.note_open(), "Tab is never ignored");
        assert!(
            matches!(
                picker.handle_key(key(KeyCode::Enter), at(3)),
                Outcome::Answer(_)
            ),
            "the user opened the note on purpose"
        );
        let mut picker = ApprovalPicker::new(&[], None, "Run it?", None, "Run it?", Some(t0));
        assert_eq!(picker.handle_key(key(KeyCode::Esc), at(1)), Outcome::Deny);
    }

    #[test]
    fn control_alt_and_unknown_keys_are_not_the_pickers() {
        let mut picker = plain();
        let now = Instant::now();
        for event in [
            with(KeyCode::Char('c'), KeyModifiers::CONTROL),
            with(KeyCode::Char('o'), KeyModifiers::CONTROL),
            with(KeyCode::Char('y'), KeyModifiers::ALT),
            key(KeyCode::F(5)),
            key(KeyCode::Left),
        ] {
            assert_eq!(picker.handle_key(event, now), Outcome::NotMine, "{event:?}");
        }
        // Inside the note, Ctrl-C still belongs to the panel.
        press(&mut picker, KeyCode::Tab);
        assert_eq!(
            picker.handle_key(with(KeyCode::Char('c'), KeyModifiers::CONTROL), now),
            Outcome::NotMine
        );
    }

    // — Notes ————————————————————————————————————————————————————

    #[test]
    fn a_note_rides_on_the_answer_and_a_denial_keeps_it_as_a_reason() {
        let mut picker = picker("ls");
        read_to_the_end(&mut picker, 80, 24);
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "  be careful \u{1b}[2J ");
        let allow = answer(press(&mut picker, KeyCode::Enter));
        assert!(allow.confirmed);
        assert_eq!(allow.note.as_deref(), Some("be careful [2J"));

        let mut picker = self::picker("ls");
        press(&mut picker, KeyCode::Up);
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "too risky");
        let deny = answer(press(&mut picker, KeyCode::Enter));
        assert!(!deny.confirmed);
        assert_eq!(deny.id.as_deref(), Some("deny"));
        assert_eq!(deny.note.as_deref(), Some("too risky"));
    }

    #[test]
    fn an_empty_note_is_none_and_esc_closes_the_note_without_answering() {
        let mut picker = plain();
        press(&mut picker, KeyCode::Tab);
        assert!(picker.note_open());
        assert_eq!(press(&mut picker, KeyCode::Esc), Outcome::Consumed);
        assert!(!picker.note_open(), "Esc closes the note first");
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "   ");
        assert_eq!(answer(press(&mut picker, KeyCode::Enter)).note, None);

        let mut picker = plain();
        press(&mut picker, KeyCode::Tab);
        press(&mut picker, KeyCode::Tab);
        assert!(!picker.note_open(), "Tab closes it too");
        assert_eq!(press(&mut picker, KeyCode::Esc), Outcome::Deny);
    }

    #[test]
    fn digits_and_aliases_are_text_inside_the_note() {
        let mut picker = plain();
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "y1n");
        let answer = answer(press(&mut picker, KeyCode::Enter));
        assert_eq!(answer.note.as_deref(), Some("y1n"));
        assert!(answer.confirmed, "the highlighted item answers");
    }

    #[test]
    fn the_note_is_edited_with_the_usual_keys() {
        let mut picker = plain();
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "hello world");
        picker.handle_key(
            with(KeyCode::Char('w'), KeyModifiers::CONTROL),
            Instant::now(),
        );
        type_str(&mut picker, "there");
        press(&mut picker, KeyCode::Home);
        press(&mut picker, KeyCode::Delete);
        press(&mut picker, KeyCode::End);
        press(&mut picker, KeyCode::Left);
        press(&mut picker, KeyCode::Backspace);
        assert_eq!(
            answer(press(&mut picker, KeyCode::Enter)).note.as_deref(),
            Some("ello thee")
        );
        let mut picker = plain();
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "abc");
        picker.handle_key(
            with(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Instant::now(),
        );
        assert_eq!(answer(press(&mut picker, KeyCode::Enter)).note, None);
    }

    #[test]
    fn a_note_is_bounded_and_pastes_only_land_in_an_open_note() {
        let mut picker = plain();
        picker.paste("dropped");
        press(&mut picker, KeyCode::Tab);
        for _ in 0..2000 {
            press(&mut picker, KeyCode::Char('a'));
        }
        picker.paste("more\nlines\u{1b}[2J");
        let note = answer(press(&mut picker, KeyCode::Enter)).note.unwrap();
        assert_eq!(note.len(), MAX_NOTE_BYTES, "typing stops at the bound");

        let mut picker = plain();
        press(&mut picker, KeyCode::Tab);
        picker.paste("two\nlines\u{1b}[2J");
        assert_eq!(
            answer(press(&mut picker, KeyCode::Enter)).note.as_deref(),
            Some("two lines[2J")
        );
    }

    #[test]
    fn a_note_row_shows_a_hint_when_empty_and_the_caret_when_not() {
        let mut picker = plain();
        press(&mut picker, KeyCode::Tab);
        let (screen, caret) = draw(&mut picker, 60, 8, None);
        assert!(
            screen.contains("note: (optional) type, then Enter"),
            "{screen}"
        );
        let (x, y) = caret.expect("the note has the caret");
        let row = screen.lines().nth(usize::from(y)).unwrap();
        assert!(row.contains("note:"), "{row}");
        assert_eq!(usize::from(x), "│  note: ".width());
        type_str(&mut picker, "hi");
        let (screen, caret) = draw(&mut picker, 60, 8, None);
        assert!(screen.contains("note: hi"), "{screen}");
        assert_eq!(caret.map(|c| c.0), Some(x + 2));
        // On a denial the field is a reason.
        press(&mut picker, KeyCode::Down);
        let (screen, _) = draw(&mut picker, 60, 8, None);
        assert!(screen.contains("reason: hi"), "{screen}");
    }

    // — The command must be seen first ——————————————————————————

    #[test]
    fn a_confirming_answer_waits_for_the_last_row_but_a_denial_never_does() {
        let mut picker = picker(&lines(30));
        draw(&mut picker, 60, 10, None);
        for code in ['y', '1', 's', '2'] {
            assert_eq!(
                press(&mut picker, KeyCode::Char(code)),
                Outcome::Consumed,
                "{code}"
            );
        }
        assert_eq!(press(&mut picker, KeyCode::Enter), Outcome::Consumed);
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "fine");
        assert_eq!(press(&mut picker, KeyCode::Enter), Outcome::Consumed);
        assert!(picker.note_open(), "a refused answer keeps the note");
        press(&mut picker, KeyCode::Esc);

        assert!(!answer(press(&mut picker, KeyCode::Char('n'))).confirmed);
        assert!(!answer(press(&mut picker, KeyCode::Char('3'))).confirmed);
        assert_eq!(press(&mut picker, KeyCode::Esc), Outcome::Deny);
        press(&mut picker, KeyCode::Up);
        assert!(!answer(press(&mut picker, KeyCode::Enter)).confirmed);
    }

    #[test]
    fn a_command_the_host_did_not_name_gates_nothing() {
        let mut picker = ApprovalPicker::new(&[], None, "Run it?", None, "Run it?\nmore", None);
        assert!(answer(press(&mut picker, KeyCode::Char('y'))).confirmed);
    }

    #[test]
    fn a_short_command_is_seen_by_the_first_frame_that_draws_it() {
        let mut picker = picker("ls");
        assert_eq!(
            press(&mut picker, KeyCode::Char('y')),
            Outcome::Consumed,
            "nothing has been drawn yet"
        );
        draw(&mut picker, 60, 10, None);
        assert!(answer(press(&mut picker, KeyCode::Char('y'))).confirmed);
    }

    #[test]
    fn the_gate_opens_on_the_frame_that_draws_the_last_row() {
        // Wrapped to 36 columns the command is 42 rows; scroll one row at a
        // time and check the gate against what the frame drew.
        for height in [5, 6, 8, 12] {
            let subject = lines(42);
            let mut picker = picker(&subject);
            let mut refused = 0;
            loop {
                let (screen, _) = draw(&mut picker, 40, height, None);
                let shown = screen.contains("row42");
                assert_eq!(picker.blocked().is_none(), shown, "{height}: {screen}");
                if shown {
                    break;
                }
                refused += 1;
                assert_eq!(
                    press(&mut picker, KeyCode::Char('y')),
                    Outcome::Consumed,
                    "{height}: refused with row42 off screen"
                );
                assert_eq!(
                    picker.handle_key(with(KeyCode::Down, KeyModifiers::SHIFT), Instant::now()),
                    Outcome::Consumed
                );
            }
            assert!(refused >= 5, "{height}: {refused}");
            assert!(answer(press(&mut picker, KeyCode::Char('y'))).confirmed);
        }
    }

    #[test]
    fn a_scroll_moves_at_most_a_page_past_the_last_frame() {
        let mut picker = picker(&lines(200));
        draw(&mut picker, 60, 12, None);
        for _ in 0..500 {
            press(&mut picker, KeyCode::PageDown);
            picker.handle_key(with(KeyCode::Down, KeyModifiers::SHIFT), Instant::now());
        }
        assert!(
            picker.scroll() <= picker.shown_scroll + picker.viewport,
            "{} past {} + {}",
            picker.scroll(),
            picker.shown_scroll,
            picker.viewport
        );
        // Every row is drawn on the way down.
        let mut seen = [false; 200];
        let mut frame = |picker: &mut ApprovalPicker| {
            let (screen, _) = draw(picker, 60, 12, None);
            for n in 1..=200 {
                if screen.contains(&format!("row{n:02} "))
                    || screen.contains(&format!("row{n:02}│"))
                {
                    seen[n - 1] = true;
                }
            }
        };
        let mut picker = self::picker(&lines(200));
        for _ in 0..200 {
            frame(&mut picker);
            for _ in 0..50 {
                press(&mut picker, KeyCode::PageDown);
            }
        }
        assert!(seen.iter().all(|seen| *seen), "a row was never drawn");
    }

    #[test]
    fn a_new_width_starts_the_read_again_from_the_top() {
        let mut picker = picker(&lines(60));
        for _ in 0..3 {
            draw(&mut picker, 40, 10, None);
            press(&mut picker, KeyCode::PageDown);
        }
        draw(&mut picker, 40, 10, None);
        assert!(picker.scroll() > 0);
        draw(&mut picker, 41, 10, None);
        assert_eq!(picker.scroll(), 0);
        assert!(!picker.seen_end || picker.blocked().is_none());
        assert_eq!(press(&mut picker, KeyCode::Char('y')), Outcome::Consumed);
    }

    #[test]
    fn a_cluster_wider_than_the_box_refuses_until_it_is_wide_enough() {
        let mut picker = picker("漢");
        let (screen, _) = draw(&mut picker, 3, 10, None);
        assert!(screen.contains('\u{fffd}'), "{screen}");
        assert_eq!(picker.blocked(), Some(Gate::Narrow));
        assert_eq!(press(&mut picker, KeyCode::Char('y')), Outcome::Consumed);
        let (screen, _) = draw(&mut picker, 50, 10, None);
        assert!(!screen.contains("too narrow"), "{screen}");
        assert!(!screen.contains('\u{fffd}'), "{screen}");
        assert_eq!(picker.blocked(), None);
        assert!(answer(press(&mut picker, KeyCode::Char('y'))).confirmed);
    }

    #[test]
    fn a_stand_in_is_never_counted_as_seen_even_if_it_was_drawn_before() {
        // Seen at 80 columns, then a narrow frame draws a stand-in: the
        // narrow frame refuses until it is wide again.
        let marks = format!("rm -- 'x{}'", "\u{ff9e}".repeat(60));
        let mut picker = picker(&marks);
        draw(&mut picker, 200, 10, None);
        assert_eq!(picker.blocked(), None);
        draw(&mut picker, 40, 10, None);
        assert_eq!(picker.blocked(), Some(Gate::Narrow));
    }

    #[test]
    fn a_box_with_no_room_for_the_command_refuses_and_says_so() {
        let mut picker = picker(&lines(5));
        let (screen, _) = draw(&mut picker, 80, 3, None);
        assert!(screen.contains("1. allow"), "{screen}");
        assert_eq!(picker.blocked(), Some(Gate::Cramped));
        assert_eq!(press(&mut picker, KeyCode::Char('y')), Outcome::Consumed);
        let (screen, _) = draw(&mut picker, 80, 3, None);
        assert!(screen.contains("too small to show the command"), "{screen}");
        // Scrolling cannot help, and is not the picker's key.
        assert_eq!(press(&mut picker, KeyCode::PageDown), Outcome::NotMine);
        // Deny works, and a bigger box lets it be read.
        assert!(!answer(press(&mut picker, KeyCode::Char('n'))).confirmed);
        read_to_the_end(&mut picker, 80, 12);
        assert!(answer(press(&mut picker, KeyCode::Char('y'))).confirmed);
    }

    #[test]
    fn a_command_over_64_kib_can_only_be_denied() {
        let subject = format!("{}printf TAIL", "x".repeat(MAX_TEXT_BYTES + 5));
        let mut picker = picker(&subject);
        assert_eq!(picker.omitted, 16);
        // Read every row that is kept, down to the note about the rest.
        let mut screen = String::new();
        for _ in 0..4000 {
            screen = draw(&mut picker, 80, 12, None).0;
            if screen.contains("16 more bytes not shown") {
                break;
            }
            press(&mut picker, KeyCode::PageDown);
        }
        assert!(screen.contains("16 more bytes not shown"), "{screen}");
        // Even then it can only be denied.
        assert_eq!(picker.blocked(), Some(Gate::TooLong));
        assert_eq!(press(&mut picker, KeyCode::Char('y')), Outcome::Consumed);
        assert_eq!(press(&mut picker, KeyCode::Enter), Outcome::Consumed);
        let (screen, _) = draw(&mut picker, 80, 12, None);
        assert!(screen.contains("only be denied"), "{screen}");
        assert!(!answer(press(&mut picker, KeyCode::Char('n'))).confirmed);
    }

    #[test]
    fn a_refusal_goes_with_the_next_key_and_when_it_no_longer_applies() {
        let mut picker = picker(&lines(30));
        draw(&mut picker, 60, 10, None);
        press(&mut picker, KeyCode::Char('y'));
        let (screen, _) = draw(&mut picker, 60, 10, None);
        assert!(screen.contains("scroll to the end"), "{screen}");
        let (screen, _) = draw(&mut picker, 60, 10, None);
        assert!(
            screen.contains("scroll to the end"),
            "{screen}: it stays between keys"
        );
        press(&mut picker, KeyCode::Down);
        let (screen, _) = draw(&mut picker, 60, 10, None);
        assert!(!screen.contains("scroll to the end"), "{screen}");

        // And once the end has been drawn the advice is stale.
        press(&mut picker, KeyCode::Char('y'));
        assert!(picker.refused);
        picker.seen_end = true;
        draw(&mut picker, 60, 10, None);
        assert!(!picker.refused);
    }

    #[test]
    fn a_refusal_names_the_scroll_keys_and_the_way_to_deny() {
        for gate in [Gate::TooLong, Gate::Narrow, Gate::Cramped, Gate::Unseen] {
            let [full, short] = gate.texts();
            for text in [full, short] {
                assert!(text.contains("Esc"), "{text}");
            }
            assert!(gate.fit(200) == full);
            assert!(gate.fit(short.chars().count()) == short);
            assert!(gate.fit(10).width() <= 10);
        }
        assert!(Gate::Unseen.texts()[0].contains("PgDn"));
        assert!(Gate::Unseen.texts()[1].contains("PgDn"));
    }

    // — Layout —————————————————————————————————————————————————

    #[test]
    fn rows_are_given_out_in_order() {
        let mut picker = picker(&lines(30));
        let plan = |picker: &mut ApprovalPicker, max, notice| picker.plan(40, max, notice);
        // Three rows: borders and one answer. Nothing of the command.
        let p = plan(&mut picker, 3, false);
        assert_eq!((p.bar, p.list, p.content, p.marker), (false, 1, 0, false));
        // Four: a row of the command, and no room for its marker.
        let p = plan(&mut picker, 4, false);
        assert_eq!((p.list, p.content, p.marker), (1, 1, false));
        // Five: the marker.
        let p = plan(&mut picker, 5, false);
        assert_eq!((p.list, p.content, p.marker), (1, 1, true));
        // Six: another answer.
        let p = plan(&mut picker, 6, false);
        assert_eq!((p.list, p.content, p.marker), (2, 1, true));
        // A notice comes before the rest of the answers.
        let p = plan(&mut picker, 6, true);
        assert_eq!((p.list, p.notice_row, p.content), (1, true, 1));
        let p = plan(&mut picker, 5, true);
        assert_eq!((p.notice_row, p.content), (false, 1));
        // The note comes before everything.
        press(&mut picker, KeyCode::Tab);
        let p = plan(&mut picker, 4, false);
        assert_eq!((p.note, p.list, p.content), (true, 1, 0));
        let p = plan(&mut picker, 5, false);
        assert_eq!((p.note, p.list, p.content), (true, 1, 1));
        // Everything else goes to the command.
        let p = plan(&mut picker, 20, false);
        assert_eq!((p.list, p.content, p.marker), (3, 13, true));
        // Never more than was given.
        for max in 1..=30 {
            assert!(plan(&mut picker, max, true).height <= usize::from(max));
        }
    }

    #[test]
    fn a_command_that_fits_has_no_marker_and_a_separator() {
        let mut picker = picker(&lines(3));
        let p = picker.plan(40, 30, false);
        assert_eq!((p.content, p.marker, p.separator), (3, false, true));
        assert_eq!(p.height, 2 + 3 + 1 + 3);
        let (screen, _) = draw(&mut picker, 40, 30, None);
        assert!(screen.contains("├"), "{screen}");
        assert!(screen.contains("echo row03"), "{screen}");
        assert!(!screen.contains("more row"), "{screen}");
        // Exactly full: no separator.
        let p = picker.plan(40, 2 + 3 + 3, false);
        assert_eq!((p.content, p.marker, p.separator), (3, false, false));
    }

    #[test]
    fn a_cut_command_counts_wrapped_rows_in_its_marker() {
        // One 200-character line wraps to 6 rows at 38 columns.
        let mut picker = picker(&"x".repeat(200));
        let (screen, _) = draw(&mut picker, 40, 6, None);
        assert!(screen.contains("… 5 more rows"), "{screen}");
        press(&mut picker, KeyCode::PageDown);
        let (screen, _) = draw(&mut picker, 40, 6, None);
        assert!(screen.contains("above"), "{screen}");
        for _ in 0..10 {
            press(&mut picker, KeyCode::PageDown);
            draw(&mut picker, 40, 6, None);
        }
        let (screen, _) = draw(&mut picker, 40, 6, None);
        assert!(screen.contains("end of text"), "{screen}");
    }

    #[test]
    fn under_three_rows_it_is_a_bar_with_no_part_of_the_command() {
        let mut picker = picker("rm -rf build");
        let (screen, _) = draw(&mut picker, 60, 1, None);
        assert!(screen.contains("❯ 1. allow once"), "{screen}");
        assert!(!screen.contains("rm -rf"), "{screen}");
        assert!(!screen.contains('┌'), "{screen}");
        // A refusal takes the one line.
        assert_eq!(press(&mut picker, KeyCode::Char('y')), Outcome::Consumed);
        let (screen, _) = draw(&mut picker, 60, 1, None);
        assert!(screen.contains("too small"), "{screen}");
        // With two rows the item stays and the refusal goes below it.
        let (screen, _) = draw(&mut picker, 60, 2, None);
        let mut rows = screen.lines();
        assert!(rows.next().unwrap().contains("❯ 1. allow once"), "{screen}");
        assert!(rows.next().unwrap().contains("too small"), "{screen}");
        // Then the key hint, or a notice from the panel.
        press(&mut picker, KeyCode::Down);
        let (screen, _) = draw(&mut picker, 60, 2, None);
        assert!(
            screen.lines().nth(1).unwrap().contains("Esc deny"),
            "{screen}"
        );
        let (screen, _) = draw(&mut picker, 60, 2, Some("queued"));
        assert!(
            screen.lines().nth(1).unwrap().contains("queued"),
            "{screen}"
        );
        // And the note field takes the item's line, with the caret in it.
        press(&mut picker, KeyCode::Tab);
        type_str(&mut picker, "hi");
        let (screen, caret) = draw(&mut picker, 60, 1, None);
        assert!(screen.contains("note: hi"), "{screen}");
        assert_eq!(
            caret,
            Some((u16::try_from("  note: hi".width()).unwrap(), 0))
        );
        // The note, not the item, shows when no refusal is up.
        assert!(!screen.contains("❯"), "{screen}");
    }

    #[test]
    fn the_box_is_drawn_at_the_bottom_of_the_area_and_never_larger() {
        let mut picker = picker(&lines(3));
        let height = picker.height(40, 30, false);
        assert_eq!(height, 9);
        let (screen, _) = draw(&mut picker, 40, 12, None);
        let rows = screen.lines().collect::<Vec<_>>();
        assert!(rows[3].starts_with('┌'), "{screen}");
        assert!(rows[11].starts_with('└'), "{screen}");
        assert!(
            rows[..3].iter().all(|row| row.trim().is_empty()),
            "{screen}"
        );
    }

    #[test]
    fn the_highlighted_answer_is_on_screen_at_every_size() {
        let long = ApprovalChoice::new("c", "x".repeat(300), &[], true);
        let mut many = choices();
        many.extend(
            (0..6).map(|n| ApprovalChoice::new(&format!("m{n}"), format!("more {n}"), &[], false)),
        );
        many.insert(1, long);
        for width in [20, 30, 40, 80] {
            for height in 3..=14 {
                let mut picker = ApprovalPicker::new(
                    &many,
                    Some(&lines(10)),
                    "Run command:",
                    None,
                    "Run command:\nmore",
                    None,
                );
                for _ in 0..many.len() {
                    let (screen, _) = draw(&mut picker, width, height, None);
                    let n = picker.selected() + 1;
                    let lead = format!("❯ {n}. ");
                    let lead = if n > 9 { "❯    ".to_string() } else { lead };
                    assert!(
                        screen.contains(&lead),
                        "{width}×{height}, item {n}: not on screen:\n{screen}"
                    );
                    press(&mut picker, KeyCode::Down);
                }
            }
        }
    }

    #[test]
    fn a_long_label_wraps_under_its_number_and_a_long_list_windows_to_the_selection() {
        let mut picker = ApprovalPicker::new(
            &[
                ApprovalChoice::new("a", "one two three four five six seven eight", &[], true),
                ApprovalChoice::new("b", "short", &[], false),
            ],
            None,
            "Run it?",
            None,
            "Run it?",
            None,
        );
        let (screen, _) = draw(&mut picker, 24, 8, None);
        assert!(screen.contains("❯ 1. one two three fou"), "{screen}");
        assert!(screen.contains("│     r five six seven"), "{screen}");
        assert!(screen.contains("  2. short"), "{screen}");
    }

    #[test]
    fn the_title_carries_the_context_and_is_clipped() {
        let mut picker = picker("ls");
        let (screen, _) = draw(&mut picker, 60, 8, None);
        let top = |screen: &str| {
            screen
                .lines()
                .find(|row| row.starts_with('┌'))
                .unwrap_or_default()
                .to_string()
        };
        assert!(
            top(&screen).contains("approve? · run_command in ~/demo"),
            "{screen}"
        );
        let (screen, _) = draw(&mut picker, 20, 8, None);
        let title = top(&screen);
        assert!(title.width() <= 20 && title.contains("approve?"), "{title}");
    }

    #[test]
    fn a_notice_has_a_row_when_there_is_room_and_the_border_when_there_is_not() {
        let mut picker = picker(&lines(30));
        let (screen, _) = draw(&mut picker, 80, 12, Some("queued for the next turn"));
        let rows = screen.lines().collect::<Vec<_>>();
        assert!(rows[10].contains("queued for the next turn"), "{screen}");
        assert!(rows[11].contains("Esc deny"), "the hint stays: {screen}");
        let (screen, _) = draw(&mut picker, 80, 4, Some("queued for the next turn"));
        let rows = screen.lines().collect::<Vec<_>>();
        assert!(rows[3].contains("queued for the next turn"), "{screen}");
        assert!(rows[3].starts_with('└'), "{screen}");
    }

    #[test]
    fn the_separator_joins_the_side_borders() {
        let mut picker = picker(&lines(2));
        let (screen, _) = draw(&mut picker, 30, 20, None);
        let row = screen
            .lines()
            .find(|row| row.contains('─') && row.contains('├'))
            .unwrap();
        assert!(
            row.starts_with('├') && row.trim_end().ends_with('┤'),
            "{row}"
        );
    }

    #[test]
    fn no_size_panics() {
        for with_note in [false, true] {
            for with_details in [false, true] {
                for refused in [false, true] {
                    let mut picker = picker(&format!("{}\n{}", lines(30), "漢".repeat(40)));
                    if with_details {
                        press(&mut picker, KeyCode::Char('?'));
                    }
                    if with_note {
                        press(&mut picker, KeyCode::Tab);
                        type_str(&mut picker, "a long long long note");
                    }
                    if refused {
                        press(&mut picker, KeyCode::Enter);
                    }
                    for width in (0..=24).chain([40, 80, 200]) {
                        for height in 0..=20 {
                            draw(&mut picker, width, height, Some("queued"));
                            picker.height(width, height, true);
                        }
                    }
                }
            }
        }
    }

    // — Details and host text ——————————————————————————————————

    #[test]
    fn question_mark_adds_the_prompt_after_the_command_in_one_scroll() {
        let mut picker = picker(&lines(3));
        let (screen, _) = draw(&mut picker, 80, 30, None);
        assert!(!screen.contains("Arguments:"), "{screen}");
        assert!(screen.contains("? details"), "{screen}");
        assert_eq!(press(&mut picker, KeyCode::Char('?')), Outcome::Consumed);
        let (screen, _) = draw(&mut picker, 80, 30, None);
        assert!(screen.contains("Arguments:"), "{screen}");
        assert!(screen.contains("? hide"), "{screen}");
        let rows = screen.lines().collect::<Vec<_>>();
        let last_command_row = rows
            .iter()
            .position(|row| row.contains("echo row03"))
            .unwrap();
        let first_detail = rows
            .iter()
            .position(|row| row.contains("Run command:"))
            .unwrap();
        assert!(last_command_row < first_detail, "{screen}");
        // The command's rows are the same, so what was seen stays seen.
        assert_eq!(picker.body.as_ref().unwrap().subject_end, 3);
        press(&mut picker, KeyCode::Char('?'));
        let (screen, _) = draw(&mut picker, 80, 30, None);
        assert!(!screen.contains("Arguments:"), "{screen}");
    }

    #[test]
    fn question_mark_does_nothing_without_details() {
        let mut picker = plain();
        let (screen, _) = draw(&mut picker, 60, 10, None);
        assert!(!screen.contains("? details"), "{screen}");
        assert_eq!(press(&mut picker, KeyCode::Char('?')), Outcome::Consumed);
        assert!(!picker.show_details);
    }

    #[test]
    fn details_are_not_capped_before_64_kib_and_say_so_after() {
        let long = (1..=300)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut picker = ApprovalPicker::new(
            &[],
            Some("ls"),
            "Run:",
            None,
            &format!("Run:\nls\n{long}"),
            None,
        );
        press(&mut picker, KeyCode::Char('?'));
        draw(&mut picker, 60, 30, None);
        let body = picker.body.as_ref().unwrap();
        assert!(body.rows.len() > 300, "{}", body.rows.len());

        let huge = "y".repeat(MAX_TEXT_BYTES + 100);
        let mut picker = ApprovalPicker::new(&[], Some("ls"), "Run:", None, &huge, None);
        press(&mut picker, KeyCode::Char('?'));
        draw(&mut picker, 60, 30, None);
        let text = picker.details.as_ref().unwrap();
        assert!(
            text.contains("100 more bytes not shown"),
            "{}",
            &text[text.len() - 90..]
        );
    }

    #[test]
    fn host_text_is_bounded_once_and_cut_text_says_so() {
        let huge = "é".repeat(5000);
        let many = vec![
            ApprovalChoice::new("a", huge.clone(), &["a"], true),
            ApprovalChoice::new("b", "deny", &["n"], false),
        ];
        let picker = ApprovalPicker::new(&many, Some("ls"), &huge, Some(&huge), "ls", None);
        let bound = 2 * 1024 + '…'.len_utf8();
        assert!(picker.items[0].text.len() <= bound);
        assert!(picker.items[0].text.ends_with('…'));
        assert_eq!(picker.items[1].text, "deny");
        assert!(picker.context.as_ref().unwrap().len() <= bound);
        let summary = ApprovalPicker::new(&[], None, &huge, None, &huge, None);
        assert!(summary.text.as_ref().unwrap().len() <= bound);
        assert!(summary.text.as_ref().unwrap().ends_with('…'));
        // The answer carries the bounded label, never the huge one.
        let mut picker = picker;
        read_to_the_end(&mut picker, 80, 20);
        assert!(answer(press(&mut picker, KeyCode::Char('1'))).label.len() <= bound);
    }

    #[test]
    fn controls_and_zero_width_characters_never_reach_the_screen() {
        let many = vec![
            ApprovalChoice::new("a", "allow \u{1b}[2Konce\r\u{200b}", &["y"], true),
            ApprovalChoice::new("b", "deny\u{202e}", &["n"], false),
        ];
        let mut picker = ApprovalPicker::new(
            &many,
            Some("echo ok\u{1b}[1A\r\u{feff}rm\u{2066} -rf"),
            "x",
            Some("ctx\u{7}"),
            "details\u{1b}[2J\u{2028}here",
            None,
        );
        press(&mut picker, KeyCode::Char('?'));
        let (screen, _) = draw(&mut picker, 60, 14, None);
        for control in [
            '\u{1b}', '\r', '\u{7}', '\u{200b}', '\u{feff}', '\u{202e}', '\u{2066}', '\u{2028}',
        ] {
            assert!(!screen.contains(control), "{control:?}:\n{screen}");
        }
        assert!(screen.contains("echo ok[1Arm -rf"), "{screen}");
        assert!(screen.contains("1. allow [2Konce"), "{screen}");
    }

    // — Work per frame ———————————————————————————————————————————

    #[test]
    fn the_body_and_the_labels_are_wrapped_once_per_width() {
        let mut picker = picker(&lines(60));
        draw(&mut picker, 60, 20, None);
        let body = picker.body.as_ref().unwrap().rows.as_ptr();
        let label = picker.items[1].rows.as_ptr();
        draw(&mut picker, 60, 20, None);
        press(&mut picker, KeyCode::PageDown);
        draw(&mut picker, 60, 20, None);
        assert_eq!(picker.body.as_ref().unwrap().rows.as_ptr(), body);
        assert_eq!(picker.items[1].rows.as_ptr(), label);
        draw(&mut picker, 50, 20, None);
        assert_eq!(picker.body.as_ref().unwrap().width, 48);
        assert_eq!(picker.items[1].width, 43);
    }

    // — Pieces ——————————————————————————————————————————————————

    #[test]
    fn hints_drop_whole_items_and_keep_deny() {
        let mut picker = picker("ls");
        let full = picker.hint(200, true);
        for part in [
            "↑↓ move",
            "1-3 pick",
            "Enter confirm",
            "Tab add note",
            "PgUp/PgDn scroll",
            "? details",
            "Esc deny",
        ] {
            assert!(full.contains(part), "{part}: {full}");
        }
        assert!(!picker.hint(200, false).contains("PgUp"));
        for width in 12..80 {
            let hint = picker.hint(width, true);
            assert!(hint.width() <= width, "{width}: {hint:?}");
            assert!(hint.contains("Esc deny"), "{width}: {hint:?}");
            assert!(
                !hint.trim().ends_with('·') && !hint.contains("…"),
                "{hint:?}"
            );
        }
        press(&mut picker, KeyCode::Tab);
        let hint = picker.hint(200, true);
        assert!(
            hint.contains("Enter send") && hint.contains("Esc cancel note"),
            "{hint}"
        );
        assert!(!hint.contains("pick"), "{hint}");
    }

    #[test]
    fn clip_cuts_by_columns_and_ends_with_an_ellipsis() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("漢字漢", 4), "漢…");
        assert_eq!(clip("ab", 1), "…");
        assert_eq!(clip("ab", 0), "");
        assert_eq!(clip("", 3), "");
    }

    #[test]
    fn the_marker_counts_rows_and_says_which_way() {
        assert_eq!(marker_text(3, 0), "… 3 more rows · PgUp/PgDn scroll");
        assert_eq!(marker_text(1, 0), "… 1 more row · PgUp/PgDn scroll");
        assert_eq!(
            marker_text(2, 1),
            "… 2 more rows · 1 above · PgUp/PgDn scroll"
        );
        assert_eq!(
            marker_text(0, 5),
            "end of text · 5 rows above · PgUp/PgDn scroll"
        );
    }

    #[test]
    fn the_note_window_keeps_the_caret_in_view() {
        assert_eq!(note_window("hello", 5, 4), ("llo".to_string(), 3));
        assert_eq!(
            note_window("hello world", 5, 8),
            ("hello wo".to_string(), 5)
        );
        assert_eq!(note_window("", 0, 5), (String::new(), 0));
        let (shown, caret) = note_window("漢字漢字漢", 9, 5);
        assert!(shown.width() <= 5 && caret <= 4, "{shown:?} {caret}");
    }
}
