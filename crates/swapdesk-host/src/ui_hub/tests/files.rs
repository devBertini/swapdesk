//! The proof of stage 44 through two hubs on two cores (127.0.0.1, the development option): the host's disk is a user
//! helper over a pipe, the viewer's disk is its fake UI serving tokens over the UI pipe. The UI ops 40 to 45 (list,
//! send, get, pause, resume, cancel), the throttled progress notes, the "saved as" note of a file that did not
//! overwrite, and the transfers that go on by themselves after a cut link (F-FILE-01, F-FILE-02, F-NET-01).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use swapdesk_files::{DEFAULT_RECEIVE, FsServer, Roots, Tokens};
use swapdesk_ipc::{FileEntry, FileListing, FilePick, FilesNote, FilesOp, Text, TransferWire};

use super::*;

const FILES: PermissionSet = PermissionSet::FILES;
const CV: PermissionSet = PermissionSet::CONTROL;

const PAUSE_OP: u16 = 43;
const RESUME_OP: u16 = 44;
const CANCEL_OP: u16 = 45;

/// The id of the next session that goes active in `role` on this UI, other than the ones in `not`.
fn active_as(ui: &mut Client, role: Role, not: &[u64]) -> u64 {
    ui.notify("an active session", |n| match n {
        Notify::Session { session_id, state, .. }
            if !not.contains(session_id) && grants_active(state).is_some_and(|(r, _)| r == role) =>
        {
            Some(*session_id)
        }
        _ => None,
    })
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-hub-files-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn data(len: usize) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect()
}

/// Equal bytes, said without printing megabytes when they are not.
fn same(a: &[u8], b: &[u8]) -> bool {
    a == b
}

/// A host machine with a user helper that serves `home`, and a viewer machine whose UI serves `tokens`; they have
/// paired, a UI each at the console, and a session is open (the host's user accepted with files allowed).
struct Pair {
    host: Machine,
    viewer: Machine,
    host_ui: Client,
    viewer_ui: Client,
    session: u64,
    home: PathBuf,
    src: PathBuf,
    recv: PathBuf,
    tokens: Arc<Tokens>,
    relay: Option<Relay>,
}

impl Pair {
    fn new(tag: &str, cut_able: bool) -> Self {
        let (home, src, recv) =
            (temp(&format!("{tag}-home")), temp(&format!("{tag}-src")), temp(&format!("{tag}-recv")));
        let mut host = Machine::new("host-pc");
        let viewer = Machine::new("viewer-pc");
        host.pin(&viewer, ALL | FILES);
        viewer.pin(&host, CV);
        host.attach_helper(IpcRole::HelperSystem);
        host.attach_helper_with(IpcRole::HelperUser, |deps| {
            deps.with_fs(Arc::new(FsServer::new(Roots::Home {
                home: home.clone(),
                receive: Some(DEFAULT_RECEIVE.into()),
            })))
        });
        let relay = cut_able.then(|| Relay::start(host.session_addr()));
        match &relay {
            Some(r) => viewer.presence.set(vec![PresencePeer {
                device_id: host.id(),
                name: "host-pc".into(),
                os: Os::Windows,
                pairing_open: false,
                addrs: vec![Ipv4Addr::LOCALHOST],
                session_port: r.port,
                pair_port: 0,
            }]),
            None => viewer.sees(&host, false),
        }
        host.sees(&viewer, false);
        host.hub.set_console_session(Some(1));
        viewer.hub.set_console_session(Some(1));
        let tokens = Arc::new(Tokens::new());
        let host_ui = host.client(info(false));
        let mut viewer_ui = Client::connect_with_disk(
            &viewer.hub,
            info(false),
            Some(Arc::new(FsServer::new(Roots::Tokens(Arc::clone(&tokens))))),
        );
        let mut host_ui = host_ui;
        let req =
            viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
        let incoming = host_ui.notify("IncomingRequest", |n| match n {
            Notify::IncomingRequest(r) => Some(r.clone()),
            _ => None,
        });
        assert!(
            host_ui
                .call(UiOp::AnswerIncoming {
                    request_id: incoming.request_id,
                    decision: Decision::Accept,
                    grants: (ALL | FILES).bits()
                })
                .is_ok()
        );
        let session = viewer_ui.connected(req);
        active_as(&mut viewer_ui, Role::Viewer, &[]);
        active_as(&mut host_ui, Role::Host, &[]);
        Self { host, viewer, host_ui, viewer_ui, session, home, src, recv, tokens, relay }
    }

