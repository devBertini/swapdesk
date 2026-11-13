//! The host side of a session: relays between the viewer (the QUIC session) and the helpers (the local pipes).
//!
//! One [`HostRelay`] task per granted session owns every decision about what the helper does for it: it reads the
//! levels (grants, pause) after every event, so a revocation takes effect whether or not an event carried it:
//!
//! * `VIEW` held: `StartVideo` on the display the viewer selected (the primary one by default), helper frames go
//!   to the session's [`VideoPath`] (a newest-wins slot and a task of its own that feeds the [`VideoSender`]), the
//!   sender's wants (keyframes, bitrate) go back as `ForceKeyframe` and `SetRate`. `VIEW` revoked:
//!   `set_paused(true)` and `StopVideo`.
//! * `AUDIO` held and the viewer asked (`AudioControl`): `StartAudio`; either one gone: `StopAudio`.
//! * Video frames, audio packets and cursor positions and shapes do not pass through the relay: the helper pump hands
//!   them to the session's [`VideoPath`] and [`DirectOut`] (stage 64), so a stalled video send holds nothing else.
//! * Input does not pass through the relay: the input stream's task (gated on `CONTROL`, so a revocation aborts it
//!   at once) forwards records to the helper that [`input_helper`] picks. The relay sends `ReleaseAll` when `CONTROL`
//!   goes, so nothing stays pressed.
//! * The session's end: `StopVideo`, `StopAudio`, `ReleaseAll`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use quinn::{RecvStream, VarInt};
use swapdesk_ipc::{Events, Message as Ipc, VideoBackend};
use swapdesk_platform::{Codec, Level};
use swapdesk_proto::{
    CodecError, DisplayList, ErrorCode, HostMute, InputEvent, Message as Ctl, PermissionSet, Preset, StreamKind, Wire,
};
use swapdesk_session::{ChannelHandler, Forwarded, IncomingStream, SessionCtx, SessionEnd, VIDEO_CONTROL_TYPES};
use swapdesk_video::{RateConfig, SenderConfig, VideoSender};
use tokio::sync::mpsc;

use super::helper_link::HelperLink;
use super::media_path::{DirectOut, VideoPath, VideoRun};
use super::routing::{HelperKind, InputRoute, SpecialVerdict, input_route, special_verdict};
use super::shared::{MediaEv, RelayEv, RelayHandle, Shared};

/// The message type of `SendSpecial`, as `Error.about_type` names it.
const SEND_SPECIAL_TYPE: u16 = 0x0109;
/// Messages the relay's queue holds before the helper pump waits.
const RELAY_QUEUE: usize = 512;
/// The relay's housekeeping beat (keyframe wants that no frame prompts).
const TICK: Duration = Duration::from_millis(100);
/// Bitrate of a `StartVideo` when there is no sender to ask (a test link without a connection).
const DEFAULT_KBPS: u32 = 8_000;
/// Records forwarded to the helper in one `Input` message (each is at most 259 bytes: well under 64 KiB).
const MAX_BATCH: usize = 128;
/// Bytes of an unfinished input record the stream task keeps; more is a protocol violation.
const MAX_PENDING: usize = 4 * 1024;

/// The display to stream: the viewer's choice if the host still has it, else the primary, else the first.
fn pick_display(selected: Option<u16>, displays: &[swapdesk_proto::Display]) -> Option<u16> {
    selected
        .filter(|s| displays.iter().any(|d| d.id == *s))
        .or_else(|| displays.iter().find(|d| d.primary).map(|d| d.id))
        .or_else(|| displays.first().map(|d| d.id))
}

/// Whether the streamed display `id` is not what it was when its video started: gone from the new list, or the same
/// id on another monitor (its name, place, scale or rotation differ). A size change alone is not: the capture follows
/// a new size by itself and sends a keyframe at it.
fn display_replaced(old: &[swapdesk_proto::Display], new: &[swapdesk_proto::Display], id: u16) -> bool {
    let Some(now) = new.iter().find(|d| d.id == id) else { return true };
    match old.iter().find(|d| d.id == id) {
        Some(was) => {
            was.name != now.name
                || (was.x, was.y) != (now.x, now.y)
                || was.scale_milli != now.scale_milli
                || was.rotation != now.rotation
        }
        // The old list did not hold it (an empty list from before the helper came): nothing to compare.
        None => false,
    }
}

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
        let video = VideoPath::new();
        video.set_gate(ctx.clone());
        let out = Arc::new(DirectOut::new(Some(ctx.clone()), ctx.connection().cloned()));
        self.shared.register_relay(
            ctx.session_id(),
            RelayHandle {
                tx: tx.clone(),
                ended: Arc::clone(&ended),
                video: Arc::clone(&video),
                out: Arc::clone(&out),
                ctx: ctx.clone(),
            },
        );
        let relay = HostRelay::new(Arc::clone(&self.shared), ctx.clone(), tx.clone(), ended, video, out);
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
    /// The codec picked for this session's video (F-VIEW-07).
    codec: Codec,
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
    path: Arc<VideoPath>,
    out: Arc<DirectOut>,
    audio_on: bool,
    audio: Option<Arc<HelperLink>>,
    last_grants: PermissionSet,
}

