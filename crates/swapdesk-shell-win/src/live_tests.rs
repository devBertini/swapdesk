//! Tests against a real shell window on a thread of the test process (the mock backend, the `dispositivos` scenario):
//! panic-proof native callbacks, and the hover measurement behind budget B8. Messages are posted or sent to the window
//! with `PostMessageW` / `SendMessageW`; nothing is injected as input and the pointer never moves.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use swapdesk_ui::mock::{MockBackend, Scenario};
use swapdesk_ui::{ControlId, UiBackend, WindowKind};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, IsIconic, IsWindow, PostMessageW, SendMessageW, WM_APP, WM_CLOSE, WM_GETTEXTLENGTH, WM_MOUSEMOVE,
};

use crate::offscreen::render;
use crate::system::Waker;
use swapdesk_paint::Interaction;

/// A running shell: the window thread and what is needed to read its CPU time.
struct Live {
    waker: Arc<Waker>,
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Live {
    fn start(scenario: Scenario) -> Live {
        Live::start_with(move |w| {
            let backend: Arc<dyn UiBackend> = Arc::new(MockBackend::new(w.clone()));
            crate::run(scenario.prepared_state(), backend, w, Vec::new()).expect("shell");
        })
    }

    /// Starts the shell with `run` on the window thread; `run` gets the waker the window will be set on.
    fn start_with(run: impl FnOnce(Arc<Waker>) + Send + 'static) -> Live {
        let waker = Arc::new(Waker::new());
        let (tx, rx) = std::sync::mpsc::channel();
        let w = Arc::clone(&waker);
        let join = std::thread::spawn(move || {
            // SAFETY: a plain query of this thread's id.
            tx.send(unsafe { GetCurrentThreadId() }).expect("thread id");
            run(w);
        });
        let thread_id = rx.recv().expect("thread id");
        let live = Live { waker, thread_id, join: Some(join) };
        let started = Instant::now();
        while live.hwnd().0.is_null() {
            assert!(started.elapsed() < Duration::from_secs(10), "the window did not appear");
            std::thread::sleep(Duration::from_millis(20));
        }
        // The first paint, and the first drain of the backend.
        std::thread::sleep(Duration::from_millis(1500));
        live
    }

    fn hwnd(&self) -> HWND {
        self.waker.window()
    }

    /// Client size in pixels and the DPI scale.
    fn geometry(&self) -> ((i32, i32), f32) {
        let mut rc = windows::Win32::Foundation::RECT::default();
        // SAFETY: our window; plain reads.
        let dpi = unsafe {
            let _ = GetClientRect(self.hwnd(), &mut rc);
            GetDpiForWindow(self.hwnd())
        };
        ((rc.right - rc.left, rc.bottom - rc.top), dpi as f32 / 96.0)
    }

    /// CPU time of the window thread: (GetThreadTimes in ms, cycles).
    fn cpu(&self) -> (f64, u64) {
        // SAFETY: a handle opened for query only and closed again; out pointers are locals.
        unsafe {
            let h: HANDLE = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, self.thread_id).expect("open thread");
            let (mut c, mut e, mut k, mut u) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u).expect("times");
            let mut cycles = 0u64;
            QueryThreadCycleTime(h, &mut cycles).expect("cycles");
            let _ = CloseHandle(h);
            let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 10_000.0;
            (t(k) + t(u), cycles)
        }
    }

    fn post_move(&self, x: i32, y: i32) {
        // SAFETY: posting to our window; a failure only drops one move.
        unsafe {
            let _ = PostMessageW(
                Some(self.hwnd()),
                WM_MOUSEMOVE,
                WPARAM(0),
                LPARAM(((y as isize & 0xFFFF) << 16) | (x as isize & 0xFFFF)),
            );
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // SAFETY: our window; the message ends the run and the message loop of its thread.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd()), crate::system::WM_QUIT_APP, WPARAM(0), LPARAM(0));
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Cycles per millisecond of this CPU, from a short spin on the calling thread.
fn cycles_per_ms() -> f64 {
    // SAFETY: the pseudo handle of the current thread, valid for the call.
    let now = || unsafe {
        let mut c = 0u64;
        QueryThreadCycleTime(windows::Win32::System::Threading::GetCurrentThread(), &mut c).expect("cycles");
        c
    };
    let (t0, c0) = (Instant::now(), now());
    while t0.elapsed() < Duration::from_millis(200) {
        std::hint::spin_loop();
    }
    (now() - c0) as f64 / t0.elapsed().as_secs_f64() / 1000.0
}

#[test]
fn a_panic_in_a_window_handler_leaves_the_window_answering() {
    let live = Live::start(Scenario::Dispositivos);
    let hwnd = live.hwnd();
    // SAFETY: sends to our window from another thread (it blocks until the window thread has answered).
    let title_len = unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None) }.0;
    assert!(title_len > 0, "the window has a title");

    // A message nothing handles, then a real one (a mouse move), each panicking once at the start of its handling.
    for msg in [WM_APP + 100, WM_MOUSEMOVE] {
        crate::window::INJECT_PANIC_MSG.store(msg, Ordering::SeqCst);
        // SAFETY: as above.
        unsafe { SendMessageW(hwnd, msg, None, Some(LPARAM(0))) };
        assert_eq!(crate::window::INJECT_PANIC_MSG.load(Ordering::SeqCst), 0, "the panic point fired for {msg:#x}");
        // SAFETY: as above; a later message is answered normally.
        let later = unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None) }.0;
        assert_eq!(later, title_len, "the window still answers after a panic in {msg:#x}");
        // SAFETY: a plain query.
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
    }
}

