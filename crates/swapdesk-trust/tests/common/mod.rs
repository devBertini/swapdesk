#![allow(dead_code)]
//! Test harness: nodes with a fake clock, a memory secret store, a scripted screen and a log capture, joined over
//! real loopback UDP through quinn.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use quinn::{RecvStream, SendStream};
use swapdesk_fakes::{FakeClock, LogCapture, MemorySecretStore};
use swapdesk_platform::MonoMicros;
use swapdesk_proto::{ErrorCode, FrameHeader, Message, Os, Sas, Wire};
use swapdesk_trust::pairing::{PairOutcome, PairUi, PairingContext, PeerInfo, Timeouts};
use swapdesk_trust::{
    AuditEvent, ClockWall, Identity, InitiateOptions, LocalInfo, MemoryAudit, ModeOptions, PairingMode, PairingService,
    ServeOptions, TrustHandle, TrustStore, connect_and_initiate, endpoint_from_socket, open_pairing_endpoint,
};
use tokio::sync::{Notify, oneshot};

pub const WALL_ORIGIN_MS: u64 = 1_700_000_000_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a pairing attempt ended with, as the screen was told.
#[derive(Debug, Clone)]
pub struct Ended {
    pub ok: bool,
    pub code: ErrorCode,
    pub outcome: Option<PairOutcome>,
}

/// A screen that answers the code at once (`decision`), or holds the answer for the test.
#[derive(Default)]
pub struct TestUi {
    pub decision: Mutex<Option<bool>>,
    pub codes: Mutex<Vec<Sas>>,
    pub announced: Mutex<Vec<PeerInfo>>,
    pub ended: Mutex<Vec<Ended>>,
    held: Mutex<Vec<oneshot::Sender<bool>>>,
    pub announced_signal: Notify,
    pub ended_signal: Notify,
}

impl TestUi {
    pub fn answering(matches: bool) -> Arc<Self> {
        let ui = Self::default();
        *lock(&ui.decision) = Some(matches);
        Arc::new(ui)
    }

    pub fn holding() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn answer_held(&self, matches: bool) {
        for tx in lock(&self.held).drain(..) {
            let _ = tx.send(matches);
        }
    }

    /// A node that takes part in several attempts: what the last one left behind must not answer for the next.
    pub fn forget_previous_attempt(&self) {
        lock(&self.ended).clear();
        lock(&self.announced).clear();
    }

    pub fn last_code(&self) -> Option<Sas> {
        lock(&self.codes).last().copied()
    }

    pub fn last_ended(&self) -> Option<Ended> {
        lock(&self.ended).last().cloned()
    }

    pub async fn wait_ended(&self) -> Ended {
        for _ in 0..200 {
            if let Some(e) = self.last_ended() {
                return e;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), self.ended_signal.notified()).await;
        }
        panic!("the attempt never ended");
    }

    pub async fn wait_announced(&self) {
        for _ in 0..200 {
            if !lock(&self.announced).is_empty() {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), self.announced_signal.notified()).await;
        }
        panic!("the peer was never announced");
    }
}

impl PairUi for TestUi {
    fn peer_announced(&self, peer: &PeerInfo) {
        lock(&self.announced).push(peer.clone());
        self.announced_signal.notify_waiters();
    }

    fn show_code(&self, sas: Sas) -> oneshot::Receiver<bool> {
        lock(&self.codes).push(sas);
        let (tx, rx) = oneshot::channel();
        match *lock(&self.decision) {
            Some(answer) => {
                let _ = tx.send(answer);
            }
            None => lock(&self.held).push(tx),
        }
        rx
    }

    fn ended(&self, result: &swapdesk_trust::Result<PairOutcome>) {
        let e = match result {
            Ok(o) => Ended { ok: true, code: ErrorCode::NoError, outcome: Some(o.clone()) },
            Err(e) => Ended { ok: false, code: e.code(), outcome: None },
        };
        lock(&self.ended).push(e);
        self.ended_signal.notify_waiters();
    }
}

/// One machine of a test.
pub struct Node {
    pub name: &'static str,
    pub identity: Arc<Identity>,
    pub secrets: Arc<MemorySecretStore>,
    pub trust: Arc<TrustHandle>,
    pub clock: FakeClock,
    pub mode: Arc<PairingMode>,
    pub ui: Arc<TestUi>,
    pub log: LogCapture,
    pub audit: Arc<MemoryAudit>,
    pub ctx: Arc<PairingContext>,
}

impl Node {
    pub fn new(name: &'static str, ui: Arc<TestUi>) -> Self {
        let identity = Arc::new(Identity::generate(WALL_ORIGIN_MS).unwrap());
        Self::build(
            name,
            identity,
            Arc::new(MemorySecretStore::new()),
            FakeClock::new(MonoMicros(0)),
            ui,
            Timeouts::default(),
        )
    }

    pub fn with_timeouts(name: &'static str, ui: Arc<TestUi>, timeouts: Timeouts) -> Self {
        let identity = Arc::new(Identity::generate(WALL_ORIGIN_MS).unwrap());
        Self::build(name, identity, Arc::new(MemorySecretStore::new()), FakeClock::new(MonoMicros(0)), ui, timeouts)
    }

