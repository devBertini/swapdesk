//! The message set of C2.4, its type numbers and size limits, and the pure `encode_message` / `decode_message`.

use swapdesk_proto::{
    Action, CodecError, CursorPosition, CursorShape, Desktop, DisplayList, ErrorCode, InputEvent, KeyframeRequest,
    Preset, Reader, ReceiverReport, VIDEO_HEADER_LEN, VideoHeader, Wire, Writer,
};

use crate::clip::{ClipData, ClipReplyBody, MAX_CLIP_MESSAGE};
use crate::error::{IpcError, Result};
use crate::files::{FsAnswer, FsCall, MAX_FS_MESSAGE, get_result, put_fs_reply, put_fs_request};
use crate::ops::{Notify, UiOp};
use crate::redact::{Bytes, Events, Payload, Shape, Text};
use crate::term::{
    MAX_SESSION_USER, MAX_TERM_ACCOUNT, MAX_TERM_BODY, MAX_TERM_CHUNK, MAX_TERM_OPEN_BODY, MAX_TERM_SMALL_BODY,
};
use crate::types::{AudioModeKind, IpcRole, VideoBackend};

/// The local protocol version carried by `Hello` and `HelloAck`.
pub const IPC_VERSION: u16 = 1;

/// Limit of `VideoFrame` and `ViewerVideo`.
pub const MAX_VIDEO_MESSAGE: usize = 16 * 1024 * 1024;
/// Limit of `Input` and `ViewerInput`.
pub const MAX_INPUT_MESSAGE: usize = 64 * 1024;
/// Largest audio datagram (PROTOCOL.md 7.2 header plus Opus).
pub const MAX_AUDIO_DATAGRAM: usize = 1500;
/// Limit of every other message.
pub const MAX_OTHER_MESSAGE: usize = 1024 * 1024;
/// Longest `HelperFault.what`.
pub const MAX_FAULT_TEXT: usize = 256;

