//! Slice 2b of the explorer (`docs/FILES-UI.md` 2.3, 2.5, 3.2, 3.3, 5, 7.2 and 10): the right pane, the other
//! computer, with the wire of minor 1.
//!
//! * The explorer asks `FilesVolumes` once when it opens a session (R-UI-3); the answer, and `UiEvent::RemoteVolumes`
//!   after it, fill the other computer's group of the navigation pane. An `Invalid` answer means the other computer
//!   runs 1.0: slice 1's behaviour stays (places derived from the home folder, `FilesList`, the note
//!   `ExplorerHomeOnly10`) and every change is disabled with `ExplorerNeeds11`.
//! * With a 1.1 peer the pane lists through `FilesListPage`. Paging choice: the interface fetches every page, one
//!   after the other, up to [`MAX_ENTRIES`] entries (10 000), as the state does not know the list's scroll position;
//!   the painter still draws only the visible rows (R-UI-5). Past the cap the status says "Mostrando os primeiros".
//! * Changes (new folder, rename, cut and paste, delete to the bin) are `FilesChange`, only with a 1.1 peer and `FILES`
//!   with `CONTROL`; delete there always asks first and never deletes for good.
//! * The conflict card before a copy, and the upload of a folder (`MakeDir` per folder, then `FilesSend` per folder).

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;

use super::explorer::{MAX_REMOTE_PLACES, REMOTE_PLACES, human_size, idx, join, parent};
use super::{ControlId, Specs, UiRole};
use crate::api::{
    ApiCode, ChangeOutcome, Command, DirEntryView, Feature, LocalListing, Msg, Os, Permission, Permissions, PlaceName,
    RemoteChange, RemoteEntry, RemoteKind, RemotePage, RemoteVolume, RemoteVolumes, SessionState, VolumeType,
};
use crate::state::{Activation, Changed, DataKind, EditId, Effect, Fetch, UiState};
use crate::strings::MsgKey;

/// Most entries the right pane loads of one folder.
pub(crate) const MAX_ENTRIES: usize = 10_000;
/// Limits of one folder upload (FILES-UI.md 5, the GUESS values).
const MAX_JOB_FILES: usize = 10_000;
const MAX_DEPTH: u16 = 32;

/// What the explorer knows about the other computer's version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Proto {
    /// Not answered yet.
    #[default]
    Unknown,
    /// `FilesVolumes` answered `Invalid`: slice 1's behaviour, the home folder only.
    V10,
    V11,
}

/// Why the right pane shows its error state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThereError {
    Code(ApiCode),
    /// The volume (its label) the folder lay on is no longer published.
    VolumeGone(String),
}

/// A copy the person asked for, split into what goes now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopyJob {
    pub to_there: bool,
    pub files: Vec<String>,
    pub folders: Vec<String>,
    /// This computer's folder (the source of an upload, the destination of a download).
    pub local_dir: PathBuf,
    /// The other computer's folder, a 1.1 path.
    pub remote_dir: String,
}

/// The conflict card: the names that exist already at the destination and what was answered so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Conflict {
    pub job: CopyJob,
    /// Names still to be answered, the first is the one the card is about.
    pub names: Vec<String>,
    pub apply_all: bool,
    pub skip: BTreeSet<String>,
    pub replace: Vec<String>,
}

/// A copy that waits for the bin to take the files it replaces.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Held {
    pub trash: Command,
    pub commands: Vec<Command>,
}

/// One folder of an upload: its local path, its parent there and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WalkItem {
    pub local: PathBuf,
    pub parent: String,
    pub name: String,
    pub depth: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WalkStep {
    /// `MakeDir` of the folder was sent.
    Making(WalkItem),
    /// The folder was made; its local listing was asked.
    Listing(WalkItem),
}

/// A folder upload under way: folders are made one at a time, each listed and sent once it exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Walk {
    pub queue: VecDeque<WalkItem>,
    pub step: Option<WalkStep>,
    pub files: usize,
}

/// What slice 2b remembers about the other computer.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct There {
    /// The session the rest belongs to.
    pub session: Option<u64>,
    pub proto: Proto,
    pub volumes: Option<RemoteVolumes>,
    pub asked: bool,
    /// The 1.1 listing: the folder, what is shown, what is hidden and where the next page starts.
    pub listed: bool,
    pub folder: String,
    /// The folder last asked for (a page of another one is stale).
    pub want: Option<String>,
    pub entries: Vec<DirEntryView>,
    pub hidden: Vec<DirEntryView>,
    pub shown: Vec<DirEntryView>,
    pub next: Option<u32>,
    pub capped: bool,
    pub free: Option<u64>,
    pub error: Option<ThereError>,
    pub show_hidden: bool,
    /// Changes: the row being renamed, a new folder waiting to open its rename, what Ctrl+X took, the delete card.
    pub renaming: Option<String>,
    pub rename_error: Option<MsgKey>,
    pub rename_when_listed: Option<String>,
    pub cut: Option<(String, Vec<String>)>,
    pub trash: Option<(String, Vec<String>)>,
    pub notice: Option<Msg>,
    pub conflict: Option<Conflict>,
    pub held: Option<Held>,
    pub walk: Option<Walk>,
}

/// A button of a card, as the screen draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStyle {
    Default,
    Primary,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardButton {
    pub id: ControlId,
    pub label: String,
    pub style: CardStyle,
    /// False: drawn disabled, with `why` as its description.
    pub enabled: bool,
    pub why: Option<String>,
}

/// A card over the explorer (slice 2b): the delete card of the other computer, the conflict card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardView {
    pub title: String,
    pub lines: Vec<String>,
    /// A check box: its control, its label and whether it is checked.
    pub check: Option<(ControlId, String, bool)>,
    /// From the left: the last one is the confirming (or default) answer.
    pub buttons: Vec<CardButton>,
}

/// One entry of the other computer's group in the navigation pane.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NavEntry {
    pub label: String,
    /// A path of the other computer: what the pane opens.
    pub path: String,
    pub volume: Option<VolumeType>,
    /// The space line, for a volume.
    pub hint: Option<String>,
}

/// `:<id>/...` names a volume; the id of the path's first component.
pub(crate) fn volume_id(path: &str) -> Option<u32> {
    path.strip_prefix(':')?.split('/').next()?.parse().ok()
}

