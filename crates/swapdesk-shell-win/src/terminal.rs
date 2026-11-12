//! The terminal panel's Windows side: the clipboard, the virtual-key codes and the viewer window's mouse and key
//! messages. The panel itself (the model of the open shell, its output router, the grid drawn over the shared painter,
//! the selection and what a key means) is `swapdesk_paint::term`, the same on the Mac (F-XTERM-01).

use std::rc::Rc;
use std::sync::OnceLock;

use swapdesk_paint::term::{KeyAction, key_action};
pub use swapdesk_paint::term::{PUMP_BUDGET, TerminalPane, set_terminal_waker, terminal_output};
use swapdesk_termview::{Key, Mods};
use swapdesk_ui::{Command, ControlId, TermPhase, WindowKind};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::WindowsAndMessaging::{
    WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::app::Shared;
use crate::system::{get_x, get_y, hiword};
use crate::window::Window;

// ---------- the clipboard ----------

const CF_UNICODETEXT: u32 = 13;

/// The clipboard's text, if it holds some.
pub fn clipboard_text(hwnd: HWND) -> Option<String> {
    // SAFETY: the clipboard is opened for this window and closed on every path; the global memory handle belongs to the
    // clipboard and is locked only while the text is copied out, bounded by `GlobalSize`.
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let global = HGLOBAL(handle.0);
            let ptr = GlobalLock(global).cast::<u16>();
            if ptr.is_null() {
                return None;
            }
            let units = GlobalSize(global) / 2;
            let all = std::slice::from_raw_parts(ptr, units);
            let end = all.iter().position(|&u| u == 0).unwrap_or(units);
            let text = String::from_utf16_lossy(&all[..end]);
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

/// Puts `text` on the clipboard as Unicode text.
pub fn set_clipboard_text(hwnd: HWND, text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * 2;
    // SAFETY: the clipboard is opened for this window and closed on every path; the memory is allocated here, filled
    // while locked, and handed to the clipboard, which owns it after a successful `SetClipboardData` (freed here only when
    // it was refused).
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let global = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let ptr = GlobalLock(global).cast::<u16>();
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return None;
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
            let _ = GlobalUnlock(global);
            if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0))).is_err() {
                let _ = GlobalFree(Some(global));
                return None;
            }
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

// ---------- keys and mouse of the window ----------

/// The key a virtual-key code stands for, as the shared key handling reads it: a letter or the space as a lowercase
/// `Key::Char`, the keys that are not text by name, `None` for any other.
fn win_key(vk: u16, mods: Mods) -> Option<Key> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&vk) {
        return Some(Key::Char(char::from(vk as u8).to_ascii_lowercase()));
    }
    Some(match vk {
        v if v == VK_SPACE.0 => Key::Char(' '),
        v if v == VK_RETURN.0 => Key::Enter,
        v if v == VK_BACK.0 => Key::Backspace,
        v if v == VK_TAB.0 => {
            if mods.shift {
                Key::BackTab
            } else {
                Key::Tab
            }
        }
        v if v == VK_ESCAPE.0 => Key::Escape,
        v if v == VK_UP.0 => Key::Up,
        v if v == VK_DOWN.0 => Key::Down,
        v if v == VK_LEFT.0 => Key::Left,
        v if v == VK_RIGHT.0 => Key::Right,
        v if v == VK_HOME.0 => Key::Home,
        v if v == VK_END.0 => Key::End,
        v if v == VK_PRIOR.0 => Key::PageUp,
        v if v == VK_NEXT.0 => Key::PageDown,
        v if v == VK_INSERT.0 => Key::Insert,
        v if v == VK_DELETE.0 => Key::Delete,
        v if (VK_F1.0..=VK_F12.0).contains(&v) => Key::F((v - VK_F1.0 + 1) as u8),
        _ => return None,
    })
}