/// One local message. The type numbers are the `0x..` of C2.4. Types whose body holds text, input or bytes
/// have a `Debug` that prints only lengths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    // 0x01 to 0x03: both directions.
    /// 0x01, client to service; always first.
    Hello {
        role: IpcRole,
        version: u16,
        pid: u32,
    },
    /// 0x02, service to client; the answer to `Hello`.
    HelloAck {
        version: u16,
        win_session: u32,
    },
    /// 0x03, both.
    Bye {
        code: ErrorCode,
    },
    // 0x20 to 0x29: service to helper.
    StartVideo {
        display: u16,
        fps: u16,
        kbps: u32,
        quality: Preset,
        backend: VideoBackend,
        /// The codec the session picked (`Capabilities::pick_codec`): the helper encodes in it.
        codec: swapdesk_proto::Codec,
    },
    StopVideo {
        display: u16,
    },
    SetRate {
        display: u16,
        kbps: u32,
        fps: u16,
    },
    ForceKeyframe {
        display: u16,
    },
    Input {
        events: Events,
    },
    ReleaseAll,
    /// `exclude_pid` 0 means none.
    StartAudio {
        exclude_pid: u32,
    },
    StopAudio,
    SetAudioMuted {
        muted: bool,
    },
    Shutdown,
    /// 0x2A (ACCOUNTS.md 5.1, the fence): the service lets capture and input go on after the desktop switch the
    /// helper reported as `generation` (`CaptureHeld`).
    ResumeCapture {
        generation: u64,
    },
    // 0x30 to 0x39: helper to service.
    Displays(DisplayList),
    /// The header's `payload_len` equals `payload.len()`.
    VideoFrame {
        header: VideoHeader,
        payload: Payload,
    },
    CursorPos(CursorPosition),
    CursorShape(Shape),
    AudioPacket(Bytes),
    AudioMode {
        mode: AudioModeKind,
    },
    DesktopChanged(Desktop),
    SasRequest,
    HelperFault {
        code: ErrorCode,
        what: Text,
    },
    /// 0x3A (the fence): the capture lost its desktop (a switch) and holds, with input, until `ResumeCapture` of this
    /// generation. Sent ahead of any frame of the new desktop.
    CaptureHeld {
        generation: u64,
    },
    /// 0x39: what the helper chose that is worth the service's log at INFO (the encoder serving a display, why a
    /// faster one was not used, a rebuild). Same rule as `HelperFault`: never a never-logged value.
    HelperNote {
        what: Text,
    },
    // 0x40 to 0x42: UI.
    UiRequest {
        req: u32,
        op: UiOp,
    },
    /// `status` 0 is ok, else `ApiCode` + 1. `body` is the encoded [`crate::ReplyBody`] of the op the request
    /// carried (the wire has no tag for it, so only the requester can decode it).
    UiReply {
        req: u32,
        status: u16,
        body: Bytes,
    },
    UiNotify(Notify),
    // 0x50 to 0x56: viewer media and control.
    ViewerVideo {
        session_id: u64,
        header: VideoHeader,
        payload: Payload,
    },
    ViewerAudio {
        session_id: u64,
        datagram: Bytes,
    },
    ViewerCursorPos {
        session_id: u64,
        pos: CursorPosition,
    },
    ViewerCursorShape {
        session_id: u64,
        shape: Shape,
    },
    ViewerInput {
        session_id: u64,
        events: Events,
    },
    ViewerKeyframe {
        session_id: u64,
        request: KeyframeRequest,
    },
    ViewerReport {
        session_id: u64,
        report: ReceiverReport,
    },
    // 0x60 to 0x66: clipboard and quick actions (service to helper or UI, answered with the same `req`).
    /// 0x60: the clipboard's change counter.
    ClipChangeCount {
        req: u32,
    },
    /// 0x61: read the clipboard.
    ClipRead {
        req: u32,
    },
    /// 0x62: write the clipboard.
    ClipWrite {
        req: u32,
        content: ClipData,
    },
    /// 0x63, helper or UI to service: the answer to 0x60, 0x61 or 0x62.
    ClipReply {
        req: u32,
        body: ClipReplyBody,
    },
    /// 0x64, service to user helper: run a quick action.
    RunAction {
        req: u32,
        action: Action,
    },
    /// 0x65, service to user helper: hold off sleep (or stop).
    KeepAwake {
        req: u32,
        on: bool,
    },
    /// 0x66, helper to service: the answer to 0x64 or 0x65 (`NoError` when it ran).
    ActionReply {
        req: u32,
        code: ErrorCode,
    },
    // 0x70 to 0x76: the terminal proxy between the service and the user helper (C2.4 "Terminal").
    /// 0x70, service to user helper: start the user's shell. `session_user` is the SID of the user logged in at the
    /// console; the helper re-checks that its own token is that user's (and never a service account). Empty: not told.
    PtyOpen {
        id: u32,
        cols: u16,
        rows: u16,
        session_user: Text,
    },
    /// 0x71, helper to service: the answer to `PtyOpen`. `code` `NoError`: the shell runs as `account`; any other code
    /// is the refusal and `account` is empty.
    PtyOpenReply {
        id: u32,
        code: ErrorCode,
        account: Text,
    },
    /// 0x72, service to helper: keystrokes for the shell (at most [`MAX_TERM_CHUNK`] bytes).
    PtyInput {
        id: u32,
        bytes: Bytes,
    },
    /// 0x73, helper to service: shell output (at most [`MAX_TERM_CHUNK`] bytes). The service reads the pipe only as
    /// fast as the viewer takes the output, which is the back-pressure.
    PtyOutput {
        id: u32,
        bytes: Bytes,
    },
    /// 0x74, service to helper.
    PtyResize {
        id: u32,
        cols: u16,
        rows: u16,
    },
    /// 0x75, service to helper: end the shell (no `PtyExited` follows).
    PtyClose {
        id: u32,
    },
    /// 0x76, helper to service: the shell ended by itself, after its last `PtyOutput`.
    PtyExited {
        id: u32,
        code: Option<i32>,
    },
    // 0x78 and 0x79: the terminal bytes between the service and the UI (C2.4 "Terminal"). `term_id` is connection-wide.
    /// 0x78, service to UI.
    TermOutput {
        term_id: u32,
        bytes: Bytes,
    },
    /// 0x79, UI to service.
    TermInput {
        term_id: u32,
        bytes: Bytes,
    },
    // 0x80 to 0x8F: the disk proxy of the files feature (C2.4 "Files"), on a helper pipe and on the UI pipe.
    /// 0x80, service to the process that owns the disk (the user helper, or the UI of a viewer session).
    FsRequest {
        req: u32,
        call: FsCall,
    },
    /// 0x81, back; never sent for a call that is fire and forget (`CloseRead`).
    FsReply {
        req: u32,
        result: core::result::Result<FsAnswer, ErrorCode>,
    },
}

