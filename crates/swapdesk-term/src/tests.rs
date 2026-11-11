use std::io::{Cursor, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use swapdesk_fakes::{FakeActions, FakePtySpawner};
use swapdesk_platform::{ErrorKind, OsGrant, PlatformError, Pty, PtyRequest, PtySpawner, Result as PResult};
use swapdesk_proto::{Action, ErrorCode, Feature, PermissionSet, QuickAction, TermOpen};

use crate::*;

const WAIT: Duration = Duration::from_secs(5);

fn open_req(id: u32, admin: bool) -> TermOpen {
    TermOpen { term_id: id, cols: 80, rows: 24, admin }
}

fn user_ctx() -> OpenContext {
    OpenContext { granted: PermissionSet::TERMINAL, user_present: true }
}

fn host(spawner: impl PtySpawner + 'static) -> (TermHost, Receiver<TermEvent>) {
    TermHost::new(Arc::new(spawner), 16)
}

/// Output of one terminal until `want` bytes arrived.
fn collect(rx: &Receiver<TermEvent>, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        match rx.recv_timeout(WAIT).expect("an event") {
            TermEvent::Output { data, .. } => got.extend(data),
            other => panic!("unexpected {other:?}"),
        }
    }
    got
}

// ---------- TermHost ----------

#[test]
fn the_shell_opens_as_the_user_and_relays_both_ways() {
    let (mut h, rx) = host(FakePtySpawner::new());
    let opened = h.open(&open_req(1, false), &user_ctx()).unwrap();
    assert_eq!((opened.term_id, opened.account.as_str()), (1, "fakeuser"));
    h.input(1, b"dir\r".to_vec()).unwrap();
    assert_eq!(collect(&rx, 4), b"dir\r");
    h.input_sender(1).unwrap().send(b"x".to_vec()).unwrap();
    assert_eq!(collect(&rx, 1), b"x");
    h.resize(1, 100, 40).unwrap();
    assert_eq!(h.resize(1, 0, 40), Err(ErrorCode::ProtocolViolation));
    assert_eq!(h.resize(9, 10, 10), Err(ErrorCode::NotFound));
    h.close(1).unwrap();
    assert_eq!(h.input(1, b"y".to_vec()), Err(ErrorCode::NotFound));
    assert_eq!(h.close(1), Err(ErrorCode::NotFound));
    // A closed terminal sends no TermExited.
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
}

#[test]
fn resizes_reach_the_pty() {
    let spawner = Arc::new(FakePtySpawner::new());
    let (mut h, _rx) = TermHost::new(Arc::clone(&spawner) as Arc<dyn PtySpawner>, 4);
    h.open(&open_req(1, false), &user_ctx()).unwrap();
    h.resize(1, 100, 40).unwrap();
    assert_eq!(spawner.resizes(), vec![(100, 40)]);
    assert_eq!(spawner.requests(), vec![PtyRequest { cols: 80, rows: 24, admin: false }]);
}

#[test]
fn at_most_four_terminals_and_ids_are_unique() {
    let (mut h, _rx) = host(FakePtySpawner::new());
    for id in 1..=4 {
        h.open(&open_req(id, false), &user_ctx()).unwrap();
    }
    assert_eq!(h.open(&open_req(5, false), &user_ctx()), Err(ErrorCode::Busy));
    assert_eq!(h.open(&open_req(2, false), &user_ctx()), Err(ErrorCode::ProtocolViolation));
    h.close(3).unwrap();
    h.open(&open_req(5, false), &user_ctx()).unwrap();
    assert_eq!(h.open_count(), 4);
}

#[test]
fn bad_sizes_are_refused() {
    let (mut h, _rx) = host(FakePtySpawner::new());
    for (c, r) in [(0, 24), (80, 0), (1001, 24), (80, 5000)] {
        let req = TermOpen { term_id: 1, cols: c, rows: r, admin: false };
        assert_eq!(h.open(&req, &user_ctx()), Err(ErrorCode::ProtocolViolation), "{c}x{r}");
    }
    assert_eq!(h.open_count(), 0);
}

