//! The input desktop, named (stage B13, D11, F-UNAT-01): which desktop receives the user's input now, as the `Desktop`
//! value a `HostState` carries.
//!
//! Microsoft Learn "Desktops" (win32/winstation/desktops): the interactive window station has three desktops, `Default`,
//! `ScreenSaver` and `Winlogon`; the system switches to `Winlogon` while a user logs on, on CTRL+ALT+DEL and when User
//! Account Control prompts. "OpenInputDesktop" (winuser) opens the desktop that receives input and
//! "GetUserObjectInformationW" with `UOI_NAME` returns the name of a desktop object (both pages read on 2026-11-12).
//!
//! Only the name `Default` is the user's own desktop. Everything else, and every failure to open or read the input
//! desktop, is [`Desktop::Secure`]: the question this answers is whether a viewer without `UNATTENDED` may see and
//! type into what is on screen, so a doubt is a no (the pure rule is [`classify_input_desktop`]). The lock screen,
//! the sign-in screen and UAC all live on `Winlogon`; the service tells the lock screen apart from the others with
//! the session's lock event (watcher.rs), the helper only says "not the user's desktop".
//!
//! `OpenInputDesktop`, `GetUserObjectInformationW` and `CloseDesktop` are looked up in `user32.dll` at run time,
//! as `sas.rs` does with `sas.dll`: this crate does not enable the `Win32_System_StationsAndDesktops` feature of
//! `windows-sys` (a `Cargo.toml` change belongs to the main session). The signatures are those of the `windows-sys`
//! 0.61.2 bindings.

use std::ffi::{OsStr, c_void};
use std::ptr::null_mut;
use std::sync::OnceLock;

use swapdesk_proto::Desktop;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use crate::sys::wide;

/// The name of the desktop the user works on.
pub const DEFAULT_DESKTOP: &str = "Default";

/// `UOI_NAME`: the name of the object, as a string (GetUserObjectInformationW).
const UOI_NAME: i32 = 2;
/// `DESKTOP_READOBJECTS`: reading the object is all the probe asks for.
const DESKTOP_READOBJECTS: u32 = 1;
/// Room for a desktop name in UTF-16 units (the names are short; a longer one is not `Default`, so it is refused).
const NAME_UNITS: usize = 256;

/// The desktop an input desktop of this name (`None`: it could not be opened or read) stands for.
///
/// Fails closed: only the user's `Default` is `Default`.
pub fn classify_input_desktop(name: Option<&str>) -> Desktop {
    match name {
        Some(n) if n.eq_ignore_ascii_case(DEFAULT_DESKTOP) => Desktop::Default,
        _ => Desktop::Secure,
    }
}

type OpenInputDesktopFn = unsafe extern "system" fn(flags: u32, inherit: i32, access: u32) -> *mut c_void;
type GetUserObjectInformationWFn =
    unsafe extern "system" fn(object: *mut c_void, index: i32, info: *mut c_void, length: u32, needed: *mut u32) -> i32;
type CloseDesktopFn = unsafe extern "system" fn(desktop: *mut c_void) -> i32;

#[derive(Clone, Copy)]
struct Procs {
    open_input: OpenInputDesktopFn,
    get_info: GetUserObjectInformationWFn,
    close: CloseDesktopFn,
}

/// The three functions of `user32.dll`, looked up once. The module is never freed: `user32.dll` stays loaded for the
/// life of any process that has a window station, and the pointers must stay valid.
fn procs() -> Option<Procs> {
    static PROCS: OnceLock<Option<Procs>> = OnceLock::new();
    *PROCS.get_or_init(|| {
        let name = wide(OsStr::new("user32.dll")).ok()?;
        // SAFETY: `name` is a null-terminated wide string; no file handle; the flag restricts the search to System32.
        let module = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return None;
        }
        // SAFETY: `module` is a loaded module and the names are null-terminated ANSI strings.
        let find = |name: &core::ffi::CStr| unsafe { GetProcAddress(module, name.as_ptr().cast()) };
        let open_input = find(c"OpenInputDesktop")?;
        let get_info = find(c"GetUserObjectInformationW")?;
        let close = find(c"CloseDesktop")?;
        // SAFETY: each cast gives the function pointer the signature of its documented function; only the type changes.
        unsafe {
            Some(Procs {
                open_input: core::mem::transmute::<unsafe extern "system" fn() -> isize, OpenInputDesktopFn>(
                    open_input,
                ),
                get_info: core::mem::transmute::<unsafe extern "system" fn() -> isize, GetUserObjectInformationWFn>(
                    get_info,
                ),
                close: core::mem::transmute::<unsafe extern "system" fn() -> isize, CloseDesktopFn>(close),
            })
        }
    })
}

/// The name of the input desktop now, or `None` when this process cannot open it (a process below SYSTEM cannot open
/// `Winlogon`; that refusal is itself the answer) or read its name.
pub fn input_desktop_name() -> Option<String> {
    let p = procs()?;
    // SAFETY: plain call with no pointer arguments; a null result is a failure and is handled below.
    let desktop = unsafe { (p.open_input)(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return None;
    }
    let mut buf = [0u16; NAME_UNITS];
    let mut needed = 0u32;
    // SAFETY: `desktop` is the live handle opened above; the buffer holds `NAME_UNITS` units and its size in bytes is
    // passed; `needed` is a live out-parameter.
    let ok = unsafe {
        (p.get_info)(desktop, UOI_NAME, buf.as_mut_ptr().cast(), (NAME_UNITS * size_of::<u16>()) as u32, &mut needed)
    };
    // SAFETY: the handle is ours (opened above) and is closed once; a failure to close has no remedy.
    unsafe { (p.close)(desktop) };
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(NAME_UNITS);
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// The input desktop as a `Desktop`: [`classify_input_desktop`] of [`input_desktop_name`].
pub fn input_desktop() -> Desktop {
    classify_input_desktop(input_desktop_name().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_users_own_desktop_is_default() {
        assert_eq!(classify_input_desktop(Some("Default")), Desktop::Default);
        assert_eq!(classify_input_desktop(Some("default")), Desktop::Default);
    }

    #[test]
    fn winlogon_the_screen_saver_and_anything_unknown_are_secure() {
        for name in ["Winlogon", "WINLOGON", "Screen-saver", "ScreenSaver", "", "Default ", "Defaultx"] {
            assert_eq!(classify_input_desktop(Some(name)), Desktop::Secure, "{name:?}");
        }
    }

    #[test]
    fn a_desktop_that_cannot_be_opened_or_read_is_secure() {
        assert_eq!(classify_input_desktop(None), Desktop::Secure);
    }

    /// Reads only: a development run is on the user's own desktop, so the probe reaches `Default` there; with the
    /// screen locked while the test runs it may not, so the assertion is on the name's shape, not on the value.
    #[test]
    fn the_probe_reads_a_name_on_this_machine() {
        if let Some(name) = input_desktop_name() {
            assert!(!name.is_empty());
        }
        let _ = input_desktop();
    }
}
