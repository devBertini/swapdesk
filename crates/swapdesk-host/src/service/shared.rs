//! State the handlers, the relays and the helper pumps share: the attached helpers, the live host relays and the
//! core's event channel. It holds no `HostService`, so the registry (which the service owns) can hold handlers that
//! point here without a cycle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use swapdesk_platform::{IfKind, Level, TraceSink};
use swapdesk_proto::{CursorPosition, CursorShape, Display, ErrorCode, RedactedLen, VideoHeader};
use swapdesk_session::{Forwarded, HostService, SessionCtx};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use super::helper_link::{HelperEvent, HelperLink};
use super::media_path::{DirectOut, VideoPath};
use super::pty_proxy::PtyRoutes;
use super::routing::{HelperKind, media_helper};
use swapdesk_ipc::Message as Ipc;

/// Events queued for the core's owner before an advisory one is dropped.
pub const CORE_EVENTS: usize = 64;

/// What the core tells its owner (the binary's supervisor, the UI dispatcher of the next slice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    HelperAttached {
        kind: HelperKind,
        pid: u32,
    },
    /// The helper's pipe ended: the supervisor relaunches it (C2.1: backoff 1, 2, 4 ... 30 s).
    HelperGone {
        kind: HelperKind,
        pid: u32,
        error: Option<String>,
    },
    HelperFault {
        kind: HelperKind,
        code: ErrorCode,
        what: String,
    },
    /// A helper asked for the secure attention sequence (honoured for `UNATTENDED` sessions only, a later slice).
    SasRequested {
        kind: HelperKind,
    },
}

/// What the media helper sent, as the relays of live sessions see it.
#[derive(Clone)]
pub(crate) enum MediaEv {
    Displays(Vec<Display>),
    /// A handle to the frame's bytes: every session gets one, none gets a copy.
    Video {
        header: VideoHeader,
        payload: bytes::Bytes,
    },
    CursorPos(CursorPosition),
    CursorShape(CursorShape),
    Audio(Vec<u8>),
    /// A helper's pipe ended (its link id).
    HelperGone(u64),
}

/// Screen pixels and system audio never show in `Debug` (CONTRACTS.md C2.3): sizes only.
impl core::fmt::Debug for MediaEv {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Displays(d) => f.debug_tuple("Displays").field(d).finish(),
            Self::Video { header, payload } => {
                f.debug_struct("Video").field("header", header).field("payload", &RedactedLen(payload.len())).finish()
            }
            Self::CursorPos(p) => f.debug_tuple("CursorPos").field(p).finish(),
            Self::CursorShape(s) => f.debug_tuple("CursorShape").field(s).finish(),
            Self::Audio(a) => f.debug_tuple("Audio").field(&RedactedLen(a.len())).finish(),
            Self::HelperGone(id) => f.debug_tuple("HelperGone").field(id).finish(),
        }
    }
}

/// Everything a host relay's task can be woken by.
pub(crate) enum RelayEv {
    Control(Forwarded),
    Media(MediaEv),
    HostMute(bool),
    /// Grants, desktop state or session end changed: the relay reads the levels again.
    Wake,
    Tick,
}

#[derive(Clone)]
pub(crate) struct RelayHandle {
    pub tx: mpsc::Sender<RelayEv>,
    pub ended: Arc<std::sync::atomic::AtomicBool>,
    /// Video frames of the session: a newest-wins slot and a sending task of its own.
    pub video: Arc<VideoPath>,
    /// Audio and cursor, sent straight from the helper pump.
    pub out: Arc<DirectOut>,
    /// The session, for what must be true of it before a held helper goes on (see [`Shared::sessions_follow`]).
    pub ctx: SessionCtx,
}

#[derive(Default)]
struct Slots {
    system: Option<Arc<HelperLink>>,
    user: Option<Arc<HelperLink>>,
}

impl Slots {
    fn get(&self, kind: HelperKind) -> Option<Arc<HelperLink>> {
        match kind {
            HelperKind::System => self.system.clone(),
            HelperKind::User => self.user.clone(),
        }
    }

    fn slot(&mut self, kind: HelperKind) -> &mut Option<Arc<HelperLink>> {
        match kind {
            HelperKind::System => &mut self.system,
            HelperKind::User => &mut self.user,
        }
    }
}

