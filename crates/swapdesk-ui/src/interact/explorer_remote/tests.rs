//! Slice 2b: the right pane, the other computer (FILES-UI.md 10).

use std::path::PathBuf;

use super::super::{PointerMods, Region};
use super::*;
use crate::api::{ApiError, Language, Listing, LocalListing, RemotePlace, Reply, Role, Ticket, UiEvent};
use crate::interact::explorer_manage::ExplorerKey;
use crate::mock::Scenario;
use crate::state::WindowKind;

fn entry(name: &str, is_dir: bool, size: u64) -> DirEntryView {
    DirEntryView { name: name.into(), is_dir, size, mtime_ms: 0 }
}

fn remote(name: &str, kind: RemoteKind, size: u64) -> RemoteEntry {
    RemoteEntry { name: name.into(), kind, size, mtime_ms: 0, hidden: false, system: false, read_only: false }
}

fn vol(id: u32, kind: VolumeType, label: &str, letter: Option<char>, has_bin: bool) -> RemoteVolume {
    RemoteVolume {
        id,
        kind,
        label: label.into(),
        letter,
        total_bytes: 500_000_000_000,
        free_bytes: 120_000_000_000,
        has_bin,
    }
}

fn volumes_11() -> RemoteVolumes {
    RemoteVolumes {
        home: ":1/Users/ana".into(),
        places: vec![
            RemotePlace { kind: PlaceName::Desktop, path: ":1/Users/ana/Desktop".into() },
            RemotePlace { kind: PlaceName::Documents, path: ":1/Users/ana/Documents".into() },
        ],
        volumes: vec![
            vol(1, VolumeType::System, "", Some('C'), true),
            vol(2, VolumeType::Fixed, "Backup", Some('D'), true),
            vol(3, VolumeType::Removable, "", Some('E'), false),
        ],
    }
}

fn page(path: &str, start: u32, entries: Vec<RemoteEntry>, next: Option<u32>) -> RemotePage {
    RemotePage { path: path.into(), start, entries, next, free: Some(80_000_000_000) }
}

fn reply(s: &mut UiState, r: Reply) {
    s.apply(UiEvent::Reply { ticket: Ticket(0), result: Ok(r) });
}

/// The command `command`, sent and answered under its own ticket, as the backend does.
fn answer(s: &mut UiState, command: &Command, result: Result<Reply, ApiError>) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(40_000);
    let t = Ticket(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    s.track(t, command);
    s.apply(UiEvent::Reply { ticket: t, result });
}

fn local_listing(s: &mut UiState) {
    reply(
        s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("foto.jpg", false, 10), entry("Fotos", true, 0), entry("novo.txt", false, 4)],
            truncated: false,
            hidden: Vec::new(),
        }),
    );
}

/// The explorer opened: what it asked on opening, with this computer's folder listed.
fn open() -> (UiState, Vec<Command>) {
    let mut s = Scenario::Visualizador.prepared_state();
    let a = s.activate(ControlId::TbFiles);
    assert!(s.explorer_open());
    local_listing(&mut s);
    (s, a.commands)
}

fn volumes_cmd(cmds: &[Command]) -> Command {
    cmds.iter().find(|c| matches!(c, Command::FilesVolumes { .. })).cloned().expect("FilesVolumes asked on opening")
}

fn first_page_cmd(s: &mut UiState) -> Command {
    s.take_followups()
        .into_iter()
        .find(|c| matches!(c, Command::FilesListPage { start: 0, .. }))
        .expect("the first page asked")
}

/// A 1.1 peer: volumes answered, the home folder listed by pages.
fn opened_11() -> UiState {
    let (mut s, cmds) = open();
    answer(&mut s, &volumes_cmd(&cmds), Ok(Reply::RemoteVolumes(volumes_11())));
    let c = first_page_cmd(&mut s);
    assert!(matches!(&c, Command::FilesListPage { path, .. } if path.is_empty()), "the home folder first");
    answer(
        &mut s,
        &c,
        Ok(Reply::RemotePage(page(
            "",
            0,
            vec![
                remote("Contratos", RemoteKind::Dir, 0),
                remote("notas.txt", RemoteKind::File, 1_200),
                remote("foto.jpg", RemoteKind::File, 3_000_000),
            ],
            None,
        ))),
    );
    s
}

