//! Files through the proxies (stage 44, F-FILE-01 and F-FILE-02): two `ServiceCore`s on 127.0.0.1. The host's disk is a
//! user helper (`run_helper` on its own thread over a real pipe) serving a temporary home; the viewer's disk is the UI's
//! token server. The service touches no path itself: every transfer below goes through `PipeFs`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use swapdesk_files::{
    Command, DEFAULT_RECEIVE, DirRead, Event, Fs, FsCaller, FsServer, FsStat, Kind, LocalFs, Moved, ReadHandle, Roots,
    Tokens,
};
use swapdesk_ipc::{FsAnswer, FsCall};
use swapdesk_swap::{AskEnd, SwapPrompt};
use swapdesk_trust::UserRef;

use super::*;
use crate::service::ExtrasHooks;

const FILES: PermissionSet = PermissionSet::FILES;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-host-files-{tag}-{}", std::process::id()));
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

/// A machine's interface for the files feature: it keeps the events, may close the link when a transfer passes a byte
/// count, and says who is at the console.
#[derive(Default)]
struct Hooks {
    events: Mutex<Vec<(u64, Event)>>,
    console: Mutex<Option<String>>,
    /// When a transfer passes this many bytes, the link goes dark (see [`Dark`]).
    kill: Mutex<Option<(Arc<AtomicBool>, u64)>>,
}

impl Hooks {
    fn has(&self, f: impl Fn(&Event) -> bool) -> bool {
        lock(&self.events).iter().any(|(_, e)| f(e))
    }

    fn wait(&self, what: &str, f: impl Fn(&Event) -> bool) {
        wait_for(what, || self.has(&f).then_some(()));
    }

    fn find<T>(&self, f: impl Fn(&Event) -> Option<T>) -> Option<T> {
        lock(&self.events).iter().find_map(|(_, e)| f(e))
    }
}

impl ExtrasHooks for Hooks {
    fn swap_ask(&self, _prompt: SwapPrompt) -> oneshot::Receiver<bool> {
        oneshot::channel().1
    }

    fn swap_ask_closed(&self, _request_id: u32, _how: AskEnd) {}

    fn address_of(&self, _device: &DeviceId) -> Option<SocketAddr> {
        None
    }

    fn swap_viewing(&self, _device: DeviceId, _opened: Result<(ViewerHandle, ViewerMessages), ConnectError>) {}

    fn chat(&self, _session_id: u64, _note: crate::service::ChatNote) {}

    fn files(&self, session_id: u64, event: Event) {
        if let Event::Progress { done, .. } = &event
            && let Some((dark, at)) = lock(&self.kill).as_ref()
            && done >= at
        {
            dark.store(true, Ordering::SeqCst);
        }
        lock(&self.events).push((session_id, event));
    }

    fn console_user(&self) -> Option<String> {
        lock(&self.console).clone()
    }
}

/// A UDP relay on loopback between the viewer and the host that can be told to drop everything: the link "dies" with
/// no close frame, so the viewer's session ends as `Lost` (a local or peer close would not be resumed) and the
/// service's reconnect takes over when the relay passes packets again.
struct Dark {
    addr: SocketAddr,
    dark: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Dark {
    fn start(host: SocketAddr) -> Dark {
        let front = std::net::UdpSocket::bind("127.0.0.1:0").expect("front");
        let back = std::net::UdpSocket::bind("127.0.0.1:0").expect("back");
        for s in [&front, &back] {
            s.set_read_timeout(Some(Duration::from_millis(50))).expect("timeout");
        }
        let addr = front.local_addr().expect("addr");
        let (dark, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
        {
            let (front, back, dark, stop, client) = (
                front.try_clone().expect("clone"),
                back.try_clone().expect("clone"),
                Arc::clone(&dark),
                Arc::clone(&stop),
                Arc::clone(&client),
            );
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65_536];
                while !stop.load(Ordering::SeqCst) {
                    if let Ok((n, from)) = front.recv_from(&mut buf) {
                        *lock(&client) = Some(from);
                        if !dark.load(Ordering::SeqCst) {
                            let _ = back.send_to(&buf[..n], host);
                        }
                    }
                }
            });
        }
        {
            let (dark, stop) = (Arc::clone(&dark), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65_536];
                while !stop.load(Ordering::SeqCst) {
                    if let Ok((n, _)) = back.recv_from(&mut buf)
                        && !dark.load(Ordering::SeqCst)
                        && let Some(to) = *lock(&client)
                    {
                        let _ = front.send_to(&buf[..n], to);
                    }
                }
            });
        }
        Dark { addr, dark, stop }
    }
}

