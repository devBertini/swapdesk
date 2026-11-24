//! The Android shell of the shared interface (docs/ANDROID.md 5): the `Painter` that replays each frame onto a
//! canvas, the native text fields of the edit slots, the accessibility bridge and the effects table. Slice A3 fills
//! it.
//!
//! Compiles to nothing off Android (the D29 pattern).
#![cfg(target_os = "android")]