/// A 1.0 peer: `FilesVolumes` answered `Invalid`, the home folder listed by `FilesList`.
fn opened_10() -> UiState {
    let (mut s, cmds) = open();
    answer(&mut s, &volumes_cmd(&cmds), Err(ApiError::new(ApiCode::Invalid)));
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

fn there_rows(s: &UiState) -> Vec<String> {
    s.explorer_pane(true).rows.into_iter().map(|r| r.name).collect()
}

fn select_there(s: &mut UiState, name: &str) {
    let n = there_rows(s).iter().position(|r| r == name).expect("the row");
    s.activate(ControlId::ExpRow(true, n as u16));
}

fn select_here(s: &mut UiState, name: &str) {
    let n = s.explorer_pane(false).rows.iter().position(|r| r.name == name).expect("the row");
    s.activate(ControlId::ExpRow(false, n as u16));
}

fn labels(s: &UiState) -> Vec<String> {
    s.explorer_places().into_iter().filter(|p| p.there).map(|p| p.label).collect()
}

fn files_change(s: &UiState, change: RemoteChange) -> Command {
    Command::FilesChange { session_id: s.session_id().unwrap(), change }
}

fn change_cmd(a: &Activation) -> Command {
    a.commands.iter().find(|c| matches!(c, Command::FilesChange { .. })).cloned().expect("a FilesChange")
}

// ----- the 1.0 fallback -----

#[test]
fn a_1_0_peer_shows_the_home_places_the_note_and_disables_every_change() {
    let mut s = opened_10();
    assert_eq!(labels(&s).len(), 5, "places derived from the home folder, no volumes");
    assert!(s.explorer_places().iter().all(|p| !p.volume));
    let v = s.explorer_pane(true);
    let note = v.notice.expect("the note");
    assert!(note.contains("usa o Swapdesk 1.0: só a pasta pessoal aparece"), "{note}");
    assert!(note.starts_with("O ") && note.contains(&v.title), "{note}");
    assert!(!v.hidden_switch, "a 1.0 peer cannot flag hidden entries");
    select_there(&mut s, "nota.txt");
    let m = s.explorer_manage();
    assert!(!m.can_new_folder && !m.can_rename && !m.can_delete);
    let why = m.change_why.expect("the reason");
    assert!(why.contains("usa o Swapdesk 1.0. Atualize o Swapdesk de lá"), "{why}");
    for id in [ControlId::ExpNewFolder, ControlId::ExpRename, ControlId::ExpDelete] {
        let a = s.activate(id);
        assert!(a.commands.is_empty(), "{id:?}");
    }
    assert!(s.explorer_manage().trash_there.is_none(), "no card for a change that cannot happen");
    // The disabled button keeps its reason as its description in the controls list.
    let specs = s.controls(WindowKind::Viewer);
    let del = specs.iter().find(|c| c.id == ControlId::ExpDelete).unwrap();
    assert!(del.disabled && del.description.as_deref() == Some(why.as_str()));
    // Folder upload is refused with the same sentence, files still go.
    s.explorer.focus_there = false;
    let folder = s.explorer_pane(false).rows.iter().position(|r| r.is_dir).expect("a folder here");
    s.activate(ControlId::ExpRow(false, folder as u16));
    let a = s.activate(ControlId::ExpCopy);
    assert!(a.commands.is_empty());
    assert!(s.explorer_note().is_some_and(|n| n.contains("usa o Swapdesk 1.0")));
}

#[test]
fn volumes_are_asked_once_when_the_explorer_opens() {
    let (mut s, cmds) = open();
    assert_eq!(cmds.iter().filter(|c| matches!(c, Command::FilesVolumes { .. })).count(), 1);
    s.activate(ControlId::ExpTela);
    let a = s.activate(ControlId::TbFiles);
    assert!(!a.commands.iter().any(|c| matches!(c, Command::FilesVolumes { .. })), "never refetched per visit");
}

// ----- volumes in the navigation pane -----

#[test]
fn volumes_fill_the_navigation_pane_and_follow_the_pushed_list() {
    let mut s = opened_11();
    assert_eq!(
        labels(&s),
        [
            "Pasta pessoal",
            "Área de trabalho",
            "Documentos",
            "Disco local (C:)",
            "Backup (D:)",
            "Unidade removível (E:)"
        ]
    );
    let removable = s.explorer_places().into_iter().find(|p| p.label == "Unidade removível (E:)").unwrap();
    assert!(removable.volume && removable.volume_kind == Some(VolumeType::Removable));
    assert_eq!(removable.hint.as_deref(), Some("120,0 GB livres de 500,0 GB"));
    assert!(s.explorer_places().iter().find(|p| p.there && p.label == "Pasta pessoal").unwrap().selected);

    // A Mac volume is named by its label, or "Volume sem nome"; a network drive by its kind.
    let mut v = volumes_11();
    v.volumes = vec![
        vol(1, VolumeType::System, "Macintosh HD", None, true),
        vol(4, VolumeType::Image, "", None, true),
        vol(5, VolumeType::Network, "", Some('Z'), false),
        vol(6, VolumeType::Optical, "", Some('F'), false),
    ];
    let c = s.apply(UiEvent::RemoteVolumes { session_id: 1, volumes: v });
    assert_eq!(c, Changed::VIEWER);
    assert_eq!(labels(&s)[3..], ["Macintosh HD", "Volume sem nome", "Unidade de rede (Z:)", "Unidade de CD/DVD (F:)"]);
    // An event of another session changes nothing.
    let c = s.apply(UiEvent::RemoteVolumes { session_id: 99, volumes: volumes_11() });
    assert_eq!(c, Changed::NONE);
    assert_eq!(labels(&s).len(), 7);
}

#[test]
fn a_pane_on_a_removed_volume_shows_its_error() {
    let mut s = opened_11();
    s.activate(ControlId::ExpPlace(true, 5));
    let c = Command::FilesListPage { session_id: 1, path: ":3".into(), start: 0 };
    answer(&mut s, &c, Ok(Reply::RemotePage(page(":3", 0, vec![remote("a.jpg", RemoteKind::File, 5)], None))));
    assert_eq!(s.explorer_pane(true).crumbs, ["Unidade removível (E:)"]);
    assert!(s.explorer_places().iter().find(|p| p.there && p.label == "Unidade removível (E:)").unwrap().selected);
    let mut v = volumes_11();
    v.volumes.truncate(2);
    s.apply(UiEvent::RemoteVolumes { session_id: 1, volumes: v });
    let (title, why) = s.explorer_pane(true).error.expect("the error state");
    assert!(!title.is_empty());
    assert_eq!(why, "O disco Unidade removível (E:) foi removido do MacBook do escritório.");
}

// ----- the listing by pages -----

#[test]
fn a_1_1_peer_lists_by_pages_and_fetches_every_one() {
    let (mut s, cmds) = open();
    answer(&mut s, &volumes_cmd(&cmds), Ok(Reply::RemoteVolumes(volumes_11())));
    let first = first_page_cmd(&mut s);
    let mut system = remote("NTUSER.DAT", RemoteKind::File, 9);
    system.system = true;
    let mut flagged = remote("oculto.txt", RemoteKind::File, 9);
    flagged.hidden = true;
    answer(
        &mut s,
        &first,
        Ok(Reply::RemotePage(page(
            "",
            0,
            vec![remote("b.txt", RemoteKind::File, 2), remote(".config", RemoteKind::Dir, 0), system, flagged],
            Some(4),
        ))),
    );
    // The next page is asked at once, from `next`.
    let again = s.take_followups();
    assert_eq!(again, vec![Command::FilesListPage { session_id: 1, path: String::new(), start: 4 }]);
    assert_eq!(there_rows(&s), ["b.txt"], "hidden and system entries stay hidden");
    answer(&mut s, &again[0], Ok(Reply::RemotePage(page("", 4, vec![remote("a.txt", RemoteKind::File, 1)], None))));
    assert!(s.take_followups().is_empty(), "the last page ends the chain");
    assert_eq!(there_rows(&s), ["a.txt", "b.txt"]);
    assert_eq!(s.explorer_pane(true).free.as_deref(), Some("80,0 GB livres de 500,0 GB"));
    // The right pane's own hidden switch.
    assert!(s.explorer_pane(true).hidden_switch);
    s.activate(ControlId::ExpHiddenThere);
    assert_eq!(there_rows(&s).len(), 5);
    assert!(s.explorer_pane(true).show_hidden && !s.explorer_pane(false).show_hidden, "each pane has its own");
    // A page nobody asked for any more is dropped.
    let c = Command::FilesListPage { session_id: 1, path: "Documents".into(), start: 0 };
    s.activate(ControlId::ExpRow(true, 0));
    answer(&mut s, &c, Ok(Reply::RemotePage(page("Documents", 0, vec![remote("x", RemoteKind::File, 1)], None))));
}

#[test]
fn a_page_for_a_folder_the_pane_left_is_dropped() {
    let mut s = opened_11();
    let to_a = s.activate(ControlId::ExpPlace(true, 2));
    let a_cmd = to_a.commands.into_iter().next().unwrap();
    assert!(matches!(&a_cmd, Command::FilesListPage { path, start: 0, .. } if path == ":1/Users/ana/Documents"));
    let to_b = s.activate(ControlId::ExpPlace(true, 1));
    let b_cmd = to_b.commands.into_iter().next().unwrap();
    answer(
        &mut s,
        &a_cmd,
        Ok(Reply::RemotePage(page(":1/Users/ana/Documents", 0, vec![remote("late", RemoteKind::File, 1)], None))),
    );
    assert!(!there_rows(&s).contains(&"late".to_string()), "the answer for the older click is stale");
    answer(
        &mut s,
        &b_cmd,
        Ok(Reply::RemotePage(page(":1/Users/ana/Desktop", 0, vec![remote("mesa", RemoteKind::File, 1)], None))),
    );
    assert_eq!(there_rows(&s), ["mesa"]);
    assert_eq!(s.explorer_pane(true).crumbs, ["Pasta pessoal", "Desktop"]);
    assert!(s.explorer_pane(true).can_up);
}

// ----- changes there -----

#[test]
fn delete_there_always_asks_and_sends_trash_only_after_excluir() {
    let mut s = opened_11();
    select_there(&mut s, "notas.txt");
    let a = s.activate(ControlId::ExpDelete);
    assert!(a.commands.is_empty(), "never straight to the bin there");
    let card = s.explorer_manage().trash_there.expect("the card");
    assert_eq!(card.title, "Mandar 1 item para a Lixeira do MacBook do escritório?");
    assert_eq!(card.lines, ["Eles podem ser recuperados na Lixeira do MacBook do escritório."]);
    let ids: Vec<ControlId> = card.buttons.iter().map(|b| b.id).collect();
    assert_eq!(ids, [ControlId::ExpThereTrashCancel, ControlId::ExpThereTrashConfirm], "Cancelar left, Excluir right");
    assert_eq!(card.buttons[1].style, CardStyle::Danger);
    assert!(card.buttons.iter().all(|b| b.style != CardStyle::Primary), "never the default");
    // In the controls list: a dialog with its heading.
    let specs = s.controls(WindowKind::Viewer);
    let at = specs.iter().position(|c| c.id == ControlId::ExpThereTrashCancel).unwrap();
    assert_eq!(specs[at].region, Region::Dialog);
    assert!(
        specs[..at].iter().any(|c| c.role == UiRole::Heading && c.label == card.title && c.region == Region::Dialog)
    );
    // Shift+Delete does nothing there; Esc and Cancelar close the card without a command.
    assert!(s.explorer_key(ExplorerKey::DeleteForever).commands.is_empty());
    assert!(s.explorer_manage().forever.is_none());
    let a = s.activate(ControlId::ExpThereTrashCancel);
    assert!(a.commands.is_empty() && s.explorer_manage().trash_there.is_none());
    s.explorer_key(ExplorerKey::Delete);
    assert!(s.explorer_manage().trash_there.is_some());
    s.explorer_key(ExplorerKey::Escape);
    assert!(s.explorer_manage().trash_there.is_none());
    // Excluir sends Trash, then the folder is listed again.
    s.explorer_key(ExplorerKey::Delete);
    let a = s.activate(ControlId::ExpThereTrashConfirm);
    let trash = files_change(&s, RemoteChange::Trash { dir: String::new(), names: vec!["notas.txt".into()] });
    assert_eq!(a.commands, vec![trash.clone()]);
    answer(&mut s, &trash, Ok(Reply::ChangeResults(vec![("notas.txt".into(), ChangeOutcome::Done)])));
    assert!(matches!(s.take_followups().as_slice(), [Command::FilesListPage { start: 0, .. }]));
    assert!(s.explorer_pane(true).notice.is_none());
}

#[test]
fn the_trash_card_of_a_windows_peer_says_recycle_bin_in_english() {
    let mut s = Scenario::ExploradorLixeiraDeLa.prepared_state();
    let card = s.explorer_manage().trash_there.expect("the scenario's card");
    assert!(card.title.starts_with("Mandar 1 item para a Lixeira do "), "{}", card.title);
    s.set_language(Language::En);
    let card = s.explorer_manage().trash_there.expect("the card");
    assert!(card.title.contains("Recycle Bin"), "{}", card.title);
    let mut mac = opened_11();
    mac.set_language(Language::En);
    select_there(&mut mac, "notas.txt");
    mac.activate(ControlId::ExpDelete);
    assert!(mac.explorer_manage().trash_there.unwrap().title.contains("Trash"));
}

#[test]
fn a_taken_name_keeps_the_rename_open_and_a_new_folder_is_named_then_renamed() {
    let mut s = opened_11();
    select_there(&mut s, "notas.txt");
    s.activate(ControlId::ExpRename);
    assert_eq!(s.explorer_manage().renaming, Some(("notas.txt".into(), None)));
    assert!(s.explorer_manage().renaming_there);
    let a = s.submit_edit(EditId::ExpRename, "a:b".into());
    assert!(a.commands.is_empty(), "an invalid name never leaves the window");
    let a = s.submit_edit(EditId::ExpRename, "foto.jpg".into());
    let rename = files_change(
        &s,
        RemoteChange::Rename { dir: String::new(), name: "notas.txt".into(), new_name: "foto.jpg".into() },
    );
    assert_eq!(a.commands, vec![rename.clone()]);
    answer(&mut s, &rename, Ok(Reply::ChangeResults(vec![("notas.txt".into(), ChangeOutcome::Exists)])));
    let (name, error) = s.explorer_manage().renaming.expect("still open");
    assert_eq!((name.as_str(), error.as_deref()), ("notas.txt", Some("Já existe um item com esse nome aqui.")));
    assert!(s.take_followups().is_empty(), "nothing changed, nothing listed again");

    s.explorer_key(ExplorerKey::Escape);
    let a = s.activate(ControlId::ExpNewFolder);
    let mk = files_change(&s, RemoteChange::MakeDir { parent: String::new(), name: "Nova pasta".into() });
    assert_eq!(a.commands, vec![mk.clone()]);
    answer(&mut s, &mk, Ok(Reply::ChangeResults(vec![("Nova pasta".into(), ChangeOutcome::Done)])));
    let list = first_page_cmd(&mut s);
    answer(
        &mut s,
        &list,
        Ok(Reply::RemotePage(page(
            "",
            0,
            vec![remote("Nova pasta", RemoteKind::Dir, 0), remote("notas.txt", RemoteKind::File, 1)],
            None,
        ))),
    );
    assert_eq!(s.explorer_manage().renaming, Some(("Nova pasta".into(), None)), "its name opens for editing");
    // The second "Nova pasta" is numbered.
    s.explorer_key(ExplorerKey::Escape);
    let a = s.activate(ControlId::ExpNewFolder);
    assert!(
        matches!(change_cmd(&a), Command::FilesChange { change: RemoteChange::MakeDir { name, .. }, .. } if name == "Nova pasta (2)")
    );
}

#[test]
fn cut_and_paste_moves_inside_the_right_pane() {
    let mut s = opened_11();
    select_there(&mut s, "notas.txt");
    s.explorer_key(ExplorerKey::Cut);
    // A folder to paste into: open Contratos, then paste there.
    s.set_pointer_mods(PointerMods { clicks: 2, ..PointerMods::default() });
    let go = s.activate(ControlId::ExpRow(true, 0));
    let go = go.commands.into_iter().next().unwrap();
    assert!(matches!(&go, Command::FilesListPage { path, .. } if path == "Contratos"));
    answer(&mut s, &go, Ok(Reply::RemotePage(page("Contratos", 0, vec![], None))));
    let a = s.explorer_key(ExplorerKey::Paste);
    let mv = files_change(
        &s,
        RemoteChange::Move { from_dir: String::new(), names: vec!["notas.txt".into()], to_dir: "Contratos".into() },
    );
    assert_eq!(a.commands, vec![mv.clone()]);
    answer(&mut s, &mv, Ok(Reply::ChangeResults(vec![("notas.txt".into(), ChangeOutcome::InvalidName)])));
    assert_eq!(s.explorer_pane(true).notice.as_deref(), Some("Uma pasta não pode ir para dentro dela mesma."));
    // Paste with the focus on this computer moves nothing there.
    s.explorer.focus_there = false;
    assert!(s.explorer_key(ExplorerKey::Paste).commands.is_empty());
}

#[test]
fn a_trash_answered_no_bin_shows_the_notice_and_the_others_their_own() {
    let s = Scenario::ExploradorSemLixeira.prepared_state();
    assert_eq!(s.explorer_pane(true).notice.as_deref(), Some("Este local não tem Lixeira; nada foi excluído."));
    assert!(
        s.controls(WindowKind::Viewer)
            .iter()
            .any(|c| c.region == Region::Status && c.label.contains("não tem Lixeira"))
    );
    assert!(s.explorer_manage().trash_there.is_none(), "the card closed");

    let cases = [
        (ChangeOutcome::NotPermitted, "Sem permissão para mudar esse item."),
        (ChangeOutcome::NotFound, "O item não está mais aí."),
        (ChangeOutcome::Failed, "Não deu para concluir. Nada foi mudado."),
        (ChangeOutcome::NoSpace, "Não deu para concluir. Nada foi mudado."),
    ];
    for (outcome, text) in cases {
        let mut s = opened_11();
        select_there(&mut s, "notas.txt");
        s.activate(ControlId::ExpDelete);
        let a = s.activate(ControlId::ExpThereTrashConfirm);
        let c = change_cmd(&a);
        answer(&mut s, &c, Ok(Reply::ChangeResults(vec![("notas.txt".into(), outcome)])));
        assert_eq!(s.explorer_pane(true).notice.as_deref(), Some(text), "{outcome:?}");
    }
    let mut s = opened_11();
    s.activate(ControlId::ExpPlace(true, 5));
    let to_e = Command::FilesListPage { session_id: 1, path: ":3".into(), start: 0 };
    answer(&mut s, &to_e, Ok(Reply::RemotePage(page(":3", 0, vec![remote("a.jpg", RemoteKind::File, 5)], None))));
    select_there(&mut s, "a.jpg");
    let m = s.explorer_manage();
    assert!(!m.can_delete, "Excluir is disabled in advance on a volume without a bin");
    assert_eq!(m.delete_why.as_deref(), Some("Este local não tem Lixeira; nada foi excluído."));
    let a = s.activate(ControlId::ExpDelete);
    assert!(a.commands.is_empty() && s.explorer_manage().trash_there.is_none());
    // A volume that went away during the change says so.
    let mut s = opened_11();
    select_there(&mut s, "notas.txt");
    s.activate(ControlId::ExpDelete);
    let c = change_cmd(&s.activate(ControlId::ExpThereTrashConfirm));
    answer(&mut s, &c, Ok(Reply::ChangeResults(vec![("notas.txt".into(), ChangeOutcome::VolumeGone)])));
    assert!(s.explorer_pane(true).notice.is_some_and(|n| n.starts_with("O disco ") && n.contains("foi removido do")));
}

#[test]
fn changes_need_files_and_control_together() {
    let mut s = opened_11();
    select_there(&mut s, "notas.txt");
    assert!(s.explorer_manage().can_delete && s.explorer_manage().can_new_folder);
    // The session now holds FILES alone.
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: crate::mock::MAC.into(),
        state: SessionState::Active {
            since_ms: 1,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::View).with(Permission::Files),
            elevation: Default::default(),
        },
    });
    let m = s.explorer_manage();
    assert!(!m.can_new_folder && !m.can_rename && !m.can_delete);
    assert_eq!(
        m.change_why.as_deref(),
        Some("Mudar arquivos no MacBook do escritório precisa também da permissão de controlar.")
    );
    for id in [ControlId::ExpNewFolder, ControlId::ExpRename, ControlId::ExpDelete] {
        assert!(s.activate(id).commands.is_empty(), "{id:?}");
    }
    assert!(s.explorer_manage().trash_there.is_none());
    // Browsing and copying stay.
    assert!(s.explorer_pane(true).error.is_none() && s.explorer_pane(true).rows.len() == 3);
    // CONTROL alone is not enough either.
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: crate::mock::MAC.into(),
        state: SessionState::Active {
            since_ms: 1,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::Control),
            elevation: Default::default(),
        },
    });
    assert!(!s.explorer_manage().can_new_folder);
}

