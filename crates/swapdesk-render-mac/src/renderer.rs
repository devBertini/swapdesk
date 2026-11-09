//! `Renderer` for macOS: decoded pictures enqueued on an `AVSampleBufferDisplayLayer` (through its
//! `sampleBufferRenderer`, macOS 14+) that sits in a container layer under the shell's toolbar.
//!
//! Layer tree this renderer adds to the shell's parent layer, at index 0 (below everything the shell has there):
//!
//! ```text
//! parent_layer                (the shell's; the toolbar layer is a sibling above the container)
//!  `- container               black, clips to the video area, y grows downwards inside it (flipped unless the parent is)
//!      |- video               AVSampleBufferDisplayLayer, videoGravity "resize", frame = where the picture lands
//!      `- cursor              the remote cursor, a layer of its own above the video
//! ```
//!
//! Scaling is geometry, never pixels: `Fit`, 1:1 and scroll all enqueue the same picture and only change the frame of
//! the video layer inside the clipping container (see [`crate::layout`]). All layer property writes are made inside a
//! `CATransaction` with actions disabled, so nothing animates.
//!
//! Threads: layer-tree edits (`attach`, `resize`, `set_scale`, `set_cursor`, `detach`, and a `present` that moves the
//! picture) belong to the main thread, like every other CALayer edit under AppKit. The enqueue itself is documented as
//! thread-safe through `sampleBufferRenderer`, but this crate is only exercised from one thread.
//!
//! Sources read on 2026-11-08 (Apple documentation and the `objc2-*` 0.3.2 docs on docs.rs):
//! - AVSampleBufferDisplayLayer and its `sampleBufferRenderer` (macOS 14+; the layer's own enqueue is deprecated):
//!   <https://developer.apple.com/documentation/avfoundation/avsamplebufferdisplaylayer> and
//!   <https://docs.rs/objc2-av-foundation/0.3.2/objc2_av_foundation/struct.AVSampleBufferDisplayLayer.html>.
//! - AVSampleBufferVideoRenderer and the queued-rendering protocol (`enqueueSampleBuffer:`, `flush`,
//!   `isReadyForMoreMediaData`, `status`, `requiresFlushToResumeDecoding`):
//!   <https://docs.rs/objc2-av-foundation/0.3.2/objc2_av_foundation/struct.AVSampleBufferVideoRenderer.html> and
//!   <https://docs.rs/objc2-av-foundation/0.3.2/objc2_av_foundation/trait.AVQueuedSampleBufferRendering.html>.
//! - `kCMSampleAttachmentKey_DisplayImmediately` (set on the sample's attachment dictionary), pixel buffers must be
//!   IOSurface-backed: the `enqueueSampleBuffer:` notes in the same AVSampleBufferDisplayLayer page, and
//!   <https://docs.rs/objc2-core-media/0.3.2/objc2_core_media/static.kCMSampleAttachmentKey_DisplayImmediately.html>.
//! - CMSampleBuffer from an image buffer, `CMVideoFormatDescriptionCreateForImageBuffer` and `...MatchesImageBuffer`:
//!   <https://docs.rs/objc2-core-media/0.3.2/objc2_core_media/struct.CMSampleBuffer.html>.
//! - CALayer, CATransaction (`setDisableActions`), `geometryFlipped`, `contentsScale`:
//!   <https://docs.rs/objc2-quartz-core/0.3.2/objc2_quartz_core/struct.CALayer.html> and
//!   <https://developer.apple.com/documentation/quartzcore/catransaction>.
//! - CGImage from raw RGBA for the cursor (`CGImageCreate`, `CGDataProviderCreateWithCFData`):
//!   <https://docs.rs/objc2-core-graphics/0.3.2/objc2_core_graphics/struct.CGImage.html>.
//! - `mach_continuous_time`: <https://developer.apple.com/documentation/kernel/1646199-mach_continuous_time>.

