//! Terminals through two `ServiceCore`s on 127.0.0.1 (stage 43; flows F-TERM-01, F-TERM-02; D13): the viewer opens a
//! terminal through the core's own API, the host's shell runs in a user helper behind the in-memory pipe (the C7 echo
//! PTY, or a scripted one), and the administrator terminal runs in the host core itself.

use std::io::{Cursor, Read, Write};

use swapdesk_fakes::{FAKE_USER, FakePtySpawner};
use swapdesk_platform::{Pty, PtyRequest, PtySpawner, Result as PResult};
use swapdesk_trust::UserRef;

use super::*;
use crate::service::{TermOut, TerminalEnv};

const SID_A: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
const SID_B: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";
const TERM: PermissionSet = ALL.union(PermissionSet::TERMINAL);
const WITH_ADMIN: PermissionSet = ALL.union(PermissionSet::ADMIN_TERMINAL);

/// What the machine tells the core: who is logged in, and the administrator spawner (when it has one).
struct Env {
    user: Mutex<Option<String>>,
    admin: Option<Arc<dyn PtySpawner>>,
}

impl TerminalEnv for Env {
    fn console_user(&self) -> Option<String> {
        lock(&self.user).clone()
    }

    fn admin_spawner(&self) -> Option<Arc<dyn PtySpawner>> {
        self.admin.clone()
    }
}

/// A shell that prints "bye" and ends with code 3, by itself.
struct ScriptSpawner;

struct ScriptPty;

impl PtySpawner for ScriptSpawner {
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
    helper: Option<TestHelper>,
    user_spawner: Arc<FakePtySpawner>,
    admin_spawner: Arc<FakePtySpawner>,
    env: Arc<Env>,
    /// The console users the helper's spawner factory was asked for.
    asked_users: Arc<Mutex<Vec<Option<String>>>>,
}

impl Rig {
    /// The host stores `stored` for the viewer and its user's card picks all of it; `user` is logged in at the console.
    fn new(stored: PermissionSet) -> Self {
        Self::build(stored, Some(SID_A), true, true)
    }

    fn build(stored: PermissionSet, user: Option<&str>, with_helper: bool, with_admin: bool) -> Self {
        let host = Machine::new("host-pc", true, false);
        let viewer = Machine::new("viewer-pc", true, false);
        Machine::pair(&host, &viewer, stored);
        *lock(&host.ui.grants) = stored;
        let admin_spawner = Arc::new(FakePtySpawner::privileged());
        let env = Arc::new(Env {
            user: Mutex::new(user.map(str::to_owned)),
            admin: with_admin.then(|| Arc::clone(&admin_spawner) as Arc<dyn PtySpawner>),
        });
        assert!(host.core.set_terminal_env(env.clone()));
        assert!(!host.core.set_terminal_env(env.clone()), "the environment is set once");
        let user_spawner = Arc::new(FakePtySpawner::new());
        let asked_users = Arc::new(Mutex::new(Vec::new()));
        let helper = with_helper.then(|| {
            let (spawner, asked) = (Arc::clone(&user_spawner), Arc::clone(&asked_users));
            TestHelper::spawn_with(&host.core, IpcRole::HelperUser, move |deps| {
                deps.with_pty(move |user| {
                    lock(&asked).push(user);
                    Arc::clone(&spawner) as Arc<dyn PtySpawner>
                })
            })
        });
        Self { host, viewer, helper, user_spawner, admin_spawner, env, asked_users }
    }

    /// A prompted session (the host's card accepts what `new` stored).
    fn prompted(&self) -> (ViewerHandle, Seen) {
        let (handle, msgs) =
            self.viewer.core.connect(self.host.addr(), self.host.device(), Intent::Control).expect("the session opens");
        (handle, Seen::new(msgs))
    }

