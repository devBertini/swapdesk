//! What the operating system gives the user helper for the explorer's wider reach (FILES-UI.md 7.4 and 8.1): the
//! mounted volumes with an identity of their own, the well-known folders, and the bin. Each system implements it in
//! its own crate (Windows in `swapdesk-sys-win`, the Mac in the agent's crate); the helper hands it to the file server,
//! which keeps the volume ids and every path rule itself.

use std::path::{Path, PathBuf};

use swapdesk_proto::{ErrorCode, PlaceKind, VolumeKind};

/// A volume mounted now, as the system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysVolume {
    /// What makes this volume itself across a removal and a return, and tells it from another volume mounted at the
    /// same place (Windows: the volume GUID path or the serial number; Mac: the volume UUID). Never shown.
    pub identity: String,
    /// The folder where its files start (`D:\`, `/`, `/Volumes/Backup`).
    pub root: PathBuf,
    pub kind: VolumeKind,
    /// The name the system shows; may be empty.
    pub label: String,
    /// The Windows drive letter (`b'A'..=b'Z'`).
    pub letter: Option<u8>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Whether a file of this volume can go to a bin; `false` makes `trash` refuse with `NoBin` before anything.
    pub has_bin: bool,
}

/// The system's part of the file server on the host side.
pub trait SystemDisk: Send + Sync {
    /// The volumes to publish now (network volumes left out unless the system's part decides otherwise; never the
    /// hidden system volumes of the Mac).
    fn volumes(&self) -> Vec<SysVolume>;

    /// The well-known folders of the user (Desktop, Documents, Downloads...), as real paths; a folder missing on this
    /// machine is left out.
    fn places(&self) -> Vec<(PlaceKind, PathBuf)>;

    /// Sends `path` (a file or a folder, never followed through a link) to its volume's bin. `NoBin` before touching
    /// anything when that volume has none; nothing is ever deleted for good.
    fn trash(&self, path: &Path) -> Result<(), ErrorCode>;

    /// The folders where the server refuses every change (FILES-UI.md 8.1 "Protected locations"): the system's own
    /// folders and the root of the system volume. Reading stays allowed.
    fn protected(&self) -> Vec<PathBuf>;
}
