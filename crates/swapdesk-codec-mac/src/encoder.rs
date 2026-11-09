//! `VideoEncoder` over a VideoToolbox `VTCompressionSession` (H.264).
//!
//! What the session is told, and why (it mirrors `swapdesk-codec-win`, `preset.rs` and `encoder.rs`):
//! - Hardware encoder required through the encoder specification (`RequireHardwareAcceleratedVideoEncoder`), with
//!   low-latency rate control (`EnableLowLatencyRateControl`); when the machine refuses the pair the session is made
//!   again with the hardware only enabled, then with neither, and [`EncoderInfo`] reports what was really obtained
//!   (`hardware` from `kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder`, `low_latency` from the
//!   specification the session accepted).
//! - `RealTime` true, no frame reordering (one frame in, at most one frame out, in order), High profile with automatic
//!   level (Main when High is refused), `AverageBitRate`, `DataRateLimits` (1.5x the mean per second), `ExpectedFrameRate`,
//!   a key frame interval so long that key frames come on demand only (`force_keyframe`, a size change), as on
//!   Windows. `PrepareToEncodeFrames` runs at configure time.
//! - The presets are those of `swapdesk-codec-win`: Texto nítido encodes at most 30 frames per second (frames arriving
//!   faster are skipped by capture time, 10% slack) and spends twice the bits; Auto and Movimento keep the requested
//!   numbers. The level reported is the smallest `level_idc` that fits size and frame rate (Table A-1), as on Windows.
//! - A frame of another size than the configured one rebuilds the session (new SPS/PPS); the first output of every
//!   session is a keyframe with `config_change: true`.
//!
//! Input is `Pixels::Gpu` holding a `CFRetained<CVPixelBuffer>` (NV12 `420f` from the screen lane, or BGRA; VideoToolbox
//! converts what the encoder does not take) or `Pixels::Cpu` BGRA, copied into a buffer of the session's pool.
//!
//! Output wait. `encode` returns the frame's own output in the same call. Two ways were measured on the Mac
//! (`OutputWait`; `tests/encode.rs::wait_modes_compared` prints them): waiting on a condvar that the output callback
//! signals, and `VTCompressionSessionCompleteFrames` up to the frame's timestamp. Measured on 2026-11-08 (MacBook, macOS
//! 27.2 arm64, hardware encoder, 1280x720 BGRA from the CPU at 8 Mbps, 90 frames each, debug build, `encode_us` from
//! submission to the callback): both modes returned every frame in the call that submitted it, with the same latency
//! (gradient: median 5.2 to 5.3 ms, p95 5.4 ms for both; busy grain: median 5.0 to 5.1 ms, p95 5.5 to 6.0 ms; maximum
//! 17 to 39 ms, the first frame of a session). The condvar gives up after 100 ms and then returns nothing (the frame
//! comes out of a later call), so `CompleteFrames`, which never returns before the frame is out, is the default
//! ([`DEFAULT_WAIT`]). A frame the encoder drops itself (its rate control, seen with full-frame random noise) has no
//! output in either mode; [`VtH264Encoder::dropped_frames`] counts them.
//!
//! Sources read on 2026-11-08:
//! - `VTCompressionSession`, `VTCompressionSessionCreate`, `EncodeFrame`, `CompleteFrames`, the output callback:
//!   <https://developer.apple.com/documentation/videotoolbox/vtcompressionsession> and
//!   <https://docs.rs/objc2-video-toolbox/0.3.2/objc2_video_toolbox/struct.VTCompressionSession.html>.
//! - Compression properties and encoder specification keys (`RealTime`, `AllowFrameReordering`, `ProfileLevel`,
//!   `AverageBitRate`, `DataRateLimits` as a CFArray of bytes and seconds, `ExpectedFrameRate`, `MaxKeyFrameInterval`,
//!   `kVTEncodeFrameOptionKey_ForceKeyFrame`, `kVTVideoEncoderSpecification_EnableLowLatencyRateControl`):
//!   <https://developer.apple.com/documentation/videotoolbox/compression-properties> and
//!   <https://docs.rs/objc2-video-toolbox/0.3.2/objc2_video_toolbox/>.
//! - Samples: key frames are the samples without `kCMSampleAttachmentKey_NotSync`
//!   (<https://developer.apple.com/documentation/coremedia/kcmsampleattachmentkey_notsync>); the parameter sets come from
//!   `CMVideoFormatDescriptionGetH264ParameterSetAtIndex`
//!   (<https://developer.apple.com/documentation/coremedia/cmvideoformatdescriptiongeth264parametersetatindex(_:parametersetindex:parametersetpointerout:parametersetsizeout:parametersetcountout:nalunitheaderlengthout:)>).
//!
//! The pure part (preset plan, level, rate numbers, timestamps) is in this file's top half and is tested on every host.

use swapdesk_platform::MonoMicros;
use swapdesk_proto::Preset;

/// Largest bitrate the encoder is ever asked for, in kbps (as `swapdesk-codec-win`).
pub const MAX_KBPS: u32 = 200_000;
/// Frame-rate cap of Texto nítido.
pub const TEXT_MAX_FPS: u32 = 30;

