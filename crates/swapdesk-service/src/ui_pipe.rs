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

/// How long a re-bind waits for the old pipe's last instances to go (its accept thread looks at its close flag every
/// [`ACCEPT_POLL`], and its connections end when cancelled).
const REBIND_WITHIN: Duration = Duration::from_secs(3);
/// The owner key of the elevator pipe: its DACL names the machine's Administrators, no user, so it never goes stale.
const ELEVATOR_OWNER: &str = "administrators";

/// Who is logged on in a session: the SID, or why there is nobody (production: [`session_user_sid`]; `--console`: the
/// current user; the tests script it to play a session id that changes hands).
type SidOf = Arc<dyn Fn(u32) -> Result<String, String> + Send + Sync>;

/// What the accept loops share.
/// The two pipes of a session: the UI's, and the elevator's (ACCOUNTS.md S7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pipe {
    Ui,
    Elevator,
}

pub struct UiPipes {
    route: Route,
    sid_of: SidOf,
    pub trace: Arc<dyn TraceSink>,
    pub mode: Mode,
    /// `-<n>` with `--instance`, else empty.
    pub suffix: String,
    pub stop: Arc<AtomicBool>,
    /// A second owner of every open connection's pipe, to cancel its blocked read at shutdown or when its session's
    /// user changes.
    open: Mutex<Vec<OpenConn>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    sessions: OpenSessions,
    /// The sessions whose elevator pipe is open.
    elev_sessions: OpenSessions,
    next: std::sync::atomic::AtomicU64,
}

/// One connection that is being served.
struct OpenConn {
    id: u64,
    session: u32,
    /// An interface (role UI); an elevator is not one.
    ui: bool,
    conn: PipeConn,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The sessions that have a bound listener, and the user each was bound for.
#[derive(Default)]
struct OpenSessions(Mutex<Vec<Served>>);

struct Served {
    session: u32,
    /// The SID in the pipe's DACL (or [`ELEVATOR_OWNER`]).
    owner: String,
    /// Set to end the listener's accept thread, which drops the listener and with it the name.
    close: Arc<AtomicBool>,
}

impl OpenSessions {
    /// Runs `open` unless `session` is served already, and counts the session as served (for `owner`) only when `open`
    /// returned `Some`: a session whose pipe could not be made (a squatted name) is tried again by the next call. The
    /// lock is held across `open`, so two callers never bind one name. Returns what `open` made and the flag that
    /// ends its listener.
    fn ensure<T>(&self, session: u32, owner: &str, open: impl FnOnce() -> Option<T>) -> Option<(T, Arc<AtomicBool>)> {
        let mut served = lock(&self.0);
        if served.iter().any(|e| e.session == session) {
            return None;
        }
        let opened = open()?;
        let close = Arc::new(AtomicBool::new(false));
        served.push(Served { session, owner: owner.to_string(), close: Arc::clone(&close) });
        Some((opened, close))
    }

    /// The session's listener was bound for another user than `now` (`None`: nobody is logged on): ends it and counts
    /// the session as not served, so the next `ensure` binds for the user who is there. `true` when it did.
    fn retire_unless(&self, session: u32, now: Option<&str>) -> bool {
        let mut served = lock(&self.0);
        let Some(at) = served.iter().position(|e| e.session == session) else { return false };
        if now.is_some_and(|n| n.eq_ignore_ascii_case(&served[at].owner)) {
            return false;
        }
        served.remove(at).close.store(true, Ordering::Release);
        true
    }

    /// The listener that was given `close` ended on its own: the next `ensure` opens a new one. A listener made since
    /// for the same session is not touched.
    fn forget(&self, session: u32, close: &Arc<AtomicBool>) {
        lock(&self.0).retain(|e| !(e.session == session && Arc::ptr_eq(&e.close, close)));
    }

    fn sessions(&self) -> Vec<u32> {
        lock(&self.0).iter().map(|e| e.session).collect()
    }

