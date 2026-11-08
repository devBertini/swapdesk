//! The UI pipe of one interactive session (C2.1, C2.3): `\\.\pipe\swapdesk\ui-<session>[-<instance>]`, created with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` (a squatted name is logged as an alert and no UI pipe exists for that session),
//! one thread accepting, one thread per connection.
//!
//! A connection is checked in this order, and refused with `Bye` at the first failure: its process is in the pipe's
//! session; its first message is `Hello` with role UI; its impersonation token's user is the session's user; the
//! connection is **admin** only when that token is elevated with Administrators enabled.

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

/// What the accept loops share.
pub struct UiPipes {
    pub hub: UiHub,
    pub trace: Arc<dyn TraceSink>,
    pub mode: Mode,
    /// `-<n>` with `--instance`, else empty.
    pub suffix: String,
    pub stop: Arc<AtomicBool>,
    /// A second owner of every open connection's pipe, to cancel its blocked read at shutdown.
    open: Mutex<Vec<(u64, PipeConn)>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    sessions: Mutex<Vec<u32>>,
    next: std::sync::atomic::AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl UiPipes {
    pub fn new(hub: UiHub, trace: Arc<dyn TraceSink>, mode: Mode, suffix: String, stop: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            hub,
            trace,
            mode,
            suffix,
            stop,
            open: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            sessions: Mutex::new(Vec::new()),
            next: std::sync::atomic::AtomicU64::new(1),
        })
    }

    fn log(&self, level: Level, what: core::fmt::Arguments<'_>) {
        self.trace.log(level, "ui-pipe", what);
    }

    /// Makes sure `ui-<session>` has a listener (once per session id).
    pub fn ensure(self: &Arc<Self>, session: u32) {
        {
            let mut seen = lock(&self.sessions);
            if seen.contains(&session) {
                return;
            }
            seen.push(session);
        }
        // The user the pipe is for.
        let acl = match self.mode {
            Mode::Dev => PipeAcl::dev(),
            Mode::Production => match session_user_sid(session) {
                Ok(sid) => Ok(PipeAcl::system_and_user(sid)),
                Err(e) => {
                    self.log(Level::Error, format_args!("alert: no UI pipe for session {session}: {e}"));
                    return;
                }
            },
        };
        let acl = match acl {
            Ok(a) => a,
            Err(e) => {
                self.log(Level::Error, format_args!("no UI pipe for session {session}: {e}"));
                return;
            }
        };
        let mut spec = PipeSpec::ui(session, acl);
        spec.name.push_str(&self.suffix);
        let mut listener = match PipeListener::bind(spec) {
            Ok(l) => l,
            Err(PipeError::Squatted { name }) => {
                self.log(Level::Error, format_args!("alert: the UI pipe name {name} is already owned by another process; no UI pipe for session {session}"));
                return;
            }
            Err(e) => {
                self.log(Level::Error, format_args!("cannot create the UI pipe of session {session}: {e}"));
                return;
            }
        };
        self.log(Level::Info, format_args!("UI pipe {} is open", listener.name()));
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
                        return;
                    }
                }
            }
        });
        match accept {
            Ok(h) => lock(&self.threads).push(h),
            Err(e) => self.log(Level::Error, format_args!("cannot start the UI pipe thread: {e}")),
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
        let (Ok(for_identity), Ok(for_cancel)) = (conn.try_clone(), conn.try_clone()) else {
            return refuse("pipe handle");
        };
        let Ok((mut reader_half, writer_half)) = conn.split() else { return refuse("pipe split") };
        reader_half.set_read_timeout(Some(HELLO_WITHIN));
        let (mut reader, mut writer) = (FrameReader::new(reader_half), FrameWriter::new(writer_half));
        // 2. `Hello` first (the impersonation of the last message read needs a read), role UI.
        let hello = match Handshake::server(&mut reader, &mut writer, session) {
            Ok(h) => h,
            Err(e) => return refuse(&format!("handshake: {e}")),
        };
        let bye = |writer: &mut FrameWriter<_>, code| {
            let _ = writer.write_message(&swapdesk_ipc::Message::Bye { code });
        };
        if hello.role != IpcRole::Ui {
            bye(&mut writer, ErrorCode::NotPermitted);
            return refuse("its role is not UI");
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
        let name =
            session_user_name(session).ok().flatten().or_else(|| std::env::var("USERNAME").ok()).unwrap_or_default();
        let info =
            ConnInfo { session_id: session, user: UserRef { id: who.user_sid.clone(), name }, admin: who.is_admin() };
        // Back to blocking reads: the UI is quiet between requests.
        let mut reader_half = reader.into_inner();
        reader_half.set_read_timeout(None);
        let reader = FrameReader::new(reader_half);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.open).push((id, for_cancel));
        let end = self.hub.serve(reader, writer, info);
        lock(&self.open).retain(|(i, _)| *i != id);
        if let Err(e) = end {
            self.log(Level::Debug, format_args!("a UI connection ended: {e}"));
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
