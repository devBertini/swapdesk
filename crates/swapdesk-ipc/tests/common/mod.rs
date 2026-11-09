//! Value strategies shared by `roundtrip.rs` (the regular proof) and `fuzz.rs` (the same properties at
//! fuzzing scale).

#![allow(dead_code, unused_imports)]

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::Union;
use swapdesk_ipc::*;
use swapdesk_proto::{
    Codec, CursorPosition, CursorShape, CursorShapeHeader, Desktop, Display, DisplayList, ErrorCode, Feature,
    InputEvent, Intent, KeyframeRequest, MouseButton, Os, Preset, ReceiverReport, VideoHeader,
};

pub fn txt(max: usize) -> BoxedStrategy<String> {
    proptest::string::string_regex(&format!("[a-z0-9 ]{{0,{max}}}")).unwrap().boxed()
}

pub fn pick<T: Clone + core::fmt::Debug + 'static>(all: &'static [T]) -> BoxedStrategy<T> {
    select(all).boxed()
}

pub fn bytes(max: usize) -> BoxedStrategy<Bytes> {
    vec(any::<u8>(), 0..max).prop_map(Bytes).boxed()
}

pub fn event() -> BoxedStrategy<InputEvent> {
    prop_oneof![
        (prop_oneof![Just(0x07u16), Just(0x0Cu16)], any::<u16>(), any::<bool>(), any::<u8>(), 0u8..8).prop_map(
            |(usage_page, usage, down, modifiers, locks)| InputEvent::Key { usage_page, usage, down, modifiers, locks }
        ),
        txt(40).prop_map(InputEvent::Text),
        (any::<u16>(), any::<u16>(), any::<u16>()).prop_map(|(display, x, y)| InputEvent::Pointer { display, x, y }),
        (pick(MouseButton::ALL), any::<bool>()).prop_map(|(button, down)| InputEvent::Button { button, down }),
        (any::<i16>(), any::<i16>()).prop_map(|(dx, dy)| InputEvent::Wheel { dx, dy }),
        Just(InputEvent::ReleaseAll),
    ]
    .boxed()
}

pub fn events() -> BoxedStrategy<Events> {
    vec(event(), 0..20).prop_map(Events).boxed()
}

pub fn header_with(payload: Vec<u8>) -> BoxedStrategy<(VideoHeader, Bytes)> {
    (
        (any::<bool>(), any::<bool>(), any::<bool>()),
        any::<u16>(),
        any::<u32>(),
        any::<u64>(),
        any::<u32>(),
        any::<u16>(),
        any::<u16>(),
    )
        .prop_map(
            move |((keyframe, config, discontinuity), display_id, frame_id, capture_us, encode_us, width, height)| {
                (
                    VideoHeader {
                        keyframe,
                        config,
                        discontinuity,
                        display_id,
                        frame_id,
                        capture_us,
                        encode_us,
                        width,
                        height,
                        codec: Codec::H264,
                        payload_len: payload.len() as u32,
                    },
                    Bytes(payload.clone()),
                )
            },
        )
        .boxed()
}

pub fn video() -> BoxedStrategy<(VideoHeader, Bytes)> {
    vec(any::<u8>(), 0..300).prop_flat_map(header_with).boxed()
}

pub fn cursor_pos() -> BoxedStrategy<CursorPosition> {
    (any::<bool>(), any::<u16>(), any::<u32>(), any::<u16>(), any::<u16>(), any::<u32>())
        .prop_map(|(visible, display, seq, x, y, shape_id)| CursorPosition { visible, display, seq, x, y, shape_id })
        .boxed()
}

pub fn shape() -> BoxedStrategy<Shape> {
    (0u16..6, 0u16..6, any::<u32>(), any::<u16>(), any::<u16>())
        .prop_flat_map(|(width, height, shape_id, hot_x, hot_y)| {
            vec(any::<u8>(), usize::from(width) * usize::from(height) * 4).prop_map(move |rgba| {
                Shape(CursorShape { header: CursorShapeHeader { shape_id, width, height, hot_x, hot_y }, rgba })
            })
        })
        .boxed()
}

pub fn display() -> BoxedStrategy<Display> {
    (
        any::<u16>(),
        txt(64),
        any::<i32>(),
        any::<i32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u16>(),
        any::<u32>(),
        any::<u16>(),
        any::<bool>(),
    )
        .prop_map(|(id, name, x, y, width, height, scale_milli, refresh_mhz, rotation, primary)| Display {
            id,
            name,
            x,
            y,
            width,
            height,
            scale_milli,
            refresh_mhz,
            rotation,
            primary,
        })
        .boxed()
}

