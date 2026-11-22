//! The copy queue (FILES-UI.md 5): every copy starts no more files than the session's transfer slots hold.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use super::*;
use crate::api::{
    ApiError, ChangeOutcome, DirEntryView, LocalListing, RemoteChange, RemoteEntry, RemoteKind, RemotePage,
    RemoteVolume, RemoteVolumes, Reply, SessionState, Ticket, VolumeType,
};

fn file(name: &str, size: u64) -> DirEntryView {
    DirEntryView { name: name.into(), is_dir: false, size, mtime_ms: 0 }
}

fn folder(name: &str) -> DirEntryView {
    DirEntryView { name: name.into(), is_dir: true, size: 0, mtime_ms: 0 }
}

fn remote(name: &str, kind: RemoteKind, size: u64) -> RemoteEntry {
    RemoteEntry { name: name.into(), kind, size, mtime_ms: 0, hidden: false, system: false, read_only: false }
}

fn downloads() -> PathBuf {
    PathBuf::from("C:/u/Downloads")
}

fn listing(path: &Path, entries: Vec<DirEntryView>) -> Reply {
    Reply::LocalListing(LocalListing { path: path.to_path_buf(), entries, truncated: false, hidden: Vec::new() })
}

/// The command `command`, sent and answered under its own ticket, as the backend does.
fn answer(s: &mut UiState, command: &Command, result: Result<Reply, ApiError>) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(90_000);
    let t = Ticket(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    s.track(t, command);
    s.apply(UiEvent::Reply { ticket: t, result });
}

/// The explorer open on a session with a 1.1 peer: this computer in `C:/u/Downloads` showing `local`, the other
/// computer's home folder showing `there`.
fn opened(local: Vec<DirEntryView>, there: Vec<RemoteEntry>) -> UiState {
    let mut s = Scenario::Visualizador.prepared_state();
    let a = s.activate(ControlId::TbFiles);
    assert!(s.explorer_open());
    s.apply(UiEvent::Reply { ticket: Ticket(0), result: Ok(listing(&downloads(), local)) });
    let volumes = a.commands.iter().find(|c| matches!(c, Command::FilesVolumes { .. })).cloned().expect("volumes");
    let system = RemoteVolume {
        id: 1,
        kind: VolumeType::System,
        label: String::new(),
        letter: Some('C'),
        total_bytes: 500_000_000_000,
        free_bytes: 120_000_000_000,
        has_bin: true,
    };
    let v = RemoteVolumes { home: ":1/Users/ana".into(), places: Vec::new(), volumes: vec![system] };
    answer(&mut s, &volumes, Ok(Reply::RemoteVolumes(v)));
    let first = s
        .take_followups()
        .into_iter()
        .find(|c| matches!(c, Command::FilesListPage { start: 0, .. }))
        .expect("the first page asked");
    let page = RemotePage { path: String::new(), start: 0, entries: there, next: None, free: Some(80_000_000_000) };
    answer(&mut s, &first, Ok(Reply::RemotePage(page)));
    s
}

fn select(s: &mut UiState, there: bool, names: &[&str]) {
    s.explorer.focus_there = there;
    let pane = &mut s.explorer.panes[usize::from(there)];
    pane.selected.clear();
    pane.selected.extend(names.iter().map(|n| n.to_string()));
}

/// The service's rule for a session's transfers, as `swapdesk-host/src/ui_hub/files.rs` applies it: a start that
/// would pass 8 unfinished transfers is refused `Busy` as a whole, and one of more than 64 files is `Invalid`.
#[derive(Default)]
struct Service {
    next: u32,
    active: Vec<u32>,
    names: BTreeMap<u32, (String, bool)>,
    /// Every file started, in order: the local path of an upload, the remote path of a download.
    started: Vec<String>,
    refused: Vec<ApiCode>,
    most: usize,
}

