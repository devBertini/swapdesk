//! The helper's terminal proxy (stage 43): a service-side driver sends `PtyOpen`, `PtyInput`, `PtyResize` and
//! `PtyClose` to `run_helper` over the in-memory pipe, with the C7 echo PTY as the helper's spawner, and reads what the
//! helper answers.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use swapdesk_fakes::{FAKE_USER, FakeClock, FakePtySpawner, RecordingInputSink, SineSource, SyntheticScreen};
use swapdesk_ipc::{Bytes, FrameReader, FrameWriter, Handshake, IpcRole, MAX_TERM_CHUNK, Message, Text};
use swapdesk_platform::{ErrorKind, MonoMicros, PlatformError, Pty, PtyRequest, PtySpawner, Result as PResult};
use swapdesk_proto::ErrorCode;

use crate::helper::{HelperDeps, run_helper};
use crate::helper_tests::Passthrough;
use crate::testpipe::{PipeWriter, duplex};

const WAIT: Duration = Duration::from_secs(10);
const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

type Deps = HelperDeps<SyntheticScreen, Passthrough, RecordingInputSink, SineSource>;

fn deps() -> Deps {
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let audio_clock = Arc::clone(&clock);
    HelperDeps::new(
        |_| Ok(SyntheticScreen::new(64, 48, 100)),
        |_frame, _config, _backend| Ok(Passthrough { n: 0, size: None }),
        || Ok(RecordingInputSink::new()),
        move || Ok(SineSource::new(audio_clock.clone()).realtime()),
        clock,
    )
}

