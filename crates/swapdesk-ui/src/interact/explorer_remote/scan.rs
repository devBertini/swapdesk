//! Walks of the other computer's folders by its listings (`docs/FILES-UI.md` 5): the download of a folder (right to
//! left) and the count behind the right pane's properties card.
//!
//! * One [`Scan`] lists folders one after the other, every page of each (`FilesListPage`), breadth first.
//! * A download scans the whole tree first, so the limits (10 000 files, 32 levels) refuse before anything is made
//!   or sent. Then the local folders are made, one at a time, by `LocalMakeDir` (the backend's worker thread, never
//!   the interface's), parents before children; only then do the files go, each `FilesGet` into its local folder.
//! * The pane's own listing and a scan never share a reply: the scan takes the page whose path and start it waits
//!   for. (A pane listing the same folder at the same moment would lose that page; the pane lists again on its next
//!   change.)

use std::collections::VecDeque;
use std::path::PathBuf;

use super::{MAX_DEPTH, MAX_JOB_FILES};
use crate::api::{ApiCode, Command, Feature, Msg, RemoteKind, RemotePage};
use crate::interact::explorer::{human_size, join};
use crate::local_disk::safe_join;
use crate::state::{Changed, UiState};
use crate::strings::MsgKey;

/// One folder to list: its path there, where it goes here (unused by a count) and how deep it lies (1 = picked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanDir {
    pub remote: String,
    pub local: PathBuf,
    pub depth: u16,
}

/// A folder being listed: the start the next page must carry and what its pages brought so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanCur {
    pub dir: ScanDir,
    pub next: u32,
    /// Name, kind and size of every entry.
    pub entries: Vec<(String, RemoteKind, u64)>,
    /// Pages taken so far: a host that never ends a folder is stopped (see [`MAX_PAGES`]).
    pub pages: u32,
}

/// Most pages one folder may take. A page holds at least one entry unless the names on it were all refused, so
/// this lets through every folder the limits allow and ends the endless one.
const MAX_PAGES: u32 = MAX_JOB_FILES as u32;

/// Why a scan stopped on a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Past the limit of files (and folders) of a job.
    TooBig,
    /// The host's pages do not move forward (`next` is not past `start`) or never end.
    Endless,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    pub queue: VecDeque<ScanDir>,
    pub cur: Option<ScanCur>,
}

/// What one page did to a scan.
enum Fed {
    /// Not the page the scan waits for.
    Foreign,
    /// The folder goes on: the next page to ask.
    More(String, u32),
    /// The folder is complete.
    Folder(ScanCur),
    /// The scan ends here, before the folder is complete.
    Stop(Stop),
}

impl Scan {
    pub(crate) fn begin(&mut self) -> Option<(String, u32)> {
        let dir = self.queue.pop_front()?;
        let path = dir.remote.clone();
        self.cur = Some(ScanCur { dir, next: 0, entries: Vec::new(), pages: 0 });
        Some((path, 0))
    }

    /// `already` is what the job holds before this folder (files or folders, whichever is more): the limit of a job
    /// is checked here, page by page, so a host cannot grow the viewer's memory by never ending a folder.
    fn feed(&mut self, path: &str, start: u32, page: &RemotePage, already: usize) -> Fed {
        let Some(cur) = &mut self.cur else { return Fed::Foreign };
        if cur.dir.remote != path || cur.next != start || page.start != start {
            return Fed::Foreign;
        }
        cur.pages += 1;
        let stop = if page.next.is_some_and(|n| n <= start) || cur.pages > MAX_PAGES {
            Some(Stop::Endless)
        } else if already.saturating_add(cur.entries.len()).saturating_add(page.entries.len()) > MAX_JOB_FILES {
            Some(Stop::TooBig)
        } else {
            None
        };
        if let Some(stop) = stop {
            self.cur = None;
            return Fed::Stop(stop);
        }
        cur.entries.extend(page.entries.iter().map(|e| (e.name.clone(), e.kind, e.size)));
        match page.next {
            Some(next) => {
                cur.next = next;
                Fed::More(cur.dir.remote.clone(), next)
            }
            None => Fed::Folder(self.cur.take().expect("current folder")),
        }
    }

    fn waits_for(&self, path: &str, start: u32) -> bool {
        self.cur.as_ref().is_some_and(|c| c.dir.remote == path && c.next == start)
    }
}

/// A folder download under way.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Download {
    pub scan: Scan,
    /// Local folders to make, parents first.
    pub folders: Vec<PathBuf>,
    /// Files to fetch: their path there and the local folder they land in.
    pub files: Vec<(String, PathBuf)>,
    /// The folders made so far; `Some` once the scan is over and the making began.
    pub made: Option<usize>,
    /// What the copy's own files wait on: the bin command of a replace, and the commands that follow.
    pub starts: Vec<(Option<Command>, Vec<Command>)>,
}

