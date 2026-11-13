//! Slice 3 of the explorer (`docs/FILES-UI.md` 4.1): the painted drag and drop inside the app.
//!
//! The shells report the pointer; the state decides everything else (what the drag carries, what is a target, what
//! the release does, the pill and its caption). The calls, in the order a gesture uses them:
//!
//! * [`UiState::drag_press`]: the left button went down; `under` is the control the painter's `hit_test` found.
//! * [`UiState::drag_move`]: the pointer moved (also while no button is down it is harmless: nothing happens without
//!   a press). `copy_key` is Ctrl on Windows and Option on the Mac, `now_ms` any monotonic clock in milliseconds.
//! * [`UiState::drag_tick`]: call about every 50 ms while [`UiState::drag_active`] (the pointer may rest on a folder:
//!   an 800 ms rest opens it). Auto-scroll uses [`drag_autoscroll`] with the list's rectangle.
//! * [`UiState::drag_release`]: the left button went up. `Some(activation)` means a drag ran or was cancelled: the
//!   shell dispatches it and must NOT activate the control under the pointer. `None` is an ordinary click.
//! * [`UiState::drag_cancel`]: focus or capture lost (Esc reaches the state through `ExplorerKey::Escape`).

use std::path::{Path, PathBuf};

use super::explorer::{idx, join};
use super::{Activation, ControlId};
use crate::api::{Command, Msg, RemoteChange};
use crate::state::{Changed, UiState};
use crate::strings::MsgKey;

/// A press followed by a move of more than this many DIPs starts a drag (FILES-UI.md 4.1).
pub const DRAG_START_DIPS: f32 = 4.0;
/// A folder row hovered this long during a drag opens (spring-loaded folders).
pub const SPRING_MS: u64 = 800;
/// The top and bottom band of a list, in DIPs, where a drag scrolls it.
pub const EDGE_DIPS: f32 = 24.0;

/// How far to scroll a list this tick while a drag is at `y`: negative up, positive down, 0 away from the edges.
/// `top` and `height` are the list's rectangle in DIPs.
pub fn drag_autoscroll(top: f32, height: f32, y: f32) -> f32 {
    let (into_top, into_bottom) = (EDGE_DIPS - (y - top), EDGE_DIPS - (top + height - y));
    let speed = |depth: f32| 2.0 + depth.clamp(0.0, EDGE_DIPS) / EDGE_DIPS * 10.0;
    if y < top || y > top + height {
        0.0
    } else if into_top > 0.0 {
        -speed(into_top)
    } else if into_bottom > 0.0 {
        speed(into_bottom)
    } else {
        0.0
    }
}

/// The drag that runs: what it carries and where the pointer is.
#[derive(Debug, Clone, PartialEq)]
struct Active {
    /// The pane the items came from, the folder they lie in (kept: the pane may open another folder meanwhile) and
    /// the items with whether each is a folder.
    there: bool,
    folder: String,
    items: Vec<(String, bool)>,
    at: (f32, f32),
    copy: bool,
    under: Option<ControlId>,
    /// The folder row under the pointer and since when (ms).
    hover: Option<(ControlId, u64)>,
    /// Size and time of each item (slice 5, `out_capture_meta`).
    meta: Vec<(u64, u64)>,
}

/// What the explorer remembers of a drag between events.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DragState {
    /// Slice 4: a drag from the system's file manager hovers the window (see `explorer_drop.rs`).
    pub(super) sys: Option<super::explorer_drop::SystemDrop>,
    /// A system drop released and waiting for the worker's `LocalKinds`.
    pub(super) drop_wait: Option<super::explorer_drop::PendingDrop>,
    press: Option<(ControlId, (f32, f32))>,
    active: Option<Active>,
    /// A drag was cancelled with the button still down: its release is not a click.
    swallow: bool,
    /// Slice 5: the drag out to the system's file manager.
    out: out::OutState,
}