#[test]
fn a_terminal_needs_the_grant_and_a_logged_in_user() {
    let (mut h, _rx) = host(FakePtySpawner::new());
    let no_grant = OpenContext { granted: PermissionSet::CONTROL, user_present: true };
    assert_eq!(h.open(&open_req(1, false), &no_grant), Err(ErrorCode::NotPermitted));
    let nobody = OpenContext { granted: PermissionSet::TERMINAL, user_present: false };
    assert_eq!(
        h.open(&open_req(1, false), &nobody),
        Err(ErrorCode::NobodyPresent),
        "F-TERM-01: no user, no user terminal"
    );
    assert_eq!(h.open_count(), 0);
}

#[test]
fn the_administrator_terminal_needs_its_grant_and_a_privileged_spawner() {
    let admin_ctx = |present| OpenContext { granted: PermissionSet::ADMIN_TERMINAL, user_present: present };
    // The user helper's spawner is not SYSTEM: it refuses `admin` itself (C3).
    let (mut h, _rx) = host(FakePtySpawner::new());
    assert_eq!(h.open(&open_req(1, true), &admin_ctx(true)), Err(ErrorCode::NotPermitted));
    // The service's spawner is: SYSTEM shell, labelled by its account, allowed with nobody logged in.
    let (mut h, _rx) = host(FakePtySpawner::privileged());
    assert_eq!(h.open(&open_req(1, true), &admin_ctx(false)).unwrap().account, "SYSTEM");
    // TERMINAL alone never opens an administrator terminal.
    assert_eq!(h.open(&open_req(2, true), &user_ctx()), Err(ErrorCode::NotPermitted));
}

#[test]
fn revoked_grants_end_the_terminals_they_covered() {
    let (mut h, _rx) = host(FakePtySpawner::privileged());
    h.open(&open_req(1, false), &user_ctx()).unwrap();
    h.open(&open_req(2, true), &OpenContext { granted: PermissionSet::ADMIN_TERMINAL, user_present: true }).unwrap();
    assert_eq!(h.enforce(PermissionSet::TERMINAL), vec![2], "only the administrator terminal went");
    assert_eq!(h.open_count(), 1);
    assert_eq!(h.enforce(PermissionSet::VIEW), vec![1]);
    assert_eq!(h.open_count(), 0);
}

struct ScriptPty;

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
        Ok(Some(7))
    }
    fn kill(&mut self) -> PResult<()> {
        Ok(())
    }
}

struct ScriptSpawner;

impl PtySpawner for ScriptSpawner {
    fn spawn(&self, _: &PtyRequest) -> PResult<Box<dyn Pty>> {
        Ok(Box::new(ScriptPty))
    }
}

#[test]
fn an_exiting_shell_reports_its_code_after_its_last_output() {
    let (mut h, rx) = host(ScriptSpawner);
    h.open(&open_req(3, false), &user_ctx()).unwrap();
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), TermEvent::Output { term_id: 3, data: b"bye\r\n".to_vec() });
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), TermEvent::Exited { term_id: 3, exit_code: Some(7) });
    h.forget(3);
    assert_eq!(h.open_count(), 0);
}

/// A shell that never ends and never writes: its output stays open, and `try_wait` is counted.
struct SilentPty {
    looks: Arc<std::sync::atomic::AtomicUsize>,
    killed: Arc<std::sync::atomic::AtomicBool>,
}

/// A reader that blocks until the PTY is killed, like a shell's output pipe.
struct UntilKilled(Arc<std::sync::atomic::AtomicBool>);

impl Read for UntilKilled {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        while !self.0.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(0)
    }
}

