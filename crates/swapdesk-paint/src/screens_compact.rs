//! The compact layout of the main window: the same screens, texts and controls as the desktop's, one pane at a time
//! under a bar, for a touched window smaller than the smallest a desktop shell allows (a phone, upright or sideways;
//! `crate::is_compact`).
//!
//! The first pane is Início with the lists the desktop keeps in its sidebar: this device, its notices, the paired
//! computers and those on the network, then the cards. A computer's page, Atividade and Ajustes come over it with a
//! back arrow in the bar. Each pane is one column that scrolls as a whole under the bar; nothing inside it scrolls
//! on its own, because a finger cannot tell two nested scrolling areas apart.
//!
//! The page functions are `screens_main`'s: they read [`Frame::compact`] where the arrangement differs (what stands
//! side by side on a desktop is stacked here) and return where their content ends, which is how the pane's height is
//! known before it is drawn: the page is laid out once against a painter that draws nothing, then painted at its
//! scroll offset.

use swapdesk_ui::tokens::{Color, TextStyle, layout, text};
use swapdesk_ui::{ControlId, DataKind, DeviceStatus, DeviceView, EditId, Fetch, Msg, MsgKey, Nav, format};

use crate::card::{self, Card, Icon, Look};
use crate::painter::{Align, Label, Painter, Rect};
use crate::screens_main::{self as main, Area, area, icons20, room};
use crate::view::{Frame, Kind, icons};

/// The bar across the top: the logo and Ajustes on the first pane, the way back on the others.
pub(crate) const BAR_H: f32 = 56.0;
/// The margin of the column at the window's sides, and of the content under the bar and above the bottom edge.
pub(crate) const MARGIN: f32 = 16.0;
/// The widest the column gets: a phone held sideways shows it centred, not stretched.
pub(crate) const COLUMN_MAX: f32 = 560.0;

/// The scrolling contents of the compact main window (`Output::scroll_key`); 1 and 2 are the desktop's.
const SCROLL_HOME: u8 = 3;
const SCROLL_DEVICE: u8 = 4;
const SCROLL_ACTIVITY: u8 = 5;
const SCROLL_SETTINGS: u8 = 6;
const SCROLL_STOPPED: u8 = 7;

/// A row of the activity log: the device and the time, then two lines of what happened.
const ACTIVITY_ROW_H: f32 = 84.0;

/// The way back, and the mark that a row opens a page (24-unit icons).
const ARROW_BACK: &str = "M19 12H5M11 6l-6 6 6 6";
const CHEVRON_RIGHT: &str = "M9 6l6 6-6 6";

/// A painter that lays text out and draws nothing: a page painted on it only says how tall it is.
struct Quiet<'a>(&'a dyn Painter);

impl Painter for Quiet<'_> {
    fn scale(&self) -> f32 {
        self.0.scale()
    }
    fn clear(&self, _: Color) {}
    fn fill_rect(&self, _: Rect, _: Color) {}
    fn fill_rrect(&self, _: Rect, _: f32, _: Color) {}
    fn stroke_rrect(&self, _: Rect, _: f32, _: f32, _: Color) {}
    fn stroke_rrect_dashed(&self, _: Rect, _: f32, _: f32, _: Color) {}
    fn line(&self, _: (f32, f32), _: (f32, f32), _: f32, _: Color, _: bool) {}
    fn fill_circle(&self, _: (f32, f32), _: f32, _: Color) {}
    fn stroke_circle(&self, _: (f32, f32), _: f32, _: f32, _: Color) {}
    fn arc_stroke(&self, _: (f32, f32), _: f32, _: f32, _: f32, _: f32, _: Color) {}
    fn fill_triangle(&self, _: [(f32, f32); 3], _: Color) {}
    fn svg(&self, _: &str, _: f32, _: f32, _: f32, _: f32, _: Color) {}
    fn shadow(&self, _: Rect, _: f32, _: f32, _: f32, _: Color) {}
    fn push_clip(&self, _: Rect) {}
    fn pop_clip(&self) {}
    fn content_height(&self, st: TextStyle) -> f32 {
        self.0.content_height(st)
    }
    fn layout(&self, text: &str, st: TextStyle, max_w: f32, align: Align, ellipsis: bool, bold_chars: usize) -> Label {
        self.0.layout(text, st, max_w, align, ellipsis, bold_chars)
    }
    fn draw(&self, _: &Label, _: f32, _: f32, _: Color) {}
}

