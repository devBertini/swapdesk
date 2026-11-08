//! The stage 15 harness: a window and composition tree that stand in for the shell, and the pipeline
//! `SyntheticScreen frames -> codec-win encoder -> codec-win decoder -> WinRenderer` measured at the presented frame.
//!
//! Threads, as in C6: the thread that calls [`run`] is the UI thread (it owns the window, pumps its messages and samples
//! the process counters); the viewer thread decodes and renders from the `NativeSurface` pointers only; the encoder runs
//! in a thread of this process or in a child process (`b1b2 --encode-child`, so the viewer's memory is its own).
//!
//! B1: `captured_at` is read back from the barcode of the presented frame (a copy of the swap chain buffer taken just
//! before `Present`) and compared with the QPC time right after `Present` returned. B2: presented frames per 1 s window
//! and counters that never arrived.
#![cfg(windows)]
#![allow(dead_code)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use swapdesk_codec_win::{
    DecoderOptions, DecoderOutput, EncoderOptions, MfH264Decoder, MfH264Encoder, MftChoice, TextureUploader,
};
use swapdesk_fakes::{BarcodeReader, FakeClock, SyntheticScreen, draw_barcode};
use swapdesk_platform::{
    AdapterId, CaptureConfig, DecodedFrame, DisplayId, EncodedFrame, EncoderConfig, Frame, MonoMicros, NativeSurface,
    Pixels, Rect, Renderer, ScaleMode, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{Codec, Preset, VideoHeader};
use swapdesk_render_win::{RendererOptions, WinRenderer, now};
use windows::Win32::Foundation::{FILETIME, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    DXGI_GPU_PREFERENCE_MINIMUM_POWER, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
    DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter1, IDXGIAdapter3, IDXGIDevice, IDXGIFactory1, IDXGIFactory6,
};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentThread, GetProcessTimes, GetThreadTimes, INFINITE, ReleaseMutex,
    WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};

// ---------------------------------------------------------------------------------------------------------------------
// The shell stand-in
// ---------------------------------------------------------------------------------------------------------------------

/// Which adapter the shell's device runs on, chosen the way `swapdesk-shell-win`'s `Gfx::new` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pref {
    /// `DXGI_GPU_PREFERENCE_MINIMUM_POWER`: the integrated GPU on this PC (the shell's default).
    Min,
    /// `DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE`: the NVIDIA GPU (`SWAPDESK_ADAPTER=high`).
    High,
}

impl Pref {
    fn dxgi(self) -> DXGI_GPU_PREFERENCE {
        match self {
            Pref::Min => DXGI_GPU_PREFERENCE_MINIMUM_POWER,
            Pref::High => DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Pref::Min => "integrated (minimum power)",
            Pref::High => "NVIDIA (high performance)",
        }
    }
}

pub fn adapter_by_pref(pref: Pref) -> IDXGIAdapter1 {
    // SAFETY: plain factory and enumeration calls.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).expect("DXGI factory");
        factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(0, pref.dxgi()).expect("adapter by preference")
    }
}

pub fn luid_of(a: &IDXGIAdapter1) -> AdapterId {
    // SAFETY: GetDesc1 only fills a struct.
    let d = unsafe { a.GetDesc1() }.expect("adapter desc");
    AdapterId(u64::from(d.AdapterLuid.HighPart as u32) << 32 | u64::from(d.AdapterLuid.LowPart))
}

pub fn name_of(a: &IDXGIAdapter1) -> String {
    // SAFETY: GetDesc1 only fills a struct.
    let d = unsafe { a.GetDesc1() }.expect("adapter desc");
    String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string()
}

struct Monitors(Vec<(String, RECT)>);

unsafe extern "system" fn collect_monitor(hmon: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> windows::core::BOOL {
    // SAFETY: `lp` is the `Monitors` that `monitors` passed; the info struct is local.
    unsafe {
        let list = &mut *(lp.0 as *mut Monitors);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, (&raw mut info).cast::<MONITORINFO>()).as_bool() {
            let name = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').to_string();
            list.0.push((name, info.monitorInfo.rcMonitor));
        }
    }
    true.into()
}

/// The monitors of the virtual desktop: (`\.\DISPLAYn`, bounds in physical pixels).
pub fn monitors() -> Vec<(String, RECT)> {
    let mut list = Monitors(Vec::new());
    // SAFETY: the callback only runs during the call, with `list` alive.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect_monitor), LPARAM((&raw mut list) as isize));
    }
    list.0
}

