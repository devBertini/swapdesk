//! The explorer's state: what opens, selection, sort and copy (FILES-UI.md 10, slice 1).

use std::path::PathBuf;

use super::*;
use crate::api::{ApiCode, Listing, Reply, Ticket, UiEvent};
use crate::interact::explorer_manage::ExplorerKey;
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
            hidden: Vec::new(),
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
    assert!(s.explorer_pane(false).status.contains("1 selecionado,"), "{}", s.explorer_pane(false).status);
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

// ----- slice 2a: managing this computer's files (FILES-UI.md 3.2, 10) -----

/// The command `a` asked for, sent and answered: tracked under its own ticket like the backend does.
fn answer(s: &mut UiState, command: &Command, result: Result<Reply, crate::api::ApiError>) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(9_000);
    let t = Ticket(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    s.track(t, command);
    s.apply(UiEvent::Reply { ticket: t, result });
}

fn select_here(s: &mut UiState, name: &str) {
    let rows = s.explorer_pane(false).rows;
    let n = rows.iter().position(|r| r.name == name).expect("the row");
    s.activate(ControlId::ExpRow(false, n as u16));
}

#[test]
fn a_new_folder_is_named_and_renamed_in_place() {
    let mut s = opened();
    let a = s.activate(ControlId::ExpNewFolder);
    let made = PathBuf::from("C:/u/Downloads").join("Nova pasta");
    assert_eq!(a.commands, vec![Command::LocalMakeDir { path: made.clone() }]);
    answer(&mut s, &Command::LocalMakeDir { path: made }, Ok(Reply::Done));
    assert!(s.take_followups().contains(&Command::LocalList { path: Some(PathBuf::from("C:/u/Downloads")) }));
    reply(
        &mut s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("Nova pasta", true, 0), entry("Pasta", true, 0)],
            truncated: false,
            hidden: Vec::new(),
        }),
    );
    assert_eq!(s.explorer_manage().renaming, Some(("Nova pasta".into(), None)), "its name opens for editing");
    let a = s.submit_edit(EditId::ExpRename, "Projetos".into());
    let rename =
        Command::LocalRename { path: PathBuf::from("C:/u/Downloads").join("Nova pasta"), name: "Projetos".into() };
    assert_eq!(a.commands, vec![rename.clone()]);
    answer(&mut s, &rename, Ok(Reply::Done));
    assert!(s.explorer_manage().renaming.is_none(), "the field closes");
}

#[test]
fn rename_refuses_a_taken_or_invalid_name_and_keeps_the_field_open() {
    let mut s = opened();
    select_here(&mut s, "foto 2.jpg");
    s.activate(ControlId::ExpRename);
    assert!(s.explorer_manage().renaming.is_some());
    let a = s.submit_edit(EditId::ExpRename, "a:b.jpg".into());
    assert!(a.commands.is_empty(), "an invalid name never leaves the window");
    assert_eq!(s.explorer_manage().renaming.unwrap().1.as_deref(), Some(s.t(MsgKey::ExplorerNameInvalid).as_str()));
    let a = s.submit_edit(EditId::ExpRename, "foto 10.jpg".into());
    answer(&mut s, &a.commands[0], Err(crate::api::ApiError::new(ApiCode::Refused)));
    let (name, error) = s.explorer_manage().renaming.expect("still open");
    assert_eq!((name.as_str(), error.as_deref()), ("foto 2.jpg", Some(s.t(MsgKey::ExplorerNameTaken).as_str())));
    s.explorer_key(ExplorerKey::Escape);
    assert!(s.explorer_manage().renaming.is_none(), "Esc closes it without saving");
}

