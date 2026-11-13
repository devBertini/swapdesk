//! The viewer window (Visualizador) and the host banner, drawn as a static shell: the video area is a placeholder
//! (its visual is stage 15), the terminal panel's grid is drawn by the window (stage 43). Toolbar, drawer, cards and the banner follow the design.

use swapdesk_ui::format;
use swapdesk_ui::tokens::{Color, TextStyle, layout, text};
use swapdesk_ui::{
    ActionKind, ControlId, DialogKind, DrawerTab, EditId, Msg, MsgKey, SessionState, SwapPhase, SwapWait, TermPhase,
    WindowKind,
};

use crate::card::{self, Card, Icon, Look};
use crate::painter::{Align, Rect};
use crate::view::{Frame, Kind, icons};

/// The viewer window.
pub fn paint_viewer(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    // With the picture in a view under the canvas, nothing may cover its rectangle: no clear, and the column is
    // filled around it below.
    if !f.video_under {
        p.clear(pal.canvas);
    }
    let Some(session) = f.st.viewer_session().cloned() else {
        if f.video_under {
            p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), pal.canvas);
        }
        return;
    };
    let peer = f.st.device_by_id(&session.device_id).map(|d| d.name.clone()).unwrap_or_default();

    // No top bar (owner, 12/11): who this is and for how long ride on the session toolbar, and the picture takes the
    // whole window, in full screen (D30) and out of it.
    let fullscreen = f.st.fullscreen();

    // The body: the video column and, when open, the drawer (448 px, border-left).
    let body = Rect::new(0.0, 0.0, f.w, f.h);
    // The Arquivos view takes the whole body: no picture (the shells hide it while `video` is none), no drawer.
    if f.st.explorer_open() {
        crate::screens_explorer::explorer(f, body);
        if let Some(kind) = f.st.dialog(WindowKind::Viewer) {
            f.out.widgets.clear();
            swap_confirm(f, &kind);
        }
        return;
    }
    let drawer = f.st.drawer();
    let video_col = Rect::new(0.0, body.y, if drawer.is_some() { body.w - layout::DRAWER_W } else { body.w }, body.h);
    let pad = if fullscreen {
        0.0
    } else if drawer.is_some() {
        20.0
    } else {
        48.0
    };
    // A real picture gets the whole column: the renderer keeps its proportion inside it, so it touches the window's
    // edges and only a band is left on one axis. A mock scenario and a capture keep the design's framed rectangle.
    let video = if f.st.live_picture() { video_col } else { fit_16_10(video_col.inset(pad)) };
    if f.video_under {
        for strip in around(video_col, video) {
            p.fill_rect(strip, pal.remote);
        }
    } else {
        p.fill_rect(video_col, pal.remote);
        p.fill_rrect(video, 4.0, f.fx.remote_screen);
        let ph = p.label_in(&f.t(MsgKey::VideoPlaceholder), text::BODY, video.w - 40.0, Align::Center);
        let (_, vy) = video.center();
        p.draw(&ph, video.x + 20.0, vy - ph.h / 2.0, f.fx.card_dark_soft);
    }
    f.out.video = Some(video);

    if let SessionState::Reconnecting { attempt } = session.state {
        reconnecting_card(f, video_col, &peer, attempt);
    } else {
        if let Some(wait) = f.st.swap_wait() {
            swap_wait_card(f, video, &wait);
        } else if let Some((body, elevate)) = f.st.paused_card() {
            paused_card(f, video, &body, elevate);
        } else if matches!(f.st.swap().map(|s| &s.phase), Some(SwapPhase::Declined(_))) {
            swap_declined_card(f, video);
        }
        if f.st.session_bar_visible() {
            let line = session_line(f, &session);
            toolbar(f, video_col, &peer, &line);
        } else if f.st.bar_handle_visible() {
            bar_handle(f, video_col);
        }
    }
    screen_note(f, video);
    // Slice 4: files dragged from the system over the picture: the pill and "Solte para enviar ao ..." (no outline).
    if let Some(d) = f.st.explorer_drag() {
        crate::screens_explorer::drag(f, &d);
    }
    if let Some(tab) = drawer {
        drawer_panel(f, Rect::new(body.w - layout::DRAWER_W, body.y, layout::DRAWER_W, body.h), tab, &peer);
    }
    // The swap confirmation covers the window.
    if let Some(kind) = f.st.dialog(WindowKind::Viewer) {
        f.out.widgets.clear();
        swap_confirm(f, &kind);
    }
}

/// The parts of `outer` not covered by `hole` (which lies inside it): above, below, left and right, without overlap.
/// Empty strips are left out.
/// The short line over the bottom of the picture saying why a drop on it did nothing (it goes by itself).
fn screen_note(f: &Frame, video: Rect) {
    let Some(note) = f.st.screen_note() else { return };
    let (p, pal) = (f.p, f.pal);
    let l = p.label_in(&note, text::CAPTION, (video.w - 64.0).clamp(40.0, 520.0), Align::Start);
    let x = p.snap(video.center().0 - (l.w + 32.0) / 2.0);
    let r = Rect::new(x, p.snap(video.bottom() - 28.0 - l.h - 16.0), l.w + 32.0, l.h + 16.0);
    p.shadow(r, 10.0, 4.0, 12.0, f.fx.shadow_dialog);
    p.bordered(r, 10.0, pal.line, pal.surface);
    p.draw(&l, r.x + 16.0, r.y + 8.0, pal.ink);
}

