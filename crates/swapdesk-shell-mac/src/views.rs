//! AppKit controls built from the view models: labels, buttons, switches, fields, selects, stacks, the sidebar and the
//! centered card. Everything here only creates views; what a control does is decided by `swapdesk-ui` (through
//! `shell.rs`), and every control's accessible name, help and role come from the same model values `swapdesk-ui`'s
//! accessibility tree uses.
//!
//! Apple documentation: `NSStackView` (developer.apple.com/documentation/appkit/nsstackview), `NSButton`
//! (.../nsbutton: `buttonWithTitle:target:action:`, `checkboxWithTitle:target:action:`, `bezelColor`,
//! `contentTintColor`), `NSSwitch`, `NSPopUpButton`, `NSTextField` (`wrappingLabelWithString:`,
//! `preferredMaxLayoutWidth`, `placeholderString`), `NSBox` (custom box type), `NSFont.systemFont(ofSize:weight:)`
//! (weights: regular 0, medium 0.23, semibold 0.3, bold 0.4, from `NSFontWeight*`), `NSAccessibility`
//! (`accessibilityLabel`, `accessibilityHelp`, `accessibilityRole`: developer.apple.com/documentation/appkit/nsaccessibilityprotocol).

use std::cell::Cell;

use objc2::Message;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSBezelStyle, NSBox, NSBoxType, NSButton, NSButtonType, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSLayoutAttribute, NSPopUpButton, NSRectFill, NSStackView, NSSwitch, NSTextField,
    NSTitlePosition, NSUserInterfaceLayoutOrientation, NSView,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSEdgeInsets, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use swapdesk_ui::tokens::{Color, Palette, TextStyle, layout, text as tx};
use swapdesk_ui::{ControlId, EditId, SelectId};

use crate::model::*;

/// What a control does, found again from its `tag` when AppKit calls the target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Control(ControlId),
    Edit(EditId),
    Select(SelectId),
    ToolScreen,
    ToolQuality,
    ToolKeys,
    ToolSwitch(SwitchTool),
    ToolSwap,
    StripCancel(u64),
    StripDismiss,
}

/// Everything a builder needs: the action target, the palette and the registries of this build.
pub struct Ctx<'a> {
    pub mtm: MainThreadMarker,
    pub target: &'a AnyObject,
    pub pal: &'static Palette,
    pub actions: &'a mut Vec<Action>,
    /// 0 for the main window, 1 for the viewer: tags carry it so one controller serves both.
    pub parity: isize,
    pub fields: &'a mut Vec<(EditId, Retained<NSTextField>)>,
}

impl Ctx<'_> {
    fn tag(&mut self, a: Action) -> isize {
        self.actions.push(a);
        (self.actions.len() as isize - 1) * 2 + self.parity
    }
}

// ---------- a flipped, optionally painted view ----------

#[derive(Default)]
pub struct PaintedIvars {
    fill: Cell<Option<(f64, f64, f64, f64)>>,
}

define_class!(
    // SAFETY: NSView subclass overriding only `isFlipped` and `drawRect:`; no `Drop`.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskPaintedView"]
    #[ivars = PaintedIvars]
    pub struct PaintedView;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for PaintedView {}

    impl PaintedView {
        // SAFETY: the selector and signature are NSView's.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: the selector and signature are NSView's; `NSRectFill` is called inside `drawRect:`, where a graphics
        // context is current.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            if let Some((r, g, b, a)) = self.ivars().fill.get() {
                NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a).setFill();
                NSRectFill(dirty);
            }
        }
    }
);

impl PaintedView {
    pub fn create(mtm: MainThreadMarker, frame: NSRect, fill: Option<Color>) -> Retained<PaintedView> {
        let this = Self::alloc(mtm).set_ivars(PaintedIvars::default());
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let view: Retained<PaintedView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.set_fill(fill);
        view
    }

    pub fn set_fill(&self, fill: Option<Color>) {
        self.ivars().fill.set(fill.map(|c| {
            let [r, g, b, a] = c.to_f32();
            (f64::from(r), f64::from(g), f64::from(b), f64::from(a))
        }));
        self.setNeedsDisplay(true);
    }
}

// ---------- small helpers ----------

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

pub fn color(c: Color) -> Retained<NSColor> {
    let [r, g, b, a] = c.to_f32();
    NSColor::colorWithSRGBRed_green_blue_alpha(f64::from(r), f64::from(g), f64::from(b), f64::from(a))
}

