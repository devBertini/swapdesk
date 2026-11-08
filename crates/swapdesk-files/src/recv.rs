//! Safe receiving of one file: a partial file in the target folder, a small record next to it of how many bytes are
//! known good, resume from that length, and the SHA-256 check before the rename.
//!
//! On disk, for the offer `name`:
//! - `name.swapdesk-part`: the bytes received so far;
//! - `name.swapdesk-meta`: 40 bytes (magic, offered size, offered mtime, verified length, a tag over the first 32).
//!
//! The verified length only moves after `sync_data` of the partial file, so after a crash or a killed link the
//! bytes up to it are the ones the sender sent; anything beyond is cut off on resume. The running hash is rebuilt
//! from the verified prefix on resume (the `ring` context cannot be saved).
//!
//! Every disk access goes through an [`Fs`]; the files are reached by folder and name, never by a path built here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ring::digest::{Context, SHA256, digest};
use swapdesk_proto::ErrorCode;

use crate::disk::{Fs, Kind, MAX_IO};
use crate::local::LocalFs;
use crate::name::validate_name;

const META_MAGIC: &[u8; 8] = b"SDFMETA1";
const META_LEN: usize = 40;
/// The verified length advances after this many new bytes (and on a pause or a clean stop).
pub const CHECKPOINT_BYTES: u64 = 1 << 20;
/// Bytes read back per call when the hash is rebuilt on resume.
const REHASH_CHUNK: usize = 256 * 1024;

/// Why `finish` did not produce a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishError {
    /// Fewer bytes arrived than were offered; nothing was touched.
    Incomplete,
    /// The hash differs: the partial file was deleted.
    Mismatch,
    Io,
}

pub struct Incoming {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    name: String,
    size: u64,
    mtime_ms: u64,
    /// The partial and record files are in use (false after a clean stop or the final rename).
    open: bool,
    hash: Context,
    received: u64,
    committed: u64,
}

fn meta_bytes(size: u64, mtime_ms: u64, committed: u64) -> [u8; META_LEN] {
    let mut b = [0u8; META_LEN];
    b[..8].copy_from_slice(META_MAGIC);
    b[8..16].copy_from_slice(&size.to_le_bytes());
    b[16..24].copy_from_slice(&mtime_ms.to_le_bytes());
    b[24..32].copy_from_slice(&committed.to_le_bytes());
    let tag = digest(&SHA256, &b[..32]);
    b[32..].copy_from_slice(&tag.as_ref()[..8]);
    b
}

/// The verified length in a meta record that belongs to this offer, `None` for anything else.
fn parse_meta(b: &[u8], size: u64, mtime_ms: u64) -> Option<u64> {
    if b.len() != META_LEN || &b[..8] != META_MAGIC {
        return None;
    }
    let tag = digest(&SHA256, &b[..32]);
    if tag.as_ref()[..8] != b[32..] {
        return None;
    }
    let u = |r: std::ops::Range<usize>| u64::from_le_bytes(b[r].try_into().unwrap_or([0; 8]));
    (u(8..16) == size && u(16..24) == mtime_ms).then(|| u(24..32))
}

impl Incoming {
    /// [`Incoming::open_with`] on this machine's disk.
    pub fn open(dir: &Path, name: &str, size: u64, mtime_ms: u64) -> Result<Self, ErrorCode> {
        Self::open_with(Arc::new(LocalFs::new()), dir, name, size, mtime_ms)
    }

    /// Prepares to receive `name` into `dir`. Finds the partial file of an earlier try with the same size and
    /// mtime and resumes from its verified length; anything else there is replaced.
    pub fn open_with(fs: Arc<dyn Fs>, dir: &Path, name: &str, size: u64, mtime_ms: u64) -> Result<Self, ErrorCode> {
        validate_name(name)?;
        match fs.stat(dir, true) {
            Ok(Some(s)) if s.is_dir => {}
            _ => return Err(ErrorCode::NotFound),
        }

        let mut resume = None;
        if let Some(pm) = fs.stat_file(dir, name, Kind::Part)? {
            if pm.is_reparse || !pm.is_file {
                return Err(ErrorCode::IoFailed);
            }
            if let Ok(Some(mm)) = fs.stat_file(dir, name, Kind::Meta)
                && !mm.is_reparse
                && mm.is_file
                && let Ok(Some(bytes)) = fs.meta_read(dir, name)
                && let Some(committed) = parse_meta(&bytes, size, mtime_ms)
            {
                resume = Some(committed.min(pm.len).min(size));
            }
        }
        if resume.is_none() {
            for kind in [Kind::Part, Kind::Meta] {
                if let Some(m) = fs.stat_file(dir, name, kind)? {
                    if m.is_reparse || !m.is_file {
                        return Err(ErrorCode::IoFailed);
                    }
                    fs.remove_file(dir, name, kind)?;
                }
            }
            fs.part_create(dir, name)?;
        }

        let committed = resume.unwrap_or(0);
        fs.part_truncate(dir, name, committed)?;
        let mut hash = Context::new(&SHA256);
        let mut at = 0u64;
        while at < committed {
            let want =
                u32::try_from(REHASH_CHUNK.min(usize::try_from(committed - at).unwrap_or(usize::MAX))).unwrap_or(0);
            let buf = fs.part_read(dir, name, at, want)?;
            if buf.is_empty() {
                return Err(ErrorCode::IoFailed);
            }
            hash.update(&buf);
            at += buf.len() as u64;
        }
        let mut me = Self {
            fs,
            dir: dir.to_path_buf(),
            name: name.to_string(),
            size,
            mtime_ms,
            open: true,
            hash,
            received: committed,
            committed,
        };
        me.write_meta()?;
        Ok(me)
    }