impl Pty for SilentPty {
    fn account(&self) -> &str {
        "silent"
    }
    fn reader(&mut self) -> PResult<Box<dyn Read + Send>> {
        Ok(Box::new(UntilKilled(Arc::clone(&self.killed))))
    }
    fn writer(&mut self) -> PResult<Box<dyn Write + Send>> {
        Ok(Box::new(std::io::sink()))
    }
    fn resize(&mut self, _: u16, _: u16) -> PResult<()> {
        Ok(())
    }
    fn try_wait(&mut self) -> PResult<Option<i32>> {
        self.looks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(None)
    }
    fn kill(&mut self) -> PResult<()> {
        self.killed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

struct SilentSpawner(Arc<std::sync::atomic::AtomicUsize>, Arc<std::sync::atomic::AtomicBool>);

impl PtySpawner for SilentSpawner {
    fn spawn(&self, _: &PtyRequest) -> PResult<Box<dyn Pty>> {
        Ok(Box::new(SilentPty { looks: Arc::clone(&self.0), killed: Arc::clone(&self.1) }))
    }
}

#[test]
fn a_running_shell_is_looked_at_with_a_growing_pause_not_every_25_ms() {
    let looks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let killed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut h, _rx) = host(SilentSpawner(Arc::clone(&looks), Arc::clone(&killed)));
    h.open(&open_req(1, false), &user_ctx()).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let n = looks.load(std::sync::atomic::Ordering::SeqCst);
    // 25, 50, 100, then 200 ms apart: about 10 looks in 1.5 s. A fixed 25 ms pause would be about 60.
    assert!((3..=14).contains(&n), "{n} looks at the shell in 1.5 s");
    h.close(1).unwrap();
}

#[test]
fn closing_a_terminal_wakes_its_watcher_at_once() {
    let looks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let killed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut h, rx) = host(SilentSpawner(Arc::clone(&looks), Arc::clone(&killed)));
    h.open(&open_req(1, false), &user_ctx()).unwrap();
    // Let the pause grow to its longest, then close: the watcher ends without waiting that long, and no `Exited` follows.
    std::thread::sleep(Duration::from_millis(600));
    let before = looks.load(std::sync::atomic::Ordering::SeqCst);
    h.close(1).unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "the viewer asked for the close: no Exited");
    std::thread::sleep(Duration::from_millis(500));
    let after = looks.load(std::sync::atomic::Ordering::SeqCst);
    // One look may have been due between reading `before` and closing; none after that.
    assert!(after <= before + 1, "the watcher kept looking after the close: {before} then {after}");
}

struct DeniedSpawner(ErrorKind);

impl PtySpawner for DeniedSpawner {
    fn spawn(&self, _: &PtyRequest) -> PResult<Box<dyn Pty>> {
        Err(PlatformError { kind: self.0, op: "spawn", os_code: Some(5), detail: "secret detail".into() })
    }
}

#[test]
fn a_failed_spawn_maps_to_a_code_without_the_detail() {
    for (kind, code) in [
        (ErrorKind::PermissionDenied(OsGrant::Administrator), ErrorCode::NotPermitted),
        (ErrorKind::Unavailable, ErrorCode::Unavailable),
        (ErrorKind::Os, ErrorCode::IoFailed),
    ] {
        let (mut h, _rx) = host(DeniedSpawner(kind));
        assert_eq!(h.open(&open_req(1, false), &user_ctx()), Err(code));
        assert_eq!(h.open_count(), 0);
    }
}

#[test]
fn a_dropped_event_receiver_ends_the_shell() {
    let (mut h, rx) = TermHost::new(Arc::new(FakePtySpawner::new()), 1);
    h.open(&open_req(1, false), &user_ctx()).unwrap();
    drop(rx);
    h.input(1, b"a".to_vec()).unwrap();
    // The reader thread fails its send, kills the PTY, and the writer then fails: the queue eventually refuses.
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if h.input(1, b"b".to_vec()).is_err() || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.input(1, b"c".to_vec()).is_err(), "the terminal is gone with its listener");
}

// ---------- wire ----------

#[test]
fn the_stream_header_is_the_term_id_little_endian() {
    assert_eq!(term_header(0x0102_0304), [4, 3, 2, 1]);
    assert_eq!(parse_term_header(&[4, 3, 2, 1, 99, 99]), Some(0x0102_0304));
    assert_eq!(parse_term_header(&[1, 2, 3]), None);
}

// ---------- quick actions ----------

fn qa(kind: Action) -> QuickAction {
    QuickAction { action_id: 42, kind }
}

