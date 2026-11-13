//! Slice 3 of the explorer (`docs/FILES-UI.md` 3.2): the context menu, drawn by each system's own menu.
//!
//! The state owns the list (one order and one set of texts for both systems, items that cannot act now disabled and
//! never hidden); the shells show it with `Effect::ContextMenu`, at the pointer. Calls: [`UiState::context_menu`]
//! (the right click, Control+click, the Menu key or Shift+F10), then [`UiState::context_items`] for the labels and
//! whether each is enabled, then [`UiState::context_chosen`] with the index, or [`UiState::context_closed`].

use std::path::PathBuf;

use super::explorer::{idx, join};
use super::explorer_manage::ExplorerKey;
use super::{Activation, ControlId};
use crate::api::{Command, Msg, RemoteChange};
use crate::state::{Changed, Effect, UiState};
use crate::strings::MsgKey;

/// What the menu was opened on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MenuTarget {
    /// A row (selected by the click when it was not) and whether it is a folder.
    Row(String, bool),
    /// The empty part of a list: the pane's current folder.
    Pane,
    /// A place or volume of the navigation pane.
    Place(u8),
}

/// The open menu: the pane it belongs to and what it was opened on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Menu {
    pub there: bool,
    pub target: MenuTarget,
}

/// The items, in the order both systems show them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Open,
    CopyAcross,
    Cut,
    Paste,
    Rename,
    NewFolder,
    Delete,
    Properties,
    Refresh,
    Reveal,
}

const ORDER: [Item; 10] = [
    Item::Open,
    Item::CopyAcross,
    Item::Cut,
    Item::Paste,
    Item::Rename,
    Item::NewFolder,
    Item::Delete,
    Item::Properties,
    Item::Refresh,
    Item::Reveal,
];

impl UiState {
    /// A context menu was asked for on `under` (the control the painter's `hit_test` found). `from_keyboard`: the Menu
    /// key or Shift+F10, which has no pointer: with no explorer control under it the menu opens on the focused pane's
    /// empty area. The activation carries `Effect::ContextMenu` when there is a menu to show.
    pub fn context_menu(&mut self, under: Option<ControlId>, from_keyboard: bool) -> Activation {
        let mut a = Activation::default();
        if !self.explorer.open {
            return a;
        }
        let under = match under {
            Some(ControlId::ExpRow(..) | ControlId::ExpPane(_) | ControlId::ExpPlace(..)) => under,
            _ if from_keyboard => Some(ControlId::ExpPane(self.explorer.focus_there)),
            _ => return a,
        };
        let menu = match under {
            Some(ControlId::ExpRow(there, n)) => {
                let rows: Vec<(String, bool)> =
                    self.sorted_rows(there).into_iter().map(|e| (e.name.clone(), e.is_dir)).collect();
                let Some((name, is_dir)) = rows.get(usize::from(n)).cloned() else { return a };
                let pane = &mut self.explorer.panes[idx(there)];
                if !pane.selected.contains(&name) {
                    pane.selected.clear();
                    pane.selected.insert(name.clone());
                    pane.anchor = Some(usize::from(n));
                }
                Menu { there, target: MenuTarget::Row(name, is_dir) }
            }
            Some(ControlId::ExpPlace(there, i)) => Menu { there, target: MenuTarget::Place(i) },
            Some(ControlId::ExpPane(there)) => {
                let pane = &mut self.explorer.panes[idx(there)];
                pane.selected.clear();
                pane.anchor = None;
                Menu { there, target: MenuTarget::Pane }
            }
            _ => return a,
        };
        self.explorer.focus_there = menu.there;
        self.explorer.menu = Some(menu);
        a.effect = Some(Effect::ContextMenu);
        a.changed = Changed::VIEWER;
        a
    }

    /// The labels of the open menu and whether each can act now (disabled ones stay in the list).
    pub fn context_items(&self) -> Vec<(String, bool)> {
        let Some(menu) = &self.explorer.menu else { return Vec::new() };
        ORDER.iter().map(|&i| (self.item_label(menu, i), self.item_enabled(menu, i))).collect()
    }

    /// The menu was dismissed with nothing chosen.
    pub fn context_closed(&mut self) {
        self.explorer.menu = None;
    }

    fn item_label(&self, menu: &Menu, item: Item) -> String {
        match item {
            Item::Open => self.t(MsgKey::ExplorerOpen),
            Item::CopyAcross if menu.there => self.t(MsgKey::ExplorerCopyHere),
            Item::CopyAcross => self.tm(&Msg::new(MsgKey::ExplorerCopyTo).with(self.peer_name())),
            Item::Cut => self.t(MsgKey::ExplorerCut),
            Item::Paste => self.t(MsgKey::ExplorerPaste),
            Item::Rename => self.t(MsgKey::ExplorerRename),
            Item::NewFolder => self.t(MsgKey::ExplorerNewFolder),
            Item::Delete => self.t(MsgKey::ExplorerDelete),
            Item::Properties => self.t(MsgKey::ExplorerProps),
            Item::Refresh => self.t(MsgKey::ExplorerRefresh),
            Item::Reveal if cfg!(target_os = "macos") => self.t(MsgKey::ExplorerRevealMac),
            Item::Reveal => self.t(MsgKey::ExplorerRevealWindows),
        }
    }

