//! Opus wrappers fixed to this crate's frame: 48 kHz stereo, 10 ms.

use opus::{Application, Bitrate, Channels, Decoder, Encoder, Signal};

use crate::{FRAME_SAMPLES, FRAME_VALUES, MAX_OPUS_PACKET, Result, SAMPLE_RATE};

/// Target bit rate: transparent-grade for system audio and about 160 bytes per 10 ms frame. A LAN has the room, and
/// a higher rate costs the encoder nothing in delay (GUESS, tuned by listening on the MacBook later).
pub const BITRATE_BPS: i32 = 128_000;
/// Encoder look-ahead at 48 kHz in the restricted low-delay application (2.5 ms).
pub const LOOKAHEAD_SAMPLES: i32 = 120;

pub struct FrameEncoder {
    enc: Encoder,
    out: [u8; MAX_OPUS_PACKET],
}

impl FrameEncoder {
    pub fn new() -> Result<Self> {
        let mut enc = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay)?;
        enc.set_bitrate(Bitrate::Bits(BITRATE_BPS))?;
        enc.set_vbr(true)?;
        // Constrained VBR keeps packets near the target size (about 161 bytes at 128 kbit/s); digital silence still
        // costs 3 bytes a frame, after libopus's roughly 0.6 s of hang time.
        enc.set_vbr_constraint(true)?;
        enc.set_signal(Signal::Music)?;
        enc.set_complexity(7)?;
        Ok(Self { enc, out: [0; MAX_OPUS_PACKET] })
    }

    /// Encodes one frame of [`FRAME_VALUES`] interleaved values; the packet never exceeds [`MAX_OPUS_PACKET`] because
    /// that is the output buffer libopus is given.
    pub fn encode(&mut self, frame: &[f32]) -> Result<&[u8]> {
        debug_assert_eq!(frame.len(), FRAME_VALUES);
        let n = self.enc.encode_float(frame, &mut self.out)?;
        Ok(&self.out[..n])
    }

    pub fn reset(&mut self) -> Result<()> {
        self.enc.reset_state()?;
        Ok(())
    }

    /// Look-ahead in samples per channel as the library reports it.
    pub fn lookahead(&mut self) -> Result<i32> {
        Ok(self.enc.get_lookahead()?)
    }
}

pub struct FrameDecoder {
    dec: Decoder,
}

impl FrameDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self { dec: Decoder::new(SAMPLE_RATE, Channels::Stereo)? })
    }

    /// Decodes one packet into `out` ([`FRAME_VALUES`] values); returns the samples per channel (480).
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        Ok(self.dec.decode_float(packet, out, false)?)
    }

    /// Fills a lost frame. With `following` (the packet after the lost one) libopus is asked for in-band FEC; the
    /// CELT-only packets of this crate carry none, so libopus answers with its own concealment. Without it, plain
    /// packet-loss concealment.
    pub fn conceal(&mut self, following: Option<&[u8]>, out: &mut [f32]) -> Result<usize> {
        if let Some(next) = following
            && let Ok(n) = self.dec.decode_float(next, out, true)
            && n == FRAME_SAMPLES
        {
            return Ok(n);
        }
        Ok(self.dec.decode_float(&[], out, false)?)
    }

    pub fn reset(&mut self) -> Result<()> {
        self.dec.reset_state()?;
        Ok(())
    }
}
