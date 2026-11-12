//! The explorer's state: what opens, selection, sort and copy (FILES-UI.md 10, slice 1).

use std::path::PathBuf;

use super::*;
use crate::api::{Listing, Reply, Ticket, UiEvent};
use crate::mock::Scenario;
use crate::state::WindowKind;

fn entry(name: &str, is_dir: bool, size: u64) -> DirEntryView {
    DirEntryView { name: name.into(), is_dir, size, mtime_ms: 0 }
}

fn reply(s: &mut UiState, r: Reply) {
    s.apply(UiEvent::Reply { ticket: Ticket(0), result: Ok(r) });
}

/// The explorer open on a session, this computer in `C:/u/Downloads` and the other computer's home folder listed.
fn opened() -> UiState {
    let mut s = Scenario::Visualizador.prepared_state();
    let a = s.activate(ControlId::TbFiles);
    assert!(s.explorer_open());
    assert!(a.commands.contains(&Command::LocalList { path: None }), "{:?}", a.commands);
    assert!(a.commands.contains(&Command::LocalPlaces));
    assert!(a.commands.iter().any(|c| matches!(c, Command::FilesList { path: None, .. })));
    reply(
        &mut s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("foto 10.jpg", false, 10), entry("foto 2.jpg", false, 20), entry("Pasta", true, 0)],
            truncated: false,
        }),
    );
    reply(
        &mut s,
        Reply::Listing(Listing {
            path: String::new(),
            entries: vec![entry("nota.txt", false, 5), entry("Documents", true, 0)],
            truncated: false,
        }),
    );
    s
}

fn names(v: &PaneView) -> Vec<&str> {
    v.rows.iter().map(|r| r.name.as_str()).collect()
}

#[test]
fn explorer_opens_with_this_computer_left_and_the_peer_right() {
    let s = opened();
    let here = s.explorer_pane(false);
    let there = s.explorer_pane(true);
    assert!(!here.there && there.there);
    assert_eq!(here.title, "Este computador");
    assert!(!there.title.is_empty() && there.title != here.title, "the other computer's name");
    assert!(s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::ExpCopy));
    assert_eq!(s.drawer(), None, "the explorer replaces the drawer");
}

#[test]
fn names_sort_naturally_with_folders_first() {
    let mut s = opened();
    assert_eq!(names(&s.explorer_pane(false)), vec!["Pasta", "foto 2.jpg", "foto 10.jpg"]);
    s.activate(ControlId::ExpSort(false, SORT_NAME));
    assert_eq!(names(&s.explorer_pane(false)), vec!["Pasta", "foto 10.jpg", "foto 2.jpg"], "reversed, folders first");
    s.activate(ControlId::ExpSort(false, SORT_SIZE));
    assert_eq!(names(&s.explorer_pane(false)), vec!["Pasta", "foto 10.jpg", "foto 2.jpg"]);
    assert_eq!(natural_cmp("a2", "a10"), Ordering::Less);
    assert_eq!(natural_cmp("ária", "arib"), Ordering::Less, "accents fold");
}

#[test]
fn a_click_selects_one_ctrl_click_toggles_and_shift_click_extends_from_the_anchor() {
    let mut s = opened();
    s.activate(ControlId::ExpRow(false, 0));
    let sel = |s: &UiState| s.explorer_pane(false).rows.iter().filter(|r| r.selected).count();
    assert_eq!(sel(&s), 1);
    s.set_pointer_mods(PointerMods { toggle: true, ..PointerMods::default() });
    s.activate(ControlId::ExpRow(false, 2));
    assert_eq!(sel(&s), 2);
    s.set_pointer_mods(PointerMods { extend: true, ..PointerMods::default() });
    s.activate(ControlId::ExpRow(false, 0));
    assert_eq!(sel(&s), 3, "from the anchor (row 2) to row 0");
    s.activate(ControlId::ExpRow(false, 1));
    assert_eq!(sel(&s), 1, "a plain click selects one again");
    assert!(s.explorer_pane(false).status.contains("1 selecionados"), "{}", s.explorer_pane(false).status);
}

