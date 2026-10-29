//! Reusable ratatui widgets shared across panels.
//!
//! Panels are free to render raw ratatui widgets, but a few patterns recur
//! often enough — and are easy enough to get subtly wrong — that they live
//! here instead.

use crate::nav::NavIntent;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

/// A virtualized, word-wrapped, scrollable transcript backed by [`Paragraph`].
///
/// A bare `Paragraph::scroll((n, 0))` over the whole transcript has three
/// problems every panel otherwise reinvents a fix for:
///
/// 1. **No clamp.** `scroll` is just an offset; paging past the end scrolls the
///    content entirely out of view into blank space.
/// 2. **Wrong unit.** The offset is in *rendered* rows, but the panel only knows
///    its *logical* line count — so the clamp must account for word wrapping at
///    the current width.
/// 3. **O(total content) per frame.** `Paragraph` re-wraps and lays out *every*
///    line each frame, then `.scroll()` discards the off-screen rows. On a long
///    transcript that turns every keystroke into a full re-wrap of all history —
///    unlike a real terminal, whose scrollback is a grid it just windows over.
///
/// This widget fixes all three. It owns the scroll offset and a "follow the
/// tail" flag, and **virtualizes** rendering: the content is wrapped only when
/// it (or the viewport size) actually changes — keyed by a caller-supplied
/// `revision` — into a per-logical-line row-height prefix sum. Each frame it
/// binary-searches that prefix to the first visible line and hands `Paragraph`
/// only the ~viewport-worth of lines that can show, so steady-state scrolling is
/// O(viewport), not O(history).
///
/// The widget owns the cached content; the panel supplies a `build` closure that
/// is invoked **only on a cache miss**, so an unchanged transcript costs nothing
/// to "rebuild" between frames.
#[derive(Debug, Clone)]
pub struct ScrollableTranscript {
    scroll: u32,
    follow_tail: bool,
    /// Viewport height seen at the last render — `apply` needs it to size a
    /// page before the next render happens.
    viewport_rows: u16,
    /// Wrapped row count seen at the last render, for clamping.
    total_rows: u32,
    /// The scrollbar gutter's screen rect at the last render, in the same
    /// absolute coordinates as a [`crossterm::event::MouseEvent`]. `None`
    /// before the first render or whenever no gutter was reserved, so a
    /// mouse event arriving between frames can hit-test against it without
    /// the panel keeping any render geometry of its own.
    track: Option<Rect>,
    /// Wrapped-content cache, rebuilt only when the key changes.
    cache: Option<TranscriptCache>,
}

/// Cached wrap of the transcript at a specific `(revision, width, height)`.
///
/// `prefix[i]` is the number of wrapped rows occupying logical lines `0..i`, so
/// `prefix.last()` is the total and `prefix.partition_point(...)` locates the
/// logical line containing any wrapped-row offset in `O(log n)`.
#[derive(Debug, Clone)]
struct TranscriptCache {
    /// `(revision, area.width, area.height)` this cache was built for.
    key: (u64, u16, u16),
    /// Reserved scrollbar gutter width (0 or 1).
    gutter: u16,
    /// The logical lines, owned so the window can be sliced each frame.
    lines: Vec<Line<'static>>,
    /// Cumulative wrapped-row counts; `len() == lines.len() + 1`, `prefix[0] == 0`.
    prefix: Vec<u32>,
    search_text: Vec<String>,
}

