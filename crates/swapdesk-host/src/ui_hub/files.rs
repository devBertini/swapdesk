//! Files in the hub (stage 44): the UI ops 40 to 45 and the notifications 20 and 21.
//!
//! * **Who may do what.** The file ops act on a viewer session of the connection that sent them (the UI that owns the
//!   session also serves its disk, C2.4 "Files"); the transfer numbers they hand out belong to that connection.
//! * **Paths.** The hub never sees a real path: a picked file is `p<token>/<name>` and a receive folder is
//!   `r<token>`, tokens the UI made; a folder of the host is whatever its listing said (relative to the host user's
//!   root). The engine in the service passes them on to the process that owns the disk.
//! * **Progress.** A transfer's progress reaches the UI at most four times a second; its first and last states always.
//! * **Resume.** The hub remembers how to start each transfer again. When a lost connection comes back (the reconnect
//!   of F-NET-01) every unfinished transfer that the user did not pause is offered again, and the receiver continues
//!   from its verified length; "Retomar" does the same for one transfer when the live connection no longer holds it.
//! * **Listings** are answered when the host's `DirListing` arrives (up to 8 s: the host answers a refused or failed
//!   request with nothing the protocol lets this side tell from silence).
//! * **The explorer** (ops 46 to 48, minor 2): a page, a change and the volume list go to the host as `DirPage`,
//!   `FsOp` and `VolumesRequest` and are answered when its message comes back (a change waits as long as the host may take, see `op_wait`). Toward a
//!   host of minor below 2 each answers `Invalid` at once, which is how the interface learns the peer is a 1.0 host.
//!   A volume list the host pushes unasked becomes the notification of kind 22. The host side's audit events of the
//!   explorer (a change done, a new kind of volume reached) are written here too: counts and kinds only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use swapdesk_files::{Command, Event};
use swapdesk_ipc::{
    ApiCode, FileEntry, FileListing, FileListing2, FilePick, FilesNote, FilesOp, FsChange, FsEntry2View, FsPage,
    Message as Ipc, Notify, OpResultView, PlaceView, ReplyBody, Text, TransferWire, VolumeView, VolumesView,
};
use swapdesk_proto::{FsName, FsOpKind};
use swapdesk_trust::{AuditEntry, AuditEvent, DeviceRef};

use super::ops::reply;
use super::views::api_of;
use super::{Conn, Hub, lock};
use crate::service::ViewerHandle;

/// Most transfers of one session at once (SPEC F-FILE-01, GUESS).
const MAX_TRANSFERS: usize = 8;
/// Progress reaches the UI at most this often per transfer.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// How long a listing waits for the host.
const LIST_WAIT: Duration = Duration::from_secs(8);
/// Slack on top of what the host may take, for the trip and the queue.
const OP_SLACK: Duration = Duration::from_secs(10);

/// How long a change waits for the host's answer (FILES-UI.md 7.4): what the host may take for every name (each its own
/// call, so `CALL_TIMEOUT`, or `MOVE_CALL_TIMEOUT` for a move) plus some slack. The host always answers inside that, with
/// the true result, so `Timeout` means the host went silent, never "a long move is still running".
fn op_wait(op: &FsOpKind) -> Duration {
    let (each, n) = match op {
        FsOpKind::Move { names, .. } => (swapdesk_files::MOVE_CALL_TIMEOUT, names.len()),
        FsOpKind::Trash { names, .. } => (swapdesk_files::CALL_TIMEOUT, names.len()),
        FsOpKind::MakeDir { .. } | FsOpKind::Rename { .. } => (swapdesk_files::CALL_TIMEOUT, 1),
    };
    each.saturating_mul(u32::try_from(n.max(1)).unwrap_or(u32::MAX)).saturating_add(OP_SLACK)
}
/// The first protocol minor that has the explorer's messages.
const EXPLORER_MINOR: u16 = 2;
/// How many times a reissue looks for the new connection's handler, and the pause between looks.
const REISSUE_TRIES: u32 = 100;
const REISSUE_PAUSE: Duration = Duration::from_millis(50);

