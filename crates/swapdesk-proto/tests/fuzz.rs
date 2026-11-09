//! Fuzzing of every decoder that reads bytes from the network (stage 29), with proptest.
//!
//! Each target is one property in two tests: `<target>::fuzz` runs a few hundred cases inside the normal suite,
//! `<target>::long` (`#[ignore = "long: fuzz"]`) runs millions in a release build:
//!
//! ```text
//! cargo test -p swapdesk-proto --release --test fuzz -- --ignored --nocapture
//! ```
//!
//! `SWAPDESK_FUZZ_CASES=<n>` sets the case count of every `long` test; each prints its cases, the decode calls
//! behind them and the seconds. The properties, per decoder:
//! - arbitrary bytes, structured byte soup that reaches deep into the field lists, and arbitrary bytes after a
//!   valid header never panic and never reserve memory from an unchecked length (`forbid(unsafe_code)` rules out
//!   a counting allocator: the soup declares counts and lengths up to 2^62 over tiny inputs, and a decoder that
//!   reserved from them would abort the process; the accepted values are bounded by the input);
//! - whatever decodes re-encodes to the same bytes (the encoding is canonical);
//! - every truncation of a valid encoding is refused, and so is every single-byte mutation that breaks the
//!   format; a mutation that is accepted re-encodes to the mutated bytes;
//! - arbitrary valid values (strategies of `common`) survive encode and decode.

mod common;

use core::fmt::Debug;
use std::cell::Cell;
use std::time::Instant;

use common::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use swapdesk_proto::codec::MAX_VARINT;
use swapdesk_proto::*;

// ---------- harness ----------

thread_local! {
    /// Decoder calls made by the target running on this thread.
    static CALLS: Cell<u64> = const { Cell::new(0) };
    /// Inputs of that target that decoded (the depth the byte soup reached).
    static ACCEPTED: Cell<u64> = const { Cell::new(0) };
}

fn accepted() {
    ACCEPTED.with(|c| c.set(c.get() + 1));
}

fn calls(n: u64) {
    CALLS.with(|c| c.set(c.get() + n));
}

fn long_cases(default: u32) -> u32 {
    std::env::var("SWAPDESK_FUZZ_CASES").ok().and_then(|v| v.replace('_', "").parse().ok()).unwrap_or(default)
}

fn run_target<S: Strategy>(name: &str, cases: u32, strategy: S, check: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let start = Instant::now();
    CALLS.with(|c| c.set(0));
    ACCEPTED.with(|c| c.set(0));
    let mut runner = TestRunner::new(Config { cases, failure_persistence: None, ..Config::default() });
    runner.run(&strategy, check).unwrap_or_else(|e| panic!("{name}: {e}"));
    println!(
        "FUZZ proto {name}: {cases} cases, {} decoder calls, {} inputs accepted, {:.1}s",
        CALLS.with(Cell::get),
        ACCEPTED.with(Cell::get),
        start.elapsed().as_secs_f64()
    );
}

/// `name(small, long) => |name, cases| run_target(..)`: the in-suite test and the ignored long one.
macro_rules! targets {
    ($( $name:ident ($small:expr, $long:expr) => $run:expr; )+) => { $(
        mod $name {
            use super::*;

            #[test]
            fn fuzz() {
                let run: fn(&str, u32) = $run;
                run(stringify!($name), $small);
            }

            #[test]
            #[ignore = "long: fuzz"]
            fn long() {
                let run: fn(&str, u32) = $run;
                run(stringify!($name), long_cases($long));
            }
        }
    )+ };
}

// ---------- byte strategies ----------

/// One piece of a byte soup: the shapes the decoders read (small varints, tags, strings, fixed integers, long
/// varints that declare huge lengths), so that a random sequence gets past the first checks.
fn token() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => (0u64..8).prop_map(varint),
        2 => (0u64..300).prop_map(varint),
        1 => any::<u64>().prop_map(|v| varint(v & MAX_VARINT)),
        1 => Just(varint(MAX_VARINT)),
        5 => (0u8..4).prop_map(|b| vec![b]),
        2 => any::<u8>().prop_map(|b| vec![b]),
        3 => vec(0x20u8..0x7F, 0..12).prop_map(|s| {
            let mut out = varint(s.len() as u64);
            out.extend(s);
            out
        }),
        1 => any::<u16>().prop_map(|v| v.to_le_bytes().to_vec()),
        1 => any::<u32>().prop_map(|v| v.to_le_bytes().to_vec()),
        1 => any::<u64>().prop_map(|v| v.to_le_bytes().to_vec()),
        1 => (0usize..17).prop_map(|n| vec![0; n]),
        1 => any::<[u8; 16]>().prop_map(|a| a.to_vec()),
        1 => any::<[u8; 32]>().prop_map(|a| a.to_vec()),
        1 => vec(any::<u8>(), 0..8),
    ]
}

