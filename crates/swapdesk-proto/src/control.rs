//! The control stream of PROTOCOL.md section 5: every message with its type ID, and the framing
//! `type: varint, len: varint, payload`.

use crate::codec::{CodecError, Reader, Result, Wire, Writer};
use crate::pairing::{PairConfirm, PairHello, PairNonce, PairResult, PairReveal};
use crate::types::{
    Action, AudioCodec, ClipSkip, Codec, Desktop, ErrorCode, Feature, GrantMode, H264Profile, Intent, KbKind, Os,
    PermissionSet, Preset, Special,
};

/// Largest frame payload (5.1); a larger declared length is `TOO_LARGE`.
pub const MAX_FRAME_PAYLOAD: usize = 65_535;

// ---------- nested records ----------

wire_struct! {
    /// `Hello.resume`: resume the same session after a lost connection (6.4).
    Resume { session_id: u64, token: [u8; 16] => redact }
}

wire_struct! {
    VideoCap {
        codec: Codec,
        profile: H264Profile,
        max_level: u8,
        max_width: u16,
        max_height: u16,
        max_fps: u16,
    }
}

wire_struct! {
    KeyboardInfo { kind: KbKind, layout: String [64] }
}

wire_struct! {
    /// One display of the host, in physical pixels of its virtual desktop.
    Display {
        id: u16,
        name: String [64],
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        scale_milli: u16,
        refresh_mhz: u32,
        rotation: u16,
        primary: bool,
    }
}

wire_struct! {
    DirEntry { name: String [255], is_dir: bool, size: u64, mtime_ms: u64 }
}

// ---------- session ----------

wire_struct! {
    /// 0x0001, viewer to host: first message, exactly once.
    Hello {
        proto_minor: u16,
        app_version: String [32],
        name: String [64],
        os: Os,
        os_version: String [32],
        intent: Intent,
        requested: PermissionSet,
        resume: Option<Resume>,
        swap_token: Option<[u8; 16]> => redact,
    }
}

wire_struct! {
    /// 0x0002, host to viewer.
    HelloAck {
        proto_minor: u16,
        app_version: String [32],
        name: String [64],
        os: Os,
        os_version: String [32],
    }
}

wire_struct! {
    /// 0x0003, both: what this build and OS can do, not what is granted. `features` bits: [`crate::feature_bits`].
    Capabilities {
        video_decode: Vec<VideoCap> [8],
        audio: Vec<AudioCodec> [4],
        max_datagram: u16,
        keyboard: KeyboardInfo,
        features: u32,
    }
}

wire_struct! {
    /// 0x0004, host to viewer.
    PromptPending { timeout_s: u16 }
}

wire_struct! {
    /// 0x0005, host to viewer.
    SessionGrant {
        session_id: u64,
        granted: PermissionSet,
        mode: GrantMode,
        resume_token: [u8; 16] => redact,
    }
}

wire_struct! {
    /// 0x0006, host to viewer: `DECLINED`, `TIMEOUT`, `NOBODY_PRESENT`, `NOT_PERMITTED` or `BUSY`.
    SessionRefused { code: ErrorCode }
}

wire_struct! {
    /// 0x0007, host to viewer.
    PermissionsChanged { granted: PermissionSet }
}

wire_struct! {
    /// 0x0008, both.
    Bye { code: ErrorCode }
}

wire_struct! {
    /// 0x0009, host to viewer.
    HostState { desktop: Desktop, paused: bool }
}

wire_struct! {
    /// 0x000A, host to viewer (D13).
    ConsentPending { feature: Feature, timeout_s: u16 }
}

wire_struct! {
    /// 0x000B, both, minor 1: what the sender's account has stored for the peer's device, not what this session
    /// holds. Display only for the receiver.
    StoredGrants { grants: PermissionSet }
}

wire_struct! {
    /// 0x000C, viewer to host, minor 1: an unattended session asks for the elevated bits, after the system's
    /// confirmation on the viewer's machine.
    Elevate {}
}

// ---------- display and video ----------

wire_struct! {
    /// 0x0101, host to viewer.
    DisplayList { displays: Vec<Display> [16] }
}

wire_struct! {
    /// 0x0102, viewer to host.
    SelectDisplay { display: u16 }
}

wire_struct! {
    /// 0x0103, viewer to host.
    VideoSettings {
        preset: Preset,
        max_fps: u16,
        max_kbps: u32,
        view_width: u16,
        view_height: u16,
    }
}

wire_struct! {
    /// 0x0104, viewer to host.
    KeyframeRequest { display: u16, last_good_frame: u32 }
}

