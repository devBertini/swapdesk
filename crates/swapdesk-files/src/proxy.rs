//! The disk over a pipe (stage 44): [`PipeFs`] is the [`Fs`] the service hands to the file engine, [`FsServer`] is
//! what the process that owns the user's files (the user helper on the host side, the UI on the viewer side) runs to
//! answer it.
//!
//! The service never opens a path a peer sent. Every path that crosses the pipe is **virtual**: relative to a root the
//! serving process chose, `/` separated, one component at a time checked by the file-name rules
//! ([`crate::name::validate_name`]). The server re-checks it, so a compromised service cannot widen what the process
//! reaches:
//!
//! * [`Roots::Home`] (the user helper): the user's home folder is the root; the empty path is the root itself; a
//!   default receive folder (a path under the root) is created on first use.
//! * [`Roots::Tokens`] (the UI): the only paths are `p<N>/<file name>` (a file the user picked, to read) and
//!   `r<N>[/<folder>...]` (a receive folder the user chose, to write into), where `N` is a token the UI itself made
//!   ([`Tokens`]). Nothing can be listed, and nothing else can be read or written.
//!
//! Read handles are `u64` ids that live only in the serving process: [`PipeFs`] tags each one with the link that made
//! it, so an id from a pipe that is gone fails with `NotFound` instead of meeting another file on the new pipe.
//! `close_read` is fire and forget. No call moves more than [`MAX_IO`] bytes, a listing has at most 2000 entries and
//! `meta_read` at most 1 KiB, all checked before anything is allocated or sent.
//!
//! The calls block (they wait for the answer): run them on a dedicated thread or in `spawn_blocking`, never on an
//! async executor thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use swapdesk_ipc::{
    Bytes, FsAnswer, FsCall, FsEntryView, FsHandleView, FsKind, FsPage, FsStatView, MAX_FS_LIST, MAX_FS_META,
    MAX_FS_NAME, MAX_FS_PATH, Message, Text, VolumesView,
};
use swapdesk_proto::ErrorCode;

use crate::disk::{DirRead, Fs, FsEntry, FsStat, Kind, MAX_IO, Moved, ReadHandle};
use crate::local::LocalFs;
use crate::name::validate_name;
use crate::system::SystemDisk;
use crate::volumes::{Volumes, volume_id, walk_no_link};

/// Where a file that arrives with no folder named is saved on the host: this virtual path under the helper's root. The
/// helper makes the folder on first use.
pub const DEFAULT_RECEIVE: &str = "Downloads/Swapdesk";

/// How long a call waits for its answer before it fails with `IoFailed`.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a `Move` waits. A move across volumes copies, verifies and sends the source to the bin, which takes as long
/// as the bytes need, so it has its own, much longer limit: the generic 30 s would tell the viewer `IoFailed` while the
/// helper carries on and finishes the move. The limit only catches a hung volume; a move that is merely long gets its
/// true result. The user helper runs each `Move` on a thread of its own, so a long one holds no other call.
pub const MOVE_CALL_TIMEOUT: Duration = Duration::from_secs(3600);
/// Bits of a read id that belong to the serving process; the link tag sits above them.
const ID_BITS: u32 = 48;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------- the client ----------

/// One live pipe to a serving process, as [`PipeFs`] sees it.
pub trait FsCaller: Send + Sync {
    /// Names the pipe: another number when another pipe (another process) answers.
    fn link_id(&self) -> u64;
    /// Sends the call and waits for its answer (`IoFailed` when the pipe closes or answers too late).
    fn call(&self, call: FsCall) -> Result<FsAnswer, ErrorCode>;
    /// Sends a call that has no answer.
    fn send(&self, call: FsCall);
}

/// Where [`PipeFs`] finds the pipe at the time of each call (the user helper may have restarted).
pub type CallerSource = Arc<dyn Fn() -> Option<Arc<dyn FsCaller>> + Send + Sync>;

/// The [`Fs`] that works through a pipe. Paths are made virtual here (`/` separators), checked for length and
/// carried as they are; the server decides what they mean.
pub struct PipeFs {
    source: CallerSource,
}

impl PipeFs {
    pub fn new(source: CallerSource) -> Self {
        Self { source }
    }

    fn caller(&self) -> Result<Arc<dyn FsCaller>, ErrorCode> {
        (self.source)().ok_or(ErrorCode::IoFailed)
    }

