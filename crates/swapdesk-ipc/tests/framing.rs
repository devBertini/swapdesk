//! Rejections, the handshake, the redacting `Debug` and a reader/writer pair over an in-memory pipe.

use std::io::{self, Cursor, Read, Write};
use std::sync::mpsc;
use std::thread;

use swapdesk_ipc::*;
use swapdesk_proto::{CodecError, ErrorCode, InputEvent, Writer};

fn varint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_varint(v).unwrap();
    w.into_bytes()
}

fn frame(ty: u64, body: &[u8]) -> Vec<u8> {
    let mut out = varint(ty);
    out.extend(varint(body.len() as u64));
    out.extend_from_slice(body);
    out
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

fn hello() -> Message {
    Message::Hello { role: IpcRole::Ui, version: IPC_VERSION, pid: 4242 }
}

#[test]
fn a_4_gib_claim_is_refused_before_any_body_is_read() {
    // Type 0x41 (UiReply, limit 1 MiB) claiming 4 GiB, then only 10 bytes.
    let mut stream = varint(0x41);
    stream.extend(varint(4 * 1024 * 1024 * 1024));
    let header_len = stream.len();
    stream.extend_from_slice(&[7u8; 10]);
    let mut reader = FrameReader::new(Counting { inner: Cursor::new(stream), taken: 0 });
    match reader.read_message() {
        Err(IpcError::TooLarge { ty, len, limit }) => {
            assert_eq!((ty, len, limit), (0x41, 4 << 30, 1 << 20));
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    // Only the two varints were consumed: no body byte was read, so nothing was reserved for it.
    assert_eq!(reader.into_inner().taken, header_len);
}

#[test]
fn per_type_limits_hold() {
    assert_eq!(limit_of(0x31), Some(16 << 20));
    assert_eq!(limit_of(0x50), Some(16 << 20));
    assert_eq!(limit_of(0x24), Some(64 << 10));
    assert_eq!(limit_of(0x54), Some(64 << 10));
    assert_eq!(limit_of(0x34), Some(1500));
    assert_eq!(limit_of(0x42), Some(1 << 20));
    assert_eq!(limit_of(0x99), None);
    // One byte over an input batch's limit is refused by the reader.
    let mut stream = varint(0x24);
    stream.extend(varint(64 * 1024 + 1));
    let mut reader = FrameReader::new(Cursor::new(stream));
    assert!(matches!(reader.read_message(), Err(IpcError::TooLarge { limit: 65536, .. })));
    // And the encoder refuses to produce one.
    let big = Message::Input { events: Events(vec![InputEvent::Text("a".repeat(200)); 400]) };
    assert!(matches!(encode_message(&big), Err(IpcError::TooLarge { ty: 0x24, .. })));
    let long = Message::AudioPacket(Bytes(vec![0; 1501]));
    assert!(encode_message(&long).is_err());
}

#[test]
fn an_unknown_type_is_a_protocol_error() {
    let bytes = frame(0x7F, &[]);
    assert!(matches!(decode_message(&bytes), Err(IpcError::UnknownType(0x7F))));
    let mut reader = FrameReader::new(Cursor::new(bytes));
    assert!(matches!(reader.read_message(), Err(IpcError::UnknownType(0x7F))));
}

#[test]
fn trailing_bytes_are_rejected() {
    // After the frame.
    let mut bytes = encode_message(&hello()).unwrap();
    bytes.push(0);
    assert!(matches!(decode_message(&bytes), Err(IpcError::Codec(CodecError::TrailingBytes))));
    // Inside the body (length covers one byte more than the fields).
    let mut body = Vec::new();
    body.extend_from_slice(&[1, 1, 0, 1, 0, 0, 0]);
    body.push(0xEE);
    assert!(matches!(decode_message(&frame(0x01, &body)), Err(IpcError::Codec(CodecError::TrailingBytes))));
    let mut reader = FrameReader::new(Cursor::new(frame(0x01, &body)));
    assert!(matches!(reader.read_message(), Err(IpcError::Codec(CodecError::TrailingBytes))));
}

#[test]
fn a_truncated_body_is_rejected() {
    let bytes = encode_message(&hello()).unwrap();
    let cut = &bytes[..bytes.len() - 1];
    assert!(matches!(decode_message(cut), Err(IpcError::Codec(CodecError::Truncated))));
    let mut reader = FrameReader::new(Cursor::new(cut.to_vec()));
    match reader.read_message() {
        Err(IpcError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected an EOF error, got {other:?}"),
    }
    // A body shorter than its fields: the length says 2, Hello needs 7.
    assert!(matches!(decode_message(&frame(0x01, &[1, 1])), Err(IpcError::Codec(CodecError::Truncated))));
}

#[test]
fn an_unknown_op_or_notify_kind_is_rejected() {
    let mut body = 1u32.to_le_bytes().to_vec();
    body.extend_from_slice(&99u16.to_le_bytes());
    assert!(decode_message(&frame(0x40, &body)).is_err());
    assert!(decode_message(&frame(0x42, &99u16.to_le_bytes())).is_err());
}

#[test]
fn a_video_header_must_match_the_payload_it_carries() {
    let msg = Message::VideoFrame {
        header: swapdesk_proto::VideoHeader {
            keyframe: true,
            config: false,
            discontinuity: false,
            display_id: 0,
            frame_id: 1,
            capture_us: 2,
            encode_us: 3,
            width: 4,
            height: 5,
            codec: swapdesk_proto::Codec::H264,
            payload_len: 3,
        },
        payload: Bytes(vec![1, 2]),
    };
    assert!(encode_message(&msg).is_err());
}

#[test]
fn the_server_wants_hello_first() {
    // The first message is a Bye: error, and nothing is answered.
    let first = encode_message(&Message::Bye { code: ErrorCode::NoError }).unwrap();
    let mut out = Vec::new();
    let result = Handshake::server(&mut FrameReader::new(Cursor::new(first)), &mut FrameWriter::new(&mut out), 3);
    assert!(matches!(result, Err(IpcError::NotHello { got: 0x03 })));
    assert!(out.is_empty());

    // A request first is just as wrong.
    let request = encode_message(&Message::UiRequest { req: 1, op: UiOp::GetNetwork }).unwrap();
    let result = Handshake::server(&mut FrameReader::new(Cursor::new(request)), &mut FrameWriter::new(&mut out), 3);
    assert!(matches!(result, Err(IpcError::NotHello { got: 0x40 })));

    // An empty stream.
    let result = Handshake::server(&mut FrameReader::new(Cursor::new(Vec::new())), &mut FrameWriter::new(&mut out), 3);
    assert!(matches!(result, Err(IpcError::Closed)));
    assert!(out.is_empty());
}

#[test]
fn hello_is_answered_with_hello_ack() {
    let first = encode_message(&hello()).unwrap();
    let mut out = Vec::new();
    let got = Handshake::server(&mut FrameReader::new(Cursor::new(first)), &mut FrameWriter::new(&mut out), 3).unwrap();
    assert_eq!(got, ClientHello { role: IpcRole::Ui, pid: 4242 });
    assert_eq!(decode_message(&out).unwrap(), Message::HelloAck { version: IPC_VERSION, win_session: 3 });

    // A Hello of another version gets Bye(UNSUPPORTED_VERSION).
    let old = encode_message(&Message::Hello { role: IpcRole::HelperUser, version: 9, pid: 1 }).unwrap();
    let mut out = Vec::new();
    let result = Handshake::server(&mut FrameReader::new(Cursor::new(old)), &mut FrameWriter::new(&mut out), 3);
    assert!(matches!(result, Err(IpcError::UnsupportedVersion(9))));
    assert_eq!(decode_message(&out).unwrap(), Message::Bye { code: ErrorCode::UnsupportedVersion });
}

#[test]
fn the_client_wants_hello_ack() {
    let reply = encode_message(&Message::Shutdown).unwrap();
    let mut sent = Vec::new();
    let result = Handshake::client(
        &mut FrameReader::new(Cursor::new(reply)),
        &mut FrameWriter::new(&mut sent),
        IpcRole::HelperSystem,
        7,
    );
    assert!(matches!(result, Err(IpcError::NotHelloAck { got: 0x29 })));
    assert_eq!(
        decode_message(&sent).unwrap(),
        Message::Hello { role: IpcRole::HelperSystem, version: IPC_VERSION, pid: 7 }
    );
}

#[test]
fn debug_prints_lengths_and_never_a_body() {
    let secret = "SECRETSAS123456";
    let mut shown = Vec::new();
    shown.push(format!(
        "{:?}",
        Message::Input {
            events: Events(vec![
                InputEvent::Text("hunter2-typed".into()),
                InputEvent::Key { usage_page: 7, usage: 0x1D, down: true, modifiers: 0, locks: 0 },
            ])
        }
    ));
    shown.push(format!("{:?}", Message::HelperFault { code: ErrorCode::Internal, what: Text("token-abc-999".into()) }));
    shown.push(format!(
        "{:?}",
        Message::UiNotify(Notify::Pairing {
            pairing_id: 1,
            state: PairingState::ShowCode { code: Text(secret.into()), peer_name: "Mac".into() }
        })
    ));
    shown.push(format!("{:?}", Message::AudioPacket(Bytes(vec![0xAB; 40]))));
    shown.push(format!(
        "{:?}",
        Message::UiNotify(Notify::ActivityAlert(AuditView {
            at_ms: 1,
            device_name: "pc".into(),
            what: Msg { key: 3, args: vec![Text("argument-with-a-secret".into())] },
            duration_s: None,
            alert: true
        }))
    ));
    shown.push(format!("{:?}", Message::UiReply { req: 1, status: 0, body: Bytes(b"BODYBYTES".to_vec()) }));
    let all = shown.join("\n");
    for needle in [
        "hunter2",
        "token-abc",
        secret,
        "argument-with",
        "BODYBYTES",
        "171", // 0xAB as decimal, as a Vec<u8> would print it
    ] {
        assert!(!all.contains(needle), "{needle} leaked in:\n{all}");
    }
    assert!(all.contains("Events(count 2)"), "{all}");
    assert!(all.contains("Text(len 15)"), "{all}");
    assert!(all.contains("Bytes(len 40)"), "{all}");
    assert!(all.contains("Bytes(len 9)"), "{all}");
}

/// The write half of an in-memory pipe.
struct PipeTx(mpsc::Sender<Vec<u8>>);

impl Write for PipeTx {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.send(buf.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The read half: blocks for the next chunk, ends when the sender is dropped.
struct PipeRx {
    rx: mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for PipeRx {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.chunk.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.chunk = next;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.pos);
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn pipe() -> (PipeTx, PipeRx) {
    let (tx, rx) = mpsc::channel();
    (PipeTx(tx), PipeRx { rx, chunk: Vec::new(), pos: 0 })
}

#[test]
fn client_and_service_talk_over_an_in_memory_pipe() {
    let (c2s_tx, c2s_rx) = pipe();
    let (s2c_tx, s2c_rx) = pipe();

    let service = thread::spawn(move || {
        let mut reader = FrameReader::new(c2s_rx);
        let mut writer = FrameWriter::new(s2c_tx);
        let hello = Handshake::server(&mut reader, &mut writer, 5).unwrap();
        assert_eq!(hello.role, IpcRole::Ui);
        let Some(Message::UiRequest { req, op }) = reader.read_message().unwrap() else {
            panic!("expected a request");
        };
        assert_eq!(op, UiOp::GetNetwork);
        let body = ReplyBody::PairingId(77).encode().unwrap();
        writer.write_message(&Message::UiReply { req, status: STATUS_OK, body: Bytes(body) }).unwrap();
        // Clean end: the client closes, the reader reports it as `None`.
        assert!(reader.read_message().unwrap().is_none());
    });

    let mut reader = FrameReader::new(s2c_rx);
    let mut writer = FrameWriter::new(c2s_tx);
    let ack = Handshake::client(&mut reader, &mut writer, IpcRole::Ui, 100).unwrap();
    assert_eq!(ack.win_session, 5);
    writer.write_message(&Message::UiRequest { req: 9, op: UiOp::GetNetwork }).unwrap();
    let Some(Message::UiReply { req, status, body }) = reader.read_message().unwrap() else {
        panic!("expected a reply");
    };
    assert_eq!((req, status), (9, STATUS_OK));
    assert_eq!(ReplyBody::decode(5, &body.0).unwrap(), ReplyBody::PairingId(77));
    drop(writer);
    service.join().unwrap();
}