    /// An unattended session: no card; the viewer asks for the terminal, as the hub does, and the host grants what is
    /// stored. `granted_by` is the account that granted `UNATTENDED` to the viewer's device.
    fn unattended(&self, granted_by: &str) -> (ViewerHandle, Seen) {
        self.host
            .trust
            .update(|s| {
                for d in &mut s.devices {
                    d.unattended_granted_by = Some(UserRef { id: granted_by.into(), name: "someone".into() });
                }
            })
            .expect("store");
        let (handle, msgs) = self
            .viewer
            .core
            .connect_asking(self.host.addr(), self.host.device(), Intent::Control, true, PermissionSet::TERMINAL)
            .expect("the unattended session opens");
        assert_eq!(handle.mode(), swapdesk_proto::GrantMode::Unattended, "no card: {:?}", handle.granted());
        // This viewer speaks minor 1: it is in without a prompt and without the machine-level bit, and D13 still
        // knows nobody accepted the session.
        assert!(!handle.granted().contains(PermissionSet::UNATTENDED), "{:?}", handle.granted());
        assert_eq!(self.host.ui.prompts.load(Ordering::SeqCst), 0, "entered without a prompt");
        (handle, Seen::new(msgs))
    }

    fn open(&self, handle: &ViewerHandle, id: u32, admin: bool) -> Result<String, ErrorCode> {
        self.viewer.core.runtime().block_on(handle.term_open(id, 80, 24, admin))
    }

    fn pty_opens(&self) -> Vec<Ipc> {
        self.helper
            .as_ref()
            .map(|h| h.sent().into_iter().filter(|m| matches!(m, Ipc::PtyOpen { .. })).collect())
            .unwrap_or_default()
    }

    fn consents(&self) -> usize {
        self.host.ui.consents.load(Ordering::SeqCst)
    }

    fn finish(mut self) {
        self.host.core.shutdown();
        self.viewer.core.shutdown();
        if let Some(h) = self.helper.as_mut() {
            h.join();
        }
    }
}

/// The output of terminal `id` until `want` bytes arrived.
fn output(seen: &mut Seen, id: u32, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        got.extend(seen.next("terminal output", |o| match o {
            ViewerOut::Term(TermOut::Output { term_id, bytes }) if *term_id == id => Some(bytes.0.clone()),
            _ => None,
        }));
    }
    got
}

fn exited(seen: &mut Seen, id: u32) -> Option<i32> {
    seen.next("the end of the terminal", |o| match o {
        ViewerOut::Term(TermOut::Exited { term_id, code }) if *term_id == id => Some(*code),
        _ => None,
    })
}

// ---------- F-TERM-01: the user's shell, in the helper ----------

#[test]
fn a_terminal_runs_in_the_user_helper_and_relays_both_ways() {
    let rig = Rig::new(TERM);
    let (handle, mut seen) = rig.prompted();
    assert_eq!(rig.open(&handle, 1, false).as_deref(), Ok(FAKE_USER), "the account the helper's shell runs as");

    // The service never started a shell: the helper was asked, with the console user it must be.
    let opens = rig.pty_opens();
    assert_eq!(opens.len(), 1);
    assert!(
        matches!(&opens[0], Ipc::PtyOpen { cols: 80, rows: 24, session_user, .. } if session_user.0 == SID_A),
        "{opens:?}"
    );
    assert_eq!(*lock(&rig.asked_users), vec![Some(SID_A.to_string())]);
    assert_eq!(rig.user_spawner.requests(), vec![PtyRequest { cols: 80, rows: 24, admin: false }]);
    assert!(rig.admin_spawner.requests().is_empty(), "the administrator spawner was not touched");

    // Keystrokes in, shell output back (the echo PTY).
    assert!(handle.term_input(1, b"dir\r".to_vec()));
    assert_eq!(output(&mut seen, 1, 4), b"dir\r");
    assert!(handle.term_input(1, b"x".to_vec()));
    assert_eq!(output(&mut seen, 1, 1), b"x");

    // Resize reaches the shell.
    assert!(handle.term_resize(1, 100, 40));
    wait_for("the shell's resize", || (rig.user_spawner.resizes() == vec![(100, 40)]).then_some(()));

    // Closing ends the shell in the helper and reports nothing: the viewer asked for it.
    assert!(handle.term_close(1));
    wait_for("PtyClose at the helper", || {
        (rig.helper.as_ref()?.count(|m| matches!(m, Ipc::PtyClose { id: 1 })) == 1).then_some(())
    });
    assert!(
        !seen.any_within(Duration::from_millis(400), |o| matches!(o, ViewerOut::Term(TermOut::Exited { .. }))),
        "no TermExited for a close"
    );
    assert!(!handle.term_input(1, b"y".to_vec()), "a closed terminal takes nothing");
    rig.finish();
}

