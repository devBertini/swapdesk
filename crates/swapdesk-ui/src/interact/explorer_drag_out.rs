//! Slice 5 of the explorer (`docs/FILES-UI.md` 4.3): a drag from the right pane out to the system's file manager.
//!
//! The flow, in the order the pieces use it:
//!
//! 1. [`UiState::drag_left_window`]: the shell tells the state the pointer left the session window with the button
//!    held. A drag from the RIGHT pane (files only; folders are refused in 1.1) becomes [`Effect::SystemDrag`] and the
//!    in-app drag ends; a drag from the left pane changes nothing.
//! 2. The shell calls [`UiState::take_system_drag`] and gets a [`DragOut`]: the files offered and a thread-safe
//!    handle. It gives the handle a waker ([`DragOut::set_waker`], which posts a message to the UI thread) and starts
//!    the system's drag (Windows `DoDragDrop`, Mac `beginDraggingSession`).
//! 3. When the system asks for a file, the shell calls [`DragOut::request`] (the staging folder is made on a worker), then [`DragOut::wait_bytes`] / [`DragOut::wait_done`], which
//!    block until the bytes are there. The waker fires; the UI thread calls [`UiState::system_drag_pump`] and
//!    dispatches the commands (`FilesGet` into the staging folder).
//! 4. Transfer events reach the handle through the state; a failure fails the waiting reads and removes the staging
//!    file (on a worker thread). The state touches no file: every file-system call runs on the caller's thread of
//!    [`DragOut`] or on a worker it starts.
//! 5. When the system is done with the drag the shell calls [`DragOut::release`]: unfinished transfers are
//!    cancelled and the staging folder is removed.
//!
//! This file is a child of `explorer_drag` so it can reach the drag's private state.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::{Activation, Changed, Command, UiState, join};
use crate::api::{Permission, TransferState};
use crate::state::Effect;

/// Longest file name a `FILEDESCRIPTORW` holds (`MAX_PATH` of 260 UTF-16 units, one of them the terminator). A
/// longer name is left out of the drag (a remote name is at most 255 units in practice, so this never happens).
pub const MAX_NAME_UNITS: usize = 259;
/// A transfer paused (link lost) for this long fails the read that waits for it.
pub const PAUSE_LIMIT: Duration = Duration::from_secs(60);
/// Suffixes the receiving engine gives its working files (`swapdesk-files` `recv.rs`).
const PART: &str = ".swapdesk-part";
const META: &str = ".swapdesk-meta";

/// One file the drag offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragOutFile {
    pub name: String,
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub mtime_ms: u64,
}

/// Why a read of a promised file fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullError {
    /// The transfer failed (link lost, refused, verification failed) or the staging folder could not be made.
    Failed,
    /// The drag ended (or was cancelled) before the bytes came.
    Released,
}

/// What a waiting read may use right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Avail {
    /// The transfer is still running: the first `limit` bytes of `path` (the partial file) are there. The last byte
    /// is held back until the SHA-256 is verified, so a failed check always fails a read.
    Partial { path: PathBuf, limit: u64 },
    /// The file is whole and verified.
    Whole(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pull {
    Idle,
    Asked,
    Running { done: u64, total: u64 },
    Paused,
    Done,
    Failed,
}

impl Pull {
    fn settled(self) -> bool {
        matches!(self, Pull::Idle | Pull::Done | Pull::Failed)
    }
}

struct Slot {
    pull: Pull,
    transfer: Option<u32>,
    saved_as: Option<String>,
    paused_since: Option<Instant>,
}

type Waker = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    slots: Vec<Slot>,
    queue: Vec<usize>,
    released: bool,
    waker: Option<Waker>,
}

/// The files of one drag out and the state of their downloads; shared between the UI thread and the threads the
/// system calls.
pub struct DragOut {
    session_id: u64,
    /// The remote folder the files lie in.
    folder: String,
    files: Vec<DragOutFile>,
    dir: PathBuf,
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl std::fmt::Debug for DragOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DragOut").field("dir", &self.dir).field("files", &self.files.len()).finish()
    }
}

impl DragOut {
    /// A drag of `files` of `folder` on the other computer, downloading into `dir` (not created yet).
    pub fn new(session_id: u64, folder: String, files: Vec<DragOutFile>, dir: PathBuf) -> Arc<Self> {
        let slots = files
            .iter()
            .map(|_| Slot { pull: Pull::Idle, transfer: None, saved_as: None, paused_since: None })
            .collect();
        Arc::new(Self {
            session_id,
            folder,
            files,
            dir,
            inner: Mutex::new(Inner { slots, queue: Vec::new(), released: false, waker: None }),
            cv: Condvar::new(),
        })
    }