    /// The user the session's listener is bound for.
    #[cfg(test)]
    fn owner_of(&self, session: u32) -> Option<String> {
        lock(&self.0).iter().find(|e| e.session == session).map(|e| e.owner.clone())
    }
}

/// The DACL of a UI pipe for `user`: SYSTEM owns it in production, the current user under `--console`.
fn ui_acl(mode: Mode, user: &str) -> PipeAcl {
    match mode {
        Mode::Dev => PipeAcl { owner: swapdesk_sys_win::PipeOwner::CurrentUser, user_sid: Some(user.to_string()) },
        Mode::Production => PipeAcl::system_and_user(user),
    }
}

impl UiPipes {
    /// Every client reaches `hub` (one account: the tests).
    #[cfg(test)]
    pub fn new(hub: UiHub, trace: Arc<dyn TraceSink>, mode: Mode, suffix: String, stop: Arc<AtomicBool>) -> Arc<Self> {
        Self::routed(Arc::new(move |_sid: &str| Ok(hub.clone())), trace, mode, suffix, stop)
    }

    /// Like [`new`](Self::new), with the session's user scripted.
    #[cfg(test)]
    fn new_with_sids(
        hub: UiHub,
        trace: Arc<dyn TraceSink>,
        mode: Mode,
        suffix: String,
        stop: Arc<AtomicBool>,
        sid_of: SidOf,
    ) -> Arc<Self> {
        Self::build(Arc::new(move |_sid: &str| Ok(hub.clone())), trace, mode, suffix, stop, sid_of)
    }

    /// A client reaches the hub `route` gives for its SID.
    pub fn routed(
        route: Route,
        trace: Arc<dyn TraceSink>,
        mode: Mode,
        suffix: String,
        stop: Arc<AtomicBool>,
    ) -> Arc<Self> {
        let sid_of: SidOf = match mode {
            Mode::Production => Arc::new(session_user_sid),
            Mode::Dev => Arc::new(|_| current_user_sid().map_err(|e| e.to_string())),
        };
        Self::build(route, trace, mode, suffix, stop, sid_of)
    }

    fn build(
        route: Route,
        trace: Arc<dyn TraceSink>,
        mode: Mode,
        suffix: String,
        stop: Arc<AtomicBool>,
        sid_of: SidOf,
    ) -> Arc<Self> {
        Arc::new(Self {
            route,
            sid_of,
            trace,
            mode,
            suffix,
            stop,
            open: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            sessions: OpenSessions::default(),
            elev_sessions: OpenSessions::default(),
            next: std::sync::atomic::AtomicU64::new(1),
        })
    }

    fn log(&self, level: Level, what: core::fmt::Arguments<'_>) {
        self.trace.log(level, "ui-pipe", what);
    }

    /// Makes sure `ui-<session>` has a listener bound for the user who is at the session now. A session counts as
    /// served only once its listener is bound: a failed attempt (no user token yet at the logon screen, a squatted
    /// name) is tried again at the next call, which is the next console change or logon of that session. A listener
    /// bound for another user (the session id changed hands: DACL of user A, user B at the session) is closed, its
    /// connections are dropped, and a new one is bound for the user who is there.
    pub fn ensure(self: &Arc<Self>, session: u32) {
        let user = (self.sid_of)(session);
        let rebinding = self.retire_if_user_changed(session, user.as_deref().ok());
        match &user {
            Ok(user) => {
                let patience = if rebinding { REBIND_WITHIN } else { Duration::ZERO };
                let opened = self.sessions.ensure(session, user, || self.bind(session, Pipe::Ui, user, patience));
                if let Some((listener, close)) = opened {
                    self.accept_on(session, listener, Pipe::Ui, close);
                }
            }
            Err(e) => self.log(Level::Error, format_args!("alert: no UI pipe for session {session}: {e}")),
        }
        // ACCOUNTS.md S7: beside it, the pipe where an administrator of the session confirms over its user's shoulder.
        // Its DACL names Administrators, not a user, so a change of user needs no new bind (the connections were
        // dropped above, and each new one looks up the user at its session).
        let opened = self
            .elev_sessions
            .ensure(session, ELEVATOR_OWNER, || self.bind(session, Pipe::Elevator, "", Duration::ZERO));
        if let Some((listener, close)) = opened {
            self.accept_on(session, listener, Pipe::Elevator, close);
        }
    }

