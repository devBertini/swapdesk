//! Audio for macOS (contract C3, `docs/CONTRACTS.md`): the viewer's `AudioSink` over the default output audio unit.
//! This crate holds the `unsafe` of the feature (Core Audio calls).
//!
//! * [`MacAudioSink`] (macOS only): plays the 48 kHz stereo interleaved f32 blocks the viewer's Opus path produces
//!   (`swapdesk-audio`'s `AudioReceiver::pop`, one 10 ms frame per playout tick) on the default output device, follows
//!   the default device when it changes, and has a linear volume. Why the audio unit and not an `AudioQueue` or
//!   `AVAudioEngine`: the module comment of `sink.rs`.
//! * [`Playout`] (every host): the lock-free ring and the real-time fill the render callback runs, with the prefill,
//!   underrun and volume behaviour pinned by unit tests that need no audio device.
//!
//! The jitter and loss concealment live upstream in `swapdesk-audio`; the sink adds only a bounded ring (60 ms) and a
//! short prefill (20 ms) so one late block is one short silence, never a stutter per device cycle. Audio that does not
//! fit the ring is dropped and counted, so a stalled device cannot grow the delay (the rule of the Windows sink).

mod playout;
#[cfg(target_os = "macos")]
mod sink;

pub use playout::{CHANNELS, Playout, PlayoutStats, SAMPLE_RATE, SinkConfig};
#[cfg(target_os = "macos")]
pub use sink::MacAudioSink;
