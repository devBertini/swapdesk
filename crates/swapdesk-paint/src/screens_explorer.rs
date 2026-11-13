//! The Arquivos view of the session window (`docs/FILES-UI.md` 2.3, slice 1): the command bar ("Tela" and the primary
//! "Copiar para ..."), the navigation pane with each computer's places, two explorer panes side by side (this computer
//! left, the other one right) with back, forward, up, the breadcrumb, the column header, the rows and the status line,
//! and the transfers bar. What each control does is `swapdesk_ui::UiState::activate`; this file only lays them out.

use swapdesk_ui::tokens::text;
use swapdesk_ui::{
    CardStyle, CardView, ControlId, DragView, EditId, ManageView, MsgKey, PaneView, SORT_MODIFIED, SORT_NAME,
    SORT_SIZE, SORT_TYPE, TransferState, VolumeType,
};

use crate::painter::{Align, Rect};
use crate::screens_viewer::human_size;
use crate::view::{Frame, Kind, icons};

const CMD_H: f32 = 56.0;
const NAV_W: f32 = 208.0;
const TRANSFERS_H: f32 = 44.0;
const TRANSFERS_OPEN_H: f32 = 264.0;
const ROW_H: f32 = 30.0;
const PANE_HEADER_H: f32 = 44.0;
const NAV_ROW_H: f32 = 40.0;
const COLUMNS_H: f32 = 28.0;
const STATUS_H: f32 = 28.0;
const PLACE_H: f32 = 32.0;
const TRANSFER_ROW_H: f32 = 58.0;

const FILE: &str = "M6 3h8l4 4v14H6zM14 3v4h4";
const BACK: &str = "M15 6l-6 6 6 6";
const FORWARD: &str = "M9 6l6 6-6 6";
const UP: &str = "M12 19V5M6 11l6-6 6 6";
/// The mark of a volume that can go away (removable, optical, network): an eject sign.
const EJECT: &str = "M6 14l6-7 6 7zM6 18h12";
const REFRESH: &str = "M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6";
const DRIVE: &str = "M3 13h18v6H3zM5 13l2-8h10l2 8M7 16h.01";
const HOME: &str = "M4 11l8-7 8 7v9h-5v-6H9v6H4z";
const PICTURE: &str = "M3 5h18v14H3zM3 16l5-5 4 4 3-3 6 6";