// ----- the conflict card -----

#[test]
fn a_conflict_asks_once_keep_both_is_the_default_and_skip_leaves_the_name_out() {
    let mut s = opened_11();
    // foto.jpg exists on both sides; novo.txt only here.
    s.explorer.focus_there = false;
    select_here(&mut s, "foto.jpg");
    let a = s.activate(ControlId::ExpCopy);
    assert!(a.commands.is_empty(), "nothing is sent before the answer");
    let card = s.explorer_manage().conflict.expect("the card");
    assert_eq!(card.title, "“foto.jpg” já existe em Pasta pessoal");
    assert!(card.check.is_none(), "one name: no check");
    let ids: Vec<ControlId> = card.buttons.iter().map(|b| b.id).collect();
    assert_eq!(
        ids,
        [
            ControlId::ExpConflictCancel,
            ControlId::ExpConflictSkip,
            ControlId::ExpConflictReplace,
            ControlId::ExpConflictKeepBoth
        ]
    );
    let styles: Vec<CardStyle> = card.buttons.iter().map(|b| b.style).collect();
    assert_eq!(styles, [CardStyle::Default, CardStyle::Default, CardStyle::Danger, CardStyle::Primary]);
    assert!(card.buttons.iter().all(|b| b.enabled), "a 1.1 peer with FILES and CONTROL and a bin");
    let specs = s.controls(WindowKind::Viewer);
    assert!(specs.iter().any(|c| c.id == ControlId::ExpConflictKeepBoth && c.region == Region::Dialog));
    // Keep both: the copy goes as before and the engine saves "foto (2).jpg".
    let a = s.activate(ControlId::ExpConflictKeepBoth);
    assert_eq!(
        a.commands,
        vec![Command::FilesSend {
            session_id: 1,
            local_paths: vec![PathBuf::from("C:/u/Downloads").join("foto.jpg")],
            remote_dir: Some(String::new())
        }]
    );
    assert!(s.explorer_manage().conflict.is_none());
    // Skip: nothing of that name goes.
    select_here(&mut s, "foto.jpg");
    s.activate(ControlId::ExpCopy);
    let a = s.activate(ControlId::ExpConflictSkip);
    assert!(a.commands.is_empty());
    // Cancelar drops the job.
    s.activate(ControlId::ExpCopy);
    let a = s.activate(ControlId::ExpConflictCancel);
    assert!(a.commands.is_empty() && s.explorer_manage().conflict.is_none());
}

