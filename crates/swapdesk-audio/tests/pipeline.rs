//! Stage 17 proofs for `swapdesk-audio`: codec quality, packetization, the jitter buffer under `swapdesk-netsim`
//! impairment in simulated time, the end-to-end B3 delay, the audio-off rule and the encode+decode cost.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use swapdesk_audio::{
    AudioReceiver, AudioSender, FRAME_US, FRAME_VALUES, FrameDecoder, FrameEncoder, FrameKind, HEADER_LEN,
    JitterBuffer, JitterConfig, MAX_DATAGRAM, MAX_OPUS_PACKET, Playout, parse_datagram,
};
use swapdesk_fakes::{FakeClock, RecordingAudioSink, SineSource};
use swapdesk_netsim::{Impairment, Link, Verdict};
use swapdesk_platform::{AudioBlock, AudioSink, AudioSource, MonoMicros, Result as PlatformResult};
use swapdesk_proto::{AudioHeader, Wire};

const T0: u64 = 5_000_000;
const MS: Duration = Duration::from_millis(1);

fn sine_frame(index: usize) -> Vec<f32> {
    (0..FRAME_VALUES / 2)
        .flat_map(|i| {
            let n = index * (FRAME_VALUES / 2) + i;
            let v = 0.25 * (core::f32::consts::TAU * (n % 48) as f32 / 48.0).sin();
            [v, v]
        })
        .collect()
}

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 23) as f32 - 1.0
    }
}

// ---------------------------------------------------------------- codec

#[test]
fn codec_round_trip_of_a_sine_has_a_high_snr() {
    let mut enc = FrameEncoder::new().unwrap();
    let mut dec = FrameDecoder::new().unwrap();
    let frames = 300;
    let mut input = vec![];
    let mut output = vec![];
    for i in 0..frames {
        let f = sine_frame(i);
        let packet = enc.encode(&f).unwrap().to_vec();
        let mut out = vec![0.0f32; FRAME_VALUES];
        assert_eq!(dec.decode(&packet, &mut out).unwrap(), 480);
        input.extend(f.chunks(2).map(|c| c[0]));
        output.extend(out.chunks(2).map(|c| c[0]));
    }
    // Best alignment: the decoder output lags the input by the look-ahead.
    let skip = 2400;
    let (mut best_lag, mut best_snr) = (0, f64::MIN);
    // The sine repeats every 48 samples: search the range around the look-ahead only.
    for lag in 0..150 {
        let (mut sig, mut err) = (0f64, 0f64);
        for n in skip..input.len() - 600 {
            let x = f64::from(input[n]);
            let y = f64::from(output[n + lag]);
            sig += x * x;
            err += (x - y) * (x - y);
        }
        let snr = 10.0 * (sig / err).log10();
        if snr > best_snr {
            (best_lag, best_snr) = (lag, snr);
        }
    }
    println!("sine round trip: SNR {best_snr:.1} dB at lag {best_lag} samples ({:.2} ms)", best_lag as f64 / 48.0);
    assert!(best_snr > 25.0, "SNR {best_snr}");
    assert!(best_lag <= 130, "lag {best_lag}");
    let mut e = FrameEncoder::new().unwrap();
    println!("encoder look-ahead: {} samples", e.lookahead().unwrap());
}

#[test]
fn packets_fit_the_datagram_even_for_full_scale_noise() {
    let mut enc = FrameEncoder::new().unwrap();
    let mut rng = Lcg(7);
    let (mut max, mut total) = (0usize, 0usize);
    for _ in 0..500 {
        let f: Vec<f32> = (0..FRAME_VALUES).map(|_| rng.next()).collect();
        let n = enc.encode(&f).unwrap().len();
        max = max.max(n);
        total += n;
    }
    println!("full-scale noise: max packet {max} B, mean {} B, limit {MAX_OPUS_PACKET} B", total / 500);
    assert!(max <= MAX_OPUS_PACKET);
    assert!(max + HEADER_LEN <= MAX_DATAGRAM);
    // A sine and silence are small.
    let mut enc = FrameEncoder::new().unwrap();
    let sine: usize = (0..200).map(|i| enc.encode(&sine_frame(i)).unwrap().len()).sum::<usize>() / 200;
    let mut silence = 0;
    for _ in 0..100 {
        silence = enc.encode(&vec![0.0; FRAME_VALUES]).unwrap().len();
    }
    println!("sine mean packet {sine} B, silence packet {silence} B");
    assert!(sine < 400);
    assert!(silence <= 4, "steady silence packet of {silence} B");
}

