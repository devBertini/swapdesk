//! `ServiceBackend`: the C6 [`UiBackend`] over the C2.4 UI pipe (`swapdesk-ipc`).
//!
//! Two threads own the stream the shell opened and checked (C2): the **backend thread** takes queued commands (and
//! the viewer messages the UI writes), gives each service command a fresh request id, remembers
//! `req -> (Ticket, op)` and writes the `UiRequest`; the **reader thread** turns every `UiReply` into
//! `UiEvent::Reply` (the body decoded with the remembered op, the status mapped to an `ApiCode`) and every
//! `UiNotify` into its `UiEvent`. Viewer media never becomes a `UiEvent` (C6): it goes to the optional
//! [`ViewerSink`]. `send` only pushes onto an unbounded queue and never blocks.
//!
//! Mapping decisions the contract leaves open (also in [`convert`]):
//! - The service also asks this process for its clipboard (C2.4 0x60 to 0x62, the viewer role of a session):
//!   [`ServiceBackend::set_clipboard`] gives it the provider; without one every call is answered with a failure.
//! - Terminal (stage 43): `TermOpen`, `TermResize` and `TermClose` are ops 32 to 34; `TermWrite` is not an op but the
//!   `TermInput` message (cut into chunks of at most [`swapdesk_ipc::MAX_TERM_CHUNK`], answered `Done` at once); the
//!   reply of `TermOpen` is `Reply::TermId` followed by `UiEvent::Term { Opened }` when the shell runs, or an error
//!   whose `detail` says why; `TermOutput` messages go to [`ViewerSink::term_output`]; `TermExited` is
//!   `UiEvent::Term { Exited }`.
//! - Commands with no op (files, transfers, `CancelSwap`, `BlockAddress`, `Elevate`,
//!   `ActivityExport`, `OpenSystemSettings`, `RequestOsPermissions`) reply at once
//!   `Err(ApiError { code: Internal, detail: None })`. `ListDisplays` is op 27: the reply is the session's display list.
//! - `SetScale`, `SetKeyboardMode` and `SetGrab` are UI-local: they reply `Done` at once and write nothing.
//! - `SetSettings` sends the machine fields only (`SettingsUpdate`); the per-user fields stay in the UI (C5.1).
//! - The pipe closing or breaking fails every pending ticket with `ServiceDown`; every later `send` replies
//!   `ServiceDown` at once. C6 has no "service is down" event, so this is Reply errors only.
//! - Identical in-flight idempotent commands are not merged here (each gets its own request and ticket).
//! - One wake per batch: `wake` is called when events land on an empty queue (the UI has not drained yet
//!   otherwise, so a wake is already on its way), and a pipe failure pushes all its replies as one batch.

pub mod clipboard;
pub mod convert;
pub mod files;

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;

use swapdesk_ipc::{
    Events, FrameReader, FrameWriter, Handshake, IpcError, IpcRole, Message, Notify, ReplyBody, TermOutcome,
    status_code,
};
use swapdesk_proto::{CursorPosition, CursorShape, InputEvent, KeyframeRequest, ReceiverReport, VideoHeader};

use crate::api::{ApiCode, ApiError, Command, Reply, TermEvent, Ticket, UiBackend, UiEvent, UiWaker};
use convert::Route;

/// Receives the viewer media the service sends (`ViewerVideo`, `ViewerAudio`, `ViewerCursorPos/Shape`). Called on
/// the reader thread: it must hand the data to the viewer pipeline and return quickly.
pub trait ViewerSink: Send + Sync {
    fn video(&self, session_id: u64, header: VideoHeader, payload: Vec<u8>);
    fn audio(&self, session_id: u64, datagram: Vec<u8>);
    fn cursor_pos(&self, session_id: u64, pos: CursorPosition);
    fn cursor_shape(&self, session_id: u64, shape: CursorShape);
    /// Terminal output (`TermOutput`) of terminal `term_id`, at most [`swapdesk_ipc::MAX_TERM_CHUNK`] bytes. Called
    /// on the reader thread: it may wait for room (that is the back-pressure that stops the host's shell) but must
    /// not wait for the UI thread. The default drops it.
    fn term_output(&self, _term_id: u32, _bytes: Vec<u8>) {}
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    events: Mutex<VecDeque<UiEvent>>,
    /// `req -> (ticket, op number)`. `down` is only set while this lock is held, so no request is registered
    /// after the pending ones were failed.
    pending: Mutex<HashMap<u32, (Ticket, u16)>>,
    down: AtomicBool,
    waker: Arc<dyn UiWaker>,
    sink: Option<Arc<dyn ViewerSink>>,
    /// The clipboard thread's queue, once a provider is set; taken when the backend is dropped so the thread ends.
    clip: Mutex<Option<mpsc::Sender<Message>>>,
    /// Where an unanswerable clipboard call is answered from the reader thread (the same queue the backend writes).
    out: Mutex<Option<mpsc::Sender<Item>>>,
    /// The thread that answers the service's disk calls (`FsRequest`); taken away when the backend is dropped.
    fs: Mutex<Option<mpsc::Sender<Message>>>,
}

