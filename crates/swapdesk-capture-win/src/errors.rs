//! Pure mapping of HRESULTs of the duplication path to C3 `ErrorKind`, and QPC ticks to `MonoMicros`.

use swapdesk_platform::{ErrorKind, PlatformError};

pub const DXGI_ERROR_INVALID_CALL: i32 = 0x887A_0001_u32 as i32;
pub const DXGI_ERROR_UNSUPPORTED: i32 = 0x887A_0004_u32 as i32;
pub const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A_0005_u32 as i32;
pub const DXGI_ERROR_DEVICE_HUNG: i32 = 0x887A_0006_u32 as i32;
pub const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A_0007_u32 as i32;
pub const DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: i32 = 0x887A_0022_u32 as i32;
pub const DXGI_ERROR_ACCESS_LOST: i32 = 0x887A_0026_u32 as i32;
pub const DXGI_ERROR_WAIT_TIMEOUT: i32 = 0x887A_0027_u32 as i32;
pub const DXGI_ERROR_SESSION_DISCONNECTED: i32 = 0x887A_0028_u32 as i32;
pub const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
pub const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

/// Where the failing call sits; the same code means different things before and after a duplication exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// `AcquireNextFrame` and the calls on an acquired frame.
    Frame,
    /// `DuplicateOutput` and device creation, for `open` and `reacquire`.
    Duplicate,
}

/// The C3 kind of an HRESULT (windows.md 1a, 1c, 1d, 2a):
/// - ACCESS_LOST, DEVICE_REMOVED, DEVICE_RESET, DEVICE_HUNG while capturing: `AccessLost` (the caller calls
///   `reacquire`, which rebuilds the device when it is gone).
/// - WAIT_TIMEOUT: `Timeout` (the capture loop turns it into `Ok(None)`).
/// - At `DuplicateOutput`: NOT_CURRENTLY_AVAILABLE (the four-process limit) and SESSION_DISCONNECTED are `Unavailable`;
///   UNSUPPORTED and E_ACCESSDENIED (secure desktop without SYSTEM) are `Unavailable` too, because the page says to wait
///   for a desktop switch or mode change and call again; the detail of the error names the likely cause.
/// - Anything else: `Os`.
pub fn classify(phase: Phase, hresult: i32) -> ErrorKind {
    match hresult {
        DXGI_ERROR_ACCESS_LOST | DXGI_ERROR_DEVICE_REMOVED | DXGI_ERROR_DEVICE_RESET | DXGI_ERROR_DEVICE_HUNG => {
            ErrorKind::AccessLost
        }
        DXGI_ERROR_WAIT_TIMEOUT => ErrorKind::Timeout,
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE | DXGI_ERROR_SESSION_DISCONNECTED => ErrorKind::Unavailable,
        DXGI_ERROR_UNSUPPORTED | E_ACCESSDENIED if phase == Phase::Duplicate => ErrorKind::Unavailable,
        DXGI_ERROR_UNSUPPORTED => ErrorKind::Unsupported,
        E_INVALIDARG | DXGI_ERROR_INVALID_CALL => ErrorKind::InvalidInput,
        _ => ErrorKind::Os,
    }
}

/// A one-line hint for the codes whose cause is known from the documentation.
pub fn hint(hresult: i32) -> &'static str {
    match hresult {
        E_ACCESSDENIED => "the secure desktop needs a LocalSystem process, or the thread is not on the input desktop",
        DXGI_ERROR_UNSUPPORTED => {
            "wait for a desktop switch or mode change, or the device is not on the adapter that owns the output"
        }
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => {
            "the four-process duplication limit of the session, or a session 0 process"
        }
        DXGI_ERROR_SESSION_DISCONNECTED => "the session is disconnected",
        DXGI_ERROR_ACCESS_LOST => "desktop switch, mode change or full-screen application",
        _ => "",
    }
}

pub fn error(kind: ErrorKind, op: &'static str, hresult: Option<i32>, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op, os_code: hresult.map(i64::from), detail: detail.into() }
}

/// A failed HRESULT as a `PlatformError`.
pub fn from_hresult(phase: Phase, op: &'static str, hresult: i32, context: &str) -> PlatformError {
    let hint = hint(hresult);
    let detail = match (context.is_empty(), hint.is_empty()) {
        (true, true) => String::new(),
        (false, true) => context.to_string(),
        (true, false) => hint.to_string(),
        (false, false) => format!("{context}; {hint}"),
    };
    error(classify(phase, hresult), op, Some(hresult), detail)
}

/// QueryPerformanceCounter ticks to microseconds, without overflow for any tick count.
pub fn ticks_to_micros(ticks: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    u64::try_from(u128::from(ticks) * 1_000_000 / u128::from(frequency)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_losses_are_access_lost() {
        for hr in [DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_DEVICE_HUNG] {
            assert_eq!(classify(Phase::Frame, hr), ErrorKind::AccessLost);
        }
    }

    #[test]
    fn the_same_code_depends_on_the_phase() {
        assert_eq!(classify(Phase::Duplicate, DXGI_ERROR_UNSUPPORTED), ErrorKind::Unavailable);
        assert_eq!(classify(Phase::Frame, DXGI_ERROR_UNSUPPORTED), ErrorKind::Unsupported);
        assert_eq!(classify(Phase::Duplicate, E_ACCESSDENIED), ErrorKind::Unavailable);
        assert_eq!(classify(Phase::Frame, E_ACCESSDENIED), ErrorKind::Os);
    }

    #[test]
    fn limit_and_disconnect_are_unavailable_and_timeout_is_timeout() {
        assert_eq!(classify(Phase::Duplicate, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE), ErrorKind::Unavailable);
        assert_eq!(classify(Phase::Duplicate, DXGI_ERROR_SESSION_DISCONNECTED), ErrorKind::Unavailable);
        assert_eq!(classify(Phase::Frame, DXGI_ERROR_WAIT_TIMEOUT), ErrorKind::Timeout);
        assert_eq!(classify(Phase::Duplicate, E_INVALIDARG), ErrorKind::InvalidInput);
        assert_eq!(classify(Phase::Frame, 0x8000_4005_u32 as i32), ErrorKind::Os);
    }

    #[test]
    fn display_carries_op_kind_code_and_detail() {
        let e = from_hresult(Phase::Duplicate, "DuplicateOutput", E_ACCESSDENIED, "display 1");
        let s = e.to_string();
        assert!(s.starts_with("DuplicateOutput: Unavailable ("), "{s}");
        assert!(s.contains("display 1; the secure desktop needs a LocalSystem process"), "{s}");
        assert_eq!(e.os_code, Some(i64::from(E_ACCESSDENIED)));
    }

    #[test]
    fn qpc_ticks_convert_without_overflow() {
        assert_eq!(ticks_to_micros(10_000_000, 10_000_000), 1_000_000);
        assert_eq!(ticks_to_micros(15, 10_000_000), 1);
        assert_eq!(ticks_to_micros(0, 0), 0);
        // 10 years of 10 MHz ticks: the intermediate product exceeds u64.
        let ticks = 10 * 365 * 86_400 * 10_000_000_u64;
        assert_eq!(ticks_to_micros(ticks, 10_000_000), 10 * 365 * 86_400 * 1_000_000);
    }
}
