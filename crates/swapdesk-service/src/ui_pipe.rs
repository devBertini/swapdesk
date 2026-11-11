//! The UI pipe of one interactive session (C2.1, C2.3): `\\.\pipe\swapdesk\ui-<session>[-<instance>]`, created with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` (a squatted name is logged as an alert and no UI pipe exists for that session),
//! one thread accepting, one thread per connection.
//!
//! A connection is checked in this order, and refused with `Bye` at the first failure: its process is in the pipe's
//! session; its first message is `Hello` with role UI or Elevator; its impersonation token's user is the session's
//! user; the connection is **admin** only when that token is elevated with Administrators enabled, and may ask for an
//! elevation (`can_elevate`) when the token holds the Administrators group, enabled or deny-only. A UI goes to the
//! hub's serve loop; an Elevator goes to the hub's restricted loop, which answers one `ElevationDescribe` and one
//! `ElevationConfirm` (docs/ELEVATION.md).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_host::ui_hub::{ConnInfo, UiHub};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake, IpcRole};
use swapdesk_platform::{Level, TraceSink};
use swapdesk_proto::ErrorCode;
use swapdesk_sys_win::{PipeAcl, PipeConn, PipeError, PipeListener, PipeSpec, current_user_sid, session_user_name};
use swapdesk_trust::UserRef;

/// How long a client has to send its `Hello`.
const HELLO_WITHIN: Duration = Duration::from_secs(5);
/// How often the accept loop looks at the stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(500);

/// Which user may open the UI pipes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `--console`: the current user, who also owns the pipe.
    Dev,
    /// The service: SYSTEM owns the pipe, the session's logged-on user is the one client.
    Production,
}

/// The SID of the user logged on in `session` (production: the SYSTEM service asks `WTSQueryUserToken`).
pub fn session_user_sid(session: u32) -> Result<String, String> {
    swapdesk_sys_win::session_user_sid(session).map_err(|e| format!("no user token for session {session}: {e}"))
}

/// The hub of the account a client's SID belongs to (ACCOUNTS.md 7.1: an interface reaches only its own account).
/// An error refuses the connection with why.
pub type Route = Arc<dyn Fn(&str) -> Result<UiHub, String> + Send + Sync>;

/// What the accept loops share.
pub struct UiPipes {
    route: Route,
    pub trace: Arc<dyn TraceSink>,
    pub mode: Mode,
    /// `-<n>` with `--instance`, else empty.
    pub suffix: String,
    pub stop: Arc<AtomicBool>,
    /// A second owner of every open connection's pipe, to cancel its blocked read at shutdown.
    open: Mutex<Vec<(u64, PipeConn)>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    sessions: OpenSessions,
    next: std::sync::atomic::AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The sessions whose UI pipe has a bound listener.
#[derive(Default)]
struct OpenSessions(Mutex<Vec<u32>>);

impl OpenSessions {
    /// Runs `open` unless `session` is served already, and counts the session as served only when `open` returned
    /// `Some`: a session whose pipe could not be made (no user token yet at the logon screen after boot, a squatted
    /// name) is tried again by the next call. The lock is held across `open`, so two callers never bind one name.
    fn ensure<T>(&self, session: u32, open: impl FnOnce() -> Option<T>) -> Option<T> {
        let mut served = lock(&self.0);
        if served.contains(&session) {
            return None;
        }
        let opened = open()?;
        served.push(session);
        Some(opened)
    }

    /// The session's listener is gone: the next `ensure` opens a new one.
    fn forget(&self, session: u32) {
        lock(&self.0).retain(|s| *s != session);
    }
}

impl UiPipes {
    /// Every client reaches `hub` (one account: the tests).
    #[cfg(test)]
    pub fn new(hub: UiHub, trace: Arc<dyn TraceSink>, mode: Mode, suffix: String, stop: Arc<AtomicBool>) -> Arc<Self> {
        Self::routed(Arc::new(move |_sid: &str| Ok(hub.clone())), trace, mode, suffix, stop)
    }

