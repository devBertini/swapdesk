//! Value strategies and generic checks shared by `protocol.rs` (the regular proof) and `fuzz.rs` (the same
//! properties at fuzzing scale).

#![allow(dead_code)]

use core::fmt::Debug;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use swapdesk_proto::*;

// ---------- strategies ----------

pub fn sel<T: Copy + Debug + 'static>(all: &'static [T]) -> impl Strategy<Value = T> {
    proptest::sample::select(all)
}

/// Valid text without NUL whose UTF-8 length is at most `max`.
pub fn text(max: usize) -> impl Strategy<Value = String> {
    let ch = prop_oneof![
        6 => (0x20u8..0x7F).prop_map(char::from),
        1 => Just('é'),
        1 => Just('日'),
        1 => Just('😀'),
    ];
    proptest::collection::vec(ch, 0..=max.min(24)).prop_map(move |cs| {
        let mut s = String::new();
        for c in cs {
            if s.len() + c.len_utf8() > max {
                break;
            }
            s.push(c);
        }
        s
    })
}

pub fn perms() -> impl Strategy<Value = PermissionSet> {
    any::<u16>().prop_map(PermissionSet::closure)
}

pub fn errc() -> impl Strategy<Value = ErrorCode> {
    sel(ErrorCode::ALL)
}

