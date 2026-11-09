//! The screens of the main window: sidebar, device detail (overview, permissions, history, notices, waking),
//! Atividade, Ajustes, Pareamento and the dialogs. Laid out in CSS pixels by hand, following the design's flex
//! boxes; the numbers in comments are the design's paddings and gaps.

use swapdesk_ui::format;
use swapdesk_ui::list::{max_scroll, visible_rows};
use swapdesk_ui::tokens::{Color, fixed, layout, text};
use swapdesk_ui::{
    ControlId, DataKind, DeviceStatus, DeviceTab, DeviceView, DialogKind, EditId, Msg, MsgKey, Nav, NoticeKind,
    PairingState, Ready, Screen, SelectId, SessionState, WindowKind,
};

use crate::gfx::{Align, Rect};
use crate::view::{Frame, Kind, icons};

const SIDEBAR_W: f32 = layout::SIDEBAR_W;

pub fn paint(f: &mut Frame) {
    f.p.clear(f.pal.canvas);
    if f.st.screen() == Screen::Pairing {
        pairing(f);
    } else {
        sidebar(f);
        match f.st.nav() {
            Nav::Devices => devices_main(f),
            Nav::Activity => activity_main(f),
            Nav::Settings => settings_main(f),
        }
        error_toast(f);
    }
    if f.st.incoming_session_open() {
        f.out.widgets.clear();
        f.out.edits.clear();
        incoming(f);
    } else if let Some(kind) = f.st.dialog(WindowKind::Main) {
        f.out.widgets.clear();
        f.out.edits.clear();
        dialog(f, &kind);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Sidebar

enum Dot {
    Solid(Color),
    Ring,
}

struct Row {
    id: Option<ControlId>,
    name: String,
    status: String,
    dot: Dot,
    selected: bool,
}

fn sidebar(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let h = f.h;
    p.fill_rect(Rect::new(0.0, 0.0, SIDEBAR_W, h), pal.sunken);
    p.fill_rect(Rect::new(SIDEBAR_W - bw, 0.0, bw, h), pal.line);

    // padding 18 12; the rows are 247 px wide (272 - 2 x 12 - the 1 px border).
    let x = 12.0;
    let w = SIDEBAR_W - 24.0 - bw;
    let mut y = 18.0;

    // Logo: 22 px mark, gap 10, padding 0 10.
    let logo = p.label(&f.t(MsgKey::AppName), text::LOGO);
    logo_mark(f, x + 10.0, y + (logo.h - 22.0) / 2.0);
    p.draw(&logo, x + 10.0 + 22.0 + 10.0, y, pal.ink);
    y += logo.h + 22.0;

    // This computer.
    let this_row = match (f.st.this_device(), f.st.settings()) {
        (Some(t), _) => Some((
            t.name.clone(),
            f.t(match t.ready {
                Ready::ReadyToReceive => MsgKey::ReadyToReceive,
                Ready::ServiceOff => MsgKey::ReadyServiceOff,
                Ready::ServiceNeedsApproval => MsgKey::ReadyServiceNeedsApproval,
                Ready::NoNetwork => MsgKey::ReadyNoNetwork,
            }),
            if t.ready == Ready::ReadyToReceive { pal.ok } else { pal.danger },
        )),
        _ => None,
    };
    let loading_this = f.st.placeholder(DataKind::ThisDevice);
    let rows: Vec<Row> = this_row
        .map(|(name, status, c)| Row { id: None, name, status, dot: Dot::Solid(c), selected: false })
        .into_iter()
        .collect();
    y += group(f, x, y, w, &f.t(MsgKey::SidebarThisComputer), &rows, if loading_this { 1 } else { 0 }) + 22.0;

    // Paired and on the network.
    let loading_devices = f.st.placeholder(DataKind::Devices);
    let (paired, network) = f.st.device_groups();
    let nav_devices = f.st.nav() == Nav::Devices;
    let selected_id = f.st.selected_device().map(|d| d.id.clone());
    let make = |i: usize| -> Row {
        let d = &f.st.devices()[i];
        Row {
            id: Some(ControlId::Device(i as u16)),
            name: d.name.clone(),
            status: format::status_line(f.st.language(), d),
            dot: match d.status {
                DeviceStatus::Online { .. } => Dot::Solid(pal.ok),
                DeviceStatus::Asleep | DeviceStatus::Offline => Dot::Solid(pal.line),
                DeviceStatus::OnNetworkUnpaired { .. } => Dot::Ring,
            },
            selected: nav_devices && selected_id.as_deref() == Some(d.id.as_str()),
        }
    };
    let paired_rows: Vec<Row> = paired.iter().map(|&i| make(i)).collect();
    let network_rows: Vec<Row> = network.iter().map(|&i| make(i)).collect();
    if loading_devices || !paired_rows.is_empty() || f.st.fetch(DataKind::Devices) == swapdesk_ui::Fetch::Ready {
        y += group(f, x, y, w, &f.t(MsgKey::SidebarPaired), &paired_rows, if loading_devices { 2 } else { 0 }) + 22.0;
        if loading_devices || !network_rows.is_empty() {
            y += group(f, x, y, w, &f.t(MsgKey::SidebarOnNetwork), &network_rows, if loading_devices { 1 } else { 0 })
                + 22.0;
        }
    }

    // The bottom block hangs from the window's bottom edge: network line, then three 40 px buttons, gap 2.
    let net_text = f.st.network().map(|n| match &n.address {
        Some(a) => f.st.tm(&Msg::new(MsgKey::NetworkLine).with(n.label.clone()).with(a.clone())),
        None => f.st.tm(&Msg::new(MsgKey::NetworkLineNoAddress).with(n.label.clone())),
    });
    let net = p.label_in(net_text.as_deref().unwrap_or("A"), text::SMALL, w - 20.0, Align::Start);
    let net_y = h - 18.0 - net.h;
    let buttons_y = net_y - 22.0 - 124.0;
    address_field(f, x, y, w, buttons_y - 12.0);
    f.button(
        ControlId::PairThisComputer,
        Rect::new(x, buttons_y, w, 40.0),
        Kind::Default,
        &f.t(MsgKey::PairThisComputer),
        true,
    );
    let k = if f.st.nav() == Nav::Activity { Kind::Selected } else { Kind::Quiet };
    f.button(ControlId::NavActivity, Rect::new(x, buttons_y + 42.0, w, 40.0), k, &f.t(MsgKey::NavActivity), true);
    let k = if f.st.nav() == Nav::Settings { Kind::Selected } else { Kind::Quiet };
    f.button(ControlId::NavSettings, Rect::new(x, buttons_y + 84.0, w, 40.0), k, &f.t(MsgKey::NavSettings), true);
    if net_text.is_some() {
        p.draw(&net, x + 10.0, net_y, pal.soft);
    } else if f.st.placeholder(DataKind::Network) {
        f.block(Rect::new(x + 10.0, net_y + 2.0, 170.0, net.h - 4.0), 4.0);
    }
}

/// F-FIND-03: the manual address field at the end of the device list: a caption over a native input (the caption
/// is cut with an ellipsis inside the sidebar's width). On a short window (`limit` is the top of the bottom block's
/// free band) the caption goes first and the field keeps its place right after the list, named for assistive
/// technology; if even that does not fit it is pinned just above the bottom block, so it is never unreachable.
fn address_field(f: &mut Frame, x: f32, y: f32, w: f32, limit: f32) {
    let p = f.p;
    let caption = p.label_ellipsis(&f.t(MsgKey::AddressLabel), text::GROUP, w - 20.0);
    let placeholder = f.t(MsgKey::AddressPlaceholder);
    let with_caption = caption.h + 4.0 + layout::FIELD_H;
    if y + with_caption <= limit {
        p.draw(&caption, x + 10.0, y, f.pal.soft);
        f.edit_field(EditId::Address, Rect::new(x, y + caption.h + 4.0, w, layout::FIELD_H), "", &placeholder);
    } else {
        let top = (y - 8.0).max(0.0).min(limit - layout::FIELD_H);
        f.edit_field(EditId::Address, Rect::new(x, top, w, layout::FIELD_H), "", &placeholder);
    }
}

/// A caption and its rows, gap 4; `skeleton` rows are drawn as blocks instead. Returns the height used.
fn group(f: &mut Frame, x: f32, y: f32, w: f32, title: &str, rows: &[Row], skeleton: usize) -> f32 {
    let p = f.p;
    let caption = p.label(title, text::GROUP);
    p.draw(&caption, x + 10.0, y, f.pal.soft);
    let mut used = caption.h;
    for _ in 0..skeleton {
        used += 4.0;
        used += skeleton_row(f, x, y + used, w);
    }
    for row in rows {
        used += 4.0;
        used += device_row(f, x, y + used, w, row);
    }
    used
}

fn row_height(f: &Frame) -> f32 {
    8.0 + f.p.label("A", text::NAME).h + f.p.label("A", text::SMALL).h + 8.0
}

fn skeleton_row(f: &Frame, x: f32, y: f32, w: f32) -> f32 {
    let h = row_height(f);
    f.p.fill_circle((x + 14.0, y + h / 2.0), 4.0, f.pal.line.with_alpha(0.55));
    f.block(Rect::new(x + 30.0, y + 11.0, (w - 60.0).min(150.0), 12.0), 4.0);
    f.block(Rect::new(x + 30.0, y + 31.0, (w - 100.0).min(100.0), 10.0), 4.0);
    h
}

/// One device row: padding 8 10, gap 12, 8 px dot, name over status. Returns its height.
fn device_row(f: &mut Frame, x: f32, y: f32, w: f32, d: &Row) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let name = p.label_ellipsis(&d.name, text::NAME, w - 10.0 - 8.0 - 12.0 - 10.0);
    let status = p.label_ellipsis(&d.status, text::SMALL, w - 10.0 - 8.0 - 12.0 - 10.0);
    let h = 8.0 + name.h + status.h + 8.0;
    let rect = Rect::new(x, y, w, h);
    if d.selected {
        // box-shadow: 0 0 0 1px var(--line)
        let ring = p.hairline();
        p.fill_rrect(Rect::new(x - ring, y - ring, w + 2.0 * ring, h + 2.0 * ring), 8.0 + ring, pal.line);
        p.fill_rrect(rect, 8.0, pal.surface);
    }
    if let Some(id) = d.id {
        f.tint(id, rect, 8.0, pal.ink, 0.06, 0.11);
        f.register(id, rect, 8.0);
    }
    let center = (x + 10.0 + 4.0, y + h / 2.0);
    match d.dot {
        Dot::Solid(c) => p.fill_circle(center, 4.0, c),
        Dot::Ring => p.stroke_circle(center, 4.0 - 0.75, 1.5, pal.soft),
    }
    let tx = x + 10.0 + 8.0 + 12.0;
    p.draw(&name, tx, y + 8.0, pal.ink);
    p.draw(&status, tx, y + 8.0 + name.h, pal.soft);
    h
}

/// The logo: two rounded cards joined by a short bar, drawn from the design's 24 x 24 SVG at 22 px.
fn logo_mark(f: &Frame, x: f32, y: f32) {
    let p = f.p;
    let (x, y) = (p.snap(x), p.snap(y));
    let s = 22.0 / 24.0;
    let sw = 2.0 * s;
    p.stroke_rrect(Rect::new(x + 2.5 * s, y + 6.0 * s, 8.0 * s, 12.0 * s), 2.0 * s, sw, f.pal.link);
    p.stroke_rrect(Rect::new(x + 13.5 * s, y + 6.0 * s, 8.0 * s, 12.0 * s), 2.0 * s, sw, f.pal.link);
    p.line((x + 10.5 * s, y + 12.0 * s), (x + 13.5 * s, y + 12.0 * s), sw, f.pal.link, true);
}

// ---------------------------------------------------------------------------------------------------------------
// Devices

struct Area {
    x: f32,
    cw: f32,
}

fn area(f: &Frame) -> Area {
    Area { x: SIDEBAR_W + 40.0, cw: (f.w - SIDEBAR_W - 80.0).max(200.0) }
}

fn devices_main(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let mut y = 32.0;
    if f.st.placeholder(DataKind::Devices) {
        skeleton_devices(f, &a);
        return;
    }
    let Some(d) = f.st.selected_device().cloned() else {
        if f.st.fetch(DataKind::Devices) == swapdesk_ui::Fetch::Ready {
            if f.st.crash_notice() {
                y += notice_cards(f, &a, y, &[NoticeKind::Crash]) + 20.0;
            }
            if f.st.no_computer_card() {
                // F20: discovery found nothing for 10 s; the action focuses the address field.
                let (title, body, button) =
                    (f.t(MsgKey::NoComputerTitle), f.t(MsgKey::NoComputerBody), f.t(MsgKey::NoComputerButton));
                let w = ((a.cw - 40.0) / 3.0).max(320.0).min(a.cw);
                state_card(
                    f,
                    (a.x, y, w),
                    icons::WIFI,
                    (pal.link_tint, pal.link),
                    (&title, &body, &button),
                    ControlId::NoComputerAddress,
                );
            } else {
                let l = p.label_in(&f.t(MsgKey::EmptyDevices), text::BODY, a.cw, Align::Start);
                p.draw(&l, a.x, y, pal.soft);
            }
        }
        return;
    };
    let waking = f.st.waking_session().filter(|s| s.device_id == d.id).cloned();

    // Header: title block grows, the buttons sit at the end (gap 16, top-aligned).
    let title = p.label_ellipsis(&d.name, text::H1, a.cw - 260.0);
    let status = p.label_ellipsis(&format::detail_line(f.st.language(), &d), text::BODY, a.cw - 260.0);
    p.draw(&title, a.x, y, pal.ink);
    p.draw(&status, a.x, y + title.h + 6.0, pal.soft);
    if waking.is_none() {
        if d.paired && swapdesk_ui::UiState::can_wake(&d) {
            // Asleep: "Acordar" replaces the connect buttons (F-WAKE-01).
            let label = f.t(MsgKey::Wake);
            let bw = f.button_width(&label, Kind::Primary);
            f.button(ControlId::Wake, Rect::new(a.x + a.cw - bw, y, bw, 40.0), Kind::Primary, &label, false);
        } else if d.paired {
            let control = f.t(MsgKey::Control);
            let view = f.t(MsgKey::ViewOnly);
            let cw = f.button_width(&control, Kind::Primary);
            let vw = f.button_width(&view, Kind::Default);
            let crect = Rect::new(a.x + a.cw - cw, y, cw, 40.0);
            let vrect = Rect::new(crect.x - 16.0 - vw, y, vw, 40.0);
            f.button(ControlId::ViewOnly, vrect, Kind::Default, &view, false);
            f.button(ControlId::Control, crect, Kind::Primary, &control, false);
        } else {
            let label = f.t(MsgKey::PairWithDevice);
            let bw = f.button_width(&label, Kind::Primary);
            f.button(ControlId::PairWithDevice, Rect::new(a.x + a.cw - bw, y, bw, 40.0), Kind::Primary, &label, false);
        }
    }
    y += (title.h + 6.0 + status.h).max(40.0) + 28.0;

    if let Some(w) = waking {
        waking_card(f, &a, y, &d, &w.state);
        return;
    }
    if d.paired {
        y += tabs(f, a.x, y, a.cw) + 28.0;
    }
    let notices = f.st.notices();
    if !notices.is_empty() {
        notice_cards(f, &a, y, &notices);
        return;
    }
    if !d.paired {
        let body = f.t(MsgKey::PairOpenBody);
        let bw = p.hairline();
        let th = f.para_height(&body, text::BODY, a.cw - 44.0 - 2.0 * bw);
        let r = Rect::new(a.x, y, a.cw, 2.0 * bw + 40.0 + th);
        f.panel(r, pal.surface);
        f.para(&body, text::BODY, a.x + bw + 22.0, y + bw + 20.0, a.cw - 44.0 - 2.0 * bw, pal.soft, Align::Start);
        return;
    }
    match f.st.tab() {
        DeviceTab::Overview => {
            y += pair_panel(f, &a, y, &d) + 28.0;
            info_panels(f, &a, y, &d);
        }
        DeviceTab::Permissions => permissions(f, &a, y, &d),
        DeviceTab::History => history(f, &a, y, &d),
    }
}

fn skeleton_devices(f: &mut Frame, a: &Area) {
    let p = f.p;
    let mut y = 32.0;
    let title_h = p.label("A", text::H1).h;
    let body_h = p.label("A", text::BODY).h;
    f.block(Rect::new(a.x, y + 2.0, 280.0, title_h - 4.0), 6.0);
    f.block(Rect::new(a.x, y + title_h + 6.0 + 3.0, 340.0, body_h - 6.0), 4.0);
    f.block(Rect::new(a.x + a.cw - 100.0 - 16.0 - 100.0, y, 100.0, 40.0), 8.0);
    f.block(Rect::new(a.x + a.cw - 100.0, y, 100.0, 40.0), 8.0);
    y += (title_h + 6.0 + body_h).max(40.0) + 28.0;
    let mut tx = a.x;
    for wdt in [80.0, 84.0, 70.0] {
        f.block(Rect::new(tx + 12.0, y + 12.0, wdt, 14.0), 4.0);
        tx += wdt + 28.0;
    }
    y += 40.0 + 28.0;
    let bw = p.hairline();
    let row_h = 64.0 + 10.0 + p.label("A", text::NAME).h + 10.0 + p.label("A", text::SMALL).h;
    let strip_h = bw + 32.0 + row_h + 22.0 + p.label("A", text::BODY).h + 32.0 + bw;
    f.block(Rect::new(a.x, y, a.cw, strip_h), 12.0);
    y += strip_h + 28.0;
    let pw = (a.cw - 20.0) / 2.0;
    let ph = 2.0 * bw + 40.0 + p.label("A", text::H2).h + 10.0 + 2.0 * body_h;
    f.block(Rect::new(a.x, y, pw, ph), 12.0);
    f.block(Rect::new(a.x + pw + 20.0, y, pw, ph), 12.0);
}

/// Tabs: 40 px high, gap 4, padding 0 12, a 2 px underline on the selected one. Returns the container's height.
fn tabs(f: &mut Frame, x: f32, y: f32, cw: f32) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    // The container's 1 px border sits under the tabs' -1 px margin.
    p.fill_rect(Rect::new(x, y + 39.0, cw, bw), pal.line);
    let mut tx = x;
    for (tab, key) in [
        (DeviceTab::Overview, MsgKey::TabOverview),
        (DeviceTab::Permissions, MsgKey::TabPermissions),
        (DeviceTab::History, MsgKey::TabHistory),
    ] {
        let selected = f.st.tab() == tab;
        let id = ControlId::Tab(tab);
        let label = p.label(&f.t(key), if selected { text::TAB_SELECTED } else { text::TAB });
        let rect = Rect::new(tx, y, label.w + 24.0, 40.0);
        if selected {
            p.fill_rect(Rect::new(rect.x, y + 38.0, rect.w, 2.0), pal.ink);
        }
        let color = if selected || f.hovered(id) { pal.ink } else { pal.soft };
        // The 2 px bottom border leaves a 38 px content box.
        p.draw(&label, tx + 12.0, y + (38.0 - label.h) / 2.0, color);
        f.register(id, rect, 0.0);
        tx += rect.w + 4.0;
    }
    39.0 + bw
}

