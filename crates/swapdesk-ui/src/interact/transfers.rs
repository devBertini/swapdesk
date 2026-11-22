//! The viewer session's transfers: the list the explorer shows, and the queue every copy goes through
//! (`docs/FILES-UI.md` 5).
//!
//! * **The queue.** The service runs at most [`MAX_ACTIVE`] unfinished transfers of one session and answers `Busy`
//!   to a start beyond that, so a copy of many files (a folder, several folders, a long selection) is never sent at
//!   once: its files wait here, in order, and start as slots free. A slot is taken by a start that was sent and not
//!   yet answered and by every transfer of the session that is neither done nor failed (a paused one keeps its slot).
//! * **When it moves.** After a copy is queued, after every transfer event of the session, after a start is answered
//!   and when the session is active again. Nothing starts while the session reconnects or is paused: a start then
//!   would fail one file after the other.
//! * **`Busy` all the same** (a transfer the queue does not know took the slot): the files go back to the head of the
//!   line and the next second tries again.
//! * **The copy under way** is everything queued since the queue was last empty; its transfers carry its number, so
//!   the summary counts this copy alone and not what the session copied before.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::api::{ApiCode, Command, Msg, SessionState, TransferState};
use crate::state::{Changed, TransferView, UiState};
use crate::strings::MsgKey;

/// Most rows the transfers panel lists (a copy of thousands of files is told by the bar's summary, not row by row).
pub const TRANSFER_ROWS: usize = 40;

/// The most unfinished transfers the service runs for one session (SPEC F-FILE-01); one more is answered `Busy`.
pub(crate) const MAX_ACTIVE: usize = 8;

/// One file of a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopyItem {
    /// A file of this computer into a folder of the other (`None`: its receive folder).
    Send { local: PathBuf, remote_dir: Option<String>, size: Option<u64> },
    /// A file of the other computer into a folder of this one.
    Get { remote: String, local_dir: PathBuf, size: Option<u64> },
}

impl CopyItem {
    pub(crate) fn size(&self) -> Option<u64> {
        match self {
            CopyItem::Send { size, .. } | CopyItem::Get { size, .. } => *size,
        }
    }

    /// The file's own name, as its row shows it.
    pub(crate) fn name(&self) -> String {
        match self {
            CopyItem::Send { local, .. } => {
                local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            }
            CopyItem::Get { remote, .. } => remote.rsplit(['/', '\\']).next().unwrap_or(remote).to_string(),
        }
    }
}

/// The copies waiting for a slot, and what the copy under way is.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CopyQueue {
    pub waiting: VecDeque<CopyItem>,
    /// Starts sent and not answered yet, each with the files it carries (they hold their slots).
    pub asked: Vec<(Command, Vec<CopyItem>)>,
    /// Starts sent before "Cancelar tudo" and not answered yet: their transfers are cancelled as they are named.
    pub cancelled: Vec<Command>,
    /// Transfers this side cancelled: their rows are gone, and a late note about one makes no new row (a row
    /// nobody can end would hold one of the session's slots for ever).
    pub gone: BTreeSet<u32>,
    /// The service answered `Busy`: the next second tries again.
    pub retry: bool,
    /// The number of the copy under way: its transfers carry it.
    pub batch: u32,
    /// Files of the copy under way whose start the service refused for good.
    pub refused: usize,
    /// Bytes a second, smoothed, and the bytes the copy had done a second ago.
    pub rate: u64,
    pub prev_done: Option<u64>,
}

