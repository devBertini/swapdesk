//! The viewer window (Visualizador) and the host banner, drawn as a static shell: the video area is a placeholder
//! (its visual is stage 15), the terminal panel's grid is drawn by the window (stage 43). Toolbar, drawer, cards and the banner follow the design.

use swapdesk_ui::format;
use swapdesk_ui::tokens::{Color, fixed, layout, text};
use swapdesk_ui::{
    ActionKind, ControlId, DialogKind, DrawerTab, EditId, Msg, MsgKey, SessionState, SwapPhase, TermPhase, WindowKind,
};

use crate::gfx::{Align, Rect};
use crate::view::{Frame, Kind, icons};

/// The viewer window.
pub fn paint_viewer(f: &mut Frame) {
    let (p, pal) = (f.p, f.pal);
    p.clear(pal.canvas);
    let Some(session) = f.st.viewer_session().cloned() else { return };
    let peer = f.st.device_by_id(&session.device_id).map(|d| d.name.clone()).unwrap_or_default();
    let bw = p.hairline();

    // Top bar: 40 px, padding 0 16, gap 10.
    p.fill_rect(Rect::new(0.0, 0.0, f.w, layout::VIEWER_BAR_H), pal.surface);
    p.fill_rect(Rect::new(0.0, layout::VIEWER_BAR_H - bw, f.w, bw), pal.line);
    let cy = layout::VIEWER_BAR_H / 2.0;
    p.fill_circle((16.0 + 4.0, cy), 4.0, pal.ok);
    let name = p.label(&peer, text::NAME);
    p.draw(&name, 16.0 + 8.0 + 10.0, (layout::VIEWER_BAR_H - name.h) / 2.0, pal.ink);
    let status = p.label(&f.st.viewer_status_text(&session), text::BODY);
    p.draw(&status, 16.0 + 8.0 + 10.0 + name.w + 10.0, (layout::VIEWER_BAR_H - status.h) / 2.0, pal.soft);
    if let (Some(set), Some(stats)) = (f.st.settings(), session.stats)
        && set.show_stats
    {
        let s = f.st.tm(&Msg::new(MsgKey::ViewerStats).with(stats.latency_ms.to_string()).with(stats.fps.to_string()));
        let l = p.label(&s, text::CAPTION);
        p.draw(&l, f.w - 16.0 - l.w, (layout::VIEWER_BAR_H - l.h) / 2.0, pal.soft);
    }

    // The body: the video column and, when open, the drawer (448 px, border-left).
    let body = Rect::new(0.0, layout::VIEWER_BAR_H, f.w, f.h - layout::VIEWER_BAR_H);
    let drawer = f.st.drawer();
    let video_col = Rect::new(0.0, body.y, if drawer.is_some() { body.w - layout::DRAWER_W } else { body.w }, body.h);
    p.fill_rect(video_col, pal.remote);
    let pad = if drawer.is_some() { 20.0 } else { 48.0 };
    let video = fit_16_10(video_col.inset(pad));
    p.fill_rrect(video, 4.0, fixed::REMOTE_SCREEN);
    f.out.video = Some(video);
    let ph = p.label_in(&f.t(MsgKey::VideoPlaceholder), text::BODY, video.w - 40.0, Align::Center);
    let (vx, vy) = video.center();
    p.draw(&ph, video.x + 20.0, vy - ph.h / 2.0, fixed::CARD_DARK_SOFT);
    let _ = vx;

    if let SessionState::Reconnecting { attempt } = session.state {
        reconnecting_card(f, video_col, &peer, attempt);
    } else {
        if matches!(f.st.swap().map(|s| &s.phase), Some(SwapPhase::Declined(_))) {
            swap_declined_card(f, video, &peer);
        }
        toolbar(f, video_col, &session.device_id);
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

fn toolbar(f: &mut Frame, col: Rect, device_id: &str) {
    let (p, _pal) = (f.p, f.pal);
    let _ = device_id;
    let display = f.st.tm(&Msg::new(MsgKey::TbScreenN).with("1"));
    let white = fixed::TOOLBAR_INK;
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
    files.pressed = f.st.drawer() == Some(DrawerTab::Files);
    let mut chat = tool(ControlId::TbChat, icons::CHAT);
    chat.pressed = f.st.drawer() == Some(DrawerTab::Chat);
    tools.extend([term, files, chat]);
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
    tools.push(Tool {
        id: ControlId::TbDisconnect,
        icon: None,
        label: Some(f.t(MsgKey::Disconnect)),
        pressed: false,
        color: fixed::TOOLBAR_DANGER,
        weight_bold: true,
        chevron: false,
        sep_before: false,
    });

    // Measure: button = padding 10 + content + 10, min 40; content = icon 18 / label / chevron 14, gap 8.
    let style = |t: &Tool| if t.weight_bold { text::BTN_PRIMARY } else { text::BTN };
    let widths: Vec<f32> = tools
        .iter()
        .map(|t| {
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
        })
        .collect();
    let seps = tools.iter().filter(|t| t.sep_before).count() as f32;
    let total = 12.0 + widths.iter().sum::<f32>() + 2.0 * (tools.len() as f32 - 1.0) + seps * (1.0 + 12.0);
    let bar = Rect::new(p.snap(col.x + (col.w - total) / 2.0), col.y + 14.0, total, 52.0);
    p.shadow(bar, 12.0, 10.0, 30.0, Color::rgba(0, 0, 0, 35));
    p.fill_rrect(bar, 12.0, fixed::TOOLBAR);
    let mut x = bar.x + 6.0;
    for (n, (t, w)) in tools.iter().zip(&widths).enumerate() {
        if n > 0 {
            x += 2.0;
        }
        if t.sep_before {
            p.fill_rect(Rect::new(x + 6.0, bar.y + 14.0, 1.0, 24.0), fixed::TOOLBAR_SEP);
            x += 13.0;
        }
        let r = Rect::new(x, bar.y + 6.0, *w, 40.0);
        if t.pressed {
            p.fill_rrect(r, 8.0, fixed::TOOLBAR_PRESSED);
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
            let ic = if t.id == ControlId::TbSwap { fixed::TOOLBAR_LINK } else { color };
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
        if t.id == ControlId::TbChat && f.st.drawer() != Some(DrawerTab::Chat) {
            unread_badge(f, (r.right() - 8.0, r.y + 6.0));
        }
        x += w;
    }
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

fn reconnecting_card(f: &mut Frame, col: Rect, peer: &str, attempt: u32) {
    let (p, _pal) = (f.p, f.pal);
    let w = 360.0f32.min(col.w - 40.0);
    let title =
        p.label_in(&f.st.tm(&Msg::new(MsgKey::ReconnectingTitle).with(peer)), text::H2_STATE, w - 44.0, Align::Start);
    let body = p.label_in(
        &f.st.tm(&Msg::new(MsgKey::ReconnectingBody).with(attempt.to_string())),
        text::BODY,
        w - 44.0,
        Align::Start,
    );
    let h = 20.0 + 40.0 + 10.0 + title.h + 10.0 + body.h + 10.0 + 40.0 + 20.0;
    let r = Rect::new(p.snap(col.x + (col.w - w) / 2.0), p.snap(col.y + (col.h - h) / 2.0), w, h);
    p.fill_rrect(r, 12.0, f.pal.remote);
    p.bordered(r, 12.0, fixed::CARD_DARK_BORDER, f.pal.remote);
    let (x, mut y) = (r.x + 22.0, r.y + 20.0);
    p.arc_stroke((x + 20.0, y + 20.0), 18.5, -90.0 + 45.0, 270.0, 3.0, fixed::SPINNER);
    y += 50.0;
    p.draw(&title, x, y, fixed::CARD_DARK_INK);
    y += title.h + 10.0;
    p.draw(&body, x, y, fixed::CARD_DARK_SOFT);
    let label = f.t(MsgKey::Disconnect);
    let bw = f.button_width(&label, Kind::Default);
    let br = Rect::new(x, r.bottom() - 20.0 - 40.0, bw, 40.0);
    p.stroke_rrect(br.inset(0.5), 8.0, 1.0, fixed::CARD_DARK_BORDER);
    f.tint(ControlId::ReconnectDisconnect, br, 8.0, Color::rgb(0xFFFFFF), 0.08, 0.14);
    let l = p.label(&label, text::BTN);
    p.draw(&l, br.x + (br.w - l.w) / 2.0, br.y + (40.0 - l.h) / 2.0, fixed::CARD_DARK_INK);
    f.register(ControlId::ReconnectDisconnect, br, 8.0);
}

fn swap_declined_card(f: &mut Frame, video: Rect, peer: &str) {
    let (p, pal) = (f.p, f.pal);
    let w = 360.0f32.min(video.w - 40.0);
    let bwid = p.hairline();
    let _ = peer;
    // Which words: the card says why the other side did not swap (F-SWAP-02).
    let (title_text, body_text) = f.st.swap_declined_texts();
    let title = p.label_in(&title_text, text::H2_STATE, w - 44.0 - 2.0 * bwid, Align::Start);
    let body = p.label_in(&body_text, text::BODY, w - 44.0 - 2.0 * bwid, Align::Start);
    let h = 2.0 * bwid + 40.0 + 40.0 + 10.0 + title.h + 10.0 + body.h + 10.0 + 40.0;
    let r = Rect::new(p.snap(video.x + (video.w - w) / 2.0), p.snap(video.y + (video.h - h) / 2.0), w, h);
    f.panel(r, pal.surface);
    let (x, mut y) = (r.x + bwid + 22.0, r.y + bwid + 20.0);
    p.fill_circle((x + 20.0, y + 20.0), 20.0, pal.sunken);
    p.stroke_circle((x + 20.0, y + 20.0), 19.5, 1.0, pal.line);
    p.svg(icons::SWAP, x + 10.0, y + 10.0, 20.0, 2.0, pal.soft);
    y += 50.0;
    p.draw(&title, x, y, pal.ink);
    y += title.h + 10.0;
    p.draw(&body, x, y, pal.soft);
    let close = f.t(MsgKey::Close);
    let cw = f.button_width(&close, Kind::Default);
    f.button(
        ControlId::SwapDeclinedClose,
        Rect::new(x, r.bottom() - bwid - 20.0 - 40.0, cw, 40.0),
        Kind::Default,
        &close,
        false,
    );
}

fn swap_confirm(f: &mut Frame, kind: &DialogKind) {
    let (p, pal) = (f.p, f.pal);
    let t = f.st.dialog_texts(kind);
    let w = layout::DIALOG_W.min(f.w - 48.0) - 64.0;
    let title = p.label_in(&t.title, text::H1_DIALOG, w, Align::Start);
    let bodies: Vec<_> = t.body.iter().map(|b| p.label_in(b, text::BODY, w, Align::Start)).collect();
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
    p.fill_rect(Rect::new(0.0, 0.0, f.w, f.h), fixed::TOOLBAR.with_alpha(0.55));
    p.shadow(r, layout::DIALOG_RADIUS, 30.0, 80.0, fixed::SHADOW_DIALOG);
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
    if let Some(pm) = &t.primary {
        let pw = f.button_width(pm, Kind::Primary);
        f.button(ControlId::DialogPrimary, Rect::new(x, y, pw, 40.0), Kind::Primary, pm, false);
        if let Some(s) = &t.secondary {
            let sw = f.button_width(s, Kind::Default);
            f.button(ControlId::DialogSecondary, Rect::new(x + pw + 10.0, y, sw, 40.0), Kind::Default, s, false);
        }
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
        (DrawerTab::Files, MsgKey::DrawerFiles),
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
        DrawerTab::Files => crate::screens_files::drawer_files(f, body, peer),
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
    p.fill_rrect(grid, 10.0, fixed::TERMINAL_BG);
    // The grid is drawn by the window after the frame (the terminal renderer), a few DIPs inside the rounded box.
    f.out.terminal = Some(grid.inset(3.0));
    let say = |f: &Frame, msg: String| {
        let l = f.p.label_in(&msg, text::CAPTION, w - 32.0, Align::Start);
        f.p.draw(&l, x + 16.0, y + 14.0, fixed::TERMINAL_INK);
    };
    match &phase {
        TermPhase::Open { .. } => f.register(ControlId::TermGrid, grid, 10.0),
        TermPhase::Opening { .. } | TermPhase::Idle => say(f, f.t(MsgKey::TermOpening)),
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
    let cy = f.h / 2.0;
    p.fill_circle((18.0 + 5.0, cy), 5.0, pal.link);
    // Bold first part, soft second part, in one line.
    let (who, since) = f.st.banner_parts(&peer, &h);
    let since = format!(" {since}");
    let who_l = p.label(&who, text::NAME);
    let since_l = p.label(&since, text::BODY);
    let tx = 18.0 + 10.0 + 14.0;
    p.draw(&who_l, tx, cy - who_l.h / 2.0, pal.ink);
    p.draw(&since_l, tx + who_l.w, cy - since_l.h / 2.0, pal.soft);

    // Buttons from the end: disconnect (solid red), swap (bordered, icon + label), chat (36 px icon button).
    let btn_h = 36.0;
    let mut x = f.w - 8.0;
    let disc = f.t(MsgKey::Disconnect);
    let dw = p.label(&disc, text::BTN_PRIMARY).w + 28.0 + 2.0 * p.hairline();
    x -= dw;
    f.button(ControlId::BannerDisconnect, Rect::new(x, cy - btn_h / 2.0, dw, btn_h), Kind::DangerSolid, &disc, false);
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
    x -= 14.0 + 36.0;
    let cr = Rect::new(x, cy - btn_h / 2.0, 36.0, btn_h);
    p.bordered(cr, 8.0, pal.line, pal.surface);
    f.tint(ControlId::BannerChat, cr, 8.0, pal.ink, 0.06, 0.11);
    p.svg(icons::CHAT, cr.x + 10.0, cr.y + 10.0, 16.0, 1.8, pal.ink);
    f.register(ControlId::BannerChat, cr, 8.0);
    unread_badge(f, (cr.right() - 4.0, cr.y + 4.0));
}

/// The banner window's size in DIPs: wide enough for its text and buttons.
pub fn banner_size() -> (f32, f32) {
    (760.0, 52.0)
}
