//! Window checks that open real windows (they need a logged-in WindowServer session and the main thread, which the
//! default test harness does not give a test). `examples/window_checks.rs` runs them from its own `main` when
//! `SWAPDESK_INTERACTIVE=1` is set, so they stay out of the default gate (`interactive: opens windows`). Each check
//! closes every window it opens.

use swapdesk_ui::mock::{MockBackend, Scenario};

use super::*;

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

pub(super) fn open(mtm: MainThreadMarker, sc: Scenario, hooks: ViewerHooks) -> Rc<Shell> {
    let state = sc.prepared_state();
    let waker = Arc::new(Waker::new());
    let backend = Arc::new(MockBackend::new(waker.clone()));
    start(mtm, state, backend, waker, Vec::new(), hooks)
}

pub(super) fn close(shell: &Rc<Shell>) {
    if let Some(v) = shell.viewer.borrow_mut().take() {
        v.window.setDelegate(None);
        v.window.close();
    }
    if let Some(b) = shell.banner.borrow_mut().take() {
        b.close();
    }
    shell.main.window.setDelegate(None);
    shell.main.window.close();
    clear_shell();
}

/// Every control the state lists for window `kind` was painted by `canvas`.
fn painted_all(shell: &Shell, canvas: &CanvasView, kind: WindowKind, scenario: &str) -> Result<(), String> {
    let widgets = canvas.widgets();
    for c in shell.state.borrow().controls(kind) {
        ensure!(
            !c.id.focusable() || widgets.iter().any(|w| w.id == c.id),
            "{scenario}: {:?} '{}' was not painted in the {kind:?} window",
            c.id,
            c.label
        );
    }
    Ok(())
}

/// Every scenario of the mock backend builds its windows: each canvas (main, viewer, banner) paints every control
/// the state lists for it; a viewer window exists exactly while a viewer session does, with its video view under the
/// canvas on the frame's video rectangle; a banner exists exactly while this Mac is hosted.
fn scenarios_build_their_windows(mtm: MainThreadMarker) -> Result<(), String> {
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        let expected_viewer = viewer_title(&state);
        let shell = open(mtm, sc, ViewerHooks::default());

        ensure!(shell.main.canvas.paint_now(), "{}: the main window's canvas did not paint", sc.name());
        // The shell says which system the viewer runs on: its shortcuts are written with the Mac's keys.
        let keys = shell.state.borrow().shortcut_keys(swapdesk_ui::ViewerAction::ToggleBar);
        ensure!(keys == "Control+Option+B", "{}: the session bar shortcut is written {keys}", sc.name());
        painted_all(&shell, &shell.main.canvas, WindowKind::Main, sc.name())?;
        match expected_viewer {
            Some(_) => {
                let v = shell.viewer.borrow();
                let Some(v) = v.as_ref() else { return Err(format!("{}: a viewer window should exist", sc.name())) };
                ensure!(v.canvas.paint_now(), "{}: the viewer's canvas did not paint", sc.name());
                painted_all(&shell, &v.canvas, WindowKind::Viewer, sc.name())?;
                ensure!(v.surface().is_some(), "{}: the video view has a layer", sc.name());
                // The video view sits under the canvas, on the rectangle the frame left for it.
                v.place_video();
                let (video, hole) = (v.video.frame(), v.canvas.video_rect());
                let hole = hole.ok_or_else(|| format!("{}: the viewer's frame has no video rectangle", sc.name()))?;
                ensure!(
                    (video.origin.x - f64::from(hole.x)).abs() < 1.0
                        && (video.origin.y - f64::from(hole.y)).abs() < 1.0
                        && (video.size.width - f64::from(hole.w)).abs() < 1.0
                        && (video.size.height - f64::from(hole.h)).abs() < 1.0,
                    "{}: the video view is at {video:?}, the frame's rectangle is {hole:?}",
                    sc.name()
                );
            }
            None => ensure!(shell.viewer.borrow().is_none(), "{}: no viewer window without a session", sc.name()),
        }
        let banner = shell.banner.borrow().as_ref().map(|b| b.canvas.clone());
        ensure!(
            banner.is_some() == shell.state.borrow().host_session().is_some(),
            "{}: a banner exists exactly while this Mac is hosted",
            sc.name()
        );
        if let Some(canvas) = banner {
            ensure!(canvas.paint_now(), "{}: the banner's canvas did not paint", sc.name());
            painted_all(&shell, &canvas, WindowKind::Banner, sc.name())?;
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

/// Lets the run loop turn for up to two seconds until the main window's miniaturized state is `want`.
fn settles_miniaturized(shell: &Shell, want: bool) -> bool {
    for _ in 0..20 {
        if shell.main.window.isMiniaturized() == want {
            return true;
        }
        objc2_foundation::NSRunLoop::currentRunLoop()
            .runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.1));
    }
    shell.main.window.isMiniaturized() == want
}

