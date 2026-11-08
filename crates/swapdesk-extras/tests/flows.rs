//! Flow proofs of stage 22 over the session layer: reconnect after an injected drop (F-NET-01) and chat both ways with
//! read state (F-CHAT-01). The host and viewer are joined by the in-memory control link with a paused clock (the
//! harness of `swapdesk-session/tests/common`, reduced to what is needed here). Clipboard (F-CLIP-01) and
//! Wake-on-LAN (F-WAKE-01) are proved by the unit tests of their modules, over the fake clipboard and a fake socket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use swapdesk_extras::chat::ChatHandler;
use swapdesk_extras::reconnect::{GiveUp, ReconnectEvent, ReconnectPolicy, reconnect, should_reconnect};
use swapdesk_extras::{ExtrasSink, NullSink};
use swapdesk_fakes::{FakeClock, MemorySecretStore};
use swapdesk_platform::MonoMicros;
use swapdesk_proto::{Capabilities, ErrorCode, GrantMode, KbKind, KeyboardInfo, Message, Os, PermissionSet};
use swapdesk_session::{
    ChannelHandler, HostConfig, HostService, HostUi, OpenError, PromptAnswer, PromptRequest, Registry, SessionCtx,
    SessionEnd, ViewerConfig, ViewerSession, memory_pair,
};
use swapdesk_trust::{PairedDevice, TrustHandle, device_id};
use tokio::sync::oneshot;

const GRANTS: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::CLIPBOARD);

struct Ui;

impl HostUi for Ui {
    fn somebody_present(&self) -> bool {
        true
    }
    fn prompt(&self, _request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(PromptAnswer::Accept(GRANTS));
        rx
    }
    fn consent(&self, _request: swapdesk_session::ConsentRequest) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(true);
        rx
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        video_decode: vec![],
        audio: vec![],
        max_datagram: 1200,
        keyboard: KeyboardInfo { kind: KbKind::Ansi, layout: "us".into() },
        features: 0,
    }
}

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock::new(MonoMicros(1)))
}

struct Pair {
    host: HostService,
    viewer: swapdesk_trust::DeviceId,
}

fn pair(host_registry: Registry) -> Pair {
    let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
    let identity = swapdesk_trust::Identity::generate(0).unwrap();
    let id = device_id(identity.spki());
    trust
        .update(|s| {
            let mut d = PairedDevice::new_paired(*identity.spki(), "viewer".into(), Os::MacOs, "15".into(), 1, None);
            d.grants_here = GRANTS;
            s.devices.push(d);
        })
        .unwrap();
    let host = HostService::new(
        HostConfig::new("host-pc", Os::Windows, "11 24H2", "0.1.0", capabilities()),
        trust,
        Arc::new(Ui),
        host_registry,
        clock(),
    );
    Pair { host, viewer: id }
}

impl Pair {
    async fn open(&self, cfg: ViewerConfig) -> Result<ViewerSession, OpenError> {
        let (host_io, viewer_io) = memory_pair();
        let (host, id) = (self.host.clone(), self.viewer);
        tokio::spawn(async move { host.serve_io(host_io, None, id).await });
        ViewerSession::open_io(viewer_io, None, Some([9; 16]), cfg).await
    }
}

fn viewer_config() -> ViewerConfig {
    let mut cfg = ViewerConfig::new("viewer-mac", Os::MacOs, "15.1", "0.1.0", capabilities(), clock());
    cfg.requested = GRANTS;
    cfg
}

fn run_paused<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap().block_on(f)
}

#[test]
fn flow_net_reconnect_resumes_after_an_injected_drop_without_a_new_prompt() {
    run_paused(async {
        let p = pair(Registry::new());
        let first = p.open(viewer_config()).await.unwrap();
        let (token, session_id) = (first.grant().resume.clone(), first.grant().session_id);

        // The drop: the viewer's link vanishes without a Bye; the host sees a lost connection. (The viewer's own
        // `Ended { Lost }` over a dead link is proved by the session crate's liveness and resume tests; here the
        // viewer side is the policy: a Lost end is retried, a Bye is not.)
        first.abandon();
        assert!(should_reconnect(&SessionEnd { kind: swapdesk_session::EndKind::Lost, code: ErrorCode::NoError }));
        let t0 = tokio::time::Instant::now();
        while !p.host.sessions().is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(1), "the host never saw the loss");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The network is still down for the first two attempts.
        let tries = Mutex::new(0u32);
        let events = Mutex::new(Vec::new());
        let resumed = reconnect(
            ReconnectPolicy::default(),
            token,
            |resume| {
                let n = {
                    let mut t = tries.lock().unwrap();
                    *t += 1;
                    *t
                };
                let (p, down) = (&p, n <= 2);
                async move {
                    if down {
                        return Err(OpenError::Lost);
                    }
                    let mut cfg = viewer_config();
                    cfg.resume = Some(resume);
                    p.open(cfg).await
                }
            },
            |e| events.lock().unwrap().push(e),
        )
        .await
        .expect("the session should come back");

        assert_eq!(resumed.grant().mode, GrantMode::Resumed);
        assert_eq!(resumed.grant().session_id, session_id, "the same session");
        assert_eq!(resumed.grant().granted, GRANTS);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ReconnectEvent::Reconnecting { attempt: 1 },
                ReconnectEvent::Reconnecting { attempt: 2 },
                ReconnectEvent::Reconnecting { attempt: 3 },
                ReconnectEvent::Back { attempt: 3, mode: GrantMode::Resumed },
            ]
        );
    });
}

