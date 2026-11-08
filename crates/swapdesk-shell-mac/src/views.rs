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
    let col = vstack(ctx, &[view_of(&caption), view_of(&tf)], 4.0);
    // The field is as wide as the column it sits in.
    let width = tf.widthAnchor().constraintEqualToConstant(if f.edit == EditId::Address {
        f64::from(layout::SIDEBAR_W) - 40.0
    } else {
        320.0
    });
    width.setActive(true);
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

/// One block as a view.
pub fn block(ctx: &mut Ctx, b: &Block, width: f64, values: &[(EditId, String)]) -> Retained<NSView> {
    match b {
        Block::Heading { text, level } => {
            let (style, ink) = if *level == 1 { (tx::H1, ctx.pal.ink) } else { (tx::H2, ctx.pal.ink) };
            let l = label_view(ctx, text, style, ink, Some(width));
            describe(&*l, text, None, Some("AXHeading"));
            view_of(&l)
        }
        Block::Text(text) => view_of(&label_view(ctx, text, tx::BODY, ctx.pal.soft, Some(width))),
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

/// The sidebar: this computer, the device list, the address field, pairing and navigation.
pub fn sidebar(ctx: &mut Ctx, sb: &Sidebar, height: f64, values: &[(EditId, String)]) -> Retained<PaintedView> {
    let w = f64::from(layout::SIDEBAR_W);
    let root = PaintedView::create(ctx.mtm, frame(0.0, 0.0, w, height), Some(ctx.pal.sunken));
    root.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable);

    let mut top: Vec<Retained<NSView>> = Vec::new();
    top.push(view_of(&label_view(ctx, &sb.this_name, tx::NAME, ctx.pal.ink, None)));
    top.push(view_of(&label_view(ctx, &sb.this_status, tx::SMALL, ctx.pal.soft, Some(w - 40.0))));
    top.push(view_of(&label_view(ctx, &sb.network_line, tx::SMALL, ctx.pal.soft, Some(w - 40.0))));
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
    top.push(field_in_sidebar(ctx, &sb.address, values));
    let upper = vstack(ctx, &top, 8.0);
    fit(&upper);
    upper.setFrameOrigin(NSPoint::new(20.0, 20.0));
    root.addSubview(&upper);

    let mut bottom: Vec<Retained<NSView>> = vec![view_of(&button(ctx, &sb.pair))];
    for n in &sb.nav {
        let tag = ctx.tag(Action::Control(n.id));
        let b = button_with(ctx, &n.label, None, ButtonStyle::Plain, false, tag);
        b.setButtonType(NSButtonType::PushOnPushOff);
        b.setBezelStyle(NSBezelStyle::AccessoryBar);
        b.setState(if n.selected { NSControlStateValueOn } else { NSControlStateValueOff });
        bottom.push(view_of(&b));
    }
    let lower = vstack(ctx, &bottom, 8.0);
    let s = fit(&lower);
    lower.setFrameOrigin(NSPoint::new(20.0, (height - s.height - 20.0).max(0.0)));
    lower.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin);
    root.addSubview(&lower);
    let root_view: &NSView = &root;
    describe(root_view, "Dispositivos", None, Some("AXGroup"));
    root
}

fn field_in_sidebar(ctx: &mut Ctx, f: &FieldModel, values: &[(EditId, String)]) -> Retained<NSView> {
    block(ctx, &Block::Field(f.clone()), f64::from(layout::SIDEBAR_W) - 40.0, values)
}
