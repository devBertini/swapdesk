//! The Android video path (docs/ANDROID.md 5): `AMediaCodec` decoding into the session surface's native window, as
//! `VideoDecoder` and `Renderer` in one, and `map_pointer`. Slice A4 fills it; the NDK calls are this crate's `unsafe`.
//!
//! Compiles to nothing off Android (the D29 pattern).
#![cfg(target_os = "android")]
