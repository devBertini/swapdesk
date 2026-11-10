//! Flow proofs of swap (PROTOCOL.md 6.3, F-SWAP-01, F-SWAP-02). The rules and their timings run over in-memory
//! links with a paused clock (20 s and 22 s cost nothing); the swap itself, both ways, runs over real QUIC on loopback
//! through `swapdesk-trust` and `HostService`, and is timed against B4.

mod common;

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::*;
use quinn::Endpoint;
use swapdesk_fakes::MonotonicClock;
use swapdesk_proto::{ErrorCode, GrantMode, Message, PermissionSet};
use swapdesk_session::{EndKind, HostService, SessionCtx, ViewerSession};
use swapdesk_swap::{AskEnd, SWAP_TYPES, SwapEnv, SwapHandler, SwapOutcome, SwapPrompt, SwapTimings};
use swapdesk_trust::{DeviceId, TrustHandle, endpoint_from_socket, open_session_endpoint};
use tokio::sync::oneshot;
use tokio::time::Instant;

const CV: PermissionSet = PermissionSet::CONTROL;

/// How the user answers the card.
#[derive(Clone, Copy)]
enum Answer {
    Yes,
    No,
    /// The card stays open.
    Hold,
}

type Connector = Box<dyn Fn(DeviceId, [u8; 16]) + Send + Sync>;

/// A machine's side of the swap, scripted.
struct Env {
    present: AtomicBool,
    /// Used when there is no trust store.
    control: AtomicBool,
    trust: Option<Arc<TrustHandle>>,
    answer: Mutex<Answer>,
    prompts: Mutex<Vec<SwapPrompt>>,
    closed: Mutex<Vec<(u32, AskEnd)>>,
    armed: Mutex<Vec<(DeviceId, [u8; 16])>>,
    viewing: Mutex<Vec<(DeviceId, [u8; 16])>>,
    held: Mutex<Vec<oneshot::Sender<bool>>>,
    host: OnceLock<HostService>,
    connector: Mutex<Option<Connector>>,
}

impl Env {
    fn new(answer: Answer, control: bool) -> Arc<Self> {
        Arc::new(Self {
            present: AtomicBool::new(true),
            control: AtomicBool::new(control),
            trust: None,
            answer: Mutex::new(answer),
            prompts: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            armed: Mutex::new(Vec::new()),
            viewing: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
            host: OnceLock::new(),
            connector: Mutex::new(None),
        })
    }

    fn with_trust(answer: Answer, trust: Arc<TrustHandle>) -> Arc<Self> {
        let e = Self::new(answer, false);
        Arc::new(Self { trust: Some(trust), ..Arc::into_inner(e).unwrap() })
    }
}

impl SwapEnv for Env {
    fn somebody_present(&self) -> bool {
        self.present.load(Ordering::SeqCst)
    }

    fn control_granted_to(&self, peer: &DeviceId) -> bool {
        match &self.trust {
            Some(t) => t.read(|s| s.find_by_id(peer).is_some_and(|d| d.grants_here.contains(PermissionSet::CONTROL))),
            None => self.control.load(Ordering::SeqCst),
        }
    }

    fn ask(&self, prompt: SwapPrompt) -> oneshot::Receiver<bool> {
        lock(&self.prompts).push(prompt);
        let (tx, rx) = oneshot::channel();
        match *lock(&self.answer) {
            Answer::Yes => {
                let _ = tx.send(true);
            }
            Answer::No => {
                let _ = tx.send(false);
            }
            Answer::Hold => lock(&self.held).push(tx),
        }
        rx
    }

    fn ask_closed(&self, request_id: u32, how: AskEnd) {
        lock(&self.closed).push((request_id, how));
    }

    fn arm_token(&self, device: DeviceId, token: [u8; 16]) {
        lock(&self.armed).push((device, token));
        if let Some(h) = self.host.get() {
            h.arm_swap_token(device, token);
        }
    }

    fn begin_viewing(&self, host: DeviceId, token: [u8; 16]) {
        lock(&self.viewing).push((host, token));
        if let Some(c) = lock(&self.connector).as_ref() {
            c(host, token);
        }
    }
}

// ---------- in-memory pair: a viewer machine and a host machine over one control link ----------

struct Pair {
    viewer: ViewerSession,
    host_ctx: SessionCtx,
    host_task: tokio::task::JoinHandle<swapdesk_session::SessionEnd>,
    venv: Arc<Env>,
    henv: Arc<Env>,
    vh: SwapHandler,
    hh: SwapHandler,
    recorder: Option<Arc<Recorder>>,
}

