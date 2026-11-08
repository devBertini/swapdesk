//! The viewer's mouse: window pixels to PROTOCOL.md 7.3 coordinates, and the records a mouse message produces.
//!
//! `WinRenderer::map_pointer` needs the renderer, which lives on the decode thread (nothing in it is `Send`). The
//! decode thread therefore publishes the three facts the mapping depends on ([`PointerMap`]: the video rectangle, the
//! picture size and display, the scale mode) and the UI thread computes the same function from them
//! (`swapdesk_render_win::layout::map_pointer` over `Layout::compute`, exactly what the renderer calls).
//!
//! Rules: a move inside the picture is a `Pointer` record when the position changed; a press inside the picture
//! first moves the pointer there, then goes as a `Button` record (a press in the letterbox or outside is ignored,
//! and so is its release); a release always goes for a button that was pressed; while a button is held the pointer is
//! clamped into the video rectangle (a drag may leave the window); a wheel turn inside the picture is a `Wheel`
//! record.

use swapdesk_platform::{Rect, ScaleMode};
use swapdesk_proto::{InputEvent, MouseButton};
use swapdesk_render_win::layout::{Layout, map_pointer};
use swapdesk_shell_win::{MouseKind, MouseMsg, PointerButton};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerMap {
    /// The video rectangle in window pixels.
    pub area: Rect,
    /// The picture's size.
    pub frame: (u32, u32),
    pub display: u16,
    pub mode: ScaleMode,
}

impl PointerMap {
    /// Window pixels to wire coordinates; `None` outside the video rectangle or in the letterbox.
    pub fn to_wire(&self, x: i32, y: i32) -> Option<(u16, u16)> {
        let layout = Layout::compute(self.frame, (self.area.width, self.area.height), self.mode)?;
        map_pointer(self.area, self.frame, &layout, x, y)
    }

    fn clamp(&self, x: i32, y: i32) -> (i32, i32) {
        let max_x = self.area.x + self.area.width.saturating_sub(1) as i32;
        let max_y = self.area.y + self.area.height.saturating_sub(1) as i32;
        (x.clamp(self.area.x, max_x), y.clamp(self.area.y, max_y))
    }
}

fn wire_button(b: PointerButton) -> MouseButton {
    match b {
        PointerButton::Left => MouseButton::Left,
        PointerButton::Right => MouseButton::Right,
        PointerButton::Middle => MouseButton::Middle,
        PointerButton::Back => MouseButton::Back,
        PointerButton::Forward => MouseButton::Forward,
    }
}

#[derive(Debug, Default)]
pub struct PointerRouter {
    /// Bit per `PointerButton` (as `u8`) pressed and sent.
    held: u8,
    at: Option<(u16, u16, u16)>,
}

impl PointerRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn any_held(&self) -> bool {
        self.held != 0
    }

    fn move_to(&mut self, map: &PointerMap, x: i32, y: i32, out: &mut Vec<InputEvent>) -> bool {
        let Some((wx, wy)) = map.to_wire(x, y) else { return false };
        if self.at != Some((map.display, wx, wy)) {
            self.at = Some((map.display, wx, wy));
            out.push(InputEvent::Pointer { display: map.display, x: wx, y: wy });
        }
        true
    }

    /// The records for one mouse message. `map` is the latest published mapping (`None` before the first picture).
    pub fn mouse(&mut self, msg: &MouseMsg, map: Option<&PointerMap>) -> Vec<InputEvent> {
        let mut out = Vec::new();
        let Some(map) = map else { return out };
        let (x, y) = if self.held != 0 { map.clamp(msg.x, msg.y) } else { (msg.x, msg.y) };
        match msg.kind {
            MouseKind::Move => {
                self.move_to(map, x, y, &mut out);
            }
            MouseKind::Button { button, down } => {
                let bit = 1u8 << (button as u8);
                if down {
                    if self.held & bit == 0 && self.move_to(map, x, y, &mut out) {
                        self.held |= bit;
                        out.push(InputEvent::Button { button: wire_button(button), down: true });
                    }
                } else if self.held & bit != 0 {
                    self.move_to(map, x, y, &mut out);
                    self.held &= !bit;
                    out.push(InputEvent::Button { button: wire_button(button), down: false });
                }
            }
            MouseKind::Wheel { dx, dy } => {
                if map.to_wire(x, y).is_some() {
                    out.push(InputEvent::Wheel { dx, dy });
                }
            }
        }
        out
    }

    /// Focus lost or the session ended: every pressed button is released on the host.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let had = self.held != 0;
        self.held = 0;
        self.at = None;
        if had { vec![InputEvent::ReleaseAll] } else { Vec::new() }
    }
}