/// The service's side of a pipe to a helper running on its own thread.
struct Rig {
    writer: FrameWriter<PipeWriter>,
    inbox: Receiver<Message>,
    /// Messages a `next` passed over, in order, for the later ones: the shell's first output can come before
    /// `PtyOpenReply` (the helper starts reading the shell before it answers), and `open` must not drop it.
    held: RefCell<VecDeque<Message>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Rig {
    fn new(role: IpcRole, deps: Deps) -> Self {
        let (helper_end, service_end) = duplex();
        let thread = std::thread::spawn(move || {
            let _ = run_helper(helper_end.reader, helper_end.writer, deps, role, 4242);
        });
        let mut reader = FrameReader::new(service_end.reader);
        let mut writer = FrameWriter::new(service_end.writer);
        Handshake::server(&mut reader, &mut writer, 3).expect("server handshake");
        let (tx, inbox) = channel();
        std::thread::spawn(move || {
            while let Ok(Some(m)) = reader.read_message() {
                if tx.send(m).is_err() {
                    break;
                }
            }
        });
        Self { writer, inbox, held: RefCell::default(), thread: Some(thread) }
    }

    fn send(&mut self, msg: Message) {
        self.writer.write_message(&msg).expect("the pipe is open");
    }

    /// The next message `f` picks, within `WAIT`. The ones it passes over (the display list, say) are kept, in order,
    /// for the next call.
    fn next<T>(&self, what: &str, mut f: impl FnMut(&Message) -> Option<T>) -> T {
        let mut held = self.held.borrow_mut();
        for at in 0..held.len() {
            if let Some(v) = f(&held[at]) {
                held.remove(at);
                return v;
            }
        }
        let end = std::time::Instant::now() + WAIT;
        loop {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let m = self.inbox.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let Some(v) = f(&m) {
                return v;
            }
            held.push_back(m);
        }
    }

    fn open(&mut self, id: u32, session_user: &str) -> (ErrorCode, String) {
        self.send(Message::PtyOpen { id, cols: 80, rows: 24, session_user: Text(session_user.into()) });
        self.next("PtyOpenReply", |m| match m {
            Message::PtyOpenReply { id: i, code, account } if *i == id => Some((*code, account.0.clone())),
            _ => None,
        })
    }

    fn output(&self, id: u32, want: usize) -> Vec<u8> {
        let mut got = Vec::new();
        while got.len() < want {
            got.extend(self.next("PtyOutput", |m| match m {
                Message::PtyOutput { id: i, bytes } if *i == id => Some(bytes.0.clone()),
                _ => None,
            }));
        }
        got
    }

    fn finish(mut self) {
        self.send(Message::Shutdown);
        if let Some(t) = self.thread.take() {
            t.join().expect("helper thread");
        }
    }
}

/// A helper that runs shells with the echo PTY, recording the console users it was asked for.
fn echo(spawner: &Arc<FakePtySpawner>, asked: &Arc<Mutex<Vec<Option<String>>>>) -> Deps {
    let (spawner, asked) = (Arc::clone(spawner), Arc::clone(asked));
    deps().with_pty(move |user| {
        asked.lock().unwrap().push(user);
        Arc::clone(&spawner) as Arc<dyn PtySpawner>
    })
}

#[test]
fn the_helper_runs_the_shell_it_is_asked_for_and_relays_both_ways() {
    let (spawner, asked) = (Arc::new(FakePtySpawner::new()), Arc::new(Mutex::new(Vec::new())));
    let mut rig = Rig::new(IpcRole::HelperUser, echo(&spawner, &asked));
    assert_eq!(rig.open(1, SID), (ErrorCode::NoError, FAKE_USER.to_string()));
    assert_eq!(
        *asked.lock().unwrap(),
        vec![Some(SID.to_string())],
        "the spawner is built for the user the service named"
    );
    assert_eq!(spawner.requests(), vec![PtyRequest { cols: 80, rows: 24, admin: false }]);

    rig.send(Message::PtyInput { id: 1, bytes: Bytes(b"dir\r".to_vec()) });
    assert_eq!(rig.output(1, 4), b"dir\r");
    rig.send(Message::PtyResize { id: 1, cols: 120, rows: 40 });
    let end = std::time::Instant::now() + WAIT;
    while spawner.resizes() != vec![(120, 40)] {
        assert!(std::time::Instant::now() < end, "the resize reaches the shell");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Closing sends no `PtyExited` (the service asked for it).
    rig.send(Message::PtyClose { id: 1 });
    assert!(
        rig.inbox.recv_timeout(Duration::from_millis(400)).map_or(true, |m| !matches!(m, Message::PtyExited { .. }))
    );
    rig.finish();
}

#[test]
fn an_empty_session_user_means_not_told() {
    let (spawner, asked) = (Arc::new(FakePtySpawner::new()), Arc::new(Mutex::new(Vec::new())));
    let mut rig = Rig::new(IpcRole::HelperUser, echo(&spawner, &asked));
    assert_eq!(rig.open(1, "").0, ErrorCode::NoError);
    assert_eq!(*asked.lock().unwrap(), vec![None]);
    rig.finish();
}

#[test]
fn a_system_helper_never_runs_a_users_shell() {
    let (spawner, asked) = (Arc::new(FakePtySpawner::new()), Arc::new(Mutex::new(Vec::new())));
    let mut rig = Rig::new(IpcRole::HelperSystem, echo(&spawner, &asked));
    assert_eq!(rig.open(1, SID), (ErrorCode::NotPermitted, String::new()));
    assert!(spawner.requests().is_empty() && asked.lock().unwrap().is_empty(), "nothing was started or even built");
    rig.finish();
}

#[test]
fn a_helper_without_terminal_support_says_unavailable() {
    let mut rig = Rig::new(IpcRole::HelperUser, deps());
    assert_eq!(rig.open(1, SID), (ErrorCode::Unavailable, String::new()));
    rig.finish();
}

/// A spawner that refuses as the token guard does when the token is not the console user's.
struct Refusing;

impl PtySpawner for Refusing {
    fn spawn(&self, _request: &PtyRequest) -> PResult<Box<dyn Pty>> {
        Err(PlatformError {
            kind: ErrorKind::Unsupported,
            op: "ConPtySpawner.spawn",
            os_code: None,
            detail: "the token is not the logged-in user's".into(),
        })
    }
}

#[test]
fn the_helpers_own_guard_refusal_goes_back_as_its_code() {
    let mut rig = Rig::new(IpcRole::HelperUser, deps().with_pty(|_| Arc::new(Refusing)));
    assert_eq!(rig.open(1, SID), (ErrorCode::NotPermitted, String::new()));
    // The helper goes on: the next open (here refused again) is answered too.
    assert_eq!(rig.open(2, SID).0, ErrorCode::NotPermitted);
    rig.finish();
}

#[test]
fn at_most_four_shells_and_ids_are_unique() {
    let (spawner, asked) = (Arc::new(FakePtySpawner::new()), Arc::new(Mutex::new(Vec::new())));
    let mut rig = Rig::new(IpcRole::HelperUser, echo(&spawner, &asked));
    for id in 1..=4 {
        assert_eq!(rig.open(id, SID).0, ErrorCode::NoError);
    }
    assert_eq!(rig.open(5, SID).0, ErrorCode::Busy);
    assert_eq!(rig.open(2, SID).0, ErrorCode::ProtocolViolation);
    rig.finish();
}

/// A shell that prints "bye" and ends with code 3 by itself.
struct Script;

struct ScriptPty;

impl PtySpawner for Script {
    fn spawn(&self, _request: &PtyRequest) -> PResult<Box<dyn Pty>> {
        Ok(Box::new(ScriptPty))
    }
}

impl Pty for ScriptPty {
    fn account(&self) -> &str {
        "scripted"
    }
    fn reader(&mut self) -> PResult<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(b"bye\r\n".to_vec())))
    }
    fn writer(&mut self) -> PResult<Box<dyn Write + Send>> {
        Ok(Box::new(std::io::sink()))
    }
    fn resize(&mut self, _: u16, _: u16) -> PResult<()> {
        Ok(())
    }
    fn try_wait(&mut self) -> PResult<Option<i32>> {
        Ok(Some(3))
    }
    fn kill(&mut self) -> PResult<()> {
        Ok(())
    }
}

