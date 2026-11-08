//! The viewer's audio path: datagram in, one 10 ms frame out per playout tick.

use crate::codec::FrameDecoder;
use crate::jitter::{JitterBuffer, JitterConfig, JitterStats, Playout};
use crate::packet::parse_datagram;
use crate::{FRAME_US, FRAME_VALUES, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Decoded from a received packet.
    Decoded,
    /// A lost or late frame filled by Opus (PLC or FEC).
    Concealed,
    /// The host is muted: silence, and the UI shows it.
    Muted,
    /// The stream is being re-synchronised: silence.
    Silence,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RxFrame {
    pub kind: FrameKind,
    /// The host's `capture_us` of the frame's first sample (estimated for concealed frames).
    pub capture_us: u64,
    /// [`FRAME_VALUES`] interleaved f32 values.
    pub samples: Vec<f32>,
}

pub struct AudioReceiver {
    jitter: JitterBuffer,
    dec: FrameDecoder,
    playing: bool,
    muted: bool,
    last_capture_us: u64,
}

impl AudioReceiver {
    pub fn new(cfg: JitterConfig) -> Result<Self> {
        Ok(Self {
            jitter: JitterBuffer::new(cfg),
            dec: FrameDecoder::new()?,
            playing: false,
            muted: false,
            last_capture_us: 0,
        })
    }

    /// One audio datagram arrived at `now_us` (the caller's monotonic time). A malformed one is an error the caller
    /// drops; it never reaches the buffer.
    pub fn on_datagram(&mut self, now_us: u64, bytes: &[u8]) -> Result<()> {
        let (header, payload) = parse_datagram(bytes)?;
        self.jitter.push(now_us, header, payload.to_vec());
        Ok(())
    }

    /// The viewer shows "muted" while this is true.
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn stats(&self) -> &JitterStats {
        self.jitter.stats()
    }

    pub fn target_ms(&self) -> u32 {
        self.jitter.target_ms()
    }

    /// One playout tick. `None` until the stream has started; after that, always a frame (silence while it
    /// re-synchronises).
    pub fn pop(&mut self, now_us: u64) -> Result<Option<RxFrame>> {
        let mut samples = vec![0.0f32; FRAME_VALUES];
        let (kind, capture_us) = match self.jitter.pop(now_us) {
            Playout::Waiting => {
                if !self.playing {
                    return Ok(None);
                }
                (FrameKind::Silence, self.last_capture_us + FRAME_US)
            }
            Playout::Packet(p) => {
                self.playing = true;
                self.last_capture_us = p.header.capture_us;
                if p.header.muted {
                    self.muted = true;
                    (FrameKind::Muted, p.header.capture_us)
                } else {
                    if self.muted {
                        self.muted = false;
                        self.dec.reset()?;
                    }
                    match self.dec.decode(&p.payload, &mut samples) {
                        Ok(_) => (FrameKind::Decoded, p.header.capture_us),
                        Err(_) => {
                            // A corrupt packet is a lost one.
                            self.dec.conceal(None, &mut samples)?;
                            (FrameKind::Concealed, p.header.capture_us)
                        }
                    }
                }
            }
            Playout::Lost { following } => {
                self.playing = true;
                self.last_capture_us += FRAME_US;
                if self.muted {
                    (FrameKind::Muted, self.last_capture_us)
                } else {
                    self.dec.conceal(following.as_deref(), &mut samples)?;
                    (FrameKind::Concealed, self.last_capture_us)
                }
            }
            Playout::Starved => {
                self.last_capture_us += FRAME_US;
                if self.muted {
                    (FrameKind::Muted, self.last_capture_us)
                } else {
                    self.dec.conceal(None, &mut samples)?;
                    (FrameKind::Concealed, self.last_capture_us)
                }
            }
        };
        Ok(Some(RxFrame { kind, capture_us, samples }))
    }
}
