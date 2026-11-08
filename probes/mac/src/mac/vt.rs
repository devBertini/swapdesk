//! `vt` (P12, optional): VideoToolbox H.264 low-latency encode of synthetic frames, forced keyframes, encode times.
//! Keyframes are detected from the NAL unit type of the first AVCC NAL (5 = IDR) instead of the
//! kCMSampleAttachmentKey_NotSync attachment, which keeps the code small; the two are equivalent for H.264.

use crate::d;
use crate::log::emit;
use crate::Args;
use objc2_core_foundation::{kCFBooleanTrue, CFBoolean, CFDictionary, CFString};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_High_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `kCMVideoCodecType_H264` ('avc1').
const CODEC_H264: u32 = 0x6176_6331;
/// `kCVPixelFormatType_32BGRA`.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

#[derive(Default)]
struct Shared {
    submit: Mutex<Vec<Option<Instant>>>,
    /// (frame index, encode ms, is keyframe, bytes)
    done: Mutex<Vec<(usize, f64, bool, usize)>>,
    errors: Mutex<Vec<i32>>,
}

unsafe extern "C-unwind" fn output(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: refcon is the Arc pointer given at session creation and outlives the session.
    let shared = unsafe { &*(refcon as *const Shared) };
    if status != 0 || sample.is_null() {
        if let Ok(mut e) = shared.errors.lock() {
            e.push(status);
        }
        return;
    }
    let index = (frame_refcon as usize).saturating_sub(1);
    let now = Instant::now();
    let started = shared.submit.lock().ok().and_then(|s| s.get(index).copied().flatten());
    let (mut key, mut bytes) = (false, 0usize);
    // SAFETY: the sample buffer is valid during the callback.
    unsafe {
        if let Some(block) = (*sample).data_buffer() {
            let mut len = 0usize;
            let mut ptr: *mut std::ffi::c_char = std::ptr::null_mut();
            if block.data_pointer(0, std::ptr::null_mut(), &mut len, &mut ptr) == 0 && !ptr.is_null() && len > 4 {
                bytes = len;
                key = (*(ptr as *const u8).add(4) & 0x1F) == 5;
            }
        }
    }
    if let (Some(t0), Ok(mut done)) = (started, shared.done.lock()) {
        done.push((index, now.duration_since(t0).as_secs_f64() * 1000.0, key, bytes));
    }
}