    fn build(
        name: &'static str,
        identity: Arc<Identity>,
        secrets: Arc<MemorySecretStore>,
        clock: FakeClock,
        ui: Arc<TestUi>,
        timeouts: Timeouts,
    ) -> Self {
        let trust = TrustHandle::open(secrets.clone()).unwrap();
        let mode = Arc::new(PairingMode::new(Arc::new(clock.clone())));
        let log = LogCapture::new();
        let audit = Arc::new(MemoryAudit::new());
        let ctx = Arc::new(PairingContext {
            identity: identity.clone(),
            trust: trust.clone(),
            mode: mode.clone(),
            wall: Arc::new(ClockWall::new(Arc::new(clock.clone()), WALL_ORIGIN_MS)),
            ui: ui.clone(),
            trace: Arc::new(log.clone()),
            audit: audit.clone(),
            local: LocalInfo { name: name.to_string(), os: Os::Windows, os_version: "11 24H2".into() },
            timeouts,
        });
        Self { name, identity, secrets, trust, clock, mode, ui, log, audit, ctx }
    }

    /// The same machine after its process restarted: same disk, same key, nothing in memory.
    pub fn restart(&self, ui: Arc<TestUi>) -> Self {
        Self::build(self.name, self.identity.clone(), self.secrets.clone(), self.clock.clone(), ui, self.ctx.timeouts)
    }

    /// The trust store as it is on disk right now.
    pub fn disk(&self) -> TrustStore {
        TrustHandle::open(self.secrets.clone()).unwrap().snapshot()
    }

    pub fn open_pairing_mode(&self) {
        self.mode.enter(ModeOptions::default());
    }

    /// Waits for the attempt that began after `audited_before` audit entries to be over. A ceremony ends its screen
    /// (`ui.ended`); a connection refused before step 2 never owned the slot, tells the screen nothing, and is
    /// known by its audit entry, from which the result is read.
    pub async fn wait_attempt_over(&self, audited_before: usize) -> Ended {
        for _ in 0..200 {
            if let Some(e) = self.ui.last_ended() {
                return e;
            }
            let entries = self.audit.entries();
            if entries.len() > audited_before {
                // Give a ceremony's own end (it comes right after its audit entry) a moment to arrive.
                tokio::time::sleep(Duration::from_millis(150)).await;
                if let Some(e) = self.ui.last_ended() {
                    return e;
                }
                let code = match &entries[audited_before].event {
                    AuditEvent::PairFailed(code) | AuditEvent::PairRefused(code) => *code,
                    AuditEvent::PairingLocked => ErrorCode::PairingLocked,
                    _ => ErrorCode::NoError,
                };
                return Ended { ok: false, code, outcome: None };
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the attempt never ended");
    }
}

pub fn loopback_socket() -> UdpSocket {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// A running pairing endpoint of `node`: its address and the service task.
pub struct Responder {
    pub addr: SocketAddr,
    pub endpoint: quinn::Endpoint,
    pub task: tokio::task::JoinHandle<()>,
    pub service: Arc<PairingService>,
}

pub fn start_responder(node: &Node) -> Responder {
    start_responder_with(node, ServeOptions::default())
}

pub fn start_responder_with(node: &Node, options: ServeOptions) -> Responder {
    let socket = loopback_socket();
    let addr = socket.local_addr().unwrap();
    let endpoint = open_pairing_endpoint(socket, &node.identity).unwrap();
    let service = PairingService::with_options(node.ctx.clone(), options);
    let task = tokio::spawn(Arc::clone(&service).serve(endpoint.clone()));
    Responder { addr, endpoint, task, service }
}

pub fn client_endpoint() -> quinn::Endpoint {
    endpoint_from_socket(loopback_socket(), None).unwrap()
}

/// Both users opened "Parear"; `initiator` connects to `responder`'s pairing endpoint. Returns what each side
/// ended with.
pub async fn pair_once(
    initiator: &Node,
    responder: &Node,
    options: InitiateOptions,
) -> (Result<PairOutcome, swapdesk_trust::TrustError>, Ended) {
    pair_once_with(initiator, responder, options, ModeOptions::default()).await
}

/// Like [`pair_once`], with what the responder's user picked when opening "Parear".
pub async fn pair_once_with(
    initiator: &Node,
    responder: &Node,
    options: InitiateOptions,
    responder_mode: ModeOptions,
) -> (Result<PairOutcome, swapdesk_trust::TrustError>, Ended) {
    responder.ui.forget_previous_attempt();
    initiator.ui.forget_previous_attempt();
    initiator.open_pairing_mode();
    responder.mode.enter(responder_mode);
    let audited_before = responder.audit.entries().len();
    let server = start_responder(responder);
    let client = client_endpoint();
    let a = connect_and_initiate(&client, &initiator.ctx, server.addr, options).await;
    let b = responder.wait_attempt_over(audited_before).await;
    if responder.mode.is_open() {
        // A refusal before step 2 is not a ceremony: the mode stays open and so does the service, until we stop it.
        server.task.abort();
        server.endpoint.close(0u32.into(), b"test over");
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(3), server.task).await;
    }
    (a, b)
}

pub fn run<F: Future>(f: F) -> F::Output {
    runtime().block_on(f)
}

// ---------- a hand-written initiator, to check the wire independently of the library's own ----------

pub async fn write_msg(send: &mut SendStream, msg: impl Into<Message>) {
    send.write_all(&msg.into().encode_frame().unwrap()).await.unwrap();
}

pub async fn read_msg(recv: &mut RecvStream) -> Message {
    let mut head = Vec::new();
    for _ in 0..2 {
        let mut first = [0u8; 1];
        recv.read_exact(&mut first).await.unwrap();
        head.push(first[0]);
        let rest = (1usize << (first[0] >> 6)) - 1;
        let mut buf = [0u8; 7];
        recv.read_exact(&mut buf[..rest]).await.unwrap();
        head.extend_from_slice(&buf[..rest]);
    }
    let h = FrameHeader::decode(&head).unwrap();
    let mut payload = vec![0u8; h.len];
    recv.read_exact(&mut payload).await.unwrap();
    Message::decode_payload(u64::from(h.type_id), &payload).unwrap()
}
