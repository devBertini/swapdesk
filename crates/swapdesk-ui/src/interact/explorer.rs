//! The files explorer of the session window (`docs/FILES-UI.md`, slice 1): the Arquivos view with this computer on the
//! left and the other computer on the right, each pane with back, forward and up, its places, a sorted details list,
//! selection, and copy across with the primary button. It replaces the drawer's file panel.
//!
//! The left pane lists this computer's disk through the backend (`Command::LocalList`, read by a worker thread of the
//! interface, never the service); the right pane lists the other computer through `FilesList` (its home folder and
//! below, as 1.0 peers allow). A copy to the other computer is `FilesSend` of the selected files into the right pane's
//! folder; a copy here is `FilesGet` of the selected files into the left pane's folder. Folders are not copied yet
//! (a later slice walks them).

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{ControlId, Specs, UiRole};
use crate::api::{ApiCode, Command, DirEntryView, LocalListing, LocalPlace, Msg, PlaceKind};
use crate::state::{Activation, Changed, DataKind, Effect, Fetch, UiState};
use crate::strings::MsgKey;

/// Sort columns, as `ControlId::ExpSort` numbers them.
pub const SORT_NAME: u8 = 0;
pub const SORT_SIZE: u8 = 1;
pub const SORT_TYPE: u8 = 2;
pub const SORT_MODIFIED: u8 = 3;

/// The other computer's places, as paths from its home folder (FILES-UI.md 2.3): the home folder, Desktop,
/// Documents, Downloads and Swapdesk's receive folder. Both systems name these folders in English on disk.
const REMOTE_PLACES: [(&str, MsgKey); 5] = [
    ("", MsgKey::ExplorerPlacePersonal),
    ("Desktop", MsgKey::ExplorerPlaceDesktop),
    ("Documents", MsgKey::ExplorerPlaceDocuments),
    ("Downloads", MsgKey::ExplorerPlaceDownloads),
    ("Downloads/Swapdesk", MsgKey::ExplorerPlaceReceived),
];

/// The modifiers and click count of the pointer press the shell is about to report as an activation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerMods {
    /// Ctrl on Windows, Command on the Mac: add or remove one row.
    pub toggle: bool,
    /// Shift: extend from the anchor.
    pub extend: bool,
    /// 2 for a double click.
    pub clicks: u8,
}

/// One pane's selection, sort and history.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PaneState {
    pub selected: BTreeSet<String>,
    pub anchor: Option<usize>,
    /// Column and ascending.
    pub sort: (u8, bool),
    pub back: Vec<String>,
    pub forward: Vec<String>,
}

impl PaneState {
    fn new() -> Self {
        Self { sort: (SORT_NAME, true), ..Self::default() }
    }
}

/// What the explorer remembers between events.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Explorer {
    pub open: bool,
    /// The left pane: this computer's folder, its listing and whether one is on its way.
    pub here_dir: Option<PathBuf>,
    pub here: Option<LocalListing>,
    pub here_error: Option<ApiCode>,
    pub here_loading: bool,
    pub places: Vec<LocalPlace>,
    pub panes: [PaneState; 2],
    /// The pane that has the keyboard: `false` this computer, `true` the other one.
    pub focus_there: bool,
    pub transfers_open: bool,
    pub pointer: PointerMods,
    /// A note for the status of the copy (a folder was left out), shown until the next copy.
    pub note: Option<MsgKey>,
}

impl Default for Explorer {
    fn default() -> Self {
        Self {
            open: false,
            here_dir: None,
            here: None,
            here_error: None,
            here_loading: false,
            places: Vec::new(),
            panes: [PaneState::new(), PaneState::new()],
            focus_there: false,
            transfers_open: false,
            pointer: PointerMods::default(),
            note: None,
        }
    }
}

/// A row of a pane as the screen draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub name: String,
    pub is_dir: bool,
    pub size: String,
    pub kind: String,
    pub modified: String,
    pub selected: bool,
}

