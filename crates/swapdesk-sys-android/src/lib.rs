//! Android system services for the core (docs/ANDROID.md 5): `NetInterfaces` from the network facts Java hands over,
//! sockets bound to the approved network, the `SecretStore` over Keystore-wrapped files, the `TraceSink` and the
//! `Clock`.
//!
//! [`network`] and [`logcat`] are Android only and hold this crate's `unsafe` (the NDK's multinetwork calls and the
//! system log). Everything else is plain Rust that builds and is tested on every host.

pub mod clock;
pub mod facts;
#[cfg(target_os = "android")]
pub mod logcat;
#[cfg(target_os = "android")]
pub mod network;
pub mod sealed;
