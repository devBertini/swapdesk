//! macOS system glue of Swapdesk (contract C3). Everything but [`admin`]'s pure part is empty on other systems, so the
//! workspace still builds on Windows.
//!
//! * [`pty::MacPtySpawner`]: the terminal's `PtySpawner`, a login shell of the process's own user on an `openpty` pair.
//! * [`actions::MacActions`]: the `ActionRunner` of the user scope (lock, sleep, restart, Activity Monitor, keep-awake).
//! * [`http::MacHttp`]: the updater's HTTPS GET on `NSURLSession` (`docs/UPDATES.md` 5).
//! * [`fs`]: the free space of a volume and the Settings pane that grants access to folders (F-FILE-03).
//! * [`lock`]: whether this user's screen is locked (D24: the Mac hosts only an unlocked, logged-in session). The
//!   decision is pure and built on every host; the reading is macOS only.
//! * [`admin`]: the administrator confirmation through Authorization Services (`docs/ELEVATION-MAC.md`). Its outcome
//!   type and status mapping are pure and built on every host; the calls are macOS only.

#[cfg(target_os = "macos")]
pub mod actions;
pub mod admin;
#[cfg(target_os = "macos")]
pub mod disk;
#[cfg(target_os = "macos")]
pub mod fs;
#[cfg(target_os = "macos")]
pub mod http;
pub mod lock;
#[cfg(target_os = "macos")]
pub mod pty;
#[cfg(target_os = "macos")]
pub mod update_mac;

#[cfg(target_os = "macos")]
pub use fs::{FILES_AND_FOLDERS_PANE, FULL_DISK_ACCESS_PANE, free_bytes};

/// The uid of this process's user, as the user reference of the Mac app (C5.1: "macOS uid + name").
#[cfg(target_os = "macos")]
pub fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}