/// The `NSFontWeight` value of a CSS-like weight (the documented constants).
fn weight(w: u16) -> f64 {
    match w {
        0..=450 => 0.0,
        451..=550 => 0.23,
        551..=650 => 0.3,
        _ => 0.4,
    }
}

pub fn font(style: TextStyle) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(f64::from(style.size), weight(style.weight))
}

/// Any view as an `NSView`.
pub fn view_of<T: AsRef<NSView> + objc2::Message>(v: &Retained<T>) -> Retained<NSView> {
    let view: &NSView = v.as_ref();
    view.retain()
}

pub fn size(w: f64, h: f64) -> NSSize {
    NSSize::new(w, h)
}

pub fn frame(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// Name, help and (where AppKit's default is wrong) role for a screen reader.
fn describe<T: NSAccessibility + Message>(view: &T, label: &str, help: Option<&str>, role: Option<&str>) {
    view.setAccessibilityLabel(Some(&ns(label)));
    if let Some(h) = help {
        view.setAccessibilityHelp(Some(&ns(h)));
    }
    if let Some(r) = role {
        view.setAccessibilityRole(Some(&ns(r)));
    }
}

fn label_view(ctx: &Ctx, text: &str, style: TextStyle, ink: Color, wrap: Option<f64>) -> Retained<NSTextField> {
    let field = match wrap {
        Some(_) => NSTextField::wrappingLabelWithString(&ns(text), ctx.mtm),
        None => NSTextField::labelWithString(&ns(text), ctx.mtm),
    };
    field.setFont(Some(&font(style)));
    field.setTextColor(Some(&color(ink)));
    if let Some(w) = wrap {
        field.setPreferredMaxLayoutWidth(w);
    }
    field
}

pub fn vstack(ctx: &Ctx, views: &[Retained<NSView>], spacing: f64) -> Retained<NSStackView> {
    let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(views), ctx.mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setAlignment(NSLayoutAttribute::Leading);
    stack.setSpacing(spacing);
    stack
}

pub fn hstack(ctx: &Ctx, views: &[Retained<NSView>], spacing: f64) -> Retained<NSStackView> {
    let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(views), ctx.mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    stack.setAlignment(NSLayoutAttribute::CenterY);
    stack.setSpacing(spacing);
    stack
}

/// Sizes a stack to what its content needs and returns that size.
pub fn fit(stack: &NSStackView) -> NSSize {
    let s = stack.fittingSize();
    stack.setFrameSize(s);
    s
}

// ---------- controls ----------

pub fn button(ctx: &mut Ctx, b: &ButtonModel) -> Retained<NSButton> {
    let tag = ctx.tag(Action::Control(b.id));
    button_with(ctx, &b.label, b.description.as_deref(), b.style, b.disabled, tag)
}

pub fn button_with(
    ctx: &mut Ctx,
    label: &str,
    help: Option<&str>,
    style: ButtonStyle,
    disabled: bool,
    tag: isize,
) -> Retained<NSButton> {
    // SAFETY: the target is the shell's controller, which implements `act:` and outlives its windows.
    let btn =
        unsafe { NSButton::buttonWithTitle_target_action(&ns(label), Some(ctx.target), Some(sel!(act:)), ctx.mtm) };
    btn.setTag(tag);
    btn.setEnabled(!disabled);
    match style {
        ButtonStyle::Primary => btn.setBezelColor(Some(&color(ctx.pal.link))),
        ButtonStyle::Danger => btn.setContentTintColor(Some(&color(ctx.pal.danger))),
        ButtonStyle::Link => {
            btn.setBordered(false);
            btn.setContentTintColor(Some(&color(ctx.pal.link)));
        }
        ButtonStyle::Plain => {}
    }
    describe(&*btn, label, help, None);
    btn
}

fn toggle(ctx: &mut Ctx, t: &ToggleModel) -> Retained<NSView> {
    let tag = ctx.tag(Action::Control(t.id));
    let state = if t.on { NSControlStateValueOn } else { NSControlStateValueOff };
    if t.check_box {
        // SAFETY: as in `button_with`.
        let c = unsafe {
            NSButton::checkboxWithTitle_target_action(&ns(&t.label), Some(ctx.target), Some(sel!(act:)), ctx.mtm)
        };
        c.setTag(tag);
        c.setState(state);
        c.setEnabled(!t.disabled);
        describe(&*c, &t.label, t.description.as_deref(), None);
        return view_of(&c);
    }
    let sw = NSSwitch::new(ctx.mtm);
    // SAFETY: as in `button_with`.
    unsafe {
        sw.setTarget(Some(ctx.target));
        sw.setAction(Some(sel!(act:)));
    }
    sw.setTag(tag);
    sw.setState(state);
    sw.setEnabled(!t.disabled);
    describe(&*sw, &t.label, t.description.as_deref(), None);
    let name = label_view(ctx, &t.label, tx::BODY, ctx.pal.ink, Some(380.0));
    let mut row = vec![view_of(&sw), view_of(&name)];
    if let Some(h) = &t.description {
        let hint = label_view(ctx, h, tx::SMALL, ctx.pal.soft, Some(380.0));
        let col = vstack(ctx, &[view_of(&name), view_of(&hint)], 2.0);
        row = vec![view_of(&sw), view_of(&col)];
    }
    view_of(&hstack(ctx, &row, 10.0))
}

fn field(ctx: &mut Ctx, f: &FieldModel, value: Option<&str>) -> Retained<NSView> {
    let tf = NSTextField::textFieldWithString(&ns(value.unwrap_or(&f.value)), ctx.mtm);
    tf.setPlaceholderString(Some(&ns(&f.placeholder)));
    tf.setTag(ctx.tag(Action::Edit(f.edit)));
    // SAFETY: as in `button_with`.
    unsafe {
        tf.setTarget(Some(ctx.target));
        tf.setAction(Some(sel!(act:)));
    }
    describe(&*tf, &f.label, None, None);
    ctx.fields.push((f.edit, tf.clone()));
    let caption = label_view(ctx, &f.label, tx::SMALL_BOLD, ctx.pal.soft, None);
    let mut column = vec![view_of(&caption)];
    if let Some(h) = &f.hint {
        let hint = label_view(ctx, h, tx::SMALL, ctx.pal.soft, Some(420.0));
        hint.setAccessibilityElement(false);
        column.push(view_of(&hint));
    }
    column.push(view_of(&tf));
    let col = vstack(ctx, &column, 4.0);
    tf.widthAnchor().constraintEqualToConstant(if f.hint.is_some() { 260.0 } else { 320.0 }).setActive(true);
    view_of(&col)
}

pub fn popup(
    ctx: &mut Ctx,
    titles: &[String],
    current: Option<usize>,
    label: &str,
    action: Action,
) -> Retained<NSPopUpButton> {
    let pop =
        NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(ctx.mtm), frame(0.0, 0.0, 160.0, 26.0), false);
    for t in titles {
        pop.addItemWithTitle(&ns(t));
    }
    if let Some(i) = current {
        pop.selectItemAtIndex(i as isize);
    }
    pop.setTag(ctx.tag(action));
    // SAFETY: as in `button_with`.
    unsafe {
        pop.setTarget(Some(ctx.target));
        pop.setAction(Some(sel!(act:)));
    }
    describe(&*pop, label, None, None);
    pop
}