fn hidden_by_name(name: &str) -> bool {
    name.starts_with('.') || name.ends_with(".swapdesk-part") || name.ends_with(".swapdesk-meta")
}

/// The folder above `path` on the other computer's 1.1 paths (`None` at a volume's root).
fn parent_11(path: &str) -> Option<String> {
    if path.starts_with(':') && !path.contains('/') {
        return None;
    }
    Some(path.rsplit_once('/').map(|(up, _)| up.to_string()).unwrap_or_default())
}

fn segments(path: &str) -> impl Iterator<Item = String> + '_ {
    path.split('/').filter(|s| !s.is_empty()).map(str::to_string)
}

fn view_of(e: &RemoteEntry) -> DirEntryView {
    DirEntryView { name: e.name.clone(), is_dir: e.kind == RemoteKind::Dir, size: e.size, mtime_ms: e.mtime_ms }
}

impl UiState {
    // ----- what the explorer knows about the other computer -----

    pub(crate) fn there_v11(&self) -> bool {
        self.explorer.there.proto == Proto::V11
    }

    fn there_v10(&self) -> bool {
        self.explorer.there.proto == Proto::V10
    }

    /// The permissions of the viewer session, when it is active.
    pub(crate) fn granted(&self) -> Option<Permissions> {
        match self.viewer_session().map(|s| &s.state) {
            Some(SessionState::Active { granted, .. }) => Some(*granted),
            _ => None,
        }
    }

    fn peer_is_windows(&self) -> bool {
        self.viewer_session().and_then(|s| self.device_by_id(&s.device_id)).is_some_and(|d| d.os == Os::Windows)
    }

    /// The other computer takes changes: a 1.1 peer and `FILES` with `CONTROL` (FILES-UI.md 3.3).
    pub(crate) fn there_can_change(&self) -> bool {
        self.there_v11()
            && self.session_id().is_some()
            && self.granted().is_some_and(|g| g.has(Permission::Files) && g.has(Permission::Control))
    }

    /// Why a change there is disabled (the description of its controls); `None` when it is not, or not known yet.
    pub(crate) fn there_block_reason(&self) -> Option<Msg> {
        let peer = self.peer_name();
        if self.there_v10() {
            return Some(Msg::new(MsgKey::ExplorerNeeds11).with(peer));
        }
        let g = self.granted()?;
        (self.there_v11() && !(g.has(Permission::Files) && g.has(Permission::Control)))
            .then(|| Msg::new(MsgKey::ExplorerNeedsControl).with(peer))
    }

    /// The volume the shown folder lies on (a home folder path lies on the home's volume).
    fn folder_volume(&self) -> Option<&RemoteVolume> {
        let t = &self.explorer.there;
        let v = t.volumes.as_ref()?;
        let id = volume_id(&self.there_folder()).or_else(|| volume_id(&v.home))?;
        v.volumes.iter().find(|x| x.id == id)
    }

    /// Whether the shown folder's volume has a bin (true when the volume list does not say).
    pub(crate) fn there_has_bin(&self) -> bool {
        self.folder_volume().is_none_or(|v| v.has_bin)
    }

    fn there_names(&self) -> Vec<String> {
        let t = &self.explorer.there;
        if t.proto == Proto::V11 && t.listed {
            t.entries.iter().chain(&t.hidden).map(|e| e.name.clone()).collect()
        } else {
            self.listing.as_ref().map(|l| l.entries.iter().map(|e| e.name.clone()).collect()).unwrap_or_default()
        }
    }

    fn here_names(&self) -> Vec<String> {
        self.explorer
            .here
            .as_ref()
            .map(|l| l.entries.iter().chain(&l.hidden).map(|e| e.name.clone()).collect())
            .unwrap_or_default()
    }

    fn selected_names(&self, there: bool) -> Vec<String> {
        let sel = &self.explorer.panes[idx(there)].selected;
        self.sorted_rows(there).into_iter().filter(|e| sel.contains(&e.name)).map(|e| e.name.clone()).collect()
    }

    // ----- opening, volumes -----

    /// The explorer opened: this is the first time for this session, ask for the volumes once.
    pub(crate) fn there_prepare(&mut self, a: &mut Activation) {
        let sid = self.session_id();
        let t = &mut self.explorer.there;
        if t.session != sid {
            *t = There { session: sid, ..There::default() };
        }
        if !t.asked
            && let Some(session_id) = sid
        {
            t.asked = true;
            a.commands.push(Command::FilesVolumes { session_id });
        }
    }

    /// `Reply::RemoteVolumes`: the other computer runs 1.1.
    pub(crate) fn there_volumes(&mut self, v: RemoteVolumes) -> Changed {
        let t = &mut self.explorer.there;
        t.proto = Proto::V11;
        t.volumes = Some(v);
        if !t.listed {
            // What FilesList brought (or nothing yet) is replaced by the first page of the same folder.
            let path = t.want.clone().unwrap_or_else(|| self.there_folder());
            self.relist_there(path, true);
        }
        Changed::VIEWER
    }

    /// `FilesVolumes` failed: `Invalid` is a 1.0 peer.
    pub(crate) fn there_volumes_failed(&mut self, code: ApiCode) -> Changed {
        if code == ApiCode::Invalid {
            self.explorer.there.proto = Proto::V10;
            return Changed::VIEWER;
        }
        Changed::NONE
    }

    /// `UiEvent::RemoteVolumes`: a disk came or went.
    pub(crate) fn there_volumes_event(&mut self, session_id: u64, v: RemoteVolumes) -> Changed {
        if self.session_id() != Some(session_id) {
            return Changed::NONE;
        }
        let folder = self.there_folder();
        let t = &mut self.explorer.there;
        t.proto = Proto::V11;
        let gone = volume_id(&folder)
            .filter(|id| !v.volumes.iter().any(|x| x.id == *id))
            .map(|id| t.volumes.as_ref().and_then(|old| old.volumes.iter().find(|x| x.id == id)).cloned());
        t.volumes = Some(v);
        if let Some(old) = gone {
            let label = match old {
                Some(vol) => self.volume_label(&vol),
                None => self.t(MsgKey::ExplorerVolumeUnnamed),
            };
            self.explorer.there.error = Some(ThereError::VolumeGone(label));
        }
        Changed::VIEWER
    }

    // ----- the listing -----

