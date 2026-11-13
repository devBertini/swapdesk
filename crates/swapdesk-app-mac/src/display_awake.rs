//! A Mac that hosts a session keeps its display on while the session lasts.
//!
//! While the display sleeps, ScreenCaptureKit lists no display, so the capture and the audio of a hosted session cannot
//! start (the host helper also asks again until a display appears, `swapdesk-host`'s `retry`). The cause is fixed here:
//!
//! - when a hosted session starts, and every [`RENEW_EVERY`] while any lasts, the app declares user activity
//!   (`IOPMAssertionDeclareUserActivity`, `kIOPMUserActiveLocal`): "This causes the display to power on and postpone
//!   display sleep up to the user's display sleep Energy Saver settings", and repeating the call with the id it returned
//!   reports continuous activity;
//! - for the life of the session a `PreventUserIdleDisplaySleep` assertion (`IOPMAssertionCreateWithName`) keeps the
//!   display from sleeping by idleness; it is released with `IOPMAssertionRelease` when the last session ends. That
//!   assertion alone does not light a display that is already off, hence the activity declaration.
//!
//! Apple documentation (read 13/11/2026):
//! developer.apple.com/documentation/iokit/1557127-iopmassertiondeclareuseractivity,
//! developer.apple.com/documentation/iokit/1557134-iopmassertioncreatewithname,
//! developer.apple.com/documentation/iokit/1557090-iopmassertionrelease,
//! developer.apple.com/documentation/iokit/kiopmassertiontypepreventuseridledisplaysleep,
//! developer.apple.com/documentation/iokit/iopmlib_h.
//!
//! It does not depend on the "keep awake" setting of Ajustes (that one is about the system's idle sleep and is the
//! hub's `keep_awake` hook): a host whose display is asleep must still deliver the session.
//!
//! The logic is generic over [`Power`], so it is tested with a fake; [`IoKitPower`] is the macOS one.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_host::ui_hub::SystemHooks;
use swapdesk_platform::{Level, TraceSink};

/// How often user activity is declared again while a session lasts (display sleep timers are minutes; this keeps the
/// display from dimming and wakes it again if something put it to sleep).
pub const RENEW_EVERY: Duration = Duration::from_secs(20);

/// The power-management calls the hook makes. Errors carry the `IOReturn`.
pub trait Power: Send + Sync + 'static {
    /// `IOPMAssertionDeclareUserActivity` with `kIOPMUserActiveLocal`; `id` is 0 the first time and the returned id after.
    fn declare_user_activity(&self, id: &mut u32) -> Result<(), i32>;
    /// `IOPMAssertionCreateWithName` of `PreventUserIdleDisplaySleep`, level on.
    fn prevent_display_sleep(&self) -> Result<u32, i32>;
    /// `IOPMAssertionRelease`.
    fn release(&self, id: u32);
}

#[derive(Default)]
struct Pulse {
    stop: bool,
    /// Declare activity now (another session began while one was held).
    poke: bool,
}

struct Hold {
    /// The display assertion; `None` when macOS refused it (activity is still declared).
    display: Option<u32>,
    pulse: Arc<(Mutex<Pulse>, Condvar)>,
    renewer: JoinHandle<()>,
}

/// The hook: holds the display on from the first hosted session to the end of the last.
pub struct DisplayAwake<P: Power> {
    power: Arc<P>,
    period: Duration,
    trace: Arc<dyn TraceSink>,
    hold: Mutex<Option<Hold>>,
}

impl<P: Power> DisplayAwake<P> {
    pub fn new(power: Arc<P>, trace: Arc<dyn TraceSink>) -> Self {
        Self::with_period(power, trace, RENEW_EVERY)
    }

    pub fn with_period(power: Arc<P>, trace: Arc<dyn TraceSink>, period: Duration) -> Self {
        Self { power, period, trace, hold: Mutex::new(None) }
    }

