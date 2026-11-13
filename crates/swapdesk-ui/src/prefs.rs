//! The per-user UI settings of C5.1 (`ui.bin`): the codec, the store trait and the file store.
//!
//! The record is `format: u16` (1) followed by one byte per field, little-endian like the other C5 records. A file
//! that is missing, of another format or damaged reads as the defaults and leaves one line in the trace; nothing here
//! ever panics or fails the start. The service never reads this file. `swapdesk-ui` stays OS-free: the shells say
//! where the file lives (`%APPDATA%\Swapdesk\ui.bin`, `~/Library/Application Support/Swapdesk/ui.bin`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::api::{KeyboardMode, Language, Quality, Settings, Theme};
use crate::shortcuts::ReleaseKey;

/// The `format` of the record this build writes.
pub const FORMAT: u16 = 2;
/// The format 1.0 wrote: the same fields, but an "off" in its swap byte could be the old default and not a choice.
const FORMAT_1: u16 = 1;

/// The starting value of "Trocar Ctrl e Cmd no Mac": on in the Mac app, off in the Windows one. The viewers apply
/// it only toward the other system (a Mac toward Windows, a Windows PC toward a Mac), so a Mac toward a Mac is
/// never swapped.
pub const fn default_swap_ctrl_cmd() -> bool {
    cfg!(target_os = "macos")
}
/// The file name inside the per-user folder.
pub const FILE_NAME: &str = "ui.bin";

/// Every per-user field. `Default` is what a first start (or an unreadable file) shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserPrefs {
    pub language: Language,
    pub theme: Theme,
    pub quality: Quality,
    pub show_stats: bool,
    pub keyboard_mode: KeyboardMode,
    pub swap_ctrl_cmd: bool,
    pub open_at_login: bool,
    pub release_key: ReleaseKey,
}

impl Default for UserPrefs {
    fn default() -> Self {
        Self {
            language: Language::PtBr,
            theme: Theme::System,
            quality: Quality::Auto,
            show_stats: false,
            keyboard_mode: KeyboardMode::Auto,
            swap_ctrl_cmd: default_swap_ctrl_cmd(),
            open_at_login: false,
            release_key: ReleaseKey::CtrlAlt,
        }
    }
}

/// Why a record was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefsError {
    /// A `format` this build does not know (a newer or older Swapdesk wrote it).
    UnknownFormat(u16),
    /// The wrong length or a tag out of range.
    Corrupt,
}

impl core::fmt::Display for PrefsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownFormat(v) => write!(f, "unknown format {v}"),
            Self::Corrupt => f.write_str("corrupt record"),
        }
    }
}

impl UserPrefs {
    /// The per-user fields of the settings the UI shows, plus the release key.
    pub fn from_settings(s: &Settings, release_key: ReleaseKey) -> Self {
        Self {
            language: s.language,
            theme: s.theme,
            quality: s.quality,
            show_stats: s.show_stats,
            keyboard_mode: s.keyboard_mode,
            swap_ctrl_cmd: s.swap_ctrl_cmd,
            open_at_login: s.open_at_login,
            release_key,
        }
    }

