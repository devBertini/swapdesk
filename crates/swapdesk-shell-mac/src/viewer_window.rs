//! The viewer window: the video host view filling the window, the floating toolbar over it (display picker, quality,
//! fit, audio, view-only, system shortcuts, send keys, the "Inverter" slot, disconnect) and the card over the video for
//! connecting, reconnecting and the swap questions.
//!
//! Apple documentation: `NSVisualEffectView` (developer.apple.com/documentation/appkit/nsvisualeffectview: material
//! `.hudWindow`, blending mode `.withinWindow`), `NSImage(systemSymbolName:accessibilityDescription:)`
//! (developer.apple.com/documentation/appkit/nsimage/3622472-init), `NSWindow.acceptsMouseMovedEvents`,
//! `NSWindowDelegate.windowShouldClose(_:)`, `CALayer.cornerRadius` (developer.apple.com/documentation/quartzcore/calayer).
//! SF Symbol names used: display, slider.horizontal.3, arrow.up.left.and.arrow.down.right, speaker.wave.2, eye,
//! command, keyboard, arrow.left.arrow.right, xmark.circle (names from Apple's SF Symbols catalogue; not verified on a
//! Mac in this session).

use std::cell::RefCell;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSBackingStoreType, NSBox, NSBoxType, NSButtonType,
    NSCellImagePosition, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSTitlePosition, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint};
use swapdesk_ui::tokens::{Palette, layout, palette, text as tx};

use crate::model::*;
use crate::video::{VideoSurface, VideoView};
use crate::views::*;

pub struct ViewerWindow {
    pub window: Retained<NSWindow>,
    pub video: Retained<VideoView>,
    pub actions: RefCell<Vec<Action>>,
    chrome: RefCell<Vec<Retained<NSView>>>,
    pub last: RefCell<Option<ViewerModel>>,
}

fn symbol(tool: &Tool) -> Option<&'static str> {
    Some(match tool {
        Tool::Screen { .. } => "display",
        Tool::Quality { .. } => "slider.horizontal.3",
        Tool::Switch { tool: SwitchTool::Fit, .. } => "arrow.up.left.and.arrow.down.right",
        Tool::Switch { tool: SwitchTool::Audio, .. } => "speaker.wave.2",
        Tool::Switch { tool: SwitchTool::ViewOnly, .. } => "eye",
        Tool::Switch { tool: SwitchTool::Grab, .. } => "command",
        Tool::Keys { .. } => "keyboard",
        Tool::Swap { .. } => "arrow.left.arrow.right",
        Tool::Disconnect { .. } => "xmark.circle",
    })
}

impl ViewerWindow {
    pub fn create(mtm: MainThreadMarker, delegate: &ProtocolObject<dyn NSWindowDelegate>, title: &str) -> ViewerWindow {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // SAFETY: the window is kept in `Shell` and `setReleasedWhenClosed(false)` is set below, as AppKit requires for
        // windows created outside a window controller.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame(0.0, 0.0, 1280.0, 800.0),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: see above.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns(title));
        window.setContentMinSize(size(640.0, 400.0));
        window.setAcceptsMouseMovedEvents(true);
        window.setDelegate(Some(delegate));
        let video = VideoView::create(mtm, frame(0.0, 0.0, 1280.0, 800.0));
        window.setContentView(Some(&view_of(&video)));
        window.center();
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&video));
        ViewerWindow {
            window,
            video,
            actions: RefCell::new(Vec::new()),
            chrome: RefCell::new(Vec::new()),
            last: RefCell::new(None),
        }
    }

    pub fn surface(&self) -> Option<VideoSurface> {
        let layer = self.video.layer()?;
        Some(VideoSurface { view: view_of(&self.video), layer, area: self.video.area() })
    }

    /// Rebuilds the toolbar and the card when the model changed.
    pub fn update(&self, mtm: MainThreadMarker, target: &AnyObject, model: &ViewerModel, show_swap: bool) {
        if self.last.borrow().as_ref() == Some(model) {
            return;
        }
        self.window.setTitle(&ns(&model.title));
        for v in self.chrome.borrow_mut().drain(..) {
            v.removeFromSuperview();
        }
        let pal: &'static Palette = palette(model.dark);
        let mut actions = self.actions.borrow_mut();
        actions.clear();
        let mut fields = Vec::new();
        let mut ctx = Ctx { mtm, target, pal, actions: &mut actions, parity: PARITY_VIEWER, fields: &mut fields };
        let bounds = self.video.bounds();
        let mut chrome: Vec<Retained<NSView>> = Vec::new();

        // The card over the video.
        if let Some(blocks) = &model.overlay {
            let cover = PaintedView::create(mtm, bounds, Some(pal.remote.with_alpha(0.55)));
            cover.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            let stack = blocks_stack(&mut ctx, blocks, f64::from(layout::DIALOG_W) - 64.0, &[]);
            let inner = stack.frame().size;
            let card = NSBox::new(mtm);
            card.setBoxType(NSBoxType::Custom);
            card.setTitlePosition(NSTitlePosition::NoTitle);
            card.setBorderWidth(0.0);
            card.setCornerRadius(f64::from(layout::DIALOG_RADIUS));
            card.setFillColor(&color(pal.surface));
            card.setContentViewMargins(size(32.0, 28.0));
            card.setContentView(Some(&view_of(&stack)));
            let (w, h) = (f64::from(layout::DIALOG_W).max(inner.width + 64.0), inner.height + 56.0);
            card.setFrame(frame((bounds.size.width - w) / 2.0, (bounds.size.height - h) / 2.0, w, h));
            card.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin
                    | NSAutoresizingMaskOptions::ViewMaxXMargin
                    | NSAutoresizingMaskOptions::ViewMinYMargin
                    | NSAutoresizingMaskOptions::ViewMaxYMargin,
            );
            cover.addSubview(&card);
            self.video.addSubview(&cover);
            chrome.push(view_of(&cover));
        }

        // The floating toolbar.
        let mut items: Vec<Retained<NSView>> = Vec::new();
        for t in &model.tools {
            if let Some(v) = tool_view(&mut ctx, t, show_swap) {
                items.push(v);
            }
        }
        let status = match &model.stats {
            Some(s) => format!("{}  {}", model.status, s),
            None => model.status.clone(),
        };
        let status_label = {
            let l = objc2_app_kit::NSTextField::labelWithString(&ns(&status), mtm);
            l.setFont(Some(&font(tx::BAR)));
            l.setTextColor(Some(&color(swapdesk_ui::tokens::fixed::TOOLBAR_INK)));
            l
        };
        items.push(view_of(&status_label));
        let row = hstack(&ctx, &items, 8.0);
        let inner = fit(&row);
        let bar = NSVisualEffectView::new(mtm);
        bar.setMaterial(NSVisualEffectMaterial::HUDWindow);
        bar.setBlendingMode(NSVisualEffectBlendingMode::WithinWindow);
        bar.setState(NSVisualEffectState::Active);
        bar.setWantsLayer(true);
        if let Some(layer) = bar.layer() {
            layer.setCornerRadius(f64::from(layout::BUTTON_RADIUS) + 4.0);
            layer.setMasksToBounds(true);
        }
        let (w, h) = (inner.width + 24.0, f64::from(layout::VIEWER_BAR_H).max(inner.height + 12.0));
        row.setFrameOrigin(NSPoint::new(12.0, (h - inner.height) / 2.0));
        bar.addSubview(&row);
        bar.setFrame(frame((bounds.size.width - w) / 2.0, 12.0, w, h));
        bar.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin);
        bar.setAccessibilityRole(Some(&ns("AXToolbar")));
        bar.setAccessibilityLabel(Some(&ns(&model.title)));
        self.video.addSubview(&bar);
        chrome.push(view_of(&bar));
        *self.chrome.borrow_mut() = chrome;
        *self.last.borrow_mut() = Some(model.clone());
    }
}