struct Setup {
    /// The viewer's user has granted CONTROL to the host machine (the viewer would be the future host).
    viewer_grants: bool,
    /// The host machine's user has granted CONTROL to the viewer machine.
    host_grants: bool,
    host_answer: Answer,
    viewer_answer: Answer,
    /// The host machine runs a recorder instead of the swap handler (a peer that never answers).
    silent_host: bool,
    timings: SwapTimings,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            viewer_grants: true,
            host_grants: true,
            host_answer: Answer::Yes,
            viewer_answer: Answer::Yes,
            silent_host: false,
            timings: SwapTimings::default(),
        }
    }
}

async fn pair(s: Setup) -> Pair {
    let venv = Env::new(s.viewer_answer, s.viewer_grants);
    let henv = Env::new(s.host_answer, s.host_grants);
    let vh = SwapHandler::with_timings(venv.clone(), s.timings);
    let hh = SwapHandler::with_timings(henv.clone(), s.timings);
    let recorder = s.silent_host.then(|| Recorder::new(SWAP_TYPES, &[]));
    let host_registry = match &recorder {
        Some(r) => registry_one(r.clone()),
        None => registry_of(&[hh.handler()]),
    };
    let mem = Mem::new(TestUi::accepting(PermissionSet::VIEW | CV), host_registry);
    let who = Who::new();
    pin(&mem.trust, &who, PermissionSet::VIEW | CV);
    let mut cfg = viewer_config(CV);
    cfg.registry = registry_of(&[vh.handler()]);
    let (session, host_task) = mem.open(&who, cfg).await;
    let viewer = session.expect("the session opens");
    let host_ctx = mem.host.session(viewer.grant().session_id).expect("the host knows the session");
    std::mem::forget(mem);
    Pair { viewer, host_ctx, host_task, venv, henv, vh, hh, recorder }
}

impl Pair {
    fn viewer_unchanged(&self) -> bool {
        self.viewer.ctx().is_active() && self.host_ctx.is_active()
    }

    /// Both sides were closed with `Bye { SWAPPED }`.
    async fn assert_closed_swapped(self) {
        let end = tokio::time::timeout(Duration::from_secs(5), self.viewer.ctx().wait_ended())
            .await
            .expect("the viewer's session ends");
        assert_eq!(end.code, ErrorCode::Swapped);
        let host_end = tokio::time::timeout(Duration::from_secs(5), self.host_task)
            .await
            .expect("the host's session ends")
            .unwrap();
        assert_eq!(host_end.code, ErrorCode::Swapped);
        assert!(matches!(host_end.kind, EndKind::LocalBye | EndKind::PeerBye));
    }
}

fn tokens_match(armed: &[(DeviceId, [u8; 16])], viewing: &[(DeviceId, [u8; 16])]) -> [u8; 16] {
    assert_eq!(armed.len(), 1, "the future host armed exactly one token");
    assert_eq!(viewing.len(), 1, "the future viewer opened exactly one new connection");
    assert_eq!(armed[0].1, viewing[0].1, "the token that was armed is the token that connects");
    assert_ne!(armed[0].1, [0; 16], "the token is random bytes");
    armed[0].1
}

// ---------- F-SWAP-01: accepted ----------

#[test]
fn accept_when_the_viewer_asks_arms_the_viewer_and_the_host_opens_the_new_connection() {
    run_paused(async {
        let p = pair(Setup::default()).await;
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::Swapped);
        eventually(Duration::from_secs(5), "the host machine to open the new connection", || {
            !lock(&p.henv.viewing).is_empty()
        })
        .await;
        // The future host (the asking viewer) armed the token for the host machine, which is the future viewer.
        tokens_match(&lock(&p.venv.armed), &lock(&p.henv.viewing));
        assert_eq!(lock(&p.venv.armed)[0].0, p.viewer.ctx().device().unwrap(), "armed for the other device");
        assert!(lock(&p.henv.armed).is_empty() && lock(&p.venv.viewing).is_empty());
        let prompt = lock(&p.henv.prompts)[0];
        assert!(!prompt.asked_becomes_host, "the asked machine becomes the viewer");
        assert_eq!(prompt.timeout, Duration::from_secs(20));
        p.assert_closed_swapped().await;
    });
}

