//! The canvas: one flipped `NSView` that paints a window's shared frame (`swapdesk-paint`, D37) with the
//! CoreGraphics painter, turns the mouse and the keyboard into the shared interaction (hover, press, activate,
//! scroll, Tab order) and keeps a native text field over every edit slot the frame reports, as the Windows window
//! does. The view keeps what the pointer is doing and where the last frame put every control; the state stays in the
//! shell.
//!
//! Apple documentation: `NSView.draw(_:)` and `isFlipped` (developer.apple.com/documentation/appkit/nsview),
//! `NSGraphicsContext.current` / `cgContext` (developer.apple.com/documentation/appkit/nsgraphicscontext),
//! `NSTrackingArea` with `inVisibleRect` (developer.apple.com/documentation/appkit/nstrackingarea), the `NSResponder`
//! mouse and key methods, `NSEvent.scrollingDeltaY` / `hasPreciseScrollingDeltas` / `keyCode`, `NSCursor.set()`,
//! `NSMenu.popUp(positioning:at:in:)` (developer.apple.com/documentation/appkit/nsmenu),
//! `NSView.bitmapImageRepForCachingDisplay(in:)` / `cacheDisplay(in:to:)`,
//! `NSBitmapImageRep.representation(using:properties:)`, `NSTextField` (`isBordered`, `drawsBackground`,
//! `focusRingType`, `placeholderAttributedString`, `currentEditor()`) and
//! `NSControlTextEditingDelegate.control(_:textView:doCommandBy:)`
//! (developer.apple.com/documentation/appkit/nscontroltexteditingdelegate).

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSControl, NSControlStateValueOn, NSControlTextEditingDelegate, NSCursor,
    NSEvent, NSEventModifierFlags, NSEventType, NSFocusRingType, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSGraphicsContext, NSMenu, NSMenuItem, NSTextField, NSTextFieldDelegate,
    NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedString, NSDictionary, NSNotification, NSObjectProtocol, NSPoint, NSRange,
    NSRect, NSSize, NSString,
};
use swapdesk_paint::{EditSlot, Frame, Interaction, Output, Rect, Widget, input, layout_dump};
use swapdesk_ui::tokens::{Palette, text};
use swapdesk_ui::{ControlId, EditId, WindowKind};

use crate::appkit::color;
use crate::painter::{CgPainter, Fonts};

mod ax;

pub use ax::ax_children_of;

/// What one notch of a wheel without precise deltas scrolls, in DIPs per line the system reports.
const WHEEL_LINE: f32 = 20.0;
/// The text of an `NSTextFieldCell` starts this far inside its frame.
const CELL_INSET: f64 = 2.0;
/// Virtual key codes (`kVK_Tab`, `kVK_Return`, `kVK_ANSI_KeypadEnter`, `kVK_Space`, `kVK_Escape`).
const KEY_TAB: u16 = 48;
const KEY_RETURN: u16 = 36;
const KEY_ENTER: u16 = 76;
const KEY_SPACE: u16 = 49;
const KEY_ESCAPE: u16 = 53;
/// The explorer's keys (FILES-UI.md 3.2, Mac column): `kVK_F2`, `kVK_Delete` (Backspace), `kVK_ANSI_N`, `kVK_ANSI_I`,
/// `kVK_ANSI_X`, `kVK_ANSI_V` (HIToolbox `Events.h`).
const KEY_F2: u16 = 120;
const KEY_BACKSPACE: u16 = 51;
const KEY_N: u16 = 45;
const KEY_I: u16 = 34;
const KEY_X: u16 = 7;
const KEY_V: u16 = 9;

/// The explorer's key for an event in the session window: F2 and Esc plain; the others with Command (Command+Option+
/// Backspace deletes for good, Command+Shift+N makes a folder).
fn explorer_key_of(code: u16, flags: NSEventModifierFlags) -> Option<swapdesk_ui::ExplorerKey> {
    use swapdesk_ui::ExplorerKey as K;
    let cmd = flags.contains(NSEventModifierFlags::Command);
    let opt = flags.contains(NSEventModifierFlags::Option);
    let shift = flags.contains(NSEventModifierFlags::Shift);
    Some(match code {
        KEY_F2 => K::Rename,
        KEY_ESCAPE => K::Escape,
        KEY_BACKSPACE if cmd && opt => K::DeleteForever,
        KEY_BACKSPACE if cmd => K::Delete,
        KEY_N if cmd && shift => K::NewFolder,
        KEY_I if cmd => K::Properties,
        KEY_X if cmd => K::Cut,
        KEY_V if cmd => K::Paste,
        _ => return None,
    })
}

thread_local! {
    /// The embedded face and the laid-out labels, shared by every canvas (main thread only).
    static FONTS: Fonts = Fonts::new();
}

/// A native text input over an edit slot of the frame.
#[derive(Clone)]
struct Field {
    id: EditId,
    view: Retained<NSTextField>,
    /// The height of its one line of text.
    height: f64,
}