use std::ptr::{self, NonNull};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::sel;
use objc2_av_foundation::{
    AVLayerVideoGravityResize, AVQueuedSampleBufferRendering, AVQueuedSampleBufferRenderingStatus,
    AVSampleBufferDisplayLayer, AVSampleBufferVideoRenderer,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFMutableDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint,
    CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapInfo, CGColor, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_core_media::{
    CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreateForImageBuffer, CMVideoFormatDescriptionMatchesImageBuffer,
    kCMSampleAttachmentKey_DisplayImmediately,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
    kCVReturnSuccess,
};
use objc2_quartz_core::{CALayer, CATransaction};
use swapdesk_platform::{
    CursorShape, CursorUpdate, DecodedFrame, DisplayId, ErrorKind, NativeSurface, Pixels, PresentStats, Rect, Renderer,
    Result, ScaleMode,
};

use crate::layout::{self, Layout};
use crate::sys::{err, now, status_err};

/// How a [`MacRenderer`] is made.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RendererOptions {
    /// Physical pixels per point of the window's screen (2.0 on a Retina display). `None` (the default): read the
    /// parent layer's `contentsScale` at `attach`. The shell calls [`MacRenderer::set_backing_scale`] when the window
    /// moves to a screen with another scale.
    pub backing_scale: Option<f64>,
}

/// Pictures enqueued since `attach`, and how often the layer had to be flushed (a picture is never dropped for
/// readiness: the newest one always goes in, see `present`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    pub presented: u64,
    /// Times the layer's renderer had failed and was flushed before the picture was enqueued.
    pub recovered: u64,
    /// Times the layer was not ready for more and its backlog was flushed to make room for the newest picture.
    pub flushed: u64,
}

struct Shown {
    size: (u32, u32),
    display: DisplayId,
    layout: Layout,
}

struct Cursor {
    layer: Retained<CALayer>,
    size: (u32, u32),
    hot: (u16, u16),
    shape_id: Option<u32>,
    has_shape: bool,
    /// Last update: pixel inside the display, visibility, display.
    at: Option<(i32, i32, bool, DisplayId)>,
    visible: bool,
}

struct Attached {
    parent: Retained<CALayer>,
    container: Retained<CALayer>,
    video: Retained<AVSampleBufferDisplayLayer>,
    sink: Retained<AVSampleBufferVideoRenderer>,
    cursor: Cursor,
    area: Rect,
    backing: f64,
    mode: ScaleMode,
    shown: Option<Shown>,
    format: Option<CFRetained<CMVideoFormatDescription>>,
    counters: Counters,
}

/// The macOS renderer. Created and used on the shell's thread (C3: nothing here is `Send`).
pub struct MacRenderer {
    options: RendererOptions,
    att: Option<Attached>,
}

impl Default for MacRenderer {
    fn default() -> Self {
        Self::new(RendererOptions::default())
    }
}

fn closed() -> swapdesk_platform::PlatformError {
    err(ErrorKind::Closed, "Renderer", "not attached")
}

/// Runs `f` with implicit animations off and commits.
fn transaction<T>(f: impl FnOnce() -> T) -> T {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    let out = f();
    CATransaction::commit();
    out
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn borrow_layer(raw: u64) -> Result<Retained<CALayer>> {
    // SAFETY: the shell owns the layer for the renderer's lifetime (C3 `NativeSurface`); `retain` adds our own
    // reference, and a null address is refused by `NonNull`... through `retain`'s null check.
    match unsafe { Retained::retain(raw as usize as *mut CALayer) } {
        Some(layer) => Ok(layer),
        None => Err(err(ErrorKind::InvalidInput, "attach", "NativeSurface parent_layer is null")),
    }
}

fn time(value: i64, timescale: i32) -> CMTime {
    CMTime { value, timescale, flags: CMTimeFlags::Valid, epoch: 0 }
}

fn invalid_time() -> CMTime {
    CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 }
}