pub fn device_status() -> BoxedStrategy<DeviceStatus> {
    prop_oneof![
        (txt(45), any::<u32>()).prop_map(|(address, rtt_ms)| DeviceStatus::Online { address, rtt_ms }),
        Just(DeviceStatus::Asleep),
        Just(DeviceStatus::Offline),
        any::<bool>().prop_map(|pairing_open| DeviceStatus::OnNetworkUnpaired { pairing_open }),
    ]
    .boxed()
}

pub fn device() -> BoxedStrategy<DeviceView> {
    let summary = (any::<u64>(), any::<u32>(), pick(Direction::ALL), pick(Desktop::ALL))
        .prop_map(|(at_ms, duration_s, direction, desktop)| SessionSummary { at_ms, duration_s, direction, desktop });
    (
        (txt(64), txt(64), pick(Os::ALL), txt(64), device_status()),
        any::<bool>(),
        any::<u16>(),
        proptest::option::of(any::<u16>()),
        proptest::option::of(summary),
        any::<bool>(),
    )
        .prop_map(|((id, name, os, os_version, status), paired, grants_here, grants_there, last_session, wakeable)| {
            DeviceView { id, name, os, os_version, status, paired, grants_here, grants_there, last_session, wakeable }
        })
        .boxed()
}

pub fn network() -> BoxedStrategy<NetworkStatus> {
    let notice = prop_oneof![
        Just(Notice::PublicNetwork),
        Just(Notice::FirewallBlocks),
        Just(Notice::ServiceNeedsApproval),
        vec(pick(OsPermission::ALL), 0..4).prop_map(|mut v| {
            v.truncate(3);
            Notice::MacPermissionsMissing(v)
        }),
    ];
    (txt(128), proptest::option::of(txt(45)), proptest::option::of(pick(NetCategoryView::ALL)), vec(notice, 0..4))
        .prop_map(|(label, address, category, notices)| NetworkStatus { label, address, category, notices })
        .boxed()
}

pub fn settings() -> BoxedStrategy<Settings> {
    (
        (txt(64), pick(Language::ALL), any::<bool>(), any::<bool>(), any::<bool>()),
        any::<bool>(),
        pick(Preset::ALL),
        any::<bool>(),
        pick(KeyboardMode::ALL),
        any::<bool>(),
    )
        .prop_map(
            |(
                (name, language, open_at_login, service_enabled, lock_on_end),
                keep_awake,
                quality,
                show_stats,
                keyboard_mode,
                swap_ctrl_cmd,
            )| Settings {
                name,
                language,
                open_at_login,
                service_enabled,
                lock_on_end,
                keep_awake,
                quality,
                show_stats,
                keyboard_mode,
                swap_ctrl_cmd,
            },
        )
        .boxed()
}

pub fn audit() -> BoxedStrategy<AuditView> {
    (
        any::<u64>(),
        txt(64),
        any::<u16>(),
        vec(txt(128).prop_map(Text), 0..5),
        proptest::option::of(any::<u32>()),
        any::<bool>(),
    )
        .prop_map(|(at_ms, device_name, key, args, duration_s, alert)| AuditView {
            at_ms,
            device_name,
            what: Msg { key, args },
            duration_s,
            alert,
        })
        .boxed()
}

pub fn api_code() -> BoxedStrategy<ApiCode> {
    pick(ApiCode::ALL)
}