/// The body limit of message type `ty`, or `None` for an unknown type.
pub const fn limit_of(ty: u64) -> Option<usize> {
    match ty {
        0x24 | 0x54 => Some(MAX_INPUT_MESSAGE),
        0x01..=0x03 | 0x20..=0x2A | 0x30 | 0x32 | 0x33 | 0x35..=0x3A => Some(MAX_OTHER_MESSAGE),
        0x40..=0x42 | 0x52 | 0x53 | 0x55 | 0x56 => Some(MAX_OTHER_MESSAGE),
        0x31 | 0x50 => Some(MAX_VIDEO_MESSAGE),
        0x34 => Some(MAX_AUDIO_DATAGRAM),
        0x62 | 0x63 => Some(MAX_CLIP_MESSAGE),
        0x60 | 0x61 | 0x64..=0x66 => Some(MAX_OTHER_MESSAGE),
        0x80 | 0x81 => Some(MAX_FS_MESSAGE),
        // session_id (8) + datagram
        0x51 => Some(8 + MAX_AUDIO_DATAGRAM),
        0x70 | 0x71 => Some(MAX_TERM_OPEN_BODY),
        0x72 | 0x73 | 0x78 | 0x79 => Some(MAX_TERM_BODY),
        0x74..=0x76 => Some(MAX_TERM_SMALL_BODY),
        _ => None,
    }
}

fn too_large(what: &'static str, limit: usize) -> CodecError {
    CodecError::TooLarge { what, limit: limit as u64 }
}

fn put_events(w: &mut Writer, events: &Events) -> Result<()> {
    w.put_varint(events.0.len() as u64)?;
    for e in &events.0 {
        e.put(w, 0)?;
    }
    Ok(())
}

fn get_events(r: &mut Reader<'_>) -> Result<Events> {
    let count = r.count(MAX_INPUT_MESSAGE, InputEvent::MIN_SIZE)?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(InputEvent::get(r, 0)?);
    }
    Ok(Events(out))
}

/// The header of a video message. Its payload is the message's tail: the bytes that follow the body on the wire,
/// which the encoder never copies ([`encode_into`]).
fn put_video_head(w: &mut Writer, header: &VideoHeader, payload: &Payload) -> Result<()> {
    if header.payload_len as usize != payload.0.len() {
        return Err(CodecError::InvalidValue("video payload length").into());
    }
    header.put(w, 0)?;
    Ok(())
}

fn get_video(r: &mut Reader<'_>) -> Result<(VideoHeader, Payload)> {
    let header = VideoHeader::get(r, 0)?;
    let payload = r.take(r.remaining())?;
    if header.payload_len as usize != payload.len() {
        return Err(CodecError::InvalidValue("video payload length").into());
    }
    Ok((header, Payload(bytes::Bytes::copy_from_slice(payload))))
}

/// The bytes a video message of type `ty` has in front of its payload (`ViewerVideo` carries the session first), or
/// `None` for every other type.
pub(crate) const fn video_prefix(ty: u64) -> Option<usize> {
    match ty {
        0x31 => Some(VIDEO_HEADER_LEN),
        0x50 => Some(8 + VIDEO_HEADER_LEN),
        _ => None,
    }
}

