//! The files messages of C2.4: round trips, limits checked before allocation, and a `Debug` that shows no path and
//! no byte.

use swapdesk_ipc::*;
use swapdesk_proto::{ErrorCode, Wire, Writer};

fn t(s: &str) -> Text {
    Text(s.to_string())
}

fn calls() -> Vec<FsCall> {
    vec![
        FsCall::ListDir { path: t("Documents/x"), max_entries: 2000 },
        FsCall::Stat { path: t(""), follow: true },
        FsCall::OpenRead { path: t("p1/report.pdf") },
        FsCall::ReadAt { id: 9, offset: 1 << 40, len: 1 << 20 },
        FsCall::ReadStat { id: 9 },
        FsCall::CloseRead { id: 9 },
        FsCall::StatFile { dir: t("r1"), name: t("a.bin"), kind: FsKind::Meta },
        FsCall::PartCreate { dir: t("r1"), name: t("a.bin") },
        FsCall::PartTruncate { dir: t("r1"), name: t("a.bin"), len: 7 },
        FsCall::PartAppend { dir: t("r1"), name: t("a.bin"), offset: 3, data: Bytes(vec![1, 2, 3]) },
        FsCall::PartLen { dir: t("r1"), name: t("a.bin") },
        FsCall::PartRead { dir: t("r1"), name: t("a.bin"), offset: 0, len: 5 },
        FsCall::PartSync { dir: t("r1"), name: t("a.bin"), final_mtime_ms: Some(5) },
        FsCall::PartRename { dir: t("r1"), name: t("a.bin"), target_name: t("a (2).bin") },
        FsCall::RemoveFile { dir: t("r1"), name: t("a.bin"), kind: FsKind::Part },
        FsCall::MetaRead { dir: t("r1"), name: t("a.bin") },
        FsCall::MetaWrite { dir: t("r1"), name: t("a.bin"), data: Bytes(vec![9; 40]) },
    ]
}

fn answers() -> Vec<core::result::Result<FsAnswer, ErrorCode>> {
    let stat = FsStatView { is_dir: false, is_file: true, is_reparse: false, len: 3, mtime_ms: 4 };
    vec![
        Ok(FsAnswer::Done),
        Ok(FsAnswer::Entries {
            entries: vec![FsEntryView { name: t("a"), is_dir: true, size: 0, mtime_ms: 1 }],
            truncated: true,
        }),
        Ok(FsAnswer::Stat(None)),
        Ok(FsAnswer::Stat(Some(stat.clone()))),
        Ok(FsAnswer::Handle(FsHandleView { id: 1, len: 2, mtime_ms: 3 })),
        Ok(FsAnswer::Data(Bytes(vec![7; 100]))),
        Ok(FsAnswer::StatNow(stat)),
        Ok(FsAnswer::Len(99)),
        Ok(FsAnswer::Meta(None)),
        Ok(FsAnswer::Meta(Some(Bytes(vec![1; 40])))),
        Err(ErrorCode::NotFound),
        Err(ErrorCode::IoFailed),
        Err(ErrorCode::InvalidName),
    ]
}

fn round(msg: &Message) {
    let bytes = encode_message(msg).unwrap();
    assert_eq!(&decode_message(&bytes).unwrap(), msg);
    let mut reader = FrameReader::new(&bytes[..]);
    assert_eq!(&reader.read_message().unwrap().unwrap(), msg);
}

fn frame_of(ty: u64, body: &[u8]) -> Vec<u8> {
    let mut frame = Writer::new();
    frame.put_varint(ty).unwrap();
    frame.put_varint(body.len() as u64).unwrap();
    frame.put_bytes(body);
    frame.into_bytes()
}

#[test]
fn every_call_and_answer_round_trips() {
    for (i, call) in calls().into_iter().enumerate() {
        assert_eq!(usize::from(call.code()), i + 1);
        round(&Message::FsRequest { req: i as u32, call });
    }
    for (i, result) in answers().into_iter().enumerate() {
        round(&Message::FsReply { req: i as u32, result });
    }
}

#[test]
fn ops_notes_and_reply_bodies_round_trip() {
    let ops = [
        FilesOp::List { session_id: 1, path: None },
        FilesOp::List { session_id: 1, path: Some(t("a/b")) },
        FilesOp::Send {
            session_id: 2,
            picks: vec![FilePick { token: 1, name: t("x.txt") }, FilePick { token: 2, name: t("y") }],
            remote_dir: Some(t("")),
        },
        FilesOp::Get { session_id: 3, remote_path: t("a/b.txt"), recv_token: 4 },
        FilesOp::Pause { transfer_id: 5 },
        FilesOp::Resume { transfer_id: 6 },
        FilesOp::Cancel { transfer_id: 7 },
    ];
    for (op, code) in ops.iter().zip([40u16, 40, 41, 42, 43, 44, 45]) {
        assert_eq!(UiOp::Files(op.clone()).code(), code);
        round(&Message::UiRequest { req: 1, op: UiOp::Files(op.clone()) });
    }
    let n = |outgoing, state| FilesNote::Transfer { session_id: 1, transfer_id: 2, name: t("n"), outgoing, state };
    let notes = [
        n(true, TransferWire::Queued),
        n(false, TransferWire::Running { done: 1, total: 2, bytes_per_s: 3 }),
        n(false, TransferWire::Paused { done: 1, total: 2, why: ApiCode::Timeout }),
        n(false, TransferWire::Done),
        n(false, TransferWire::Failed { reason: ApiCode::NotPermitted }),
        FilesNote::SavedAs { session_id: 1, transfer_id: 2, name: t("n (2)") },
    ];
    for (note, kind) in notes.iter().zip([20u16, 20, 20, 20, 20, 21]) {
        assert_eq!(Notify::Files(note.clone()).kind(), kind);
        round(&Message::UiNotify(Notify::Files(note.clone())));
    }
    let listing = FileListing {
        path: t("Docs"),
        entries: vec![FileEntry { name: t("a"), is_dir: false, size: 1, mtime_ms: 2 }],
        truncated: false,
    };
    for (op, body) in [
        (40u16, ReplyBody::Listing(listing)),
        (41, ReplyBody::TransferIds(vec![1, 2, 3])),
        (42, ReplyBody::TransferId(9)),
        (43, ReplyBody::Done),
    ] {
        let bytes = body.encode().unwrap();
        assert_eq!(ReplyBody::decode(op, &bytes).unwrap(), body);
    }
}