pub fn op() -> BoxedStrategy<UiOp> {
    let ops: Vec<BoxedStrategy<UiOp>> = vec![
        Just(UiOp::GetThisDevice).boxed(),
        Just(UiOp::GetNetwork).boxed(),
        Just(UiOp::ListDevices).boxed(),
        txt(15).prop_map(|ipv4| UiOp::AddByAddress { ipv4 }).boxed(),
        proptest::option::of(txt(64)).prop_map(|device_id| UiOp::PairOpen { device_id }).boxed(),
        (any::<u32>(), txt(64)).prop_map(|(pairing_id, device_id)| UiOp::PairStart { pairing_id, device_id }).boxed(),
        (any::<u32>(), any::<bool>())
            .prop_map(|(pairing_id, matches)| UiOp::PairConfirm { pairing_id, matches })
            .boxed(),
        any::<u32>().prop_map(|pairing_id| UiOp::PairCancel { pairing_id }).boxed(),
        (txt(64), any::<u16>()).prop_map(|(device_id, grants)| UiOp::SetGrants { device_id, grants }).boxed(),
        txt(64).prop_map(|device_id| UiOp::ForgetDevice { device_id }).boxed(),
        (txt(64), pick(Intent::ALL), any::<bool>())
            .prop_map(|(device_id, intent, unattended)| UiOp::Connect { device_id, intent, unattended })
            .boxed(),
        any::<u64>().prop_map(|session_id| UiOp::Disconnect { session_id }).boxed(),
        (any::<u32>(), pick(Decision::ALL), any::<u16>())
            .prop_map(|(request_id, decision, grants)| UiOp::AnswerIncoming { request_id, decision, grants })
            .boxed(),
        (any::<u64>(), any::<u16>())
            .prop_map(|(session_id, display)| UiOp::SelectDisplay { session_id, display })
            .boxed(),
        (any::<u64>(), pick(Preset::ALL))
            .prop_map(|(session_id, quality)| UiOp::SetQuality { session_id, quality })
            .boxed(),
        (any::<u64>(), any::<bool>()).prop_map(|(session_id, on)| UiOp::SetAudio { session_id, on }).boxed(),
        (any::<u64>(), any::<bool>()).prop_map(|(session_id, on)| UiOp::SetViewOnly { session_id, on }).boxed(),
        (any::<u64>(), pick(SpecialKeys::ALL))
            .prop_map(|(session_id, keys)| UiOp::SendKeys { session_id, keys })
            .boxed(),
        Just(UiOp::GetSettings).boxed(),
        (txt(64), any::<bool>(), any::<bool>(), any::<bool>())
            .prop_map(|(name, service_enabled, lock_on_end, keep_awake)| {
                UiOp::SetSettings(SettingsUpdate { name, service_enabled, lock_on_end, keep_awake })
            })
            .boxed(),
        (proptest::option::of(any::<u64>()), any::<u32>())
            .prop_map(|(before_ms, limit)| UiOp::ActivityList { before_ms, limit })
            .boxed(),
        any::<u64>().prop_map(|session_id| UiOp::RequestSwap { session_id }).boxed(),
        (any::<u32>(), any::<bool>()).prop_map(|(request_id, accept)| UiOp::AnswerSwap { request_id, accept }).boxed(),
        (any::<u64>(), txt(4096))
            .prop_map(|(session_id, text)| UiOp::ChatSend { session_id, text: Text(text) })
            .boxed(),
        any::<u64>().prop_map(|session_id| UiOp::ChatMarkRead { session_id }).boxed(),
        (txt(64), pick(Intent::ALL)).prop_map(|(device_id, intent)| UiOp::Wake { device_id, intent }).boxed(),
        any::<u64>().prop_map(|session_id| UiOp::ListDisplays { session_id }).boxed(),
    ];
    Union::new(ops).boxed()
}

pub fn display_view() -> BoxedStrategy<DisplayView> {
    (any::<u16>(), txt(64), any::<u32>(), any::<u32>(), any::<u16>(), any::<bool>())
        .prop_map(|(id, name, width, height, scale_milli, primary)| DisplayView {
            id,
            name,
            width,
            height,
            scale_milli,
            primary,
        })
        .boxed()
}

pub fn swap_note() -> BoxedStrategy<SwapNote> {
    prop_oneof![
        (any::<u32>(), txt(64), any::<u16>()).prop_map(|(request_id, by, remaining_s)| SwapNote::Asked {
            request_id,
            by,
            remaining_s
        }),
        pick(SwapWhy::ALL).prop_map(|why| SwapNote::Declined { why }),
        Just(SwapNote::Done),
    ]
    .boxed()
}

pub fn chat_item() -> BoxedStrategy<ChatItem> {
    (any::<u32>(), txt(4096), any::<u64>())
        .prop_map(|(msg_id, text, sent_at_ms)| ChatItem { msg_id, text: Text(text), sent_at_ms })
        .boxed()
}