pub(crate) struct Shared {
    pub rt: Handle,
    pub trace: Arc<dyn TraceSink>,
    pub audio_exclude_pid: u32,
    slots: Mutex<Slots>,
    relays: Mutex<HashMap<u64, RelayHandle>>,
    events: SyncSender<CoreEvent>,
    /// Told after every event is queued, so the owner's loop sleeps until there is one.
    waker: OnceLock<Arc<dyn Fn() + Send + Sync>>,
    /// Advisory events dropped because the owner was not reading.
    events_dropped: AtomicU64,
    /// The account this core serves, on a machine of several (ACCOUNTS.md 5.4); unset for the only one.
    pub account: OnceLock<String>,
    /// Reads who is at the console now, for the fence (ACCOUNTS.md 5.1); unset: the cached owner alone decides.
    pub console_probe: OnceLock<super::ConsoleProbe>,
    /// Where the media helper's report of the input desktop goes (the account's hub, which keeps the console's
    /// desktop and tells the sessions the effective one); unset: the sessions are told the helper's value as it is.
    pub desktop_sink: OnceLock<super::DesktopSink>,
    /// Sends the secure attention sequence (the service process only); unset: a `SendSpecial` is answered
    /// `UNAVAILABLE`.
    pub sas_sender: OnceLock<super::SasSender>,
    /// The shells the user helper runs for the terminal proxy.
    pub ptys: PtyRoutes,
    /// The kind of the interface behind each local address a peer connected to, as the gate saw it when it let the
    /// connection in: a session's video starts at the rate of its link (F-VIEW-09) without asking the system again.
    links: Mutex<HashMap<std::net::IpAddr, IfKind>>,
    /// This machine has a hardware HEVC encoder (`ServiceConfig::host_hevc_hardware`); a session's codec is picked with it.
    pub host_hevc_hardware: AtomicBool,
    /// This machine decodes HEVC in hardware: a viewer session lists HEVC in its capabilities (F-VIEW-07).
    pub hevc_decode: Arc<AtomicBool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// The gate let a connection in on `local`, an address of an interface of `kind`.
    pub fn note_link(&self, local: std::net::IpAddr, kind: IfKind) {
        lock(&self.links).insert(local, kind);
    }

    /// Whether the interface that owns `local` is a wired one, as far as the gate saw it. An address the gate never
    /// judged (loopback in development) is not: the session starts at the wireless rate.
    pub fn is_wired(&self, local: Option<std::net::IpAddr>) -> bool {
        local.is_some_and(|ip| lock(&self.links).get(&ip) == Some(&IfKind::Ethernet))
    }

    pub fn new(rt: Handle, trace: Arc<dyn TraceSink>, audio_exclude_pid: u32, events: SyncSender<CoreEvent>) -> Self {
        Self {
            rt,
            trace,
            audio_exclude_pid,
            slots: Mutex::default(),
            relays: Mutex::default(),
            events,
            waker: OnceLock::new(),
            events_dropped: AtomicU64::new(0),
            account: OnceLock::new(),
            console_probe: OnceLock::new(),
            desktop_sink: OnceLock::new(),
            sas_sender: OnceLock::new(),
            ptys: PtyRoutes::default(),
            links: Mutex::default(),
            host_hevc_hardware: AtomicBool::new(false),
            hevc_decode: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn log(&self, level: Level, what: core::fmt::Arguments<'_>) {
        self.trace.log(level, "service", what);
    }

    /// Queues `event` for the owner. The queue is bounded ([`CORE_EVENTS`]): a helper that faults or asks for the secure
    /// attention sequence in a loop cannot fill it, because those two are advisory and dropped (counted) when the owner
    /// is not reading. `HelperAttached` and `HelperGone` change what the supervisor does, so they are always delivered:
    /// when there is no room a blocking thread waits for it (a helper starts at most once per second of backoff).
    pub fn emit(&self, event: CoreEvent) {
        let essential = matches!(event, CoreEvent::HelperAttached { .. } | CoreEvent::HelperGone { .. });
        match self.events.try_send(event) {
            Ok(()) => self.wake(),
            Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(event)) if essential => {
                let tx = self.events.clone();
                let waker = self.waker.get().cloned();
                self.rt.spawn_blocking(move || {
                    // A closed receiver means nobody listens: not an error.
                    if tx.send(event).is_ok()
                        && let Some(wake) = waker
                    {
                        wake();
                    }
                });
            }
            Err(TrySendError::Full(_)) => {
                if self.events_dropped.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.log(Level::Warn, format_args!("the core's event queue is full: advisory events are dropped"));
                }
            }
        }
    }

