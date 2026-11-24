//! The screens of the main window: sidebar, device detail (overview, permissions, history, notices, waking),
//! Atividade, Ajustes, Pareamento and the dialogs. Laid out in CSS pixels by hand, following the design's flex
//! boxes; the numbers in comments are the design's paddings and gaps.

use swapdesk_ui::format;
use swapdesk_ui::home::{HomeCard, RecentLine};
use swapdesk_ui::list::{max_scroll, visible_rows};
use swapdesk_ui::tokens::{Color, layout, text};
use swapdesk_ui::{
    ControlId, DataKind, DeviceStatus, DeviceTab, DeviceView, DialogKind, EditId, Msg, MsgKey, Nav, NoticeKind,
    PairingState, Screen, SelectId, WaitCard, WaitIcon, WindowKind,
};

use crate::card::{self, Card, Icon, Look};
use crate::painter::{Align, Rect};
use crate::view::{Frame, Kind, icons};

const SIDEBAR_W: f32 = layout::SIDEBAR_W;

pub fn paint(f: &mut Frame) {
    f.p.clear(f.pal.canvas);
    // The elevated instance's window is the confirmation step alone: nothing of the app is behind it.
    if f.st.elevated_confirm().is_some() {
        dialog(f, &DialogKind::ElevatedConfirm);
        return;
    }
    if f.st.screen() == Screen::Stopped {
        // The core did not start: its one card alone in the window (D51), with nothing of the app behind it.
        if f.compact {
            crate::screens_compact::stopped(f);
        } else {
            f.page_top = 40.0;
            f.page_floor = f.h - 40.0;
            stopped_page(f);
        }
    } else if f.st.screen() == Screen::Pairing {
        pairing(f);
    } else if f.compact {
        crate::screens_compact::paint(f);
    } else {
        sidebar(f);
        let _ = match f.st.nav() {
            Nav::Home => home_main(f),
            Nav::Devices => devices_main(f),
            Nav::Activity => activity_main(f),
            Nav::Settings => settings_main(f),
        };
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

    // padding 18 12; the rows are as wide as the sidebar less 2 x 12 and the 1 px border.
    let x = 12.0;
    let w = SIDEBAR_W - 24.0 - bw;
    let mut y = 18.0;

    // Logo: 22 px mark, gap 10, padding 0 10.
    let logo = p.label(&f.t(MsgKey::AppName), text::LOGO);
    logo_mark(f, x + 10.0, y + (logo.h - 22.0) / 2.0);
    p.draw(&logo, x + 10.0 + 22.0 + 10.0, y, pal.ink);
    y += logo.h + 22.0;

    // Início: the first item, selected while the home is shown.
    nav_row(
        f,
        ControlId::NavHome,
        Rect::new(x, y, w, 40.0),
        icons20::HOUSE,
        &f.t(MsgKey::NavHome),
        f.st.nav() == Nav::Home,
    );
    y += 40.0 + 22.0;

    // Ajustes hangs from the window's bottom edge; the lists are clipped above it.
    let settings_y = h - 18.0 - 40.0;
    let settings = f.t(MsgKey::NavSettings);
    nav_row(
        f,
        ControlId::NavSettings,
        Rect::new(x, settings_y, w, 40.0),
        icons20::GEAR,
        &settings,
        f.st.nav() == Nav::Settings,
    );

    // Paired and on the network.
    p.push_clip(Rect::new(0.0, y - 4.0, SIDEBAR_W - bw, (settings_y - 8.0 - (y - 4.0)).max(0.0)));
    let loading_devices = f.st.placeholder(DataKind::Devices);
    let (paired, network) = f.st.device_groups();
    let nav_devices = f.st.nav() == Nav::Devices;
    let selected_id = f.st.selected_device().map(|d| d.id.clone());
    let make = |i: usize| -> Row {
        let d = &f.st.devices()[i];
        Row {
            id: Some(ControlId::Device(i as u16)),
            name: d.name.clone(),
            status: f.st.device_status_line(d),
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
        y += group(f, x, y, w, &f.t(MsgKey::SidebarPaired), &paired_rows, if loading_devices { 2 } else { 0 });
        // D55: an account with no pairing on a computer a 1.0 install left data on says why its list is empty.
        if !loading_devices && paired_rows.is_empty() && f.st.this_device().is_some_and(|t| t.legacy_note) {
            let note = p.label_in(&f.t(MsgKey::SidebarLegacyNote), text::SMALL, w - 20.0, Align::Start);
            p.draw(&note, x + 10.0, y + 6.0, pal.soft);
            y += 6.0 + note.h;
        }
        y += 22.0;
        if loading_devices || !network_rows.is_empty() {
            y += group(f, x, y, w, &f.t(MsgKey::SidebarOnNetwork), &network_rows, if loading_devices { 1 } else { 0 })
                + 22.0;
        }
    }
    let _ = y;
    p.pop_clip();
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
pub(crate) fn logo_mark(f: &Frame, x: f32, y: f32) {
    let p = f.p;
    let (x, y) = (p.snap(x), p.snap(y));
    let s = 22.0 / 24.0;
    let sw = 2.0 * s;
    p.stroke_rrect(Rect::new(x + 2.5 * s, y + 6.0 * s, 8.0 * s, 12.0 * s), 2.0 * s, sw, f.pal.link);
    p.stroke_rrect(Rect::new(x + 13.5 * s, y + 6.0 * s, 8.0 * s, 12.0 * s), 2.0 * s, sw, f.pal.link);
    p.line((x + 10.5 * s, y + 12.0 * s), (x + 13.5 * s, y + 12.0 * s), sw, f.pal.link, true);
}

// ---------------------------------------------------------------------------------------------------------------
// Início (D35), after the board "22. Início"

/// Icons of the board, drawn in its 20-unit box (the painter's paths are 24-unit: see [`icon20`]).
pub(crate) mod icons20 {
    pub const HOUSE: &str = "M3 9l7-5.5L17 9v7.5a1 1 0 0 1-1 1h-3.5v-5h-5v5H4a1 1 0 0 1-1-1z";
    pub const GEAR: &str = "M7.5 10a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0-5 0M10 2.5v2M10 15.5v2M2.5 10h2M15.5 10h2M4.7 4.7l1.4 1.4M13.9 13.9l1.4 1.4M4.7 15.3l1.4-1.4M13.9 6.1l1.4-1.4";
    pub const WIFI: &str = "M2.5 8a11 11 0 0 1 15 0M5 10.75a7.25 7.25 0 0 1 10 0M7.5 13.5a3.5 3.5 0 0 1 5 0M10 16h.01";
    pub const DESKTOP: &str = "M4.25 2.75h11.5a.75.75 0 0 1 .75.75v8a.75.75 0 0 1-.75.75H4.25a.75.75 0 0 1-.75-.75v-8a.75.75 0 0 1 .75-.75zM5.5 4.75h9v5.5h-9zM3.5 12.25L1.5 16.5h17l-2-4.25M5.5 14.4h9";
    pub const LAPTOP: &str = "M4.75 3.5h10.5a1.25 1.25 0 0 1 1.25 1.25v7a1.25 1.25 0 0 1-1.25 1.25H4.75A1.25 1.25 0 0 1 3.5 11.75v-7A1.25 1.25 0 0 1 4.75 3.5zM1.75 13h16.5v.75a1.75 1.75 0 0 1-1.75 1.75h-13a1.75 1.75 0 0 1-1.75-1.75z";
    /// A phone upright, with the short line of its home indicator.
    pub const PHONE: &str = "M6.75 2.25h6.5a1.5 1.5 0 0 1 1.5 1.5v12.5a1.5 1.5 0 0 1-1.5 1.5h-6.5a1.5 1.5 0 0 1-1.5-1.5V3.75a1.5 1.5 0 0 1 1.5-1.5zM8.75 15h2.5";
}

/// Strokes a board icon (20-unit box, 1.75 stroke) at `(x, y)` as `px` DIPs.
pub(crate) fn icon20(f: &Frame, path: &str, x: f32, y: f32, px: f32, color: Color) {
    f.p.svg(path, x, y, px * 1.2, 1.75, color);
}

/// A sidebar navigation row (Início, Ajustes): icon, label, the device rows' selected look.
fn nav_row(f: &mut Frame, id: ControlId, r: Rect, icon: &str, label: &str, selected: bool) {
    let (p, pal) = (f.p, f.pal);
    if selected {
        let ring = p.hairline();
        p.fill_rrect(Rect::new(r.x - ring, r.y - ring, r.w + 2.0 * ring, r.h + 2.0 * ring), 8.0 + ring, pal.line);
        p.fill_rrect(r, 8.0, pal.surface);
    }
    f.tint(id, r, 8.0, pal.ink, 0.06, 0.11);
    icon20(f, icon, r.x + 10.0, r.y + (r.h - 20.0) / 2.0, 20.0, pal.ink);
    let l = p.label_ellipsis(label, if selected { text::NAME } else { text::BTN }, r.w - 10.0 - 20.0 - 12.0 - 10.0);
    p.draw(&l, r.x + 10.0 + 20.0 + 12.0, r.y + (r.h - l.h) / 2.0, pal.ink);
    f.register(id, r, 8.0);
}

/// The narrowest a card gets: two columns need two of them and the gap.
const CARD_MIN_W: f32 = 300.0;
const CARD_GAP: f32 = 20.0;
/// Padding of a card inside its 1 px border (the board: 22 above and below, 24 at the sides).
const CARD_PAD_X: f32 = 24.0;
const CARD_PAD_Y: f32 = 22.0;
/// The time column of the recent-activity list and its gap to the line.
const TIME_W: f32 = 84.0;
const TIME_GAP: f32 = 14.0;

fn home_main(f: &mut Frame) -> f32 {
    let a = area(f);
    let mut y = home_header(f, &a, f.page_top);
    // The crash notice, the "no computer found" card or the plain empty line.
    y += home_notices(f, &a, y);
    home_cards(f, &a, y)
}

/// The header of Início: this computer, its state and its network. Returns where the page goes on.
pub(crate) fn home_header(f: &mut Frame, a: &Area, mut y: f32) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let home = f.st.home();

    // Header: the 40 px computer icon, the name over the state line, the network pill at the right.
    let title_h = p.label("A", text::H1).h;
    let body_h = p.label("A", text::BODY).h;
    // Narrow content: the pill drops under the header instead of cutting its text.
    let below = a.cw < 700.0;
    let pill_max = if below { a.cw } else { a.cw * 0.38 };
    let pill = home.network.as_ref().map(|c| {
        let l = p.label_ellipsis(&c.text, text::CAPTION, (pill_max - 14.0 - 20.0 - 8.0 - 14.0).max(60.0));
        (l, c.warn)
    });
    let pill_w = pill.as_ref().map_or(0.0, |(l, _)| 14.0 + 20.0 + 8.0 + l.w + 14.0);
    let head_w = if pill_w > 0.0 && !below { (a.cw - pill_w - 16.0).max(120.0) } else { a.cw };
    let block_h = title_h + 4.0 + body_h;
    let head_h = block_h.max(40.0);
    if let Some(os) = home.os {
        let path = match os {
            swapdesk_ui::Os::MacOs => icons20::LAPTOP,
            swapdesk_ui::Os::Android => icons20::PHONE,
            swapdesk_ui::Os::Windows => icons20::DESKTOP,
        };
        icon20(f, path, a.x, y + (head_h - 40.0) / 2.0, 40.0, pal.ink);
    }
    let tx = a.x + 40.0 + 16.0;
    let ty = y + (head_h - block_h) / 2.0;
    match &home.name {
        Some(name) => {
            let l = p.label_ellipsis(name, text::H1, head_w - 56.0);
            p.draw(&l, tx, ty, pal.ink);
        }
        None if f.st.placeholder(DataKind::ThisDevice) => f.block(Rect::new(tx, ty + 2.0, 220.0, title_h - 4.0), 6.0),
        None => {}
    }
    match &home.status {
        Some(status) => {
            let dot = if home.ready { pal.ok } else { pal.danger };
            let sy = ty + title_h + 4.0;
            p.fill_circle((tx + 4.0, sy + body_h / 2.0), 4.0, dot);
            let l = p.label_ellipsis(status, text::BODY, head_w - 56.0 - 16.0);
            p.draw(&l, tx + 16.0, sy, pal.soft);
        }
        None if f.st.placeholder(DataKind::ThisDevice) => {
            f.block(Rect::new(tx, ty + title_h + 4.0 + 3.0, 180.0, body_h - 6.0), 4.0);
        }
        None => {}
    }
    let mut pill_y = y + (head_h - 36.0) / 2.0;
    let mut pill_x = a.x + a.cw - pill_w;
    if below {
        pill_y = y + head_h + 16.0;
        pill_x = a.x;
    }
    if let Some((l, warn)) = &pill {
        let r = Rect::new(pill_x, pill_y, pill_w, 36.0);
        let (fill, ink) = if *warn { (pal.danger_tint, pal.danger) } else { (pal.surface, pal.soft) };
        p.fill_rrect(r, 18.0, fill);
        icon20(f, icons20::WIFI, r.x + 14.0, r.y + 8.0, 20.0, ink);
        p.draw(l, r.x + 14.0 + 20.0 + 8.0, r.y + (r.h - l.h) / 2.0, ink);
    } else if f.st.placeholder(DataKind::Network) {
        f.block(Rect::new(pill_x.min(a.x + a.cw - 200.0), pill_y, 200.0, 36.0), 18.0);
    }
    if below && (pill.is_some() || f.st.placeholder(DataKind::Network)) {
        y += 36.0 + 16.0;
    }
    y + head_h + 28.0
}

/// The cards of Início from `y` down. Returns where they end.
pub(crate) fn home_cards(f: &mut Frame, a: &Area, mut y: f32) -> f32 {
    let home = f.st.home();
    // The cards, as a list: two columns when the content is wide enough, one otherwise; each as tall as it needs.
    let cols = if a.cw >= 2.0 * CARD_MIN_W + CARD_GAP { 2 } else { 1 };
    let card_w = (a.cw - CARD_GAP * (cols as f32 - 1.0)) / cols as f32;
    let mut i = 0;
    while i < home.cards.len() {
        let row = &home.cards[i..(i + cols).min(home.cards.len())];
        // What is left below this row: the activity card shows as many lines as fit. A compact page scrolls, so
        // its card shows the newest few (about five).
        let avail = if f.compact { recent_fixed(f) + 320.0 } else { (f.h - y - 32.0).max(0.0) };
        let mut row_h = 0.0f32;
        for (n, card) in row.iter().enumerate() {
            let h = card_height(f, card, card_w, avail);
            let r = Rect::new(a.x + n as f32 * (card_w + CARD_GAP), y, card_w, h);
            paint_card(f, card, r, avail);
            row_h = row_h.max(h);
        }
        y += row_h + CARD_GAP;
        i += cols;
    }
    y - CARD_GAP
}

/// The update card, the crash notice, the "no computer found" card or the empty line. Each is one card across the
/// page among the page's other cards, with its content centred (D51). Returns the height used, with the gap after it.
pub(crate) fn home_notices(f: &mut Frame, a: &Area, y: f32) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let mut used = 0.0;
    // A phone that may not reach the local network: nothing below works without it, so it says so first.
    if f.st.notices().first() == Some(&NoticeKind::LocalNetwork) {
        let at = Rect::new(a.x, y + used, a.cw, 0.0);
        used += notice_card(f, Look::Inline, NoticeKind::LocalNetwork, at).h + 20.0;
    }
    // UPDATES.md 10.1: the update card comes first.
    if let Some(u) = f.st.update_card() {
        use swapdesk_ui::update_card::UpdIcon;
        let icon = match u.icon {
            UpdIcon::New => Icon::Circle(pal.link_tint, pal.link, icons::DOWNLOAD),
            UpdIcon::Wait => Icon::Spinner,
            UpdIcon::Done => Icon::Circle(pal.link_tint, pal.link, icons::CHECK),
            UpdIcon::Warn => Icon::Circle(pal.danger_tint, pal.danger, icons::SHIELD_ALERT),
        };
        let body = match (&u.body, &u.note) {
            (Some(b), Some(n)) => Some(format!("{b}\n{n}")),
            (Some(b), None) => Some(b.clone()),
            (None, n) => n.clone(),
        };
        let buttons: Vec<(ControlId, String, Kind)> = u
            .buttons
            .iter()
            .map(|(id, label, primary)| (*id, label.clone(), if *primary { Kind::Primary } else { Kind::Default }))
            .collect();
        let c = Card {
            look: Look::Inline,
            icon: Some(icon),
            title: Some(&u.title),
            body: body.as_deref(),
            bar: u.bar.as_ref(),
            buttons: &buttons,
        };
        let h = c.height(f, a.cw);
        c.paint(f, Rect::new(a.x, y, a.cw, h));
        used += h + 20.0;
    }
    // B16: the first-run guide, until the person closes it ("Pular" left, "Entendi" right).
    if let Some((title, body)) = f.st.guide_card() {
        let buttons = [
            (ControlId::GuideSkip, f.t(MsgKey::GuideSkip), Kind::Default),
            (ControlId::GuideDone, f.t(MsgKey::GuideDone), Kind::Primary),
        ];
        let c = Card {
            look: Look::Inline,
            icon: Some(Icon::Circle(pal.link_tint, pal.link, icons::SWAP)),
            title: Some(&title),
            body: Some(&body),
            bar: None,
            buttons: &buttons,
        };
        let h = c.height(f, a.cw);
        c.paint(f, Rect::new(a.x, y + used, a.cw, h));
        used += h + 20.0;
    }
    if f.st.fetch(DataKind::Devices) != swapdesk_ui::Fetch::Ready {
        return used;
    }
    // Every notice, the crash first. A notice of the network (a Public network, a firewall that blocks) explains
    // why no computer is listed, and with none listed this page is the only one there is.
    for kind in f.st.notices().into_iter().filter(|k| *k != NoticeKind::LocalNetwork) {
        used += notice_card(f, Look::Inline, kind, Rect::new(a.x, y + used, a.cw, 0.0)).h + 20.0;
    }
    if f.st.no_computer_card() {
        // F20: discovery found nothing for 10 s; the action focuses the address field.
        let (title, body, button) =
            (f.t(MsgKey::NoComputerTitle), f.t(MsgKey::NoComputerBody), f.t(MsgKey::NoComputerButton));
        let buttons = [(ControlId::NoComputerAddress, button, Kind::Primary)];
        let c = Card {
            look: Look::Inline,
            icon: Some(Icon::Circle(pal.link_tint, pal.link, icons::WIFI)),
            title: Some(&title),
            body: Some(&body),
            bar: None,
            buttons: &buttons,
        };
        let h = c.height(f, a.cw);
        c.paint(f, Rect::new(a.x, y + used, a.cw, h));
        used += h + 20.0;
    } else if f.st.devices().is_empty() {
        let l = p.label_in(&f.t(MsgKey::EmptyDevices), text::BODY, a.cw, Align::Center);
        p.draw(&l, a.x, y + used, pal.soft);
        used += l.h + 20.0;
    }
    used
}

/// Height of a recent-activity row: the device name (one line) over the event (wrapped), 10 px above and below.
fn recent_row_h(f: &Frame, line: &RecentLine, text_w: f32) -> f32 {
    let p = f.p;
    let who = if line.who.is_empty() { 0.0 } else { p.label("A", text::NAME).h };
    let what = p.label_in(&line.what, text::CAPTION, text_w, Align::Start).h;
    (who + what).max(p.label("A", text::SMALL).h + 1.0) + 20.0
}

fn recent_text_w(inner: f32) -> f32 {
    (inner - TIME_W - TIME_GAP).max(60.0)
}

/// How many entries fit in `room` DIPs of list height (at most all of them).
fn recent_fit(f: &Frame, room: f32, lines: &[RecentLine], text_w: f32) -> usize {
    let mut used = 0.0;
    for (n, line) in lines.iter().enumerate() {
        used += recent_row_h(f, line, text_w);
        if used > room {
            return n;
        }
    }
    lines.len()
}

/// The part of a card's height that is not its list: border, padding and the title row.
fn recent_fixed(f: &Frame) -> f32 {
    let p = f.p;
    2.0 * p.hairline() + 2.0 * CARD_PAD_Y + p.label("A", text::H2).h + 6.0
}

fn card_height(f: &Frame, card: &HomeCard, w: f32, avail: f32) -> f32 {
    let p = f.p;
    let bw = p.hairline();
    let inner = w - 2.0 * bw - 2.0 * CARD_PAD_X;
    let title_h = p.label("A", text::H2).h;
    match card {
        HomeCard::Pair { body, button: None, address_hint, .. } => {
            // A phone's card: title, body, divider, label, hint, field.
            let body_h = f.para_height(body, text::BODY, inner);
            let label_h = p.label("A", text::NAME).h;
            let hint_h = f.para_height(address_hint, text::SMALL, inner);
            2.0 * bw + 2.0 * CARD_PAD_Y + title_h + 16.0 + body_h + 16.0 + 1.0 + 16.0 + label_h + 8.0 + hint_h - 4.0
                + 8.0
                + 40.0
        }
        HomeCard::Pair { body, address_label, address_hint, .. } => {
            let body_h = f.para_height(body, text::BODY, inner);
            let label_h = p.label("A", text::NAME).h;
            let hint_h = f.para_height(address_hint, text::SMALL, inner);
            let _ = address_label;
            // title, body, button, divider, label, hint (tucked 4 px up into the label), field; gap 16 / 8.
            2.0 * bw
                + 2.0 * CARD_PAD_Y
                + title_h
                + 16.0
                + body_h
                + 16.0
                + 40.0
                + 16.0
                + 1.0
                + 16.0
                + label_h
                + 8.0
                + hint_h
                - 4.0
                + 8.0
                + 40.0
        }
        HomeCard::Recent { lines, .. } => {
            let tw = recent_text_w(inner);
            let list_h = if f.st.placeholder(DataKind::Activity) {
                3.0 * (p.label("A", text::BODY).h + 20.0)
            } else if lines.is_empty() {
                p.label("A", text::BODY).h + 10.0
            } else {
                let n = recent_fit(f, (avail - recent_fixed(f)).max(0.0), lines, tw);
                lines.iter().take(n).map(|l| recent_row_h(f, l, tw)).sum()
            };
            2.0 * bw + 2.0 * CARD_PAD_Y + title_h + 6.0 + list_h
        }
    }
}

fn paint_card(f: &mut Frame, card: &HomeCard, r: Rect, avail: f32) {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let (x, inner) = (r.x + bw + CARD_PAD_X, r.w - 2.0 * bw - 2.0 * CARD_PAD_X);
    let mut y = r.y + bw + CARD_PAD_Y;
    f.panel(r, pal.surface);
    match card {
        HomeCard::Pair { title, body, button, address_label, address_hint, add } => {
            let t = p.label_ellipsis(title, text::H2, inner);
            p.draw(&t, x, y, pal.ink);
            y += t.h + 16.0;
            y += f.para(body, text::BODY, x, y, inner, pal.soft, Align::Start) + 16.0;
            if let Some(button) = button {
                let bwid = f.button_width(button, Kind::Primary).min(inner);
                f.button(ControlId::PairThisComputer, Rect::new(x, y, bwid, 40.0), Kind::Primary, button, false);
                y += 40.0 + 16.0;
            }
            p.fill_rect(Rect::new(x, y, inner, 1.0_f32.max(bw)), pal.line);
            y += 1.0 + 16.0;
            let cap = p.label_ellipsis(address_label, text::NAME, inner);
            p.draw(&cap, x, y, pal.ink);
            y += cap.h + 8.0 - 4.0;
            y += f.para(address_hint, text::SMALL, x, y, inner, pal.soft, Align::Start) + 8.0;
            let add_w = f.button_width(add, Kind::Default);
            let placeholder = f.t(MsgKey::AddressPlaceholder);
            let field_w = (inner - add_w - 8.0).max(80.0);
            f.edit_field(
                EditId::Address,
                Rect::new(x, y + (40.0 - layout::FIELD_H) / 2.0, field_w, layout::FIELD_H),
                "",
                &placeholder,
            );
            f.button(ControlId::AddAddress, Rect::new(x + field_w + 8.0, y, add_w, 40.0), Kind::Default, add, false);
        }
        HomeCard::Recent { title, lines, empty, all } => {
            // "Ver tudo" is an inline link at the right of the title, as "Editar" is on the device page.
            let edit_style = text::NAME;
            let link = p.label(all, edit_style);
            let t = p.label_ellipsis(title, text::H2, inner - link.w - 12.0);
            p.draw(&t, x, y, pal.ink);
            let lx = x + inner - link.w;
            let ly = y + (t.h - link.h) / 2.0;
            p.draw(&link, lx, ly, pal.link);
            let content = p.content_height(edit_style);
            let link_box = Rect::new(lx, ly + (link.h - content) / 2.0, link.w, content);
            if f.hovered(ControlId::HomeActivityAll) {
                p.fill_rect(Rect::new(lx, link_box.bottom() - 1.5, link.w, bw), pal.link);
            }
            f.register(ControlId::HomeActivityAll, link_box, 0.0);
            y += t.h + 6.0;
            let tw = recent_text_w(inner);
            if f.st.placeholder(DataKind::Activity) {
                for _ in 0..3 {
                    f.block(Rect::new(x, y + 12.0, TIME_W - 14.0, 10.0), 4.0);
                    f.block(Rect::new(x + TIME_W + TIME_GAP, y + 10.0, tw * 0.8, 14.0), 4.0);
                    y += p.label("A", text::BODY).h + 20.0;
                }
            } else if lines.is_empty() {
                let l = p.label_ellipsis(empty, text::BODY, inner);
                p.draw(&l, x + (inner - l.w) / 2.0, y + 10.0, pal.soft);
            } else {
                let shown = recent_fit(f, (avail - recent_fixed(f)).max(0.0), lines, tw);
                for (n, line) in lines.iter().take(shown).enumerate() {
                    let rh = recent_row_h(f, line, tw);
                    if n > 0 {
                        p.fill_rect(Rect::new(x, y, inner, bw), pal.line);
                    }
                    let when = p.label_ellipsis(&line.when, text::SMALL, TIME_W);
                    p.draw(&when, x, y + 10.0 + 1.0, pal.soft);
                    // The device name (600, ink) over the event (13 px, soft; danger for an alert).
                    let tx = x + TIME_W + TIME_GAP;
                    let mut ly = y + 10.0;
                    if !line.who.is_empty() {
                        let who = p.label_ellipsis(&line.who, text::NAME, tw);
                        p.draw(&who, tx, ly, pal.ink);
                        ly += who.h;
                    }
                    let what = p.label_in(&line.what, text::CAPTION, tw, Align::Start);
                    p.draw(&what, tx, ly, if line.alert { pal.danger } else { pal.soft });
                    y += rh;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Devices

/// The column a page lays its content in: where it starts and how wide it is.
pub(crate) struct Area {
    pub(crate) x: f32,
    pub(crate) cw: f32,
}

pub(crate) fn area(f: &Frame) -> Area {
    if f.compact {
        // One column across the window, with the margin of a phone's screen; held sideways it is not stretched.
        use crate::screens_compact::{COLUMN_MAX, MARGIN};
        let cw = (f.w - 2.0 * MARGIN).clamp(120.0, COLUMN_MAX);
        return Area { x: f.p.snap((f.w - cw) / 2.0), cw };
    }
    Area { x: SIDEBAR_W + 40.0, cw: (f.w - SIDEBAR_W - 80.0).max(200.0) }
}

/// What a page leaves under `y` (its header, its tabs) down to the bottom margin: where a card that stands alone in
/// the page is centred (D51).
pub(crate) fn room(f: &Frame, a: &Area, y: f32) -> Rect {
    Rect::new(a.x, y, a.cw, (f.page_floor - y).max(0.0))
}

/// The stopped screen's content: the card of why the core did not start, centred in what the window leaves, or
/// from the top down when the window is shorter than the card. Returns where it ends.
pub(crate) fn stopped_page(f: &mut Frame) -> f32 {
    let (x, w) = if f.compact {
        let a = area(f);
        (a.x, a.cw)
    } else {
        (40.0, f.w - 80.0)
    };
    let kinds = f.st.notices();
    notice_cards(f, Rect::new(x, f.page_top, w, (f.page_floor - f.page_top).max(0.0)), &kinds)
}

/// What the header of a device's page offers, the negative action first and the positive one last: back to an open
/// session or its end, the wake of a computer that sleeps, the two ways to connect, or pairing.
fn device_actions(f: &Frame, d: &DeviceView) -> Vec<(ControlId, String, Kind)> {
    if d.paired && f.st.open_session_with(&d.id).is_some() {
        // A session with it is open: back to it (the positive action, right) or end it (left).
        vec![
            (ControlId::SessionEnd, f.t(MsgKey::Disconnect), Kind::Default),
            (ControlId::SessionShow, f.t(MsgKey::SessionBack), Kind::Primary),
        ]
    } else if d.needs_update {
        // Its Swapdesk is too old for this device: nothing here dials it until it is updated.
        Vec::new()
    } else if d.paired && !swapdesk_ui::UiState::dials(d) {
        // A phone: nothing here starts a connection to it (F-NET-03).
        Vec::new()
    } else if d.paired && swapdesk_ui::UiState::can_wake(d) {
        // Asleep: "Acordar" replaces the connect buttons (F-WAKE-01).
        vec![(ControlId::Wake, f.t(MsgKey::Wake), Kind::Primary)]
    } else if d.paired {
        vec![
            (ControlId::ViewOnly, f.t(MsgKey::ViewOnly), Kind::Default),
            (ControlId::Control, f.t(MsgKey::Control), Kind::Primary),
        ]
    } else {
        vec![(ControlId::PairWithDevice, f.t(MsgKey::PairWithDevice), Kind::Primary)]
    }
}

/// A device's page. Returns where its content ends.
pub(crate) fn devices_main(f: &mut Frame) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let mut y = f.page_top;
    if f.st.placeholder(DataKind::Devices) {
        return skeleton_devices(f, &a);
    }
    let Some(d) = f.st.selected_device().cloned() else {
        return if f.compact { crate::screens_compact::home(f) } else { home_main(f) };
    };
    // A wake or a connect that waits, or a connect that did not open: its card takes the page's content.
    let card = f.st.wait_card(&d.id);
    let actions = if card.is_none() { device_actions(f, &d) } else { Vec::new() };

    if f.compact {
        y = crate::screens_compact::device_header(f, &a, y, &d, &actions);
    } else {
        // Header: title block grows, the buttons sit at the end (gap 16, top-aligned).
        let head_w = a.cw - 260.0;
        let renaming = f.st.renaming().filter(|_| f.st.rename_available(&d)).cloned();
        let detail = format::detail_line(f.st.language(), &d);
        // Not renaming: the name, then "Renomear" beside it. Renaming: the field with "Salvar" and "Cancelar".
        let (title_h, status) = if let Some(r) = &renaming {
            let (save, cancel) = (f.t(MsgKey::RenameSave), f.t(MsgKey::Cancel));
            let (sw, cw) = (f.button_width(&save, Kind::Primary), f.button_width(&cancel, Kind::Default));
            let field_w = (head_w - sw - cw - 2.0 * 8.0 - 12.0).clamp(120.0, 360.0);
            f.edit_field(
                EditId::Rename,
                Rect::new(a.x, y + (40.0 - layout::FIELD_H) / 2.0, field_w, layout::FIELD_H),
                &d.name,
                "",
            );
            let sx = a.x + field_w + 12.0;
            // Negative first, positive last (owner, 09/11).
            f.button(ControlId::RenameCancel, Rect::new(sx, y, cw, 40.0), Kind::Default, &cancel, false);
            f.button(ControlId::RenameSave, Rect::new(sx + cw + 8.0, y, sw, 40.0), Kind::Primary, &save, false);
            // The refusal sits where the detail line is, in the danger colour.
            let (text, color) = match &r.error {
                Some(e) => (f.st.rename_error_text(e), pal.danger),
                None => (detail.clone(), pal.soft),
            };
            let status = p.label_ellipsis(&text, text::BODY, head_w);
            p.draw(&status, a.x, y + 40.0 + 6.0, color);
            (40.0, status)
        } else {
            let rename = f.t(MsgKey::RenameAction);
            let can_rename = f.st.rename_available(&d) && card.is_none();
            let rw = if can_rename { f.button_width(&rename, Kind::Quiet) + 12.0 } else { 0.0 };
            let title = p.label_ellipsis(&d.name, text::H1, (head_w - rw).max(80.0));
            let status = p.label_ellipsis(&detail, text::BODY, head_w);
            p.draw(&title, a.x, y, pal.ink);
            p.draw(&status, a.x, y + title.h + 6.0, pal.soft);
            if can_rename {
                let bw = rw - 12.0;
                f.button(
                    ControlId::RenameDevice,
                    Rect::new(a.x + title.w + 12.0, y + (title.h - 40.0) / 2.0, bw, 40.0),
                    Kind::Quiet,
                    &rename,
                    false,
                );
            }
            (title.h, status)
        };
        // The actions end at the page's right edge, 16 apart, the positive one last.
        let widths: Vec<f32> = actions.iter().map(|(_, label, kind)| f.button_width(label, *kind)).collect();
        let mut rects = vec![Rect::new(0.0, 0.0, 0.0, 0.0); actions.len()];
        let mut right = a.x + a.cw;
        for i in (0..actions.len()).rev() {
            rects[i] = Rect::new(right - widths[i], y, widths[i], 40.0);
            right = rects[i].x - 16.0;
        }
        for ((id, label, kind), rect) in actions.iter().zip(rects) {
            f.button(*id, rect, *kind, label, false);
        }
        y += (title_h + 6.0 + status.h).max(40.0) + 28.0;
    }

    if let Some(card) = card {
        return wait_card(f, room(f, &a, y), &card).bottom();
    }
    if d.paired {
        y += tabs(f, a.x, y, a.cw) + 28.0;
    }
    let notices = f.st.notices();
    if !notices.is_empty() {
        return notice_cards(f, room(f, &a, y), &notices);
    }
    if !d.paired && d.needs_update {
        // Its Swapdesk is older than this device can talk to: what to do about it, alone in the page (D51).
        let (title, body) = (f.t(MsgKey::NeedsUpdateTitle), f.t(MsgKey::NeedsUpdateBody));
        return Card {
            look: Look::Alone,
            icon: Some(Icon::Circle(pal.link_tint, pal.link, icons::DOWNLOAD)),
            title: Some(&title),
            body: Some(&body),
            bar: None,
            buttons: &[],
        }
        .paint_in(f, room(f, &a, y), card::W)
        .bottom();
    }
    if !d.paired {
        // Not paired yet: what pairing takes, alone in the page (D51).
        let body = f.t(MsgKey::PairNotPairedBody);
        return Card {
            look: Look::Alone,
            icon: Some(Icon::Circle(pal.link_tint, pal.link, icons::SHIELD)),
            title: None,
            body: Some(&body),
            bar: None,
            buttons: &[],
        }
        .paint_in(f, room(f, &a, y), card::W)
        .bottom();
    }
    match f.st.tab() {
        DeviceTab::Overview => {
            y += pair_panel(f, &a, y, &d) + 28.0;
            info_panels(f, &a, y, &d)
        }
        DeviceTab::Permissions => permissions(f, &a, y, &d),
        DeviceTab::History => history(f, &a, y, &d),
        DeviceTab::Chat => {
            // The conversation with the device that controls this computer, in a panel that takes the rest of the
            // page: the same messages, field and button as the session window's chat panel.
            let rest = if f.compact { f.page_floor - y } else { f.h - y - 32.0 };
            let r = Rect::new(a.x, y, a.cw, rest.max(220.0));
            f.panel(r, f.pal.surface);
            crate::screens_viewer::drawer_chat(f, r);
            r.bottom()
        }
    }
}

fn skeleton_devices(f: &mut Frame, a: &Area) -> f32 {
    let p = f.p;
    let mut y = f.page_top;
    let title_h = p.label("A", text::H1).h;
    let body_h = p.label("A", text::BODY).h;
    if f.compact {
        // The compact page: the name, its line, the two buttons across, the strip and one panel.
        f.block(Rect::new(a.x, y + 2.0, (a.cw * 0.7).min(280.0), title_h - 4.0), 6.0);
        f.block(Rect::new(a.x, y + title_h + 6.0 + 3.0, (a.cw * 0.9).min(340.0), body_h - 6.0), 4.0);
        y += title_h + 6.0 + body_h + 16.0;
        let half = (a.cw - 12.0) / 2.0;
        f.block(Rect::new(a.x, y, half, 40.0), 8.0);
        f.block(Rect::new(a.x + half + 12.0, y, half, 40.0), 8.0);
        y += 40.0 + 24.0;
        f.block(Rect::new(a.x, y, a.cw, 190.0), 12.0);
        y += 190.0 + 16.0;
        f.block(Rect::new(a.x, y, a.cw, 110.0), 12.0);
        return y + 110.0;
    }
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
    y + ph
}

/// Tabs: 40 px high, gap 4, padding 0 12, a 2 px underline on the selected one. Returns the container's height. In the
/// compact layout tabs that do not fit side by side share the width equally and trim their names.
fn tabs(f: &mut Frame, x: f32, y: f32, cw: f32) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    // The container's 1 px border sits under the tabs' -1 px margin.
    p.fill_rect(Rect::new(x, y + 39.0, cw, bw), pal.line);
    let list: Vec<(DeviceTab, MsgKey)> = f.st.device_tabs().into_iter().collect();
    let style = |f: &Frame, tab: DeviceTab| if f.st.tab() == tab { text::TAB_SELECTED } else { text::TAB };
    let share = if f.compact && !list.is_empty() {
        let gaps = 4.0 * (list.len() - 1) as f32;
        let natural: f32 = list.iter().map(|(tab, key)| p.label(&f.t(*key), style(f, *tab)).w + 24.0).sum();
        (natural + gaps > cw).then(|| (cw - gaps) / list.len() as f32)
    } else {
        None
    };
    let mut tx = x;
    for (tab, key) in list {
        let selected = f.st.tab() == tab;
        let id = ControlId::Tab(tab);
        let label = match share {
            Some(w) => p.label_ellipsis(&f.t(key), style(f, tab), (w - 8.0).max(8.0)),
            None => p.label(&f.t(key), style(f, tab)),
        };
        let rect = Rect::new(tx, y, share.unwrap_or(label.w + 24.0), 40.0);
        if selected {
            p.fill_rect(Rect::new(rect.x, y + 38.0, rect.w, 2.0), pal.ink);
        }
        let color = if selected || f.hovered(id) { pal.ink } else { pal.soft };
        // The 2 px bottom border leaves a 38 px content box.
        let lx = if share.is_some() { tx + (rect.w - label.w) / 2.0 } else { tx + 12.0 };
        p.draw(&label, lx, y + (38.0 - label.h) / 2.0, color);
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
    // padding 32 36, gap 22; columns 180 px wide with the wire between them. The compact layout keeps the strip in
    // the page's width: less padding, and columns as wide as leave the wire its room.
    let bw = p.hairline();
    let (pad_x, pad_y) = if f.compact { (16.0, 24.0) } else { (36.0, 32.0) };
    let inner_w = a.cw - 2.0 * bw - 2.0 * pad_x;
    let col_w =
        if f.compact { ((inner_w - 96.0) / 2.0).clamp(72.0, layout::PAIR_COLUMN_W) } else { layout::PAIR_COLUMN_W };
    // A phone comes in and is never dialled: the wire points at this computer and the note says who starts.
    let phone = !swapdesk_ui::UiState::dials(d);
    let note_key = if phone { MsgKey::PhoneNote } else { MsgKey::SwapNote };
    let note = p.label_in(&f.t(note_key), text::BODY, inner_w, Align::Center);
    let this = this_name(f);
    let caption_this = f.t(MsgKey::DeviceCaptionThis);
    let caption_that = format::os_label(f.st.language(), d.os, &d.os_version);
    // In the narrow columns a name wraps: the row is as tall as the taller column, and both captions start level.
    let name_h =
        f.compact.then(|| f.para_height(&this, text::NAME, col_w).max(f.para_height(&d.name, text::NAME, col_w)));
    let row_h = match name_h {
        Some(name_h) => {
            let caption_h =
                f.para_height(&caption_this, text::SMALL, col_w).max(f.para_height(&caption_that, text::SMALL, col_w));
            64.0 + 10.0 + name_h + 10.0 + caption_h
        }
        None => 64.0 + 10.0 + p.label("A", text::NAME).h + 10.0 + p.label("A", text::SMALL).h,
    };
    let h = bw + pad_y + row_h + 22.0 + note.h + pad_y + bw;
    f.panel(Rect::new(a.x, y, a.cw, h), pal.surface);

    let left = a.x + bw + pad_x;
    let right = a.x + a.cw - bw - pad_x - col_w;
    let top = y + bw + pad_y;
    device_column(f, (left, top, col_w), &this, &caption_this, pal.ink, f.st.on_phone(), name_h);
    device_column(f, (right, top, col_w), &d.name, &caption_that, pal.link, phone, name_h);

    // The middle column is centred on the row, then pulled up by its -34 px top margin.
    let mid_x = left + col_w;
    let mid_w = right - mid_x;
    let wire_key = if phone { MsgKey::PhoneControls } else { MsgKey::YouControl };
    let wire_label = p.label_in(&f.t(wire_key), text::WIRE, mid_w, Align::Center);
    let wire_top = top + (row_h - (wire_label.h + 10.0 + 18.0 - 34.0)) / 2.0 - 34.0;
    p.draw(&wire_label, mid_x, wire_top, pal.link);
    let cy = wire_top + wire_label.h + 10.0 + 9.0;
    if phone {
        f.wire_back(mid_x, mid_x + mid_w, cy, layout::PAIR_WIRE, layout::PAIR_ARROW_W, layout::PAIR_ARROW_H, pal.link);
    } else {
        f.wire(mid_x, mid_x + mid_w, cy, layout::PAIR_WIRE, layout::PAIR_ARROW_W, layout::PAIR_ARROW_H, pal.link);
    }

    p.draw(&note, a.x + bw + pad_x, top + row_h + 22.0, pal.soft);
    h
}

/// A column of the strip at `(x, y)`, `w` wide (180 px in the design): the device's outline, its name (600), its
/// caption (12 px), gap 10, all centred. `name_h` is the height both columns keep for the name, when it is not this
/// column's own.
fn device_column(
    f: &Frame,
    (x, y, w): (f32, f32, f32),
    name: &str,
    caption: &str,
    stroke: Color,
    phone: bool,
    name_h: Option<f32>,
) {
    let p = f.p;
    if phone {
        f.phone(x + (w - 64.0) / 2.0, y, 64.0, 2.5, stroke);
    } else {
        f.laptop(x + (w - 64.0) / 2.0, y, 64.0, 2.5, stroke);
    }
    let n = p.label_in(name, text::NAME, w, Align::Center);
    p.draw(&n, x, y + 64.0 + 10.0, f.pal.ink);
    let c = p.label_in(caption, text::SMALL, w, Align::Center);
    p.draw(&c, x, y + 64.0 + 10.0 + name_h.unwrap_or(n.h) + 10.0, f.pal.soft);
}

/// Two equal panels, gap 20, padding 20 22, gap 10 between the heading and the text; one above the other, each as
/// tall as its own text, in the compact layout. Returns where they end.
fn info_panels(f: &mut Frame, a: &Area, y: f32, d: &DeviceView) -> f32 {
    let (p, pal) = (f.p, f.pal);
    if !f.st.grants_shown() {
        // Nothing connects to this device (a phone): the last session alone, across the page.
        let bw = p.hairline();
        let last = p.label_in(&f.st.last_session_text(d), text::BODY, a.cw - 2.0 * bw - 44.0, Align::Start);
        let title = p.label(&f.t(MsgKey::LastSessionTitle), text::H2);
        let panel = Rect::new(a.x, y, a.cw, 2.0 * bw + 40.0 + title.h + 10.0 + last.h);
        f.panel(panel, pal.surface);
        p.draw(&title, panel.x + bw + 22.0, panel.y + bw + 20.0, pal.ink);
        p.draw(&last, panel.x + bw + 22.0, panel.y + bw + 20.0 + title.h + 10.0, pal.soft);
        return panel.bottom();
    }
    let stacked = f.compact;
    let pw = if stacked { a.cw } else { (a.cw - 20.0) / 2.0 };
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
    // The first panel's heading names the computer: one trimmed line, or in the compact layout as many as it needs.
    let heading_text = f.st.tm(&Msg::new(MsgKey::PermsPanelTitle).with(d.name.clone()));
    let heading = if stacked {
        p.label_in(&heading_text, text::H2, text_w - edit.w - 12.0, Align::Start)
    } else {
        p.label_ellipsis(&heading_text, text::H2, text_w - edit.w - 12.0)
    };
    let h = 2.0 * bw + 40.0 + heading_h + 10.0 + perms_l.h.max(last_l.h);
    let first_head_h = if stacked { heading.h.max(heading_h) } else { heading_h };
    let (h1, h2) = if stacked {
        (2.0 * bw + 40.0 + first_head_h + 10.0 + perms_l.h, 2.0 * bw + 40.0 + heading_h + 10.0 + last_l.h)
    } else {
        (h, h)
    };

    let first = Rect::new(a.x, y, pw, h1);
    let second = if stacked { Rect::new(a.x, y + h1 + 16.0, pw, h2) } else { Rect::new(a.x + pw + 20.0, y, pw, h2) };
    f.panel(first, pal.surface);
    f.panel(second, pal.surface);

    p.draw(&heading, first.x + bw + 22.0, first.y + bw + 20.0, pal.ink);
    p.draw(&perms_l, first.x + bw + 22.0, first.y + bw + 20.0 + first_head_h + 10.0, pal.soft);

    // "Editar" is an inline link: its focus box is the font's content area, not the line box.
    let edit_x = first.right() - bw - 22.0 - edit.w;
    let edit_y = first.y + bw + 20.0 + (heading_h - edit.h) / 2.0;
    p.draw(&edit, edit_x, edit_y, pal.link);
    let content = p.content_height(edit_style);
    let edit_box = Rect::new(edit_x, edit_y + (edit.h - content) / 2.0, edit.w, content);
    if f.hovered(ControlId::EditGrants) {
        p.fill_rect(Rect::new(edit_x, edit_box.bottom() - 1.5, edit.w, p.hairline()), pal.link);
    }
    f.register(ControlId::EditGrants, edit_box, 0.0);

    let title2 = p.label(&f.t(MsgKey::LastSessionTitle), text::H2);
    p.draw(&title2, second.x + bw + 22.0, second.y + bw + 20.0, pal.ink);
    p.draw(&last_l, second.x + bw + 22.0, second.y + bw + 20.0 + heading_h + 10.0, pal.soft);
    first.bottom().max(second.bottom())
}

/// The Permissões tab: what the other machine may do here (switches), what this one may do there (read only); side
/// by side, or the second under the first in the compact layout, where a row grows with its text. Returns where the
/// tab ends.
fn permissions(f: &mut Frame, a: &Area, y: f32, d: &DeviceView) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let stacked = f.compact;
    let colw = if stacked { a.cw } else { (a.cw - 20.0) / 2.0 };
    let h2 = p.label("A", text::H2).h;
    let lx = a.x;

    // A title names the computer: one trimmed line, or in the compact layout as many as it needs.
    let head = |f: &Frame, key: MsgKey| {
        let s = f.st.tm(&Msg::new(key).with(d.name.clone()));
        if stacked { p.label_in(&s, text::H2, colw, Align::Start) } else { p.label_ellipsis(&s, text::H2, colw) }
    };
    // What the other machine may do here; not where nothing connects to this device (a phone), whose tab is the
    // other machine's answers alone.
    let mut second = (a.x, y);
    if f.st.grants_shown() {
        // Left: h2, gap 10, panel of switch rows (min-height 50, padding 0 20, border-top).
        let title = head(f, MsgKey::PermsHereTitle);
        p.draw(&title, lx, y, pal.ink);
        let rows = f.st.permission_rows();
        let panel_top = y + if stacked { title.h } else { h2 } + 10.0;
        // Each row's label and hint; in the compact layout they wrap beside the switch and the row is as tall as they
        // need.
        let text_w = colw - 2.0 * bw - 40.0 - layout::SWITCH_W - 12.0;
        let lay = |s: &str, st| if stacked { p.label_in(s, st, text_w, Align::Start) } else { p.label(s, st) };
        let labels: Vec<_> = rows
            .iter()
            .map(|(_, label, hint)| (lay(label, text::BODY), hint.as_ref().map(|h| lay(h, text::SMALL))))
            .collect();
        let heights: Vec<f32> = labels
            .iter()
            .map(|(l, hint)| if stacked { (l.h + hint.as_ref().map_or(0.0, |h| h.h) + 20.0).max(52.0) } else { 50.0 })
            .collect();
        let panel_h =
            if stacked { 2.0 * bw + heights.iter().sum::<f32>() } else { 2.0 * bw + rows.len() as f32 * 50.0 };
        f.panel(Rect::new(lx, panel_top, colw, panel_h), pal.surface);
        let mut ry = panel_top + bw;
        for (n, ((perm, _, _), (l, hint_l))) in rows.iter().zip(&labels).enumerate() {
            let row_h = heights[n];
            if n > 0 {
                p.fill_rect(Rect::new(lx + bw, ry, colw - 2.0 * bw, bw), pal.line);
            }
            let block_h = l.h + hint_l.as_ref().map_or(0.0, |h| h.h);
            let ty = ry + (row_h - block_h) / 2.0;
            p.draw(l, lx + bw + 20.0, ty, pal.ink);
            if let Some(h) = hint_l {
                p.draw(h, lx + bw + 20.0, ty + l.h, pal.soft);
            }
            // A permission this computer cannot serve is drawn off and dimmed, like one that waits (its line says why).
            let available = f.st.grant_available(*perm);
            let on = available && d.grants_here.has(*perm);
            let waiting = f.st.grant_waiting(*perm) || !available;
            f.switch(
                ControlId::Grant(*perm),
                lx + colw - bw - 20.0 - layout::SWITCH_W,
                ry + (row_h - layout::SWITCH_H) / 2.0,
                on,
                waiting,
            );
            ry += row_h;
        }

        second = if stacked { (a.x, panel_top + panel_h + 24.0) } else { (a.x + colw + 20.0, y) };
    }

    // The other machine's answers (beside the first, or under it in the compact layout), on a sunken panel; then
    // the note and "Esquecer".
    let (rx, top2) = second;
    let title = head(f, MsgKey::PermsThereTitle);
    let panel_top = top2 + if stacked { title.h } else { h2 } + 10.0;
    p.draw(&title, rx, top2, pal.ink);
    let answers = f.st.there_rows(d);
    let there_h = 2.0 * bw + answers.len() as f32 * 50.0;
    f.panel(Rect::new(rx, panel_top, colw, there_h), pal.sunken);
    let mut ry = panel_top + bw;
    let yes = f.t(MsgKey::PermYes);
    for (n, (label, answer)) in answers.iter().enumerate() {
        if n > 0 {
            p.fill_rect(Rect::new(rx + bw, ry, colw - 2.0 * bw, bw), pal.line);
        }
        let (style, color) = if *answer == yes { (text::NAME, pal.ok) } else { (text::BODY, pal.soft) };
        let al = p.label(answer, style);
        let l = if stacked {
            p.label_ellipsis(label, text::BODY, (colw - 2.0 * bw - 40.0 - al.w - 12.0).max(40.0))
        } else {
            p.label(label, text::BODY)
        };
        p.draw(&l, rx + bw + 20.0, ry + (50.0 - l.h) / 2.0, pal.ink);
        p.draw(&al, rx + colw - bw - 20.0 - al.w, ry + (50.0 - al.h) / 2.0, color);
        ry += 50.0;
    }
    let note_y = panel_top + there_h + 10.0;
    let note = f.st.tm(&Msg::new(MsgKey::ThereNote).with(d.name.clone()));
    let nh = f.para(&note, text::CAPTION, rx, note_y, colw, pal.soft, Align::Start);
    let forget = f.t(MsgKey::ForgetDevice);
    let fw = f.button_width(&forget, Kind::Danger);
    let button = Rect::new(rx, note_y + nh + 10.0 + 8.0, fw, 40.0);
    f.button(ControlId::Forget, button, Kind::Danger, &forget, false);
    button.bottom()
}

/// The history tab holds one message for now: a card alone in the tab (D51). Returns where it ends.
fn history(f: &mut Frame, a: &Area, y: f32, d: &DeviceView) -> f32 {
    let pal = f.pal;
    let title = f.st.tm(&Msg::new(MsgKey::HistoryTitle).with(d.name.clone()));
    let body = f.t(MsgKey::HistoryPlaceholder);
    Card {
        look: Look::Alone,
        icon: Some(Icon::Circle(pal.link_tint, pal.link, icons::CLOCK)),
        title: Some(&title),
        body: Some(&body),
        bar: None,
        buttons: &[],
    }
    .paint_in(f, room(f, a, y), card::W)
    .bottom()
}

fn notice_icon(kind: NoticeKind) -> String {
    match kind {
        NoticeKind::Public | NoticeKind::Firewall | NoticeKind::LocalNetwork => icons::WIFI.to_owned(),
        NoticeKind::Service | NoticeKind::ScreenLock => icons::lock(),
        NoticeKind::MacPerms | NoticeKind::ReceiveFolder | NoticeKind::NotStarted => icons::SHIELD.to_owned(),
        NoticeKind::Crash | NoticeKind::DataUnreadable => icons::SHIELD_ALERT.to_owned(),
    }
}

/// One notice as a card: its icon, title, text and primary button, centred (D51). `r` gives the place and the
/// width; a height of 0 means as tall as the content. Returns the card's rectangle.
fn notice_card(f: &mut Frame, look: Look, kind: NoticeKind, r: Rect) -> Rect {
    let pal = f.pal;
    let (title, body, button) = f.st.notice_texts(kind);
    // A notice that waits (the system's question is on the screen) is not an alarm.
    let calm = matches!(kind, NoticeKind::MacPerms | NoticeKind::ReceiveFolder) || button.is_empty();
    let (tint, ink) = if calm { (pal.link_tint, pal.link) } else { (pal.danger_tint, pal.danger) };
    let icon = notice_icon(kind);
    let buttons = notice_buttons(kind, button);
    let c = Card {
        look,
        icon: Some(Icon::Circle(tint, ink, &icon)),
        title: Some(&title),
        body: Some(&body),
        bar: None,
        buttons: &buttons,
    };
    let h = r.h.max(c.height(f, r.w));
    c.paint(f, Rect::new(r.x, r.y, r.w, h))
}

/// The height [`notice_card`] takes at width `w`.
fn notice_card_height(f: &Frame, look: Look, kind: NoticeKind, w: f32) -> f32 {
    let (title, body, button) = f.st.notice_texts(kind);
    let buttons = notice_buttons(kind, button);
    Card { look, icon: Some(Icon::Spinner), title: Some(&title), body: Some(&body), bar: None, buttons: &buttons }
        .height(f, w)
}

/// The button of a notice; none when the notice has no text for one (`UiState::notice_texts`).
fn notice_buttons(kind: NoticeKind, button: String) -> Vec<(ControlId, String, Kind)> {
    if button.is_empty() { Vec::new() } else { vec![(ControlId::Notice(kind), button, Kind::Primary)] }
}

/// The notices that take a device page's content, centred in `room` (D51): one is a card alone; several stand side
/// by side, up to three per row (one per row in the compact layout), each row as tall as its tallest card. Returns
/// where they end.
fn notice_cards(f: &mut Frame, room: Rect, kinds: &[NoticeKind]) -> f32 {
    let p = f.p;
    if let [kind] = kinds {
        let w = card::W.min(room.w);
        let h = notice_card_height(f, Look::Alone, *kind, w);
        let at = Rect::new(p.snap(room.x + (room.w - w) / 2.0), p.snap(room.y + ((room.h - h) / 2.0).max(0.0)), w, h);
        return notice_card(f, Look::Alone, *kind, at).bottom();
    }
    let gap = 20.0;
    let per_row = if f.compact { 1 } else { kinds.len().clamp(1, 3) };
    let w = ((room.w - gap * (per_row as f32 - 1.0)) / per_row as f32).min(card::W);
    let rows: Vec<&[NoticeKind]> = kinds.chunks(per_row).collect();
    let heights: Vec<f32> = rows
        .iter()
        .map(|row| row.iter().map(|k| notice_card_height(f, Look::Inline, *k, w)).fold(0.0, f32::max))
        .collect();
    let total = heights.iter().sum::<f32>() + gap * rows.len().saturating_sub(1) as f32;
    let mut y = p.snap(room.y + ((room.h - total) / 2.0).max(0.0));
    for (row, h) in rows.iter().zip(&heights) {
        let row_w = w * row.len() as f32 + gap * (row.len() as f32 - 1.0);
        let mut x = p.snap(room.x + (room.w - row_w) / 2.0);
        for kind in row.iter() {
            notice_card(f, Look::Inline, *kind, Rect::new(x, y, w, *h));
            x += w + gap;
        }
        y += h + gap;
    }
    y - gap
}

/// The card a device's page shows in place of its content while a wake or a connect waits, or after a connect that
/// did not open: an icon, what is waited for and on whom, the time left when there is a limit, and the way out. It
/// stands alone in the page, so it is centred in the room under the header, with its own content centred, and wide
/// enough to be read at a glance (D51).
fn wait_card(f: &mut Frame, room: Rect, card: &WaitCard) -> Rect {
    let pal = f.pal;
    let (tint, ink, icon) = match card.icon {
        WaitIcon::Power => (pal.link_tint, pal.link, icons::POWER),
        WaitIcon::Clock => (pal.link_tint, pal.link, icons::CLOCK),
        WaitIcon::ShieldAlert => (pal.danger_tint, pal.danger, icons::SHIELD_ALERT),
    };
    let buttons: Vec<(ControlId, String, Kind)> = card
        .buttons
        .iter()
        .map(|b| (b.id, b.label.clone(), if b.primary { Kind::Primary } else { Kind::Default }))
        .collect();
    Card {
        look: Look::Alone,
        icon: Some(Icon::Circle(tint, ink, icon)),
        title: Some(&card.title),
        body: Some(&card.body),
        bar: card.bar.as_ref(),
        buttons: &buttons,
    }
    .paint_in(f, room, card::W)
}

/// The error strip: floats over the bottom of the content so nothing moves when it appears.
pub(crate) fn error_toast(f: &mut Frame) {
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

pub(crate) fn activity_main(f: &mut Frame) -> f32 {
    if f.compact {
        return crate::screens_compact::activity(f);
    }
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let mut y = f.page_top;
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
        return y;
    }
    if !loading && total == 0 {
        // Nothing logged yet: the page says so, alone and centred (D51), instead of a table with no rows.
        let empty = f.t(MsgKey::HomeRecentEmpty);
        return Card {
            look: Look::Alone,
            icon: Some(Icon::Circle(pal.link_tint, pal.link, icons::CLOCK)),
            title: None,
            body: Some(&empty),
            bar: None,
            buttons: &[],
        }
        .paint_in(f, room(f, &a, y), card::W)
        .bottom();
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
        let scroll = f.page_scroll(SCROLL_ACTIVITY, max_scroll(total, 50.0, list.h));
        f.out.list_area = Some(list);
        for n in visible_rows(total, 50.0, scroll, list.h) {
            let row = &rows[n];
            let ry = list.y + n as f32 * 50.0 - scroll;
            let mut cx = list.x;
            let who = f.st.activity_who(row);
            let cells = [
                (format::table_when(f.st.language(), row.at_ms, f.st.clock()), pal.ink, text::BODY),
                (who, if row.alert { pal.danger } else { pal.ink }, if row.alert { text::NAME } else { text::BODY }),
                (f.st.activity_what(row), pal.ink, text::BODY),
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
    panel.bottom()
}

// ---------------------------------------------------------------------------------------------------------------
// Ajustes

enum SettingRow {
    Edit {
        label: String,
        hint: Option<String>,
        value: String,
        width: f32,
    },
    Select {
        id: SelectId,
        label: String,
        hint: Option<String>,
        value: String,
        width: f32,
    },
    Toggle {
        id: ControlId,
        label: String,
        hint: Option<String>,
        on: bool,
    },
    Status {
        label: String,
        hint: Option<String>,
        value: Option<(String, Color)>,
    },
    /// A label and its hint with a button at the row's end ("Procurar agora").
    Button {
        id: ControlId,
        label: String,
        hint: Option<String>,
        button: String,
    },
}

pub(crate) fn settings_main(f: &mut Frame) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let y = f.page_top;
    let title = p.label(&f.t(MsgKey::SettingsTitle), text::H1);
    let top = y + title.h + 28.0;
    let colw = (a.cw - 24.0) / 2.0;
    let (lx, rx) = (a.x, a.x + colw + 24.0);
    let Some(set) = f.st.settings().cloned() else {
        if f.compact && f.st.placeholder(DataKind::Settings) {
            // One column of groups, as the page will be.
            let mut gy = top;
            for n in [3usize, 3, 2, 2] {
                f.block(Rect::new(a.x, gy + 2.0, 140.0, 15.0), 4.0);
                let h = 2.0 + n as f32 * 52.0;
                f.block(Rect::new(a.x, gy + 21.75 + 10.0, a.cw, h), 12.0);
                gy += 21.75 + 10.0 + h + 24.0;
            }
            return gy - 24.0;
        }
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
        return top;
    };
    let net = f.st.network().cloned();
    let net_row = match &net {
        Some(n) => match &n.address {
            Some(addr) => f.st.tm(&Msg::new(MsgKey::NetworkRow).with(n.label.clone()).with(addr.clone())),
            None => f.st.tm(&Msg::new(MsgKey::NetworkRowNoAddress).with(n.label.clone())),
        },
        None => f.t(MsgKey::NoNetwork),
    };
    let left = [
        (
            MsgKey::SidebarThisComputer,
            vec![
                SettingRow::Edit {
                    label: f.t(MsgKey::FieldName),
                    hint: Some(f.t(MsgKey::NameNetworkHint)),
                    value: set.name.clone(),
                    width: 190.0,
                },
                SettingRow::Select {
                    id: SelectId::Language,
                    label: f.t(MsgKey::FieldLanguage),
                    hint: None,
                    value: f.st.select_value(SelectId::Language),
                    width: 190.0,
                },
                SettingRow::Select {
                    id: SelectId::Theme,
                    label: f.t(MsgKey::FieldTheme),
                    hint: None,
                    value: f.st.select_value(SelectId::Theme),
                    width: 190.0,
                },
                SettingRow::Toggle {
                    id: ControlId::OpenAtLogin,
                    label: f.t(MsgKey::OpenAtLogin),
                    hint: None,
                    on: set.open_at_login,
                },
            ],
        ),
        (
            MsgKey::GroupUnattended,
            vec![
                SettingRow::Toggle {
                    id: ControlId::Receive,
                    label: f.t(MsgKey::ServiceName),
                    hint: Some(f.t(MsgKey::ServiceHint)),
                    on: set.service_enabled,
                },
                SettingRow::Toggle {
                    id: ControlId::LockOnEnd,
                    label: f.t(MsgKey::LockOnEnd),
                    hint: None,
                    on: set.lock_on_end,
                },
                SettingRow::Toggle {
                    id: ControlId::KeepAwake,
                    label: f.t(MsgKey::KeepAwake),
                    hint: None,
                    on: set.keep_awake,
                },
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
                SettingRow::Toggle {
                    id: ControlId::ShowStats,
                    label: f.t(MsgKey::ShowStats),
                    hint: None,
                    on: set.show_stats,
                },
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
                    hint: None,
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
    // D65: the devices blocked by MAC, each with "Desbloquear", and what blocking by MAC cannot promise.
    let mut right = Vec::from(right);
    let mut blocked: Vec<SettingRow> =
        f.st.blocked_rows()
            .into_iter()
            .map(|(label, line, n)| SettingRow::Button {
                id: ControlId::Unblock(n),
                label,
                hint: Some(line),
                button: f.t(MsgKey::Unblock),
            })
            .collect();
    if blocked.is_empty() {
        blocked.push(SettingRow::Status { label: f.t(MsgKey::BlockedNone), hint: None, value: None });
    }
    if let Some(SettingRow::Status { hint, .. } | SettingRow::Button { hint, .. } | SettingRow::Toggle { hint, .. }) =
        blocked.last_mut()
    {
        let note = f.t(MsgKey::BlockedNote);
        *hint = Some(match hint.take() {
            Some(h) => format!(
                "{h}
{note}"
            ),
            None => note,
        });
    }
    right.push((MsgKey::GroupBlocked, blocked));
    // UPDATES.md 10.1: "Atualizações", when the service has an updater: the switch, then the version with its last
    // check and "Procurar agora".
    let mut left = Vec::from(left);
    if let (Some(info), Some(lines)) = (f.st.update_info().cloned(), f.st.update_lines()) {
        use swapdesk_ui::update_card::UpdAction;
        let mut lines = lines.into_iter();
        let version = lines.next().unwrap_or_default();
        let rest: Vec<String> = lines.collect();
        left.push((
            MsgKey::UpdSection,
            vec![
                SettingRow::Toggle {
                    id: ControlId::Update(UpdAction::Switch),
                    label: f.t(MsgKey::UpdSwitch),
                    hint: Some(f.t(MsgKey::UpdSwitchHint)),
                    on: info.check_on,
                },
                SettingRow::Button {
                    id: ControlId::Update(UpdAction::CheckNow),
                    label: version,
                    hint: (!rest.is_empty()).then(|| rest.join("\n")),
                    button: f.t(MsgKey::UpdCheckNow),
                },
            ],
        ));
    }
    if f.st.on_phone() {
        // A phone: its name, language and theme, the picture, the network. The rest is a computer's (opening at
        // login, receiving, a desktop's keyboard, the devices it blocks), as the state's own list of controls says.
        left.retain(|(group, _)| *group == MsgKey::SidebarThisComputer);
        for (_, rows) in &mut left {
            rows.retain(|row| !matches!(row, SettingRow::Toggle { id: ControlId::OpenAtLogin, .. }));
        }
        right.retain(|(group, _)| matches!(group, MsgKey::GroupImage | MsgKey::GroupNetwork));
    }
    if f.compact {
        // One column, the left groups first; the page itself scrolls (`screens_compact`).
        p.draw(&title, a.x, y, pal.ink);
        let mut gy = top;
        for (title, rows) in left.into_iter().chain(right) {
            gy += settings_group(f, a.x, gy, a.cw, &f.t(title), &rows) + 24.0;
        }
        return gy - 24.0;
    }
    // The page scrolls when a column is taller than the window: the bottom margin equals the top one.
    let column = |f: &Frame, groups: &[(MsgKey, Vec<SettingRow>)]| {
        groups.iter().map(|(title, rows)| settings_group_height(f, colw, &f.t(*title), rows) + 24.0).sum::<f32>()
    };
    let content = top + column(f, &left).max(column(f, &right)) - 24.0 + y;
    let scroll = f.page_scroll(SCROLL_SETTINGS, (content - f.h).max(0.0));
    p.push_clip(Rect::new(SIDEBAR_W, 0.0, f.w - SIDEBAR_W, f.h));
    p.draw(&title, a.x, y - scroll, pal.ink);
    let mut gy = top - scroll;
    for (title, rows) in left {
        gy += settings_group(f, lx, gy, colw, &f.t(title), &rows) + 24.0;
    }
    let mut gy = top - scroll;
    for (title, rows) in right {
        gy += settings_group(f, rx, gy, colw, &f.t(title), &rows) + 24.0;
    }
    p.pop_clip();
    content - scroll
}

/// The scrolling contents of the main window (`Output::scroll_key`).
const SCROLL_ACTIVITY: u8 = 1;
const SCROLL_SETTINGS: u8 = 2;

/// `.group`: h2, gap 10, a panel of rows (min-height 52, padding 0 18, border-top). Returns the height.
/// The space a settings row keeps above and below its label and hint (`.row{padding:7px 18px}` of the board).
const SETTING_ROW_PAD: f32 = 7.0;

/// In the compact layout a row's text field or list goes under its label, across the row: beside it there is no
/// room for both.
fn row_stacked(f: &Frame, row: &SettingRow) -> bool {
    f.compact && matches!(row, SettingRow::Edit { .. } | SettingRow::Select { .. })
}

/// The space a stacked row keeps above its label and below its control, and between the two.
const STACKED_PAD: f32 = 12.0;
const STACKED_GAP: f32 = 8.0;

/// The height of each row of a group: at least 52 px, growing with its label and hint and keeping the row padding
/// of the design above and below the text (a two-line hint or the list of shortcuts makes a taller row, never a
/// cramped one).
fn setting_row_heights(f: &Frame, w: f32, rows: &[SettingRow]) -> Vec<f32> {
    let p = f.p;
    let bw = p.hairline();
    rows.iter()
        .map(|row| {
            let (label, hint, control_w) = row_parts(p, row);
            if row_stacked(f, row) {
                let text_w = w - 2.0 * bw - 36.0;
                let l = p.label_in(label, text::BODY, text_w, Align::Start).h;
                let h = hint.map_or(0.0, |h| p.label_in(h, text::SMALL, text_w, Align::Start).h);
                return STACKED_PAD + l + h + STACKED_GAP + layout::FIELD_H + STACKED_PAD;
            }
            let text_w = w - 2.0 * bw - 36.0 - control_w - 16.0;
            let l = p.label_in(label, text::BODY, text_w, Align::Start).h;
            let h = hint.map_or(0.0, |h| p.label_in(h, text::SMALL, text_w, Align::Start).h);
            (l + h + 2.0 * SETTING_ROW_PAD).max(52.0)
        })
        .collect()
}

/// What [`settings_group`] returns for the same group, without painting it.
fn settings_group_height(f: &Frame, w: f32, title: &str, rows: &[SettingRow]) -> f32 {
    let bw = f.p.hairline();
    f.p.label(title, text::H2).h + 10.0 + 2.0 * bw + setting_row_heights(f, w, rows).iter().sum::<f32>()
}

fn settings_group(f: &mut Frame, x: f32, y: f32, w: f32, title: &str, rows: &[SettingRow]) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let h2 = p.label(title, text::H2);
    p.draw(&h2, x, y, pal.ink);
    let top = y + h2.h + 10.0;
    let heights = setting_row_heights(f, w, rows);
    let panel_h = 2.0 * bw + heights.iter().sum::<f32>();
    f.panel(Rect::new(x, top, w, panel_h), pal.surface);
    let mut ry = top + bw;
    for (n, row) in rows.iter().enumerate() {
        let row_h = heights[n];
        if n > 0 {
            p.fill_rect(Rect::new(x + bw, ry, w - 2.0 * bw, bw), pal.line);
        }
        let (label, hint, control_w) = row_parts(p, row);
        let stacked = row_stacked(f, row);
        let lx = x + bw + 18.0;
        let text_w = if stacked { w - 2.0 * bw - 36.0 } else { w - 2.0 * bw - 36.0 - control_w - 16.0 };
        let l = p.label_in(label, text::BODY, text_w, Align::Start);
        let hl = hint.map(|h| p.label_in(h, text::SMALL, text_w, Align::Start));
        let block_h = l.h + hl.as_ref().map_or(0.0, |h| h.h);
        let ty = if stacked { ry + STACKED_PAD } else { ry + (row_h - block_h).max(0.0) / 2.0 };
        p.draw(&l, lx, ty, pal.ink);
        if let Some(h) = &hl {
            p.draw(h, lx, ty + l.h, pal.soft);
        }
        let cx = if stacked { lx } else { x + w - bw - 18.0 - control_w };
        // A field or a list: beside the text and centred on the row, or under the text and as wide as it.
        let field = |width: f32| {
            if stacked {
                Rect::new(cx, ty + block_h + STACKED_GAP, text_w, layout::FIELD_H)
            } else {
                Rect::new(cx, ry + (row_h - layout::FIELD_H) / 2.0, width, layout::FIELD_H)
            }
        };
        match row {
            SettingRow::Edit { value, width, .. } => {
                f.edit_field(EditId::Name, field(*width), value, "");
            }
            SettingRow::Select { id, value, width, .. } => {
                f.select_field(ControlId::Select(*id), field(*width), value);
            }
            SettingRow::Toggle { id, on, .. } => f.switch(*id, cx, ry + (row_h - layout::SWITCH_H) / 2.0, *on, false),
            SettingRow::Status { value: Some((s, c)), .. } => {
                let sl = p.label(s, text::NAME);
                p.draw(&sl, cx, ry + (row_h - sl.h) / 2.0, *c);
            }
            SettingRow::Status { .. } => {}
            SettingRow::Button { id, button, .. } => {
                let bw = f.button_width(button, Kind::Default);
                f.button(*id, Rect::new(cx, ry + (row_h - 40.0) / 2.0, bw, 40.0), Kind::Default, button, false);
            }
        }
        ry += row_h;
    }
    h2.h + 10.0 + panel_h
}

/// Label, hint and the width of the control at the row's end.
fn row_parts<'r>(p: &dyn crate::painter::Painter, row: &'r SettingRow) -> (&'r str, Option<&'r str>, f32) {
    match row {
        SettingRow::Edit { label, hint, width, .. } => (label.as_str(), hint.as_deref(), *width),
        SettingRow::Select { label, hint, width, .. } => (label.as_str(), hint.as_deref(), *width),
        SettingRow::Toggle { label, hint, .. } => (label.as_str(), hint.as_deref(), layout::SWITCH_W),
        SettingRow::Status { label, hint, value, .. } => {
            (label.as_str(), hint.as_deref(), value.as_ref().map_or(0.0, |(s, _)| p.label(s, text::NAME).w))
        }
        SettingRow::Button { label, hint, button, .. } => {
            let bw = p.label(button, crate::view::button_style(Kind::Default)).w + 32.0 + 2.0 * p.hairline();
            (label.as_str(), hint.as_deref(), bw)
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Pareamento

/// One block of the pairing card, in the order the Mac app stacks them (`swapdesk-shell-mac` `blocks_from`).
enum PairItem {
    Title(String),
    Body(String),
    Code(String),
    Countdown(u16),
    Buttons(Vec<(ControlId, String, Kind)>),
}

/// The pairing screen: one centred card with its content centred (D51), the same on Windows and macOS (owner,
/// 09/11: one interface on both systems). Card 520 wide, margins 32 and 28, 14 between blocks.
fn pairing(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let Some(view) = f.st.pairing().cloned() else { return };
    p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), pal.canvas);
    let msg = |f: &Frame, key: MsgKey, arg: &str| f.st.tm(&Msg::new(key).with(arg.to_string()));
    let items: Vec<PairItem> = match &view.state {
        PairingState::ShowCode { code, peer_name } => {
            let mut items = vec![
                PairItem::Title(msg(f, MsgKey::PairTitleWith, peer_name)),
                PairItem::Body(msg(f, MsgKey::PairCompare, peer_name)),
                PairItem::Code(code.clone()),
            ];
            match f.st.pair_wait() {
                // This side said the codes match: the card says what it waits for and keeps the way out.
                Some(wait) => {
                    items.push(PairItem::Body(wait.body));
                    items.push(PairItem::Buttons(
                        wait.buttons
                            .iter()
                            .map(|b| (b.id, b.label.clone(), if b.primary { Kind::Primary } else { Kind::Default }))
                            .collect(),
                    ));
                }
                None => {
                    items.push(PairItem::Body(msg(f, MsgKey::PairWarn, peer_name)));
                    items.push(PairItem::Buttons(vec![
                        (ControlId::PairSame, f.t(MsgKey::PairSame), Kind::Primary),
                        (ControlId::PairDifferent, f.t(MsgKey::PairDifferent), Kind::Default),
                    ]));
                }
            }
            items
        }
        PairingState::Open { remaining_s } => vec![
            PairItem::Title(f.t(MsgKey::PairThisComputer)),
            PairItem::Body(f.t(MsgKey::PairOpenBody)),
            PairItem::Countdown(*remaining_s),
            PairItem::Body(f.t(MsgKey::PairSearching)),
            PairItem::Buttons(vec![(ControlId::PairCancel, f.t(MsgKey::PairCancel), Kind::Default)]),
        ],
        PairingState::Waiting => vec![
            PairItem::Title(f.t(MsgKey::PairWaitingTitle)),
            PairItem::Body(f.t(MsgKey::PairWaitingBody)),
            PairItem::Buttons(vec![(ControlId::PairCancel, f.t(MsgKey::PairCancel), Kind::Default)]),
        ],
        PairingState::Done { device_id } => {
            let who = f.st.device_by_id(device_id).map(|d| d.name.clone()).unwrap_or_default();
            vec![
                PairItem::Title(msg(f, MsgKey::PairDoneTitle, &who)),
                PairItem::Body(f.t(MsgKey::PairDoneBody)),
                PairItem::Buttons(vec![(ControlId::PairDone, f.t(MsgKey::PairDoneButton), Kind::Primary)]),
            ]
        }
        // This phone has no secure screen lock: what to set, the way to the system's settings and the way out.
        PairingState::Failed { reason: swapdesk_ui::ApiCode::NeedsScreenLock } => {
            let (title, body, button) = f.st.notice_texts(NoticeKind::ScreenLock);
            vec![
                PairItem::Title(title),
                PairItem::Body(body),
                PairItem::Buttons(vec![
                    (ControlId::Notice(NoticeKind::ScreenLock), button, Kind::Primary),
                    (ControlId::PairClose, f.t(MsgKey::PairClose), Kind::Default),
                ]),
            ]
        }
        // Trying again changes nothing until the computer is updated: the way out is the only button.
        PairingState::Failed { reason: swapdesk_ui::ApiCode::NeedsUpdate } => vec![
            PairItem::Title(f.t(MsgKey::NeedsUpdateTitle)),
            PairItem::Body(f.t(MsgKey::PairNeedsUpdateBody)),
            PairItem::Buttons(vec![(ControlId::PairClose, f.t(MsgKey::PairClose), Kind::Primary)]),
        ],
        PairingState::Failed { reason } => vec![
            PairItem::Title(f.t(MsgKey::PairFailedTitle)),
            PairItem::Body(format!(
                "{} {}",
                f.st.error_text(&swapdesk_ui::ApiError::new(*reason)),
                f.t(MsgKey::PairFailedBody)
            )),
            PairItem::Buttons(vec![
                (ControlId::PairRetry, f.t(MsgKey::PairRetry), Kind::Primary),
                (ControlId::PairClose, f.t(MsgKey::PairClose), Kind::Default),
            ]),
        ],
    };

    const GAP: f32 = 14.0;
    let card_w = layout::DIALOG_W.min(f.w - 32.0);
    // Margins 32 at the sides; 24 in the compact layout, where the card is as wide as the window allows.
    let pad_x = if f.compact { 24.0 } else { 32.0 };
    let inner = card_w - 2.0 * pad_x;
    let height = |f: &Frame, it: &PairItem| -> f32 {
        match it {
            PairItem::Title(t) => f.para_height(t, text::H1, inner),
            PairItem::Body(t) => f.para_height(t, text::BODY, inner),
            PairItem::Code(c) => p.label(c, code_style(f, c, inner)).h,
            PairItem::Countdown(r) => p.label(&format::countdown(*r), text::COUNTDOWN).h,
            PairItem::Buttons(list) => card::actions_height(f, inner, list),
        }
    };
    let content: f32 = items.iter().map(|it| height(f, it)).sum::<f32>() + GAP * (items.len().saturating_sub(1)) as f32;
    let card_h = content + 56.0;
    let card = Rect::new((f.w - card_w) / 2.0, ((f.h - card_h) / 2.0).max(24.0), card_w, card_h);
    p.bordered(card, layout::DIALOG_RADIUS, pal.line, pal.surface);
    let x = card.x + pad_x;
    let mut y = card.y + 28.0;
    for it in &items {
        let h = height(f, it);
        match it {
            PairItem::Title(t) => {
                f.para(t, text::H1, x, y, inner, pal.ink, Align::Center);
            }
            PairItem::Body(t) => {
                f.para(t, text::BODY, x, y, inner, pal.soft, Align::Center);
            }
            PairItem::Code(c) => {
                let code = p.label(c, code_style(f, c, inner));
                p.draw(&code, x + (inner - code.w) / 2.0, y, pal.link);
            }
            PairItem::Countdown(r) => {
                let num = p.label(&format::countdown(*r), text::COUNTDOWN);
                p.draw(&num, x, y, pal.ink);
                let frac = (f32::from(*r) / f32::from(layout::PAIR_COUNTDOWN_S)).clamp(0.0, 1.0);
                let (bx, by) = (x + num.w + 14.0, y + num.h / 2.0 - 3.0);
                let bw = (inner - num.w - 14.0).max(40.0);
                p.fill_rrect(Rect::new(bx, by, bw, 6.0), 3.0, pal.line);
                p.fill_rrect(Rect::new(bx, by, bw * frac, 6.0), 3.0, pal.link);
            }
            PairItem::Buttons(list) => card::actions(f, x, y, inner, list, false),
        }
        y += h + GAP;
    }
}

/// The style of the pairing code: the design's 64 px, or in the compact layout the largest of its smaller steps that
/// keeps the code on one line inside the card.
fn code_style(f: &Frame, code: &str, inner: f32) -> swapdesk_ui::tokens::TextStyle {
    let mut style = text::PAIR_CODE;
    while f.compact && style.size > 32.0 && f.p.label(code, style).w > inner {
        style.size -= 8.0;
    }
    style
}

// ---------------------------------------------------------------------------------------------------------------
// Dialogs

/// The width of a dialog's card and its padding: 520 with 24 left at each side of the window and padding 32; in the
/// compact layout the window's margin at each side and padding 24.
fn dialog_box(f: &Frame) -> (f32, f32) {
    if f.compact {
        (layout::DIALOG_W.min(f.w - 2.0 * crate::screens_compact::MARGIN), 24.0)
    } else {
        (layout::DIALOG_W.min(f.w - 48.0), 32.0)
    }
}

/// The dialog card (padding 32, radius 14, shadow) centred over a backdrop; returns its rectangle given the content
/// height.
fn card(f: &Frame, content_h: f32) -> Rect {
    let (w, pad) = dialog_box(f);
    let h = content_h + 2.0 * pad;
    let r = Rect::new(f.p.snap((f.w - w) / 2.0), f.p.snap(((f.h - h) / 2.0).max(16.0)), w, h);
    if f.st.elevated_confirm().is_some() {
        // Alone in its own window the card sits on the window's canvas with a hairline: a dialog's shadow needs the
        // room of a screen behind it.
        f.p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), f.pal.canvas);
        f.p.bordered(r, layout::DIALOG_RADIUS, f.pal.line, f.pal.surface);
        return r;
    }
    f.p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), f.pal.remote);
    f.p.shadow(r, layout::DIALOG_RADIUS, 30.0, 80.0, f.fx.shadow_dialog);
    f.p.fill_rrect(r, layout::DIALOG_RADIUS, f.pal.surface);
    r
}

/// The incoming-session dialog (Pedido de conexão).
fn incoming(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let Some(i) = f.st.incoming().cloned() else { return };
    let (card_w, pad) = dialog_box(f);
    let w = card_w - 2.0 * pad;
    let title_s = f.st.incoming_title(&i);
    let sub_s = f.st.tm(&Msg::new(MsgKey::IncomingSub).with(i.remaining_s.to_string()));
    // The request itself is centred (D51); the choices under it are a form and keep their columns.
    let title = p.label_in(&title_s, text::H1_DIALOG, w, Align::Center);
    let sub = p.label_in(&sub_s, text::BODY, w, Align::Center);
    let legend = p.label(&f.t(MsgKey::AllowThisSession), text::NAME);
    let boxes = f.st.incoming_boxes();
    // The choices in two columns; one under the other in the compact layout, where two do not hold their texts.
    let cols = if f.compact { 1 } else { 2 };
    let rows = boxes.len().div_ceil(cols);
    let colw = if cols == 1 { w } else { (w - 16.0) / 2.0 };
    // A row of the grid is as tall as its tallest box: 32, more where a label wraps (the compact layout).
    let box_h: Vec<f32> = boxes.iter().map(|(_, key)| f.checkbox_height(colw, &f.t(*key))).collect();
    let row_h: Vec<f32> = box_h.chunks(cols).map(|row| row.iter().copied().fold(0.0, f32::max)).collect();
    let boxes_h = row_h.iter().sum::<f32>() + (rows as f32 - 1.0) * 4.0;
    let grid_h = legend.h + 8.0 + boxes_h;
    let remember = f.t(MsgKey::RememberUnattended);
    let remember_h = f.checkbox_height(w, &remember);
    let icon_h = 44.0;
    // The buttons as one centred group: "Recusar" first, the two that let the other side in after it.
    let list = [
        (ControlId::Decline, f.t(MsgKey::Decline), Kind::Default),
        (ControlId::ViewOnlyAccept, f.t(MsgKey::ViewOnlyAccept), Kind::Default),
        (ControlId::Accept, f.t(MsgKey::Accept), Kind::Primary),
    ];
    let content_h = icon_h
        + 24.0
        + (title.h + 8.0 + sub.h)
        + 24.0
        + grid_h
        + 24.0
        + (16.0 + 1.0 + remember_h)
        + 24.0
        + card::actions_height(f, w, &list);
    let r = card(f, content_h);
    let (x, mut y) = (r.x + pad, r.y + pad);

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
    for (n, (perm, key)) in boxes.iter().enumerate() {
        let (col, row) = (n % cols, n / cols);
        let above: f32 = row_h[..row].iter().map(|h| h + 4.0).sum();
        f.checkbox(
            ControlId::Allow(*perm),
            x + col as f32 * (colw + 16.0),
            y + above,
            colw,
            i.allow.has(*perm),
            &f.t(*key),
        );
    }
    y += boxes_h + 24.0;
    p.fill_rect(Rect::new(x, y, w, p.hairline()), pal.line);
    y += 1.0 + 16.0;
    f.checkbox(ControlId::RememberUnattended, x, y, w, i.remember_unattended, &remember);
    y += remember_h + 24.0;
    card::actions(f, x, y, w, &list, false);
}

/// Consent card, elevation step and swap dialogs: an icon, a title, text, an optional line and the buttons, all
/// centred in the card (D51).
fn dialog(f: &mut Frame, kind: &DialogKind) {
    let (p, pal) = (f.p, f.pal);
    let t = f.st.dialog_texts(kind);
    let (card_w, pad) = dialog_box(f);
    let w = card_w - 2.0 * pad;
    let title = p.label_in(&t.title, text::H1_DIALOG, w, Align::Center);
    let bodies: Vec<_> = t.body.iter().map(|b| p.label_in(b, text::BODY, w, Align::Center)).collect();
    // The elevation step's line has a turning arc before it: the two are centred as one.
    let arc = matches!(kind, DialogKind::Elevation);
    let extra = t.extra.as_ref().map(|e| {
        if arc {
            p.label_in(e, text::CAPTION, w - 28.0, Align::Start)
        } else {
            p.label_in(e, text::CAPTION, w, Align::Center)
        }
    });
    let mut content_h = 44.0 + 20.0 + title.h;
    for b in &bodies {
        content_h += 12.0 + b.h;
    }
    if let Some(e) = &extra {
        content_h += 16.0 + e.h;
    }
    let mut list = Vec::new();
    if let Some(s) = &t.secondary {
        list.push((ControlId::DialogSecondary, s.clone(), Kind::Default));
    }
    if let Some(pm) = &t.primary {
        list.push((ControlId::DialogPrimary, pm.clone(), Kind::Primary));
    }
    content_h += 28.0 + card::actions_height(f, w, &list);
    let r = card(f, content_h);
    let (x, mut y) = (r.x + pad, r.y + pad);
    let icon = match kind {
        DialogKind::Elevation | DialogKind::ElevatedConfirm => icons::SHIELD.to_owned(),
        DialogKind::Consent => icons::terminal(),
        DialogKind::SwapConfirm | DialogKind::SwapAsk => icons::SWAP.to_owned(),
        // Shown over the session window, never here.
        DialogKind::EndSession => icons::FOLDER.to_owned(),
    };
    let cx = x + w / 2.0;
    p.fill_circle((cx, y + 22.0), 22.0, pal.link_tint);
    p.svg(&icon, cx - 11.0, y + 11.0, 22.0, 2.0, pal.link);
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
        if arc {
            let gx = x + (w - 28.0 - e.w) / 2.0;
            p.arc_stroke((gx + 10.0, y + e.h / 2.0), 8.0, -90.0, 270.0, 2.5, pal.link);
            p.draw(e, gx + 28.0, y, pal.ink);
        } else {
            p.draw(e, x, y, pal.ink);
        }
        y += e.h;
    }
    y += 28.0;
    card::actions(f, x, y, w, &list, false);
}