/// Pixel buffer attributes that make a buffer IOSurface-backed, as the display layer needs.
fn surface_attributes() -> CFRetained<CFDictionary> {
    let number = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
    let surface_props = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: the statics are the framework's own, valid for the process lifetime.
    let (format_key, surface_key) = unsafe { (kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey) };
    let dict = CFDictionary::<CFString, CFType>::from_slices(
        &[format_key, surface_key],
        &[number.as_ref(), surface_props.as_ref()],
    );
    // SAFETY: a CFDictionary whatever its key and value types say.
    unsafe { CFRetained::cast_unchecked::<CFDictionary>(dict) }
}

/// A CPU BGRA frame as an IOSurface-backed pixel buffer.
fn upload(stride: usize, data: &[u8], w: u32, h: u32) -> Result<CFRetained<CVImageBuffer>> {
    let row = w as usize * 4;
    if w == 0 || h == 0 || stride < row || data.len() < stride * h as usize {
        return Err(err(ErrorKind::InvalidInput, "present", "CPU frame smaller than its width, height and stride"));
    }
    let attrs = surface_attributes();
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: valid attributes and out pointer; on success the buffer carries one reference (the Create rule).
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            w as usize,
            h as usize,
            kCVPixelFormatType_32BGRA,
            Some(&attrs),
            NonNull::from(&mut out),
        )
    };
    if status != kCVReturnSuccess {
        return Err(status_err("CVPixelBufferCreate", status));
    }
    let ptr = NonNull::new(out).ok_or_else(|| err(ErrorKind::Os, "CVPixelBufferCreate", "no pixel buffer"))?;
    // SAFETY: Create rule.
    let buffer = unsafe { CFRetained::from_raw(ptr) };
    // SAFETY: the base address is only written between a successful lock and the unlock below.
    let locked = unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
    if locked != kCVReturnSuccess {
        return Err(status_err("CVPixelBufferLockBaseAddress", locked));
    }
    let dst_stride = CVPixelBufferGetBytesPerRow(&buffer);
    let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
    let result = if base.is_null() || dst_stride < row {
        Err(err(ErrorKind::Os, "present", "pixel buffer has no writable BGRA rows"))
    } else {
        for y in 0..h as usize {
            // SAFETY: the locked buffer holds `h` rows of `dst_stride >= row` bytes; the source slice holds `h` rows of
            // `stride >= row` bytes (checked above); the two do not overlap.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr().add(y * stride), base.add(y * dst_stride), row) };
        }
        Ok(())
    };
    // SAFETY: balances the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
    result.map(|()| buffer)
}

/// The remote cursor as a `CGImage` (straight RGBA in, premultiplied on the image). Shapes are at most 256x256
/// (PROTOCOL.md 7.4).
fn cursor_image(shape: &CursorShape) -> Result<CFRetained<CGImage>> {
    let (w, h) = (usize::from(shape.width), usize::from(shape.height));
    if w == 0 || h == 0 || w > 256 || h > 256 || shape.rgba.len() != w * h * 4 {
        return Err(err(ErrorKind::InvalidInput, "set_cursor", "cursor shape size does not match its pixels"));
    }
    let mut premultiplied = Vec::with_capacity(shape.rgba.len());
    for px in shape.rgba.as_chunks::<4>().0 {
        let a = u32::from(px[3]);
        let mul = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
        premultiplied.extend_from_slice(&[mul(px[0]), mul(px[1]), mul(px[2]), px[3]]);
    }
    let data = CFData::from_bytes(&premultiplied);
    let provider = CGDataProvider::with_cf_data(Some(&data))
        .ok_or_else(|| err(ErrorKind::Os, "CGDataProviderCreateWithCFData", "no provider"))?;
    let space = CGColorSpace::new_device_rgb()
        .ok_or_else(|| err(ErrorKind::Os, "CGColorSpaceCreateDeviceRGB", "no color space"))?;
    // SAFETY: 8 bits per component, 32 per pixel, `w * 4` bytes per row, premultiplied RGBA: exactly the layout of the
    // data behind the provider; no decode array.
    let image = unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::PremultipliedLast.0),
            Some(&provider),
            ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    };
    image.ok_or_else(|| err(ErrorKind::Os, "CGImageCreate", "no image"))
}