    /// Asks the first page of `path` (the pane shows what it has meanwhile when `quiet`).
    fn relist_there(&mut self, path: String, show_placeholder: bool) {
        let Some(session_id) = self.session_id() else { return };
        self.explorer.there.want = Some(path.clone());
        if show_placeholder {
            self.begin_fetch(DataKind::Listing);
        }
        self.followups.push(Command::FilesListPage { session_id, path, start: 0 });
    }

    /// `Reply::RemotePage`; `asked` is the path and start the command carried, when it was tracked.
    pub(crate) fn there_page(&mut self, asked: Option<(String, u32)>, page: RemotePage) -> Changed {
        self.consent_answered(Feature::Files);
        let Some(session_id) = self.session_id() else { return Changed::NONE };
        let t = &mut self.explorer.there;
        let path = asked.map_or_else(|| page.path.clone(), |(p, _)| p);
        if page.start == 0 {
            if t.want.as_ref().is_some_and(|w| *w != path) {
                return Changed::NONE;
            }
            t.folder = path.clone();
            t.entries.clear();
            t.hidden.clear();
            t.error = None;
            t.free = page.free;
            t.capped = false;
            t.listed = true;
        } else {
            if !t.listed || t.folder != path || t.next != Some(page.start) {
                return Changed::NONE;
            }
            if page.free.is_some() {
                t.free = page.free;
            }
        }
        for e in &page.entries {
            let view = view_of(e);
            if e.hidden || e.system || hidden_by_name(&e.name) {
                t.hidden.push(view);
            } else {
                t.entries.push(view);
            }
        }
        t.next = page.next;
        let total = t.entries.len() + t.hidden.len();
        if let Some(next) = page.next {
            if total < MAX_ENTRIES {
                self.followups.push(Command::FilesListPage { session_id, path, start: next });
            } else {
                t.capped = true;
            }
        }
        self.refresh_there_rows();
        self.fetch.insert(DataKind::Listing, Fetch::Ready);
        if page.next.is_none() || self.explorer.there.capped {
            self.there_listed();
        }
        Changed::VIEWER
    }

    /// The last page of the folder is in: a folder just created opens its rename field.
    fn there_listed(&mut self) {
        let t = &mut self.explorer.there;
        let Some(name) = t.rename_when_listed.take() else { return };
        if t.shown.iter().any(|e| e.name == name) {
            self.explorer.panes[1].selected = std::iter::once(name.clone()).collect();
            self.explorer.there.renaming = Some(name);
            self.explorer.there.rename_error = None;
        }
    }

    /// `FilesListPage` failed.
    pub(crate) fn there_page_failed(&mut self, asked: &Command, code: ApiCode) -> Changed {
        self.consent_answered(Feature::Files);
        let Command::FilesListPage { path, start, .. } = asked else { return Changed::NONE };
        if code == ApiCode::Invalid {
            // The other computer runs 1.0 after all: slice 1's listing.
            self.explorer.there.proto = Proto::V10;
            let path = path.clone();
            if let Some(session_id) = self.session_id() {
                self.followups.push(Command::FilesList { session_id, path: Some(path) });
            }
            return Changed::VIEWER;
        }
        let t = &mut self.explorer.there;
        if *start > 0 {
            // A later page failed: what is loaded stays, and the rest is not fetched.
            t.next = None;
            return Changed::VIEWER;
        }
        if t.want.as_ref().is_some_and(|w| w != path) {
            return Changed::NONE;
        }
        t.error = Some(ThereError::Code(code));
        self.fail_fetch(DataKind::Listing);
        Changed::VIEWER
    }

    /// The right pane's rows: its entries, and its hidden ones when they are shown.
    pub(crate) fn refresh_there_rows(&mut self) {
        let t = &mut self.explorer.there;
        let mut shown = t.entries.clone();
        if t.show_hidden {
            shown.extend(t.hidden.iter().cloned());
        }
        t.shown = shown;
    }

    // ----- the navigation pane -----

    pub(crate) fn volume_label(&self, v: &RemoteVolume) -> String {
        match v.letter {
            Some(l) => {
                let letter = format!("{l}:");
                if !v.label.is_empty() {
                    return self.tm(&Msg::new(MsgKey::ExplorerDriveLabeled).with(v.label.clone()).with(letter));
                }
                let key = match v.kind {
                    VolumeType::Removable => MsgKey::ExplorerDriveRemovable,
                    VolumeType::Network => MsgKey::ExplorerDriveNetwork,
                    VolumeType::Optical => MsgKey::ExplorerDriveOptical,
                    VolumeType::System | VolumeType::Fixed | VolumeType::Image => MsgKey::ExplorerDriveLocal,
                };
                self.tm(&Msg::new(key).with(letter))
            }
            None if v.label.is_empty() => self.t(MsgKey::ExplorerVolumeUnnamed),
            None => v.label.clone(),
        }
    }

    fn place_name(&self, kind: PlaceName) -> String {
        self.t(match kind {
            PlaceName::Desktop => MsgKey::ExplorerPlaceDesktop,
            PlaceName::Documents => MsgKey::ExplorerPlaceDocuments,
            PlaceName::Downloads => MsgKey::ExplorerPlaceDownloads,
            PlaceName::Pictures => MsgKey::ExplorerPlacePictures,
            PlaceName::Music => MsgKey::ExplorerPlaceMusic,
            PlaceName::Videos => MsgKey::ExplorerPlaceVideos,
            PlaceName::Received => MsgKey::ExplorerPlaceReceived,
        })
    }

    /// The other computer's group of the navigation pane: with a 1.1 peer its home folder, places and volumes; else
    /// the places derived from the home folder (FILES-UI.md 7.2).
    pub(crate) fn remote_nav(&self) -> Vec<NavEntry> {
        let t = &self.explorer.there;
        let (true, Some(v)) = (t.proto == Proto::V11, &t.volumes) else {
            return REMOTE_PLACES
                .iter()
                .map(|(path, key)| NavEntry {
                    label: self.t(*key),
                    path: (*path).to_string(),
                    volume: None,
                    hint: None,
                })
                .collect();
        };
        let mut out = vec![NavEntry {
            label: self.t(MsgKey::ExplorerPlacePersonal),
            path: v.home.clone(),
            volume: None,
            hint: None,
        }];
        out.extend(v.places.iter().map(|p| NavEntry {
            label: self.place_name(p.kind),
            path: p.path.clone(),
            volume: None,
            hint: None,
        }));
        out.extend(v.volumes.iter().map(|vol| {
            NavEntry {
                label: self.volume_label(vol),
                path: format!(":{}", vol.id),
                volume: Some(vol.kind),
                hint: Some(
                    self.tm(&Msg::new(MsgKey::ExplorerVolumeSpace)
                        .with(human_size(self.lang, vol.free_bytes))
                        .with(human_size(self.lang, vol.total_bytes))),
                ),
            }
        }));
        out.truncate(MAX_REMOTE_PLACES);
        out
    }

