//! The development harness (cargo feature `dev-harness`, never in a release library): the scripted viewer of
//! `swapdesk-crossdrive`, the one the cross-machine proofs run on a headless Mac, driven here through the phone's own
//! backend. It adds a computer by its address, pairs with it, opens a session without a picture, ends it and
//! forgets the computer, with no human touch.
//!
//! It meets its orchestrator (a script on the development PC) as the Mac's does: markers on the system log
//! (`VIEWER_CODE 123 456`, ...) and two files in `<app folder>/harness`, `peer_code` (the computer's code, written
//! by the orchestrator after it compared the two) and `go`. It confirms a pairing code only on that word.

use std::time::{Duration, Instant};

use swapdesk_crossdrive::{Media, ViewerConfig, viewer_drive};
use swapdesk_lan::guard::approved_addrs;
use swapdesk_platform::{DecodedFrame, Level, NetInterfaces, Result as PlatformResult, VideoDecoder};
use swapdesk_proto::VideoHeader;
use swapdesk_sys_android::logcat;

/// The harness runs without a picture until the phone has its decoder (slice A4).
struct NoDecoder;

impl VideoDecoder for NoDecoder {
    fn decode(&mut self, _header: &VideoHeader, _annex_b: &[u8]) -> PlatformResult<Option<DecodedFrame>> {
        Ok(None)
    }

    fn reset(&mut self) {}
}

/// `DevHarness.run`: starts the script named by `spec` on a thread of its own and returns at once. `spec` is
/// `viewer <address of the computer>` ("viewer 10.0.2.2:47850"). The last marker is `PHONE_RESULT PASS` or
/// `PHONE_RESULT FAIL <why>`.
pub fn run(spec: &str) -> String {
    let mut words = spec.split_whitespace();
    let (Some("viewer"), Some(host)) = (words.next(), words.next()) else {
        return format!("HARNESS FAIL: {spec:?} is not \"viewer <address>\"");
    };
    let host = host.to_owned();
    let Some((backend, dir)) = crate::startup::app().as_ref().map(|a| (a.service.backend(), a.dir.join("harness")))
    else {
        return "HARNESS FAIL: the core is not started".into();
    };
    let spawned = std::thread::Builder::new().name("swapdesk-harness".into()).spawn(move || {
        let log = |line: &str| logcat::write(Level::Info, line);
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return log(&format!("PHONE_RESULT FAIL the harness folder: {e}"));
        }
        // The app has just started: the script begins once Java has reported a network the guard approves.
        let waited = Instant::now();
        let approved = || approved_addrs(&crate::startup::networks().list().unwrap_or_default());
        while approved().is_empty() {
            if waited.elapsed() > Duration::from_secs(15) {
                return log("PHONE_RESULT FAIL no approved network after 15 s");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut cfg = ViewerConfig::new(&host, dir.join("peer_code"), dir.join("go"), dir.join("anchor"));
        cfg.no_media = true;
        cfg.send_input = false;
        cfg.hold = Duration::from_secs(2);
        let media = Media::new(Box::new(|| Box::new(NoDecoder)));
        let (_report, result) = viewer_drive(&backend, &media, &cfg, &log);
        log(&match result {
            Ok(()) => "PHONE_RESULT PASS".to_owned(),
            Err(why) => format!("PHONE_RESULT FAIL {why}"),
        });
    });
    match spawned {
        Ok(_) => "HARNESS started".into(),
        Err(e) => format!("HARNESS FAIL: {e}"),
    }
}
