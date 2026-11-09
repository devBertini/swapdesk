//! macOS system glue of Swapdesk (contract C3). The whole crate is empty on other systems, so the workspace still
//! builds on Windows.
//!
//! * [`pty::MacPtySpawner`]: the terminal's `PtySpawner`, a login shell of the process's own user on an `openpty` pair.
//! * [`actions::MacActions`]: the `ActionRunner` of the user scope (lock, sleep, restart, Activity Monitor, keep-awake).
#![cfg(target_os = "macos")]

pub mod actions;
pub mod pty;
