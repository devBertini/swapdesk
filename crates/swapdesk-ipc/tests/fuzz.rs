//! Fuzzing of the local IPC decoders (stage 29), with proptest: `decode_message`, `FrameReader`, `Handshake`,
//! `ReplyBody::decode` and every body inside them (ops, notifications, clipboard, terminal, files, video and audio).
//!
//! Each target is one property in two tests: `<target>::fuzz` runs a few hundred cases inside the normal suite,
//! `<target>::long` (`#[ignore = "long: fuzz"]`) runs millions in a release build:
//!
//! ```text
//! cargo test -p swapdesk-ipc --release --test fuzz -- --ignored --nocapture
//! ```
//!
//! `SWAPDESK_FUZZ_CASES=<n>` sets the case count of every `long` test. The properties are those of
//! `swapdesk-proto/tests/fuzz.rs`: arbitrary bytes, byte soup, framed soup with lying lengths and mutated valid
//! frames never panic and never reserve from an unchecked length (`forbid(unsafe_code)` rules out a counting
//! allocator, so the soup declares lengths up to the type limit, 16 MiB, over a few bytes and the stream reader
//! must stop at the end of the data); what decodes re-encodes to the same bytes; every truncation of a valid frame
//! is refused; arbitrary valid values round trip, also through a `FrameReader` fed in small pieces.

mod common;

use std::cell::Cell;
use std::io::{self, Cursor, Read};
use std::time::Instant;

use common::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use swapdesk_ipc::*;
use swapdesk_proto::Writer;
use swapdesk_proto::codec::MAX_VARINT;

// ---------- harness ----------

type Check = core::result::Result<(), TestCaseError>;

thread_local! {
    /// Decoder calls made by the target running on this thread.
    static CALLS: Cell<u64> = const { Cell::new(0) };
    /// Inputs of that target that decoded (the depth the byte soup reached).
    static ACCEPTED: Cell<u64> = const { Cell::new(0) };
}

fn calls(n: u64) {
    CALLS.with(|c| c.set(c.get() + n));
}

fn accepted() {
    ACCEPTED.with(|c| c.set(c.get() + 1));
}

fn long_cases(default: u32) -> u32 {
    std::env::var("SWAPDESK_FUZZ_CASES").ok().and_then(|v| v.replace('_', "").parse().ok()).unwrap_or(default)
}

fn run_target<S: Strategy>(name: &str, cases: u32, strategy: S, check: impl Fn(S::Value) -> Check) {
    let start = Instant::now();
    CALLS.with(|c| c.set(0));
    ACCEPTED.with(|c| c.set(0));
    let mut runner = TestRunner::new(Config { cases, failure_persistence: None, ..Config::default() });
    runner.run(&strategy, check).unwrap_or_else(|e| panic!("{name}: {e}"));
    println!(
        "FUZZ ipc {name}: {cases} cases, {} decoder calls, {} inputs accepted, {:.1}s",
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

fn varint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_varint(v).unwrap();
    w.into_bytes()
}

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

fn soup() -> impl Strategy<Value = Vec<u8>> {
    vec(token(), 0..24).prop_map(|parts| parts.concat())
}

/// Every message type number this version knows.
fn known_types() -> Vec<u64> {
    (0u64..0x100).filter(|t| limit_of(*t).is_some()).collect()
}

fn any_type() -> impl Strategy<Value = u64> {
    proptest::sample::select(known_types())
}

fn frame_of(ty: u64, body: &[u8]) -> Vec<u8> {
    let mut f = varint(ty);
    f.extend(varint(body.len() as u64));
    f.extend_from_slice(body);
    f
}

fn hostile_frame() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // header and soup, length right
        4 => (any_type(), soup()).prop_map(|(ty, body)| frame_of(ty, &body)),
        // length lies: a few bytes more or less, at a limit, or the largest the type allows
        2 => (any_type(), soup(), prop_oneof![Just(1i64), Just(-1), Just(2), Just(1 << 16), Just(1 << 20), Just(1 << 24), Just(1 << 40)])
            .prop_map(|(ty, body, delta)| {
                let len = (body.len() as i64).saturating_add(delta).clamp(0, MAX_VARINT as i64) as u64;
                let mut f = varint(ty);
                f.extend(varint(len));
                f.extend(body);
                f
            }),
        // a valid frame, then more bytes
        2 => (any_message(), vec(any::<u8>(), 0..16)).prop_map(|(m, tail)| {
            let mut f = encode_message(&m).unwrap();
            f.extend(tail);
            f
        }),
        // anything
        2 => vec(any::<u8>(), 0..96),
    ]
}

/// One edit of a byte string.
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

fn fail(msg: String) -> TestCaseError {
    TestCaseError::fail(msg)
}