#[test]
fn concealment_fills_a_lost_frame_with_continuing_audio() {
    let mut enc = FrameEncoder::new().unwrap();
    let mut dec = FrameDecoder::new().unwrap();
    let mut out = vec![0.0f32; FRAME_VALUES];
    let mut packets = vec![];
    for i in 0..60 {
        packets.push(enc.encode(&sine_frame(i)).unwrap().to_vec());
    }
    let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
    for (i, p) in packets.iter().enumerate() {
        if i == 40 {
            // Frame 40 is lost; the next packet is available (FEC attempt), then plain PLC for frame 41 as well.
            assert_eq!(dec.conceal(Some(&packets[41]), &mut out).unwrap(), 480);
            let with_next = rms(&out);
            assert!(with_next > 0.05, "concealed frame rms {with_next}");
            continue;
        }
        if i == 50 {
            assert_eq!(dec.conceal(None, &mut out).unwrap(), 480);
            let plc = rms(&out);
            assert!(plc > 0.05, "PLC rms {plc}");
            continue;
        }
        dec.decode(p, &mut out).unwrap();
    }
}

// ---------------------------------------------------------------- jitter buffer under netsim, simulated time

struct JitterRun {
    played: u64,
    lost_frames: u64,
    dropped_by_link: u64,
    underruns: u64,
    late: u64,
    resyncs: u64,
    trimmed: u64,
    target_min: u32,
    target_max: u32,
}

/// 10 simulated minutes of 100 packets per second through a `netsim::Link`; the playout tick runs at a 3 ms phase.
fn simulate_jitter(seed: u64, imp: Impairment, seconds: u64) -> JitterRun {
    let mut link = Link::new(imp, seed);
    let mut buf = JitterBuffer::new(JitterConfig::default());
    let mut heap: BinaryHeap<Reverse<(Duration, u32)>> = BinaryHeap::new();
    let (mut played, mut lost_frames, mut dropped) = (0u64, 0u64, 0u64);
    let (mut tmin, mut tmax) = (u32::MAX, 0u32);
    for k in 0..seconds * 100 {
        let t = k * FRAME_US;
        match link.decide(Duration::from_micros(t), 170) {
            Verdict::Deliver { at } => heap.push(Reverse((at, k as u32))),
            Verdict::Drop(_) => dropped += 1,
        }
        let tick = t + 3_000;
        while let Some(Reverse((at, seq))) = heap.peek().copied() {
            if at > Duration::from_micros(tick) {
                break;
            }
            heap.pop();
            let header = AudioHeader {
                discontinuity: false,
                muted: false,
                frame_ms: 10,
                seq,
                capture_us: u64::from(seq) * FRAME_US,
            };
            buf.push(at.as_micros() as u64, header, vec![1; 160]);
        }
        match buf.pop(tick) {
            Playout::Packet(_) => played += 1,
            Playout::Lost { .. } => lost_frames += 1,
            Playout::Starved | Playout::Waiting => {}
        }
        if k > 500 {
            tmin = tmin.min(buf.target_ms());
            tmax = tmax.max(buf.target_ms());
        }
    }
    let s = buf.stats();
    JitterRun {
        played,
        lost_frames,
        dropped_by_link: dropped,
        underruns: s.underruns,
        late: s.late,
        resyncs: s.resyncs,
        trimmed: s.trimmed,
        target_min: tmin,
        target_max: tmax,
    }
}

#[test]
fn jitter_buffer_survives_ten_simulated_minutes_of_loss_and_jitter() {
    let handles: Vec<_> = (1..=4u64)
        .map(|seed| {
            std::thread::spawn(move || {
                let imp = Impairment { loss: 0.01, delay: 2 * MS, jitter: 20 * MS, ..Impairment::default() };
                (seed, simulate_jitter(seed, imp, 600))
            })
        })
        .collect();
    for h in handles {
        let (seed, r) = h.join().unwrap();
        println!(
            "seed {seed}: played {} lost-concealed {} link-dropped {} underruns {} late {} resyncs {} trimmed {} target {}..{} ms",
            r.played,
            r.lost_frames,
            r.dropped_by_link,
            r.underruns,
            r.late,
            r.resyncs,
            r.trimmed,
            r.target_min,
            r.target_max
        );
        assert_eq!(r.underruns, 0, "seed {seed}");
        assert_eq!(r.resyncs, 0, "seed {seed}");
        assert_eq!(r.late, 0, "seed {seed}");
        // Every packet the link dropped was concealed exactly once: PLC fills each gap.
        assert!(
            r.lost_frames.abs_diff(r.dropped_by_link) <= 4,
            "seed {seed}: {} vs {}",
            r.lost_frames,
            r.dropped_by_link
        );
        assert!(r.played + r.lost_frames >= 59_900);
        assert!(r.target_min >= 20 && r.target_max <= 40);
    }
}

