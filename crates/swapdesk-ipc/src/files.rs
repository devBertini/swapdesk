//! The file messages of C2.4 ("Files", stage 44): the disk proxy a process that owns the user's files serves to the
//! service (`FsRequest` 0x80 and `FsReply` 0x81, on the user helper's pipe for the host side and on the UI pipe for
//! the viewer side), the UI ops 40 to 45 and the notifications 20 and 21.
//!
//! Every path and name on the wire is a [`Text`] (its `Debug` prints a length) and every body of bytes a [`Bytes`], so
//! no `Debug` of this module shows a path or file content. Every length is checked against its limit before
//! anything is allocated. The paths are virtual: relative to a root the process that owns the disk chose (see
//! CONTRACTS.md C2.4 "Files"); this module only carries them.

use swapdesk_proto::{CodecError, EntryKind, ErrorCode, PlaceKind, Reader, VolumeKind, Wire, Writer};

use crate::error::IpcError;
use crate::redact::{Bytes, Text};
use crate::types::ApiCode;

type CResult<T> = core::result::Result<T, CodecError>;

/// Longest virtual path of a call, a `FileOffer.dest` and a listing (the proto limit of `DirRequest.path`).
pub const MAX_FS_PATH: usize = 1024;
/// Longest file name (PROTOCOL.md 5.3).
pub const MAX_FS_NAME: usize = 255;
/// Most bytes one `PartAppend`, `PartRead`, `ReadAt` or `MetaWrite` moves: the same as `swapdesk_files::MAX_IO`.
pub const MAX_FS_IO: usize = 1 << 20;
/// Most entries one `ListDir` answers (PROTOCOL.md 5.3 `DirListing`).
pub const MAX_FS_LIST: usize = 2000;
/// Most bytes `MetaRead` answers.
pub const MAX_FS_META: usize = 1024;
/// Limit of `FsRequest` and `FsReply`: a full chunk plus the path, the name and the numbers around it.
pub const MAX_FS_MESSAGE: usize = MAX_FS_IO + 4096;
/// Most files one `FilesSend` carries.
pub const MAX_PICKS: usize = 64;
/// Most names one change carries (`FsOp`, FILES-UI.md 8.4).
pub const MAX_OP_NAMES: usize = 64;
/// Most places and volumes of a volume list (FILES-UI.md 8.4).
pub const MAX_PLACES: usize = 16;
pub const MAX_VOLUMES: usize = 32;
/// Longest volume label.
pub const MAX_VOLUME_LABEL: usize = 64;

wire_enum! {
    /// Which of a receiver's two side files a call is about.
    FsKind { Part = 0, Meta = 1 }
}

/// One disk call, the `Fs` trait of `swapdesk-files` one for one. The first byte of the body is the call number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsCall {
    /// 1.
    ListDir { path: Text, max_entries: u32 },
    /// 2.
    Stat { path: Text, follow: bool },
    /// 3.
    OpenRead { path: Text },
    /// 4.
    ReadAt { id: u64, offset: u64, len: u32 },
    /// 5.
    ReadStat { id: u64 },
    /// 6. Fire and forget: no reply.
    CloseRead { id: u64 },
    /// 7.
    StatFile { dir: Text, name: Text, kind: FsKind },
    /// 8.
    PartCreate { dir: Text, name: Text },
    /// 9.
    PartTruncate { dir: Text, name: Text, len: u64 },
    /// 10. `data` is the rest of the body (at most [`MAX_FS_IO`]); the part file must be exactly `offset` long.
    PartAppend { dir: Text, name: Text, offset: u64, data: Bytes },
    /// 11.
    PartLen { dir: Text, name: Text },
    /// 12.
    PartRead { dir: Text, name: Text, offset: u64, len: u32 },
    /// 13.
    PartSync { dir: Text, name: Text, final_mtime_ms: Option<u64> },
    /// 14.
    PartRename { dir: Text, name: Text, target_name: Text },
    /// 15.
    RemoveFile { dir: Text, name: Text, kind: FsKind },
    /// 16.
    MetaRead { dir: Text, name: Text },
    /// 17. `data` is the rest of the body (at most [`MAX_FS_META`]).
    MetaWrite { dir: Text, name: Text, data: Bytes },
    /// 24 (`FILES-UI.md` 7.3; 18 to 23 are the explorer's): the free bytes of the volume that holds `dir`, for the
    /// free-space check before a file is accepted (F-FILE-03).
    FreeSpace { dir: Text },
    /// 18 (FILES-UI.md 7.4): a page of `path` from entry `start`, at most `max` entries, with flags and the free
    /// space; answered with `Entries2`.
    ListPage { path: Text, start: u32, max: u16 },
    /// 19: makes the folder `dir/name`; `Exists` when anything is there.
    MakeDir { dir: Text, name: Text },
    /// 20: renames `dir/name` to `dir/new_name`, never replacing (`Exists`).
    Rename { dir: Text, name: Text, new_name: Text },
    /// 21: moves `from_dir/name` into `to_dir`, never replacing; across volumes the helper copies, verifies and sends
    /// the source to the bin.
    Move { from_dir: Text, name: Text, to_dir: Text },
    /// 22: sends `dir/name` to the bin; `NoBin` before touching anything where there is none.
    Trash { dir: Text, name: Text },
    /// 23: the home folder, the places and the published volumes; answered with `Volumes`.
    Volumes,
}

