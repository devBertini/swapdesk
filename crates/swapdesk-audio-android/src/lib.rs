//! The Android `AudioSink` on AAudio in its low-latency mode (docs/ANDROID.md 5). Slice A6 fills it; the NDK calls
//! are this crate's `unsafe`.
//!
//! Compiles to nothing off Android (the D29 pattern).
#![cfg(target_os = "android")]
