//! `Swapdesk.exe`'s logic: the command line, the connection to the service and the viewer pipeline (stage 18g).
//!
//! - [`cli`]: the arguments (`--mock <scenario>`, `--elevated <ticket>`, `--service-pid <pid>`, `--instance <n>`), parsed without side effects.
//! - [`connect`]: the UI pipe of this console session, opened with the identification-level check of C2.1 (the server
//!   must be the service the SCM reports, or the development `--service-pid`), the handshake on a helper thread with a
//!   deadline, and [`connect::DownBackend`] for when nothing answers (the UI then shows its service-down state).
//! - [`elevate`]: `WinElevator`, which starts this program as an administrator through UAC to confirm one action, and
//!   the instance that runs with `--elevated <ticket>` (ELEVATION.md).
//! - [`viewer`]: the viewer pipeline. The service's media reaches a [`viewer::ViewerPipeline`] through the
//!   `ViewerSink` of `swapdesk-ui`; the shell's window reaches it through `swapdesk_shell_win::ViewerHost`. Video never
//!   crosses the UI API (C6): a decode thread owns the decoder and the renderer.
//!
//! Everything but [`shared`] is Windows only (the macOS app is `swapdesk-app-mac`, which uses [`shared`]); the crate holds
//! no `unsafe`.
//!
//! - [`shared`]: the OS-free half of the viewer pipeline (mailbox, feedback, cursor, audio player, output side).

pub mod shared;

#[cfg(windows)]
pub mod cli;
#[cfg(windows)]
pub mod connect;
#[cfg(windows)]
pub mod elevate;
#[cfg(windows)]
pub mod viewer;

#[cfg(all(test, windows))]
mod tests {
    /// The shell opens the folder the panic hook writes into: one path, two definitions (the shell cannot depend on
    /// the system crate), kept equal here.
    #[test]
    fn the_shell_opens_the_folder_the_crash_marker_goes_to() {
        assert_eq!(swapdesk_shell_win::logs_dir(), swapdesk_sys_win::user_log_dir());
    }
}
