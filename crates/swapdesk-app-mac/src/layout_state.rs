//! The pointer mapping, shared between the main thread (the renderer lives there and publishes it) and whoever maps a
//! pointer position (the input lane's closure, `MacInput::set_pointer_map`).
//!
//! `MacRenderer::map_pointer` needs the renderer, which is not `Send`; what it computes is a pure function of three
//! facts: the video area in window pixels, the picture's size and the renderer's last [`Layout`]. The main thread
//! publishes those after every picture, resize and scale change ([`LayoutState::set`]) and [`LayoutState::map`] computes
//! exactly what `Renderer::map_pointer` computes (`swapdesk_render_mac::layout::map_pointer`).

use std::sync::{Mutex, MutexGuard, PoisonError};

use swapdesk_platform::Rect;
use swapdesk_render_mac::layout::{Layout, map_pointer};

/// What `Renderer::map_pointer` depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerLayout {
    /// The video rectangle in window pixels.
    pub area: Rect,
    /// The picture's size.
    pub frame: (u32, u32),
    pub layout: Layout,
}

#[derive(Default)]
pub struct LayoutState(Mutex<Option<PointerLayout>>);

impl LayoutState {
    fn lock(&self) -> MutexGuard<'_, Option<PointerLayout>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set(&self, layout: Option<PointerLayout>) {
        *self.lock() = layout;
    }

    pub fn get(&self) -> Option<PointerLayout> {
        *self.lock()
    }

    /// Window pixels to the wire's 0 to 65535 coordinates; `None` before the first picture, outside the area and in the
    /// letterbox.
    pub fn map(&self, x: i32, y: i32) -> Option<(u16, u16)> {
        let l = self.get()?;
        map_pointer(l.area, l.frame, &l.layout, x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_platform::ScaleMode;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, width: w, height: h }
    }

    #[test]
    fn it_maps_like_the_renderer_does_and_forgets_when_cleared() {
        let state = LayoutState::default();
        assert_eq!(state.map(5, 5), None, "no picture yet");
        // A 160x96 picture fitted in a 1000x1000 pixel area at the window's origin: width limited, 600 high, y offset 200.
        let area = rect(0, 0, 1000, 1000);
        let layout = Layout::compute((160, 96), (1000, 1000), ScaleMode::Fit).expect("layout");
        state.set(Some(PointerLayout { area, frame: (160, 96), layout }));
        assert_eq!(state.map(0, 200), Some((0, 0)), "the picture's corner");
        assert_eq!(state.map(999, 799), Some((65535, 65535)));
        assert_eq!(state.map(500, 100), None, "letterbox above the picture");
        assert_eq!(state.map(1000, 500), None, "outside the area");
        state.set(None);
        assert_eq!(state.map(0, 200), None);
    }
}