#[test]
fn cut_and_paste_moves_inside_this_computer() {
    let mut s = opened();
    select_here(&mut s, "foto 2.jpg");
    s.explorer_key(ExplorerKey::Cut);
    let a = s.explorer_key(ExplorerKey::Paste);
    assert_eq!(
        a.commands,
        vec![Command::LocalMove {
            paths: vec![PathBuf::from("C:/u/Downloads").join("foto 2.jpg")],
            into: PathBuf::from("C:/u/Downloads")
        }]
    );
    answer(&mut s, &a.commands[0], Err(crate::api::ApiError::new(ApiCode::Invalid)));
    assert_eq!(s.explorer_manage().notice.as_deref(), Some(s.t(MsgKey::ExplorerMoveIntoItself).as_str()));
}

#[test]
fn delete_sends_to_the_bin_and_shift_delete_asks_first() {
    let mut s = opened();
    select_here(&mut s, "Pasta");
    let a = s.explorer_key(ExplorerKey::Delete);
    let path = PathBuf::from("C:/u/Downloads").join("Pasta");
    assert_eq!(a.commands, vec![Command::LocalTrash { paths: vec![path.clone()] }], "no question: the bin is the undo");

    let a = s.explorer_key(ExplorerKey::DeleteForever);
    assert!(a.commands.is_empty(), "for good only after the card");
    let (title, _) = s.explorer_manage().forever.expect("the card");
    assert_eq!(title, "Excluir 1 item para sempre?");
    let ids: Vec<ControlId> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
    let (cancel, confirm) = (
        ids.iter().position(|i| *i == ControlId::ExpForeverCancel).unwrap(),
        ids.iter().position(|i| *i == ControlId::ExpForeverConfirm).unwrap(),
    );
    assert!(cancel < confirm, "Cancelar first, the danger button last");
    let a = s.activate(ControlId::ExpForeverConfirm);
    assert_eq!(a.commands, vec![Command::LocalDeleteForever { paths: vec![path] }]);
}

#[test]
fn delete_on_a_local_volume_without_a_bin_refuses_with_the_notice() {
    let mut s = opened();
    select_here(&mut s, "Pasta");
    let a = s.explorer_key(ExplorerKey::Delete);
    answer(&mut s, &a.commands[0], Err(crate::api::ApiError::new(ApiCode::Refused)));
    let notice = s.explorer_manage().notice.expect("the notice");
    assert_eq!(notice, "Este local não tem Lixeira; nada foi excluído.");
    assert!(s.controls(WindowKind::Viewer).iter().any(|c| c.label == notice && c.region == Region::Status));
}

#[test]
fn properties_count_a_folder_and_close() {
    let mut s = opened();
    select_here(&mut s, "Pasta");
    let a = s.explorer_key(ExplorerKey::Properties);
    let path = PathBuf::from("C:/u/Downloads").join("Pasta");
    assert_eq!(a.commands, vec![Command::LocalProps { path: path.clone() }]);
    assert_eq!(s.explorer_manage().props.unwrap().1, vec!["Contando…".to_owned()]);
    reply(
        &mut s,
        Reply::LocalProps(crate::api::LocalProps {
            path,
            files: 1,
            folders: 2,
            bytes: 2048,
            truncated: false,
            free_bytes: Some(1_000_000_000),
        }),
    );
    let (title, lines) = s.explorer_manage().props.expect("the card");
    assert_eq!(title, "Propriedades de Pasta");
    assert!(lines[0].starts_with("1 arquivo e 2 pastas"), "{lines:?}");
    assert!(lines[1].starts_with("Livre neste disco:"), "{lines:?}");
    s.activate(ControlId::ExpPropsClose);
    assert!(s.explorer_manage().props.is_none());
}

#[test]
fn hidden_items_show_only_when_asked() {
    let mut s = opened();
    reply(
        &mut s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("a.txt", false, 1)],
            truncated: false,
            hidden: vec![entry(".config", true, 0)],
        }),
    );
    let names = |s: &UiState| s.explorer_pane(false).rows.into_iter().map(|r| r.name).collect::<Vec<_>>();
    assert_eq!(names(&s), ["a.txt"]);
    s.activate(ControlId::ExpHidden);
    assert!(names(&s).contains(&".config".to_owned()));
}