pub fn notify() -> BoxedStrategy<Notify> {
    let pairing = prop_oneof![
        any::<u16>().prop_map(|remaining_s| PairingState::Open { remaining_s }),
        Just(PairingState::Waiting),
        (txt(16), txt(64)).prop_map(|(c, peer_name)| PairingState::ShowCode { code: Text(c), peer_name }),
        txt(64).prop_map(|device_id| PairingState::Done { device_id }),
        api_code().prop_map(|reason| PairingState::Failed { reason }),
    ];
    let state = prop_oneof![
        Just(SessionState::Connecting),
        any::<u16>().prop_map(|remaining_s| SessionState::Prompted { remaining_s }),
        (any::<u64>(), pick(Role::ALL), any::<u16>())
            .prop_map(|(since_ms, role, granted)| { SessionState::Active { since_ms, role, granted } }),
        pick(Desktop::ALL).prop_map(|desktop| SessionState::Paused { desktop }),
        any::<u32>().prop_map(|attempt| SessionState::Reconnecting { attempt }),
        any::<u16>().prop_map(|remaining_s| SessionState::Waking { remaining_s }),
        proptest::option::of(api_code()).prop_map(|reason| SessionState::Ended { reason }),
    ];
    let kind = prop_oneof![
        pick(Intent::ALL).prop_map(|intent| RequestKind::Session { intent }),
        pick(Feature::ALL).prop_map(|feature| RequestKind::Consent { feature }),
    ];
    let incoming = (any::<u32>(), device(), kind, any::<u16>(), any::<u16>()).prop_map(
        |(request_id, device, kind, preset, timeout_s)| IncomingRequest { request_id, device, kind, preset, timeout_s },
    );
    let dv = (any::<u16>(), txt(64), any::<u32>(), any::<u32>(), any::<u16>(), any::<bool>()).prop_map(
        |(id, name, width, height, scale_milli, primary)| DisplayView { id, name, width, height, scale_milli, primary },
    );
    let all: Vec<BoxedStrategy<Notify>> = vec![
        vec(device(), 0..3).prop_map(Notify::DevicesChanged).boxed(),
        network().prop_map(Notify::NetworkChanged).boxed(),
        (any::<u32>(), pairing).prop_map(|(pairing_id, state)| Notify::Pairing { pairing_id, state }).boxed(),
        incoming.prop_map(Notify::IncomingRequest).boxed(),
        (any::<u32>(), api_code())
            .prop_map(|(request_id, reason)| Notify::IncomingClosed { request_id, reason })
            .boxed(),
        (any::<u64>(), txt(64), state)
            .prop_map(|(session_id, device_id, state)| Notify::Session { session_id, device_id, state })
            .boxed(),
        (any::<u64>(), vec(dv, 0..4)).prop_map(|(session_id, list)| Notify::Displays { session_id, list }).boxed(),
        (any::<u64>(), any::<u32>(), any::<u16>(), any::<u32>())
            .prop_map(|(session_id, latency_ms, fps, kbps)| Notify::Stats { session_id, latency_ms, fps, kbps })
            .boxed(),
        audit().prop_map(Notify::ActivityAlert).boxed(),
        settings().prop_map(Notify::SettingsChanged).boxed(),
        (any::<u64>(), swap_note()).prop_map(|(session_id, note)| Notify::Swap { session_id, note }).boxed(),
        (any::<u64>(), chat_item()).prop_map(|(session_id, item)| Notify::Chat { session_id, item }).boxed(),
        (any::<u64>(), any::<u32>()).prop_map(|(session_id, up_to)| Notify::ChatRead { session_id, up_to }).boxed(),
    ];
    Union::new(all).boxed()
}