/// A reader that hands out a few bytes per call, sometimes an `Interrupted`, to prove the framing does not depend
/// on how the pipe cuts the stream.
struct Chunky {
    inner: Cursor<Vec<u8>>,
    sizes: Vec<u8>,
    at: usize,
}

impl Read for Chunky {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let step = self.sizes[self.at % self.sizes.len()];
        self.at += 1;
        if step == 0 {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let n = usize::from(step).min(buf.len());
        self.inner.read(&mut buf[..n])
    }
}

// ---------- checks ----------

/// What a `FrameReader` makes of `bytes`: every message up to the first error or the clean end.
fn read_all(bytes: &[u8]) -> (Vec<Message>, Option<IpcError>) {
    let mut reader = FrameReader::new(Cursor::new(bytes.to_vec()));
    let mut out = Vec::new();
    loop {
        calls(1);
        match reader.read_message() {
            Ok(Some(m)) => out.push(m),
            Ok(None) => return (out, None),
            Err(e) => return (out, Some(e)),
        }
        assert!(out.len() <= bytes.len(), "a frame took no bytes");
    }
}

/// Arbitrary bytes as one frame and as a stream.
fn check_frame(bytes: &[u8]) -> Check {
    calls(1);
    let one = decode_message(bytes);
    let (stream, err) = read_all(bytes);
    if let Ok(m) = &one {
        accepted();
        let again = encode_message(m).map_err(|e| fail(format!("{e}")))?;
        prop_assert_eq!(&again[..], bytes, "an accepted frame is canonical");
        // the stream reader sees the same single message and a clean end
        prop_assert_eq!(&stream, &vec![m.clone()]);
        prop_assert!(err.is_none());
        for cut in 0..bytes.len() {
            calls(1);
            prop_assert!(decode_message(&bytes[..cut]).is_err(), "frame prefix {} decoded", cut);
        }
    } else {
        // whatever the stream reader did accept re-encodes to a prefix of the input
        let mut used = 0;
        for m in &stream {
            used += encode_message(m).map_err(|e| fail(format!("{e}")))?.len();
        }
        prop_assert!(used <= bytes.len());
    }
    Ok(())
}

/// A valid message: round trip through both readers, trailing bytes, every truncation, mutations and chunked reads.
fn check_message_damage(m: &Message, sizes: &[u8]) -> Check {
    let frame = encode_message(m).map_err(|e| fail(format!("{e}")))?;
    calls(2);
    prop_assert_eq!(&decode_message(&frame).map_err(|e| fail(format!("{e}")))?, m);
    let (stream, err) = read_all(&frame);
    prop_assert_eq!(stream, vec![m.clone()]);
    prop_assert!(err.is_none());

    // trailing bytes: refused as one frame, left for the next as a stream
    let mut longer = frame.clone();
    longer.push(0);
    calls(1);
    prop_assert!(decode_message(&longer).is_err());

    // every truncation: refused; the stream reader says "ended" only at offset 0
    for cut in 0..frame.len() {
        calls(2);
        prop_assert!(decode_message(&frame[..cut]).is_err(), "prefix {} decoded", cut);
        let mut reader = FrameReader::new(&frame[..cut]);
        match reader.read_message() {
            Ok(None) => prop_assert_eq!(cut, 0),
            Ok(Some(_)) => return Err(fail(format!("a stream cut at {cut} gave a message"))),
            Err(_) => {}
        }
    }

    // single-byte mutations: refused, or equal to what they re-encode to
    for i in 0..frame.len().min(96) {
        for flip in [0x01u8, 0x40, 0x80, 0xFF] {
            let mut mutated = frame.clone();
            mutated[i] ^= flip;
            calls(1);
            if let Ok(y) = decode_message(&mutated) {
                prop_assert_eq!(encode_message(&y).map_err(|e| fail(format!("{e}")))?, mutated);
            }
        }
    }

    // two messages back to back, read in small pieces, interrupted now and then
    let mut two = frame.clone();
    two.extend_from_slice(&frame);
    let mut reader = FrameReader::new(Chunky { inner: Cursor::new(two), sizes: sizes.to_vec(), at: 0 });
    for _ in 0..2 {
        calls(1);
        prop_assert_eq!(reader.read_message().map_err(|e| fail(format!("{e}")))?, Some(m.clone()));
    }
    let end = reader.read_message().map_err(|e| fail(format!("{e}")))?;
    prop_assert!(end.is_none());
    Ok(())
}

