//! COM and Win32 helpers shared by the source and the sink.

use std::sync::OnceLock;

use swapdesk_platform::{ErrorKind, PlatformError};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Output format of every stream: 48 kHz, stereo, 32-bit float.
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

pub fn wave_format_48k_stereo_f32() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: 2,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// Maps a failed call to the C3 kind: device gone or reset is `AccessLost` (the caller reopens), no endpoint, device
/// in use or no audio service is `Unavailable`.
pub fn os_error(op: &'static str, e: &windows::core::Error) -> PlatformError {
    let code = e.code().0;
    let kind = match code as u32 {
        0x8889_0004 | 0x8889_0026 => ErrorKind::AccessLost, // AUDCLNT_E_DEVICE_INVALIDATED, _RESOURCES_INVALIDATED
        0x8889_000A | 0x8889_0010 | 0x8007_0490 => ErrorKind::Unavailable, // DEVICE_IN_USE, SERVICE_NOT_RUNNING, E_NOTFOUND
        0x8889_0008 => ErrorKind::Unsupported,                             // UNSUPPORTED_FORMAT
        _ => ErrorKind::Os,
    };
    PlatformError { kind, op, os_code: Some(i64::from(code)), detail: e.message() }
}

pub fn failure(kind: ErrorKind, op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.to_string() }
}

/// COM initialised for the thread that made it (MTA), released with it. A thread already in another apartment keeps
/// that one.
pub struct ComGuard(bool);

impl ComGuard {
    pub fn init() -> Self {
        // SAFETY: plain call; the result says whether this call must be balanced by CoUninitialize.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self(hr.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx of this guard, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// An auto-reset event the audio engine signals when a buffer is ready.
pub struct Event(pub HANDLE);

impl Event {
    pub fn new() -> Result<Self, PlatformError> {
        // SAFETY: no pointers; an unnamed auto-reset event, initially not signalled.
        unsafe { CreateEventW(None, false, false, None) }.map(Self).map_err(|e| os_error("CreateEventW", &e))
    }

    /// True when signalled, false on timeout.
    pub fn wait(&self, ms: u32) -> bool {
        // SAFETY: the handle is owned by this value.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed once.
        let _ = unsafe { CloseHandle(self.0) };
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

/// The machine's monotonic clock in microseconds (the `MonoMicros` of C3: QueryPerformanceCounter).
pub fn qpc_micros() -> u64 {
    let mut t = 0i64;
    // SAFETY: valid out pointer; cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    let ticks = u128::from(u64::try_from(t).unwrap_or(0));
    (ticks * 1_000_000 / u128::from(qpc_frequency())) as u64
}

pub fn default_render_device() -> Result<IMMDevice, PlatformError> {
    // SAFETY: COM is initialised on this thread by the caller's `ComGuard`.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| os_error("CoCreateInstance(MMDeviceEnumerator)", &e))?;
        enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| os_error("GetDefaultAudioEndpoint", &e))
    }
}

/// The endpoint's id string as UTF-16, to tell when the default device changed.
pub fn device_id(device: &IMMDevice) -> Result<Vec<u16>, PlatformError> {
    // SAFETY: GetId returns a CoTaskMem string that is copied and freed here.
    unsafe {
        let id = device.GetId().map_err(|e| os_error("IMMDevice.GetId", &e))?;
        let v = id.as_wide().to_vec();
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(v)
    }
}

pub fn activate_client(device: &IMMDevice) -> Result<IAudioClient, PlatformError> {
    // SAFETY: plain COM call on an initialised thread.
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|e| os_error("IMMDevice.Activate(IAudioClient)", &e))
}