fn actx(granted: PermissionSet, user_present: bool) -> ActionContext {
    ActionContext { granted, user_present, paused: false }
}

#[test]
fn quick_actions_run_through_the_runner_and_report_success() {
    let fake = FakeActions::new();
    let mut relay = ActionRelay::new(fake.clone());
    let done = relay.handle(&qa(Action::Lock), &actx(PermissionSet::CONTROL, true));
    assert_eq!(
        (done.result.action_id, done.result.code, done.ran, done.action),
        (42, ErrorCode::NoError, true, Action::Lock)
    );
    for a in [Action::Sleep, Action::Restart, Action::TaskMonitor] {
        assert_eq!(relay.handle(&qa(a), &actx(PermissionSet::CONTROL, true)).result.code, ErrorCode::NoError);
    }
    assert_eq!(fake.ran(), vec![Action::Lock, Action::Sleep, Action::Restart, Action::TaskMonitor]);
}

/// A paused session (the host at its lock, sign-in or secure desktop, the session without `UNATTENDED`) may not
/// restart, suspend or lock the computer: its input is dropped there, and so are its actions.
#[test]
fn a_paused_session_runs_no_quick_action() {
    let fake = FakeActions::new();
    let mut relay = ActionRelay::new(fake.clone());
    for a in [Action::Restart, Action::Sleep, Action::Lock, Action::TaskMonitor] {
        for user_present in [true, false] {
            let ctx = ActionContext { granted: PermissionSet::CONTROL, user_present, paused: true };
            let done = relay.handle(&qa(a), &ctx);
            assert_eq!((done.result.code, done.ran), (ErrorCode::Unavailable, false), "{a:?}");
        }
    }
    assert!(fake.ran().is_empty(), "nothing reached the runner");
    // The same session, back on the user's desktop, runs them.
    let done = relay.handle(&qa(Action::Restart), &actx(PermissionSet::CONTROL, true));
    assert_eq!((done.result.code, done.ran), (ErrorCode::NoError, true));
}

#[test]
fn quick_actions_need_control() {
    let fake = FakeActions::new();
    let mut relay = ActionRelay::new(fake.clone());
    let done = relay.handle(&qa(Action::Restart), &actx(PermissionSet::VIEW.union(PermissionSet::TERMINAL), true));
    assert_eq!((done.result.code, done.ran), (ErrorCode::NotPermitted, false));
    assert!(fake.ran().is_empty(), "a refused action never reaches the runner");
}

#[test]
fn the_task_monitor_is_refused_with_nobody_logged_in_but_the_system_calls_go_on() {
    let fake = FakeActions::new();
    let mut relay = ActionRelay::new(fake.clone());
    let nobody = actx(PermissionSet::CONTROL, false);
    let done = relay.handle(&qa(Action::TaskMonitor), &nobody);
    assert_eq!((done.result.code, done.ran), (ErrorCode::NobodyPresent, false));
    for a in [Action::Lock, Action::Sleep, Action::Restart] {
        assert_eq!(relay.handle(&qa(a), &nobody).result.code, ErrorCode::NoError);
    }
    assert_eq!(fake.ran(), vec![Action::Lock, Action::Sleep, Action::Restart]);
}

#[test]
fn a_runner_failure_is_reported_as_a_code() {
    let fake = FakeActions::new();
    fake.refuse(Action::Sleep, ErrorKind::Unsupported);
    fake.refuse(Action::Restart, ErrorKind::Os);
    let mut relay = ActionRelay::new(fake);
    let ctx = actx(PermissionSet::CONTROL, true);
    assert_eq!(relay.handle(&qa(Action::Sleep), &ctx).result.code, ErrorCode::NotPermitted);
    let done = relay.handle(&qa(Action::Restart), &ctx);
    assert_eq!((done.result.code, done.ran), (ErrorCode::IoFailed, true));
    relay.keep_awake(true).unwrap();
    assert_eq!(relay.runner().awake_calls(), vec![true]);
}

// ---------- D13 ----------

struct Asker {
    answer: Result<(), ErrorCode>,
    asked: Vec<Feature>,
}

