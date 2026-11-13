//! Slice 3: the painted drag inside the app and the context menu (FILES-UI.md 3.2, 4.1 and 10).

use std::path::PathBuf;

use super::*;
use crate::PointerMods;
use crate::api::{
    ApiCode, ApiError, DirEntryView, Listing, LocalListing, Permission, Permissions, PlaceName, RemoteEntry,
    RemoteKind, RemotePage, RemotePlace, RemoteVolume, RemoteVolumes, Reply, Role, SessionState, Ticket, UiEvent,
    VolumeType,
};
use crate::interact::explorer_manage::ExplorerKey;
use crate::mock::Scenario;
use crate::state::Effect;

pub(super) fn entry(name: &str, is_dir: bool) -> DirEntryView {
    DirEntryView { name: name.into(), is_dir, size: 10, mtime_ms: 0 }
}

fn remote(name: &str, kind: RemoteKind) -> RemoteEntry {
    RemoteEntry { name: name.into(), kind, size: 10, mtime_ms: 0, hidden: false, system: false, read_only: false }
}

fn reply(s: &mut UiState, r: Reply) {
    s.apply(UiEvent::Reply { ticket: Ticket(0), result: Ok(r) });
}

fn answer(s: &mut UiState, command: &Command, result: Result<Reply, ApiError>) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(70_000);
    let t = Ticket(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    s.track(t, command);
    s.apply(UiEvent::Reply { ticket: t, result });
}

fn volumes_11() -> RemoteVolumes {
    let vol = |id, kind, label: &str, letter, has_bin| RemoteVolume {
        id,
        kind,
        label: label.into(),
        letter: Some(letter),
        total_bytes: 500_000_000_000,
        free_bytes: 120_000_000_000,
        has_bin,
    };
    RemoteVolumes {
        home: ":1/Users/ana".into(),
        places: vec![RemotePlace { kind: PlaceName::Desktop, path: ":1/Users/ana/Desktop".into() }],
        volumes: vec![vol(1, VolumeType::System, "", 'C', true)],
    }
}

/// This computer in `C:/u/Downloads`: row 0 the folder "Fotos", then "foto.jpg" and "novo.txt".
fn local_listing(s: &mut UiState) {
    reply(
        s,
        Reply::LocalListing(LocalListing {
            path: PathBuf::from("C:/u/Downloads"),
            entries: vec![entry("foto.jpg", false), entry("Fotos", true), entry("novo.txt", false)],
            truncated: false,
            hidden: Vec::new(),
        }),
    );
}

/// A 1.1 peer, FILES and CONTROL granted: the home folder shows "Contratos" (row 0), "foto.jpg", "notas.txt".
pub(super) fn opened_11() -> UiState {
    let mut s = Scenario::Visualizador.prepared_state();
    let a = s.activate(ControlId::TbFiles);
    local_listing(&mut s);
    let volumes = a.commands.iter().find(|c| matches!(c, Command::FilesVolumes { .. })).cloned().unwrap();
    answer(&mut s, &volumes, Ok(Reply::RemoteVolumes(volumes_11())));
    let first = s
        .take_followups()
        .into_iter()
        .find(|c| matches!(c, Command::FilesListPage { start: 0, .. }))
        .expect("the first page");
    let entries = vec![
        remote("Contratos", RemoteKind::Dir),
        remote("notas.txt", RemoteKind::File),
        remote("foto.jpg", RemoteKind::File),
    ];
    answer(
        &mut s,
        &first,
        Ok(Reply::RemotePage(RemotePage { path: String::new(), start: 0, entries, next: None, free: None })),
    );
    s
}

fn opened_10() -> UiState {
    let mut s = Scenario::Visualizador.prepared_state();
    let a = s.activate(ControlId::TbFiles);
    local_listing(&mut s);
    let volumes = a.commands.iter().find(|c| matches!(c, Command::FilesVolumes { .. })).cloned().unwrap();
    answer(&mut s, &volumes, Err(ApiError::new(ApiCode::Invalid)));
    reply(
        &mut s,
        Reply::Listing(Listing {
            path: String::new(),
            entries: vec![entry("Documents", true), entry("nota.txt", false)],
            truncated: false,
        }),
    );
    s
}

