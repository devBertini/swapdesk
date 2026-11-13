//! The `.sdpkg` update package (`docs/UPDATES.md` 3): the same files as the release zip's programs, uncompressed, so
//! no decoder is needed in a service.
//!
//! Layout (integers little-endian): the magic [`MAGIC`], `count: u32`, then per entry `path_len: u16`, the path,
//! `mode: u16` (0o644 or 0o755), `size: u64` and the bytes. Nothing may follow the last entry. A path is relative,
//! ASCII letters, digits, `.`, `-`, `_` and `/`, with no empty, `.` or `..` segment, no leading `/` and no backslash;
//! no two entries share a path, and no entry is both a file and the folder of another. There are no symlinks: the
//! packer is given files only.
//!
//! [`Package::read_verified`] compares the size and the SHA-256 with the manifest's entry before it reads a byte of
//! the layout.

use crate::manifest::{FileEntry, MAX_PACKAGE};
use crate::{UpdateError, sha256};

pub const MAGIC: &[u8; 8] = b"SDPKG1\n\0";
const MAX_ENTRIES: u32 = 100_000;
const MAX_PATH: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub mode: u16,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Package {
    pub entries: Vec<Entry>,
}

/// A path the package may carry.
pub fn path_ok(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
        && path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

fn check_tree(entries: &[Entry]) -> Result<(), UpdateError> {
    let mut paths: Vec<&str> = Vec::with_capacity(entries.len());
    for e in entries {
        if !path_ok(&e.path) {
            return Err(UpdateError::BadPath);
        }
        if e.mode != 0o644 && e.mode != 0o755 {
            return Err(UpdateError::Malformed("mode"));
        }
        paths.push(&e.path);
    }
    paths.sort_unstable();
    if paths.windows(2).any(|w| w[0] == w[1]) {
        return Err(UpdateError::BadPath);
    }
    // A file that is also the folder of another entry.
    for p in &paths {
        let folder = format!("{p}/");
        if paths.iter().any(|q| q.starts_with(&folder)) {
            return Err(UpdateError::BadPath);
        }
    }
    Ok(())
}

struct Reader<'a> {
    b: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], UpdateError> {
        if self.b.len() < n {
            return Err(UpdateError::Malformed("truncated package"));
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }
    fn u16(&mut self) -> Result<u16, UpdateError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| UpdateError::Malformed("u16"))?))
    }
    fn u32(&mut self) -> Result<u32, UpdateError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| UpdateError::Malformed("u32"))?))
    }
    fn u64(&mut self) -> Result<u64, UpdateError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| UpdateError::Malformed("u64"))?))
    }
}

impl Package {
    /// The package's bytes. Refuses a tree [`Package::read`] would refuse.
    pub fn write(&self) -> Result<Vec<u8>, UpdateError> {
        check_tree(&self.entries)?;
        let count = u32::try_from(self.entries.len()).map_err(|_| UpdateError::TooLarge)?;
        if count > MAX_ENTRIES {
            return Err(UpdateError::TooLarge);
        }
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&count.to_le_bytes());
        for e in &self.entries {
            let len = u16::try_from(e.path.len()).map_err(|_| UpdateError::BadPath)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(e.path.as_bytes());
            out.extend_from_slice(&e.mode.to_le_bytes());
            out.extend_from_slice(&(e.bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(&e.bytes);
        }
        if out.len() as u64 > MAX_PACKAGE {
            return Err(UpdateError::TooLarge);
        }
        Ok(out)
    }

    /// Reads a package. Only for bytes already checked against the manifest: [`Package::read_verified`].
    pub fn read(bytes: &[u8]) -> Result<Package, UpdateError> {
        if bytes.len() as u64 > MAX_PACKAGE {
            return Err(UpdateError::TooLarge);
        }
        let mut r = Reader { b: bytes };
        if r.take(MAGIC.len())? != MAGIC {
            return Err(UpdateError::Malformed("magic"));
        }
        let count = r.u32()?;
        if count > MAX_ENTRIES {
            return Err(UpdateError::TooLarge);
        }
        let mut entries = Vec::new();
        for _ in 0..count {
            let len = usize::from(r.u16()?);
            let path = core::str::from_utf8(r.take(len)?).map_err(|_| UpdateError::BadPath)?.to_owned();
            let mode = r.u16()?;
            let size = usize::try_from(r.u64()?).map_err(|_| UpdateError::TooLarge)?;
            let bytes = r.take(size)?.to_vec();
            entries.push(Entry { path, mode, bytes });
        }
        if !r.b.is_empty() {
            return Err(UpdateError::Malformed("bytes after the last entry"));
        }
        check_tree(&entries)?;
        Ok(Package { entries })
    }

    /// Reads a downloaded package after its size and SHA-256 equal the manifest's entry.
    pub fn read_verified(bytes: &[u8], entry: &FileEntry) -> Result<Package, UpdateError> {
        if bytes.len() as u64 != entry.size || sha256(bytes) != entry.sha256 {
            return Err(UpdateError::Mismatch);
        }
        Package::read(bytes)
    }

    /// The entry at `path`.
    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.path == path)
    }
}
