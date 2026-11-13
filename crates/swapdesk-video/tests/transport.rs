//! Flow proofs of the video transport (PROTOCOL.md 4 and 7.1; F-VIEW-04; B2 and B1, transport part): frames over
//! real QUIC through `swapdesk-trust`, through `ImpairedSocket` where the case needs loss or a dead link.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use swapdesk_fakes::MonotonicClock;
use swapdesk_netsim::Impairment;
use swapdesk_platform::Clock;
use swapdesk_proto::{
    Codec, ErrorCode, KeyframeRequest, ReceiverReport, StreamKind, VIDEO_HEADER_LEN, VideoHeader, Wire,
};
use swapdesk_video::{
    FeedbackSink, ReceivedFrame, ReceiverConfig, STOP_BUDGET, STOP_STALE, SendOutcome, SenderConfig, VideoReceiver,
};

/// Loss only: `ImpairedSocket` delays go through the OS timer, whose 15 ms granularity on Windows turns a 1 ms
/// delay into a 20 ms RTT and a 6 MB/s stream into 0.4 MB/s at 1% loss (measured in stage 14), so these cases
/// impair with loss alone.
fn lossy(loss: f64) -> Impairment {
    Impairment { loss, ..Impairment::default() }
}

async fn next_frame(rig: &mut Rig, what: &str) -> ReceivedFrame {
    within(Duration::from_secs(5), what, rig.frames.recv()).await.expect("the receiver ended")
}

fn header(id: u32, key: bool, len: usize) -> VideoHeader {
    VideoHeader {
        keyframe: key,
        config: key,
        discontinuity: false,
        display_id: DISPLAY,
        frame_id: id,
        capture_us: 7,
        encode_us: 1,
        width: 1920,
        height: 1080,
        codec: Codec::H264,
        payload_len: len as u32,
    }
}

/// A frame stream written by hand: the kind, the header and `body` bytes; `finish` ends it.
async fn raw_stream(conn: &quinn::Connection, head: &[u8], body: &[u8], finish: bool) -> quinn::SendStream {
    let mut s = conn.open_uni().await.unwrap();
    let mut wire = StreamKind::Video.encode().unwrap();
    wire.extend_from_slice(head);
    wire.extend_from_slice(body);
    s.write_all(&wire).await.unwrap();
    if finish {
        s.finish().unwrap();
    }
    s
}

