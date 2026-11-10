//! The drawing context of one frame: the shared widgets (buttons, switches, panels, placeholders), hit-test
//! registration and the output the window needs afterwards (where each control landed, where the native text inputs
//! and the video area are).

use swapdesk_ui::tokens::{Color, Palette, TextStyle, layout, palette, text};
use swapdesk_ui::{ControlId, EditId, UiState};

use crate::painter::{Align, Painter, Rect, rrect_path};

/// What the pointer and keyboard are doing, owned by the window.
#[derive(Default, Clone, PartialEq)]
pub struct Interaction {
    pub hover: Option<ControlId>,
    pub pressed: Option<ControlId>,
    pub focus: Option<ControlId>,
    /// The ring shows after keyboard navigation and hides on a mouse click (CSS `:focus-visible`).
    pub focus_visible: bool,
    /// Scroll offsets of the lists, in DIPs.
    pub scroll: f32,
    pub drawer_scroll: f32,
    /// Which scrolling content of the main window `scroll` belongs to (the `scroll_key` of the last frame's
    /// [`Output`]): another one starts at its top.
    pub scroll_key: u8,
}

/// Where a control ended up in the last frame, for hit testing, the focus ring and the accessibility bounds.
#[derive(Clone, Copy, Debug)]
pub struct Widget {
    pub id: ControlId,
    pub rect: Rect,
    pub radius: f32,
}

/// A native text input the window must place over the drawn field.
#[derive(Clone, Debug)]
pub struct EditSlot {
    pub id: EditId,
    /// The text area, in DIPs.
    pub rect: Rect,
    pub value: String,
    pub placeholder: String,
}

#[derive(Default)]
pub struct Output {
    pub widgets: Vec<Widget>,
    pub edits: Vec<EditSlot>,
    /// The video rectangle of a viewer window, in DIPs: stage 15 puts the video visual here.
    pub video: Option<Rect>,
    /// The largest scroll offset of the list on screen.
    pub scroll_max: f32,
    /// Which scrolling content this frame has in the main window (0: none or the only one of its window). The
    /// window keeps it with its scroll offset (see [`Interaction::scroll_key`]).
    pub scroll_key: u8,
    /// The drawer's scrollable list, if any, in DIPs (for wheel routing).
    pub list_area: Option<Rect>,
    /// Where the drawer's terminal grid goes, in DIPs, when the panel shows the terminal tab: the window draws the
    /// grid there after the frame (R-UI-11: its renderer is made the first time this is `Some`).
    pub terminal: Option<Rect>,
    /// The hint on screen and the control it is about, in DIPs: the card under the hovered button of the session
    /// bar that says what the button does.
    pub hint: Option<(ControlId, Rect)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Default,
    Primary,
    Quiet,
    Danger,
    /// The dark toolbar's red solid button on a light card.
    DangerSolid,
}

pub struct Frame<'a> {
    pub p: &'a dyn Painter,
    pub pal: &'static Palette,
    pub st: &'a UiState,
    pub ix: &'a Interaction,
    pub out: Output,
    /// Offscreen captures draw input values themselves; a live window shows the native inputs.
    pub offscreen: bool,
    /// The shell shows the remote picture in a view under a transparent canvas (macOS): the viewer screen then leaves
    /// the video rectangle unpainted. `false` where the picture is composed over the frame (Windows) and offscreen.
    pub video_under: bool,
    pub w: f32,
    pub h: f32,
}