    fn ask(&self, call: FsCall) -> Result<FsAnswer, ErrorCode> {
        self.caller()?.call(call)
    }
}

fn virt(p: &Path) -> Result<Text, ErrorCode> {
    let s = p.to_str().ok_or(ErrorCode::InvalidName)?;
    if s.len() > MAX_FS_PATH {
        return Err(ErrorCode::TooLarge);
    }
    Ok(Text(s.replace('\\', "/")))
}

fn nm(s: &str) -> Result<Text, ErrorCode> {
    if s.len() > MAX_FS_NAME {
        return Err(ErrorCode::InvalidName);
    }
    Ok(Text(s.to_string()))
}

fn stat_of(v: &FsStatView) -> FsStat {
    FsStat { is_dir: v.is_dir, is_file: v.is_file, is_reparse: v.is_reparse, len: v.len, mtime_ms: v.mtime_ms }
}

fn stat_view(s: &FsStat) -> FsStatView {
    FsStatView { is_dir: s.is_dir, is_file: s.is_file, is_reparse: s.is_reparse, len: s.len, mtime_ms: s.mtime_ms }
}

fn kind_wire(k: Kind) -> FsKind {
    match k {
        Kind::Part => FsKind::Part,
        Kind::Meta => FsKind::Meta,
    }
}

fn kind_of(k: FsKind) -> Kind {
    match k {
        FsKind::Part => Kind::Part,
        FsKind::Meta => Kind::Meta,
    }
}

/// A reply of the wrong shape is a broken server: `IoFailed`.
fn bad() -> ErrorCode {
    ErrorCode::IoFailed
}

impl Fs for PipeFs {
    fn list_dir(&self, path: &Path, max_entries: u32) -> Result<DirRead, ErrorCode> {
        let max = max_entries.min(MAX_FS_LIST as u32);
        match self.ask(FsCall::ListDir { path: virt(path)?, max_entries: max })? {
            FsAnswer::Entries { entries, truncated } if entries.len() <= max as usize => Ok(DirRead {
                entries: entries
                    .into_iter()
                    .map(|e| FsEntry { name: e.name.0, is_dir: e.is_dir, size: e.size, mtime_ms: e.mtime_ms })
                    .collect(),
                truncated,
            }),
            _ => Err(bad()),
        }
    }

    fn stat(&self, path: &Path, follow: bool) -> Result<Option<FsStat>, ErrorCode> {
        match self.ask(FsCall::Stat { path: virt(path)?, follow })? {
            FsAnswer::Stat(s) => Ok(s.as_ref().map(stat_of)),
            _ => Err(bad()),
        }
    }

    fn open_read(&self, path: &Path) -> Result<ReadHandle, ErrorCode> {
        let caller = self.caller()?;
        match caller.call(FsCall::OpenRead { path: virt(path)? })? {
            FsAnswer::Handle(h) if h.id < (1 << ID_BITS) => {
                Ok(ReadHandle { id: tag(caller.link_id(), h.id), len: h.len, mtime_ms: h.mtime_ms })
            }
            _ => Err(bad()),
        }
    }

    fn read_at(&self, id: u64, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        if len as usize > MAX_IO {
            return Err(ErrorCode::TooLarge);
        }
        let caller = self.caller()?;
        let raw = untag(caller.link_id(), id)?;
        match caller.call(FsCall::ReadAt { id: raw, offset, len })? {
            FsAnswer::Data(d) if d.0.len() <= len as usize => Ok(d.0),
            _ => Err(bad()),
        }
    }

    fn read_stat(&self, id: u64) -> Result<FsStat, ErrorCode> {
        let caller = self.caller()?;
        let raw = untag(caller.link_id(), id)?;
        match caller.call(FsCall::ReadStat { id: raw })? {
            FsAnswer::StatNow(s) => Ok(stat_of(&s)),
            _ => Err(bad()),
        }
    }

    fn close_read(&self, id: u64) {
        // A handle of a pipe that is gone died with it; there is nothing to close.
        if let Ok(caller) = self.caller()
            && let Ok(raw) = untag(caller.link_id(), id)
        {
            caller.send(FsCall::CloseRead { id: raw });
        }
    }

    fn stat_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<Option<FsStat>, ErrorCode> {
        match self.ask(FsCall::StatFile { dir: virt(dir)?, name: nm(name)?, kind: kind_wire(kind) })? {
            FsAnswer::Stat(s) => Ok(s.as_ref().map(stat_of)),
            _ => Err(bad()),
        }
    }

