//! The folder rules of C5.2: `%ProgramData%\Swapdesk` is created by the service itself, owned by SYSTEM, with a
//! protected DACL (SYSTEM and Administrators full, nobody else). A pre-existing folder with another owner, or a
//! reparse point, is moved aside and replaced. Reparse points are never followed.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use swapdesk_platform::{Level, PlatformError, TraceSink};

use crate::sys::{self, FolderSecurity, SecurityDescriptor, invalid};

/// Who owns the folder and who may use it. [`FolderSpec::production`] is what the service applies; a test passes
/// the current user in the same places because only SYSTEM can make SYSTEM the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSpec {
    /// SID (or SDDL alias) that owns a folder this crate creates.
    pub owner: String,
    /// SIDs given full control, inherited by every file and folder below.
    pub full_control: Vec<String>,
    /// Owners that a pre-existing folder may have and be kept.
    pub trusted_owners: Vec<String>,
}

impl FolderSpec {
    /// Owner SYSTEM; SYSTEM and Administrators full; a folder owned by either is trusted.
    pub fn production() -> Self {
        Self {
            owner: "SY".into(),
            full_control: vec!["SY".into(), "BA".into()],
            trusted_owners: vec!["SY".into(), "BA".into()],
        }
    }

    /// The descriptor applied: `O:<owner>D:P(A;OICI;FA;;;<sid>)...`. `P` stops inheritance from `%ProgramData%`.
    pub fn sddl(&self) -> String {
        let aces: String = self.full_control.iter().map(|s| format!("(A;OICI;FA;;;{s})")).collect();
        format!("O:{}D:P{aces}", self.owner)
    }

    /// The DACL part of [`Self::sddl`], with SID aliases written out, as `read_security` will report it once the
    /// system adds nothing else (the `AI` flag it may add is ignored by [`dacl_matches`]).
    pub fn expected_aces(&self) -> Result<Vec<String>, PlatformError> {
        self.full_control.iter().map(|s| Ok(format!("(A;OICI;FA;;;{})", sys::normalize_sid(s)?))).collect()
    }
}

/// What `ensure_folder` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReport {
    pub path: PathBuf,
    /// A fresh folder was created (also true after moving a bad one aside).
    pub created: bool,
    /// Where an untrusted pre-existing folder went (`<name>.untrusted-<unix ms>`).
    pub moved_aside: Option<PathBuf>,
}

fn unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// True when `read` (as returned by [`sys::read_security`]) holds exactly the protected DACL of `spec`: the `P`
/// flag, and the spec's access-allowed entries in order, with SID aliases ("SY") written out before comparing.
pub fn dacl_matches(spec: &FolderSpec, read: &FolderSecurity) -> Result<bool, PlatformError> {
    let Some(rest) = read.dacl_sddl.strip_prefix("D:") else { return Ok(false) };
    let flags: String = rest.chars().take_while(|c| c.is_ascii_uppercase()).collect();
    let mut got = Vec::new();
    for ace in rest[flags.len()..].split_inclusive(')') {
        let Some(sid) = ace.strip_prefix("(A;OICI;FA;;;").and_then(|a| a.strip_suffix(')')) else { return Ok(false) };
        got.push(format!("(A;OICI;FA;;;{})", sys::normalize_sid(sid)?));
    }
    Ok(flags.starts_with('P') && got == spec.expected_aces()?)
}

enum Verdict {
    /// Owner and DACL are the spec's; the handle that proved it is kept to fix the DACL through.
    Trusted(sys::Handle),
    Untrusted(String),
    /// The name vanished between the failed create and the look.
    Gone,
}

/// ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND.
fn is_gone(e: &PlatformError) -> bool {
    matches!(e.os_code, Some(2 | 3))
}

/// ERROR_ACCESS_DENIED.
fn is_denied(e: &PlatformError) -> bool {
    e.os_code == Some(5)
}

/// Looks at an existing name through one handle opened without following it (finding 6). A folder is trusted only
/// when it is a real directory, its owner is trusted and its DACL is exactly the spec's (finding 1); one that
/// cannot be judged because access is denied is untrusted too (finding 5).
fn judge(spec: &FolderSpec, path: &Path) -> Result<Verdict, PlatformError> {
    let h = match sys::open_dir_for_judging(path) {
        Ok(h) => h,
        Err(e) if is_gone(&e) => return Ok(Verdict::Gone),
        Err(e) if is_denied(&e) => return Ok(Verdict::Untrusted("it could not be judged (access denied)".into())),
        Err(e) => return Err(e),
    };
    let (is_dir, is_reparse) = sys::kind_of(&h)?;
    if is_reparse {
        return Ok(Verdict::Untrusted("it is a reparse point".into()));
    }
    if !is_dir {
        return Ok(Verdict::Untrusted("it is not a directory".into()));
    }
    let read = match sys::security_of(&h) {
        Ok(r) => r,
        Err(e) if is_denied(&e) => return Ok(Verdict::Untrusted("it could not be judged (access denied)".into())),
        Err(e) => return Err(e),
    };
    let mut owner_trusted = false;
    for trusted in &spec.trusted_owners {
        owner_trusted |= sys::normalize_sid(trusted)? == read.owner;
    }
    if !owner_trusted {
        return Ok(Verdict::Untrusted("its owner is not trusted".into()));
    }
    if !dacl_matches(spec, &read)? {
        return Ok(Verdict::Untrusted(format!("its DACL is not the protected DACL of the spec ({})", read.dacl_sddl)));
    }
    Ok(Verdict::Trusted(h))
}

