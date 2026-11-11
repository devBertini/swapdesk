//! The control stream as two bounded channels (PROTOCOL.md 5.1). The state machine reads `Inbound` items and writes
//! through an [`Outbox`], so it runs the same over a QUIC bidirectional stream and over the in-memory pair the tests
//! use with a paused clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use quinn::{RecvStream, SendStream};
use swapdesk_proto::{ErrorCode, FrameHeader, Message, Wire};
use tokio::sync::{mpsc, oneshot};

/// Priority of the control stream (PROTOCOL.md 4).
pub const CONTROL_PRIORITY: i32 = 3;
/// Once a frame has begun, the rest of it must arrive within this long (a stalled peer cannot hold a task forever).
pub const FRAME_STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// A message waits at most this long for room in the outgoing queue, and a write at most this long to be accepted.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const QUEUE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The peer sent something the format forbids (or a frame over its limit): fatal, with this code.
    Violation(ErrorCode),
    /// The stream ended or failed without a `Bye`: the connection is lost.
    Lost,
}

pub type Inbound = Result<Message, LinkError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The link is gone.
    Closed,
    /// The queue did not drain within [`SEND_TIMEOUT`]: the peer is not reading.
    Stalled,
    /// The message does not fit a frame.
    TooLarge,
    /// Over a per-session cap (chat).
    Busy,
}

enum Out {
    Msg(Message),
    Flush(oneshot::Sender<()>),
    /// Finish the stream, wait for the peer to acknowledge what was written, then answer.
    Close(oneshot::Sender<()>),
}

/// The write side of the control stream. Cheap to clone.
///
/// Every control message of a session leaves through it, so it is where the rule of PROTOCOL.md 1 is kept: a peer
/// is sent nothing newer than the protocol minor it announced. A value it does not know goes as the older value
/// that says the same, and a message type it does not know is not sent at all (`Ok`: there is nothing to retry).
#[derive(Clone)]
pub struct Outbox {
    tx: mpsc::Sender<Out>,
    /// The minor the peer announced; 0 until its `Hello` or `HelloAck`, which is also what 1.0 says.
    peer_minor: Arc<AtomicU16>,
}

impl Outbox {
    fn new(tx: mpsc::Sender<Out>) -> Self {
        Self { tx, peer_minor: Arc::new(AtomicU16::new(0)) }
    }

    /// The peer announced `minor`: from now on it may be sent what that minor knows.
    pub(crate) fn set_peer_minor(&self, minor: u16) {
        self.peer_minor.store(minor, Ordering::SeqCst);
    }

    fn for_peer(&self, msg: Message) -> Option<Message> {
        msg.for_peer(self.peer_minor.load(Ordering::SeqCst))
    }

    pub async fn send(&self, msg: impl Into<Message>) -> Result<(), SendError> {
        let Some(msg) = self.for_peer(msg.into()) else { return Ok(()) };
        match tokio::time::timeout(SEND_TIMEOUT, self.tx.send(Out::Msg(msg))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(SendError::Closed),
            Err(_) => Err(SendError::Stalled),
        }
    }

    pub fn try_send(&self, msg: impl Into<Message>) -> Result<(), SendError> {
        let Some(msg) = self.for_peer(msg.into()) else { return Ok(()) };
        self.tx.try_send(Out::Msg(msg)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Stalled,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }

    /// Waits until everything queued before this call was written (or `limit` passed).
    pub async fn flush(&self, limit: Duration) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Out::Flush(tx)).await.is_ok() {
            let _ = tokio::time::timeout(limit, rx).await;
        }
    }

    /// Writes what is queued, finishes the stream and waits (up to `limit`) until the peer has acknowledged it, so a
    /// `Bye` or an `Error` is not lost to the connection close that follows.
    pub async fn close(&self, limit: Duration) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Out::Close(tx)).await.is_ok() {
            let _ = tokio::time::timeout(limit, rx).await;
        }
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Both halves of a control stream.
pub struct ControlIo {
    pub out: Outbox,
    pub rx: mpsc::Receiver<Inbound>,
}

