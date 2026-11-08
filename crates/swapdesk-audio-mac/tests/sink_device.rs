//! `MacAudioSink` against the real default output device of the Mac. The tone test plays 0.5 s at gain 0.05 (about
//! -26 dBFS); every other test plays silence. A Mac with no output device skips with a message instead of failing.
#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use swapdesk_audio_mac::{MacAudioSink, SinkConfig};
use swapdesk_platform::{AudioBlock, AudioSink, ErrorKind, MonoMicros};

fn start_or_skip(sink: &mut MacAudioSink, test: &str) -> bool {
    match sink.start() {
        Ok(()) => true,
        Err(e) if matches!(e.kind, ErrorKind::Unavailable | ErrorKind::Os) => {
            eprintln!("SKIP {test}: the default output audio unit did not start ({e}); no usable output device");
            false
        }
        Err(e) => panic!("{test}: unexpected error {e}"),
    }
}

/// 10 ms of a 440 Hz sine, continuing from `frame0`.
fn tone(frame0: usize) -> AudioBlock {
    let samples = (frame0..frame0 + 480)
        .flat_map(|i| {
            let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin() * 0.5;
            [v, v]
        })
        .collect();
    AudioBlock { captured_at: MonoMicros(0), samples }
}

/// Writes `blocks` 10 ms blocks at wall-clock pace.
fn play(sink: &mut MacAudioSink, blocks: usize) {
    let t0 = Instant::now();
    for n in 0..blocks {
        sink.write(&tone(n * 480)).expect("write");
        let due = t0 + Duration::from_millis(10 * (n as u64 + 1));
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
}

fn drain(sink: &MacAudioSink) -> bool {
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        if sink.queued() < Duration::from_millis(1) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_silent_stream_is_consumed_at_the_device_pace_without_drops() {
    let mut sink = MacAudioSink::new(SinkConfig { volume: 0.0, ..SinkConfig::default() });
    if !start_or_skip(&mut sink, "silent stream") {
        return;
    }
    play(&mut sink, 50);
    let stats = sink.stats();
    assert_eq!(stats.frames_written, 50 * 480);
    assert_eq!(stats.dropped_frames, 0, "a paced writer never overflows the ring: {stats:?}");
    assert!(stats.frames_played > 0, "the device never pulled from the ring: {stats:?}");
    assert!(sink.queued() <= Duration::from_millis(60), "queued {:?}", sink.queued());
    eprintln!(
        "stats {stats:?}, device buffer {:?}, requested {:?}",
        sink.device_buffer_frames(),
        sink.requested_buffer_frames()
    );
    assert!(drain(&sink), "the ring never drained: {:?}", sink.queued());
    sink.stop();
    assert_eq!(sink.queued(), Duration::ZERO);
}

#[test]
fn a_quiet_tone_plays_and_the_volume_can_be_changed_while_it_does() {
    let mut sink = MacAudioSink::new(SinkConfig { volume: 0.05, ..SinkConfig::default() });
    if !start_or_skip(&mut sink, "quiet tone") {
        return;
    }
    play(&mut sink, 25);
    sink.set_volume(0.02);
    assert_eq!(sink.volume(), 0.02);
    play(&mut sink, 25);
    let stats = sink.stats();
    assert!(stats.frames_played >= 40 * 480, "most of the tone should have been taken by the device: {stats:?}");
    assert!(drain(&sink));
    sink.set_volume(0.0);
}

#[test]
fn write_before_start_and_after_stop_is_closed_and_a_restart_works() {
    let mut sink = MacAudioSink::new(SinkConfig { volume: 0.0, ..SinkConfig::default() });
    let err = sink.write(&tone(0)).expect_err("not started");
    assert_eq!(err.kind, ErrorKind::Closed);
    assert_eq!(sink.queued(), Duration::ZERO);
    if !start_or_skip(&mut sink, "restart") {
        return;
    }
    play(&mut sink, 5);
    sink.stop();
    assert_eq!(sink.write(&tone(0)).expect_err("stopped").kind, ErrorKind::Closed);
    assert!(start_or_skip(&mut sink, "restart"), "the second start must work too");
    assert_eq!(sink.stats().frames_written, 0, "a restart begins with fresh counters");
    play(&mut sink, 5);
    assert_eq!(sink.stats().frames_written, 5 * 480);
}

#[test]
fn the_device_change_counter_starts_at_zero_and_the_sink_keeps_running() {
    let mut sink = MacAudioSink::new(SinkConfig { volume: 0.0, ..SinkConfig::default() });
    if !start_or_skip(&mut sink, "device watch") {
        return;
    }
    // The owner's real default device is never changed by a test: only the counter's wiring can be checked here.
    assert_eq!(sink.device_changes(), 0);
    play(&mut sink, 3);
    assert!(sink.is_running());
}
