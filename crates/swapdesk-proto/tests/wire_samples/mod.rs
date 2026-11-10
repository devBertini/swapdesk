//! The sample table of the 1.0 wire fixtures (PROTOCOL.md, "Wire fixtures of 1.0").
//!
//! One explicit value per wire item, fixed in this source (no randomness, no clock). This one file compiles
//! unchanged in the tree of tag `v1.0.0`, where `generate.rs` writes the fixtures from the 1.0 code, and in the
//! current tree, where `wire_1_0.rs` checks them. It uses only the public API that exists at that tag.
//!
//! Every sample is an [`Item`]: what the wire carries (a control frame, a stream start, a datagram, a record, a
//! byte string both peers hash), with its encoder and its decoder.

#![allow(dead_code)]

use core::ops::Range;

use swapdesk_proto::*;

// ---------- the items ----------

/// A wire item as a peer receives it.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A control or pairing frame: `type`, `len`, payload.
    Frame(Message),
    /// The `type` and `len` that start a frame.
    FrameHeader(FrameHeader),
    /// The input stream: kind 0x01, then records.
    InputStream(Vec<InputEvent>),
    /// The start of a video frame stream: kind 0x02, then the 32-byte header (the Annex B bytes follow).
    VideoStream(VideoHeader),
    /// An audio datagram header (the Opus packet follows).
    AudioDatagram(AudioHeader),
    /// A cursor position datagram.
    CursorPositionDatagram(CursorPosition),
    /// A whole cursor shape stream: kind 0x03, header, RGBA.
    CursorShapeStream(CursorShape),
    /// A whole clipboard stream: kind 0x04, header, bytes.
    ClipboardStream(ClipboardItem),
    /// The start of a file data stream: kind 0x05, `transfer_id`, `offset` (raw bytes follow).
    FileDataStream(FileDataHeader),
    /// The start of a terminal stream: kind 0x10, `term_id` (raw PTY bytes follow).
    TerminalStream(TerminalStreamHeader),
    /// `COMMIT_LABEL || spki_I || nonce_I`, the bytes that `PairHello.commit` hashes.
    CommitPreimage { spki_i: [u8; SPKI_LEN], nonce_i: [u8; NONCE_LEN] },
    /// `SAS_LABEL || ek || spki_I || spki_R || nonce_I || nonce_R`, the bytes that the SAS hashes.
    SasPreimage {
        ek: [u8; EXPORTER_LEN],
        spki_i: [u8; SPKI_LEN],
        spki_r: [u8; SPKI_LEN],
        nonce_i: [u8; NONCE_LEN],
        nonce_r: [u8; NONCE_LEN],
    },
    /// A SAS digest and the code shown for it: the digest, the value as `u32` little-endian, the text "482 913".
    SasDigits { digest: [u8; 32] },
    /// A constant both peers must agree on (ALPN, labels, ports, limits), as its bytes.
    Constant(Vec<u8>),
}

fn with_kind(kind: StreamKind, body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = kind.encode().map_err(|e| e.to_string())?;
    out.extend_from_slice(body);
    Ok(out)
}

fn after_kind(kind: StreamKind, bytes: &[u8]) -> Result<&[u8], String> {
    let (got, used) = StreamKind::decode_prefix(bytes).map_err(|e| e.to_string())?;
    if got != kind {
        return Err(format!("stream kind {got:?}, wanted {kind:?}"));
    }
    Ok(&bytes[used..])
}

fn split<const N: usize>(rest: &mut &[u8]) -> Result<[u8; N], String> {
    if rest.len() < N {
        return Err("preimage too short".into());
    }
    let (head, tail) = rest.split_at(N);
    *rest = tail;
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    Ok(out)
}

fn take_label<'a>(bytes: &'a [u8], label: &[u8]) -> Result<&'a [u8], String> {
    bytes.strip_prefix(label).ok_or_else(|| "preimage label differs".to_string())
}

fn sas_digits_bytes(digest: &[u8; 32]) -> Vec<u8> {
    let sas = Sas::from_digest(digest);
    let mut out = digest.to_vec();
    out.extend_from_slice(&sas.value().to_le_bytes());
    out.extend_from_slice(sas.to_string().as_bytes());
    out
}