    /// Advisory events dropped so far.
    pub fn events_dropped(&self) -> u64 {
        self.events_dropped.load(Ordering::Relaxed)
    }

    /// Sets what runs after every event is queued (once).
    pub fn set_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.waker.set(waker).is_ok()
    }

    fn wake(&self) {
        if let Some(wake) = self.waker.get() {
            wake();
        }
    }

    pub fn helper(&self, kind: HelperKind) -> Option<Arc<HelperLink>> {
        lock(&self.slots).get(kind).filter(|h| h.is_alive())
    }

    /// The helper that serves video, cursor, displays and audio.
    pub fn media(&self) -> Option<(HelperKind, Arc<HelperLink>)> {
        media_helper(self.helper(HelperKind::System), self.helper(HelperKind::User))
    }

    pub fn media_displays(&self) -> Vec<Display> {
        self.media().map(|(_, h)| h.displays()).unwrap_or_default()
    }

    /// Puts `link` in its slot; returns the helper it replaced.
    pub fn install(&self, link: Arc<HelperLink>) -> Option<Arc<HelperLink>> {
        let kind = link.kind();
        lock(&self.slots).slot(kind).replace(link)
    }

    /// Empties the slot of `kind` when it still holds the helper `id`.
    pub fn remove_if(&self, kind: HelperKind, id: u64) {
        let mut slots = lock(&self.slots);
        let slot = slots.slot(kind);
        if slot.as_ref().is_some_and(|h| h.id == id) {
            *slot = None;
        }
    }

    pub fn register_relay(&self, session_id: u64, handle: RelayHandle) {
        lock(&self.relays).insert(session_id, handle);
    }

    pub fn unregister_relay(&self, session_id: u64) {
        lock(&self.relays).remove(&session_id);
    }

    pub fn relay(&self, session_id: u64) -> Option<RelayHandle> {
        lock(&self.relays).get(&session_id).cloned()
    }

    fn relays(&self) -> Vec<RelayHandle> {
        lock(&self.relays).values().cloned().collect()
    }

    /// Hands `ev` to every live session. Video goes to the session's slot, audio and cursor leave at once; none of
    /// them waits (stage 64). Display lists and a helper's end go through the relay's queue, waiting for room.
    pub async fn fan(&self, ev: MediaEv) {
        match ev {
            MediaEv::Video { header, payload } => {
                // Every session but the last gets another handle to the frame; the last one gets this one.
                let mut handles = self.relays().into_iter().peekable();
                let mut payload = Some(payload);
                while let Some(h) = handles.next() {
                    let one = if handles.peek().is_some() { payload.clone() } else { payload.take() };
                    if let Some(one) = one {
                        h.video.offer(header, one);
                    }
                }
            }
            MediaEv::Audio(datagram) => {
                for h in self.relays() {
                    h.out.audio(datagram.clone());
                }
            }
            MediaEv::CursorPos(pos) => {
                for h in self.relays() {
                    h.out.cursor_pos(&pos);
                }
            }
            MediaEv::CursorShape(shape) => {
                for h in self.relays() {
                    h.out.cursor_shape(&shape);
                }
            }
            ev @ (MediaEv::Displays(_) | MediaEv::HelperGone(_)) => self.fan_queued(ev).await,
        }
    }

    async fn fan_queued(&self, ev: MediaEv) {
        let mut handles = self.relays().into_iter().peekable();
        let mut ev = Some(ev);
        while let Some(h) = handles.next() {
            let one = if handles.peek().is_some() { ev.clone() } else { ev.take() };
            if let Some(one) = one {
                let _ = h.tx.send(RelayEv::Media(one)).await;
            }
        }
    }

    /// The host user's mute, to every live session's viewer.
    pub fn mute_all(&self, muted: bool) {
        for h in self.relays() {
            let _ = h.tx.try_send(RelayEv::HostMute(muted));
        }
    }

    /// Whether every live host session has taken `desktop` as its own (its pause flag and `HostState` follow from it).
    /// A held helper goes on only then: a picture of a protected desktop never reaches a session that has not yet
    /// been paused for it (the session applies a desktop report on its own task, a moment after the report).
    pub fn sessions_follow(&self, desktop: swapdesk_proto::Desktop) -> bool {
        self.relays().iter().all(|h| h.ended.load(Ordering::Acquire) || h.ctx.desktop_state().desktop == desktop)
    }

    pub fn wake_all(&self) {
        for h in self.relays() {
            let _ = h.tx.try_send(RelayEv::Wake);
        }
    }
}

