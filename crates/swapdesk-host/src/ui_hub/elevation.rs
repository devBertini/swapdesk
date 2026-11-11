//! The administrator confirmation (`docs/ELEVATION.md` 2 and 3.2): a UI that is not elevated cannot grant `UNATTENDED`
//! or `ADMIN_TERMINAL`, so it asks the service for a ticket bound to its own connection and to the exact action, an
//! elevated instance of the same user confirms that ticket, and the service then lets that one action through once
//! on the asking connection (a pass). The asking UI sends its command again and the existing code path finds the pass.
//!
//! * A ticket is 16 random bytes, lives 120 s, belongs to one connection (a new one replaces it) and is never logged;
//!   there are at most 16 on the machine.
//! * A pass lives 30 s on the asking connection, serves one use of one exact action, and no message names it.
//! * The approver is a connection of an administrator token, of the ticket's user and Windows session, reaching the
//!   hub through [`UiHub::describe_elevation`] and [`UiHub::confirm_elevation`], either from [`UiHub::serve_elevator`]
//!   (a pipe) or in process.
//! * Both lifetimes are read on the hub's clock; entries are swept on every elevation op and pass lookup.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use swapdesk_ipc::{
    ApiCode, ElevAction, ElevationOutcome, ElevationTicket, FrameReader, FrameWriter, IpcError, Message as Ipc,
    ReplyBody, UiOp,
};
use swapdesk_platform::{Level, MonoMicros};
use swapdesk_proto::{ErrorCode, PermissionSet};
use swapdesk_session::random_token;
use swapdesk_trust::device_id_hex;

use super::ops::reply_message;
use super::views::parse_device_id;
use super::{Conn, ConnInfo, Hub, UiHub, lock};

/// How long a ticket stays usable after `ElevationBegin`.
const TICKET_TTL: Duration = Duration::from_secs(120);
/// How long a pass stays usable after the confirmation.
const PASS_TTL: Duration = Duration::from_secs(30);
/// How long an elevator connection may stay: the ticket's life plus a margin.
const ELEVATOR_LIFETIME: Duration = Duration::from_secs(125);
/// Open tickets on the machine, all connections together.
const MAX_TICKETS: usize = 16;
/// Unused passes one connection holds; a fifth pushes the oldest out.
const MAX_PASSES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicketState {
    Open,
    Confirmed,
    Declined,
}

/// One ticket: what it is for, who asked and who may approve it.
struct TicketEntry {
    ticket: [u8; 16],
    /// The asking connection.
    conn: u64,
    session: u32,
    action: ElevAction,
    peer_name: String,
    expires: MonoMicros,
    state: TicketState,
}

/// What an administrator confirmed for a connection: one use of `action`.
pub(crate) struct Pass {
    action: ElevAction,
    /// Who approved: the activity log names them (`GrantsChanged.by`).
    approver: swapdesk_trust::UserRef,
    expires: MonoMicros,
}

/// The hub's table of tickets and the lifetimes it works with.
pub(super) struct Elevation {
    tickets: Vec<TicketEntry>,
    ticket_ttl: Duration,
    pass_ttl: Duration,
    /// How long an elevator connection may stay (a test shortens it).
    pub(super) elevator_lifetime: Duration,
}

impl Default for Elevation {
    fn default() -> Self {
        Self { tickets: Vec::new(), ticket_ttl: TICKET_TTL, pass_ttl: PASS_TTL, elevator_lifetime: ELEVATOR_LIFETIME }
    }
}

impl Elevation {
    fn sweep(&mut self, now: MonoMicros) {
        self.tickets.retain(|t| t.expires > now);
    }

    /// Tickets in the table, expired ones the next sweep takes included.
    #[cfg(test)]
    pub(super) fn open_tickets(&self) -> usize {
        self.tickets.len()
    }
}

/// Compares two tickets without stopping at the first difference.
fn tokens_equal(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let diff = a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Whole seconds, rounded up, as the wire carries them.
fn seconds_up(micros: u64) -> u16 {
    u16::try_from(micros.div_ceil(1_000_000)).unwrap_or(u16::MAX)
}

fn kind(action: &ElevAction) -> &'static str {
    match action {
        ElevAction::SetGrants { .. } => "SetGrants",
        ElevAction::OpenAdminTerminal { .. } => "OpenAdminTerminal",
        ElevAction::ElevateSession { .. } => "ElevateSession",
    }
}

/// The device an action concerns, as the trace shows it.
fn device_of(action: &ElevAction) -> String {
    match action {
        ElevAction::SetGrants { device_id, .. } => device_id.clone(),
        ElevAction::OpenAdminTerminal { session_id } | ElevAction::ElevateSession { session_id } => {
            format!("of session {session_id:#x}")
        }
    }
}

