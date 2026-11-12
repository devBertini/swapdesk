//! `ScreenSource` and the cursor over ScreenCaptureKit (stage 24, lane screen).
//!
//! `MacScreen` (macOS only) captures one display at a time with one `SCStream`. The stream delivers NV12 full range
//! (`'420f'`) `CVPixelBuffer`s on a serial dispatch queue; the output object keeps only the newest one (a mailbox, newest
//! wins, the dirty rects of a replaced frame are merged into the newer one) and `next_frame` hands it on.
//!
//! # Conventions other lanes rely on
//!
//! * **Display bounds** (`DisplayInfo.bounds`) are in *physical pixels of the virtual desktop*: the origin is the
//!   CoreGraphics point origin of the display (`CGDisplayBounds`, global points, y down, primary at 0,0) multiplied by
//!   THAT display's scale and rounded; the size is the point size times the same scale (the pixel size of the mode's
//!   backing store). With mixed scales the pixel origins of two displays are therefore not one contiguous pixel
//!   space; map a global point to a display with [`cursor_to_pixels`] using the display's own origin and scale.
//!   The input lane uses the same rule in reverse: global point = `bounds.x / scale + x_in_display / scale`.
//! * **`DisplayId(n)`** is a small index in the order displays were first seen (the main display is index 0 on the first
//!   enumeration); it keeps its `CGDirectDisplayID` for the life of the `MacScreen`.
//! * **`Frame.pixels`** is `Pixels::Gpu(GpuSurface(Box<CFRetained<CVPixelBuffer>>))`: the boxed concrete type is exactly
//!   `objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>` (the encoder lane downcasts it). The buffer is
//!   IOSurface backed, bi-planar 4:2:0 full range, `width x height` = the display's pixel size, colour matrix ITU-R 709.
//! * **Cursor** position is the hot spot in pixels inside the open display (`CGEventGetLocation` is the hot spot), shapes
//!   are straight (not premultiplied) RGBA, top-down, at most 256 x 256, hot spot in pixels.
//!
//! # Facts from primary pages (read 08/11/2026)
//!
//! * `SCStreamFrameInfoDirtyRects`: "an array of rectangles that represents a union of the rectangles redrawn and moved",
//!   `CGRect` in `NSValue`, in pixels (developer.apple.com/documentation/screencapturekit/scstreamframeinfo/dirtyrects;
//!   units from the SDK header comment shipped in objc2-screen-capture-kit 0.3.2).
//! * `SCFrameStatus`: Complete, Idle, Blank, Suspended, Started, Stopped; only Complete carries a new picture
//!   (developer.apple.com/documentation/screencapturekit/scframestatus).
//! * `SCStreamConfiguration.queueDepth`: default and minimum 3 frames
//!   (developer.apple.com/documentation/screencapturekit/scstreamconfiguration/queuedepth).
//! * Screen Recording grant: `CGPreflightScreenCaptureAccess` only reads the grant (never prompts); the request API is
//!   never called here. A user decline surfaces as `SCStreamErrorUserDeclined` (-3801).
//! * Cursor shape: ScreenCaptureKit does not deliver the pointer image when `showsCursor` is false, and no documented API
//!   returns another process's cursor image. `NSCursor.currentSystemCursor` is the only AppKit way; its header marks it
//!   deprecated ("will always be nil in a future version of macOS", and recommends `NSCursor.currentCursor`, which is the
//!   calling application's cursor only). It is not documented as main-thread-free; this module calls it from the
//!   capture thread, at most every 50 ms. When it returns nil (no window server session, or a future macOS) there is no
//!   shape and no shape change is reported: the process-wide `+[NSCursor arrowCursor]` is never touched here, because
//!   AppKit does not document it as usable off the main thread and it would hand out the shared image object.
//!
//! # Behaviour of the stream
//!
//! * Stream stops: `stream:didStopWithError:` keeps the `NSError` code and `next_frame` maps it with the table of
//!   `sck_error` (stopped on purpose is `Closed`, so the helper stops instead of re-acquiring; the system stopping the
//!   stream is `AccessLost`).
//! * Every AppKit and ScreenCaptureKit call on the caller's thread runs inside an autorelease pool, so a long-lived
//!   capture thread that has none of its own does not accumulate autoreleased objects.
//! * Waits on completion handlers are bounded (`REPLY_TIMEOUT` for the content query and the start, 2 s for a stop).
//!   A start that times out is stopped (bounded) before the stream is dropped. They are not longer on purpose: the
//!   helper's `stop_video` joins the capture thread, so a long wait in `open` would freeze the helper's message loop
//!   (the macOS "allow this month" prompt is what can hold the first reply; a timeout then is retried by the user).

use core::time::Duration;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use swapdesk_platform::{ErrorKind, MonoMicros, Rect};

use crate::sck_error;

/// Largest cursor bitmap edge sent to the viewer.
pub const MAX_CURSOR_EDGE: u32 = 256;
/// Refresh rate reported when the mode says 0 (built-in panels): 60 Hz in millihertz.
pub const DEFAULT_REFRESH_MHZ: u32 = 60_000;
/// `SCStreamErrorUserDeclined`.
pub const SC_ERROR_USER_DECLINED: i64 = sck_error::USER_DECLINED;

// ---------- pure parts ----------

/// `scale_milli` from the mode's backing-store width in pixels and its width in points (2000 on a Retina panel).
pub fn scale_milli_from(pixel_width: u32, point_width: u32) -> u16 {
    if pixel_width == 0 || point_width == 0 {
        return 1000;
    }
    let milli = (u64::from(pixel_width) * 1000 + u64::from(point_width) / 2) / u64::from(point_width);
    u16::try_from(milli.clamp(1000, 4000)).unwrap_or(1000)
}

