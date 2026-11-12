//! Proof of PROTOCOL.md 9.3: round trips for every message and header, `encode(decode(b)) == b` for accepted
//! bytes, and rejection (without panics or large allocations) of truncation, trailing bytes, over-limit lengths,
//! huge declared counts, bad UTF-8, bad tags and non-minimal varints.
//!
//! "Without large allocations": `forbid(unsafe_code)` rules out a counting allocator, so the tests feed tiny
//! inputs that declare 2^62 items or bytes; a decoder that reserved from the declared count would abort the test
//! process (capacity overflow or allocation failure). Accepted values are checked to hold no more capacity than
//! the input had bytes.

mod common;

use core::fmt::Debug;

use common::*;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, TestRunner};
use swapdesk_proto::*;

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
    check_u8::<Codec>(&[1, 2]);
    check_u8::<AudioCodec>(&[1]);
    check_u8::<Intent>(&[0, 1]);
    check_u8::<H264Profile>(&[0, 1]);
    check_u8::<KbKind>(&[0, 1, 2, 3, 4]);
    check_u8::<GrantMode>(&[0, 1, 2, 3]);
    check_u8::<Desktop>(&[0, 1, 2, 3, 4]);
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
        if code <= 26 {
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
    assert!(Message::decode_payload(0x0705, &[1, 27, 0]).is_err());
    assert!(Message::decode_payload(0x0FFF, &[27, 0, 1, 0, 0, 0, 0, 0]).is_err());
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
                24 => val == 1 || val == 2,
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
    let strategy = (any::<[u8; 44]>(), any::<[u8; 44]>(), any::<[u8; 32]>(), any::<[u8; 32]>(), any::<[u8; 32]>());
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

// ---------- minor 1 ----------

#[test]
fn stored_grants_and_grants_sync_round_trip_byte_for_byte() {
    for bits in 0..=0x00FFu16 {
        let Some(grants) = PermissionSet::from_bits(bits) else { continue };
        let stored = Message::StoredGrants(StoredGrants { grants });
        let frame = stored.encode_frame().unwrap();
        assert_eq!(Message::decode_frame_exact(&frame).unwrap(), stored);
        assert_eq!(frame, [&[0x0B, 0x02][..], &bits.to_le_bytes()].concat());
        for proto_minor in [1u16, 2, 0x1234, u16::MAX] {
            let sync = Message::GrantsSync(GrantsSync { proto_minor, grants });
            let frame = sync.encode_frame().unwrap();
            assert_eq!(Message::decode_frame_exact(&frame).unwrap(), sync);
            assert_eq!(frame, [&[0x48, 0x01, 0x04][..], &proto_minor.to_le_bytes(), &bits.to_le_bytes()].concat());
        }
    }
}

#[test]
fn stored_grants_and_grants_sync_have_the_documented_bytes() {
    let stored = Message::StoredGrants(StoredGrants { grants: PermissionSet::CONTROL });
    assert_eq!(stored.encode_frame().unwrap(), [0x0B, 0x02, 0x03, 0x00]);
    let sync = Message::GrantsSync(GrantsSync { proto_minor: 1, grants: PermissionSet::CONTROL });
    assert_eq!(sync.encode_frame().unwrap(), [0x48, 0x01, 0x04, 0x01, 0x00, 0x03, 0x00]);
    // Neither travels on the pairing endpoint.
    assert!(!stored.is_pairing() && !sync.is_pairing());
}

#[test]
fn a_grant_set_that_breaks_implies_or_uses_reserved_bits_is_rejected_in_both() {
    for (bits, what) in [
        (0b10u16, "CONTROL without VIEW"),
        (0b1_0000, "ADMIN_TERMINAL without TERMINAL"),
        (0b1000_0000, "UNATTENDED without VIEW"),
        (0x0101, "reserved bit 8"),
        (0x8000, "reserved bit 15"),
    ] {
        let b = bits.to_le_bytes();
        assert_eq!(
            Message::decode_payload(0x000B, &b),
            Err(CodecError::InvalidValue("permission set")),
            "StoredGrants: {what}"
        );
        assert_eq!(
            Message::decode_payload(0x0801, &[1, 0, b[0], b[1]]),
            Err(CodecError::InvalidValue("permission set")),
            "GrantsSync: {what}"
        );
    }
    // The payload is exactly its fields: nothing short, nothing after them.
    assert!(Message::decode_payload(0x000B, &[3]).is_err());
    assert_eq!(Message::decode_payload(0x000B, &[3, 0, 0]), Err(CodecError::TrailingBytes));
    assert!(Message::decode_payload(0x0801, &[1, 0, 3]).is_err());
    assert_eq!(Message::decode_payload(0x0801, &[1, 0, 3, 0, 0]), Err(CodecError::TrailingBytes));
}

#[test]
fn elevate_and_account_away_round_trip_byte_for_byte() {
    let elevate = Message::Elevate(Elevate {});
    let frame = elevate.encode_frame().unwrap();
    assert_eq!(Message::decode_frame_exact(&frame).unwrap(), elevate);
    // No fields: any payload byte is one too many.
    assert_eq!(Message::decode_payload(0x000C, &[0]), Err(CodecError::TrailingBytes));

    let away: Vec<Message> = vec![
        SessionRefused { code: ErrorCode::AccountAway }.into(),
        Bye { code: ErrorCode::AccountAway }.into(),
        ErrorMessage { code: ErrorCode::AccountAway, fatal: false, about_type: 0x000C, about_id: 0 }.into(),
        HostState { desktop: Desktop::AccountAway, paused: true }.into(),
        HostState { desktop: Desktop::AccountAway, paused: false }.into(),
    ];
    for m in away {
        let frame = m.encode_frame().unwrap();
        assert_eq!(Message::decode_frame_exact(&frame).unwrap(), m, "{m:?}");
        assert_eq!(Message::decode_frame_exact(&frame).unwrap().encode_frame().unwrap(), frame);
    }
}

#[test]
fn they_have_the_documented_bytes() {
    assert_eq!(Message::Elevate(Elevate {}).encode_frame().unwrap(), [0x0C, 0x00]);
    let refused = Message::SessionRefused(SessionRefused { code: ErrorCode::AccountAway });
    assert_eq!(refused.encode_frame().unwrap(), [0x06, 0x02, 0x16, 0x00]);
    let state = Message::HostState(HostState { desktop: Desktop::AccountAway, paused: true });
    assert_eq!(state.encode_frame().unwrap(), [0x09, 0x02, 0x04, 0x01]);
}

/// The minor of a message is the newest of its type and of the enum values it carries, so one check before a
/// send keeps everything of minor 1 away from a 1.0 peer.
#[test]
fn every_type_names_the_minor_that_introduced_it() {
    for id in Message::ALL_TYPE_IDS {
        let want = u16::from(matches!(*id, 0x000B | 0x000C | 0x0801));
        assert_eq!(Message::type_min_minor(*id), want, "{id:#06x}");
    }
    assert_eq!(Message::type_min_minor(0x3FFF), 0, "an unknown type names no minor");

    // Every generated message of a 1.0 type that carries no newer value is of minor 0; the new types are of 1.
    let mut runner = TestRunner::new(Config { cases: 64, failure_persistence: None, ..Config::default() });
    for strategy in message_strategies() {
        runner
            .run(&strategy, |m| {
                let carries_new = format!("{m:?}").contains("AccountAway");
                let want = u16::from(Message::type_min_minor(m.type_id()) == 1 || carries_new);
                prop_assert_eq!(m.min_minor(), want, "{:?}", m);
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{e}"));
    }

    // Each message that can carry a value of minor 1 says so.
    let code = ErrorCode::AccountAway;
    let carriers: Vec<Message> = vec![
        SessionRefused { code }.into(),
        Bye { code }.into(),
        SwapDecline { request_id: 1, code }.into(),
        FileDecline { transfer_id: 1, code }.into(),
        QuickActionResult { action_id: 1, code }.into(),
        PairResult { ok: false, code }.into(),
        ErrorMessage { code, fatal: true, about_type: 0, about_id: 0 }.into(),
        HostState { desktop: Desktop::AccountAway, paused: true }.into(),
    ];
    for m in carriers {
        assert_eq!(m.min_minor(), 1, "{m:?}");
        assert_eq!(Message::type_min_minor(m.type_id()), 0, "{m:?}: its type is of 1.0");
    }
    assert_eq!(Message::from(Bye { code: ErrorCode::NoError }).min_minor(), 0);
}

/// The one mapping the send path applies: a peer of minor 0 gets no minor-1 type, and each minor-1 value in the
/// form its own minor has for it. A peer of minor 1 gets everything as it is.
#[test]
fn no_minor_1_type_or_value_reaches_a_minor_0_peer_in_any_message() {
    let away = ErrorCode::AccountAway;
    let carriers: Vec<Message> = vec![
        SessionRefused { code: away }.into(),
        Bye { code: away }.into(),
        SwapDecline { request_id: 7, code: away }.into(),
        FileDecline { transfer_id: 7, code: away }.into(),
        QuickActionResult { action_id: 7, code: away }.into(),
        PairResult { ok: false, code: away }.into(),
        ErrorMessage { code: away, fatal: true, about_type: 3, about_id: 7 }.into(),
        HostState { desktop: Desktop::AccountAway, paused: false }.into(),
    ];
    for m in carriers {
        assert_eq!(m.clone().for_peer(1), Some(m.clone()), "a peer of minor 1 gets it whole");
        let old = m.clone().for_peer(0).expect("its type is of 1.0: it is sent, in its 1.0 form");
        assert_eq!((old.type_id(), old.min_minor()), (m.type_id(), 0), "{m:?} -> {old:?}");
        assert!(format!("{old:?}").contains(if matches!(m, Message::HostState(_)) {
            "Locked"
        } else {
            "NobodyPresent"
        }));
        // What 1.0 can decode: the frame round-trips, and nothing else of the message moved.
        assert_eq!(Message::decode_frame_exact(&old.encode_frame().unwrap()).unwrap(), old);
    }
    // A desktop the old peer cannot be told of is one it must not act on: it reads as locked and paused.
    let state = Message::from(HostState { desktop: Desktop::AccountAway, paused: false }).for_peer(0);
    assert_eq!(state, Some(HostState { desktop: Desktop::Locked, paused: true }.into()));
    // The types of minor 1 are not sent at all.
    for m in [
        Message::StoredGrants(StoredGrants { grants: PermissionSet::VIEW }),
        Message::Elevate(Elevate {}),
        Message::GrantsSync(GrantsSync { proto_minor: 1, grants: PermissionSet::VIEW }),
    ] {
        assert_eq!(m.clone().for_peer(0), None, "{m:?}");
        assert_eq!(m.clone().for_peer(1), Some(m));
    }
    // Every message of 1.0 is untouched for every peer.
    let mut runner = TestRunner::new(Config { cases: 32, failure_persistence: None, ..Config::default() });
    for strategy in message_strategies() {
        runner
            .run(&strategy, |m| {
                if m.min_minor() == 0 {
                    prop_assert_eq!(m.clone().for_peer(0), Some(m.clone()));
                }
                prop_assert_eq!(m.clone().for_peer(u16::MAX), Some(m));
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{e}"));
    }
}

fn caps_with(codecs: &[Codec]) -> Capabilities {
    Capabilities {
        video_decode: codecs
            .iter()
            .map(|c| VideoCap {
                codec: *c,
                profile: H264Profile::Main,
                max_level: 51,
                max_width: 3840,
                max_height: 2160,
                max_fps: 60,
            })
            .collect(),
        audio: vec![AudioCodec::Opus48Stereo],
        max_datagram: 1200,
        keyboard: KeyboardInfo { kind: KbKind::Unknown, layout: String::new() },
        features: 0,
    }
}

#[test]
fn an_hevc_decoder_is_never_listed_to_a_minor_0_peer() {
    let both = Message::Capabilities(caps_with(&[Codec::Hevc, Codec::H264]));
    assert_eq!(both.min_minor(), 1);
    assert_eq!(both.clone().for_peer(1), Some(both.clone()), "a minor-1 peer reads the HEVC entry");
    let old = both.for_peer(0).expect("Capabilities is a 1.0 type");
    assert_eq!(old, Message::Capabilities(caps_with(&[Codec::H264])));
    assert_eq!(old.min_minor(), 0);
    assert_eq!(Message::decode_frame_exact(&old.encode_frame().unwrap()).unwrap(), old);
    // A peer that lists only H.264 is sent its capabilities as they are.
    let h264 = Message::Capabilities(caps_with(&[Codec::H264]));
    assert_eq!(h264.clone().for_peer(0), Some(h264));
}

#[test]
fn every_capability_combination_picks_the_expected_codec() {
    for viewer in [&[][..], &[Codec::H264], &[Codec::Hevc], &[Codec::Hevc, Codec::H264]] {
        for minor in [0u16, 1, 7] {
            for host_hw in [false, true] {
                let want = if host_hw && minor >= 1 && viewer.contains(&Codec::Hevc) {
                    Codec::Hevc
                } else {
                    Codec::H264
                };
                assert_eq!(caps_with(viewer).pick_codec(minor, host_hw), want, "{viewer:?} minor {minor} hw {host_hw}");
            }
        }
    }
}

#[test]
fn a_video_header_in_hevc_round_trips() {
    let header = VideoHeader {
        keyframe: true,
        config: true,
        discontinuity: false,
        display_id: 1,
        frame_id: 9,
        capture_us: 5,
        encode_us: 3,
        width: 1920,
        height: 1080,
        codec: Codec::Hevc,
        payload_len: 4,
    };
    let mut w = swapdesk_proto::codec::Writer::new();
    swapdesk_proto::codec::Wire::put(&header, &mut w, 0).unwrap();
    let bytes = w.into_bytes();
    let mut r = swapdesk_proto::codec::Reader::new(&bytes);
    assert_eq!(<VideoHeader as swapdesk_proto::codec::Wire>::get(&mut r, 0).unwrap(), header);
}
