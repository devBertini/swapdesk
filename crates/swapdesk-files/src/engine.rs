//! The state machine of the file messages (PROTOCOL.md 5.3, 0x0401 to 0x040B), with no network and no task: it takes
//! a message or a local command and answers with [`Out`] items the caller carries out (send a message, open or stop a
//! data stream, tell the UI). The disk work it does (partial files, listings) is blocking, so a caller on an async
//! runtime reaches it through `spawn_blocking`.
//!
//! Resume has two layers. Inside a live session `FilePause` / `FileResume` / `FileAccept` realign the sender to the
//! receiver's verified length. After the link died, the session (or the user's "Retomar") offers the same file again:
//! the receiver finds its partial file by name, size and mtime and answers `FileAccept { resume_from }`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use swapdesk_ipc::VolumesView;
use swapdesk_proto::{
    DirEntry2, DirListing, DirPage, DirPageListing, DirRequest, ErrorCode, ErrorMessage, FileAccept, FileCancel,
    FileDecline, FileDone, FileGet, FileOffer, FilePause, FileResume, FileVerified, FsOp, FsOpKind, FsOpResult,
    MAX_FRAME_PAYLOAD, Message, OpItem, Place, Volume, VolumeKind, VolumesList, VolumesRequest, Wire,
};

use crate::disk::Fs;
use crate::listing::{list_dir_with, resolve_request};
use crate::local::LocalFs;
use crate::name::validate_name;
use crate::recv::{FinishError, Incoming};
use crate::send::{ChunkReader, Outgoing};

/// Largest file a receiver accepts unless configured otherwise (GUESS: 1 TiB; a bigger offer is `TOO_LARGE`).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1 << 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Host,
    Viewer,
}

#[derive(Debug, Clone)]
pub struct FilesConfig {
    pub side: Side,
    /// Host: where a `FileOffer` without `dest` is saved (none means it is declined `UNAVAILABLE`).
    pub default_dir: Option<PathBuf>,
    /// Host: what a `DirRequest` without a path lists.
    pub home: PathBuf,
    pub max_file_size: u64,
}

impl FilesConfig {
    pub fn new(side: Side, home: PathBuf) -> Self {
        Self { side, default_dir: None, home, max_file_size: DEFAULT_MAX_FILE_SIZE }
    }
}

/// What the UI is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A file is coming in (`resume_from` > 0: continuing an earlier try).
    Incoming {
        id: u32,
        name: String,
        size: u64,
        resume_from: u64,
    },
    /// Bytes moved: `done` of `total` (counted from the start of the file).
    Progress {
        id: u32,
        done: u64,
        total: u64,
    },
    /// A received file passed its SHA-256 check and is at `path`.
    Received {
        id: u32,
        path: PathBuf,
    },
    /// A sent file was received and verified.
    Sent {
        id: u32,
    },
    /// The transfer is paused (by either side or by the link); `Retomar` resumes it.
    Paused {
        id: u32,
    },
    Failed {
        id: u32,
        code: ErrorCode,
    },
    Cancelled {
        id: u32,
    },
    Listing {
        req_id: u32,
        path: String,
        entries: Vec<swapdesk_proto::DirEntry>,
        truncated: bool,
        /// Entries left out because their name is not one valid component (see [`keep_valid_names`]).
        dropped: u32,
    },
    /// Viewer: a page of the host's folder (the answer to a `DirPage`).
    PageListing {
        req_id: u32,
        path: String,
        start: u32,
        entries: Vec<DirEntry2>,
        next: Option<u32>,
        free_bytes: Option<u64>,
        /// Entries left out because their name is not one valid component (see [`keep_valid_names`]).
        dropped: u32,
    },
    /// Viewer: the host's home, places and volumes; `req_id` 0 when the host pushed it because the set changed.
    Volumes(VolumesList),
    /// Viewer: the outcome of a change, one item per name.
    OpResult {
        op_id: u32,
        results: Vec<OpItem>,
    },
    /// Host, for the audit (FILES-UI.md 8.3, tag 19): one change message done. `op` 0 new folder, 1 rename, 2 move,
    /// 3 to the bin; `count` the names done; `area` 0 the home folder, 1 elsewhere on the system volume, 2 another
    /// volume.
    Changed {
        op: u8,
        count: u32,
        area: u8,
    },
    /// Host, for the audit (tag 20): the viewer reached a new kind of volume; `kinds` is every kind reached so far
    /// (bit 0 the system volume outside home, 1 fixed, 2 removable, 3 network, 4 optical or image).
    Reach {
        kinds: u8,
    },
}

/// Most `FsOp` messages of one session in flight (FILES-UI.md 8.4).
pub const MAX_FS_OPS: usize = 4;

/// The answer to an `FsOp` that is one too many: `BUSY` for every name, in order.
pub fn busy_result(op: &FsOp) -> Message {
    failed_result(op, ErrorCode::Busy)
}

