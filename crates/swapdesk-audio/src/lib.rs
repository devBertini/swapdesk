//! Audio of Swapdesk (contracts C3 and PROTOCOL.md 7.2, 7.x `AudioControl`; decision D16).
//!
//! The host side is [`AudioSender`]: it pulls 48 kHz stereo f32 blocks from a C3 `AudioSource`, cuts them into 10 ms
//! frames, encodes each with Opus (libopus through the `opus` crate) and returns one audio datagram per frame. With
//! audio off it never starts the source and returns nothing (D16). The viewer side is [`AudioReceiver`]: it parses
//! datagrams, keeps them in an adaptive [`JitterBuffer`] and hands out one decoded (or concealed) 10 ms frame per
//! playout tick.
//!
//! Time is always passed in by the caller (microseconds on the caller's own monotonic timeline), so the buffer is
//! tested over simulated minutes without a clock. No `unsafe` and no I/O here: the QUIC datagrams and the playout
//! thread belong to the callers.
//!
//! Frame size: 10 ms (480 samples per channel, 960 interleaved values, the C3 `AudioBlock` unit). A frame is the
//! smallest unit the receiver can lose, delay or conceal, and the sender must collect a whole frame before it can
//! encode it, so the frame length is added to the delay once on each side; 10 ms with the restricted low-delay
//! application (CELT only, 2.5 ms look-ahead) costs 12.5 ms of algorithmic delay against 22.5 ms at 20 ms frames,
//! for 100 datagrams per second whose 16-byte headers add 12.8 kbit/s on a LAN (negligible). The `frame_ms` field of
//! PROTOCOL.md 7.2 carries it; this crate sends and accepts 10 only.

mod codec;
mod jitter;
mod packet;
mod receiver;
mod sender;

pub use codec::{BITRATE_BPS, FrameDecoder, FrameEncoder, LOOKAHEAD_SAMPLES};
pub use jitter::{JitterBuffer, JitterConfig, JitterStats, Playout, QueuedPacket};
pub use packet::{PacketError, build_datagram, parse_datagram};
pub use receiver::{AudioReceiver, FrameKind, RxFrame};
pub use sender::{AudioSender, SenderStats};

use swapdesk_platform::PlatformError;

/// Sample rate of every audio path.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved channels (stereo).
pub const CHANNELS: usize = 2;
/// Frame length in milliseconds (the `frame_ms` header field).
pub const FRAME_MS: u16 = 10;
/// Frame length in microseconds.
pub const FRAME_US: u64 = 10_000;
/// Samples per channel in a frame.
pub const FRAME_SAMPLES: usize = 480;
/// Interleaved f32 values in a frame (10 ms = 960 values, C3).
pub const FRAME_VALUES: usize = FRAME_SAMPLES * CHANNELS;
/// Header bytes before the Opus packet (PROTOCOL.md 7.2).
pub const HEADER_LEN: usize = swapdesk_proto::AUDIO_HEADER_LEN;
/// Largest audio datagram: it fits one QUIC datagram on a 1280-byte-MTU path with the QUIC and UDP headers (GUESS,
/// the conservative IPv6 minimum; the video path may choose another).
pub const MAX_DATAGRAM: usize = 1200;
/// Largest Opus packet that fits a datagram.
pub const MAX_OPUS_PACKET: usize = MAX_DATAGRAM - HEADER_LEN;

#[derive(Debug)]
pub enum AudioError {
    Opus(opus::Error),
    Platform(PlatformError),
    Packet(PacketError),
}

impl core::fmt::Display for AudioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Opus(e) => write!(f, "opus: {e}"),
            Self::Platform(e) => write!(f, "platform: {e}"),
            Self::Packet(e) => write!(f, "packet: {e}"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<opus::Error> for AudioError {
    fn from(e: opus::Error) -> Self {
        Self::Opus(e)
    }
}

impl From<PlatformError> for AudioError {
    fn from(e: PlatformError) -> Self {
        Self::Platform(e)
    }
}

impl From<PacketError> for AudioError {
    fn from(e: PacketError) -> Self {
        Self::Packet(e)
    }
}

pub type Result<T> = core::result::Result<T, AudioError>;