fn round_i32(v: f64) -> i32 {
    // Display coordinates are far below i32 range; the clamp keeps a corrupt value from wrapping.
    v.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// Bounds in physical pixels of the virtual desktop from `CGDisplayBounds` (points: x, y, width, height).
pub fn bounds_in_pixels(points: (f64, f64, f64, f64), scale_milli: u16) -> Rect {
    let s = f64::from(scale_milli) / 1000.0;
    Rect {
        x: round_i32(points.0 * s),
        y: round_i32(points.1 * s),
        width: round_i32(points.2 * s).max(0) as u32,
        height: round_i32(points.3 * s).max(0) as u32,
    }
}

/// A global point (hot spot) to pixels inside the display whose point origin and scale are given.
pub fn cursor_to_pixels(global: (f64, f64), origin_points: (f64, f64), scale_milli: u16) -> (i32, i32) {
    let s = f64::from(scale_milli) / 1000.0;
    (round_i32((global.0 - origin_points.0) * s), round_i32((global.1 - origin_points.1) * s))
}

/// Whether a pixel position lies inside a display of `width x height` pixels.
pub fn inside_display(x: i32, y: i32, width: u32, height: u32) -> bool {
    x >= 0 && y >= 0 && i64::from(x) < i64::from(width) && i64::from(y) < i64::from(height)
}

/// `CGDisplayRotation` degrees to the `DisplayInfo.rotation` value (0, 90, 180, 270).
pub fn rotation_degrees(degrees: f64) -> u16 {
    let d = degrees.rem_euclid(360.0).round() as u32;
    (((d + 45) / 90) % 4 * 90) as u16
}

/// Refresh rate in Hz (0 = unknown) to millihertz.
pub fn refresh_mhz(hz: f64) -> u32 {
    if hz.is_finite() && hz > 0.5 { (hz * 1000.0).round() as u32 } else { DEFAULT_REFRESH_MHZ }
}

/// Name by the C3 convention: "Built-in display" or "Display <n>" (n counts from 1).
pub fn display_name(builtin: bool, id: u16) -> String {
    if builtin { "Built-in display".to_string() } else { format!("Display {}", u32::from(id) + 1) }
}

/// Keeps `DisplayId` indexes stable. `known` holds the `CGDirectDisplayID` of each index; new displays are appended
/// (on the first call the main display is placed first). Returns `(index, cg id)` for each active display.
pub fn assign_display_ids(known: &mut Vec<u32>, active: &[u32], main: u32) -> Vec<(u16, u32)> {
    if known.is_empty() && active.contains(&main) {
        known.push(main);
    }
    for &cg in active {
        if !known.contains(&cg) {
            known.push(cg);
        }
    }
    active
        .iter()
        .filter_map(|cg| known.iter().position(|k| k == cg).and_then(|i| u16::try_from(i).ok()).map(|i| (i, *cg)))
        .collect()
}

/// Dirty rects in pixels (`x, y, w, h`) clamped to the frame; empty rectangles are dropped. An empty result means the
/// whole frame (C3: "empty = unknown").
pub fn dirty_rects(raw: &[[f64; 4]], frame_width: u32, frame_height: u32) -> Vec<Rect> {
    let (fw, fh) = (f64::from(frame_width), f64::from(frame_height));
    let mut out = Vec::new();
    for r in raw {
        if !r.iter().all(|v| v.is_finite()) {
            continue;
        }
        let x0 = r[0].floor().clamp(0.0, fw);
        let y0 = r[1].floor().clamp(0.0, fh);
        let x1 = (r[0] + r[2]).ceil().clamp(0.0, fw);
        let y1 = (r[1] + r[3]).ceil().clamp(0.0, fh);
        if x1 > x0 && y1 > y0 {
            out.push(Rect { x: x0 as i32, y: y0 as i32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 });
        }
    }
    out
}

/// The dirty rects of two frames collapsed into one when the older one was dropped: if either is "whole frame" the
/// union is the whole frame.
pub fn merge_dirty(older: Vec<Rect>, newer: Vec<Rect>) -> Vec<Rect> {
    if older.is_empty() || newer.is_empty() {
        return Vec::new();
    }
    let mut all = older;
    all.extend(newer);
    if all.len() > 64 {
        return Vec::new();
    }
    all
}

/// What a frame's `SCStreamFrameInfoStatus` means for `next_frame`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A new picture.
    Complete,
    /// Idle, Blank, Suspended: nothing new.
    Nothing,
    /// Started, Stopped or unknown: ignored.
    Ignore,
}

/// `SCFrameStatus` raw value (`None` = attachment missing, treated as a picture).
pub fn classify_status(status: Option<i64>) -> FrameKind {
    match status {
        None | Some(0) => FrameKind::Complete,
        Some(1..=3) => FrameKind::Nothing,
        Some(_) => FrameKind::Ignore,
    }
}

/// Error kind for an `SCStreamError` code (the table shared with the audio source, see `sck_error`).
pub fn error_kind_for_code(code: i64) -> ErrorKind {
    sck_error::kind_for_code(code)
}

/// Id of a cursor shape: FNV-1a (32 bit) over width, height and the pixels, never 0 (0 means "no shape" on the wire).
pub fn shape_id(width: u16, height: u16, rgba: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    let mut feed = |b: u8| {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    };
    for b in width.to_le_bytes().into_iter().chain(height.to_le_bytes()) {
        feed(b);
    }
    for &b in rgba {
        feed(b);
    }
    if h == 0 { 1 } else { h }
}

/// Pixel size of a cursor bitmap for an image of `points` size on a display of `scale`, capped to
/// [`MAX_CURSOR_EDGE`]. Returns `(width, height, factor)`, `factor` = pixels per point actually used (also for the hot
/// spot). `None` for an empty image.
pub fn cursor_target_size(width_points: f64, height_points: f64, scale: f64) -> Option<(u32, u32, f64)> {
    if !(width_points.is_finite() && height_points.is_finite() && scale.is_finite())
        || width_points < 0.5
        || height_points < 0.5
        || scale <= 0.0
    {
        return None;
    }
    let mut factor = scale;
    let longest = width_points.max(height_points) * factor;
    if longest > f64::from(MAX_CURSOR_EDGE) {
        factor *= f64::from(MAX_CURSOR_EDGE) / longest;
    }
    let w = ((width_points * factor).round() as u32).clamp(1, MAX_CURSOR_EDGE);
    let h = ((height_points * factor).round() as u32).clamp(1, MAX_CURSOR_EDGE);
    Some((w, h, factor))
}

/// Premultiplied RGBA rows of `stride` bytes (top-down) to tight straight RGBA.
pub fn straight_rgba_from_premultiplied(src: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let start = row * stride;
        let Some(line) = src.get(start..start + width * 4) else { break };
        for px in line.as_chunks::<4>().0 {
            let a = u32::from(px[3]);
            if a == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else if a == 255 {
                out.extend_from_slice(px);
            } else {
                let un = |c: u8| ((u32::from(c) * 255 + a / 2) / a).min(255) as u8;
                out.extend_from_slice(&[un(px[0]), un(px[1]), un(px[2]), px[3]]);
            }
        }
    }
    out
}

// ---------- the mailbox ----------

/// A frame waiting in the mailbox.
pub struct Pending<B> {
    pub buffer: B,
    pub captured_at: MonoMicros,
    pub dirty: Vec<Rect>,
}

/// What `Mailbox::wait` returns.
pub enum Wake<B> {
    Frame(Pending<B>),
    /// An Idle/Blank/Suspended sample arrived: nothing new.
    Idle,
    /// The stream stopped for this reason (latched: every later wait returns it again).
    Stopped(Stop),
    Timeout,
}

/// Why the stream stopped: the error kind the helper acts on, the `NSError` code when there is one, a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub kind: ErrorKind,
    pub code: Option<i64>,
    pub message: String,
}

struct MailState<B> {
    frame: Option<Pending<B>>,
    idle: u64,
    idle_seen: u64,
    stopped: Option<Stop>,
    dropped: u64,
}

/// The single slot between the dispatch queue (producer) and the capture thread (consumer): newest wins.
pub struct Mailbox<B> {
    state: Mutex<MailState<B>>,
    cond: Condvar,
}