#[test]
fn a_shell_that_ends_by_itself_reaches_the_viewer_after_its_last_bytes() {
    let host = Machine::new("host-pc", true, false);
    let viewer = Machine::new("viewer-pc", true, false);
    Machine::pair(&host, &viewer, TERM);
    *lock(&host.ui.grants) = TERM;
    host.core.set_terminal_env(Arc::new(Env { user: Mutex::new(Some(SID_A.into())), admin: None }));
    let mut helper =
        TestHelper::spawn_with(&host.core, IpcRole::HelperUser, |deps| deps.with_pty(|_| Arc::new(ScriptSpawner)));
    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    let mut seen = Seen::new(msgs);
    let rt = viewer.core.runtime();
    assert_eq!(rt.block_on(handle.term_open(7, 80, 24, false)).as_deref(), Ok("scripted"));
    assert_eq!(output(&mut seen, 7, 5), b"bye\r\n");
    assert_eq!(exited(&mut seen, 7), Some(3), "the shell's exit code");
    // Nothing is left to close: the terminal is gone on both sides.
    assert!(!handle.term_close(7));
    host.core.shutdown();
    viewer.core.shutdown();
    helper.join();
}

#[test]
fn a_session_without_the_terminal_grant_is_refused_by_the_core() {
    let rig = Rig::new(ALL);
    let (handle, _seen) = rig.prompted();
    assert_eq!(rig.open(&handle, 1, false), Err(ErrorCode::NotPermitted));
    assert!(rig.pty_opens().is_empty(), "the helper was never asked");
    rig.finish();
}

#[test]
fn nobody_logged_in_refuses_a_user_terminal_and_asks_the_helper_for_nothing() {
    let rig = Rig::build(TERM, None, true, true);
    let (handle, _seen) = rig.prompted();
    assert_eq!(rig.open(&handle, 1, false), Err(ErrorCode::NobodyPresent), "F-TERM-01: no user, no user terminal");
    assert!(rig.pty_opens().is_empty());
    // The user going and coming back changes the next answer, not a cached one.
    *lock(&rig.env.user) = Some(SID_A.into());
    assert_eq!(rig.open(&handle, 2, false).as_deref(), Ok(FAKE_USER));
    rig.finish();
}

#[test]
fn no_user_helper_means_unavailable() {
    let rig = Rig::build(TERM, Some(SID_A), false, true);
    let (handle, _seen) = rig.prompted();
    assert_eq!(rig.open(&handle, 1, false), Err(ErrorCode::Unavailable));
    rig.finish();
}

#[test]
fn a_fifth_terminal_is_busy_and_a_closed_one_makes_room() {
    let rig = Rig::new(TERM);
    let (handle, _seen) = rig.prompted();
    for id in 1..=4 {
        assert_eq!(rig.open(&handle, id, false).as_deref(), Ok(FAKE_USER), "terminal {id}");
    }
    assert_eq!(rig.open(&handle, 5, false), Err(ErrorCode::Busy), "at most four per session");
    assert!(handle.term_close(2));
    wait_for("room for another", || (rig.open(&handle, 6, false).is_ok()).then_some(()));
    assert_eq!(rig.open(&handle, 1, false), Err(ErrorCode::ProtocolViolation), "ids are unique");
    rig.finish();
}

#[test]
fn a_helper_that_vanishes_ends_its_terminals() {
    let rig = Rig::new(TERM);
    let (handle, mut seen) = rig.prompted();
    assert!(rig.open(&handle, 1, false).is_ok());
    // The helper goes away (as if it crashed): its pipe ends.
    assert!(rig.host.core.helper(HelperKind::User).expect("attached").send_now(Ipc::Shutdown));
    assert_eq!(exited(&mut seen, 1), Some(-1), "no exit code came: the shell counts as failed");
    assert_eq!(rig.open(&handle, 2, false), Err(ErrorCode::Unavailable), "and a new one cannot start");
    rig.finish();
}

#[test]
fn revoking_the_grant_ends_the_terminal_within_the_second() {
    let rig = Rig::new(TERM);
    let (handle, mut seen) = rig.prompted();
    assert!(rig.open(&handle, 1, false).is_ok());
    let started = Instant::now();
    assert!(rig.host.core.host().set_session_grants(handle.session_id(), ALL));
    assert_eq!(exited(&mut seen, 1), None, "the host ended it: no shell exit code");
    assert!(started.elapsed() < Duration::from_secs(3), "F-PERM-02: {:?}", started.elapsed());
    wait_for("PtyClose at the helper", || {
        (rig.helper.as_ref()?.count(|m| matches!(m, Ipc::PtyClose { id: 1 })) == 1).then_some(())
    });
    rig.finish();
}

