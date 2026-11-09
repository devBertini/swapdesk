//! The screens of Swapdesk, drawn once for every OS (D37). Each shell implements [`Painter`] on its own graphics
//! (Direct2D and DirectWrite on Windows, CoreGraphics and CoreText on macOS); the screens here draw a
//! `swapdesk_ui::UiState` through it and report where every control, text input, video area and terminal grid landed
//! ([`Output`]), so both systems show the same layout, spacing, type and colours. No OS API lives here.

mod painter;
mod screens_files;
mod screens_main;
mod screens_viewer;
mod view;

pub use painter::{Align, Label, Painter, Rect, rrect_path};
pub use screens_viewer::banner_size;
pub use view::{EditSlot, Frame, Interaction, Kind, Output, Widget, button_style, finish, icons};

use swapdesk_ui::WindowKind;

/// Paints the window `kind` of the frame's state and returns what the frame registered (with the focus ring drawn).
pub fn paint(mut frame: Frame<'_>, kind: WindowKind) -> Output {
    match kind {
        WindowKind::Main => screens_main::paint(&mut frame),
        WindowKind::Viewer => screens_viewer::paint_viewer(&mut frame),
        WindowKind::Banner => screens_viewer::paint_banner(&mut frame),
    }
    finish(frame, kind == WindowKind::Viewer)
}

/// The window size a scenario's window has in the design.
pub fn design_size(kind: WindowKind) -> (f32, f32) {
    match kind {
        WindowKind::Banner => banner_size(),
        _ => (swapdesk_ui::tokens::layout::DESIGN_W, swapdesk_ui::tokens::layout::DESIGN_H),
    }
}

#[cfg(test)]
mod tests;
