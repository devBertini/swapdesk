//! The host banner: a small floating panel at the top of the screen while another computer watches or controls this
//! Mac, with who, since when, "Inverter" when offered and "Desconectar". It is the Mac's side of the Windows banner
//! window (`swapdesk-shell-win`, `screens_viewer::paint_banner`): a surface card with a 2 pt link ring, a link dot,
//! the bold and soft parts of the line, then the buttons.
//!
//! Apple documentation: `NSPanel` and `NSWindowStyleMask.nonactivatingPanel`
//! (developer.apple.com/documentation/appkit/nspanel), `NSWindow.level` with `NSStatusWindowLevel`,
//! `NSWindow.collectionBehavior` (`.canJoinAllSpaces`, `.stationary`, `.fullScreenAuxiliary`: on every Space and over
//! full-screen apps), `NSWindow.hidesOnDeactivate`, `NSScreen.visibleFrame` (below the menu bar).

use std::cell::RefCell;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAccessibility, NSBackingStoreType, NSBox, NSBoxType, NSColor, NSPanel, NSScreen, NSStatusWindowLevel,
    NSTextField, NSTitlePosition, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint};
use swapdesk_ui::ControlId;
use swapdesk_ui::tokens::{Palette, palette, text as tx};

use crate::model::{BannerModel, ButtonStyle};
use crate::views::*;

const HEIGHT: f64 = 52.0;
/// Below the menu bar, as the Windows banner sits below the top edge.
const TOP_GAP: f64 = 12.0;

pub struct BannerWindow {
    pub panel: Retained<NSPanel>,
    pub actions: RefCell<Vec<Action>>,
    last: RefCell<Option<BannerModel>>,
}

impl BannerWindow {
    pub fn create(mtm: MainThreadMarker) -> BannerWindow {
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame(0.0, 0.0, 600.0, HEIGHT),
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the panel is kept in `Shell`, so AppKit must not release it on close, as it requires for windows
        // created outside a window controller.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setHidesOnDeactivate(false);
        panel.setLevel(NSStatusWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        BannerWindow { panel, actions: RefCell::new(Vec::new()), last: RefCell::new(None) }
    }

    /// Rebuilds the card when the model changed and keeps the panel at the top centre of the main screen.
    pub fn update(&self, mtm: MainThreadMarker, target: &AnyObject, model: &BannerModel) {
        if self.last.borrow().as_ref() == Some(model) {
            return;
        }
        let pal: &'static Palette = palette(model.dark);
        let mut actions = self.actions.borrow_mut();
        actions.clear();
        let mut fields = Vec::new();
        let mut ctx = Ctx { mtm, target, pal, actions: &mut actions, parity: PARITY_BANNER, fields: &mut fields };

        let dot = PaintedView::create(mtm, frame(0.0, 0.0, 10.0, 10.0), Some(pal.link));
        dot.setWantsLayer(true);
        if let Some(layer) = dot.layer() {
            layer.setCornerRadius(5.0);
        }
        dot.widthAnchor().constraintEqualToConstant(10.0).setActive(true);
        dot.heightAnchor().constraintEqualToConstant(10.0).setActive(true);
        let who = NSTextField::labelWithString(&ns(&model.who), mtm);
        who.setFont(Some(&font(tx::NAME)));
        who.setTextColor(Some(&color(pal.ink)));
        let since = NSTextField::labelWithString(&ns(&model.since), mtm);
        since.setFont(Some(&font(tx::BODY)));
        since.setTextColor(Some(&color(pal.soft)));
        let line = hstack(&ctx, &[view_of(&who), view_of(&since)], 4.0);

        let mut items: Vec<Retained<NSView>> = vec![view_of(&dot), view_of(&line)];
        let mut buttons: Vec<Retained<NSView>> = Vec::new();
        if let Some((label, disabled)) = &model.swap {
            let tag = ctx.tag(Action::Control(ControlId::BannerSwap));
            let b = button_with(&mut ctx, label, None, ButtonStyle::Plain, *disabled, tag);
            if let Some(img) = objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &ns("arrow.left.arrow.right"),
                Some(&ns(label)),
            ) {
                b.setImage(Some(&img));
                b.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageLeading);
            }
            buttons.push(view_of(&b));
        }
        let tag = ctx.tag(Action::Control(ControlId::BannerDisconnect));
        let disconnect = button_with(&mut ctx, &model.disconnect, None, ButtonStyle::Danger, false, tag);
        // Solid red with a white label, as the Windows banner's "Desconectar". Drawn by its own layer: AppKit greys a
        // bezel colour in a window that is not key, and this panel never is.
        disconnect.setBordered(false);
        disconnect.setContentTintColor(Some(&NSColor::whiteColor()));
        disconnect.setKeyEquivalent(&ns(""));
        let label = disconnect.fittingSize();
        let (bw, bh) = ((label.width + 28.0).ceil(), 36.0);
        let solid = PaintedView::create(mtm, frame(0.0, 0.0, bw, bh), Some(pal.danger));
        solid.setWantsLayer(true);
        if let Some(layer) = solid.layer() {
            layer.setCornerRadius(8.0);
        }
        disconnect.setFrame(frame(0.0, 0.0, bw, bh));
        solid.addSubview(&disconnect);
        solid.widthAnchor().constraintEqualToConstant(bw).setActive(true);
        solid.heightAnchor().constraintEqualToConstant(bh).setActive(true);
        buttons.push(view_of(&solid));
        let gap = view_of(&NSView::new(mtm));
        gap.widthAnchor().constraintGreaterThanOrEqualToConstant(24.0).setActive(true);
        items.push(gap);
        items.extend(buttons);
        let row = hstack(&ctx, &items, 10.0);
        let inner = fit(&row);