#[test]
fn accept_when_the_host_asks_arms_the_host_and_the_viewer_machine_gives_way() {
    run_paused(async {
        let p = pair(Setup::default()).await;
        assert_eq!(
            p.hh.request(&p.host_ctx).await,
            SwapOutcome::Swapped,
            "the host asked the viewer, who is to become the host"
        );
        eventually(Duration::from_secs(5), "the host machine to open the new connection", || {
            !lock(&p.henv.viewing).is_empty()
        })
        .await;
        // The future host is B, the viewer machine: it armed at once and said so. The host machine is the future viewer.
        tokens_match(&lock(&p.venv.armed), &lock(&p.henv.viewing));
        assert!(lock(&p.henv.armed).is_empty() && lock(&p.venv.viewing).is_empty());
        assert!(lock(&p.venv.prompts)[0].asked_becomes_host, "the viewer is asked whether it takes over as host");
        p.assert_closed_swapped().await;
    });
}

// ---------- F-SWAP-02: declined, timed out, nobody present, not permitted ----------

#[test]
fn decline_leaves_the_session_unchanged_and_says_declined() {
    run_paused(async {
        let p = pair(Setup { host_answer: Answer::No, ..Setup::default() }).await;
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::Declined(ErrorCode::Declined));
        assert!(p.viewer_unchanged());
        assert!(lock(&p.venv.armed).is_empty() && lock(&p.henv.armed).is_empty() && lock(&p.henv.viewing).is_empty());
        assert_eq!(lock(&p.henv.closed)[0].1, AskEnd::Answered);
        assert!(!p.vh.pending(p.viewer.ctx()), "a new request may follow");
    });
}

#[test]
fn twenty_seconds_without_an_answer_decline_with_timeout() {
    run_paused(async {
        let p = pair(Setup { host_answer: Answer::Hold, ..Setup::default() }).await;
        let t0 = Instant::now();
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::Declined(ErrorCode::Timeout));
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_secs(20) && took < Duration::from_secs(22),
            "declined by the asked side at 20 s, before the asker gives up: {took:?}"
        );
        assert_eq!(lock(&p.henv.closed)[0].1, AskEnd::TimedOut, "the card closes");
        assert!(p.viewer_unchanged());
        assert!(lock(&p.venv.armed).is_empty() && lock(&p.henv.viewing).is_empty());
    });
}

#[test]
fn the_asker_gives_up_at_22_seconds_and_cancels() {
    run_paused(async {
        let p = pair(Setup { silent_host: true, ..Setup::default() }).await;
        let t0 = Instant::now();
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::GaveUp);
        assert!(t0.elapsed() >= Duration::from_secs(22) && t0.elapsed() < Duration::from_secs(23));
        let rec = p.recorder.clone().unwrap();
        eventually(Duration::from_secs(5), "the cancel to arrive", || rec.count() == 2).await;
        assert!(matches!(rec.messages()[0], Message::SwapRequest(_)));
        assert!(matches!(rec.messages()[1], Message::SwapCancel(_)));
        assert!(p.viewer_unchanged());
        assert!(!p.vh.pending(p.viewer.ctx()));
    });
}

#[test]
fn a_cancel_closes_the_card_of_the_asked_user() {
    run_paused(async {
        let timings = SwapTimings { give_up: Duration::from_secs(5), ..SwapTimings::default() };
        let p = pair(Setup { host_answer: Answer::Hold, timings, ..Setup::default() }).await;
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::GaveUp);
        eventually(Duration::from_secs(5), "the card to close", || !lock(&p.henv.closed).is_empty()).await;
        assert_eq!(lock(&p.henv.closed)[0].1, AskEnd::Cancelled);
        assert!(!p.hh.pending(&p.host_ctx));
        assert!(p.viewer_unchanged());
    });
}

#[test]
fn nobody_present_at_the_asked_side_is_refused_at_once_without_a_prompt() {
    run_paused(async {
        let p = pair(Setup::default()).await;
        p.henv.present.store(false, Ordering::SeqCst);
        let t0 = Instant::now();
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::Declined(ErrorCode::NobodyPresent));
        assert!(t0.elapsed() < Duration::from_secs(1), "at once");
        assert!(lock(&p.henv.prompts).is_empty(), "no prompt");
        assert!(p.viewer_unchanged());

        // The same the other way round.
        p.venv.present.store(false, Ordering::SeqCst);
        assert_eq!(p.hh.request(&p.host_ctx).await, SwapOutcome::Declined(ErrorCode::NobodyPresent));
        assert!(lock(&p.venv.prompts).is_empty());
    });
}