impl FsCall {
    /// The call number (first byte of the body).
    pub const fn code(&self) -> u8 {
        match self {
            Self::ListDir { .. } => 1,
            Self::Stat { .. } => 2,
            Self::OpenRead { .. } => 3,
            Self::ReadAt { .. } => 4,
            Self::ReadStat { .. } => 5,
            Self::CloseRead { .. } => 6,
            Self::StatFile { .. } => 7,
            Self::PartCreate { .. } => 8,
            Self::PartTruncate { .. } => 9,
            Self::PartAppend { .. } => 10,
            Self::PartLen { .. } => 11,
            Self::PartRead { .. } => 12,
            Self::PartSync { .. } => 13,
            Self::PartRename { .. } => 14,
            Self::RemoveFile { .. } => 15,
            Self::MetaRead { .. } => 16,
            Self::MetaWrite { .. } => 17,
            Self::FreeSpace { .. } => 24,
            Self::ListPage { .. } => 18,
            Self::MakeDir { .. } => 19,
            Self::Rename { .. } => 20,
            Self::Move { .. } => 21,
            Self::Trash { .. } => 22,
            Self::Volumes => 23,
        }
    }

    /// True for the one call that is answered by nothing.
    pub const fn is_fire_and_forget(&self) -> bool {
        matches!(self, Self::CloseRead { .. })
    }

    pub(crate) fn put(&self, w: &mut Writer) -> CResult<()> {
        w.put_u8(self.code());
        let path = |w: &mut Writer, t: &Text| t.put(w, MAX_FS_PATH);
        let name = |w: &mut Writer, t: &Text| t.put(w, MAX_FS_NAME);
        match self {
            Self::ListDir { path: p, max_entries } => {
                path(w, p)?;
                w.put_u32(*max_entries);
            }
            Self::Stat { path: p, follow } => {
                path(w, p)?;
                w.put_bool(*follow);
            }
            Self::OpenRead { path: p } | Self::FreeSpace { dir: p } => path(w, p)?,
            Self::ReadAt { id, offset, len } => {
                w.put_u64(*id);
                w.put_u64(*offset);
                w.put_u32(*len);
            }
            Self::ReadStat { id } | Self::CloseRead { id } => w.put_u64(*id),
            Self::StatFile { dir, name: n, kind } | Self::RemoveFile { dir, name: n, kind } => {
                path(w, dir)?;
                name(w, n)?;
                kind.put(w, 0)?;
            }
            Self::PartCreate { dir, name: n } | Self::PartLen { dir, name: n } | Self::MetaRead { dir, name: n } => {
                path(w, dir)?;
                name(w, n)?;
            }
            Self::PartTruncate { dir, name: n, len } => {
                path(w, dir)?;
                name(w, n)?;
                w.put_u64(*len);
            }
            Self::PartAppend { dir, name: n, offset, data } => {
                path(w, dir)?;
                name(w, n)?;
                w.put_u64(*offset);
                if data.0.len() > MAX_FS_IO {
                    return Err(CodecError::TooLarge { what: "file chunk", limit: MAX_FS_IO as u64 });
                }
                w.put_bytes(&data.0);
            }
            Self::PartRead { dir, name: n, offset, len } => {
                path(w, dir)?;
                name(w, n)?;
                w.put_u64(*offset);
                w.put_u32(*len);
            }
            Self::PartSync { dir, name: n, final_mtime_ms } => {
                path(w, dir)?;
                name(w, n)?;
                final_mtime_ms.put(w, 0)?;
            }
            Self::PartRename { dir, name: n, target_name } => {
                path(w, dir)?;
                name(w, n)?;
                name(w, target_name)?;
            }
            Self::MetaWrite { dir, name: n, data } => {
                path(w, dir)?;
                name(w, n)?;
                if data.0.len() > MAX_FS_META {
                    return Err(CodecError::TooLarge { what: "meta record", limit: MAX_FS_META as u64 });
                }
                w.put_bytes(&data.0);
            }
            Self::ListPage { path: p, start, max } => {
                path(w, p)?;
                w.put_u32(*start);
                w.put_u16(*max);
            }
            Self::MakeDir { dir, name: n } | Self::Trash { dir, name: n } => {
                path(w, dir)?;
                name(w, n)?;
            }
            Self::Rename { dir, name: n, new_name } => {
                path(w, dir)?;
                name(w, n)?;
                name(w, new_name)?;
            }
            Self::Move { from_dir, name: n, to_dir } => {
                path(w, from_dir)?;
                name(w, n)?;
                path(w, to_dir)?;
            }
            Self::Volumes => {}
        }
        Ok(())
    }

