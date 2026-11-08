//! The terminal messages of C2.4 "Terminal": numbers, round trips, limits checked before allocation, and a `Debug`
//! that never prints a terminal byte or a session user.

use std::io::{self, Cursor, Read};

use swapdesk_ipc::*;
use swapdesk_proto::{ErrorCode, Writer};

fn varint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_varint(v).unwrap();
    w.into_bytes()
}

/// A reader that counts the bytes taken from it.
struct Counting<R> {
    inner: R,
    taken: usize,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.taken += n;
        Ok(n)
    }
}

fn all_messages() -> Vec<(u64, Message)> {
    let bytes = |b: &[u8]| Bytes(b.to_vec());
    vec![
        (0x70, Message::PtyOpen { id: 7, cols: 120, rows: 30, session_user: Text("S-1-5-21-1-2-3-1001".into()) }),
        (0x70, Message::PtyOpen { id: 8, cols: 80, rows: 24, session_user: Text(String::new()) }),
        (0x71, Message::PtyOpenReply { id: 7, code: ErrorCode::NoError, account: Text("claudio".into()) }),
        (0x71, Message::PtyOpenReply { id: 7, code: ErrorCode::NotPermitted, account: Text(String::new()) }),
        (0x72, Message::PtyInput { id: 7, bytes: bytes(b"dir\r") }),
        (0x72, Message::PtyInput { id: 7, bytes: bytes(b"") }),
        (0x73, Message::PtyOutput { id: 7, bytes: bytes(b"\x1b[31mred\x1b[0m") }),
        (0x74, Message::PtyResize { id: 7, cols: 200, rows: 50 }),
        (0x75, Message::PtyClose { id: 7 }),
        (0x76, Message::PtyExited { id: 7, code: Some(3) }),
        (0x76, Message::PtyExited { id: 7, code: None }),
        (0x78, Message::TermOutput { term_id: 9, bytes: bytes(b"hello") }),
        (0x79, Message::TermInput { term_id: 9, bytes: bytes(b"\x03") }),
    ]
}

#[test]
fn every_terminal_message_has_its_number_and_round_trips() {
    for (ty, msg) in all_messages() {
        assert_eq!(msg.type_code(), ty, "{msg:?}");
        let wire = encode_message(&msg).unwrap();
        assert_eq!(decode_message(&wire).unwrap(), msg);
        let mut reader = FrameReader::new(Cursor::new(wire));
        assert_eq!(reader.read_message().unwrap().unwrap(), msg);
        assert!(reader.read_message().unwrap().is_none());
        assert!(limit_of(ty).is_some());
    }
}

#[test]
fn the_ui_ops_the_reply_and_the_notification_carry_their_numbers() {
    let ops = [
        (UiOp::TermOpen { session_id: 4, cols: 80, rows: 24, admin: true }, 32),
        (UiOp::TermResize { term_id: 5, cols: 100, rows: 40 }, 33),
        (UiOp::TermClose { term_id: 5 }, 34),
    ];
    for (op, code) in ops {
        assert_eq!(op.code(), code);
        let msg = Message::UiRequest { req: 1, op };
        assert_eq!(decode_message(&encode_message(&msg).unwrap()).unwrap(), msg);
    }
    let opened = ReplyBody::TermOpened { term_id: 5, outcome: TermOutcome::Opened, account: Text("claudio".into()) };
    assert_eq!(ReplyBody::decode(32, &opened.encode().unwrap()).unwrap(), opened);
    for outcome in TermOutcome::ALL {
        let refused = ReplyBody::TermOpened { term_id: 0, outcome: *outcome, account: Text(String::new()) };
        assert_eq!(ReplyBody::decode(32, &refused.encode().unwrap()).unwrap(), refused);
    }
    assert_eq!(ReplyBody::decode(33, &[]).unwrap(), ReplyBody::Done);
    assert_eq!(ReplyBody::decode(34, &[]).unwrap(), ReplyBody::Done);
    for code in [Some(0), Some(-1), None] {
        let n = Notify::TermExited { term_id: 5, exit_code: code };
        assert_eq!(n.kind(), 16);
        let msg = Message::UiNotify(n);
        assert_eq!(decode_message(&encode_message(&msg).unwrap()).unwrap(), msg);
    }
}

#[test]
fn an_error_code_maps_onto_an_outcome() {
    assert_eq!(TermOutcome::of_error(ErrorCode::NoError), TermOutcome::Opened);
    assert_eq!(TermOutcome::of_error(ErrorCode::NotPermitted), TermOutcome::NotPermitted);
    assert_eq!(TermOutcome::of_error(ErrorCode::NobodyPresent), TermOutcome::NobodyPresent);
    assert_eq!(TermOutcome::of_error(ErrorCode::Declined), TermOutcome::Declined);
    assert_eq!(TermOutcome::of_error(ErrorCode::Timeout), TermOutcome::TimedOut);
    assert_eq!(TermOutcome::of_error(ErrorCode::Busy), TermOutcome::TooMany);
    assert_eq!(TermOutcome::of_error(ErrorCode::Unavailable), TermOutcome::Unavailable);
    assert_eq!(TermOutcome::of_error(ErrorCode::IoFailed), TermOutcome::Failed);
}

