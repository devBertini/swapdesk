//! OS-free process logic of the Swapdesk service and helpers (stage 18, `docs/CONTRACTS.md` C2).
//!
//! [`helper`] is the helper's loop: generic over the C3 traits, it talks the C2.4 service-to-helper and
//! helper-to-service messages over any `Read` / `Write` pair. No `unsafe`, no async runtime.
//!
//! [`service`] is the OS-free core of `swapdesk-service.exe`: the session endpoint as host and viewer, the helper
//! links and the relays between them. It owns a tokio runtime.
//!
//! [`ui_hub`] serves the UI pipe (C2.4 `UiRequest` ops, notifications, viewer media) over any `Read` / `Write`, and
//! is the `HostUi` of the core. No OS either.

// Outside tests an unwrap is a crash of the service: return the error or use `expect` with the reason.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

pub mod accounts;
pub mod helper;
mod helper_pty;
pub mod netcache;
pub mod outbox;
pub mod proxy;
pub mod rpc;
pub mod scale;
pub mod service;
pub mod ui_hub;

#[cfg(test)]
mod helper_extras_tests;
#[cfg(test)]
mod helper_fs_tests;
#[cfg(test)]
mod helper_pty_tests;
#[cfg(test)]
mod helper_tests;
#[cfg(test)]
mod testpipe;