impl Drop for Dark {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// The UI's disk, called directly (the hub's tests put it behind the UI pipe).
struct Direct {
    server: FsServer,
    link: u64,
}

impl FsCaller for Direct {
    fn link_id(&self) -> u64 {
        self.link
    }

    fn call(&self, call: FsCall) -> Result<FsAnswer, ErrorCode> {
        self.server.handle(call).unwrap_or(Ok(FsAnswer::Done))
    }

    fn send(&self, call: FsCall) {
        let _ = self.server.handle(call);
    }
}

struct Rig {
    host: Machine,
    viewer: Machine,
    host_hooks: Arc<Hooks>,
    viewer_hooks: Arc<Hooks>,
    user: TestHelper,
    home: PathBuf,
    src: PathBuf,
    recv: PathBuf,
    tokens: Arc<Tokens>,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let host = Machine::with_grants("host-pc", true, false, ALL | FILES);
        let viewer = Machine::new("viewer-pc", true, false);
        Machine::pair(&host, &viewer, ALL | FILES | PermissionSet::UNATTENDED);
        host.trust
            .update(|s| {
                s.devices
                    .iter_mut()
                    .for_each(|d| d.unattended_granted_by = Some(UserRef { id: "S-1".into(), name: "admin".into() }))
            })
            .expect("store");
        let (host_hooks, viewer_hooks) = (Arc::new(Hooks::default()), Arc::new(Hooks::default()));
        assert!(host.core.set_extras(host_hooks.clone()));
        assert!(viewer.core.set_extras(viewer_hooks.clone()));
        let (home, src, recv) =
            (temp(&format!("{tag}-home")), temp(&format!("{tag}-src")), temp(&format!("{tag}-recv")));
        let server = Arc::new(FsServer::new(Roots::Home { home: home.clone(), receive: Some(DEFAULT_RECEIVE.into()) }));
        let user = TestHelper::spawn_with(&host.core, IpcRole::HelperUser, move |deps| deps.with_fs(server));
        Self { host, viewer, host_hooks, viewer_hooks, user, home, src, recv, tokens: Arc::new(Tokens::new()) }
    }

    /// Opens the session and gives it the UI's disk.
    fn connect(&self, unattended: bool) -> (ViewerHandle, Seen) {
        self.connect_to(self.host.addr(), unattended)
    }

    fn connect_to(&self, addr: SocketAddr, unattended: bool) -> (ViewerHandle, Seen) {
        let (handle, msgs) = self
            .viewer
            .core
            .connect_with(addr, self.host.device(), Intent::Control, unattended)
            .expect("the session opens");
        let ui_disk = Direct { server: FsServer::new(Roots::Tokens(Arc::clone(&self.tokens))), link: 1 };
        handle.files().bind(Arc::new(ui_disk));
        assert!(handle.granted().contains(FILES), "the host's user allowed files: {:?}", handle.granted());
        (handle, Seen::new(msgs))
    }