/// A reply body by op: arbitrary bytes never panic, accepted ones are canonical and prefix-free.
fn check_reply_bytes(op: u16, body: &[u8]) -> Check {
    calls(1);
    if let Ok(v) = ReplyBody::decode(op, body) {
        accepted();
        prop_assert_eq!(v.encode().map_err(|e| fail(format!("{e}")))?, body);
        for cut in 0..body.len() {
            calls(1);
            prop_assert!(ReplyBody::decode(op, &body[..cut]).is_err(), "reply prefix {} decoded", cut);
        }
    }
    Ok(())
}

// ---------- targets ----------

targets! {
    frame_soup(1_500, 5_000_000) => |name, cases| {
        run_target(name, cases, hostile_frame(), |b| check_frame(&b))
    };
    frame_random(1_500, 5_000_000) => |name, cases| {
        run_target(name, cases, vec(any::<u8>(), 0..64), |b| check_frame(&b))
    };
    frame_mutations(600, 2_000_000) => |name, cases| {
        run_target(name, cases, (any_message(), edits()), |(m, edits)| {
            check_frame(&apply(encode_message(&m).map_err(|e| fail(format!("{e}")))?, &edits))
        })
    };
    message_values(150, 300_000) => |name, cases| {
        run_target(name, cases, (any_message(), vec(0u8..=5, 0..8).prop_map(|mut v| { v.push(1); v })), |(m, sizes)| check_message_damage(&m, &sizes))
    };
    reply_bytes(2_000, 5_000_000) => |name, cases| {
        run_target(name, cases, (0u16..60, prop_oneof![3 => soup(), 1 => vec(any::<u8>(), 0..48)]), |(op, b)| {
            check_reply_bytes(op, &b)
        })
    };
    reply_values(300, 1_500_000) => |name, cases| {
        run_target(name, cases, reply_body(), |(op, body)| {
            calls(1);
            let bytes = body.encode().map_err(|e| fail(format!("{e}")))?;
            prop_assert_eq!(&ReplyBody::decode(op, &bytes).map_err(|e| fail(format!("{e}")))?, &body);
            check_reply_bytes(op, &bytes)?;
            let mut longer = bytes.clone();
            longer.push(0);
            prop_assert!(ReplyBody::decode(op, &longer).is_err());
            Ok(())
        })
    };
    statuses(500, 2_000_000) => |name, cases| {
        run_target(name, cases, any::<u16>(), |s| {
            calls(1);
            match status_code(s) {
                Ok(None) => prop_assert_eq!(s, STATUS_OK),
                Ok(Some(code)) => prop_assert_eq!(code.status(), s),
                Err(_) => {}
            }
            Ok(())
        })
    };
    handshake_server(1_000, 3_000_000) => |name, cases| {
        run_target(name, cases, prop_oneof![2 => hostile_frame(), 1 => (any::<u32>(), any::<u16>(), any::<u32>()).prop_map(|(role, version, pid)| {
            let mut body = Writer::new();
            body.put_u8((role % 5) as u8);
            body.put_u16(version % 3);
            body.put_u32(pid);
            frame_of(0x01, &body.into_bytes())
        })], |bytes| {
            calls(1);
            let mut reader = FrameReader::new(Cursor::new(bytes));
            let mut out = Vec::new();
            let mut writer = FrameWriter::new(&mut out);
            if Handshake::server(&mut reader, &mut writer, 7).is_ok() {
                accepted();
                // the answer is a whole frame the client side accepts
                let mut c = FrameReader::new(Cursor::new(out.clone()));
                let ack = matches!(c.read_message(), Ok(Some(Message::HelloAck { .. })));
                prop_assert!(ack);
            }
            Ok(())
        })
    };
    handshake_client(1_000, 3_000_000) => |name, cases| {
        run_target(name, cases, hostile_frame(), |bytes| {
            calls(1);
            let mut reader = FrameReader::new(Cursor::new(bytes));
            let mut sent = Vec::new();
            let mut writer = FrameWriter::new(&mut sent);
            if Handshake::client(&mut reader, &mut writer, IpcRole::Ui, 1).is_ok() {
                accepted();
            }
            prop_assert!(!sent.is_empty(), "the client always says Hello first");
            Ok(())
        })
    };
}

/// The value strategies reach every message type number the decoder knows.
#[test]
fn every_type_has_a_strategy() {
    use proptest::strategy::ValueTree;
    let strategy = Union::new(vec![message(), Union::new(extra_messages()).boxed()]);
    let mut runner = TestRunner::deterministic();
    let seen: std::collections::BTreeSet<u64> =
        (0..4_000).map(|_| strategy.new_tree(&mut runner).unwrap().current().type_code()).collect();
    let missing: Vec<u64> = known_types().into_iter().filter(|t| !seen.contains(t)).collect();
    assert!(missing.is_empty(), "no value strategy for message types {missing:x?}");
}
