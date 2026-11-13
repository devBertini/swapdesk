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
use crate::fsutil::long_form;
use crate::name::validate_name;
use crate::plat;

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
    // A change of letter case alone finds "the target" in the entry itself, which is allowed unless a different entry
    // holds that name (the one look that cannot be one step with the rename: the twin would have to appear in the
    // instant between the look and the rename).
    if name.to_lowercase() == new_name.to_lowercase() {
        if case_twin(dir, new_name, name) {
            return Err(ErrorCode::Exists);
        }
        return std::fs::rename(from, to).map_err(|e| map_err(&e));
    }
    // `std::fs::rename` replaces what is at the target (a file on Windows, anything on Unix). The system's no-replace
    // rename makes the check and the move one step, so a name someone made a moment ago is never replaced.
    plat::rename_noreplace(&from, &to).map_err(|e| map_err(&e))
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
    // (also by long names: `LONGFO~1/sub` is inside `Long Folder`; the source's own name is not resolved, so a link is
    // looked at as itself and not as its target).
    if within(to_dir, &src) || within(&long_form(to_dir), &long_form(from_dir).join(name)) {
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
    match plat::rename_noreplace(&src, &dest) {
        Ok(()) => Ok(Moved::Renamed),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_across(&src, &src_meta, &dest).map(|()| Moved::Copied)
        }
        Err(e) => Err(map_err(&e)),
    }
}

/// Copies `src` to `dest` for a move across volumes. `dest` is never replaced: when something is there already (it
/// appeared since the caller looked) the copy fails with `Exists` and that entry is left alone. A copy that fails
/// otherwise takes away the tree it made; the source is untouched either way.
fn copy_across(src: &Path, src_meta: &Metadata, dest: &Path) -> Result<(), ErrorCode> {
    match copy_tree(src, src_meta, dest, 0) {
        Ok(()) => Ok(()),
        Err(Failed { code, dest_was_there: true }) => Err(code),
        Err(Failed { code, dest_was_there: false }) => {
            let _ = remove_tree(dest);
            Err(code)
        }
    }
}