fn around(outer: Rect, hole: Rect) -> impl Iterator<Item = Rect> {
    [
        Rect::new(outer.x, outer.y, outer.w, hole.y - outer.y),
        Rect::new(outer.x, hole.bottom(), outer.w, outer.bottom() - hole.bottom()),
        Rect::new(outer.x, hole.y, hole.x - outer.x, hole.h),
        Rect::new(hole.right(), hole.y, outer.right() - hole.right(), hole.h),
    ]
    .into_iter()
    .filter(|r| r.w > 0.0 && r.h > 0.0)
}

fn fit_16_10(area: Rect) -> Rect {
    let w = area.w.min(area.h * 1.6);
    let h = w / 1.6;
    Rect::new(area.x + (area.w - w) / 2.0, area.y + (area.h - h) / 2.0, w, h)
}

// ----- toolbar -----

struct Tool {
    id: ControlId,
    icon: Option<String>,
    label: Option<String>,
    pressed: bool,
    color: Color,
    weight_bold: bool,
    chevron: bool,
    sep_before: bool,
}

/// The second line of the toolbar's session segment: how long this has gone on ("você controla há 68 min"), and the
/// latency, frame rate and codec when Ajustes shows them (the state composes it, so it is read with the toolbar).
fn session_line(f: &Frame, session: &swapdesk_ui::state::SessionView) -> String {
    f.st.viewer_session_line(session)
}

/// The toolbar's text styles of its session segment: the peer's name, then the session line, small and soft.
const SEG_NAME: TextStyle = TextStyle::new(12.0, 600);
const SEG_LINE: TextStyle = TextStyle::new(11.0, 400);