    /// A logon, a logoff or a console change was seen: every session served is looked at again, so a session whose
    /// user left or changed loses its pipe and the connections of that user, and `console` is served.
    pub fn sync(self: &Arc<Self>, console: Option<u32>) {
        for session in self.sessions.sessions() {
            if Some(session) == console {
                continue;
            }
            match (self.sid_of)(session) {
                Ok(_) => self.ensure(session),
                Err(_) => {
                    self.retire_if_user_changed(session, None);
                }
            }
        }
        if let Some(session) = console {
            self.ensure(session);
        }
    }

    /// Ends the UI listener of `session` when it was bound for another user than `now`, and drops every connection of
    /// the session (a client of the user who left, an elevator that answers for that user's account). `true` when it
    /// did.
    fn retire_if_user_changed(&self, session: u32, now: Option<&str>) -> bool {
        if !self.sessions.retire_unless(session, now) {
            return false;
        }
        self.log(Level::Info, format_args!("the user of session {session} changed or left: its pipes are closed"));
        for c in lock(&self.open).iter().filter(|c| c.session == session) {
            c.conn.cancel_io();
        }
        true
    }

    /// Creates the listener of `session`'s pipe, bound for `user` (the UI pipe) with the ACL its mode calls for;
    /// `None` (logged) when it cannot. A name still held by the instances of a listener that just ended is waited for
    /// up to `patience`.
    fn bind(&self, session: u32, pipe: Pipe, user: &str, patience: Duration) -> Option<PipeListener> {
        let acl = match (pipe, self.mode) {
            (Pipe::Elevator, Mode::Dev) => PipeAcl::dev_and_administrators(),
            (Pipe::Elevator, Mode::Production) => PipeAcl::system_and_administrators(),
            (Pipe::Ui, mode) => ui_acl(mode, user),
        };
        let mut spec = match pipe {
            Pipe::Ui => PipeSpec::ui(session, acl),
            Pipe::Elevator => PipeSpec::elevator(session, acl),
        };
        spec.name.push_str(&self.suffix);
        let until = std::time::Instant::now() + patience;
        let listener = loop {
            match PipeListener::bind(spec.clone()) {
                Ok(l) => break l,
                Err(PipeError::Squatted { .. }) if std::time::Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(PipeError::Squatted { name }) => {
                    self.log(Level::Error, format_args!("alert: the UI pipe name {name} is already owned by another process; no UI pipe for session {session}"));
                    return None;
                }
                Err(e) => {
                    self.log(Level::Error, format_args!("cannot create the UI pipe of session {session}: {e}"));
                    return None;
                }
            }
        };
        self.log(Level::Info, format_args!("UI pipe {} is open", listener.name()));
        Some(listener)
    }

    /// Accepts the connections of `session`'s pipe on a thread of their own, until `close` is set. When the pipe fails
    /// or the thread cannot start, the session is forgotten so the next `ensure` makes a new listener.
    fn accept_on(self: &Arc<Self>, session: u32, mut listener: PipeListener, pipe: Pipe, close: Arc<AtomicBool>) {
        let me = Arc::clone(self);
        let on_thread = Arc::clone(&close);
        let accept = std::thread::Builder::new().name(format!("ui-pipe-{session}")).spawn(move || {
            while !me.stop.load(Ordering::Acquire) && !on_thread.load(Ordering::Acquire) {
                match listener.accept(Some(ACCEPT_POLL)) {
                    Ok(conn) => {
                        let worker = Arc::clone(&me);
                        let spawned = std::thread::Builder::new()
                            .name(format!("ui-conn-{session}"))
                            .spawn(move || worker.connection(session, conn, pipe));
                        match spawned {
                            Ok(h) => lock(&me.threads).push(h),
                            Err(e) => me.log(Level::Warn, format_args!("cannot start a connection thread: {e}")),
                        }
                    }
                    Err(PipeError::Timeout) => {}
                    Err(e) => {
                        me.log(Level::Error, format_args!("the {pipe:?} pipe of session {session} failed: {e}"));
                        me.sessions_of(pipe).forget(session, &on_thread);
                        return;
                    }
                }
            }
        });
        match accept {
            Ok(h) => lock(&self.threads).push(h),
            Err(e) => {
                self.log(Level::Error, format_args!("cannot start the UI pipe thread: {e}"));
                self.sessions_of(pipe).forget(session, &close);
            }
        }
    }