/// Paints the Arquivos view in `body` (below the session's top bar).
pub(crate) fn explorer(f: &mut Frame, body: Rect) {
    let (p, pal) = (f.p, f.pal);
    p.fill_rect(body, pal.canvas);
    let has_transfers = !f.st.viewer_transfers().is_empty();
    let tr_h = if !has_transfers {
        0.0
    } else if f.st.explorer_transfers_open() {
        TRANSFERS_OPEN_H
    } else {
        TRANSFERS_H
    };

    // ----- command bar -----
    let bar = Rect::new(body.x, body.y, body.w, CMD_H);
    p.fill_rect(bar, pal.surface);
    p.fill_rect(Rect::new(bar.x, bar.bottom() - p.hairline(), bar.w, p.hairline()), pal.line);
    let tela = f.t(MsgKey::ExplorerBackToScreen);
    let tw = f.button_width(&tela, Kind::Default) + 26.0;
    let tr = Rect::new(bar.x + 16.0, bar.y + 8.0, tw, 40.0);
    f.button(ControlId::ExpTela, tr, Kind::Default, "", false);
    p.svg(PICTURE, tr.x + 14.0, tr.y + 11.0, 18.0, 1.8, pal.ink);
    let tl = p.label(&tela, text::BTN);
    p.draw(&tl, tr.x + 14.0 + 18.0 + 8.0, tr.y + (40.0 - tl.h) / 2.0, pal.ink);
    if let Some(note) = f.st.explorer_note() {
        let l = p.label_ellipsis(&note, text::CAPTION, body.w * 0.4);
        p.draw(&l, tr.right() + 16.0, bar.y + (CMD_H - l.h) / 2.0, pal.soft);
    }
    let copy = f.st.explorer_copy_label();
    let cw = f.button_width(&copy, Kind::Primary);
    let kind = if f.st.explorer_can_copy() { Kind::Primary } else { Kind::Default };
    f.button(ControlId::ExpCopy, Rect::new(bar.right() - 16.0 - cw, bar.y + 8.0, cw, 40.0), kind, &copy, false);
    // Slice 2a: this computer's commands, before the primary button; disabled, never hidden, when they cannot act.
    let m = f.st.explorer_manage();
    let mut x = bar.right() - 16.0 - cw - 12.0;
    for (id, key, on) in [
        (ControlId::ExpProps, MsgKey::ExplorerProps, m.can_props),
        (ControlId::ExpDelete, MsgKey::ExplorerDelete, m.can_delete),
        (ControlId::ExpRename, MsgKey::ExplorerRename, m.can_rename),
        (ControlId::ExpNewFolder, MsgKey::ExplorerNewFolder, m.can_new_folder),
    ] {
        let label = f.t(key);
        let w = f.button_width(&label, Kind::Default);
        x -= w;
        command_button(f, id, Rect::new(x, bar.y + 8.0, w, 40.0), &label, on);
        x -= 8.0;
    }

    // ----- navigation pane -----
    let main_h = body.h - CMD_H - tr_h;
    let nav = Rect::new(body.x, bar.bottom(), NAV_W, main_h);
    p.fill_rect(nav, pal.sunken);
    p.fill_rect(Rect::new(nav.right() - p.hairline(), nav.y, p.hairline(), nav.h), pal.line);
    p.push_clip(nav);
    let mut y = nav.y + 16.0;
    let peer = f.st.explorer_pane(true).title;
    let mut last_there = None;
    for place in f.st.explorer_places() {
        if last_there != Some(place.there) {
            if last_there.is_some() {
                y += 16.0;
            }
            let head = if place.there { peer.clone() } else { f.t(MsgKey::ExplorerThisComputer) };
            let h = p.label_ellipsis(&head.to_uppercase(), text::GROUP, nav.w - 24.0);
            p.draw(&h, nav.x + 12.0, y, pal.soft);
            y += h.h + 8.0;
            last_there = Some(place.there);
        }
        let r = Rect::new(nav.x + 8.0, y, nav.w - 16.0, PLACE_H);
        let id = ControlId::ExpPlace(place.there, place.index);
        if place.selected {
            p.fill_rrect(r, 6.0, pal.link_tint);
        }
        f.tint(id, r, 6.0, pal.ink, 0.05, 0.09);
        let ink = if place.selected { pal.link } else { pal.ink };
        p.svg(if place.volume { DRIVE } else { HOME }, r.x + 10.0, r.y + 8.0, 16.0, 1.7, ink);
        // A volume that can go away carries the eject mark at its end; hovered, a volume shows its space there.
        let ejectable =
            matches!(place.volume_kind, Some(VolumeType::Removable | VolumeType::Optical | VolumeType::Network));
        let mut end = r.right() - 8.0;
        if ejectable {
            end -= 16.0;
            p.svg(EJECT, end, r.y + 9.0, 14.0, 1.6, pal.soft);
            end -= 4.0;
        }
        let hint = place.hint.as_ref().filter(|_| f.hovered(id));
        if let Some(h) = hint {
            let hl = p.label_ellipsis(h, text::CAPTION, (end - r.x - 34.0) * 0.6);
            end -= hl.w;
            p.draw(&hl, end, r.y + (PLACE_H - hl.h) / 2.0, pal.soft);
            end -= 6.0;
        }
        let l = p.label_ellipsis(&place.label, text::BODY, end - r.x - 34.0);
        p.draw(&l, r.x + 34.0, r.y + (PLACE_H - l.h) / 2.0, ink);
        f.register(id, r, 6.0);
        y += PLACE_H + 2.0;
    }
    p.pop_clip();

    // ----- the two panes -----
    let area_x = nav.right() + 12.0;
    let pw = ((body.right() - area_x - 12.0 - 12.0) / 2.0).max(120.0);
    let ph = main_h - 24.0;
    for (i, there) in [false, true].into_iter().enumerate() {
        let r = Rect::new(area_x + i as f32 * (pw + 12.0), nav.y + 12.0, pw, ph);
        let v = f.st.explorer_pane(there);
        pane(f, r, &v);
    }

    // ----- transfers bar -----
    if has_transfers {
        transfers(f, Rect::new(body.x, body.bottom() - tr_h, body.w, tr_h));
    }

    // ----- a drag, over the panes (slice 3) -----
    if let Some(d) = f.st.explorer_drag() {
        drag(f, &d);
    }

    // ----- the cards, over the view -----
    if let Some(card) = m.trash_there.as_ref().or(m.conflict.as_ref()) {
        view_card(f, body, card);
    } else if let Some((title, body_text)) = &m.forever {
        let cancel = f.t(MsgKey::Cancel);
        let delete = f.t(MsgKey::ExplorerForeverButton);
        explorer_card(
            f,
            body,
            title,
            std::slice::from_ref(body_text),
            &[
                (ControlId::ExpForeverCancel, cancel, Kind::Default),
                (ControlId::ExpForeverConfirm, delete, Kind::Danger),
            ],
        );
    } else if let Some((title, lines)) = &m.props {
        let close = f.t(MsgKey::Close);
        explorer_card(f, body, title, lines, &[(ControlId::ExpPropsClose, close, Kind::Default)]);
    }
}

