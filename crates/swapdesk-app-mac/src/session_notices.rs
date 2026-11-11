//! ACCOUNTS.md 12.2: hosting only while this app's user session is the active one. `NSWorkspace` posts
//! `sessionDidResignActiveNotification` when the user session switches out (fast user switching) and
//! `sessionDidBecomeActiveNotification` when it switches back in; an app launched in an inactive session gets the
//! resign notice right after it finishes launching.
//!
//! Apple documentation: developer.apple.com/documentation/appkit/nsworkspace/sessiondidresignactivenotification and
//! .../sessiondidbecomeactivenotification (read 2026-11-10, ACCOUNTS.md 16).

use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceSessionDidBecomeActiveNotification, NSWorkspaceSessionDidResignActiveNotification,
};
use objc2_foundation::{NSNotification, NSObjectProtocol};

/// Keeps the two observers registered; dropping it leaves them in the center for the life of the process, which is
/// what the app wants (it keeps it for its whole run).
pub struct SessionObserver {
    _tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

/// Calls `on_change(true)` when this user session becomes active and `on_change(false)` when it resigns. Call it on
/// the main thread (the workspace's notifications are posted there).
pub fn observe(on_change: impl Fn(bool) + 'static) -> SessionObserver {
    let on_change: Rc<dyn Fn(bool)> = Rc::new(on_change);
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let mut tokens = Vec::new();
    // SAFETY: the two names are AppKit's constants, read once here.
    let names = unsafe {
        [(NSWorkspaceSessionDidResignActiveNotification, false), (NSWorkspaceSessionDidBecomeActiveNotification, true)]
    };
    for (name, active) in names {
        let f = Rc::clone(&on_change);
        let block = RcBlock::new(move |_note: NonNull<NSNotification>| f(active));
        // SAFETY: the block lives as long as the center keeps it (it copies it); no object filter, no queue (the
        // posting thread, the main thread for the workspace).
        let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
        tokens.push(token);
    }
    SessionObserver { _tokens: tokens }
}
