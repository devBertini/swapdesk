//! What the host's banner says about files (FILES-UI.md 6): one extra line while files move or change in the session
//! this computer hosts, never with a file name (anyone near the screen sees the banner).
//!
//! * While transfers run: "Copiando arquivos: 2 de 5" (files going to the viewer) or "Recebendo arquivos: 2 de 5"
//!   (files arriving here). The count is of the files the host's engine knows in the current run of transfers: it
//!   starts again when the first one begins after all had ended.
//! * After a change the viewer made (new folder, rename, move, to the bin): "Renomeou 1 item" and the like, for
//!   [`LINE_MS`]; changes of one kind that follow each other inside that time add up.
//! * Once per session, when the viewer first reaches a volume outside the home folder: "Abrindo arquivos de outros
//!   discos", for [`LINE_MS`].
//!
//! The data comes from the host's own engine through the hub (`FilesNote` kinds 20, 23 and 24 for the hosted session);
//! the state keeps only counts and the instant of the last change.

use crate::api::{Msg, TransferState};
use crate::state::{Changed, UiState};
use crate::strings::MsgKey;

/// How long a change's line (and the other-disks line) stays on the banner (FILES-UI.md 6, GUESS value).
pub const LINE_MS: u64 = 10_000;

/// The audit's numbering of a change (`Event::Changed.op`).
const OP_NEW_FOLDER: u8 = 0;
const OP_RENAME: u8 = 1;
const OP_MOVE: u8 = 2;
const OP_TRASH: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Moving {
    id: u32,
    /// True: this computer sends the file to the viewer.
    outgoing: bool,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NoteKind {
    Changed { op: u8, count: u32 },
    Reach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Note {
    kind: NoteKind,
    at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HostFiles {
    /// The hosted session these facts belong to.
    session: u64,
    moving: Vec<Moving>,
    note: Option<Note>,
    /// The other-disks line was said in this session: it is never said twice.
    reach_said: bool,
}

impl HostFiles {
    /// The direction to show and its `(done, total)`: sending first, while any file of it is unfinished.
    fn running(&self) -> Option<(bool, usize, usize)> {
        let dir = [true, false].into_iter().find(|d| self.moving.iter().any(|m| m.outgoing == *d && !m.finished))?;
        let of_dir = self.moving.iter().filter(|m| m.outgoing == dir);
        let total = of_dir.clone().count();
        let done = of_dir.filter(|m| m.finished).count();
        Some((dir, done, total))
    }

    fn note_live(&self, now_ms: u64) -> Option<&Note> {
        self.note.as_ref().filter(|n| now_ms < n.at_ms.saturating_add(LINE_MS))
    }
}

impl UiState {
    /// True (and the facts reset when it is another session's) when `session_id` is the hosted session.
    fn host_files_of(&mut self, session_id: u64) -> bool {
        if self.host_session().is_none_or(|h| h.id != session_id) {
            return false;
        }
        if self.host_files.session != session_id {
            self.host_files = HostFiles { session: session_id, ..HostFiles::default() };
        }
        true
    }

    /// Whether a transfer event is about the hosted session (it then feeds the banner and is nobody's explorer row).
    pub(crate) fn is_hosted_transfer(&self, session_id: u64) -> bool {
        self.host_session().is_some_and(|h| h.id == session_id)
    }

    /// A transfer of the hosted session moved or ended.
    pub(crate) fn host_transfer(&mut self, session_id: u64, id: u32, outgoing: bool, state: &TransferState) -> Changed {
        if !self.host_files_of(session_id) {
            return Changed::NONE;
        }
        let finished = matches!(state, TransferState::Done | TransferState::Failed { .. });
        let hf = &mut self.host_files;
        if let Some(m) = hf.moving.iter_mut().find(|m| m.id == id && m.outgoing == outgoing) {
            m.finished = finished;
        } else {
            if hf.moving.iter().all(|m| m.finished) {
                hf.moving.clear();
            }
            hf.moving.push(Moving { id, outgoing, finished });
        }
        Changed::BANNER
    }

    /// The viewer changed files on this computer.
    pub(crate) fn host_files_changed(&mut self, session_id: u64, op: u8, count: u32) -> Changed {
        if !self.host_files_of(session_id) || count == 0 || op > OP_TRASH {
            return Changed::NONE;
        }
        let now = self.clock.now_ms;
        let hf = &mut self.host_files;
        let sum = match hf.note_live(now) {
            Some(Note { kind: NoteKind::Changed { op: before, count: c }, .. }) if *before == op => {
                c.saturating_add(count)
            }
            _ => count,
        };
        hf.note = Some(Note { kind: NoteKind::Changed { op, count: sum }, at_ms: now });
        Changed::BANNER
    }

    /// The viewer reached a volume outside the home folder: said once per session.
    pub(crate) fn host_files_reach(&mut self, session_id: u64) -> Changed {
        if !self.host_files_of(session_id) || self.host_files.reach_said {
            return Changed::NONE;
        }
        self.host_files.reach_said = true;
        self.host_files.note = Some(Note { kind: NoteKind::Reach, at_ms: self.clock.now_ms });
        Changed::BANNER
    }

    /// The banner's extra line, if one is due now: transfers first, else the last change or the other-disks note
    /// while it is younger than [`LINE_MS`]. Never a file name.
    pub fn banner_files_line(&self) -> Option<String> {
        let hf = &self.host_files;
        let host = self.host_session()?;
        if hf.session != host.id {
            return None;
        }
        if let Some((outgoing, done, total)) = hf.running() {
            let key = if outgoing { MsgKey::BannerFilesSending } else { MsgKey::BannerFilesReceiving };
            return Some(self.tm(&Msg::new(key).with(done.to_string()).with(total.to_string())));
        }
        let note = hf.note_live(self.clock.now_ms)?;
        Some(match &note.kind {
            NoteKind::Reach => self.t(MsgKey::BannerFilesOtherDisks),
            NoteKind::Changed { op, count } => {
                let key = match *op {
                    OP_NEW_FOLDER => return Some(self.t(MsgKey::BannerFilesNewFolder)),
                    OP_RENAME => MsgKey::BannerFilesRenamed,
                    OP_MOVE => MsgKey::BannerFilesMoved,
                    _ => MsgKey::BannerFilesTrashed,
                };
                self.tm(&Msg::new(key).with(count.to_string()))
            }
        })
    }

    /// Milliseconds until the extra line's note expires (so the banner repaints without it), `None` when no note is
    /// showing. A running transfer needs no timer: its events repaint the banner.
    pub(crate) fn banner_files_expiry_ms(&self, now_ms: u64) -> Option<u64> {
        let hf = &self.host_files;
        if self.host_session().is_none_or(|h| h.id != hf.session) {
            return None;
        }
        let n = hf.note_live(now_ms)?;
        Some(n.at_ms.saturating_add(LINE_MS).saturating_sub(now_ms).max(1))
    }
}
