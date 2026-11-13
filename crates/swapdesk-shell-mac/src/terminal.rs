//! The terminal panel's macOS side (F-XTERM-01): the pasteboard, the key codes and the viewer canvas's mouse and key
//! events. The panel itself (the model of the open shell, its output router, the grid drawn over the shared painter,
//! the selection and what a key means) is `swapdesk_paint::term`, the same on Windows.
//!
//! The Mac's own conventions: Command+C copies the selection and Command+V pastes; Control+C is always the
//! interrupt (Terminal.app does the same); Option makes characters as the keyboard layout says, so it reaches the
//! shell as text, and only with the keys that are not text does it count as Alt.

use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use swapdesk_paint::term::{KeyAction, key_action};
use swapdesk_termview::{Key, Mods};
use swapdesk_ui::{Command, ControlId};

use crate::canvas::CanvasView;

/// The key a macOS virtual key code stands for, as the shared key handling reads it: a letter or the space as a
/// lowercase `Key::Char` (from the characters the key makes without modifiers), the keys that are not text by name,
/// `None` for any other.
pub(crate) fn mac_key(code: u16, plain: Option<char>, shift: bool) -> Option<Key> {
    Some(match code {
        36 | 76 => Key::Enter,
        48 if shift => Key::BackTab,
        48 => Key::Tab,
        49 => Key::Char(' '),
        51 => Key::Backspace,
        53 => Key::Escape,
        114 => Key::Insert,
        117 => Key::Delete,
        115 => Key::Home,
        119 => Key::End,
        116 => Key::PageUp,
        121 => Key::PageDown,
        123 => Key::Left,
        124 => Key::Right,
        125 => Key::Down,
        126 => Key::Up,
        122 => Key::F(1),
        120 => Key::F(2),
        99 => Key::F(3),
        118 => Key::F(4),
        96 => Key::F(5),
        97 => Key::F(6),
        98 => Key::F(7),
        100 => Key::F(8),
        101 => Key::F(9),
        109 => Key::F(10),
        103 => Key::F(11),
        111 => Key::F(12),
        _ => match plain {
            Some(c) if c.is_ascii_alphabetic() => Key::Char(c.to_ascii_lowercase()),
            _ => return None,
        },
    })
}

/// What a key press with Command means to the grid: copy, paste, or nothing (the menus take it).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommandKey {
    Copy,
    Paste,
    Pass,
}

pub(crate) fn command_key(plain: Option<char>) -> CommandKey {
    match plain.map(|c| c.to_ascii_lowercase()) {
        Some('c') => CommandKey::Copy,
        Some('v') => CommandKey::Paste,
        _ => CommandKey::Pass,
    }
}

/// The pasteboard's text, if it holds some.
pub(crate) fn pasteboard_text() -> Option<String> {
    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: `NSPasteboardTypeString` is an immutable string constant of AppKit.
    let kind = unsafe { NSPasteboardTypeString };
    pb.stringForType(kind).map(|s| s.to_string())
}

/// Puts `text` on the pasteboard as plain text.
pub(crate) fn set_pasteboard_text(text: &str) -> bool {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    // SAFETY: as in `pasteboard_text`.
    let kind = unsafe { NSPasteboardTypeString };
    pb.setString_forType(&NSString::from_str(text), kind)
}

fn write(term_id: u32, bytes: Vec<u8>) {
    if !bytes.is_empty() {
        crate::shell::send_from_canvas(vec![Command::TermWrite { term_id, bytes }]);
    }
}

fn copy_selection() -> bool {
    let text = crate::shell::with_terminal(|t| t.selection_text()).flatten();
    match text {
        Some(t) => {
            let ok = set_pasteboard_text(&t);
            crate::shell::with_terminal(|t| t.clear_selection());
            ok
        }
        None => false,
    }
}

fn paste(term_id: u32) {
    let Some(text) = pasteboard_text() else { return };
    if let Some(bytes) = crate::shell::with_terminal(|t| t.paste_bytes(&text)).flatten() {
        write(term_id, bytes);
    }
}

/// A key of the canvas while the grid has the focus and a shell runs; `false` leaves it to the canvas and AppKit.
pub(crate) fn key(canvas: &CanvasView, event: &NSEvent) -> bool {
    if canvas.focus() != Some(ControlId::TermGrid) {
        return false;
    }
    let Some(term_id) = crate::shell::running_terminal() else { return false };
    let flags = event.modifierFlags();
    let plain = event.charactersIgnoringModifiers().and_then(|s| s.to_string().chars().next());
    if flags.contains(NSEventModifierFlags::Command) {
        match command_key(plain) {
            CommandKey::Copy => {
                copy_selection();
            }
            CommandKey::Paste => paste(term_id),
            CommandKey::Pass => return false,
        }
        canvas.setNeedsDisplay(true);
        return true;
    }
    let shift = flags.contains(NSEventModifierFlags::Shift);
    let ctrl = flags.contains(NSEventModifierFlags::Control);
    let option = flags.contains(NSEventModifierFlags::Option);
    let key = mac_key(event.keyCode(), plain, shift);
    // Option is Alt only for the keys that are not text; with a letter it makes the layout's character.
    let alt = option && !matches!(key, Some(Key::Char(_)));
    let mods = Mods { shift, ctrl, alt };
    let Some((rows, alt_screen, app_cursor)) = crate::shell::with_terminal(|t| t.modes_for_keys()).flatten() else {
        return false;
    };
    match key_action(key, mods, rows, alt_screen, app_cursor) {
        KeyAction::Send(bytes) => {
            crate::shell::with_terminal(|t| t.reset_view());
            write(term_id, bytes);
        }
        // Control+C is the interrupt on the Mac, with or without a selection; Command+C copies.
        KeyAction::CopyOrInterrupt | KeyAction::Copy => write(term_id, vec![0x03]),
        KeyAction::Paste => paste(term_id),
        KeyAction::Scroll(n) => {
            crate::shell::with_terminal(|t| t.scroll(n));
        }
        KeyAction::Pass => {
            if ctrl {
                return false;
            }
            let text = event.characters().map(|s| s.to_string()).unwrap_or_default();
            match crate::shell::with_terminal(|t| t.text_bytes(&text)).flatten() {
                Some(bytes) => write(term_id, bytes),
                None => return false,
            }
        }
    }
    canvas.setNeedsDisplay(true);
    true
}

