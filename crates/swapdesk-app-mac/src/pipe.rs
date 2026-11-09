//! The in-memory duplex between the UI hub's `serve` and the shell's `ServiceBackend` (D27: the app embeds the service,
//! so the C2.4 UI pipe is a pair of byte channels instead of a named pipe or a socket). The frames on it are the same
//! the Windows pipe carries, so the shell's backend is the same `ServiceBackend`.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel};
use std::time::Duration;

/// How often a blocked read looks at its cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);

pub struct PipeReader {
    rx: Receiver<Vec<u8>>,
    left: Vec<u8>,
    pos: usize,
    cancel: Option<Arc<AtomicBool>>,
}

impl PipeReader {
    /// Reads end of file once `flag` is set, even while the other end still holds its writer: how the app stops the hub's
    /// `serve` (blocked in a read) at quit, since the shell keeps the backend, and with it the other end, alive.
    pub fn cancel_with(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = Some(flag);
    }
}

pub struct PipeWriter {
    tx: Tx,
}

enum Tx {
    Open(Sender<Vec<u8>>),
    /// A write waits while `bound` chunks are queued: the writer feels a slow reader, as on a real pipe.
    Bounded(SyncSender<Vec<u8>>),
}

/// One end of the duplex.
pub struct End {
    pub reader: PipeReader,
    pub writer: PipeWriter,
}

/// Two connected ends: what one writes the other reads. Reading returns end of file once the other end's writer is
/// gone, which is how the hub's `serve` and the backend's reader thread learn that the peer left.
pub fn duplex() -> (End, End) {
    let (a_tx, a_rx) = channel();
    let (b_tx, b_rx) = channel();
    pair(Tx::Open(a_tx), a_rx, Tx::Open(b_tx), b_rx)
}

/// [`duplex`] whose writes wait once `bound` chunks are queued toward the reader. The host helper's link uses it: the
/// helper's writer then blocks behind a slow service and its outbox drops media (`swapdesk_host::outbox`), as it does
/// behind a full named pipe on Windows, instead of the queue growing without limit.
pub fn bounded_duplex(bound: usize) -> (End, End) {
    let (a_tx, a_rx) = sync_channel(bound);
    let (b_tx, b_rx) = sync_channel(bound);
    pair(Tx::Bounded(a_tx), a_rx, Tx::Bounded(b_tx), b_rx)
}

fn pair(a_tx: Tx, a_rx: Receiver<Vec<u8>>, b_tx: Tx, b_rx: Receiver<Vec<u8>>) -> (End, End) {
    (
        End {
            reader: PipeReader { rx: b_rx, left: Vec::new(), pos: 0, cancel: None },
            writer: PipeWriter { tx: a_tx },
        },
        End {
            reader: PipeReader { rx: a_rx, left: Vec::new(), pos: 0, cancel: None },
            writer: PipeWriter { tx: b_tx },
        },
    )
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.left.len() {
            loop {
                match self.rx.recv_timeout(CANCEL_POLL) {
                    Ok(chunk) => {
                        self.left = chunk;
                        self.pos = 0;
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if self.cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                            return Ok(0);
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(0),
                }
            }
        }
        let n = buf.len().min(self.left.len() - self.pos);
        buf[..n].copy_from_slice(&self.left[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let sent = match &self.tx {
            Tx::Open(tx) => tx.send(buf.to_vec()).is_ok(),
            Tx::Bounded(tx) => tx.send(buf.to_vec()).is_ok(),
        };
        if !sent {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_cross_in_both_directions_and_the_end_is_seen() {
        let (mut a, mut b) = duplex();
        a.writer.write_all(b"hello").unwrap();
        let mut got = [0u8; 5];
        b.reader.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
        b.writer.write_all(b"yo").unwrap();
        let mut two = [0u8; 2];
        a.reader.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"yo");
        drop(a.writer);
        assert_eq!(b.reader.read(&mut got).unwrap(), 0, "end of file when the peer's writer is gone");
        assert!(b.writer.write_all(b"x").is_ok(), "the other direction still works");
        drop(a.reader);
        assert!(b.writer.write_all(b"x").is_err(), "a closed peer is a broken pipe");
    }

    #[test]
    fn a_bounded_pipe_makes_the_writer_wait_for_the_reader() {
        let (mut a, mut b) = bounded_duplex(2);
        a.writer.write_all(b"1").unwrap();
        a.writer.write_all(b"2").unwrap();
        let writer = std::thread::spawn(move || {
            a.writer.write_all(b"3").unwrap();
            a
        });
        std::thread::sleep(Duration::from_millis(120));
        assert!(!writer.is_finished(), "the third write waits while two chunks are queued");
        let mut one = [0u8; 1];
        b.reader.read_exact(&mut one).unwrap();
        let a = writer.join().unwrap();
        let mut rest = [0u8; 2];
        b.reader.read_exact(&mut rest).unwrap();
        assert_eq!((&one, &rest), (b"1", b"23"));
        drop(a.writer);
        assert_eq!(b.reader.read(&mut one).unwrap(), 0);
    }

    #[test]
    fn a_blocked_read_ends_when_its_cancel_flag_is_set_though_the_peer_is_alive() {
        let (a, mut b) = duplex();
        let flag = Arc::new(AtomicBool::new(false));
        b.reader.cancel_with(Arc::clone(&flag));
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4];
            b.reader.read(&mut buf).unwrap()
        });
        std::thread::sleep(Duration::from_millis(120));
        assert!(!reader.is_finished(), "the read blocks while nothing arrives");
        flag.store(true, Ordering::SeqCst);
        assert_eq!(reader.join().unwrap(), 0, "end of file after the cancel");
        drop(a);
    }
}