    pub(crate) fn get(r: &mut Reader<'_>) -> CResult<Self> {
        let code = r.u8()?;
        let path = |r: &mut Reader<'_>| Text::get(r, MAX_FS_PATH);
        let name = |r: &mut Reader<'_>| Text::get(r, MAX_FS_NAME);
        // The rest of the body, after the limit is checked: nothing is copied before.
        let rest = |r: &mut Reader<'_>, limit: usize, what: &'static str| -> CResult<Bytes> {
            if r.remaining() > limit {
                return Err(CodecError::TooLarge { what, limit: limit as u64 });
            }
            Ok(Bytes(r.take(r.remaining())?.to_vec()))
        };
        Ok(match code {
            1 => Self::ListDir { path: path(r)?, max_entries: r.u32()? },
            2 => Self::Stat { path: path(r)?, follow: r.bool()? },
            3 => Self::OpenRead { path: path(r)? },
            4 => Self::ReadAt { id: r.u64()?, offset: r.u64()?, len: r.u32()? },
            5 => Self::ReadStat { id: r.u64()? },
            6 => Self::CloseRead { id: r.u64()? },
            7 => Self::StatFile { dir: path(r)?, name: name(r)?, kind: FsKind::get(r, 0)? },
            8 => Self::PartCreate { dir: path(r)?, name: name(r)? },
            9 => Self::PartTruncate { dir: path(r)?, name: name(r)?, len: r.u64()? },
            10 => Self::PartAppend {
                dir: path(r)?,
                name: name(r)?,
                offset: r.u64()?,
                data: rest(r, MAX_FS_IO, "file chunk")?,
            },
            11 => Self::PartLen { dir: path(r)?, name: name(r)? },
            12 => Self::PartRead { dir: path(r)?, name: name(r)?, offset: r.u64()?, len: r.u32()? },
            13 => Self::PartSync { dir: path(r)?, name: name(r)?, final_mtime_ms: <Option<u64>>::get(r, 0)? },
            14 => Self::PartRename { dir: path(r)?, name: name(r)?, target_name: name(r)? },
            15 => Self::RemoveFile { dir: path(r)?, name: name(r)?, kind: FsKind::get(r, 0)? },
            16 => Self::MetaRead { dir: path(r)?, name: name(r)? },
            17 => Self::MetaWrite { dir: path(r)?, name: name(r)?, data: rest(r, MAX_FS_META, "meta record")? },
            24 => Self::FreeSpace { dir: path(r)? },
            18 => Self::ListPage { path: path(r)?, start: r.u32()?, max: r.u16()? },
            19 => Self::MakeDir { dir: path(r)?, name: name(r)? },
            20 => Self::Rename { dir: path(r)?, name: name(r)?, new_name: name(r)? },
            21 => Self::Move { from_dir: path(r)?, name: name(r)?, to_dir: path(r)? },
            22 => Self::Trash { dir: path(r)?, name: name(r)? },
            23 => Self::Volumes,
            other => return Err(CodecError::UnknownTag { what: "FsRequest call", tag: u64::from(other) }),
        })
    }
}