#[test]
fn the_end_of_the_session_ends_the_shells() {
    let rig = Rig::new(TERM);
    let (handle, _seen) = rig.prompted();
    assert!(rig.open(&handle, 1, false).is_ok());
    handle.disconnect();
    wait_for("PtyClose at the helper", || {
        (rig.helper.as_ref()?.count(|m| matches!(m, Ipc::PtyClose { .. })) == 1).then_some(())
    });
    wait_for("the host session to end", || rig.host.core.host().sessions().is_empty().then_some(()));
    rig.finish();
}

// ---------- F-TERM-02: the administrator terminal runs in the service ----------

#[test]
fn the_administrator_terminal_runs_in_the_service_never_in_the_helper() {
    let rig = Rig::new(WITH_ADMIN);
    let (handle, msgs) = rig
        .viewer
        .core
        .connect_asking(rig.host.addr(), rig.host.device(), Intent::Control, false, PermissionSet::ADMIN_TERMINAL)
        .expect("the session opens");
    let mut seen = Seen::new(msgs);
    assert_eq!(rig.open(&handle, 1, true).as_deref(), Ok("SYSTEM"), "labelled by its account");
    assert_eq!(rig.admin_spawner.requests(), vec![PtyRequest { cols: 80, rows: 24, admin: true }]);
    assert!(rig.pty_opens().is_empty(), "the helper never saw it");
    assert!(rig.user_spawner.requests().is_empty());
    assert!(handle.term_input(1, b"whoami\r".to_vec()));
    assert_eq!(output(&mut seen, 1, 7), b"whoami\r");
    rig.finish();
}

#[test]
fn the_administrator_terminal_needs_it_asked_for_and_a_spawner() {
    // D10: a viewer that did not ask for it in its `Hello` cannot open it, whatever the host stored.
    let rig = Rig::new(WITH_ADMIN);
    let (plain, _seen) = rig.prompted();
    assert_eq!(rig.open(&plain, 1, true), Err(ErrorCode::NotPermitted));
    assert!(rig.admin_spawner.requests().is_empty());
    drop(plain);
    // Asked for and granted, but this machine has no administrator spawner (a development run as a user).
    let rig2 = Rig::build(WITH_ADMIN, Some(SID_A), true, false);
    let (handle, _seen) = rig2
        .viewer
        .core
        .connect_asking(rig2.host.addr(), rig2.host.device(), Intent::Control, false, PermissionSet::ADMIN_TERMINAL)
        .map(|(h, m)| (h, Seen::new(m)))
        .expect("the session opens");
    assert_eq!(rig2.open(&handle, 1, true), Err(ErrorCode::Unavailable));
    rig.finish();
    rig2.finish();
}

#[test]
fn the_administrator_terminal_opens_with_nobody_logged_in() {
    let rig = Rig::build(WITH_ADMIN, None, true, true);
    let (handle, _seen) = rig
        .viewer
        .core
        .connect_asking(rig.host.addr(), rig.host.device(), Intent::Control, false, PermissionSet::ADMIN_TERMINAL)
        .map(|(h, m)| (h, Seen::new(m)))
        .expect("the session opens");
    assert_eq!(rig.open(&handle, 1, true).as_deref(), Ok("SYSTEM"));
    assert_eq!(rig.open(&handle, 2, false), Err(ErrorCode::NobodyPresent), "a user terminal still needs a user");
    rig.finish();
}

// ---------- D13: another user's terminal in an unattended session asks that user first ----------

#[test]
fn d13_the_user_who_granted_unattended_access_is_not_asked() {
    let rig = Rig::build(TERM.union(PermissionSet::UNATTENDED), Some(SID_A), true, true);
    let (handle, _seen) = rig.unattended(SID_A);
    assert_eq!(rig.open(&handle, 1, false).as_deref(), Ok(FAKE_USER));
    assert_eq!(rig.consents(), 0, "the granter is the one logged in: nothing to ask");
    rig.finish();
}