/// One pane as the screen draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneView {
    pub there: bool,
    pub title: String,
    /// The folder, as the breadcrumb shows it: the place's name, then each folder.
    pub crumbs: Vec<String>,
    pub rows: Vec<RowView>,
    pub loading: bool,
    /// The error state: its title and reason.
    pub error: Option<(String, String)>,
    pub status: String,
    pub truncated: Option<String>,
    pub can_back: bool,
    pub can_forward: bool,
    pub can_up: bool,
    pub sort: (u8, bool),
    pub focused: bool,
}

/// A place or volume of the navigation pane.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceView {
    pub there: bool,
    pub index: u8,
    pub label: String,
    pub volume: bool,
    pub selected: bool,
}

/// Natural order, case-insensitive: a number inside a name compares by value ("foto 2" before "foto 10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut n = String::new();
                while let Some(c) = x.peek().copied().filter(char::is_ascii_digit) {
                    n.push(c);
                    x.next();
                }
                let mut m = String::new();
                while let Some(d) = y.peek().copied().filter(char::is_ascii_digit) {
                    m.push(d);
                    y.next();
                }
                let (n, m) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                let o = n.len().cmp(&m.len()).then_with(|| n.cmp(m));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = fold(c).cmp(&fold(d));
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

/// A character for comparing names: lower case, the accents of Portuguese and the common Latin ones removed.
fn fold(c: char) -> char {
    let c = c.to_lowercase().next().unwrap_or(c);
    match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        other => other,
    }
}

/// The extension of a file name, upper case, without the dot.
fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty() && ext.len() <= 8).then(|| ext.to_uppercase())
}

/// The Type column (FILES-UI.md 2.4): one table for both systems.
pub fn type_of(lang: crate::api::Language, name: &str, is_dir: bool) -> String {
    use crate::strings::text;
    if is_dir {
        return text(lang, &Msg::new(MsgKey::ExplorerTypeFolder));
    }
    let Some(ext) = extension(name) else { return text(lang, &Msg::new(MsgKey::ExplorerTypeFile)) };
    let key = match ext.as_str() {
        "PDF" => return text(lang, &Msg::new(MsgKey::ExplorerTypePdf)),
        "TXT" | "MD" | "RTF" => return text(lang, &Msg::new(MsgKey::ExplorerTypeText)),
        "EXE" | "MSI" | "APP" | "DMG" | "PKG" => return text(lang, &Msg::new(MsgKey::ExplorerTypeApp)),
        "PNG" | "JPG" | "JPEG" | "GIF" | "BMP" | "HEIC" | "WEBP" | "TIFF" | "SVG" => MsgKey::ExplorerTypeImage,
        "MP4" | "MOV" | "MKV" | "AVI" | "WEBM" | "M4V" => MsgKey::ExplorerTypeVideo,
        "MP3" | "WAV" | "M4A" | "FLAC" | "AAC" | "OGG" => MsgKey::ExplorerTypeAudio,
        "ZIP" | "RAR" | "7Z" | "TAR" | "GZ" | "TGZ" | "XZ" => MsgKey::ExplorerTypeArchive,
        _ => MsgKey::ExplorerTypeExt,
    };
    text(lang, &Msg::new(key).with(ext))
}

/// `entries` in the pane's order: folders first, then by the sorted column.
fn sorted(entries: &[DirEntryView], sort: (u8, bool)) -> Vec<&DirEntryView> {
    let mut v: Vec<&DirEntryView> = entries.iter().collect();
    v.sort_by(|a, b| {
        let dirs = b.is_dir.cmp(&a.is_dir);
        let by = match sort.0 {
            SORT_SIZE => a.size.cmp(&b.size),
            SORT_TYPE => extension(&a.name).cmp(&extension(&b.name)),
            SORT_MODIFIED => a.mtime_ms.cmp(&b.mtime_ms),
            _ => Ordering::Equal,
        }
        .then_with(|| natural_cmp(&a.name, &b.name));
        dirs.then(if sort.1 { by } else { by.reverse() })
    });
    v
}