impl Item {
    /// The bytes the 1.0 code puts on the wire for this item.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let err = |e: CodecError| e.to_string();
        match self {
            Item::Frame(m) => m.encode_frame().map_err(err),
            Item::FrameHeader(h) => h.encode().map_err(err),
            Item::InputStream(records) => {
                let mut body = Vec::new();
                for r in records {
                    body.extend(r.encode().map_err(err)?);
                }
                with_kind(StreamKind::Input, &body)
            }
            Item::VideoStream(h) => with_kind(StreamKind::Video, &h.encode().map_err(err)?),
            Item::AudioDatagram(h) => h.encode().map_err(err),
            Item::CursorPositionDatagram(p) => p.encode().map_err(err),
            Item::CursorShapeStream(s) => with_kind(StreamKind::CursorShape, &s.encode().map_err(err)?),
            Item::ClipboardStream(c) => with_kind(StreamKind::Clipboard, &c.encode().map_err(err)?),
            Item::FileDataStream(h) => with_kind(StreamKind::FileData, &h.encode().map_err(err)?),
            Item::TerminalStream(h) => with_kind(StreamKind::Terminal, &h.encode().map_err(err)?),
            Item::CommitPreimage { spki_i, nonce_i } => Ok(CommitmentInput { spki_i, nonce_i }.preimage()),
            Item::SasPreimage { ek, spki_i, spki_r, nonce_i, nonce_r } => {
                Ok(SasInput { ek, spki_i, spki_r, nonce_i, nonce_r }.preimage())
            }
            Item::SasDigits { digest } => Ok(sas_digits_bytes(digest)),
            Item::Constant(bytes) => Ok(bytes.clone()),
        }
    }

    /// Reads `bytes` the way a receiver does, into an item of the same kind as `self`.
    pub fn decode_like(&self, bytes: &[u8]) -> Result<Item, String> {
        let err = |e: CodecError| e.to_string();
        Ok(match self {
            Item::Frame(_) => Item::Frame(Message::decode_frame_exact(bytes).map_err(err)?),
            Item::FrameHeader(_) => Item::FrameHeader(FrameHeader::decode(bytes).map_err(err)?),
            Item::InputStream(_) => {
                let mut rest = after_kind(StreamKind::Input, bytes)?;
                let mut records = Vec::new();
                while !rest.is_empty() {
                    let (r, used) = InputEvent::decode_prefix(rest).map_err(err)?;
                    records.push(r);
                    rest = &rest[used..];
                }
                Item::InputStream(records)
            }
            Item::VideoStream(_) => {
                Item::VideoStream(VideoHeader::decode(after_kind(StreamKind::Video, bytes)?).map_err(err)?)
            }
            Item::AudioDatagram(_) => Item::AudioDatagram(AudioHeader::decode(bytes).map_err(err)?),
            Item::CursorPositionDatagram(_) => {
                Item::CursorPositionDatagram(CursorPosition::decode(bytes).map_err(err)?)
            }
            Item::CursorShapeStream(_) => {
                Item::CursorShapeStream(CursorShape::decode(after_kind(StreamKind::CursorShape, bytes)?).map_err(err)?)
            }
            Item::ClipboardStream(_) => {
                Item::ClipboardStream(ClipboardItem::decode(after_kind(StreamKind::Clipboard, bytes)?).map_err(err)?)
            }
            Item::FileDataStream(_) => {
                Item::FileDataStream(FileDataHeader::decode(after_kind(StreamKind::FileData, bytes)?).map_err(err)?)
            }
            Item::TerminalStream(_) => Item::TerminalStream(
                TerminalStreamHeader::decode(after_kind(StreamKind::Terminal, bytes)?).map_err(err)?,
            ),
            Item::CommitPreimage { .. } => {
                let mut rest = take_label(bytes, COMMIT_LABEL)?;
                let spki_i = split::<SPKI_LEN>(&mut rest)?;
                let nonce_i = split::<NONCE_LEN>(&mut rest)?;
                if !rest.is_empty() {
                    return Err("preimage too long".into());
                }
                Item::CommitPreimage { spki_i, nonce_i }
            }
            Item::SasPreimage { .. } => {
                let mut rest = take_label(bytes, SAS_LABEL)?;
                let ek = split::<EXPORTER_LEN>(&mut rest)?;
                let spki_i = split::<SPKI_LEN>(&mut rest)?;
                let spki_r = split::<SPKI_LEN>(&mut rest)?;
                let nonce_i = split::<NONCE_LEN>(&mut rest)?;
                let nonce_r = split::<NONCE_LEN>(&mut rest)?;
                if !rest.is_empty() {
                    return Err("preimage too long".into());
                }
                Item::SasPreimage { ek, spki_i, spki_r, nonce_i, nonce_r }
            }
            Item::SasDigits { .. } => {
                let mut rest = bytes;
                let digest = split::<32>(&mut rest)?;
                // The code is derived: the receiver recomputes it, and the fixture must agree.
                if rest != &sas_digits_bytes(&digest)[32..] {
                    return Err("the code shown for the digest differs".into());
                }
                Item::SasDigits { digest }
            }
            Item::Constant(_) => Item::Constant(bytes.to_vec()),
        })
    }

    /// The family of the index.
    pub fn family(&self) -> &'static str {
        match self {
            Item::Frame(m) if m.is_pairing() => "pairing",
            Item::Frame(_) => "control",
            Item::FrameHeader(_) => "framing",
            Item::InputStream(_) => "input",
            Item::VideoStream(_) => "video",
            Item::AudioDatagram(_) => "audio",
            Item::CursorPositionDatagram(_) | Item::CursorShapeStream(_) => "cursor",
            Item::ClipboardStream(_) => "clipboard",
            Item::FileDataStream(_) => "file-stream",
            Item::TerminalStream(_) => "terminal-stream",
            Item::CommitPreimage { .. } | Item::SasPreimage { .. } | Item::SasDigits { .. } => "pairing-math",
            Item::Constant(_) => "constant",
        }
    }

    /// What the item is, for the index.
    pub fn describe(&self) -> String {
        match self {
            Item::Frame(m) => format!("frame type {:#06x}", m.type_id()),
            Item::FrameHeader(h) => format!("frame header type {:#06x}", h.type_id),
            Item::InputStream(r) => format!("stream kind 0x01, {} record(s)", r.len()),
            Item::VideoStream(_) => "stream kind 0x02, video header".into(),
            Item::AudioDatagram(_) => "datagram kind 0x01, audio header".into(),
            Item::CursorPositionDatagram(_) => "datagram kind 0x02, cursor position".into(),
            Item::CursorShapeStream(_) => "stream kind 0x03, cursor shape".into(),
            Item::ClipboardStream(_) => "stream kind 0x04, clipboard item".into(),
            Item::FileDataStream(_) => "stream kind 0x05, file data header".into(),
            Item::TerminalStream(_) => "stream kind 0x10, terminal header".into(),
            Item::CommitPreimage { .. } => "commitment preimage".into(),
            Item::SasPreimage { .. } => "SAS preimage".into(),
            Item::SasDigits { .. } => "SAS digest and the six digits".into(),
            Item::Constant(b) => format!("constant, {} byte(s)", b.len()),
        }
    }

    /// The type ID, for a frame.
    pub fn type_id(&self) -> Option<u16> {
        match self {
            Item::Frame(m) => Some(m.type_id()),
            _ => None,
        }
    }

    /// The stream kind a stream item starts with.
    pub fn stream_kind(&self) -> Option<StreamKind> {
        match self {
            Item::InputStream(_) => Some(StreamKind::Input),
            Item::VideoStream(_) => Some(StreamKind::Video),
            Item::CursorShapeStream(_) => Some(StreamKind::CursorShape),
            Item::ClipboardStream(_) => Some(StreamKind::Clipboard),
            Item::FileDataStream(_) => Some(StreamKind::FileData),
            Item::TerminalStream(_) => Some(StreamKind::Terminal),
            _ => None,
        }
    }

    /// The datagram kind a datagram item starts with.
    pub fn datagram_kind(&self) -> Option<DatagramKind> {
        match self {
            Item::AudioDatagram(_) => Some(DatagramKind::Audio),
            Item::CursorPositionDatagram(_) => Some(DatagramKind::CursorPosition),
            _ => None,
        }
    }

    /// The input records of an input stream item.
    pub fn input_records(&self) -> &[InputEvent] {
        match self {
            Item::InputStream(r) => r,
            _ => &[],
        }
    }

    /// For `Hello` and `HelloAck`: the same item with the sender's own minor, as a newer build sends it.
    pub fn with_own_minor(&self, minor: u16) -> Item {
        match self {
            Item::Frame(Message::Hello(h)) => Item::Frame(Message::Hello(Hello { proto_minor: minor, ..h.clone() })),
            Item::Frame(Message::HelloAck(h)) => {
                Item::Frame(Message::HelloAck(HelloAck { proto_minor: minor, ..h.clone() }))
            }
            other => other.clone(),
        }
    }

    /// The bytes of `proto_minor` inside the encoded item (`Hello` and `HelloAck`: the first payload field).
    pub fn own_minor_range(&self, bytes: &[u8]) -> Option<Range<usize>> {
        match self {
            Item::Frame(Message::Hello(_) | Message::HelloAck(_)) => {
                let (_, header_len) = FrameHeader::decode_prefix(bytes).ok()?;
                Some(header_len..header_len + 2)
            }
            _ => None,
        }
    }
}

