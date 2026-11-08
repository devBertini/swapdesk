//! F-FILE-01 over a real QUIC session on loopback (the session harness of `swapdesk-session`, copied into
//! `tests/common`): the link is killed in the middle of an upload, the viewer reconnects, offers the file again and the
//! host continues from its verified length; the final SHA-256 matches. The same for a download.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Quic, TestUi, eventually, lock, registry_one, run, viewer_config};
use ring::digest::{SHA256, digest};
use swapdesk_files::{Command, Event, FilesHandler, FilesSink};
use swapdesk_proto::PermissionSet;

/// Collects events; may close a connection when a transfer passes a byte count (the killed link).
#[derive(Default)]
struct Sink {
    events: Mutex<Vec<Event>>,
    kill: Mutex<Option<(quinn::Connection, u64)>>,
}

impl Sink {
    fn has(&self, f: impl Fn(&Event) -> bool) -> bool {
        lock(&self.events).iter().any(f)
    }
}

impl FilesSink for Sink {
    fn event(&self, event: Event) {
        if let Event::Progress { done, .. } = &event
            && let Some((conn, at)) = lock(&self.kill).as_ref()
            && done >= at
        {
            conn.close(7u32.into(), b"link killed by the test");
        }
        lock(&self.events).push(event);
    }
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-files-quic-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn data(len: usize) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect()
}

fn sha(d: &[u8]) -> Vec<u8> {
    digest(&SHA256, d).as_ref().to_vec()
}