#[test]
fn a_panic_in_the_input_subclass_leaves_the_control_answering() {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GWLP_WNDPROC, SetWindowLongPtrW, WINDOW_EX_STYLE, WM_SETTEXT, WS_OVERLAPPED,
    };
    use windows::core::w;
    // SAFETY: an EDIT window of the test thread, subclassed with the shell's procedure exactly as the shell does it
    // (the original procedure is saved first); it is destroyed at the end. `SendMessageW` on the same thread calls
    // the procedure directly.
    unsafe {
        let hmod = GetModuleHandleW(None).ok().map(Into::into);
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!("abc"),
            WS_OVERLAPPED,
            0,
            0,
            50,
            20,
            None,
            None,
            hmod,
            None,
        )
        .expect("edit");
        let original = SetWindowLongPtrW(edit, GWLP_WNDPROC, crate::window::edit_proc as *const () as isize);
        let _ = crate::window::EDIT_ORIGINAL_PROC.compare_exchange(0, original, Ordering::SeqCst, Ordering::SeqCst);
        assert_eq!(SendMessageW(edit, WM_GETTEXTLENGTH, None, None).0, 3);
        crate::window::INJECT_PANIC_MSG.store(WM_GETTEXTLENGTH, Ordering::SeqCst);
        // The panicking call still reaches the control's own procedure.
        assert_eq!(SendMessageW(edit, WM_GETTEXTLENGTH, None, None).0, 3);
        assert_eq!(crate::window::INJECT_PANIC_MSG.load(Ordering::SeqCst), 0, "the panic point fired");
        let _ = SendMessageW(edit, WM_SETTEXT, None, Some(LPARAM(w!("abcd").as_ptr() as isize)));
        assert_eq!(SendMessageW(edit, WM_GETTEXTLENGTH, None, None).0, 4);
        let _ = DestroyWindow(edit);
    }
}

/// B8-style hover measurement on a real window: 600 `WM_MOUSEMOVE` posts over ~10 s, then the window thread's CPU time.
/// Run by hand: `cargo nextest run -p swapdesk-shell-win --run-ignored only -E 'test(/hover_cpu/)' --no-capture`.
#[test]
#[ignore = "measure: 10 s real-window CPU measurement, run by hand"]
fn hover_cpu_of_the_window_thread() {
    let live = Live::start(Scenario::Dispositivos);
    let ((cw, ch), scale) = live.geometry();
    let state = Scenario::Dispositivos.prepared_state();
    let r = render(&state, WindowKind::Main, (cw as f32 / scale, ch as f32 / scale), scale, &Interaction::default())
        .expect("render");
    let centre = |id: ControlId| {
        let w = r.output.widgets.iter().find(|w| w.id == id).expect("control");
        let (x, y) = w.rect.center();
        ((x * scale) as i32, (y * scale) as i32)
    };
    let mut targets: Vec<(i32, i32)> = Vec::new();
    for w in &r.output.widgets {
        if !matches!(w.id, ControlId::Edit(_)) && targets.len() < 2 && !targets.contains(&centre(w.id)) {
            targets.push(centre(w.id));
        }
    }
    assert_eq!(targets.len(), 2, "two controls to hover");
    let per_ms = cycles_per_ms();

    for (name, alternate) in [
        ("no messages at all (idle)", None),
        ("alternating between two controls (every move changes the hover)", Some(true)),
        ("steady over one control", Some(false)),
    ] {
        let (t0, c0) = live.cpu();
        let p0 = crate::window::PAINT_COUNT.load(Ordering::SeqCst);
        let started = Instant::now();
        for i in 0..600u32 {
            if let Some(alternate) = alternate {
                let (x, y) = targets[if alternate { (i % 2) as usize } else { 0 }];
                live.post_move(x, y);
            }
            let due = started + Duration::from_millis(u64::from(i + 1) * 10_000 / 600);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        std::thread::sleep(Duration::from_millis(300));
        let (t1, c1) = live.cpu();
        let paints = crate::window::PAINT_COUNT.load(Ordering::SeqCst) - p0;
        println!(
            "HOVER {name}: 600 moves in {:.1} s, paints {paints}, window thread CPU {:.1} ms (GetThreadTimes), {:.1} ms (cycles / {:.0} per ms)",
            started.elapsed().as_secs_f64(),
            t1 - t0,
            (c1 - c0) as f64 / per_ms,
            per_ms
        );
    }
}

/// A backend that answers nothing and delivers the events a test pushes.
#[derive(Default)]
struct Scripted(std::sync::Mutex<Vec<swapdesk_ui::UiEvent>>);

impl Scripted {
    fn push(&self, e: swapdesk_ui::UiEvent) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(e);
    }
}