// ---------- the samples ----------

pub struct Sample {
    pub name: String,
    pub item: Item,
}

/// The fixture of a sample is `<name>.hex`.
pub fn file_name(name: &str) -> String {
    format!("{name}.hex")
}

/// 16 bytes per line, lowercase, one final newline: a changed byte changes one line.
pub fn to_hex_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    for line in bytes.chunks(16) {
        for b in line {
            out.push_str(&format!("{b:02x}"));
        }
        out.push('\n');
    }
    out
}

pub fn from_hex_text(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(format!("not a lowercase hex digit: {:?}", c as char)),
    };
    digits.chunks(2).map(|p| Ok(nibble(p[0])? << 4 | nibble(p[1])?)).collect()
}

/// The index: one line per sample, `name`, `family`, `what` separated by tabs, in table order.
pub fn index_text(samples: &[Sample]) -> String {
    let mut out = String::from("# name\tfamily\twhat\n");
    for s in samples {
        out.push_str(&format!("{}\t{}\t{}\n", s.name, s.item.family(), s.item.describe()));
    }
    out
}

struct Table(Vec<Sample>);

impl Table {
    fn add(&mut self, name: impl Into<String>, item: Item) {
        self.0.push(Sample { name: name.into(), item });
    }

    fn msg(&mut self, name: impl Into<String>, m: impl Into<Message>) {
        self.add(name, Item::Frame(m.into()));
    }
}

// Explicit lists of every value of each wire enum the 1.0 code has. The samples are built from these lists, and
// `enum_coverage` hands them to the completeness test next to the crate's own `ALL`.
const OS: &[Os] = &[Os::Windows, Os::MacOs];
const CODEC: &[Codec] = &[Codec::H264];
const AUDIO_CODEC: &[AudioCodec] = &[AudioCodec::Opus48Stereo];
const INTENT: &[Intent] = &[Intent::View, Intent::Control];
const H264_PROFILE: &[H264Profile] = &[H264Profile::Main, H264Profile::High];
const KB_KIND: &[KbKind] = &[KbKind::Ansi, KbKind::Iso, KbKind::Jis, KbKind::Abnt2, KbKind::Unknown];
const GRANT_MODE: &[GrantMode] = &[GrantMode::Prompted, GrantMode::Unattended, GrantMode::Resumed, GrantMode::Swap];
const DESKTOP: &[Desktop] = &[Desktop::Default, Desktop::Secure, Desktop::Locked, Desktop::LoginWindow];
const FEATURE: &[Feature] = &[Feature::Terminal, Feature::Files];
const PRESET: &[Preset] = &[Preset::Auto, Preset::Text, Preset::Motion];
const SPECIAL: &[Special] = &[Special::SecureAttention];
const CLIP_SKIP: &[ClipSkip] = &[ClipSkip::Concealed, ClipSkip::TooLarge, ClipSkip::Unsupported];
const ACTION: &[Action] = &[Action::Lock, Action::Sleep, Action::Restart, Action::TaskMonitor];
const MOUSE_BUTTON: &[MouseButton] =
    &[MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::Back, MouseButton::Forward];
const CLIP_KIND: &[ClipKind] = &[ClipKind::Text, ClipKind::Png];
const ERROR_CODE: &[ErrorCode] = &[
    ErrorCode::NoError,
    ErrorCode::ProtocolViolation,
    ErrorCode::UnsupportedVersion,
    ErrorCode::UnknownDevice,
    ErrorCode::NotPermitted,
    ErrorCode::NobodyPresent,
    ErrorCode::Declined,
    ErrorCode::Timeout,
    ErrorCode::Busy,
    ErrorCode::PairingLocked,
    ErrorCode::CommitMismatch,
    ErrorCode::SasRejected,
    ErrorCode::TooLarge,
    ErrorCode::NotFound,
    ErrorCode::IoFailed,
    ErrorCode::Unavailable,
    ErrorCode::Swapped,
    ErrorCode::HostShutdown,
    ErrorCode::Forgotten,
    ErrorCode::Internal,
    ErrorCode::InvalidName,
    ErrorCode::AlreadyPaired,
];

/// One wire enum for the completeness test: its name, the tags the samples use, the tags the crate defines.
pub struct EnumCoverage {
    pub name: &'static str,
    pub sampled: Vec<u64>,
    pub defined: Vec<u64>,
}

fn cover<T: Copy>(name: &'static str, sampled: &[T], all: &[T], tag: impl Fn(T) -> u64) -> EnumCoverage {
    EnumCoverage {
        name,
        sampled: sampled.iter().map(|v| tag(*v)).collect(),
        defined: all.iter().map(|v| tag(*v)).collect(),
    }
}

/// Every wire enum with the values its samples carry and the values the current crate defines.
pub fn enum_coverage() -> Vec<EnumCoverage> {
    vec![
        cover("Os", OS, Os::ALL, |v| u64::from(v.tag())),
        cover("Codec", CODEC, Codec::ALL, |v| u64::from(v.tag())),
        cover("AudioCodec", AUDIO_CODEC, AudioCodec::ALL, |v| u64::from(v.tag())),
        cover("Intent", INTENT, Intent::ALL, |v| u64::from(v.tag())),
        cover("H264Profile", H264_PROFILE, H264Profile::ALL, |v| u64::from(v.tag())),
        cover("KbKind", KB_KIND, KbKind::ALL, |v| u64::from(v.tag())),
        cover("GrantMode", GRANT_MODE, GrantMode::ALL, |v| u64::from(v.tag())),
        cover("Desktop", DESKTOP, Desktop::ALL, |v| u64::from(v.tag())),
        cover("Feature", FEATURE, Feature::ALL, |v| u64::from(v.tag())),
        cover("Preset", PRESET, Preset::ALL, |v| u64::from(v.tag())),
        cover("Special", SPECIAL, Special::ALL, |v| u64::from(v.tag())),
        cover("ClipSkip", CLIP_SKIP, ClipSkip::ALL, |v| u64::from(v.tag())),
        cover("Action", ACTION, Action::ALL, |v| u64::from(v.tag())),
        cover("MouseButton", MOUSE_BUTTON, MouseButton::ALL, |v| u64::from(v.tag())),
        cover("ClipKind", CLIP_KIND, ClipKind::ALL, |v| u64::from(v.tag())),
        cover("ErrorCode", ERROR_CODE, ErrorCode::ALL, |v| u64::from(v.tag())),
    ]
}

