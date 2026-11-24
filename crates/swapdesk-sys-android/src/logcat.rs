//! The system log as a `TraceSink` (C2.3): each record is one line of logcat under the tag `Swapdesk`, so a
//! development build can be followed with `adb logcat -s Swapdesk`. Never content: callers follow the never-logged
//! list of C2.3, as for every sink. The app also keeps its trace in a file of its private folder.

use std::ffi::{CString, c_char, c_int};

use swapdesk_platform::{Level, TraceSink};

// `android/log.h`: the priorities of `__android_log_write`.
const DEBUG: c_int = 3;
const INFO: c_int = 4;
const WARN: c_int = 5;
const ERROR: c_int = 6;

const TAG: &std::ffi::CStr = c"Swapdesk";

#[link(name = "log")]
unsafe extern "C" {
    /// `int __android_log_write(int prio, const char* tag, const char* text)`.
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

/// Writes one line to the system log under the tag `Swapdesk`. A NUL inside `line` ends it there.
pub fn write(level: Level, line: &str) {
    let prio = match level {
        Level::Error => ERROR,
        Level::Warn => WARN,
        Level::Info => INFO,
        Level::Debug => DEBUG,
    };
    let text = CString::new(line.split('\0').next().unwrap_or_default()).unwrap_or_default();
    // SAFETY: both pointers are NUL-terminated strings that live for the call, which copies them.
    unsafe { __android_log_write(prio, TAG.as_ptr(), text.as_ptr()) };
}

/// The sink: records above `max_level` are dropped.
pub struct Logcat {
    pub max_level: Level,
}

impl TraceSink for Logcat {
    fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>) {
        if level <= self.max_level {
            write(level, &format!("{target} {what}"));
        }
    }
}