impl HostRelay {
    fn new(
        shared: Arc<Shared>,
        ctx: SessionCtx,
        tx: mpsc::Sender<RelayEv>,
        ended: Arc<AtomicBool>,
        path: Arc<VideoPath>,
        out: Arc<DirectOut>,
    ) -> Self {
        let target_kbps = Arc::new(AtomicU32::new(0));
        let sender = ctx.connection().cloned().map(|conn| {
            let (rate, wake) = (Arc::clone(&target_kbps), tx);
            let sink = move |kbps: u32| {
                rate.store(kbps, Ordering::Relaxed);
                let _ = wake.try_send(RelayEv::Wake);
            };
            // A wired link starts higher than a wireless one; both ramp to the ceiling while the link is clean.
            let rate = RateConfig::for_link(shared.is_wired(conn.local_ip()));
            VideoSender::new(conn, SenderConfig { rate, ..SenderConfig::default() }, Arc::new(sink))
        });
        // The video task lives as long as the relay: it ends when `cleanup` closes the path.
        if let Some(sender) = sender.clone() {
            shared.rt.spawn(Arc::clone(&path).run(sender));
        }
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
            path,
            out,
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<RelayEv>) {
        if !self.displays.is_empty() {
            // The session's first `DisplayList` came from `start_info`; a list that changed since is sent again.
            let _ = self.ctx.send(DisplayList { displays: self.displays.clone() }).await;
        }
        self.reconcile().await;
        self.out.set(self.audio.is_some(), self.video.is_some());
        while let Some(ev) = rx.recv().await {
            if self.ended.load(Ordering::Acquire) || self.ctx.ended().is_some() {
                break;
            }
            self.on_event(ev).await;
            self.poll_keyframe();
            self.reconcile().await;
            self.out.set(self.audio.is_some(), self.video.is_some());
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
            Ctl::SendSpecial(m) => self.send_special(m.key),
            _ => {}
        }
    }

    /// A viewer's `SendSpecial` (PROTOCOL.md 0x0109, D11): the session needs `UNATTENDED` (the session layer already
    /// refuses it otherwise with `NOT_PERMITTED`; the verdict checks again, with the desktop, at the moment of
    /// sending), and the service process sends the sequence. A refusal or a failure is an `Error` about the message,
    /// not fatal.
    fn send_special(&self, key: swapdesk_proto::Special) {
        let granted = self.ctx.granted();
        let desktop = self.ctx.desktop_state().desktop;
        let refused = |code: ErrorCode| {
            let _ = self.ctx.try_send(swapdesk_proto::ErrorMessage {
                code,
                fatal: false,
                about_type: SEND_SPECIAL_TYPE,
                about_id: 0,
            });
        };
        match special_verdict(key, granted, desktop) {
            SpecialVerdict::Refuse(code) => {
                self.shared.log(
                    Level::Warn,
                    format_args!("session {:#x}: SendSpecial refused ({code:?})", self.ctx.session_id()),
                );
                refused(code);
            }
            SpecialVerdict::Send => match self.shared.sas_sender.get() {
                Some(send) => match send() {
                    Ok(()) => self.shared.log(
                        Level::Info,
                        format_args!("session {:#x}: the secure attention sequence was sent", self.ctx.session_id()),
                    ),
                    Err(why) => {
                        self.shared.log(Level::Warn, format_args!("the secure attention sequence failed: {why}"));
                        refused(ErrorCode::Unavailable);
                    }
                },
                None => {
                    self.shared.log(
                        Level::Info,
                        format_args!("no sender for the secure attention sequence on this core: SendSpecial refused"),
                    );
                    refused(ErrorCode::Unavailable);
                }
            },
        }
    }

