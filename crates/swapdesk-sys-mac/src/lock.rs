//! Whether this user's screen is locked (D24, `docs/SPEC.md` F-UNAT-02): the Mac hosts only a logged-in, unlocked
//! session, because at the lock screen and the login window macOS gives a process neither capture nor injected input
//! (probe P4 to P6, `docs/RESEARCH.md`).
//!
//! * The source is the window server session's dictionary, `CGSessionCopyCurrentDictionary`
//!   (developer.apple.com/documentation/coregraphics/cgsessioncopycurrentdictionary(), read 2026-11-12: "information
//!   about the caller's window server session", nullable). Apple documents two of its keys, which `CGSession.h` defines
//!   as the strings `kCGSSessionOnConsoleKey` (the session is on a console) and `kCGSessionLoginDoneKey` (the login
//!   operation has been done). The lock itself is the key `CGSSessionScreenIsLocked`, present with a true value while
//!   the screen is locked. Neither Apple's pages nor the header document that key (UNPROVEN by a primary page; it is
//!   the one the system writes), so the reading fails closed: anything that is not plainly "on the console, logged in,
//!   not locked" counts as locked.
//! * [`screen_state_of`] is the whole decision, pure and tested on every host. [`read_screen_state`] is the thin OS
//!   call, macOS only.

/// What this process can read of its window server session, each fact as the dictionary gave it (`None`: the key was
/// absent or not a boolean).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionFacts {
    /// `kCGSessionOnConsoleKey`.
    pub on_console: Option<bool>,
    /// `kCGSessionLoginDoneKey`.
    pub login_done: Option<bool>,
    /// `CGSSessionScreenIsLocked`.
    pub screen_locked: Option<bool>,
}

/// Where this user's screen stands, for hosting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// On the console, logged in and not locked: the only state that can be hosted.
    Unlocked,
    /// The lock screen, or the login window before the login is done.
    Locked,
    /// The session is not the one on the console (another user took it by fast user switching).
    OffConsole,
    /// The dictionary could not be read, or does not say whether the session is on the console: treated as locked.
    Unreadable,
}

impl ScreenState {
    /// Whether a session may be hosted now.
    pub fn can_host(self) -> bool {
        self == Self::Unlocked
    }
}

/// The state a reading stands for. `None` is a dictionary that could not be had (`CGSessionCopyCurrentDictionary`
/// returned NULL: no window server session, as in a plain SSH login). Order matters: a session off the console is
/// reported as such before the lock, and nothing is `Unlocked` unless the console is positively this session's.
pub fn screen_state_of(facts: Option<SessionFacts>) -> ScreenState {
    let Some(f) = facts else { return ScreenState::Unreadable };
    match f.on_console {
        Some(false) => ScreenState::OffConsole,
        None => ScreenState::Unreadable,
        Some(true) if f.screen_locked == Some(true) || f.login_done == Some(false) => ScreenState::Locked,
        Some(true) => ScreenState::Unlocked,
    }
}

/// Reads this process's session now. Never shows anything, never blocks, safe on any thread.
#[cfg(target_os = "macos")]
pub fn read_screen_state() -> ScreenState {
    screen_state_of(read_facts())
}

