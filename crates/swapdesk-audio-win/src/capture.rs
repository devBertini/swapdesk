//! `AudioSource` over WASAPI loopback (event-driven, shared mode).

use std::time::{Duration, Instant};

use swapdesk_platform::{AudioBlock, AudioSource, ErrorKind, MonoMicros, Result};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    IAudioCaptureClient, IAudioClient, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::activate::activate_exclude_process;
use crate::com::{
    ComGuard, Event, activate_client, default_render_device, device_id, failure, os_error, qpc_micros,
    wave_format_48k_stereo_f32,
};
use crate::convert::{Converter, InputFormat, SampleFormat};

const BLOCK_FRAMES: usize = 480;
const BLOCK_VALUES: usize = BLOCK_FRAMES * 2;
const BLOCK_US: u64 = 10_000;
/// A packet this far past the end of the timeline is a gap that is filled with silence.
const GAP_FILL_AFTER_US: u64 = 20_000;
/// Longer gaps (sleep, device change) restart the timeline instead.
const GAP_RESYNC_US: u64 = 500_000;
/// No packet for this long means nothing is playing: the pause is filled with silence.
const IDLE_AFTER_US: u64 = 40_000;
/// How often the default endpoint is compared with the open one.
const DEVICE_POLL: Duration = Duration::from_secs(1);

/// How the source captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// Loopback of the default render endpoint.
    Loopback,
    /// Process-exclusion loopback: every process but this one's tree.
    ExcludeProcessTree(u32),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureStats {
    pub packets: u64,
    pub frames: u64,
    /// Frames of silence added for pauses and gaps.
    pub filled_frames: u64,
    /// Times the stream was reopened (device invalidated or default endpoint changed).
    pub reopened: u64,
    /// The mix format fallback was needed (the endpoint refused `AUTOCONVERTPCM`).
    pub software_conversion: bool,
}

pub(crate) struct Open {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    conv: Option<Converter>,
    /// The default endpoint's id (plain loopback only).
    device: Option<Vec<u16>>,
    mode: CaptureMode,
    /// Which of the Initialize attempts of process loopback worked (0 for the others).
    pub(crate) attempt: usize,
    _com: ComGuard,
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: stopping a stream that was started or never was is harmless.
        let _ = unsafe { self.client.Stop() };
    }
}

fn init_flags(extra: u32) -> u32 {
    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | extra
}

fn finish(
    client: IAudioClient,
    conv: Option<Converter>,
    device: Option<Vec<u16>>,
    mode: CaptureMode,
    event: Event,
    com: ComGuard,
) -> Result<Open> {
    // SAFETY: the client is initialised for event callbacks; the handle is owned by `event`, which outlives it.
    unsafe {
        client.SetEventHandle(event.0).map_err(|e| os_error("IAudioClient.SetEventHandle", &e))?;
        let capture: IAudioCaptureClient =
            client.GetService().map_err(|e| os_error("IAudioClient.GetService(IAudioCaptureClient)", &e))?;
        client.Start().map_err(|e| os_error("IAudioClient.Start", &e))?;
        Ok(Open { client, capture, event, conv, device, mode, attempt: 0, _com: com })
    }
}