    fn sessions_of(&self, pipe: Pipe) -> &OpenSessions {
        match pipe {
            Pipe::Ui => &self.sessions,
            Pipe::Elevator => &self.elev_sessions,
        }
    }

    /// One client of the UI pipe of `session`, or of its elevator pipe.
    fn connection(self: &Arc<Self>, session: u32, conn: PipeConn, pipe: Pipe) {
        if pipe == Pipe::Elevator {
            return self.elevator_connection(session, conn);
        }
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
        match (self.sid_of)(session) {
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
        lock(&self.open).push(OpenConn { id, session, ui: hello.role == IpcRole::Ui, conn: for_cancel });
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
        lock(&self.open).retain(|c| c.id != id);
        if let Err(e) = end {
            self.log(Level::Debug, format_args!("{what} connection ended: {e}"));
        }
    }

    /// One client of `elev-<session>` (ACCOUNTS.md S7, ELEVATION.md 3.3): role `Elevator` only, a process of that
    /// session whose token is elevated with Administrators enabled, of any user; it answers for the tickets of the
    /// account whose user is at that session, in that account's hub.
    fn elevator_connection(self: &Arc<Self>, session: u32, conn: PipeConn) {
        let refuse = |why: &str| {
            self.log(Level::Warn, format_args!("a client of the elevator pipe of session {session} was refused: {why}"))
        };
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
        let hello = match Handshake::server(&mut reader, &mut writer, session) {
            Ok(h) => h,
            Err(e) => return refuse(&format!("handshake: {e}")),
        };
        let bye = |writer: &mut FrameWriter<_>, code| {
            let _ = writer.write_message(&swapdesk_ipc::Message::Bye { code });
        };
        if hello.role != IpcRole::Elevator {
            bye(&mut writer, ErrorCode::NotPermitted);
            return refuse("its role is not Elevator");
        }
        let who = match for_identity.identify_client() {
            Ok(w) if w.is_admin() => w,
            Ok(_) => {
                bye(&mut writer, ErrorCode::NotPermitted);
                return refuse("its token is not an elevated administrator's");
            }
            Err(e) => {
                bye(&mut writer, ErrorCode::NotPermitted);
                return refuse(&format!("identify: {e}"));
            }
        };
        // The tickets of this session live in the hub of the account whose user is at it.
        let hub = match (self.sid_of)(session).and_then(|sid| (self.route)(&sid)) {
            Ok(h) => h,
            Err(e) => {
                bye(&mut writer, ErrorCode::NotPermitted);
                return refuse(&format!("no account at the session: {e}"));
            }
        };
        let info = ConnInfo {
            session_id: session,
            user: UserRef { id: who.user_sid.clone(), name: String::new() },
            admin: true,
            can_elevate: true,
        };
        let mut reader_half = reader.into_inner();
        reader_half.set_read_timeout(None);
        let reader = FrameReader::new(reader_half);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.open).push(OpenConn { id, session, ui: false, conn: for_cancel });
        let for_cut = Mutex::new(for_cut);
        let cancel: Arc<dyn Fn() + Send + Sync> = Arc::new(move || lock(&for_cut).cancel_io());
        let end = hub.serve_elevator_cancellable(reader, writer, info, cancel);
        lock(&self.open).retain(|c| c.id != id);
        if let Err(e) = end {
            self.log(Level::Debug, format_args!("an elevator connection ended: {e}"));
        }
    }