#[test]
fn flow_net_reconnect_gives_up_after_the_60_second_window_and_on_a_final_refusal() {
    run_paused(async {
        let p = pair(Registry::new());
        let s = p.open(viewer_config()).await.unwrap();
        let token = s.grant().resume.clone();
        s.abandon();

        let t0 = tokio::time::Instant::now();
        let r = reconnect(
            ReconnectPolicy::default(),
            token.clone(),
            |_| async { Err::<ViewerSession, _>(OpenError::Lost) },
            |_| {},
        )
        .await;
        assert!(matches!(r, Err(GiveUp::WindowElapsed)));
        let took = t0.elapsed();
        assert!(took >= Duration::from_secs(60) && took < Duration::from_secs(63), "took {took:?}");

        let r = reconnect(
            ReconnectPolicy::default(),
            token,
            |_| async { Err::<ViewerSession, _>(OpenError::Refused(ErrorCode::Declined)) },
            |_| {},
        )
        .await;
        assert!(matches!(r, Err(GiveUp::Refused(ErrorCode::Declined))));
    });
}

/// Delegates to a `ChatHandler` and keeps the session's `ctx` so the test can send from the host side.
struct Capture {
    inner: Arc<ChatHandler>,
    ctx: Mutex<Option<SessionCtx>>,
}

impl ChannelHandler for Capture {
    fn name(&self) -> &'static str {
        "chat-capture"
    }
    fn messages(&self) -> &'static [u16] {
        self.inner.messages()
    }
    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        self.inner.on_message(ctx, msg);
    }
    fn on_start(&self, ctx: &SessionCtx) {
        *self.ctx.lock().unwrap() = Some(ctx.clone());
        self.inner.on_start(ctx);
    }
    fn on_end(&self, ctx: &SessionCtx, end: &SessionEnd) {
        self.inner.on_end(ctx, end);
    }
}

#[test]
fn flow_chat_messages_travel_both_ways_with_read_state_and_vanish_at_the_end() {
    run_paused(async {
        let sink: Arc<dyn ExtrasSink> = Arc::new(NullSink);
        let host_chat = Arc::new(Capture { inner: ChatHandler::new(Arc::clone(&sink)), ctx: Mutex::new(None) });
        let mut hr = Registry::new();
        hr.register(host_chat.clone()).unwrap();
        let p = pair(hr);
        let viewer_chat = ChatHandler::new(sink);
        let mut vr = Registry::new();
        vr.register(viewer_chat.clone()).unwrap();
        let mut cfg = viewer_config();
        cfg.registry = vr;
        let s = p.open(cfg).await.unwrap();

        let id = viewer_chat.send(s.ctx(), "oi", 1_000).await.unwrap();
        wait(|| host_chat.inner.entries().len() == 1).await;
        assert_eq!(host_chat.inner.unread(), 1);
        assert!(!viewer_chat.entries()[0].read, "sent, not read yet");

        let host_ctx = host_chat.ctx.lock().unwrap().clone().unwrap();
        host_chat.inner.mark_read(&host_ctx).await;
        wait(|| viewer_chat.entries()[0].read).await;
        assert_eq!(viewer_chat.entries()[0].msg_id, id);

        host_chat.inner.send(&host_ctx, "olá", 2_000).await.unwrap();
        wait(|| viewer_chat.unread() == 1).await;
        assert_eq!(viewer_chat.entries().len(), 2);

        s.disconnect().await;
        wait(|| viewer_chat.entries().is_empty() && host_chat.inner.entries().is_empty()).await;
    });
}

async fn wait(mut cond: impl FnMut() -> bool) {
    let t0 = tokio::time::Instant::now();
    while !cond() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
