//! The file flows through two engines joined by a function: no network, a link that can be cut at a byte count.
//! Flows: F-FILE-01 (transfer, resume after a lost link, hash check, name and size refusals, viewer accepts a file
//! only as the answer to its own request), F-FILE-02 (listing and download).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ring::digest::{SHA256, digest};
use swapdesk_files::{Engine, Event, FilesConfig, Out, Side};
use swapdesk_proto::{DirRequest, ErrorCode, FileDecline, FileDone, FileOffer, Message};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("swapdesk-files-flows-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn data(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

fn sha(d: &[u8]) -> [u8; 32] {
    digest(&SHA256, d).as_ref().try_into().unwrap()
}

/// A host engine and a viewer engine of one session.
struct World {
    host: Engine,
    viewer: Engine,
    host_events: Vec<Event>,
    viewer_events: Vec<Event>,
    /// Messages one side sent that the other answered with an error or a decline, for the assertions.
    to_viewer: Vec<Message>,
    to_host: Vec<Message>,
}

impl World {
    fn new(host_dir: Option<&Path>, home: &Path) -> Self {
        let mut hc = FilesConfig::new(Side::Host, home.to_path_buf());
        hc.default_dir = host_dir.map(Path::to_path_buf);
        Self {
            host: Engine::new(hc),
            viewer: Engine::new(FilesConfig::new(Side::Viewer, PathBuf::new())),
            host_events: vec![],
            viewer_events: vec![],
            to_viewer: vec![],
            to_host: vec![],
        }
    }

    /// Carries out `outs` that `from_viewer` side produced and everything they cause. `budget`: a data stream stops
    /// (without finishing) once this many bytes crossed in the call; the budget is shared by the call's streams.
    fn pump(&mut self, from_viewer: bool, outs: Vec<Out>, mut budget: Option<u64>) {
        let mut q: VecDeque<(bool, Out)> = outs.into_iter().map(|o| (from_viewer, o)).collect();
        while let Some((viewer_origin, out)) = q.pop_front() {
            match out {
                Out::Msg(m) => {
                    let replies = if viewer_origin {
                        self.to_host.push(m.clone());
                        self.host.on_message(m)
                    } else {
                        self.to_viewer.push(m.clone());
                        self.viewer.on_message(m)
                    };
                    q.extend(replies.into_iter().map(|o| (!viewer_origin, o)));
                }
                Out::Event(e) => {
                    if viewer_origin {
                        self.viewer_events.push(e);
                    } else {
                        self.host_events.push(e);
                    }
                }
                Out::Stop { .. } => {}
                Out::Send { id, from } => {
                    let (sender, receiver) = if viewer_origin {
                        (&mut self.viewer, &mut self.host)
                    } else {
                        (&mut self.host, &mut self.viewer)
                    };
                    let mut reader = sender.open_reader(id, from).expect("the sender opens its reader");
                    let mut offset = from;
                    let mut cut = false;
                    while let Some(chunk) = reader.next_chunk().unwrap() {
                        let chunk = match budget {
                            Some(0) => {
                                cut = true;
                                break;
                            }
                            Some(b) if (chunk.len() as u64) > b => {
                                cut = true;
                                budget = Some(0);
                                chunk[..usize::try_from(b).unwrap()].to_vec()
                            }
                            Some(b) => {
                                budget = Some(b - chunk.len() as u64);
                                chunk
                            }
                            None => chunk,
                        };
                        let n = chunk.len() as u64;
                        let outs = receiver.on_data(id, offset, &chunk);
                        offset += n;
                        q.extend(outs.into_iter().map(|o| (!viewer_origin, o)));
                        if cut {
                            break;
                        }
                    }
                    if cut {
                        let outs = receiver.data_broken(id);
                        q.extend(outs.into_iter().map(|o| (!viewer_origin, o)));
                        continue;
                    }
                    let sha = reader.sha256().unwrap();
                    let outs = sender.sent_all(id, sha);
                    q.extend(outs.into_iter().map(|o| (viewer_origin, o)));
                    let outs = receiver.data_ended(id);
                    q.extend(outs.into_iter().map(|o| (!viewer_origin, o)));
                }
            }
        }
    }

    /// The link died: both sides drop their live transfers.
    fn kill_link(&mut self) {
        let a = self.host.link_lost();
        let b = self.viewer.link_lost();
        self.host_events.extend(a.into_iter().filter_map(ev));
        self.viewer_events.extend(b.into_iter().filter_map(ev));
    }
}

fn ev(o: Out) -> Option<Event> {
    match o {
        Out::Event(e) => Some(e),
        _ => None,
    }
}

fn find_incoming(events: &[Event]) -> (u64, u64) {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Incoming { size, resume_from, .. } => Some((*size, *resume_from)),
            _ => None,
        })
        .expect("an Incoming event")
}

