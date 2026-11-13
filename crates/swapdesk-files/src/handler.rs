//! [`FilesHandler`]: the `ChannelHandler` that carries the file messages and the 0x05 data streams of a session
//! through an [`Engine`].
//!
//! One worker task per session owns the order of everything the engine says: it runs each peer message or local
//! command on a blocking thread (the engine touches the disk), then carries out the answer (control messages in
//! order, data streams as their own tasks). Data streams run under the `FILES` grant, so a revocation stops them
//! within the session core's own limit (F-PERM-02); the transfers then wait, with their verified length on disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use swapdesk_proto::{ErrorCode, FileDataHeader, FsOpKind, Message, PermissionSet, StreamKind, Wire};
use swapdesk_session::{ChannelHandler, IncomingStream, Role, SessionCtx, SessionEnd};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::AbortHandle;

use crate::disk::Fs;
use crate::engine::{Engine, Event, FilesConfig, MAX_FS_OPS, Out, Side, busy_result, failed_result};
use crate::local::LocalFs;
use crate::send::ChunkReader;

/// Where the UI hears about transfers.
pub trait FilesSink: Send + Sync + 'static {
    fn event(&self, event: Event);
}

/// What the local user asks of the session's file feature.
#[derive(Debug, Clone)]
pub enum Command {
    SendFile {
        id: u32,
        path: PathBuf,
        dest: Option<String>,
    },
    Get {
        id: u32,
        path: String,
        save_dir: PathBuf,
    },
    Dir {
        req_id: u32,
        path: Option<String>,
    },
    /// Viewer: a page of a folder of the host (a 1.1 path, minor 2).
    DirPage {
        req_id: u32,
        path: String,
        start: u32,
        max: u16,
    },
    /// Viewer: the host's home, places and volumes (minor 2).
    Volumes {
        req_id: u32,
    },
    /// Viewer: a change of the host's files (minor 2).
    FsOp {
        op_id: u32,
        op: FsOpKind,
    },
    /// Host: send the viewer a fresh volume list (`req_id` 0). Internal: see [`FilesHandler::push_volumes`].
    PushVolumes,
    Pause(u32),
    Resume(u32),
    Cancel(u32),
}

/// Local work carries the place it holds in the local share of the queue, given back when the worker takes it.
enum Work {
    Msg(Message),
    Cmd(Command, OwnedSemaphorePermit),
    Outs(Vec<Out>, OwnedSemaphorePermit),
}

/// Local items waiting for a session's worker (one engine call on a blocking thread each): the user's commands and what
/// the data tasks found.
const WORK_QUEUE: usize = 1024;
/// The peer's messages waiting, on top of those: a peer that has sent this many file messages ahead of the disk thread,
/// which has not got to the first, is not following the protocol (a transfer is a handful of messages), so the session
/// ends `Busy`. The channel has room for both shares at once, so local work can never take the places of the peer's
/// ordinary messages: a stalled worker does not turn a legitimate `FileAccept` into a "flood".
const PEER_BACKLOG: usize = 256;

/// The bounded queue in front of a session's worker, and what each kind of sender does when it is full:
/// * the peer's messages are counted apart ([`PEER_BACKLOG`]); past it the caller ends the session (`Busy`);
/// * the local user's commands are refused when the local share ([`WORK_QUEUE`] places) is taken (`command` returns
///   `false`, the UI says so and the user tries again);
/// * the tasks of the data streams wait for a place in the local share (a slow disk slows the streams, nothing grows).
#[derive(Clone)]
struct WorkQueue {
    tx: mpsc::Sender<Work>,
    peer: Arc<AtomicUsize>,
    /// The places of the local share.
    local: Arc<Semaphore>,
}

/// What became of a peer's message.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Queued,
    /// The peer is too far ahead: end the session.
    Flood,
    /// The worker is gone (the session ended).
    Closed,
}

impl WorkQueue {
    fn new() -> (Self, mpsc::Receiver<Work>) {
        let (tx, rx) = mpsc::channel(WORK_QUEUE + PEER_BACKLOG);
        (Self { tx, peer: Arc::default(), local: Arc::new(Semaphore::new(WORK_QUEUE)) }, rx)
    }

