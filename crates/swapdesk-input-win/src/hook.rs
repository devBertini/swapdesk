//! `InputGrabber` over a `WH_KEYBOARD_LL` hook on its own thread (F-INPUT-04, ui.md "Raw keys").
//!
//! The hook thread owns no window and runs only a message loop (the hook needs one, and a hook that misses
//! `LowLevelHooksTimeout` is silently removed, so the procedure only does table lookups and a `try_send`). Every
//! keyboard event goes to the host as a key record; while system-key capture is on, Win, Alt+Tab, Alt+Esc and Ctrl+Esc
//! are also kept from the viewer's own OS. Only one grabber exists per process: the hook procedure reaches its state
//! through a process-wide slot.

use crate::grab::{GrabConfig, GrabState, LlKey, Translated};
use std::sync::mpsc::{SyncSender, TrySendError, channel};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use swapdesk_platform::{ErrorKind, InputGrabber, PlatformError, Result};
use swapdesk_proto::InputEvent;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, HKL, ToUnicodeEx};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, HC_ACTION,
    KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_QUIT,
};
use windows::core::PCWSTR;

struct Ctx {
    state: GrabState,
    tx: SyncSender<InputEvent>,
    /// An event could not be queued (the consumer is behind): a `ReleaseAll` goes first as soon as there is room, so
    /// the host never keeps a key whose release was lost.
    lost: bool,
    dropped: u64,
}

static CTX: Mutex<Option<Ctx>> = Mutex::new(None);

fn ctx() -> MutexGuard<'static, Option<Ctx>> {
    CTX.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn send(ctx: &mut Ctx, event: InputEvent) {
    if ctx.lost {
        match ctx.tx.try_send(InputEvent::ReleaseAll) {
            Ok(()) => ctx.lost = false,
            Err(_) => {
                ctx.dropped += 1;
                return;
            }
        }
    }
    match ctx.tx.try_send(event) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        Err(TrySendError::Full(_)) => {
            ctx.lost = true;
            ctx.dropped += 1;
        }
    }
}

/// `ToUnicodeEx` flag: do not change the keyboard state (Windows 10 version 1607 and later). Without it every call
/// from the hook would consume or set the kernel's dead-key state, and the viewer's own typing would see it altered.
const TO_UNICODE_KEEP_STATE: u32 = 0x4;

/// What the viewer's layout types for a key: `ToUnicodeEx` against the foreground thread's layout, with the modifier
/// state the grabber tracks. The call leaves the dead-key state alone (`TO_UNICODE_KEEP_STATE`), so it can be repeated.
fn translate(key: &LlKey, mods: u8, locks: u8) -> Translated {
    // SAFETY: plain queries of the foreground window's thread and layout.
    let hkl = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None)) };
    translate_with(key, mods, locks, hkl)
}

/// [`translate`] against a given layout.
fn translate_with(key: &LlKey, mods: u8, locks: u8, hkl: HKL) -> Translated {
    let mut state = [0u8; 256];
    let mut set = |vks: &[usize]| {
        for v in vks {
            state[*v] = 0x80;
        }
    };
    use swapdesk_keys::modifier as m;
    if mods & m::LSHIFT != 0 {
        set(&[0x10, 0xA0]);
    }
    if mods & m::RSHIFT != 0 {
        set(&[0x10, 0xA1]);
    }
    if mods & m::LCTRL != 0 {
        set(&[0x11, 0xA2]);
    }
    if mods & m::RCTRL != 0 {
        set(&[0x11, 0xA3]);
    }
    if mods & m::LALT != 0 {
        set(&[0x12, 0xA4]);
    }
    if mods & m::RALT != 0 {
        // AltGr: layouts that have it read Ctrl and Alt together.
        set(&[0x12, 0xA5, 0x11, 0xA2]);
    }
    if locks & 1 != 0 {
        state[0x14] = 1;
    }
    if locks & 2 != 0 {
        state[0x90] = 1;
    }
    let mut buf = [0u16; 8];
    // SAFETY: a translation into a local buffer from a local state table.
    let n = unsafe { ToUnicodeEx(key.vk, key.scan & 0xFF, &state, &mut buf, TO_UNICODE_KEEP_STATE, Some(hkl)) };
    match n {
        0 => Translated::default(),
        n if n < 0 => Translated { text: None, dead: true },
        n => {
            let len = usize::try_from(n).unwrap_or(0).min(buf.len());
            Translated { text: Some(String::from_utf16_lossy(&buf[..len])), dead: false }
        }
    }
}

