//! Bodies of the clipboard and quick-action messages (0x60 to 0x66, C2.4 "Clipboard and quick actions").
//!
//! A clipboard content is `tag: u8` (0 text, 1 PNG) and the bytes up to the end of the body. The size is checked
//! against the PROTOCOL.md 7.5 limits before anything is allocated, and the `Debug` of every type that holds content
//! prints a kind and a length, never the content.

use core::fmt;

use swapdesk_proto::{CodecError, MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_TEXT, Reader, Writer};

use crate::macros::unknown_tag;

type CResult<T> = Result<T, CodecError>;

/// Limit of `ClipWrite` and `ClipReply`: the largest image plus the fixed fields.
pub const MAX_CLIP_MESSAGE: usize = MAX_CLIPBOARD_PNG as usize + 64;

/// What a clipboard holds, as it crosses a local pipe. Its `Debug` prints the kind and the size.
#[derive(Clone, PartialEq, Eq)]
pub enum ClipData {
    Text(String),
    Png(Vec<u8>),
}

impl fmt::Debug for ClipData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(t) => write!(f, "ClipData::Text(len {})", t.len()),
            Self::Png(p) => write!(f, "ClipData::Png(len {})", p.len()),
        }
    }
}

impl ClipData {
    pub(crate) fn put(&self, w: &mut Writer) -> CResult<()> {
        match self {
            Self::Text(t) => {
                if t.len() > MAX_CLIPBOARD_TEXT as usize {
                    return Err(CodecError::OverLimit { what: "clipboard text", limit: u64::from(MAX_CLIPBOARD_TEXT) });
                }
                w.put_u8(0);
                w.put_bytes(t.as_bytes());
            }
            Self::Png(p) => {
                if p.len() > MAX_CLIPBOARD_PNG as usize {
                    return Err(CodecError::OverLimit { what: "clipboard image", limit: u64::from(MAX_CLIPBOARD_PNG) });
                }
                w.put_u8(1);
                w.put_bytes(p);
            }
        }
        Ok(())
    }

    /// Reads a content that runs to the end of the body.
    pub(crate) fn get(r: &mut Reader<'_>) -> CResult<Self> {
        let tag = r.u8()?;
        let (limit, what) = match tag {
            0 => (MAX_CLIPBOARD_TEXT, "clipboard text"),
            1 => (MAX_CLIPBOARD_PNG, "clipboard image"),
            t => return Err(unknown_tag("ClipData", t)),
        };
        // Checked before the copy.
        if r.remaining() > limit as usize {
            return Err(CodecError::OverLimit { what, limit: u64::from(limit) });
        }
        let bytes = r.take(r.remaining())?;
        Ok(if tag == 0 {
            Self::Text(String::from_utf8(bytes.to_vec()).map_err(|_| CodecError::InvalidUtf8)?)
        } else {
            Self::Png(bytes.to_vec())
        })
    }
}

/// Why a helper or a UI could not do a clipboard call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipFail {
    /// The clipboard is busy or not reachable now (transient).
    Unavailable = 0,
    /// The content was refused (over the limit, not a PNG).
    Invalid = 1,
    /// Anything else, including "this process has no clipboard".
    Other = 2,
}

impl ClipFail {
    fn from_tag(t: u8) -> CResult<Self> {
        match t {
            0 => Ok(Self::Unavailable),
            1 => Ok(Self::Invalid),
            2 => Ok(Self::Other),
            t => Err(unknown_tag("ClipFail", t)),
        }
    }
}

/// The answer to `ClipChangeCount`, `ClipRead` or `ClipWrite` (message 0x63).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipReplyBody {
    /// To `ClipChangeCount`.
    Count(u64),
    /// To `ClipRead`: what the clipboard holds.
    Content(ClipData),
    /// To `ClipRead`: content marked as not to be shared (a password manager's copy).
    Concealed,
    /// To `ClipRead`: nothing on the clipboard.
    Empty,
    /// To `ClipRead`: a format this version does not carry.
    Unsupported,
    /// To `ClipWrite`: written.
    Written,
    /// To any of the three: it failed.
    Failed(ClipFail),
}

impl ClipReplyBody {
    pub(crate) fn put(&self, w: &mut Writer) -> CResult<()> {
        match self {
            Self::Count(n) => {
                w.put_u8(0);
                w.put_u64(*n);
            }
            Self::Content(c) => {
                w.put_u8(1);
                c.put(w)?;
            }
            Self::Concealed => w.put_u8(2),
            Self::Empty => w.put_u8(3),
            Self::Unsupported => w.put_u8(4),
            Self::Written => w.put_u8(5),
            Self::Failed(f) => {
                w.put_u8(6);
                w.put_u8(*f as u8);
            }
        }
        Ok(())
    }

    pub(crate) fn get(r: &mut Reader<'_>) -> CResult<Self> {
        Ok(match r.u8()? {
            0 => Self::Count(r.u64()?),
            1 => Self::Content(ClipData::get(r)?),
            2 => Self::Concealed,
            3 => Self::Empty,
            4 => Self::Unsupported,
            5 => Self::Written,
            6 => Self::Failed(ClipFail::from_tag(r.u8()?)?),
            t => return Err(unknown_tag("ClipReplyBody", t)),
        })
    }
}