    fn part_create(&self, dir: &Path, name: &str) -> Result<(), ErrorCode> {
        self.done(FsCall::PartCreate { dir: virt(dir)?, name: nm(name)? })
    }

    fn part_truncate(&self, dir: &Path, name: &str, len: u64) -> Result<(), ErrorCode> {
        self.done(FsCall::PartTruncate { dir: virt(dir)?, name: nm(name)?, len })
    }

    fn part_append(&self, dir: &Path, name: &str, offset: u64, data: &[u8]) -> Result<(), ErrorCode> {
        if data.len() > MAX_IO {
            return Err(ErrorCode::TooLarge);
        }
        self.done(FsCall::PartAppend { dir: virt(dir)?, name: nm(name)?, offset, data: Bytes(data.to_vec()) })
    }

    fn part_len(&self, dir: &Path, name: &str) -> Result<u64, ErrorCode> {
        match self.ask(FsCall::PartLen { dir: virt(dir)?, name: nm(name)? })? {
            FsAnswer::Len(n) => Ok(n),
            _ => Err(bad()),
        }
    }

    fn part_read(&self, dir: &Path, name: &str, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        if len as usize > MAX_IO {
            return Err(ErrorCode::TooLarge);
        }
        match self.ask(FsCall::PartRead { dir: virt(dir)?, name: nm(name)?, offset, len })? {
            FsAnswer::Data(d) if d.0.len() <= len as usize => Ok(d.0),
            _ => Err(bad()),
        }
    }

    fn part_sync(&self, dir: &Path, name: &str, final_mtime_ms: Option<u64>) -> Result<(), ErrorCode> {
        self.done(FsCall::PartSync { dir: virt(dir)?, name: nm(name)?, final_mtime_ms })
    }

    fn part_rename(&self, dir: &Path, name: &str, target_name: &str) -> Result<(), ErrorCode> {
        self.done(FsCall::PartRename { dir: virt(dir)?, name: nm(name)?, target_name: nm(target_name)? })
    }

    fn remove_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<(), ErrorCode> {
        self.done(FsCall::RemoveFile { dir: virt(dir)?, name: nm(name)?, kind: kind_wire(kind) })
    }

    fn meta_read(&self, dir: &Path, name: &str) -> Result<Option<Vec<u8>>, ErrorCode> {
        match self.ask(FsCall::MetaRead { dir: virt(dir)?, name: nm(name)? })? {
            FsAnswer::Meta(m) => Ok(m.map(|b| b.0)),
            _ => Err(bad()),
        }
    }

    fn free_space(&self, dir: &Path) -> Result<Option<u64>, ErrorCode> {
        match self.ask(FsCall::FreeSpace { dir: virt(dir)? })? {
            FsAnswer::Free(n) => Ok(n),
            _ => Err(bad()),
        }
    }

    fn meta_write(&self, dir: &Path, name: &str, data: &[u8]) -> Result<(), ErrorCode> {
        if data.len() > MAX_FS_META {
            return Err(ErrorCode::TooLarge);
        }
        self.done(FsCall::MetaWrite { dir: virt(dir)?, name: nm(name)?, data: Bytes(data.to_vec()) })
    }

    fn list_page(&self, path: &Path, start: u32, max: u16) -> Result<FsPage, ErrorCode> {
        let max = max.min(MAX_FS_LIST as u16);
        match self.ask(FsCall::ListPage { path: virt(path)?, start, max })? {
            FsAnswer::Entries2(page) if page.entries.len() <= usize::from(max) => Ok(page),
            _ => Err(bad()),
        }
    }

    fn make_dir(&self, dir: &Path, name: &str) -> Result<(), ErrorCode> {
        self.done(FsCall::MakeDir { dir: virt(dir)?, name: nm(name)? })
    }

    fn rename(&self, dir: &Path, name: &str, new_name: &str) -> Result<(), ErrorCode> {
        self.done(FsCall::Rename { dir: virt(dir)?, name: nm(name)?, new_name: nm(new_name)? })
    }

    /// Through a pipe the serving process has finished the whole move (a copy's source is already in the bin), so the
    /// answer is always `Renamed`.
    fn move_into(&self, from_dir: &Path, name: &str, to_dir: &Path) -> Result<Moved, ErrorCode> {
        self.done(FsCall::Move { from_dir: virt(from_dir)?, name: nm(name)?, to_dir: virt(to_dir)? })?;
        Ok(Moved::Renamed)
    }