/// Structured byte soup.
fn soup() -> impl Strategy<Value = Vec<u8>> {
    vec(token(), 0..24).prop_map(|parts| parts.concat())
}

/// Soup half of the time, plain random bytes the other half.
fn bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![3 => soup(), 1 => vec(any::<u8>(), 0..64)]
}

fn fail(msg: String) -> TestCaseError {
    TestCaseError::fail(msg)
}

// ---------- checks ----------

/// Arbitrary bytes into a whole-buffer decoder: no panic; accepted bytes are canonical, every strict prefix of
/// them is refused, and a mutation is refused or re-encodes to itself.
fn check_bytes<T: Wire + Debug>(input: &[u8]) -> Result<(), TestCaseError> {
    calls(2);
    if let Ok((y, used)) = T::decode_prefix(input) {
        prop_assert_eq!(y.encode().map_err(|e| fail(format!("{e}")))?, &input[..used]);
    }
    let Ok(y) = T::decode(input) else {
        return Ok(());
    };
    accepted();
    prop_assert_eq!(y.encode().map_err(|e| fail(format!("{e}")))?, input);
    for cut in 0..input.len() {
        calls(1);
        prop_assert!(T::decode(&input[..cut]).is_err(), "prefix of {} bytes decoded", cut);
    }
    // the value is small here, so every position is mutated
    for i in 0..input.len().min(96) {
        for flip in [0x01u8, 0x40, 0x80, 0xFF] {
            let mut m = input.to_vec();
            m[i] ^= flip;
            calls(1);
            if let Ok(z) = T::decode(&m) {
                prop_assert_eq!(z.encode().map_err(|e| fail(format!("{e}")))?, m);
            }
        }
    }
    Ok(())
}

/// A valid value, then arbitrary bytes after it: the value decodes alone from the front, and the whole is
/// `TrailingBytes` when the suffix is not empty. Then the generic round trip of `check_wire`.
fn check_value<T: Wire + Debug + PartialEq>(x: &T, suffix: &[u8]) -> Result<(), TestCaseError> {
    calls(1);
    check_wire(x)?;
    let enc = x.encode().map_err(|e| fail(format!("{e}")))?;
    let mut all = enc.clone();
    all.extend_from_slice(suffix);
    calls(2);
    let (again, used) = T::decode_prefix(&all).map_err(|e| fail(format!("{e}")))?;
    prop_assert_eq!(&again, x);
    prop_assert_eq!(used, enc.len());
    if !suffix.is_empty() {
        prop_assert_eq!(T::decode(&all).err(), Some(CodecError::TrailingBytes));
    }
    Ok(())
}

/// A payload of message `id`: no panic, canonical when accepted, every strict prefix refused, mutations
/// refused or canonical.
fn check_payload(id: u16, payload: &[u8]) -> Result<(), TestCaseError> {
    calls(1);
    let Ok(m) = Message::decode_payload(u64::from(id), payload) else {
        return Ok(());
    };
    accepted();
    prop_assert_eq!(m.type_id(), id);
    prop_assert_eq!(m.encode_payload().map_err(|e| fail(format!("{e}")))?, payload);
    prop_assert_eq!(
        Message::decode_payload(u64::from(id), &[payload, &[0]].concat()).err(),
        Some(CodecError::TrailingBytes)
    );
    for cut in 0..payload.len() {
        calls(1);
        prop_assert!(Message::decode_payload(u64::from(id), &payload[..cut]).is_err(), "prefix {} decoded", cut);
    }
    for i in 0..payload.len().min(96) {
        for flip in [0x01u8, 0x40, 0x80, 0xFF] {
            let mut mutated = payload.to_vec();
            mutated[i] ^= flip;
            calls(1);
            if let Ok(z) = Message::decode_payload(u64::from(id), &mutated) {
                prop_assert_eq!(z.encode_payload().map_err(|e| fail(format!("{e}")))?, mutated);
            }
        }
    }
    Ok(())
}

