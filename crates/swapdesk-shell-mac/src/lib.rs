//! The macOS shell of Swapdesk (D18, D27): AppKit through objc2, over `swapdesk-ui`. It draws `swapdesk_ui::UiState`
//! and wires OS events to it; it holds no state and no text of its own.
//!
//! Layers:
//! - pure Rust, built and tested on every platform: [`model`] (state to what each window shows), [`input`] (the events
//!   of the video view and the trait the input lane implements), [`locale`];
//! - macOS only (`cfg(target_os = "macos")`): the windows, controls, timers, accessibility and theme following.
//!
//! The slots for the other lanes: [`ViewerInput`] (input lane, pure) and `VideoSlot` (render lane, macOS only), given
//! to [`run_with_host`] as [`ViewerHooks`].
//!
//! Apple documentation read for this crate (docs.rs objc2 0.6 / objc2-app-kit 0.3 and developer.apple.com): see the
//! header of each macOS module.

pub mod input;
pub mod launch;
pub mod locale;
pub mod model;

pub use input::{KeyMsg, Modifiers, MouseKind, MouseMsg, PointerButton, ViewerInput};
pub use locale::{language_from_tags, language_override, theme_override};
pub use model::{BannerModel, MainModel, VideoArea, ViewerModel, banner_model, main_model, viewer_model};

#[cfg(target_os = "macos")]
mod banner_window;
#[cfg(target_os = "macos")]
mod canvas;
#[cfg(target_os = "macos")]
mod cg;
#[cfg(target_os = "macos")]
pub mod painter;
#[cfg(target_os = "macos")]
mod shell;
#[cfg(target_os = "macos")]
mod system;
#[cfg(target_os = "macos")]
mod video;
#[cfg(target_os = "macos")]
mod viewer_window;
// The main window is painted by the canvas: its AppKit builders stay unused until M1.6 removes this module with the
// viewer's and the banner's.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
mod views;

#[cfg(target_os = "macos")]
pub use banner_window::set_snapshot_dir;
#[cfg(target_os = "macos")]
pub use shell::{ViewerHooks, run, run_with_host, selftest, set_shot};
#[cfg(target_os = "macos")]
pub use shell::{close_main_window, main_window_visible, reopen_main_window, selftest_click, status_item_present};
#[cfg(target_os = "macos")]
pub use system::{Waker, system_dark, system_language};
#[cfg(target_os = "macos")]
pub use video::{VideoSlot, VideoSurface, ViewCursor, set_view_cursor};

#[cfg(test)]
mod model_tests;
#[cfg(all(test, target_os = "macos"))]
mod painter_tests;
