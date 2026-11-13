//! `Renderer` for Windows (contract C3, `docs/CONTRACTS.md`): decoded NV12 textures through the D3D11 video processor into
//! a DXGI flip-model composition swap chain (waitable, latency 1) that is the content of a visual under the shell's
//! toolbar visual (D18). The decoder and the swap chain share one D3D11 device: the shell's, from the `NativeSurface`.
//!
//! [`layout`] (Fit, 1:1, scroll, letterbox, the pointer mapping) is pure and builds on every host; the D3D11 and
//! DirectComposition code lives behind `cfg(windows)`. This crate holds the `unsafe` of the feature; every other crate
//! forbids it. A renderer is created on the viewer thread and used only from it: nothing here is `Send`.

pub mod layout;

#[cfg(windows)]
mod cursor;
#[cfg(windows)]
mod proc;
#[cfg(windows)]
mod renderer;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
pub use renderer::{Counters, Probe, RendererOptions, Timing, WinRenderer};
#[cfg(windows)]
pub use sys::{device_luid, now, simulate_device_removed};