/// Waits until every frame stream the sender opened was acknowledged (the ack of the last one is delayed).
async fn wait_idle(sender: &swapdesk_video::VideoSender) {
    let t0 = Instant::now();
    while sender.in_flight() > 0 {
        assert!(t0.elapsed() < Duration::from_secs(3), "{} frame streams never completed", sender.in_flight());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The encoder's frames until it emits a keyframe (it does when the sender wants one); the P-frames are sent.
async fn until_keyframe(enc: &mut TestEncoder, sender: &swapdesk_video::VideoSender) -> swapdesk_video::Frame {
    for _ in 0..400 {
        let f = enc.next(sender);
        if f.header.keyframe {
            return f;
        }
        sender.send(f).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the encoder never emitted a keyframe");
}

// ---------- the basic contract ----------

#[test]
fn frames_arrive_in_order_with_their_headers_and_payloads_intact() {
    run(async {
        let pair = plain_pair().await;
        let mut rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        let mut sent = Vec::new();
        for _ in 0..40 {
            let f = enc.next(&rig.sender);
            sent.push((f.header, f.payload.len()));
            assert_eq!(rig.sender.send(f).await, SendOutcome::Sent);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut decoder = DecoderModel::default();
        for (h, len) in &sent {
            let got = next_frame(&mut rig, "a frame").await;
            assert_eq!(got.header.frame_id, h.frame_id);
            assert_eq!(
                (got.header.keyframe, got.header.config, got.header.display_id),
                (h.keyframe, h.config, h.display_id)
            );
            assert_eq!(
                (got.header.capture_us, got.header.width, got.header.height, got.header.codec),
                (h.capture_us, h.width, h.height, h.codec)
            );
            assert_eq!(got.header.payload_len as usize, *len, "payload_len is set from the payload");
            assert_eq!(got.payload.len(), *len);
            assert!(payload_ok(h.frame_id, &got.payload), "payload of frame {} was damaged", h.frame_id);
            assert!(decoder.feed(&got.header));
        }
        assert_eq!(decoder.errors, 0);
        let s = rig.sender.stats();
        assert_eq!((s.sent, s.keyframes_sent), (40, 1));
        let r = rig.receiver.stats();
        assert_eq!((r.sequence.delivered, r.sequence.lost, r.sequence.dropped), (40, 0, 0));
    });
}

#[test]
fn a_payload_over_8_mib_is_refused_before_a_stream_opens() {
    run(async {
        let pair = plain_pair().await;
        let rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        let mut f = enc.next(&rig.sender);
        f.payload = vec![0; 8 * 1024 * 1024 + 1].into();
        assert_eq!(rig.sender.send(f).await, SendOutcome::TooLarge);
        assert_eq!(rig.sender.stats().sent, 0);
    });
}

// ---------- staleness at the host ----------

#[test]
fn a_newer_keyframe_resets_the_unfinished_older_streams() {
    run(async {
        let pair = impaired_pair(Impairment::default(), 1).await;
        let mut rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        // Frame 1 (a keyframe) and frame 2 get through.
        for _ in 0..2 {
            let f = enc.next(&rig.sender);
            rig.sender.send(f).await;
        }
        for _ in 0..2 {
            next_frame(&mut rig, "a warm-up frame").await;
        }
        wait_idle(&rig.sender).await;
        // The link dies; two P-frames are left unfinished, then the encoder emits a keyframe.
        let link = pair.host_link.clone().unwrap();
        link.set_link_up(false);
        for _ in 0..2 {
            let f = enc.next(&rig.sender);
            assert_eq!(rig.sender.send(f).await, SendOutcome::Sent);
        }
        assert_eq!(rig.sender.in_flight(), 2);
        rig.sender.on_keyframe_request(&KeyframeRequest { display: DISPLAY, last_good_frame: 2 });
        tokio::time::sleep(Duration::from_millis(120)).await;
        let key = enc.next(&rig.sender);
        assert!(key.header.keyframe, "the receiver's request reaches the encoder");
        assert_eq!(rig.sender.send(key).await, SendOutcome::Sent);
        assert_eq!(rig.sender.stats().reset_for_keyframe, 2, "both unfinished P-frame streams were reset");
        link.set_link_up(true);
        let got = next_frame(&mut rig, "the keyframe").await;
        assert_eq!(got.header.frame_id, 5);
        assert!(got.header.keyframe);
        // The reset frames (3 and 4) never reach the decoder, and the sequence goes on from the keyframe.
        let f = enc.next(&rig.sender);
        rig.sender.send(f).await;
        assert_eq!(next_frame(&mut rig, "the next frame").await.header.frame_id, 6);
        let r = rig.receiver.stats();
        assert_eq!(r.sequence.lost, 2, "frame ids 3 and 4 never reached the decoder");
    });
}

#[test]
fn a_backlog_over_100_ms_resets_everything_wants_a_keyframe_and_cuts_the_bitrate() {
    run(async {
        let pair = impaired_pair(Impairment::default(), 2).await;
        let mut rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        for _ in 0..3 {
            let f = enc.next(&rig.sender);
            rig.sender.send(f).await;
            next_frame(&mut rig, "a warm-up frame").await;
        }
        wait_idle(&rig.sender).await;
        let before = rig.sender.target_kbps();
        let link = pair.host_link.clone().unwrap();
        link.set_link_up(false);
        // Two P-frames pile up back to back (a sleep between them would make the second find the first stale on a
        // machine where the sleep overshoots); the third, 150 ms later, finds them older than 100 ms.
        for _ in 0..2 {
            let f = enc.next(&rig.sender);
            assert_eq!(rig.sender.send(f).await, SendOutcome::Sent);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let f = enc.next(&rig.sender);
        assert_eq!(rig.sender.send(f).await, SendOutcome::DroppedBacklog);
        let s = rig.sender.stats();
        assert_eq!(s.reset_for_backlog, 2);
        assert!(rig.sender.target_kbps() < before, "the bitrate was cut: {} -> {}", before, rig.sender.target_kbps());
        assert_eq!(*rig.bitrates.lock().unwrap().last().unwrap(), rig.sender.target_kbps(), "the encoder was told");
        // An encoder that has not yet heard that a keyframe is wanted: its P-frames are dropped, not sent.
        let stale_p = swapdesk_video::Frame { header: header(100, false, 0), payload: payload_of(100, 1000).into() };
        assert_eq!(rig.sender.send(stale_p).await, SendOutcome::DroppedAwaitingKeyframe);
        link.set_link_up(true);
        let f = enc.next(&rig.sender);
        assert!(f.header.keyframe, "the encoder was told to emit a keyframe");
        assert_eq!(rig.sender.send(f).await, SendOutcome::Sent);
        let got = next_frame(&mut rig, "the keyframe").await;
        assert!(got.header.keyframe);
        assert_eq!(got.header.frame_id, 7);
        let r = rig.receiver.stats();
        assert_eq!((r.sequence.delivered, r.sequence.lost), (4, 3), "frames 4 to 6 never reached the decoder");
    });
}

#[test]
fn a_paused_sender_sends_nothing_and_wants_a_keyframe_on_resume() {
    run(async {
        let pair = plain_pair().await;
        let mut rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        let f = enc.next(&rig.sender);
        rig.sender.send(f).await;
        next_frame(&mut rig, "the first frame").await;
        rig.sender.set_paused(true);
        for _ in 0..5 {
            let f = enc.next(&rig.sender);
            assert_eq!(rig.sender.send(f).await, SendOutcome::Paused);
        }
        assert_eq!(rig.sender.stats().dropped_paused, 5);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), rig.frames.recv()).await.is_err(),
            "a frame arrived while paused"
        );
        rig.sender.set_paused(false);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let f = enc.next(&rig.sender);
        assert!(f.header.keyframe, "frames were skipped, so a keyframe is wanted");
        assert_eq!(rig.sender.send(f).await, SendOutcome::Sent);
        assert_eq!(next_frame(&mut rig, "the keyframe").await.header.frame_id, 7);
    });
}

#[test]
fn rate_control_follows_the_receiver_reports_over_a_real_connection() {
    run(async {
        let pair = plain_pair().await;
        let rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let start = rig.sender.target_kbps();
        let report = |lost| ReceiverReport {
            frames_presented: 60,
            frames_lost: lost,
            decode_us_p50: 1,
            present_lag_us_p50: 1,
            audio_underruns: 0,
        };
        rig.sender.on_receiver_report(&report(0));
        let probed = rig.sender.target_kbps();
        assert!(probed > start, "a calm report probes upward: {start} -> {probed}");
        rig.sender.on_receiver_report(&report(4));
        let cut = rig.sender.target_kbps();
        assert!(cut < probed, "frame loss lowers it: {probed} -> {cut}");
        rig.sender.set_bitrate_cap(5_000);
        assert_eq!(rig.sender.target_kbps(), 5_000, "the viewer's ceiling applies at once");
        let told = rig.bitrates.lock().unwrap().clone();
        assert_eq!(told, vec![start, probed, cut, 5_000], "every change reached the encoder's sink");
    });
}

// ---------- staleness and malformed streams at the viewer ----------

#[test]
fn the_viewer_stops_streams_of_frames_older_than_the_last_presented() {
    run(async {
        let pair = plain_pair().await;
        let (requests, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let (receiver, mut frames) = VideoReceiver::new(
            ReceiverConfig::default(),
            Arc::new(move |r: KeyframeRequest| {
                let _ = requests.send(r);
            }),
        );
        tokio::spawn({
            let (r, c) = (receiver.clone(), pair.viewer.clone());
            async move { r.serve(c).await }
        });
        let key = payload_of(10, 1000);
        raw_stream(&pair.host, &header(10, true, key.len()).encode().unwrap(), &key, true).await;
        assert_eq!(within(Duration::from_secs(3), "frame 10", frames.recv()).await.unwrap().header.frame_id, 10);

        // Frame 9 is older than the one presented: its stream is stopped at the header, before any payload.
        let old = raw_stream(&pair.host, &header(9, false, 5_000_000).encode().unwrap(), &[], false).await;
        let code = within(Duration::from_secs(3), "the stop of frame 9", old.stopped()).await.unwrap();
        assert_eq!(code.map(|c| c.into_inner()), Some(u64::from(STOP_STALE)));

        // A gap: frame 12 finishes with 11 missing. After the hold the viewer asks for a keyframe, and a P-frame
        // that opens meanwhile is stopped at its header.
        let p12 = payload_of(12, 1000);
        raw_stream(&pair.host, &header(12, false, p12.len()).encode().unwrap(), &p12, true).await;
        let req = within(Duration::from_secs(3), "the keyframe request", asked.recv()).await.unwrap();
        assert_eq!(req, KeyframeRequest { display: DISPLAY, last_good_frame: 10 });
        let p13 = raw_stream(&pair.host, &header(13, false, 5_000_000).encode().unwrap(), &[], false).await;
        let code = within(Duration::from_secs(3), "the stop of frame 13", p13.stopped()).await.unwrap();
        assert_eq!(code.map(|c| c.into_inner()), Some(u64::from(STOP_STALE)));
        assert!(frames.try_recv().is_err(), "nothing undecodable reached the decoder");

        // The keyframe ends the wait.
        let k14 = payload_of(14, 1000);
        raw_stream(&pair.host, &header(14, true, k14.len()).encode().unwrap(), &k14, true).await;
        assert_eq!(within(Duration::from_secs(3), "the keyframe", frames.recv()).await.unwrap().header.frame_id, 14);
        let s = receiver.stats();
        assert_eq!((s.sequence.delivered, s.sequence.lost, s.refused), (2, 3, 2));
    });
}

#[test]
fn a_decode_error_asks_for_a_keyframe() {
    run(async {
        let pair = plain_pair().await;
        let mut rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        for _ in 0..3 {
            let f = enc.next(&rig.sender);
            rig.sender.send(f).await;
            next_frame(&mut rig, "a frame").await;
        }
        rig.receiver.decode_failed(DISPLAY);
        // The request travels the control stream; the encoder then emits a keyframe.
        let key = until_keyframe(&mut enc, &rig.sender).await;
        assert!(key.header.keyframe);
        rig.sender.send(key).await;
        assert!(next_frame(&mut rig, "the keyframe").await.header.keyframe);
    });
}

struct CountMalformed(std::sync::atomic::AtomicU32);

impl FeedbackSink for CountMalformed {
    fn request_keyframe(&self, _: KeyframeRequest) {}
    fn malformed_frame(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn malformed_frame_streams_are_stopped_and_reported() {
    run(async {
        let pair = plain_pair().await;
        let feedback = Arc::new(CountMalformed(Default::default()));
        let (receiver, mut frames) = VideoReceiver::new(ReceiverConfig::default(), feedback.clone());
        tokio::spawn({
            let (r, c) = (receiver.clone(), pair.viewer.clone());
            async move { r.serve(c).await }
        });
        let violation = Some(u64::from(ErrorCode::ProtocolViolation.tag()));

        // A bad header version: the stream is stopped at the header.
        let mut head = header(1, true, 10).encode().unwrap();
        head[0] = 2;
        let s = raw_stream(&pair.host, &head, &[0; 10], false).await;
        let code = within(Duration::from_secs(3), "stop", s.stopped()).await.unwrap();
        assert_eq!(code.map(|c| c.into_inner()), violation);

        // A declared length over 8 MiB is refused as soon as the header is read.
        let mut head = header(4, true, 10).encode().unwrap();
        head[28..32].copy_from_slice(&(8 * 1024 * 1024 + 1u32).to_le_bytes());
        assert_eq!(head.len(), VIDEO_HEADER_LEN);
        let s = raw_stream(&pair.host, &head, &[], false).await;
        let code = within(Duration::from_secs(3), "stop", s.stopped()).await.unwrap();
        assert_eq!(code.map(|c| c.into_inner()), violation);

        // A payload shorter than its declared length, and one longer.
        raw_stream(&pair.host, &header(2, true, 1000).encode().unwrap(), &[0; 10], true).await;
        raw_stream(&pair.host, &header(3, true, 10).encode().unwrap(), &[0; 20], true).await;
        let t0 = Instant::now();
        while receiver.stats().malformed < 4 {
            assert!(
                t0.elapsed() < Duration::from_secs(3),
                "only {} of 4 malformed streams were noticed",
                receiver.stats().malformed
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(feedback.0.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert!(frames.try_recv().is_err(), "a malformed frame reached the decoder");
    });
}

#[test]
fn past_the_buffer_budget_the_oldest_unfinished_streams_are_stopped_first() {
    run(async {
        let pair = plain_pair().await;
        let cfg = ReceiverConfig { max_buffered: 300_000, ..ReceiverConfig::default() };
        let (receiver, _frames) = VideoReceiver::new(cfg, Arc::new(|_: KeyframeRequest| {}));
        tokio::spawn({
            let (r, c) = (receiver.clone(), pair.viewer.clone());
            async move { r.serve(c).await }
        });
        // Three keyframes that never finish: 200 KB of 1 MB each.
        let body = vec![1u8; 200_000];
        let mut streams = Vec::new();
        for id in 1..=3 {
            streams.push(raw_stream(&pair.host, &header(id, true, 1_000_000).encode().unwrap(), &body, false).await);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let oldest = within(Duration::from_secs(3), "the budget stop", streams[0].stopped()).await.unwrap();
        assert_eq!(oldest.map(|c| c.into_inner()), Some(u64::from(STOP_BUDGET)));
        assert!(receiver.stats().budget_stops >= 1);
        // The newest one is still being read.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), streams[2].stopped()).await.is_err(),
            "the newest stream was stopped too"
        );
    });
}

// ---------- F-VIEW-04: recovery at 1% loss ----------

/// Runs a 60 fps encoder for `secs`, losing one frame at the host every second from `first_loss` on (it is
/// encoded, never sent: what a backlog reset or a capture drop looks like to the viewer), and returns for each loss
/// the time until the viewer shows a clean sequence again, plus the decoder model and the counters.
async fn recovery_run(
    pair: &Pair,
    secs: u64,
    first_loss: u32,
) -> (Vec<f64>, DecoderModel, u64, swapdesk_video::ReceiverStats, swapdesk_video::SenderStats) {
    let Rig { sender, receiver, mut frames, bitrates: _ } =
        rig(pair, SenderConfig::default(), ReceiverConfig::default()).await;
    let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
    enc.gop = 100_000;

    let deliveries = Arc::new(std::sync::Mutex::new(Vec::<(Instant, u32, bool, bool)>::new()));
    let consumer = tokio::spawn({
        let deliveries = deliveries.clone();
        async move {
            while let Some(f) = frames.recv().await {
                let ok = payload_ok(f.header.frame_id, &f.payload);
                deliveries.lock().unwrap().push((Instant::now(), f.header.frame_id, f.header.keyframe, ok));
            }
        }
    });

    let period = Duration::from_nanos(1_000_000_000 / 60);
    let start = tokio::time::Instant::now();
    let total = (secs * 60) as u32;
    let mut injections = Vec::new();
    for n in 0..total {
        tokio::time::sleep_until(start + period * n).await;
        if n >= first_loss && (n - first_loss).is_multiple_of(60) {
            let id = enc.skip(&sender);
            injections.push((Instant::now(), id));
            continue;
        }
        let f = enc.next(&sender);
        sender.send(f).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    consumer.abort();
    let seen = deliveries.lock().unwrap().clone();
    let mut decoder = DecoderModel::default();
    let mut damaged = 0;
    for (_, id, key, ok) in &seen {
        decoder.feed_raw(*id, *key);
        damaged += u64::from(!ok);
    }
    let mut recoveries = Vec::new();
    for (at, lost_id) in &injections {
        // Clean again = the first keyframe after the loss that the sequence goes on from (its successor follows). On a
        // machine so busy that an acknowledgement takes over 100 ms the sender cuts its backlog and drops the frame
        // after a keyframe: the next keyframe is the recovery then, and the 500 ms still has to hold.
        let clean = (0..seen.len())
            .filter(|&i| seen[i].2 && seen[i].1.wrapping_sub(*lost_id) as i32 > 0)
            .find_map(|i| seen[i + 1..].iter().find(|(_, id, _, _)| *id == seen[i].1.wrapping_add(1)))
            .unwrap_or_else(|| panic!("no keyframe followed by its successor after the loss of frame {lost_id}"));
        recoveries.push(clean.0.duration_since(*at).as_secs_f64() * 1000.0);
    }
    (recoveries, decoder, damaged, receiver.stats(), sender.stats())
}

#[test]
fn after_a_lost_frame_the_viewer_requests_a_keyframe_and_is_clean_again_within_500_ms_at_1_percent_loss() {
    run(async {
        // 1% of the packets lost each way: the frames of a 1080p stream take tens of packets each.
        let pair = impaired_pair(lossy(0.01), 7).await;
        let (mut recoveries, decoder, damaged, rstats, sstats) = recovery_run(&pair, 12, 30).await;
        recoveries.sort_by(f64::total_cmp);
        let host = pair.host_link.clone().unwrap().stats();
        let viewer = pair.viewer_link.clone().unwrap().stats();
        eprintln!(
            "F-VIEW-04 at 1% loss: {} losses, recovery ms min {:.0} median {:.0} max {:.0}; packets lost host {} of {}, viewer {} of {}; keyframes sent {}; receiver lost {} dropped {} requests {}",
            recoveries.len(),
            recoveries[0],
            percentile(&recoveries, 0.5),
            recoveries.last().unwrap(),
            host.dropped_loss,
            host.sent,
            viewer.dropped_loss,
            viewer.sent,
            sstats.keyframes_sent,
            rstats.sequence.lost,
            rstats.sequence.dropped,
            rstats.sequence.requests,
        );
        assert!(recoveries.len() >= 10, "{} losses injected", recoveries.len());
        assert!(host.dropped_loss + viewer.dropped_loss > 0, "the link lost no packet, so the case proved nothing");
        for r in &recoveries {
            assert!(*r <= 500.0, "a clean picture came back after {r:.0} ms: {recoveries:?}");
        }
        assert_eq!(
            (decoder.errors, damaged),
            (0, 0),
            "the decoder was given a frame it could not decode, or a payload was damaged"
        );
    });
}

// ---------- B2 and B1, transport part ----------

struct Sustained {
    sent: u64,
    delivered: u64,
    per_second: Vec<u64>,
    latency_ms: Vec<f64>,
    decoder: DecoderModel,
    lost: u64,
    damaged: u64,
    /// Frames the host itself discarded: reset (a keyframe or a backlog, PROTOCOL.md 4) or dropped.
    host_discards: u64,
}

/// 1080p60-sized frames (a 200 KB keyframe every 2 s, P-frames of 50 to 100 KB) for `secs` seconds.
async fn sustained_run(pair: &Pair, secs: u64) -> Sustained {
    let Rig { sender, receiver, mut frames, bitrates: _ } =
        rig(pair, SenderConfig::default(), ReceiverConfig::default()).await;
    let clock = Arc::new(MonotonicClock::new());
    let mut enc = TestEncoder::new(clock.clone());
    let origin = Instant::now();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let consumer = tokio::spawn({
        let (clock, done) = (clock.clone(), done.clone());
        async move {
            let mut decoder = DecoderModel::default();
            let (mut per_second, mut latency_ms, mut delivered, mut damaged) =
                (vec![0u64; secs as usize + 3], Vec::new(), 0u64, 0u64);
            loop {
                let f = match tokio::time::timeout(Duration::from_millis(700), frames.recv()).await {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    Err(_) if done.load(std::sync::atomic::Ordering::SeqCst) => break,
                    Err(_) => continue,
                };
                let now = clock.now().0;
                latency_ms.push(now.saturating_sub(f.header.capture_us) as f64 / 1000.0);
                let window = f.received_at.duration_since(origin).as_secs() as usize;
                if let Some(slot) = per_second.get_mut(window) {
                    *slot += 1;
                }
                damaged += u64::from(!payload_ok(f.header.frame_id, &f.payload));
                decoder.feed(&f.header);
                delivered += 1;
            }
            (per_second, latency_ms, delivered, decoder, damaged)
        }
    });
    let period = Duration::from_nanos(1_000_000_000 / 60);
    let start = tokio::time::Instant::now();
    let total = secs * 60;
    let mut sent = 0u64;
    for n in 0..total {
        tokio::time::sleep_until(start + period * n as u32).await;
        let f = enc.next(&sender);
        if sender.send(f).await == SendOutcome::Sent {
            sent += 1;
        }
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let (per_second, latency_ms, delivered, decoder, damaged) =
        within(Duration::from_secs(5), "the viewer to drain", consumer).await.unwrap();
    let st = sender.stats();
    let host_discards = st.reset_for_keyframe
        + st.reset_for_backlog
        + st.reset_for_timeout
        + st.reset_for_pause
        + st.dropped_other
        + st.dropped_paused;
    Sustained {
        sent,
        delivered,
        per_second: per_second[..secs as usize].to_vec(),
        latency_ms,
        decoder,
        lost: receiver.stats().sequence.lost,
        damaged,
        host_discards,
    }
}

fn report_sustained(label: &str, s: &Sustained) -> (f64, f64, f64) {
    let mut windows: Vec<f64> = s.per_second.iter().map(|n| *n as f64).collect();
    windows.sort_by(f64::total_cmp);
    let mut lat = s.latency_ms.clone();
    lat.sort_by(f64::total_cmp);
    let (median, p95, max) = (percentile(&lat, 0.5), percentile(&lat, 0.95), lat.last().copied().unwrap_or(0.0));
    let loss = 100.0 * (s.sent - s.delivered.min(s.sent)) as f64 / s.sent as f64;
    eprintln!(
        "{label}: sent {} delivered {} loss {:.3}% fps per 1 s window {:?} (median {}); transport latency ms median {:.2} p95 {:.2} max {:.2}; decoder errors {}",
        s.sent,
        s.delivered,
        loss,
        s.per_second,
        percentile(&windows, 0.5),
        median,
        p95,
        max,
        s.decoder.errors
    );
    (median, p95, loss)
}

fn check_sustained(s: &Sustained) {
    let mut windows: Vec<f64> = s.per_second.iter().map(|n| *n as f64).collect();
    windows.sort_by(f64::total_cmp);
    assert!(percentile(&windows, 0.5) >= 59.0, "median fps per 1 s window {:?}", s.per_second);
    let loss = (s.sent - s.delivered.min(s.sent)) as f64 / s.sent as f64;
    assert!(loss <= 0.005, "frame loss {:.3}% (sent {} delivered {})", loss * 100.0, s.sent, s.delivered);
    // The stream of a frame still unacknowledged when a keyframe is queued is reset by the host (PROTOCOL.md 4), and
    // on a machine that is busy an acknowledgement can take longer than one frame period: so a frame id may be missing,
    // but only one the host discarded itself. The viewer loses nothing of its own, and the decoder is never given a
    // frame it cannot decode.
    assert!(
        s.lost <= s.host_discards,
        "ids lost at the viewer {} but the host discarded only {}",
        s.lost,
        s.host_discards
    );
    assert_eq!(
        (s.decoder.errors, s.damaged),
        (0, 0),
        "decoder errors {}, damaged payloads {}",
        s.decoder.errors,
        s.damaged
    );
}

#[test]
fn b2_transport_sustains_1080p60_sized_frames_for_10_seconds_on_loopback() {
    run(async {
        let pair = plain_pair().await;
        let s = sustained_run(&pair, 10).await;
        let (median, p95, _) = report_sustained("B2/B1 plain loopback", &s);
        check_sustained(&s);
        // B1 allows 40 ms median and 60 ms p95 for the whole capture-to-present path: the transport alone must
        // leave room for capture, encode, decode and present (measured: about 1 ms and 2 ms).
        assert!(median <= 10.0 && p95 <= 25.0, "transport latency median {median:.2} ms, p95 {p95:.2} ms");
    });
}

#[test]
fn b2_transport_through_an_unimpaired_impairment_layer() {
    run(async {
        let pair = impaired_pair(Impairment::default(), 3).await;
        let s = sustained_run(&pair, 10).await;
        report_sustained("B2/B1 ImpairedSocket at 0%", &s);
        check_sustained(&s);
    });
}

// ---------- stage 37 (review stage14-session-video.md): findings 3, 5 and 20 ----------

/// A receiver on the viewer's connection whose frames the test reads itself.
fn bare_receiver(pair: &Pair, cfg: ReceiverConfig) -> (VideoReceiver, tokio::sync::mpsc::Receiver<ReceivedFrame>) {
    let (receiver, frames) = VideoReceiver::new(cfg, Arc::new(|_: KeyframeRequest| {}));
    tokio::spawn({
        let (r, c) = (receiver.clone(), pair.viewer.clone());
        async move { r.serve(c).await }
    });
    (receiver, frames)
}

const MIB: usize = 1024 * 1024;

/// Finding 3: the frames queued for the decoder count in the byte budget, whatever the frame count of the queue.
#[test]
fn f3_the_decoder_queue_is_bounded_in_bytes() {
    run(async {
        let pair = plain_pair().await;
        let cfg = ReceiverConfig { max_buffered: 4 * MIB, ..ReceiverConfig::default() };
        let (_receiver, frames) = bare_receiver(&pair, cfg);
        // A decoder that does not read: a keyframe and eleven P-frames of 1 MiB each.
        for id in 1..=12u32 {
            let body = payload_of(id, MIB);
            raw_stream(&pair.host, &header(id, id == 1, body.len()).encode().unwrap(), &body, true).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(frames.len() <= 4, "{} MiB are queued for the decoder, over the 4 MiB budget", frames.len());
    });
}

/// Finding 3: a frame parked behind a missing predecessor counts in the budget, so a stream that pushes past it is stopped.
#[test]
fn f3_parked_frames_count_in_the_buffer_budget() {
    run(async {
        let pair = plain_pair().await;
        let cfg = ReceiverConfig { max_buffered: 2 * MIB, hold: Duration::from_secs(30), ..ReceiverConfig::default() };
        let (_receiver, mut frames) = bare_receiver(&pair, cfg);
        let key = payload_of(1, 1000);
        raw_stream(&pair.host, &header(1, true, key.len()).encode().unwrap(), &key, true).await;
        within(Duration::from_secs(3), "the keyframe", frames.recv()).await.unwrap();
        // Frame 2 never comes; frames 3 and 4 finish and are parked: 2 MiB, the whole budget.
        for id in 3..=4u32 {
            let body = payload_of(id, MIB);
            raw_stream(&pair.host, &header(id, false, body.len()).encode().unwrap(), &body, true).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Frame 5 starts to arrive: past the budget, it is stopped for it.
        let body = payload_of(5, MIB);
        let s5 = raw_stream(&pair.host, &header(5, false, MIB).encode().unwrap(), &body[..MIB / 2], false).await;
        let code = within(Duration::from_secs(1), "the budget stop of frame 5", s5.stopped()).await.unwrap();
        assert_eq!(code.map(|c| c.into_inner()), Some(u64::from(STOP_BUDGET)));
    });
}

/// Finding 20b: a stream still open for a frame older than the one just presented is stopped at once.
#[test]
fn f20b_an_open_stream_older_than_the_presented_frame_is_stopped() {
    run(async {
        let pair = plain_pair().await;
        let (_receiver, mut frames) = bare_receiver(&pair, ReceiverConfig::default());
        let key = payload_of(1, 1000);
        raw_stream(&pair.host, &header(1, true, key.len()).encode().unwrap(), &key, true).await;
        within(Duration::from_secs(3), "frame 1", frames.recv()).await.unwrap();
        // Frame 3 starts and stalls (its stream stays open); frame 2 then arrives and is presented, and so is a
        // later keyframe.
        let p3 = payload_of(3, 100_000);
        let s3 = raw_stream(&pair.host, &header(3, false, p3.len()).encode().unwrap(), &p3[..10_000], false).await;
        let p2 = payload_of(2, 1000);
        raw_stream(&pair.host, &header(2, false, p2.len()).encode().unwrap(), &p2, true).await;
        within(Duration::from_secs(3), "frame 2", frames.recv()).await.unwrap();
        // The host moves on: a keyframe 5 is presented while frame 3 still has its stream open.
        let k5 = payload_of(5, 1000);
        raw_stream(&pair.host, &header(5, true, k5.len()).encode().unwrap(), &k5, true).await;
        within(Duration::from_secs(3), "frame 5", frames.recv()).await.unwrap();
        let code =
            within(Duration::from_millis(500), "the stop of the stale stream of frame 3", s3.stopped()).await.unwrap();
        assert_eq!(code.map(|c| c.into_inner()), Some(u64::from(STOP_STALE)));
    });
}

/// Finding 20c: an idle receiver does not wake its ordering timer every 10 ms (B6).
#[test]
fn f20c_an_idle_receiver_does_not_wake_every_tick() {
    run(async {
        let pair = plain_pair().await;
        let (receiver, mut frames) = bare_receiver(&pair, ReceiverConfig::default());
        let key = payload_of(1, 1000);
        raw_stream(&pair.host, &header(1, true, key.len()).encode().unwrap(), &key, true).await;
        within(Duration::from_secs(3), "frame 1", frames.recv()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = receiver.stats().ticks;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let woke = receiver.stats().ticks - before;
        assert!(woke <= 3, "the ordering timer woke {woke} times in 600 ms with nothing waiting");
    });
}

/// Finding 5: pausing (a revoked `VIEW`, a locked desktop) resets the frames still in flight at once and they are never sent.
#[test]
fn f5_pausing_resets_the_frames_in_flight_and_they_are_not_sent_later() {
    run(async {
        let pair = impaired_pair(Impairment::default(), 21).await;
        let mut rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        for _ in 0..2 {
            let f = enc.next(&rig.sender);
            rig.sender.send(f).await;
            next_frame(&mut rig, "a warm-up frame").await;
        }
        wait_idle(&rig.sender).await;
        let link = pair.host_link.clone().unwrap();
        link.set_link_up(false);
        for _ in 0..3 {
            let f = enc.next(&rig.sender);
            assert_eq!(rig.sender.send(f).await, SendOutcome::Sent);
        }
        assert_eq!(rig.sender.in_flight(), 3);
        rig.sender.set_paused(true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(rig.sender.in_flight(), 0, "the frames in flight were reset when the sender paused");
        link.set_link_up(true);
        assert!(
            tokio::time::timeout(Duration::from_millis(600), rig.frames.recv()).await.is_err(),
            "a frame that was in flight when VIEW was revoked reached the viewer"
        );
    });
}

/// Finding 20e: a backlog reset spares a keyframe in flight (a keyframe is exempt from the age rule).
#[test]
fn f20e_a_backlog_reset_does_not_reset_the_keyframe_in_flight() {
    run(async {
        let pair = impaired_pair(Impairment::default(), 22).await;
        let mut rig = rig(&pair, SenderConfig::default(), ReceiverConfig::default()).await;
        let mut enc = TestEncoder::new(Arc::new(MonotonicClock::new()));
        let link = pair.host_link.clone().unwrap();
        link.set_link_up(false);
        // The first frame is a keyframe; it and a P-frame are in flight when the third frame finds the P-frame old.
        for _ in 0..2 {
            let f = enc.next(&rig.sender);
            assert_eq!(rig.sender.send(f).await, SendOutcome::Sent);
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        let f = enc.next(&rig.sender);
        assert_eq!(rig.sender.send(f).await, SendOutcome::DroppedBacklog);
        assert_eq!(rig.sender.stats().reset_for_backlog, 1, "only the P-frame was reset");
        assert_eq!(rig.sender.in_flight(), 1, "the keyframe is still being sent");
        link.set_link_up(true);
        let got = next_frame(&mut rig, "the keyframe").await;
        assert_eq!((got.header.frame_id, got.header.keyframe), (1, true));
    });
}