impl Service {
    fn start(&mut self, s: &mut UiState, c: &Command) {
        let files: Vec<(String, bool)> = match c {
            Command::FilesSend { local_paths, .. } => {
                local_paths.iter().map(|p| (p.to_string_lossy().replace('\\', "/"), true)).collect()
            }
            Command::FilesGet { remote_path, .. } => vec![(remote_path.clone(), false)],
            other => panic!("not a start: {other:?}"),
        };
        let code = if files.len() > 64 {
            Some(ApiCode::Invalid)
        } else if self.active.len() + files.len() > 8 {
            Some(ApiCode::Busy)
        } else {
            None
        };
        if let Some(code) = code {
            self.refused.push(code);
            answer(s, c, Err(ApiError::new(code)));
            return;
        }
        let ids: Vec<u32> = files
            .iter()
            .map(|_| {
                self.next += 1;
                self.next
            })
            .collect();
        let reply = match c {
            Command::FilesSend { .. } => Reply::TransferIds(ids.clone()),
            _ => Reply::TransferId(ids[0]),
        };
        answer(s, c, Ok(reply));
        for (id, (path, outgoing)) in ids.iter().zip(files) {
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            self.active.push(*id);
            self.started.push(path);
            self.names.insert(*id, (name.clone(), outgoing));
            transfer(s, *id, 1, &name, outgoing, TransferState::Queued);
        }
        self.most = self.most.max(self.active.len());
    }

    /// The oldest running transfer ends; false when none runs.
    fn finish(&mut self, s: &mut UiState) -> bool {
        if self.active.is_empty() {
            return false;
        }
        let id = self.active.remove(0);
        let (name, outgoing) = self.names[&id].clone();
        transfer(s, id, 1, &name, outgoing, TransferState::Done);
        true
    }
}

/// Sends `first` and everything the state asks for after it: starts go to the service, `other` answers the rest
/// (`None`: left unanswered); when nothing is left to send the oldest transfer ends, until none runs.
fn drive(
    s: &mut UiState,
    svc: &mut Service,
    first: Vec<Command>,
    mut other: impl FnMut(&Command) -> Option<Result<Reply, ApiError>>,
) {
    let mut todo: VecDeque<Command> = first.into();
    loop {
        while let Some(c) = todo.pop_front() {
            match &c {
                Command::FilesSend { .. } | Command::FilesGet { .. } => svc.start(s, &c),
                _ => {
                    if let Some(r) = other(&c) {
                        answer(s, &c, r);
                    }
                }
            }
            todo.extend(s.take_followups());
        }
        if !svc.finish(s) {
            break;
        }
        todo.extend(s.take_followups());
    }
}

fn twelve() -> Vec<DirEntryView> {
    (1..=12).map(|i| file(&format!("f{i:02}.bin"), 1_000 * i)).collect()
}

fn names(range: std::ops::RangeInclusive<u32>) -> Vec<String> {
    range.map(|i| format!("f{i:02}.bin")).collect()
}

fn select_files(s: &mut UiState, files: &[String]) {
    select(s, false, &files.iter().map(String::as_str).collect::<Vec<_>>());
}

fn sent(c: &Command) -> Vec<String> {
    match c {
        Command::FilesSend { local_paths, .. } => {
            local_paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
        }
        other => panic!("not a FilesSend: {other:?}"),
    }
}

#[test]
fn more_than_eight_files_wait_and_start_as_slots_free() {
    let mut s = opened(twelve(), Vec::new());
    let all = names(1..=12);
    select_files(&mut s, &all);
    let a = s.activate(ControlId::ExpCopy);
    assert_eq!(a.commands.len(), 1, "one start: {:?}", a.commands);
    assert_eq!(sent(&a.commands[0]), all[..8], "the first eight go, in the list's order");
    // The service names them: each has its row at once, and nothing more starts while all slots are taken.
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds((1..=8).collect())));
    assert!(s.take_followups().is_empty());
    assert_eq!(s.viewer_transfers().len(), 8);
    assert!(s.viewer_transfers().iter().all(|t| t.state == TransferState::Queued && t.size.is_some()));
    // One ends: the ninth starts, alone.
    transfer(&mut s, 3, 1, "f03.bin", true, TransferState::Done);
    let f = s.take_followups();
    assert_eq!(f.len(), 1);
    assert_eq!(sent(&f[0]), ["f09.bin"]);
    // Its start is not answered yet and holds its slot: another end starts exactly one more.
    transfer(&mut s, 1, 1, "f01.bin", true, TransferState::Failed { reason: ApiCode::Internal });
    let g = s.take_followups();
    assert_eq!(g.len(), 1);
    assert_eq!(sent(&g[0]), ["f10.bin"], "a failed transfer frees its slot too");
    // Two more end before the answers come: the last two go in one start.
    transfer(&mut s, 2, 1, "f02.bin", true, TransferState::Done);
    transfer(&mut s, 4, 1, "f04.bin", true, TransferState::Done);
    let h = s.take_followups();
    assert_eq!(h.iter().flat_map(sent).collect::<Vec<_>>(), ["f11.bin", "f12.bin"]);
    assert!(s.explorer.copy.waiting.is_empty());
}

