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
    GetClientRect, IsWindow, PostMessageW, SendMessageW, WM_APP, WM_CLOSE, WM_GETTEXTLENGTH, WM_MOUSEMOVE,
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
        let waker = Arc::new(Waker::new());
        let (tx, rx) = std::sync::mpsc::channel();
        let w = Arc::clone(&waker);
        let join = std::thread::spawn(move || {
            // SAFETY: a plain query of this thread's id.
            tx.send(unsafe { GetCurrentThreadId() }).expect("thread id");
            let backend: Arc<dyn UiBackend> = Arc::new(MockBackend::new(w.clone()));
            crate::run(scenario.prepared_state(), backend, w, Vec::new()).expect("shell");
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
        // SAFETY: closing our window ends the message loop of its thread.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
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