/// What the transfers bar says about the copy under way.
#[derive(Debug, Clone, PartialEq)]
pub struct CopySummary {
    /// The file being copied now (the first that runs) and how many others run beside it.
    pub now: Option<String>,
    pub others: usize,
    pub items_done: usize,
    pub items_total: usize,
    /// Files that did not arrive (failed or refused).
    pub failed: usize,
    pub bytes_done: u64,
    /// `None` while the size of some file is not known: the fraction then counts files.
    pub bytes_total: Option<u64>,
    /// 0.0 to 1.0, by bytes when every size is known, by files otherwise.
    pub fraction: f32,
    pub bytes_per_s: u64,
    pub seconds_left: Option<u64>,
    /// Files waiting for a slot.
    pub waiting: usize,
    /// The folders of the copy are still being read: the totals may grow.
    pub reading: bool,
    /// Nothing is left to do: every file arrived or failed.
    pub finished: bool,
}

/// The transfers bar as the screen draws it: every text made here, so both systems show the same words.
#[derive(Debug, Clone, PartialEq)]
pub struct TransfersBar {
    /// What the copy does now: "Copiando relatório.pdf e mais 2", "Cópia concluída".
    pub headline: String,
    /// Its numbers: "37% · 12 de 340 itens · 1,1 GB de 1,8 GB · 94 MB/s · falta 1 min".
    pub detail: String,
    /// 0.0 to 1.0 of the copy under way.
    pub fraction: f32,
    /// "Cancelar tudo" has something to cancel.
    pub can_cancel: bool,
    /// The copy is over and files of it did not arrive.
    pub failed: bool,
}

fn unfinished(state: &TransferState) -> bool {
    !matches!(state, TransferState::Done | TransferState::Failed { .. })
}

impl UiState {
    /// The transfers of the viewer session shown (the other sessions' belong to the host role).
    pub fn viewer_transfers(&self) -> Vec<&TransferView> {
        let session = self.viewer_session().map(|s| s.id);
        self.transfers.iter().filter(|t| Some(t.session_id) == session).collect()
    }

    /// The line of a failed transfer's row: "no free space" when the other computer's volume was too full
    /// (F-FILE-03), "did not finish" for every other reason.
    pub fn transfer_failure_text(&self, reason: crate::api::ApiCode) -> String {
        self.name_of(if reason == crate::api::ApiCode::NoSpace {
            MsgKey::TransferNoSpace
        } else {
            MsgKey::TransferFailed
        })
    }

    // ----- the queue -----

    /// The slots of the session in use: starts not answered yet, and transfers neither done nor failed.
    fn copy_slots_used(&self) -> usize {
        let session = self.session_id();
        let asked: usize = self.explorer.copy.asked.iter().map(|(_, items)| items.len()).sum();
        asked + self.transfers.iter().filter(|t| Some(t.session_id) == session && unfinished(&t.state)).count()
    }

    /// Whether the copy under way has nothing left to start or to finish.
    pub(crate) fn copy_idle(&self) -> bool {
        let q = &self.explorer.copy;
        let session = self.session_id();
        q.waiting.is_empty()
            && q.asked.is_empty()
            && !self
                .transfers
                .iter()
                .any(|t| Some(t.session_id) == session && t.batch == q.batch && unfinished(&t.state))
    }

    fn copy_begin_if_idle(&mut self) {
        if self.copy_idle() {
            let q = &mut self.explorer.copy;
            q.batch = q.batch.wrapping_add(1);
            q.refused = 0;
            q.rate = 0;
            q.prev_done = None;
        }
    }

    /// Queues `items` behind what waits already; the commands to send now are the answer (see [`Self::copy_pump`]).
    pub(crate) fn copy_enqueue(&mut self, items: Vec<CopyItem>) -> Vec<Command> {
        if items.is_empty() {
            return Vec::new();
        }
        self.copy_begin_if_idle();
        self.explorer.copy.waiting.extend(items);
        self.copy_pump()
    }

    /// Queues `items` ahead of what waits: a file the system asked for (a drag out of the window) reads from a
    /// stream that waits for it, so it does not stand behind a long copy.
    pub(crate) fn copy_enqueue_first(&mut self, items: Vec<CopyItem>) -> Vec<Command> {
        if items.is_empty() {
            return Vec::new();
        }
        self.copy_begin_if_idle();
        for item in items.into_iter().rev() {
            self.explorer.copy.waiting.push_front(item);
        }
        self.copy_pump()
    }