/// Offers a message of the viewer window to the terminal panel; `Some` when the panel took it (the window then does not
/// handle it). Keys go to the shell while the grid has the keyboard focus and a shell runs; the mouse selects, pastes and
/// scrolls over the grid.
pub(crate) fn message(shared: &Rc<Shared>, w: &Window, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if w.kind != WindowKind::Viewer {
        return None;
    }
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEMOVE | WM_RBUTTONDOWN | WM_MOUSEWHEEL => {
            mouse(shared, w, msg, wparam, lparam)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR => {
            key(shared, w, msg, wparam, lparam)
        }
        _ => None,
    }
}

fn running_terminal(shared: &Shared) -> Option<u32> {
    let st = shared.state.borrow();
    matches!(st.term_phase(), TermPhase::Open { .. }).then(|| st.term_id()).flatten()
}

fn write_to_shell(shared: &Rc<Shared>, term_id: u32, bytes: Vec<u8>) {
    if !bytes.is_empty() {
        shared.send_commands(vec![Command::TermWrite { term_id, bytes }]);
    }
}

fn copy_selection(shared: &Rc<Shared>, w: &Window) -> bool {
    let text = shared.terminal.borrow().selection_text();
    match text {
        Some(t) => set_clipboard_text(w.hwnd(), &t),
        None => false,
    }
}

fn paste(shared: &Rc<Shared>, w: &Window, term_id: u32) {
    let Some(text) = clipboard_text(w.hwnd()) else { return };
    let bytes = shared.terminal.borrow_mut().paste_bytes(&text);
    if let Some(b) = bytes {
        write_to_shell(shared, term_id, b);
    }
}

