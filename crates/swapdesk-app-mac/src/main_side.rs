//! What belongs to the main thread: the `MacRenderer` and every edit of the video layer tree.
//!
//! [`MainSide`] is built when the shell's viewer window exists (`VideoSlot::attached`), kept in a thread-local of the main
//! thread ([`crate::slot`]) and fed from three places, all on the main thread: the shell (resize, backing-scale change),
//! the UI backend's commands (the scale mode) and the decode thread's [`Handoff`] (pictures and cursor updates).
//!
//! Units: the shell's `VideoArea` is in points; the renderer takes window pixels (points times the backing scale). The
//! conversion is [`pixel_area`]. After every change that moves the picture the layout is published to the shared
//! [`LayoutState`] the pointer map reads.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use objc2::rc::Retained;
use objc2_app_kit::NSView;
use swapdesk_app::shared::cursor::shown as cursor_shown;
use swapdesk_platform::{CursorShape, CursorUpdate, DecodedFrame, ErrorKind, NativeSurface, Rect, Renderer, ScaleMode};
use swapdesk_render_mac::{Counters, MacRenderer, RendererOptions};
use swapdesk_shell_mac::{VideoArea, ViewCursor, set_view_cursor};

use crate::decode::Shared;
use crate::handoff::{Drained, Handoff};
use crate::layout_state::{LayoutState, PointerLayout};

/// A decoded picture on its way to the main thread.
pub struct MainFrame {
    pub frame: DecodedFrame,
    pub decode_us: u32,
    pub arrived: Instant,
}

impl MainFrame {
    pub fn new(frame: DecodedFrame, decode_us: u32, arrived: Instant) -> Self {
        Self { frame, decode_us, arrived }
    }
}

// SAFETY: a `DecodedFrame` of this app is `Pixels::Gpu` holding a `CFRetained<CVPixelBuffer>` made by `VtH264Decoder`.
// CoreVideo buffers are reference-counted Core Foundation objects whose retain and release are atomic, and VideoToolbox
// hands them out for use on any thread. The frame is moved to the main thread whole, never used on two threads at once,
// and dropped there or on the decode thread. (`DecodedFrame` is not `Send` only because `GpuSurface` is a type-erased
// `Box<dyn Any>`.) A CPU picture (`Pixels::Cpu`) is plain bytes.
unsafe impl Send for MainFrame {}

/// The video area of the shell (points) in window pixels: every number is `points * scale`, rounded.
pub fn pixel_area(a: &VideoArea) -> Rect {
    let px = |v: f64| (v * a.scale).round();
    let size = |v: f64| px(v).max(0.0) as u32;
    Rect { x: px(a.x) as i32, y: px(a.y) as i32, width: size(a.w), height: size(a.h) }
}

/// What a picture that reached the screen reports (tests, traces).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentEvent {
    pub frame_id: u32,
    pub display: u16,
    pub width: u32,
    pub height: u32,
    pub decode_us: u32,
    /// Arrival in the viewer to the end of `present`.
    pub lag_us: u32,
}

pub type PresentObserver = Arc<dyn Fn(&PresentEvent) + Send + Sync>;

pub struct MainSide {
    renderer: MacRenderer,
    /// The shell's video view, to read its size again after layout; `None` in tests that attach to a bare layer.
    view: Option<Retained<NSView>>,
    area: Rect,
    backing: f64,
    scale: ScaleMode,
    frame: Option<(u32, u32)>,
    display: u16,
    shared: Arc<Shared>,
    layout: Arc<LayoutState>,
    handoff: Arc<Handoff<MainFrame>>,
    observer: Option<PresentObserver>,
    /// The last cursor update applied, without its shape, and whether it was drawn: applied again when the local
    /// pointer switch flips.
    last_cursor: Option<(CursorUpdate, bool)>,
    /// The local pointer switch the renderer's cursor was last set under.
    local_applied: bool,
    /// The host's current cursor shape, for the local pointer.
    shape: Option<CursorShape>,
    /// What the video view's pointer was last set to: the shape id and the backing scale it was built for, or `None`
    /// for the system arrow.
    view_cursor: Option<(u32, u64)>,
}