pub struct CanvasIvars {
    kind: WindowKind,
    ix: RefCell<Interaction>,
    /// What the last paint registered: the controls, the edit slots, the areas and the scroll range.
    out: RefCell<Output>,
    painted: Cell<bool>,
    /// Paints AppKit asked for in its own display pass (not the ones `paint_now` forces), and the ones among them
    /// that found the state busy and drew nothing.
    passes: Cell<u64>,
    skipped: Cell<u64>,
    /// `paint_now` is painting: the pass is not AppKit's.
    forcing: Cell<bool>,
    /// The option chosen in the menu that is open (see [`CanvasView::pick`]).
    picked: Cell<Option<usize>>,
    /// Development: the first paint writes its layout dump here (`--shot`).
    dump: RefCell<Option<PathBuf>>,
    /// The native inputs made so far; one whose slot is not drawn is hidden, not removed (it keeps its text).
    fields: RefCell<Vec<Field>>,
    /// A placement of the fields is queued on the main queue.
    placing: Cell<bool>,
    /// The theme the fields' colours were set for.
    fields_dark: Cell<Option<u32>>,
    /// The view that has the keyboard while no text field does: the video view of a viewer window (its keys go to
    /// the host), else the canvas itself.
    home: RefCell<Option<Retained<NSView>>>,
    /// The accessibility elements of the last frame (see [`ax`]).
    ax: RefCell<ax::AxState>,
    /// Development: the accessibility dump of the first paint goes here once its text fields are placed (`--shot`).
    ax_dump: RefCell<Option<PathBuf>>,
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

