#![allow(dead_code)]
//! Test harness for the session flows: a host with a scripted UI and a viewer, joined either by an in-memory
//! control link (a paused clock, exact timings) or by real QUIC over loopback through `swapdesk-trust` and
//! `ImpairedSocket` (the wire, the gate, real loss).

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use quinn::Endpoint;
use swapdesk_fakes::{FakeClock, MemorySecretStore};
use swapdesk_netsim::{ImpairedSocket, Impairment, LinkHandle};
use swapdesk_platform::MonoMicros;
use swapdesk_proto::{Capabilities, ErrorCode, KbKind, KeyboardInfo, Message, Os, PermissionSet, StreamKind};
use swapdesk_session::{
    ChannelHandler, ConsentRequest, ControlIo, HostConfig, HostEvent, HostService, HostUi, IncomingStream,
    PromptAnswer, PromptEnd, PromptRequest, Registry, SessionCtx, SessionEnd, StartInfo, ViewerConfig, memory_pair,
};
use swapdesk_trust::tls::session_server_config;
use swapdesk_trust::{
    DeviceId, Identity, PairedDevice, TrustHandle, UserRef, device_id, endpoint_from_socket, open_session_endpoint,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A multi-thread runtime for the tests that use real sockets.
pub fn run<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap().block_on(f)
}

/// A current-thread runtime with a paused clock: sleeps cost nothing and are exact. Only for in-memory links.
pub fn run_paused<F: Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
    rt.block_on(f)
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        video_decode: vec![],
        audio: vec![],
        max_datagram: 1200,
        keyboard: KeyboardInfo { kind: KbKind::Ansi, layout: "us".into() },
        features: 0,
    }
}

pub async fn eventually(limit: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let t0 = tokio::time::Instant::now();
    while !cond() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------- the host's UI, scripted ----------

#[derive(Clone, Copy)]
pub enum Script {
    Answer(PromptAnswer),
    /// The card stays open: the test answers it with `answer_held`, or never.
    Hold,
}

pub struct TestUi {
    pub present: AtomicBool,
    pub script: Mutex<Script>,
    pub consent_script: Mutex<Option<bool>>,
    pub prompts: Mutex<Vec<PromptRequest>>,
    held: Mutex<Vec<oneshot::Sender<PromptAnswer>>>,
    held_consent: Mutex<Vec<oneshot::Sender<bool>>>,
    pub closed: Mutex<Vec<(u64, PromptEnd)>>,
    pub consents: Mutex<Vec<ConsentRequest>>,
    pub events: Mutex<Vec<HostEvent>>,
    pub lock_on_end: AtomicBool,
    pub locks: AtomicUsize,
    pub info: Mutex<StartInfo>,
    /// A faulty UI: `prompt` panics.
    pub panic_on_prompt: AtomicBool,
    /// A slow UI: `event(SessionStarted)` blocks its caller this long.
    pub event_delay: Mutex<Option<Duration>>,
    /// How each consent card was closed.
    pub consents_closed: Mutex<Vec<PromptEnd>>,
}

impl TestUi {
    pub fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            present: AtomicBool::new(true),
            script: Mutex::new(script),
            consent_script: Mutex::new(Some(true)),
            prompts: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
            held_consent: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            consents: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            lock_on_end: AtomicBool::new(false),
            locks: AtomicUsize::new(0),
            info: Mutex::new(StartInfo::default()),
            panic_on_prompt: AtomicBool::new(false),
            event_delay: Mutex::new(None),
            consents_closed: Mutex::new(Vec::new()),
        })
    }

    pub fn accepting(grants: PermissionSet) -> Arc<Self> {
        Self::new(Script::Answer(PromptAnswer::Accept(grants)))
    }

    pub fn set_present(&self, present: bool) {
        self.present.store(present, Ordering::SeqCst);
    }

    pub fn prompt_count(&self) -> usize {
        lock(&self.prompts).len()
    }

    pub fn answer_held(&self, answer: PromptAnswer) {
        for tx in lock(&self.held).drain(..) {
            let _ = tx.send(answer);
        }
    }

    /// Consent: `Some(answer)` answers at once, `None` holds the question.
    pub fn answer_consent_with(&self, answer: Option<bool>) {
        *lock(&self.consent_script) = answer;
    }

    pub fn events(&self) -> Vec<HostEvent> {
        lock(&self.events).clone()
    }

    pub fn find_event<T>(&self, f: impl Fn(&HostEvent) -> Option<T>) -> Option<T> {
        lock(&self.events).iter().find_map(f)
    }

    pub fn set_lock_on_end(&self, on: bool) {
        self.lock_on_end.store(on, Ordering::SeqCst);
    }
}

