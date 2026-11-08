//! [`LocalFs`]: the [`Fs`] that works on this machine's disk, as the process it runs in. It re-checks every name and
//! never follows a link when it writes (see the trait's documentation).

use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

use swapdesk_proto::ErrorCode;

use crate::disk::{DirRead, Fs, FsEntry, FsStat, Kind, MAX_IO, ReadHandle};
use crate::fsutil::{Create, is_reparse, open_rw_nofollow};
use crate::name::{META_SUFFIX, PART_SUFFIX, validate_name};

/// Files a `LocalFs` keeps open for reading at once.
const MAX_OPEN: usize = 256;
/// Most bytes `meta_read` returns.
const META_READ_MAX: usize = 1024;

#[derive(Default)]
pub struct LocalFs {
    next: AtomicU64,
    open: Mutex<HashMap<u64, Arc<Mutex<File>>>>,
}

impl LocalFs {
    pub fn new() -> Self {
        Self::default()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn io_failed(_: io::Error) -> ErrorCode {
    ErrorCode::IoFailed
}

fn mtime_of(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn stat_of(meta: &Metadata) -> FsStat {
    FsStat {
        is_dir: meta.is_dir(),
        is_file: meta.is_file(),
        is_reparse: is_reparse(meta),
        len: meta.len(),
        mtime_ms: mtime_of(meta),
    }
}

/// The path of the part or meta file of `name`, after the name rules: always a direct child of `dir`.
fn file_path(dir: &Path, name: &str, kind: Kind) -> Result<PathBuf, ErrorCode> {
    validate_name(name)?;
    let suffix = match kind {
        Kind::Part => PART_SUFFIX,
        Kind::Meta => META_SUFFIX,
    };
    Ok(dir.join(format!("{name}{suffix}")))
}

fn check_len(len: usize) -> Result<(), ErrorCode> {
    if len > MAX_IO { Err(ErrorCode::TooLarge) } else { Ok(()) }
}

/// Reads up to `len` bytes at `offset`; short only at the end of the file.
fn read_range(file: &mut File, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
    let len = len as usize;
    check_len(len)?;
    file.seek(SeekFrom::Start(offset)).map_err(io_failed)?;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let n = file.read(&mut buf[got..]).map_err(io_failed)?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    Ok(buf)
}

impl LocalFs {
    fn handle(&self, id: u64) -> Result<Arc<Mutex<File>>, ErrorCode> {
        lock(&self.open).get(&id).cloned().ok_or(ErrorCode::NotFound)
    }

    fn open_part(&self, dir: &Path, name: &str) -> Result<File, ErrorCode> {
        open_rw_nofollow(&file_path(dir, name, Kind::Part)?, Create::No).map_err(io_failed)
    }
}

impl Fs for LocalFs {
    fn list_dir(&self, path: &Path, max_entries: u32) -> Result<DirRead, ErrorCode> {
        let read = std::fs::read_dir(path)
            .map_err(|e| if e.kind() == io::ErrorKind::NotFound { ErrorCode::NotFound } else { ErrorCode::IoFailed })?;
        let mut all: Vec<FsEntry> = Vec::new();
        for item in read.flatten() {
            let Some(name) = item.file_name().to_str().map(str::to_string) else { continue };
            if name.len() > 255 {
                continue;
            }
            // `metadata` follows links so a linked folder shows as a folder; a broken link is listed as an empty file.
            let (is_dir, size, mtime_ms) = match std::fs::metadata(item.path()).or_else(|_| item.metadata()) {
                Ok(m) => (
                    m.is_dir(),
                    if m.is_dir() { 0 } else { m.len() },
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0)),
                ),
                Err(_) => (false, 0, 0),
            };
            all.push(FsEntry { name, is_dir, size, mtime_ms });
        }
        all.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        let max = max_entries as usize;
        let truncated = all.len() > max;
        all.truncate(max);
        Ok(DirRead { entries: all, truncated })
    }

    fn stat(&self, path: &Path, follow: bool) -> Result<Option<FsStat>, ErrorCode> {
        let r = if follow { std::fs::metadata(path) } else { std::fs::symlink_metadata(path) };
        match r {
            Ok(m) => Ok(Some(stat_of(&m))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(ErrorCode::IoFailed),
        }
    }

    fn open_read(&self, path: &Path) -> Result<ReadHandle, ErrorCode> {
        let file = crate::fsutil::open_read(path)
            .map_err(|e| if e.kind() == io::ErrorKind::NotFound { ErrorCode::NotFound } else { ErrorCode::IoFailed })?;
        let meta = file.metadata().map_err(io_failed)?;
        let mut open = lock(&self.open);
        if open.len() >= MAX_OPEN {
            return Err(ErrorCode::Busy);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        open.insert(id, Arc::new(Mutex::new(file)));
        Ok(ReadHandle { id, len: meta.len(), mtime_ms: mtime_of(&meta) })
    }

    fn read_at(&self, id: u64, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        let h = self.handle(id)?;
        let mut f = lock(&h);
        read_range(&mut f, offset, len)
    }

    fn read_stat(&self, id: u64) -> Result<FsStat, ErrorCode> {
        let h = self.handle(id)?;
        let f = lock(&h);
        Ok(stat_of(&f.metadata().map_err(io_failed)?))
    }

    fn close_read(&self, id: u64) {
        lock(&self.open).remove(&id);
    }

    fn stat_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<Option<FsStat>, ErrorCode> {
        self.stat(&file_path(dir, name, kind)?, false)
    }

    fn part_create(&self, dir: &Path, name: &str) -> Result<(), ErrorCode> {
        open_rw_nofollow(&file_path(dir, name, Kind::Part)?, Create::New).map(drop).map_err(io_failed)
    }

    fn part_truncate(&self, dir: &Path, name: &str, len: u64) -> Result<(), ErrorCode> {
        self.open_part(dir, name)?.set_len(len).map_err(io_failed)
    }

    fn part_append(&self, dir: &Path, name: &str, offset: u64, data: &[u8]) -> Result<(), ErrorCode> {
        check_len(data.len())?;
        let mut f = self.open_part(dir, name)?;
        if f.metadata().map_err(io_failed)?.len() != offset {
            return Err(ErrorCode::ProtocolViolation);
        }
        f.seek(SeekFrom::Start(offset)).map_err(io_failed)?;
        f.write_all(data).map_err(io_failed)
    }

    fn part_len(&self, dir: &Path, name: &str) -> Result<u64, ErrorCode> {
        Ok(self.open_part(dir, name)?.metadata().map_err(io_failed)?.len())
    }

    fn part_read(&self, dir: &Path, name: &str, offset: u64, len: u32) -> Result<Vec<u8>, ErrorCode> {
        let mut f = self.open_part(dir, name)?;
        read_range(&mut f, offset, len)
    }

    fn part_sync(&self, dir: &Path, name: &str, final_mtime_ms: Option<u64>) -> Result<(), ErrorCode> {
        let f = self.open_part(dir, name)?;
        match final_mtime_ms {
            None => f.sync_data().map_err(io_failed),
            Some(ms) => {
                f.sync_all().map_err(io_failed)?;
                // Keep the sender's modification time when the OS lets us.
                let _ = f.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
                Ok(())
            }
        }
    }

    fn part_rename(&self, dir: &Path, name: &str, target_name: &str) -> Result<(), ErrorCode> {
        let from = file_path(dir, name, Kind::Part)?;
        validate_name(target_name)?;
        let to = dir.join(target_name);
        // Never over a file. Windows refuses an existing target by itself (no replace flag); elsewhere `rename`
        // replaces, so the check comes first.
        if std::fs::symlink_metadata(&to).is_ok() {
            return Err(ErrorCode::IoFailed);
        }
        std::fs::rename(from, to).map_err(io_failed)
    }

    fn remove_file(&self, dir: &Path, name: &str, kind: Kind) -> Result<(), ErrorCode> {
        std::fs::remove_file(file_path(dir, name, kind)?).map_err(io_failed)
    }

    fn meta_read(&self, dir: &Path, name: &str) -> Result<Option<Vec<u8>>, ErrorCode> {
        let path = file_path(dir, name, Kind::Meta)?;
        let Ok(m) = std::fs::symlink_metadata(&path) else { return Ok(None) };
        if is_reparse(&m) || !m.is_file() {
            return Ok(None);
        }
        let Ok(f) = File::open(&path) else { return Ok(None) };
        let mut buf = Vec::new();
        f.take(META_READ_MAX as u64).read_to_end(&mut buf).map_err(io_failed)?;
        Ok(Some(buf))
    }

    fn meta_write(&self, dir: &Path, name: &str, data: &[u8]) -> Result<(), ErrorCode> {
        check_len(data.len())?;
        let mut f = open_rw_nofollow(&file_path(dir, name, Kind::Meta)?, Create::OrOpen).map_err(io_failed)?;
        f.seek(SeekFrom::Start(0)).map_err(io_failed)?;
        f.write_all(data).map_err(io_failed)?;
        f.set_len(data.len() as u64).map_err(io_failed)?;
        f.sync_data().map_err(io_failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("swapdesk-files-local-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_name_that_escapes_the_folder_is_refused_by_the_fs_itself() {
        let root = dir("escape");
        let inner = root.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let fs = LocalFs::new();
        for bad in ["../x", "..\\x", "a/b", "C:\\x", "..", "s:ads", "nul", "x.swapdesk-part"] {
            assert_eq!(fs.part_create(&inner, bad), Err(ErrorCode::InvalidName), "{bad}");
            assert_eq!(fs.part_append(&inner, bad, 0, b"x"), Err(ErrorCode::InvalidName), "{bad}");
            assert_eq!(fs.part_truncate(&inner, bad, 0), Err(ErrorCode::InvalidName), "{bad}");
            assert_eq!(fs.part_rename(&inner, bad, "ok"), Err(ErrorCode::InvalidName), "{bad}");
            assert_eq!(fs.meta_write(&inner, bad, b"x"), Err(ErrorCode::InvalidName), "{bad}");
            assert_eq!(fs.meta_read(&inner, bad), Err(ErrorCode::InvalidName), "{bad}");
            assert_eq!(fs.remove_file(&inner, bad, Kind::Part), Err(ErrorCode::InvalidName), "{bad}");
            assert_eq!(fs.stat_file(&inner, bad, Kind::Meta), Err(ErrorCode::InvalidName), "{bad}");
        }
        fs.part_create(&inner, "good").unwrap();
        assert_eq!(fs.part_rename(&inner, "good", "../escaped"), Err(ErrorCode::InvalidName));
        assert!(!root.join("escaped").exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "nothing was created outside the folder");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_never_replaces_and_chunks_are_capped() {
        let d = dir("rename");
        let fs = LocalFs::new();
        fs.part_create(&d, "a.bin").unwrap();
        fs.part_append(&d, "a.bin", 0, b"abc").unwrap();
        assert_eq!(fs.part_append(&d, "a.bin", 0, b"x"), Err(ErrorCode::ProtocolViolation));
        assert_eq!(fs.part_append(&d, "a.bin", 3, &vec![0u8; MAX_IO + 1]), Err(ErrorCode::TooLarge));
        assert_eq!(fs.part_len(&d, "a.bin"), Ok(3));
        std::fs::write(d.join("a.bin"), b"mine").unwrap();
        assert_eq!(fs.part_rename(&d, "a.bin", "a.bin"), Err(ErrorCode::IoFailed));
        assert_eq!(std::fs::read(d.join("a.bin")).unwrap(), b"mine");
        assert_eq!(fs.part_read(&d, "a.bin", 1, 10), Ok(b"bc".to_vec()));
        fs.part_rename(&d, "a.bin", "b.bin").unwrap();
        assert_eq!(std::fs::read(d.join("b.bin")).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_handles_are_opaque_ids() {
        let d = dir("handles");
        std::fs::write(d.join("f"), b"hello world").unwrap();
        let fs = LocalFs::new();
        let h = fs.open_read(&d.join("f")).unwrap();
        assert_eq!(h.len, 11);
        assert_eq!(fs.read_at(h.id, 6, 100), Ok(b"world".to_vec()));
        assert_eq!(fs.read_stat(h.id).unwrap().len, 11);
        fs.close_read(h.id);
        assert_eq!(fs.read_at(h.id, 0, 1), Err(ErrorCode::NotFound));
        assert_eq!(fs.open_read(&d).err(), Some(ErrorCode::IoFailed));
        assert_eq!(fs.open_read(&d.join("none")).err(), Some(ErrorCode::NotFound));
        let _ = std::fs::remove_dir_all(&d);
    }
}