#[test]
fn several_conflicts_ask_once_for_all_or_one_by_one() {
    let mut s = opened_11();
    reply(
        &mut s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("foto.jpg", false, 1), entry("notas.txt", false, 1), entry("novo.txt", false, 1)],
            truncated: false,
            hidden: Vec::new(),
        }),
    );
    s.explorer.focus_there = false;
    for n in 0..3 {
        s.set_pointer_mods(PointerMods { toggle: true, ..PointerMods::default() });
        s.activate(ControlId::ExpRow(false, n));
    }
    s.activate(ControlId::ExpCopy);
    let card = s.explorer_manage().conflict.expect("the card");
    assert_eq!(card.title, "2 itens já existem em Pasta pessoal");
    assert_eq!(card.check.as_ref().map(|c| c.2), Some(true), "checked when more than one");
    // Applied to all: one answer, both names skipped, novo.txt still goes.
    let a = s.activate(ControlId::ExpConflictSkip);
    assert_eq!(
        a.commands,
        vec![Command::FilesSend {
            session_id: 1,
            local_paths: vec![PathBuf::from("C:/u/Downloads").join("novo.txt")],
            remote_dir: Some(String::new())
        }]
    );
    // Unchecked: the first name only, then the next is asked.
    s.activate(ControlId::ExpCopy);
    s.activate(ControlId::ExpConflictApplyAll);
    let a = s.activate(ControlId::ExpConflictSkip);
    assert!(a.commands.is_empty());
    assert_eq!(s.explorer_manage().conflict.unwrap().title, "“notas.txt” já existe em Pasta pessoal");
}

