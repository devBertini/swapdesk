//! The updater's HTTPS GET on Windows (UPDATES.md 5): WinHTTP, the system's client, so TLS, certificate validation
//! and the proxy settings are the system's. Synchronous mode on the caller's thread with the guarantees set one by
//! one: TLS 1.2 or 1.3 only, the system's proxy settings (`WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`), cookies disabled,
//! automatic redirects off and followed here under `swapdesk_update::http`'s policy (at most 5, never to http),
//! certificate errors never ignored. A cancel closes the request handle from a watcher thread, which ends a blocked
//! call. (`WINHTTP_FLAG_SECURE_DEFAULTS` would set the first two but forces the asynchronous mode.)
//!
//! Microsoft documentation: WinHttpOpen, WinHttpOpenRequest, WinHttpSetOption (WINHTTP_OPTION_SECURE_PROTOCOLS,
//! WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_DISABLE_FEATURE), WinHttpQueryHeaders, WinHttpReadData,
//! WinHttpCloseHandle (learn.microsoft.com/windows/win32/api/winhttp/; the constants as windows-sys 0.61.2 has them).

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use swapdesk_update::http::{
    ACCEPT, HttpError, HttpGet, MAX_REDIRECTS, USER_AGENT, Url, UrlPolicy, check_url, is_redirect, redirect_target,
};
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_DISABLE_COOKIES, WINHTTP_FLAG_SECURE,
    WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3, WINHTTP_OPTION_DISABLE_FEATURE,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_OPTION_SECURE_PROTOCOLS,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts,
};

/// How often the watcher looks at the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);
/// Resolve, connect, send and receive timeouts, in milliseconds.
const TIMEOUTS_MS: (i32, i32, i32, i32) = (15_000, 15_000, 30_000, 30_000);

/// The WinHTTP client of the updater.
pub struct WinHttp {
    policy: UrlPolicy,
}

impl WinHttp {
    /// Production: `https` only.
    pub fn new() -> Self {
        Self { policy: UrlPolicy::HttpsOnly }
    }

    /// Tests: `https`, and `http` to `127.0.0.1` (a loopback server).
    pub fn for_loopback_tests() -> Self {
        Self { policy: UrlPolicy::HttpsOrLoopbackHttp }
    }
}

impl Default for WinHttp {
    fn default() -> Self {
        Self::new()
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A WinHTTP handle, closed once.
struct Handle(*mut c_void);

// SAFETY: a WinHTTP handle is an opaque value the library validates on each call; it may be used and closed from any
// thread (the cancel watcher closes the request handle to end a blocked call).
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a handle this module opened and has not closed (each `Handle` closes once).
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn last_error(what: &str) -> HttpError {
    // SAFETY: no preconditions.
    let code = unsafe { GetLastError() };
    HttpError::Network(format!("{what} failed with Windows error {code}"))
}

fn set_u32(handle: *mut c_void, option: u32, value: u32) -> Result<(), HttpError> {
    // SAFETY: `value` lives across the call and its size is passed.
    let ok = unsafe { WinHttpSetOption(handle, option, std::ptr::from_ref(&value).cast(), 4) };
    if ok == 0 { Err(last_error("WinHttpSetOption")) } else { Ok(()) }
}

/// What one request ended with.
enum Hop {
    Body,
    Redirect(String),
}

/// The request handle, shared with the watcher that closes it on a cancel.
type Shared = Arc<Mutex<Option<Handle>>>;

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, Option<Handle>> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WinHttp {
    fn session() -> Result<Handle, HttpError> {
        let agent = wide(USER_AGENT);
        // SAFETY: `agent` is a NUL-terminated wide string living across the call; no proxy names with the automatic
        // access type.
        let session =
            Handle(unsafe { WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0) });
        if session.0.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        set_u32(
            session.0,
            WINHTTP_OPTION_SECURE_PROTOCOLS,
            WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3,
        )?;
        let (r, c, s, v) = TIMEOUTS_MS;
        // SAFETY: a live session handle.
        if unsafe { WinHttpSetTimeouts(session.0, r, c, s, v) } == 0 {
            return Err(last_error("WinHttpSetTimeouts"));
        }
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)] // one request: where, the bound, the sink, and the three ways out
    fn hop(
        session: &Handle,
        url: &Url,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
        written: &mut u64,
    ) -> Result<Hop, HttpError> {
        let host = wide(&url.host);
        // SAFETY: a live session; `host` is NUL-terminated and lives across the call.
        let connect = Handle(unsafe { WinHttpConnect(session.0, host.as_ptr(), url.port, 0) });
        if connect.0.is_null() {
            return Err(last_error("WinHttpConnect"));
        }
        let (verb, path) = (wide("GET"), wide(&url.path));
        let flags = if url.https { WINHTTP_FLAG_SECURE } else { 0 };
        // SAFETY: a live connection; the strings are NUL-terminated and live across the call; no referrer, no
        // accept types (the one Accept header is added below).
        let request =
            unsafe { WinHttpOpenRequest(connect.0, verb.as_ptr(), path.as_ptr(), null(), null(), null(), flags) };
        if request.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }
        let shared: Shared = Arc::new(Mutex::new(Some(Handle(request))));
        set_u32(request, WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER)?;
        set_u32(request, WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_DISABLE_COOKIES)?;

