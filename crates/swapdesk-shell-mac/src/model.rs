//! What the shell takes from `UiState` besides the painted frame, as plain data: the appearance to force for a theme,
//! the viewer window's title, the host banner's line and the video rectangle the render lane draws into. Pure Rust,
//! no AppKit, so it is tested on every platform. The screens themselves are painted from the shared frame
//! (`swapdesk-paint`, D37) and described to assistive clients from the shared tree (`swapdesk_ui::a11y`).

use swapdesk_ui::{UiState, WindowKind, a11y};

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

/// The viewer window's title while a viewer session exists, else `None` (no viewer window).
pub fn viewer_title(state: &UiState) -> Option<String> {
    state.viewer_session().map(|_| a11y::window_title(state, WindowKind::Viewer))
}

// ---------- the host banner ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerModel {
    /// The panel's accessible title (it has no title bar): the name of the banner in the shared accessibility tree.
    pub title: String,
    /// "MacBook do escritório controla este computador", drawn bold.
    pub who: String,
    /// "há 12 min", drawn soft.
    pub since: String,
}

impl BannerModel {
    /// The whole line: the panel is put in front again when it changes.
    pub fn label(&self) -> String {
        format!("{} {}", self.who, self.since)
    }
}

/// The banner while this computer hosts a session, else `None`.
pub fn banner_model(state: &UiState) -> Option<BannerModel> {
    let h = state.host_session()?;
    let peer = state.device_by_id(&h.device_id).map(|d| d.name.clone()).unwrap_or_default();
    let (who, since) = state.banner_parts(&peer, h);
    Some(BannerModel { title: a11y::window_title(state, WindowKind::Banner), who, since })
}