fn this_name(f: &Frame) -> String {
    f.st.this_device().map(|t| t.name.clone()).or_else(|| f.st.settings().map(|s| s.name.clone())).unwrap_or_default()
}

/// The "who controls whom" panel. Returns its height.
fn pair_panel(f: &mut Frame, a: &Area, y: f32, d: &DeviceView) -> f32 {
    let (p, pal) = (f.p, f.pal);
    // padding 32 36, gap 22; columns 180 px wide with the wire between them.
    let bw = p.hairline();
    let inner_w = a.cw - 2.0 * bw - 72.0;
    let note = p.label_in(&f.t(MsgKey::SwapNote), text::BODY, inner_w, Align::Center);
    let row_h = 64.0 + 10.0 + p.label("A", text::NAME).h + 10.0 + p.label("A", text::SMALL).h;
    let h = bw + 32.0 + row_h + 22.0 + note.h + 32.0 + bw;
    f.panel(Rect::new(a.x, y, a.cw, h), pal.surface);

    let left = a.x + bw + 36.0;
    let right = a.x + a.cw - bw - 36.0 - layout::PAIR_COLUMN_W;
    let top = y + bw + 32.0;
    device_column(f, left, top, &this_name(f), &f.t(MsgKey::DeviceCaptionThis), pal.ink);
    device_column(f, right, top, &d.name, &format::os_label(f.st.language(), d.os, &d.os_version), pal.link);

    // The middle column is centred on the row, then pulled up by its -34 px top margin.
    let mid_x = left + layout::PAIR_COLUMN_W;
    let mid_w = right - mid_x;
    let wire_label = p.label_in(&f.t(MsgKey::YouControl), text::WIRE, mid_w, Align::Center);
    let wire_top = top + (row_h - (wire_label.h + 10.0 + 18.0 - 34.0)) / 2.0 - 34.0;
    p.draw(&wire_label, mid_x, wire_top, pal.link);
    let cy = wire_top + wire_label.h + 10.0 + 9.0;
    f.wire(mid_x, mid_x + mid_w, cy, layout::PAIR_WIRE, layout::PAIR_ARROW_W, layout::PAIR_ARROW_H, pal.link);

    p.draw(&note, a.x + bw + 36.0, top + row_h + 22.0, pal.soft);
    h
}

