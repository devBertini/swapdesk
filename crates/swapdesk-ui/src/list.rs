//! R-UI-5: long lists draw only the rows in view plus a margin.

use std::ops::Range;

/// Extra rows drawn above and below the viewport so a scroll step never shows a gap.
pub const MARGIN_ROWS: usize = 4;

/// The rows of `total` to draw: those that intersect the viewport `[scroll, scroll + viewport_h)` of fixed-height
/// rows, plus [`MARGIN_ROWS`] each side.
pub fn visible_rows(total: usize, row_h: f32, scroll: f32, viewport_h: f32) -> Range<usize> {
    if total == 0 || row_h <= 0.0 {
        return 0..0;
    }
    let first = (scroll.max(0.0) / row_h).floor() as usize;
    let last = ((scroll.max(0.0) + viewport_h.max(0.0)) / row_h).ceil() as usize;
    first.saturating_sub(MARGIN_ROWS).min(total)..(last + MARGIN_ROWS).min(total)
}

/// The largest scroll offset that still fills the viewport.
pub fn max_scroll(total: usize, row_h: f32, viewport_h: f32) -> f32 {
    (total as f32 * row_h - viewport_h).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_visible_rows_and_a_margin_are_drawn() {
        let r = visible_rows(10_000, 50.0, 0.0, 400.0);
        assert_eq!(r, 0..12);
        let r = visible_rows(10_000, 50.0, 5000.0, 400.0);
        assert_eq!(r, 96..112);
        assert!(r.len() < 20, "10 000 rows draw {} rows", r.len());
    }

    #[test]
    fn edges_and_empty_lists() {
        assert_eq!(visible_rows(0, 50.0, 0.0, 400.0), 0..0);
        assert_eq!(visible_rows(5, 50.0, 0.0, 400.0), 0..5);
        assert_eq!(visible_rows(100, 50.0, 1e9, 400.0), 100..100);
        assert_eq!(max_scroll(100, 50.0, 400.0), 4600.0);
        assert_eq!(max_scroll(3, 50.0, 400.0), 0.0);
    }
}