/// Whether `path` still names the object `h` was opened on (a swap of the name after the judgment shows here).
fn still_names(path: &Path, h: &sys::Handle) -> Result<bool, PlatformError> {
    match sys::open_attributes_only(path) {
        Ok(again) => Ok(sys::file_id(&again)? == sys::file_id(h)?),
        Err(e) if is_gone(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

const RENAME_TRIES: u32 = 6;
const RENAME_PAUSE: Duration = Duration::from_millis(100);

/// Moves an untrusted name aside, trying again for a moment (an antivirus scan or an indexer may hold it). Ok(false)
/// means the name was already gone. A rename that keeps failing is an alert and an error, never a deletion.
fn move_aside(path: &Path, aside: &Path, trace: Option<&dyn TraceSink>) -> Result<bool, PlatformError> {
    let mut last = None;
    for attempt in 0..RENAME_TRIES {
        if attempt > 0 {
            std::thread::sleep(RENAME_PAUSE);
        }
        match sys::rename(path, aside, false) {
            Ok(()) => return Ok(true),
            Err(e) if is_gone(&e) => return Ok(false),
            Err(e) => last = Some(e),
        }
    }
    let e = last.unwrap_or_else(|| invalid("ensure_folder", "rename never ran"));
    let detail = format!("the untrusted folder {} could not be moved aside: {}", path.display(), e.detail);
    if let Some(t) = trace {
        t.log(Level::Warn, "secret", format_args!("alert: {detail}; the service cannot start safely"));
    }
    Err(PlatformError { kind: e.kind, op: "ensure_folder", os_code: e.os_code, detail })
}

/// Creates `parent/name` with the spec's owner and protected DACL, or keeps a trusted existing one, or moves an
/// untrusted one aside (`<name>.untrusted-<unix ms>`, logged as a warning) and creates a fresh one. Trusted means
/// the owner and the whole DACL are the spec's; anything else, including a folder that cannot be judged, is moved.
pub fn ensure_folder(
    spec: &FolderSpec,
    parent: &Path,
    name: &str,
    trace: Option<&dyn TraceSink>,
) -> Result<FolderReport, PlatformError> {
    ensure_folder_with(spec, parent, name, trace, &mut || {})
}

/// [`ensure_folder`] with a hook run after a folder was judged trusted and before its name is checked again: the
/// place a test swaps the name.
pub(crate) fn ensure_folder_with(
    spec: &FolderSpec,
    parent: &Path,
    name: &str,
    trace: Option<&dyn TraceSink>,
    after_judged: &mut dyn FnMut(),
) -> Result<FolderReport, PlatformError> {
    if name.is_empty() || name.contains(['\\', '/', ':']) || name == "." || name == ".." {
        return Err(invalid("ensure_folder", "the folder name is not one path component"));
    }
    let path = parent.join(name);
    let sd = SecurityDescriptor::from_sddl(&spec.sddl())?;
    let mut moved_aside = None;
    // Someone may re-create or swap the name while we work (a standard user can create folders in ProgramData),
    // so a few rounds are tried before giving up.
    for _ in 0..4 {
        if sys::create_dir_with(&path, &sd)? {
            return Ok(FolderReport { path, created: true, moved_aside });
        }
        match judge(spec, &path)? {
            Verdict::Trusted(h) => {
                // Through the same handle the folder was judged on, so a swap of the name cannot redirect it.
                sys::set_dacl(&h, sd.dacl()?)?;
                after_judged();
                if still_names(&path, &h)? {
                    return Ok(FolderReport { path, created: false, moved_aside });
                }
            }
            Verdict::Gone => {}
            Verdict::Untrusted(why) => {
                let aside = parent.join(format!("{name}.untrusted-{}", unix_ms()));
                if move_aside(&path, &aside, trace)? {
                    if let Some(t) = trace {
                        t.log(
                            Level::Warn,
                            "secret",
                            format_args!("{} was moved aside to {} because {why}", path.display(), aside.display()),
                        );
                    }
                    moved_aside = Some(aside);
                }
            }
        }
    }
    Err(invalid("ensure_folder", "the folder name kept changing under us"))
}