impl UiBackend for Scripted {
    fn send(&self, _command: swapdesk_ui::Command) -> swapdesk_ui::Ticket {
        swapdesk_ui::Ticket(0)
    }
    fn drain(&self) -> Vec<swapdesk_ui::UiEvent> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

/// The viewer pipeline's hooks of a real run, doing nothing.
struct NoHost;

impl crate::ViewerHost for NoHost {
    fn opened(&self, _surface: swapdesk_platform::NativeSurface, _area: swapdesk_platform::Rect, _scale: f32) {}
    fn resized(&self, _area: swapdesk_platform::Rect, _scale: f32) {}
    fn closed(&self) {}
    fn focus(&self, _focused: bool) {}
    fn mouse(&self, _msg: crate::MouseMsg) -> bool {
        false
    }
    fn captures_keyboard(&self) -> bool {
        false
    }
    fn key(&self, _msg: &crate::KeyMsg) -> bool {
        false
    }
    fn pointer_shape(&self) -> Option<Arc<swapdesk_platform::CursorShape>> {
        None
    }
}

fn session(id: u64, state: swapdesk_ui::SessionState) -> swapdesk_ui::UiEvent {
    swapdesk_ui::UiEvent::Session { session_id: id, device_id: swapdesk_ui::mock::DESKTOP.into(), state }
}

fn active(role: swapdesk_ui::Role) -> swapdesk_ui::SessionState {
    swapdesk_ui::SessionState::Active {
        since_ms: 0,
        role,
        granted: swapdesk_ui::Permissions::NONE,
        elevation: Default::default(),
    }
}

fn iconic(live: &Live) -> bool {
    // SAFETY: a plain query of our window.
    unsafe { IsIconic(live.hwnd()) }.as_bool()
}

/// Waits up to two seconds for the main window to be (or not to be) minimized.
fn settles_iconic(live: &Live, want: bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if iconic(live) == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    iconic(live) == want
}

/// D32 with the notification area (B16): in a real run a person's close hides the main window and the run goes on;
/// an update about to be installed still ends the run (a close that only hid the window would keep the process alive
/// and block the applier).
#[test]
fn closing_the_main_window_of_a_real_run_hides_it_and_an_update_still_ends_the_run() {
    use swapdesk_ui::UiWaker;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    let backend = Arc::new(Scripted::default());
    let b = Arc::clone(&backend);
    let mut live = Live::start_with(move |w| {
        let host: Arc<dyn crate::ViewerHost> = Arc::new(NoHost);
        let backend: Arc<dyn UiBackend> = b;
        crate::run_with_host(swapdesk_ui::UiState::default(), backend, w, Vec::new(), Some(host)).expect("shell");
    });
    let hwnd = live.hwnd();
    // SAFETY: our window.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: plain queries of our window.
    assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool(), "the close keeps the window");
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "the close hides it into the notification area");
    assert!(!live.join.as_ref().expect("running").is_finished(), "the run goes on");

    backend.push(swapdesk_ui::UiEvent::UpdateRestarting);
    live.waker.wake();
    let join = live.join.take().expect("running");
    let started = Instant::now();
    while !join.is_finished() {
        assert!(started.elapsed() < Duration::from_secs(5), "the update's restart did not end the run");
        std::thread::sleep(Duration::from_millis(20));
    }
    join.join().expect("the shell thread");
}

/// F-UI-04: the notification area menu's two entries reach the window as commands: "Abrir" shows the hidden main
/// window and "Sair" ends the run.
#[test]
fn the_tray_menu_open_shows_the_main_window_and_quit_ends_the_run() {
    use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, WM_COMMAND};
    let backend = Arc::new(Scripted::default());
    let b = Arc::clone(&backend);
    let mut live = Live::start_with(move |w| {
        let host: Arc<dyn crate::ViewerHost> = Arc::new(NoHost);
        let backend: Arc<dyn UiBackend> = b;
        crate::run_with_host(swapdesk_ui::UiState::default(), backend, w, Vec::new(), Some(host)).expect("shell");
    });
    let hwnd = live.hwnd();
    // SAFETY: our window.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "the close hid the window");

    // SAFETY: our window.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(crate::tray::CMD_OPEN), LPARAM(0));
    }
    let started = Instant::now();
    while !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        assert!(started.elapsed() < Duration::from_secs(5), "Open did not show the main window");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!live.join.as_ref().expect("running").is_finished(), "Open keeps the run going");

    // SAFETY: our window.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(crate::tray::CMD_QUIT), LPARAM(0));
    }
    let join = live.join.take().expect("running");
    let started = Instant::now();
    while !join.is_finished() {
        assert!(started.elapsed() < Duration::from_secs(5), "Quit did not end the run");
        std::thread::sleep(Duration::from_millis(20));
    }
    join.join().expect("the shell thread");
}

#[test]
fn the_main_window_minimizes_when_a_session_starts_and_not_in_a_mock_run() {
    use swapdesk_ui::{Role, SessionState, UiWaker};
    use windows::Win32::UI::WindowsAndMessaging::{SW_RESTORE, ShowWindow};

    // A mock run holds sessions already Active, and keeps its window (captures show it).
    for scenario in [Scenario::Banner, Scenario::Visualizador] {
        let mock = Live::start(scenario);
        std::thread::sleep(Duration::from_millis(500));
        assert!(!iconic(&mock), "a mock run never minimizes ({scenario:?})");
    }

    // A real run (with the viewer pipeline's hooks): this machine is controlled, then it controls.
    let backend = Arc::new(Scripted::default());
    backend.push(session(1, active(Role::Host)));
    let b = Arc::clone(&backend);
    let live = Live::start_with(move |w| {
        let host: Arc<dyn crate::ViewerHost> = Arc::new(NoHost);
        let backend: Arc<dyn UiBackend> = b;
        crate::run_with_host(swapdesk_ui::UiState::default(), backend, w, Vec::new(), Some(host)).expect("shell");
    });
    assert!(settles_iconic(&live, true), "the first Active state of a session minimizes the main window");

    // The person brings it back: a pause and an Active state of the same session leave it where it is.
    // SAFETY: our window.
    unsafe {
        let _ = ShowWindow(live.hwnd(), SW_RESTORE);
    }
    assert!(settles_iconic(&live, false), "the window was restored");
    backend.push(session(1, SessionState::Paused { desktop: swapdesk_ui::DesktopKind::Locked }));
    backend.push(session(1, active(Role::Host)));
    live.waker.wake();
    std::thread::sleep(Duration::from_millis(700));
    assert!(!iconic(&live), "the same session never minimizes twice");

    // A new session, this time controlling, minimizes it again.
    backend.push(session(2, active(Role::Viewer)));
    live.waker.wake();
    assert!(settles_iconic(&live, true), "a new session minimizes the main window again");

    // Owner, 11/11: the window comes back when the session this computer controlled ends and none is left; the
    // session in which it was controlled leaves it minimized (D60).
    backend.push(session(1, SessionState::Ended { reason: None }));
    live.waker.wake();
    std::thread::sleep(Duration::from_millis(700));
    assert!(iconic(&live), "session 2 (controlling) still goes on");
    backend.push(session(2, SessionState::Ended { reason: None }));
    live.waker.wake();
    assert!(settles_iconic(&live, false), "the controlling session ended: the main window is restored");

    // Controlled alone: the window stays minimized after the end.
    backend.push(session(3, active(Role::Host)));
    live.waker.wake();
    assert!(settles_iconic(&live, true), "a new session minimizes the main window");
    backend.push(session(3, SessionState::Ended { reason: None }));
    live.waker.wake();
    std::thread::sleep(Duration::from_millis(700));
    assert!(iconic(&live), "the computer that was controlled keeps its window minimized");
}

