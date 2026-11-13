//! The card that stands alone in its area (D51): a wait, a state, an empty page. It is centred in that area, its own
//! content is centred (icon, title, text, the buttons as one group) and it is wide enough to be read at a glance.
//! Every such card of both windows is painted here, so they share one look.

use swapdesk_ui::tokens::{Color, TextStyle, layout, text};
use swapdesk_ui::{ControlId, WaitBar};

use crate::painter::{Align, Rect};
use crate::view::{Frame, Kind};

/// The width of a card that stands alone, when its area has the room.
pub const W: f32 = 480.0;
/// Centred text is not laid out wider than this: longer lines are hard to read from the middle.
const TEXT_MAX: f32 = 560.0;
const ICON: f32 = 40.0;
/// Between the buttons of a centred group.
const ACTIONS_GAP: f32 = 12.0;

#[derive(Clone, Copy)]
pub enum Icon<'a> {
    /// A tinted circle with an icon: the circle's colour, the icon's colour, the icon's path.
    Circle(Color, Color, &'a str),
    /// A sunken circle with a hairline ring: something that ended.
    Ring(&'a str),
    /// The turning arc of something in progress.
    Spinner,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// Alone in a page or over the video: padding 32, the dialog's title.
    Alone,
    /// The same card over the remote picture while there is none: dark, so it does not glare.
    Dark,
    /// One card among a page's other cards: their padding and their title size.
    Inline,
}

struct Metrics {
    pad_x: f32,
    pad_y: f32,
    after_icon: f32,
    title: TextStyle,
    after_title: f32,
    before_bar: f32,
    before_actions: f32,
}

impl Look {
    fn metrics(self) -> Metrics {
        match self {
            Look::Alone | Look::Dark => Metrics {
                pad_x: 32.0,
                pad_y: 32.0,
                after_icon: 20.0,
                title: text::H1_DIALOG,
                after_title: 12.0,
                before_bar: 20.0,
                before_actions: 28.0,
            },
            Look::Inline => Metrics {
                pad_x: 22.0,
                pad_y: 20.0,
                after_icon: 10.0,
                title: text::H2_STATE,
                after_title: 10.0,
                before_bar: 12.0,
                before_actions: 16.0,
            },
        }
    }
}

pub struct Card<'a> {
    pub look: Look,
    pub icon: Option<Icon<'a>>,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    /// The countdown row of a wait: there for the whole wait, so the card never grows.
    pub bar: Option<&'a WaitBar>,
    pub buttons: &'a [(ControlId, String, Kind)],
}

