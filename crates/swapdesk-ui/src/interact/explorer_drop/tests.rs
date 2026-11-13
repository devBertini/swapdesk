//! Slice 4: files dragged from the system's file manager into the app (FILES-UI.md 4.2 and 10).

use std::path::PathBuf;

use super::*;
use crate::api::{
    ApiCode, ApiError, DirEntryView, Listing, LocalListing, Permission, Permissions, PlaceName, RemoteEntry,
    RemoteKind, RemotePage, RemotePlace, RemoteVolume, RemoteVolumes, Reply, Role, SessionState, Ticket, UiEvent,
    VolumeType,
};
use crate::interact::explorer::join;
use crate::mock::Scenario;

fn entry(name: &str, is_dir: bool) -> DirEntryView {
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
fn opened_11() -> UiState {
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

const HERE: (f32, f32) = (300.0, 240.0);

fn row(there: bool, n: u16) -> ControlId {
    ControlId::ExpRow(there, n)
}

fn files(paths: &[&str]) -> SystemDragData {
    SystemDragData::Files(paths.iter().map(PathBuf::from).collect())
}

/// The explorer closed: the Tela view with the picture.
fn on_screen() -> UiState {
    let mut s = opened_11();
    s.activate(ControlId::ExpTela);
    assert!(!s.explorer_open());
    s
}

/// The drop asked the worker which paths are folders (and touched no disk itself): answers it as the worker would
/// and returns the commands the copy then sends.
fn resolve(s: &mut UiState, r: &DropReply, folders: &[&str]) -> Vec<Command> {
    let ask = r.activation.commands.iter().find(|c| matches!(c, Command::LocalKinds { .. })).cloned();
    let Some(ref ask @ Command::LocalKinds { ref paths }) = ask else { panic!("no LocalKinds in {:?}", r.activation) };
    let kinds = paths.iter().map(|p| (p.clone(), folders.iter().any(|f| PathBuf::from(f) == *p))).collect();
    answer(s, ask, Ok(Reply::LocalKinds(kinds)));
    s.take_followups()
}

fn sent(commands: &[Command]) -> Vec<(Vec<PathBuf>, Option<String>)> {
    commands
        .iter()
        .filter_map(|c| match c {
            Command::FilesSend { local_paths, remote_dir, .. } => Some((local_paths.clone(), remote_dir.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_system_drop_on_a_remote_folder_uploads_into_it() {
    let mut s = opened_11();
    // "Contratos" is row 0 of the other computer's pane.
    let r = s.system_drag_enter(files(&["C:/docs/relatorio.pdf"]), Some(row(true, 0)), HERE, 1_000);
    assert_eq!(r.effect, DropEffect::Copy);
    let v = s.explorer_drag().expect("the drop is drawn like a drag");
    assert_eq!(v.pill, "relatorio.pdf");
    assert_eq!(v.target, Some(row(true, 0)));
    assert_eq!(v.caption.as_deref(), Some("Copiar para Contratos, no MacBook do escritório"));
    assert!(v.copy && !v.caption_is_reason);
    let r = s.system_drop(Some(row(true, 0)), HERE);
    assert_eq!(r.effect, DropEffect::Copy);
    assert!(sent(&r.activation.commands).is_empty(), "nothing is sent before the worker answered");
    let commands = resolve(&mut s, &r, &[]);
    let folder = s.there_folder();
    assert_eq!(sent(&commands), vec![(vec![PathBuf::from("C:/docs/relatorio.pdf")], Some(join(&folder, "Contratos")))]);
    assert!(s.explorer_drag().is_none(), "nothing stays drawn after the drop");

    // Two files: the pill counts them; a remote place or volume is a target too.
    let desktop = s.remote_nav().iter().position(|n| n.path.ends_with("Desktop")).unwrap() as u8;
    let place = Some(ControlId::ExpPlace(true, desktop));
    s.system_drag_enter(files(&["C:/docs/a.txt", "C:/docs/b.txt"]), place, HERE, 2_000);
    assert_eq!(s.explorer_drag().unwrap().pill, "2 itens");
    let r = s.system_drop(place, HERE);
    let commands = resolve(&mut s, &r, &[]);
    assert_eq!(
        sent(&commands),
        vec![(
            vec![PathBuf::from("C:/docs/a.txt"), PathBuf::from("C:/docs/b.txt")],
            Some(":1/Users/ana/Desktop".into())
        )]
    );

    // A folder goes up through the walk of slice 2b (a 1.1 peer): MakeDir first.
    s.system_drag_enter(files(&["C:/docs/Fotos"]), Some(ControlId::ExpPane(true)), HERE, 3_000);
    let r = s.system_drop(Some(ControlId::ExpPane(true)), HERE);
    assert_eq!(r.effect, DropEffect::Copy);
    let commands = resolve(&mut s, &r, &["C:/docs/Fotos"]);
    assert!(commands.iter().any(|c| matches!(c, Command::FilesChange { .. })), "{commands:?}");

    // This computer's pane takes no copy: it says why and the OS is told "none".
    let r = s.system_drag_enter(files(&["C:/docs/a.txt"]), Some(row(false, 1)), HERE, 4_000);
    assert_eq!(r.effect, DropEffect::None);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.caption.as_deref(), Some("Para copiar, solte no outro painel."));
    assert!(v.caption_is_reason && v.target.is_none());
    assert!(s.system_drop(Some(row(false, 1)), HERE).activation.commands.is_empty());
}

#[test]
fn a_drop_on_the_picture_sends_to_the_default_folder() {
    let mut s = on_screen();
    let r = s.system_drag_enter(files(&["C:/docs/foto.jpg", "C:/docs/Fotos"]), None, HERE, 1_000);
    assert_eq!(r.effect, DropEffect::Copy);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.caption.as_deref(), Some("Solte para enviar ao MacBook do escritório"));
    assert_eq!(v.target, None, "the picture has no control to outline");
    assert_eq!(v.pill, "2 itens");
    let r = s.system_drop(None, HERE);
    assert_eq!(r.effect, DropEffect::Copy);
    let commands = resolve(&mut s, &r, &["C:/docs/Fotos"]);
    assert_eq!(sent(&commands), vec![(vec![PathBuf::from("C:/docs/foto.jpg")], None)]);
    // Only folders: nothing goes and the explorer's note says why (the default folder cannot take a walk).
    s.system_drag_enter(files(&["C:/docs/Fotos"]), None, HERE, 2_000);
    let r = s.system_drop(None, HERE);
    let commands = resolve(&mut s, &r, &["C:/docs/Fotos"]);
    assert!(sent(&commands).is_empty());
    // Over a control of the bar it is not the picture.
    let r = s.system_drag_enter(files(&["C:/docs/foto.jpg"]), Some(ControlId::TbFiles), HERE, 3_000);
    assert_eq!(r.effect, DropEffect::None);
    assert!(s.system_drop(Some(ControlId::TbFiles), HERE).activation.commands.is_empty());
}

#[test]
fn a_drop_without_disk_files_is_refused_with_the_reason() {
    let reason = "Só dá para enviar arquivos que estão no disco.";
    for (mut s, over) in [(opened_11(), Some(row(true, 0))), (on_screen(), None)] {
        let r = s.system_drag_enter(SystemDragData::NotFiles, over, HERE, 1_000);
        assert_eq!(r.effect, DropEffect::None);
        let v = s.explorer_drag().unwrap();
        assert_eq!((v.caption.as_deref(), v.caption_is_reason, v.target, v.copy), (Some(reason), true, None, false));
        let r = s.system_drop(over, HERE);
        assert_eq!(r.effect, DropEffect::None);
        assert!(r.activation.commands.is_empty());
    }
    // An empty list is the same.
    let mut s = opened_11();
    assert_eq!(s.system_drag_enter(files(&[]), Some(row(true, 0)), HERE, 1_000).effect, DropEffect::None);
}

#[test]
fn a_folder_dropped_toward_a_1_0_peer_is_refused() {
    let mut s = opened_10();
    // The hover cannot tell a folder from a file (no disk call on the UI thread): it accepts; the reason comes
    // when the worker says the path is a folder.
    let r = s.system_drag_enter(files(&["C:/docs/Fotos"]), Some(ControlId::ExpPane(true)), HERE, 1_000);
    assert_eq!(r.effect, DropEffect::Copy);
    let r = s.system_drop(Some(ControlId::ExpPane(true)), HERE);
    let commands = resolve(&mut s, &r, &["C:/docs/Fotos"]);
    assert!(
        !commands.iter().any(|c| matches!(c, Command::FilesSend { .. } | Command::FilesChange { .. })),
        "{commands:?}"
    );
    assert!(s.explorer.note.is_some(), "the reason is the explorer's note");
    // Files still go to a 1.0 peer's folder.
    s.system_drag_enter(files(&["C:/docs/a.txt"]), Some(ControlId::ExpPane(true)), HERE, 2_000);
    let r = s.system_drop(Some(ControlId::ExpPane(true)), HERE);
    assert_eq!(r.effect, DropEffect::Copy);
    assert_eq!(sent(&resolve(&mut s, &r, &[])).len(), 1);
    // Without FILES granted nothing goes.
    grant(&mut s, Permissions::NONE.with(Permission::View));
    let r = s.system_drag_enter(files(&["C:/docs/a.txt"]), Some(ControlId::ExpPane(true)), HERE, 3_000);
    assert_eq!(r.effect, DropEffect::None);
}

#[test]
fn leaving_the_window_clears_the_highlight() {
    let mut s = opened_11();
    s.system_drag_enter(files(&["C:/docs/a.txt"]), Some(row(true, 0)), HERE, 1_000);
    assert_eq!(s.explorer_drag().and_then(|v| v.target), Some(row(true, 0)));
    assert_eq!(s.system_drag_leave(), Changed::VIEWER);
    assert!(s.explorer_drag().is_none());
    assert_eq!(s.system_drag_leave(), Changed::NONE);
    // A late drop after leaving does nothing.
    let r = s.system_drop(Some(row(true, 0)), HERE);
    assert_eq!((r.effect, r.activation.commands.is_empty()), (DropEffect::None, true));
    // Moving off every target drops the highlight but keeps the pill.
    s.system_drag_enter(files(&["C:/docs/a.txt"]), Some(row(true, 0)), HERE, 2_000);
    s.system_drag_over(None, HERE, 2_100);
    let v = s.explorer_drag().unwrap();
    assert_eq!((v.target, v.caption), (None, None));
}

#[test]
fn a_folder_row_hovered_by_a_system_drop_opens() {
    let mut s = opened_11();
    s.system_drag_enter(files(&["C:/docs/a.txt"]), Some(row(true, 0)), HERE, 1_000);
    let r = s.system_drag_over(Some(row(true, 0)), HERE, 1_000 + SPRING_MS + 1);
    assert!(r.activation.commands.iter().any(|c| matches!(c, Command::FilesListPage { .. })), "{:?}", r.activation);
}

#[test]
fn a_system_drop_on_a_breadcrumb_segment_of_the_other_computer_uploads_into_its_folder() {
    let mut s = opened_11();
    let desk = s.remote_nav().iter().position(|n| n.path.ends_with("Desktop")).unwrap() as u8;
    let go = s.activate(ControlId::ExpPlace(true, desk));
    let cmd = go.commands.into_iter().next().unwrap();
    let page = RemotePage {
        path: ":1/Users/ana/Desktop".into(),
        start: 0,
        entries: vec![remote("Sub", RemoteKind::Dir)],
        next: None,
        free: None,
    };
    answer(&mut s, &cmd, Ok(Reply::RemotePage(page)));
    let home = Some(ControlId::ExpCrumb(true, 0));
    let r = s.system_drag_enter(files(&["C:/docs/relatorio.pdf"]), home, HERE, 1_000);
    assert_eq!(r.effect, DropEffect::Copy);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.target, home, "the segment is outlined");
    assert_eq!(v.caption.as_deref(), Some("Copiar para Pasta pessoal, no MacBook do escritório"));
    let r = s.system_drop(home, HERE);
    let commands = resolve(&mut s, &r, &[]);
    assert_eq!(sent(&commands), vec![(vec![PathBuf::from("C:/docs/relatorio.pdf")], Some(":1/Users/ana".into()))]);
    // The segment of the folder shown is the pane's own folder: the button's path, with its conflict check.
    let cur = Some(ControlId::ExpCrumb(true, 1));
    let r = s.system_drag_enter(files(&["C:/docs/a.txt"]), cur, HERE, 2_000);
    assert_eq!(r.effect, DropEffect::Copy);
    // This computer's segments take no copy: they say why and the OS is told "none".
    let mine = Some(ControlId::ExpCrumb(false, s.explorer_pane(false).crumbs.len() as u8 - 2));
    let r = s.system_drag_enter(files(&["C:/docs/a.txt"]), mine, HERE, 3_000);
    assert_eq!(r.effect, DropEffect::None);
    let v = s.explorer_drag().unwrap();
    assert_eq!(v.caption.as_deref(), Some("Para copiar, solte no outro painel."));
    assert!(v.caption_is_reason && v.target.is_none());
}

#[test]
fn folders_only_on_the_picture_say_why_over_the_picture_and_the_line_goes_by_itself() {
    let mut s = on_screen();
    assert_eq!(s.screen_note(), None);
    s.system_drag_enter(files(&["C:/docs/Fotos"]), None, HERE, 1_000);
    let r = s.system_drop(None, HERE);
    let commands = resolve(&mut s, &r, &["C:/docs/Fotos"]);
    assert!(sent(&commands).is_empty());
    let note = s.screen_note().expect("the reason shows where the drop happened: the picture");
    assert_eq!(note, "Pastas ainda não são enviadas: escolha os arquivos de dentro delas.");
    // The accessibility tree reads it as a status of the picture's window.
    assert!(s.controls(crate::state::WindowKind::Viewer).iter().any(|c| c.label == note));
    // It is a short-lived line: it counts down with the clock and goes.
    for _ in 0..5 {
        s.tick();
    }
    assert!(s.screen_note().is_some(), "still there before its time is up");
    assert!(s.tick().hits(crate::state::WindowKind::Viewer), "its going repaints the window");
    assert_eq!(s.screen_note(), None);
    assert!(!s.controls(crate::state::WindowKind::Viewer).iter().any(|c| c.label == note));
    // Files among the folders send the files and leave no line.
    s.system_drag_enter(files(&["C:/docs/foto.jpg", "C:/docs/Fotos"]), None, HERE, 2_000);
    let r = s.system_drop(None, HERE);
    resolve(&mut s, &r, &["C:/docs/Fotos"]);
    assert_eq!(s.screen_note(), None);
    // The line is for the picture: the explorer shows nothing of it.
    s.system_drag_enter(files(&["C:/docs/Fotos"]), None, HERE, 3_000);
    let r = s.system_drop(None, HERE);
    resolve(&mut s, &r, &["C:/docs/Fotos"]);
    assert!(s.screen_note().is_some());
    s.activate(ControlId::TbFiles);
    assert_eq!(s.screen_note(), None, "hidden while the explorer is open");
}
