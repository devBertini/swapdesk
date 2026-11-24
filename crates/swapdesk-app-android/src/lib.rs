//! The Android app's shared library (docs/ANDROID.md 5): what `System.loadLibrary("swapdesk_android")` loads. Java
//! calls in through the JNI entry points of `jni_entry`; everything behind them is the Rust core the desktops run.
//!
//! [`phone_service`] is the app's service core, plain Rust that builds and is tested on every host. The JNI entry points
//! and the one core they start (`startup`) are Android only.
//!
//! The cargo feature `dev-harness` adds what a development build alone has, and no release library carries: the
//! self-test and the reachability probe of slice A1 (`selftest`), and the scripted viewer (`harness`).

#[cfg(all(target_os = "android", feature = "dev-harness"))]
mod harness;
#[cfg(target_os = "android")]
mod jni_entry;
pub mod phone_service;
#[cfg(all(target_os = "android", feature = "dev-harness"))]
pub mod selftest;
#[cfg(target_os = "android")]
mod startup;

/// The version the app shows and logs: the workspace's version and the protocol it speaks.
pub fn version() -> String {
    format!("{} (protocol 1.{})", env!("CARGO_PKG_VERSION"), swapdesk_proto::PROTO_MINOR)
}
