//! Input that comes from a session never operates Swapdesk itself (owner, 22/11/2026).
//!
//! While this computer is controlled, the controller's mouse and keyboard reach every program on it. Swapdesk's own
//! windows are the exception: with them the controller could change what it is allowed to do here, accept a request
//! in the person's place or end other sessions. The host writes [`WIN_SESSION_INPUT_MARK`] into every input record
//! it injects (`swapdesk-input-win`), Windows hands that value to the thread that takes the message
//! (`GetMessageExtraInfo`), and the message loops of this program drop such a message before it is translated or
//! dispatched, so no window, card, dialog or text field of Swapdesk ever sees it. Input of the person at the machine
//! carries no such mark, whatever tool it comes through.
//!
//! What a loop of this program does not take is out of its reach: the clicks inside a menu the system runs (the tray
//! menu) are covered where the menu is opened, by [`press_from_session`].

use std::sync::atomic::{AtomicBool, Ordering};

use swapdesk_platform::WIN_SESSION_INPUT_MARK;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageExtraInfo, MSG, TranslateMessage, WM_KEYFIRST, WM_KEYLAST, WM_MOUSEFIRST, WM_MOUSELAST,
    WM_NCMOUSEMOVE, WM_NCXBUTTONDBLCLK,
};

/// What a test says the extra information of the next messages is, since a message a test posts carries none.
#[cfg(test)]
pub(crate) static TEST_EXTRA: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The extra information of the message this thread just took from its queue.
fn extra_now() -> usize {
    #[cfg(test)]
    {
        let told = TEST_EXTRA.load(Ordering::SeqCst);
        if told != 0 {
            return told;
        }
    }
    // SAFETY: a plain read of the calling thread's own state.
    unsafe { GetMessageExtraInfo() }.0 as usize
}

/// One message a loop of this program took from its queue: dropped when it is the controller's input, translated
/// and dispatched otherwise. Every loop of the program that takes input goes through here.
pub fn dispatch(msg: &MSG) {
    if from_session(msg.message, extra_now()) {
        return;
    }
    // SAFETY: the standard translation and dispatch of a message this thread took.
    unsafe {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
    }
}

/// Whether `message` is mouse or keyboard input: the client and the frame's mouse messages (the frame's close and
/// minimise buttons are the window's too) and the key and character messages.
fn is_input(message: u32) -> bool {
    (WM_MOUSEFIRST..=WM_MOUSELAST).contains(&message)
        || (WM_NCMOUSEMOVE..=WM_NCXBUTTONDBLCLK).contains(&message)
        || (WM_KEYFIRST..=WM_KEYLAST).contains(&message)
}

/// Whether a message a loop of this program took is the controller's input, to be dropped. `extra` is what
/// `GetMessageExtraInfo` gave for it.
pub fn from_session(message: u32, extra: usize) -> bool {
    extra == WIN_SESSION_INPUT_MARK && is_input(message)
}

/// Whether the last mouse button pressed anywhere on this desktop came from the session. Kept by the banner's hook,
/// which sees every mouse event with its mark while this computer is controlled.
static LAST_PRESS_FROM_SESSION: AtomicBool = AtomicBool::new(false);

/// The banner's hook saw a mouse button go down, with the extra information of that event.
pub fn note_press(extra: usize) {
    LAST_PRESS_FROM_SESSION.store(extra == WIN_SESSION_INPUT_MARK, Ordering::Relaxed);
}

/// Whether the press that led here (a click on the tray icon, which the system tells with a message of its own that
/// carries no mark) was the controller's.
pub fn press_from_session() -> bool {
    LAST_PRESS_FROM_SESSION.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_CHAR, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCLBUTTONDOWN, WM_PAINT, WM_SYSKEYDOWN,
        WM_TIMER,
    };

    use super::*;

    #[test]
    fn mouse_and_keyboard_messages_with_the_sessions_mark_are_dropped_and_nothing_else_is() {
        for input in [WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCLBUTTONDOWN, WM_KEYDOWN, WM_SYSKEYDOWN, WM_CHAR]
        {
            assert!(from_session(input, WIN_SESSION_INPUT_MARK), "{input:#x} from the session");
            assert!(!from_session(input, 0), "{input:#x} of the person at the machine");
            assert!(!from_session(input, 0xFF51_5700), "{input:#x} of a pen or a touch screen");
        }
        // What is not input is never dropped, whatever the last input's mark was.
        for other in [WM_PAINT, WM_TIMER, crate::tray::WM_TRAY] {
            assert!(!from_session(other, WIN_SESSION_INPUT_MARK), "{other:#x}");
        }
    }

    #[test]
    fn the_tray_knows_whether_the_last_press_was_the_controllers() {
        note_press(WIN_SESSION_INPUT_MARK);
        assert!(press_from_session());
        note_press(0);
        assert!(!press_from_session(), "the person's own click opens the tray as always");
    }
}