    /// The starts that fit the free slots, in the queue's order: files going to one folder of the other computer
    /// share a `FilesSend`, each file coming here is a `FilesGet`. Empty unless the session is active.
    pub(crate) fn copy_pump(&mut self) -> Vec<Command> {
        let Some(session) = self.viewer_session() else { return Vec::new() };
        if !matches!(session.state, SessionState::Active { .. }) {
            return Vec::new();
        }
        let session_id = session.id;
        let mut free = MAX_ACTIVE.saturating_sub(self.copy_slots_used());
        let mut out = Vec::new();
        let q = &mut self.explorer.copy;
        while free > 0 {
            let Some(first) = q.waiting.pop_front() else { break };
            let command = match &first {
                CopyItem::Get { remote, local_dir, .. } => {
                    let c = Command::FilesGet { session_id, remote_path: remote.clone(), local_dir: local_dir.clone() };
                    q.asked.push((c.clone(), vec![first]));
                    free -= 1;
                    c
                }
                CopyItem::Send { remote_dir, .. } => {
                    let dir = remote_dir.clone();
                    let mut items = vec![first];
                    while items.len() < free
                        && matches!(q.waiting.front(), Some(CopyItem::Send { remote_dir: d, .. }) if *d == dir)
                    {
                        items.extend(q.waiting.pop_front());
                    }
                    let local_paths = items
                        .iter()
                        .filter_map(|i| match i {
                            CopyItem::Send { local, .. } => Some(local.clone()),
                            CopyItem::Get { .. } => None,
                        })
                        .collect();
                    let c = Command::FilesSend { session_id, local_paths, remote_dir: dir };
                    free -= items.len();
                    q.asked.push((c.clone(), items));
                    c
                }
            };
            out.push(command);
        }
        out
    }

    /// The service named the transfers of a start (`ids`, in the order of its files): each gets its row at once,
    /// with the size the copy knows, so the list and the summary do not wait for the first note of each.
    pub(crate) fn copy_started(&mut self, command: Option<Command>, ids: &[u32]) -> Changed {
        let Some(command) = command else { return Changed::NONE };
        let (session_id, outgoing) = match &command {
            Command::FilesSend { session_id, .. } => (*session_id, true),
            Command::FilesGet { session_id, .. } => (*session_id, false),
            _ => return Changed::NONE,
        };
        // A start "Cancelar tudo" gave up on: its transfers end as they are named.
        if let Some(i) = self.explorer.copy.cancelled.iter().position(|c| *c == command) {
            self.explorer.copy.cancelled.remove(i);
            self.followups.extend(ids.iter().map(|id| Command::TransferCancel { transfer_id: *id }));
            self.copy_drop_rows(ids);
            return Changed::VIEWER;
        }
        let items = match self.explorer.copy.asked.iter().position(|(c, _)| *c == command) {
            Some(i) => self.explorer.copy.asked.remove(i).1,
            // A start that did not come from the queue: its files as the command names them, sizes unknown.
            None => match &command {
                Command::FilesSend { local_paths, remote_dir, .. } => local_paths
                    .iter()
                    .map(|p| CopyItem::Send { local: p.clone(), remote_dir: remote_dir.clone(), size: None })
                    .collect(),
                Command::FilesGet { remote_path, local_dir, .. } => {
                    vec![CopyItem::Get { remote: remote_path.clone(), local_dir: local_dir.clone(), size: None }]
                }
                _ => Vec::new(),
            },
        };
        let batch = self.explorer.copy.batch;
        for (id, item) in ids.iter().zip(items) {
            match self.transfers.iter_mut().find(|t| t.id == *id) {
                // Its first note came before the answer.
                Some(t) => {
                    t.size = t.size.or(item.size());
                    t.batch = batch;
                }
                None => self.transfers.push(TransferView {
                    id: *id,
                    session_id,
                    name: item.name(),
                    outgoing,
                    state: TransferState::Queued,
                    saved_as: None,
                    size: item.size(),
                    batch,
                }),
            }
        }
        let more = self.copy_pump();
        self.followups.extend(more);
        Changed::VIEWER
    }