impl<'a> Frame<'a> {
    pub fn new(
        p: &'a dyn Painter,
        st: &'a UiState,
        ix: &'a Interaction,
        size: (f32, f32),
        offscreen: bool,
    ) -> Frame<'a> {
        Frame {
            p,
            pal: palette(st.dark()),
            st,
            ix,
            out: Output::default(),
            offscreen,
            video_under: false,
            w: size.0,
            h: size.1,
        }
    }

    pub fn t(&self, key: swapdesk_ui::MsgKey) -> String {
        self.st.t(key)
    }

    pub fn register(&mut self, id: ControlId, rect: Rect, radius: f32) {
        self.out.widgets.push(Widget { id, rect, radius });
    }

    pub fn hovered(&self, id: ControlId) -> bool {
        self.ix.hover == Some(id)
    }

    /// Hover and pressed tint over a widget's box.
    pub fn tint(&self, id: ControlId, rect: Rect, radius: f32, base: Color, hover: f32, pressed: f32) {
        if !self.hovered(id) {
            return;
        }
        let a = if self.ix.pressed == Some(id) { pressed } else { hover };
        self.p.fill_rrect(rect, radius, base.with_alpha(a));
    }

    // ----- text -----

    /// Draws one line of text at `(x, y)` and returns its height.
    pub fn text(&self, s: &str, st: TextStyle, x: f32, y: f32, c: Color) -> f32 {
        let l = self.p.label(s, st);
        self.p.draw(&l, x, y, c);
        l.h
    }

    /// Draws wrapped text in `width` and returns its height.
    #[allow(clippy::too_many_arguments)] // drawing helpers take the geometry and colour the CSS gives them
    pub fn para(&self, s: &str, st: TextStyle, x: f32, y: f32, width: f32, c: Color, align: Align) -> f32 {
        let l = self.p.label_in(s, st, width, align);
        self.p.draw(&l, x, y, c);
        l.h
    }

    /// The height wrapped text would take, without drawing it.
    pub fn para_height(&self, s: &str, st: TextStyle, width: f32) -> f32 {
        self.p.label_in(s, st, width, Align::Start).h
    }

    // ----- controls -----

    /// Width of a button for `text`: padding 16 and a 1 px border on each side.
    pub fn button_width(&self, s: &str, kind: Kind) -> f32 {
        self.p.label(s, button_style(kind)).w + 32.0 + 2.0 * self.p.hairline()
    }

    /// A 40 px button (radius 8, padding 0 16). `start` aligns the label to the start (sidebar entries).
    pub fn button(&mut self, id: ControlId, rect: Rect, kind: Kind, s: &str, start: bool) {
        let (p, pal) = (self.p, self.pal);
        let style = button_style(kind);
        let label = p.label(s, style);
        let radius = layout::BUTTON_RADIUS;
        match kind {
            Kind::Default => p.bordered(rect, radius, pal.line, pal.surface),
            Kind::Primary => p.fill_rrect(rect, radius, pal.link),
            Kind::Quiet => {}
            Kind::Danger => {
                let bw = p.hairline();
                p.stroke_rrect(rect.inset(bw / 2.0), radius - bw / 2.0, bw, pal.danger);
            }
            Kind::DangerSolid => p.fill_rrect(rect, radius, pal.danger),
        }
        match kind {
            Kind::Primary => self.tint(id, rect, radius, pal.link_ink, 0.14, 0.24),
            _ => self.tint(id, rect, radius, pal.ink, 0.06, 0.11),
        }
        let color = match kind {
            Kind::Primary => pal.link_ink,
            Kind::Danger => pal.danger,
            Kind::DangerSolid => Color::rgb(0xFFFFFF),
            _ => pal.ink,
        };
        let tx = if start { rect.x + p.hairline() + 16.0 } else { rect.x + (rect.w - label.w) / 2.0 };
        p.draw(&label, tx, rect.y + (rect.h - label.h) / 2.0, color);
        self.register(id, rect, radius);
    }

    /// A switch (38 x 22, radius 11) at `(x, y)`; `waiting` dims it while the service has not answered.
    pub fn switch(&mut self, id: ControlId, x: f32, y: f32, on: bool, waiting: bool) {
        let p = self.p;
        let r = Rect::new(x, y, layout::SWITCH_W, layout::SWITCH_H);
        let bg = if on { self.pal.link } else { self.pal.line };
        let bg = if waiting { bg.with_alpha(0.55) } else { bg };
        p.fill_rrect(r, 11.0, bg);
        let kx = if on { r.right() - 3.0 - 16.0 } else { r.x + 3.0 };
        p.fill_circle((kx + 8.0, r.y + 11.0), 8.0, Color::rgb(0xFFFFFF));
        self.register(id, r, 11.0);
    }

    /// An 18 px checkbox with its label to the right; the whole row (min-height 32) is the hit box.
    pub fn checkbox(&mut self, id: ControlId, x: f32, y: f32, width: f32, on: bool, s: &str) {
        let (p, pal) = (self.p, self.pal);
        let row = Rect::new(x, y, width, 32.0);
        let box_r = Rect::new(x, y + 7.0, 18.0, 18.0);
        if on {
            p.fill_rrect(box_r, 4.0, pal.link);
            p.svg("M5 12.5l4.5 4.5L19 7.5", box_r.x, box_r.y, 18.0, 2.6, pal.link_ink);
        } else {
            p.bordered(box_r, 4.0, pal.soft, pal.surface);
            p.fill_rrect(box_r.inset(p.hairline() * 1.5), 3.0, pal.surface);
        }
        let l = p.label(s, text::BODY);
        p.draw(&l, x + 18.0 + 10.0, y + (32.0 - l.h) / 2.0, pal.ink);
        self.register(id, row, 6.0);
    }

    /// A bordered panel (radius 12).
    /// Registers the scrolling content `key` of the main window with its largest offset and gives the offset to
    /// paint it at: the window's own while it shows the same content, the top when it comes from another screen.
    pub fn page_scroll(&mut self, key: u8, max: f32) -> f32 {
        self.out.scroll_max = max;
        self.out.scroll_key = key;
        if self.ix.scroll_key == key { self.ix.scroll.clamp(0.0, max) } else { 0.0 }
    }

    pub fn panel(&self, r: Rect, fill: Color) {
        self.p.bordered(r, layout::PANEL_RADIUS, self.pal.line, fill);
    }

    /// A neutral placeholder block (R-UI-1): same footprint as the content that will replace it.
    pub fn block(&self, r: Rect, radius: f32) {
        self.p.fill_rrect(r, radius, self.pal.line.with_alpha(0.55));
    }

    /// A select-like field: bordered box, value, chevron. Returns nothing; registers the control.
    pub fn select_field(&mut self, id: ControlId, r: Rect, value: &str) {
        let (p, pal) = (self.p, self.pal);
        p.bordered(r, layout::BUTTON_RADIUS, pal.line, pal.surface);
        self.tint(id, r, layout::BUTTON_RADIUS, pal.ink, 0.04, 0.08);
        let l = p.label_ellipsis(value, text::BODY, r.w - 20.0 - 26.0);
        p.draw(&l, r.x + 10.0 + p.hairline(), r.y + (r.h - l.h) / 2.0, pal.ink);
        p.svg("M6 9l6 6 6-6", r.right() - 10.0 - 16.0, r.y + (r.h - 16.0) / 2.0, 16.0, 2.0, pal.soft);
        self.register(id, r, layout::BUTTON_RADIUS);
    }

    /// A text input field frame; the native input goes inside it (or the value is drawn, offscreen).
    pub fn edit_field(&mut self, id: EditId, r: Rect, value: &str, placeholder: &str) {
        let (p, pal) = (self.p, self.pal);
        p.bordered(r, layout::BUTTON_RADIUS, pal.line, pal.surface);
        let inner = Rect::new(
            r.x + 10.0 + p.hairline(),
            r.y + p.hairline(),
            r.w - 20.0 - 2.0 * p.hairline(),
            r.h - 2.0 * p.hairline(),
        );
        if self.offscreen {
            let (s, c) = if value.is_empty() { (placeholder, pal.soft) } else { (value, pal.ink) };
            let l = p.label_ellipsis(s, text::BODY, inner.w);
            p.draw(&l, inner.x, inner.y + (inner.h - l.h) / 2.0, c);
        }
        self.out.edits.push(EditSlot { id, rect: inner, value: value.to_owned(), placeholder: placeholder.to_owned() });
        self.register(ControlId::Edit(id), r, layout::BUTTON_RADIUS);
    }

    /// `:focus-visible`: a 2 px outline 2 px outside the box, following its rounding.
    pub fn focus_ring(&self, on_dark: bool) {
        if !self.ix.focus_visible {
            return;
        }
        let Some(w) = self.out.widgets.iter().find(|w| Some(w.id) == self.ix.focus) else { return };
        let radius = if w.radius > 0.0 { w.radius + 3.0 } else { 0.0 };
        let color = if on_dark { swapdesk_ui::tokens::fixed::TOOLBAR_LINK } else { self.pal.link };
        self.p.stroke_rrect(
            Rect::new(w.rect.x - 3.0, w.rect.y - 3.0, w.rect.w + 6.0, w.rect.h + 6.0),
            radius,
            2.0,
            color,
        );
    }

    /// A card icon: a 40 px circle with a 20 px icon.
    pub fn icon_circle(&self, x: f32, y: f32, bg: Color, fg: Color, path: &str) {
        self.p.fill_circle((x + 20.0, y + 20.0), 20.0, bg);
        self.p.svg(path, x + 10.0, y + 10.0, 20.0, 2.0, fg);
    }

    /// The pair strip's laptop: the design's 64 x 64 SVG scaled to `size`.
    pub fn laptop(&self, x: f32, y: f32, size: f32, stroke: f32, color: Color) {
        let p = self.p;
        let (x, y) = (p.snap(x), p.snap(y));
        let s = size / 64.0;
        p.stroke_rrect(Rect::new(x + 10.0 * s, y + 14.0 * s, 44.0 * s, 28.0 * s), 3.0 * s, stroke, color);
        p.line((x + 4.0 * s, y + 48.0 * s), (x + 60.0 * s, y + 48.0 * s), stroke, color, false);
    }

    /// The wire with its arrowhead from `x0` to `x1` at `cy`: thickness 3, head 16 x 18 (or smaller).
    #[allow(clippy::too_many_arguments)] // drawing helpers take the geometry and colour the CSS gives them
    pub fn wire(&self, x0: f32, x1: f32, cy: f32, thickness: f32, head_w: f32, head_h: f32, color: Color) {
        self.p.fill_rrect(
            Rect::new(x0, cy - thickness / 2.0, (x1 - x0 - head_w).max(0.0), thickness),
            thickness / 2.0 - 0.01,
            color,
        );
        self.p.fill_triangle([(x1 - head_w, cy - head_h / 2.0), (x1, cy), (x1 - head_w, cy + head_h / 2.0)], color);
    }
}