    pub fn files(&self) -> &[DragOutFile] {
        &self.files
    }

    /// The staging folder of this drag.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The staging path of file `i` while it is partial.
    pub fn part_path(&self, i: usize) -> PathBuf {
        self.dir.join(format!("{}{PART}", self.files[i].name))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The function that wakes the UI thread (it must call [`UiState::system_drag_pump`] there).
    pub fn set_waker(&self, waker: Waker) {
        self.lock().waker = Some(waker);
    }

    fn wake(&self) {
        let waker = self.lock().waker.clone();
        if let Some(w) = waker {
            w();
        }
    }

    /// The system asked for file `i`: queues the download and makes the staging folder on a worker thread (the
    /// asking thread may be the UI thread, which touches no file). Asking again is harmless.
    pub fn request(self: &Arc<Self>, i: usize) -> Result<(), PullError> {
        {
            let mut g = self.lock();
            if g.released {
                return Err(PullError::Released);
            }
            match g.slots.get(i).map(|s| s.pull) {
                None | Some(Pull::Failed) => return Err(PullError::Failed),
                Some(Pull::Idle) => g.slots[i].pull = Pull::Asked,
                Some(_) => return Ok(()),
            }
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let made = std::fs::create_dir_all(&me.dir);
            {
                let mut g = me.lock();
                if made.is_err() {
                    g.slots[i].pull = Pull::Failed;
                    me.cv.notify_all();
                    return;
                }
                g.queue.push(i);
            }
            me.wake();
        });
        Ok(())
    }

