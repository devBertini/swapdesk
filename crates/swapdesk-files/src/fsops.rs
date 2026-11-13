//! The explorer's disk work on real paths (FILES-UI.md 7.4 and 8.1): page listings with flags, new folder, rename and
//! move. Every name goes through `validate_name`; nothing here replaces an existing entry. Which components of a path
//! may be links, and which folders are protected, is the server's rule ([`crate::proxy`]), checked before these run.

use std::fs::Metadata;
use std::io;
use std::path::{Component, Path};
use std::time::UNIX_EPOCH;

use swapdesk_ipc::{FsEntry2View, FsPage, Text};
use swapdesk_proto::{EntryKind, ErrorCode};

use crate::disk::Moved;
use crate::name::validate_name;

/// Deepest folder tree a cross-volume move copies (a guard against a runaway walk; links are never followed).
const MAX_COPY_DEPTH: u32 = 64;

fn mtime_of(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The flags of an entry (`DirEntry2.flags`): hidden, system, read-only.
fn flags_of(name: &str, meta: &Metadata) -> u8 {
    let mut flags = 0;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let a = meta.file_attributes();
        if a & 0x2 != 0 {
            flags |= swapdesk_proto::ENTRY_HIDDEN;
        }
        if a & 0x4 != 0 {
            flags |= swapdesk_proto::ENTRY_SYSTEM;
        }
        if a & 0x1 != 0 {
            flags |= swapdesk_proto::ENTRY_READ_ONLY;
        }
        let _ = name;
    }
    #[cfg(not(windows))]
    {
        if name.starts_with('.') {
            flags |= swapdesk_proto::ENTRY_HIDDEN;
        }
        if meta.permissions().readonly() {
            flags |= swapdesk_proto::ENTRY_READ_ONLY;
        }
    }
    flags
}

pub(crate) fn map_err(e: &io::Error) -> ErrorCode {
    match e.kind() {
        io::ErrorKind::NotFound => ErrorCode::NotFound,
        io::ErrorKind::AlreadyExists => ErrorCode::Exists,
        io::ErrorKind::PermissionDenied => ErrorCode::NotPermitted,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => ErrorCode::NoSpace,
        _ => ErrorCode::IoFailed,
    }
}

/// A page of `path` as [`crate::Fs::list_page`] says. The folder is read again for every page, so a page is only as
/// stable as the folder (an entry added between two pages can shift the next one).
pub(crate) fn list_page(path: &Path, start: u32, max: u16) -> Result<FsPage, ErrorCode> {
    let read = std::fs::read_dir(path).map_err(|e| map_err(&e))?;
    // The bool only orders the page: a link to a folder sorts with the folders, and is still a `Link`.
    let mut all: Vec<(bool, FsEntry2View)> = Vec::new();
    for item in read.flatten() {
        let Some(name) = item.file_name().to_str().map(str::to_string) else { continue };
        if name.len() > 255 {
            continue;
        }
        let Ok(meta) = item.metadata() else { continue };
        let (kind, dir_like, size) = if meta.file_type().is_symlink() {
            (EntryKind::Link, std::fs::metadata(item.path()).is_ok_and(|m| m.is_dir()), 0)
        } else if meta.is_dir() {
            (EntryKind::Dir, true, 0)
        } else {
            (EntryKind::File, false, meta.len())
        };
        let flags = flags_of(&name, &meta);
        all.push((dir_like, FsEntry2View { name: Text(name), kind, size, mtime_ms: mtime_of(&meta), flags }));
    }
    all.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.0.to_lowercase().cmp(&b.1.name.0.to_lowercase())));
    let start = start as usize;
    let total = all.len();
    let entries: Vec<FsEntry2View> = all.into_iter().skip(start).take(usize::from(max)).map(|(_, e)| e).collect();
    let end = start.saturating_add(entries.len());
    let next = (end < total).then(|| u32::try_from(end).unwrap_or(u32::MAX));
    Ok(FsPage { entries, next, free: None })
}

/// Something (even a dangling link) is at `p`.
fn there(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

pub(crate) fn make_dir(dir: &Path, name: &str) -> Result<(), ErrorCode> {
    validate_name(name)?;
    // `create_dir` fails on anything that is there, a dangling link included.
    std::fs::create_dir(dir.join(name)).map_err(|e| map_err(&e))
}

/// An entry of the folder other than `except` has the name `wanted` in some letter case.
fn case_twin(dir: &Path, wanted: &str, except: &str) -> bool {
    let lower = wanted.to_lowercase();
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| e.file_name().to_str().is_some_and(|n| n != except && n.to_lowercase() == lower))
    })
}

