//! Terminals in the hub (stage 43): the UI ops `TermOpen`, `TermResize` and `TermClose`, the `TermInput` message, the
//! relay of `TermOutput` and `TermExited` to the UI, and the audit entry of an opened terminal.
//!
//! A terminal is identified on the UI pipe by a connection-wide id the hub picks, so the UI's input and output never
//! name a session; the hub keeps `term_id -> session_id` on the connection and only the connection that opened a
//! terminal may touch it (a number is not a capability on another connection). The administrator terminal needs an
//! administrator connection (C2.3, D10): the UI hint is not enough.

use std::sync::Arc;

use swapdesk_ipc::{ApiCode, Message as Ipc, ReplyBody, TermOutcome, Text};
use swapdesk_trust::{AuditEntry, AuditEvent, DeviceId, DeviceRef};

use super::ops::reply;
use super::{Conn, Hub, lock};

/// Largest terminal grid the protocol takes (`TermOpen` is refused above it, `swapdesk-term`).
const MAX_DIM: u16 = 1000;

fn dims_ok(cols: u16, rows: u16) -> bool {
    (1..=MAX_DIM).contains(&cols) && (1..=MAX_DIM).contains(&rows)
}

impl Hub {
    /// `TermOpen`: replies when the shell runs or was refused (up to the host's 20 s question).
    pub(super) fn term_open(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        req: u32,
        session_id: u64,
        cols: u16,
        rows: u16,
        admin: bool,
    ) {
        if !dims_ok(cols, rows) {
            return reply(conn, req, Err(self.api_error(ApiCode::Invalid, "terminal size")));
        }
        let Some(handle) = lock(&conn.viewers).get(&session_id).map(|v| v.handle.clone()) else {
            return reply(conn, req, Err(ApiCode::NotFound));
        };
        // ELEVATION.md 3.2: the administrator terminal of another machine needs this machine's confirmation: an
        // administrator connection, a session already elevated (confirmed then), or the pass of a confirmation.
        if admin
            && !(conn.info.admin
                || handle.granted().contains(swapdesk_proto::PermissionSet::UNATTENDED)
                || self.take_pass(conn, &swapdesk_ipc::ElevAction::OpenAdminTerminal { session_id }))
        {
            return reply(
                conn,
                req,
                Err(self.api_error(ApiCode::NeedsAdmin, "administrator terminal without this machine's confirmation")),
            );
        }
        let Some(rt) = self.rt().cloned() else { return reply(conn, req, Err(ApiCode::ServiceDown)) };
        let term_id = self.fresh_id();
        let (me, c) = (Arc::clone(self), Arc::clone(conn));
        rt.spawn(async move {
            let body = match handle.term_open(term_id, cols, rows, admin).await {
                Ok(account) => {
                    lock(&c.terms).insert(term_id, session_id);
                    ReplyBody::TermOpened { term_id, outcome: TermOutcome::Opened, account: Text(account) }
                }
                Err(code) => {
                    me.log(
                        swapdesk_platform::Level::Info,
                        format_args!("terminal refused for session {session_id:#x}: {code:?}"),
                    );
                    ReplyBody::TermOpened {
                        term_id: 0,
                        outcome: TermOutcome::of_error(code),
                        account: Text(String::new()),
                    }
                }
            };
            reply(&c, req, Ok(body));
        });
    }

    /// Op 50, "Elevar controle" (ACCOUNTS.md 9.3): with an administrator connection or the pass of its confirmation,
    /// the session asks the host and the reply waits for its answer.
    pub(super) fn elevate_session(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64) {
        let Some(handle) = lock(&conn.viewers).get(&session_id).map(|v| v.handle.clone()) else {
            return reply(conn, req, Err(ApiCode::NotFound));
        };
        if !(conn.info.admin || self.take_pass(conn, &swapdesk_ipc::ElevAction::ElevateSession { session_id })) {
            return reply(
                conn,
                req,
                Err(self.api_error(ApiCode::NeedsAdmin, "elevation without this machine's confirmation")),
            );
        }
        let Some(rt) = self.rt().cloned() else { return reply(conn, req, Err(ApiCode::ServiceDown)) };
        let (me, c) = (Arc::clone(self), Arc::clone(conn));
        rt.spawn(async move {
            let result = match handle.elevate().await {
                Ok(()) => Ok(ReplyBody::Done),
                Err(code) => {
                    me.log(
                        swapdesk_platform::Level::Info,
                        format_args!("elevation of session {session_id:#x} refused: {code:?}"),
                    );
                    Err(match code {
                        swapdesk_proto::ErrorCode::UnsupportedVersion => ApiCode::Invalid,
                        swapdesk_proto::ErrorCode::NotPermitted => ApiCode::NotPermitted,
                        swapdesk_proto::ErrorCode::Timeout => ApiCode::Timeout,
                        _ => ApiCode::Internal,
                    })
                }
            };
            reply(&c, req, result);
        });
    }

    /// The viewer handle of a terminal of this connection.
    fn term_handle(&self, conn: &Conn, term_id: u32) -> Option<crate::service::ViewerHandle> {
        let session_id = *lock(&conn.terms).get(&term_id)?;
        lock(&conn.viewers).get(&session_id).map(|v| v.handle.clone())
    }

    pub(super) fn term_resize(&self, conn: &Conn, term_id: u32, cols: u16, rows: u16) -> Result<(), ApiCode> {
        if !dims_ok(cols, rows) {
            return Err(ApiCode::Invalid);
        }
        let handle = self.term_handle(conn, term_id).ok_or(ApiCode::NotFound)?;
        if handle.term_resize(term_id, cols, rows) { Ok(()) } else { Err(ApiCode::NotFound) }
    }

    pub(super) fn term_close(&self, conn: &Conn, term_id: u32) -> Result<(), ApiCode> {
        let handle = self.term_handle(conn, term_id).ok_or(ApiCode::NotFound)?;
        lock(&conn.terms).remove(&term_id);
        handle.term_close(term_id);
        Ok(())
    }

    /// `TermInput` from the UI: the bytes go to the shell; a terminal that is not this connection's drops them.
    pub(super) fn term_input(&self, conn: &Conn, term_id: u32, bytes: Vec<u8>) {
        if let Some(handle) = self.term_handle(conn, term_id) {
            handle.term_input(term_id, bytes);
        }
    }

    /// What the UI hears of a terminal ending: ordered after its last output, and the id forgotten.
    pub(super) async fn term_exited(&self, conn: &Conn, term_id: u32, code: Option<i32>) -> bool {
        lock(&conn.terms).remove(&term_id);
        conn.queue.term(Ipc::UiNotify(swapdesk_ipc::Notify::TermExited { term_id, exit_code: code })).await
    }

    /// The audit entry of a terminal the peer `device` opened on this machine (F-SESS-04).
    pub(super) fn terminal_opened(&self, device: DeviceId, admin: bool) {
        self.p.audit.append(AuditEntry {
            time_ms: self.p.wall.unix_ms(),
            device: DeviceRef::Known(device),
            event: AuditEvent::TerminalOpened { admin },
            duration_s: None,
        });
    }
}
