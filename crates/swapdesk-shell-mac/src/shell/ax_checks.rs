//! Window checks of the accessibility tree (run with the others, see `selftest`): what an assistive client reads in
//! every window of every scenario is the tree `swapdesk_ui::a11y` builds for it, and an element answers a press, the
//! focus and a hit test as its control does.
//!
//! The expected role names come from AppKit's own constants where AppKit has one; the canvas writes them as texts.

use accesskit::{Action, AriaCurrent, Node, NodeId, Role, Toggled, TreeUpdate};
use objc2_app_kit::{
    NSAccessibilityButtonRole, NSAccessibilityCheckBoxRole, NSAccessibilityGroupRole, NSAccessibilityLinkRole,
    NSAccessibilityPopUpButtonRole, NSAccessibilityProgressIndicatorRole, NSAccessibilityRadioButtonRole,
    NSAccessibilityStaticTextRole, NSAccessibilitySwitchSubrole, NSAccessibilityTabButtonSubrole,
    NSAccessibilityTabGroupRole, NSAccessibilityTextFieldRole, NSAccessibilityToggleSubrole,
    NSAccessibilityToolbarRole, NSAccessibilityWindowRole,
};
use objc2_foundation::{NSDate, NSPoint, NSRunLoop};
use swapdesk_ui::mock::Scenario;
use swapdesk_ui::{Region, UiRole, a11y};

use super::selftest::{close, open};
use super::*;

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

/// The AppKit role and subrole a node of the shared tree must be read as; `toggles` when the node is on or off.
fn ax_names(role: Role, toggles: bool) -> (String, Option<String>) {
    // SAFETY: AppKit's role and subrole constants are immutable strings.
    unsafe {
        match role {
            Role::Window => (NSAccessibilityWindowRole.to_string(), None),
            Role::Button if toggles => {
                (NSAccessibilityButtonRole.to_string(), Some(NSAccessibilityToggleSubrole.to_string()))
            }
            Role::Button => (NSAccessibilityButtonRole.to_string(), None),
            Role::Tab => {
                (NSAccessibilityRadioButtonRole.to_string(), Some(NSAccessibilityTabButtonSubrole.to_string()))
            }
            Role::Switch => (NSAccessibilityCheckBoxRole.to_string(), Some(NSAccessibilitySwitchSubrole.to_string())),
            Role::CheckBox => (NSAccessibilityCheckBoxRole.to_string(), None),
            Role::Link => (NSAccessibilityLinkRole.to_string(), None),
            Role::ComboBox => (NSAccessibilityPopUpButtonRole.to_string(), None),
            Role::TextInput => (NSAccessibilityTextFieldRole.to_string(), None),
            Role::ProgressIndicator => (NSAccessibilityProgressIndicatorRole.to_string(), None),
            Role::TabList => (NSAccessibilityTabGroupRole.to_string(), None),
            Role::Toolbar => (NSAccessibilityToolbarRole.to_string(), None),
            Role::Table => (NSAccessibilityGroupRole.to_string(), None),
            // AppKit has no constant for a heading or a landmark: the names WebKit and SwiftUI expose.
            Role::Heading => ("AXHeading".to_owned(), None),
            Role::Navigation => (NSAccessibilityGroupRole.to_string(), Some("AXLandmarkNavigation".to_owned())),
            Role::Main => (NSAccessibilityGroupRole.to_string(), Some("AXLandmarkMain".to_owned())),
            Role::Complementary => (NSAccessibilityGroupRole.to_string(), Some("AXLandmarkComplementary".to_owned())),
            Role::Dialog => (NSAccessibilityGroupRole.to_string(), Some("AXApplicationDialog".to_owned())),
            Role::Status => (NSAccessibilityGroupRole.to_string(), Some("AXApplicationStatus".to_owned())),
            Role::Label | Role::Row | Role::Cell | Role::ColumnHeader => {
                (NSAccessibilityStaticTextRole.to_string(), None)
            }
            other => (format!("no AppKit role for {other:?}"), None),
        }
    }
}