impl Inner {
    fn push(&self, batch: Vec<UiEvent>) {
        if batch.is_empty() {
            return;
        }
        let was_empty = {
            let mut q = lock(&self.events);
            let was_empty = q.is_empty();
            q.extend(batch);
            was_empty
        };
        if was_empty {
            self.waker.wake();
        }
    }

    fn reply(&self, ticket: Ticket, result: Result<Reply, ApiError>) {
        self.push(vec![UiEvent::Reply { ticket, result }]);
    }

    fn is_down(&self) -> bool {
        self.down.load(Ordering::Acquire)
    }

    /// The pipe is gone: every pending ticket fails with `ServiceDown`, in one batch.
    fn fail_all(&self) {
        let failed: Vec<UiEvent> = {
            let mut p = lock(&self.pending);
            self.down.store(true, Ordering::Release);
            let mut list: Vec<(Ticket, u16)> = p.drain().map(|(_, v)| v).collect();
            list.sort_by_key(|(t, _)| t.0);
            list.into_iter()
                .map(|(ticket, _)| UiEvent::Reply { ticket, result: Err(ApiError::new(ApiCode::ServiceDown)) })
                .collect()
        };
        self.push(failed);
    }

    fn answer(&self, req: u32, status: u16, body: &[u8]) {
        let Some((ticket, op)) = lock(&self.pending).remove(&req) else { return };
        let result = match status_code(status) {
            Ok(Some(code)) => Err(ApiError::new(convert::code_from_wire(code))),
            Ok(None) => match ReplyBody::decode(op, body) {
                // The open ran (the id, then the account the shell runs as) or was refused (why, in the error).
                Ok(ReplyBody::TermOpened { term_id, outcome, account }) => {
                    if outcome == TermOutcome::Opened {
                        self.push(vec![
                            UiEvent::Reply { ticket, result: Ok(Reply::TermId(term_id)) },
                            UiEvent::Term { term_id, event: TermEvent::Opened { account: account.0 } },
                        ]);
                    } else {
                        self.reply(ticket, Err(convert::term_outcome_error(outcome)));
                    }
                    return;
                }
                Ok(b) => convert::reply_from_wire(b).map_err(|_| ApiError::new(ApiCode::Internal)),
                Err(_) => Err(ApiError::new(ApiCode::Internal)),
            },
            Err(_) => Err(ApiError::new(ApiCode::Internal)),
        };
        self.reply(ticket, result);
    }

    fn handle(&self, msg: Message) {
        match msg {
            Message::UiReply { req, status, body } => self.answer(req, status, &body.0),
            Message::UiNotify(n) => self.notify(n),
            Message::ViewerVideo { session_id, header, payload } => {
                if let Some(s) = &self.sink {
                    // The reader made this buffer for this frame alone, so the sink gets the buffer itself: turning
                    // the only handle to it back into a `Vec` copies nothing.
                    s.video(session_id, header, payload.0.into());
                }
            }
            Message::ViewerAudio { session_id, datagram } => {
                if let Some(s) = &self.sink {
                    s.audio(session_id, datagram.0);
                }
            }
            Message::ViewerCursorPos { session_id, pos } => {
                if let Some(s) = &self.sink {
                    s.cursor_pos(session_id, pos);
                }
            }
            Message::ViewerCursorShape { session_id, shape } => {
                if let Some(s) = &self.sink {
                    s.cursor_shape(session_id, shape.0);
                }
            }
            Message::ClipChangeCount { .. } | Message::ClipRead { .. } | Message::ClipWrite { .. } => {
                self.clipboard_call(msg)
            }
            Message::TermOutput { term_id, bytes } => {
                if let Some(s) = &self.sink {
                    s.term_output(term_id, bytes.0);
                }
            }
            // The service asks this process (the user's own) to read a picked file or write into the receive folder.
            Message::FsRequest { req, call } => {
                if let Some(tx) = lock(&self.fs).as_ref() {
                    let _ = tx.send(Message::FsRequest { req, call });
                }
            }
            // Anything else is not for a UI connection; ignored.
            _ => {}
        }
    }

