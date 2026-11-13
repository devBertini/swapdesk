//! Slice 5: the state's decisions for a drag out to the system's file manager (FILES-UI.md 4.3 and 10).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::*;
use crate::api::{ApiCode, TransferState, UiEvent};
use crate::interact::explorer_drag::tests::{drag, opened_11, row};

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("swapdesk-dragout-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn transfer(s: &mut UiState, id: u32, name: &str, state: TransferState) {
    s.apply(UiEvent::Transfer { transfer_id: id, session_id: 1, name: name.into(), outgoing: false, state });
}

/// A drag of the right pane's "foto.jpg" that left the window.
fn out_drag(tag: &str) -> (UiState, Arc<DragOut>, PathBuf) {
    let mut s = opened_11();
    let root = temp_root(tag);
    s.set_staging_root(root.clone());
    drag(&mut s, row(true, 1), None, false);
    let a = s.drag_left_window();
    assert_eq!(a.effect, Some(Effect::SystemDrag));
    let out = s.take_system_drag().expect("the drag out");
    (s, out, root)
}

fn session_of(s: &UiState) -> u64 {
    s.session_id().unwrap()
}

#[test]
fn a_drag_from_the_right_pane_leaving_the_window_becomes_a_system_drag() {
    let (s, out, root) = out_drag("becomes");
    assert!(!s.drag_active(), "the in-app drag ended");
    assert_eq!(out.files(), &[DragOutFile { name: "foto.jpg".into(), size: 10, mtime_ms: 0 }]);
    assert!(out.dir().starts_with(&root));

    // A folder among the items is left out; a drag of folders only has nothing to promise.
    let mut s = opened_11();
    s.set_staging_root(root);
    drag(&mut s, row(true, 0), None, false);
    let a = s.drag_left_window();
    assert_eq!(a.effect, None);
    assert!(s.drag_active(), "the in-app drag goes on");
    assert!(s.take_system_drag().is_none());
}

#[test]
fn a_drag_from_the_left_pane_leaving_the_window_does_nothing_new() {
    let mut s = opened_11();
    drag(&mut s, row(false, 1), None, false);
    let a = s.drag_left_window();
    assert_eq!(a, Activation::default());
    assert!(s.drag_active());
    assert!(s.take_system_drag().is_none());
    // No drag at all: nothing either.
    let mut s = opened_11();
    assert_eq!(s.drag_left_window(), Activation::default());
}

#[test]
fn the_os_asking_for_a_file_downloads_it_into_staging() {
    let (mut s, out, root) = out_drag("asks");
    let woken = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let w = woken.clone();
    out.set_waker(Arc::new(move || {
        w.fetch_add(1, Ordering::Relaxed);
    }));
    assert!(s.system_drag_pump().commands.is_empty(), "nothing downloads before the system asks");

    out.request(0).unwrap();
    out.request(0).unwrap();
    wait_until(|| woken.load(Ordering::Relaxed) >= 1);
    assert_eq!(woken.load(Ordering::Relaxed), 1, "one wake for one ask");
    assert!(out.dir().is_dir(), "the staging folder is made on a worker");
    let a = s.system_drag_pump();
    let folder = s.there_folder();
    assert_eq!(
        a.commands,
        vec![Command::FilesGet {
            session_id: session_of(&s),
            remote_path: join(&folder, "foto.jpg"),
            local_dir: out.dir().to_path_buf()
        }]
    );
    assert!(s.system_drag_pump().commands.is_empty(), "asked once, downloaded once");

    // The bytes: only up to the verified-safe part while it runs, the whole file at the end.
    let (id, sid) = (7, session_of(&s));
    s.apply(UiEvent::Transfer {
        transfer_id: id,
        session_id: sid,
        name: "foto.jpg".into(),
        outgoing: false,
        state: TransferState::Running { done: 6, total: 10, bytes_per_s: 1 },
    });
    assert_eq!(out.wait_bytes(0, 4), Ok(Avail::Partial { path: out.part_path(0), limit: 6 }));
    transfer(&mut s, id, "foto.jpg", TransferState::Running { done: 10, total: 10, bytes_per_s: 1 });
    // The last byte waits for the verification: 9 of 10 are all a read may have.
    assert_eq!(out.wait_bytes(0, 9), Ok(Avail::Partial { path: out.part_path(0), limit: 9 }));
    transfer(&mut s, id, "foto.jpg", TransferState::Done);
    assert_eq!(out.wait_done(0), Ok(out.dir().join("foto.jpg")));

    // The system lets go: the staging folder goes away.
    std::fs::write(out.dir().join("foto.jpg"), b"0123456789").unwrap();
    out.release();
    s.system_drag_pump();
    wait_until(|| !root.join(out.dir().file_name().unwrap()).exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_failed_transfer_fails_the_promise_and_removes_the_staging_file() {
    let (mut s, out, root) = out_drag("fails");
    out.request(0).unwrap();
    wait_until(|| out.dir().is_dir());
    s.system_drag_pump();
    std::fs::write(out.part_path(0), b"012345").unwrap();
    std::fs::write(out.dir().join("foto.jpg.swapdesk-meta"), b"m").unwrap();
    transfer(&mut s, 3, "foto.jpg", TransferState::Running { done: 6, total: 10, bytes_per_s: 1 });
    // A read that waits for the end fails when the transfer fails (the SHA-256 differed).
    let waiting = out.clone();
    let reader = std::thread::spawn(move || waiting.wait_done(0));
    std::thread::sleep(Duration::from_millis(300));
    transfer(&mut s, 3, "foto.jpg", TransferState::Failed { reason: ApiCode::Invalid });
    assert_eq!(reader.join().unwrap(), Err(PullError::Failed));
    assert_eq!(out.wait_bytes(0, 1), Err(PullError::Failed));
    wait_until(|| !out.part_path(0).exists() && !out.dir().join("foto.jpg.swapdesk-meta").exists());

    // Released while a transfer runs: the reads fail and the transfer is cancelled.
    let (mut s, out, _) = out_drag("released");
    out.request(0).unwrap();
    wait_until(|| out.dir().is_dir());
    std::thread::sleep(Duration::from_millis(50));
    s.system_drag_pump();
    transfer(&mut s, 4, "foto.jpg", TransferState::Running { done: 1, total: 10, bytes_per_s: 1 });
    out.release();
    assert_eq!(out.wait_bytes(0, 5), Err(PullError::Released));
    let a = s.system_drag_pump();
    assert_eq!(a.commands, vec![Command::TransferCancel { transfer_id: 4 }]);
    let _ = std::fs::remove_dir_all(root);
}

fn wait_until(mut done: impl FnMut() -> bool) {
    let t = Instant::now();
    while !done() {
        assert!(t.elapsed() < Duration::from_secs(5), "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}