/// A 180 px column: laptop outline, name (600), caption (12 px), gap 10, all centred.
fn device_column(f: &Frame, x: f32, y: f32, name: &str, caption: &str, stroke: Color) {
    let p = f.p;
    f.laptop(x + (layout::PAIR_COLUMN_W - 64.0) / 2.0, y, 64.0, 2.5, stroke);
    let n = p.label_in(name, text::NAME, layout::PAIR_COLUMN_W, Align::Center);
    p.draw(&n, x, y + 64.0 + 10.0, f.pal.ink);
    let c = p.label_in(caption, text::SMALL, layout::PAIR_COLUMN_W, Align::Center);
    p.draw(&c, x, y + 64.0 + 10.0 + n.h + 10.0, f.pal.soft);
}

/// Two equal panels, gap 20, padding 20 22, gap 10 between the heading and the text.
fn info_panels(f: &mut Frame, a: &Area, y: f32, d: &DeviceView) {
    let (p, pal) = (f.p, f.pal);
    let pw = (a.cw - 20.0) / 2.0;
    let bw = p.hairline();
    let text_w = pw - 2.0 * bw - 44.0;
    let perms = f.st.permission_summary(d.grants_here);
    let last = f.st.last_session_text(d);
    let perms_l = p.label_in(&perms, text::BODY, text_w, Align::Start);
    let last_l = p.label_in(&last, text::BODY, text_w, Align::Start);
    let edit_style = text::NAME;
    let edit_text = f.t(MsgKey::PermsPanelEdit);
    let edit = p.label(&edit_text, edit_style);
    let heading_h = p.label("A", text::H2).h.max(edit.h);
    let h = 2.0 * bw + 40.0 + heading_h + 10.0 + perms_l.h.max(last_l.h);

    let first = Rect::new(a.x, y, pw, h);
    let second = Rect::new(a.x + pw + 20.0, y, pw, h);
    f.panel(first, pal.surface);
    f.panel(second, pal.surface);

    let heading = p.label_ellipsis(
        &f.st.tm(&Msg::new(MsgKey::PermsPanelTitle).with(d.name.clone())),
        text::H2,
        text_w - edit.w - 12.0,
    );
    p.draw(&heading, first.x + bw + 22.0, first.y + bw + 20.0, pal.ink);
    p.draw(&perms_l, first.x + bw + 22.0, first.y + bw + 20.0 + heading_h + 10.0, pal.soft);

    // "Editar" is an inline link: its focus box is the font's content area, not the line box.
    let edit_x = first.right() - bw - 22.0 - edit.w;
    let edit_y = first.y + bw + 20.0 + (heading_h - edit.h) / 2.0;
    p.draw(&edit, edit_x, edit_y, pal.link);
    let content = p.fonts.content_height(edit_style);
    let edit_box = Rect::new(edit_x, edit_y + (edit.h - content) / 2.0, edit.w, content);
    if f.hovered(ControlId::EditGrants) {
        p.fill_rect(Rect::new(edit_x, edit_box.bottom() - 1.5, edit.w, p.hairline()), pal.link);
    }
    f.register(ControlId::EditGrants, edit_box, 0.0);

    let title2 = p.label(&f.t(MsgKey::LastSessionTitle), text::H2);
    p.draw(&title2, second.x + bw + 22.0, second.y + bw + 20.0, pal.ink);
    p.draw(&last_l, second.x + bw + 22.0, second.y + bw + 20.0 + heading_h + 10.0, pal.soft);
}