    /// A clipboard call of the service: to the clipboard thread, or a failure when this UI has no clipboard.
    fn clipboard_call(&self, msg: Message) {
        let mut slot = lock(&self.clip);
        let sent = match slot.as_ref() {
            Some(tx) => tx.send(msg).map_err(|e| e.0),
            None => Err(msg),
        };
        let Err(msg) = sent else { return };
        // No clipboard thread (or it ended): answered below.
        *slot = None;
        drop(slot);
        if let (Some(reply), Some(out)) = (clipboard::no_clipboard(&msg), lock(&self.out).as_ref()) {
            let _ = out.send(Item::Viewer(reply));
        }
    }

    fn notify(&self, n: Notify) {
        if let Ok(e) = convert::notify_to_event(n) {
            self.push(vec![e]);
        }
    }
}

/// The route of a command: the plain one, except for the files commands that hand out tokens.
fn routed(command: &Command, tokens: &swapdesk_files::Tokens) -> Route {
    match files::route_files(command, tokens) {
        Some(Ok(op)) => Route::Op(op),
        Some(Err(_)) => Route::Invalid,
        None => convert::route(command),
    }
}

enum Item {
    Command(Ticket, Command),
    Viewer(Message),
}

/// The C6 backend over the UI pipe. Dropping it closes the writing half; the reader thread ends when the service
/// closes its end.
pub struct ServiceBackend {
    inner: Arc<Inner>,
    tx: mpsc::Sender<Item>,
    next_ticket: AtomicU64,
    win_session: u32,
    /// What the service may reach on this machine: the files the user picked and the folders chosen to receive into.
    disk: files::Disk,
}

impl ServiceBackend {
    /// Handshake as role `Ui` with `pid`, then start the backend and reader threads. No viewer sink: viewer media
    /// is dropped.
    pub fn start(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        waker: Arc<dyn UiWaker>,
        pid: u32,
    ) -> Result<Self, IpcError> {
        Self::start_with_sink(reader, writer, waker, pid, None)
    }

