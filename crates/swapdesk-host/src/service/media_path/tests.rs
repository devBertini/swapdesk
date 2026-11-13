//! Stage 64 (C10): the video slot and the direct audio path, alone and over a real QUIC connection through
//! `ImpairedSocket`.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::Connection;
use swapdesk_fakes::MemorySecretStore;
use swapdesk_netsim::{ImpairedSocket, Impairment, LinkHandle};
use swapdesk_proto::{Codec, KeyframeRequest, Os, VideoHeader};
use swapdesk_trust::tls::session_server_config;
use swapdesk_trust::{Identity, PairedDevice, PinLookup, TrustHandle, accept_session, connect_session};
use swapdesk_video::{BitrateSink, ReceiverConfig, SenderConfig, VideoReceiver, VideoSender};

use super::*;

fn header(id: u32, key: bool) -> VideoHeader {
    VideoHeader {
        keyframe: key,
        config: key,
        discontinuity: false,
        display_id: 0,
        frame_id: id,
        capture_us: 0,
        encode_us: 0,
        width: 64,
        height: 48,
        codec: Codec::H264,
        payload_len: 0,
    }
}

/// A path with a running capture whose keyframe requests are counted.
fn path_with_run() -> (Arc<VideoPath>, Arc<AtomicUsize>) {
    let path = VideoPath::new();
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    path.set_run(Some(VideoRun {
        display: 0,
        key: Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    }));
    (path, asked)
}

fn next_frame(path: &VideoPath) -> Option<Frame> {
    let (stamped, lost) = path.take()?;
    path.admit(stamped, lost)
}

#[test]
fn the_slot_keeps_only_the_newest_frame_and_the_gap_drops_p_frames_until_a_keyframe() {
    let (path, asked) = path_with_run();
    // Nothing waits behind a stalled send: three frames offered while the task is busy leave one, flagged as a gap.
    path.offer(header(1, true), vec![1].into());
    assert_eq!(next_frame(&path).map(|f| f.header.frame_id), Some(1), "a keyframe passes");
    for id in 2..=4 {
        path.offer(header(id, false), vec![id as u8].into());
    }
    assert_eq!(path.stats().replaced, 2, "frames 2 and 3 were replaced by 4");
    assert!(next_frame(&path).is_none(), "frame 4 follows a gap: it would not decode, so it is dropped");
    assert_eq!(asked.load(Ordering::SeqCst), 1, "and a keyframe is asked for");
    // Until the keyframe comes, P-frames are dropped without asking again.
    path.offer(header(5, false), vec![5].into());
    assert!(next_frame(&path).is_none());
    assert_eq!(asked.load(Ordering::SeqCst), 1, "asked once inside KEY_WAIT");
    path.offer(header(6, true), vec![6].into());
    assert_eq!(next_frame(&path).map(|f| f.header.frame_id), Some(6));
    path.offer(header(7, false), vec![7].into());
    assert_eq!(next_frame(&path).map(|f| f.header.frame_id), Some(7), "the stream flows again");
    assert_eq!(path.stats().keyframes_asked, 1);
}

#[test]
fn a_p_frame_that_waited_past_the_backlog_limit_is_dropped_and_a_keyframe_asked() {
    let (path, asked) = path_with_run();
    path.offer(header(1, true), vec![1].into());
    assert!(next_frame(&path).is_some());
    path.offer(header(2, false), vec![2].into());
    std::thread::sleep(STALE + Duration::from_millis(40));
    assert!(next_frame(&path).is_none(), "older than {STALE:?}: stale");
    assert_eq!(path.stats().stale, 1);
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // A stale keyframe is still a keyframe: it resets the stream.
    path.offer(header(3, true), vec![3].into());
    std::thread::sleep(STALE + Duration::from_millis(40));
    assert_eq!(next_frame(&path).map(|f| f.header.frame_id), Some(3));
}

#[test]
fn frames_without_a_capture_or_of_another_display_never_enter_the_slot() {
    let path = VideoPath::new();
    path.offer(header(1, true), vec![1].into());
    assert!(path.take().is_none(), "no capture runs");
    let (path, _) = path_with_run();
    let mut other = header(1, true);
    other.display_id = 9;
    path.offer(other, vec![1].into());
    assert!(path.take().is_none(), "another display");
    path.offer(header(2, true), vec![2].into());
    path.set_run(None);
    assert!(path.take().is_none(), "a stopped capture takes its waiting frame with it");
}

// ---------- over QUIC through ImpairedSocket ----------

struct Pair {
    host: Connection,
    viewer: Connection,
    host_link: LinkHandle,
    viewer_link: LinkHandle,
    _endpoints: (quinn::Endpoint, quinn::Endpoint),
}

