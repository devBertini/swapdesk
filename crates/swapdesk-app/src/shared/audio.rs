//! The viewer's audio (D16, C3 `AudioSink`): `ViewerAudio` datagrams go to the jitter buffer and Opus decoder of
//! `swapdesk-audio` and out through a sink, on a thread of their own.
//!
//! The reader thread only hands datagrams over (`push`, bounded: a full queue drops the datagram, which the jitter
//! buffer conceals) with the time it arrived, taken on the reader thread so the buffer measures lateness exactly.
//!
//! Who paces the playout. A sink paced by its own device clock (`AudioSink::device_paced`, the Windows render sink:
//! event-driven WASAPI) sets the rhythm: the thread waits for the device to ask, takes the datagrams that arrived,
//! and pulls from the receiver exactly as many 10 ms frames as the device queue has room for. The device clock is
//! then the only clock the playout follows; a drift against the host's capture clock shows up as the jitter buffer's
//! depth and is absorbed there (a repeated frame or a dropped one, both counted), never as a queue that overflows.
//! A sink with no clock of its own (the default) is written on a 10 ms tick: every tick takes one frame from the
//! receiver and writes it; a tick that comes late catches up by at most three frames, then the clock restarts.
//!
//! The sink is created on the thread and started with the first frame (so a viewer that never hears anything never
//! opens the audio device); a sink that fails to start ends the audio, never the session.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_audio::{AudioReceiver, FRAME_US, JitterConfig, RxFrame};
use swapdesk_platform::{AudioBlock, AudioSink, MonoMicros};

/// Datagrams waiting for the audio thread (a second of audio).
const QUEUE: usize = 100;
/// Frames written at most in one wake-up when the thread is late.
const CATCH_UP: u32 = 3;
/// Longest wait for the device between two checks of the stop flag.
const DEVICE_WAIT: Duration = Duration::from_millis(50);

/// What the audio thread counts, published for the report. Nothing is dropped without a count.
#[derive(Default)]
struct Counters {
    /// Receiver underruns (nothing queued at a pull) plus the device's own (queue empty at a wake-up).
    underruns: Arc<AtomicU32>,
    played: AtomicU32,
    /// Frames the device queue refused (an overrun).
    overrun_frames: AtomicU32,
    /// Frames repeated ahead of starvation (the device clock ran faster than the host's).
    stretched: AtomicU32,
    /// Frames dropped to bring the depth down (the device clock ran slower).
    trimmed: AtomicU32,
}

fn to_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

pub struct AudioPlayer {
    tx: Option<SyncSender<(u64, Vec<u8>)>>,
    epoch: Instant,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// The production player on Windows: WASAPI shared mode on the default render endpoint.
    #[cfg(windows)]
    pub fn start(underruns: Arc<AtomicU32>) -> Self {
        Self::start_with(underruns, swapdesk_audio_win::WasapiRenderSink::new)
    }

