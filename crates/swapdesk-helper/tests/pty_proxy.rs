//! The helper's terminal proxy with the real Windows pieces (stage 43; F-TERM-01): `run_helper` with the ConPTY spawner
//! of `swapdesk-sys-win`, driven over an in-memory pipe by the service's side of C2.4. The shell it starts runs as the
//! user of this process (the helper's user), its output makes the round trip, and the helper's own token guard refuses
//! a console user that is not that user. The SYSTEM administrator shell is not here: it needs the installed service.
#![cfg(windows)]

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use swapdesk_audio_win::WasapiLoopbackSource;
use swapdesk_capture_win::{DxgiScreen, QpcClock};
use swapdesk_codec_win::{H264Encoder, H264EncoderOptions};
use swapdesk_host::helper::{HelperDeps, run_helper};
use swapdesk_ipc::{Bytes, FrameReader, FrameWriter, Handshake, IpcRole, Message, Text};
use swapdesk_platform::PtySpawner;
use swapdesk_sys_win::current_user_sid;
use swapdesk_sys_win::pty::{ConPtySpawner, Shell};

const WAIT: Duration = Duration::from_secs(30);

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

struct PipeWriter(Sender<Vec<u8>>);

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv_timeout(WAIT) {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
                Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::TimedOut.into()),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.send(data.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn pipe() -> (PipeReader, PipeWriter) {
    let (tx, rx) = channel();
    (PipeReader { rx, buf: Vec::new(), pos: 0 }, PipeWriter(tx))
}

struct Service {
    writer: FrameWriter<PipeWriter>,
    inbox: Receiver<Message>,
    helper: Option<std::thread::JoinHandle<()>>,
}

impl Service {
    /// A user helper whose terminals run `cmd.exe` through the real ConPTY spawner.
    fn start() -> Service {
        let ((h_in, svc_out), (svc_in, h_out)) = (pipe(), pipe());
        let deps = HelperDeps::new(
            |_| Ok(DxgiScreen::new()),
            |_frame, _config, _backend| Ok(H264Encoder::new(H264EncoderOptions::default())),
            || Ok(swapdesk_input_win::open_sink()),
            || Ok(WasapiLoopbackSource::new()),
            Arc::new(QpcClock),
        )
        .with_pty(|session_user| {
            Arc::new(ConPtySpawner::current_user(Shell::command("cmd.exe /k"), session_user)) as Arc<dyn PtySpawner>
        });
        let helper = std::thread::spawn(move || {
            let _ = run_helper(h_in, h_out, deps, IpcRole::HelperUser, std::process::id());
        });
        let mut reader = FrameReader::new(svc_in);
        let mut writer = FrameWriter::new(svc_out);
        Handshake::server(&mut reader, &mut writer, 1).expect("the handshake");
        let (tx, inbox) = channel();
        std::thread::spawn(move || {
            while let Ok(Some(m)) = reader.read_message() {
                if tx.send(m).is_err() {
                    break;
                }
            }
        });
        Service { writer, inbox, helper: Some(helper) }
    }

    fn send(&mut self, m: Message) {
        self.writer.write_message(&m).expect("the pipe is open");
    }

    fn next<T>(&self, what: &str, mut f: impl FnMut(&Message) -> Option<T>) -> T {
        let end = Instant::now() + WAIT;
        loop {
            let m = self
                .inbox
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let Some(v) = f(&m) {
                return v;
            }
        }
    }

    /// The helper's answer: the refusal code by name (`NoError` when the shell runs) and the account.
    fn open(&mut self, id: u32, session_user: &str) -> (String, String) {
        self.send(Message::PtyOpen { id, cols: 200, rows: 30, session_user: Text(session_user.into()) });
        self.next("PtyOpenReply", |m| match m {
            Message::PtyOpenReply { id: i, code, account } if *i == id => {
                Some((format!("{code:?}"), account.0.clone()))
            }
            _ => None,
        })
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.writer.write_message(&Message::Shutdown);
        if let Some(t) = self.helper.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn the_helper_runs_the_shell_as_its_own_user_and_the_output_makes_the_round_trip() {
    let me = current_user_sid().expect("this process's user");
    let mut svc = Service::start();
    let (code, account) = svc.open(1, &me);
    assert_eq!(code, "NoError", "refused");
    assert!(
        account.eq_ignore_ascii_case(&std::env::var("USERNAME").unwrap_or_default()),
        "the shell's account {account}"
    );
    // The system's own whoami (a GNU one earlier in PATH would not know `/user`), then the shell exits with 3.
    svc.send(Message::PtyInput {
        id: 1,
        bytes: Bytes(b"\"%SystemRoot%\\System32\\whoami.exe\" /user\r\nexit 3\r\n".to_vec()),
    });
    let mut text = Vec::new();
    let end = Instant::now() + WAIT;
    let exit = loop {
        let left = end.saturating_duration_since(Instant::now());
        match svc.inbox.recv_timeout(left).expect("the shell ends in time") {
            Message::PtyOutput { id: 1, bytes } => text.extend(bytes.0),
            Message::PtyExited { id: 1, code } => break code,
            _ => {}
        }
    };
    let text = String::from_utf8_lossy(&text);
    assert!(text.contains(&me), "whoami printed the user's SID {me}: {text}");
    assert!(!text.contains("S-1-5-18"), "never SYSTEM: {text}");
    assert_eq!(exit, Some(3), "the exit code comes after the output");
}

#[test]
fn the_helpers_own_token_guard_refuses_a_console_user_that_is_not_its_user() {
    let mut svc = Service::start();
    // The service says somebody else is logged in: this process's token is not theirs, and nothing is started.
    let (code, account) = svc.open(1, "S-1-5-21-1111111111-2222222222-3333333333-1002");
    assert_eq!((code.as_str(), account.as_str()), ("NotPermitted", ""));
    // An administrator terminal does not exist on this path at all: the message has no such field.
}
