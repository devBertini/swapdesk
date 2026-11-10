//! macOS system glue of Swapdesk (contract C3). Everything but [`admin`]'s pure part is empty on other systems, so the
//! workspace still builds on Windows.
//!
//! * [`pty::MacPtySpawner`]: the terminal's `PtySpawner`, a login shell of the process's own user on an `openpty` pair.
//! * [`actions::MacActions`]: the `ActionRunner` of the user scope (lock, sleep, restart, Activity Monitor, keep-awake).
//! * [`admin`]: the administrator confirmation through Authorization Services (`docs/ELEVATION-MAC.md`). Its outcome
//!   type and status mapping are pure and built on every host; the calls are macOS only.

#[cfg(target_os = "macos")]
pub mod actions;
pub mod admin;
#[cfg(target_os = "macos")]
pub mod pty;