#[cfg(target_os = "macos")]
fn read_facts() -> Option<SessionFacts> {
    use std::ffi::{c_char, c_void};

    // CoreFoundation and CoreGraphics are system frameworks; only the few calls this reading needs are declared.
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFGetTypeID(cf: *const c_void) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(boolean: *const c_void) -> u8;
        fn CFStringCreateWithCString(alloc: *const c_void, c_str: *const c_char, encoding: u32) -> *const c_void;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> *const c_void;
    }
    /// `kCFStringEncodingUTF8`.
    const UTF8: u32 = 0x0800_0100;

    /// The boolean under the key named `key`, or `None` when it is absent or not a boolean. The two documented keys
    /// are macros in `CGSession.h` (`CFSTR("kCGSSessionOnConsoleKey")`, `CFSTR("kCGSessionLoginDoneKey")`), not exported
    /// symbols, so each key is built from its text and released here.
    ///
    /// # Safety
    /// `dict` is a live CFDictionary.
    unsafe fn boolean(dict: *const c_void, key: &std::ffi::CStr) -> Option<bool> {
        // SAFETY: the caller's contract; the key string is created and released here, and the value is borrowed from
        // the dictionary ("get" rule) and only read.
        unsafe {
            let key = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), UTF8);
            if key.is_null() {
                return None;
            }
            let value = CFDictionaryGetValue(dict, key);
            let read =
                (!value.is_null() && CFGetTypeID(value) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(value) != 0);
            CFRelease(key);
            read
        }
    }

    // SAFETY: CGSessionCopyCurrentDictionary has no preconditions; the result is owned by this function (a "Copy") and
    // released below exactly once.
    unsafe {
        let dict = CGSessionCopyCurrentDictionary();
        if dict.is_null() {
            return None;
        }
        let facts = SessionFacts {
            on_console: boolean(dict, c"kCGSSessionOnConsoleKey"),
            login_done: boolean(dict, c"kCGSessionLoginDoneKey"),
            screen_locked: boolean(dict, c"CGSSessionScreenIsLocked"),
        };
        CFRelease(dict);
        Some(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(on_console: Option<bool>, login_done: Option<bool>, screen_locked: Option<bool>) -> Option<SessionFacts> {
        Some(SessionFacts { on_console, login_done, screen_locked })
    }

    #[test]
    fn a_logged_in_session_on_the_console_that_is_not_locked_can_host() {
        // The lock key is absent when the screen is not locked.
        assert_eq!(screen_state_of(facts(Some(true), Some(true), None)), ScreenState::Unlocked);
        assert_eq!(screen_state_of(facts(Some(true), Some(true), Some(false))), ScreenState::Unlocked);
        assert_eq!(screen_state_of(facts(Some(true), None, None)), ScreenState::Unlocked);
        assert!(ScreenState::Unlocked.can_host());
    }

    #[test]
    fn the_lock_screen_and_the_login_window_cannot_host() {
        assert_eq!(screen_state_of(facts(Some(true), Some(true), Some(true))), ScreenState::Locked);
        assert_eq!(screen_state_of(facts(Some(true), Some(false), None)), ScreenState::Locked);
        assert_eq!(screen_state_of(facts(Some(true), Some(false), Some(true))), ScreenState::Locked);
        assert!(!ScreenState::Locked.can_host());
    }

    #[test]
    fn a_session_another_user_pushed_off_the_console_cannot_host() {
        for locked in [None, Some(false), Some(true)] {
            assert_eq!(screen_state_of(facts(Some(false), Some(true), locked)), ScreenState::OffConsole);
        }
        assert!(!ScreenState::OffConsole.can_host());
    }

    #[test]
    fn what_cannot_be_read_counts_as_locked_never_as_unlocked() {
        assert_eq!(screen_state_of(None), ScreenState::Unreadable);
        // A dictionary that does not say it is on the console is not trusted, whatever else it says.
        assert_eq!(screen_state_of(facts(None, Some(true), None)), ScreenState::Unreadable);
        assert_eq!(screen_state_of(facts(None, None, None)), ScreenState::Unreadable);
        assert!(!ScreenState::Unreadable.can_host());
    }

    #[test]
    fn only_one_reading_in_the_whole_table_can_host() {
        let options = [None, Some(false), Some(true)];
        for on_console in options {
            for login_done in options {
                for screen_locked in options {
                    let state = screen_state_of(facts(on_console, login_done, screen_locked));
                    let expected = on_console == Some(true) && login_done != Some(false) && screen_locked != Some(true);
                    assert_eq!(state.can_host(), expected, "{on_console:?} {login_done:?} {screen_locked:?}");
                }
            }
        }
    }

    /// The real reading, on a Mac: it must not panic or hang whether or not this process has a window server session
    /// (a plain SSH login has none). The value itself is not asserted: it depends on who is at the Mac.
    #[cfg(target_os = "macos")]
    #[test]
    fn reading_the_real_session_returns_a_state_without_blocking() {
        let started = std::time::Instant::now();
        let state = read_screen_state();
        eprintln!("screen state of this process: {state:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // The same reading twice agrees when nothing changed in between.
        assert_eq!(state, read_screen_state());
    }
}
