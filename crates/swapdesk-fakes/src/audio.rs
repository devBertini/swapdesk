//! `SineSource` and `RecordingAudioSink` (C7): the B3 audio-delay measurement.
//!
//! The source plays a 1 kHz sine at -12 dBFS (peak 0.251) and, at the start of every second of its timeline, a 5 ms
//! marker burst: a 3 kHz cosine at -3 dBFS (peak 0.708, starting at full amplitude so the onset is one sample).
//! The sink finds a marker onset as the first frame whose left sample exceeds 0.5 in magnitude with no such frame in
//! the 10 ms before it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_platform::{AudioBlock, AudioSink, AudioSource, Clock, ErrorKind, MonoMicros, PlatformError, Result};

use crate::lock;

const RATE: u64 = 48_000;
const BLOCK_FRAMES: u64 = 480; // 10 ms
const MARKER_FRAMES: u64 = 240; // 5 ms
const MARKER_PERIOD_FRAMES: u64 = RATE;
const SINE_AMPLITUDE: f32 = 0.251_188_64; // -12 dBFS
const MARKER_AMPLITUDE: f32 = 0.707_945_8; // -3 dBFS
const ONSET_THRESHOLD: f32 = 0.5;

fn err(kind: ErrorKind, op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.to_string() }
}

/// One marker the source produced: its sequence number and the `captured_at` of its first sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerLog {
    pub seq: u64,
    pub captured_at: MonoMicros,
}

pub struct SineSource {
    clock: Arc<dyn Clock>,
    realtime: bool,
    running: bool,
    start_at: MonoMicros,
    started: Instant,
    block_no: u64,
    log: Arc<Mutex<Vec<MarkerLog>>>,
}

impl SineSource {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            realtime: false,
            running: false,
            start_at: MonoMicros(0),
            started: Instant::now(),
            block_no: 0,
            log: Arc::default(),
        }
    }

    /// Blocks are delivered at 10 ms intervals of the wall clock instead of at once.
    pub fn realtime(mut self) -> Self {
        self.realtime = true;
        self
    }

    /// The markers produced so far; the handle stays valid after the source moves to another thread.
    pub fn log_handle(&self) -> Arc<Mutex<Vec<MarkerLog>>> {
        Arc::clone(&self.log)
    }

    pub fn marker_log(&self) -> Vec<MarkerLog> {
        lock(&self.log).clone()
    }
}

impl AudioSource for SineSource {
    /// `exclude_pid` is ignored: this source has no process to exclude.
    fn start(&mut self, _exclude_pid: Option<u32>) -> Result<()> {
        self.running = true;
        self.start_at = self.clock.now();
        self.started = Instant::now();
        self.block_no = 0;
        lock(&self.log).clear();
        Ok(())
    }

    fn next_block(&mut self, timeout: Duration) -> Result<Option<AudioBlock>> {
        if !self.running {
            return Err(err(ErrorKind::Closed, "SineSource.next_block", "not started"));
        }
        if self.realtime {
            let due = self.started + Duration::from_millis(10) * u32::try_from(self.block_no).unwrap_or(u32::MAX);
            let wait = due.saturating_duration_since(Instant::now());
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        let captured_at = MonoMicros(self.start_at.0 + self.block_no * 10_000);
        let first = self.block_no * BLOCK_FRAMES;
        let mut samples = Vec::with_capacity(BLOCK_FRAMES as usize * 2);
        for n in first..first + BLOCK_FRAMES {
            let in_second = n % MARKER_PERIOD_FRAMES;
            let v = if in_second < MARKER_FRAMES {
                if in_second == 0 {
                    let offset = (n - first) * 1_000_000 / RATE;
                    lock(&self.log).push(MarkerLog {
                        seq: n / MARKER_PERIOD_FRAMES,
                        captured_at: MonoMicros(captured_at.0 + offset),
                    });
                }
                MARKER_AMPLITUDE * (core::f32::consts::TAU * (in_second % 16) as f32 / 16.0).cos()
            } else {
                SINE_AMPLITUDE * (core::f32::consts::TAU * (n % 48) as f32 / 48.0).sin()
            };
            samples.push(v);
            samples.push(v);
        }
        self.block_no += 1;
        Ok(Some(AudioBlock { captured_at, samples }))
    }

    fn stop(&mut self) {
        self.running = false;
    }
}

/// A marker onset found in what the sink played: its frame index in the whole stream and the time it starts to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Onset {
    pub frame_index: u64,
    pub played_at: MonoMicros,
}

#[derive(Default)]
struct SinkState {
    started: bool,
    written_any: bool,
    frames: u64,
    play_end: u64,
    underruns: Vec<Duration>,
    onsets: Vec<Onset>,
    last_loud: Option<u64>,
    blocks: Vec<AudioBlock>,
}

/// Plays on the injected clock: a block written when the queue is empty starts at once, otherwise right after the
/// queued audio. Clones share the state.
#[derive(Clone)]
pub struct RecordingAudioSink {
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<SinkState>>,
}

impl RecordingAudioSink {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, state: Arc::default() }
    }

    pub fn onsets(&self) -> Vec<Onset> {
        lock(&self.state).onsets.clone()
    }

    /// Gaps with nothing queued, one entry per gap with its length.
    pub fn underruns(&self) -> Vec<Duration> {
        lock(&self.state).underruns.clone()
    }

    pub fn blocks(&self) -> Vec<AudioBlock> {
        lock(&self.state).blocks.clone()
    }
}

impl AudioSink for RecordingAudioSink {
    fn start(&mut self) -> Result<()> {
        let mut s = lock(&self.state);
        *s = SinkState { started: true, ..SinkState::default() };
        Ok(())
    }

