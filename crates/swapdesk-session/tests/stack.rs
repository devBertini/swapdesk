//! Session and video transport together over one real QUIC connection through `swapdesk-trust`: what the service
//! will wire in stage 18. Video control messages reach the host's sender through the registry, frames reach the
//! viewer's receiver through its stream handler, and the session's pause and revocation stop the sender (F-VIEW-04
//! through the session, F-SESS-01's pause, F-PERM-02's revocation of `VIEW`).

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use swapdesk_fakes::MonotonicClock;
use swapdesk_platform::Clock;
use swapdesk_proto::*;
use swapdesk_session::{ChannelHandler, ForwardHandler, IncomingStream, SessionCtx, VIDEO_CONTROL_TYPES, ViewerEvent};
use swapdesk_video::{Frame, NoBitrateSink, ReceiverConfig, SenderConfig, VideoReceiver, VideoSender};
use tokio::sync::mpsc;

const VC: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::FILES);

/// The viewer's handler for `Video` streams: hands them to the receiver.
struct VideoStreams(VideoReceiver);

impl ChannelHandler for VideoStreams {
    fn name(&self) -> &'static str {
        "video-streams"
    }

    fn streams(&self) -> &'static [StreamKind] {
        &[StreamKind::Video]
    }

    fn on_message(&self, _: &SessionCtx, _: Message) {}

    fn on_stream(&self, _: &SessionCtx, stream: IncomingStream) {
        self.0.handle_stream(stream.recv);
    }
}

fn payload_of(id: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(31).wrapping_add(id) as u8).collect()
}

/// What the service's wiring does on the host: the sender follows the session's pause and grants, and the video
/// control messages reach the sender.
fn wire_host(ctx: &SessionCtx, sender: &VideoSender, mut control: mpsc::Receiver<swapdesk_session::Forwarded>) {
    fn update(ctx: &SessionCtx, sender: &VideoSender) {
        sender.set_paused(ctx.is_paused() || !ctx.granted().contains(PermissionSet::VIEW));
    }
    for which in 0..2 {
        let (ctx, sender) = (ctx.clone(), sender.clone());
        tokio::spawn(async move {
            let (mut grants, mut state) = (ctx.watch_granted(), ctx.watch_state());
            loop {
                update(&ctx, &sender);
                let changed = if which == 0 { grants.changed().await.is_ok() } else { state.changed().await.is_ok() };
                if !changed {
                    return;
                }
            }
        });
    }
    let sender = sender.clone();
    tokio::spawn(async move {
        while let Some(f) = control.recv().await {
            // A message may have waited in the channel while its permission was taken away (F-PERM-02).
            if !f.still_permitted() {
                continue;
            }
            match f.msg {
                Message::KeyframeRequest(r) => sender.on_keyframe_request(&r),
                Message::ReceiverReport(r) => sender.on_receiver_report(&r),
                _ => {}
            }
        }
    });
}

struct Producer {
    next: AtomicU32,
    clock: Arc<MonotonicClock>,
    skip: Mutex<Vec<u32>>,
    stop: std::sync::atomic::AtomicBool,
}

/// 60 fps of 30 KB frames; a keyframe at the start and whenever the sender wants one.
fn start_producer(sender: VideoSender) -> Arc<Producer> {
    let p = Arc::new(Producer {
        next: AtomicU32::new(1),
        clock: Arc::new(MonotonicClock::new()),
        skip: Mutex::new(Vec::new()),
        stop: Default::default(),
    });
    let me = p.clone();
    tokio::spawn(async move {
        let period = Duration::from_nanos(1_000_000_000 / 60);
        let start = tokio::time::Instant::now();
        let mut n = 0u32;
        let mut first = true;
        while !me.stop.load(Ordering::SeqCst) {
            tokio::time::sleep_until(start + period * n).await;
            n += 1;
            let id = me.next.fetch_add(1, Ordering::SeqCst);
            let want = sender.take_keyframe_request(0);
            let key = first || want;
            first = false;
            if lock(&me.skip).contains(&id) {
                continue;
            }
            let header = VideoHeader {
                keyframe: key,
                config: key,
                discontinuity: false,
                display_id: 0,
                frame_id: id,
                capture_us: me.clock.now().0,
                encode_us: 1_000,
                width: 1920,
                height: 1080,
                codec: Codec::H264,
                payload_len: 0,
            };
            let len = if key { 60_000 } else { 30_000 };
            let _ = sender.send(Frame { header, payload: payload_of(id, len) }).await;
        }
    });
    p
}