#[test]
fn a_shell_that_ends_is_reported_after_its_last_output() {
    let mut rig = Rig::new(IpcRole::HelperUser, deps().with_pty(|_| Arc::new(Script)));
    assert_eq!(rig.open(9, SID), (ErrorCode::NoError, "scripted".to_string()));
    let mut order = Vec::new();
    let code = rig.next("PtyExited", |m| match m {
        Message::PtyOutput { id: 9, bytes } => {
            order.push(format!("out {}", bytes.0.len()));
            None
        }
        Message::PtyExited { id: 9, code } => Some(*code),
        _ => None,
    });
    assert_eq!(code, Some(3), "the shell's exit code");
    assert_eq!(order, vec!["out 5".to_string()], "the output came first, then the end");
    rig.finish();
}

#[test]
fn a_flood_of_output_arrives_whole_in_chunks_within_the_limit() {
    // A shell that writes more than one chunk at once: every PtyOutput stays within the limit.
    struct Flood;
    struct FloodPty;
    impl PtySpawner for Flood {
        fn spawn(&self, _request: &PtyRequest) -> PResult<Box<dyn Pty>> {
            Ok(Box::new(FloodPty))
        }
    }
    impl Pty for FloodPty {
        fn account(&self) -> &str {
            "flood"
        }
        fn reader(&mut self) -> PResult<Box<dyn Read + Send>> {
            Ok(Box::new(Cursor::new(vec![b'x'; 3 * MAX_TERM_CHUNK + 17])))
        }
        fn writer(&mut self) -> PResult<Box<dyn Write + Send>> {
            Ok(Box::new(std::io::sink()))
        }
        fn resize(&mut self, _: u16, _: u16) -> PResult<()> {
            Ok(())
        }
        fn try_wait(&mut self) -> PResult<Option<i32>> {
            Ok(None)
        }
        fn kill(&mut self) -> PResult<()> {
            Ok(())
        }
    }
    let mut rig = Rig::new(IpcRole::HelperUser, deps().with_pty(|_| Arc::new(Flood)));
    assert_eq!(rig.open(1, SID).0, ErrorCode::NoError);
    let mut total = 0;
    while total < 3 * MAX_TERM_CHUNK + 17 {
        total += rig.next("PtyOutput", |m| match m {
            Message::PtyOutput { bytes, .. } => Some(bytes.0.len()),
            _ => None,
        });
    }
    assert_eq!(total, 3 * MAX_TERM_CHUNK + 17, "all of it arrived, in chunks the framing accepted");
    rig.finish();
}
