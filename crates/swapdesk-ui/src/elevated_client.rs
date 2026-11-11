//! The elevated instance's side of the UI pipe (ELEVATION.md 3.4): handshake as role `Elevator`, ask the service what
//! the ticket is for, let the person confirm, tell the service. No OS call: the Windows binary and the loopback test
//! share it, and the binary turns [`ElevatedExit`] into its exit code.

use std::io::{Read, Write};

use swapdesk_ipc::{
    ApiCode, ElevAction, ElevationTicket, FrameReader, FrameWriter, Handshake, IpcError, IpcRole, Message, ReplyBody,
    UiOp, status_code,
};
use swapdesk_proto::ErrorCode;

use crate::{ElevatedAction, Language, Permissions};

/// The line the confirmation window shows for `action`, in `lang`: the words of the step that asked.
pub fn action_line(lang: Language, action: &ElevAction, peer_name: &str) -> String {
    match action {
        ElevAction::SetGrants { device_id, grants } => crate::interact::elevation_action_text(
            lang,
            &ElevatedAction::SetGrants { device_id: device_id.clone(), grants: Permissions(*grants) },
            peer_name,
        ),
    }
}

/// How the elevated instance ended. The code is the process exit code (ELEVATION.md 3.5; 2 is the command line's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevatedExit {
    /// The service answered `Done` to an approval.
    Confirmed,
    /// The person declined; `approve: false` was sent.
    Declined,
    /// The service cannot be reached: no handshake, or the pipe broke.
    Unreachable,
    /// The service refused this account.
    Refused,
    /// The ticket is unknown, used or expired.
    Expired,
    /// The service says this process is not an administrator.
    NotAdmin,
    /// A reply this side cannot read.
    Internal,
}

impl ElevatedExit {
    pub const fn code(self) -> u8 {
        match self {
            Self::Confirmed => 0,
            Self::Declined => 1,
            Self::Unreachable => 3,
            Self::Refused => 4,
            Self::Expired => 5,
            Self::NotAdmin => 6,
            Self::Internal => 7,
        }
    }
}

fn exit_of(code: ApiCode) -> ElevatedExit {
    match code {
        ApiCode::NeedsAdmin => ElevatedExit::NotAdmin,
        ApiCode::NotPermitted => ElevatedExit::Refused,
        ApiCode::NotFound => ElevatedExit::Expired,
        ApiCode::ServiceDown => ElevatedExit::Unreachable,
        _ => ElevatedExit::Internal,
    }
}

/// Sends `op` as request `req` and reads until its reply: the decoded body, or how this side ends.
fn ask<R: Read, W: Write>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    req: u32,
    op: UiOp,
) -> Result<ReplyBody, ElevatedExit> {
    let code = op.code();
    writer.write_message(&Message::UiRequest { req, op }).map_err(|_| ElevatedExit::Unreachable)?;
    loop {
        match reader.read_message() {
            Ok(Some(Message::UiReply { req: r, status, body })) if r == req => {
                return match status_code(status) {
                    Ok(Some(c)) => Err(exit_of(c)),
                    Ok(None) => ReplyBody::decode(code, &body.0).map_err(|_| ElevatedExit::Internal),
                    Err(_) => Err(ElevatedExit::Internal),
                };
            }
            Ok(Some(Message::Bye { code: ErrorCode::NotPermitted })) => return Err(ElevatedExit::Refused),
            // The elevator connection's lifetime ran out: the ticket is no longer usable either.
            Ok(Some(Message::Bye { code: ErrorCode::Timeout })) => return Err(ElevatedExit::Expired),
            Ok(Some(Message::Bye { .. })) | Ok(None) | Err(IpcError::Io(_)) => return Err(ElevatedExit::Unreachable),
            Err(_) => return Err(ElevatedExit::Internal),
            // An elevator connection gets no notification; anything else is not an answer to this request.
            Ok(Some(_)) => {}
        }
    }
}

/// Runs the instance's side over a connection already opened to the service. `confirm` gets the action as the
/// service holds it, the peer's name and the seconds left, and answers whether the person approves.
pub fn run<R: Read, W: Write>(
    reader: R,
    writer: W,
    ticket: &ElevationTicket,
    confirm: impl FnOnce(&ElevAction, &str, u16) -> bool,
) -> ElevatedExit {
    let mut reader = FrameReader::new(reader);
    let mut writer = FrameWriter::new(writer);
    match Handshake::client(&mut reader, &mut writer, IpcRole::Elevator, std::process::id()) {
        Ok(_) => {}
        Err(IpcError::PeerBye(ErrorCode::NotPermitted)) => return ElevatedExit::Refused,
        Err(_) => return ElevatedExit::Unreachable,
    }
    let ReplyBody::ElevationAction { action, peer_name, remaining_s } =
        (match ask(&mut reader, &mut writer, 1, UiOp::ElevationDescribe { ticket: *ticket }) {
            Ok(b) => b,
            Err(e) => return e,
        })
    else {
        return ElevatedExit::Internal;
    };
    let approve = confirm(&action, &peer_name, remaining_s);
    match ask(&mut reader, &mut writer, 2, UiOp::ElevationConfirm { ticket: *ticket, approve }) {
        Ok(ReplyBody::Done) if approve => ElevatedExit::Confirmed,
        Ok(ReplyBody::Done) => ElevatedExit::Declined,
        Ok(_) => ElevatedExit::Internal,
        Err(e) => e,
    }
}