pub(crate) fn rename(dir: &Path, name: &str, new_name: &str) -> Result<(), ErrorCode> {
    validate_name(name)?;
    validate_name(new_name)?;
    let from = dir.join(name);
    let to = dir.join(new_name);
    if !there(&from) {
        return Err(ErrorCode::NotFound);
    }
    if name == new_name {
        return Err(ErrorCode::Exists);
    }
    // Windows `rename` replaces a file and Unix `rename` replaces anything: look first. A change of letter case alone
    // finds "the target" in the entry itself, which is allowed unless a different entry holds that name.
    if there(&to) {
        let only_case = name.to_lowercase() == new_name.to_lowercase();
        if !only_case || case_twin(dir, new_name, name) {
            return Err(ErrorCode::Exists);
        }
    }
    std::fs::rename(from, to).map_err(|e| map_err(&e))
}

/// `a` is `b` or inside it, comparing names without letter case (the usual volumes are case-insensitive; refusing a
/// case-different spelling too is the safe side).
pub(crate) fn within(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> Vec<String> {
        p.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect()
    };
    let (a, b) = (norm(a), norm(b));
    a.len() >= b.len() && a[..b.len()] == b[..]
}

pub(crate) fn move_into(from_dir: &Path, name: &str, to_dir: &Path) -> Result<Moved, ErrorCode> {
    validate_name(name)?;
    let src = from_dir.join(name);
    let dest = to_dir.join(name);
    let Ok(src_meta) = std::fs::symlink_metadata(&src) else { return Err(ErrorCode::NotFound) };
    // A folder never goes into itself, nor into anything inside it.
    if within(to_dir, &src) {
        return Err(ErrorCode::InvalidName);
    }
    match std::fs::metadata(to_dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(ErrorCode::InvalidName),
        Err(e) => return Err(map_err(&e)),
    }
    if there(&dest) {
        return Err(ErrorCode::Exists);
    }
    match std::fs::rename(&src, &dest) {
        Ok(()) => Ok(Moved::Renamed),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            if let Err(code) = copy_tree(&src, &src_meta, &dest, 0) {
                // What this call made is a fresh tree nobody else knows: take it away again; the source is untouched.
                let _ = remove_tree(&dest);
                return Err(code);
            }
            Ok(Moved::Copied)
        }
        Err(e) => Err(map_err(&e)),
    }
}

pub(crate) fn remove_tree(p: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

/// Copies `src` to the absent `dest`, entry by entry without following a link, and checks every file's size.
fn copy_tree(src: &Path, meta: &Metadata, dest: &Path, depth: u32) -> Result<(), ErrorCode> {
    if depth > MAX_COPY_DEPTH {
        return Err(ErrorCode::TooLarge);
    }
    if meta.file_type().is_symlink() {
        return copy_link(src, dest);
    }
    if meta.is_dir() {
        std::fs::create_dir(dest).map_err(|e| map_err(&e))?;
        for item in std::fs::read_dir(src).map_err(|e| map_err(&e))? {
            let item = item.map_err(|e| map_err(&e))?;
            let m = item.metadata().map_err(|e| map_err(&e))?;
            copy_tree(&item.path(), &m, &dest.join(item.file_name()), depth + 1)?;
        }
        return Ok(());
    }
    if !meta.is_file() {
        return Err(ErrorCode::IoFailed);
    }
    let copied = std::fs::copy(src, dest).map_err(|e| map_err(&e))?;
    let now = std::fs::metadata(dest).map_err(|e| map_err(&e))?;
    if copied != meta.len() || now.len() != meta.len() {
        return Err(ErrorCode::IoFailed);
    }
    Ok(())
}

#[cfg(unix)]
fn copy_link(src: &Path, dest: &Path) -> Result<(), ErrorCode> {
    let target = std::fs::read_link(src).map_err(|e| map_err(&e))?;
    std::os::unix::fs::symlink(target, dest).map_err(|e| map_err(&e))
}

#[cfg(not(unix))]
fn copy_link(_src: &Path, _dest: &Path) -> Result<(), ErrorCode> {
    // Making a link on Windows needs a privilege the user helper does not hold.
    Err(ErrorCode::IoFailed)
}