    /// The entry of `nav` the right pane's folder is on: the one with its path, else the volume it lies on.
    pub(crate) fn remote_nav_selected(&self, nav: &[NavEntry]) -> Option<usize> {
        let t = &self.explorer.there;
        if !(t.listed || self.listing.is_some()) {
            return None;
        }
        let home = t.volumes.as_ref().map(|v| v.home.clone()).unwrap_or_default();
        let canon = |p: &str| if p.is_empty() { home.clone() } else { p.to_string() };
        let folder = canon(&self.there_folder());
        nav.iter()
            .position(|n| canon(&n.path) == folder)
            .or_else(|| nav.iter().position(|n| n.volume.is_some() && folder.starts_with(&format!("{}/", n.path))))
    }

    /// The breadcrumb of the right pane: the volume or the home folder, then each folder.
    pub(crate) fn crumbs_there(&self) -> Vec<String> {
        let personal = self.t(MsgKey::ExplorerPlacePersonal);
        let folder = self.there_folder();
        let mut c = vec![personal];
        if !self.there_v11() {
            c.extend(segments(&folder));
            return c;
        }
        let home = self.explorer.there.volumes.as_ref().map(|v| v.home.clone()).unwrap_or_default();
        if !home.is_empty()
            && let Some(rest) = folder.strip_prefix(&home).filter(|r| r.is_empty() || r.starts_with('/'))
        {
            c.extend(segments(rest));
        } else if let Some(id) = volume_id(&folder) {
            let label = self
                .explorer
                .there
                .volumes
                .as_ref()
                .and_then(|v| v.volumes.iter().find(|x| x.id == id))
                .map_or_else(|| self.t(MsgKey::ExplorerVolumeUnnamed), |x| self.volume_label(x));
            c = vec![label];
            c.extend(segments(&folder).skip(1));
        } else {
            c.extend(segments(&folder));
        }
        c
    }

    /// The folder above the right pane's, if there is one.
    pub(crate) fn there_up(&self) -> Option<String> {
        let folder = self.there_folder();
        if !self.there_v11() {
            return parent(&folder);
        }
        if folder.is_empty() {
            let home = self.explorer.there.volumes.as_ref().map(|v| v.home.clone()).unwrap_or_default();
            return if home.is_empty() { None } else { parent_11(&home) };
        }
        parent_11(&folder)
    }

    /// The right pane's error state: its reason.
    pub(crate) fn there_error_reason(&self) -> Option<String> {
        let peer = self.peer_name();
        let e = self.explorer.there.error.as_ref()?;
        Some(match e {
            ThereError::VolumeGone(label) => {
                self.tm(&Msg::new(MsgKey::ExplorerErrVolumeGone).with(label.clone()).with(peer))
            }
            ThereError::Code(ApiCode::NotFound) => self.t(MsgKey::ExplorerErrGone),
            ThereError::Code(ApiCode::Timeout) => self.tm(&Msg::new(MsgKey::ExplorerErrTimeout).with(peer)),
            ThereError::Code(ApiCode::NotPermitted) => self.tm(&Msg::new(MsgKey::ExplorerErrNoGrant).with(peer)),
            ThereError::Code(_) => self.tm(&Msg::new(MsgKey::ExplorerErrDenied).with(peer)),
        })
    }

    /// The free space of the shown folder's volume, as the header or status line says it.
    pub(crate) fn there_free_text(&self) -> Option<String> {
        let t = &self.explorer.there;
        if t.proto != Proto::V11 {
            return None;
        }
        let free = t.free.or_else(|| self.folder_volume().map(|v| v.free_bytes))?;
        Some(match self.folder_volume() {
            Some(v) if v.total_bytes > 0 => self.tm(&Msg::new(MsgKey::ExplorerVolumeSpace)
                .with(human_size(self.lang, free))
                .with(human_size(self.lang, v.total_bytes))),
            _ => self.tm(&Msg::new(MsgKey::ExplorerPropsFree).with(human_size(self.lang, free))),
        })
    }

    /// The line under the right pane: a change's notice, else the 1.0 note.
    pub(crate) fn there_notice_text(&self) -> Option<String> {
        if let Some(n) = &self.explorer.there.notice {
            return Some(self.tm(n));
        }
        self.there_v10().then(|| self.tm(&Msg::new(MsgKey::ExplorerHomeOnly10).with(self.peer_name())))
    }

    // ----- the commands -----

    /// A change command on the right pane (the focus is there); false when `id` is not one of them.
    pub(crate) fn there_activate(&mut self, id: ControlId, a: &mut Activation) -> bool {
        match id {
            ControlId::ExpNewFolder => self.there_new_folder(a),
            ControlId::ExpRename => self.there_start_rename(a),
            ControlId::ExpDelete => self.there_ask_trash(a),
            _ => return false,
        }
        true
    }

