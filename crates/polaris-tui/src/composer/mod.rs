//! Grapheme- and column-aware text buffer backing the shell's input area.
//!
//! The composer exists because a terminal caret is positioned in *display
//! columns*, while a Rust `String` is indexed in bytes and iterated in `char`s,
//! and none of the three agree once the text leaves ASCII:
//!
//! - `"世界"` is 2 chars, 6 bytes, and **4 columns**.
//! - `"é"` written as `e` + U+0301 is 2 chars, 3 bytes, and **1 column** — and
//!   `Backspace` must delete both chars, not orphan the accent.
//! - `"👨‍👩‍👧"` is 5 chars joined by zero-width joiners, and renders as **one**
//!   two-column glyph.
//!
//! So the buffer tracks the caret as a **byte offset held on a grapheme
//! boundary**, moves it by grapheme cluster, and measures with
//! [`unicode_width`]. Callers never do arithmetic on the offset themselves;
//! every mutation and motion goes through a method here.
//!
//! # Layout
//!
//! [`Composer::rows`] hard-wraps at grapheme boundaries into display rows, and
//! [`Composer::caret`] reports the caret as a `(row, column)` pair against that
//! same wrap. Wrapping is deliberately *not* word-aware: this buffer holds
//! prompts and pasted code, where a stable column for every byte matters more
//! than a pretty right margin.
//!
//! # Tabs
//!
//! Tabs are kept literally in the buffer and advance to the next multiple of
//! [`TAB_WIDTH`], so pasted code lines up. [`Composer::render_row`] expands them
//! to spaces for display, which keeps the rendered string and the measured
//! column in agreement.

pub(crate) mod editor;
#[cfg(feature = "panels")]
pub(crate) mod history;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Columns a tab advances to the next multiple of.
pub const TAB_WIDTH: usize = 4;

/// Zero-width joiner. A grapheme containing one is an emoji sequence that the
/// terminal draws as a single glyph, whatever its parts measure individually.
const ZWJ: char = '\u{200D}';

/// Display columns a single grapheme cluster occupies.
///
/// Clamped to 2 because a cluster is one user-perceived character and no
/// terminal draws one in more than two cells — whereas summing the widths of
/// an emoji sequence's components (as [`unicode_width`] does) reports 6 for a
/// three-person family emoji and 4 for a flag. Clamping is what keeps the caret
/// under the glyph the user sees.
fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.contains(ZWJ) {
        return 2;
    }
    UnicodeWidthStr::width(grapheme).min(2)
}

/// Column reached after laying out `grapheme` starting at `col`.
fn advance(col: usize, grapheme: &str) -> usize {
    if grapheme == "\t" {
        return col + (TAB_WIDTH - col % TAB_WIDTH);
    }
    col + grapheme_width(grapheme)
}

/// One display row of the wrapped buffer: a byte range into
/// [`Composer::text`], excluding any newline that terminated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// Byte offset where the row starts.
    pub start: usize,
    /// Byte offset one past the row's last byte.
    pub end: usize,
}

/// A multi-line text buffer measured in display columns.
///
/// See the [module docs](self) for why the caret is a byte offset rather than
/// a char index.
#[derive(Debug, Default, Clone)]
pub struct Composer {
    text: String,
    /// Byte offset of the caret. Invariant: always on a grapheme boundary of
    /// `text`, and always `<= text.len()`.
    cursor: usize,
}

impl Composer {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The buffer's contents.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the buffer holds no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Caret position as a byte offset into [`text`](Self::text).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the buffer spans more than one logical line.
    #[must_use]
    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    /// Empty the buffer and park the caret at the start.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    // — Mutation ————————————————————————————————————————————————