/// A session coming up minimizes the main window to the Dock in a real run (one with the video slot) and in neither
/// a mock run nor a second state of the same session; a new session minimizes it again.
fn a_started_session_minimizes_the_main_window_once_in_a_real_run(mtm: MainThreadMarker) -> Result<(), String> {
    use swapdesk_ui::{Role, SessionState, UiEvent};

    struct Slot;
    impl VideoSlot for Slot {
        fn attached(&self, _s: &crate::video::VideoSurface) {}
        fn resized(&self, _a: VideoArea) {}
        fn detached(&self) {}
    }
    let session = |id: u64, state: SessionState| UiEvent::Session { session_id: id, device_id: String::new(), state };
    let active = |role: Role| SessionState::Active { since_ms: 0, role, granted: swapdesk_ui::Permissions::NONE };

    // A mock run: the scenario holds an Active host session and the window stays.
    let shell = open(mtm, Scenario::Banner, ViewerHooks::default());
    ensure!(!settles_miniaturized(&shell, true), "a mock run minimized the main window");
    close(&shell);

    // A real run: the first Active state of the scenario's session minimizes it.
    let hooks = ViewerHooks { video: Some(Arc::new(Slot)), ..ViewerHooks::default() };
    let shell = open(mtm, Scenario::Banner, hooks);
    ensure!(settles_miniaturized(&shell, true), "the main window was not minimized when the session started");

    // Brought back by the person, the same session never minimizes it again.
    shell.main.window.deminiaturize(None);
    ensure!(settles_miniaturized(&shell, false), "the main window did not come back");
    let host_id = shell.state.borrow().host_session().map(|s| s.id).ok_or("no host session")?;
    let paused = SessionState::Paused { desktop: swapdesk_ui::DesktopKind::Locked };
    shell.state.borrow_mut().apply(session(host_id, paused));
    shell.state.borrow_mut().apply(session(host_id, active(Role::Host)));
    shell.minimize_main_for_session();
    ensure!(!settles_miniaturized(&shell, true), "the same session minimized the main window twice");

    // A new session does.
    shell.state.borrow_mut().apply(session(host_id + 100, active(Role::Host)));
    shell.minimize_main_for_session();
    ensure!(settles_miniaturized(&shell, true), "a new session did not minimize the main window");
    shell.main.window.deminiaturize(None);
    close(&shell);
    Ok(())
}

/// The mock backend, keeping every command the shell sends.
struct Recording {
    inner: Arc<MockBackend>,
    sent: std::sync::Mutex<Vec<Command>>,
}

impl UiBackend for Recording {
    fn send(&self, command: Command) -> swapdesk_ui::Ticket {
        self.sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(command.clone());
        self.inner.send(command)
    }
    fn drain(&self) -> Vec<swapdesk_ui::UiEvent> {
        self.inner.drain()
    }
}

/// The two pickers of the files panel: "Escolher arquivos" opens a panel for any number of files and no folder, the
/// receive folder one for a single folder that may be created there; the files chosen leave as one `FilesSend` of
/// the viewer session, the folder chosen becomes the receive folder. (The panels are not shown: they wait for a
/// person.) Opening the logs with no logs folder, as in a mock run, does nothing.
fn the_pickers_are_set_up_and_what_is_chosen_reaches_the_state(mtm: MainThreadMarker) -> Result<(), String> {
    let files = files_panel(mtm, "Escolher arquivos");
    ensure!(
        files.canChooseFiles() && !files.canChooseDirectories() && files.allowsMultipleSelection(),
        "the files panel takes files {}, folders {}, several {}",
        files.canChooseFiles(),
        files.canChooseDirectories(),
        files.allowsMultipleSelection()
    );
    let folder = folder_panel(mtm, "Escolher a pasta");
    ensure!(
        !folder.canChooseFiles()
            && folder.canChooseDirectories()
            && !folder.allowsMultipleSelection()
            && folder.canCreateDirectories(),
        "the folder panel takes files {}, folders {}, several {}, new folders {}",
        folder.canChooseFiles(),
        folder.canChooseDirectories(),
        folder.allowsMultipleSelection(),
        folder.canCreateDirectories()
    );

    let state = Scenario::Arquivos.prepared_state();
    let waker = Arc::new(Waker::new());
    let backend =
        Arc::new(Recording { inner: Arc::new(MockBackend::new(waker.clone())), sent: std::sync::Mutex::default() });
    let shell = start(mtm, state, backend.clone(), waker, Vec::new(), ViewerHooks::default());
    let session = shell.state.borrow().viewer_session().map(|s| s.id).ok_or("the scenario has no viewer session")?;

    let dir = std::path::PathBuf::from("/tmp/swapdesk-recebidos");
    shell.folder_chosen(dir.clone());
    ensure!(
        shell.state.borrow().receive_dir() == Some(dir.as_path()),
        "the receive folder is {:?}",
        shell.state.borrow().receive_dir()
    );

    let chosen = vec![std::path::PathBuf::from("/tmp/relatório.pdf"), std::path::PathBuf::from("/tmp/fotos.zip")];
    shell.files_chosen(Vec::new());
    shell.files_chosen(chosen.clone());
    let sends: Vec<Command> = backend
        .sent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|c| matches!(c, Command::FilesSend { .. }))
        .cloned()
        .collect();
    ensure!(
        matches!(sends.as_slice(), [Command::FilesSend { session_id, local_paths, .. }]
            if *session_id == session && *local_paths == chosen),
        "the files chosen left as {sends:?}"
    );

    shell.run(WindowKind::Main, Handling::OpenLogsFolder);
    close(&shell);
    Ok(())
}