fn received_path(events: &[Event]) -> PathBuf {
    events
        .iter()
        .find_map(|e| match e {
            Event::Received { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("a Received event")
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
}

// ---------- F-FILE-01 ----------

#[test]
fn an_upload_arrives_whole_with_a_checked_hash() {
    let (src, dst) = (temp("up-src"), temp("up-dst"));
    let content = data(900_000, 1);
    std::fs::write(src.join("photo.bin"), &content).unwrap();
    let mut w = World::new(Some(&dst), &dst);
    let outs = w.viewer.send_file(7, &src.join("photo.bin"), None);
    w.pump(true, outs, None);
    assert_eq!(std::fs::read(received_path(&w.host_events)).unwrap(), content);
    assert!(w.viewer_events.contains(&Event::Sent { id: 7 }), "{:?}", w.viewer_events);
    assert_eq!(leftovers(&dst), ["photo.bin"], "no partial file or record is left");
    assert_eq!(w.host.active() + w.viewer.active(), 0);
    let _ = (std::fs::remove_dir_all(&src), std::fs::remove_dir_all(&dst));
}

#[test]
fn an_upload_resumes_after_the_link_is_killed_midway() {
    let (src, dst) = (temp("kill-src"), temp("kill-dst"));
    let content = data(5_000_000, 2);
    std::fs::write(src.join("big.bin"), &content).unwrap();
    let mut w = World::new(Some(&dst), &dst);

    // Session 1: 3.2 MB cross, then the link dies.
    let outs = w.viewer.send_file(1, &src.join("big.bin"), None);
    w.pump(true, outs, Some(3_200_000));
    w.kill_link();
    assert!(w.host_events.iter().all(|e| !matches!(e, Event::Received { .. })));
    let mut names = leftovers(&dst);
    names.sort();
    assert_eq!(names, ["big.bin.swapdesk-meta", "big.bin.swapdesk-part"]);

    // Session 2: new engines (a reconnect), the same file offered again.
    let mut w2 = World::new(Some(&dst), &dst);
    let outs = w2.viewer.send_file(1, &src.join("big.bin"), None);
    w2.pump(true, outs, None);
    let (size, resume_from) = find_incoming(&w2.host_events);
    assert_eq!(size, 5_000_000);
    assert!(resume_from > 0 && resume_from <= 3_200_000, "resumed from {resume_from}");
    let path = received_path(&w2.host_events);
    assert_eq!(sha(&std::fs::read(path).unwrap()), sha(&content), "the SHA-256 of the whole file matches");
    assert!(w2.viewer_events.contains(&Event::Sent { id: 1 }));
    assert_eq!(leftovers(&dst), ["big.bin"]);
    let _ = (std::fs::remove_dir_all(&src), std::fs::remove_dir_all(&dst));
}

#[test]
fn a_download_resumes_after_the_link_is_killed_midway() {
    let (host_files, save) = (temp("dl-host"), temp("dl-save"));
    let content = data(4_200_000, 3);
    let file = host_files.join("movie.bin");
    std::fs::write(&file, &content).unwrap();
    let get = file.to_string_lossy().into_owned();

    let mut w = World::new(None, &host_files);
    let outs = w.viewer.request_get(5, get.clone(), save.clone());
    w.pump(true, outs, Some(2_500_000));
    w.kill_link();

    let mut w2 = World::new(None, &host_files);
    let outs = w2.viewer.request_get(5, get, save.clone());
    w2.pump(true, outs, None);
    let (_, resume_from) = find_incoming(&w2.viewer_events);
    assert!(resume_from > 0, "the download continued from {resume_from}");
    assert_eq!(std::fs::read(received_path(&w2.viewer_events)).unwrap(), content);
    assert_eq!(leftovers(&save), ["movie.bin"]);
    let _ = (std::fs::remove_dir_all(&host_files), std::fs::remove_dir_all(&save));
}

#[test]
fn pause_then_retomar_continues_inside_the_session() {
    let (src, dst) = (temp("pause-src"), temp("pause-dst"));
    let content = data(3_000_000, 4);
    std::fs::write(src.join("p.bin"), &content).unwrap();
    let mut w = World::new(Some(&dst), &dst);
    let outs = w.viewer.send_file(2, &src.join("p.bin"), None);
    w.pump(true, outs, Some(1_000_000));
    let outs = w.viewer.pause(2);
    w.pump(true, outs, None);
    assert!(w.host_events.contains(&Event::Paused { id: 2 }));
    let outs = w.viewer.resume(2);
    w.pump(true, outs, None);
    assert_eq!(std::fs::read(received_path(&w.host_events)).unwrap(), content);
    let _ = (std::fs::remove_dir_all(&src), std::fs::remove_dir_all(&dst));
}

#[test]
fn the_receiver_can_pause_and_resume_too() {
    let (src, dst) = (temp("rpause-src"), temp("rpause-dst"));
    let content = data(2_400_000, 5);
    std::fs::write(src.join("r.bin"), &content).unwrap();
    let mut w = World::new(Some(&dst), &dst);
    let outs = w.viewer.send_file(3, &src.join("r.bin"), None);
    w.pump(true, outs, Some(900_000));
    let outs = w.host.pause(3);
    w.pump(false, outs, None);
    let outs = w.host.resume(3);
    w.pump(false, outs, None);
    assert_eq!(std::fs::read(received_path(&w.host_events)).unwrap(), content);
    let _ = (std::fs::remove_dir_all(&src), std::fs::remove_dir_all(&dst));
}

#[test]
fn cancel_removes_the_partial_file() {
    let (src, dst) = (temp("cancel-src"), temp("cancel-dst"));
    std::fs::write(src.join("c.bin"), data(1_000_000, 6)).unwrap();
    let mut w = World::new(Some(&dst), &dst);
    let outs = w.viewer.send_file(4, &src.join("c.bin"), None);
    w.pump(true, outs, Some(300_000));
    let outs = w.viewer.cancel(4);
    w.pump(true, outs, None);
    assert!(leftovers(&dst).is_empty(), "{:?}", leftovers(&dst));
    assert!(w.host_events.contains(&Event::Cancelled { id: 4 }));
    let _ = (std::fs::remove_dir_all(&src), std::fs::remove_dir_all(&dst));
}

#[test]
fn a_wrong_hash_is_refused_and_the_partial_file_deleted() {
    let dst = temp("badhash");
    let mut w = World::new(Some(&dst), &dst);
    let offer = FileOffer { transfer_id: 9, name: "x.bin".into(), size: 4, mtime_ms: 1, dest: None };
    let outs = w.host.on_message(Message::FileOffer(offer));
    assert!(outs.iter().any(|o| matches!(o, Out::Msg(Message::FileAccept(_)))));
    w.host.on_data(9, 0, b"abcd");
    let outs = w.host.on_message(Message::FileDone(FileDone { transfer_id: 9, sha256: sha(b"abcX") }));
    assert!(outs.iter().any(|o| matches!(o, Out::Msg(Message::FileVerified(v)) if !v.ok)));
    assert!(leftovers(&dst).is_empty());
    let _ = std::fs::remove_dir_all(&dst);
}

#[test]
fn a_stream_over_the_offered_size_fails_the_transfer() {
    let dst = temp("oversize");
    let mut w = World::new(Some(&dst), &dst);
    let offer = FileOffer { transfer_id: 9, name: "x.bin".into(), size: 4, mtime_ms: 1, dest: None };
    w.host.on_message(Message::FileOffer(offer));
    let outs = w.host.on_data(9, 0, b"abcdef");
    assert!(outs.iter().any(|o| matches!(o, Out::Event(Event::Failed { code: ErrorCode::TooLarge, .. }))), "{outs:?}");
    assert_eq!(w.host.active(), 0);
    assert!(leftovers(&dst).is_empty());
    let _ = std::fs::remove_dir_all(&dst);
}

#[test]
fn names_that_break_the_rules_are_declined_with_invalid_name_and_nothing_is_written() {
    let dst = temp("names");
    let mut w = World::new(Some(&dst), &dst);
    for (i, name) in ["..", "a/b", "a\\b", "C:\\x", "x:stream", "nul.txt", "con", "a\0b"].into_iter().enumerate() {
        let offer = FileOffer { transfer_id: i as u32 + 1, name: name.into(), size: 1, mtime_ms: 1, dest: None };
        let outs = w.host.on_message(Message::FileOffer(offer));
        assert!(
            matches!(
                outs.as_slice(),
                [Out::Msg(Message::FileDecline(FileDecline { code: ErrorCode::InvalidName, .. }))]
            ),
            "{name:?}: {outs:?}"
        );
    }
    assert!(leftovers(&dst).is_empty());
    assert_eq!(w.host.active(), 0);
    let _ = std::fs::remove_dir_all(&dst);
}

#[test]
fn an_offer_over_the_size_limit_is_too_large() {
    let dst = temp("limit");
    let mut cfg = FilesConfig::new(Side::Host, dst.clone());
    cfg.default_dir = Some(dst.clone());
    cfg.max_file_size = 1000;
    let mut host = Engine::new(cfg);
    let offer = FileOffer { transfer_id: 1, name: "x".into(), size: 1001, mtime_ms: 1, dest: None };
    let outs = host.on_message(Message::FileOffer(offer));
    assert!(matches!(outs.as_slice(), [Out::Msg(Message::FileDecline(FileDecline { code: ErrorCode::TooLarge, .. }))]));
    let _ = std::fs::remove_dir_all(&dst);
}

#[test]
fn a_viewer_takes_a_file_only_as_the_answer_to_its_own_request() {
    let save = temp("unsolicited");
    let mut w = World::new(None, &save);
    // The host pushes an offer nobody asked for.
    let offer = FileOffer {
        transfer_id: 77,
        name: "evil.exe".into(),
        size: 3,
        mtime_ms: 1,
        dest: Some(save.to_string_lossy().into_owned()),
    };
    let outs = w.viewer.on_message(Message::FileOffer(offer.clone()));
    assert!(
        matches!(outs.as_slice(), [Out::Msg(Message::FileDecline(FileDecline { code: ErrorCode::NotPermitted, .. }))]),
        "{outs:?}"
    );
    assert!(leftovers(&save).is_empty());

    // An offer with another id than the request is refused as well; the right one is accepted and `dest` ignored.
    let other = temp("unsolicited-dest");
    w.viewer.request_get(5, "irrelevant".into(), save.clone());
    let wrong = FileOffer { transfer_id: 6, ..offer.clone() };
    assert!(matches!(w.viewer.on_message(Message::FileOffer(wrong)).as_slice(), [Out::Msg(Message::FileDecline(_))]));
    let right =
        FileOffer { transfer_id: 5, name: "ok.bin".into(), dest: Some(other.to_string_lossy().into_owned()), ..offer };
    w.viewer.on_message(Message::FileOffer(right));
    w.viewer.on_data(5, 0, b"abc");
    assert!(leftovers(&other).is_empty(), "dest is ignored by a viewer");
    assert!(leftovers(&save).contains(&"ok.bin.swapdesk-part".to_string()));
    let _ = (std::fs::remove_dir_all(&save), std::fs::remove_dir_all(&other));
}

#[test]
fn a_host_honours_dest_and_declines_without_any_folder() {
    let (default, chosen) = (temp("dest-default"), temp("dest-chosen"));
    let src = temp("dest-src");
    std::fs::write(src.join("d.bin"), b"hello").unwrap();
    let mut w = World::new(Some(&default), &default);
    let outs = w.viewer.send_file(1, &src.join("d.bin"), Some(chosen.to_string_lossy().into_owned()));
    w.pump(true, outs, None);
    assert_eq!(leftovers(&chosen), ["d.bin"]);
    assert!(leftovers(&default).is_empty());

    let mut w = World::new(None, &default);
    let outs = w.viewer.send_file(1, &src.join("d.bin"), None);
    w.pump(true, outs, None);
    assert!(w.viewer_events.contains(&Event::Failed { id: 1, code: ErrorCode::Unavailable }), "{:?}", w.viewer_events);
    for d in [&default, &chosen, &src] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn an_existing_file_is_never_overwritten() {
    let (src, dst) = (temp("ow-src"), temp("ow-dst"));
    std::fs::write(src.join("a.txt"), b"new").unwrap();
    std::fs::write(dst.join("a.txt"), b"mine").unwrap();
    let mut w = World::new(Some(&dst), &dst);
    let outs = w.viewer.send_file(1, &src.join("a.txt"), None);
    w.pump(true, outs, None);
    assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"mine");
    assert_eq!(std::fs::read(dst.join("a (1).txt")).unwrap(), b"new");
    let _ = (std::fs::remove_dir_all(&src), std::fs::remove_dir_all(&dst));
}

#[test]
fn an_ads_or_reserved_name_cannot_be_sent_from_a_local_file_either() {
    let src = temp("sendnames");
    std::fs::write(src.join("nul.txt"), b"x").unwrap();
    let mut e = Engine::new(FilesConfig::new(Side::Viewer, PathBuf::new()));
    let outs = e.send_file(1, &src.join("nul.txt"), None);
    assert!(matches!(outs.as_slice(), [Out::Event(Event::Failed { code: ErrorCode::InvalidName, .. })]));
    let _ = std::fs::remove_dir_all(&src);
}

#[test]
fn a_sender_that_is_told_to_resume_past_the_end_stops_the_transfer() {
    let src = temp("pastend");
    std::fs::write(src.join("a.bin"), b"abc").unwrap();
    let mut e = Engine::new(FilesConfig::new(Side::Viewer, PathBuf::new()));
    e.send_file(1, &src.join("a.bin"), None);
    let outs = e.on_message(Message::FileAccept(swapdesk_proto::FileAccept { transfer_id: 1, resume_from: 4 }));
    assert!(outs.iter().any(|o| matches!(o, Out::Event(Event::Failed { code: ErrorCode::ProtocolViolation, .. }))));
    assert_eq!(e.active(), 0);
    let _ = std::fs::remove_dir_all(&src);
}

// ---------- F-FILE-02 ----------

#[test]
fn the_viewer_lists_a_host_folder_and_the_home_default() {
    let home = temp("list-home");
    std::fs::create_dir(home.join("docs")).unwrap();
    std::fs::write(home.join("a.txt"), b"12").unwrap();
    let mut w = World::new(None, &home);
    let outs = w.viewer.request_dir(1, None);
    w.pump(true, outs, None);
    let listing = w.viewer_events.iter().find_map(|e| match e {
        Event::Listing { entries, truncated, .. } => Some((entries.clone(), *truncated)),
        _ => None,
    });
    let (entries, truncated) = listing.expect("a listing");
    assert!(!truncated);
    assert_eq!(
        entries.iter().map(|e| (e.name.as_str(), e.is_dir)).collect::<Vec<_>>(),
        [("docs", true), ("a.txt", false)]
    );

    // A folder that is not there is NOT_FOUND, an error about the request.
    let outs = w.viewer.request_dir(2, Some(home.join("nope").to_string_lossy().into_owned()));
    w.pump(true, outs, None);
    assert!(
        w.to_viewer.iter().any(|m| matches!(m, Message::Error(e) if e.code == ErrorCode::NotFound && e.about_id == 2))
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_viewer_cannot_be_asked_for_its_folders_or_files() {
    let mut v = Engine::new(FilesConfig::new(Side::Viewer, PathBuf::from(".")));
    let outs = v.on_message(Message::DirRequest(DirRequest { req_id: 1, path: None }));
    assert!(matches!(outs.as_slice(), [Out::Msg(Message::Error(e))] if e.code == ErrorCode::NotPermitted));
    let outs = v.on_message(Message::FileGet(swapdesk_proto::FileGet { transfer_id: 1, path: "x".into() }));
    assert!(matches!(outs.as_slice(), [Out::Msg(Message::Error(e))] if e.code == ErrorCode::NotPermitted));
}

#[test]
fn a_listing_nobody_asked_for_is_dropped() {
    let mut v = Engine::new(FilesConfig::new(Side::Viewer, PathBuf::from(".")));
    let outs = v.on_message(Message::DirListing(swapdesk_proto::DirListing {
        req_id: 9,
        path: "x".into(),
        entries: vec![],
        truncated: false,
    }));
    assert!(outs.is_empty());
}

#[test]
fn a_get_for_a_missing_file_answers_not_found() {
    let home = temp("get-missing");
    let mut w = World::new(None, &home);
    let outs = w.viewer.request_get(3, home.join("missing").to_string_lossy().into_owned(), home.clone());
    w.pump(true, outs, None);
    assert!(
        w.to_viewer.iter().any(|m| matches!(m, Message::Error(e) if e.code == ErrorCode::NotFound && e.about_id == 3))
    );
    let _ = std::fs::remove_dir_all(&home);
}
