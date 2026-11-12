//! The file panel's view model: browsing, sending, receiving, the transfers and their outcomes.

use std::path::PathBuf;

use super::*;
use crate::api::{ApiCode, DirEntryView, Listing, Reply, Ticket, UiEvent};
use crate::mock::Scenario;
use crate::state::{DrawerTab, Fetch, WindowKind};

fn open_panel() -> UiState {
    let mut s = Scenario::Visualizador.prepared_state();
    let a = s.activate(ControlId::DrawerTab(DrawerTab::Files));
    assert!(
        matches!(a.commands[0], Command::FilesList { path: None, .. }),
        "the first opening lists the other computer's root"
    );
    s
}

fn entry(name: &str, is_dir: bool) -> DirEntryView {
    DirEntryView { name: name.into(), is_dir, size: if is_dir { 3 } else { 1500 }, mtime_ms: 1 }
}

fn listing(s: &mut UiState, path: &str, entries: Vec<DirEntryView>, truncated: bool) {
    s.apply(UiEvent::Reply {
        ticket: Ticket(0),
        result: Ok(Reply::Listing(Listing { path: path.into(), entries, truncated })),
    });
}

fn transfer(s: &mut UiState, id: u32, session_id: u64, name: &str, outgoing: bool, state: TransferState) {
    s.apply(UiEvent::Transfer { transfer_id: id, session_id, name: name.into(), outgoing, state });
}

fn labels(s: &UiState) -> Vec<String> {
    s.controls(WindowKind::Viewer).into_iter().map(|c| c.label).collect()
}

fn ids(s: &UiState) -> Vec<ControlId> {
    s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect()
}

#[test]
fn browsing_opens_folders_goes_up_and_back_to_the_root() {
    let mut s = open_panel();
    assert_eq!(s.drawer(), Some(DrawerTab::Files));
    assert_eq!(s.fetch(DataKind::Listing), Fetch::Pending, "a listing is on its way");
    listing(&mut s, "", vec![entry("Documentos", true), entry("nota.txt", false)], false);
    assert_eq!((s.remote_folder(), s.can_go_up()), (Some(""), false));
    assert!(!ids(&s).contains(&ControlId::FilesUp), "no way up from the root");

    // Open a folder: its path is the one the other computer spells, joined with "/".
    let a = s.activate(ControlId::FilesRow(0));
    assert_eq!(a.commands, vec![Command::FilesList { session_id: 1, path: Some("Documentos".into()) }]);
    assert_eq!(s.fetch(DataKind::Listing), Fetch::Pending);
    listing(&mut s, "Documentos", vec![entry("Contratos", true)], false);
    assert!(s.can_go_up() && ids(&s).contains(&ControlId::FilesUp));
    let a = s.activate(ControlId::FilesRow(0));
    assert_eq!(a.commands, vec![Command::FilesList { session_id: 1, path: Some("Documentos/Contratos".into()) }]);
    listing(&mut s, "Documentos/Contratos", vec![], false);
    assert!(labels(&s).iter().any(|l| l == &s.t(MsgKey::ChooseFiles)));

    // Up, then up again, then "Trocar pasta" (the root).
    assert_eq!(
        s.activate(ControlId::FilesUp).commands,
        vec![Command::FilesList { session_id: 1, path: Some("Documentos".into()) }]
    );
    listing(&mut s, "Documentos", vec![entry("Contratos", true)], false);
    assert_eq!(
        s.activate(ControlId::FilesUp).commands,
        vec![Command::FilesList { session_id: 1, path: Some(String::new()) }]
    );
    assert!(s.activate(ControlId::FilesUp).commands.is_empty() || s.can_go_up(), "up at the root does nothing more");
    assert_eq!(s.activate(ControlId::FilesChange).commands, vec![Command::FilesList { session_id: 1, path: None }]);
    // A row that is no longer there does nothing.
    assert!(s.activate(ControlId::FilesRow(40)).commands.is_empty());
}

