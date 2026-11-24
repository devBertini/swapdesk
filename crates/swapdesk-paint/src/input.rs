//! Pointer and keyboard logic over the last frame's widgets, shared by every shell: which control is under a point,
//! the Tab order of what was drawn and the next control to focus. Coordinates are DIPs, like the widgets'.

use swapdesk_ui::{ControlId, UiState, WindowKind};

use crate::view::Widget;

/// The control under `(x, y)`. Widgets registered later are drawn on top, so the last one that holds the point wins.
pub fn hit_test(widgets: &[Widget], x: f32, y: f32) -> Option<ControlId> {
    widgets.iter().rev().find(|w| w.rect.contains(x, y)).map(|w| w.id)
}

/// The control a finger at `(x, y)` takes, in a window that is touched. A touch inside a control's own box is that
/// control's (the one drawn last when two overlap). Outside every box it goes to the control whose touch target it is
/// in, and of several to the one whose box is nearest: targets grown to 48 DIPs may overlap, boxes decide.
pub fn hit_touch(widgets: &[Widget], x: f32, y: f32) -> Option<ControlId> {
    let reached = |w: &&Widget| w.touch.contains(x, y);
    if let Some(w) = widgets.iter().rev().filter(reached).find(|w| w.rect.contains(x, y)) {
        return Some(w.id);
    }
    let away = |w: &Widget| {
        let dx = (w.rect.x - x).max(x - w.rect.right()).max(0.0);
        let dy = (w.rect.y - y).max(y - w.rect.bottom()).max(0.0);
        dx * dx + dy * dy
    };
    widgets.iter().filter(reached).min_by(|a, b| away(a).total_cmp(&away(b))).map(|w| w.id)
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

/// The control Enter or Space presses: the focused one, while it is still one of the frame's controls (`order`, see
/// [`tab_order`]). The focus can stay on a control that a card now covers, or that went off: a key pressed then
/// must press nothing, never the button behind the question that is on the screen.
pub fn key_target(order: &[ControlId], focus: Option<ControlId>) -> Option<ControlId> {
    focus.filter(|f| !matches!(f, ControlId::Edit(_)) && order.contains(f))
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

#[cfg(test)]
mod key_target_tests {
    use super::*;
    use crate::painter::Rect;
    use swapdesk_ui::mock::Scenario;

    /// Review of 22/11/2026, H4: with the focus on "Controlar" and a request from another computer on the screen,
    /// Enter pressed the button under the card. A key now presses only a control of the frame that is shown.
    #[test]
    fn enter_never_presses_a_control_that_a_card_covers() {
        // The device page: "Controlar" is there, and has the focus.
        let page = Scenario::Dispositivos.prepared_state();
        let control = ControlId::Control;
        let rect = Rect::new(0.0, 0.0, 10.0, 10.0);
        let widget = |id| Widget { id, rect, radius: 0.0, touch: rect };
        let shown = [widget(control)];
        let order = tab_order(&page, WindowKind::Main, &shown);
        assert_eq!(key_target(&order, Some(control)), Some(control));
        // A request arrives: the card's buttons are the frame's only widgets (the page's were cleared).
        let asked = Scenario::PedidoConexao.prepared_state();
        let card: Vec<Widget> = asked
            .controls(WindowKind::Main)
            .into_iter()
            .filter(|c| c.id.focusable() && !c.disabled && !matches!(c.id, ControlId::Control | ControlId::ViewOnly))
            .map(|c| widget(c.id))
            .collect();
        assert!(!card.is_empty(), "the card has its buttons");
        let order = tab_order(&asked, WindowKind::Main, &card);
        assert_eq!(key_target(&order, Some(control)), None, "the button behind the card is not pressed");
        assert_eq!(key_target(&order, Some(card[0].id)), Some(card[0].id), "a button of the card is");
        assert_eq!(key_target(&order, None), None);
    }
}