    /// Blocks until at least `upto` bytes of file `i` can be read (see [`Avail`]), or the file is whole, or the
    /// transfer failed. Never call it from the UI thread.
    pub fn wait_bytes(&self, i: usize, upto: u64) -> Result<Avail, PullError> {
        let mut g = self.lock();
        loop {
            if g.slots.get(i).is_none() {
                return Err(PullError::Failed);
            }
            let name = g.slots[i].saved_as.clone().unwrap_or_else(|| self.files[i].name.clone());
            match g.slots[i].pull {
                Pull::Failed => return Err(PullError::Failed),
                Pull::Done => return Ok(Avail::Whole(self.dir.join(name))),
                _ if g.released => return Err(PullError::Released),
                Pull::Running { done, total } => {
                    let limit = done.min(total.saturating_sub(1));
                    if limit >= upto {
                        return Ok(Avail::Partial { path: self.part_path(i), limit });
                    }
                }
                Pull::Paused => {
                    if g.slots[i].paused_since.is_some_and(|t| t.elapsed() > PAUSE_LIMIT) {
                        g.slots[i].pull = Pull::Failed;
                        self.cv.notify_all();
                        drop(g);
                        self.spawn_remove_file(i);
                        return Err(PullError::Failed);
                    }
                }
                Pull::Idle | Pull::Asked => {}
            }
            g = self.cv.wait_timeout(g, Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Blocks until file `i` is whole and verified; its path, or why not.
    pub fn wait_done(&self, i: usize) -> Result<PathBuf, PullError> {
        match self.wait_bytes(i, u64::MAX)? {
            Avail::Whole(p) => Ok(p),
            Avail::Partial { .. } => Err(PullError::Failed),
        }
    }

    /// The system is done with the drag (the data object is gone, the session ended, every promise was kept or
    /// broken): the waiting reads fail, unfinished transfers are cancelled and the staging folder is removed.
    pub fn release(&self) {
        self.lock().released = true;
        self.cv.notify_all();
        self.wake();
    }

    /// Settles file `i` as a finished (`ok`) or failed transfer without the backend: for the shells' tests of their
    /// COM and AppKit objects.
    #[doc(hidden)]
    pub fn settle_for_test(&self, i: usize, ok: bool) {
        self.lock().slots[i].pull = if ok { Pull::Done } else { Pull::Failed };
        self.cv.notify_all();
    }

    /// Everything that was asked has ended, one way or the other.
    fn settled(&self) -> bool {
        self.lock().slots.iter().all(|s| s.pull.settled())
    }

    fn drain(&self) -> (Vec<usize>, Vec<u32>, bool) {
        let mut g = self.lock();
        let mut asks = std::mem::take(&mut g.queue);
        let released = g.released;
        if released {
            // Nothing new starts once the system let go: the files asked for in the meantime fail.
            for i in asks.drain(..) {
                if g.slots[i].pull == Pull::Asked && g.slots[i].transfer.is_none() {
                    g.slots[i].pull = Pull::Failed;
                }
            }
        }
        let cancels = if released {
            g.slots.iter().filter(|s| !s.pull.settled()).filter_map(|s| s.transfer).collect()
        } else {
            Vec::new()
        };
        (asks, cancels, released)
    }

    /// A `Transfer` event; the id to cancel when the drag is already released and the transfer still lives.
    fn on_transfer(&self, id: u32, session_id: u64, name: &str, outgoing: bool, state: &TransferState) -> Option<u32> {
        if outgoing || session_id != self.session_id {
            return None;
        }
        let mut g = self.lock();
        let released = g.released;
        let i = g.slots.iter().position(|s| s.transfer == Some(id)).or_else(|| {
            let i = (0..self.files.len()).find(|&i| {
                g.slots[i].transfer.is_none() && g.slots[i].pull == Pull::Asked && self.files[i].name == name
            })?;
            g.slots[i].transfer = Some(id);
            Some(i)
        })?;
        let slot = &mut g.slots[i];
        if slot.pull.settled() {
            return None;
        }
        let was_paused = slot.pull == Pull::Paused;
        slot.pull = match *state {
            TransferState::Queued => Pull::Asked,
            TransferState::Running { done, total, .. } => Pull::Running { done, total },
            TransferState::Paused { .. } => Pull::Paused,
            TransferState::Done => Pull::Done,
            TransferState::Failed { .. } => Pull::Failed,
        };
        slot.paused_since = match (slot.pull, was_paused) {
            (Pull::Paused, true) => slot.paused_since,
            (Pull::Paused, false) => Some(Instant::now()),
            _ => None,
        };
        let failed = slot.pull == Pull::Failed;
        let alive = !slot.pull.settled();
        drop(g);
        self.cv.notify_all();
        if failed {
            self.spawn_remove_file(i);
        }
        (released && alive).then_some(id)
    }

    fn on_saved_as(&self, id: u32, name: &str) {
        if let Some(s) = self.lock().slots.iter_mut().find(|s| s.transfer == Some(id)) {
            s.saved_as = Some(name.to_string());
        }
    }

    /// Removes what a failed or cancelled transfer left of file `i` (on a worker thread).
    fn spawn_remove_file(&self, i: usize) {
        let (dir, name) = (self.dir.clone(), self.files[i].name.clone());
        std::thread::spawn(move || {
            for n in [name.clone(), format!("{name}{PART}"), format!("{name}{META}")] {
                let _ = std::fs::remove_file(dir.join(n));
            }
        });
    }

    /// Removes the staging folder (on a worker thread; it retries a few times while the engine lets go of a file).
    fn spawn_remove_dir(&self) {
        let dir = self.dir.clone();
        std::thread::spawn(move || {
            for _ in 0..10 {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => return,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                    Err(_) => std::thread::sleep(Duration::from_millis(500)),
                }
            }
        });
    }
}

/// The folder holding the staging folders of drags out: `%LOCALAPPDATA%\Swapdesk\staging`,
/// `~/Library/Caches/Swapdesk/staging` (no file-system call).
pub fn staging_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("Swapdesk").join("staging")
}

/// Removes the leftovers of a crash: call once at start. Runs on a worker thread.
pub fn clean_stale_staging(root: PathBuf) {
    std::thread::spawn(move || {
        let _ = std::fs::remove_dir_all(root);
    });
}

fn next_dir(root: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(1);
    root.join(N.fetch_add(1, Ordering::Relaxed).to_string())
}

/// What the explorer remembers of drags out.
#[derive(Default, Clone)]
pub(crate) struct OutState {
    /// Made by `drag_left_window`, not yet taken by the shell.
    pending: Option<Arc<DragOut>>,
    /// The drag whose downloads the state still follows.
    current: Option<Arc<DragOut>>,
    root: Option<PathBuf>,
}

impl std::fmt::Debug for OutState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutState").field("pending", &self.pending.is_some()).field("current", &self.current).finish()
    }
}

impl PartialEq for OutState {
    fn eq(&self, other: &Self) -> bool {
        let same = |a: &Option<Arc<DragOut>>, b: &Option<Arc<DragOut>>| match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        same(&self.pending, &other.pending) && same(&self.current, &other.current)
    }
}

impl UiState {
    /// Where staging folders go (default [`staging_root`]); tests and shells with another place set it.
    pub fn set_staging_root(&mut self, root: PathBuf) {
        self.explorer.drag.out.root = Some(root);
    }

