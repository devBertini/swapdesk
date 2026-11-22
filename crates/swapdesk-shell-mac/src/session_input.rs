//! Input that comes from a session never operates Swapdesk itself (owner, 22/11/2026).
//!
//! While this Mac is controlled, the controller's mouse and keyboard reach every program on it. Swapdesk's own
//! windows are the exception: with them the controller could change what it is allowed to do here, accept a request
//! in the person's place or end other sessions. The host's input sink posts its events from this very process and
//! marks them (`swapdesk-input-mac`'s `POSTED_MARK`); the application object below drops a mouse or key event that
//! says so before any window, menu bar item or text field of Swapdesk gets it. An event of a device at the Mac says
//! neither, whatever tool it comes through.
//!
//! The rule itself is `swapdesk_paint::banner_guard::mac_posted_by_session`, the one the banner's tap follows.
//!
//! Out of its reach: what a loop of the system takes by itself once it runs (a menu that is already open, a file
//! panel); the click that would open them is an event like any other and is dropped here.

use objc2::rc::Retained;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSEvent, NSEventType};
use objc2_core_graphics::{CGEvent, CGEventField};
use objc2_foundation::NSObjectProtocol;
use swapdesk_paint::banner_guard::mac_posted_by_session;

/// Whether `kind` is mouse or keyboard input.
fn is_input(kind: NSEventType) -> bool {
    matches!(
        kind,
        NSEventType::LeftMouseDown
            | NSEventType::LeftMouseUp
            | NSEventType::RightMouseDown
            | NSEventType::RightMouseUp
            | NSEventType::OtherMouseDown
            | NSEventType::OtherMouseUp
            | NSEventType::MouseMoved
            | NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged
            | NSEventType::ScrollWheel
            | NSEventType::KeyDown
            | NSEventType::KeyUp
            | NSEventType::FlagsChanged
    )
}

/// Whether `event` is the controller's input: mouse or keyboard, posted by the host's sink.
fn from_session(event: &NSEvent) -> bool {
    if !is_input(event.r#type()) {
        return false;
    }
    let Some(cg) = event.CGEvent() else { return false };
    let field = |f: CGEventField| CGEvent::integer_value_field(Some(&cg), f);
    mac_posted_by_session(
        field(CGEventField::EventSourceUnixProcessID),
        field(CGEventField::EventSourceUserData),
        i64::from(std::process::id()),
    )
}

define_class!(
    // SAFETY: the subclass adds no state and no `Drop`; `sendEvent:` either returns or hands the event to the
    // superclass unchanged, which is what NSApplication asks of an override.
    #[unsafe(super = NSApplication)]
    #[thread_kind = MainThreadOnly]
    #[name = "SwapdeskApplication"]
    pub struct App;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for App {}

    impl App {
        // SAFETY: the selector and signature are NSApplication's.
        #[unsafe(method(sendEvent:))]
        fn send_event(&self, event: &NSEvent) {
            if from_session(event) {
                return;
            }
            // SAFETY: the superclass's own dispatch, with the event as it came.
            unsafe { msg_send![super(self), sendEvent: event] }
        }
    }
);

/// The application object, of the class above. The first `sharedApplication` of a process decides the class of its
/// application, so the shell asks here before anything else asks AppKit.
pub fn shared(mtm: MainThreadMarker) -> Retained<NSApplication> {
    // SAFETY: `sharedApplication` is a class method of NSApplication that a subclass inherits; it returns the one
    // application object, made of the receiving class when none exists yet.
    let app: Retained<NSApplication> = unsafe { msg_send![App::class(), sharedApplication] };
    let _ = mtm;
    app
}

/// Whether the application object is of this module's class. It is not when something asked AppKit for the
/// application before [`shared`]: the filter then does not run, and the shell says so in the trace.
pub fn is_ours(app: &NSApplication) -> bool {
    app.isKindOfClass(App::class())
}

/// The window check of this module (it needs the main thread, and runs before any other check makes the
/// application): the application object is this module's, and on real event objects a mouse press is the
/// controller's by the sink's mark or by this process's id, and a device's otherwise.
pub(crate) fn check(mtm: MainThreadMarker) -> Result<(), String> {
    use objc2_core_foundation::CGPoint;
    use objc2_core_graphics::{CGEventType, CGMouseButton};
    use swapdesk_paint::banner_guard::MAC_POSTED_MARK;

    let app = shared(mtm);
    if !is_ours(&app) {
        return Err(
            "the application object is not Swapdesk's own: the controller's input would reach its windows".into()
        );
    }
    let own = i64::from(std::process::id());
    let press = |pid: i64, data: i64| -> Result<Retained<NSEvent>, String> {
        let cg =
            CGEvent::new_mouse_event(None, CGEventType::LeftMouseDown, CGPoint { x: 5.0, y: 5.0 }, CGMouseButton::Left)
                .ok_or("CGEventCreateMouseEvent returned NULL")?;
        CGEvent::set_integer_value_field(Some(&cg), CGEventField::EventSourceUnixProcessID, pid);
        CGEvent::set_integer_value_field(Some(&cg), CGEventField::EventSourceUserData, data);
        NSEvent::eventWithCGEvent(&cg).ok_or_else(|| "no NSEvent for the CGEvent".to_owned())
    };
    let read = |e: &NSEvent| -> Option<(i64, i64)> {
        let cg = e.CGEvent()?;
        Some((
            CGEvent::integer_value_field(Some(&cg), CGEventField::EventSourceUnixProcessID),
            CGEvent::integer_value_field(Some(&cg), CGEventField::EventSourceUserData),
        ))
    };
    let device = press(0, 0)?;
    if from_session(&device) {
        return Err(format!(
            "a device's press was taken for the controller's (pid and mark read {:?}, own {own})",
            read(&device)
        ));
    }
    let marked = press(0, MAC_POSTED_MARK)?;
    if !from_session(&marked) {
        return Err(format!(
            "a press with the sink's mark was not taken for the controller's (read {:?})",
            read(&marked)
        ));
    }
    let posted_here = press(own, 0)?;
    if !from_session(&posted_here) {
        return Err(format!(
            "a press posted by this process was not taken for the controller's (read {:?})",
            read(&posted_here)
        ));
    }
    // The controller's press goes nowhere: handing it to the application must be harmless.
    app.sendEvent(&marked);
    Ok(())
}
