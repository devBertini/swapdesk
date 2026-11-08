//! `VideoDecoder` over a VideoToolbox `VTDecompressionSession`.
//!
//! Low-latency mode, no reordering: one Annex B access unit in, at most one picture out. The session decodes
//! synchronously (no decode flags, so the output callback has run when `VTDecompressionSessionDecodeFrame` returns) and
//! in display order of arrival (no temporal processing). The picture is a `CVPixelBuffer` kept alive by a `CFRetained`
//! inside the `GpuSurface` (NV12, IOSurface-backed: [`DecoderOutput::Gpu`], for the renderer) or a BGRA copy in system
//! memory ([`DecoderOutput::CpuBgra`], for tests).
//!
//! Sources read on 2026-11-08 (Apple documentation and the `objc2-*` 0.3.2 docs, docs.rs):
//! - VTDecompressionSessionCreate, DecodeFrame, Invalidate and the output callback:
//!   <https://developer.apple.com/documentation/videotoolbox/vtdecompressionsession> and
//!   <https://docs.rs/objc2-video-toolbox/0.3.2/objc2_video_toolbox/struct.VTDecompressionSession.html>.
//! - Format description from H.264 parameter sets (`CMVideoFormatDescriptionCreateFromH264ParameterSets`: raw NAL
//!   units, `nal_unit_header_length` 4):
//!   <https://developer.apple.com/documentation/coremedia/cmvideoformatdescriptioncreatefromh264parametersets(allocator:parametersetcount:parametersetpointers:parametersetsizes:nalunitheaderlength:formatdescriptionout:)>
//!   and <https://docs.rs/objc2-core-media/0.3.2/objc2_core_media/fn.CMVideoFormatDescriptionCreateFromH264ParameterSets.html>.
//! - Samples: `CMBlockBuffer::create_with_memory_block` (with `kCMBlockBufferAssureMemoryNowFlag`) and
//!   `replace_data_bytes`, `CMSampleBuffer::create_ready`:
//!   <https://docs.rs/objc2-core-media/0.3.2/objc2_core_media/struct.CMBlockBuffer.html> and
//!   <https://docs.rs/objc2-core-media/0.3.2/objc2_core_media/struct.CMSampleBuffer.html>.
//! - Pixel buffers: `CVPixelBufferGetWidth` and friends, `kCVPixelBufferPixelFormatTypeKey`,
//!   `kCVPixelBufferIOSurfacePropertiesKey`: <https://docs.rs/objc2-core-video/0.3.2/objc2_core_video/>.
//! - CoreFoundation ownership (`CFRetained`, the Create rule):
//!   <https://docs.rs/objc2-core-foundation/0.3.2/objc2_core_foundation/struct.CFRetained.html>.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::time::Instant;

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescription, CMVideoFormatDescriptionCreateFromH264ParameterSets, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
};
use swapdesk_platform::{DecodedFrame, DisplayId, ErrorKind, GpuSurface, MonoMicros, Pixels, Result, VideoDecoder};
use swapdesk_proto::VideoHeader;

use crate::annexb::{self, AVCC_LENGTH_SIZE, ParamSets};
use crate::sys::{err, status_err};

/// What the decoder hands back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecoderOutput {
    /// `Pixels::Gpu` holding a `CFRetained<CVPixelBuffer>` (`objc2_core_foundation::CFRetained`, the type
    /// `objc2_core_video::CVPixelBuffer`): NV12 video range, IOSurface-backed, exactly the picture size. The renderer
    /// of this workspace (`swapdesk-render-mac`) downcasts it.
    #[default]
    Gpu,
    /// `Pixels::Cpu`: BGRA rows, `stride = width * 4` (VideoToolbox converts).
    CpuBgra,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DecoderOptions {
    pub output: DecoderOutput,
}

/// What the output callback left for `decode`.
struct Landed {
    status: i32,
    flags: u32,
    /// A `CVImageBuffer` the callback retained (`CFRetained::into_raw`), 0 when there was none.
    image: usize,
}

/// The cell the output callback writes into. `Landed` holds only integers, so the `Mutex` is `Sync` and the callback may
/// run on any thread VideoToolbox picks.
struct Slot(Mutex<Option<Landed>>);

