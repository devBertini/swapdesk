//! The Android app's shared library (docs/ANDROID.md 5): what `System.loadLibrary("swapdesk_android")` loads. Java
//! calls in through the JNI entry points of `jni_entry`; everything behind them is the Rust core the desktops run.
//!
//! Slice A1, the foundation: the library says its version and runs [`selftest`], which exercises on the device what
//! a cross-compile alone cannot prove (key generation, a pinned-key QUIC handshake, Opus).
//!
//! Compiles to nothing off Android (the D29 pattern), so the desktop gate builds an empty crate.
#![cfg(target_os = "android")]

mod jni_entry;
pub mod selftest;

/// The version the app shows and logs: the workspace's version and the protocol it speaks.
pub fn version() -> String {
    format!("{} (protocol 1.{})", env!("CARGO_PKG_VERSION"), swapdesk_proto::PROTO_MINOR)
}
