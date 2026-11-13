//! This computer's disk for the explorer's left pane (`docs/FILES-UI.md` 2, slice 1): a folder's listing, the places
//! (home, desktop, documents, downloads) and the drives or volumes. The interface reads its own disk itself (CONTRACTS
//! C2.3: nothing about this computer's disk reaches the service); the backend runs these on a worker thread, never on
//! the UI thread (R-UI-8). Only `std`: no OS call of its own, so both systems share it.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use std::sync::OnceLock;

use crate::api::{ApiCode, DirEntryView, LocalListing, LocalPlace, PlaceKind};

/// What this computer's disk needs from the system and `std` cannot do: the bin and a volume's free space. Each shell
/// gives its own at start ([`set_os`]); the calls run on the backend's worker thread, never the UI thread (R-UI-8).
#[derive(Clone, Copy)]
pub struct LocalOs {
    /// Sends the paths to the system's bin. `Refused`: the volume has no bin, and nothing was deleted.
    pub trash: fn(&[PathBuf]) -> Result<(), ApiCode>,
    pub free_bytes: fn(&Path) -> Option<u64>,
}

static OS: OnceLock<LocalOs> = OnceLock::new();

/// The shell's system calls for this computer's disk; once, at start.
pub fn set_os(os: LocalOs) {
    let _ = OS.set(os);
}

/// Most entries one listing returns (the same cap as a remote listing, PROTOCOL.md 5.3 `DirListing`).
pub const MAX_LOCAL_ENTRIES: usize = 20_000;

/// The transfer engine's own side files: never listed, hidden items shown or not.
fn is_engine_file(name: &str) -> bool {
    name.ends_with(".swapdesk-part") || name.ends_with(".swapdesk-meta")
}

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
    let mut hidden = Vec::new();
    let mut truncated = false;
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let Ok(meta) = item.metadata() else { continue };
        if is_engine_file(&name) {
            continue;
        }
        let into_hidden = is_hidden(&name, hidden_attribute(&meta));
        if entries.len() + hidden.len() >= MAX_LOCAL_ENTRIES {
            truncated = true;
            break;
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let entry =
            DirEntryView { name, is_dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() }, mtime_ms };
        if into_hidden { hidden.push(entry) } else { entries.push(entry) }
    }
    Ok(LocalListing { path: dir.to_path_buf(), entries, truncated, hidden })
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

/// The reply to a command on this computer's disk (any other command is `Invalid`).
pub fn answer(command: &crate::api::Command) -> Result<crate::api::Reply, crate::api::ApiError> {
    use crate::api::{ApiError, Command, Reply};
    let done = |r: Result<(), ApiCode>| r.map(|()| Reply::Done).map_err(ApiError::new);
    match command {
        Command::LocalList { path } => {
            let dir = path.clone().or_else(default_dir).ok_or_else(|| ApiError::new(ApiCode::NotFound))?;
            list(&dir).map(Reply::LocalListing).map_err(ApiError::new)
        }
        Command::LocalPlaces => Ok(Reply::LocalPlaces(places())),
        Command::LocalMakeDir { path } => done(make_dir(path)),
        Command::LocalRename { path, name } => done(rename(path, name).map(|_| ())),
        Command::LocalMove { paths, into } => done(move_into(paths, into)),
        Command::LocalDeleteForever { paths } => done(delete_forever(paths)),
        // Without the shell's bin nothing is deleted: a delete that cannot reach a bin never becomes a permanent one.
        Command::LocalTrash { paths } => done(OS.get().map_or(Err(ApiCode::Refused), |os| (os.trash)(paths))),
        Command::LocalProps { path } => props(path).map(Reply::LocalProps).map_err(ApiError::new),
        _ => Err(ApiError::new(ApiCode::Invalid)),
    }
}

