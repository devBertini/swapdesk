//! COM and Media Foundation start-up, error mapping, DXGI adapters and D3D11 devices.

use std::cell::Cell;
use std::sync::OnceLock;

use swapdesk_platform::{AdapterId, ErrorKind, PlatformError, Result};
use windows::Win32::Foundation::{HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1,
};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::Interface;

/// Builds a [`PlatformError`] from a Windows error.
pub(crate) fn os_err(op: &'static str, e: &windows::core::Error) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: Some(i64::from(e.code().0)), detail: e.message() }
}

pub(crate) fn err(kind: ErrorKind, op: &'static str, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.into() }
}

pub(crate) trait Ctx<T> {
    fn ctx(self, op: &'static str) -> Result<T>;
}

impl<T> Ctx<T> for windows::core::Result<T> {
    fn ctx(self, op: &'static str) -> Result<T> {
        self.map_err(|e| os_err(op, &e))
    }
}

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

static MF_STARTED: OnceLock<std::result::Result<(), i32>> = OnceLock::new();

/// Joins the calling thread to the COM multithreaded apartment (once per thread) and starts Media Foundation (once per
/// process). A thread that already runs a single-threaded apartment keeps it: Media Foundation works there too.
pub(crate) fn init_thread() -> Result<()> {
    if !COM_READY.get() {
        // SAFETY: plain COM initialisation of the calling thread; the apartment lives as long as the thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(PlatformError {
                kind: ErrorKind::Os,
                op: "CoInitializeEx",
                os_code: Some(i64::from(hr.0)),
                detail: String::new(),
            });
        }
        COM_READY.set(true);
    }
    let started = MF_STARTED.get_or_init(|| {
        // SAFETY: MFStartup is reference counted and safe to call from any thread; it is never shut down because
        // Media Foundation objects of this crate may live until process exit.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.code().0)
    });
    started.map_err(|code| PlatformError {
        kind: ErrorKind::Os,
        op: "MFStartup",
        os_code: Some(i64::from(code)),
        detail: String::new(),
    })
}

/// One DXGI adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterDesc {
    /// The adapter LUID, `HighPart << 32 | LowPart` (C3 `AdapterId` on Windows).
    pub id: AdapterId,
    pub name: String,
    pub vendor_id: u32,
    /// The Microsoft Basic Render Driver (WARP).
    pub software: bool,
}

pub(crate) fn luid_to_id(low: u32, high: i32) -> AdapterId {
    AdapterId(u64::from(high as u32) << 32 | u64::from(low))
}

pub(crate) fn id_to_luid(id: AdapterId) -> (u32, i32) {
    (id.0 as u32, (id.0 >> 32) as u32 as i32)
}

fn describe(adapter: &IDXGIAdapter1) -> Result<AdapterDesc> {
    // SAFETY: GetDesc1 only fills the returned struct.
    let d = unsafe { adapter.GetDesc1() }.ctx("IDXGIAdapter1::GetDesc1")?;
    let end = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
    Ok(AdapterDesc {
        id: luid_to_id(d.AdapterLuid.LowPart, d.AdapterLuid.HighPart),
        name: String::from_utf16_lossy(&d.Description[..end]),
        vendor_id: d.VendorId,
        software: d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
    })
}

fn each_adapter() -> Result<Vec<(IDXGIAdapter1, AdapterDesc)>> {
    init_thread()?;
    // SAFETY: factory creation and adapter enumeration take no pointers from us.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ctx("CreateDXGIFactory1")?;
    let mut out = Vec::new();
    for i in 0.. {
        // SAFETY: see above.
        match unsafe { factory.EnumAdapters1(i) } {
            Ok(a) => {
                let d = describe(&a)?;
                out.push((a, d));
            }
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(os_err("IDXGIFactory1::EnumAdapters1", &e)),
        }
    }
    Ok(out)
}

/// Every DXGI adapter on the machine, in DXGI order.
pub fn list_adapters() -> Result<Vec<AdapterDesc>> {
    Ok(each_adapter()?.into_iter().map(|(_, d)| d).collect())
}

/// The adapter with this LUID. `AdapterId(0)` picks the first hardware adapter (the one DXGI lists first).
pub(crate) fn find_adapter(id: AdapterId) -> Result<(IDXGIAdapter1, AdapterDesc)> {
    let all = each_adapter()?;
    let pick = if id.0 == 0 {
        all.iter().position(|(_, d)| !d.software).or(if all.is_empty() { None } else { Some(0) })
    } else {
        all.iter().position(|(_, d)| d.id == id)
    };
    match pick {
        Some(i) => Ok(all.into_iter().nth(i).expect("index from position")),
        None => Err(err(ErrorKind::Unavailable, "find_adapter", format!("no DXGI adapter with LUID {:#x}", id.0))),
    }
}

/// A D3D11 device with video and BGRA support on `adapter`, with the multithread guard Media Foundation needs.
pub(crate) fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    init_thread()?;
    let base: IDXGIAdapter = adapter.cast().ctx("IDXGIAdapter1 as IDXGIAdapter")?;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut context = None;
    // SAFETY: the out pointers are valid for the call; the adapter outlives it.
    unsafe {
        D3D11CreateDevice(
            &base,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .ctx("D3D11CreateDevice")?;
    let device = device.ok_or_else(|| err(ErrorKind::Os, "D3D11CreateDevice", "no device returned"))?;
    let context = context.ok_or_else(|| err(ErrorKind::Os, "D3D11CreateDevice", "no context returned"))?;
    protect_multithread(&device);
    Ok((device, context))
}

/// Media Foundation transforms use the device from their own threads: turn the multithread guard on.
pub(crate) fn protect_multithread(device: &ID3D11Device) {
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: a plain flag on the device.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
}

/// The LUID of the adapter a device runs on.
pub(crate) fn device_adapter(device: &ID3D11Device) -> Result<AdapterDesc> {
    let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast().ctx("ID3D11Device as IDXGIDevice")?;
    // SAFETY: GetAdapter only returns an interface.
    let adapter = unsafe { dxgi.GetAdapter() }.ctx("IDXGIDevice::GetAdapter")?;
    let a1: IDXGIAdapter1 = adapter.cast().ctx("IDXGIAdapter as IDXGIAdapter1")?;
    describe(&a1)
}
