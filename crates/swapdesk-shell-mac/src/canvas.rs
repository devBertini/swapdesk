//! The canvas: one flipped `NSView` that paints a window's shared frame (`swapdesk-paint`, D37) with the
//! CoreGraphics painter and turns the mouse into the shared interaction (hover, press, activate, scroll), as the
//! Windows window does. The view keeps what the pointer is doing and where the last frame put every control; the
//! state stays in the shell.
//!
//! Apple documentation: `NSView.draw(_:)` and `isFlipped` (developer.apple.com/documentation/appkit/nsview),
//! `NSGraphicsContext.current` / `cgContext` (developer.apple.com/documentation/appkit/nsgraphicscontext),
//! `NSTrackingArea` with `inVisibleRect` (developer.apple.com/documentation/appkit/nstrackingarea), the `NSResponder`
//! mouse methods, `NSEvent.scrollingDeltaY` / `hasPreciseScrollingDeltas`, `NSCursor.set()`,
//! `NSMenu.popUp(positioning:at:in:)` (developer.apple.com/documentation/appkit/nsmenu),
//! `NSView.bitmapImageRepForCachingDisplay(in:)` / `cacheDisplay(in:to:)` and
//! `NSBitmapImageRep.representation(using:properties:)`.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSControlStateValueOn, NSCursor, NSEvent, NSGraphicsContext, NSMenu,
    NSMenuItem, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{MainThreadMarker, NSDictionary, NSObjectProtocol, NSPoint, NSRect, NSString};
use swapdesk_paint::{Frame, Interaction, Output, Rect, Widget, input, layout_dump};
use swapdesk_ui::{ControlId, WindowKind};

use crate::painter::{CgPainter, Fonts};

/// What one notch of a wheel without precise deltas scrolls, in DIPs per line the system reports.
const WHEEL_LINE: f32 = 20.0;

thread_local! {
    /// The embedded face and the laid-out labels, shared by every canvas (main thread only).
    static FONTS: Fonts = Fonts::new();
}

pub struct CanvasIvars {
    kind: WindowKind,
    ix: RefCell<Interaction>,
    /// What the last paint registered: the controls, the areas and the scroll range.
    out: RefCell<Output>,
    painted: Cell<bool>,
    /// The option chosen in the menu that is open (see [`CanvasView::pick`]).
    picked: Cell<Option<usize>>,
    /// Development: the first paint writes its layout dump here (`--shot`).
    dump: RefCell<Option<PathBuf>>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond calling super where AppKit expects it; this class does not
    // implement `Drop`.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskCanvasView"]
    #[ivars = CanvasIvars]
    pub struct CanvasView;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for CanvasView {}

    impl CanvasView {
        // SAFETY (all methods below): the selectors and signatures are those of NSView / NSResponder, and
        // `menuPicked:` is the action this view gives its own menu items.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // The main window's frame covers every pixel; a viewer's leaves the video rectangle to the view below.
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            self.ivars().kind == WindowKind::Main
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.paint();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.hover(self.hit(event));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.hover(self.hit(event));
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            self.hover(self.hit(event));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.hover(None);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let hit = self.hit(event);
            {
                let mut ix = self.ivars().ix.borrow_mut();
                ix.pressed = hit;
                ix.hover = hit;
                ix.focus = hit;
                ix.focus_visible = false;
            }
            if let Some(w) = self.window() {
                w.makeFirstResponder(Some(self));
            }
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let hit = self.hit(event);
            let pressed = {
                let mut ix = self.ivars().ix.borrow_mut();
                ix.hover = hit;
                ix.pressed.take()
            };
            self.setNeedsDisplay(true);
            // A click is a press and a release on the same control; a text input only takes the focus.
            if let (Some(p), Some(h)) = (pressed, hit)
                && p == h
                && !matches!(p, ControlId::Edit(_))
            {
                crate::shell::activate_from(self.ivars().kind, p);
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let max = self.ivars().out.borrow().scroll_max;
            if max <= 0.0 {
                return;
            }
            let step = if event.hasPreciseScrollingDeltas() { 1.0 } else { WHEEL_LINE };
            let delta = event.scrollingDeltaY() as f32 * step;
            let moved = {
                let mut ix = self.ivars().ix.borrow_mut();
                let to = (ix.scroll - delta).clamp(0.0, max);
                std::mem::replace(&mut ix.scroll, to) != to
            };
            if moved {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(menuPicked:))]
        fn menu_picked(&self, sender: &NSMenuItem) {
            self.ivars().picked.set(usize::try_from(sender.tag()).ok());
        }
    }
);

impl CanvasView {
    pub fn create(mtm: MainThreadMarker, frame: NSRect, kind: WindowKind) -> Retained<CanvasView> {
        let this = Self::alloc(mtm).set_ivars(CanvasIvars {
            kind,
            ix: RefCell::default(),
            out: RefCell::default(),
            painted: Cell::new(false),
            picked: Cell::new(None),
            dump: RefCell::new(None),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let view: Retained<CanvasView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let options = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::InVisibleRect;
        let owner: &AnyObject = view.as_ref();
        // SAFETY: the owner is the view, which implements the mouse methods the options ask for; the view keeps the
        // area and AppKit drops it with the view. `InVisibleRect` makes the rectangle follow the view's visible part.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                options,
                Some(owner),
                None,
            )
        };
        view.addTrackingArea(&area);
        view
    }