/// A command of the bar: a normal button, or the same box with its label in soft ink when it cannot act now (it is
/// still a place a click reaches; the state ignores it).
fn command_button(f: &mut Frame, id: ControlId, r: Rect, label: &str, on: bool) {
    if on {
        f.button(id, r, Kind::Default, label, false);
        return;
    }
    let (p, pal) = (f.p, f.pal);
    p.bordered(r, 8.0, pal.line, pal.surface);
    let l = p.label(label, text::BTN);
    p.draw(&l, r.x + (r.w - l.w) / 2.0, r.y + (r.h - l.h) / 2.0, pal.soft);
    f.register(id, r, 8.0);
}

/// A card centred over the view (D51): its title, its lines, and its buttons as one group, the confirming one on the
/// right (skill laying-out-buttons).
fn explorer_card(f: &mut Frame, area: Rect, title: &str, lines: &[String], buttons: &[(ControlId, String, Kind)]) {
    let p = f.p;
    let w = 420.0_f32.min(area.w - 48.0);
    let t = p.label_in(title, text::NAME, w - 48.0, Align::Center);
    let ls: Vec<_> = lines.iter().map(|l| p.label_in(l, text::BODY, w - 48.0, Align::Center)).collect();
    let h = 24.0 + t.h + 10.0 + ls.iter().map(|l| l.h + 4.0).sum::<f32>() + 20.0 + 40.0 + 24.0;
    p.fill_rect(area, f.fx.shadow_dialog);
    let card = Rect::new(p.snap(area.x + (area.w - w) / 2.0), p.snap(area.y + (area.h - h) / 2.0), w, h);
    f.panel(card, f.pal.surface);
    let mut y = card.y + 24.0;
    p.draw(&t, card.x + 24.0, y, f.pal.ink);
    y += t.h + 10.0;
    for l in &ls {
        p.draw(l, card.x + 24.0, y, f.pal.soft);
        y += l.h + 4.0;
    }
    y += 20.0;
    let widths: Vec<f32> = buttons.iter().map(|(_, s, k)| f.button_width(s, *k)).collect();
    let group = widths.iter().sum::<f32>() + 8.0 * (buttons.len().saturating_sub(1)) as f32;
    let mut x = card.x + (w - group) / 2.0;
    for ((id, s, k), bw) in buttons.iter().zip(&widths) {
        f.button(*id, Rect::new(x, y, *bw, 40.0), *k, s, false);
        x += bw + 8.0;
    }
}