#[test]
fn a_declared_length_over_the_limit_is_refused_before_a_byte_is_read() {
    let limit = MAX_FS_MESSAGE as u64;
    for ty in [0x80u64, 0x81] {
        let mut w = Writer::new();
        w.put_varint(ty).unwrap();
        w.put_varint(limit + 1).unwrap();
        let frame = w.into_bytes();
        // No body follows: a reader that tried to read it would fail with an I/O error instead.
        let err = FrameReader::new(&frame[..]).read_message().unwrap_err();
        assert!(matches!(err, IpcError::TooLarge { .. }), "{err:?}");
    }
}

#[test]
fn chunks_paths_and_lists_over_their_limit_are_refused() {
    let req = |call| encode_message(&Message::FsRequest { req: 1, call });
    assert!(
        req(FsCall::PartAppend { dir: t("r1"), name: t("a"), offset: 0, data: Bytes(vec![0; MAX_FS_IO + 1]) }).is_err()
    );
    assert!(
        req(FsCall::PartAppend { dir: t("r1"), name: t("a"), offset: 0, data: Bytes(vec![0; MAX_FS_IO]) }).is_ok(),
        "a full chunk fits the message limit"
    );
    assert!(req(FsCall::OpenRead { path: Text("a".repeat(MAX_FS_PATH + 1)) }).is_err());
    assert!(req(FsCall::PartCreate { dir: t(""), name: Text("a".repeat(MAX_FS_NAME + 1)) }).is_err());
    assert!(req(FsCall::MetaWrite { dir: t(""), name: t("a"), data: Bytes(vec![0; MAX_FS_META + 1]) }).is_err());
    let many = FsAnswer::Entries {
        entries: (0..=MAX_FS_LIST).map(|_| FsEntryView { name: t("a"), is_dir: false, size: 0, mtime_ms: 0 }).collect(),
        truncated: false,
    };
    assert!(encode_message(&Message::FsReply { req: 1, result: Ok(many) }).is_err());
    let picks = FilesOp::Send {
        session_id: 1,
        picks: (0..=MAX_PICKS as u32).map(|i| FilePick { token: i, name: t("a") }).collect(),
        remote_dir: None,
    };
    assert!(encode_message(&Message::UiRequest { req: 1, op: UiOp::Files(picks) }).is_err());

    // A hostile body: a chunk over the limit inside a legal frame length is refused by the decoder.
    let mut w = Writer::new();
    w.put_u32(1);
    w.put_u8(10);
    t("r1").put(&mut w, MAX_FS_PATH).unwrap();
    t("a").put(&mut w, MAX_FS_NAME).unwrap();
    w.put_u64(0);
    w.put_bytes(&vec![0; MAX_FS_IO + 1]);
    assert!(decode_message(&frame_of(0x80, &w.into_bytes())).is_err());
    // An unknown call number is a protocol error.
    let mut w = Writer::new();
    w.put_u32(1);
    w.put_u8(99);
    assert!(decode_message(&frame_of(0x80, &w.into_bytes())).is_err());
}

#[test]
fn debug_prints_lengths_never_a_path_or_a_byte() {
    let secret_path = "Documents/tax-return-2026.pdf";
    let mut shown = String::new();
    for call in calls() {
        let call = match call {
            FsCall::PartAppend { dir, name, offset, .. } => {
                FsCall::PartAppend { dir, name, offset, data: Bytes(b"SECRETBYTES".to_vec()) }
            }
            FsCall::OpenRead { .. } => FsCall::OpenRead { path: t(secret_path) },
            other => other,
        };
        shown += &format!("{:?}", Message::FsRequest { req: 1, call });
    }
    shown += &format!("{:?}", Message::FsReply { req: 1, result: Ok(FsAnswer::Data(Bytes(b"SECRETBYTES".to_vec()))) });
    shown += &format!(
        "{:?}",
        Message::FsReply {
            req: 1,
            result: Ok(FsAnswer::Entries {
                entries: vec![FsEntryView { name: t(secret_path), is_dir: false, size: 1, mtime_ms: 1 }],
                truncated: false
            })
        }
    );
    shown += &format!(
        "{:?}",
        Message::UiRequest {
            req: 1,
            op: UiOp::Files(FilesOp::Get { session_id: 1, remote_path: t(secret_path), recv_token: 1 })
        }
    );
    shown += &format!(
        "{:?}",
        Message::UiNotify(Notify::Files(FilesNote::SavedAs { session_id: 1, transfer_id: 1, name: t(secret_path) }))
    );
    shown +=
        &format!("{:?}", ReplyBody::Listing(FileListing { path: t(secret_path), entries: vec![], truncated: false }));
    assert!(!shown.contains("tax-return"), "{shown}");
    assert!(!shown.contains("SECRETBYTES") && !shown.contains("[83, 69"), "{shown}");
    assert!(shown.contains("len"));
}