wire_struct! {
    /// What an entry is (`FsStat` of `swapdesk-files`).
    FsStatView { is_dir: bool, is_file: bool, is_reparse: bool, len: u64, mtime_ms: u64 }
}

wire_struct! {
    /// A file opened for reading (`ReadHandle`): `id` is meaningful only to the process that opened it.
    FsHandleView { id: u64, len: u64, mtime_ms: u64 }
}

wire_struct! {
    /// A folder entry (`FsEntry`).
    FsEntryView { name: Text [MAX_FS_NAME], is_dir: bool, size: u64, mtime_ms: u64 }
}

wire_struct! {
    /// A folder entry of a page (`DirEntry2` of PROTOCOL.md 5.3): `flags` holds the proto's `ENTRY_*` bits.
    FsEntry2View { name: Text [MAX_FS_NAME], kind: EntryKind, size: u64, mtime_ms: u64, flags: u8 }
}

wire_struct! {
    /// A well-known folder, as a virtual path.
    PlaceView { kind: PlaceKind, path: Text [MAX_FS_PATH] }
}

wire_struct! {
    /// A published volume: its virtual paths start with `:<id>`; `letter` is the Windows drive letter. The label is
    /// shown, never used as a path.
    VolumeView {
        id: u32,
        kind: VolumeKind,
        label: Text [MAX_VOLUME_LABEL],
        letter: Option<u8>,
        total_bytes: u64,
        free_bytes: u64,
        has_bin: bool,
    }
}

wire_struct! {
    /// The home folder (a virtual path), the places and the volumes of the machine that owns the disk.
    VolumesView { home: Text [MAX_FS_PATH], places: Vec<PlaceView> [MAX_PLACES], volumes: Vec<VolumeView> [MAX_VOLUMES] }
}

wire_struct! {
    /// A page of a folder (`FsAnswer::Entries2`, and the UI's `FileListing2` without its path).
    FsPage { entries: Vec<FsEntry2View> [MAX_FS_LIST], next: Option<u32>, free: Option<u64> }
}

/// What a successful call answers. The first byte of the body is the variant number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsAnswer {
    /// 0: `PartCreate`, `PartTruncate`, `PartAppend`, `PartSync`, `PartRename`, `RemoveFile`, `MetaWrite`.
    Done,
    /// 1: `ListDir`.
    Entries { entries: Vec<FsEntryView>, truncated: bool },
    /// 2: `Stat`, `StatFile`.
    Stat(Option<FsStatView>),
    /// 3: `OpenRead`.
    Handle(FsHandleView),
    /// 4: `ReadAt`, `PartRead`: the rest of the body.
    Data(Bytes),
    /// 5: `ReadStat`.
    StatNow(FsStatView),
    /// 6: `PartLen`.
    Len(u64),
    /// 7: `MetaRead`: `None` when the file is missing or not a regular file.
    Meta(Option<Bytes>),
    /// 10 (8 and 9 are the explorer's): `FreeSpace`; `None` when the disk cannot tell.
    Free(Option<u64>),
    /// 8: `ListPage`.
    Entries2(FsPage),
    /// 9: `Volumes`.
    Volumes(VolumesView),
}

