//! `ScreenSource` over DXGI Desktop Duplication (contract C3, C2.1; windows.md 1 and 2).
//!
//! One `DxgiScreen` captures one display at a time on the thread that created it (thread affinity: `SetThreadDesktop`
//! binds the thread, the duplication and its device live on it). Frames are GPU copies of the duplicated surface in a
//! pooled BGRA texture on the device of the adapter that owns the output; a CPU readback is opt-in.

use crate::clock::ticks_to_mono;
use crate::desktop::InputDesktop;
use crate::enumerate::DpiScope;
use crate::enumerate::{self, OutputEntry};
use crate::errors::{self, DXGI_ERROR_WAIT_TIMEOUT, Phase};
use crate::geometry::{self, DisplayChange};
use crate::gpu::{DevicePool, GpuFrame, TexturePool};
use crate::hdr::{self, ScrgbToSrgb};
use crate::pointer;
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::sync::Arc;
use std::thread::ThreadId;
use std::time::{Duration, Instant};
use swapdesk_platform::{
    CaptureConfig, CursorShape, CursorUpdate, DisplayId, DisplayInfo, ErrorKind, Frame, GpuSurface, MonoMicros, Pixels,
    PlatformError, Rect, Result, ScreenSource,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

/// Options fixed when the source is built.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureOptions {
    /// Deliver `Pixels::Cpu` (a tight BGRA copy) instead of `Pixels::Gpu`. For tests and fallbacks.
    pub cpu_readback: bool,
}

/// Counters since the source was built, for measurements and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    pub frames: u64,
    /// Desktop updates the system merged into delivered frames (`AccumulatedFrames - 1` each).
    pub coalesced: u64,
    /// `next_frame` calls that waited out the timeout.
    pub timeouts: u64,
    /// Wake-ups that carried only a pointer change.
    pub cursor_only: u64,
    pub access_lost: u64,
    pub reacquires: u64,
    pub textures_created: u64,
    /// Frames that came as FP16 scRGB (an Advanced Color display) and were converted to sRGB at the SDR white level.
    pub converted: u64,
}

struct Open {
    dup: IDXGIOutputDuplication,
    textures: Arc<TexturePool>,
    luid: u64,
    /// The GDI name of the output (`\.\DISPLAY5`), for its SDR white level.
    gdi_name: String,
    /// The SDR white level in scRGB units and when it was read; re-read every [`WHITE_REFRESH`] while FP16 frames come.
    white: Cell<(f32, Option<Instant>)>,
    /// Made at the first FP16 frame.
    converter: RefCell<Option<ScrgbToSrgb>>,
}

/// How often the SDR white level is read again (the user can move the slider during a session).
const WHITE_REFRESH: Duration = Duration::from_secs(2);

/// Duplicates `output`, asking for FP16 too (`DuplicateOutput1`), so an Advanced Color desktop arrives with its full
/// range instead of clamped to 8 bits; the legacy `DuplicateOutput` when the newer call is missing or refused.
fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device) -> windows::core::Result<IDXGIOutputDuplication> {
    if let Ok(o5) = output.cast::<IDXGIOutput5>() {
        let _dpi = DpiScope::enter();
        // SAFETY: a device made on this output's adapter; the format list lives across the call.
        if let Ok(d) = unsafe { o5.DuplicateOutput1(device, 0, &[hdr::FP16, DXGI_FORMAT_B8G8R8A8_UNORM]) } {
            return Ok(d);
        }
    }
    // SAFETY: as above, the legacy call.
    unsafe { output.DuplicateOutput(device) }
}

#[derive(Default)]
struct Cursor {
    /// Last top-left of the pointer image, in pixels of the display.
    x: i32,
    y: i32,
    visible: bool,
    shape_serial: u32,
    /// The shape in use, re-offered after a reacquire.
    shape: Option<CursorShape>,
    pending_shape: Option<CursorShape>,
    pending: bool,
}