impl Cursor {
    fn new(container: &CALayer) -> Self {
        let layer = CALayer::new();
        layer.setHidden(true);
        container.addSublayer(&layer);
        Self { layer, size: (0, 0), hot: (0, 0), shape_id: None, has_shape: false, at: None, visible: false }
    }

    fn set_shape(&mut self, shape: &CursorShape, backing: f64) -> Result<()> {
        if self.shape_id == Some(shape.id) && self.has_shape {
            self.hot = (shape.hot_x, shape.hot_y);
            return Ok(());
        }
        let image = cursor_image(shape)?;
        self.layer.setContentsScale(backing);
        // SAFETY: a CGImage is a valid `contents` value of a layer (Core Animation accepts CGImageRef, which is
        // toll-free bridged as an object); the layer retains it.
        unsafe { self.layer.setContents(Some(&*CFRetained::as_ptr(&image).as_ptr().cast::<AnyObject>())) };
        self.size = (u32::from(shape.width), u32::from(shape.height));
        self.hot = (shape.hot_x, shape.hot_y);
        self.shape_id = Some(shape.id);
        self.has_shape = true;
        Ok(())
    }

    /// Puts the cursor's hot spot at `spot` (area-relative pixels), or hides it.
    fn place(&mut self, spot: Option<(f32, f32)>, backing: f64) {
        match spot {
            Some((x, y)) if self.has_shape => {
                let left = (f64::from(x) - f64::from(self.hot.0)).round();
                let top = (f64::from(y) - f64::from(self.hot.1)).round();
                self.layer.setFrame(rect(
                    left / backing,
                    top / backing,
                    f64::from(self.size.0) / backing,
                    f64::from(self.size.1) / backing,
                ));
                self.layer.setHidden(false);
                self.visible = true;
            }
            _ => {
                self.layer.setHidden(true);
                self.visible = false;
            }
        }
    }
}

impl Attached {
    fn pixel(&self, v: u32) -> f64 {
        f64::from(v) / self.backing
    }

    /// Frame of the container in the parent layer, from the area in window pixels (top-left origin).
    fn container_frame(&self) -> CGRect {
        let a = self.area;
        let (w, h) = (a.width.max(1), a.height.max(1));
        let x = f64::from(a.x) / self.backing;
        let y = if self.parent.isGeometryFlipped() {
            f64::from(a.y) / self.backing
        } else {
            self.parent.bounds().size.height - (f64::from(a.y) + f64::from(h)) / self.backing
        };
        rect(x, y, self.pixel(w), self.pixel(h))
    }

    fn area_size(&self) -> (u32, u32) {
        (self.area.width.max(1), self.area.height.max(1))
    }

    /// Container, video layer and cursor in their places for the current area, backing scale and last picture.
    fn apply_geometry(&mut self) {
        let container = self.container_frame();
        let video = match &self.shown {
            Some(s) => {
                // The whole picture is drawn at its destination scale and the container clips to the area: Fit, 1:1
                // and scroll are the same operation with different numbers.
                let l = s.layout;
                let kx = f64::from(l.dest.w) / f64::from(l.src.w.max(1));
                let ky = f64::from(l.dest.h) / f64::from(l.src.h.max(1));
                let x = f64::from(l.dest.x) - f64::from(l.src.x) * kx;
                let y = f64::from(l.dest.y) - f64::from(l.src.y) * ky;
                rect(
                    x / self.backing,
                    y / self.backing,
                    f64::from(s.size.0) * kx / self.backing,
                    f64::from(s.size.1) * ky / self.backing,
                )
            }
            None => rect(0.0, 0.0, self.pixel(self.area_size().0), self.pixel(self.area_size().1)),
        };
        transaction(|| {
            self.container.setFrame(container);
            self.video.setFrame(video);
            self.place_cursor_inner();
        });
    }