/// Every permission set the samples carry, by name.
const PERMISSION_SETS: &[(&str, PermissionSet)] = &[
    ("empty", PermissionSet::EMPTY),
    ("view", PermissionSet::VIEW),
    ("control", PermissionSet::CONTROL),
    ("audio", PermissionSet::AUDIO),
    ("terminal", PermissionSet::TERMINAL),
    ("admin_terminal", PermissionSet::ADMIN_TERMINAL),
    ("files", PermissionSet::FILES),
    ("clipboard", PermissionSet::CLIPBOARD),
    ("unattended", PermissionSet::UNATTENDED),
    ("all", PermissionSet::closure(0x00FF)),
];

/// The permission bits the samples set, together: every bit 0 to 7.
pub fn sampled_permission_bits() -> u16 {
    PERMISSION_SETS.iter().fold(0, |acc, (_, p)| acc | p.bits())
}

fn bytes_of(fill: u8) -> [u8; 16] {
    [fill; 16]
}

fn ascending<const N: usize>(start: u8) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = start.wrapping_add(i as u8);
    }
    out
}

/// A string of exactly `len` bytes of ASCII.
fn ascii(len: usize) -> String {
    "abcdefghijklmnopqrstuvwxyz0123456789".chars().cycle().take(len).collect()
}

const MULTIBYTE: &str = "Máquina do João 日本 😀";

fn hello(minor: u16) -> Hello {
    Hello {
        proto_minor: minor,
        app_version: "1.0.0".into(),
        name: "Notebook da sala".into(),
        os: Os::Windows,
        os_version: "10.0.26200".into(),
        intent: Intent::View,
        requested: PermissionSet::VIEW,
        resume: None,
        swap_token: None,
    }
}

fn display(id: u16) -> Display {
    Display {
        id,
        name: format!("DISPLAY{id}"),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        scale_milli: 1000,
        refresh_mhz: 60_000,
        rotation: 0,
        primary: id == 0,
    }
}

fn video_cap(profile: H264Profile) -> VideoCap {
    VideoCap { codec: Codec::H264, profile, max_level: 42, max_width: 3840, max_height: 2160, max_fps: 60 }
}

fn capabilities(features: u32) -> Capabilities {
    Capabilities {
        video_decode: vec![video_cap(H264Profile::High)],
        audio: vec![AudioCodec::Opus48Stereo],
        max_datagram: 1200,
        keyboard: KeyboardInfo { kind: KbKind::Abnt2, layout: "pt-BR".into() },
        features,
    }
}

fn video_header(keyframe: bool, config: bool, discontinuity: bool, payload_len: u32) -> VideoHeader {
    VideoHeader {
        keyframe,
        config,
        discontinuity,
        display_id: 0x0102,
        frame_id: 0x0304_0506,
        capture_us: 0x0708_090A_0B0C_0D0E,
        encode_us: 0x0F10_1112,
        width: 1920,
        height: 1080,
        codec: Codec::H264,
        payload_len,
    }
}

/// Every sample, in a fixed order with unique names.
pub fn samples() -> Vec<Sample> {
    let mut t = Table(Vec::new());
    session_samples(&mut t);
    display_and_video_samples(&mut t);
    swap_chat_clipboard_samples(&mut t);
    file_samples(&mut t);
    terminal_action_wake_samples(&mut t);
    pairing_samples(&mut t);
    keepalive_samples(&mut t);
    constant_samples(&mut t);
    media_samples(&mut t);
    input_samples(&mut t);
    t.0
}

