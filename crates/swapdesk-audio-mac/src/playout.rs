//! The playout side of the sink as plain data, with no Core Audio call: a lock-free single-producer, single-consumer
//! ring of interleaved stereo f32 frames, and the real-time `render` that fills the device's buffer from it.
//!
//! The producer is the thread that calls `AudioSink::write` (the viewer's playout thread, which gets a decoded 10 ms
//! frame from the jitter buffer in `swapdesk-audio` every tick); the consumer is the audio unit's render callback on the
//! real-time thread. Neither side takes a lock or allocates after construction (a lock in the render callback risks a
//! priority inversion and a click). The ring holds a bounded delay: audio that does not fit is dropped and counted, never
//! queued, so a stalled device cannot build latency (the same rule as the Windows sink, `swapdesk-audio-win`).
//!
//! Behaviour pinned by the unit tests below:
//! - Prefill: after `start` and after every starvation, the render outputs silence until `prefill` frames are queued, so
//!   one late block costs one short silence instead of a stutter at every device cycle.
//! - Underrun: a render that finds less than it needs plays what there is, pads with silence, counts one underrun and
//!   goes back to prefilling.
//! - Volume: a linear gain, ramped across each render so a change never clicks; 0.0 is mute.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Interleaved channels.
pub const CHANNELS: usize = 2;
/// Sample rate of every audio path (C3).
pub const SAMPLE_RATE: u32 = 48_000;

/// What a sink is built with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SinkConfig {
    /// Most audio the sink keeps ahead of the device, in milliseconds. 40 ms on Windows; a little more here because the
    /// audio unit pulls in device-sized bursts.
    pub capacity_ms: u32,
    /// Audio that must be queued before playing starts or resumes, in milliseconds.
    pub prefill_ms: u32,
    /// The I/O buffer of the default output device in frames (`kAudioDevicePropertyBufferFrameSize`), asked for at
    /// `start`; the device keeps its own size when the request fails or is `None`. 256 frames = 5.3 ms at 48 kHz.
    pub device_buffer_frames: Option<u32>,
    /// Linear gain, 0.0 (mute) to 1.0.
    pub volume: f32,
}

impl Default for SinkConfig {
    fn default() -> Self {
        Self { capacity_ms: 60, prefill_ms: 20, device_buffer_frames: Some(256), volume: 1.0 }
    }
}

fn frames_of_ms(ms: u32) -> usize {
    (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize
}

/// Counters, read by anyone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayoutStats {
    /// Frames accepted into the ring.
    pub frames_written: u64,
    /// Frames refused because the ring was full.
    pub dropped_frames: u64,
    /// Frames the device took from the ring.
    pub frames_played: u64,
    /// Starvations after playing had started (one per episode, not per render).
    pub underruns: u64,
}

pub struct Playout {
    ring: Box<[AtomicU32]>,
    /// Counts of values (not frames) ever read and written; the slot is `count % ring.len()`.
    read: AtomicU64,
    write: AtomicU64,
    prefill_values: usize,
    playing: AtomicBool,
    /// `f32` bits of the wanted gain and of the gain the last render ended on.
    gain_target: AtomicU32,
    gain_applied: AtomicU32,
    written: AtomicU64,
    dropped: AtomicU64,
    played: AtomicU64,
    underruns: AtomicU64,
    /// Set by the device watcher, taken by the producer (re-apply what a new device lost).
    device_changed: AtomicUsize,
}

fn clamp_gain(v: f32) -> f32 {
    if v.is_nan() { 1.0 } else { v.clamp(0.0, 1.0) }
}

impl Playout {
    pub fn new(config: &SinkConfig) -> Self {
        let cap = frames_of_ms(config.capacity_ms).max(frames_of_ms(config.prefill_ms) + 480).max(960) * CHANNELS;
        let gain = clamp_gain(config.volume).to_bits();
        Self {
            ring: (0..cap).map(|_| AtomicU32::new(0)).collect(),
            read: AtomicU64::new(0),
            write: AtomicU64::new(0),
            prefill_values: frames_of_ms(config.prefill_ms) * CHANNELS,
            playing: AtomicBool::new(false),
            gain_target: AtomicU32::new(gain),
            gain_applied: AtomicU32::new(gain),
            written: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            played: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            device_changed: AtomicUsize::new(0),
        }
    }