fn handle(key: LlKey) -> bool {
    let mut guard = ctx();
    let Some(c) = guard.as_mut() else {
        return false;
    };
    let locks = c.state.locks();
    let verdict = c.state.on_key(key, |k, mods| translate(k, mods, locks));
    for e in verdict.events {
        send(c, e);
    }
    verdict.swallow
}

/// Runs the swallow decision of the hook under `catch_unwind`: a panic must never unwind into the system's hook
/// dispatcher (undefined behaviour across the FFI boundary). On a panic the key is passed down the chain (`false`: not
/// swallowed) and a line naming the origin goes to the debugger output; the panic hook has already left its record.
///
/// Release builds use `panic = "abort"` (D15), where a panic ends the process before this returns: the guard protects
/// the unwind builds (tests, dev) and the panic hook's record is kept either way.
fn guarded(decide: impl FnOnce() -> bool) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(decide)) {
        Ok(swallow) => swallow,
        Err(_) => {
            trace_line("swapdesk: panic caught in the keyboard hook procedure");
            false
        }
    }
}

/// A line of the debugger output (names only, never content), as the shell's slow-work trace writes them.
fn trace_line(line: &str) {
    let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: the buffer is NUL-terminated and outlives the call.
    unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };
}

/// Test-only hook point: the next key the procedure sees panics once while this is set.
#[cfg(test)]
static INJECT_PANIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // SAFETY: for HC_ACTION `lParam` points to a KBDLLHOOKSTRUCT valid for this call.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let key = LlKey { vk: kb.vkCode, scan: kb.scanCode, flags: kb.flags.0 };
        let swallow = guarded(|| {
            #[cfg(test)]
            if INJECT_PANIC.swap(false, std::sync::atomic::Ordering::SeqCst) {
                panic!("injected panic in the keyboard hook");
            }
            handle(key)
        });
        if swallow {
            return LRESULT(1);
        }
    }
    // SAFETY: passes the unchanged arguments down the hook chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

struct Hooked {
    thread_id: u32,
    join: JoinHandle<()>,
}

fn unavailable(op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind: ErrorKind::Unavailable, op, os_code: None, detail: detail.to_owned() }
}

fn spawn_hook_thread() -> Result<Hooked> {
    let (ready_tx, ready_rx) = channel::<std::result::Result<u32, PlatformError>>();
    let join = std::thread::Builder::new()
        .name("swapdesk-keyboard-hook".to_owned())
        .spawn(move || {
            // SAFETY: the hook is installed and removed on this thread, which then runs its message loop; the procedure
            // is a plain function that reaches its state through the process-wide slot.
            unsafe {
                let mut msg = MSG::default();
                // Creates this thread's message queue before anyone can post to it.
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                let hmod = GetModuleHandleW(None).ok().map(Into::into);
                let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod, 0) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = ready_tx.send(Err(PlatformError {
                            kind: ErrorKind::Os,
                            op: "SetWindowsHookEx(WH_KEYBOARD_LL)",
                            os_code: Some(i64::from(e.code().0)),
                            detail: e.message(),
                        }));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(GetCurrentThreadId()));
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(hook);
            }
        })
        .map_err(|e| PlatformError {
            kind: ErrorKind::Os,
            op: "spawn hook thread",
            os_code: e.raw_os_error().map(i64::from),
            detail: e.to_string(),
        })?;
    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Ok(Hooked { thread_id, join }),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => Err(unavailable("keyboard hook", "the hook thread ended before it was ready")),
    }
}

/// The grabber. Create it in the viewer; `start` when the viewer window gains focus, `stop` when it loses it.
pub struct WinGrabber {
    config: GrabConfig,
    grab_system_keys: bool,
    hooked: Option<Hooked>,
}

impl WinGrabber {
    pub fn new(config: GrabConfig) -> Self {
        Self { config, grab_system_keys: false, hooked: None }
    }

    pub fn is_running(&self) -> bool {
        self.hooked.is_some()
    }

    /// New mode, layout comparison or swap setting; applies at once when the grabber runs.
    pub fn set_config(&mut self, config: GrabConfig) {
        self.config = config;
        if self.hooked.is_some()
            && let Some(c) = ctx().as_mut()
        {
            c.state.set_config(config);
        }
    }

    /// Events the consumer was too slow to take since `start`.
    pub fn dropped(&self) -> u64 {
        ctx().as_ref().map_or(0, |c| c.dropped)
    }
}