    fn place_cursor_inner(&mut self) {
        let spot = match (&self.shown, self.cursor.at) {
            (Some(s), Some((x, y, true, display))) if display == s.display => s.layout.dest_point(x, y),
            _ => None,
        };
        self.cursor.place(spot, self.backing);
    }

    fn relayout(&mut self) {
        if let Some(s) = self.shown.as_mut()
            && let Some(l) = Layout::compute(s.size, (self.area.width.max(1), self.area.height.max(1)), self.mode)
        {
            s.layout = l;
        }
        self.apply_geometry();
    }

    fn image_of(&self, frame: &DecodedFrame) -> Result<CFRetained<CVImageBuffer>> {
        match &frame.pixels {
            Pixels::Gpu(surface) => surface
                .0
                .downcast_ref::<CFRetained<CVPixelBuffer>>()
                .cloned()
                .ok_or_else(|| err(ErrorKind::Unsupported, "present", "the GPU surface is not a CVPixelBuffer")),
            Pixels::Cpu { stride, data } => upload(*stride as usize, data, frame.width, frame.height),
        }
    }

    /// A ready sample buffer around the picture, marked to be displayed at once.
    fn sample_of(&mut self, image: &CVImageBuffer) -> Result<CFRetained<CMSampleBuffer>> {
        // SAFETY: `desc` and `image` are live.
        let reuse =
            self.format.as_ref().is_some_and(|desc| unsafe { CMVideoFormatDescriptionMatchesImageBuffer(desc, image) });
        if !reuse {
            let mut out: *const CMVideoFormatDescription = ptr::null();
            // SAFETY: valid image and out pointer; on success the description carries one reference (Create rule).
            let status = unsafe { CMVideoFormatDescriptionCreateForImageBuffer(None, image, NonNull::from(&mut out)) };
            if status != 0 {
                return Err(status_err("CMVideoFormatDescriptionCreateForImageBuffer", status));
            }
            let ptr = NonNull::new(out.cast_mut())
                .ok_or_else(|| err(ErrorKind::Os, "CMVideoFormatDescriptionCreateForImageBuffer", "no description"))?;
            // SAFETY: Create rule.
            self.format = Some(unsafe { CFRetained::from_raw(ptr) });
        }
        let Some(format) = self.format.as_ref() else {
            return Err(err(ErrorKind::Os, "present", "no format description"));
        };
        let mut timing = CMSampleTimingInfo {
            duration: invalid_time(),
            presentationTimeStamp: time(now().0 as i64, 1_000_000),
            decodeTimeStamp: invalid_time(),
        };
        let mut out: *mut CMSampleBuffer = ptr::null_mut();
        // SAFETY: live image and description, one timing entry that outlives the call, valid out pointer; the sample
        // buffer retains the image and the description.
        let status = unsafe {
            CMSampleBuffer::create_ready_with_image_buffer(
                None,
                image,
                format,
                NonNull::from(&mut timing),
                NonNull::from(&mut out),
            )
        };
        if status != 0 {
            return Err(status_err("CMSampleBufferCreateReadyWithImageBuffer", status));
        }
        let ptr = NonNull::new(out)
            .ok_or_else(|| err(ErrorKind::Os, "CMSampleBufferCreateReadyWithImageBuffer", "no sample buffer"))?;
        // SAFETY: Create rule.
        let sample = unsafe { CFRetained::from_raw(ptr) };
        // SAFETY: the attachments array holds one mutable dictionary per sample (one here); the reference stays valid
        // while `sample` lives, and nothing else mutates it meanwhile.
        unsafe {
            if let Some(array) = sample.sample_attachments_array(true) {
                let array = CFRetained::cast_unchecked::<CFArray<CFMutableDictionary<CFString, CFBoolean>>>(array);
                if !array.is_empty() {
                    array.get_unchecked(0).set(kCMSampleAttachmentKey_DisplayImmediately, CFBoolean::new(true));
                }
            }
        }
        Ok(sample)
    }