    fn begin(&self) {
        let mut hold = self.hold.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = hold.as_ref() {
            // Another session began: that is user activity too.
            h.pulse.0.lock().unwrap_or_else(PoisonError::into_inner).poke = true;
            h.pulse.1.notify_all();
            return;
        }
        let mut activity = 0u32;
        if let Err(code) = self.power.declare_user_activity(&mut activity) {
            self.trace.log(Level::Warn, "app", format_args!("display awake: user activity was refused ({code:#x})"));
        }
        let display = match self.power.prevent_display_sleep() {
            Ok(id) => Some(id),
            Err(code) => {
                self.trace.log(
                    Level::Warn,
                    "app",
                    format_args!("display awake: the assertion was refused ({code:#x})"),
                );
                None
            }
        };
        let pulse = Arc::new((Mutex::new(Pulse::default()), Condvar::new()));
        let (power, period, thread_pulse) = (Arc::clone(&self.power), self.period, Arc::clone(&pulse));
        let renewer = std::thread::Builder::new().name("display-awake".into()).spawn(move || {
            let (lock, wake) = &*thread_pulse;
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if state.stop {
                    break;
                }
                if state.poke {
                    state.poke = false;
                } else {
                    let (next, timeout) = wake.wait_timeout(state, period).unwrap_or_else(PoisonError::into_inner);
                    state = next;
                    if state.stop {
                        break;
                    }
                    if !timeout.timed_out() && !state.poke {
                        continue;
                    }
                    state.poke = false;
                }
                drop(state);
                // A failure is not worth a line every 20 s: the next call tries again.
                let _ = power.declare_user_activity(&mut activity);
                state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            }
            drop(state);
            if activity != 0 {
                power.release(activity);
            }
        });
        match renewer {
            Ok(renewer) => {
                self.trace.log(Level::Info, "app", format_args!("display awake: held for the hosted session"));
                *hold = Some(Hold { display, pulse, renewer });
            }
            Err(e) => {
                self.trace.log(Level::Warn, "app", format_args!("display awake: no thread to renew with: {e}"));
                if activity != 0 {
                    self.power.release(activity);
                }
                if let Some(id) = display {
                    self.power.release(id);
                }
            }
        }
    }

    fn end(&self) {
        let Some(h) = self.hold.lock().unwrap_or_else(PoisonError::into_inner).take() else { return };
        h.pulse.0.lock().unwrap_or_else(PoisonError::into_inner).stop = true;
        h.pulse.1.notify_all();
        let _ = h.renewer.join();
        if let Some(id) = h.display {
            self.power.release(id);
        }
        self.trace.log(Level::Info, "app", format_args!("display awake: released"));
    }
}

impl<P: Power> SystemHooks for DisplayAwake<P> {
    fn hosted_sessions(&self, live: usize) {
        if live > 0 {
            self.begin();
        } else {
            self.end();
        }
    }
}

impl<P: Power> Drop for DisplayAwake<P> {
    fn drop(&mut self) {
        self.end();
    }
}

/// Two hooks as one: every call goes to both.
pub struct Both(pub Arc<dyn SystemHooks>, pub Arc<dyn SystemHooks>);

impl SystemHooks for Both {
    fn keep_awake(&self, on: bool) {
        self.0.keep_awake(on);
        self.1.keep_awake(on);
    }
    fn lock_screen(&self, console_session: u32) {
        self.0.lock_screen(console_session);
        self.1.lock_screen(console_session);
    }
    fn hosted_sessions(&self, live: usize) {
        self.0.hosted_sessions(live);
        self.1.hosted_sessions(live);
    }
    fn service_enabled(&self, on: bool) {
        self.0.service_enabled(on);
        self.1.service_enabled(on);
    }
}

#[cfg(target_os = "macos")]
pub use iokit::IoKitPower;

#[cfg(target_os = "macos")]
mod iokit {
    use std::ffi::c_void;

    use objc2_foundation::NSString;

    use super::Power;

    /// `kIOPMUserActiveLocal`: the user is in front of the machine (IOPMLib.h).
    const USER_ACTIVE_LOCAL: u32 = 0;
    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: u32 = 255;
    /// `kIOPMAssertionTypePreventUserIdleDisplaySleep`.
    const PREVENT_USER_IDLE_DISPLAY_SLEEP: &str = "PreventUserIdleDisplaySleep";
    const NAME: &str = "Swapdesk hosting a session";

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionDeclareUserActivity(name: *const c_void, user_type: u32, id: *mut u32) -> i32;
        fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// The real IOKit calls. A `NSString` is toll-free bridged to the `CFStringRef` they take.
    pub struct IoKitPower;

    fn cf(s: &NSString) -> *const c_void {
        std::ptr::from_ref(s).cast()
    }

    impl Power for IoKitPower {
        fn declare_user_activity(&self, id: &mut u32) -> Result<(), i32> {
            let name = NSString::from_str(NAME);
            // SAFETY: the name is a live CFString (an NSString) for the call, `id` points to a valid u32.
            let r = unsafe { IOPMAssertionDeclareUserActivity(cf(&name), USER_ACTIVE_LOCAL, id) };
            if r == 0 { Ok(()) } else { Err(r) }
        }

        fn prevent_display_sleep(&self) -> Result<u32, i32> {
            let (kind, name) = (NSString::from_str(PREVENT_USER_IDLE_DISPLAY_SLEEP), NSString::from_str(NAME));
            let mut id = 0u32;
            // SAFETY: both strings are live CFStrings for the call, `id` points to a valid u32.
            let r = unsafe { IOPMAssertionCreateWithName(cf(&kind), LEVEL_ON, cf(&name), &mut id) };
            if r == 0 { Ok(id) } else { Err(r) }
        }