/// How to start a transfer again.
#[derive(Clone)]
enum Start {
    Send {
        path: String,
        dest: Option<String>,
    },
    Get {
        remote_path: String,
        save_dir: String,
    },
    /// A transfer the other side started (host role): nothing to restart here.
    Theirs,
}

struct Rec {
    conn: u64,
    name: String,
    outgoing: bool,
    start: Start,
    total: u64,
    done: u64,
    /// The user paused it: a reconnect does not start it again.
    user_paused: bool,
    terminal: bool,
    last_sent: Option<Instant>,
    last_sent_done: u64,
}

/// What an answer from the host is waited for as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Waits {
    Dir,
    Page,
    Op,
    Volumes,
}

struct ListWait {
    conn: Arc<Conn>,
    req: u32,
    what: Waits,
}

/// The hub's file transfers, by session and transfer number, and the listings waiting for their answer.
#[derive(Default)]
pub(crate) struct FilesTable {
    recs: HashMap<(u64, u32), Rec>,
    lists: HashMap<u32, ListWait>,
}

fn last_component(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

impl Hub {
    fn files_note(&self, session_id: u64, note: FilesNote) {
        for c in self.conns_for_session(session_id) {
            c.queue.control(Ipc::UiNotify(Notify::Files(note.clone())));
        }
    }

    fn transfer_note(&self, session_id: u64, id: u32, name: &str, outgoing: bool, state: TransferWire) {
        self.files_note(
            session_id,
            FilesNote::Transfer { session_id, transfer_id: id, name: Text(name.to_string()), outgoing, state },
        );
    }

    fn viewer_of(&self, conn: &Conn, session_id: u64) -> Option<ViewerHandle> {
        lock(&conn.viewers).get(&session_id).map(|v| v.handle.clone())
    }

    /// The record of a transfer this connection started, with the session it belongs to.
    fn local_rec(&self, conn: &Conn, id: u32) -> Option<(u64, ViewerHandle)> {
        let session_id = lock(&self.files)
            .recs
            .iter()
            .find(|((_, i), r)| *i == id && r.conn == conn.id && !matches!(r.start, Start::Theirs))
            .map(|((s, _), _)| *s)?;
        Some((session_id, self.viewer_of(conn, session_id)?))
    }

    fn active_in(&self, session_id: u64) -> usize {
        lock(&self.files).recs.iter().filter(|((s, _), r)| *s == session_id && !r.terminal).count()
    }

    // ---------- the ops ----------

    /// One of the UI ops 40 to 45.
    pub(super) fn files_op(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, op: FilesOp) {
        match op {
            FilesOp::List { session_id, path } => self.files_list(conn, req, session_id, path.map(|t| t.0)),
            FilesOp::Send { session_id, picks, remote_dir } => {
                self.files_send(conn, req, session_id, picks, remote_dir.map(|t| t.0))
            }
            FilesOp::Get { session_id, remote_path, recv_token } => {
                self.files_get(conn, req, session_id, remote_path.0, recv_token)
            }
            FilesOp::Pause { transfer_id } => self.transfer_pause(conn, req, transfer_id),
            FilesOp::Resume { transfer_id } => self.transfer_resume(conn, req, transfer_id),
            FilesOp::Cancel { transfer_id } => self.transfer_cancel(conn, req, transfer_id),
            FilesOp::ListPage { session_id, path, start } => self.files_page(conn, req, session_id, path.0, start),
            FilesOp::Change { session_id, change } => self.files_change(conn, req, session_id, change),
            FilesOp::Volumes { session_id } => self.files_volumes(conn, req, session_id),
        }
    }

    /// The viewer session and its handle for an explorer op: `NotFound` for a session this connection does not own,
    /// `Invalid` toward a host of minor below 2.
    fn explorer_handle(&self, conn: &Conn, req: u32, session_id: u64) -> Option<ViewerHandle> {
        let Some(handle) = self.viewer_of(conn, session_id) else {
            reply(conn, req, Err(ApiCode::NotFound));
            return None;
        };
        if handle.ctx().peer_minor() < EXPLORER_MINOR {
            reply(conn, req, Err(ApiCode::Invalid));
            return None;
        }
        Some(handle)
    }

    /// Sends `cmd` to the session's engine and answers `Busy` when it takes none; otherwise the answer waits for the
    /// host's message, or `Timeout` after `wait`.
    fn explorer_ask(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        req: u32,
        handle: &ViewerHandle,
        what: Waits,
        wait: Duration,
        cmd: impl FnOnce(u32) -> Command,
    ) {
        let req_id = self.fresh_id();
        lock(&self.files).lists.insert(req_id, ListWait { conn: Arc::clone(conn), req, what });
        if !handle.files().command(cmd(req_id)) {
            lock(&self.files).lists.remove(&req_id);
            return reply(conn, req, Err(ApiCode::Busy));
        }
        if let Some(rt) = self.rt() {
            let me = Arc::clone(self);
            rt.spawn(async move {
                tokio::time::sleep(wait).await;
                if let Some(w) = lock(&me.files).lists.remove(&req_id) {
                    reply(&w.conn, w.req, Err(ApiCode::Timeout));
                }
            });
        }
    }

    /// `FilesListPage` (46): a page of a folder of the host, a 1.1 path.
    pub fn files_page(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64, path: String, start: u32) {
        let Some(handle) = self.explorer_handle(conn, req, session_id) else { return };
        self.explorer_ask(conn, req, &handle, Waits::Page, LIST_WAIT, |req_id| Command::DirPage {
            req_id,
            path,
            start,
            max: 2000,
        });
    }

    /// `FilesOp` (47): a change of the host's files, answered when the host's `FsOpResult` arrives.
    pub fn files_change(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64, change: FsChange) {
        let Some(handle) = self.explorer_handle(conn, req, session_id) else { return };
        let op = op_of(change);
        let wait = op_wait(&op);
        self.explorer_ask(conn, req, &handle, Waits::Op, wait, |op_id| Command::FsOp { op_id, op });
    }

    /// `FilesVolumes` (48): the host's home, places and volumes.
    pub fn files_volumes(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64) {
        let Some(handle) = self.explorer_handle(conn, req, session_id) else { return };
        self.explorer_ask(conn, req, &handle, Waits::Volumes, LIST_WAIT, |req_id| Command::Volumes { req_id });
    }

    /// The device of host session `session_id`, for the audit.
    fn host_session_device(&self, session_id: u64) -> Option<swapdesk_trust::DeviceId> {
        self.core()?.host().sessions().into_iter().find(|s| s.session_id == session_id).map(|s| s.device)
    }

    fn file_audit(&self, session_id: u64, event: AuditEvent) {
        let Some(device) = self.host_session_device(session_id) else { return };
        self.p.audit.append(AuditEntry {
            time_ms: self.p.wall.unix_ms(),
            device: DeviceRef::Known(device),
            event,
            duration_s: None,
        });
    }

    /// The waiter of `req_id` when it waits for `what`; one that waits for something else stays.
    fn take_wait(&self, req_id: u32, what: Waits) -> Option<ListWait> {
        let mut t = lock(&self.files);
        if t.lists.get(&req_id).is_some_and(|w| w.what == what) { t.lists.remove(&req_id) } else { None }
    }

    /// `FilesList`: the folder of the host, answered when its listing arrives.
    pub fn files_list(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64, path: Option<String>) {
        let Some(handle) = self.viewer_of(conn, session_id) else { return reply(conn, req, Err(ApiCode::NotFound)) };
        let req_id = self.fresh_id();
        lock(&self.files).lists.insert(req_id, ListWait { conn: Arc::clone(conn), req, what: Waits::Dir });
        if !handle.files().command(Command::Dir { req_id, path }) {
            lock(&self.files).lists.remove(&req_id);
            return reply(conn, req, Err(ApiCode::Busy));
        }
        if let Some(rt) = self.rt() {
            let me = Arc::clone(self);
            rt.spawn(async move {
                tokio::time::sleep(LIST_WAIT).await;
                if let Some(w) = lock(&me.files).lists.remove(&req_id) {
                    reply(&w.conn, w.req, Err(ApiCode::Timeout));
                }
            });
        }
    }

    /// `FilesSend`: one transfer per pick, into `remote_dir` of the host.
    pub fn files_send(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        req: u32,
        session_id: u64,
        picks: Vec<FilePick>,
        remote_dir: Option<String>,
    ) {
        let Some(handle) = self.viewer_of(conn, session_id) else { return reply(conn, req, Err(ApiCode::NotFound)) };
        if picks.is_empty() || picks.iter().any(|p| p.name.0.is_empty() || p.name.0.contains(['/', '\\'])) {
            return reply(conn, req, Err(ApiCode::Invalid));
        }
        if self.active_in(session_id) + picks.len() > MAX_TRANSFERS {
            return reply(conn, req, Err(ApiCode::Busy));
        }
        let mut ids = Vec::with_capacity(picks.len());
        let mut started = Vec::new();
        for p in picks {
            let id = self.fresh_id();
            let path = format!("p{}/{}", p.token, p.name.0);
            let start = Start::Send { path: path.clone(), dest: remote_dir.clone() };
            lock(&self.files).recs.insert(
                (session_id, id),
                Rec {
                    conn: conn.id,
                    name: p.name.0.clone(),
                    outgoing: true,
                    start,
                    total: 0,
                    done: 0,
                    user_paused: false,
                    terminal: false,
                    last_sent: None,
                    last_sent_done: 0,
                },
            );
            ids.push(id);
            started.push((id, p.name.0, path));
        }
        reply(conn, req, Ok(ReplyBody::TransferIds(ids)));
        for (id, name, path) in started {
            self.transfer_note(session_id, id, &name, true, TransferWire::Queued);
            if !handle.files().command(Command::SendFile { id, path: PathBuf::from(path), dest: remote_dir.clone() }) {
                self.finish_failed(session_id, id, ApiCode::Busy);
            }
        }
    }

    /// `FilesGet`: the file at `remote_path` of the host, into the receive folder the UI holds under `recv_token`.
    pub fn files_get(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        req: u32,
        session_id: u64,
        remote_path: String,
        recv_token: u32,
    ) {
        let Some(handle) = self.viewer_of(conn, session_id) else { return reply(conn, req, Err(ApiCode::NotFound)) };
        let name = last_component(&remote_path);
        if name.is_empty() {
            return reply(conn, req, Err(ApiCode::Invalid));
        }
        if self.active_in(session_id) + 1 > MAX_TRANSFERS {
            return reply(conn, req, Err(ApiCode::Busy));
        }
        let id = self.fresh_id();
        let save_dir = format!("r{recv_token}");
        let start = Start::Get { remote_path: remote_path.clone(), save_dir: save_dir.clone() };
        lock(&self.files).recs.insert(
            (session_id, id),
            Rec {
                conn: conn.id,
                name: name.clone(),
                outgoing: false,
                start,
                total: 0,
                done: 0,
                user_paused: false,
                terminal: false,
                last_sent: None,
                last_sent_done: 0,
            },
        );
        reply(conn, req, Ok(ReplyBody::TransferId(id)));
        self.transfer_note(session_id, id, &name, false, TransferWire::Queued);
        if !handle.files().command(Command::Get { id, path: remote_path, save_dir: PathBuf::from(save_dir) }) {
            self.finish_failed(session_id, id, ApiCode::Busy);
        }
    }

    /// `TransferPause`.
    pub fn transfer_pause(&self, conn: &Conn, req: u32, id: u32) {
        let Some((session_id, handle)) = self.local_rec(conn, id) else {
            return reply(conn, req, Err(ApiCode::NotFound));
        };
        if let Some(r) = lock(&self.files).recs.get_mut(&(session_id, id)) {
            r.user_paused = true;
        }
        handle.files().command(Command::Pause(id));
        reply(conn, req, Ok(ReplyBody::Done));
    }

    /// `TransferResume` ("Retomar"): the live connection continues it, or, when that connection no longer holds it
    /// (the link broke and came back), it is offered again and the receiver continues from its verified length.
    pub fn transfer_resume(&self, conn: &Conn, req: u32, id: u32) {
        let Some((session_id, handle)) = self.local_rec(conn, id) else {
            return reply(conn, req, Err(ApiCode::NotFound));
        };
        let start = {
            let mut t = lock(&self.files);
            let Some(r) = t.recs.get_mut(&(session_id, id)) else { return reply(conn, req, Err(ApiCode::NotFound)) };
            if r.terminal {
                return reply(conn, req, Err(ApiCode::NotFound));
            }
            r.user_paused = false;
            r.start.clone()
        };
        let ok = if handle.files().knows(id) {
            handle.files().command(Command::Resume(id))
        } else {
            handle.files().command(command_of(id, &start))
        };
        reply(conn, req, if ok { Ok(ReplyBody::Done) } else { Err(ApiCode::Busy) });
    }

    /// `TransferCancel`.
    pub fn transfer_cancel(&self, conn: &Conn, req: u32, id: u32) {
        let Some((session_id, handle)) = self.local_rec(conn, id) else {
            return reply(conn, req, Err(ApiCode::NotFound));
        };
        lock(&self.files).recs.remove(&(session_id, id));
        if handle.files().knows(id) {
            handle.files().command(Command::Cancel(id));
        }
        reply(conn, req, Ok(ReplyBody::Done));
    }

    // ---------- what the engines report ----------

    fn finish_failed(&self, session_id: u64, id: u32, reason: ApiCode) {
        let (name, outgoing) = {
            let mut t = lock(&self.files);
            let Some(r) = t.recs.get_mut(&(session_id, id)) else { return };
            r.terminal = true;
            (r.name.clone(), r.outgoing)
        };
        self.transfer_note(session_id, id, &name, outgoing, TransferWire::Failed { reason });
    }

    /// An event of the files feature of session `session_id` (either role).
    pub(crate) fn files_event(&self, session_id: u64, event: Event) {
        match event {
            Event::Incoming { id, name, size, resume_from } => {
                let outgoing = {
                    let mut t = lock(&self.files);
                    let rec = t.recs.entry((session_id, id)).or_insert_with(|| Rec {
                        conn: 0,
                        name: name.clone(),
                        outgoing: false,
                        start: Start::Theirs,
                        total: size,
                        done: resume_from,
                        user_paused: false,
                        terminal: false,
                        last_sent: None,
                        last_sent_done: resume_from,
                    });
                    rec.total = size;
                    rec.done = resume_from;
                    rec.last_sent = Some(Instant::now());
                    rec.last_sent_done = resume_from;
                    rec.name.clone_from(&name);
                    rec.outgoing
                };
                self.transfer_note(
                    session_id,
                    id,
                    &name,
                    outgoing,
                    TransferWire::Running { done: resume_from, total: size, bytes_per_s: 0 },
                );
            }
            Event::Progress { id, done, total } => {
                let now = Instant::now();
                let (name, outgoing, rate) = {
                    let mut t = lock(&self.files);
                    // The host's own data for a file it sends has no `Incoming`: it gets its record here.
                    let rec = t.recs.entry((session_id, id)).or_insert_with(|| Rec {
                        conn: 0,
                        name: String::new(),
                        outgoing: true,
                        start: Start::Theirs,
                        total,
                        done,
                        user_paused: false,
                        terminal: false,
                        last_sent: None,
                        last_sent_done: 0,
                    });
                    rec.done = done;
                    rec.total = total;
                    let due = rec.last_sent.is_none_or(|at| now.duration_since(at) >= PROGRESS_EVERY) || done >= total;
                    if !due || rec.terminal {
                        return;
                    }
                    let rate = rec.last_sent.map_or(0, |at| {
                        let ms = u64::try_from(now.duration_since(at).as_millis()).unwrap_or(u64::MAX).max(1);
                        done.saturating_sub(rec.last_sent_done).saturating_mul(1000) / ms
                    });
                    rec.last_sent = Some(now);
                    rec.last_sent_done = done;
                    (rec.name.clone(), rec.outgoing, rate)
                };
                self.transfer_note(
                    session_id,
                    id,
                    &name,
                    outgoing,
                    TransferWire::Running { done, total, bytes_per_s: rate },
                );
            }
            Event::Received { id, path } => {
                let saved = path.file_name().and_then(|n| n.to_str()).map(str::to_string);
                let (name, outgoing, renamed) = {
                    let mut t = lock(&self.files);
                    let Some(r) = t.recs.get_mut(&(session_id, id)) else { return };
                    r.terminal = true;
                    r.done = r.total;
                    (r.name.clone(), r.outgoing, saved.filter(|s| *s != r.name))
                };
                if let Some(saved) = renamed {
                    self.files_note(session_id, FilesNote::SavedAs { session_id, transfer_id: id, name: Text(saved) });
                }
                self.transfer_note(session_id, id, &name, outgoing, TransferWire::Done);
            }
            Event::Sent { id } => {
                let (name, outgoing) = {
                    let mut t = lock(&self.files);
                    let Some(r) = t.recs.get_mut(&(session_id, id)) else { return };
                    r.terminal = true;
                    (r.name.clone(), r.outgoing)
                };
                self.transfer_note(session_id, id, &name, outgoing, TransferWire::Done);
            }
            Event::Paused { id } => {
                let (name, outgoing, done, total) = {
                    let t = lock(&self.files);
                    let Some(r) = t.recs.get(&(session_id, id)) else { return };
                    if r.terminal {
                        return;
                    }
                    (r.name.clone(), r.outgoing, r.done, r.total)
                };
                self.transfer_note(
                    session_id,
                    id,
                    &name,
                    outgoing,
                    TransferWire::Paused { done, total, why: ApiCode::Timeout },
                );
            }
            Event::Failed { id, code } => self.finish_failed(session_id, id, api_of(code)),
            Event::Cancelled { id } => {
                // The user's own cancel removed the record already; this is the other side cancelling.
                let known = lock(&self.files).recs.contains_key(&(session_id, id));
                if known {
                    self.finish_failed(session_id, id, ApiCode::Refused);
                }
            }
            Event::Listing { req_id, path, entries, truncated } => {
                let Some(w) = self.take_wait(req_id, Waits::Dir) else { return };
                let listing = FileListing {
                    path: Text(path),
                    entries: entries
                        .into_iter()
                        .map(|e| FileEntry { name: Text(e.name), is_dir: e.is_dir, size: e.size, mtime_ms: e.mtime_ms })
                        .collect(),
                    truncated,
                };
                reply(&w.conn, w.req, Ok(ReplyBody::Listing(listing)));
            }
            Event::PageListing { req_id, path, start, entries, next, free_bytes } => {
                let Some(w) = self.take_wait(req_id, Waits::Page) else { return };
                let page = FsPage {
                    entries: entries
                        .into_iter()
                        .map(|e| FsEntry2View {
                            name: Text(e.name),
                            kind: e.kind,
                            size: e.size,
                            mtime_ms: e.mtime_ms,
                            flags: e.flags,
                        })
                        .collect(),
                    next,
                    free: free_bytes,
                };
                reply(&w.conn, w.req, Ok(ReplyBody::Listing2(FileListing2 { path: Text(path), start, page })));
            }
            Event::Volumes(list) => {
                let req_id = list.req_id;
                let view = volumes_view(list);
                if req_id == 0 {
                    // The host's set changed and it said so unasked.
                    self.files_note(session_id, FilesNote::Volumes { session_id, volumes: view });
                } else if let Some(w) = self.take_wait(req_id, Waits::Volumes) {
                    reply(&w.conn, w.req, Ok(ReplyBody::Volumes(view)));
                }
            }
            Event::OpResult { op_id, results } => {
                let Some(w) = self.take_wait(op_id, Waits::Op) else { return };
                let results = results.into_iter().map(|i| OpResultView { name: Text(i.name), code: i.code }).collect();
                reply(&w.conn, w.req, Ok(ReplyBody::OpResults(results)));
            }
            // The host side's audit (FILES-UI.md 8.3): counts and kinds, never a name.
            // The same facts go to the console UI for the session banner (FILES-UI.md 6): counts, never a name.
            Event::Changed { op, count, area } => {
                self.files_note(session_id, FilesNote::Changed { session_id, op, count });
                self.file_audit(session_id, AuditEvent::FilesChanged { op, count, area })
            }
            Event::Reach { kinds } => {
                self.files_note(session_id, FilesNote::Reach { session_id });
                self.file_audit(session_id, AuditEvent::FilesReach { kinds })
            }
        }
    }

    /// The connection of viewer session `session_id` is back (F-NET-01): every unfinished transfer the user did not
    /// pause is offered again over it, so the receivers continue from their verified length.
    pub(crate) fn files_reissue(self: &Arc<Self>, session_id: u64, handle: &ViewerHandle) {
        let todo: Vec<(u32, Start)> = lock(&self.files)
            .recs
            .iter()
            .filter(|((s, _), r)| {
                *s == session_id && !r.terminal && !r.user_paused && !matches!(r.start, Start::Theirs)
            })
            .map(|((_, id), r)| (*id, r.start.clone()))
            .collect();
        if todo.is_empty() {
            return;
        }
        let Some(rt) = self.rt() else { return };
        let handle = handle.clone();
        rt.spawn(async move {
            for (id, start) in todo {
                // The new connection's handler starts a moment after the session reports it is back.
                for _ in 0..REISSUE_TRIES {
                    if handle.files().knows(id) || handle.files().command(command_of(id, &start)) {
                        break;
                    }
                    tokio::time::sleep(REISSUE_PAUSE).await;
                }
            }
        });
    }

    /// Forgets the transfers and listings of a session that ended.
    pub(crate) fn files_session_ended(&self, session_id: u64) {
        let mut t = lock(&self.files);
        t.recs.retain(|(s, _), _| *s != session_id);
    }
}

/// The wire's change for the interface's.
fn op_of(change: FsChange) -> FsOpKind {
    let names = |v: Vec<swapdesk_ipc::FsNameView>| v.into_iter().map(|n| FsName { name: n.name.0 }).collect();
    match change {
        FsChange::MakeDir { parent, name } => FsOpKind::MakeDir { parent: parent.0, name: name.0 },
        FsChange::Rename { dir, name, new_name } => FsOpKind::Rename { dir: dir.0, name: name.0, new_name: new_name.0 },
        FsChange::Move { from_dir, names: n, to_dir } => {
            FsOpKind::Move { from_dir: from_dir.0, names: names(n), to_dir: to_dir.0 }
        }
        FsChange::Trash { dir, names: n } => FsOpKind::Trash { dir: dir.0, names: names(n) },
    }
}

/// The host's volume list for the interface.
fn volumes_view(list: swapdesk_proto::VolumesList) -> VolumesView {
    VolumesView {
        home: Text(list.home),
        places: list.places.into_iter().map(|p| PlaceView { kind: p.kind, path: Text(p.path) }).collect(),
        volumes: list
            .volumes
            .into_iter()
            .map(|v| VolumeView {
                id: v.id,
                kind: v.kind,
                label: Text(v.label),
                letter: v.letter,
                total_bytes: v.total_bytes,
                free_bytes: v.free_bytes,
                has_bin: v.has_bin,
            })
            .collect(),
    }
}

fn command_of(id: u32, start: &Start) -> Command {
    match start {
        Start::Send { path, dest } => Command::SendFile { id, path: PathBuf::from(path), dest: dest.clone() },
        Start::Get { remote_path, save_dir } => {
            Command::Get { id, path: remote_path.clone(), save_dir: PathBuf::from(save_dir) }
        }
        // Never reissued: the filter keeps these out.
        Start::Theirs => Command::Cancel(id),
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    fn names(n: usize) -> Vec<FsName> {
        (0..n).map(|i| FsName { name: format!("n{i}") }).collect()
    }

    #[test]
    fn a_change_is_waited_for_longer_than_the_host_may_take_for_it() {
        let mv = |n| FsOpKind::Move { from_dir: String::new(), names: names(n), to_dir: String::new() };
        assert!(op_wait(&mv(1)) > swapdesk_files::MOVE_CALL_TIMEOUT, "a long move is not a timeout");
        assert!(op_wait(&mv(64)) > swapdesk_files::MOVE_CALL_TIMEOUT * 64, "each name has its own call");
        let trash = FsOpKind::Trash { dir: String::new(), names: names(3) };
        assert!(op_wait(&trash) > swapdesk_files::CALL_TIMEOUT * 3);
        let dir = FsOpKind::MakeDir { parent: String::new(), name: "x".into() };
        assert!(op_wait(&dir) > swapdesk_files::CALL_TIMEOUT);
    }
}