#[test]
fn replace_sends_the_old_file_to_the_bin_first_in_both_directions() {
    // Upload: Trash there, the copy only after it answered Done.
    let mut s = opened_11();
    s.explorer.focus_there = false;
    select_here(&mut s, "foto.jpg");
    s.activate(ControlId::ExpCopy);
    let a = s.activate(ControlId::ExpConflictReplace);
    let trash = files_change(&s, RemoteChange::Trash { dir: String::new(), names: vec!["foto.jpg".into()] });
    assert_eq!(a.commands, vec![trash.clone()], "the bin first, no copy yet");
    answer(&mut s, &trash, Ok(Reply::ChangeResults(vec![("foto.jpg".into(), ChangeOutcome::Done)])));
    let after = s.take_followups();
    assert!(
        after.iter().any(|c| matches!(c, Command::FilesSend { remote_dir: Some(d), .. } if d.is_empty())),
        "{after:?}"
    );
    // A bin that refuses keeps the old file and sends nothing.
    select_here(&mut s, "foto.jpg");
    s.activate(ControlId::ExpCopy);
    let a = s.activate(ControlId::ExpConflictReplace);
    let trash = change_cmd(&a);
    answer(&mut s, &trash, Ok(Reply::ChangeResults(vec![("foto.jpg".into(), ChangeOutcome::NoBin)])));
    assert!(!s.take_followups().iter().any(|c| matches!(c, Command::FilesSend { .. })));
    assert!(s.explorer_pane(true).notice.is_some());

    // Download: the local bin first, then FilesGet.
    let mut s = opened_11();
    s.explorer.focus_there = true;
    reply(
        &mut s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("foto.jpg", false, 1)],
            truncated: false,
            hidden: Vec::new(),
        }),
    );
    select_there(&mut s, "foto.jpg");
    s.activate(ControlId::ExpCopy);
    let a = s.activate(ControlId::ExpConflictReplace);
    let local = Command::LocalTrash { paths: vec![PathBuf::from("C:/u/Downloads").join("foto.jpg")] };
    assert_eq!(a.commands, vec![local.clone()]);
    answer(&mut s, &local, Ok(Reply::Done));
    let after = s.take_followups();
    assert!(after.contains(&Command::FilesGet {
        session_id: 1,
        remote_path: "foto.jpg".into(),
        local_dir: PathBuf::from("C:/u/Downloads"),
    }));
}

#[test]
fn replace_is_disabled_with_its_reason_without_a_1_1_peer_control_or_a_bin() {
    // A 1.0 peer.
    let mut s = opened_10();
    reply(
        &mut s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("nota.txt", false, 1)],
            truncated: false,
            hidden: Vec::new(),
        }),
    );
    s.explorer.focus_there = false;
    s.activate(ControlId::ExpRow(false, 0));
    s.activate(ControlId::ExpCopy);
    let card = s.explorer_manage().conflict.expect("the card");
    let replace = card.buttons.iter().find(|b| b.id == ControlId::ExpConflictReplace).unwrap();
    assert!(!replace.enabled && replace.why.as_deref().is_some_and(|w| w.contains("usa o Swapdesk 1.0")));
    assert!(s.activate(ControlId::ExpConflictReplace).commands.is_empty(), "a disabled button does nothing");
    assert!(s.explorer_manage().conflict.is_some(), "the card stays");
    let specs = s.controls(WindowKind::Viewer);
    let b = specs.iter().find(|c| c.id == ControlId::ExpConflictReplace).unwrap();
    assert!(b.disabled && b.description.is_some());
    // Keep both is still there.
    assert!(!s.activate(ControlId::ExpConflictKeepBoth).commands.is_empty());

    // A volume without a bin.
    let mut s = opened_11();
    s.activate(ControlId::ExpPlace(true, 5));
    let to_e = Command::FilesListPage { session_id: 1, path: ":3".into(), start: 0 };
    answer(&mut s, &to_e, Ok(Reply::RemotePage(page(":3", 0, vec![remote("foto.jpg", RemoteKind::File, 5)], None))));
    s.explorer.focus_there = false;
    select_here(&mut s, "foto.jpg");
    s.activate(ControlId::ExpCopy);
    let card = s.explorer_manage().conflict.expect("the card");
    let replace = card.buttons.iter().find(|b| b.id == ControlId::ExpConflictReplace).unwrap();
    assert!(!replace.enabled);
    assert_eq!(replace.why.as_deref(), Some("Este local não tem Lixeira; nada foi excluído."));
}

// ----- folder upload -----

#[test]
fn a_folder_upload_makes_the_folders_before_sending_their_files() {
    let mut s = opened_11();
    s.explorer.focus_there = false;
    let n = s.explorer_pane(false).rows.iter().position(|r| r.name == "Fotos").unwrap();
    s.activate(ControlId::ExpRow(false, n as u16));
    assert!(s.explorer_can_copy(), "a folder can go to a 1.1 peer");
    let a = s.activate(ControlId::ExpCopy);
    let mk = files_change(&s, RemoteChange::MakeDir { parent: String::new(), name: "Fotos".into() });
    assert_eq!(a.commands, vec![mk.clone()], "the folder is made first, nothing is sent yet");
    answer(&mut s, &mk, Ok(Reply::ChangeResults(vec![("Fotos".into(), ChangeOutcome::Done)])));
    let local = PathBuf::from("C:/u/Downloads").join("Fotos");
    let f = s.take_followups();
    assert_eq!(f, vec![Command::LocalList { path: Some(local.clone()) }]);
    // The walk's listing is not the left pane's: the pane keeps its folder.
    answer(
        &mut s,
        &f[0],
        Ok(Reply::LocalListing(LocalListing {
            path: local.clone(),
            entries: vec![entry("a.jpg", false, 1), entry("Verão", true, 0)],
            truncated: false,
            hidden: vec![entry(".b", false, 1)],
        })),
    );
    assert_eq!(s.explorer.here_dir, Some(PathBuf::from("C:/u/Downloads")));
    let f = s.take_followups();
    assert_eq!(
        f,
        vec![
            Command::FilesSend {
                session_id: 1,
                local_paths: vec![local.join("a.jpg"), local.join(".b")],
                remote_dir: Some("Fotos".into())
            },
            files_change(&s, RemoteChange::MakeDir { parent: "Fotos".into(), name: "Verão".into() }),
        ],
        "the files of a folder go once it exists, then the next folder is made"
    );
    // The subfolder: made (it exists already: merged into), listed, sent.
    answer(&mut s, &f[1], Ok(Reply::ChangeResults(vec![("Verão".into(), ChangeOutcome::Exists)])));
    let l = s.take_followups();
    assert_eq!(l, vec![Command::LocalList { path: Some(local.join("Verão")) }]);
    answer(
        &mut s,
        &l[0],
        Ok(Reply::LocalListing(LocalListing {
            path: local.join("Verão"),
            entries: vec![entry("praia.jpg", false, 1)],
            truncated: false,
            hidden: Vec::new(),
        })),
    );
    assert_eq!(
        s.take_followups(),
        vec![Command::FilesSend {
            session_id: 1,
            local_paths: vec![local.join("Verão").join("praia.jpg")],
            remote_dir: Some("Fotos/Verão".into())
        }]
    );
    assert!(s.explorer.there.walk.is_none(), "the walk is over");
    assert!(s.explorer_transfers_open());
}

#[test]
fn a_folder_upload_stops_when_a_folder_cannot_be_made() {
    let mut s = opened_11();
    s.explorer.focus_there = false;
    let n = s.explorer_pane(false).rows.iter().position(|r| r.name == "Fotos").unwrap();
    s.activate(ControlId::ExpRow(false, n as u16));
    let mk = change_cmd(&s.activate(ControlId::ExpCopy));
    answer(&mut s, &mk, Ok(Reply::ChangeResults(vec![("Fotos".into(), ChangeOutcome::NotPermitted)])));
    assert!(s.take_followups().is_empty(), "nothing is listed or sent");
    assert_eq!(s.explorer_pane(true).notice.as_deref(), Some("Sem permissão para mudar esse item."));
    assert!(s.explorer.there.walk.is_none());
}