/// What a properties card counted so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Counted {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub truncated: bool,
}

/// The right pane's properties card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThereProps {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_ms: u64,
    /// The count of a folder: running while `scan` is `Some`.
    pub scan: Option<Scan>,
    pub counted: Counted,
}

fn partial_name(name: &str) -> bool {
    name.ends_with(".swapdesk-part") || name.ends_with(".swapdesk-meta")
}

impl UiState {
    // ----- the download of a folder -----

    /// Whether a folder can come from the other computer; the note's key when it cannot (a 1.0 peer keeps today's
    /// behaviour: folders are left out).
    pub(crate) fn download_folder_block(&self) -> Option<MsgKey> {
        (!self.there_v11()).then_some(MsgKey::ExplorerFoldersLater)
    }

    /// A download is making its local folders: another one cannot join it now.
    pub(crate) fn download_making(&self) -> bool {
        self.explorer.there.download.as_ref().is_some_and(|d| d.made.is_some())
    }

    /// Starts scanning the picked folders of `remote_dir` toward `local_dir` (or joins the scan under way); the
    /// command is the first page to ask, when the scan was idle. Call only when not [`Self::download_making`].
    pub(crate) fn download_start(
        &mut self,
        folders: Vec<String>,
        local_dir: &std::path::Path,
        remote_dir: &str,
        start: (Option<Command>, Vec<Command>),
    ) -> Option<Command> {
        let session_id = self.session_id()?;
        let t = &mut self.explorer.there;
        let dl = t.download.get_or_insert_with(Download::default);
        let idle = dl.scan.cur.is_none();
        dl.starts.push(start);
        let mut refused = false;
        for name in folders {
            // The one checked join: a name that is not one plain component is never a folder of ours.
            let Some(local) = safe_join(local_dir, &name) else {
                refused = true;
                continue;
            };
            dl.folders.push(local.clone());
            dl.scan.queue.push_back(ScanDir { remote: join(remote_dir, &name), local, depth: 1 });
        }
        let first = if idle { dl.scan.begin() } else { None };
        if first.is_none() && idle {
            // Every folder was refused: only the files of the copy are left, and they go now.
            dl.made = Some(0);
        }
        if refused {
            t.notice = Some(Msg::new(MsgKey::ExplorerNameInvalid));
        }
        match first {
            Some((path, start)) => Some(Command::FilesListPage { session_id, path, start }),
            None => {
                if idle {
                    self.download_make();
                }
                None
            }
        }
    }

    fn download_abort(&mut self, notice: Msg) {
        self.explorer.there.download = None;
        self.explorer.there.notice = Some(notice);
    }

    pub(super) fn too_big(&self) -> Msg {
        Msg::new(MsgKey::ExplorerJobTooBig).with(crate::format::count(MAX_JOB_FILES as u64)).with(MAX_DEPTH.to_string())
    }

    fn failure_notice(code: ApiCode) -> Msg {
        Msg::new(match code {
            ApiCode::NotPermitted => MsgKey::ExplorerNotPermitted,
            ApiCode::NotFound => MsgKey::ExplorerGone,
            _ => MsgKey::ExplorerOpFailed,
        })
    }

    /// A folder of the download is listed in full: its files are planned, its folders queued (limits checked).
    fn download_folder(&mut self, cur: ScanCur) {
        let Some(dl) = &mut self.explorer.there.download else { return };
        let mut dirs = Vec::new();
        for (name, kind, _) in cur.entries {
            match kind {
                RemoteKind::File if !partial_name(&name) => {
                    dl.files.push((join(&cur.dir.remote, &name), cur.dir.local.clone()));
                }
                RemoteKind::Dir => dirs.push(name),
                RemoteKind::File | RemoteKind::Link => {}
            }
        }
        if dl.files.len() > MAX_JOB_FILES
            || dl.folders.len() + dirs.len() > MAX_JOB_FILES
            || (!dirs.is_empty() && cur.dir.depth >= MAX_DEPTH)
        {
            self.download_abort(self.too_big());
            return;
        }
        let mut refused = false;
        for name in dirs {
            let Some(local) = safe_join(&cur.dir.local, &name) else {
                refused = true;
                continue;
            };
            dl.folders.push(local.clone());
            dl.scan.queue.push_back(ScanDir { remote: join(&cur.dir.remote, &name), local, depth: cur.dir.depth + 1 });
        }
        if refused {
            self.explorer.there.notice = Some(Msg::new(MsgKey::ExplorerNameInvalid));
        }
        self.download_next();
    }