fn session_samples(t: &mut Table) {
    // Hello: every Os and Intent, the optional parts, the string limits, multibyte text.
    for os in OS {
        for intent in INTENT {
            t.msg(
                format!("msg_hello_{os:?}_{intent:?}").to_lowercase(),
                Hello { os: *os, intent: *intent, ..hello(0) },
            );
        }
    }
    t.msg(
        "msg_hello_swap_token",
        Hello {
            os: Os::MacOs,
            intent: Intent::Control,
            requested: PermissionSet::CONTROL,
            swap_token: Some(bytes_of(0x5A)),
            ..hello(0)
        },
    );
    t.msg(
        "msg_hello_resume",
        Hello {
            requested: PermissionSet::CONTROL | PermissionSet::AUDIO,
            resume: Some(Resume { session_id: 0x0102_0304_0506_0708, token: bytes_of(0xC3) }),
            ..hello(0)
        },
    );
    t.msg(
        "msg_hello_unattended_admin",
        Hello {
            intent: Intent::Control,
            requested: PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::FILES,
            resume: Some(Resume { session_id: u64::MAX, token: ascending(0) }),
            swap_token: Some(ascending(0xF0)),
            ..hello(0)
        },
    );
    t.msg(
        "msg_hello_string_limits",
        Hello { app_version: ascii(32), name: ascii(64), os_version: ascii(32), ..hello(0) },
    );
    t.msg("msg_hello_multibyte", Hello { name: MULTIBYTE.into(), ..hello(0) });
    t.msg(
        "msg_hello_empty_strings",
        Hello { app_version: String::new(), name: String::new(), os_version: String::new(), ..hello(0) },
    );

    for os in OS {
        t.msg(
            format!("msg_hello_ack_{os:?}").to_lowercase(),
            HelloAck {
                proto_minor: 0,
                app_version: "1.0.0".into(),
                name: "Mesa".into(),
                os: *os,
                os_version: "14.5".into(),
            },
        );
    }
    t.msg(
        "msg_hello_ack_string_limits",
        HelloAck {
            proto_minor: 0,
            app_version: ascii(32),
            name: MULTIBYTE.into(),
            os: Os::Windows,
            os_version: ascii(32),
        },
    );

    t.msg("msg_capabilities_basic", capabilities(0b11_1111));
    t.msg(
        "msg_capabilities_empty",
        Capabilities {
            video_decode: vec![],
            audio: vec![],
            max_datagram: 0,
            keyboard: KeyboardInfo { kind: KbKind::Unknown, layout: String::new() },
            features: 0,
        },
    );
    t.msg(
        "msg_capabilities_limits",
        Capabilities {
            video_decode: (0..8).map(|i| VideoCap { max_level: 30 + i, ..video_cap(H264Profile::Main) }).collect(),
            audio: vec![AudioCodec::Opus48Stereo; 4],
            max_datagram: u16::MAX,
            keyboard: KeyboardInfo { kind: KbKind::Iso, layout: ascii(64) },
            features: u32::MAX,
        },
    );
    for profile in H264_PROFILE {
        for codec in CODEC {
            t.msg(
                format!("msg_capabilities_video_{profile:?}_{codec:?}").to_lowercase(),
                Capabilities {
                    video_decode: vec![VideoCap { codec: *codec, ..video_cap(*profile) }],
                    ..capabilities(0)
                },
            );
        }
    }
    for codec in AUDIO_CODEC {
        t.msg(
            format!("msg_capabilities_audio_{codec:?}").to_lowercase(),
            Capabilities { audio: vec![*codec], ..capabilities(0) },
        );
    }
    for kind in KB_KIND {
        t.msg(
            format!("msg_capabilities_keyboard_{kind:?}").to_lowercase(),
            Capabilities { keyboard: KeyboardInfo { kind: *kind, layout: "layout".into() }, ..capabilities(0) },
        );
    }
    for (name, bit) in [
        ("clipboard_images", feature_bits::CLIPBOARD_IMAGES),
        ("file_resume", feature_bits::FILE_RESUME),
        ("terminal", feature_bits::TERMINAL),
        ("admin_terminal", feature_bits::ADMIN_TERMINAL),
        ("quick_actions", feature_bits::QUICK_ACTIONS),
        ("sas", feature_bits::SAS),
    ] {
        t.msg(format!("msg_capabilities_feature_{name}"), capabilities(bit));
    }
    t.msg("msg_capabilities_feature_unknown_bits", capabilities(0xFFFF_FFC0));

    t.msg("msg_prompt_pending", PromptPending { timeout_s: 30 });
    t.msg("msg_prompt_pending_max", PromptPending { timeout_s: u16::MAX });

    for (i, mode) in GRANT_MODE.iter().enumerate() {
        t.msg(
            format!("msg_session_grant_{mode:?}").to_lowercase(),
            SessionGrant {
                session_id: 0x1000 + i as u64,
                granted: PERMISSION_SETS[i + 1].1,
                mode: *mode,
                resume_token: bytes_of(0xA5),
            },
        );
    }
    t.msg(
        "msg_session_grant_max_ids",
        SessionGrant {
            session_id: u64::MAX,
            granted: PermissionSet::closure(0x00FF),
            mode: GrantMode::Prompted,
            resume_token: ascending(1),
        },
    );
    for code in
        [ErrorCode::Declined, ErrorCode::Timeout, ErrorCode::NobodyPresent, ErrorCode::NotPermitted, ErrorCode::Busy]
    {
        t.msg(format!("msg_session_refused_{code:?}").to_lowercase(), SessionRefused { code });
    }
    for (name, granted) in PERMISSION_SETS {
        t.msg(format!("msg_permissions_changed_{name}"), PermissionsChanged { granted: *granted });
    }
    t.msg(
        "msg_permissions_changed_combined",
        PermissionsChanged { granted: PermissionSet::CONTROL | PermissionSet::AUDIO | PermissionSet::CLIPBOARD },
    );
    for code in ERROR_CODE {
        t.msg(format!("msg_bye_{code:?}").to_lowercase(), Bye { code: *code });
    }
    for desktop in DESKTOP {
        for paused in [false, true] {
            t.msg(
                format!("msg_host_state_{desktop:?}_{}", if paused { "paused" } else { "running" }).to_lowercase(),
                HostState { desktop: *desktop, paused },
            );
        }
    }
    for (feature, timeout_s) in FEATURE.iter().zip([30, 60]) {
        t.msg(
            format!("msg_consent_pending_{feature:?}").to_lowercase(),
            ConsentPending { feature: *feature, timeout_s },
        );
    }
}

fn display_and_video_samples(t: &mut Table) {
    t.msg("msg_display_list_empty", DisplayList { displays: vec![] });
    t.msg("msg_display_list_one", DisplayList { displays: vec![display(0)] });
    t.msg(
        "msg_display_list_two",
        DisplayList {
            displays: vec![
                display(0),
                Display {
                    id: 1,
                    name: "Monitor lateral 日本".into(),
                    x: -2560,
                    y: -120,
                    width: 2560,
                    height: 1440,
                    scale_milli: 1500,
                    refresh_mhz: 59_940,
                    rotation: 90,
                    primary: false,
                },
            ],
        },
    );
    t.msg(
        "msg_display_list_extremes",
        DisplayList {
            displays: vec![Display {
                id: u16::MAX,
                name: ascii(64),
                x: i32::MIN,
                y: i32::MAX,
                width: u32::MAX,
                height: u32::MAX,
                scale_milli: u16::MAX,
                refresh_mhz: u32::MAX,
                rotation: 270,
                primary: true,
            }],
        },
    );
    t.msg("msg_display_list_sixteen", DisplayList { displays: (0..16).map(display).collect() });
    t.msg("msg_select_display", SelectDisplay { display: 1 });
    t.msg("msg_select_display_max", SelectDisplay { display: u16::MAX });
    for (i, preset) in PRESET.iter().enumerate() {
        t.msg(
            format!("msg_video_settings_{preset:?}").to_lowercase(),
            VideoSettings {
                preset: *preset,
                max_fps: 30 + 15 * i as u16,
                max_kbps: 8000 * (i as u32 + 1),
                view_width: 1280,
                view_height: 720,
            },
        );
    }
    t.msg(
        "msg_video_settings_zero",
        VideoSettings { preset: Preset::Auto, max_fps: 0, max_kbps: 0, view_width: 0, view_height: 0 },
    );
    t.msg("msg_keyframe_request", KeyframeRequest { display: 1, last_good_frame: 0x0102_0304 });
    t.msg("msg_keyframe_request_max", KeyframeRequest { display: u16::MAX, last_good_frame: u32::MAX });
    t.msg(
        "msg_receiver_report",
        ReceiverReport {
            frames_presented: 60,
            frames_lost: 2,
            decode_us_p50: 2500,
            present_lag_us_p50: 8300,
            audio_underruns: 1,
        },
    );
    t.msg(
        "msg_receiver_report_max",
        ReceiverReport {
            frames_presented: u32::MAX,
            frames_lost: u32::MAX,
            decode_us_p50: u32::MAX,
            present_lag_us_p50: u32::MAX,
            audio_underruns: u32::MAX,
        },
    );
    t.msg("msg_stream_stats", StreamStats { kbps: 12_000, fps: 60, encoder: "h264_mf (NVIDIA)".into() });
    t.msg("msg_stream_stats_empty", StreamStats { kbps: 0, fps: 0, encoder: String::new() });
    for enabled in [false, true] {
        t.msg(format!("msg_audio_control_{enabled}"), AudioControl { enabled });
        t.msg(format!("msg_host_mute_{enabled}"), HostMute { muted: enabled });
    }
    for key in SPECIAL {
        t.msg(format!("msg_send_special_{key:?}").to_lowercase(), SendSpecial { key: *key });
    }
}