#[test]
fn a_folder_upload_needs_control_and_a_1_1_peer() {
    let mut s = opened_11();
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: crate::mock::MAC.into(),
        state: SessionState::Active {
            since_ms: 1,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::View).with(Permission::Files),
            elevation: Default::default(),
        },
    });
    s.explorer.focus_there = false;
    let n = s.explorer_pane(false).rows.iter().position(|r| r.name == "Fotos").unwrap();
    s.activate(ControlId::ExpRow(false, n as u16));
    let a = s.activate(ControlId::ExpCopy);
    assert!(a.commands.is_empty());
    assert!(s.explorer_note().is_some_and(|n| n.contains("precisa também da permissão de controlar")));
}

// ----- the folder download and the right pane's properties (FILES-UI.md 5, 3.2) -----

fn list_cmd(s: &UiState, path: &str, start: u32) -> Command {
    Command::FilesListPage { session_id: s.session_id().unwrap(), path: path.into(), start }
}

fn make_dir(path: PathBuf) -> Command {
    Command::LocalMakeDir { path }
}

fn get(path: &str, dir: PathBuf) -> Command {
    Command::FilesGet { session_id: 1, remote_path: path.into(), local_dir: dir }
}

fn page_of(s: &mut UiState, path: &str, start: u32, entries: Vec<RemoteEntry>, next: Option<u32>) {
    let c = list_cmd(s, path, start);
    answer(s, &c, Ok(Reply::RemotePage(page(path, start, entries, next))));
}

fn copy_folder_down(s: &mut UiState, name: &str) -> Activation {
    s.explorer.focus_there = true;
    select_there(s, name);
    s.activate(ControlId::ExpCopy)
}

fn select_there_more(s: &mut UiState, name: &str) {
    s.explorer.panes[1].selected.insert(name.to_string());
}

#[test]
fn a_folder_download_makes_the_local_folders_before_fetching_the_files_into_them() {
    let mut s = opened_11();
    s.explorer.focus_there = true;
    select_there(&mut s, "Contratos");
    assert!(s.explorer_can_copy(), "a folder can come from a 1.1 peer");
    let a = s.activate(ControlId::ExpCopy);
    assert_eq!(a.commands, vec![list_cmd(&s, "Contratos", 0)], "the scan starts; nothing is made or fetched");
    let reading = s.there_notice_text().expect("the wait is said");
    assert!(reading.starts_with("Lendo as pastas do "), "{reading}");
    page_of(
        &mut s,
        "Contratos",
        0,
        vec![remote("a.txt", RemoteKind::File, 1), remote("Sub", RemoteKind::Dir, 0)],
        Some(2),
    );
    assert_eq!(s.take_followups(), vec![list_cmd(&s, "Contratos", 2)], "every page of a folder");
    page_of(&mut s, "Contratos", 2, vec![remote("b.txt", RemoteKind::File, 1)], None);
    assert_eq!(s.take_followups(), vec![list_cmd(&s, "Contratos/Sub", 0)]);
    page_of(&mut s, "Contratos/Sub", 0, vec![remote("c.txt", RemoteKind::File, 1)], None);
    let root = PathBuf::from("C:/u/Downloads").join("Contratos");
    let f = s.take_followups();
    assert_eq!(f, vec![make_dir(root.clone())], "the scan is over: the folders are made, on the worker");
    answer(&mut s, &f[0], Ok(Reply::Done));
    let f = s.take_followups();
    assert_eq!(f, vec![make_dir(root.join("Sub"))], "one at a time, parents first");
    answer(&mut s, &f[0], Err(ApiError::new(ApiCode::Refused)));
    assert!(s.explorer.there.notice.is_none(), "a folder that exists is merged into");
    let f = s.take_followups();
    assert_eq!(
        &f[..3],
        &[
            get("Contratos/a.txt", root.clone()),
            get("Contratos/b.txt", root.clone()),
            get("Contratos/Sub/c.txt", root.join("Sub")),
        ],
        "the files go into their folders, in order"
    );
    assert!(matches!(f.get(3), Some(Command::LocalList { .. })), "the left pane lists the new folders");
    assert!(s.explorer.there.download.is_none());
    assert!(s.explorer_transfers_open());
    assert!(s.there_notice_text().is_none_or(|t| !t.starts_with("Lendo")), "the reading line goes with the scan");
}

#[test]
fn a_folder_download_asks_about_the_files_that_exist_and_can_be_cancelled() {
    let mut s = opened_11();
    s.explorer.focus_there = true;
    select_there(&mut s, "foto.jpg");
    select_there_more(&mut s, "Contratos");
    let a = s.activate(ControlId::ExpCopy);
    assert!(a.commands.is_empty(), "foto.jpg exists here: the card asks first");
    let a = s.activate(ControlId::ExpConflictKeepBoth);
    assert_eq!(a.commands, vec![list_cmd(&s, "Contratos", 0)]);
    // The file of the selection goes after the folders are made, not before.
    page_of(&mut s, "Contratos", 0, vec![], None);
    let f = s.take_followups();
    assert_eq!(f.len(), 1);
    answer(&mut s, &f[0], Ok(Reply::Done));
    let f = s.take_followups();
    assert_eq!(f[0], get("foto.jpg", PathBuf::from("C:/u/Downloads")));
    // Esc during the scan stops it.
    let mut s = opened_11();
    copy_folder_down(&mut s, "Contratos");
    s.explorer_key(ExplorerKey::Escape);
    assert!(s.explorer.there.download.is_none());
    page_of(&mut s, "Contratos", 0, vec![remote("a.txt", RemoteKind::File, 1)], None);
    assert!(s.take_followups().iter().all(|c| !matches!(c, Command::LocalMakeDir { .. })));
}

#[test]
fn a_folder_download_over_the_limits_does_not_start() {
    let mut s = opened_11();
    copy_folder_down(&mut s, "Contratos");
    let many: Vec<RemoteEntry> = (0..10_001).map(|i| remote(&format!("f{i}"), RemoteKind::File, 1)).collect();
    page_of(&mut s, "Contratos", 0, many, None);
    assert!(s.take_followups().is_empty(), "nothing is made or fetched");
    assert!(s.explorer.there.download.is_none());
    assert_eq!(
        s.explorer_pane(true).notice.as_deref(),
        Some("Esta cópia passa do limite de 10\u{a0}000 arquivos ou 32 níveis de pastas. Nada foi copiado.")
    );
    // 32 levels: the 32nd folder still holds a folder.
    let mut s = opened_11();
    copy_folder_down(&mut s, "Contratos");
    let mut path = "Contratos".to_string();
    for level in 1..=32 {
        page_of(&mut s, &path, 0, vec![remote("d", RemoteKind::Dir, 0)], None);
        if level < 32 {
            assert_eq!(s.take_followups().len(), 1, "level {level} goes on");
            path = format!("{path}/d");
        }
    }
    assert!(s.take_followups().is_empty(), "nothing is made");
    assert!(s.explorer.there.download.is_none());
    assert!(s.explorer_pane(true).notice.is_some_and(|n| n.contains("32 níveis")));
}

#[test]
fn a_folder_download_from_a_1_0_host_is_unchanged() {
    let mut s = opened_10();
    s.explorer.focus_there = true;
    select_there(&mut s, "Documents");
    assert!(!s.explorer_can_copy(), "folders only: nothing to copy");
    let a = s.activate(ControlId::ExpCopy);
    assert!(a.commands.is_empty());
    assert!(s.explorer.there.download.is_none());
    select_there_more(&mut s, "nota.txt");
    let a = s.activate(ControlId::ExpCopy);
    assert_eq!(a.commands, vec![get("nota.txt", PathBuf::from("C:/u/Downloads"))], "files only, as before");
    assert!(s.explorer_note().is_some(), "and the note says folders are left out");
}

