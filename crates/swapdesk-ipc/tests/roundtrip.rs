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
    InputEvent, Intent, KeyframeRequest, MouseButton, Os, Preset, ReceiverReport, VideoHeader, Wire,
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
            (txt(64), pick(Os::ALL), pick(Ready::ALL), any::<bool>()).prop_map(|(name, os, ready, legacy_note)| (1u16, ReplyBody::ThisDevice(ThisDevice { name, os, ready, legacy_note }))),
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

#[test]
fn elevation_ops_and_replies_round_trip() {
    let t = ElevationTicket([7; 16]);
    let action = ElevAction::SetGrants { device_id: "dev-1".into(), grants: 0x1ff };
    let ops = [
        (UiOp::ElevationBegin { action: action.clone() }, 35),
        (UiOp::ElevationDescribe { ticket: t }, 36),
        (UiOp::ElevationConfirm { ticket: t, approve: true }, 37),
        (UiOp::ElevationConfirm { ticket: t, approve: false }, 37),
        (UiOp::ElevationEnd { ticket: t }, 38),
    ];
    for (op, code) in ops {
        assert_eq!(op.code(), code);
        let msg = Message::UiRequest { req: 9, op };
        assert_eq!(decode_message(&encode_message(&msg).unwrap()).unwrap(), msg);
    }
    let replies = [
        (35u16, ReplyBody::ElevationTicket { ticket: t, ttl_s: 120 }),
        (36, ReplyBody::ElevationAction { action, peer_name: "Sala".into(), remaining_s: 97 }),
        (37, ReplyBody::Done),
    ];
    for (op, body) in replies {
        assert_eq!(ReplyBody::decode(op, &body.encode().unwrap()).unwrap(), body);
    }
    assert_eq!(ElevationOutcome::ALL.len(), 4);
    for (i, outcome) in ElevationOutcome::ALL.iter().enumerate() {
        assert_eq!(usize::from(outcome.tag()), i);
        let body = ReplyBody::ElevationOutcome(*outcome);
        assert_eq!(ReplyBody::decode(38, &body.encode().unwrap()).unwrap(), body);
    }
    assert!(ReplyBody::decode(38, &[4]).is_err());
    assert_eq!(IpcRole::Elevator.tag(), 4);
    let hello = Message::Hello { role: IpcRole::Elevator, version: IPC_VERSION, pid: 11 };
    assert_eq!(decode_message(&encode_message(&hello).unwrap()).unwrap(), hello);
}

#[test]
fn a_connect_entry_end_round_trips_for_every_why() {
    assert_eq!(ConnectWhy::ALL.len(), 8);
    for (i, why) in ConnectWhy::ALL.iter().enumerate() {
        assert_eq!(usize::from(why.tag()), i, "tags are appended, never reordered");
        let state = SessionState::NotOpened { why: *why };
        let note = Notify::Session { session_id: 1 << 63 | 5, device_id: "dev-1".into(), state };
        let msg = Message::UiNotify(note);
        let bytes = encode_message(&msg).unwrap();
        assert_eq!(decode_message(&bytes).unwrap(), msg);
    }
    // The state is tag 7, after `Ended`; a tag past the last why is a codec error.
    let mut w = swapdesk_proto::Writer::new();
    SessionState::NotOpened { why: ConnectWhy::Failed }.put(&mut w, 0).unwrap();
    assert_eq!(w.into_bytes(), [7, 7]);
    assert!(SessionState::get(&mut swapdesk_proto::Reader::new(&[7, 8]), 0).is_err());
}

#[test]
fn an_elevation_ticket_debug_prints_no_byte() {
    let t = ElevationTicket([0xAB; 16]);
    assert_eq!(format!("{t:?}"), "ElevationTicket(16 bytes)");
    let op = format!("{:?}", UiOp::ElevationConfirm { ticket: t, approve: true });
    assert!(!op.contains("171") && !op.to_lowercase().contains("ab"), "{op}");
    let reply = format!("{:?}", ReplyBody::ElevationTicket { ticket: t, ttl_s: 120 });
    assert!(reply.contains("16 bytes") && !reply.contains("171"), "{reply}");
}

