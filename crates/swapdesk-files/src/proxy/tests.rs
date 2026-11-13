//! `PipeFs` against `FsServer` over a pipe that really encodes every message (C2.4 framing) and a server thread, so the
//! wire, the correlation and the containment are all in the path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use ring::digest::{SHA256, digest};
use swapdesk_ipc::{decode_message, encode_message};

use super::*;
use crate::recv::Incoming;
use crate::send::Outgoing;

static LINKS: AtomicU64 = AtomicU64::new(1);

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-proxy-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A server thread on one end of a message pipe; dropping the harness stops it and fails what is in flight.
struct Link {
    caller: Arc<PipeCaller>,
    thread: Option<JoinHandle<()>>,
    kill: Sender<()>,
}

impl Link {
    fn start(server: FsServer) -> Link {
        let pending = Arc::new(Pending::new());
        let (to_server, from_client): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
        let (kill, killed) = channel::<()>();
        let to_server = Mutex::new(to_server);
        let caller =
            Arc::new(PipeCaller::new(LINKS.fetch_add(1, Ordering::Relaxed), Arc::clone(&pending), move |msg| {
                // Every message goes through the real encoder, like on a pipe.
                let bytes = encode_message(&msg).expect("the client sends only encodable messages");
                to_server.lock().unwrap().send(bytes).is_ok()
            }));
        let reply_to = Arc::clone(&pending);
        let thread = std::thread::spawn(move || {
            loop {
                if killed.try_recv().is_ok() {
                    break;
                }
                let Ok(bytes) = from_client.recv_timeout(Duration::from_millis(20)) else {
                    if matches!(from_client.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)) {
                        break;
                    }
                    continue;
                };
                let msg = decode_message(&bytes).expect("the server reads what the client wrote");
                if let Some(Message::FsReply { req, result }) = server.serve(msg) {
                    // The reply travels through the encoder as well.
                    let back = decode_message(&encode_message(&Message::FsReply { req, result }).unwrap()).unwrap();
                    if let Message::FsReply { req, result } = back {
                        reply_to.complete(req, result);
                    }
                }
            }
            reply_to.close();
        });
        Link { caller, thread: Some(thread), kill }
    }

    fn fs(&self) -> PipeFs {
        let c: Arc<dyn FsCaller> = self.caller.clone();
        PipeFs::new(Arc::new(move || Some(Arc::clone(&c))))
    }

    fn stop(&mut self) {
        let _ = self.kill.send(());
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop();
    }
}

