//! The host side of a session: relays between the viewer (the QUIC session) and the helpers (the local pipes).
//!
//! One [`HostRelay`] task per granted session owns every decision about what the helper does for it: it reads the
//! levels (grants, pause) after every event, so a revocation takes effect whether or not an event carried it:
//!
//! * `VIEW` held: `StartVideo` on the display the viewer selected (the primary one by default), helper frames go
//!   to the session's [`VideoSender`], the sender's wants (keyframes, bitrate) go back as `ForceKeyframe` and
//!   `SetRate`, cursor positions and shapes go to the viewer. `VIEW` revoked: `set_paused(true)` and `StopVideo`.
//! * `AUDIO` held and the viewer asked (`AudioControl`): `StartAudio`, helper packets go out as QUIC datagrams;
//!   either one gone: `StopAudio`.
//! * Input does not pass through the relay: the input stream's task (gated on `CONTROL`, so a revocation aborts it
//!   at once) forwards records to the helper that [`input_helper`] picks. The relay sends `ReleaseAll` when `CONTROL`
//!   goes, so nothing stays pressed.
//! * The session's end: `StopVideo`, `StopAudio`, `ReleaseAll`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use quinn::{RecvStream, VarInt};
use swapdesk_ipc::{Events, Message as Ipc, VideoBackend};
use swapdesk_platform::Level;
use swapdesk_proto::{
    CodecError, DisplayList, ErrorCode, HostMute, InputEvent, Message as Ctl, PermissionSet, Preset, StreamKind,
    VideoHeader, Wire,
};
use swapdesk_session::{ChannelHandler, Forwarded, IncomingStream, SessionCtx, SessionEnd, VIDEO_CONTROL_TYPES};
use swapdesk_video::{Frame, SenderConfig, VideoSender};
use tokio::sync::mpsc;

use super::helper_link::HelperLink;
use super::routing::{HelperKind, input_helper};
use super::shared::{MediaEv, RelayEv, RelayHandle, Shared};

/// Messages the relay's queue holds before the helper pump waits.
const RELAY_QUEUE: usize = 512;
/// The relay's housekeeping beat (keyframe wants that no frame prompts).
const TICK: Duration = Duration::from_millis(100);
/// How long P-frames are held back after a `ForceKeyframe` before it is asked again.
const KEY_WAIT: Duration = Duration::from_millis(500);
/// Bitrate of a `StartVideo` when there is no sender to ask (a test link without a connection).
const DEFAULT_KBPS: u32 = 8_000;
/// Records forwarded to the helper in one `Input` message (each is at most 259 bytes: well under 64 KiB).
const MAX_BATCH: usize = 128;
/// Bytes of an unfinished input record the stream task keeps; more is a protocol violation.
const MAX_PENDING: usize = 4 * 1024;

fn code(c: ErrorCode) -> VarInt {
    VarInt::from_u32(u32::from(c.tag()))
}

/// The registry entry of the host: control messages of the video pipeline, the input stream, and the lifecycle of
/// each session's relay.
pub(crate) struct HostRelays {
    pub shared: Arc<Shared>,
}

impl HostRelays {
    fn wake(&self, ctx: &SessionCtx) {
        if let Some(h) = self.shared.relay(ctx.session_id()) {
            let _ = h.tx.try_send(RelayEv::Wake);
        }
    }
}