        // The keyboard went to a text field (or another view): the ring of a painted control goes, as after a click.
        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            let hide = {
                let mut ix = self.ivars().ix.borrow_mut();
                let on_edit = matches!(ix.focus, Some(ControlId::Edit(_)));
                !on_edit && std::mem::replace(&mut ix.focus_visible, false)
            };
            if hide {
                self.setNeedsDisplay(true);
            }
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.paint();
        }

        // A viewer's canvas lets the mouse through to the video view below, where the frame left the video
        // rectangle free of controls.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            // The point comes in the superview's coordinates: a viewer's container is flipped like this view, so
            // the view's own origin is all that separates the two.
            let origin = self.frame().origin;
            let (x, y) = ((point.x - origin.x) as f32, (point.y - origin.y) as f32);
            let through = {
                let out = self.ivars().out.borrow();
                out.video.is_some_and(|v| v.contains(x, y)) && input::hit_test(&out.widgets, x, y).is_none()
            };
            if through && self.ivars().home.borrow().is_some() {
                None
            } else {
                // SAFETY: forwards the same point to NSView's implementation.
                unsafe { msg_send![super(self), hitTest: point] }
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let hit = self.hit(event);
            let over_video = hit.is_none() && self.over_video(event);
            self.hover(hit, over_video);
            if over_video {
                self.pass_move_to_video(event);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if crate::terminal::mouse_dragged(self, event) {
                return;
            }
            let hit = self.hit(event);
            if self.explorer_on() {
                // A press on a row, moved, is the explorer's drag; Option is the copy key.
                let copy = event.modifierFlags().contains(NSEventModifierFlags::Option);
                crate::shell::drag_move(hit, self.point(event), copy);
            }
            self.hover(hit, hit.is_none() && self.over_video(event));
        }

        // The explorer's context menu on a right click (Control+click is handled in `mouseDown:`).
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            if self.explorer_on() {
                self.context_at(event);
            } else {
                // SAFETY: forwards the same event to NSView's implementation.
                unsafe { msg_send![super(self), rightMouseDown: event] }
            }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            let hit = self.hit(event);
            self.hover(hit, hit.is_none() && self.over_video(event));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.hover(None, false);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if self.ivars().kind == WindowKind::Viewer && crate::terminal::mouse_down(self, event) {
                return;
            }
            let hit = self.hit(event);
            if self.explorer_on() {
                if event.modifierFlags().contains(NSEventModifierFlags::Control) {
                    // Control+click is a right click on the Mac.
                    self.context_at(event);
                    return;
                }
                crate::shell::drag_press(hit, self.point(event));
            }
            {
                let mut ix = self.ivars().ix.borrow_mut();
                ix.pressed = hit;
                ix.hover = hit;
                ix.focus = hit;
                ix.focus_visible = false;
            }
            // A click on the box of a text input (around the native field) puts the keyboard in the field.
            let field = match hit {
                Some(ControlId::Edit(e)) => self.field(e),
                _ => None,
            };
            match &field {
                Some(f) => {
                    if let Some(w) = self.window() {
                        w.makeFirstResponder(Some(f));
                    }
                }
                None => self.take_keyboard(),
            }
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if crate::terminal::mouse_up(self) {
                return;
            }
            let hit = self.hit(event);
            let pressed = {
                let mut ix = self.ivars().ix.borrow_mut();
                ix.hover = hit;
                ix.pressed.take()
            };
            self.setNeedsDisplay(true);
            if self.explorer_on() {
                let copy = event.modifierFlags().contains(NSEventModifierFlags::Option);
                if crate::shell::drag_release(hit, self.point(event), copy) {
                    // A drag ran (or was cancelled): the release is not a click on what is under it.
                    return;
                }
            }
            // A click is a press and a release on the same control; a text input only takes the focus.
            if let (Some(p), Some(h)) = (pressed, hit)
                && p == h
                && !matches!(p, ControlId::Edit(_))
            {
                // The explorer's rows select and open with Command, Shift and a double click.
                let flags = event.modifierFlags();
                crate::shell::set_pointer_mods(swapdesk_ui::PointerMods {
                    toggle: flags.contains(NSEventModifierFlags::Command),
                    extend: flags.contains(NSEventModifierFlags::Shift),
                    clicks: u8::try_from(event.clickCount().clamp(1, 2)).unwrap_or(1),
                });
                crate::shell::activate_from(self.ivars().kind, p);
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let line = text::TERM.size * text::TERM.line;
            if self.ivars().kind == WindowKind::Viewer && crate::terminal::scroll(self, event, line) {
                return;
            }
            // The explorer's right pane scrolls on its own (its offset is `drawer_scroll`).
            let second = self.ivars().out.borrow().second_list;
            if let Some((area, max)) = second {
                let p = self.convertPoint_fromView(event.locationInWindow(), None);
                if area.contains(p.x as f32, p.y as f32) {
                    let step = if event.hasPreciseScrollingDeltas() { 1.0 } else { WHEEL_LINE };
                    let delta = event.scrollingDeltaY() as f32 * step;
                    let mut ix = self.ivars().ix.borrow_mut();
                    ix.drawer_scroll = (ix.drawer_scroll - delta).clamp(0.0, max);
                    drop(ix);
                    self.setNeedsDisplay(true);
                    return;
                }
            }
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

        // The explorer's Command shortcuts reach the session window before the menu's Cut and Paste do.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let mine = (self.ivars().kind == WindowKind::Viewer
                && self.editing().is_none()
                && crate::shell::explorer_open())
            .then(|| explorer_key_of(event.keyCode(), event.modifierFlags()))
            .flatten()
            .filter(|k| !matches!(k, swapdesk_ui::ExplorerKey::Rename | swapdesk_ui::ExplorerKey::Escape));
            if let Some(k) = mine {
                crate::shell::explorer_key(k);
                true
            } else {
                // SAFETY: forwards the same event to NSView's implementation.
                let handled: bool = unsafe { msg_send![super(self), performKeyEquivalent: event] };
                handled
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.key(event) {
                // SAFETY: forwards the same event to NSView's implementation.
                unsafe { msg_send![super(self), keyDown: event] }
            }
        }

        #[unsafe(method(menuPicked:))]
        fn menu_picked(&self, sender: &NSMenuItem) {
            self.ivars().picked.set(usize::try_from(sender.tag()).ok());
        }

        // Accessibility: the view is painted, so it answers for its controls itself (see `ax`). It stays out of the
        // tree (a plain view is not an element): its children are the window's landmarks.
        #[unsafe(method_id(accessibilityChildren))]
        fn accessibility_children(&self) -> Option<Retained<NSArray<AnyObject>>> {
            Some(self.ax_children())
        }

        #[unsafe(method_id(accessibilityHitTest:))]
        fn accessibility_hit_test(&self, point: NSPoint) -> Option<Retained<AnyObject>> {
            // SAFETY: forwards the same point to NSView's implementation.
            self.ax_hit(point).or_else(|| unsafe { msg_send![super(self), accessibilityHitTest: point] })
        }

        #[unsafe(method_id(accessibilityFocusedUIElement))]
        fn accessibility_focused(&self) -> Option<Retained<AnyObject>> {
            // SAFETY: forwards to NSView's implementation.
            self.ax_focused().or_else(|| unsafe { msg_send![super(self), accessibilityFocusedUIElement] })
        }
    }

    // SAFETY: the selectors and signatures are the protocol's (the text view is taken as a plain object).
    unsafe impl NSControlTextEditingDelegate for CanvasView {
        // Return, Tab, Shift+Tab and Esc typed in a field.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, control: &NSControl, _text_view: &AnyObject, command: Sel) -> bool {
            self.field_command(control, command)
        }

        #[unsafe(method(controlTextDidEndEditing:))]
        fn did_end_editing(&self, notification: &NSNotification) {
            self.field_left(notification);
        }
    }

    // SAFETY: `NSTextFieldDelegate` adds only optional methods, none implemented here.
    unsafe impl NSTextFieldDelegate for CanvasView {}
);