    /// A file the user picks in the UI: its `FilePick`.
    fn pick(&self, name: &str, content: &[u8]) -> FilePick {
        std::fs::write(self.src.join(name), content).unwrap();
        let (token, picked) = self.tokens.pick(self.src.join(name)).unwrap();
        FilePick { token, name: Text(picked) }
    }

    fn send(&mut self, picks: Vec<FilePick>, dir: Option<&str>) -> Result<ReplyBody, ApiCode> {
        let session_id = self.session;
        self.viewer_ui.call(UiOp::Files(FilesOp::Send { session_id, picks, remote_dir: dir.map(|d| Text(d.into())) }))
    }

    fn get(&mut self, path: &str) -> Result<ReplyBody, ApiCode> {
        let recv_token = self.tokens.receive_into(self.recv.clone());
        let session_id = self.session;
        self.viewer_ui.call(UiOp::Files(FilesOp::Get { session_id, remote_path: Text(path.into()), recv_token }))
    }

    fn finish(self) {
        let Pair { host, viewer, host_ui, viewer_ui, home, src, recv, relay, .. } = self;
        drop((host_ui, viewer_ui, relay));
        viewer.finish();
        host.finish();
        for d in [&home, &src, &recv] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn state_of(ui: &mut Client, id: u32, what: &str, f: impl Fn(&TransferWire) -> bool) -> TransferWire {
    ui.notify(what, |n| match n {
        Notify::Files(FilesNote::Transfer { transfer_id, state, .. }) if *transfer_id == id && f(state) => Some(*state),
        _ => None,
    })
}

fn is_done(s: &TransferWire) -> bool {
    matches!(s, TransferWire::Done)
}

fn one_id(r: Result<ReplyBody, ApiCode>) -> u32 {
    match r.expect("a transfer number") {
        ReplyBody::TransferId(id) => id,
        ReplyBody::TransferIds(ids) => {
            assert_eq!(ids.len(), 1);
            ids[0]
        }
        other => panic!("not a transfer number: {other:?}"),
    }
}

fn listing(r: Result<ReplyBody, ApiCode>) -> FileListing {
    match r.expect("a listing") {
        ReplyBody::Listing(l) => l,
        other => panic!("not a listing: {other:?}"),
    }
}

fn names(entries: &[FileEntry]) -> Vec<(String, bool)> {
    entries.iter().map(|e| (e.name.0.clone(), e.is_dir)).collect()
}

#[test]
fn the_file_ops_browse_send_and_get_with_throttled_progress_and_the_saved_as_note() {
    let mut p = Pair::new("ops", false);
    std::fs::create_dir_all(p.home.join("docs")).unwrap();
    std::fs::create_dir_all(p.home.join("incoming")).unwrap();
    let down = data(2_000_000);
    std::fs::write(p.home.join("docs").join("down.bin"), &down).unwrap();
    let session = p.session;

    // F-FILE-02: the host's home, then a folder of it; a folder outside the root is never shown.
    let root = listing(p.viewer_ui.call(UiOp::Files(FilesOp::List { session_id: session, path: None })));
    assert_eq!((root.path.0.as_str(), root.truncated), ("", false));
    assert_eq!(names(&root.entries), [("docs".to_string(), true), ("incoming".to_string(), true)]);
    let docs =
        listing(p.viewer_ui.call(UiOp::Files(FilesOp::List { session_id: session, path: Some(Text("docs".into())) })));
    assert_eq!(
        (docs.path.0.as_str(), docs.entries[0].name.0.as_str(), docs.entries[0].size),
        ("docs", "down.bin", 2_000_000)
    );

    // F-FILE-01 upload: queued, running (at most 4 notes a second), then done; the host's user's UI hears it too.
    let up = data(6_000_000);
    let pick = p.pick("up.bin", &up);
    let id = one_id(p.send(vec![pick.clone()], Some("incoming")));
    state_of(&mut p.viewer_ui, id, "queued", |s| matches!(s, TransferWire::Queued));
    let started = Instant::now();
    let mut running = 0;
    loop {
        let s = state_of(&mut p.viewer_ui, id, "progress or done", |_| true);
        match s {
            TransferWire::Running { total, .. } => {
                assert_eq!(total, 6_000_000);
                running += 1;
            }
            TransferWire::Done => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    let secs = started.elapsed().as_secs_f64().max(0.25);
    assert!(f64::from(running) <= 4.0 * secs + 2.0, "{running} progress notes in {secs:.2} s: throttled to 4 a second");
    assert!(same(&std::fs::read(p.home.join("incoming").join("up.bin")).unwrap(), &up));
    state_of(&mut p.host_ui, id, "the host's user hears of the file", is_done);

    // The name is taken: the host saves "up (1).bin" and its user's UI is told which name it got.
    let id2 = one_id(p.send(vec![pick], Some("incoming")));
    state_of(&mut p.viewer_ui, id2, "the second upload", is_done);
    assert!(std::fs::metadata(p.home.join("incoming").join("up (1).bin")).is_ok(), "the first file stays");
    let saved = p.host_ui.notify("saved as", |n| match n {
        Notify::Files(FilesNote::SavedAs { transfer_id, name, .. }) if *transfer_id == id2 => Some(name.0.clone()),
        _ => None,
    });
    assert_eq!(saved, "up (1).bin");

    // F-FILE-02 download into the receive folder the UI chose; the second time the name is taken on this side.
    let g1 = one_id(p.get("docs/down.bin"));
    state_of(&mut p.viewer_ui, g1, "the download", is_done);
    assert!(same(&std::fs::read(p.recv.join("down.bin")).unwrap(), &down));
    let g2 = one_id(p.get("docs/down.bin"));
    let saved = p.viewer_ui.notify("saved as", |n| match n {
        Notify::Files(FilesNote::SavedAs { transfer_id, name, .. }) if *transfer_id == g2 => Some(name.0.clone()),
        _ => None,
    });
    assert_eq!(saved, "down (1).bin");
    state_of(&mut p.viewer_ui, g2, "the second download", is_done);
    assert!(same(&std::fs::read(p.recv.join("down (1).bin")).unwrap(), &down));

    // Refusals: nothing reaches the host for a request that is not one of this UI's own.
    let other = p.viewer.client(info(false));
    let mut other = other;
    assert_eq!(
        other.call(UiOp::Files(FilesOp::List { session_id: session, path: None })).unwrap_err(),
        ApiCode::NotFound,
        "another UI's session"
    );
    assert_eq!(
        p.viewer_ui.call(UiOp::Files(FilesOp::List { session_id: session + 1, path: None })).unwrap_err(),
        ApiCode::NotFound
    );
    let bad = FilePick { token: 1, name: Text("a/b".into()) };
    assert_eq!(p.send(vec![bad], None).unwrap_err(), ApiCode::Invalid, "a name with a separator");
    assert_eq!(p.send(Vec::new(), None).unwrap_err(), ApiCode::Invalid);
    let nine: Vec<FilePick> = (0..9).map(|i| FilePick { token: 100 + i, name: Text(format!("f{i}")) }).collect();
    assert_eq!(p.send(nine, None).unwrap_err(), ApiCode::Busy, "at most 8 transfers of a session at once");
    for op in [PAUSE_OP, RESUME_OP, CANCEL_OP] {
        let f = match op {
            PAUSE_OP => FilesOp::Pause { transfer_id: 9999 },
            RESUME_OP => FilesOp::Resume { transfer_id: 9999 },
            _ => FilesOp::Cancel { transfer_id: 9999 },
        };
        assert_eq!(p.viewer_ui.call(UiOp::Files(f)).unwrap_err(), ApiCode::NotFound);
    }
    // A pick the UI never made is refused by the UI's own disk: the transfer fails and nothing is sent.
    let ghost = FilePick { token: 9000, name: Text("ghost.bin".into()) };
    let gid = one_id(p.send(vec![ghost], None));
    state_of(&mut p.viewer_ui, gid, "the ghost to fail", |s| matches!(s, TransferWire::Failed { .. }));
    drop(other);
    p.finish();
}

#[test]
fn a_transfer_the_user_pauses_waits_for_retomar_and_continues_from_the_verified_length() {
    let mut p = Pair::new("pause", false);
    std::fs::create_dir_all(p.home.join("incoming")).unwrap();
    let big = data(160 * 1024 * 1024);
    let pick = p.pick("big.bin", &big);
    let id = one_id(p.send(vec![pick], Some("incoming")));
    // Paused at once: the file moves at about 100 MB/s, so it is far from done.
    assert!(p.viewer_ui.call(UiOp::Files(FilesOp::Pause { transfer_id: id })).is_ok());
    state_of(&mut p.viewer_ui, id, "paused", |s| matches!(s, TransferWire::Paused { .. }));
    // "Retomar".
    assert!(p.viewer_ui.call(UiOp::Files(FilesOp::Resume { transfer_id: id })).is_ok());
    state_of(&mut p.viewer_ui, id, "done after Retomar", is_done);
    assert!(same(&std::fs::read(p.home.join("incoming").join("big.bin")).unwrap(), &big));
    assert!(!p.home.join("incoming").join("big.bin.swapdesk-part").exists());
    // Cancel removes a transfer and its partial file.
    let pick = p.pick("cancel.bin", &big);
    let cid = one_id(p.send(vec![pick], Some("incoming")));
    state_of(&mut p.viewer_ui, cid, "bytes moving", |s| matches!(s, TransferWire::Running { .. }));
    assert!(p.viewer_ui.call(UiOp::Files(FilesOp::Cancel { transfer_id: cid })).is_ok());
    wait_for("the partial file to go", || {
        (!p.home.join("incoming").join("cancel.bin.swapdesk-part").exists()).then_some(())
    });
    assert!(!p.home.join("incoming").join("cancel.bin").exists());
    p.finish();
}

// ---------- F-NET-01 for files: the link is cut, the unfinished transfer goes on by itself ----------

/// A UDP relay between the viewer and the host's endpoint that can be cut (the same shape as the one of the swap and
/// chat proofs): while it is, nothing crosses either way.
struct Relay {
    port: u16,
    cut: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Relay {
    fn start(target: SocketAddr) -> Self {
        let front = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("relay socket");
        front.set_read_timeout(Some(Duration::from_millis(50))).expect("timeout");
        let port = front.local_addr().expect("addr").port();
        let (cut, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (cut2, stop2) = (Arc::clone(&cut), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut upstream: HashMap<SocketAddr, UdpSocket> = HashMap::new();
            let mut buf = vec![0u8; 65_536];
            while !stop2.load(Ordering::SeqCst) {
                let Ok((n, client)) = front.recv_from(&mut buf) else { continue };
                if cut2.load(Ordering::SeqCst) {
                    continue;
                }
                let up = upstream.entry(client).or_insert_with(|| {
                    let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("upstream socket");
                    s.connect(target).expect("connect");
                    s.set_read_timeout(Some(Duration::from_millis(50))).expect("timeout");
                    let (back, front, cut, stop) = (
                        s.try_clone().expect("clone"),
                        front.try_clone().expect("clone"),
                        Arc::clone(&cut2),
                        Arc::clone(&stop2),
                    );
                    std::thread::spawn(move || {
                        let mut b = vec![0u8; 65_536];
                        while !stop.load(Ordering::SeqCst) {
                            if let Ok(n) = back.recv(&mut b)
                                && !cut.load(Ordering::SeqCst)
                            {
                                let _ = front.send_to(&b[..n], client);
                            }
                        }
                    });
                    s
                });
                let _ = up.send(&buf[..n]);
            }
        });
        Self { port, cut, stop }
    }

    fn cut(&self, on: bool) {
        self.cut.store(on, Ordering::SeqCst);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_cut_link_pauses_the_transfers_and_the_reconnect_offers_them_again_with_no_click() {
    let mut p = Pair::new("cut", true);
    std::fs::create_dir_all(p.home.join("incoming")).unwrap();
    std::fs::write(p.home.join("down.bin"), data(160 * 1024 * 1024)).unwrap();
    let up = data(160 * 1024 * 1024);
    let pick = p.pick("up.bin", &up);
    let up_id = one_id(p.send(vec![pick], Some("incoming")));
    let down_id = one_id(p.get("down.bin"));
    state_of(
        &mut p.viewer_ui,
        up_id,
        "the upload moving",
        |s| matches!(s, TransferWire::Running { done, .. } if *done > 0),
    );
    state_of(
        &mut p.viewer_ui,
        down_id,
        "the download moving",
        |s| matches!(s, TransferWire::Running { done, .. } if *done > 0),
    );

    // The Wi-Fi drops: both transfers pause (the link broke), the session says "Reconectando".
    let relay = p.relay.take().expect("a cut-able pair");
    relay.cut(true);
    for id in [up_id, down_id] {
        state_of(&mut p.viewer_ui, id, "paused by the lost link", |s| matches!(s, TransferWire::Paused { .. }));
    }
    let partial = std::fs::metadata(p.home.join("incoming").join("up.bin.swapdesk-part"))
        .expect("the host keeps the partial file")
        .len();
    assert!(partial < up.len() as u64);
    p.viewer_ui.notify("Reconectando", |n| match n {
        Notify::Session { session_id, state: SessionState::Reconnecting { attempt }, .. }
            if *session_id == p.session && *attempt >= 1 =>
        {
            Some(())
        }
        _ => None,
    });

    // The network is back: nobody clicks "Retomar", and both finish with the right bytes.
    relay.cut(false);
    state_of(&mut p.viewer_ui, up_id, "the upload to finish", is_done);
    state_of(&mut p.viewer_ui, down_id, "the download to finish", is_done);
    assert!(same(&std::fs::read(p.home.join("incoming").join("up.bin")).unwrap(), &up));
    assert!(same(&std::fs::read(p.recv.join("down.bin")).unwrap(), &std::fs::read(p.home.join("down.bin")).unwrap()));
    assert!(
        !p.home.join("incoming").join("up.bin.swapdesk-part").exists()
            && !p.recv.join("down.bin.swapdesk-part").exists()
    );
    p.relay = Some(relay);
    p.finish();
}

#[test]
fn a_transfer_the_user_paused_stays_paused_through_a_reconnect() {
    let mut p = Pair::new("cutpaused", true);
    std::fs::create_dir_all(p.home.join("incoming")).unwrap();
    let up = data(160 * 1024 * 1024);
    let pick = p.pick("up.bin", &up);
    let id = one_id(p.send(vec![pick], Some("incoming")));
    assert!(p.viewer_ui.call(UiOp::Files(FilesOp::Pause { transfer_id: id })).is_ok());
    state_of(&mut p.viewer_ui, id, "paused", |s| matches!(s, TransferWire::Paused { .. }));
    let relay = p.relay.take().expect("a cut-able pair");
    relay.cut(true);
    p.viewer_ui.notify("Reconectando", |n| {
        matches!(n, Notify::Session { state: SessionState::Reconnecting { attempt }, .. } if *attempt >= 1)
            .then_some(())
    });
    relay.cut(false);
    p.viewer_ui.notify("the session is back", |n| match n {
        Notify::Session { session_id, state, .. } if *session_id == p.session => grants_active(state),
        _ => None,
    });
    assert!(
        p.viewer_ui.nothing_within(Duration::from_secs(2), |m| matches!(
            m,
            Ipc::UiNotify(Notify::Files(FilesNote::Transfer { state: TransferWire::Running { .. }, .. }))
        )),
        "a paused transfer is not started again"
    );
    // "Retomar" after the reconnect offers it again and it finishes.
    assert!(p.viewer_ui.call(UiOp::Files(FilesOp::Resume { transfer_id: id })).is_ok());
    state_of(&mut p.viewer_ui, id, "done after Retomar", is_done);
    assert!(same(&std::fs::read(p.home.join("incoming").join("up.bin")).unwrap(), &up));
    p.relay = Some(relay);
    p.finish();
}