pub fn display() -> impl Strategy<Value = Display> {
    (
        any::<u16>(),
        text(64),
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
}

pub fn dir_entry() -> impl Strategy<Value = DirEntry> {
    (text(255), any::<bool>(), any::<u64>(), any::<u64>()).prop_map(|(name, is_dir, size, mtime_ms)| DirEntry {
        name,
        is_dir,
        size,
        mtime_ms,
    })
}

pub fn dir_entry2() -> impl Strategy<Value = DirEntry2> {
    (text(255), sel(EntryKind::ALL), any::<u64>(), any::<u64>(), any::<u8>())
        .prop_map(|(name, kind, size, mtime_ms, flags)| DirEntry2 { name, kind, size, mtime_ms, flags })
}

pub fn place() -> impl Strategy<Value = Place> {
    (sel(PlaceKind::ALL), text(1024)).prop_map(|(kind, path)| Place { kind, path })
}

pub fn volume() -> impl Strategy<Value = Volume> {
    (
        any::<u32>(),
        sel(VolumeKind::ALL),
        text(64),
        proptest::option::of(any::<u8>()),
        any::<u64>(),
        any::<u64>(),
        any::<bool>(),
    )
        .prop_map(|(id, kind, label, letter, total_bytes, free_bytes, has_bin)| Volume {
            id,
            kind,
            label,
            letter,
            total_bytes,
            free_bytes,
            has_bin,
        })
}

pub fn fs_names() -> impl Strategy<Value = Vec<FsName>> {
    proptest::collection::vec(text(255).prop_map(|name| FsName { name }), 0..=4)
}

pub fn fs_op_kind() -> impl Strategy<Value = FsOpKind> {
    prop_oneof![
        (text(1024), text(255)).prop_map(|(parent, name)| FsOpKind::MakeDir { parent, name }),
        (text(1024), text(255), text(255)).prop_map(|(dir, name, new_name)| FsOpKind::Rename { dir, name, new_name }),
        (text(1024), fs_names(), text(1024)).prop_map(|(from_dir, names, to_dir)| FsOpKind::Move {
            from_dir,
            names,
            to_dir
        }),
        (text(1024), fs_names()).prop_map(|(dir, names)| FsOpKind::Trash { dir, names }),
    ]
}

pub fn video_cap() -> impl Strategy<Value = VideoCap> {
    (sel(Codec::ALL), sel(H264Profile::ALL), any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()).prop_map(
        |(codec, profile, max_level, max_width, max_height, max_fps)| VideoCap {
            codec,
            profile,
            max_level,
            max_width,
            max_height,
            max_fps,
        },
    )
}

macro_rules! msg {
    ($v:ident { $($f:ident : $s:expr),* $(,)? }) => {
        msg!($v, $v { $($f : $s),* })
    };
    ($v:ident, $st:ident { $($f:ident : $s:expr),* $(,)? }) => {
        ( $($s,)* ).prop_map(|( $($f,)* )| Message::$v($st { $($f),* })).boxed()
    };
}

/// One strategy per message type, in the order of the table in 5.3.
pub fn message_strategies() -> Vec<BoxedStrategy<Message>> {
    let majors = || proptest::collection::vec(any::<u16>(), 0..=MAX_PAIR_MAJORS);
    vec![
        msg!(Hello {
            proto_minor: any::<u16>(),
            app_version: text(32),
            name: text(64),
            os: sel(Os::ALL),
            os_version: text(32),
            intent: sel(Intent::ALL),
            requested: perms(),
            resume: proptest::option::of(
                (any::<u64>(), any::<[u8; 16]>()).prop_map(|(session_id, token)| Resume { session_id, token })
            ),
            swap_token: proptest::option::of(any::<[u8; 16]>()),
        }),
        msg!(HelloAck {
            proto_minor: any::<u16>(),
            app_version: text(32),
            name: text(64),
            os: sel(Os::ALL),
            os_version: text(32),
        }),
        msg!(Capabilities {
            video_decode: proptest::collection::vec(video_cap(), 0..=8),
            audio: proptest::collection::vec(sel(AudioCodec::ALL), 0..=4),
            max_datagram: any::<u16>(),
            keyboard: (sel(KbKind::ALL), text(64)).prop_map(|(kind, layout)| KeyboardInfo { kind, layout }),
            features: any::<u32>(),
        }),
        msg!(PromptPending { timeout_s: any::<u16>() }),
        msg!(SessionGrant {
            session_id: any::<u64>(),
            granted: perms(),
            mode: sel(GrantMode::ALL),
            resume_token: any::<[u8; 16]>(),
        }),
        msg!(SessionRefused { code: errc() }),
        msg!(PermissionsChanged { granted: perms() }),
        msg!(Bye { code: errc() }),
        msg!(HostState { desktop: sel(Desktop::ALL), paused: any::<bool>() }),
        msg!(ConsentPending { feature: sel(Feature::ALL), timeout_s: any::<u16>() }),
        msg!(StoredGrants { grants: perms() }),
        Just(Message::Elevate(Elevate {})).boxed(),
        msg!(DisplayList { displays: proptest::collection::vec(display(), 0..=4) }),
        msg!(SelectDisplay { display: any::<u16>() }),
        msg!(VideoSettings {
            preset: sel(Preset::ALL),
            max_fps: any::<u16>(),
            max_kbps: any::<u32>(),
            view_width: any::<u16>(),
            view_height: any::<u16>(),
        }),
        msg!(KeyframeRequest { display: any::<u16>(), last_good_frame: any::<u32>() }),
        msg!(ReceiverReport {
            frames_presented: any::<u32>(),
            frames_lost: any::<u32>(),
            decode_us_p50: any::<u32>(),
            present_lag_us_p50: any::<u32>(),
            audio_underruns: any::<u32>(),
        }),
        msg!(StreamStats { kbps: any::<u32>(), fps: any::<u16>(), encoder: text(64) }),
        msg!(AudioControl { enabled: any::<bool>() }),
        msg!(HostMute { muted: any::<bool>() }),
        msg!(SendSpecial { key: sel(Special::ALL) }),
        msg!(SwapRequest { request_id: any::<u32>() }),
        msg!(SwapAccept { request_id: any::<u32>(), token: any::<[u8; 16]>() }),
        msg!(SwapDecline { request_id: any::<u32>(), code: errc() }),
        msg!(SwapCancel { request_id: any::<u32>() }),
        msg!(SwapArmed { request_id: any::<u32>() }),
        msg!(ChatMessage { msg_id: any::<u32>(), text: text(4096), sent_at_ms: any::<u64>() }),
        msg!(ChatRead { up_to: any::<u32>() }),
        msg!(ClipboardSync { enabled: any::<bool>() }),
        msg!(ClipboardSkipped { reason: sel(ClipSkip::ALL) }),
        msg!(FileOffer {
            transfer_id: any::<u32>(),
            name: text(255),
            size: any::<u64>(),
            mtime_ms: any::<u64>(),
            dest: proptest::option::of(text(1024)),
        }),
        msg!(FileAccept { transfer_id: any::<u32>(), resume_from: any::<u64>() }),
        msg!(FileDecline { transfer_id: any::<u32>(), code: errc() }),
        msg!(FileDone { transfer_id: any::<u32>(), sha256: any::<[u8; 32]>() }),
        msg!(FileVerified { transfer_id: any::<u32>(), ok: any::<bool>() }),
        msg!(FilePause { transfer_id: any::<u32>() }),
        msg!(FileResume { transfer_id: any::<u32>() }),
        msg!(FileCancel { transfer_id: any::<u32>() }),
        msg!(DirRequest { req_id: any::<u32>(), path: proptest::option::of(text(1024)) }),
        msg!(DirListing {
            req_id: any::<u32>(),
            path: text(1024),
            entries: proptest::collection::vec(dir_entry(), 0..=4),
            truncated: any::<bool>(),
        }),
        msg!(FileGet { transfer_id: any::<u32>(), path: text(1024) }),
        msg!(DirPage { req_id: any::<u32>(), path: text(1024), start: any::<u32>(), max: any::<u16>() }),
        msg!(DirPageListing {
            req_id: any::<u32>(),
            path: text(1024),
            start: any::<u32>(),
            entries: proptest::collection::vec(dir_entry2(), 0..=4),
            next: proptest::option::of(any::<u32>()),
            free_bytes: proptest::option::of(any::<u64>()),
        }),
        msg!(VolumesRequest { req_id: any::<u32>() }),
        msg!(VolumesList {
            req_id: any::<u32>(),
            home: text(1024),
            places: proptest::collection::vec(place(), 0..=3),
            volumes: proptest::collection::vec(volume(), 0..=3),
        }),
        msg!(FsOp { op_id: any::<u32>(), op: fs_op_kind() }),
        msg!(FsOpResult {
            op_id: any::<u32>(),
            results: proptest::collection::vec(
                (text(255), errc()).prop_map(|(name, code)| OpItem { name, code }),
                0..=4
            ),
        }),
        msg!(TermOpen { term_id: any::<u32>(), cols: any::<u16>(), rows: any::<u16>(), admin: any::<bool>() }),
        msg!(TermOpened { term_id: any::<u32>(), account: text(64) }),
        msg!(TermResize { term_id: any::<u32>(), cols: any::<u16>(), rows: any::<u16>() }),
        msg!(TermClose { term_id: any::<u32>() }),
        msg!(TermExited { term_id: any::<u32>(), exit_code: proptest::option::of(any::<i32>()) }),
        msg!(QuickAction { action_id: any::<u32>(), kind: sel(Action::ALL) }),
        msg!(QuickActionResult { action_id: any::<u32>(), code: errc() }),
        msg!(WakeInfo { macs: proptest::collection::vec(any::<[u8; 6]>(), 0..=8) }),
        msg!(PairHello {
            name: text(64),
            os: sel(Os::ALL),
            os_version: text(32),
            majors: majors(),
            commit: any::<[u8; 32]>(),
        }),
        msg!(PairNonce {
            name: text(64),
            os: sel(Os::ALL),
            os_version: text(32),
            majors: majors(),
            nonce_r: any::<[u8; 32]>(),
        }),
        msg!(PairReveal { nonce_i: any::<[u8; 32]>() }),
        msg!(PairConfirm { matches: any::<bool>() }),
        msg!(PairResult { ok: any::<bool>(), code: errc() }),
        msg!(GrantsSync { proto_minor: any::<u16>(), grants: perms() }),
        msg!(Ping { nonce: any::<u32>(), t_send_us: any::<u64>() }),
        msg!(Pong { nonce: any::<u32>(), t_send_us: any::<u64>(), t_peer_us: any::<u64>() }),
        msg!(
            Error,
            ErrorMessage { code: errc(), fatal: any::<bool>(), about_type: any::<u16>(), about_id: any::<u32>() }
        ),
    ]
}

pub fn video_header() -> impl Strategy<Value = VideoHeader> {
    (
        (any::<bool>(), any::<bool>(), any::<bool>()),
        any::<u16>(),
        any::<u32>(),
        any::<u64>(),
        any::<u32>(),
        (any::<u16>(), any::<u16>()),
        sel(Codec::ALL),
        0..=MAX_VIDEO_PAYLOAD,
    )
        .prop_map(
            |(
                (keyframe, config, discontinuity),
                display_id,
                frame_id,
                capture_us,
                encode_us,
                (width, height),
                codec,
                payload_len,
            )| {
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
                    codec,
                    payload_len,
                }
            },
        )
}