fn grant(s: &mut UiState, granted: Permissions) {
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: crate::mock::MAC.into(),
        state: SessionState::Active { since_ms: 1, role: Role::Viewer, granted, elevation: Default::default() },
    });
}

const FAR: (f32, f32) = (200.0, 200.0);

/// Press on `from`, move far enough to start, and hover `over`.
pub(super) fn drag(s: &mut UiState, from: ControlId, over: Option<ControlId>, copy: bool) {
    s.drag_press(Some(from), (100.0, 100.0));
    s.drag_move(over, FAR, copy, 1_000);
}

pub(super) fn row(there: bool, n: u16) -> ControlId {
    ControlId::ExpRow(there, n)
}

#[test]
fn a_drag_inside_one_pane_moves_and_with_the_copy_key_does_nothing_here() {
    let mut s = opened_11();
    // "foto.jpg" is row 1 of this computer, the folder "Fotos" row 0.
    drag(&mut s, row(false, 1), Some(row(false, 0)), false);
    let v = s.explorer_drag().expect("a drag runs");
    assert_eq!(v.pill, "foto.jpg");
    assert_eq!(v.target, Some(row(false, 0)));
    assert_eq!(v.caption.as_deref(), Some("Mover para Fotos"));
    assert!(!v.copy && !v.caption_is_reason);
    let a = s.drag_release(Some(row(false, 0)), FAR, false).expect("a drag ended");
    assert_eq!(
        a.commands,
        vec![Command::LocalMove {
            paths: vec![PathBuf::from("C:/u/Downloads").join("foto.jpg")],
            into: PathBuf::from("C:/u/Downloads").join("Fotos"),
        }]
    );
    assert!(!s.drag_active());

    // The copy key: there is no copy inside a computer, so nothing happens and the caption says why.
    drag(&mut s, row(false, 1), Some(row(false, 0)), true);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.target, None, "a refused target is not highlighted");
    assert_eq!(v.caption.as_deref(), Some("Para copiar, solte no outro painel."));
    assert!(v.caption_is_reason);
    assert!(s.drag_release(Some(row(false, 0)), FAR, true).unwrap().commands.is_empty());

    // Dropped on the pane's own folder, or on the empty area: nothing, no highlight, no caption.
    drag(&mut s, row(false, 1), Some(ControlId::ExpPane(false)), false);
    let v = s.explorer_drag().unwrap();
    assert_eq!((v.target, v.caption), (None, None));
    assert!(s.drag_release(Some(ControlId::ExpPane(false)), FAR, false).unwrap().commands.is_empty());
}

#[test]
fn a_drag_between_panes_always_copies_into_the_target_folder() {
    let mut s = opened_11();
    // This computer's "foto.jpg" onto the other computer's folder row "Contratos": a copy, whatever key is held.
    for copy in [false, true] {
        drag(&mut s, row(false, 1), Some(row(true, 0)), copy);
        let v = s.explorer_drag().unwrap();
        assert_eq!(v.target, Some(row(true, 0)));
        assert_eq!(v.caption.as_deref(), Some("Copiar para Contratos, no MacBook do escritório"));
        assert!(v.copy);
        let a = s.drag_release(Some(row(true, 0)), FAR, copy).unwrap();
        let sent = a.commands.iter().find_map(|c| match c {
            Command::FilesSend { local_paths, remote_dir, .. } => Some((local_paths.clone(), remote_dir.clone())),
            _ => None,
        });
        let folder = s.there_folder();
        assert_eq!(
            sent,
            Some((vec![PathBuf::from("C:/u/Downloads").join("foto.jpg")], Some(join(&folder, "Contratos")))),
            "{:?}",
            a.commands
        );
    }
    // The other computer's place takes it too.
    let place = s.remote_nav().iter().position(|n| n.path.ends_with("Desktop")).unwrap() as u8;
    drag(&mut s, row(false, 1), Some(ControlId::ExpPlace(true, place)), false);
    let a = s.drag_release(Some(ControlId::ExpPlace(true, place)), FAR, false).unwrap();
    assert!(
        a.commands
            .iter()
            .any(|c| matches!(c, Command::FilesSend { remote_dir: Some(d), .. } if d == ":1/Users/ana/Desktop"))
    );
    // The other pane's own folder goes through the conflict check of the button: "foto.jpg" exists there.
    drag(&mut s, row(false, 1), Some(ControlId::ExpPane(true)), false);
    let a = s.drag_release(Some(ControlId::ExpPane(true)), FAR, false).unwrap();
    assert!(a.commands.is_empty(), "{:?}", a.commands);
    assert!(s.explorer_manage().conflict.is_some(), "the conflict card asks first");

    // The other direction: "notas.txt" onto this computer's folder "Fotos".
    let mut s = opened_11();
    drag(&mut s, row(true, 2), Some(row(false, 0)), false);
    assert_eq!(s.explorer_drag().unwrap().caption.as_deref(), Some("Copiar para Fotos"));
    let a = s.drag_release(Some(row(false, 0)), FAR, false).unwrap();
    let folder = s.there_folder();
    assert!(
        a.commands.iter().any(|c| matches!(c, Command::FilesGet { remote_path, local_dir, .. }
            if remote_path.ends_with("notas.txt") && remote_path.starts_with(&folder)
                && *local_dir == PathBuf::from("C:/u/Downloads").join("Fotos"))),
        "{:?}",
        a.commands
    );
}