        let card = NSBox::new(mtm);
        card.setBoxType(NSBoxType::Custom);
        card.setTitlePosition(NSTitlePosition::NoTitle);
        card.setCornerRadius(12.0);
        card.setBorderWidth(2.0);
        card.setBorderColor(&color(pal.link));
        card.setFillColor(&color(pal.surface));
        card.setContentViewMargins(size(0.0, 0.0));
        let w = (inner.width + 36.0).ceil();
        let content = NSView::new(mtm);
        content.setFrame(frame(0.0, 0.0, w, HEIGHT));
        row.setFrameOrigin(NSPoint::new(18.0, (HEIGHT - inner.height) / 2.0));
        content.addSubview(&row);
        card.setContentView(Some(&content));
        card.setFrame(frame(0.0, 0.0, w, HEIGHT));
        card.setAccessibilityLabel(Some(&ns(&model.label())));
        card.setAccessibilityRole(Some(&ns("AXGroup")));

        let screen = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame());
        let (x, y) = match screen {
            Some(v) => (v.origin.x + (v.size.width - w) / 2.0, v.origin.y + v.size.height - HEIGHT - TOP_GAP),
            None => (0.0, 0.0),
        };
        self.panel.setContentView(Some(&view_of(&card)));
        self.panel.setFrame_display(frame(x, y, w, HEIGHT), true);
        self.panel.setTitle(&ns(&model.label()));
        self.panel.orderFrontRegardless();
        *self.last.borrow_mut() = Some(model.clone());
    }

    pub fn close(&self) {
        self.panel.orderOut(None);
    }

    /// Development proof (`--drive-host`): writes where the panel is and how it floats to `<dir>/banner.log`, then,
    /// a second later, has `screencapture` take the whole screen and the panel alone (`banner-screen.png`,
    /// `banner-window.png`). The app already holds the screen recording grant a host needs; nothing is asked.
    pub fn snapshot(&self, dir: &std::path::Path) {
        let f = self.panel.frame();
        let line = format!(
            "BANNER visible={} level={} frame={:.0},{:.0} {:.0}x{:.0} label={:?}
",
            self.panel.isVisible(),
            self.panel.level(),
            f.origin.x,
            f.origin.y,
            f.size.width,
            f.size.height,
            self.last.borrow().as_ref().map(BannerModel::label).unwrap_or_default(),
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("banner.log"))
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
        let (dir, window) = (dir.to_path_buf(), self.panel.windowNumber());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let shot = |args: &[String]| std::process::Command::new("/usr/sbin/screencapture").args(args).status();
            let screen = dir.join("banner-screen.png").to_string_lossy().into_owned();
            let alone = dir.join("banner-window.png").to_string_lossy().into_owned();
            let _ = shot(&["-x".into(), screen]);
            let _ = shot(&["-x".into(), format!("-l{window}"), alone]);
        });
    }
}

static SNAPSHOT_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Development proof: the folder where the first banner of the run is recorded (see [`BannerWindow::snapshot`]).
pub fn set_snapshot_dir(dir: std::path::PathBuf) {
    let _ = SNAPSHOT_DIR.set(dir);
}

pub fn snapshot_dir() -> Option<&'static std::path::Path> {
    SNAPSHOT_DIR.get().map(std::path::PathBuf::as_path)
}