/// `parent/name` with `/`, the empty path being the root.
fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() { name.to_string() } else { format!("{folder}/{name}") }
}

fn parent(folder: &str) -> Option<String> {
    (!folder.is_empty()).then(|| folder.rsplit_once('/').map(|(up, _)| up.to_string()).unwrap_or_default())
}

fn idx(there: bool) -> usize {
    usize::from(there)
}

impl UiState {
    /// The Arquivos view is open in the session window (the picture is hidden while it is).
    pub fn explorer_open(&self) -> bool {
        self.explorer.open && self.viewer_session().is_some()
    }

    /// The shell says which modifiers and how many clicks the press it reports next had (Ctrl or Command, Shift,
    /// double click). The explorer's rows read it; it is forgotten after the activation.
    pub fn set_pointer_mods(&mut self, mods: PointerMods) {
        self.explorer.pointer = mods;
    }

    fn here_entries(&self) -> &[DirEntryView] {
        self.explorer.here.as_ref().map_or(&[], |l| &l.entries)
    }

    fn there_entries(&self) -> &[DirEntryView] {
        self.listing.as_ref().map_or(&[], |l| &l.entries)
    }

    fn entries(&self, there: bool) -> &[DirEntryView] {
        if there { self.there_entries() } else { self.here_entries() }
    }

    /// The rows of a pane in their order (what `ExpRow(there, n)` numbers).
    fn sorted_rows(&self, there: bool) -> Vec<&DirEntryView> {
        sorted(self.entries(there), self.explorer.panes[idx(there)].sort)
    }

    /// The other computer's folder the right pane shows ("" its home folder).
    fn there_folder(&self) -> String {
        self.listing.as_ref().map(|l| l.path.clone()).unwrap_or_default()
    }

    /// Opens the view and lists both panes the first time (R-UI-11: nothing is made before).
    pub(crate) fn explorer_toggle(&mut self, a: &mut Activation) {
        if self.explorer.open {
            self.explorer.open = false;
            a.changed = Changed::VIEWER;
            return;
        }
        self.explorer.open = true;
        self.drawer = None;
        if self.explorer.here.is_none() && !self.explorer.here_loading {
            self.list_here(None, a);
        }
        if self.explorer.places.is_empty() {
            a.commands.push(Command::LocalPlaces);
        }
        if self.listing.is_none()
            && self.fetch(DataKind::Listing) == Fetch::Missing
            && let Some(session_id) = self.session_id()
        {
            self.fetch.insert(DataKind::Listing, Fetch::Pending);
            a.commands.push(Command::FilesList { session_id, path: None });
        }
        a.changed = Changed::VIEWER;
    }

    fn list_here(&mut self, path: Option<PathBuf>, a: &mut Activation) {
        self.explorer.here_loading = true;
        self.explorer.here_error = None;
        a.commands.push(Command::LocalList { path });
        a.changed = Changed::VIEWER;
    }

    fn list_there(&mut self, path: Option<String>, a: &mut Activation) {
        if let Some(session_id) = self.session_id() {
            self.fetch.insert(DataKind::Listing, Fetch::Pending);
            a.commands.push(Command::FilesList { session_id, path });
        }
        a.changed = Changed::VIEWER;
    }

    /// Goes to `folder` in a pane, keeping the history (back, forward).
    fn go(&mut self, there: bool, folder: String, a: &mut Activation) {
        let current = if there {
            self.listing.as_ref().map(|l| l.path.clone())
        } else {
            self.explorer.here_dir.as_ref().map(|d| d.to_string_lossy().into_owned())
        };
        let pane = &mut self.explorer.panes[idx(there)];
        if let Some(c) = current
            && c != folder
        {
            pane.back.push(c);
            pane.forward.clear();
        }
        self.open_folder(there, folder, a);
    }

