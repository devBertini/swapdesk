//! The keyboard grabber against the real hook (stage 16). Synthetic `SendInput` events go to a test window that is the
//! foreground window; an observer hook installed before the grabber sits after it in the hook chain, so it sees exactly
//! what the grabber let through: a swallowed key never reaches it.
#![cfg(windows)]

mod common;

use common::{TestWindow, aware, serial};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use swapdesk_capture_win::displays;
use swapdesk_input_win::grab::{GrabConfig, Injected};
use swapdesk_input_win::sink::Injector;
use swapdesk_input_win::{WinGrabber, WinInputSink, open_sink};
use swapdesk_keys::mode::KeyboardMode;
use swapdesk_keys::modifier;
use swapdesk_keys::shortcuts::{ReleaseKey, ViewerAction};
use swapdesk_platform::{InputGrabber, InputSink};
use swapdesk_proto::InputEvent;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_QUIT,
};
use windows::core::w;

/// What the observer hook saw: `(vk, scanCode, flags)`.
static SEEN: Mutex<Vec<(u32, u32, u32)>> = Mutex::new(Vec::new());

fn seen() -> MutexGuard<'static, Vec<(u32, u32, u32)>> {
    SEEN.lock().unwrap_or_else(|p| p.into_inner())
}

unsafe extern "system" fn observer(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION `lParam` points to a KBDLLHOOKSTRUCT valid for this call.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        seen().push((kb.vkCode, kb.scanCode, kb.flags.0));
    }
    // SAFETY: passes the unchanged arguments on.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

struct Observer {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl Observer {
    fn start() -> Self {
        seen().clear();
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        let join = std::thread::spawn(move || {
            // SAFETY: hook installed and removed on this thread, which runs its message loop.
            unsafe {
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                let hmod = GetModuleHandleW(None).ok().map(Into::into);
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(observer), hmod, 0).expect("observer hook");
                tx.send(GetCurrentThreadId()).unwrap();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(hook);
            }
        });
        Self { thread_id: rx.recv().expect("observer ready"), join: Some(join) }
    }

    fn vks(&self) -> Vec<u32> {
        seen().iter().map(|s| s.0).collect()
    }

