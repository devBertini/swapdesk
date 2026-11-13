//! The explorer's messages (FILES-UI.md 7.3, minor 2) through a host engine and a viewer engine: the host's disk is
//! the real chain (`PipeFs` over `FsServer` over the `LocalFs`, with a fake system part publishing two volumes).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use swapdesk_files::{
    Engine, Event, FilesConfig, FsCaller, FsServer, Out, PipeFs, Roots, Side, SysVolume, SystemDisk, busy_result,
};
use swapdesk_ipc::{FsAnswer, FsCall};
use swapdesk_proto::{
    DirPage, ErrorCode, FsName, FsOp, FsOpKind, MAX_FRAME_PAYLOAD, Message, PlaceKind, VolumeKind, VolumesRequest,
};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-files-explorer-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct System {
    volumes: Mutex<Vec<SysVolume>>,
    bin: PathBuf,
}

impl SystemDisk for System {
    fn volumes(&self) -> Vec<SysVolume> {
        self.volumes.lock().unwrap().clone()
    }

    fn places(&self) -> Vec<(PlaceKind, PathBuf)> {
        Vec::new()
    }

    fn trash(&self, path: &Path) -> Result<(), ErrorCode> {
        std::fs::rename(path, self.bin.join(path.file_name().unwrap())).map_err(|_| ErrorCode::IoFailed)
    }

