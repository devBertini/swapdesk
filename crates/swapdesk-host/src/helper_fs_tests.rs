//! The user helper's disk server over the test pipe (stage 44): a service-side driver sends `FsRequest` messages to
//! `run_helper` and reads the `FsReply` of each. Containment, size limits and the life of a read id are proven here,
//! on the wire, with the helper's own `FsServer`.

use std::path::PathBuf;
use std::sync::Arc;

use swapdesk_files::{FsServer, Roots};
use swapdesk_ipc::{Bytes, FsAnswer, FsCall, FsKind, MAX_FS_IO, Message, Text};
use swapdesk_proto::ErrorCode;

use crate::helper_tests::{Rig, rig_with};

type Reply = Result<FsAnswer, ErrorCode>;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-helper-fs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn t(s: &str) -> Text {
    Text(s.to_string())
}

fn serving(home: &std::path::Path) -> Rig {
    rig_with(None, Some(Arc::new(FsServer::new(Roots::Home { home: home.to_path_buf(), receive: None }))))
}

/// One call and its reply (the number ties them).
fn call(rig: &mut Rig, req: u32, call: FsCall) -> Reply {
    rig.send(&Message::FsRequest { req, call });
    rig.wait(|m| match m {
        Message::FsReply { req: r, result } if *r == req => Some(result.clone()),
        _ => None,
    })
}