fn toolbar(f: &mut Frame, col: Rect, peer: &str, line: &str) {
    let (p, _pal) = (f.p, f.pal);
    let display = f.st.tm(&Msg::new(MsgKey::TbScreenN).with("1"));
    let white = f.fx.toolbar_ink;
    let tool = |id, icon: &str| Tool {
        id,
        icon: Some(icon.to_owned()),
        label: None,
        pressed: false,
        color: white,
        weight_bold: false,
        chevron: false,
        sep_before: false,
    };
    let mut tools = vec![
        Tool {
            id: ControlId::TbScreen,
            icon: None,
            label: Some(display),
            pressed: false,
            color: white,
            weight_bold: false,
            chevron: true,
            sep_before: false,
        },
        tool(ControlId::TbFit, icons::FIT),
    ];
    let mut audio = tool(ControlId::TbAudio, icons::AUDIO);
    audio.pressed = f.st.audio_on();
    audio.sep_before = true;
    let mut grab = tool(ControlId::TbGrab, &icons::keyboard());
    grab.pressed = f.st.grab_on();
    tools.extend([audio, grab, tool(ControlId::TbKeys, icons::KEYS_ARROW)]);
    let mut term = tool(ControlId::TbTerminal, &icons::terminal());
    term.pressed = f.st.drawer() == Some(DrawerTab::Terminal);
    term.sep_before = true;
    let mut files = tool(ControlId::TbFiles, icons::FOLDER);
    files.pressed = f.st.explorer_open();
    let mut chat = tool(ControlId::TbChat, icons::CHAT);
    chat.pressed = f.st.drawer() == Some(DrawerTab::Chat);
    tools.extend([term, files, chat]);
    // Swap between Windows and macOS is not in 1.0: "Inverter" is laid out only toward a machine of this OS, and
    // "Desconectar" then takes its separator, so the bar has no gap.
    let swap_offered = f.st.viewer_session().is_none_or(|v| f.st.swap_offered(v));
    if swap_offered {
        tools.push(Tool {
            id: ControlId::TbSwap,
            icon: Some(icons::SWAP.to_owned()),
            label: Some(f.t(MsgKey::TbSwap)),
            pressed: false,
            color: white,
            weight_bold: false,
            chevron: false,
            sep_before: true,
        });
    }
    // ACCOUNTS.md 9: an entry without a prompt offers its elevation, after the confirmation on this machine.
    if f.st.can_elevate_session() {
        tools.push(Tool {
            id: ControlId::TbElevate,
            icon: None,
            label: Some(f.t(MsgKey::ElevateButton)),
            pressed: false,
            color: white,
            weight_bold: false,
            chevron: false,
            sep_before: true,
        });
    }
    let mut collapse = tool(ControlId::TbCollapse, CHEVRON_UP);
    collapse.sep_before = true;
    tools.push(Tool {
        id: ControlId::TbDisconnect,
        icon: None,
        label: Some(f.t(MsgKey::Disconnect)),
        pressed: false,
        color: f.fx.toolbar_danger,
        weight_bold: true,
        chevron: false,
        sep_before: !swap_offered,
    });
    tools.push(collapse);

    // Measure: button = padding 10 + content + 10, min 40; content = icon 18 / label / chevron 14, gap 8.
    let style = |t: &Tool| if t.weight_bold { text::BTN_PRIMARY } else { text::BTN };
    let measure = |t: &Tool| {
        let mut parts: Vec<f32> = Vec::new();
        if t.icon.is_some() {
            parts.push(18.0);
        }
        if let Some(l) = &t.label {
            parts.push(p.label(l, style(t)).w);
        }
        if t.chevron {
            parts.push(14.0);
        }
        let content: f32 = parts.iter().sum::<f32>() + 8.0 * (parts.len().saturating_sub(1)) as f32;
        (content + 20.0).max(40.0)
    };
    let mut widths: Vec<f32> = tools.iter().map(measure).collect();
    // The buttons alone, with their gaps and separators, and the bar's padding.
    let seps = tools.iter().filter(|t| t.sep_before).count() as f32;
    let buttons_w =
        |widths: &[f32]| 12.0 + widths.iter().sum::<f32>() + 2.0 * (widths.len() as f32 - 1.0) + seps * 13.0;
    // The session segment at the leading edge: the connection dot, the peer's name and the session line, discreet,
    // then a separator (owner, 12/11: the line that was the window's top bar). Its frame around the text, and the
    // separator after it.
    const SEG_FRAME: f32 = 10.0 + 8.0 + 8.0 + 10.0;
    const SEG_SEP: f32 = 13.0;
    let text_w = p.label(peer, SEG_NAME).w.max(p.label(line, SEG_LINE).w);
    // The bar stays inside the picture's column (narrow with a side panel open): it gives up, in turn, the label of
    // "Inverter" (its icon stays), then the end of the name and of the session line, then the whole segment.
    let room = col.w - 24.0;
    if buttons_w(&widths) + SEG_FRAME + text_w + SEG_SEP > room
        && let Some(n) = tools.iter().position(|t| t.id == ControlId::TbSwap)
    {
        tools[n].label = None;
        widths[n] = measure(&tools[n]);
    }
    const SEG_TEXT_MIN: f32 = 72.0;
    let seg_text = text_w.min(room - buttons_w(&widths) - SEG_FRAME - SEG_SEP);
    let segment = (seg_text >= SEG_TEXT_MIN.min(text_w))
        .then(|| (p.label_ellipsis(peer, SEG_NAME, seg_text), p.label_ellipsis(line, SEG_LINE, seg_text)));
    let seg_w = segment.as_ref().map_or(0.0, |(n, s)| SEG_FRAME + n.w.max(s.w));
    let total = buttons_w(&widths) + if segment.is_some() { seg_w + SEG_SEP } else { 0.0 };
    let bar = Rect::new(p.snap(col.x + (col.w - total) / 2.0), col.y + 14.0, total, 52.0);
    p.shadow(bar, 12.0, 10.0, 30.0, f.fx.shadow_dialog);
    p.fill_rrect(bar, 12.0, f.fx.toolbar);
    let seg_x = bar.x + 6.0;
    let mut x = seg_x;
    if let Some((name, sub)) = &segment {
        p.fill_circle((seg_x + 10.0 + 4.0, bar.y + 26.0), 4.0, f.pal.ok);
        let text_x = seg_x + 10.0 + 8.0 + 8.0;
        let gap = 2.0;
        let top = bar.y + (52.0 - name.h - gap - sub.h) / 2.0;
        p.draw(name, text_x, top, f.fx.toolbar_ink);
        p.draw(sub, text_x, top + name.h + gap, f.fx.toolbar_soft);
        x += seg_w;
        p.fill_rect(Rect::new(x + 6.0, bar.y + 14.0, 1.0, 24.0), f.fx.toolbar_sep);
        x += SEG_SEP;
    }
    let mut pointed = None;
    for (n, (t, w)) in tools.iter().zip(&widths).enumerate() {
        if n > 0 {
            x += 2.0;
        }
        if t.sep_before {
            p.fill_rect(Rect::new(x + 6.0, bar.y + 14.0, 1.0, 24.0), f.fx.toolbar_sep);
            x += 13.0;
        }
        let r = Rect::new(x, bar.y + 6.0, *w, 40.0);
        if t.pressed {
            p.fill_rrect(r, 8.0, f.fx.toolbar_pressed);
        }
        f.tint(t.id, r, 8.0, Color::rgb(0xFFFFFF), 0.10, 0.18);
        let color = if t.pressed { Color::rgb(0xFFFFFF) } else { t.color };
        let lab = t.label.as_ref().map(|l| p.label(l, style(t)));
        let mut parts = 0.0f32;
        let mut count = 0;
        if t.icon.is_some() {
            parts += 18.0;
            count += 1;
        }
        if let Some(l) = &lab {
            parts += l.w;
            count += 1;
        }
        if t.chevron {
            parts += 14.0;
            count += 1;
        }
        parts += 8.0 * (count as f32 - 1.0).max(0.0);
        let mut cx = r.x + (r.w - parts) / 2.0;
        if let Some(icon) = &t.icon {
            let ic = if t.id == ControlId::TbSwap { f.fx.toolbar_link } else { color };
            p.svg(icon, cx, r.y + 11.0, 18.0, 1.8, ic);
            cx += 18.0 + 8.0;
        }
        if let Some(l) = &lab {
            p.draw(l, cx, r.y + (40.0 - l.h) / 2.0, color);
            cx += l.w + 8.0;
        }
        if t.chevron {
            p.svg(icons::CHEVRON, cx, r.y + 13.0, 14.0, 2.0, color);
        }
        f.register(t.id, r, 8.0);
        if f.hovered(t.id) {
            pointed = Some((t.id, r));
        }
        if t.id == ControlId::TbChat && f.st.drawer() != Some(DrawerTab::Chat) {
            unread_badge(f, (r.right() - 8.0, r.y + 6.0));
        }
        x += w;
    }
    // The hint of the button under the pointer (D42): what that button does, right under it.
    if let Some((id, button)) = pointed {
        button_hint(f, col, bar, id, button);
    }
}