    fn trash(&self, dir: &Path, name: &str) -> Result<(), ErrorCode> {
        self.done(FsCall::Trash { dir: virt(dir)?, name: nm(name)? })
    }

    fn volumes(&self) -> Result<VolumesView, ErrorCode> {
        match self.ask(FsCall::Volumes)? {
            FsAnswer::Volumes(v) => Ok(v),
            _ => Err(bad()),
        }
    }
}

impl PipeFs {
    fn done(&self, call: FsCall) -> Result<(), ErrorCode> {
        match self.ask(call)? {
            FsAnswer::Done => Ok(()),
            _ => Err(bad()),
        }
    }
}

/// The id a caller of [`PipeFs`] sees: the server's id with the link that made it above.
fn tag(link: u64, id: u64) -> u64 {
    ((link & 0xFFFF) << ID_BITS) | id
}

/// The server's id of a tagged one; `NotFound` when another link made it.
fn untag(link: u64, id: u64) -> Result<u64, ErrorCode> {
    if id >> ID_BITS == link & 0xFFFF { Ok(id & ((1 << ID_BITS) - 1)) } else { Err(ErrorCode::NotFound) }
}

// ---------- request and reply correlation over a pipe ----------

type Answer = Result<FsAnswer, ErrorCode>;

#[derive(Default)]
struct PendingState {
    map: HashMap<u32, SyncSender<Answer>>,
    closed: bool,
}

/// The calls in flight on one pipe: each waits for the reply with its request number.
#[derive(Default)]
pub struct Pending {
    next: AtomicU32,
    state: Mutex<PendingState>,
}

impl Pending {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a request, runs `send` with its number, and waits up to `timeout` for the answer. `send` returns
    /// false when the pipe took nothing. A closed pipe, a failed send and a late answer are `IoFailed`.
    pub fn call(&self, timeout: Duration, send: impl FnOnce(u32) -> bool) -> Answer {
        let (tx, rx) = sync_channel(1);
        let req = {
            let mut st = lock(&self.state);
            if st.closed {
                return Err(ErrorCode::IoFailed);
            }
            let req = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            st.map.insert(req, tx);
            req
        };
        if !send(req) {
            lock(&self.state).map.remove(&req);
            return Err(ErrorCode::IoFailed);
        }
        match rx.recv_timeout(timeout) {
            Ok(a) => a,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                lock(&self.state).map.remove(&req);
                Err(ErrorCode::IoFailed)
            }
        }
    }

    /// An answer arrived (called by the thread that reads the pipe). An unknown number is ignored.
    pub fn complete(&self, req: u32, answer: Answer) {
        if let Some(tx) = lock(&self.state).map.remove(&req) {
            let _ = tx.try_send(answer);
        }
    }

    /// The pipe is gone: every call in flight fails and later ones fail at once.
    pub fn close(&self) {
        let mut st = lock(&self.state);
        st.closed = true;
        for (_, tx) in st.map.drain() {
            let _ = tx.try_send(Err(ErrorCode::IoFailed));
        }
    }

    pub fn in_flight(&self) -> usize {
        lock(&self.state).map.len()
    }
}

/// An [`FsCaller`] over a way to write messages (a helper's outbox, a UI connection's queue) and a [`Pending`] the
/// reader of that pipe feeds with [`Pending::complete`].
pub struct PipeCaller {
    link_id: u64,
    pending: Arc<Pending>,
    write: Box<dyn Fn(Message) -> bool + Send + Sync>,
    timeout: Duration,
    move_timeout: Duration,
}

impl PipeCaller {
    pub fn new(link_id: u64, pending: Arc<Pending>, write: impl Fn(Message) -> bool + Send + Sync + 'static) -> Self {
        Self { link_id, pending, write: Box::new(write), timeout: CALL_TIMEOUT, move_timeout: MOVE_CALL_TIMEOUT }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Another limit for `Move` calls (see [`MOVE_CALL_TIMEOUT`]).
    #[must_use]
    pub fn with_move_timeout(mut self, timeout: Duration) -> Self {
        self.move_timeout = timeout;
        self
    }

    pub fn pending(&self) -> &Arc<Pending> {
        &self.pending
    }
}

impl FsCaller for PipeCaller {
    fn link_id(&self) -> u64 {
        self.link_id
    }