pub struct DxgiScreen {
    owner: ThreadId,
    pool: Arc<DevicePool>,
    options: CaptureOptions,
    // Field order matters on drop: the duplication goes before the thread leaves its desktop.
    open: Option<Open>,
    target: Option<(DisplayId, CaptureConfig)>,
    lost: bool,
    desktop: InputDesktop,
    known_displays: Vec<DisplayInfo>,
    cursor: Cursor,
    last_captured: u64,
    /// When the next frame may be delivered under `max_fps`; advances by one interval per frame, so processing time
    /// does not stretch the period.
    next_due: Option<Instant>,
    moves: Vec<DXGI_OUTDUPL_MOVE_RECT>,
    dirty: Vec<RECT>,
    shape_buf: Vec<u8>,
    stats: CaptureStats,
    _not_send: PhantomData<*const ()>,
}

fn thread_error(op: &'static str) -> PlatformError {
    errors::error(ErrorKind::InvalidInput, op, None, "called from a thread other than the one that created the source")
}

fn state_error(op: &'static str, detail: &str) -> PlatformError {
    errors::error(ErrorKind::InvalidInput, op, None, detail)
}

/// Releases an acquired frame on every exit path.
struct Held<'a>(&'a IDXGIOutputDuplication);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        // SAFETY: a frame was acquired on this duplication and not yet released.
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

impl DxgiScreen {
    /// A source with its own device pool and the default options (GPU frames).
    pub fn new() -> Self {
        Self::with_options(DevicePool::new(), CaptureOptions::default())
    }

    /// A source over a shared device pool (one device per adapter for every display of the process).
    pub fn with_options(pool: Arc<DevicePool>, options: CaptureOptions) -> Self {
        Self {
            owner: std::thread::current().id(),
            pool,
            options,
            open: None,
            target: None,
            lost: false,
            desktop: InputDesktop::new(),
            known_displays: Vec::new(),
            cursor: Cursor::default(),
            last_captured: 0,
            next_due: None,
            moves: Vec::new(),
            dirty: Vec::new(),
            shape_buf: Vec::new(),
            stats: CaptureStats::default(),
            _not_send: PhantomData,
        }
    }

    pub fn stats(&self) -> CaptureStats {
        let mut s = self.stats;
        if let Some(o) = &self.open {
            s.textures_created = o.textures.created() as u64;
        }
        s
    }

    pub fn device_pool(&self) -> &Arc<DevicePool> {
        &self.pool
    }

    /// The device and adapter LUID of the display being captured.
    pub fn luid(&self) -> Option<u64> {
        self.open.as_ref().map(|o| o.luid)
    }

    /// Re-enumerates and says what differs from the list the last `displays` or `poll_display_changes` saw. Call it
    /// on a `WatchEvent::DisplayChange`, or every second or so as a safety net.
    pub fn poll_display_changes(&mut self) -> Result<Vec<DisplayChange>> {
        let new = enumerate::displays()?;
        let changes = geometry::diff_displays(&self.known_displays, &new);
        self.known_displays = new;
        Ok(changes)
    }

    /// Drops the duplication exactly as a real `DXGI_ERROR_ACCESS_LOST` does: the next `next_frame` fails with
    /// `AccessLost` until `reacquire`. For tests; a real desktop switch needs SYSTEM to follow onto the secure desktop.
    pub fn simulate_access_lost(&mut self) {
        self.open = None;
        self.lost = true;
    }

    fn check_thread(&self, op: &'static str) -> Result<()> {
        if std::thread::current().id() == self.owner { Ok(()) } else { Err(thread_error(op)) }
    }

    fn find(entries: Vec<OutputEntry>, id: DisplayId) -> Option<OutputEntry> {
        entries.into_iter().find(|e| e.info.id == id)
    }

    /// Resolves the display on a fresh enumeration, takes the device of its adapter and duplicates the output.
    fn build(&mut self, id: DisplayId) -> Result<Open> {
        let entries = enumerate::enumerate()?;
        self.known_displays = entries.iter().map(|e| e.info.clone()).collect();
        let entry =
            Self::find(entries, id).ok_or_else(|| state_error("open", &format!("display {} is not attached", id.0)))?;
        let luid = entry.key.adapter_luid;
        let gpu = self.pool.device_for(luid, &entry.adapter)?;
        // SAFETY: the device was created on the adapter that enumerated this output (windows.md 2a).
        let dup = match duplicate(&entry.output, &gpu.device) {
            Ok(d) => d,
            Err(e) => {
                if !gpu.is_healthy() {
                    self.pool.invalidate(luid);
                }
                return Err(errors::from_hresult(
                    Phase::Duplicate,
                    "DuplicateOutput",
                    e.code().0,
                    &format!("display {} on {}", id.0, entry.adapter_name),
                ));
            }
        };
        // SAFETY: plain query on a live duplication.
        let desc = unsafe { dup.GetDesc() };
        let (w, h) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        if w == 0 || h == 0 {
            return Err(state_error("DuplicateOutput", "the duplication reports an empty mode"));
        }
        // SAFETY: plain query on a live output.
        let gdi_name = unsafe { entry.output.GetDesc() }
            .map(|d| {
                let end = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(d.DeviceName.len());
                String::from_utf16_lossy(&d.DeviceName[..end])
            })
            .unwrap_or_default();
        Ok(Open {
            dup,
            textures: TexturePool::new(gpu, w, h),
            luid,
            gdi_name,
            white: Cell::new((1.0, None)),
            converter: RefCell::new(None),
        })
    }