wire_struct! {
    /// 0x0105, viewer to host, once per second.
    ReceiverReport {
        frames_presented: u32,
        frames_lost: u32,
        decode_us_p50: u32,
        present_lag_us_p50: u32,
        audio_underruns: u32,
    }
}

wire_struct! {
    /// 0x0106, host to viewer, once per second.
    StreamStats { kbps: u32, fps: u16, encoder: String [64] }
}

wire_struct! {
    /// 0x0107, viewer to host.
    AudioControl { enabled: bool }
}

wire_struct! {
    /// 0x0108, host to viewer.
    HostMute { muted: bool }
}

wire_struct! {
    /// 0x0109, viewer to host.
    SendSpecial { key: Special }
}

// ---------- swap ----------

wire_struct! {
    /// 0x0201, either.
    SwapRequest { request_id: u32 }
}

wire_struct! {
    /// 0x0202, sent by the side that was asked.
    SwapAccept { request_id: u32, token: [u8; 16] => redact }
}

wire_struct! {
    /// 0x0203, either.
    SwapDecline { request_id: u32, code: ErrorCode }
}

wire_struct! {
    /// 0x0204, either.
    SwapCancel { request_id: u32 }
}

wire_struct! {
    /// 0x0205, either: the future host is ready for the new connection.
    SwapArmed { request_id: u32 }
}

// ---------- chat and clipboard ----------

wire_struct! {
    /// 0x0301, both.
    ChatMessage { msg_id: u32, text: String [4096] => redact, sent_at_ms: u64 }
}

wire_struct! {
    /// 0x0302, both.
    ChatRead { up_to: u32 }
}

wire_struct! {
    /// 0x0303, both.
    ClipboardSync { enabled: bool }
}

wire_struct! {
    /// 0x0304, both.
    ClipboardSkipped { reason: ClipSkip }
}

// ---------- files ----------

wire_struct! {
    /// 0x0401, sender.
    FileOffer {
        transfer_id: u32,
        name: String [255],
        size: u64,
        mtime_ms: u64,
        dest: Option<String> [1024],
    }
}

wire_struct! {
    /// 0x0402, receiver.
    FileAccept { transfer_id: u32, resume_from: u64 }
}

wire_struct! {
    /// 0x0403, receiver.
    FileDecline { transfer_id: u32, code: ErrorCode }
}

wire_struct! {
    /// 0x0404, sender, after the last data stream finished.
    FileDone { transfer_id: u32, sha256: [u8; 32] }
}

wire_struct! {
    /// 0x0405, receiver.
    FileVerified { transfer_id: u32, ok: bool }
}

wire_struct! {
    /// 0x0406, either.
    FilePause { transfer_id: u32 }
}

wire_struct! {
    /// 0x0407, either.
    FileResume { transfer_id: u32 }
}

wire_struct! {
    /// 0x0408, either.
    FileCancel { transfer_id: u32 }
}

wire_struct! {
    /// 0x0409, viewer to host; `None` is the user's home.
    DirRequest { req_id: u32, path: Option<String> [1024] }
}

wire_struct! {
    /// 0x040A, host to viewer.
    DirListing {
        req_id: u32,
        path: String [1024],
        entries: Vec<DirEntry> [2000],
        truncated: bool,
    }
}

wire_struct! {
    /// 0x040B, viewer to host.
    FileGet { transfer_id: u32, path: String [1024] }
}

// ---------- terminal ----------

wire_struct! {
    /// 0x0501, viewer to host.
    TermOpen { term_id: u32, cols: u16, rows: u16, admin: bool }
}

wire_struct! {
    /// 0x0502, host to viewer.
    TermOpened { term_id: u32, account: String [64] }
}

wire_struct! {
    /// 0x0503, viewer to host.
    TermResize { term_id: u32, cols: u16, rows: u16 }
}

wire_struct! {
    /// 0x0504, viewer to host.
    TermClose { term_id: u32 }
}

wire_struct! {
    /// 0x0505, host to viewer.
    TermExited { term_id: u32, exit_code: Option<i32> }
}

// ---------- actions and wake ----------

wire_struct! {
    /// 0x0601, viewer to host.
    QuickAction { action_id: u32, kind: Action }
}

wire_struct! {
    /// 0x0602, host to viewer.
    QuickActionResult { action_id: u32, code: ErrorCode }
}

wire_struct! {
    /// 0x0603, host to viewer.
    WakeInfo { macs: Vec<[u8; 6]> [8] }
}

// ---------- between paired accounts, outside a session ----------