impl HostUi for TestUi {
    fn somebody_present(&self) -> bool {
        self.present.load(Ordering::SeqCst)
    }

    fn prompt(&self, request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
        assert!(!self.panic_on_prompt.load(Ordering::SeqCst), "the host UI is faulty (this panic is the test)");
        lock(&self.prompts).push(request);
        let (tx, rx) = oneshot::channel();
        match *lock(&self.script) {
            Script::Answer(a) => {
                let _ = tx.send(a);
            }
            Script::Hold => lock(&self.held).push(tx),
        }
        rx
    }

    fn prompt_closed(&self, request_id: u64, how: PromptEnd) {
        lock(&self.closed).push((request_id, how));
    }

    fn consent(&self, request: ConsentRequest) -> oneshot::Receiver<bool> {
        lock(&self.consents).push(request);
        let (tx, rx) = oneshot::channel();
        match *lock(&self.consent_script) {
            Some(a) => {
                let _ = tx.send(a);
            }
            None => lock(&self.held_consent).push(tx),
        }
        rx
    }

    fn lock_on_end(&self) -> bool {
        self.lock_on_end.load(Ordering::SeqCst)
    }

    fn lock_screen(&self) {
        self.locks.fetch_add(1, Ordering::SeqCst);
    }

    fn start_info(&self) -> StartInfo {
        lock(&self.info).clone()
    }

    fn event(&self, event: HostEvent) {
        if matches!(event, HostEvent::SessionStarted { .. }) {
            let delay = *lock(&self.event_delay);
            if let Some(d) = delay {
                // A UI call that blocks its caller (it runs on the session's task).
                std::thread::sleep(d);
            }
        }
        lock(&self.events).push(event);
    }

    fn consent_closed(&self, _request: &ConsentRequest, how: PromptEnd) {
        lock(&self.consents_closed).push(how);
    }
}

// ---------- a feature handler that records what it was given ----------

pub struct Recorder {
    pub name: &'static str,
    pub types: &'static [u16],
    pub kinds: &'static [StreamKind],
    pub got: Mutex<Vec<Message>>,
    pub streams: AtomicUsize,
    pub perms: Mutex<Vec<(PermissionSet, PermissionSet)>>,
    pub states: Mutex<Vec<(swapdesk_proto::Desktop, bool)>>,
    pub started: AtomicUsize,
    pub ended: Mutex<Vec<SessionEnd>>,
    /// Ticks of the task this handler runs while `TERMINAL` is granted (the stand-in for a terminal relay).
    pub ticks: Arc<AtomicUsize>,
    pub sessions: Mutex<Vec<SessionCtx>>,
    /// The order of `on_start`, `on_stream` and `on_end` calls.
    pub order: Mutex<Vec<&'static str>>,
}

impl Recorder {
    pub fn new(types: &'static [u16], kinds: &'static [StreamKind]) -> Arc<Self> {
        Arc::new(Self {
            name: "recorder",
            types,
            kinds,
            got: Mutex::new(Vec::new()),
            streams: AtomicUsize::new(0),
            perms: Mutex::new(Vec::new()),
            states: Mutex::new(Vec::new()),
            started: AtomicUsize::new(0),
            ended: Mutex::new(Vec::new()),
            ticks: Arc::new(AtomicUsize::new(0)),
            sessions: Mutex::new(Vec::new()),
            order: Mutex::new(Vec::new()),
        })
    }