impl CanvasView {
    pub fn create(mtm: MainThreadMarker, frame: NSRect, kind: WindowKind) -> Retained<CanvasView> {
        let this = Self::alloc(mtm).set_ivars(CanvasIvars {
            kind,
            ix: RefCell::default(),
            out: RefCell::default(),
            painted: Cell::new(false),
            passes: Cell::new(0),
            skipped: Cell::new(0),
            forcing: Cell::new(false),
            picked: Cell::new(None),
            dump: RefCell::new(None),
            fields: RefCell::default(),
            placing: Cell::new(false),
            fields_dark: Cell::new(None),
            home: RefCell::new(None),
            ax: RefCell::default(),
            ax_dump: RefCell::new(None),
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

    /// Development (`--shot`): the first paint writes its layout dump to `path` (`<name>.layout.txt`), and what an
    /// assistive client reads in it to `<name>.a11y.txt`.
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

    fn snapshot(&self) -> Option<Retained<NSBitmapImageRep>> {
        snapshot_of(self)
    }

    /// Paints now and places the text fields, without waiting for the window's next display pass: `true` when the
    /// frame was painted.
    pub fn paint_now(&self) -> bool {
        self.ivars().painted.set(false);
        self.ivars().forcing.set(true);
        let painted = self.snapshot().is_some() && self.painted();
        self.ivars().forcing.set(false);
        self.place_fields();
        painted
    }

    /// How many times AppKit's own display pass painted this view, and how many of those found the state busy.
    pub fn display_passes(&self) -> (u64, u64) {
        (self.ivars().passes.get(), self.ivars().skipped.get())
    }

    /// Development (`--shot`): the view with its text fields as a PNG at `path`.
    pub fn save_png(&self, path: &Path) -> bool {
        // The first pass places the fields the picture must show.
        self.paint_now();
        save_view_png(self, path)
    }

    /// Where control `id` was painted, in the view's coordinates (DIPs, y down).
    pub fn rect_of(&self, id: ControlId) -> Option<Rect> {
        self.ivars().out.borrow().widgets.iter().find(|w| w.id == id).map(|w| w.rect)
    }

    /// Opens a menu of `options` (label, checked) under control `at` and waits for the person: the index chosen, or
    /// `None` when the menu was dismissed.
    pub fn pick(&self, options: &[(String, bool)], at: ControlId) -> Option<usize> {
        let r = self.rect_of(at)?;
        let items: Vec<(String, bool, bool)> = options.iter().map(|(label, on)| (label.clone(), *on, true)).collect();
        self.popup(&items, NSPoint::new(f64::from(r.x), f64::from(r.bottom())))
    }

    /// Opens the system's menu of `items` (label, checked, enabled) at `at` (this view's coordinates) and waits for the
    /// person: the index chosen, or `None` when it was dismissed. The select menus and the explorer's context menu
    /// both use it; a disabled item stays in the list, greyed (`autoenablesItems` is off, so `enabled` rules).
    pub fn popup(&self, items: &[(String, bool, bool)], at: NSPoint) -> Option<usize> {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let target: &AnyObject = self.as_ref();
        for (i, (label, on, enabled)) in items.iter().enumerate() {
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
            item.setEnabled(*enabled);
            if *on {
                item.setState(NSControlStateValueOn);
            }
            menu.addItem(&item);
        }
        self.ivars().picked.set(None);
        menu.popUpMenuPositioningItem_atLocation_inView(None, at, Some(self));
        self.ivars().picked.take()
    }

    fn hit(&self, event: &NSEvent) -> Option<ControlId> {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        input::hit_test(&self.ivars().out.borrow().widgets, p.x as f32, p.y as f32)
    }

    /// The explorer of this viewer window is open: its rows take the mouse (drag, context menu).
    fn explorer_on(&self) -> bool {
        self.ivars().kind == WindowKind::Viewer && crate::shell::explorer_open()
    }

    /// The event's point in this view's coordinates (DIPs: the view is flipped like the frame).
    fn point(&self, event: &NSEvent) -> (f32, f32) {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        (p.x as f32, p.y as f32)
    }

    /// A right click or Control+click on the explorer: the state decides what the menu holds.
    fn context_at(&self, event: &NSEvent) {
        crate::shell::context_menu(self.hit(event), self.point(event));
    }

    /// While a drag runs: the list under the pointer scrolls when the pointer is in its top or bottom band.
    pub fn drag_autoscroll(&self) {
        let Some(window) = self.window() else { return };
        let p = self.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
        let (x, y) = (p.x as f32, p.y as f32);
        let iv = self.ivars();
        let (second, list, max) = {
            let out = iv.out.borrow();
            (out.second_list, out.list_area, out.scroll_max)
        };
        let moved = {
            let mut ix = iv.ix.borrow_mut();
            if let Some((area, most)) = second.filter(|(a, _)| a.contains(x, y)) {
                let to = (ix.drawer_scroll + swapdesk_ui::drag_autoscroll(area.y, area.h, y)).clamp(0.0, most);
                std::mem::replace(&mut ix.drawer_scroll, to) != to
            } else if let Some(area) = list.filter(|a| a.contains(x, y)) {
                let to = (ix.scroll + swapdesk_ui::drag_autoscroll(area.y, area.h, y)).clamp(0.0, max);
                std::mem::replace(&mut ix.scroll, to) != to
            } else {
                false
            }
        };
        if moved {
            self.setNeedsDisplay(true);
        }
    }

    /// The control under the pointer changed: its tint is repainted and the pointer shows what a click would do. Over
    /// the video rectangle the pointer is the video view's (the host's cursor shape), set here too: the toolbar and the
    /// chat lie inside that rectangle, so a pointer leaving them never crosses the video view's cursor rect.
    fn hover(&self, hit: Option<ControlId>, over_video: bool) {
        let was = std::mem::replace(&mut self.ivars().ix.borrow_mut().hover, hit);
        if was != hit {
            self.setNeedsDisplay(true);
        }
        match crate::model::pointer_shape(hit, over_video) {
            crate::model::PointerShape::Text => NSCursor::IBeamCursor().set(),
            crate::model::PointerShape::Hand => NSCursor::pointingHandCursor().set(),
            crate::model::PointerShape::Arrow => NSCursor::arrowCursor().set(),
            crate::model::PointerShape::Video => match self.ivars().home.borrow().as_ref() {
                Some(home) => crate::video::show_view_cursor(home),
                None => NSCursor::arrowCursor().set(),
            },
        }
    }

    /// The event is over the video rectangle of a viewer (where the video view below takes the mouse).
    fn over_video(&self, event: &NSEvent) -> bool {
        if self.ivars().home.borrow().is_none() {
            return false;
        }
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        self.ivars().out.borrow().video.is_some_and(|v| v.contains(p.x as f32, p.y as f32))
    }

    /// A pointer move over the video rectangle belongs to the video view. AppKit sends a move to the owner of the
    /// tracking area under the pointer, which is this canvas (it covers the window, and its area is always active);
    /// the video view below would get moves by itself only as the first responder of the key window. So the canvas
    /// is the one path: every move over the video goes on to the video view, with the keyboard anywhere and with the
    /// window in front or not, as the Windows viewer forwards the mouse over its video. (The viewer window does not
    /// ask AppKit for mouse-moved events, or the video view would get each move twice.)
    fn pass_move_to_video(&self, event: &NSEvent) {
        let home = self.ivars().home.borrow().clone();
        if let Some(home) = home {
            home.mouseMoved(event);
        }
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
                let mut frame = Frame::new(&painter, state, &ix, size, false);
                frame.video_under = iv.kind == WindowKind::Viewer;
                let out = swapdesk_paint::paint(frame, iv.kind);
                // The drawer's terminal grid, over the frame, through the same painter as the screens.
                let commands = if iv.kind == WindowKind::Viewer {
                    let focused = ix.focus == Some(ControlId::TermGrid);
                    crate::shell::with_terminal(|t| t.paint(&painter, out.terminal, focused)).unwrap_or_default()
                } else {
                    Vec::new()
                };
                (out, commands)
            })
        });
        if !iv.forcing.get() {
            iv.passes.set(iv.passes.get() + 1);
        }
        // The state is busy only inside the shell's own calls, which mark the view dirty when they end.
        let Some((out, commands)) = out else {
            iv.skipped.set(iv.skipped.get() + 1);
            return;
        };
        // A new grid size for the host's terminal goes out after the paint, outside `drawRect:`.
        if !commands.is_empty() {
            DispatchQueue::main().exec_async(move || crate::shell::send_from_canvas(commands));
        }
        {
            // The wheel can leave the list scrolled past a list that got shorter; another scrolling content starts
            // at its top.
            let mut ix = iv.ix.borrow_mut();
            if ix.scroll_key != out.scroll_key {
                ix.scroll_key = out.scroll_key;
                ix.scroll = 0.0;
            }
            ix.scroll = ix.scroll.clamp(0.0, out.scroll_max);
        }
        // The first paint is the scenario as it was opened; a later one may already show what the backend answered.
        if let Some(path) = iv.dump.borrow_mut().take() {
            let _ = std::fs::write(&path, layout_dump(&out));
            let beside = path.to_string_lossy().strip_suffix("layout.txt").map(|stem| format!("{stem}a11y.txt"));
            *iv.ax_dump.borrow_mut() = beside.map(PathBuf::from);
        }
        *iv.out.borrow_mut() = out;
        iv.painted.set(true);
        iv.ax.borrow_mut().stale = true;
        // The view tree must not change inside `drawRect:`: the fields follow on the next turn of the main queue.
        if !iv.placing.replace(true) {
            DispatchQueue::main().exec_async(crate::shell::place_fields);
        }
    }

    // ----- the keyboard -----

    /// A key the canvas itself takes: `false` leaves it to AppKit.
    fn key(&self, event: &NSEvent) -> bool {
        let kind = self.ivars().kind;
        if kind == WindowKind::Viewer && crate::terminal::key(self, event) {
            return true;
        }
        if kind == WindowKind::Viewer
            && crate::shell::explorer_open()
            && let Some(k) = explorer_key_of(event.keyCode(), event.modifierFlags())
        {
            crate::shell::explorer_key(k);
            return true;
        }
        match event.keyCode() {
            KEY_TAB => {
                self.move_focus(event.modifierFlags().contains(NSEventModifierFlags::Shift));
                true
            }
            KEY_RETURN | KEY_ENTER | KEY_SPACE => {
                let focus = self.ivars().ix.borrow().focus;
                if let Some(id) = focus.filter(|i| !matches!(i, ControlId::Edit(_))) {
                    crate::shell::activate_from(kind, id);
                }
                true
            }
            KEY_ESCAPE => {
                // The rename field closes without saving, else a waiting card is cancelled or an ended one closed,
                // wherever the keyboard is in the main window; the other windows swallow the key.
                if kind == WindowKind::Main {
                    crate::shell::escape();
                }
                true
            }
            _ => false,
        }
    }

    /// The field that has the keyboard.
    pub fn editing(&self) -> Option<EditId> {
        let fields = self.ivars().fields.borrow();
        fields.iter().find(|f| !f.view.isHidden() && f.view.currentEditor().is_some()).map(|f| f.id)
    }

    /// Tab and Shift+Tab: the next control of the frame's Tab order takes the focus and shows its ring.
    fn move_focus(&self, backwards: bool) {
        let iv = self.ivars();
        let order = crate::shell::with_state(|st| input::tab_order(st, iv.kind, &iv.out.borrow().widgets));
        if let Some(next) = input::next_focus(&order.unwrap_or_default(), self.focus(), backwards) {
            self.focus_control(next, true);
        }
    }

    /// Gives control `id` the focus; `visible` shows the ring (keyboard navigation). A text input gets the keyboard
    /// in its native field, any other control leaves it with the canvas.
    pub fn focus_control(&self, id: ControlId, visible: bool) {
        {
            let mut ix = self.ivars().ix.borrow_mut();
            ix.focus = Some(id);
            ix.focus_visible = visible;
        }
        let field = match id {
            ControlId::Edit(e) => self.field(e),
            _ => None,
        };
        match &field {
            Some(f) => {
                if let Some(w) = self.window() {
                    w.makeFirstResponder(Some(f));
                }
            }
            None => self.take_keyboard(),
        }
        self.setNeedsDisplay(true);
    }

    /// The control that has the focus: the field being typed in, else the painted control.
    pub fn focus(&self) -> Option<ControlId> {
        self.editing().map(ControlId::Edit).or(self.ivars().ix.borrow().focus)
    }

    /// The keyboard leaves the text fields: it goes to the home view (see [`CanvasView::set_keyboard_home`]).
    fn take_keyboard(&self) {
        let Some(w) = self.window() else { return };
        // The terminal grid takes the keys itself (they go to the other computer's shell, not as input events).
        let grid = self.ivars().ix.borrow().focus == Some(ControlId::TermGrid);
        match self.ivars().home.borrow().as_ref() {
            // A hidden video view (the Arquivos view) takes no keys.
            Some(home) if !grid && !home.isHidden() => w.makeFirstResponder(Some(home)),
            _ => w.makeFirstResponder(Some(self)),
        };
    }

    /// A viewer window's video view: it keeps the keyboard while no text field has it, and gets the mouse over the
    /// video rectangle.
    pub fn set_keyboard_home(&self, view: &NSView) {
        *self.ivars().home.borrow_mut() = Some(view.retain());
    }

    /// Where the last paint left room for the video, in the view's coordinates (DIPs, y down).
    pub fn video_rect(&self) -> Option<Rect> {
        self.ivars().out.borrow().video
    }

    // ----- the native text fields -----

    /// The visible field of input `id`.
    fn field(&self, id: EditId) -> Option<Retained<NSTextField>> {
        let fields = self.ivars().fields.borrow();
        fields.iter().find(|f| f.id == id && !f.view.isHidden()).map(|f| f.view.clone())
    }

    /// Which input `control` is, when it is one of this canvas's fields.
    pub fn edit_of(&self, control: &NSControl) -> Option<EditId> {
        let wanted = std::ptr::from_ref(control).cast::<c_void>();
        let fields = self.ivars().fields.borrow();
        fields.iter().find(|f| Retained::as_ptr(&f.view).cast::<c_void>() == wanted).map(|f| f.id)
    }

    /// Empties input `id` (the chat line, once it was sent).
    pub fn clear_edit(&self, id: EditId) {
        if let Some(f) = self.field(id) {
            f.setStringValue(&NSString::from_str(""));
        }
    }

    /// What input `id` holds (`None` when it is not on screen).
    pub fn edit_text(&self, id: EditId) -> Option<String> {
        self.field(id).map(|f| f.stringValue().to_string())
    }

    /// Creates, moves, fills and hides the native fields to match the edit slots of the last paint. Never called
    /// inside `drawRect:`. No borrow of the field list is held across an AppKit call that can call the delegate back.
    pub fn place_fields(&self) {
        let iv = self.ivars();
        iv.placing.set(false);
        let Some(pal) = crate::shell::with_state(|st| st.palette()) else { return };
        // The fields are coloured again when the palette's surface changes (theme, "Increase contrast").
        let recolour = iv.fields_dark.replace(Some(pal.surface.hex())) != Some(pal.surface.hex());
        let slots: Vec<EditSlot> = iv.out.borrow().edits.clone();
        let known: Vec<Field> = iv.fields.borrow().clone();
        for slot in &slots {
            let found = known.iter().find(|f| f.id == slot.id).cloned();
            let created = found.is_none();
            let field = match found {
                Some(f) => f,
                None => {
                    let Some(f) = self.make_field(slot.id) else { continue };
                    iv.fields.borrow_mut().push(f.clone());
                    f
                }
            };
            let view = &field.view;
            if created || recolour {
                style_field(view, &pal, &slot.placeholder);
            }
            // One line of text, centred in the slot; the cell's own inset is taken back so the text starts where the
            // frame would draw it.
            let frame = NSRect::new(
                NSPoint::new(
                    f64::from(slot.rect.x) - CELL_INSET,
                    f64::from(slot.rect.y) + (f64::from(slot.rect.h) - field.height) / 2.0,
                ),
                NSSize::new(f64::from(slot.rect.w) + 2.0 * CELL_INSET, field.height),
            );
            if view.frame() != frame {
                view.setFrame(frame);
            }
            let was_hidden = view.isHidden();
            if was_hidden {
                view.setHidden(false);
            }
            let value = NSString::from_str(&slot.value);
            let renames = matches!(slot.id, EditId::Rename | EditId::ExpRename);
            if renames && (created || was_hidden) {
                // A rename field opens prefilled, with the keyboard in it and its text selected; the explorer's selects
                // the name without its extension (FILES-UI.md 3.2).
                view.setStringValue(&value);
                self.focus_control(ControlId::Edit(slot.id), false);
                if let Some(editor) = view.currentEditor() {
                    let end = if slot.id == EditId::ExpRename { stem_utf16(&slot.value) } else { value.length() };
                    editor.setSelectedRange(NSRange::new(0, end));
                }
            } else if created {
                view.setStringValue(&value);
            } else if !renames
                && view.currentEditor().is_none()
                && !slot.value.is_empty()
                && view.stringValue().to_string() != slot.value
            {
                // The state's value wins while nobody types; the rename field keeps what was typed.
                view.setStringValue(&value);
            }
        }
        for field in known.iter().filter(|f| !f.view.isHidden() && !slots.iter().any(|s| s.id == f.id)) {
            if field.view.currentEditor().is_some() {
                self.take_keyboard();
            }
            field.view.setHidden(true);
        }
    }

    /// A borderless, transparent, one-line field in the frame's body type, with this view as its delegate.
    fn make_field(&self, id: EditId) -> Option<Field> {
        let view = NSTextField::initWithFrame(NSTextField::alloc(self.mtm()), NSRect::ZERO);
        view.setBordered(false);
        view.setBezeled(false);
        view.setDrawsBackground(false);
        view.setFocusRingType(NSFocusRingType::None);
        view.setUsesSingleLineMode(true);
        if let Some(cell) = view.cell() {
            cell.setScrollable(true);
            cell.setWraps(false);
        }
        let font = FONTS.with(|fonts| fonts.font(text::BODY.size, text::BODY.weight))?;
        // SAFETY: a CTFont is toll-free bridged to NSFont; the painter's cache keeps it alive and the field retains it.
        view.setFont(Some(unsafe { &*font.get().cast::<NSFont>() }));
        // SAFETY: the delegate is this view, the field's superview, which outlives it; the property is weak.
        unsafe { view.setDelegate(Some(ProtocolObject::from_ref(self))) };
        let height = view.fittingSize().height;
        self.addSubview(&view);
        Some(Field { id, view, height })
    }

    /// Return, Tab, Shift+Tab and Esc typed in field `control`: `true` when the canvas handled the command.
    fn field_command(&self, control: &NSControl, command: Sel) -> bool {
        let Some(id) = self.edit_of(control) else { return false };
        if command == sel!(insertNewline:) {
            crate::shell::submit_edit(id, control.stringValue().to_string());
            if id == EditId::Chat {
                control.setStringValue(&NSString::from_str(""));
            }
            // A rename field keeps the keyboard: a refused name is corrected in place.
            if !matches!(id, EditId::Rename | EditId::ExpRename) {
                self.take_keyboard();
            }
            true
        } else if command == sel!(insertTab:) {
            self.move_focus(false);
            true
        } else if command == sel!(insertBacktab:) {
            self.move_focus(true);
            true
        } else if command == sel!(cancelOperation:) {
            match id {
                EditId::Rename => crate::shell::activate_from(self.ivars().kind, ControlId::RenameCancel),
                EditId::ExpRename => crate::shell::explorer_key(swapdesk_ui::ExplorerKey::Escape),
                EditId::Name => {
                    let name = crate::shell::with_state(|st| st.settings().map(|s| s.name.clone())).flatten();
                    if let Some(name) = name {
                        control.setStringValue(&NSString::from_str(&name));
                    }
                }
                EditId::Address | EditId::Chat => {}
            }
            self.take_keyboard();
            true
        } else {
            false
        }
    }

    /// The keyboard left a field: this computer's name is saved when its field is left, as on Windows.
    fn field_left(&self, notification: &NSNotification) {
        let Some(object) = notification.object() else { return };
        let Some(control) = object.downcast_ref::<NSControl>() else { return };
        if self.edit_of(control) == Some(EditId::Name) {
            crate::shell::submit_edit(EditId::Name, control.stringValue().to_string());
        }
    }

    // ----- development: the capture's field dump and the self-test's input -----

    /// One line per edit slot of the last paint: the slot, and the native field's frame and visibility.
    pub fn fields_dump(&self) -> String {
        let iv = self.ivars();
        let fields = iv.fields.borrow();
        let mut s = String::new();
        for slot in &iv.out.borrow().edits {
            let r = slot.rect;
            let _ = write!(s, "field {:?} slot {:.2} {:.2} {:.2} {:.2}", slot.id, r.x, r.y, r.w, r.h);
            let _ = match fields.iter().find(|f| f.id == slot.id) {
                Some(f) => {
                    let fr = f.view.frame();
                    writeln!(
                        s,
                        " frame {:.2} {:.2} {:.2} {:.2} {}",
                        fr.origin.x,
                        fr.origin.y,
                        fr.size.width,
                        fr.size.height,
                        if f.view.isHidden() { "hidden" } else { "visible" }
                    )
                }
                None => writeln!(s, " missing"),
            };
        }
        s
    }

    /// A mouse click (down and up) on control `id`, sent through the window as AppKit sends the mouse.
    pub fn send_click(&self, id: ControlId) -> bool {
        let (Some(rect), Some(window)) = (self.rect_of(id), self.window()) else { return false };
        let (x, y) = rect.center();
        let at = self.convertPoint_toView(NSPoint::new(f64::from(x), f64::from(y)), None);
        for kind in [NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind,
                at,
                NSEventModifierFlags::empty(),
                0.0,
                window.windowNumber(),
                None,
                0,
                1,
                1.0,
            );
            let Some(event) = event else { return false };
            window.sendEvent(&event);
        }
        true
    }

    /// Key presses sent through the window to whatever has the keyboard: each character of `text`, in order.
    pub fn send_keys(&self, text: &str) -> bool {
        let Some(window) = self.window() else { return false };
        for c in text.chars() {
            let code = key_code_of(c);
            let chars = NSString::from_str(&c.to_string());
            for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    kind,
                    NSPoint::new(0.0, 0.0),
                    NSEventModifierFlags::empty(),
                    0.0,
                    window.windowNumber(),
                    None,
                    &chars,
                    &chars,
                    false,
                    code,
                );
                let Some(event) = event else { return false };
                window.sendEvent(&event);
            }
        }
        true
    }
}