async fn impaired_pair() -> Pair {
    let lo = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let host_id = Identity::generate(0).unwrap();
    let viewer_id = Identity::generate(0).unwrap();
    let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
    trust
        .update(|s| {
            s.devices.push(PairedDevice::new_paired(
                *viewer_id.spki(),
                "viewer".into(),
                Os::Windows,
                "11".into(),
                1,
                None,
            ))
        })
        .unwrap();
    let hs = ImpairedSocket::bind(lo, Impairment::default(), 1).unwrap();
    let vs = ImpairedSocket::bind(lo, Impairment::default(), 2).unwrap();
    let (host_link, viewer_link) = (hs.handle(), vs.handle());
    let server = session_server_config(&host_id, trust.clone()).unwrap();
    let host_ep = hs.into_endpoint(Some(server)).unwrap();
    let viewer_ep = vs.into_endpoint(None).unwrap();
    let addr = host_ep.local_addr().unwrap();
    let pin: Arc<dyn PinLookup> = trust;
    let ep = host_ep.clone();
    let accepting =
        tokio::spawn(async move { accept_session(ep.accept().await.expect("an incoming connection"), &*pin).await });
    let viewer = connect_session(&viewer_ep, &viewer_id, addr, *host_id.spki()).await.unwrap();
    let (host, _) = accepting.await.unwrap().unwrap();
    Pair { host, viewer, host_link, viewer_link, _endpoints: (host_ep, viewer_ep) }
}

struct NoRate;

impl BitrateSink for NoRate {
    fn set_target_kbps(&self, _kbps: u32) {}
}

/// A chain-checking decoder: a keyframe, or the successor of the last frame it decoded.
#[derive(Default)]
struct Decoder {
    last: Option<u32>,
    /// (when, frame id) of every frame the chain broke on.
    errors: Vec<Instant>,
    decoded: Vec<Instant>,
    keyframes: Vec<Instant>,
}

impl Decoder {
    fn feed(&mut self, id: u32, key: bool, at: Instant) {
        if key || self.last.is_some_and(|l| id == l.wrapping_add(1)) {
            self.last = Some(id);
            self.decoded.push(at);
            if key {
                self.keyframes.push(at);
            }
        } else {
            self.errors.push(at);
        }
    }
}

