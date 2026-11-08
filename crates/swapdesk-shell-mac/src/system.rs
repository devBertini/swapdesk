//! Small macOS facts: the waker, the system language and appearance, the local clock.
//!
//! Apple documentation: `NSLocale.preferredLanguages`
//! (developer.apple.com/documentation/foundation/nslocale/1415614-preferredlanguages), `NSApplication.effectiveAppearance`
//! and `NSAppearance.bestMatch(from:)` (developer.apple.com/documentation/appkit/nsappearance/1533530-bestmatch),
//! `NSTimeZone.secondsFromGMT` (developer.apple.com/documentation/foundation/nstimezone/1393123-secondsfromgmt),
//! `dispatch2::DispatchQueue::main().exec_async` (docs.rs/dispatch2/0.3.1).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2_app_kit::NSApplication;
use objc2_foundation::{MainThreadMarker, NSArray, NSLocale, NSString, NSTimeZone, ns_string};
use swapdesk_ui::{Language, UiWaker};

use crate::locale::{language_from_tags, language_override, theme_override};

/// Wakes the UI thread from any thread (a block on the main dispatch queue, C6). Coalesces: one wake is queued at a
/// time, so a burst of backend events costs one drain.
pub struct Waker {
    queued: Arc<AtomicBool>,
}

impl Waker {
    pub fn new() -> Waker {
        Waker { queued: Arc::new(AtomicBool::new(false)) }
    }

    /// The UI thread is about to drain: a new wake may be queued again.
    pub(crate) fn drained(&self) {
        self.queued.store(false, Ordering::SeqCst);
    }
}

impl Default for Waker {
    fn default() -> Self {
        Waker::new()
    }
}

impl UiWaker for Waker {
    fn wake(&self) {
        if !self.queued.swap(true, Ordering::SeqCst) {
            DispatchQueue::main().exec_async(crate::shell::wake_on_main);
        }
    }
}

/// pt-BR when the first preferred language is Portuguese, else en; `SWAPDESK_LANG=en|pt` overrides in development.
pub fn system_language() -> Language {
    if let Some(l) = language_override(std::env::var("SWAPDESK_LANG").ok().as_deref()) {
        return l;
    }
    let langs = NSLocale::preferredLanguages();
    let tags: Vec<String> = langs.to_vec().iter().map(|s| s.to_string()).collect();
    language_from_tags(tags.iter().map(String::as_str))
}

/// True in Dark appearance; `SWAPDESK_THEME=dark|light` overrides in development. Needs the main thread.
pub fn system_dark() -> bool {
    if let Some(d) = theme_override(std::env::var("SWAPDESK_THEME").ok().as_deref()) {
        return d;
    }
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let app = NSApplication::sharedApplication(mtm);
    let names: Retained<NSArray<NSString>> =
        NSArray::from_slice(&[ns_string!("NSAppearanceNameAqua"), ns_string!("NSAppearanceNameDarkAqua")]);
    app.effectiveAppearance()
        .bestMatchFromAppearancesWithNames(&names)
        .is_some_and(|n| n.to_string() == "NSAppearanceNameDarkAqua")
}

/// The wall clock and the local UTC offset for `UiState` (dates of the device list, Atividade, chat).
pub fn local_clock() -> swapdesk_ui::format::Clock {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let offset_s = NSTimeZone::localTimeZone().secondsFromGMT();
    swapdesk_ui::format::Clock { now_ms, utc_offset_min: (offset_s / 60) as i32 }
}
