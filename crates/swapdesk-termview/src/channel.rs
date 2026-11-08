//! The bounded output channel between the network side and the terminal model (C6: 256 chunks, GUESS), and the
//! `TermView` that owns the model and drains the channel with a byte budget per call.

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};

use crate::term::Term;

/// Chunks the channel holds before the sender waits (C6, GUESS).
pub const OUTPUT_CHANNEL_CHUNKS: usize = 256;

/// The receiving side is gone (the terminal panel closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

/// The network side's end: output chunks from the 0x10 stream go in here.
#[derive(Clone)]
pub struct OutputTx(SyncSender<Vec<u8>>);

impl OutputTx {
    /// Waits for room (this is the back-pressure: a full channel stops the stream reader, so QUIC flow control stops
    /// the host's PTY reader). Meant for a dedicated thread, as `blocking_send` is.
    pub fn send(&self, chunk: Vec<u8>) -> Result<(), Closed> {
        self.0.send(chunk).map_err(|_| Closed)
    }

    /// Never waits: `Err(Some(chunk))` hands the chunk back when the channel is full, `Err(None)` when it is closed.
    pub fn try_send(&self, chunk: Vec<u8>) -> Result<(), Option<Vec<u8>>> {
        self.0.try_send(chunk).map_err(|e| match e {
            TrySendError::Full(c) => Some(c),
            TrySendError::Disconnected(_) => None,
        })
    }
}

/// A channel of `capacity` chunks (at least 1).
pub fn output_channel(capacity: usize) -> (OutputTx, Receiver<Vec<u8>>) {
    let (tx, rx) = sync_channel(capacity.max(1));
    (OutputTx(tx), rx)
}

/// What one [`TermView::pump`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pump {
    pub bytes: usize,
    pub chunks: usize,
    /// The budget ran out with chunks still queued: pump again (next frame) rather than loop.
    pub more: bool,
    /// The sender is gone and the queue is empty: the terminal ended.
    pub closed: bool,
    /// What the terminal answers to the host program; the owner writes it to the stream (input direction).
    pub replies: Vec<u8>,
}

/// The terminal model with its output channel: the UI thread owns it and calls [`pump`](Self::pump) when woken.
pub struct TermView {
    pub term: Term,
    rx: Receiver<Vec<u8>>,
}

impl TermView {
    /// A model of `cols` x `rows` with `scrollback` rows of history and its channel of [`OUTPUT_CHANNEL_CHUNKS`].
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> (TermView, OutputTx) {
        Self::with_capacity(cols, rows, scrollback, OUTPUT_CHANNEL_CHUNKS)
    }

    pub fn with_capacity(cols: usize, rows: usize, scrollback: usize, chunks: usize) -> (TermView, OutputTx) {
        let (tx, rx) = output_channel(chunks);
        (TermView { term: Term::new(cols, rows, scrollback), rx }, tx)
    }

    /// Feeds queued chunks to the parser until `budget` bytes were parsed (a chunk in progress is finished), so one
    /// burst of output cannot hold the UI thread (R-UI-1).
    pub fn pump(&mut self, budget: usize) -> Pump {
        let mut out = Pump::default();
        loop {
            if out.bytes >= budget {
                out.more = true;
                break;
            }
            match self.rx.try_recv() {
                Ok(chunk) => {
                    out.bytes += chunk.len();
                    out.chunks += 1;
                    self.term.feed(&chunk);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    out.closed = true;
                    break;
                }
            }
        }
        out.replies = self.term.screen_mut().take_replies();
        out
    }
}