impl Default for ScrollableTranscript {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollableTranscript {
    /// A new transcript pinned to the tail — the natural state for a chat or
    /// log view, where the latest content is what you want on screen.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scroll: 0,
            follow_tail: true,
            viewport_rows: 0,
            total_rows: 0,
            track: None,
            cache: None,
        }
    }

    /// Current scroll offset in rendered (wrapped) rows.
    #[must_use]
    pub fn offset(&self) -> u16 {
        self.scroll.min(u32::from(u16::MAX)) as u16
    }

    /// Whether the view is pinned to the tail and will track new content.
    #[must_use]
    pub fn is_following(&self) -> bool {
        self.follow_tail
    }

    /// The largest valid scroll offset given the geometry of the last render.
    fn max_scroll(&self) -> u32 {
        self.total_rows
            .saturating_sub(u32::from(self.viewport_rows))
    }

    /// Current wrapped-row offset without the legacy 16-bit saturation of [`Self::offset`].
    #[must_use]
    pub fn row_offset(&self) -> u32 {
        self.scroll
    }

    /// Logical line and row within it at the top of the viewport.
    /// Logical anchors survive wrapping changes when the terminal is resized.
    #[must_use]
    pub fn anchor(&self) -> Option<(usize, u32)> {
        let cache = self.cache.as_ref()?;
        if cache.lines.is_empty() {
            return None;
        }
        let line = cache
            .prefix
            .partition_point(|row| *row <= self.scroll)
            .saturating_sub(1)
            .min(cache.lines.len() - 1);
        Some((line, self.scroll.saturating_sub(cache.prefix[line])))
    }

    /// Jump to a cached logical line and stop following new output.
    /// Returns false before the first render or for an invalid line.
    pub fn jump_to_line(&mut self, line: usize) -> bool {
        let Some(cache) = &self.cache else {
            return false;
        };
        if line >= cache.lines.len() {
            return false;
        }
        self.scroll = cache.prefix[line].min(self.max_scroll());
        self.follow_tail = false;
        true
    }

    /// Reveal the first occurrence of `query` within a logical line, including
    /// matches below its first wrapped row. Uses the same Paragraph wrapping
    /// as rendering, measured through the end of the matched word.
    pub fn jump_to_match(&mut self, line: usize, query: &str) -> bool {
        self.jump_to_occurrence(line, query, 0)
    }

    /// Reveal the zero-based occurrence of `query` in one logical line.
    pub fn jump_to_occurrence(&mut self, line: usize, query: &str, occurrence: usize) -> bool {
        let Some(cache) = &self.cache else {
            return false;
        };
        let Some(text) = cache.lines.get(line).map(ToString::to_string) else {
            return false;
        };
        let needle = query.to_lowercase();
        let folded_end = text
            .to_lowercase()
            .match_indices(&needle)
            .nth(occurrence)
            .map(|(at, _)| at + needle.len());
        let mut folded_bytes = 0;
        let end = folded_end
            .and_then(|target| {
                text.char_indices().find_map(|(offset, ch)| {
                    folded_bytes += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
                    (folded_bytes >= target).then_some(offset + ch.len_utf8())
                })
            })
            .unwrap_or(0);
        let rows = Paragraph::new(text[..end].to_owned())
            .wrap(Wrap { trim: false })
            .line_count(cache.key.1.saturating_sub(cache.gutter));
        self.scroll = cache.prefix[line]
            .saturating_add(rows.saturating_sub(1) as u32)
            .min(self.max_scroll());
        self.follow_tail = false;
        true
    }

    /// Find the next case-insensitive literal match, wrapping at the end.
    /// `after` is the previous matching logical line; `None` starts at the
    /// beginning (or end for backwards searches). Searches cached displayed
    /// text, independent of style spans and terminal width.
    #[must_use]
    pub fn find(&self, query: &str, after: Option<usize>, backwards: bool) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        let cache = self.cache.as_ref()?;
        let len = cache.lines.len();
        if len == 0 {
            return None;
        }
        let needle = query.to_lowercase();
        let first = match (after, backwards) {
            (Some(i), false) => (i.saturating_add(1)) % len,
            (Some(i), true) => (i.min(len).saturating_add(len) - 1) % len,
            (None, false) => 0,
            (None, true) => len - 1,
        };
        (0..len)
            .map(|step| {
                if backwards {
                    (first + len - step) % len
                } else {
                    (first + step) % len
                }
            })
            .find(|&i| cache.search_text[i].contains(&needle))
    }

    /// Find an occurrence as `(logical line, occurrence within that line)`.
    /// Repeated matches on one wrapped line are distinct. Search wraps once
    /// and returns `None` for an empty query or no matches.
    #[must_use]
    pub fn find_occurrence(
        &self,
        query: &str,
        after: Option<(usize, usize)>,
        backwards: bool,
    ) -> Option<(usize, usize)> {
        if query.is_empty() {
            return None;
        }
        let cache = self.cache.as_ref()?;
        let needle = query.to_lowercase();
        let mut first = None;
        let mut last = None;
        let mut chosen = None;
        for (line, text) in cache.search_text.iter().enumerate() {
            for (occurrence, _) in text.match_indices(&needle).enumerate() {
                let hit = (line, occurrence);
                first.get_or_insert(hit);
                last = Some(hit);
                if backwards {
                    if after.is_none_or(|cursor| hit < cursor) {
                        chosen = Some(hit);
                    }
                } else if after.is_none_or(|cursor| hit > cursor) {
                    return Some(hit);
                }
            }
        }
        if backwards { chosen.or(last) } else { first }
    }

    /// Apply a navigation intent.
    ///
    /// Uses the viewport height cached from the previous [`render`](Self::render)
    /// to size a page — accurate in steady state, and harmless (a zero-height
    /// page) before the first render. Any downward move that lands at the
    /// bottom re-engages tail-following; any upward move disengages it.
    pub fn apply(&mut self, intent: NavIntent) {
        // Keep one row of context across a page jump, like a pager does.
        let page = u32::from(self.viewport_rows.saturating_sub(1).max(1));
        // Keep this match exhaustive (no catch-all) so new variants force a
        // decision here.
        match intent {
            NavIntent::LineUp => {
                self.scroll = self.scroll.saturating_sub(1);
                self.follow_tail = false;
            }
            NavIntent::LineDown => {
                self.scroll = self.scroll.saturating_add(1);
            }
            NavIntent::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                self.follow_tail = false;
            }
            NavIntent::PageDown => {
                self.scroll = self.scroll.saturating_add(page);
            }
            NavIntent::Top => {
                self.scroll = 0;
                self.follow_tail = false;
            }
            NavIntent::Bottom => {
                self.follow_tail = true;
            }
        }
        self.scroll = self.scroll.min(self.max_scroll());
        // A downward move (or Bottom) that reaches the end re-pins the tail.
        if self.scroll == self.max_scroll()
            && matches!(
                intent,
                NavIntent::LineDown | NavIntent::PageDown | NavIntent::Bottom
            )
        {
            self.follow_tail = true;
        }
    }

    /// Render the transcript into `area`, clamping the scroll offset against the
    /// wrapped row count and re-pinning to the tail if following.
    ///
    /// `revision` is a cheap, monotonic-ish content fingerprint the caller bumps
    /// whenever the lines `build` would produce change. When it (and the area
    /// size) match the cache, `build` is **not** called and the wrap is reused;
    /// otherwise `build` runs once and its lines are wrapped and cached. Build
    /// fresh `'static` lines from the panel's data — if the panel wraps the
    /// transcript in a [`Block`](ratatui::widgets::Block), pass the block's
    /// *inner* area so the wrap width is correct.
    pub fn render<F>(&mut self, frame: &mut Frame<'_>, area: Rect, revision: u64, build: F)
    where
        F: FnOnce() -> Vec<Line<'static>>,
    {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Compute the new geometry/scroll into locals so the borrow of `cache`
        // (returned by `refresh_cache`) ends before the `self` write-back below.
        let follow_tail = self.follow_tail;
        let prev_scroll = self.scroll;
        let anchor = self.anchor();
        let cache = self.refresh_cache(area, revision, build);

        let gutter = cache.gutter;
        let text_area = Rect {
            width: area.width - gutter,
            ..area
        };
        let total_rows = *cache.prefix.last().unwrap_or(&0);
        let viewport_rows = text_area.height;
        let max_scroll = total_rows.saturating_sub(u32::from(viewport_rows));
        let scroll = if follow_tail {
            max_scroll
        } else {
            anchor
                .and_then(|(line, intra)| {
                    cache.prefix.get(line).map(|row| {
                        let height = cache
                            .prefix
                            .get(line + 1)
                            .copied()
                            .unwrap_or(*row)
                            .saturating_sub(*row);
                        row.saturating_add(intra.min(height.saturating_sub(1)))
                    })
                })
                .unwrap_or(prev_scroll)
                .min(max_scroll)
        };

        // Window: only the logical lines that can appear in the viewport are
        // handed to `Paragraph`, so it wraps O(viewport) rows rather than the
        // whole transcript. `intra` is the offset *within* the first such line.
        if !cache.lines.is_empty() {
            let scroll_rows = scroll;
            let first = cache
                .prefix
                .partition_point(|&p| p <= scroll_rows)
                .saturating_sub(1);
            let intra =
                u16::try_from(scroll.saturating_sub(cache.prefix[first])).unwrap_or(u16::MAX);
            let end_row = scroll_rows + u32::from(text_area.height);
            let mut last = cache.prefix.partition_point(|&p| p < end_row);
            last = last.clamp(first + 1, cache.lines.len());

            let window: Vec<Line<'static>> = cache.lines[first..last].to_vec();
            let paragraph = Paragraph::new(window)
                .wrap(Wrap { trim: false })
                .scroll((intra, 0));
            frame.render_widget(paragraph, text_area);
        }
        // `cache` borrow ends here — `self` is free to mutate again.

        // Native ratatui affordance, drawn into the reserved gutter column to the
        // right of the text — never over it. Ratatui only draws the thumb from
        // the offset we already track; it takes no input itself. The track rect
        // is persisted so a later, separate mouse event (see `drag_gutter`) can
        // hit-test the gutter without the panel keeping its own render geometry.
        self.track = if gutter > 0 {
            let mut state = ScrollbarState::new(max_scroll as usize).position(scroll as usize);
            let track = Rect {
                x: text_area.x + text_area.width,
                y: area.y,
                width: 1,
                height: area.height,
            };
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                track,
                &mut state,
            );
            Some(track)
        } else {
            None
        };

        self.total_rows = total_rows;
        self.viewport_rows = viewport_rows;
        self.scroll = scroll;
    }

    /// If `(column, row)` — in the same absolute coordinates as a
    /// [`crossterm::event::MouseEvent`] — falls on the scrollbar gutter drawn
    /// at the last render, jump to the position that row implies and return
    /// `true`. Otherwise a no-op returning `false`, so the caller can let a
    /// click or drag outside the gutter pass through untouched.
    ///
    /// Always `false` before the first render and whenever the last render
    /// had no gutter (content fit the viewport, nothing to scroll) — there is
    /// no track to hit in either case. Disengages tail-following unless the
    /// resulting position is the very bottom.
    pub fn drag_gutter(&mut self, column: u16, row: u16) -> bool {
        let Some(track) = self.track else {
            return false;
        };
        if column != track.x || row < track.y || row >= track.y.saturating_add(track.height) {
            return false;
        }
        let max_scroll = self.max_scroll();
        if max_scroll > 0 && track.height > 1 {
            let offset = row.saturating_sub(track.y).min(track.height - 1);
            let fraction = f64::from(offset) / f64::from(track.height - 1);
            self.scroll = (fraction * f64::from(max_scroll)).round() as u32;
            self.follow_tail = self.scroll >= max_scroll;
        }
        true
    }

    /// Return the wrap cache for `(revision, width, height)`, rebuilding it (and
    /// running the `build` closure) only on a miss. Dropping a stale entry first
    /// lets [`Option::get_or_insert_with`] hand back a guaranteed-live `&`, so
    /// the caller never has to unwrap a freshly populated `Option`.
    fn refresh_cache<F>(&mut self, area: Rect, revision: u64, build: F) -> &TranscriptCache
    where
        F: FnOnce() -> Vec<Line<'static>>,
    {
        let key = (revision, area.width, area.height);
        if self.cache.as_ref().is_none_or(|c| c.key != key) {
            self.cache = None;
        }
        self.cache.get_or_insert_with(|| {
            let lines = build();
            // Wrapping narrower only ever *adds* rows, so a transcript that
            // overflows at full width still overflows at width-1: the gutter
            // decision is stable. Measure at full width first; reserve a
            // 1-column gutter and re-measure only when it overflows (so the
            // common short case wraps just once).
            let prefix_full = wrapped_prefix(&lines, area.width);
            let overflows =
                u16::try_from(*prefix_full.last().unwrap_or(&0)).unwrap_or(u16::MAX) > area.height;
            let gutter = u16::from(overflows && area.width > 1);
            let prefix = if gutter == 0 {
                prefix_full
            } else {
                wrapped_prefix(&lines, area.width - gutter)
            };
            let search_text = lines
                .iter()
                .map(|line| line.to_string().to_lowercase())
                .collect();
            TranscriptCache {
                search_text,
                key,
                gutter,
                lines,
                prefix,
            }
        })
    }
}