#[test]
fn several_folders_with_their_contents_go_up_whole_and_never_pass_the_limit_of_the_service() {
    // What the owner did: several folders, each with files (one of them with a folder inside), copied at once.
    let mut s = opened(vec![folder("A"), folder("B"), folder("C")], Vec::new());
    select(&mut s, false, &["A", "B", "C"]);
    let tree: BTreeMap<PathBuf, Vec<DirEntryView>> = [
        (downloads().join("A"), (1..=4).map(|i| file(&format!("a{i}.txt"), 10)).chain([folder("Sub")]).collect()),
        (downloads().join("A").join("Sub"), (1..=9).map(|i| file(&format!("s{i}.txt"), 10)).collect()),
        (downloads().join("B"), (1..=4).map(|i| file(&format!("b{i}.txt"), 10)).collect()),
        (downloads().join("C"), (1..=4).map(|i| file(&format!("c{i}.txt"), 10)).collect()),
    ]
    .into();
    let mut svc = Service::default();
    let mut made = Vec::new();
    let a = s.activate(ControlId::ExpCopy);
    drive(&mut s, &mut svc, a.commands, |c| match c {
        Command::FilesChange { change: RemoteChange::MakeDir { parent, name }, .. } => {
            made.push(if parent.is_empty() { name.clone() } else { format!("{parent}/{name}") });
            Some(Ok(Reply::ChangeResults(vec![(name.clone(), ChangeOutcome::Done)])))
        }
        Command::LocalList { path: Some(p) } => tree.get(p).map(|e| Ok(listing(p, e.clone()))),
        _ => None,
    });
    assert_eq!(svc.refused, Vec::<ApiCode>::new(), "the service never had to refuse a start");
    assert!(svc.most <= 8, "{} transfers at once", svc.most);
    assert_eq!(made, ["A", "B", "C", "A/Sub"], "every folder is made there");
    let mut started = svc.started.clone();
    started.sort();
    let mut want: Vec<String> = tree
        .iter()
        .flat_map(|(dir, entries)| {
            entries.iter().filter(|e| !e.is_dir).map(move |e| dir.join(&e.name).to_string_lossy().replace('\\', "/"))
        })
        .collect();
    want.sort();
    assert_eq!(want.len(), 21);
    assert_eq!(started, want, "every file of every folder was sent, once");
    assert!(s.viewer_transfers().iter().all(|t| t.state == TransferState::Done));
    assert!(s.error().is_none(), "nothing is refused, so no error strip");
}

#[test]
fn a_folder_of_many_files_comes_down_whole_and_never_passes_the_limit_of_the_service() {
    let mut s = opened(Vec::new(), vec![remote("Contratos", RemoteKind::Dir, 0)]);
    select(&mut s, true, &["Contratos"]);
    let files: Vec<RemoteEntry> = (1..=20).map(|i| remote(&format!("c{i:02}.pdf"), RemoteKind::File, 2_000)).collect();
    let mut svc = Service::default();
    let a = s.activate(ControlId::ExpCopy);
    drive(&mut s, &mut svc, a.commands, |c| match c {
        Command::FilesListPage { path, start: 0, .. } if path == "Contratos" => {
            Some(Ok(Reply::RemotePage(RemotePage {
                path: path.clone(),
                start: 0,
                entries: files.clone(),
                next: None,
                free: None,
            })))
        }
        Command::LocalMakeDir { .. } => Some(Ok(Reply::Done)),
        _ => None,
    });
    assert_eq!(svc.refused, Vec::<ApiCode>::new(), "the service never had to refuse a start");
    assert_eq!(svc.most, 8, "as many at once as the session holds");
    let want: Vec<String> = (1..=20).map(|i| format!("Contratos/c{i:02}.pdf")).collect();
    assert_eq!(svc.started, want, "every file was fetched, in the folder's order");
    assert!(s.error().is_none());
}

