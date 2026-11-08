//! `AudioSink` over WASAPI render (shared mode, default endpoint), driven by the endpoint's own clock.
//!
//! The stream is event driven (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`): the audio engine signals an event handle every
//! period (10 ms on a typical endpoint) when the queue has room, and [`WasapiRenderSink::wait_room`] returns then, so
//! the caller pulls exactly the frames the device consumed from its jitter buffer. A drift between the host's capture
//! clock and this device's clock is thereby absorbed by the jitter buffer (stretch or trim, counted), never by a
//! queue that overflows and loses its tail.
//!
//! Facts used, read on learn.microsoft.com on 2026-11-08:
//! * `IAudioClient::Initialize` (https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-initialize):
//!   `hnsPeriodicity` is nonzero only in exclusive mode, so a shared-mode stream passes 0; an event-driven shared
//!   stream must call `SetEventHandle` (otherwise releasing it can stall for seconds); a requested buffer smaller
//!   than the engine's minimum is raised to that minimum.
//! * `AvSetMmThreadCharacteristicsW` (https://learn.microsoft.com/en-us/windows/win32/api/avrt/nf-avrt-avsetmmthreadcharacteristicsw):
//!   the task name must be a subkey of `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile\Tasks`
//!   and the caller reverts with `AvRevertMmThreadCharacteristics` when done. The default tasks include `Audio` and
//!   `Pro Audio` (https://learn.microsoft.com/en-us/windows/win32/procthread/multimedia-class-scheduler-service).

use std::time::Duration;

use swapdesk_platform::{AudioBlock, AudioSink, ErrorKind, Result, SinkCounts};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioRenderClient,
};
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW};
use windows::core::w;

use crate::com::{
    ComGuard, Event, activate_client, default_render_device, failure, os_error, wave_format_48k_stereo_f32,
};

/// The buffer asked of the engine, in 100 ns units (30 ms). The engine raises it to its own minimum when that is
/// larger; the real size is read back after `Initialize`.
const BUFFER_HNS: i64 = 300_000;
/// The fill mark: the sink keeps about this many frames queued (20 ms at 48 kHz), never more than the buffer holds.
const MARK_FRAMES: u32 = 960;
/// Frames in one 10 ms block.
const BLOCK_FRAMES: u32 = 480;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub blocks: u64,
    pub frames: u64,
    /// Frames that did not fit in the queue and were dropped (an overrun; counted, never silent).
    pub dropped_frames: u64,
    /// Wake-ups that found the queue empty while the stream was running.
    pub underruns: u64,
    /// Wake-ups signalled by the endpoint's event.
    pub wakes: u64,
}

/// A thread registered with MMCSS; the registration is reverted on drop.
struct Mmcss(HANDLE);

impl Mmcss {
    fn register() -> Option<Self> {
        let mut index = 0u32;
        for name in [w!("Pro Audio"), w!("Audio")] {
            // SAFETY: `name` is a static NUL-terminated wide string and `index` a valid out pointer; a zero index is
            // what the first call must pass.
            if let Ok(h) = unsafe { AvSetMmThreadCharacteristicsW(name, &mut index) } {
                return Some(Self(h));
            }
            index = 0;
        }
        None
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on the thread that owns this value.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

struct Open {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
    event: Event,
    // Field order is drop order: revert MMCSS, then release COM.
    _mmcss: Option<Mmcss>,
    _com: ComGuard,
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: stopping a stream that was started or never was is harmless.
        let _ = unsafe { self.client.Stop() };
    }
}

#[derive(Default)]
pub struct WasapiRenderSink {
    open: Option<Open>,
    stats: RenderStats,
}

impl WasapiRenderSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> &RenderStats {
        &self.stats
    }

    /// Frames the endpoint buffer holds (known after `start`).
    pub fn buffer_frames(&self) -> Option<u32> {
        self.open.as_ref().map(|o| o.buffer_frames)
    }

