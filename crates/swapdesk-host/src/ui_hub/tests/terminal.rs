//! Terminals through the UI pipe (stage 43; F-TERM-01, F-TERM-02, F-SESS-04): two hubs on two cores, a fake UI each. The
//! viewer's UI opens a terminal with `TermOpen`, types with `TermInput` messages and reads `TermOutput` and
//! `TermExited`; the host's shell runs in a user helper behind the in-memory pipe (the C7 echo PTY, or a scripted one).

use std::io::{Cursor, Read, Write};

use swapdesk_fakes::{FAKE_USER, FakePtySpawner};
use swapdesk_ipc::{Bytes, TermOutcome};
use swapdesk_platform::{Pty, PtyRequest, PtySpawner, Result as PResult};

use super::*;
use crate::service::TerminalEnv;

const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
const RESIZE: u16 = 33;
const GRANTS: PermissionSet = ALL.union(PermissionSet::ADMIN_TERMINAL);

struct Env {
    user: Option<String>,
    admin: Option<Arc<dyn PtySpawner>>,
}

impl TerminalEnv for Env {
    fn console_user(&self) -> Option<String> {
        self.user.clone()
    }

    fn admin_spawner(&self) -> Option<Arc<dyn PtySpawner>> {
        self.admin.clone()
    }
}

/// A shell that prints "bye" and ends with code 3.
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

struct Rig {
    host: Machine,
    viewer: Machine,
    host_ui: Client,
    viewer_ui: Client,
    spawner: Arc<FakePtySpawner>,
}

impl Rig {
    /// The host stores `stored` for the viewer and its card accepts all of it; `user` is logged in at the host.
    fn new(stored: PermissionSet, user: Option<&str>, shell: Option<Arc<dyn PtySpawner>>) -> Self {
        let mut host = Machine::new("host-pc");
        let viewer = Machine::new("viewer-pc");
        host.pin(&viewer, stored);
        viewer.pin(&host, PermissionSet::CONTROL);
        let spawner = Arc::new(FakePtySpawner::new());
        let shell = shell.unwrap_or_else(|| Arc::clone(&spawner) as Arc<dyn PtySpawner>);
        host.attach_helper_with(IpcRole::HelperUser, move |deps| deps.with_pty(move |_| Arc::clone(&shell)));
        let admin: Arc<dyn PtySpawner> = Arc::new(FakePtySpawner::privileged());
        assert!(host.core.set_terminal_env(Arc::new(Env { user: user.map(str::to_owned), admin: Some(admin) })));
        viewer.sees(&host, false);
        host.sees(&viewer, false);
        host.hub.set_console_session(Some(1));
        viewer.hub.set_console_session(Some(1));
        let host_ui = host.client(info(false));
        let viewer_ui = viewer.client(info(false));
        Self { host, viewer, host_ui, viewer_ui, spawner }
    }

    /// `ui` (a connection of the viewer machine) connects to the host; the host's user accepts `stored`.
    fn open_session(&mut self, ui: &mut Client, stored: PermissionSet) -> u64 {
        let req = ui.request(UiOp::Connect { device_id: self.host.hex(), intent: Intent::Control, unattended: false });
        let incoming = self.host_ui.notify("IncomingRequest", |n| match n {
            Notify::IncomingRequest(r) => Some(r.clone()),
            _ => None,
        });
        assert!(
            self.host_ui
                .call(UiOp::AnswerIncoming {
                    request_id: incoming.request_id,
                    decision: Decision::Accept,
                    grants: stored.bits()
                })
                .is_ok()
        );
        let ReplyBody::SessionId(id) = ui.reply(req, CONNECT_OP).expect("the session opens") else { panic!() };
        id
    }

    fn finish(self) {
        let Rig { host, viewer, host_ui, viewer_ui, .. } = self;
        drop((host_ui, viewer_ui));
        viewer.finish();
        host.finish();
    }
}

fn open(ui: &mut Client, session_id: u64, admin: bool) -> (u32, TermOutcome, String) {
    match ui.call(UiOp::TermOpen { session_id, cols: 80, rows: 24, admin }).expect("the open answers") {
        ReplyBody::TermOpened { term_id, outcome, account } => (term_id, outcome, account.0),
        other => panic!("{other:?}"),
    }
}