#[test]
fn jitter_buffer_recovers_from_a_link_outage() {
    let imp = Impairment { delay: 2 * MS, jitter: 5 * MS, ..Impairment::default() };
    let mut link = Link::new(imp, 9);
    let mut buf = JitterBuffer::new(JitterConfig::default());
    let mut heap: BinaryHeap<Reverse<(Duration, u32)>> = BinaryHeap::new();
    let mut frames_after = 0u64;
    for k in 0..3_000u64 {
        let t = k * FRAME_US;
        link.set_up(!(1_000..1_100).contains(&k)); // one second without a packet
        if let Verdict::Deliver { at } = link.decide(Duration::from_micros(t), 170) {
            heap.push(Reverse((at, k as u32)));
        }
        let tick = t + 3_000;
        while let Some(Reverse((at, seq))) = heap.peek().copied() {
            if at > Duration::from_micros(tick) {
                break;
            }
            heap.pop();
            let h = AudioHeader { discontinuity: false, muted: false, frame_ms: 10, seq, capture_us: 0 };
            buf.push(at.as_micros() as u64, h, vec![1; 160]);
        }
        if matches!(buf.pop(tick), Playout::Packet(_)) && k > 1_200 {
            frames_after += 1;
        }
    }
    assert!(buf.stats().resyncs >= 1);
    assert!(frames_after > 1_700, "{frames_after}");
}

// ---------------------------------------------------------------- end to end

struct E2e {
    delays_ms: Vec<f64>,
    sink_underruns: usize,
    jitter_underruns: u64,
    concealed: u64,
    dropped: u64,
    onsets: usize,
    markers: usize,
    sent: u64,
    bytes: u64,
    target: u32,
}

/// SineSource -> AudioSender (Opus) -> netsim link -> AudioReceiver (jitter buffer, Opus PLC) -> RecordingAudioSink,
/// all on one fake clock. A block is complete 10 ms after its first sample was captured, so the sender sends then.
fn run_e2e(seconds: u64, imp: Impairment, seed: u64, phase_us: u64) -> E2e {
    let clock = FakeClock::new(MonoMicros(T0));
    let source = SineSource::new(Arc::new(clock.clone()));
    let log = source.log_handle();
    let mut sender = AudioSender::new(source, None).unwrap();
    let mut rx = AudioReceiver::new(JitterConfig::default()).unwrap();
    let mut sink = RecordingAudioSink::new(Arc::new(clock.clone()));
    sink.start().unwrap();
    sender.set_enabled(true).unwrap();
    let mut link = Link::new(imp, seed);
    let mut heap: BinaryHeap<Reverse<(Duration, Vec<u8>)>> = BinaryHeap::new();
    let (mut concealed, mut dropped) = (0u64, 0u64);
    for k in 0..seconds * 100 {
        let t = k * FRAME_US;
        clock.set(MonoMicros(T0 + t + FRAME_US));
        if let Some(d) = sender.poll(Duration::ZERO).unwrap() {
            let sent_at = Duration::from_micros(t + FRAME_US);
            match link.decide(sent_at, d.len()) {
                Verdict::Deliver { at } => heap.push(Reverse((at, d))),
                Verdict::Drop(_) => dropped += 1,
            }
        }
        let tick = t + FRAME_US + phase_us;
        while let Some(Reverse((at, _))) = heap.peek() {
            if *at > Duration::from_micros(tick) {
                break;
            }
            let Reverse((at, d)) = heap.pop().unwrap();
            rx.on_datagram(T0 + at.as_micros() as u64, &d).unwrap();
        }
        clock.set(MonoMicros(T0 + tick));
        if let Some(f) = rx.pop(T0 + tick).unwrap() {
            if f.kind == FrameKind::Concealed {
                concealed += 1;
            }
            sink.write(&AudioBlock { captured_at: MonoMicros(f.capture_us), samples: f.samples }).unwrap();
        }
    }
    let markers = log.lock().unwrap().clone();
    let onsets = sink.onsets();
    let mut delays_ms = vec![];
    for o in &onsets {
        if let Some(m) = markers.iter().rev().find(|m| m.captured_at.0 <= o.played_at.0) {
            delays_ms.push((o.played_at.0 - m.captured_at.0) as f64 / 1000.0);
        }
    }
    delays_ms.sort_by(f64::total_cmp);
    E2e {
        delays_ms,
        sink_underruns: sink.underruns().len(),
        jitter_underruns: rx.stats().underruns,
        concealed,
        dropped,
        onsets: onsets.len(),
        markers: markers.len(),
        sent: sender.stats().datagrams,
        bytes: sender.stats().bytes,
        target: rx.target_ms(),
    }
}

