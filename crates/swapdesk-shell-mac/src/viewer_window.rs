//! The viewer window: the shared viewer screen (`swapdesk-paint`: top bar, toolbar, drawer, cards) painted by a
//! canvas, with the video view under it. The frame leaves the video rectangle unpainted (`Frame::video_under`), the
//! canvas lets the mouse through there, and the video view is kept on that rectangle after every paint.
//!
//! Apple documentation: `NSView.addSubview(_:)` (later subviews are drawn above earlier ones),
//! `NSView.autoresizingMask`, `NSWindow.acceptsMouseMovedEvents`, `NSWindow.backingScaleFactor`
//! (developer.apple.com/documentation/appkit/nswindow).

use std::cell::Cell;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::MainThreadMarker;
use swapdesk_ui::WindowKind;

use crate::appkit::{PaintedView, frame, ns, size, view_of};
use crate::canvas::CanvasView;
use crate::model::VideoArea;
use crate::video::{VideoSurface, VideoView};

pub struct ViewerWindow {
    pub window: Retained<NSWindow>,
    pub video: Retained<VideoView>,
    pub canvas: Retained<CanvasView>,
    /// The content view: the video view below, the canvas above.
    pub content: Retained<NSView>,
    /// The area the render lane was last told.
    area: Cell<Option<VideoArea>>,
}

impl ViewerWindow {
    pub fn create(mtm: MainThreadMarker, delegate: &ProtocolObject<dyn NSWindowDelegate>, title: &str) -> ViewerWindow {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // The window opens at the design's size, like the Windows viewer.
        let (w, h) = swapdesk_paint::design_size(WindowKind::Viewer);
        let bounds = frame(0.0, 0.0, f64::from(w), f64::from(h));
        // SAFETY: the window is kept in `Shell` and `setReleasedWhenClosed(false)` is set below, as AppKit requires for
        // windows created outside a window controller.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                bounds,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: see above.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns(title));
        window.setContentMinSize(size(640.0, 400.0));
        // Pointer moves reach the video view through the canvas's tracking area (`CanvasView::pass_move_to_video`),
        // not through the first responder: the window does not ask for mouse-moved events.
        window.setAcceptsMouseMovedEvents(false);
        window.setDelegate(Some(delegate));

        // A flipped container, so the video view's frame is the frame's rectangle as it is (y down). It is filled with
        // the colour of the remote screen's placeholder: that is what shows in the video rectangle until the first
        // picture arrives.
        let content = view_of(&PaintedView::create(mtm, bounds, Some(swapdesk_ui::tokens::fixed::REMOTE_SCREEN)));
        let video = VideoView::create(mtm, bounds);
        let canvas = CanvasView::create(mtm, bounds, WindowKind::Viewer);
        canvas.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        content.addSubview(&video);
        content.addSubview(&canvas);
        canvas.set_keyboard_home(&video);
        window.setContentView(Some(&content));
        window.center();
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&video));
        ViewerWindow { window, video, canvas, content, area: Cell::new(None) }
    }

    /// F-VIEW-06: what the host should encode at: the video view's size in physical pixels (0x0 while the view is
    /// hidden: the Arquivos view) and the refresh rate of the screen the window is on (0 when unknown).
    pub fn view_report(&self) -> (u32, u32, u16) {
        let hz = self.window.screen().map_or(0, |s| u16::try_from(s.maximumFramesPerSecond()).unwrap_or(0));
        let (w, h) = if self.video.isHidden() { (0, 0) } else { self.video.area().physical() };
        (w, h, hz)
    }

    pub fn surface(&self) -> Option<VideoSurface> {
        let layer = self.video.layer()?;
        Some(VideoSurface { view: view_of(&self.video), layer, area: self.video.area() })
    }

    /// Puts the video view on the rectangle the last paint left for it, snapped to device pixels as the painter
    /// snaps the fills around it (no seam, no overlap). `Some(area)` when the area changed since the last call.
    pub fn place_video(&self) -> Option<VideoArea> {
        // The Arquivos view leaves no video rectangle: the video view hides (nothing is torn down, so going back is
        // instant) and gives the keyboard to the canvas, so no key reaches the other computer meanwhile.
        let Some(r) = self.canvas.video_rect() else {
            if !self.video.isHidden() {
                self.video.setHidden(true);
                let video_has_keys = self.window.firstResponder().is_some_and(|f| {
                    std::ptr::eq(
                        objc2::rc::Retained::as_ptr(&f).cast::<()>(),
                        objc2::rc::Retained::as_ptr(&self.video).cast::<()>(),
                    )
                });
                if video_has_keys {
                    self.window.makeFirstResponder(Some(&self.canvas));
                }
            }
            return None;
        };
        if self.video.isHidden() {
            self.video.setHidden(false);
        }
        let scale = self.window.backingScaleFactor();
        let snap = |v: f32| (f64::from(v) * scale).round() / scale;
        let (left, top, right, bottom) = (snap(r.x), snap(r.y), snap(r.right()), snap(r.bottom()));
        let wanted = frame(left, top, right - left, bottom - top);
        if self.video.frame() != wanted {
            self.video.setFrame(wanted);
        }
        let area = self.video.area();
        (self.area.replace(Some(area)) != Some(area)).then_some(area)
    }
}
