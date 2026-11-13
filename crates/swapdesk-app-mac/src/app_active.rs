//! The app coming back to the front. macOS grants (Screen Recording, Accessibility, Input Monitoring) and the login
//! item's approval are given in System Settings, so the person switches away, flips the switch and returns: that is
//! when the main window's notices about them are read again (`UiHub::mac_status_changed`).
//!
//! Apple documentation: developer.apple.com/documentation/appkit/nsapplication/didbecomeactivenotification.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSApplicationDidBecomeActiveNotification;
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};

/// Keeps the observer registered; dropping it leaves it in the center for the life of the process.
pub struct ActiveObserver {
    _token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

/// Calls `on_active` each time the app becomes the active application. Call it on the main thread (AppKit posts there).
pub fn observe(on_active: impl Fn() + 'static) -> ActiveObserver {
    let center = NSNotificationCenter::defaultCenter();
    let block = RcBlock::new(move |_note: NonNull<NSNotification>| on_active());
    // SAFETY: the name is AppKit's constant; the center copies the block; no object filter, no queue (the posting
    // thread, the main thread for AppKit).
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSApplicationDidBecomeActiveNotification),
            None,
            None,
            &block,
        )
    };
    ActiveObserver { _token: token }
}