/// The arrow of the bar's collapse button, and the handle's (pointing down: the bar comes back from there).
const CHEVRON_UP: &str = "M18 15l-6-6-6 6";

/// A collapsed session bar (B22): a small tab at the top edge of the picture, centred, with the connection dot and a
/// downward arrow; a click brings the bar back.
fn bar_handle(f: &mut Frame, col: Rect) {
    let p = f.p;
    let (w, h) = (64.0, 22.0);
    let r = Rect::new(p.snap(col.x + (col.w - w) / 2.0), col.y, w, h);
    // Rounded at the bottom only: the top corners run past the picture's edge.
    let tab = Rect::new(r.x, r.y - 10.0, r.w, r.h + 10.0);
    p.shadow(tab, 10.0, 6.0, 16.0, f.fx.shadow_dialog);
    p.fill_rrect(tab, 10.0, f.fx.toolbar);
    f.tint(ControlId::TbExpand, r, 8.0, Color::rgb(0xFFFFFF), 0.10, 0.18);
    p.fill_circle((r.x + 18.0, r.y + h / 2.0), 3.5, f.pal.ok);
    p.svg(icons::CHEVRON, r.x + 30.0, r.y + 4.0, 14.0, 2.0, f.fx.toolbar_ink);
    f.register(ControlId::TbExpand, r, 8.0);
}

/// What the hovered button of the session bar does: one line in a small dark card under the bar, centred on the
/// button and kept inside the column.
fn button_hint(f: &mut Frame, col: Rect, bar: Rect, id: ControlId, button: Rect) {
    let Some(what) = f.st.toolbar_hint(id) else { return };
    let p = f.p;
    let room = (col.w - 16.0 - 20.0).max(40.0);
    let label = p.label_in(&what, text::SMALL, room, Align::Start);
    let (w, h) = (label.w + 20.0, label.h + 12.0);
    let (cx, _) = button.center();
    // Snapped to the pixel grid inside the margins: rounding the clamped edge up would push the card past them.
    let x = p.snap((cx - w / 2.0).min(col.right() - 8.0 - w).max(col.x + 8.0));
    let x = if x + w > col.right() - 8.0 { (col.right() - 8.0 - w).floor() } else { x };
    let r = Rect::new(x, bar.bottom() + 6.0, w, h);
    p.shadow(r, 8.0, 6.0, 18.0, Color::rgba(0, 0, 0, 35));
    p.fill_rrect(r, 8.0, f.fx.toolbar);
    p.draw(&label, r.x + 10.0, r.y + 6.0, f.fx.toolbar_ink);
    f.out.hint = Some((id, r));
}

/// The count of chat messages not read yet, as a small red dot with the number, centred on `at`; nothing at zero.
fn unread_badge(f: &Frame, at: (f32, f32)) {
    let n = f.st.chat_unread();
    if n == 0 {
        return;
    }
    let p = f.p;
    let label = p.label(&n.min(99).to_string(), text::SMALL_BOLD);
    let radius = (label.w / 2.0 + 4.0).max(8.0);
    p.fill_circle(at, radius, f.pal.danger);
    p.draw(&label, at.0 - label.w / 2.0, at.1 - label.h / 2.0, Color::rgb(0xFFFFFF));
}

// ----- cards in the video area -----

/// The cards over the video stand alone in it: centred, with their content centred, in the look of the main
/// window's wait card (D51).
fn reconnecting_card(f: &mut Frame, col: Rect, peer: &str, attempt: u32) {
    let title = f.st.tm(&Msg::new(MsgKey::ReconnectingTitle).with(peer));
    let body = f.st.tm(&Msg::new(MsgKey::ReconnectingBody).with(attempt.to_string()));
    // The time left of the minute the service keeps trying (R-UI-13): a row of its own, there from the start.
    let bar = f.st.reconnect_wait();
    let buttons = [(ControlId::ReconnectDisconnect, f.t(MsgKey::Disconnect), Kind::Default)];
    Card {
        look: Look::Dark,
        icon: Some(Icon::Spinner),
        title: Some(&title),
        body: Some(&body),
        bar: bar.as_ref(),
        buttons: &buttons,
    }
    .paint_in(f, col, card::W.min(col.w - 40.0));
}

/// The picture is paused (ACCOUNTS.md 5.1 and 9): why, and "Elevar controle" where the session can be elevated.
fn paused_card(f: &mut Frame, video: Rect, body: &str, elevate: bool) {
    let buttons: Vec<(ControlId, String, Kind)> =
        if elevate { vec![(ControlId::PausedElevate, f.t(MsgKey::ElevateButton), Kind::Primary)] } else { Vec::new() };
    Card {
        look: Look::Dark,
        icon: Some(Icon::Ring(icons::SHIELD)),
        title: None,
        body: Some(body),
        bar: None,
        buttons: &buttons,
    }
    .paint_in(f, video, card::W.min(video.w - 40.0));
}