/// The numbers a request turns into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// Frames per second arriving from capture.
    pub input_fps: u32,
    /// Frames per second the encoder encodes (the preset cap applied).
    pub output_fps: u32,
    /// Target bitrate after the preset multiplier, kbps.
    pub kbps: u32,
}

pub fn effective(preset: Preset, kbps: u32, fps: u16) -> Effective {
    let (max_fps, percent) = match preset {
        Preset::Auto | Preset::Motion => (u32::MAX, 100u64),
        Preset::Text => (TEXT_MAX_FPS, 200u64),
    };
    let input_fps = u32::from(fps).max(1);
    Effective {
        input_fps,
        output_fps: input_fps.min(max_fps),
        kbps: (u64::from(kbps) * percent / 100).min(u64::from(MAX_KBPS)) as u32,
    }
}

/// H.264 `level_idc` for a picture size and frame rate (Table A-1 macroblock rates and frame sizes).
pub fn level_idc(width: u32, height: u32, fps: u32) -> u32 {
    let mbs = width.div_ceil(16) * height.div_ceil(16);
    let rate = u64::from(mbs) * u64::from(fps);
    // (level_idc, MaxFS, MaxMBPS)
    const LEVELS: [(u32, u32, u64); 10] = [
        (30, 1620, 40_500),
        (31, 3600, 108_000),
        (32, 5120, 216_000),
        (40, 8192, 245_760),
        (42, 8704, 522_240),
        (50, 22_080, 589_824),
        (51, 36_864, 983_040),
        (52, 36_864, 2_073_600),
        (60, 139_264, 4_177_920),
        (62, 139_264, 16_711_680),
    ];
    LEVELS.iter().find(|&&(_, fs, mbps)| mbs <= fs && rate <= mbps).map_or(62, |&(l, _, _)| l)
}

/// `DataRateLimits` for a mean bitrate: bytes allowed in a one second window, 1.5x the mean.
pub fn data_rate_limit_bytes(kbps: u32) -> i64 {
    i64::from(kbps) * 1000 / 8 * 3 / 2
}

/// The presentation timestamp (microseconds) for a frame captured at `captured`: the capture time, bumped when it does
/// not exceed the last one, since a compression session wants strictly increasing timestamps.
pub fn next_pts(last: Option<i64>, captured: MonoMicros) -> i64 {
    let wanted = i64::try_from(captured.0).unwrap_or(i64::MAX - 1);
    match last {
        Some(l) if wanted <= l => l + 1,
        _ => wanted,
    }
}

/// Microseconds between encoded frames when the preset caps the rate below the arriving one, else 0.
pub fn skip_interval_us(eff: &Effective) -> u64 {
    if eff.output_fps < eff.input_fps { 1_000_000 / u64::from(eff.output_fps) } else { 0 }
}

/// True when a frame arrives too soon after the last encoded one (10% slack, as `swapdesk-codec-win`).
pub fn skip_frame(interval_us: u64, last_encoded: Option<u64>, captured: MonoMicros, forced: bool) -> bool {
    interval_us > 0
        && !forced
        && last_encoded.is_some_and(|last| captured.0.saturating_sub(last) < interval_us * 9 / 10)
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    #[test]
    fn text_halves_the_frame_rate_and_doubles_the_bits() {
        let e = effective(Preset::Text, 8000, 60);
        assert_eq!((e.input_fps, e.output_fps, e.kbps), (60, 30, 16_000));
        let a = effective(Preset::Auto, 8000, 60);
        assert_eq!((a.input_fps, a.output_fps, a.kbps), (60, 60, 8000));
        let m = effective(Preset::Motion, 8000, 120);
        assert_eq!((m.output_fps, m.kbps), (120, 8000));
        assert_eq!(effective(Preset::Text, 150_000, 24).kbps, MAX_KBPS);
        assert_eq!(effective(Preset::Text, 8000, 24).output_fps, 24);
        assert_eq!(effective(Preset::Auto, 8000, 0).input_fps, 1);
    }

    #[test]
    fn levels_for_common_sizes() {
        assert_eq!(level_idc(1920, 1080, 30), 40);
        assert_eq!(level_idc(1920, 1080, 60), 42);
        assert_eq!(level_idc(2560, 1440, 60), 51);
        assert_eq!(level_idc(3840, 2160, 30), 51);
        assert_eq!(level_idc(3840, 2160, 60), 52);
        assert_eq!(level_idc(1280, 720, 30), 31);
    }

    #[test]
    fn data_rate_limit_is_one_and_a_half_times_the_mean() {
        // 8000 kbps = 1_000_000 bytes per second.
        assert_eq!(data_rate_limit_bytes(8000), 1_500_000);
        assert_eq!(data_rate_limit_bytes(0), 0);
    }

    #[test]
    fn timestamps_increase_strictly() {
        assert_eq!(next_pts(None, MonoMicros(5)), 5);
        assert_eq!(next_pts(Some(4), MonoMicros(5)), 5);
        assert_eq!(next_pts(Some(5), MonoMicros(5)), 6);
        assert_eq!(next_pts(Some(9), MonoMicros(5)), 10);
        assert!(next_pts(None, MonoMicros(u64::MAX)) > 0);
    }

    #[test]
    fn frames_arriving_too_soon_are_skipped_under_a_cap() {
        let e = effective(Preset::Text, 4000, 60);
        let interval = skip_interval_us(&e);
        assert_eq!(interval, 33_333);
        // 16.6 ms after the last encoded frame: skipped; 30 ms (above 90% of the interval): kept.
        assert!(skip_frame(interval, Some(1_000_000), MonoMicros(1_016_666), false));
        assert!(!skip_frame(interval, Some(1_000_000), MonoMicros(1_030_000), false));
        assert!(!skip_frame(interval, Some(1_000_000), MonoMicros(1_016_666), true));
        assert!(!skip_frame(interval, None, MonoMicros(5), false));
        assert_eq!(skip_interval_us(&effective(Preset::Auto, 4000, 60)), 0);
        assert!(!skip_frame(0, Some(1), MonoMicros(2), false));
    }
}

