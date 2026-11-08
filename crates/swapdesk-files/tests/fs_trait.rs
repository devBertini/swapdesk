//! The engine over an `Fs` that is not the plain `LocalFs`: a recording one and a fault-injecting one. Proves the engine
//! reaches the disk only through the trait, and that a failed rename leaves the partial file intact.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ring::digest::{SHA256, digest};
use swapdesk_files::{DirRead, Engine, Event, FilesConfig, Fs, FsStat, Kind, LocalFs, Out, ReadHandle, Side};
use swapdesk_proto::{DirRequest, ErrorCode, FileDone, FileOffer, Message};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-files-fstrait-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Wraps `LocalFs`, records each call's name, and can fail renames.
struct Probe {
    inner: LocalFs,
    calls: Mutex<Vec<&'static str>>,
    fail_rename: AtomicBool,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self { inner: LocalFs::new(), calls: Mutex::new(Vec::new()), fail_rename: AtomicBool::new(false) })
    }

    fn rec(&self, call: &'static str) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

impl Fs for Probe {
    fn list_dir(&self, path: &Path, max_entries: u32) -> Result<DirRead, ErrorCode> {
        self.rec("list_dir");
        self.inner.list_dir(path, max_entries)
    }
    fn stat(&self, path: &Path, follow: bool) -> Result<Option<FsStat>, ErrorCode> {
        self.rec("stat");
        self.inner.stat(path, follow)
    }
    fn open_read(&self, path: &Path) -> Result<ReadHandle, ErrorCode> {
        self.rec("open_read");
        self.inner.open_read(path)
    }
    fn read_at(&self, id: u64, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        self.rec("read_at");
        self.inner.read_at(id, offset, len)
    }
    fn read_stat(&self, id: u64) -> Result<FsStat, ErrorCode> {
        self.rec("read_stat");
        self.inner.read_stat(id)
    }
    fn close_read(&self, id: u64) {
        self.rec("close_read");
        self.inner.close_read(id);
    }
    fn stat_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<Option<FsStat>, ErrorCode> {
        self.rec("stat_file");
        self.inner.stat_file(dir, name, kind)
    }
    fn part_create(&self, dir: &Path, name: &str) -> Result<(), ErrorCode> {
        self.rec("part_create");
        self.inner.part_create(dir, name)
    }
    fn part_truncate(&self, dir: &Path, name: &str, len: u64) -> Result<(), ErrorCode> {
        self.rec("part_truncate");
        self.inner.part_truncate(dir, name, len)
    }
    fn part_append(&self, dir: &Path, name: &str, offset: u64, data: &[u8]) -> Result<(), ErrorCode> {
        self.rec("part_append");
        self.inner.part_append(dir, name, offset, data)
    }
    fn part_len(&self, dir: &Path, name: &str) -> Result<u64, ErrorCode> {
        self.rec("part_len");
        self.inner.part_len(dir, name)
    }
    fn part_read(&self, dir: &Path, name: &str, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        self.rec("part_read");
        self.inner.part_read(dir, name, offset, len)
    }
    fn part_sync(&self, dir: &Path, name: &str, final_mtime_ms: Option<u64>) -> Result<(), ErrorCode> {
        self.rec("part_sync");
        self.inner.part_sync(dir, name, final_mtime_ms)
    }
    fn part_rename(&self, dir: &Path, name: &str, target_name: &str) -> Result<(), ErrorCode> {
        self.rec("part_rename");
        if self.fail_rename.load(Ordering::SeqCst) {
            return Err(ErrorCode::IoFailed);
        }
        self.inner.part_rename(dir, name, target_name)
    }
    fn remove_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<(), ErrorCode> {
        self.rec("remove_file");
        self.inner.remove_file(dir, name, kind)
    }
    fn meta_read(&self, dir: &Path, name: &str) -> Result<Option<Vec<u8>>, ErrorCode> {
        self.rec("meta_read");
        self.inner.meta_read(dir, name)
    }
    fn meta_write(&self, dir: &Path, name: &str, data: &[u8]) -> Result<(), ErrorCode> {
        self.rec("meta_write");
        self.inner.meta_write(dir, name, data)
    }
}

fn host(fs: Arc<Probe>, dir: &Path) -> Engine {
    let mut cfg = FilesConfig::new(Side::Host, dir.to_path_buf());
    cfg.default_dir = Some(dir.to_path_buf());
    Engine::with_fs(cfg, fs)
}

fn offer(e: &mut Engine, name: &str, data: &[u8]) -> Vec<Out> {
    e.on_message(Message::FileOffer(FileOffer {
        transfer_id: 1,
        name: name.into(),
        size: data.len() as u64,
        mtime_ms: 1_000,
        dest: None,
    }))
}

fn sha(d: &[u8]) -> [u8; 32] {
    digest(&SHA256, d).as_ref().try_into().unwrap()
}

#[test]
fn a_failing_rename_leaves_the_partial_file_intact() {
    let dir = temp("rename");
    let probe = Probe::new();
    probe.fail_rename.store(true, Ordering::SeqCst);
    let mut e = host(Arc::clone(&probe), &dir);
    let data = vec![7u8; 5000];
    offer(&mut e, "a.bin", &data);
    e.on_data(1, 0, &data);
    let outs = e.on_message(Message::FileDone(FileDone { transfer_id: 1, sha256: sha(&data) }));

    assert!(
        outs.iter().any(|o| matches!(o, Out::Msg(Message::FileVerified(v)) if !v.ok)),
        "the sender is told it failed"
    );
    assert!(outs.iter().any(|o| matches!(o, Out::Event(Event::Failed { id: 1, code: ErrorCode::IoFailed }))));
    assert!(probe.calls().contains(&"part_rename"));
    assert_eq!(std::fs::read(dir.join("a.bin.swapdesk-part")).unwrap(), data, "the partial file is untouched");
    assert!(!dir.join("a.bin").exists(), "no final file appeared");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_successful_receive_and_a_listing_go_through_the_trait_only() {
    let dir = temp("record");
    let probe = Probe::new();
    let mut e = host(Arc::clone(&probe), &dir);
    let data = vec![3u8; 3000];
    offer(&mut e, "b.bin", &data);
    e.on_data(1, 0, &data);
    let outs = e.on_message(Message::FileDone(FileDone { transfer_id: 1, sha256: sha(&data) }));
    assert!(outs.iter().any(|o| matches!(o, Out::Event(Event::Received { id: 1, .. }))));
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), data);

    let outs = e.on_message(Message::DirRequest(DirRequest { req_id: 9, path: None }));
    assert!(
        outs.iter()
            .any(|o| matches!(o, Out::Msg(Message::DirListing(l)) if l.entries.iter().any(|x| x.name == "b.bin")))
    );

    let calls = probe.calls();
    for needed in [
        "stat",
        "stat_file",
        "part_create",
        "part_truncate",
        "meta_write",
        "part_append",
        "part_sync",
        "part_rename",
        "remove_file",
        "list_dir",
    ] {
        assert!(calls.contains(&needed), "{needed} was not called: {calls:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn local_fs_refuses_a_path_that_escapes_its_folder_even_when_the_caller_asks() {
    let root = temp("escape");
    let inner = root.join("inner");
    std::fs::create_dir(&inner).unwrap();
    let fs = LocalFs::new();
    // A compromised caller bypasses the engine and hands the disk owner hostile names directly.
    assert_eq!(fs.part_create(&inner, "../pwned"), Err(ErrorCode::InvalidName));
    assert_eq!(fs.meta_write(&inner, "..\\pwned", b"x"), Err(ErrorCode::InvalidName));
    assert_eq!(fs.part_append(&inner, "sub/x", 0, b"x"), Err(ErrorCode::InvalidName));
    fs.part_create(&inner, "ok").unwrap();
    assert_eq!(fs.part_rename(&inner, "ok", "../pwned"), Err(ErrorCode::InvalidName));
    assert!(!root.join("pwned").exists() && !root.join("pwned.swapdesk-part").exists());
    let _ = std::fs::remove_dir_all(&root);
}