        fn release(&self, id: u32) {
            // SAFETY: the id came from the calls above; a stale id is answered with an error code, nothing else.
            unsafe { IOPMAssertionRelease(id) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    use swapdesk_fakes::LogCapture;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Activity(u32),
        Prevent,
        Release(u32),
    }

    /// A fake power API: records every call; the ids it hands out are 100, 101, ...
    #[derive(Default)]
    struct FakePower {
        calls: Mutex<Vec<Call>>,
        next: AtomicU32,
    }

    impl FakePower {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn count(&self, f: impl Fn(&Call) -> bool) -> usize {
            self.calls().iter().filter(|c| f(c)).count()
        }
    }

    impl Power for FakePower {
        fn declare_user_activity(&self, id: &mut u32) -> Result<(), i32> {
            if *id == 0 {
                *id = 100 + self.next.fetch_add(1, Ordering::SeqCst);
            }
            self.calls.lock().unwrap().push(Call::Activity(*id));
            Ok(())
        }
        fn prevent_display_sleep(&self) -> Result<u32, i32> {
            self.calls.lock().unwrap().push(Call::Prevent);
            Ok(100 + self.next.fetch_add(1, Ordering::SeqCst))
        }
        fn release(&self, id: u32) {
            self.calls.lock().unwrap().push(Call::Release(id));
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < end, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn hook(power: &Arc<FakePower>) -> DisplayAwake<FakePower> {
        DisplayAwake::with_period(Arc::clone(power), Arc::new(LogCapture::new()), Duration::from_millis(20))
    }

    #[test]
    fn taken_at_the_first_session_renewed_while_any_lasts_released_after_the_last() {
        let power = Arc::new(FakePower::default());
        let awake = hook(&power);
        assert!(power.calls().is_empty(), "nothing before a session");

        awake.hosted_sessions(1);
        let calls = power.calls();
        assert!(matches!(calls[0], Call::Activity(_)), "activity first, so the display wakes: {calls:?}");
        assert_eq!(calls[1], Call::Prevent);

        // Renewed on the period, with the id the first declaration returned.
        wait_until("renewals", || power.count(|c| matches!(c, Call::Activity(_))) >= 4);
        let first = calls[0].clone();
        assert!(power.calls().iter().filter(|c| matches!(c, Call::Activity(_))).all(|c| *c == first));

        // A second session: the assertion is not taken twice, and the display is poked awake again.
        let before = power.count(|c| matches!(c, Call::Activity(_)));
        awake.hosted_sessions(2);
        wait_until("a poke", || power.count(|c| matches!(c, Call::Activity(_))) > before);
        assert_eq!(power.count(|c| *c == Call::Prevent), 1);

        // One ends, one is left: still held.
        awake.hosted_sessions(1);
        assert_eq!(power.count(|c| matches!(c, Call::Release(_))), 0);

        // The last ends: both ids are released and the renewal stops.
        awake.hosted_sessions(0);
        let released: Vec<_> = power.calls().into_iter().filter(|c| matches!(c, Call::Release(_))).collect();
        assert_eq!(released.len(), 2, "{:?}", power.calls());
        assert!(released.contains(&Call::Release(100)) && released.contains(&Call::Release(101)));
        let after = power.calls().len();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(power.calls().len(), after, "nothing is renewed after the end");
    }

    #[test]
    fn a_second_round_takes_it_again_and_ending_twice_releases_once() {
        let power = Arc::new(FakePower::default());
        let awake = hook(&power);
        awake.hosted_sessions(1);
        awake.hosted_sessions(0);
        awake.hosted_sessions(0);
        assert_eq!(power.count(|c| matches!(c, Call::Release(_))), 2);
        awake.hosted_sessions(1);
        assert_eq!(power.count(|c| *c == Call::Prevent), 2);
        drop(awake);
        assert_eq!(power.count(|c| matches!(c, Call::Release(_))), 4, "dropping the hook lets go too");
    }

    #[test]
    fn the_composite_forwards_to_both_hooks() {
        let power = Arc::new(FakePower::default());
        let both = Both(Arc::new(hook(&power)), Arc::new(swapdesk_host::ui_hub::NoHooks));
        both.hosted_sessions(1);
        assert_eq!(power.count(|c| *c == Call::Prevent), 1);
        both.hosted_sessions(0);
        assert_eq!(power.count(|c| matches!(c, Call::Release(_))), 2);
    }

    /// The real IOKit calls on this Mac: the assertions are taken and let go (macOS answers success to any process).
    #[cfg(target_os = "macos")]
    #[test]
    fn the_real_iokit_calls_succeed_and_the_assertion_is_released() {
        let power = IoKitPower;
        let mut activity = 0u32;
        power.declare_user_activity(&mut activity).expect("user activity");
        assert_ne!(activity, 0);
        let id = power.prevent_display_sleep().expect("display assertion");
        assert_ne!(id, 0);
        power.release(id);
        power.release(activity);
    }
}