/// The mix format of the endpoint, as the fallback converter's input.
fn mix_input_format(client: &IAudioClient) -> Result<(*mut WAVEFORMATEX, InputFormat)> {
    // SAFETY: GetMixFormat returns a CoTaskMem block that the caller frees; the extensible block is read only when
    // the tag says so.
    unsafe {
        let p = client.GetMixFormat().map_err(|e| os_error("IAudioClient.GetMixFormat", &e))?;
        let wf = *p;
        let tag = wf.wFormatTag;
        let bits = wf.wBitsPerSample;
        let mut float = tag == 3;
        if tag == 0xFFFE {
            let ext = *(p as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            float = sub.to_u128() >> 96 == 3;
        }
        let format = match (float, bits) {
            (true, 32) => SampleFormat::F32,
            (false, 16) => SampleFormat::I16,
            (false, 24) => SampleFormat::I24,
            (false, 32) => SampleFormat::I32,
            _ => {
                CoTaskMemFree(Some(p as *const _));
                return Err(failure(ErrorKind::Unsupported, "GetMixFormat", "unsupported sample format"));
            }
        };
        let input = InputFormat { rate: wf.nSamplesPerSec, channels: wf.nChannels, format };
        Ok((p, input))
    }
}

fn open_default_loopback() -> Result<Open> {
    let com = ComGuard::init();
    let event = Event::new()?;
    let device = default_render_device()?;
    let id = device_id(&device)?;
    let client = activate_client(&device)?;
    let fmt = wave_format_48k_stereo_f32();
    let flags = init_flags(AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY);
    // SAFETY: `fmt` is a valid WAVEFORMATEX for the call.
    let first = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &fmt, None) };
    match first {
        Ok(()) => finish(client, None, Some(id), CaptureMode::Loopback, event, com),
        Err(_) => {
            // The endpoint refused the conversion: a failed Initialize leaves the client unusable, so take a new one and
            // run in the mix format with the software converter.
            drop(client);
            let client = activate_client(&device)?;
            let (p, input) = mix_input_format(&client)?;
            // SAFETY: `p` is the endpoint's own mix format; it is freed after the call.
            let r = unsafe {
                let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, init_flags(0), 0, 0, p, None);
                CoTaskMemFree(Some(p as *const _));
                r
            };
            r.map_err(|e| os_error("IAudioClient.Initialize(mix format)", &e))?;
            finish(client, Some(Converter::new(input)), Some(id), CaptureMode::Loopback, event, com)
        }
    }
}

pub(crate) fn open_process_loopback(pid: u32) -> Result<Open> {
    let com = ComGuard::init();
    let fmt = wave_format_48k_stereo_f32();
    // The virtual device has no mix format: the format is explicit. A failed Initialize spoils the client, so every
    // attempt activates a new one.
    // Measured on Windows 11 build 26300: attempt 0 works; NOPERSIST and the conversion flags are refused with
    // AUDCLNT_E_INVALID_STREAM_FLAG (0x88890021). The 200 ms buffer is the Microsoft sample's choice, kept as a
    // fallback.
    let attempts: [(u32, i64); 2] = [(init_flags(0), 0), (init_flags(0), 200_000)];
    let mut errors = String::new();
    for (attempt, (flags, hns)) in attempts.into_iter().enumerate() {
        let client = activate_exclude_process(pid)?;
        // SAFETY: `fmt` is a valid WAVEFORMATEX for the call.
        match unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, hns, 0, &fmt, None) } {
            Ok(()) => {
                let mut open = finish(client, None, None, CaptureMode::ExcludeProcessTree(pid), Event::new()?, com)?;
                open.attempt = attempt;
                return Ok(open);
            }
            Err(e) => errors.push_str(&format!("[flags {flags:#x} buffer {hns}: {:#x}] ", e.code().0 as u32)),
        }
    }
    Err(failure(ErrorKind::Os, "IAudioClient.Initialize(process loopback)", &errors))
}