fn output(ui: &mut Client, term_id: u32, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        got.extend(ui.take("terminal output", |m| match m {
            Ipc::TermOutput { term_id: id, bytes } if *id == term_id => Some(bytes.0.clone()),
            _ => None,
        }));
    }
    got
}

#[test]
fn a_terminal_opens_through_the_ui_and_bytes_flow_both_ways_then_it_closes() {
    let mut r = Rig::new(GRANTS, Some(SID), None);
    let mut ui = std::mem::replace(&mut r.viewer_ui, r.viewer.client(info(false)));
    let session = r.open_session(&mut ui, ALL.union(PermissionSet::TERMINAL));
    let (id, outcome, account) = open(&mut ui, session, false);
    assert_eq!((outcome, account.as_str()), (TermOutcome::Opened, FAKE_USER));
    assert_ne!(id, 0);

    ui.send(&Ipc::TermInput { term_id: id, bytes: Bytes(b"dir\r".to_vec()) });
    assert_eq!(output(&mut ui, id, 4), b"dir\r");

    let req = ui.request(UiOp::TermResize { term_id: id, cols: 120, rows: 40 });
    assert!(matches!(ui.reply(req, RESIZE), Ok(ReplyBody::Done)));
    wait_for("the shell's resize", || (r.spawner.resizes() == vec![(120, 40)]).then_some(()));
    assert_eq!(ui.call(UiOp::TermResize { term_id: id, cols: 0, rows: 40 }).unwrap_err(), ApiCode::Invalid);

    // The host's audit has the terminal (F-SESS-04), as the user's, not the administrator's.
    wait_for("the audit entry", || {
        r.host.audit.entries().iter().any(|e| e.event == AuditEvent::TerminalOpened { admin: false }).then_some(())
    });

    assert!(matches!(ui.call(UiOp::TermClose { term_id: id }), Ok(ReplyBody::Done)));
    assert_eq!(ui.call(UiOp::TermClose { term_id: id }).unwrap_err(), ApiCode::NotFound, "gone");
    assert!(
        ui.nothing_within(Duration::from_millis(300), |m| matches!(m, Ipc::UiNotify(Notify::TermExited { .. }))),
        "no TermExited for a close"
    );
    drop(ui);
    r.finish();
}

#[test]
fn a_terminal_belongs_to_the_connection_that_opened_it() {
    let mut r = Rig::new(GRANTS, Some(SID), None);
    let mut ui = std::mem::replace(&mut r.viewer_ui, r.viewer.client(info(false)));
    let session = r.open_session(&mut ui, ALL.union(PermissionSet::TERMINAL));
    let (id, outcome, _) = open(&mut ui, session, false);
    assert_eq!(outcome, TermOutcome::Opened);
    let mut other = r.viewer.client(info(false));
    // A number is not a capability on another connection: nothing resizes, closes or types into it.
    assert_eq!(other.call(UiOp::TermResize { term_id: id, cols: 90, rows: 30 }).unwrap_err(), ApiCode::NotFound);
    assert_eq!(other.call(UiOp::TermClose { term_id: id }).unwrap_err(), ApiCode::NotFound);
    assert_eq!(
        other.call(UiOp::TermOpen { session_id: session, cols: 80, rows: 24, admin: false }).unwrap_err(),
        ApiCode::NotFound,
        "nor does it own the session"
    );
    other.send(&Ipc::TermInput { term_id: id, bytes: Bytes(b"stolen".to_vec()) });
    assert!(
        ui.nothing_within(Duration::from_millis(400), |m| matches!(m, Ipc::TermOutput { .. })),
        "nothing reached the shell"
    );
    drop((ui, other));
    r.finish();
}