/// The payload length that the header in `prefix` (the [`video_prefix`] bytes of a message of type `ty`) declares,
/// for the stream reader to check against the frame's length before it reads the payload.
pub(crate) fn video_declared(ty: u64, prefix: &[u8]) -> Result<usize> {
    let header = VideoHeader::decode(&prefix[if ty == 0x50 { 8 } else { 0 }..])?;
    Ok(header.payload_len as usize)
}

/// A video message of type `ty` from its `prefix` and the payload that followed it, which is taken as it is. The same
/// checks and errors as [`decode_message`] makes on the whole body.
pub(crate) fn video_from_parts(ty: u64, prefix: &[u8], payload: Payload) -> Result<Message> {
    let mut r = Reader::new(prefix);
    let session_id = if ty == 0x50 { Some(r.u64()?) } else { None };
    let header = VideoHeader::get(&mut r, 0)?;
    r.finish()?;
    if header.payload_len as usize != payload.0.len() {
        return Err(CodecError::InvalidValue("video payload length").into());
    }
    Ok(match session_id {
        Some(session_id) => Message::ViewerVideo { session_id, header, payload },
        None => Message::VideoFrame { header, payload },
    })
}

fn put_datagram(w: &mut Writer, d: &Bytes) -> Result<()> {
    if d.0.len() > MAX_AUDIO_DATAGRAM {
        return Err(too_large("audio datagram", MAX_AUDIO_DATAGRAM).into());
    }
    w.put_bytes(&d.0);
    Ok(())
}

fn get_datagram(r: &mut Reader<'_>) -> Result<Bytes> {
    if r.remaining() > MAX_AUDIO_DATAGRAM {
        return Err(too_large("audio datagram", MAX_AUDIO_DATAGRAM).into());
    }
    Ok(Bytes(r.take(r.remaining())?.to_vec()))
}

fn put_term_bytes(w: &mut Writer, id: u32, bytes: &Bytes) -> Result<()> {
    if bytes.0.len() > MAX_TERM_CHUNK {
        return Err(too_large("terminal chunk", MAX_TERM_CHUNK).into());
    }
    w.put_u32(id);
    w.put_bytes(&bytes.0);
    Ok(())
}

fn get_term_bytes(r: &mut Reader<'_>) -> Result<(u32, Bytes)> {
    let id = r.u32()?;
    if r.remaining() > MAX_TERM_CHUNK {
        return Err(too_large("terminal chunk", MAX_TERM_CHUNK).into());
    }
    Ok((id, Bytes(r.take(r.remaining())?.to_vec())))
}

impl Message {
    /// The type number on the wire.
    pub const fn type_code(&self) -> u64 {
        match self {
            Self::Hello { .. } => 0x01,
            Self::HelloAck { .. } => 0x02,
            Self::Bye { .. } => 0x03,
            Self::StartVideo { .. } => 0x20,
            Self::StopVideo { .. } => 0x21,
            Self::SetRate { .. } => 0x22,
            Self::ForceKeyframe { .. } => 0x23,
            Self::Input { .. } => 0x24,
            Self::ReleaseAll => 0x25,
            Self::StartAudio { .. } => 0x26,
            Self::StopAudio => 0x27,
            Self::SetAudioMuted { .. } => 0x28,
            Self::Shutdown => 0x29,
            Self::ResumeCapture { .. } => 0x2A,
            Self::CaptureHeld { .. } => 0x3A,
            Self::Displays(_) => 0x30,
            Self::VideoFrame { .. } => 0x31,
            Self::CursorPos(_) => 0x32,
            Self::CursorShape(_) => 0x33,
            Self::AudioPacket(_) => 0x34,
            Self::AudioMode { .. } => 0x35,
            Self::DesktopChanged(_) => 0x36,
            Self::SasRequest => 0x37,
            Self::HelperFault { .. } => 0x38,
            Self::HelperNote { .. } => 0x39,
            Self::UiRequest { .. } => 0x40,
            Self::UiReply { .. } => 0x41,
            Self::UiNotify(_) => 0x42,
            Self::ViewerVideo { .. } => 0x50,
            Self::ViewerAudio { .. } => 0x51,
            Self::ViewerCursorPos { .. } => 0x52,
            Self::ViewerCursorShape { .. } => 0x53,
            Self::ViewerInput { .. } => 0x54,
            Self::ViewerKeyframe { .. } => 0x55,
            Self::ViewerReport { .. } => 0x56,
            Self::ClipChangeCount { .. } => 0x60,
            Self::ClipRead { .. } => 0x61,
            Self::ClipWrite { .. } => 0x62,
            Self::ClipReply { .. } => 0x63,
            Self::RunAction { .. } => 0x64,
            Self::KeepAwake { .. } => 0x65,
            Self::ActionReply { .. } => 0x66,
            Self::PtyOpen { .. } => 0x70,
            Self::PtyOpenReply { .. } => 0x71,
            Self::PtyInput { .. } => 0x72,
            Self::PtyOutput { .. } => 0x73,
            Self::PtyResize { .. } => 0x74,
            Self::PtyClose { .. } => 0x75,
            Self::PtyExited { .. } => 0x76,
            Self::TermOutput { .. } => 0x78,
            Self::TermInput { .. } => 0x79,
            Self::FsRequest { .. } => 0x80,
            Self::FsReply { .. } => 0x81,
        }
    }

