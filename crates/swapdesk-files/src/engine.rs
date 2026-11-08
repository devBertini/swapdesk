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

use swapdesk_proto::{
    DirListing, DirRequest, ErrorCode, ErrorMessage, FileAccept, FileCancel, FileDecline, FileDone, FileGet, FileOffer,
    FilePause, FileResume, FileVerified, Message,
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
    },
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
    /// Viewer: the folder chosen for each `FileGet` still waiting for its offer.
    gets: HashMap<u32, PathBuf>,
    dir_reqs: HashSet<u32>,
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
        Self { cfg, fs, transfers: HashMap::new(), gets: HashMap::new(), dir_reqs: HashSet::new() }
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
        self.gets.insert(id, save_dir);
        vec![Out::Msg(Message::FileGet(FileGet { transfer_id: id, path }))]
    }

    /// Viewer: asks for a folder of the host (`None` is its home).
    pub fn request_dir(&mut self, req_id: u32, path: Option<String>) -> Vec<Out> {
        self.dir_reqs.insert(req_id);
        vec![Out::Msg(Message::DirRequest(DirRequest { req_id, path }))]
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
            _ => Vec::new(),
        }
    }

    fn on_offer(&mut self, m: FileOffer) -> Vec<Out> {
        let id = m.transfer_id;
        let decline = |code| vec![Out::Msg(Message::FileDecline(FileDecline { transfer_id: id, code }))];
        // A viewer accepts a file only as the answer to its own `FileGet`; `dest` is ignored there.
        let dir = match self.cfg.side {
            Side::Viewer => match self.gets.remove(&id) {
                Some(dir) => dir,
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
        match Incoming::open_with(Arc::clone(&self.fs), &dir, &m.name, m.size, m.mtime_ms) {
            Ok(inc) => {
                let resume_from = inc.resume_from();
                self.transfers.insert(id, Transfer::Rx(Box::new(Rx { inc, done_sha: None, paused: false })));
                vec![
                    Out::Event(Event::Incoming { id, name: m.name, size: m.size, resume_from }),
                    Out::Msg(Message::FileAccept(FileAccept { transfer_id: id, resume_from })),
                ]
            }
            Err(code) => decline(code),
        }
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
            vec![Out::Event(Event::Listing {
                req_id: m.req_id,
                path: m.path,
                entries: m.entries,
                truncated: m.truncated,
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
        match Outgoing::open_with(Arc::clone(&self.fs), std::path::Path::new(&m.path)) {
            Ok(out) => {
                let offer = out.offer(id, None);
                self.transfers.insert(id, Transfer::Tx(Tx { out, state: TxState::Offered }));
                vec![Out::Msg(Message::FileOffer(offer))]
            }
            Err(code) => vec![error_msg(code, 0x040B, id)],
        }
    }
}
