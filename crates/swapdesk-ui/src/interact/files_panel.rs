//! The file panel of the viewer's drawer (stage 44, F-FILE-01 and F-FILE-02): browse a folder of the other computer,
//! send files, choose the folder to receive into, follow the transfers, "Retomar" a paused one.
//!
//! The panel never touches a disk. It sends C6 commands; the backend turns the picked files and the receive folder
//! into tokens the service can name and nothing else (C2.4 "Files"). Choosing files and folders is the shell's job
//! (the native dialogs): the panel asks with an [`Effect`] and gets the answer through [`UiState::send_files`] and
//! [`UiState::set_receive_dir`].

use std::path::{Path, PathBuf};

use super::{ControlId, Specs, UiRole};
use crate::api::{Command, Msg, TransferState};
use crate::state::{Activation, Changed, DataKind, Effect, Fetch, UiState};
use crate::strings::MsgKey;

/// What the panel remembers between events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FilesPanel {
    /// The folder the user chose to receive files into; `None` until the shell or the user sets one.
    pub recv_dir: Option<PathBuf>,
    /// A download asked for before a receive folder existed: it starts when the folder is set.
    pub waiting_download: Option<String>,
    /// The page of the folder's listing the panel shows (`FILES_PAGE` entries each).
    pub page: usize,
}

/// Entries of the folder's listing the panel shows at a time.
pub const FILES_PAGE: usize = 5;

/// `parent/name`, with the host's `/` and an empty path as the root.
fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() { name.to_string() } else { format!("{folder}/{name}") }
}

/// The folder above `folder` (the root for a first-level one); `None` at the root.
fn parent(folder: &str) -> Option<String> {
    if folder.is_empty() {
        return None;
    }
    Some(folder.rsplit_once('/').map(|(up, _)| up.to_string()).unwrap_or_default())
}

impl UiState {
    /// The folder files are received into, if one is set.
    pub fn receive_dir(&self) -> Option<&Path> {
        self.files.recv_dir.as_deref()
    }

    /// The remote folder the panel shows, as the other computer spells it (empty for its root).
    pub fn remote_folder(&self) -> Option<&str> {
        self.listing.as_ref().map(|l| l.path.as_str())
    }

    /// The entries of the listing the panel shows now (indices into `listing().entries`).
    pub fn files_page_rows(&self) -> std::ops::Range<usize> {
        let total = self.listing.as_ref().map_or(0, |l| l.entries.len());
        let first = (self.files.page * FILES_PAGE).min(total.saturating_sub(1) / FILES_PAGE * FILES_PAGE);
        first..(first + FILES_PAGE).min(total)
    }

    /// The transfers of the viewer session shown (the other sessions' belong to the host role).
    pub fn viewer_transfers(&self) -> Vec<&crate::state::TransferView> {
        let session = self.viewer_session().map(|s| s.id);
        self.transfers.iter().filter(|t| Some(t.session_id) == session).collect()
    }

    /// Can the panel go up a folder?
    pub fn can_go_up(&self) -> bool {
        self.remote_folder().is_some_and(|p| !p.is_empty())
    }

    /// The shell chose the receive folder (or has a default one). A download that waited for it starts now.
    pub fn set_receive_dir(&mut self, dir: PathBuf) -> Activation {
        self.files.recv_dir = Some(dir.clone());
        let mut a = Activation { changed: Changed::VIEWER, ..Activation::default() };
        if let (Some(path), Some(session_id)) = (self.files.waiting_download.take(), self.session_id()) {
            a.commands.push(Command::FilesGet { session_id, remote_path: path, local_dir: dir });
        }
        a
    }

    /// The shell picked these files for the folder the panel shows: they go there (the root's default folder when
    /// none is shown yet).
    pub fn send_files(&mut self, paths: Vec<PathBuf>) -> Activation {
        let mut a = Activation::default();
        let Some(session_id) = self.session_id() else { return a };
        if paths.is_empty() {
            return a;
        }
        let remote_dir = self.remote_folder().map(str::to_string);
        a.commands.push(Command::FilesSend { session_id, local_paths: paths, remote_dir });
        a.changed = Changed::VIEWER;
        a
    }

    fn list_folder(&mut self, path: Option<String>, a: &mut Activation) {
        if let Some(session_id) = self.session_id() {
            // A new folder has nothing cached: placeholders after 100 ms.
            self.fetch.insert(DataKind::Listing, Fetch::Pending);
            a.commands.push(Command::FilesList { session_id, path });
        }
        a.changed = Changed::VIEWER;
    }