pub(crate) fn remove_tree(p: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

/// Why a [`copy_tree`] stopped.
struct Failed {
    code: ErrorCode,
    /// The entry at `dest` was there before the copy made anything (`AlreadyExists` at the very first step), so it is
    /// not the copy's to take away.
    dest_was_there: bool,
}

impl Failed {
    /// A failure of the first step at the top.
    fn at_top(e: &io::Error) -> Self {
        Self { code: map_err(e), dest_was_there: e.kind() == io::ErrorKind::AlreadyExists }
    }

    /// A failure anywhere else: what is at `dest` is the copy's own tree.
    fn inside(code: ErrorCode) -> Self {
        Self { code, dest_was_there: false }
    }
}

/// Copies `src` to `dest`, which must be absent, entry by entry without following a link, and checks every file's
/// size. Nothing is ever replaced: a folder is made with `create_dir` and a file with a copy that fails when its
/// target exists.
fn copy_tree(src: &Path, meta: &Metadata, dest: &Path, depth: u32) -> Result<(), Failed> {
    if depth > MAX_COPY_DEPTH {
        return Err(Failed::inside(ErrorCode::TooLarge));
    }
    let top = depth == 0;
    let step = |e: io::Error| if top { Failed::at_top(&e) } else { Failed::inside(map_err(&e)) };
    let inside = |e: io::Error| Failed::inside(map_err(&e));
    if meta.file_type().is_symlink() {
        return copy_link(src, dest).map_err(|e| if top { e } else { Failed::inside(e.code) });
    }
    if meta.is_dir() {
        std::fs::create_dir(dest).map_err(step)?;
        for item in std::fs::read_dir(src).map_err(inside)? {
            let item = item.map_err(inside)?;
            let m = item.metadata().map_err(inside)?;
            copy_tree(&item.path(), &m, &dest.join(item.file_name()), depth + 1).map_err(|e| Failed::inside(e.code))?;
        }
        return Ok(());
    }
    if !meta.is_file() {
        return Err(Failed::inside(ErrorCode::IoFailed));
    }
    plat::copy_file_noreplace(src, dest).map_err(step)?;
    let now = std::fs::metadata(dest).map_err(inside)?;
    if now.len() != meta.len() {
        return Err(Failed::inside(ErrorCode::IoFailed));
    }
    Ok(())
}

#[cfg(unix)]
fn copy_link(src: &Path, dest: &Path) -> Result<(), Failed> {
    let target = std::fs::read_link(src).map_err(|e| Failed::inside(map_err(&e)))?;
    std::os::unix::fs::symlink(target, dest).map_err(|e| Failed::at_top(&e))
}

#[cfg(not(unix))]
fn copy_link(_src: &Path, _dest: &Path) -> Result<(), Failed> {
    // Making a link on Windows needs a privilege the user helper does not hold.
    Err(Failed::inside(ErrorCode::IoFailed))
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("swapdesk-fsops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `n` threads each rename their own file to the one name `target`, released together: whatever the order, exactly
    /// one wins and the others are told `Exists`, and the winner's bytes are what is there at the end. A check followed
    /// by a rename that replaces would let a second one through.
    fn race(
        d: &Path,
        round: usize,
        n: usize,
        rename_to_target: impl Fn(&str) -> Result<(), ErrorCode> + Sync,
    ) -> usize {
        let barrier = Barrier::new(n);
        let target = format!("target-{round}");
        let wins: Vec<(usize, bool)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let (barrier, rename_to_target) = (&barrier, &rename_to_target);
                    s.spawn(move || {
                        let mine = format!("src-{round}-{i}");
                        std::fs::write(d.join(&mine), format!("from {i}")).unwrap();
                        barrier.wait();
                        (i, rename_to_target(&mine).is_ok())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<usize> = wins.iter().filter(|(_, ok)| *ok).map(|(i, _)| *i).collect();
        if winners.len() == 1 {
            assert_eq!(std::fs::read_to_string(d.join(&target)).unwrap(), format!("from {}", winners[0]));
        }
        winners.len()
    }

    // The no-replace guarantee is the system's (Windows, the Mac); the `std` fallback elsewhere has the gap plat.rs says.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn concurrent_renames_to_one_name_never_replace_each_other() {
        let d = dir("race-rename");
        let n = 8;
        for round in 0..150 {
            let target = format!("target-{round}");
            let winners = race(&d, round, n, |mine| rename(&d, mine, &target));
            assert_eq!(winners, 1, "round {round}: every one of the renames that returned Ok replaced the one before");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_copy_across_volumes_never_replaces_or_removes_what_is_at_the_destination() {
        // The destination appears after the move looked: the copy fails with `Exists`, and what is there (a file, or a
        // folder with files in it) is still there, untouched, since the copy made none of it.
        let d = dir("copy-across");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src").join("f.txt"), b"source").unwrap();
        std::fs::write(d.join("taken.txt"), b"theirs").unwrap();
        let meta = std::fs::symlink_metadata(d.join("src").join("f.txt")).unwrap();
        assert_eq!(copy_across(&d.join("src").join("f.txt"), &meta, &d.join("taken.txt")), Err(ErrorCode::Exists));
        assert_eq!(std::fs::read(d.join("taken.txt")).unwrap(), b"theirs");

        std::fs::create_dir_all(d.join("tree").join("deep")).unwrap();
        std::fs::write(d.join("tree").join("deep").join("a.txt"), b"a").unwrap();
        std::fs::create_dir_all(d.join("occupied")).unwrap();
        std::fs::write(d.join("occupied").join("theirs.txt"), b"theirs").unwrap();
        let tree = std::fs::symlink_metadata(d.join("tree")).unwrap();
        assert_eq!(copy_across(&d.join("tree"), &tree, &d.join("occupied")), Err(ErrorCode::Exists));
        assert_eq!(std::fs::read(d.join("occupied").join("theirs.txt")).unwrap(), b"theirs");
        assert!(d.join("tree").join("deep").join("a.txt").exists(), "the source is untouched");

        // A free destination gets the whole tree; a failure half way takes away what the copy made.
        copy_across(&d.join("tree"), &tree, &d.join("fresh")).unwrap();
        assert_eq!(std::fs::read(d.join("fresh").join("deep").join("a.txt")).unwrap(), b"a");
        let _ = std::fs::remove_dir_all(&d);
    }

    // The no-replace guarantee is the system's (Windows, the Mac); the `std` fallback elsewhere has the gap plat.rs says.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn concurrent_moves_into_one_folder_never_replace_each_other() {
        let d = dir("race-move");
        let (from, to) = (d.join("from"), d.join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        for round in 0..60 {
            // The same name arrives from `n` folders at once.
            let n = 8;
            let barrier = Barrier::new(n);
            let ok: Vec<(usize, bool)> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..n)
                    .map(|i| {
                        let (barrier, d, to) = (&barrier, &d, &to);
                        s.spawn(move || {
                            let src = d.join(format!("src-{round}-{i}"));
                            std::fs::create_dir_all(&src).unwrap();
                            std::fs::write(src.join(format!("f-{round}")), format!("from {i}")).unwrap();
                            barrier.wait();
                            (i, move_into(&src, &format!("f-{round}"), to).is_ok())
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let winners: Vec<_> = ok.iter().filter(|(_, ok)| *ok).collect();
            assert_eq!(winners.len(), 1, "round {round}: a move replaced the file another move had just put there");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
