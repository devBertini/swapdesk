//! Window checks that open real windows (they need a logged-in WindowServer session and the main thread, which the
//! default test harness does not give a test). `examples/window_checks.rs` runs them from its own `main` when
//! `SWAPDESK_INTERACTIVE=1` is set, so they stay out of the default gate (`interactive: opens windows`). Each check
//! closes every window it opens.

use objc2_app_kit::NSAccessibility;
use swapdesk_ui::mock::{MockBackend, Scenario};

use super::*;

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

fn labels(view: &NSView, out: &mut Vec<String>) {
    for sub in view.subviews().to_vec() {
        if let Some(l) = sub.accessibilityLabel() {
            out.push(l.to_string());
        }
        labels(&sub, out);
    }
}

fn found(window: &NSWindow) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = window.contentView() {
        labels(&v, &mut out);
    }
    out
}

fn open(mtm: MainThreadMarker, sc: Scenario, hooks: ViewerHooks) -> Rc<Shell> {
    let state = sc.prepared_state();
    let waker = Arc::new(Waker::new());
    let backend = Arc::new(MockBackend::new(waker.clone()));
    start(mtm, state, backend, waker, Vec::new(), hooks)
}

fn close(shell: &Rc<Shell>) {
    if let Some(v) = shell.viewer.borrow_mut().take() {
        v.window.setDelegate(None);
        v.window.close();
    }
    shell.main.window.setDelegate(None);
    shell.main.window.close();
    clear_shell();
}

/// Every scenario of the mock backend builds its windows: the main window's canvas paints every control the state
/// lists, every tool of the viewer carries its accessible name, and a viewer window exists exactly while a viewer
/// session does.
fn scenarios_build_their_windows(mtm: MainThreadMarker) -> Result<(), String> {
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        let expected_viewer = viewer_model(&state);
        let shell = open(mtm, sc, ViewerHooks::default());

        ensure!(shell.main.canvas.paint_now(), "{}: the main window's canvas did not paint", sc.name());
        let widgets = shell.main.canvas.widgets();
        for c in shell.state.borrow().controls(WindowKind::Main) {
            ensure!(
                !c.id.focusable() || widgets.iter().any(|w| w.id == c.id),
                "{}: {:?} '{}' was not painted in the main window",
                sc.name(),
                c.id,
                c.label
            );
        }
        match expected_viewer {
            Some(m) => {
                let v = shell.viewer.borrow();
                let Some(v) = v.as_ref() else { return Err(format!("{}: a viewer window should exist", sc.name())) };
                let names = found(&v.window);
                for t in &m.tools {
                    let label = match t {
                        Tool::Screen { label, .. }
                        | Tool::Quality { label, .. }
                        | Tool::Switch { label, .. }
                        | Tool::Keys { label, .. }
                        | Tool::Disconnect { label } => label,
                        Tool::Swap { .. } => continue, // hidden unless `show_swap`
                    };
                    ensure!(names.contains(label), "{}: tool '{label}' missing ({names:?})", sc.name());
                }
                ensure!(v.surface().is_some(), "{}: the video view has a layer", sc.name());
            }
            None => ensure!(shell.viewer.borrow().is_none(), "{}: no viewer window without a session", sc.name()),
        }
        close(&shell);
    }
    Ok(())
}

/// The address field takes real key presses; what was typed and the keyboard in the field survive a state change and
/// a repaint (the device list changed while the person types); Tab then moves the focus to the next control of the
/// frame's order.
fn typing_survives_a_repaint_and_tab_moves_on(mtm: MainThreadMarker) -> Result<(), String> {
    let shell = open(mtm, Scenario::Inicio, ViewerHooks::default());
    let canvas = &shell.main.canvas;
    let address = ControlId::Edit(EditId::Address);
    ensure!(canvas.paint_now(), "the canvas did not paint");
    canvas.focus_control(address, true);
    ensure!(canvas.editing() == Some(EditId::Address), "the address field did not take the keyboard");
    ensure!(canvas.send_keys("192.168.1."), "the keys were not sent");
    let typed = canvas.edit_text(EditId::Address);
    ensure!(typed.as_deref() == Some("192.168.1."), "the field holds {typed:?}");

    shell.refresh(Changed::ALL);
    ensure!(canvas.paint_now(), "the canvas did not paint again");
    let kept = canvas.edit_text(EditId::Address);
    ensure!(kept.as_deref() == Some("192.168.1."), "the typed text was lost: {kept:?}");
    ensure!(canvas.editing() == Some(EditId::Address), "the keyboard left the field");

    let order = swapdesk_paint::input::tab_order(&shell.state.borrow(), WindowKind::Main, &canvas.widgets());
    let next = swapdesk_paint::input::next_focus(&order, Some(address), false);
    ensure!(next.is_some() && next != Some(address), "nothing follows the address field in {order:?}");
    ensure!(canvas.send_keys("\t"), "Tab was not sent");
    ensure!(canvas.focus() == next, "Tab moved the focus to {:?}, not {next:?}", canvas.focus());
    close(&shell);
    Ok(())
}

/// The render lane is attached once and detached before the window goes.
fn render_slot_lifecycle(mtm: MainThreadMarker) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Slot(AtomicUsize, AtomicUsize);
    impl VideoSlot for Slot {
        fn attached(&self, s: &crate::video::VideoSurface) {
            if s.parent_layer_ptr() != 0 {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn resized(&self, _a: VideoArea) {}
        fn detached(&self) {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
    }
    let slot = Arc::new(Slot(AtomicUsize::new(0), AtomicUsize::new(0)));
    let hooks = ViewerHooks { video: Some(slot.clone()), ..ViewerHooks::default() };
    let shell = open(mtm, Scenario::Visualizador, hooks);
    ensure!(slot.0.load(Ordering::SeqCst) == 1, "attached once with a layer");
    let Some(id) = shell.state.borrow().viewer_session().map(|s| s.id) else { return Err("no viewer session".into()) };
    shell.state.borrow_mut().apply(swapdesk_ui::UiEvent::Session {
        session_id: id,
        device_id: String::new(),
        state: swapdesk_ui::SessionState::Ended { reason: None },
    });
    shell.refresh(Changed::ALL);
    ensure!(slot.1.load(Ordering::SeqCst) == 1, "detached once");
    ensure!(shell.viewer.borrow().is_none(), "the viewer window went with its session");
    close(&shell);
    Ok(())
}

/// Runs every window check; returns the first failure. Call on the main thread only.
pub fn run_window_checks() -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("the window checks need the main thread")?;
    scenarios_build_their_windows(mtm)?;
    typing_survives_a_repaint_and_tab_moves_on(mtm)?;
    render_slot_lifecycle(mtm)
}