/// Every packet ready now, as (`MonoMicros` of its first sample, converted 48 kHz stereo samples), and the error that
/// stopped the reading, if any.
fn read_packets(open: &mut Open, stats: &mut CaptureStats) -> (Vec<(u64, Vec<f32>)>, Result<()>) {
    let mut packets = Vec::new();
    loop {
        // SAFETY: the capture client belongs to this open stream; the pointers are valid for the calls and the buffer
        // is read only between GetBuffer and ReleaseBuffer.
        let step: Result<Option<(u64, Vec<f32>)>> = unsafe {
            (|| {
                let size = open
                    .capture
                    .GetNextPacketSize()
                    .map_err(|e| os_error("IAudioCaptureClient.GetNextPacketSize", &e))?;
                if size == 0 {
                    return Ok(None);
                }
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut::<u8>(), 0u32, 0u32, 0u64);
                open.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))
                    .map_err(|e| os_error("IAudioCaptureClient.GetBuffer", &e))?;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let n = frames as usize;
                let mut converted = Vec::with_capacity(n * 2);
                match open.conv.as_mut() {
                    Some(conv) => {
                        let width = usize::from(conv.input().channels) * conv.input().format.bytes();
                        let bytes = if data.is_null() { &[][..] } else { std::slice::from_raw_parts(data, n * width) };
                        conv.push(bytes, n, silent, &mut converted);
                    }
                    None if silent || data.is_null() => converted.resize(n * 2, 0.0),
                    None => {
                        converted.resize(n * 2, 0.0);
                        std::ptr::copy_nonoverlapping(data, converted.as_mut_ptr().cast::<u8>(), n * 8);
                    }
                }
                open.capture.ReleaseBuffer(frames).map_err(|e| os_error("IAudioCaptureClient.ReleaseBuffer", &e))?;
                stats.packets += 1;
                stats.frames += n as u64;
                Ok(Some((qpc / 10, converted)))
            })()
        };
        match step {
            Ok(Some(p)) => packets.push(p),
            Ok(None) => return (packets, Ok(())),
            Err(e) => return (packets, Err(e)),
        }
    }
}

pub struct WasapiLoopbackSource {
    open: Option<Open>,
    exclude_pid: Option<u32>,
    /// Converted 48 kHz stereo samples not yet handed out.
    queue: Vec<f32>,
    /// `MonoMicros` of `queue[0]` when the queue is not empty.
    queue_ts: u64,
    /// `MonoMicros` of the sample after the last one queued.
    end_ts: u64,
    have_timeline: bool,
    last_packet_at: u64,
    last_poll: Instant,
    stats: CaptureStats,
}

impl Default for WasapiLoopbackSource {
    fn default() -> Self {
        Self::new()
    }
}

impl WasapiLoopbackSource {
    pub fn new() -> Self {
        Self {
            open: None,
            exclude_pid: None,
            queue: Vec::new(),
            queue_ts: 0,
            end_ts: 0,
            have_timeline: false,
            last_packet_at: 0,
            last_poll: Instant::now(),
            stats: CaptureStats::default(),
        }
    }

    /// How the running capture works: `None` before `start`. With `start(Some(pid))` this tells whether the process
    /// exclusion took effect or the source fell back to plain loopback.
    pub fn mode(&self) -> Option<CaptureMode> {
        self.open.as_ref().map(|o| o.mode)
    }

    pub fn stats(&self) -> &CaptureStats {
        &self.stats
    }

    fn open_stream(&mut self) -> Result<()> {
        let open = match self.exclude_pid {
            Some(pid) => open_process_loopback(pid).or_else(|_| open_default_loopback()),
            None => open_default_loopback(),
        }?;
        self.stats.software_conversion |= open.conv.is_some();
        self.open = Some(open);
        self.queue.clear();
        self.have_timeline = false;
        self.last_packet_at = qpc_micros();
        Ok(())
    }

    /// Closes and opens the stream again (the device was invalidated or the default endpoint changed).
    fn reopen(&mut self) -> Result<()> {
        self.open = None;
        self.stats.reopened += 1;
        self.open_stream()
    }

    fn append_silence(&mut self, frames: usize) {
        if self.queue.is_empty() {
            self.queue_ts = self.end_ts;
        }
        self.queue.resize(self.queue.len() + frames * 2, 0.0);
        self.end_ts += frames as u64 * 1_000_000 / 48_000;
        self.stats.filled_frames += frames as u64;
    }

    /// Reads every packet the engine has ready into the queue.
    fn drain(&mut self) -> Result<()> {
        let Some(open) = self.open.as_mut() else {
            return Err(failure(ErrorKind::Closed, "WasapiLoopbackSource.next_block", "not started"));
        };
        let packets = read_packets(open, &mut self.stats);
        // Whatever was read before an error is kept.
        let (packets, result) = packets;
        for (ts, converted) in packets {
            self.place(ts, converted);
            self.last_packet_at = qpc_micros();
        }
        result
    }