#[test]
fn refusals_are_outcomes_and_a_request_that_cannot_be_placed_is_an_error() {
    // Nobody logged in at the host.
    let mut r = Rig::new(GRANTS, None, None);
    let mut ui = std::mem::replace(&mut r.viewer_ui, r.viewer.client(info(false)));
    let session = r.open_session(&mut ui, ALL.union(PermissionSet::TERMINAL));
    assert_eq!(open(&mut ui, session, false).1, TermOutcome::NobodyPresent);
    assert_eq!(
        ui.call(UiOp::TermOpen { session_id: session, cols: 0, rows: 24, admin: false }).unwrap_err(),
        ApiCode::Invalid
    );
    assert_eq!(
        ui.call(UiOp::TermOpen { session_id: session + 1, cols: 80, rows: 24, admin: false }).unwrap_err(),
        ApiCode::NotFound
    );
    drop(ui);
    r.finish();
    // A session the host's user gave no terminal to.
    let mut r = Rig::new(GRANTS, Some(SID), None);
    let mut ui = std::mem::replace(&mut r.viewer_ui, r.viewer.client(info(false)));
    let session = r.open_session(&mut ui, ALL);
    assert_eq!(open(&mut ui, session, false).1, TermOutcome::NotPermitted);
    drop(ui);
    r.finish();
}

#[test]
fn the_administrator_terminal_needs_an_elevated_connection_and_runs_as_system() {
    let mut r = Rig::new(GRANTS, Some(SID), None);
    // A plain connection is refused by the service itself (C2.3, D10): the UI turns this into the elevation step.
    let mut plain = std::mem::replace(&mut r.viewer_ui, r.viewer.client(info(false)));
    let plain_session = r.open_session(&mut plain, ALL.union(PermissionSet::TERMINAL));
    assert_eq!(
        plain.call(UiOp::TermOpen { session_id: plain_session, cols: 80, rows: 24, admin: true }).unwrap_err(),
        ApiCode::NeedsAdmin
    );
    // The host serves one session at a time: the plain one ends first.
    assert!(plain.call(UiOp::Disconnect { session_id: plain_session }).is_ok());
    wait_for("the host to be free", || r.host.core.host().sessions().is_empty().then_some(()));
    // An elevated one asks for `ADMIN_TERMINAL` when it connects and gets the SYSTEM shell.
    let mut elevated = r.viewer.client(info(true));
    let session = r.open_session(&mut elevated, GRANTS.union(PermissionSet::TERMINAL));
    let (id, outcome, account) = open(&mut elevated, session, true);
    assert_eq!((outcome, account.as_str()), (TermOutcome::Opened, "SYSTEM"));
    elevated.send(&Ipc::TermInput { term_id: id, bytes: Bytes(b"whoami\r".to_vec()) });
    assert_eq!(output(&mut elevated, id, 7), b"whoami\r");
    wait_for("the audit entry", || {
        r.host.audit.entries().iter().any(|e| e.event == AuditEvent::TerminalOpened { admin: true }).then_some(())
    });
    drop((plain, elevated));
    r.finish();
}

#[test]
fn a_shell_that_ends_is_told_to_the_ui_after_its_last_output() {
    let mut r = Rig::new(GRANTS, Some(SID), Some(Arc::new(Script)));
    let mut ui = std::mem::replace(&mut r.viewer_ui, r.viewer.client(info(false)));
    let session = r.open_session(&mut ui, ALL.union(PermissionSet::TERMINAL));
    let (id, outcome, account) = open(&mut ui, session, false);
    assert_eq!((outcome, account.as_str()), (TermOutcome::Opened, "scripted"));
    // Everything that reaches the UI about this terminal, in order, until its end.
    let mut seen = Vec::new();
    let code = ui.take("the end of the terminal", |m| match m {
        Ipc::TermOutput { term_id, bytes } if *term_id == id => {
            seen.push(format!("output {}", bytes.0.len()));
            None
        }
        Ipc::UiNotify(Notify::TermExited { term_id, exit_code }) if *term_id == id => Some(*exit_code),
        _ => None,
    });
    assert_eq!(code, Some(3));
    assert_eq!(seen, vec!["output 5".to_string()], "the bytes came first");
    // The id is forgotten with the end.
    assert_eq!(ui.call(UiOp::TermClose { term_id: id }).unwrap_err(), ApiCode::NotFound);
    drop(ui);
    r.finish();
}