fn io_code(e: &std::io::Error) -> ApiCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => ApiCode::NotFound,
        std::io::ErrorKind::PermissionDenied => ApiCode::NotPermitted,
        std::io::ErrorKind::AlreadyExists => ApiCode::Refused,
        _ => ApiCode::Internal,
    }
}

/// A name a folder of this computer can hold: the shared rule (FILES-UI.md 7.5) and, on Windows, none of `* ? " < >
/// |`, which Windows refuses.
pub fn name_ok(name: &str) -> bool {
    swapdesk_files::validate_name(name).is_ok() && !(cfg!(windows) && name.chars().any(|c| "*?\"<>|".contains(c)))
}

/// "Nova pasta", then "Nova pasta (2)" and on, the first that no entry of the folder has (letters' case aside).
pub fn next_free_name(base: &str, taken: &[&str]) -> String {
    let free = |n: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(n) || t.to_lowercase() == n.to_lowercase());
    if free(base) {
        return base.to_owned();
    }
    (2..).map(|i| format!("{base} ({i})")).find(|n| free(n)).unwrap_or_else(|| base.to_owned())
}

/// Creates the folder `path`; `Refused` when something has its name already.
pub fn make_dir(path: &Path) -> Result<(), ApiCode> {
    let name = path.file_name().and_then(|n| n.to_str()).ok_or(ApiCode::Invalid)?;
    if !name_ok(name) {
        return Err(ApiCode::Invalid);
    }
    std::fs::create_dir(path).map_err(|e| io_code(&e))
}

/// Renames `path` to `name` in the same folder and returns the new path. A change of letters' case alone is a
/// rename too; any other taken name is `Refused` (nothing is replaced).
pub fn rename(path: &Path, name: &str) -> Result<PathBuf, ApiCode> {
    if !name_ok(name) {
        return Err(ApiCode::Invalid);
    }
    let old = path.file_name().and_then(|n| n.to_str()).ok_or(ApiCode::Invalid)?;
    let to = path.parent().ok_or(ApiCode::Invalid)?.join(name);
    if old == name {
        return Ok(to);
    }
    let case_only = old.to_lowercase() == name.to_lowercase();
    if !case_only && std::fs::symlink_metadata(&to).is_ok() {
        return Err(ApiCode::Refused);
    }
    std::fs::rename(path, &to).map_err(|e| io_code(&e))?;
    Ok(to)
}