#[test]
fn a_long_folder_pages_five_entries_at_a_time_and_every_page_is_reachable() {
    let mut s = open_panel();
    let entries: Vec<DirEntryView> = (0..12).map(|i| entry(&format!("f{i:02}.txt"), false)).collect();
    listing(&mut s, "", entries, true);
    assert_eq!(s.files_page_rows(), 0..FILES_PAGE);
    let rows = |s: &UiState| ids(s).into_iter().filter(|i| matches!(i, ControlId::FilesRow(_))).count();
    assert_eq!(rows(&s), FILES_PAGE, "only the page is in the control list");
    assert!(ids(&s).contains(&ControlId::FilesPage(true)) && !ids(&s).contains(&ControlId::FilesPage(false)));
    s.activate(ControlId::FilesPage(true));
    assert_eq!(s.files_page_rows(), FILES_PAGE..2 * FILES_PAGE);
    assert!(ids(&s).contains(&ControlId::FilesPage(false)));
    s.activate(ControlId::FilesPage(true));
    assert_eq!(s.files_page_rows(), 10..12, "the last page is short");
    s.activate(ControlId::FilesPage(true));
    assert_eq!(s.files_page_rows(), 10..12, "no page past the end");
    assert!(!ids(&s).contains(&ControlId::FilesPage(true)));
    // A row of the last page names its own file, by its place in the whole listing.
    assert!(ids(&s).contains(&ControlId::FilesRow(11)));
    s.activate(ControlId::FilesPage(false));
    s.activate(ControlId::FilesPage(false));
    s.activate(ControlId::FilesPage(false));
    assert_eq!(s.files_page_rows(), 0..FILES_PAGE);
    // A new listing starts at the first page.
    s.activate(ControlId::FilesPage(true));
    listing(&mut s, "", (0..7).map(|i| entry(&format!("g{i}"), true)).collect(), false);
    assert_eq!(s.files_page_rows(), 0..FILES_PAGE);
}

#[test]
fn a_download_goes_to_the_receive_folder_and_waits_for_one_when_there_is_none() {
    let mut s = open_panel();
    listing(&mut s, "Fotos", vec![entry("praia.jpg", false)], false);
    assert_eq!(s.receive_dir(), None);

    // No receive folder yet: the panel asks the shell for one, and the download starts when it is set.
    let a = s.activate(ControlId::FilesRow(0));
    assert_eq!((a.commands.len(), a.effect), (0, Some(Effect::PickReceiveFolder)));
    let dir = PathBuf::from("C:/Users/x/Downloads");
    let a = s.set_receive_dir(dir.clone());
    assert_eq!(
        a.commands,
        vec![Command::FilesGet { session_id: 1, remote_path: "Fotos/praia.jpg".into(), local_dir: dir.clone() }]
    );
    assert_eq!(s.receive_dir(), Some(dir.as_path()));
    // Setting it again starts nothing.
    assert!(s.set_receive_dir(dir.clone()).commands.is_empty());

    // With a folder set, a file row downloads at once; the folder button asks for another.
    let a = s.activate(ControlId::FilesRow(0));
    assert_eq!(
        a.commands,
        vec![Command::FilesGet { session_id: 1, remote_path: "Fotos/praia.jpg".into(), local_dir: dir.clone() }]
    );
    assert_eq!(s.activate(ControlId::FilesRecvFolder).effect, Some(Effect::PickReceiveFolder));
    assert!(labels(&s).iter().any(|l| l.contains("Downloads")), "the button names the folder: {:?}", labels(&s));

    // The root's file has a one-component path.
    listing(&mut s, "", vec![entry("raiz.txt", false)], false);
    assert_eq!(
        s.activate(ControlId::FilesRow(0)).commands,
        vec![Command::FilesGet { session_id: 1, remote_path: "raiz.txt".into(), local_dir: dir }]
    );
}

#[test]
fn sending_asks_the_shell_for_files_and_sends_them_to_the_folder_shown() {
    let mut s = open_panel();
    assert_eq!(s.activate(ControlId::FilesChoose).effect, Some(Effect::PickFiles));
    listing(&mut s, "Projetos/Site", vec![], false);
    let files = vec![PathBuf::from("C:/a/um.txt"), PathBuf::from("C:/a/dois.txt")];
    let a = s.send_files(files.clone());
    assert_eq!(
        a.commands,
        vec![Command::FilesSend { session_id: 1, local_paths: files, remote_dir: Some("Projetos/Site".into()) }]
    );
    assert!(s.send_files(Vec::new()).commands.is_empty(), "nothing picked, nothing sent");
    // The root is the empty path: the files go to the root, not the default folder.
    listing(&mut s, "", vec![], false);
    let a = s.send_files(vec![PathBuf::from("C:/a/um.txt")]);
    assert!(matches!(&a.commands[0], Command::FilesSend { remote_dir: Some(d), .. } if d.is_empty()));
    // Before any listing the host's default receive folder is meant.
    let mut fresh = Scenario::Visualizador.prepared_state();
    let a = fresh.send_files(vec![PathBuf::from("C:/a/um.txt")]);
    assert!(matches!(&a.commands[0], Command::FilesSend { remote_dir: None, .. }));
    // No session, nothing to send into.
    let mut idle = Scenario::Dispositivos.prepared_state();
    assert!(idle.send_files(vec![PathBuf::from("C:/a/um.txt")]).commands.is_empty());
}

