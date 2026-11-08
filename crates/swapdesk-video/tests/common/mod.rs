#![allow(dead_code)]
//! Test harness: two machines joined over loopback through `swapdesk-trust` (pinned keys, the post-handshake
//! gate), optionally through `ImpairedSocket`, plus a synthetic encoder whose frames are opaque payloads with the
//! frame counter and capture time in the header (what the SyntheticScreen barcode carries in a real run).

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use swapdesk_fakes::{MemorySecretStore, MonotonicClock};
use swapdesk_netsim::{ImpairedSocket, Impairment, LinkHandle};
use swapdesk_platform::Clock;
use swapdesk_proto::{Codec, FrameHeader, KeyframeRequest, Message, Os, VideoHeader, Wire};
use swapdesk_trust::tls::session_server_config;
use swapdesk_trust::{
    Identity, PairedDevice, PinLookup, TrustHandle, accept_session, connect_session, endpoint_from_socket,
    open_session_endpoint,
};
use swapdesk_video::{BitrateSink, Frame, ReceivedFrame, ReceiverConfig, SenderConfig, VideoReceiver, VideoSender};
use tokio::sync::mpsc;

pub fn run<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap().block_on(f)
}

fn lo() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// A host and a viewer connected through the production session setup.
pub struct Pair {
    pub host: Connection,
    pub viewer: Connection,
    pub host_link: Option<LinkHandle>,
    pub viewer_link: Option<LinkHandle>,
    _endpoints: (Endpoint, Endpoint),
}

pub async fn plain_pair() -> Pair {
    build(None, 0).await
}

/// Both directions impaired (`ImpairedSocket` on both sockets).
pub async fn impaired_pair(impairment: Impairment, seed: u64) -> Pair {
    build(Some(impairment), seed).await
}

async fn build(impairment: Option<Impairment>, seed: u64) -> Pair {
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
    let (host_ep, viewer_ep, host_link, viewer_link) = match impairment {
        None => {
            let hs = UdpSocket::bind(lo()).unwrap();
            let vs = UdpSocket::bind(lo()).unwrap();
            (
                open_session_endpoint(hs, &host_id, trust.clone()).unwrap(),
                endpoint_from_socket(vs, None).unwrap(),
                None,
                None,
            )
        }
        Some(imp) => {
            let hs = ImpairedSocket::bind(lo(), imp.clone(), seed).unwrap();
            let vs = ImpairedSocket::bind(lo(), imp, seed + 1).unwrap();
            let (hl, vl) = (hs.handle(), vs.handle());
            let server = session_server_config(&host_id, trust.clone()).unwrap();
            (hs.into_endpoint(Some(server)).unwrap(), vs.into_endpoint(None).unwrap(), Some(hl), Some(vl))
        }
    };
    let addr = host_ep.local_addr().unwrap();
    let pin: Arc<dyn PinLookup> = trust;
    let ep = host_ep.clone();
    let accepting =
        tokio::spawn(async move { accept_session(ep.accept().await.expect("an incoming connection"), &*pin).await });
    let viewer = connect_session(&viewer_ep, &viewer_id, addr, *host_id.spki()).await.unwrap();
    let (host, _) = accepting.await.unwrap().unwrap();
    Pair { host, viewer, host_link, viewer_link, _endpoints: (host_ep, viewer_ep) }
}

// ---------- control-stream helpers (the real framing, to carry KeyframeRequest like a session does) ----------

pub async fn write_msg(send: &mut SendStream, msg: impl Into<Message>) {
    send.write_all(&msg.into().encode_frame().unwrap()).await.unwrap();
}

pub async fn read_msg(recv: &mut RecvStream) -> Option<Message> {
    let mut head = Vec::new();
    for _ in 0..2 {
        let mut first = [0u8; 1];
        recv.read_exact(&mut first).await.ok()?;
        head.push(first[0]);
        let rest = (1usize << (first[0] >> 6)) - 1;
        let mut buf = [0u8; 7];
        recv.read_exact(&mut buf[..rest]).await.ok()?;
        head.extend_from_slice(&buf[..rest]);
    }
    let header = FrameHeader::decode(&head).ok()?;
    let mut payload = vec![0u8; header.len];
    recv.read_exact(&mut payload).await.ok()?;
    Message::decode_payload(u64::from(header.type_id), &payload).ok()
}

// ---------- the synthetic encoder ----------

pub const DISPLAY: u16 = 0;

/// Payload bytes that depend on the frame id, so a mix-up or a torn frame is caught.
pub fn payload_of(id: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(31).wrapping_add(id) as u8).collect()
}

pub fn payload_ok(id: u32, payload: &[u8]) -> bool {
    payload.iter().enumerate().all(|(i, b)| *b == (i as u32).wrapping_mul(31).wrapping_add(id) as u8)
}

