//! In-memory duplex pipe for tests: two mpsc-backed `Read` / `Write` ends.

use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

/// How long a read waits before it fails, so a broken test cannot hang.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

pub struct PipeReader {
    rx: Receiver<Vec<u8>>,
    timeout: Option<Duration>,
    left: Vec<u8>,
    pos: usize,
}

pub struct PipeWriter {
    tx: Sender<Vec<u8>>,
}

/// One side of a duplex pipe: what it reads is what the other side wrote.
pub struct End {
    pub reader: PipeReader,
    pub writer: PipeWriter,
}

fn half(timeout: Option<Duration>) -> (PipeWriter, PipeReader) {
    let (tx, rx) = channel();
    (PipeWriter { tx }, PipeReader { rx, timeout, left: Vec::new(), pos: 0 })
}

pub fn duplex() -> (End, End) {
    duplex_with(Some(READ_TIMEOUT))
}

/// A pipe whose reads wait as long as it takes (the service's reader thread sits on one between messages).
#[cfg(test)]
pub fn duplex_blocking() -> (End, End) {
    duplex_with(None)
}

fn duplex_with(timeout: Option<Duration>) -> (End, End) {
    let (a_to_b_w, a_to_b_r) = half(timeout);
    let (b_to_a_w, b_to_a_r) = half(timeout);
    (End { reader: b_to_a_r, writer: a_to_b_w }, End { reader: a_to_b_r, writer: b_to_a_w })
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.left.len() {
            let chunk = match self.timeout {
                None => match self.rx.recv() {
                    Ok(chunk) => chunk,
                    Err(_) => return Ok(0),
                },
                Some(t) => match self.rx.recv_timeout(t) {
                    Ok(chunk) => chunk,
                    Err(RecvTimeoutError::Disconnected) => return Ok(0),
                    Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::TimedOut.into()),
                },
            };
            self.left = chunk;
            self.pos = 0;
        }
        let n = buf.len().min(self.left.len() - self.pos);
        buf[..n].copy_from_slice(&self.left[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx.send(buf.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