fn swap_chat_clipboard_samples(t: &mut Table) {
    t.msg("msg_swap_request", SwapRequest { request_id: 7 });
    t.msg("msg_swap_accept", SwapAccept { request_id: 7, token: ascending(0x10) });
    for code in
        [ErrorCode::Declined, ErrorCode::Timeout, ErrorCode::NobodyPresent, ErrorCode::NotPermitted, ErrorCode::Busy]
    {
        t.msg(format!("msg_swap_decline_{code:?}").to_lowercase(), SwapDecline { request_id: 7, code });
    }
    t.msg("msg_swap_cancel", SwapCancel { request_id: u32::MAX });
    t.msg("msg_swap_armed", SwapArmed { request_id: 7 });

    t.msg("msg_chat_message", ChatMessage { msg_id: 1, text: "Olá, tudo bem?".into(), sent_at_ms: 1_762_700_000_000 });
    t.msg(
        "msg_chat_message_multibyte",
        ChatMessage { msg_id: 2, text: format!("linha 1\nlinha 2\t{MULTIBYTE}"), sent_at_ms: 0 },
    );
    t.msg("msg_chat_message_empty", ChatMessage { msg_id: u32::MAX, text: String::new(), sent_at_ms: u64::MAX });
    t.msg("msg_chat_message_long", ChatMessage { msg_id: 3, text: ascii(300), sent_at_ms: 1 });
    t.msg("msg_chat_read", ChatRead { up_to: 42 });
    for enabled in [false, true] {
        t.msg(format!("msg_clipboard_sync_{enabled}"), ClipboardSync { enabled });
    }
    for reason in CLIP_SKIP {
        t.msg(format!("msg_clipboard_skipped_{reason:?}").to_lowercase(), ClipboardSkipped { reason: *reason });
    }
}

fn file_samples(t: &mut Table) {
    t.msg(
        "msg_file_offer",
        FileOffer {
            transfer_id: 1,
            name: "relatório final.pdf".into(),
            size: 1_048_576,
            mtime_ms: 1_762_700_000_000,
            dest: None,
        },
    );
    t.msg(
        "msg_file_offer_dest",
        FileOffer {
            transfer_id: 2,
            name: "foto.png".into(),
            size: u64::MAX,
            mtime_ms: 0,
            dest: Some("C:\\Users\\viewer\\Downloads".into()),
        },
    );
    t.msg(
        "msg_file_offer_limits",
        FileOffer { transfer_id: u32::MAX, name: ascii(255), size: 0, mtime_ms: u64::MAX, dest: Some(ascii(300)) },
    );
    t.msg("msg_file_accept", FileAccept { transfer_id: 1, resume_from: 0 });
    t.msg("msg_file_accept_resume", FileAccept { transfer_id: 1, resume_from: 524_288 });
    for code in [ErrorCode::InvalidName, ErrorCode::Declined, ErrorCode::NotPermitted, ErrorCode::IoFailed] {
        t.msg(format!("msg_file_decline_{code:?}").to_lowercase(), FileDecline { transfer_id: 3, code });
    }
    t.msg("msg_file_done", FileDone { transfer_id: 1, sha256: ascending(0x20) });
    for ok in [false, true] {
        t.msg(format!("msg_file_verified_{ok}"), FileVerified { transfer_id: 1, ok });
    }
    t.msg("msg_file_pause", FilePause { transfer_id: 1 });
    t.msg("msg_file_resume", FileResume { transfer_id: 1 });
    t.msg("msg_file_cancel", FileCancel { transfer_id: 1 });
    t.msg("msg_dir_request_home", DirRequest { req_id: 1, path: None });
    t.msg("msg_dir_request_path", DirRequest { req_id: 2, path: Some("/Users/viewer/Documents".into()) });
    t.msg("msg_dir_listing_empty", DirListing { req_id: 1, path: "C:\\".into(), entries: vec![], truncated: false });
    t.msg(
        "msg_dir_listing",
        DirListing {
            req_id: 2,
            path: "/Users/viewer".into(),
            entries: vec![
                DirEntry { name: "Documents".into(), is_dir: true, size: 0, mtime_ms: 1_762_000_000_000 },
                DirEntry { name: "notas.txt".into(), is_dir: false, size: 1234, mtime_ms: 1_762_100_000_000 },
                DirEntry { name: "日本語.md".into(), is_dir: false, size: u64::MAX, mtime_ms: u64::MAX },
            ],
            truncated: true,
        },
    );
    t.msg("msg_file_get", FileGet { transfer_id: 9, path: "C:\\Users\\host\\Desktop\\a.zip".into() });
}

fn terminal_action_wake_samples(t: &mut Table) {
    for admin in [false, true] {
        t.msg(format!("msg_term_open_admin_{admin}"), TermOpen { term_id: 1, cols: 120, rows: 30, admin });
    }
    t.msg("msg_term_opened", TermOpened { term_id: 1, account: "claudio".into() });
    t.msg("msg_term_opened_system", TermOpened { term_id: u32::MAX, account: "SYSTEM".into() });
    t.msg("msg_term_resize", TermResize { term_id: 1, cols: 200, rows: 50 });
    t.msg("msg_term_close", TermClose { term_id: 1 });
    t.msg("msg_term_exited_none", TermExited { term_id: 1, exit_code: None });
    for (name, code) in
        [("zero", 0), ("signal", 130), ("negative", -1), ("windows_ctrl_c", -1_073_741_510), ("min", i32::MIN)]
    {
        t.msg(format!("msg_term_exited_{name}"), TermExited { term_id: 1, exit_code: Some(code) });
    }
    for (i, kind) in ACTION.iter().enumerate() {
        t.msg(
            format!("msg_quick_action_{kind:?}").to_lowercase(),
            QuickAction { action_id: 100 + i as u32, kind: *kind },
        );
    }
    for code in [ErrorCode::NoError, ErrorCode::NotPermitted, ErrorCode::Unavailable, ErrorCode::IoFailed] {
        t.msg(format!("msg_quick_action_result_{code:?}").to_lowercase(), QuickActionResult { action_id: 100, code });
    }
    t.msg("msg_wake_info_empty", WakeInfo { macs: vec![] });
    t.msg("msg_wake_info_one", WakeInfo { macs: vec![[0x00, 0x1B, 0x44, 0x11, 0x3A, 0xB7]] });
    t.msg("msg_wake_info_eight", WakeInfo { macs: (0..8).map(|i| ascending::<6>(i * 6)).collect() });
}