pub use crate::shared::out::coalesce;

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, width: w, height: h }
    }

    /// A 1920x1080 picture in a 1000x1000 area at (10, 20): letterboxed top and bottom.
    fn fit_map() -> PointerMap {
        PointerMap { area: rect(10, 20, 1000, 1000), frame: (1920, 1080), display: 3, mode: ScaleMode::Fit }
    }

    fn msg(kind: MouseKind, x: i32, y: i32) -> MouseMsg {
        MouseMsg { kind, x, y, inside: true }
    }

    #[test]
    fn the_corners_of_the_picture_map_to_the_corners_of_the_wire_range() {
        let m = fit_map();
        // The picture is 1000 x 563 at y offset (1000 - 563) / 2 = 218 (the renderer's tests fix these numbers).
        assert_eq!(m.to_wire(10, 20 + 218), Some((0, 0)));
        assert_eq!(m.to_wire(10 + 999, 20 + 218 + 562), Some((65535, 65535)));
        let (x, y) = m.to_wire(10 + 500, 20 + 500).expect("the middle");
        assert!((i32::from(x) - 32768).abs() <= 70 && (i32::from(y) - 32768).abs() <= 120, "{x} {y}");
    }

    #[test]
    fn the_letterbox_and_the_outside_map_to_nothing() {
        let m = fit_map();
        assert_eq!(m.to_wire(10 + 500, 20 + 10), None, "top bar");
        assert_eq!(m.to_wire(5, 600), None, "left of the area");
        assert_eq!(m.to_wire(10 + 1000, 600), None, "right of the area");
    }

    #[test]
    fn one_to_one_maps_pixels_without_scaling() {
        let m = PointerMap { area: rect(0, 0, 800, 600), frame: (1920, 1080), display: 0, mode: ScaleMode::OneToOne };
        let (x, y) = m.to_wire(100, 50).expect("inside");
        assert_eq!(
            (x, y),
            (swapdesk_render_win::layout::normalize(100, 1920), swapdesk_render_win::layout::normalize(50, 1080))
        );
    }

    #[test]
    fn a_move_sends_a_pointer_record_only_when_the_position_changes() {
        let mut r = PointerRouter::new();
        let m = fit_map();
        let first = r.mouse(&msg(MouseKind::Move, 500, 500), Some(&m));
        assert!(matches!(first.as_slice(), [InputEvent::Pointer { display: 3, .. }]));
        assert!(r.mouse(&msg(MouseKind::Move, 500, 500), Some(&m)).is_empty());
        assert!(r.mouse(&msg(MouseKind::Move, 500, 21), Some(&m)).is_empty(), "letterbox");
        assert!(r.mouse(&msg(MouseKind::Move, 500, 500), None).is_empty(), "no picture yet");
    }

    #[test]
    fn a_press_moves_the_pointer_first_and_its_release_goes_even_outside() {
        let mut r = PointerRouter::new();
        let m = fit_map();
        let down = r.mouse(&msg(MouseKind::Button { button: PointerButton::Left, down: true }, 300, 400), Some(&m));
        assert!(matches!(
            down.as_slice(),
            [InputEvent::Pointer { .. }, InputEvent::Button { button: MouseButton::Left, down: true }]
        ));
        assert!(r.any_held());
        // Dragged out of the window to the left: clamped to the area's edge.
        let drag = r.mouse(&MouseMsg { inside: false, ..msg(MouseKind::Move, -50, 400) }, Some(&m));
        assert!(matches!(drag.as_slice(), [InputEvent::Pointer { x: 0, .. }]), "{drag:?}");
        let up = r.mouse(
            &MouseMsg {
                inside: false,
                ..msg(MouseKind::Button { button: PointerButton::Left, down: false }, -50, 400)
            },
            Some(&m),
        );
        assert!(matches!(up.as_slice(), [InputEvent::Button { button: MouseButton::Left, down: false }]));
        assert!(!r.any_held());
    }

    #[test]
    fn a_press_in_the_letterbox_is_ignored_and_so_is_its_release() {
        let mut r = PointerRouter::new();
        let m = fit_map();
        assert!(
            r.mouse(&msg(MouseKind::Button { button: PointerButton::Right, down: true }, 500, 25), Some(&m)).is_empty()
        );
        assert!(
            r.mouse(&msg(MouseKind::Button { button: PointerButton::Right, down: false }, 500, 500), Some(&m))
                .is_empty()
        );
    }

    #[test]
    fn the_wheel_goes_inside_the_picture_only_and_keeps_its_notches() {
        let mut r = PointerRouter::new();
        let m = fit_map();
        assert_eq!(
            r.mouse(&msg(MouseKind::Wheel { dx: 0, dy: -240 }, 500, 500), Some(&m)),
            vec![InputEvent::Wheel { dx: 0, dy: -240 }]
        );
        assert!(r.mouse(&msg(MouseKind::Wheel { dx: 0, dy: 120 }, 500, 25), Some(&m)).is_empty());
    }

    #[test]
    fn release_all_only_when_something_is_held() {
        let mut r = PointerRouter::new();
        assert!(r.release_all().is_empty());
        let m = fit_map();
        r.mouse(&msg(MouseKind::Button { button: PointerButton::Middle, down: true }, 500, 500), Some(&m));
        assert_eq!(r.release_all(), vec![InputEvent::ReleaseAll]);
        assert!(!r.any_held());
    }

    #[test]
    fn coalesce_keeps_the_newest_of_a_run_of_moves_and_the_order_of_the_rest() {
        let p = |x| InputEvent::Pointer { display: 0, x, y: 0 };
        let b = InputEvent::Button { button: MouseButton::Left, down: true };
        let out = coalesce(vec![p(1), p(2), p(3), b.clone(), p(4), p(5)]);
        assert_eq!(out, vec![p(3), b, p(5)]);
    }
}