    /// `s` with the per-user fields replaced by these.
    pub fn overlay(&self, s: Settings) -> Settings {
        Settings {
            language: self.language,
            theme: self.theme,
            quality: self.quality,
            show_stats: self.show_stats,
            keyboard_mode: self.keyboard_mode,
            swap_ctrl_cmd: self.swap_ctrl_cmd,
            open_at_login: self.open_at_login,
            ..s
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(10);
        b.extend_from_slice(&FORMAT.to_le_bytes());
        b.push(match self.language {
            Language::PtBr => 0,
            Language::En => 1,
        });
        b.push(match self.theme {
            Theme::System => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        });
        b.push(match self.quality {
            Quality::Auto => 0,
            Quality::Text => 1,
            Quality::Motion => 2,
        });
        b.push(u8::from(self.show_stats));
        b.push(match self.keyboard_mode {
            KeyboardMode::Auto => 0,
            KeyboardMode::Physical => 1,
            KeyboardMode::Text => 2,
        });
        b.push(u8::from(self.swap_ctrl_cmd));
        b.push(u8::from(self.open_at_login));
        b.push(match self.release_key {
            ReleaseKey::CtrlAlt => 0,
            ReleaseKey::RightCtrl => 1,
            ReleaseKey::CtrlAltShift => 2,
        });
        b
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PrefsError> {
        Self::decode_with(bytes, default_swap_ctrl_cmd())
    }

    /// `decode` with the swap default to use for a record of format 1: its "off" may be the old default and not a
    /// choice, so it reads as `swap_default`; its "on" stays on. A record of the current format is read as stored.
    pub fn decode_with(bytes: &[u8], swap_default: bool) -> Result<Self, PrefsError> {
        let [f0, f1, rest @ ..] = bytes else { return Err(PrefsError::Corrupt) };
        let format = u16::from_le_bytes([*f0, *f1]);
        if format != FORMAT && format != FORMAT_1 {
            return Err(PrefsError::UnknownFormat(format));
        }
        let [language, theme, quality, stats, keyboard, swap, login, release] = rest else {
            return Err(PrefsError::Corrupt);
        };
        let flag = |v: u8| match v {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PrefsError::Corrupt),
        };
        Ok(Self {
            language: match language {
                0 => Language::PtBr,
                1 => Language::En,
                _ => return Err(PrefsError::Corrupt),
            },
            theme: match theme {
                0 => Theme::System,
                1 => Theme::Light,
                2 => Theme::Dark,
                _ => return Err(PrefsError::Corrupt),
            },
            quality: match quality {
                0 => Quality::Auto,
                1 => Quality::Text,
                2 => Quality::Motion,
                _ => return Err(PrefsError::Corrupt),
            },
            show_stats: flag(*stats)?,
            keyboard_mode: match keyboard {
                0 => KeyboardMode::Auto,
                1 => KeyboardMode::Physical,
                2 => KeyboardMode::Text,
                _ => return Err(PrefsError::Corrupt),
            },
            swap_ctrl_cmd: flag(*swap)? || (format == FORMAT_1 && swap_default),
            open_at_login: flag(*login)?,
            release_key: match release {
                0 => ReleaseKey::CtrlAlt,
                1 => ReleaseKey::RightCtrl,
                2 => ReleaseKey::CtrlAltShift,
                _ => return Err(PrefsError::Corrupt),
            },
        })
    }
}

/// Where the record lives. `read` returns `None` when there is no record yet.
pub trait PrefsStore: Send {
    fn read(&self) -> io::Result<Option<Vec<u8>>>;
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
}

/// The record in a file: written to a temporary file beside it, flushed, then renamed over it, so a reader (or a
/// crash) never sees half a record.
#[derive(Debug, Clone)]
pub struct FilePrefs {
    path: PathBuf,
}

impl FilePrefs {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `<folder>/ui.bin`.
    pub fn in_dir(dir: &Path) -> Self {
        Self::new(dir.join(FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn temp_path(&self) -> PathBuf {
        let mut name = self.path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".tmp");
        self.path.with_file_name(name)
    }
}

impl PrefsStore for FilePrefs {
    fn read(&self) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = self.temp_path();
        let result = (|| {
            let mut f = std::fs::File::create(&temp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

type NoteHook = Arc<dyn Fn(&str) + Send + Sync>;
static HOOK: Mutex<Option<NoteHook>> = Mutex::new(None);

/// Sends the lines this module leaves (a record that was not used, a write that failed) to the shell's trace. Never
/// content: the reason only.
pub fn set_note_hook(hook: impl Fn(&str) + Send + Sync + 'static) {
    if let Ok(mut h) = HOOK.lock() {
        *h = Some(Arc::new(hook));
    }
}

fn note(line: &str) {
    let hook = HOOK.lock().ok().and_then(|h| h.clone());
    if let Some(hook) = hook {
        hook(line);
    }
}

/// The stored record, or the defaults with a trace line when it is unreadable, of another format or damaged. A
/// missing file is a first start: defaults, no line.
pub fn load(store: &dyn PrefsStore) -> UserPrefs {
    load_or(store, UserPrefs::default())
}

/// The same with the defaults to use when no usable record exists (a first start keeps the system's language).
pub fn load_or(store: &dyn PrefsStore, defaults: UserPrefs) -> UserPrefs {
    match store.read() {
        Ok(None) => defaults,
        Ok(Some(bytes)) => UserPrefs::decode_with(&bytes, defaults.swap_ctrl_cmd).unwrap_or_else(|e| {
            note(&format!("ui.bin not used ({e}): defaults"));
            defaults
        }),
        Err(e) => {
            note(&format!("ui.bin not read ({}): defaults", e.kind()));
            defaults
        }
    }
}

/// Writes the record; a failure leaves a trace line and nothing else.
pub fn save(store: &dyn PrefsStore, prefs: &UserPrefs) {
    if let Err(e) = store.write(&prefs.encode()) {
        note(&format!("ui.bin not written ({})", e.kind()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("swapdesk-prefs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn sample() -> UserPrefs {
        UserPrefs {
            language: Language::En,
            theme: Theme::Dark,
            quality: Quality::Motion,
            show_stats: true,
            keyboard_mode: KeyboardMode::Text,
            swap_ctrl_cmd: true,
            open_at_login: true,
            release_key: ReleaseKey::CtrlAltShift,
        }
    }

    #[test]
    fn the_record_round_trips_and_starts_with_its_format() {
        let p = sample();
        let bytes = p.encode();
        assert_eq!(&bytes[..2], &2u16.to_le_bytes());
        assert_eq!(UserPrefs::decode(&bytes), Ok(p));
        assert_eq!(UserPrefs::decode(&UserPrefs::default().encode()), Ok(UserPrefs::default()));
    }

    #[test]
    fn another_format_or_a_damaged_record_is_refused() {
        let mut other = sample().encode();
        other[0] = 3;
        assert_eq!(UserPrefs::decode(&other), Err(PrefsError::UnknownFormat(3)));
        let good = sample().encode();
        assert_eq!(UserPrefs::decode(&good[..good.len() - 1]), Err(PrefsError::Corrupt), "truncated");
        let mut long = good.clone();
        long.push(0);
        assert_eq!(UserPrefs::decode(&long), Err(PrefsError::Corrupt), "padded");
        assert_eq!(UserPrefs::decode(&[]), Err(PrefsError::Corrupt));
        for at in 2..good.len() {
            let mut bad = good.clone();
            bad[at] = 9;
            assert_eq!(UserPrefs::decode(&bad), Err(PrefsError::Corrupt), "tag {at} out of range");
        }
    }

    fn format_1(swap: u8) -> Vec<u8> {
        // language En, theme Dark, quality Motion, stats on, keyboard Text, swap, login on, release CtrlAltShift.
        let mut b = 1u16.to_le_bytes().to_vec();
        b.extend_from_slice(&[1, 2, 2, 1, 2, swap, 1, 2]);
        b
    }

    #[test]
    fn the_swap_starts_on_only_where_the_system_says_so() {
        assert_eq!(UserPrefs::default().swap_ctrl_cmd, default_swap_ctrl_cmd());
        assert_eq!(default_swap_ctrl_cmd(), cfg!(target_os = "macos"));
        let first =
            load_or(&FilePrefs::in_dir(&temp_dir("first")), UserPrefs { swap_ctrl_cmd: true, ..Default::default() });
        assert!(first.swap_ctrl_cmd, "no file yet: the default given");
    }

    #[test]
    fn an_old_record_loads_and_its_off_reads_as_the_default_while_its_on_stays() {
        for default in [false, true] {
            let off = UserPrefs::decode_with(&format_1(0), default).unwrap();
            assert_eq!(off.swap_ctrl_cmd, default);
            let on = UserPrefs::decode_with(&format_1(1), default).unwrap();
            assert!(on.swap_ctrl_cmd);
            assert_eq!(
                (off.language, off.theme, off.quality, off.release_key),
                (Language::En, Theme::Dark, Quality::Motion, ReleaseKey::CtrlAltShift)
            );
            assert!(off.show_stats && off.open_at_login);
        }
        assert_eq!(UserPrefs::decode_with(&format_1(2), true), Err(PrefsError::Corrupt));
    }

    #[test]
    fn an_explicit_off_in_the_current_format_is_respected() {
        for default in [false, true] {
            let off = UserPrefs { swap_ctrl_cmd: false, ..sample() };
            assert_eq!(UserPrefs::decode_with(&off.encode(), default), Ok(off));
        }
    }

    #[test]
    fn an_old_file_is_read_through_the_store_with_the_given_default() {
        let dir = temp_dir("old");
        let store = FilePrefs::in_dir(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(store.path(), format_1(0)).unwrap();
        let loaded = load_or(&store, UserPrefs { swap_ctrl_cmd: true, ..Default::default() });
        assert!(loaded.swap_ctrl_cmd && loaded.show_stats, "the old record is used, its off read as the default");
        save(&store, &UserPrefs { swap_ctrl_cmd: false, ..loaded });
        let again = load_or(&store, UserPrefs { swap_ctrl_cmd: true, ..Default::default() });
        assert!(!again.swap_ctrl_cmd, "a choice made after the change stays off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_unknown_or_corrupt_file_reads_as_the_defaults() {
        let dir = temp_dir("load");
        let store = FilePrefs::in_dir(&dir);
        assert_eq!(load(&store), UserPrefs::default(), "no file yet");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(store.path(), [9, 9, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(load(&store), UserPrefs::default(), "unknown format");
        std::fs::write(store.path(), b"not a record at all").unwrap();
        assert_eq!(load(&store), UserPrefs::default(), "corrupt");
        // A directory where the file should be cannot be read: still the defaults.
        std::fs::remove_file(store.path()).unwrap();
        std::fs::create_dir(store.path()).unwrap();
        assert_eq!(load(&store), UserPrefs::default(), "unreadable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_write_goes_through_a_temp_file_and_a_rename() {
        let dir = temp_dir("write");
        let store = FilePrefs::in_dir(&dir.join("nested"));
        save(&store, &UserPrefs::default());
        assert_eq!(load(&store), UserPrefs::default());
        save(&store, &sample());
        assert_eq!(load(&store), sample(), "the second write replaces the first");
        let names: Vec<String> = std::fs::read_dir(dir.join("nested"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["ui.bin".to_string()], "no temp file is left behind");
        // A failed write (the target is a directory) reports the error and leaves no temp file.
        let blocked = FilePrefs::new(dir.join("nested"));
        assert!(blocked.write(&sample().encode()).is_err());
        assert!(!blocked.temp_path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