#[test]
fn a_start_the_service_calls_busy_waits_again_and_the_next_second_tries_it() {
    let mut s = opened(twelve(), Vec::new());
    select_files(&mut s, &names(1..=2));
    let a = s.activate(ControlId::ExpCopy);
    answer(&mut s, &a.commands[0], Err(ApiError::new(ApiCode::Busy)));
    assert!(s.error().is_none(), "nothing was lost: no error strip");
    assert!(s.take_followups().is_empty(), "not in a loop: it waits for the next second");
    assert_eq!(s.explorer.copy.waiting.len(), 2);
    s.tick();
    assert_eq!(s.take_followups(), a.commands, "the same start again");
    // Any other refusal is told, and the files do not come back.
    let again = a.commands[0].clone();
    answer(&mut s, &again, Err(ApiError::new(ApiCode::NotPermitted)));
    assert!(s.error().is_some());
    assert!(s.explorer.copy.waiting.is_empty() && s.explorer.copy.asked.is_empty());
    assert_eq!(s.copy_summary().map(|c| c.failed), Some(2));
}

#[test]
fn nothing_starts_while_the_session_reconnects_and_the_copy_goes_on_when_it_is_back() {
    let mut s = opened(twelve(), Vec::new());
    let (device, active) = {
        let v = s.viewer_session().unwrap();
        (v.device_id.clone(), v.state.clone())
    };
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: device.clone(),
        state: SessionState::Reconnecting { attempt: 1 },
    });
    s.take_followups();
    select_files(&mut s, &names(1..=1));
    let a = s.activate(ControlId::ExpCopy);
    assert!(a.commands.is_empty(), "a start now would only fail: {:?}", a.commands);
    assert_eq!(s.explorer.copy.waiting.len(), 1);
    s.apply(UiEvent::Session { session_id: 1, device_id: device, state: active });
    let f = s.take_followups();
    assert!(f.iter().any(|c| matches!(c, Command::FilesSend { .. })), "the copy goes on: {f:?}");
}

#[test]
fn cancelling_a_transfer_starts_the_next_file_of_the_queue() {
    let mut s = opened(twelve(), Vec::new());
    select_files(&mut s, &names(1..=9));
    let a = s.activate(ControlId::ExpCopy);
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds((1..=8).collect())));
    let c = s.activate(ControlId::TransferCancel(5)).commands;
    assert_eq!(c[0], Command::TransferCancel { transfer_id: 5 }, "the cancel goes first");
    assert_eq!(sent(&c[1]), ["f09.bin"]);
}

#[test]
fn the_copy_queue_ends_with_its_session() {
    let mut s = opened(twelve(), Vec::new());
    select_files(&mut s, &names(1..=12));
    s.activate(ControlId::ExpCopy);
    assert_eq!(s.explorer.copy.waiting.len(), 4);
    let device = s.viewer_session().unwrap().device_id.clone();
    s.apply(UiEvent::Session { session_id: 1, device_id: device, state: SessionState::Ended { reason: None } });
    assert!(s.explorer.copy.waiting.is_empty() && s.explorer.copy.asked.is_empty());
    assert!(s.copy_summary().is_none());
}