#[test]
fn video_runs_through_a_session_recovers_from_a_lost_frame_pauses_with_the_host_and_stops_when_view_is_revoked() {
    run(async {
        // Host: the registry carries the video control messages out and records the session.
        let (control_handler, control_rx) = ForwardHandler::new("video-control", VIDEO_CONTROL_TYPES, 256);
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::accepting(VC);
        let q = Quic::start(ui, registry_of(&[control_handler, rec.clone()]), None, |_| {}).await;
        let v = q.viewer(VC, None);

        // Viewer: the receiver, fed by the session's stream routing, asking for keyframes through the session.
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<KeyframeRequest>();
        let (receiver, mut frames) = VideoReceiver::new(
            ReceiverConfig::default(),
            Arc::new(move |r: KeyframeRequest| {
                let _ = req_tx.send(r);
            }),
        );
        let mut cfg = viewer_config(VC);
        cfg.registry = registry_of(&[Arc::new(VideoStreams(receiver.clone()))]);
        let mut s = q.open(&v, cfg).await.unwrap();
        let viewer_ctx = s.ctx().clone();
        tokio::spawn({
            let ctx = viewer_ctx.clone();
            async move {
                while let Some(r) = req_rx.recv().await {
                    let _ = ctx.send(r).await;
                }
            }
        });
        tokio::spawn({
            let (ctx, receiver) = (viewer_ctx.clone(), receiver.clone());
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if ctx.send(receiver.report(1_000, 1_000, 0)).await.is_err() {
                        return;
                    }
                }
            }
        });

        // Host: the sender on the session's own connection.
        eventually(Duration::from_secs(3), "the host session", || !lock(&rec.sessions).is_empty()).await;
        let host_ctx = lock(&rec.sessions)[0].clone();
        let sender =
            VideoSender::new(host_ctx.connection().unwrap().clone(), SenderConfig::default(), Arc::new(NoBitrateSink));
        wire_host(&host_ctx, &sender, control_rx);
        let producer = start_producer(sender.clone());

        // The decoder model and the delivery log.
        let log = Arc::new(Mutex::new(Vec::<(Instant, u32, bool)>::new()));
        let decoder = Arc::new(Mutex::new((None::<u32>, 0u64)));
        tokio::spawn({
            let (log, decoder) = (log.clone(), decoder.clone());
            async move {
                while let Some(f) = frames.recv().await {
                    let ok =
                        f.header.keyframe || lock(&decoder).0.is_some_and(|l| f.header.frame_id == l.wrapping_add(1));
                    let mut d = lock(&decoder);
                    if ok {
                        d.0 = Some(f.header.frame_id);
                    } else {
                        d.1 += 1;
                    }
                    lock(&log).push((Instant::now(), f.header.frame_id, f.header.keyframe));
                }
            }
        });
        let delivered = || lock(&log).len();

        // 1. Video flows through the session's connection.
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(delivered() >= 50, "{} frames in the first second", delivered());

        // 2. A frame lost at the host: the viewer asks through the session, the host answers with a keyframe, and
        // the picture is clean again within 500 ms.
        let lost_id = producer.next.load(Ordering::SeqCst) + 2;
        lock(&producer.skip).push(lost_id);
        let lost_at = Instant::now();
        // The picture is clean again at the first keyframe after the loss that the sequence goes on from (its successor
        // follows). A keyframe whose successor never came was followed by a reset or a drop at the host (a machine so
        // busy that an acknowledgement took over 100 ms makes the sender cut its backlog): the next keyframe is the
        // recovery then, and the 500 ms below still has to hold. The log is read until that point exists, not at a
        // fixed time, so a slow machine fails on the measured time and never on where the snapshot fell.
        let clean_at = |seen: &[(Instant, u32, bool)]| {
            (0..seen.len())
                .find(|&i| {
                    seen[i].2 && seen[i].1 > lost_id && seen[i + 1..].iter().any(|(_, id, _)| *id == seen[i].1 + 1)
                })
                .map(|i| seen[i + 1..].iter().find(|(_, id, _)| *id == seen[i].1 + 1).unwrap().0)
        };
        let clean = loop {
            let seen = lock(&log).clone();
            if let Some(at) = clean_at(&seen) {
                break at;
            }
            assert!(lost_at.elapsed() < Duration::from_secs(5), "the picture was not clean again 5 s after the loss");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let recovery = clean.duration_since(lost_at);
        eprintln!(
            "F-VIEW-04 through the session: clean again {:.0} ms after the lost frame",
            recovery.as_secs_f64() * 1000.0
        );
        assert!(recovery < Duration::from_millis(500), "recovered after {recovery:?}");
        assert!(sender.stats().keyframe_requests >= 1, "the request travelled the control stream");

        // 3. The host locks: video pauses within 1 s, input would be dropped, and it resumes with a keyframe.
        q.host.set_desktop(Desktop::Locked);
        wait_event(&mut s, Duration::from_secs(1), "paused", |e| {
            matches!(e, ViewerEvent::HostState { paused: true, .. })
        })
        .await;
        let paused_at = Instant::now();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let n = delivered();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let last_during_pause = lock(&log).last().unwrap().0;
        assert!(
            last_during_pause.duration_since(paused_at) < Duration::from_secs(1),
            "frames kept coming {:?} after the pause",
            last_during_pause.duration_since(paused_at)
        );
        assert_eq!(delivered(), n, "no frame while the host is paused");
        assert!(sender.stats().dropped_paused > 10);
        q.host.set_desktop(Desktop::Default);
        let resumed_at = Instant::now();
        while delivered() <= n + 5 {
            assert!(
                resumed_at.elapsed() < Duration::from_secs(2),
                "no frames after the pause: delivered {} of {n}+5; sender {:?} paused {}; receiver {:?}; viewer end {:?}; host end {:?}",
                delivered(),
                sender.stats(),
                sender.is_paused(),
                receiver.stats(),
                s.ctx().ended(),
                host_ctx.ended()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            lock(&log).iter().rev().take_while(|(_, id, _)| *id > 0).any(|(_, _, key)| *key),
            "the stream resumes with a keyframe"
        );

        // 4. The host's user takes `VIEW` away: the sender stops within 1 s.
        let id = s.grant().session_id;
        let flowing = delivered();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(delivered() > flowing + 10, "frames are flowing before the revocation");
        let before = delivered();
        let t = Instant::now();
        assert!(q.host.set_session_grants(id, PermissionSet::FILES));
        wait_event(
            &mut s,
            Duration::from_secs(1),
            "PermissionsChanged",
            |e| matches!(e, ViewerEvent::PermissionsChanged(g) if *g == PermissionSet::FILES),
        )
        .await;
        eventually(Duration::from_secs(1), "the sender to stop", || sender.is_paused()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let n = delivered();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(delivered(), n, "frames went on after VIEW was revoked");
        let stopped = lock(&log).last().unwrap().0.saturating_duration_since(t);
        eprintln!(
            "F-PERM-02 for video: {} frames in the 300 ms before the revocation, none after it; the last frame was {:.0} ms before it",
            before - flowing,
            t.duration_since(lock(&log).last().unwrap().0).as_secs_f64() * 1000.0
        );
        assert!(stopped < Duration::from_secs(1));

        // The decoder was never given a frame it could not decode.
        assert_eq!(lock(&decoder).1, 0);
        producer.stop.store(true, Ordering::SeqCst);
        q.stop();
    });
}