fn monitor_origin(n: u32) -> Option<(i32, i32)> {
    let want = format!(r"\\.\DISPLAY{n}");
    monitors().into_iter().find(|(name, _)| *name == want).map(|(_, r)| (r.left, r.top))
}

/// `\.\DISPLAYn` of the monitor the window is on, and its refresh rate in Hz.
pub fn window_monitor(hwnd: HWND) -> (String, u32) {
    // SAFETY: plain queries with local out structs.
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        let _ = GetMonitorInfoW(hmon, (&raw mut info).cast::<MONITORINFO>());
        let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
        let _ = EnumDisplaySettingsW(windows::core::PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode);
        (String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').to_string(), mode.dmDisplayFrequency)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: the default procedure for every message.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Held while a test uses the screen and the GPU for a window: the capture crate's tests (desktop duplication of the real
/// screen, a topmost flicker window) take the same named mutex, so the two kinds never overlap in one `nextest` run.
struct Exclusive(windows::Win32::Foundation::HANDLE);

impl Exclusive {
    fn acquire() -> Exclusive {
        // SAFETY: a named mutex shared by the test processes; waited on with no timeout, released in `drop`.
        unsafe {
            let h = CreateMutexW(None, false, w!("Local\\swapdesk-capture-win-tests")).expect("mutex");
            WaitForSingleObject(h, INFINITE);
            Exclusive(h)
        }
    }
}

impl Drop for Exclusive {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex (it waited on it).
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// A window with the shell's composition tree: root visual, an empty video parent visual, and the D3D11 device and
/// DirectComposition device the shell would hand to the renderer.
pub struct ShellStub {
    // Last field to drop: the window and devices go first, then the mutex is released.
    pub hwnd: HWND,
    pub d3d: ID3D11Device,
    pub dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _root: IDCompositionVisual,
    parent: IDCompositionVisual,
    pub adapter_name: String,
    pub luid: AdapterId,
    _exclusive: Exclusive,
}

impl ShellStub {
    /// `client`: the client area in physical pixels. Per-monitor DPI v2, like the shell.
    pub fn new(pref: Pref, client: (u32, u32), display: Option<u32>) -> windows::core::Result<ShellStub> {
        let exclusive = Exclusive::acquire();
        // SAFETY: window class and window creation, and composition objects on live devices, all on this thread.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: w!("SwapdeskRenderHarness"),
                ..Default::default()
            };
            RegisterClassExW(&class); // a second registration in one process fails harmlessly
            let ex = WS_EX_NOREDIRECTIONBITMAP;
            let origin = display.and_then(monitor_origin).map_or((80, 40), |(x, y)| (x + 80, y + 40));
            let hwnd = CreateWindowExW(
                ex,
                w!("SwapdeskRenderHarness"),
                w!("Swapdesk render harness"),
                WS_OVERLAPPEDWINDOW,
                origin.0,
                origin.1,
                800,
                600,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            let dpi = GetDpiForWindow(hwnd);
            let mut frame = RECT { left: 0, top: 0, right: client.0 as i32, bottom: client.1 as i32 };
            AdjustWindowRectExForDpi(&mut frame, WS_OVERLAPPEDWINDOW, false, ex, dpi)?;
            SetWindowPos(
                hwnd,
                None,
                origin.0,
                origin.1,
                frame.right - frame.left,
                frame.bottom - frame.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

            // The device exactly as `Gfx::new` makes it: adapter by preference, BGRA support only.
            let adapter = adapter_by_pref(pref);
            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let d3d = device.ok_or_else(windows::core::Error::empty)?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let target = dcomp.CreateTargetForHwnd(hwnd, false)?;
            let root = dcomp.CreateVisual()?;
            let parent = dcomp.CreateVisual()?;
            root.AddVisual(&parent, false, None::<&IDCompositionVisual>)?;
            target.SetRoot(&root)?;
            dcomp.Commit()?;
            let stub = ShellStub {
                hwnd,
                d3d,
                dcomp,
                _target: target,
                _root: root,
                parent,
                adapter_name: name_of(&adapter),
                luid: luid_of(&adapter),
                _exclusive: exclusive,
            };
            stub.pump();
            Ok(stub)
        }
    }

    /// The raw pointers the renderer attaches with (`swapdesk_shell_win::native_surface` hands out the same three).
    pub fn surface(&self) -> NativeSurface {
        NativeSurface::Windows {
            dcomp_device: self.dcomp.as_raw() as usize as u64,
            parent_visual: self.parent.as_raw() as usize as u64,
            d3d11_device: self.d3d.as_raw() as usize as u64,
        }
    }

    pub fn client_size(&self) -> (u32, u32) {
        let mut r = RECT::default();
        // SAFETY: fills a RECT for our window.
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
        }
        ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
    }

    pub fn dpi(&self) -> u32 {
        // SAFETY: a getter for our window.
        unsafe { GetDpiForWindow(self.hwnd) }
    }

    /// Handles every pending window message.
    pub fn pump(&self) {
        let mut msg = MSG::default();
        // SAFETY: the standard non-blocking message pump of our thread.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for ShellStub {
    fn drop(&mut self) {
        // SAFETY: destroys our window once.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Process counters
// ---------------------------------------------------------------------------------------------------------------------

pub fn private_working_set_mb() -> f64 {
    let mut c =
        PROCESS_MEMORY_COUNTERS_EX2 { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32, ..Default::default() };
    // SAFETY: fills the counters of this process; `cb` tells the size of the extended struct.
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb);
    }
    c.PrivateWorkingSetSize as f64 / (1024.0 * 1024.0)
}

/// What this process uses on each hardware adapter: (name, dedicated MB, shared MB).
pub fn gpu_memory_mb() -> Vec<(String, f64, f64)> {
    let mut out = Vec::new();
    // SAFETY: plain enumeration and queries.
    unsafe {
        let Ok(factory) = windows::Win32::Graphics::Dxgi::CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        for i in 0.. {
            let Ok(a) = factory.EnumAdapters1(i) else { break };
            let Ok(d) = a.GetDesc1() else { continue };
            if d.Flags & windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let Ok(a3) = a.cast::<IDXGIAdapter3>() else { continue };
            let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            let mut non_local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            let _ = a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local);
            let _ = a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut non_local);
            out.push((name_of(&a), local.CurrentUsage as f64 / 1048576.0, non_local.CurrentUsage as f64 / 1048576.0));
        }
    }
    out
}

fn secs(f: FILETIME) -> f64 {
    (u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)) as f64 * 100e-9
}

pub fn process_cpu_seconds() -> f64 {
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: fills four FILETIMEs for this process.
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    secs(k) + secs(u)
}

pub fn thread_cpu_seconds() -> f64 {
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: fills four FILETIMEs for this thread.
    unsafe {
        let _ = GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u);
    }
    secs(k) + secs(u)
}