#[test]
fn properties_of_a_remote_file_come_from_the_listing() {
    let mut s = opened_11();
    s.explorer.focus_there = true;
    s.explorer.there.entries.iter_mut().find(|e| e.name == "notas.txt").unwrap().mtime_ms = 1_700_000_000_000;
    s.refresh_there_rows();
    select_there(&mut s, "notas.txt");
    assert!(s.explorer_manage().can_props, "enabled on the right pane with one selection");
    let a = s.activate(ControlId::ExpProps);
    assert!(a.commands.is_empty(), "a file needs no walk");
    let (title, lines) = s.explorer_manage().props.expect("the card");
    assert_eq!(title, "Propriedades de notas.txt");
    assert!(lines.contains(&"Tipo: Documento de texto".to_string()), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("Tamanho: 1 KB")), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("Modificado: ")), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("Livre neste disco")), "{lines:?}");
    let specs = s.controls(WindowKind::Viewer);
    assert!(specs.iter().any(|c| c.id == ControlId::ExpPropsClose && c.region == Region::Dialog));
    s.activate(ControlId::ExpPropsClose);
    assert!(s.explorer_manage().props.is_none());
}

#[test]
fn properties_of_a_remote_folder_count_pages_and_subfolders() {
    let mut s = opened_11();
    s.explorer.focus_there = true;
    select_there(&mut s, "Contratos");
    let a = s.activate(ControlId::ExpProps);
    assert_eq!(a.commands, vec![list_cmd(&s, "Contratos", 0)]);
    assert!(s.explorer_manage().props.unwrap().1.contains(&"Contando…".to_string()));
    page_of(
        &mut s,
        "Contratos",
        0,
        vec![remote("a", RemoteKind::File, 100), remote("Sub", RemoteKind::Dir, 0)],
        Some(2),
    );
    assert_eq!(s.take_followups(), vec![list_cmd(&s, "Contratos", 2)]);
    page_of(&mut s, "Contratos", 2, vec![remote("b", RemoteKind::File, 200)], None);
    assert_eq!(s.take_followups(), vec![list_cmd(&s, "Contratos/Sub", 0)]);
    assert!(s.explorer_manage().props.unwrap().1.contains(&"Contando…".to_string()));
    page_of(&mut s, "Contratos/Sub", 0, vec![remote("c", RemoteKind::File, 300)], None);
    assert!(s.take_followups().is_empty());
    let lines = s.explorer_manage().props.unwrap().1;
    let size = super::super::explorer::human_size(Language::PtBr, 600);
    assert!(lines.contains(&format!("3 arquivos e 1 pasta, {size}")), "{lines:?}");
    assert!(!lines.contains(&"Contando…".to_string()));
    assert!(lines.contains(&"Tipo: Pasta".to_string()));
    assert_eq!(s.explorer.there.folder, "", "the pane's own listing was not touched by the count");
}

#[test]
fn properties_are_enabled_on_the_right_pane_for_one_selection_or_none_only() {
    let mut s = opened_11();
    s.explorer.focus_there = true;
    assert!(s.explorer_manage().can_props, "nothing selected: the folder shown");
    select_there(&mut s, "notas.txt");
    assert!(s.explorer_manage().can_props);
    select_there_more(&mut s, "foto.jpg");
    assert!(!s.explorer_manage().can_props, "two rows: disabled");
    let a = s.activate(ControlId::ExpProps);
    assert!(a.commands.is_empty() && s.explorer_manage().props.is_none());
    // A 1.0 peer lists no pages to count with.
    let mut s = opened_10();
    s.explorer.focus_there = true;
    assert!(!s.explorer_manage().can_props);
    // The shown folder, when nothing is selected; Esc closes the card.
    let mut s = opened_11();
    s.explorer.focus_there = true;
    let a = s.activate(ControlId::ExpProps);
    assert_eq!(a.commands, vec![list_cmd(&s, "", 0)]);
    s.explorer_key(ExplorerKey::Escape);
    assert!(s.explorer_manage().props.is_none());
}

// ----- names a host sends (security review 13/11, items 1, 6, 10a) -----

fn hostile_dirs() -> Vec<&'static str> {
    vec![
        r"..\..\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup",
        r"C:\Users\ana\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup",
        r"\Windows\Temp",
        "../../Library/LaunchAgents",
        "..",
        "C:",
    ]
}