    /// Development (`--shot`): the first paint writes its layout dump to `path`.
    pub fn set_dump(&self, path: PathBuf) {
        *self.ivars().dump.borrow_mut() = Some(path);
    }

    /// The controls of the last paint (empty before the first one).
    pub fn widgets(&self) -> Vec<Widget> {
        self.ivars().out.borrow().widgets.clone()
    }

    pub fn painted(&self) -> bool {
        self.ivars().painted.get()
    }

    /// Paints the view (and what sits on it) into a bitmap at the window's resolution, through `drawRect:`. It needs
    /// neither the screen nor the screen recording grant, so it works with the Mac locked.
    fn snapshot(&self) -> Option<Retained<NSBitmapImageRep>> {
        let bounds = self.bounds();
        let rep = self.bitmapImageRepForCachingDisplayInRect(bounds)?;
        self.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
        Some(rep)
    }

    /// Paints now, without waiting for the window's next display pass: `true` when the frame was painted.
    pub fn paint_now(&self) -> bool {
        self.ivars().painted.set(false);
        self.snapshot().is_some() && self.painted()
    }

    /// Development (`--shot`): the view as a PNG at `path`.
    pub fn save_png(&self, path: &Path) -> bool {
        let Some(rep) = self.snapshot() else { return false };
        // SAFETY: PNG takes no properties: the dictionary is empty.
        let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) };
        data.is_some_and(|d| d.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true))
    }

    /// Where control `id` was painted, in the view's coordinates (DIPs, y down).
    pub fn rect_of(&self, id: ControlId) -> Option<Rect> {
        self.ivars().out.borrow().widgets.iter().find(|w| w.id == id).map(|w| w.rect)
    }

    /// Opens a menu of `options` (label, checked) under control `at` and waits for the person: the index chosen, or
    /// `None` when the menu was dismissed.
    pub fn pick(&self, options: &[(String, bool)], at: ControlId) -> Option<usize> {
        let r = self.rect_of(at)?;
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let target: &AnyObject = self.as_ref();
        for (i, (label, on)) in options.iter().enumerate() {
            // SAFETY: `menuPicked:` is this view's own method; the item's target is set to the view below.
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(label),
                    Some(sel!(menuPicked:)),
                    &NSString::from_str(""),
                )
            };
            // SAFETY: the target is this view, which outlives the menu (the menu is dropped when this call returns).
            unsafe { item.setTarget(Some(target)) };
            item.setTag(i as isize);
            if *on {
                item.setState(NSControlStateValueOn);
            }
            menu.addItem(&item);
        }
        self.ivars().picked.set(None);
        let at = NSPoint::new(f64::from(r.x), f64::from(r.bottom()));
        menu.popUpMenuPositioningItem_atLocation_inView(None, at, Some(self));
        self.ivars().picked.take()
    }

    fn hit(&self, event: &NSEvent) -> Option<ControlId> {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        input::hit_test(&self.ivars().out.borrow().widgets, p.x as f32, p.y as f32)
    }

    /// The control under the pointer changed: its tint is repainted and the pointer shows what a click would do.
    fn hover(&self, hit: Option<ControlId>) {
        let was = std::mem::replace(&mut self.ivars().ix.borrow_mut().hover, hit);
        if was != hit {
            self.setNeedsDisplay(true);
        }
        let cursor = match hit {
            Some(ControlId::Edit(_)) => NSCursor::IBeamCursor(),
            Some(_) => NSCursor::pointingHandCursor(),
            None => NSCursor::arrowCursor(),
        };
        cursor.set();
    }

    /// Paints the frame on the context AppKit made current for `drawRect:` and keeps what it registered.
    fn paint(&self) {
        let Some(gc) = NSGraphicsContext::currentContext() else { return };
        let cg = gc.CGContext();
        let ctx = Retained::as_ptr(&cg).cast_mut().cast::<c_void>();
        let bounds = self.bounds();
        let size = (bounds.size.width as f32, bounds.size.height as f32);
        let scale = self.window().map_or(1.0, |w| w.backingScaleFactor()) as f32;
        let iv = self.ivars();
        let out = crate::shell::with_state(|state| {
            FONTS.with(|fonts| {
                // SAFETY: `ctx` is the current context of this `drawRect:` and `cg` keeps it alive past the paint;
                // the view is flipped, so its CTM is y-down in points, as the painter expects.
                let painter = unsafe { CgPainter::new(ctx, fonts, scale, size) };
                let ix = iv.ix.borrow();
                // Until the native text fields sit in the edit slots, the frame draws their values itself.
                let mut frame = Frame::new(&painter, state, &ix, size, true);
                frame.video_under = iv.kind == WindowKind::Viewer;
                swapdesk_paint::paint(frame, iv.kind)
            })
        });
        // The state is busy only inside the shell's own calls, which mark the view dirty when they end.
        let Some(out) = out else { return };
        {
            // The wheel can leave the list scrolled past a list that got shorter.
            let mut ix = iv.ix.borrow_mut();
            ix.scroll = ix.scroll.clamp(0.0, out.scroll_max);
        }
        // The first paint is the scenario as it was opened; a later one may already show what the backend answered.
        if let Some(path) = iv.dump.borrow_mut().take() {
            let _ = std::fs::write(path, layout_dump(&out));
        }
        *iv.out.borrow_mut() = out;
        iv.painted.set(true);
    }
}