    pub fn messages(&self) -> Vec<Message> {
        lock(&self.got).clone()
    }

    pub fn count(&self) -> usize {
        lock(&self.got).len()
    }
}

impl ChannelHandler for Recorder {
    fn name(&self) -> &'static str {
        self.name
    }

    fn messages(&self) -> &'static [u16] {
        self.types
    }

    fn streams(&self) -> &'static [StreamKind] {
        self.kinds
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        // A terminal-like feature: opening one starts a task that needs `TERMINAL` and ticks until revoked.
        if let Message::TermOpen(_) = &msg {
            let ticks = Arc::clone(&self.ticks);
            ctx.spawn_gated(PermissionSet::TERMINAL, async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
        }
        lock(&self.got).push(msg);
    }

    fn on_stream(&self, _ctx: &SessionCtx, stream: IncomingStream) {
        lock(&self.order).push("stream");
        self.streams.fetch_add(1, Ordering::SeqCst);
        drop(stream);
    }

    fn on_permissions(&self, _ctx: &SessionCtx, old: PermissionSet, now: PermissionSet) {
        lock(&self.perms).push((old, now));
    }

    fn on_state(&self, _ctx: &SessionCtx, desktop: swapdesk_proto::Desktop, paused: bool) {
        lock(&self.states).push((desktop, paused));
    }

    fn on_start(&self, ctx: &SessionCtx) {
        lock(&self.order).push("start");
        self.started.fetch_add(1, Ordering::SeqCst);
        lock(&self.sessions).push(ctx.clone());
    }

    fn on_end(&self, _ctx: &SessionCtx, end: &SessionEnd) {
        lock(&self.order).push("end");
        lock(&self.ended).push(*end);
    }
}

pub fn registry_one<H: ChannelHandler>(handler: Arc<H>) -> Registry {
    let mut r = Registry::new();
    r.register(handler).unwrap();
    r
}

pub fn registry_of(handlers: &[Arc<dyn ChannelHandler>]) -> Registry {
    let mut r = Registry::new();
    for h in handlers {
        r.register(Arc::clone(h)).unwrap();
    }
    r
}

// ---------- devices ----------

/// A viewer machine: its key and its device ID.
pub struct Who {
    pub identity: Arc<Identity>,
    pub id: DeviceId,
}

impl Who {
    pub fn new() -> Self {
        let identity = Arc::new(Identity::generate(0).unwrap());
        let id = device_id(identity.spki());
        Self { identity, id }
    }
}

/// What the host stores about a device.
pub fn pin(trust: &TrustHandle, who: &Who, grants_here: PermissionSet) {
    trust
        .update(|s| {
            let mut d =
                PairedDevice::new_paired(*who.identity.spki(), "viewer".into(), Os::MacOs, "15".into(), 1, None);
            d.grants_here = grants_here;
            if grants_here.contains(PermissionSet::UNATTENDED) {
                d.unattended_granted_by = Some(UserRef { id: "S-1-5-21".into(), name: "admin".into() });
            }
            s.devices.push(d);
        })
        .unwrap();
}

pub fn set_stored(trust: &TrustHandle, who: &Who, grants_here: PermissionSet) {
    trust
        .update(|s| s.devices.iter_mut().filter(|d| d.device_id == who.id).for_each(|d| d.grants_here = grants_here))
        .unwrap();
}

pub fn host_config() -> HostConfig {
    HostConfig::new("host-pc", Os::Windows, "11 24H2", "0.1.0", capabilities())
}

pub fn viewer_config(requested: PermissionSet) -> ViewerConfig {
    let mut cfg = ViewerConfig::new(
        "viewer-mac",
        Os::MacOs,
        "15.1",
        "0.1.0",
        capabilities(),
        Arc::new(FakeClock::new(MonoMicros(1))),
    );
    cfg.requested = requested;
    cfg
}