/// An answer that gives `code` to every name of `op`, in order.
pub fn failed_result(op: &FsOp, code: ErrorCode) -> Message {
    let results = op.op.names().into_iter().map(|n| OpItem { name: n.to_string(), code }).collect();
    Message::FsOpResult(FsOpResult { op_id: op.op_id, results })
}

/// The disk work of one `FsOp`, apart from the engine: it holds only the disk, so it runs without the engine's lock
/// and a long change (a copy across volumes) leaves the session's other messages alone.
pub struct OpJob {
    fs: Arc<dyn Fs>,
    op: FsOp,
    /// Raised when the permission for the change is gone or the session ended: the names not begun are left alone.
    cancel: Arc<AtomicBool>,
}

impl OpJob {
    pub fn op(&self) -> &FsOp {
        &self.op
    }

    /// The flag [`OpJob::run`] looks at before each name. Raising it stops the change at the next name (one already
    /// begun finishes: a move that started ends copied and verified or takes its copy away, never half).
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Does the change: each name its own call, in order, and its own result; what was done stays done. A name not
    /// begun when the cancel flag is up is not touched and its result is `UNAVAILABLE`. Blocks.
    pub fn run(&self) -> Vec<OpItem> {
        use std::path::Path;
        let fs = &self.fs;
        let item = |name: &str, r: Result<(), ErrorCode>| OpItem {
            name: name.to_string(),
            code: r.err().unwrap_or(ErrorCode::NoError),
        };
        // Each name: its own result, from the call unless the change was cancelled before the name began.
        let each = |name: &str, call: &dyn Fn() -> Result<(), ErrorCode>| {
            if self.cancel.load(Ordering::SeqCst) {
                item(name, Err(ErrorCode::Unavailable))
            } else {
                item(name, call())
            }
        };
        match &self.op.op {
            FsOpKind::MakeDir { parent, name } => vec![each(name, &|| fs.make_dir(Path::new(parent), name))],
            FsOpKind::Rename { dir, name, new_name } => {
                vec![each(name, &|| fs.rename(Path::new(dir), name, new_name))]
            }
            FsOpKind::Move { from_dir, names, to_dir } => names
                .iter()
                .map(|n| each(&n.name, &|| fs.move_into(Path::new(from_dir), &n.name, Path::new(to_dir)).map(|_| ())))
                .collect(),
            FsOpKind::Trash { dir, names } => {
                names.iter().map(|n| each(&n.name, &|| fs.trash(Path::new(dir), &n.name))).collect()
            }
        }
    }
}

