//! What the release tools do, as functions the tests call (the bins only read their arguments).
//!
//! * [`pack`]: a build's folder into a [`Package`]: every file under it, its path relative to the folder, mode 0755
//!   for programs (an executable bit on Unix; on Windows `.exe` and `.dll`, and files under `Contents/MacOS`) and
//!   0644 for the rest. A symbolic link anywhere is refused: the package format has none.
//! * [`manifest_for`]: the manifest of a release from the packages the owner's PC downloaded, hashed here from their
//!   bytes, never from a `.sha256` that came with them.
//! * [`sign_release`]: the manifest and its signature, checked against the key's own public half before they are
//!   written.

use std::fs;
use std::path::{Path, PathBuf};

use swapdesk_update::manifest::{public_key, sign, verify_with};
use swapdesk_update::{Apply, Channel, Entry, FileEntry, Manifest, Package, Version, sha256};

/// Why a tool stopped.
#[derive(Debug)]
pub enum ToolError {
    Io(PathBuf, std::io::Error),
    Symlink(PathBuf),
    Update(swapdesk_update::UpdateError),
    Usage(String),
}

impl core::fmt::Display for ToolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Symlink(p) => write!(f, "{}: a symbolic link (the package format has none)", p.display()),
            Self::Update(e) => write!(f, "{e}"),
            Self::Usage(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<swapdesk_update::UpdateError> for ToolError {
    fn from(e: swapdesk_update::UpdateError) -> Self {
        Self::Update(e)
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ToolError + '_ {
    move |e| ToolError::Io(path.to_path_buf(), e)
}

#[cfg(unix)]
fn executable(_rel: &str, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(rel: &str, _meta: &fs::Metadata) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.ends_with(".exe") || lower.ends_with(".dll") || rel.contains("/Contents/MacOS/")
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<(), ToolError> {
    let mut names: Vec<_> = fs::read_dir(dir).map_err(io(dir))?.collect::<Result<_, _>>().map_err(io(dir))?;
    names.sort_by_key(fs::DirEntry::file_name);
    for item in names {
        let path = item.path();
        let meta = fs::symlink_metadata(&path).map_err(io(&path))?;
        if meta.file_type().is_symlink() {
            return Err(ToolError::Symlink(path));
        }
        if meta.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| ToolError::Usage(format!("{} is outside the folder", path.display())))?
            .to_string_lossy()
            .replace('\\', "/");
        let mode = if executable(&rel, &meta) { 0o755 } else { 0o644 };
        let bytes = fs::read(&path).map_err(io(&path))?;
        out.push(Entry { path: rel, mode, bytes });
    }
    Ok(())
}

/// Every file under `root` as a package (see the module).
pub fn pack(root: &Path) -> Result<Package, ToolError> {
    let meta = fs::symlink_metadata(root).map_err(io(root))?;
    if meta.file_type().is_symlink() {
        return Err(ToolError::Symlink(root.to_path_buf()));
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    let package = Package { entries };
    // The package's own rules (paths, modes, duplicates) are checked by its writer.
    package.write()?;
    Ok(package)
}

/// The manifest of `version` for the packages `files` (`(target, path)`), each hashed from its bytes.
pub fn manifest_for(version: &str, apply: Apply, files: &[(String, PathBuf)]) -> Result<Manifest, ToolError> {
    let version = Version::parse(version)?;
    let mut entries = Vec::new();
    for (target, path) in files {
        let bytes = fs::read(path).map_err(io(path))?;
        // The package must read: a release never ships one its own installs would refuse.
        Package::read(&bytes)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| ToolError::Usage(format!("{} has no file name", path.display())))?
            .to_owned();
        entries.push(FileEntry { target: target.clone(), name, size: bytes.len() as u64, sha256: sha256(&bytes) });
    }
    let channel = if version.is_pre() { Channel::Preview } else { Channel::Stable };
    Ok(Manifest { version, channel, apply, files: entries })
}

/// The manifest's text and its signature file, signed with `pkcs8` and verified with its public half.
pub fn sign_release(manifest: &Manifest, pkcs8: &[u8]) -> Result<(String, String), ToolError> {
    let text = manifest.to_text();
    let sig = sign(text.as_bytes(), pkcs8)?;
    let public = public_key(pkcs8)?;
    let back = verify_with(text.as_bytes(), sig.as_bytes(), &[public])?;
    if &back != manifest {
        return Err(ToolError::Usage("the signed manifest does not read back as itself".into()));
    }
    Ok((text, sig))
}

/// The line a public key takes in `swapdesk-update/src/keys.rs`.
pub fn key_line(public: &[u8; 32], label: &str) -> String {
    let bytes: Vec<String> = public.iter().map(|b| format!("0x{b:02x}")).collect();
    format!("    // {label}\n    [{}],", bytes.join(", "))
}