impl<B> Default for Mailbox<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B> Mailbox<B> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MailState { frame: None, idle: 0, idle_seen: 0, stopped: None, dropped: 0 }),
            cond: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MailState<B>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stores a frame, replacing (and merging the dirty rects of) an unconsumed one.
    pub fn push(&self, mut p: Pending<B>) {
        let mut s = self.lock();
        if let Some(old) = s.frame.take() {
            p.dirty = merge_dirty(old.dirty, core::mem::take(&mut p.dirty));
            s.dropped += 1;
        }
        s.frame = Some(p);
        drop(s);
        self.cond.notify_all();
    }

    pub fn push_idle(&self) {
        self.lock().idle += 1;
        self.cond.notify_all();
    }

    /// Latches the first stop; later ones are ignored.
    pub fn stop(&self, stop: Stop) {
        self.lock().stopped.get_or_insert(stop);
        self.cond.notify_all();
    }

    /// Frames replaced before they were taken.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    pub fn wait(&self, timeout: Duration) -> Wake<B> {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        loop {
            if let Some(stop) = &s.stopped {
                return Wake::Stopped(stop.clone());
            }
            if let Some(p) = s.frame.take() {
                return Wake::Frame(p);
            }
            if s.idle > s.idle_seen {
                s.idle_seen = s.idle;
                return Wake::Idle;
            }
            let now = Instant::now();
            if now >= deadline {
                return Wake::Timeout;
            }
            s = self.cond.wait_timeout(s, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
        }
    }
}

#[cfg(target_os = "macos")]
fn state_error(op: &'static str, detail: &str) -> swapdesk_platform::PlatformError {
    swapdesk_platform::PlatformError { kind: ErrorKind::Closed, op, os_code: None, detail: detail.to_string() }
}

