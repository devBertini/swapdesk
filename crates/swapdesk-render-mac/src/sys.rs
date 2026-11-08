//! Error helpers and the monotonic clock of the macOS renderer.

use std::sync::OnceLock;

use swapdesk_platform::{ErrorKind, MonoMicros, PlatformError};

pub(crate) fn err(kind: ErrorKind, op: &'static str, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.into() }
}

/// A failed call that returned an `OSStatus` or `CVReturn`.
pub(crate) fn status_err(op: &'static str, status: i32) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: Some(i64::from(status)), detail: String::new() }
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

// SAFETY (declarations): `mach_continuous_time` and `mach_timebase_info` are in libSystem, which every macOS process
// links; the signatures are those of `<mach/mach_time.h>`.
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

fn timebase() -> (u64, u64) {
    static BASE: OnceLock<(u64, u64)> = OnceLock::new();
    *BASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: `info` is a valid, writable `mach_timebase_info_data_t`.
        let status = unsafe { mach_timebase_info(&raw mut info) };
        if status == 0 && info.numer != 0 && info.denom != 0 {
            (u64::from(info.numer), u64::from(info.denom))
        } else {
            (1, 1)
        }
    })
}

/// Microseconds of `mach_continuous_time` (C3 `MonoMicros` on macOS): monotonic, shared by the processes of one
/// machine, and it keeps counting while the Mac sleeps.
pub fn now() -> MonoMicros {
    let (numer, denom) = timebase();
    // SAFETY: a plain libSystem call without arguments.
    let ticks = unsafe { mach_continuous_time() };
    let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom);
    MonoMicros((nanos / 1000) as u64)
}