/// The drag of slice 3 (FILES-UI.md 4.1): the target outlined in `link` (a pane's border at 2 px), then the pill under
/// the pointer (the first item's icon and name, or the count) and the caption under it.
pub(crate) fn drag(f: &mut Frame, d: &DragView) {
    let (p, pal) = (f.p, f.pal);
    if let Some(target) = d.target
        && let Some(w) = f.out.widgets.iter().rev().find(|w| w.id == target)
    {
        let width = if matches!(target, ControlId::ExpPane(_)) { 2.0 } else { 1.5 };
        p.stroke_rrect(w.rect, w.radius.max(4.0), width, pal.link);
    }
    let name = p.label_ellipsis(&d.pill, text::SMALL_BOLD, 220.0);
    let pill = Rect::new(p.snap(d.at.0 + 14.0), p.snap(d.at.1 + 14.0), name.w + 46.0, 30.0);
    p.shadow(pill, 15.0, 4.0, 12.0, f.fx.shadow_dialog);
    p.bordered(pill, 15.0, pal.line, pal.surface);
    let icon = if d.is_dir { icons::FOLDER } else { FILE };
    p.svg(icon, pill.x + 12.0, pill.y + 7.0, 16.0, 1.6, if d.is_dir { pal.link } else { pal.soft });
    p.draw(&name, pill.x + 34.0, pill.y + (pill.h - name.h) / 2.0, pal.ink);
    if let Some(c) = &d.caption {
        let l = p.label_ellipsis(c, text::CAPTION, 320.0);
        let cr = Rect::new(pill.x, pill.bottom() + 6.0, l.w + 16.0, l.h + 8.0);
        p.fill_rrect(cr, 6.0, pal.surface);
        p.draw(&l, cr.x + 8.0, cr.y + 4.0, if d.caption_is_reason { pal.soft } else { pal.link });
    }
}

/// A card of the other computer's pane (slice 2b): the "Mandar para a Lixeira do ..." card and the conflict card, with
/// its check box and its buttons from the left, the confirming or default one last; a button that cannot act is drawn
/// in soft ink and the state ignores it.
fn view_card(f: &mut Frame, area: Rect, card: &CardView) {
    let p = f.p;
    let w = 460.0_f32.min(area.w - 48.0);
    let t = p.label_in(&card.title, text::NAME, w - 48.0, Align::Center);
    let ls: Vec<_> = card.lines.iter().map(|l| p.label_in(l, text::BODY, w - 48.0, Align::Center)).collect();
    let check_h = if card.check.is_some() { 36.0 } else { 0.0 };
    let h = 24.0 + t.h + 10.0 + ls.iter().map(|l| l.h + 4.0).sum::<f32>() + check_h + 20.0 + 40.0 + 24.0;
    p.fill_rect(area, f.fx.shadow_dialog);
    let r = Rect::new(p.snap(area.x + (area.w - w) / 2.0), p.snap(area.y + (area.h - h) / 2.0), w, h);
    f.panel(r, f.pal.surface);
    let mut y = r.y + 24.0;
    p.draw(&t, r.x + 24.0, y, f.pal.ink);
    y += t.h + 10.0;
    for l in &ls {
        p.draw(l, r.x + 24.0, y, f.pal.soft);
        y += l.h + 4.0;
    }
    if let Some((id, label, on)) = &card.check {
        y += 8.0;
        f.checkbox(*id, r.x + 24.0, y, w - 48.0, *on, label);
        y += check_h - 8.0;
    }
    y += 20.0;
    let kind = |s: CardStyle| match s {
        CardStyle::Default => Kind::Default,
        CardStyle::Primary => Kind::Primary,
        CardStyle::Danger => Kind::Danger,
    };
    let widths: Vec<f32> = card.buttons.iter().map(|b| f.button_width(&b.label, kind(b.style))).collect();
    let group = widths.iter().sum::<f32>() + 8.0 * (card.buttons.len().saturating_sub(1)) as f32;
    let mut x = r.x + (w - group) / 2.0;
    for (b, bw) in card.buttons.iter().zip(&widths) {
        let br = Rect::new(x, y, *bw, 40.0);
        if b.enabled {
            f.button(b.id, br, kind(b.style), &b.label, false);
        } else {
            command_button(f, b.id, br, &b.label, false);
        }
        x += bw + 8.0;
    }
}

