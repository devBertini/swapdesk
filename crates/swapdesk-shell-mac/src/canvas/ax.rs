//! Accessibility of a canvas (C6): the tree `swapdesk_ui::a11y` builds for a window, the one Windows hands to UI
//! Automation, becomes AppKit accessibility elements here. The canvas is one painted view, so every node of the tree
//! gets an element of its own: a landmark per region, a tab group per run of tabs, and one element per control with
//! the control's name, state and rectangle. A text input is the native field that sits in its slot (its cell, the
//! element AppKit exposes for a control), named after the control. Pressing an element activates the control as a click does.
//!
//! The elements are built when an assistive client first asks for them and kept current after every paint from then
//! on; an element keeps its identity while its control stays in the tree, so a client's cursor stays where it was.
//!
//! Roles: AppKit has no heading or landmark role of its own, so those use the names WebKit and SwiftUI expose and
//! VoiceOver knows (`AXHeading`, `AXLandmarkNavigation`, ...). The activity table is a labelled group followed by
//! its rows as texts: AppKit's `AXTable` and `AXRow` need the rows as children with the table protocols, and the
//! shared tree lists the rows beside the table.
//!
//! Apple documentation: `NSAccessibilityElement` and the `NSAccessibility` protocol
//! (developer.apple.com/documentation/appkit/nsaccessibilityelement,
//! developer.apple.com/documentation/appkit/nsaccessibilityprotocol), `NSAccessibilityUnignoredAncestor` and
//! `NSAccessibilityPostNotification` (developer.apple.com/documentation/appkit/nsaccessibility-swift.struct), the
//! role and subrole constants (developer.apple.com/documentation/appkit/nsaccessibility/role).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Write as _;

use accesskit::{Action, AriaCurrent, Node, NodeId, Role, Toggled};
use dispatch2::DispatchQueue;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification,
    NSAccessibilityElement, NSAccessibilityFocusedUIElementChangedNotification,
    NSAccessibilityLayoutChangedNotification, NSAccessibilityPostNotification,
    NSAccessibilityPostNotificationWithUserInfo, NSAccessibilityPriorityKey, NSAccessibilityPriorityLevel,
    NSAccessibilityUnignoredAncestor, NSAccessibilityUnignoredDescendant, NSAccessibilityValueChangedNotification,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use swapdesk_paint::{Rect, input};
use swapdesk_ui::ControlId;
use swapdesk_ui::a11y;

use crate::model::{LiveAnnouncer, live_region_text};

use super::CanvasView;

/// The AppKit role and subrole of a node of the shared tree; `toggles` when the node is on or off.
fn ax_role(role: Role, toggles: bool) -> (&'static str, Option<&'static str>) {
    match role {
        // A button that stays on or off (the viewer's toolbar) is AppKit's toggle.
        Role::Button if toggles => ("AXButton", Some("AXToggle")),
        Role::Button => ("AXButton", None),
        Role::Tab => ("AXRadioButton", Some("AXTabButton")),
        Role::Switch => ("AXCheckBox", Some("AXSwitch")),
        Role::CheckBox => ("AXCheckBox", None),
        Role::Link => ("AXLink", None),
        Role::ComboBox => ("AXPopUpButton", None),
        Role::TextInput => ("AXTextField", None),
        Role::Heading => ("AXHeading", None),
        Role::ProgressIndicator => ("AXProgressIndicator", None),
        Role::TabList => ("AXTabGroup", None),
        Role::Toolbar => ("AXToolbar", None),
        Role::Navigation => ("AXGroup", Some("AXLandmarkNavigation")),
        Role::Main => ("AXGroup", Some("AXLandmarkMain")),
        Role::Complementary => ("AXGroup", Some("AXLandmarkComplementary")),
        Role::Dialog => ("AXGroup", Some("AXApplicationDialog")),
        Role::Status => ("AXGroup", Some("AXApplicationStatus")),
        Role::Table => ("AXGroup", None),
        // A text, and what the tree lists as a row or a cell: a line of text to read.
        _ => ("AXStaticText", None),
    }
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn object<T: AsRef<AnyObject>>(of: &Retained<T>) -> Retained<AnyObject> {
    let o: &AnyObject = (**of).as_ref();
    o.retain()
}

fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let (x, y) = (a.x.min(b.x), a.y.min(b.y));
            let (r, bt) = ((a.x + a.w).max(b.x + b.w), (a.y + a.h).max(b.y + b.h));
            Some(Rect { x, y, w: r - x, h: bt - y })
        }
        (a, b) => a.or(b),
    }
}