#[test]
fn the_summary_counts_the_copy_under_way_by_bytes_and_names_the_file_that_runs() {
    let mut s = opened(twelve(), Vec::new());
    select_files(&mut s, &names(1..=3));
    let a = s.activate(ControlId::ExpCopy);
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds(vec![1, 2, 3])));
    let c = s.copy_summary().expect("a copy is under way");
    assert_eq!((c.items_done, c.items_total, c.bytes_done, c.bytes_total), (0, 3, 0, Some(6_000)));
    assert_eq!((c.now.as_deref(), c.finished), (None, false));
    transfer(&mut s, 1, 1, "f01.bin", true, TransferState::Done);
    transfer(&mut s, 2, 1, "f02.bin", true, TransferState::Running { done: 500, total: 2_000, bytes_per_s: 100 });
    transfer(&mut s, 3, 1, "f03.bin", true, TransferState::Running { done: 0, total: 3_000, bytes_per_s: 0 });
    let c = s.copy_summary().unwrap();
    assert_eq!((c.items_done, c.items_total, c.bytes_done, c.bytes_total), (1, 3, 1_500, Some(6_000)));
    assert_eq!((c.now.as_deref(), c.others), (Some("f02.bin"), 1));
    assert!((c.fraction - 0.25).abs() < 1e-6, "{}", c.fraction);
    // The rate is what the copy moved in the last seconds; the time left follows it.
    s.tick();
    transfer(&mut s, 2, 1, "f02.bin", true, TransferState::Running { done: 1_500, total: 2_000, bytes_per_s: 100 });
    s.tick();
    let c = s.copy_summary().unwrap();
    assert_eq!((c.bytes_per_s, c.seconds_left), (1_000, Some(4)), "3 500 bytes left at 1 000 a second");
    transfer(&mut s, 2, 1, "f02.bin", true, TransferState::Done);
    transfer(&mut s, 3, 1, "f03.bin", true, TransferState::Failed { reason: ApiCode::NoSpace });
    let c = s.copy_summary().unwrap();
    assert_eq!((c.items_done, c.items_total, c.failed, c.finished), (2, 2, 1, true));
    assert!((c.fraction - 1.0).abs() < 1e-6);
    // The next copy is counted by itself: what the session copied before is not in its numbers.
    select_files(&mut s, &names(4..=4));
    let a = s.activate(ControlId::ExpCopy);
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds(vec![9])));
    let c = s.copy_summary().unwrap();
    assert_eq!((c.items_done, c.items_total, c.failed, c.bytes_total), (0, 1, 0, Some(4_000)));
}

#[test]
fn a_cancelled_transfer_never_comes_back_as_a_row() {
    // Review of 22/11, F3: a note about a transfer the person had cancelled made a new row nobody could end, and
    // an unfinished row holds one of the session's slots.
    let mut s = opened(twelve(), Vec::new());
    select_files(&mut s, &names(1..=2));
    let a = s.activate(ControlId::ExpCopy);
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds(vec![1, 2])));
    s.activate(ControlId::TransferCancel(1));
    transfer(&mut s, 1, 1, "", true, TransferState::Running { done: 5, total: 10, bytes_per_s: 0 });
    transfer(&mut s, 1, 1, "f01.bin", true, TransferState::Failed { reason: ApiCode::Refused });
    assert_eq!(s.viewer_transfers().iter().map(|t| t.id).collect::<Vec<_>>(), [2]);
    assert_eq!(s.copy_summary().map(|c| (c.items_total, c.failed)), Some((1, 0)));
}

#[test]
fn a_folder_the_other_computer_refuses_is_left_out_and_the_other_folders_still_go() {
    // Review of 22/11, F4: one refused folder name ended the whole upload, the other picked folders included.
    let mut s = opened(vec![folder("A"), folder("B")], Vec::new());
    select(&mut s, false, &["A", "B"]);
    let tree: BTreeMap<PathBuf, Vec<DirEntryView>> = [
        (downloads().join("A"), vec![file("a1.txt", 10), folder("a:b")]),
        (downloads().join("B"), vec![file("b1.txt", 10)]),
    ]
    .into();
    let mut svc = Service::default();
    let a = s.activate(ControlId::ExpCopy);
    drive(&mut s, &mut svc, a.commands, |c| match c {
        Command::FilesChange { change: RemoteChange::MakeDir { name, .. }, .. } => {
            let outcome = if name == "a:b" { ChangeOutcome::InvalidName } else { ChangeOutcome::Done };
            Some(Ok(Reply::ChangeResults(vec![(name.clone(), outcome)])))
        }
        Command::LocalList { path: Some(p) } => tree.get(p).map(|e| Ok(listing(p, e.clone()))),
        _ => None,
    });
    let mut started = svc.started.clone();
    started.sort();
    assert_eq!(started, ["C:/u/Downloads/A/a1.txt", "C:/u/Downloads/B/b1.txt"], "the rest of the copy went");
    assert!(s.explorer_pane(true).notice.is_some(), "the folder left out is said");
    assert!(s.explorer.there.walk.is_none());
}