impl Hub {
    fn log_elevation(&self, what: &str, action: &ElevAction) {
        self.log(
            Level::Info,
            format_args!("elevation ticket {what}: {} for device {}", kind(action), device_of(action)),
        );
    }

    /// `ElevationBegin`: a ticket for `action`, for the console's UI of any account (ACCOUNTS.md S7: a standard
    /// account's change is approved by an administrator over its shoulder). The action is checked the way the
    /// command it stands for is, so the administrator is never asked about one that would fail.
    pub(super) fn elevation_begin(&self, conn: &Conn, action: ElevAction) -> Result<ReplyBody, ApiCode> {
        self.require_console(conn, "an elevation ticket for a UI that is not the console's")?;
        let (action, peer_name) = self.checked_action(conn, action)?;
        let now = self.p.clock.now();
        let mut table = lock(&self.elevation);
        table.sweep(now);
        if table.tickets.iter().filter(|t| t.conn != conn.id).count() >= MAX_TICKETS {
            return Err(self.api_error(ApiCode::Busy, "sixteen elevation tickets are open"));
        }
        // One open ticket per connection: asking again replaces it.
        table.tickets.retain(|t| t.conn != conn.id);
        let ticket = random_token();
        let ttl = table.ticket_ttl;
        self.log_elevation("opened", &action);
        table.tickets.push(TicketEntry {
            ticket,
            conn: conn.id,
            session: conn.info.session_id,
            action,
            peer_name,
            expires: MonoMicros(now.0.saturating_add(micros(ttl))),
            state: TicketState::Open,
        });
        Ok(ReplyBody::ElevationTicket { ticket: ElevationTicket(ticket), ttl_s: seconds_up(micros(ttl)) })
    }

    /// The action in its canonical form (the device id as lowercase hex) and the name of the device it concerns,
    /// refused as `SetGrants` refuses it: grants that are not a valid set, a device id that is malformed or unknown.
    fn checked_action(&self, conn: &Conn, action: ElevAction) -> Result<(ElevAction, String), ApiCode> {
        match action {
            ElevAction::SetGrants { device_id, grants } => {
                let new = PermissionSet::from_bits(grants).ok_or(ApiCode::Invalid)?;
                let id = parse_device_id(&device_id).ok_or(ApiCode::Invalid)?;
                let name = self.p.trust.read(|s| s.find_by_id(&id).map(|d| d.name.clone())).ok_or(ApiCode::NotFound)?;
                Ok((ElevAction::SetGrants { device_id: device_id_hex(&id), grants: new.bits() }, name))
            }
            // ELEVATION.md 3.2: a viewer session of this connection; the administrator terminal only where the
            // session holds it, the elevation only for an entry without a prompt toward a host of minor 1 or newer
            // that is not elevated yet.
            ElevAction::OpenAdminTerminal { session_id } | ElevAction::ElevateSession { session_id } => {
                let handle = lock(&conn.viewers).get(&session_id).map(|v| v.handle.clone()).ok_or(ApiCode::NotFound)?;
                let granted = handle.granted();
                let ok = match action {
                    ElevAction::OpenAdminTerminal { .. } => granted.contains(PermissionSet::ADMIN_TERMINAL),
                    _ => crate::service::viewer_elevation_of(&handle.ctx(), handle.mode()) == 1,
                };
                if !ok {
                    return Err(ApiCode::Invalid);
                }
                let name =
                    self.p.trust.read(|s| s.find_by_id(&handle.device()).map(|d| d.name.clone())).unwrap_or_default();
                Ok((action, name))
            }
        }
    }

    /// `ElevationEnd`: how the asking connection's ticket ended. The entry goes; an open ticket is closed by this.
    /// A ticket of another connection is `Unknown`, as one that is not there.
    pub(super) fn elevation_end(&self, conn: &Conn, ticket: &ElevationTicket) -> ElevationOutcome {
        let now = self.p.clock.now();
        let mut table = lock(&self.elevation);
        table.sweep(now);
        let Some(i) = table.tickets.iter().position(|t| t.conn == conn.id && tokens_equal(&t.ticket, &ticket.0)) else {
            return ElevationOutcome::Unknown;
        };
        let entry = table.tickets.remove(i);
        drop(table);
        self.log_elevation("ended", &entry.action);
        match entry.state {
            TicketState::Open => ElevationOutcome::Open,
            TicketState::Confirmed => ElevationOutcome::Confirmed,
            TicketState::Declined => ElevationOutcome::Declined,
        }
    }

