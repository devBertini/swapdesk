//! The host's media path of a session, off the relay's serial task (stage 64, findings C10).
//!
//! * [`VideoPath`]: video frames from the helper pump go into a one-frame slot with an arrival stamp (newest wins) and
//!   a task of its own sends them. `VideoSender::send` may wait up to 250 ms for the peer to allow another stream;
//!   nothing else waits behind it. A frame that was replaced in the slot, or that waited past [`STALE`], is a gap in
//!   the stream: the P-frames that follow would not decode at the viewer, so they are dropped and a keyframe is asked
//!   for (PROTOCOL.md 4, "Stale video").
//! * [`DirectOut`]: audio datagrams and cursor positions and shapes leave straight from the helper pump, never through
//!   the relay's queue, so a stalled video send or a busy relay holds none of them back.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::Connection;
use swapdesk_proto::{CursorPosition, CursorShape, PermissionSet, StreamKind, VideoHeader, Wire};
use swapdesk_session::SessionCtx;
use swapdesk_video::{Frame, VideoSender};
use tokio::sync::Notify;

/// A P-frame that waited longer than this in the slot is dropped (the 100 ms backlog rule of PROTOCOL.md 4).
pub(crate) const STALE: Duration = Duration::from_millis(100);
/// How long P-frames are held back after a keyframe was asked for before it is asked again.
pub(crate) const KEY_WAIT: Duration = Duration::from_millis(500);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Stamped {
    header: VideoHeader,
    payload: Bytes,
    at: Instant,
}

#[derive(Default)]
struct Slot {
    frame: Option<Stamped>,
    /// A frame was replaced before it was taken: the stream has a gap.
    lost: bool,
}

/// What the video task needs of the running capture: the display, and how to ask the helper for a keyframe.
#[derive(Clone)]
pub(crate) struct VideoRun {
    pub display: u16,
    pub key: Arc<dyn Fn() + Send + Sync>,
}

/// Counters of what the path dropped (for logs and tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PathStats {
    pub replaced: u64,
    pub stale: u64,
    pub awaiting_key: u64,
    pub keyframes_asked: u64,
}

pub(crate) struct VideoPath {
    slot: Mutex<Slot>,
    wake: Notify,
    closed: AtomicBool,
    run: Mutex<Option<VideoRun>>,
    /// Set when a keyframe was asked for and has not come: P-frames are dropped meanwhile.
    awaiting: Mutex<Option<Instant>>,
    replaced: AtomicU64,
    stale: AtomicU64,
    awaiting_key: AtomicU64,
    asked: AtomicU64,
    /// The session the frames are for: a frame is never sent while its session is paused or lacks `VIEW`, whatever the
    /// sender's own flag says (the relay sets that one a moment after the session changes; F-UNAT-01).
    gate: Mutex<Option<SessionCtx>>,
}