fn pairing_samples(t: &mut Table) {
    for os in OS {
        t.msg(
            format!("msg_pair_hello_{os:?}").to_lowercase(),
            PairHello {
                name: "Notebook da sala".into(),
                os: *os,
                os_version: "14.5".into(),
                majors: vec![1],
                commit: ascending(0x30),
            },
        );
        t.msg(
            format!("msg_pair_nonce_{os:?}").to_lowercase(),
            PairNonce {
                name: "Mesa".into(),
                os: *os,
                os_version: "10.0.26200".into(),
                majors: vec![1, 2],
                nonce_r: ascending(0x50),
            },
        );
    }
    t.msg(
        "msg_pair_hello_limits",
        PairHello {
            name: ascii(64),
            os: Os::Windows,
            os_version: ascii(32),
            majors: (1..=8).collect(),
            commit: [0xFF; 32],
        },
    );
    t.msg(
        "msg_pair_nonce_empty_majors",
        PairNonce {
            name: MULTIBYTE.into(),
            os: Os::MacOs,
            os_version: String::new(),
            majors: vec![],
            nonce_r: [0; 32],
        },
    );
    t.msg("msg_pair_reveal", PairReveal { nonce_i: ascending(0x70) });
    for matches in [false, true] {
        t.msg(format!("msg_pair_confirm_{matches}"), PairConfirm { matches });
    }
    t.msg("msg_pair_result_ok", PairResult { ok: true, code: ErrorCode::NoError });
    for code in [
        ErrorCode::PairingLocked,
        ErrorCode::Busy,
        ErrorCode::AlreadyPaired,
        ErrorCode::NotPermitted,
        ErrorCode::UnsupportedVersion,
        ErrorCode::SasRejected,
        ErrorCode::CommitMismatch,
    ] {
        t.msg(format!("msg_pair_result_{code:?}").to_lowercase(), PairResult { ok: false, code });
    }

    t.add("pairing_commit_preimage", Item::CommitPreimage { spki_i: ascending(0x80), nonce_i: ascending(0x70) });
    t.add(
        "pairing_sas_preimage",
        Item::SasPreimage {
            ek: ascending(0x01),
            spki_i: ascending(0x80),
            spki_r: ascending(0xC0),
            nonce_i: ascending(0x70),
            nonce_r: ascending(0x50),
        },
    );
    let mut digest = [0u8; 32];
    digest[..8].copy_from_slice(&482_913u64.to_be_bytes());
    t.add("pairing_sas_digits_482913", Item::SasDigits { digest });
    digest[..8].copy_from_slice(&(7 * 1_000_000u64 + 42).to_be_bytes());
    t.add("pairing_sas_digits_leading_zeros", Item::SasDigits { digest });
    digest[..8].copy_from_slice(&u64::MAX.to_be_bytes());
    digest[8..].fill(0xFF);
    t.add("pairing_sas_digits_max", Item::SasDigits { digest });
}

fn keepalive_samples(t: &mut Table) {
    t.msg("msg_ping", Ping { nonce: 1, t_send_us: 2 });
    t.msg("msg_ping_max", Ping { nonce: u32::MAX, t_send_us: u64::MAX });
    t.msg("msg_pong", Pong { nonce: 1, t_send_us: 2, t_peer_us: 3 });
    t.msg("msg_pong_max", Pong { nonce: u32::MAX, t_send_us: u64::MAX, t_peer_us: u64::MAX });
    t.msg(
        "msg_error_fatal",
        ErrorMessage { code: ErrorCode::ProtocolViolation, fatal: true, about_type: 0x0F01, about_id: 0 },
    );
    t.msg(
        "msg_error_not_fatal",
        ErrorMessage { code: ErrorCode::NotFound, fatal: false, about_type: 0x0409, about_id: 77 },
    );
    t.msg(
        "msg_error_max",
        ErrorMessage { code: ErrorCode::AlreadyPaired, fatal: true, about_type: u16::MAX, about_id: u32::MAX },
    );
    t.add("framing_header_ping", Item::FrameHeader(FrameHeader { type_id: 0x0F01, len: 12 }));
    t.add("framing_header_max_len", Item::FrameHeader(FrameHeader { type_id: 0x040A, len: MAX_FRAME_PAYLOAD }));
    t.add("framing_header_empty", Item::FrameHeader(FrameHeader { type_id: 0x0001, len: 0 }));
}

fn constant_samples(t: &mut Table) {
    let text = |t: &mut Table, name: &str, v: &[u8]| t.add(name, Item::Constant(v.to_vec()));
    text(t, "const_alpn", ALPN);
    text(t, "const_pair_alpn", PAIR_ALPN);
    text(t, "const_commit_label", COMMIT_LABEL);
    text(t, "const_sas_label", SAS_LABEL);
    text(t, "const_exporter_label", EXPORTER_LABEL);
    let word = |t: &mut Table, name: &str, v: u16| t.add(name, Item::Constant(v.to_le_bytes().to_vec()));
    word(t, "const_protocol_version", PROTOCOL_VERSION);
    word(t, "const_session_port", SESSION_PORT);
    word(t, "const_pairing_port", PAIRING_PORT);
    word(t, "const_max_cursor_dimension", MAX_CURSOR_DIMENSION);
    let limit = |t: &mut Table, name: &str, v: u64| t.add(name, Item::Constant(v.to_le_bytes().to_vec()));
    limit(t, "const_max_frame_payload", MAX_FRAME_PAYLOAD as u64);
    limit(t, "const_max_video_payload", u64::from(MAX_VIDEO_PAYLOAD));
    limit(t, "const_max_clipboard_text", u64::from(MAX_CLIPBOARD_TEXT));
    limit(t, "const_max_clipboard_png", u64::from(MAX_CLIPBOARD_PNG));
    limit(t, "const_max_input_text", MAX_INPUT_TEXT as u64);
    limit(t, "const_max_pair_majors", MAX_PAIR_MAJORS as u64);
    limit(t, "const_video_header_len", VIDEO_HEADER_LEN as u64);
    limit(t, "const_audio_header_len", AUDIO_HEADER_LEN as u64);
    limit(t, "const_cursor_position_len", CURSOR_POSITION_LEN as u64);
    limit(t, "const_cursor_shape_header_len", CURSOR_SHAPE_HEADER_LEN as u64);
    limit(t, "const_clipboard_header_len", CLIPBOARD_HEADER_LEN as u64);
    limit(t, "const_nonce_len", NONCE_LEN as u64);
    limit(t, "const_exporter_len", EXPORTER_LEN as u64);
    limit(t, "const_spki_len", SPKI_LEN as u64);
    limit(t, "const_sas_modulus", Sas::MODULUS);
}

