//! Counters of other processes by PID (CPU time, private working set), the machine's processor count, and the end of
//! a process the measurement started.

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};

struct Handle(HANDLE);

impl Handle {
    fn open(pid: u32, terminate: bool) -> Option<Handle> {
        let access = if terminate {
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE
        } else {
            PROCESS_QUERY_LIMITED_INFORMATION
        };
        // SAFETY: opens a handle that `drop` closes.
        unsafe { OpenProcess(access, false, pid) }.ok().map(Handle)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn secs(f: FILETIME) -> f64 {
    (u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)) as f64 * 100e-9
}

/// Kernel plus user CPU seconds of process `pid` since it started; `None` when it is gone.
pub fn cpu_seconds(pid: u32) -> Option<f64> {
    let h = Handle::open(pid, false)?;
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: fills four FILETIMEs for a live handle.
    unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u) }.ok()?;
    Some(secs(k) + secs(u))
}

/// The private working set of process `pid` in MB (Task Manager's "Memory" column).
pub fn private_working_set_mb(pid: u32) -> Option<f64> {
    let h = Handle::open(pid, false)?;
    let mut c =
        PROCESS_MEMORY_COUNTERS_EX2 { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32, ..Default::default() };
    // SAFETY: fills the counters; `cb` tells the size of the extended struct.
    unsafe { GetProcessMemoryInfo(h.0, (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) }.ok()?;
    Some(c.PrivateWorkingSetSize as f64 / (1024.0 * 1024.0))
}

/// The file name of the image of process `pid`, lower case.
pub fn image_name(pid: u32) -> Option<String> {
    let h = Handle::open(pid, false)?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` wide characters; the call writes at most that many and sets `len`.
    unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len) }
        .ok()?;
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit(['\\', '/']).next().map(str::to_lowercase)
}

/// Ends process `pid` when its image is `image` (so a PID reused by another program is never touched).
pub fn kill_if_image(pid: u32, image: &str) -> bool {
    if image_name(pid).as_deref() != Some(image) {
        return false;
    }
    let Some(h) = Handle::open(pid, true) else { return false };
    // SAFETY: ends a process this measurement started, through a handle opened for it.
    unsafe { TerminateProcess(h.0, 1) }.is_ok()
}

/// Logical processors of this machine.
pub fn logical_processors() -> u32 {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: fills a plain struct.
    unsafe { GetSystemInfo(&mut info) };
    info.dwNumberOfProcessors.max(1)
}