    fn draw(&mut self, image: CFRetained<CVImageBuffer>, display: DisplayId, size: (u32, u32)) -> Result<PresentStats> {
        let layout = Layout::compute(size, self.area_size(), self.mode)
            .ok_or_else(|| err(ErrorKind::InvalidInput, "present", "empty picture or area"))?;
        let moved = self.shown.as_ref().is_none_or(|s| s.layout != layout || s.size != size || s.display != display);
        if moved {
            self.shown = Some(Shown { size, display, layout });
            self.apply_geometry();
        }
        let sample = self.sample_of(&image)?;
        // SAFETY: plain property reads and a flush on the layer's own renderer.
        unsafe {
            if self.sink.status() == AVQueuedSampleBufferRenderingStatus::Failed
                || self.sink.requiresFlushToResumeDecoding()
            {
                // The layer stopped taking pictures (decoder resources reclaimed, system sleep): a flush resets it. A
                // decoded picture needs no keyframe, so the next one shows at once.
                self.sink.flush();
                self.counters.recovered += 1;
            }
            if !self.sink.isReadyForMoreMediaData() {
                // The queue holds pictures nobody has shown yet (window hidden, covered or not on screen, a burst): the
                // newest picture is the one that matters, so the backlog goes (a flush keeps the shown image) and the new
                // one is queued right behind it. The flush completes asynchronously, so the layer may still report "not
                // ready" for a moment; Apple documents `enqueueSampleBuffer:` as safe in that state, and the queue
                // stays at a picture or two because every such call flushes first.
                self.sink.flush();
                self.counters.flushed += 1;
            }
            self.sink.enqueueSampleBuffer(&sample);
        }
        let presented_at = now();
        self.counters.presented += 1;
        Ok(PresentStats { presented_at })
    }
}

impl MacRenderer {
    pub fn new(options: RendererOptions) -> Self {
        Self { options, att: None }
    }

    /// Draws the last picture again under `scale` (the user changed Fit, 1:1 or the scroll position; a static host sends
    /// no new picture). `present` also takes the scale with every picture.
    pub fn set_scale(&mut self, scale: ScaleMode) -> Result<()> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        a.mode = scale;
        a.relayout();
        Ok(())
    }

    /// The window moved to a screen with another pixels-per-point factor.
    pub fn set_backing_scale(&mut self, pixels_per_point: f64) -> Result<()> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        if !(pixels_per_point.is_finite() && pixels_per_point > 0.0) {
            return Err(err(
                ErrorKind::InvalidInput,
                "set_backing_scale",
                format!("{pixels_per_point} is not a scale factor"),
            ));
        }
        a.backing = pixels_per_point;
        a.relayout();
        Ok(())
    }

    /// The layout of the last picture, in area-relative pixels (the shell draws scroll bars from it).
    pub fn layout(&self) -> Option<Layout> {
        self.att.as_ref().and_then(|a| a.shown.as_ref().map(|s| s.layout))
    }

    pub fn counters(&self) -> Counters {
        self.att.as_ref().map(|a| a.counters).unwrap_or_default()
    }

    /// The container layer that was added to the shell's parent layer (tests and the shell's own bookkeeping).
    pub fn container_layer(&self) -> Option<&CALayer> {
        self.att.as_ref().map(|a| &*a.container)
    }

    /// The `AVSampleBufferDisplayLayer` that shows the video.
    pub fn video_layer(&self) -> Option<&AVSampleBufferDisplayLayer> {
        self.att.as_ref().map(|a| &*a.video)
    }

    /// Whether the cursor layer is currently shown.
    pub fn cursor_visible(&self) -> bool {
        self.att.as_ref().is_some_and(|a| a.cursor.visible)
    }
}