    /// Writes the body into `w` and returns its tail: the bytes that follow on the wire and are not copied into `w`
    /// (a video payload; empty for every other message).
    fn put_body<'a>(&'a self, w: &mut Writer) -> Result<&'a [u8]> {
        match self {
            Self::Hello { role, version, pid } => {
                role.put(w, 0)?;
                w.put_u16(*version);
                w.put_u32(*pid);
            }
            Self::HelloAck { version, win_session } => {
                w.put_u16(*version);
                w.put_u32(*win_session);
            }
            Self::Bye { code } => code.put(w, 0)?,
            Self::StartVideo { display, fps, kbps, quality, backend, codec } => {
                w.put_u16(*display);
                w.put_u16(*fps);
                w.put_u32(*kbps);
                quality.put(w, 0)?;
                backend.put(w, 0)?;
                codec.put(w, 0)?;
            }
            Self::StopVideo { display } | Self::ForceKeyframe { display } => w.put_u16(*display),
            Self::SetRate { display, kbps, fps } => {
                w.put_u16(*display);
                w.put_u32(*kbps);
                w.put_u16(*fps);
            }
            Self::Input { events } => put_events(w, events)?,
            Self::ReleaseAll | Self::StopAudio | Self::Shutdown | Self::SasRequest => {}
            Self::StartAudio { exclude_pid } => w.put_u32(*exclude_pid),
            Self::SetAudioMuted { muted } => w.put_bool(*muted),
            Self::Displays(list) => list.put(w, 0)?,
            Self::VideoFrame { header, payload } => {
                put_video_head(w, header, payload)?;
                return Ok(&payload.0);
            }
            Self::CursorPos(p) => p.put(w, 0)?,
            Self::CursorShape(s) => s.0.put(w, 0)?,
            Self::AudioPacket(d) => put_datagram(w, d)?,
            Self::AudioMode { mode } => mode.put(w, 0)?,
            Self::ResumeCapture { generation } | Self::CaptureHeld { generation } => w.put_u64(*generation),
            Self::DesktopChanged(d) => d.put(w, 0)?,
            Self::HelperFault { code, what } => {
                code.put(w, 0)?;
                what.put(w, MAX_FAULT_TEXT)?;
            }
            Self::HelperNote { what } => what.put(w, MAX_FAULT_TEXT)?,
            Self::UiRequest { req, op } => {
                w.put_u32(*req);
                w.put_u16(op.code());
                op.put_body(w)?;
            }
            Self::UiReply { req, status, body } => {
                w.put_u32(*req);
                w.put_u16(*status);
                w.put_bytes(&body.0);
            }
            Self::UiNotify(n) => {
                w.put_u16(n.kind());
                n.put_body(w)?;
            }
            Self::ViewerVideo { session_id, header, payload } => {
                w.put_u64(*session_id);
                put_video_head(w, header, payload)?;
                return Ok(&payload.0);
            }
            Self::ViewerAudio { session_id, datagram } => {
                w.put_u64(*session_id);
                put_datagram(w, datagram)?;
            }
            Self::ViewerCursorPos { session_id, pos } => {
                w.put_u64(*session_id);
                pos.put(w, 0)?;
            }
            Self::ViewerCursorShape { session_id, shape } => {
                w.put_u64(*session_id);
                shape.0.put(w, 0)?;
            }
            Self::ViewerInput { session_id, events } => {
                w.put_u64(*session_id);
                put_events(w, events)?;
            }
            Self::ViewerKeyframe { session_id, request } => {
                w.put_u64(*session_id);
                request.put(w, 0)?;
            }
            Self::ViewerReport { session_id, report } => {
                w.put_u64(*session_id);
                report.put(w, 0)?;
            }
            Self::ClipChangeCount { req } | Self::ClipRead { req } => w.put_u32(*req),
            Self::ClipWrite { req, content } => {
                w.put_u32(*req);
                content.put(w)?;
            }
            Self::ClipReply { req, body } => {
                w.put_u32(*req);
                body.put(w)?;
            }
            Self::RunAction { req, action } => {
                w.put_u32(*req);
                action.put(w, 0)?;
            }
            Self::KeepAwake { req, on } => {
                w.put_u32(*req);
                w.put_bool(*on);
            }
            Self::ActionReply { req, code } => {
                w.put_u32(*req);
                code.put(w, 0)?;
            }
            Self::PtyOpen { id, cols, rows, session_user } => {
                w.put_u32(*id);
                w.put_u16(*cols);
                w.put_u16(*rows);
                session_user.put(w, MAX_SESSION_USER)?;
            }
            Self::PtyOpenReply { id, code, account } => {
                w.put_u32(*id);
                code.put(w, 0)?;
                account.put(w, MAX_TERM_ACCOUNT)?;
            }
            Self::PtyInput { id, bytes }
            | Self::PtyOutput { id, bytes }
            | Self::TermOutput { term_id: id, bytes }
            | Self::TermInput { term_id: id, bytes } => {
                put_term_bytes(w, *id, bytes)?;
            }
            Self::PtyResize { id, cols, rows } => {
                w.put_u32(*id);
                w.put_u16(*cols);
                w.put_u16(*rows);
            }
            Self::PtyClose { id } => w.put_u32(*id),
            Self::PtyExited { id, code } => {
                w.put_u32(*id);
                code.put(w, 0)?;
            }
            Self::FsRequest { req, call } => put_fs_request(w, *req, call)?,
            Self::FsReply { req, result } => put_fs_reply(w, *req, result)?,
        }
        Ok(&[])
    }

    /// Decodes the whole body `body` of message type `ty` (already checked against its limit).
    fn get_body(ty: u64, body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let r = &mut r;
        let msg = match ty {
            0x01 => Self::Hello { role: IpcRole::get(r, 0)?, version: r.u16()?, pid: r.u32()? },
            0x02 => Self::HelloAck { version: r.u16()?, win_session: r.u32()? },
            0x03 => Self::Bye { code: ErrorCode::get(r, 0)? },
            0x20 => Self::StartVideo {
                display: r.u16()?,
                fps: r.u16()?,
                kbps: r.u32()?,
                quality: Preset::get(r, 0)?,
                backend: VideoBackend::get(r, 0)?,
                codec: swapdesk_proto::Codec::get(r, 0)?,
            },
            0x21 => Self::StopVideo { display: r.u16()? },
            0x22 => Self::SetRate { display: r.u16()?, kbps: r.u32()?, fps: r.u16()? },
            0x23 => Self::ForceKeyframe { display: r.u16()? },
            0x24 => Self::Input { events: get_events(r)? },
            0x25 => Self::ReleaseAll,
            0x26 => Self::StartAudio { exclude_pid: r.u32()? },
            0x27 => Self::StopAudio,
            0x28 => Self::SetAudioMuted { muted: r.bool()? },
            0x29 => Self::Shutdown,
            0x2A => Self::ResumeCapture { generation: r.u64()? },
            0x3A => Self::CaptureHeld { generation: r.u64()? },
            0x30 => Self::Displays(DisplayList::get(r, 0)?),
            0x31 => {
                let (header, payload) = get_video(r)?;
                Self::VideoFrame { header, payload }
            }
            0x32 => Self::CursorPos(CursorPosition::get(r, 0)?),
            0x33 => Self::CursorShape(Shape(CursorShape::get(r, 0)?)),
            0x34 => Self::AudioPacket(get_datagram(r)?),
            0x35 => Self::AudioMode { mode: AudioModeKind::get(r, 0)? },
            0x36 => Self::DesktopChanged(Desktop::get(r, 0)?),
            0x37 => Self::SasRequest,
            0x38 => Self::HelperFault { code: ErrorCode::get(r, 0)?, what: Text::get(r, MAX_FAULT_TEXT)? },
            0x39 => Self::HelperNote { what: Text::get(r, MAX_FAULT_TEXT)? },
            0x40 => {
                let req = r.u32()?;
                let op = r.u16()?;
                Self::UiRequest { req, op: UiOp::get_body(op, r)? }
            }
            0x41 => {
                let req = r.u32()?;
                let status = r.u16()?;
                Self::UiReply { req, status, body: Bytes(r.take(r.remaining())?.to_vec()) }
            }
            0x42 => {
                let kind = r.u16()?;
                Self::UiNotify(Notify::get_body(kind, r)?)
            }
            0x50 => {
                let session_id = r.u64()?;
                let (header, payload) = get_video(r)?;
                Self::ViewerVideo { session_id, header, payload }
            }
            0x51 => Self::ViewerAudio { session_id: r.u64()?, datagram: get_datagram(r)? },
            0x52 => Self::ViewerCursorPos { session_id: r.u64()?, pos: CursorPosition::get(r, 0)? },
            0x53 => Self::ViewerCursorShape { session_id: r.u64()?, shape: Shape(CursorShape::get(r, 0)?) },
            0x54 => Self::ViewerInput { session_id: r.u64()?, events: get_events(r)? },
            0x55 => Self::ViewerKeyframe { session_id: r.u64()?, request: KeyframeRequest::get(r, 0)? },
            0x56 => Self::ViewerReport { session_id: r.u64()?, report: ReceiverReport::get(r, 0)? },
            0x60 => Self::ClipChangeCount { req: r.u32()? },
            0x61 => Self::ClipRead { req: r.u32()? },
            0x62 => Self::ClipWrite { req: r.u32()?, content: ClipData::get(r)? },
            0x63 => Self::ClipReply { req: r.u32()?, body: ClipReplyBody::get(r)? },
            0x64 => Self::RunAction { req: r.u32()?, action: Action::get(r, 0)? },
            0x65 => Self::KeepAwake { req: r.u32()?, on: r.bool()? },
            0x66 => Self::ActionReply { req: r.u32()?, code: ErrorCode::get(r, 0)? },
            0x70 => Self::PtyOpen {
                id: r.u32()?,
                cols: r.u16()?,
                rows: r.u16()?,
                session_user: Text::get(r, MAX_SESSION_USER)?,
            },
            0x71 => Self::PtyOpenReply {
                id: r.u32()?,
                code: ErrorCode::get(r, 0)?,
                account: Text::get(r, MAX_TERM_ACCOUNT)?,
            },
            0x72 => {
                let (id, bytes) = get_term_bytes(r)?;
                Self::PtyInput { id, bytes }
            }
            0x73 => {
                let (id, bytes) = get_term_bytes(r)?;
                Self::PtyOutput { id, bytes }
            }
            0x74 => Self::PtyResize { id: r.u32()?, cols: r.u16()?, rows: r.u16()? },
            0x75 => Self::PtyClose { id: r.u32()? },
            0x76 => Self::PtyExited { id: r.u32()?, code: <Option<i32>>::get(r, 0)? },
            0x78 => {
                let (term_id, bytes) = get_term_bytes(r)?;
                Self::TermOutput { term_id, bytes }
            }
            0x79 => {
                let (term_id, bytes) = get_term_bytes(r)?;
                Self::TermInput { term_id, bytes }
            }
            0x80 => Self::FsRequest { req: r.u32()?, call: FsCall::get(r)? },
            0x81 => Self::FsReply { req: r.u32()?, result: get_result(r)? },
            other => return Err(IpcError::UnknownType(other)),
        };
        r.finish()?;
        Ok(msg)
    }
}