#[test]
fn the_one_second_timer_runs_while_a_copy_is_under_way_and_stops_when_it_is_over() {
    let mut s = opened(twelve(), Vec::new());
    assert!(!s.needs_tick(), "nothing counts time before the copy");
    select_files(&mut s, &names(1..=1));
    let a = s.activate(ControlId::ExpCopy);
    assert!(s.needs_tick(), "the start is out: the copy is under way");
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds(vec![1])));
    assert!(s.needs_tick(), "its rate and time left are measured each second");
    transfer(&mut s, 1, 1, "f01.bin", true, TransferState::Done);
    assert!(!s.needs_tick(), "the copy is over");
}

#[test]
fn the_bar_says_what_is_copied_now_how_much_is_done_how_fast_and_how_long_it_takes() {
    // The owner's order of 22/11/2026: "um visual mostrando que está sendo copiado, % feito, qual está sendo feito
    // agora".
    let mut s = opened(twelve(), Vec::new());
    assert!(s.transfers_bar().is_none(), "nothing was copied yet");
    select_files(&mut s, &names(1..=12));
    let a = s.activate(ControlId::ExpCopy);
    let bar = s.transfers_bar().expect("the copy shows at once");
    assert_eq!(bar.headline, "Preparando a cópia…");
    assert_eq!(bar.detail, "0% · 0 de 12 itens · 0 B de 78 KB · 12 na fila");
    assert!(bar.can_cancel && !bar.failed);
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds((1..=8).collect())));
    transfer(&mut s, 1, 1, "f01.bin", true, TransferState::Done);
    transfer(&mut s, 2, 1, "f02.bin", true, TransferState::Running { done: 1_000, total: 2_000, bytes_per_s: 0 });
    transfer(&mut s, 3, 1, "f03.bin", true, TransferState::Running { done: 0, total: 3_000, bytes_per_s: 0 });
    s.tick();
    transfer(&mut s, 3, 1, "f03.bin", true, TransferState::Running { done: 2_000, total: 3_000, bytes_per_s: 0 });
    s.tick();
    let bar = s.transfers_bar().unwrap();
    assert_eq!(bar.headline, "Copiando f02.bin e mais 1");
    // 4 000 of 78 000 bytes; 2 000 bytes in the last second, 74 000 to go: 37 s, told in steps of 5 s. Four files
    // wait: three for a slot, and the ninth, whose start is out and not named yet.
    assert_eq!(bar.detail, "5% · 1 de 12 itens · 4 KB de 78 KB · 2 KB/s · falta 40 s · 4 na fila");
    assert!((bar.fraction - 4_000.0 / 78_000.0).abs() < 1e-6);
    // The same in English, from the same state.
    s.apply(UiEvent::LanguageChanged(crate::api::Language::En));
    let bar = s.transfers_bar().unwrap();
    assert_eq!(bar.headline, "Copying f02.bin and 1 more");
    assert_eq!(bar.detail, "5% · 1 of 12 items · 4 KB of 78 KB · 2 KB/s · 40 s left · 4 waiting");
}