impl FsAnswer {
    fn put(&self, w: &mut Writer) -> CResult<()> {
        match self {
            Self::Done => w.put_u8(0),
            Self::Entries { entries, truncated } => {
                w.put_u8(1);
                w.put_bool(*truncated);
                entries.put(w, MAX_FS_LIST)?;
            }
            Self::Stat(s) => {
                w.put_u8(2);
                s.put(w, 0)?;
            }
            Self::Handle(h) => {
                w.put_u8(3);
                h.put(w, 0)?;
            }
            Self::Data(d) => {
                w.put_u8(4);
                if d.0.len() > MAX_FS_IO {
                    return Err(CodecError::TooLarge { what: "file chunk", limit: MAX_FS_IO as u64 });
                }
                w.put_bytes(&d.0);
            }
            Self::StatNow(s) => {
                w.put_u8(5);
                s.put(w, 0)?;
            }
            Self::Len(n) => {
                w.put_u8(6);
                w.put_u64(*n);
            }
            Self::Meta(m) => {
                w.put_u8(7);
                match m {
                    None => w.put_bool(false),
                    Some(b) => {
                        if b.0.len() > MAX_FS_META {
                            return Err(CodecError::TooLarge { what: "meta record", limit: MAX_FS_META as u64 });
                        }
                        w.put_bool(true);
                        w.put_bytes(&b.0);
                    }
                }
            }
            Self::Free(n) => {
                w.put_u8(10);
                n.put(w, 0)?;
            }
            Self::Entries2(p) => {
                w.put_u8(8);
                p.put(w, 0)?;
            }
            Self::Volumes(v) => {
                w.put_u8(9);
                v.put(w, 0)?;
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>) -> CResult<Self> {
        Ok(match r.u8()? {
            0 => Self::Done,
            1 => {
                let truncated = r.bool()?;
                Self::Entries { truncated, entries: <Vec<FsEntryView>>::get(r, MAX_FS_LIST)? }
            }
            2 => Self::Stat(<Option<FsStatView>>::get(r, 0)?),
            3 => Self::Handle(FsHandleView::get(r, 0)?),
            4 => {
                if r.remaining() > MAX_FS_IO {
                    return Err(CodecError::TooLarge { what: "file chunk", limit: MAX_FS_IO as u64 });
                }
                Self::Data(Bytes(r.take(r.remaining())?.to_vec()))
            }
            5 => Self::StatNow(FsStatView::get(r, 0)?),
            6 => Self::Len(r.u64()?),
            7 => {
                if r.bool()? {
                    if r.remaining() > MAX_FS_META {
                        return Err(CodecError::TooLarge { what: "meta record", limit: MAX_FS_META as u64 });
                    }
                    Self::Meta(Some(Bytes(r.take(r.remaining())?.to_vec())))
                } else {
                    Self::Meta(None)
                }
            }
            10 => Self::Free(<Option<u64>>::get(r, 0)?),
            8 => Self::Entries2(FsPage::get(r, 0)?),
            9 => Self::Volumes(VolumesView::get(r, 0)?),
            tag => return Err(CodecError::UnknownTag { what: "FsReply answer", tag: u64::from(tag) }),
        })
    }
}

/// The body of `FsReply` after `req`: `u8` 0 then the answer, or 1 then the `ErrorCode`.
pub(crate) fn put_result(w: &mut Writer, result: &core::result::Result<FsAnswer, ErrorCode>) -> CResult<()> {
    match result {
        Ok(a) => {
            w.put_u8(0);
            a.put(w)
        }
        Err(code) => {
            w.put_u8(1);
            code.put(w, 0)
        }
    }
}

pub(crate) fn get_result(r: &mut Reader<'_>) -> CResult<core::result::Result<FsAnswer, ErrorCode>> {
    match r.u8()? {
        0 => Ok(Ok(FsAnswer::get(r)?)),
        1 => Ok(Err(ErrorCode::get(r, 0)?)),
        tag => Err(CodecError::UnknownTag { what: "FsReply status", tag: u64::from(tag) }),
    }
}

// ---------- the UI ops 40 to 45 ----------

wire_struct! {
    /// A file the user picked in the UI: the UI process holds the real path under `token`; the service only ever
    /// sees the token and the name.
    FilePick { token: u32, name: Text [MAX_FS_NAME] }
}

wire_struct! {
    /// A name inside an [`FsChange`].
    FsNameView { name: Text [MAX_FS_NAME] }
}

/// A change of the other computer's files (the wire's `FsOpKind`, FILES-UI.md 7.3), as the UI asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsChange {
    MakeDir { parent: Text, name: Text },
    Rename { dir: Text, name: Text, new_name: Text },
    Move { from_dir: Text, names: Vec<FsNameView>, to_dir: Text },
    Trash { dir: Text, names: Vec<FsNameView> },
}

impl Wire for FsChange {
    const MIN_SIZE: usize = 3;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::MakeDir { parent, name } => {
                w.put_u8(0);
                parent.put(w, MAX_FS_PATH)?;
                name.put(w, MAX_FS_NAME)
            }
            Self::Rename { dir, name, new_name } => {
                w.put_u8(1);
                dir.put(w, MAX_FS_PATH)?;
                name.put(w, MAX_FS_NAME)?;
                new_name.put(w, MAX_FS_NAME)
            }
            Self::Move { from_dir, names, to_dir } => {
                w.put_u8(2);
                from_dir.put(w, MAX_FS_PATH)?;
                names.put(w, MAX_OP_NAMES)?;
                to_dir.put(w, MAX_FS_PATH)
            }
            Self::Trash { dir, names } => {
                w.put_u8(3);
                dir.put(w, MAX_FS_PATH)?;
                names.put(w, MAX_OP_NAMES)
            }
        }
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        let path = |r: &mut Reader<'_>| Text::get(r, MAX_FS_PATH);
        let name = |r: &mut Reader<'_>| Text::get(r, MAX_FS_NAME);
        Ok(match r.u8()? {
            0 => Self::MakeDir { parent: path(r)?, name: name(r)? },
            1 => Self::Rename { dir: path(r)?, name: name(r)?, new_name: name(r)? },
            2 => Self::Move { from_dir: path(r)?, names: Vec::get(r, MAX_OP_NAMES)?, to_dir: path(r)? },
            3 => Self::Trash { dir: path(r)?, names: Vec::get(r, MAX_OP_NAMES)? },
            tag => return Err(CodecError::UnknownTag { what: "FsChange", tag: u64::from(tag) }),
        })
    }
}