    fn open_folder(&mut self, there: bool, folder: String, a: &mut Activation) {
        let pane = &mut self.explorer.panes[idx(there)];
        pane.selected.clear();
        pane.anchor = None;
        if there {
            self.list_there(Some(folder), a);
        } else {
            self.list_here(Some(PathBuf::from(folder)), a);
        }
    }

    /// The reply to `LocalList`.
    pub(crate) fn here_listed(&mut self, l: LocalListing) -> Changed {
        self.explorer.here_dir = Some(l.path.clone());
        self.explorer.here = Some(l);
        self.explorer.here_loading = false;
        self.explorer.here_error = None;
        Changed::VIEWER
    }

    pub(crate) fn here_failed(&mut self, code: ApiCode) -> Changed {
        self.explorer.here_loading = false;
        self.explorer.here_error = Some(code);
        Changed::VIEWER
    }

    pub(crate) fn places_listed(&mut self, places: Vec<LocalPlace>) -> Changed {
        self.explorer.places = places;
        Changed::VIEWER
    }

    /// A click (or Enter) on row `n` of a pane: select it as the modifiers say; a double click opens it.
    fn row_activated(&mut self, there: bool, n: usize, a: &mut Activation) {
        let mods = std::mem::take(&mut self.explorer.pointer);
        let rows: Vec<(String, bool)> =
            self.sorted_rows(there).into_iter().map(|e| (e.name.clone(), e.is_dir)).collect();
        let Some((name, is_dir)) = rows.get(n).cloned() else { return };
        self.explorer.focus_there = there;
        a.changed = Changed::VIEWER;
        if mods.clicks >= 2 {
            self.open_row(there, &name, is_dir, a);
            return;
        }
        let pane = &mut self.explorer.panes[idx(there)];
        if mods.extend {
            let from = pane.anchor.unwrap_or(n);
            let (lo, hi) = (from.min(n), from.max(n));
            if !mods.toggle {
                pane.selected.clear();
            }
            pane.selected.extend(rows[lo..=hi].iter().map(|(s, _)| s.clone()));
        } else if mods.toggle {
            if !pane.selected.remove(&name) {
                pane.selected.insert(name);
            }
            pane.anchor = Some(n);
        } else {
            pane.selected.clear();
            pane.selected.insert(name);
            pane.anchor = Some(n);
        }
    }

    fn open_row(&mut self, there: bool, name: &str, is_dir: bool, a: &mut Activation) {
        if there {
            if is_dir {
                let folder = join(&self.there_folder(), name);
                self.go(true, folder, a);
            } else {
                self.explorer.note = Some(MsgKey::ExplorerOpenThere);
            }
            return;
        }
        let Some(dir) = self.explorer.here_dir.clone() else { return };
        let path = dir.join(name);
        if is_dir {
            self.go(false, path.to_string_lossy().into_owned(), a);
        } else {
            self.local_target = Some(path);
            a.effect = Some(Effect::OpenLocal);
        }
    }

    /// "Copiar para ...": the focused pane's selected files go to the other pane's folder.
    fn explorer_copy(&mut self, a: &mut Activation) {
        let Some(session_id) = self.session_id() else { return };
        let there = self.explorer.focus_there;
        let picked: Vec<(String, bool)> = self
            .sorted_rows(there)
            .into_iter()
            .filter(|e| self.explorer.panes[idx(there)].selected.contains(&e.name))
            .map(|e| (e.name.clone(), e.is_dir))
            .collect();
        self.explorer.note = picked.iter().any(|(_, d)| *d).then_some(MsgKey::ExplorerFoldersLater);
        let files: Vec<String> = picked.into_iter().filter(|(_, d)| !d).map(|(n, _)| n).collect();
        if files.is_empty() {
            a.changed = Changed::VIEWER;
            return;
        }
        if there {
            let Some(local_dir) = self.explorer.here_dir.clone() else { return };
            let folder = self.there_folder();
            for name in files {
                a.commands.push(Command::FilesGet {
                    session_id,
                    remote_path: join(&folder, &name),
                    local_dir: local_dir.clone(),
                });
            }
        } else {
            let Some(dir) = self.explorer.here_dir.clone() else { return };
            let local_paths = files.iter().map(|n| dir.join(n)).collect();
            a.commands.push(Command::FilesSend { session_id, local_paths, remote_dir: Some(self.there_folder()) });
        }
        self.explorer.transfers_open = true;
        a.changed = Changed::VIEWER;
    }

