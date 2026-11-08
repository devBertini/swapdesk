//! `NetInterfaces` for macOS (contract C3, `docs/CONTRACTS.md`): the interface list for the LAN guard with each
//! interface's kind, IPv4 addresses, MAC address and link state. Only physical Wi-Fi and Ethernet interfaces are ever
//! reported as [`swapdesk_platform::IfKind::WiFi`] or [`swapdesk_platform::IfKind::Ethernet`]; the guard approves those
//! two kinds and nothing else (D6, PROTOCOL.md 1).
//!
//! * [`classify`] is pure and builds on every host: it turns the facts (BSD name, the SystemConfiguration interface
//!   type, the `getifaddrs` flags) into an `IfKind`. Anything doubtful falls on the refused side.
//! * `mac_impl` reads the live facts and exists on macOS only: `getifaddrs(3)` for names, flags, IPv4 addresses and
//!   link-layer addresses, and SystemConfiguration's `SCNetworkInterfaceCopyAll` for each interface's type, since the
//!   BSD name alone cannot tell a physical `en` interface from a virtual one.
//!
//! The category and firewall fields of `NetInterface` are Windows facts and stay `None` here.
//!
//! Sources (read 2026-11-08): `getifaddrs(3)` man page on macOS; SCNetworkConfiguration reference,
//! <https://developer.apple.com/documentation/systemconfiguration/scnetworkinterface> and
//! `kSCNetworkInterfaceTypeIEEE80211` / `kSCNetworkInterfaceTypeEthernet`; the Rust bindings are `objc2-system-configuration`
//! 0.3.2, <https://docs.rs/objc2-system-configuration/0.3.2/objc2_system_configuration/struct.SCNetworkInterface.html>.
//! This crate holds the `unsafe` of the feature; `swapdesk-lan` forbids it.

pub mod classify;

#[cfg(target_os = "macos")]
mod mac_impl;

#[cfg(target_os = "macos")]
pub use mac_impl::{InterfaceReport, MacNetInterfaces, list_interfaces};