    /// Puts a converted packet captured at `ts` on the timeline: small differences are ignored (the audio clock is
    /// the truth), a gap up to 500 ms is filled with silence, anything else restarts the timeline.
    fn place(&mut self, ts: u64, converted: Vec<f32>) {
        if !self.have_timeline {
            self.have_timeline = true;
            self.queue.clear();
            self.queue_ts = ts;
            self.end_ts = ts;
        } else if ts > self.end_ts + GAP_FILL_AFTER_US {
            let gap = ts - self.end_ts;
            if gap <= GAP_RESYNC_US {
                self.append_silence((gap * 48 / 1000) as usize);
            } else {
                self.queue.clear();
                self.queue_ts = ts;
                self.end_ts = ts;
            }
        }
        if self.queue.is_empty() {
            self.queue_ts = self.end_ts;
        }
        let frames = converted.len() / 2;
        self.queue.extend_from_slice(&converted);
        self.end_ts += frames as u64 * 1_000_000 / 48_000;
    }

    /// Loopback delivers nothing while nothing plays: after a pause the timeline is carried forward with silence.
    fn fill_idle(&mut self) {
        let now = qpc_micros();
        if now.saturating_sub(self.last_packet_at) < IDLE_AFTER_US {
            return;
        }
        if !self.have_timeline || now.saturating_sub(self.end_ts) > 1_000_000 {
            self.queue.clear();
            self.have_timeline = true;
            self.queue_ts = now;
            self.end_ts = now;
            return;
        }
        let target = now.saturating_sub(GAP_FILL_AFTER_US);
        if target > self.end_ts {
            let blocks = (target - self.end_ts) / BLOCK_US;
            if blocks > 0 {
                self.append_silence(blocks as usize * BLOCK_FRAMES);
            }
        }
    }

    fn default_device_changed(&mut self) -> bool {
        if self.last_poll.elapsed() < DEVICE_POLL {
            return false;
        }
        self.last_poll = Instant::now();
        let Some(open) = self.open.as_ref() else {
            return false;
        };
        let Some(current) = open.device.as_ref() else {
            return false;
        };
        default_render_device().and_then(|d| device_id(&d)).is_ok_and(|id| &id != current)
    }

    fn take_block(&mut self) -> Option<AudioBlock> {
        if self.queue.len() < BLOCK_VALUES {
            return None;
        }
        let samples: Vec<f32> = self.queue.drain(..BLOCK_VALUES).collect();
        let captured_at = MonoMicros(self.queue_ts);
        self.queue_ts += BLOCK_US;
        Some(AudioBlock { captured_at, samples })
    }
}

impl AudioSource for WasapiLoopbackSource {
    /// `exclude_pid`: process-exclusion loopback leaving that process tree out; when it cannot be opened (Windows
    /// before build 20348, or the activation fails) plain loopback runs instead and [`Self::mode`] says so.
    fn start(&mut self, exclude_pid: Option<u32>) -> Result<()> {
        self.stop();
        self.exclude_pid = exclude_pid;
        self.stats = CaptureStats::default();
        self.last_poll = Instant::now();
        self.open_stream()
    }

    fn next_block(&mut self, timeout: Duration) -> Result<Option<AudioBlock>> {
        if self.open.is_none() {
            return Err(failure(ErrorKind::Closed, "WasapiLoopbackSource.next_block", "not started"));
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.default_device_changed() {
                self.reopen()?;
            }
            match self.drain() {
                Ok(()) => {}
                Err(e) if e.kind == ErrorKind::AccessLost => self.reopen()?,
                Err(e) => return Err(e),
            }
            if let Some(b) = self.take_block() {
                return Ok(Some(b));
            }
            self.fill_idle();
            if let Some(b) = self.take_block() {
                return Ok(Some(b));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let ms = left.as_millis().clamp(1, 10) as u32;
            if let Some(open) = self.open.as_ref() {
                open.event.wait(ms);
            }
        }
    }

    fn stop(&mut self) {
        self.open = None;
        self.queue.clear();
        self.have_timeline = false;
    }
}
