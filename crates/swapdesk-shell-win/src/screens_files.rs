//! The file panel of the viewer's drawer (stage 44): the folder of the other computer with its entries (a page of
//! them at a time), "Escolher arquivos", the folder files are saved into, and the transfers with their progress,
//! "Retomar" and the "saved as" line of a file that did not overwrite another. What each control does is
//! `swapdesk_ui::UiState::activate`; this file only lays them out.

use swapdesk_ui::tokens::{Color, text};
use swapdesk_ui::{ApiCode, ControlId, DataKind, Msg, MsgKey, TransferState};

use crate::gfx::{Align, Rect};
use crate::screens_viewer::human_size;
use crate::view::{Frame, Kind};

const ROW_H: f32 = 46.0;
const TRANSFER_H: f32 = 58.0;

/// The height of the panel's content, measured the way [`drawer_files`] lays it out, so the wheel can scroll exactly
/// to its end.
fn content_height(f: &Frame, w: f32, peer: &str) -> f32 {
    let p = f.p;
    let mut h = 18.0 + 40.0 + 14.0;
    if f.st.placeholder(DataKind::Listing) {
        h += 2.0 * ROW_H;
    } else if let Some(l) = f.st.listing() {
        let range = f.st.files_page_rows();
        h += if l.entries.is_empty() { ROW_H } else { (range.end - range.start) as f32 * ROW_H };
        if l.entries.len() > UI_PAGE {
            h += 44.0;
        }
        if l.truncated {
            h += p.label_in(&f.t(MsgKey::FilesTruncated), text::CAPTION, w, Align::Start).h + 6.0;
        }
    }
    h += 12.0 + 40.0 + 12.0;
    let drop_text = f.st.tm(&Msg::new(MsgKey::FilesDrop).with(peer));
    h += 22.0 + p.label_in(&drop_text, text::BODY, w - 44.0, Align::Center).h + 10.0 + 40.0 + 22.0 + 14.0 + 6.0;
    h += p.label(&f.t(MsgKey::Transfers), text::H2).h + 14.0;
    h + f.st.viewer_transfers().len() as f32 * TRANSFER_H + 18.0
}