#[test]
fn an_unknown_elev_action_tag_is_a_codec_error() {
    let begin = Message::UiRequest {
        req: 1,
        op: UiOp::ElevationBegin { action: ElevAction::SetGrants { device_id: "d".into(), grants: 1 } },
    };
    let wire = encode_message(&begin).unwrap();
    // The action is the last body of the request: tag, str(len 1, "d"), u16.
    let tag_at = wire.len() - 5;
    assert_eq!(wire[tag_at], 0);
    for tag in [1u8, 2, 255] {
        let mut bad = wire.clone();
        bad[tag_at] = tag;
        assert!(matches!(decode_message(&bad), Err(IpcError::Codec(_))), "request, tag {tag}");
        let mut body = ReplyBody::ElevationAction {
            action: ElevAction::SetGrants { device_id: "d".into(), grants: 1 },
            peer_name: "p".into(),
            remaining_s: 5,
        }
        .encode()
        .unwrap();
        body[0] = tag;
        assert!(matches!(ReplyBody::decode(36, &body), Err(IpcError::Codec(_))), "reply, tag {tag}");
    }
}

/// ELEVATION.md 3.1 and ACCOUNTS.md 9.3: the actions of tags 2 and 3 and op 50 round-trip; tag 1, and every tag from
/// 4 up, is a codec error.
#[test]
fn elev_action_tags_2_and_3_round_trip_and_tag_1_is_a_codec_error() {
    for action in [ElevAction::OpenAdminTerminal { session_id: 7 }, ElevAction::ElevateSession { session_id: u64::MAX }]
    {
        let msg = Message::UiRequest { req: 3, op: UiOp::ElevationBegin { action } };
        assert_eq!(decode_message(&encode_message(&msg).unwrap()).unwrap(), msg);
    }
    let op = Message::UiRequest { req: 4, op: UiOp::ElevateSession { session_id: 9 } };
    assert_eq!(decode_message(&encode_message(&op).unwrap()).unwrap(), op);
    let mut bytes = encode_message(&Message::UiRequest {
        req: 5,
        op: UiOp::ElevationBegin { action: ElevAction::ElevateSession { session_id: 1 } },
    })
    .unwrap();
    // The action's tag is the first byte after the op's header: find tag 3 followed by session id 1 and break it.
    let at = bytes.windows(9).position(|w| w == [3, 1, 0, 0, 0, 0, 0, 0, 0]).expect("the action");
    for bad in [1u8, 4, 255] {
        bytes[at] = bad;
        assert!(decode_message(&bytes).is_err(), "tag {bad}");
    }
}

/// UPDATES.md 13: the updater's ops 51 to 56, its view and its two notifications round-trip.
#[test]
fn update_ops_round_trip() {
    let ops = [
        (UiOp::UpdateGet, 51),
        (UiOp::UpdateCheckNow, 52),
        (UiOp::UpdateInstall { version: "1.1.1".into(), end_sessions: true }, 53),
        (UiOp::UpdateCancel, 54),
        (UiOp::UpdateDismiss { version: "1.1.1".into() }, 55),
        (UiOp::UpdateSetCheck { on: false }, 56),
    ];
    for (op, code) in ops {
        assert_eq!(op.code(), code);
        let msg = Message::UiRequest { req: 3, op };
        assert_eq!(decode_message(&encode_message(&msg).unwrap()).unwrap(), msg);
    }
    let phases = [
        UpdatePhase::Idle,
        UpdatePhase::Checking,
        UpdatePhase::Offered { version: "1.1.1".into(), manual: false },
        UpdatePhase::Downloading { version: "1.1.1".into(), done: 4_100_000, total: 12_400_000 },
        UpdatePhase::Verifying { version: "1.1.1".into() },
        UpdatePhase::Ready { version: "1.1.1".into() },
        UpdatePhase::Failed { reason: 5 },
    ];
    for phase in phases {
        let view = UpdateView {
            running: "1.1.0".into(),
            phase,
            check_on: true,
            last_check_ms: Some(1_700_000_000_000),
            dismissed: Some("1.1.1".into()),
            last_failure: Some(1),
            rolled_back: Some("1.1.2".into()),
            updated: true,
        };
        let reply =
            Message::UiReply { req: 3, status: 0, body: Bytes(ReplyBody::Update(view.clone()).encode().unwrap()) };
        assert_eq!(decode_message(&encode_message(&reply).unwrap()).unwrap(), reply);
        let note = Message::UiNotify(Notify::UpdateChanged(view));
        assert_eq!(decode_message(&encode_message(&note).unwrap()).unwrap(), note);
    }
    let restarting = Message::UiNotify(Notify::UpdateRestarting);
    assert_eq!(decode_message(&encode_message(&restarting).unwrap()).unwrap(), restarting);
}
