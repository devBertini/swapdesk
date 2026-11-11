//! The updater's HTTPS GET on macOS (UPDATES.md 5 and 2.3): `NSURLSession`, the system's client, so TLS, certificate
//! validation (ATS) and the proxy settings are the system's. Each hop is one data task on an ephemeral session
//! (`NSURLSessionConfiguration.ephemeral`: no cookie storage, no cache, no credentials on disk) whose delegate pushes
//! what happens into a channel the calling thread reads, so the caller's thread enforces the bound, reports progress
//! and watches the cancel flag (a cancel cancels the task). Automatic redirects are off: the delegate answers the
//! redirect callback with nil, so the 30x reaches the caller, which follows it under `swapdesk_update::http`'s policy
//! (at most 5, never to http).
//!
//! Apple documentation: NSURLSession, NSURLSessionConfiguration (ephemeral, HTTPShouldSetCookies, HTTPCookieStorage,
//! HTTPCookieAcceptPolicy, URLCache, requestCachePolicy), NSURLSessionTaskDelegate
//! (`URLSession:task:willPerformHTTPRedirection:newRequest:completionHandler:`), NSURLSessionDataDelegate.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{
    NSData, NSError, NSHTTPCookieAcceptPolicy, NSHTTPURLResponse, NSMutableURLRequest, NSObjectProtocol,
    NSOperationQueue, NSString, NSURL, NSURLRequest, NSURLRequestCachePolicy, NSURLResponse, NSURLSession,
    NSURLSessionConfiguration, NSURLSessionDataDelegate, NSURLSessionDataTask, NSURLSessionDelegate,
    NSURLSessionResponseDisposition, NSURLSessionTask, NSURLSessionTaskDelegate,
};
use swapdesk_update::http::{
    ACCEPT, HttpError, HttpGet, MAX_REDIRECTS, USER_AGENT, Url, UrlPolicy, check_url, is_redirect, redirect_target,
};

/// How often the calling thread looks at the cancel flag while the task is quiet.
const CANCEL_POLL: Duration = Duration::from_millis(50);
/// No bytes for this long on a request: the system's timeout (seconds), the same 30 s as the Windows receive timeout.
const REQUEST_TIMEOUT_SECS: f64 = 30.0;

/// The NSURLSession client of the updater.
pub struct MacHttp {
    policy: UrlPolicy,
}

impl MacHttp {
    /// Production: `https` only.
    pub fn new() -> Self {
        Self { policy: UrlPolicy::HttpsOnly }
    }

    /// Tests: `https`, and `http` to `127.0.0.1` (a loopback server).
    pub fn for_loopback_tests() -> Self {
        Self { policy: UrlPolicy::HttpsOrLoopbackHttp }
    }
}

impl Default for MacHttp {
    fn default() -> Self {
        Self::new()
    }
}

/// What the delegate tells the calling thread.
enum Event {
    /// The response head: the status and the `Location` header if there is one.
    Response {
        status: u16,
        location: Option<String>,
    },
    Data(Vec<u8>),
    /// The task ended: `None` is success, `Some` the system's words for the failure.
    Done(Option<String>),
}

struct Ivars {
    events: Sender<Event>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class has no `Drop` and its only ivar is a channel sender
    // touched through `&self`.
    #[unsafe(super = NSObject)]
    #[name = "SwapdeskUpdateDelegate"]
    #[ivars = Ivars]
    struct UpdateDelegate;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for UpdateDelegate {}

    // SAFETY: no methods are implemented.
    unsafe impl NSURLSessionDelegate for UpdateDelegate {}

    // SAFETY: the selectors and signatures are those of `NSURLSessionTaskDelegate`.
    unsafe impl NSURLSessionTaskDelegate for UpdateDelegate {
        #[unsafe(method(URLSession:task:willPerformHTTPRedirection:newRequest:completionHandler:))]
        fn will_redirect(
            &self,
            _session: &NSURLSession,
            _task: &NSURLSessionTask,
            _response: &NSHTTPURLResponse,
            _request: &NSURLRequest,
            completion_handler: &DynBlock<dyn Fn(*mut NSURLRequest)>,
        ) {
            // nil: do not follow; the 30x is delivered as the task's response and the caller judges it.
            completion_handler.call((ptr::null_mut(),));
        }

        #[unsafe(method(URLSession:task:didCompleteWithError:))]
        fn did_complete(&self, _session: &NSURLSession, _task: &NSURLSessionTask, error: Option<&NSError>) {
            let words = error.map(|e| format!("{} (code {})", e.localizedDescription(), e.code()));
            let _ = self.ivars().events.send(Event::Done(words));
        }
    }

    // SAFETY: the selectors and signatures are those of `NSURLSessionDataDelegate`.
    unsafe impl NSURLSessionDataDelegate for UpdateDelegate {
        #[unsafe(method(URLSession:dataTask:didReceiveResponse:completionHandler:))]
        fn did_receive_response(
            &self,
            _session: &NSURLSession,
            _task: &NSURLSessionDataTask,
            response: &NSURLResponse,
            completion_handler: &DynBlock<dyn Fn(NSURLSessionResponseDisposition)>,
        ) {
            let any: &AnyObject = response;
            let (status, location) = match any.downcast_ref::<NSHTTPURLResponse>() {
                Some(http) => {
                    let status = u16::try_from(http.statusCode()).unwrap_or(0);
                    let location = http.valueForHTTPHeaderField(&NSString::from_str("Location")).map(|l| l.to_string());
                    (status, location)
                }
                None => (0, None),
            };
            let _ = self.ivars().events.send(Event::Response { status, location });
            completion_handler.call((NSURLSessionResponseDisposition::Allow,));
        }