/// `run_e2e` with the viewer's playout driven by a device queue (W1.2): the device wakes every 10 ms (offset by
/// `phase_us`), consumes 480 frames, and the playout writes one frame at a time while the 20 ms mark has room, the
/// first one after a frame of priming silence. A frame is heard after the frames queued ahead of it, so the sink is
/// stamped with that time: the delay measured is the full delay including the device queue.
fn run_e2e_device(seconds: u64, imp: Impairment, seed: u64, phase_us: u64) -> E2e {
    let clock = FakeClock::new(MonoMicros(T0));
    let source = SineSource::new(Arc::new(clock.clone()));
    let log = source.log_handle();
    let mut sender = AudioSender::new(source, None).unwrap();
    let mut rx = AudioReceiver::new(JitterConfig::default()).unwrap();
    let mut sink = RecordingAudioSink::new(Arc::new(clock.clone()));
    sink.start().unwrap();
    sender.set_enabled(true).unwrap();
    let mut link = Link::new(imp, seed);
    let mut heap: BinaryHeap<Reverse<(Duration, Vec<u8>)>> = BinaryHeap::new();
    let (mut concealed, mut dropped, mut device_underruns) = (0u64, 0u64, 0usize);
    let (mut started, mut queued) = (false, 0u32);
    for k in 0..seconds * 100 {
        let t = k * FRAME_US;
        clock.set(MonoMicros(T0 + t + FRAME_US));
        if let Some(d) = sender.poll(Duration::ZERO).unwrap() {
            let sent_at = Duration::from_micros(t + FRAME_US);
            match link.decide(sent_at, d.len()) {
                Verdict::Deliver { at } => heap.push(Reverse((at, d))),
                Verdict::Drop(_) => dropped += 1,
            }
        }
        let tick = t + FRAME_US + phase_us;
        while let Some(Reverse((at, _))) = heap.peek() {
            if *at > Duration::from_micros(tick) {
                break;
            }
            let Reverse((at, d)) = heap.pop().unwrap();
            rx.on_datagram(T0 + at.as_micros() as u64, &d).unwrap();
        }
        if started {
            if queued < FRAME_FRAMES {
                device_underruns += 1;
                queued = 0;
            } else {
                queued -= FRAME_FRAMES;
            }
        }
        while queued + FRAME_FRAMES <= DEVICE_MARK_FRAMES {
            let Some(f) = rx.pop(T0 + tick).unwrap() else { break };
            if !started {
                started = true;
                queued += FRAME_FRAMES;
            }
            if f.kind == FrameKind::Concealed {
                concealed += 1;
            }
            let heard_at = tick + u64::from(queued) * 1_000_000 / 48_000;
            clock.set(MonoMicros(T0 + heard_at));
            sink.write(&AudioBlock { captured_at: MonoMicros(f.capture_us), samples: f.samples }).unwrap();
            queued += FRAME_FRAMES;
        }
    }
    let markers = log.lock().unwrap().clone();
    let onsets = sink.onsets();
    let mut delays_ms = vec![];
    for o in &onsets {
        if let Some(m) = markers.iter().rev().find(|m| m.captured_at.0 <= o.played_at.0) {
            delays_ms.push((o.played_at.0 - m.captured_at.0) as f64 / 1000.0);
        }
    }
    delays_ms.sort_by(f64::total_cmp);
    E2e {
        delays_ms,
        sink_underruns: device_underruns,
        jitter_underruns: rx.stats().underruns,
        concealed,
        dropped,
        onsets: onsets.len(),
        markers: markers.len(),
        sent: sender.stats().datagrams,
        bytes: sender.stats().bytes,
        target: rx.target_ms(),
    }
}

fn median(v: &[f64]) -> f64 {
    v[v.len() / 2]
}