/// The Permissões tab: what the other machine may do here (switches), what this one may do there (read only).
fn permissions(f: &mut Frame, a: &Area, y: f32, d: &DeviceView) {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let colw = (a.cw - 20.0) / 2.0;
    let h2 = p.label("A", text::H2).h;
    let (lx, rx) = (a.x, a.x + colw + 20.0);

    // Left: h2, gap 10, panel of switch rows (min-height 50, padding 0 20, border-top).
    let title = p.label_ellipsis(&f.st.tm(&Msg::new(MsgKey::PermsHereTitle).with(d.name.clone())), text::H2, colw);
    p.draw(&title, lx, y, pal.ink);
    let rows = f.st.permission_rows();
    let panel_top = y + h2 + 10.0;
    let panel_h = 2.0 * bw + rows.len() as f32 * 50.0;
    f.panel(Rect::new(lx, panel_top, colw, panel_h), pal.surface);
    let mut ry = panel_top + bw;
    for (n, (perm, label, hint)) in rows.iter().enumerate() {
        if n > 0 {
            p.fill_rect(Rect::new(lx + bw, ry, colw - 2.0 * bw, bw), pal.line);
        }
        let l = p.label(label, text::BODY);
        let hint_l = hint.as_ref().map(|h| p.label(h, text::SMALL));
        let block_h = l.h + hint_l.as_ref().map_or(0.0, |h| h.h);
        let ty = ry + (50.0 - block_h) / 2.0;
        p.draw(&l, lx + bw + 20.0, ty, pal.ink);
        if let Some(h) = &hint_l {
            p.draw(h, lx + bw + 20.0, ty + l.h, pal.soft);
        }
        let on = d.grants_here.has(*perm);
        let waiting = f.st.grant_waiting(*perm);
        f.switch(
            ControlId::Grant(*perm),
            lx + colw - bw - 20.0 - layout::SWITCH_W,
            ry + (50.0 - layout::SWITCH_H) / 2.0,
            on,
            waiting,
        );
        ry += 50.0;
    }

    // Right: the other machine's answers, on a sunken panel; then the note and "Esquecer".
    let title = p.label_ellipsis(&f.st.tm(&Msg::new(MsgKey::PermsThereTitle).with(d.name.clone())), text::H2, colw);
    p.draw(&title, rx, y, pal.ink);
    let answers = f.st.there_rows(d);
    let there_h = 2.0 * bw + answers.len() as f32 * 50.0;
    f.panel(Rect::new(rx, panel_top, colw, there_h), pal.sunken);
    let mut ry = panel_top + bw;
    let yes = f.t(MsgKey::PermYes);
    for (n, (label, answer)) in answers.iter().enumerate() {
        if n > 0 {
            p.fill_rect(Rect::new(rx + bw, ry, colw - 2.0 * bw, bw), pal.line);
        }
        let l = p.label(label, text::BODY);
        p.draw(&l, rx + bw + 20.0, ry + (50.0 - l.h) / 2.0, pal.ink);
        let (style, color) = if *answer == yes { (text::NAME, pal.ok) } else { (text::BODY, pal.soft) };
        let al = p.label(answer, style);
        p.draw(&al, rx + colw - bw - 20.0 - al.w, ry + (50.0 - al.h) / 2.0, color);
        ry += 50.0;
    }
    let note_y = panel_top + there_h + 10.0;
    let note = f.st.tm(&Msg::new(MsgKey::ThereNote).with(d.name.clone()));
    let nh = f.para(&note, text::CAPTION, rx, note_y, colw, pal.soft, Align::Start);
    let forget = f.t(MsgKey::ForgetDevice);
    let fw = f.button_width(&forget, Kind::Danger);
    f.button(ControlId::Forget, Rect::new(rx, note_y + nh + 10.0 + 8.0, fw, 40.0), Kind::Danger, &forget, false);
}

fn history(f: &mut Frame, a: &Area, y: f32, d: &DeviceView) {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let tw = a.cw - 44.0 - 2.0 * bw;
    let title = f.st.tm(&Msg::new(MsgKey::HistoryTitle).with(d.name.clone()));
    let body = f.t(MsgKey::HistoryPlaceholder);
    let h2 = p.label(&title, text::H2);
    let th = f.para_height(&body, text::BODY, tw);
    f.panel(Rect::new(a.x, y, a.cw, 2.0 * bw + 40.0 + h2.h + 10.0 + th), pal.surface);
    p.draw(&h2, a.x + bw + 22.0, y + bw + 20.0, pal.ink);
    f.para(&body, text::BODY, a.x + bw + 22.0, y + bw + 20.0 + h2.h + 10.0, tw, pal.soft, Align::Start);
}

// ---------------------------------------------------------------------------------------------------------------
// Notices and states

fn notice_icon(kind: NoticeKind) -> String {
    match kind {
        NoticeKind::Public | NoticeKind::Firewall => icons::WIFI.to_owned(),
        NoticeKind::Service => icons::lock(),
        NoticeKind::MacPerms => icons::SHIELD.to_owned(),
        NoticeKind::Crash => icons::SHIELD_ALERT.to_owned(),
    }
}

/// `.msg` cards (padding 20 22, gap 10, icon 40, h2 17/1.3, a primary button at the bottom), up to three per row.
/// Returns the height the cards take.
fn notice_cards(f: &mut Frame, a: &Area, y: f32, kinds: &[NoticeKind]) -> f32 {
    let pal = f.pal;
    let per_row = kinds.len().clamp(1, 3);
    let gap = 20.0;
    let w = (a.cw - gap * (per_row as f32 - 1.0)) / per_row as f32;
    let mut total = 0.0f32;
    for (n, kind) in kinds.iter().enumerate() {
        let (col, row) = (n % 3, n / 3);
        let (title, body, button) = f.st.notice_texts(*kind);
        let x = a.x + col as f32 * (w + gap);
        let tinted = if matches!(kind, NoticeKind::MacPerms) {
            (pal.link_tint, pal.link)
        } else {
            (pal.danger_tint, pal.danger)
        };
        // Every card of a row is laid out by its own text, as before; rows stack by the last height.
        let h = state_card_height(f, w, &title, &body);
        let top = y + row as f32 * (h + gap);
        state_card(f, (x, top, w), &notice_icon(*kind), tinted, (&title, &body, &button), ControlId::Notice(*kind));
        total = total.max(row as f32 * (h + gap) + h);
    }
    total
}

fn state_card_height(f: &Frame, w: f32, title: &str, body: &str) -> f32 {
    let p = f.p;
    let bw = p.hairline();
    let inner = w - 44.0 - 2.0 * bw;
    let t = p.label_in(title, text::H2_STATE, inner, Align::Start);
    let b = p.label_in(body, text::BODY, inner, Align::Start);
    2.0 * bw + 40.0 + 40.0 + 10.0 + t.h + 10.0 + b.h + 10.0 + 40.0
}

/// One card of a state: icon circle, title, body and a primary button; returns its height.
fn state_card(
    f: &mut Frame,
    at: (f32, f32, f32),
    icon: &str,
    tint: (Color, Color),
    texts: (&str, &str, &str),
    id: ControlId,
) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let (title, body, button) = texts;
    let (x, top, w) = at;
    let bw = p.hairline();
    let inner = w - 44.0 - 2.0 * bw;
    let t = p.label_in(title, text::H2_STATE, inner, Align::Start);
    let b = p.label_in(body, text::BODY, inner, Align::Start);
    let h = state_card_height(f, w, title, body);
    f.panel(Rect::new(x, top, w, h), pal.surface);
    let (ix, mut iy) = (x + bw + 22.0, top + bw + 20.0);
    f.icon_circle(ix, iy, tint.0, tint.1, icon);
    iy += 40.0 + 10.0;
    p.draw(&t, ix, iy, pal.ink);
    iy += t.h + 10.0;
    p.draw(&b, ix, iy, pal.soft);
    let bwid = f.button_width(button, Kind::Primary);
    f.button(id, Rect::new(ix, top + h - bw - 20.0 - 40.0, bwid, 40.0), Kind::Primary, button, false);
    h
}

