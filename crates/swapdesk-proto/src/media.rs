//! Media, input and clipboard formats of PROTOCOL.md section 4 and 7: the stream and datagram kinds, the
//! 32-byte video header, the 16-byte audio and cursor-position headers, the cursor shape, input records and
//! the clipboard item. All fixed integers are little-endian.

use crate::codec::{CodecError, Reader, Result, Wire, Writer};
use crate::types::{Codec, MouseButton};

/// Largest video payload (7.1).
pub const MAX_VIDEO_PAYLOAD: u32 = 8 * 1024 * 1024;
pub const VIDEO_HEADER_LEN: usize = 32;
pub const AUDIO_HEADER_LEN: usize = 16;
pub const CURSOR_POSITION_LEN: usize = 16;
pub const CURSOR_SHAPE_HEADER_LEN: usize = 12;
/// Largest cursor width or height (7.4).
pub const MAX_CURSOR_DIMENSION: u16 = 256;
pub const CLIPBOARD_HEADER_LEN: usize = 9;
/// Clipboard text limit (7.5).
pub const MAX_CLIPBOARD_TEXT: u32 = 1024 * 1024;
/// Clipboard PNG limit (7.5).
pub const MAX_CLIPBOARD_PNG: u32 = 16 * 1024 * 1024;
/// Longest `Text` input record (7.3).
pub const MAX_INPUT_TEXT: usize = 256;

const VIDEO_VERSION: u8 = 1;

/// What a unidirectional or terminal stream carries; the first varint of the stream (section 4).
/// The control stream (the client's first bidirectional stream) has no kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Input = 0x01,
    Video = 0x02,
    CursorShape = 0x03,
    Clipboard = 0x04,
    FileData = 0x05,
    Terminal = 0x10,
}

impl StreamKind {
    pub const ALL: &'static [Self] =
        &[Self::Input, Self::Video, Self::CursorShape, Self::Clipboard, Self::FileData, Self::Terminal];

    pub const fn tag(self) -> u8 {
        self as u8
    }

    pub const fn from_tag(tag: u64) -> Option<Self> {
        match tag {
            0x01 => Some(Self::Input),
            0x02 => Some(Self::Video),
            0x03 => Some(Self::CursorShape),
            0x04 => Some(Self::Clipboard),
            0x05 => Some(Self::FileData),
            0x10 => Some(Self::Terminal),
            _ => None,
        }
    }
}

impl Wire for StreamKind {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_varint(u64::from(self.tag()))
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let tag = r.varint()?;
        Self::from_tag(tag).ok_or(CodecError::UnknownTag { what: "stream kind", tag })
    }
}

wire_enum! {
    /// First byte of a datagram (section 4).
    DatagramKind: u8 { Audio = 0x01, CursorPosition = 0x02 }
}

fn flags_byte(r: &mut Reader<'_>, defined: u8, what: &'static str) -> Result<u8> {
    let flags = r.u8()?;
    if flags & !defined != 0 { Err(CodecError::InvalidValue(what)) } else { Ok(flags) }
}

fn reserved_zero(r: &mut Reader<'_>, n: usize, what: &'static str) -> Result<()> {
    if r.take(n)?.iter().any(|b| *b != 0) { Err(CodecError::InvalidValue(what)) } else { Ok(()) }
}

/// The 32-byte header of a video frame stream (7.1); `version` is always 1 and is not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    /// Flags bit 0.
    pub keyframe: bool,
    /// Flags bit 1: SPS/PPS included, configuration or size changed.
    pub config: bool,
    /// Flags bit 2: encoder or capture restarted.
    pub discontinuity: bool,
    pub display_id: u16,
    pub frame_id: u32,
    /// Host monotonic clock (`MonoMicros`) at capture.
    pub capture_us: u64,
    pub encode_us: u32,
    pub width: u16,
    pub height: u16,
    pub codec: Codec,
    /// Bytes of Annex B that follow, at most [`MAX_VIDEO_PAYLOAD`].
    pub payload_len: u32,
}

