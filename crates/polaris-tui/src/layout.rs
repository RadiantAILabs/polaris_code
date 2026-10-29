//! Layout computation for the shell chrome and panel area.
//!
//! The shell reserves a one-line tab bar at the top and a one-line status
//! line at the bottom. The remaining area is the *workspace* — split among
//! panels according to [`LayoutMode`](crate::config::LayoutMode).

use crate::config::LayoutMode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Result of [`compute_chrome`].
pub(crate) struct Chrome {
    /// Top tab bar (1 row).
    pub tabs: Rect,
    /// Main workspace where panel(s) render.
    pub workspace: Rect,
    /// Bottom status line (1 row).
    pub status: Rect,
}

/// Splits the full terminal area into the shell's chrome.
pub(crate) fn compute_chrome(area: Rect) -> Chrome {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    Chrome {
        tabs: chunks[0],
        workspace: chunks[1],
        status: chunks[2],
    }
}

/// Computes per-panel rectangles for the workspace given a layout mode.
///
/// For [`LayoutMode::Single`] only the focused panel is laid out; other
/// slots are empty rectangles the caller is expected to skip. For
/// [`LayoutMode::Tiled`] every panel gets a slice. The split is column-major
/// up to four panels, then 2×N grid.
pub(crate) fn compute_panels(
    area: Rect,
    count: usize,
    focused: usize,
    mode: LayoutMode,
) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    match mode {
        LayoutMode::Single => {
            let mut out = vec![Rect::default(); count];
            if focused < count {
                out[focused] = area;
            }
            out
        }
        LayoutMode::Tiled => tile(area, count),
    }
}

fn tile(area: Rect, count: usize) -> Vec<Rect> {
    debug_assert!(count > 0);
    match count {
        1 => vec![area],
        2 => Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
            .to_vec(),
        3 => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(cols[1]);
            vec![cols[0], right[0], right[1]]
        }
        n => {
            // 2-column grid; rows balanced.
            let rows = n.div_ceil(2);
            let row_constraints: Vec<Constraint> = (0..rows)
                .map(|_| Constraint::Ratio(1, rows as u32))
                .collect();
            let row_rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(row_constraints)
                .split(area);
            let mut out = Vec::with_capacity(n);
            for (row_idx, row_rect) in row_rects.iter().enumerate() {
                let in_row = (n - row_idx * 2).min(2);
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(vec![Constraint::Ratio(1, in_row as u32); in_row])
                    .split(*row_rect);
                out.extend_from_slice(&cols);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn no_panels_yields_no_rects() {
        assert!(compute_panels(area(), 0, 0, LayoutMode::Tiled).is_empty());
        assert!(compute_panels(area(), 0, 0, LayoutMode::Single).is_empty());
    }

    #[test]
    fn single_mode_lays_out_only_the_focused_panel() {
        let rects = compute_panels(area(), 3, 1, LayoutMode::Single);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[1], area());
        // Non-focused slots are empty so the caller skips them.
        assert_eq!(rects[0], Rect::default());
        assert_eq!(rects[2], Rect::default());
    }

    #[test]
    fn single_mode_out_of_range_focus_lays_out_nothing() {
        let rects = compute_panels(area(), 2, 5, LayoutMode::Single);
        assert_eq!(rects.len(), 2);
        assert!(rects.iter().all(|r| *r == Rect::default()));
    }

    #[test]
    fn tiled_returns_one_rect_per_panel_and_all_are_visible() {
        // 1, 2, 3 are special-cased; n>=4 takes the grid path. Check each
        // returns exactly `count` non-empty rectangles.
        for count in 1..=6 {
            let rects = compute_panels(area(), count, 0, LayoutMode::Tiled);
            assert_eq!(rects.len(), count, "count={count}");
            assert!(
                rects.iter().all(|r| r.width > 0 && r.height > 0),
                "count={count} produced an empty rect: {rects:?}"
            );
        }
    }

    #[test]
    fn tiled_two_panels_split_width_in_half() {
        let rects = compute_panels(area(), 2, 0, LayoutMode::Tiled);
        assert_eq!(rects.len(), 2);
        // Side by side, covering the full width with no gap.
        assert_eq!(rects[0].x, 0);
        assert_eq!(rects[1].x, rects[0].width);
        assert_eq!(rects[0].width + rects[1].width, 80);
        assert_eq!(rects[0].height, 24);
    }

    #[test]
    fn tiled_five_panels_use_three_rows_with_a_short_last_row() {
        // 5 panels → 3 rows: [2, 2, 1]. The lone panel in the last row spans
        // the full width (in_row == 1).
        let rects = compute_panels(area(), 5, 0, LayoutMode::Tiled);
        assert_eq!(rects.len(), 5);
        assert_eq!(rects[4].width, 80);
    }
}