    /// As [`start`](Self::start), with the sink that receives the viewer media messages.
    pub fn start_with_sink(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        waker: Arc<dyn UiWaker>,
        pid: u32,
        sink: Option<Arc<dyn ViewerSink>>,
    ) -> Result<Self, IpcError> {
        let mut reader = FrameReader::new(reader);
        let mut writer = FrameWriter::new(writer);
        let hello = Handshake::client(&mut reader, &mut writer, IpcRole::Ui, pid)?;
        let inner = Arc::new(Inner {
            events: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashMap::new()),
            down: AtomicBool::new(false),
            waker,
            sink,
            clip: Mutex::new(None),
            out: Mutex::new(None),
            fs: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel();
        *lock(&inner.out) = Some(tx.clone());
        let disk = files::Disk::new();
        {
            let to_writer = tx.clone();
            let fs_tx = disk.spawn_server(move |msg| {
                let _ = to_writer.send(Item::Viewer(msg));
            })?;
            *lock(&inner.fs) = Some(fs_tx);
        }
        let tokens = Arc::clone(&disk.tokens);

        let reading = Arc::clone(&inner);
        thread::Builder::new().name("swapdesk-ui-pipe-read".into()).spawn(move || {
            loop {
                match reader.read_message() {
                    Ok(Some(Message::Bye { .. })) | Ok(None) | Err(_) => break,
                    Ok(Some(msg)) => reading.handle(msg),
                }
            }
            reading.fail_all();
        })?;

        let writing = Arc::clone(&inner);
        thread::Builder::new().name("swapdesk-ui-pipe-write".into()).spawn(move || {
            let next_req = AtomicU32::new(1);
            while let Ok(item) = rx.recv() {
                match item {
                    Item::Command(ticket, command) => match routed(&command, &tokens) {
                        Route::LocalDone => writing.reply(ticket, Ok(Reply::Done)),
                        Route::Unsupported => writing.reply(ticket, Err(ApiError::new(ApiCode::Internal))),
                        Route::Messages(list) => {
                            if writing.is_down() {
                                writing.reply(ticket, Err(ApiError::new(ApiCode::ServiceDown)));
                                continue;
                            }
                            if list.iter().any(|m| writer.write_message(m).is_err()) {
                                writing.fail_all();
                                writing.reply(ticket, Err(ApiError::new(ApiCode::ServiceDown)));
                                break;
                            }
                            writing.reply(ticket, Ok(Reply::Done));
                        }
                        Route::Invalid => writing.reply(ticket, Err(ApiError::new(ApiCode::Invalid))),
                        Route::Op(op) => {
                            let req = next_req.fetch_add(1, Ordering::Relaxed);
                            {
                                let mut p = lock(&writing.pending);
                                if writing.is_down() {
                                    drop(p);
                                    writing.reply(ticket, Err(ApiError::new(ApiCode::ServiceDown)));
                                    continue;
                                }
                                p.insert(req, (ticket, op.code()));
                            }
                            if writer.write_message(&Message::UiRequest { req, op }).is_err() {
                                writing.fail_all();
                                break;
                            }
                        }
                    },
                    Item::Viewer(msg) => {
                        if !writing.is_down() && writer.write_message(&msg).is_err() {
                            writing.fail_all();
                            break;
                        }
                    }
                }
            }
        })?;

        Ok(ServiceBackend { inner, tx, next_ticket: AtomicU64::new(1), win_session: hello.win_session, disk })
    }

    /// Gives the backend this user's clipboard: the service reads and writes it for the clipboard sync of a viewer
    /// session. The provider runs on a thread of its own.
    pub fn set_clipboard(&self, mut provider: impl swapdesk_platform::ClipboardProvider + Send + 'static) {
        let (ctx, crx) = mpsc::channel::<Message>();
        let out = self.tx.clone();
        let spawned = thread::Builder::new().name("swapdesk-ui-clipboard".into()).spawn(move || {
            while let Ok(msg) = crx.recv() {
                if let Some(reply) = clipboard::answer_clipboard(&mut provider, &msg) {
                    let _ = out.send(Item::Viewer(reply));
                }
            }
        });
        if spawned.is_ok() {
            *lock(&self.inner.clip) = Some(ctx);
        }
    }

    /// The Windows session id the service reported in `HelloAck`.
    pub fn win_session(&self) -> u32 {
        self.win_session
    }

    /// True once the pipe closed or broke.
    pub fn is_down(&self) -> bool {
        self.inner.is_down()
    }

    /// The tokens of the files the service may read and the folders it may write into (tests and the shells' dialogs).
    pub fn tokens(&self) -> &Arc<swapdesk_files::Tokens> {
        &self.disk.tokens
    }

    /// Writes `ViewerInput` (UI to service). Never blocks; dropped when the service is down.
    pub fn viewer_input(&self, session_id: u64, events: Vec<InputEvent>) {
        self.viewer(Message::ViewerInput { session_id, events: Events(events) });
    }

    /// Writes `ViewerKeyframe`.
    pub fn viewer_keyframe(&self, session_id: u64, request: KeyframeRequest) {
        self.viewer(Message::ViewerKeyframe { session_id, request });
    }

    /// Writes `ViewerReport`.
    pub fn viewer_report(&self, session_id: u64, report: ReceiverReport) {
        self.viewer(Message::ViewerReport { session_id, report });
    }

    fn viewer(&self, msg: Message) {
        // A closed queue means the backend thread ended: the service is down and the message has nowhere to go.
        let _ = self.tx.send(Item::Viewer(msg));
    }
}

impl Drop for ServiceBackend {
    fn drop(&mut self) {
        // The clipboard thread and the reader thread hold queue ends: let them go so the writer thread ends.
        *lock(&self.inner.clip) = None;
        *lock(&self.inner.out) = None;
        // Ends the disk thread, which holds a way to write to the pipe.
        lock(&self.inner.fs).take();
    }
}

impl UiBackend for ServiceBackend {
    fn send(&self, command: Command) -> Ticket {
        let ticket = Ticket(self.next_ticket.fetch_add(1, Ordering::Relaxed));
        if self.inner.is_down() || self.tx.send(Item::Command(ticket, command)).is_err() {
            self.inner.reply(ticket, Err(ApiError::new(ApiCode::ServiceDown)));
        }
        ticket
    }

    fn drain(&self) -> Vec<UiEvent> {
        lock(&self.inner.events).drain(..).collect()
    }
}

#[cfg(test)]
mod tests;
