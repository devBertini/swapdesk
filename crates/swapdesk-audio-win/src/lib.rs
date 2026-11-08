//! Audio for Windows (contract C3, `docs/CONTRACTS.md`; research windows.md 6): an `AudioSource` over WASAPI
//! loopback and an `AudioSink` over WASAPI render. This crate holds the `unsafe` of the feature.
//!
//! * [`WasapiLoopbackSource`]: event-driven shared-mode loopback of the default render endpoint. With
//!   `start(Some(pid))` it asks for process-exclusion loopback (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`,
//!   `EXCLUDE_TARGET_PROCESS_TREE`, Windows 10 build 20348 and later) so a viewer on the same PC is left out of the
//!   capture; where that is not available it falls back to plain loopback and says so through
//!   [`WasapiLoopbackSource::mode`]. Audio from applications in exclusive mode is not captured (windows.md 6a).
//! * [`WasapiRenderSink`]: shared-mode render on the default endpoint, 48 kHz stereo f32 in, the engine converts.
//!
//! Both deliver and accept the C3 unit: 48 kHz, stereo, interleaved f32. The source hands out 10 ms blocks
//! (960 values) whose `captured_at` is the QPC-based `MonoMicros` of the first sample; loopback delivers nothing
//! while nothing plays, so the source fills such pauses with silence to keep the timeline continuous. Objects are
//! created and used on one thread (C3); each initialises COM for that thread (MTA).
//!
//! The pure format conversion ([`convert`]) builds on every host.

pub mod convert;

#[cfg(windows)]
mod activate;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod com;
#[cfg(windows)]
mod render;

#[cfg(windows)]
pub use activate::{ProcessLoopbackReport, probe_process_loopback};
#[cfg(windows)]
pub use capture::{CaptureMode, CaptureStats, WasapiLoopbackSource};
#[cfg(windows)]
pub use com::qpc_micros;
#[cfg(windows)]
pub use render::{RenderStats, WasapiRenderSink};
