//! The video host view: the slot the render lane fills and the place the input lane's events come from.
//!
//! [`VideoView`] is a flipped, layer-backed `NSView` that fills the viewer window. The render lane gets its layer
//! through [`VideoSlot`] (it adds an `AVSampleBufferDisplayLayer` under it; CONTRACTS `NativeSurface::MacOs`), the input
//! lane gets key, modifier, mouse and focus events through `ViewerInput` (translated from `NSEvent` here).
//!
//! Apple documentation: `NSView` / `NSResponder` event methods (developer.apple.com/documentation/appkit/nsresponder),
//! `NSEvent` (developer.apple.com/documentation/appkit/nsevent: `keyCode`, `characters`, `modifierFlags`,
//! `isARepeat`, `locationInWindow`, `buttonNumber`, `scrollingDeltaX`, `hasPreciseScrollingDeltas`),
//! `NSView.performKeyEquivalent(with:)`, `NSView.isFlipped`; objc2 `define_class!` (docs.rs/objc2/0.6.5/objc2/macro.define_class.html).

use std::cell::RefCell;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::{AnyThread, ClassType, DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSCursor, NSEvent, NSEventModifierFlags, NSImage, NSView};
use objc2_core_graphics::CGImage;
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize};
use objc2_quartz_core::CALayer;

use crate::input::{KeyMsg, Modifiers, MouseKind, MouseMsg, PointerButton, ViewerInput};
use crate::model::VideoArea;

/// What the render lane receives when the viewer window is up: the host view and its root layer. The view lives as
/// long as the window; `VideoSlot::detached` is called before it goes.
pub struct VideoSurface {
    pub view: Retained<NSView>,
    pub layer: Retained<CALayer>,
    pub area: VideoArea,
}

impl VideoSurface {
    /// The parent `CALayer` pointer for `swapdesk_platform::NativeSurface::MacOs { parent_layer }`.
    pub fn parent_layer_ptr(&self) -> u64 {
        Retained::as_ptr(&self.layer) as usize as u64
    }
}

/// Implemented by the render lane (`swapdesk-render-mac`). Called on the main thread; return quickly.
pub trait VideoSlot: Send + Sync {
    /// The viewer window exists and was laid out: add the video layer under `surface.layer`.
    fn attached(&self, surface: &VideoSurface);
    /// The video rectangle or the screen's scale changed.
    fn resized(&self, area: VideoArea);
    /// The window is closing: detach everything built on `attached` before returning.
    fn detached(&self);
}

#[derive(Default)]
pub struct VideoIvars {
    input: RefCell<Option<Arc<dyn ViewerInput>>>,
    /// The pointer over the video: the host's shape while the viewer controls, else the system arrow (`None`).
    cursor: RefCell<Option<Retained<NSCursor>>>,
}

/// What the pointer looks like over the video view.
pub enum ViewCursor<'a> {
    /// The system arrow (the viewer does not control: the remote arrow is drawn in the picture).
    System,
    /// The host's cursor shape: its size and hot spot in points, the hot spot from the top-left corner.
    Image { image: &'a CGImage, size: (f64, f64), hot: (f64, f64) },
}

/// Sets the pointer of the video view behind `view` (the `VideoSurface::view`); any other view is left alone.
pub fn set_view_cursor(view: &NSView, cursor: ViewCursor<'_>) {
    if !view.isKindOfClass(VideoView::class()) {
        return;
    }
    // SAFETY: the object is a `VideoView` (checked above), whose layout starts with its `NSView` superclass.
    let video = unsafe { &*(view as *const NSView).cast::<VideoView>() };
    let ns = match cursor {
        ViewCursor::System => None,
        ViewCursor::Image { image, size, hot } => {
            let picture = NSImage::initWithCGImage_size(NSImage::alloc(), image, NSSize::new(size.0, size.1));
            Some(NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &picture, NSPoint::new(hot.0, hot.1)))
        }
    };
    video.set_cursor(ns);
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond calling super where AppKit expects it; this class does not
    // implement `Drop`.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskVideoView"]
    #[ivars = VideoIvars]
    pub struct VideoView;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for VideoView {}

    impl VideoView {
        // SAFETY (all methods below): the selectors and signatures are those of NSView / NSResponder.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            if let Some(c) = self.ivars().cursor.borrow().as_ref() {
                self.addCursorRect_cursor(self.bounds(), c);
            }
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            if let Some(i) = self.input() {
                i.focus(true);
            }
            true
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            if let Some(i) = self.input() {
                i.focus(false);
            }
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.key(event, true) {
                // SAFETY: forwards the same event to NSView's implementation.
                unsafe { msg_send![super(self), keyDown: event] }
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            if !self.key(event, false) {
                // SAFETY: forwards the same event to NSView's implementation.
                unsafe { msg_send![super(self), keyUp: event] }
            }
        }

        // While the pipeline captures the keyboard, shortcuts (Cmd+Q excepted by the grabber lane) reach it before the menu.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            match self.input() {
                Some(i) if i.captures_keyboard() => self.key(event, true),
                _ => false,
            }
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let taken = self.input().is_some_and(|i| i.modifiers_changed(modifiers_of(event.modifierFlags())));
            if !taken {
                // SAFETY: forwards the same event to NSView's implementation.
                unsafe { msg_send![super(self), flagsChanged: event] }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(w) = self.window() {
                w.makeFirstResponder(Some(self));
            }
            self.button(event, PointerButton::Left, true);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.button(event, PointerButton::Left, false);
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.button(event, PointerButton::Right, true);
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.button(event, PointerButton::Right, false);
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.button(event, other_button(event), true);
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            self.button(event, other_button(event), false);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.mouse(event, MouseKind::Move);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.mouse(event, MouseKind::Drag { button: PointerButton::Left });
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.mouse(event, MouseKind::Drag { button: PointerButton::Right });
        }
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.mouse(event, MouseKind::Drag { button: other_button(event) });
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.mouse(
                event,
                MouseKind::Scroll { dx: event.scrollingDeltaX(), dy: event.scrollingDeltaY(), precise: event.hasPreciseScrollingDeltas() },
            );
        }
    }
);