    /// Runs `then` on the open ticket `ticket` when `approver` may answer for it: an administrator token (`NeedsAdmin`),
    /// in the ticket's Windows session (`NotPermitted`), any user of it: an administrator who signs in at the prompt
    /// over a standard account's shoulder approves for it (ACCOUNTS.md 8.2, S7); a ticket that is open and not
    /// expired and whose asking connection is still connected (`NotFound`).
    fn with_ticket<T>(
        &self,
        ticket: &ElevationTicket,
        approver: &ConnInfo,
        then: impl FnOnce(&mut TicketEntry, &Conn, Duration) -> T,
    ) -> Result<T, ApiCode> {
        if !approver.admin {
            return Err(
                self.api_error(ApiCode::NeedsAdmin, "an elevation approval from a connection that is not elevated")
            );
        }
        // Taken before the table is: the two locks are never held in the other order.
        let conns = self.conns();
        let now = self.p.clock.now();
        let mut table = lock(&self.elevation);
        table.sweep(now);
        let pass_ttl = table.pass_ttl;
        let entry = table
            .tickets
            .iter_mut()
            .find(|t| tokens_equal(&t.ticket, &ticket.0) && t.state == TicketState::Open)
            .ok_or_else(|| self.api_error(ApiCode::NotFound, "an elevation ticket that is not open"))?;
        if entry.session != approver.session_id {
            return Err(self.api_error(ApiCode::NotPermitted, "an elevation approval from another session"));
        }
        let asking = conns
            .iter()
            .find(|c| c.id == entry.conn)
            .ok_or_else(|| self.api_error(ApiCode::NotFound, "an elevation ticket whose connection is gone"))?;
        Ok(then(entry, asking, pass_ttl))
    }

    pub(super) fn elevation_describe(
        &self,
        ticket: &ElevationTicket,
        approver: &ConnInfo,
    ) -> Result<(ElevAction, String, u16), ApiCode> {
        let now = self.p.clock.now();
        self.with_ticket(ticket, approver, |entry, _, _| {
            (entry.action.clone(), entry.peer_name.clone(), seconds_up(entry.expires.0.saturating_sub(now.0)))
        })
    }

    pub(super) fn elevation_confirm(
        &self,
        ticket: &ElevationTicket,
        approver: &ConnInfo,
        approve: bool,
    ) -> Result<(), ApiCode> {
        let now = self.p.clock.now();
        let (what, action) = self.with_ticket(ticket, approver, |entry, asking, pass_ttl| {
            if !approve {
                entry.state = TicketState::Declined;
                return ("declined", entry.action.clone());
            }
            entry.state = TicketState::Confirmed;
            let mut passes = lock(&asking.passes);
            passes.retain(|p| p.expires > now);
            if passes.len() >= MAX_PASSES {
                passes.remove(0);
            }
            passes.push(Pass {
                action: entry.action.clone(),
                approver: approver.user.clone(),
                expires: MonoMicros(now.0.saturating_add(micros(pass_ttl))),
            });
            ("confirmed", entry.action.clone())
        })?;
        self.log_elevation(what, &action);
        Ok(())
    }

    /// Removes and returns the first unexpired pass of `conn` for exactly `action`: one use.
    pub(super) fn take_pass(&self, conn: &Conn, action: &ElevAction) -> bool {
        self.take_pass_by(conn, action).is_some()
    }

    /// Uses up the pass for exactly `action`, if there is one, and says who approved it.
    pub(super) fn take_pass_by(&self, conn: &Conn, action: &ElevAction) -> Option<swapdesk_trust::UserRef> {
        let now = self.p.clock.now();
        lock(&self.elevation).sweep(now);
        let mut passes = lock(&conn.passes);
        passes.retain(|p| p.expires > now);
        let i = passes.iter().position(|p| &p.action == action)?;
        Some(passes.remove(i).approver)
    }

    /// The connection ended: its ticket and its passes go with it.
    pub(super) fn drop_elevation_of(&self, conn: &Conn) {
        lock(&self.elevation).tickets.retain(|t| t.conn != conn.id);
        lock(&conn.passes).clear();
    }

