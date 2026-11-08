//! Proof of PROTOCOL.md 9.3: round trips for every message and header, `encode(decode(b)) == b` for accepted
//! bytes, and rejection (without panics or large allocations) of truncation, trailing bytes, over-limit lengths,
//! huge declared counts, bad UTF-8, bad tags and non-minimal varints.
//!
//! "Without large allocations": `forbid(unsafe_code)` rules out a counting allocator, so the tests feed tiny
//! inputs that declare 2^62 items or bytes; a decoder that reserved from the declared count would abort the test
//! process (capacity overflow or allocation failure). Accepted values are checked to hold no more capacity than
//! the input had bytes.

use core::fmt::Debug;

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use swapdesk_proto::*;

// ---------- strategies ----------

fn sel<T: Copy + Debug + 'static>(all: &'static [T]) -> impl Strategy<Value = T> {
    proptest::sample::select(all)
}

/// Valid text without NUL whose UTF-8 length is at most `max`.
fn text(max: usize) -> impl Strategy<Value = String> {
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

fn perms() -> impl Strategy<Value = PermissionSet> {
    any::<u16>().prop_map(PermissionSet::closure)
}

fn errc() -> impl Strategy<Value = ErrorCode> {
    sel(ErrorCode::ALL)
}

fn display() -> impl Strategy<Value = Display> {
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

fn dir_entry() -> impl Strategy<Value = DirEntry> {
    (text(255), any::<bool>(), any::<u64>(), any::<u64>()).prop_map(|(name, is_dir, size, mtime_ms)| DirEntry {
        name,
        is_dir,
        size,
        mtime_ms,
    })
}

fn video_cap() -> impl Strategy<Value = VideoCap> {
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
fn message_strategies() -> Vec<BoxedStrategy<Message>> {
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
        msg!(Ping { nonce: any::<u32>(), t_send_us: any::<u64>() }),
        msg!(Pong { nonce: any::<u32>(), t_send_us: any::<u64>(), t_peer_us: any::<u64>() }),
        msg!(
            Error,
            ErrorMessage { code: errc(), fatal: any::<bool>(), about_type: any::<u16>(), about_id: any::<u32>() }
        ),
    ]
}

fn video_header() -> impl Strategy<Value = VideoHeader> {
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

fn audio_header() -> impl Strategy<Value = AudioHeader> {
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

fn cursor_position() -> impl Strategy<Value = CursorPosition> {
    (any::<bool>(), any::<u16>(), any::<u32>(), any::<u16>(), any::<u16>(), any::<u32>())
        .prop_map(|(visible, display, seq, x, y, shape_id)| CursorPosition { visible, display, seq, x, y, shape_id })
}

fn cursor_shape_header() -> impl Strategy<Value = CursorShapeHeader> {
    (any::<u32>(), 0..=MAX_CURSOR_DIMENSION, 0..=MAX_CURSOR_DIMENSION, any::<u16>(), any::<u16>())
        .prop_map(|(shape_id, width, height, hot_x, hot_y)| CursorShapeHeader { shape_id, width, height, hot_x, hot_y })
}

fn cursor_shape() -> impl Strategy<Value = CursorShape> {
    (any::<u32>(), 0u16..=6, 0u16..=6, any::<u16>(), any::<u16>(), any::<u8>()).prop_map(
        |(shape_id, width, height, hot_x, hot_y, fill)| {
            let header = CursorShapeHeader { shape_id, width, height, hot_x, hot_y };
            CursorShape { header, rgba: vec![fill; header.rgba_len()] }
        },
    )
}

fn clipboard_header() -> impl Strategy<Value = ClipboardHeader> {
    (any::<u32>(), sel(ClipKind::ALL), any::<u32>()).prop_map(|(seq, kind, len)| ClipboardHeader {
        seq,
        kind,
        len: len % (kind.max_len() + 1),
    })
}

fn clipboard_item() -> impl Strategy<Value = ClipboardItem> {
    let payload = prop_oneof![
        proptest::collection::vec(prop_oneof![Just('a'), Just('\0'), Just('é'), Just('😀')], 0..20)
            .prop_map(|cs| ClipboardPayload::Text(cs.into_iter().collect())),
        proptest::collection::vec(any::<u8>(), 0..40).prop_map(ClipboardPayload::Png),
    ];
    (any::<u32>(), payload).prop_map(|(seq, payload)| ClipboardItem { seq, payload })
}

fn input_event() -> impl Strategy<Value = InputEvent> {
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

fn fail(msg: String) -> TestCaseError {
    TestCaseError::fail(msg)
}

/// A `u64` as a varint in a longer form than it needs (length 2, 4 or 8 bytes), if one exists for `len` bytes.
fn padded_varint(v: u64, len: usize) -> Vec<u8> {
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
fn check_wire<T: Wire + Debug + PartialEq>(x: &T) -> Result<(), TestCaseError> {
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
fn check_random<T: Wire + Debug>(bytes: &[u8]) -> Result<(), TestCaseError> {
    if let Ok(y) = T::decode(bytes) {
        prop_assert_eq!(y.encode().map_err(|e| fail(format!("{e}")))?, bytes);
    }
    if let Ok((y, used)) = T::decode_prefix(bytes) {
        prop_assert_eq!(y.encode().map_err(|e| fail(format!("{e}")))?, &bytes[..used]);
    }
    Ok(())
}

fn check_message(m: &Message) -> Result<(), TestCaseError> {
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

// ---------- round trips, per type ----------

#[test]
fn every_message_type_is_covered_once() {
    let strategies = message_strategies();
    assert_eq!(strategies.len(), Message::ALL_TYPE_IDS.len());
    let mut runner = TestRunner::deterministic();
    let ids: Vec<u16> = strategies.iter().map(|s| s.new_tree(&mut runner).unwrap().current().type_id()).collect();
    assert_eq!(ids, Message::ALL_TYPE_IDS, "strategies follow the table order and cover every type ID");
}

#[test]
fn every_message_round_trips_and_rejects_damage() {
    let mut runner = TestRunner::new(Config { cases: 96, failure_persistence: None, ..Config::default() });
    for strategy in message_strategies() {
        runner.run(&strategy, |m| check_message(&m)).unwrap_or_else(|e| panic!("{e}"));
    }
}

fn run_wire<T: Wire + Debug + PartialEq>(strategy: impl Strategy<Value = T>, cases: u32) {
    let mut runner = TestRunner::new(Config { cases, failure_persistence: None, ..Config::default() });
    runner.run(&strategy, |x| check_wire(&x)).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn nested_records_round_trip() {
    run_wire(display(), 256);
    run_wire(dir_entry(), 256);
    run_wire(video_cap(), 256);
    run_wire(perms(), 256);
    run_wire(errc(), 64);
}

#[test]
fn media_headers_round_trip() {
    run_wire(video_header(), 256);
    run_wire(audio_header(), 256);
    run_wire(cursor_position(), 256);
    run_wire(cursor_shape_header(), 256);
    run_wire(cursor_shape(), 128);
    run_wire(clipboard_header(), 256);
    run_wire(clipboard_item(), 256);
    run_wire(
        (any::<u32>(), any::<u64>()).prop_map(|(transfer_id, offset)| FileDataHeader { transfer_id, offset }),
        128,
    );
    run_wire(any::<u32>().prop_map(|term_id| TerminalStreamHeader { term_id }), 128);
}

#[test]
fn input_records_round_trip() {
    run_wire(input_event(), 512);
    // a stream of records decodes one after another
    let mut runner = TestRunner::new(Config { cases: 128, failure_persistence: None, ..Config::default() });
    runner
        .run(&proptest::collection::vec(input_event(), 0..8), |events| {
            let mut stream = Vec::new();
            for e in &events {
                stream.extend(e.encode().map_err(|e| fail(format!("{e}")))?);
            }
            let mut rest = &stream[..];
            let mut back = Vec::new();
            while !rest.is_empty() {
                let (e, used) = InputEvent::decode_prefix(rest).map_err(|e| fail(format!("{e}")))?;
                back.push(e);
                rest = &rest[used..];
            }
            prop_assert_eq!(back, events);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn stream_and_datagram_kinds_round_trip() {
    for kind in StreamKind::ALL {
        check_wire(kind).unwrap();
    }
    for kind in DatagramKind::ALL {
        check_wire(kind).unwrap();
    }
    assert_eq!(StreamKind::ALL.len(), 6);
}

#[test]
fn frame_headers_round_trip() {
    for &type_id in Message::ALL_TYPE_IDS {
        for len in [0usize, 1, 63, 64, 16_383, 16_384, MAX_FRAME_PAYLOAD] {
            check_wire(&FrameHeader { type_id, len }).unwrap();
        }
    }
}

// ---------- random bytes ----------

proptest! {
    #![proptest_config(Config { cases: 4096, failure_persistence: None, ..Config::default() })]

    #[test]
    fn random_payloads_never_panic_and_accepted_ones_are_canonical(
        id in sel(Message::ALL_TYPE_IDS),
        bytes in proptest::collection::vec(any::<u8>(), 0..48),
    ) {
        if let Ok(m) = Message::decode_payload(u64::from(id), &bytes) {
            prop_assert_eq!(m.type_id(), id);
            prop_assert_eq!(m.encode_payload().unwrap(), bytes);
        }
    }

    #[test]
    fn random_frames_never_panic_and_accepted_ones_are_canonical(
        bytes in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        if let Ok((m, used)) = Message::decode_frame(&bytes) {
            prop_assert_eq!(m.encode_frame().unwrap(), &bytes[..used]);
        }
    }

    #[test]
    fn random_bytes_never_break_the_header_decoders(bytes in proptest::collection::vec(any::<u8>(), 0..48)) {
        check_random::<VideoHeader>(&bytes)?;
        check_random::<AudioHeader>(&bytes)?;
        check_random::<CursorPosition>(&bytes)?;
        check_random::<CursorShapeHeader>(&bytes)?;
        check_random::<CursorShape>(&bytes)?;
        check_random::<ClipboardHeader>(&bytes)?;
        check_random::<ClipboardItem>(&bytes)?;
        check_random::<InputEvent>(&bytes)?;
        check_random::<StreamKind>(&bytes)?;
        check_random::<DatagramKind>(&bytes)?;
        check_random::<FileDataHeader>(&bytes)?;
        check_random::<FrameHeader>(&bytes)?;
        check_random::<PermissionSet>(&bytes)?;
        check_random::<Display>(&bytes)?;
        check_random::<DirEntry>(&bytes)?;
    }
}

// ---------- rejection, by hand ----------

fn varint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_varint(v).unwrap();
    w.into_bytes()
}

const HUGE: u64 = (1 << 62) - 1;

#[test]
fn huge_declared_counts_are_refused_without_reserving() {
    // Each payload declares up to 2^62 - 1 items and carries almost nothing; a decoder that reserved from the
    // declared count would abort this process.
    let with_prefix = |prefix: &[u8], id: u16| {
        for declared in [17u64, 2001, 1 << 20, 1 << 32, HUGE] {
            let mut payload = prefix.to_vec();
            payload.extend(varint(declared));
            payload.extend_from_slice(&[0; 8]);
            let err = Message::decode_payload(u64::from(id), &payload).unwrap_err();
            assert!(
                matches!(err, CodecError::OverLimit { .. } | CodecError::CountExceedsInput),
                "type {id:#x}, count {declared}: {err}"
            );
            assert_eq!(err.code(), ErrorCode::ProtocolViolation);
        }
    };
    with_prefix(&[], 0x0003); // Capabilities.video_decode
    with_prefix(&[], 0x0101); // DisplayList
    with_prefix(&[], 0x0603); // WakeInfo
    let mut dir = 7u32.to_le_bytes().to_vec(); // DirListing: req_id, path "", then entries
    dir.push(0);
    with_prefix(&dir, 0x040A);
    with_prefix(&[0, 0, 0], 0x0701); // PairHello: name "", os, os_version "", majors
}

#[test]
fn counts_inside_the_limit_still_need_their_bytes() {
    // 16 displays declared (the limit), 10 bytes given
    let mut payload = varint(16);
    payload.extend_from_slice(&[0; 10]);
    assert_eq!(Message::decode_payload(0x0101, &payload).unwrap_err(), CodecError::CountExceedsInput);
    // 2000 entries declared, a few given
    let mut payload = 1u32.to_le_bytes().to_vec();
    payload.push(0);
    payload.extend(varint(2000));
    payload.extend_from_slice(&[0; 100]);
    assert_eq!(Message::decode_payload(0x040A, &payload).unwrap_err(), CodecError::CountExceedsInput);
}

#[test]
fn counts_at_the_limit_decode_and_one_more_does_not() {
    let displays = |n: usize| {
        Message::DisplayList(DisplayList {
            displays: (0..n)
                .map(|i| Display {
                    id: i as u16,
                    name: String::new(),
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    scale_milli: 1000,
                    refresh_mhz: 60_000,
                    rotation: 0,
                    primary: i == 0,
                })
                .collect(),
        })
    };
    let ok = displays(16);
    assert_eq!(Message::decode_frame_exact(&ok.encode_frame().unwrap()).unwrap(), ok);
    assert!(displays(17).encode_frame().is_err());
    let macs = |n: usize| Message::WakeInfo(WakeInfo { macs: vec![[1; 6]; n] });
    assert!(macs(8).encode_frame().is_ok());
    assert!(macs(9).encode_frame().is_err());
    let entries = |n: usize| {
        Message::DirListing(DirListing {
            req_id: 1,
            path: String::new(),
            entries: vec![DirEntry { name: String::new(), is_dir: false, size: 0, mtime_ms: 0 }; n],
            truncated: n > 0,
        })
    };
    let ok = entries(2000);
    let frame = ok.encode_frame().unwrap();
    assert_eq!(Message::decode_frame_exact(&frame).unwrap(), ok);
    assert!(entries(2001).encode_frame().is_err());
    // an over-limit count that does have its bytes is still refused, by the count
    let mut payload = 1u32.to_le_bytes().to_vec();
    payload.push(0);
    payload.extend(varint(2001));
    payload.extend(core::iter::repeat_n(0u8, 2001 * 18));
    payload.push(0);
    assert!(matches!(Message::decode_payload(0x040A, &payload), Err(CodecError::OverLimit { .. })));
}

#[test]
fn accepted_vectors_hold_no_more_capacity_than_the_input_had_bytes() {
    let mut runner = TestRunner::new(Config { cases: 128, failure_persistence: None, ..Config::default() });
    let strategy = (
        proptest::collection::vec(display(), 0..=16),
        proptest::collection::vec(dir_entry(), 0..=20),
        proptest::collection::vec(any::<[u8; 6]>(), 0..=8),
        text(1024),
    );
    runner
        .run(&strategy, |(displays, entries, macs, path)| {
            let list = DisplayList { displays }.encode().unwrap();
            let back = DisplayList::decode(&list).unwrap();
            prop_assert!(back.displays.capacity() <= list.len());
            let dir = DirListing { req_id: 1, path, entries, truncated: false }.encode().unwrap();
            let back = DirListing::decode(&dir).unwrap();
            prop_assert!(back.entries.capacity() <= dir.len());
            prop_assert!(back.path.capacity() <= dir.len());
            let wake = WakeInfo { macs }.encode().unwrap();
            prop_assert!(WakeInfo::decode(&wake).unwrap().macs.capacity() <= wake.len());
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
}

/// A `Hello` payload with a chosen `name` field (raw bytes after a chosen length) and valid everything else.
fn hello_with_name(declared_len: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = vec![0, 0, 0]; // proto_minor, app_version ""
    p.extend_from_slice(declared_len);
    p.extend_from_slice(name);
    p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // os, os_version "", intent, requested, resume, swap_token
    p
}

#[test]
fn strings_over_limit_with_bad_utf8_or_with_nul_are_refused() {
    let ok = hello_with_name(&[2], b"ab");
    assert!(Message::decode_payload(1, &ok).is_ok());
    assert!(Message::decode_payload(1, &hello_with_name(&varint(64), &[b'a'; 64])).is_ok());
    let mut over = varint(65);
    let name = vec![b'a'; 65];
    assert!(matches!(Message::decode_payload(1, &hello_with_name(&over, &name)), Err(CodecError::OverLimit { .. })));
    over = varint(HUGE);
    assert!(matches!(Message::decode_payload(1, &hello_with_name(&over, &[])), Err(CodecError::OverLimit { .. })));
    for bad in [&[0xFF][..], &[0xC3], &[0xE2, 0x82], &[0xC0, 0x80], &[0xED, 0xA0, 0x80], &[0xF4, 0x90, 0x80, 0x80]] {
        let p = hello_with_name(&varint(bad.len() as u64), bad);
        assert_eq!(Message::decode_payload(1, &p), Err(CodecError::InvalidUtf8), "{bad:x?}");
    }
    assert_eq!(Message::decode_payload(1, &hello_with_name(&[3], b"a\0b")), Err(CodecError::InteriorNul));
    // the same on the encode side
    let mut hello = match Message::decode_payload(1, &ok).unwrap() {
        Message::Hello(h) => h,
        _ => unreachable!(),
    };
    hello.name = "a\0".into();
    assert!(Message::Hello(hello.clone()).encode_frame().is_err());
    hello.name = "a".repeat(65);
    assert!(Message::Hello(hello.clone()).encode_frame().is_err());
    hello.name = "a".repeat(64);
    assert!(Message::Hello(hello).encode_frame().is_ok());
}

#[test]
fn chat_text_limit_is_4096_bytes() {
    let chat = |n: usize| Message::ChatMessage(ChatMessage { msg_id: 1, text: "x".repeat(n), sent_at_ms: 2 });
    let frame = chat(4096).encode_frame().unwrap();
    assert_eq!(Message::decode_frame_exact(&frame).unwrap(), chat(4096));
    assert!(chat(4097).encode_frame().is_err());
    let mut payload = 1u32.to_le_bytes().to_vec();
    payload.extend(varint(4097));
    payload.extend(core::iter::repeat_n(b'x', 4097));
    payload.extend_from_slice(&[0; 8]);
    assert!(matches!(Message::decode_payload(0x0301, &payload), Err(CodecError::OverLimit { .. })));
    // multi-byte characters count in bytes: 2049 two-byte characters are 4098 bytes
    assert!(
        Message::ChatMessage(ChatMessage { msg_id: 1, text: "é".repeat(2049), sent_at_ms: 2 }).encode_frame().is_err()
    );
}

#[test]
fn every_unknown_tag_is_fatal() {
    fn check_u8<T: Wire + Debug>(known: &[u8]) {
        for tag in 0..=255u8 {
            let r = T::decode(&[tag]);
            if known.contains(&tag) {
                assert!(r.is_ok(), "{tag}");
            } else {
                assert!(matches!(r, Err(CodecError::UnknownTag { .. })), "tag {tag}: {r:?}");
            }
        }
    }
    check_u8::<Os>(&[0, 1]);
    check_u8::<Codec>(&[1]);
    check_u8::<AudioCodec>(&[1]);
    check_u8::<Intent>(&[0, 1]);
    check_u8::<H264Profile>(&[0, 1]);
    check_u8::<KbKind>(&[0, 1, 2, 3, 4]);
    check_u8::<GrantMode>(&[0, 1, 2, 3]);
    check_u8::<Desktop>(&[0, 1, 2, 3]);
    check_u8::<Feature>(&[0, 1]);
    check_u8::<Preset>(&[0, 1, 2]);
    check_u8::<Special>(&[0]);
    check_u8::<ClipSkip>(&[0, 1, 2]);
    check_u8::<Action>(&[0, 1, 2, 3]);
    check_u8::<MouseButton>(&[1, 2, 3, 4, 5]);
    check_u8::<ClipKind>(&[1, 2]);
}

#[test]
fn error_codes_outside_section_8_are_fatal() {
    for code in 0..=u16::MAX {
        let r = ErrorCode::decode(&code.to_le_bytes());
        if code <= 21 {
            assert_eq!(r.unwrap().tag(), code);
        } else {
            assert!(matches!(r, Err(CodecError::UnknownTag { .. })), "{code}");
        }
    }
}

#[test]
fn bad_tags_inside_messages_are_fatal() {
    let good = hello_with_name(&[0], &[]);
    assert!(Message::decode_payload(1, &good).is_ok());
    // layout: proto_minor(2) app_version(1) name(1) os(1) os_version(1) intent(1) requested(2) resume(1) swap(1)
    for (offset, what) in [(4usize, "os"), (6, "intent"), (9, "resume option"), (10, "swap option")] {
        let mut b = good.clone();
        b[offset] = 2;
        assert!(Message::decode_payload(1, &b).is_err(), "{what}");
    }
    // a bool of 2
    assert_eq!(Message::decode_payload(0x0107, &[2]), Err(CodecError::InvalidBool));
    assert_eq!(Message::decode_payload(0x0107, &[1]).unwrap(), Message::AudioControl(AudioControl { enabled: true }));
    // an unknown enum value in PairResult.code and Error.code
    assert!(Message::decode_payload(0x0705, &[1, 22, 0]).is_err());
    assert!(Message::decode_payload(0x0FFF, &[22, 0, 1, 0, 0, 0, 0, 0]).is_err());
}

#[test]
fn permission_sets_that_break_implies_or_use_reserved_bits_are_fatal() {
    let grant = |bits: u16| {
        let mut p = 5u64.to_le_bytes().to_vec();
        p.extend_from_slice(&bits.to_le_bytes());
        p.push(0);
        p.extend_from_slice(&[0; 16]);
        Message::decode_payload(0x0005, &p)
    };
    assert!(grant(PermissionSet::CONTROL.bits()).is_ok());
    assert!(grant(0b10).is_err(), "CONTROL without VIEW");
    assert!(grant(0b1_0000).is_err(), "ADMIN_TERMINAL without TERMINAL");
    assert!(grant(0b1000_0000).is_err(), "UNATTENDED without VIEW");
    assert!(grant(0x0101).is_err(), "reserved bit 8");
    assert!(grant(0x8000).is_err(), "reserved bit 15");
}

#[test]
fn non_minimal_varints_are_refused_everywhere_they_appear() {
    // frame type and length (also exercised for every message in check_message)
    assert_eq!(Message::decode_frame(&[0x40, 0x08, 0x00]), Err(CodecError::NonMinimalVarint));
    assert_eq!(Message::decode_frame(&[0x08, 0x40, 0x02, 0x00, 0x00]), Err(CodecError::NonMinimalVarint));
    // a string length: Hello.name declared as a two-byte varint
    let p = hello_with_name(&[0x40, 0x02], b"ab");
    assert_eq!(Message::decode_payload(1, &p), Err(CodecError::NonMinimalVarint));
    // a vector count
    assert_eq!(Message::decode_payload(0x0101, &[0x40, 0x00]), Err(CodecError::NonMinimalVarint));
    // a stream kind
    assert_eq!(StreamKind::decode(&[0x40, 0x02]), Err(CodecError::NonMinimalVarint));
    assert_eq!(StreamKind::decode(&[0x80, 0, 0, 0x02]), Err(CodecError::NonMinimalVarint));
    assert_eq!(StreamKind::decode(&[0xC0, 0, 0, 0, 0, 0, 0, 0x02]), Err(CodecError::NonMinimalVarint));
}

#[test]
fn unknown_types_and_oversized_frames_are_refused_before_the_payload() {
    for ty in [0u64, 0x0010, 0x0100, 0x0700, 0x0706, 0x0FFE, 0x1000, 1 << 20, HUGE] {
        let mut f = varint(ty);
        f.extend(varint(3));
        let err = Message::decode_frame(&f).unwrap_err();
        assert!(matches!(err, CodecError::UnknownTag { .. }), "{ty:#x}: {err}");
        assert!(Message::decode_payload(ty, &[0; 3]).is_err());
    }
    for len in [65_536u64, 1 << 20, 1 << 32, HUGE] {
        let mut f = varint(0x0F01);
        f.extend(varint(len));
        let err = Message::decode_frame(&f).unwrap_err();
        assert!(matches!(err, CodecError::TooLarge { .. }), "{len}: {err}");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }
    // 65 535 is allowed: only the missing bytes fail it
    let mut f = varint(0x0F01);
    f.extend(varint(65_535));
    assert_eq!(Message::decode_frame(&f), Err(CodecError::Truncated));
}

#[test]
fn media_limits_are_checked_before_any_payload_is_read() {
    // video: payload_len above 8 MiB
    let mut header = VideoHeader {
        keyframe: false,
        config: false,
        discontinuity: false,
        display_id: 0,
        frame_id: 0,
        capture_us: 0,
        encode_us: 0,
        width: 1,
        height: 1,
        codec: Codec::H264,
        payload_len: MAX_VIDEO_PAYLOAD,
    };
    let mut b = header.encode().unwrap();
    assert!(VideoHeader::decode(&b).is_ok());
    b[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(VideoHeader::decode(&b), Err(CodecError::TooLarge { .. })));
    header.payload_len = MAX_VIDEO_PAYLOAD + 1;
    assert!(header.encode().is_err());
    // clipboard: len above the limit of its kind, then a legal len with no bytes behind it
    for (kind, limit) in [(1u8, MAX_CLIPBOARD_TEXT), (2, MAX_CLIPBOARD_PNG)] {
        let mut b = 9u32.to_le_bytes().to_vec();
        b.push(kind);
        b.extend((limit + 1).to_le_bytes());
        assert!(matches!(ClipboardItem::decode(&b), Err(CodecError::TooLarge { .. })));
        b.truncate(5);
        b.extend(limit.to_le_bytes());
        assert_eq!(ClipboardItem::decode(&b), Err(CodecError::Truncated));
        b.truncate(5);
        b.extend(u32::MAX.to_le_bytes());
        assert!(matches!(ClipboardItem::decode(&b), Err(CodecError::TooLarge { .. })));
    }
    // clipboard text that is not UTF-8
    let mut b = 1u32.to_le_bytes().to_vec();
    b.push(1);
    b.extend(2u32.to_le_bytes());
    b.extend([0xC3, 0x28]);
    assert_eq!(ClipboardItem::decode(&b), Err(CodecError::InvalidUtf8));
    // cursor shape: dimensions above 256 are refused before the RGBA is wanted
    let mut b = 1u32.to_le_bytes().to_vec();
    b.extend(257u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend([0; 4]);
    assert!(matches!(CursorShape::decode(&b), Err(CodecError::OverLimit { .. })));
    // cursor shape at 256 × 256 declared with no data: truncated, nothing reserved
    let mut b = 1u32.to_le_bytes().to_vec();
    b.extend(256u16.to_le_bytes());
    b.extend(256u16.to_le_bytes());
    b.extend([0; 4]);
    assert_eq!(CursorShape::decode(&b), Err(CodecError::Truncated));
    // input text: 256 bytes ok, 257 refused, bad UTF-8 refused, unknown tag refused
    let text_record = |n: usize, byte: u8| {
        let mut b = vec![0x02];
        b.extend((n as u16).to_le_bytes());
        b.extend(core::iter::repeat_n(byte, n));
        b
    };
    assert!(InputEvent::decode(&text_record(256, b'a')).is_ok());
    assert!(matches!(InputEvent::decode(&text_record(257, b'a')), Err(CodecError::OverLimit { .. })));
    assert_eq!(InputEvent::decode(&text_record(1, 0xFF)), Err(CodecError::InvalidUtf8));
    let mut declared_huge = vec![0x02];
    declared_huge.extend(u16::MAX.to_le_bytes());
    assert!(matches!(InputEvent::decode(&declared_huge), Err(CodecError::OverLimit { .. })));
    for tag in [0x00u8, 0x07, 0x7F, 0xFF] {
        assert!(matches!(InputEvent::decode(&[tag]), Err(CodecError::UnknownTag { .. })));
    }
    // key: unknown usage page, lock bits beyond Scroll, bool of 2; button 0 and 6
    assert!(InputEvent::decode(&[1, 0x09, 0, 4, 0, 1, 0, 0]).is_err());
    assert!(InputEvent::decode(&[1, 0x07, 0, 4, 0, 1, 0, 8]).is_err());
    assert!(InputEvent::decode(&[1, 0x07, 0, 4, 0, 2, 0, 0]).is_err());
    assert!(InputEvent::decode(&[4, 0, 1]).is_err());
    assert!(InputEvent::decode(&[4, 6, 1]).is_err());
    assert!(InputEvent::decode(&[4, 1, 1]).is_ok());
}

#[test]
fn headers_refuse_wrong_kinds_versions_flags_and_reserved_bytes() {
    let video = VideoHeader {
        keyframe: true,
        config: true,
        discontinuity: true,
        display_id: 1,
        frame_id: 2,
        capture_us: 3,
        encode_us: 4,
        width: 5,
        height: 6,
        codec: Codec::H264,
        payload_len: 7,
    }
    .encode()
    .unwrap();
    for off in [0usize, 1, 24, 25, 26, 27] {
        for val in 0..=255u8 {
            let mut b = video.clone();
            b[off] = val;
            let ok = VideoHeader::decode(&b).is_ok();
            let want = match off {
                0 => val == 1,
                1 => val < 8,
                24 => val == 1,
                _ => val == 0,
            };
            assert_eq!(ok, want, "offset {off} value {val}");
        }
    }
    let audio = AudioHeader { discontinuity: true, muted: true, frame_ms: 20, seq: 1, capture_us: 2 }.encode().unwrap();
    for val in 0..=255u8 {
        let mut b = audio.clone();
        b[0] = val;
        assert_eq!(AudioHeader::decode(&b).is_ok(), val == 1);
        let mut b = audio.clone();
        b[1] = val;
        assert_eq!(AudioHeader::decode(&b).is_ok(), val < 4);
    }
    let cursor = CursorPosition { visible: true, display: 1, seq: 2, x: 3, y: 4, shape_id: 5 }.encode().unwrap();
    for val in 0..=255u8 {
        let mut b = cursor.clone();
        b[0] = val;
        assert_eq!(CursorPosition::decode(&b).is_ok(), val == 2);
        let mut b = cursor.clone();
        b[1] = val;
        assert_eq!(CursorPosition::decode(&b).is_ok(), val < 2);
    }
    assert_eq!(video.len(), 32);
    assert_eq!(audio.len(), 16);
    assert_eq!(cursor.len(), 16);
}

#[test]
fn pairing_inputs_are_plain_concatenations() {
    let mut runner = TestRunner::new(Config { cases: 128, failure_persistence: None, ..Config::default() });
    let strategy = (
        proptest::collection::vec(any::<u8>(), 0..64),
        proptest::collection::vec(any::<u8>(), 0..64),
        any::<[u8; 32]>(),
        any::<[u8; 32]>(),
        any::<[u8; 32]>(),
    );
    runner
        .run(&strategy, |(spki_i, spki_r, ek, ni, nr)| {
            let c = CommitmentInput { spki_i: &spki_i, nonce_i: &ni }.preimage();
            prop_assert_eq!(c.len(), COMMIT_LABEL.len() + spki_i.len() + 32);
            prop_assert_eq!(&c[..COMMIT_LABEL.len()], COMMIT_LABEL);
            prop_assert_eq!(&c[c.len() - 32..], &ni[..]);
            let s = SasInput { ek: &ek, spki_i: &spki_i, spki_r: &spki_r, nonce_i: &ni, nonce_r: &nr }.preimage();
            prop_assert_eq!(s.len(), SAS_LABEL.len() + 32 + spki_i.len() + spki_r.len() + 64);
            prop_assert_eq!(&s[s.len() - 32..], &nr[..]);
            prop_assert!(Sas::from_digest(&ek).value() < 1_000_000);
            prop_assert_eq!(Sas::from_digest(&ek).to_string().len(), 7);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
}