/// The other side did not swap: the card says why (F-SWAP-02).
fn swap_declined_card(f: &mut Frame, video: Rect) {
    let (title, body) = f.st.swap_declined_texts();
    let buttons = [(ControlId::SwapDeclinedClose, f.t(MsgKey::Close), Kind::Default)];
    Card {
        look: Look::Alone,
        icon: Some(Icon::Ring(icons::SWAP)),
        title: Some(&title),
        body: Some(&body),
        bar: None,
        buttons: &buttons,
    }
    .paint_in(f, video, card::W.min(video.w - 40.0));
}

/// The viewer asked to swap: who is asked, what happens if nobody answers and the time left, in the place and
/// style of the declined card. No button: an outgoing request cannot be withdrawn.
fn swap_wait_card(f: &mut Frame, video: Rect, wait: &SwapWait) {
    let pal = f.pal;
    Card {
        look: Look::Alone,
        icon: Some(Icon::Circle(pal.link_tint, pal.link, icons::SWAP)),
        title: Some(&wait.title),
        body: Some(&wait.body),
        bar: Some(&wait.bar),
        buttons: &[],
    }
    .paint_in(f, video, card::W.min(video.w - 40.0));
}

/// The swap confirmation over the viewer: a centred card with its text and buttons centred (D51).
fn swap_confirm(f: &mut Frame, kind: &DialogKind) {
    let (p, pal) = (f.p, f.pal);
    let t = f.st.dialog_texts(kind);
    let w = layout::DIALOG_W.min(f.w - 48.0) - 64.0;
    let title = p.label_in(&t.title, text::H1_DIALOG, w, Align::Center);
    let bodies: Vec<_> = t.body.iter().map(|b| p.label_in(b, text::BODY, w, Align::Center)).collect();
    let mut h = 32.0 + title.h + 28.0 + 40.0 + 32.0;
    for b in &bodies {
        h += 12.0 + b.h;
    }
    let r = Rect::new(
        p.snap((f.w - layout::DIALOG_W.min(f.w - 48.0)) / 2.0),
        p.snap((f.h - h) / 2.0),
        layout::DIALOG_W.min(f.w - 48.0),
        h,
    );
    p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), f.fx.toolbar.with_alpha(0.55));
    p.shadow(r, layout::DIALOG_RADIUS, 30.0, 80.0, f.fx.shadow_dialog);
    p.fill_rrect(r, layout::DIALOG_RADIUS, pal.surface);
    let (x, mut y) = (r.x + 32.0, r.y + 32.0);
    p.draw(&title, x, y, pal.ink);
    y += title.h;
    for b in &bodies {
        y += 12.0;
        p.draw(b, x, y, pal.soft);
        y += b.h;
    }
    y += 28.0;
    // One centred group: the negative action on the left, the primary on the right.
    if let Some(pm) = &t.primary {
        let mut list = Vec::new();
        if let Some(s) = &t.secondary {
            list.push((ControlId::DialogSecondary, s.clone(), Kind::Default));
        }
        list.push((ControlId::DialogPrimary, pm.clone(), Kind::Primary));
        card::actions(f, x, y, r.w - 64.0, &list, false);
    }
}

// ----- drawer -----

fn drawer_panel(f: &mut Frame, r: Rect, open: DrawerTab, peer: &str) {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    p.fill_rect(r, pal.surface);
    p.fill_rect(Rect::new(r.x, r.y, bw, r.h), pal.line);
    // Tabs: 44 px, padding 0 10, gap 2 and 12 px to the start; the close button at the end.
    let mut x = r.x + 12.0;
    for (tab, key) in [
        (DrawerTab::Terminal, MsgKey::DrawerTerminal),
        (DrawerTab::Chat, MsgKey::DrawerChat),
        (DrawerTab::Clipboard, MsgKey::DrawerClipboard),
    ] {
        let selected = open == tab;
        let id = ControlId::DrawerTab(tab);
        let l = p.label(&f.t(key), if selected { text::TAB_SELECTED } else { text::TAB });
        let tr = Rect::new(x, r.y, l.w + 20.0, 44.0);
        if selected {
            p.fill_rect(Rect::new(tr.x, r.y + 42.0, tr.w, 2.0), pal.ink);
        }
        let color = if selected || f.hovered(id) { pal.ink } else { pal.soft };
        p.draw(&l, x + 10.0, r.y + (42.0 - l.h) / 2.0, color);
        f.register(id, tr, 0.0);
        x += tr.w + 2.0;
    }
    let close = Rect::new(r.right() - 8.0 - 40.0, r.y + 2.0, 40.0, 40.0);
    p.bordered(close, 8.0, pal.line, pal.surface);
    f.tint(ControlId::DrawerClose, close, 8.0, pal.ink, 0.06, 0.11);
    p.svg(icons::CLOSE, close.x + 12.0, close.y + 12.0, 16.0, 2.0, pal.ink);
    f.register(ControlId::DrawerClose, close, 8.0);
    p.fill_rect(Rect::new(r.x + bw, r.y + 44.0, r.w - bw, bw), pal.line);

    let body = Rect::new(r.x + bw, r.y + 44.0 + bw, r.w - bw, r.h - 44.0 - bw);
    match open {
        DrawerTab::Terminal => drawer_terminal(f, body, peer),
        DrawerTab::Chat => drawer_chat(f, body),
        DrawerTab::Clipboard => drawer_clipboard(f, body),
    }
}