impl ChannelHandler for HostRelays {
    fn name(&self) -> &'static str {
        "host-relay"
    }

    fn messages(&self) -> &'static [u16] {
        VIDEO_CONTROL_TYPES
    }

    fn streams(&self) -> &'static [StreamKind] {
        &[StreamKind::Input]
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Ctl) {
        if let Some(h) = self.shared.relay(ctx.session_id()) {
            // A full queue loses the excess, as `ForwardHandler` does: a viewer that floods does not hold the session.
            let _ = h.tx.try_send(RelayEv::Control(Forwarded { session: ctx.clone(), msg }));
        }
    }

    fn on_stream(&self, ctx: &SessionCtx, stream: IncomingStream) {
        // Only `Input` is registered. The task needs `CONTROL` now and stops the moment the session loses it.
        let task = input_task(Arc::clone(&self.shared), ctx.clone(), stream.recv);
        if !ctx.spawn_gated(PermissionSet::CONTROL, task) {
            self.shared
                .log(Level::Debug, format_args!("input stream of session {:#x} refused: no CONTROL", ctx.session_id()));
        }
    }

    fn on_permissions(&self, ctx: &SessionCtx, _old: PermissionSet, _now: PermissionSet) {
        self.wake(ctx);
    }

    fn on_state(&self, ctx: &SessionCtx, _desktop: swapdesk_proto::Desktop, _paused: bool) {
        self.wake(ctx);
    }

    fn on_start(&self, ctx: &SessionCtx) {
        let (tx, rx) = mpsc::channel(RELAY_QUEUE);
        let ended = Arc::new(AtomicBool::new(false));
        self.shared.register_relay(ctx.session_id(), RelayHandle { tx: tx.clone(), ended: Arc::clone(&ended) });
        let relay = HostRelay::new(Arc::clone(&self.shared), ctx.clone(), tx.clone(), ended);
        self.shared.rt.spawn(relay.run(rx));
        self.shared.rt.spawn(ticker(tx));
    }

    fn on_end(&self, ctx: &SessionCtx, _end: &SessionEnd) {
        if let Some(h) = self.shared.relay(ctx.session_id()) {
            h.ended.store(true, Ordering::Release);
            let _ = h.tx.try_send(RelayEv::Wake);
        }
    }
}

async fn ticker(tx: mpsc::Sender<RelayEv>) {
    let mut beat = tokio::time::interval(TICK);
    loop {
        beat.tick().await;
        if matches!(tx.try_send(RelayEv::Tick), Err(mpsc::error::TrySendError::Closed(_))) {
            return;
        }
    }
}

struct RunVideo {
    helper: Arc<HelperLink>,
    display: u16,
    fps: u16,
    preset: Preset,
    kbps_sent: u32,
}

struct HostRelay {
    shared: Arc<Shared>,
    ctx: SessionCtx,
    ended: Arc<AtomicBool>,
    sender: Option<VideoSender>,
    target_kbps: Arc<AtomicU32>,
    displays: Vec<swapdesk_proto::Display>,
    selected: Option<u16>,
    preset: Preset,
    max_fps: u16,
    video: Option<RunVideo>,
    audio_on: bool,
    audio: Option<Arc<HelperLink>>,
    awaiting_key: Option<Instant>,
    last_grants: PermissionSet,
}

