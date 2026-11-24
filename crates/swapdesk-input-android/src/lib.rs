//! Android input for a session (docs/ANDROID.md 5): the touch, key and committed-text records Java hands over,
//! turned into the session's `InputEvent`s (the C3 `InputGrabber` role); later the `ClipboardProvider`. Slice A5 fills
//! it.
//!
//! Compiles to nothing off Android (the D29 pattern).
#![cfg(target_os = "android")]