impl Card<'_> {
    /// The width of the text inside a card `w` wide.
    fn text_w(&self, f: &Frame, w: f32) -> f32 {
        (w - 2.0 * self.look.metrics().pad_x - 2.0 * f.p.hairline()).min(TEXT_MAX)
    }

    /// The height the card takes at width `w`.
    pub fn height(&self, f: &Frame, w: f32) -> f32 {
        let p = f.p;
        let m = self.look.metrics();
        let tw = self.text_w(f, w);
        let mut h = 0.0;
        // Each part after the first is preceded by its own gap.
        let add = |h: &mut f32, gap: f32, part: f32| {
            if *h > 0.0 {
                *h += gap;
            }
            *h += part;
        };
        if self.icon.is_some() {
            add(&mut h, 0.0, ICON);
        }
        if let Some(t) = self.title {
            add(&mut h, m.after_icon, p.label_in(t, m.title, tw, Align::Center).h);
        }
        if let Some(b) = self.body {
            let gap = if self.title.is_some() { m.after_title } else { m.after_icon };
            add(&mut h, gap, p.label_in(b, text::BODY, tw, Align::Center).h);
        }
        if self.bar.is_some() {
            add(&mut h, m.before_bar, f.countdown_row_h());
        }
        if !self.buttons.is_empty() {
            add(&mut h, m.before_actions, layout::BUTTON_H);
        }
        2.0 * p.hairline() + 2.0 * m.pad_y + h
    }

    /// Paints the card `w` wide (or as wide as `area`), centred in `area`; an area too short keeps the card at its
    /// top. Returns the card's rectangle.
    pub fn paint_in(&self, f: &mut Frame, area: Rect, w: f32) -> Rect {
        let w = w.min(area.w);
        let h = self.height(f, w);
        let x = f.p.snap(area.x + (area.w - w) / 2.0);
        let y = f.p.snap(area.y + ((area.h - h) / 2.0).max(0.0));
        self.paint(f, Rect::new(x, y, w, h))
    }

    /// Paints the card in `r`, at least as tall as [`Card::height`]: the buttons sit on its bottom line.
    pub fn paint(&self, f: &mut Frame, r: Rect) -> Rect {
        let (p, pal) = (f.p, f.pal);
        let m = self.look.metrics();
        let dark = self.look == Look::Dark;
        let bw = p.hairline();
        let (ink, soft) = if dark { (f.fx.card_dark_ink, f.fx.card_dark_soft) } else { (pal.ink, pal.soft) };
        if dark {
            p.bordered(r, layout::PANEL_RADIUS, f.fx.card_dark_border, pal.remote);
        } else {
            f.panel(r, pal.surface);
        }
        let inner = r.w - 2.0 * m.pad_x - 2.0 * bw;
        let tw = self.text_w(f, r.w);
        let ix = r.x + bw + m.pad_x;
        let tx = ix + (inner - tw) / 2.0;
        let mut y = r.y + bw + m.pad_y;
        let mut first = true;
        let mut step = |y: &mut f32, gap: f32| {
            if !first {
                *y += gap;
            }
            first = false;
        };
        if let Some(icon) = self.icon {
            step(&mut y, 0.0);
            let x = ix + (inner - ICON) / 2.0;
            let c = (x + ICON / 2.0, y + ICON / 2.0);
            match icon {
                Icon::Circle(tint, fg, path) => f.icon_circle(x, y, tint, fg, path),
                Icon::Ring(path) => {
                    p.fill_circle(c, 20.0, pal.sunken);
                    p.stroke_circle(c, 19.5, 1.0, pal.line);
                    p.svg(path, x + 10.0, y + 10.0, 20.0, 2.0, pal.soft);
                }
                Icon::Spinner => p.arc_stroke(c, 18.5, -90.0 + 45.0, 270.0, 3.0, f.fx.spinner),
            }
            y += ICON;
        }
        if let Some(t) = self.title {
            step(&mut y, m.after_icon);
            let l = p.label_in(t, m.title, tw, Align::Center);
            p.draw(&l, tx, y, ink);
            y += l.h;
        }
        if let Some(b) = self.body {
            step(&mut y, if self.title.is_some() { m.after_title } else { m.after_icon });
            let l = p.label_in(b, text::BODY, tw, Align::Center);
            p.draw(&l, tx, y, soft);
            y += l.h;
        }
        if let Some(bar) = self.bar {
            step(&mut y, m.before_bar);
            let colors = if dark {
                [f.fx.card_dark_border, f.fx.spinner, f.fx.card_dark_soft]
            } else {
                [pal.line, pal.link, pal.soft]
            };
            f.countdown_row((tx, y), tw, bar, colors);
        }
        if !self.buttons.is_empty() {
            actions(f, ix, r.bottom() - bw - m.pad_y - layout::BUTTON_H, inner, self.buttons, dark);
        }
        r
    }
}

/// The buttons of a card or a dialog as one group centred across `w`, in the order of the rule: the negative ones
/// on the left, the positive ones (`Kind::Primary`) on the right. `dark` draws them for a dark card.
pub fn actions(f: &mut Frame, x: f32, y: f32, w: f32, list: &[(ControlId, String, Kind)], dark: bool) {
    let mut list: Vec<&(ControlId, String, Kind)> = list.iter().collect();
    list.sort_by_key(|b| b.2 == Kind::Primary);
    let widths: Vec<f32> = list.iter().map(|(_, label, kind)| f.button_width(label, *kind)).collect();
    let total = widths.iter().sum::<f32>() + ACTIONS_GAP * widths.len().saturating_sub(1) as f32;
    let mut bx = f.p.snap(x + (w - total) / 2.0);
    for ((id, label, kind), bw) in list.into_iter().zip(widths) {
        let r = Rect::new(bx, y, bw, layout::BUTTON_H);
        if dark {
            dark_button(f, *id, r, label);
        } else {
            f.button(*id, r, *kind, label, false);
        }
        bx += bw + ACTIONS_GAP;
    }
}

/// A plain button on a dark card: a hairline outline and light text.
fn dark_button(f: &mut Frame, id: ControlId, r: Rect, label: &str) {
    let p = f.p;
    let radius = layout::BUTTON_RADIUS;
    p.stroke_rrect(r.inset(0.5), radius, 1.0, f.fx.card_dark_border);
    f.tint(id, r, radius, Color::rgb(0xFFFFFF), 0.08, 0.14);
    let l = p.label(label, text::BTN);
    p.draw(&l, r.x + (r.w - l.w) / 2.0, r.y + (r.h - l.h) / 2.0, f.fx.card_dark_ink);
    f.register(id, r, radius);
}