    /// An activation of one of the panel's controls; false when `id` is not one.
    pub(crate) fn files_activate(&mut self, id: ControlId, a: &mut Activation) -> bool {
        match id {
            // "Trocar pasta": back to the root of the other computer.
            ControlId::FilesChange => self.list_folder(None, a),
            ControlId::FilesUp => {
                if let Some(up) = self.remote_folder().and_then(parent) {
                    self.list_folder(Some(up), a);
                }
            }
            ControlId::FilesRow(i) => {
                let folder = self.remote_folder().unwrap_or_default().to_string();
                let Some(entry) = self.listing.as_ref().and_then(|l| l.entries.get(usize::from(i))).cloned() else {
                    return true;
                };
                let path = join(&folder, &entry.name);
                if entry.is_dir {
                    self.list_folder(Some(path), a);
                } else if let (Some(dir), Some(session_id)) = (self.files.recv_dir.clone(), self.session_id()) {
                    a.commands.push(Command::FilesGet { session_id, remote_path: path, local_dir: dir });
                    a.changed = Changed::VIEWER;
                } else {
                    self.files.waiting_download = Some(path);
                    a.effect = Some(Effect::PickReceiveFolder);
                    a.changed = Changed::NONE;
                }
            }
            ControlId::FilesPage(next) => {
                let last = self.listing.as_ref().map_or(0, |l| l.entries.len().saturating_sub(1) / FILES_PAGE);
                self.files.page =
                    if next { (self.files.page + 1).min(last) } else { self.files.page.saturating_sub(1) };
                a.changed = Changed::VIEWER;
            }
            ControlId::FilesChoose => {
                a.effect = Some(Effect::PickFiles);
                a.changed = Changed::NONE;
            }
            ControlId::FilesRecvFolder => {
                a.effect = Some(Effect::PickReceiveFolder);
                a.changed = Changed::NONE;
            }
            ControlId::TransferResume(transfer_id) => {
                a.commands.push(Command::TransferResume { transfer_id });
                a.changed = Changed::VIEWER;
            }
            ControlId::TransferCancel(transfer_id) => {
                a.commands.push(Command::TransferCancel { transfer_id });
                self.transfers.retain(|t| t.id != transfer_id);
                a.changed = Changed::VIEWER;
            }
            _ => return false,
        }
        true
    }

    /// The panel's controls in Tab order: folder, up, entries, send, receive folder, then the transfers.
    pub(super) fn files_controls(&self, s: &mut Specs) {
        s.button(ControlId::FilesChange, self.name_of(MsgKey::ChangeFolder));
        if self.can_go_up() {
            s.button(ControlId::FilesUp, self.name_of(MsgKey::FilesUp));
        }
        if let Some(l) = &self.listing {
            for i in self.files_page_rows() {
                let e = &l.entries[i];
                let Ok(i) = u16::try_from(i) else { break };
                let verb = self.name_of(if e.is_dir { MsgKey::FilesOpen } else { MsgKey::FilesDownload });
                s.add(ControlId::FilesRow(i), UiRole::Button, format!("{verb}: {}", e.name));
            }
        }
        if let Some(l) = &self.listing
            && l.entries.len() > FILES_PAGE
        {
            let rows = self.files_page_rows();
            if rows.start > 0 {
                s.button(ControlId::FilesPage(false), self.name_of(MsgKey::FilesPrevPage));
            }
            if rows.end < l.entries.len() {
                s.button(ControlId::FilesPage(true), self.name_of(MsgKey::FilesNextPage));
            }
        }
        s.button(ControlId::FilesChoose, self.name_of(MsgKey::ChooseFiles));
        let shown = self.files.recv_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
        s.button(ControlId::FilesRecvFolder, self.tm(&Msg::new(MsgKey::FilesSaveTo).with(shown)));
        for t in self.viewer_transfers() {
            if matches!(t.state, TransferState::Paused { .. }) {
                s.button(
                    ControlId::TransferResume(t.id),
                    format!("{}: {}", self.name_of(MsgKey::TransferResume), t.name),
                );
            }
            if !matches!(t.state, TransferState::Done) {
                s.button(ControlId::TransferCancel(t.id), format!("{}: {}", self.name_of(MsgKey::Cancel), t.name));
            }
        }
    }
}

#[cfg(test)]
mod tests;
