//! Property tests: every message, every op, every notification and every reply body survives encode and decode,
//! both through `decode_message` and through a `FrameReader`.

mod common;

#[allow(unused_imports)]
use common::*;
use proptest::collection::vec;
use proptest::prelude::*;
use swapdesk_ipc::*;
#[allow(unused_imports)]
use swapdesk_proto::{
    Codec, CursorPosition, CursorShape, CursorShapeHeader, Desktop, Display, DisplayList, ErrorCode, Feature,
    InputEvent, Intent, KeyframeRequest, MouseButton, Os, Preset, ReceiverReport, VideoHeader,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn every_message_round_trips(msg in any_message()) {
        let bytes = encode_message(&msg).unwrap();
        prop_assert_eq!(decode_message(&bytes).unwrap(), msg.clone());
        let mut reader = FrameReader::new(std::io::Cursor::new(bytes));
        prop_assert_eq!(reader.read_message().unwrap(), Some(msg));
        prop_assert!(reader.read_message().unwrap().is_none());
    }

    #[test]
    fn every_reply_body_round_trips(
        (op, body) in prop_oneof![
            Just((6u16, ReplyBody::Done)),
            Just((20u16, ReplyBody::Done)),
            (txt(64), pick(Os::ALL), pick(Ready::ALL)).prop_map(|(name, os, ready)| (1u16, ReplyBody::ThisDevice(ThisDevice { name, os, ready }))),
            network().prop_map(|n| (2u16, ReplyBody::Network(n))),
            vec(device(), 0..3).prop_map(|d| (3u16, ReplyBody::Devices(d))),
            device().prop_map(|d| (4u16, ReplyBody::Device(d))),
            any::<u32>().prop_map(|v| (5u16, ReplyBody::PairingId(v))),
            any::<u64>().prop_map(|v| (11u16, ReplyBody::SessionId(v))),
            any::<u64>().prop_map(|v| (26u16, ReplyBody::SessionId(v))),
            any::<u32>().prop_map(|v| (22u16, ReplyBody::RequestId(v))),
            chat_item().prop_map(|v| (24u16, ReplyBody::Chat(v))),
            Just((23u16, ReplyBody::Done)),
            Just((25u16, ReplyBody::Done)),
            settings().prop_map(|s| (19u16, ReplyBody::Settings(s))),
            vec(audit(), 0..3).prop_map(|a| (21u16, ReplyBody::Activity(a))),
            vec(display_view(), 0..3).prop_map(|d| (27u16, ReplyBody::Displays(d))),
        ]
    ) {
        let bytes = body.encode().unwrap();
        prop_assert_eq!(ReplyBody::decode(op, &bytes).unwrap(), body);
    }

    #[test]
    fn a_status_is_ok_or_an_api_code_plus_one(code in api_code()) {
        prop_assert_eq!(status_code(STATUS_OK).unwrap(), None);
        prop_assert_eq!(status_code(code.status()).unwrap(), Some(code));
        prop_assert!(status_code(ApiCode::ALL.len() as u16 + 1).is_err());
    }
}

#[test]
fn the_new_ops_carry_their_numbers_and_the_chat_text_stays_out_of_debug() {
    let ops = [
        (UiOp::RequestSwap { session_id: 1 }, 22),
        (UiOp::AnswerSwap { request_id: 2, accept: true }, 23),
        (UiOp::ChatSend { session_id: 3, text: Text("segredo".into()) }, 24),
        (UiOp::ChatMarkRead { session_id: 4 }, 25),
        (UiOp::Wake { device_id: "d".into(), intent: Intent::View }, 26),
        (UiOp::ListDisplays { session_id: 5 }, 27),
    ];
    for (op, code) in &ops {
        assert_eq!(op.code(), *code);
    }
    assert!(!format!("{:?}", ops[2].0).contains("segredo"), "the chat text of a request is never printed");
    let note =
        Notify::Chat { session_id: 1, item: ChatItem { msg_id: 1, text: Text("segredo".into()), sent_at_ms: 2 } };
    assert!(!format!("{note:?}").contains("segredo"), "chat text is never printed");
    // A text over the limit is refused before it is sent.
    let long = UiOp::ChatSend { session_id: 1, text: Text("x".repeat(4097)) };
    assert!(encode_message(&Message::UiRequest { req: 1, op: long }).is_err());
}