pub fn run(args: &Args) {
    let frames = args.num("frames", 600) as usize;
    let size = args.get("size").unwrap_or("1920x1080");
    let (w, h) = size
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .unwrap_or((1920, 1080));
    let force: Vec<usize> = args.get("force-key").unwrap_or("").split(',').filter_map(|s| s.parse().ok()).collect();

    let shared = Arc::new(Shared::default());
    if let Ok(mut s) = shared.submit.lock() {
        s.resize(frames, None);
    }
    let refcon = Arc::as_ptr(&shared) as *mut c_void;

    let (Some(true_value), key_spec) = (unsafe { kCFBooleanTrue }, unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl }) else {
        emit("vt.session", false, &d!("error" => "kCFBooleanTrue missing"));
        return;
    };
    let spec = CFDictionary::<CFString, CFBoolean>::from_slices(&[key_spec], &[true_value]);
    let mut session_ptr: *mut VTCompressionSession = std::ptr::null_mut();
    // SAFETY: all pointers are valid; `shared` outlives the session (invalidated below).
    let status = unsafe {
        VTCompressionSession::create(
            None,
            w as i32,
            h as i32,
            CODEC_H264,
            Some(spec.as_opaque()),
            None,
            None,
            Some(output),
            refcon,
            NonNull::from(&mut session_ptr),
        )
    };
    if status != 0 || session_ptr.is_null() {
        emit("vt.session", false, &d!("os_status" => i64::from(status), "low_latency_requested" => true));
        return;
    }
    // SAFETY: create returned a +1 retained session pointer.
    let session = unsafe { &*session_ptr };
    // GUESS: VTCompressionSession derefs/converts to the generic CFType that VTSessionSetProperty takes.
    // SAFETY: statics are valid CFStrings; values are CFBoolean.
    let set_status = unsafe {
        let s: &objc2_core_foundation::CFType = session.as_ref();
        [
            VTSessionSetProperty(s, kVTCompressionPropertyKey_RealTime, Some(true_value.as_ref())),
            VTSessionSetProperty(s, kVTCompressionPropertyKey_ProfileLevel, Some(kVTProfileLevel_H264_High_AutoLevel.as_ref())),
            VTSessionSetProperty(
                s,
                kVTCompressionPropertyKey_AllowFrameReordering,
                Some(objc2_core_foundation::kCFBooleanFalse.unwrap_or(true_value).as_ref()),
            ),
        ]
    };
    emit(
        "vt.session",
        status == 0,
        &d!(
            "low_latency_requested" => true,
            "properties_status" => format!("{set_status:?}"),
            "profile_requested" => "H264_High_AutoLevel",
            "size" => size
        ),
    );

    let mut pixel_buffer: *mut objc2_core_video::CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: valid out pointer.
    let created = unsafe { CVPixelBufferCreate(None, w, h, PIXEL_FORMAT_BGRA, None, NonNull::from(&mut pixel_buffer)) };
    // SAFETY: checked non-null below.
    let Some(buffer) = (if created == 0 { unsafe { pixel_buffer.as_ref() } } else { None }) else {
        emit("vt.frames", false, &d!("error" => "CVPixelBufferCreate failed", "cv_return" => i64::from(created)));
        return;
    };
    let force_dict = CFDictionary::<CFString, CFBoolean>::from_slices(&[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }], &[true_value]);
    let frame_duration = Duration::from_nanos(16_666_667);
    let start = Instant::now();
    for i in 0..frames {
        // Moving gradient so the encoder has real changes to code (one shared buffer: probe only).
        // SAFETY: the buffer is locked while written, then unlocked.
        unsafe {
            let flags = CVPixelBufferLockFlags(0);
            if CVPixelBufferLockBaseAddress(buffer, flags) == 0 {
                let base = CVPixelBufferGetBaseAddress(buffer) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRow(buffer);
                if !base.is_null() {
                    for y in 0..h {
                        let row = base.add(y * stride);
                        for x in (0..w).step_by(8) {
                            let p = row.add(x * 4);
                            *p = ((x + i * 7) & 0xFF) as u8;
                            *p.add(1) = ((y + i * 3) & 0xFF) as u8;
                            *p.add(2) = (i & 0xFF) as u8;
                        }
                    }
                }
                CVPixelBufferUnlockBaseAddress(buffer, flags);
            }
        }
        if let Ok(mut s) = shared.submit.lock() {
            s[i] = Some(Instant::now());
        }
        let pts = CMTime { value: i as i64, timescale: 60, flags: CMTimeFlags::Valid, epoch: 0 };
        let duration = CMTime { value: 1, timescale: 60, flags: CMTimeFlags::Valid, epoch: 0 };
        let props = if force.contains(&i) { Some(force_dict.as_opaque()) } else { None };
        let mut info = VTEncodeInfoFlags::empty();
        // SAFETY: valid session, buffer and out pointer; the refcon encodes the frame index + 1.
        let r = unsafe { session.encode_frame(buffer, pts, duration, props, (i + 1) as *mut c_void, &mut info) };
        if r != 0 {
            emit("vt.encode", false, &d!("frame" => i, "os_status" => i64::from(r)));
            break;
        }
        let due = start + frame_duration * (i as u32 + 1);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    // SAFETY: flush and release the session.
    unsafe {
        let _ = session.complete_frames(CMTime { value: 0, timescale: 0, flags: CMTimeFlags::empty(), epoch: 0 });
        session.invalidate();
    }

    let done = shared.done.lock().map(|d| d.clone()).unwrap_or_default();
    let mut ms: Vec<f64> = done.iter().map(|x| x.1).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = if ms.is_empty() { 0.0 } else { ms.iter().sum::<f64>() / ms.len() as f64 };
    let p95 = ms.get(((ms.len() as f64) * 0.95) as usize).or(ms.last()).copied().unwrap_or(0.0);
    let mut keys: Vec<usize> = done.iter().filter(|x| x.2).map(|x| x.0).collect();
    keys.sort_unstable();
    let honoured: Vec<bool> = force.iter().map(|f| keys.contains(f)).collect();
    emit(
        "vt.frames",
        !done.is_empty(),
        &d!(
            "encoded" => done.len(),
            "mean_ms" => mean,
            "p95_ms" => p95,
            "keyframes_at" => format!("{keys:?}"),
            "forced_at" => format!("{force:?}"),
            "forced_honoured" => format!("{honoured:?}"),
            "callback_errors" => format!("{:?}", shared.errors.lock().map(|e| e.clone()).unwrap_or_default())
        ),
    );
}