/// "Acordando o Desktop": a card with a progress bar and a cancel button, in place of the device's content.
fn waking_card(f: &mut Frame, a: &Area, y: f32, d: &DeviceView, state: &SessionState) {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let w = (a.cw - 40.0) / 3.0;
    let inner = w - 44.0 - 2.0 * bw;
    let title = f.st.tm(&Msg::new(MsgKey::WakingTitle).with(d.name.clone()));
    let body = f.t(MsgKey::WakingBody);
    let t = p.label_in(&title, text::H2_STATE, inner, Align::Start);
    let b = p.label_in(&body, text::BODY, inner, Align::Start);
    let h = 2.0 * bw + 40.0 + 40.0 + 10.0 + t.h + 10.0 + b.h + 10.0 + 6.0 + 10.0 + 40.0;
    f.panel(Rect::new(a.x, y, w, h), pal.surface);
    let (ix, mut iy) = (a.x + bw + 22.0, y + bw + 20.0);
    f.icon_circle(ix, iy, pal.link_tint, pal.link, icons::POWER);
    iy += 50.0;
    p.draw(&t, ix, iy, pal.ink);
    iy += t.h + 10.0;
    p.draw(&b, ix, iy, pal.soft);
    iy += b.h + 10.0;
    let remaining = if let SessionState::Waking { remaining_s } = state { *remaining_s } else { 60 };
    let done = (60u16.saturating_sub(remaining) as f32 / 60.0).clamp(0.0, 1.0);
    p.fill_rrect(Rect::new(ix, iy, inner, 6.0), 3.0, pal.line);
    p.fill_rrect(Rect::new(ix, iy, inner * done, 6.0), 3.0, pal.link);
    let cancel = f.t(MsgKey::Cancel);
    let cw = f.button_width(&cancel, Kind::Default);
    f.button(ControlId::WakeCancel, Rect::new(ix, y + h - bw - 20.0 - 40.0, cw, 40.0), Kind::Default, &cancel, false);
}

