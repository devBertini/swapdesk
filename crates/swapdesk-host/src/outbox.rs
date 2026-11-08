//! The one writer of a helper's pipe: a bounded queue in front of a single writer thread.
//!
//! Policy: media (`VideoFrame`, `AudioPacket`, `CursorPos`) never waits for a slow pipe: when the queue is full the
//! message is dropped and counted. Every other message is control and is never dropped: its sender waits for room.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

use swapdesk_ipc::{FrameWriter, IpcError, Message};

/// A cheap handle to the queue; every thread of the helper holds a clone.
#[derive(Clone)]
pub struct Outbox {
    tx: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
    skipped: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
}

/// What the writer thread reports when it ends.
#[derive(Debug)]
pub struct WriterReport {
    /// The first I/O error that closed the pipe, if any.
    pub io_error: Option<IpcError>,
    /// Messages that could not be encoded (over their limit) and were skipped; the pipe stayed open.
    pub skipped: u64,
}

/// The result of [`Outbox::try_send_control`].
#[derive(Debug)]
pub enum TryControl {
    Sent,
    /// The queue is full: the message comes back.
    Full(Message),
    /// The pipe is closed: nothing can be sent any more.
    Closed,
}

/// Media messages are droppable; everything else is control.
pub fn is_media(msg: &Message) -> bool {
    matches!(msg, Message::VideoFrame { .. } | Message::AudioPacket(_) | Message::CursorPos(_))
}

impl Outbox {
    /// A queue of `capacity` messages (at least 1) and the receiving end for the writer thread.
    pub fn new(capacity: usize) -> (Self, Receiver<Message>) {
        let (tx, rx) = sync_channel(capacity.max(1));
        let outbox = Self { tx, dropped: Arc::default(), skipped: Arc::default(), closed: Arc::default() };
        (outbox, rx)
    }

    /// Queues a media message without waiting. `false` when it was dropped (queue full, counted) or the pipe is closed.
    pub fn send_media(&self, msg: Message) -> bool {
        if self.is_closed() {
            return false;
        }
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Queues a control message, waiting for room. `false` only when the pipe is closed.
    pub fn send_control(&self, msg: Message) -> bool {
        if self.is_closed() {
            return false;
        }
        self.tx.send(msg).is_ok()
    }

    /// Queues a control message without waiting: `Full` hands the message back so an async caller can retry after a
    /// sleep instead of blocking a runtime thread.
    pub fn try_send_control(&self, msg: Message) -> TryControl {
        if self.is_closed() {
            return TryControl::Closed;
        }
        match self.tx.try_send(msg) {
            Ok(()) => TryControl::Sent,
            Err(TrySendError::Full(m)) => TryControl::Full(m),
            Err(TrySendError::Disconnected(_)) => TryControl::Closed,
        }
    }

    /// Routes by [`is_media`].
    pub fn send(&self, msg: Message) -> bool {
        if is_media(&msg) { self.send_media(msg) } else { self.send_control(msg) }
    }

    /// Marks the pipe closed (its reading side ended): later sends return `false` and queued messages are discarded.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Media messages dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// True once the writer thread met an I/O error: nothing more can be sent.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Runs the writer thread: it writes every queued message in order and ends when every [`Outbox`] clone is gone.
/// After an I/O error it keeps draining (so no sender blocks) and discards.
pub fn spawn_writer<W: Write + Send + 'static>(
    outbox: &Outbox,
    rx: Receiver<Message>,
    mut writer: FrameWriter<W>,
) -> std::io::Result<JoinHandle<WriterReport>> {
    let closed = Arc::clone(&outbox.closed);
    let skipped = Arc::clone(&outbox.skipped);
    std::thread::Builder::new().name("helper-writer".into()).spawn(move || {
        let mut io_error = None;
        while let Ok(msg) = rx.recv() {
            if closed.load(Ordering::Acquire) {
                continue;
            }
            match writer.write_message(&msg) {
                Ok(()) => {}
                Err(IpcError::Io(e)) => {
                    closed.store(true, Ordering::Release);
                    io_error = Some(IpcError::Io(e));
                }
                Err(_) => {
                    skipped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        WriterReport { io_error, skipped: skipped.load(Ordering::Relaxed) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    use swapdesk_ipc::Bytes;
    use swapdesk_proto::VideoHeader;

    fn video(n: u32) -> Message {
        let header = VideoHeader {
            keyframe: false,
            config: false,
            discontinuity: false,
            display_id: 0,
            frame_id: n,
            capture_us: 0,
            encode_us: 0,
            width: 1,
            height: 1,
            codec: swapdesk_proto::Codec::H264,
            payload_len: 1,
        };
        Message::VideoFrame { header, payload: Bytes(vec![0]) }
    }

    #[test]
    fn a_full_queue_drops_video_but_never_control() {
        let (outbox, rx) = Outbox::new(2);
        assert!(outbox.send_media(video(0)));
        assert!(outbox.send_media(video(1)));
        assert!(!outbox.send_media(video(2)));
        assert!(!outbox.send(video(3)));
        assert!(!outbox.send_media(Message::AudioPacket(Bytes(vec![1]))));
        assert_eq!(outbox.dropped(), 3);

        // A control message waits for room instead of being dropped.
        let (done_tx, done_rx) = mpsc::channel();
        let sender = outbox.clone();
        let t = std::thread::spawn(move || {
            let ok = sender.send_control(Message::StopAudio);
            done_tx.send(ok).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(150)).is_err());
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { .. }));
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        t.join().unwrap();

        // Queue now: video(1), StopAudio. Control is all there, only the dropped video is missing.
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { header, .. } if header.frame_id == 1));
        assert_eq!(rx.recv().unwrap(), Message::StopAudio);
        assert!(rx.try_recv().is_err());
        assert_eq!(outbox.dropped(), 3);
    }

    #[test]
    fn control_is_not_media() {
        assert!(!is_media(&Message::Shutdown));
        assert!(!is_media(&Message::SasRequest));
        assert!(is_media(&video(0)));
    }
}