/// The first pane: Início, and a device page with no device to show.
fn at_home(f: &Frame) -> bool {
    match f.st.nav() {
        Nav::Home => true,
        Nav::Devices => !f.st.placeholder(DataKind::Devices) && f.st.selected_device().is_none(),
        Nav::Activity | Nav::Settings => false,
    }
}

/// The pane of the state. Returns where its content ends.
fn page(f: &mut Frame) -> f32 {
    match f.st.nav() {
        Nav::Home => home(f),
        Nav::Devices => main::devices_main(f),
        Nav::Activity => main::activity_main(f),
        Nav::Settings => main::settings_main(f),
    }
}

/// Puts a page in `view`, scrolled by `scroll`: where its content starts, and the line a card that stands alone is
/// centred above (the bottom of what is seen with nothing scrolled).
fn place(f: &mut Frame, view: Rect, scroll: f32) {
    f.page_top = view.y + MARGIN - scroll;
    f.page_floor = view.bottom() - MARGIN - scroll;
}

/// The compact main window: the pane under its bar.
pub(crate) fn paint(f: &mut Frame) {
    let view = Rect::new(0.0, BAR_H, f.w, (f.h - BAR_H).max(0.0));
    let key = match f.st.nav() {
        _ if at_home(f) => SCROLL_HOME,
        Nav::Activity => SCROLL_ACTIVITY,
        Nav::Settings => SCROLL_SETTINGS,
        _ => SCROLL_DEVICE,
    };
    let scroll = pane(f, view, key, page);
    bar(f, scroll > 0.0);
    main::error_toast(f);
}

/// The stopped screen in the compact layout: its one card in a pane that takes the whole window, with no bar
/// (there is nowhere to go), and that scrolls when the screen is too short for the card (a phone held sideways).
pub(crate) fn stopped(f: &mut Frame) {
    let view = Rect::new(0.0, 0.0, f.w, f.h);
    pane(f, view, SCROLL_STOPPED, main::stopped_page);
}

/// Paints `content` as the pane that scrolls in `view` under the scroll key `key`. Returns the offset it is at.
fn pane(f: &mut Frame, view: Rect, key: u8, content: fn(&mut Frame) -> f32) -> f32 {
    // The height of the pane, from a pass that draws nothing. It is laid out at the offset the window asks for, so
    // a list that only lays out its rows in sight measures the same rows this frame will draw.
    let asked = if f.ix.scroll_key == key { f.ix.scroll.max(0.0) } else { 0.0 };
    let height = {
        let quiet = Quiet(f.p);
        let mut dry = Frame::new(&quiet, f.st, f.ix, (f.w, f.h), f.offscreen);
        dry.compact = true;
        place(&mut dry, view, asked);
        content(&mut dry) + asked - view.y + MARGIN
    };
    let scroll = f.page_scroll(key, (height - view.h).max(0.0));
    f.out.page_view = Some(view);

    f.p.push_clip(view);
    f.touch_clip = Some(view);
    place(f, view, scroll);
    content(f);
    f.touch_clip = None;
    f.p.pop_clip();
    scroll
}

/// The bar: on the first pane the logo and Ajustes; on the others the way back, and "Renomear" on a computer's page.
/// It is drawn after the pane, over what scrolled under it, with a line under it then.
fn bar(f: &mut Frame, lifted: bool) {
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    p.fill_rect(Rect::new(0.0, 0.0, f.w, BAR_H), pal.canvas);
    if lifted {
        let bw = p.hairline();
        p.fill_rect(Rect::new(0.0, BAR_H - bw, f.w, bw), pal.line);
    }
    // A 40 px button whose 20 px icon stands on the column's edge.
    let button_y = (BAR_H - 40.0) / 2.0;
    if at_home(f) {
        let logo = p.label(&f.t(MsgKey::AppName), text::LOGO);
        main::logo_mark(f, a.x, (BAR_H - 22.0) / 2.0);
        p.draw(&logo, a.x + 22.0 + 10.0, (BAR_H - logo.h) / 2.0, pal.ink);
        let r = Rect::new(a.x + a.cw - 30.0, button_y, 40.0, 40.0);
        f.tint(ControlId::NavSettings, r, 8.0, pal.ink, 0.06, 0.11);
        main::icon20(f, icons20::GEAR, r.x + 10.0, r.y + 10.0, 20.0, pal.ink);
        f.register(ControlId::NavSettings, r, 8.0);
        return;
    }
    let r = Rect::new(a.x - 10.0, button_y, 40.0, 40.0);
    f.tint(ControlId::NavHome, r, 8.0, pal.ink, 0.06, 0.11);
    p.svg(ARROW_BACK, r.x + 10.0, r.y + 10.0, 20.0, 2.0, pal.ink);
    f.register(ControlId::NavHome, r, 8.0);
    if rename_offered(f) {
        let rename = f.t(MsgKey::RenameAction);
        let w = f.button_width(&rename, Kind::Quiet);
        let r = Rect::new(a.x + a.cw - w + 16.0, button_y, w, 40.0);
        f.button(ControlId::RenameDevice, r, Kind::Quiet, &rename, false);
    }
}

