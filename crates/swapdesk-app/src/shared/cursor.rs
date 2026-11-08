//! The remote cursor: the service's `CursorPosition` (0 to 65535 across the display, a shape id) and `CursorShape`
//! records become the renderer's `CursorUpdate` (hot-spot pixel inside the display, the shape when it changed).
//!
//! Positions are ordered by `seq` (wrapping): an older or repeated one is dropped. A shape may arrive after the
//! position that names it; the position is applied again when it does. Nothing is drawn before the first picture
//! (the display size is not known).

use std::collections::HashMap;

use swapdesk_platform::{CursorShape, CursorUpdate, DisplayId};
use swapdesk_proto::CursorPosition;

/// Shapes kept by id (the host sends each id once).
const MAX_KEPT: usize = 32;

#[derive(Default)]
pub struct CursorTracker {
    shapes: HashMap<u32, CursorShape>,
    applied_shape: Option<u32>,
    last: Option<CursorPosition>,
}

/// `round(v * (size - 1) / 65535)`: the inverse of the wire normalization.
pub fn denormalize(v: u16, size: u32) -> i32 {
    if size <= 1 {
        return 0;
    }
    ((u64::from(v) * u64::from(size - 1) + 32767) / 65535) as i32
}

impl CursorTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_shape(&mut self, shape: CursorShape) {
        if self.shapes.len() >= MAX_KEPT && !self.shapes.contains_key(&shape.id) {
            self.shapes.clear();
            self.applied_shape = None;
        }
        // A re-sent id may carry a new image: apply it again.
        if self.applied_shape == Some(shape.id) {
            self.applied_shape = None;
        }
        self.shapes.insert(shape.id, shape);
    }

    /// A new position. `None` when it is older than the last one or the frame size is not known yet.
    pub fn on_position(&mut self, pos: &CursorPosition, frame: Option<(u32, u32)>) -> Option<CursorUpdate> {
        if let Some(last) = &self.last
            && (pos.seq.wrapping_sub(last.seq) as i32) <= 0
        {
            return None;
        }
        self.last = Some(*pos);
        self.build(frame)
    }

    /// The last position again (a shape arrived, or the picture size became known).
    pub fn refresh(&mut self, frame: Option<(u32, u32)>) -> Option<CursorUpdate> {
        self.build(frame)
    }

    fn build(&mut self, frame: Option<(u32, u32)>) -> Option<CursorUpdate> {
        let pos = self.last?;
        let (w, h) = frame?;
        let shape = if self.applied_shape != Some(pos.shape_id) {
            self.shapes.get(&pos.shape_id).cloned().inspect(|_| self.applied_shape = Some(pos.shape_id))
        } else {
            None
        };
        Some(CursorUpdate {
            display: DisplayId(pos.display),
            x: denormalize(pos.x, w),
            y: denormalize(pos.y, h),
            visible: pos.visible,
            shape,
        })
    }

    /// True when the shape the host's cursor uses is known (the local pointer may be hidden then).
    pub fn has_shape(&self) -> bool {
        self.applied_shape.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(seq: u32, x: u16, shape_id: u32) -> CursorPosition {
        CursorPosition { visible: true, display: 0, seq, x, y: 65535, shape_id }
    }

    fn shape(id: u32) -> CursorShape {
        CursorShape { id, width: 1, height: 1, hot_x: 0, hot_y: 0, rgba: vec![0; 4] }
    }

    #[test]
    fn positions_denormalize_to_display_pixels() {
        assert_eq!(denormalize(0, 1920), 0);
        assert_eq!(denormalize(65535, 1920), 1919);
        assert_eq!(denormalize(32768, 1921), 960);
        assert_eq!(denormalize(5, 1), 0);
    }

    #[test]
    fn nothing_is_drawn_before_the_first_picture() {
        let mut t = CursorTracker::new();
        assert!(t.on_position(&pos(1, 0, 0), None).is_none());
        let u = t.refresh(Some((100, 50))).expect("the position is kept");
        assert_eq!((u.x, u.y), (0, 49));
    }

    #[test]
    fn older_and_repeated_positions_are_dropped_across_the_wrap() {
        let mut t = CursorTracker::new();
        assert!(t.on_position(&pos(u32::MAX, 0, 0), Some((10, 10))).is_some());
        assert!(t.on_position(&pos(u32::MAX, 0, 0), Some((10, 10))).is_none());
        assert!(t.on_position(&pos(0, 0, 0), Some((10, 10))).is_some(), "0 follows u32::MAX");
        assert!(t.on_position(&pos(u32::MAX - 1, 0, 0), Some((10, 10))).is_none());
    }

    #[test]
    fn a_shape_is_handed_over_once_and_late_shapes_are_applied_on_refresh() {
        let mut t = CursorTracker::new();
        let first = t.on_position(&pos(1, 0, 7), Some((10, 10))).unwrap();
        assert!(first.shape.is_none(), "shape 7 is not known yet");
        assert!(!t.has_shape());
        t.on_shape(shape(7));
        let again = t.refresh(Some((10, 10))).unwrap();
        assert_eq!(again.shape.as_ref().map(|s| s.id), Some(7));
        assert!(t.has_shape());
        let next = t.on_position(&pos(2, 100, 7), Some((10, 10))).unwrap();
        assert!(next.shape.is_none(), "already applied");
        t.on_shape(shape(8));
        assert_eq!(t.on_position(&pos(3, 0, 8), Some((10, 10))).unwrap().shape.map(|s| s.id), Some(8));
    }
}