fn same(a: &AnyObject, b: &AnyObject) -> bool {
    std::ptr::from_ref(a).cast::<c_void>() == std::ptr::from_ref(b).cast::<c_void>()
}

pub struct AxIvars {
    canvas: Weak<CanvasView>,
    /// The control this element stands for; none for a landmark or a tab group.
    control: Cell<Option<ControlId>>,
    /// Pressing it activates the control.
    press: Cell<bool>,
    /// Where it is in the canvas (DIPs, y down): the control's rectangle, or what its children cover.
    rect: Cell<Option<Rect>>,
    /// The state last set (value, toggled, selected), to tell a client when it changes.
    said: RefCell<String>,
}

define_class!(
    // SAFETY: NSAccessibilityElement is made to be subclassed and has no requirements beyond calling super's `init`;
    // this class does not implement `Drop`.
    #[unsafe(super = NSAccessibilityElement)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskAxElement"]
    #[ivars = AxIvars]
    pub struct AxElement;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for AxElement {}

    impl AxElement {
        // SAFETY (all methods below): the selectors and signatures are those of the NSAccessibility protocol.

        // The frame in screen coordinates, computed when asked: it follows the window as it moves.
        #[unsafe(method(accessibilityFrame))]
        fn frame(&self) -> NSRect {
            let iv = self.ivars();
            match (iv.canvas.load(), iv.rect.get()) {
                (Some(canvas), Some(r)) => canvas.screen_rect(r),
                _ => NSRect::ZERO,
            }
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn perform_press(&self) -> bool {
            let iv = self.ivars();
            match (iv.canvas.load(), iv.control.get(), iv.press.get()) {
                (Some(canvas), Some(id), true) => {
                    let kind = canvas.ivars().kind;
                    // The client gets its answer first: an activation may open a menu or a panel, which runs its
                    // own event loop until the person is done.
                    DispatchQueue::main().exec_async(move || crate::shell::activate_from(kind, id));
                    true
                }
                _ => false,
            }
        }

        // A text, a heading or a landmark offers no press.
        #[unsafe(method(isAccessibilitySelectorAllowed:))]
        fn selector_allowed(&self, selector: Sel) -> bool {
            if selector == sel!(accessibilityPerformPress) {
                self.ivars().press.get()
            } else {
                // SAFETY: forwards the same selector to NSAccessibilityElement's implementation.
                unsafe { msg_send![super(self), isAccessibilitySelectorAllowed: selector] }
            }
        }

        #[unsafe(method(isAccessibilityFocused))]
        fn focused(&self) -> bool {
            let iv = self.ivars();
            match (iv.canvas.load(), iv.control.get()) {
                (Some(canvas), Some(id)) => canvas.focus() == Some(id),
                _ => false,
            }
        }

        // A client moving the keyboard focus to a control: as Tab would, with the ring shown.
        #[unsafe(method(setAccessibilityFocused:))]
        fn set_focused(&self, focused: bool) {
            let iv = self.ivars();
            if let (true, Some(canvas), Some(id)) = (focused, iv.canvas.load(), iv.control.get())
                && id.focusable()
            {
                canvas.focus_control(id, true);
            }
        }
    }
);

impl AxElement {
    fn create(canvas: &CanvasView) -> Retained<AxElement> {
        let this = Self::alloc(canvas.mtm()).set_ivars(AxIvars {
            canvas: Weak::new(canvas),
            control: Cell::new(None),
            press: Cell::new(false),
            rect: Cell::new(None),
            said: RefCell::default(),
        });
        // SAFETY: `init` is NSAccessibilityElement's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// The elements of a canvas, as last built.
#[derive(Default)]
pub(super) struct AxState {
    /// The frame was painted again since the elements were built.
    pub(super) stale: bool,
    /// An assistive client asked for the elements: they follow every paint from now on.
    live: bool,
    /// The landmarks, the canvas's own children.
    top: Vec<Retained<AnyObject>>,
    elements: Vec<(NodeId, Retained<AxElement>)>,
    /// What stands for each control: its element, or the native field of a text input.
    controls: Vec<(ControlId, Retained<AnyObject>)>,
    /// The tree's shape and where each node is; a change is told to the clients.
    shape: Vec<(NodeId, Vec<NodeId>, Option<[i32; 4]>)>,
    focus: Option<ControlId>,
    /// The live region's text as last synced: a change is announced, a repaint or a countdown tick is not.
    announcer: LiveAnnouncer,
}

/// Tells the clients of `element`'s window to read `text` (a polite announcement: it waits for the speech in progress).
fn post_announcement(element: &AnyObject, text: &str) {
    // SAFETY: the keys and the notification name are AppKit's immutable constants; the dictionary holds a string and
    // a number, as the notification documents; `element` is an accessibility element.
    unsafe {
        let keys = [NSAccessibilityAnnouncementKey, NSAccessibilityPriorityKey];
        let values: Vec<Retained<AnyObject>> =
            vec![object(&ns(text)), object(&NSNumber::new_isize(NSAccessibilityPriorityLevel::Medium.0))];
        let info = NSDictionary::from_retained_objects(&keys, &values);
        NSAccessibilityPostNotificationWithUserInfo(
            element,
            NSAccessibilityAnnouncementRequestedNotification,
            Some(&info),
        );
    }
}

struct Build<'a> {
    canvas: &'a CanvasView,
    nodes: HashMap<NodeId, &'a Node>,
    ids: &'a [(NodeId, ControlId)],
    old: &'a [(NodeId, Retained<AxElement>)],
    elements: Vec<(NodeId, Retained<AxElement>)>,
    controls: Vec<(ControlId, Retained<AnyObject>)>,
    shape: Vec<(NodeId, Vec<NodeId>, Option<[i32; 4]>)>,
    /// The elements kept from the last build whose state changed.
    changed: Vec<Retained<AxElement>>,
}

impl Build<'_> {
    /// The element of node `id` under `parent`, with everything below it, and the rectangle it covers.
    fn node(&mut self, id: NodeId, parent: &AnyObject) -> Option<(Retained<AnyObject>, Option<Rect>)> {
        let node = *self.nodes.get(&id)?;
        let control = self.ids.iter().find(|(n, _)| *n == id).map(|(_, c)| *c);
        let label = node.label().unwrap_or_default();
        let help = node.description().map(ns);
        let bounds = node.bounds().map(|b| Rect {
            x: b.x0 as f32,
            y: b.y0 as f32,
            w: (b.x1 - b.x0) as f32,
            h: (b.y1 - b.y0) as f32,
        });

        // A text input is the native field in its slot: AppKit already tells its text, selection and editing. The
        // element of a control is the one AppKit does not ignore, its cell.
        if let Some(ControlId::Edit(edit)) = control
            && let Some(field) = self.canvas.field(edit)
        {
            let view: &AnyObject = field.as_ref();
            // SAFETY: the view is an accessibility object (every NSView is).
            let object = unsafe { NSAccessibilityUnignoredDescendant(view) }.unwrap_or_else(|| view.retain());
            let name = ns(label);
            // SAFETY: the setters of the NSAccessibility protocol, which a view and a cell both implement, with the
            // types it documents; the parent is a landmark of this canvas and the property is weak.
            unsafe {
                let _: () = msg_send![&*object, setAccessibilityLabel: &*name];
                let _: () = msg_send![&*object, setAccessibilityHelp: help.as_deref()];
                let _: () = msg_send![&*object, setAccessibilityParent: parent];
            }
            let f = field.frame();
            let rect =
                Rect { x: f.origin.x as f32, y: f.origin.y as f32, w: f.size.width as f32, h: f.size.height as f32 };
            self.controls.push((ControlId::Edit(edit), object.clone()));
            self.shape.push((id, Vec::new(), Some(grid(rect))));
            return Some((object, Some(rect)));
        }

        let kept = self.old.iter().find(|(n, _)| *n == id).map(|(_, e)| e.clone());
        let fresh = kept.is_none();
        let el = kept.unwrap_or_else(|| AxElement::create(self.canvas));
        let (role, subrole) = ax_role(node.role(), node.toggled().is_some());
        el.setAccessibilityRole(Some(&ns(role)));
        el.setAccessibilitySubrole(subrole.map(ns).as_deref());
        let selected = node.is_selected() == Some(true) || node.aria_current() == Some(AriaCurrent::True);
        let toggled = node.toggled().map(|t| t == Toggled::True);
        // What a client reads as the value: the text itself for a text, on or off for a switch, selected or not for
        // a tab, else the control's own value (the option a menu shows, the time left).
        let value: Option<Retained<AnyObject>> = if role == "AXStaticText" {
            Some(object(&ns(label)))
        } else if let Some(on) = toggled {
            Some(object(&NSNumber::new_i32(i32::from(on))))
        } else if node.role() == Role::Tab {
            Some(object(&NSNumber::new_i32(i32::from(selected))))
        } else if node.role() == Role::Heading {
            // VoiceOver reads a heading's level from its value, a number (WebKit's AXHeading, LayoutTests
            // accessibility/mac/element-level.html).
            Some(object(&NSNumber::new_i32(i32::try_from(node.level().unwrap_or(1)).unwrap_or(1))))
        } else {
            node.value().map(|v| object(&ns(v)))
        };
        if role == "AXStaticText" {
            el.setAccessibilityLabel(None);
        } else {
            el.setAccessibilityLabel(Some(&ns(label)));
        }
        // SAFETY: the value is a string or a number, as the role documents.
        unsafe { el.setAccessibilityValue(value.as_deref()) };
        el.setAccessibilityHelp(help.as_deref());
        el.setAccessibilitySelected(selected);
        el.setAccessibilityEnabled(!node.is_disabled());
        el.setAccessibilityModal(node.is_modal());
        // SAFETY: the parent is an accessibility element (the canvas's unignored ancestor or an element of this
        // tree); the property is weak, so no cycle is made.
        unsafe { el.setAccessibilityParent(Some(parent)) };

        let me: &AnyObject = el.as_ref();
        let mut children: Vec<Retained<AnyObject>> = Vec::new();
        let mut child_ids = Vec::new();
        let mut covered = None;
        for child in node.children() {
            if let Some((object, rect)) = self.node(*child, me) {
                children.push(object);
                child_ids.push(*child);
                covered = union(covered, rect);
            }
        }
        // SAFETY: every child is an accessibility element (an `AxElement` or a text field).
        unsafe { el.setAccessibilityChildren(Some(&NSArray::from_retained_slice(&children))) };

        let rect = bounds.or(covered);
        let iv = el.ivars();
        iv.control.set(control);
        iv.press.set(node.supports_action(Action::Click));
        iv.rect.set(rect);
        let said = format!("{toggled:?}|{selected}|{:?}", node.value());
        if std::mem::replace(&mut *iv.said.borrow_mut(), said.clone()) != said && !fresh {
            self.changed.push(el.clone());
        }
        if let Some(c) = control {
            self.controls.push((c, me.retain()));
        }
        self.shape.push((id, child_ids, rect.map(grid)));
        self.elements.push((id, el.clone()));
        Some((me.retain(), rect))
    }
}

/// The accessibility children of `el`, as a client is given them.
pub fn ax_children_of(el: &AnyObject) -> Vec<Retained<AnyObject>> {
    // SAFETY: the getter of the NSAccessibility protocol, which every accessibility object answers with an array of
    // elements or nil.
    let children: Option<Retained<NSArray<AnyObject>>> = unsafe { msg_send![el, accessibilityChildren] };
    children.map(|c| c.to_vec()).unwrap_or_default()
}

/// A rectangle in tenths of a DIP, to tell when an element moved.
fn grid(r: Rect) -> [i32; 4] {
    [r.x, r.y, r.w, r.h].map(|v| (v * 10.0).round() as i32)
}

/// One accessibility element as a client reads it, for the checks and the `--shot` dump.
pub struct AxLine {
    pub depth: usize,
    /// Role, name and state, in the layout of `swapdesk_ui::a11y::dump`.
    pub text: String,
    /// Its frame in the canvas (x, y, width, height in DIPs, y down); none when it has no frame.
    pub rect: Option<[f64; 4]>,
    /// Its parent is the element it was listed under.
    pub parent_ok: bool,
}

impl CanvasView {
    /// `r` (canvas coordinates) on the screen.
    fn screen_rect(&self, r: Rect) -> NSRect {
        let local =
            NSRect::new(NSPoint::new(f64::from(r.x), f64::from(r.y)), NSSize::new(f64::from(r.w), f64::from(r.h)));
        let in_window = self.convertRect_toView(local, None);
        match self.window() {
            Some(w) => w.convertRectToScreen(in_window),
            None => in_window,
        }
    }

    /// A screen rectangle in the canvas's coordinates.
    fn local_rect(&self, screen: NSRect) -> NSRect {
        let in_window = match self.window() {
            Some(w) => w.convertRectFromScreen(screen),
            None => screen,
        };
        self.convertRect_fromView(in_window, None)
    }

    /// The element the landmarks hang under: the canvas is not an element itself, so it is the nearest one above
    /// (the window).
    pub fn ax_parent(&self) -> Retained<AnyObject> {
        let me: &AnyObject = self.as_ref();
        // SAFETY: the view is an accessibility object (every NSView is).
        unsafe { NSAccessibilityUnignoredAncestor(me) }.unwrap_or_else(|| me.retain())
    }

    /// Builds the elements again from the shared tree of the state as last painted. `announce` tells the clients
    /// what changed. Nothing happens while the shell is changing the state: the paint that follows comes back here.
    fn ax_sync(&self, announce: bool) {
        let iv = self.ivars();
        let kind = iv.kind;
        let focus = self.focus();
        let built = crate::shell::with_state(|st| {
            let out = iv.out.borrow();
            let bounds = |id: ControlId| {
                out.widgets.iter().find(|w| w.id == id).map(|w| {
                    let r = w.rect;
                    (f64::from(r.x), f64::from(r.y), f64::from(r.x + r.w), f64::from(r.y + r.h))
                })
            };
            let tree = a11y::build_tree(st, kind, &bounds, focus);
            let ids: Vec<(NodeId, ControlId)> = st.controls(kind).iter().map(|c| (a11y::node_id(c.id), c.id)).collect();
            (tree, ids, live_region_text(st, kind))
        });
        let Some((tree, ids, live_text)) = built else { return };
        // No borrow of the elements is held while AppKit is called.
        let mut old = std::mem::take(&mut *iv.ax.borrow_mut());
        let announcement = old.announcer.changed(&live_text);
        let parent = self.ax_parent();
        let mut b = Build {
            canvas: self,
            nodes: tree.nodes.iter().map(|(i, n)| (*i, n)).collect(),
            ids: &ids,
            old: &old.elements,
            elements: Vec::new(),
            controls: Vec::new(),
            shape: Vec::new(),
            changed: Vec::new(),
        };
        // The window is the root: its landmarks are the canvas's children.
        let regions: Vec<NodeId> = b.nodes.get(&a11y::ROOT).map(|r| r.children().to_vec()).unwrap_or_default();
        let top: Vec<Retained<AnyObject>> =
            regions.iter().filter_map(|r| b.node(*r, &parent)).map(|(object, _)| object).collect();
        let (elements, controls, shape, changed) = (b.elements, b.controls, b.shape, b.changed);
        let moved = old.shape != shape;
        let refocused = old.focus != focus;
        let focused = focus.and_then(|f| controls.iter().find(|(c, _)| *c == f).map(|(_, o)| o.clone()));
        *iv.ax.borrow_mut() =
            AxState { stale: false, live: old.live, top, elements, controls, shape, focus, announcer: old.announcer };
        if !announce {
            return;
        }
        if let Some(text) = &announcement {
            post_announcement(&parent, text);
        }
        // SAFETY: each object is an accessibility element; the notification names are AppKit's immutable constants.
        unsafe {
            if moved {
                NSAccessibilityPostNotification(&parent, NSAccessibilityLayoutChangedNotification);
            }
            for el in &changed {
                let object: &AnyObject = el.as_ref();
                NSAccessibilityPostNotification(object, NSAccessibilityValueChangedNotification);
            }
            if refocused && let Some(object) = &focused {
                NSAccessibilityPostNotification(object, NSAccessibilityFocusedUIElementChangedNotification);
            }
        }
    }

    /// A client asked: the elements exist from now on and are current.
    fn ax_wanted(&self) {
        let stale = {
            let mut ax = self.ivars().ax.borrow_mut();
            let first = !std::mem::replace(&mut ax.live, true);
            first || ax.stale
        };
        if stale {
            self.ax_sync(false);
        }
    }

    /// After a paint and the placement of the text fields: the elements follow, once a client has asked for them.
    pub fn ax_after_paint(&self) {
        if self.ivars().ax.borrow().live {
            self.ax_sync(true);
        }
        let dump = self.ivars().ax_dump.borrow_mut().take();
        if let Some(path) = dump {
            let _ = std::fs::write(path, self.ax_dump());
        }
    }

    /// The canvas's accessibility children: the landmarks of the window.
    pub(super) fn ax_children(&self) -> Retained<NSArray<AnyObject>> {
        self.ax_wanted();
        NSArray::from_retained_slice(&self.ivars().ax.borrow().top)
    }

    /// The element of the control under a point of the screen.
    pub(super) fn ax_hit(&self, screen: NSPoint) -> Option<Retained<AnyObject>> {
        self.ax_wanted();
        let p = self.local_rect(NSRect::new(screen, NSSize::new(0.0, 0.0))).origin;
        let hit = input::hit_test(&self.ivars().out.borrow().widgets, p.x as f32, p.y as f32)?;
        self.ax_element_of(hit)
    }

    /// The element of the control that has the focus.
    pub(super) fn ax_focused(&self) -> Option<Retained<AnyObject>> {
        self.ax_wanted();
        self.ax_element_of(self.focus()?)
    }

    /// What stands for control `id` in the accessibility tree: its element, or the native field of a text input.
    pub fn ax_element_of(&self, id: ControlId) -> Option<Retained<AnyObject>> {
        self.ax_wanted();
        self.ivars().ax.borrow().controls.iter().find(|(c, _)| *c == id).map(|(_, o)| o.clone())
    }

    // ----- development: the tree as a client reads it -----

    /// Every element under the window, read through the accessibility protocol as an assistive client reads it:
    /// first the window, then the landmarks and what they hold.
    pub fn ax_lines(&self) -> Vec<AxLine> {
        let parent = self.ax_parent();
        let mut out = Vec::new();
        // SAFETY: the accessibility getters of an accessibility object, with the types the protocol documents.
        let (role, title): (Option<Retained<NSString>>, Option<Retained<NSString>>) =
            unsafe { (msg_send![&*parent, accessibilityRole], msg_send![&*parent, accessibilityTitle]) };
        out.push(AxLine {
            depth: 0,
            text: format!(
                "{} \"{}\"",
                role.map(|r| r.to_string()).unwrap_or_default(),
                title.map(|t| t.to_string()).unwrap_or_default()
            ),
            rect: None,
            parent_ok: true,
        });
        let children = self.ax_children();
        for child in children.to_vec() {
            self.ax_describe(&child, &parent, 1, &mut out);
        }
        out
    }

    fn ax_describe(&self, el: &AnyObject, parent: &AnyObject, depth: usize, out: &mut Vec<AxLine>) {
        type Text = Option<Retained<NSString>>;
        type Object = Option<Retained<AnyObject>>;
        // SAFETY: `el` is an accessibility element (an `AxElement` or a text field); these are the getters of the
        // NSAccessibility protocol with the types it documents.
        let (role, subrole, label, help): (Text, Text, Text, Text) = unsafe {
            (
                msg_send![el, accessibilityRole],
                msg_send![el, accessibilitySubrole],
                msg_send![el, accessibilityLabel],
                msg_send![el, accessibilityHelp],
            )
        };
        // SAFETY: as above.
        let (value, up): (Object, Object) =
            unsafe { (msg_send![el, accessibilityValue], msg_send![el, accessibilityParent]) };
        // SAFETY: as above.
        let (selected, press, frame): (bool, bool, NSRect) = unsafe {
            (
                msg_send![el, isAccessibilitySelected],
                msg_send![el, isAccessibilitySelectorAllowed: sel!(accessibilityPerformPress)],
                msg_send![el, accessibilityFrame],
            )
        };
        let role = role.map(|r| r.to_string()).unwrap_or_default();
        // A native text field is AppKit's own element: what its cell allows is the same for every text field, and
        // the control it stands for takes the keyboard, not a press.
        let press = press && role != "AXTextField";
        let text_value = value.as_deref().and_then(|v| v.downcast_ref::<NSString>()).map(|s| s.to_string());
        let number = value.as_deref().and_then(|v| v.downcast_ref::<NSNumber>()).map(|n| n.as_i32());
        let label = label.map(|l| l.to_string()).filter(|l| !l.is_empty());
        // A text's name is its value.
        let is_text = role == "AXStaticText";
        let name = if is_text { text_value.clone().unwrap_or_default() } else { label.unwrap_or_default() };
        let subrole = subrole.map(|s| s.to_string()).filter(|s| !s.is_empty());
        let mut text = role.clone();
        if let Some(s) = &subrole {
            let _ = write!(text, "/{s}");
        }
        let _ = write!(text, " \"{name}\"");
        if (role == "AXCheckBox" || subrole.as_deref() == Some("AXToggle"))
            && let Some(n) = number
        {
            let _ = write!(text, " toggled={}", if n != 0 { "True" } else { "False" });
        }
        if selected {
            text.push_str(" selected");
        }
        if !is_text && let Some(v) = text_value.filter(|v| !v.is_empty()) {
            let _ = write!(text, " value=\"{v}\"");
        }
        if let Some(h) = help.map(|h| h.to_string()).filter(|h| !h.is_empty()) {
            let _ = write!(text, " description=\"{h}\"");
        }
        if press {
            text.push_str(" [click]");
        }
        let rect = (frame.size.width > 0.0 && frame.size.height > 0.0).then(|| {
            let r = self.local_rect(frame);
            [r.origin.x, r.origin.y, r.size.width, r.size.height]
        });
        out.push(AxLine { depth, text, rect, parent_ok: up.is_some_and(|u| same(&u, parent)) });
        // A text field's own children (its editor, while it is typed in) are AppKit's.
        if role == "AXTextField" {
            return;
        }
        for child in ax_children_of(el) {
            self.ax_describe(&child, el, depth + 1, out);
        }
    }

    /// The tree as text, one line per element, in the layout of `swapdesk_ui::a11y::dump`.
    pub fn ax_dump(&self) -> String {
        let mut s = String::new();
        for line in self.ax_lines() {
            let _ = write!(s, "{}{}", "  ".repeat(line.depth), line.text);
            if let Some([x, y, w, h]) = line.rect {
                let _ = write!(s, " @({x:.0},{y:.0} {w:.0}x{h:.0})");
            }
            s.push('\n');
        }
        s
    }
}