    fn write(&mut self, block: &AudioBlock) -> Result<()> {
        let now = self.clock.now().0;
        let mut s = lock(&self.state);
        if !s.started {
            return Err(err(ErrorKind::Closed, "RecordingAudioSink.write", "not started"));
        }
        let play_start = if !s.written_any {
            now
        } else if now > s.play_end {
            let gap = now - s.play_end;
            s.underruns.push(Duration::from_micros(gap));
            now
        } else {
            s.play_end
        };
        s.written_any = true;
        let frames = (block.samples.len() / 2) as u64;
        for i in 0..frames {
            let g = s.frames + i;
            if block.samples[i as usize * 2].abs() > ONSET_THRESHOLD {
                if s.last_loud.is_none_or(|l| g - l > BLOCK_FRAMES) {
                    s.onsets.push(Onset { frame_index: g, played_at: MonoMicros(play_start + i * 1_000_000 / RATE) });
                }
                s.last_loud = Some(g);
            }
        }
        s.frames += frames;
        s.play_end = play_start + frames * 1_000_000 / RATE;
        s.blocks.push(block.clone());
        Ok(())
    }

    fn queued(&self) -> Duration {
        let now = self.clock.now().0;
        Duration::from_micros(lock(&self.state).play_end.saturating_sub(now))
    }

    fn stop(&mut self) {
        lock(&self.state).started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeClock;

    const T0: u64 = 5_000_000;

    #[test]
    fn source_levels_and_marker_shape() {
        let clock = FakeClock::new(MonoMicros(T0));
        let mut src = SineSource::new(Arc::new(clock));
        src.start(None).unwrap();
        let first = src.next_block(Duration::ZERO).unwrap().unwrap();
        assert_eq!(first.samples.len(), 960);
        let loud = first.samples.chunks(2).filter(|f| f[0].abs() > 0.5).count();
        assert!((60..=240).contains(&loud), "{loud}");
        assert!((first.samples[0] - MARKER_AMPLITUDE).abs() < 1e-6);
        // Second block of the first second: the sine only, peak -12 dBFS.
        for _ in 0..1 {
            src.next_block(Duration::ZERO).unwrap();
        }
        let b = src.next_block(Duration::ZERO).unwrap().unwrap();
        let peak = b.samples.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((peak - SINE_AMPLITUDE).abs() < 0.01, "{peak}");
        assert_eq!(b.captured_at, MonoMicros(T0 + 20_000));
    }

    fn run(blocks: u64, delay_us: u64, stall_at: Option<(u64, u64)>) -> (SineSource, RecordingAudioSink) {
        let clock = FakeClock::new(MonoMicros(T0));
        let mut src = SineSource::new(Arc::new(clock.clone()));
        let mut sink = RecordingAudioSink::new(Arc::new(clock.clone()));
        src.start(None).unwrap();
        sink.start().unwrap();
        let mut extra = 0;
        for n in 0..blocks {
            if let Some((at, us)) = stall_at
                && n == at
            {
                extra = us;
            }
            let b = src.next_block(Duration::ZERO).unwrap().unwrap();
            clock.set(MonoMicros(b.captured_at.0 + delay_us + extra));
            sink.write(&b).unwrap();
        }
        (src, sink)
    }

    #[test]
    fn marker_positions_match_the_log() {
        let (src, sink) = run(350, 0, None);
        let log = src.marker_log();
        assert_eq!(log.len(), 4); // seconds 0, 1, 2, 3
        let onsets = sink.onsets();
        assert_eq!(onsets.len(), log.len());
        for (o, m) in onsets.iter().zip(&log) {
            assert_eq!(o.frame_index, m.seq * RATE);
            assert_eq!(o.played_at, m.captured_at);
        }
        assert!(sink.underruns().is_empty());
    }

    #[test]
    fn onset_found_within_one_ms_on_clean_input_with_a_delay() {
        let (src, sink) = run(250, 37_000, None);
        for (o, m) in sink.onsets().iter().zip(&src.marker_log()) {
            let delay = o.played_at.0 - m.captured_at.0;
            assert!(delay.abs_diff(37_000) <= 1_000, "{delay}");
        }
        assert_eq!(sink.onsets().len(), 3);
    }

    #[test]
    fn stall_counts_one_underrun_and_queue_tracks_playback() {
        let (_, sink) = run(40, 0, Some((10, 25_000)));
        assert_eq!(sink.underruns(), vec![Duration::from_millis(25)]);
        assert_eq!(sink.blocks().len(), 40);
        let clock = FakeClock::new(MonoMicros(0));
        let mut sink = RecordingAudioSink::new(Arc::new(clock.clone()));
        sink.start().unwrap();
        sink.write(&AudioBlock { captured_at: MonoMicros(0), samples: vec![0.0; 960] }).unwrap();
        assert_eq!(sink.queued(), Duration::from_millis(10));
        clock.advance(Duration::from_millis(4));
        assert_eq!(sink.queued(), Duration::from_millis(6));
        clock.advance(Duration::from_millis(40));
        assert_eq!(sink.queued(), Duration::ZERO);
    }

    #[test]
    fn stopped_source_and_sink_report_closed() {
        let clock = FakeClock::default();
        let mut src = SineSource::new(Arc::new(clock.clone()));
        assert_eq!(src.next_block(Duration::ZERO).err().unwrap().kind, ErrorKind::Closed);
        let mut sink = RecordingAudioSink::new(Arc::new(clock));
        let b = AudioBlock { captured_at: MonoMicros(0), samples: vec![0.0; 960] };
        assert_eq!(sink.write(&b).err().unwrap().kind, ErrorKind::Closed);
    }
}