/// The error strip: floats over the bottom of the content so nothing moves when it appears.
fn error_toast(f: &mut Frame) {
    let Some(e) = f.st.error() else { return };
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let msg = f.st.error_text(e);
    let close = f.t(MsgKey::Close);
    let cw = f.button_width(&close, Kind::Danger);
    let l = p.label_in(&msg, text::BODY, a.cw - 32.0 - 20.0 - 14.0 - 14.0 - cw, Align::Start);
    let h = (14.0 * 2.0 + l.h.max(20.0).max(40.0)).max(68.0);
    let r = Rect::new(a.x, f.h - 24.0 - h, a.cw, h);
    p.shadow(r, 12.0, 6.0, 20.0, Color::rgba(0, 0, 0, 25));
    p.fill_rrect(r, 12.0, pal.danger_tint);
    p.svg(icons::SHIELD_ALERT, r.x + 16.0, r.y + (h - 20.0) / 2.0, 20.0, 2.0, pal.danger);
    p.draw(&l, r.x + 16.0 + 20.0 + 14.0, r.y + (h - l.h) / 2.0, pal.ink);
    f.button(
        ControlId::ErrorClose,
        Rect::new(r.right() - 16.0 - cw, r.y + (h - 40.0) / 2.0, cw, 40.0),
        Kind::Danger,
        &close,
        false,
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Atividade

fn activity_main(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let mut y = 32.0;
    // Header: title block grows, the export button sits at the end; main gap is 24 on this screen.
    let title = p.label(&f.t(MsgKey::ActivityTitle), text::H1);
    let sub = p.label_in(&f.t(MsgKey::ActivitySub), text::BODY, a.cw - 200.0, Align::Start);
    p.draw(&title, a.x, y, pal.ink);
    p.draw(&sub, a.x, y + title.h + 6.0, pal.soft);
    let export = f.t(MsgKey::ExportLog);
    let ew = f.button_width(&export, Kind::Default);
    f.button(ControlId::Export, Rect::new(a.x + a.cw - ew, y, ew, 40.0), Kind::Default, &export, false);
    y += (title.h + 6.0 + sub.h).max(40.0) + 24.0;

    if let Some(alert) = f.st.alert().cloned() {
        let addr = alert.what.args.first().cloned().unwrap_or_default();
        let block = f.st.tm(&Msg::new(MsgKey::BlockAddress).with(addr));
        let when = format::when_phrase(f.st.language(), alert.at_ms, f.st.clock()).to_lowercase();
        let first = f.st.tm(&Msg::new(MsgKey::AlertUnknownPair).with(when));
        let rest = f.t(MsgKey::AlertUnknownRest);
        let bw_btn = f.button_width(&block, Kind::Danger);
        let tw = a.cw - 16.0 - 20.0 - 14.0 - 14.0 - bw_btn - 16.0;
        let full = format!("{first} {rest}");
        let l = p.label_rich(&full, text::BODY, tw, first.chars().count());
        let h = 14.0 * 2.0 + l.h.max(40.0);
        let r = Rect::new(a.x, y, a.cw, h);
        p.fill_rrect(r, 12.0, pal.danger_tint);
        p.svg(icons::SHIELD_ALERT, r.x + 16.0, r.y + (h - 20.0) / 2.0, 20.0, 2.0, pal.danger);
        p.draw(&l, r.x + 16.0 + 20.0 + 14.0, r.y + (h - l.h) / 2.0, pal.ink);
        f.button(
            ControlId::BlockAlert,
            Rect::new(r.right() - 16.0 - bw_btn, r.y + (h - 40.0) / 2.0, bw_btn, 40.0),
            Kind::Danger,
            &block,
            false,
        );
        y += h + 24.0;
    }

    // The log: header row 40, rows 50, drawn in pages (only what is in view, plus a margin).
    let bw = p.hairline();
    let rows = f.st.activity();
    let total = rows.len();
    let loading = f.st.placeholder(DataKind::Activity);
    let content_h = 2.0 * bw + 40.0 + if loading { 6.0 * 50.0 } else { total as f32 * 50.0 };
    let avail = (f.h - y - 32.0).max(120.0);
    let panel_h = content_h.min(avail);
    let panel = Rect::new(a.x, y, a.cw, panel_h);
    if f.st.fetch(DataKind::Activity) == swapdesk_ui::Fetch::Pending {
        return;
    }
    f.panel(panel, pal.surface);
    let inner_w = a.cw - 2.0 * bw;
    let cols = [130.0f32, 200.0, inner_w - 130.0 - 200.0 - 100.0, 100.0];
    let heads = [MsgKey::ColWhen, MsgKey::ColDevice, MsgKey::ColWhat, MsgKey::ColDuration];
    let mut cx = a.x + bw;
    for (c, key) in cols.iter().zip(heads) {
        let l = p.label_ellipsis(&f.t(key), text::TABLE_HEAD, c - 36.0);
        p.draw(&l, cx + 18.0, y + bw + (40.0 - l.h) / 2.0, pal.soft);
        cx += c;
    }
    p.fill_rect(Rect::new(a.x + bw, y + bw + 40.0 - bw, inner_w, bw), pal.line);
    let list = Rect::new(a.x + bw, y + bw + 40.0, inner_w, panel_h - 2.0 * bw - 40.0);
    p.push_clip(list);
    if loading {
        for n in 0..6 {
            let ry = list.y + n as f32 * 50.0;
            f.block(Rect::new(list.x + 18.0, ry + 18.0, 90.0, 14.0), 4.0);
            f.block(Rect::new(list.x + 130.0 + 18.0, ry + 18.0, 120.0, 14.0), 4.0);
            f.block(Rect::new(list.x + 330.0 + 18.0, ry + 18.0, (cols[2] - 120.0).max(60.0), 14.0), 4.0);
            if n < 5 {
                p.fill_rect(Rect::new(list.x, ry + 50.0 - bw, list.w, bw), pal.line);
            }
        }
    } else {
        f.out.scroll_max = max_scroll(total, 50.0, list.h);
        f.out.list_area = Some(list);
        let scroll = f.ix.scroll.clamp(0.0, f.out.scroll_max);
        for n in visible_rows(total, 50.0, scroll, list.h) {
            let row = &rows[n];
            let ry = list.y + n as f32 * 50.0 - scroll;
            let mut cx = list.x;
            let who = if row.alert { f.t(MsgKey::UnknownDevice) } else { row.device_name.clone() };
            let cells = [
                (format::table_when(f.st.language(), row.at_ms, f.st.clock()), pal.ink, text::BODY),
                (who, if row.alert { pal.danger } else { pal.ink }, if row.alert { text::NAME } else { text::BODY }),
                (f.st.tm(&row.what), pal.ink, text::BODY),
                (f.st.duration_cell(row), pal.soft, text::BODY),
            ];
            for ((s, color, style), c) in cells.into_iter().zip(cols) {
                // Two lines fit the 50 px row; longer text is trimmed.
                let l = p.label_in(&s, style, c - 36.0, Align::Start);
                p.draw(&l, cx + 18.0, ry + (50.0 - l.h) / 2.0, color);
                cx += c;
            }
            if n + 1 < total {
                p.fill_rect(Rect::new(list.x, ry + 50.0 - bw, list.w, bw), pal.line);
            }
        }
    }
    p.pop_clip();
}

// ---------------------------------------------------------------------------------------------------------------
// Ajustes

enum SettingRow {
    Edit { label: String, value: String, width: f32 },
    Select { id: SelectId, label: String, hint: Option<String>, value: String, width: f32 },
    Toggle { id: ControlId, label: String, on: bool },
    Status { label: String, hint: Option<String>, value: Option<(String, Color)> },
}

fn settings_main(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let y = 32.0;
    let title = p.label(&f.t(MsgKey::SettingsTitle), text::H1);
    p.draw(&title, a.x, y, pal.ink);
    let top = y + title.h + 28.0;
    let colw = (a.cw - 24.0) / 2.0;
    let (lx, rx) = (a.x, a.x + colw + 24.0);
    let Some(set) = f.st.settings().cloned() else {
        if f.st.placeholder(DataKind::Settings) {
            for (x, rows) in [(lx, [3usize, 3]), (rx, [2, 2])] {
                let mut gy = top;
                for n in rows {
                    f.block(Rect::new(x, gy + 2.0, 140.0, 15.0), 4.0);
                    let h = 2.0 + n as f32 * 52.0;
                    f.block(Rect::new(x, gy + 21.75 + 10.0, colw, h), 12.0);
                    gy += 21.75 + 10.0 + h + 24.0;
                }
            }
        }
        return;
    };
    let net = f.st.network().cloned();
    let net_row = match &net {
        Some(n) => match &n.address {
            Some(addr) => f.st.tm(&Msg::new(MsgKey::NetworkRow).with(n.label.clone()).with(addr.clone())),
            None => f.st.tm(&Msg::new(MsgKey::NetworkRowNoAddress).with(n.label.clone())),
        },
        None => f.t(MsgKey::NoNetwork),
    };
    let service = if set.service_enabled {
        (f.t(MsgKey::ServiceActive), pal.ok)
    } else {
        (f.t(MsgKey::ServiceInactive), pal.soft)
    };
    let left = [
        (
            MsgKey::SidebarThisComputer,
            vec![
                SettingRow::Edit { label: f.t(MsgKey::FieldName), value: set.name.clone(), width: 190.0 },
                SettingRow::Select {
                    id: SelectId::Language,
                    label: f.t(MsgKey::FieldLanguage),
                    hint: None,
                    value: f.st.select_value(SelectId::Language),
                    width: 190.0,
                },
                SettingRow::Toggle {
                    id: ControlId::OpenAtLogin,
                    label: f.t(MsgKey::OpenAtLogin),
                    on: set.open_at_login,
                },
            ],
        ),
        (
            MsgKey::GroupUnattended,
            vec![
                SettingRow::Status {
                    label: f.t(MsgKey::ServiceName),
                    hint: Some(f.t(MsgKey::ServiceHint)),
                    value: Some(service),
                },
                SettingRow::Toggle { id: ControlId::LockOnEnd, label: f.t(MsgKey::LockOnEnd), on: set.lock_on_end },
                SettingRow::Toggle { id: ControlId::KeepAwake, label: f.t(MsgKey::KeepAwake), on: set.keep_awake },
            ],
        ),
    ];
    let right = [
        (
            MsgKey::GroupImage,
            vec![
                SettingRow::Select {
                    id: SelectId::Quality,
                    label: f.t(MsgKey::Quality),
                    hint: None,
                    value: f.st.select_value(SelectId::Quality),
                    width: 170.0,
                },
                SettingRow::Toggle { id: ControlId::ShowStats, label: f.t(MsgKey::ShowStats), on: set.show_stats },
            ],
        ),
        (
            MsgKey::GroupKeyboard,
            vec![
                SettingRow::Select {
                    id: SelectId::Keyboard,
                    label: f.t(MsgKey::SendKeys),
                    hint: Some(f.t(MsgKey::SendKeysHint)),
                    value: f.st.select_value(SelectId::Keyboard),
                    width: 150.0,
                },
                SettingRow::Toggle {
                    id: ControlId::SwapCtrlCmd,
                    label: f.t(MsgKey::SwapCtrlCmd),
                    on: set.swap_ctrl_cmd,
                },
                SettingRow::Select {
                    id: SelectId::ReleaseKey,
                    label: f.t(MsgKey::ReleaseKeyLabel),
                    hint: Some(f.t(MsgKey::ReleaseKeyHint)),
                    value: f.st.select_value(SelectId::ReleaseKey),
                    width: 150.0,
                },
                SettingRow::Select {
                    id: SelectId::FullscreenMonitor,
                    label: f.t(MsgKey::FullscreenMonitorLabel),
                    hint: None,
                    value: f.st.select_value(SelectId::FullscreenMonitor),
                    width: 190.0,
                },
                // D30: every viewer shortcut, one per line, and the note that they hold with the capture on.
                SettingRow::Status {
                    label: f.t(MsgKey::GroupShortcuts),
                    hint: Some(
                        f.st.shortcut_rows()
                            .into_iter()
                            .map(|(what, keys)| format!("{what}: {keys}"))
                            .chain(std::iter::once(f.t(MsgKey::ShortcutsKeepHint)))
                            .collect::<Vec<_>>()
                            .join(
                                "
",
                            ),
                    ),
                    value: None,
                },
            ],
        ),
        (
            MsgKey::GroupNetwork,
            vec![SettingRow::Status { label: net_row, hint: Some(f.t(MsgKey::NetworkHint)), value: None }],
        ),
    ];
    let mut gy = top;
    for (title, rows) in left {
        gy += settings_group(f, lx, gy, colw, &f.t(title), &rows) + 24.0;
    }
    let mut gy = top;
    for (title, rows) in right {
        gy += settings_group(f, rx, gy, colw, &f.t(title), &rows) + 24.0;
    }
}

/// `.group`: h2, gap 10, a panel of rows (min-height 52, padding 0 18, border-top). Returns the height.
fn settings_group(f: &mut Frame, x: f32, y: f32, w: f32, title: &str, rows: &[SettingRow]) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let h2 = p.label(title, text::H2);
    p.draw(&h2, x, y, pal.ink);
    let top = y + h2.h + 10.0;
    // Rows are at least 52 px and grow with their label and hint (a two-line hint makes a taller row).
    let heights: Vec<f32> = rows
        .iter()
        .map(|row| {
            let (label, hint, control_w) = row_parts(p, row);
            let text_w = w - 2.0 * bw - 36.0 - control_w - 16.0;
            let l = p.label_in(label, text::BODY, text_w, Align::Start).h;
            let h = hint.map_or(0.0, |h| p.label_in(h, text::SMALL, text_w, Align::Start).h);
            (l + h).max(52.0)
        })
        .collect();
    let panel_h = 2.0 * bw + heights.iter().sum::<f32>();
    f.panel(Rect::new(x, top, w, panel_h), pal.surface);
    let mut ry = top + bw;
    for (n, row) in rows.iter().enumerate() {
        let row_h = heights[n];
        if n > 0 {
            p.fill_rect(Rect::new(x + bw, ry, w - 2.0 * bw, bw), pal.line);
        }
        let (label, hint, control_w) = row_parts(p, row);
        let lx = x + bw + 18.0;
        let text_w = w - 2.0 * bw - 36.0 - control_w - 16.0;
        let l = p.label_in(label, text::BODY, text_w, Align::Start);
        let hl = hint.map(|h| p.label_in(h, text::SMALL, text_w, Align::Start));
        let block_h = l.h + hl.as_ref().map_or(0.0, |h| h.h);
        let ty = ry + (row_h - block_h).max(0.0) / 2.0;
        p.draw(&l, lx, ty, pal.ink);
        if let Some(h) = &hl {
            p.draw(h, lx, ty + l.h, pal.soft);
        }
        let cx = x + w - bw - 18.0 - control_w;
        match row {
            SettingRow::Edit { value, width, .. } => {
                f.edit_field(
                    EditId::Name,
                    Rect::new(cx, ry + (row_h - layout::FIELD_H) / 2.0, *width, layout::FIELD_H),
                    value,
                    "",
                );
            }
            SettingRow::Select { id, value, width, .. } => {
                f.select_field(
                    ControlId::Select(*id),
                    Rect::new(cx, ry + (row_h - layout::FIELD_H) / 2.0, *width, layout::FIELD_H),
                    value,
                );
            }
            SettingRow::Toggle { id, on, .. } => f.switch(*id, cx, ry + (row_h - layout::SWITCH_H) / 2.0, *on, false),
            SettingRow::Status { value: Some((s, c)), .. } => {
                let sl = p.label(s, text::NAME);
                p.draw(&sl, cx, ry + (row_h - sl.h) / 2.0, *c);
            }
            SettingRow::Status { .. } => {}
        }
        ry += row_h;
    }
    h2.h + 10.0 + panel_h
}

/// Label, hint and the width of the control at the row's end.
fn row_parts<'r>(p: &crate::gfx::Painter, row: &'r SettingRow) -> (&'r str, Option<&'r str>, f32) {
    match row {
        SettingRow::Edit { label, width, .. } => (label.as_str(), None, *width),
        SettingRow::Select { label, hint, width, .. } => (label.as_str(), hint.as_deref(), *width),
        SettingRow::Toggle { label, .. } => (label.as_str(), None, layout::SWITCH_W),
        SettingRow::Status { label, hint, value, .. } => {
            (label.as_str(), hint.as_deref(), value.as_ref().map_or(0.0, |(s, _)| p.label(s, text::NAME).w))
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Pareamento

fn pairing(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let Some(view) = f.st.pairing().cloned() else { return };
    let bw = p.hairline();
    p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), pal.surface);
    // The bar: 40 px, padding 0 16, "Swapdesk no" soft (500) then the name (600), on the sunken colour.
    p.fill_rect(Rect::new(0.0, 0.0, f.w, 40.0), pal.sunken);
    p.fill_rect(Rect::new(0.0, 40.0 - bw, f.w, bw), pal.line);
    let prefix = p.label(&f.t(MsgKey::PairingBarPrefix), swapdesk_ui::tokens::TextStyle::new(13.0, 500));
    p.draw(&prefix, 16.0, (40.0 - prefix.h) / 2.0, pal.soft);
    let name = p.label(&this_name(f), text::BAR);
    p.draw(&name, 16.0 + prefix.w + 8.0, (40.0 - name.h) / 2.0, pal.ink);

    // The body: padding 36 40, gap 20.
    let x = 40.0;
    let mut y = 40.0 + 36.0;
    let maxw = 400.0f32.min(f.w - 80.0);
    let buttons_y = f.h - 36.0 - 40.0;
    let small = text::CAPTION;
    match &view.state {
        PairingState::ShowCode { code, peer_name } => {
            y += f.text(&f.t(MsgKey::PairingModeFixed), small, x, y, pal.soft) + 20.0;
            y += f.text(&f.st.tm(&Msg::new(MsgKey::PairTitleWith).with(peer_name.clone())), text::H1, x, y, pal.ink)
                + 20.0;
            y += f.para(
                &f.st.tm(&Msg::new(MsgKey::PairCompare).with(peer_name.clone())),
                text::BODY,
                x,
                y,
                maxw,
                pal.soft,
                Align::Start,
            ) + 20.0;
            y += 12.0;
            y += f.text(code, text::PAIR_CODE, x, y, pal.link) + 12.0 + 20.0;
            f.para(
                &f.st.tm(&Msg::new(MsgKey::PairWarn).with(peer_name.clone())),
                small,
                x,
                y,
                maxw,
                pal.soft,
                Align::Start,
            );
            let same = f.t(MsgKey::PairSame);
            let diff = f.t(MsgKey::PairDifferent);
            let sw = f.button_width(&same, Kind::Primary);
            f.button(ControlId::PairSame, Rect::new(x, buttons_y, sw, 40.0), Kind::Primary, &same, false);
            let dw = f.button_width(&diff, Kind::Default);
            f.button(
                ControlId::PairDifferent,
                Rect::new(x + sw + 10.0, buttons_y, dw, 40.0),
                Kind::Default,
                &diff,
                false,
            );
        }
        PairingState::Open { remaining_s } => {
            let caption = f.st.tm(&Msg::new(MsgKey::PairingModeCountdown).with(format::countdown(*remaining_s)));
            y += f.text(&caption, small, x, y, pal.soft) + 20.0;
            y += f.text(&f.t(MsgKey::PairThisComputer), text::H1, x, y, pal.ink) + 20.0;
            y += f.para(&f.t(MsgKey::PairOpenBody), text::BODY, x, y, maxw, pal.soft, Align::Start) + 20.0;
            // The 2-minute bar: what is left of the window.
            let frac = (f32::from(*remaining_s) / f32::from(layout::PAIR_COUNTDOWN_S)).clamp(0.0, 1.0);
            p.fill_rrect(Rect::new(x, y, maxw, 6.0), 3.0, pal.line);
            p.fill_rrect(Rect::new(x, y, maxw * frac, 6.0), 3.0, pal.link);
            y += 6.0 + 20.0;
            p.arc_stroke((x + 10.0, y + 10.0), 8.0, -90.0, 270.0, 2.5, pal.link);
            f.text(&f.t(MsgKey::PairSearching), text::BODY, x + 28.0, y, pal.soft);
            pair_cancel(f, x, buttons_y);
        }
        PairingState::Waiting => {
            y += f.text(&f.t(MsgKey::PairingModeFixed), small, x, y, pal.soft) + 20.0;
            y += f.text(&f.t(MsgKey::PairWaitingTitle), text::H1, x, y, pal.ink) + 20.0;
            y += f.para(&f.t(MsgKey::PairWaitingBody), text::BODY, x, y, maxw, pal.soft, Align::Start) + 20.0;
            p.arc_stroke((x + 10.0, y + 10.0), 8.0, -90.0, 270.0, 2.5, pal.link);
            pair_cancel(f, x, buttons_y);
        }
        PairingState::Done { device_id } => {
            let who = f.st.device_by_id(device_id).map(|d| d.name.clone()).unwrap_or_default();
            y += f.text(&f.st.tm(&Msg::new(MsgKey::PairDoneTitle).with(who)), text::H1, x, y, pal.ink) + 20.0;
            f.para(&f.t(MsgKey::PairDoneBody), text::BODY, x, y, maxw, pal.soft, Align::Start);
            let done = f.t(MsgKey::PairDoneButton);
            let w = f.button_width(&done, Kind::Primary);
            f.button(ControlId::PairDone, Rect::new(x, buttons_y, w, 40.0), Kind::Primary, &done, false);
        }
        PairingState::Failed { reason } => {
            y += f.text(&f.t(MsgKey::PairFailedTitle), text::H1, x, y, pal.ink) + 20.0;
            let msg =
                format!("{} {}", f.st.error_text(&swapdesk_ui::ApiError::new(*reason)), f.t(MsgKey::PairFailedBody));
            f.para(&msg, text::BODY, x, y, maxw, pal.soft, Align::Start);
            let retry = f.t(MsgKey::PairRetry);
            let close = f.t(MsgKey::PairClose);
            let rw = f.button_width(&retry, Kind::Primary);
            f.button(ControlId::PairRetry, Rect::new(x, buttons_y, rw, 40.0), Kind::Primary, &retry, false);
            let cw = f.button_width(&close, Kind::Default);
            f.button(ControlId::PairClose, Rect::new(x + rw + 10.0, buttons_y, cw, 40.0), Kind::Default, &close, false);
        }
    }
    // Disabled look while "Os códigos são iguais" waits for the service.
    if f.st
        .controls(WindowKind::Main)
        .iter()
        .any(|c| c.disabled && matches!(c.id, ControlId::PairSame | ControlId::PairDifferent))
    {
        for w in f.out.widgets.clone() {
            if matches!(w.id, ControlId::PairSame | ControlId::PairDifferent) {
                p.fill_rrect(w.rect, layout::BUTTON_RADIUS, pal.surface.with_alpha(0.55));
            }
        }
    }
}

fn pair_cancel(f: &mut Frame, x: f32, y: f32) {
    let cancel = f.t(MsgKey::PairCancel);
    let w = f.button_width(&cancel, Kind::Default);
    f.button(ControlId::PairCancel, Rect::new(x, y, w, 40.0), Kind::Default, &cancel, false);
}

// ---------------------------------------------------------------------------------------------------------------
// Dialogs

/// The dialog card (padding 32, radius 14, shadow) centred over a backdrop; returns its rectangle given the content
/// height.
fn card(f: &Frame, content_h: f32) -> Rect {
    let w = layout::DIALOG_W.min(f.w - 48.0);
    let h = content_h + 64.0;
    let r = Rect::new(f.p.snap((f.w - w) / 2.0), f.p.snap(((f.h - h) / 2.0).max(16.0)), w, h);
    f.p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), f.pal.remote);
    f.p.shadow(r, layout::DIALOG_RADIUS, 30.0, 80.0, fixed::SHADOW_DIALOG);
    f.p.fill_rrect(r, layout::DIALOG_RADIUS, f.pal.surface);
    r
}

