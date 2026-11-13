//! Slice 2a of the explorer (`docs/FILES-UI.md` 3.2 and 10): managing this computer's files from the left pane:
//! new folder, inline rename, cut and paste (a move, also across volumes), delete to the bin (Delete) and for good
//! (Shift+Delete, after a card), properties with a count and the volume's free space, and hidden items. Every change
//! runs on this computer's disk in this process (`local_disk`), never through the service.

use std::path::PathBuf;

use crate::api::{ApiCode, Command, DirEntryView, LocalProps, Msg};
use crate::interact::{Activation, ControlId};
use crate::state::{Changed, EditId, UiState};
use crate::strings::MsgKey;

/// A key the explorer acts on, as each shell maps it (FILES-UI.md 3.2: Windows keys and Mac keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorerKey {
    /// F2.
    Rename,
    /// Delete (Windows), Command+Backspace (Mac).
    Delete,
    /// Shift+Delete (Windows), Command+Option+Backspace (Mac).
    DeleteForever,
    /// Ctrl+Shift+N (Windows), Command+Shift+N (Mac).
    NewFolder,
    /// Alt+Enter (Windows), Command+I (Mac).
    Properties,
    /// Ctrl+X, Command+X.
    Cut,
    /// Ctrl+V, Command+V.
    Paste,
    /// Esc: closes the rename field, the card or the properties.
    Escape,
}

/// What slice 2a remembers.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Manage {
    /// The left pane's row whose name is being edited in place, and why the last name was refused.
    pub renaming: Option<String>,
    pub rename_error: Option<MsgKey>,
    /// A folder just created: its rename field opens once the listing shows it.
    pub rename_when_listed: Option<String>,
    /// What Ctrl+X took, moved by the next Ctrl+V.
    pub cut: Vec<PathBuf>,
    /// The items the "Excluir para sempre" card asks about.
    pub forever: Vec<PathBuf>,
    /// The properties card: the item's name, the path asked about and, once counted, what is inside.
    pub props: Option<(String, PathBuf, Option<LocalProps>)>,
    /// The left pane shows its hidden items.
    pub show_hidden: bool,
    /// One line for the status of the left pane: why the last change did not happen.
    pub notice: Option<MsgKey>,
}

/// The slice 2a parts of the left pane, as the screen draws them.
#[derive(Debug, Clone, PartialEq)]
pub struct ManageView {
    /// The row being renamed and the reason the last name was refused.
    pub renaming: Option<(String, Option<String>)>,
    /// The card "Excluir N itens para sempre?": its title and body.
    pub forever: Option<(String, String)>,
    /// The properties card: its title and its lines.
    pub props: Option<(String, Vec<String>)>,
    pub notice: Option<String>,
    pub show_hidden: bool,
    /// Rename and Properties act on one selected row; Delete on one or more.
    pub can_rename: bool,
    pub can_delete: bool,
    /// Slice 2b: the commands act on the focused pane. Nova pasta needs a folder shown there; Propriedades is for
    /// this computer's pane only.
    pub can_new_folder: bool,
    pub can_props: bool,
    /// The row being renamed is in the other computer's pane (`renaming` names it).
    pub renaming_there: bool,
    /// Why Nova pasta, Renomear and Excluir are disabled on the other computer's pane (needs 1.1, needs control);
    /// `delete_why` adds the volume without a bin. The description of the disabled buttons.
    pub change_why: Option<String>,
    pub delete_why: Option<String>,
    /// The card "Mandar N itens para a Lixeira do ...?" of the other computer, and the conflict card; at most one.
    pub trash_there: Option<super::explorer_remote::CardView>,
    pub conflict: Option<super::explorer_remote::CardView>,
}

impl UiState {
    /// The rows the left pane lists: its entries, and its hidden ones when they are shown.
    pub(crate) fn here_visible(&self) -> Vec<DirEntryView> {
        let Some(l) = &self.explorer.here else { return Vec::new() };
        let mut out = l.entries.clone();
        if self.explorer.manage.show_hidden {
            out.extend(l.hidden.iter().cloned());
        }
        out
    }

    /// Keeps [`crate::interact::explorer`]'s copy of the left pane's rows current (a listing, the hidden switch).
    pub(crate) fn refresh_here_rows(&mut self) {
        self.explorer.here_shown = self.here_visible();
    }