pub(crate) fn drawer_files(f: &mut Frame, b: Rect, peer: &str) {
    let (p, pal) = (f.p, f.pal);
    let (x, w) = (b.x + 20.0, b.w - 40.0);
    // The panel is taller than the drawer when the folder is long: the wheel scrolls it (the window's generic list
    // scroll), and what is out of view is neither drawn nor hit.
    let max = (content_height(f, w, peer) - b.h).max(0.0);
    f.out.scroll_max = max;
    f.out.list_area = Some(b);
    let scroll = f.ix.scroll.clamp(0.0, max);
    let mut y = b.y + 18.0 - scroll;
    let first_widget = f.out.widgets.len();
    p.push_clip(b);

    // The folder: its caption and name, "Trocar pasta" (back to the root), "Pasta acima" when there is one.
    let change = f.t(MsgKey::ChangeFolder);
    let cw = f.button_width(&change, Kind::Default);
    let cap = p.label(&f.st.tm(&Msg::new(MsgKey::FilesFolderOn).with(peer)), text::SMALL);
    p.draw(&cap, x, y, pal.soft);
    let loading = f.st.placeholder(DataKind::Listing);
    let folder = f.st.remote_folder().unwrap_or_default().to_string();
    let up = f.st.can_go_up().then(|| f.t(MsgKey::FilesUp));
    let uw = up.as_ref().map_or(0.0, |u| f.button_width(u, Kind::Default) + 8.0);
    if loading {
        f.block(Rect::new(x, y + cap.h + 3.0, 120.0, 14.0), 4.0);
    } else {
        let shown = if folder.is_empty() { "/".to_string() } else { format!("/{folder}") };
        let nm = p.label_ellipsis(&shown, text::NAME, (w - cw - uw - 10.0).max(40.0));
        p.draw(&nm, x, y + cap.h, pal.ink);
    }
    f.button(ControlId::FilesChange, Rect::new(x + w - cw, y, cw, 40.0), Kind::Default, &change, false);
    if let Some(u) = &up {
        f.button(ControlId::FilesUp, Rect::new(x + w - cw - uw, y, uw - 8.0, 40.0), Kind::Default, u, false);
    }
    y += 40.0 + 14.0;

    // Entries: rows of 44, one page at a time.
    let listing = f.st.listing().cloned();
    let range = f.st.files_page_rows();
    let rows = (range.end - range.start).min(UI_PAGE);
    if loading {
        for n in 0..2 {
            f.block(Rect::new(x + 12.0, y + n as f32 * ROW_H + 14.0, 200.0, 14.0), 4.0);
            f.block(Rect::new(x + w - 12.0 - 60.0, y + n as f32 * ROW_H + 10.0, 60.0, 24.0), 6.0);
        }
        y += 2.0 * ROW_H;
    } else if let Some(l) = &listing {
        if l.entries.is_empty() {
            let e = p.label(&f.t(MsgKey::FilesEmpty), text::BODY);
            p.draw(&e, x + 12.0, y + (44.0 - e.h) / 2.0, pal.soft);
            y += ROW_H;
        }
        for (n, i) in range.clone().enumerate() {
            let e = &l.entries[i];
            let ry = y + n as f32 * ROW_H;
            let label = f.t(if e.is_dir { MsgKey::FilesOpen } else { MsgKey::FilesDownload });
            let bw = f.button_width(&label, Kind::Default);
            let size = if e.is_dir {
                f.st.tm(&Msg::new(MsgKey::FilesItems).with(e.size.to_string()))
            } else {
                human_size(e.size)
            };
            let sl = p.label(&size, text::CAPTION);
            let nm = p.label_ellipsis(&e.name, text::BODY, (w - 24.0 - bw - 10.0 - sl.w - 10.0).max(40.0));
            p.draw(&nm, x + 12.0, ry + (44.0 - nm.h) / 2.0, pal.ink);
            p.draw(&sl, x + w - 12.0 - bw - 10.0 - sl.w, ry + (44.0 - sl.h) / 2.0, pal.soft);
            if let Ok(idx) = u16::try_from(i) {
                f.button(
                    ControlId::FilesRow(idx),
                    Rect::new(x + w - 12.0 - bw, ry + 2.0, bw, 40.0),
                    Kind::Default,
                    &label,
                    false,
                );
            }
        }
        y += rows as f32 * ROW_H;
        // Paging, and the note of a listing the host cut short.
        if l.entries.len() > UI_PAGE {
            let (prev, next) = (f.t(MsgKey::FilesPrevPage), f.t(MsgKey::FilesNextPage));
            let (pw, nw) = (f.button_width(&prev, Kind::Default), f.button_width(&next, Kind::Default));
            if range.start > 0 {
                f.button(ControlId::FilesPage(false), Rect::new(x, y, pw, 40.0), Kind::Default, &prev, false);
            }
            if range.end < l.entries.len() {
                f.button(ControlId::FilesPage(true), Rect::new(x + w - nw, y, nw, 40.0), Kind::Default, &next, false);
            }
            y += 44.0;
        }
        if l.truncated {
            let t = p.label_in(&f.t(MsgKey::FilesTruncated), text::CAPTION, w, Align::Start);
            p.draw(&t, x, y, pal.soft);
            y += t.h + 6.0;
        }
    }
    y += 12.0;

    // Where received files go.
    let shown = f.st.receive_dir().map(|d| d.display().to_string()).unwrap_or_default();
    let save = f.st.tm(&Msg::new(MsgKey::FilesSaveTo).with(shown));
    let sw = f.button_width(&save, Kind::Default).min(w);
    f.button(ControlId::FilesRecvFolder, Rect::new(x, y, sw, 40.0), Kind::Default, &save, false);
    y += 40.0 + 12.0;

    // Choose files: a dashed box, centred.
    let drop_text = f.st.tm(&Msg::new(MsgKey::FilesDrop).with(peer));
    let dl = p.label_in(&drop_text, text::BODY, w - 44.0, Align::Center);
    let choose = f.t(MsgKey::ChooseFiles);
    let chw = f.button_width(&choose, Kind::Primary);
    let dh = 22.0 + dl.h + 10.0 + 40.0 + 22.0;
    let dr = Rect::new(x, y, w, dh);
    p.stroke_rrect_dashed(dr.inset(0.75), 10.0, 1.5, pal.line);
    p.draw(&dl, x + 22.0, y + 22.0, pal.soft);
    f.button(
        ControlId::FilesChoose,
        Rect::new(x + (w - chw) / 2.0, y + 22.0 + dl.h + 10.0, chw, 40.0),
        Kind::Primary,
        &choose,
        false,
    );
    y += dh + 14.0 + 6.0;

    // The transfers of this session.
    let h2 = p.label(&f.t(MsgKey::Transfers), text::H2);
    p.draw(&h2, x, y, pal.ink);
    y += h2.h + 14.0;
    let transfers: Vec<_> = f.st.viewer_transfers().into_iter().cloned().collect();
    p.push_clip(Rect::new(x, y, w, (b.bottom() - y).max(0.0)));
    for (n, t) in transfers.iter().enumerate() {
        let ry = y + n as f32 * TRANSFER_H;
        let name = p.label_ellipsis(&t.name, text::NAME, (w - 190.0).max(40.0));
        p.draw(&name, x, ry, pal.ink);
        let id = t.id;
        let status = |s: &str, st: swapdesk_ui::tokens::TextStyle, c: Color| {
            let l = p.label(s, st);
            p.draw(&l, x + w - l.w, ry + 1.0, c);
        };
        // Buttons of the second line, from the right: "Cancelar", then "Retomar" on a paused one.
        let cancel = f.t(MsgKey::Cancel);
        let cancel_w = f.button_width(&cancel, Kind::Default);
        let mut right = x + w;
        let mut bar_w = w;
        if !matches!(t.state, TransferState::Done) {
            right -= cancel_w;
            f.button(
                ControlId::TransferCancel(id),
                Rect::new(right, ry + 20.0, cancel_w, 34.0),
                Kind::Default,
                &cancel,
                false,
            );
            bar_w = right - 10.0 - x;
        }
        match &t.state {
            TransferState::Running { done, total, bytes_per_s } => {
                let s = f.st.tm(&Msg::new(MsgKey::TransferProgress)
                    .with(human_size(*done))
                    .with(human_size(*total))
                    .with(human_size(*bytes_per_s)));
                status(&s, text::CAPTION, pal.soft);
                bar(f, x, ry + 33.0, bar_w, *done as f32 / (*total).max(1) as f32, pal.link);
            }
            TransferState::Queued => {
                bar(f, x, ry + 33.0, bar_w, 0.0, pal.soft);
            }
            TransferState::Paused { done, total, .. } => {
                let resume = f.t(MsgKey::TransferResume);
                let rw = f.button_width(&resume, Kind::Default);
                right -= rw + 8.0;
                f.button(
                    ControlId::TransferResume(id),
                    Rect::new(right, ry + 20.0, rw, 34.0),
                    Kind::Default,
                    &resume,
                    false,
                );
                bar_w = right - 10.0 - x;
                status(&f.t(MsgKey::TransferPaused), text::CAPTION, pal.danger);
                bar(f, x, ry + 33.0, bar_w, *done as f32 / (*total).max(1) as f32, pal.soft);
            }
            TransferState::Done => {
                let msg = if t.outgoing {
                    f.st.tm(&Msg::new(MsgKey::TransferSent).with(peer))
                } else {
                    f.t(MsgKey::TransferReceived)
                };
                status(&msg, text::NAME, pal.ok);
                if let Some(saved) = &t.saved_as {
                    let line = f.st.tm(&Msg::new(MsgKey::TransferSavedAs).with(saved.as_str()));
                    let l = p.label_ellipsis(&line, text::CAPTION, w);
                    p.draw(&l, x, ry + 26.0, pal.soft);
                }
            }
            TransferState::Failed { reason } => {
                let key = if matches!(reason, ApiCode::NotPermitted | ApiCode::Refused) {
                    MsgKey::TransferRefused
                } else {
                    MsgKey::TransferFailed
                };
                let l = p.label_ellipsis(&f.t(key), text::CAPTION, (bar_w).max(40.0));
                p.draw(&l, x, ry + 28.0, pal.danger);
            }
        }
    }
    p.pop_clip();
    p.pop_clip();
    // A control scrolled out of the drawer keeps its place in the list but cannot be hit: its box is cut to the drawer.
    for w in &mut f.out.widgets[first_widget..] {
        let (left, top) = (w.rect.x.max(b.x), w.rect.y.max(b.y));
        let (right, bottom) = (w.rect.right().min(b.right()), w.rect.bottom().min(b.bottom()));
        w.rect = if right > left && bottom > top {
            Rect::new(left, top, right - left, bottom - top)
        } else {
            Rect::new(b.x, b.y, 0.0, 0.0)
        };
    }
}

