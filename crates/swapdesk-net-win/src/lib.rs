//! `NetInterfaces` for Windows (contract C3, `docs/CONTRACTS.md`): the adapter list for the LAN guard with each
//! adapter's kind, IPv4 addresses, network category (Network List Manager) and firewall state (F-FIND-02, F-FIND-04).
//!
//! [`classify`] is pure and builds on every host; the Win32 and COM calls live in `windows_impl` and exist on Windows
//! only. This crate holds the `unsafe` of the feature; `swapdesk-lan` forbids it.

pub mod classify;

#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod windows_impl;

#[cfg(windows)]
pub use watch::NetworkWatcher;
#[cfg(windows)]
pub use windows_impl::{AdapterReport, WindowsNetInterfaces, list_adapters, process_cpu_time};