#[test]
fn a_folder_cannot_be_dropped_into_itself() {
    let mut s = opened_11();
    drag(&mut s, row(false, 0), Some(row(false, 0)), false);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.target, None);
    assert_eq!(v.caption.as_deref(), Some("Uma pasta não pode ir para dentro dela mesma."));
    assert!(s.drag_release(Some(row(false, 0)), FAR, false).unwrap().commands.is_empty());
    // On the other computer too.
    drag(&mut s, row(true, 0), Some(row(true, 0)), false);
    assert_eq!(s.explorer_drag().unwrap().target, None);
    assert!(s.drag_release(Some(row(true, 0)), FAR, false).unwrap().commands.is_empty());
}

#[test]
fn esc_cancels_a_drag_and_nothing_changes() {
    let mut s = opened_11();
    drag(&mut s, row(false, 1), Some(row(false, 0)), false);
    assert!(s.drag_active());
    let a = s.explorer_key(ExplorerKey::Escape);
    assert!(a.commands.is_empty() && !s.drag_active() && s.explorer_drag().is_none());
    // The button is still down: its release is no drop and no click.
    let a = s.drag_release(Some(row(false, 0)), FAR, false).expect("the release is swallowed");
    assert!(a.commands.is_empty() && a.effect.is_none());
    // The next press and release are an ordinary click again.
    s.drag_press(Some(row(false, 0)), (10.0, 10.0));
    assert!(s.drag_release(Some(row(false, 0)), (10.0, 10.0), false).is_none());
    // A lost capture cancels too.
    drag(&mut s, row(false, 1), Some(row(false, 0)), false);
    s.drag_cancel();
    assert!(!s.drag_active());
}

#[test]
fn a_press_without_moving_is_a_click_not_a_drag() {
    let mut s = opened_11();
    s.drag_press(Some(row(false, 1)), (100.0, 100.0));
    s.drag_move(Some(row(false, 1)), (102.0, 102.0), false, 10);
    assert!(!s.drag_active());
    assert!(s.drag_release(Some(row(false, 1)), (102.0, 102.0), false).is_none(), "the shell clicks");
    // A press somewhere that is not a row never starts one.
    s.drag_press(Some(ControlId::ExpCopy), (0.0, 0.0));
    s.drag_move(None, FAR, false, 20);
    assert!(!s.drag_active());
    // A drag selects the pressed row first when it was not selected, and carries the selection of the pane.
    s.activate(row(false, 1));
    s.set_pointer_mods(PointerMods { toggle: true, ..PointerMods::default() });
    s.activate(row(false, 2));
    drag(&mut s, row(false, 1), None, false);
    assert_eq!(s.explorer_drag().unwrap().pill, "2 itens");
}

