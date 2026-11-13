//! Error mapping, the QPC clock, DXGI adapters and D3D11 devices.

use std::sync::OnceLock;

use swapdesk_platform::{AdapterId, ErrorKind, MonoMicros, PlatformError, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::Interface;

const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A_0005_u32 as i32;
const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A_0007_u32 as i32;
const DXGI_ERROR_DEVICE_HUNG: i32 = 0x887A_0006_u32 as i32;
const DXGI_ERROR_DRIVER_INTERNAL_ERROR: i32 = 0x887A_0020_u32 as i32;
pub(crate) const DXGI_ERROR_WAS_STILL_DRAWING: i32 = 0x887A_000A_u32 as i32;

/// `PlatformError` from a Windows error. A lost or reset GPU device is `Unavailable` (detach and attach again).
pub(crate) fn os_err(op: &'static str, e: &windows::core::Error) -> PlatformError {
    let code = e.code().0;
    let lost = matches!(
        code,
        DXGI_ERROR_DEVICE_REMOVED | DXGI_ERROR_DEVICE_RESET | DXGI_ERROR_DEVICE_HUNG | DXGI_ERROR_DRIVER_INTERNAL_ERROR
    );
    let (kind, detail) = if lost {
        (ErrorKind::Unavailable, format!("GPU device lost, detach and attach again: {}", e.message()))
    } else {
        (ErrorKind::Os, e.message())
    };
    PlatformError { kind, op, os_code: Some(i64::from(code)), detail }
}

/// Devices a test declared removed (F-VIEW-08): every renderer on one of them fails as on a real removal, until the
/// process ends. Empty outside tests, so the check costs one atomic load.
static SIMULATED: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
static ANY_SIMULATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tests only: from now on the D3D11 device at `d3d11_device` (the raw pointer a `NativeSurface` carries) behaves as
/// removed for every renderer of this process: `attach` and `present` fail with `DXGI_ERROR_DEVICE_REMOVED` and
/// [`crate::WinRenderer::device_lost`] says so. Real Direct3D 11 has no call that removes a device on demand.
#[doc(hidden)]
pub fn simulate_device_removed(d3d11_device: u64) {
    SIMULATED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(d3d11_device);
    ANY_SIMULATED.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn simulated_removed(device: &ID3D11Device) -> bool {
    ANY_SIMULATED.load(std::sync::atomic::Ordering::SeqCst)
        && SIMULATED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&(device.as_raw() as usize as u64))
}

/// The device is gone: removed by a test, or by the driver (`GetDeviceRemovedReason` is not `S_OK`).
pub(crate) fn device_removed(device: &ID3D11Device) -> bool {
    // SAFETY: a plain query on a live device.
    simulated_removed(device) || unsafe { device.GetDeviceRemovedReason() }.is_err()
}

/// The error a call on a removed device gives.
pub(crate) fn removed_err(op: &'static str) -> PlatformError {
    os_err(op, &windows::core::Error::from(windows::core::HRESULT(DXGI_ERROR_DEVICE_REMOVED)))
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

fn qpc_frequency() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: valid out pointer; cannot fail on Windows XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        u64::try_from(f).unwrap_or(0).max(1)
    })
}

/// The machine's QueryPerformanceCounter in microseconds: the `MonoMicros` clock of C3 (the capture crate's clock).
pub fn now() -> MonoMicros {
    let mut t = 0i64;
    // SAFETY: valid out pointer; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    let ticks = u64::try_from(t).unwrap_or(0);
    MonoMicros(u64::try_from(u128::from(ticks) * 1_000_000 / u128::from(qpc_frequency())).unwrap_or(u64::MAX))
}

/// A device and its immediate context.
#[derive(Clone)]
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

impl Gpu {
    /// Wraps the shell's device: the renderer and the decoder call it from the viewer thread while the UI thread draws
    /// on it, so the multithread guard is switched on (a plain flag, harmless when already set).
    pub(crate) fn from_device(device: ID3D11Device) -> Result<Self> {
        protect_multithread(&device);
        // SAFETY: GetImmediateContext only returns an interface.
        let context = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        Ok(Self { device, context })
    }

    /// A device of its own on the adapter with this LUID (`high << 32 | low`); `AdapterId(0)` is the first hardware one.
    pub(crate) fn on_adapter(id: AdapterId) -> Result<Self> {
        let adapter = find_adapter(id)?;
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
        Ok(Self { device, context })
    }
}

fn protect_multithread(device: &ID3D11Device) {
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: a plain flag on the device.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
}

fn find_adapter(id: AdapterId) -> Result<IDXGIAdapter1> {
    // SAFETY: plain factory and enumeration calls on objects created here.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ctx("CreateDXGIFactory1")?;
        let mut first_hardware = None;
        for i in 0.. {
            let Ok(a) = factory.EnumAdapters1(i) else { break };
            let d = a.GetDesc1().ctx("IDXGIAdapter1::GetDesc1")?;
            let luid = u64::from(d.AdapterLuid.HighPart as u32) << 32 | u64::from(d.AdapterLuid.LowPart);
            let software = d.Flags & windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
            if id.0 != 0 && luid == id.0 {
                return Ok(a);
            }
            if first_hardware.is_none() && !software {
                first_hardware = Some(a);
            }
        }
        match (id.0, first_hardware) {
            (0, Some(a)) => Ok(a),
            _ => Err(err(ErrorKind::Unavailable, "find_adapter", format!("no DXGI adapter with LUID {:#x}", id.0))),
        }
    }
}

/// The LUID of the adapter a device runs on.
pub fn device_luid(device: &ID3D11Device) -> Result<AdapterId> {
    let dxgi: IDXGIDevice = device.cast().ctx("ID3D11Device as IDXGIDevice")?;
    // SAFETY: GetAdapter and GetDesc only return interfaces and structs.
    let d = unsafe {
        let a: IDXGIAdapter1 =
            dxgi.GetAdapter().ctx("IDXGIDevice::GetAdapter")?.cast().ctx("IDXGIAdapter as IDXGIAdapter1")?;
        a.GetDesc1().ctx("IDXGIAdapter1::GetDesc1")?
    };
    Ok(AdapterId(u64::from(d.AdapterLuid.HighPart as u32) << 32 | u64::from(d.AdapterLuid.LowPart)))
}

/// An owned Win32 handle (the swap chain's frame-latency object).
pub(crate) struct Waitable(pub HANDLE);

impl Drop for Waitable {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle was returned by GetFrameLatencyWaitableObject and is closed exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}
