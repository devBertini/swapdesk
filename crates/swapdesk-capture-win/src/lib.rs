//! `ScreenSource` for Windows (contract C3, `docs/CONTRACTS.md`): DXGI Desktop Duplication, one D3D11 device per
//! adapter that owns an output, frames as BGRA GPU textures, the pointer channel, the display list and the
//! desktop-switch and display-change notifications (F-VIEW-02 host side; windows.md 1 and 2).
//!
//! The pure parts (display geometry, pointer shape conversion, error mapping) build on every host and are unit
//! tested; the Win32 and COM calls live in the Windows modules. This crate holds the `unsafe` of the feature.
//!
//! Where it runs: the capture runs in the system helper (contract C2.1), on a thread that owns no window and no hook.
//! A `DxgiScreen` is created on that thread and used only there. The watcher runs its own thread.

pub mod errors;
pub mod geometry;
pub mod pointer;

#[cfg(windows)]
mod clock;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod enumerate;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod hdr;
#[cfg(windows)]
mod screen;
#[cfg(windows)]
mod watcher;

#[cfg(windows)]
pub use clock::{QpcClock, qpc_frequency, qpc_ticks, ticks_to_mono};
#[cfg(windows)]
pub use desktop::InputDesktop;
#[cfg(windows)]
pub use enumerate::{AdapterReport, OutputReport, adapter_map, displays, video_memory_in_use};
#[cfg(windows)]
pub use gpu::{CpuImage, DevicePool, GpuDevice, GpuFrame};
#[cfg(windows)]
pub use hdr::scrgb_to_srgb8;
#[cfg(windows)]
pub use screen::{CaptureOptions, CaptureStats, DxgiScreen, gpu_payload};
#[cfg(windows)]
pub use watcher::{DisplayWatcher, WatchEvent};