        #[unsafe(method(URLSession:dataTask:didReceiveData:))]
        fn did_receive_data(&self, _session: &NSURLSession, _task: &NSURLSessionDataTask, data: &NSData) {
            let _ = self.ivars().events.send(Event::Data(data.to_vec()));
        }
    }
);

impl UpdateDelegate {
    fn new(events: Sender<Event>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { events });
        // SAFETY: `init` of NSObject on a freshly allocated, ivar-initialised instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// What one request ended with.
enum Hop {
    Body,
    Redirect(String),
}

/// An ephemeral session that is invalidated (its tasks cancelled) when it goes out of scope.
struct Session {
    session: Retained<NSURLSession>,
    // The session retains its delegate until it is invalidated; ours is kept for clarity of ownership.
    _delegate: Retained<UpdateDelegate>,
    _queue: Retained<NSOperationQueue>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.session.invalidateAndCancel();
    }
}

fn session(events: Sender<Event>) -> Session {
    let config = NSURLSessionConfiguration::ephemeralSessionConfiguration();
    config.setHTTPShouldSetCookies(false);
    config.setHTTPCookieStorage(None);
    config.setHTTPCookieAcceptPolicy(NSHTTPCookieAcceptPolicy::Never);
    config.setURLCache(None);
    config.setRequestCachePolicy(NSURLRequestCachePolicy::ReloadIgnoringLocalCacheData);
    config.setTimeoutIntervalForRequest(REQUEST_TIMEOUT_SECS);
    let delegate = UpdateDelegate::new(events);
    let queue = NSOperationQueue::new();
    queue.setMaxConcurrentOperationCount(1);
    // SAFETY: the delegate implements the protocols the session calls and is `'static`; the queue is serial.
    let session = unsafe {
        NSURLSession::sessionWithConfiguration_delegate_delegateQueue(
            &config,
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        )
    };
    Session { session, _delegate: delegate, _queue: queue }
}

fn request(url: &Url) -> Result<Retained<NSMutableURLRequest>, HttpError> {
    let text = format!("{}://{}{}", if url.https { "https" } else { "http" }, authority(url), url.path);
    let nsurl = NSURL::URLWithString(&NSString::from_str(&text))
        .ok_or_else(|| HttpError::Refused(format!("the system cannot parse the URL: {text}")))?;
    let req = NSMutableURLRequest::requestWithURL(&nsurl);
    req.setHTTPShouldHandleCookies(false);
    req.setCachePolicy(NSURLRequestCachePolicy::ReloadIgnoringLocalCacheData);
    req.setValue_forHTTPHeaderField(Some(&NSString::from_str(USER_AGENT)), &NSString::from_str("User-Agent"));
    req.setValue_forHTTPHeaderField(Some(&NSString::from_str(ACCEPT)), &NSString::from_str("Accept"));
    // CFNetwork adds `Accept-Language` from the user's language list, which would tell a server the owner's locale
    // (2.3); an empty value replaces it. `Connection` and `Accept-Encoding` are the system's and say nothing of the user.
    req.setValue_forHTTPHeaderField(Some(&NSString::from_str("")), &NSString::from_str("Accept-Language"));
    Ok(req)
}

/// `host[:port]`, the port left out when it is the scheme's default.
fn authority(url: &Url) -> String {
    let default = if url.https { 443 } else { 80 };
    if url.port == default { url.host.clone() } else { format!("{}:{}", url.host, url.port) }
}

impl MacHttp {
    #[allow(clippy::too_many_arguments)] // one request: where, the bound, the sink, and the three ways out
    fn hop(
        url: &Url,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
        written: &mut u64,
    ) -> Result<Hop, HttpError> {
        let (tx, rx): (Sender<Event>, Receiver<Event>) = channel();
        let session = session(tx);
        let req = request(url)?;
        // SAFETY-free: a safe call on a live session and request.
        let task = session.session.dataTaskWithRequest(&req);
        task.resume();
        let mut answered = false;
        loop {
            if cancel.load(Ordering::Acquire) {
                task.cancel();
                return Err(HttpError::Cancelled);
            }
            let event = match rx.recv_timeout(CANCEL_POLL) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Err(HttpError::Network("the session closed".into())),
            };
            match event {
                Event::Response { status, location } => {
                    if is_redirect(status) {
                        task.cancel();
                        return match location {
                            Some(l) => Ok(Hop::Redirect(l)),
                            None => Err(HttpError::Refused("a redirect without a Location".into())),
                        };
                    }
                    if status != 200 {
                        task.cancel();
                        return Err(HttpError::Status(status));
                    }
                    answered = true;
                }
                Event::Data(bytes) => {
                    if !answered {
                        continue;
                    }
                    let room = max_bytes.saturating_sub(*written);
                    let take = (bytes.len() as u64).min(room) as usize;
                    if let Err(e) = sink.write_all(&bytes[..take]) {
                        task.cancel();
                        return Err(HttpError::Network(format!("the sink: {e}")));
                    }
                    *written += take as u64;
                    progress(*written);
                    if take < bytes.len() {
                        task.cancel();
                        return Err(HttpError::TooLarge);
                    }
                }
                Event::Done(None) => return Ok(Hop::Body),
                Event::Done(Some(words)) => {
                    return Err(if cancel.load(Ordering::Acquire) {
                        HttpError::Cancelled
                    } else {
                        HttpError::Network(words)
                    });
                }
            }
        }
    }
}

impl HttpGet for MacHttp {
    fn get(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), HttpError> {
        let mut at = check_url(url, self.policy)?;
        let mut written = 0u64;
        for _ in 0..=MAX_REDIRECTS {
            match Self::hop(&at, max_bytes, sink, progress, cancel, &mut written)? {
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
