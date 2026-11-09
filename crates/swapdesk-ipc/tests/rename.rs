//! `RenameDevice` (op 30): its number, round trip, the name limit checked on encode and decode, and the empty reply.

use swapdesk_ipc::*;

fn round(msg: &Message) -> Message {
    decode_message(&encode_message(msg).expect("encodes")).expect("decodes")
}

#[test]
fn rename_device_has_op_30_and_round_trips() {
    let op = UiOp::RenameDevice { device_id: "ab".repeat(16), name: "Sala \u{e1}rea".into() };
    assert_eq!(op.code(), 30);
    let msg = Message::UiRequest { req: 7, op };
    assert_eq!(round(&msg), msg);
    assert_eq!(ReplyBody::decode(30, &[]).unwrap(), ReplyBody::Done);
}

#[test]
fn rename_device_name_is_bounded_by_max_name() {
    let at_limit = UiOp::RenameDevice { device_id: "x".into(), name: "n".repeat(MAX_NAME) };
    let msg = Message::UiRequest { req: 1, op: at_limit };
    assert_eq!(round(&msg), msg);
    let over =
        Message::UiRequest { req: 1, op: UiOp::RenameDevice { device_id: "x".into(), name: "n".repeat(MAX_NAME + 1) } };
    assert!(encode_message(&over).is_err());
}