impl Wire for VideoHeader {
    const MIN_SIZE: usize = VIDEO_HEADER_LEN;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        if self.payload_len > MAX_VIDEO_PAYLOAD {
            return Err(CodecError::TooLarge { what: "video payload", limit: u64::from(MAX_VIDEO_PAYLOAD) });
        }
        w.put_u8(VIDEO_VERSION);
        w.put_u8(u8::from(self.keyframe) | u8::from(self.config) << 1 | u8::from(self.discontinuity) << 2);
        w.put_u16(self.display_id);
        w.put_u32(self.frame_id);
        w.put_u64(self.capture_us);
        w.put_u32(self.encode_us);
        w.put_u16(self.width);
        w.put_u16(self.height);
        self.codec.put(w, 0)?;
        w.put_bytes(&[0; 3]);
        w.put_u32(self.payload_len);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        if r.u8()? != VIDEO_VERSION {
            return Err(CodecError::InvalidValue("video header version"));
        }
        let flags = flags_byte(r, 0b111, "video header flags")?;
        let display_id = r.u16()?;
        let frame_id = r.u32()?;
        let capture_us = r.u64()?;
        let encode_us = r.u32()?;
        let width = r.u16()?;
        let height = r.u16()?;
        let codec = Codec::get(r, 0)?;
        reserved_zero(r, 3, "video header reserved bytes")?;
        let payload_len = r.u32()?;
        if payload_len > MAX_VIDEO_PAYLOAD {
            return Err(CodecError::TooLarge { what: "video payload", limit: u64::from(MAX_VIDEO_PAYLOAD) });
        }
        Ok(Self {
            keyframe: flags & 1 != 0,
            config: flags & 2 != 0,
            discontinuity: flags & 4 != 0,
            display_id,
            frame_id,
            capture_us,
            encode_us,
            width,
            height,
            codec,
            payload_len,
        })
    }
}

/// The 16-byte header of an audio datagram (7.2); the Opus packet follows. The kind byte (0x01) is implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// Flags bit 0.
    pub discontinuity: bool,
    /// Flags bit 1.
    pub muted: bool,
    pub frame_ms: u16,
    pub seq: u32,
    pub capture_us: u64,
}

impl Wire for AudioHeader {
    const MIN_SIZE: usize = AUDIO_HEADER_LEN;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        DatagramKind::Audio.put(w, 0)?;
        w.put_u8(u8::from(self.discontinuity) | u8::from(self.muted) << 1);
        w.put_u16(self.frame_ms);
        w.put_u32(self.seq);
        w.put_u64(self.capture_us);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        if DatagramKind::get(r, 0)? != DatagramKind::Audio {
            return Err(CodecError::InvalidValue("audio header kind"));
        }
        let flags = flags_byte(r, 0b11, "audio header flags")?;
        Ok(Self {
            discontinuity: flags & 1 != 0,
            muted: flags & 2 != 0,
            frame_ms: r.u16()?,
            seq: r.u32()?,
            capture_us: r.u64()?,
        })
    }
}

/// The 16-byte cursor position datagram (7.4). The kind byte (0x02) is implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    /// Flags bit 0.
    pub visible: bool,
    pub display: u16,
    pub seq: u32,
    /// 0 to 65535 across the display, as in the pointer record.
    pub x: u16,
    pub y: u16,
    pub shape_id: u32,
}

impl Wire for CursorPosition {
    const MIN_SIZE: usize = CURSOR_POSITION_LEN;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        DatagramKind::CursorPosition.put(w, 0)?;
        w.put_u8(u8::from(self.visible));
        w.put_u16(self.display);
        w.put_u32(self.seq);
        w.put_u16(self.x);
        w.put_u16(self.y);
        w.put_u32(self.shape_id);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        if DatagramKind::get(r, 0)? != DatagramKind::CursorPosition {
            return Err(CodecError::InvalidValue("cursor position kind"));
        }
        let flags = flags_byte(r, 0b1, "cursor position flags")?;
        Ok(Self {
            visible: flags & 1 != 0,
            display: r.u16()?,
            seq: r.u32()?,
            x: r.u16()?,
            y: r.u16()?,
            shape_id: r.u32()?,
        })
    }
}

fn check_cursor_dimension(v: u16) -> Result<()> {
    if v > MAX_CURSOR_DIMENSION {
        Err(CodecError::OverLimit { what: "cursor dimension", limit: u64::from(MAX_CURSOR_DIMENSION) })
    } else {
        Ok(())
    }
}

/// The fixed part of a cursor shape stream (7.4); `width × height × 4` bytes of RGBA follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorShapeHeader {
    pub shape_id: u32,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
}