wire_struct! {
    /// 0x0801, both, minor 1: the only message of a grants sync connection, sent once each way in place of
    /// `Hello`. `grants` is what the sender's account has stored for the other device.
    GrantsSync { proto_minor: u16, grants: PermissionSet }
}

// ---------- keepalive and errors ----------

wire_struct! {
    /// 0x0F01, both.
    Ping { nonce: u32, t_send_us: u64 }
}

wire_struct! {
    /// 0x0F02, both.
    Pong { nonce: u32, t_send_us: u64, t_peer_us: u64 }
}

wire_struct! {
    /// 0x0FFF, both. `about_id` is the request, transfer or terminal id it answers, 0 if none.
    ErrorMessage { code: ErrorCode, fatal: bool, about_type: u16, about_id: u32 }
}

macro_rules! messages {
    ( $( $var:ident ( $ty:ident ) = $id:literal ),+ $(,)? ) => {
        /// Every message of the control stream and of the pairing stream.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Message {
            $( $var($ty), )+
        }

        impl Message {
            /// Every type ID, in the order of the table in 5.3.
            pub const ALL_TYPE_IDS: &'static [u16] = &[$($id),+];

            /// The `u16` type ID sent as a varint.
            pub const fn type_id(&self) -> u16 {
                match self {
                    $( Self::$var(_) => $id, )+
                }
            }

            /// True for a type ID this version knows.
            pub const fn is_known_type(type_id: u64) -> bool {
                matches!(type_id, $( $id )|+)
            }

            /// The encoded fields, without the frame header.
            pub fn encode_payload(&self) -> Result<Vec<u8>> {
                match self {
                    $( Self::$var(m) => m.encode(), )+
                }
            }

            /// Decodes a whole payload: trailing bytes, an unknown type or a bad field are errors.
            pub fn decode_payload(type_id: u64, payload: &[u8]) -> Result<Self> {
                match type_id {
                    $( $id => Ok(Self::$var(<$ty as Wire>::decode(payload)?)), )+
                    tag => Err(CodecError::UnknownTag { what: "message type", tag }),
                }
            }
        }

        $(
            impl From<$ty> for Message {
                fn from(m: $ty) -> Self {
                    Self::$var(m)
                }
            }
        )+
    };
}

messages! {
    Hello(Hello) = 0x0001,
    HelloAck(HelloAck) = 0x0002,
    Capabilities(Capabilities) = 0x0003,
    PromptPending(PromptPending) = 0x0004,
    SessionGrant(SessionGrant) = 0x0005,
    SessionRefused(SessionRefused) = 0x0006,
    PermissionsChanged(PermissionsChanged) = 0x0007,
    Bye(Bye) = 0x0008,
    HostState(HostState) = 0x0009,
    ConsentPending(ConsentPending) = 0x000A,
    StoredGrants(StoredGrants) = 0x000B,
    Elevate(Elevate) = 0x000C,
    DisplayList(DisplayList) = 0x0101,
    SelectDisplay(SelectDisplay) = 0x0102,
    VideoSettings(VideoSettings) = 0x0103,
    KeyframeRequest(KeyframeRequest) = 0x0104,
    ReceiverReport(ReceiverReport) = 0x0105,
    StreamStats(StreamStats) = 0x0106,
    AudioControl(AudioControl) = 0x0107,
    HostMute(HostMute) = 0x0108,
    SendSpecial(SendSpecial) = 0x0109,
    SwapRequest(SwapRequest) = 0x0201,
    SwapAccept(SwapAccept) = 0x0202,
    SwapDecline(SwapDecline) = 0x0203,
    SwapCancel(SwapCancel) = 0x0204,
    SwapArmed(SwapArmed) = 0x0205,
    ChatMessage(ChatMessage) = 0x0301,
    ChatRead(ChatRead) = 0x0302,
    ClipboardSync(ClipboardSync) = 0x0303,
    ClipboardSkipped(ClipboardSkipped) = 0x0304,
    FileOffer(FileOffer) = 0x0401,
    FileAccept(FileAccept) = 0x0402,
    FileDecline(FileDecline) = 0x0403,
    FileDone(FileDone) = 0x0404,
    FileVerified(FileVerified) = 0x0405,
    FilePause(FilePause) = 0x0406,
    FileResume(FileResume) = 0x0407,
    FileCancel(FileCancel) = 0x0408,
    DirRequest(DirRequest) = 0x0409,
    DirListing(DirListing) = 0x040A,
    FileGet(FileGet) = 0x040B,
    TermOpen(TermOpen) = 0x0501,
    TermOpened(TermOpened) = 0x0502,
    TermResize(TermResize) = 0x0503,
    TermClose(TermClose) = 0x0504,
    TermExited(TermExited) = 0x0505,
    QuickAction(QuickAction) = 0x0601,
    QuickActionResult(QuickActionResult) = 0x0602,
    WakeInfo(WakeInfo) = 0x0603,
    PairHello(PairHello) = 0x0701,
    PairNonce(PairNonce) = 0x0702,
    PairReveal(PairReveal) = 0x0703,
    PairConfirm(PairConfirm) = 0x0704,
    PairResult(PairResult) = 0x0705,
    GrantsSync(GrantsSync) = 0x0801,
    Ping(Ping) = 0x0F01,
    Pong(Pong) = 0x0F02,
    Error(ErrorMessage) = 0x0FFF,
}