fn select(ctx: &mut Ctx, s: &SelectModel) -> Retained<NSView> {
    let titles: Vec<String> = s.options.iter().map(|(t, _)| t.clone()).collect();
    let current = s.options.iter().position(|(_, on)| *on);
    let pop = popup(ctx, &titles, current, &s.label, Action::Select(s.select));
    if let Some(h) = &s.description {
        pop.setAccessibilityHelp(Some(&ns(h)));
    }
    let caption = label_view(ctx, &s.label, tx::SMALL_BOLD, ctx.pal.soft, None);
    view_of(&vstack(ctx, &[view_of(&caption), view_of(&pop)], 4.0))
}

fn skeleton(ctx: &Ctx, lines: u8) -> Retained<NSView> {
    let bars: Vec<Retained<NSView>> = (0..lines)
        .map(|i| {
            let bar = NSBox::new(ctx.mtm);
            bar.setBoxType(NSBoxType::Custom);
            bar.setTitlePosition(NSTitlePosition::NoTitle);
            bar.setBorderWidth(0.0);
            bar.setCornerRadius(6.0);
            bar.setFillColor(&color(ctx.pal.line));
            bar.widthAnchor().constraintEqualToConstant(if i % 2 == 0 { 420.0 } else { 300.0 }).setActive(true);
            bar.heightAnchor().constraintEqualToConstant(14.0).setActive(true);
            describe(&*bar, "", None, None);
            view_of(&bar)
        })
        .collect();
    let stack = vstack(ctx, &bars, 12.0);
    // Decorative: a screen reader skips the placeholder lines.
    stack.setAccessibilityElement(false);
    view_of(&stack)
}