    /// True when the thread that called `start` is registered with MMCSS.
    pub fn mmcss_registered(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o._mmcss.is_some())
    }

    fn padding(open: &Open) -> Result<u32> {
        // SAFETY: plain COM call on this stream's client.
        unsafe { open.client.GetCurrentPadding() }.map_err(|e| os_error("IAudioClient.GetCurrentPadding", &e))
    }

    fn write_frames(&mut self, samples: &[f32]) -> Result<()> {
        let Some(open) = self.open.as_ref() else {
            return Err(failure(ErrorKind::Closed, "WasapiRenderSink.write", "not started"));
        };
        let frames = samples.len() / 2;
        let padding = Self::padding(open)?;
        let free = open.buffer_frames.saturating_sub(padding) as usize;
        let n = frames.min(free);
        self.stats.blocks += 1;
        self.stats.dropped_frames += (frames - n) as u64;
        if n == 0 {
            return Ok(());
        }
        // SAFETY: the render buffer returned by GetBuffer holds `n` frames of 8 bytes and is written only before
        // ReleaseBuffer; the source slice holds at least `n * 2` values.
        unsafe {
            let dst = open.render.GetBuffer(n as u32).map_err(|e| os_error("IAudioRenderClient.GetBuffer", &e))?;
            std::ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), dst, n * 8);
            open.render.ReleaseBuffer(n as u32, 0).map_err(|e| os_error("IAudioRenderClient.ReleaseBuffer", &e))?;
        }
        self.stats.frames += n as u64;
        Ok(())
    }
}

impl AudioSink for WasapiRenderSink {
    fn start(&mut self) -> Result<()> {
        self.stop();
        self.stats = RenderStats::default();
        let com = ComGuard::init();
        // The thread that opens the stream is the one that serves it.
        let mmcss = Mmcss::register();
        let device = default_render_device()?;
        let client = activate_client(&device)?;
        let fmt = wave_format_48k_stereo_f32();
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let event = Event::new()?;
        // SAFETY: `fmt` is a valid WAVEFORMATEX for the call; the interfaces belong to this client; the event handle
        // is owned by `Open` and outlives the stream.
        let open = unsafe {
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &fmt, None)
                .map_err(|e| os_error("IAudioClient.Initialize(render)", &e))?;
            client.SetEventHandle(event.0).map_err(|e| os_error("IAudioClient.SetEventHandle(render)", &e))?;
            let buffer_frames = client.GetBufferSize().map_err(|e| os_error("IAudioClient.GetBufferSize", &e))?;
            let render: IAudioRenderClient =
                client.GetService().map_err(|e| os_error("IAudioClient.GetService(IAudioRenderClient)", &e))?;
            client.Start().map_err(|e| os_error("IAudioClient.Start", &e))?;
            Open { client, render, buffer_frames, event, _mmcss: mmcss, _com: com }
        };
        self.open = Some(open);
        Ok(())
    }

    fn write(&mut self, block: &AudioBlock) -> Result<()> {
        self.write_frames(&block.samples)
    }

    fn queued(&self) -> Duration {
        let Some(open) = self.open.as_ref() else {
            return Duration::ZERO;
        };
        let padding = Self::padding(open).unwrap_or(0);
        Duration::from_micros(u64::from(padding) * 1_000_000 / 48_000)
    }

    fn stop(&mut self) {
        self.open = None;
    }

    fn device_paced(&self) -> bool {
        true
    }

    fn wait_room(&mut self, timeout: Duration) -> Result<usize> {
        let Some(open) = self.open.as_ref() else {
            return Err(failure(ErrorKind::Closed, "WasapiRenderSink.wait_room", "not started"));
        };
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        if !open.event.wait(ms) {
            return Ok(0);
        }
        self.stats.wakes += 1;
        let padding = Self::padding(open)?;
        if padding == 0 {
            self.stats.underruns += 1;
        }
        let mark = MARK_FRAMES.min(open.buffer_frames);
        Ok((mark.saturating_sub(padding) / BLOCK_FRAMES) as usize)
    }

    fn prime(&mut self) -> Result<()> {
        self.write_frames(&[0.0; BLOCK_FRAMES as usize * 2])
    }

    fn counts(&self) -> SinkCounts {
        SinkCounts { underruns: self.stats.underruns, overrun_frames: self.stats.dropped_frames }
    }
}