#[test]
fn end_to_end_clean_link_marker_delay_is_within_b3() {
    let r = run_e2e(60, Impairment { delay: 2 * MS, ..Impairment::default() }, 1, 3_000);
    println!(
        "clean: {} markers, {} onsets, delay median {:.1} ms min {:.1} max {:.1}, target {} ms, underruns sink {} jitter {}",
        r.markers,
        r.onsets,
        median(&r.delays_ms),
        r.delays_ms[0],
        r.delays_ms.last().unwrap(),
        r.target,
        r.sink_underruns,
        r.jitter_underruns
    );
    assert!(r.onsets >= r.markers - 1);
    assert!(median(&r.delays_ms) <= 120.0);
    assert_eq!(r.sink_underruns, 0);
    assert_eq!(r.jitter_underruns, 0);
    assert_eq!(r.concealed, 0);
}

/// B3 as the project measures it (sine source with a marker every 1 s, onset found in the recording sink), 60 s of
/// simulated time, with the device queue of the event-driven sink counted in the delay.
#[test]
fn end_to_end_device_paced_marker_delay_is_within_b3() {
    for (name, imp, seed) in [
        ("clean", Impairment { delay: 2 * MS, ..Impairment::default() }, 1),
        (
            "1% loss + 20 ms jitter",
            Impairment { loss: 0.01, delay: 2 * MS, jitter: 20 * MS, ..Impairment::default() },
            11,
        ),
    ] {
        let r = run_e2e_device(60, imp, seed, 3_000);
        println!(
            "B3 device-paced, {name}: {} markers, {} onsets, delay median {:.1} ms min {:.1} max {:.1}, target {} ms, device underruns {}, jitter underruns {}",
            r.markers,
            r.onsets,
            median(&r.delays_ms),
            r.delays_ms[0],
            r.delays_ms.last().unwrap(),
            r.target,
            r.sink_underruns,
            r.jitter_underruns
        );
        assert!(r.onsets + 2 >= r.markers, "{name}");
        assert!(median(&r.delays_ms) <= 120.0, "{name}");
        assert_eq!(r.sink_underruns, 0, "{name}");
        assert_eq!(r.jitter_underruns, 0, "{name}");
    }
}

#[test]
fn end_to_end_ten_minutes_with_one_percent_loss_and_20_ms_jitter() {
    let imp = Impairment { loss: 0.01, delay: 2 * MS, jitter: 20 * MS, ..Impairment::default() };
    let r = run_e2e(600, imp, 11, 3_000);
    let p95 = r.delays_ms[r.delays_ms.len() * 95 / 100];
    println!(
        "impaired 10 min: {} markers, {} onsets, delay median {:.1} ms p95 {:.1} max {:.1}, target {} ms, sink underruns {}, jitter underruns {}, link dropped {}, concealed {}, {} datagrams {:.1} kbit/s",
        r.markers,
        r.onsets,
        median(&r.delays_ms),
        p95,
        r.delays_ms.last().unwrap(),
        r.target,
        r.sink_underruns,
        r.jitter_underruns,
        r.dropped,
        r.concealed,
        r.sent,
        r.bytes as f64 * 8.0 / 600.0 / 1000.0
    );
    assert!(median(&r.delays_ms) <= 120.0, "median {}", median(&r.delays_ms));
    assert_eq!(r.sink_underruns, 0);
    assert_eq!(r.jitter_underruns, 0);
    assert!(r.concealed.abs_diff(r.dropped) <= 4, "{} vs {}", r.concealed, r.dropped);
    // A marker whose frame was lost is not found; the rest are.
    assert!(r.onsets as u64 + 20 >= r.markers as u64, "{} onsets of {}", r.onsets, r.markers);
}

// ---------------------------------------------------------------- audio off, mute, framing

/// Counts what the sender does to its source.
struct Counting<S> {
    inner: S,
    starts: Vec<Option<u32>>,
    stops: usize,
    blocks: usize,
}

impl<S: AudioSource> AudioSource for Counting<S> {
    fn start(&mut self, exclude_pid: Option<u32>) -> PlatformResult<()> {
        self.starts.push(exclude_pid);
        self.inner.start(exclude_pid)
    }
    fn next_block(&mut self, timeout: Duration) -> PlatformResult<Option<AudioBlock>> {
        self.blocks += 1;
        self.inner.next_block(timeout)
    }
    fn stop(&mut self) {
        self.stops += 1;
        self.inner.stop();
    }
}

fn counting_sine() -> Counting<SineSource> {
    let clock = FakeClock::new(MonoMicros(T0));
    Counting { inner: SineSource::new(Arc::new(clock)), starts: vec![], stops: 0, blocks: 0 }
}