impl CursorShapeHeader {
    /// Bytes of RGBA that follow the header.
    pub const fn rgba_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

impl Wire for CursorShapeHeader {
    const MIN_SIZE: usize = CURSOR_SHAPE_HEADER_LEN;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        check_cursor_dimension(self.width)?;
        check_cursor_dimension(self.height)?;
        w.put_u32(self.shape_id);
        w.put_u16(self.width);
        w.put_u16(self.height);
        w.put_u16(self.hot_x);
        w.put_u16(self.hot_y);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let shape_id = r.u32()?;
        let width = r.u16()?;
        let height = r.u16()?;
        check_cursor_dimension(width)?;
        check_cursor_dimension(height)?;
        Ok(Self { shape_id, width, height, hot_x: r.u16()?, hot_y: r.u16()? })
    }
}

/// A whole cursor shape stream: the header and exactly `width × height × 4` bytes of straight RGBA.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub header: CursorShapeHeader,
    pub rgba: Vec<u8>,
}

/// The pixels are whatever is under the pointer: `Debug` shows their size only.
impl core::fmt::Debug for CursorShape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CursorShape")
            .field("header", &self.header)
            .field("rgba", &crate::codec::RedactedLen(self.rgba.len()))
            .finish()
    }
}

impl Wire for CursorShape {
    const MIN_SIZE: usize = CURSOR_SHAPE_HEADER_LEN;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        if self.rgba.len() != self.header.rgba_len() {
            return Err(CodecError::InvalidValue("cursor RGBA length"));
        }
        self.header.put(w, 0)?;
        w.put_bytes(&self.rgba);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let header = CursorShapeHeader::get(r, 0)?;
        let rgba = r.take(header.rgba_len())?.to_vec();
        Ok(Self { header, rgba })
    }
}

wire_enum! {
    /// Clipboard item kind (7.5).
    ClipKind: u8 { Text = 1, Png = 2 }
}

impl ClipKind {
    pub const fn max_len(self) -> u32 {
        match self {
            Self::Text => MAX_CLIPBOARD_TEXT,
            Self::Png => MAX_CLIPBOARD_PNG,
        }
    }
}

fn check_clip_len(kind: ClipKind, len: u32) -> Result<()> {
    if len > kind.max_len() {
        Err(CodecError::TooLarge { what: "clipboard item", limit: u64::from(kind.max_len()) })
    } else {
        Ok(())
    }
}

/// The fixed part of a clipboard stream (7.5); `len` bytes follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardHeader {
    pub seq: u32,
    pub kind: ClipKind,
    pub len: u32,
}

impl Wire for ClipboardHeader {
    const MIN_SIZE: usize = CLIPBOARD_HEADER_LEN;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        check_clip_len(self.kind, self.len)?;
        w.put_u32(self.seq);
        self.kind.put(w, 0)?;
        w.put_u32(self.len);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let seq = r.u32()?;
        let kind = ClipKind::get(r, 0)?;
        let len = r.u32()?;
        check_clip_len(kind, len)?;
        Ok(Self { seq, kind, len })
    }
}

/// The data of a clipboard item. Text is valid UTF-8 (a NUL is allowed: 7.5 does not call this a `str`).
#[derive(Clone, PartialEq, Eq)]
pub enum ClipboardPayload {
    Text(String),
    Png(Vec<u8>),
}

/// Clipboard content is never logged (CONTRACTS.md C2.3): only the kind and the length show.
impl core::fmt::Debug for ClipboardPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Text(t) => write!(f, "Text(<redacted {} bytes>)", t.len()),
            Self::Png(p) => write!(f, "Png(<redacted {} bytes>)", p.len()),
        }
    }
}

/// A whole clipboard stream (7.5): the header and its bytes; the newest `seq` wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardItem {
    pub seq: u32,
    pub payload: ClipboardPayload,
}