/// The point of `event` in the canvas's coordinates (DIPs, y down).
fn point(canvas: &CanvasView, event: &NSEvent) -> (f32, f32) {
    let p = canvas.convertPoint_fromView(event.locationInWindow(), None);
    (p.x as f32, p.y as f32)
}

/// Mouse down over the grid: a click starts a selection, two select the word, three the line; the grid takes the
/// keyboard while a shell runs. `false` when the press is not over the grid.
pub(crate) fn mouse_down(canvas: &CanvasView, event: &NSEvent) -> bool {
    let (x, y) = point(canvas, event);
    if !crate::shell::with_terminal(|t| t.contains(x, y)).unwrap_or(false) {
        return false;
    }
    let now = (event.timestamp() * 1000.0) as u64;
    let double = (NSEvent::doubleClickInterval() * 1000.0) as u64;
    crate::shell::with_terminal(|t| t.mouse_down(x, y, now, double));
    if crate::shell::running_terminal().is_some() {
        canvas.focus_control(ControlId::TermGrid, false);
    }
    canvas.setNeedsDisplay(true);
    true
}

/// A drag while a selection is made: it follows the pointer. `false` when no selection is being made.
pub(crate) fn mouse_dragged(canvas: &CanvasView, event: &NSEvent) -> bool {
    if !crate::shell::with_terminal(|t| t.selecting()).unwrap_or(false) {
        return false;
    }
    let (x, y) = point(canvas, event);
    crate::shell::with_terminal(|t| t.mouse_move(x, y));
    canvas.setNeedsDisplay(true);
    true
}

/// The button comes up after a selection drag. `false` when no selection was being made.
pub(crate) fn mouse_up(canvas: &CanvasView) -> bool {
    if !crate::shell::with_terminal(|t| t.selecting()).unwrap_or(false) {
        return false;
    }
    crate::shell::with_terminal(|t| t.mouse_up());
    canvas.setNeedsDisplay(true);
    true
}

/// The wheel over the grid scrolls the history: a trackpad by its own distance (one line per line height), a wheel
/// three lines a notch. `false` when the pointer is not over the grid.
pub(crate) fn scroll(canvas: &CanvasView, event: &NSEvent, line_h: f32) -> bool {
    let (x, y) = point(canvas, event);
    if !crate::shell::with_terminal(|t| t.contains(x, y)).unwrap_or(false) {
        return false;
    }
    let dy = event.scrollingDeltaY() as f32;
    let lines = if event.hasPreciseScrollingDeltas() { (dy / line_h).round() } else { dy.signum() * 3.0 };
    if lines != 0.0 {
        crate::shell::with_terminal(|t| t.scroll(lines as isize));
        canvas.setNeedsDisplay(true);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_key_codes_name_the_keys_the_shared_handling_reads() {
        assert_eq!(mac_key(0, Some('A'), true), Some(Key::Char('a')), "a letter, lowercase");
        assert_eq!(mac_key(49, Some(' '), false), Some(Key::Char(' ')), "the space");
        assert_eq!(mac_key(36, None, false), Some(Key::Enter));
        assert_eq!(mac_key(76, None, false), Some(Key::Enter), "the keypad's Enter");
        assert_eq!(mac_key(48, None, true), Some(Key::BackTab));
        assert_eq!(mac_key(51, None, false), Some(Key::Backspace), "the Mac's delete key erases backwards");
        assert_eq!(mac_key(117, None, false), Some(Key::Delete), "forward delete");
        assert_eq!(mac_key(96, None, false), Some(Key::F(5)));
        assert_eq!(mac_key(126, None, false), Some(Key::Up));
        assert_eq!(mac_key(18, Some('1'), false), None, "a digit is text");
        let ctrl = Mods { ctrl: true, ..Mods::NONE };
        assert_eq!(key_action(mac_key(2, Some('d'), false), ctrl, 30, false, false), KeyAction::Send(vec![0x04]));
        assert_eq!(command_key(Some('C')), CommandKey::Copy);
        assert_eq!(command_key(Some('v')), CommandKey::Paste);
        assert_eq!(command_key(Some('q')), CommandKey::Pass, "the menus take Command+Q");
    }
}