    /// The slice 2b cards and the right pane's own switch; false for any other control.
    pub(crate) fn remote_activate(&mut self, id: ControlId, a: &mut Activation) -> bool {
        match id {
            ControlId::ExpHiddenThere => {
                let t = &mut self.explorer.there;
                t.show_hidden = !t.show_hidden;
                self.explorer.panes[1].anchor = None;
                self.refresh_there_rows();
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpThereTrashCancel => {
                self.explorer.there.trash = None;
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpThereTrashConfirm => {
                let Some((dir, names)) = self.explorer.there.trash.take() else { return true };
                if let Some(session_id) = self.session_id().filter(|_| self.there_can_change()) {
                    a.commands.push(Command::FilesChange { session_id, change: RemoteChange::Trash { dir, names } });
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpConflictApplyAll => {
                if let Some(c) = &mut self.explorer.there.conflict {
                    c.apply_all = !c.apply_all;
                    a.changed = Changed::VIEWER;
                }
            }
            ControlId::ExpConflictCancel => {
                self.explorer.there.conflict = None;
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpConflictSkip => self.conflict_answer(Answer::Skip, a),
            ControlId::ExpConflictReplace => self.conflict_answer(Answer::Replace, a),
            ControlId::ExpConflictKeepBoth => self.conflict_answer(Answer::KeepBoth, a),
            _ => return false,
        }
        true
    }

    fn there_new_folder(&mut self, a: &mut Activation) {
        let Some(session_id) = self.session_id().filter(|_| self.there_can_change()) else { return };
        let folder = self.there_folder();
        let names = self.there_names();
        let taken: Vec<&str> = names.iter().map(String::as_str).collect();
        let name = crate::local_disk::next_free_name(&self.t(MsgKey::ExplorerNewFolder), &taken);
        let t = &mut self.explorer.there;
        t.notice = None;
        t.rename_when_listed = Some(name.clone());
        a.commands.push(Command::FilesChange { session_id, change: RemoteChange::MakeDir { parent: folder, name } });
        a.changed = Changed::VIEWER;
    }

    fn there_start_rename(&mut self, a: &mut Activation) {
        let picked = self.selected_names(true);
        if !self.there_can_change() || picked.len() != 1 {
            return;
        }
        self.explorer.manage.renaming = None;
        let t = &mut self.explorer.there;
        t.renaming = Some(picked[0].clone());
        t.rename_error = None;
        t.notice = None;
        a.effect = Some(Effect::FocusEdit(EditId::ExpRename));
        a.changed = Changed::VIEWER;
    }

    /// The name rules of both systems; the other computer's Windows also refuses `* ? " < > |`.
    fn there_name_ok(&self, name: &str) -> bool {
        swapdesk_files::validate_name(name).is_ok()
            && !(self.peer_is_windows() && name.chars().any(|c| "*?\"<>|".contains(c)))
    }

    /// The inline rename on the right pane was committed with `text`.
    pub(crate) fn submit_there_rename(&mut self, text: String, a: &mut Activation) {
        let Some(old) = self.explorer.there.renaming.clone() else { return };
        let name = text.trim_end_matches(['\r', '\n']).to_owned();
        a.changed = Changed::VIEWER;
        if name == old {
            self.explorer.there.renaming = None;
            return;
        }
        if !self.there_name_ok(&name) {
            self.explorer.there.rename_error = Some(MsgKey::ExplorerNameInvalid);
            return;
        }
        let Some(session_id) = self.session_id().filter(|_| self.there_can_change()) else { return };
        let dir = self.there_folder();
        a.commands
            .push(Command::FilesChange { session_id, change: RemoteChange::Rename { dir, name: old, new_name: name } });
    }

    /// Delete on the right pane always asks first (FILES-UI.md 3.2): the card, then `Trash` on "Excluir".
    fn there_ask_trash(&mut self, a: &mut Activation) {
        let names = self.selected_names(true);
        if !self.there_can_change() || names.is_empty() {
            return;
        }
        if !self.there_has_bin() {
            self.explorer.there.notice = Some(Msg::new(MsgKey::ExplorerNoBin));
        } else {
            self.explorer.there.notice = None;
            self.explorer.there.trash = Some((self.there_folder(), names));
        }
        a.changed = Changed::VIEWER;
    }

    /// Ctrl+X with the focus on the right pane.
    pub(crate) fn there_cut(&mut self) {
        if !self.there_can_change() {
            return;
        }
        let names = self.selected_names(true);
        self.explorer.manage.cut.clear();
        self.explorer.there.cut = (!names.is_empty()).then(|| (self.there_folder(), names));
        self.explorer.there.notice = None;
    }

    /// Ctrl+V with the focus on the right pane: what was cut there moves into the folder shown.
    pub(crate) fn there_paste(&mut self, a: &mut Activation) {
        let Some(session_id) = self.session_id().filter(|_| self.there_can_change()) else { return };
        let to_dir = self.there_folder();
        let Some((from_dir, names)) = self.explorer.there.cut.take() else { return };
        if from_dir == to_dir {
            return;
        }
        a.commands.push(Command::FilesChange { session_id, change: RemoteChange::Move { from_dir, names, to_dir } });
        a.changed = Changed::VIEWER;
    }

    /// Esc: closes what the right pane has open; true when something was.
    pub(crate) fn there_escape(&mut self) -> bool {
        let t = &mut self.explorer.there;
        let open = t.renaming.is_some() || t.trash.is_some() || t.conflict.is_some();
        t.renaming = None;
        t.rename_error = None;
        t.trash = None;
        t.conflict = None;
        open
    }

    fn notice_of(&self, change: &RemoteChange, outcome: ChangeOutcome, label: String) -> Msg {
        let peer = self.peer_name();
        Msg::new(match outcome {
            ChangeOutcome::NoBin => MsgKey::ExplorerNoBin,
            ChangeOutcome::VolumeGone => return Msg::new(MsgKey::ExplorerErrVolumeGone).with(label).with(peer),
            ChangeOutcome::NotPermitted => MsgKey::ExplorerNotPermitted,
            ChangeOutcome::NotFound => MsgKey::ExplorerGone,
            ChangeOutcome::Exists => MsgKey::ExplorerNameTaken,
            ChangeOutcome::InvalidName => match change {
                RemoteChange::Move { .. } => MsgKey::ExplorerMoveIntoItself,
                RemoteChange::MakeDir { .. } | RemoteChange::Rename { .. } => MsgKey::ExplorerNameInvalid,
                RemoteChange::Trash { .. } => MsgKey::ExplorerOpFailed,
            },
            ChangeOutcome::Done | ChangeOutcome::NoSpace | ChangeOutcome::Failed => MsgKey::ExplorerOpFailed,
        })
    }

    fn folder_label(&self) -> String {
        match self.folder_volume() {
            Some(v) => self.volume_label(v),
            None => self.t(MsgKey::ExplorerVolumeUnnamed),
        }
    }

    /// The folder is listed again after a change, whatever came of it.
    fn relist_after_change(&mut self) {
        let folder = self.there_folder();
        self.relist_there(folder, false);
    }

    /// `Reply::ChangeResults` for `command`.
    pub(crate) fn there_change_results(
        &mut self,
        command: Option<Command>,
        results: Vec<(String, ChangeOutcome)>,
    ) -> Changed {
        let Some(command) = command else { return Changed::NONE };
        let Command::FilesChange { change, .. } = command.clone() else { return Changed::NONE };
        let bad = results.iter().find(|(_, o)| *o != ChangeOutcome::Done).map(|(_, o)| *o);
        if let Some(c) = self.walk_made(&command, bad) {
            return c;
        }
        let label = self.folder_label();
        let notice = bad.map(|o| self.notice_of(&change, o, label));
        match &change {
            RemoteChange::Rename { .. } => {
                let t = &mut self.explorer.there;
                match bad {
                    Some(ChangeOutcome::Exists) => {
                        t.rename_error = Some(MsgKey::ExplorerNameTaken);
                        return Changed::VIEWER;
                    }
                    Some(ChangeOutcome::InvalidName) => {
                        t.rename_error = Some(MsgKey::ExplorerNameInvalid);
                        return Changed::VIEWER;
                    }
                    _ => {
                        t.renaming = None;
                        t.rename_error = None;
                        self.explorer.panes[1].selected.clear();
                    }
                }
            }
            RemoteChange::MakeDir { .. } => {
                if bad.is_some() {
                    self.explorer.there.rename_when_listed = None;
                }
            }
            RemoteChange::Move { .. } => self.explorer.panes[1].selected.clear(),
            RemoteChange::Trash { .. } => {
                self.explorer.panes[1].selected.clear();
                self.held_after(&command, bad.is_none());
            }
        }
        self.explorer.there.notice = notice;
        self.relist_after_change();
        Changed::VIEWER
    }

    /// A change command failed as a whole (`code`), not name by name.
    pub(crate) fn there_change_failed(&mut self, command: &Command, code: ApiCode) -> Changed {
        let Command::FilesChange { change, .. } = command else { return Changed::NONE };
        let key = match code {
            ApiCode::Invalid => {
                self.explorer.there.proto = Proto::V10;
                MsgKey::ExplorerNeeds11
            }
            ApiCode::NotPermitted => MsgKey::ExplorerNeedsControl,
            _ => MsgKey::ExplorerOpFailed,
        };
        let msg = Msg::new(key).with(self.peer_name());
        if self.walk_made(command, Some(ChangeOutcome::Failed)).is_some() {
            self.explorer.there.notice = Some(msg);
            return Changed::VIEWER;
        }
        self.held_after(command, false);
        let t = &mut self.explorer.there;
        match change {
            RemoteChange::Rename { .. } => t.rename_error = Some(MsgKey::ExplorerOpFailed),
            RemoteChange::MakeDir { .. } => t.rename_when_listed = None,
            _ => {}
        }
        t.notice = Some(msg);
        Changed::VIEWER
    }

    // ----- copy: the conflict card, the folder upload -----

    /// Why a folder cannot go to the other computer; `None` when it can.
    pub(crate) fn upload_folder_block(&self) -> Option<MsgKey> {
        match self.explorer.there.proto {
            Proto::V10 => Some(MsgKey::ExplorerNeeds11),
            Proto::Unknown => Some(MsgKey::ExplorerFoldersLater),
            Proto::V11 if !self.there_can_change() => Some(MsgKey::ExplorerNeedsControl),
            Proto::V11 => None,
        }
    }

    /// Whether the focused pane has something to copy: a file, or a folder that can go up.
    pub(crate) fn copy_has_something(&self) -> bool {
        let there = self.explorer.focus_there;
        let sel = &self.explorer.panes[idx(there)].selected;
        let picked = self.entries(there).iter().filter(|e| sel.contains(&e.name));
        let mut any = false;
        for e in picked {
            if !e.is_dir || (!there && self.upload_folder_block().is_none()) {
                any = true;
            }
        }
        any
    }

    /// "Copiar para ...": starts the job of the focused pane's selection toward the other pane.
    pub(crate) fn start_copy(&mut self, a: &mut Activation) {
        if self.session_id().is_none() {
            return;
        }
        let there = self.explorer.focus_there;
        let picked: Vec<(String, bool)> = {
            let sel = &self.explorer.panes[idx(there)].selected;
            self.sorted_rows(there)
                .into_iter()
                .filter(|e| sel.contains(&e.name))
                .map(|e| (e.name.clone(), e.is_dir))
                .collect()
        };
        let src = if there {
            self.there_folder()
        } else {
            let Some(dir) = self.explorer.here_dir.clone() else { return };
            dir.to_string_lossy().into_owned()
        };
        self.copy_items(!there, picked, src, None, a);
    }

    /// The copy of `picked` (names, folder or not) out of the folder `src` of one computer into `dest`, a folder of
    /// the other (`None`: the other pane's current folder, whose listing lets the conflict card ask first; another
    /// folder's listing is not known, so the engine's own rule of never overwriting is what protects its files).
    /// The primary button and a drag between panes both end here.
    pub(crate) fn copy_items(
        &mut self,
        to_there: bool,
        picked: Vec<(String, bool)>,
        src: String,
        dest: Option<String>,
        a: &mut Activation,
    ) {
        if self.session_id().is_none() {
            return;
        }
        self.explorer.note = None;
        let files: Vec<String> = picked.iter().filter(|(_, d)| !d).map(|(n, _)| n.clone()).collect();
        let mut folders: Vec<String> = picked.iter().filter(|(_, d)| *d).map(|(n, _)| n.clone()).collect();
        if !folders.is_empty() {
            let block = if to_there { self.upload_folder_block() } else { Some(MsgKey::ExplorerFoldersLater) };
            if let Some(why) = block {
                self.explorer.note = Some(why);
                folders.clear();
            }
        }
        a.changed = Changed::VIEWER;
        if files.is_empty() && folders.is_empty() {
            return;
        }
        let (local_dir, remote_dir) = if to_there {
            (PathBuf::from(&src), dest.clone().unwrap_or_else(|| self.there_folder()))
        } else {
            let Some(local) = dest.as_ref().map(PathBuf::from).or_else(|| self.explorer.here_dir.clone()) else {
                return;
            };
            (local, src)
        };
        let job = CopyJob { to_there, files, folders, local_dir, remote_dir };
        let known = if dest.is_some() {
            Vec::new()
        } else if to_there {
            self.there_names()
        } else {
            self.here_names()
        };
        let lower: BTreeSet<String> = known.iter().map(|n| n.to_lowercase()).collect();
        let names: Vec<String> = job.files.iter().filter(|n| lower.contains(&n.to_lowercase())).cloned().collect();
        if names.is_empty() {
            self.run_job(job, &BTreeSet::new(), Vec::new(), a);
        } else {
            let apply_all = names.len() > 1;
            self.explorer.there.conflict =
                Some(Conflict { job, names, apply_all, skip: BTreeSet::new(), replace: Vec::new() });
        }
    }

    /// What `Substituir` needs and lacks, for the conflict card's destination.
    fn replace_block(&self, to_there: bool) -> Option<Msg> {
        if !to_there {
            return None;
        }
        let peer = self.peer_name();
        if self.there_v10() || self.explorer.there.proto == Proto::Unknown {
            return Some(Msg::new(MsgKey::ExplorerNeeds11).with(peer));
        }
        if !self.there_can_change() {
            return Some(Msg::new(MsgKey::ExplorerNeedsControl).with(peer));
        }
        (!self.there_has_bin()).then(|| Msg::new(MsgKey::ExplorerNoBin))
    }

    fn conflict_answer(&mut self, answer: Answer, a: &mut Activation) {
        let Some(mut c) = self.explorer.there.conflict.take() else { return };
        if answer == Answer::Replace && self.replace_block(c.job.to_there).is_some() {
            self.explorer.there.conflict = Some(c);
            return;
        }
        let take = if c.apply_all { c.names.len() } else { 1 };
        let decided: Vec<String> = c.names.drain(..take.min(c.names.len())).collect();
        match answer {
            Answer::Skip => c.skip.extend(decided),
            Answer::Replace => c.replace.extend(decided),
            Answer::KeepBoth => {}
        }
        a.changed = Changed::VIEWER;
        if c.names.is_empty() {
            let Conflict { job, skip, replace, .. } = c;
            self.run_job(job, &skip, replace, a);
        } else {
            self.explorer.there.conflict = Some(c);
        }
    }

    /// Sends the job: files now (after the bin took the ones replaced), folders by the walk.
    fn run_job(&mut self, job: CopyJob, skip: &BTreeSet<String>, replace: Vec<String>, a: &mut Activation) {
        let Some(session_id) = self.session_id() else { return };
        let files: Vec<String> = job.files.iter().filter(|n| !skip.contains(*n)).cloned().collect();
        let mut copy = Vec::new();
        if job.to_there {
            if !files.is_empty() {
                copy.push(Command::FilesSend {
                    session_id,
                    local_paths: files.iter().map(|n| job.local_dir.join(n)).collect(),
                    remote_dir: Some(job.remote_dir.clone()),
                });
            }
        } else {
            for n in &files {
                copy.push(Command::FilesGet {
                    session_id,
                    remote_path: join(&job.remote_dir, n),
                    local_dir: job.local_dir.clone(),
                });
            }
        }
        if replace.is_empty() {
            a.commands.extend(copy);
        } else {
            let trash = if job.to_there {
                Command::FilesChange {
                    session_id,
                    change: RemoteChange::Trash { dir: job.remote_dir.clone(), names: replace },
                }
            } else {
                Command::LocalTrash { paths: replace.iter().map(|n| job.local_dir.join(n)).collect() }
            };
            self.explorer.there.held = Some(Held { trash: trash.clone(), commands: copy });
            a.commands.push(trash);
        }
        if !job.folders.is_empty() {
            let walk = self.explorer.there.walk.get_or_insert_with(Walk::default);
            let idle = walk.step.is_none();
            for name in job.folders {
                walk.queue.push_back(WalkItem {
                    local: job.local_dir.join(&name),
                    parent: job.remote_dir.clone(),
                    name,
                    depth: 1,
                });
            }
            if idle && let Some(c) = self.walk_advance() {
                a.commands.push(c);
            }
        }
        self.explorer.transfers_open = true;
        a.changed = Changed::VIEWER;
    }

    /// The bin answered for a command a replace waits on: the copy goes on when it took everything.
    pub(crate) fn held_after(&mut self, command: &Command, done: bool) {
        if self.explorer.there.held.as_ref().is_none_or(|h| h.trash != *command) {
            return;
        }
        if let Some(h) = self.explorer.there.held.take()
            && done
        {
            self.followups.extend(h.commands);
        }
    }

    // ----- the folder upload -----

    /// The next folder of the walk: its `MakeDir`, or `None` when the walk is over.
    fn walk_advance(&mut self) -> Option<Command> {
        let session_id = self.session_id()?;
        let walk = self.explorer.there.walk.as_mut()?;
        let Some(item) = walk.queue.pop_front() else {
            self.explorer.there.walk = None;
            return None;
        };
        let change = RemoteChange::MakeDir { parent: item.parent.clone(), name: item.name.clone() };
        walk.step = Some(WalkStep::Making(item));
        Some(Command::FilesChange { session_id, change })
    }

    fn walk_abort(&mut self, notice: Msg) {
        self.explorer.there.walk = None;
        self.explorer.there.notice = Some(notice);
    }

    /// A `MakeDir` answered: when it is the walk's, the folder is listed here next. `bad` is its failure, if any.
    fn walk_made(&mut self, command: &Command, bad: Option<ChangeOutcome>) -> Option<Changed> {
        let Command::FilesChange { change: RemoteChange::MakeDir { parent, name }, .. } = command else { return None };
        let step = self.explorer.there.walk.as_ref()?.step.clone();
        let Some(WalkStep::Making(item)) = step else { return None };
        if item.parent != *parent || item.name != *name {
            return None;
        }
        // A folder that exists is merged into, as the system's file managers do.
        match bad {
            None | Some(ChangeOutcome::Exists) => {
                self.followups.push(Command::LocalList { path: Some(item.local.clone()) });
                if let Some(w) = &mut self.explorer.there.walk {
                    w.step = Some(WalkStep::Listing(item));
                }
            }
            Some(o) => {
                let label = self.folder_label();
                let msg =
                    self.notice_of(&RemoteChange::MakeDir { parent: parent.clone(), name: name.clone() }, o, label);
                self.walk_abort(msg);
            }
        }
        Some(Changed::VIEWER)
    }

    /// `Reply::LocalListing`: the walk's, or the left pane's (returned back).
    pub(crate) fn walk_listed(&mut self, asked: Option<&Command>, l: LocalListing) -> Result<Changed, LocalListing> {
        let Some(Walk { step: Some(WalkStep::Listing(item)), .. }) = &self.explorer.there.walk else { return Err(l) };
        let item = item.clone();
        let same = match asked {
            Some(Command::LocalList { path: Some(p) }) => *p == item.local,
            _ => l.path == item.local,
        };
        if !same {
            return Err(l);
        }
        let Some(session_id) = self.session_id() else { return Err(l) };
        let remote = join(&item.parent, &item.name);
        let all: Vec<&DirEntryView> = l.entries.iter().chain(&l.hidden).collect();
        let files: Vec<PathBuf> = all.iter().filter(|e| !e.is_dir).map(|e| item.local.join(&e.name)).collect();
        let dirs: Vec<&&DirEntryView> = all.iter().filter(|e| e.is_dir).collect();
        let Some(walk) = self.explorer.there.walk.as_mut() else { return Err(l) };
        walk.files += files.len();
        if walk.files > MAX_JOB_FILES || (!dirs.is_empty() && item.depth >= MAX_DEPTH) {
            // The job does not go on (FILES-UI.md 5: 10 000 files and 32 levels). The closest text there is.
            self.walk_abort(Msg::new(MsgKey::ExplorerOpFailed));
            return Ok(Changed::VIEWER);
        }
        for d in dirs {
            walk.queue.push_back(WalkItem {
                local: item.local.join(&d.name),
                parent: remote.clone(),
                name: d.name.clone(),
                depth: item.depth + 1,
            });
        }
        walk.step = None;
        if !files.is_empty() {
            self.followups.push(Command::FilesSend { session_id, local_paths: files, remote_dir: Some(remote) });
        }
        if let Some(next) = self.walk_advance() {
            self.followups.push(next);
        }
        Ok(Changed::VIEWER)
    }

    /// The walk's listing failed (`path` is what was asked): the walk stops, the files sent stay sent.
    pub(crate) fn walk_list_failed(&mut self, path: Option<&PathBuf>) -> bool {
        let Some(Walk { step: Some(WalkStep::Listing(item)), .. }) = &self.explorer.there.walk else { return false };
        if path.is_none_or(|p| *p != item.local) {
            return false;
        }
        self.walk_abort(Msg::new(MsgKey::ExplorerNotPermitted));
        true
    }

    // ----- what the screen and the controls read -----

    pub(crate) fn there_trash_card(&self) -> Option<CardView> {
        let (_, names) = self.explorer.there.trash.as_ref()?;
        let peer = self.peer_name();
        let key = if self.peer_is_windows() {
            MsgKey::ExplorerTrashThereTitleWindows
        } else {
            MsgKey::ExplorerTrashThereTitle
        };
        Some(CardView {
            title: self.tm(&Msg::new(key).with(names.len().to_string()).with(peer.clone())),
            lines: vec![self.tm(&Msg::new(MsgKey::ExplorerTrashThereBody).with(peer))],
            check: None,
            buttons: vec![
                CardButton {
                    id: ControlId::ExpThereTrashCancel,
                    label: self.t(MsgKey::Cancel),
                    style: CardStyle::Default,
                    enabled: true,
                    why: None,
                },
                CardButton {
                    id: ControlId::ExpThereTrashConfirm,
                    label: self.t(MsgKey::ExplorerDelete),
                    style: CardStyle::Danger,
                    enabled: true,
                    why: None,
                },
            ],
        })
    }

    pub(crate) fn conflict_card(&self) -> Option<CardView> {
        let c = self.explorer.there.conflict.as_ref()?;
        let dest = if c.job.to_there { self.crumbs_there() } else { self.crumbs_here() }.pop().unwrap_or_default();
        let title = if c.names.len() == 1 {
            self.tm(&Msg::new(MsgKey::ExplorerConflictOneTitle).with(c.names[0].clone()).with(dest))
        } else {
            self.tm(&Msg::new(MsgKey::ExplorerConflictTitle).with(c.names.len().to_string()).with(dest))
        };
        let mut lines: Vec<String> = c.names.iter().take(3).cloned().collect();
        lines.push(self.t(MsgKey::ExplorerConflictReplaceNote));
        let block = self.replace_block(c.job.to_there).map(|m| self.tm(&m));
        let btn = |id, key, style, enabled, why| CardButton { id, label: self.t(key), style, enabled, why };
        Some(CardView {
            title,
            lines,
            check: (c.names.len() > 1)
                .then(|| (ControlId::ExpConflictApplyAll, self.t(MsgKey::ExplorerConflictApplyAll), c.apply_all)),
            buttons: vec![
                btn(ControlId::ExpConflictCancel, MsgKey::Cancel, CardStyle::Default, true, None),
                btn(ControlId::ExpConflictSkip, MsgKey::ExplorerConflictSkip, CardStyle::Default, true, None),
                btn(
                    ControlId::ExpConflictReplace,
                    MsgKey::ExplorerConflictReplace,
                    CardStyle::Danger,
                    block.is_none(),
                    block,
                ),
                btn(ControlId::ExpConflictKeepBoth, MsgKey::ExplorerConflictKeepBoth, CardStyle::Primary, true, None),
            ],
        })
    }

    /// The slice 2b cards in the accessibility tree: each a dialog with its heading.
    pub(crate) fn remote_cards_controls(&self, s: &mut Specs) {
        for card in [self.there_trash_card(), self.conflict_card()].into_iter().flatten() {
            s.region(super::Region::Dialog);
            s.heading(2, card.title.clone());
            for l in &card.lines {
                s.text(UiRole::Text, l.clone());
            }
            if let Some((id, label, on)) = &card.check {
                let c = s.add(*id, UiRole::CheckBox, label.clone());
                c.toggled = Some(*on);
            }
            for b in &card.buttons {
                let c = s.add(b.id, UiRole::Button, b.label.clone());
                c.disabled = !b.enabled;
                c.description = b.why.clone();
            }
            s.region(super::Region::Content);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Skip,
    Replace,
    KeepBoth,
}

#[cfg(test)]
#[path = "explorer_remote_tests.rs"]
mod tests;