    fn ms_ceil(d: Duration) -> u32 {
        u32::try_from(d.as_micros().div_ceil(1000)).unwrap_or(u32::MAX)
    }

    fn update_pointer(&mut self, open: &Open, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            self.cursor.x = info.PointerPosition.Position.x;
            self.cursor.y = info.PointerPosition.Position.y;
            self.cursor.visible = info.PointerPosition.Visible.as_bool();
            self.cursor.pending = true;
        }
        if info.PointerShapeBufferSize == 0 {
            return;
        }
        let need = info.PointerShapeBufferSize as usize;
        if self.shape_buf.len() < need {
            self.shape_buf.resize(need, 0);
        }
        let mut required = 0u32;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: the buffer holds at least `PointerShapeBufferSize` bytes; the out pointers are valid.
        let got = unsafe {
            open.dup.GetFramePointerShape(need as u32, self.shape_buf.as_mut_ptr().cast(), &mut required, &mut shape)
        };
        if got.is_err() {
            return;
        }
        let used = (required as usize).min(self.shape_buf.len());
        if let Some(c) =
            pointer::convert_shape(shape.Type, shape.Width, shape.Height, shape.Pitch, &self.shape_buf[..used])
        {
            self.cursor.shape_serial = self.cursor.shape_serial.wrapping_add(1);
            let hot_x = u16::try_from(shape.HotSpot.x.max(0)).unwrap_or(0).min(c.width.saturating_sub(1));
            let hot_y = u16::try_from(shape.HotSpot.y.max(0)).unwrap_or(0).min(c.height.saturating_sub(1));
            let s = CursorShape {
                id: self.cursor.shape_serial,
                width: c.width,
                height: c.height,
                hot_x,
                hot_y,
                rgba: c.rgba,
            };
            self.cursor.shape = Some(s.clone());
            self.cursor.pending_shape = Some(s);
            self.cursor.pending = true;
        }
    }

    /// Dirty rectangles of the held frame (dirty plus move destinations), clamped to the frame. Empty when the system
    /// gave no metadata: treat as the whole frame.
    fn read_dirty(&mut self, open: &Open, info: &DXGI_OUTDUPL_FRAME_INFO, width: u32, height: u32) -> Vec<Rect> {
        let total = info.TotalMetadataBufferSize as usize;
        if total == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let move_cap = total / size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1;
        let rect_cap = total / size_of::<RECT>() + 1;
        self.moves.resize(move_cap, DXGI_OUTDUPL_MOVE_RECT::default());
        self.dirty.resize(rect_cap, RECT::default());
        let mut need = 0u32;
        // SAFETY: the buffers hold at least `total` bytes; `need` is a valid out pointer.
        if unsafe {
            open.dup.GetFrameMoveRects(
                (move_cap * size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32,
                self.moves.as_mut_ptr(),
                &mut need,
            )
        }
        .is_ok()
        {
            let n = need as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>();
            out.extend(self.moves[..n.min(move_cap)].iter().map(|m| m.DestinationRect));
        }
        need = 0;
        // SAFETY: as above.
        if unsafe {
            open.dup.GetFrameDirtyRects((rect_cap * size_of::<RECT>()) as u32, self.dirty.as_mut_ptr(), &mut need)
        }
        .is_ok()
        {
            let n = need as usize / size_of::<RECT>();
            out.extend_from_slice(&self.dirty[..n.min(rect_cap)]);
        }
        out.iter()
            .filter_map(|r| {
                let (l, t) = (r.left.clamp(0, width as i32), r.top.clamp(0, height as i32));
                let (rr, b) = (r.right.clamp(0, width as i32), r.bottom.clamp(0, height as i32));
                (rr > l && b > t).then(|| Rect { x: l, y: t, width: (rr - l) as u32, height: (b - t) as u32 })
            })
            .collect()
    }
}