/// How often a held helper is looked at again, and after how long its sessions pause meanwhile (ACCOUNTS.md 5.1,
/// GUESS values).
const FENCE_RETRY: std::time::Duration = std::time::Duration::from_millis(200);
/// How often the sessions' catch-up with a desktop is looked at once the fence would let the helper go on.
const FENCE_FOLLOW: std::time::Duration = std::time::Duration::from_millis(20);
const FENCE_PAUSE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// The fence, the service's side: the helper's capture and input go on after a desktop switch only once this account
/// may host now (its user at the console, or nobody for an entry without a prompt). Until then nothing of the new
/// desktop reaches a session, and after 10 s the sessions pause as away. Fails closed: a newer switch, or a helper that
/// went, ends the wait.
async fn resume_when_allowed(shared: Arc<Shared>, host: HostService, link: Arc<HelperLink>, generation: u64) {
    let started = tokio::time::Instant::now();
    let mut paused = false;
    loop {
        if link.held.load(std::sync::atomic::Ordering::SeqCst) != generation || !link.is_alive() {
            return;
        }
        if fence_allows(&shared, &host) {
            let desktop = host.desktop_now();
            if paused {
                host.set_desktop(desktop);
                paused = false;
            }
            // The sessions are on the desktop the helper now captures before its pictures may reach them (F-UNAT-01).
            if !shared.sessions_follow(desktop) {
                tokio::time::sleep(FENCE_FOLLOW).await;
                continue;
            }
            link.resumed.store(generation, std::sync::atomic::Ordering::SeqCst);
            link.send_now(swapdesk_ipc::Message::ResumeCapture { generation });
            shared.log(Level::Info, format_args!("the helper goes on after the switch ({generation})"));
            return;
        }
        if !paused && started.elapsed() >= FENCE_PAUSE_AFTER {
            shared.log(Level::Info, format_args!("fenced: another account is at the console ({generation})"));
            host.set_desktop(swapdesk_proto::Desktop::AccountAway);
            paused = true;
        }
        tokio::time::sleep(FENCE_RETRY).await;
    }
}

/// Whether the fence may let a helper go on: the cached console owner must allow it, and so must a fresh read of the
/// console (the cached owner changes only when the service hears of a logon, which may come after the switch). A
/// user that cannot be read fails closed.
fn fence_allows(shared: &Shared, host: &HostService) -> bool {
    use swapdesk_session::ConsoleOwner;
    if !host.may_host_now() {
        return false;
    }
    let (Some(account), Some(probe)) = (shared.account.get(), shared.console_probe.get()) else { return true };
    let owner = match probe() {
        super::ConsoleRead::Nobody => ConsoleOwner::Nobody,
        super::ConsoleRead::User(u) if u.eq_ignore_ascii_case(account) => ConsoleOwner::This,
        super::ConsoleRead::User(_) | super::ConsoleRead::Unreadable => ConsoleOwner::Other,
    };
    host.may_host_with(owner)
}