    /// Fills the sizes and times of the drag that just started from the pane's current rows.
    pub(super) fn out_capture_meta(&mut self) {
        let Some(drag) = &self.explorer.drag.active else { return };
        let meta: Vec<(u64, u64)> = {
            let rows = self.sorted_rows(drag.there);
            drag.items
                .iter()
                .map(|(n, _)| rows.iter().find(|r| &r.name == n).map_or((0, 0), |r| (r.size, r.mtime_ms)))
                .collect()
        };
        if let Some(d) = &mut self.explorer.drag.active {
            d.meta = meta;
        }
    }

    /// The pointer left the session window with the left button held. A drag from the right pane carrying files
    /// becomes a system drag ([`Effect::SystemDrag`]; the shell calls [`UiState::take_system_drag`]) and the in-app
    /// drag ends. A drag from the left pane, or one with nothing to promise, changes nothing (the drag goes on).
    pub fn drag_left_window(&mut self) -> Activation {
        let mut a = Activation::default();
        let Some(drag) = &self.explorer.drag.active else { return a };
        if !drag.there || self.explorer.drag.out.current.as_ref().is_some_and(|c| !c.settled()) {
            return a;
        }
        let Some(session_id) = self.session_id() else { return a };
        if self.there_error_reason().is_some() || self.granted().is_some_and(|g| !g.has(Permission::Files)) {
            return a;
        }
        let files: Vec<DragOutFile> = drag
            .items
            .iter()
            .zip(&drag.meta)
            .filter(|((name, is_dir), _)| !*is_dir && name.encode_utf16().count() <= MAX_NAME_UNITS)
            .map(|((name, _), (size, mtime_ms))| DragOutFile { name: name.clone(), size: *size, mtime_ms: *mtime_ms })
            .collect();
        if files.is_empty() {
            return a;
        }
        let folder = drag.folder.clone();
        let root = self.explorer.drag.out.root.clone().unwrap_or_else(staging_root);
        let out = DragOut::new(session_id, folder, files, next_dir(&root));
        let o = &mut self.explorer.drag.out;
        // An older drag still downloading is let go: its files are the system's to finish or fail.
        if let Some(old) = o.current.replace(out.clone()) {
            old.release();
        }
        o.pending = Some(out);
        let d = &mut self.explorer.drag;
        d.active = None;
        d.press = None;
        d.swallow = true;
        a.effect = Some(Effect::SystemDrag);
        a.changed = Changed::VIEWER;
        a
    }

    /// The drag the last [`Effect::SystemDrag`] announced.
    pub fn take_system_drag(&mut self) -> Option<Arc<DragOut>> {
        self.explorer.drag.out.pending.take()
    }

    /// Call on the UI thread when the drag's waker fired: the downloads the system asked for start (`FilesGet` into
    /// the staging folder) and a released drag's unfinished transfers are cancelled.
    pub fn system_drag_pump(&mut self) -> Activation {
        let mut a = Activation::default();
        let Some(out) = self.explorer.drag.out.current.clone() else { return a };
        let (asks, cancels, released) = out.drain();
        for i in asks {
            a.commands.push(Command::FilesGet {
                session_id: out.session_id,
                remote_path: join(&out.folder, &out.files[i].name),
                local_dir: out.dir.clone(),
            });
        }
        a.commands.extend(cancels.into_iter().map(|transfer_id| Command::TransferCancel { transfer_id }));
        if released {
            self.system_drag_done_if_settled(&out);
        }
        a
    }

    fn system_drag_done_if_settled(&mut self, out: &Arc<DragOut>) {
        // Cancelled transfers settle through their own events; the folder goes when the last one has.
        if out.settled() && self.explorer.drag.out.current.as_ref().is_some_and(|c| Arc::ptr_eq(c, out)) {
            self.explorer.drag.out.current = None;
            out.spawn_remove_dir();
        }
    }

    /// Hook of `apply(UiEvent::Transfer)`.
    pub(crate) fn system_drag_transfer(
        &mut self,
        id: u32,
        session_id: u64,
        name: &str,
        outgoing: bool,
        state: &TransferState,
    ) {
        let Some(out) = self.explorer.drag.out.current.clone() else { return };
        if let Some(cancel) = out.on_transfer(id, session_id, name, outgoing, state) {
            self.followups.push(Command::TransferCancel { transfer_id: cancel });
        }
        if out.lock().released {
            self.system_drag_done_if_settled(&out);
        }
    }

    /// Hook of `apply(UiEvent::TransferSavedAs)`.
    pub(crate) fn system_drag_saved_as(&mut self, id: u32, name: &str) {
        if let Some(out) = &self.explorer.drag.out.current {
            out.on_saved_as(id, name);
        }
    }
}

#[cfg(test)]
#[path = "explorer_drag_out_tests.rs"]
mod tests;
