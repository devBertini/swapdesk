//! The local message set between the Swapdesk service, its helpers and the UI (`docs/CONTRACTS.md` C2.4), with
//! blocking framing over `Read` / `Write`. No I/O of its own, no `unsafe`, no async runtime.
//!
//! One message is `varint type`, `varint length`, body; the proto types inside are embedded with their own
//! `Wire` encoding. A length above the type's limit is refused before any body byte is read, an unknown type is
//! a protocol error, and the `Debug` of every message prints lengths, never tokens, text, input or bytes.

#[macro_use]
mod macros;
mod clip;
mod error;
mod files;
mod frame;
mod handshake;
mod message;
mod ops;
mod redact;
mod term;
mod types;

pub use clip::{ClipData, ClipFail, ClipReplyBody, MAX_CLIP_MESSAGE};
pub use error::{IpcError, Result};
pub use files::{
    FileEntry, FileListing, FileListing2, FilePick, FilesNote, FilesOp, FsAnswer, FsCall, FsChange, FsEntry2View,
    FsEntryView, FsHandleView, FsKind, FsNameView, FsPage, FsStatView, MAX_FS_IO, MAX_FS_LIST, MAX_FS_MESSAGE,
    MAX_FS_META, MAX_FS_NAME, MAX_FS_PATH, MAX_OP_NAMES, MAX_PICKS, MAX_PLACES, MAX_VOLUME_LABEL, MAX_VOLUMES,
    OpResultView, PlaceView, TransferWire, VolumeView, VolumesView,
};
pub use frame::{FrameReader, FrameWriter};
pub use handshake::{ClientHello, Handshake, ServerHello};
pub use message::{
    IPC_VERSION, MAX_AUDIO_DATAGRAM, MAX_FAULT_TEXT, MAX_INPUT_MESSAGE, MAX_OTHER_MESSAGE, MAX_VIDEO_MESSAGE, Message,
    decode_message, encode_into, encode_message, limit_of,
};
pub use ops::{Notify, ReplyBody, STATUS_OK, UiOp, status_code};
pub use redact::{Bytes, ElevationTicket, Events, Payload, Shape, Text};
pub use term::{MAX_SESSION_USER, MAX_TERM_ACCOUNT, MAX_TERM_CHUNK, TermOutcome};
pub use types::*;
