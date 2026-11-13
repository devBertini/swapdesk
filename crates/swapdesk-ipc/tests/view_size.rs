//! F-VIEW-06: `SetViewSize` (UI op 61, a `Done` reply) and `SetScale` (helper message 0x2B) keep their numbers and
//! round trip.

use swapdesk_ipc::*;

fn round(msg: &Message) -> Message {
    decode_message(&encode_message(msg).expect("encodes")).expect("decodes")
}

#[test]
fn set_view_size_has_op_61_and_round_trips() {
    let op = UiOp::SetViewSize { session_id: 0xAB_CDEF, width: 1920, height: 1080, max_fps: 144 };
    assert_eq!(op.code(), 61);
    let msg = Message::UiRequest { req: 9, op };
    assert_eq!(round(&msg), msg);
    // Native (1:1 mode) is 0x0.
    let native =
        Message::UiRequest { req: 9, op: UiOp::SetViewSize { session_id: 1, width: 0, height: 0, max_fps: 60 } };
    assert_eq!(round(&native), native);
    assert_eq!(ReplyBody::decode(61, &[]).unwrap(), ReplyBody::Done);
}

#[test]
fn set_scale_is_message_0x2b_and_round_trips() {
    let msg = Message::SetScale { display: 2, width: 960, height: 540 };
    assert_eq!(msg.type_code(), 0x2B);
    assert_eq!(round(&msg), msg);
}