#[test]
fn a_future_host_that_never_granted_control_cannot_ask_and_nothing_is_sent() {
    run_paused(async {
        let p = pair(Setup { viewer_grants: false, silent_host: true, ..Setup::default() }).await;
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::Declined(ErrorCode::NotPermitted));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(p.recorder.clone().unwrap().count(), 0, "the check is local: no SwapRequest left");
        assert!(p.viewer_unchanged());
    });
}

#[test]
fn a_future_host_that_never_granted_control_declines_at_once_without_a_prompt() {
    run_paused(async {
        let p = pair(Setup { viewer_grants: false, ..Setup::default() }).await;
        // The host asks; the viewer would become the host but has not granted CONTROL to the host machine.
        let t0 = Instant::now();
        assert_eq!(p.hh.request(&p.host_ctx).await, SwapOutcome::Declined(ErrorCode::NotPermitted));
        assert!(t0.elapsed() < Duration::from_secs(1), "at once");
        assert!(lock(&p.venv.prompts).is_empty(), "no prompt");
        assert!(p.viewer_unchanged());
    });
}

#[test]
fn control_revoked_while_the_card_is_open_declines_not_permitted() {
    run_paused(async {
        let p = pair(Setup { viewer_answer: Answer::Hold, ..Setup::default() }).await;
        let hh = p.hh.clone();
        let ctx = p.host_ctx.clone();
        let asking = tokio::spawn(async move { hh.request(&ctx).await });
        eventually(Duration::from_secs(5), "the card", || !lock(&p.venv.prompts).is_empty()).await;
        p.venv.control.store(false, Ordering::SeqCst);
        for tx in lock(&p.venv.held).drain(..) {
            let _ = tx.send(true);
        }
        assert_eq!(asking.await.unwrap(), SwapOutcome::Declined(ErrorCode::NotPermitted));
        assert!(lock(&p.venv.armed).is_empty());
        assert!(p.viewer_unchanged());
    });
}

#[test]
fn one_swap_at_a_time() {
    run_paused(async {
        let p = pair(Setup { host_answer: Answer::Hold, ..Setup::default() }).await;
        let (vh, ctx) = (p.vh.clone(), p.viewer.ctx().clone());
        let first = tokio::spawn(async move { vh.request(&ctx).await });
        eventually(Duration::from_secs(5), "the first request", || p.vh.pending(p.viewer.ctx())).await;
        assert_eq!(p.vh.request(p.viewer.ctx()).await, SwapOutcome::Declined(ErrorCode::Busy));
        // The other side refuses a second request on the same session too.
        assert_eq!(first.await.unwrap(), SwapOutcome::Declined(ErrorCode::Timeout));
    });
}

// ---------- F-SWAP-01: the swap itself over real QUIC, both ways, under B4 ----------

struct Machine {
    who: Who,
    trust: Arc<TrustHandle>,
    env: Arc<Env>,
    handler: SwapHandler,
    host: HostService,
    addr: std::net::SocketAddr,
    client: Endpoint,
    /// The session this machine has open as a viewer, and when each swap connection was granted.
    viewing: Arc<Mutex<Option<ViewerSession>>>,
    results: Arc<Mutex<Vec<(Instant, GrantMode)>>>,
    _server: tokio::task::JoinHandle<()>,
}

impl Machine {
    fn start() -> Self {
        let who = Who::new();
        let trust = TrustHandle::open(Arc::new(swapdesk_fakes::MemorySecretStore::new())).unwrap();
        let env = Env::with_trust(Answer::Yes, trust.clone());
        let handler = SwapHandler::new(env.clone());
        let host = HostService::new(
            host_config(),
            trust.clone(),
            TestUi::accepting(PermissionSet::VIEW | CV),
            registry_of(&[handler.handler()]),
            Arc::new(MonotonicClock::new()),
        );
        env.host.set(host.clone()).ok().unwrap();
        let server = open_session_endpoint(UdpSocket::bind(lo()).unwrap(), &who.identity, trust.clone()).unwrap();
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn({
            let (host, trust, server) = (host.clone(), trust.clone(), server.clone());
            serve_endpoint(host, trust, server)
        });
        let client = endpoint_from_socket(UdpSocket::bind(lo()).unwrap(), None).unwrap();
        Self {
            who,
            trust,
            env,
            handler,
            host,
            addr,
            client,
            viewing: Arc::new(Mutex::new(None)),
            results: Arc::new(Mutex::new(Vec::new())),
            _server: task,
        }
    }