/// Entries per page: the same number the view model pages by.
const UI_PAGE: usize = swapdesk_ui::FILES_PAGE;

fn bar(f: &Frame, x: f32, y: f32, w: f32, frac: f32, color: Color) {
    f.p.fill_rrect(Rect::new(x, y, w, 6.0), 3.0, f.pal.line);
    f.p.fill_rrect(Rect::new(x, y, w * frac.clamp(0.0, 1.0), 6.0), 3.0, color);
}

#[cfg(test)]
mod tests {
    use swapdesk_ui::mock::Scenario;
    use swapdesk_ui::{ControlId, DirEntryView, Listing, Reply, Ticket, UiEvent, UiState, WindowKind};

    use super::*;
    use crate::offscreen::{design_size, render};
    use crate::view::Interaction;

    fn panel_state() -> UiState {
        let mut s = Scenario::Visualizador.prepared_state();
        s.activate(ControlId::TbFiles);
        let entries = (0..12)
            .map(|i| DirEntryView {
                name: format!("arquivo-{i:02}.pdf"),
                is_dir: i % 4 == 0,
                size: 1500 * i,
                mtime_ms: 1,
            })
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
            let r2 =
                render(&s, WindowKind::Viewer, design_size(WindowKind::Viewer), 1.0, &end).expect("renders scrolled");
            let widget =
                |r: &crate::offscreen::Rendered, id| r.output.widgets.iter().find(|w| w.id == id).map(|w| w.rect);
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
}
