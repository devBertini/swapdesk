//! The drawer's file panel drawn by the Windows renderer (stage 44): what it registers, and its captures.

use swapdesk_ui::mock::Scenario;
use swapdesk_ui::{ControlId, DirEntryView, DrawerTab, Listing, Reply, Ticket, UiEvent, UiState, WindowKind};

use swapdesk_paint::{Interaction, design_size};
use swapdesk_ui::{ApiCode, TransferState};

use crate::offscreen::render;

fn panel_state() -> UiState {
    let mut s = Scenario::Visualizador.prepared_state();
    s.activate(ControlId::DrawerTab(DrawerTab::Files));
    let entries = (0..12)
        .map(|i| DirEntryView { name: format!("arquivo-{i:02}.pdf"), is_dir: i % 4 == 0, size: 1500 * i, mtime_ms: 1 })
        .collect();
    s.apply(UiEvent::Reply {
        ticket: Ticket(0),
        result: Ok(Reply::Listing(Listing { path: "Documentos/Contratos".into(), entries, truncated: true })),
    });
    let t = |id, name: &str, outgoing, state| UiEvent::Transfer {
        transfer_id: id,
        session_id: 1,
        name: name.into(),
        outgoing,
        state,
    };
    s.apply(t(1, "relatorio.pdf", true, TransferState::Running { done: 40, total: 100, bytes_per_s: 10 }));
    s.apply(t(2, "praia.jpg", false, TransferState::Paused { done: 30, total: 100, why: ApiCode::Timeout }));
    s.apply(t(3, "foto.jpg", false, TransferState::Done));
    s.apply(UiEvent::TransferSavedAs { transfer_id: 3, session_id: 1, name: "foto (1).jpg".into() });
    s.apply(t(4, "negado.bin", true, TransferState::Failed { reason: ApiCode::NotPermitted }));
    s.apply(t(5, "fila.bin", true, TransferState::Queued));
    let _ = s.set_receive_dir(std::path::PathBuf::from("C:/Users/x/Downloads"));
    s
}

#[test]
fn the_panel_draws_every_control_the_state_lists_and_nothing_else_in_light_and_dark() {
    for dark in [false, true] {
        let mut s = panel_state();
        if dark {
            s.apply(UiEvent::ThemeChanged { dark: true });
        }
        let r = render(&s, WindowKind::Viewer, design_size(WindowKind::Viewer), 1.0, &Interaction::default())
            .expect("renders");
        assert!(r.distinct_colors() > 3);
        let known: Vec<ControlId> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
        for id in [
            ControlId::FilesChange,
            ControlId::FilesUp,
            ControlId::FilesPage(true),
            ControlId::FilesRow(0),
            ControlId::FilesRow(4),
            ControlId::FilesChoose,
            ControlId::FilesRecvFolder,
        ]
        .into_iter()
        .chain([
            ControlId::TransferResume(2),
            ControlId::TransferCancel(1),
            ControlId::TransferCancel(2),
            ControlId::TransferCancel(4),
            ControlId::TransferCancel(5),
        ]) {
            assert!(known.contains(&id), "dark {dark}: the state does not list {id:?}");
            assert!(r.output.widgets.iter().any(|w| w.id == id), "dark {dark}: the screen did not draw {id:?}");
        }
        // A panel taller than the drawer scrolls; scrolled to the end, what left the view cannot be hit (an empty box)
        // and the last transfer's controls are in view.
        assert!(r.output.scroll_max > 0.0, "dark {dark}: the long panel scrolls");
        let end = Interaction { scroll: r.output.scroll_max, ..Interaction::default() };
        let r2 = render(&s, WindowKind::Viewer, design_size(WindowKind::Viewer), 1.0, &end).expect("renders scrolled");
        let widget = |r: &crate::offscreen::Rendered, id| r.output.widgets.iter().find(|w| w.id == id).map(|w| w.rect);
        assert_eq!(
            widget(&r2, ControlId::FilesChange).map(|x| x.w),
            Some(0.0),
            "the top button scrolled out of the drawer"
        );
        assert!(
            widget(&r2, ControlId::TransferCancel(5)).is_some_and(|x| x.w > 0.0 && x.h > 0.0),
            "the last transfer is in view"
        );
        assert!(
            !known.contains(&ControlId::TransferCancel(3)) && !known.contains(&ControlId::FilesRow(5)),
            "a finished transfer has no cancel and only a page of entries is listed"
        );
        let extra: Vec<_> = r.output.widgets.iter().filter(|w| !known.contains(&w.id)).map(|w| w.id).collect();
        assert!(extra.is_empty(), "dark {dark}: drawn but not listed: {extra:?}");
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-results/stage44-files");
        std::fs::create_dir_all(&dir).expect("dir");
        r.save_png(&dir.join(if dark { "painel-escuro.png" } else { "painel.png" })).expect("png");
    }
}

#[test]
fn the_receive_folder_defaults_to_the_users_downloads_folder_when_there_is_one() {
    // Whatever this PC has, the answer is a folder that exists or nothing.
    if let Some(d) = crate::files_dialogs::default_receive_dir() {
        assert!(d.is_dir() && d.ends_with("Downloads"));
    }
}
