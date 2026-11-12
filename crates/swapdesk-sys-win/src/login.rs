//! "Open at login" (F-SYS-01, D32's Windows counterpart): the per-user Run key value `Swapdesk` follows the
//! setting. `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` holds the quoted path of the installed
//! `Swapdesk.exe`; off removes the value. Only the copy in `%ProgramFiles%\Swapdesk` is ever written (a development
//! build must not leave a Run entry that points into a build folder).
//!
//! The decisions are pure ([`run_command_line`], [`is_installed_exe`], [`reconcile`] over a [`RunStore`]); the real
//! store is [`HkcuRun`]. Tests use a fake store: they never touch the registry.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW,
};

use crate::sys::{Result, invalid, os_error, wide};

/// The key under HKCU that Windows reads at sign-in.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The value's name.
pub const RUN_VALUE: &str = "Swapdesk";

/// The Run value for `exe`: the path in quotes (a path with spaces is one program).
pub fn run_command_line(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// True when `exe` is `<program files>\Swapdesk\Swapdesk.exe` (the installed copy; case-insensitive, as Windows is).
pub fn is_installed_exe(exe: &Path, program_files: &Path) -> bool {
    let want = program_files.join("Swapdesk").join("Swapdesk.exe");
    want.to_string_lossy().eq_ignore_ascii_case(&exe.to_string_lossy())
}

/// Where the Run value lives: a fake in tests, [`HkcuRun`] for real.
pub trait RunStore {
    fn get(&self) -> Result<Option<String>>;
    fn set(&mut self, command: &str) -> Result<()>;
    /// Removes the value; an absent value is fine.
    fn remove(&mut self) -> Result<()>;
}

/// What [`reconcile`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Written,
    Removed,
    Unchanged,
}

/// Makes the store say `wanted`: on writes the command when it is missing or different (a moved install heals), off
/// removes the value when present.
pub fn reconcile(store: &mut dyn RunStore, command: &str, wanted: bool) -> Result<Applied> {
    let current = store.get()?;
    if wanted {
        if current.as_deref() == Some(command) {
            return Ok(Applied::Unchanged);
        }
        store.set(command)?;
        Ok(Applied::Written)
    } else if current.is_some() {
        store.remove()?;
        Ok(Applied::Removed)
    } else {
        Ok(Applied::Unchanged)
    }
}

/// The real store: the value `Swapdesk` of the current user's Run key.
pub struct HkcuRun;

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegCreateKeyExW and is closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

fn open_key() -> Result<Key> {
    let sub = wide(OsStr::new(RUN_KEY))?;
    let mut key: HKEY = null_mut();
    // SAFETY: `sub` is NUL-terminated and outlives the call; `key` is a valid out pointer.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            null(),
            &mut key,
            null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(os_error("RegCreateKeyExW", rc));
    }
    Ok(Key(key))
}

impl RunStore for HkcuRun {
    fn get(&self) -> Result<Option<String>> {
        let key = open_key()?;
        let name = wide(OsStr::new(RUN_VALUE))?;
        let mut kind = 0u32;
        let mut bytes = 0u32;
        // SAFETY: the first call only asks for the size; every pointer is valid for the call.
        let rc = unsafe { RegQueryValueExW(key.0, name.as_ptr(), null(), &mut kind, null_mut(), &mut bytes) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS {
            return Err(os_error("RegQueryValueExW", rc));
        }
        if kind != REG_SZ {
            // Not ours: report it as different text so a wanted write replaces it.
            return Ok(Some(String::new()));
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let mut size = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
        // SAFETY: `buf` holds `size` bytes and stays alive for the call.
        let rc = unsafe {
            RegQueryValueExW(key.0, name.as_ptr(), null(), &mut kind, buf.as_mut_ptr().cast::<u8>(), &mut size)
        };
        if rc != ERROR_SUCCESS {
            return Err(os_error("RegQueryValueExW", rc));
        }
        let units = (size as usize / 2).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..units]);
        Ok(Some(text.trim_end_matches('\0').to_owned()))
    }

