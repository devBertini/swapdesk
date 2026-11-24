//! Android system services for the core (docs/ANDROID.md 5): `NetInterfaces` from the network facts Java hands over,
//! sockets bound to the approved network (`android_setsocknetwork`), the `SecretStore` over Keystore-wrapped files,
//! the `TraceSink` and the `Clock`. Slice A2 fills it; the NDK calls are this crate's `unsafe`.
//!
//! Compiles to nothing off Android (the D29 pattern).
#![cfg(target_os = "android")]