#[test]
fn the_bar_tells_the_end_of_the_copy_and_what_did_not_arrive() {
    let mut s = opened(twelve(), Vec::new());
    select_files(&mut s, &names(1..=2));
    let a = s.activate(ControlId::ExpCopy);
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds(vec![1, 2])));
    transfer(&mut s, 1, 1, "f01.bin", true, TransferState::Done);
    transfer(&mut s, 2, 1, "f02.bin", true, TransferState::Failed { reason: ApiCode::NoSpace });
    let bar = s.transfers_bar().unwrap();
    assert_eq!(bar.headline, "Cópia concluída com falhas");
    assert_eq!(bar.detail, "100% · 1 de 1 item · 1 KB de 1 KB · 1 com falha");
    assert!(bar.failed && !bar.can_cancel);
    // A link that dropped is said, not hidden behind "Preparando".
    select_files(&mut s, &names(3..=3));
    s.activate(ControlId::ExpCopy);
    let device = s.viewer_session().unwrap().device_id.clone();
    s.apply(UiEvent::Session { session_id: 1, device_id: device, state: SessionState::Reconnecting { attempt: 1 } });
    assert_eq!(s.transfers_bar().unwrap().headline, "Esperando a conexão voltar…");
}

#[test]
fn cancelar_tudo_drops_what_waits_cancels_what_runs_and_what_is_named_later() {
    let mut s = opened(twelve(), Vec::new());
    select_files(&mut s, &names(1..=12));
    let a = s.activate(ControlId::ExpCopy);
    answer(&mut s, &a.commands[0], Ok(Reply::TransferIds((1..=8).collect())));
    transfer(&mut s, 1, 1, "f01.bin", true, TransferState::Done);
    // The ninth file's start is out and not named yet.
    let ninth = s.take_followups();
    assert_eq!(sent(&ninth[0]), ["f09.bin"]);
    let c = s.activate(ControlId::TransfersCancelAll).commands;
    let cancelled: Vec<u32> = c
        .iter()
        .map(|c| match c {
            Command::TransferCancel { transfer_id } => *transfer_id,
            other => panic!("only cancels: {other:?}"),
        })
        .collect();
    assert_eq!(cancelled, (2..=8).collect::<Vec<_>>(), "everything that still ran");
    assert!(s.explorer.copy.waiting.is_empty(), "what waited never starts");
    assert_eq!(s.viewer_transfers().iter().map(|t| t.id).collect::<Vec<_>>(), [1], "what arrived stays listed");
    // The start that was out is named now: it is cancelled at once and never shows.
    answer(&mut s, &ninth[0], Ok(Reply::TransferIds(vec![9])));
    assert_eq!(s.take_followups(), vec![Command::TransferCancel { transfer_id: 9 }]);
    transfer(&mut s, 9, 1, "f09.bin", true, TransferState::Queued);
    assert_eq!(s.viewer_transfers().len(), 1);
    let bar = s.transfers_bar().unwrap();
    assert_eq!((bar.headline.as_str(), bar.can_cancel), ("Cópia concluída", false));
    assert!(
        !s.controls(crate::state::WindowKind::Viewer).iter().any(|c| c.id == ControlId::TransfersCancelAll),
        "nothing is left to cancel"
    );
}

#[test]
fn the_panel_lists_what_runs_first_and_no_more_rows_than_it_holds() {
    let mut s = opened(Vec::new(), Vec::new());
    for id in 1..=60 {
        transfer(&mut s, id, 1, &format!("d{id}.bin"), false, TransferState::Done);
    }
    transfer(&mut s, 61, 1, "falhou.bin", false, TransferState::Failed { reason: ApiCode::Internal });
    transfer(&mut s, 62, 1, "espera.bin", false, TransferState::Queued);
    transfer(&mut s, 63, 1, "parou.bin", false, TransferState::Paused { done: 1, total: 2, why: ApiCode::Timeout });
    transfer(&mut s, 64, 1, "agora.bin", false, TransferState::Running { done: 1, total: 2, bytes_per_s: 0 });
    let rows = s.transfer_rows();
    assert_eq!(rows.len(), crate::interact::TRANSFER_ROWS);
    assert_eq!(rows.iter().take(5).map(|t| t.id).collect::<Vec<_>>(), [64, 63, 61, 62, 60], "then the newest done");
    assert_eq!(s.transfer_line(rows[0]), "1 B de 2 B, 0 B/s");
    assert_eq!(s.transfer_line(rows[3]), "Na fila");
    assert_eq!(s.transfer_line(rows[4]), "Recebido");
}
