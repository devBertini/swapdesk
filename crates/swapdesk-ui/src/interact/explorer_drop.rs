//! Slice 4 of the explorer (`docs/FILES-UI.md` 4.2): files dragged from the system's file manager into the app.
//!
//! The shells own the OS part (OLE `IDropTarget` on Windows, `NSDraggingDestination` on the Mac) and report four
//! things, always in DIPs of the window and with the control the painter's `hit_test` found under the pointer
//! (`None` over the picture of the Tela view):
//!
//! * [`UiState::system_drag_enter`]: a drag came in, with the local paths it carries ([`SystemDragData::Files`]) or [`SystemDragData::NotFiles`] (a `CFSTR_FILEDESCRIPTOR` without `CF_HDROP`, a
//!   promised file). The reply's [`DropEffect`] is what the OS is told.
//! * [`UiState::system_drag_over`]: the pointer moved or rested (OLE calls `DragOver` often even when it does not
//!   move; a folder row hovered for 800 ms opens, as in slice 3). `now_ms` is any monotonic clock in milliseconds.
//! * [`UiState::system_drag_leave`]: the drag left the window or was cancelled.
//! * [`UiState::system_drop`]: released. The reply's activation carries `Command::LocalKinds`: whether each path is
//!   a file or a folder is read by the backend's worker, never on the UI thread (a slow or network volume would freeze
//!   the window), and its answer starts the copy (`FilesSend`, or the folder upload for a 1.1 peer, or the reason in
//!   the explorer's note). The shells never touch the file system for a drop. So the hover cannot tell a folder from a
//!   file: it decides from the paths alone and the OS is told "copy" whenever the target could take files.
//!
//! The painter draws the same [`DragView`] as slice 3 (`UiState::explorer_drag`): the pill with the first dropped
//! file's name (or the count), the caption and the outline of the folder row, place, volume or pane. Over the
//! picture there is no control to outline: the view has `target: None` and a caption (`ExplorerDropOnScreen`), and
//! the painter must draw the pill and caption over the video too.

use std::path::PathBuf;

use super::explorer_drag::{DragView, SPRING_MS, Target};
use super::{Activation, ControlId};
use crate::api::{Command, Msg, Permission};
use crate::state::{Changed, UiState};
use crate::strings::MsgKey;

/// What a system drag carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemDragData {
    /// Files and folders on a disk (`CF_HDROP`, file URLs).
    Files(Vec<PathBuf>),
    /// Anything else: virtual files of a mail client or a zip folder, promised files. Refused.
    NotFiles,
}

/// What the OS is told a drop does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropEffect {
    /// `DROPEFFECT_COPY` / `NSDragOperationCopy`.
    Copy,
    /// `DROPEFFECT_NONE` / `NSDragOperationNone`.
    None,
}

/// The answer to every system-drag call.
#[derive(Debug, Clone, PartialEq)]
pub struct DropReply {
    pub effect: DropEffect,
    pub activation: Activation,
}

/// A system drag over the window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SystemDrop {
    data: SystemDragData,
    at: (f32, f32),
    under: Option<ControlId>,
    hover: Option<(ControlId, u64)>,
}

/// A drop released and waiting for the worker to say which paths are folders.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingDrop {
    /// The picture of the Tela view (otherwise the other computer's pane, in `dest`).
    picture: bool,
    /// The folder of the other computer named by the target (`None`: the pane's own current folder).
    dest: Option<String>,
}

/// Where the drop would land.
enum Place {
    Pane(Target),
    /// The picture of the Tela view: the other computer's default receive folder.
    Picture,
}

enum Verdict {
    Nothing,
    Refused(String),
    Send,
}

impl UiState {
    fn drop_place(&self, under: Option<ControlId>) -> Option<Place> {
        if self.explorer_open() {
            self.drag_target(under).map(Place::Pane)
        } else if under.is_none() && self.viewer_session().is_some() {
            Some(Place::Picture)
        } else {
            None
        }
    }