    fn call(&self, call: FsCall) -> Result<FsAnswer, ErrorCode> {
        let timeout = if matches!(call, FsCall::Move { .. }) { self.move_timeout } else { self.timeout };
        self.pending.call(timeout, |req| (self.write)(Message::FsRequest { req, call }))
    }

    fn send(&self, call: FsCall) {
        let _ = (self.write)(Message::FsRequest { req: 0, call });
    }
}

// ---------- the server ----------

/// The tokens a UI hands out for the files it lets the service reach: the file the user picked (`p<N>/<name>`) and the
/// folder the user chose to receive into (`r<N>`).
#[derive(Default)]
pub struct Tokens {
    next: AtomicU32,
    picks: Mutex<HashMap<u32, PathBuf>>,
    recvs: Mutex<HashMap<u32, PathBuf>>,
}

impl Tokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lets the service read the file at `path` (a file the user picked); returns its token and the name the offer
    /// will carry. `None` for a path with no usable file name.
    pub fn pick(&self, path: PathBuf) -> Option<(u32, String)> {
        let name = path.file_name()?.to_str()?.to_string();
        let token = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        lock(&self.picks).insert(token, path);
        Some((token, name))
    }

    /// Lets the service write into the folder `dir` (the folder the user chose); returns its token.
    pub fn receive_into(&self, dir: PathBuf) -> u32 {
        let token = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        lock(&self.recvs).insert(token, dir);
        token
    }

    /// Takes the token away (a transfer is over).
    pub fn forget(&self, token: u32) {
        lock(&self.picks).remove(&token);
        lock(&self.recvs).remove(&token);
    }
}

/// What a serving process lets the service reach.
pub enum Roots {
    /// The user helper's roots (FILES-UI.md 8.1). The empty first component is the home folder (the 1.0 meaning of
    /// every path); with a [`SystemDisk`] (see [`FsServer::with_system`]) a first component `:<id>` is a published
    /// volume. `receive` is the virtual path of the folder created on first use for files that arrive with no folder
    /// named.
    Home { home: PathBuf, receive: Option<String> },
    /// Only what the UI put in `Tokens`.
    Tokens(Arc<Tokens>),
}

/// Answers the calls of a [`PipeFs`] on this machine's disk, inside its [`Roots`].
pub struct FsServer {
    fs: Arc<dyn Fs>,
    roots: Roots,
    volumes: Option<Volumes>,
}

/// A virtual path split into checked components (the UI's token roots, and the part of a 1.1 path after its root).
fn components(v: &str) -> Result<Vec<&str>, ErrorCode> {
    if v.is_empty() {
        return Ok(Vec::new());
    }
    if v.len() > MAX_FS_PATH {
        return Err(ErrorCode::TooLarge);
    }
    let parts: Vec<&str> = v.split('/').collect();
    for p in &parts {
        validate_name(p)?;
    }
    Ok(parts)
}

