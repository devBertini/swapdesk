//! The `SendInput` injector: planned inputs to `INPUT` records, the virtual desktop from `GetSystemMetrics`, the lock
//! toggle state and the input-desktop follow.

use crate::plan::{CharKey, KbKind, MouseInput, RawInput};
use crate::sink::Injector;
use swapdesk_platform::{ErrorKind, PlatformError, Rect, Result};
use swapdesk_proto::MouseButton;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop, HDESK, OpenInputDesktop,
    SetThreadDesktop,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyExW, SendInput, VIRTUAL_KEY,
    VK_CAPITAL, VK_NUMLOCK, VK_SCROLL, VkKeyScanExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

fn os_error(op: &'static str, detail: String, code: i64) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: Some(code), detail }
}

fn kb(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

/// One planned input as the `INPUT` record `SendInput` takes.
pub(crate) fn to_input(raw: &RawInput) -> INPUT {
    match raw {
        RawInput::Key(k) => {
            let up = if k.up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
            match k.kind {
                KbKind::Scan { code, extended } => {
                    let ext = if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
                    kb(0, u16::from(code), KEYEVENTF_SCANCODE | ext | up)
                }
                KbKind::Vk { vk, scan, extended } => {
                    let ext = if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
                    kb(vk, u16::from(scan), ext | up)
                }
                KbKind::Unicode(unit) => kb(0, unit, KEYEVENTF_UNICODE | up),
            }
        }
        RawInput::Mouse(m) => match *m {
            MouseInput::MoveAbs { x, y } => {
                mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
            }
            MouseInput::Button { button, down } => match button {
                MouseButton::Left => mouse(0, 0, 0, if down { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }),
                MouseButton::Right => mouse(0, 0, 0, if down { MOUSEEVENTF_RIGHTDOWN } else { MOUSEEVENTF_RIGHTUP }),
                MouseButton::Middle => mouse(0, 0, 0, if down { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP }),
                MouseButton::Back => {
                    mouse(0, 0, u32::from(XBUTTON1), if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP })
                }
                MouseButton::Forward => {
                    mouse(0, 0, u32::from(XBUTTON2), if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP })
                }
            },
            // `mouseData` holds the signed delta as a DWORD.
            MouseInput::Wheel(d) => mouse(0, 0, d as u32, MOUSEEVENTF_WHEEL),
            MouseInput::HWheel(d) => mouse(0, 0, d as u32, MOUSEEVENTF_HWHEEL),
        },
    }
}

/// Makes the calling thread per-monitor DPI aware (v2) while it lives, so system metrics are physical pixels.
struct DpiScope(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);