impl ConsentAsker for Asker {
    fn ask(&mut self, feature: Feature) -> Result<(), ErrorCode> {
        self.asked.push(feature);
        self.answer
    }
}

fn facts<'a>(unattended: bool, granter: Option<&'a str>, user: Option<&'a str>) -> ConsentFacts<'a> {
    ConsentFacts {
        account: None,
        session_unattended: unattended,
        granted_by: granter,
        console_user: user,
        admin_terminal: false,
    }
}

#[test]
fn d13_asks_only_for_another_users_terminal_in_an_unattended_session() {
    use ConsentNeed::*;
    assert_eq!(
        consent_need(&facts(false, Some("S-1"), Some("S-2"))),
        NotNeeded,
        "a prompted session already had its user's say"
    );
    assert_eq!(consent_need(&facts(true, Some("S-1"), Some("S-1"))), NotNeeded, "the granter is the logged-in user");
    assert_eq!(consent_need(&facts(true, Some("S-1"), Some("S-2"))), Ask);
    assert_eq!(consent_need(&facts(true, None, Some("S-2"))), Ask, "an unknown granter is never assumed");
    assert_eq!(consent_need(&facts(true, Some("S-1"), None)), Refuse(ErrorCode::NobodyPresent));
    let admin = ConsentFacts { admin_terminal: true, ..facts(true, Some("S-1"), None) };
    assert_eq!(consent_need(&admin), NotNeeded, "the administrator terminal holds no user's data");
}

#[test]
fn the_consent_step_asks_once_and_obeys_the_answer() {
    let mut yes = Asker { answer: Ok(()), asked: vec![] };
    assert_eq!(run_consent(&facts(true, Some("S-1"), Some("S-2")), Feature::Terminal, &mut yes), Ok(()));
    assert_eq!(yes.asked, vec![Feature::Terminal]);
    for refusal in [ErrorCode::Declined, ErrorCode::Timeout, ErrorCode::NobodyPresent] {
        let mut no = Asker { answer: Err(refusal), asked: vec![] };
        assert_eq!(run_consent(&facts(true, Some("S-1"), Some("S-2")), Feature::Terminal, &mut no), Err(refusal));
    }
    let mut never = Asker { answer: Ok(()), asked: vec![] };
    assert_eq!(run_consent(&facts(true, Some("S-1"), Some("S-1")), Feature::Terminal, &mut never), Ok(()));
    assert_eq!(
        run_consent(&facts(true, Some("S-1"), None), Feature::Terminal, &mut never),
        Err(ErrorCode::NobodyPresent)
    );
    assert!(never.asked.is_empty(), "nothing is asked when the answer is already known");
}

/// ACCOUNTS.md 5.4: on a machine of several accounts the terminal and the files run only as the session's own
/// account; nobody is asked, nobody logged in is "unavailable", and the administrator terminal still opens.
#[test]
fn terminal_and_files_never_run_as_another_account() {
    let at = |console: Option<&'static str>, admin: bool| ConsentFacts {
        session_unattended: true,
        granted_by: Some("S-1-5-21-1"),
        console_user: console,
        admin_terminal: admin,
        account: Some("S-1-5-21-1"),
    };
    assert_eq!(consent_need(&at(Some("S-1-5-21-1"), false)), ConsentNeed::NotNeeded, "its own account");
    assert_eq!(consent_need(&at(Some("s-1-5-21-1"), false)), ConsentNeed::NotNeeded, "SIDs compare without case");
    assert_eq!(consent_need(&at(Some("S-1-5-21-2"), false)), ConsentNeed::Refuse(ErrorCode::AccountAway));
    assert_eq!(consent_need(&at(None, false)), ConsentNeed::Refuse(ErrorCode::Unavailable));
    assert_eq!(consent_need(&at(None, true)), ConsentNeed::NotNeeded, "the administrator terminal");
    // Never a question: no fact ever gives `Ask`.
    for console in [None, Some("S-1-5-21-1"), Some("S-1-5-21-2")] {
        for admin in [false, true] {
            assert_ne!(consent_need(&at(console, admin)), ConsentNeed::Ask);
        }
    }
}