fn drawer_terminal(f: &mut Frame, b: Rect, peer: &str) {
    let (p, pal) = (f.p, f.pal);
    let (x, mut y, w) = (b.x + 20.0, b.y + 18.0, b.w - 40.0);
    let phase = f.st.term_phase().clone();
    let account = f.st.term_account().unwrap_or("").to_owned();
    // F-TERM-02: the administrator terminal says so in the panel.
    // The note says whose account the commands run as: only while a shell runs.
    if matches!(phase, TermPhase::Open { .. }) {
        let note = if f.st.term_is_admin() {
            Msg::new(MsgKey::TermAdminNote).with(peer)
        } else {
            Msg::new(MsgKey::TerminalNote).with(peer).with(account)
        };
        let note = f.st.tm(&note);
        y += f.para(&note, text::BODY, x, y, w, pal.soft, Align::Start) + 14.0;
    }

    let reopen = matches!(phase, TermPhase::Exited { .. } | TermPhase::Failed { .. });
    let admin_button = f.st.admin_terminal_offered() && !f.st.term_is_admin();
    let buttons_h = if reopen || admin_button { 40.0 + 14.0 } else { 0.0 };
    let hint = matches!(phase, TermPhase::Open { .. })
        .then(|| p.label_in(&f.t(MsgKey::TermHint), text::CAPTION, w, Align::Start));
    let hint_h = hint.as_ref().map_or(0.0, |h| h.h + 8.0);
    let actions_h = 21.75 + 14.0 + 2.0 * 40.0 + 8.0;
    let box_h = (b.bottom() - 18.0 - actions_h - 14.0 - buttons_h - hint_h - y).max(80.0);
    let grid = Rect::new(x, y, w, box_h);
    p.fill_rrect(grid, 10.0, f.fx.terminal_bg);
    // The grid is drawn by the window after the frame (the terminal renderer), a few DIPs inside the rounded box.
    f.out.terminal = Some(grid.inset(3.0));
    let say = |f: &Frame, msg: String| {
        let l = f.p.label_in(&msg, text::CAPTION, w - 32.0, Align::Start);
        f.p.draw(&l, x + 16.0, y + 14.0, f.fx.terminal_ink);
    };
    match &phase {
        TermPhase::Open { .. } => f.register(ControlId::TermGrid, grid, 10.0),
        // The host asks its user first (D13): who is asked, and the time the question has left.
        TermPhase::Opening { .. } | TermPhase::Idle => match f.st.consent_wait(swapdesk_ui::Feature::Terminal) {
            Some(bar) => {
                let msg = f.st.tm(&Msg::new(MsgKey::TermConsentWaiting).with(peer));
                let l = f.p.label_in(&msg, text::CAPTION, w - 32.0, Align::Start);
                f.p.draw(&l, x + 16.0, y + 14.0, f.fx.terminal_ink);
                let colors = [f.fx.card_dark_border, f.fx.spinner, f.fx.card_dark_soft];
                f.countdown_row((x + 16.0, y + 14.0 + l.h + 10.0), (w - 32.0).min(320.0), &bar, colors);
            }
            None => say(f, f.t(MsgKey::TermOpening)),
        },
        TermPhase::Failed { why, .. } => say(f, f.st.tm(why)),
        // The last screen stays visible; the line below says the shell ended.
        TermPhase::Exited { .. } => {}
    }
    y += box_h + 8.0;
    if let Some(h) = hint {
        p.draw(&h, x, y, pal.soft);
        y += h.h;
    }
    if let TermPhase::Exited { code, .. } = &phase {
        let line = match code {
            Some(c) => f.st.tm(&Msg::new(MsgKey::TermExitedCode).with(c.to_string())),
            None => f.t(MsgKey::TermEnded),
        };
        y += f.para(&line, text::CAPTION, x, y, w, pal.soft, Align::Start);
    }
    y += 6.0;
    if reopen || admin_button {
        let mut bx = x;
        if reopen {
            let label = f.t(MsgKey::TermReopen);
            let bw = f.button_width(&label, Kind::Default);
            f.button(ControlId::TermReopen, Rect::new(bx, y, bw, 40.0), Kind::Default, &label, false);
            bx += bw + 8.0;
        }
        if admin_button {
            let label = f.t(MsgKey::TermOpenAdmin);
            let bw = f.button_width(&label, Kind::Default);
            f.button(ControlId::TermAdmin, Rect::new(bx, y, bw, 40.0), Kind::Default, &label, false);
        }
        y += 40.0 + 14.0;
    }
    let h2 = p.label(&f.t(MsgKey::QuickActions), text::H2);
    p.draw(&h2, x, y, pal.ink);
    y += h2.h + 14.0;
    let cw = (w - 8.0) / 2.0;
    for (n, (kind, key)) in f.st.quick_actions().into_iter().enumerate() {
        let r = Rect::new(x + (n % 2) as f32 * (cw + 8.0), y + (n / 2) as f32 * 48.0, cw, 40.0);
        let k = if kind == ActionKind::Restart { Kind::Danger } else { Kind::Default };
        f.button(ControlId::Action(kind), r, k, &f.t(key), false);
    }
    // One action at a time (R-UI-13): while one waits for the other computer the buttons are dimmed and a line
    // under them says what is waited for.
    if f.st.actions_disabled() {
        for w in f.out.widgets.clone() {
            if matches!(w.id, ControlId::Action(_)) {
                p.fill_rrect(w.rect, layout::BUTTON_RADIUS, pal.surface.with_alpha(0.55));
            }
        }
    }
    if let Some(wait) = f.st.action_wait() {
        let rows = f.st.quick_actions().len().div_ceil(2);
        f.para(&wait.line, text::CAPTION, x, y + rows as f32 * 48.0, w, pal.soft, Align::Start);
    }
}