impl Wire for ClipboardItem {
    const MIN_SIZE: usize = CLIPBOARD_HEADER_LEN;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        let (kind, bytes) = match &self.payload {
            ClipboardPayload::Text(s) => (ClipKind::Text, s.as_bytes()),
            ClipboardPayload::Png(b) => (ClipKind::Png, b.as_slice()),
        };
        let len = u32::try_from(bytes.len())
            .map_err(|_| CodecError::TooLarge { what: "clipboard item", limit: u64::from(kind.max_len()) })?;
        ClipboardHeader { seq: self.seq, kind, len }.put(w, 0)?;
        w.put_bytes(bytes);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let header = ClipboardHeader::get(r, 0)?;
        let bytes = r.take(header.len as usize)?;
        let payload = match header.kind {
            ClipKind::Text => {
                ClipboardPayload::Text(core::str::from_utf8(bytes).map_err(|_| CodecError::InvalidUtf8)?.to_owned())
            }
            ClipKind::Png => ClipboardPayload::Png(bytes.to_vec()),
        };
        Ok(Self { seq: header.seq, payload })
    }
}

wire_struct! {
    /// Start of a file data stream (kind 0x05): raw bytes follow to the end of the stream.
    FileDataHeader { transfer_id: u32, offset: u64 }
}

wire_struct! {
    /// Start of a terminal stream (kind 0x10): raw PTY bytes follow both ways.
    TerminalStreamHeader { term_id: u32 }
}

/// HID usage pages a key record may carry (7.3).
pub const USAGE_PAGE_KEYBOARD: u16 = 0x07;
pub const USAGE_PAGE_CONSUMER: u16 = 0x0C;

/// One input record (7.3): a tag byte and fixed fields. Records follow one another on the input stream.
#[derive(Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Physical key. `modifiers` is the HID modifier byte after the event; `locks` has bit 0 Caps, 1 Num, 2 Scroll.
    Key {
        usage_page: u16,
        usage: u16,
        down: bool,
        modifiers: u8,
        locks: u8,
    },
    /// Typed text, at most [`MAX_INPUT_TEXT`] UTF-8 bytes.
    Text(String),
    /// Absolute pointer, 0 to 65535 across the display.
    Pointer {
        display: u16,
        x: u16,
        y: u16,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// In 1/120 of a notch (`dy > 0` away from the user, `dx > 0` right).
    Wheel {
        dx: i16,
        dy: i16,
    },
    /// Every key and button up.
    ReleaseAll,
}

/// Keys and text are never logged (CONTRACTS.md C2.3: text input records; a key stream spells a password too):
/// a key shows only its direction, text only its length.
impl core::fmt::Debug for InputEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Key { down, .. } => write!(f, "Key {{ down: {down}, <redacted> }}"),
            Self::Text(t) => write!(f, "Text(<redacted {} bytes>)", t.len()),
            Self::Pointer { display, x, y } => {
                f.debug_struct("Pointer").field("display", display).field("x", x).field("y", y).finish()
            }
            Self::Button { button, down } => {
                f.debug_struct("Button").field("button", button).field("down", down).finish()
            }
            Self::Wheel { dx, dy } => f.debug_struct("Wheel").field("dx", dx).field("dy", dy).finish(),
            Self::ReleaseAll => f.write_str("ReleaseAll"),
        }
    }
}

const TAG_KEY: u8 = 0x01;
const TAG_TEXT: u8 = 0x02;
const TAG_POINTER: u8 = 0x03;
const TAG_BUTTON: u8 = 0x04;
const TAG_WHEEL: u8 = 0x05;
const TAG_RELEASE_ALL: u8 = 0x06;

fn check_key(usage_page: u16, locks: u8) -> Result<()> {
    if usage_page != USAGE_PAGE_KEYBOARD && usage_page != USAGE_PAGE_CONSUMER {
        return Err(CodecError::InvalidValue("key usage page"));
    }
    if locks & !0b111 != 0 {
        return Err(CodecError::InvalidValue("key lock bits"));
    }
    Ok(())
}

