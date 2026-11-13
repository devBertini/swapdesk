//! What the shell takes from `UiState` besides the painted frame, as plain data: the appearance to force for a theme,
//! the viewer window's title, the host banner's line and the video rectangle the render lane draws into. Pure Rust,
//! no AppKit, so it is tested on every platform. The screens themselves are painted from the shared frame
//! (`swapdesk-paint`, D37) and described to assistive clients from the shared tree (`swapdesk_ui::a11y`).

use swapdesk_ui::{ControlId, UiState, WindowKind, a11y};

/// The `NSAppearanceName` the app is forced to for a theme choice; `None` (Sistema) clears the override so the app
/// follows the OS again.
pub fn appearance_name(theme: swapdesk_ui::Theme) -> Option<&'static str> {
    match theme {
        swapdesk_ui::Theme::System => None,
        swapdesk_ui::Theme::Light => Some("NSAppearanceNameAqua"),
        swapdesk_ui::Theme::Dark => Some("NSAppearanceNameDarkAqua"),
    }
}

// ---------- the viewer window ----------

/// The video rectangle the render lane draws into: points, top-left origin, in the window's content view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoArea {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `backingScaleFactor` of the window's screen.
    pub scale: f64,
}

impl VideoArea {
    /// The area in physical pixels (points times the screen's scale), what the host encodes at (F-VIEW-06).
    pub fn physical(&self) -> (u32, u32) {
        let px = |v: f64| (v * self.scale).round().clamp(0.0, f64::from(u32::MAX)) as u32;
        (px(self.w), px(self.h))
    }
}

/// The viewer window's title while a viewer session exists, else `None` (no viewer window).
pub fn viewer_title(state: &UiState) -> Option<String> {
    state.viewer_session().map(|_| a11y::window_title(state, WindowKind::Viewer))
}

// ---------- the host banner ----------

#[derive(Debug, Clone, PartialEq)]
pub struct BannerModel {
    /// The panel's accessible title (it has no title bar): the name of the banner in the shared accessibility tree.
    pub title: String,
    /// "MacBook do escritório controla este computador", drawn bold.
    pub who: String,
    /// "há 12 min", drawn soft.
    pub since: String,
    /// The line under them while files move or change here ("Copiando arquivos: 2 de 5"), never a file name.
    pub files: Option<String>,
    /// The panel's size in points: one line taller while `files` is there.
    pub size: (f32, f32),
}

impl BannerModel {
    /// The whole line: the panel is put in front again when it changes.
    pub fn label(&self) -> String {
        // The second part is empty while the session reconnects or a swap waits: no stray space then.
        let parts = [Some(self.who.as_str()), Some(self.since.as_str()), self.files.as_deref()];
        parts.into_iter().flatten().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
    }
}

/// The banner while this computer hosts a session, else `None`.
pub fn banner_model(state: &UiState) -> Option<BannerModel> {
    let h = state.host_session()?;
    let peer = state.device_by_id(&h.device_id).map(|d| d.name.clone()).unwrap_or_default();
    let (who, since) = state.banner_parts(&peer, h);
    Some(BannerModel {
        title: a11y::window_title(state, WindowKind::Banner),
        who,
        since,
        files: state.banner_files_line(),
        size: swapdesk_paint::banner_size_for(state),
    })
}

// ---------- the live region ----------

/// The text of the window's polite live region (the wait card's title and body), one line per control; empty while
/// there is no card. A countdown or any control outside the region is not part of it.
pub fn live_region_text(state: &UiState, kind: WindowKind) -> String {
    let lines: Vec<String> = state
        .controls(kind)
        .into_iter()
        .filter(|c| c.region == swapdesk_ui::Region::Status)
        .map(|c| c.label)
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

/// Remembers the live region's text as last synced, so a client is told once per change and never for a repaint.
#[derive(Debug, Default)]
pub struct LiveAnnouncer {
    said: String,
}

impl LiveAnnouncer {
    /// The text to announce now, if the region holds text that differs from the last one seen. An empty region
    /// forgets the last text, so a new card of the same words is announced again.
    pub fn changed(&mut self, text: &str) -> Option<String> {
        if self.said == text {
            return None;
        }
        self.said = text.to_owned();
        (!text.is_empty()).then(|| text.to_owned())
    }
}

/// What the pointer looks like at a point of a canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerShape {
    /// Over a text field.
    Text,
    /// Over any other control: a click acts.
    Hand,
    /// Over nothing that acts.
    Arrow,
    /// Over the video of a viewer: the video view's own pointer (the host's shape while the viewer controls, else the
    /// arrow).
    Video,
}

/// The pointer at a point: `hit` is the control under it, `over_video` whether the point is on a viewer's picture.
/// Every point has a shape, so a pointer that leaves a button for the picture or the chat never keeps the hand
/// (owner, 12/11: it stayed the pointing hand over the screen and the chat).
pub fn pointer_shape(hit: Option<ControlId>, over_video: bool) -> PointerShape {
    match hit {
        Some(ControlId::Edit(_)) => PointerShape::Text,
        Some(_) => PointerShape::Hand,
        None if over_video => PointerShape::Video,
        None => PointerShape::Arrow,
    }
}

#[cfg(test)]
mod view_area_tests {
    use super::VideoArea;

    #[test]
    fn the_area_is_reported_in_physical_pixels() {
        let a = VideoArea { x: 0.0, y: 40.0, w: 960.0, h: 540.0, scale: 2.0 };
        assert_eq!(a.physical(), (1920, 1080));
        let one = VideoArea { scale: 1.0, ..a };
        assert_eq!(one.physical(), (960, 540));
        let fractional = VideoArea { w: 800.5, h: 600.25, scale: 2.0, ..a };
        assert_eq!(fractional.physical(), (1601, 1201));
    }
}