impl Slot {
    fn take(&self) -> Option<Landed> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

/// `VTDecompressionOutputCallback`: stores status, flags and a retained image in the slot `refcon` points to.
///
/// # Safety
/// `refcon` is the address of the `Slot` of the session that owns this callback; the session is invalidated (no more
/// callbacks) before the slot is freed. `image` is null or a live image buffer for the duration of the call.
unsafe extern "C-unwind" fn on_frame(
    refcon: *mut c_void,
    _source_frame: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: see the function's contract.
    let Some(slot) = (unsafe { refcon.cast::<Slot>().as_ref() }) else { return };
    let raw = match NonNull::new(image) {
        // SAFETY: `image` is live during the callback; `retain` adds the reference the slot keeps.
        Some(p) => CFRetained::into_raw(unsafe { CFRetained::retain(p) }).as_ptr() as usize,
        None => 0,
    };
    if let Ok(mut guard) = slot.0.lock() {
        if let Some(old) = guard.replace(Landed { status, flags: flags.0, image: raw }) {
            release(old.image);
        }
    } else {
        release(raw);
    }
}

/// Gives back the reference of an image the callback retained.
fn release(raw: usize) {
    if let Some(p) = NonNull::new(raw as *mut CVImageBuffer) {
        // SAFETY: `raw` came from `CFRetained::into_raw`, so it carries one reference that this drop releases.
        drop(unsafe { CFRetained::from_raw(p) });
    }
}

/// A running session with the format it was made for.
struct Session {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMVideoFormatDescription>,
    slot: Box<Slot>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: tears the session down before its callback cell (`slot`, dropped after this) goes away; no callback
        // runs after `invalidate` returns.
        unsafe { self.session.invalidate() };
        if let Some(l) = self.slot.take() {
            release(l.image);
        }
    }
}

/// The pixel buffer attributes the session is asked to output: the format, IOSurface-backed.
fn destination_attributes(output: DecoderOutput) -> CFRetained<CFDictionary> {
    let format = match output {
        DecoderOutput::Gpu => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        DecoderOutput::CpuBgra => kCVPixelFormatType_32BGRA,
    };
    let number = CFNumber::new_i32(format as i32);
    let surface_props = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: the statics are the framework's own, valid for the process lifetime.
    let (format_key, surface_key) = unsafe { (kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey) };
    let dict = CFDictionary::<CFString, CFType>::from_slices(
        &[format_key, surface_key],
        &[number.as_ref(), surface_props.as_ref()],
    );
    // SAFETY: the dictionary is a CFDictionary whatever its key and value types say.
    unsafe { CFRetained::cast_unchecked::<CFDictionary>(dict) }
}

fn make_format(params: &ParamSets) -> Result<CFRetained<CMVideoFormatDescription>> {
    let sets: Vec<&[u8]> = params.all().collect();
    let mut pointers: Vec<NonNull<u8>> = sets.iter().filter_map(|s| NonNull::new(s.as_ptr().cast_mut())).collect();
    let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
    if pointers.len() != sets.len() || sets.is_empty() {
        return Err(err(
            ErrorKind::InvalidInput,
            "CMVideoFormatDescriptionCreateFromH264ParameterSets",
            "empty parameter set",
        ));
    }
    let mut out: *const CMFormatDescription = ptr::null();
    // SAFETY: `pointers` and `sizes` hold `sets.len()` valid entries that outlive the call; the framework copies the
    // bytes. `out` is a valid pointer. On success the result carries one reference (the Create rule).
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            sets.len(),
            NonNull::new_unchecked(pointers.as_mut_ptr()),
            NonNull::new_unchecked(sizes.as_ptr().cast_mut()),
            AVCC_LENGTH_SIZE as i32,
            NonNull::from(&mut out),
        )
    };
    if status != 0 {
        return Err(status_err("CMVideoFormatDescriptionCreateFromH264ParameterSets", status));
    }
    let ptr = NonNull::new(out.cast_mut()).ok_or_else(|| {
        err(ErrorKind::Os, "CMVideoFormatDescriptionCreateFromH264ParameterSets", "no format description")
    })?;
    // SAFETY: Create rule, see above.
    Ok(unsafe { CFRetained::from_raw(ptr) })
}

fn make_session(params: &ParamSets, output: DecoderOutput) -> Result<Session> {
    let format = make_format(params)?;
    let slot = Box::new(Slot(Mutex::new(None)));
    let record = VTDecompressionOutputCallbackRecord {
        decompressionOutputCallback: Some(on_frame),
        decompressionOutputRefCon: ptr::from_ref::<Slot>(&slot).cast_mut().cast::<c_void>(),
    };
    let attrs = destination_attributes(output);
    let mut out: *mut VTDecompressionSession = ptr::null_mut();
    // SAFETY: `format` and `attrs` are live; `record` is copied by the call; the refcon points into the box that the
    // returned `Session` keeps until after `invalidate`; `out` is a valid pointer and receives a +1 session.
    let status =
        unsafe { VTDecompressionSession::create(None, &format, None, Some(&attrs), &record, NonNull::from(&mut out)) };
    if status != 0 {
        return Err(status_err("VTDecompressionSessionCreate", status));
    }
    let ptr = NonNull::new(out).ok_or_else(|| err(ErrorKind::Os, "VTDecompressionSessionCreate", "no session"))?;
    // SAFETY: Create rule, see above.
    let session = unsafe { CFRetained::from_raw(ptr) };
    Ok(Session { session, format, slot })
}