/// As [`viewer_config`], for a viewer of 1.0: its `Hello` says protocol minor 0.
pub fn viewer_config_1_0(requested: PermissionSet) -> ViewerConfig {
    let mut cfg = viewer_config(requested);
    cfg.proto_minor = 0;
    cfg
}

// ---------- host and viewer over memory ----------

pub struct Mem {
    pub host: HostService,
    pub ui: Arc<TestUi>,
    pub trust: Arc<TrustHandle>,
}

impl Mem {
    pub fn new(ui: Arc<TestUi>, registry: Registry) -> Self {
        Self::with(ui, registry, |_| {})
    }

    pub fn with(ui: Arc<TestUi>, registry: Registry, tweak: impl FnOnce(&mut HostConfig)) -> Self {
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let mut cfg = host_config();
        tweak(&mut cfg);
        let host = HostService::new(cfg, trust.clone(), ui.clone(), registry, Arc::new(FakeClock::new(MonoMicros(1))));
        Self { host, ui, trust }
    }

    /// The host's side of a new connection from `who`, and the viewer's raw end of the control link.
    pub fn connect_raw(&self, who: &Who) -> (ControlIo, JoinHandle<SessionEnd>) {
        let (host_io, viewer_io) = memory_pair();
        let host = self.host.clone();
        let id = who.id;
        (viewer_io, tokio::spawn(async move { host.serve_io(host_io, None, id).await }))
    }

    /// A viewer session over memory. Returns the open result and the host task.
    pub async fn open(
        &self,
        who: &Who,
        cfg: ViewerConfig,
    ) -> (Result<swapdesk_session::ViewerSession, swapdesk_session::OpenError>, JoinHandle<SessionEnd>) {
        let (host_io, viewer_io) = memory_pair();
        let host = self.host.clone();
        let id = who.id;
        let task = tokio::spawn(async move { host.serve_io(host_io, None, id).await });
        let session = swapdesk_session::ViewerSession::open_io(viewer_io, None, Some(device_id_of_host()), cfg).await;
        (session, task)
    }
}

fn device_id_of_host() -> DeviceId {
    [9; 16]
}