/// Room kept in front of a body for its type and its length (two varints of at most 8 bytes each).
const FRAME_PREFIX: usize = 16;

/// One frame, `varint type`, `varint length`, body, as two slices that follow each other on the wire: the head, built
/// in `scratch` (its old contents are discarded, its capacity is reused), and the tail, which is the message's own
/// video payload, not copied (empty for every other message). Refuses a body above the type's limit.
pub fn encode_into<'s, 'm>(msg: &'m Message, scratch: &'s mut Vec<u8>) -> Result<(&'s [u8], &'m [u8])> {
    let (start, tail) = frame_parts(msg, scratch)?;
    Ok((&scratch[start..], tail))
}

/// [`encode_into`] for a caller that goes on using `scratch`: the head is `scratch[start..]`.
pub(crate) fn frame_parts<'m>(msg: &'m Message, scratch: &mut Vec<u8>) -> Result<(usize, &'m [u8])> {
    let ty = msg.type_code();
    let limit = limit_of(ty).ok_or(IpcError::UnknownType(ty))?;
    scratch.clear();
    scratch.resize(FRAME_PREFIX, 0);
    // The writer borrows the buffer and hands it back whatever happens, so an error keeps its capacity too.
    let mut w = Writer::appending(core::mem::take(scratch));
    let body = msg.put_body(&mut w);
    let body_end = w.len();
    let framed = body.and_then(|tail| {
        let len = body_end - FRAME_PREFIX + tail.len();
        if len > limit {
            return Err(IpcError::TooLarge { ty, len: len as u64, limit: limit as u64 });
        }
        // The two varints are written after the body and then moved in front of it: nothing but `scratch` is used.
        w.put_varint(ty)?;
        w.put_varint(len as u64)?;
        Ok(tail)
    });
    *scratch = w.into_bytes();
    let tail = framed?;
    let start = FRAME_PREFIX - (scratch.len() - body_end);
    scratch.copy_within(body_end.., start);
    scratch.truncate(body_end);
    Ok((start, tail))
}