/// Build the cumulative wrapped-row prefix sum for `lines` at `width`.
///
/// `out[0] == 0` and `out[i+1] - out[i]` is the number of rows logical line `i`
/// occupies once word-wrapped at `width` (at least 1, so a blank line still
/// takes a row). Uses ratatui's own wrapper via [`Paragraph::line_count`] so the
/// measured heights match what actually renders.
fn wrapped_prefix(lines: &[Line<'static>], width: u16) -> Vec<u32> {
    let mut prefix = Vec::with_capacity(lines.len() + 1);
    let mut acc: u32 = 0;
    prefix.push(0);
    for line in lines {
        let rows = if width == 0 {
            1
        } else {
            u32::try_from(
                Paragraph::new(line.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(width),
            )
            .unwrap_or(u32::MAX)
            .max(1)
        };
        acc = acc.saturating_add(rows);
        prefix.push(acc);
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Render `lines` into a `width x height` terminal and return the
    /// transcript's resulting scroll offset. Each call uses a fresh revision so
    /// the cache rebuilds, mirroring a content change.
    fn render_at(
        transcript: &mut ScrollableTranscript,
        width: u16,
        height: u16,
        lines: &[&str],
    ) -> u16 {
        // Distinct revision per (content, size) so the cache reflects this call.
        let revision = lines.iter().map(|s| s.len() as u64).sum::<u64>()
            ^ (u64::from(width) << 32)
            ^ (u64::from(height) << 48)
            ^ ((lines.len() as u64) << 16);
        let owned: Vec<Line<'static>> = lines.iter().map(|s| Line::from(s.to_string())).collect();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                transcript.render(frame, frame.area(), revision, || owned.clone());
            })
            .unwrap();
        transcript.offset()
    }

    #[test]
    fn following_pins_to_tail_as_content_grows() {
        let mut t = ScrollableTranscript::new();
        // 10 lines into a 4-row viewport: tail offset is 10 - 4 = 6.
        let lines: Vec<&str> = (0..10).map(|_| "line").collect();
        assert_eq!(render_at(&mut t, 20, 4, &lines), 6);
        assert!(t.is_following());

        // Two more lines while followed: offset tracks to the new tail.
        let more: Vec<&str> = (0..12).map(|_| "line").collect();
        assert_eq!(render_at(&mut t, 20, 4, &more), 8);
    }

    #[test]
    fn page_down_cannot_scroll_past_the_end() {
        let mut t = ScrollableTranscript::new();
        let lines: Vec<&str> = (0..10).map(|_| "line").collect();
        render_at(&mut t, 20, 4, &lines); // caches geometry: 10 rows, 4 tall.

        t.apply(NavIntent::Top);
        assert_eq!(t.offset(), 0);
        assert!(!t.is_following());

        // Several page-downs must saturate at max_scroll (6), not run away.
        for _ in 0..5 {
            t.apply(NavIntent::PageDown);
        }
        assert_eq!(t.offset(), 6);
        assert!(t.is_following());
    }

    #[test]
    fn wrapped_lines_count_as_multiple_rows() {
        let mut t = ScrollableTranscript::new();
        // One spaceless 28-char line. At full width 10 it needs 3 rows (>2 tall),
        // so the content overflows and a 1-column scrollbar gutter is reserved —
        // leaving width 9 for text, which breaks the word into 9+9+9+1 = 4 rows.
        // The tail offset is therefore 4 - 2 = 2, proving the clamp uses *wrapped*
        // rows measured against the gutter-reduced text width, not the logical line.
        let long = "0123456789012345678901234567";
        assert_eq!(render_at(&mut t, 10, 2, &[long]), 2);
    }

    #[test]
    fn no_gutter_reserved_when_content_fits() {
        // Content within the viewport doesn't overflow, so no scrollbar gutter is
        // reserved and the full width is available — the offset stays pinned at 0.
        let mut t = ScrollableTranscript::new();
        assert_eq!(render_at(&mut t, 10, 5, &["short", "lines"]), 0);
    }

    #[test]
    fn line_up_disengages_follow() {
        let mut t = ScrollableTranscript::new();
        let lines: Vec<&str> = (0..10).map(|_| "line").collect();
        render_at(&mut t, 20, 4, &lines);
        assert!(t.is_following());

        t.apply(NavIntent::LineUp);
        assert!(!t.is_following());
        assert_eq!(t.offset(), 5);
    }

    #[test]
    fn content_shorter_than_viewport_clamps_to_zero() {
        let mut t = ScrollableTranscript::new();
        assert_eq!(render_at(&mut t, 20, 10, &["only", "three", "lines"]), 0);
        t.apply(NavIntent::PageDown);
        assert_eq!(t.offset(), 0);
    }

    #[test]
    fn build_closure_skipped_on_cache_hit() {
        use std::cell::Cell;
        // Same revision + size across two renders → the second must reuse the
        // cache and not invoke the build closure (the virtualization contract).
        let mut t = ScrollableTranscript::new();
        let builds = Cell::new(0u32);
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        for _ in 0..2 {
            terminal
                .draw(|frame| {
                    t.render(frame, frame.area(), 42, || {
                        builds.set(builds.get() + 1);
                        (0..10).map(|i| Line::from(format!("line {i}"))).collect()
                    });
                })
                .unwrap();
        }
        assert_eq!(
            builds.get(),
            1,
            "closure must run once for a stable revision"
        );

        // A new revision forces exactly one more rebuild.
        terminal
            .draw(|frame| {
                t.render(frame, frame.area(), 43, || {
                    builds.set(builds.get() + 1);
                    (0..10).map(|i| Line::from(format!("line {i}"))).collect()
                });
            })
            .unwrap();
        assert_eq!(builds.get(), 2);
    }

    #[test]
    fn windowed_render_matches_a_known_viewport() {
        // 100 short lines, width 20, height 5. Tail offset = 100 - 5 = 95, so the
        // visible window is lines 95..=99. Virtualization must still land the tail
        // exactly where a full-paragraph scroll would.
        let mut t = ScrollableTranscript::new();
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(render_at(&mut t, 20, 5, &refs), 95);

        // Scroll to the very top and confirm the offset pins at 0.
        t.apply(NavIntent::Top);
        assert_eq!(t.offset(), 0);
    }

    #[test]
    fn drag_gutter_jumps_to_the_clicked_fraction_of_the_track() {
        let mut t = ScrollableTranscript::new();
        // 15 one-row lines into a 20x5 viewport overflow, reserving a
        // 1-column gutter at x = 19 (0-indexed); max_scroll = 15 - 5 = 10.
        let lines: Vec<String> = (0..15).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(
            render_at(&mut t, 20, 5, &refs),
            10,
            "starts pinned to the tail"
        );

        assert!(t.drag_gutter(19, 0));
        assert_eq!(t.offset(), 0, "top of the track jumps to the very start");
        assert!(!t.is_following());

        assert!(t.drag_gutter(19, 2));
        assert_eq!(t.offset(), 5, "middle of the track jumps to the midpoint");

        assert!(t.drag_gutter(19, 4));
        assert_eq!(t.offset(), 10, "bottom of the track jumps to the tail");
        assert!(t.is_following(), "landing on the last row re-pins the tail");
    }

    #[test]
    fn drag_outside_the_gutter_column_or_track_is_a_miss() {
        let mut t = ScrollableTranscript::new();
        let lines: Vec<String> = (0..15).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(render_at(&mut t, 20, 5, &refs), 10);

        // One column left of the gutter: a miss that leaves the scroll and
        // the follow state exactly as they were, regardless of row.
        assert!(!t.drag_gutter(18, 2));
        assert_eq!(t.offset(), 10);
        assert!(t.is_following());

        // The right column, but a row outside the track's height.
        assert!(!t.drag_gutter(19, 5));
        assert_eq!(t.offset(), 10);
        assert!(t.is_following());
    }

    #[test]
    fn drag_gutter_is_a_miss_before_any_render_and_when_no_gutter_is_reserved() {
        let mut t = ScrollableTranscript::new();
        assert!(!t.drag_gutter(0, 0), "no render yet: nothing to hit");

        // Content that fits the viewport reserves no gutter at all, so every
        // column — including where the gutter would sit if it existed —
        // must be a miss.
        assert_eq!(render_at(&mut t, 10, 5, &["short", "lines"]), 0);
        for column in 0..10 {
            assert!(
                !t.drag_gutter(column, 0),
                "column {column} must pass through: there is no scrollbar to hit"
            );
        }
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    fn draw(
        view: &mut ScrollableTranscript,
        width: u16,
        revision: u64,
        lines: &[Line<'static>],
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 5)).unwrap();
        terminal
            .draw(|f| view.render(f, f.area(), revision, || lines.to_vec()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }
    #[test]
    fn large_transcript_reaches_past_u16_and_reuses_cache() {
        let mut view = ScrollableTranscript::new();
        let lines: Vec<_> = (0..70_000).map(|i| Line::raw(format!("row {i}"))).collect();
        assert!(draw(&mut view, 30, 1, &lines).contains("row 69999"));
        assert!(view.row_offset() > 65_535);
        view.jump_to_line(69_000);
        assert!(draw(&mut view, 30, 1, &lines).contains("row 69000"));
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
        terminal
            .draw(|f| {
                view.render(f, f.area(), 1, || {
                    panic!("idle scrolling rebuilt all history")
                })
            })
            .unwrap();
    }
    #[test]
    fn resize_preserves_logical_anchor_and_search_reveals_a_wrapped_match() {
        let mut view = ScrollableTranscript::new();
        let mut lines: Vec<_> = (0..10)
            .map(|i| Line::raw(format!("row {i} has several words to wrap")))
            .collect();
        lines.push(Line::raw(format!("{}needle end", "padding ".repeat(30))));
        lines.extend((0..10).map(|_| Line::raw("after")));
        draw(&mut view, 50, 1, &lines);
        view.jump_to_line(5);
        draw(&mut view, 18, 1, &lines);
        assert_eq!(view.anchor().unwrap().0, 5);
        let line = view.find("NEEDLE", None, false).unwrap();
        view.jump_to_match(line, "NEEDLE");
        assert!(draw(&mut view, 18, 1, &lines).contains("needle"));
        assert_eq!(view.find("absent", None, false), None);
        assert!(!view.is_following());
    }
}