#[test]
fn debug_prints_lengths_and_never_a_terminal_byte_or_the_session_user() {
    let secret = "hunter2-password";
    let user = "S-1-5-21-1111-2222-3333-1001";
    let shown = [
        Message::PtyInput { id: 1, bytes: Bytes(secret.as_bytes().to_vec()) },
        Message::PtyOutput { id: 1, bytes: Bytes(secret.as_bytes().to_vec()) },
        Message::TermInput { term_id: 1, bytes: Bytes(secret.as_bytes().to_vec()) },
        Message::TermOutput { term_id: 1, bytes: Bytes(secret.as_bytes().to_vec()) },
        Message::PtyOpen { id: 1, cols: 80, rows: 24, session_user: Text(user.into()) },
        Message::PtyOpenReply { id: 1, code: ErrorCode::NoError, account: Text("claudio".into()) },
        Message::UiReply { req: 1, status: 0, body: Bytes(secret.as_bytes().to_vec()) },
    ];
    for m in shown {
        let text = format!("{m:?}");
        assert!(
            !text.contains(secret) && !text.contains("hunter") && !text.contains(user) && !text.contains("claudio"),
            "{text}"
        );
        assert!(text.contains("len"), "a length is shown: {text}");
    }
    let reply = ReplyBody::TermOpened { term_id: 1, outcome: TermOutcome::Opened, account: Text("claudio".into()) };
    assert!(!format!("{reply:?}").contains("claudio"));
}

#[test]
fn a_chunk_over_the_limit_is_refused_when_encoding_and_when_decoding() {
    let big = Bytes(vec![0x41; MAX_TERM_CHUNK + 1]);
    for msg in [
        Message::PtyInput { id: 1, bytes: big.clone() },
        Message::PtyOutput { id: 1, bytes: big.clone() },
        Message::TermInput { term_id: 1, bytes: big.clone() },
        Message::TermOutput { term_id: 1, bytes: big },
    ] {
        assert!(encode_message(&msg).is_err(), "{msg:?}");
    }
    // The largest chunk goes through.
    let full = Message::PtyOutput { id: 1, bytes: Bytes(vec![7; MAX_TERM_CHUNK]) };
    assert_eq!(decode_message(&encode_message(&full).unwrap()).unwrap(), full);
    // A session user over its maximum is refused too.
    let long = Message::PtyOpen { id: 1, cols: 1, rows: 1, session_user: Text("S".repeat(MAX_SESSION_USER + 1)) };
    assert!(encode_message(&long).is_err());
}

#[test]
fn a_declared_length_over_the_limit_is_refused_before_one_body_byte_is_read() {
    for (ty, limit) in [
        (0x72u64, 4 + MAX_TERM_CHUNK),
        (0x73, 4 + MAX_TERM_CHUNK),
        (0x78, 4 + MAX_TERM_CHUNK),
        (0x79, 4 + MAX_TERM_CHUNK),
        (0x70, 512),
        (0x71, 512),
        (0x76, 64),
    ] {
        let mut stream = varint(ty);
        stream.extend(varint(4 * 1024 * 1024 * 1024));
        let header_len = stream.len();
        stream.extend_from_slice(&[7u8; 10]);
        let mut reader = FrameReader::new(Counting { inner: Cursor::new(stream), taken: 0 });
        match reader.read_message() {
            Err(IpcError::TooLarge { ty: t, len, limit: l }) => assert_eq!((t, len, l), (ty, 4 << 30, limit as u64)),
            other => panic!("type {ty:#x}: expected TooLarge, got {other:?}"),
        }
        assert_eq!(reader.into_inner().taken, header_len, "type {ty:#x}: only the header was read");
    }
    // One byte over the chunk limit, declared exactly: refused with the limit named.
    let mut stream = varint(0x73);
    stream.extend(varint((4 + MAX_TERM_CHUNK + 1) as u64));
    assert!(matches!(FrameReader::new(Cursor::new(stream)).read_message(), Err(IpcError::TooLarge { ty: 0x73, .. })));
}

#[test]
fn a_truncated_or_padded_terminal_body_is_a_protocol_error() {
    // PtyResize needs 8 bytes.
    let mut short = varint(0x74);
    short.extend(varint(3));
    short.extend_from_slice(&[1, 2, 3]);
    assert!(decode_message(&short).is_err());
    // PtyClose with a trailing byte.
    let mut padded = varint(0x75);
    padded.extend(varint(5));
    padded.extend_from_slice(&[1, 0, 0, 0, 9]);
    assert!(decode_message(&padded).is_err());
    // An unknown kind between the two ranges is still an unknown type.
    for ty in [0x77u64, 0x7A] {
        let mut frame = varint(ty);
        frame.extend(varint(0));
        assert!(matches!(decode_message(&frame), Err(IpcError::UnknownType(t)) if t == ty), "type {ty:#x}");
    }
}