        // The watcher: a cancel closes the request handle, which ends a blocked send, receive or read.
        let done = AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    if cancel.load(Ordering::Acquire) {
                        drop(lock(&shared).take());
                        return;
                    }
                    std::thread::sleep(CANCEL_POLL);
                }
            });
            let result = Self::exchange(&shared, request, max_bytes, sink, progress, cancel, written);
            done.store(true, Ordering::Release);
            result
        });
        drop(lock(&shared).take());
        if cancel.load(Ordering::Acquire) {
            return Err(HttpError::Cancelled);
        }
        result
    }

    fn exchange(
        shared: &Shared,
        request: *mut c_void,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
        written: &mut u64,
    ) -> Result<Hop, HttpError> {
        let alive = || lock(shared).is_some() && !cancel.load(Ordering::Acquire);
        let headers = wide(&format!("Accept: {ACCEPT}\r\n"));
        // SAFETY: the request handle is live (or closed by a cancel, which WinHTTP reports as an error); the header
        // string is NUL-terminated (length -1 means "up to the NUL"); no body.
        if unsafe { WinHttpSendRequest(request, headers.as_ptr(), u32::MAX, null(), 0, 0, 0) } == 0 {
            return Err(if alive() { last_error("WinHttpSendRequest") } else { HttpError::Cancelled });
        }
        // SAFETY: as above; the reserved argument is null.
        if unsafe { WinHttpReceiveResponse(request, null_mut()) } == 0 {
            return Err(if alive() { last_error("WinHttpReceiveResponse") } else { HttpError::Cancelled });
        }
        let mut status = 0u32;
        let mut len = 4u32;
        // SAFETY: a number query into a live u32 of the size passed; no header name, no index.
        let ok = unsafe {
            WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                std::ptr::from_mut(&mut status).cast(),
                &mut len,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error("WinHttpQueryHeaders"));
        }
        let status = u16::try_from(status).unwrap_or(0);
        if is_redirect(status) {
            let mut buf = vec![0u16; 4096];
            let mut bytes = (buf.len() * 2) as u32;
            // SAFETY: a string query into `buf`, whose size in bytes is passed.
            let ok = unsafe {
                WinHttpQueryHeaders(
                    request,
                    WINHTTP_QUERY_LOCATION,
                    null(),
                    buf.as_mut_ptr().cast(),
                    &mut bytes,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(HttpError::Refused("a redirect without a Location".into()));
            }
            return Ok(Hop::Redirect(String::from_utf16_lossy(&buf[..(bytes / 2) as usize])));
        }
        if status != 200 {
            return Err(HttpError::Status(status));
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if !alive() {
                return Err(HttpError::Cancelled);
            }
            let mut got = 0u32;
            // SAFETY: `buf` holds the length passed; `got` is a live out-parameter.
            if unsafe { WinHttpReadData(request, buf.as_mut_ptr().cast(), buf.len() as u32, &mut got) } == 0 {
                return Err(if alive() { last_error("WinHttpReadData") } else { HttpError::Cancelled });
            }
            if got == 0 {
                return Ok(Hop::Body);
            }
            let room = max_bytes.saturating_sub(*written);
            let take = u64::from(got).min(room) as usize;
            sink.write_all(&buf[..take]).map_err(|e| HttpError::Network(format!("the sink: {e}")))?;
            *written += take as u64;
            progress(*written);
            if take < got as usize {
                return Err(HttpError::TooLarge);
            }
        }
    }
}

impl HttpGet for WinHttp {
    fn get(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), HttpError> {
        let mut at = check_url(url, self.policy)?;
        let session = Self::session()?;
        let mut written = 0u64;
        for _ in 0..=MAX_REDIRECTS {
            match Self::hop(&session, &at, max_bytes, sink, progress, cancel, &mut written)? {
                Hop::Body => return Ok(()),
                Hop::Redirect(location) => at = redirect_target(&at, &location, self.policy)?,
            }
        }
        Err(HttpError::TooManyRedirects)
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