wire_struct! {
    /// The outcome of one name of a change: `NoError` when done.
    OpResultView { name: Text [MAX_FS_NAME], code: ErrorCode }
}

wire_struct! {
    /// The reply to `FilesOp::ListPage`: the folder as the host spells it and one page of it.
    FileListing2 { path: Text [MAX_FS_PATH], start: u32, page: FsPage }
}

/// The UI ops of the files feature (numbers 40 to 49; 49 is free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesOp {
    /// 40: list a folder of the host of the viewer session (`None` is its home); the reply is a [`FileListing`].
    List { session_id: u64, path: Option<Text> },
    /// 41: send the picked files into `remote_dir` of the host (`None` is the host's default receive folder); the
    /// reply is the transfer numbers, one per pick, in order.
    Send { session_id: u64, picks: Vec<FilePick>, remote_dir: Option<Text> },
    /// 42: download `remote_path` of the host into the receive folder the UI holds under `recv_token`; the reply is
    /// the transfer number.
    Get { session_id: u64, remote_path: Text, recv_token: u32 },
    /// 43.
    Pause { transfer_id: u32 },
    /// 44: "Retomar".
    Resume { transfer_id: u32 },
    /// 45.
    Cancel { transfer_id: u32 },
    /// 46: a page of a folder of the host (a 1.1 path); the reply is a [`FileListing2`]. `Invalid` at once toward a
    /// host of minor below 2, so the UI knows the peer is 1.0 without a field of its own.
    ListPage { session_id: u64, path: Text, start: u32 },
    /// 47: a change of the host's files; the reply is the results (`OpResults`) after the host's answer, `Timeout`
    /// after 30 s; `Invalid` toward a host of minor below 2.
    Change { session_id: u64, change: FsChange },
    /// 48: the host's home folder, places and volumes; the reply is `Volumes`; `Invalid` toward a host of minor
    /// below 2.
    Volumes { session_id: u64 },
}

impl FilesOp {
    pub const fn code(&self) -> u16 {
        match self {
            Self::List { .. } => 40,
            Self::Send { .. } => 41,
            Self::Get { .. } => 42,
            Self::Pause { .. } => 43,
            Self::Resume { .. } => 44,
            Self::Cancel { .. } => 45,
            Self::ListPage { .. } => 46,
            Self::Change { .. } => 47,
            Self::Volumes { .. } => 48,
        }
    }

    /// True for a code this module owns.
    pub const fn owns(op: u16) -> bool {
        matches!(op, 40..=49)
    }