/// Moves `paths` into `into`. Refused before anything moves when a name is taken there or a folder would go into
/// itself; across volumes a folder or file is copied, then the source is removed.
pub fn move_into(paths: &[PathBuf], into: &Path) -> Result<(), ApiCode> {
    let into_full = std::fs::canonicalize(into).map_err(|e| io_code(&e))?;
    for p in paths {
        let full = std::fs::canonicalize(p).map_err(|e| io_code(&e))?;
        if into_full.starts_with(&full) {
            return Err(ApiCode::Invalid);
        }
        let name = p.file_name().ok_or(ApiCode::Invalid)?;
        if full.parent() != Some(into_full.as_path()) && std::fs::symlink_metadata(into.join(name)).is_ok() {
            return Err(ApiCode::Refused);
        }
    }
    for p in paths {
        let name = p.file_name().ok_or(ApiCode::Invalid)?;
        let to = into.join(name);
        if std::fs::canonicalize(p).ok().and_then(|f| f.parent().map(Path::to_path_buf)) == Some(into_full.clone()) {
            continue;
        }
        if std::fs::rename(p, &to).is_err() {
            // Another volume: copy, then remove the source only once the copy is whole.
            copy_tree(p, &to).map_err(|e| io_code(&e))?;
            remove_tree(p).map_err(|e| io_code(&e))?;
        }
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

fn remove_tree(p: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

/// Deletes `paths` for good (the person confirmed).
pub fn delete_forever(paths: &[PathBuf]) -> Result<(), ApiCode> {
    for p in paths {
        remove_tree(p).map_err(|e| io_code(&e))?;
    }
    Ok(())
}

/// Most entries a properties count walks before it reports what it has (`truncated`).
pub const MAX_PROPS_ENTRIES: u64 = 2_000_000;

/// What is inside `path`: its files, folders and bytes (links are counted, not followed).
pub fn props(path: &Path) -> Result<crate::api::LocalProps, ApiCode> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| io_code(&e))?;
    let free_bytes = OS.get().and_then(|os| (os.free_bytes)(path));
    let mut out = crate::api::LocalProps {
        path: path.to_path_buf(),
        files: 0,
        folders: 0,
        bytes: 0,
        truncated: false,
        free_bytes,
    };
    if !meta.is_dir() {
        out.files = 1;
        out.bytes = meta.len();
        return Ok(out);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            if out.files + out.folders >= MAX_PROPS_ENTRIES {
                out.truncated = true;
                return Ok(out);
            }
            let Ok(m) = entry.metadata() else { continue };
            if m.is_dir() && !entry.file_type().is_ok_and(|t| t.is_symlink()) {
                out.folders += 1;
                stack.push(entry.path());
            } else {
                out.files += 1;
                out.bytes += m.len();
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swapdesk-2a-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_folder_takes_the_first_free_name_and_renames_in_place() {
        assert_eq!(next_free_name("Nova pasta", &["a.txt"]), "Nova pasta");
        assert_eq!(next_free_name("Nova pasta", &["nova PASTA", "Nova pasta (2)"]), "Nova pasta (3)");
        let dir = scratch("new");
        make_dir(&dir.join("Nova pasta")).unwrap();
        assert_eq!(make_dir(&dir.join("Nova pasta")), Err(ApiCode::Refused));
        let to = rename(&dir.join("Nova pasta"), "Projetos").unwrap();
        assert!(to.is_dir() && !dir.join("Nova pasta").exists());
        rename(&to, "PROJETOS").expect("a change of case alone is a rename");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_refuses_a_taken_or_invalid_name_and_changes_nothing() {
        let dir = scratch("rename");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        assert_eq!(rename(&dir.join("a.txt"), "b.txt"), Err(ApiCode::Refused));
        for bad in ["", "x/y", "con", "trailing.", " lead", "a:b"] {
            assert_eq!(rename(&dir.join("a.txt"), bad), Err(ApiCode::Invalid), "{bad:?}");
        }
        if cfg!(windows) {
            assert_eq!(rename(&dir.join("a.txt"), "what?.txt"), Err(ApiCode::Invalid));
        }
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"b", "nothing replaced");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cut_and_paste_moves_inside_this_computer_and_never_replaces() {
        let dir = scratch("move");
        std::fs::create_dir_all(dir.join("src/inner")).unwrap();
        std::fs::write(dir.join("src/inner/f.txt"), b"f").unwrap();
        std::fs::create_dir_all(dir.join("dest")).unwrap();
        move_into(&[dir.join("src")], &dir.join("dest")).unwrap();
        assert_eq!(std::fs::read(dir.join("dest/src/inner/f.txt")).unwrap(), b"f");
        assert!(!dir.join("src").exists());
        assert_eq!(move_into(&[dir.join("dest")], &dir.join("dest/src")), Err(ApiCode::Invalid), "into itself");
        std::fs::create_dir_all(dir.join("other/src")).unwrap();
        assert_eq!(move_into(&[dir.join("other/src")], &dir.join("dest")), Err(ApiCode::Refused));
        assert!(dir.join("other/src").exists(), "nothing moved");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn properties_count_a_folders_files_folders_and_bytes() {
        let dir = scratch("props");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/x.bin"), [0u8; 10]).unwrap();
        std::fs::write(dir.join("a/b/y.bin"), [0u8; 5]).unwrap();
        let p = props(&dir.join("a")).unwrap();
        assert_eq!((p.files, p.folders, p.bytes, p.truncated), (2, 1, 15, false));
        delete_forever(&[dir.join("a")]).unwrap();
        assert!(!dir.join("a").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