    /// The local file the last `Effect::OpenLocal` names, once.
    pub fn take_local_target(&mut self) -> Option<PathBuf> {
        self.local_target.take()
    }

    /// An activation of one of the explorer's controls; false when `id` is not one.
    pub(crate) fn explorer_activate(&mut self, id: ControlId, a: &mut Activation) -> bool {
        match id {
            ControlId::ExpTela => {
                self.explorer.open = false;
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpCopy => self.explorer_copy(a),
            ControlId::ExpRow(there, n) => self.row_activated(there, usize::from(n), a),
            ControlId::ExpSort(there, col) => {
                let pane = &mut self.explorer.panes[idx(there)];
                pane.sort = if pane.sort.0 == col { (col, !pane.sort.1) } else { (col, true) };
                pane.anchor = None;
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpBack(there) | ControlId::ExpForward(there) => {
                let forward = matches!(id, ControlId::ExpForward(_));
                let current = if there {
                    self.listing.as_ref().map(|l| l.path.clone())
                } else {
                    self.explorer.here_dir.as_ref().map(|d| d.to_string_lossy().into_owned())
                };
                let pane = &mut self.explorer.panes[idx(there)];
                let target = if forward { pane.forward.pop() } else { pane.back.pop() };
                if let Some(t) = target {
                    if let Some(c) = current {
                        if forward { pane.back.push(c) } else { pane.forward.push(c) }
                    }
                    self.open_folder(there, t, a);
                }
            }
            ControlId::ExpUp(there) => {
                let up = if there {
                    parent(&self.there_folder())
                } else {
                    self.explorer.here_dir.as_deref().and_then(Path::parent).map(|p| p.to_string_lossy().into_owned())
                };
                if let Some(up) = up {
                    self.go(there, up, a);
                }
            }
            ControlId::ExpRefresh(there) => {
                if there {
                    self.list_there(Some(self.there_folder()), a);
                } else {
                    let dir = self.explorer.here_dir.clone();
                    self.list_here(dir, a);
                }
            }
            ControlId::ExpPlace(there, i) => {
                let target = if there {
                    REMOTE_PLACES.get(usize::from(i)).map(|(p, _)| (*p).to_string())
                } else {
                    self.explorer.places.get(usize::from(i)).map(|p| p.path.to_string_lossy().into_owned())
                };
                if let Some(t) = target {
                    self.explorer.focus_there = there;
                    self.go(there, t, a);
                }
            }
            ControlId::ExpTransfers => {
                self.explorer.transfers_open = !self.explorer.transfers_open;
                a.changed = Changed::VIEWER;
            }
            _ => return false,
        }
        true
    }

    // ----- what the screen draws -----

    fn crumbs_here(&self) -> Vec<String> {
        let Some(dir) = &self.explorer.here_dir else { return Vec::new() };
        let place = self
            .explorer
            .places
            .iter()
            .filter(|p| dir.starts_with(&p.path))
            .max_by_key(|p| p.path.components().count());
        let mut out = Vec::new();
        let rest = match place {
            Some(p) => {
                out.push(self.place_label(p));
                dir.strip_prefix(&p.path).ok().map(Path::to_path_buf).unwrap_or_default()
            }
            None => dir.clone(),
        };
        out.extend(rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()));
        out
    }

    fn place_label(&self, p: &LocalPlace) -> String {
        match p.kind {
            PlaceKind::Home => self.t(MsgKey::ExplorerPlaceHome),
            PlaceKind::Desktop => self.t(MsgKey::ExplorerPlaceDesktop),
            PlaceKind::Documents => self.t(MsgKey::ExplorerPlaceDocuments),
            PlaceKind::Downloads => self.t(MsgKey::ExplorerPlaceDownloads),
            PlaceKind::Volume if p.label.ends_with(':') => {
                self.tm(&Msg::new(MsgKey::ExplorerDriveLocal).with(p.label.clone()))
            }
            PlaceKind::Volume => p.label.clone(),
        }
    }

    /// The navigation pane: this computer's places and volumes, then the other computer's places.
    pub fn explorer_places(&self) -> Vec<PlaceView> {
        let here = self.explorer.here_dir.as_deref();
        let mut out: Vec<PlaceView> = self
            .explorer
            .places
            .iter()
            .enumerate()
            .take(usize::from(u8::MAX))
            .map(|(i, p)| PlaceView {
                there: false,
                index: i as u8,
                label: self.place_label(p),
                volume: p.kind == PlaceKind::Volume,
                selected: here == Some(p.path.as_path()),
            })
            .collect();
        let folder = self.listing.as_ref().map(|l| l.path.as_str());
        out.extend(REMOTE_PLACES.iter().enumerate().map(|(i, (path, key))| PlaceView {
            there: true,
            index: i as u8,
            label: self.t(*key),
            volume: false,
            selected: folder == Some(*path),
        }));
        out
    }

    /// One pane of the explorer.
    pub fn explorer_pane(&self, there: bool) -> PaneView {
        let lang = self.language();
        let peer = self.peer_name();
        let pane = &self.explorer.panes[idx(there)];
        let rows: Vec<RowView> = self
            .sorted_rows(there)
            .into_iter()
            .map(|e| RowView {
                name: e.name.clone(),
                is_dir: e.is_dir,
                size: if e.is_dir { String::new() } else { crate::format::size_mb(lang, e.size) },
                kind: type_of(lang, &e.name, e.is_dir),
                modified: if e.mtime_ms == 0 {
                    String::new()
                } else {
                    crate::format::table_when(lang, e.mtime_ms, self.clock)
                },
                selected: pane.selected.contains(&e.name),
            })
            .collect();
        let loading = if there { self.placeholder(DataKind::Listing) } else { self.explorer.here_loading };
        let error = if there {
            None
        } else {
            self.explorer.here_error.map(|code| {
                let reason = match code {
                    ApiCode::NotFound => self.t(MsgKey::ExplorerErrGone),
                    _ => self.t(MsgKey::ExplorerErrDeniedHere),
                };
                (self.t(MsgKey::ExplorerErrTitle), reason)
            })
        };
        let picked: Vec<&RowView> = rows.iter().filter(|r| r.selected).collect();
        let status = if picked.is_empty() {
            if rows.len() == 1 {
                self.t(MsgKey::ExplorerOneItem)
            } else {
                self.tm(&Msg::new(MsgKey::ExplorerItems).with(rows.len().to_string()))
            }
        } else {
            let bytes: u64 = self
                .entries(there)
                .iter()
                .filter(|e| pane.selected.contains(&e.name) && !e.is_dir)
                .map(|e| e.size)
                .sum();
            self.tm(&Msg::new(MsgKey::ExplorerSelected)
                .with(picked.len().to_string())
                .with(crate::format::size_mb(lang, bytes)))
        };
        let truncated = if there {
            self.listing.as_ref().filter(|l| l.truncated).map(|l| l.entries.len())
        } else {
            self.explorer.here.as_ref().filter(|l| l.truncated).map(|l| l.entries.len())
        }
        .map(|n| self.tm(&Msg::new(MsgKey::ExplorerShowingFirst).with(n.to_string())));
        let crumbs = if there {
            let mut c = vec![self.t(MsgKey::ExplorerPlacePersonal)];
            c.extend(self.there_folder().split('/').filter(|s| !s.is_empty()).map(str::to_string));
            c
        } else {
            self.crumbs_here()
        };
        PaneView {
            there,
            title: if there { peer } else { self.t(MsgKey::ExplorerThisComputer) },
            crumbs,
            rows,
            loading,
            error,
            status,
            truncated,
            can_back: !pane.back.is_empty(),
            can_forward: !pane.forward.is_empty(),
            can_up: if there {
                !self.there_folder().is_empty()
            } else {
                self.explorer.here_dir.as_deref().and_then(Path::parent).is_some()
            },
            sort: pane.sort,
            focused: self.explorer.focus_there == there,
        }
    }

    /// The primary button's text: toward the pane that does not have the keyboard.
    pub fn explorer_copy_label(&self) -> String {
        if self.explorer.focus_there {
            self.t(MsgKey::ExplorerCopyHere)
        } else {
            self.tm(&Msg::new(MsgKey::ExplorerCopyTo).with(self.peer_name()))
        }
    }

    /// Whether the focused pane has a file selected to copy.
    pub fn explorer_can_copy(&self) -> bool {
        let there = self.explorer.focus_there;
        self.entries(there).iter().any(|e| !e.is_dir && self.explorer.panes[idx(there)].selected.contains(&e.name))
    }

    /// The note under the command bar (a folder left out of a copy, a file that opens only there).
    pub fn explorer_note(&self) -> Option<String> {
        self.explorer.note.map(|k| self.tm(&Msg::new(k).with(self.peer_name())))
    }

    pub fn explorer_transfers_open(&self) -> bool {
        self.explorer.transfers_open
    }

    /// The name of the other computer of the viewer session.
    fn peer_name(&self) -> String {
        self.viewer_session().and_then(|s| self.device_by_id(&s.device_id)).map(|d| d.name.clone()).unwrap_or_default()
    }

    /// The explorer's controls in Tab order: the command bar, the places, then each pane's buttons, columns and rows.
    pub(super) fn explorer_controls(&self, s: &mut Specs) {
        s.button(ControlId::ExpTela, self.name_of(MsgKey::ExplorerBackToScreen));
        s.button(ControlId::ExpCopy, self.explorer_copy_label());
        for p in self.explorer_places() {
            s.button(ControlId::ExpPlace(p.there, p.index), p.label);
        }
        for there in [false, true] {
            let v = self.explorer_pane(there);
            if v.can_back {
                s.button(ControlId::ExpBack(there), self.name_of(MsgKey::ExplorerBack));
            }
            if v.can_forward {
                s.button(ControlId::ExpForward(there), self.name_of(MsgKey::ExplorerForward));
            }
            if v.can_up {
                s.button(ControlId::ExpUp(there), self.name_of(MsgKey::ExplorerUp));
            }
            s.button(ControlId::ExpRefresh(there), self.name_of(MsgKey::ExplorerRefresh));
            for (col, key) in [
                (SORT_NAME, MsgKey::ExplorerColName),
                (SORT_SIZE, MsgKey::ExplorerColSize),
                (SORT_TYPE, MsgKey::ExplorerColType),
                (SORT_MODIFIED, MsgKey::ExplorerColModified),
            ] {
                s.button(ControlId::ExpSort(there, col), self.name_of(key));
            }
            for (i, r) in v.rows.iter().enumerate().take(usize::from(u16::MAX)) {
                s.add(ControlId::ExpRow(there, i as u16), UiRole::Button, format!("{} {}", r.name, r.kind));
            }
        }
        if !self.viewer_transfers().is_empty() {
            s.button(ControlId::ExpTransfers, self.name_of(MsgKey::ExplorerTransfers));
        }
    }
}

#[cfg(test)]
#[path = "explorer_tests.rs"]
mod tests;