/// Two ends joined by memory, for tests of the state machine with a paused clock.
pub fn memory_pair() -> (ControlIo, ControlIo) {
    fn half(to_peer: mpsc::Sender<Inbound>) -> Outbox {
        let (tx, mut rx) = mpsc::channel::<Out>(QUEUE);
        tokio::spawn(async move {
            while let Some(out) = rx.recv().await {
                match out {
                    Out::Msg(m) => {
                        if to_peer.send(Ok(m)).await.is_err() {
                            return;
                        }
                    }
                    Out::Flush(done) => {
                        let _ = done.send(());
                    }
                    Out::Close(done) => {
                        let _ = done.send(());
                        return;
                    }
                }
            }
        });
        Outbox::new(tx)
    }
    let (a_in_tx, a_in_rx) = mpsc::channel(QUEUE);
    let (b_in_tx, b_in_rx) = mpsc::channel(QUEUE);
    (ControlIo { out: half(b_in_tx), rx: a_in_rx }, ControlIo { out: half(a_in_tx), rx: b_in_rx })
}

/// One varint's bytes off a stream (1, 2, 4 or 8 bytes); the codec checks minimality afterwards. `None` at a clean
/// end of the stream before the first byte.
pub(crate) async fn read_varint_bytes(
    recv: &mut RecvStream,
    out: &mut Vec<u8>,
    first_timeout: Option<Duration>,
) -> Result<bool, LinkError> {
    let mut first = [0u8; 1];
    let read = recv.read_exact(&mut first);
    let got = match first_timeout {
        None => read.await,
        Some(t) => tokio::time::timeout(t, read).await.map_err(|_| LinkError::Lost)?,
    };
    match got {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(false),
        Err(_) => return Err(LinkError::Lost),
    }
    out.push(first[0]);
    let rest = (1usize << (first[0] >> 6)) - 1;
    let mut buf = [0u8; 7];
    tokio::time::timeout(FRAME_STEP_TIMEOUT, recv.read_exact(&mut buf[..rest]))
        .await
        .map_err(|_| LinkError::Lost)?
        .map_err(|_| LinkError::Lost)?;
    out.extend_from_slice(&buf[..rest]);
    Ok(true)
}

fn violation(e: swapdesk_proto::CodecError) -> LinkError {
    LinkError::Violation(e.code())
}

async fn read_frame(recv: &mut RecvStream) -> Result<Option<Message>, LinkError> {
    let mut head = Vec::with_capacity(4);
    // Waiting for the next frame has no timeout of its own: silence is the liveness probe's business.
    if !read_varint_bytes(recv, &mut head, None).await? {
        return Ok(None);
    }
    read_varint_bytes(recv, &mut head, Some(FRAME_STEP_TIMEOUT)).await?;
    let header = FrameHeader::decode(&head).map_err(violation)?;
    let mut payload = vec![0u8; header.len];
    tokio::time::timeout(FRAME_STEP_TIMEOUT, recv.read_exact(&mut payload))
        .await
        .map_err(|_| LinkError::Lost)?
        .map_err(|_| LinkError::Lost)?;
    Message::decode_payload(u64::from(header.type_id), &payload).map(Some).map_err(violation)
}

impl ControlIo {
    /// Takes the control stream of a connection: the client's first bidirectional stream.
    pub fn from_quinn(mut send: SendStream, mut recv: RecvStream) -> Self {
        let _ = send.set_priority(CONTROL_PRIORITY);
        let (in_tx, in_rx) = mpsc::channel::<Inbound>(QUEUE);
        tokio::spawn(async move {
            loop {
                match read_frame(&mut recv).await {
                    Ok(Some(msg)) => {
                        if in_tx.send(Ok(msg)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = in_tx.send(Err(LinkError::Lost)).await;
                        return;
                    }
                    Err(e) => {
                        let _ = in_tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });
        let (out_tx, mut out_rx) = mpsc::channel::<Out>(QUEUE);
        tokio::spawn(async move {
            while let Some(out) = out_rx.recv().await {
                match out {
                    Out::Msg(m) => {
                        let Ok(frame) = m.encode_frame() else { continue };
                        match tokio::time::timeout(SEND_TIMEOUT, send.write_all(&frame)).await {
                            Ok(Ok(())) => {}
                            _ => return,
                        }
                    }
                    Out::Flush(done) => {
                        let _ = done.send(());
                    }
                    Out::Close(done) => {
                        let _ = send.finish();
                        let _ = tokio::time::timeout(Duration::from_secs(1), send.stopped()).await;
                        let _ = done.send(());
                        return;
                    }
                }
            }
            let _ = send.finish();
        });
        Self { out: Outbox::new(out_tx), rx: in_rx }
    }
}
