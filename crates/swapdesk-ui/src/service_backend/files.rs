//! The files feature of the backend (stage 44): the C6 commands `FilesSend` and `FilesGet` as C2.4 ops, the wire to C6
//! conversions, and the disk server the UI process runs for the viewer sessions.
//!
//! The UI is the process of the user's own files on the viewer side. The service asks it, over the UI pipe, to read the
//! files the user picked and to write into the folder the user chose (`FsRequest`, C2.4 "Files"); it answers from a
//! thread of its own and only for the **tokens** it made itself: the real path of a picked file or of a receive folder
//! never leaves this process, and a service that sends any other path gets `InvalidName`.

use std::sync::{Arc, mpsc};
use std::thread;

use swapdesk_files::{FsServer, Roots, Tokens};
use swapdesk_ipc as w;
use swapdesk_ipc::{FilePick, FilesNote, FilesOp, Message, Text, TransferWire};

use crate::api as a;
use crate::api::{ApiCode, ApiError, Command};

/// What the UI keeps for the service's disk calls.
pub(crate) struct Disk {
    pub tokens: Arc<Tokens>,
    pub server: Arc<FsServer>,
}

impl Disk {
    pub fn new() -> Self {
        let tokens = Arc::new(Tokens::new());
        let server = Arc::new(FsServer::new(Roots::Tokens(Arc::clone(&tokens))));
        Self { tokens, server }
    }

    /// Answers `FsRequest` messages from a thread of its own (a slow disk never holds up the pipe's reader), writing
    /// each `FsReply` with `write`. The thread ends when the returned sender is dropped.
    pub fn spawn_server(&self, write: impl Fn(Message) + Send + 'static) -> std::io::Result<mpsc::Sender<Message>> {
        let (tx, rx) = mpsc::channel::<Message>();
        let server = Arc::clone(&self.server);
        thread::Builder::new().name("swapdesk-ui-files".into()).spawn(move || {
            while let Ok(msg) = rx.recv() {
                if let Some(reply) = server.serve(msg) {
                    write(reply);
                }
            }
        })?;
        Ok(tx)
    }
}

/// The op of `FilesSend` and `FilesGet`: it needs the tokens, so the plain [`super::convert::route`] leaves it out.
/// `None` for any other command.
pub(crate) fn route_files(c: &Command, tokens: &Tokens) -> Option<Result<w::UiOp, ApiError>> {
    let invalid = || ApiError::new(ApiCode::Invalid);
    Some(match c {
        Command::FilesSend { session_id, local_paths, remote_dir } => {
            if local_paths.is_empty() || local_paths.len() > w::MAX_PICKS {
                return Some(Err(invalid()));
            }
            let mut picks = Vec::with_capacity(local_paths.len());
            for p in local_paths {
                match tokens.pick(p.clone()) {
                    Some((token, name)) => picks.push(FilePick { token, name: Text(name) }),
                    None => {
                        // A file with no usable name cannot be offered: nothing of this batch is handed out.
                        for done in &picks {
                            tokens.forget(done.token);
                        }
                        return Some(Err(invalid()));
                    }
                }
            }
            Ok(w::UiOp::Files(FilesOp::Send {
                session_id: *session_id,
                picks,
                remote_dir: remote_dir.clone().map(Text),
            }))
        }
        Command::FilesGet { session_id, remote_path, local_dir } => {
            let recv_token = tokens.receive_into(local_dir.clone());
            Ok(w::UiOp::Files(FilesOp::Get {
                session_id: *session_id,
                remote_path: Text(remote_path.clone()),
                recv_token,
            }))
        }
        _ => return None,
    })
}

/// The ops of the commands that need no token.
pub(crate) fn op_of(c: &Command) -> Option<w::UiOp> {
    Some(w::UiOp::Files(match c {
        Command::FilesList { session_id, path } => {
            FilesOp::List { session_id: *session_id, path: path.clone().map(Text) }
        }
        Command::TransferPause { transfer_id } => FilesOp::Pause { transfer_id: *transfer_id },
        Command::TransferResume { transfer_id } => FilesOp::Resume { transfer_id: *transfer_id },
        Command::TransferCancel { transfer_id } => FilesOp::Cancel { transfer_id: *transfer_id },
        _ => return None,
    }))
}

pub fn listing_from_wire(l: w::FileListing) -> a::Listing {
    a::Listing {
        path: l.path.0,
        entries: l
            .entries
            .into_iter()
            .map(|e| a::DirEntryView { name: e.name.0, is_dir: e.is_dir, size: e.size, mtime_ms: e.mtime_ms })
            .collect(),
        truncated: l.truncated,
    }
}

pub fn listing_to_wire(l: &a::Listing) -> w::FileListing {
    w::FileListing {
        path: Text(l.path.clone()),
        entries: l
            .entries
            .iter()
            .map(|e| w::FileEntry { name: Text(e.name.clone()), is_dir: e.is_dir, size: e.size, mtime_ms: e.mtime_ms })
            .collect(),
        truncated: l.truncated,
    }
}

pub fn transfer_state_from_wire(s: TransferWire) -> a::TransferState {
    match s {
        TransferWire::Queued => a::TransferState::Queued,
        TransferWire::Running { done, total, bytes_per_s } => a::TransferState::Running { done, total, bytes_per_s },
        TransferWire::Paused { done, total, why } => {
            a::TransferState::Paused { done, total, why: super::convert::code_from_wire(why) }
        }
        TransferWire::Done => a::TransferState::Done,
        TransferWire::Failed { reason } => a::TransferState::Failed { reason: super::convert::code_from_wire(reason) },
    }
}

pub fn transfer_state_to_wire(s: &a::TransferState) -> TransferWire {
    match s {
        a::TransferState::Queued => TransferWire::Queued,
        a::TransferState::Running { done, total, bytes_per_s } => {
            TransferWire::Running { done: *done, total: *total, bytes_per_s: *bytes_per_s }
        }
        a::TransferState::Paused { done, total, why } => {
            TransferWire::Paused { done: *done, total: *total, why: super::convert::code_to_wire(*why) }
        }
        a::TransferState::Done => TransferWire::Done,
        a::TransferState::Failed { reason } => TransferWire::Failed { reason: super::convert::code_to_wire(*reason) },
    }
}

/// The `UiEvent` of a files notification.
pub fn note_to_event(n: FilesNote) -> a::UiEvent {
    match n {
        FilesNote::Transfer { session_id, transfer_id, name, outgoing, state } => a::UiEvent::Transfer {
            transfer_id,
            session_id,
            name: name.0,
            outgoing,
            state: transfer_state_from_wire(state),
        },
        FilesNote::SavedAs { session_id, transfer_id, name } => {
            a::UiEvent::TransferSavedAs { transfer_id, session_id, name: name.0 }
        }
    }
}

/// The notification of a transfer event, for tests and mocks that play the service.
pub fn event_to_note(e: &a::UiEvent) -> Option<FilesNote> {
    Some(match e {
        a::UiEvent::Transfer { transfer_id, session_id, name, outgoing, state } => FilesNote::Transfer {
            session_id: *session_id,
            transfer_id: *transfer_id,
            name: Text(name.clone()),
            outgoing: *outgoing,
            state: transfer_state_to_wire(state),
        },
        a::UiEvent::TransferSavedAs { transfer_id, session_id, name } => {
            FilesNote::SavedAs { session_id: *session_id, transfer_id: *transfer_id, name: Text(name.clone()) }
        }
        _ => return None,
    })
}
