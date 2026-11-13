//! Small file-system helpers: the reparse-point check and the open calls that never follow one.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// True when `meta` describes a symbolic link, junction or any other reparse point (`FILE_ATTRIBUTE_REPARSE_POINT`
/// on Windows, a symlink elsewhere). Takes the metadata of the entry itself (`symlink_metadata`), never of its target.
pub fn is_reparse(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 || meta.file_type().is_symlink()
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Something exists at `path` (a dangling link counts).
pub fn exists_nofollow(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn no_follow(opts: &mut OpenOptions) {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the link itself, so the check below sees it.
        opts.custom_flags(0x0020_0000);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW on macOS.
        opts.custom_flags(0x0100);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = opts;
    }
}

/// What `open_rw_nofollow` does when nothing, or something, is at the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Create {
    /// The file must exist.
    No,
    /// The file must not exist yet.
    New,
    /// Open the file, creating it when missing.
    OrOpen,
}

/// Opens a regular file for reading and writing without following a reparse point. `Create::New` fails when
/// anything is there already. The opened handle is checked again, so a link swapped in after the caller's own check
/// is refused as well.
pub fn open_rw_nofollow(path: &Path, create: Create) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    match create {
        Create::No => {}
        Create::New => {
            opts.create_new(true);
        }
        Create::OrOpen => {
            opts.create(true);
        }
    }
    no_follow(&mut opts);
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    if is_reparse(&meta) || !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

/// Opens a regular file for reading only (the sending side: the user's own file, links are read through).
pub fn open_read(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

/// The same location as `p` with every name in its long, final form (Windows 8.3 short names such as `PROGRA~3` are
/// expanded, links are resolved), for the guards that compare places by their names. The deepest ancestor that exists
/// is resolved by the system and the part below it, which does not exist yet (a folder about to be made), is appended
/// as written. A path nothing of which exists is returned as it is.
pub fn long_form(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        match std::fs::canonicalize(&cur) {
            Ok(real) => {
                let mut out = strip_verbatim(&real);
                out.extend(tail.iter().rev());
                return out;
            }
            Err(_) => match (cur.file_name(), cur.parent()) {
                (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                    tail.push(name.to_owned());
                    cur = parent.to_path_buf();
                }
                _ => return p.to_path_buf(),
            },
        }
    }
}

/// `canonicalize` on Windows answers in the verbatim form (a `//?/` prefix before the drive); the guards compare names as the rest of the
/// crate writes them, so a verbatim drive path goes back to the plain one.
fn strip_verbatim(p: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut comps = p.components();
    if let Some(Component::Prefix(pc)) = comps.clone().next()
        && let Prefix::VerbatimDisk(letter) = pc.kind()
    {
        comps.next();
        let mut out = PathBuf::from(format!("{}:", char::from(letter)));
        for c in comps {
            out.push(c.as_os_str());
        }
        return out;
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn eq_nocase(a: &Path, b: &Path) -> bool {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    }

    #[test]
    #[cfg(windows)]
    fn long_form_expands_a_short_name_and_keeps_the_missing_tail() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let short = PathBuf::from(format!("{drive}/PROGRA~3"));
        if !short.is_dir() {
            eprintln!("no 8.3 name for ProgramData on this volume: nothing to resolve");
            return;
        }
        let want = PathBuf::from(std::env::var("ProgramData").expect("ProgramData"));
        assert!(eq_nocase(&long_form(&short), &want), "{:?} vs {want:?}", long_form(&short));
        let tail = short.join("swapdesk-no-such-folder").join("deeper").join("x.txt");
        let long = long_form(&tail);
        assert!(eq_nocase(&long, &want.join("swapdesk-no-such-folder").join("deeper").join("x.txt")), "{long:?}");
    }

    #[test]
    fn long_form_names_the_same_place_and_keeps_what_is_missing() {
        let here = std::env::temp_dir();
        let missing = here.join("swapdesk-long-form-missing").join("a");
        let long = long_form(&missing);
        assert!(long.ends_with(Path::new("swapdesk-long-form-missing").join("a")), "{long:?}");
        assert_eq!(long.parent().and_then(Path::parent), Some(long_form(&here).as_path()));
        // A path that is entirely relative and absent is returned as it is.
        assert_eq!(long_form(Path::new("swapdesk-no-such-thing")), Path::new("swapdesk-no-such-thing"));
    }
}