#[cfg(target_os = "macos")]
pub use mac::{DisplayRegistry, MacScreen, render_cursor, system_cursor_shape};

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use core::ffi::c_void;
    use std::sync::Arc;
    use std::sync::mpsc;

    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::AnyObject;
    use objc2::runtime::ProtocolObject;
    use objc2::{AnyThread, DefinedClass, define_class, msg_send};
    use objc2_app_kit::NSCursor;
    use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CFType, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin,
        CGDisplayMode, CGDisplayRotation, CGEvent, CGGetActiveDisplayList, CGImageAlphaInfo, CGMainDisplayID,
        CGPreflightScreenCaptureAccess, CGRectMakeWithDictionaryRepresentation,
    };
    use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    };
    use objc2_foundation::{
        NSArray, NSError, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSValue,
    };
    use objc2_screen_capture_kit::{
        SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
        SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
    };
    use swapdesk_platform::{
        AdapterId, CaptureConfig, Clock, CursorShape, CursorUpdate, DisplayId, DisplayInfo, Frame, GpuSurface, OsGrant,
        Pixels, PlatformError, Result, ScreenSource,
    };

    /// How long the content query and the start may take (see the module documentation for why it is not longer).
    const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
    /// How long a stop may take before the stream is dropped anyway.
    const STOP_TIMEOUT: Duration = Duration::from_secs(2);
    /// How often `next_frame` re-reads the display mode to notice a size change.
    const GEOMETRY_CHECK: Duration = Duration::from_millis(250);
    /// Minimum time between two reads of the system cursor image.
    const SHAPE_POLL: Duration = Duration::from_millis(50);
    const QUEUE_DEPTH: isize = 4;

    /// Moves a value to another thread when the type has no `Send` but the use is single-owner.
    struct Handoff<T>(T);
    // SAFETY: used only for values that are handed to exactly one consumer thread and never touched concurrently:
    // Core Foundation retain/release is thread safe and the pixel buffer is only read through the encoder afterwards;
    // the shareable content is created by the completion handler and used only after the hand-off.
    unsafe impl<T> Send for Handoff<T> {}

    type Buffer = Handoff<CFRetained<CVPixelBuffer>>;

    struct OutputIvars {
        mailbox: Arc<Mailbox<Buffer>>,
        clock: Arc<dyn Clock>,
        /// Pixel size the stream was built for; a different buffer size stops the mailbox with a message.
        size: (u32, u32),
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; this class does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "SwapdeskScreenCaptureOutput"]
        #[ivars = OutputIvars]
        struct ScreenOutput;

        // SAFETY: `NSObjectProtocol` has no safety requirements.
        unsafe impl NSObjectProtocol for ScreenOutput {}

        // SAFETY: the selector and signature are those of `SCStreamOutput`.
        unsafe impl SCStreamOutput for ScreenOutput {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
                if kind != SCStreamOutputType::Screen {
                    return;
                }
                // The queue's own pool is drained at times of the system's choosing: one per sample keeps it flat.
                autoreleasepool(|_| self.handle_sample(sample));
            }
        }

        // SAFETY: the selector and signature are those of `SCStreamDelegate`.
        unsafe impl SCStreamDelegate for ScreenOutput {
            #[unsafe(method(stream:didStopWithError:))]
            fn did_stop(&self, _stream: &SCStream, error: &NSError) {
                let code = error.code() as i64;
                self.ivars().mailbox.stop(Stop {
                    kind: sck_error::stop_kind(code),
                    code: Some(code),
                    message: error.localizedDescription().to_string(),
                });
            }
        }
    );

    impl ScreenOutput {
        fn create(ivars: OutputIvars) -> Retained<Self> {
            let this = Self::alloc().set_ivars(ivars);
            // SAFETY: `init` is NSObject's initializer.
            unsafe { msg_send![super(this), init] }
        }

        fn handle_sample(&self, sample: &CMSampleBuffer) {
            let iv = self.ivars();
            // SAFETY: both calls only read the sample buffer handed to this callback.
            if !unsafe { sample.is_valid() && sample.data_is_ready() } {
                return;
            }
            let attachments = frame_attachments(sample);
            match classify_status(attachments.status) {
                FrameKind::Ignore => {}
                FrameKind::Nothing => iv.mailbox.push_idle(),
                FrameKind::Complete => {
                    // SAFETY: reading the image buffer of a valid sample; the result is retained.
                    let Some(buffer) = (unsafe { sample.image_buffer() }) else {
                        // A complete status without a picture: nothing to deliver.
                        iv.mailbox.push_idle();
                        return;
                    };
                    let (w, h) = (CVPixelBufferGetWidth(&buffer) as u32, CVPixelBufferGetHeight(&buffer) as u32);
                    if (w, h) != iv.size {
                        // The display changed mode under the stream: rebuild it (`AccessLost`).
                        iv.mailbox.stop(Stop {
                            kind: ErrorKind::AccessLost,
                            code: None,
                            message: format!(
                                "frame size {w}x{h} differs from the stream's {}x{}",
                                iv.size.0, iv.size.1
                            ),
                        });
                        return;
                    }
                    iv.mailbox.push(Pending {
                        buffer: Handoff(buffer),
                        captured_at: iv.clock.now(),
                        dirty: dirty_rects(&attachments.dirty, w, h),
                    });
                }
            }
        }
    }

    struct Attachments {
        status: Option<i64>,
        dirty: Vec<[f64; 4]>,
    }

    /// Reads the `SCStreamFrameInfo` keys of the first sample attachment dictionary.
    fn frame_attachments(sample: &CMSampleBuffer) -> Attachments {
        let mut out = Attachments { status: None, dirty: Vec::new() };
        // SAFETY: reads the attachments array of a valid sample buffer (not created when absent).
        let Some(array) = (unsafe { sample.sample_attachments_array(false) }) else { return out };
        let array: &CFArray = &array;
        if array.count() < 1 {
            return out;
        }
        // SAFETY: index 0 is in range; the attachments array holds CFDictionary values.
        let dict_ptr = unsafe { array.value_at_index(0) };
        if dict_ptr.is_null() {
            return out;
        }
        // SAFETY: the pointer is a live CFDictionary owned by the sample buffer for this call.
        let dict: &CFDictionary = unsafe { &*dict_ptr.cast::<CFDictionary>() };
        // SAFETY: the keys are process-lifetime NSString constants; CFDictionary lookup by pointer/equality.
        let status = unsafe { dict.value((SCStreamFrameInfoStatus as *const NSString).cast::<c_void>()) };
        if !status.is_null() {
            // SAFETY: the status value is an NSNumber.
            out.status = Some(unsafe { &*status.cast::<NSNumber>() }.integerValue() as i64);
        }
        // SAFETY: as above.
        let rects = unsafe { dict.value((SCStreamFrameInfoDirtyRects as *const NSString).cast::<c_void>()) };
        if !rects.is_null() {
            // The dirty rects value is an array of CGRect dictionary representations (SCStreamFrameInfoDirtyRects;
            // CGRectMakeWithDictionaryRepresentation reads one). Each element's type is checked before it is read:
            // sending `rectValue` to one of those dictionaries aborted the process on the first complete frame.
            // SAFETY: the value is a live CFArray (toll-free bridged NSArray) owned by the sample buffer.
            let list: &CFArray = unsafe { &*rects.cast::<CFArray>() };
            for i in 0..list.count() {
                // SAFETY: `i` is in range; the element lives as long as the array.
                let item = unsafe { list.value_at_index(i) };
                if item.is_null() {
                    continue;
                }
                // SAFETY: every element of a CFArray is a CF object.
                let cf: &CFType = unsafe { &*item.cast::<CFType>() };
                let rect = if let Some(dict) = cf.downcast_ref::<CFDictionary>() {
                    let mut r = CGRect::default();
                    // SAFETY: `dict` is a CFDictionary and `r` a valid out pointer.
                    unsafe { CGRectMakeWithDictionaryRepresentation(Some(dict), &mut r) }.then_some(r)
                } else {
                    // SAFETY: every CF object is also an Objective-C object (toll-free bridging).
                    let obj: &AnyObject = unsafe { &*item.cast::<AnyObject>() };
                    // SAFETY: an NSValue that holds a CGRect (the type is checked by the downcast).
                    obj.downcast_ref::<NSValue>().map(|v| unsafe { v.rectValue() })
                };
                if let Some(r) = rect {
                    out.dirty.push([r.origin.x, r.origin.y, r.size.width, r.size.height]);
                }
            }
        }
        out
    }

    // ---------- display geometry ----------

    #[derive(Debug, Clone, Copy)]
    struct Geometry {
        cg: u32,
        origin_points: (f64, f64),
        scale_milli: u16,
        size_px: (u32, u32),
    }

    fn read_geometry(cg: u32) -> Option<Geometry> {
        let b = CGDisplayBounds(cg);
        let mode = CGDisplayCopyDisplayMode(cg)?;
        let (pw, w) = (CGDisplayMode::pixel_width(Some(&mode)) as u32, CGDisplayMode::width(Some(&mode)) as u32);
        let scale = scale_milli_from(pw, w);
        let px = bounds_in_pixels((b.origin.x, b.origin.y, b.size.width, b.size.height), scale);
        Some(Geometry {
            cg,
            origin_points: (b.origin.x, b.origin.y),
            scale_milli: scale,
            size_px: (px.width, px.height),
        })
    }

    fn active_display_ids() -> Result<Vec<u32>> {
        let mut ids = [0u32; 32];
        let mut count = 0u32;
        // SAFETY: the buffer holds 32 ids and `count` receives how many were written.
        let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
        if err != objc2_core_graphics::CGError::Success {
            return Err(PlatformError {
                kind: ErrorKind::Os,
                op: "CGGetActiveDisplayList",
                os_code: Some(i64::from(err.0)),
                detail: String::new(),
            });
        }
        Ok(ids[..(count as usize).min(ids.len())].to_vec())
    }

    // ---------- errors ----------

    fn sc_error(op: &'static str, e: &NSError) -> PlatformError {
        let code = e.code() as i64;
        let kind = error_kind_for_code(code);
        let detail = match kind {
            ErrorKind::PermissionDenied(_) => {
                format!("Screen Recording is not allowed for this program: {}", e.localizedDescription())
            }
            _ => e.localizedDescription().to_string(),
        };
        PlatformError { kind, op, os_code: Some(code), detail }
    }

    fn denied(op: &'static str) -> PlatformError {
        PlatformError {
            kind: ErrorKind::PermissionDenied(OsGrant::ScreenRecording),
            op,
            os_code: None,
            detail: "Screen Recording is not allowed for this program (System Settings > Privacy & Security)".into(),
        }
    }

    // ---------- the stream ----------

    struct OpenStream {
        stream: Retained<SCStream>,
        output: Retained<ScreenOutput>,
        _queue: DispatchRetained<DispatchQueue>,
        mailbox: Arc<Mailbox<Buffer>>,
        geometry: Geometry,
        checked: Instant,
    }

    struct Target {
        display: DisplayId,
        config: CaptureConfig,
    }

    #[derive(Default)]
    struct CursorState {
        last: Option<(i32, i32, bool)>,
        shape_id: Option<u32>,
        polled: Option<Instant>,
    }

    /// `ScreenSource` for macOS. Built and used on one thread.
    pub struct MacScreen {
        clock: Arc<dyn Clock>,
        known: DisplayRegistry,
        target: Option<Target>,
        open: Option<OpenStream>,
        cursor: CursorState,
    }

    /// The `CGDirectDisplayID` of each `DisplayId` index (see [`assign_display_ids`]), shared by every `MacScreen` of
    /// one helper: the screen that lists the displays and the one that streams a display agree on what an id means,
    /// whatever order the system lists a monitor in after one was plugged in.
    pub type DisplayRegistry = Arc<std::sync::Mutex<Vec<u32>>>;

    impl MacScreen {
        /// A screen with its own display ids.
        pub fn new(clock: Arc<dyn Clock>) -> Result<Self> {
            Self::with_registry(clock, DisplayRegistry::default())
        }

        /// A screen whose display ids come from `registry`, shared with the other screens of the helper.
        pub fn with_registry(clock: Arc<dyn Clock>, registry: DisplayRegistry) -> Result<Self> {
            Ok(Self { clock, known: registry, target: None, open: None, cursor: CursorState::default() })
        }

        fn cg_of(&mut self, display: DisplayId) -> Result<u32> {
            let index = usize::from(display.0);
            let lookup = |known: &DisplayRegistry| {
                known.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(index).copied()
            };
            // A display plugged in since the last listing has no index yet: list again before refusing.
            let cg = match lookup(&self.known) {
                Some(cg) => Some(cg),
                None => {
                    self.list()?;
                    lookup(&self.known)
                }
            };
            cg.ok_or_else(|| PlatformError {
                kind: ErrorKind::InvalidInput,
                op: "MacScreen::open",
                os_code: None,
                detail: format!("no display {}", display.0),
            })
        }

        fn list(&mut self) -> Result<Vec<(u16, u32)>> {
            let active = active_display_ids()?;
            let mut known = self.known.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(assign_display_ids(&mut known, &active, CGMainDisplayID()))
        }

        /// Builds and starts the stream inside an autorelease pool (the capture thread has no pool of its own).
        fn build(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()> {
            autoreleasepool(|_| self.build_in_pool(display, config))
        }

        fn build_in_pool(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()> {
            if !CGPreflightScreenCaptureAccess() {
                return Err(denied("CGPreflightScreenCaptureAccess"));
            }
            let cg = self.cg_of(display)?;
            let geometry = read_geometry(cg).ok_or_else(|| PlatformError {
                kind: ErrorKind::AccessLost,
                op: "CGDisplayCopyDisplayMode",
                os_code: None,
                detail: format!("display {cg} has no mode"),
            })?;
            let content = shareable_content()?;
            // SAFETY: plain property reads on the content object, used on this thread only.
            let all = unsafe { content.0.displays() };
            let sc_display = (0..all.count())
                .map(|i| all.objectAtIndex(i))
                // SAFETY: `displayID` is a plain property read.
                .find(|d| unsafe { d.displayID() } == cg)
                .ok_or_else(|| PlatformError {
                    kind: ErrorKind::AccessLost,
                    op: "SCShareableContent",
                    os_code: None,
                    detail: format!("display {cg} is not shareable"),
                })?;
            let no_windows: Retained<NSArray<SCWindow>> = NSArray::new();
            // SAFETY: initializer of SCContentFilter with a display and an empty exclusion list.
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &sc_display, &no_windows)
            };
            // SAFETY: SCStreamConfiguration setters, all on the object just created.
            let conf = unsafe {
                let c = SCStreamConfiguration::new();
                c.setWidth(geometry.size_px.0 as usize);
                c.setHeight(geometry.size_px.1 as usize);
                c.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
                c.setColorMatrix(objc2_core_graphics::kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
                c.setShowsCursor(false);
                c.setQueueDepth(QUEUE_DEPTH);
                c.setMinimumFrameInterval(frame_interval(config.max_fps));
                c
            };
            let mailbox = Arc::new(Mailbox::new());
            let output = ScreenOutput::create(OutputIvars {
                mailbox: mailbox.clone(),
                clock: self.clock.clone(),
                size: geometry.size_px,
            });
            let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
            // SAFETY: initializer with a filter, a configuration and the delegate (kept alive in `OpenStream`).
            let stream = unsafe {
                SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &conf, Some(delegate))
            };
            let queue = DispatchQueue::new("swapdesk.capture.screen", DispatchQueueAttr::SERIAL);
            let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
            // SAFETY: the queue is serial and outlives the stream (held in `OpenStream`).
            unsafe {
                stream.addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Screen, Some(&queue))
            }
            .map_err(|e| sc_error("SCStream.addStreamOutput", &e))?;
            let open = OpenStream { stream, output, _queue: queue, mailbox, geometry, checked: Instant::now() };
            let (tx, rx) = mpsc::channel::<Option<(i64, String)>>();
            let block = RcBlock::new(move |err: *mut NSError| {
                // SAFETY: when non-null the pointer is a live NSError for the duration of the callback.
                let e = unsafe { err.as_ref() }.map(|e| (e.code() as i64, e.localizedDescription().to_string()));
                let _ = tx.send(e);
            });
            // SAFETY: the block lives until the call returns; ScreenCaptureKit copies it.
            unsafe { open.stream.startCaptureWithCompletionHandler(Some(&block)) };
            match rx.recv_timeout(REPLY_TIMEOUT) {
                Ok(None) => {}
                Ok(Some((code, detail))) => {
                    // The start failed: the stream never ran and is dropped as it is.
                    let kind = error_kind_for_code(code);
                    let detail = if matches!(kind, ErrorKind::PermissionDenied(_)) {
                        format!("Screen Recording is not allowed for this program: {detail}")
                    } else {
                        detail
                    };
                    return Err(PlatformError { kind, op: "SCStream.startCapture", os_code: Some(code), detail });
                }
                Err(_) => {
                    // No reply: the start may still complete later (a consent prompt). Stop it, so that no capture
                    // session outlives the stream object, then drop it.
                    halt(open);
                    return Err(PlatformError {
                        kind: ErrorKind::Timeout,
                        op: "SCStream.startCapture",
                        os_code: None,
                        detail: "no reply from ScreenCaptureKit".into(),
                    });
                }
            }
            self.open = Some(open);
            self.target = Some(Target { display, config });
            self.cursor = CursorState::default();
            Ok(())
        }

        fn stop_stream(&mut self) {
            let Some(open) = self.open.take() else { return };
            autoreleasepool(|_| halt(open));
        }
    }

    /// Stops a stream (waiting at most [`STOP_TIMEOUT`] for the completion) and detaches its output.
    fn halt(open: OpenStream) {
        let (tx, rx) = mpsc::channel::<()>();
        let block = RcBlock::new(move |_err: *mut NSError| {
            let _ = tx.send(());
        });
        // SAFETY: the block lives until the call returns; the completion only signals the channel.
        unsafe { open.stream.stopCaptureWithCompletionHandler(Some(&block)) };
        let _ = rx.recv_timeout(STOP_TIMEOUT);
        let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*open.output);
        // SAFETY: removes the output added in `build`; failure (already stopped) is harmless.
        let _ = unsafe { open.stream.removeStreamOutput_type_error(sink, SCStreamOutputType::Screen) };
    }

    impl Drop for MacScreen {
        fn drop(&mut self) {
            self.stop_stream();
        }
    }

    fn frame_interval(max_fps: u16) -> CMTime {
        // `CMTimeMake(1, fps)` built by hand: a valid CMTime of 1/fps seconds (0 = the system's maximum rate).
        if max_fps == 0 {
            CMTime { value: 0, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 }
        } else {
            CMTime { value: 1, timescale: i32::from(max_fps), flags: CMTimeFlags::Valid, epoch: 0 }
        }
    }

    fn shareable_content() -> Result<Handoff<Retained<SCShareableContent>>> {
        let (tx, rx) = mpsc::channel::<core::result::Result<Handoff<Retained<SCShareableContent>>, PlatformError>>();
        let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
            // SAFETY: `content` is a live object when non-null; retaining it makes it outlive the callback.
            let r = match unsafe { Retained::retain(content) } {
                Some(c) => Ok(Handoff(c)),
                None => Err(match unsafe { err.as_ref() } {
                    Some(e) => sc_error("SCShareableContent.get", e),
                    None => denied("SCShareableContent.get"),
                }),
            };
            let _ = tx.send(r);
        });
        // SAFETY: the block is copied by ScreenCaptureKit and only sends on the channel.
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(PlatformError {
                kind: ErrorKind::Timeout,
                op: "SCShareableContent.get",
                os_code: None,
                detail: "no reply from ScreenCaptureKit".into(),
            }),
        }
    }

    impl ScreenSource for MacScreen {
        fn displays(&mut self) -> Result<Vec<DisplayInfo>> {
            let main = CGMainDisplayID();
            let mut out = Vec::new();
            for (index, cg) in self.list()? {
                let Some(g) = read_geometry(cg) else { continue };
                let b = CGDisplayBounds(cg);
                let hz = CGDisplayCopyDisplayMode(cg).map_or(0.0, |m| CGDisplayMode::refresh_rate(Some(&m)));
                out.push(DisplayInfo {
                    id: DisplayId(index),
                    name: display_name(CGDisplayIsBuiltin(cg), index),
                    bounds: bounds_in_pixels((b.origin.x, b.origin.y, b.size.width, b.size.height), g.scale_milli),
                    scale_milli: g.scale_milli,
                    refresh_mhz: refresh_mhz(hz),
                    rotation: rotation_degrees(CGDisplayRotation(cg)),
                    primary: cg == main,
                    adapter: AdapterId(0),
                });
            }
            out.sort_by_key(|d| d.id.0);
            Ok(out)
        }

        fn open(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()> {
            self.stop_stream();
            self.build(display, config)
        }

        fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
            let Some(target) = &self.target else { return Err(state_error("MacScreen::next_frame", "not open")) };
            let display = target.display;
            let Some(open) = self.open.as_mut() else {
                return Err(PlatformError {
                    kind: ErrorKind::AccessLost,
                    op: "MacScreen::next_frame",
                    os_code: None,
                    detail: "stream is down, reacquire".into(),
                });
            };
            if open.checked.elapsed() >= GEOMETRY_CHECK {
                open.checked = Instant::now();
                match read_geometry(open.geometry.cg) {
                    Some(g) if g.size_px == open.geometry.size_px => open.geometry = g,
                    other => {
                        let detail = match other {
                            Some(g) => format!("display size changed to {}x{}", g.size_px.0, g.size_px.1),
                            None => "display went away".to_string(),
                        };
                        return Err(PlatformError {
                            kind: ErrorKind::AccessLost,
                            op: "MacScreen::next_frame",
                            os_code: None,
                            detail,
                        });
                    }
                }
            }
            let size = open.geometry.size_px;
            match open.mailbox.wait(timeout) {
                Wake::Frame(p) => Ok(Some(Frame {
                    display,
                    captured_at: p.captured_at,
                    width: size.0,
                    height: size.1,
                    pixels: Pixels::Gpu(GpuSurface(Box::new(p.buffer.0))),
                    dirty: p.dirty,
                    content_changed: true,
                })),
                Wake::Idle | Wake::Timeout => Ok(None),
                Wake::Stopped(stop) => Err(PlatformError {
                    kind: stop.kind,
                    op: "SCStream.didStopWithError",
                    os_code: stop.code,
                    detail: stop.message,
                }),
            }
        }

        fn reacquire(&mut self) -> Result<()> {
            let Some(t) = &self.target else { return Err(state_error("MacScreen::reacquire", "never opened")) };
            let (display, config) = (t.display, t.config);
            self.stop_stream();
            self.build(display, config)
        }

        fn take_cursor(&mut self) -> Option<CursorUpdate> {
            let display = self.target.as_ref()?.display;
            let g = self.open.as_ref()?.geometry;
            let event = CGEvent::new(None)?;
            let loc = CGEvent::location(Some(&event));
            let (x, y) = cursor_to_pixels((loc.x, loc.y), g.origin_points, g.scale_milli);
            let visible = inside_display(x, y, g.size_px.0, g.size_px.1);
            let first = self.cursor.last.is_none();
            let mut shape = None;
            let due = self.cursor.polled.is_none_or(|t| t.elapsed() >= SHAPE_POLL);
            if due {
                self.cursor.polled = Some(Instant::now());
                if let Some(s) = system_cursor_shape(f64::from(g.scale_milli) / 1000.0)
                    && self.cursor.shape_id != Some(s.id)
                {
                    self.cursor.shape_id = Some(s.id);
                    shape = Some(s);
                }
            }
            let now = (x, y, visible);
            if !first && self.cursor.last == Some(now) && shape.is_none() {
                return None;
            }
            self.cursor.last = Some(now);
            Some(CursorUpdate { display, x, y, visible, shape })
        }

        fn close(&mut self) {
            self.stop_stream();
            self.target = None;
            self.cursor = CursorState::default();
        }
    }

    /// The cursor image the system shows now, as straight RGBA (see the module doc for the API caveat). `None` when
    /// AppKit has no current system cursor (no window server session): there is no fallback image, so the caller reports
    /// no shape change. Runs inside an autorelease pool.
    #[allow(deprecated)]
    pub fn system_cursor_shape(scale: f64) -> Option<CursorShape> {
        autoreleasepool(|_| {
            let cursor = NSCursor::currentSystemCursor()?;
            render_cursor(&cursor, scale)
        })
    }

    /// Renders an `NSCursor` to straight RGBA at `scale` pixels per point (capped to 256 x 256).
    pub fn render_cursor(cursor: &NSCursor, scale: f64) -> Option<CursorShape> {
        let image = cursor.image();
        let size: NSSize = image.size();
        let hot: NSPoint = cursor.hotSpot();
        let (w, h, factor) = cursor_target_size(size.width, size.height, scale)?;
        let (wu, hu) = (w as usize, h as usize);
        let mut buf = vec![0u8; wu * hu * 4];
        let space = CGColorSpace::new_device_rgb()?;
        // SAFETY: `buf` holds `h` rows of `w * 4` bytes and outlives the context (dropped before `buf` is read).
        let ctx = unsafe {
            CGBitmapContextCreate(
                buf.as_mut_ptr().cast::<c_void>(),
                wu,
                hu,
                8,
                wu * 4,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }?;
        let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(f64::from(w), f64::from(h)));
        // SAFETY: `rect` is a valid pointer for the call; no context and no hints.
        let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
        CGContext::draw_image(
            Some(&ctx),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(f64::from(w), f64::from(h))),
            Some(&cg),
        );
        drop(ctx);
        let rgba = straight_rgba_from_premultiplied(&buf, wu * 4, wu, hu);
        let hot_x = (hot.x * factor).round().clamp(0.0, f64::from(w.saturating_sub(1))) as u16;
        let hot_y = (hot.y * factor).round().clamp(0.0, f64::from(h.saturating_sub(1))) as u16;
        let (w16, h16) = (u16::try_from(w).ok()?, u16::try_from(h).ok()?);
        Some(CursorShape { id: shape_id(w16, h16, &rgba), width: w16, height: h16, hot_x, hot_y, rgba })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_platform::OsGrant;

    #[test]
    fn scale_from_mode() {
        assert_eq!(scale_milli_from(2880, 1440), 2000);
        assert_eq!(scale_milli_from(1920, 1920), 1000);
        assert_eq!(scale_milli_from(0, 100), 1000);
        assert_eq!(scale_milli_from(3024, 1512), 2000);
    }

    #[test]
    fn bounds_points_to_pixels() {
        let r = bounds_in_pixels((-1440.0, 100.0, 1440.0, 900.0), 2000);
        assert_eq!(r, Rect { x: -2880, y: 200, width: 2880, height: 1800 });
        let r = bounds_in_pixels((0.0, 0.0, 1920.0, 1080.0), 1000);
        assert_eq!(r, Rect { x: 0, y: 0, width: 1920, height: 1080 });
    }

    #[test]
    fn cursor_position_in_pixels() {
        // Retina display placed left of the main one: global point (-1000, 50), origin (-1440, 0).
        assert_eq!(cursor_to_pixels((-1000.0, 50.0), (-1440.0, 0.0), 2000), (880, 100));
        assert_eq!(cursor_to_pixels((10.4, 10.6), (0.0, 0.0), 1000), (10, 11));
        assert!(inside_display(0, 0, 10, 10));
        assert!(inside_display(9, 9, 10, 10));
        assert!(!inside_display(10, 5, 10, 10));
        assert!(!inside_display(-1, 5, 10, 10));
    }

    #[test]
    fn rotation_and_refresh() {
        assert_eq!(rotation_degrees(0.0), 0);
        assert_eq!(rotation_degrees(90.0), 90);
        assert_eq!(rotation_degrees(270.0), 270);
        assert_eq!(rotation_degrees(360.0), 0);
        assert_eq!(rotation_degrees(-90.0), 270);
        assert_eq!(refresh_mhz(59.94), 59_940);
        assert_eq!(refresh_mhz(0.0), DEFAULT_REFRESH_MHZ);
    }

    #[test]
    fn names_and_ids() {
        assert_eq!(display_name(true, 0), "Built-in display");
        assert_eq!(display_name(false, 1), "Display 2");
        let mut known = Vec::new();
        let first = assign_display_ids(&mut known, &[7, 3, 9], 9);
        assert_eq!(first, vec![(1, 7), (2, 3), (0, 9)]);
        // 7 unplugged, 5 appears: indexes of the others stay, 5 gets a new one.
        let next = assign_display_ids(&mut known, &[3, 9, 5], 9);
        assert_eq!(next, vec![(2, 3), (0, 9), (3, 5)]);
        // 7 back: its old index returns.
        assert_eq!(assign_display_ids(&mut known, &[7], 9), vec![(1, 7)]);
    }

    #[test]
    fn dirty_conversion_clamps_and_drops() {
        let raw =
            [[10.2, 20.7, 100.0, 50.0], [-5.0, -5.0, 10.0, 10.0], [1900.0, 0.0, 100.0, 10.0], [5.0, 5.0, 0.0, 9.0]];
        let got = dirty_rects(&raw, 1920, 1080);
        assert_eq!(
            got,
            vec![
                Rect { x: 10, y: 20, width: 101, height: 51 },
                Rect { x: 0, y: 0, width: 5, height: 5 },
                Rect { x: 1900, y: 0, width: 20, height: 10 },
            ]
        );
        assert!(dirty_rects(&[], 10, 10).is_empty());
        assert!(dirty_rects(&[[f64::NAN, 0.0, 1.0, 1.0]], 10, 10).is_empty());
    }

    #[test]
    fn dirty_merge_rules() {
        let a = vec![Rect { x: 0, y: 0, width: 1, height: 1 }];
        let b = vec![Rect { x: 5, y: 5, width: 1, height: 1 }];
        assert_eq!(merge_dirty(a.clone(), b.clone()).len(), 2);
        assert!(merge_dirty(Vec::new(), b.clone()).is_empty());
        assert!(merge_dirty(a, Vec::new()).is_empty());
    }

    #[test]
    fn status_classes() {
        assert_eq!(classify_status(Some(0)), FrameKind::Complete);
        assert_eq!(classify_status(None), FrameKind::Complete);
        for s in 1..=3 {
            assert_eq!(classify_status(Some(s)), FrameKind::Nothing);
        }
        assert_eq!(classify_status(Some(4)), FrameKind::Ignore);
        assert_eq!(classify_status(Some(5)), FrameKind::Ignore);
        assert_eq!(error_kind_for_code(-3801), ErrorKind::PermissionDenied(OsGrant::ScreenRecording));
        assert_eq!(error_kind_for_code(-3811), ErrorKind::Os);
    }

    #[test]
    fn stream_error_codes_map_to_the_helper_kinds() {
        // The table itself is tested in `sck_error`; this is the screen's view of it.
        assert_eq!(error_kind_for_code(-3817), ErrorKind::Closed);
        assert_eq!(error_kind_for_code(-3816), ErrorKind::Closed);
        assert_eq!(error_kind_for_code(-3808), ErrorKind::Closed);
        assert_eq!(error_kind_for_code(SC_ERROR_USER_DECLINED), ErrorKind::PermissionDenied(OsGrant::ScreenRecording));
        assert_eq!(error_kind_for_code(-3821), ErrorKind::AccessLost);
    }

    #[test]
    fn shape_id_is_stable_and_sensitive() {
        let px = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let a = shape_id(2, 1, &px);
        assert_eq!(a, shape_id(2, 1, &px));
        assert_ne!(a, shape_id(1, 2, &px));
        let mut other = px.clone();
        other[7] = 9;
        assert_ne!(a, shape_id(2, 1, &other));
        assert_ne!(a, 0);
    }

    #[test]
    fn cursor_size_caps_at_256() {
        assert_eq!(cursor_target_size(16.0, 16.0, 2.0), Some((32, 32, 2.0)));
        let (w, h, f) = cursor_target_size(200.0, 100.0, 2.0).unwrap();
        assert_eq!((w, h), (256, 128));
        assert!((f - 1.28).abs() < 1e-9);
        assert_eq!(cursor_target_size(0.0, 10.0, 2.0), None);
        assert_eq!(cursor_target_size(10.0, 10.0, 0.0), None);
    }

    #[test]
    fn premultiplied_to_straight_rgba() {
        // Two pixels, stride 12 (4 bytes padding): opaque red, half-transparent premultiplied (50,100,150 at a=200).
        let src = [255, 0, 0, 255, 100, 50, 25, 100, 9, 9, 9, 9];
        let out = straight_rgba_from_premultiplied(&src, 12, 2, 1);
        assert_eq!(out, vec![255, 0, 0, 255, 255, 128, 64, 100]);
        // Fully transparent pixels lose their colour.
        assert_eq!(straight_rgba_from_premultiplied(&[7, 7, 7, 0], 4, 1, 1), vec![0, 0, 0, 0]);
        // A short buffer yields only the complete rows.
        assert_eq!(straight_rgba_from_premultiplied(&[1, 2, 3, 255], 4, 1, 2), vec![1, 2, 3, 255]);
    }

    #[test]
    fn mailbox_newest_wins_and_merges_dirty() {
        let m: Mailbox<u32> = Mailbox::new();
        let r1 = vec![Rect { x: 0, y: 0, width: 2, height: 2 }];
        let r2 = vec![Rect { x: 8, y: 8, width: 2, height: 2 }];
        m.push(Pending { buffer: 1, captured_at: MonoMicros(10), dirty: r1 });
        m.push(Pending { buffer: 2, captured_at: MonoMicros(20), dirty: r2 });
        assert_eq!(m.dropped(), 1);
        match m.wait(Duration::from_millis(10)) {
            Wake::Frame(p) => {
                assert_eq!(p.buffer, 2);
                assert_eq!(p.captured_at, MonoMicros(20));
                assert_eq!(p.dirty.len(), 2);
            }
            _ => panic!("expected a frame"),
        }
        assert!(matches!(m.wait(Duration::from_millis(5)), Wake::Timeout));
    }

    #[test]
    fn mailbox_idle_and_stop() {
        let m: Mailbox<u32> = Mailbox::new();
        m.push_idle();
        assert!(matches!(m.wait(Duration::from_millis(5)), Wake::Idle));
        assert!(matches!(m.wait(Duration::from_millis(5)), Wake::Timeout));
        m.push(Pending { buffer: 1, captured_at: MonoMicros(1), dirty: Vec::new() });
        m.stop(Stop { kind: ErrorKind::Closed, code: Some(-3817), message: "boom".into() });
        // The first stop wins over a waiting frame and over a later stop, keeps its kind and code, and stays latched.
        m.stop(Stop { kind: ErrorKind::AccessLost, code: Some(-3821), message: "later".into() });
        for _ in 0..2 {
            match m.wait(Duration::from_millis(5)) {
                Wake::Stopped(s) => {
                    assert_eq!((s.kind, s.code, s.message.as_str()), (ErrorKind::Closed, Some(-3817), "boom"));
                }
                _ => panic!("expected the latched stop"),
            }
        }
    }

    #[test]
    fn mailbox_wakes_a_waiter() {
        let m = std::sync::Arc::new(Mailbox::<u32>::new());
        let m2 = m.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            m2.push(Pending { buffer: 7, captured_at: MonoMicros(1), dirty: Vec::new() });
        });
        assert!(matches!(m.wait(Duration::from_secs(5)), Wake::Frame(p) if p.buffer == 7));
        t.join().unwrap();
    }

    #[cfg(target_os = "macos")]
    mod mac {
        use super::super::*;
        use std::sync::Arc;
        use swapdesk_platform::{CaptureConfig, Clock, DisplayId, OsGrant, Pixels, ScreenSource};

        struct StdClock(Instant);
        impl Clock for StdClock {
            fn now(&self) -> MonoMicros {
                MonoMicros(self.0.elapsed().as_micros() as u64)
            }
        }

        fn screen() -> MacScreen {
            MacScreen::new(Arc::new(StdClock(Instant::now()))).unwrap()
        }

        #[test]
        fn displays_never_panics_and_lists_sane_values_when_there_are_any() {
            // Over SSH there is no window server session and the list is empty; in a GUI session it is not.
            let list = screen().displays().unwrap();
            for d in &list {
                assert!(d.bounds.width > 0 && d.bounds.height > 0, "{d:?}");
                assert!((1000..=4000).contains(&d.scale_milli), "{d:?}");
                assert!(d.refresh_mhz >= 1000, "{d:?}");
                assert!([0, 90, 180, 270].contains(&d.rotation), "{d:?}");
                assert!(!d.name.is_empty());
            }
            assert!(list.iter().filter(|d| d.primary).count() <= 1);
        }

        #[test]
        #[ignore = "needs a window server session (logged-in GUI session; SSH has none): lists the real displays"]
        fn displays_lists_at_least_one_display() {
            let list = screen().displays().unwrap();
            assert!(!list.is_empty(), "no active display");
            assert_eq!(list.iter().filter(|d| d.primary).count(), 1);
            assert_eq!(list[0].id, DisplayId(0));
            assert!(list[0].primary);
        }

        #[test]
        fn open_without_the_grant_is_permission_denied() {
            if objc2_core_graphics::CGPreflightScreenCaptureAccess() {
                eprintln!("this process has the Screen Recording grant: the denial path is not reachable here");
                return;
            }
            let mut s = screen();
            let err = s.open(DisplayId(0), CaptureConfig { max_fps: 30 }).unwrap_err();
            assert_eq!(err.kind, ErrorKind::PermissionDenied(OsGrant::ScreenRecording), "{err}");
            assert!(s.next_frame(Duration::from_millis(10)).is_err());
        }

        #[test]
        fn open_an_unknown_display_is_invalid_input() {
            if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
                return;
            }
            let err = screen().open(DisplayId(99), CaptureConfig { max_fps: 30 }).unwrap_err();
            assert_eq!(err.kind, ErrorKind::InvalidInput, "{err}");
        }

        #[test]
        #[ignore = "needs Screen Recording: captures the main display for about a second"]
        fn captures_thirty_frames_at_thirty_fps() {
            use objc2_core_foundation::CFRetained;
            use objc2_core_video::{
                CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            };
            let mut s = screen();
            let list = s.displays().unwrap();
            let d = list.iter().find(|d| d.primary).unwrap();
            s.open(d.id, CaptureConfig { max_fps: 30 }).unwrap();
            let mut got = 0;
            let start = Instant::now();
            while got < 30 && start.elapsed() < Duration::from_secs(20) {
                if let Some(f) = s.next_frame(Duration::from_millis(500)).unwrap() {
                    assert_eq!((f.width, f.height), (d.bounds.width, d.bounds.height));
                    let Pixels::Gpu(g) = &f.pixels else { panic!("expected a GPU surface") };
                    let buf = g.0.downcast_ref::<CFRetained<CVPixelBuffer>>().expect("CFRetained<CVPixelBuffer>");
                    assert_eq!(CVPixelBufferGetPixelFormatType(buf), kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
                    assert_eq!(CVPixelBufferGetWidth(buf) as u32, f.width);
                    assert_eq!(CVPixelBufferGetHeight(buf) as u32, f.height);
                    got += 1;
                }
                let _ = s.take_cursor();
            }
            s.close();
            assert_eq!(got, 30);
        }

        #[test]
        #[ignore = "needs Screen Recording: opens a stream to read the pointer in display pixels"]
        fn cursor_reports_position_and_a_shape() {
            let mut s = screen();
            let list = s.displays().unwrap();
            let d = list.iter().find(|d| d.primary).unwrap();
            s.open(d.id, CaptureConfig { max_fps: 30 }).unwrap();
            let first = s.take_cursor().expect("first call reports the pointer");
            let shape = first.shape.expect("first call carries a shape");
            assert!(shape.width > 0 && shape.height > 0 && shape.width <= 256 && shape.height <= 256);
            assert_eq!(shape.rgba.len(), usize::from(shape.width) * usize::from(shape.height) * 4);
            assert!(shape.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0), "fully transparent cursor");
            assert!(s.take_cursor().is_none(), "unchanged pointer reports nothing");
            s.close();
        }

        #[test]
        fn no_system_cursor_means_no_shape_and_no_fallback() {
            // Over SSH there is no window server session and `currentSystemCursor` is nil: `None`, never a panic and
            // never the process-wide arrow cursor. With a session the shape must be a sane bitmap.
            if let Some(shape) = system_cursor_shape(2.0) {
                assert_eq!(shape.rgba.len(), usize::from(shape.width) * usize::from(shape.height) * 4);
            }
        }

        #[test]
        #[ignore = "needs a window server session (logged-in GUI session; SSH has none): NSCursor is nil without it"]
        fn system_cursor_renders_to_rgba() {
            #[allow(deprecated)]
            let Some(cursor) = objc2_app_kit::NSCursor::currentSystemCursor() else {
                eprintln!("no current system cursor: nothing to render");
                return;
            };
            let shape = render_cursor(&cursor, 2.0).expect("system cursor renders");
            assert_eq!(shape.rgba.len(), usize::from(shape.width) * usize::from(shape.height) * 4);
            assert!(shape.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255), "no opaque pixel");
            assert!(shape.hot_x < shape.width && shape.hot_y < shape.height);
            assert_eq!(shape.id, shape_id(shape.width, shape.height, &shape.rgba));
        }
    }
}
