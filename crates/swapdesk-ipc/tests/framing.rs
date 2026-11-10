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
        payload: Payload::from(vec![1, 2]),
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
    // A video payload prints as text where its bytes are text and as `\x..` where they are not.
    let pixels = [b"PIXELS-OF-THE-SCREEN".as_slice(), &[0xAB; 13]].concat();
    shown.push(format!(
        "{:?}",
        Message::VideoFrame { header: video_header(pixels.len()), payload: Payload::from(pixels.clone()) }
    ));
    shown.push(format!(
        "{:?}",
        Message::ViewerVideo { session_id: 4, header: video_header(pixels.len()), payload: Payload::from(pixels) }
    ));
    let all = shown.join("\n");
    for needle in [
        "hunter2",
        "token-abc",
        secret,
        "argument-with",
        "BODYBYTES",
        "171",    // 0xAB as decimal, as a Vec<u8> would print it
        "xab",    // and as the shared bytes of a video payload would
        "PIXELS", // the text bytes of that payload
    ] {
        assert!(!all.contains(needle), "{needle} leaked in:\n{all}");
    }
    assert!(all.contains("Events(count 2)"), "{all}");
    assert!(all.contains("Text(len 15)"), "{all}");
    assert!(all.contains("Bytes(len 40)"), "{all}");
    assert!(all.contains("Bytes(len 9)"), "{all}");
    assert_eq!(all.matches("Payload(len 33)").count(), 2, "{all}");
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

// ---------- the video frame's own path through the reader and the writer ----------

