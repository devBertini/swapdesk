//! A thread that sleeps until something happens in a folder: `FindFirstChangeNotificationW` and a wait on its handle,
//! instead of looking for the file every few hundred milliseconds. Development mode uses it for the `stop` file.
//!
//! Source read on 2026-11-08: `FindFirstChangeNotificationW`
//! <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstchangenotificationw>: the handle is
//! signalled when a change of the filtered kind happens in the folder, and `FindNextChangeNotification` arms it again;
//! a change between the wait returning and the re-arm is not lost, because the caller looks at the folder after the
//! re-arm.

use std::ffi::OsStr;
use std::path::Path;

use swapdesk_platform::Result;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_NOTIFY_CHANGE_FILE_NAME, FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

use crate::sys::{invalid, last_error, wide};

/// Wakes a thread when a file is created, deleted or renamed in a folder (not its subfolders).
pub struct DirWatch(HANDLE);

// SAFETY: a change-notification handle is a kernel handle: any thread may wait on it, re-arm it or close it.
unsafe impl Send for DirWatch {}
// SAFETY: as above; `wait` takes `&self` and the kernel serializes the calls.
unsafe impl Sync for DirWatch {}

impl DirWatch {
    /// Starts watching `dir`, which must exist.
    pub fn new(dir: &Path) -> Result<Self> {
        let path = wide(OsStr::new(dir))?;
        // SAFETY: `path` is a NUL-terminated wide string that lives through the call; the filter is a plain flag.
        let handle = unsafe { FindFirstChangeNotificationW(path.as_ptr(), 0, FILE_NOTIFY_CHANGE_FILE_NAME) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(last_error("FindFirstChangeNotificationW"));
        }
        Ok(Self(handle))
    }

    /// Blocks until something changed in the folder since the last call (or since `new`), then arms the watch again.
    /// The caller looks at the folder afterwards: a notice says that something changed, not what.
    pub fn wait(&self) -> Result<()> {
        // SAFETY: the handle is valid until `drop`.
        if unsafe { WaitForSingleObject(self.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(last_error("WaitForSingleObject"));
        }
        // SAFETY: the same valid handle, signalled by the wait above.
        if unsafe { FindNextChangeNotification(self.0) } == 0 {
            return Err(invalid("FindNextChangeNotification", "the folder watch could not be armed again"));
        }
        Ok(())
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        // SAFETY: the handle came from `FindFirstChangeNotificationW` and is closed once.
        unsafe { FindCloseChangeNotification(self.0) };
    }
}
