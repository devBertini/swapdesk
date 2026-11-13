//! D24 on the Mac host (`docs/SPEC.md` F-UNAT-02): the Mac hosts only a logged-in, unlocked session. This module keeps
//! the hub's desktop true to the screen: `Desktop::Locked` while the screen is locked (or cannot be read), `Default`
//! while it is not.
//!
//! * What follows from `Locked` is the shared host's job: a request is refused at once (`NOBODY_PRESENT`, since nobody
//!   present means a locked desktop, `HubHostUi::somebody_present`), a live session is told `HostState { Locked, paused:
//!   true }`, and it goes on with `HostState { Default }` when the screen unlocks. The lock is never opened remotely:
//!   no message and no permission reaches the lock screen (D24).
//! * The reading is `swapdesk_sys_mac::lock` polled by a thread of its own: the call is cheap, never blocks and works
//!   on any thread, and a poll also gives the state at start and survives a missed notification. The decisions are
//!   pure ([`desktop_for`], [`Tracker`]) and tested on every host; only the probe handed to [`watch`] is macOS.

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use swapdesk_host::ui_hub::UiHub;
use swapdesk_platform::{Level, TraceSink};
use swapdesk_proto::Desktop;

/// How often the screen is read. A lock is told to a live session within this, and so is the unlock.
pub const POLL: Duration = Duration::from_millis(500);

/// One reading of the screen, as the watch takes it: whether a session may be hosted, and why not (for the trace). The
/// app's own copy of `swapdesk_sys_mac::lock::ScreenState`, so this module builds and is tested without the macOS crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub can_host: bool,
    pub why: &'static str,
}

impl Reading {
    pub const UNLOCKED: Self = Self { can_host: true, why: "unlocked" };
    pub const LOCKED: Self = Self { can_host: false, why: "locked" };
    pub const OFF_CONSOLE: Self = Self { can_host: false, why: "off the console" };
    pub const UNREADABLE: Self = Self { can_host: false, why: "unreadable" };
}

/// The screen as macOS says it now (`swapdesk_sys_mac::lock`), every state but an unlocked one unhostable.
#[cfg(target_os = "macos")]
pub fn read_screen() -> Reading {
    use swapdesk_sys_mac::lock::{ScreenState, read_screen_state};
    match read_screen_state() {
        ScreenState::Unlocked => Reading::UNLOCKED,
        ScreenState::Locked => Reading::LOCKED,
        ScreenState::OffConsole => Reading::OFF_CONSOLE,
        ScreenState::Unreadable => Reading::UNREADABLE,
    }
}

/// The desktop a reading stands for. Only an unlocked, logged-in session on the console is `Default`; every other
/// state, an unreadable one included, is `Locked`, so a doubtful reading never lets a session through (fail closed).
pub fn desktop_for(state: Reading) -> Desktop {
    if state.can_host { Desktop::Default } else { Desktop::Locked }
}

/// Remembers the last reading and says when the desktop it stands for changed. The first reading always counts, so the
/// hub learns the state at start.
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<Desktop>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The desktop to tell the hub, when this reading changes it. Two readings that differ (`OffConsole` then
    /// `Unreadable`) but stand for one desktop change nothing.
    pub fn observe(&mut self, state: Reading) -> Option<Desktop> {
        let desktop = desktop_for(state);
        (self.last.replace(desktop) != Some(desktop)).then_some(desktop)
    }
}