    /// A player over any sink; `make` runs on the audio thread (COM objects belong to their thread).
    /// `underruns` is where the thread publishes the count of cuts (the report reads it): the receiver's empty pulls
    /// plus the device's empty wake-ups.
    pub fn start_with<S: AudioSink + 'static>(
        underruns: Arc<AtomicU32>,
        make: impl FnOnce() -> S + Send + 'static,
    ) -> Self {
        let (tx, rx) = sync_channel(QUEUE);
        let counters = Arc::new(Counters { underruns, ..Counters::default() });
        let stop = Arc::new(AtomicBool::new(false));
        let epoch = Instant::now();
        let (c, s) = (Arc::clone(&counters), Arc::clone(&stop));
        let join = std::thread::Builder::new()
            .name("swapdesk-audio-play".into())
            .spawn(move || playout(rx, make(), epoch, &c, &s))
            .ok();
        Self { tx: Some(tx), epoch, counters, stop, join }
    }

    /// Hands a datagram over, stamped with its arrival time. Never blocks; a full queue drops it.
    pub fn push(&self, datagram: Vec<u8>) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send((micros_since(self.epoch, Instant::now()), datagram));
        }
    }

    /// Cuts since the start: pulls with nothing to play mid-stream, and device wake-ups that found its queue empty.
    pub fn underruns(&self) -> u32 {
        self.counters.underruns.load(Ordering::Relaxed)
    }

    /// Frames written to the sink.
    pub fn played(&self) -> u32 {
        self.counters.played.load(Ordering::Relaxed)
    }

    /// Frames the device queue refused (an overrun).
    pub fn overrun_frames(&self) -> u32 {
        self.counters.overrun_frames.load(Ordering::Relaxed)
    }

    /// Frames repeated ahead of starvation to absorb a faster device clock.
    pub fn stretched(&self) -> u32 {
        self.counters.stretched.load(Ordering::Relaxed)
    }

    /// Frames dropped to absorb a slower device clock.
    pub fn trimmed(&self) -> u32 {
        self.counters.trimmed.load(Ordering::Relaxed)
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn micros_since(epoch: Instant, t: Instant) -> u64 {
    t.duration_since(epoch).as_micros() as u64
}

/// Writes one frame to the sink and counts it.
fn emit<S: AudioSink>(sink: &mut S, frame: RxFrame, counters: &Counters) {
    if sink.write(&AudioBlock { captured_at: MonoMicros(frame.capture_us), samples: frame.samples }).is_ok() {
        counters.played.fetch_add(1, Ordering::Relaxed);
    }
}

/// Publishes the receiver's and the sink's counts.
fn publish<S: AudioSink>(receiver: &AudioReceiver, sink: &S, counters: &Counters) {
    let (r, d) = (receiver.stats(), sink.counts());
    counters.underruns.store(to_u32(r.underruns + d.underruns), Ordering::Relaxed);
    counters.overrun_frames.store(to_u32(d.overrun_frames), Ordering::Relaxed);
    counters.stretched.store(to_u32(r.stretched), Ordering::Relaxed);
    counters.trimmed.store(to_u32(r.trimmed), Ordering::Relaxed);
}

fn playout<S: AudioSink>(
    rx: Receiver<(u64, Vec<u8>)>,
    mut sink: S,
    epoch: Instant,
    counters: &Counters,
    stop: &AtomicBool,
) {
    let Ok(mut receiver) = AudioReceiver::new(JitterConfig::default()) else { return };
    let tick = Duration::from_micros(FRAME_US);
    let mut next = epoch + tick;
    let mut started = false;
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok((at_us, datagram)) => {
                // A malformed datagram is dropped; it never reaches the buffer.
                let _ = receiver.on_datagram(at_us, &datagram);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let now = Instant::now();
        let mut done = 0;
        while next <= now && done < CATCH_UP {
            if let Ok(Some(frame)) = receiver.pop(micros_since(epoch, now)) {
                if !started {
                    if sink.start().is_err() {
                        return;
                    }
                    started = true;
                    if sink.device_paced() {
                        // From here the device's clock leads: prime its queue and hand over.
                        if sink.prime().is_err() {
                            return;
                        }
                        emit(&mut sink, frame, counters);
                        device_paced_loop(&rx, &mut sink, &mut receiver, epoch, counters, stop);
                        sink.stop();
                        return;
                    }
                }
                emit(&mut sink, frame, counters);
            }
            next += tick;
            done += 1;
        }
        if next <= now {
            next = now + tick;
        }
        publish(&receiver, &sink, counters);
    }
    if started {
        sink.stop();
    }
}

/// The device sets the pace: each wake-up takes the datagrams that arrived and pulls as many frames as the device
/// queue has room for.
fn device_paced_loop<S: AudioSink>(
    rx: &Receiver<(u64, Vec<u8>)>,
    sink: &mut S,
    receiver: &mut AudioReceiver,
    epoch: Instant,
    counters: &Counters,
    stop: &AtomicBool,
) {
    loop {
        let Ok(room) = sink.wait_room(DEVICE_WAIT) else { return };
        loop {
            match rx.try_recv() {
                Ok((at_us, datagram)) => {
                    let _ = receiver.on_datagram(at_us, &datagram);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let now = micros_since(epoch, Instant::now());
        for _ in 0..room {
            if let Ok(Some(frame)) = receiver.pop(now) {
                emit(sink, frame, counters);
            }
        }
        publish(receiver, sink, counters);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use swapdesk_audio::AudioSender;
    use swapdesk_platform::{AudioSource, Result as PlatformResult};

    /// Records what the player writes.
    struct Recorder {
        log: Arc<Mutex<Vec<AudioBlock>>>,
        started: Arc<AtomicBool>,
    }

    impl AudioSink for Recorder {
        fn start(&mut self) -> PlatformResult<()> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn write(&mut self, block: &AudioBlock) -> PlatformResult<()> {
            self.log.lock().unwrap().push(block.clone());
            Ok(())
        }
        fn queued(&self) -> Duration {
            Duration::ZERO
        }
        fn stop(&mut self) {}
    }

    /// A source that hands out `blocks` 10 ms blocks of a loud sine, one per call.
    struct Tone {
        left: u32,
        phase: f32,
    }

    impl AudioSource for Tone {
        fn start(&mut self, _exclude_pid: Option<u32>) -> PlatformResult<()> {
            Ok(())
        }
        fn next_block(&mut self, _timeout: Duration) -> PlatformResult<Option<AudioBlock>> {
            if self.left == 0 {
                return Ok(None);
            }
            self.left -= 1;
            let mut samples = Vec::with_capacity(960);
            for _ in 0..480 {
                let v = (self.phase).sin() * 0.5;
                self.phase += 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
                samples.push(v);
                samples.push(v);
            }
            Ok(Some(AudioBlock { captured_at: MonoMicros(0), samples }))
        }
        fn stop(&mut self) {}
    }

    fn datagrams(count: u32) -> Vec<Vec<u8>> {
        let mut sender = AudioSender::new(Tone { left: count, phase: 0.0 }, None).expect("sender");
        let mut out = Vec::new();
        sender.set_enabled(true).expect("start");
        for _ in 0..count + 4 {
            out.extend(sender.poll(Duration::from_millis(1)).expect("poll"));
        }
        out
    }

    #[test]
    fn datagrams_become_frames_in_the_sink_and_the_device_opens_only_for_the_first() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(AtomicBool::new(false));
        let (l, s) = (Arc::clone(&log), Arc::clone(&started));
        let player = AudioPlayer::start_with(Arc::new(AtomicU32::new(0)), move || Recorder { log: l, started: s });
        std::thread::sleep(Duration::from_millis(60));
        assert!(!started.load(Ordering::SeqCst), "nothing heard, nothing opened");
        let all = datagrams(40);
        assert!(all.len() >= 30, "the sender made {} datagrams", all.len());
        for d in all {
            player.push(d);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(started.load(Ordering::SeqCst));
        let blocks = log.lock().unwrap();
        assert!(blocks.len() >= 25, "only {} frames were played", blocks.len());
        assert!(blocks.iter().all(|b| b.samples.len() == 960));
        let loud = blocks.iter().filter(|b| b.samples.iter().any(|v| v.abs() > 0.1)).count();
        assert!(loud >= 20, "only {loud} frames carried the tone");
        drop(blocks);
        assert_eq!(player.played() as usize, log.lock().unwrap().len());
    }

    /// A sink with a clock of its own: `wait_room` sleeps one period (the device consuming 10 ms) and reports room
    /// for one frame; it keeps the queue of a real device (primed with one silent frame).
    struct Paced {
        log: Arc<Mutex<Vec<AudioBlock>>>,
        primed: Arc<AtomicU32>,
    }

    impl AudioSink for Paced {
        fn start(&mut self) -> PlatformResult<()> {
            Ok(())
        }
        fn write(&mut self, block: &AudioBlock) -> PlatformResult<()> {
            self.log.lock().unwrap().push(block.clone());
            Ok(())
        }
        fn queued(&self) -> Duration {
            Duration::ZERO
        }
        fn stop(&mut self) {}
        fn device_paced(&self) -> bool {
            true
        }
        fn wait_room(&mut self, timeout: Duration) -> PlatformResult<usize> {
            std::thread::sleep(Duration::from_millis(10).min(timeout));
            Ok(1)
        }
        fn prime(&mut self) -> PlatformResult<()> {
            self.primed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn a_device_paced_sink_is_primed_once_and_fed_one_frame_per_wake() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let primed = Arc::new(AtomicU32::new(0));
        let (l, p) = (Arc::clone(&log), Arc::clone(&primed));
        let player = AudioPlayer::start_with(Arc::new(AtomicU32::new(0)), move || Paced { log: l, primed: p });
        for d in datagrams(60) {
            player.push(d);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(primed.load(Ordering::SeqCst), 1, "primed once, at the start");
        let blocks = log.lock().unwrap();
        assert!(blocks.len() >= 40, "only {} frames were played", blocks.len());
        let loud = blocks.iter().filter(|b| b.samples.iter().any(|v| v.abs() > 0.1)).count();
        assert!(loud >= 35, "only {loud} frames carried the tone");
        drop(blocks);
        assert_eq!(player.overrun_frames(), 0);
        drop(player);
    }

    #[test]
    fn garbage_and_a_full_queue_never_block_or_crash() {
        let player = AudioPlayer::start_with(Arc::new(AtomicU32::new(0)), || Recorder {
            log: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(AtomicBool::new(false)),
        });
        let t0 = Instant::now();
        for _ in 0..(QUEUE * 3) {
            player.push(vec![1, 2, 3]);
        }
        assert!(t0.elapsed() < Duration::from_millis(200), "push must not block");
        drop(player);
    }
}