impl Message {
    /// True for the five `Pair*` messages, which travel on the pairing endpoint only.
    pub const fn is_pairing(&self) -> bool {
        matches!(self.type_id(), 0x0701..=0x0705)
    }

    /// The protocol minor that introduced the type `type_id` (PROTOCOL.md 1); 0 for every type of 1.0 and for an
    /// unknown one.
    pub const fn type_min_minor(type_id: u16) -> u16 {
        match type_id {
            0x000B | 0x000C | 0x0801 => 1,
            _ => 0,
        }
    }

    /// The lowest minor a peer must have announced to be sent this message: the newest of its type and of the
    /// enum values it carries. A sender checks it against the peer's minor before it writes the message.
    pub const fn min_minor(&self) -> u16 {
        let carried = match self {
            Self::SessionRefused(m) => m.code.min_minor(),
            Self::Bye(m) => m.code.min_minor(),
            Self::HostState(m) => m.desktop.min_minor(),
            Self::SwapDecline(m) => m.code.min_minor(),
            Self::FileDecline(m) => m.code.min_minor(),
            Self::QuickActionResult(m) => m.code.min_minor(),
            Self::PairResult(m) => m.code.min_minor(),
            Self::Error(m) => m.code.min_minor(),
            _ => 0,
        };
        let of_type = Self::type_min_minor(self.type_id());
        if carried > of_type { carried } else { of_type }
    }

    /// This message as a peer that announced `minor` may be sent it: unchanged when the peer knows all of it; with
    /// each newer enum value replaced by the older one that says the same ([`ErrorCode::for_minor`],
    /// [`Desktop::for_minor`]); `None` when the type itself is newer than the peer, which is then sent nothing.
    pub fn for_peer(self, minor: u16) -> Option<Self> {
        if Self::type_min_minor(self.type_id()) > minor {
            return None;
        }
        if self.min_minor() <= minor {
            return Some(self);
        }
        Some(match self {
            Self::SessionRefused(mut m) => {
                m.code = m.code.for_minor(minor);
                Self::SessionRefused(m)
            }
            Self::Bye(mut m) => {
                m.code = m.code.for_minor(minor);
                Self::Bye(m)
            }
            Self::HostState(mut m) => {
                // A desktop the peer does not know reads as locked, and a locked desktop it cannot see is paused.
                m.paused |= m.desktop.for_minor(minor) != m.desktop;
                m.desktop = m.desktop.for_minor(minor);
                Self::HostState(m)
            }
            Self::SwapDecline(mut m) => {
                m.code = m.code.for_minor(minor);
                Self::SwapDecline(m)
            }
            Self::FileDecline(mut m) => {
                m.code = m.code.for_minor(minor);
                Self::FileDecline(m)
            }
            Self::QuickActionResult(mut m) => {
                m.code = m.code.for_minor(minor);
                Self::QuickActionResult(m)
            }
            Self::PairResult(mut m) => {
                m.code = m.code.for_minor(minor);
                Self::PairResult(m)
            }
            Self::Error(mut m) => {
                m.code = m.code.for_minor(minor);
                Self::Error(m)
            }
            other => other,
        })
    }

    /// The whole frame: `type`, `len`, payload. A payload over [`MAX_FRAME_PAYLOAD`] is an error.
    pub fn encode_frame(&self) -> Result<Vec<u8>> {
        let payload = self.encode_payload()?;
        if payload.len() > MAX_FRAME_PAYLOAD {
            return Err(CodecError::TooLarge { what: "frame payload", limit: MAX_FRAME_PAYLOAD as u64 });
        }
        let mut w = Writer::new();
        w.put_varint(u64::from(self.type_id()))?;
        w.put_varint(payload.len() as u64)?;
        w.put_bytes(&payload);
        Ok(w.into_bytes())
    }