/// The running watch: dropping it stops the thread.
pub struct ScreenWatch {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ScreenWatch {
    fn drop(&mut self) {
        // Dropping the sender wakes the thread at once.
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts reading the screen with `probe` every `interval` and tells `hub` its desktop whenever it changes. The first
/// reading is taken on this thread before it returns, so a request that arrives right after start already meets the
/// right desktop.
pub fn watch(
    hub: UiHub,
    probe: impl Fn() -> Reading + Send + 'static,
    interval: Duration,
    trace: Arc<dyn TraceSink>,
) -> std::io::Result<ScreenWatch> {
    let mut tracker = Tracker::new();
    let mut tell = move |state: Reading| {
        if let Some(desktop) = tracker.observe(state) {
            trace.log(Level::Info, "app", format_args!("the screen is {}: the desktop is {desktop:?}", state.why));
            hub.set_desktop(desktop);
        }
    };
    tell(probe());
    let (stop, stopped) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new().name("swapdesk-screen-lock".into()).spawn(move || {
        // A timeout is the next poll; the sender dropped (or a message) is the end.
        while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
            tell(probe());
        }
    })?;
    Ok(ScreenWatch { stop: Some(stop), thread: Some(thread) })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use swapdesk_proto::Desktop;

    use super::*;
    use crate::service::test_support::Rig;

    #[test]
    fn only_the_unlocked_session_is_the_default_desktop() {
        assert_eq!(desktop_for(Reading::UNLOCKED), Desktop::Default);
        for state in [Reading::LOCKED, Reading::OFF_CONSOLE, Reading::UNREADABLE] {
            assert_eq!(desktop_for(state), Desktop::Locked, "{state:?}");
        }
    }

    #[test]
    fn the_tracker_reports_the_first_reading_and_then_only_changes() {
        let mut t = Tracker::new();
        assert_eq!(t.observe(Reading::UNLOCKED), Some(Desktop::Default), "the start counts");
        assert_eq!(t.observe(Reading::UNLOCKED), None);
        assert_eq!(t.observe(Reading::LOCKED), Some(Desktop::Locked));
        assert_eq!(t.observe(Reading::UNREADABLE), None, "locked either way");
        assert_eq!(t.observe(Reading::OFF_CONSOLE), None);
        assert_eq!(t.observe(Reading::UNLOCKED), Some(Desktop::Default));
        assert_eq!(t.observe(Reading::LOCKED), Some(Desktop::Locked));
    }

    #[test]
    fn a_session_that_starts_locked_is_told_so_before_the_first_poll() {
        let mut t = Tracker::new();
        assert_eq!(t.observe(Reading::LOCKED), Some(Desktop::Locked));
    }

    /// A probe the test moves by hand.
    struct Dial(Mutex<Reading>);

    impl Dial {
        fn set(&self, s: Reading) {
            *self.0.lock().unwrap() = s;
        }
    }

    fn until(what: &str, f: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < end, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_hub_follows_the_screen_lock_and_unlock() {
        let rig = Rig::start("screen-lock", None);
        let hub = rig.svc.hub().clone();
        let host_ui = hub.host_ui();
        let dial = Arc::new(Dial(Mutex::new(Reading::UNLOCKED)));
        let log = Arc::new(swapdesk_fakes::LogCapture::new());
        let probe = Arc::clone(&dial);
        let _watch = watch(hub, move || *probe.0.lock().unwrap(), Duration::from_millis(5), log).expect("starts");

        // Unlocked: the host is the default desktop, and somebody is present to be asked.
        assert_eq!(host_ui.current_desktop(), Some(Desktop::Default));
        assert!(host_ui.somebody_present());

        // Locked: the sessions are on the lock screen and nobody is present, so a prompt is refused at once.
        dial.set(Reading::LOCKED);
        until("the lock reaches the hub", || host_ui.current_desktop() == Some(Desktop::Locked));
        assert!(!host_ui.somebody_present(), "a locked Mac has nobody present");
        assert_eq!(host_ui.start_info().desktop, Desktop::Locked, "a new session is told it is locked");

        // A reading that fails stays locked.
        dial.set(Reading::UNREADABLE);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(host_ui.current_desktop(), Some(Desktop::Locked));

        // Unlocked again: the sessions go on.
        dial.set(Reading::UNLOCKED);
        until("the unlock reaches the hub", || host_ui.current_desktop() == Some(Desktop::Default));
        assert!(host_ui.somebody_present());
    }

    #[test]
    fn a_screen_that_is_locked_at_start_is_known_when_the_watch_returns() {
        let rig = Rig::start("screen-lock-start", None);
        let hub = rig.svc.hub().clone();
        let host_ui = hub.host_ui();
        let log = Arc::new(swapdesk_fakes::LogCapture::new());
        let _watch = watch(hub, || Reading::LOCKED, Duration::from_secs(3600), log).expect("starts");
        assert_eq!(host_ui.current_desktop(), Some(Desktop::Locked), "no wait for a poll");
        assert!(!host_ui.somebody_present());
    }

    #[test]
    fn dropping_the_watch_stops_its_thread_at_once() {
        let rig = Rig::start("screen-lock-drop", None);
        let log = Arc::new(swapdesk_fakes::LogCapture::new());
        let started = Instant::now();
        let watch = watch(rig.svc.hub().clone(), || Reading::UNLOCKED, Duration::from_secs(3600), log).expect("starts");
        drop(watch);
        assert!(started.elapsed() < Duration::from_secs(5), "it did not wait for the next poll");
    }
}