/// Style of a button label.
pub fn button_style(kind: Kind) -> TextStyle {
    match kind {
        Kind::Primary | Kind::Danger | Kind::DangerSolid => text::BTN_PRIMARY,
        _ => text::BTN,
    }
}

/// Icons of the design (24-unit viewBox, stroke), one path each.
pub mod icons {
    pub const CHEVRON: &str = "M6 9l6 6 6-6";
    pub const FIT: &str = "M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5";
    pub const AUDIO: &str = "M4 10v4h4l5 4V6L8 10H4zM16 9a4 4 0 0 1 0 6";
    pub const KEYS_ARROW: &str = "M5 12h14M13 6l6 6-6 6";
    pub const TERMINAL_GLYPH: &str = "M7 9l3 3-3 3M12 15h5";
    pub const FOLDER: &str = "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z";
    pub const CHAT: &str = "M4 5h16v11H9l-5 4z";
    pub const SWAP: &str = "M7 7h11l-3-3M17 17H6l3 3";
    pub const CLOSE: &str = "M6 6l12 12M18 6L6 18";
    pub const SHIELD: &str = "M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z";
    pub const SHIELD_ALERT: &str = "M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6zM12 9v4M12 16h.01";
    pub const WIFI: &str = "M2 9a15 15 0 0 1 20 0M5.5 12.5a10 10 0 0 1 13 0M9 16a5 5 0 0 1 6 0M12 19.5h.01";
    pub const POWER: &str = "M12 3v9M6.3 6.3a8 8 0 1 0 11.4 0";
    pub const CLOCK: &str = "M21 12a9 9 0 1 1-18 0a9 9 0 1 1 18 0zM12 7v5l3 2";
    pub const KEY_DOTS: &str = "M6 10h.01M10 10h.01M14 10h.01M18 10h.01M7 14h10";

    pub fn lock() -> String {
        format!("{}M8 11V8a4 4 0 0 1 8 0v3", super::rrect_path(5.0, 11.0, 14.0, 9.0, 2.0))
    }
    pub fn keyboard() -> String {
        format!("{}{}", super::rrect_path(2.0, 6.0, 20.0, 12.0, 2.0), KEY_DOTS)
    }
    pub fn terminal() -> String {
        format!("{}{}", super::rrect_path(3.0, 4.0, 18.0, 16.0, 2.0), TERMINAL_GLYPH)
    }
}

/// The outcome of a frame, for the caller.
pub fn finish(f: Frame<'_>, on_dark_ring: bool) -> Output {
    f.focus_ring(on_dark_ring);
    f.out
}