fn icon_button(f: &mut Frame, id: ControlId, r: Rect, path: &str, enabled: bool) {
    let (p, pal) = (f.p, f.pal);
    if enabled {
        f.tint(id, r, 8.0, pal.ink, 0.06, 0.11);
        f.register(id, r, 8.0);
    }
    let ink = if enabled { pal.ink } else { pal.soft.with_alpha(0.5) };
    p.svg(path, r.x + (r.w - 18.0) / 2.0, r.y + (r.h - 18.0) / 2.0, 18.0, 1.8, ink);
}

/// Column positions inside a pane: (x of size, x of type, x of modified, whether type and modified show).
fn columns(r: Rect) -> (f32, Option<f32>, Option<f32>) {
    let right = r.right() - 14.0;
    let modified = (r.w >= 400.0).then_some(right - 132.0);
    let after_mod = modified.unwrap_or(right);
    let kind = (r.w >= 560.0).then_some(after_mod - 128.0 - 8.0);
    let size = kind.unwrap_or(after_mod) - 88.0 - 8.0;
    (size, kind, modified)
}

fn pane(f: &mut Frame, r: Rect, v: &PaneView) {
    let (p, pal) = (f.p, f.pal);
    f.panel(r, pal.surface);
    // The pane itself is a place a click or a drop reaches (its empty area); registered first, so its rows win.
    f.register(ControlId::ExpPane(v.there), r, 10.0);
    if v.focused {
        p.stroke_rrect(r, 10.0, 1.5, pal.link);
    }
    p.push_clip(r);
    // Header: the laptop and the title.
    f.laptop(r.x + 14.0, r.y + 12.0, 20.0, 1.6, pal.soft);
    let there = v.there;
    let m: ManageView = f.st.explorer_manage();
    let mut header_end = r.right() - 14.0;
    if v.hidden_switch {
        // Each pane's hidden items switch (FILES-UI.md 2.5), at the right of its header.
        let id = if there { ControlId::ExpHiddenThere } else { ControlId::ExpHidden };
        let label = f.t(if v.show_hidden { MsgKey::ExplorerHideHidden } else { MsgKey::ExplorerShowHidden });
        let l = p.label_ellipsis(&label, text::CAPTION, r.w * 0.45);
        let hr = Rect::new(header_end - l.w - 12.0, r.y + 8.0, l.w + 12.0, 28.0);
        f.tint(id, hr, 6.0, pal.ink, 0.05, 0.09);
        p.draw(&l, hr.x + 6.0, hr.y + (28.0 - l.h) / 2.0, pal.link);
        f.register(id, hr, 6.0);
        header_end = hr.x - 8.0;
    }
    if let Some(free) = &v.free {
        // The free space of the shown folder's volume, before the switch.
        let l = p.label_ellipsis(free, text::CAPTION, r.w * 0.4);
        p.draw(&l, header_end - l.w, r.y + (PANE_HEADER_H - l.h) / 2.0, pal.soft);
        header_end -= l.w + 12.0;
    }
    // The title takes what the header has left.
    let title = p.label_ellipsis(&v.title, text::NAME, (header_end - r.x - 42.0).max(40.0));
    p.draw(&title, r.x + 42.0, r.y + (PANE_HEADER_H - title.h) / 2.0, pal.ink);
    // Navigation row: back, forward, up, refresh and the breadcrumb.
    let ny = r.y + PANE_HEADER_H;
    icon_button(f, ControlId::ExpBack(there), Rect::new(r.x + 8.0, ny + 4.0, 32.0, 32.0), BACK, v.can_back);
    icon_button(f, ControlId::ExpForward(there), Rect::new(r.x + 44.0, ny + 4.0, 32.0, 32.0), FORWARD, v.can_forward);
    icon_button(f, ControlId::ExpUp(there), Rect::new(r.x + 80.0, ny + 4.0, 32.0, 32.0), UP, v.can_up);
    icon_button(f, ControlId::ExpRefresh(there), Rect::new(r.right() - 40.0, ny + 4.0, 32.0, 32.0), REFRESH, true);
    let addr = Rect::new(r.x + 120.0, ny + 4.0, (r.right() - 48.0 - r.x - 120.0).max(40.0), 32.0);
    p.bordered(addr, 8.0, pal.line, pal.sunken);
    let crumbs = v.crumbs.join("  ›  ");
    let cl = p.label_ellipsis(&crumbs, text::BODY, addr.w - 20.0);
    p.draw(&cl, addr.x + 10.0, addr.y + (32.0 - cl.h) / 2.0, pal.ink);
    // Column header.
    let hy = ny + NAV_ROW_H;
    let head = Rect::new(r.x + 1.0, hy, r.w - 2.0, COLUMNS_H);
    p.fill_rect(head, pal.sunken);
    p.fill_rect(Rect::new(head.x, head.bottom() - p.hairline(), head.w, p.hairline()), pal.line);
    let (size_x, kind_x, mod_x) = columns(r);
    let col = |f: &mut Frame, sort: u8, key: MsgKey, x: f32, w: f32, end: bool| {
        let id = ControlId::ExpSort(there, sort);
        let cell = Rect::new(x, hy, w, COLUMNS_H);
        f.tint(id, cell, 0.0, f.pal.ink, 0.05, 0.09);
        let mut s = f.t(key);
        if v.sort.0 == sort {
            s.push_str(if v.sort.1 { " ▲" } else { " ▼" });
        }
        let l = f.p.label_ellipsis(&s, text::GROUP, w - 8.0);
        let lx = if end { x + w - l.w - 4.0 } else { x + 4.0 };
        f.p.draw(&l, lx, hy + (COLUMNS_H - l.h) / 2.0, f.pal.soft);
        f.register(id, cell, 0.0);
    };
    col(f, SORT_NAME, MsgKey::ExplorerColName, r.x + 36.0, size_x - r.x - 40.0, false);
    col(f, SORT_SIZE, MsgKey::ExplorerColSize, size_x, 88.0, true);
    if let Some(x) = kind_x {
        col(f, SORT_TYPE, MsgKey::ExplorerColType, x + 8.0, 128.0, false);
    }
    if let Some(x) = mod_x {
        col(f, SORT_MODIFIED, MsgKey::ExplorerColModified, x, 132.0, false);
    }
    // Rows, states and the status line.
    let rows = Rect::new(r.x + 1.0, head.bottom(), r.w - 2.0, r.bottom() - STATUS_H - head.bottom());
    if v.loading && v.rows.is_empty() {
        for i in 0..8 {
            let y = rows.y + 6.0 + i as f32 * ROW_H;
            f.block(Rect::new(rows.x + 14.0, y + 7.0, 16.0, 16.0), 3.0);
            f.block(Rect::new(rows.x + 40.0, y + 9.0, 120.0 + (i % 3) as f32 * 50.0, 12.0), 4.0);
            f.block(Rect::new(size_x + 40.0, y + 9.0, 48.0, 12.0), 4.0);
        }
    } else if let Some((title, why)) = &v.error {
        center_state(f, rows, title, why);
    } else if v.rows.is_empty() {
        center_state(f, rows, "", &f.t(MsgKey::ExplorerEmpty));
    } else {
        let total = v.rows.len() as f32 * ROW_H;
        let max = (total - rows.h).max(0.0);
        let scroll = if there {
            f.out.second_list = Some((rows, max));
            f.ix.drawer_scroll.clamp(0.0, max)
        } else {
            f.out.scroll_max = max;
            f.out.list_area = Some(rows);
            f.ix.scroll.clamp(0.0, max)
        };
        p.push_clip(rows);
        let first = (scroll / ROW_H).floor() as usize;
        let visible = (rows.h / ROW_H).ceil() as usize + 1;
        for (i, row) in v.rows.iter().enumerate().skip(first).take(visible) {
            let y = rows.y + i as f32 * ROW_H - scroll;
            let rr = Rect::new(rows.x + 4.0, y, rows.w - 8.0, ROW_H);
            let id = ControlId::ExpRow(there, u16::try_from(i).unwrap_or(u16::MAX));
            if row.selected {
                p.fill_rrect(rr, 4.0, pal.link_tint);
            }
            f.tint(id, rr, 4.0, pal.ink, 0.04, 0.08);
            let ink = if row.is_dir { pal.link } else { pal.soft };
            p.svg(if row.is_dir { icons::FOLDER } else { FILE }, r.x + 14.0, y + 7.0, 16.0, 1.6, ink);
            let renaming = there == m.renaming_there && m.renaming.as_ref().is_some_and(|(n, _)| *n == row.name);
            if renaming {
                // The inline rename: a native field over the name cell.
                let cell = Rect::new(r.x + 36.0, y + 2.0, size_x - r.x - 44.0, ROW_H - 4.0);
                f.edit_field(EditId::ExpRename, cell, &row.name, "");
            } else {
                let name = p.label_ellipsis(&row.name, text::BODY, size_x - r.x - 48.0);
                p.draw(&name, r.x + 40.0, y + (ROW_H - name.h) / 2.0, pal.ink);
            }
            let size = p.label(&row.size, text::CAPTION);
            p.draw(&size, size_x + 88.0 - size.w - 4.0, y + (ROW_H - size.h) / 2.0, pal.soft);
            if let Some(x) = kind_x {
                let l = p.label_ellipsis(&row.kind, text::CAPTION, 124.0);
                p.draw(&l, x + 12.0, y + (ROW_H - l.h) / 2.0, pal.soft);
            }
            if let Some(x) = mod_x {
                let l = p.label_ellipsis(&row.modified, text::CAPTION, 128.0);
                p.draw(&l, x + 4.0, y + (ROW_H - l.h) / 2.0, pal.soft);
            }
            // Only the visible part of a row is a hit box.
            let hit_top = rr.y.max(rows.y);
            let hit = Rect::new(rr.x, hit_top, rr.w, (rr.bottom().min(rows.bottom()) - hit_top).max(0.0));
            if hit.h > 0.0 {
                f.register(id, hit, 4.0);
            }
        }
        p.pop_clip();
    }
    let sy = r.bottom() - STATUS_H;
    p.fill_rect(Rect::new(r.x + 1.0, sy, r.w - 2.0, p.hairline()), pal.line);
    // The status says why a name was refused or a change did not happen, in place of the count; the right pane also
    // carries the 1.0 note there, as information.
    let refused = (there == m.renaming_there).then(|| m.renaming.as_ref().and_then(|(_, e)| e.clone())).flatten();
    let why = refused.or_else(|| if there { None } else { m.notice.clone() });
    let (status, ink) = match (&why, &v.notice) {
        (Some(w), _) => (w.as_str(), pal.danger),
        (None, Some(n)) if there => (n.as_str(), if v.notice_error { pal.danger } else { pal.soft }),
        _ => (v.status.as_str(), pal.soft),
    };
    let st = p.label_ellipsis(status, text::CAPTION, r.w * 0.6);
    p.draw(&st, r.x + 14.0, sy + (STATUS_H - st.h) / 2.0, ink);
    if let Some(t) = &v.truncated {
        let l = p.label_ellipsis(t, text::CAPTION, r.w * 0.35);
        p.draw(&l, r.right() - 14.0 - l.w, sy + (STATUS_H - l.h) / 2.0, pal.soft);
    }
    p.pop_clip();
}

