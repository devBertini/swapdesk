//! The Windows shell of Swapdesk (D18): Win32 windows over a DirectComposition tree, Direct2D/DirectWrite drawing of
//! every screen, native `EDIT` inputs, per-monitor DPI v2, the system theme, UI Automation through AccessKit, and no
//! work while idle. It draws `swapdesk_ui::UiState` and wires OS events to it; it holds no text of its own.
//!
//! The viewer window and the host banner: the video area is where the viewer pipeline of `swapdesk-app` attaches its
//! renderer ([`ViewerHost`] carries the window's layout, focus, mouse and keyboard to it); the terminal panel of the
//! drawer draws a `swapdesk-termview` model fed by [`terminal_output`] (stage 43).

mod app;
mod cursor;
mod dialogs;
mod files_dialogs;
mod fullscreen;
mod gfx;
mod offscreen;
mod surface;
mod system;
mod terminal;
mod viewer;
mod window;

pub use app::{native_surface, run, run_with_host, video_area};
pub use offscreen::{Rendered, design_size, render};
pub use swapdesk_platform::NativeSurface;
pub use system::{Waker, logs_dir, message_box, system_dark, system_language, trace_line};
pub use terminal::terminal_output;
pub use swapdesk_paint::{Interaction, Output, Widget};
pub use viewer::{HeadlessSurface, KeyMsg, MouseKind, MouseMsg, PointerButton, ViewerHost};

#[cfg(test)]
mod files_panel_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod tests;
