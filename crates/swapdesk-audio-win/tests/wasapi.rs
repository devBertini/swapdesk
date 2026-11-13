//! WASAPI on this PC: a loopback source and a render sink are opened, one second of audio is read, and only the
//! format and the frame counts are asserted. Nothing that is captured is stored, saved or inspected beyond that the
//! values are finite numbers; the sink plays digital silence.
#![cfg(windows)]

use std::time::{Duration, Instant};

use swapdesk_audio_win::{CaptureMode, WasapiLoopbackSource, WasapiRenderSink, probe_process_loopback, qpc_micros};
use swapdesk_platform::{AudioBlock, AudioSink, AudioSource, ErrorKind, MonoMicros};

struct OneSecond {
    blocks: usize,
    first_ts: u64,
    last_ts: u64,
    max_step_error_us: u64,
    elapsed: Duration,
}

/// Reads 100 blocks (1 s) while the sink plays silence at the same pace; checks every block's shape.
fn read_one_second(src: &mut WasapiLoopbackSource, sink: &mut WasapiRenderSink) -> OneSecond {
    let silence = AudioBlock { captured_at: MonoMicros(0), samples: vec![0.0; 960] };
    let started = Instant::now();
    let (mut blocks, mut first_ts, mut last_ts, mut max_err) = (0usize, 0u64, 0u64, 0u64);
    while blocks < 100 && started.elapsed() < Duration::from_secs(5) {
        sink.write(&silence).unwrap();
        let Some(b) = src.next_block(Duration::from_millis(50)).unwrap() else {
            continue;
        };
        assert_eq!(b.samples.len(), 960, "a block is 10 ms of 48 kHz stereo");
        assert!(b.samples.iter().all(|s| s.is_finite()));
        if blocks == 0 {
            first_ts = b.captured_at.0;
        } else {
            assert!(b.captured_at.0 > last_ts, "timestamps increase");
            max_err = max_err.max(b.captured_at.0.abs_diff(last_ts + 10_000));
        }
        last_ts = b.captured_at.0;
        blocks += 1;
    }
    OneSecond { blocks, first_ts, last_ts, max_step_error_us: max_err, elapsed: started.elapsed() }
}