impl DpiScope {
    fn enter() -> Self {
        // SAFETY: plain thread-state call; the previous context is restored in `drop`.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        // SAFETY: restores the context saved in `enter`.
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

/// The virtual desktop in physical pixels.
pub fn virtual_screen() -> Option<Rect> {
    let _dpi = DpiScope::enter();
    // SAFETY: plain metric queries.
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(Rect { x, y, width: u32::try_from(w).ok()?, height: u32::try_from(h).ok()? })
}

/// Toggle state of Caps, Num and Scroll Lock (bit 0, 1, 2) as the calling thread's input state sees it.
pub(crate) fn lock_toggles() -> u8 {
    let toggled = |vk: VIRTUAL_KEY| -> u8 {
        // SAFETY: plain key-state query.
        u8::from(unsafe { GetKeyState(i32::from(vk.0)) } & 1 != 0)
    };
    toggled(VK_CAPITAL) | (toggled(VK_NUMLOCK) << 1) | (toggled(VK_SCROLL) << 2)
}

/// `GENERIC_ALL`: the access the thread's desktop handle is opened with.
const GENERIC_ALL: u32 = 0x1000_0000;

/// Injects through `SendInput` on the calling thread's desktop.
///
/// `SendInput` is bound by UIPI and fails silently against a higher-integrity window (windows.md 5a): the return value
/// says how many records were queued, never whether a window received them, so callers check effects.
#[derive(Default)]
pub struct SendInputInjector {
    desktop: Option<HDESK>,
    original: Option<HDESK>,
}

impl SendInputInjector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Injector for SendInputInjector {
    fn send(&mut self, inputs: &[RawInput]) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let records: Vec<INPUT> = inputs.iter().map(to_input).collect();
        let size = i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(40);
        // SAFETY: `records` is a valid slice of fully initialised INPUT structures and `size` is `sizeof(INPUT)`.
        let sent = unsafe { SendInput(&records, size) };
        if usize::try_from(sent).ok() != Some(records.len()) {
            // SAFETY: reads the calling thread's last error.
            let code = unsafe { GetLastError() }.0;
            return Err(os_error(
                "SendInput",
                format!(
                    "{sent} of {} records queued (blocked by another input thread or the desktop is not ours)",
                    records.len()
                ),
                i64::from(code),
            ));
        }
        Ok(())
    }

    fn virtual_desktop(&self) -> Option<Rect> {
        virtual_screen()
    }

    fn lock_state(&self) -> Option<u8> {
        // `GetKeyState` answers from the calling thread's input state, which only moves while that thread takes
        // messages: a helper thread that never pumps would read a stale value and toggle locks over and over. A fresh
        // thread starts from the system's current state.
        std::thread::spawn(lock_toggles).join().ok()
    }

    fn key_for_char(&self, c: char) -> Option<CharKey> {
        let mut buf = [0u16; 2];
        let [unit] = c.encode_utf16(&mut buf) else { return None };
        // The layout of the thread that owns the foreground window: the one that will read the key.
        // SAFETY: plain queries with no pointer argument but the optional process id, which is not asked for.
        let layout = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None)) };
        // SAFETY: plain query on a character and a layout handle.
        let found = unsafe { VkKeyScanExW(*unit, layout) };
        if found == -1 {
            return None;
        }
        let [vk, state] = found.to_le_bytes();
        // Bit 0 is Shift; Ctrl, Alt (AltGr) and the layout's own bits leave the character to Unicode.
        if state & !1 != 0 {
            return None;
        }
        // SAFETY: plain query on a virtual key and a layout handle.
        let scan = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC, Some(layout)) };
        let code = u8::try_from(scan).ok().filter(|s| (1..0x80).contains(s))?;
        Some(CharKey { code, shift: state & 1 != 0, letter: vk.is_ascii_uppercase() })
    }

    fn follow_input_desktop(&mut self) -> Result<()> {
        // SAFETY: plain query; the returned handle belongs to the process and is not closed by us.
        let original = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();
        // SAFETY: no pointer arguments; the handle is closed below or kept in `self.desktop`.
        let new = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) }
            .map_err(|e| PlatformError {
                kind: ErrorKind::Unavailable,
                op: "OpenInputDesktop",
                os_code: Some(i64::from(e.code().0)),
                detail: e.message(),
            })?;
        // SAFETY: `new` is a live desktop handle just opened.
        if let Err(e) = unsafe { SetThreadDesktop(new) } {
            // SAFETY: `new` is ours and no thread uses it.
            let _ = unsafe { CloseDesktop(new) };
            return Err(PlatformError {
                kind: ErrorKind::Unavailable,
                op: "SetThreadDesktop",
                os_code: Some(i64::from(e.code().0)),
                detail: format!("{} (the thread must own no windows or hooks)", e.message()),
            });
        }
        if self.original.is_none() {
            self.original = original;
        }
        if let Some(old) = self.desktop.replace(new) {
            // SAFETY: the thread moved off `old`; it is ours.
            let _ = unsafe { CloseDesktop(old) };
        }
        Ok(())
    }
}

