//! The service's end of a helper pipe (C2.1, C2.4): the server handshake, one reader thread and one writer path.
//!
//! The reader thread turns each helper message into a [`HelperEvent`] on a bounded channel (`blocking_send`: a slow
//! consumer slows the reader, the pipe fills, and the helper's own writer drops media instead of waiting, which is
//! the policy of [`crate::outbox`]). The writer path is the [`Outbox`] and its writer thread: control messages are
//! never dropped, and an async caller waits for room with a sleep, never by blocking a runtime thread.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use swapdesk_files::{FsCaller, Pending, PipeCaller};
use swapdesk_ipc::{AudioModeKind, FrameReader, FrameWriter, Handshake, IpcError, IpcRole, Message};
use swapdesk_proto::{CursorPosition, CursorShape, Desktop, Display, DisplayList, ErrorCode};
use tokio::sync::mpsc;

use super::routing::HelperKind;
use crate::outbox::{Outbox, TryControl, spawn_writer};
use crate::rpc::Rpc;

/// Queued control messages toward a helper before a sender waits.
const CONTROL_QUEUE: usize = 256;
/// Events queued from the reader thread before it waits for the consumer.
const EVENT_QUEUE: usize = 64;
/// How long an async sender sleeps between two looks at a full queue.
const FULL_RETRY: Duration = Duration::from_millis(2);

/// What a helper told the service.
#[derive(Debug)]
pub enum HelperEvent {
    Displays(DisplayList),
    Video {
        header: swapdesk_proto::VideoHeader,
        payload: Vec<u8>,
    },
    CursorPos(CursorPosition),
    CursorShape(CursorShape),
    /// One audio datagram, PROTOCOL.md 7.2 header included.
    Audio(Vec<u8>),
    AudioMode(AudioModeKind),
    Desktop(Desktop),
    SasRequest,
    Fault {
        code: ErrorCode,
        what: String,
    },
    /// The terminal proxy (C2.4 "Terminal"): the helper's answers and output for the shells it runs. The bytes are
    /// as private as typed text: nothing prints them.
    PtyOpenReply {
        id: u32,
        code: ErrorCode,
        account: String,
    },
    PtyOutput {
        id: u32,
        bytes: Vec<u8>,
    },
    PtyExited {
        id: u32,
        code: Option<i32>,
    },
    /// The pipe ended (EOF, a framing error, a protocol error) or the helper sent `Bye`. Always the last event.
    Gone {
        error: Option<String>,
    },
}

/// Why a helper could not be attached.
#[derive(Debug)]
pub enum AttachError {
    /// The role is the UI's, not a helper's.
    NotAHelperRole(IpcRole),
    /// The handshake failed (framing, version, or the first message was not `Hello`).
    Handshake(IpcError),
    /// The `Hello` carried another role than the caller expected.
    RoleMismatch {
        expected: IpcRole,
        got: IpcRole,
    },
    Spawn(std::io::Error),
}

