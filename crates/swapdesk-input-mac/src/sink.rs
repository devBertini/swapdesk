//! `InputSink` (contract C3) on `CGEventPost`: the Mac as the host that a remote viewer types and points on.
//!
//! [`crate::sink_logic::SinkPlanner`] decides what every wire record becomes; this file only builds each planned event
//! as a `CGEvent` from a `CGEventSource` of the `HIDSystemState` and posts it at `kCGHIDEventTap`. It is created and
//! used on the helper's input thread (it holds a CF object, so it is not `Send`).
//!
//! Apple sources (read 2026-11-08):
//! - `CGEventPost`, `CGEventCreateKeyboardEvent`, `CGEventCreateMouseEvent`, `CGEventCreateScrollWheelEvent2`,
//!   `CGEventKeyboardSetUnicodeString` (a string of up to 20 UTF-16 units per event; the virtual key is then ignored by
//!   most applications), `kCGMouseEventClickState`: `CoreGraphics/CGEvent.h`, `CGRemoteOperation.h` and
//!   `CGEventTypes.h` in the macOS SDK; https://developer.apple.com/documentation/coregraphics/cgevent
//! - Posting needs the Accessibility grant of the process (`AXIsProcessTrusted`); without it the events are dropped
//!   silently, so [`MacInputSink::apply`] checks first and reports `PermissionDenied(Accessibility)`. It never prompts.
//! - The double-click interval is `NSEvent.doubleClickInterval` (seconds), read once when the sink is made.
//! - Modifier keys are posted as `kCGEventFlagsChanged` events with the full flags; other events carry the flags of
//!   the modifiers held.
//!
//! UNPROVEN on hardware (the owner's click at the Mac is needed for the Accessibility grant): that the posted
//! modifier events act as real ones in every application, and the wheel signs (see `sink_logic`).

use std::time::Instant;

use objc2_app_kit::NSEvent;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};
use swapdesk_keys::KbKind;
use swapdesk_platform::{DisplayInfo, ErrorKind, InputSink, OsGrant, PlatformError, Result};
use swapdesk_proto::InputEvent;

use crate::platform::detect_keyboard_kind;
use crate::sink_logic::{MouseBtn, MouseKind, Planned, SinkPlanner};

fn os_failure(op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: None, detail: detail.to_owned() }
}

fn denied() -> PlatformError {
    PlatformError {
        kind: ErrorKind::PermissionDenied(OsGrant::Accessibility),
        op: "InputSink.apply",
        os_code: None,
        detail: "Accessibility access is not granted to this process".to_owned(),
    }
}

/// Whether this process may post events (`AXIsProcessTrusted`, read without a prompt).
pub fn can_post() -> bool {
    crate::access().accessibility
}

pub struct MacInputSink {
    planner: SinkPlanner,
    source: Option<CFRetained<CGEventSource>>,
    started: Instant,
    /// Where the last mouse event was posted, for the delta fields.
    last_at: Option<(f64, f64)>,
}

impl MacInputSink {
    /// A sink for the keyboard kind of this Mac and the system's double-click interval.
    pub fn new() -> Self {
        Self::with_keyboard(detect_keyboard_kind())
    }

    pub fn with_keyboard(keyboard: KbKind) -> Self {
        let seconds = NSEvent::doubleClickInterval();
        let ms = if seconds.is_finite() && seconds > 0.0 { (seconds * 1000.0) as u64 } else { 500 };
        Self { planner: SinkPlanner::new(keyboard, ms), source: None, started: Instant::now(), last_at: None }
    }

    /// Keys held down by this sink, as `(usage_page, usage)`.
    pub fn held_keys(&self) -> Vec<(u16, u16)> {
        self.planner.held_keys()
    }

    fn source(&mut self) -> Result<CFRetained<CGEventSource>> {
        if let Some(s) = &self.source {
            return Ok(s.clone());
        }
        let s = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| os_failure("CGEventSourceCreate", "returned NULL"))?;
        self.source = Some(s.clone());
        Ok(s)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn post_all(&mut self, plan: &[Planned]) -> Result<()> {
        if plan.is_empty() {
            return Ok(());
        }
        let source = self.source()?;
        for p in plan {
            self.post(&source, p)?;
        }
        Ok(())
    }

