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
    Bytes, FsAnswer, FsCall, FsEntryView, FsHandleView, FsKind, FsStatView, MAX_FS_LIST, MAX_FS_META, MAX_FS_NAME,
    MAX_FS_PATH, Message, Text,
};
use swapdesk_proto::ErrorCode;

use crate::disk::{DirRead, Fs, FsEntry, FsStat, Kind, MAX_IO, ReadHandle};
use crate::local::LocalFs;
use crate::name::validate_name;

/// Where a file that arrives with no folder named is saved on the host: this virtual path under the helper's root. The
/// helper makes the folder on first use.
pub const DEFAULT_RECEIVE: &str = "Downloads/Swapdesk";

/// How long a call waits for its answer before it fails with `IoFailed`.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
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

    fn meta_write(&self, dir: &Path, name: &str, data: &[u8]) -> Result<(), ErrorCode> {
        if data.len() > MAX_FS_META {
            return Err(ErrorCode::TooLarge);
        }
        self.done(FsCall::MetaWrite { dir: virt(dir)?, name: nm(name)?, data: Bytes(data.to_vec()) })
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
}

impl PipeCaller {
    pub fn new(link_id: u64, pending: Arc<Pending>, write: impl Fn(Message) -> bool + Send + Sync + 'static) -> Self {
        Self { link_id, pending, write: Box::new(write), timeout: CALL_TIMEOUT }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
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
        self.pending.call(self.timeout, |req| (self.write)(Message::FsRequest { req, call }))
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
    /// Everything under `home`; `receive` is the virtual path of the folder created on first use for files that
    /// arrive with no folder named.
    Home { home: PathBuf, receive: Option<String> },
    /// Only what the UI put in `Tokens`.
    Tokens(Arc<Tokens>),
}

/// Answers the calls of a [`PipeFs`] on this machine's disk, inside its [`Roots`].
pub struct FsServer {
    fs: Arc<dyn Fs>,
    roots: Roots,
}

/// A virtual path split into checked components.
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

impl FsServer {
    /// A server on this machine's disk (`LocalFs`).
    pub fn new(roots: Roots) -> Self {
        Self { fs: Arc::new(LocalFs::new()), roots }
    }

    /// A server over another `Fs` (tests).
    pub fn with_fs(fs: Arc<dyn Fs>, roots: Roots) -> Self {
        Self { fs, roots }
    }

    /// A path that names a folder (or, for `stat`, anything under the root).
    fn folder(&self, v: &str) -> Result<PathBuf, ErrorCode> {
        let parts = components(v)?;
        match &self.roots {
            Roots::Home { home, receive } => {
                let mut p = home.clone();
                p.extend(&parts);
                if receive.as_deref() == Some(v) && !v.is_empty() && !p.exists() {
                    // The default receive folder is made when first needed; failing leaves the usual NotFound.
                    let _ = std::fs::create_dir_all(&p);
                }
                Ok(p)
            }
            Roots::Tokens(t) => {
                let (first, rest) = parts.split_first().ok_or(ErrorCode::InvalidName)?;
                let token = token_of(first, 'r').ok_or(ErrorCode::InvalidName)?;
                let mut p = lock(&t.recvs).get(&token).cloned().ok_or(ErrorCode::InvalidName)?;
                p.extend(rest);
                Ok(p)
            }
        }
    }

    /// A path to read as a file: under the root on the host side, a picked file on the viewer side.
    fn readable(&self, v: &str) -> Result<PathBuf, ErrorCode> {
        let parts = components(v)?;
        match &self.roots {
            Roots::Home { home, .. } => {
                let mut p = home.clone();
                p.extend(&parts);
                Ok(p)
            }
            Roots::Tokens(t) => {
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
            FsCall::PartCreate { dir, name } => done(fs.part_create(&self.folder(&dir.0)?, &name.0)),
            FsCall::PartTruncate { dir, name, len } => done(fs.part_truncate(&self.folder(&dir.0)?, &name.0, len)),
            FsCall::PartAppend { dir, name, offset, data } => {
                done(fs.part_append(&self.folder(&dir.0)?, &name.0, offset, &data.0))
            }
            FsCall::PartLen { dir, name } => Ok(FsAnswer::Len(fs.part_len(&self.folder(&dir.0)?, &name.0)?)),
            FsCall::PartRead { dir, name, offset, len } => {
                Ok(FsAnswer::Data(Bytes(fs.part_read(&self.folder(&dir.0)?, &name.0, offset, len)?)))
            }
            FsCall::PartSync { dir, name, final_mtime_ms } => {
                done(fs.part_sync(&self.folder(&dir.0)?, &name.0, final_mtime_ms))
            }
            FsCall::PartRename { dir, name, target_name } => {
                done(fs.part_rename(&self.folder(&dir.0)?, &name.0, &target_name.0))
            }
            FsCall::RemoveFile { dir, name, kind } => {
                done(fs.remove_file(&self.folder(&dir.0)?, &name.0, kind_of(kind)))
            }
            FsCall::MetaRead { dir, name } => {
                Ok(FsAnswer::Meta(fs.meta_read(&self.folder(&dir.0)?, &name.0)?.map(Bytes)))
            }
            FsCall::MetaWrite { dir, name, data } => done(fs.meta_write(&self.folder(&dir.0)?, &name.0, &data.0)),
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