/// "Renomear" is in the bar of a computer's page while the page shows that computer at rest: not while its name is
/// being typed, and not while a card of a wait takes the page.
fn rename_offered(f: &Frame) -> bool {
    f.st.nav() == Nav::Devices
        && !f.st.placeholder(DataKind::Devices)
        && f.st.renaming().is_none()
        && f.st.selected_device().is_some_and(|d| f.st.rename_available(d) && f.st.wait_card(&d.id).is_none())
}

// ---------------------------------------------------------------------------------------------------------------
// Início

/// The first pane: this device and its network, the notices, the computers, the cards.
pub(crate) fn home(f: &mut Frame) -> f32 {
    let a = area(f);
    let mut y = main::home_header(f, &a, f.page_top);
    y += main::home_notices(f, &a, y);
    y = device_groups(f, &a, y);
    main::home_cards(f, &a, y)
}

/// "Pareados" and "Na rede", the lists of the desktop's sidebar: each a caption over a panel of rows, one per
/// computer, that open its page. A list with nothing in it is left out. Returns where the page goes on.
fn device_groups(f: &mut Frame, a: &Area, mut y: f32) -> f32 {
    let loading = f.st.placeholder(DataKind::Devices);
    if !loading && f.st.fetch(DataKind::Devices) != Fetch::Ready && f.st.devices().is_empty() {
        return y;
    }
    let (paired, network) = f.st.device_groups();
    for (title, list, skeleton) in [(MsgKey::SidebarPaired, paired, 2), (MsgKey::SidebarOnNetwork, network, 1)] {
        let rows = if loading { skeleton } else { list.len() };
        if rows == 0 {
            continue;
        }
        let (p, pal) = (f.p, f.pal);
        let bw = p.hairline();
        let caption = p.label(&f.t(title), text::GROUP);
        p.draw(&caption, a.x, y, pal.soft);
        y += caption.h + 8.0;
        let row_h = 10.0 + p.label("A", text::NAME).h + p.label("A", text::SMALL).h + 10.0;
        let panel = Rect::new(a.x, y, a.cw, 2.0 * bw + rows as f32 * row_h);
        f.panel(panel, pal.surface);
        for n in 0..rows {
            let r = Rect::new(panel.x + bw, panel.y + bw + n as f32 * row_h, panel.w - 2.0 * bw, row_h);
            if n > 0 {
                p.fill_rect(Rect::new(r.x, r.y, r.w, bw), pal.line);
            }
            if loading {
                p.fill_circle((r.x + 18.0 + 4.0, r.y + row_h / 2.0), 4.0, pal.line.with_alpha(0.55));
                f.block(Rect::new(r.x + 42.0, r.y + 13.0, (r.w - 100.0).min(150.0), 12.0), 4.0);
                f.block(Rect::new(r.x + 42.0, r.y + 33.0, (r.w - 140.0).min(100.0), 10.0), 4.0);
            } else if let Some(&index) = list.get(n) {
                device_row(f, r, index);
            }
        }
        y = panel.bottom() + 24.0;
    }
    y
}