impl VideoPath {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::default(),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            run: Mutex::new(None),
            awaiting: Mutex::new(None),
            replaced: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            awaiting_key: AtomicU64::new(0),
            asked: AtomicU64::new(0),
            gate: Mutex::new(None),
        })
    }

    /// Names the session whose pause and grants gate every frame.
    pub fn set_gate(&self, ctx: SessionCtx) {
        *lock(&self.gate) = Some(ctx);
    }

    /// Whether the session forbids a picture now: paused on a desktop it may not see, or no `VIEW`.
    fn shut(&self) -> bool {
        lock(&self.gate).as_ref().is_some_and(|c| c.is_paused() || !c.granted().contains(PermissionSet::VIEW))
    }

    /// The capture that runs now (`None`: none). Whatever waited in the slot belonged to the old one.
    pub fn set_run(&self, run: Option<VideoRun>) {
        *lock(&self.run) = run;
        *lock(&self.awaiting) = None;
        let mut s = lock(&self.slot);
        s.frame = None;
        s.lost = false;
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    pub fn stats(&self) -> PathStats {
        PathStats {
            replaced: self.replaced.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
            awaiting_key: self.awaiting_key.load(Ordering::Relaxed),
            keyframes_asked: self.asked.load(Ordering::Relaxed),
        }
    }

    /// A frame from the helper: never waits. It replaces the one still in the slot (newest wins).
    pub fn offer(&self, header: VideoHeader, payload: Bytes) {
        match &*lock(&self.run) {
            Some(run) if run.display == header.display_id => {}
            _ => return,
        }
        {
            let mut s = lock(&self.slot);
            if s.frame.replace(Stamped { header, payload, at: Instant::now() }).is_some() {
                s.lost = true;
                self.replaced.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.wake.notify_one();
    }

    fn ask_key(&self, run: &VideoRun) {
        *lock(&self.awaiting) = Some(Instant::now());
        self.asked.fetch_add(1, Ordering::Relaxed);
        (run.key)();
    }

    /// A keyframe is needed: asked now, unless one was asked less than [`KEY_WAIT`] ago.
    fn want_key(&self, run: &VideoRun) {
        let due = lock(&self.awaiting).is_none_or(|since| since.elapsed() > KEY_WAIT);
        if due {
            self.ask_key(run);
        }
    }

    /// The sender's wants (the viewer's `KeyframeRequest`, a reset backlog, a resume after a pause) become a keyframe
    /// request to the helper. The sender spaces them itself (`min_keyframe_interval`).
    pub fn poll_keyframe(&self, sender: &VideoSender) {
        let Some(run) = lock(&self.run).clone() else { return };
        if lock(&self.awaiting).is_none() && sender.take_keyframe_request(run.display) {
            self.ask_key(&run);
        }
    }

    /// Whether `frame` goes to the sender. A gap (a replaced or stale frame) or a pending keyframe request drops
    /// P-frames and asks for the keyframe.
    fn admit(&self, stamped: Stamped, lost: bool) -> Option<Frame> {
        let run = lock(&self.run).clone()?;
        let Stamped { header, payload, at } = stamped;
        if header.display_id != run.display {
            return None;
        }
        let key = header.keyframe || header.discontinuity;
        if key {
            *lock(&self.awaiting) = None;
            return Some(Frame { header, payload });
        }
        let aged = at.elapsed() > STALE;
        if lost || aged {
            if aged && !lost {
                self.stale.fetch_add(1, Ordering::Relaxed);
            }
            self.want_key(&run);
            return None;
        }
        if lock(&self.awaiting).is_some() {
            // A keyframe was asked for and has not come: what follows would not decode at the viewer. Ask again when
            // it takes too long.
            self.awaiting_key.fetch_add(1, Ordering::Relaxed);
            self.want_key(&run);
            return None;
        }
        Some(Frame { header, payload })
    }

    /// The frame in the slot, and whether one was replaced before it.
    fn take(&self) -> Option<(Stamped, bool)> {
        let mut s = lock(&self.slot);
        s.frame.take().map(|f| (f, std::mem::take(&mut s.lost)))
    }

    /// The video task: takes the newest frame, sends it, takes the next. Ends on [`close`](Self::close).
    pub async fn run(self: Arc<Self>, sender: VideoSender) {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            let Some((stamped, lost)) = self.take() else {
                // `notify_one` keeps a permit when nobody waits, so an offer between the look and the wait is not lost.
                self.wake.notified().await;
                continue;
            };
            if self.shut() {
                // Dropped here, so the stream has a gap: a keyframe is asked for, and the P-frames that follow are
                // dropped until it comes.
                if let Some(run) = lock(&self.run).clone() {
                    self.want_key(&run);
                }
            } else if let Some(frame) = self.admit(stamped, lost) {
                let _ = sender.send(frame).await;
            }
            self.poll_keyframe(&sender);
        }
    }
}

/// What leaves straight from the helper pump: audio datagrams, cursor positions and shapes.
pub(crate) struct DirectOut {
    /// The session, for its grants and pause and the cursor shape's stream. `None` in a test over a bare connection.
    ctx: Option<SessionCtx>,
    conn: Option<Connection>,
    /// The relay has an audio capture running for this session.
    audio: AtomicBool,
    /// The relay has video running for this session (the cursor goes with the screen).
    screen: AtomicBool,
}

impl DirectOut {
    pub fn new(ctx: Option<SessionCtx>, conn: Option<Connection>) -> Self {
        Self { ctx, conn, audio: AtomicBool::new(false), screen: AtomicBool::new(false) }
    }

    /// The relay says what runs for the session.
    pub fn set(&self, audio: bool, screen: bool) {
        self.audio.store(audio, Ordering::Relaxed);
        self.screen.store(screen, Ordering::Relaxed);
    }

    fn allowed(&self, grant: PermissionSet) -> bool {
        self.ctx.as_ref().is_none_or(|c| c.granted().contains(grant) && !c.is_paused())
    }

    pub fn audio(&self, datagram: Vec<u8>) {
        if self.audio.load(Ordering::Relaxed)
            && self.allowed(PermissionSet::AUDIO)
            && let Some(conn) = &self.conn
            && datagram.len() <= conn.max_datagram_size().unwrap_or(0)
        {
            let _ = conn.send_datagram(datagram.into());
        }
    }

    pub fn cursor_pos(&self, pos: &CursorPosition) {
        if self.screen.load(Ordering::Relaxed)
            && self.allowed(PermissionSet::VIEW)
            && let (Some(conn), Ok(bytes)) = (&self.conn, pos.encode())
        {
            let _ = conn.send_datagram(bytes.into());
        }
    }

    pub fn cursor_shape(&self, shape: &CursorShape) {
        if self.screen.load(Ordering::Relaxed)
            && self.allowed(PermissionSet::VIEW)
            && let Some(ctx) = &self.ctx
            && let Ok(bytes) = shape.encode()
        {
            let ctx = ctx.clone();
            ctx.clone().spawn(async move {
                if let Ok(mut stream) = ctx.open_uni(StreamKind::CursorShape).await
                    && stream.write_all(&bytes).await.is_ok()
                {
                    let _ = stream.finish();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
