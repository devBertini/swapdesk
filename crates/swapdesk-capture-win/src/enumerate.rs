//! Adapters, outputs and per-display geometry from DXGI, GDI and the per-monitor DPI API (windows.md 2a, 2b).

use crate::errors::{self, Phase};
use crate::geometry::{self, OutputKey, RawOutput};
use swapdesk_platform::{DisplayInfo, ErrorKind, PlatformError, Rect, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter1,
    IDXGIAdapter3, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW, GetMonitorInfoW,
    HMONITOR, MONITORINFO,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetThreadDpiAwarenessContext,
};
use windows::core::{Interface, PCWSTR};

/// Everything needed to duplicate one output: the C3 facts, where it lives and the DXGI objects.
pub struct OutputEntry {
    pub info: DisplayInfo,
    pub key: OutputKey,
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput1,
    pub adapter_name: String,
}

/// One adapter with the outputs it owns, for the R3 map. Adapters without outputs are listed too.
#[derive(Debug, Clone)]
pub struct AdapterReport {
    pub luid: u64,
    pub description: String,
    pub vendor_id: u32,
    pub dedicated_video_mib: u64,
    pub software: bool,
    pub outputs: Vec<OutputReport>,
}

#[derive(Debug, Clone)]
pub struct OutputReport {
    pub output_index: u32,
    pub device_name: String,
    pub attached_to_desktop: bool,
    /// Present for outputs that are attached to the desktop.
    pub display: Option<DisplayInfo>,
    pub hmonitor: isize,
    /// `DesktopCoordinates` as DXGI reports them to this thread (per-monitor DPI aware while enumerating); it should
    /// equal the GDI-derived `display.bounds`.
    pub dxgi_rect: Rect,
}

pub(crate) fn os_error(op: &'static str, e: &windows::core::Error) -> PlatformError {
    errors::from_hresult(Phase::Duplicate, op, e.code().0, &e.message())
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

fn rect_of(r: &RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        width: u32::try_from(i64::from(r.right) - i64::from(r.left)).unwrap_or(0),
        height: u32::try_from(i64::from(r.bottom) - i64::from(r.top)).unwrap_or(0),
    }
}

/// Makes the calling thread per-monitor DPI aware (v2) for as long as it lives and restores the previous mode after:
/// the DPI and coordinates read inside are physical, whatever the host process declared.
struct DpiScope(DPI_AWARENESS_CONTEXT);

