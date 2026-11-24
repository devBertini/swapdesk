//! The screens of Swapdesk, drawn once for every OS (D37). Each shell implements [`Painter`] on its own graphics
//! (Direct2D and DirectWrite on Windows, CoreGraphics and CoreText on macOS); the screens here draw a
//! `swapdesk_ui::UiState` through it and report where every control, text input, video area and terminal grid landed
//! ([`Output`]), so both systems show the same layout, spacing, type and colours. No OS API lives here.

pub mod banner_guard;
mod card;
mod font;
pub mod grid;
pub mod input;
mod painter;
mod screens_compact;
mod screens_explorer;
mod screens_main;
mod screens_viewer;
pub mod svg;
pub mod term;
mod view;

pub use font::{INSTRUMENT_SANS, face_metrics, label_height, line_box};
pub use painter::{Align, Label, Painter, Rect, rrect_path};
pub use screens_viewer::{banner_size, banner_size_for};
pub use view::{EditSlot, Frame, Interaction, Kind, Output, TOUCH_MIN, Widget, button_style, finish, icons};

use swapdesk_ui::WindowKind;

/// Paints the window `kind` of the frame's state and returns what the frame registered (with the focus ring drawn).
pub fn paint(mut frame: Frame<'_>, kind: WindowKind) -> Output {
    frame.compact = kind == WindowKind::Main && frame.ix.touch && is_compact((frame.w, frame.h));
    match kind {
        WindowKind::Main => screens_main::paint(&mut frame),
        WindowKind::Viewer => screens_viewer::paint_viewer(&mut frame),
        WindowKind::Banner => screens_viewer::paint_banner(&mut frame),
    }
    finish(frame, kind == WindowKind::Viewer)
}

/// Whether a touched main window of this size, in DIPs, takes the compact layout: it is smaller than the smallest
/// window a desktop shell allows (`layout::MIN_W` by `layout::MIN_H`), the size the desktop layout was drawn down to.
/// A phone is, upright and sideways; a tablet is not. A window that is pointed at never takes it, at any size.
pub fn is_compact(size: (f32, f32)) -> bool {
    use swapdesk_ui::tokens::layout;
    size.0 < layout::MIN_W || size.1 < layout::MIN_H
}

/// Where a frame put things, one line each, rounded to 1/100 DIP: the controls, the text inputs, the video and the
/// terminal grid. Two shells that agree on this text agree on the layout.
pub fn layout_dump(out: &Output) -> String {
    use std::fmt::Write;
    let r = |r: &Rect| format!("{:.2} {:.2} {:.2} {:.2}", r.x, r.y, r.w, r.h);
    let mut s = String::new();
    for w in &out.widgets {
        let _ = writeln!(s, "widget {:?} {}", w.id, r(&w.rect));
    }
    for e in &out.edits {
        let _ = writeln!(s, "edit {:?} {}", e.id, r(&e.rect));
    }
    for (name, area) in [("video", out.video), ("terminal", out.terminal), ("list", out.list_area)] {
        if let Some(a) = area {
            let _ = writeln!(s, "{name} {}", r(&a));
        }
    }
    s
}

/// The size of the elevated instance's confirmation window in DIPs: the dialog card and its margin.
pub fn confirm_size() -> (f32, f32) {
    (swapdesk_ui::tokens::layout::DIALOG_W + 48.0, 380.0)
}

/// The window size a scenario's window has in the design.
pub fn design_size(kind: WindowKind) -> (f32, f32) {
    match kind {
        WindowKind::Banner => banner_size(),
        _ => (swapdesk_ui::tokens::layout::DESIGN_W, swapdesk_ui::tokens::layout::DESIGN_H),
    }
}

#[cfg(test)]
mod compact_tests;
#[cfg(test)]
mod tests;
