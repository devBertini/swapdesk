//! The clipboard and quick-action messages (0x60 to 0x66, ops 28 and 29, notify 14): numbers, round trips, limits
//! checked before allocation and the redacting `Debug`.

use std::io::Cursor;

use swapdesk_ipc::*;
use swapdesk_proto::{Action, ErrorCode, Writer};

fn round(msg: &Message) -> Message {
    let bytes = encode_message(msg).expect("encodes");
    decode_message(&bytes).expect("decodes")
}

fn varint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_varint(v).unwrap();
    w.into_bytes()
}

#[test]
fn every_new_message_has_its_number_and_round_trips() {
    let all = [
        (Message::ClipChangeCount { req: 1 }, 0x60),
        (Message::ClipRead { req: 2 }, 0x61),
        (Message::ClipWrite { req: 3, content: ClipData::Text("olá".into()) }, 0x62),
        (Message::ClipWrite { req: 3, content: ClipData::Png(vec![1, 2, 3]) }, 0x62),
        (Message::ClipReply { req: 4, body: ClipReplyBody::Count(u64::MAX) }, 0x63),
        (Message::ClipReply { req: 4, body: ClipReplyBody::Content(ClipData::Text(String::new())) }, 0x63),
        (Message::ClipReply { req: 4, body: ClipReplyBody::Concealed }, 0x63),
        (Message::ClipReply { req: 4, body: ClipReplyBody::Empty }, 0x63),
        (Message::ClipReply { req: 4, body: ClipReplyBody::Unsupported }, 0x63),
        (Message::ClipReply { req: 4, body: ClipReplyBody::Written }, 0x63),
        (Message::ClipReply { req: 4, body: ClipReplyBody::Failed(ClipFail::Invalid) }, 0x63),
        (Message::RunAction { req: 5, action: Action::TaskMonitor }, 0x64),
        (Message::KeepAwake { req: 6, on: true }, 0x65),
        (Message::ActionReply { req: 7, code: ErrorCode::NobodyPresent }, 0x66),
    ];
    for (msg, code) in all {
        assert_eq!(msg.type_code(), code);
        assert_eq!(round(&msg), msg);
    }
}

#[test]
fn the_new_ops_and_notify_carry_their_numbers() {
    let sync = UiOp::SetClipboardSync { session_id: 1, on: false };
    let run = UiOp::RunAction { session_id: 2, action: Action::Lock };
    assert_eq!((sync.code(), run.code()), (28, 29));
    for op in [sync, run] {
        let msg = Message::UiRequest { req: 9, op };
        assert_eq!(round(&msg), msg);
    }
    let n = Notify::ClipboardSkipped { session_id: 3, reason: ApiCode::Invalid };
    assert_eq!(n.kind(), 14);
    let msg = Message::UiNotify(n);
    assert_eq!(round(&msg), msg);
    for op in [28, 29] {
        assert_eq!(ReplyBody::decode(op, &[]).unwrap(), ReplyBody::Done);
    }
}

#[test]
fn clipboard_content_never_reaches_debug() {
    let secret = "hunter2-segredo";
    let shown = [
        format!("{:?}", Message::ClipWrite { req: 1, content: ClipData::Text(secret.into()) }),
        format!("{:?}", Message::ClipReply { req: 1, body: ClipReplyBody::Content(ClipData::Text(secret.into())) }),
        format!("{:?}", Message::ClipWrite { req: 1, content: ClipData::Png(secret.bytes().collect()) }),
    ];
    for s in shown {
        assert!(!s.contains(secret) && !s.contains("hunter"), "{s}");
        assert!(s.contains("len"), "{s}");
    }
}

#[test]
fn an_oversize_content_is_refused_before_it_is_sent_and_before_it_is_read() {
    use swapdesk_proto::{MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_TEXT};
    let text = Message::ClipWrite { req: 1, content: ClipData::Text("x".repeat(MAX_CLIPBOARD_TEXT as usize + 1)) };
    assert!(encode_message(&text).is_err());
    let png = Message::ClipWrite { req: 1, content: ClipData::Png(vec![0; MAX_CLIPBOARD_PNG as usize + 1]) };
    assert!(encode_message(&png).is_err());

    // A text body one byte over the text limit, inside the message limit: refused by the content check.
    let mut body = Writer::new();
    body.put_u32(1);
    body.put_u8(0);
    body.put_bytes(&vec![b'x'; MAX_CLIPBOARD_TEXT as usize + 1]);
    let mut frame = varint(0x62);
    frame.extend(varint(body.len() as u64));
    frame.extend(body.into_bytes());
    assert!(decode_message(&frame).is_err());

    // A 4 GiB claim for 0x62 is refused before any body byte is read.
    let mut stream = varint(0x62);
    stream.extend(varint(4 << 30));
    stream.extend_from_slice(&[7u8; 10]);
    let mut reader = FrameReader::new(Cursor::new(stream));
    assert!(matches!(reader.read_message(), Err(IpcError::TooLarge { ty: 0x62, .. })));

    // Small messages keep the 1 MiB limit.
    let mut stream = varint(0x64);
    stream.extend(varint(2 << 20));
    let mut reader = FrameReader::new(Cursor::new(stream));
    assert!(matches!(reader.read_message(), Err(IpcError::TooLarge { ty: 0x64, .. })));
}

#[test]
fn an_unknown_tag_or_bad_utf8_is_a_codec_error() {
    let mut body = Writer::new();
    body.put_u32(1);
    body.put_u8(9);
    let mut frame = varint(0x62);
    frame.extend(varint(body.len() as u64));
    frame.extend(body.into_bytes());
    assert!(decode_message(&frame).is_err());

    let mut body = Writer::new();
    body.put_u32(1);
    body.put_u8(0);
    body.put_bytes(&[0xff, 0xfe]);
    let mut frame = varint(0x62);
    frame.extend(varint(body.len() as u64));
    frame.extend(body.into_bytes());
    assert!(decode_message(&frame).is_err());
}
