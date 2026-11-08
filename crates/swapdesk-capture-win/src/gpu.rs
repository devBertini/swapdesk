//! One D3D11 device per adapter that owns an output, a texture pool for delivered frames and the opt-in readback.

use crate::enumerate::os_error;
use crate::errors::{self, Phase};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use swapdesk_platform::{ErrorKind, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter1;
use windows::core::Interface;

/// A D3D11 device and its immediate context on one adapter. Multithread protection is on, because the encoder and the
/// capture thread both use the device (contract C2.1: BGRA to NV12 and the MF encoder sit next to capture).
pub struct GpuDevice {
    pub luid: u64,
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

impl GpuDevice {
    pub fn create(luid: u64, adapter: &IDXGIAdapter1) -> Result<Self> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: out pointers are valid for the call; the adapter is live; with an explicit adapter the driver type
        // must be UNKNOWN.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|e| os_error("D3D11CreateDevice", &e))?;
        let (Some(device), Some(context)) = (device, context) else {
            return Err(errors::error(ErrorKind::Os, "D3D11CreateDevice", None, "no device returned"));
        };
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            // SAFETY: plain flag on a live context.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        Ok(Self { luid, device, context })
    }

    /// Whether the device is still usable (not removed, reset or hung).
    pub fn is_healthy(&self) -> bool {
        // SAFETY: plain query.
        unsafe { self.device.GetDeviceRemovedReason() }.is_ok()
    }
}

/// Devices by adapter LUID, shared by every `DxgiScreen` of a process so two displays on one adapter use one device.
#[derive(Default)]
pub struct DevicePool {
    devices: Mutex<HashMap<u64, Arc<GpuDevice>>>,
}

impl DevicePool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u64, Arc<GpuDevice>>> {
        self.devices.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The healthy device of the adapter, created on first use and again after a removal or reset.
    pub fn device_for(&self, luid: u64, adapter: &IDXGIAdapter1) -> Result<Arc<GpuDevice>> {
        let mut map = self.lock();
        if let Some(d) = map.get(&luid)
            && d.is_healthy()
        {
            return Ok(d.clone());
        }
        let d = Arc::new(GpuDevice::create(luid, adapter)?);
        map.insert(luid, d.clone());
        Ok(d)
    }

    /// Forgets a device so the next use builds a new one.
    pub fn invalidate(&self, luid: u64) {
        self.lock().remove(&luid);
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A CPU copy of a frame, BGRA, rows of `stride` bytes (tight: `width * 4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuImage {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

/// Textures of one size on one device, handed out for frames and taken back when a frame is dropped, so a steady
/// capture allocates nothing after warm-up.
pub(crate) struct TexturePool {
    pub(crate) gpu: Arc<GpuDevice>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    free: Mutex<Vec<ID3D11Texture2D>>,
    staging: Mutex<Option<ID3D11Texture2D>>,
    created: std::sync::atomic::AtomicUsize,
}

/// Textures kept for reuse; more than this many in flight are simply released when their frames drop.
const POOL_KEEP: usize = 8;

/// Bind flags of a delivered frame's texture. The encoders' D3D11 video processor (BGRA to NV12) only takes an input view
/// of a texture whose bind flags include DECODER, VIDEO_ENCODER, RENDER_TARGET or UNORDERED_ACCESS, or are 0
/// (`ID3D11VideoDevice::CreateVideoProcessorInputView`, Remarks); SHADER_RESOURCE alone is refused with E_INVALIDARG
/// (measured on Intel UHD with Media Foundation). RENDER_TARGET also lets NVENC register the texture without a copy.
const FRAME_BIND_FLAGS: u32 = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;

fn texture_desc(width: u32, height: u32, staging: bool) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: if staging { 0 } else { FRAME_BIND_FLAGS },
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        MiscFlags: 0,
    }
}

impl TexturePool {
    pub(crate) fn new(gpu: Arc<GpuDevice>, width: u32, height: u32) -> Arc<Self> {
        Arc::new(Self {
            gpu,
            width,
            height,
            free: Mutex::new(Vec::new()),
            staging: Mutex::new(None),
            created: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn create_texture(&self, staging: bool) -> Result<ID3D11Texture2D> {
        let desc = texture_desc(self.width, self.height, staging);
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` and the out pointer are valid for the call.
        unsafe { self.gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .map_err(|e| errors::from_hresult(Phase::Frame, "CreateTexture2D", e.code().0, &e.message()))?;
        tex.ok_or_else(|| errors::error(ErrorKind::Os, "CreateTexture2D", None, "no texture returned"))
    }

    pub(crate) fn take(self: &Arc<Self>) -> Result<GpuFrame> {
        let reused = self.free.lock().unwrap_or_else(|p| p.into_inner()).pop();
        let texture = match reused {
            Some(t) => t,
            None => {
                self.created.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.create_texture(false)?
            }
        };
        Ok(GpuFrame { texture: Some(texture), pool: self.clone() })
    }

    /// Textures this pool has ever allocated (for the leak check: it stops growing once the consumer keeps up).
    pub(crate) fn created(&self) -> usize {
        self.created.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A captured frame on the GPU: a BGRA `ID3D11Texture2D` on the device of the adapter that owns the output. The
/// texture goes back to the pool on drop. It is the payload of `Pixels::Gpu(GpuSurface(..))`: downcast with
/// `surface.0.downcast::<GpuFrame>()`.
pub struct GpuFrame {
    texture: Option<ID3D11Texture2D>,
    pool: Arc<TexturePool>,
}

impl GpuFrame {
    pub fn texture(&self) -> &ID3D11Texture2D {
        // The texture is only taken out in `drop`.
        self.texture.as_ref().expect("texture present until drop")
    }

    pub fn device(&self) -> &Arc<GpuDevice> {
        &self.pool.gpu
    }

    pub fn width(&self) -> u32 {
        self.pool.width
    }

    pub fn height(&self) -> u32 {
        self.pool.height
    }

    /// Copies the frame to CPU memory through a staging texture. Blocks until the GPU has finished the frame. For tests
    /// and fallbacks; the default path never calls it.
    pub fn read_back(&self) -> Result<CpuImage> {
        let pool = &self.pool;
        let mut guard = pool.staging.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(pool.create_texture(true)?);
        }
        let staging = guard.as_ref().expect("just created");
        let ctx = &pool.gpu.context;
        // SAFETY: both textures belong to this device and have the same size and format.
        unsafe { ctx.CopyResource(staging, self.texture()) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture was created with CPU read access; the out struct is valid.
        unsafe { ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .map_err(|e| errors::from_hresult(Phase::Frame, "ID3D11DeviceContext::Map", e.code().0, &e.message()))?;
        let (w, h) = (pool.width as usize, pool.height as usize);
        let row = w * 4;
        let mut data = vec![0u8; row * h];
        let pitch = mapped.RowPitch as usize;
        let src = mapped.pData as *const u8;
        for y in 0..h {
            // SAFETY: the mapping holds `RowPitch * height` bytes and `row <= RowPitch`; `data` has `row * h` bytes.
            unsafe { std::ptr::copy_nonoverlapping(src.add(y * pitch), data.as_mut_ptr().add(y * row), row) };
        }
        // SAFETY: unmaps the subresource mapped above.
        unsafe { ctx.Unmap(staging, 0) };
        Ok(CpuImage { width: pool.width, height: pool.height, stride: (row) as u32, data })
    }
}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        if let Some(t) = self.texture.take() {
            let mut free = self.pool.free.lock().unwrap_or_else(|p| p.into_inner());
            if free.len() < POOL_KEEP {
                free.push(t);
            }
        }
    }
}
