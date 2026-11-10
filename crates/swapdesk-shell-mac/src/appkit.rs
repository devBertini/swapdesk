//! Small AppKit helpers the windows share: a flipped view with an optional fill, and conversions from the design
//! tokens and plain numbers to AppKit's types.
//!
//! Apple documentation: `NSView.isFlipped` and `draw(_:)` (developer.apple.com/documentation/appkit/nsview),
//! `NSRectFill` (developer.apple.com/documentation/appkit/1473652-nsrectfill),
//! `NSColor(srgbRed:green:blue:alpha:)` (developer.apple.com/documentation/appkit/nscolor).

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{NSColor, NSRectFill, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use swapdesk_ui::tokens::Color;

#[derive(Default)]
pub struct PaintedIvars {
    fill: Cell<Option<(f64, f64, f64, f64)>>,
}

define_class!(
    // SAFETY: NSView subclass overriding only `isFlipped` and `drawRect:`; no `Drop`.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskPaintedView"]
    #[ivars = PaintedIvars]
    pub struct PaintedView;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for PaintedView {}

    impl PaintedView {
        // SAFETY: the selector and signature are NSView's.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: the selector and signature are NSView's; `NSRectFill` is called inside `drawRect:`, where a graphics
        // context is current.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            if let Some((r, g, b, a)) = self.ivars().fill.get() {
                NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a).setFill();
                NSRectFill(dirty);
            }
        }
    }
);

impl PaintedView {
    pub fn create(mtm: MainThreadMarker, frame: NSRect, fill: Option<Color>) -> Retained<PaintedView> {
        let this = Self::alloc(mtm).set_ivars(PaintedIvars::default());
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let view: Retained<PaintedView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.set_fill(fill);
        view
    }

    pub fn set_fill(&self, fill: Option<Color>) {
        self.ivars().fill.set(fill.map(|c| {
            let [r, g, b, a] = c.to_f32();
            (f64::from(r), f64::from(g), f64::from(b), f64::from(a))
        }));
        self.setNeedsDisplay(true);
    }
}

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

pub fn color(c: Color) -> Retained<NSColor> {
    let [r, g, b, a] = c.to_f32();
    NSColor::colorWithSRGBRed_green_blue_alpha(f64::from(r), f64::from(g), f64::from(b), f64::from(a))
}

/// Any view as an `NSView`.
pub fn view_of<T: AsRef<NSView> + Message>(v: &Retained<T>) -> Retained<NSView> {
    let view: &NSView = v.as_ref();
    view.retain()
}

pub fn size(w: f64, h: f64) -> NSSize {
    NSSize::new(w, h)
}

pub fn frame(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}