    async fn media(&mut self, ev: MediaEv) {
        match ev {
            MediaEv::Displays(list) => {
                // F-VIEW-02: the display on the wire may now be another monitor, or gone, or laid out anew: the video
                // starts again on the right one (a keyframe at its size follows) instead of streaming a stale source.
                if let Some(run) = &self.video
                    && display_replaced(&self.displays, &list, run.display)
                {
                    self.stop_video().await;
                }
                self.displays = list.clone();
                let _ = self.ctx.send(DisplayList { displays: list }).await;
            }
            MediaEv::HelperGone(id) => {
                if self.video.as_ref().is_some_and(|v| v.helper.id == id) {
                    self.video = None;
                    self.path.set_run(None);
                }
                if self.audio.as_ref().is_some_and(|a| a.id == id) {
                    self.audio = None;
                }
            }
            // The helper pump hands these to the session's `VideoPath` and `DirectOut` itself.
            MediaEv::Video { .. } | MediaEv::CursorPos(_) | MediaEv::CursorShape(_) | MediaEv::Audio(_) => {}
        }
    }

    /// The sender's wants (the viewer's `KeyframeRequest`, a reset backlog, a resume after a pause) become a
    /// `ForceKeyframe` to the helper.
    fn poll_keyframe(&self) {
        if let Some(sender) = &self.sender {
            self.path.poll_keyframe(sender);
        }
    }

    /// F-VIEW-07: HEVC for this viewer only when both machines have it in hardware and the viewer's minor knows it.
    fn session_codec(&self) -> Codec {
        super::session_codec(
            self.ctx.peer_capabilities().as_ref(),
            self.ctx.peer_minor(),
            self.shared.host_hevc_hardware.load(Ordering::Relaxed),
        )
    }

    fn fps(&self) -> u16 {
        let refresh =
            self.pick_display().and_then(|id| self.displays.iter().find(|d| d.id == id)).map_or(0, |d| d.refresh_mhz);
        crate::scale::target_fps(self.preset, self.max_fps, refresh)
    }