fn time(value: i64, timescale: i32) -> CMTime {
    CMTime { value, timescale, flags: CMTimeFlags::Valid, epoch: 0 }
}

/// One AVCC access unit as a ready `CMSampleBuffer` with one sample.
fn make_sample(avcc: &[u8], format: &CMVideoFormatDescription, pts: i64) -> Result<CFRetained<CMSampleBuffer>> {
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: a null memory block with `AssureMemoryNow` makes CoreMedia allocate `avcc.len()` bytes now; `block` is a
    // valid out pointer and receives a +1 buffer.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            avcc.len(),
            None,
            ptr::null(),
            0,
            avcc.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    if status != 0 {
        return Err(status_err("CMBlockBufferCreateWithMemoryBlock", status));
    }
    let block = NonNull::new(block)
        .ok_or_else(|| err(ErrorKind::Os, "CMBlockBufferCreateWithMemoryBlock", "no block buffer"))?;
    // SAFETY: Create rule.
    let block = unsafe { CFRetained::from_raw(block) };
    let source = NonNull::new(avcc.as_ptr().cast_mut().cast::<c_void>())
        .ok_or_else(|| err(ErrorKind::InvalidInput, "decode", "empty access unit"))?;
    // SAFETY: copies `avcc.len()` bytes from `source` into the buffer allocated above, which holds exactly that many.
    let status = unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, avcc.len()) };
    if status != 0 {
        return Err(status_err("CMBlockBufferReplaceDataBytes", status));
    }
    let timing = CMSampleTimingInfo {
        duration: time(1, 60),
        presentationTimeStamp: time(pts, 60),
        decodeTimeStamp: time(pts, 60),
    };
    let size = avcc.len();
    let mut sample: *mut CMSampleBuffer = ptr::null_mut();
    // SAFETY: one sample, one timing entry, one size entry, all pointing at locals that outlive the call; the block
    // buffer and the format description are retained by the sample buffer.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &raw const timing,
            1,
            &raw const size,
            NonNull::from(&mut sample),
        )
    };
    if status != 0 {
        return Err(status_err("CMSampleBufferCreateReady", status));
    }
    let sample =
        NonNull::new(sample).ok_or_else(|| err(ErrorKind::Os, "CMSampleBufferCreateReady", "no sample buffer"))?;
    // SAFETY: Create rule.
    Ok(unsafe { CFRetained::from_raw(sample) })
}

/// Reads a BGRA pixel buffer into tightly packed rows.
fn read_bgra(image: &CVImageBuffer, w: u32, h: u32) -> Result<Vec<u8>> {
    // SAFETY: the base address is only touched between a successful lock and the unlock below.
    let locked = unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    if locked != kCVReturnSuccess {
        return Err(status_err("CVPixelBufferLockBaseAddress", locked));
    }
    let stride = CVPixelBufferGetBytesPerRow(image);
    let base = CVPixelBufferGetBaseAddress(image).cast::<u8>().cast_const();
    let row = w as usize * 4;
    let rows = h as usize;
    let result = if base.is_null() || stride < row {
        Err(err(ErrorKind::Os, "decode", "pixel buffer has no readable BGRA rows"))
    } else {
        let mut data = Vec::with_capacity(row * rows);
        for y in 0..rows {
            // SAFETY: the locked buffer holds `rows` rows of `stride` bytes starting at `base`, and `row <= stride`.
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(base.add(y * stride), row) });
        }
        Ok(data)
    };
    // SAFETY: balances the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    result
}

/// Decoder over VideoToolbox. See the module documentation. Objects are created and used on one thread (C3 thread
/// rules): nothing here is `Send`.
pub struct VtH264Decoder {
    options: DecoderOptions,
    /// The last SPS/PPS seen; a keyframe without parameter sets decodes with these.
    params: Option<ParamSets>,
    session: Option<Session>,
    waiting_keyframe: bool,
    last_id: Option<u32>,
    seq: i64,
    decode_us: u32,
    sessions: u32,
    started: Instant,
}

impl Default for VtH264Decoder {
    fn default() -> Self {
        Self::new(DecoderOptions::default())
    }
}

impl VtH264Decoder {
    pub fn new(options: DecoderOptions) -> Self {
        Self {
            options,
            params: None,
            session: None,
            waiting_keyframe: true,
            last_id: None,
            seq: 0,
            decode_us: 0,
            sessions: 0,
            started: Instant::now(),
        }
    }

    /// Microseconds the last `decode` call took (input to picture).
    pub fn last_decode_us(&self) -> u32 {
        self.decode_us
    }