    pub(crate) fn put_body(&self, w: &mut Writer) -> CResult<()> {
        match self {
            Self::List { session_id, path } => {
                w.put_u64(*session_id);
                path.put(w, MAX_FS_PATH)?;
            }
            Self::Send { session_id, picks, remote_dir } => {
                w.put_u64(*session_id);
                picks.put(w, MAX_PICKS)?;
                remote_dir.put(w, MAX_FS_PATH)?;
            }
            Self::Get { session_id, remote_path, recv_token } => {
                w.put_u64(*session_id);
                remote_path.put(w, MAX_FS_PATH)?;
                w.put_u32(*recv_token);
            }
            Self::Pause { transfer_id } | Self::Resume { transfer_id } | Self::Cancel { transfer_id } => {
                w.put_u32(*transfer_id)
            }
            Self::ListPage { session_id, path, start } => {
                w.put_u64(*session_id);
                path.put(w, MAX_FS_PATH)?;
                w.put_u32(*start);
            }
            Self::Change { session_id, change } => {
                w.put_u64(*session_id);
                change.put(w, 0)?;
            }
            Self::Volumes { session_id } => w.put_u64(*session_id),
        }
        Ok(())
    }

    pub(crate) fn get_body(op: u16, r: &mut Reader<'_>) -> core::result::Result<Self, IpcError> {
        Ok(match op {
            40 => Self::List { session_id: r.u64()?, path: <Option<Text>>::get(r, MAX_FS_PATH)? },
            41 => Self::Send {
                session_id: r.u64()?,
                picks: <Vec<FilePick>>::get(r, MAX_PICKS)?,
                remote_dir: <Option<Text>>::get(r, MAX_FS_PATH)?,
            },
            42 => Self::Get { session_id: r.u64()?, remote_path: Text::get(r, MAX_FS_PATH)?, recv_token: r.u32()? },
            43 => Self::Pause { transfer_id: r.u32()? },
            44 => Self::Resume { transfer_id: r.u32()? },
            45 => Self::Cancel { transfer_id: r.u32()? },
            46 => Self::ListPage { session_id: r.u64()?, path: Text::get(r, MAX_FS_PATH)?, start: r.u32()? },
            47 => Self::Change { session_id: r.u64()?, change: FsChange::get(r, 0)? },
            48 => Self::Volumes { session_id: r.u64()? },
            other => {
                return Err(IpcError::Codec(CodecError::UnknownTag { what: "UiRequest op", tag: u64::from(other) }));
            }
        })
    }
}

wire_struct! {
    /// One entry of a [`FileListing`].
    FileEntry { name: Text [MAX_FS_NAME], is_dir: bool, size: u64, mtime_ms: u64 }
}

wire_struct! {
    /// The reply to `FilesOp::List`: the folder as the host spells it (relative to its root, `/` separated, empty for
    /// the root), its entries (folders first) and whether more existed.
    FileListing { path: Text [MAX_FS_PATH], entries: Vec<FileEntry> [MAX_FS_LIST], truncated: bool }
}

// ---------- the notifications 20 and 21 ----------

wire_enum! {
    /// What a [`FilesNote::Transfer`] says about the transfer's stage; the numbers travel in the note.
    TransferKind { Queued = 0, Running = 1, Paused = 2, Done = 3, Failed = 4 }
}

const _: usize = TransferKind::ALL.len();

/// A transfer's state on the wire (the C6 `TransferState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferWire {
    Queued,
    Running { done: u64, total: u64, bytes_per_s: u64 },
    Paused { done: u64, total: u64, why: ApiCode },
    Done,
    Failed { reason: ApiCode },
}

impl Wire for TransferWire {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::Queued => TransferKind::Queued.put(w, 0)?,
            Self::Running { done, total, bytes_per_s } => {
                TransferKind::Running.put(w, 0)?;
                w.put_u64(*done);
                w.put_u64(*total);
                w.put_u64(*bytes_per_s);
            }
            Self::Paused { done, total, why } => {
                TransferKind::Paused.put(w, 0)?;
                w.put_u64(*done);
                w.put_u64(*total);
                why.put(w, 0)?;
            }
            Self::Done => TransferKind::Done.put(w, 0)?,
            Self::Failed { reason } => {
                TransferKind::Failed.put(w, 0)?;
                reason.put(w, 0)?;
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        Ok(match TransferKind::get(r, 0)? {
            TransferKind::Queued => Self::Queued,
            TransferKind::Running => Self::Running { done: r.u64()?, total: r.u64()?, bytes_per_s: r.u64()? },
            TransferKind::Paused => Self::Paused { done: r.u64()?, total: r.u64()?, why: ApiCode::get(r, 0)? },
            TransferKind::Done => Self::Done,
            TransferKind::Failed => Self::Failed { reason: ApiCode::get(r, 0)? },
        })
    }
}