    /// A start failed. `Busy`: its files wait again, first in line, and the next second tries (the answer is
    /// `Some`: nothing is told, nothing was lost). Any other code: the files count as refused and the caller says
    /// why (`None`).
    pub(crate) fn copy_refused(&mut self, command: &Command, code: ApiCode) -> Option<Changed> {
        let q = &mut self.explorer.copy;
        if let Some(i) = q.cancelled.iter().position(|c| c == command) {
            q.cancelled.remove(i);
            return Some(Changed::NONE);
        }
        let i = q.asked.iter().position(|(c, _)| c == command)?;
        let (_, items) = q.asked.remove(i);
        if code == ApiCode::Busy {
            for item in items.into_iter().rev() {
                q.waiting.push_front(item);
            }
            q.retry = true;
            return Some(Changed::VIEWER);
        }
        q.refused += items.len();
        let more = self.copy_pump();
        self.followups.extend(more);
        None
    }

    /// One second passed: a start the service called busy is tried again, and the copy's rate is measured.
    pub(crate) fn copy_tick(&mut self) -> Changed {
        let mut changed = Changed::NONE;
        if self.explorer.copy.retry {
            self.explorer.copy.retry = false;
            let more = self.copy_pump();
            self.followups.extend(more);
        }
        if !self.copy_idle() {
            let done = self.copy_bytes_done();
            let q = &mut self.explorer.copy;
            if let Some(prev) = q.prev_done {
                let now = done.saturating_sub(prev);
                // A light smoothing: the time left does not jump with each second.
                q.rate = if q.rate == 0 { now } else { (q.rate * 3 + now) / 4 };
            }
            q.prev_done = Some(done);
            changed = Changed::VIEWER;
        }
        changed
    }

    /// The session ended: nothing of its copies is left.
    pub(crate) fn copy_reset(&mut self) {
        self.explorer.copy = CopyQueue::default();
    }

    /// Transfers this side cancelled (`ids`): their rows leave the list, and nothing told about them later brings
    /// one back.
    pub(crate) fn copy_drop_rows(&mut self, ids: &[u32]) {
        self.transfers.retain(|t| !ids.contains(&t.id));
        self.explorer.copy.gone.extend(ids);
    }

    /// The files that were to land in `dir` are not wanted any more (the drag out that asked for them ended): what
    /// waits is dropped, and what was sent and not yet named is cancelled as soon as it is.
    pub(crate) fn copy_forget_dir(&mut self, dir: &Path) {
        let into = |i: &CopyItem| matches!(i, CopyItem::Get { local_dir, .. } if local_dir == dir);
        let q = &mut self.explorer.copy;
        q.waiting.retain(|i| !into(i));
        let (dropped, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut q.asked).into_iter().partition(|(_, items)| items.iter().any(into));
        q.asked = kept;
        q.cancelled.extend(dropped.into_iter().map(|(c, _)| c));
    }

    /// "Cancelar tudo": what waits is dropped, what runs is cancelled, what was sent and not yet named is cancelled
    /// as soon as it is. The rows of the unfinished transfers leave the list; done and failed ones stay.
    pub(crate) fn copy_cancel_all(&mut self) -> Vec<Command> {
        let session = self.session_id();
        let q = &mut self.explorer.copy;
        q.waiting.clear();
        q.retry = false;
        let sent: Vec<Command> = q.asked.drain(..).map(|(c, _)| c).collect();
        q.cancelled.extend(sent);
        let ids: Vec<u32> = self
            .transfers
            .iter()
            .filter(|t| Some(t.session_id) == session && unfinished(&t.state))
            .map(|t| t.id)
            .collect();
        self.copy_drop_rows(&ids);
        ids.into_iter().map(|transfer_id| Command::TransferCancel { transfer_id }).collect()
    }