impl Default for DxgiScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenSource for DxgiScreen {
    fn displays(&mut self) -> Result<Vec<DisplayInfo>> {
        self.check_thread("displays")?;
        let list = enumerate::displays()?;
        self.known_displays = list.clone();
        Ok(list)
    }

    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()> {
        self.check_thread("open")?;
        self.open = None;
        self.lost = false;
        let mut desktop_note = String::new();
        if let Err(e) = self.desktop.follow() {
            desktop_note = format!("; follow input desktop: {e}");
        }
        match self.build(display) {
            Ok(o) => {
                self.open = Some(o);
                self.target = Some((display, config));
                self.next_due = None;
                self.cursor.pending_shape = self.cursor.shape.clone();
                self.cursor.pending = self.cursor.pending_shape.is_some();
                Ok(())
            }
            Err(mut e) => {
                e.detail.push_str(&desktop_note);
                Err(e)
            }
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        self.check_thread("next_frame")?;
        if self.lost {
            return Err(errors::error(ErrorKind::AccessLost, "next_frame", None, "call reacquire"));
        }
        let (display_id, config) = self.target.ok_or_else(|| state_error("next_frame", "not open"))?;
        let started = Instant::now();
        // Pace at the acquire: the system keeps accumulating changes while we wait, so the next frame is the newest.
        if config.max_fps > 0
            && let Some(due) = self.next_due
        {
            let now = Instant::now();
            if due > now {
                let wait = due - now;
                if wait >= timeout {
                    std::thread::sleep(timeout);
                    self.stats.timeouts += 1;
                    return Ok(None);
                }
                std::thread::sleep(wait);
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let Some(open) = self.open.take() else {
            return Err(errors::error(ErrorKind::AccessLost, "next_frame", None, "no duplication; call reacquire"));
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: valid out pointers; a successful acquire is paired with `ReleaseFrame` through `Held`.
        let acquired = unsafe { open.dup.AcquireNextFrame(Self::ms_ceil(remaining), &mut info, &mut resource) };
        if let Err(e) = acquired {
            if e.code().0 == DXGI_ERROR_WAIT_TIMEOUT {
                self.stats.timeouts += 1;
                self.open = Some(open);
                return Ok(None);
            }
            let err = errors::from_hresult(
                Phase::Frame,
                "AcquireNextFrame",
                e.code().0,
                &format!("display {}", display_id.0),
            );
            if err.kind == ErrorKind::AccessLost {
                // Release the duplication now, as the documentation requires, and wait for `reacquire`.
                self.stats.access_lost += 1;
                self.lost = true;
                if !open_device_ok(&open) {
                    self.pool.invalidate(open.luid);
                }
                return Err(err);
            }
            self.open = Some(open);
            return Err(err);
        }
        let held = Held(&open.dup);
        self.update_pointer(&open, &info);
        if info.LastPresentTime == 0 {
            // Only the pointer changed: the caller reads it with `take_cursor`.
            self.stats.cursor_only += 1;
            drop(held);
            self.open = Some(open);
            return Ok(None);
        }
        let (width, height) = (open.textures.width, open.textures.height);
        let dirty = self.read_dirty(&open, &info, width, height);
        let Some(resource) = resource else {
            drop(held);
            self.open = Some(open);
            return Err(errors::error(ErrorKind::Os, "AcquireNextFrame", None, "no desktop resource"));
        };
        let result = (|| -> Result<Frame> {
            let src: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| errors::from_hresult(Phase::Frame, "IDXGIResource::cast", e.code().0, &e.message()))?;
            let target = open.textures.take()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: GetDesc only fills the struct.
            unsafe { src.GetDesc(&mut desc) };
            if desc.Format == hdr::FP16 {
                convert_fp16(&open, &src, target.texture(), (width, height))?;
                self.stats.converted += 1;
            } else {
                // SAFETY: both textures belong to the device of this adapter and share size and format.
                unsafe { open.textures.gpu.context.CopyResource(target.texture(), &src) };
            }
            let ticks = u64::try_from(info.LastPresentTime).unwrap_or(0);
            let captured = ticks_to_mono(ticks).0.max(self.last_captured);
            self.last_captured = captured;
            let pixels = if self.options.cpu_readback {
                let img = target.read_back()?;
                Pixels::Cpu { stride: img.stride, data: img.data }
            } else {
                Pixels::Gpu(GpuSurface(Box::new(target)))
            };
            Ok(Frame {
                display: display_id,
                captured_at: MonoMicros(captured),
                width,
                height,
                pixels,
                dirty,
                content_changed: true,
            })
        })();
        drop(resource);
        drop(held);
        self.open = Some(open);
        match result {
            Ok(frame) => {
                self.stats.frames += 1;
                self.stats.coalesced += u64::from(info.AccumulatedFrames.saturating_sub(1));
                if config.max_fps > 0 {
                    let interval = Duration::from_micros(1_000_000 / u64::from(config.max_fps));
                    let now = Instant::now();
                    let mut due = self.next_due.unwrap_or(now) + interval;
                    if due + interval < now {
                        due = now + interval;
                    }
                    self.next_due = Some(due);
                }
                Ok(Some(frame))
            }
            Err(e) => Err(e),
        }
    }

    fn reacquire(&mut self) -> Result<()> {
        self.check_thread("reacquire")?;
        let (display, config) = self.target.ok_or_else(|| state_error("reacquire", "never opened"))?;
        self.open = None;
        self.lost = true;
        self.stats.reacquires += 1;
        let mut desktop_note = String::new();
        if let Err(e) = self.desktop.follow() {
            desktop_note = format!("; follow input desktop: {e}");
        }
        match self.build(display) {
            Ok(o) => {
                self.open = Some(o);
                self.lost = false;
                self.target = Some((display, config));
                self.next_due = None;
                self.cursor.pending_shape = self.cursor.shape.clone();
                self.cursor.pending = self.cursor.pending_shape.is_some();
                Ok(())
            }
            Err(mut e) => {
                e.detail.push_str(&desktop_note);
                Err(e)
            }
        }
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        if !self.cursor.pending {
            return None;
        }
        let display = self.target.map(|t| t.0)?;
        self.cursor.pending = false;
        // The duplication reports the top-left of the pointer image; the C3 position (as the viewer's pointer
        // coordinates, PROTOCOL.md 7.3) is the hot spot, so the viewer draws the shape at `x - hot_x`.
        let (hx, hy) = self.cursor.shape.as_ref().map_or((0, 0), |s| (i32::from(s.hot_x), i32::from(s.hot_y)));
        Some(CursorUpdate {
            display,
            x: self.cursor.x + hx,
            y: self.cursor.y + hy,
            visible: self.cursor.visible,
            shape: self.cursor.pending_shape.take(),
        })
    }

    fn close(&mut self) {
        self.open = None;
        self.target = None;
        self.lost = false;
        self.cursor.pending = false;
        self.cursor.pending_shape = None;
    }
}

/// An FP16 scRGB frame into the BGRA frame texture, at the output's SDR white level.
fn convert_fp16(open: &Open, src: &ID3D11Texture2D, dst: &ID3D11Texture2D, size: (u32, u32)) -> Result<()> {
    let (mut white, read_at) = open.white.get();
    if read_at.is_none_or(|t| t.elapsed() >= WHITE_REFRESH) {
        white = hdr::sdr_white(&open.gdi_name).unwrap_or(white);
        open.white.set((white, Some(Instant::now())));
    }
    let mut converter = open.converter.borrow_mut();
    if converter.is_none() {
        *converter = Some(ScrgbToSrgb::new(&open.textures.gpu.device)?);
    }
    let gpu = &open.textures.gpu;
    converter.as_mut().expect("made above").convert(&gpu.context, src, dst, size, white)
}

fn open_device_ok(open: &Open) -> bool {
    open.textures.gpu.is_healthy()
}

/// The GPU payload of a delivered frame, when it carries one (`Pixels::Gpu` made by this crate).
pub fn gpu_payload(frame: &Frame) -> Option<&GpuFrame> {
    match &frame.pixels {
        Pixels::Gpu(surface) => surface.0.downcast_ref::<GpuFrame>(),
        Pixels::Cpu { .. } => None,
    }
}
