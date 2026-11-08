use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use swapdesk_platform::{ErrorKind, OsGrant, PlatformError, Pty, PtyRequest, PtySpawner, Result};

use crate::lock;

/// Account of a PTY spawned without `admin`.
pub const FAKE_USER: &str = "fakeuser";
const FAKE_ADMIN: &str = "SYSTEM";
const KILLED_EXIT: i32 = 137;

fn err(kind: ErrorKind, op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.to_string() }
}

#[derive(Default)]
struct SpawnerState {
    requests: Vec<PtyRequest>,
    resizes: Vec<(u16, u16)>,
}

/// Spawns echo PTYs: what the writer sends comes back on the reader. A spawner that is not `privileged` refuses
/// `admin` requests with `PermissionDenied(Administrator)`, as a real one that is not SYSTEM or root does.
pub struct FakePtySpawner {
    privileged: bool,
    state: Arc<Mutex<SpawnerState>>,
}

impl FakePtySpawner {
    pub fn new() -> Self {
        Self { privileged: false, state: Arc::default() }
    }

    /// Plays the service or daemon: admin requests spawn as "SYSTEM".
    pub fn privileged() -> Self {
        Self { privileged: true, state: Arc::default() }
    }

    pub fn requests(&self) -> Vec<PtyRequest> {
        lock(&self.state).requests.clone()
    }

    /// Every `resize` of every PTY it spawned.
    pub fn resizes(&self) -> Vec<(u16, u16)> {
        lock(&self.state).resizes.clone()
    }
}

impl Default for FakePtySpawner {
    fn default() -> Self {
        Self::new()
    }
}

impl PtySpawner for FakePtySpawner {
    fn spawn(&self, request: &PtyRequest) -> Result<Box<dyn Pty>> {
        if request.admin && !self.privileged {
            return Err(err(
                ErrorKind::PermissionDenied(OsGrant::Administrator),
                "FakePtySpawner.spawn",
                "admin refused",
            ));
        }
        if request.cols == 0 || request.rows == 0 {
            return Err(err(ErrorKind::InvalidInput, "FakePtySpawner.spawn", "empty size"));
        }
        lock(&self.state).requests.push(*request);
        let (tx, rx) = channel();
        Ok(Box::new(FakePty {
            account: if request.admin { FAKE_ADMIN } else { FAKE_USER },
            tx: Some(tx),
            rx: Some(rx),
            killed: Arc::default(),
            state: Arc::clone(&self.state),
        }))
    }
}

struct FakePty {
    account: &'static str,
    tx: Option<Sender<Vec<u8>>>,
    rx: Option<Receiver<Vec<u8>>>,
    killed: Arc<AtomicBool>,
    state: Arc<Mutex<SpawnerState>>,
}

struct EchoWriter(Sender<Vec<u8>>, Arc<AtomicBool>);

impl Write for EchoWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.1.load(Ordering::SeqCst) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        self.0.send(buf.to_vec()).map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct EchoReader {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    killed: Arc<AtomicBool>,
}

impl Read for EchoReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            match self.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => self.pending = chunk,
                Err(RecvTimeoutError::Timeout) if self.killed.load(Ordering::SeqCst) => return Ok(0),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl Pty for FakePty {
    fn account(&self) -> &str {
        self.account
    }

    fn reader(&mut self) -> Result<Box<dyn Read + Send>> {
        let rx = self.rx.take().ok_or_else(|| err(ErrorKind::Closed, "FakePty.reader", "already taken"))?;
        Ok(Box::new(EchoReader { rx, pending: Vec::new(), killed: Arc::clone(&self.killed) }))
    }

    fn writer(&mut self) -> Result<Box<dyn Write + Send>> {
        let tx = self.tx.clone().ok_or_else(|| err(ErrorKind::Closed, "FakePty.writer", "killed"))?;
        Ok(Box::new(EchoWriter(tx, Arc::clone(&self.killed))))
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if self.killed.load(Ordering::SeqCst) {
            return Err(err(ErrorKind::Closed, "FakePty.resize", "killed"));
        }
        lock(&self.state).resizes.push((cols, rows));
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        Ok(self.killed.load(Ordering::SeqCst).then_some(KILLED_EXIT))
    }

    fn kill(&mut self) -> Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        self.tx = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(admin: bool) -> PtyRequest {
        PtyRequest { cols: 120, rows: 30, admin }
    }

    #[test]
    fn echoes_resizes_and_ends_on_kill() {
        let spawner = FakePtySpawner::new();
        let mut pty = spawner.spawn(&req(false)).unwrap();
        assert_eq!(pty.account(), FAKE_USER);
        let mut r = pty.reader().unwrap();
        let mut w = pty.writer().unwrap();
        w.write_all(b"dir\r\n").unwrap();
        let mut buf = [0u8; 16];
        let mut got = Vec::new();
        while got.len() < 5 {
            let n = r.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"dir\r\n");
        assert!(pty.reader().is_err());
        pty.resize(100, 40).unwrap();
        assert_eq!(spawner.resizes(), vec![(100, 40)]);
        assert_eq!(pty.try_wait().unwrap(), None);
        let reader_thread = std::thread::spawn(move || {
            let mut b = [0u8; 8];
            r.read(&mut b).unwrap()
        });
        pty.kill().unwrap();
        assert_eq!(reader_thread.join().unwrap(), 0);
        assert_eq!(pty.try_wait().unwrap(), Some(137));
        assert!(w.write_all(b"x").is_err());
        assert!(pty.resize(1, 1).is_err());
        assert_eq!(spawner.requests(), vec![req(false)]);
    }

    #[test]
    fn admin_needs_a_privileged_spawner() {
        let e = FakePtySpawner::new().spawn(&req(true)).err().unwrap();
        assert_eq!(e.kind, ErrorKind::PermissionDenied(OsGrant::Administrator));
        let pty = FakePtySpawner::privileged().spawn(&req(true)).unwrap();
        assert_eq!(pty.account(), "SYSTEM");
        let e = FakePtySpawner::new().spawn(&PtyRequest { cols: 0, rows: 5, admin: false }).err().unwrap();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
    }
}