// ---------------------------------------------------------------------------------------------------------------------
// The encoder side
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EncodeSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub kbps: u32,
    pub seconds: f64,
    /// LUID of the adapter that encodes (Quick Sync on this PC).
    pub adapter: u64,
    /// Distinct pre-rendered frames the synthetic screen cycles through.
    pub pool: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Produced {
    /// Frames captured (each with its own barcode counter).
    pub captured: u32,
    /// Encoded frames sent on.
    pub encoded: u32,
}

/// The synthetic screen at `fps` on the wall clock: `pool` frames come from `SyntheticScreen` (moving gradient, frame
/// number, barcode), and each emission stamps its barcode with the counter and the QPC time of the emission, as the
/// screen does for every frame it makes. Each frame goes to the GPU, through the encoder, and to `sink`.
pub fn produce(s: &EncodeSettings, mut sink: impl FnMut(EncodedFrame) -> bool) -> Result<Produced, String> {
    let mut screen = SyntheticScreen::new(s.width, s.height, s.fps).with_clock(Arc::new(FakeClock::new(MonoMicros(0))));
    screen.open(DisplayId(0), CaptureConfig { max_fps: s.fps }).map_err(|e| e.to_string())?;
    let mut pool: Vec<Vec<u8>> = Vec::new();
    for _ in 0..s.pool.max(1) {
        let f = screen.next_frame(Duration::from_secs(5)).map_err(|e| e.to_string())?.ok_or("no frame")?;
        let Pixels::Cpu { data, .. } = f.pixels else { return Err("synthetic frames are CPU frames".into()) };
        pool.push(data);
    }
    let stride = s.width * 4;
    let uploader = TextureUploader::new(AdapterId(s.adapter)).map_err(|e| e.to_string())?;
    let mut enc = MfH264Encoder::new(EncoderOptions {
        choice: MftChoice::Auto,
        device: Some(uploader.device()),
        ..Default::default()
    });
    let cfg = EncoderConfig {
        width: s.width,
        height: s.height,
        fps: s.fps,
        kbps: s.kbps,
        preset: Preset::Auto,
        adapter: AdapterId(s.adapter),
    };
    enc.configure(&cfg).map_err(|e| e.to_string())?;
    let total = (s.seconds * f64::from(s.fps)).round() as u32;
    let start = Instant::now() + Duration::from_millis(200);
    let mut produced = Produced::default();
    for n in 0..total {
        // Pace to the frame's slot: sleep to just before it, spin the rest.
        let due = start + Duration::from_nanos((u128::from(n) * 1_000_000_000 / u128::from(s.fps)) as u64);
        loop {
            let left = due.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if left > Duration::from_micros(1500) {
                std::thread::sleep(left - Duration::from_micros(1000));
            } else {
                std::hint::spin_loop();
            }
        }
        let captured_at = now();
        let idx = n as usize % pool.len();
        draw_barcode(&mut pool[idx], stride, s.width, s.height, n, captured_at);
        let frame = Frame {
            display: DisplayId(0),
            captured_at,
            width: s.width,
            height: s.height,
            pixels: Pixels::Cpu { stride, data: std::mem::take(&mut pool[idx]) },
            dirty: Vec::new(),
            content_changed: true,
        };
        let gpu = uploader.upload(&frame).map_err(|e| e.to_string())?;
        let Pixels::Cpu { data, .. } = frame.pixels else { unreachable!("made above") };
        pool[idx] = data;
        produced.captured += 1;
        for e in enc.encode(&gpu, false).map_err(|e| e.to_string())? {
            produced.encoded += 1;
            if !sink(e) {
                return Ok(produced);
            }
        }
    }
    for e in enc.flush().map_err(|e| e.to_string())? {
        produced.encoded += 1;
        if !sink(e) {
            break;
        }
    }
    Ok(produced)
}