impl Wire for InputEvent {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        match self {
            Self::Key { usage_page, usage, down, modifiers, locks } => {
                check_key(*usage_page, *locks)?;
                w.put_u8(TAG_KEY);
                w.put_u16(*usage_page);
                w.put_u16(*usage);
                w.put_bool(*down);
                w.put_u8(*modifiers);
                w.put_u8(*locks);
            }
            Self::Text(text) => {
                if text.len() > MAX_INPUT_TEXT {
                    return Err(CodecError::OverLimit { what: "input text", limit: MAX_INPUT_TEXT as u64 });
                }
                w.put_u8(TAG_TEXT);
                w.put_u16(text.len() as u16);
                w.put_bytes(text.as_bytes());
            }
            Self::Pointer { display, x, y } => {
                w.put_u8(TAG_POINTER);
                w.put_u16(*display);
                w.put_u16(*x);
                w.put_u16(*y);
            }
            Self::Button { button, down } => {
                w.put_u8(TAG_BUTTON);
                button.put(w, 0)?;
                w.put_bool(*down);
            }
            Self::Wheel { dx, dy } => {
                w.put_u8(TAG_WHEEL);
                w.put_i16(*dx);
                w.put_i16(*dy);
            }
            Self::ReleaseAll => w.put_u8(TAG_RELEASE_ALL),
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        match r.u8()? {
            TAG_KEY => {
                let usage_page = r.u16()?;
                let usage = r.u16()?;
                let down = r.bool()?;
                let modifiers = r.u8()?;
                let locks = r.u8()?;
                check_key(usage_page, locks)?;
                Ok(Self::Key { usage_page, usage, down, modifiers, locks })
            }
            TAG_TEXT => {
                let len = usize::from(r.u16()?);
                if len > MAX_INPUT_TEXT {
                    return Err(CodecError::OverLimit { what: "input text", limit: MAX_INPUT_TEXT as u64 });
                }
                let bytes = r.take(len)?;
                Ok(Self::Text(core::str::from_utf8(bytes).map_err(|_| CodecError::InvalidUtf8)?.to_owned()))
            }
            TAG_POINTER => Ok(Self::Pointer { display: r.u16()?, x: r.u16()?, y: r.u16()? }),
            TAG_BUTTON => Ok(Self::Button { button: MouseButton::get(r, 0)?, down: r.bool()? }),
            TAG_WHEEL => Ok(Self::Wheel { dx: r.i16()?, dy: r.i16()? }),
            TAG_RELEASE_ALL => Ok(Self::ReleaseAll),
            tag => Err(CodecError::UnknownTag { what: "input record", tag: u64::from(tag) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_video() -> VideoHeader {
        VideoHeader {
            keyframe: true,
            config: false,
            discontinuity: true,
            display_id: 0x0102,
            frame_id: 0x0304_0506,
            capture_us: 0x0708_090A_0B0C_0D0E,
            encode_us: 0x0F10_1112,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            payload_len: 4096,
        }
    }

    #[test]
    fn video_header_layout_matches_7_1() {
        let b = sample_video().encode().unwrap();
        assert_eq!(b.len(), VIDEO_HEADER_LEN);
        assert_eq!(b[0], 1);
        assert_eq!(b[1], 0b101);
        assert_eq!(&b[2..4], &[0x02, 0x01]);
        assert_eq!(&b[4..8], &[0x06, 0x05, 0x04, 0x03]);
        assert_eq!(&b[8..16], &[0x0E, 0x0D, 0x0C, 0x0B, 0x0A, 0x09, 0x08, 0x07]);
        assert_eq!(&b[16..20], &[0x12, 0x11, 0x10, 0x0F]);
        assert_eq!(&b[20..22], &1920u16.to_le_bytes());
        assert_eq!(&b[22..24], &1080u16.to_le_bytes());
        assert_eq!(b[24], 1);
        assert_eq!(&b[25..28], &[0, 0, 0]);
        assert_eq!(&b[28..32], &4096u32.to_le_bytes());
    }

    #[test]
    fn video_header_rejects_what_7_1_forbids() {
        let good = sample_video().encode().unwrap();
        for (off, val) in [(0, 2u8), (1, 0b1000), (24, 2), (25, 1), (27, 1)] {
            let mut b = good.clone();
            b[off] = val;
            assert!(VideoHeader::decode(&b).is_err(), "offset {off}");
        }
        let mut big = sample_video();
        big.payload_len = MAX_VIDEO_PAYLOAD + 1;
        assert!(matches!(big.encode(), Err(CodecError::TooLarge { .. })));
        let mut b = good;
        b[28..32].copy_from_slice(&(MAX_VIDEO_PAYLOAD + 1).to_le_bytes());
        assert!(matches!(VideoHeader::decode(&b), Err(CodecError::TooLarge { .. })));
    }

    #[test]
    fn audio_and_cursor_headers_are_16_bytes_with_their_kind() {
        let a =
            AudioHeader { discontinuity: false, muted: true, frame_ms: 20, seq: 7, capture_us: 9 }.encode().unwrap();
        assert_eq!(a.len(), AUDIO_HEADER_LEN);
        assert_eq!((a[0], a[1]), (0x01, 0b10));
        let c = CursorPosition { visible: true, display: 1, seq: 2, x: 3, y: 4, shape_id: 5 }.encode().unwrap();
        assert_eq!(c.len(), CURSOR_POSITION_LEN);
        assert_eq!((c[0], c[1]), (0x02, 1));
        assert!(AudioHeader::decode(&c).is_err());
        assert!(CursorPosition::decode(&a).is_err());
    }

    #[test]
    fn input_records_match_7_3() {
        let key = InputEvent::Key { usage_page: 7, usage: 4, down: true, modifiers: 0x02, locks: 1 };
        assert_eq!(key.encode().unwrap(), [0x01, 7, 0, 4, 0, 1, 0x02, 1]);
        assert_eq!(InputEvent::Text("a".into()).encode().unwrap(), [0x02, 1, 0, b'a']);
        assert_eq!(InputEvent::Pointer { display: 1, x: 2, y: 3 }.encode().unwrap(), [0x03, 1, 0, 2, 0, 3, 0]);
        assert_eq!(InputEvent::Button { button: MouseButton::Right, down: false }.encode().unwrap(), [0x04, 2, 0]);
        assert_eq!(InputEvent::Wheel { dx: -1, dy: 120 }.encode().unwrap(), [0x05, 0xFF, 0xFF, 120, 0]);
        assert_eq!(InputEvent::ReleaseAll.encode().unwrap(), [0x06]);
    }

    #[test]
    fn input_stream_prefix_decoding_reports_what_it_used() {
        let mut bytes = InputEvent::ReleaseAll.encode().unwrap();
        bytes.extend(InputEvent::Wheel { dx: 1, dy: 2 }.encode().unwrap());
        let (first, used) = InputEvent::decode_prefix(&bytes).unwrap();
        assert_eq!((first, used), (InputEvent::ReleaseAll, 1));
        let (second, used) = InputEvent::decode_prefix(&bytes[1..]).unwrap();
        assert_eq!((second, used), (InputEvent::Wheel { dx: 1, dy: 2 }, 5));
    }

    #[test]
    fn input_text_over_256_bytes_is_refused_both_ways() {
        assert!(InputEvent::Text("a".repeat(257)).encode().is_err());
        assert!(InputEvent::Text("a".repeat(256)).encode().is_ok());
        let mut b = vec![0x02];
        b.extend_from_slice(&257u16.to_le_bytes());
        b.extend(core::iter::repeat_n(b'a', 257));
        assert!(matches!(InputEvent::decode(&b), Err(CodecError::OverLimit { .. })));
    }

    #[test]
    fn stream_kind_is_a_varint() {
        assert_eq!(StreamKind::Terminal.encode().unwrap(), [0x10]);
        assert!(matches!(StreamKind::decode(&[0x06]), Err(CodecError::UnknownTag { .. })));
        assert_eq!(StreamKind::decode(&[0x40, 0x10]), Err(CodecError::NonMinimalVarint));
    }

    #[test]
    fn clipboard_limits_are_checked_before_reading() {
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes());
        b.push(1);
        b.extend_from_slice(&(MAX_CLIPBOARD_TEXT + 1).to_le_bytes());
        assert!(matches!(ClipboardItem::decode(&b), Err(CodecError::TooLarge { .. })));
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes());
        b.push(2);
        b.extend_from_slice(&MAX_CLIPBOARD_PNG.to_le_bytes());
        assert_eq!(ClipboardItem::decode(&b), Err(CodecError::Truncated));
    }

    #[test]
    fn cursor_shape_checks_dimensions_and_data_length() {
        let header = CursorShapeHeader { shape_id: 1, width: 2, height: 3, hot_x: 0, hot_y: 1 };
        let shape = CursorShape { header, rgba: vec![7; 24] };
        let bytes = shape.encode().unwrap();
        assert_eq!(CursorShape::decode(&bytes).unwrap(), shape);
        assert!(CursorShape { header, rgba: vec![0; 23] }.encode().is_err());
        let huge = CursorShapeHeader { width: 257, ..header };
        assert!(huge.encode().is_err());
        let mut b = bytes;
        b[4..6].copy_from_slice(&257u16.to_le_bytes());
        assert!(CursorShape::decode(&b).is_err());
    }
}
