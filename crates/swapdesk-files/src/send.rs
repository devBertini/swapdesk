//! The sending side of one file: the offer, and a reader that continues from the receiver's verified length while
//! hashing the whole file for `FileDone`.
//!
//! The sender reads as the logged-in user and follows links: the peer can already name any path that user can read
//! (`FileGet`), so a link adds no reach. A file that changes while it is sent fails the transfer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ring::digest::{Context, SHA256};
use swapdesk_proto::{ErrorCode, FileOffer};

use crate::disk::{Fs, ReadHandle};
use crate::local::LocalFs;
use crate::name::validate_name;

/// Bytes read and written per data-stream write.
pub const CHUNK: usize = 256 * 1024;

#[derive(Clone)]
pub struct Outgoing {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    name: String,
    size: u64,
    mtime_ms: u64,
}

impl std::fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outgoing")
            .field("path", &self.path)
            .field("name", &self.name)
            .field("size", &self.size)
            .field("mtime_ms", &self.mtime_ms)
            .finish()
    }
}

impl Outgoing {
    /// [`Outgoing::open_with`] on this machine's disk.
    pub fn open(path: &Path) -> Result<Self, ErrorCode> {
        Self::open_with(Arc::new(LocalFs::new()), path)
    }

    /// A regular file to send. Its name must be one the receiver will take (`INVALID_NAME` otherwise).
    pub fn open_with(fs: Arc<dyn Fs>, path: &Path) -> Result<Self, ErrorCode> {
        let h = fs.open_read(path)?;
        fs.close_read(h.id);
        let name = path.file_name().and_then(|n| n.to_str()).ok_or(ErrorCode::InvalidName)?.to_string();
        validate_name(&name)?;
        if name.len() > 255 {
            return Err(ErrorCode::InvalidName);
        }
        Ok(Self { fs, path: path.to_path_buf(), name, size: h.len, mtime_ms: h.mtime_ms })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn offer(&self, transfer_id: u32, dest: Option<String>) -> FileOffer {
        FileOffer { transfer_id, name: self.name.clone(), size: self.size, mtime_ms: self.mtime_ms, dest }
    }

    /// A reader positioned at `from` (the receiver's `resume_from`). The bytes before it are read and hashed so the
    /// final digest covers the whole file.
    pub fn reader(&self, from: u64) -> Result<ChunkReader, ErrorCode> {
        if from > self.size {
            return Err(ErrorCode::ProtocolViolation);
        }
        let h = self.fs.open_read(&self.path)?;
        // From here the reader owns the handle and closes it on drop.
        let mut r = ChunkReader {
            fs: Arc::clone(&self.fs),
            handle: h,
            hash: Context::new(&SHA256),
            pos: 0,
            size: self.size,
            mtime_ms: self.mtime_ms,
            from,
        };
        r.check_unchanged()?;
        while r.pos < from {
            let want = u32::try_from(CHUNK.min(usize::try_from(from - r.pos).unwrap_or(usize::MAX))).unwrap_or(0);
            let buf = r.fs.read_at(r.handle.id, r.pos, want)?;
            if buf.is_empty() {
                return Err(ErrorCode::IoFailed);
            }
            r.hash.update(&buf);
            r.pos += buf.len() as u64;
        }
        Ok(r)
    }
}

pub struct ChunkReader {
    fs: Arc<dyn Fs>,
    handle: ReadHandle,
    hash: Context,
    pos: u64,
    size: u64,
    mtime_ms: u64,
    from: u64,
}

impl Drop for ChunkReader {
    fn drop(&mut self) {
        self.fs.close_read(self.handle.id);
    }
}

impl ChunkReader {
    /// The offset of the first byte `next_chunk` returns.
    pub fn from(&self) -> u64 {
        self.from
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    fn check_unchanged(&self) -> Result<(), ErrorCode> {
        let st = self.fs.read_stat(self.handle.id).map_err(|_| ErrorCode::IoFailed)?;
        if st.len != self.size || st.mtime_ms != self.mtime_ms {
            return Err(ErrorCode::IoFailed);
        }
        Ok(())
    }

    /// The next bytes, `None` at the end of the file.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ErrorCode> {
        if self.pos >= self.size {
            return Ok(None);
        }
        let want = CHUNK.min(usize::try_from(self.size - self.pos).unwrap_or(CHUNK));
        let buf = self
            .fs
            .read_at(self.handle.id, self.pos, u32::try_from(want).unwrap_or(0))
            .map_err(|_| ErrorCode::IoFailed)?;
        if buf.len() != want {
            return Err(ErrorCode::IoFailed);
        }
        self.hash.update(&buf);
        self.pos += want as u64;
        Ok(Some(buf))
    }

    /// The SHA-256 of the whole file; only once everything was read, and only if the file did not change.
    pub fn sha256(self) -> Result<[u8; 32], ErrorCode> {
        if self.pos != self.size {
            return Err(ErrorCode::Internal);
        }
        self.check_unchanged()?;
        let d = self.hash.clone().finish();
        d.as_ref().try_into().map_err(|_| ErrorCode::Internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::digest::digest;

    fn file_with(tag: &str, data: &[u8], name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("swapdesk-files-send-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        (dir, p)
    }

    #[test]
    fn reader_from_an_offset_hashes_the_whole_file() {
        let data: Vec<u8> = (0..700_000u32).map(|i| (i % 253) as u8).collect();
        let (dir, p) = file_with("offset", &data, "x.bin");
        let out = Outgoing::open(&p).unwrap();
        assert_eq!((out.name(), out.size()), ("x.bin", 700_000));
        let mut r = out.reader(300_001).unwrap();
        let mut got = Vec::new();
        while let Some(c) = r.next_chunk().unwrap() {
            got.extend_from_slice(&c);
        }
        assert_eq!(got, data[300_001..]);
        assert_eq!(r.sha256().unwrap().as_slice(), digest(&SHA256, &data).as_ref());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offset_past_the_end_and_bad_sources_are_refused() {
        let (dir, p) = file_with("bad", b"abc", "x.bin");
        let out = Outgoing::open(&p).unwrap();
        assert_eq!(out.reader(4).err(), Some(ErrorCode::ProtocolViolation));
        assert!(out.reader(3).is_ok());
        assert_eq!(Outgoing::open(&dir).err(), Some(ErrorCode::IoFailed));
        assert_eq!(Outgoing::open(&dir.join("missing")).err(), Some(ErrorCode::NotFound));
        let (dir2, p2) = file_with("badname", b"abc", "nul.txt");
        assert_eq!(Outgoing::open(&p2).err(), Some(ErrorCode::InvalidName));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn a_file_that_changes_fails_the_hash_step() {
        let (dir, p) = file_with("changed", &[1u8; 1000], "x.bin");
        let out = Outgoing::open(&p).unwrap();
        let mut r = out.reader(0).unwrap();
        while r.next_chunk().unwrap().is_some() {}
        std::fs::write(&p, [2u8; 1001]).unwrap();
        assert_eq!(r.sha256().err(), Some(ErrorCode::IoFailed));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