#[test]
fn d13_another_user_is_asked_first_and_a_yes_opens_the_terminal() {
    let rig = Rig::build(TERM.union(PermissionSet::UNATTENDED), Some(SID_B), true, true);
    let (handle, _seen) = rig.unattended(SID_A);
    assert_eq!(rig.open(&handle, 1, false).as_deref(), Ok(FAKE_USER));
    assert_eq!(rig.consents(), 1, "the logged-in user was asked once");
    // The shell was started for the user who said yes, not for the granter.
    assert_eq!(*lock(&rig.asked_users), vec![Some(SID_B.to_string())]);
    rig.finish();
}

#[test]
fn d13_a_no_from_the_other_user_refuses_and_starts_nothing() {
    let rig = Rig::build(TERM.union(PermissionSet::UNATTENDED), Some(SID_B), true, true);
    *lock(&rig.host.ui.consent_answer) = Some(false);
    let (handle, _seen) = rig.unattended(SID_A);
    assert_eq!(rig.open(&handle, 1, false), Err(ErrorCode::Declined));
    assert_eq!(rig.consents(), 1);
    assert!(rig.pty_opens().is_empty(), "no shell was started");
    rig.finish();
}

#[test]
fn d13_nobody_present_is_refused_without_asking() {
    let rig = Rig::build(TERM.union(PermissionSet::UNATTENDED), None, true, true);
    let (handle, _seen) = rig.unattended(SID_A);
    assert_eq!(rig.open(&handle, 1, false), Err(ErrorCode::NobodyPresent));
    assert_eq!(rig.consents(), 0, "there is nobody to ask");
    rig.finish();
}

#[test]
fn d13_a_prompted_session_and_the_administrator_terminal_never_ask() {
    // A prompted session had its user's say at the card.
    let rig = Rig::build(TERM, Some(SID_B), true, true);
    let (handle, _seen) = rig.prompted();
    assert_eq!(rig.open(&handle, 1, false).as_deref(), Ok(FAKE_USER));
    assert_eq!(rig.consents(), 0);
    rig.finish();
    // The administrator terminal holds no user's data, so D13 never asks about it. A viewer of minor 1 that entered
    // without a prompt does not hold it yet (it comes with Elevate): the open is refused, and nobody is asked.
    let rig =
        Rig::build(TERM.union(PermissionSet::UNATTENDED).union(PermissionSet::ADMIN_TERMINAL), Some(SID_B), true, true);
    rig.host
        .trust
        .update(|s| {
            s.devices
                .iter_mut()
                .for_each(|d| d.unattended_granted_by = Some(UserRef { id: SID_A.into(), name: "someone".into() }))
        })
        .expect("store");
    let (handle, _msgs) = rig
        .viewer
        .core
        .connect_asking(
            rig.host.addr(),
            rig.host.device(),
            Intent::Control,
            true,
            PermissionSet::TERMINAL.union(PermissionSet::ADMIN_TERMINAL),
        )
        .expect("the unattended session opens");
    assert_eq!(handle.mode(), swapdesk_proto::GrantMode::Unattended);
    assert_eq!(rig.open(&handle, 1, true), Err(ErrorCode::NotPermitted));
    assert_eq!(rig.consents(), 0);
    rig.finish();
}

#[test]
fn the_consent_step_follows_run_consent() {
    use swapdesk_term::{ConsentFacts, ConsentNeed, consent_need};
    // The step the handler runs is `consent_need` decided, with the question asked through the session: the same table
    // as `swapdesk_term::run_consent` (tested there), so the service must not invent its own rule.
    let facts = |unattended, granted_by, console_user, admin| ConsentFacts {
        session_unattended: unattended,
        granted_by,
        console_user,
        admin_terminal: admin,
        account: None,
    };
    assert_eq!(consent_need(&facts(false, Some(SID_A), Some(SID_B), false)), ConsentNeed::NotNeeded);
    assert_eq!(consent_need(&facts(true, Some(SID_A), Some(SID_A), false)), ConsentNeed::NotNeeded);
    assert_eq!(consent_need(&facts(true, Some(SID_A), Some(SID_B), false)), ConsentNeed::Ask);
    assert_eq!(
        consent_need(&facts(true, None, Some(SID_B), false)),
        ConsentNeed::Ask,
        "an unknown granter is not assumed to be the user"
    );
    assert_eq!(consent_need(&facts(true, Some(SID_A), None, false)), ConsentNeed::Refuse(ErrorCode::NobodyPresent));
    assert_eq!(consent_need(&facts(true, Some(SID_A), Some(SID_B), true)), ConsentNeed::NotNeeded);
}