#[cfg(target_os = "macos")]
pub use vt::{DEFAULT_WAIT, OutputWait, VtH264Encoder};

#[cfg(target_os = "macos")]
mod vt {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
    use objc2_core_media::{
        CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeIndefinite,
        kCMVideoCodecType_H264,
    };
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
        kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
        kCVPixelFormatType_32BGRA, kCVReturnSuccess,
    };
    use objc2_video_toolbox::{
        VTCompressionSession, VTEncodeInfoFlags, VTSessionCopyProperty, VTSessionSetProperty,
        kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
        kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
        kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
        kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
        kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, kVTEncodeFrameOptionKey_ForceKeyFrame,
        kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_H264_Main_AutoLevel,
        kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
        kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
    };
    use swapdesk_platform::{
        Clock, EncodedFrame, EncoderConfig, EncoderInfo, ErrorKind, Frame, MonoMicros, Pixels, Result, VideoEncoder,
    };
    use swapdesk_proto::H264Profile;

    use super::{Effective, data_rate_limit_bytes, effective, level_idc, next_pts, skip_frame, skip_interval_us};
    use crate::annexb::{self, AVCC_LENGTH_SIZE};
    use crate::sys::{err, status_err};

    /// How long `encode` waits for the frame's output when waiting on the condvar.
    const OUTPUT_WAIT: Duration = Duration::from_millis(100);
    /// Timescale of the presentation timestamps: microseconds, the unit of `captured_at`.
    const TIMESCALE: i32 = 1_000_000;

    /// How `encode` gets the frame's own output back in the same call.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OutputWait {
        /// Wait (up to 100 ms) on a condvar that the output callback signals.
        Condvar,
        /// `VTCompressionSessionCompleteFrames` up to the frame's timestamp.
        CompleteFrames,
    }

    /// The mode in use unless [`VtH264Encoder::set_output_wait`] says otherwise: see the module documentation.
    pub const DEFAULT_WAIT: OutputWait = OutputWait::CompleteFrames;

    enum Landed {
        Frame { keyframe: bool, annex_b: Vec<u8> },
        Dropped,
        Failed { status: i32, what: &'static str },
    }

    /// One callback result: the frame's timestamp (the source frame refcon) and when it landed.
    struct Out {
        pts: i64,
        done_at: u64,
        landed: Landed,
    }

    /// What the output callback shares with `encode`.
    struct Shared {
        clock: Arc<dyn Clock>,
        outs: Mutex<Vec<Out>>,
        signal: Condvar,
    }

    /// Reads one encoded sample: the key frame flag and the Annex B bytes (parameter sets first on a key frame).
    fn extract(sample: &CMSampleBuffer) -> std::result::Result<(bool, Vec<u8>), (i32, &'static str)> {
        // SAFETY: `sample` is live for the call; the calls only read it and return retained or borrowed CF objects.
        let keyframe = unsafe { is_sync_sample(sample) };
        // SAFETY: as above.
        let block = unsafe { sample.data_buffer() }.ok_or((-1, "sample without data"))?;
        // SAFETY: as above.
        let len = unsafe { block.data_length() };
        let mut avcc = vec![0u8; len];
        if len > 0 {
            let dest = NonNull::new(avcc.as_mut_ptr().cast::<c_void>()).ok_or((-1, "empty buffer"))?;
            // SAFETY: copies `len` bytes of the block (reachable range 0..len) into `avcc`, which holds exactly `len`.
            let status = unsafe { block.copy_data_bytes(0, len, dest) };
            if status != 0 {
                return Err((status, "CMBlockBufferCopyDataBytes"));
            }
        }
        let sets = if keyframe {
            // SAFETY: as above.
            let format = unsafe { sample.format_description() }.ok_or((-1, "keyframe without format description"))?;
            let sets = parameter_sets(&format)?;
            Some(sets)
        } else {
            None
        };
        let prefix: Vec<&[u8]> = sets.iter().flatten().map(Vec::as_slice).collect();
        let annex_b = annexb::avcc_to_annexb(&prefix, &avcc, AVCC_LENGTH_SIZE).ok_or((-1, "corrupt AVCC sample"))?;
        Ok((keyframe, annex_b))
    }

    /// True unless the sample carries `kCMSampleAttachmentKey_NotSync` set to true.
    ///
    /// # Safety
    /// `sample` must be a live sample buffer.
    unsafe fn is_sync_sample(sample: &CMSampleBuffer) -> bool {
        // SAFETY: the caller guarantees a live sample.
        let Some(array) = (unsafe { sample.sample_attachments_array(false) }) else { return true };
        // SAFETY: the array holds one CFDictionary per sample; casting the element type is how the typed API reads it.
        let array = unsafe {
            CFRetained::cast_unchecked::<objc2_core_foundation::CFArray<CFDictionary<CFString, CFType>>>(array)
        };
        let Some(dict) = array.get(0) else { return true };
        // SAFETY: the key is the framework's own static, valid for the process lifetime.
        let key = unsafe { objc2_core_media::kCMSampleAttachmentKey_NotSync };
        match dict.get(key).and_then(|v| v.downcast::<CFBoolean>().ok()) {
            Some(not_sync) => !not_sync.value(),
            None => true,
        }
    }

    /// SPS then PPS of the sample's format description, each without start code.
    fn parameter_sets(
        format: &objc2_core_media::CMFormatDescription,
    ) -> std::result::Result<Vec<Vec<u8>>, (i32, &'static str)> {
        let mut count = 0usize;
        // SAFETY: only the count is asked for; the other out pointers may be null.
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut count,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err((status, "CMVideoFormatDescriptionGetH264ParameterSetAtIndex"));
        }
        let mut sets = Vec::with_capacity(count);
        for i in 0..count {
            let mut p: *const u8 = ptr::null();
            let mut size = 0usize;
            // SAFETY: valid out pointers; the returned memory belongs to `format`, which outlives the copy below.
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format,
                    i,
                    &raw mut p,
                    &raw mut size,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if status != 0 || p.is_null() {
                return Err((status, "CMVideoFormatDescriptionGetH264ParameterSetAtIndex"));
            }
            // SAFETY: `p` points to `size` readable bytes owned by `format`.
            sets.push(unsafe { std::slice::from_raw_parts(p, size) }.to_vec());
        }
        Ok(sets)
    }

    /// `VTCompressionOutputCallback`: stores the sample's result for `encode`.
    ///
    /// # Safety
    /// `refcon` is the address of the `Shared` of the session that owns this callback (the session is invalidated before
    /// it is freed); `sample` is null or a live sample buffer for the duration of the call; `frame_refcon` is the
    /// timestamp `encode` passed.
    unsafe extern "C-unwind" fn on_output(
        refcon: *mut c_void,
        frame_refcon: *mut c_void,
        status: i32,
        flags: VTEncodeInfoFlags,
        sample: *mut CMSampleBuffer,
    ) {
        // SAFETY: see the function's contract.
        let Some(shared) = (unsafe { refcon.cast::<Shared>().as_ref() }) else { return };
        let pts = frame_refcon as usize as i64;
        let landed = if status != 0 {
            Landed::Failed { status, what: "VTCompressionOutputCallback" }
        } else if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            Landed::Dropped
        } else {
            // SAFETY: see the function's contract.
            match unsafe { sample.as_ref() } {
                None => Landed::Dropped,
                Some(s) => match extract(s) {
                    Ok((keyframe, annex_b)) => Landed::Frame { keyframe, annex_b },
                    Err((status, what)) => Landed::Failed { status, what },
                },
            }
        };
        let done_at = shared.clock.now().0;
        if let Ok(mut outs) = shared.outs.lock() {
            outs.push(Out { pts, done_at, landed });
        }
        shared.signal.notify_all();
    }

    fn time(value: i64) -> CMTime {
        CMTime { value, timescale: TIMESCALE, flags: CMTimeFlags::Valid, epoch: 0 }
    }

    /// A running session with the callback cell it reports into.
    struct Session {
        session: CFRetained<VTCompressionSession>,
        shared: Box<Shared>,
        width: u32,
        height: u32,
        hardware: bool,
        low_latency: bool,
        profile: H264Profile,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // SAFETY: finishes pending frames, then tears the session down before its callback cell (`shared`, dropped
            // after this) goes away; no callback runs after `invalidate` returns.
            unsafe {
                self.session.complete_frames(kCMTimeIndefinite);
                self.session.invalidate();
            }
        }
    }

    /// A frame handed to the session whose output has not been collected.
    struct Pending {
        pts: i64,
        captured_at: MonoMicros,
        started: u64,
    }

    /// `VideoEncoder` over VideoToolbox. See the module documentation. Created and used on one thread.
    pub struct VtH264Encoder {
        clock: Arc<dyn Clock>,
        wait: OutputWait,
        cfg: Option<EncoderConfig>,
        eff: Effective,
        session: Option<Session>,
        pending: VecDeque<Pending>,
        last_pts: Option<i64>,
        last_encoded_at: Option<u64>,
        skip_interval: u64,
        config_pending: bool,
        dropped: u64,
    }

    impl VtH264Encoder {
        pub fn new(clock: Arc<dyn Clock>) -> Self {
            Self {
                clock,
                wait: DEFAULT_WAIT,
                cfg: None,
                eff: Effective { input_fps: 1, output_fps: 1, kbps: 0 },
                session: None,
                pending: VecDeque::new(),
                last_pts: None,
                last_encoded_at: None,
                skip_interval: 0,
                config_pending: true,
                dropped: 0,
            }
        }

        /// Frames the encoder dropped itself (its rate control, or no output for them) since this object was made.
        pub fn dropped_frames(&self) -> u64 {
            self.dropped
        }

        /// Chooses how `encode` waits for its output (measurements; the default is [`DEFAULT_WAIT`]).
        pub fn set_output_wait(&mut self, wait: OutputWait) {
            self.wait = wait;
        }

        /// True once `configure` built a session.
        pub fn is_configured(&self) -> bool {
            self.session.is_some()
        }

        fn build(&mut self, cfg: &EncoderConfig) -> Result<EncoderInfo> {
            // The old session finishes its frames and goes first.
            self.session = None;
            self.pending.clear();
            if cfg.width == 0 || cfg.height == 0 || cfg.width > 16_384 || cfg.height > 16_384 {
                return Err(err(ErrorKind::InvalidInput, "configure", format!("size {}x{}", cfg.width, cfg.height)));
            }
            let eff = effective(cfg.preset, cfg.kbps, cfg.fps);
            let session = make_session(cfg, &eff, self.clock.clone())?;
            let info = EncoderInfo {
                name: if session.hardware {
                    "VideoToolbox H.264 (hardware)".to_owned()
                } else {
                    "VideoToolbox H.264 (software)".to_owned()
                },
                hardware: session.hardware,
                profile: session.profile,
                level: level_idc(cfg.width, cfg.height, eff.output_fps) as u8,
                low_latency: session.low_latency,
            };
            self.session = Some(session);
            self.cfg = Some(*cfg);
            self.eff = eff;
            self.skip_interval = skip_interval_us(&eff);
            self.last_encoded_at = None;
            self.config_pending = true;
            Ok(info)
        }

        /// Collects what the callback left, matching it to the frames that were handed in.
        fn drain(&mut self) -> Result<Vec<EncodedFrame>> {
            let Some(session) = self.session.as_ref() else { return Ok(Vec::new()) };
            let mut outs = match session.shared.outs.lock() {
                Ok(mut g) => std::mem::take(&mut *g),
                Err(_) => return Err(err(ErrorKind::Os, "encode", "output cell poisoned")),
            };
            outs.sort_by_key(|o| o.pts);
            let (width, height) = (session.width as u16, session.height as u16);
            let mut frames = Vec::new();
            for out in outs {
                let idx = self.pending.iter().position(|p| p.pts == out.pts);
                let pending = idx.and_then(|i| self.pending.remove(i));
                // Frames older than this one that never came back (dropped by the encoder) are forgotten.
                self.pending.retain(|p| p.pts > out.pts);
                match out.landed {
                    Landed::Failed { status, what } => return Err(status_err(what, status)),
                    Landed::Dropped => self.dropped += 1,
                    Landed::Frame { keyframe, annex_b } => {
                        let (captured_at, started) = pending
                            .map_or((MonoMicros(out.pts.max(0) as u64), out.done_at), |p| (p.captured_at, p.started));
                        let config_change = keyframe && self.config_pending;
                        if config_change {
                            self.config_pending = false;
                        }
                        frames.push(EncodedFrame {
                            captured_at,
                            encode_us: out.done_at.saturating_sub(started).min(u64::from(u32::MAX)) as u32,
                            keyframe,
                            config_change,
                            width,
                            height,
                            annex_b,
                        });
                    }
                }
            }
            Ok(frames)
        }

        /// Waits for the output of the frame stamped `pts`, then drains.
        fn collect(&mut self, pts: i64) -> Result<Vec<EncodedFrame>> {
            let wait = self.wait;
            if let Some(session) = self.session.as_ref() {
                match wait {
                    OutputWait::CompleteFrames => {
                        // SAFETY: a live session; emits every frame up to `pts` before returning.
                        let status = unsafe { session.session.complete_frames(time(pts)) };
                        if status != 0 {
                            return Err(status_err("VTCompressionSessionCompleteFrames", status));
                        }
                    }
                    OutputWait::Condvar => {
                        if let Ok(guard) = session.shared.outs.lock() {
                            // A timeout is not an error: the output is collected by a later call.
                            let _ = session
                                .shared
                                .signal
                                .wait_timeout_while(guard, OUTPUT_WAIT, |outs| !outs.iter().any(|o| o.pts >= pts));
                        }
                    }
                }
            }
            self.drain()
        }

        fn source_buffer(&self, frame: &Frame) -> Result<CFRetained<CVPixelBuffer>> {
            let session = self.session.as_ref().ok_or_else(|| err(ErrorKind::Closed, "encode", "not configured"))?;
            match &frame.pixels {
                Pixels::Gpu(surface) => {
                    let buffer = surface.0.downcast_ref::<CFRetained<CVPixelBuffer>>().ok_or_else(|| {
                        err(ErrorKind::InvalidInput, "encode", "GpuSurface is not a CFRetained<CVPixelBuffer>")
                    })?;
                    let (w, h) = (CVPixelBufferGetWidth(buffer) as u32, CVPixelBufferGetHeight(buffer) as u32);
                    if (w, h) != (frame.width, frame.height) {
                        return Err(err(
                            ErrorKind::InvalidInput,
                            "encode",
                            format!("pixel buffer is {w}x{h}, frame says {}x{}", frame.width, frame.height),
                        ));
                    }
                    Ok(buffer.clone())
                }
                Pixels::Cpu { stride, data } => {
                    let (w, h, stride) = (frame.width as usize, frame.height as usize, *stride as usize);
                    let row = w * 4;
                    if stride < row || data.len() < stride * (h - 1) + row {
                        return Err(err(
                            ErrorKind::InvalidInput,
                            "encode",
                            format!("{} bytes do not hold {w}x{h} BGRA rows of stride {stride}", data.len()),
                        ));
                    }
                    let buffer = new_bgra_buffer(session, w, h)?;
                    copy_into(&buffer, data, stride, row, h)?;
                    Ok(buffer)
                }
            }
        }
    }

    impl VideoEncoder for VtH264Encoder {
        fn configure(&mut self, config: &EncoderConfig) -> Result<EncoderInfo> {
            self.build(config)
        }

        fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
            let cfg = self.cfg.ok_or_else(|| err(ErrorKind::Closed, "encode", "not configured"))?;
            if frame.width == 0 || frame.height == 0 {
                return Err(err(ErrorKind::InvalidInput, "encode", "empty frame"));
            }
            let mut out = Vec::new();
            let mut force = force_keyframe;
            if (frame.width, frame.height) != (cfg.width, cfg.height) {
                // A display size change: a new session, new SPS/PPS, the first frame a keyframe with `config_change`.
                out = self.flush()?;
                self.build(&EncoderConfig { width: frame.width, height: frame.height, ..cfg })?;
                force = true;
            }
            if skip_frame(self.skip_interval, self.last_encoded_at, frame.captured_at, force) {
                return Ok(out);
            }
            let started = self.clock.now().0;
            let buffer = self.source_buffer(frame)?;
            let pts = next_pts(self.last_pts, frame.captured_at);
            let props = force.then(force_keyframe_properties);
            let session = self.session.as_ref().ok_or_else(|| err(ErrorKind::Closed, "encode", "not configured"))?;
            let duration = time(1_000_000 / i64::from(self.eff.output_fps.max(1)));
            let mut info = VTEncodeInfoFlags::empty();
            // SAFETY: a live session and pixel buffer (the session retains it as long as it needs); `props` is null or a
            // dictionary of the right types; the frame refcon carries the timestamp (no pointer, never dereferenced);
            // `info` is a valid out pointer.
            let status = unsafe {
                session.session.encode_frame(
                    &buffer,
                    time(pts),
                    duration,
                    props.as_deref(),
                    pts as usize as *mut c_void,
                    &raw mut info,
                )
            };
            if status != 0 {
                return Err(status_err("VTCompressionSessionEncodeFrame", status));
            }
            self.last_pts = Some(pts);
            self.last_encoded_at = Some(frame.captured_at.0);
            self.pending.push_back(Pending { pts, captured_at: frame.captured_at, started });
            out.extend(self.collect(pts)?);
            Ok(out)
        }

        fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()> {
            let cfg = self.cfg.ok_or_else(|| err(ErrorKind::Closed, "set_rate", "not configured"))?;
            let eff = effective(cfg.preset, kbps, fps);
            let session = self.session.as_ref().ok_or_else(|| err(ErrorKind::Closed, "set_rate", "not configured"))?;
            apply_rate(&session.session, &eff, true)?;
            self.cfg = Some(EncoderConfig { kbps, fps, ..cfg });
            self.eff = eff;
            self.skip_interval = skip_interval_us(&eff);
            Ok(())
        }

        fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
            if let Some(session) = self.session.as_ref() {
                // SAFETY: a live session; an indefinite time completes every pending frame.
                let status = unsafe { session.session.complete_frames(kCMTimeIndefinite) };
                if status != 0 {
                    return Err(status_err("VTCompressionSessionCompleteFrames", status));
                }
            }
            self.drain()
        }
    }

    fn set_prop(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
        let target: &CFType = session.as_ref();
        // SAFETY: a live session, a framework key and a CF value of the type that key documents.
        unsafe { VTSessionSetProperty(target, key, Some(value)) }
    }

    fn set_required(session: &VTCompressionSession, op: &'static str, key: &CFString, value: &CFType) -> Result<()> {
        match set_prop(session, key, value) {
            0 => Ok(()),
            status => Err(status_err(op, status)),
        }
    }

    /// Bitrate, data rate limit and frame rate. `AverageBitRate` and `ExpectedFrameRate` must be accepted; the rate
    /// limit is a hint that some rate controllers (low latency) do not take, so its refusal is not an error.
    fn apply_rate(session: &VTCompressionSession, eff: &Effective, live: bool) -> Result<()> {
        let bps = i32::try_from(u64::from(eff.kbps) * 1000).unwrap_or(i32::MAX);
        // SAFETY (the statics): the framework's own, valid for the process lifetime.
        let avg = unsafe { kVTCompressionPropertyKey_AverageBitRate };
        set_required(session, "VTSessionSetProperty(AverageBitRate)", avg, CFNumber::new_i32(bps).as_ref())?;
        let fps = unsafe { kVTCompressionPropertyKey_ExpectedFrameRate };
        set_required(
            session,
            "VTSessionSetProperty(ExpectedFrameRate)",
            fps,
            CFNumber::new_f64(f64::from(eff.output_fps)).as_ref(),
        )?;
        let limits_key = unsafe { kVTCompressionPropertyKey_DataRateLimits };
        let bytes = CFNumber::new_i64(data_rate_limit_bytes(eff.kbps));
        let seconds = CFNumber::new_f64(1.0);
        let limits = objc2_core_foundation::CFArray::<CFNumber>::from_objects(&[&*bytes, &*seconds]);
        let status = set_prop(session, limits_key, limits.as_ref());
        let _ = (status, live);
        Ok(())
    }

    fn force_keyframe_properties() -> CFRetained<CFDictionary> {
        // SAFETY: the framework's own static.
        let key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let dict = CFDictionary::<CFString, CFType>::from_slices(&[key], &[CFBoolean::new(true).as_ref()]);
        // SAFETY: a CFDictionary whatever its key and value types say.
        unsafe { CFRetained::cast_unchecked::<CFDictionary>(dict) }
    }

    fn encoder_specification(require_hardware: bool, hardware: bool, low_latency: bool) -> CFRetained<CFDictionary> {
        let mut keys: Vec<&CFString> = Vec::new();
        let yes = CFBoolean::new(true);
        // SAFETY: the framework's own statics.
        unsafe {
            if require_hardware {
                keys.push(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder);
            } else if hardware {
                keys.push(kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder);
            }
            if low_latency {
                keys.push(kVTVideoEncoderSpecification_EnableLowLatencyRateControl);
            }
        }
        let values: Vec<&CFType> = keys.iter().map(|_| yes.as_ref()).collect();
        let dict = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        // SAFETY: a CFDictionary whatever its key and value types say.
        unsafe { CFRetained::cast_unchecked::<CFDictionary>(dict) }
    }

    /// Source pixel buffer attributes: BGRA at the session size, IOSurface-backed, so the session's pool hands out
    /// buffers the CPU copy can fill and VideoToolbox converts to what the encoder takes.
    fn source_attributes(width: u32, height: u32) -> CFRetained<CFDictionary> {
        let format = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
        let (w, h) = (CFNumber::new_i32(width as i32), CFNumber::new_i32(height as i32));
        let surface = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: the framework's own statics.
        let (fk, wk, hk, sk) = unsafe {
            (
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            )
        };
        let dict = CFDictionary::<CFString, CFType>::from_slices(
            &[fk, wk, hk, sk],
            &[format.as_ref(), w.as_ref(), h.as_ref(), surface.as_ref()],
        );
        // SAFETY: a CFDictionary whatever its key and value types say.
        unsafe { CFRetained::cast_unchecked::<CFDictionary>(dict) }
    }

    fn make_session(cfg: &EncoderConfig, eff: &Effective, clock: Arc<dyn Clock>) -> Result<Session> {
        let attrs = source_attributes(cfg.width, cfg.height);
        // (require hardware, enable hardware, low latency): the strictest first.
        let attempts = [(true, true, true), (false, true, true), (false, true, false), (false, false, false)];
        let mut last = err(ErrorKind::Unavailable, "VTCompressionSessionCreate", "no attempt ran");
        for (require, hardware, low_latency) in attempts {
            let spec = encoder_specification(require, hardware, low_latency);
            let shared =
                Box::new(Shared { clock: clock.clone(), outs: Mutex::new(Vec::new()), signal: Condvar::new() });
            let mut out: *mut VTCompressionSession = ptr::null_mut();
            // SAFETY: the dictionaries are live; the refcon points into the box that the returned `Session` keeps until
            // after `invalidate`; `out` is a valid out pointer and receives a +1 session.
            let status = unsafe {
                VTCompressionSession::create(
                    None,
                    cfg.width as i32,
                    cfg.height as i32,
                    kCMVideoCodecType_H264,
                    Some(&spec),
                    Some(&attrs),
                    None,
                    Some(on_output),
                    ptr::from_ref::<Shared>(&shared).cast_mut().cast::<c_void>(),
                    NonNull::from(&mut out),
                )
            };
            if status != 0 {
                last = status_err("VTCompressionSessionCreate", status);
                continue;
            }
            let Some(raw) = NonNull::new(out) else {
                last = err(ErrorKind::Os, "VTCompressionSessionCreate", "no session");
                continue;
            };
            // SAFETY: Create rule.
            let session = unsafe { CFRetained::from_raw(raw) };
            let hardware_used = if require { true } else { using_hardware(&session).unwrap_or(false) };
            let mut built = Session {
                session,
                shared,
                width: cfg.width,
                height: cfg.height,
                hardware: hardware_used,
                low_latency,
                profile: H264Profile::High,
            };
            match configure_properties(&built.session, cfg, eff) {
                Ok(profile) => {
                    built.profile = profile;
                    return Ok(built);
                }
                // A property this configuration refuses (low latency rate control is picky): try the next one.
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Reads `kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder`.
    fn using_hardware(session: &VTCompressionSession) -> Option<bool> {
        let target: &CFType = session.as_ref();
        let mut out: *const CFBoolean = ptr::null();
        // SAFETY: a live session and the framework key; `out` is where a retained CFBoolean is stored on success.
        let status = unsafe {
            VTSessionCopyProperty(
                target,
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                None,
                (&raw mut out).cast::<c_void>(),
            )
        };
        if status != 0 {
            return None;
        }
        // SAFETY: Copy rule: the call stored a +1 object.
        NonNull::new(out.cast_mut()).map(|p| unsafe { CFRetained::from_raw(p) }.value())
    }

    /// Session properties; returns the profile the encoder accepted.
    fn configure_properties(
        session: &VTCompressionSession,
        cfg: &EncoderConfig,
        eff: &Effective,
    ) -> Result<H264Profile> {
        // SAFETY (the statics): the framework's own, valid for the process lifetime.
        let realtime = unsafe { kVTCompressionPropertyKey_RealTime };
        set_required(session, "VTSessionSetProperty(RealTime)", realtime, CFBoolean::new(true).as_ref())?;
        let reorder = unsafe { kVTCompressionPropertyKey_AllowFrameReordering };
        set_required(session, "VTSessionSetProperty(AllowFrameReordering)", reorder, CFBoolean::new(false).as_ref())?;
        let level_key = unsafe { kVTCompressionPropertyKey_ProfileLevel };
        let profile = if set_prop(session, level_key, unsafe { kVTProfileLevel_H264_High_AutoLevel }.as_ref()) == 0 {
            H264Profile::High
        } else {
            set_required(
                session,
                "VTSessionSetProperty(ProfileLevel)",
                level_key,
                unsafe { kVTProfileLevel_H264_Main_AutoLevel }.as_ref(),
            )?;
            H264Profile::Main
        };
        // Key frames on demand only (as Windows: GOP of `u32::MAX`).
        let interval = unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval };
        set_required(
            session,
            "VTSessionSetProperty(MaxKeyFrameInterval)",
            interval,
            CFNumber::new_i32(i32::MAX).as_ref(),
        )?;
        // Speed over quality only for Movimento; Auto and Texto keep the encoder's default. Optional.
        let speed = unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality };
        let _ = set_prop(session, speed, CFBoolean::new(cfg.preset == swapdesk_proto::Preset::Motion).as_ref());
        apply_rate(session, eff, false)?;
        // SAFETY: a live session.
        let status = unsafe { session.prepare_to_encode_frames() };
        if status != 0 {
            return Err(status_err("VTCompressionSessionPrepareToEncodeFrames", status));
        }
        Ok(profile)
    }

    /// A BGRA buffer from the session's pool (the pool exists once the session does), else a plain one.
    fn new_bgra_buffer(session: &Session, w: usize, h: usize) -> Result<CFRetained<CVPixelBuffer>> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: a live session.
        if let Some(pool) = unsafe { session.session.pixel_buffer_pool() } {
            // SAFETY: a live pool; `out` is a valid out pointer and receives a +1 buffer.
            let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut out)) };
            if status == kCVReturnSuccess
                && let Some(p) = NonNull::new(out)
            {
                // SAFETY: Create rule.
                return Ok(unsafe { CFRetained::from_raw(p) });
            }
            out = ptr::null_mut();
        }
        let attrs = source_attributes(w as u32, h as u32);
        // SAFETY: valid attributes and out pointer; the result carries one reference on success.
        let status = unsafe {
            CVPixelBufferCreate(None, w, h, kCVPixelFormatType_32BGRA, Some(&attrs), NonNull::from(&mut out))
        };
        if status != kCVReturnSuccess {
            return Err(status_err("CVPixelBufferCreate", status));
        }
        let p = NonNull::new(out).ok_or_else(|| err(ErrorKind::Os, "CVPixelBufferCreate", "no pixel buffer"))?;
        // SAFETY: Create rule.
        Ok(unsafe { CFRetained::from_raw(p) })
    }

    /// Copies `h` rows of `row` bytes (source stride `stride`) into a locked BGRA buffer.
    fn copy_into(buffer: &CVPixelBuffer, data: &[u8], stride: usize, row: usize, h: usize) -> Result<()> {
        // SAFETY: the base address is only touched between a successful lock and the unlock below.
        let locked = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags(0)) };
        if locked != kCVReturnSuccess {
            return Err(status_err("CVPixelBufferLockBaseAddress", locked));
        }
        let dst_stride = CVPixelBufferGetBytesPerRow(buffer);
        let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
        let result = if base.is_null() || dst_stride < row || CVPixelBufferGetHeight(buffer) < h {
            Err(err(ErrorKind::Os, "encode", "pixel buffer has no writable BGRA rows"))
        } else {
            for y in 0..h {
                // SAFETY: the locked buffer holds at least `h` rows of `dst_stride >= row` bytes from `base`; the source
                // row `y` lies inside `data` (length checked by the caller: `stride * (h - 1) + row`).
                unsafe {
                    ptr::copy_nonoverlapping(data.as_ptr().add(y * stride), base.add(y * dst_stride), row);
                }
            }
            Ok(())
        };
        // SAFETY: balances the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags(0)) };
        result
    }
}
