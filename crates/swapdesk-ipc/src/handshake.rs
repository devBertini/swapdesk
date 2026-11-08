//! The first exchange of every connection: `Hello` from the client, `HelloAck` from the service.

use std::io::{Read, Write};

use swapdesk_proto::ErrorCode;

use crate::error::{IpcError, Result};
use crate::frame::{FrameReader, FrameWriter};
use crate::message::{IPC_VERSION, Message};
use crate::types::IpcRole;

/// What the service learns from a valid `Hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientHello {
    pub role: IpcRole,
    pub pid: u32,
}

/// What the client learns from a valid `HelloAck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerHello {
    pub win_session: u32,
}

/// Both sides of the opening exchange.
pub struct Handshake;

impl Handshake {
    /// Client side: sends `Hello` (version [`IPC_VERSION`]) and waits for `HelloAck`. A `Bye` or any other
    /// answer is an error.
    pub fn client<R: Read, W: Write>(
        reader: &mut FrameReader<R>,
        writer: &mut FrameWriter<W>,
        role: IpcRole,
        pid: u32,
    ) -> Result<ServerHello> {
        writer.write_message(&Message::Hello { role, version: IPC_VERSION, pid })?;
        match reader.read_message()? {
            Some(Message::HelloAck { version, win_session }) => {
                if version != IPC_VERSION {
                    return Err(IpcError::UnsupportedVersion(version));
                }
                Ok(ServerHello { win_session })
            }
            Some(Message::Bye { code }) => Err(IpcError::PeerBye(code)),
            Some(other) => Err(IpcError::NotHelloAck { got: other.type_code() }),
            None => Err(IpcError::Closed),
        }
    }

    /// Service side: the first message must be `Hello`; anything else is an error and nothing is answered (the
    /// caller closes the pipe). A `Hello` of another version is answered with `Bye(UNSUPPORTED_VERSION)`.
    pub fn server<R: Read, W: Write>(
        reader: &mut FrameReader<R>,
        writer: &mut FrameWriter<W>,
        win_session: u32,
    ) -> Result<ClientHello> {
        match reader.read_message()? {
            Some(Message::Hello { role, version, pid }) => {
                if version != IPC_VERSION {
                    // Best effort: the version error is what the caller reports.
                    let _ = writer.write_message(&Message::Bye { code: ErrorCode::UnsupportedVersion });
                    return Err(IpcError::UnsupportedVersion(version));
                }
                writer.write_message(&Message::HelloAck { version: IPC_VERSION, win_session })?;
                Ok(ClientHello { role, pid })
            }
            Some(other) => Err(IpcError::NotHello { got: other.type_code() }),
            None => Err(IpcError::Closed),
        }
    }
}