/// The narrowest a home card gets, and the gap between cards.
const CARD_MIN_W: f64 = 300.0;
const CARD_GAP: f64 = 20.0;

/// The network line as a rounded pill (surface, or the danger tint when it warns).
fn pill(ctx: &Ctx, text: &str, warn: bool) -> Retained<NSView> {
    let ink = if warn { ctx.pal.danger } else { ctx.pal.soft };
    let l = label_view(ctx, text, tx::CAPTION, ink, None);
    let inner = l.fittingSize();
    let boxed = NSBox::new(ctx.mtm);
    boxed.setBoxType(NSBoxType::Custom);
    boxed.setTitlePosition(NSTitlePosition::NoTitle);
    boxed.setBorderWidth(0.0);
    boxed.setCornerRadius(18.0);
    boxed.setFillColor(&color(if warn { ctx.pal.danger_tint } else { ctx.pal.surface }));
    boxed.setContentViewMargins(size(14.0, 8.0));
    boxed.setContentView(Some(&view_of(&l)));
    let (w, h) = (inner.width + 28.0, 36.0);
    boxed.setFrameSize(size(w, h));
    boxed.widthAnchor().constraintEqualToConstant(w).setActive(true);
    boxed.heightAnchor().constraintEqualToConstant(h).setActive(true);
    view_of(&boxed)
}

/// A home card: the blocks on a surface with a border, padding 24 / 22.
fn card(ctx: &mut Ctx, blocks: &[Block], width: f64, values: &[(EditId, String)]) -> Retained<NSView> {
    let stack = blocks_stack(ctx, blocks, width - 48.0, values);
    let content = fit(&stack);
    let boxed = NSBox::new(ctx.mtm);
    boxed.setBoxType(NSBoxType::Custom);
    boxed.setTitlePosition(NSTitlePosition::NoTitle);
    boxed.setBorderWidth(1.0);
    boxed.setBorderColor(&color(ctx.pal.line));
    boxed.setCornerRadius(f64::from(layout::PANEL_RADIUS));
    boxed.setFillColor(&color(ctx.pal.surface));
    boxed.setContentViewMargins(size(24.0, 22.0));
    boxed.setContentView(Some(&view_of(&stack)));
    let h = content.height + 44.0;
    boxed.setFrameSize(size(width, h));
    boxed.widthAnchor().constraintEqualToConstant(width).setActive(true);
    boxed.heightAnchor().constraintEqualToConstant(h).setActive(true);
    view_of(&boxed)
}