#[test]
fn a_300_ms_stall_holds_no_audio_back_and_the_video_recovers_with_a_keyframe() {
    const STALL_AT: Duration = Duration::from_millis(600);
    const STALL: Duration = Duration::from_millis(300);
    const RUN: Duration = Duration::from_millis(2600);
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async {
        let pair = impaired_pair().await;

        // Viewer: video through the receiver, audio datagrams stamped on arrival.
        let decoder = Arc::new(Mutex::new(Decoder::default()));
        let (receiver, mut frames) = VideoReceiver::new(ReceiverConfig::default(), Arc::new(|_: KeyframeRequest| {}));
        tokio::spawn({
            let (receiver, conn) = (receiver.clone(), pair.viewer.clone());
            async move { receiver.serve(conn).await }
        });
        tokio::spawn({
            let decoder = Arc::clone(&decoder);
            async move {
                while let Some(f) = frames.recv().await {
                    let key = f.header.keyframe || f.header.discontinuity;
                    decoder.lock().unwrap().feed(f.header.frame_id, key, Instant::now());
                }
            }
        });
        let heard: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::default();
        tokio::spawn({
            let (conn, heard) = (pair.viewer.clone(), Arc::clone(&heard));
            async move {
                while let Ok(d) = conn.read_datagram().await {
                    if d.first() == Some(&0x01) && d.len() >= 5 {
                        let seq = u32::from_be_bytes([d[1], d[2], d[3], d[4]]);
                        heard.lock().unwrap().push((seq, Instant::now()));
                    }
                }
            }
        });

        // Host: the sender, the video task and the direct audio path.
        let sender = VideoSender::new(pair.host.clone(), SenderConfig::default(), Arc::new(NoRate));
        let path = VideoPath::new();
        let key_wanted = Arc::new(AtomicBool::new(false));
        let key_calls = Arc::new(AtomicUsize::new(0));
        {
            let (wanted, calls) = (Arc::clone(&key_wanted), Arc::clone(&key_calls));
            path.set_run(Some(VideoRun {
                display: 0,
                key: Arc::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    wanted.store(true, Ordering::SeqCst);
                }),
            }));
        }
        tokio::spawn(Arc::clone(&path).run(sender.clone()));
        let out = Arc::new(DirectOut::new(None, Some(pair.host.clone())));
        out.set(true, true);

        let begin = Instant::now();
        let done = Arc::new(AtomicBool::new(false));
        // The encoder: 60 fps, a keyframe first and whenever one was asked for.
        let produced = Arc::new(AtomicU32::new(0));
        tokio::spawn({
            let (path, wanted, done, produced) =
                (Arc::clone(&path), Arc::clone(&key_wanted), Arc::clone(&done), Arc::clone(&produced));
            async move {
                let mut id = 0u32;
                let mut tick = tokio::time::interval(Duration::from_millis(16));
                while !done.load(Ordering::Relaxed) {
                    tick.tick().await;
                    id += 1;
                    let key = id == 1 || wanted.swap(false, Ordering::SeqCst);
                    let len = if key { 90_000 } else { 30_000 };
                    path.offer(header(id, key), vec![(id % 251) as u8; len].into());
                    produced.store(id, Ordering::Relaxed);
                }
            }
        });
        // Audio: 10 ms packets, each stamped with its send time. `send` times show the path never waits.
        let sent: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::default();
        tokio::spawn({
            let (out, sent, done) = (Arc::clone(&out), Arc::clone(&sent), Arc::clone(&done));
            async move {
                let mut tick = tokio::time::interval(Duration::from_millis(10));
                let mut seq = 0u32;
                while !done.load(Ordering::Relaxed) {
                    tick.tick().await;
                    seq += 1;
                    let mut d = vec![0x01];
                    d.extend_from_slice(&seq.to_be_bytes());
                    d.extend_from_slice(&[0u8; 40]);
                    sent.lock().unwrap().push((seq, Instant::now()));
                    out.audio(d);
                }
            }
        });

        // The stall.
        tokio::time::sleep(STALL_AT).await;
        let stall_start = Instant::now();
        pair.host_link.set_link_up(false);
        pair.viewer_link.set_link_up(false);
        tokio::time::sleep(STALL).await;
        pair.host_link.set_link_up(true);
        pair.viewer_link.set_link_up(true);
        let stall_end = Instant::now();
        tokio::time::sleep(RUN - STALL_AT - STALL).await;
        done.store(true, Ordering::Relaxed);
        path.close();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Audio: the sending side never waited (no gap in the sends), and what was sent while the link was up arrived,
        // with no hole between consecutive packets outside the outage.
        let sent = sent.lock().unwrap().clone();
        let send_gap = sent.windows(2).map(|w| w[1].1 - w[0].1).max().unwrap();
        assert!(send_gap < Duration::from_millis(60), "the audio sender waited {send_gap:?}");
        let heard = heard.lock().unwrap().clone();
        let outage = (stall_start - Duration::from_millis(60), stall_end + Duration::from_millis(60));
        let must_arrive: Vec<u32> =
            sent.iter().filter(|(_, at)| *at < outage.0 || *at > outage.1).map(|(seq, _)| *seq).collect();
        let missing: Vec<u32> = must_arrive.iter().copied().filter(|s| !heard.iter().any(|(h, _)| h == s)).collect();
        assert!(
            missing.is_empty(),
            "{} of {} audio packets outside the outage never arrived: {missing:?}",
            missing.len(),
            must_arrive.len()
        );
        let late = heard
            .iter()
            .filter(|(_, at)| *at > stall_end + Duration::from_millis(60))
            .map(|(_, at)| *at)
            .collect::<Vec<_>>();
        let hole = late.windows(2).map(|w| w[1] - w[0]).max().unwrap_or_default();
        assert!(hole < Duration::from_millis(60), "audio had a hole of {hole:?} after the stall");
        // QUIC's own loss recovery (the video streams that were in flight hold the congestion window until a probe)
        // sets how soon after the link returns a datagram leaves; the relay adds nothing to it.
        let first_back =
            heard.iter().map(|(_, at)| *at).filter(|at| *at > stall_end).min().expect("audio after the stall");
        assert!(
            first_back - stall_end < Duration::from_millis(400),
            "audio resumed {:?} after the link came back",
            first_back - stall_end
        );

        // Video: the stall broke the chain, a keyframe was asked for, and the stream is clean again after it.
        let stats = path.stats();
        assert!(key_calls.load(Ordering::SeqCst) >= 1, "a keyframe was asked for ({stats:?})");
        let d = decoder.lock().unwrap();
        let recovery = d
            .keyframes
            .iter()
            .find(|at| **at > stall_end)
            .copied()
            .unwrap_or_else(|| panic!("no keyframe reached the viewer after the stall ({stats:?})"));
        assert!(
            recovery - stall_end < Duration::from_millis(1500),
            "the keyframe came {:?} after the link returned",
            recovery - stall_end
        );
        let after = d.decoded.iter().filter(|at| **at >= recovery).count();
        assert!(after >= 20, "only {after} frames decoded after the recovery keyframe ({stats:?})");
        let broken = d.errors.iter().filter(|at| **at >= recovery).count();
        assert_eq!(broken, 0, "the chain broke {broken} times after the recovery keyframe");
        assert!(begin.elapsed() > RUN);
    });
}