#[test]
fn audio_off_captures_and_sends_nothing() {
    let mut sender = AudioSender::new(counting_sine(), Some(4242)).unwrap();
    assert!(!sender.is_enabled());
    for _ in 0..1_000 {
        assert_eq!(sender.poll(Duration::ZERO).unwrap(), None);
    }
    assert!(sender.source().starts.is_empty(), "the source was started");
    assert_eq!(sender.source().blocks, 0, "the source was read");
    assert_eq!(sender.stats().datagrams, 0);
    // Turning it off again changes nothing.
    sender.set_enabled(false).unwrap();
    assert_eq!(sender.source().stops, 0);

    // On: the source starts with the excluded PID and datagrams flow.
    sender.set_enabled(true).unwrap();
    assert_eq!(sender.source().starts, vec![Some(4242)]);
    let first = sender.poll(Duration::ZERO).unwrap().expect("a datagram");
    let (h, payload) = parse_datagram(&first).unwrap();
    assert!(h.discontinuity, "the first packet after enabling is flagged");
    assert_eq!((h.seq, h.frame_ms, h.muted), (0, 10, false));
    assert!(!payload.is_empty());

    // Off again: the source stops, nothing more is read or sent.
    let blocks = sender.source().blocks;
    let sent = sender.stats().datagrams;
    sender.set_enabled(false).unwrap();
    assert_eq!(sender.source().stops, 1);
    for _ in 0..1_000 {
        assert_eq!(sender.poll(Duration::ZERO).unwrap(), None);
    }
    assert_eq!(sender.source().blocks, blocks);
    assert_eq!(sender.stats().datagrams, sent);
}

#[test]
fn mute_sends_header_only_frames_and_the_viewer_plays_silence_and_knows() {
    let mut sender = AudioSender::new(counting_sine(), None).unwrap();
    let mut rx = AudioReceiver::new(JitterConfig::default()).unwrap();
    sender.set_enabled(true).unwrap();
    let mut kinds = vec![];
    let mut now = 1_000_000u64;
    for k in 0..150 {
        sender.set_muted((50..100).contains(&k)).unwrap();
        let d = sender.poll(Duration::ZERO).unwrap().unwrap();
        if (50..100).contains(&k) {
            assert_eq!(d.len(), HEADER_LEN, "a muted frame is header only");
        } else {
            assert!(d.len() > HEADER_LEN);
        }
        rx.on_datagram(now, &d).unwrap();
        if let Some(f) = rx.pop(now).unwrap() {
            if f.kind == FrameKind::Muted {
                assert!(f.samples.iter().all(|s| *s == 0.0));
                assert!(rx.is_muted());
            }
            kinds.push((f.kind, f.samples.iter().fold(0f32, |m, s| m.max(s.abs()))));
        }
        now += FRAME_US;
    }
    assert_eq!(sender.stats().muted_datagrams, 50);
    let muted = kinds.iter().filter(|(k, _)| *k == FrameKind::Muted).count();
    assert!((45..=50).contains(&muted), "{muted}");
    assert!(!rx.is_muted(), "unmuted again");
    // After the unmute the audio is back with its level.
    let (last_kind, peak) = *kinds.last().unwrap();
    assert_eq!(last_kind, FrameKind::Decoded);
    assert!(peak > 0.1, "{peak}");
}

struct Chunked {
    sizes: Vec<usize>,
    i: usize,
    at: u64,
    gap_before: Option<usize>,
}

impl AudioSource for Chunked {
    fn start(&mut self, _: Option<u32>) -> PlatformResult<()> {
        Ok(())
    }
    fn next_block(&mut self, _: Duration) -> PlatformResult<Option<AudioBlock>> {
        let frames = self.sizes[self.i % self.sizes.len()];
        if self.gap_before == Some(self.i) {
            self.at += 500_000;
        }
        let b = AudioBlock { captured_at: MonoMicros(self.at), samples: vec![0.1; frames * 2] };
        self.at += frames as u64 * 1_000_000 / 48_000;
        self.i += 1;
        Ok(Some(b))
    }
    fn stop(&mut self) {}
}