    /// The machine's connector: a swap token opens a session to `peer` at its address.
    fn wire_connector(&self, peer: &Machine) {
        let (client, identity, addr, spki) =
            (self.client.clone(), self.who.identity.clone(), peer.addr, *peer.who.identity.spki());
        let (viewing, results, handler) = (self.viewing.clone(), self.results.clone(), self.handler.clone());
        let peer_id = peer.who.id;
        *lock(&self.env.connector) = Some(Box::new(move |host, token| {
            assert_eq!(host, peer_id);
            let (client, identity, viewing, results, handler) =
                (client.clone(), identity.clone(), viewing.clone(), results.clone(), handler.clone());
            tokio::spawn(async move {
                let mut cfg = viewer_config(CV);
                cfg.swap_token = Some(token);
                cfg.registry = registry_of(&[handler.handler()]);
                let s = ViewerSession::connect(&client, &identity, addr, spki, cfg)
                    .await
                    .expect("the swap connection opens");
                lock(&results).push((Instant::now(), s.grant().mode));
                *lock(&viewing) = Some(s);
            });
        }));
    }
}

#[test]
fn loopback_swap_works_both_ways_each_under_one_second() {
    run(async {
        let (m1, m2) = (Machine::start(), Machine::start());
        // Each machine has granted CONTROL to the other (the future host's check, 6.3 step 1).
        pin(&m1.trust, &m2.who, PermissionSet::VIEW | CV);
        pin(&m2.trust, &m1.who, PermissionSet::VIEW | CV);
        m1.wire_connector(&m2);
        m2.wire_connector(&m1);

        // M2 views M1.
        let mut cfg = viewer_config(CV);
        cfg.registry = registry_of(&[m2.handler.handler()]);
        let first = ViewerSession::connect(&m2.client, &m2.who.identity, m1.addr, *m1.who.identity.spki(), cfg)
            .await
            .expect("the first session opens");
        assert_eq!(first.grant().mode, GrantMode::Prompted);

        // Swap 1: the viewer M2 asks; M1 (the host) accepts; M1 then views M2.
        let t0 = Instant::now();
        assert_eq!(m2.handler.request(first.ctx()).await, SwapOutcome::Swapped);
        eventually(Duration::from_secs(5), "M1 to be granted on M2", || !lock(&m1.results).is_empty()).await;
        let (at, mode) = lock(&m1.results)[0];
        assert_eq!(mode, GrantMode::Swap, "a swap grants without a prompt");
        eprintln!("B4: swap 1 (the viewer asks) in {:.1} ms", (at - t0).as_secs_f64() * 1000.0);
        assert!(at - t0 < Duration::from_secs(1), "B4: swap 1 took {:?}", at - t0);
        assert_eq!(first.ctx().wait_ended().await.code, ErrorCode::Swapped);

        // The reverse session is live; M2 is its host.
        let second = lock(&m1.viewing).take().expect("M1 holds the new session");
        assert!(second.ctx().is_active());
        assert!(second.granted().contains(CV));
        eventually(Duration::from_secs(5), "the old session to leave M1's host", || m1.host.sessions().is_empty())
            .await;

        // Swap 2: the new viewer M1 asks; M2 (the host now) accepts; M2 views M1 again.
        let t1 = Instant::now();
        assert_eq!(m1.handler.request(second.ctx()).await, SwapOutcome::Swapped);
        eventually(Duration::from_secs(5), "M2 to be granted on M1", || !lock(&m2.results).is_empty()).await;
        let (at, mode) = lock(&m2.results)[0];
        assert_eq!(mode, GrantMode::Swap);
        eprintln!("B4: swap 2 (the new viewer asks) in {:.1} ms", (at - t1).as_secs_f64() * 1000.0);
        assert!(at - t1 < Duration::from_secs(1), "B4: swap 2 took {:?}", at - t1);
        assert_eq!(second.ctx().wait_ended().await.code, ErrorCode::Swapped);
        let third = lock(&m2.viewing).take().expect("M2 holds the session again");
        assert!(third.ctx().is_active());

        // A token works once: nothing else armed is left to use.
        assert_eq!(lock(&m1.env.armed).len() + lock(&m2.env.armed).len(), 2);
    });
}
