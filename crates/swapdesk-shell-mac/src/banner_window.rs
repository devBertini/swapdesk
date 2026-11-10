//! The host banner: a small floating panel at the top of the screen while another computer watches or controls this
//! Mac. Its content is a canvas that paints the shared banner screen (`swapdesk-paint`: who, since when, "Inverter"
//! when offered, "Desconectar"), the same as the Windows banner window.
//!
//! Apple documentation: `NSPanel` and `NSWindowStyleMask.nonactivatingPanel`
//! (developer.apple.com/documentation/appkit/nspanel), `NSWindow.level` with `NSStatusWindowLevel`,
//! `NSWindow.collectionBehavior` (`.canJoinAllSpaces`, `.stationary`, `.fullScreenAuxiliary`: on every Space and over
//! full-screen apps), `NSWindow.hidesOnDeactivate`, `NSScreen.visibleFrame` (below the menu bar),
//! `CALayer.cornerRadius` / `masksToBounds`.

use std::cell::RefCell;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSPanel, NSScreen, NSStatusWindowLevel, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::MainThreadMarker;
use swapdesk_ui::WindowKind;

use crate::appkit::{frame, ns};
use crate::canvas::CanvasView;
use crate::model::BannerModel;

/// Below the menu bar, as the Windows banner sits below the top edge.
const TOP_GAP: f64 = 12.0;
/// The card's corner radius (the frame draws its ring with the same one).
const RADIUS: f64 = 12.0;

pub struct BannerWindow {
    pub panel: Retained<NSPanel>,
    pub canvas: Retained<CanvasView>,
    /// The banner's line, as a screen reader says it (the proof log quotes it).
    label: RefCell<String>,
}

impl BannerWindow {
    pub fn create(mtm: MainThreadMarker) -> BannerWindow {
        let (w, h) = swapdesk_paint::banner_size();
        let bounds = frame(0.0, 0.0, f64::from(w), f64::from(h));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            bounds,
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
        // The frame fills the whole rectangle; the layer rounds it, as the Windows banner's window is rounded.
        let canvas = CanvasView::create(mtm, bounds, WindowKind::Banner);
        canvas.setWantsLayer(true);
        if let Some(layer) = canvas.layer() {
            layer.setCornerRadius(RADIUS);
            layer.setMasksToBounds(true);
        }
        panel.setContentView(Some(&canvas));
        BannerWindow { panel, canvas, label: RefCell::new(String::new()) }
    }

    /// Paints the banner again; when it is new or its line changed, the panel is also put at the top centre of the
    /// main screen, in front. A refresh that changed nothing here must not touch the window server: the banner is
    /// refreshed every second while a countdown runs somewhere else.
    pub fn update(&self, model: &BannerModel) {
        let label = model.label();
        self.canvas.setNeedsDisplay(true);
        if self.panel.isVisible() && *self.label.borrow() == label {
            return;
        }
        let size = self.panel.frame().size;
        let screen = NSScreen::mainScreen(self.panel.mtm()).map(|s| s.visibleFrame());
        let (x, y) = match screen {
            Some(v) => {
                (v.origin.x + (v.size.width - size.width) / 2.0, v.origin.y + v.size.height - size.height - TOP_GAP)
            }
            None => (0.0, 0.0),
        };
        self.panel.setFrame_display(frame(x, y, size.width, size.height), true);
        self.panel.setTitle(&ns(&model.title));
        self.panel.orderFrontRegardless();
        *self.label.borrow_mut() = label;
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
            "BANNER visible={} level={} frame={:.0},{:.0} {:.0}x{:.0} label={:?}\n",
            self.panel.isVisible(),
            self.panel.level(),
            f.origin.x,
            f.origin.y,
            f.size.width,
            f.size.height,
            self.label.borrow(),
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("banner.log"))
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
        // The banner's content, painted into a bitmap: it comes out with the screen locked too.
        self.canvas.save_png(&dir.join("banner-content.png"));
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