#[test]
fn hovering_a_folder_800_ms_opens_it() {
    let mut s = opened_11();
    drag(&mut s, row(false, 1), Some(row(true, 0)), false);
    assert!(s.drag_tick(1_000 + SPRING_MS - 1).commands.is_empty());
    let a = s.drag_tick(1_000 + SPRING_MS);
    assert!(
        a.commands.iter().any(|c| matches!(c, Command::FilesListPage { path, .. } if path.ends_with("Contratos"))),
        "{:?}",
        a.commands
    );
    assert!(s.drag_active(), "the drag goes on in the folder");
    // Moving to another row restarts the wait.
    let mut s = opened_11();
    drag(&mut s, row(false, 1), Some(row(true, 0)), false);
    s.drag_move(Some(row(true, 1)), FAR, false, 1_700);
    assert!(s.drag_tick(1_000 + SPRING_MS).commands.is_empty());
    // A file row is not opened.
    assert!(s.drag_tick(1_700 + SPRING_MS).commands.is_empty());
}

#[test]
fn a_move_there_needs_control_and_1_1() {
    let mut s = opened_11();
    drag(&mut s, row(true, 2), Some(row(true, 0)), false);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.caption.as_deref(), Some("Mover para Contratos"));
    let a = s.drag_release(Some(row(true, 0)), FAR, false).unwrap();
    let folder = s.there_folder();
    assert!(
        a.commands
            .iter()
            .any(|c| matches!(c, Command::FilesChange { change: RemoteChange::Move { from_dir, names, to_dir }, .. }
            if *from_dir == folder && names == &["notas.txt".to_string()] && to_dir.ends_with("Contratos"))),
        "{:?}",
        a.commands
    );

    // FILES alone: the move is refused with the reason, nothing is highlighted or sent.
    grant(&mut s, Permissions::NONE.with(Permission::View).with(Permission::Files));
    drag(&mut s, row(true, 2), Some(row(true, 0)), false);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.target, None);
    assert_eq!(
        v.caption.as_deref(),
        Some("Mudar arquivos no MacBook do escritório precisa também da permissão de controlar.")
    );
    assert!(s.drag_release(Some(row(true, 0)), FAR, false).unwrap().commands.is_empty());

    // A 1.0 peer: the same, with the reason of its version.
    let mut s = opened_10();
    drag(&mut s, row(true, 1), Some(row(true, 0)), false);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.target, None);
    assert!(v.caption.unwrap().contains("Swapdesk 1.0"));
    assert!(s.drag_release(Some(row(true, 0)), FAR, false).unwrap().commands.is_empty());
}

#[test]
fn a_click_on_a_pane_focuses_it_and_clears_its_selection() {
    let mut s = opened_11();
    s.activate(row(false, 1));
    assert!(!s.explorer_pane(false).rows.iter().all(|r| !r.selected));
    s.activate(ControlId::ExpPane(true));
    s.activate(ControlId::ExpPane(false));
    assert!(s.explorer_pane(false).rows.iter().all(|r| !r.selected));
    assert!(s.explorer_pane(false).focused);
    s.activate(ControlId::ExpPane(true));
    assert!(s.explorer_pane(true).focused);
}

#[test]
fn the_autoscroll_helper_scrolls_only_in_the_edges() {
    assert!(drag_autoscroll(100.0, 400.0, 105.0) < 0.0);
    assert!(drag_autoscroll(100.0, 400.0, 495.0) > 0.0);
    assert_eq!(drag_autoscroll(100.0, 400.0, 300.0), 0.0);
    assert_eq!(drag_autoscroll(100.0, 400.0, 50.0), 0.0, "outside the list");
}