/// One computer of a list: its dot, its name over its state, and the mark that the row opens its page.
fn device_row(f: &mut Frame, r: Rect, index: usize) {
    let (p, pal) = (f.p, f.pal);
    let d = &f.st.devices()[index];
    let id = ControlId::Device(index as u16);
    f.tint(id, r, layout::PANEL_RADIUS - 2.0, pal.ink, 0.06, 0.11);
    let center = (r.x + 18.0 + 4.0, r.y + r.h / 2.0);
    match d.status {
        DeviceStatus::Online { .. } => p.fill_circle(center, 4.0, pal.ok),
        DeviceStatus::Asleep | DeviceStatus::Offline => p.fill_circle(center, 4.0, pal.line),
        DeviceStatus::OnNetworkUnpaired { .. } => p.stroke_circle(center, 4.0 - 0.75, 1.5, pal.soft),
    }
    let tx = r.x + 18.0 + 8.0 + 16.0;
    let tw = (r.right() - 14.0 - 16.0 - 12.0 - tx).max(40.0);
    let name = p.label_ellipsis(&d.name, text::NAME, tw);
    let status = p.label_ellipsis(&f.st.device_status_line(d), text::SMALL, tw);
    let ty = r.y + (r.h - name.h - status.h) / 2.0;
    p.draw(&name, tx, ty, pal.ink);
    p.draw(&status, tx, ty + name.h, pal.soft);
    p.svg(CHEVRON_RIGHT, r.right() - 14.0 - 16.0, r.y + (r.h - 16.0) / 2.0, 16.0, 2.0, pal.soft);
    f.register(id, r, layout::PANEL_RADIUS - 2.0);
}

// ---------------------------------------------------------------------------------------------------------------
// A computer's page

/// The header of a computer's page in the compact layout: its name across the page (the field with its two buttons
/// under it while it is renamed), its state line, and `actions` side by side under them, sharing the width.
/// "Renomear" is in the bar. Returns where the page goes on.
pub(crate) fn device_header(
    f: &mut Frame,
    a: &Area,
    mut y: f32,
    d: &DeviceView,
    actions: &[(ControlId, String, Kind)],
) -> f32 {
    let pal = f.pal;
    let detail = format::detail_line(f.st.language(), d);
    let renaming = f.st.renaming().filter(|_| f.st.rename_available(d)).cloned();
    if let Some(r) = &renaming {
        f.edit_field(
            EditId::Rename,
            Rect::new(a.x, y + (40.0 - layout::FIELD_H) / 2.0, a.cw, layout::FIELD_H),
            &d.name,
            "",
        );
        y += 40.0 + 8.0;
        // Negative first, positive last, at the page's right edge.
        let (save, cancel) = (f.t(MsgKey::RenameSave), f.t(MsgKey::Cancel));
        let (sw, cw) = (f.button_width(&save, Kind::Primary), f.button_width(&cancel, Kind::Default));
        let sx = a.x + a.cw - sw;
        f.button(ControlId::RenameCancel, Rect::new(sx - 8.0 - cw, y, cw, 40.0), Kind::Default, &cancel, false);
        f.button(ControlId::RenameSave, Rect::new(sx, y, sw, 40.0), Kind::Primary, &save, false);
        y += 40.0 + 8.0;
        // The refusal sits where the detail line is, in the danger colour.
        let (line, color) = match &r.error {
            Some(e) => (f.st.rename_error_text(e), pal.danger),
            None => (detail, pal.soft),
        };
        y += f.para(&line, text::BODY, a.x, y, a.cw, color, Align::Start);
    } else {
        y += f.para(&d.name, text::H1, a.x, y, a.cw, pal.ink, Align::Start) + 6.0;
        y += f.para(&detail, text::BODY, a.x, y, a.cw, pal.soft, Align::Start);
    }
    if !actions.is_empty() {
        y += 16.0;
        let gap = 12.0;
        let w = (a.cw - gap * (actions.len() - 1) as f32) / actions.len() as f32;
        for (n, (id, label, kind)) in actions.iter().enumerate() {
            f.button(*id, Rect::new(a.x + n as f32 * (w + gap), y, w, 40.0), *kind, label, false);
        }
        y += 40.0;
    }
    y + 24.0
}

// ---------------------------------------------------------------------------------------------------------------
// Atividade