    fn finish(mut self, handle: ViewerHandle) {
        drop(handle);
        self.host.core.shutdown();
        self.viewer.core.shutdown();
        self.user.join();
        for d in [&self.home, &self.src, &self.recv] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn run(handle: &ViewerHandle, cmd: Command) {
    wait_for("the session's files to be live", || handle.files().command(cmd.clone()).then_some(()));
}

fn pick(rig: &Rig, name: &str, content: &[u8]) -> String {
    std::fs::write(rig.src.join(name), content).unwrap();
    let (token, picked) = rig.tokens.pick(rig.src.join(name)).unwrap();
    assert_eq!(picked, name);
    format!("p{token}/{name}")
}

#[test]
fn files_travel_both_ways_through_the_proxies_and_never_overwrite() {
    let rig = Rig::new("both");
    std::fs::create_dir_all(rig.home.join("docs")).unwrap();
    std::fs::create_dir_all(rig.home.join("incoming")).unwrap();
    let down = data(3_000_000);
    std::fs::write(rig.home.join("docs").join("down.bin"), &down).unwrap();
    let up = data(2_100_000);
    let up_path = pick(&rig, "up.bin", &up);
    let recv_token = rig.tokens.receive_into(rig.recv.clone());
    let (handle, _seen) = rig.connect(false);
    let vh = &rig.viewer_hooks;

    // F-FILE-02: the host's folders, as the host user's helper lists them (the root, then a folder).
    run(&handle, Command::Dir { req_id: 1, path: None });
    vh.wait("the root listing", |e| matches!(e, Event::Listing { req_id: 1, .. }));
    let Some(Event::Listing { path, entries, .. }) =
        vh.find(|e| matches!(e, Event::Listing { req_id: 1, .. }).then(|| e.clone()))
    else {
        unreachable!()
    };
    assert_eq!(path, "");
    let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
    assert_eq!(names, [("docs", true), ("incoming", true)], "folders only, in the helper's home");
    run(&handle, Command::Dir { req_id: 2, path: Some("docs".into()) });
    vh.wait("the docs listing", |e| matches!(e, Event::Listing { req_id: 2, .. }));
    let Some(Event::Listing { entries, .. }) =
        vh.find(|e| matches!(e, Event::Listing { req_id: 2, .. }).then(|| e.clone()))
    else {
        unreachable!()
    };
    assert_eq!((entries[0].name.as_str(), entries[0].size), ("down.bin", down.len() as u64));

    // F-FILE-01 upload into a folder of the host, then again: the name is taken, so it is saved as "up (2).bin".
    run(&handle, Command::SendFile { id: 10, path: PathBuf::from(&up_path), dest: Some("incoming".into()) });
    vh.wait("the sender to hear it verified", |e| matches!(e, Event::Sent { id: 10 }));
    assert!(same(&std::fs::read(rig.home.join("incoming").join("up.bin")).unwrap(), &up));
    run(&handle, Command::SendFile { id: 11, path: PathBuf::from(&up_path), dest: Some("incoming".into()) });
    vh.wait("the second upload", |e| matches!(e, Event::Sent { id: 11 }));
    assert!(same(&std::fs::read(rig.home.join("incoming").join("up (2).bin")).unwrap(), &up));
    assert!(
        same(&std::fs::read(rig.home.join("incoming").join("up.bin")).unwrap(), &up),
        "the first file is untouched"
    );
    let saved: Vec<PathBuf> = lock(&rig.host_hooks.events)
        .iter()
        .filter_map(|(_, e)| if let Event::Received { path, .. } = e { Some(path.clone()) } else { None })
        .collect();
    assert_eq!(
        saved,
        [Path::new("incoming").join("up.bin"), Path::new("incoming").join("up (2).bin")],
        "the host's user is told where each went"
    );

    // No folder named: the host's default receive folder, made by the helper on first use.
    run(&handle, Command::SendFile { id: 12, path: PathBuf::from(&up_path), dest: None });
    vh.wait("the default-folder upload", |e| matches!(e, Event::Sent { id: 12 }));
    assert!(same(&std::fs::read(rig.home.join("Downloads").join("Swapdesk").join("up.bin")).unwrap(), &up));

    // A download into the folder the UI chose (by token).
    run(
        &handle,
        Command::Get { id: 20, path: "docs/down.bin".into(), save_dir: PathBuf::from(format!("r{recv_token}")) },
    );
    vh.wait("the download", |e| matches!(e, Event::Received { id: 20, .. }));
    assert!(same(&std::fs::read(rig.recv.join("down.bin")).unwrap(), &down), "the downloaded file differs");
    assert!(
        std::fs::read_dir(&rig.recv).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("swapdesk-")),
        "no part or meta file is left"
    );

    // Paths outside the host's root are refused by the helper: nothing arrives and nothing leaks.
    run(&handle, Command::Dir { req_id: 3, path: Some("../..".into()) });
    run(
        &handle,
        Command::Get { id: 21, path: "../outside.txt".into(), save_dir: PathBuf::from(format!("r{recv_token}")) },
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(!vh.has(|e| matches!(e, Event::Listing { req_id: 3, .. } | Event::Incoming { id: 21, .. })));
    rig.finish(handle);
}

#[test]
fn a_transfer_resumes_after_the_link_is_killed_through_the_proxies() {
    const SIZE: usize = 16 * 1024 * 1024;
    const KILL_AT: u64 = 6 * 1024 * 1024;
    let rig = Rig::new("resume");
    std::fs::create_dir_all(rig.home.join("incoming")).unwrap();
    let content = data(SIZE);
    let path = pick(&rig, "big.bin", &content);
    let relay = Dark::start(rig.host.addr());
    let (handle, mut seen) = rig.connect_to(relay.addr, false);
    *lock(&rig.viewer_hooks.kill) = Some((Arc::clone(&relay.dark), KILL_AT));
    run(&handle, Command::SendFile { id: 1, path: PathBuf::from(&path), dest: Some("incoming".into()) });

    // The link goes dark mid-way for longer than the liveness window (8 s): the session is lost, the service
    // reconnects by itself (F-NET-01) once packets pass again, and the host keeps the verified bytes.
    wait_for("the link to go dark", || relay.dark.load(Ordering::SeqCst).then_some(()));
    std::thread::sleep(Duration::from_secs(10));
    relay.dark.store(false, Ordering::SeqCst);
    seen.next("Reconnecting", |o| {
        matches!(o, ViewerOut::State(c) if matches!(c.state, SessionState::Reconnecting { .. })).then_some(())
    });
    seen.next("Active again", |o| {
        matches!(o, ViewerOut::State(c) if matches!(c.state, SessionState::Active { .. })).then_some(())
    });
    assert!(!rig.host_hooks.has(|e| matches!(e, Event::Received { .. })), "the file had not arrived whole");
    let part = std::fs::metadata(rig.home.join("incoming").join("big.bin.swapdesk-part"))
        .expect("the partial file is kept")
        .len();
    assert!(part > 0 && part < SIZE as u64, "partial length {part}");

    // Offered again over the new connection: the helper's disk finds the partial file and the host continues.
    *lock(&rig.viewer_hooks.kill) = None;
    wait_for("the old engine to be gone", || (!handle.files().knows(1)).then_some(()));
    run(&handle, Command::SendFile { id: 1, path: PathBuf::from(&path), dest: Some("incoming".into()) });
    rig.host_hooks.wait("the host to verify the file", |e| matches!(e, Event::Received { .. }));
    rig.viewer_hooks.wait("the sender to hear it verified", |e| matches!(e, Event::Sent { id: 1 }));
    let resume_from =
        rig.host_hooks.find(|e| if let Event::Incoming { resume_from, .. } = e { Some(*resume_from) } else { None });
    let last_resume = lock(&rig.host_hooks.events)
        .iter()
        .rev()
        .find_map(|(_, e)| if let Event::Incoming { resume_from, .. } = e { Some(*resume_from) } else { None });
    assert!(resume_from.is_some());
    let last_resume = last_resume.unwrap();
    assert!(last_resume > 0 && last_resume <= part, "resumed from {last_resume}, the partial file was {part}");
    assert!(
        same(&std::fs::read(rig.home.join("incoming").join("big.bin")).unwrap(), &content),
        "the resumed file differs"
    );
    assert!(
        !rig.home.join("incoming").join("big.bin.swapdesk-part").exists()
            && !rig.home.join("incoming").join("big.bin.swapdesk-meta").exists()
    );
    rig.finish(handle);
}

/// A transfer in an unattended session, with `console` at the machine and the host's user answering `answer`.
/// Returns what the sender heard and how many questions the user was asked.
fn unattended_upload(tag: &str, console: Option<&str>, answer: Option<bool>) -> (Vec<Event>, usize) {
    let rig = Rig::new(tag);
    std::fs::create_dir_all(rig.home.join("incoming")).unwrap();
    *lock(&rig.host_hooks.console) = console.map(str::to_string);
    *lock(&rig.host.ui.consent_answer) = answer;
    let path = pick(&rig, "a.bin", b"hello");
    let (handle, _seen) = rig.connect(true);
    assert_eq!(rig.host.ui.prompts.load(Ordering::SeqCst), 0, "entered without a prompt");
    run(&handle, Command::SendFile { id: 1, path: PathBuf::from(&path), dest: Some("incoming".into()) });
    wait_for("the transfer to end", || {
        rig.viewer_hooks.has(|e| matches!(e, Event::Sent { .. } | Event::Failed { .. })).then_some(())
    });
    let first = lock(&rig.viewer_hooks.events).iter().map(|(_, e)| e.clone()).collect::<Vec<_>>();
    // A second request in the same session.
    run(&handle, Command::SendFile { id: 2, path: PathBuf::from(&path), dest: Some("incoming".into()) });
    wait_for("the second transfer to end", || {
        rig.viewer_hooks.has(|e| matches!(e, Event::Sent { id: 2 } | Event::Failed { id: 2, .. })).then_some(())
    });
    let all = lock(&rig.viewer_hooks.events).iter().map(|(_, e)| e.clone()).collect::<Vec<_>>();
    assert!(all.len() > first.len());
    let asked = rig.host.ui.consents.load(Ordering::SeqCst);
    rig.finish(handle);
    (all, asked)
}

#[test]
fn d13_the_granter_at_the_console_is_not_asked() {
    let (events, asked) = unattended_upload("d13-same", Some("S-1"), None);
    assert_eq!(asked, 0);
    assert!(
        events.iter().any(|e| matches!(e, Event::Sent { id: 1 }))
            && events.iter().any(|e| matches!(e, Event::Sent { id: 2 }))
    );
}

#[test]
fn d13_another_user_at_the_console_is_asked_and_a_yes_lets_the_transfer_through() {
    let (events, asked) = unattended_upload("d13-yes", Some("S-2"), Some(true));
    assert_eq!(asked, 1, "asked once for the session, not for each request");
    assert!(
        events.iter().any(|e| matches!(e, Event::Sent { id: 1 }))
            && events.iter().any(|e| matches!(e, Event::Sent { id: 2 }))
    );
}

#[test]
fn d13_a_no_refuses_every_transfer_of_the_session_and_nobody_present_refuses_without_asking() {
    let (events, asked) = unattended_upload("d13-no", Some("S-2"), Some(false));
    assert_eq!(asked, 1, "a no is final for the session");
    for id in [1, 2] {
        assert!(
            events.iter().any(|e| matches!(e, Event::Failed { id: i, code: ErrorCode::Declined } if *i == id)),
            "{events:?}"
        );
    }
    let (events, asked) = unattended_upload("d13-nobody", None, Some(true));
    assert_eq!(asked, 0, "nobody at the console: refused at once");
    assert!(events.iter().any(|e| matches!(e, Event::Failed { id: 1, code: ErrorCode::NobodyPresent })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Sent { .. })));
}

/// A disk whose `move_into` waits for the test to let it finish (a copy across volumes that takes a long time); every
/// other call goes to the real disk.
struct SlowMove {
    inner: LocalFs,
    gate: Mutex<std::sync::mpsc::Receiver<()>>,
    entered: AtomicBool,
}

impl Fs for SlowMove {
    fn list_dir(&self, path: &Path, max_entries: u32) -> Result<DirRead, ErrorCode> {
        self.inner.list_dir(path, max_entries)
    }
    fn stat(&self, path: &Path, follow: bool) -> Result<Option<FsStat>, ErrorCode> {
        self.inner.stat(path, follow)
    }
    fn open_read(&self, path: &Path) -> Result<ReadHandle, ErrorCode> {
        self.inner.open_read(path)
    }
    fn read_at(&self, id: u64, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        self.inner.read_at(id, offset, len)
    }
    fn read_stat(&self, id: u64) -> Result<FsStat, ErrorCode> {
        self.inner.read_stat(id)
    }
    fn close_read(&self, id: u64) {
        self.inner.close_read(id)
    }
    fn stat_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<Option<FsStat>, ErrorCode> {
        self.inner.stat_file(dir, name, kind)
    }
    fn part_create(&self, dir: &Path, name: &str) -> Result<(), ErrorCode> {
        self.inner.part_create(dir, name)
    }
    fn part_truncate(&self, dir: &Path, name: &str, len: u64) -> Result<(), ErrorCode> {
        self.inner.part_truncate(dir, name, len)
    }
    fn part_append(&self, dir: &Path, name: &str, offset: u64, data: &[u8]) -> Result<(), ErrorCode> {
        self.inner.part_append(dir, name, offset, data)
    }
    fn part_len(&self, dir: &Path, name: &str) -> Result<u64, ErrorCode> {
        self.inner.part_len(dir, name)
    }
    fn part_read(&self, dir: &Path, name: &str, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        self.inner.part_read(dir, name, offset, len)
    }
    fn part_sync(&self, dir: &Path, name: &str, final_mtime_ms: Option<u64>) -> Result<(), ErrorCode> {
        self.inner.part_sync(dir, name, final_mtime_ms)
    }
    fn part_rename(&self, dir: &Path, name: &str, target_name: &str) -> Result<(), ErrorCode> {
        self.inner.part_rename(dir, name, target_name)
    }
    fn remove_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<(), ErrorCode> {
        self.inner.remove_file(dir, name, kind)
    }
    fn meta_read(&self, dir: &Path, name: &str) -> Result<Option<Vec<u8>>, ErrorCode> {
        self.inner.meta_read(dir, name)
    }
    fn meta_write(&self, dir: &Path, name: &str, data: &[u8]) -> Result<(), ErrorCode> {
        self.inner.meta_write(dir, name, data)
    }
    fn move_into(&self, from_dir: &Path, name: &str, to_dir: &Path) -> Result<Moved, ErrorCode> {
        self.entered.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = lock(&self.gate).recv();
        self.inner.move_into(from_dir, name, to_dir)
    }
}

#[test]
fn a_listing_is_answered_while_a_long_move_runs_and_the_move_then_reports_its_true_result() {
    let mut rig = Rig::new("slowmove");
    let (release, gate) = std::sync::mpsc::channel();
    let slow = Arc::new(SlowMove { inner: LocalFs::new(), gate: Mutex::new(gate), entered: AtomicBool::new(false) });
    let server = Arc::new(FsServer::with_fs(slow.clone(), Roots::Home { home: rig.home.clone(), receive: None }));
    rig.user = TestHelper::spawn_with(&rig.host.core, IpcRole::HelperUser, move |deps| deps.with_fs(server));
    std::fs::create_dir_all(rig.home.join("from")).unwrap();
    std::fs::create_dir_all(rig.home.join("to")).unwrap();
    std::fs::write(rig.home.join("from").join("big.bin"), b"payload").unwrap();
    let (handle, _seen) = rig.connect(false);
    let vh = &rig.viewer_hooks;

    let name = |n: &str| swapdesk_proto::FsName { name: n.into() };
    run(
        &handle,
        Command::FsOp {
            op_id: 1,
            op: swapdesk_proto::FsOpKind::Move {
                from_dir: "from".into(),
                names: vec![name("big.bin")],
                to_dir: "to".into(),
            },
        },
    );
    wait_for("the move to start on the disk", || slow.entered.load(std::sync::atomic::Ordering::SeqCst).then_some(()));
    // While it runs, a listing of the same session is answered, and the move has not answered yet.
    run(&handle, Command::Dir { req_id: 5, path: Some("to".into()) });
    vh.wait("a listing while the move runs", |e| matches!(e, Event::Listing { req_id: 5, .. }));
    assert!(!vh.has(|e| matches!(e, Event::OpResult { .. })), "the move is still running");
    // Past the generic 30 s the viewer must still hear the truth, not a failure: it hears it when the disk is done.
    release.send(()).unwrap();
    vh.wait("the move's result", |e| matches!(e, Event::OpResult { op_id: 1, .. }));
    let results =
        vh.find(|e| if let Event::OpResult { results, .. } = e { Some(results.clone()) } else { None }).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].code, ErrorCode::NoError, "the move was done");
    assert!(rig.home.join("to").join("big.bin").exists() && !rig.home.join("from").join("big.bin").exists());
    rig.finish(handle);
}