pub fn audio_header() -> impl Strategy<Value = AudioHeader> {
    (any::<bool>(), any::<bool>(), any::<u16>(), any::<u32>(), any::<u64>()).prop_map(
        |(discontinuity, muted, frame_ms, seq, capture_us)| AudioHeader {
            discontinuity,
            muted,
            frame_ms,
            seq,
            capture_us,
        },
    )
}

pub fn cursor_position() -> impl Strategy<Value = CursorPosition> {
    (any::<bool>(), any::<u16>(), any::<u32>(), any::<u16>(), any::<u16>(), any::<u32>())
        .prop_map(|(visible, display, seq, x, y, shape_id)| CursorPosition { visible, display, seq, x, y, shape_id })
}

pub fn cursor_shape_header() -> impl Strategy<Value = CursorShapeHeader> {
    (any::<u32>(), 0..=MAX_CURSOR_DIMENSION, 0..=MAX_CURSOR_DIMENSION, any::<u16>(), any::<u16>())
        .prop_map(|(shape_id, width, height, hot_x, hot_y)| CursorShapeHeader { shape_id, width, height, hot_x, hot_y })
}

pub fn cursor_shape() -> impl Strategy<Value = CursorShape> {
    (any::<u32>(), 0u16..=6, 0u16..=6, any::<u16>(), any::<u16>(), any::<u8>()).prop_map(
        |(shape_id, width, height, hot_x, hot_y, fill)| {
            let header = CursorShapeHeader { shape_id, width, height, hot_x, hot_y };
            CursorShape { header, rgba: vec![fill; header.rgba_len()] }
        },
    )
}

