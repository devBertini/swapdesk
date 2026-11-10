//! Pointer and keyboard logic over the last frame's widgets, shared by every shell: which control is under a point,
//! the Tab order of what was drawn and the next control to focus. Coordinates are DIPs, like the widgets'.

use swapdesk_ui::{ControlId, UiState, WindowKind};

use crate::view::Widget;

/// The control under `(x, y)`. Widgets registered later are drawn on top, so the last one that holds the point wins.
pub fn hit_test(widgets: &[Widget], x: f32, y: f32) -> Option<ControlId> {
    widgets.iter().rev().find(|w| w.rect.contains(x, y)).map(|w| w.id)
}

/// The controls of window `kind` that can take the focus now, in Tab order: focusable, enabled and drawn in the last
/// frame (`widgets`).
pub fn tab_order(state: &UiState, kind: WindowKind, widgets: &[Widget]) -> Vec<ControlId> {
    state
        .controls(kind)
        .into_iter()
        .filter(|c| c.id.focusable() && !c.disabled && widgets.iter().any(|w| w.id == c.id))
        .map(|c| c.id)
        .collect()
}

/// The control Tab (or Shift+Tab when `backwards`) moves to from `current`, wrapping at both ends. With no focus, or a
/// focus that is no longer in `order`, it starts at the first control (the last one backwards). `None` when `order`
/// is empty.
pub fn next_focus(order: &[ControlId], current: Option<ControlId>, backwards: bool) -> Option<ControlId> {
    let n = order.len();
    if n == 0 {
        return None;
    }
    let at = current.and_then(|c| order.iter().position(|&i| i == c));
    let next = match (at, backwards) {
        (None, false) => 0,
        (None, true) => n - 1,
        (Some(i), false) => (i + 1) % n,
        (Some(i), true) => (i + n - 1) % n,
    };
    Some(order[next])
}