#[test]
fn the_helper_answers_disk_calls_in_the_users_home_and_refuses_paths_outside_it() {
    let base = temp("contain");
    let home = base.join("home");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    std::fs::write(base.join("secret.txt"), b"outside").unwrap();
    std::fs::write(home.join("docs").join("a.txt"), b"inside").unwrap();
    let mut rig = serving(&home);

    let Ok(FsAnswer::Entries { entries, truncated }) =
        call(&mut rig, 1, FsCall::ListDir { path: t("docs"), max_entries: 100 })
    else {
        panic!("a listing")
    };
    assert_eq!((entries.len(), truncated, entries[0].name.0.as_str()), (1, false, "a.txt"));
    // Every way out of the home is refused with InvalidName, by the helper itself.
    let mut req = 10;
    for bad in [
        "..",
        "../secret.txt",
        "docs/../../secret.txt",
        "/etc/passwd",
        "C:/Windows",
        "C:\\Windows",
        "docs\\..\\..\\secret.txt",
        "docs//a.txt",
    ] {
        for c in [
            FsCall::ListDir { path: t(bad), max_entries: 10 },
            FsCall::OpenRead { path: t(bad) },
            FsCall::Stat { path: t(bad), follow: true },
            FsCall::PartCreate { dir: t(bad), name: t("x") },
        ] {
            req += 1;
            assert_eq!(call(&mut rig, req, c), Err(ErrorCode::InvalidName), "{bad}");
        }
    }
    assert_eq!(
        call(&mut rig, 900, FsCall::PartRename { dir: t("docs"), name: t("x"), target_name: t("../y") }),
        Err(ErrorCode::InvalidName)
    );
    assert!(
        !base.join("x.swapdesk-part").exists()
            && !home.join("x.swapdesk-part").exists()
            && !home.join("docs").join("x.swapdesk-part").exists()
    );
    assert_eq!(std::fs::read(base.join("secret.txt")).unwrap(), b"outside");
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_helper_enforces_the_size_limits_and_the_append_offset() {
    let home = temp("limits");
    let mut rig = serving(&home);
    let dir = || t("");
    assert_eq!(call(&mut rig, 1, FsCall::PartCreate { dir: dir(), name: t("a.bin") }), Ok(FsAnswer::Done));
    assert_eq!(
        call(&mut rig, 2, FsCall::PartRead { dir: dir(), name: t("a.bin"), offset: 0, len: MAX_FS_IO as u32 + 1 }),
        Err(ErrorCode::TooLarge)
    );
    // A full chunk goes through the wire and the disk; a chunk at the wrong offset is a protocol violation.
    assert_eq!(
        call(
            &mut rig,
            3,
            FsCall::PartAppend { dir: dir(), name: t("a.bin"), offset: 0, data: Bytes(vec![7; MAX_FS_IO]) }
        ),
        Ok(FsAnswer::Done)
    );
    assert_eq!(
        call(&mut rig, 4, FsCall::PartAppend { dir: dir(), name: t("a.bin"), offset: 10, data: Bytes(vec![1]) }),
        Err(ErrorCode::ProtocolViolation)
    );
    assert_eq!(
        call(&mut rig, 5, FsCall::PartLen { dir: dir(), name: t("a.bin") }),
        Ok(FsAnswer::Len(MAX_FS_IO as u64))
    );
    let Ok(FsAnswer::Data(d)) = call(&mut rig, 6, FsCall::PartRead { dir: dir(), name: t("a.bin"), offset: 5, len: 4 })
    else {
        panic!("data")
    };
    assert_eq!(d.0, vec![7; 4]);
    // The meta file is read up to 1 KiB whatever its size.
    std::fs::write(home.join("a.bin.swapdesk-meta"), vec![3u8; 4000]).unwrap();
    let Ok(FsAnswer::Meta(Some(m))) = call(&mut rig, 7, FsCall::MetaRead { dir: dir(), name: t("a.bin") }) else {
        panic!("meta")
    };
    assert_eq!(m.0.len(), 1024);
    // A folder lists at most 2000 entries, whatever is asked.
    for i in 0..2050 {
        std::fs::write(home.join(format!("f{i:04}")), b"").unwrap();
    }
    let Ok(FsAnswer::Entries { entries, truncated }) =
        call(&mut rig, 8, FsCall::ListDir { path: dir(), max_entries: u32::MAX })
    else {
        panic!("a listing")
    };
    assert_eq!((entries.len(), truncated), (2000, true));
    // A name over 255 bytes never reaches the helper's disk code: the wire refuses it at the sender.
    assert!(
        swapdesk_ipc::encode_message(&Message::FsRequest {
            req: 9,
            call: FsCall::PartCreate { dir: dir(), name: Text("n".repeat(256)) }
        })
        .is_err()
    );
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_read_id_lives_only_in_the_helper_that_opened_it_and_close_read_has_no_reply() {
    let home = temp("ids");
    std::fs::write(home.join("one.txt"), b"first file").unwrap();
    std::fs::write(home.join("two.txt"), b"other file").unwrap();
    let mut a = serving(&home);
    let Ok(FsAnswer::Handle(h)) = call(&mut a, 1, FsCall::OpenRead { path: t("one.txt") }) else { panic!("a handle") };
    assert_eq!(
        call(&mut a, 2, FsCall::ReadAt { id: h.id, offset: 0, len: 5 }),
        Ok(FsAnswer::Data(Bytes(b"first".to_vec())))
    );
    // A close answers nothing: the next reply is the next request's, and the id is gone.
    a.send(&Message::FsRequest { req: 3, call: FsCall::CloseRead { id: h.id } });
    assert_eq!(call(&mut a, 4, FsCall::ReadAt { id: h.id, offset: 0, len: 5 }), Err(ErrorCode::NotFound));
    // A new helper (the old one died) numbers its handles afresh; the dead helper's id means nothing to it unless the
    // service uses it again, which `PipeFs` prevents by tagging ids with the link: the helper alone cannot tell.
    a.send(&Message::Shutdown);
    a.finish().expect("clean end");
    let mut b = serving(&home);
    assert_eq!(
        call(&mut b, 1, FsCall::ReadAt { id: h.id, offset: 0, len: 5 }),
        Err(ErrorCode::NotFound),
        "no handle was opened in this process"
    );
    assert_eq!(call(&mut b, 2, FsCall::ReadStat { id: h.id }), Err(ErrorCode::NotFound));
    let Ok(FsAnswer::Handle(h2)) = call(&mut b, 3, FsCall::OpenRead { path: t("two.txt") }) else { panic!("a handle") };
    assert_eq!(
        call(&mut b, 4, FsCall::ReadAt { id: h2.id, offset: 0, len: 10 }),
        Ok(FsAnswer::Data(Bytes(b"other file".to_vec())))
    );
    // Not a regular file, and a missing one.
    assert_eq!(call(&mut b, 5, FsCall::OpenRead { path: t("") }), Err(ErrorCode::IoFailed));
    assert_eq!(call(&mut b, 6, FsCall::OpenRead { path: t("nope.txt") }), Err(ErrorCode::NotFound));
    b.send(&Message::Shutdown);
    b.finish().expect("clean end");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_helper_with_no_disk_fails_every_call_and_stays_silent_for_a_close() {
    let mut rig = rig_with(None, None);
    assert_eq!(call(&mut rig, 1, FsCall::PartLen { dir: t(""), name: t("a") }), Err(ErrorCode::IoFailed));
    rig.send(&Message::FsRequest { req: 2, call: FsCall::CloseRead { id: 1 } });
    assert_eq!(call(&mut rig, 3, FsCall::ReadStat { id: 1 }), Err(ErrorCode::IoFailed));
    let _ = FsKind::Part;
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
}

/// A system part whose bin never answers until the test lets it: stands for the shell's delete waiting on a dialog.
struct StuckBin {
    root: PathBuf,
    entered: std::sync::atomic::AtomicBool,
    gate: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl swapdesk_files::SystemDisk for StuckBin {
    fn volumes(&self) -> Vec<swapdesk_files::SysVolume> {
        vec![swapdesk_files::SysVolume {
            identity: "stuck".into(),
            root: self.root.clone(),
            kind: swapdesk_proto::VolumeKind::Fixed,
            label: String::new(),
            letter: None,
            total_bytes: 1000,
            free_bytes: 500,
            has_bin: true,
        }]
    }

    fn places(&self) -> Vec<(swapdesk_proto::PlaceKind, PathBuf)> {
        Vec::new()
    }

    fn trash(&self, path: &std::path::Path) -> Result<(), ErrorCode> {
        self.entered.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.gate.lock().unwrap_or_else(|p| p.into_inner()).recv();
        std::fs::remove_file(path).map_err(|_| ErrorCode::IoFailed)
    }

    fn protected(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

#[test]
fn a_trash_that_waits_on_the_shell_holds_no_other_disk_call() {
    // The shell's delete may stop on a dialog (an item it cannot recycle); the helper's disk thread must not stop with
    // it: a listing is answered while the trash waits, and the trash then answers with its own result.
    let home = temp("stuckbin");
    std::fs::write(home.join("old.txt"), b"x").unwrap();
    let (release, gate) = std::sync::mpsc::channel();
    let bin = Arc::new(StuckBin {
        root: home.clone(),
        entered: std::sync::atomic::AtomicBool::new(false),
        gate: std::sync::Mutex::new(gate),
    });
    let server = FsServer::new(Roots::Home { home: home.clone(), receive: None }).with_system(bin.clone());
    let mut rig = rig_with(None, Some(Arc::new(server)));
    rig.send(&Message::FsRequest { req: 1, call: FsCall::Trash { dir: t(""), name: t("old.txt") } });
    let start = std::time::Instant::now();
    while !bin.entered.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "the trash never reached the bin");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let listing = call(&mut rig, 2, FsCall::ListPage { path: t(""), start: 0, max: 10 });
    assert!(matches!(listing, Ok(FsAnswer::Entries2(_))), "a listing while the trash waits: {listing:?}");
    release.send(()).unwrap();
    let trashed = rig.wait(|m| match m {
        Message::FsReply { req: 1, result } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(trashed, Ok(FsAnswer::Done));
    assert!(!home.join("old.txt").exists());
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
    let _ = std::fs::remove_dir_all(&home);
}