impl core::fmt::Display for AttachError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAHelperRole(r) => write!(f, "{r:?} is not a helper role"),
            Self::Handshake(e) => write!(f, "helper handshake: {e}"),
            Self::RoleMismatch { expected, got } => write!(f, "expected a {expected:?}, the peer said {got:?}"),
            Self::Spawn(e) => write!(f, "spawn: {e}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// One attached helper.
pub struct HelperLink {
    pub(crate) id: u64,
    kind: HelperKind,
    pid: u32,
    outbox: Outbox,
    displays: Mutex<Vec<Display>>,
    /// Blocking requests to this helper (clipboard, quick actions): the answers come back through the reader thread.
    rpc: Arc<Rpc>,
    /// The disk proxy toward this helper (C2.4 "Files"): its calls wait for the replies the reader thread routes.
    fs: Arc<PipeCaller>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl HelperLink {
    pub fn kind(&self) -> HelperKind {
        self.kind
    }

    /// The process id the helper announced in its `Hello`.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// False once the pipe is closed.
    pub fn is_alive(&self) -> bool {
        !self.outbox.is_closed()
    }

    /// The disk of the user this helper runs as, for `PipeFs` (only the user helper serves one).
    pub fn fs_caller(&self) -> Arc<dyn FsCaller> {
        self.fs.clone()
    }

    /// The displays of the helper's last `Displays` message.
    pub fn displays(&self) -> Vec<Display> {
        self.displays.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// The request side of the pipe, for the proxies that run on a thread that may block.
    pub fn rpc(&self) -> Arc<Rpc> {
        Arc::clone(&self.rpc)
    }

    pub(crate) fn set_displays(&self, list: Vec<Display>) {
        *self.displays.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = list;
    }

    /// Queues a control message; waits (asynchronously) for room. `false` when the pipe is closed.
    pub async fn send(&self, msg: Message) -> bool {
        let mut msg = msg;
        loop {
            match self.outbox.try_send_control(msg) {
                TryControl::Sent => return true,
                TryControl::Closed => return false,
                TryControl::Full(back) => {
                    msg = back;
                    tokio::time::sleep(FULL_RETRY).await;
                }
            }
        }
    }

    /// Queues a control message without waiting; a full queue loses it (shutdown paths only).
    pub fn send_now(&self, msg: Message) -> bool {
        matches!(self.outbox.try_send_control(msg), TryControl::Sent)
    }

    /// The same for a thread that is not on the runtime (it may block for room).
    pub fn send_blocking(&self, msg: Message) -> bool {
        self.outbox.send_control(msg)
    }
}

/// The helper's pipe after the handshake: the link, the events and the writer thread to join.
pub(crate) struct Attached {
    pub link: HelperLink,
    pub events: mpsc::Receiver<HelperEvent>,
}

/// Does the service side of the handshake on `reader` / `writer`, then starts the reader thread and the writer
/// thread. Blocks until the helper's `Hello` arrives: call it from the supervisor's thread, never from a runtime
/// task.
pub(crate) fn attach<R, W>(role: IpcRole, win_session: u32, reader: R, writer: W) -> Result<Attached, AttachError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let kind = HelperKind::of_role(role).ok_or(AttachError::NotAHelperRole(role))?;
    let mut reader = FrameReader::new(reader);
    let mut framed = FrameWriter::new(writer);
    let hello = Handshake::server(&mut reader, &mut framed, win_session).map_err(AttachError::Handshake)?;
    if hello.role != role {
        return Err(AttachError::RoleMismatch { expected: role, got: hello.role });
    }
    let (outbox, rx) = Outbox::new(CONTROL_QUEUE);
    // The writer thread ends when every `Outbox` clone is gone; its report is of no use to the service (a broken
    // pipe shows as `Gone` from the reader).
    drop(spawn_writer(&outbox, rx, framed).map_err(AttachError::Spawn)?);
    let (tx, events) = mpsc::channel(EVENT_QUEUE);
    let closer = outbox.clone();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let sender = outbox.clone();
    let rpc = Rpc::new(move |m| sender.send_control(m));
    let reading = Arc::clone(&rpc);
    let pending = Arc::new(Pending::new());
    let writer_side = outbox.clone();
    let fs = Arc::new(PipeCaller::new(id, Arc::clone(&pending), move |msg| writer_side.send_control(msg)));
    std::thread::Builder::new()
        .name(format!("service-helper-{kind:?}"))
        .spawn(move || read_loop(reader, &tx, &closer, &reading, &pending))
        .map_err(AttachError::Spawn)?;
    let link = HelperLink { id, kind, pid: hello.pid, outbox, displays: Mutex::new(Vec::new()), rpc, fs };
    Ok(Attached { link, events })
}

fn read_loop<R: Read>(
    mut reader: FrameReader<R>,
    tx: &mpsc::Sender<HelperEvent>,
    outbox: &Outbox,
    rpc: &Rpc,
    fs: &Pending,
) {
    let error = loop {
        let msg = match reader.read_message() {
            Ok(Some(m)) => m,
            Ok(None) => break None,
            Err(e) => break Some(e.to_string()),
        };
        let event = match msg {
            Message::Displays(list) => HelperEvent::Displays(list),
            Message::VideoFrame { header, payload } => HelperEvent::Video { header, payload: payload.0 },
            Message::CursorPos(p) => HelperEvent::CursorPos(p),
            Message::CursorShape(s) => HelperEvent::CursorShape(s.0),
            Message::AudioPacket(d) => HelperEvent::Audio(d.0),
            Message::AudioMode { mode } => HelperEvent::AudioMode(mode),
            Message::DesktopChanged(d) => HelperEvent::Desktop(d),
            Message::SasRequest => HelperEvent::SasRequest,
            Message::HelperFault { code, what } => HelperEvent::Fault { code, what: what.0 },
            // The answer to a blocking request of the service: its waiter takes it, no event.
            m @ (Message::ClipReply { .. } | Message::ActionReply { .. }) => {
                rpc.resolve(m);
                continue;
            }
            Message::PtyOpenReply { id, code, account } => HelperEvent::PtyOpenReply { id, code, account: account.0 },
            Message::PtyOutput { id, bytes } => HelperEvent::PtyOutput { id, bytes: bytes.0 },
            Message::PtyExited { id, code } => HelperEvent::PtyExited { id, code },
            // A disk answer goes straight to the call that waits for it (on a thread of its own, not an event).
            Message::FsReply { req, result } => {
                fs.complete(req, result);
                continue;
            }
            Message::Bye { .. } => break None,
            other => break Some(format!("unexpected message {:#x} from a helper", other.type_code())),
        };
        if tx.blocking_send(event).is_err() {
            // Nobody listens any more (the core is shutting down).
            break None;
        }
    };
    // Whatever ended the pipe, nothing can be written to it from now on either, and the disk calls in flight fail
    // (the read handles of that helper died with it).
    outbox.close();
    rpc.close();
    fs.close();
    let _ = tx.blocking_send(HelperEvent::Gone { error });
}