    /// The selected rows of the left pane, as paths.
    fn here_selected(&self) -> Vec<PathBuf> {
        let Some(dir) = &self.explorer.here_dir else { return Vec::new() };
        self.explorer.panes[0].selected.iter().map(|n| dir.join(n)).collect()
    }

    /// Slice 2a acts on the left pane only (the other computer's files change from slice 2b).
    fn manage_here(&self) -> bool {
        self.explorer.open && !self.explorer.focus_there && self.explorer.here_dir.is_some()
    }

    /// A key the shell mapped (see [`ExplorerKey`]).
    pub fn explorer_key(&mut self, key: ExplorerKey) -> Activation {
        let mut a = Activation::default();
        if !self.explorer.open {
            return a;
        }
        let id = match key {
            ExplorerKey::Rename => ControlId::ExpRename,
            ExplorerKey::Delete => ControlId::ExpDelete,
            ExplorerKey::NewFolder => ControlId::ExpNewFolder,
            ExplorerKey::Properties => ControlId::ExpProps,
            ExplorerKey::DeleteForever => {
                self.ask_forever(&mut a);
                return a;
            }
            ExplorerKey::Cut => {
                if self.explorer.focus_there {
                    self.there_cut();
                } else if self.manage_here() {
                    self.explorer.there.cut = None;
                    self.explorer.manage.cut = self.here_selected();
                    self.explorer.manage.notice = None;
                }
                return a;
            }
            ExplorerKey::Paste => {
                if self.explorer.focus_there {
                    self.there_paste(&mut a);
                } else {
                    self.paste(&mut a);
                }
                return a;
            }
            ExplorerKey::Escape => {
                let m = &mut self.explorer.manage;
                let here = m.renaming.take().is_some() || !m.forever.is_empty() || m.props.take().is_some();
                m.rename_error = None;
                m.forever.clear();
                if self.there_escape() || here {
                    a.changed = Changed::VIEWER;
                }
                return a;
            }
        };
        self.manage_activate(id, &mut a);
        a
    }

    fn ask_forever(&mut self, a: &mut Activation) {
        if self.manage_here() {
            let picked = self.here_selected();
            if !picked.is_empty() {
                self.explorer.manage.forever = picked;
                a.changed = Changed::VIEWER;
            }
        }
    }