/// One frame in a buffer of its own ([`encode_into`] makes one without it).
pub fn encode_message(msg: &Message) -> Result<Vec<u8>> {
    let mut scratch = Vec::new();
    let (head, tail) = encode_into(msg, &mut scratch)?;
    let mut out = Vec::with_capacity(head.len() + tail.len());
    out.extend_from_slice(head);
    out.extend_from_slice(tail);
    Ok(out)
}

/// Decodes exactly one frame from `bytes`: an unknown type, a length above the type's limit (checked before
/// anything is copied), a length that does not match the bytes, trailing bytes and a malformed body are errors.
pub fn decode_message(bytes: &[u8]) -> Result<Message> {
    let mut r = Reader::new(bytes);
    let ty = r.varint()?;
    let len = r.varint()?;
    let limit = limit_of(ty).ok_or(IpcError::UnknownType(ty))?;
    if len > limit as u64 {
        return Err(IpcError::TooLarge { ty, len, limit: limit as u64 });
    }
    // `len` <= 16 MiB here, so the cast is exact.
    let body = r.take(len as usize)?;
    r.finish()?;
    Message::get_body(ty, body)
}

/// Checks a declared `len` of type `ty` against its limit and returns the limit; used by the stream reader
/// before it reads or allocates anything.
pub(crate) fn check_declared(ty: u64, len: u64) -> Result<usize> {
    let limit = limit_of(ty).ok_or(IpcError::UnknownType(ty))?;
    if len > limit as u64 {
        return Err(IpcError::TooLarge { ty, len, limit: limit as u64 });
    }
    Ok(len as usize)
}

pub(crate) fn decode_body(ty: u64, body: &[u8]) -> Result<Message> {
    Message::get_body(ty, body)
}