pub fn clipboard_header() -> impl Strategy<Value = ClipboardHeader> {
    (any::<u32>(), sel(ClipKind::ALL), any::<u32>()).prop_map(|(seq, kind, len)| ClipboardHeader {
        seq,
        kind,
        len: len % (kind.max_len() + 1),
    })
}

pub fn clipboard_item() -> impl Strategy<Value = ClipboardItem> {
    let payload = prop_oneof![
        proptest::collection::vec(prop_oneof![Just('a'), Just('\0'), Just('é'), Just('😀')], 0..20)
            .prop_map(|cs| ClipboardPayload::Text(cs.into_iter().collect())),
        proptest::collection::vec(any::<u8>(), 0..40).prop_map(ClipboardPayload::Png),
    ];
    (any::<u32>(), payload).prop_map(|(seq, payload)| ClipboardItem { seq, payload })
}

pub fn input_event() -> impl Strategy<Value = InputEvent> {
    let key = (
        prop_oneof![Just(USAGE_PAGE_KEYBOARD), Just(USAGE_PAGE_CONSUMER)],
        any::<u16>(),
        any::<bool>(),
        any::<u8>(),
        0u8..8,
    )
        .prop_map(|(usage_page, usage, down, modifiers, locks)| InputEvent::Key {
            usage_page,
            usage,
            down,
            modifiers,
            locks,
        });
    prop_oneof![
        key,
        text(MAX_INPUT_TEXT).prop_map(InputEvent::Text),
        (any::<u16>(), any::<u16>(), any::<u16>()).prop_map(|(display, x, y)| InputEvent::Pointer { display, x, y }),
        (sel(MouseButton::ALL), any::<bool>()).prop_map(|(button, down)| InputEvent::Button { button, down }),
        (any::<i16>(), any::<i16>()).prop_map(|(dx, dy)| InputEvent::Wheel { dx, dy }),
        Just(InputEvent::ReleaseAll),
    ]
}

// ---------- generic checks ----------

pub fn fail(msg: String) -> TestCaseError {
    TestCaseError::fail(msg)
}

/// A `u64` as a varint in a longer form than it needs (length 2, 4 or 8 bytes), if one exists for `len` bytes.
pub fn padded_varint(v: u64, len: usize) -> Vec<u8> {
    let prefix: u8 = match len {
        2 => 0x40,
        4 => 0x80,
        8 => 0xC0,
        _ => unreachable!(),
    };
    let mut bytes = v.to_be_bytes()[8 - len..].to_vec();
    bytes[0] |= prefix;
    bytes
}