    /// The next folder to list, or the making of the local folders when the scan is over.
    fn download_next(&mut self) {
        let Some(session_id) = self.session_id() else { return };
        let Some(dl) = &mut self.explorer.there.download else { return };
        if let Some((path, start)) = dl.scan.begin() {
            self.followups.push(Command::FilesListPage { session_id, path, start });
            return;
        }
        dl.made = Some(0);
        self.download_make();
    }

    /// Makes the next local folder, or sends the files when all exist.
    fn download_make(&mut self) {
        let Some(dl) = &mut self.explorer.there.download else { return };
        let Some(made) = dl.made else { return };
        if let Some(path) = dl.folders.get(made) {
            self.followups.push(Command::LocalMakeDir { path: path.clone() });
            return;
        }
        let Some(dl) = self.explorer.there.download.take() else { return };
        if let Some(session_id) = self.session_id() {
            for (trash, copy) in dl.starts {
                let cmds = self.release(trash, copy);
                self.followups.extend(cmds);
            }
            for (remote_path, local_dir) in dl.files {
                self.followups.push(Command::FilesGet { session_id, remote_path, local_dir });
            }
        }
        self.explorer.transfers_open = true;
        // The new folders show in the left pane.
        let here = self.explorer.here_dir.clone();
        self.followups.push(Command::LocalList { path: here });
    }

    /// A `LocalMakeDir` answered; `failed` is its code when it failed. `None` when it is not the download's.
    pub(crate) fn download_made(&mut self, path: &std::path::Path, failed: Option<ApiCode>) -> Option<Changed> {
        let dl = self.explorer.there.download.as_ref()?;
        let made = dl.made?;
        if dl.folders.get(made).is_none_or(|p| p != path) {
            return None;
        }
        match failed {
            // A folder that exists is merged into, as the system's file managers do.
            None | Some(ApiCode::Refused) => {
                if let Some(dl) = &mut self.explorer.there.download {
                    dl.made = Some(made + 1);
                }
                self.download_make();
            }
            Some(code) => self.download_abort(Self::failure_notice(code)),
        }
        Some(Changed::VIEWER)
    }

    // ----- the pages -----

    /// `Reply::RemotePage` for a scan (a download's or a count's); the page comes back when it is the pane's.
    pub(crate) fn scan_page(&mut self, asked: Option<&(String, u32)>, page: RemotePage) -> Result<Changed, RemotePage> {
        let Some((path, start)) = asked else { return Err(page) };
        let Some(session_id) = self.session_id() else { return Err(page) };
        let t = &mut self.explorer.there;
        let fed = match (&mut t.download, &mut t.props) {
            (Some(dl), _) if dl.made.is_none() && dl.scan.waits_for(path, *start) => {
                let held = dl.files.len().max(dl.folders.len());
                (true, dl.scan.feed(path, *start, &page, held))
            }
            (_, Some(ThereProps { scan: Some(scan), counted, .. })) if scan.waits_for(path, *start) => {
                let held = usize::try_from(counted.files + counted.folders).unwrap_or(usize::MAX);
                (false, scan.feed(path, *start, &page, held))
            }
            _ => return Err(page),
        };
        self.consent_answered(Feature::Files);
        let (download, fed) = fed;
        match fed {
            Fed::Foreign => return Err(page),
            Fed::More(path, start) => self.followups.push(Command::FilesListPage { session_id, path, start }),
            Fed::Folder(cur) if download => self.download_folder(cur),
            Fed::Folder(cur) => self.props_folder(cur),
            Fed::Stop(stop) if download => {
                let notice = match stop {
                    Stop::TooBig => self.too_big(),
                    Stop::Endless => Msg::new(MsgKey::ExplorerOpFailed),
                };
                self.download_abort(notice);
            }
            Fed::Stop(_) => {
                // The count ends with what it has.
                if let Some(p) = &mut self.explorer.there.props {
                    p.counted.truncated = true;
                    p.scan = None;
                }
            }
        }
        Ok(Changed::VIEWER)
    }

    /// A scan's page failed: the scan stops with the reason. False when the page was not one the scans wait for.
    pub(crate) fn scan_failed(&mut self, path: &str, start: u32, code: ApiCode) -> bool {
        let t = &self.explorer.there;
        if t.download.as_ref().is_some_and(|d| d.made.is_none() && d.scan.waits_for(path, start)) {
            self.download_abort(Self::failure_notice(code));
            return true;
        }
        if t.props.as_ref().and_then(|p| p.scan.as_ref()).is_some_and(|s| s.waits_for(path, start)) {
            self.explorer.there.props = None;
            self.explorer.there.notice = Some(Self::failure_notice(code));
            return true;
        }
        false
    }

