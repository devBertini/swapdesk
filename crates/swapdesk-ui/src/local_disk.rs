//! This computer's disk for the explorer's left pane (`docs/FILES-UI.md` 2, slice 1): a folder's listing, the places
//! (home, desktop, documents, downloads) and the drives or volumes. The interface reads its own disk itself (CONTRACTS
//! C2.3: nothing about this computer's disk reaches the service); the backend runs these on a worker thread, never on
//! the UI thread (R-UI-8). Only `std`: no OS call of its own, so both systems share it.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::api::{ApiCode, DirEntryView, LocalListing, LocalPlace, PlaceKind};

/// Most entries one listing returns (the same cap as a remote listing, PROTOCOL.md 5.3 `DirListing`).
pub const MAX_LOCAL_ENTRIES: usize = 20_000;

/// The engine's own side files and dot files are hidden (FILES-UI.md 2.5); on Windows also the hidden and system
/// attributes.
pub fn is_hidden(name: &str, attributes_hidden: bool) -> bool {
    attributes_hidden
        || name.starts_with('.')
        || name.ends_with(".swapdesk-part")
        || name.ends_with(".swapdesk-meta")
        || name.eq_ignore_ascii_case("desktop.ini")
}

#[cfg(windows)]
fn hidden_attribute(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_HIDDEN (0x2) and FILE_ATTRIBUTE_SYSTEM (0x4).
    m.file_attributes() & 0x6 != 0
}

#[cfg(not(windows))]
fn hidden_attribute(_: &std::fs::Metadata) -> bool {
    false
}

/// The entries of `dir`, hidden ones left out, at most [`MAX_LOCAL_ENTRIES`] (`truncated` then). Not sorted: the
/// explorer sorts as its column says.
pub fn list(dir: &Path) -> Result<LocalListing, ApiCode> {
    let read = std::fs::read_dir(dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ApiCode::NotFound,
        std::io::ErrorKind::PermissionDenied => ApiCode::NotPermitted,
        _ => ApiCode::Internal,
    })?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let Ok(meta) = item.metadata() else { continue };
        if is_hidden(&name, hidden_attribute(&meta)) {
            continue;
        }
        if entries.len() >= MAX_LOCAL_ENTRIES {
            truncated = true;
            break;
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        entries.push(DirEntryView {
            name,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            mtime_ms,
        });
    }
    Ok(LocalListing { path: dir.to_path_buf(), entries, truncated })
}

/// The user's home folder.
pub fn home() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_dir())
}

/// The folder the left pane opens at first: the user's Downloads, else the home folder.
pub fn default_dir() -> Option<PathBuf> {
    let home = home()?;
    let downloads = home.join("Downloads");
    Some(if downloads.is_dir() { downloads } else { home })
}

/// The places and the volumes, in the navigation pane's order. A place whose folder does not exist is left out.
pub fn places() -> Vec<LocalPlace> {
    let mut out = Vec::new();
    if let Some(home) = home() {
        out.push(LocalPlace { kind: PlaceKind::Home, label: String::new(), path: home.clone() });
        for (kind, sub) in
            [(PlaceKind::Desktop, "Desktop"), (PlaceKind::Documents, "Documents"), (PlaceKind::Downloads, "Downloads")]
        {
            let p = home.join(sub);
            if p.is_dir() {
                out.push(LocalPlace { kind, label: String::new(), path: p });
            }
        }
    }
    out.extend(volumes());
    out
}

/// The drives (Windows: the letters that answer) or the mounted volumes (macOS: `/` and `/Volumes/*`).
#[cfg(windows)]
pub fn volumes() -> Vec<LocalPlace> {
    // A, B: floppy letters are skipped (a probe of an empty floppy drive waits on the hardware).
    (b'C'..=b'Z')
        .filter_map(|l| {
            let root = PathBuf::from(format!("{}:\\", char::from(l)));
            std::fs::metadata(&root).is_ok().then(|| LocalPlace {
                kind: PlaceKind::Volume,
                label: format!("{}:", char::from(l)),
                path: root,
            })
        })
        .collect()
}

#[cfg(not(windows))]
pub fn volumes() -> Vec<LocalPlace> {
    let mut out = Vec::new();
    let mut names: Vec<(String, PathBuf)> = std::fs::read_dir("/Volumes")
        .map(|r| {
            r.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let path = e.path();
                    // The startup volume appears in /Volumes as a link to `/`: it is listed once, as the root.
                    let is_root = std::fs::canonicalize(&path).ok().is_some_and(|c| c == Path::new("/"));
                    (!name.starts_with('.') && !is_root).then_some((name, path))
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    out.push(LocalPlace { kind: PlaceKind::Volume, label: String::from("Macintosh HD"), path: PathBuf::from("/") });
    out.extend(names.into_iter().map(|(label, path)| LocalPlace { kind: PlaceKind::Volume, label, path }));
    out
}

/// The reply to a `LocalList` or `LocalPlaces` command (any other command is `Invalid`).
pub fn answer(command: &crate::api::Command) -> Result<crate::api::Reply, crate::api::ApiError> {
    use crate::api::{ApiError, Command, Reply};
    match command {
        Command::LocalList { path } => {
            let dir = path.clone().or_else(default_dir).ok_or_else(|| ApiError::new(ApiCode::NotFound))?;
            list(&dir).map(Reply::LocalListing).map_err(ApiError::new)
        }
        Command::LocalPlaces => Ok(Reply::LocalPlaces(places())),
        _ => Err(ApiError::new(ApiCode::Invalid)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_names_and_the_engine_files_are_left_out() {
        assert!(is_hidden(".git", false));
        assert!(is_hidden("a.bin.swapdesk-part", false));
        assert!(is_hidden("a.bin.swapdesk-meta", false));
        assert!(is_hidden("NTUSER.DAT", true), "the system's own hidden flag");
        assert!(!is_hidden("relatório.pdf", false));
    }

    #[test]
    fn a_folder_lists_its_files_and_folders_and_a_missing_one_is_not_found() {
        let dir = std::env::temp_dir().join(format!("swapdesk-local-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"abc").unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        let mut l = list(&dir).unwrap();
        l.entries.sort_by(|a, b| a.name.cmp(&b.name));
        let seen: Vec<_> = l.entries.iter().map(|e| (e.name.as_str(), e.is_dir, e.size)).collect();
        assert_eq!(seen, vec![("a.txt", false, 3), ("sub", true, 0)]);
        assert!(!l.truncated && l.path == dir);
        assert_eq!(list(&dir.join("missing")).err(), Some(ApiCode::NotFound));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_places_start_with_the_home_folder_and_list_at_least_one_volume() {
        let p = places();
        if home().is_some() {
            assert_eq!(p[0].kind, PlaceKind::Home);
        }
        assert!(p.iter().any(|x| x.kind == PlaceKind::Volume), "{p:?}");
    }
}