#[test]
fn blocks_of_any_size_are_cut_into_10_ms_frames_with_exact_timestamps() {
    let src = Chunked { sizes: vec![441, 480, 1024, 100, 960], i: 0, at: 7_000_000, gap_before: Some(40) };
    let mut sender = AudioSender::new(src, None).unwrap();
    sender.set_enabled(true).unwrap();
    let mut heads: Vec<AudioHeader> = vec![];
    for _ in 0..400 {
        if let Some(d) = sender.poll(Duration::ZERO).unwrap() {
            heads.push(parse_datagram(&d).unwrap().0);
        }
    }
    assert!(heads.len() > 60, "{}", heads.len());
    for w in heads.windows(2) {
        assert_eq!(w[1].seq, w[0].seq + 1);
        if !w[1].discontinuity {
            // The source's own timestamps carry their rounding to a microsecond per block.
            assert!(
                w[1].capture_us.abs_diff(w[0].capture_us + FRAME_US) < 100,
                "{} {}",
                w[1].capture_us,
                w[0].capture_us
            );
        } else {
            assert!(w[1].capture_us > w[0].capture_us + 400_000);
        }
    }
    assert_eq!(sender.stats().discontinuities, 2, "the enabling and the 500 ms gap");
}

#[test]
fn header_round_trips_with_the_proto_crate() {
    let mut sender = AudioSender::new(counting_sine(), None).unwrap();
    sender.set_enabled(true).unwrap();
    let d = sender.poll(Duration::ZERO).unwrap().unwrap();
    let h = AudioHeader::decode(&d[..HEADER_LEN]).unwrap();
    assert_eq!(d[0], 0x01);
    assert_eq!(h.capture_us, T0);
    assert_eq!(h.encode().unwrap(), d[..HEADER_LEN]);
}

// ---------------------------------------------------------------- cost

#[test]
fn cpu_cost_of_encode_plus_decode_per_second_of_audio() {
    let seconds = 20usize;
    let mut enc = FrameEncoder::new().unwrap();
    let mut dec = FrameDecoder::new().unwrap();
    let mut rng = Lcg(3);
    let sine: Vec<Vec<f32>> = (0..100).map(sine_frame).collect();
    let noise: Vec<Vec<f32>> = (0..100).map(|_| (0..FRAME_VALUES).map(|_| rng.next() * 0.1).collect()).collect();
    let mut out = vec![0.0f32; FRAME_VALUES];
    for (name, frames) in [("sine", &sine), ("noise -20 dBFS", &noise)] {
        let (mut t_enc, mut t_dec) = (Duration::ZERO, Duration::ZERO);
        for i in 0..seconds * 100 {
            let f = &frames[i % 100];
            let a = Instant::now();
            let p = enc.encode(f).unwrap().to_vec();
            let b = Instant::now();
            dec.decode(&p, &mut out).unwrap();
            t_enc += b - a;
            t_dec += b.elapsed();
        }
        let per_s = |d: Duration| d.as_secs_f64() * 1000.0 / seconds as f64;
        println!(
            "{name}: encode {:.2} ms/s + decode {:.2} ms/s = {:.2} ms CPU per second of audio ({:.2} % of one core)",
            per_s(t_enc),
            per_s(t_dec),
            per_s(t_enc) + per_s(t_dec),
            (per_s(t_enc) + per_s(t_dec)) / 10.0
        );
        assert!(per_s(t_enc) + per_s(t_dec) < 100.0);
    }
}

// ---------------------------------------------------------------- device-driven playout over a drifting clock (W1.2, C4)

/// Frames of the viewer's device queue the playout keeps topped up: it writes one 10 ms frame whenever the queue
/// holds room for it below this mark (20 ms, the same mark the Windows sink uses).
const DEVICE_MARK_FRAMES: u32 = 960;
const FRAME_FRAMES: u32 = 480;

struct DeviceRun {
    sink_underruns: u64,
    jitter_underruns_after_warmup: u64,
    played: u64,
    concealed: u64,
    link_dropped: u64,
    max_depth: usize,
    stretched: u64,
    trimmed: u64,
    resyncs: u64,
}