    fn protected(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

fn volume(identity: &str, root: &Path, kind: VolumeKind) -> SysVolume {
    SysVolume {
        identity: identity.to_string(),
        root: root.to_path_buf(),
        kind,
        label: identity.to_string(),
        letter: None,
        total_bytes: 100,
        free_bytes: 50,
        has_bin: true,
    }
}

/// The server called in process, as a pipe would carry it.
struct Direct(FsServer);

impl FsCaller for Direct {
    fn link_id(&self) -> u64 {
        1
    }

    fn call(&self, call: FsCall) -> Result<FsAnswer, ErrorCode> {
        self.0.handle(call).expect("a call with an answer")
    }

    fn send(&self, call: FsCall) {
        let _ = self.0.handle(call);
    }
}

struct Rig {
    root: PathBuf,
    home: PathBuf,
    sys: Arc<System>,
    host: Engine,
    viewer: Engine,
}

fn rig(tag: &str) -> Rig {
    let root = temp(tag);
    let (home, system_volume, stick) = (root.join("home"), root.join("sysvol"), root.join("stick"));
    for d in [&home, &system_volume, &stick, &root.join("bin")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let sys = Arc::new(System {
        volumes: Mutex::new(vec![
            volume("sys", &system_volume, VolumeKind::System),
            volume("stick", &stick, VolumeKind::Removable),
        ]),
        bin: root.join("bin"),
    });
    let server = FsServer::new(Roots::Home { home: home.clone(), receive: None })
        .with_system(Arc::clone(&sys) as Arc<dyn SystemDisk>);
    let direct: Arc<dyn FsCaller> = Arc::new(Direct(server));
    let fs = Arc::new(PipeFs::new(Arc::new(move || Some(Arc::clone(&direct)))));
    let host = Engine::with_fs(FilesConfig::new(Side::Host, PathBuf::new()), fs);
    let viewer = Engine::new(FilesConfig::new(Side::Viewer, PathBuf::new()));
    Rig { root, home, sys, host, viewer }
}

impl Rig {
    /// What the viewer's `outs` cause: the host's answers go back to the viewer. Returns the viewer's events and the
    /// host's, and the messages the host sent.
    fn run(&mut self, outs: Vec<Out>) -> (Vec<Event>, Vec<Event>, Vec<Message>) {
        let (mut viewer_events, mut host_events, mut sent) = (vec![], vec![], vec![]);
        let mut q: VecDeque<(bool, Out)> = outs.into_iter().map(|o| (true, o)).collect();
        while let Some((from_viewer, out)) = q.pop_front() {
            match out {
                Out::Msg(m) => {
                    let replies = if from_viewer {
                        self.host.on_message(m)
                    } else {
                        sent.push(m.clone());
                        self.viewer.on_message(m)
                    };
                    q.extend(replies.into_iter().map(|o| (!from_viewer, o)));
                }
                Out::Event(e) => {
                    if from_viewer {
                        viewer_events.push(e);
                    } else {
                        host_events.push(e);
                    }
                }
                Out::Stop { .. } | Out::Send { .. } => {}
            }
        }
        (viewer_events, host_events, sent)
    }
}

fn names(items: &[swapdesk_proto::DirEntry2]) -> Vec<&str> {
    items.iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn a_page_listing_splits_at_the_frame_limit_and_next_continues_it() {
    let mut r = rig("split");
    for i in 0..1500 {
        std::fs::write(r.home.join(format!("{i:04}-{}", "n".repeat(200))), b"").unwrap();
    }
    let mut seen: Vec<String> = Vec::new();
    let mut start = 0u32;
    let mut pages = 0;
    loop {
        let outs = r.viewer.request_page(7, String::new(), start, 2000);
        let (events, _, sent) = r.run(outs);
        let Some(Message::DirPageListing(l)) = sent.first() else { panic!("a page listing, got {sent:?}") };
        // Every page is one frame.
        let frame = Message::DirPageListing(l.clone()).encode_frame().expect("the page fits one frame");
        assert!(frame.len() <= MAX_FRAME_PAYLOAD + 8, "{} bytes", frame.len());
        assert_eq!(l.start, start);
        assert!(!l.entries.is_empty());
        let [Event::PageListing { entries, next, free_bytes, .. }] = events.as_slice() else {
            panic!("one event: {events:?}")
        };
        assert_eq!(entries.len(), l.entries.len());
        assert_eq!(*free_bytes, l.free_bytes);
        seen.extend(names(entries).into_iter().map(str::to_string));
        pages += 1;
        match next {
            Some(n) => {
                assert_eq!(*n, start + u32::try_from(entries.len()).unwrap(), "next starts where the page stopped");
                start = *n;
            }
            None => break,
        }
    }
    assert!(pages >= 2, "1500 entries of 200 bytes do not fit one frame: {pages} page(s)");
    assert_eq!(seen.len(), 1500, "every entry once");
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "in order, with no repeat");
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn a_page_of_a_volume_and_a_gone_volume() {
    let mut r = rig("volpage");
    std::fs::write(r.root.join("stick").join("on-stick.txt"), b"x").unwrap();
    let outs = r.viewer.request_page(1, ":2".to_string(), 0, 100);
    let (events, _, _) = r.run(outs);
    let [Event::PageListing { entries, next: None, free_bytes, .. }] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!(names(entries), ["on-stick.txt"]);
    assert_eq!(*free_bytes, Some(50), "the volume's own free space when the disk gives none");
    r.sys.volumes.lock().unwrap().clear();
    let outs = r.viewer.request_page(2, ":2".to_string(), 0, 100);
    let (events, _, sent) = r.run(outs);
    assert!(events.is_empty());
    let [Message::Error(e)] = sent.as_slice() else { panic!("an error: {sent:?}") };
    assert_eq!((e.code, e.about_type, e.about_id), (ErrorCode::VolumeGone, 0x040C, 2));
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn volumes_come_asked_or_pushed_and_only_when_expected() {
    let mut r = rig("volumes");
    let outs = r.viewer.request_volumes(5);
    let (events, _, sent) = r.run(outs);
    assert!(matches!(sent.as_slice(), [Message::VolumesList(l)] if l.req_id == 5 && l.volumes.len() == 2));
    let [Event::Volumes(list)] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!(list.home, "");
    assert_eq!(list.volumes[1].kind, VolumeKind::Removable);
    // The host's push has request number 0.
    let pushed = match r.host.volumes_push().into_iter().next() {
        Some(Out::Msg(Message::VolumesList(l))) => l,
        other => panic!("{other:?}"),
    };
    assert_eq!(pushed.req_id, 0);
    let got = r.viewer.on_message(Message::VolumesList(pushed));
    assert!(matches!(got.as_slice(), [Out::Event(Event::Volumes(l))] if l.req_id == 0));
    // An answer nobody asked for is dropped.
    let mut stray = match r.host.volumes_push().into_iter().next() {
        Some(Out::Msg(Message::VolumesList(l))) => l,
        other => panic!("{other:?}"),
    };
    stray.req_id = 99;
    assert!(r.viewer.on_message(Message::VolumesList(stray)).is_empty());
    // The viewer never serves them: a host-only request to a viewer is refused.
    let refused = r.viewer.on_message(Message::VolumesRequest(VolumesRequest { req_id: 3 }));
    assert!(matches!(refused.as_slice(), [Out::Msg(Message::Error(e))] if e.code == ErrorCode::NotPermitted));
    let refused = r.viewer.on_message(Message::DirPage(DirPage { req_id: 3, path: String::new(), start: 0, max: 5 }));
    assert!(matches!(refused.as_slice(), [Out::Msg(Message::Error(e))] if e.code == ErrorCode::NotPermitted));
    // And a host does not take the viewer's side of the exchange.
    assert!(r.host.request_volumes(1).is_empty() && r.host.request_page(1, String::new(), 0, 1).is_empty());
    let _ = std::fs::remove_dir_all(&r.root);
}

fn name(n: &str) -> FsName {
    FsName { name: n.to_string() }
}

#[test]
fn changes_run_one_result_per_name_and_are_audited_by_count() {
    let mut r = rig("ops");
    std::fs::write(r.home.join("a.txt"), b"a").unwrap();
    std::fs::write(r.home.join("b.txt"), b"b").unwrap();
    std::fs::write(r.home.join("c.txt"), b"c").unwrap();
    std::fs::create_dir(r.home.join("dir")).unwrap();
    std::fs::write(r.home.join("dir").join("b.txt"), b"in dir").unwrap();
    let op = |r: &mut Rig, id: u32, kind: FsOpKind| {
        let outs = r.viewer.request_op(id, kind);
        r.run(outs)
    };
    // New folder: one name, an audit entry for the home folder.
    let (events, host_events, _) = op(&mut r, 1, FsOpKind::MakeDir { parent: String::new(), name: "new".into() });
    let [Event::OpResult { op_id: 1, results }] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!((results[0].name.as_str(), results[0].code), ("new", ErrorCode::NoError));
    assert_eq!(host_events, [Event::Changed { op: 0, count: 1, area: 0 }]);
    // Rename onto a name that exists: refused, nothing changed, nothing audited.
    let (events, host_events, _) =
        op(&mut r, 2, FsOpKind::Rename { dir: String::new(), name: "a.txt".into(), new_name: "b.txt".into() });
    let [Event::OpResult { results, .. }] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!(results[0].code, ErrorCode::Exists);
    assert!(host_events.is_empty(), "a change that changed nothing is not counted: {host_events:?}");
    // Move of three names: one result each, in order; the one that collides says so and the others are done.
    let (events, host_events, _) = op(
        &mut r,
        3,
        FsOpKind::Move {
            from_dir: String::new(),
            names: vec![name("a.txt"), name("b.txt"), name("c.txt"), name("missing")],
            to_dir: "dir".into(),
        },
    );
    let [Event::OpResult { results, .. }] = events.as_slice() else { panic!("{events:?}") };
    let codes: Vec<_> = results.iter().map(|i| (i.name.as_str(), i.code)).collect();
    assert_eq!(
        codes,
        [
            ("a.txt", ErrorCode::NoError),
            ("b.txt", ErrorCode::Exists),
            ("c.txt", ErrorCode::NoError),
            ("missing", ErrorCode::NotFound)
        ]
    );
    assert_eq!(host_events, [Event::Changed { op: 2, count: 2, area: 0 }]);
    assert_eq!(std::fs::read(r.home.join("dir").join("b.txt")).unwrap(), b"in dir", "never replaced");
    assert!(r.home.join("b.txt").exists());
    // To the bin, on the removable volume: area 2, and the volume kind is reached (audit tag 20) once.
    std::fs::write(r.root.join("stick").join("old.txt"), b"o").unwrap();
    std::fs::write(r.root.join("stick").join("old2.txt"), b"o").unwrap();
    let (events, host_events, _) = op(&mut r, 4, FsOpKind::Trash { dir: ":2".into(), names: vec![name("old.txt")] });
    let [Event::OpResult { results, .. }] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!(results[0].code, ErrorCode::NoError);
    assert_eq!(host_events, [Event::Reach { kinds: 1 << 2 }, Event::Changed { op: 3, count: 1, area: 2 }]);
    assert!(r.root.join("bin").join("old.txt").exists() && !r.root.join("stick").join("old.txt").exists());
    let (_, host_events, _) = op(&mut r, 5, FsOpKind::Trash { dir: ":2".into(), names: vec![name("old2.txt")] });
    assert_eq!(host_events, [Event::Changed { op: 3, count: 1, area: 2 }], "the removable kind was reached already");
    // A listing on the system volume adds its own bit, once.
    let outs = r.viewer.request_page(8, ":1".to_string(), 0, 10);
    let (_, host_events, _) = r.run(outs);
    assert_eq!(host_events, [Event::Reach { kinds: (1 << 2) | 1 }]);
    let outs = r.viewer.request_page(9, ":1".to_string(), 0, 10);
    assert!(r.run(outs).1.is_empty());
    // An answer for an op the viewer never made is dropped.
    let stray = swapdesk_proto::FsOpResult { op_id: 77, results: vec![] };
    assert!(r.viewer.on_message(Message::FsOpResult(stray)).is_empty());
    let _ = std::fs::remove_dir_all(&r.root);
}

#[test]
fn a_viewer_engine_does_not_serve_changes_and_a_busy_answer_names_every_name() {
    let mut r = rig("busy");
    let op = FsOp { op_id: 4, op: FsOpKind::Trash { dir: String::new(), names: vec![name("x"), name("y")] } };
    let refused = r.viewer.on_message(Message::FsOp(op.clone()));
    assert!(matches!(refused.as_slice(), [Out::Msg(Message::Error(e))] if e.code == ErrorCode::NotPermitted));
    let Message::FsOpResult(res) = busy_result(&op) else { panic!("a result") };
    assert_eq!(res.results.iter().map(|i| i.code).collect::<Vec<_>>(), [ErrorCode::Busy, ErrorCode::Busy]);
    let _ = std::fs::remove_dir_all(&r.root);
}

// ---------- names a host sends (security review 13/11, item 1) ----------

fn hostile_names() -> Vec<&'static str> {
    vec![
        r"..\..\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup",
        r"C:\Users\x\Startup",
        r"\Windows\Temp",
        "../../Library/LaunchAgents",
        "..",
        ".",
        "",
        "C:",
        "nul",
        "a:b",
        "tail.",
    ]
}

fn entry2(name: &str, kind: swapdesk_proto::EntryKind) -> swapdesk_proto::DirEntry2 {
    swapdesk_proto::DirEntry2 { name: name.to_string(), kind, size: 1, mtime_ms: 0, flags: 0 }
}

#[test]
fn the_viewer_drops_every_page_entry_whose_name_is_not_one_valid_component() {
    let mut r = rig("hostile-page");
    let _ = r.viewer.request_page(9, String::new(), 0, 100);
    let mut entries: Vec<_> = hostile_names().into_iter().map(|n| entry2(n, swapdesk_proto::EntryKind::Dir)).collect();
    entries.push(entry2("Fine", swapdesk_proto::EntryKind::Dir));
    entries.push(entry2("ok.txt", swapdesk_proto::EntryKind::File));
    let outs = r.viewer.on_message(Message::DirPageListing(swapdesk_proto::DirPageListing {
        req_id: 9,
        path: String::new(),
        start: 0,
        entries,
        next: None,
        free_bytes: None,
    }));
    let [Out::Event(Event::PageListing { entries, .. })] = outs.as_slice() else { panic!("{outs:?}") };
    assert_eq!(names(entries), ["Fine", "ok.txt"]);
}

#[test]
fn the_viewer_drops_every_1_0_listing_entry_whose_name_is_not_one_valid_component() {
    let mut r = rig("hostile-list");
    let _ = r.viewer.request_dir(4, None);
    let mut entries: Vec<_> = hostile_names()
        .into_iter()
        .map(|n| swapdesk_proto::DirEntry { name: n.to_string(), is_dir: true, size: 0, mtime_ms: 0 })
        .collect();
    entries.push(swapdesk_proto::DirEntry { name: "Fine".into(), is_dir: true, size: 0, mtime_ms: 0 });
    let outs = r.viewer.on_message(Message::DirListing(swapdesk_proto::DirListing {
        req_id: 4,
        path: String::new(),
        entries,
        truncated: false,
    }));
    let [Out::Event(Event::Listing { entries, .. })] = outs.as_slice() else { panic!("{outs:?}") };
    let got: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(got, ["Fine"]);
}