/// The incoming-session dialog (Pedido de conexão).
fn incoming(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let Some(i) = f.st.incoming().cloned() else { return };
    let w = layout::DIALOG_W.min(f.w - 48.0) - 64.0;
    let title_s = f.st.incoming_title(&i);
    let sub_s = f.st.tm(&Msg::new(MsgKey::IncomingSub).with(i.remaining_s.to_string()));
    let title = p.label_in(&title_s, text::H1_DIALOG, w, Align::Start);
    let sub = p.label_in(&sub_s, text::BODY, w, Align::Start);
    let legend = p.label(&f.t(MsgKey::AllowThisSession), text::NAME);
    let boxes = f.st.incoming_boxes();
    let rows = boxes.len().div_ceil(2);
    let grid_h = legend.h + 8.0 + rows as f32 * 32.0 + (rows as f32 - 1.0) * 4.0;
    let icon_h = 44.0;
    let content_h = icon_h + 24.0 + (title.h + 8.0 + sub.h) + 24.0 + grid_h + 24.0 + (16.0 + 1.0 + 32.0) + 24.0 + 40.0;
    let r = card(f, content_h);
    let (x, mut y) = (r.x + 32.0, r.y + 32.0);

    // The strip: laptop, wire, laptop (aria-hidden in the design).
    f.laptop(x, y, 44.0, 3.0, pal.link);
    f.laptop(x + w - 44.0, y, 44.0, 3.0, pal.ink);
    f.wire(x + 44.0 + 14.0, x + w - 44.0 - 14.0, y + 22.0, 3.0, 12.0, 14.0, pal.link);
    y += icon_h + 24.0;
    p.draw(&title, x, y, pal.ink);
    p.draw(&sub, x, y + title.h + 8.0, pal.soft);
    y += title.h + 8.0 + sub.h + 24.0;

    p.draw(&legend, x, y, pal.ink);
    y += legend.h + 8.0;
    let colw = (w - 16.0) / 2.0;
    for (n, (perm, key)) in boxes.iter().enumerate() {
        let (col, row) = (n % 2, n / 2);
        f.checkbox(
            ControlId::Allow(*perm),
            x + col as f32 * (colw + 16.0),
            y + row as f32 * 36.0,
            colw,
            i.allow.has(*perm),
            &f.t(*key),
        );
    }
    y += rows as f32 * 32.0 + (rows as f32 - 1.0) * 4.0 + 24.0;
    p.fill_rect(Rect::new(x, y, w, p.hairline()), pal.line);
    y += 1.0 + 16.0;
    f.checkbox(ControlId::RememberUnattended, x, y, w, i.remember_unattended, &f.t(MsgKey::RememberUnattended));
    y += 32.0 + 24.0;

    // Buttons: "Recusar" quiet at the start (no left padding), the other two at the end.
    let decline = f.t(MsgKey::Decline);
    let view = f.t(MsgKey::ViewOnlyAccept);
    let accept = f.t(MsgKey::Accept);
    let aw = f.button_width(&accept, Kind::Primary);
    let vw = f.button_width(&view, Kind::Default);
    let dw = f.button_width(&decline, Kind::Quiet) - 16.0;
    f.button(ControlId::Accept, Rect::new(x + w - aw, y, aw, 40.0), Kind::Primary, &accept, false);
    f.button(ControlId::ViewOnlyAccept, Rect::new(x + w - aw - 10.0 - vw, y, vw, 40.0), Kind::Default, &view, false);
    // Quiet with padding-left 0: draw the label at the card's edge.
    let drect = Rect::new(x - 16.0 - p.hairline(), y, dw + 16.0 + 16.0 + 2.0 * p.hairline(), 40.0);
    f.button(ControlId::Decline, drect, Kind::Quiet, &decline, true);
}