/// Round trip, truncation at every offset, a trailing byte, and single-byte mutations whose accepted results
/// must re-encode to the mutated bytes. For types that decode from a whole buffer.
pub fn check_wire<T: Wire + Debug + PartialEq>(x: &T) -> Result<(), TestCaseError> {
    let bytes = x.encode().map_err(|e| fail(format!("encode {x:?}: {e}")))?;
    prop_assert_eq!(&T::decode(&bytes).map_err(|e| fail(format!("{e}")))?, x);
    for cut in 0..bytes.len() {
        prop_assert!(T::decode(&bytes[..cut]).is_err(), "prefix of {} bytes decoded", cut);
    }
    let mut longer = bytes.clone();
    longer.push(0);
    prop_assert_eq!(T::decode(&longer).err(), Some(CodecError::TrailingBytes));
    let (again, used) = T::decode_prefix(&longer).map_err(|e| fail(format!("{e}")))?;
    prop_assert_eq!((&again, used), (x, bytes.len()));
    for i in 0..bytes.len() {
        for flip in [0x01u8, 0x80, 0xFF] {
            let mut m = bytes.clone();
            m[i] ^= flip;
            if let Ok(y) = T::decode(&m) {
                prop_assert_eq!(
                    y.encode().map_err(|e| fail(format!("{e}")))?,
                    m,
                    "accepted mutation does not re-encode"
                );
            }
        }
    }
    Ok(())
}

/// Random bytes never panic, and what is accepted re-encodes to the same bytes.
pub fn check_random<T: Wire + Debug>(bytes: &[u8]) -> Result<(), TestCaseError> {
    if let Ok(y) = T::decode(bytes) {
        prop_assert_eq!(y.encode().map_err(|e| fail(format!("{e}")))?, bytes);
    }
    if let Ok((y, used)) = T::decode_prefix(bytes) {
        prop_assert_eq!(y.encode().map_err(|e| fail(format!("{e}")))?, &bytes[..used]);
    }
    Ok(())
}

pub fn check_message(m: &Message) -> Result<(), TestCaseError> {
    let ty = m.type_id();
    let id = u64::from(ty);
    let payload = m.encode_payload().map_err(|e| fail(format!("{e}")))?;
    let frame = m.encode_frame().map_err(|e| fail(format!("{e}")))?;

    // decode(encode(x)) == x, at the payload and at the frame level, and the bytes are canonical
    prop_assert_eq!(&Message::decode_payload(id, &payload).map_err(|e| fail(format!("{e}")))?, m);
    prop_assert_eq!(&Message::decode_frame_exact(&frame).map_err(|e| fail(format!("{e}")))?, m);
    prop_assert_eq!(Message::decode_frame(&frame).map_err(|e| fail(format!("{e}")))?.1, frame.len());

    // truncation at every offset, of the payload and of the frame
    for cut in 0..payload.len() {
        prop_assert!(Message::decode_payload(id, &payload[..cut]).is_err(), "payload prefix {} decoded", cut);
    }
    for cut in 0..frame.len() {
        prop_assert!(Message::decode_frame(&frame[..cut]).is_err(), "frame prefix {} decoded", cut);
    }

    // trailing bytes: inside the payload (fatal), after the frame (left for the next frame)
    let mut longer = payload.clone();
    longer.push(0);
    prop_assert_eq!(Message::decode_payload(id, &longer).err(), Some(CodecError::TrailingBytes));
    let mut longer = frame.clone();
    longer.push(0);
    prop_assert_eq!(Message::decode_frame_exact(&longer).err(), Some(CodecError::TrailingBytes));

    // non-minimal varints in the frame header, in the type and in the length
    let shortest = |v: u64| {
        if v < 1 << 6 {
            1
        } else if v < 1 << 14 {
            2
        } else if v < 1 << 30 {
            4
        } else {
            8
        }
    };
    for len in [2usize, 4, 8] {
        if len > shortest(u64::from(ty)) {
            let mut f = padded_varint(u64::from(ty), len);
            f.extend(varint(payload.len() as u64));
            f.extend_from_slice(&payload);
            prop_assert_eq!(Message::decode_frame(&f).err(), Some(CodecError::NonMinimalVarint));
        }
        if len > shortest(payload.len() as u64) {
            let mut f = varint(u64::from(ty));
            f.extend(padded_varint(payload.len() as u64, len));
            f.extend_from_slice(&payload);
            prop_assert_eq!(Message::decode_frame(&f).err(), Some(CodecError::NonMinimalVarint));
        }
    }

    // single-byte mutations of the frame: whatever is accepted re-encodes to the same bytes
    for i in 0..frame.len() {
        for flip in [0x01u8, 0x80, 0xFF] {
            let mut mutated = frame.clone();
            mutated[i] ^= flip;
            if let Ok((y, used)) = Message::decode_frame(&mutated) {
                prop_assert_eq!(y.encode_frame().map_err(|e| fail(format!("{e}")))?, &mutated[..used]);
            }
        }
    }
    Ok(())
}

pub fn varint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_varint(v).unwrap();
    w.into_bytes()
}