/// Reads a helper's events until its pipe ends: the glue between the reader thread and the sessions.
pub(crate) async fn pump(
    shared: Arc<Shared>,
    host: HostService,
    link: Arc<HelperLink>,
    mut events: mpsc::Receiver<HelperEvent>,
) {
    let kind = link.kind();
    while let Some(ev) = events.recv().await {
        let is_media = shared.media().is_some_and(|(_, m)| m.id == link.id);
        match ev {
            HelperEvent::Displays(list) => {
                link.set_displays(list.displays.clone());
                // A new display list from a helper that is not the media one changes nothing for the sessions.
                if is_media {
                    // The injecting helper maps the pointer onto these displays, not onto a listing of its own that
                    // may be older (owner, 12/11: a mode change under the same id moved every click, D64).
                    for other in [HelperKind::System, HelperKind::User].into_iter().filter_map(|k| shared.helper(k)) {
                        if other.id != link.id {
                            other.send(Ipc::Displays(list.clone())).await;
                        }
                    }
                    shared.fan(MediaEv::Displays(list.displays)).await;
                }
            }
            // The fence: nothing a held helper sends reaches a session (it should send nothing; this is the second lock).
            HelperEvent::Video { .. }
            | HelperEvent::CursorPos(_)
            | HelperEvent::CursorShape(_)
            | HelperEvent::Audio(_)
                if link.is_held() => {}
            HelperEvent::Video { header, payload } if is_media => shared.fan(MediaEv::Video { header, payload }).await,
            HelperEvent::CursorPos(p) if is_media => shared.fan(MediaEv::CursorPos(p)).await,
            HelperEvent::CursorShape(s) if is_media => shared.fan(MediaEv::CursorShape(s)).await,
            HelperEvent::Audio(d) if is_media => shared.fan(MediaEv::Audio(d)).await,
            HelperEvent::Video { .. }
            | HelperEvent::CursorPos(_)
            | HelperEvent::CursorShape(_)
            | HelperEvent::Audio(_) => {}
            HelperEvent::AudioMode(mode) => {
                shared.log(Level::Debug, format_args!("{kind:?} helper audio mode {mode:?}"))
            }
            HelperEvent::Desktop(desktop) => {
                shared.log(Level::Info, format_args!("{kind:?} helper reports desktop {desktop:?}"));
                // Only the media helper's desktop counts: it is the one whose pictures the sessions see. The report
                // goes to the hub, which combines it with the console's owner and the lock state; without a hub
                // (a bare core) the sessions get it as it is.
                if is_media {
                    match shared.desktop_sink.get() {
                        Some(sink) => sink(desktop),
                        None => host.set_desktop(desktop),
                    }
                }
            }
            HelperEvent::CaptureHeld(generation) => {
                shared.log(Level::Info, format_args!("{kind:?} helper holds after a desktop switch ({generation})"));
                // Capture threads of several displays may report out of order: the newest switch counts.
                link.held.fetch_max(generation, std::sync::atomic::Ordering::SeqCst);
                shared.rt.spawn(resume_when_allowed(Arc::clone(&shared), host.clone(), Arc::clone(&link), generation));
            }
            // Only the SYSTEM helper may ask for the secure attention sequence (D11): the user helper runs as a
            // user and its asking is a sign of something wrong.
            HelperEvent::SasRequest if kind == HelperKind::System => shared.emit(CoreEvent::SasRequested { kind }),
            HelperEvent::SasRequest => shared
                .log(Level::Warn, format_args!("{kind:?} helper asked for the secure attention sequence: ignored")),
            // Only the user helper runs shells (a system helper refuses them): its words about a terminal count, and
            // only for a terminal it was asked to open. The output waits for room (back-pressure toward the helper).
            HelperEvent::PtyOpenReply { id, code, account } if kind == HelperKind::User => {
                shared.ptys.reply(link.id, id, code, account)
            }
            HelperEvent::PtyOutput { id, bytes } if kind == HelperKind::User => {
                shared.ptys.output(link.id, id, bytes).await
            }
            HelperEvent::PtyExited { id, code } if kind == HelperKind::User => shared.ptys.exited(link.id, id, code),
            HelperEvent::PtyOpenReply { .. } | HelperEvent::PtyOutput { .. } | HelperEvent::PtyExited { .. } => {
                shared.log(Level::Warn, format_args!("{kind:?} helper sent a terminal message: ignored"));
            }
            HelperEvent::Note { what } => shared.log(Level::Info, format_args!("{kind:?} helper: {what}")),
            HelperEvent::Fault { code, what } => {
                shared.log(Level::Warn, format_args!("{kind:?} helper fault {code:?}: {what}"));
                shared.emit(CoreEvent::HelperFault { kind, code, what });
            }
            HelperEvent::Gone { error } => {
                // What the gone media helper said about the input desktop dies with it: the next one reports its own
                // before it sends a frame (it reports at the start of its capture), and a picture of a protected
                // desktop cannot come from a helper that is not there.
                if is_media && let Some(sink) = shared.desktop_sink.get() {
                    sink(swapdesk_proto::Desktop::Default);
                }
                shared.remove_if(kind, link.id);
                shared.ptys.fail_link(link.id);
                match &error {
                    Some(e) => {
                        shared.log(Level::Warn, format_args!("{kind:?} helper (pid {}) is gone: {e}", link.pid()))
                    }
                    None => shared.log(Level::Info, format_args!("{kind:?} helper (pid {}) is gone", link.pid())),
                }
                shared.emit(CoreEvent::HelperGone { kind, pid: link.pid(), error });
                shared.fan(MediaEv::HelperGone(link.id)).await;
                shared.wake_all();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use swapdesk_fakes::LogCapture;

    use super::*;

    fn shared() -> (tokio::runtime::Runtime, Arc<Shared>, std::sync::mpsc::Receiver<CoreEvent>) {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(CORE_EVENTS);
        let shared = Arc::new(Shared::new(rt.handle().clone(), Arc::new(LogCapture::new()), 0, tx));
        (rt, shared, rx)
    }

    #[test]
    fn advisory_events_stop_at_the_bound_and_a_helper_gone_is_still_delivered_once_the_owner_reads() {
        let (_rt, shared, rx) = shared();
        let fault = |n: usize| CoreEvent::HelperFault {
            kind: HelperKind::User,
            code: ErrorCode::Internal,
            what: n.to_string(),
        };
        // A helper that faults in a loop while the owner does not read.
        for n in 0..(CORE_EVENTS * 8) {
            shared.emit(fault(n));
        }
        assert_eq!(
            shared.events_dropped(),
            (CORE_EVENTS * 7) as u64,
            "everything past the bound is dropped and counted"
        );
        // The queue is full and a helper's end must still reach the supervisor.
        shared.emit(CoreEvent::HelperGone { kind: HelperKind::User, pid: 77, error: None });
        let mut seen = 0;
        let gone = loop {
            match rx.recv_timeout(Duration::from_secs(5)).expect("the owner is told the helper is gone") {
                CoreEvent::HelperGone { pid, .. } => break pid,
                CoreEvent::HelperFault { .. } => seen += 1,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(gone, 77);
        assert_eq!(seen, CORE_EVENTS, "the first events, in order, are the ones that were kept");
    }

    #[test]
    fn the_waker_runs_after_each_queued_event_and_the_owner_wakes_on_it_not_on_a_timer() {
        let (_rt, shared, rx) = shared();
        let woke = Arc::new(AtomicUsize::new(0));
        let (ping_tx, ping_rx) = std::sync::mpsc::channel();
        {
            let (woke, ping) = (Arc::clone(&woke), Mutex::new(ping_tx));
            assert!(shared.set_waker(Arc::new(move || {
                woke.fetch_add(1, Ordering::SeqCst);
                let _ = lock(&ping).send(());
            })));
        }
        let emitter = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                shared.emit(CoreEvent::SasRequested { kind: HelperKind::System });
            })
        };
        let began = std::time::Instant::now();
        ping_rx.recv_timeout(Duration::from_secs(10)).expect("the waker ran");
        assert!(began.elapsed() >= Duration::from_millis(150), "nothing woke the owner before the event");
        assert!(matches!(rx.try_recv(), Ok(CoreEvent::SasRequested { .. })), "the event is queued when the waker runs");
        emitter.join().unwrap();
        assert_eq!(woke.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_session_is_wired_only_on_an_address_the_gate_saw_on_ethernet() {
        use std::net::{IpAddr, Ipv4Addr};
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
        let (tx, _rx) = std::sync::mpsc::sync_channel(4);
        let shared = Shared::new(rt.handle().clone(), Arc::new(LogCapture::new()), 0, tx);
        let (cable, wifi, unseen) = (
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 10)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        );
        shared.note_link(cable, IfKind::Ethernet);
        shared.note_link(wifi, IfKind::WiFi);
        assert!(shared.is_wired(Some(cable)));
        assert!(!shared.is_wired(Some(wifi)));
        assert!(!shared.is_wired(Some(unseen)), "an address the gate never judged starts at the wireless rate");
        assert!(!shared.is_wired(None));
        // The interface behind an address can change (a dock, a new network): the last look wins.
        shared.note_link(cable, IfKind::WiFi);
        assert!(!shared.is_wired(Some(cable)));
    }
}