/// An empty or error state centred in the rows area (D51).
fn center_state(f: &Frame, area: Rect, title: &str, body: &str) {
    let (p, pal) = (f.p, f.pal);
    let w = (area.w - 48.0).min(360.0);
    let x = area.x + (area.w - w) / 2.0;
    let t = (!title.is_empty()).then(|| p.label_in(title, text::NAME, w, Align::Center));
    let b = p.label_in(body, text::BODY, w, Align::Center);
    let h = 32.0 + 12.0 + t.as_ref().map_or(0.0, |t| t.h + 6.0) + b.h;
    let mut y = area.y + (area.h - h) / 2.0;
    p.svg(icons::FOLDER, area.x + (area.w - 32.0) / 2.0, y, 32.0, 1.6, pal.soft);
    y += 32.0 + 12.0;
    if let Some(t) = t {
        p.draw(&t, x, y, pal.ink);
        y += t.h + 6.0;
    }
    p.draw(&b, x, y, pal.soft);
}

fn transfers(f: &mut Frame, r: Rect) {
    let (p, pal) = (f.p, f.pal);
    p.fill_rect(r, pal.surface);
    p.fill_rect(Rect::new(r.x, r.y, r.w, p.hairline()), pal.line);
    let list = f.st.viewer_transfers().into_iter().cloned().collect::<Vec<_>>();
    let running = list.iter().filter(|t| matches!(t.state, TransferState::Running { .. })).count();
    let head = p.label(&f.t(MsgKey::ExplorerTransfers), text::NAME);
    p.draw(&head, r.x + 16.0, r.y + (TRANSFERS_H - head.h) / 2.0, pal.ink);
    let summary = f.st.tm(&swapdesk_ui::Msg::new(MsgKey::ExplorerTransfersRunning).with(running.to_string()));
    let sl = p.label(&summary, text::CAPTION);
    p.draw(&sl, r.x + 16.0 + head.w + 12.0, r.y + (TRANSFERS_H - sl.h) / 2.0, pal.soft);
    let open = f.st.explorer_transfers_open();
    let label = f.t(if open { MsgKey::ExplorerHide } else { MsgKey::ExplorerShow });
    let bw = f.button_width(&label, Kind::Default);
    f.button(
        ControlId::ExpTransfers,
        Rect::new(r.right() - 16.0 - bw, r.y + 6.0, bw, 32.0),
        Kind::Default,
        &label,
        false,
    );
    if !open {
        return;
    }
    let area = Rect::new(r.x, r.y + TRANSFERS_H, r.w, r.h - TRANSFERS_H);
    p.push_clip(area);
    for (i, t) in list.iter().rev().enumerate() {
        let y = area.y + i as f32 * TRANSFER_ROW_H;
        if y > area.bottom() {
            break;
        }
        let name = p.label_ellipsis(&t.name, text::BODY, r.w * 0.5);
        p.draw(&name, r.x + 16.0, y + 8.0, pal.ink);
        let (fraction, line) = match t.state {
            TransferState::Running { done, total, .. } => {
                (done as f32 / total.max(1) as f32, format!("{} / {}", human_size(done), human_size(total)))
            }
            TransferState::Paused { done, total, .. } => {
                (done as f32 / total.max(1) as f32, f.t(MsgKey::TransferPaused))
            }
            TransferState::Done => (1.0, f.t(MsgKey::TransferReceived)),
            TransferState::Failed { .. } => (0.0, f.t(MsgKey::TransferFailed)),
            TransferState::Queued => (0.0, String::new()),
        };
        let l = p.label(&line, text::CAPTION);
        p.draw(&l, r.x + 16.0, y + 8.0 + name.h + 4.0, pal.soft);
        // Buttons at the right (FILES-UI.md 5: "Cancelar" on the left, "Retomar" on the right), the bar before them.
        let mut bx = r.right() - 16.0;
        if matches!(t.state, TransferState::Paused { .. }) {
            let label = f.t(MsgKey::TransferResume);
            let w = f.button_width(&label, Kind::Primary);
            bx -= w;
            f.button(ControlId::TransferResume(t.id), Rect::new(bx, y + 9.0, w, 40.0), Kind::Primary, &label, false);
            bx -= 8.0;
        }
        if !matches!(t.state, TransferState::Done) {
            let label = f.t(MsgKey::Cancel);
            let w = f.button_width(&label, Kind::Default);
            bx -= w;
            f.button(ControlId::TransferCancel(t.id), Rect::new(bx, y + 9.0, w, 40.0), Kind::Default, &label, false);
            bx -= 16.0;
        }
        let track = Rect::new(bx - 240.0, y + 26.0, 240.0, 6.0);
        p.fill_rrect(track, 3.0, pal.line);
        p.fill_rrect(Rect::new(track.x, track.y, track.w * fraction.clamp(0.0, 1.0), 6.0), 3.0, pal.link);
    }
    p.pop_clip();
}
