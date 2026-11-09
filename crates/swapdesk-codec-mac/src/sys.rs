//! Error mapping and the monotonic clock for the macOS codec.

use swapdesk_platform::{ErrorKind, PlatformError};

/// `OSStatus` values this crate tells apart (VideoToolbox `VTErrors.h`, CoreVideo `CVReturn.h`; the values come from
/// `objc2-video-toolbox` and `objc2-core-video`).
pub(crate) const VT_INVALID_SESSION: i32 = -12903;
pub(crate) const VT_DECODER_NOT_AVAILABLE_NOW: i32 = -12913;
pub(crate) const VT_DECODER_UNSUPPORTED_DATA_FORMAT: i32 = -12910;
pub(crate) const VT_PROPERTY_NOT_SUPPORTED: i32 = -12900;
pub(crate) const VT_PARAMETER: i32 = -12902;
pub(crate) const VT_COULD_NOT_FIND_ENCODER: i32 = -12908;
pub(crate) const VT_ENCODER_NOT_AVAILABLE_NOW: i32 = -12915;
/// `kCVReturnInvalidArgument`.
pub(crate) const CV_INVALID_ARGUMENT: i32 = -6661;

pub(crate) fn err(kind: ErrorKind, op: &'static str, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.into() }
}

/// A failed call that returned an `OSStatus`. Statuses that mean "the session or encoder is gone or busy for now" map
/// to `Unavailable`, "not decodable or not supported here" to `Unsupported`, "bad parameter or frame" to
/// `InvalidInput`, everything else to `Os`. The code goes into `os_code` and shows in the message.
pub(crate) fn status_err(op: &'static str, status: i32) -> PlatformError {
    let kind = match status {
        VT_INVALID_SESSION
        | VT_DECODER_NOT_AVAILABLE_NOW
        | VT_ENCODER_NOT_AVAILABLE_NOW
        | VT_COULD_NOT_FIND_ENCODER => ErrorKind::Unavailable,
        VT_DECODER_UNSUPPORTED_DATA_FORMAT | VT_PROPERTY_NOT_SUPPORTED => ErrorKind::Unsupported,
        VT_PARAMETER | CV_INVALID_ARGUMENT => ErrorKind::InvalidInput,
        _ => ErrorKind::Os,
    };
    PlatformError { kind, op, os_code: Some(i64::from(status)), detail: String::new() }
}