/// A drag as the painter draws it: the pill under the pointer, its caption and the target to highlight.
#[derive(Debug, Clone, PartialEq)]
pub struct DragView {
    /// The first item's name, or "{n} itens".
    pub pill: String,
    /// The first item is a folder (its icon).
    pub is_dir: bool,
    /// "Mover para X", "Copiar para X" or the reason the target refuses; `None` over no target.
    pub caption: Option<String>,
    /// The caption is a reason (no highlight, drawn as information, not as an action).
    pub caption_is_reason: bool,
    /// The pointer, in DIPs.
    pub at: (f32, f32),
    /// The control to highlight: a folder row, a place or volume (`link_tint` fill, `link` outline) or a pane
    /// (`ExpPane`: its border in `link`, 2 px). `None` when the drop would do nothing or is refused.
    pub target: Option<ControlId>,
    /// The drop copies.
    pub copy: bool,
}

/// Where a drop would land.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Target {
    pub(super) there: bool,
    pub(super) folder: String,
    pub(super) label: String,
    pub(super) highlight: ControlId,
}

/// What releasing now would do.
#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    Nothing,
    Refused(String),
    Move,
    Copy,
}

/// `item` is `dest` or an ancestor of it (a folder cannot go inside itself).
fn within(there: bool, dest: &str, item: &str) -> bool {
    if there {
        dest == item || dest.strip_prefix(item).is_some_and(|r| r.starts_with('/'))
    } else {
        Path::new(dest).starts_with(item)
    }
}

fn same_folder(there: bool, a: &str, b: &str) -> bool {
    if there { a == b } else { Path::new(a) == Path::new(b) }
}

impl UiState {
    /// Whether a drag is running (the shells keep their 50 ms timer and capture while it is).
    pub fn drag_active(&self) -> bool {
        self.explorer.drag.active.is_some()
    }

    /// The left button went down on `under`.
    pub fn drag_press(&mut self, under: Option<ControlId>, at: (f32, f32)) {
        let d = &mut self.explorer.drag;
        d.swallow = false;
        d.press = match under {
            Some(id @ ControlId::ExpRow(..)) if self.explorer.open => Some((id, at)),
            _ => None,
        };
    }

    /// The pointer moved. The activation carries what a spring-loaded folder opening asks for.
    pub fn drag_move(&mut self, under: Option<ControlId>, at: (f32, f32), copy_key: bool, now_ms: u64) -> Activation {
        let mut a = Activation::default();
        if self.explorer.drag.active.is_none() {
            let Some((ControlId::ExpRow(there, n), from)) = self.explorer.drag.press else { return a };
            if (at.0 - from.0).hypot(at.1 - from.1) <= DRAG_START_DIPS {
                return a;
            }
            self.drag_start(there, usize::from(n), at);
        }
        let Some(drag) = &mut self.explorer.drag.active else { return a };
        drag.at = at;
        drag.copy = copy_key;
        drag.under = under;
        drag.hover = match (under, drag.hover) {
            (Some(ControlId::ExpRow(..)), Some((id, since))) if Some(id) == under => Some((id, since)),
            (Some(id @ ControlId::ExpRow(..)), _) => Some((id, now_ms)),
            _ => None,
        };
        self.drag_tick_into(now_ms, &mut a);
        a.changed = Changed::VIEWER;
        a
    }

    /// Time passed with the pointer where it was: a folder hovered for [`SPRING_MS`] opens.
    pub fn drag_tick(&mut self, now_ms: u64) -> Activation {
        let mut a = Activation::default();
        if self.drag_active() {
            self.drag_tick_into(now_ms, &mut a);
        }
        a
    }

    fn drag_tick_into(&mut self, now_ms: u64, a: &mut Activation) {
        let Some(drag) = &self.explorer.drag.active else { return };
        let Some((ControlId::ExpRow(there, n), since)) = drag.hover else { return };
        if now_ms.saturating_sub(since) < SPRING_MS {
            return;
        }
        let Some(target) = self.drag_target(Some(ControlId::ExpRow(there, n))) else { return };
        // Only a folder row (a file resolves to its pane), and never the folder being dragged.
        if target.highlight != ControlId::ExpRow(there, n) || matches!(self.verdict(&target), Verdict::Refused(_)) {
            return;
        }
        if let Some(d) = &mut self.explorer.drag.active {
            d.hover = None;
            d.under = None;
        }
        self.go(there, target.folder, a);
        a.changed = Changed::VIEWER;
    }