impl Renderer for MacRenderer {
    fn attach(&mut self, surface: NativeSurface, area: Rect) -> Result<()> {
        if self.att.is_some() {
            self.detach();
        }
        let NativeSurface::MacOs { parent_layer } = surface else {
            return Err(err(ErrorKind::Unsupported, "attach", "not a macOS NativeSurface"));
        };
        let parent = borrow_layer(parent_layer)?;
        // SAFETY: plain construction of a layer; the checks below run before it is used.
        let video = unsafe { AVSampleBufferDisplayLayer::new() };
        if !video.respondsToSelector(sel!(sampleBufferRenderer)) {
            return Err(err(
                ErrorKind::Unsupported,
                "attach",
                "AVSampleBufferDisplayLayer.sampleBufferRenderer needs macOS 14",
            ));
        }
        // SAFETY: the property exists (checked above); the returned renderer is retained.
        let sink = unsafe { video.sampleBufferRenderer() };
        let scale = self.options.backing_scale.unwrap_or_else(|| {
            let s = parent.contentsScale();
            if s.is_finite() && s > 0.0 { s } else { 1.0 }
        });
        let container = CALayer::new();
        let cursor = transaction(|| {
            // `geometryFlipped` is relative to the superlayer: a flipped parent (a flipped NSView's layer) already
            // counts y downwards, and flipping the container again would make the cursor run upside down.
            container.setGeometryFlipped(!parent.isGeometryFlipped());
            container.setMasksToBounds(true);
            container.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 1.0)));
            // SAFETY: both statics are the framework's own; the gravity string is a valid constant.
            unsafe {
                if let Some(gravity) = AVLayerVideoGravityResize {
                    video.setVideoGravity(gravity);
                }
            }
            video.setContentsScale(scale);
            container.addSublayer(&video);
            let cursor = Cursor::new(&container);
            parent.insertSublayer_atIndex(&container, 0);
            cursor
        });
        let mut att = Attached {
            parent,
            container,
            video,
            sink,
            cursor,
            area,
            backing: scale,
            mode: ScaleMode::Fit,
            shown: None,
            format: None,
            counters: Counters::default(),
        };
        att.apply_geometry();
        self.att = Some(att);
        Ok(())
    }

    fn resize(&mut self, area: Rect) -> Result<()> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        a.area = area;
        // The display layer keeps showing its last picture; only the geometry changes.
        a.relayout();
        Ok(())
    }

    fn present(&mut self, frame: DecodedFrame, scale: ScaleMode) -> Result<PresentStats> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        a.mode = scale;
        let image = a.image_of(&frame)?;
        a.draw(image, frame.display, (frame.width, frame.height))
    }

    fn set_cursor(&mut self, cursor: &CursorUpdate) -> Result<()> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        if let Some(shape) = &cursor.shape {
            let backing = a.backing;
            a.cursor.set_shape(shape, backing)?;
        }
        a.cursor.at = Some((cursor.x, cursor.y, cursor.visible && a.cursor.has_shape, cursor.display));
        transaction(|| a.place_cursor_inner());
        Ok(())
    }

    fn map_pointer(&self, x: i32, y: i32) -> Option<(u16, u16)> {
        let a = self.att.as_ref()?;
        let s = a.shown.as_ref()?;
        layout::map_pointer(a.area, s.size, &s.layout, x, y)
    }

    fn detach(&mut self) {
        if let Some(a) = self.att.take() {
            transaction(|| {
                // SAFETY: a flush on the layer's own renderer.
                unsafe { a.sink.flush() };
                a.container.removeFromSuperlayer();
            });
        }
    }
}

impl Drop for MacRenderer {
    fn drop(&mut self) {
        self.detach();
    }
}