    /// Capacity in frames.
    pub fn capacity_frames(&self) -> usize {
        self.ring.len() / CHANNELS
    }

    fn queued_values(&self) -> usize {
        // Read the write side first: a concurrent render can only shrink the difference, never make it negative.
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        usize::try_from(w.saturating_sub(r)).unwrap_or(usize::MAX)
    }

    /// Frames written and not yet taken by the device.
    pub fn queued_frames(&self) -> usize {
        self.queued_values() / CHANNELS
    }

    /// Producer side. Accepts whole frames only; returns the frames accepted (the rest are dropped and counted).
    pub fn push(&self, samples: &[f32]) -> usize {
        let values = samples.len() & !1;
        let free = self.ring.len() - self.queued_values();
        let n = values.min(free) & !1;
        let w = self.write.load(Ordering::Relaxed);
        let cap = self.ring.len() as u64;
        for (i, s) in samples[..n].iter().enumerate() {
            self.ring[((w + i as u64) % cap) as usize].store(s.to_bits(), Ordering::Relaxed);
        }
        self.write.store(w + n as u64, Ordering::Release);
        self.written.fetch_add((n / CHANNELS) as u64, Ordering::Relaxed);
        self.dropped.fetch_add(((values - n) / CHANNELS) as u64, Ordering::Relaxed);
        n / CHANNELS
    }

    /// Consumer side, called from the render callback: fills `out` (interleaved stereo; an odd tail value is zeroed).
    pub fn render(&self, out: &mut [f32]) {
        let want = out.len() & !1;
        let avail = self.queued_values();
        let mut taken = 0;
        if !self.playing.load(Ordering::Relaxed) && avail >= self.prefill_values.max(CHANNELS) {
            self.playing.store(true, Ordering::Relaxed);
        }
        if self.playing.load(Ordering::Relaxed) {
            taken = want.min(avail) & !1;
            let r = self.read.load(Ordering::Relaxed);
            let cap = self.ring.len() as u64;
            for (i, o) in out[..taken].iter_mut().enumerate() {
                *o = f32::from_bits(self.ring[((r + i as u64) % cap) as usize].load(Ordering::Relaxed));
            }
            self.read.store(r + taken as u64, Ordering::Release);
            self.played.fetch_add((taken / CHANNELS) as u64, Ordering::Relaxed);
            if taken < want {
                self.underruns.fetch_add(1, Ordering::Relaxed);
                self.playing.store(false, Ordering::Relaxed);
            }
        }
        out[taken..].fill(0.0);
        self.apply_gain(&mut out[..taken]);
    }

