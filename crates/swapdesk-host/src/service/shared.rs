//! State the handlers, the relays and the helper pumps share: the attached helpers, the live host relays and the
//! core's event channel. It holds no `HostService`, so the registry (which the service owns) can hold handlers that
//! point here without a cycle.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use swapdesk_platform::{Level, TraceSink};
use swapdesk_proto::{CursorPosition, CursorShape, Display, ErrorCode, RedactedLen, VideoHeader};
use swapdesk_session::{Forwarded, HostService};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use super::helper_link::{HelperEvent, HelperLink};
use super::pty_proxy::PtyRoutes;
use super::routing::{HelperKind, media_helper};

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
    Video {
        header: VideoHeader,
        payload: Vec<u8>,
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
    pub ended: Arc<AtomicBool>,
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
    events: Mutex<std::sync::mpsc::Sender<CoreEvent>>,
    /// The shells the user helper runs for the terminal proxy.
    pub ptys: PtyRoutes,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    pub fn new(
        rt: Handle,
        trace: Arc<dyn TraceSink>,
        audio_exclude_pid: u32,
        events: std::sync::mpsc::Sender<CoreEvent>,
    ) -> Self {
        Self {
            rt,
            trace,
            audio_exclude_pid,
            slots: Mutex::default(),
            relays: Mutex::default(),
            events: Mutex::new(events),
            ptys: PtyRoutes::default(),
        }
    }

    pub fn log(&self, level: Level, what: core::fmt::Arguments<'_>) {
        self.trace.log(level, "service", what);
    }

    pub fn emit(&self, event: CoreEvent) {
        // A closed receiver means nobody listens: not an error.
        let _ = lock(&self.events).send(event);
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

    /// Hands `ev` to every live relay, waiting for room (media backpressure reaches the helper's pipe).
    pub async fn fan(&self, ev: MediaEv) {
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

    pub fn wake_all(&self) {
        for h in self.relays() {
            let _ = h.tx.try_send(RelayEv::Wake);
        }
    }
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
                    shared.fan(MediaEv::Displays(list.displays)).await;
                }
            }
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
                if is_media {
                    host.set_desktop(desktop);
                }
            }
            HelperEvent::SasRequest => shared.emit(CoreEvent::SasRequested { kind }),
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
            HelperEvent::Fault { code, what } => {
                shared.log(Level::Warn, format_args!("{kind:?} helper fault {code:?}: {what}"));
                shared.emit(CoreEvent::HelperFault { kind, code, what });
            }
            HelperEvent::Gone { error } => {
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