#[test]
fn the_context_menu_lists_the_same_items_in_the_same_order_with_disabled_ones_kept() {
    let mut s = opened_11();
    let labels = |s: &UiState| s.context_items().into_iter().map(|(l, _)| l).collect::<Vec<_>>();
    let enabled = |s: &UiState, label: &str| s.context_items().into_iter().find(|(l, _)| l == label).unwrap().1;
    let want_here = [
        "Abrir",
        "Copiar para MacBook do escritório",
        "Recortar",
        "Colar",
        "Renomear",
        "Nova pasta",
        "Excluir",
        "Propriedades",
        "Atualizar",
        // The system's own file manager, named per system.
        if cfg!(target_os = "macos") { "Mostrar no Finder" } else { "Mostrar no Explorador de Arquivos" },
    ];

    // On a row.
    let a = s.context_menu(Some(row(false, 1)), false);
    assert_eq!(a.effect, Some(Effect::ContextMenu));
    assert_eq!(labels(&s), want_here);
    assert!(enabled(&s, "Renomear") && enabled(&s, "Excluir") && enabled(&s, "Recortar"));
    assert!(!enabled(&s, "Colar"), "nothing was cut: disabled, not hidden");
    // On the empty part of the list: the same list, the row items disabled.
    s.context_menu(Some(ControlId::ExpPane(false)), false);
    assert_eq!(labels(&s), want_here);
    assert!(!enabled(&s, "Abrir") && !enabled(&s, "Renomear") && !enabled(&s, "Excluir"));
    assert!(enabled(&s, "Nova pasta") && enabled(&s, "Atualizar") && enabled(&s, "Propriedades"));
    // On a place.
    s.context_menu(Some(ControlId::ExpPlace(false, 0)), false);
    assert_eq!(labels(&s), want_here);
    assert!(enabled(&s, "Abrir") && !enabled(&s, "Nova pasta"));
    // On the other computer: its own wording for the copy, the file manager item stays and is disabled.
    s.context_menu(Some(row(true, 2)), false);
    let there = labels(&s);
    assert_eq!(there.len(), want_here.len());
    assert_eq!(there[1], "Copiar para este computador");
    assert_eq!(there[2..], want_here[2..]);
    assert!(!enabled(&s, want_here[9]) && !enabled(&s, "Propriedades"));
    assert!(enabled(&s, "Renomear"), "a 1.1 peer with control takes a rename");
    assert!(!enabled(&s, "Abrir"), "a file there is not opened");

    // The keyboard (the Menu key) with nothing under it opens on the focused pane.
    s.context_menu(None, true);
    assert_eq!(s.context_items().len(), want_here.len());
    // A right click on a control that is not the explorer's opens nothing.
    assert!(s.context_menu(Some(ControlId::ExpCopy), false).effect.is_none());
}

#[test]
fn choosing_a_context_item_runs_the_same_action_as_the_commands() {
    let mut s = opened_11();
    // Renomear on a row of this computer: the field opens.
    s.context_menu(Some(row(false, 1)), false);
    let a = s.context_chosen(4);
    assert_eq!(a.effect, Some(Effect::FocusEdit(crate::state::EditId::ExpRename)));
    // Recortar then Colar into a folder row.
    s.explorer_key(ExplorerKey::Escape);
    s.context_menu(Some(row(false, 2)), false);
    s.context_chosen(2);
    s.context_menu(Some(row(false, 0)), false);
    let a = s.context_chosen(3);
    assert_eq!(
        a.commands,
        vec![Command::LocalMove {
            paths: vec![PathBuf::from("C:/u/Downloads").join("novo.txt")],
            into: PathBuf::from("C:/u/Downloads").join("Fotos"),
        }]
    );
    // Copiar para ... runs the primary button's copy.
    s.context_menu(Some(row(false, 1)), false);
    let a = s.context_chosen(1);
    assert!(
        a.commands.iter().any(|c| matches!(c, Command::FilesSend { .. })) || s.explorer_manage().conflict.is_some()
    );
    // A disabled item does nothing; Mostrar no ... asks the shell to reveal the item.
    s.context_menu(Some(ControlId::ExpPane(false)), false);
    assert!(s.context_chosen(0).commands.is_empty());
    s.context_menu(Some(row(false, 1)), false);
    let a = s.context_chosen(9);
    assert_eq!(a.effect, Some(Effect::RevealLocal));
    assert_eq!(s.take_local_target(), Some(PathBuf::from("C:/u/Downloads").join("foto.jpg")));
}

#[test]
fn the_drag_and_menu_scenarios_hold_their_states() {
    let s = Scenario::ExploradorArrastando.prepared_state();
    let v = s.explorer_drag().expect("a drag in the air");
    assert_eq!(v.pill, "orçamento-2026.xlsx");
    assert_eq!(v.caption.as_deref(), Some("Mover para Fotos da viagem"));
    assert_eq!(v.target, Some(ControlId::ExpRow(false, 0)));
    let s = Scenario::ExploradorMenu.prepared_state();
    assert_eq!(s.context_items().len(), 10);
}