pub async fn next_msg(io: &mut ControlIo, what: &str) -> Message {
    match tokio::time::timeout(Duration::from_secs(30), io.rx.recv()).await {
        Ok(Some(Ok(m))) => m,
        Ok(other) => panic!("waiting for {what}: the link gave {other:?}"),
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// Reads until a message of the wanted shape, skipping `Ping`, `Pong` and what the closure rejects.
pub async fn next_matching<T>(io: &mut ControlIo, what: &str, mut f: impl FnMut(&Message) -> Option<T>) -> T {
    loop {
        let m = next_msg(io, what).await;
        if let Some(v) = f(&m) {
            return v;
        }
    }
}

/// The raw viewer says hello and reads until the decision; returns the decision message.
pub async fn raw_hello(
    io: &mut ControlIo,
    requested: PermissionSet,
    resume: Option<swapdesk_proto::Resume>,
    swap_token: Option<[u8; 16]>,
) {
    io.out
        .send(swapdesk_proto::Hello {
            proto_minor: 0,
            app_version: "t".into(),
            name: "raw".into(),
            os: Os::MacOs,
            os_version: "15".into(),
            intent: swapdesk_proto::Intent::Control,
            requested,
            resume,
            swap_token,
        })
        .await
        .unwrap();
    io.out.send(capabilities()).await.unwrap();
}

// ---------- host and viewer over real QUIC ----------

pub fn lo() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

pub struct Quic {
    pub host: HostService,
    pub ui: Arc<TestUi>,
    pub trust: Arc<TrustHandle>,
    pub host_identity: Arc<Identity>,
    pub addr: SocketAddr,
    pub host_link: Option<LinkHandle>,
    pub endpoint: Endpoint,
    task: JoinHandle<()>,
}

impl Quic {
    pub async fn start(
        ui: Arc<TestUi>,
        registry: Registry,
        impairment: Option<Impairment>,
        tweak: impl FnOnce(&mut HostConfig),
    ) -> Self {
        let host_identity = Arc::new(Identity::generate(0).unwrap());
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let mut cfg = host_config();
        tweak(&mut cfg);
        let host =
            HostService::new(cfg, trust.clone(), ui.clone(), registry, Arc::new(swapdesk_fakes::MonotonicClock::new()));
        let (endpoint, host_link) = match impairment {
            None => {
                (open_session_endpoint(UdpSocket::bind(lo()).unwrap(), &host_identity, trust.clone()).unwrap(), None)
            }
            Some(imp) => {
                let sock = ImpairedSocket::bind(lo(), imp, 11).unwrap();
                let link = sock.handle();
                (
                    sock.into_endpoint(Some(session_server_config(&host_identity, trust.clone()).unwrap())).unwrap(),
                    Some(link),
                )
            }
        };
        let addr = endpoint.local_addr().unwrap();
        let task = tokio::spawn({
            let (host, trust, endpoint) = (host.clone(), trust.clone(), endpoint.clone());
            serve_endpoint(host, trust, endpoint)
        });
        Self { host, ui, trust, host_identity, addr, host_link, endpoint, task }
    }

    /// A viewer machine paired with this host, holding `grants_here` on it.
    pub fn viewer(&self, grants_here: PermissionSet, impairment: Option<Impairment>) -> QuicViewer {
        let who = Who::new();
        pin(&self.trust, &who, grants_here);
        let (endpoint, link) = match impairment {
            None => (endpoint_from_socket(UdpSocket::bind(lo()).unwrap(), None).unwrap(), None),
            Some(imp) => {
                let sock = ImpairedSocket::bind(lo(), imp, 21).unwrap();
                let link = sock.handle();
                (sock.into_endpoint(None).unwrap(), Some(link))
            }
        };
        QuicViewer { who, endpoint, link }
    }

    pub async fn open(
        &self,
        v: &QuicViewer,
        cfg: ViewerConfig,
    ) -> Result<swapdesk_session::ViewerSession, swapdesk_session::OpenError> {
        swapdesk_session::ViewerSession::connect(
            &v.endpoint,
            &v.who.identity,
            self.addr,
            *self.host_identity.spki(),
            cfg,
        )
        .await
    }

    pub fn stop(&self) {
        self.task.abort();
    }
}

pub struct QuicViewer {
    pub who: Who,
    pub endpoint: Endpoint,
    pub link: Option<LinkHandle>,
}

/// Waits for the first event of a kind from the viewer's event stream, within `limit`.
pub async fn wait_event(
    s: &mut swapdesk_session::ViewerSession,
    limit: Duration,
    what: &str,
    mut f: impl FnMut(&swapdesk_session::ViewerEvent) -> bool,
) -> swapdesk_session::ViewerEvent {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match tokio::time::timeout_at(deadline, s.next_event()).await {
            Ok(Some(e)) if f(&e) => return e,
            Ok(Some(_)) => {}
            Ok(None) => panic!("the session ended while waiting for {what}: {:?}", s.ctx().ended()),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

pub fn assert_refused(r: Result<swapdesk_session::ViewerSession, swapdesk_session::OpenError>, code: ErrorCode) {
    match r {
        Err(swapdesk_session::OpenError::Refused(c)) => assert_eq!(c, code),
        Err(e) => panic!("expected a refusal with {code:?}, got {e:?}"),
        Ok(_) => panic!("expected a refusal with {code:?}, but the session opened"),
    }
}

pub fn elapsed_since(t: Instant) -> Duration {
    t.elapsed()
}

/// What the service's accept loop does with the connections of a test endpoint: the handshake and the gate, then one
/// task per session. It has no LAN guard and no limit (the service's own loop has both), so it stays in the tests.
pub async fn serve_endpoint(host: HostService, trust: Arc<TrustHandle>, endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let (host, trust) = (host.clone(), Arc::clone(&trust));
        tokio::spawn(async move {
            if let Ok((conn, device)) = swapdesk_trust::accept_session(incoming, &*trust).await {
                host.serve(conn, device).await;
            }
        });
    }
}