fn mouse(shared: &Rc<Shared>, w: &Window, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, ReleaseCapture, SetCapture};
    let scale = w.scale();
    let (px, py) = if msg == WM_MOUSEWHEEL {
        // The wheel message carries screen coordinates.
        let mut p = windows::Win32::Foundation::POINT { x: get_x(lparam.0), y: get_y(lparam.0) };
        // SAFETY: our window; converts a point to its client coordinates.
        unsafe {
            let _ = ScreenToClient(w.hwnd(), &mut p);
        }
        (p.x, p.y)
    } else {
        (get_x(lparam.0), get_y(lparam.0))
    };
    let (x, y) = (px as f32 / scale, py as f32 / scale);
    match msg {
        WM_LBUTTONDOWN => {
            if !shared.terminal.borrow().contains(x, y) {
                return None;
            }
            // SAFETY: a plain read of the system's double-click time.
            let double = u64::from(unsafe { GetDoubleClickTime() });
            let now = u64::try_from(click_clock().elapsed().as_millis()).unwrap_or(u64::MAX);
            let selecting = {
                let mut pane = shared.terminal.borrow_mut();
                pane.mouse_down(x, y, now, double);
                pane.selecting()
            };
            if running_terminal(shared).is_some() {
                w.focus_control(ControlId::TermGrid, false);
            }
            if selecting {
                // SAFETY: our own window takes the pointer while the selection is dragged.
                unsafe {
                    SetCapture(w.hwnd());
                }
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_MOUSEMOVE => {
            if !shared.terminal.borrow().selecting() {
                return None;
            }
            shared.terminal.borrow_mut().mouse_move(x, y);
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_LBUTTONUP => {
            if !shared.terminal.borrow().selecting() {
                return None;
            }
            shared.terminal.borrow_mut().mouse_up();
            // SAFETY: balances the capture taken on the button press.
            unsafe {
                let _ = ReleaseCapture();
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_RBUTTONDOWN => {
            if !shared.terminal.borrow().contains(x, y) {
                return None;
            }
            // Windows Terminal's convention: a selection is copied (and cleared), otherwise the clipboard is pasted.
            if copy_selection(shared, w) {
                shared.terminal.borrow_mut().clear_selection();
            } else if let Some(id) = running_terminal(shared) {
                paste(shared, w, id);
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        _ => {
            // WM_MOUSEWHEEL: three lines a notch, away from the user scrolls back into the history.
            if !shared.terminal.borrow().contains(x, y) {
                return None;
            }
            let notches = i32::from(hiword(wparam.0 as isize) as i16) as isize;
            shared.terminal.borrow_mut().scroll(notches * 3 / 120);
            w.invalidate();
            Some(LRESULT(0))
        }
    }
}

/// The origin of the click times (milliseconds since the first use).
fn click_clock() -> &'static std::time::Instant {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now)
}

fn keyboard_mods() -> Mods {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
    // SAFETY: plain key state reads.
    unsafe {
        Mods {
            shift: GetKeyState(i32::from(VK_SHIFT.0)) < 0,
            ctrl: GetKeyState(i32::from(VK_CONTROL.0)) < 0,
            alt: GetKeyState(i32::from(VK_MENU.0)) < 0,
        }
    }
}

fn key(shared: &Rc<Shared>, w: &Window, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F4, VK_SPACE};
    let _ = lparam;
    if w.focused_control() != Some(ControlId::TermGrid) {
        return None;
    }
    let term_id = running_terminal(shared)?;
    let alt_text = |unit: u16| -> Option<Vec<u8>> {
        // Alt+character: the character, escape-prefixed (meta), as xterm does.
        let mut bytes = vec![0x1b];
        bytes.extend(shared.terminal.borrow_mut().char_bytes(unit)?);
        Some(bytes)
    };
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u16;
            let mods = keyboard_mods();
            // Alt+F4 closes the window and Alt+Space opens its menu: the system's.
            if msg == WM_SYSKEYDOWN && (vk == VK_F4.0 || (vk == VK_SPACE.0 && !mods.ctrl)) {
                return None;
            }
            let (rows, alt_screen, app_cursor) = shared.terminal.borrow().modes_for_keys()?;
            match key_action(win_key(vk, mods), mods, rows, alt_screen, app_cursor) {
                KeyAction::Pass => return None,
                KeyAction::Send(bytes) => {
                    shared.terminal.borrow_mut().reset_view();
                    write_to_shell(shared, term_id, bytes);
                }
                KeyAction::Copy => {
                    if copy_selection(shared, w) {
                        shared.terminal.borrow_mut().clear_selection();
                    }
                }
                KeyAction::CopyOrInterrupt => {
                    if copy_selection(shared, w) {
                        shared.terminal.borrow_mut().clear_selection();
                    } else {
                        write_to_shell(shared, term_id, vec![0x03]);
                    }
                }
                KeyAction::Paste => paste(shared, w, term_id),
                KeyAction::Scroll(n) => shared.terminal.borrow_mut().scroll(n),
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_KEYUP => Some(LRESULT(0)),
        WM_CHAR => {
            let bytes = shared.terminal.borrow_mut().char_bytes(wparam.0 as u16);
            if let Some(b) = bytes {
                write_to_shell(shared, term_id, b);
                w.invalidate();
            }
            Some(LRESULT(0))
        }
        WM_SYSCHAR => {
            if let Some(b) = alt_text(wparam.0 as u16) {
                write_to_shell(shared, term_id, b);
                w.invalidate();
            }
            Some(LRESULT(0))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_keys_name_the_keys_the_shared_handling_reads() {
        let none = Mods::NONE;
        assert_eq!(win_key(u16::from(b'D'), none), Some(Key::Char('d')), "a letter, lowercase");
        assert_eq!(win_key(0x20, none), Some(Key::Char(' ')), "the space");
        assert_eq!(win_key(0x0D, none), Some(Key::Enter));
        assert_eq!(win_key(0x09, Mods { shift: true, ..none }), Some(Key::BackTab));
        assert_eq!(win_key(0x74, none), Some(Key::F(5)));
        assert_eq!(win_key(0x21, none), Some(Key::PageUp));
        assert_eq!(win_key(0x10, none), None, "Shift alone is no key");
        let ctrl = Mods { ctrl: true, ..none };
        assert_eq!(key_action(win_key(u16::from(b'C'), ctrl), ctrl, 30, false, false), KeyAction::CopyOrInterrupt);
        assert_eq!(key_action(win_key(u16::from(b'A'), none), none, 30, false, false), KeyAction::Pass, "text");
    }
}
