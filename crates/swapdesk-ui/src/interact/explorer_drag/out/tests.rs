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

#[test]
fn folders_only_leaving_the_window_say_why_in_the_caption_until_the_pointer_is_back() {
    let mut s = opened_11();
    // "Contratos" is the right pane's only folder (row 0).
    drag(&mut s, row(true, 0), None, false);
    assert_eq!(s.explorer_drag().unwrap().caption, None, "inside the window nothing is said");
    let a = s.drag_left_window();
    assert_eq!(a.effect, None);
    assert!(a.changed.hits(crate::state::WindowKind::Viewer), "the caption repaints the window");
    assert!(s.drag_active(), "the in-app drag goes on");
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.caption.as_deref(), Some("Pastas ainda não podem ser arrastadas para fora do app."));
    assert!(v.caption_is_reason && v.target.is_none() && !v.copy);
    // Back inside the window: the reason goes, and the pointer over a target reads as before (a folder of the other
    // computer cannot be copied down in 1.1 either: that reason, not this one).
    s.drag_move(Some(row(false, 0)), (100.0, 120.0), false, 2_000);
    let v = s.explorer_drag().unwrap();
    assert_ne!(v.caption.as_deref(), Some("Pastas ainda não podem ser arrastadas para fora do app."));
    assert!(v.caption.is_some() && v.caption_is_reason, "{v:?}");
    // The left pane's folders say nothing: they are the system's own files.
    let mut s = opened_11();
    drag(&mut s, row(false, 0), None, false);
    s.drag_left_window();
    assert_eq!(s.explorer_drag().unwrap().caption, None);
}

#[test]
fn files_and_folders_mixed_leaving_the_window_drag_the_files_and_leave_the_folders() {
    let mut s = opened_11();
    s.set_staging_root(temp_root("mixed"));
    // Row 0 "Contratos" (a folder) and row 1 "foto.jpg" selected, dragged by the file.
    s.activate(row(true, 0));
    s.set_pointer_mods(crate::PointerMods { toggle: true, ..crate::PointerMods::default() });
    s.activate(row(true, 1));
    drag(&mut s, row(true, 1), None, false);
    assert_eq!(s.explorer_drag().unwrap().pill, "2 itens");
    let a = s.drag_left_window();
    assert_eq!(a.effect, Some(Effect::SystemDrag), "the files go");
    let out = s.take_system_drag().expect("the drag out");
    assert_eq!(out.files().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["foto.jpg"]);
    assert!(!s.drag_active() && s.explorer_drag().is_none(), "no refusal is shown: the drag became the system's");
}

#[test]
fn a_drag_out_keeps_only_names_that_are_one_valid_component_and_never_removes_outside_staging() {
    let root = temp_root("hostile");
    std::fs::create_dir_all(&root).unwrap();
    let staging = root.join("1");
    let victim = root.join("victim.txt");
    std::fs::write(&victim, b"the user's thesis").unwrap();
    let file = |name: &str| DragOutFile { name: name.into(), size: 1, mtime_ms: 0 };
    let out = DragOut::new(
        1,
        String::new(),
        vec![
            file("../victim.txt"),
            file(r"..\victim.txt"),
            file(r"C:\victim.txt"),
            file("/victim.txt"),
            file("fine.bin"),
        ],
        staging.clone(),
    );
    let names: Vec<&str> = out.files().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["fine.bin"], "a name that is a path is not promised");
    // The slot of the honest file never reaches the victim: a failed transfer removes only inside staging.
    out.request(0).unwrap();
    wait_until(|| staging.is_dir());
    assert_eq!(
        out.on_transfer(5, 1, "../victim.txt", false, &TransferState::Failed { reason: ApiCode::Invalid }),
        None
    );
    assert!(victim.exists(), "the file outside staging stays");
    // A name the engine saved it as that climbs out fails the read instead of handing out a path outside.
    assert_eq!(
        out.on_transfer(7, 1, "fine.bin", false, &TransferState::Running { done: 1, total: 2, bytes_per_s: 1 }),
        None
    );
    out.on_saved_as(7, "../victim.txt");
    out.on_transfer(7, 1, "fine.bin", false, &TransferState::Done);
    assert_eq!(out.wait_bytes(0, 0), Err(PullError::Failed));
    assert!(victim.exists());
    let _ = std::fs::remove_dir_all(root);
}