impl HostRelay {
    fn new(shared: Arc<Shared>, ctx: SessionCtx, tx: mpsc::Sender<RelayEv>, ended: Arc<AtomicBool>) -> Self {
        let target_kbps = Arc::new(AtomicU32::new(0));
        let sender = ctx.connection().cloned().map(|conn| {
            let (rate, wake) = (Arc::clone(&target_kbps), tx);
            let sink = move |kbps: u32| {
                rate.store(kbps, Ordering::Relaxed);
                let _ = wake.try_send(RelayEv::Wake);
            };
            VideoSender::new(conn, SenderConfig::default(), Arc::new(sink))
        });
        Self {
            displays: shared.media_displays(),
            last_grants: ctx.granted(),
            shared,
            ctx,
            ended,
            sender,
            target_kbps,
            selected: None,
            preset: Preset::Auto,
            max_fps: 0,
            video: None,
            audio_on: false,
            audio: None,
            awaiting_key: None,
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<RelayEv>) {
        if !self.displays.is_empty() {
            // The session's first `DisplayList` came from `start_info`; a list that changed since is sent again.
            let _ = self.ctx.send(DisplayList { displays: self.displays.clone() }).await;
        }
        self.reconcile().await;
        while let Some(ev) = rx.recv().await {
            if self.ended.load(Ordering::Acquire) || self.ctx.ended().is_some() {
                break;
            }
            self.on_event(ev).await;
            self.poll_keyframe().await;
            self.reconcile().await;
        }
        self.cleanup().await;
    }

    async fn on_event(&mut self, ev: RelayEv) {
        match ev {
            RelayEv::Control(f) => self.control(f),
            RelayEv::Media(m) => self.media(m).await,
            RelayEv::HostMute(muted) => {
                let _ = self.ctx.send(HostMute { muted }).await;
            }
            RelayEv::Wake | RelayEv::Tick => {}
        }
    }

    fn control(&mut self, f: Forwarded) {
        // The message may have waited in the queue while its permission was taken away (F-PERM-02).
        if !f.still_permitted() {
            return;
        }
        match f.msg {
            Ctl::SelectDisplay(m) => {
                if self.displays.iter().any(|d| d.id == m.display) {
                    self.selected = Some(m.display);
                }
            }
            Ctl::VideoSettings(m) => {
                self.preset = m.preset;
                self.max_fps = m.max_fps;
                if let Some(s) = &self.sender {
                    s.set_bitrate_cap(m.max_kbps);
                }
            }
            Ctl::KeyframeRequest(r) => {
                if let Some(s) = &self.sender {
                    s.on_keyframe_request(&r);
                }
            }
            Ctl::ReceiverReport(r) => {
                if let Some(s) = &self.sender {
                    s.on_receiver_report(&r);
                }
            }
            Ctl::AudioControl(c) => self.audio_on = c.enabled,
            Ctl::SendSpecial(_) => self.shared.log(Level::Info, format_args!("SendSpecial is not wired in this build")),
            _ => {}
        }
    }

    async fn media(&mut self, ev: MediaEv) {
        match ev {
            MediaEv::Displays(list) => {
                self.displays = list.clone();
                let _ = self.ctx.send(DisplayList { displays: list }).await;
            }
            MediaEv::HelperGone(id) => {
                if self.video.as_ref().is_some_and(|v| v.helper.id == id) {
                    self.video = None;
                    self.awaiting_key = None;
                }
                if self.audio.as_ref().is_some_and(|a| a.id == id) {
                    self.audio = None;
                }
            }
            MediaEv::Video { header, payload } => self.video_frame(header, payload).await,
            MediaEv::CursorPos(pos) => {
                if self.shows_screen()
                    && let (Some(conn), Ok(bytes)) = (self.ctx.connection(), pos.encode())
                {
                    let _ = conn.send_datagram(bytes.into());
                }
            }
            MediaEv::CursorShape(shape) => {
                if self.shows_screen() {
                    let ctx = self.ctx.clone();
                    self.ctx.spawn(async move {
                        let Ok(bytes) = shape.encode() else { return };
                        if let Ok(mut stream) = ctx.open_uni(StreamKind::CursorShape).await
                            && stream.write_all(&bytes).await.is_ok()
                        {
                            let _ = stream.finish();
                        }
                    });
                }
            }
            MediaEv::Audio(datagram) => {
                if self.audio.is_some()
                    && self.ctx.granted().contains(PermissionSet::AUDIO)
                    && !self.ctx.is_paused()
                    && let Some(conn) = self.ctx.connection()
                    && datagram.len() <= conn.max_datagram_size().unwrap_or(0)
                {
                    let _ = conn.send_datagram(datagram.into());
                }
            }
        }
    }

    fn shows_screen(&self) -> bool {
        self.video.is_some() && self.ctx.granted().contains(PermissionSet::VIEW) && !self.ctx.is_paused()
    }

    async fn video_frame(&mut self, header: VideoHeader, payload: Vec<u8>) {
        let (Some(sender), Some(run)) = (self.sender.clone(), &self.video) else { return };
        if header.display_id != run.display {
            return;
        }
        if let Some(since) = self.awaiting_key {
            if header.keyframe || header.discontinuity {
                self.awaiting_key = None;
            } else {
                // A keyframe was asked for and has not come: what follows would not decode at the viewer. Ask again
                // when it takes too long.
                if since.elapsed() > KEY_WAIT {
                    let (helper, display) = (Arc::clone(&run.helper), run.display);
                    self.awaiting_key = Some(Instant::now());
                    helper.send(Ipc::ForceKeyframe { display }).await;
                }
                return;
            }
        }
        let _ = sender.send(Frame { header, payload }).await;
    }

    /// The sender's wants (the viewer's `KeyframeRequest`, a reset backlog, a resume after a pause) become a
    /// `ForceKeyframe` to the helper. The sender spaces them itself (`min_keyframe_interval`).
    async fn poll_keyframe(&mut self) {
        let (Some(sender), Some(run)) = (&self.sender, &self.video) else { return };
        if self.awaiting_key.is_none() && sender.take_keyframe_request(run.display) {
            let (helper, display) = (Arc::clone(&run.helper), run.display);
            self.awaiting_key = Some(Instant::now());
            helper.send(Ipc::ForceKeyframe { display }).await;
        }
    }

    fn fps(&self) -> u16 {
        if self.max_fps == 0 { 60 } else { self.max_fps.clamp(1, 240) }
    }

    /// The display to stream: the viewer's choice if the host still has it, else the primary, else the first.
    fn pick_display(&self) -> Option<u16> {
        self.selected
            .filter(|s| self.displays.iter().any(|d| d.id == *s))
            .or_else(|| self.displays.iter().find(|d| d.primary).map(|d| d.id))
            .or_else(|| self.displays.first().map(|d| d.id))
    }

    /// Brings the helper's work in line with the session's grants and the viewer's choices.
    async fn reconcile(&mut self) {
        let granted = self.ctx.granted();
        let view = granted.contains(PermissionSet::VIEW);
        if let Some(s) = &self.sender {
            s.set_paused(self.ctx.is_paused() || !view);
        }
        if self.last_grants.contains(PermissionSet::CONTROL) && !granted.contains(PermissionSet::CONTROL) {
            self.release_all().await;
        }
        self.last_grants = granted;

        let media = self.shared.media().map(|(_, h)| h);
        self.reconcile_video(view, media.as_ref()).await;
        self.reconcile_audio(granted.contains(PermissionSet::AUDIO), media).await;
    }

    async fn reconcile_video(&mut self, view: bool, media: Option<&Arc<HelperLink>>) {
        let want = if view { media.zip(self.pick_display()) } else { None };
        let (fps, preset) = (self.fps(), self.preset);
        let same = match (&self.video, want) {
            (Some(run), Some((helper, display))) => {
                run.helper.id == helper.id && run.display == display && run.fps == fps && run.preset == preset
            }
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.stop_video().await;
            if let Some((helper, display)) = want {
                let kbps = self.sender.as_ref().map_or(DEFAULT_KBPS, VideoSender::target_kbps);
                let started = helper
                    .send(Ipc::StartVideo { display, fps, kbps, quality: preset, backend: VideoBackend::Auto })
                    .await;
                if started {
                    self.video = Some(RunVideo { helper: Arc::clone(helper), display, fps, preset, kbps_sent: kbps });
                    self.awaiting_key = None;
                }
            }
        }
        // The rate control's target goes to the encoder.
        let target = self.target_kbps.load(Ordering::Relaxed);
        if let Some(run) = self.video.as_mut()
            && target != 0
            && target != run.kbps_sent
        {
            run.kbps_sent = target;
            let msg = Ipc::SetRate { display: run.display, kbps: target, fps: run.fps };
            let helper = Arc::clone(&run.helper);
            helper.send(msg).await;
        }
    }

    async fn reconcile_audio(&mut self, granted: bool, media: Option<Arc<HelperLink>>) {
        let want = self.audio_on && granted;
        let current = self.audio.as_ref().map(|a| a.id);
        match (want, media) {
            (true, Some(helper)) if current != Some(helper.id) => {
                self.stop_audio().await;
                if helper.send(Ipc::StartAudio { exclude_pid: self.shared.audio_exclude_pid }).await {
                    self.audio = Some(helper);
                }
            }
            (true, Some(_)) => {}
            (true, None) => self.audio = None,
            (false, _) => self.stop_audio().await,
        }
    }

    async fn stop_video(&mut self) {
        if let Some(run) = self.video.take() {
            run.helper.send(Ipc::StopVideo { display: run.display }).await;
        }
        self.awaiting_key = None;
    }

    async fn stop_audio(&mut self) {
        if let Some(helper) = self.audio.take() {
            helper.send(Ipc::StopAudio).await;
        }
    }

    /// `ReleaseAll` to every attached helper: nothing may stay pressed on either.
    async fn release_all(&self) {
        for kind in [HelperKind::System, HelperKind::User] {
            if let Some(h) = self.shared.helper(kind) {
                h.send(Ipc::ReleaseAll).await;
            }
        }
    }

    async fn cleanup(&mut self) {
        self.stop_video().await;
        self.stop_audio().await;
        self.release_all().await;
        if let Some(sender) = &self.sender {
            sender.set_paused(true);
        }
        self.shared.unregister_relay(self.ctx.session_id());
    }
}

/// Reads the input stream's records (PROTOCOL.md 7.3) and forwards them, in order, to the helper that serves the
/// session's input. Runs gated on `CONTROL`: the session aborts it the moment the grant goes.
async fn input_task(shared: Arc<Shared>, ctx: SessionCtx, mut recv: RecvStream) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let mut last: Option<HelperKind> = None;
    let mut warned = false;
    loop {
        let n = match recv.read(&mut chunk).await {
            Ok(Some(n)) => n,
            // The viewer finished the stream, or the stream or connection broke.
            _ => return,
        };
        pending.extend_from_slice(&chunk[..n]);
        let mut batch: Vec<InputEvent> = Vec::new();
        let mut used_total = 0;
        loop {
            match InputEvent::decode_prefix(&pending[used_total..]) {
                Ok((event, used)) => {
                    used_total += used;
                    batch.push(event);
                    if batch.len() >= MAX_BATCH {
                        deliver(&shared, &ctx, std::mem::take(&mut batch), &mut last, &mut warned).await;
                    }
                }
                Err(CodecError::Truncated) => break,
                Err(_) => {
                    let _ = recv.stop(code(ErrorCode::ProtocolViolation));
                    ctx.bye(ErrorCode::ProtocolViolation);
                    return;
                }
            }
        }
        pending.drain(..used_total);
        if pending.len() > MAX_PENDING {
            let _ = recv.stop(code(ErrorCode::ProtocolViolation));
            ctx.bye(ErrorCode::ProtocolViolation);
            return;
        }
        deliver(&shared, &ctx, batch, &mut last, &mut warned).await;
    }
}

async fn deliver(
    shared: &Shared,
    ctx: &SessionCtx,
    events: Vec<InputEvent>,
    last: &mut Option<HelperKind>,
    warned: &mut bool,
) {
    if events.is_empty() || !ctx.can_inject() {
        // Not granted any more, or the host is paused (D11, F-SESS-01): the records are dropped.
        return;
    }
    let kind = input_helper(ctx.granted());
    let Some(helper) = shared.helper(kind) else {
        // Never the other helper: a prompted session must not reach the SYSTEM injector (D11).
        if !*warned {
            *warned = true;
            shared.log(
                Level::Warn,
                format_args!("no {kind:?} helper attached: input of session {:#x} is dropped", ctx.session_id()),
            );
        }
        return;
    };
    if let Some(prev) = *last
        && prev != kind
        && let Some(old) = shared.helper(prev)
    {
        // The injector changed (UNATTENDED gained or lost): the old one lets go of everything it holds.
        old.send(Ipc::ReleaseAll).await;
    }
    *last = Some(kind);
    helper.send(Ipc::Input { events: Events(events) }).await;
}