    fn peer_message(&self, msg: Message) -> Admit {
        if self.peer.fetch_add(1, Ordering::SeqCst) >= PEER_BACKLOG {
            self.peer.fetch_sub(1, Ordering::SeqCst);
            return Admit::Flood;
        }
        match self.tx.try_send(Work::Msg(msg)) {
            Ok(()) => Admit::Queued,
            Err(e) => {
                self.peer.fetch_sub(1, Ordering::SeqCst);
                match e {
                    // `Full` cannot happen with both shares inside the channel's room; if it ever did, it would not be
                    // the peer's doing.
                    mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_) => Admit::Closed,
                }
            }
        }
    }

    fn command(&self, cmd: Command) -> bool {
        match Arc::clone(&self.local).try_acquire_owned() {
            Ok(place) => self.tx.try_send(Work::Cmd(cmd, place)).is_ok(),
            Err(_) => false,
        }
    }

    /// What a data task found, in order, waiting for a place when the worker is behind.
    async fn outs(&self, outs: Vec<Out>) {
        if let Ok(place) = Arc::clone(&self.local).acquire_owned().await {
            let _ = self.tx.send(Work::Outs(outs, place)).await;
        }
    }
}

/// The `FsOp` messages of a session admitted and not yet answered: at most [`MAX_FS_OPS`] (FILES-UI.md 8.4).
#[derive(Default)]
struct OpSlots(AtomicUsize);

