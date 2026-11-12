//! The Mac's host side of the terminal (F-TERM-01, F-XTERM-01) with the real pieces: `host::host_helper`, the loop the
//! app runs for hosting, driven over an in-memory pipe by the service's side of C2.4 ("Terminal"), the way the service
//! drives it once a viewer's `TermOpen` has passed the session core. The shell it starts is a login shell of this
//! process's user on a real pseudo-terminal; its output makes the round trip, a resize reaches it and a close ends it.
//!
//! Nothing here starts capture, input or a quick action: the helper builds those only on their own messages, and the
//! quick action runner of the app is the real one (it would lock or sleep this Mac), so no `RunAction` is sent. The
//! quick actions are proven where a recorder stands in for the system (`swapdesk_sys_mac::actions`, `swapdesk-term`).
//! Run with `scripts/mac/remote.sh test -p swapdesk-app-mac`.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use swapdesk_ipc::{Bytes, FrameReader, FrameWriter, Handshake, Message, Text};
use swapdesk_platform::{Clock, MonoMicros};
use swapdesk_proto::ErrorCode;

use crate::host::host_helper;
use crate::pipe::{PipeWriter, duplex};

const WAIT: Duration = Duration::from_secs(30);

struct TestClock(Instant);

impl Clock for TestClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX))
    }
}

/// The service's end of the helper link.
struct Service {
    writer: FrameWriter<PipeWriter>,
    inbox: Receiver<Message>,
    helper: Option<std::thread::JoinHandle<()>>,
}

impl Service {
    fn start() -> Service {
        let (service_end, helper_end) = duplex();
        let run = host_helper(Arc::new(TestClock(Instant::now())));
        let helper = std::thread::spawn(move || {
            let _ = run(helper_end.reader, helper_end.writer);
        });
        let mut reader = FrameReader::new(service_end.reader);
        let mut writer = FrameWriter::new(service_end.writer);
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

    /// The helper's answer to a `PtyOpen`: the code and the account.
    fn open(&mut self, id: u32, cols: u16, rows: u16, session_user: &str) -> (ErrorCode, String) {
        self.send(Message::PtyOpen { id, cols, rows, session_user: Text(session_user.into()) });
        let end = Instant::now() + WAIT;
        loop {
            match self.inbox.recv_timeout(end.saturating_duration_since(Instant::now())).expect("a PtyOpenReply") {
                Message::PtyOpenReply { id: i, code, account } if i == id => return (code, account.0),
                _ => {}
            }
        }
    }

    /// Collects the terminal's output into `text` until `needle` shows up or the shell ends (`Some(code)`).
    fn read_until(&self, id: u32, text: &mut String, needle: &str) -> Option<Option<i32>> {
        let end = Instant::now() + WAIT;
        while !text.contains(needle) {
            let left = end.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(Message::PtyOutput { id: i, bytes }) if i == id => text.push_str(&String::from_utf8_lossy(&bytes.0)),
                Ok(Message::PtyExited { id: i, code }) if i == id => return Some(code),
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for {needle:?}; got {text:?}"),
            }
        }
        None
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

/// The name of the user running this test, read from the system (`id -un`), not from `$USER`.
fn user_name() -> String {
    let out = std::process::Command::new("/usr/bin/id").arg("-un").output().expect("id runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_host_helper_opens_a_login_shell_as_the_user_with_the_asked_size_and_relays_both_ways() {
    let me = user_name();
    let mut svc = Service::start();
    let (code, account) = svc.open(1, 100, 30, "");
    assert_eq!(code, ErrorCode::NoError, "refused");
    assert_eq!(account, me, "the shell's account is the user running the app");
    // The echoed command line holds `$(whoami)`, never the expanded text, so a match is the shell's own output.
    svc.send(Message::PtyInput { id: 1, bytes: Bytes(b"echo who=$(whoami) size=$(stty size) term=$TERM\n".to_vec()) });
    let mut text = String::new();
    let want = format!("who={me} size=30 100 term=xterm-256color");
    assert_eq!(svc.read_until(1, &mut text, &want), None, "the shell stayed up");
    assert!(!text.contains("who=root"), "never root: {text}");
}

#[test]
fn a_resize_reaches_the_shell_as_sigwinch_and_a_close_ends_it() {
    let mut svc = Service::start();
    assert_eq!(svc.open(7, 80, 24, "").0, ErrorCode::NoError);
    // The trap runs when SIGWINCH arrives, and `stty size` then reads the new size: both prove the signal and the size.
    svc.send(Message::PtyInput {
        id: 7,
        bytes: Bytes(b"trap 'echo winch=$(stty size)' WINCH; echo armed=$((40+2))\n".to_vec()),
    });
    let mut text = String::new();
    assert_eq!(svc.read_until(7, &mut text, "armed=42"), None);
    svc.send(Message::PtyResize { id: 7, cols: 132, rows: 43 });
    assert_eq!(svc.read_until(7, &mut text, "winch=43 132"), None, "{text}");
    svc.send(Message::PtyClose { id: 7 });
    // The helper answers a close with nothing further, and a later terminal opens as usual.
    assert_eq!(svc.open(8, 80, 24, "").0, ErrorCode::NoError, "a terminal opens after the close");
}

#[test]
fn the_shells_exit_code_arrives_after_its_output() {
    let mut svc = Service::start();
    assert_eq!(svc.open(2, 80, 24, "").0, ErrorCode::NoError);
    svc.send(Message::PtyInput { id: 2, bytes: Bytes(b"echo last=$((1+2)); exit 5\n".to_vec()) });
    let mut text = String::new();
    assert_eq!(svc.read_until(2, &mut text, "last=3"), None);
    assert_eq!(svc.read_until(2, &mut text, "never-appears"), Some(Some(5)));
}
