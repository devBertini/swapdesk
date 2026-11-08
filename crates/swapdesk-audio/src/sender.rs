//! The host's audio path: `AudioSource` blocks in, audio datagrams out.
//!
//! Audio off (the default, D16): the source is never started and `poll` returns nothing without touching it, so
//! nothing is captured, encoded or sent. `set_enabled(true)` follows the viewer's `AudioControl { enabled: true }`
//! with `AUDIO` granted; `set_enabled(false)` stops the source.

use std::collections::VecDeque;
use std::time::Duration;

use swapdesk_platform::{AudioBlock, AudioSource};
use swapdesk_proto::AudioHeader;

use crate::codec::FrameEncoder;
use crate::packet::build_datagram;
use crate::{CHANNELS, FRAME_MS, FRAME_US, FRAME_VALUES, Result, SAMPLE_RATE};

/// A block whose timestamp is further than this from where the previous one ended is a capture gap (device change,
/// stall): the partial frame is dropped and the next packet carries DISCONTINUITY.
const GAP_US: u64 = 30_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderStats {
    pub datagrams: u64,
    pub bytes: u64,
    pub muted_datagrams: u64,
    pub discontinuities: u64,
}

pub struct AudioSender<S: AudioSource> {
    source: S,
    enc: FrameEncoder,
    exclude_pid: Option<u32>,
    enabled: bool,
    muted: bool,
    seq: u32,
    pending: Vec<f32>,
    pending_start_us: u64,
    next_expected_us: Option<u64>,
    discontinuity: bool,
    out: VecDeque<Vec<u8>>,
    stats: SenderStats,
}

impl<S: AudioSource> AudioSender<S> {
    /// Audio is off until [`Self::set_enabled`]. `exclude_pid` is passed to `AudioSource::start` (leave the viewer's
    /// process tree out of the capture when both run on this PC).
    pub fn new(source: S, exclude_pid: Option<u32>) -> Result<Self> {
        Ok(Self {
            source,
            enc: FrameEncoder::new()?,
            exclude_pid,
            enabled: false,
            muted: false,
            seq: 0,
            pending: Vec::with_capacity(FRAME_VALUES * 2),
            pending_start_us: 0,
            next_expected_us: None,
            discontinuity: false,
            out: VecDeque::new(),
            stats: SenderStats::default(),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn stats(&self) -> &SenderStats {
        &self.stats
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// `AudioControl`: on opens the capture, off closes it and drops everything queued.
    pub fn set_enabled(&mut self, on: bool) -> Result<()> {
        if on == self.enabled {
            return Ok(());
        }
        if on {
            self.source.start(self.exclude_pid)?;
            self.enc.reset()?;
            self.discontinuity = true;
        } else {
            self.source.stop();
        }
        self.enabled = on;
        self.pending.clear();
        self.next_expected_us = None;
        self.out.clear();
        Ok(())
    }

    /// While muted the capture keeps running (so unmuting has no stale audio) but frames go out as header-only
    /// datagrams with the MUTED flag: the viewer plays silence and shows it, and nothing is encoded.
    pub fn set_muted(&mut self, muted: bool) -> Result<()> {
        if self.muted && !muted {
            self.enc.reset()?;
        }
        self.muted = muted;
        Ok(())
    }

    /// The next datagram, waiting at most `timeout` for the source; `None` when audio is off or no whole frame is
    /// ready.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        if !self.enabled {
            return Ok(None);
        }
        if let Some(d) = self.out.pop_front() {
            return Ok(Some(d));
        }
        let Some(block) = self.source.next_block(timeout)? else {
            return Ok(None);
        };
        self.ingest(&block)?;
        Ok(self.out.pop_front())
    }

    fn ingest(&mut self, block: &AudioBlock) -> Result<()> {
        let values = block.samples.len() / CHANNELS * CHANNELS;
        let block_us = (values / CHANNELS) as u64 * 1_000_000 / u64::from(SAMPLE_RATE);
        let at = block.captured_at.0;
        if self.next_expected_us.is_some_and(|e| at.abs_diff(e) > GAP_US) {
            self.discontinuity = true;
            self.pending.clear();
        }
        if self.pending.is_empty() {
            self.pending_start_us = at;
        }
        self.next_expected_us = Some(at + block_us);
        self.pending.extend_from_slice(&block.samples[..values]);
        while self.pending.len() >= FRAME_VALUES {
            let header = AudioHeader {
                discontinuity: self.discontinuity,
                muted: self.muted,
                frame_ms: FRAME_MS,
                seq: self.seq,
                capture_us: self.pending_start_us,
            };
            let datagram = if self.muted {
                build_datagram(&header, &[])?
            } else {
                let payload = self.enc.encode(&self.pending[..FRAME_VALUES])?;
                build_datagram(&header, payload)?
            };
            self.pending.drain(..FRAME_VALUES);
            self.pending_start_us += FRAME_US;
            self.seq = self.seq.wrapping_add(1);
            if self.discontinuity {
                self.stats.discontinuities += 1;
            }
            self.discontinuity = false;
            self.stats.datagrams += 1;
            self.stats.bytes += datagram.len() as u64;
            if self.muted {
                self.stats.muted_datagrams += 1;
            }
            self.out.push_back(datagram);
        }
        Ok(())
    }
}