pub fn message() -> BoxedStrategy<Message> {
    let all: Vec<BoxedStrategy<Message>> = vec![
        (pick(IpcRole::ALL), any::<u16>(), any::<u32>())
            .prop_map(|(role, version, pid)| Message::Hello { role, version, pid })
            .boxed(),
        (any::<u16>(), any::<u32>())
            .prop_map(|(version, win_session)| Message::HelloAck { version, win_session })
            .boxed(),
        pick(ErrorCode::ALL).prop_map(|code| Message::Bye { code }).boxed(),
        (any::<u16>(), any::<u16>(), any::<u32>(), pick(Preset::ALL), pick(VideoBackend::ALL))
            .prop_map(|(display, fps, kbps, quality, backend)| Message::StartVideo {
                display,
                fps,
                kbps,
                quality,
                backend,
            })
            .boxed(),
        any::<u16>().prop_map(|display| Message::StopVideo { display }).boxed(),
        (any::<u16>(), any::<u32>(), any::<u16>())
            .prop_map(|(display, kbps, fps)| Message::SetRate { display, kbps, fps })
            .boxed(),
        any::<u16>().prop_map(|display| Message::ForceKeyframe { display }).boxed(),
        events().prop_map(|events| Message::Input { events }).boxed(),
        Just(Message::ReleaseAll).boxed(),
        any::<u32>().prop_map(|exclude_pid| Message::StartAudio { exclude_pid }).boxed(),
        Just(Message::StopAudio).boxed(),
        any::<bool>().prop_map(|muted| Message::SetAudioMuted { muted }).boxed(),
        Just(Message::Shutdown).boxed(),
        vec(display(), 0..3).prop_map(|displays| Message::Displays(DisplayList { displays })).boxed(),
        video().prop_map(|(header, payload)| Message::VideoFrame { header, payload }).boxed(),
        cursor_pos().prop_map(Message::CursorPos).boxed(),
        shape().prop_map(Message::CursorShape).boxed(),
        bytes(MAX_AUDIO_DATAGRAM + 1).prop_map(Message::AudioPacket).boxed(),
        pick(AudioModeKind::ALL).prop_map(|mode| Message::AudioMode { mode }).boxed(),
        pick(Desktop::ALL).prop_map(Message::DesktopChanged).boxed(),
        Just(Message::SasRequest).boxed(),
        (pick(ErrorCode::ALL), txt(256)).prop_map(|(code, w)| Message::HelperFault { code, what: Text(w) }).boxed(),
        (any::<u32>(), op()).prop_map(|(req, op)| Message::UiRequest { req, op }).boxed(),
        (any::<u32>(), 0u16..=10, bytes(100))
            .prop_map(|(req, status, body)| Message::UiReply { req, status, body })
            .boxed(),
        notify().prop_map(Message::UiNotify).boxed(),
        (any::<u64>(), video())
            .prop_map(|(session_id, (header, payload))| Message::ViewerVideo { session_id, header, payload })
            .boxed(),
        (any::<u64>(), bytes(MAX_AUDIO_DATAGRAM + 1))
            .prop_map(|(session_id, datagram)| Message::ViewerAudio { session_id, datagram })
            .boxed(),
        (any::<u64>(), cursor_pos()).prop_map(|(session_id, pos)| Message::ViewerCursorPos { session_id, pos }).boxed(),
        (any::<u64>(), shape())
            .prop_map(|(session_id, shape)| Message::ViewerCursorShape { session_id, shape })
            .boxed(),
        (any::<u64>(), events()).prop_map(|(session_id, events)| Message::ViewerInput { session_id, events }).boxed(),
        (any::<u64>(), any::<u16>(), any::<u32>())
            .prop_map(|(session_id, display, last_good_frame)| Message::ViewerKeyframe {
                session_id,
                request: KeyframeRequest { display, last_good_frame },
            })
            .boxed(),
        (any::<u64>(), any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>())
            .prop_map(|(session_id, a, b, c, d, e)| Message::ViewerReport {
                session_id,
                report: ReceiverReport {
                    frames_presented: a,
                    frames_lost: b,
                    decode_us_p50: c,
                    present_lag_us_p50: d,
                    audio_underruns: e,
                },
            })
            .boxed(),
    ];
    Union::new(all).boxed()
}

// ---------- the clipboard, action, terminal and files messages, ops, notifications and reply bodies ----------

pub fn text(max: usize) -> BoxedStrategy<Text> {
    txt(max).prop_map(Text).boxed()
}

pub fn fs_call() -> BoxedStrategy<FsCall> {
    let p = || text(40);
    let n = || text(20);
    let kind = || pick(FsKind::ALL);
    let all: Vec<BoxedStrategy<FsCall>> = vec![
        (p(), any::<u32>()).prop_map(|(path, max_entries)| FsCall::ListDir { path, max_entries }).boxed(),
        (p(), any::<bool>()).prop_map(|(path, follow)| FsCall::Stat { path, follow }).boxed(),
        p().prop_map(|path| FsCall::OpenRead { path }).boxed(),
        (any::<u64>(), any::<u64>(), any::<u32>())
            .prop_map(|(id, offset, len)| FsCall::ReadAt { id, offset, len })
            .boxed(),
        any::<u64>().prop_map(|id| FsCall::ReadStat { id }).boxed(),
        any::<u64>().prop_map(|id| FsCall::CloseRead { id }).boxed(),
        (p(), n(), kind()).prop_map(|(dir, name, kind)| FsCall::StatFile { dir, name, kind }).boxed(),
        (p(), n()).prop_map(|(dir, name)| FsCall::PartCreate { dir, name }).boxed(),
        (p(), n(), any::<u64>()).prop_map(|(dir, name, len)| FsCall::PartTruncate { dir, name, len }).boxed(),
        (p(), n(), any::<u64>(), bytes(300))
            .prop_map(|(dir, name, offset, data)| FsCall::PartAppend { dir, name, offset, data })
            .boxed(),
        (p(), n()).prop_map(|(dir, name)| FsCall::PartLen { dir, name }).boxed(),
        (p(), n(), any::<u64>(), any::<u32>())
            .prop_map(|(dir, name, offset, len)| FsCall::PartRead { dir, name, offset, len })
            .boxed(),
        (p(), n(), proptest::option::of(any::<u64>()))
            .prop_map(|(dir, name, final_mtime_ms)| FsCall::PartSync { dir, name, final_mtime_ms })
            .boxed(),
        (p(), n(), n()).prop_map(|(dir, name, target_name)| FsCall::PartRename { dir, name, target_name }).boxed(),
        (p(), n(), kind()).prop_map(|(dir, name, kind)| FsCall::RemoveFile { dir, name, kind }).boxed(),
        (p(), n()).prop_map(|(dir, name)| FsCall::MetaRead { dir, name }).boxed(),
        (p(), n(), bytes(80)).prop_map(|(dir, name, data)| FsCall::MetaWrite { dir, name, data }).boxed(),
    ];
    Union::new(all).boxed()
}