/// One encoded frame on a pipe: captured_at u64, encode_us u32, keyframe u8, config u8, width u16, height u16, len u32.
pub fn write_frame(w: &mut impl Write, e: &EncodedFrame) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(22);
    head.extend_from_slice(&e.captured_at.0.to_le_bytes());
    head.extend_from_slice(&e.encode_us.to_le_bytes());
    head.push(u8::from(e.keyframe));
    head.push(u8::from(e.config_change));
    head.extend_from_slice(&e.width.to_le_bytes());
    head.extend_from_slice(&e.height.to_le_bytes());
    head.extend_from_slice(&(e.annex_b.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(&e.annex_b)
}

pub fn read_frame(r: &mut impl Read) -> std::io::Result<Option<EncodedFrame>> {
    let mut head = [0u8; 22];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(head[18..22].try_into().expect("4 bytes")) as usize;
    let mut annex_b = vec![0u8; len];
    r.read_exact(&mut annex_b)?;
    Ok(Some(EncodedFrame {
        captured_at: MonoMicros(u64::from_le_bytes(head[0..8].try_into().expect("8 bytes"))),
        encode_us: u32::from_le_bytes(head[8..12].try_into().expect("4 bytes")),
        keyframe: head[12] != 0,
        config_change: head[13] != 0,
        width: u16::from_le_bytes(head[14..16].try_into().expect("2 bytes")),
        height: u16::from_le_bytes(head[16..18].try_into().expect("2 bytes")),
        annex_b,
    }))
}

// ---------------------------------------------------------------------------------------------------------------------
// The viewer side
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub counter: u32,
    pub captured_at: u64,
    pub received_at: u64,
    pub presented_at: u64,
    pub encode_us: u32,
    pub decode_us: u32,
    pub wait_us: u32,
    pub blt_us: u32,
    pub present_us: u32,
    /// Waiting for the GPU to finish the probe copy (the measurement's own cost).
    pub probe_us: u32,
}

#[derive(Debug, Default)]
pub struct Viewed {
    pub samples: Vec<Sample>,
    pub unreadable: u32,
    pub decode_errors: u32,
    pub dropped_by_renderer: u32,
    pub render_errors: Vec<String>,
    pub no_picture: u32,
    pub viewer_cpu_s: f64,
    pub decoder_name: String,
    pub mem_after_first_frame_mb: f64,
}

/// Where the renderer's device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOn {
    /// The shell's device (zero copy: the decoder shares it).
    Shell,
    /// A device of its own on this adapter, which the decoder shares instead.
    Adapter(Pref),
}

