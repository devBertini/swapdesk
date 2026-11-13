//! The helper server's volume side (FILES-UI.md 8.1): the table that gives each published volume an `id` it keeps for
//! the life of the helper, the 1.1 paths (`:<id>/...`), the rule that a change never passes through a link, and the
//! protected folders. Paths here are real paths; the server turns virtual paths into them with this.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use swapdesk_ipc::{MAX_FS_PATH, MAX_PLACES, MAX_VOLUME_LABEL, MAX_VOLUMES, PlaceView, Text, VolumeView, VolumesView};
use swapdesk_proto::{ErrorCode, PlaceKind};

use crate::fsops::within;
use crate::name::validate_name;
use crate::system::{SysVolume, SystemDisk};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Default)]
struct Table {
    next: u32,
    by_identity: HashMap<String, u32>,
    by_id: HashMap<u32, String>,
    /// The ids published when the change poll last looked (`None` before its first look).
    reported: Option<Vec<u32>>,
}

impl Table {
    /// The id of `identity`: the one it had before, or a new one that no volume of this helper's life had.
    fn assign(&mut self, identity: &str) -> u32 {
        if let Some(id) = self.by_identity.get(identity) {
            return *id;
        }
        self.next = self.next.saturating_add(1);
        let id = self.next;
        self.by_identity.insert(identity.to_string(), id);
        self.by_id.insert(id, identity.to_string());
        id
    }
}

/// The volumes a server publishes, over the system's part.
pub(crate) struct Volumes {
    system: Arc<dyn SystemDisk>,
    table: Mutex<Table>,
}

/// The first component of a 1.1 path, `:<id>`: the id, `InvalidName` when it is not written as a plain decimal.
pub(crate) fn volume_id(first: &str) -> Result<u32, ErrorCode> {
    let digits = first.strip_prefix(':').ok_or(ErrorCode::InvalidName)?;
    let plain = !digits.is_empty()
        && digits.len() <= 9
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if !plain {
        return Err(ErrorCode::InvalidName);
    }
    digits.parse().map_err(|_| ErrorCode::InvalidName)
}

/// The components of `p` below `base` (names compared without letter case), as strings; `None` when `p` is not inside
/// `base` or has a name that is not text.
pub(crate) fn rel_under(base: &Path, p: &Path) -> Option<Vec<String>> {
    if !within(p, base) {
        return None;
    }
    let skip = base.components().filter(|c| !matches!(c, std::path::Component::CurDir)).count();
    p.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .skip(skip)
        .map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect()
}

/// No component of `rest` below `base` is a symbolic link or a junction (the entry itself is looked at, never its
/// target). A component that is not there ends the walk: the change that follows fails by itself.
pub(crate) fn walk_no_link(base: &Path, rest: &[String]) -> Result<(), ErrorCode> {
    let mut cur = base.to_path_buf();
    for part in rest {
        cur.push(part);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(ErrorCode::InvalidName),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(ErrorCode::IoFailed),
        }
    }
    Ok(())
}