/// A backend that records the commands it is sent and delivers the events a test pushes.
#[derive(Default)]
struct Recording {
    events: std::sync::Mutex<Vec<swapdesk_ui::UiEvent>>,
    commands: std::sync::Mutex<Vec<swapdesk_ui::Command>>,
}

impl Recording {
    fn push(&self, e: swapdesk_ui::UiEvent) {
        self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(e);
    }
    fn disconnects(&self) -> usize {
        let c = self.commands.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        c.iter().filter(|c| matches!(c, swapdesk_ui::Command::Disconnect { .. })).count()
    }
}

impl UiBackend for Recording {
    fn send(&self, command: swapdesk_ui::Command) -> swapdesk_ui::Ticket {
        self.commands.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(command);
        swapdesk_ui::Ticket(0)
    }
    fn drain(&self) -> Vec<swapdesk_ui::UiEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

fn entry_event(state: swapdesk_ui::SessionState) -> swapdesk_ui::UiEvent {
    swapdesk_ui::UiEvent::Session {
        session_id: swapdesk_ui::mock::ENTRY,
        device_id: swapdesk_ui::mock::MAC.into(),
        state,
    }
}

fn last_focus() -> Option<ControlId> {
    *crate::window::LAST_FOCUS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A real shell on `state` with a recording backend.
fn start_recording(state: swapdesk_ui::UiState) -> (Live, Arc<Recording>) {
    let backend = Arc::new(Recording::default());
    let b = Arc::clone(&backend);
    let live = Live::start_with(move |w| {
        let backend: Arc<dyn UiBackend> = b;
        crate::run(state, backend, w, Vec::new()).expect("shell");
    });
    (live, backend)
}

/// Delivers an event of the service to the shell and waits for it to be handled.
fn deliver(live: &Live, backend: &Recording, e: swapdesk_ui::UiEvent) {
    use swapdesk_ui::UiWaker;
    backend.push(e);
    live.waker.wake();
    std::thread::sleep(Duration::from_millis(700));
}

fn press_escape(live: &Live) {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
    // SAFETY: sends to our window from another thread (it blocks until the window thread has answered).
    unsafe { SendMessageW(live.hwnd(), WM_KEYDOWN, Some(WPARAM(usize::from(VK_ESCAPE.0))), Some(LPARAM(0))) };
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn escape_on_the_main_window_cancels_a_waiting_card() {
    use swapdesk_ui::SessionState;
    let (live, backend) = start_recording(Scenario::Dispositivos.prepared_state());
    deliver(&live, &backend, entry_event(SessionState::Prompted { remaining_s: 15 }));
    assert_eq!(backend.disconnects(), 0);
    press_escape(&live);
    assert_eq!(backend.disconnects(), 1, "Esc sent the cancel of the waiting entry");
}

#[test]
fn escape_on_the_main_window_closes_an_ended_card() {
    use swapdesk_ui::{ConnectWhy, SessionState};
    let (live, backend) = start_recording(Scenario::Dispositivos.prepared_state());
    deliver(&live, &backend, entry_event(SessionState::NotOpened { why: ConnectWhy::Declined }));
    assert_eq!(last_focus(), Some(ControlId::ConnectClose));
    press_escape(&live);
    assert_eq!(backend.disconnects(), 0, "closing an ended card sends nothing");
    assert_eq!(last_focus(), Some(ControlId::Control), "Fechar ran: the card is gone and focus is back on the button");
}

#[test]
fn escape_over_a_dialog_keeps_its_old_meaning() {
    use swapdesk_ui::SessionState;
    let (live, backend) = start_recording(Scenario::PedidoConexao.prepared_state());
    deliver(&live, &backend, entry_event(SessionState::Prompted { remaining_s: 15 }));
    press_escape(&live);
    assert_eq!(backend.disconnects(), 0, "a dialog over the window takes Esc itself: the wait is not cancelled");
}

#[test]
fn a_service_event_that_ends_a_connect_in_failure_moves_focus_to_fechar() {
    use swapdesk_ui::{ConnectWhy, SessionState};
    let (live, backend) = start_recording(Scenario::Dispositivos.prepared_state());
    // The wait card of a click gets Cancelar from the click's own effect; a news item of the wait asks for nothing.
    deliver(&live, &backend, entry_event(SessionState::Prompted { remaining_s: 15 }));
    assert_eq!(last_focus(), None, "a wait that goes on takes no focus");
    deliver(&live, &backend, entry_event(SessionState::NotOpened { why: ConnectWhy::Declined }));
    assert_eq!(last_focus(), Some(ControlId::ConnectClose), "the end card's Fechar takes the focus");
}

const CONFIRM_LINE: &str = "Permitir que o MacBook do escritório entre sem perguntar";

fn confirm_state(remaining_s: u16) -> swapdesk_ui::UiState {
    swapdesk_ui::UiState::for_elevated_confirm(swapdesk_ui::Language::PtBr, false, CONFIRM_LINE.into(), remaining_s)
}

/// The elevated instance's confirmation window on a thread; the channel gets its answer when it closes.
fn start_confirm(remaining_s: u16) -> (Live, std::sync::mpsc::Receiver<Option<bool>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let live = Live::start_with(move |w| {
        let _ = tx.send(crate::app::run_confirm_with(confirm_state(remaining_s), w).expect("shell"));
    });
    (live, rx)
}

#[test]
fn enter_alone_confirms_nothing() {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
    use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
    let (live, answer) = start_confirm(90);
    // SAFETY: sends to our window from another thread (it blocks until the window thread has answered).
    unsafe { SendMessageW(live.hwnd(), WM_KEYDOWN, Some(WPARAM(usize::from(VK_RETURN.0))), Some(LPARAM(0))) };
    std::thread::sleep(Duration::from_millis(400));
    assert!(answer.try_recv().is_err(), "no button has the keyboard: Enter confirms nothing and the window stays");
    // Esc is "Cancelar": the window closes with a no.
    press_escape(&live);
    assert_eq!(answer.recv_timeout(Duration::from_secs(5)), Ok(Some(false)));
}

#[test]
fn it_closes_as_declined_when_the_time_runs_out() {
    let (_live, answer) = start_confirm(4);
    assert_eq!(answer.recv_timeout(Duration::from_secs(15)), Ok(Some(false)));
}

#[test]
fn closing_the_elevated_window_is_no_approval_and_a_click_on_confirmar_is_one() {
    use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDOWN, WM_LBUTTONUP};
    let (live, answer) = start_confirm(90);
    // SAFETY: posting to our window.
    unsafe {
        let _ = PostMessageW(Some(live.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    assert_eq!(answer.recv_timeout(Duration::from_secs(5)), Ok(None));
    drop(live);

    // A real button press where "Confirmar" is painted approves.
    let (live, answer) = start_confirm(90);
    let ((w, h), scale) = live.geometry();
    let size = (w as f32 / scale, h as f32 / scale);
    let r = render(&confirm_state(90), WindowKind::Main, size, scale, &Interaction::default()).unwrap();
    let rect = r.output.widgets.iter().find(|w| w.id == ControlId::DialogPrimary).map(|w| w.rect).unwrap();
    let (cx, cy) = rect.center();
    let (px, py) = ((cx * scale) as isize, (cy * scale) as isize);
    let at = LPARAM(((py & 0xFFFF) << 16) | (px & 0xFFFF));
    // SAFETY: sends to our window from another thread.
    unsafe {
        SendMessageW(live.hwnd(), WM_MOUSEMOVE, Some(WPARAM(0)), Some(at));
        SendMessageW(live.hwnd(), WM_LBUTTONDOWN, Some(WPARAM(1)), Some(at));
        SendMessageW(live.hwnd(), WM_LBUTTONUP, Some(WPARAM(0)), Some(at));
    }
    assert_eq!(answer.recv_timeout(Duration::from_secs(5)), Ok(Some(true)));
}

impl Recording {
    fn view_sizes(&self) -> Vec<(u16, u16, u16)> {
        let c = self.commands.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        c.iter()
            .filter_map(|c| match c {
                swapdesk_ui::Command::SetViewSize { width, height, max_fps, .. } => Some((*width, *height, *max_fps)),
                _ => None,
            })
            .collect()
    }
}

/// F-VIEW-06: a real viewer window reports its video area in physical pixels, and again when it is resized; the same
/// size again sends nothing.
#[test]
fn a_viewer_window_reports_its_video_area_and_a_resize_reports_it_again() {
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GetWindowRect, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos,
    };
    let (live, backend) = start_recording(Scenario::Visualizador.prepared_state());
    let first = backend.view_sizes();
    assert_eq!(first.len(), 1, "one report for the first layout: {first:?}");
    let (w, h, _hz) = first[0];

    // The viewer is the thread's other window (the waker holds the main one).
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        // SAFETY: `lparam` is the vector passed below, alive for the whole enumeration.
        unsafe { (*(lparam.0 as *mut Vec<isize>)).push(hwnd.0 as isize) };
        true.into()
    }
    let mut all: Vec<isize> = Vec::new();
    // SAFETY: enumerates this test's window thread; the callback only reads the handles.
    unsafe {
        let _ = EnumThreadWindows(live.thread_id, Some(collect), LPARAM(std::ptr::from_mut(&mut all) as isize));
    }
    let main = live.hwnd().0 as isize;
    let viewer = all
        .into_iter()
        .filter(|h| *h != main)
        .map(|h| HWND(h as *mut std::ffi::c_void))
        .find(|h| {
            let mut r = windows::Win32::Foundation::RECT::default();
            // SAFETY: a plain read of a window of this thread.
            unsafe { GetWindowRect(*h, &mut r).is_ok() && r.right - r.left > 600 }
        })
        .expect("the viewer window");
    let mut rc = windows::Win32::Foundation::RECT::default();
    // SAFETY: our window, from another thread (blocks until the window thread has handled it).
    unsafe {
        let _ = GetWindowRect(viewer, &mut rc);
        let _ = SetWindowPos(
            viewer,
            None,
            0,
            0,
            rc.right - rc.left + 160,
            rc.bottom - rc.top + 100,
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }
    std::thread::sleep(Duration::from_millis(900));
    let after = backend.view_sizes();
    assert_eq!(after.len(), 2, "a resize reports once more: {after:?}");
    assert!(after[1].0 > w && after[1].1 > h, "the area grew from {w}x{h}: {after:?}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(backend.view_sizes().len(), 2, "nothing more while the size holds");
}

/// Slice 4 (FILES-UI.md 4.2): the viewer window is an OLE drop target, the main window is not. OLE marks a registered
/// window with the property `OleDropTargetInterface`; that it is set proves `OleInitialize` ran on the UI thread
/// (`RegisterDragDrop` fails on a thread that only called `CoInitializeEx`) and the registration went through.
#[test]
fn the_viewer_window_is_registered_as_a_drop_target_and_the_main_window_is_not() {
    use windows::Win32::Foundation::TRUE;
    use windows::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, GetPropW};
    use windows::core::{BOOL, w};

    // A mock run of the viewer scenario: its viewer window exists next to the main one.
    let live = Live::start(Scenario::Visualizador);
    let main = live.hwnd();
    struct Found(Vec<HWND>);
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Found` of the caller, alive for the whole enumeration.
        unsafe { (*(lparam.0 as *mut Found)).0.push(hwnd) };
        TRUE
    }
    let mut found = Found(Vec::new());
    // SAFETY: enumerates the windows of the shell thread; the pointer outlives the call.
    unsafe {
        let _ = EnumThreadWindows(live.thread_id, Some(collect), LPARAM(&mut found as *mut Found as isize));
    }
    // SAFETY: plain property reads of our windows.
    let registered = |h: HWND| !unsafe { GetPropW(h, w!("OleDropTargetInterface")) }.0.is_null();
    let top = found.0;
    assert!(!registered(main), "the main window takes no drops");
    assert!(top.iter().any(|&h| h != main && registered(h)), "the viewer window is a drop target ({top:?})");
}

// ----- the host banner: for the person at this computer alone (owner's report of 22/11/2026) -----

mod banner {
    use std::sync::atomic::AtomicUsize;

    use swapdesk_ui::tokens;
    use windows::Win32::Foundation::{LRESULT, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowRect, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WindowFromPoint,
    };
    use windows::core::BOOL;

    use super::*;
    use crate::banner_guard::{POINTER_PRESS, POINTER_RELEASE, WM_BANNER_POINTER, pack};

    /// The banner's window of a running shell: the tool window of its thread.
    fn banner_window(live: &Live) -> HWND {
        unsafe extern "system" fn each(hwnd: HWND, found: LPARAM) -> BOOL {
            // SAFETY: `found` is the address of the `HWND` the caller keeps on its stack for the whole enumeration.
            unsafe {
                if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
                    *(found.0 as *mut HWND) = hwnd;
                    return BOOL(0);
                }
            }
            BOOL(1)
        }
        let mut found = HWND::default();
        // SAFETY: enumerates the windows of the shell's thread; the callback writes into `found`.
        unsafe {
            let _ = EnumThreadWindows(live.thread_id, Some(each), LPARAM(std::ptr::from_mut(&mut found) as isize));
        }
        assert!(!found.0.is_null(), "the shell of a hosted session has a banner");
        found
    }

    fn rect_of(hwnd: HWND) -> RECT {
        let mut r = RECT::default();
        // SAFETY: a plain read of a window's rectangle.
        unsafe { GetWindowRect(hwnd, &mut r) }.expect("window rectangle");
        r
    }

    /// The pixels of the screen under `r`, as 0xRRGGBB, row by row.
    fn screen_pixels(r: RECT) -> Vec<u32> {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut out = vec![0u32; (w * h) as usize];
        // SAFETY: GDI objects made and released here; the buffer holds w * h pixels of 32 bits.
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bitmap.into());
            let _ = BitBlt(mem, 0, 0, w, h, Some(screen), r.left, r.top, SRCCOPY);
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            GetDIBits(mem, bitmap, 0, h as u32, Some(out.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
            SelectObject(mem, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
        out.iter().map(|p| p & 0x00FF_FFFF).collect()
    }

    /// Where "Desconectar" is in the banner's client pixels (its centre), and its rectangle there.
    fn disconnect_button(state: &swapdesk_ui::UiState, scale: f32) -> ((i32, i32), (i32, i32, i32, i32)) {
        let size = swapdesk_paint::banner_size_for(state);
        let shot = render(state, WindowKind::Banner, size, scale, &Interaction::default()).expect("render");
        let w = shot.output.widgets.iter().find(|w| w.id == ControlId::BannerDisconnect).expect("Desconectar");
        let (x, y, bw, bh) = (w.rect.x * scale, w.rect.y * scale, w.rect.w * scale, w.rect.h * scale);
        (((x + bw / 2.0) as i32, (y + bh / 2.0) as i32), (x as i32, y as i32, bw as i32, bh as i32))
    }

    fn close_to(pixel: u32, color: tokens::Color) -> bool {
        let near = |a: u32, b: u8| a.abs_diff(u32::from(b)) <= 24;
        near(pixel >> 16 & 0xFF, color.r) && near(pixel >> 8 & 0xFF, color.g) && near(pixel & 0xFF, color.b)
    }

    #[test]
    fn the_banner_lets_the_mouse_through_stays_on_the_screen_and_takes_a_real_click_from_its_hook() {
        // This test looks at the banner's own pixels: it is kept in screen captures for the test's run.
        crate::window::KEEP_BANNER_IN_CAPTURE.store(true, Ordering::Relaxed);
        let state = Scenario::Banner.prepared_state();
        let (live, backend) = start_recording(Scenario::Banner.prepared_state());
        let banner = banner_window(&live);
        // SAFETY: plain reads about our own window.
        let (ex, scale) =
            unsafe { (GetWindowLongPtrW(banner, GWL_EXSTYLE) as u32, GetDpiForWindow(banner) as f32 / 96.0) };
        assert!(ex & WS_EX_LAYERED.0 != 0 && ex & WS_EX_TRANSPARENT.0 != 0, "transparent to the mouse: {ex:#x}");
        assert!(ex & WS_EX_NOACTIVATE.0 != 0, "it never takes the keyboard: {ex:#x}");

        // The system's own hit test: whatever is found at the banner's button is not the banner.
        let r = rect_of(banner);
        let ((cx, cy), (bx, by, bw, bh)) = disconnect_button(&state, scale);
        // SAFETY: a plain query of the window at a screen point.
        let under = unsafe { WindowFromPoint(POINT { x: r.left + cx, y: r.top + cy }) };
        assert_ne!(under, banner, "a click at the banner's button goes to what lies under it");

        // It is on the screen all the same: the red of "Desconectar" where the button is drawn. The first paint of a
        // new window comes when the system gets to it (seconds, with the whole suite running beside this test), so
        // the screen is looked at until the button is there.
        let width = (r.right - r.left) as usize;
        let red_now = || {
            let pixels = screen_pixels(r);
            (by..by + bh)
                .flat_map(|y| (bx..bx + bw).map(move |x| (x, y)))
                .filter(|(x, y)| {
                    let p = pixels[*y as usize * width + *x as usize];
                    close_to(p, tokens::LIGHT.danger) || close_to(p, tokens::DARK.danger)
                })
                .count()
        };
        let started = Instant::now();
        let mut red = red_now();
        while red * 4 < (bw * bh) as usize && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
            red = red_now();
        }
        assert!(red * 4 >= (bw * bh) as usize, "the banner is drawn on the screen: {red} red pixels of {}", bw * bh);

        // A real click over the button, as the banner's hook tells it: the session is disconnected.
        assert_eq!(backend.disconnects(), 0);
        for kind in [POINTER_PRESS, POINTER_RELEASE] {
            // SAFETY: posts to our window.
            unsafe { PostMessageW(Some(banner), WM_BANNER_POINTER, WPARAM(kind), LPARAM(pack(cx, cy))) }.expect("post");
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(backend.disconnects(), 1, "the person at this computer still ends the session with a click");
        crate::window::KEEP_BANNER_IN_CAPTURE.store(false, Ordering::Relaxed);
    }

    static UNDER_CLICKS: AtomicUsize = AtomicUsize::new(0);

    /// A plain window of the test that counts the left clicks it gets, covering `r`.
    fn window_under(r: RECT) -> HWND {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, WM_LBUTTONDOWN, WNDCLASSW,
            WS_POPUP,
        };
        use windows::core::w;
        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
            if msg == WM_LBUTTONDOWN {
                UNDER_CLICKS.fetch_add(1, Ordering::SeqCst);
            }
            // SAFETY: the default procedure with the arguments as they came.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // SAFETY: a window class and a window of the test, on the calling thread.
        unsafe {
            let instance = GetModuleHandleW(None).expect("module");
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: w!("SwapdeskTestUnderBanner"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST,
                w!("SwapdeskTestUnderBanner"),
                w!("under the banner"),
                WS_POPUP,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("window under the banner");
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            hwnd
        }
    }

    #[test]
    #[ignore = "interactive: injects a real click over the banner and moves the pointer for a moment"]
    fn interactive_an_injected_click_over_the_banner_reaches_the_window_under_it_and_never_the_banner() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            DestroyWindow, DispatchMessageW, GetCursorPos, GetSystemMetrics, HWND_TOPMOST, MSG, PM_REMOVE,
            PeekMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
            SWP_NOMOVE, SWP_NOSIZE, SetCursorPos, SetWindowPos,
        };
        let state = Scenario::Banner.prepared_state();
        let (live, backend) = start_recording(Scenario::Banner.prepared_state());
        let banner = banner_window(&live);
        let r = rect_of(banner);
        // SAFETY: plain Win32 calls: the test's own window, the pointer's place, one injected click.
        unsafe {
            let scale = GetDpiForWindow(banner) as f32 / 96.0;
            let ((cx, cy), _) = disconnect_button(&state, scale);
            let under = window_under(r);
            // The banner stays above the test's window, as it stays above every window.
            let _ = SetWindowPos(banner, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let mut was = POINT::default();
            let _ = GetCursorPos(&mut was);
            let (vx, vy, vw, vh) = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            );
            let abs = |v: i32, origin: i32, span: i32| ((v - origin) * 65_535 + span / 2) / (span - 1).max(1);
            let mouse = |flags, dx, dy| INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
            };
            let (x, y) = (abs(r.left + cx, vx, vw), abs(r.top + cy, vy, vh));
            let moved = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
            let inputs = [mouse(moved, x, y), mouse(MOUSEEVENTF_LEFTDOWN, 0, 0), mouse(MOUSEEVENTF_LEFTUP, 0, 0)];
            assert_eq!(SendInput(&inputs, size_of::<INPUT>() as i32), 3, "the click was injected");
            // The test's window lives on this thread: its messages are pumped here.
            let until = Instant::now() + Duration::from_millis(800);
            let mut msg = MSG::default();
            while Instant::now() < until {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = SetCursorPos(was.x, was.y);
            let _ = DestroyWindow(under);
        }
        assert_eq!(UNDER_CLICKS.load(Ordering::SeqCst), 1, "the click reached the window under the banner");
        assert_eq!(backend.disconnects(), 0, "and the banner did nothing with it");
    }
}

/// The banner's "Desconectar" has a second way that needs neither the banner's window nor its hook (the pointer
/// over a window of an elevated program may never reach a hook of this process): the tray menu's entry.
#[test]
fn the_tray_menu_ends_the_session_this_computer_is_controlled_in() {
    use windows::Win32::UI::WindowsAndMessaging::WM_COMMAND;
    let (live, backend) = start_recording(Scenario::Banner.prepared_state());
    assert_eq!(backend.disconnects(), 0);
    // SAFETY: our window; the id the menu's entry sends.
    unsafe {
        let _ = PostMessageW(Some(live.hwnd()), WM_COMMAND, WPARAM(crate::tray::CMD_DISCONNECT), LPARAM(0));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(backend.disconnects(), 1, "the session is ended from the tray");
}

/// Review of 22/11/2026, H1: closed into the notification area the main window is hidden, and a request that needs
/// an answer (another computer asks to control this one) was drawn in that hidden window: its 20 s ran out and it
/// declined itself, with the person at the PC never knowing. The window now comes back for it.
#[test]
fn a_request_that_needs_an_answer_brings_back_a_window_closed_into_the_notification_area() {
    use swapdesk_ui::UiWaker;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    let backend = Arc::new(Scripted::default());
    let b = Arc::clone(&backend);
    let live = Live::start_with(move |w| {
        let host: Arc<dyn crate::ViewerHost> = Arc::new(NoHost);
        let backend: Arc<dyn UiBackend> = b;
        crate::run_with_host(swapdesk_ui::UiState::default(), backend, w, Vec::new(), Some(host)).expect("shell");
    });
    let hwnd = live.hwnd();
    // SAFETY: our window.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: a plain query of our window.
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "the close hides it into the notification area");

    let device = Scenario::Dispositivos.prepared_state().devices()[0].clone();
    backend.push(swapdesk_ui::UiEvent::IncomingRequest(swapdesk_ui::IncomingRequest {
        request_id: 7,
        device,
        kind: swapdesk_ui::RequestKind::Session { intent: swapdesk_ui::Intent::Control },
        preset: swapdesk_ui::Permissions::default(),
        timeout_s: 20,
    }));
    live.waker.wake();
    let started = Instant::now();
    // SAFETY: a plain query of our window.
    while !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        assert!(started.elapsed() < Duration::from_secs(5), "the request did not bring the window back");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ----- the keys of a captured session belong to the other computer (review of 22/11/2026, D1) -----

/// A pipeline that takes every key of the viewer window, as the real one does while a session has the keyboard.
struct KeysHost;

impl crate::ViewerHost for KeysHost {
    fn opened(&self, _surface: swapdesk_platform::NativeSurface, _area: swapdesk_platform::Rect, _scale: f32) {}
    fn resized(&self, _area: swapdesk_platform::Rect, _scale: f32) {}
    fn closed(&self) {}
    fn focus(&self, _focused: bool) {}
    fn mouse(&self, _msg: crate::MouseMsg) -> bool {
        false
    }
    fn captures_keyboard(&self) -> bool {
        true
    }
    fn key(&self, _msg: &crate::KeyMsg) -> bool {
        true
    }
    fn pointer_shape(&self) -> Option<Arc<swapdesk_platform::CursorShape>> {
        None
    }
}

/// The viewer window of a running shell: the window of its thread that is neither the main one nor the banner.
fn viewer_window(live: &Live) -> HWND {
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GWL_EXSTYLE, GetWindowLongPtrW, IsWindowVisible, WS_EX_TOOLWINDOW,
    };
    use windows::core::BOOL;
    struct Find {
        main: HWND,
        found: HWND,
    }
    unsafe extern "system" fn each(hwnd: HWND, find: LPARAM) -> BOOL {
        // SAFETY: `find` is the address of the `Find` the caller keeps for the whole enumeration.
        unsafe {
            let f = &mut *(find.0 as *mut Find);
            let tool = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
            if hwnd != f.main && !tool && IsWindowVisible(hwnd).as_bool() {
                f.found = hwnd;
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let mut find = Find { main: live.hwnd(), found: HWND::default() };
    // SAFETY: enumerates the windows of the shell's thread; the callback writes into `find`.
    unsafe {
        let _ = EnumThreadWindows(live.thread_id, Some(each), LPARAM(std::ptr::from_mut(&mut find) as isize));
    }
    assert!(!find.found.0.is_null(), "a session has its viewer window");
    find.found
}

/// With "Capturar atalhos do sistema" on, Alt+F4 is the other computer's: it closes the program there. It used to
/// close this window too, which ends the session, and Alt+Space opened this window's own menu.
#[test]
fn alt_f4_in_a_captured_session_does_not_end_the_session() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F4, VK_SPACE};
    use windows::Win32::UI::WindowsAndMessaging::{WM_SYSKEYDOWN, WM_SYSKEYUP};
    let backend = Arc::new(Recording::default());
    let b = Arc::clone(&backend);
    let live = Live::start_with(move |w| {
        let host: Arc<dyn crate::ViewerHost> = Arc::new(KeysHost);
        let backend: Arc<dyn UiBackend> = b;
        crate::run_with_host(Scenario::Visualizador.prepared_state(), backend, w, Vec::new(), Some(host))
            .expect("shell");
    });
    let viewer = viewer_window(&live);
    // lParam: repeat 1, the key's scan code, and bit 29 (Alt is down), as the system sends Alt+key.
    let alt =
        |scan: isize, up: bool| LPARAM(1 | (scan << 16) | (1 << 29) | if up { 0xC000_0000u32 as isize } else { 0 });
    for (vk, scan) in [(VK_F4, 0x3E), (VK_SPACE, 0x39)] {
        // SAFETY: sends to our window from another thread (it blocks until the window thread has answered).
        unsafe {
            SendMessageW(viewer, WM_SYSKEYDOWN, Some(WPARAM(usize::from(vk.0))), Some(alt(scan, false)));
            SendMessageW(viewer, WM_SYSKEYUP, Some(WPARAM(usize::from(vk.0))), Some(alt(scan, true)));
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(backend.disconnects(), 0, "the session goes on: the keys were the other computer's");
    // SAFETY: a plain query of our window.
    assert!(unsafe { IsWindow(Some(viewer)) }.as_bool(), "and its window is still there");
}