impl InputGrabber for WinGrabber {
    fn start(&mut self, events: SyncSender<InputEvent>) -> Result<()> {
        if self.hooked.is_some() {
            return Err(unavailable("InputGrabber.start", "already started"));
        }
        {
            let mut guard = ctx();
            if guard.is_some() {
                return Err(unavailable("InputGrabber.start", "another grabber of this process is running"));
            }
            let mut state = GrabState::new(self.config, crate::winsend::lock_toggles());
            state.set_grab_system_keys(self.grab_system_keys);
            *guard = Some(Ctx { state, tx: events, lost: false, dropped: 0 });
        }
        match spawn_hook_thread() {
            Ok(h) => {
                self.hooked = Some(h);
                Ok(())
            }
            Err(e) => {
                *ctx() = None;
                Err(e)
            }
        }
    }

    fn set_grab_system_keys(&mut self, on: bool) -> Result<()> {
        self.grab_system_keys = on;
        if self.hooked.is_some()
            && let Some(c) = ctx().as_mut()
            && c.state.set_grab_system_keys(on)
        {
            // A swallowed key is still held: the viewer's OS never saw it, so it will never release it.
            send(c, InputEvent::ReleaseAll);
        }
        Ok(())
    }

    fn stop(&mut self) {
        let Some(h) = self.hooked.take() else {
            return;
        };
        if let Some(c) = ctx().as_mut() {
            // Focus lost: the host releases whatever the viewer still held.
            c.state.reset();
            send(c, InputEvent::ReleaseAll);
        }
        // SAFETY: posts WM_QUIT to the hook thread's own queue; the thread then unhooks and ends.
        let _ = unsafe { PostThreadMessageW(h.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        let _ = h.join.join();
        *ctx() = None;
    }
}

impl Drop for WinGrabber {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_keys::modifier as m;
    use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};

    /// A dead key must not change the state: translating it twice gives the same answer. Without the flag the
    /// second call would compose the pending accent with itself and type a character instead of reporting a dead key.
    #[test]
    fn translating_a_dead_key_twice_gives_the_same_answer() {
        // SAFETY: reads the calling thread's layout.
        let hkl = unsafe { GetKeyboardLayout(0) };
        let mut found = None;
        for vk in (0xBAu32..=0xC0).chain(0xDB..=0xDF).chain([0x36, 0x37, 0x38, 0x39, 0x30]) {
            // SAFETY: a table lookup.
            let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) };
            for mods in [0, m::LSHIFT, m::RALT] {
                let key = LlKey { vk, scan, flags: 0 };
                if translate_with(&key, mods, 0, hkl).dead {
                    found = Some((key, mods));
                }
            }
        }
        let Some((key, mods)) = found else {
            eprintln!("SKIPPED: the active keyboard layout has no dead key on the probed keys");
            return;
        };
        let first = translate_with(&key, mods, 0, hkl);
        let second = translate_with(&key, mods, 0, hkl);
        assert!(first.dead);
        assert_eq!(first, second);
    }

    /// A panic in the decision does not unwind out of the hook procedure: the key is passed on, and the next key is
    /// decided normally. Runs the real procedure with a synthetic key struct (no key is injected into the system).
    #[test]
    fn a_panic_in_the_hook_body_passes_the_key_on() {
        assert!(!guarded(|| panic!("boom")), "a panic means: do not swallow");
        assert!(guarded(|| true), "a normal verdict passes through");

        let kb = KBDLLHOOKSTRUCT { vkCode: 0x41, scanCode: 0x1E, flags: Default::default(), time: 0, dwExtraInfo: 0 };
        let lparam = LPARAM(std::ptr::from_ref(&kb) as isize);
        INJECT_PANIC.store(true, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: `lparam` points to a KBDLLHOOKSTRUCT that outlives the call; no hook is installed and no context
        // is set, so the body returns `false` and `CallNextHookEx` passes the call on harmlessly.
        let r = unsafe { hook_proc(HC_ACTION as i32, WPARAM(0x100), lparam) };
        assert!(!INJECT_PANIC.load(std::sync::atomic::Ordering::SeqCst), "the panic point fired");
        assert_ne!(r, LRESULT(1), "the key was not swallowed");
        // SAFETY: as above.
        let again = unsafe { hook_proc(HC_ACTION as i32, WPARAM(0x100), lparam) };
        assert_ne!(again, LRESULT(1));
    }
}