impl MainSide {
    /// Adds the video container to the layer behind `parent_layer` (the pointer of `VideoSurface::parent_layer_ptr`)
    /// at pixel `area`.
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        parent_layer: u64,
        view: Option<Retained<NSView>>,
        area: Rect,
        backing: f64,
        scale: ScaleMode,
        shared: Arc<Shared>,
        layout: Arc<LayoutState>,
        handoff: Arc<Handoff<MainFrame>>,
        observer: Option<PresentObserver>,
    ) -> Result<Self, String> {
        let mut renderer = MacRenderer::new(RendererOptions { backing_scale: Some(backing) });
        renderer.attach(NativeSurface::MacOs { parent_layer }, area).map_err(|e| format!("renderer attach: {e:?}"))?;
        let mut side = Self {
            renderer,
            view,
            area,
            backing,
            scale,
            frame: None,
            display: 0,
            shared,
            layout,
            handoff,
            observer,
            last_cursor: None,
            local_applied: false,
            shape: None,
            view_cursor: None,
        };
        let _ = side.renderer.set_scale(scale);
        // Anything the decode thread queued before the window was up is not for this renderer; the wake is re-armed.
        side.handoff.clear();
        side.publish();
        Ok(side)
    }

    pub fn counters(&self) -> Counters {
        self.renderer.counters()
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn backing(&self) -> f64 {
        self.backing
    }

    pub fn scale_mode(&self) -> ScaleMode {
        self.scale
    }

    /// The video rectangle and the screen's scale changed (shell: resize, screen change).
    pub fn resized(&mut self, area: Rect, backing: f64) {
        if backing.is_finite() && backing > 0.0 && (backing - self.backing).abs() > f64::EPSILON {
            self.backing = backing;
            if let Err(e) = self.renderer.set_backing_scale(backing) {
                self.shared.error("backing scale", format!("{e:?}"));
            }
        }
        self.area = area;
        if let Err(e) = self.renderer.resize(area) {
            self.shared.error("resize", format!("{e:?}"));
        }
        self.publish();
    }

    /// Reads the video view's size and screen scale again and applies them: the shell gives the first area before the
    /// window is laid out, so the app asks once more after the run loop turned. Returns the scale applied.
    pub fn relayout_from_view(&mut self) -> Option<f64> {
        let view = self.view.as_ref()?;
        let b = view.bounds();
        let scale = view.window().map_or(self.backing, |w| w.backingScaleFactor());
        let area = pixel_area(&VideoArea { x: b.origin.x, y: b.origin.y, w: b.size.width, h: b.size.height, scale });
        if area != self.area || (scale - self.backing).abs() > f64::EPSILON {
            self.resized(area, scale);
        }
        Some(self.backing)
    }

    /// Fit, 1:1 or scroll: the last picture is drawn again under the new mode.
    pub fn set_scale(&mut self, mode: ScaleMode) {
        self.scale = mode;
        if let Err(e) = self.renderer.set_scale(mode) {
            self.shared.error("scale", format!("{e:?}"));
        }
        self.publish();
    }

    /// Takes what the decode thread queued: the newest picture, then the cursor updates in order.
    pub fn drain(&mut self) {
        let drained = self.handoff.drain();
        self.apply(drained);
    }

    pub fn apply(&mut self, drained: Drained<MainFrame>) {
        if drained.replaced > 0 {
            // Decoded and never drawn: late for the screen, not for the stream.
            self.shared.lost.fetch_add(drained.replaced, Ordering::Relaxed);
        }
        if let Some(f) = drained.frame {
            self.present(f);
        }
        let local = self.shared.local_pointer.load(Ordering::SeqCst);
        let flipped = local != self.local_applied;
        self.local_applied = local;
        let had_items = !drained.cursors.is_empty();
        for item in drained.cursors {
            self.set_cursor(&item.update, item.drawn, local);
            self.last_cursor = Some((CursorUpdate { shape: None, ..item.update }, item.drawn));
        }
        if flipped
            && !had_items
            && let Some((last, drawn)) = self.last_cursor.clone()
        {
            self.set_cursor(&last, drawn, local);
        }
    }

    fn set_cursor(&mut self, update: &CursorUpdate, drawn: bool, local: bool) {
        if let Some(s) = &update.shape {
            self.shape = Some(s.clone());
        }
        let shown = cursor_shown(update, local);
        match self.renderer.set_cursor(&shown) {
            Ok(()) => self.shared.cursor_drawn.store(drawn && !local, Ordering::SeqCst),
            Err(e) => self.shared.error("cursor", format!("{e:?}")),
        }
        self.apply_view_cursor(local);
    }

    /// While the viewer controls, the pointer over the video takes the host's shape (at its pixel size, as the drawn
    /// arrow had it); otherwise the system arrow.
    fn apply_view_cursor(&mut self, local: bool) {
        let want = if local { self.shape.as_ref().map(|s| (s.id, self.backing.to_bits())) } else { None };
        if want == self.view_cursor {
            return;
        }
        let Some(view) = self.view.clone() else { return };
        match (want, &self.shape) {
            (Some(_), Some(shape)) => match swapdesk_render_mac::cursor_image(shape) {
                Ok(image) => {
                    let b = self.backing;
                    *self.shared.cursor_note.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format!(
                        "shape {}x{} hot {},{} backing {b} view {:.1}x{:.1} hot {:.1},{:.1}",
                        shape.width,
                        shape.height,
                        shape.hot_x,
                        shape.hot_y,
                        f64::from(shape.width) / b,
                        f64::from(shape.height) / b,
                        f64::from(shape.hot_x) / b,
                        f64::from(shape.hot_y) / b
                    ));
                    set_view_cursor(
                        &view,
                        ViewCursor::Image {
                            image: &image,
                            size: (f64::from(shape.width) / b, f64::from(shape.height) / b),
                            hot: (f64::from(shape.hot_x) / b, f64::from(shape.hot_y) / b),
                        },
                    );
                }
                Err(e) => {
                    self.shared.error("cursor", format!("{e:?}"));
                    set_view_cursor(&view, ViewCursor::System);
                }
            },
            _ => set_view_cursor(&view, ViewCursor::System),
        }
        self.view_cursor = want;
    }

    fn present(&mut self, f: MainFrame) {
        let MainFrame { frame, decode_us, arrived } = f;
        let (width, height, frame_id, display) = (frame.width, frame.height, frame.frame_id, frame.display.0);
        match self.renderer.present(frame, self.scale) {
            Ok(_) => {
                self.frame = Some((width, height));
                self.display = display;
                self.shared.presented.fetch_add(1, Ordering::Relaxed);
                self.shared.last_frame_id.store(u64::from(frame_id), Ordering::Relaxed);
                self.publish();
                if let Some(obs) = &self.observer {
                    let lag_us = u32::try_from(arrived.elapsed().as_micros()).unwrap_or(u32::MAX);
                    obs(&PresentEvent { frame_id, display, width, height, decode_us, lag_us });
                }
            }
            Err(e) => {
                self.shared.lost.fetch_add(1, Ordering::Relaxed);
                self.shared.error("present", format!("{e:?}"));
                if e.kind == ErrorKind::Unavailable {
                    self.shared.error("present", "the renderer was lost");
                }
            }
        }
    }

    fn publish(&self) {
        let layout = match (self.renderer.layout(), self.frame) {
            (Some(layout), Some(frame)) => Some(PointerLayout { area: self.area, frame, layout }),
            _ => None,
        };
        self.layout.set(layout);
        *self.shared.geometry_note.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            self.renderer.geometry_note();
    }

    /// The display of the last picture drawn (0 before the first).
    pub fn display(&self) -> u16 {
        self.display
    }
}

impl Drop for MainSide {
    fn drop(&mut self) {
        if let Some(view) = &self.view {
            set_view_cursor(view, ViewCursor::System);
        }
        self.layout.set(None);
        self.shared.cursor_drawn.store(false, Ordering::SeqCst);
        self.renderer.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_become_window_pixels_at_the_screens_scale() {
        let a = VideoArea { x: 0.0, y: 0.0, w: 640.5, h: 400.0, scale: 2.0 };
        assert_eq!(pixel_area(&a), Rect { x: 0, y: 0, width: 1281, height: 800 });
        let b = VideoArea { x: 10.25, y: 4.0, w: 100.0, h: 50.0, scale: 1.0 };
        assert_eq!(pixel_area(&b), Rect { x: 10, y: 4, width: 100, height: 50 });
        let neg = VideoArea { x: 0.0, y: 0.0, w: -3.0, h: 2.0, scale: 2.0 };
        assert_eq!(pixel_area(&neg).width, 0, "a negative size is an empty area, not a wrap");
    }
}