struct Expected {
    depth: usize,
    text: String,
    /// x, y, width, height in the canvas.
    rect: Option<[f64; 4]>,
}

/// The shared tree as the lines the Mac elements must read as (the layout of `a11y::dump`, with AppKit's roles).
fn expected(tree: &TreeUpdate) -> Vec<Expected> {
    fn walk(tree: &TreeUpdate, id: NodeId, depth: usize, out: &mut Vec<Expected>) {
        let Some(n): Option<&Node> = tree.nodes.iter().find(|(i, _)| *i == id).map(|(_, n)| n) else { return };
        let (role, subrole) = ax_names(n.role(), n.toggled().is_some());
        let mut text = role;
        if let Some(s) = subrole {
            text.push_str(&format!("/{s}"));
        }
        text.push_str(&format!(" \"{}\"", n.label().unwrap_or("")));
        if let Some(t) = n.toggled() {
            text.push_str(if t == Toggled::True { " toggled=True" } else { " toggled=False" });
        }
        if n.is_selected() == Some(true) || n.aria_current() == Some(AriaCurrent::True) {
            text.push_str(" selected");
        }
        if let Some(v) = n.value().filter(|v| !v.is_empty()) {
            text.push_str(&format!(" value=\"{v}\""));
        }
        if let Some(d) = n.description() {
            text.push_str(&format!(" description=\"{d}\""));
        }
        if n.supports_action(Action::Click) {
            text.push_str(" [click]");
        }
        let rect = n.bounds().map(|b| [b.x0, b.y0, b.x1 - b.x0, b.y1 - b.y0]);
        out.push(Expected { depth, text, rect });
        for c in n.children() {
            walk(tree, *c, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(tree, a11y::ROOT, 0, &mut out);
    out
}

/// The elements of `canvas`, read through the accessibility protocol, are the shared tree of window `kind`: the
/// same nodes in the same order and depth, each with its role, name, state, press and rectangle, and each listed
/// under its own parent.
fn reads_as_the_shared_tree(
    shell: &Shell,
    canvas: &CanvasView,
    kind: WindowKind,
    scenario: &str,
) -> Result<(), String> {
    let lines = canvas.ax_lines();
    let tree = {
        let state = shell.state.borrow();
        let bounds = |id: ControlId| {
            canvas.rect_of(id).map(|r| (f64::from(r.x), f64::from(r.y), f64::from(r.x + r.w), f64::from(r.y + r.h)))
        };
        a11y::build_tree(&state, kind, &bounds, canvas.focus())
    };
    // The window itself lists the landmarks: the canvas is not an element, so AppKit hands its children to the
    // element above it, where a client starts.
    let window = canvas.ax_parent();
    let listed = crate::canvas::ax_children_of(&window);
    let view: &AnyObject = canvas.as_ref();
    let landmarks = crate::canvas::ax_children_of(view);
    ensure!(!landmarks.is_empty(), "{scenario} {kind:?}: the canvas has no landmark");
    for landmark in &landmarks {
        ensure!(
            listed.iter().any(|l| std::ptr::eq(Retained::as_ptr(l), Retained::as_ptr(landmark))),
            "{scenario} {kind:?}: the window does not list a landmark of its canvas among its {} children",
            listed.len()
        );
    }
    let problems = a11y::validate(&tree);
    ensure!(problems.is_empty(), "{scenario} {kind:?}: the shared tree has problems: {problems:?}");
    let want = expected(&tree);
    let at = |i: usize| format!("{scenario} {kind:?}, element {i}");
    for (i, (got, want)) in lines.iter().zip(&want).enumerate() {
        ensure!(
            got.depth == want.depth && got.text == want.text,
            "{}: read as\n  {}{}\nthe shared tree has\n  {}{}",
            at(i),
            "  ".repeat(got.depth),
            got.text,
            "  ".repeat(want.depth),
            want.text
        );
        ensure!(got.parent_ok, "{}: '{}' does not name the element above it as its parent", at(i), got.text);
        let Some(w) = want.rect else { continue };
        let Some(g) = got.rect else {
            return Err(format!("{}: '{}' has no frame, the control is at {w:?}", at(i), got.text));
        };
        let equal = g.iter().zip(&w).all(|(a, b)| (a - b).abs() < 0.01);
        // A native text field is the one line of text inside the box the frame drew for the input.
        let inside = got.text.starts_with("AXTextField")
            && g[0] >= w[0] - 3.0
            && g[1] >= w[1] - 3.0
            && g[0] + g[2] <= w[0] + w[2] + 3.0
            && g[1] + g[3] <= w[1] + w[3] + 3.0;
        ensure!(equal || inside, "{}: '{}' is at {g:?}, the control at {w:?}", at(i), got.text);
    }
    ensure!(
        lines.len() == want.len(),
        "{scenario} {kind:?}: {} elements read, the shared tree has {} nodes:\n{}",
        lines.len(),
        want.len(),
        canvas.ax_dump()
    );
    Ok(())
}

/// In every scenario, every window (main, viewer, banner) reads as its shared tree.
fn every_window_reads_as_the_shared_tree(mtm: MainThreadMarker) -> Result<(), String> {
    let mut windows = 0;
    for sc in Scenario::ALL {
        let shell = open(mtm, sc, ViewerHooks::default());
        let kinds = shell.state.borrow().windows();
        for kind in kinds {
            let Some(canvas) = shell.canvas(kind) else {
                return Err(format!("{}: the {kind:?} window has no canvas", sc.name()));
            };
            ensure!(canvas.paint_now(), "{}: the {kind:?} canvas did not paint", sc.name());
            reads_as_the_shared_tree(&shell, &canvas, kind, sc.name())?;
            windows += 1;
        }
        close(&shell);
    }
    ensure!(windows > Scenario::ALL.len(), "only {windows} windows were read: no viewer or banner among them");
    Ok(())
}

/// Lets the main queue run its blocks for a moment.
fn turn_the_run_loop() {
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
}

/// An element answers as its control: a hit test at the control's centre finds it, the focus of the canvas is the
/// focused element, a press on a sidebar item opens that screen, and a text offers no press.
fn elements_answer_as_their_controls(mtm: MainThreadMarker) -> Result<(), String> {
    let shell = open(mtm, Scenario::Inicio, ViewerHooks::default());
    let canvas = shell.main.canvas.clone();
    ensure!(canvas.paint_now(), "the canvas did not paint");
    let Some(window) = canvas.window() else { return Err("the canvas has no window".into()) };
    let same = |a: &AnyObject, b: &AnyObject| {
        std::ptr::eq(std::ptr::from_ref(a).cast::<u8>(), std::ptr::from_ref(b).cast::<u8>())
    };

    // A hit test, in screen coordinates as a client gives them.
    let widgets = canvas.widgets();
    let mut tested = 0;
    for w in &widgets {
        let (x, y) = w.rect.center();
        if swapdesk_paint::input::hit_test(&widgets, x, y) != Some(w.id) {
            continue;
        }
        let in_window = canvas.convertPoint_toView(NSPoint::new(f64::from(x), f64::from(y)), None);
        let on_screen = window.convertPointToScreen(in_window);
        // SAFETY: `accessibilityHitTest:` takes a screen point and returns an accessibility element or nil.
        let hit: Option<Retained<AnyObject>> = unsafe { msg_send![&*canvas, accessibilityHitTest: on_screen] };
        let want = canvas.ax_element_of(w.id);
        ensure!(
            matches!((&hit, &want), (Some(h), Some(e)) if same(h, e)),
            "a hit test at the centre of {:?} found {:?}, its element is {:?}",
            w.id,
            hit.as_deref().map(std::ptr::from_ref),
            want.as_deref().map(std::ptr::from_ref)
        );
        tested += 1;
    }
    ensure!(tested >= 5, "only {tested} controls were hit-tested");

    // The focus.
    let order = swapdesk_paint::input::tab_order(&shell.state.borrow(), WindowKind::Main, &widgets);
    let Some(target) = order.iter().copied().find(|id| !matches!(id, ControlId::Edit(_))) else {
        return Err("no control to focus".into());
    };
    canvas.focus_control(target, true);
    // SAFETY: the getters of the NSAccessibility protocol, with the types it documents.
    let focused: Option<Retained<AnyObject>> = unsafe { msg_send![&*canvas, accessibilityFocusedUIElement] };
    let element = canvas.ax_element_of(target).ok_or("the focused control has no element")?;
    ensure!(focused.as_deref().is_some_and(|f| same(f, &element)), "the focused element is not {target:?}'s");
    // SAFETY: as above.
    let says: bool = unsafe { msg_send![&*element, isAccessibilityFocused] };
    ensure!(says, "{target:?}'s element does not say it has the focus");

    // A press: a sidebar item that is not the open one.
    let item = shell
        .state
        .borrow()
        .controls(WindowKind::Main)
        .into_iter()
        .find(|c| c.region == Region::Sidebar && c.role == UiRole::Button && !c.selected && !c.disabled)
        .ok_or("no closed sidebar item")?;
    let element = canvas.ax_element_of(item.id).ok_or("the sidebar item has no element")?;
    // SAFETY: as above.
    let (offered, pressed): (bool, bool) = unsafe {
        (
            msg_send![&*element, isAccessibilitySelectorAllowed: sel!(accessibilityPerformPress)],
            msg_send![&*element, accessibilityPerformPress],
        )
    };
    ensure!(offered && pressed, "'{}' offers a press: {offered}, took it: {pressed}", item.label);
    let opened =
        |shell: &Shell| shell.state.borrow().controls(WindowKind::Main).iter().any(|c| c.id == item.id && c.selected);
    for _ in 0..40 {
        if opened(&shell) {
            break;
        }
        turn_the_run_loop();
    }
    ensure!(opened(&shell), "a press on '{}' did not open its screen", item.label);

    // The tree follows the state: after the paint the pressed item reads as selected.
    ensure!(canvas.paint_now(), "the canvas did not paint again");
    canvas.ax_after_paint();
    let line = canvas.ax_lines().into_iter().find(|l| l.text.contains(&format!("\"{}\"", item.label)));
    ensure!(
        line.as_ref().is_some_and(|l| l.text.contains(" selected")),
        "after the press '{}' reads as {:?}",
        item.label,
        line.map(|l| l.text)
    );
    // The same element stands for the control across paints: a client's cursor stays on it.
    let again = canvas.ax_element_of(item.id).ok_or("the sidebar item lost its element")?;
    ensure!(same(&again, &element), "'{}' got a new element after a paint", item.label);

    // A text offers no press.
    let text = shell
        .state
        .borrow()
        .controls(WindowKind::Main)
        .into_iter()
        .find(|c| matches!(c.role, UiRole::Text | UiRole::Heading))
        .ok_or("no text on the screen")?;
    let element = canvas.ax_element_of(text.id).ok_or("the text has no element")?;
    // SAFETY: as above.
    let (offered, pressed): (bool, bool) = unsafe {
        (
            msg_send![&*element, isAccessibilitySelectorAllowed: sel!(accessibilityPerformPress)],
            msg_send![&*element, accessibilityPerformPress],
        )
    };
    ensure!(!offered && !pressed, "the text '{}' offers a press: {offered}, took it: {pressed}", text.label);
    close(&shell);
    Ok(())
}

pub(super) fn run(mtm: MainThreadMarker) -> Result<(), String> {
    every_window_reads_as_the_shared_tree(mtm)?;
    elements_answer_as_their_controls(mtm)
}