    /// The restricted loop of an elevated instance: `Describe` once, `Confirm` once, nothing else.
    fn elevator_loop<W: Write>(
        &self,
        rx: &mpsc::Receiver<Result<Option<Ipc>, IpcError>>,
        writer: &mut FrameWriter<W>,
        info: &ConnInfo,
        lifetime: Duration,
    ) -> Result<(), IpcError> {
        let deadline = Instant::now() + lifetime;
        let (mut described, mut confirmed) = (false, false);
        while !confirmed {
            if self.shutting.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = match rx.recv_timeout(left) {
                Ok(Ok(Some(msg))) => msg,
                // The end of the pipe, or the reader thread gone.
                Ok(Ok(None)) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => {
                    let _ = writer.write_message(&Ipc::Bye { code: ErrorCode::Timeout });
                    return Ok(());
                }
            };
            match msg {
                Ipc::Bye { .. } => return Ok(()),
                Ipc::UiRequest { req, op: UiOp::ElevationDescribe { ticket } } if !described => {
                    described = true;
                    let r = self.elevation_describe(&ticket, info).map(|(action, peer_name, remaining_s)| {
                        ReplyBody::ElevationAction { action, peer_name, remaining_s }
                    });
                    writer.write_message(&reply_message(req, r))?;
                }
                Ipc::UiRequest { req, op: UiOp::ElevationConfirm { ticket, approve } } if !confirmed => {
                    confirmed = true;
                    let r = self.elevation_confirm(&ticket, info, approve).map(|()| ReplyBody::Done);
                    writer.write_message(&reply_message(req, r))?;
                }
                // Any other message, and a second Describe or Confirm, is a protocol error.
                other => {
                    self.log(
                        Level::Debug,
                        format_args!("elevator connection: unexpected message {:#x}", other.type_code()),
                    );
                    let _ = writer.write_message(&Ipc::Bye { code: ErrorCode::ProtocolViolation });
                    return Err(IpcError::UnknownType(other.type_code()));
                }
            }
        }
        Ok(())
    }
}

impl UiHub {
    /// What an elevated instance of the asking user would be shown: the action the ticket is for, the name of the
    /// device it concerns and the seconds the ticket has left. `approver` is the connection that asks, as the pipe
    /// checks (or the in-process app) established it.
    pub fn describe_elevation(
        &self,
        ticket: &ElevationTicket,
        approver: &ConnInfo,
    ) -> Result<(ElevAction, String, u16), ApiCode> {
        self.hub.elevation_describe(ticket, approver)
    }

    /// The administrator's answer: `approve` puts a single-use pass on the asking connection; either answer closes the
    /// ticket for good.
    pub fn confirm_elevation(
        &self,
        ticket: &ElevationTicket,
        approver: &ConnInfo,
        approve: bool,
    ) -> Result<(), ApiCode> {
        self.hub.elevation_confirm(ticket, approver, approve)
    }

    /// Lifetimes of a ticket and of a pass, read on the hub's clock. The defaults are the 120 s and 30 s of the design;
    /// tests give short ones.
    pub fn set_elevation_ttls(&self, ticket: Duration, pass: Duration) {
        let mut table = lock(&self.hub.elevation);
        table.ticket_ttl = ticket;
        table.pass_ttl = pass;
    }

    /// Serves the connection of an elevated instance until it ends: after the server handshake with `Hello.role`
    /// checked to be `Elevator` and the pipe checks of C2.1 and C2.3 (`info`). It is not one of the hub's connections
    /// (no notifications, no console, no clipboard or disk), answers `ElevationDescribe` once and `ElevationConfirm`
    /// once, and ends after the confirmation, on `Bye`, at the end of the pipe or after 125 s. Anything else is
    /// answered with `Bye` (protocol violation) and ends it. Blocks the calling thread.
    pub fn serve_elevator<R, W>(
        &self,
        reader: FrameReader<R>,
        writer: FrameWriter<W>,
        info: ConnInfo,
    ) -> Result<(), IpcError>
    where
        R: Read + Send + 'static,
        W: Write,
    {
        self.serve_elevator_with(reader, writer, info, None)
    }

    /// [`serve_elevator`](Self::serve_elevator) for a pipe the owner can cancel: `cancel` runs when the loop ends and
    /// must cancel the pipe's pending read, so the thread that reads it ends too and the owner can drop the pipe.
    pub fn serve_elevator_cancellable<R, W>(
        &self,
        reader: FrameReader<R>,
        writer: FrameWriter<W>,
        info: ConnInfo,
        cancel: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), IpcError>
    where
        R: Read + Send + 'static,
        W: Write,
    {
        self.serve_elevator_with(reader, writer, info, Some(cancel))
    }

    fn serve_elevator_with<R, W>(
        &self,
        mut reader: FrameReader<R>,
        mut writer: FrameWriter<W>,
        info: ConnInfo,
        cancel: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<(), IpcError>
    where
        R: Read + Send + 'static,
        W: Write,
    {
        // The read waits on its own thread so that the lifetime holds on a pipe that says nothing.
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("ui-hub-elevator-read".into()).spawn(move || {
            loop {
                let msg = reader.read_message();
                let last = !matches!(msg, Ok(Some(_)));
                if tx.send(msg).is_err() || last {
                    break;
                }
            }
        })?;
        let lifetime = lock(&self.hub.elevation).elevator_lifetime;
        let end = self.hub.elevator_loop(&rx, &mut writer, &info, lifetime);
        drop(rx);
        if let Some(cancel) = cancel {
            cancel();
        }
        end
    }
}