    /// The folder a paste from this menu goes into: the folder row, the place, else the pane's own folder.
    fn menu_folder(&self, menu: &Menu) -> Option<String> {
        let there = menu.there;
        match &menu.target {
            MenuTarget::Row(name, true) => {
                let base = self.pane_folder(there)?;
                Some(if there {
                    join(&base, name)
                } else {
                    PathBuf::from(base).join(name).to_string_lossy().into_owned()
                })
            }
            MenuTarget::Place(i) => {
                if there {
                    self.remote_nav().get(usize::from(*i)).map(|n| n.path.clone())
                } else {
                    self.explorer.places.get(usize::from(*i)).map(|p| p.path.to_string_lossy().into_owned())
                }
            }
            MenuTarget::Row(_, false) | MenuTarget::Pane => self.pane_folder(there),
        }
    }

    /// The local path of the item a "show in the file manager" would reveal.
    fn menu_local_path(&self, menu: &Menu) -> Option<PathBuf> {
        if menu.there {
            return None;
        }
        match &menu.target {
            MenuTarget::Row(name, _) => self.explorer.here_dir.as_ref().map(|d| d.join(name)),
            MenuTarget::Pane => self.explorer.here_dir.clone(),
            MenuTarget::Place(i) => self.explorer.places.get(usize::from(*i)).map(|p| p.path.clone()),
        }
    }

    fn item_enabled(&self, menu: &Menu, item: Item) -> bool {
        let there = menu.there;
        let selected = self.explorer.panes[idx(there)].selected.len();
        let row = matches!(menu.target, MenuTarget::Row(..));
        let can_change = if there { self.there_can_change() } else { self.explorer.here_dir.is_some() };
        match item {
            Item::Open => match &menu.target {
                MenuTarget::Row(_, is_dir) => selected == 1 && (*is_dir || !there),
                MenuTarget::Place(_) => true,
                MenuTarget::Pane => false,
            },
            Item::CopyAcross => row && self.copy_has_something(),
            Item::Cut => row && can_change,
            Item::Paste => {
                let Some(dest) = self.menu_folder(menu) else { return false };
                if there {
                    can_change && self.explorer.there.cut.as_ref().is_some_and(|(from, _)| *from != dest)
                } else {
                    can_change && !self.explorer.manage.cut.is_empty()
                }
            }
            Item::Rename => row && selected == 1 && can_change,
            Item::NewFolder => !matches!(menu.target, MenuTarget::Place(_)) && can_change,
            Item::Delete => row && selected > 0 && can_change && (!there || self.there_has_bin()),
            Item::Properties => !there && (menu.target == MenuTarget::Pane || (row && selected == 1)) && can_change,
            Item::Refresh => true,
            Item::Reveal => self.menu_local_path(menu).is_some(),
        }
    }

    /// Item `index` of the menu was chosen: the same action as the command bar, the keys or a drop.
    pub fn context_chosen(&mut self, index: usize) -> Activation {
        let mut a = Activation::default();
        let Some(menu) = self.explorer.menu.take() else { return a };
        let Some(&item) = ORDER.get(index) else { return a };
        if !self.item_enabled(&menu, item) {
            return a;
        }
        let there = menu.there;
        self.explorer.focus_there = there;
        match item {
            Item::Open => match &menu.target {
                MenuTarget::Row(name, is_dir) => self.open_row(there, name, *is_dir, &mut a),
                MenuTarget::Place(i) => {
                    self.explorer_activate(ControlId::ExpPlace(there, *i), &mut a);
                }
                MenuTarget::Pane => {}
            },
            Item::CopyAcross => self.explorer_copy(&mut a),
            Item::Cut => return self.explorer_key(ExplorerKey::Cut),
            Item::Paste => {
                if let (Some(dest), Some(session_id)) = (self.menu_folder(&menu), self.session_id()) {
                    if there {
                        if let Some((from_dir, names)) = self.explorer.there.cut.take() {
                            a.commands.push(Command::FilesChange {
                                session_id,
                                change: RemoteChange::Move { from_dir, names, to_dir: dest },
                            });
                        }
                    } else {
                        let paths = std::mem::take(&mut self.explorer.manage.cut);
                        a.commands.push(Command::LocalMove { paths, into: PathBuf::from(dest) });
                    }
                }
            }
            Item::Rename => {
                self.manage_activate(ControlId::ExpRename, &mut a);
            }
            Item::NewFolder => {
                self.manage_activate(ControlId::ExpNewFolder, &mut a);
            }
            Item::Delete => {
                self.manage_activate(ControlId::ExpDelete, &mut a);
            }
            Item::Properties => {
                self.manage_activate(ControlId::ExpProps, &mut a);
            }
            Item::Refresh => {
                self.explorer_activate(ControlId::ExpRefresh(there), &mut a);
            }
            Item::Reveal => {
                if let Some(path) = self.menu_local_path(&menu) {
                    self.local_target = Some(path);
                    a.effect = Some(Effect::RevealLocal);
                }
            }
        }
        a.changed = Changed::VIEWER;
        a
    }
}