    /// True after `reset` or a detected gap, until a keyframe arrives.
    pub fn waiting_for_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    /// Decompression sessions made so far (one per new SPS/PPS and one after an invalidated session).
    pub fn sessions_created(&self) -> u32 {
        self.sessions
    }

    /// Time since this decoder was made, for measurements.
    pub fn uptime(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    fn ensure_session(&mut self) -> Result<()> {
        if self.session.is_none() {
            let params = self.params.as_ref().ok_or_else(|| {
                err(ErrorKind::InvalidInput, "decode", "no SPS/PPS seen yet: the first keyframe must carry them")
            })?;
            self.session = Some(make_session(params, self.options.output)?);
            self.sessions += 1;
        }
        Ok(())
    }

    fn decode_unit(&mut self, header: &VideoHeader, annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
        let avcc = annexb::to_avcc(annex_b);
        if avcc.is_empty() {
            return Ok(None);
        }
        let output = self.options.output;
        let pts = self.seq;
        self.seq += 1;
        let session = self.session.as_mut().expect("ensure_session ran");
        let sample = make_sample(&avcc, &session.format, pts)?;
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: a valid session and sample; no flags, so the call returns after the callback ran; `info` is a valid
        // out pointer; no per-frame refcon.
        let status = unsafe {
            session.session.decode_frame(&sample, VTDecodeFrameFlags::empty(), ptr::null_mut(), &raw mut info)
        };
        if status != 0 {
            return Err(status_err("VTDecompressionSessionDecodeFrame", status));
        }
        let Some(landed) = session.slot.take() else {
            return Ok(None);
        };
        // Takes ownership of the image reference the callback retained.
        let image = NonNull::new(landed.image as *mut CVImageBuffer)
            // SAFETY: `landed.image` came from `CFRetained::into_raw` in the callback.
            .map(|p| unsafe { CFRetained::from_raw(p) });
        if landed.status != 0 {
            return Err(status_err("VTDecompressionOutputCallback", landed.status));
        }
        if landed.flags & VTDecodeInfoFlags::FrameDropped.0 != 0 {
            return Ok(None);
        }
        let Some(image) = image else { return Ok(None) };
        let (w, h) = (CVPixelBufferGetWidth(&image) as u32, CVPixelBufferGetHeight(&image) as u32);
        let pixels = match output {
            DecoderOutput::Gpu => Pixels::Gpu(GpuSurface(Box::new(image))),
            DecoderOutput::CpuBgra => Pixels::Cpu { stride: w * 4, data: read_bgra(&image, w, h)? },
        };
        Ok(Some(DecodedFrame {
            display: DisplayId(header.display_id),
            frame_id: header.frame_id,
            captured_at: MonoMicros(header.capture_us),
            width: w,
            height: h,
            pixels,
        }))
    }
}

impl VideoDecoder for VtH264Decoder {
    fn decode(&mut self, header: &VideoHeader, annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
        if !header.keyframe {
            // A hole in the frame ids: the picture would be wrong from here to the next keyframe.
            if let Some(last) = self.last_id
                && header.frame_id != last.wrapping_add(1)
            {
                self.reset();
            }
            if self.waiting_keyframe {
                return Ok(None);
            }
            if header.discontinuity {
                self.reset();
                return Ok(None);
            }
        }
        // New parameter sets (first stream, size or configuration change) need a new session; the old one goes first.
        if let Some(sets) = annexb::parameter_sets(annex_b)
            && self.params.as_ref() != Some(&sets)
        {
            self.params = Some(sets);
            self.session = None;
        }
        // Parameter sets, SEI and delimiters alone are no picture: remembered above, nothing to decode, and the stream
        // is no closer to a keyframe.
        if !annexb::has_picture(annex_b) {
            return Ok(None);
        }
        self.ensure_session()?;
        self.waiting_keyframe = false;
        self.last_id = Some(header.frame_id);
        let t0 = Instant::now();
        let outcome = self.decode_unit(header, annex_b);
        match outcome {
            Ok(frame) => {
                self.decode_us = t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
                Ok(frame)
            }
            Err(e) => {
                self.waiting_keyframe = true;
                // A session the system invalidated (sleep, decoder reclaimed) is made again on the next keyframe.
                if e.os_code == Some(i64::from(crate::sys::VT_INVALID_SESSION)) {
                    self.session = None;
                }
                Err(e)
            }
        }
    }

    fn reset(&mut self) {
        self.waiting_keyframe = true;
        self.last_id = None;
        // Synchronous decoding leaves nothing queued in the session, and an IDR picture decodes without any state of
        // the earlier pictures, so the session stays; a late callback result, if any, is dropped.
        if let Some(session) = self.session.as_ref()
            && let Some(l) = session.slot.take()
        {
            release(l.image);
        }
    }
}