/// A 1080p60-like encoder: a keyframe of about 200 KB at the start, on request and every `gop` frames, and P-frames
/// of 50 to 100 KB.
pub struct TestEncoder {
    pub next_id: u32,
    since_key: u32,
    started: bool,
    pub gop: u32,
    rng: u64,
    clock: Arc<MonotonicClock>,
}

impl TestEncoder {
    pub fn new(clock: Arc<MonotonicClock>) -> Self {
        Self { next_id: 1, since_key: 0, started: false, gop: 120, rng: 0x9E37_79B9_7F4A_7C15, clock }
    }

    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// The next frame. `sender` says whether the receiver wants a keyframe.
    pub fn next(&mut self, sender: &VideoSender) -> Frame {
        let want = sender.take_keyframe_request(DISPLAY);
        let key = !self.started || want || self.since_key >= self.gop;
        self.started = true;
        self.since_key = if key { 0 } else { self.since_key + 1 };
        let len = if key { 200_000 } else { 50_000 + (self.rand() % 50_000) as usize };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let header = VideoHeader {
            keyframe: key,
            config: key,
            discontinuity: false,
            display_id: DISPLAY,
            frame_id: id,
            capture_us: self.clock.now().0,
            encode_us: 2_000,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            payload_len: 0,
        };
        Frame { header, payload: payload_of(id, len) }
    }

    /// The frame the encoder produced but the test chooses not to send (a frame lost at the host).
    pub fn skip(&mut self, sender: &VideoSender) -> u32 {
        let id = self.next_id;
        let _ = self.next(sender);
        id
    }
}

/// What a decoder would accept: a keyframe, or the successor of the last frame it decoded.
#[derive(Default)]
pub struct DecoderModel {
    pub last_ok: Option<u32>,
    pub decoded: u64,
    pub errors: u64,
}

impl DecoderModel {
    pub fn feed(&mut self, header: &VideoHeader) -> bool {
        self.feed_raw(header.frame_id, header.keyframe)
    }

    pub fn feed_raw(&mut self, frame_id: u32, keyframe: bool) -> bool {
        let ok = keyframe || self.last_ok.is_some_and(|l| frame_id == l.wrapping_add(1));
        if ok {
            self.last_ok = Some(frame_id);
            self.decoded += 1;
        } else {
            self.errors += 1;
        }
        ok
    }
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

pub async fn within<T>(limit: Duration, what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(limit, fut).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

// ---------- a sender and a receiver wired through a control stream ----------

/// The host's `VideoSender` and the viewer's `VideoReceiver` on one connection, with `KeyframeRequest` going back
/// over a real control stream (the framing of PROTOCOL.md 5.1), like a session carries it.
pub struct Rig {
    pub sender: VideoSender,
    pub receiver: VideoReceiver,
    pub frames: mpsc::Receiver<ReceivedFrame>,
    /// Every bitrate the sender told the encoder.
    pub bitrates: Arc<Mutex<Vec<u32>>>,
}

struct Recorder(Arc<Mutex<Vec<u32>>>);

impl BitrateSink for Recorder {
    fn set_target_kbps(&self, kbps: u32) {
        self.0.lock().unwrap().push(kbps);
    }
}

pub async fn rig(pair: &Pair, sender_cfg: SenderConfig, receiver_cfg: ReceiverConfig) -> Rig {
    let bitrates = Arc::new(Mutex::new(Vec::new()));
    let sender = VideoSender::new(pair.host.clone(), sender_cfg, Arc::new(Recorder(bitrates.clone())));

    // Viewer: feedback -> control stream.
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<KeyframeRequest>();
    let (mut csend, _crecv) = pair.viewer.open_bi().await.unwrap();
    csend.set_priority(3).unwrap();
    write_msg(&mut csend, swapdesk_proto::Ping { nonce: 0, t_send_us: 0 }).await;
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            write_msg(&mut csend, req).await;
        }
    });
    let (receiver, frames) = VideoReceiver::new(
        receiver_cfg,
        Arc::new(move |req: KeyframeRequest| {
            let _ = req_tx.send(req);
        }),
    );
    tokio::spawn({
        let receiver = receiver.clone();
        let conn = pair.viewer.clone();
        async move { receiver.serve(conn).await }
    });

    // Host: control stream -> sender.
    let (_hsend, mut hrecv) =
        within(Duration::from_secs(5), "the control stream", pair.host.accept_bi()).await.unwrap();
    tokio::spawn({
        let sender = sender.clone();
        async move {
            while let Some(msg) = read_msg(&mut hrecv).await {
                if let Message::KeyframeRequest(req) = msg {
                    sender.on_keyframe_request(&req);
                }
            }
        }
    });
    Rig { sender, receiver, frames, bitrates }
}