    fn drop_verdict(&self, data: &SystemDragData, place: &Place) -> Verdict {
        if self.session_id().is_none() {
            return Verdict::Nothing;
        }
        if !matches!(data, SystemDragData::Files(items) if !items.is_empty()) {
            return Verdict::Refused(self.t(MsgKey::ExplorerDiskOnly));
        }
        let no_grant = self.granted().is_some_and(|g| !g.has(Permission::Files));
        let peer = self.peer_name();
        match place {
            Place::Picture => {
                if no_grant {
                    return Verdict::Refused(self.tm(&Msg::new(MsgKey::ExplorerErrNoGrant).with(peer)));
                }
                Verdict::Send
            }
            // A copy on this computer does not exist yet (only the move does).
            Place::Pane(t) if !t.there => Verdict::Refused(self.t(MsgKey::ExplorerDragNoCopyHere)),
            Place::Pane(_) => {
                if let Some(reason) = self.there_error_reason() {
                    return Verdict::Refused(reason);
                }
                if no_grant {
                    return Verdict::Refused(self.tm(&Msg::new(MsgKey::ExplorerErrNoGrant).with(peer)));
                }
                Verdict::Send
            }
        }
    }

    fn drop_effect(&self, data: &SystemDragData, under: Option<ControlId>) -> DropEffect {
        match self.drop_place(under).map(|p| self.drop_verdict(data, &p)) {
            Some(Verdict::Send) => DropEffect::Copy,
            _ => DropEffect::None,
        }
    }

    /// A drag from the system's file manager came into the window.
    pub fn system_drag_enter(
        &mut self,
        data: SystemDragData,
        under: Option<ControlId>,
        at: (f32, f32),
        now_ms: u64,
    ) -> DropReply {
        self.explorer.drag.sys = Some(SystemDrop { data, at, under, hover: None });
        self.system_drag_over(under, at, now_ms)
    }

    /// The pointer moved, or rested, over the window.
    pub fn system_drag_over(&mut self, under: Option<ControlId>, at: (f32, f32), now_ms: u64) -> DropReply {
        let mut a = Activation::default();
        let Some(sys) = &mut self.explorer.drag.sys else {
            return DropReply { effect: DropEffect::None, activation: a };
        };
        sys.at = at;
        sys.under = under;
        sys.hover = match (under, sys.hover) {
            (Some(ControlId::ExpRow(..)), Some((id, since))) if Some(id) == under => Some((id, since)),
            (Some(id @ ControlId::ExpRow(..)), _) => Some((id, now_ms)),
            _ => None,
        };
        let hover = sys.hover;
        let data = sys.data.clone();
        a.changed = Changed::VIEWER;
        // Spring-loaded folder: only a folder row that would take the drop.
        if let Some((row @ ControlId::ExpRow(..), since)) = hover
            && now_ms.saturating_sub(since) >= SPRING_MS
            && self.explorer_open()
            && let Some(target) = self.drag_target(Some(row))
            && target.highlight == row
            && matches!(self.drop_verdict(&data, &Place::Pane(target.clone())), Verdict::Send)
        {
            if let Some(s) = &mut self.explorer.drag.sys {
                s.hover = None;
                s.under = None;
            }
            self.go(target.there, target.folder, &mut a);
            return DropReply { effect: DropEffect::None, activation: a };
        }
        DropReply { effect: self.drop_effect(&data, under), activation: a }
    }

    /// The drag left the window or was cancelled: the highlight goes.
    pub fn system_drag_leave(&mut self) -> Changed {
        if self.explorer.drag.sys.take().is_some() { Changed::VIEWER } else { Changed::NONE }
    }

    /// The drag was released over the window.
    pub fn system_drop(&mut self, under: Option<ControlId>, at: (f32, f32)) -> DropReply {
        let mut a = Activation { changed: Changed::VIEWER, ..Activation::default() };
        let none = |a| DropReply { effect: DropEffect::None, activation: a };
        let Some(mut sys) = self.explorer.drag.sys.take() else { return none(a) };
        sys.at = at;
        sys.under = under;
        let Some(place) = self.drop_place(under) else { return none(a) };
        if !matches!(self.drop_verdict(&sys.data, &place), Verdict::Send) {
            return none(a);
        }
        let SystemDragData::Files(paths) = sys.data else { return none(a) };
        let wait = match place {
            Place::Picture => PendingDrop { picture: true, dest: None },
            Place::Pane(target) => {
                // The pane's own current folder is the destination the button uses (its listing is known, so the
                // conflict card can ask); another folder is named.
                let current = self.pane_folder(true).is_some_and(|c| c == target.folder);
                PendingDrop { picture: false, dest: (!current).then_some(target.folder) }
            }
        };
        self.explorer.drag.drop_wait = Some(wait);
        a.commands.push(Command::LocalKinds { paths });
        DropReply { effect: DropEffect::Copy, activation: a }
    }