/// `seconds` of simulated time. The host sends one frame every `src_interval_us` of the viewer's time (its capture
/// clock runs fast or slow by that ratio); the viewer's device asks for data every 10 ms exactly (its event fires
/// once per period, consumption is 480 frames per wake) and the playout writes while the 20 ms mark has room.
/// The device queue starts with one frame of silence (priming) so the jitter cushion is not spent filling it.
fn simulate_device(seed: u64, imp: Impairment, src_interval_us: f64, seconds: u64) -> DeviceRun {
    let mut link = Link::new(imp, seed);
    let mut buf = JitterBuffer::new(JitterConfig::default());
    let mut heap: BinaryHeap<Reverse<(Duration, u32)>> = BinaryHeap::new();
    let warmup_us = 30_000_000u64;
    let (mut sent, mut dropped) = (0u64, 0u64);
    let (mut sink_underruns, mut played, mut concealed) = (0u64, 0u64, 0u64);
    let (mut max_depth, mut underruns_at_warmup) = (0usize, None);
    let mut started = false;
    let mut queued: u32 = 0;
    for w in 0..seconds * 100 {
        let now = w * FRAME_US + 3_000;
        while (sent as f64) * src_interval_us <= now as f64 {
            let t = (sent as f64 * src_interval_us) as u64;
            match link.decide(Duration::from_micros(t), 170) {
                Verdict::Deliver { at } => heap.push(Reverse((at, sent as u32))),
                Verdict::Drop(_) => dropped += 1,
            }
            sent += 1;
        }
        while let Some(Reverse((at, seq))) = heap.peek().copied() {
            if at > Duration::from_micros(now) {
                break;
            }
            heap.pop();
            let header = AudioHeader {
                discontinuity: false,
                muted: false,
                frame_ms: 10,
                seq,
                capture_us: (f64::from(seq) * src_interval_us) as u64,
            };
            buf.push(at.as_micros() as u64, header, vec![1; 160]);
        }
        if started {
            if queued < FRAME_FRAMES {
                sink_underruns += 1;
                queued = 0;
            } else {
                queued -= FRAME_FRAMES;
            }
        }
        while queued + FRAME_FRAMES <= DEVICE_MARK_FRAMES {
            let out = buf.pop(now);
            if !started {
                match out {
                    Playout::Waiting | Playout::Starved => break,
                    _ => {
                        started = true;
                        queued += FRAME_FRAMES; // priming silence
                    }
                }
            }
            match out {
                Playout::Packet(_) => played += 1,
                Playout::Lost { .. } | Playout::Starved => concealed += 1,
                Playout::Waiting => {}
            }
            queued += FRAME_FRAMES;
        }
        if now > warmup_us {
            underruns_at_warmup.get_or_insert(buf.stats().underruns);
            max_depth = max_depth.max(buf.depth_frames());
        }
    }
    let s = buf.stats();
    DeviceRun {
        sink_underruns,
        jitter_underruns_after_warmup: s.underruns - underruns_at_warmup.unwrap_or(0),
        played,
        concealed,
        link_dropped: dropped,
        max_depth,
        stretched: s.stretched,
        trimmed: s.trimmed,
        resyncs: s.resyncs,
    }
}

#[test]
fn device_driven_playout_absorbs_a_tenth_of_a_percent_of_clock_drift_over_ten_minutes() {
    let cases: Vec<(&str, f64)> = vec![
        ("source 0.1% faster", 10_000.0 / 1.001),
        ("source 0.1% slower", 10_000.0 / 0.999),
        ("no drift", 10_000.0),
    ];
    let handles: Vec<_> = cases
        .into_iter()
        .enumerate()
        .map(|(i, (name, interval))| {
            std::thread::spawn(move || {
                let imp = Impairment { loss: 0.01, delay: 2 * MS, jitter: 20 * MS, ..Impairment::default() };
                (name, simulate_device(21 + i as u64, imp, interval, 600))
            })
        })
        .collect();
    for h in handles {
        let (name, r) = h.join().unwrap();
        println!(
            "{name}: played {} concealed {} link-dropped {} sink underruns {} jitter underruns after warm-up {} max depth {} frames, stretched {} trimmed {} resyncs {}",
            r.played,
            r.concealed,
            r.link_dropped,
            r.sink_underruns,
            r.jitter_underruns_after_warmup,
            r.max_depth,
            r.stretched,
            r.trimmed,
            r.resyncs
        );
        assert_eq!(r.sink_underruns, 0, "{name}");
        assert_eq!(r.jitter_underruns_after_warmup, 0, "{name}");
        assert_eq!(r.resyncs, 0, "{name}");
        assert!(r.max_depth <= 8, "{name}: depth {}", r.max_depth);
        // 0.1% of 600 s is 60 frames: the drift is absorbed by counted repeats or drops, never silently.
        match name {
            "source 0.1% faster" => assert!(r.trimmed >= 50, "{name}: trimmed {}", r.trimmed),
            "source 0.1% slower" => assert!(r.stretched >= 50, "{name}: stretched {}", r.stretched),
            _ => assert!(r.stretched + r.trimmed <= 10, "{name}"),
        }
    }
}