    /// Insert one typed character at the caret.
    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Insert a hard line break at the caret.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// If the caret sits just after an unescaped `\\`, turn that backslash into
    /// a newline and report `true`.
    ///
    /// This is the fallback for terminals that cannot report `Shift+Enter`
    /// separately from `Enter` — Terminal.app and older iTerm2 among them,
    /// where *no* modifier+Enter combination is distinguishable. Without it,
    /// those users would have no way to enter a second line at all.
    ///
    /// Trailing backslashes are counted, so an odd run continues the line and
    /// an even run is a literal escaped backslash that submits normally. A
    /// pasted Windows path ending `C:\\dir\\\\` therefore does not silently
    /// become a line continuation.
    pub fn take_line_continuation(&mut self) -> bool {
        let trailing = self.text[..self.cursor]
            .chars()
            .rev()
            .take_while(|&ch| ch == '\\')
            .count();
        if trailing % 2 == 0 {
            return false;
        }
        let start = self.cursor - 1;
        self.text.replace_range(start..self.cursor, "\n");
        self.cursor = start + 1;
        true
    }

    /// Insert pasted text at the caret, literally.
    ///
    /// "Literally" with two sanitisations that are not negotiable in a
    /// terminal:
    ///
    /// - **Line endings are normalised** to `\n`, so a CRLF paste from a
    ///   Windows editor does not leave stray `\r`s that would make the caret
    ///   jump to column 0 mid-row.
    /// - **C0 control characters are dropped**, `\n` and `\t` excepted. A paste
    ///   is attacker-influenced text (it can come from a web page), and an
    ///   embedded `ESC` would otherwise be handed to the terminal as a control
    ///   sequence when the row is drawn.
    ///
    /// Newlines are inserted as newlines and never submit — submission is a
    /// key binding, and this method is not one.
    pub fn insert_paste(&mut self, pasted: &str) {
        let mut cleaned = String::with_capacity(pasted.len());
        let mut chars = pasted.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    // CRLF collapses to one newline; a lone CR is still a line
                    // break in older Mac-era text.
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    cleaned.push('\n');
                }
                '\n' | '\t' => cleaned.push(ch),
                c if c.is_control() => {}
                c => cleaned.push(c),
            }
        }
        self.text.insert_str(self.cursor, &cleaned);
        self.cursor += cleaned.len();
    }

    /// Delete the grapheme before the caret. A no-op at the start.
    ///
    /// Deletes a whole cluster, so backspacing over `e` + U+0301 removes the
    /// accent and its base together rather than leaving a floating mark.
    pub fn backspace(&mut self) {
        let start = self.prev_boundary(self.cursor);
        if start < self.cursor {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    /// Delete the grapheme at the caret. A no-op at the end.
    pub fn delete_forward(&mut self) {
        let end = self.next_boundary(self.cursor);
        if end > self.cursor {
            self.text.replace_range(self.cursor..end, "");
        }
    }

    /// Delete the byte range `[from, to)` (either order) and leave the caret at
    /// the lower bound. Both offsets are snapped outward to grapheme
    /// boundaries, so a range computed from a motion can never split a cluster.
    pub fn delete_range(&mut self, from: usize, to: usize) {
        let (lo, hi) = (from.min(to), from.max(to));
        let lo = self.floor_boundary(lo);
        let hi = self.ceil_boundary(hi);
        if lo >= hi {
            return;
        }
        self.text.replace_range(lo..hi, "");
        self.cursor = lo;
    }

    // — Motion ——————————————————————————————————————————————————

    /// Park the caret at `at`, snapped down to a grapheme boundary and
    /// clamped to the buffer. The escape hatch for motions computed as byte
    /// offsets, such as word jumps.
    pub fn seek(&mut self, at: usize) {
        self.cursor = self.floor_boundary(at);
    }

    /// Move the caret one grapheme left.
    pub fn move_left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    /// Move the caret one grapheme right.
    pub fn move_right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    /// Move the caret to the start of its logical line.
    pub fn move_line_start(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    /// Move the caret to the end of its logical line.
    pub fn move_line_end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    /// Byte offset of the start of the logical line containing `at`.
    #[must_use]
    pub fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |nl| nl + 1)
    }

    /// Byte offset of the end of the logical line containing `at`.
    #[must_use]
    pub fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |nl| at + nl)
    }

    /// Byte offset of the previous word boundary, scanning left from the caret:
    /// skip whitespace, then skip the word. Newlines count as whitespace, so
    /// word motion crosses lines the way it does in a shell.
    #[must_use]
    pub fn prev_word(&self) -> usize {
        let mut at = self.cursor;
        while at > 0 {
            let prev = self.prev_boundary(at);
            if !self.text[prev..at].chars().all(char::is_whitespace) {
                break;
            }
            at = prev;
        }
        while at > 0 {
            let prev = self.prev_boundary(at);
            if self.text[prev..at].chars().all(char::is_whitespace) {
                break;
            }
            at = prev;
        }
        at
    }

    /// Byte offset of the next word boundary, scanning right from the caret.
    #[must_use]
    pub fn next_word(&self) -> usize {
        let len = self.text.len();
        let mut at = self.cursor;
        while at < len {
            let next = self.next_boundary(at);
            if !self.text[at..next].chars().all(char::is_whitespace) {
                break;
            }
            at = next;
        }
        while at < len {
            let next = self.next_boundary(at);
            if self.text[at..next].chars().all(char::is_whitespace) {
                break;
            }
            at = next;
        }
        at
    }

    /// Move the caret one display row up, holding its column where the target
    /// row is long enough. Returns `false` when already on the first row, so
    /// the caller can let the key mean something else (walking prompt history,
    /// say) instead of swallowing it.
    pub fn move_row_up(&mut self, width: usize) -> bool {
        let rows = self.rows(width);
        let (row, col) = self.caret_in(&rows, width);
        if row == 0 {
            return false;
        }
        self.cursor = self.offset_at(&rows[row - 1], col);
        true
    }

    /// Move the caret one display row down. Returns `false` on the last row.
    pub fn move_row_down(&mut self, width: usize) -> bool {
        let rows = self.rows(width);
        let (row, col) = self.caret_in(&rows, width);
        if row + 1 >= rows.len() {
            return false;
        }
        self.cursor = self.offset_at(&rows[row + 1], col);
        true
    }

    // — Layout ——————————————————————————————————————————————————

    /// Hard-wrap the buffer into display rows at `width` columns.
    ///
    /// Always returns at least one row. A logical line whose width is an exact
    /// multiple of `width` is followed by an empty row, so a caret sitting past
    /// the last glyph has somewhere to be.
    #[must_use]
    pub fn rows(&self, width: usize) -> Vec<Row> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for line in self.logical_lines() {
            let mut start = line.start;
            let mut col = 0;
            for (offset, grapheme) in self.text[line.start..line.end].grapheme_indices(true) {
                let at = line.start + offset;
                let next = advance(col, grapheme);
                if next > width && at > start {
                    rows.push(Row { start, end: at });
                    start = at;
                    col = advance(0, grapheme);
                } else {
                    col = next;
                }
            }
            rows.push(Row {
                start,
                end: line.end,
            });
            // A row filled to exactly `width` leaves the caret nowhere to sit:
            // the next grapheme typed will land on a fresh row, so that row has
            // to exist for the caret to be drawn on it.
            if col == width && line.end > start {
                rows.push(Row {
                    start: line.end,
                    end: line.end,
                });
            }
        }
        rows
    }

    /// Caret position as `(row index, display column)` against [`rows`](Self::rows).
    #[must_use]
    pub fn caret(&self, width: usize) -> (usize, usize) {
        let rows = self.rows(width);
        self.caret_in(&rows, width)
    }

    /// The row's text with tabs expanded to spaces, ready to draw.
    ///
    /// Expansion happens here rather than in the buffer so the text stays
    /// literal, and uses the same [`advance`] rule the caret is measured with —
    /// which is what keeps the drawn glyphs and the reported column aligned.
    #[must_use]
    pub fn render_row(&self, row: Row) -> String {
        let mut out = String::with_capacity(row.end - row.start);
        let mut col = 0;
        for grapheme in self.text[row.start..row.end].graphemes(true) {
            let next = advance(col, grapheme);
            if grapheme == "\t" {
                out.extend(std::iter::repeat_n(' ', next - col));
            } else {
                out.push_str(grapheme);
            }
            col = next;
        }
        out
    }

    // — Internals ————————————————————————————————————————————————

    /// Logical lines as byte ranges, excluding their terminating newline.
    /// Always yields at least one range, so an empty buffer has one empty line.
    fn logical_lines(&self) -> Vec<Row> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (offset, _) in self.text.match_indices('\n') {
            lines.push(Row { start, end: offset });
            start = offset + 1;
        }
        lines.push(Row {
            start,
            end: self.text.len(),
        });
        lines
    }

    fn caret_in(&self, rows: &[Row], width: usize) -> (usize, usize) {
        // Last row whose range contains the caret. Scanning from the end
        // resolves the boundary case where the caret sits exactly on a wrap
        // point and is therefore the end of one row and the start of the next:
        // the caret belongs at the start of the later row, where the next typed
        // grapheme will land.
        let index = rows
            .iter()
            .rposition(|row| self.cursor >= row.start && self.cursor <= row.end)
            .unwrap_or(rows.len().saturating_sub(1));
        let row = rows[index];
        let mut col = 0;
        for (offset, grapheme) in self.text[row.start..row.end].grapheme_indices(true) {
            if row.start + offset >= self.cursor {
                break;
            }
            col = advance(col, grapheme);
        }
        (index, col.min(width.max(1)))
    }

    /// Byte offset within `row` closest to display column `col`, clamped to the
    /// row's end so a vertical move onto a shorter row lands at its end.
    fn offset_at(&self, row: &Row, col: usize) -> usize {
        let mut at = row.start;
        let mut current = 0;
        for (offset, grapheme) in self.text[row.start..row.end].grapheme_indices(true) {
            if current >= col {
                return row.start + offset;
            }
            current = advance(current, grapheme);
            at = row.start + offset + grapheme.len();
        }
        at
    }

    fn prev_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(offset, _)| offset)
    }

    fn next_boundary(&self, at: usize) -> usize {
        self.text[at..]
            .graphemes(true)
            .next()
            .map_or(at, |grapheme| at + grapheme.len())
    }

    /// Largest grapheme boundary `<= at`.
    fn floor_boundary(&self, at: usize) -> usize {
        let at = at.min(self.text.len());
        if self.text.is_char_boundary(at) && self.is_grapheme_boundary(at) {
            return at;
        }
        self.prev_boundary(self.snap_char(at))
    }

    /// Smallest grapheme boundary `>= at`.
    fn ceil_boundary(&self, at: usize) -> usize {
        let at = at.min(self.text.len());
        if self.text.is_char_boundary(at) && self.is_grapheme_boundary(at) {
            return at;
        }
        self.next_boundary(self.prev_boundary(self.snap_char(at)))
    }

    fn snap_char(&self, mut at: usize) -> usize {
        while at > 0 && !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn is_grapheme_boundary(&self, at: usize) -> bool {
        at == 0
            || at == self.text.len()
            || self
                .text
                .grapheme_indices(true)
                .any(|(offset, _)| offset == at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Family emoji: five scalars joined by ZWJs, drawn as one glyph.
    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    /// `e` + combining acute. Two chars, three bytes, one column.
    const E_ACUTE: &str = "e\u{301}";

    fn with(text: &str) -> Composer {
        let mut composer = Composer::new();
        composer.insert_paste(text);
        composer
    }

    #[test]
    fn measures_display_columns_not_chars_or_bytes() {
        // The three disagree, and only the column count positions a caret.
        assert_eq!(grapheme_width("a"), 1);
        assert_eq!(grapheme_width("世"), 2);
        assert_eq!(grapheme_width(E_ACUTE), 1, "combining mark adds no column");
        assert_eq!(grapheme_width(FAMILY), 2, "ZWJ sequence is one glyph");
        assert_eq!(grapheme_width("\u{1F1F0}\u{1F1F7}"), 2, "flag is one glyph");
        assert_eq!(grapheme_width("\u{2615}\u{FE0F}"), 2, "emoji + selector");
    }

    #[test]
    fn caret_column_tracks_glyphs_across_scripts() {
        let mut composer = Composer::new();
        for ch in "世界".chars() {
            composer.insert_char(ch);
        }
        // 2 chars, 6 bytes, 4 columns — the caret belongs at 4.
        assert_eq!(composer.text().chars().count(), 2);
        assert_eq!(composer.cursor(), 6);
        assert_eq!(composer.caret(80), (0, 4));

        let composer = with(FAMILY);
        assert_eq!(composer.caret(80), (0, 2), "five scalars, two columns");

        let composer = with(E_ACUTE);
        assert_eq!(composer.caret(80), (0, 1));
    }

    #[test]
    fn backspace_removes_a_whole_grapheme_cluster() {
        let mut composer = with(E_ACUTE);
        composer.backspace();
        assert!(
            composer.is_empty(),
            "base and combining mark must go together, leaving no orphan accent"
        );

        let mut composer = with(FAMILY);
        composer.backspace();
        assert!(composer.is_empty(), "the whole ZWJ sequence is one delete");

        let mut composer = with("世界");
        composer.backspace();
        assert_eq!(composer.text(), "世");
    }

    #[test]
    fn backspace_and_delete_are_no_ops_at_the_edges() {
        let mut composer = Composer::new();
        composer.backspace();
        composer.delete_forward();
        assert!(composer.is_empty());

        let mut composer = with("ab");
        composer.move_line_start();
        composer.backspace();
        assert_eq!(composer.text(), "ab", "backspace at offset 0 does nothing");
        composer.move_line_end();
        composer.delete_forward();
        assert_eq!(composer.text(), "ab", "delete at the end does nothing");
    }

    #[test]
    fn motion_steps_by_cluster_in_both_directions() {
        let mut composer = with(&format!("a{E_ACUTE}世"));
        composer.move_line_start();
        assert_eq!(composer.cursor(), 0);
        composer.move_right();
        assert_eq!(composer.cursor(), 1, "past 'a'");
        composer.move_right();
        assert_eq!(composer.cursor(), 4, "past the whole e+accent cluster");
        composer.move_right();
        assert_eq!(composer.cursor(), 7, "past 世");
        composer.move_right();
        assert_eq!(composer.cursor(), 7, "clamped at the end");
        composer.move_left();
        assert_eq!(composer.cursor(), 4);
        composer.move_left();
        assert_eq!(composer.cursor(), 1);
    }

    #[test]
    fn paste_normalises_line_endings() {
        let mut composer = Composer::new();
        composer.insert_paste("one\r\ntwo\rthree\nfour");
        assert_eq!(
            composer.text(),
            "one\ntwo\nthree\nfour",
            "CRLF and lone CR both become one newline"
        );
        assert_eq!(composer.rows(80).len(), 4);
    }

    #[test]
    fn paste_strips_control_characters_but_keeps_tabs_and_newlines() {
        let mut composer = Composer::new();
        // An ESC in pasted text would otherwise be handed to the terminal as
        // the start of a control sequence when the row is drawn.
        composer.insert_paste("safe\u{1b}[31m\u{7}text\n\tindented");
        assert_eq!(composer.text(), "safe[31mtext\n\tindented");
        assert!(!composer.text().contains('\u{1b}'));
        assert!(!composer.text().contains('\u{7}'));
    }

    #[test]
    fn paste_inserts_at_the_caret_without_submitting() {
        let mut composer = with("start end");
        // Park the caret between the words.
        composer.move_line_start();
        for _ in 0..6 {
            composer.move_right();
        }
        composer.insert_paste("A\nB\n");
        assert_eq!(composer.text(), "start A\nB\nend");
        assert_eq!(composer.cursor(), 10, "caret follows the pasted text");
    }

    #[test]
    fn multi_line_paste_is_one_draft_not_many_submissions() {
        let mut composer = Composer::new();
        let pasted = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        composer.insert_paste(&pasted);
        assert_eq!(composer.rows(80).len(), 200);
        assert_eq!(composer.text(), pasted, "content survives byte for byte");
    }

    #[test]
    fn wraps_at_grapheme_boundaries_never_mid_cluster() {
        // Three double-width glyphs in a five-column area: two fit, the third
        // wraps whole rather than being split across the boundary.
        let composer = with("世界語");
        let rows = composer.rows(5);
        assert_eq!(rows.len(), 2);
        assert_eq!(&composer.text()[rows[0].start..rows[0].end], "世界");
        assert_eq!(&composer.text()[rows[1].start..rows[1].end], "語");
    }

    #[test]
    fn caret_at_an_exact_wrap_point_sits_on_the_next_row() {
        let composer = with("abcd");
        // Width 4, caret at the end: the next typed character lands on row 1,
        // so that is where the caret must be drawn.
        let rows = composer.rows(4);
        assert_eq!(rows.len(), 2, "a full row is followed by an empty one");
        assert_eq!(composer.caret(4), (1, 0));
    }

    #[test]
    fn tabs_advance_to_the_next_tab_stop() {
        let composer = with("\tx");
        assert_eq!(composer.render_row(composer.rows(80)[0]), "    x");

        // A tab after two columns advances to column 4, not by a fixed width.
        let composer = with("ab\tx");
        assert_eq!(composer.render_row(composer.rows(80)[0]), "ab  x");

        let mut composer = with("ab\t");
        composer.move_line_end();
        assert_eq!(composer.caret(80), (0, TAB_WIDTH));
    }

    #[test]
    fn logical_lines_survive_wrapping() {
        let composer = with("a\n\nb");
        let rows = composer.rows(80);
        assert_eq!(rows.len(), 3, "the blank line keeps its own row");
        assert_eq!(&composer.text()[rows[1].start..rows[1].end], "");
    }

    #[test]
    fn line_start_and_end_are_per_logical_line() {
        let mut composer = with("first\nsecond\nthird");
        // Caret sits at the end, on the third line.
        composer.move_line_start();
        assert_eq!(composer.cursor(), 13, "start of 'third', not of the buffer");
        composer.move_line_end();
        assert_eq!(composer.cursor(), 18);
    }

    #[test]
    fn single_line_home_end_span_the_whole_text() {
        // With no newline, Home/End mean start-of-text / end-of-text.
        let mut composer = with("hello world");
        composer.move_line_start();
        assert_eq!(composer.cursor(), 0);
        composer.move_line_end();
        assert_eq!(composer.cursor(), 11);
    }

    #[test]
    fn vertical_motion_holds_the_column_and_clamps_to_short_rows() {
        let mut composer = with("longest line\nab\nanother line");
        composer.move_line_start();
        for _ in 0..7 {
            composer.move_right();
        }
        assert_eq!(composer.caret(80), (2, 7));

        assert!(composer.move_row_up(80));
        assert_eq!(
            composer.caret(80),
            (1, 2),
            "column clamps to the end of the shorter row"
        );

        assert!(composer.move_row_up(80));
        assert_eq!(composer.caret(80), (0, 2));
    }

    #[test]
    fn vertical_motion_declines_past_the_first_and_last_row() {
        // Declining rather than swallowing is what leaves Up/Down free to mean
        // "walk prompt history" at the composer's edges.
        let mut composer = with("only line");
        assert!(!composer.move_row_up(80), "no row above");
        assert!(!composer.move_row_down(80), "no row below");

        let mut composer = with("a\nb");
        composer.move_line_end();
        assert!(!composer.move_row_down(80));
        assert!(composer.move_row_up(80));
        assert!(!composer.move_row_up(80));
    }

    #[test]
    fn vertical_motion_moves_within_a_wrapped_logical_line() {
        let mut composer = with("abcdefgh");
        composer.move_line_end();
        assert_eq!(composer.caret(4), (2, 0));
        assert!(composer.move_row_up(4), "wrapped rows are rows too");
        assert_eq!(composer.caret(4), (1, 0));
    }

    #[test]
    fn word_motion_crosses_newlines() {
        let mut composer = with("alpha beta\ngamma");
        assert_eq!(composer.cursor(), 16);
        composer.cursor = composer.prev_word();
        assert_eq!(&composer.text()[composer.cursor()..], "gamma");
        composer.cursor = composer.prev_word();
        assert_eq!(&composer.text()[composer.cursor()..], "beta\ngamma");
    }

    #[test]
    fn word_motion_stops_at_the_buffer_edges() {
        let mut composer = with("   ");
        assert_eq!(composer.prev_word(), 0, "all whitespace collapses to 0");
        composer.move_line_start();
        assert_eq!(composer.next_word(), 3);
    }

    #[test]
    fn delete_range_snaps_outward_to_cluster_boundaries() {
        let mut composer = with(&format!("x{E_ACUTE}y"));
        // Offset 2 is inside the e+accent cluster. Deleting [1, 2) must not
        // leave a severed cluster behind.
        composer.delete_range(1, 2);
        assert_eq!(composer.text(), "xy");
        assert_eq!(composer.cursor(), 1);
    }

    #[test]
    fn delete_range_ignores_an_empty_span() {
        let mut composer = with("abc");
        composer.delete_range(2, 2);
        assert_eq!(composer.text(), "abc");
    }

    #[test]
    fn narrow_terminals_still_lay_out() {
        let composer = with("世a");
        // One column cannot fit a double-width glyph. It overflows its row
        // rather than being split or dropped, and layout still terminates.
        let rows = composer.rows(1);
        assert_eq!(rows.len(), 3);
        assert_eq!(&composer.text()[rows[0].start..rows[0].end], "世");
        assert_eq!(&composer.text()[rows[1].start..rows[1].end], "a");
        assert_eq!(&composer.text()[rows[2].start..rows[2].end], "");
        // Zero width is clamped to one rather than dividing by zero.
        assert!(!composer.rows(0).is_empty());
    }

    #[test]
    fn is_multiline_reports_hard_breaks_not_wrapping() {
        assert!(!with("a long line that will certainly wrap").is_multiline());
        assert!(with("a\nb").is_multiline());
    }

    #[test]
    fn clear_resets_text_and_caret() {
        let mut composer = with("something");
        composer.clear();
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.caret(80), (0, 0));
    }

    #[test]
    fn an_odd_trailing_backslash_continues_the_line() {
        let mut composer = with("first\\");
        assert!(composer.take_line_continuation());
        assert_eq!(composer.text(), "first\n");
        assert_eq!(composer.cursor(), 6);
        assert!(composer.is_multiline());
    }

    #[test]
    fn an_even_trailing_backslash_is_a_literal_and_submits() {
        // A pasted Windows path must not turn into a line continuation.
        let mut composer = with("C:\\\\dir\\\\");
        assert!(!composer.take_line_continuation());
        assert_eq!(composer.text(), "C:\\\\dir\\\\");
    }

    #[test]
    fn no_trailing_backslash_is_not_a_continuation() {
        let mut composer = with("plain");
        assert!(!composer.take_line_continuation());
        let mut composer = Composer::new();
        assert!(!composer.take_line_continuation(), "empty buffer is safe");
    }

    #[test]
    fn continuation_applies_at_the_caret_not_the_end() {
        let mut composer = with("a\\bc");
        composer.move_line_start();
        composer.move_right();
        composer.move_right();
        assert!(composer.take_line_continuation());
        assert_eq!(composer.text(), "a\nbc");
    }
}
