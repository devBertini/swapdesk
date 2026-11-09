//! `SCStreamError` codes and what the helper should do about each: one table shared by the screen and the audio source.
//!
//! Codes from `SCStreamErrorCode` (developer.apple.com/documentation/screencapturekit/scstreamerror/code; values as
//! shipped in `objc2-screen-capture-kit` 0.3.2, `SCError.rs`). Pure code: it builds and is tested on every host.
//!
//! The helper acts on the kind only (`swapdesk-host`, `helper.rs`): `AccessLost` makes the video thread call
//! `reacquire` and go on, `Unavailable` and `Timeout` are reported once and retried, `Closed` and everything else end
//! the thread, `PermissionDenied` names the grant to the UI.

use swapdesk_platform::{ErrorKind, OsGrant};

/// `SCStreamErrorUserDeclined`.
pub(crate) const USER_DECLINED: i64 = -3801;

/// The kind for the code of an `SCStreamError` reported by a call that failed (start, content query) or by a stream
/// that stopped.
///
/// * `-3817` user stopped, `-3816` removing stream, `-3808` attempt to stop in a stop state: the stream was stopped
///   on purpose or is already going down: `Closed`, so the helper stops instead of re-acquiring.
/// * `-3801` user declined: `PermissionDenied(ScreenRecording)`.
/// * `-3821` system stopped the stream (display sleep, display or configuration change, another capture taking over):
///   `AccessLost`, the stream is rebuilt. `-3804` application connection invalid and `-3805` connection interrupted
///   (the capture daemon restarted) are the same: rebuild.
/// * `-3814` no display list, `-3815` no capture source, `-3813` no window list, `-3802` failed to start, `-3818`
///   failed to start audio capture: nothing to capture right now, `Unavailable`.
/// * anything else: `Os`.
pub(crate) fn kind_for_code(code: i64) -> ErrorKind {
    match code {
        -3817 | -3816 | -3808 => ErrorKind::Closed,
        USER_DECLINED => ErrorKind::PermissionDenied(OsGrant::ScreenRecording),
        -3821 | -3804 | -3805 => ErrorKind::AccessLost,
        -3814 | -3815 | -3813 | -3802 | -3818 => ErrorKind::Unavailable,
        _ => ErrorKind::Os,
    }
}

/// The kind for a stream that was running and then stopped with `code` (`stream:didStopWithError:`). Same table, except
/// that "nothing to capture right now" becomes `AccessLost`: the stop is latched, so only a rebuild (`reacquire`, which
/// then reports `Unavailable` until the display is back) can recover; `Unavailable` from `next_frame` would repeat the
/// same latched error for ever.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn stop_kind(code: i64) -> ErrorKind {
    match kind_for_code(code) {
        ErrorKind::Unavailable => ErrorKind::AccessLost,
        kind => kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stopped_on_purpose_codes_are_closed() {
        for code in [-3817, -3816, -3808] {
            assert_eq!(kind_for_code(code), ErrorKind::Closed, "{code}");
            assert_eq!(stop_kind(code), ErrorKind::Closed, "{code}");
        }
    }

    #[test]
    fn a_declined_grant_names_screen_recording() {
        let denied = ErrorKind::PermissionDenied(OsGrant::ScreenRecording);
        assert_eq!(kind_for_code(-3801), denied);
        assert_eq!(kind_for_code(USER_DECLINED), denied);
        assert_eq!(stop_kind(-3801), denied);
    }

    #[test]
    fn the_system_stopping_the_stream_is_access_lost() {
        assert_eq!(kind_for_code(-3821), ErrorKind::AccessLost);
        assert_eq!(kind_for_code(-3804), ErrorKind::AccessLost);
        assert_eq!(kind_for_code(-3805), ErrorKind::AccessLost);
        assert_eq!(stop_kind(-3821), ErrorKind::AccessLost);
    }

    #[test]
    fn nothing_to_capture_is_unavailable_at_start_and_access_lost_when_a_stream_stops() {
        for code in [-3814, -3815, -3813, -3802, -3818] {
            assert_eq!(kind_for_code(code), ErrorKind::Unavailable, "{code}");
            assert_eq!(stop_kind(code), ErrorKind::AccessLost, "{code}");
        }
    }

    #[test]
    fn other_codes_are_os() {
        for code in [-3803, -3806, -3807, -3809, -3810, -3811, -3812, -3819, -3820, -1, 0, 1] {
            assert_eq!(kind_for_code(code), ErrorKind::Os, "{code}");
            assert_eq!(stop_kind(code), ErrorKind::Os, "{code}");
        }
    }

    #[test]
    fn both_sources_use_this_table() {
        for code in -3830..=-3795 {
            assert_eq!(crate::audio::kind_for_stream_code(code), kind_for_code(code), "{code}");
            assert_eq!(crate::screen::error_kind_for_code(code), kind_for_code(code), "{code}");
        }
    }
}