fn viewer(
    surface: NativeSurface,
    area: Rect,
    render_on: RenderOn,
    buffers: u32,
    rx: Receiver<EncodedFrame>,
    ready: SyncSender<()>,
) -> Result<Viewed, String> {
    let options = match render_on {
        RenderOn::Shell => RendererOptions { buffers, wait: Duration::from_millis(200), ..Default::default() },
        RenderOn::Adapter(p) => {
            RendererOptions { adapter: Some(luid_of(&adapter_by_pref(p))), buffers, wait: Duration::from_millis(200) }
        }
    };
    let mut renderer = WinRenderer::new(options);
    renderer.attach(surface, area).map_err(|e| e.to_string())?;
    let mut decoder = MfH264Decoder::new(DecoderOptions {
        output: DecoderOutput::Gpu,
        adapter: AdapterId(0),
        device: renderer.device(),
        dxva: true,
    });
    renderer.set_probe(Some((0, 0, 512, 64)));
    let reader = BarcodeReader::new();
    let mut v = Viewed::default();
    let cpu0 = thread_cpu_seconds();
    let mut frame_id = 0u32;
    let mut signalled = false;
    loop {
        let e = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let received_at = now().0;
        let header = VideoHeader {
            keyframe: e.keyframe,
            config: e.config_change,
            discontinuity: false,
            display_id: 0,
            frame_id,
            capture_us: e.captured_at.0,
            encode_us: e.encode_us,
            width: e.width,
            height: e.height,
            codec: Codec::H264,
            payload_len: e.annex_b.len() as u32,
        };
        frame_id += 1;
        let decoded: Option<DecodedFrame> = match decoder.decode_settled(&header, &e.annex_b) {
            Ok(d) => d,
            Err(err) => {
                v.decode_errors += 1;
                if v.render_errors.len() < 5 {
                    v.render_errors.push(format!("decode: {err}"));
                }
                continue;
            }
        };
        let Some(frame) = decoded else {
            v.no_picture += 1;
            continue;
        };
        let decode_us = decoder.last_decode_us();
        match renderer.present(frame, ScaleMode::Fit) {
            Ok(stats) => {
                let t = renderer.timing();
                let tp = std::time::Instant::now();
                let probe = renderer.take_probe().map_err(|e| e.to_string())?;
                let probe_us = tp.elapsed().as_micros() as u32;
                let Some(reading) = probe.and_then(|p| reader.read_bgra(&p.data, p.stride, p.width, p.height)) else {
                    v.unreadable += 1;
                    continue;
                };
                if !signalled {
                    v.mem_after_first_frame_mb = private_working_set_mb();
                    let _ = ready.send(());
                    signalled = true;
                }
                v.samples.push(Sample {
                    counter: reading.counter,
                    captured_at: reading.captured_at.0,
                    received_at,
                    presented_at: stats.presented_at.0,
                    encode_us: e.encode_us,
                    decode_us,
                    wait_us: t.wait_us,
                    blt_us: t.blt_us,
                    present_us: t.present_us,
                    probe_us,
                });
            }
            Err(err) if err.kind == swapdesk_platform::ErrorKind::Timeout => v.dropped_by_renderer += 1,
            Err(err) => {
                if v.render_errors.len() < 5 {
                    v.render_errors.push(format!("present: {err}"));
                }
            }
        }
    }
    v.viewer_cpu_s = thread_cpu_seconds() - cpu0;
    v.decoder_name = decoder.mft_name().map(|(n, dxva)| format!("{n} (DXVA {dxva})")).unwrap_or_default();
    renderer.detach();
    Ok(v)
}

// ---------------------------------------------------------------------------------------------------------------------
// A run and its report
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub seconds: f64,
    pub fps: u16,
    pub width: u32,
    pub height: u32,
    pub kbps: u32,
    pub shell: Pref,
    pub render_on: RenderOn,
    /// Run the encoder in a child process of this executable (`--encode-child`), so memory is the viewer's alone.
    pub encoder_child: bool,
    pub pool: usize,
    /// Put the window on `\.\DISPLAYn` (None: the primary monitor).
    pub display: Option<u32>,
    pub buffers: u32,
}