// ----- the breadcrumb's segments take a drop for their folder -----

/// The other computer's pane in its "Desktop" folder (a 1.1 path under the home folder): "Sub" and "a.txt".
fn there_in_documents(s: &mut UiState) {
    let desk = s.remote_nav().iter().position(|n| n.path.ends_with("Desktop")).unwrap() as u8;
    let go = s.activate(ControlId::ExpPlace(true, desk));
    let cmd = go.commands.into_iter().next().unwrap();
    assert!(matches!(&cmd, Command::FilesListPage { path, .. } if path == ":1/Users/ana/Desktop"), "{cmd:?}");
    let entries = vec![remote("Sub", RemoteKind::Dir), remote("a.txt", RemoteKind::File)];
    let page = RemotePage { path: ":1/Users/ana/Desktop".into(), start: 0, entries, next: None, free: None };
    answer(s, &cmd, Ok(Reply::RemotePage(page)));
    assert_eq!(s.explorer_pane(true).crumbs, ["Pasta pessoal", "Desktop"]);
}

#[test]
fn a_breadcrumb_segment_takes_a_move_inside_its_computer_and_the_current_folder_takes_nothing() {
    let mut s = opened_11();
    // This computer: "foto.jpg" onto the segment of the folder above the one shown.
    let up = s.explorer_pane(false).crumbs.len() as u8 - 2;
    let seg = ControlId::ExpCrumb(false, up);
    drag(&mut s, row(false, 1), Some(seg), false);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.target, Some(seg), "the segment is highlighted");
    assert_eq!(
        v.caption.as_deref(),
        Some(format!("Mover para {}", s.explorer_pane(false).crumbs[up as usize]).as_str())
    );
    let a = s.drag_release(Some(seg), FAR, false).unwrap();
    assert_eq!(
        a.commands,
        vec![Command::LocalMove {
            paths: vec![PathBuf::from("C:/u/Downloads").join("foto.jpg")],
            into: PathBuf::from("C:/u/Downloads").parent().unwrap().to_path_buf(),
        }]
    );
    // The last segment is the folder the pane shows: nothing to do, no highlight, no caption.
    let cur = Some(ControlId::ExpCrumb(false, up + 1));
    drag(&mut s, row(false, 1), cur, false);
    let v = s.explorer_drag().unwrap();
    assert_eq!((v.target, v.caption), (None, None));
    assert!(s.drag_release(cur, FAR, false).unwrap().commands.is_empty());
    // The copy key inside one computer says why it does nothing, as on a folder row.
    drag(&mut s, row(false, 1), Some(seg), true);
    let v = s.explorer_drag().unwrap();
    assert!(v.caption_is_reason && v.target.is_none());
    assert_eq!(v.caption.as_deref(), Some("Para copiar, solte no outro painel."));
    assert!(s.drag_release(Some(seg), FAR, true).unwrap().commands.is_empty());
    // A segment that does not exist is no target.
    drag(&mut s, row(false, 1), Some(ControlId::ExpCrumb(false, 99)), false);
    assert_eq!(s.explorer_drag().unwrap().target, None);
}

#[test]
fn a_breadcrumb_segment_of_the_other_computer_moves_there_and_copies_across() {
    let mut s = opened_11();
    there_in_documents(&mut s);
    let home = Some(ControlId::ExpCrumb(true, 0));
    // Inside the other computer: a move into the home folder.
    drag(&mut s, row(true, 1), home, false);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.caption.as_deref(), Some("Mover para Pasta pessoal"));
    assert_eq!(v.target, home);
    let a = s.drag_release(home, FAR, false).unwrap();
    assert!(
        a.commands
            .iter()
            .any(|c| matches!(c, Command::FilesChange { change: RemoteChange::Move { from_dir, names, to_dir }, .. }
            if from_dir == ":1/Users/ana/Desktop" && names == &["a.txt".to_string()] && to_dir == ":1/Users/ana")),
        "{:?}",
        a.commands
    );
    // Across: this computer's file is copied into the segment's folder, whatever key is held.
    drag(&mut s, row(false, 1), home, false);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.caption.as_deref(), Some("Copiar para Pasta pessoal, no MacBook do escritório"));
    assert!(v.copy);
    let a = s.drag_release(home, FAR, false).unwrap();
    assert!(
        a.commands.iter().any(|c| matches!(c, Command::FilesSend { remote_dir: Some(d), .. } if d == ":1/Users/ana")),
        "{:?}",
        a.commands
    );
    // A folder cannot go into itself: its own segment is never above it, but the folder shown is "Desktop",
    // and "Sub" dragged on the segment of "Desktop" (the current folder) does nothing.
    let cur = Some(ControlId::ExpCrumb(true, 1));
    drag(&mut s, row(true, 0), cur, false);
    assert_eq!(s.explorer_drag().unwrap().target, None);
    // Clicking a segment opens its folder and the click on the last one stays.
    let a = s.activate(ControlId::ExpCrumb(true, 0));
    assert!(
        matches!(a.commands.first(), Some(Command::FilesListPage { path, .. }) if path == ":1/Users/ana"),
        "{:?}",
        a.commands
    );
}