/// One block as a view.
pub fn block(ctx: &mut Ctx, b: &Block, width: f64, values: &[(EditId, String)]) -> Retained<NSView> {
    match b {
        Block::Heading { text, level } => {
            let (style, ink) = if *level == 1 { (tx::H1, ctx.pal.ink) } else { (tx::H2, ctx.pal.ink) };
            let l = label_view(ctx, text, style, ink, Some(width));
            describe(&*l, text, None, Some("AXHeading"));
            view_of(&l)
        }
        Block::HeadingAction { text, level, action } => {
            let style = if *level == 1 { tx::H1 } else { tx::H2 };
            let l = label_view(ctx, text, style, ctx.pal.ink, Some(width));
            describe(&*l, text, None, Some("AXHeading"));
            let b = button(ctx, action);
            view_of(&hstack(ctx, &[view_of(&l), view_of(&b)], 12.0))
        }
        Block::FieldAction { field: f, action } => {
            let kept = values.iter().find(|(e, _)| *e == f.edit).map(|(_, v)| v.as_str());
            let input = field(ctx, f, kept);
            let btn = button(ctx, action);
            let row = hstack(ctx, &[input, view_of(&btn)], 8.0);
            row.setAlignment(NSLayoutAttribute::Bottom);
            view_of(&row)
        }
        Block::Pill { text, warn } => pill(ctx, text, *warn),
        Block::Cards(cards) => {
            // Two columns when the width allows two cards, one otherwise (the same rule as the Windows shell).
            let cols = if width >= 2.0 * CARD_MIN_W + CARD_GAP { 2 } else { 1 };
            let card_w = (width - CARD_GAP * (cols as f64 - 1.0)) / cols as f64;
            let views: Vec<Retained<NSView>> = cards.iter().map(|c| card(ctx, c, card_w, values)).collect();
            let rows: Vec<Retained<NSView>> = views
                .chunks(cols)
                .map(|r| {
                    let row = hstack(ctx, r, CARD_GAP);
                    row.setAlignment(NSLayoutAttribute::Top);
                    view_of(&row)
                })
                .collect();
            view_of(&vstack(ctx, &rows, CARD_GAP))
        }
        Block::ButtonRow(buttons) => {
            let views: Vec<Retained<NSView>> = buttons.iter().map(|b| view_of(&button(ctx, b))).collect();
            view_of(&hstack(ctx, &views, 8.0))
        }
        Block::Text(text) => view_of(&label_view(ctx, text, tx::BODY, ctx.pal.soft, Some(width))),
        Block::Entry { when, who, what, alert, label } => {
            let time = label_view(ctx, when, tx::SMALL, ctx.pal.soft, None);
            time.widthAnchor().constraintEqualToConstant(84.0).setActive(true);
            let name = label_view(ctx, who, tx::NAME, ctx.pal.ink, Some((width - 98.0).max(60.0)));
            let event = label_view(
                ctx,
                what,
                tx::CAPTION,
                if *alert { ctx.pal.danger } else { ctx.pal.soft },
                Some((width - 98.0).max(60.0)),
            );
            let col = if who.is_empty() {
                vstack(ctx, &[view_of(&event)], 0.0)
            } else {
                vstack(ctx, &[view_of(&name), view_of(&event)], 0.0)
            };
            let row = hstack(ctx, &[view_of(&time), view_of(&col)], 14.0);
            row.setAlignment(NSLayoutAttribute::Top);
            // One element for a screen reader, named as the Atividade row is.
            row.setAccessibilityElement(true);
            describe(&*row, label, None, Some("AXGroup"));
            view_of(&row)
        }
        Block::Row(text) => view_of(&label_view(ctx, text, tx::SMALL, ctx.pal.ink, Some(width))),
        Block::Code { digits, label } => {
            let l = label_view(ctx, digits, tx::PAIR_CODE, ctx.pal.ink, None);
            l.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(f64::from(tx::PAIR_CODE.size), 0.3)));
            // The screen reader says "Código 482 913", not the bare digits.
            describe(&*l, label, None, Some("AXStaticText"));
            view_of(&l)
        }
        Block::Button(btn) => view_of(&button(ctx, btn)),
        Block::Toggle(t) => toggle(ctx, t),
        Block::Field(f) => {
            let kept = values.iter().find(|(e, _)| *e == f.edit).map(|(_, v)| v.as_str());
            field(ctx, f, kept)
        }
        Block::Select(s) => select(ctx, s),
        Block::Tabs(tabs) => {
            let btns: Vec<Retained<NSView>> = tabs
                .iter()
                .map(|t| {
                    let tag = ctx.tag(Action::Control(t.id));
                    let b = button_with(ctx, &t.label, None, ButtonStyle::Plain, false, tag);
                    b.setButtonType(NSButtonType::PushOnPushOff);
                    b.setBezelStyle(NSBezelStyle::Push);
                    b.setState(if t.selected { NSControlStateValueOn } else { NSControlStateValueOff });
                    view_of(&b)
                })
                .collect();
            view_of(&hstack(ctx, &btns, 6.0))
        }
        Block::Skeleton(n) => skeleton(ctx, *n),
    }
}

pub fn blocks_stack(ctx: &mut Ctx, blocks: &[Block], width: f64, values: &[(EditId, String)]) -> Retained<NSStackView> {
    let views: Vec<Retained<NSView>> = blocks.iter().map(|b| block(ctx, b, width, values)).collect();
    let stack = vstack(ctx, &views, 14.0);
    stack.setEdgeInsets(NSEdgeInsets { top: 0.0, left: 0.0, bottom: 0.0, right: 0.0 });
    fit(&stack);
    stack
}

