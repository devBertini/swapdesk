//! Folder listing for F-FILE-02: what the host answers a `DirRequest` with, as the logged-in user.

use std::path::{Path, PathBuf};

use swapdesk_proto::{DirEntry, ErrorCode, MAX_FRAME_PAYLOAD, Wire};

use crate::disk::Fs;
use crate::local::LocalFs;

/// Most entries a `DirListing` carries (PROTOCOL.md 5.3).
pub const MAX_ENTRIES: usize = 2000;

/// What a listing holds before it becomes a `DirListing`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The folder listed, as the host spells it.
    pub path: String,
    pub entries: Vec<DirEntry>,
    pub truncated: bool,
}

/// [`list_dir_with`] on this machine's disk.
pub fn list_dir(path: &Path) -> Result<Listing, ErrorCode> {
    list_dir_with(&LocalFs::new(), path)
}

/// Lists `path` through `fs`: folders first, then files, each group by name (case-insensitive). Entries whose name is
/// not valid UTF-8 or is over 255 bytes are left out, since the wire cannot carry them. The list stops before the
/// encoded frame would pass 65 535 bytes or 2000 entries, and `truncated` says so.
pub fn list_dir_with(fs: &dyn Fs, path: &Path) -> Result<Listing, ErrorCode> {
    let read = fs.list_dir(path, MAX_ENTRIES as u32)?;
    let shown = path.to_str().ok_or(ErrorCode::IoFailed)?.to_string();
    if shown.len() > 1024 {
        return Err(ErrorCode::TooLarge);
    }
    let all: Vec<DirEntry> = read
        .entries
        .into_iter()
        .map(|e| DirEntry { name: e.name, is_dir: e.is_dir, size: e.size, mtime_ms: e.mtime_ms })
        .collect();

    // The frame holds: req_id 4, the path (length prefix up to 2 bytes + bytes), the entry count (up to 4), the flag.
    let mut budget = MAX_FRAME_PAYLOAD.saturating_sub(4 + 2 + shown.len() + 4 + 1);
    let mut entries = Vec::new();
    let mut truncated = read.truncated;
    for e in all {
        let cost = e.encode().map_err(|_| ErrorCode::Internal)?.len();
        if entries.len() >= MAX_ENTRIES || cost > budget {
            truncated = true;
            break;
        }
        budget -= cost;
        entries.push(e);
    }
    Ok(Listing { path: shown, entries, truncated })
}

/// The folder a `DirRequest` with no path means: the user's home. The host passes the home folder it found
/// (`USERPROFILE` or `HOME`) so this crate reads no environment of its own.
pub fn resolve_request(path: Option<&str>, home: &Path) -> PathBuf {
    path.map_or_else(|| home.to_path_buf(), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_proto::{DirListing, Message};

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("swapdesk-files-list-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn folders_first_then_files_by_name() {
        let d = dir("order");
        std::fs::write(d.join("b.txt"), b"12345").unwrap();
        std::fs::write(d.join("A.txt"), b"1").unwrap();
        std::fs::create_dir(d.join("zeta")).unwrap();
        std::fs::create_dir(d.join("alpha")).unwrap();
        let l = list_dir(&d).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| (e.name.as_str(), e.is_dir, e.size)).collect();
        assert_eq!(names, [("alpha", true, 0), ("zeta", true, 0), ("A.txt", false, 1), ("b.txt", false, 5)]);
        assert!(!l.truncated);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_folder_is_not_found() {
        let d = dir("missing");
        assert_eq!(list_dir(&d.join("nope")).err(), Some(ErrorCode::NotFound));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_big_folder_is_cut_before_the_frame_overflows() {
        let d = dir("big");
        for i in 0..1500 {
            std::fs::write(d.join(format!("{i:04}-{}", "n".repeat(200))), b"").unwrap();
        }
        let l = list_dir(&d).unwrap();
        assert!(l.truncated && l.entries.len() < 1500 && !l.entries.is_empty());
        let msg = Message::DirListing(DirListing {
            req_id: u32::MAX,
            path: l.path,
            entries: l.entries,
            truncated: l.truncated,
        });
        let frame = msg.encode_frame().expect("the listing fits one frame");
        assert!(frame.len() <= MAX_FRAME_PAYLOAD + 8);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn request_without_a_path_is_home() {
        assert_eq!(resolve_request(None, Path::new("H")), PathBuf::from("H"));
        assert_eq!(resolve_request(Some("X"), Path::new("H")), PathBuf::from("X"));
    }
}