pub(crate) fn human_size(bytes: u64) -> String {
    const KB: f64 = 1000.0;
    let b = bytes as f64;
    let s = if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    };
    s.replace('.', ",")
}

fn drawer_chat(f: &mut Frame, b: Rect) {
    let (p, pal) = (f.p, f.pal);
    let bw = p.hairline();
    let (x, w) = (b.x + 20.0, b.w - 40.0);
    // The input row at the bottom: padding 14 20, border-top, gap 10.
    let row_h = 14.0 + 40.0 + 14.0;
    p.fill_rect(Rect::new(b.x, b.bottom() - row_h, b.w, bw), pal.line);
    let send = f.t(MsgKey::ChatSendButton);
    let sw = f.button_width(&send, Kind::Primary);
    f.button(
        ControlId::ChatSend,
        Rect::new(x + w - sw, b.bottom() - 14.0 - 40.0, sw, 40.0),
        Kind::Primary,
        &send,
        false,
    );
    let field = Rect::new(x, b.bottom() - 14.0 - 40.0, w - sw - 10.0, 40.0);
    f.edit_field(EditId::Chat, field, "", &f.t(MsgKey::ChatPlaceholder));

    let mut y = b.y + 20.0;
    let note = p.label_in(&f.t(MsgKey::ChatNote), text::CAPTION, w, Align::Center);
    p.draw(&note, x, y, pal.soft);
    y += note.h + 12.0;
    let msgs: Vec<_> = f.st.chat().to_vec();
    let last_mine_read = msgs.iter().rev().find(|m| m.mine && m.read).map(|m| m.msg_id);
    for m in &msgs {
        let maxw = w * 0.8;
        let l = p.label_in(&m.text, text::BODY, maxw - 28.0, Align::Start);
        let bw_ = (l.w + 28.0).min(maxw);
        let bh = l.h + 20.0;
        let bx = if m.mine { x + w - bw_ } else { x };
        let r = Rect::new(bx, y, bw_, bh);
        if m.mine {
            p.fill_rrect(r, 14.0, pal.link);
            p.draw(&l, bx + 14.0, y + 10.0, pal.link_ink);
        } else {
            p.bordered(r, 14.0, pal.line, pal.sunken);
            p.draw(&l, bx + 14.0, y + 10.0, pal.ink);
        }
        y += bh + 12.0;
        if Some(m.msg_id) == last_mine_read {
            let t = format::local_time(m.sent_at_ms, f.st.clock().utc_offset_min);
            let s = f.st.tm(&Msg::new(MsgKey::ChatReadAt).with(format!("{:02}:{:02}", t.hour, t.minute)));
            let rl = p.label(&s, text::SMALL);
            p.draw(&rl, x + w - rl.w, y - 6.0, pal.soft);
            y += rl.h + 6.0;
        }
    }
}

fn drawer_clipboard(f: &mut Frame, b: Rect) {
    let (p, pal) = (f.p, f.pal);
    let (x, y, w) = (b.x + 20.0, b.y + 20.0, b.w - 40.0);
    let title = p.label(&f.t(MsgKey::ClipTitle), text::H2);
    let body = p.label_in(&f.t(MsgKey::ClipBody), text::CAPTION, w - 32.0 - 38.0 - 14.0, Align::Start);
    let h = 14.0 * 2.0 + title.h + 2.0 + body.h;
    let r = Rect::new(x, y, w, h);
    f.panel(r, pal.surface);
    p.draw(&title, x + 16.0, y + 14.0, pal.ink);
    p.draw(&body, x + 16.0, y + 14.0 + title.h + 2.0, pal.soft);
    let on = f.st.clipboard_on();
    f.switch(ControlId::ClipToggle, r.right() - 16.0 - layout::SWITCH_W, y + (h - layout::SWITCH_H) / 2.0, on, false);
}

// ---------------------------------------------------------------------------------------------------------------
// Banner