// ----- the limits of a folder upload say so (FILES-UI.md 5: 10 000 files, 32 levels) -----

/// The folder "Fotos" of this computer on its way up: its folder is made there and its listing is asked for.
fn start_folder_upload(s: &mut UiState) -> Command {
    s.explorer.focus_there = false;
    let n = s.explorer_pane(false).rows.iter().position(|r| r.name == "Fotos").unwrap();
    s.activate(row(false, n as u16));
    let mk = s.activate(ControlId::ExpCopy).commands.into_iter().next().expect("the folder is made first");
    answer(s, &mk, Ok(Reply::ChangeResults(vec![("Fotos".into(), crate::api::ChangeOutcome::Done)])));
    s.take_followups().into_iter().next().expect("the folder is listed")
}

const TOO_BIG: &str = "Esta cópia passa do limite de 10\u{a0}000 arquivos ou 32 níveis de pastas. Nada foi copiado.";

#[test]
fn a_folder_upload_over_10000_files_stops_with_the_limit_text() {
    let mut s = opened_11();
    let list = start_folder_upload(&mut s);
    let Command::LocalList { path: Some(local) } = &list else { panic!("{list:?}") };
    let entries = (0..10_001).map(|i| entry(&format!("f{i}.jpg"), false)).collect();
    answer(
        &mut s,
        &list,
        Ok(Reply::LocalListing(LocalListing { path: local.clone(), entries, truncated: false, hidden: Vec::new() })),
    );
    assert_eq!(s.explorer_pane(true).notice.as_deref(), Some(TOO_BIG));
    assert!(s.take_followups().is_empty(), "nothing is sent");
    // The English text carries the same numbers.
    s.set_language(crate::api::Language::En);
    assert_eq!(
        s.explorer_pane(true).notice.as_deref(),
        Some("This copy is over the limit of 10\u{a0}000 files or 32 folder levels. Nothing was copied.")
    );
}

#[test]
fn a_folder_upload_deeper_than_32_levels_stops_with_the_limit_text() {
    let mut s = opened_11();
    let mut list = start_folder_upload(&mut s);
    for level in 0..40 {
        let Command::LocalList { path: Some(local) } = &list else { panic!("{list:?}") };
        let listing =
            LocalListing { path: local.clone(), entries: vec![entry("d", true)], truncated: false, hidden: Vec::new() };
        answer(&mut s, &list, Ok(Reply::LocalListing(listing)));
        let next = s.take_followups();
        let Some(mk) = next.into_iter().find(|c| matches!(c, Command::FilesChange { .. })) else {
            // "Fotos" is level 1: the 32nd listing (index 31) is the last level, and a folder below it is refused.
            assert_eq!(level, 31, "the walk stops below level 32 and not before");
            assert_eq!(s.explorer_pane(true).notice.as_deref(), Some(TOO_BIG));
            return;
        };
        let Command::FilesChange { change: RemoteChange::MakeDir { name, .. }, .. } = &mk else { panic!("{mk:?}") };
        let name = name.clone();
        answer(&mut s, &mk, Ok(Reply::ChangeResults(vec![(name, crate::api::ChangeOutcome::Done)])));
        list = s.take_followups().into_iter().next().expect("the next folder is listed");
    }
    panic!("the walk never stopped");
}