async fn command_when_live(h: &FilesHandler, cmd: Command) {
    let mut tries = 0;
    while !h.command(cmd.clone()) {
        tries += 1;
        assert!(tries < 400, "the session never became live");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const SIZE: usize = 16 * 1024 * 1024;
const KILL_AT: u64 = 6 * 1024 * 1024;

#[test]
fn an_upload_resumes_after_the_quic_link_is_killed_midway() {
    run(async {
        let (src, host_dir) = (temp("up-src"), temp("up-host"));
        let content = data(SIZE);
        std::fs::write(src.join("big.bin"), &content).unwrap();

        let host_sink = Arc::new(Sink::default());
        let host_files = FilesHandler::new(host_dir.clone(), Some(host_dir.clone()), host_sink.clone());
        let ui = TestUi::accepting(PermissionSet::VIEW | PermissionSet::FILES);
        let q = Quic::start(ui, registry_one(host_files.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::VIEW | PermissionSet::FILES, None);

        let viewer_sink = Arc::new(Sink::default());
        let viewer_files = FilesHandler::new(src.clone(), None, viewer_sink.clone());

        // Session 1: the upload starts, and the link dies when 6 MiB were written.
        let mut cfg = viewer_config(PermissionSet::VIEW | PermissionSet::FILES);
        cfg.registry = registry_one(viewer_files.clone());
        let s1 = q.open(&v, cfg).await.unwrap();
        *lock(&viewer_sink.kill) = Some((s1.ctx().connection().unwrap().clone(), KILL_AT));
        command_when_live(&viewer_files, Command::SendFile { id: 1, path: src.join("big.bin"), dest: None }).await;
        tokio::time::timeout(Duration::from_secs(30), s1.ctx().wait_ended())
            .await
            .expect("the killed link ends the session");
        eventually(Duration::from_secs(10), "both sides to drop the dead session", || {
            host_files.active() == 0 && viewer_files.active() == 0
        })
        .await;
        assert!(!host_sink.has(|e| matches!(e, Event::Received { .. })), "the file must not have arrived whole");
        let part = std::fs::metadata(host_dir.join("big.bin.swapdesk-part")).expect("the partial file is kept").len();
        assert!(part > 0 && part < SIZE as u64, "partial length {part}");

        // Session 2: reconnect, offer the same file again.
        *lock(&viewer_sink.kill) = None;
        let mut cfg = viewer_config(PermissionSet::VIEW | PermissionSet::FILES);
        cfg.registry = registry_one(viewer_files.clone());
        let s2 = q.open(&v, cfg).await.unwrap();
        command_when_live(&viewer_files, Command::SendFile { id: 1, path: src.join("big.bin"), dest: None }).await;
        eventually(Duration::from_secs(60), "the host to verify the file", || {
            host_sink.has(|e| matches!(e, Event::Received { .. }))
        })
        .await;
        eventually(Duration::from_secs(10), "the sender to hear FileVerified", || {
            viewer_sink.has(|e| matches!(e, Event::Sent { id: 1 }))
        })
        .await;

        let resume_from = lock(&host_sink.events)
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Incoming { resume_from, .. } => Some(*resume_from),
                _ => None,
            })
            .unwrap();
        assert!(resume_from > 0 && resume_from <= part, "resumed from {resume_from}, partial was {part}");
        let got = std::fs::read(host_dir.join("big.bin")).unwrap();
        assert_eq!(sha(&got), sha(&content), "SHA-256 of the received file");
        assert!(!host_dir.join("big.bin.swapdesk-part").exists() && !host_dir.join("big.bin.swapdesk-meta").exists());
        drop(s2);
        q.stop();
        let _ = (std::fs::remove_dir_all(&src), std::fs::remove_dir_all(&host_dir));
    });
}

#[test]
fn a_download_resumes_after_the_quic_link_is_killed_midway() {
    run(async {
        let (host_files_dir, save) = (temp("dl-host"), temp("dl-save"));
        let content = data(SIZE);
        std::fs::write(host_files_dir.join("movie.bin"), &content).unwrap();

        let host_sink = Arc::new(Sink::default());
        // The host kills the link when it has sent 6 MiB; its sink sees its own progress.
        let host_files = FilesHandler::new(host_files_dir.clone(), None, host_sink.clone());
        let ui = TestUi::accepting(PermissionSet::VIEW | PermissionSet::FILES);
        let q = Quic::start(ui, registry_one(host_files.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::VIEW | PermissionSet::FILES, None);
        let viewer_sink = Arc::new(Sink::default());
        let viewer_files = FilesHandler::new(save.clone(), None, viewer_sink.clone());
        let get = host_files_dir.join("movie.bin").to_string_lossy().into_owned();

        let mut cfg = viewer_config(PermissionSet::VIEW | PermissionSet::FILES);
        cfg.registry = registry_one(viewer_files.clone());
        let s1 = q.open(&v, cfg).await.unwrap();
        // The viewer kills the link when it has received 6 MiB.
        *lock(&viewer_sink.kill) = Some((s1.ctx().connection().unwrap().clone(), KILL_AT));
        command_when_live(&viewer_files, Command::Get { id: 5, path: get.clone(), save_dir: save.clone() }).await;
        tokio::time::timeout(Duration::from_secs(30), s1.ctx().wait_ended())
            .await
            .expect("the killed link ends the session");
        eventually(Duration::from_secs(10), "both sides to drop the dead session", || {
            host_files.active() == 0 && viewer_files.active() == 0
        })
        .await;
        assert!(!viewer_sink.has(|e| matches!(e, Event::Received { .. })));
        let part = std::fs::metadata(save.join("movie.bin.swapdesk-part")).expect("the partial file is kept").len();
        assert!(part > 0 && part < SIZE as u64, "partial length {part}");

        *lock(&viewer_sink.kill) = None;
        let mut cfg = viewer_config(PermissionSet::VIEW | PermissionSet::FILES);
        cfg.registry = registry_one(viewer_files.clone());
        let s2 = q.open(&v, cfg).await.unwrap();
        command_when_live(&viewer_files, Command::Get { id: 5, path: get, save_dir: save.clone() }).await;
        eventually(Duration::from_secs(60), "the viewer to verify the file", || {
            viewer_sink.has(|e| matches!(e, Event::Received { .. }))
        })
        .await;
        let resume_from = lock(&viewer_sink.events)
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Incoming { resume_from, .. } => Some(*resume_from),
                _ => None,
            })
            .unwrap();
        assert!(resume_from > 0 && resume_from <= part, "resumed from {resume_from}, partial was {part}");
        assert_eq!(sha(&std::fs::read(save.join("movie.bin")).unwrap()), sha(&content));
        drop(s2);
        q.stop();
        let _ = (std::fs::remove_dir_all(&host_files_dir), std::fs::remove_dir_all(&save));
    });
}
