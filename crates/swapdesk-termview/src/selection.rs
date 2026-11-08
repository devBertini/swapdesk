//! A text selection over stable row ids (see `Screen::row_id`), so it stays on its text while the output scrolls.

/// A cell: `row` is a stable row id, `col` a 0-based column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub row: u64,
    pub col: usize,
}

/// The selected span, from the point where the mouse went down (`anchor`) to where it is now (`head`); the head may
/// come before the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
}

impl Selection {
    /// `(start, end)` in reading order, both included.
    pub fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    pub fn contains(&self, row: u64, col: usize) -> bool {
        let (a, b) = self.ordered();
        let p = Point { row, col };
        a <= p && p <= b
    }
}