pub fn fs_stat() -> BoxedStrategy<FsStatView> {
    (any::<bool>(), any::<bool>(), any::<bool>(), any::<u64>(), any::<u64>())
        .prop_map(|(is_dir, is_file, is_reparse, len, mtime_ms)| FsStatView {
            is_dir,
            is_file,
            is_reparse,
            len,
            mtime_ms,
        })
        .boxed()
}

pub fn fs_result() -> BoxedStrategy<core::result::Result<FsAnswer, ErrorCode>> {
    let entry = (text(20), any::<bool>(), any::<u64>(), any::<u64>())
        .prop_map(|(name, is_dir, size, mtime_ms)| FsEntryView { name, is_dir, size, mtime_ms });
    let ok: Vec<BoxedStrategy<FsAnswer>> = vec![
        Just(FsAnswer::Done).boxed(),
        (vec(entry, 0..4), any::<bool>())
            .prop_map(|(entries, truncated)| FsAnswer::Entries { entries, truncated })
            .boxed(),
        proptest::option::of(fs_stat()).prop_map(FsAnswer::Stat).boxed(),
        (any::<u64>(), any::<u64>(), any::<u64>())
            .prop_map(|(id, len, mtime_ms)| FsAnswer::Handle(FsHandleView { id, len, mtime_ms }))
            .boxed(),
        bytes(300).prop_map(FsAnswer::Data).boxed(),
        fs_stat().prop_map(FsAnswer::StatNow).boxed(),
        any::<u64>().prop_map(FsAnswer::Len).boxed(),
        proptest::option::of(bytes(80)).prop_map(FsAnswer::Meta).boxed(),
    ];
    prop_oneof![
        4 => Union::new(ok).prop_map(Ok),
        1 => pick(ErrorCode::ALL).prop_map(Err),
    ]
    .boxed()
}

pub fn files_op() -> BoxedStrategy<FilesOp> {
    let pick_file = (any::<u32>(), text(20)).prop_map(|(token, name)| FilePick { token, name });
    let all: Vec<BoxedStrategy<FilesOp>> = vec![
        (any::<u64>(), proptest::option::of(text(40)))
            .prop_map(|(session_id, path)| FilesOp::List { session_id, path })
            .boxed(),
        (any::<u64>(), vec(pick_file, 0..4), proptest::option::of(text(40)))
            .prop_map(|(session_id, picks, remote_dir)| FilesOp::Send { session_id, picks, remote_dir })
            .boxed(),
        (any::<u64>(), text(40), any::<u32>())
            .prop_map(|(session_id, remote_path, recv_token)| FilesOp::Get { session_id, remote_path, recv_token })
            .boxed(),
        any::<u32>().prop_map(|transfer_id| FilesOp::Pause { transfer_id }).boxed(),
        any::<u32>().prop_map(|transfer_id| FilesOp::Resume { transfer_id }).boxed(),
        any::<u32>().prop_map(|transfer_id| FilesOp::Cancel { transfer_id }).boxed(),
    ];
    Union::new(all).boxed()
}