#[test]
fn transfers_show_progress_resume_cancel_and_the_saved_as_outcome() {
    let mut s = open_panel();
    transfer(&mut s, 5, 1, "relatorio.pdf", true, TransferState::Queued);
    transfer(&mut s, 5, 1, "relatorio.pdf", true, TransferState::Running { done: 10, total: 100, bytes_per_s: 50 });
    transfer(&mut s, 6, 1, "praia.jpg", false, TransferState::Paused { done: 40, total: 100, why: ApiCode::Timeout });
    transfer(&mut s, 7, 1, "foto.jpg", false, TransferState::Done);
    // A transfer of another session (this machine hosting) is not this panel's.
    transfer(&mut s, 9, 777, "alheio.bin", false, TransferState::Done);
    assert_eq!(s.viewer_transfers().iter().map(|t| t.id).collect::<Vec<_>>(), [5, 6, 7]);

    // "Retomar" only on the paused one; "Cancelar" on every one that is not done.
    let all = ids(&s);
    assert!(
        all.contains(&ControlId::TransferResume(6))
            && !all.contains(&ControlId::TransferResume(5))
            && !all.contains(&ControlId::TransferResume(7))
    );
    assert!(
        all.contains(&ControlId::TransferCancel(5))
            && all.contains(&ControlId::TransferCancel(6))
            && !all.contains(&ControlId::TransferCancel(7))
    );
    assert_eq!(s.activate(ControlId::TransferResume(6)).commands, vec![Command::TransferResume { transfer_id: 6 }]);
    assert_eq!(s.activate(ControlId::TransferCancel(5)).commands, vec![Command::TransferCancel { transfer_id: 5 }]);
    assert_eq!(
        s.viewer_transfers().iter().map(|t| t.id).collect::<Vec<_>>(),
        [6, 7],
        "a cancelled transfer leaves the list at once"
    );

    // Files never overwrite: the name the file got is shown beside "Recebido".
    s.apply(UiEvent::TransferSavedAs { transfer_id: 7, session_id: 1, name: "foto (1).jpg".into() });
    let t = s.viewer_transfers()[1].clone();
    assert_eq!(
        (t.name.as_str(), t.saved_as.as_deref(), t.state.clone()),
        ("foto.jpg", Some("foto (1).jpg"), TransferState::Done)
    );
    s.apply(UiEvent::TransferSavedAs { transfer_id: 404, session_id: 1, name: "x".into() });
    assert_eq!(s.viewer_transfers().len(), 2, "a note about no transfer changes nothing");
    // A refusal is a failed transfer with its reason.
    transfer(&mut s, 6, 1, "praia.jpg", false, TransferState::Failed { reason: ApiCode::NotPermitted });
    assert!(matches!(s.viewer_transfers()[0].state, TransferState::Failed { reason: ApiCode::NotPermitted }));
}

#[test]
fn the_panel_resets_with_the_viewer_session_but_keeps_the_receive_folder() {
    let mut s = open_panel();
    listing(&mut s, "Fotos", vec![entry("praia.jpg", false)], false);
    s.activate(ControlId::FilesRow(0));
    assert!(s.files.waiting_download.is_some());
    transfer(&mut s, 1, 1, "x", true, TransferState::Queued);
    let dir = PathBuf::from("C:/Users/x/Downloads");
    let _ = s.set_receive_dir(dir.clone());
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: s.sessions[0].device_id.clone(),
        state: crate::api::SessionState::Ended { reason: None },
    });
    assert_eq!((s.listing().is_none(), s.transfers().len(), s.files.waiting_download.is_none()), (true, 0, true));
    assert_eq!(s.receive_dir(), Some(dir.as_path()), "the folder is the user's choice, not the session's");
}

#[test]
fn every_panel_control_has_a_name_for_a_screen_reader() {
    let mut s = open_panel();
    listing(&mut s, "Fotos", vec![entry("sub", true), entry("praia.jpg", false)], false);
    transfer(&mut s, 6, 1, "praia.jpg", false, TransferState::Paused { done: 1, total: 2, why: ApiCode::Timeout });
    let controls = s.controls(WindowKind::Viewer);
    for id in [
        ControlId::FilesChange,
        ControlId::FilesUp,
        ControlId::FilesRow(0),
        ControlId::FilesRow(1),
        ControlId::FilesChoose,
        ControlId::FilesRecvFolder,
        ControlId::TransferResume(6),
        ControlId::TransferCancel(6),
    ] {
        let c = controls.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id:?} is missing"));
        assert!(!c.label.trim().is_empty(), "{id:?} has no name");
    }
    let row = |i| controls.iter().find(|c| c.id == ControlId::FilesRow(i)).unwrap().label.clone();
    assert!(row(0).contains("sub") && row(0).starts_with(&s.t(MsgKey::FilesOpen)));
    assert!(row(1).contains("praia.jpg") && row(1).starts_with(&s.t(MsgKey::FilesDownload)));
}