impl Config {
    pub fn b1b2(seconds: f64) -> Config {
        Config {
            seconds,
            fps: 60,
            width: 1920,
            height: 1080,
            kbps: 20_000,
            shell: Pref::Min,
            render_on: RenderOn::Shell,
            encoder_child: false,
            pool: 4,
            display: None,
            buffers: 2,
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub shell_adapter: String,
    pub render_adapter: String,
    pub dpi: u32,
    pub monitor: (String, u32),
    pub window_client: (u32, u32),
    pub produced: Produced,
    pub viewed: Viewed,
    pub mem_baseline_mb: f64,
    pub mem_samples_mb: Vec<f64>,
    pub gpu_mem_mb: Vec<(String, f64, f64)>,
    pub process_cpu_percent_of_core: f64,
    pub wall_s: f64,
    pub encoder_child: bool,
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v
}

impl Report {
    /// Capture-to-present latencies in ms, from the barcode's `captured_at` to the QPC time after `Present`.
    pub fn latencies_ms(&self) -> Vec<f64> {
        sorted(
            self.viewed.samples.iter().map(|s| s.presented_at.saturating_sub(s.captured_at) as f64 / 1000.0).collect(),
        )
    }

    /// Counters that were produced but never presented (barcode read back and unique).
    pub fn lost(&self) -> u32 {
        let mut seen = std::collections::HashSet::new();
        for s in &self.viewed.samples {
            seen.insert(s.counter);
        }
        self.produced.captured.saturating_sub(seen.len() as u32)
    }

    pub fn lost_percent(&self) -> f64 {
        f64::from(self.lost()) / f64::from(self.produced.captured.max(1)) * 100.0
    }

    /// Presented frames per 1 s window, full windows only (the first and last partial ones are dropped).
    pub fn fps_windows(&self) -> Vec<f64> {
        let Some(first) = self.viewed.samples.first().map(|s| s.presented_at) else { return Vec::new() };
        let last = self.viewed.samples.last().map_or(first, |s| s.presented_at);
        let n = ((last - first) / 1_000_000) as usize;
        let mut buckets = vec![0u32; n + 1];
        for s in &self.viewed.samples {
            buckets[((s.presented_at - first) / 1_000_000) as usize] += 1;
        }
        buckets.pop();
        buckets.into_iter().map(f64::from).collect()
    }

    fn stage(&self, f: impl Fn(&Sample) -> f64) -> (f64, f64) {
        let v = sorted(self.viewed.samples.iter().map(f).collect());
        (percentile(&v, 0.5), percentile(&v, 0.95))
    }

    pub fn text(&self) -> String {
        let lat = self.latencies_ms();
        let fps = sorted(self.fps_windows());
        let (enc_m, enc_p) = self.stage(|s| f64::from(s.encode_us) / 1000.0);
        let (net_m, net_p) = self.stage(|s| s.received_at.saturating_sub(s.captured_at) as f64 / 1000.0);
        let (dec_m, dec_p) = self.stage(|s| f64::from(s.decode_us) / 1000.0);
        let (wait_m, wait_p) = self.stage(|s| f64::from(s.wait_us) / 1000.0);
        let (blt_m, blt_p) = self.stage(|s| f64::from(s.blt_us) / 1000.0);
        let (pre_m, pre_p) = self.stage(|s| f64::from(s.present_us) / 1000.0);
        let (rest_m, rest_p) = self.stage(|s| s.presented_at.saturating_sub(s.received_at) as f64 / 1000.0);
        let (probe_m, probe_p) = self.stage(|s| f64::from(s.probe_us) / 1000.0);
        let mut mem = self.mem_samples_mb.clone();
        mem.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        let mut out = String::new();
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "shell device: {} | renderer device: {} | window client {}x{} at {} dpi on {} ({} Hz)",
            self.shell_adapter,
            self.render_adapter,
            self.window_client.0,
            self.window_client.1,
            self.dpi,
            self.monitor.0,
            self.monitor.1
        );
        let _ = writeln!(out, "decoder: {}", self.viewed.decoder_name);
        let _ = writeln!(
            out,
            "run {:.1} s | captured {} | encoded {} | presented (barcode read) {} | lost {} ({:.3} %) | unreadable {} | decode errors {} | dropped by the renderer {} | decoder no-picture {}",
            self.wall_s,
            self.produced.captured,
            self.produced.encoded,
            self.viewed.samples.len(),
            self.lost(),
            self.lost_percent(),
            self.viewed.unreadable,
            self.viewed.decode_errors,
            self.viewed.dropped_by_renderer,
            self.viewed.no_picture
        );
        let _ = writeln!(
            out,
            "B1 capture-to-present ms: median {:.2} | p95 {:.2} | p99 {:.2} | max {:.2} | min {:.2}",
            percentile(&lat, 0.5),
            percentile(&lat, 0.95),
            percentile(&lat, 0.99),
            lat.last().copied().unwrap_or(f64::NAN),
            lat.first().copied().unwrap_or(f64::NAN)
        );
        let warm = sorted(
            self.viewed
                .samples
                .iter()
                .filter(|s| s.counter >= 120)
                .map(|s| s.presented_at.saturating_sub(s.captured_at) as f64 / 1000.0)
                .collect(),
        );
        let _ = writeln!(
            out,
            "B1 after the first 2 s: median {:.2} | p95 {:.2} | p99 {:.2} | max {:.2}",
            percentile(&warm, 0.5),
            percentile(&warm, 0.95),
            percentile(&warm, 0.99),
            warm.last().copied().unwrap_or(f64::NAN)
        );
        let _ = writeln!(
            out,
            "B2 presented fps per 1 s window: median {:.1} | min {:.0} | windows {} | windows under 59: {}",
            percentile(&fps, 0.5),
            fps.first().copied().unwrap_or(f64::NAN),
            fps.len(),
            fps.iter().filter(|f| **f < 59.0).count()
        );
        let _ = writeln!(
            out,
            "stages ms (median / p95): capture to viewer's receive {net_m:.2} / {net_p:.2} (encode alone {enc_m:.2} / {enc_p:.2}) | decode {dec_m:.2} / {dec_p:.2} | receive to present {rest_m:.2} / {rest_p:.2} = latency wait {wait_m:.2} / {wait_p:.2} + blit {blt_m:.2} / {blt_p:.2} + Present {pre_m:.2} / {pre_p:.2} (+ the rest in decode and the probe copy)"
        );
        let _ = writeln!(
            out,
            "measurement's own cost after Present, ms (median / p95): waiting for the probe copy {probe_m:.2} / {probe_p:.2}"
        );
        let _ = writeln!(
            out,
            "memory: private working set MB baseline (window, devices, renderer attached, no stream) {:.1} | after the first frame {:.1} | streaming median {:.1} max {:.1} (viewer process only: {})",
            self.mem_baseline_mb,
            self.viewed.mem_after_first_frame_mb,
            percentile(&mem, 0.5),
            mem.last().copied().unwrap_or(f64::NAN),
            if self.encoder_in_child() { "encoder in a child process" } else { "encoder in this process" }
        );
        let series: Vec<String> = self
            .mem_samples_mb
            .iter()
            .step_by((self.mem_samples_mb.len() / 12).max(1))
            .map(|m| format!("{m:.0}"))
            .collect();
        let _ = writeln!(
            out,
            "memory series MB (every {} s): {}",
            2 * (self.mem_samples_mb.len() / 12).max(1),
            series.join(" ")
        );
        for (name, local, shared) in &self.gpu_mem_mb {
            let _ =
                writeln!(out, "GPU memory of this process on {name}: dedicated {local:.1} MB, shared {shared:.1} MB");
        }
        let _ = writeln!(
            out,
            "CPU: viewer thread {:.1} % of one core | whole process {:.1} % of one core",
            self.viewed.viewer_cpu_s / self.wall_s.max(0.001) * 100.0,
            self.process_cpu_percent_of_core
        );
        out
    }

    fn encoder_in_child(&self) -> bool {
        self.encoder_child
    }
}

/// Runs the whole pipeline for `cfg.seconds` and reports. Call on the thread that should own the window (the UI thread).
pub fn run(cfg: &Config, encoder_child_exe: Option<&std::path::Path>) -> Result<Report, String> {
    // SAFETY: raises the timer resolution of the process for the pacing sleeps.
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
    let shell =
        ShellStub::new(cfg.shell, (cfg.width, cfg.height), cfg.display).map_err(|e| format!("shell stub: {e}"))?;
    let client = shell.client_size();
    let render_adapter = match cfg.render_on {
        RenderOn::Shell => shell.adapter_name.clone(),
        RenderOn::Adapter(p) => name_of(&adapter_by_pref(p)),
    };
    let area = Rect { x: 0, y: 0, width: client.0, height: client.1 };
    let settings = EncodeSettings {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        kbps: cfg.kbps,
        seconds: cfg.seconds,
        adapter: luid_of(&adapter_by_pref(Pref::Min)).0,
        pool: cfg.pool,
    };
    let (tx, rx) = mpsc::sync_channel::<EncodedFrame>(16);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<()>(1);
    let surface = shell.surface();
    let render_on = cfg.render_on;
    let buffers = cfg.buffers;
    // Baseline memory: the window, devices and tree exist; the renderer is attached on the viewer thread right now.
    let viewer_thread = std::thread::Builder::new()
        .name("viewer".into())
        .spawn(move || viewer(surface, area, render_on, buffers, rx, ready_tx))
        .map_err(|e| e.to_string())?;
    for _ in 0..40 {
        shell.pump();
        std::thread::sleep(Duration::from_millis(25));
    }
    let mem_baseline = private_working_set_mb();

    let cpu0 = process_cpu_seconds();
    let t0 = Instant::now();
    let produced: Arc<std::sync::Mutex<Option<Result<Produced, String>>>> = Arc::new(std::sync::Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let producer = {
        let (produced, done, settings) = (Arc::clone(&produced), Arc::clone(&done), settings.clone());
        let exe = encoder_child_exe.map(|p| p.to_path_buf());
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                let result = match exe {
                    None => produce(&settings, |e| tx.send(e).is_ok()),
                    Some(exe) => run_child(&exe, &settings, &tx),
                };
                *produced.lock().expect("lock") = Some(result);
                done.store(true, Ordering::SeqCst);
            })
            .map_err(|e| e.to_string())?
    };
    // The UI thread: pump the window, sample memory every 2 s once frames flow.
    let mut mem_samples = Vec::new();
    let mut next_sample = Instant::now();
    let mut streaming = false;
    while !done.load(Ordering::SeqCst) {
        shell.pump();
        if !streaming && ready_rx.try_recv().is_ok() {
            streaming = true;
            next_sample = Instant::now() + Duration::from_secs(3);
        }
        if streaming && Instant::now() >= next_sample {
            mem_samples.push(private_working_set_mb());
            next_sample += Duration::from_secs(2);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let gpu_mem = gpu_memory_mb();
    let _ = producer.join();
    let viewed = viewer_thread.join().map_err(|_| "viewer thread panicked".to_string())??;
    let wall = t0.elapsed().as_secs_f64();
    let process_cpu = (process_cpu_seconds() - cpu0) / wall.max(0.001) * 100.0;
    let produced = produced.lock().expect("lock").take().ok_or("encoder thread did not finish")??;
    Ok(Report {
        shell_adapter: shell.adapter_name.clone(),
        render_adapter,
        dpi: shell.dpi(),
        monitor: window_monitor(shell.hwnd),
        window_client: client,
        produced,
        viewed,
        mem_baseline_mb: mem_baseline,
        mem_samples_mb: mem_samples,
        gpu_mem_mb: gpu_mem,
        process_cpu_percent_of_core: process_cpu,
        wall_s: wall,
        encoder_child: cfg.encoder_child,
    })
}

/// The child side: encodes and writes frames to stdout. Nothing else may print to stdout.
pub fn encode_child_main(s: &EncodeSettings) -> Result<(), String> {
    // SAFETY: raises the timer resolution of the process for the pacing sleeps.
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
    let mut failed = None;
    produce(s, |e| match write_frame(&mut out, &e).and_then(|()| out.flush()) {
        Ok(()) => true,
        Err(err) => {
            failed = Some(err.to_string());
            false
        }
    })?;
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn run_child(exe: &std::path::Path, s: &EncodeSettings, tx: &SyncSender<EncodedFrame>) -> Result<Produced, String> {
    let mut child = Command::new(exe)
        .args([
            "--encode-child",
            &s.width.to_string(),
            &s.height.to_string(),
            &s.fps.to_string(),
            &s.kbps.to_string(),
            &s.seconds.to_string(),
            &s.adapter.to_string(),
            &s.pool.to_string(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn encoder child: {e}"))?;
    let mut stdout = std::io::BufReader::with_capacity(1 << 20, child.stdout.take().ok_or("no child stdout")?);
    let mut produced = Produced::default();
    while let Some(e) = read_frame(&mut stdout).map_err(|e| e.to_string())? {
        produced.encoded += 1;
        if tx.send(e).is_err() {
            break;
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("encoder child exited with {status}"));
    }
    produced.captured = (s.seconds * f64::from(s.fps)).round() as u32;
    Ok(produced)
}