#[test]
fn a_change_stops_before_its_next_name_when_control_is_withdrawn() {
    // A change of the host's files needs FILES and CONTROL. When the host's user takes one back while a long change
    // runs, the name being worked on finishes (a move is never half) and the ones not begun are left alone.
    let mut rig = Rig::new("withdrawn");
    let (release, gate) = std::sync::mpsc::channel();
    let slow = Arc::new(SlowMove { inner: LocalFs::new(), gate: Mutex::new(gate), entered: AtomicBool::new(false) });
    let server = Arc::new(FsServer::with_fs(slow.clone(), Roots::Home { home: rig.home.clone(), receive: None }));
    rig.user = TestHelper::spawn_with(&rig.host.core, IpcRole::HelperUser, move |deps| deps.with_fs(server));
    std::fs::create_dir_all(rig.home.join("from")).unwrap();
    std::fs::create_dir_all(rig.home.join("to")).unwrap();
    for n in ["one.bin", "two.bin", "three.bin"] {
        std::fs::write(rig.home.join("from").join(n), n).unwrap();
    }
    let (handle, _seen) = rig.connect(false);
    let name = |n: &str| swapdesk_proto::FsName { name: n.into() };
    run(
        &handle,
        Command::FsOp {
            op_id: 1,
            op: swapdesk_proto::FsOpKind::Move {
                from_dir: "from".into(),
                names: vec![name("one.bin"), name("two.bin"), name("three.bin")],
                to_dir: "to".into(),
            },
        },
    );
    wait_for("the first move to start on the disk", || {
        slow.entered.load(std::sync::atomic::Ordering::SeqCst).then_some(())
    });
    // The user takes CONTROL back while the first name is on the disk.
    let remaining = PermissionSet::VIEW | FILES;
    assert!(rig.host.core.host().set_session_grants(handle.session_id(), remaining));
    // Give the session core its moment to abort what needed CONTROL, then let the disk finish the first name.
    std::thread::sleep(Duration::from_millis(300));
    release.send(()).unwrap();
    wait_for("the first name to be done", || rig.home.join("to").join("one.bin").exists().then_some(()));
    // A second and third name would each wait on the gate forever if they began: release them so a run that does not
    // stop shows itself as moved files, not as a hang.
    release.send(()).unwrap();
    release.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(rig.home.join("to").join("one.bin").exists(), "the name being worked on finished");
    assert!(
        rig.home.join("from").join("two.bin").exists() && rig.home.join("from").join("three.bin").exists(),
        "the names not begun were left alone"
    );
    assert!(!rig.home.join("to").join("two.bin").exists() && !rig.home.join("to").join("three.bin").exists());
    rig.finish(handle);
}
