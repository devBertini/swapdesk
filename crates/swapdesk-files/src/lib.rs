//! File transfers of Swapdesk (SPEC F-FILE-01 and F-FILE-02, PROTOCOL.md 5.3 and the 0x05 data stream).
//!
//! - [`disk`]: the [`Fs`] trait every disk access goes through, and [`local`] its `LocalFs`.
//! - [`proxy`]: [`PipeFs`], the [`Fs`] over a pipe, and [`FsServer`], what the process that owns the files runs.
//! - [`name`]: the name rules both sides apply.
//! - [`access`]: the state of the receiving folder when the system refuses this process (macOS grant missing).
//! - [`collide`]: names that are the same file under normalization and letter case, and the new name chosen.
//! - [`space`]: the free-space rule that declines an offer with `NO_SPACE`.
//! - [`recv`]: safe receiving into a partial file, resume from the verified length, SHA-256 check, rename.
//! - [`send`]: the offer and a reader that continues from the receiver's verified length.
//! - [`listing`]: the remote folder listing.
//! - [`engine`]: the state machine over the file messages, without network or tasks.
//! - [`handler`]: [`FilesHandler`], the `ChannelHandler` that registers in `swapdesk-session`.

pub mod access;
pub mod collide;
pub mod disk;
pub mod engine;
mod fsops;
pub mod fsutil;
pub mod handler;
pub mod listing;
pub mod local;
pub mod name;
pub mod proxy;
pub mod recv;
pub mod send;
pub mod space;
pub mod system;
mod volumes;

pub use access::{AccessState, ReceiveAccess};
pub use disk::{DirRead, Fs, FsEntry, FsStat, Kind, MAX_IO, Moved, ReadHandle};
pub use engine::{
    DEFAULT_MAX_FILE_SIZE, Engine, Event, FilesConfig, MAX_FS_OPS, OpJob, Out, Side, busy_result, failed_result,
};
pub use handler::{Command, FilesHandler, FilesSink};
pub use listing::{Listing, list_dir, list_dir_with};
pub use local::{FreeSpaceFn, LocalFs};
pub use name::validate_name;
pub use proxy::{
    CALL_TIMEOUT, CallerSource, DEFAULT_RECEIVE, FsCaller, FsServer, MOVE_CALL_TIMEOUT, Pending, PipeCaller, PipeFs,
    Roots, Tokens,
};
pub use recv::{FinishError, Incoming};
pub use send::{ChunkReader, Outgoing};
pub use system::{SysVolume, SystemDisk};