/// Consent card, elevation step and swap dialogs: an icon, a title, text, an optional line and two buttons.
fn dialog(f: &mut Frame, kind: &DialogKind) {
    let (p, pal) = (f.p, f.pal);
    let t = f.st.dialog_texts(kind);
    let w = layout::DIALOG_W.min(f.w - 48.0) - 64.0;
    let title = p.label_in(&t.title, text::H1_DIALOG, w, Align::Start);
    let bodies: Vec<_> = t.body.iter().map(|b| p.label_in(b, text::BODY, w, Align::Start)).collect();
    let extra = t.extra.as_ref().map(|e| p.label_in(e, text::CAPTION, w, Align::Start));
    let mut content_h = 44.0 + 20.0 + title.h;
    for b in &bodies {
        content_h += 12.0 + b.h;
    }
    if let Some(e) = &extra {
        content_h += 16.0 + e.h;
    }
    content_h += 28.0 + 40.0;
    let r = card(f, content_h);
    let (x, mut y) = (r.x + 32.0, r.y + 32.0);
    let icon = match kind {
        DialogKind::Elevation => icons::SHIELD.to_owned(),
        DialogKind::Consent => icons::terminal(),
        DialogKind::SwapConfirm | DialogKind::SwapAsk => icons::SWAP.to_owned(),
    };
    p.fill_circle((x + 22.0, y + 22.0), 22.0, pal.link_tint);
    p.svg(&icon, x + 11.0, y + 11.0, 22.0, 2.0, pal.link);
    y += 44.0 + 20.0;
    p.draw(&title, x, y, pal.ink);
    y += title.h;
    for b in &bodies {
        y += 12.0;
        p.draw(b, x, y, pal.soft);
        y += b.h;
    }
    if let Some(e) = &extra {
        y += 16.0;
        if matches!(kind, DialogKind::Elevation) {
            p.arc_stroke((x + 10.0, y + e.h / 2.0), 8.0, -90.0, 270.0, 2.5, pal.link);
            p.draw(e, x + 28.0, y, pal.ink);
        } else {
            p.draw(e, x, y, pal.ink);
        }
        y += e.h;
    }
    y += 28.0;
    // Secondary quiet at the start, primary at the end.
    if let Some(s) = &t.secondary {
        let sw = f.button_width(s, Kind::Quiet);
        f.button(ControlId::DialogSecondary, Rect::new(x - 16.0 - p.hairline(), y, sw, 40.0), Kind::Quiet, s, true);
    }
    if let Some(pm) = &t.primary {
        let pw = f.button_width(pm, Kind::Primary);
        f.button(ControlId::DialogPrimary, Rect::new(x + w - pw, y, pw, 40.0), Kind::Primary, pm, false);
    }
}
