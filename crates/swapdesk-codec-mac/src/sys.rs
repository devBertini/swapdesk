//! Error mapping and the monotonic clock for the macOS codec.

use swapdesk_platform::{ErrorKind, PlatformError};

/// `OSStatus` values this crate tells apart (VideoToolbox `VTErrors.h`, CoreVideo `CVReturn.h`; the values come from
/// `objc2-video-toolbox` and `objc2-core-video`).
pub(crate) const VT_INVALID_SESSION: i32 = -12903;
pub(crate) const VT_DECODER_NOT_AVAILABLE_NOW: i32 = -12913;
pub(crate) const VT_DECODER_UNSUPPORTED_DATA_FORMAT: i32 = -12910;

pub(crate) fn err(kind: ErrorKind, op: &'static str, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.into() }
}

/// A failed call that returned an `OSStatus`. Statuses that mean "the session is gone for now" map to `Unavailable`,
/// "this stream is not decodable here" to `Unsupported`, everything else to `Os`.
pub(crate) fn status_err(op: &'static str, status: i32) -> PlatformError {
    let kind = match status {
        VT_INVALID_SESSION | VT_DECODER_NOT_AVAILABLE_NOW => ErrorKind::Unavailable,
        VT_DECODER_UNSUPPORTED_DATA_FORMAT => ErrorKind::Unsupported,
        _ => ErrorKind::Os,
    };
    PlatformError { kind, op, os_code: Some(i64::from(status)), detail: String::new() }
}