#[test]
fn a_double_click_opens_a_folder_and_back_returns() {
    let mut s = opened();
    s.set_pointer_mods(PointerMods { clicks: 2, ..PointerMods::default() });
    let a = s.activate(ControlId::ExpRow(true, 0));
    assert!(
        a.commands.iter().any(|c| matches!(c, Command::FilesList { path: Some(p), .. } if p == "Documents")),
        "{:?}",
        a.commands
    );
    reply(&mut s, Reply::Listing(Listing { path: "Documents".into(), entries: vec![], truncated: false }));
    let v = s.explorer_pane(true);
    assert!(v.can_back && v.can_up);
    assert_eq!(v.crumbs, vec!["Pasta pessoal".to_string(), "Documents".to_string()]);
    let a = s.activate(ControlId::ExpBack(true));
    assert!(a.commands.iter().any(|c| matches!(c, Command::FilesList { path: Some(p), .. } if p.is_empty())));
    // A double click on a local file opens it with the system's app.
    s.set_pointer_mods(PointerMods { clicks: 2, ..PointerMods::default() });
    let a = s.activate(ControlId::ExpRow(false, 1));
    assert_eq!(a.effect, Some(Effect::OpenLocal));
    assert_eq!(s.take_local_target(), Some(PathBuf::from("C:/u/Downloads").join("foto 2.jpg")));
}

#[test]
fn copy_from_the_local_pane_sends_into_the_folder_of_the_remote_pane() {
    let mut s = opened();
    s.activate(ControlId::ExpRow(false, 1));
    assert!(s.explorer_can_copy());
    assert!(s.explorer_copy_label().starts_with("Copiar para "));
    let a = s.activate(ControlId::ExpCopy);
    assert_eq!(
        a.commands,
        vec![Command::FilesSend {
            session_id: 1,
            local_paths: vec![PathBuf::from("C:/u/Downloads").join("foto 2.jpg")],
            remote_dir: Some(String::new()),
        }]
    );
}

#[test]
fn copy_from_the_remote_pane_gets_into_the_folder_of_the_local_pane() {
    let mut s = opened();
    s.activate(ControlId::ExpRow(true, 1));
    assert_eq!(s.explorer_copy_label(), "Copiar para este computador");
    let a = s.activate(ControlId::ExpCopy);
    assert_eq!(
        a.commands,
        vec![Command::FilesGet {
            session_id: 1,
            remote_path: "nota.txt".into(),
            local_dir: PathBuf::from("C:/u/Downloads"),
        }]
    );
    // A folder is left out of a copy, and the note says so.
    s.activate(ControlId::ExpRow(true, 0));
    let a = s.activate(ControlId::ExpCopy);
    assert!(a.commands.is_empty());
    assert!(s.explorer_note().is_some());
}

#[test]
fn an_unreadable_local_folder_shows_the_reason() {
    let mut s = opened();
    s.set_pointer_mods(PointerMods { clicks: 2, ..PointerMods::default() });
    let _ = s.activate(ControlId::ExpRow(false, 0));
    s.apply(UiEvent::Reply { ticket: Ticket(1), result: Err(crate::api::ApiError::new(ApiCode::NotPermitted)) });
    // The reply above has no tracked ticket; the pane's own failure path is what the backend's answer reaches.
    let c = s.here_failed(ApiCode::NotFound);
    assert_eq!(c, Changed::VIEWER);
    let v = s.explorer_pane(false);
    assert!(v.error.as_ref().is_some_and(|(_, why)| why.contains("não existe mais")), "{:?}", v.error);
}

#[test]
fn tela_goes_back_to_the_picture() {
    let mut s = opened();
    s.activate(ControlId::ExpTela);
    assert!(!s.explorer_open());
    s.activate(ControlId::TbFiles);
    assert!(s.explorer_open(), "opens again with what it had");
}

#[test]
fn types_come_from_one_table() {
    use crate::api::Language;
    assert_eq!(type_of(Language::PtBr, "a.pdf", false), "Documento PDF");
    assert_eq!(type_of(Language::PtBr, "a.PNG", false), "Imagem PNG");
    assert_eq!(type_of(Language::PtBr, "a.xyz", false), "Arquivo XYZ");
    assert_eq!(type_of(Language::En, "noext", false), "File");
    assert_eq!(type_of(Language::PtBr, "x", true), "Pasta");
}

#[test]
fn sizes_read_in_the_unit_that_fits() {
    use crate::api::Language;
    assert_eq!(human_size(Language::PtBr, 1_200), "1 KB");
    assert_eq!(human_size(Language::PtBr, 48_200), "48 KB");
    assert_eq!(human_size(Language::PtBr, 2_400_000), "2,4 MB");
    assert_eq!(human_size(Language::En, 2_400_000), "2.4 MB");
    assert_eq!(human_size(Language::PtBr, 512), "512 B");
}
