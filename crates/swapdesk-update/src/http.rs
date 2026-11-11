//! The HTTPS GET the updater needs (UPDATES.md 5), behind one trait the system clients implement: WinHTTP in
//! `swapdesk-sys-win`, `NSURLSession` in `swapdesk-sys-mac`. What is policy lives here, OS-free and tested: which URLs
//! may be fetched, how a redirect is judged, the bound on a body, and the headers a request carries (2.3).

use std::fmt;
use std::sync::atomic::AtomicBool;

/// At most this many redirects in one GET (UPDATES.md 5).
pub const MAX_REDIRECTS: u8 = 5;

/// The only headers a request carries besides `Host` (UPDATES.md 2.3): a constant user agent says the least.
pub const USER_AGENT: &str = "Swapdesk";
pub const ACCEPT: &str = "*/*";

/// Which URLs a client may fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlPolicy {
    /// Production: `https` only.
    HttpsOnly,
    /// Tests: `https`, and `http` to `127.0.0.1` only (a loopback server).
    HttpsOrLoopbackHttp,
}

/// A URL a client may fetch: scheme, host, port and the path with its query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Starts with `/`.
    pub path: String,
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.https { "https" } else { "http" };
        write!(f, "{scheme}://{}:{}{}", self.host, self.port, self.path)
    }
}

/// Why a GET did not end with a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The URL (or a redirect's target) is not one the policy allows: not https, or malformed.
    Refused(String),
    /// More than [`MAX_REDIRECTS`] redirects.
    TooManyRedirects,
    /// The body is longer than the bound the caller gave; what came before the bound was written.
    TooLarge,
    /// The caller cancelled.
    Cancelled,
    /// The server answered with this status (not 200, not a redirect).
    Status(u16),
    /// The system's client failed (name resolution, TLS, the connection), with its words.
    Network(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(why) => write!(f, "refused: {why}"),
            Self::TooManyRedirects => write!(f, "more than {MAX_REDIRECTS} redirects"),
            Self::TooLarge => write!(f, "the body is longer than its bound"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Status(s) => write!(f, "HTTP status {s}"),
            Self::Network(e) => write!(f, "network: {e}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// One GET, redirects followed under the policy: the body goes to `sink`, at most `max_bytes` of it (more is
/// `TooLarge`); `progress` hears the bytes written so far; `cancel` set from another thread ends it within a second.
pub trait HttpGet: Send + Sync {
    fn get(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), HttpError>;
}

/// Parses `url` and checks it against `policy`.
pub fn check_url(url: &str, policy: UrlPolicy) -> Result<Url, HttpError> {
    let refused = |why: &str| Err(HttpError::Refused(format!("{why}: {url}")));
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return refused("not an http(s) URL");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') || path.contains('#') || url.chars().any(char::is_whitespace) {
        return refused("malformed");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(p) if p != 0 => (h, p),
            _ => return refused("bad port"),
        },
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return refused("bad host");
    }
    if !https && !(policy == UrlPolicy::HttpsOrLoopbackHttp && host == "127.0.0.1") {
        return refused("not https");
    }
    Ok(Url { https, host: host.to_ascii_lowercase(), port, path: path.to_string() })
}

/// The target of a redirect from `from`: an absolute URL, or a path on the same origin; then checked against
/// `policy`, so a redirect from https to http is refused like any http URL.
pub fn redirect_target(from: &Url, location: &str, policy: UrlPolicy) -> Result<Url, HttpError> {
    if location.starts_with('/') && !location.starts_with("//") {
        let scheme = if from.https { "https" } else { "http" };
        return check_url(&format!("{scheme}://{}:{}{location}", from.host, from.port), policy);
    }
    check_url(location, policy)
}

/// Whether `status` is a redirect the client follows.
pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_fetches_https_only_and_tests_add_loopback_http() {
        let u =
            check_url("https://github.com/devBertini/swapdesk/releases/latest/download/x.txt", UrlPolicy::HttpsOnly)
                .unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port), (true, "github.com", 443));
        assert_eq!(u.path, "/devBertini/swapdesk/releases/latest/download/x.txt");
        for bad in ["http://github.com/x", "ftp://a/b", "https://", "https://user@h/x", "https://h:0/", "https://h/x#f"]
        {
            assert!(matches!(check_url(bad, UrlPolicy::HttpsOnly), Err(HttpError::Refused(_))), "{bad}");
        }
        assert!(check_url("http://127.0.0.1:8080/a", UrlPolicy::HttpsOnly).is_err());
        assert!(check_url("http://127.0.0.1:8080/a", UrlPolicy::HttpsOrLoopbackHttp).is_ok());
        assert!(check_url("http://10.0.0.1/a", UrlPolicy::HttpsOrLoopbackHttp).is_err(), "loopback only");
    }

    #[test]
    fn a_redirect_is_judged_like_a_url_and_https_to_http_is_refused() {
        let from = check_url("https://github.com/a", UrlPolicy::HttpsOnly).unwrap();
        let to = redirect_target(&from, "https://objects.githubusercontent.com/b?x=1", UrlPolicy::HttpsOnly).unwrap();
        assert_eq!((to.host.as_str(), to.path.as_str()), ("objects.githubusercontent.com", "/b?x=1"));
        assert_eq!(redirect_target(&from, "/c", UrlPolicy::HttpsOnly).unwrap().to_string(), "https://github.com:443/c");
        assert!(redirect_target(&from, "http://github.com/c", UrlPolicy::HttpsOnly).is_err(), "https to http");
        assert!(redirect_target(&from, "//evil/c", UrlPolicy::HttpsOnly).is_err(), "scheme-relative is refused");
        assert!(is_redirect(302) && is_redirect(308) && !is_redirect(200) && !is_redirect(304));
    }
}