    /// A client reaches the hub `route` gives for its SID.
    pub fn routed(
        route: Route,
        trace: Arc<dyn TraceSink>,
        mode: Mode,
        suffix: String,
        stop: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            route,
            trace,
            mode,
            suffix,
            stop,
            open: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            sessions: OpenSessions::default(),
            next: std::sync::atomic::AtomicU64::new(1),
        })
    }

    fn log(&self, level: Level, what: core::fmt::Arguments<'_>) {
        self.trace.log(level, "ui-pipe", what);
    }

    /// Makes sure `ui-<session>` has a listener (once per session id). A session counts as served only once its
    /// listener is bound: a failed attempt (no user token yet at the logon screen, a squatted name) is tried again at
    /// the next call, which is the next console change or logon of that session.
    pub fn ensure(self: &Arc<Self>, session: u32) {
        let Some(listener) = self.sessions.ensure(session, || self.bind(session)) else { return };
        self.accept_on(session, listener);
    }

    /// Creates the listener of `session`'s pipe with the ACL its mode calls for; `None` (logged) when it cannot.
    fn bind(&self, session: u32) -> Option<PipeListener> {
        // The user the pipe is for.
        let acl = match self.mode {
            Mode::Dev => PipeAcl::dev(),
            Mode::Production => match session_user_sid(session) {
                Ok(sid) => Ok(PipeAcl::system_and_user(sid)),
                Err(e) => {
                    self.log(Level::Error, format_args!("alert: no UI pipe for session {session}: {e}"));
                    return None;
                }
            },
        };
        let acl = match acl {
            Ok(a) => a,
            Err(e) => {
                self.log(Level::Error, format_args!("no UI pipe for session {session}: {e}"));
                return None;
            }
        };
        let mut spec = PipeSpec::ui(session, acl);
        spec.name.push_str(&self.suffix);
        let listener = match PipeListener::bind(spec) {
            Ok(l) => l,
            Err(PipeError::Squatted { name }) => {
                self.log(Level::Error, format_args!("alert: the UI pipe name {name} is already owned by another process; no UI pipe for session {session}"));
                return None;
            }
            Err(e) => {
                self.log(Level::Error, format_args!("cannot create the UI pipe of session {session}: {e}"));
                return None;
            }
        };
        self.log(Level::Info, format_args!("UI pipe {} is open", listener.name()));
        Some(listener)
    }

    /// Accepts the connections of `session`'s pipe on a thread of their own. When the pipe fails or the thread cannot
    /// start, the session is forgotten so the next `ensure` makes a new listener.
    fn accept_on(self: &Arc<Self>, session: u32, mut listener: PipeListener) {
        let me = Arc::clone(self);
        let accept = std::thread::Builder::new().name(format!("ui-pipe-{session}")).spawn(move || {
            while !me.stop.load(Ordering::Acquire) {
                match listener.accept(Some(ACCEPT_POLL)) {
                    Ok(conn) => {
                        let worker = Arc::clone(&me);
                        let spawned = std::thread::Builder::new()
                            .name(format!("ui-conn-{session}"))
                            .spawn(move || worker.connection(session, conn));
                        match spawned {
                            Ok(h) => lock(&me.threads).push(h),
                            Err(e) => me.log(Level::Warn, format_args!("cannot start a connection thread: {e}")),
                        }
                    }
                    Err(PipeError::Timeout) => {}
                    Err(e) => {
                        me.log(Level::Error, format_args!("the UI pipe of session {session} failed: {e}"));
                        me.sessions.forget(session);
                        return;
                    }
                }
            }
        });
        match accept {
            Ok(h) => lock(&self.threads).push(h),
            Err(e) => {
                self.log(Level::Error, format_args!("cannot start the UI pipe thread: {e}"));
                self.sessions.forget(session);
            }
        }
    }

    /// One client of the UI pipe of `session`.
    fn connection(self: &Arc<Self>, session: u32, conn: PipeConn) {
        let refuse = |why: &str| {
            self.log(Level::Warn, format_args!("a client of the UI pipe of session {session} was refused: {why}"))
        };
        // 1. The client process runs in this session.
        match conn.client_session_id() {
            Ok(s) if s == session => {}
            Ok(s) => return refuse(&format!("it runs in session {s}")),
            Err(e) => return refuse(&e.to_string()),
        }
        let (Ok(for_identity), Ok(for_cancel), Ok(for_cut)) = (conn.try_clone(), conn.try_clone(), conn.try_clone())
        else {
            return refuse("pipe handle");
        };
        let Ok((mut reader_half, writer_half)) = conn.split() else { return refuse("pipe split") };
        reader_half.set_read_timeout(Some(HELLO_WITHIN));
        let (mut reader, mut writer) = (FrameReader::new(reader_half), FrameWriter::new(writer_half));
        // 2. `Hello` first (the impersonation of the last message read needs a read), role UI or Elevator.
        let hello = match Handshake::server(&mut reader, &mut writer, session) {
            Ok(h) => h,
            Err(e) => return refuse(&format!("handshake: {e}")),
        };
        let bye = |writer: &mut FrameWriter<_>, code| {
            let _ = writer.write_message(&swapdesk_ipc::Message::Bye { code });
        };
        if hello.role != IpcRole::Ui && hello.role != IpcRole::Elevator {
            bye(&mut writer, ErrorCode::NotPermitted);
            return refuse("its role is not UI or Elevator");
        }
        // 3. The token's user is the session's user; 4. admin only for an elevated token with Administrators enabled.
        let who = match for_identity.identify_client() {
            Ok(w) => w,
            Err(e) => {
                bye(&mut writer, ErrorCode::NotPermitted);
                return refuse(&format!("identify: {e}"));
            }
        };
        let expected = match self.mode {
            Mode::Dev => current_user_sid().map_err(|e| e.to_string()),
            Mode::Production => session_user_sid(session),
        };
        match expected {
            Ok(sid) if sid.eq_ignore_ascii_case(&who.user_sid) => {}
            Ok(_) => {
                bye(&mut writer, ErrorCode::NotPermitted);
                return refuse("its user is not the session's user");
            }
            Err(e) => {
                bye(&mut writer, ErrorCode::NotPermitted);
                return refuse(&e);
            }
        }
        // 5. The account of that user: its hub alone (built the first time the user opens Swapdesk).
        let hub = match (self.route)(&who.user_sid) {
            Ok(h) => h,
            Err(e) => {
                bye(&mut writer, ErrorCode::Internal);
                return refuse(&format!("no account: {e}"));
            }
        };
        let name =
            session_user_name(session).ok().flatten().or_else(|| std::env::var("USERNAME").ok()).unwrap_or_default();
        let info = ConnInfo {
            session_id: session,
            user: UserRef { id: who.user_sid.clone(), name },
            admin: who.is_admin(),
            can_elevate: who.administrators_present,
        };
        // Back to blocking reads: the UI is quiet between requests.
        let mut reader_half = reader.into_inner();
        reader_half.set_read_timeout(None);
        let reader = FrameReader::new(reader_half);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.open).push((id, for_cancel));
        // Cancelling the pipe's I/O unblocks the thread that reads it. A UI that stops reading its messages is cut by the
        // hub this way: `serve` ends and the pipe closes, so the UI sees the end of the pipe and reconnects. The
        // elevator loop does it when it ends, so its reading thread ends with it.
        let for_cut = Mutex::new(for_cut);
        let cancel: Arc<dyn Fn() + Send + Sync> = Arc::new(move || lock(&for_cut).cancel_io());
        let (what, end) = if hello.role == IpcRole::Elevator {
            ("an elevator", hub.serve_elevator_cancellable(reader, writer, info, cancel))
        } else {
            ("a UI", hub.serve_cancellable(reader, writer, info, cancel))
        };
        lock(&self.open).retain(|(i, _)| *i != id);
        if let Err(e) = end {
            self.log(Level::Debug, format_args!("{what} connection ended: {e}"));
        }
    }

    /// The service stops: unblocks every connection and waits for the threads.
    pub fn stop_all(&self) {
        self.stop.store(true, Ordering::Release);
        for (_, c) in lock(&self.open).iter() {
            c.cancel_io();
        }
        let threads: Vec<_> = lock(&self.threads).drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU32;

    use swapdesk_capture_win::QpcClock;
    use swapdesk_host::ui_hub::{DevOptions, HubParams, MachineSettings, NoHooks, NoPresence};
    use swapdesk_ipc::{ApiCode, ElevAction, ElevationTicket, Message, ReplyBody, UiOp};
    use swapdesk_platform::{NetInterface, NetInterfaces, SecretStore};
    use swapdesk_proto::Os;
    use swapdesk_sys_win::{ClientIdentity, PipeReader, PipeWriter, connect, session_of_pid};
    use swapdesk_trust::{Identity, MemoryAudit, SystemWallClock, TrustHandle};

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    /// The SID lookup of a session, scripted: it fails until the user has logged on.
    struct Sids {
        logged_on: Cell<bool>,
        asked: Cell<u32>,
    }

    impl Sids {
        fn lookup(&self, session: u32) -> Result<String, String> {
            self.asked.set(self.asked.get() + 1);
            if self.logged_on.get() {
                Ok(format!("S-1-5-21-{session}"))
            } else {
                Err(format!("no user token for session {session}"))
            }
        }
    }

    // ---------- a development-shape pipe on this PC ----------

    struct MemorySecrets(Mutex<HashMap<String, Vec<u8>>>);

    impl SecretStore for MemorySecrets {
        fn load(&self, name: &str) -> swapdesk_platform::Result<Option<Vec<u8>>> {
            Ok(lock(&self.0).get(name).cloned())
        }
        fn store(&self, name: &str, bytes: &[u8]) -> swapdesk_platform::Result<()> {
            lock(&self.0).insert(name.to_string(), bytes.to_vec());
            Ok(())
        }
        fn delete(&self, name: &str) -> swapdesk_platform::Result<()> {
            lock(&self.0).remove(name);
            Ok(())
        }
    }

    struct NoInterfaces;

    impl NetInterfaces for NoInterfaces {
        fn list(&self) -> swapdesk_platform::Result<Vec<NetInterface>> {
            Ok(Vec::new())
        }
    }

    /// Keeps what the pipes log, so a refusal can be told from a silence.
    #[derive(Default)]
    struct Lines(Mutex<Vec<String>>);

    impl TraceSink for Lines {
        fn log(&self, _level: Level, _target: &'static str, what: core::fmt::Arguments<'_>) {
            lock(&self.0).push(what.to_string());
        }
    }

    impl Lines {
        fn has(&self, part: &str) -> bool {
            lock(&self.0).iter().any(|l| l.contains(part))
        }
    }

    fn own_pid() -> u32 {
        std::process::id()
    }

    /// A number no other pipe of this process has used.
    fn unique() -> u32 {
        static N: AtomicU32 = AtomicU32::new(0);
        N.fetch_add(1, Ordering::SeqCst)
    }

    fn own_session() -> u32 {
        session_of_pid(own_pid()).expect("this process's session")
    }

    /// A real hub with in-memory stores behind the pipe of this process's own session, in the shape `--console` makes:
    /// the current user owns the pipe and is its one client.
    struct DevPipe {
        pipes: Arc<UiPipes>,
        hub: UiHub,
        lines: Arc<Lines>,
        name: String,
    }

    impl DevPipe {
        fn new() -> Self {
            let suffix = format!("-t{}n{}", own_pid(), unique());
            let secrets = Arc::new(MemorySecrets(Mutex::new(HashMap::new())));
            let trust = TrustHandle::open(secrets.clone()).expect("trust");
            let lines = Arc::new(Lines::default());
            let hub = UiHub::new(HubParams {
                os: Os::Windows,
                os_version: "Windows".into(),
                identity: Arc::new(Identity::generate(0).expect("identity")),
                trust,
                secrets,
                settings: MachineSettings::new("test pc"),
                interfaces: Arc::new(NoInterfaces),
                presence: Arc::new(NoPresence),
                audit: Arc::new(MemoryAudit::new()),
                clock: Arc::new(QpcClock),
                wall: Arc::new(SystemWallClock),
                trace: lines.clone(),
                hooks: Arc::new(NoHooks),
                dev: Some(DevOptions::default()),
            });
            let session = own_session();
            hub.set_console_session(Some(session));
            let pipes =
                UiPipes::new(hub.clone(), lines.clone(), Mode::Dev, suffix.clone(), Arc::new(AtomicBool::new(false)));
            pipes.ensure(session);
            Self { pipes, hub, lines, name: format!("{}{suffix}", swapdesk_sys_win::ui_pipe_name(session)) }
        }

        fn client(&self, role: IpcRole) -> Client {
            Client::open(&self.name, role)
        }
    }

    impl Drop for DevPipe {
        fn drop(&mut self) {
            self.pipes.stop_all();
            self.hub.shutdown();
        }
    }

    /// The client end: connects as the service's own client does, then says `Hello` as `role`. Reads give up after
    /// [`WAIT`], so a service that stays silent fails the test instead of hanging it.
    struct Client {
        reader: FrameReader<PipeReader>,
        writer: FrameWriter<PipeWriter>,
    }

    impl Client {
        fn open(pipe: &str, role: IpcRole) -> Self {
            let conn = connect(pipe, own_pid(), WAIT).expect("the pipe opens");
            let (mut reader_half, writer_half) = conn.split().expect("split");
            reader_half.set_read_timeout(Some(WAIT));
            let (mut reader, mut writer) = (FrameReader::new(reader_half), FrameWriter::new(writer_half));
            Handshake::client(&mut reader, &mut writer, role, own_pid()).expect("the handshake is answered");
            Self { reader, writer }
        }

        fn send(&mut self, req: u32, op: UiOp) {
            self.writer.write_message(&Message::UiRequest { req, op }).expect("write");
        }

        /// The next message that is not a notification, `None` at the end of the pipe.
        fn next(&mut self) -> Option<Message> {
            loop {
                match self.reader.read_message().expect("a message or the end of the pipe") {
                    Some(Message::UiNotify(_)) => {}
                    other => return other,
                }
            }
        }

        /// The reply to request `req`: `Ok` with its body, or the `ApiCode` it failed with.
        fn reply(&mut self, req: u32, op: u16) -> Result<ReplyBody, ApiCode> {
            match self.next() {
                Some(Message::UiReply { req: r, status, body }) if r == req => {
                    match swapdesk_ipc::status_code(status).expect("a status") {
                        Some(code) => Err(code),
                        None => Ok(ReplyBody::decode(op, &body.0).expect("the body decodes")),
                    }
                }
                other => panic!("not the reply to request {req}: {other:?}"),
            }
        }

        /// The service ends the pipe with `Bye(code)` and nothing after it.
        fn expect_bye_and_end(&mut self, code: ErrorCode) {
            match self.next() {
                Some(Message::Bye { code: got }) => assert_eq!(got, code),
                other => panic!("expected Bye({code:?}), got {other:?}"),
            }
            assert!(self.next().is_none(), "the pipe ends after the Bye");
        }
    }

    /// What the pipe's identification says about this very process, through a pipe of the same shape.
    fn own_identity() -> ClientIdentity {
        use std::io::{Read, Write};
        let name = format!("t-self-{}-{}", own_pid(), unique());
        let spec = PipeSpec { name: name.clone(), acl: PipeAcl::dev().expect("acl"), max_instances: 1 };
        let mut listener = PipeListener::bind(spec).expect("bind");
        let client = std::thread::spawn(move || {
            let mut c = connect(&name, own_pid(), WAIT).expect("connect");
            c.write_all(b"hello").expect("write");
            c
        });
        let mut server = listener.accept(Some(WAIT)).expect("accept");
        server.read_exact(&mut [0u8; 5]).expect("read");
        let who = server.identify_client().expect("identify");
        drop(client.join().expect("client"));
        who
    }

    #[test]
    fn an_elevator_hello_is_served_by_the_restricted_loop_and_another_role_is_refused() {
        let dev = DevPipe::new();
        let me = own_identity();

        // An elevator is answered by the restricted loop: `ElevationDescribe` for a ticket nobody opened is `NeedsAdmin`
        // when this process is not elevated (the approver check comes first), else `NotFound`. A UI connection would be
        // told `NotPermitted` for this op, so the answer shows which loop served it.
        let mut elevator = dev.client(IpcRole::Elevator);
        elevator.send(1, UiOp::ElevationDescribe { ticket: ElevationTicket([7; 16]) });
        let want = if me.is_admin() { ApiCode::NotFound } else { ApiCode::NeedsAdmin };
        assert_eq!(elevator.reply(1, 36).unwrap_err(), want);

        // Anything else is a protocol violation: the loop answers `Bye` and the pipe closes, where the UI loop would
        // answer the op.
        let mut elevator = dev.client(IpcRole::Elevator);
        elevator.send(1, UiOp::GetThisDevice);
        elevator.expect_bye_and_end(ErrorCode::ProtocolViolation);

        // A helper's role on the UI pipe is refused with `Bye(NotPermitted)`, as before.
        for role in [IpcRole::HelperUser, IpcRole::HelperSystem] {
            let mut helper = dev.client(role);
            helper.expect_bye_and_end(ErrorCode::NotPermitted);
        }
        assert!(dev.lines.has("its role is not UI or Elevator"), "the refusal is logged");

        // A UI still goes to the UI loop: the op the elevator loop serves is answered `NotPermitted` there.
        let mut ui = dev.client(IpcRole::Ui);
        ui.send(1, UiOp::ElevationDescribe { ticket: ElevationTicket([7; 16]) });
        assert_eq!(ui.reply(1, 36).unwrap_err(), ApiCode::NotPermitted);
    }

    #[test]
    fn a_ui_connection_may_ask_for_a_ticket_exactly_when_its_token_holds_administrators() {
        let dev = DevPipe::new();
        let me = own_identity();
        // A well-formed device that is not paired: past the `can_elevate` gate the answer is `NotFound`; at the gate
        // it is `NotPermitted`. (A ticket itself needs a paired device, which the hub's own tests make.)
        let action = ElevAction::SetGrants { device_id: "ab".repeat(16), grants: 0 };
        let mut ui = dev.client(IpcRole::Ui);
        ui.send(1, UiOp::ElevationBegin { action });
        let want = if me.administrators_present { ApiCode::NotFound } else { ApiCode::NotPermitted };
        assert_eq!(ui.reply(1, 35).unwrap_err(), want, "administrators present: {}", me.administrators_present);
    }

    #[test]
    fn a_session_whose_sid_lookup_failed_at_the_logon_screen_gets_its_pipe_at_the_next_try() {
        let sessions = OpenSessions::default();
        let sids = Sids { logged_on: Cell::new(false), asked: Cell::new(0) };
        let open = || sids.lookup(2).ok();
        // Boot: the console is the logon screen, nobody has a token yet.
        assert!(sessions.ensure(2, open).is_none());
        // The user logs on: the ConsoleChanged that follows must try again and succeed.
        sids.logged_on.set(true);
        assert_eq!(sessions.ensure(2, open).as_deref(), Some("S-1-5-21-2"));
        assert_eq!(sids.asked.get(), 2);
        // Served now: no second listener for the same session.
        assert!(sessions.ensure(2, open).is_none());
        assert_eq!(sids.asked.get(), 2, "a served session is not looked up again");
    }

    #[test]
    fn a_listener_that_ended_is_made_again_and_sessions_are_independent() {
        let sessions = OpenSessions::default();
        assert_eq!(sessions.ensure(1, || Some("pipe-1")), Some("pipe-1"));
        assert_eq!(sessions.ensure(3, || Some("pipe-3")), Some("pipe-3"));
        assert_eq!(sessions.ensure(1, || Some("again")), None);
        sessions.forget(1);
        assert_eq!(sessions.ensure(1, || Some("pipe-1b")), Some("pipe-1b"));
        assert_eq!(sessions.ensure(3, || Some("again")), None, "forgetting one session leaves the other served");
    }
}
