//! The disk, as the file engine sees it: one trait, every call a single request and a single reply.
//!
//! Why a trait: the service runs as SYSTEM and must never open a path a peer sent. A later stage runs the disk access
//! in the user's own process and reaches it over a pipe, so every call here takes plain arguments and returns plain
//! owned values (no borrowed handle outlives a call; an open file is an opaque `u64` id). One call maps to one
//! message pair. No call moves more than [`MAX_IO`] bytes.
//!
//! The process that owns the disk (here [`crate::LocalFs`]) re-checks everything itself, even when the caller is
//! compromised: a file name given to a `part_*`, `meta_*`, `stat_file` or `remove_file` call is checked against the
//! name rules ([`crate::name::validate_name`]: a single component, no `.` or `..`, no separator, no stream, no
//! reserved device name, never a `.swapdesk-part` or `.swapdesk-meta` name), so the file always lies directly in the
//! folder given; files are opened without following a link or junction and a handle is checked again after opening;
//! a rename never replaces a file. A refusal is `ErrorCode::InvalidName`, `IoFailed` and so on, never a panic.

use std::path::Path;

use swapdesk_proto::ErrorCode;

/// Most bytes one call reads or writes (the pipe's message limit).
pub const MAX_IO: usize = 1 << 20;

/// Which of the two files a receiver keeps next to the final name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `name.swapdesk-part`
    Part,
    /// `name.swapdesk-meta`
    Meta,
}

/// What a file-system entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStat {
    pub is_dir: bool,
    pub is_file: bool,
    /// A symbolic link, junction or other reparse point (the entry itself, never its target).
    pub is_reparse: bool,
    pub len: u64,
    pub mtime_ms: u64,
}

/// One folder entry. Names that are not valid UTF-8 or are over 255 bytes are never returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    /// 0 for a folder.
    pub size: u64,
    pub mtime_ms: u64,
}

/// A folder read: folders first, then files, each group by name (case-insensitive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirRead {
    pub entries: Vec<FsEntry>,
    /// More entries existed than `max_entries`.
    pub truncated: bool,
}

/// A file opened for reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadHandle {
    pub id: u64,
    pub len: u64,
    pub mtime_ms: u64,
}

/// Every disk operation of the file engine. `dir` is the receiving folder, `name` the offered file name; the part and
/// meta files are `dir/<name>.swapdesk-part` and `dir/<name>.swapdesk-meta`.
pub trait Fs: Send + Sync {
    /// Lists `path`, sorted as [`DirRead`] says, at most `max_entries` entries. `NotFound` when it is missing.
    fn list_dir(&self, path: &Path, max_entries: u32) -> Result<DirRead, ErrorCode>;

    /// Free bytes on the volume that holds the folder `path`, `None` when this disk cannot tell (the receiver then
    /// skips the free-space check; a full disk later is `NoSpace` from the write).
    fn free_space(&self, _path: &Path) -> Result<Option<u64>, ErrorCode> {
        Ok(None)
    }

    /// The entry at `path`: `None` when nothing is there. `follow` false looks at a link itself (`symlink_metadata`).
    fn stat(&self, path: &Path, follow: bool) -> Result<Option<FsStat>, ErrorCode>;

    /// Opens a regular file for reading (the sending side: the user's own file, links are read through; `IoFailed`
    /// for anything that is not a regular file, `NotFound` when missing) and gives its length and mtime.
    fn open_read(&self, path: &Path) -> Result<ReadHandle, ErrorCode>;
    /// Reads up to `len` (at most [`MAX_IO`]) bytes at `offset`; fewer only at the end of the file.
    fn read_at(&self, id: u64, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode>;
    /// The current length and mtime of an open file.
    fn read_stat(&self, id: u64) -> Result<FsStat, ErrorCode>;
    /// Closes an open file (an unknown id is ignored).
    fn close_read(&self, id: u64);

    /// The part or meta file of `name` in `dir`, looked at without following a link.
    fn stat_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<Option<FsStat>, ErrorCode>;
    /// Creates an empty part file; fails when anything is there.
    fn part_create(&self, dir: &Path, name: &str) -> Result<(), ErrorCode>;
    /// Cuts the part file to `len` bytes (it must be a regular file, not a link).
    fn part_truncate(&self, dir: &Path, name: &str, len: u64) -> Result<(), ErrorCode>;
    /// Appends `data` (at most [`MAX_IO`] bytes) to the part file, which must be exactly `offset` bytes long.
    fn part_append(&self, dir: &Path, name: &str, offset: u64, data: &[u8]) -> Result<(), ErrorCode>;
    fn part_len(&self, dir: &Path, name: &str) -> Result<u64, ErrorCode>;
    /// Reads up to `len` (at most [`MAX_IO`]) bytes of the part file at `offset`.
    fn part_read(&self, dir: &Path, name: &str, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode>;
    /// Flushes the part file to disk (`sync_data`); with `final_mtime_ms` it also sets the modification time (when the
    /// OS lets it) and flushes metadata too (`sync_all`).
    fn part_sync(&self, dir: &Path, name: &str, final_mtime_ms: Option<u64>) -> Result<(), ErrorCode>;
    /// Moves the part file to `dir/target_name` without ever replacing a file (`IoFailed` if `target_name` exists).
    /// `target_name` follows the name rules too.
    fn part_rename(&self, dir: &Path, name: &str, target_name: &str) -> Result<(), ErrorCode>;
    /// Removes the part or meta file (a link there is removed as a link, never followed).
    fn remove_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<(), ErrorCode>;

    /// The meta file's bytes (at most 1 KiB are read); `None` when it is missing, a link or not a regular file.
    fn meta_read(&self, dir: &Path, name: &str) -> Result<Option<Vec<u8>>, ErrorCode>;
    /// Writes the meta file (created when missing, otherwise cut to `data`'s length) and syncs it.
    fn meta_write(&self, dir: &Path, name: &str, data: &[u8]) -> Result<(), ErrorCode>;
}