impl OpSlots {
    /// Takes a place; false when all are taken (the message is answered `BUSY` and never queued).
    fn take(&self) -> bool {
        if self.0.fetch_add(1, Ordering::SeqCst) >= MAX_FS_OPS {
            self.0.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        true
    }

    fn give(&self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The pushed volume lists of one session: at most one a second, the last change always going out.
#[derive(Default)]
struct PushState {
    last: Option<Instant>,
    trailing: bool,
}

const PUSH_EVERY: Duration = Duration::from_secs(1);

struct Live {
    session_id: u64,
    ctx: SessionCtx,
    work: WorkQueue,
    engine: Arc<Mutex<Engine>>,
    ops: Arc<OpSlots>,
    push: Arc<Mutex<PushState>>,
}

pub struct FilesHandler {
    /// Host: the folder for offers without `dest`; both: the home folder for `DirRequest` with no path.
    default_dir: Option<PathBuf>,
    home: PathBuf,
    max_file_size: u64,
    fs: Arc<dyn Fs>,
    sink: Arc<dyn FilesSink>,
    live: Mutex<Option<Live>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl FilesHandler {
    pub fn new(home: PathBuf, default_dir: Option<PathBuf>, sink: Arc<dyn FilesSink>) -> Arc<Self> {
        Self::with_fs(Arc::new(LocalFs::new()), home, default_dir, crate::engine::DEFAULT_MAX_FILE_SIZE, sink)
    }

    /// Like `new`, with another size limit for received files.
    pub fn with_max_file_size(
        home: PathBuf,
        default_dir: Option<PathBuf>,
        max_file_size: u64,
        sink: Arc<dyn FilesSink>,
    ) -> Arc<Self> {
        Self::with_fs(Arc::new(LocalFs::new()), home, default_dir, max_file_size, sink)
    }

    /// Like `with_max_file_size`, with every disk access going through `fs` (a proxy to the process that owns the
    /// user's files, so the service never opens a path itself).
    pub fn with_fs(
        fs: Arc<dyn Fs>,
        home: PathBuf,
        default_dir: Option<PathBuf>,
        max_file_size: u64,
        sink: Arc<dyn FilesSink>,
    ) -> Arc<Self> {
        Arc::new(Self { default_dir, home, max_file_size, fs, sink, live: Mutex::new(None) })
    }

    /// Runs a local command in the live session; false when there is none.
    pub fn command(&self, cmd: Command) -> bool {
        lock(&self.live).as_ref().is_some_and(|l| l.work.command(cmd))
    }

    /// Host: tells the viewer the set of volumes changed, with a fresh `VolumesList` (`req_id` 0). Only toward a viewer
    /// of minor 2 or more that still holds `FILES`; at most one a second (a change inside the second goes out when it
    /// ends). The caller (the D13 gate) says whether the session may be told anything.
    pub fn push_volumes(&self) -> bool {
        let Some((ctx, work, state)) =
            lock(&self.live).as_ref().map(|l| (l.ctx.clone(), l.work.clone(), Arc::clone(&l.push)))
        else {
            return false;
        };
        if ctx.peer_minor() < 2 || !ctx.granted().contains(PermissionSet::FILES) {
            return false;
        }
        let wait = {
            let mut st = lock(&state);
            match st.last.map(|at| PUSH_EVERY.saturating_sub(at.elapsed())) {
                Some(wait) if !wait.is_zero() => {
                    if st.trailing {
                        return true;
                    }
                    st.trailing = true;
                    Some(wait)
                }
                _ => {
                    st.last = Some(Instant::now());
                    None
                }
            }
        };
        match wait {
            None => work.command(Command::PushVolumes),
            Some(wait) => {
                ctx.spawn(async move {
                    tokio::time::sleep(wait).await;
                    let mut st = lock(&state);
                    st.trailing = false;
                    st.last = Some(Instant::now());
                    drop(st);
                    work.command(Command::PushVolumes);
                });
                true
            }
        }
    }

    /// Does the live session's engine hold transfer `id`? After a lost link it holds none: the transfer is offered
    /// again (the receiver resumes from its verified length).
    pub fn knows(&self, id: u32) -> bool {
        lock(&self.live).as_ref().is_some_and(|l| lock(&l.engine).has(id))
    }

    /// Transfers the engine holds now.
    pub fn active(&self) -> usize {
        lock(&self.live).as_ref().map_or(0, |l| lock(&l.engine).active())
    }
}

impl ChannelHandler for FilesHandler {
    fn name(&self) -> &'static str {
        "files"
    }

    fn messages(&self) -> &'static [u16] {
        &[
            0x0401, 0x0402, 0x0403, 0x0404, 0x0405, 0x0406, 0x0407, 0x0408, 0x0409, 0x040A, 0x040B, 0x040C, 0x040D,
            0x040E, 0x040F, 0x0410, 0x0411,
        ]
    }

    fn streams(&self) -> &'static [StreamKind] {
        &[StreamKind::FileData]
    }

    fn on_start(&self, ctx: &SessionCtx) {
        let side = match ctx.role() {
            Role::Host => Side::Host,
            Role::Viewer => Side::Viewer,
        };
        let mut cfg = FilesConfig::new(side, self.home.clone());
        cfg.default_dir = self.default_dir.clone();
        cfg.max_file_size = self.max_file_size;
        let engine = Arc::new(Mutex::new(Engine::with_fs(cfg, Arc::clone(&self.fs))));
        let (tx, rx) = WorkQueue::new();
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let ops = Arc::new(OpSlots::default());
        ctx.spawn(worker(
            ctx.clone(),
            Arc::clone(&engine),
            Arc::clone(&self.sink),
            tx.clone(),
            rx,
            streams,
            Arc::clone(&ops),
        ));
        *lock(&self.live) =
            Some(Live { session_id: ctx.session_id(), ctx: ctx.clone(), work: tx, engine, ops, push: Arc::default() });
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        // The explorer's requests exist from minor 2 on: a viewer that announced less cannot have sent them.
        let explorer = matches!(msg, Message::DirPage(_) | Message::VolumesRequest(_) | Message::FsOp(_));
        if explorer && ctx.peer_minor() < 2 {
            return;
        }
        let live = lock(&self.live);
        let Some(l) = live.as_ref() else { return };
        let is_op = matches!(msg, Message::FsOp(_));
        if let Message::FsOp(op) = &msg
            && !l.ops.take()
        {
            // One too many: every name is `BUSY`, nothing is queued.
            let busy = busy_result(op);
            drop(live);
            let sender = ctx.clone();
            ctx.spawn(async move {
                let _ = sender.send(busy).await;
            });
            return;
        }
        let admitted = l.work.peer_message(msg);
        if is_op && admitted != Admit::Queued {
            l.ops.give();
        }
        drop(live);
        if admitted == Admit::Flood {
            ctx.bye(ErrorCode::Busy);
        }
    }

    fn on_stream(&self, ctx: &SessionCtx, stream: IncomingStream) {
        let live = lock(&self.live);
        let Some(l) = live.as_ref() else { return };
        let engine = Arc::clone(&l.engine);
        let work = l.work.clone();
        drop(live);
        spawn_stream(ctx, None, receive_stream(engine, work, stream.recv));
    }

    fn on_permissions(&self, _ctx: &SessionCtx, old: PermissionSet, now: PermissionSet) {
        if old.contains(PermissionSet::FILES) && !now.contains(PermissionSet::FILES) {
            // The data tasks are already aborted; the transfers wait on disk until the grant returns and the files
            // are offered again.
            if let Some(l) = lock(&self.live).as_ref() {
                let outs = lock(&l.engine).link_lost();
                for o in outs {
                    if let Out::Event(e) = o {
                        self.sink.event(e);
                    }
                }
            }
        }
    }

    fn on_end(&self, ctx: &SessionCtx, _end: &SessionEnd) {
        // A late `on_end` of an old session must not drop the state of the one that replaced it.
        let mut live = lock(&self.live);
        let mine = live.as_ref().is_some_and(|l| l.session_id == ctx.session_id());
        let taken = if mine { live.take() } else { None };
        drop(live);
        if let Some(l) = taken {
            let outs = lock(&l.engine).link_lost();
            for o in outs {
                if let Out::Event(e) = o {
                    self.sink.event(e);
                }
            }
        }
    }
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type Streams = Arc<Mutex<HashMap<u32, AbortHandle>>>;

/// Starts a data task that ends with the session and with the `FILES` grant. `id` registers it for `Stop`.
fn spawn_stream<F>(ctx: &SessionCtx, registry: Option<(&Streams, u32)>, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let inner = tokio::spawn(fut);
    let abort = inner.abort_handle();
    if let Some((streams, id)) = registry {
        lock(streams).insert(id, abort.clone());
    }
    let guard = AbortOnDrop(abort.clone());
    if !ctx.spawn_gated(PermissionSet::FILES, async move {
        let _guard = guard;
        let _ = inner.await;
    }) {
        abort.abort();
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

async fn worker(
    ctx: SessionCtx,
    engine: Arc<Mutex<Engine>>,
    sink: Arc<dyn FilesSink>,
    work_tx: WorkQueue,
    mut rx: mpsc::Receiver<Work>,
    streams: Streams,
    ops: Arc<OpSlots>,
) {
    while let Some(w) = rx.recv().await {
        let outs = match w {
            Work::Outs(o, place) => {
                drop(place);
                o
            }
            Work::Msg(m) => {
                work_tx.peer.fetch_sub(1, Ordering::SeqCst);
                if let Message::FsOp(op) = m {
                    // A change of the files never runs on this worker: a long one (a copy across volumes) would hold
                    // every other file message of the session behind it. It takes its place in `ops` (at most
                    // MAX_FS_OPS, already counted by `on_message`) and gives it back with its answer.
                    let e = Arc::clone(&engine);
                    match blocking(move || lock(&e).begin_op(op)).await {
                        Some(Ok(job)) => {
                            // The change belongs to the permissions that allowed it (FILES and CONTROL, FILES-UI.md
                            // 3.3): when either is withdrawn, or the session ends, the task is stopped and the names it
                            // has not begun are never touched. It does not start at all when they are not held now.
                            let refused = failed_result(job.op(), ErrorCode::NotPermitted);
                            let task = run_op(Arc::clone(&engine), work_tx.clone(), Arc::clone(&ops), job);
                            if ctx.spawn_gated(PermissionSet::FILES.union(PermissionSet::CONTROL), task) {
                                continue;
                            }
                            ops.give();
                            vec![Out::Msg(refused)]
                        }
                        Some(Err(outs)) => {
                            ops.give();
                            outs
                        }
                        None => {
                            ops.give();
                            Vec::new()
                        }
                    }
                } else {
                    let e = Arc::clone(&engine);
                    blocking(move || lock(&e).on_message(m)).await.unwrap_or_default()
                }
            }
            Work::Cmd(c, place) => {
                drop(place);
                let e = Arc::clone(&engine);
                blocking(move || {
                    let mut e = lock(&e);
                    match c {
                        Command::SendFile { id, path, dest } => e.send_file(id, &path, dest),
                        Command::Get { id, path, save_dir } => e.request_get(id, path, save_dir),
                        Command::Dir { req_id, path } => e.request_dir(req_id, path),
                        Command::DirPage { req_id, path, start, max } => e.request_page(req_id, path, start, max),
                        Command::Volumes { req_id } => e.request_volumes(req_id),
                        Command::FsOp { op_id, op } => e.request_op(op_id, op),
                        Command::PushVolumes => e.volumes_push(),
                        Command::Pause(id) => e.pause(id),
                        Command::Resume(id) => e.resume(id),
                        Command::Cancel(id) => e.cancel(id),
                    }
                })
                .await
                .unwrap_or_default()
            }
        };
        for out in outs {
            match out {
                Out::Msg(m) => {
                    if ctx.send(m).await.is_err() {
                        return;
                    }
                }
                Out::Event(e) => sink.event(e),
                Out::Stop { id } => {
                    if let Some(h) = lock(&streams).remove(&id) {
                        h.abort();
                    }
                }
                Out::Send { id, from } => {
                    if let Some(h) = lock(&streams).remove(&id) {
                        h.abort();
                    }
                    spawn_stream(
                        &ctx,
                        Some((&streams, id)),
                        send_stream(ctx.clone(), Arc::clone(&engine), work_tx.clone(), id, from),
                    );
                }
            }
        }
    }
}

/// Raises a change's cancel flag when the task that waits for the change is dropped before it ends (aborted: a
/// permission withdrawn, the session over).
struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// One change of the host's files, off the worker, run under the permissions that allow it (the caller starts this with
/// `spawn_gated`). The disk work and its answer are a task of their own that no abort reaches, so the name being worked
/// on always finishes (a move that began ends copied, verified, source to the bin, or takes its copy away, never half)
/// and the viewer's audit hears what was done; this task only waits for it. When this task is aborted, its guard raises
/// the change's cancel flag: the names not begun are not touched, and the slot is given back all the same.
async fn run_op(engine: Arc<Mutex<Engine>>, work: WorkQueue, ops: Arc<OpSlots>, job: crate::engine::OpJob) {
    let job = Arc::new(job);
    let guard = CancelOnDrop(job.cancel_flag());
    let task = tokio::spawn(do_op(engine, work, ops, job));
    let _ = task.await;
    // Finished on its own: raising the flag now touches nothing.
    drop(guard);
}

/// The disk work of one change on a blocking thread without the engine's lock, then the answer and the audit events
/// through the worker's queue, so the viewer always hears what became of it.
async fn do_op(engine: Arc<Mutex<Engine>>, work: WorkQueue, ops: Arc<OpSlots>, job: Arc<crate::engine::OpJob>) {
    let j = Arc::clone(&job);
    let outs = match blocking(move || j.run()).await {
        Some(results) => {
            let (e, j) = (Arc::clone(&engine), Arc::clone(&job));
            blocking(move || lock(&e).finish_op(&j, results)).await
        }
        None => None,
    };
    let outs = outs.unwrap_or_else(|| vec![Out::Msg(failed_result(job.op(), ErrorCode::Internal))]);
    work.outs(outs).await;
    ops.give();
}

/// The sending side of one data stream: header, the bytes from `from`, finish, then `FileDone` through the engine.
async fn send_stream(ctx: SessionCtx, engine: Arc<Mutex<Engine>>, work: WorkQueue, id: u32, from: u64) {
    let fail = |code: ErrorCode| {
        let e = Arc::clone(&engine);
        let work = work.clone();
        async move {
            if let Some(outs) = blocking(move || lock(&e).send_failed(id, code)).await {
                work.outs(outs).await;
            }
        }
    };
    let e = Arc::clone(&engine);
    let reader = match blocking(move || lock(&e).open_reader(id, from)).await {
        Some(Ok(r)) => r,
        Some(Err(code)) => return fail(code).await,
        None => return fail(ErrorCode::Internal).await,
    };
    let mut stream = match ctx.open_uni(StreamKind::FileData).await {
        Ok(s) => s,
        Err(code) => return fail(code).await,
    };
    let header = match (FileDataHeader { transfer_id: id, offset: from }).encode() {
        Ok(h) => h,
        Err(_) => return fail(ErrorCode::Internal).await,
    };
    if stream.write_all(&header).await.is_err() {
        // The link is gone: the receiver keeps its verified length; the session core reports the loss.
        return;
    }
    let mut reader: ChunkReader = reader;
    let mut progress = from;
    let total = {
        let e = Arc::clone(&engine);
        blocking(move || lock(&e).total_of(id)).await.flatten().unwrap_or(0)
    };
    loop {
        let step = blocking(move || {
            let c = reader.next_chunk();
            (reader, c)
        })
        .await;
        let Some((r, chunk)) = step else { return fail(ErrorCode::Internal).await };
        reader = r;
        match chunk {
            Ok(Some(bytes)) => {
                if stream.write_all(&bytes).await.is_err() {
                    return;
                }
                progress += bytes.len() as u64;
                work.outs(vec![Out::Event(Event::Progress { id, done: progress, total })]).await;
            }
            Ok(None) => break,
            Err(code) => return fail(code).await,
        }
    }
    if stream.finish().is_err() {
        return;
    }
    let sha = match blocking(move || reader.sha256()).await {
        Some(Ok(s)) => s,
        Some(Err(code)) => return fail(code).await,
        None => return fail(ErrorCode::Internal).await,
    };
    let e = Arc::clone(&engine);
    if let Some(outs) = blocking(move || lock(&e).sent_all(id, sha)).await {
        work.outs(outs).await;
    }
}

/// The receiving side of one data stream (the kind byte is already read).
async fn receive_stream(engine: Arc<Mutex<Engine>>, work: WorkQueue, mut recv: quinn::RecvStream) {
    let mut head = [0u8; 12];
    if recv.read_exact(&mut head).await.is_err() {
        return;
    }
    let Ok(h) = FileDataHeader::decode(&head) else { return };
    let id = h.transfer_id;
    let mut offset = h.offset;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                let e = Arc::clone(&engine);
                let data = buf[..n].to_vec();
                let at = offset;
                offset += n as u64;
                let outs = blocking(move || lock(&e).on_data(id, at, &data)).await.unwrap_or_default();
                let failed = outs.iter().any(|o| matches!(o, Out::Event(Event::Failed { .. })));
                work.outs(outs).await;
                if failed {
                    return;
                }
            }
            Ok(None) => {
                let e = Arc::clone(&engine);
                if let Some(outs) = blocking(move || lock(&e).data_ended(id)).await {
                    work.outs(outs).await;
                }
                return;
            }
            Err(_) => {
                let e = Arc::clone(&engine);
                if let Some(outs) = blocking(move || lock(&e).data_broken(id)).await {
                    work.outs(outs).await;
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_proto::FilePause;

    fn pause(id: u32) -> Message {
        Message::FilePause(FilePause { transfer_id: id })
    }

    #[test]
    fn a_peer_cannot_queue_more_than_its_backlog_and_the_worker_taking_one_gives_the_room_back() {
        let (queue, mut rx) = WorkQueue::new();
        for i in 0..PEER_BACKLOG {
            assert_eq!(queue.peer_message(pause(i as u32)), Admit::Queued, "message {i}");
        }
        assert_eq!(queue.peer_message(pause(9999)), Admit::Flood, "one past the backlog");
        assert_eq!(queue.peer.load(Ordering::SeqCst), PEER_BACKLOG, "a refused message is not counted");
        // The worker takes one and counts it off, as `worker` does.
        assert!(matches!(rx.try_recv(), Ok(Work::Msg(_))));
        queue.peer.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(queue.peer_message(pause(1)), Admit::Queued);
    }

    #[test]
    fn local_work_fills_its_own_share_and_never_takes_the_places_of_the_peers_ordinary_messages() {
        let (queue, mut rx) = WorkQueue::new();
        let mut taken = 0;
        while queue.command(Command::Pause(1)) {
            taken += 1;
            assert!(taken <= WORK_QUEUE, "the queue has a bound");
        }
        assert_eq!(taken, WORK_QUEUE, "commands fill the local share and then say no");
        // The worker is stalled and the local share is full: the peer's next ordinary messages still get in, up to
        // its own backlog, and only that is a flood.
        for i in 0..PEER_BACKLOG {
            assert_eq!(queue.peer_message(pause(i as u32)), Admit::Queued, "peer message {i} after a full local share");
        }
        assert_eq!(queue.peer_message(pause(9999)), Admit::Flood);
        // A place comes back when the worker takes a local item.
        let Ok(Work::Cmd(_, place)) = rx.try_recv() else { panic!("the first item is a command") };
        drop(place);
        assert!(queue.command(Command::Pause(2)));
    }

    #[test]
    fn an_fs_op_beyond_four_in_flight_answers_busy_for_every_name() {
        let slots = OpSlots::default();
        for i in 0..MAX_FS_OPS {
            assert!(slots.take(), "op {i} gets a place");
        }
        assert!(!slots.take(), "the fifth does not");
        assert!(!slots.take(), "and neither does a sixth: a refusal takes nothing");
        slots.give();
        assert!(slots.take(), "a place freed by an answer is taken again");
        // The answer a refused one gets: BUSY for each name, in the message's order.
        let op = swapdesk_proto::FsOp {
            op_id: 9,
            op: FsOpKind::Trash {
                dir: String::new(),
                names: ["a", "b", "c"].map(|n| swapdesk_proto::FsName { name: n.to_string() }).to_vec(),
            },
        };
        let Message::FsOpResult(r) = busy_result(&op) else { panic!("an FsOpResult") };
        assert_eq!(r.op_id, 9);
        let seen: Vec<_> = r.results.iter().map(|i| (i.name.as_str(), i.code)).collect();
        assert_eq!(seen, [("a", ErrorCode::Busy), ("b", ErrorCode::Busy), ("c", ErrorCode::Busy)]);
    }

    #[test]
    fn the_explorer_messages_are_registered() {
        let h = FilesHandler::new(PathBuf::new(), None, Arc::new(NoSink));
        for t in 0x040C..=0x0411u16 {
            assert!(h.messages().contains(&t), "{t:#x}");
        }
    }

    struct NoSink;
    impl FilesSink for NoSink {
        fn event(&self, _event: Event) {}
    }

    #[test]
    fn a_closed_worker_is_not_a_flood() {
        let (queue, rx) = WorkQueue::new();
        drop(rx);
        assert_eq!(queue.peer_message(pause(1)), Admit::Closed);
        assert!(!queue.command(Command::Pause(1)));
        assert_eq!(queue.peer.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_data_task_waits_for_room_instead_of_growing_the_queue() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        rt.block_on(async {
            let (queue, mut rx) = WorkQueue::new();
            for _ in 0..WORK_QUEUE {
                assert!(queue.command(Command::Pause(1)));
            }
            let waiting = queue.clone();
            let task = tokio::spawn(async move { waiting.outs(Vec::new()).await });
            tokio::time::timeout(std::time::Duration::from_millis(100), std::future::pending::<()>())
                .await
                .unwrap_err();
            assert!(!task.is_finished(), "the queue is full: the data task waits");
            // The worker takes one item and gives its place back.
            let Some(Work::Cmd(_, place)) = rx.recv().await else { panic!("a command first") };
            drop(place);
            tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap();
        });
    }
}