/// What the caller must do.
#[derive(Debug)]
pub enum Out {
    Msg(Message),
    /// Open a data stream for the transfer and send from `from` (call [`Engine::open_reader`]).
    Send {
        id: u32,
        from: u64,
    },
    /// Stop the transfer's data stream if one runs.
    Stop {
        id: u32,
    },
    Event(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxState {
    /// Offered, waiting for `FileAccept`.
    Offered,
    Sending,
    Paused,
    /// `FileDone` sent, waiting for `FileVerified`.
    Awaiting,
}

struct Tx {
    out: Outgoing,
    state: TxState,
}

/// A `FileGet` waiting for its offer.
struct PendingGet {
    save_dir: PathBuf,
    /// The last component of the path asked for: the only name the offer may carry.
    wanted: String,
}

struct Rx {
    inc: Incoming,
    done_sha: Option<[u8; 32]>,
    paused: bool,
}

enum Transfer {
    Tx(Tx),
    Rx(Box<Rx>),
}

pub struct Engine {
    cfg: FilesConfig,
    fs: Arc<dyn Fs>,
    transfers: HashMap<u32, Transfer>,
    /// Viewer: the folder chosen and the file name asked for, for each `FileGet` still waiting for its offer.
    gets: HashMap<u32, PendingGet>,
    dir_reqs: HashSet<u32>,
    /// Viewer: the explorer's requests waiting for their answer.
    page_reqs: HashSet<u32>,
    vol_reqs: HashSet<u32>,
    ops: HashSet<u32>,
    /// Host: the volume kinds the viewer reached so far (FILES-UI.md 8.3, the bits of `Event::Reach`).
    reach: u8,
    /// Host: the volumes as the disk last said, to tell the kind of a volume path.
    kinds: Vec<(u32, VolumeKind)>,
}

/// The entries of a host's listing whose name is one valid component, and how many were left out. A name that is
/// a path (`..\x`, `C:\x`, `/x`) must never reach a viewer's disk, so it never reaches the viewer's interface.
pub(crate) fn keep_valid_names<T>(entries: Vec<T>, name: impl Fn(&T) -> &str) -> (Vec<T>, u32) {
    let total = entries.len();
    let kept: Vec<T> = entries.into_iter().filter(|e| validate_name(name(e)).is_ok()).collect();
    let dropped = u32::try_from(total - kept.len()).unwrap_or(u32::MAX);
    (kept, dropped)
}

fn error_msg(code: ErrorCode, about_type: u16, about_id: u32) -> Out {
    Out::Msg(Message::Error(ErrorMessage { code, fatal: false, about_type, about_id }))
}

fn failed(id: u32, code: ErrorCode) -> Out {
    Out::Event(Event::Failed { id, code })
}

impl Engine {
    /// An engine over this machine's disk ([`LocalFs`]).
    pub fn new(cfg: FilesConfig) -> Self {
        Self::with_fs(cfg, Arc::new(LocalFs::new()))
    }

    /// An engine whose every disk access goes through `fs`.
    pub fn with_fs(cfg: FilesConfig, fs: Arc<dyn Fs>) -> Self {
        Self {
            cfg,
            fs,
            transfers: HashMap::new(),
            gets: HashMap::new(),
            dir_reqs: HashSet::new(),
            page_reqs: HashSet::new(),
            vol_reqs: HashSet::new(),
            ops: HashSet::new(),
            reach: 0,
            kinds: Vec::new(),
        }
    }

    pub fn side(&self) -> Side {
        self.cfg.side
    }

    pub fn active(&self) -> usize {
        self.transfers.len()
    }

    /// Does the engine hold transfer `id` (running, paused, or a `FileGet` still waiting for its offer)?
    pub fn has(&self, id: u32) -> bool {
        self.transfers.contains_key(&id) || self.gets.contains_key(&id)
    }

    // ---------- local commands ----------

    /// Offers the file at `path`. `dest` is the folder on the receiving host (viewer to host only).
    pub fn send_file(&mut self, id: u32, path: &std::path::Path, dest: Option<String>) -> Vec<Out> {
        if self.transfers.contains_key(&id) || self.gets.contains_key(&id) {
            return vec![failed(id, ErrorCode::Busy)];
        }
        match Outgoing::open_with(Arc::clone(&self.fs), path) {
            Ok(out) => {
                let dest = if self.cfg.side == Side::Viewer { dest } else { None };
                let offer = out.offer(id, dest);
                self.transfers.insert(id, Transfer::Tx(Tx { out, state: TxState::Offered }));
                vec![Out::Msg(Message::FileOffer(offer))]
            }
            Err(code) => vec![failed(id, code)],
        }
    }

    /// Viewer: asks the host for `path`, to be saved into `save_dir`. Only the offer that answers this id is accepted.
    pub fn request_get(&mut self, id: u32, path: String, save_dir: PathBuf) -> Vec<Out> {
        if self.cfg.side != Side::Viewer || self.transfers.contains_key(&id) || self.gets.contains_key(&id) {
            return vec![failed(id, ErrorCode::Busy)];
        }
        let wanted = path.rsplit(['/', '\u{5c}']).next().unwrap_or_default().to_string();
        self.gets.insert(id, PendingGet { save_dir, wanted });
        vec![Out::Msg(Message::FileGet(FileGet { transfer_id: id, path }))]
    }

    /// Viewer: asks for a folder of the host (`None` is its home).
    pub fn request_dir(&mut self, req_id: u32, path: Option<String>) -> Vec<Out> {
        self.dir_reqs.insert(req_id);
        vec![Out::Msg(Message::DirRequest(DirRequest { req_id, path }))]
    }

    /// Viewer: asks for a page of a folder of the host (a 1.1 path).
    pub fn request_page(&mut self, req_id: u32, path: String, start: u32, max: u16) -> Vec<Out> {
        if self.cfg.side != Side::Viewer {
            return Vec::new();
        }
        self.page_reqs.insert(req_id);
        vec![Out::Msg(Message::DirPage(DirPage { req_id, path, start, max }))]
    }

    /// Viewer: asks for the host's home, places and volumes.
    pub fn request_volumes(&mut self, req_id: u32) -> Vec<Out> {
        if self.cfg.side != Side::Viewer {
            return Vec::new();
        }
        self.vol_reqs.insert(req_id);
        vec![Out::Msg(Message::VolumesRequest(VolumesRequest { req_id }))]
    }

    /// Viewer: asks the host to change its files.
    pub fn request_op(&mut self, op_id: u32, op: FsOpKind) -> Vec<Out> {
        if self.cfg.side != Side::Viewer {
            return Vec::new();
        }
        self.ops.insert(op_id);
        vec![Out::Msg(Message::FsOp(FsOp { op_id, op }))]
    }

    /// Host: the volume list as it is now, to push to the viewer (`req_id` 0) when the set changed.
    pub fn volumes_push(&mut self) -> Vec<Out> {
        if self.cfg.side != Side::Host {
            return Vec::new();
        }
        match self.volumes_list(0) {
            Ok(list) => vec![Out::Msg(Message::VolumesList(list))],
            Err(_) => Vec::new(),
        }
    }

    pub fn pause(&mut self, id: u32) -> Vec<Out> {
        let mut outs = Vec::new();
        match self.transfers.get_mut(&id) {
            Some(Transfer::Tx(t)) if matches!(t.state, TxState::Sending | TxState::Offered) => {
                t.state = TxState::Paused;
                outs.push(Out::Stop { id });
            }
            Some(Transfer::Rx(r)) if !r.paused => {
                r.paused = true;
                r.inc.suspend_keep_open();
                outs.push(Out::Stop { id });
            }
            _ => return outs,
        }
        outs.push(Out::Msg(Message::FilePause(FilePause { transfer_id: id })));
        outs.push(Out::Event(Event::Paused { id }));
        outs
    }

    /// "Retomar". A receiver answers itself with `FileAccept` at its verified length; a sender asks with
    /// `FileResume` and the receiver answers.
    pub fn resume(&mut self, id: u32) -> Vec<Out> {
        match self.transfers.get_mut(&id) {
            Some(Transfer::Rx(r)) => {
                r.paused = false;
                match r.inc.realign() {
                    Ok(from) => vec![Out::Msg(Message::FileAccept(FileAccept { transfer_id: id, resume_from: from }))],
                    Err(code) => self.fail_rx(id, code),
                }
            }
            Some(Transfer::Tx(t)) if t.state == TxState::Paused => {
                t.state = TxState::Offered;
                vec![Out::Msg(Message::FileResume(FileResume { transfer_id: id }))]
            }
            _ => Vec::new(),
        }
    }

    pub fn cancel(&mut self, id: u32) -> Vec<Out> {
        self.gets.remove(&id);
        let Some(t) = self.transfers.remove(&id) else { return Vec::new() };
        if let Transfer::Rx(mut r) = t {
            r.inc.discard();
        }
        vec![
            Out::Stop { id },
            Out::Msg(Message::FileCancel(FileCancel { transfer_id: id })),
            Out::Event(Event::Cancelled { id }),
        ]
    }

    /// The link is gone: nothing continues until the file is offered again over a new one. Partial files stay on
    /// disk with their verified length.
    pub fn link_lost(&mut self) -> Vec<Out> {
        let mut outs = Vec::new();
        for (id, t) in self.transfers.drain() {
            if let Transfer::Rx(mut r) = t {
                r.inc.suspend();
            }
            outs.push(Out::Event(Event::Paused { id }));
        }
        self.gets.clear();
        self.dir_reqs.clear();
        self.page_reqs.clear();
        self.vol_reqs.clear();
        self.ops.clear();
        outs
    }

    // ---------- data path ----------

    /// The reader for the data stream of an accepted outgoing transfer.
    pub fn open_reader(&mut self, id: u32, from: u64) -> Result<ChunkReader, ErrorCode> {
        match self.transfers.get_mut(&id) {
            Some(Transfer::Tx(t)) => {
                let r = t.out.reader(from)?;
                t.state = TxState::Sending;
                Ok(r)
            }
            _ => Err(ErrorCode::NotFound),
        }
    }

    /// Everything of the file went out on the data stream (and the stream finished): `FileDone`.
    pub fn sent_all(&mut self, id: u32, sha256: [u8; 32]) -> Vec<Out> {
        match self.transfers.get_mut(&id) {
            Some(Transfer::Tx(t)) if t.state == TxState::Sending => {
                t.state = TxState::Awaiting;
                vec![Out::Msg(Message::FileDone(FileDone { transfer_id: id, sha256 }))]
            }
            _ => Vec::new(),
        }
    }

    /// The sender could not read or write the file: the transfer fails and the peer is told.
    pub fn send_failed(&mut self, id: u32, code: ErrorCode) -> Vec<Out> {
        if self.transfers.remove(&id).is_none() {
            return Vec::new();
        }
        vec![
            error_msg(code, 0x0404, id),
            Out::Msg(Message::FileCancel(FileCancel { transfer_id: id })),
            failed(id, code),
        ]
    }

    /// Bytes of a data stream (`offset` is the stream header's). Fails the transfer on a wrong offset, more bytes than
    /// offered, or a disk error.
    pub fn on_data(&mut self, id: u32, offset: u64, bytes: &[u8]) -> Vec<Out> {
        let Some(Transfer::Rx(r)) = self.transfers.get_mut(&id) else { return Vec::new() };
        if r.paused {
            return Vec::new();
        }
        // A stream that was stopped may still deliver bytes the file already has: drop what is behind the end of the
        // partial file and keep only the part that continues it.
        let have = r.inc.received();
        let end = offset.saturating_add(bytes.len() as u64);
        if end <= have {
            return Vec::new();
        }
        let (offset, bytes) = if offset < have {
            (have, &bytes[usize::try_from(have - offset).unwrap_or(bytes.len())..])
        } else {
            (offset, bytes)
        };
        match r.inc.write(offset, bytes) {
            Ok(()) => vec![Out::Event(Event::Progress { id, done: r.inc.received(), total: r.inc.size() })],
            Err(code) => self.fail_rx(id, code),
        }
    }

    /// A data stream ended cleanly: when `FileDone` already came, finish the file.
    pub fn data_ended(&mut self, id: u32) -> Vec<Out> {
        self.try_finish(id)
    }

    /// A data stream broke (the link, or the sender stopped it): the received bytes become the verified length. The
    /// state does not change: a pause or a lost link is reported by its own message, and a stale stream must not
    /// stop the one that replaced it.
    pub fn data_broken(&mut self, id: u32) -> Vec<Out> {
        if let Some(Transfer::Rx(r)) = self.transfers.get_mut(&id) {
            r.inc.suspend_keep_open();
        }
        Vec::new()
    }

    /// Size of an outgoing transfer.
    pub fn total_of(&self, id: u32) -> Option<u64> {
        match self.transfers.get(&id) {
            Some(Transfer::Tx(t)) => Some(t.out.size()),
            _ => None,
        }
    }

    fn fail_rx(&mut self, id: u32, code: ErrorCode) -> Vec<Out> {
        if let Some(Transfer::Rx(mut r)) = self.transfers.remove(&id) {
            if matches!(code, ErrorCode::TooLarge | ErrorCode::ProtocolViolation) {
                r.inc.discard();
            } else {
                r.inc.suspend();
            }
        }
        vec![
            error_msg(code, 0x0401, id),
            Out::Msg(Message::FileCancel(FileCancel { transfer_id: id })),
            failed(id, code),
        ]
    }

    fn try_finish(&mut self, id: u32) -> Vec<Out> {
        let Some(Transfer::Rx(r)) = self.transfers.get_mut(&id) else { return Vec::new() };
        let Some(sha) = r.done_sha else { return Vec::new() };
        match r.inc.finish(&sha) {
            Ok(path) => {
                self.transfers.remove(&id);
                vec![
                    Out::Msg(Message::FileVerified(FileVerified { transfer_id: id, ok: true })),
                    Out::Event(Event::Received { id, path }),
                ]
            }
            // The data stream may still be on its way: wait for `data_ended`.
            Err(FinishError::Incomplete) => Vec::new(),
            Err(e) => {
                self.transfers.remove(&id);
                let code = ErrorCode::IoFailed;
                let _ = e;
                vec![Out::Msg(Message::FileVerified(FileVerified { transfer_id: id, ok: false })), failed(id, code)]
            }
        }
    }

    // ---------- the peer's messages ----------

    pub fn on_message(&mut self, msg: Message) -> Vec<Out> {
        match msg {
            Message::FileOffer(m) => self.on_offer(m),
            Message::FileAccept(m) => self.on_accept(m),
            Message::FileDecline(FileDecline { transfer_id, code }) => {
                self.gets.remove(&transfer_id);
                match self.transfers.remove(&transfer_id) {
                    Some(_) => vec![Out::Stop { id: transfer_id }, failed(transfer_id, code)],
                    None => Vec::new(),
                }
            }
            Message::FileDone(m) => match self.transfers.get_mut(&m.transfer_id) {
                Some(Transfer::Rx(r)) => {
                    r.done_sha = Some(m.sha256);
                    self.try_finish(m.transfer_id)
                }
                _ => vec![error_msg(ErrorCode::NotFound, 0x0404, m.transfer_id)],
            },
            Message::FileVerified(m) => match self.transfers.get(&m.transfer_id) {
                Some(Transfer::Tx(t)) if t.state == TxState::Awaiting => {
                    self.transfers.remove(&m.transfer_id);
                    vec![if m.ok {
                        Out::Event(Event::Sent { id: m.transfer_id })
                    } else {
                        failed(m.transfer_id, ErrorCode::IoFailed)
                    }]
                }
                _ => Vec::new(),
            },
            Message::FilePause(m) => match self.transfers.get_mut(&m.transfer_id) {
                Some(Transfer::Tx(t)) if matches!(t.state, TxState::Sending | TxState::Offered) => {
                    t.state = TxState::Paused;
                    vec![Out::Stop { id: m.transfer_id }, Out::Event(Event::Paused { id: m.transfer_id })]
                }
                Some(Transfer::Rx(r)) if !r.paused => {
                    r.paused = true;
                    r.inc.suspend_keep_open();
                    vec![Out::Event(Event::Paused { id: m.transfer_id })]
                }
                _ => Vec::new(),
            },
            Message::FileResume(m) => match self.transfers.get_mut(&m.transfer_id) {
                // The receiver answers a resume with `FileAccept` at its verified length.
                Some(Transfer::Rx(_)) => self.resume(m.transfer_id),
                _ => Vec::new(),
            },
            Message::FileCancel(m) => {
                self.gets.remove(&m.transfer_id);
                match self.transfers.remove(&m.transfer_id) {
                    Some(t) => {
                        if let Transfer::Rx(mut r) = t {
                            r.inc.discard();
                        }
                        vec![Out::Stop { id: m.transfer_id }, Out::Event(Event::Cancelled { id: m.transfer_id })]
                    }
                    None => Vec::new(),
                }
            }
            Message::DirRequest(m) => self.on_dir_request(m),
            Message::DirListing(m) => self.on_listing(m),
            Message::FileGet(m) => self.on_get(m),
            Message::DirPage(m) => self.on_dir_page(m),
            Message::DirPageListing(m) => self.on_page_listing(m),
            Message::VolumesRequest(m) => self.on_volumes_request(m),
            Message::VolumesList(m) => self.on_volumes_list(m),
            Message::FsOp(m) => self.on_fs_op(m),
            Message::FsOpResult(m) => self.on_op_result(m),
            _ => Vec::new(),
        }
    }

    fn on_offer(&mut self, m: FileOffer) -> Vec<Out> {
        let id = m.transfer_id;
        let decline = |code| vec![Out::Msg(Message::FileDecline(FileDecline { transfer_id: id, code }))];
        // A viewer accepts a file only as the answer to its own `FileGet`; `dest` is ignored there.
        let dir = match self.cfg.side {
            Side::Viewer => match self.gets.remove(&id) {
                // The file saved is the file asked for: a host that answers `a.pdf` with `a.pdf.exe` is refused, so the
                // name the viewer's user chose is the name that lands on their disk.
                Some(get) if get.wanted != m.name => {
                    let mut outs = decline(ErrorCode::InvalidName);
                    outs.push(failed(id, ErrorCode::InvalidName));
                    return outs;
                }
                Some(get) => get.save_dir,
                None => return decline(ErrorCode::NotPermitted),
            },
            Side::Host => {
                if self.transfers.contains_key(&id) {
                    return decline(ErrorCode::ProtocolViolation);
                }
                match m.dest.as_deref().map(PathBuf::from).or_else(|| self.cfg.default_dir.clone()) {
                    Some(d) => d,
                    None => return decline(ErrorCode::Unavailable),
                }
            }
        };
        if let Err(code) = validate_name(&m.name) {
            return decline(code);
        }
        if m.size > self.cfg.max_file_size {
            return decline(ErrorCode::TooLarge);
        }
        if self.transfers.contains_key(&id) {
            return decline(ErrorCode::ProtocolViolation);
        }
        let mut outs: Vec<Out> = match (&m.dest, self.cfg.side) {
            (Some(dest), Side::Host) => self.note_reach(dest).into_iter().collect(),
            _ => Vec::new(),
        };
        match Incoming::open_with(Arc::clone(&self.fs), &dir, &m.name, m.size, m.mtime_ms) {
            Ok(inc) => {
                let resume_from = inc.resume_from();
                self.transfers.insert(id, Transfer::Rx(Box::new(Rx { inc, done_sha: None, paused: false })));
                outs.push(Out::Event(Event::Incoming { id, name: m.name, size: m.size, resume_from }));
                outs.push(Out::Msg(Message::FileAccept(FileAccept { transfer_id: id, resume_from })));
            }
            Err(code) => outs.extend(decline(code)),
        }
        outs
    }

    fn on_accept(&mut self, m: FileAccept) -> Vec<Out> {
        let id = m.transfer_id;
        let Some(Transfer::Tx(t)) = self.transfers.get_mut(&id) else { return Vec::new() };
        if t.state == TxState::Awaiting {
            return Vec::new();
        }
        if m.resume_from > t.out.size() {
            self.transfers.remove(&id);
            return vec![
                Out::Stop { id },
                error_msg(ErrorCode::ProtocolViolation, 0x0402, id),
                Out::Msg(Message::FileCancel(FileCancel { transfer_id: id })),
                failed(id, ErrorCode::ProtocolViolation),
            ];
        }
        let was_sending = t.state == TxState::Sending;
        t.state = TxState::Sending;
        let mut outs = Vec::new();
        if was_sending {
            outs.push(Out::Stop { id });
        }
        outs.push(Out::Send { id, from: m.resume_from });
        outs
    }

    fn on_dir_request(&mut self, m: DirRequest) -> Vec<Out> {
        if self.cfg.side != Side::Host {
            return vec![error_msg(ErrorCode::NotPermitted, 0x0409, m.req_id)];
        }
        let path = resolve_request(m.path.as_deref(), &self.cfg.home);
        match list_dir_with(self.fs.as_ref(), &path) {
            Ok(l) => vec![Out::Msg(Message::DirListing(DirListing {
                req_id: m.req_id,
                path: l.path,
                entries: l.entries,
                truncated: l.truncated,
            }))],
            Err(code) => vec![error_msg(code, 0x0409, m.req_id)],
        }
    }

    fn on_listing(&mut self, m: DirListing) -> Vec<Out> {
        if self.cfg.side == Side::Viewer && self.dir_reqs.remove(&m.req_id) {
            let (entries, dropped) = keep_valid_names(m.entries, |e| &e.name);
            vec![Out::Event(Event::Listing {
                req_id: m.req_id,
                path: m.path,
                entries,
                truncated: m.truncated,
                dropped,
            })]
        } else {
            Vec::new()
        }
    }

    fn on_get(&mut self, m: FileGet) -> Vec<Out> {
        let id = m.transfer_id;
        if self.cfg.side != Side::Host {
            return vec![error_msg(ErrorCode::NotPermitted, 0x040B, id)];
        }
        if self.transfers.contains_key(&id) {
            return vec![error_msg(ErrorCode::Busy, 0x040B, id)];
        }
        let mut outs: Vec<Out> = self.note_reach(&m.path).into_iter().collect();
        match Outgoing::open_with(Arc::clone(&self.fs), std::path::Path::new(&m.path)) {
            Ok(out) => {
                let offer = out.offer(id, None);
                self.transfers.insert(id, Transfer::Tx(Tx { out, state: TxState::Offered }));
                outs.push(Out::Msg(Message::FileOffer(offer)));
            }
            Err(code) => outs.push(error_msg(code, 0x040B, id)),
        }
        outs
    }

    // ---------- the explorer's messages (minor 2, FILES-UI.md 7.3) ----------

    /// The kind of the volume a 1.1 path starts on (`None` for a home path or a volume the disk does not publish).
    fn kind_of_path(&mut self, path: &str) -> Option<VolumeKind> {
        let id: u32 = path.strip_prefix(':')?.split('/').next()?.parse().ok()?;
        if !self.kinds.iter().any(|(i, _)| *i == id) {
            let _ = self.volumes_list(0);
        }
        self.kinds.iter().find(|(i, _)| *i == id).map(|(_, k)| *k)
    }

    /// Host: the viewer reaches `path`; the first time it reaches a kind of volume, the audit hears of it.
    fn note_reach(&mut self, path: &str) -> Option<Out> {
        if self.cfg.side != Side::Host || !path.starts_with(':') {
            return None;
        }
        let bit = match self.kind_of_path(path)? {
            VolumeKind::System => 1,
            VolumeKind::Fixed => 1 << 1,
            VolumeKind::Removable => 1 << 2,
            VolumeKind::Network => 1 << 3,
            VolumeKind::Optical | VolumeKind::Image => 1 << 4,
        };
        if self.reach & bit != 0 {
            return None;
        }
        self.reach |= bit;
        Some(Out::Event(Event::Reach { kinds: self.reach }))
    }

    /// Host: the disk's home, places and volumes as the wire's `VolumesList`.
    fn volumes_list(&mut self, req_id: u32) -> Result<VolumesList, ErrorCode> {
        let VolumesView { home, places, volumes } = self.fs.volumes()?;
        self.kinds = volumes.iter().map(|v| (v.id, v.kind)).collect();
        Ok(VolumesList {
            req_id,
            home: home.0,
            places: places.into_iter().map(|p| Place { kind: p.kind, path: p.path.0 }).collect(),
            volumes: volumes
                .into_iter()
                .map(|v| Volume {
                    id: v.id,
                    kind: v.kind,
                    label: v.label.0,
                    letter: v.letter,
                    total_bytes: v.total_bytes,
                    free_bytes: v.free_bytes,
                    has_bin: v.has_bin,
                })
                .collect(),
        })
    }

    fn on_dir_page(&mut self, m: DirPage) -> Vec<Out> {
        if self.cfg.side != Side::Host {
            return vec![error_msg(ErrorCode::NotPermitted, 0x040C, m.req_id)];
        }
        let mut outs: Vec<Out> = self.note_reach(&m.path).into_iter().collect();
        let max = m.max.clamp(1, 2000);
        let page = match self.fs.list_page(std::path::Path::new(&m.path), m.start, max) {
            Ok(p) => p,
            Err(code) => {
                outs.push(error_msg(code, 0x040C, m.req_id));
                return outs;
            }
        };
        // The frame holds: req_id 4, the path (length prefix up to 2 bytes + bytes), start 4, the entry count (up to
        // 4), `next` (1 + 4) and `free_bytes` (1 + 8).
        let mut budget = MAX_FRAME_PAYLOAD.saturating_sub(4 + 2 + m.path.len() + 4 + 4 + 5 + 9);
        let mut next = page.next;
        let mut entries = Vec::new();
        for (i, e) in page.entries.into_iter().enumerate() {
            let entry = DirEntry2 { name: e.name.0, kind: e.kind, size: e.size, mtime_ms: e.mtime_ms, flags: e.flags };
            let Ok(bytes) = entry.encode() else { continue };
            if bytes.len() > budget {
                // The page ends before the frame would pass its limit; the next one starts at this entry.
                next = Some(m.start.saturating_add(u32::try_from(i).unwrap_or(u32::MAX)));
                break;
            }
            budget -= bytes.len();
            entries.push(entry);
        }
        outs.push(Out::Msg(Message::DirPageListing(DirPageListing {
            req_id: m.req_id,
            path: m.path,
            start: m.start,
            entries,
            next,
            free_bytes: page.free,
        })));
        outs
    }

    fn on_page_listing(&mut self, m: DirPageListing) -> Vec<Out> {
        if self.cfg.side == Side::Viewer && self.page_reqs.remove(&m.req_id) {
            let (entries, dropped) = keep_valid_names(m.entries, |e| &e.name);
            vec![Out::Event(Event::PageListing {
                req_id: m.req_id,
                path: m.path,
                start: m.start,
                entries,
                next: m.next,
                free_bytes: m.free_bytes,
                dropped,
            })]
        } else {
            Vec::new()
        }
    }

    fn on_volumes_request(&mut self, m: VolumesRequest) -> Vec<Out> {
        if self.cfg.side != Side::Host {
            return vec![error_msg(ErrorCode::NotPermitted, 0x040E, m.req_id)];
        }
        match self.volumes_list(m.req_id) {
            Ok(list) => vec![Out::Msg(Message::VolumesList(list))],
            Err(code) => vec![error_msg(code, 0x040E, m.req_id)],
        }
    }

    fn on_volumes_list(&mut self, m: VolumesList) -> Vec<Out> {
        // A push (`req_id` 0) is never asked for; anything else must answer a request of ours.
        if self.cfg.side == Side::Viewer && (m.req_id == 0 || self.vol_reqs.remove(&m.req_id)) {
            vec![Out::Event(Event::Volumes(m))]
        } else {
            Vec::new()
        }
    }

    /// Host: one change message, run to its end on this thread. Each name gets its own call, in order, and its own
    /// result; what was done stays done. The handler does not use this: it splits the work (see [`Engine::begin_op`]) so
    /// a long change never holds the session's message worker.
    fn on_fs_op(&mut self, m: FsOp) -> Vec<Out> {
        match self.begin_op(m) {
            Ok(job) => {
                let done = job.run();
                self.finish_op(&job, done)
            }
            Err(outs) => outs,
        }
    }

    /// Host: takes a change message apart. `Err` is the answer when there is nothing to run (this is not a host);
    /// `Ok` is the disk work, which [`OpJob::run`] does without the engine, and [`Engine::finish_op`] then turns into
    /// the answer and the audit events.
    pub fn begin_op(&mut self, m: FsOp) -> Result<OpJob, Vec<Out>> {
        if self.cfg.side != Side::Host {
            return Err(vec![error_msg(ErrorCode::NotPermitted, 0x0410, m.op_id)]);
        }
        Ok(OpJob { fs: Arc::clone(&self.fs), op: m, cancel: Arc::new(AtomicBool::new(false)) })
    }

    /// The audit's area of a 1.1 path: 0 the home folder, 1 elsewhere on the system volume, 2 another volume.
    fn area_of(&mut self, dir: &str) -> u8 {
        if !dir.starts_with(':') {
            0
        } else if self.kind_of_path(dir) == Some(VolumeKind::System) {
            1
        } else {
            2
        }
    }

    /// Host: the answer and the audit events of a change whose disk work is done.
    pub fn finish_op(&mut self, job: &OpJob, results: Vec<OpItem>) -> Vec<Out> {
        let (op, first_dir, other_dir) = match &job.op.op {
            FsOpKind::MakeDir { parent, .. } => (0u8, parent.as_str(), None),
            FsOpKind::Rename { dir, .. } => (1, dir.as_str(), None),
            FsOpKind::Move { from_dir, to_dir, .. } => (2, from_dir.as_str(), Some(to_dir.as_str())),
            FsOpKind::Trash { dir, .. } => (3, dir.as_str(), None),
        };
        let count = u32::try_from(results.iter().filter(|r| r.code == ErrorCode::NoError).count()).unwrap_or(u32::MAX);
        let mut outs: Vec<Out> = Vec::new();
        outs.extend(self.note_reach(first_dir));
        if let Some(d) = other_dir {
            outs.extend(self.note_reach(d));
        }
        // A move touches two places: the audit says the wider of the two (a file taken from the home folder to a
        // removable volume is a change of another volume, not of the home folder).
        let mut area = self.area_of(first_dir);
        if let Some(d) = other_dir {
            area = area.max(self.area_of(d));
        }
        outs.push(Out::Msg(Message::FsOpResult(FsOpResult { op_id: job.op.op_id, results })));
        if count > 0 {
            outs.push(Out::Event(Event::Changed { op, count, area }));
        }
        outs
    }

    fn on_op_result(&mut self, m: FsOpResult) -> Vec<Out> {
        if self.cfg.side == Side::Viewer && self.ops.remove(&m.op_id) {
            vec![Out::Event(Event::OpResult { op_id: m.op_id, results: m.results })]
        } else {
            Vec::new()
        }
    }
}