    /// The worker said which dropped paths are folders: the copy starts.
    pub(crate) fn drop_resolved(&mut self, kinds: Vec<(PathBuf, bool)>) -> Changed {
        let Some(wait) = self.explorer.drag.drop_wait.take() else { return Changed::NONE };
        let Some(session_id) = self.session_id() else { return Changed::NONE };
        let mut a = Activation { changed: Changed::VIEWER, ..Activation::default() };
        if wait.picture {
            let local_paths: Vec<PathBuf> = kinds.iter().filter(|(_, d)| !d).map(|(p, _)| p.clone()).collect();
            if local_paths.is_empty() {
                // The default receive folder is not known here, so a folder (walked into a named parent) cannot go.
                self.explorer.note = Some(MsgKey::ExplorerFoldersLater);
            } else {
                a.commands.push(Command::FilesSend { session_id, local_paths, remote_dir: None });
            }
        } else {
            let Some(src) = kinds.first().and_then(|(p, _)| p.parent()).map(PathBuf::from) else {
                return Changed::NONE;
            };
            // The items lying in the first item's folder go through the copy path (conflict card, folder walk).
            // Files from other folders (a search result) are sent as they are; folders from other folders are left
            // out (the walk takes one source folder).
            let (mut same, mut other) = (Vec::new(), Vec::new());
            for (path, is_dir) in kinds {
                let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
                if path.parent() == Some(src.as_path()) {
                    same.push((name, is_dir));
                } else if !is_dir {
                    other.push(path);
                }
            }
            let src = src.to_string_lossy().into_owned();
            self.copy_items(true, same, src, wait.dest.clone(), &mut a);
            if !other.is_empty() {
                let remote_dir = Some(wait.dest.unwrap_or_else(|| self.there_folder()));
                a.commands.push(Command::FilesSend { session_id, local_paths: other, remote_dir });
            }
        }
        self.followups.extend(a.commands);
        a.changed
    }

    /// The worker could not read the dropped paths (or timed out): nothing is copied.
    pub(crate) fn drop_failed(&mut self) -> Changed {
        self.explorer.drag.drop_wait = None;
        Changed::NONE
    }

    /// The pill, caption and target of the system drag that hovers the window (slice 4).
    pub(super) fn system_drop_view(&self) -> Option<DragView> {
        let sys = self.explorer.drag.sys.as_ref()?;
        let place = self.drop_place(sys.under);
        let verdict = place.as_ref().map_or(Verdict::Nothing, |p| self.drop_verdict(&sys.data, p));
        let (pill, is_dir) = match &sys.data {
            SystemDragData::Files(items) => match items.as_slice() {
                [one] => (one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), false),
                many => (self.tm(&Msg::new(MsgKey::ExplorerDragItems).with(many.len().to_string())), false),
            },
            SystemDragData::NotFiles => ("\u{2026}".to_string(), false),
        };
        let (caption, caption_is_reason, target, copy) = match (verdict, place) {
            (Verdict::Nothing, _) | (Verdict::Send, None) => (None, false, None, false),
            (Verdict::Refused(why), _) => (Some(why), true, None, false),
            (Verdict::Send, Some(Place::Picture)) => {
                (Some(self.tm(&Msg::new(MsgKey::ExplorerDropOnScreen).with(self.peer_name()))), false, None, true)
            }
            (Verdict::Send, Some(Place::Pane(t))) => (
                Some(self.tm(&Msg::new(MsgKey::ExplorerDragCopyThere).with(t.label).with(self.peer_name()))),
                false,
                Some(t.highlight),
                true,
            ),
        };
        Some(DragView { pill, is_dir, caption, caption_is_reason, at: sys.at, target, copy })
    }
}

#[cfg(test)]
#[path = "explorer_drop_tests.rs"]
mod tests;