impl Drop for SendInputInjector {
    fn drop(&mut self) {
        if let Some(cur) = self.desktop.take() {
            if let Some(orig) = self.original {
                // SAFETY: puts the thread back on the desktop it started on.
                let _ = unsafe { SetThreadDesktop(orig) };
            }
            // SAFETY: ours; fails harmlessly when the thread could not leave it.
            let _ = unsafe { CloseDesktop(cur) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::KbInput;

    fn flags_of(i: &INPUT) -> u32 {
        // SAFETY: the tests build only keyboard records here.
        unsafe { i.Anonymous.ki.dwFlags.0 }
    }

    #[test]
    fn scan_records_carry_the_scancode_and_extended_flags() {
        let down = to_input(&RawInput::Key(KbInput { kind: KbKind::Scan { code: 0x38, extended: true }, up: false }));
        let up = to_input(&RawInput::Key(KbInput { kind: KbKind::Scan { code: 0x38, extended: true }, up: true }));
        assert_eq!(flags_of(&down), KEYEVENTF_SCANCODE.0 | KEYEVENTF_EXTENDEDKEY.0);
        assert_eq!(flags_of(&up), KEYEVENTF_SCANCODE.0 | KEYEVENTF_EXTENDEDKEY.0 | KEYEVENTF_KEYUP.0);
        // SAFETY: keyboard records.
        unsafe {
            assert_eq!(down.Anonymous.ki.wVk.0, 0);
            assert_eq!(down.Anonymous.ki.wScan, 0x38);
        }
    }

    #[test]
    fn unicode_records_use_the_unicode_flag_only() {
        let r = to_input(&RawInput::Key(KbInput { kind: KbKind::Unicode(0xE7), up: false }));
        assert_eq!(flags_of(&r), KEYEVENTF_UNICODE.0);
        // SAFETY: keyboard record.
        unsafe {
            assert_eq!(r.Anonymous.ki.wScan, 0xE7);
            assert_eq!(r.Anonymous.ki.wVk.0, 0);
        }
    }

    #[test]
    fn vk_records_keep_the_virtual_key_and_not_the_scancode_flag() {
        let r =
            to_input(&RawInput::Key(KbInput { kind: KbKind::Vk { vk: 0xB3, scan: 0x22, extended: true }, up: true }));
        assert_eq!(flags_of(&r), KEYEVENTF_EXTENDEDKEY.0 | KEYEVENTF_KEYUP.0);
        // SAFETY: keyboard record.
        unsafe {
            assert_eq!(r.Anonymous.ki.wVk.0, 0xB3);
        }
    }

    #[test]
    fn mouse_records() {
        let m = to_input(&RawInput::Mouse(MouseInput::MoveAbs { x: 100, y: 200 }));
        // SAFETY: mouse records.
        unsafe {
            assert_eq!(m.Anonymous.mi.dx, 100);
            assert_eq!(m.Anonymous.mi.dy, 200);
            assert_eq!(m.Anonymous.mi.dwFlags, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK);
        }
        let w = to_input(&RawInput::Mouse(MouseInput::Wheel(-120)));
        let h = to_input(&RawInput::Mouse(MouseInput::HWheel(60)));
        let x = to_input(&RawInput::Mouse(MouseInput::Button { button: MouseButton::Forward, down: true }));
        // SAFETY: mouse records.
        unsafe {
            assert_eq!(w.Anonymous.mi.mouseData as i32, -120);
            assert_eq!(w.Anonymous.mi.dwFlags, MOUSEEVENTF_WHEEL);
            assert_eq!(h.Anonymous.mi.mouseData, 60);
            assert_eq!(h.Anonymous.mi.dwFlags, MOUSEEVENTF_HWHEEL);
            assert_eq!(x.Anonymous.mi.mouseData, u32::from(XBUTTON2));
            assert_eq!(x.Anonymous.mi.dwFlags, MOUSEEVENTF_XDOWN);
        }
    }

    /// This computer's layout (any Latin one: US, ABNT2, ISO) has F on scan 0x21 and 1 on 0x02, Shift for capitals;
    /// a character no key types is left to Unicode.
    #[test]
    fn the_layout_in_use_gives_the_key_of_a_letter_and_a_digit() {
        let i = SendInputInjector::new();
        assert_eq!(i.key_for_char('f'), Some(CharKey { code: 0x21, shift: false, letter: true }));
        assert_eq!(i.key_for_char('F'), Some(CharKey { code: 0x21, shift: true, letter: true }));
        assert_eq!(i.key_for_char('1'), Some(CharKey { code: 0x02, shift: false, letter: false }));
        assert_eq!(i.key_for_char('\u{1F600}'), None, "outside the BMP");
        assert_eq!(i.key_for_char('\u{263A}'), None, "no key types it");
    }
}
