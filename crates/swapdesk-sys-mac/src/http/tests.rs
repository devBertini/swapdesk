//! UPDATES.md 14 U3 on macOS: NSURLSession against a loopback server whose request bytes the tests read.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use super::*;

/// What the server does on one connection, after it read the request.
type Respond = Box<dyn FnOnce(&mut TcpStream) + Send>;

/// One response per connection, by the order the connections come; each request's bytes go to the receiver.
fn serve(responses: Vec<Respond>) -> (u16, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for respond in responses {
            let Ok((mut stream, _)) = listener.accept() else { return };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&req).into_owned());
            respond(&mut stream);
        }
    });
    (port, rx)
}

fn body(bytes: Vec<u8>) -> Respond {
    Box::new(move |s| {
        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
        let _ = s.write_all(&bytes);
    })
}

fn redirect(to: String, extra: &'static str) -> Respond {
    Box::new(move |s| {
        let _ =
            write!(s, "HTTP/1.1 302 Found\r\nLocation: {to}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n");
    })
}

/// CFNetwork reads the app's ATS settings from the main bundle on the first session of a process, and for a test
/// binary the "bundle" is its folder (`target/debug/deps`), which it scans whole: about ten seconds, once per process.
/// A time-bound test warms it up before the clock starts; an app bundle's folder is small and pays nothing.
fn warm_up() {
    drop(objc2_foundation::NSURLSessionConfiguration::ephemeralSessionConfiguration());
}

fn get(url: &str, max: u64, cancel: &AtomicBool) -> (Result<(), HttpError>, Vec<u8>) {
    let mut sink = Vec::new();
    let r = MacHttp::for_loopback_tests().get(url, max, &mut sink, &mut |_| {}, cancel);
    (r, sink)
}

#[test]
fn a_whole_body_within_its_bound_arrives() {
    let (port, _rx) = serve(vec![body(b"hello".to_vec())]);
    let (r, sink) = get(&format!("http://127.0.0.1:{port}/a"), 100, &AtomicBool::new(false));
    assert_eq!(r, Ok(()));
    assert_eq!(sink, b"hello");
}

#[test]
fn a_body_longer_than_the_bound_is_cut_and_reported() {
    let (port, _rx) = serve(vec![body(vec![7u8; 10_000])]);
    let (r, sink) = get(&format!("http://127.0.0.1:{port}/a"), 4096, &AtomicBool::new(false));
    assert_eq!(r, Err(HttpError::TooLarge));
    assert_eq!(sink.len(), 4096, "nothing past the bound is written");
}

#[test]
fn cancel_stops_a_download_within_a_second() {
    // A server that promises 10 MB and sends 1 KB every 100 ms.
    let (port, _rx) = serve(vec![Box::new(|s: &mut TcpStream| {
        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: 10485760\r\n\r\n");
        for _ in 0..600 {
            if s.write_all(&[1u8; 1024]).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })]);
    warm_up();
    let cancel = AtomicBool::new(false);
    let (r, cancelled_at, returned_at) = std::thread::scope(|scope| {
        let at = scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(400));
            cancel.store(true, Ordering::Release);
            Instant::now()
        });
        let (r, _) = get(&format!("http://127.0.0.1:{port}/slow"), u64::MAX, &cancel);
        let returned_at = Instant::now();
        (r, at.join().unwrap(), returned_at)
    });
    assert_eq!(r, Err(HttpError::Cancelled));
    let took = returned_at.saturating_duration_since(cancelled_at);
    assert!(took < Duration::from_secs(1), "the GET ended {took:?} after the cancel");
}

#[test]
fn a_redirect_from_https_to_http_is_refused() {
    // The loopback test policy allows http to 127.0.0.1 only: a redirect to any other http URL is refused like the
    // https-to-http redirect it stands for (production allows no http at all: `check_url`'s own test).
    let (port, _rx) = serve(vec![redirect("http://example.com/x".into(), "")]);
    let (r, sink) = get(&format!("http://127.0.0.1:{port}/a"), 100, &AtomicBool::new(false));
    assert!(matches!(r, Err(HttpError::Refused(_))), "{r:?}");
    assert!(sink.is_empty());
}

#[test]
fn more_than_five_redirects_are_refused() {
    // Seven redirects in a row on the same server: the sixth is one too many.
    let (port, _rx) = serve((0..7).map(|i| redirect(format!("/r{i}"), "")).collect());
    let (r, _) = get(&format!("http://127.0.0.1:{port}/start"), 100, &AtomicBool::new(false));
    assert_eq!(r, Err(HttpError::TooManyRedirects));
}

#[test]
fn no_cookie_or_identifier_header_is_sent() {
    // The first answer sets a cookie and redirects to the same server: the second request must not carry it.
    let (port, rx) = serve(vec![redirect("/b".into(), "Set-Cookie: track=1\r\n"), body(b"ok".to_vec())]);
    let (r, _) = get(&format!("http://127.0.0.1:{port}/a"), 100, &AtomicBool::new(false));
    assert_eq!(r, Ok(()));
    let requests: Vec<String> = rx.try_iter().collect();
    assert_eq!(requests.len(), 2);
    let who: Vec<String> = ["USER", "LOGNAME", "HOSTNAME"].iter().filter_map(|v| std::env::var(v).ok()).collect();
    for req in &requests {
        let lower = req.to_ascii_lowercase();
        assert!(!lower.contains("cookie"), "no cookie: {req}");
        assert!(lower.contains("\r\nuser-agent: swapdesk\r\n"), "the constant user agent: {req}");
        for w in &who {
            assert!(!lower.contains(&w.to_ascii_lowercase()), "no machine or account name ({w}): {req}");
        }
        for line in lower.lines().filter(|l| l.starts_with("accept-language:")) {
            assert!(line["accept-language:".len()..].trim().is_empty(), "no locale: {req}");
        }
        for h in ["authorization", "if-none-match", "x-"] {
            assert!(!lower.contains(&format!("\r\n{h}")), "no {h} header: {req}");
        }
    }
}

/// UPDATES.md 14 U3: the real GitHub path, through its redirect, once by hand (`--run-ignored only`).
#[test]
#[ignore = "network: fetches a published release asset from github.com"]
fn fetches_a_published_asset_through_the_redirect() {
    let url = "https://github.com/devBertini/swapdesk/releases/download/v1.0.0/Swapdesk-windows-x64-1.0.0.zip.sha256";
    let mut sink = Vec::new();
    let r = MacHttp::new().get(url, 4096, &mut sink, &mut |_| {}, &AtomicBool::new(false));
    assert_eq!(r, Ok(()));
    let text = String::from_utf8(sink).unwrap();
    println!("{text}");
    let hash = text.split_whitespace().next().unwrap_or_default();
    assert!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "{text}");
    assert!(text.contains("Swapdesk-windows-x64-1.0.0.zip"), "{text}");
}