pub fn files_note() -> BoxedStrategy<FilesNote> {
    let state = prop_oneof![
        Just(TransferWire::Queued),
        (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(|(done, total, bytes_per_s)| TransferWire::Running {
            done,
            total,
            bytes_per_s
        }),
        (any::<u64>(), any::<u64>(), api_code()).prop_map(|(done, total, why)| TransferWire::Paused {
            done,
            total,
            why
        }),
        Just(TransferWire::Done),
        api_code().prop_map(|reason| TransferWire::Failed { reason }),
    ];
    prop_oneof![
        (any::<u64>(), any::<u32>(), text(20), any::<bool>(), state).prop_map(
            |(session_id, transfer_id, name, outgoing, state)| FilesNote::Transfer {
                session_id,
                transfer_id,
                name,
                outgoing,
                state
            }
        ),
        (any::<u64>(), any::<u32>(), text(20)).prop_map(|(session_id, transfer_id, name)| FilesNote::SavedAs {
            session_id,
            transfer_id,
            name
        }),
    ]
    .boxed()
}

pub fn extra_op() -> BoxedStrategy<UiOp> {
    let all: Vec<BoxedStrategy<UiOp>> = vec![
        (any::<u64>(), any::<bool>()).prop_map(|(session_id, on)| UiOp::SetClipboardSync { session_id, on }).boxed(),
        (any::<u64>(), pick(swapdesk_proto::Action::ALL))
            .prop_map(|(session_id, action)| UiOp::RunAction { session_id, action })
            .boxed(),
        (any::<u64>(), any::<u16>(), any::<u16>(), any::<bool>())
            .prop_map(|(session_id, cols, rows, admin)| UiOp::TermOpen { session_id, cols, rows, admin })
            .boxed(),
        (any::<u32>(), any::<u16>(), any::<u16>())
            .prop_map(|(term_id, cols, rows)| UiOp::TermResize { term_id, cols, rows })
            .boxed(),
        any::<u32>().prop_map(|term_id| UiOp::TermClose { term_id }).boxed(),
        files_op().prop_map(UiOp::Files).boxed(),
    ];
    Union::new(all).boxed()
}

pub fn any_op() -> BoxedStrategy<UiOp> {
    prop_oneof![3 => op(), 2 => extra_op()].boxed()
}

pub fn extra_notify() -> BoxedStrategy<Notify> {
    prop_oneof![
        (any::<u64>(), api_code()).prop_map(|(session_id, reason)| Notify::ClipboardSkipped { session_id, reason }),
        (any::<u32>(), proptest::option::of(any::<i32>()))
            .prop_map(|(term_id, exit_code)| Notify::TermExited { term_id, exit_code }),
        files_note().prop_map(Notify::Files),
    ]
    .boxed()
}

pub fn any_notify() -> BoxedStrategy<Notify> {
    prop_oneof![3 => notify(), 1 => extra_notify()].boxed()
}

pub fn clip_data() -> BoxedStrategy<ClipData> {
    prop_oneof![txt(200).prop_map(ClipData::Text), vec(any::<u8>(), 0..200).prop_map(ClipData::Png)].boxed()
}

pub fn clip_reply() -> BoxedStrategy<ClipReplyBody> {
    prop_oneof![
        any::<u64>().prop_map(ClipReplyBody::Count),
        clip_data().prop_map(ClipReplyBody::Content),
        Just(ClipReplyBody::Concealed),
        Just(ClipReplyBody::Empty),
        Just(ClipReplyBody::Unsupported),
        Just(ClipReplyBody::Written),
        prop_oneof![Just(ClipFail::Unavailable), Just(ClipFail::Invalid), Just(ClipFail::Other)]
            .prop_map(ClipReplyBody::Failed),
    ]
    .boxed()
}

/// The messages `message()` leaves out: clipboard, actions, terminal, files and the ops, notifications and
/// replies those add.
pub fn extra_messages() -> Vec<BoxedStrategy<Message>> {
    vec![
        any::<u32>().prop_map(|req| Message::ClipChangeCount { req }).boxed(),
        any::<u32>().prop_map(|req| Message::ClipRead { req }).boxed(),
        (any::<u32>(), clip_data()).prop_map(|(req, content)| Message::ClipWrite { req, content }).boxed(),
        (any::<u32>(), clip_reply()).prop_map(|(req, body)| Message::ClipReply { req, body }).boxed(),
        (any::<u32>(), pick(swapdesk_proto::Action::ALL))
            .prop_map(|(req, action)| Message::RunAction { req, action })
            .boxed(),
        (any::<u32>(), any::<bool>()).prop_map(|(req, on)| Message::KeepAwake { req, on }).boxed(),
        (any::<u32>(), pick(ErrorCode::ALL)).prop_map(|(req, code)| Message::ActionReply { req, code }).boxed(),
        (any::<u32>(), any::<u16>(), any::<u16>(), text(40))
            .prop_map(|(id, cols, rows, session_user)| Message::PtyOpen { id, cols, rows, session_user })
            .boxed(),
        (any::<u32>(), pick(ErrorCode::ALL), text(40))
            .prop_map(|(id, code, account)| Message::PtyOpenReply { id, code, account })
            .boxed(),
        (any::<u32>(), bytes(300)).prop_map(|(id, bytes)| Message::PtyInput { id, bytes }).boxed(),
        (any::<u32>(), bytes(300)).prop_map(|(id, bytes)| Message::PtyOutput { id, bytes }).boxed(),
        (any::<u32>(), any::<u16>(), any::<u16>())
            .prop_map(|(id, cols, rows)| Message::PtyResize { id, cols, rows })
            .boxed(),
        any::<u32>().prop_map(|id| Message::PtyClose { id }).boxed(),
        (any::<u32>(), proptest::option::of(any::<i32>()))
            .prop_map(|(id, code)| Message::PtyExited { id, code })
            .boxed(),
        (any::<u32>(), bytes(300)).prop_map(|(term_id, bytes)| Message::TermOutput { term_id, bytes }).boxed(),
        (any::<u32>(), bytes(300)).prop_map(|(term_id, bytes)| Message::TermInput { term_id, bytes }).boxed(),
        (any::<u32>(), fs_call()).prop_map(|(req, call)| Message::FsRequest { req, call }).boxed(),
        (any::<u32>(), fs_result()).prop_map(|(req, result)| Message::FsReply { req, result }).boxed(),
        (any::<u32>(), extra_op()).prop_map(|(req, op)| Message::UiRequest { req, op }).boxed(),
        extra_notify().prop_map(Message::UiNotify).boxed(),
    ]
}

/// Every message of the set: `message()` and the extras.
pub fn any_message() -> BoxedStrategy<Message> {
    prop_oneof![
        3 => message(),
        4 => Union::new(extra_messages()),
    ]
    .boxed()
}

/// A reply body with the op number of the request it answers, for every op that has a body.
pub fn reply_body() -> BoxedStrategy<(u16, ReplyBody)> {
    let listing = (
        text(40),
        vec(
            (text(20), any::<bool>(), any::<u64>(), any::<u64>())
                .prop_map(|(name, is_dir, size, mtime_ms)| FileEntry { name, is_dir, size, mtime_ms }),
            0..4,
        ),
        any::<bool>(),
    )
        .prop_map(|(path, entries, truncated)| FileListing { path, entries, truncated });
    prop_oneof![
        Just((6u16, ReplyBody::Done)),
        (txt(64), pick(Os::ALL), pick(Ready::ALL))
            .prop_map(|(name, os, ready)| (1u16, ReplyBody::ThisDevice(ThisDevice { name, os, ready }))),
        network().prop_map(|n| (2u16, ReplyBody::Network(n))),
        vec(device(), 0..3).prop_map(|d| (3u16, ReplyBody::Devices(d))),
        device().prop_map(|d| (4u16, ReplyBody::Device(d))),
        any::<u32>().prop_map(|v| (5u16, ReplyBody::PairingId(v))),
        any::<u64>().prop_map(|v| (11u16, ReplyBody::SessionId(v))),
        any::<u64>().prop_map(|v| (26u16, ReplyBody::SessionId(v))),
        any::<u32>().prop_map(|v| (22u16, ReplyBody::RequestId(v))),
        chat_item().prop_map(|v| (24u16, ReplyBody::Chat(v))),
        settings().prop_map(|s| (19u16, ReplyBody::Settings(s))),
        vec(audit(), 0..3).prop_map(|a| (21u16, ReplyBody::Activity(a))),
        vec(display_view(), 0..3).prop_map(|d| (27u16, ReplyBody::Displays(d))),
        (any::<u32>(), pick(TermOutcome::ALL), text(40))
            .prop_map(|(term_id, outcome, account)| (32u16, ReplyBody::TermOpened { term_id, outcome, account })),
        listing.prop_map(|l| (40u16, ReplyBody::Listing(l))),
        vec(any::<u32>(), 0..5).prop_map(|v| (41u16, ReplyBody::TransferIds(v))),
        any::<u32>().prop_map(|v| (42u16, ReplyBody::TransferId(v))),
    ]
    .boxed()
}