/// The activity log in the compact layout: the title and what the log is, "Exportar" at the page's right edge, the
/// alert with its button under its text, then one row per entry: the device and the time, and under them what
/// happened (two lines) with how long it took. Rows have one height and only those in sight are laid out, since
/// the log can be long. Returns where the page ends.
pub(crate) fn activity(f: &mut Frame) -> f32 {
    let (p, pal) = (f.p, f.pal);
    let a = area(f);
    let mut y = f.page_top;
    y += f.para(&f.t(MsgKey::ActivityTitle), text::H1, a.x, y, a.cw, pal.ink, Align::Start) + 6.0;
    y += f.para(&f.t(MsgKey::ActivitySub), text::BODY, a.x, y, a.cw, pal.soft, Align::Start) + 16.0;
    let export = f.t(MsgKey::ExportLog);
    let ew = f.button_width(&export, Kind::Default);
    f.button(ControlId::Export, Rect::new(a.x + a.cw - ew, y, ew, 40.0), Kind::Default, &export, false);
    y += 40.0 + 24.0;

    if let Some(alert) = f.st.alert().cloned() {
        let addr = alert.what.args.first().cloned().unwrap_or_default();
        let block = f.st.tm(&Msg::new(MsgKey::BlockAddress).with(addr));
        let when = format::when_phrase(f.st.language(), alert.at_ms, f.st.clock()).to_lowercase();
        let first = f.st.tm(&Msg::new(MsgKey::AlertUnknownPair).with(when));
        let rest = f.t(MsgKey::AlertUnknownRest);
        let full = format!("{first} {rest}");
        let tw = (a.cw - 16.0 - 20.0 - 14.0 - 16.0).max(40.0);
        let l = p.label_rich(&full, text::BODY, tw, first.chars().count());
        let bw_btn = f.button_width(&block, Kind::Danger).min(a.cw - 32.0);
        let h = 14.0 + l.h + 12.0 + 40.0 + 14.0;
        let r = Rect::new(a.x, y, a.cw, h);
        p.fill_rrect(r, 12.0, pal.danger_tint);
        p.svg(icons::SHIELD_ALERT, r.x + 16.0, r.y + 14.0, 20.0, 2.0, pal.danger);
        p.draw(&l, r.x + 16.0 + 20.0 + 14.0, r.y + 14.0, pal.ink);
        let button = Rect::new(r.right() - 16.0 - bw_btn, r.y + 14.0 + l.h + 12.0, bw_btn, 40.0);
        f.button(ControlId::BlockAlert, button, Kind::Danger, &block, false);
        y += h + 24.0;
    }

    let total = f.st.activity().len();
    let loading = f.st.placeholder(DataKind::Activity);
    if f.st.fetch(DataKind::Activity) == Fetch::Pending {
        return y;
    }
    if !loading && total == 0 {
        // Nothing logged yet: the page says so, alone and centred (D51), instead of a list with no rows.
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
    let bw = p.hairline();
    let count = if loading { 6 } else { total };
    let panel = Rect::new(a.x, y, a.cw, 2.0 * bw + count as f32 * ACTIVITY_ROW_H);
    f.panel(panel, pal.surface);
    let (x, w) = (panel.x + bw + 16.0, panel.w - 2.0 * bw - 32.0);
    let list_y = panel.y + bw;
    // The rows between the bar and the window's bottom edge.
    let row_at = |edge: f32| (((edge - list_y) / ACTIVITY_ROW_H).max(0.0) as usize).min(count);
    for n in row_at(BAR_H)..(row_at(f.h) + 1).min(count) {
        let ry = list_y + n as f32 * ACTIVITY_ROW_H;
        if n > 0 {
            p.fill_rect(Rect::new(panel.x + bw, ry, panel.w - 2.0 * bw, bw), pal.line);
        }
        if loading {
            f.block(Rect::new(x, ry + 16.0, (w * 0.5).min(160.0), 14.0), 4.0);
            f.block(Rect::new(x, ry + 42.0, w * 0.8, 12.0), 4.0);
            continue;
        }
        let row = &f.st.activity()[n];
        let when = p.label(&format::table_when(f.st.language(), row.at_ms, f.st.clock()), text::SMALL);
        let who = p.label_ellipsis(&f.st.activity_who(row), text::NAME, (w - when.w - 12.0).max(40.0));
        p.draw(&who, x, ry + 12.0, if row.alert { pal.danger } else { pal.ink });
        p.draw(&when, x + w - when.w, ry + 12.0 + (who.h - when.h) / 2.0, pal.soft);
        let duration = f.st.duration_cell(row);
        let took = (!duration.is_empty()).then(|| p.label(&duration, text::SMALL));
        let what_w = (w - took.as_ref().map_or(0.0, |t| t.w + 12.0)).max(40.0);
        let what = p.label_in(&f.st.activity_what(row), text::CAPTION, what_w, Align::Start);
        let what_y = ry + 12.0 + who.h + 2.0;
        // Two lines fit the row; a longer text is cut at the row's edge.
        p.push_clip(Rect::new(x, what_y, w, ry + ACTIVITY_ROW_H - 8.0 - what_y));
        p.draw(&what, x, what_y, pal.soft);
        p.pop_clip();
        if let Some(t) = &took {
            p.draw(t, x + w - t.w, what_y, pal.soft);
        }
    }
    panel.bottom()
}