fn other_button(event: &NSEvent) -> PointerButton {
    match event.buttonNumber() {
        2 => PointerButton::Middle,
        n => PointerButton::Other(n.clamp(3, 255) as u8),
    }
}

fn modifiers_of(flags: NSEventModifierFlags) -> Modifiers {
    let mut m = Modifiers::default();
    for (flag, bit) in [
        (NSEventModifierFlags::Shift, Modifiers::SHIFT),
        (NSEventModifierFlags::Control, Modifiers::CONTROL),
        (NSEventModifierFlags::Option, Modifiers::OPTION),
        (NSEventModifierFlags::Command, Modifiers::COMMAND),
        (NSEventModifierFlags::CapsLock, Modifiers::CAPS_LOCK),
        (NSEventModifierFlags::Function, Modifiers::FUNCTION),
    ] {
        if flags.contains(flag) {
            m = m.union(bit);
        }
    }
    m
}

impl VideoView {
    pub fn create(mtm: MainThreadMarker, frame: NSRect) -> Retained<VideoView> {
        let this = Self::alloc(mtm).set_ivars(VideoIvars::default());
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let view: Retained<VideoView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setWantsLayer(true);
        view
    }

    pub fn set_input(&self, input: Option<Arc<dyn ViewerInput>>) {
        *self.ivars().input.borrow_mut() = input;
    }

    fn input(&self) -> Option<Arc<dyn ViewerInput>> {
        self.ivars().input.borrow().clone()
    }

    fn set_cursor(&self, cursor: Option<Retained<NSCursor>>) {
        let now = cursor.clone();
        *self.ivars().cursor.borrow_mut() = cursor;
        let Some(window) = self.window() else { return };
        window.invalidateCursorRectsForView(self);
        // Cursor rects apply on the next move or entry; a pointer already over the video takes the shape now.
        let p = self.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
        let b = self.bounds();
        if p.x >= b.origin.x && p.y >= b.origin.y && p.x < b.origin.x + b.size.width && p.y < b.origin.y + b.size.height
        {
            now.unwrap_or_else(NSCursor::arrowCursor).set();
        }
    }

    /// True when the input lane took the key.
    fn key(&self, event: &NSEvent, down: bool) -> bool {
        let Some(input) = self.input() else { return false };
        let text = if down { event.characters().map(|s| s.to_string()).filter(|s| !s.is_empty()) } else { None };
        let msg = KeyMsg {
            key_code: event.keyCode(),
            down,
            repeat: down && event.isARepeat(),
            text,
            modifiers: modifiers_of(event.modifierFlags()),
        };
        input.key(&msg)
    }

    fn point(&self, event: &NSEvent) -> NSPoint {
        self.convertPoint_fromView(event.locationInWindow(), None)
    }

    fn mouse(&self, event: &NSEvent, kind: MouseKind) -> bool {
        let Some(input) = self.input() else { return false };
        let p = self.point(event);
        input.mouse(MouseMsg { kind, x: p.x, y: p.y, modifiers: modifiers_of(event.modifierFlags()) })
    }

    fn button(&self, event: &NSEvent, button: PointerButton, down: bool) {
        self.mouse(event, MouseKind::Button { button, down });
    }

    /// The area the video fills: the whole view, in points, plus the screen's scale.
    pub fn area(&self) -> VideoArea {
        let b = self.bounds();
        let scale = self.window().map_or(1.0, |w| w.backingScaleFactor());
        VideoArea { x: b.origin.x, y: b.origin.y, w: b.size.width, h: b.size.height, scale }
    }
}