    fn clear(&self) {
        seen().clear();
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: posts WM_QUIT to the observer thread's queue.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Fields drop in this order: the grabber first, the lock last.
struct Rig {
    grabber: WinGrabber,
    observer: Observer,
    sink: WinInputSink,
    win: TestWindow,
    rx: Receiver<InputEvent>,
    /// Usages this test injected: a person typing on the PC meanwhile must not break the exact-list checks.
    injected: Vec<u16>,
    _serial: common::Serial,
}

fn rig(config: GrabConfig) -> Rig {
    rig_with(config, None)
}

fn rig_with(config: GrabConfig, on_shortcut: Option<swapdesk_input_win::ShortcutHandler>) -> Rig {
    let serial = serial();
    aware();
    let displays = displays().expect("displays");
    let primary = displays.iter().find(|d| d.primary).expect("primary display").clone();
    let win = TestWindow::open((primary.bounds.x + 200, primary.bounds.y + 200), (640, 400));
    let mut sink = open_sink();
    sink.displays_changed(&displays);
    let observer = Observer::start();
    let (tx, rx) = sync_channel(256);
    let mut grabber = WinGrabber::new(config);
    if let Some(h) = on_shortcut {
        grabber.set_shortcut_handler(h);
    }
    grabber.start(tx).expect("start the grabber");
    Rig { grabber, observer, sink, win, rx, injected: Vec::new(), _serial: serial }
}

fn injected_only() -> GrabConfig {
    GrabConfig { injected: Injected::Only, ..GrabConfig::default() }
}

impl Rig {
    fn key(&mut self, usage: u16, down: bool, mods: u8) {
        self.win.guard();
        // The locks of the machine as they are, so the sink presses no lock key of its own.
        let locks = self.sink.injector().lock_state().unwrap_or(0);
        if !self.injected.contains(&usage) {
            self.injected.push(usage);
        }
        self.sink.apply(&InputEvent::Key { usage_page: 7, usage, down, modifiers: mods, locks }).expect("apply");
    }

    fn tap(&mut self, usage: u16, mods: u8) {
        self.key(usage, true, mods);
        self.key(usage, false, mods);
    }

    /// Key records count only for the usages this test injected (a person typing meanwhile is not its business).
    fn relevant(&self, e: &InputEvent) -> bool {
        match e {
            InputEvent::Key { usage, .. } => self.injected.contains(usage),
            _ => true,
        }
    }

    /// Events the grabber sent for the keys this test injected, until `n` arrived or `budget` passed (the window's loop
    /// keeps running meanwhile).
    fn events(&self, n: usize, budget: Duration) -> Vec<InputEvent> {
        let start = Instant::now();
        let mut out: Vec<InputEvent> = Vec::new();
        while out.len() < n && start.elapsed() < budget {
            self.win.settle(Duration::from_millis(10));
            while let Ok(e) = self.rx.try_recv() {
                if self.relevant(&e) {
                    out.push(e);
                }
            }
        }
        // A short extra look for strays.
        self.win.settle(Duration::from_millis(60));
        while let Ok(e) = self.rx.try_recv() {
            if self.relevant(&e) {
                out.push(e);
            }
        }
        out
    }
}

fn k(usage: u16, down: bool, mods: u8) -> InputEvent {
    InputEvent::Key { usage_page: 7, usage, down, modifiers: mods, locks: 0 }
}

/// Strips the `locks` byte, which depends on the machine's lock keys.
fn no_locks(evs: Vec<InputEvent>) -> Vec<InputEvent> {
    evs.into_iter()
        .map(|e| match e {
            InputEvent::Key { usage_page, usage, down, modifiers, .. } => {
                InputEvent::Key { usage_page, usage, down, modifiers, locks: 0 }
            }
            other => other,
        })
        .collect()
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn the_hook_installs_refuses_a_second_grabber_and_stops_cleanly() {
    let _s = serial();
    let (tx, _rx) = sync_channel(8);
    let mut g = WinGrabber::new(GrabConfig::default());
    assert!(!g.is_running());
    g.start(tx).expect("first start");
    assert!(g.is_running());
    let (tx2, _rx2) = sync_channel(8);
    assert!(g.start(tx2).is_err(), "already started");
    let mut other = WinGrabber::new(GrabConfig::default());
    let (tx3, _rx3) = sync_channel(8);
    assert!(other.start(tx3).is_err(), "one grabber per process");
    g.stop();
    assert!(!g.is_running());
    // It can start again, and stop is idempotent.
    let (tx4, rx4) = sync_channel(8);
    g.start(tx4).expect("restart");
    g.stop();
    g.stop();
    // Stopping tells the host to release everything the viewer held.
    assert_eq!(rx4.try_recv(), Ok(InputEvent::ReleaseAll));
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn synthetic_keys_are_seen_in_order_and_altgr_on_abnt2_drops_the_fake_ctrl() {
    let mut r = rig(injected_only());
    r.win.use_layout(w!("00010416"));
    r.win.settle(Duration::from_millis(50));
    // F13 (nothing types it), then AltGr + Q.
    r.tap(0x68, 0);
    r.key(0xE6, true, modifier::RALT);
    r.key(0x14, true, modifier::RALT);
    r.key(0x14, false, modifier::RALT);
    r.key(0xE6, false, 0);
    let got = no_locks(r.events(6, Duration::from_secs(3)));
    assert_eq!(
        got,
        vec![
            k(0x68, true, 0),
            k(0x68, false, 0),
            k(0xE6, true, modifier::RALT),
            k(0x14, true, modifier::RALT),
            k(0x14, false, modifier::RALT),
            k(0xE6, false, 0),
        ],
        "no Ctrl in the records"
    );
    // The hook chain did see Windows' fake left Ctrl; this is the evidence for the detection rule.
    let raw = seen().clone();
    println!("hook events for AltGr+Q on ABNT2 (vk, scanCode, flags): {raw:?}");
    assert!(
        raw.iter().any(|(vk, scan, _)| *vk == 0xA2 && *scan == 0x21D),
        "expected the fake left Ctrl (vk 0xA2, scan 0x21D) in {raw:?}"
    );
    assert!(r.win.is_foreground());
    r.grabber.stop();
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn system_keys_are_swallowed_while_enabled_and_the_release_is_sent_on_disable() {
    let mut r = rig(injected_only());
    r.grabber.set_grab_system_keys(true).unwrap();

    // Win: sent to the host, never seen further down the chain (Start does not open).
    r.tap(0xE3, 0);
    let got = no_locks(r.events(2, Duration::from_secs(3)));
    assert_eq!(got, vec![k(0xE3, true, modifier::LGUI), k(0xE3, false, 0)]);
    assert!(!r.observer.vks().contains(&0x5B), "Win reached the OS: {:?}", seen());
    assert!(r.win.is_foreground(), "the test window lost the foreground");

    // Alt+Tab, Alt+Esc, Ctrl+Esc: the Tab and Esc are swallowed, Alt and Ctrl pass.
    for (modifier_usage, bit, key, vk) in [
        (0xE2u16, modifier::LALT, 0x2Bu16, 0x09u32),
        (0xE2, modifier::LALT, 0x29, 0x1B),
        (0xE0, modifier::LCTRL, 0x29, 0x1B),
    ] {
        r.observer.clear();
        r.key(modifier_usage, true, bit);
        r.tap(key, bit);
        r.key(modifier_usage, false, 0);
        let got = no_locks(r.events(4, Duration::from_secs(3)));
        assert_eq!(
            got,
            vec![k(modifier_usage, true, bit), k(key, true, bit), k(key, false, bit), k(modifier_usage, false, 0)],
            "sent to the host: key {key:#x}"
        );
        let vks = r.observer.vks();
        assert!(!vks.contains(&vk), "key {key:#x} (vk {vk:#x}) reached the OS: {vks:?}");
        assert!(r.win.is_foreground(), "the test window lost the foreground after key {key:#x}");
    }

    // Other keys pass.
    r.observer.clear();
    r.tap(0x68, 0);
    let got = no_locks(r.events(2, Duration::from_secs(3)));
    assert_eq!(got, vec![k(0x68, true, 0), k(0x68, false, 0)]);
    assert!(r.observer.vks().contains(&0x7C), "F13 should pass: {:?}", r.observer.vks());

    // A swallowed key still held when the grab is switched off: the host gets ReleaseAll.
    r.key(0xE3, true, modifier::LGUI);
    let got = no_locks(r.events(1, Duration::from_secs(3)));
    assert_eq!(got, vec![k(0xE3, true, modifier::LGUI)]);
    r.grabber.set_grab_system_keys(false).unwrap();
    let got = r.events(1, Duration::from_secs(3));
    assert_eq!(got, vec![InputEvent::ReleaseAll]);

    // The sink still counts the Win key the hook swallowed as held: a new sink forgets it, so no stray Win release is
    // injected into the OS when the next key syncs its modifiers.
    r.sink = open_sink();
    // Off: F14 still flows and nothing is swallowed (checked with a key that is harmless to the OS).
    r.observer.clear();
    r.tap(0x69, 0);
    let got = no_locks(r.events(2, Duration::from_secs(3)));
    assert_eq!(got, vec![k(0x69, true, 0), k(0x69, false, 0)]);
    assert!(r.observer.vks().contains(&0x7D), "F14 should pass: {:?}", r.observer.vks());
    r.grabber.stop();
    assert!(r.win.is_foreground());
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn injected_events_are_ignored_by_default_so_a_local_host_never_feeds_itself() {
    let mut r = rig(GrabConfig::default());
    r.tap(0x68, 0);
    let got = r.events(1, Duration::from_millis(400));
    assert!(got.is_empty(), "{got:?}");
    assert!(r.observer.vks().contains(&0x7C), "the event itself did flow");
    r.grabber.stop();
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn text_mode_turns_printable_keys_into_unicode_text_and_keeps_shortcuts_physical() {
    let config = GrabConfig {
        mode: KeyboardMode::Auto,
        layouts_match: false,
        injected: Injected::Only,
        ..GrabConfig::default()
    };
    let mut r = rig(config);
    r.win.use_layout(w!("00010416"));
    r.win.settle(Duration::from_millis(50));
    // Shift+A, c-cedilla key, dead tilde + A, AltGr+Q, Ctrl+C.
    r.key(0xE1, true, modifier::LSHIFT);
    r.tap(0x04, modifier::LSHIFT);
    r.key(0xE1, false, 0);
    r.tap(0x33, 0);
    r.tap(0x34, 0);
    r.tap(0x04, 0);
    r.key(0xE6, true, modifier::RALT);
    r.tap(0x14, modifier::RALT);
    r.key(0xE6, false, 0);
    r.key(0xE0, true, modifier::LCTRL);
    r.tap(0x06, modifier::LCTRL);
    r.key(0xE0, false, 0);
    let got = no_locks(r.events(7, Duration::from_secs(4)));
    println!("text-mode events: {got:?}");
    let texts: Vec<String> = got
        .iter()
        .filter_map(|e| match e {
            InputEvent::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["A", "\u{e7}", "\u{e3}", "/"], "events: {got:?}");
    // The letter keys themselves were not sent as key records; the shortcut was.
    let letter_keys: Vec<&InputEvent> = got
        .iter()
        .filter(|e| matches!(e, InputEvent::Key { usage, .. } if matches!(*usage, 0x04 | 0x33 | 0x34 | 0x14)))
        .collect();
    assert!(letter_keys.is_empty(), "{letter_keys:?}");
    assert!(got.contains(&k(0x06, true, modifier::LCTRL)), "Ctrl+C stays physical: {got:?}");
    assert!(got.contains(&k(0x06, false, modifier::LCTRL)), "{got:?}");
    r.grabber.stop();
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn the_release_key_and_the_viewer_shortcuts_reach_the_handler_and_stay_from_the_os() {
    let fired: std::sync::Arc<Mutex<Vec<ViewerAction>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&fired);
    let handler: swapdesk_input_win::ShortcutHandler =
        std::sync::Arc::new(move |a| sink.lock().unwrap_or_else(|p| p.into_inner()).push(a));
    let config = GrabConfig { release: Some(ReleaseKey::CtrlAlt), ..injected_only() };
    let mut r = rig_with(config, Some(handler));
    r.grabber.set_grab_system_keys(true).unwrap();
    let got = |r: &Rig| {
        r.win.settle(Duration::from_millis(150));
        fired.lock().unwrap_or_else(|p| p.into_inner()).clone()
    };

    // Ctrl+Alt pressed and let go with nothing between: the keyboard is released.
    r.key(0xE0, true, modifier::LCTRL);
    r.key(0xE2, true, modifier::LCTRL | modifier::LALT);
    r.key(0xE2, false, modifier::LCTRL);
    r.key(0xE0, false, 0);
    assert_eq!(got(&r), vec![ViewerAction::ReleaseKeyboard]);

    // Ctrl+Alt+B shows the bar, Ctrl+Alt+End sends Ctrl+Alt+Del, Ctrl+Alt+Q disconnects, F11 is full screen; none of
    // their letters reaches the OS (the observer sits after the grabber in the hook chain).
    fired.lock().unwrap_or_else(|p| p.into_inner()).clear();
    r.observer.clear();
    let ca = modifier::LCTRL | modifier::LALT;
    r.key(0xE0, true, modifier::LCTRL);
    r.key(0xE2, true, ca);
    r.tap(0x05, ca);
    r.tap(0x4D, ca);
    r.tap(0x14, ca);
    r.key(0xE2, false, modifier::LCTRL);
    r.key(0xE0, false, 0);
    r.tap(0x44, 0);
    let seen_vks = {
        r.win.settle(Duration::from_millis(150));
        r.observer.vks()
    };
    let fired_now = got(&r);
    assert_eq!(
        fired_now,
        vec![ViewerAction::ToggleBar, ViewerAction::CtrlAltDel, ViewerAction::Disconnect, ViewerAction::FullScreen]
    );
    for vk in [0x42u32, 0x23, 0x51, 0x7A] {
        assert!(!seen_vks.contains(&vk), "vk {vk:#x} reached the OS: {seen_vks:?}");
    }
    r.grabber.stop();
}