    fn drag_start(&mut self, there: bool, n: usize, at: (f32, f32)) {
        let rows: Vec<(String, bool)> =
            self.sorted_rows(there).into_iter().map(|e| (e.name.clone(), e.is_dir)).collect();
        let Some((name, _)) = rows.get(n).cloned() else {
            self.explorer.drag.press = None;
            return;
        };
        let Some(folder) = self.pane_folder(there) else {
            self.explorer.drag.press = None;
            return;
        };
        let pane = &mut self.explorer.panes[idx(there)];
        if !pane.selected.contains(&name) {
            pane.selected.clear();
            pane.selected.insert(name);
            pane.anchor = Some(n);
        }
        let items: Vec<(String, bool)> = rows.into_iter().filter(|(r, _)| pane.selected.contains(r)).collect();
        self.explorer.focus_there = there;
        self.explorer.pointer = Default::default();
        self.explorer.drag.press = None;
        self.explorer.drag.active =
            Some(Active { there, folder, items, at, copy: false, under: None, hover: None, meta: Vec::new() });
        self.out_capture_meta();
    }

    /// The control under the pointer is, or stands for, a folder this drag could land in.
    pub(super) fn drag_target(&self, under: Option<ControlId>) -> Option<Target> {
        let pane = |there: bool| -> Option<Target> {
            let folder = self.pane_folder(there)?;
            let crumbs = if there { self.crumbs_there() } else { self.crumbs_here() };
            let label = crumbs
                .last()
                .cloned()
                .unwrap_or_else(|| if there { self.peer_name() } else { self.t(MsgKey::ExplorerThisComputer) });
            Some(Target { there, folder, label, highlight: ControlId::ExpPane(there) })
        };
        match under? {
            ControlId::ExpRow(there, n) => {
                let rows = self.sorted_rows(there);
                let e = rows.get(usize::from(n))?;
                if !e.is_dir {
                    return pane(there);
                }
                let folder = self.pane_folder(there)?;
                let child = if there {
                    join(&folder, &e.name)
                } else {
                    PathBuf::from(folder).join(&e.name).to_string_lossy().into_owned()
                };
                Some(Target { there, folder: child, label: e.name.clone(), highlight: ControlId::ExpRow(there, n) })
            }
            ControlId::ExpPlace(there, i) => {
                let folder = if there {
                    self.remote_nav().get(usize::from(i))?.path.clone()
                } else {
                    self.explorer.places.get(usize::from(i))?.path.to_string_lossy().into_owned()
                };
                let label = self.explorer_places().into_iter().find(|p| p.there == there && p.index == i)?.label;
                Some(Target { there, folder, label, highlight: ControlId::ExpPlace(there, i) })
            }
            ControlId::ExpPane(there) => pane(there),
            _ => None,
        }
    }

    /// What releasing on `target` would do, and why not.
    fn verdict(&self, target: &Target) -> Verdict {
        let Some(drag) = &self.explorer.drag.active else { return Verdict::Nothing };
        if target.there == drag.there {
            if same_folder(drag.there, &target.folder, &drag.folder) {
                return Verdict::Nothing;
            }
            let into_itself = drag.items.iter().any(|(name, is_dir)| {
                let item = if drag.there {
                    join(&drag.folder, name)
                } else {
                    PathBuf::from(&drag.folder).join(name).to_string_lossy().into_owned()
                };
                *is_dir && within(drag.there, &target.folder, &item)
            });
            if into_itself {
                return Verdict::Refused(self.t(MsgKey::ExplorerMoveIntoItself));
            }
            // A copy inside one computer does not exist yet (only the move does), here or there.
            if drag.copy {
                return Verdict::Refused(self.t(MsgKey::ExplorerDragNoCopyHere));
            }
            if drag.there && !self.there_can_change() {
                let why = self
                    .there_block_reason()
                    .unwrap_or_else(|| Msg::new(MsgKey::ExplorerNeeds11).with(self.peer_name()));
                return Verdict::Refused(self.tm(&why));
            }
            return Verdict::Move;
        }
        // Across the two computers: always a copy, through the same path as the primary button.
        if self.session_id().is_none() {
            return Verdict::Nothing;
        }
        if let Some(reason) = self.there_error_reason() {
            return Verdict::Refused(reason);
        }
        if self.granted().is_some_and(|g| !g.has(crate::api::Permission::Files)) {
            return Verdict::Refused(self.tm(&Msg::new(MsgKey::ExplorerErrNoGrant).with(self.peer_name())));
        }
        if drag.items.iter().all(|(_, is_dir)| *is_dir) {
            let block = if drag.there { Some(MsgKey::ExplorerFoldersLater) } else { self.upload_folder_block() };
            if let Some(key) = block {
                return Verdict::Refused(self.tm(&Msg::new(key).with(self.peer_name())));
            }
        }
        Verdict::Copy
    }