    fn apply_gain(&self, out: &mut [f32]) {
        let target = f32::from_bits(self.gain_target.load(Ordering::Relaxed));
        let from = f32::from_bits(self.gain_applied.load(Ordering::Relaxed));
        self.gain_applied.store(target.to_bits(), Ordering::Relaxed);
        if out.is_empty() || (from == 1.0 && target == 1.0) {
            return;
        }
        let frames = out.len() / CHANNELS;
        for (i, frame) in out.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let g = if from == target { target } else { from + (target - from) * ((i + 1) as f32 / frames as f32) };
            for s in frame {
                *s *= g;
            }
        }
    }

    /// Linear gain, clamped to 0.0..=1.0 (NaN is ignored).
    pub fn set_volume(&self, volume: f32) {
        if !volume.is_nan() {
            self.gain_target.store(clamp_gain(volume).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.gain_target.load(Ordering::Relaxed))
    }

    pub fn stats(&self) -> PlayoutStats {
        PlayoutStats {
            frames_written: self.written.load(Ordering::Relaxed),
            dropped_frames: self.dropped.load(Ordering::Relaxed),
            frames_played: self.played.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
        }
    }

    /// Back to the state after `new` (not while a render may run).
    pub fn reset(&self) {
        self.read.store(0, Ordering::Release);
        self.write.store(0, Ordering::Release);
        self.playing.store(false, Ordering::Relaxed);
        self.written.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.played.store(0, Ordering::Relaxed);
        self.underruns.store(0, Ordering::Relaxed);
        let g = self.gain_target.load(Ordering::Relaxed);
        self.gain_applied.store(g, Ordering::Relaxed);
    }

    /// The default output device changed: counted here by the watcher.
    pub fn note_device_change(&self) {
        self.device_changed.fetch_add(1, Ordering::Relaxed);
    }

    /// Device changes seen since `new`.
    pub fn device_changes(&self) -> usize {
        self.device_changed.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SinkConfig {
        SinkConfig { capacity_ms: 60, prefill_ms: 20, device_buffer_frames: None, volume: 1.0 }
    }

    /// `frames` stereo frames whose two values are the frame index (left) and its negative (right).
    fn ramp(from: usize, frames: usize) -> Vec<f32> {
        (from..from + frames).flat_map(|i| [i as f32, -(i as f32)]).collect()
    }

    #[test]
    fn it_stays_silent_until_the_prefill_is_queued_then_plays_in_order() {
        let p = Playout::new(&cfg());
        let mut out = vec![9.0f32; 2 * 256];
        p.push(&ramp(0, 480));
        p.render(&mut out);
        assert!(out.iter().all(|v| *v == 0.0), "480 frames is below the 960-frame prefill");
        assert_eq!(p.stats().underruns, 0, "waiting to prefill is not an underrun");
        p.push(&ramp(480, 480));
        p.render(&mut out);
        assert_eq!(&out[..6], &[0.0, -0.0, 1.0, -1.0, 2.0, -2.0]);
        let mut next = vec![0.0f32; 2 * 256];
        p.render(&mut next);
        assert_eq!(next[0], 256.0);
        assert_eq!(p.stats().frames_played, 512);
        assert_eq!(p.queued_frames(), 960 - 512);
    }

    #[test]
    fn the_ring_wraps_around_without_losing_a_sample() {
        let p = Playout::new(&cfg());
        let mut got = Vec::new();
        let mut sent = 0;
        let mut out = vec![0.0f32; 2 * 240];
        for _ in 0..50 {
            sent += p.push(&ramp(sent, 480));
            p.render(&mut out);
            got.extend_from_slice(&out);
            p.render(&mut out);
            got.extend_from_slice(&out);
        }
        // After the first silent prefill, what plays is the ramp, contiguous, whatever the wrap points were.
        let first = got.iter().position(|v| *v != 0.0).expect("audio played");
        let mut expect = got[first];
        assert_eq!(expect, 1.0, "the ramp starts at its second frame (the first is all zeros)");
        for frame in got[first..].as_chunks::<2>().0 {
            if *frame == [0.0, 0.0] {
                break; // an underrun's silence ends the contiguous stretch
            }
            assert_eq!(frame[0], expect);
            assert_eq!(frame[1], -expect);
            expect += 1.0;
        }
        assert!(expect > 2880.0, "the wrap was crossed more than once: {expect}");
    }

    #[test]
    fn audio_that_does_not_fit_is_dropped_and_counted_never_queued() {
        let p = Playout::new(&cfg());
        let cap = p.capacity_frames();
        assert_eq!(cap, 2880);
        assert_eq!(p.push(&ramp(0, cap + 100)), cap);
        assert_eq!(p.stats().dropped_frames, 100);
        assert_eq!(p.queued_frames(), cap);
        assert_eq!(p.push(&ramp(0, 10)), 0);
        assert_eq!(p.stats().dropped_frames, 110);
        // An odd tail value is not a frame.
        let q = Playout::new(&cfg());
        assert_eq!(q.push(&[1.0, 2.0, 3.0]), 1);
        assert_eq!(q.stats().dropped_frames, 0);
    }

    #[test]
    fn a_starved_render_pads_with_silence_counts_one_underrun_and_prefills_again() {
        let p = Playout::new(&cfg());
        p.push(&ramp(0, 960));
        let mut out = vec![0.0f32; 2 * 600];
        p.render(&mut out); // 600 played, 360 left
        p.render(&mut out); // 360 played, 240 frames of silence
        assert_eq!(out[2 * 359], 959.0);
        assert!(out[2 * 360..].iter().all(|v| *v == 0.0));
        assert_eq!(p.stats().underruns, 1);
        p.render(&mut out);
        p.render(&mut out);
        assert_eq!(p.stats().underruns, 1, "silence while refilling is one episode");
        // A short top-up is below the prefill and stays silent; reaching it resumes playing.
        p.push(&ramp(1000, 480));
        p.render(&mut out);
        assert!(out.iter().all(|v| *v == 0.0));
        p.push(&ramp(1480, 480));
        p.render(&mut out);
        assert_eq!(out[0], 1000.0);
    }

    #[test]
    fn volume_scales_ramps_without_a_step_and_mutes() {
        let p = Playout::new(&cfg());
        let ones: Vec<f32> = vec![1.0; 2 * 960];
        p.push(&ones);
        let mut out = vec![0.0f32; 2 * 100];
        p.render(&mut out);
        assert!(out.iter().all(|v| *v == 1.0), "unity gain leaves the samples alone");
        p.set_volume(0.0);
        p.render(&mut out);
        assert!(out[0] < 1.0 && out[0] > 0.9, "the first frame of a fade is near the old gain: {}", out[0]);
        assert_eq!(out[2 * 99], 0.0, "the last frame reaches the target");
        for pair in out.as_chunks::<2>().0.windows(2) {
            assert!(pair[0][0] >= pair[1][0], "monotonic fade");
        }
        p.render(&mut out);
        assert!(out.iter().all(|v| *v == 0.0), "muted");
        p.set_volume(0.5);
        p.render(&mut out);
        assert_eq!(out[2 * 99], 0.5);
        p.set_volume(7.0);
        assert_eq!(p.volume(), 1.0);
        p.set_volume(-1.0);
        assert_eq!(p.volume(), 0.0);
        p.set_volume(f32::NAN);
        assert_eq!(p.volume(), 0.0, "NaN is ignored");
    }

    #[test]
    fn reset_returns_to_the_state_after_new_and_keeps_the_volume() {
        let p = Playout::new(&SinkConfig { volume: 0.25, ..cfg() });
        p.push(&ramp(0, 960));
        let mut out = vec![0.0f32; 2 * 10];
        p.render(&mut out);
        p.reset();
        assert_eq!(p.queued_frames(), 0);
        assert_eq!(p.stats(), PlayoutStats::default());
        assert_eq!(p.volume(), 0.25);
        p.render(&mut out);
        assert!(out.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn an_empty_or_odd_output_buffer_is_harmless() {
        let p = Playout::new(&cfg());
        p.render(&mut []);
        p.push(&ramp(0, 960));
        let mut odd = vec![5.0f32; 5];
        p.render(&mut odd);
        assert_eq!(odd[4], 0.0, "the half frame is zeroed");
        assert_eq!(odd[..4], [0.0, -0.0, 1.0, -1.0]);
    }

    #[test]
    fn a_producer_and_a_consumer_thread_agree_on_every_frame() {
        use std::sync::Arc;
        let p = Arc::new(Playout::new(&SinkConfig { capacity_ms: 60, prefill_ms: 1, ..cfg() }));
        let total = 200_000usize;
        let producer = {
            let p = Arc::clone(&p);
            std::thread::spawn(move || {
                let mut sent = 0;
                while sent < total {
                    let n = 480.min(total - sent);
                    // Frame values start at 1: a frame of zeros is the silence the consumer skips.
                    let accepted = p.push(&ramp(sent + 1, n));
                    sent += accepted;
                    if accepted < n {
                        std::thread::yield_now();
                    }
                    // A short pause keeps the ring from always being full.
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            })
        };
        let mut expect = 1usize;
        let mut out = vec![0.0f32; 2 * 128];
        while expect <= total {
            p.render(&mut out);
            for frame in out.as_chunks::<2>().0 {
                if *frame == [0.0, 0.0] {
                    continue; // silence between underruns
                }
                assert_eq!(frame[0], expect as f32, "frames must come out in order, none repeated or skipped");
                assert_eq!(frame[1], -(expect as f32));
                expect += 1;
            }
            std::thread::yield_now();
        }
        producer.join().expect("producer");
    }
}