    /// The display to stream: the viewer's choice if the host still has it, else the primary, else the first.
    fn pick_display(&self) -> Option<u16> {
        pick_display(self.selected, &self.displays)
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
        let codec = self.session_codec();
        let same = match (&self.video, want) {
            (Some(run), Some((helper, display))) => {
                run.helper.id == helper.id
                    && run.display == display
                    && run.fps == fps
                    && run.preset == preset
                    && run.codec == codec
            }
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.stop_video().await;
            if want.is_none() && view {
                self.shared.log(
                    Level::Debug,
                    format_args!(
                        "session {:#x}: VIEW held but no video: media helper {}, displays {}",
                        self.ctx.session_id(),
                        media.is_some(),
                        self.displays.len()
                    ),
                );
            }
            if let Some((helper, display)) = want {
                let kbps = self.sender.as_ref().map_or(DEFAULT_KBPS, VideoSender::target_kbps);
                let started = helper
                    .send(Ipc::StartVideo { display, fps, kbps, quality: preset, backend: VideoBackend::Auto, codec })
                    .await;
                self.shared.log(
                    Level::Debug,
                    format_args!(
                        "session {:#x}: StartVideo display {display} fps {fps} codec {codec:?} sent: {started}",
                        self.ctx.session_id()
                    ),
                );
                if started {
                    self.video =
                        Some(RunVideo { helper: Arc::clone(helper), display, fps, preset, codec, kbps_sent: kbps });
                    let (link, rt) = (Arc::clone(helper), self.shared.rt.clone());
                    // A keyframe is asked for from the video task, which must not wait on the helper's queue.
                    let key = Arc::new(move || {
                        let link = Arc::clone(&link);
                        rt.spawn(async move {
                            link.send(Ipc::ForceKeyframe { display }).await;
                        });
                    });
                    self.path.set_run(Some(VideoRun { display, key }));
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
        self.path.set_run(None);
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
        self.out.set(false, false);
        self.path.close();
        self.shared.log(
            Level::Debug,
            format_args!("session {:#x}: video path at its end: {:?}", self.ctx.session_id(), self.path.stats()),
        );
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
    let mut state = InputState::default();
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
                        deliver(&shared, &ctx, std::mem::take(&mut batch), &mut state).await;
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
        deliver(&shared, &ctx, batch, &mut state).await;
    }
}

/// What the input task remembers between batches.
#[derive(Default)]
struct InputState {
    /// The helper that got the last batch.
    last: Option<HelperKind>,
    /// The helper link that was last given the media helper's displays in this session.
    listed: Option<u64>,
    warned: bool,
    /// A batch was dropped (paused host, no helper, a closed pipe): the next one that goes through is preceded by a
    /// `ReleaseAll`, since the dropped batch may have held the key-up of a key that is down at the helper (D9).
    dropped: bool,
}

async fn deliver(shared: &Shared, ctx: &SessionCtx, events: Vec<InputEvent>, st: &mut InputState) {
    if events.is_empty() {
        return;
    }
    if !ctx.can_inject() {
        // Not granted any more, or the host is paused (D11, F-SESS-01): the records are dropped.
        st.dropped = true;
        return;
    }
    // Which helper takes the records on the desktop the host is on (D11, F-UNAT-01): a protected desktop only from the
    // system helper and only for `UNATTENDED`; nothing while another account is at the console.
    let kind = match input_route(ctx.granted(), ctx.desktop_state().desktop) {
        InputRoute::Helper(kind) => kind,
        InputRoute::Drop => {
            st.dropped = true;
            return;
        }
    };
    let Some(helper) = shared.helper(kind) else {
        // Never the other helper: a prompted session must not reach the SYSTEM injector (D11).
        st.dropped = true;
        if !st.warned {
            st.warned = true;
            shared.log(
                Level::Warn,
                format_args!("no {kind:?} helper attached: input of session {:#x} is dropped", ctx.session_id()),
            );
        }
        return;
    };
    if let Some(prev) = st.last
        && prev != kind
        && let Some(old) = shared.helper(prev)
    {
        // The injector changed (UNATTENDED gained or lost): the old one lets go of everything it holds.
        old.send(Ipc::ReleaseAll).await;
    }
    st.last = Some(kind);
    // D64: each session maps its pointer onto the displays of now, the ones its pictures show. A helper that is not
    // the media one gets them before its first batch; later changes reach it from the pump.
    if st.listed != Some(helper.id) {
        st.listed = Some(helper.id);
        if let Some((_, media)) = shared.media()
            && media.id != helper.id
        {
            helper.send(Ipc::Displays(DisplayList { displays: media.displays() })).await;
        }
    }
    if std::mem::take(&mut st.dropped) {
        helper.send(Ipc::ReleaseAll).await;
    }
    if !helper.send(Ipc::Input { events: Events(events) }).await {
        st.dropped = true;
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use swapdesk_proto::Display;

    fn display(id: u16, x: i32, scale_milli: u16, primary: bool) -> Display {
        Display {
            id,
            name: format!("Display {id}"),
            x,
            y: 0,
            width: 1920,
            height: 1080,
            scale_milli,
            refresh_mhz: 60_000,
            rotation: 0,
            primary,
        }
    }

    #[test]
    fn the_viewers_choice_wins_while_the_host_has_it_else_the_primary_else_the_first() {
        let list = [display(0, 0, 1000, false), display(1, 1920, 2000, true), display(2, 3840, 1500, false)];
        assert_eq!(pick_display(None, &list), Some(1), "the primary by default");
        assert_eq!(pick_display(Some(2), &list), Some(2));
        assert_eq!(pick_display(Some(9), &list), Some(1), "a display that is gone falls back to the primary");
        let no_primary = [display(3, 0, 1000, false), display(4, 100, 1000, false)];
        assert_eq!(pick_display(Some(9), &no_primary), Some(3));
        assert_eq!(pick_display(None, &[]), None);
    }

    #[test]
    fn a_streamed_display_is_replaced_when_gone_or_another_monitor_but_not_for_a_size_change() {
        let old = [display(0, 0, 1000, true), display(1, 1920, 2000, false)];
        assert!(!display_replaced(&old, &old, 1), "an unchanged list");
        assert!(display_replaced(&old, &old[..1], 1), "gone");
        let moved = [display(0, 0, 1000, true), display(1, 2560, 2000, false)];
        assert!(display_replaced(&old, &moved, 1), "another place");
        let rescaled = [display(0, 0, 1000, true), display(1, 1920, 1500, false)];
        assert!(display_replaced(&old, &rescaled, 1), "another scale");
        let mut other = old.clone();
        other[1].name = "Other".into();
        assert!(display_replaced(&old, &other, 1), "another name");
        let mut resized = old.clone();
        resized[1].width = 2560;
        resized[1].primary = true;
        assert!(!display_replaced(&old, &resized, 1), "the capture follows a size change itself");
        assert!(!display_replaced(&old[..1], &old, 1), "no earlier entry to compare");
        assert!(!display_replaced(&old, &old, 0), "another display's entry is not this one's");
    }
}