    /// The pill, its caption and the highlighted target of the drag that runs.
    pub fn explorer_drag(&self) -> Option<DragView> {
        let Some(drag) = self.explorer.drag.active.as_ref() else { return self.system_drop_view() };
        let pill = match drag.items.as_slice() {
            [(name, _)] => name.clone(),
            many => self.tm(&Msg::new(MsgKey::ExplorerDragItems).with(many.len().to_string())),
        };
        let target = self.drag_target(drag.under);
        let (caption, caption_is_reason, highlight, copy) = match target.as_ref().map(|t| (t, self.verdict(t))) {
            None | Some((_, Verdict::Nothing)) => (None, false, None, false),
            Some((_, Verdict::Refused(why))) => (Some(why), true, None, false),
            Some((t, Verdict::Move)) => (
                Some(self.tm(&Msg::new(MsgKey::ExplorerDragMove).with(t.label.clone()))),
                false,
                Some(t.highlight),
                false,
            ),
            Some((t, Verdict::Copy)) => {
                let msg = if t.there {
                    Msg::new(MsgKey::ExplorerDragCopyThere).with(t.label.clone()).with(self.peer_name())
                } else {
                    Msg::new(MsgKey::ExplorerDragCopy).with(t.label.clone())
                };
                (Some(self.tm(&msg)), false, Some(t.highlight), true)
            }
        };
        Some(DragView {
            pill,
            is_dir: drag.items.first().is_some_and(|(_, d)| *d),
            caption,
            caption_is_reason,
            at: drag.at,
            target: highlight,
            copy,
        })
    }

    /// The left button went up. `Some`: a drag ran (or was cancelled): dispatch it and do not click. `None`: a click.
    pub fn drag_release(&mut self, under: Option<ControlId>, at: (f32, f32), copy_key: bool) -> Option<Activation> {
        let d = &mut self.explorer.drag;
        d.press = None;
        if std::mem::take(&mut d.swallow) {
            return Some(Activation::default());
        }
        let drag = d.active.as_mut()?;
        drag.at = at;
        drag.copy = copy_key;
        drag.under = under;
        let target = self.drag_target(under);
        let verdict = target.as_ref().map_or(Verdict::Nothing, |t| self.verdict(t));
        let drag = self.explorer.drag.active.take()?;
        let mut a = Activation { changed: Changed::VIEWER, ..Activation::default() };
        let (Some(target), Verdict::Move | Verdict::Copy) = (target, &verdict) else { return Some(a) };
        let names: Vec<String> = drag.items.iter().map(|(n, _)| n.clone()).collect();
        if verdict == Verdict::Move {
            if drag.there {
                if let Some(session_id) = self.session_id() {
                    self.explorer.there.notice = None;
                    a.commands.push(Command::FilesChange {
                        session_id,
                        change: RemoteChange::Move { from_dir: drag.folder, names, to_dir: target.folder },
                    });
                }
            } else {
                let paths = names.iter().map(|n| PathBuf::from(&drag.folder).join(n)).collect();
                a.commands.push(Command::LocalMove { paths, into: PathBuf::from(target.folder) });
            }
        } else {
            // The pane's own current folder is the destination the button uses (its listing is known, so the
            // conflict card can ask); another folder is named.
            let current = self.pane_folder(target.there).is_some_and(|c| same_folder(target.there, &c, &target.folder));
            let dest = (!current).then_some(target.folder);
            self.copy_items(!drag.there, drag.items, drag.folder, dest, &mut a);
        }
        Some(a)
    }

    /// Focus or capture was lost, or Esc: the drag ends and nothing changes. A release that still comes is no click.
    pub fn drag_cancel(&mut self) -> Changed {
        let d = &mut self.explorer.drag;
        let was = d.active.take().is_some();
        d.swallow = was;
        d.press = None;
        if was { Changed::VIEWER } else { Changed::NONE }
    }
}

pub mod out;

#[cfg(test)]
mod tests;