    fn set(&mut self, command: &str) -> Result<()> {
        let key = open_key()?;
        let name = wide(OsStr::new(RUN_VALUE))?;
        let data = wide(OsStr::new(command))?;
        let bytes = u32::try_from(data.len() * 2).map_err(|_| invalid("run value", "command too long"))?;
        // SAFETY: `data` is NUL-terminated UTF-16 of `bytes` bytes, alive for the call.
        let rc = unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast::<u8>(), bytes) };
        if rc != ERROR_SUCCESS {
            return Err(os_error("RegSetValueExW", rc));
        }
        Ok(())
    }

    fn remove(&mut self) -> Result<()> {
        let key = open_key()?;
        let name = wide(OsStr::new(RUN_VALUE))?;
        // SAFETY: `name` is NUL-terminated and alive for the call.
        let rc = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(os_error("RegDeleteValueW", rc));
        }
        Ok(())
    }
}

/// `%ProgramW6432%` (the 64-bit Program Files, as `install.ps1` uses), else `%ProgramFiles%`.
fn program_files() -> Option<PathBuf> {
    std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")).map(PathBuf::from)
}

/// Applies the setting to the real Run key for the running program. Returns what was done, or `None` when this
/// program is not the installed copy (a development build: the registry is left alone).
pub fn apply_open_at_login(wanted: bool) -> Result<Option<Applied>> {
    let exe = std::env::current_exe().map_err(|e| invalid("current_exe", e.to_string()))?;
    let Some(pf) = program_files() else { return Ok(None) };
    if !is_installed_exe(&exe, &pf) {
        return Ok(None);
    }
    reconcile(&mut HkcuRun, &run_command_line(&exe), wanted).map(Some)
}

/// Whether the Run value is present now (any value).
pub fn open_at_login_is_set() -> bool {
    HkcuRun.get().ok().flatten().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        value: Option<String>,
        writes: u32,
    }

    impl RunStore for Fake {
        fn get(&self) -> Result<Option<String>> {
            Ok(self.value.clone())
        }
        fn set(&mut self, command: &str) -> Result<()> {
            self.value = Some(command.to_owned());
            self.writes += 1;
            Ok(())
        }
        fn remove(&mut self) -> Result<()> {
            self.value = None;
            Ok(())
        }
    }

    const EXE: &str = r"C:\Program Files\Swapdesk\Swapdesk.exe";

    #[test]
    fn the_run_value_is_the_quoted_path() {
        assert_eq!(run_command_line(Path::new(EXE)), "\"C:\\Program Files\\Swapdesk\\Swapdesk.exe\"");
    }

    #[test]
    fn only_the_installed_copy_may_write_the_run_key() {
        let pf = Path::new(r"C:\Program Files");
        assert!(is_installed_exe(Path::new(EXE), pf));
        assert!(is_installed_exe(Path::new(r"c:\program files\swapdesk\SWAPDESK.EXE"), pf));
        assert!(!is_installed_exe(Path::new(r"C:\sdt\b12\debug\Swapdesk.exe"), pf));
        assert!(!is_installed_exe(Path::new(r"C:\Program Files\Swapdesk-other\Swapdesk.exe"), pf));
        assert!(!is_installed_exe(Path::new(r"C:\Program Files\Swapdesk\swapdesk-helper.exe"), pf));
    }

    #[test]
    fn on_writes_once_off_removes_and_both_are_idempotent() {
        let cmd = run_command_line(Path::new(EXE));
        let mut s = Fake::default();
        assert_eq!(reconcile(&mut s, &cmd, false).unwrap(), Applied::Unchanged);
        assert_eq!(reconcile(&mut s, &cmd, true).unwrap(), Applied::Written);
        assert_eq!(reconcile(&mut s, &cmd, true).unwrap(), Applied::Unchanged);
        assert_eq!(s.writes, 1);
        assert_eq!(s.value.as_deref(), Some(cmd.as_str()));
        assert_eq!(reconcile(&mut s, &cmd, false).unwrap(), Applied::Removed);
        assert_eq!(s.value, None);
        assert_eq!(reconcile(&mut s, &cmd, false).unwrap(), Applied::Unchanged);
    }

    #[test]
    fn a_stale_path_is_replaced_when_wanted() {
        let mut s = Fake { value: Some("\"D:\\old\\Swapdesk.exe\"".into()), writes: 0 };
        let cmd = run_command_line(Path::new(EXE));
        assert_eq!(reconcile(&mut s, &cmd, true).unwrap(), Applied::Written);
        assert_eq!(s.value.as_deref(), Some(cmd.as_str()));
    }
}