/// `p7` or `r7` to 7.
fn token_of(first: &str, prefix: char) -> Option<u32> {
    let digits = first.strip_prefix(prefix)?;
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A 1.1 path on the helper's roots: where it starts and what follows, so a change can walk the components.
struct Resolved {
    base: PathBuf,
    rest: Vec<String>,
}

impl Resolved {
    fn real(&self) -> PathBuf {
        let mut p = self.base.clone();
        p.extend(&self.rest);
        p
    }
}

impl FsServer {
    /// A server on this machine's disk (`LocalFs`).
    pub fn new(roots: Roots) -> Self {
        Self { fs: Arc::new(LocalFs::new()), roots, volumes: None }
    }

    /// A server over another `Fs` (tests, the helper's `LocalFs` with its free-space probe).
    pub fn with_fs(fs: Arc<dyn Fs>, roots: Roots) -> Self {
        Self { fs, roots, volumes: None }
    }

    /// Lets the server publish the volumes `system` reports and serve the explorer's calls on them (FILES-UI.md 8.1):
    /// volume paths, `Volumes`, the bin and the protected folders. Without it a server speaks the 1.0 paths only.
    #[must_use]
    pub fn with_system(mut self, system: Arc<dyn SystemDisk>) -> Self {
        self.volumes = Some(Volumes::new(system));
        self
    }

    /// True when the set of published volumes changed since the last call (the first compares with the set at the time
    /// of [`Self::with_system`]). The helper
    /// asks about once a second while it serves files and tells the service with `FsVolumesChanged`.
    pub fn poll_volumes_changed(&self) -> bool {
        self.volumes.as_ref().is_some_and(Volumes::changed)
    }

    /// Does this server publish volumes (so a poll means something)?
    pub fn publishes_volumes(&self) -> bool {
        self.volumes.is_some()
    }

    /// A 1.1 path of the helper's roots: where it starts and the checked names after it. Only for [`Roots::Home`].
    fn resolve(&self, v: &str) -> Result<Resolved, ErrorCode> {
        let Roots::Home { home, .. } = &self.roots else { return Err(ErrorCode::InvalidName) };
        if v.len() > MAX_FS_PATH {
            return Err(ErrorCode::TooLarge);
        }
        if let Some(rest) = v.strip_prefix(':') {
            // The volume's own component is `:<id>`; `validate_name` refuses `:` for every other one.
            let (first, tail) = rest.split_once('/').map_or((rest, None), |(f, t)| (f, Some(t)));
            let id = volume_id(&format!(":{first}"))?;
            let volumes = self.volumes.as_ref().ok_or(ErrorCode::VolumeGone)?;
            let base = volumes.find(id)?.root;
            // `:3/` has an empty component: refused, like any other.
            let rest = match tail {
                None => Vec::new(),
                Some("") => return Err(ErrorCode::InvalidName),
                Some(t) => components(t)?.into_iter().map(str::to_string).collect(),
            };
            return Ok(Resolved { base, rest });
        }
        let rest = components(v)?.into_iter().map(str::to_string).collect();
        Ok(Resolved { base: home.clone(), rest })
    }

    /// A path that names a folder (or, for `stat`, anything under the root).
    fn folder(&self, v: &str) -> Result<PathBuf, ErrorCode> {
        match &self.roots {
            Roots::Home { receive, .. } => {
                let p = self.resolve(v)?.real();
                if receive.as_deref() == Some(v) && !v.is_empty() && !p.exists() {
                    // The default receive folder is made when first needed; failing leaves the usual NotFound.
                    let _ = std::fs::create_dir_all(&p);
                }
                Ok(p)
            }
            Roots::Tokens(t) => {
                let parts = components(v)?;
                let (first, rest) = parts.split_first().ok_or(ErrorCode::InvalidName)?;
                let token = token_of(first, 'r').ok_or(ErrorCode::InvalidName)?;
                let mut p = lock(&t.recvs).get(&token).cloned().ok_or(ErrorCode::InvalidName)?;
                p.extend(rest);
                Ok(p)
            }
        }
    }

    /// A folder something is written into or changed in: the path of [`Self::folder`], with the rules of a change
    /// (FILES-UI.md 8.1): no link or junction at any component below the root, and not inside a protected folder.
    fn writable(&self, v: &str) -> Result<PathBuf, ErrorCode> {
        if matches!(self.roots, Roots::Tokens(_)) {
            return self.folder(v);
        }
        let r = self.resolve(v)?;
        walk_no_link(&r.base, &r.rest)?;
        self.folder(v)
    }

    /// The folder `dir` for a change that touches `names` in it: `writable`, and none of the entries protected.
    fn changeable(&self, dir: &str, names: &[&str]) -> Result<PathBuf, ErrorCode> {
        let real = self.writable(dir)?;
        if let Some(v) = &self.volumes {
            for n in names {
                validate_name(n)?;
                if v.protected_hit(&real.join(n)) {
                    return Err(ErrorCode::NotPermitted);
                }
            }
        }
        Ok(real)
    }

    /// A path to read as a file: under the root on the host side, a picked file on the viewer side.
    fn readable(&self, v: &str) -> Result<PathBuf, ErrorCode> {
        match &self.roots {
            Roots::Home { .. } => Ok(self.resolve(v)?.real()),
            Roots::Tokens(t) => {
                let parts = components(v)?;
                let [first, name] = parts.as_slice() else { return Err(ErrorCode::InvalidName) };
                let token = token_of(first, 'p').ok_or(ErrorCode::InvalidName)?;
                let path = lock(&t.picks).get(&token).cloned().ok_or(ErrorCode::InvalidName)?;
                // The name in the path is the picked file's own: the offer cannot carry another.
                if path.file_name().and_then(|n| n.to_str()) != Some(name) {
                    return Err(ErrorCode::InvalidName);
                }
                Ok(path)
            }
        }
    }

    /// Only the host side lists folders.
    fn listable(&self, v: &str) -> Result<PathBuf, ErrorCode> {
        match &self.roots {
            Roots::Home { .. } => self.folder(v),
            Roots::Tokens(_) => Err(ErrorCode::InvalidName),
        }
    }

    /// Answers one call; `None` for a call that has no answer.
    pub fn handle(&self, call: FsCall) -> Option<Result<FsAnswer, ErrorCode>> {
        if call.is_fire_and_forget() {
            if let FsCall::CloseRead { id } = call {
                self.fs.close_read(id);
            }
            return None;
        }
        Some(self.answer(call))
    }

    /// The explorer's calls (FILES-UI.md 7.4): the home roots only; the UI's token roots refuse every one.
    fn explorer(&self, call: FsCall) -> Result<FsAnswer, ErrorCode> {
        let Roots::Home { home, receive } = &self.roots else { return Err(ErrorCode::InvalidName) };
        let fs = &self.fs;
        let done = |r: Result<(), ErrorCode>| r.map(|()| FsAnswer::Done);
        match call {
            FsCall::ListPage { path, start, max } => {
                let real = self.listable(&path.0)?;
                let mut page = fs.list_page(&real, start, max.min(MAX_FS_LIST as u16))?;
                if page.free.is_none()
                    && let Some(v) = &self.volumes
                {
                    let snap = v.snapshot();
                    page.free = Volumes::containing(&snap, &real).map(|(_, vol)| vol.free_bytes);
                }
                Ok(FsAnswer::Entries2(page))
            }
            FsCall::MakeDir { dir, name } => {
                let real = self.changeable(&dir.0, &[&name.0])?;
                done(fs.make_dir(&real, &name.0))
            }
            FsCall::Rename { dir, name, new_name } => {
                let real = self.changeable(&dir.0, &[&name.0, &new_name.0])?;
                done(fs.rename(&real, &name.0, &new_name.0))
            }
            FsCall::Move { from_dir, name, to_dir } => {
                let from = self.changeable(&from_dir.0, &[&name.0])?;
                let to = self.changeable(&to_dir.0, &[&name.0])?;
                done(self.move_entry(&from, &name.0, &to))
            }
            FsCall::Trash { dir, name } => {
                let real = self.changeable(&dir.0, &[&name.0])?;
                done(self.trash_entry(&real.join(&name.0)))
            }
            FsCall::Volumes => {
                let view = match &self.volumes {
                    Some(v) => v.view(home, receive.as_deref()),
                    None => VolumesView { home: Text(String::new()), places: Vec::new(), volumes: Vec::new() },
                };
                Ok(FsAnswer::Volumes(view))
            }
            _ => Err(ErrorCode::InvalidName),
        }
    }

    /// Sends `target` to its volume's bin: `NoBin` before anything where the volume has none.
    fn trash_entry(&self, target: &Path) -> Result<(), ErrorCode> {
        let Some(v) = &self.volumes else { return Err(ErrorCode::NotPermitted) };
        if std::fs::symlink_metadata(target).is_err() {
            return Err(ErrorCode::NotFound);
        }
        let snap = v.snapshot();
        if Volumes::containing(&snap, target).is_some_and(|(_, vol)| !vol.has_bin) {
            return Err(ErrorCode::NoBin);
        }
        v.system().trash(target)
    }

    /// `Move`: in place when it can be; across volumes the disk copies and verifies, and the source then goes to the
    /// bin (so a volume without one refuses before anything is copied). A source that cannot go to the bin takes its
    /// copy away again.
    fn move_entry(&self, from: &Path, name: &str, to: &Path) -> Result<(), ErrorCode> {
        let src = from.join(name);
        if let Some(v) = &self.volumes {
            let snap = v.snapshot();
            let src_vol = Volumes::containing(&snap, &src);
            let dest_vol = Volumes::containing(&snap, to);
            if src_vol.map(|(id, _)| id) != dest_vol.map(|(id, _)| id) && src_vol.is_some_and(|(_, vol)| !vol.has_bin) {
                return Err(ErrorCode::NoBin);
            }
        }
        match self.fs.move_into(from, name, to)? {
            Moved::Renamed => Ok(()),
            Moved::Copied => self.trash_entry(&src).inspect_err(|_| {
                let _ = crate::fsops::remove_tree(&to.join(name));
            }),
        }
    }

    fn answer(&self, call: FsCall) -> Result<FsAnswer, ErrorCode> {
        let fs = &self.fs;
        let done = |r: Result<(), ErrorCode>| r.map(|()| FsAnswer::Done);
        match call {
            FsCall::ListDir { path, max_entries } => {
                let path = self.listable(&path.0)?;
                let max = max_entries.min(MAX_FS_LIST as u32);
                let read = fs.list_dir(&path, max)?;
                Ok(FsAnswer::Entries {
                    truncated: read.truncated,
                    entries: read
                        .entries
                        .into_iter()
                        .map(|e| FsEntryView {
                            name: Text(e.name),
                            is_dir: e.is_dir,
                            size: e.size,
                            mtime_ms: e.mtime_ms,
                        })
                        .collect(),
                })
            }
            FsCall::Stat { path, follow } => {
                // The UI answers `stat` for its receive folders only (the name a file would get).
                let p = self.folder(&path.0)?;
                Ok(FsAnswer::Stat(fs.stat(&p, follow)?.as_ref().map(stat_view)))
            }
            FsCall::OpenRead { path } => {
                let h = fs.open_read(&self.readable(&path.0)?)?;
                Ok(FsAnswer::Handle(FsHandleView { id: h.id, len: h.len, mtime_ms: h.mtime_ms }))
            }
            FsCall::ReadAt { id, offset, len } => Ok(FsAnswer::Data(Bytes(fs.read_at(id, offset, len)?))),
            FsCall::ReadStat { id } => Ok(FsAnswer::StatNow(stat_view(&fs.read_stat(id)?))),
            FsCall::CloseRead { id } => {
                fs.close_read(id);
                Ok(FsAnswer::Done)
            }
            FsCall::StatFile { dir, name, kind } => {
                Ok(FsAnswer::Stat(fs.stat_file(&self.folder(&dir.0)?, &name.0, kind_of(kind))?.as_ref().map(stat_view)))
            }
            FsCall::PartCreate { dir, name } => done(fs.part_create(&self.changeable(&dir.0, &[&name.0])?, &name.0)),
            FsCall::PartTruncate { dir, name, len } => done(fs.part_truncate(&self.writable(&dir.0)?, &name.0, len)),
            FsCall::PartAppend { dir, name, offset, data } => {
                done(fs.part_append(&self.writable(&dir.0)?, &name.0, offset, &data.0))
            }
            FsCall::PartLen { dir, name } => Ok(FsAnswer::Len(fs.part_len(&self.folder(&dir.0)?, &name.0)?)),
            FsCall::PartRead { dir, name, offset, len } => {
                Ok(FsAnswer::Data(Bytes(fs.part_read(&self.folder(&dir.0)?, &name.0, offset, len)?)))
            }
            FsCall::PartSync { dir, name, final_mtime_ms } => {
                done(fs.part_sync(&self.writable(&dir.0)?, &name.0, final_mtime_ms))
            }
            FsCall::PartRename { dir, name, target_name } => {
                done(fs.part_rename(&self.changeable(&dir.0, &[&target_name.0])?, &name.0, &target_name.0))
            }
            FsCall::RemoveFile { dir, name, kind } => {
                done(fs.remove_file(&self.writable(&dir.0)?, &name.0, kind_of(kind)))
            }
            FsCall::MetaRead { dir, name } => {
                Ok(FsAnswer::Meta(fs.meta_read(&self.folder(&dir.0)?, &name.0)?.map(Bytes)))
            }
            FsCall::MetaWrite { dir, name, data } => done(fs.meta_write(&self.writable(&dir.0)?, &name.0, &data.0)),
            FsCall::FreeSpace { dir } => Ok(FsAnswer::Free(fs.free_space(&self.folder(&dir.0)?)?)),
            // FILES-UI.md 7.4: served on the helper's roots; the UI's token roots refuse every one of them.
            call @ (FsCall::ListPage { .. }
            | FsCall::MakeDir { .. }
            | FsCall::Rename { .. }
            | FsCall::Move { .. }
            | FsCall::Trash { .. }
            | FsCall::Volumes) => self.explorer(call),
        }
    }

    /// One message of a pipe: an `FsRequest` becomes its `FsReply` (nothing for a fire-and-forget call); anything
    /// else is not for a disk server and answers nothing.
    pub fn serve(&self, msg: Message) -> Option<Message> {
        match msg {
            Message::FsRequest { req, call } => self.handle(call).map(|result| Message::FsReply { req, result }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vol_tests;