    /// Whether "Cancelar tudo" has something to cancel.
    pub fn copy_can_cancel(&self) -> bool {
        !self.copy_idle()
    }

    // ----- what the bar says -----

    fn copy_rows(&self) -> impl Iterator<Item = &TransferView> {
        let session = self.session_id();
        let batch = self.explorer.copy.batch;
        self.transfers.iter().filter(move |t| Some(t.session_id) == session && t.batch == batch)
    }

    fn copy_bytes_done(&self) -> u64 {
        self.copy_rows()
            .map(|t| match t.state {
                TransferState::Running { done, .. } | TransferState::Paused { done, .. } => done,
                TransferState::Done => t.size.unwrap_or(0),
                TransferState::Queued | TransferState::Failed { .. } => 0,
            })
            .sum()
    }

    /// The copy under way, for the transfers bar; `None` when the session copied nothing yet.
    pub fn copy_summary(&self) -> Option<CopySummary> {
        let q = &self.explorer.copy;
        let pending = q.waiting.iter().chain(q.asked.iter().flat_map(|(_, items)| items));
        let (mut items_total, mut items_done, mut failed) = (0usize, 0usize, q.refused);
        let (mut bytes_total, mut sized) = (0u64, true);
        let (mut now, mut others, mut rows) = (None, 0usize, 0usize);
        for t in self.copy_rows() {
            rows += 1;
            match &t.state {
                TransferState::Failed { .. } => {
                    failed += 1;
                    continue;
                }
                TransferState::Done => items_done += 1,
                TransferState::Running { .. } => {
                    if now.is_none() {
                        now = Some(t.name.clone());
                    } else {
                        others += 1;
                    }
                }
                TransferState::Queued | TransferState::Paused { .. } => {}
            }
            items_total += 1;
            match t.size {
                Some(size) => bytes_total += size,
                None => sized = false,
            }
        }
        let mut waiting = 0;
        for item in pending {
            waiting += 1;
            items_total += 1;
            match item.size() {
                Some(size) => bytes_total += size,
                None => sized = false,
            }
        }
        if rows == 0 && waiting == 0 && failed == 0 {
            return None;
        }
        let bytes_done = self.copy_bytes_done();
        let finished = self.copy_idle();
        let fraction = if finished {
            1.0
        } else if sized && bytes_total > 0 {
            bytes_done as f32 / bytes_total as f32
        } else {
            items_done as f32 / items_total.max(1) as f32
        };
        let left = bytes_total.saturating_sub(bytes_done);
        let seconds_left = (sized && !finished && q.rate > 0).then(|| left.div_ceil(q.rate));
        Some(CopySummary {
            now,
            others,
            items_done,
            items_total,
            failed,
            bytes_done,
            bytes_total: sized.then_some(bytes_total),
            fraction: fraction.clamp(0.0, 1.0),
            bytes_per_s: q.rate,
            seconds_left,
            waiting,
            reading: self.copy_reading(),
            finished,
        })
    }

