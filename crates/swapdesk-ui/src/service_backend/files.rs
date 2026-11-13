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
        Command::FilesListPage { session_id, path, start } => {
            FilesOp::ListPage { session_id: *session_id, path: Text(path.clone()), start: *start }
        }
        Command::FilesChange { session_id, change } => {
            FilesOp::Change { session_id: *session_id, change: change_to_wire(change) }
        }
        Command::FilesVolumes { session_id } => FilesOp::Volumes { session_id: *session_id },
        _ => return None,
    }))
}

fn names_to_wire(names: &[String]) -> Vec<w::FsNameView> {
    names.iter().map(|n| w::FsNameView { name: Text(n.clone()) }).collect()
}

/// A change as the wire carries it.
pub fn change_to_wire(c: &a::RemoteChange) -> w::FsChange {
    match c {
        a::RemoteChange::MakeDir { parent, name } => {
            w::FsChange::MakeDir { parent: Text(parent.clone()), name: Text(name.clone()) }
        }
        a::RemoteChange::Rename { dir, name, new_name } => {
            w::FsChange::Rename { dir: Text(dir.clone()), name: Text(name.clone()), new_name: Text(new_name.clone()) }
        }
        a::RemoteChange::Move { from_dir, names, to_dir } => w::FsChange::Move {
            from_dir: Text(from_dir.clone()),
            names: names_to_wire(names),
            to_dir: Text(to_dir.clone()),
        },
        a::RemoteChange::Trash { dir, names } => {
            w::FsChange::Trash { dir: Text(dir.clone()), names: names_to_wire(names) }
        }
    }
}

/// What one name's code means to the explorer.
pub fn outcome_from_wire(code: swapdesk_proto::ErrorCode) -> a::ChangeOutcome {
    use swapdesk_proto::ErrorCode as E;
    match code {
        E::NoError => a::ChangeOutcome::Done,
        E::Exists => a::ChangeOutcome::Exists,
        E::NoBin => a::ChangeOutcome::NoBin,
        E::VolumeGone => a::ChangeOutcome::VolumeGone,
        E::InvalidName => a::ChangeOutcome::InvalidName,
        E::NotPermitted => a::ChangeOutcome::NotPermitted,
        E::NotFound => a::ChangeOutcome::NotFound,
        E::NoSpace => a::ChangeOutcome::NoSpace,
        _ => a::ChangeOutcome::Failed,
    }
}

/// A page of the other computer's folder.
pub fn page_from_wire(l: w::FileListing2) -> a::RemotePage {
    use swapdesk_proto::{ENTRY_HIDDEN, ENTRY_READ_ONLY, ENTRY_SYSTEM, EntryKind};
    a::RemotePage {
        path: l.path.0,
        start: l.start,
        entries: l
            .page
            .entries
            .into_iter()
            .map(|e| a::RemoteEntry {
                name: e.name.0,
                kind: match e.kind {
                    EntryKind::File => a::RemoteKind::File,
                    EntryKind::Dir => a::RemoteKind::Dir,
                    EntryKind::Link => a::RemoteKind::Link,
                },
                size: e.size,
                mtime_ms: e.mtime_ms,
                hidden: e.flags & ENTRY_HIDDEN != 0,
                system: e.flags & ENTRY_SYSTEM != 0,
                read_only: e.flags & ENTRY_READ_ONLY != 0,
            })
            .collect(),
        next: l.page.next,
        free: l.page.free,
    }
}

/// The other computer's home folder, places and volumes.
pub fn volumes_from_wire(v: w::VolumesView) -> a::RemoteVolumes {
    use swapdesk_proto::{PlaceKind as P, VolumeKind as V};
    a::RemoteVolumes {
        home: v.home.0,
        places: v
            .places
            .into_iter()
            .map(|p| a::RemotePlace {
                kind: match p.kind {
                    P::Desktop => a::PlaceName::Desktop,
                    P::Documents => a::PlaceName::Documents,
                    P::Downloads => a::PlaceName::Downloads,
                    P::Pictures => a::PlaceName::Pictures,
                    P::Music => a::PlaceName::Music,
                    P::Videos => a::PlaceName::Videos,
                    P::Received => a::PlaceName::Received,
                },
                path: p.path.0,
            })
            .collect(),
        volumes: v
            .volumes
            .into_iter()
            .map(|d| a::RemoteVolume {
                id: d.id,
                kind: match d.kind {
                    V::System => a::VolumeType::System,
                    V::Fixed => a::VolumeType::Fixed,
                    V::Removable => a::VolumeType::Removable,
                    V::Network => a::VolumeType::Network,
                    V::Optical => a::VolumeType::Optical,
                    V::Image => a::VolumeType::Image,
                },
                label: d.label.0,
                letter: d.letter.filter(u8::is_ascii_uppercase).map(char::from),
                total_bytes: d.total_bytes,
                free_bytes: d.free_bytes,
                has_bin: d.has_bin,
            })
            .collect(),
    }
}

/// The wire form of the other computer's volumes (tests and mocks that play the service).
pub fn volumes_to_wire(v: &a::RemoteVolumes) -> w::VolumesView {
    use swapdesk_proto::{PlaceKind as P, VolumeKind as V};
    w::VolumesView {
        home: Text(v.home.clone()),
        places: v
            .places
            .iter()
            .map(|p| w::PlaceView {
                kind: match p.kind {
                    a::PlaceName::Desktop => P::Desktop,
                    a::PlaceName::Documents => P::Documents,
                    a::PlaceName::Downloads => P::Downloads,
                    a::PlaceName::Pictures => P::Pictures,
                    a::PlaceName::Music => P::Music,
                    a::PlaceName::Videos => P::Videos,
                    a::PlaceName::Received => P::Received,
                },
                path: Text(p.path.clone()),
            })
            .collect(),
        volumes: v
            .volumes
            .iter()
            .map(|d| w::VolumeView {
                id: d.id,
                kind: match d.kind {
                    a::VolumeType::System => V::System,
                    a::VolumeType::Fixed => V::Fixed,
                    a::VolumeType::Removable => V::Removable,
                    a::VolumeType::Network => V::Network,
                    a::VolumeType::Optical => V::Optical,
                    a::VolumeType::Image => V::Image,
                },
                label: Text(d.label.clone()),
                letter: d.letter.and_then(|c| u8::try_from(c).ok()),
                total_bytes: d.total_bytes,
                free_bytes: d.free_bytes,
                has_bin: d.has_bin,
            })
            .collect(),
    }
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
        FilesNote::Volumes { session_id, volumes } => {
            a::UiEvent::RemoteVolumes { session_id, volumes: volumes_from_wire(volumes) }
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
        a::UiEvent::RemoteVolumes { session_id, volumes } => {
            FilesNote::Volumes { session_id: *session_id, volumes: volumes_to_wire(volumes) }
        }
        _ => return None,
    })
}