/// A shell on `scenario` over a recording backend.
fn open_recording(mtm: MainThreadMarker, scenario: Scenario) -> (Rc<Shell>, Arc<Recording>) {
    let waker = Arc::new(Waker::new());
    let backend =
        Arc::new(Recording { inner: Arc::new(MockBackend::new(waker.clone())), sent: std::sync::Mutex::default() });
    let shell = start(mtm, scenario.prepared_state(), backend.clone(), waker, Vec::new(), ViewerHooks::default());
    (shell, backend)
}

fn disconnects(backend: &Recording) -> usize {
    let sent = backend.sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    sent.iter().filter(|c| matches!(c, Command::Disconnect { .. })).count()
}

/// A connect card of the main window and the keyboard: Esc cancels a waiting card and closes an ended one (a key
/// event through the window, as a person gives it), a dialog over the window keeps Esc for itself, and a service
/// event that ends a connect in failure moves the focus to the card's Fechar.
fn escape_and_focus_serve_the_connect_card(mtm: MainThreadMarker) -> Result<(), String> {
    use swapdesk_ui::mock::{ENTRY, MAC};
    use swapdesk_ui::{ConnectWhy, SessionState, UiEvent};
    let entry = |state: SessionState| UiEvent::Session { session_id: ENTRY, device_id: MAC.into(), state };

    let (shell, backend) = open_recording(mtm, Scenario::Dispositivos);
    let canvas = shell.main.canvas.clone();
    shell.state.borrow_mut().apply(entry(SessionState::Prompted { remaining_s: 15 }));
    shell.drain_backend();
    ensure!(canvas.paint_now(), "the main window did not paint");
    ensure!(disconnects(&backend) == 0, "a Disconnect was sent before Esc");
    ensure!(canvas.send_keys("\u{1b}"), "the Esc key event was not sent");
    ensure!(disconnects(&backend) == 1, "Esc on a waiting card sent {} cancels", disconnects(&backend));

    shell.state.borrow_mut().apply(entry(SessionState::NotOpened { why: ConnectWhy::Declined }));
    shell.drain_backend();
    ensure!(canvas.focus() == Some(ControlId::ConnectClose), "focus after the failure is {:?}", canvas.focus());
    ensure!(canvas.paint_now(), "the main window did not paint the end card");
    ensure!(canvas.send_keys("\u{1b}"), "the Esc key event was not sent");
    ensure!(shell.state.borrow().wait_card(MAC).is_none(), "Esc did not close the ended card");
    ensure!(disconnects(&backend) == 1, "closing an ended card sent a command");
    ensure!(canvas.focus() == Some(ControlId::Control), "focus after Fechar is {:?}", canvas.focus());
    close(&shell);

    let (shell, backend) = open_recording(mtm, Scenario::PedidoConexao);
    shell.state.borrow_mut().apply(entry(SessionState::Prompted { remaining_s: 15 }));
    ensure!(shell.main.canvas.paint_now(), "the main window did not paint the dialog");
    ensure!(shell.main.canvas.send_keys("\u{1b}"), "the Esc key event was not sent");
    ensure!(disconnects(&backend) == 0, "Esc over a dialog cancelled the wait");
    close(&shell);
    Ok(())
}

/// Runs every window check; returns the first failure. Call on the main thread only.
pub fn run_window_checks() -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("the window checks need the main thread")?;
    escape_and_focus_serve_the_connect_card(mtm)?;
    scenarios_build_their_windows(mtm)?;
    typing_survives_a_repaint_and_tab_moves_on(mtm)?;
    render_slot_lifecycle(mtm)?;
    a_started_session_minimizes_the_main_window_once_in_a_real_run(mtm)?;
    the_pickers_are_set_up_and_what_is_chosen_reaches_the_state(mtm)?;
    super::ax_checks::run(mtm)
}