impl DpiScope {
    fn enter() -> Self {
        // SAFETY: plain thread-state call; the previous context is restored in `drop`.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: `self.0` came from the matching Set call on this thread.
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

fn adapter_description(desc: &DXGI_ADAPTER_DESC1) -> String {
    wide_to_string(&desc.Description)
}

fn luid_u64(desc: &DXGI_ADAPTER_DESC1) -> u64 {
    // The LUID's high part is signed in the API; keep the bits.
    (u64::from(desc.AdapterLuid.HighPart as u32) << 32) | u64::from(desc.AdapterLuid.LowPart)
}

fn factory() -> Result<IDXGIFactory1> {
    // SAFETY: no preconditions; a fresh factory sees the current adapters and outputs.
    unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.map_err(|e| os_error("CreateDXGIFactory1", &e))
}

fn adapters(factory: &IDXGIFactory1) -> Result<Vec<IDXGIAdapter1>> {
    let mut list = Vec::new();
    for i in 0.. {
        // SAFETY: index enumeration, ends with DXGI_ERROR_NOT_FOUND.
        match unsafe { factory.EnumAdapters1(i) } {
            Ok(a) => list.push(a),
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(os_error("EnumAdapters1", &e)),
        }
    }
    Ok(list)
}

fn outputs_of(adapter: &IDXGIAdapter1) -> Vec<(u32, IDXGIOutput)> {
    let mut list = Vec::new();
    for i in 0.. {
        // SAFETY: index enumeration, ends with DXGI_ERROR_NOT_FOUND (any error stops the walk).
        match unsafe { adapter.EnumOutputs(i) } {
            Ok(o) => list.push((i, o)),
            Err(_) => break,
        }
    }
    list
}

fn monitor_facts(hmonitor: HMONITOR) -> (bool, u32) {
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `info.cbSize` is set; the handle came from DXGI.
    let primary = unsafe { GetMonitorInfoW(hmonitor, &mut info) }.as_bool() && info.dwFlags & 1 != 0;
    let (mut dx, mut dy) = (0u32, 0u32);
    // SAFETY: out pointers are valid for the call.
    let dpi = match unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } {
        Ok(()) => dx,
        Err(_) => 0,
    };
    (primary, dpi)
}

/// Mode of the display from GDI: virtual-desktop position and size in physical pixels, refresh rate in Hz.
fn gdi_mode(device_name: &[u16]) -> Option<(i32, i32, u32, u32, u32)> {
    let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    // SAFETY: `device_name` is NUL-terminated (a DXGI DeviceName buffer); `dm.dmSize` is set.
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(device_name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool();
    if !ok {
        return None;
    }
    // SAFETY: for a display device the position member of the union is the one the mode fills in.
    let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
    Some((pos.x, pos.y, dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency))
}

fn friendly_name(device_name: &[u16]) -> String {
    let mut dd = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
    // SAFETY: `device_name` is NUL-terminated; `dd.cb` is set; index 0 is the monitor on that adapter output.
    if unsafe { EnumDisplayDevicesW(PCWSTR(device_name.as_ptr()), 0, &mut dd, 0) }.as_bool() {
        wide_to_string(&dd.DeviceString)
    } else {
        String::new()
    }
}

struct Found {
    raw: RawOutput,
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    hmonitor: isize,
    dxgi_rect: Rect,
    attached: bool,
    output_index: u32,
    device_name: String,
}

fn walk() -> Result<Vec<Found>> {
    let _dpi = DpiScope::enter();
    let factory = factory()?;
    let mut found = Vec::new();
    for adapter in adapters(&factory)? {
        // SAFETY: plain query on a live adapter.
        let adesc = unsafe { adapter.GetDesc1() }.map_err(|e| os_error("IDXGIAdapter1::GetDesc1", &e))?;
        for (index, output) in outputs_of(&adapter) {
            // SAFETY: plain query on a live output.
            let Ok(odesc) = (unsafe { output.GetDesc() }) else { continue };
            let Ok(output1) = output.cast::<IDXGIOutput1>() else { continue };
            let dxgi_rect = rect_of(&odesc.DesktopCoordinates);
            let device_name = wide_to_string(&odesc.DeviceName);
            let attached = odesc.AttachedToDesktop.as_bool();
            let (primary, dpi) = monitor_facts(odesc.Monitor);
            // GDI gives the physical mode regardless of the process's DPI mode; DXGI's rectangle is the fallback.
            let (rect, refresh_hz) = match gdi_mode(&odesc.DeviceName) {
                Some((x, y, w, h, hz)) if w > 0 && h > 0 => (Rect { x, y, width: w, height: h }, hz),
                _ => (dxgi_rect, 0),
            };
            found.push(Found {
                raw: RawOutput {
                    adapter_luid: luid_u64(&adesc),
                    adapter_name: adapter_description(&adesc),
                    output_index: index,
                    device_name: device_name.clone(),
                    monitor_name: friendly_name(&odesc.DeviceName),
                    rect,
                    dpi,
                    refresh_hz,
                    rotation_deg: geometry::rotation_degrees(odesc.Rotation.0),
                    primary,
                },
                adapter: adapter.clone(),
                output: output1,
                hmonitor: odesc.Monitor.0 as isize,
                dxgi_rect,
                attached,
                output_index: index,
                device_name,
            });
        }
    }
    Ok(found)
}

/// Every output attached to the desktop with the objects to duplicate it, in the C3 order (primary first).
pub fn enumerate() -> Result<Vec<OutputEntry>> {
    let found = walk()?;
    let raws: Vec<RawOutput> = found.iter().filter(|f| f.attached).map(|f| f.raw.clone()).collect();
    let mut entries = Vec::new();
    for (info, key) in geometry::normalize(&raws) {
        let Some(f) = found
            .iter()
            .find(|f| f.attached && f.raw.adapter_luid == key.adapter_luid && f.output_index == key.output_index)
        else {
            continue;
        };
        entries.push(OutputEntry {
            info,
            key,
            adapter: f.adapter.clone(),
            output: f.output.clone(),
            adapter_name: f.raw.adapter_name.clone(),
        });
    }
    Ok(entries)
}

/// The C3 display list (`ScreenSource::displays`).
pub fn displays() -> Result<Vec<DisplayInfo>> {
    Ok(enumerate()?.into_iter().map(|e| e.info).collect())
}

/// Adapter, LUID and output of every display (risk R3), adapters without outputs included.
pub fn adapter_map() -> Result<Vec<AdapterReport>> {
    let found = walk()?;
    let attached_raws: Vec<RawOutput> = found.iter().filter(|f| f.attached).map(|f| f.raw.clone()).collect();
    let normalized = geometry::normalize(&attached_raws);
    let factory = {
        let _dpi = DpiScope::enter();
        factory()?
    };
    let mut reports = Vec::new();
    for adapter in adapters(&factory)? {
        // SAFETY: plain query on a live adapter.
        let adesc = unsafe { adapter.GetDesc1() }.map_err(|e| os_error("IDXGIAdapter1::GetDesc1", &e))?;
        let luid = luid_u64(&adesc);
        let outputs = found
            .iter()
            .filter(|f| f.raw.adapter_luid == luid)
            .map(|f| OutputReport {
                output_index: f.output_index,
                device_name: f.device_name.clone(),
                attached_to_desktop: f.attached,
                display: normalized
                    .iter()
                    .find(|(_, k)| k.adapter_luid == luid && k.output_index == f.output_index)
                    .map(|(d, _)| d.clone()),
                hmonitor: f.hmonitor,
                dxgi_rect: f.dxgi_rect,
            })
            .collect();
        reports.push(AdapterReport {
            luid,
            description: adapter_description(&adesc),
            vendor_id: adesc.VendorId,
            dedicated_video_mib: (adesc.DedicatedVideoMemory as u64) >> 20,
            software: adesc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            outputs,
        });
    }
    Ok(reports)
}

/// Video memory this process currently uses on the adapter (local plus non-local segments), from
/// `IDXGIAdapter3::QueryVideoMemoryInfo`. For the leak check.
pub fn video_memory_in_use(adapter_luid: u64) -> Result<u64> {
    let factory = factory()?;
    for adapter in adapters(&factory)? {
        // SAFETY: plain query on a live adapter.
        let adesc = unsafe { adapter.GetDesc1() }.map_err(|e| os_error("IDXGIAdapter1::GetDesc1", &e))?;
        if luid_u64(&adesc) != adapter_luid {
            continue;
        }
        let a3 = adapter.cast::<IDXGIAdapter3>().map_err(|e| os_error("IDXGIAdapter3", &e))?;
        let mut total = 0u64;
        for seg in [DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL] {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            // SAFETY: `info` is a valid out struct; node 0 is the adapter's only node on these GPUs.
            unsafe { a3.QueryVideoMemoryInfo(0, seg, &mut info) }.map_err(|e| os_error("QueryVideoMemoryInfo", &e))?;
            total += info.CurrentUsage;
        }
        return Ok(total);
    }
    Err(errors::error(ErrorKind::InvalidInput, "video_memory_in_use", None, "no adapter with that LUID"))
}