/// The UTF-16 length of a file name without its extension (all of it when it has none or starts with its only dot).
fn stem_utf16(name: &str) -> usize {
    let stem = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    stem.encode_utf16().count()
}

/// The virtual key code of the key that types `c` on the ANSI layout (`kVK_ANSI_*`, `kVK_Return`, `kVK_Tab`,
/// `kVK_Space`); 0 (the A key) for a character with no key of its own here. A text field reads the characters of a
/// key event, the video view sends the key itself to the host, so the sent keys must be the real ones.
fn key_code_of(c: char) -> u16 {
    const LETTERS: [u16; 26] =
        [0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    match c {
        'a'..='z' => LETTERS[c as usize - 'a' as usize],
        'A'..='Z' => LETTERS[c as usize - 'A' as usize],
        '0'..='9' => DIGITS[c as usize - '0' as usize],
        '\r' => KEY_RETURN,
        '\t' => KEY_TAB,
        '\u{1b}' => KEY_ESCAPE,
        ' ' => KEY_SPACE,
        '.' => 47,
        ',' => 43,
        '-' => 27,
        '/' => 44,
        _ => 0,
    }
}

/// Paints `view` (and what sits on it) into a bitmap at the window's resolution, through `drawRect:`. It needs
/// neither the screen nor the screen recording grant, so it works with the Mac locked. A layer filled from outside
/// `drawRect:` (the video) is not in it.
fn snapshot_of(view: &NSView) -> Option<Retained<NSBitmapImageRep>> {
    let bounds = view.bounds();
    let rep = view.bitmapImageRepForCachingDisplayInRect(bounds)?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    Some(rep)
}

/// Development (`--shot`): `view` as a PNG at `path`.
pub fn save_view_png(view: &NSView, path: &Path) -> bool {
    let Some(rep) = snapshot_of(view) else { return false };
    // SAFETY: PNG takes no properties: the dictionary is empty.
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) };
    data.is_some_and(|d| d.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true))
}

/// The colours of a field for the palette: ink for the text, soft for the placeholder (in the same face).
fn style_field(view: &NSTextField, pal: &Palette, placeholder: &str) {
    view.setTextColor(Some(&color(pal.ink)));
    let Some(font) = view.font() else { return };
    let soft = color(pal.soft);
    // SAFETY: AppKit's attribute name constants are immutable strings; each value has the type its key documents
    // (NSFont, NSColor).
    let text = unsafe {
        let keys = [NSFontAttributeName, NSForegroundColorAttributeName];
        let values: [&AnyObject; 2] = [font.as_ref(), soft.as_ref()];
        let attrs = NSDictionary::from_slices(&keys, &values);
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(placeholder),
            Some(&attrs),
        )
    };
    view.setPlaceholderAttributedString(Some(&text));
}