/// The connect-flow line: text, countdown or reason, and its button.
pub fn strip(ctx: &mut Ctx, s: &Strip, width: f64) -> Retained<NSView> {
    let ink = if s.kind == StripKind::Ended { ctx.pal.danger } else { ctx.pal.ink };
    let mut row = vec![view_of(&label_view(ctx, &s.text, tx::NAME, ink, None))];
    if let Some(d) = &s.detail {
        row.push(view_of(&label_view(ctx, d, tx::BODY, ctx.pal.soft, None)));
    }
    if let Some((label, session)) = &s.cancel {
        let tag = ctx.tag(Action::StripCancel(*session));
        row.push(view_of(&button_with(ctx, label, None, ButtonStyle::Plain, false, tag)));
    }
    if let Some(label) = &s.dismiss {
        let tag = ctx.tag(Action::StripDismiss);
        row.push(view_of(&button_with(ctx, label, None, ButtonStyle::Plain, false, tag)));
    }
    let stack = hstack(ctx, &row, 12.0);
    let inner = fit(&stack);
    let boxed = NSBox::new(ctx.mtm);
    boxed.setBoxType(NSBoxType::Custom);
    boxed.setTitlePosition(NSTitlePosition::NoTitle);
    boxed.setBorderWidth(0.0);
    boxed.setCornerRadius(f64::from(layout::BUTTON_RADIUS));
    boxed.setFillColor(&color(if s.kind == StripKind::Ended { ctx.pal.danger_tint } else { ctx.pal.link_tint }));
    boxed.setContentViewMargins(size(12.0, 8.0));
    boxed.setContentView(Some(&view_of(&stack)));
    boxed.setFrameSize(size(width.max(inner.width + 24.0), inner.height + 16.0));
    describe(&*boxed, &s.text, s.detail.as_deref(), Some("AXGroup"));
    view_of(&boxed)
}

/// A navigation item of the sidebar (Início, Ajustes), the selected one pressed in.
fn nav_button(ctx: &mut Ctx, n: &NavItem) -> Retained<NSView> {
    let tag = ctx.tag(Action::Control(n.id));
    let b = button_with(ctx, &n.label, None, ButtonStyle::Plain, false, tag);
    b.setButtonType(NSButtonType::PushOnPushOff);
    b.setBezelStyle(NSBezelStyle::AccessoryBar);
    b.setState(if n.selected { NSControlStateValueOn } else { NSControlStateValueOff });
    view_of(&b)
}

/// The sidebar: the app's name, Início, the device lists and Ajustes at the bottom.
pub fn sidebar(ctx: &mut Ctx, sb: &Sidebar, height: f64) -> Retained<PaintedView> {
    let w = f64::from(layout::SIDEBAR_W);
    let root = PaintedView::create(ctx.mtm, frame(0.0, 0.0, w, height), Some(ctx.pal.sunken));
    root.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable);

    let mut top: Vec<Retained<NSView>> = Vec::new();
    top.push(view_of(&label_view(ctx, &sb.app_name, tx::LOGO, ctx.pal.ink, None)));
    top.push(nav_button(ctx, &sb.home));
    if sb.loading {
        top.push(skeleton(ctx, 4));
    }
    for g in &sb.groups {
        top.push(view_of(&label_view(ctx, &g.title, tx::GROUP, ctx.pal.soft, None)));
        for r in &g.rows {
            let tag = ctx.tag(Action::Control(r.id));
            let b = button_with(ctx, &r.name, None, ButtonStyle::Plain, false, tag);
            b.setButtonType(NSButtonType::PushOnPushOff);
            b.setBezelStyle(NSBezelStyle::AccessoryBar);
            b.setState(if r.selected { NSControlStateValueOn } else { NSControlStateValueOff });
            // The row reads "Name, status" exactly as swapdesk-ui's tree does; the status line below is decorative.
            describe(&*b, &r.label, None, None);
            let status = label_view(ctx, &r.status, tx::SMALL, ctx.pal.soft, Some(w - 60.0));
            status.setAccessibilityElement(false);
            let col = vstack(ctx, &[view_of(&b), view_of(&status)], 0.0);
            top.push(view_of(&col));
        }
    }
    let upper = vstack(ctx, &top, 8.0);
    fit(&upper);
    upper.setFrameOrigin(NSPoint::new(20.0, 20.0));
    root.addSubview(&upper);

    let bottom: Vec<Retained<NSView>> = sb.nav.iter().map(|n| nav_button(ctx, n)).collect();
    let lower = vstack(ctx, &bottom, 8.0);
    let s = fit(&lower);
    lower.setFrameOrigin(NSPoint::new(20.0, (height - s.height - 20.0).max(0.0)));
    lower.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin);
    root.addSubview(&lower);
    let root_view: &NSView = &root;
    describe(root_view, "Dispositivos", None, Some("AXGroup"));
    root
}