fn tool_view(ctx: &mut Ctx, t: &Tool, show_swap: bool) -> Option<Retained<NSView>> {
    let help = |label: &str| Some(label.to_owned());
    match t {
        Tool::Screen { label, current, options } => {
            let titles: Vec<String> = if options.is_empty() {
                vec![current.clone()]
            } else {
                options.iter().map(|(_, n, _)| n.clone()).collect()
            };
            let cur = options.iter().position(|(_, _, on)| *on).or(Some(0));
            let pop = popup(ctx, &titles, cur, label, Action::ToolScreen);
            pop.setToolTip(Some(&ns(label)));
            Some(view_of(&pop))
        }
        Tool::Quality { label, options } => {
            let titles: Vec<String> = options.iter().map(|(_, n, _)| n.clone()).collect();
            let pop = popup(ctx, &titles, options.iter().position(|(_, _, on)| *on), label, Action::ToolQuality);
            pop.setToolTip(Some(&ns(label)));
            Some(view_of(&pop))
        }
        Tool::Keys { label, options } => {
            // A pull-down: its first item is the title, so the menu holds the label and then the chords.
            let mut titles = vec![String::new()];
            titles.extend(options.iter().map(|(_, n)| n.clone()));
            let pop = popup(ctx, &titles, None, label, Action::ToolKeys);
            pop.setPullsDown(true);
            pop.setToolTip(Some(&ns(label)));
            if let (Some(img), Some(item)) = (icon(t, label), pop.itemAtIndex(0)) {
                item.setImage(Some(&img));
            }
            Some(view_of(&pop))
        }
        Tool::Switch { tool, label, on, disabled } => {
            let tag = ctx.actions.len() as isize * WINDOWS + ctx.parity;
            ctx.actions.push(Action::ToolSwitch(*tool));
            let b = button_with(ctx, label, help(label).as_deref(), ButtonStyle::Plain, *disabled, tag);
            b.setButtonType(NSButtonType::PushOnPushOff);
            b.setState(if *on { NSControlStateValueOn } else { NSControlStateValueOff });
            iconize(&b, t, label);
            Some(view_of(&b))
        }
        Tool::Swap { label } => {
            let tag = ctx.actions.len() as isize * WINDOWS + ctx.parity;
            ctx.actions.push(Action::ToolSwap);
            let b = button_with(ctx, label, None, ButtonStyle::Plain, false, tag);
            iconize(&b, t, label);
            // The slot is laid out only when the shell is told "Inverter" is available.
            b.setHidden(!show_swap);
            show_swap.then(|| view_of(&b))
        }
        Tool::Disconnect { label } => {
            let tag = ctx.actions.len() as isize * WINDOWS + ctx.parity;
            ctx.actions.push(Action::Control(swapdesk_ui::ControlId::TbDisconnect));
            let b = button_with(ctx, label, None, ButtonStyle::Danger, false, tag);
            iconize(&b, t, label);
            Some(view_of(&b))
        }
    }
}

fn icon(t: &Tool, label: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(symbol(t)?), Some(&ns(label)))
}

/// Shows the SF Symbol instead of the long title; the title stays as tooltip and accessible name.
fn iconize(b: &objc2_app_kit::NSButton, t: &Tool, label: &str) {
    b.setToolTip(Some(&ns(label)));
    if let Some(img) = icon(t, label) {
        b.setImage(Some(&img));
        b.setImagePosition(NSCellImagePosition::ImageOnly);
    }
}