/// Arbitrary bytes into the frame decoder: no panic, `used` is inside the input, an accepted frame is canonical
/// and a prefix of it is `Truncated` or refused.
fn check_frame_bytes(input: &[u8]) -> Result<(), TestCaseError> {
    calls(1);
    if let Ok((m, used)) = Message::decode_frame(input) {
        accepted();
        prop_assert!(used <= input.len());
        prop_assert!(used > 0);
        let again = m.encode_frame().map_err(|e| fail(format!("{e}")))?;
        prop_assert_eq!(&again[..], &input[..used]);
        for cut in 0..used {
            calls(1);
            prop_assert!(Message::decode_frame(&input[..cut]).is_err(), "frame prefix {} decoded", cut);
        }
        calls(1);
        prop_assert_eq!(Message::decode_frame_exact(&input[..used]).ok(), Some(m));
    }
    calls(1);
    let _ = FrameHeader::decode_prefix(input);
    Ok(())
}

/// A stream of frames read one after another, as the session does: every step consumes at least one byte,
/// and the loop ends on the first error or at the end of the buffer.
fn check_frame_stream(input: &[u8]) -> Result<(), TestCaseError> {
    let mut rest = input;
    while !rest.is_empty() {
        calls(1);
        match Message::decode_frame(rest) {
            Ok((_, used)) => {
                prop_assert!(used > 0 && used <= rest.len());
                rest = &rest[used..];
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// Input records one after another from arbitrary bytes.
fn check_input_stream(input: &[u8]) -> Result<(), TestCaseError> {
    let mut rest = input;
    while !rest.is_empty() {
        calls(1);
        match InputEvent::decode_prefix(rest) {
            Ok((e, used)) => {
                prop_assert!(used > 0 && used <= rest.len());
                prop_assert_eq!(e.encode().map_err(|e| fail(format!("{e}")))?, &rest[..used]);
                rest = &rest[used..];
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// The primitives of `Reader` on arbitrary bytes with arbitrary limits.
fn check_reader(input: &[u8], max: usize, min_item: usize) -> Result<(), TestCaseError> {
    calls(6);
    let mut r = Reader::new(input);
    if let Ok(v) = r.varint() {
        prop_assert!(v <= MAX_VARINT);
        prop_assert_eq!(varint(v), &input[..r.position()], "a varint is read in its minimal form only");
    }
    let mut r = Reader::new(input);
    if let Ok(s) = r.str(max) {
        prop_assert!(s.len() <= max);
        prop_assert!(!s.contains('\0'));
    }
    let mut r = Reader::new(input);
    if let Ok(n) = r.count(max, min_item) {
        prop_assert!(n <= max);
        prop_assert!(n.checked_mul(min_item).is_some_and(|need| need <= input.len() - r.position()));
    }
    let mut r = Reader::new(input);
    let _ = (r.u8(), r.u16(), r.u32(), r.u64(), r.i16(), r.i32(), r.bool());
    let mut r = Reader::new(input);
    let _ = r.take(max);
    prop_assert!(r.position() <= input.len());
    let mut r = Reader::new(input);
    let _ = r.array::<16>();
    Ok(())
}

// ---------- targets ----------

fn any_message() -> impl Strategy<Value = Message> {
    Union::new(message_strategies())
}

fn any_id() -> impl Strategy<Value = u16> {
    sel(Message::ALL_TYPE_IDS)
}

/// A frame of message `id` with `body`, the declared length correct.
fn frame_of(id: u16, body: &[u8]) -> Vec<u8> {
    let mut f = varint(u64::from(id));
    f.extend(varint(body.len() as u64));
    f.extend_from_slice(body);
    f
}

fn hostile_frame() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // header and soup, length right
        4 => (any_id(), soup()).prop_map(|(id, body)| frame_of(id, &body)),
        // length lies: a few bytes more or less than the body, or huge
        2 => (any_id(), soup(), prop_oneof![Just(1i64), Just(-1), Just(2), Just(65_535), Just(65_536), Just(1 << 40)])
            .prop_map(|(id, body, delta)| {
                let len = (body.len() as i64).saturating_add(delta).clamp(0, MAX_VARINT as i64) as u64;
                let mut f = varint(u64::from(id));
                f.extend(varint(len));
                f.extend(body);
                f
            }),
        // a valid frame, then more bytes
        2 => (any_message(), vec(any::<u8>(), 0..16)).prop_map(|(m, tail)| {
            let mut f = m.encode_frame().unwrap();
            f.extend(tail);
            f
        }),
        // anything
        2 => vec(any::<u8>(), 0..96),
    ]
}

/// One edit of a byte string: flip bits, overwrite, insert, delete, duplicate a run or cut the end.
#[derive(Debug, Clone)]
enum Edit {
    Flip(usize, u8),
    Set(usize, u8),
    Insert(usize, Vec<u8>),
    Delete(usize, usize),
    Repeat(usize, usize),
    Cut(usize),
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
    vec(
        prop_oneof![
            3 => (any::<usize>(), 1u8..=255).prop_map(|(i, m)| Edit::Flip(i, m)),
            3 => (any::<usize>(), any::<u8>()).prop_map(|(i, b)| Edit::Set(i, b)),
            2 => (any::<usize>(), token()).prop_map(|(i, t)| Edit::Insert(i, t)),
            1 => (any::<usize>(), 1usize..8).prop_map(|(i, n)| Edit::Delete(i, n)),
            1 => (any::<usize>(), 1usize..16).prop_map(|(i, n)| Edit::Repeat(i, n)),
            1 => any::<usize>().prop_map(Edit::Cut),
        ],
        1..4,
    )
}

fn apply(mut b: Vec<u8>, edits: &[Edit]) -> Vec<u8> {
    for e in edits {
        let n = b.len();
        match e {
            Edit::Flip(i, m) if n > 0 => b[i % n] ^= m,
            Edit::Set(i, v) if n > 0 => b[i % n] = *v,
            Edit::Insert(i, t) => {
                let at = i % (n + 1);
                b.splice(at..at, t.iter().copied());
            }
            Edit::Delete(i, k) if n > 0 => {
                let at = i % n;
                b.drain(at..(at + k).min(n));
            }
            Edit::Repeat(i, k) if n > 0 => {
                let at = i % n;
                let run: Vec<u8> = b[at..(at + k).min(n)].to_vec();
                b.splice(at..at, run);
            }
            Edit::Cut(i) if n > 0 => b.truncate(i % n),
            _ => {}
        }
    }
    b
}

targets! {
    message_mutations(600, 3_000_000) => |name, cases| {
        run_target(name, cases, (any_message(), edits()), |(m, edits)| {
            let payload = m.encode_payload().map_err(|e| fail(format!("{e}")))?;
            check_payload(m.type_id(), &apply(payload, &edits))?;
            let frame = m.encode_frame().map_err(|e| fail(format!("{e}")))?;
            check_frame_bytes(&apply(frame, &edits))
        })
    };
    message_payload_soup(1_500, 3_000_000) => |name, cases| {
        run_target(name, cases, (any_id(), soup()), |(id, b)| check_payload(id, &b))
    };
    message_payload_random(1_500, 5_000_000) => |name, cases| {
        run_target(name, cases, (any_id(), vec(any::<u8>(), 0..64)), |(id, b)| check_payload(id, &b))
    };
    message_values(120, 400_000) => |name, cases| {
        run_target(name, cases, (any_message(), vec(any::<u8>(), 0..16)), |(m, suffix)| {
            calls(1);
            check_message(&m)?;
            // a frame followed by arbitrary bytes still decodes alone from the front
            let mut f = m.encode_frame().map_err(|e| fail(format!("{e}")))?;
            let len = f.len();
            f.extend_from_slice(&suffix);
            let (back, used) = Message::decode_frame(&f).map_err(|e| fail(format!("{e}")))?;
            prop_assert_eq!(back, m);
            prop_assert_eq!(used, len);
            Ok(())
        })
    };
    frame_decoder(1_500, 5_000_000) => |name, cases| {
        run_target(name, cases, hostile_frame(), |b| check_frame_bytes(&b))
    };
    frame_stream(500, 2_000_000) => |name, cases| {
        run_target(name, cases, vec(hostile_frame(), 1..6), |frames| check_frame_stream(&frames.concat()))
    };
    reader_primitives(2_000, 5_000_000) => |name, cases| {
        run_target(name, cases, (bytes(), 0usize..300, 0usize..9), |(b, max, min)| check_reader(&b, max, min))
    };
    varints(2_000, 5_000_000) => |name, cases| {
        run_target(name, cases, any::<u64>(), |v| {
            calls(1);
            let v = v & MAX_VARINT;
            let enc = varint(v);
            let mut r = Reader::new(&enc);
            prop_assert_eq!(r.varint().map_err(|e| fail(format!("{e}")))?, v);
            prop_assert_eq!(r.remaining(), 0);
            // every longer form of the same value is refused
            for len in [2usize, 4, 8] {
                if enc.len() < len && (len == 8 || v < 1 << (6 * len / 2 + 2 * (len / 2))) {
                    let mut p = v.to_be_bytes()[8 - len..].to_vec();
                    p[0] |= match len { 2 => 0x40, 4 => 0x80, _ => 0xC0 };
                    prop_assert_eq!(Reader::new(&p).varint().err(), Some(CodecError::NonMinimalVarint));
                }
            }
            Ok(())
        })
    };
    input_event_stream(1_000, 3_000_000) => |name, cases| {
        run_target(name, cases, bytes(), |b| check_input_stream(&b))
    };
    sas(500, 2_000_000) => |name, cases| {
        run_target(name, cases, (any::<[u8; 32]>(), vec(any::<u8>(), 0..64), vec(any::<u8>(), 0..64), any::<[u8; 32]>(), any::<[u8; 32]>()),
            |(digest, spki_i, spki_r, ni, nr)| {
                calls(1);
                let sas = Sas::from_digest(&digest);
                prop_assert!(u64::from(sas.value()) < Sas::MODULUS);
                let shown = sas.to_string();
                prop_assert_eq!(shown.len(), 7);
                prop_assert_eq!(shown.as_bytes()[3], b' ');
                let pre = CommitmentInput { spki_i: &spki_i, nonce_i: &ni }.preimage();
                prop_assert_eq!(pre.len(), COMMIT_LABEL.len() + spki_i.len() + NONCE_LEN);
                let ek = digest;
                let pre = SasInput { ek: &ek, spki_i: &spki_i, spki_r: &spki_r, nonce_i: &ni, nonce_r: &nr }.preimage();
                prop_assert_eq!(pre.len(), SAS_LABEL.len() + EXPORTER_LEN + spki_i.len() + spki_r.len() + 2 * NONCE_LEN);
                Ok(())
            })
    };
}

/// For each wire type: `<name>_bytes` (arbitrary bytes and soup) and `<name>_values` (valid values with
/// arbitrary bytes after them).
macro_rules! wire_targets {
    ($( $name:ident : $ty:ty = $strategy:expr ; )+) => { $(
        mod $name {
            use super::*;

            fn bytes_run(name: &str, cases: u32) {
                run_target(name, cases, bytes(), |b| check_bytes::<$ty>(&b))
            }

            fn values_run(name: &str, cases: u32) {
                run_target(name, cases, ($strategy, vec(any::<u8>(), 0..16)), |(x, suffix)| check_value::<$ty>(&x, &suffix))
            }

            #[test]
            fn bytes_fuzz() {
                bytes_run(concat!(stringify!($name), "_bytes"), 1_000);
            }

            #[test]
            #[ignore = "long: fuzz"]
            fn bytes_long() {
                bytes_run(concat!(stringify!($name), "_bytes"), long_cases(3_000_000));
            }

            #[test]
            fn values_fuzz() {
                values_run(concat!(stringify!($name), "_values"), 150);
            }

            #[test]
            #[ignore = "long: fuzz"]
            fn values_long() {
                values_run(concat!(stringify!($name), "_values"), long_cases(1_000_000));
            }
        }
    )+ };
}

wire_targets! {
    video_header_t: VideoHeader = video_header();
    audio_header_t: AudioHeader = audio_header();
    cursor_position_t: CursorPosition = cursor_position();
    cursor_shape_header_t: CursorShapeHeader = cursor_shape_header();
    cursor_shape_t: CursorShape = cursor_shape();
    clipboard_header_t: ClipboardHeader = clipboard_header();
    clipboard_item_t: ClipboardItem = clipboard_item();
    input_event_t: InputEvent = input_event();
    file_data_header_t: FileDataHeader =
        (any::<u32>(), any::<u64>()).prop_map(|(transfer_id, offset)| FileDataHeader { transfer_id, offset });
    terminal_stream_header_t: TerminalStreamHeader = any::<u32>().prop_map(|term_id| TerminalStreamHeader { term_id });
    stream_kind_t: StreamKind = sel(StreamKind::ALL);
    datagram_kind_t: DatagramKind = sel(DatagramKind::ALL);
    frame_header_t: FrameHeader = (any_id(), 0..=MAX_FRAME_PAYLOAD).prop_map(|(type_id, len)| FrameHeader { type_id, len });
    permission_set_t: PermissionSet = perms();
    display_t: Display = display();
    dir_entry_t: DirEntry = dir_entry();
    video_cap_t: VideoCap = video_cap();
}