#[test]
fn loopback_and_render_run_for_one_second() {
    let mut sink = WasapiRenderSink::new();
    let mut src = WasapiLoopbackSource::new();
    sink.start().expect("render endpoint");
    src.start(None).expect("loopback of the default render endpoint");
    assert_eq!(src.mode(), Some(CaptureMode::Loopback));
    let r = read_one_second(&mut src, &mut sink);
    println!(
        "loopback: {} blocks in {:.2} s, timeline {:.3} s, max step error {} us, packets {}, frames {}, filled {}, software conversion {}; render: {} frames written, {} dropped, buffer {} frames, queued {:?}",
        r.blocks,
        r.elapsed.as_secs_f64(),
        (r.last_ts - r.first_ts) as f64 / 1e6,
        r.max_step_error_us,
        src.stats().packets,
        src.stats().frames,
        src.stats().filled_frames,
        src.stats().software_conversion,
        sink.stats().frames,
        sink.stats().dropped_frames,
        sink.buffer_frames().unwrap(),
        sink.queued()
    );
    assert_eq!(r.blocks, 100);
    // 100 blocks of 10 ms span 990 ms between the first and the last timestamp.
    assert!((r.last_ts - r.first_ts).abs_diff(990_000) <= 50_000, "{}", r.last_ts - r.first_ts);
    assert!(r.elapsed < Duration::from_secs(3), "{:?}", r.elapsed);
    assert!(sink.stats().frames >= 48_000 / 2);
    assert!(sink.queued() <= Duration::from_millis(45));
    // The first timestamp is the machine's QPC clock of a moment ago.
    assert!(qpc_micros().abs_diff(r.last_ts) < 2_000_000);
    src.stop();
    sink.stop();
    assert_eq!(src.next_block(Duration::ZERO).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn process_exclusion_availability_is_reported_and_used() {
    let report = probe_process_loopback();
    println!("process-exclusion loopback: {report:?}");
    let pid = std::process::id();
    let mut sink = WasapiRenderSink::new();
    let mut src = WasapiLoopbackSource::new();
    sink.start().unwrap();
    src.start(Some(pid)).unwrap();
    let mode = src.mode().unwrap();
    println!("source mode with exclude_pid: {mode:?}");
    if report.available {
        assert_eq!(mode, CaptureMode::ExcludeProcessTree(pid));
    } else {
        assert_eq!(mode, CaptureMode::Loopback, "falls back to plain loopback");
    }
    let r = read_one_second(&mut src, &mut sink);
    println!(
        "{} blocks in {:.2} s, timeline {:.3} s, packets {}, filled {}",
        r.blocks,
        r.elapsed.as_secs_f64(),
        (r.last_ts - r.first_ts) as f64 / 1e6,
        src.stats().packets,
        src.stats().filled_frames
    );
    assert_eq!(r.blocks, 100);
}

#[test]
fn unstarted_source_and_sink_report_closed_and_a_full_sink_drops_instead_of_queueing() {
    let mut src = WasapiLoopbackSource::new();
    assert_eq!(src.next_block(Duration::ZERO).unwrap_err().kind, ErrorKind::Closed);
    let mut sink = WasapiRenderSink::new();
    let block = AudioBlock { captured_at: MonoMicros(0), samples: vec![0.0; 960] };
    assert_eq!(sink.write(&block).unwrap_err().kind, ErrorKind::Closed);
    assert_eq!(sink.queued(), Duration::ZERO);
    sink.start().unwrap();
    // 1 s of silence at once: the 40 ms queue takes what fits and the rest is dropped.
    for _ in 0..100 {
        sink.write(&block).unwrap();
    }
    let s = sink.stats();
    println!(
        "burst of 1 s: {} frames accepted, {} dropped, buffer {} frames",
        s.frames,
        s.dropped_frames,
        sink.buffer_frames().unwrap()
    );
    assert!(s.dropped_frames > 0);
    assert!(s.frames <= u64::from(sink.buffer_frames().unwrap()) + 960);
    assert_eq!(s.frames + s.dropped_frames, 48_000);
    sink.stop();
}

/// The endpoint paces the writer: for 3 s the thread waits for the device's event and writes exactly the frames it
/// has room for (digital silence). The wake-ups follow the device clock (about 100 a second), nothing is dropped and
/// the queue never runs dry; the thread is registered with MMCSS.
#[test]
fn the_render_endpoint_paces_the_writer_without_overruns_or_underruns() {
    let mut sink = WasapiRenderSink::new();
    sink.start().expect("render endpoint");
    assert!(sink.device_paced());
    sink.prime().unwrap();
    let silence = AudioBlock { captured_at: MonoMicros(0), samples: vec![0.0; 960] };
    // The player writes the first real frame right after the priming one: 20 ms queued.
    sink.write(&silence).unwrap();
    let started = Instant::now();
    let mut written = 1u64;
    while started.elapsed() < Duration::from_secs(3) {
        let room = sink.wait_room(Duration::from_millis(100)).unwrap();
        for _ in 0..room {
            sink.write(&silence).unwrap();
            written += 1;
        }
    }
    let (s, buffer) = (sink.stats().clone(), sink.buffer_frames().unwrap());
    println!(
        "paced 3 s: {} wakes, {} frames written, {} underruns, {} dropped frames, buffer {buffer} frames, mmcss {}",
        s.wakes,
        written,
        s.underruns,
        s.dropped_frames,
        sink.mmcss_registered()
    );
    assert!(s.wakes >= 200, "{} wakes in 3 s", s.wakes);
    assert!(written >= 200, "{written} frames written");
    assert_eq!(s.dropped_frames, 0);
    assert_eq!(s.underruns, 0);
    assert!(sink.mmcss_registered());
    assert_eq!(sink.counts().overrun_frames, 0);
    sink.stop();
}

/// Magnitude of the `freq` component of the left channel of `blocks` (Goertzel), in sample units.
fn tone_level(blocks: &[Vec<f32>], freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / 48_000.0;
    let (mut re, mut im, mut n) = (0f64, 0f64, 0usize);
    for b in blocks {
        for f in b.chunks(2) {
            let phase = w * n as f64;
            re += f64::from(f[0]) * phase.cos();
            im -= f64::from(f[0]) * phase.sin();
            n += 1;
        }
    }
    2.0 * (re * re + im * im).sqrt() / n.max(1) as f64
}

/// Renders a very quiet 1.234 kHz tone (peak 0.005, -46 dBFS) from this process while capturing for about a second,
/// and returns the tone's level in what was captured and, as the floor to judge it against, the level at a frequency
/// nothing played (2.345 kHz). The captured samples are analysed in memory and dropped.
fn capture_level_with_own_tone(exclude: Option<u32>, amp: f32) -> (f64, f64) {
    let mut sink = WasapiRenderSink::new();
    let mut src = WasapiLoopbackSource::new();
    sink.start().unwrap();
    src.start(exclude).unwrap();
    let mut phase = 0usize;
    let mut captured: Vec<Vec<f32>> = vec![];
    let started = Instant::now();
    while captured.len() < 100 && started.elapsed() < Duration::from_secs(5) {
        let samples: Vec<f32> = (0..480)
            .flat_map(|i| {
                let v = amp * (2.0 * std::f64::consts::PI * 1_234.0 * (phase + i) as f64 / 48_000.0).sin() as f32;
                [v, v]
            })
            .collect();
        phase += 480;
        sink.write(&AudioBlock { captured_at: MonoMicros(0), samples }).unwrap();
        if let Some(b) = src.next_block(Duration::from_millis(50)).unwrap() {
            captured.push(b.samples);
        }
    }
    // Skip the first 20 blocks: the render stream needs a moment to reach the mixer.
    (tone_level(&captured[20..], 1_234.0), tone_level(&captured[20..], 2_345.0))
}

#[test]
fn process_exclusion_really_leaves_this_process_out() {
    if !probe_process_loopback().available {
        println!("process loopback unavailable here: exclusion not checked");
        return;
    }
    let (plain, floor) = capture_level_with_own_tone(None, 0.005);
    let (excluded, _) = capture_level_with_own_tone(Some(std::process::id()), 0.005);
    println!(
        "own 1.234 kHz tone in plain loopback {plain:.5} (floor {floor:.5}), with this process excluded {excluded:.5} (rendered peak 0.005)"
    );
    // The level that reaches the loopback depends on the mixer and the moment's load: the tone is judged against the
    // floor of the same capture, not against a fixed number.
    assert!(plain > 10.0 * floor.max(1e-6), "plain loopback should hear the tone: {plain} over a floor of {floor}");
    assert!(excluded < plain * 0.2, "excluded {excluded} vs plain {plain}");
}
