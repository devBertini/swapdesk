//! The Mac part of the file server's `SystemDisk` (FILES-UI.md 7.4, 8.1): the volumes, folders and Trash that
//! `swapdesk_sys_mac::disk` reads, in the shapes `swapdesk_files` asks for. Safe code only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use swapdesk_files::{SysVolume, SystemDisk};
use swapdesk_proto::{ErrorCode, PlaceKind};
use swapdesk_sys_mac::disk::{self, TrashError};

pub struct MacSystemDisk;

pub fn system_disk() -> Arc<dyn SystemDisk> {
    Arc::new(MacSystemDisk)
}

impl SystemDisk for MacSystemDisk {
    fn volumes(&self) -> Vec<SysVolume> {
        disk::volumes()
            .into_iter()
            .map(|v| SysVolume {
                identity: v.identity,
                root: v.root,
                kind: v.kind,
                label: v.label,
                letter: v.letter,
                total_bytes: v.total_bytes,
                free_bytes: v.free_bytes,
                has_bin: v.has_bin,
            })
            .collect()
    }

    fn places(&self) -> Vec<(PlaceKind, PathBuf)> {
        disk::places()
    }

    fn trash(&self, path: &Path) -> Result<(), ErrorCode> {
        disk::trash(&[path.to_path_buf()]).map_err(|e| match e {
            TrashError::NoBin => ErrorCode::NoBin,
            TrashError::NotFound => ErrorCode::NotFound,
            TrashError::NotPermitted => ErrorCode::NotPermitted,
            TrashError::Failed => ErrorCode::IoFailed,
        })
    }

    fn protected(&self) -> Vec<PathBuf> {
        disk::protected()
    }
}