    fn paste(&mut self, a: &mut Activation) {
        let Some(into) = self.explorer.here_dir.clone() else { return };
        if !self.manage_here() || self.explorer.manage.cut.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.explorer.manage.cut);
        a.commands.push(Command::LocalMove { paths, into });
    }

    /// The slice 2a controls of the explorer; false for any other control.
    pub(crate) fn manage_activate(&mut self, id: ControlId, a: &mut Activation) -> bool {
        // On the other computer's pane the same gestures are changes there (slice 2b).
        if self.explorer.focus_there
            && matches!(id, ControlId::ExpNewFolder | ControlId::ExpRename | ControlId::ExpDelete)
        {
            return self.there_activate(id, a);
        }
        match id {
            ControlId::ExpNewFolder => {
                let Some(dir) = self.explorer.here_dir.clone().filter(|_| self.manage_here()) else { return true };
                let visible = self.here_visible();
                let hidden = self.explorer.here.as_ref().map(|l| l.hidden.clone()).unwrap_or_default();
                let taken: Vec<&str> = visible.iter().chain(hidden.iter()).map(|e| e.name.as_str()).collect();
                let name = crate::local_disk::next_free_name(&self.t(MsgKey::ExplorerNewFolder), &taken);
                self.explorer.manage.rename_when_listed = Some(name.clone());
                a.commands.push(Command::LocalMakeDir { path: dir.join(name) });
            }
            ControlId::ExpRename => {
                let picked = self.here_selected();
                if self.manage_here()
                    && picked.len() == 1
                    && let Some(name) = picked[0].file_name().map(|n| n.to_string_lossy().into_owned())
                {
                    self.explorer.manage.renaming = Some(name);
                    self.explorer.manage.rename_error = None;
                    a.effect = Some(crate::state::Effect::FocusEdit(EditId::ExpRename));
                    a.changed = Changed::VIEWER;
                }
            }
            ControlId::ExpDelete => {
                // To the bin with no question: the bin is the undo (FILES-UI.md 3.2).
                let paths = self.here_selected();
                if self.manage_here() && !paths.is_empty() {
                    a.commands.push(Command::LocalTrash { paths });
                }
            }
            ControlId::ExpForeverConfirm => {
                let paths = std::mem::take(&mut self.explorer.manage.forever);
                if !paths.is_empty() {
                    a.commands.push(Command::LocalDeleteForever { paths });
                    a.changed = Changed::VIEWER;
                }
            }
            ControlId::ExpForeverCancel => {
                self.explorer.manage.forever.clear();
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpProps => {
                let picked = self.here_selected();
                let target = match picked.as_slice() {
                    [one] => Some(one.clone()),
                    [] => self.explorer.here_dir.clone(),
                    _ => None,
                };
                if let Some(path) = target.filter(|_| self.manage_here()) {
                    let name = path
                        .file_name()
                        .map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
                    self.explorer.manage.props = Some((name, path.clone(), None));
                    a.commands.push(Command::LocalProps { path });
                    a.changed = Changed::VIEWER;
                }
            }
            ControlId::ExpPropsClose => {
                self.explorer.manage.props = None;
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpHidden => {
                self.explorer.manage.show_hidden = !self.explorer.manage.show_hidden;
                self.explorer.panes[0].anchor = None;
                self.refresh_here_rows();
                a.changed = Changed::VIEWER;
            }
            _ => return self.remote_activate(id, a),
        }
        true
    }

    /// The inline rename field was committed with `text` (Enter, or a click elsewhere).
    pub(crate) fn submit_explorer_rename(&mut self, text: String, a: &mut Activation) {
        if self.explorer.there.renaming.is_some() {
            self.submit_there_rename(text, a);
            return;
        }
        let Some(old) = self.explorer.manage.renaming.clone() else { return };
        let Some(dir) = self.explorer.here_dir.clone() else { return };
        let name = text.trim_end_matches(['\r', '\n']).to_owned();
        a.changed = Changed::VIEWER;
        if name == old {
            self.explorer.manage.renaming = None;
            return;
        }
        if !crate::local_disk::name_ok(&name) {
            self.explorer.manage.rename_error = Some(MsgKey::ExplorerNameInvalid);
            return;
        }
        a.commands.push(Command::LocalRename { path: dir.join(&old), name });
    }

    /// A slice 2a command was done: the listing is read again, and a new folder opens its rename field.
    pub(crate) fn manage_done(&mut self, command: &Command) -> Changed {
        match command {
            Command::LocalRename { .. } => {
                self.explorer.manage.renaming = None;
                self.explorer.manage.rename_error = None;
                self.explorer.panes[0].selected.clear();
            }
            Command::LocalTrash { .. } | Command::LocalDeleteForever { .. } | Command::LocalMove { .. } => {
                self.explorer.panes[0].selected.clear();
                self.explorer.manage.notice = None;
                // A replace waits for the bin to take what it replaces (FILES-UI.md 5).
                self.held_after(command, true);
            }
            Command::LocalMakeDir { .. } => {}
            _ => return Changed::NONE,
        }
        let dir = self.explorer.here_dir.clone();
        self.followups.push(Command::LocalList { path: dir });
        Changed::VIEWER
    }

    /// A slice 2a command failed with `code`: the rename field stays open with the reason, any other change says why
    /// in the left pane's status.
    pub(crate) fn manage_failed(&mut self, command: &Command, code: ApiCode) -> Changed {
        self.held_after(command, false);
        let reason = match (command, code) {
            (Command::LocalRename { .. }, ApiCode::Refused) => {
                self.explorer.manage.rename_error = Some(MsgKey::ExplorerNameTaken);
                return Changed::VIEWER;
            }
            (Command::LocalRename { .. }, ApiCode::Invalid) => {
                self.explorer.manage.rename_error = Some(MsgKey::ExplorerNameInvalid);
                return Changed::VIEWER;
            }
            (Command::LocalTrash { .. }, ApiCode::Refused) => MsgKey::ExplorerNoBin,
            (Command::LocalMove { .. }, ApiCode::Invalid) => MsgKey::ExplorerMoveIntoItself,
            (Command::LocalMove { .. } | Command::LocalMakeDir { .. }, ApiCode::Refused) => MsgKey::ExplorerNameTaken,
            (_, ApiCode::NotPermitted) => MsgKey::ExplorerNotPermitted,
            (_, ApiCode::NotFound) => MsgKey::ExplorerGone,
            (
                Command::LocalMakeDir { .. }
                | Command::LocalTrash { .. }
                | Command::LocalDeleteForever { .. }
                | Command::LocalMove { .. }
                | Command::LocalProps { .. },
                _,
            ) => MsgKey::ExplorerOpFailed,
            _ => return Changed::NONE,
        };
        if matches!(command, Command::LocalMakeDir { .. }) {
            self.explorer.manage.rename_when_listed = None;
        }
        if matches!(command, Command::LocalProps { .. }) {
            self.explorer.manage.props = None;
        }
        self.explorer.manage.notice = Some(reason);
        Changed::VIEWER
    }

    /// The left pane was listed again: a folder just created gets its rename field (the shells focus a rename slot
    /// as it appears, with its name selected).
    pub(crate) fn manage_listed(&mut self) {
        let Some(name) = self.explorer.manage.rename_when_listed.take() else { return };
        if self.explorer.here_shown.iter().any(|e| e.name == name) {
            self.explorer.panes[0].selected = std::iter::once(name.clone()).collect();
            self.explorer.manage.renaming = Some(name);
            self.explorer.manage.rename_error = None;
        }
    }

    /// What `LocalProps` counted, for the card still open on that item.
    pub(crate) fn props_counted(&mut self, props: LocalProps) -> Changed {
        // Only the count of the item the card is open on: a reply to an older request is moot.
        let Some((_, asked, slot)) = &mut self.explorer.manage.props else { return Changed::NONE };
        if *asked != props.path {
            return Changed::NONE;
        }
        *slot = Some(props);
        Changed::VIEWER
    }

    /// The slice 2a state of the left pane, as the screen draws it.
    pub fn explorer_manage(&self) -> ManageView {
        let m = &self.explorer.manage;
        let focus_there = self.explorer.focus_there;
        let n = self.explorer.panes[usize::from(focus_there)].selected.len();
        let here = self.manage_here();
        let there_ok = self.there_can_change();
        let there = &self.explorer.there;
        let forever = (!m.forever.is_empty()).then(|| {
            let count = m.forever.len().to_string();
            (self.tm(&Msg::new(MsgKey::ExplorerForeverTitle).with(count)), self.t(MsgKey::ExplorerForeverBody))
        });
        let props = m.props.as_ref().map(|(name, _, counted)| {
            let title = self.tm(&Msg::new(MsgKey::ExplorerPropsTitle).with(name.clone()));
            let lines = match counted {
                None => vec![self.t(MsgKey::ExplorerPropsCounting)],
                Some(p) => {
                    let size = super::explorer::human_size(self.lang, p.bytes);
                    let mut lines = vec![
                        self.tm(&Msg::new(MsgKey::ExplorerPropsContents)
                            .with(p.files.to_string())
                            .with(p.folders.to_string())
                            .with(size)),
                    ];
                    if p.truncated {
                        lines.push(self.tm(
                            &Msg::new(MsgKey::ExplorerPropsMore).with(crate::local_disk::MAX_PROPS_ENTRIES.to_string()),
                        ));
                    }
                    if let Some(free) = p.free_bytes {
                        lines.push(self.tm(
                            &Msg::new(MsgKey::ExplorerPropsFree).with(super::explorer::human_size(self.lang, free)),
                        ));
                    }
                    lines
                }
            };
            (title, lines)
        });
        let renaming = match (&there.renaming, &m.renaming) {
            (Some(r), _) => Some((r.clone(), there.rename_error.map(|k| self.t(k)))),
            (None, Some(r)) => Some((r.clone(), m.rename_error.map(|k| self.t(k)))),
            (None, None) => None,
        };
        let change_why = if focus_there { self.there_block_reason().map(|r| self.tm(&r)) } else { None };
        let delete_why = if focus_there {
            change_why
                .clone()
                .or_else(|| (there_ok && n > 0 && !self.there_has_bin()).then(|| self.t(MsgKey::ExplorerNoBin)))
        } else {
            None
        };
        ManageView {
            renaming,
            renaming_there: there.renaming.is_some(),
            can_new_folder: if focus_there { there_ok } else { true },
            can_props: here,
            change_why,
            delete_why,
            trash_there: self.there_trash_card(),
            conflict: self.conflict_card(),
            forever,
            props,
            notice: m.notice.map(|k| self.t(k)),
            show_hidden: m.show_hidden,
            can_rename: if focus_there { there_ok && n == 1 } else { here && n == 1 },
            can_delete: if focus_there { there_ok && n > 0 && self.there_has_bin() } else { here && n > 0 },
        }
    }
}