fn video_header(payload_len: usize) -> swapdesk_proto::VideoHeader {
    swapdesk_proto::VideoHeader {
        keyframe: true,
        config: false,
        discontinuity: false,
        display_id: 0,
        frame_id: 1,
        capture_us: 2,
        encode_us: 3,
        width: 1920,
        height: 1080,
        codec: swapdesk_proto::Codec::H264,
        payload_len: payload_len as u32,
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A writer's stream that remembers how each `write` call cut the bytes.
#[derive(Default)]
struct Writes {
    bytes: Vec<u8>,
    calls: Vec<usize>,
}

impl Write for Writes {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        self.calls.push(buf.len());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that hands out at most `step` bytes per call.
struct Slow<R> {
    inner: R,
    step: usize,
}

impl<R: Read> Read for Slow<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.step.min(buf.len());
        self.inner.read(&mut buf[..n])
    }
}

#[test]
fn encode_into_gives_the_frame_in_two_parts_and_the_payload_is_not_copied() {
    let payload = Payload::from(pattern(5000));
    let at = payload.0.as_ptr();
    let msg = Message::ViewerVideo { session_id: 7, header: video_header(5000), payload };
    // What was in the buffer is discarded; the buffer itself is reused.
    let mut scratch = vec![0xEE; 300];
    let (head, tail) = encode_into(&msg, &mut scratch).unwrap();
    assert_eq!(tail.as_ptr(), at, "the tail is the payload itself, not a copy");
    assert_eq!([head, tail].concat(), encode_message(&msg).unwrap());
    assert_eq!(decode_message(&[head, tail].concat()).unwrap(), msg);
    // Every other message is whole in the head.
    let small = hello();
    let (head, tail) = encode_into(&small, &mut scratch).unwrap();
    assert!(tail.is_empty());
    assert_eq!(head, encode_message(&small).unwrap());
    // A message that cannot be encoded leaves the buffer usable.
    let long = Message::AudioPacket(Bytes(vec![0; 1501]));
    assert!(encode_into(&long, &mut scratch).is_err());
    let (head, _) = encode_into(&hello(), &mut scratch).unwrap();
    assert_eq!(head, encode_message(&hello()).unwrap());
}

#[test]
fn a_writer_sends_a_large_payload_by_itself_and_a_small_one_with_its_head() {
    for (len, calls) in [(0usize, 1usize), (300, 1), (16 * 1024, 1), (16 * 1024 + 1, 2), (200_000, 2)] {
        let msg = Message::VideoFrame { header: video_header(len), payload: Payload::from(pattern(len)) };
        let mut writer = FrameWriter::new(Writes::default());
        writer.write_message(&hello()).unwrap();
        writer.write_message(&msg).unwrap();
        writer.write_message(&hello()).unwrap();
        let out = writer.into_inner();
        let hello_bytes = encode_message(&hello()).unwrap();
        let expected = [hello_bytes.as_slice(), &encode_message(&msg).unwrap(), &hello_bytes].concat();
        assert_eq!(out.bytes, expected, "a payload of {len} bytes");
        assert_eq!(out.calls.len(), 2 + calls, "a payload of {len} bytes: {:?}", out.calls);
        if calls == 2 {
            assert_eq!(out.calls[2], len, "the payload went to the pipe in one piece");
        }
        // And a reader gets the three messages back, however the pipe cuts them.
        let mut reader = FrameReader::new(Slow { inner: Cursor::new(out.bytes), step: 4093 });
        assert_eq!(reader.read_message().unwrap(), Some(hello()));
        assert_eq!(reader.read_message().unwrap(), Some(msg));
        assert_eq!(reader.read_message().unwrap(), Some(hello()));
        assert_eq!(reader.read_message().unwrap(), None);
    }
}

#[test]
fn a_payload_larger_than_the_reader_reserves_ahead_is_read_whole() {
    // 1 MiB is reserved before the bytes arrive; the rest of the buffer follows them.
    for len in [1024 * 1024 - 1, 1024 * 1024, 1024 * 1024 + 1, 2 * 1024 * 1024 + 4567] {
        let msg =
            Message::ViewerVideo { session_id: 3, header: video_header(len), payload: Payload::from(pattern(len)) };
        let wire = encode_message(&msg).unwrap();
        let mut reader = FrameReader::new(Slow { inner: Cursor::new(wire.clone()), step: 100_000 });
        let read = reader.read_message().unwrap();
        assert_eq!(read.as_ref(), Some(&msg), "a payload of {len} bytes");
        assert_eq!(reader.read_message().unwrap(), None);
        // The message holds its payload in a buffer of exactly its length (the buffer grew with the bytes, and ends
        // with nothing spare): the only handle gives the buffer itself back.
        let Some(Message::ViewerVideo { payload, .. }) = read else { unreachable!() };
        let at = payload.0.as_ptr();
        let buffer = Vec::from(payload.0);
        assert_eq!((buffer.as_ptr(), buffer.len(), buffer.capacity()), (at, len, len), "a payload of {len} bytes");
        // Cut anywhere in the payload, the stream is an error, never a shorter frame.
        for cut in [wire.len() - 1, wire.len() - len / 2, wire.len() - len] {
            let mut reader = FrameReader::new(Cursor::new(wire[..cut].to_vec()));
            match reader.read_message() {
                Err(IpcError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
                other => panic!("a stream cut at {cut} of {}: {other:?}", wire.len()),
            }
        }
    }
}

#[test]
fn a_video_frame_whose_header_disagrees_with_its_length_is_refused_before_its_payload_is_read() {
    for (ty, session) in [(0x31u64, None), (0x50, Some(9u64))] {
        let mut body = session.map(|s: u64| s.to_le_bytes().to_vec()).unwrap_or_default();
        let mut w = Writer::new();
        swapdesk_proto::Wire::put(&video_header(4_000_000), &mut w, 0).unwrap();
        body.extend(w.into_bytes());
        let prefix = body.len();
        // The header says 4 MB; the length of the frame says 1000 bytes of payload.
        body.extend(pattern(1000));
        let wire = frame(ty, &body);
        let head = wire.len() - 1000;
        assert!(matches!(
            decode_message(&wire),
            Err(IpcError::Codec(CodecError::InvalidValue("video payload length")))
        ));
        let mut reader = FrameReader::new(Counting { inner: Cursor::new(wire), taken: 0 });
        assert!(matches!(
            reader.read_message(),
            Err(IpcError::Codec(CodecError::InvalidValue("video payload length")))
        ));
        assert_eq!(reader.into_inner().taken, head, "type {ty:#x}: the two varints and the {prefix}-byte prefix only");
        // A body too short to hold the header is the same error for both readers.
        let short = frame(ty, &body[..prefix - 1]);
        assert!(matches!(decode_message(&short), Err(IpcError::Codec(CodecError::Truncated))));
        let mut reader = FrameReader::new(Cursor::new(short));
        assert!(matches!(reader.read_message(), Err(IpcError::Codec(CodecError::Truncated))));
    }
}

#[test]
fn debug_of_a_reader_and_a_writer_shows_no_byte_of_the_last_message() {
    let secret = Message::UiReply { req: 1, status: STATUS_OK, body: Bytes(vec![233; 40]) };
    let mut writer = FrameWriter::new(io::sink());
    writer.write_message(&secret).unwrap();
    let shown = format!("{writer:?}");
    assert!(shown.starts_with("FrameWriter") && !shown.contains("233"), "{shown}");
    /// A stream whose own `Debug` shows nothing of what it holds.
    struct Quiet(Cursor<Vec<u8>>);
    impl Read for Quiet {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl core::fmt::Debug for Quiet {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("Quiet")
        }
    }
    let mut reader = FrameReader::new(Quiet(Cursor::new(encode_message(&secret).unwrap())));
    assert_eq!(reader.read_message().unwrap(), Some(secret));
    let shown = format!("{reader:?}");
    assert!(shown.starts_with("FrameReader") && shown.contains("Quiet") && !shown.contains("233"), "{shown}");
}