/// Whether `p` lies under `root` by plain components (no `..` in it).
fn inside(root: &std::path::Path, p: &std::path::Path) -> bool {
    p.starts_with(root) && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Answers every command of a download as a malicious host and a full disk would: each folder listed holds one
/// `payload.exe`, each local folder "already exists" (the merge). Returns every command that was sent.
fn drain_download(s: &mut UiState, first: Vec<Command>) -> Vec<Command> {
    let mut todo: std::collections::VecDeque<Command> = first.into();
    let mut sent = Vec::new();
    for _ in 0..500 {
        todo.extend(s.take_followups());
        let Some(c) = todo.pop_front() else { return sent };
        match &c {
            Command::FilesListPage { path, start, .. } => {
                let p = page(path, *start, vec![remote("payload.exe", RemoteKind::File, 1)], None);
                answer(s, &c, Ok(Reply::RemotePage(p)));
            }
            Command::LocalMakeDir { .. } => answer(s, &c, Err(ApiError::new(ApiCode::Refused))),
            _ => {}
        }
        sent.push(c);
    }
    panic!("the download never ended: {} commands so far", sent.len());
}

#[test]
fn a_hostile_host_cannot_steer_a_folder_download_out_of_the_chosen_folder() {
    let mut s = opened_11();
    let a = copy_folder_down(&mut s, "Contratos");
    assert_eq!(a.commands, vec![list_cmd(&s, "Contratos", 0)]);
    let mut entries: Vec<RemoteEntry> = hostile_dirs().into_iter().map(|n| remote(n, RemoteKind::Dir, 0)).collect();
    entries.push(remote("Fine", RemoteKind::Dir, 0));
    entries.push(remote("payload.exe", RemoteKind::File, 1));
    page_of(&mut s, "Contratos", 0, entries, None);
    let sent = drain_download(&mut s, Vec::new());
    let root = PathBuf::from("C:/u/Downloads").join("Contratos");
    let mut made = Vec::new();
    for c in &sent {
        match c {
            Command::LocalMakeDir { path } => {
                assert!(inside(&root, path), "a folder outside the chosen one: {path:?}");
                made.push(path.clone());
            }
            Command::FilesGet { local_dir, remote_path, .. } => {
                assert!(inside(&root, local_dir), "a file into {local_dir:?}");
                assert!(
                    remote_path.split('/').all(|p| p != ".." && !p.contains('\\') && !p.contains(':')),
                    "{remote_path}"
                );
            }
            Command::FilesListPage { path, .. } => {
                assert!(
                    path.split('/').all(|p| p != ".." && !p.contains('\\') && !p.contains(':')),
                    "asked for {path}"
                );
            }
            _ => {}
        }
    }
    assert_eq!(made, vec![root.clone(), root.join("Fine")], "only the honest folder is made, under the picked one");
    assert!(
        sent.iter().any(|c| matches!(c, Command::FilesGet { local_dir, remote_path, .. }
            if *local_dir == root.join("Fine") && remote_path == "Contratos/Fine/payload.exe")),
        "the honest files still come: {sent:?}"
    );
    assert!(s.explorer.there.notice.is_some(), "the person is told that names were left out");
}

#[test]
fn the_right_pane_never_lists_a_name_that_is_not_one_valid_component() {
    let mut s = opened_11();
    let mut entries: Vec<RemoteEntry> = hostile_dirs().into_iter().map(|n| remote(n, RemoteKind::Dir, 0)).collect();
    entries.push(remote(r"..\..\thesis.docx", RemoteKind::File, 5));
    entries.push(remote("ok.txt", RemoteKind::File, 5));
    let c = list_cmd(&s, "", 0);
    s.explorer.there.want = Some(String::new());
    answer(&mut s, &c, Ok(Reply::RemotePage(page("", 0, entries, None))));
    assert_eq!(there_rows(&s), ["ok.txt"]);
    assert!(s.explorer.there.notice.is_some());
}

#[test]
fn a_1_0_listing_is_cleaned_of_such_names_too() {
    let mut s = opened_10();
    reply(
        &mut s,
        Reply::Listing(Listing {
            path: String::new(),
            entries: vec![entry(r"..\..\x", true, 0), entry("../y", false, 1), entry("Documents", true, 0)],
            truncated: false,
        }),
    );
    assert_eq!(there_rows(&s), ["Documents"]);
}

#[test]
fn a_page_that_asks_for_itself_again_or_overruns_the_limit_stops_the_scan_at_once() {
    // next == start would ask the same page forever.
    let mut s = opened_11();
    copy_folder_down(&mut s, "Contratos");
    page_of(&mut s, "Contratos", 0, vec![remote("a.txt", RemoteKind::File, 1)], Some(0));
    assert!(s.take_followups().is_empty(), "no page is asked again");
    assert!(s.explorer.there.download.is_none());
    assert!(s.explorer.there.notice.is_some());
    // A step backwards is the same.
    let mut s = opened_11();
    copy_folder_down(&mut s, "Contratos");
    page_of(&mut s, "Contratos", 0, vec![remote("a.txt", RemoteKind::File, 1)], Some(1));
    assert_eq!(s.take_followups(), vec![list_cmd(&s, "Contratos", 1)]);
    page_of(&mut s, "Contratos", 1, vec![remote("b.txt", RemoteKind::File, 1)], Some(1));
    assert!(s.take_followups().is_empty() && s.explorer.there.download.is_none());
    // The limit holds inside one folder, before its last page: nothing past it is kept or asked.
    let mut s = opened_11();
    copy_folder_down(&mut s, "Contratos");
    let many: Vec<RemoteEntry> = (0..10_001).map(|i| remote(&format!("f{i}"), RemoteKind::File, 1)).collect();
    page_of(&mut s, "Contratos", 0, many, Some(10_001));
    assert!(s.take_followups().is_empty(), "the next page is not asked");
    assert!(s.explorer.there.download.is_none());
    assert!(s.explorer.there.notice.as_ref().is_some_and(|n| s.tm(n).contains("limite")));
    // The properties count stops the same way, with what it has.
    let mut s = opened_11();
    s.explorer.focus_there = true;
    let a = s.activate(ControlId::ExpProps);
    assert_eq!(a.commands, vec![list_cmd(&s, "", 0)]);
    page_of(&mut s, "", 0, vec![remote("a.txt", RemoteKind::File, 1)], Some(0));
    assert!(s.take_followups().is_empty(), "no page is asked again");
    assert!(s.explorer.there.props.as_ref().is_some_and(|p| p.scan.is_none()), "the count is over");
}

#[test]
fn a_name_this_computer_cannot_hold_is_not_made_even_when_the_hosts_rule_lets_it_through() {
    // `what?` passes the shared rule (a Mac may name a folder so) but Windows refuses it: the one checked join
    // leaves it out of the folders made, on a Windows viewer.
    if !cfg!(windows) {
        return;
    }
    let mut s = opened_11();
    copy_folder_down(&mut s, "Contratos");
    page_of(&mut s, "Contratos", 0, vec![remote("what?", RemoteKind::Dir, 0), remote("ok", RemoteKind::Dir, 0)], None);
    let sent = drain_download(&mut s, Vec::new());
    let root = PathBuf::from("C:/u/Downloads").join("Contratos");
    let made: Vec<PathBuf> = sent
        .iter()
        .filter_map(|c| if let Command::LocalMakeDir { path } = c { Some(path.clone()) } else { None })
        .collect();
    assert_eq!(made, vec![root.clone(), root.join("ok")]);
    assert!(s.explorer.there.notice.is_some());
}

fn local_with_folder(s: &mut UiState, name: &str) {
    reply(
        s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry(name, true, 0), entry("foto.jpg", false, 10)],
            truncated: false,
            hidden: Vec::new(),
        }),
    );
}

#[test]
fn a_downloaded_folder_that_exists_here_asks_before_it_merges() {
    let mut s = opened_11();
    local_with_folder(&mut s, "Contratos");
    let a = copy_folder_down(&mut s, "Contratos");
    assert!(a.commands.is_empty(), "nothing is scanned or made before the answer");
    let card = s.explorer_manage().conflict.expect("the card asks about the folder");
    assert_eq!(card.title, "“Contratos” já existe em Downloads");
    let replace = card.buttons.iter().find(|b| b.id == ControlId::ExpConflictReplace).unwrap();
    assert!(!replace.enabled, "a folder is merged or skipped, never replaced");
    assert!(s.activate(ControlId::ExpConflictReplace).commands.is_empty());
    assert!(s.explorer_manage().conflict.is_some(), "the card stays");
    // Cancelar: nothing happens.
    let a = s.activate(ControlId::ExpConflictCancel);
    assert!(a.commands.is_empty() && s.explorer.there.download.is_none());
    // Pular: the folder is left out.
    copy_folder_down(&mut s, "Contratos");
    let a = s.activate(ControlId::ExpConflictSkip);
    assert!(a.commands.is_empty() && s.explorer.there.download.is_none(), "{:?}", a.commands);
    // The primary answer merges: the scan starts, as it did before the card.
    copy_folder_down(&mut s, "Contratos");
    let a = s.activate(ControlId::ExpConflictKeepBoth);
    assert_eq!(a.commands, vec![list_cmd(&s, "Contratos", 0)]);
}

#[test]
fn a_folder_that_does_not_exist_here_still_downloads_without_asking() {
    let mut s = opened_11();
    local_with_folder(&mut s, "Outra");
    let a = copy_folder_down(&mut s, "Contratos");
    assert_eq!(a.commands, vec![list_cmd(&s, "Contratos", 0)]);
    assert!(s.explorer_manage().conflict.is_none());
}

// ----- the scenarios -----

#[test]
fn the_other_disk_scenario_shows_the_removable_volume_and_its_neighbours() {
    let s = Scenario::ExploradorOutroDisco.prepared_state();
    let labels = labels(&s);
    assert!(labels.contains(&"Backup (D:)".to_string()) && labels.contains(&"Unidade removível (E:)".to_string()));
    let v = s.explorer_pane(true);
    assert_eq!(v.crumbs, ["Unidade removível (E:)"]);
    assert_eq!(v.rows.len(), 5);
    assert!(v.free.is_some_and(|f| f.contains("livres")));
    assert!(s.explorer_places().iter().any(|p| p.there && p.selected && p.label == "Unidade removível (E:)"));
    assert!(!s.explorer_manage().can_delete || s.explorer_manage().delete_why.is_none());
}