    fn write_meta(&mut self) -> Result<(), ErrorCode> {
        if !self.open {
            return Err(ErrorCode::Internal);
        }
        let rec = meta_bytes(self.size, self.mtime_ms, self.committed);
        self.fs.meta_write(&self.dir, &self.name, &rec)
    }

    /// Bytes on disk that are known good: where the sender is told to continue.
    pub fn resume_from(&self) -> u64 {
        self.committed
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Cuts the partial file back to the verified length and continues from there (a pause, a new data stream
    /// after a lost one). Returns the offset the sender must start at.
    pub fn realign(&mut self) -> Result<u64, ErrorCode> {
        self.checkpoint()?;
        // Everything up to `committed` is what we just synced; the running hash already covers it.
        Ok(self.committed)
    }

    /// Makes what was received so far the verified length (data is synced first).
    pub fn checkpoint(&mut self) -> Result<(), ErrorCode> {
        if self.committed == self.received {
            return Ok(());
        }
        if !self.open {
            return Err(ErrorCode::Internal);
        }
        self.fs.part_sync(&self.dir, &self.name, None)?;
        self.committed = self.received;
        self.write_meta()
    }

    /// Appends the bytes of a data stream that start at `offset`. The offset must be where the partial file ends;
    /// more bytes than were offered fail the transfer with `TOO_LARGE`.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), ErrorCode> {
        if offset != self.received {
            return Err(ErrorCode::ProtocolViolation);
        }
        let end = self.received.checked_add(data.len() as u64).ok_or(ErrorCode::TooLarge)?;
        if end > self.size {
            return Err(ErrorCode::TooLarge);
        }
        if !self.open {
            return Err(ErrorCode::Internal);
        }
        // One call per piece of at most `MAX_IO` bytes (the proxy's message limit).
        let mut at = self.received;
        for piece in data.chunks(MAX_IO) {
            self.fs.part_append(&self.dir, &self.name, at, piece)?;
            at += piece.len() as u64;
        }
        self.hash.update(data);
        self.received = end;
        if self.received - self.committed >= CHECKPOINT_BYTES {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Verifies the SHA-256 against the sender's and moves the file to its final name (never over an existing
    /// file: "name (1).ext" is used then). Returns the final path. A mismatch deletes the partial file. When the
    /// rename fails the partial file stays as it is.
    pub fn finish(&mut self, expected: &[u8; 32]) -> Result<PathBuf, FinishError> {
        if self.received != self.size {
            return Err(FinishError::Incomplete);
        }
        let got = self.hash.clone().finish();
        if got.as_ref() != expected {
            self.discard();
            return Err(FinishError::Mismatch);
        }
        if !self.open {
            return Err(FinishError::Io);
        }
        self.open = false;
        // Keep the sender's modification time when the OS lets us.
        self.fs.part_sync(&self.dir, &self.name, Some(self.mtime_ms)).map_err(|_| FinishError::Io)?;
        let target_name = self.unique_target();
        self.fs.part_rename(&self.dir, &self.name, &target_name).map_err(|_| FinishError::Io)?;
        let _ = self.fs.remove_file(&self.dir, &self.name, Kind::Meta);
        Ok(self.dir.join(target_name))
    }

    /// Deletes the partial file and its record (a cancel, a mismatch).
    pub fn discard(&mut self) {
        self.open = false;
        let _ = self.fs.remove_file(&self.dir, &self.name, Kind::Part);
        let _ = self.fs.remove_file(&self.dir, &self.name, Kind::Meta);
    }

    /// Makes the received bytes the verified length and keeps the files open (a pause).
    pub fn suspend_keep_open(&mut self) {
        let _ = self.checkpoint();
    }

    /// Stops cleanly: the received bytes become the verified length and the files are closed.
    pub fn suspend(&mut self) {
        let _ = self.checkpoint();
        self.open = false;
    }

    /// `name`, or `stem (n).ext` for the first n that is free in the folder.
    fn unique_target(&self) -> String {
        let free = |n: &str| !matches!(self.fs.stat(&self.dir.join(n), false), Ok(Some(_)));
        if free(&self.name) {
            return self.name.clone();
        }
        let name = &self.name;
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name.as_str(), ""),
        };
        (1u32..).map(|n| format!("{stem} ({n}){ext}")).find(|c| free(c)).unwrap_or_else(|| name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(
            format!("swapdesk-files-{tag}-{}-{:?}", std::process::id(), std::thread::current().id())
                .replace(['(', ')'], ""),
        );
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sha(data: &[u8]) -> [u8; 32] {
        digest(&SHA256, data).as_ref().try_into().unwrap()
    }

    #[test]
    fn full_receive_checks_hash_and_renames() {
        let dir = temp_dir("full");
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut rx = Incoming::open(&dir, "a.bin", data.len() as u64, 5_000).unwrap();
        assert_eq!(rx.resume_from(), 0);
        rx.write(0, &data[..60_000]).unwrap();
        assert_eq!(rx.finish(&sha(&data)), Err(FinishError::Incomplete));
        rx.write(60_000, &data[60_000..]).unwrap();
        let path = rx.finish(&sha(&data)).unwrap();
        assert_eq!(path, dir.join("a.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(!dir.join("a.bin.swapdesk-part").exists() && !dir.join("a.bin.swapdesk-meta").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatch_deletes_the_partial_file() {
        let dir = temp_dir("mismatch");
        let mut rx = Incoming::open(&dir, "a.bin", 4, 1).unwrap();
        rx.write(0, b"abcd").unwrap();
        assert_eq!(rx.finish(&sha(b"abce")), Err(FinishError::Mismatch));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_overwrites_an_existing_file() {
        let dir = temp_dir("nooverwrite");
        std::fs::write(dir.join("a.txt"), b"mine").unwrap();
        let mut rx = Incoming::open(&dir, "a.txt", 3, 1).unwrap();
        rx.write(0, b"abc").unwrap();
        let path = rx.finish(&sha(b"abc")).unwrap();
        assert_eq!(path, dir.join("a (1).txt"));
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_offset_and_oversize_are_refused() {
        let dir = temp_dir("limits");
        let mut rx = Incoming::open(&dir, "a.bin", 10, 1).unwrap();
        assert_eq!(rx.write(3, b"x"), Err(ErrorCode::ProtocolViolation));
        assert_eq!(rx.write(0, &[0u8; 11]), Err(ErrorCode::TooLarge));
        rx.write(0, &[1u8; 10]).unwrap();
        assert_eq!(rx.write(10, b"x"), Err(ErrorCode::TooLarge));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_names_do_not_touch_the_disk() {
        let dir = temp_dir("badname");
        for n in ["../x", "a\\b", "nul.txt", "s:ads", ".."] {
            assert_eq!(Incoming::open(&dir, n, 1, 1).err(), Some(ErrorCode::InvalidName), "{n}");
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_continues_from_the_verified_length_and_the_hash_matches() {
        let dir = temp_dir("resume");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(31) >> 3) as u8).collect();
        let size = data.len() as u64;
        let mut rx = Incoming::open(&dir, "big.bin", size, 77).unwrap();
        // 2.5 MiB arrive, then the link dies: the receiver is dropped without a clean stop. Two checkpoints
        // happened (1 MiB each time a megabyte was pending), the tail after the last one is not trusted.
        let mut off = 0usize;
        while off < 2_600_000 {
            let end = (off + 100_000).min(data.len());
            rx.write(off as u64, &data[off..end]).unwrap();
            off = end;
        }
        let committed = rx.resume_from();
        assert!(committed > 0 && committed < off as u64, "committed {committed} received {off}");
        drop(rx);

        let mut rx = Incoming::open(&dir, "big.bin", size, 77).unwrap();
        assert_eq!(rx.resume_from(), committed);
        assert_eq!(rx.received(), committed);
        let start = committed as usize;
        rx.write(committed, &data[start..]).unwrap();
        let path = rx.finish(&sha(&data)).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_file_of_another_offer_is_replaced() {
        let dir = temp_dir("otheroffer");
        let mut rx = Incoming::open(&dir, "a.bin", 100, 1).unwrap();
        rx.write(0, &[9u8; 50]).unwrap();
        rx.suspend();
        // Same name, other mtime: a different file.
        let rx2 = Incoming::open(&dir, "a.bin", 100, 2).unwrap();
        assert_eq!(rx2.resume_from(), 0);
        let rx3 = Incoming::open(&dir, "a.bin", 101, 2).unwrap();
        assert_eq!(rx3.resume_from(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suspend_makes_everything_received_verified() {
        let dir = temp_dir("suspend");
        let mut rx = Incoming::open(&dir, "a.bin", 100, 1).unwrap();
        rx.write(0, &[7u8; 40]).unwrap();
        assert_eq!(rx.resume_from(), 0);
        rx.suspend();
        let rx = Incoming::open(&dir, "a.bin", 100, 1).unwrap();
        assert_eq!(rx.resume_from(), 40);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_meta_record_starts_over() {
        let dir = temp_dir("corrupt");
        let mut rx = Incoming::open(&dir, "a.bin", 100, 1).unwrap();
        rx.write(0, &[7u8; 40]).unwrap();
        rx.suspend();
        let mut m = std::fs::read(dir.join("a.bin.swapdesk-meta")).unwrap();
        m[25] ^= 1;
        std::fs::write(dir.join("a.bin.swapdesk-meta"), m).unwrap();
        let rx = Incoming::open(&dir, "a.bin", 100, 1).unwrap();
        assert_eq!(rx.resume_from(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