fn home_link(home: &Path, receive: Option<&str>) -> Link {
    Link::start(FsServer::new(Roots::Home { home: home.to_path_buf(), receive: receive.map(str::to_string) }))
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn a_file_is_received_through_the_pipe_and_never_overwrites() {
    let home = temp("recv");
    let link = home_link(&home, Some("Downloads/Swapdesk"));
    let fs: Arc<dyn Fs> = Arc::new(link.fs());
    let body = data(2_500_000);
    let sha: [u8; 32] = digest(&SHA256, &body).as_ref().try_into().unwrap();
    // The folder does not exist yet: asking about the default receive folder makes it (the helper's own choice).
    let dir = Path::new("Downloads/Swapdesk");
    for round in 0..2 {
        let mut inc = Incoming::open_with(Arc::clone(&fs), dir, "report.bin", body.len() as u64, 7).unwrap();
        for (i, chunk) in body.chunks(256 * 1024).enumerate() {
            inc.write((i * 256 * 1024) as u64, chunk).unwrap();
        }
        let path = inc.finish(&sha).unwrap();
        let expect = if round == 0 { "report.bin" } else { "report (2).bin" };
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), expect, "files never overwrite");
        assert_eq!(std::fs::read(home.join("Downloads").join("Swapdesk").join(expect)).unwrap(), body);
    }
    let left: Vec<_> = std::fs::read_dir(home.join("Downloads").join("Swapdesk"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left.len(), 2, "no part or meta file is left: {left:?}");
    drop(link);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_file_the_viewer_saves_carries_the_mark_of_the_web() {
    // The viewer's disk (the UI's token roots) is where a download lands: the finished file is marked as coming from
    // another machine (Windows: the Zone.Identifier stream, ZoneId=3; Mac: the quarantine attribute), and it is marked
    // before the last rename, so the finished name is never seen unmarked.
    let base = temp("motw");
    let recv = base.join("recv");
    std::fs::create_dir_all(&recv).unwrap();
    let tokens = Arc::new(Tokens::new());
    let r = tokens.receive_into(recv.clone());
    let link = Link::start(FsServer::new(Roots::Tokens(Arc::clone(&tokens))));
    let fs: Arc<dyn Fs> = Arc::new(link.fs());
    let body = data(300_000);
    let sha: [u8; 32] = digest(&SHA256, &body).as_ref().try_into().unwrap();
    let dir = PathBuf::from(format!("r{r}"));
    let mut inc = Incoming::open_with(Arc::clone(&fs), &dir, "setup.exe", body.len() as u64, 7).unwrap();
    inc.write(0, &body).unwrap();
    let path = inc.finish(&sha).unwrap();
    assert_eq!(path, dir.join("setup.exe"));
    assert_eq!(std::fs::read(recv.join("setup.exe")).unwrap(), body, "the bytes are the file's own");
    assert!(crate::plat::is_marked_downloaded(&recv.join("setup.exe")), "the received file is marked");
    // A file the host receives from the viewer is the user's own transfer to their own machine: not marked.
    let home = temp("motw-host");
    let host_link = home_link(&home, None);
    let host_fs: Arc<dyn Fs> = Arc::new(host_link.fs());
    let mut inc = Incoming::open_with(host_fs, Path::new(""), "up.bin", body.len() as u64, 7).unwrap();
    inc.write(0, &body).unwrap();
    inc.finish(&sha).unwrap();
    assert!(!crate::plat::is_marked_downloaded(&home.join("up.bin")));
    drop((link, host_link));
    let _ = (std::fs::remove_dir_all(&base), std::fs::remove_dir_all(&home));
}

#[test]
fn a_file_is_read_through_the_pipe_with_the_whole_file_hashed() {
    let home = temp("send");
    std::fs::create_dir_all(home.join("docs")).unwrap();
    let body = data(900_000);
    std::fs::write(home.join("docs").join("a.bin"), &body).unwrap();
    let link = home_link(&home, None);
    let fs: Arc<dyn Fs> = Arc::new(link.fs());
    let out = Outgoing::open_with(Arc::clone(&fs), Path::new("docs/a.bin")).unwrap();
    assert_eq!((out.name(), out.size()), ("a.bin", 900_000));
    let mut r = out.reader(123_457).unwrap();
    let mut got = Vec::new();
    while let Some(c) = r.next_chunk().unwrap() {
        got.extend_from_slice(&c);
    }
    assert_eq!(got, body[123_457..]);
    assert_eq!(r.sha256().unwrap().as_slice(), digest(&SHA256, &body).as_ref());
    drop(link);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_path_outside_the_chosen_root_is_refused_by_the_server_itself() {
    let base = temp("contain");
    let home = base.join("home");
    std::fs::create_dir_all(home.join("in")).unwrap();
    std::fs::write(base.join("secret.txt"), b"outside").unwrap();
    std::fs::write(home.join("in").join("ok.txt"), b"inside").unwrap();
    let link = home_link(&home, None);
    let fs = link.fs();
    let bad_paths = [
        "..",
        "../secret.txt",
        "in/../../secret.txt",
        "/etc/passwd",
        "/",
        "C:/Windows",
        "C:\\Windows",
        "in//ok.txt",
        "in/",
        "a:b",
        "con",
        "in/nul.txt",
        "x.swapdesk-part",
    ];
    for bad in bad_paths {
        assert_eq!(fs.list_dir(Path::new(bad), 10).err(), Some(ErrorCode::InvalidName), "list {bad}");
        assert_eq!(fs.stat(Path::new(bad), true).err(), Some(ErrorCode::InvalidName), "stat {bad}");
        assert_eq!(fs.open_read(Path::new(bad)).err(), Some(ErrorCode::InvalidName), "open {bad}");
        assert_eq!(fs.part_create(Path::new(bad), "x").err(), Some(ErrorCode::InvalidName), "part_create in {bad}");
    }
    // Raw calls, as a hostile service could write them: backslashes never reach the file system.
    let raw = |call| link.caller.call(call);
    for p in ["..\\secret.txt", "in\\..\\..\\secret.txt", "in\\ok.txt"] {
        assert_eq!(raw(FsCall::OpenRead { path: Text(p.into()) }).err(), Some(ErrorCode::InvalidName), "{p}");
        assert_eq!(raw(FsCall::Stat { path: Text(p.into()), follow: true }).err(), Some(ErrorCode::InvalidName), "{p}");
    }
    assert_eq!(
        raw(FsCall::PartCreate { dir: Text("..".into()), name: Text("x".into()) }).err(),
        Some(ErrorCode::InvalidName)
    );
    assert_eq!(
        raw(FsCall::PartRename { dir: Text("in".into()), name: Text("x".into()), target_name: Text("../y".into()) })
            .err(),
        Some(ErrorCode::InvalidName)
    );
    assert!(!base.join("x.swapdesk-part").exists() && !home.join("x.swapdesk-part").exists());
    // What is inside the root still works.
    assert_eq!(fs.list_dir(Path::new("in"), 10).unwrap().entries.len(), 1);
    assert!(fs.open_read(Path::new("in/ok.txt")).is_ok());
    assert_eq!(std::fs::read(base.join("secret.txt")).unwrap(), b"outside");
    drop(link);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_ui_server_reaches_only_its_tokens() {
    let base = temp("tokens");
    let picked = base.join("pick");
    let recv = base.join("recv");
    std::fs::create_dir_all(&picked).unwrap();
    std::fs::create_dir_all(recv.join("sub")).unwrap();
    std::fs::write(picked.join("mine.txt"), b"picked").unwrap();
    std::fs::write(picked.join("other.txt"), b"not picked").unwrap();
    let tokens = Arc::new(Tokens::new());
    let (p, name) = tokens.pick(picked.join("mine.txt")).unwrap();
    assert_eq!(name, "mine.txt");
    let r = tokens.receive_into(recv.clone());
    let link = Link::start(FsServer::new(Roots::Tokens(Arc::clone(&tokens))));
    let fs = link.fs();
    // The picked file reads; the name must be its own.
    let h = fs.open_read(Path::new(&format!("p{p}/mine.txt"))).unwrap();
    assert_eq!(fs.read_at(h.id, 0, 100).unwrap(), b"picked");
    fs.close_read(h.id);
    for bad in [
        format!("p{p}/other.txt"),
        format!("p{p}"),
        format!("p{}/mine.txt", p + 100),
        "mine.txt".into(),
        format!("r{r}/x"),
        "../pick/other.txt".to_string(),
        "".into(),
    ] {
        assert_eq!(fs.open_read(Path::new(&bad)).err(), Some(ErrorCode::InvalidName), "open {bad}");
    }
    // Nothing is listed, and unknown tokens and plain paths name nothing.
    assert_eq!(fs.list_dir(Path::new(&format!("r{r}")), 10).err(), Some(ErrorCode::InvalidName));
    for bad in [format!("r{}", r + 100), "recv".into(), format!("p{p}"), "..".into(), format!("r{r}/../pick")] {
        assert_eq!(fs.part_create(Path::new(&bad), "x").err(), Some(ErrorCode::InvalidName), "{bad}");
        assert_eq!(fs.stat(Path::new(&bad), true).err(), Some(ErrorCode::InvalidName), "{bad}");
    }
    // The receive folder (and a folder in it) takes files.
    fs.part_create(Path::new(&format!("r{r}")), "got.bin").unwrap();
    fs.part_append(Path::new(&format!("r{r}")), "got.bin", 0, b"abc").unwrap();
    assert_eq!(std::fs::read(recv.join("got.bin.swapdesk-part")).unwrap(), b"abc");
    fs.part_create(Path::new(&format!("r{r}/sub")), "in.bin").unwrap();
    assert!(recv.join("sub").join("in.bin.swapdesk-part").exists());
    // A token the UI forgot stops working.
    tokens.forget(r);
    assert_eq!(fs.part_create(Path::new(&format!("r{r}")), "later.bin").err(), Some(ErrorCode::InvalidName));
    drop(link);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn size_limits_hold_on_both_ends() {
    let home = temp("limits");
    let link = home_link(&home, None);
    let fs = link.fs();
    fs.part_create(Path::new(""), "a.bin").unwrap();
    // The client refuses before it sends anything.
    assert_eq!(fs.part_append(Path::new(""), "a.bin", 0, &vec![0; MAX_IO + 1]), Err(ErrorCode::TooLarge));
    assert_eq!(fs.part_read(Path::new(""), "a.bin", 0, MAX_IO as u32 + 1), Err(ErrorCode::TooLarge));
    assert_eq!(fs.meta_write(Path::new(""), "a.bin", &[0; 1025]), Err(ErrorCode::TooLarge));
    assert_eq!(fs.part_len(Path::new(""), "a.bin"), Ok(0));
    // A hostile service skips the client: the server's own checks answer.
    let raw = |call| link.caller.call(call);
    assert_eq!(
        raw(FsCall::PartRead { dir: Text("".into()), name: Text("a.bin".into()), offset: 0, len: MAX_IO as u32 + 1 })
            .err(),
        Some(ErrorCode::TooLarge)
    );
    // A full chunk is fine; a chunk at the wrong offset is a protocol violation.
    fs.part_append(Path::new(""), "a.bin", 0, &vec![1; MAX_IO]).unwrap();
    assert_eq!(fs.part_append(Path::new(""), "a.bin", 5, b"x"), Err(ErrorCode::ProtocolViolation));
    assert_eq!(fs.part_len(Path::new(""), "a.bin"), Ok(MAX_IO as u64));
    // A listing never passes 2000 entries, whatever is asked.
    for i in 0..2100 {
        std::fs::write(home.join(format!("f{i:04}")), b"").unwrap();
    }
    let all = fs.list_dir(Path::new(""), u32::MAX).unwrap();
    assert_eq!(all.entries.len(), 2000);
    assert!(all.truncated);
    let few = fs.list_dir(Path::new(""), 10).unwrap();
    assert_eq!((few.entries.len(), few.truncated), (10, true));
    // meta_read returns at most 1 KiB of a larger file.
    std::fs::write(home.join("a.bin.swapdesk-meta"), vec![9u8; 5000]).unwrap();
    assert_eq!(fs.meta_read(Path::new(""), "a.bin").unwrap().unwrap().len(), 1024);
    drop(link);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_read_id_lives_only_with_the_pipe_that_made_it() {
    let home = temp("ids");
    std::fs::write(home.join("one.txt"), b"first file").unwrap();
    std::fs::write(home.join("two.txt"), b"other file").unwrap();
    let mut a = home_link(&home, None);
    let slot: Arc<Mutex<Option<Arc<dyn FsCaller>>>> = Arc::new(Mutex::new(Some(a.caller.clone())));
    let source = Arc::clone(&slot);
    let fs = PipeFs::new(Arc::new(move || source.lock().unwrap().clone()));
    let h = fs.open_read(Path::new("one.txt")).unwrap();
    assert_eq!(fs.read_at(h.id, 0, 5).unwrap(), b"first");
    // The pipe ends (the helper died): its handles died with it, and calls fail instead of hanging.
    a.stop();
    assert!(a.caller.pending().in_flight() == 0);
    assert_eq!(fs.read_at(h.id, 0, 5), Err(ErrorCode::IoFailed));
    // A new helper answers on a new pipe whose own first id is the same number, for another file.
    let b = home_link(&home, None);
    let h2 = fs.open_read(Path::new("two.txt")).unwrap_err();
    assert_eq!(h2, ErrorCode::IoFailed, "the dead pipe is still the source");
    *slot.lock().unwrap() = Some(b.caller.clone());
    let h2 = fs.open_read(Path::new("two.txt")).unwrap();
    assert_ne!(h2.id, h.id, "the two links tag their ids apart");
    assert_eq!(fs.read_at(h.id, 0, 5), Err(ErrorCode::NotFound), "the old id never reaches the new pipe's file");
    assert_eq!(fs.read_stat(h.id), Err(ErrorCode::NotFound));
    fs.close_read(h.id);
    assert_eq!(fs.read_at(h2.id, 0, 5).unwrap(), b"other");
    // close_read is fire and forget, and a closed handle is gone on the server.
    fs.close_read(h2.id);
    assert_eq!(fs.read_at(h2.id, 0, 5), Err(ErrorCode::NotFound));
    drop(b);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_call_with_no_answer_times_out_and_a_closed_pipe_fails_at_once() {
    let pending = Arc::new(Pending::new());
    let sent = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&sent);
    let caller = PipeCaller::new(77, Arc::clone(&pending), move |m| {
        log.lock().unwrap().push(m);
        true
    })
    .with_timeout(Duration::from_millis(50));
    assert_eq!(
        caller.call(FsCall::PartLen { dir: Text("".into()), name: Text("a".into()) }).err(),
        Some(ErrorCode::IoFailed)
    );
    assert_eq!(pending.in_flight(), 0, "the late call is forgotten");
    // A late answer for it changes nothing.
    pending.complete(1, Ok(FsAnswer::Len(5)));
    // A send the pipe refuses fails at once.
    let refusing = PipeCaller::new(78, Arc::new(Pending::new()), |_| false);
    assert_eq!(refusing.call(FsCall::ReadStat { id: 1 }).err(), Some(ErrorCode::IoFailed));
    // A close fails what waits and everything after.
    let p2 = Arc::new(Pending::new());
    let waiter = {
        let p2 = Arc::clone(&p2);
        std::thread::spawn(move || p2.call(Duration::from_secs(10), |_| true))
    };
    while p2.in_flight() == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    p2.close();
    assert_eq!(waiter.join().unwrap().err(), Some(ErrorCode::IoFailed));
    assert_eq!(p2.call(Duration::from_secs(1), |_| true).err(), Some(ErrorCode::IoFailed));
    assert_eq!(sent.lock().unwrap().len(), 1);
}

#[test]
fn a_wrong_shaped_answer_is_an_io_failure() {
    // A server that answers `Len` to everything.
    struct Liar;
    impl FsCaller for Liar {
        fn link_id(&self) -> u64 {
            9
        }
        fn call(&self, _: FsCall) -> Result<FsAnswer, ErrorCode> {
            Ok(FsAnswer::Len(1))
        }
        fn send(&self, _: FsCall) {}
    }
    let c: Arc<dyn FsCaller> = Arc::new(Liar);
    let fs = PipeFs::new(Arc::new(move || Some(Arc::clone(&c))));
    assert_eq!(fs.list_dir(Path::new(""), 5).err(), Some(ErrorCode::IoFailed));
    assert_eq!(fs.open_read(Path::new("a")).err(), Some(ErrorCode::IoFailed));
    assert_eq!(fs.part_create(Path::new(""), "a").err(), Some(ErrorCode::IoFailed));
    // A chunk bigger than what was asked is refused too.
    struct Greedy;
    impl FsCaller for Greedy {
        fn link_id(&self) -> u64 {
            0
        }
        fn call(&self, _: FsCall) -> Result<FsAnswer, ErrorCode> {
            Ok(FsAnswer::Data(Bytes(vec![0; 10])))
        }
        fn send(&self, _: FsCall) {}
    }
    let c: Arc<dyn FsCaller> = Arc::new(Greedy);
    let fs = PipeFs::new(Arc::new(move || Some(Arc::clone(&c))));
    assert_eq!(fs.part_read(Path::new(""), "a", 0, 5).err(), Some(ErrorCode::IoFailed));
    assert_eq!(fs.part_read(Path::new(""), "a", 0, 10).unwrap().len(), 10);
    // No pipe at all.
    let none = PipeFs::new(Arc::new(|| None));
    assert_eq!(none.part_len(Path::new(""), "a").err(), Some(ErrorCode::IoFailed));
    none.close_read(1);
}

#[test]
fn a_move_outlasts_the_generic_call_timeout_and_gets_its_true_answer() {
    // The generic limit is short, the move's is long: a copy that takes longer than the generic limit must answer what
    // the helper did, not `IoFailed` while the helper carries on.
    let pending = Arc::new(Pending::new());
    let answerer = Arc::clone(&pending);
    let caller = PipeCaller::new(79, Arc::clone(&pending), move |m| {
        if let Message::FsRequest { req, call } = m {
            let answerer = Arc::clone(&answerer);
            let slow = matches!(call, FsCall::Move { .. });
            std::thread::spawn(move || {
                // The move takes four times the generic limit; everything else is never answered.
                if slow {
                    std::thread::sleep(Duration::from_millis(200));
                    answerer.complete(req, Ok(FsAnswer::Done));
                }
            });
        }
        true
    })
    .with_timeout(Duration::from_millis(50))
    .with_move_timeout(Duration::from_secs(10));
    let mv = FsCall::Move { from_dir: Text("a".into()), name: Text("n".into()), to_dir: Text("b".into()) };
    assert_eq!(caller.call(mv).ok(), Some(FsAnswer::Done), "a long move is not a failed one");
    assert_eq!(
        caller.call(FsCall::PartLen { dir: Text("".into()), name: Text("a".into()) }).err(),
        Some(ErrorCode::IoFailed),
        "every other call keeps the generic limit"
    );
}

#[test]
fn a_receive_folder_that_climbs_out_or_is_relative_names_nothing() {
    let base = temp("recv-climb");
    let recv = base.join("recv");
    std::fs::create_dir_all(&recv).unwrap();
    std::fs::create_dir_all(base.join("outside")).unwrap();
    let tokens = Arc::new(Tokens::new());
    let good = tokens.receive_into(recv.clone());
    let climbing = tokens.receive_into(recv.join("..").join("outside"));
    let relative = tokens.receive_into(PathBuf::from("recv"));
    let link = Link::start(FsServer::new(Roots::Tokens(Arc::clone(&tokens))));
    let fs = link.fs();
    assert!(fs.part_create(Path::new(&format!("r{good}")), "ok.txt").is_ok(), "a plain folder still works");
    for (what, token) in [("climbing", climbing), ("relative", relative)] {
        assert_eq!(
            fs.part_create(Path::new(&format!("r{token}")), "x.txt").err(),
            Some(ErrorCode::InvalidName),
            "{what}"
        );
    }
    assert!(!base.join("outside").join("x.txt.swapdesk-part").exists());
    drop(link);
    let _ = std::fs::remove_dir_all(&base);
}
