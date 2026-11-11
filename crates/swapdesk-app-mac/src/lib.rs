//! `Swapdesk.app`'s logic (stage 23, D27, D29): the macOS program that wires the pieces the other lanes built.
//!
//! - the service in this process: a `ServiceCore` and the UI hub, and the shell's `ServiceBackend` over an in-memory
//!   pipe ([`service`], [`pipe`]);
//! - the shell: `swapdesk-shell-mac` windows, with this crate's [`slot`] as the video slot and the input lane's
//!   `MacInput` as the viewer input ([`app`]);
//! - the viewer media path: the service's `ViewerSink` -> mailbox -> [`decode`] thread (VideoToolbox) -> [`handoff`] ->
//!   [`main_side`] (the renderer, main thread only) ([`pipeline`]);
//! - the OS-free half of the pipeline is `swapdesk-app`'s `shared` module (mailbox, feedback, cursor, audio player,
//!   output side), the same one `Swapdesk.exe` uses.
//!
//! Built on every host: [`cli`], [`elevate`] (the administrator confirmation; only its system dialog is macOS), [`handoff`], [`layout_state`], [`login_switch`], [`pipe`], [`prefs`], [`service`], [`trace`] and the
//! command language of [`ui_script`] (so the Windows gate type-checks and tests them). macOS only: [`decode`], [`main_side`], [`pipeline`], [`slot`], [`app`],
//! [`selftest`].
//!
//! The crate holds the one `unsafe` of this lane (a decoded picture moved to the main thread, [`main_side::MainFrame`])
//! and the `atexit` registration of [`app`].

pub mod cli;
pub mod elevate;
pub mod handoff;
pub mod layout_state;
pub mod login_switch;
pub mod pipe;
pub mod prefs;
pub mod service;
pub mod trace;
pub mod ui_script;

#[cfg(target_os = "macos")]
pub mod app;
#[cfg(target_os = "macos")]
pub mod decode;
#[cfg(target_os = "macos")]
pub mod drive;
#[cfg(target_os = "macos")]
pub mod host;
#[cfg(target_os = "macos")]
pub mod login_item;
#[cfg(target_os = "macos")]
pub mod main_side;
#[cfg(target_os = "macos")]
pub mod pipeline;
#[cfg(target_os = "macos")]
pub mod selftest;
#[cfg(target_os = "macos")]
pub mod session_notices;
#[cfg(target_os = "macos")]
pub mod slot;
#[cfg(target_os = "macos")]
pub mod update;

#[cfg(all(test, target_os = "macos"))]
mod input_tests;
#[cfg(all(test, target_os = "macos"))]
mod media_tests;