    fn post(&mut self, source: &CGEventSource, planned: &Planned) -> Result<()> {
        match planned {
            Planned::Key { kvk, down, flags, modifier, repeat } => {
                let ev = CGEvent::new_keyboard_event(Some(source), *kvk, *down)
                    .ok_or_else(|| os_failure("CGEventCreateKeyboardEvent", "returned NULL"))?;
                CGEvent::set_flags(Some(&ev), CGEventFlags::from_bits_retain(*flags));
                if *modifier {
                    CGEvent::set_type(Some(&ev), CGEventType::FlagsChanged);
                }
                if *repeat {
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField::KeyboardEventAutorepeat, 1);
                }
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            }
            Planned::Text { units } => {
                for down in [true, false] {
                    let ev = CGEvent::new_keyboard_event(Some(source), 0, down)
                        .ok_or_else(|| os_failure("CGEventCreateKeyboardEvent", "returned NULL"))?;
                    // SAFETY: `units` is a live slice of `units.len()` UTF-16 units (at most two); the call copies it.
                    unsafe { CGEvent::keyboard_set_unicode_string(Some(&ev), units.len() as u64, units.as_ptr()) };
                    CGEvent::set_flags(Some(&ev), CGEventFlags::empty());
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
                }
            }
            Planned::Mouse { kind, button, at, click_state, flags } => {
                let (x, y) = match at {
                    Some(p) => *p,
                    None => {
                        let here = CGEvent::new(Some(source))
                            .map(|e| CGEvent::location(Some(&e)))
                            .ok_or_else(|| os_failure("CGEventCreate", "returned NULL"))?;
                        (here.x, here.y)
                    }
                };
                let (cg_type, cg_button) = mouse_types(*kind, *button);
                let ev = CGEvent::new_mouse_event(Some(source), cg_type, CGPoint { x, y }, cg_button)
                    .ok_or_else(|| os_failure("CGEventCreateMouseEvent", "returned NULL"))?;
                CGEvent::set_flags(Some(&ev), CGEventFlags::from_bits_retain(*flags));
                if *click_state > 0 {
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventClickState, *click_state);
                }
                if let MouseBtn::Other(n) = button {
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventButtonNumber, i64::from(*n));
                }
                if let Some((px, py)) = self.last_at {
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventDeltaX, (x - px) as i64);
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventDeltaY, (y - py) as i64);
                }
                self.last_at = Some((x, y));
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            }
            Planned::Scroll { vertical, horizontal, flags } => {
                let ev = CGEvent::new_scroll_wheel_event2(
                    Some(source),
                    CGScrollEventUnit::Pixel,
                    2,
                    *vertical,
                    *horizontal,
                    0,
                )
                .ok_or_else(|| os_failure("CGEventCreateScrollWheelEvent2", "returned NULL"))?;
                CGEvent::set_flags(Some(&ev), CGEventFlags::from_bits_retain(*flags));
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
            }
        }
        Ok(())
    }
}

impl Default for MacInputSink {
    fn default() -> Self {
        Self::new()
    }
}

/// The CG event type and button of a planned mouse event.
fn mouse_types(kind: MouseKind, button: MouseBtn) -> (CGEventType, CGMouseButton) {
    let cg_button = match button {
        MouseBtn::Left => CGMouseButton::Left,
        MouseBtn::Right => CGMouseButton::Right,
        MouseBtn::Other(n) => CGMouseButton(u32::from(n)),
    };
    let cg_type = match (kind, button) {
        (MouseKind::Moved, _) => CGEventType::MouseMoved,
        (MouseKind::Down, MouseBtn::Left) => CGEventType::LeftMouseDown,
        (MouseKind::Up, MouseBtn::Left) => CGEventType::LeftMouseUp,
        (MouseKind::Dragged, MouseBtn::Left) => CGEventType::LeftMouseDragged,
        (MouseKind::Down, MouseBtn::Right) => CGEventType::RightMouseDown,
        (MouseKind::Up, MouseBtn::Right) => CGEventType::RightMouseUp,
        (MouseKind::Dragged, MouseBtn::Right) => CGEventType::RightMouseDragged,
        (MouseKind::Down, MouseBtn::Other(_)) => CGEventType::OtherMouseDown,
        (MouseKind::Up, MouseBtn::Other(_)) => CGEventType::OtherMouseUp,
        (MouseKind::Dragged, MouseBtn::Other(_)) => CGEventType::OtherMouseDragged,
    };
    (cg_type, cg_button)
}

impl InputSink for MacInputSink {
    fn displays_changed(&mut self, displays: &[DisplayInfo]) {
        self.planner.displays_changed(displays);
    }

    fn apply(&mut self, event: &InputEvent) -> Result<()> {
        // Checked before planning, so a refused record leaves the held set as it was.
        if !can_post() {
            return Err(denied());
        }
        let now = self.now_ms();
        let plan = self.planner.plan(event, now)?;
        self.post_all(&plan)
    }

    fn release_all(&mut self) -> Result<()> {
        let plan = self.planner.release_all();
        if plan.is_empty() {
            return Ok(());
        }
        // The tracking is already cleared: a stuck record would keep releasing forever.
        if !can_post() {
            return Err(denied());
        }
        self.post_all(&plan)
    }

    fn follow_input_desktop(&mut self) -> Result<()> {
        Ok(())
    }
}