    /// Decodes one frame from the start of `bytes` and returns it with the bytes it took.
    /// [`CodecError::Truncated`] means the frame is not complete yet.
    pub fn decode_frame(bytes: &[u8]) -> Result<(Self, usize)> {
        let (header, header_len) = FrameHeader::decode_prefix(bytes)?;
        let mut r = Reader::new(&bytes[header_len..]);
        let payload = r.take(header.len)?;
        let msg = Self::decode_payload(u64::from(header.type_id), payload)?;
        Ok((msg, header_len + header.len))
    }

    /// Decodes `bytes` as exactly one frame.
    pub fn decode_frame_exact(bytes: &[u8]) -> Result<Self> {
        let (msg, used) = Self::decode_frame(bytes)?;
        if used == bytes.len() { Ok(msg) } else { Err(CodecError::TrailingBytes) }
    }
}

/// The `type` and `len` that start a frame, checked as soon as they are read: an unknown type or a length over
/// [`MAX_FRAME_PAYLOAD`] is refused before any payload is waited for or buffered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub type_id: u16,
    pub len: usize,
}

impl Wire for FrameHeader {
    const MIN_SIZE: usize = 2;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        if self.len > MAX_FRAME_PAYLOAD {
            return Err(CodecError::TooLarge { what: "frame payload", limit: MAX_FRAME_PAYLOAD as u64 });
        }
        w.put_varint(u64::from(self.type_id))?;
        w.put_varint(self.len as u64)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let type_id = r.varint()?;
        if !Message::is_known_type(type_id) {
            return Err(CodecError::UnknownTag { what: "message type", tag: type_id });
        }
        let len = r.varint()?;
        if len > MAX_FRAME_PAYLOAD as u64 {
            return Err(CodecError::TooLarge { what: "frame payload", limit: MAX_FRAME_PAYLOAD as u64 });
        }
        Ok(Self { type_id: type_id as u16, len: len as usize })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_ids_are_unique_and_match_the_table() {
        let mut ids = Message::ALL_TYPE_IDS.to_vec();
        assert_eq!(ids.len(), 58);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 58);
        let ping = Message::Ping(Ping { nonce: 1, t_send_us: 2 });
        assert_eq!(ping.type_id(), 0x0F01);
        assert_eq!(ping.encode_frame().unwrap()[..3], [0x4F, 0x01, 12]);
    }

    #[test]
    fn a_hello_has_the_field_order_of_5_3() {
        let hello = Hello {
            proto_minor: 0,
            app_version: "1.0".into(),
            name: "pc".into(),
            os: Os::MacOs,
            os_version: "14".into(),
            intent: Intent::Control,
            requested: PermissionSet::CONTROL,
            resume: None,
            swap_token: Some([7; 16]),
        };
        let payload = hello.encode().unwrap();
        let mut want = vec![0, 0, 3];
        want.extend_from_slice(b"1.0");
        want.extend_from_slice(&[2]);
        want.extend_from_slice(b"pc");
        want.extend_from_slice(&[1, 2]);
        want.extend_from_slice(b"14");
        want.extend_from_slice(&[1, 3, 0, 0, 1]);
        want.extend_from_slice(&[7; 16]);
        assert_eq!(payload, want);
    }

    #[test]
    fn frame_length_over_65535_is_too_large_before_the_payload_arrives() {
        let mut w = Writer::new();
        w.put_varint(0x0F01).unwrap();
        w.put_varint(65_536).unwrap();
        let bytes = w.into_bytes();
        let err = Message::decode_frame(&bytes).unwrap_err();
        assert_eq!(err, CodecError::TooLarge { what: "frame payload", limit: 65_535 });
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn an_unknown_type_is_refused_at_once() {
        let err = Message::decode_frame(&[0x3F, 0x00]).unwrap_err();
        assert!(matches!(err, CodecError::UnknownTag { what: "message type", tag: 0x3F }));
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn pairing_messages_are_flagged() {
        assert!(Message::PairConfirm(PairConfirm { matches: true }).is_pairing());
        assert!(!Message::Bye(Bye { code: ErrorCode::NoError }).is_pairing());
    }

    #[test]
    fn an_oversized_payload_cannot_be_framed() {
        let big = Message::ChatMessage(ChatMessage { msg_id: 1, text: "é".repeat(2048), sent_at_ms: 0 });
        assert!(big.encode_frame().is_ok());
        let listing = Message::DirListing(DirListing {
            req_id: 1,
            path: String::new(),
            entries: (0..2000)
                .map(|i| DirEntry { name: "n".repeat(255), is_dir: i % 2 == 0, size: 1, mtime_ms: 2 })
                .collect(),
            truncated: false,
        });
        assert!(matches!(listing.encode_frame(), Err(CodecError::TooLarge { .. })));
    }
}