/// At most `max` bytes of `s`, cut at a character boundary.
fn cut(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

impl Volumes {
    /// The set published at this moment is the one a later [`Self::changed`] compares with.
    pub(crate) fn new(system: Arc<dyn SystemDisk>) -> Self {
        let me = Self { system, table: Mutex::new(Table::default()) };
        me.changed();
        me
    }

    pub(crate) fn system(&self) -> &Arc<dyn SystemDisk> {
        &self.system
    }

    /// The volumes published now, each with its id. Two volumes that report one identity count once.
    pub(crate) fn snapshot(&self) -> Vec<(u32, SysVolume)> {
        let mut seen = Vec::<String>::new();
        let mounted: Vec<SysVolume> = self
            .system
            .volumes()
            .into_iter()
            .filter(|v| {
                let fresh = !seen.contains(&v.identity);
                seen.push(v.identity.clone());
                fresh
            })
            .collect();
        let mut table = lock(&self.table);
        mounted
            .into_iter()
            .map(|v| {
                let id = table.assign(&v.identity);
                (id, v)
            })
            .collect()
    }

    /// The volume `id` names, if it is mounted now as the volume it was given to. An id never handed out, and one whose
    /// volume is gone, are `VolumeGone`.
    pub(crate) fn find(&self, id: u32) -> Result<SysVolume, ErrorCode> {
        let snap = self.snapshot();
        snap.into_iter().find(|(i, _)| *i == id).map(|(_, v)| v).ok_or(ErrorCode::VolumeGone)
    }

    /// True when the published set differs from the one the last call (or the creation) saw.
    pub(crate) fn changed(&self) -> bool {
        let mut ids: Vec<u32> = self.snapshot().into_iter().map(|(id, _)| id).collect();
        ids.sort_unstable();
        let mut table = lock(&self.table);
        let was = table.reported.replace(ids.clone());
        was.is_some_and(|w| w != ids)
    }

    /// The published volume whose root holds `p` (the deepest root when several do).
    pub(crate) fn containing<'a>(snap: &'a [(u32, SysVolume)], p: &Path) -> Option<&'a (u32, SysVolume)> {
        snap.iter().filter(|(_, v)| within(p, &v.root)).max_by_key(|(_, v)| v.root.components().count())
    }

    /// Whether a change to `target` (the entry itself, or what would be made there) is refused by the protected
    /// folders: anything inside one, the folder itself, and a folder that holds one (moving it would move that). The
    /// root of a volume protects only the entries directly in it.
    pub(crate) fn protected_hit(&self, target: &Path) -> bool {
        self.system.protected().iter().any(|p| {
            if p.parent().is_none() {
                within(target, p) && target.parent().is_none_or(|parent| within(p, parent) && within(parent, p))
            } else {
                within(target, p) || within(p, target)
            }
        })
    }

    /// The answer to `FsCall::Volumes`: the home folder (the empty path), the places as 1.1 paths and the volumes.
    pub(crate) fn view(&self, home: &Path, receive: Option<&str>) -> VolumesView {
        let snap = self.snapshot();
        let volumes = snap
            .iter()
            .take(MAX_VOLUMES)
            .map(|(id, v)| VolumeView {
                id: *id,
                kind: v.kind,
                label: Text(cut(&v.label, MAX_VOLUME_LABEL)),
                letter: v.letter,
                total_bytes: v.total_bytes,
                free_bytes: v.free_bytes,
                has_bin: v.has_bin,
            })
            .collect();
        let mut places: Vec<PlaceView> = self
            .system
            .places()
            .into_iter()
            .filter_map(|(kind, real)| Some(PlaceView { kind, path: Text(virtual_of(&snap, home, &real)?) }))
            .collect();
        if let Some(r) = receive.filter(|r| home.join(r).is_dir()) {
            places.push(PlaceView { kind: PlaceKind::Received, path: Text(r.to_string()) });
        }
        places.truncate(MAX_PLACES);
        VolumesView { home: Text(String::new()), places, volumes }
    }
}

/// The 1.1 path of a real path: below the home folder as it is, else below the volume that holds it as `:<id>/...`.
/// `None` when no published volume holds it or a name on the way cannot be named in a path.
fn virtual_of(snap: &[(u32, SysVolume)], home: &Path, real: &Path) -> Option<String> {
    let (prefix, rest) = match rel_under(home, real) {
        Some(rest) => (String::new(), rest),
        None => {
            let (id, v) = Volumes::containing(snap, real)?;
            (format!(":{id}"), rel_under(&v.root, real)?)
        }
    };
    if rest.iter().any(|c| validate_name(c).is_err()) {
        return None;
    }
    let joined = if prefix.is_empty() {
        rest.join("/")
    } else {
        std::iter::once(prefix).chain(rest).collect::<Vec<_>>().join("/")
    };
    (joined.len() <= MAX_FS_PATH).then_some(joined)
}