/// The notifications of the files feature (kinds 20 to 24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesNote {
    /// 20: a transfer of the session changed (progress at most 4 times a second per transfer).
    Transfer { session_id: u64, transfer_id: u32, name: Text, outgoing: bool, state: TransferWire },
    /// 21: a received file was saved under `name`, which differs from the offered name because that one was taken
    /// (files never overwrite: "name (2).ext"). Sent just before the `Done` of the same transfer.
    SavedAs { session_id: u64, transfer_id: u32, name: Text },
    /// 22: the host's set of volumes changed (pushed by the host, at most once a second).
    Volumes { session_id: u64, volumes: VolumesView },
    /// 23: host side only, for the session banner (FILES-UI.md 6): the viewer made a change here. `op` is the audit's
    /// (0 new folder, 1 rename, 2 move, 3 to the bin), `count` the names done. Counts only, never a name.
    Changed { session_id: u64, op: u8, count: u32 },
    /// 24: host side only, for the session banner: the viewer reached a volume outside the home folder, sent the first
    /// time it happens in the session.
    Reach { session_id: u64 },
}

impl FilesNote {
    pub const fn kind(&self) -> u16 {
        match self {
            Self::Transfer { .. } => 20,
            Self::SavedAs { .. } => 21,
            Self::Volumes { .. } => 22,
            Self::Changed { .. } => 23,
            Self::Reach { .. } => 24,
        }
    }

    pub const fn owns(kind: u16) -> bool {
        matches!(kind, 20..=24)
    }

    pub(crate) fn put_body(&self, w: &mut Writer) -> CResult<()> {
        match self {
            Self::Transfer { session_id, transfer_id, name, outgoing, state } => {
                w.put_u64(*session_id);
                w.put_u32(*transfer_id);
                name.put(w, MAX_FS_NAME)?;
                w.put_bool(*outgoing);
                state.put(w, 0)?;
            }
            Self::SavedAs { session_id, transfer_id, name } => {
                w.put_u64(*session_id);
                w.put_u32(*transfer_id);
                name.put(w, MAX_FS_NAME)?;
            }
            Self::Volumes { session_id, volumes } => {
                w.put_u64(*session_id);
                volumes.put(w, 0)?;
            }
            Self::Changed { session_id, op, count } => {
                w.put_u64(*session_id);
                w.put_u8(*op);
                w.put_u32(*count);
            }
            Self::Reach { session_id } => w.put_u64(*session_id),
        }
        Ok(())
    }

    pub(crate) fn get_body(kind: u16, r: &mut Reader<'_>) -> core::result::Result<Self, IpcError> {
        Ok(match kind {
            20 => Self::Transfer {
                session_id: r.u64()?,
                transfer_id: r.u32()?,
                name: Text::get(r, MAX_FS_NAME)?,
                outgoing: r.bool()?,
                state: TransferWire::get(r, 0)?,
            },
            21 => Self::SavedAs { session_id: r.u64()?, transfer_id: r.u32()?, name: Text::get(r, MAX_FS_NAME)? },
            22 => Self::Volumes { session_id: r.u64()?, volumes: VolumesView::get(r, 0)? },
            23 => Self::Changed { session_id: r.u64()?, op: r.u8()?, count: r.u32()? },
            24 => Self::Reach { session_id: r.u64()? },
            other => {
                return Err(IpcError::Codec(CodecError::UnknownTag { what: "UiNotify kind", tag: u64::from(other) }));
            }
        })
    }
}

pub(crate) fn put_fs_request(w: &mut Writer, req: u32, call: &FsCall) -> CResult<()> {
    w.put_u32(req);
    call.put(w)
}

pub(crate) fn put_fs_reply(
    w: &mut Writer,
    req: u32,
    result: &core::result::Result<FsAnswer, ErrorCode>,
) -> CResult<()> {
    w.put_u32(req);
    put_result(w, result)
}