    // ----- the right pane's properties -----

    /// Propriedades on the right pane: one selected row, or the shown folder when none is. Needs a 1.1 listing.
    pub(crate) fn there_props_ok(&self) -> bool {
        let t = &self.explorer.there;
        self.explorer.open
            && self.explorer.focus_there
            && self.there_v11()
            && t.listed
            && self.explorer.panes[1].selected.len() <= 1
    }

    /// Opens the card; a folder starts its count (the first page is the returned command).
    pub(crate) fn there_props_open(&mut self) -> Option<Command> {
        if !self.there_props_ok() {
            return None;
        }
        let session_id = self.session_id()?;
        let folder = self.there_folder();
        let picked: Vec<String> = self.explorer.panes[1].selected.iter().cloned().collect();
        let (name, is_dir, size, mtime_ms, remote) = match picked.as_slice() {
            [one] => {
                let e = self.entries(true).iter().find(|e| e.name == *one)?;
                (e.name.clone(), e.is_dir, e.size, e.mtime_ms, join(&folder, &e.name))
            }
            _ => (self.crumbs_there().pop().unwrap_or_default(), true, 0, 0, folder),
        };
        self.explorer.manage.props = None;
        let mut props = ThereProps { name, is_dir, size, mtime_ms, scan: None, counted: Counted::default() };
        let mut command = None;
        if is_dir {
            let mut scan = Scan::default();
            scan.queue.push_back(ScanDir { remote, local: PathBuf::new(), depth: 1 });
            let (path, start) = scan.begin()?;
            command = Some(Command::FilesListPage { session_id, path, start });
            props.scan = Some(scan);
        }
        self.explorer.there.props = Some(props);
        command
    }

    /// A folder of the count is listed in full: it is added, its folders queued; the limits stop the count.
    fn props_folder(&mut self, cur: ScanCur) {
        let Some(session_id) = self.session_id() else { return };
        let Some(p) = &mut self.explorer.there.props else { return };
        let Some(scan) = &mut p.scan else { return };
        for (name, kind, size) in cur.entries {
            match kind {
                RemoteKind::Dir => {
                    p.counted.folders += 1;
                    if cur.dir.depth < MAX_DEPTH {
                        scan.queue.push_back(ScanDir {
                            remote: join(&cur.dir.remote, &name),
                            local: PathBuf::new(),
                            depth: cur.dir.depth + 1,
                        });
                    } else {
                        p.counted.truncated = true;
                    }
                }
                RemoteKind::File | RemoteKind::Link => {
                    p.counted.files += 1;
                    p.counted.bytes += size;
                }
            }
        }
        if p.counted.files + p.counted.folders > MAX_JOB_FILES as u64 {
            p.counted.truncated = true;
            p.scan = None;
            return;
        }
        match scan.begin() {
            Some((path, start)) => self.followups.push(Command::FilesListPage { session_id, path, start }),
            None => p.scan = None,
        }
    }

    /// The card's title and lines; `None` when it is not open.
    pub(crate) fn there_props_view(&self) -> Option<(String, Vec<String>)> {
        let p = self.explorer.there.props.as_ref()?;
        let title = self.tm(&Msg::new(MsgKey::ExplorerPropsTitle).with(p.name.clone()));
        let named = |key: MsgKey, value: String| self.tm(&Msg::new(key).with(value));
        let mut lines =
            vec![named(MsgKey::ExplorerPropsKind, crate::interact::explorer::type_of(self.lang, &p.name, p.is_dir))];
        if !p.is_dir {
            lines.push(named(MsgKey::ExplorerPropsSize, human_size(self.lang, p.size)));
        }
        if p.mtime_ms != 0 {
            lines.push(named(
                MsgKey::ExplorerPropsModified,
                crate::format::table_when(self.lang, p.mtime_ms, self.clock),
            ));
        }
        if p.is_dir {
            let c = &p.counted;
            if p.scan.is_some() {
                lines.push(self.t(MsgKey::ExplorerPropsCounting));
            } else {
                lines.push(
                    self.tm(&Msg::new(MsgKey::ExplorerPropsContents)
                        .with(crate::format::count(c.files))
                        .with(crate::format::count(c.folders))
                        .with(human_size(self.lang, c.bytes))),
                );
                if c.truncated {
                    lines.push(
                        self.tm(&Msg::new(MsgKey::ExplorerPropsMore).with(crate::format::count(c.files + c.folders))),
                    );
                }
            }
        }
        if let Some(free) = self.there_free_bytes() {
            lines.push(self.tm(&Msg::new(MsgKey::ExplorerPropsFree).with(human_size(self.lang, free))));
        }
        Some((title, lines))
    }
}
