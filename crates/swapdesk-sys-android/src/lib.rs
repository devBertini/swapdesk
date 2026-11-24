//! Android system services for the core (docs/ANDROID.md 5): `NetInterfaces` from the network facts Java hands over,
//! sockets bound to the approved network, the `SecretStore` over Keystore-wrapped files, the `TraceSink` and the
//! `Clock`.
//!
//! [`network`] is Android only and holds this crate's `unsafe` (the NDK's multinetwork calls). Everything else is
//! plain Rust that builds and is tested on every host.

#[cfg(target_os = "android")]
pub mod network;