/// The host banner: "<peer> controla este computador há 12 min", chat, swap and disconnect.
pub fn paint_banner(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    let Some(h) = f.st.host_session().cloned() else { return };
    let peer = f.st.device_by_id(&h.device_id).map(|d| d.name.clone()).unwrap_or_default();
    p.clear(pal.surface);
    // 2 px link ring inside the window (box-shadow 0 0 0 2px var(--link)).
    p.stroke_rrect(Rect::new(1.0, 1.0, f.w - 2.0, f.h - 2.0), 12.0, 2.0, pal.link);
    // The row of who, since when and the buttons keeps its height; the files line (FILES-UI.md 6) sits under it.
    let cy = BANNER_ROW_H / 2.0;
    p.fill_circle((18.0 + 5.0, cy), 5.0, pal.link);
    // Bold first part, soft second part, in one line.
    let (who, since) = f.st.banner_parts(&peer, &h);
    let who_l = p.label(&who, text::NAME);
    let tx = 18.0 + 10.0 + 14.0;
    p.draw(&who_l, tx, cy - who_l.h / 2.0, pal.ink);
    // A wait (a swap asked from here, a session reconnecting) is the first part alone. The second part never runs
    // under the buttons: it is trimmed to the room they leave.
    if !since.is_empty() {
        let disc_w = p.label(&f.t(MsgKey::Disconnect), text::BTN_PRIMARY).w + 28.0 + 2.0 * p.hairline();
        let swap_w = if f.st.swap_offered(&h) {
            14.0 + p.label(&f.t(MsgKey::TbSwap), text::BTN).w + 28.0 + 2.0 * p.hairline() + 16.0 + 8.0
        } else {
            0.0
        };
        let buttons_x = f.w - 8.0 - disc_w - swap_w - 14.0 - 36.0;
        let room = (buttons_x - 14.0 - (tx + who_l.w)).max(0.0);
        let since_l = p.label_ellipsis(&format!(" {since}"), text::BODY, room);
        p.draw(&since_l, tx + who_l.w, cy - since_l.h / 2.0, pal.soft);
    }

    // Buttons from the end: disconnect (solid red), swap (bordered, icon + label), chat (36 px icon button).
    let btn_h = 36.0;
    let mut x = f.w - 8.0;
    let disc = f.t(MsgKey::Disconnect);
    let dw = p.label(&disc, text::BTN_PRIMARY).w + 28.0 + 2.0 * p.hairline();
    x -= dw;
    f.button(ControlId::BannerDisconnect, Rect::new(x, cy - btn_h / 2.0, dw, btn_h), Kind::DangerSolid, &disc, false);
    // Swap between Windows and macOS is not in 1.0: the button exists only toward a machine of this OS, and the
    // chat button then sits next to "Desconectar" with the usual gap.
    if f.st.swap_offered(&h) {
        let swap = f.t(MsgKey::TbSwap);
        let sw = p.label(&swap, text::BTN).w + 28.0 + 2.0 * p.hairline() + 16.0 + 8.0;
        x -= 14.0 + sw;
        let sr = Rect::new(x, cy - btn_h / 2.0, sw, btn_h);
        p.bordered(sr, 8.0, pal.line, pal.surface);
        f.tint(ControlId::BannerSwap, sr, 8.0, pal.ink, 0.06, 0.11);
        p.svg(icons::SWAP, sr.x + 14.0 + p.hairline(), sr.y + 10.0, 16.0, 2.0, pal.link);
        let sl = p.label(&swap, text::BTN);
        p.draw(&sl, sr.x + 14.0 + p.hairline() + 16.0 + 8.0, sr.y + (btn_h - sl.h) / 2.0, pal.ink);
        f.register(ControlId::BannerSwap, sr, 8.0);
    }
    x -= 14.0 + 36.0;
    let cr = Rect::new(x, cy - btn_h / 2.0, 36.0, btn_h);
    p.bordered(cr, 8.0, pal.line, pal.surface);
    f.tint(ControlId::BannerChat, cr, 8.0, pal.ink, 0.06, 0.11);
    p.svg(icons::CHAT, cr.x + 10.0, cr.y + 10.0, 16.0, 1.8, pal.ink);
    f.register(ControlId::BannerChat, cr, 8.0);
    unread_badge(f, (cr.right() - 4.0, cr.y + 4.0));
    // One soft line under the row while files move or change here; never a file name (the state's line has none).
    if let Some(line) = f.st.banner_files_line() {
        let l = p.label_ellipsis(&line, text::BODY, (f.w - tx - 16.0).max(0.0));
        p.draw(&l, tx, BANNER_ROW_H + BANNER_FILES_H / 2.0 - 2.0 - l.h / 2.0, pal.soft);
    }
}

/// The banner's main row: who controls, since when, chat, swap and disconnect.
const BANNER_ROW_H: f32 = 52.0;
/// The extra height of the banner while it has a files line.
const BANNER_FILES_H: f32 = 26.0;

/// The banner window's size in DIPs when it has only its main row: wide enough for its text and buttons.
pub fn banner_size() -> (f32, f32) {
    (820.0, BANNER_ROW_H)
}

/// The banner window's size for `st`: one line taller while the banner has a files line.
pub fn banner_size_for(st: &swapdesk_ui::state::UiState) -> (f32, f32) {
    let (w, h) = banner_size();
    if st.banner_files_line().is_some() { (w, h + BANNER_FILES_H) } else { (w, h) }
}