    /// The transfers bar: the headline and the numbers of the copy under way. `None` when the session copied
    /// nothing yet.
    pub fn transfers_bar(&self) -> Option<TransfersBar> {
        use crate::format::count;
        let c = self.copy_summary()?;
        let size = |b: u64| super::explorer::human_size(self.lang, b);
        let n = |v: usize| count(v as u64);
        let active = self.viewer_session().is_some_and(|s| matches!(s.state, SessionState::Active { .. }));
        let paused = self.copy_rows().any(|t| matches!(t.state, TransferState::Paused { .. }));
        let headline = if c.finished {
            self.t(if c.failed > 0 { MsgKey::TransfersFinishedFailed } else { MsgKey::TransfersFinished })
        } else if !active {
            self.t(MsgKey::TransfersWaitingLink)
        } else if let Some(name) = &c.now {
            if c.others > 0 {
                self.tm(&Msg::new(MsgKey::TransfersCopyingMore).with(name.clone()).with(n(c.others)))
            } else {
                self.tm(&Msg::new(MsgKey::TransfersCopying).with(name.clone()))
            }
        } else if c.reading {
            self.t(MsgKey::TransfersReading)
        } else if paused {
            self.t(MsgKey::TransfersPaused)
        } else {
            self.t(MsgKey::TransfersPreparing)
        };
        let mut parts = vec![format!("{}%", (c.fraction * 100.0).floor() as u32)];
        parts.push(self.tm(&Msg::new(MsgKey::TransfersItems).with(n(c.items_done)).with(n(c.items_total))));
        if let Some(total) = c.bytes_total.filter(|t| *t > 0) {
            parts.push(self.tm(&Msg::new(MsgKey::TransfersBytes).with(size(c.bytes_done)).with(size(total))));
        }
        if !c.finished && c.bytes_per_s > 0 {
            parts.push(self.tm(&Msg::new(MsgKey::TransfersSpeed).with(size(c.bytes_per_s))));
        }
        if let Some(left) = c.seconds_left {
            parts.push(self.tm(&Msg::new(MsgKey::TransfersLeft).with(crate::format::time_left(left))));
        }
        if c.waiting > 0 {
            parts.push(self.tm(&Msg::new(MsgKey::TransfersQueued).with(n(c.waiting))));
        }
        if c.failed > 0 {
            parts.push(self.tm(&Msg::new(MsgKey::TransfersFailedCount).with(n(c.failed))));
        }
        Some(TransfersBar {
            headline,
            detail: parts.join(" · "),
            fraction: c.fraction,
            can_cancel: !c.finished,
            failed: c.finished && c.failed > 0,
        })
    }

    /// The second line of a transfer's row: how far it is, or how it ended.
    pub fn transfer_line(&self, t: &TransferView) -> String {
        let size = |b: u64| super::explorer::human_size(self.lang, b);
        match &t.state {
            TransferState::Running { done, total, bytes_per_s } => self
                .tm(&Msg::new(MsgKey::TransferProgress).with(size(*done)).with(size(*total)).with(size(*bytes_per_s))),
            TransferState::Paused { .. } => self.t(MsgKey::TransferPaused),
            TransferState::Done => match (&t.saved_as, t.outgoing) {
                // Files never overwrite: the name the file got is said.
                (Some(name), _) => self.tm(&Msg::new(MsgKey::TransferSavedAs).with(name.clone())),
                (None, true) => self.tm(&Msg::new(MsgKey::TransferSent).with(self.peer_name())),
                (None, false) => self.t(MsgKey::TransferReceived),
            },
            TransferState::Failed { reason } => self.transfer_failure_text(*reason),
            TransferState::Queued => self.t(MsgKey::TransferWaiting),
        }
    }

    /// The rows the transfers panel lists: what runs first, then what is paused, what failed, what the service
    /// has not started yet and what is done, the newest first in each group; at most [`TRANSFER_ROWS`].
    pub fn transfer_rows(&self) -> Vec<&TransferView> {
        let rank = |t: &TransferView| match t.state {
            TransferState::Running { .. } => 0,
            TransferState::Paused { .. } => 1,
            TransferState::Failed { .. } => 2,
            TransferState::Queued => 3,
            TransferState::Done => 4,
        };
        let mut rows = self.viewer_transfers();
        rows.reverse();
        rows.sort_by_key(|t| rank(t));
        rows.truncate(TRANSFER_ROWS);
        rows
    }

    /// The folders of a copy are still being read (an upload's walk, a download's scan).
    fn copy_reading(&self) -> bool {
        let t = &self.explorer.there;
        t.walk.is_some() || t.download.is_some()
    }
}

#[cfg(test)]
mod tests;