    /// The Windows sessions that have an interface connected (an update reopens it there, UPDATES.md 6.2 step 7).
    pub fn sessions_with_ui(&self) -> Vec<u32> {
        let mut v: Vec<u32> = lock(&self.open).iter().filter(|c| c.ui).map(|c| c.session).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The service stops: unblocks every connection and waits for the threads.
    pub fn stop_all(&self) {
        self.stop.store(true, Ordering::Release);
        for c in lock(&self.open).iter() {
            c.conn.cancel_io();
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

    /// A real hub with in-memory stores.
    fn test_hub(lines: &Arc<Lines>) -> UiHub {
        let secrets = Arc::new(MemorySecrets(Mutex::new(HashMap::new())));
        let trust = TrustHandle::open(secrets.clone()).expect("trust");
        UiHub::new(HubParams {
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
        })
    }

    impl DevPipe {
        fn new() -> Self {
            let suffix = format!("-t{}n{}", own_pid(), unique());
            let lines = Arc::new(Lines::default());
            let hub = test_hub(&lines);
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

        /// A client of the session's elevator pipe (`elev-<session><suffix>`).
        fn elevator_client(&self, role: IpcRole) -> Client {
            Client::open(&self.name.replacen("ui-", "elev-", 1), role)
        }
    }

    impl Drop for DevPipe {
        fn drop(&mut self) {
            self.pipes.stop_all();
            self.hub.shutdown();
        }
    }

    /// A UI pipe on this PC's own session whose logged-on user is scripted: `None` is the logon screen. It has the
    /// development owner (the current user, who can make the pipe's next instances, which SYSTEM does in production) and
    /// the DACL user row and the session's user check of the scripted user.
    struct SessionPipe {
        pipes: Arc<UiPipes>,
        hub: UiHub,
        lines: Arc<Lines>,
        user: Arc<Mutex<Option<String>>>,
        name: String,
    }

    /// A well-formed SID of a group this process is not in (BUILTIN Guests): "another user".
    const OTHER_USER: &str = "S-1-5-32-546";

    impl SessionPipe {
        fn new() -> Self {
            let suffix = format!("-t{}n{}", own_pid(), unique());
            let lines = Arc::new(Lines::default());
            let hub = test_hub(&lines);
            let session = own_session();
            hub.set_console_session(Some(session));
            let user = Arc::new(Mutex::new(None::<String>));
            let sid_of: SidOf = {
                let user = Arc::clone(&user);
                Arc::new(move |s| lock(&user).clone().ok_or_else(|| format!("no user token for session {s}")))
            };
            let pipes = UiPipes::new_with_sids(
                hub.clone(),
                lines.clone(),
                Mode::Dev,
                suffix.clone(),
                Arc::new(AtomicBool::new(false)),
                sid_of,
            );
            Self { pipes, hub, lines, user, name: format!("{}{suffix}", swapdesk_sys_win::ui_pipe_name(session)) }
        }

        /// `who` logs on at the session (`None`: the session has no user), then the service sees the change.
        fn logon(&self, who: Option<&str>) {
            *lock(&self.user) = who.map(str::to_string);
            self.pipes.sync(Some(own_session()));
        }

        /// Whether a client can open the pipe now (its DACL names this process's user).
        fn opens(&self) -> bool {
            connect(&self.name, own_pid(), Duration::from_millis(300)).is_ok()
        }

        /// The pipe is gone (or refuses this user) within a few seconds.
        fn closes_soon(&self) -> bool {
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < until {
                if !self.opens() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        }
    }

    impl Drop for SessionPipe {
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

        /// The service dropped this client: its pipe ends (after a `Bye` or a cancelled read), nothing else comes.
        fn assert_ended(&mut self) {
            loop {
                match self.reader.read_message() {
                    Ok(Some(Message::UiNotify(_) | Message::Bye { .. })) => {}
                    Ok(Some(other)) => panic!("unexpected {other:?} on a dropped client"),
                    Err(swapdesk_ipc::IpcError::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                        panic!("the client was not dropped: its read timed out")
                    }
                    Ok(None) | Err(_) => return,
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
    fn the_sessions_with_an_interface_are_listed_for_the_update_and_an_elevator_is_not() {
        let dev = DevPipe::new();
        assert!(dev.pipes.sessions_with_ui().is_empty());
        let mut elevator = dev.client(IpcRole::Elevator);
        elevator.send(1, UiOp::ElevationDescribe { ticket: ElevationTicket([7; 16]) });
        let _ = elevator.reply(1, 36);
        assert!(dev.pipes.sessions_with_ui().is_empty(), "an elevator is not an interface");
        let mut ui = dev.client(IpcRole::Ui);
        ui.send(1, UiOp::GetThisDevice);
        let _ = ui.reply(1, 1);
        assert_eq!(dev.pipes.sessions_with_ui(), vec![own_session()]);
    }

    #[test]
    fn a_ui_connection_of_any_account_may_ask_for_a_ticket() {
        let dev = DevPipe::new();
        let me = own_identity();
        // ACCOUNTS.md S7: every account may ask; a well-formed device that is not paired is `NotFound`. (A ticket
        // itself needs a paired device, which the hub's own tests make.)
        let action = ElevAction::SetGrants { device_id: "ab".repeat(16), grants: 0 };
        let mut ui = dev.client(IpcRole::Ui);
        ui.send(1, UiOp::ElevationBegin { action });
        assert_eq!(
            ui.reply(1, 35).unwrap_err(),
            ApiCode::NotFound,
            "administrators present: {}",
            me.administrators_present
        );
    }

    #[test]
    fn a_session_whose_sid_lookup_failed_at_the_logon_screen_gets_its_pipe_at_the_next_try() {
        let sessions = OpenSessions::default();
        let sids = Sids { logged_on: Cell::new(false), asked: Cell::new(0) };
        let open = || sids.lookup(2).ok();
        // Boot: the console is the logon screen, nobody has a token yet.
        assert!(sessions.ensure(2, "nobody", open).is_none());
        // The user logs on: the ConsoleChanged that follows must try again and succeed.
        sids.logged_on.set(true);
        assert_eq!(sessions.ensure(2, "S-1-5-21-2", open).map(|(sid, _)| sid).as_deref(), Some("S-1-5-21-2"));
        assert_eq!(sids.asked.get(), 2);
        // Served now: no second listener for the same session.
        assert!(sessions.ensure(2, "S-1-5-21-2", open).is_none());
        assert_eq!(sids.asked.get(), 2, "a served session is not looked up again");
    }

    /// Security finding of 13/11: a session id that changes hands. User A held session 2, then signed out; user B got
    /// session 2. The pipe's DACL names the user it was bound for, so without a new bind B's `Swapdesk.exe` gets
    /// ACCESS_DENIED until the service restarts. After the change event the listener is bound for B, A's listener
    /// is closed (its accept thread ends and drops the name, or the new bind could not take it), and A's connected
    /// client is dropped.
    #[test]
    fn a_session_id_reused_by_another_user_gets_a_pipe_for_that_user() {
        let me = current_user_sid().expect("this process's user");
        let session = own_session();
        let pipe = SessionPipe::new();
        // A holds the session.
        pipe.logon(Some(OTHER_USER));
        assert_eq!(pipe.pipes.sessions.owner_of(session).as_deref(), Some(OTHER_USER));
        // A signs out and B logs on at the same session id: the change event must leave a listener that is B's.
        pipe.logon(Some(&me));
        assert_eq!(pipe.pipes.sessions.owner_of(session).as_deref(), Some(me.as_str()), "the pipe is bound for B");
        let mut ui = Client::open(&pipe.name, IpcRole::Ui);
        ui.send(1, UiOp::GetThisDevice);
        assert!(ui.reply(1, 1).is_ok(), "B's interface is served");
        assert!(pipe.lines.has("UI pipe") && !pipe.lines.has("already owned"), "the new bind took the name");
        // The session goes to A again: B's client is dropped and the pipe is A's.
        pipe.logon(Some(OTHER_USER));
        ui.assert_ended();
        assert_eq!(pipe.pipes.sessions.owner_of(session).as_deref(), Some(OTHER_USER));
        assert!(pipe.pipes.sessions_with_ui().is_empty());
        // The elevator pipe names Administrators, not a user: it stays the one listener it was.
        assert_eq!(pipe.pipes.elev_sessions.owner_of(session).as_deref(), Some(ELEVATOR_OWNER));
    }

    /// A signed-out session leaves no listener and no connection of its user: the connected client is dropped, and the
    /// same user logging on again gets a pipe again.
    #[test]
    fn a_logoff_closes_the_pipe_and_drops_the_connected_client() {
        let me = current_user_sid().expect("this process's user");
        let pipe = SessionPipe::new();
        pipe.logon(Some(&me));
        let mut ui = Client::open(&pipe.name, IpcRole::Ui);
        ui.send(1, UiOp::GetThisDevice);
        assert!(ui.reply(1, 1).is_ok());
        assert_eq!(pipe.pipes.sessions_with_ui(), vec![own_session()]);

        pipe.logon(None);
        ui.assert_ended();
        assert!(pipe.closes_soon(), "no listener is left for a session without a user");
        assert!(pipe.pipes.sessions_with_ui().is_empty());
        assert_eq!(pipe.pipes.sessions.owner_of(own_session()), None, "the session is forgotten");

        pipe.logon(Some(&me));
        assert!(pipe.opens(), "the user logging on again gets a pipe");
    }

    /// The change event of a logon or logoff in a session that is not the console's still reaches that session: it is
    /// served by `sync`, which looks at every session that has a listener.
    #[test]
    fn a_change_event_looks_at_the_sessions_that_are_not_the_console() {
        let me = current_user_sid().expect("this process's user");
        let other_session = own_session() + 1000;
        let pipe = SessionPipe::new();
        pipe.logon(Some(OTHER_USER));
        pipe.pipes.ensure(other_session);
        assert_eq!(pipe.pipes.sessions.owner_of(other_session).as_deref(), Some(OTHER_USER));
        pipe.logon(Some(&me));
        assert_eq!(pipe.pipes.sessions.owner_of(other_session).as_deref(), Some(me.as_str()));
        pipe.logon(None);
        assert_eq!(pipe.pipes.sessions.owner_of(other_session), None, "logged off: no listener");
    }

    #[test]
    fn the_ui_pipe_acl_names_the_user_it_is_bound_for() {
        let acl = ui_acl(Mode::Production, OTHER_USER);
        assert_eq!(acl.user_sid.as_deref(), Some(OTHER_USER));
        assert_eq!(acl.owner, swapdesk_sys_win::PipeOwner::System);
        assert_eq!(ui_acl(Mode::Dev, "S-1-1-0").user_sid.as_deref(), Some("S-1-1-0"));
    }

    #[test]
    fn a_listener_that_ended_is_made_again_and_sessions_are_independent() {
        let sessions = OpenSessions::default();
        let (_, one) = sessions.ensure(1, "a", || Some("pipe-1")).expect("opened");
        assert!(sessions.ensure(3, "a", || Some("pipe-3")).is_some());
        assert!(sessions.ensure(1, "a", || Some("again")).is_none());
        // An older listener's end does not forget a newer one's session.
        sessions.forget(1, &Arc::new(AtomicBool::new(false)));
        assert!(sessions.ensure(1, "a", || Some("again")).is_none());
        sessions.forget(1, &one);
        assert!(sessions.ensure(1, "a", || Some("pipe-1b")).is_some());
        assert!(sessions.ensure(3, "a", || Some("again")).is_none(), "forgetting one session leaves the other served");
    }

    #[test]
    fn a_listener_bound_for_another_user_is_retired_and_one_for_the_same_user_is_kept() {
        let sessions = OpenSessions::default();
        let (_, close) = sessions.ensure(2, "S-1-5-21-A", || Some(())).expect("opened");
        assert!(!sessions.retire_unless(2, Some("s-1-5-21-a")), "the same user, whatever the case");
        assert!(!close.load(Ordering::Acquire));
        assert!(sessions.retire_unless(2, Some("S-1-5-21-B")), "another user at the session");
        assert!(close.load(Ordering::Acquire), "the old listener is told to end");
        assert!(sessions.ensure(2, "S-1-5-21-B", || Some(())).is_some(), "the session is served again, for B");
        assert!(sessions.retire_unless(2, None), "nobody at the session");
        assert!(!sessions.retire_unless(2, None), "nothing left to retire");
    }

    /// ACCOUNTS.md S7: `elev-<session>` serves the role `Elevator` only, and only to an elevated administrator's
    /// token; a UI there is refused.
    #[test]
    fn the_elevator_pipe_serves_only_the_elevator_role() {
        let dev = DevPipe::new();
        let me = own_identity();
        let mut ui = dev.elevator_client(IpcRole::Ui);
        ui.expect_bye_and_end(ErrorCode::NotPermitted);
        assert!(dev.lines.has("its role is not Elevator"), "the refusal is logged");
        let mut elevator = dev.elevator_client(IpcRole::Elevator);
        if me.is_admin() {
            // Served by the restricted loop: a ticket nobody opened is not found.
            elevator.send(1, UiOp::ElevationDescribe { ticket: ElevationTicket([7; 16]) });
            assert_eq!(elevator.reply(1, 36).unwrap_err(), ApiCode::NotFound);
        } else {
            elevator.expect_bye_and_end(ErrorCode::NotPermitted);
            assert!(dev.lines.has("its token is not an elevated administrator's"));
        }
    }
}