fn media_samples(t: &mut Table) {
    for bits in 0..8u8 {
        let (keyframe, config, discontinuity) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
        t.add(
            format!("video_header_flags_{bits:03b}"),
            Item::VideoStream(video_header(keyframe, config, discontinuity, 4096)),
        );
    }
    t.add("video_header_empty_payload", Item::VideoStream(video_header(false, false, false, 0)));
    t.add("video_header_max_payload", Item::VideoStream(video_header(true, true, false, MAX_VIDEO_PAYLOAD)));
    t.add(
        "video_header_extremes",
        Item::VideoStream(VideoHeader {
            display_id: u16::MAX,
            frame_id: u32::MAX,
            capture_us: u64::MAX,
            encode_us: u32::MAX,
            width: u16::MAX,
            height: u16::MAX,
            ..video_header(true, false, true, 1)
        }),
    );

    for bits in 0..4u8 {
        t.add(
            format!("audio_header_flags_{bits:02b}"),
            Item::AudioDatagram(AudioHeader {
                discontinuity: bits & 1 != 0,
                muted: bits & 2 != 0,
                frame_ms: 10,
                seq: 7,
                capture_us: 123_456_789,
            }),
        );
    }
    t.add(
        "audio_header_extremes",
        Item::AudioDatagram(AudioHeader {
            discontinuity: true,
            muted: true,
            frame_ms: u16::MAX,
            seq: u32::MAX,
            capture_us: u64::MAX,
        }),
    );

    for visible in [false, true] {
        t.add(
            format!("cursor_position_visible_{visible}"),
            Item::CursorPositionDatagram(CursorPosition { visible, display: 1, seq: 2, x: 3, y: 4, shape_id: 5 }),
        );
    }
    t.add(
        "cursor_position_extremes",
        Item::CursorPositionDatagram(CursorPosition {
            visible: true,
            display: u16::MAX,
            seq: u32::MAX,
            x: u16::MAX,
            y: u16::MAX,
            shape_id: u32::MAX,
        }),
    );
    let header = CursorShapeHeader { shape_id: 1, width: 2, height: 3, hot_x: 0, hot_y: 1 };
    t.add("cursor_shape_2x3", Item::CursorShapeStream(CursorShape { header, rgba: ascending::<24>(0x40).to_vec() }));
    t.add(
        "cursor_shape_empty",
        Item::CursorShapeStream(CursorShape {
            header: CursorShapeHeader { shape_id: 0, width: 0, height: 0, hot_x: 0, hot_y: 0 },
            rgba: vec![],
        }),
    );
    t.add(
        "cursor_shape_hot_spot_outside",
        Item::CursorShapeStream(CursorShape {
            header: CursorShapeHeader { shape_id: u32::MAX, width: 1, height: 1, hot_x: u16::MAX, hot_y: u16::MAX },
            rgba: vec![0xFF, 0x80, 0x00, 0x7F],
        }),
    );

    t.add(
        "clipboard_text",
        Item::ClipboardStream(ClipboardItem { seq: 1, payload: ClipboardPayload::Text("copiado: ação".into()) }),
    );
    t.add(
        "clipboard_text_empty",
        Item::ClipboardStream(ClipboardItem { seq: 2, payload: ClipboardPayload::Text(String::new()) }),
    );
    t.add(
        "clipboard_text_nul_and_multibyte",
        Item::ClipboardStream(ClipboardItem {
            seq: u32::MAX,
            payload: ClipboardPayload::Text(format!("a\0b\r\n{MULTIBYTE}")),
        }),
    );
    t.add(
        "clipboard_png",
        Item::ClipboardStream(ClipboardItem {
            seq: 3,
            payload: ClipboardPayload::Png(
                [&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A][..], &ascending::<24>(0)[..]].concat(),
            ),
        }),
    );
    t.add(
        "clipboard_png_empty",
        Item::ClipboardStream(ClipboardItem { seq: 4, payload: ClipboardPayload::Png(vec![]) }),
    );

    t.add("file_data_header", Item::FileDataStream(FileDataHeader { transfer_id: 1, offset: 0 }));
    t.add(
        "file_data_header_extremes",
        Item::FileDataStream(FileDataHeader { transfer_id: u32::MAX, offset: u64::MAX }),
    );
    t.add("terminal_header", Item::TerminalStream(TerminalStreamHeader { term_id: 1 }));
    t.add("terminal_header_max", Item::TerminalStream(TerminalStreamHeader { term_id: u32::MAX }));
}

fn input_samples(t: &mut Table) {
    let one = |t: &mut Table, name: &str, e: InputEvent| t.add(name, Item::InputStream(vec![e]));
    one(t, "input_key_keyboard", InputEvent::Key { usage_page: 7, usage: 4, down: true, modifiers: 0x02, locks: 1 });
    one(
        t,
        "input_key_keyboard_up",
        InputEvent::Key { usage_page: 7, usage: 0xE7, down: false, modifiers: 0xFF, locks: 0b111 },
    );
    one(
        t,
        "input_key_consumer",
        InputEvent::Key { usage_page: 0x0C, usage: 0x00E2, down: true, modifiers: 0, locks: 0 },
    );
    one(t, "input_text", InputEvent::Text("a".into()));
    one(t, "input_text_multibyte", InputEvent::Text(MULTIBYTE.into()));
    one(t, "input_text_empty", InputEvent::Text(String::new()));
    one(t, "input_text_max", InputEvent::Text(ascii(256)));
    one(t, "input_pointer", InputEvent::Pointer { display: 1, x: 2, y: 3 });
    one(t, "input_pointer_extremes", InputEvent::Pointer { display: u16::MAX, x: u16::MAX, y: u16::MAX });
    for button in MOUSE_BUTTON {
        one(
            t,
            &format!("input_button_{button:?}_down").to_lowercase(),
            InputEvent::Button { button: *button, down: true },
        );
    }
    one(t, "input_button_left_up", InputEvent::Button { button: MouseButton::Left, down: false });
    one(t, "input_wheel", InputEvent::Wheel { dx: -1, dy: 120 });
    one(t, "input_wheel_extremes", InputEvent::Wheel { dx: i16::MIN, dy: i16::MAX });
    one(t, "input_release_all", InputEvent::ReleaseAll);
    t.add("input_stream_empty", Item::InputStream(vec![]));
    t.add(
        "input_stream_sequence",
        Item::InputStream(vec![
            InputEvent::Pointer { display: 0, x: 32768, y: 32768 },
            InputEvent::Button { button: MouseButton::Left, down: true },
            InputEvent::Key { usage_page: 7, usage: 4, down: true, modifiers: 0, locks: 0 },
            InputEvent::Text("x".into()),
            InputEvent::Wheel { dx: 0, dy: -240 },
            InputEvent::Button { button: MouseButton::Left, down: false },
            InputEvent::ReleaseAll,
        ]),
    );
}
