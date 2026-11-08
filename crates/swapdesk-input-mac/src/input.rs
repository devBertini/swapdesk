//! [`MacInput`]: the viewer's input on the Mac. It implements the shell's `ViewerInput` (key, modifier, mouse and focus
//! events of the video view) over the state machine of [`crate::logic`], and sends the wire records on the channel the
//! session reads (`SyncSender<InputEvent>`, the same one `InputGrabber::start` takes).
//!
//! One `MacInput` is shared by clones: the shell holds one as `Arc<dyn ViewerInput>`, the grabber (the event tap's
//! thread) another. All state sits behind one mutex held for table lookups and a `try_send` only: the shell's main
//! thread must never wait (the `ViewerInput` contract) and the tap's callback must answer fast or macOS disables the tap.

use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use swapdesk_proto::InputEvent;
use swapdesk_shell_mac::{KeyMsg, Modifiers, MouseMsg, ViewerInput};

use crate::logic::{InputConfig, InputCore, KeyProbe, Outcome};

/// Where a point of the video view falls in the remote display, in the pixels of the window (`Renderer::map_pointer`):
/// `None` outside the picture. Called with the input lock held, so it must not call back into [`MacInput`].
pub type PointerMap = Arc<dyn Fn(i32, i32) -> Option<(u16, u16)> + Send + Sync>;

/// The OS state the keyboard logic reads: which physical key is down (to tell left from right modifier keys) and the
/// flags now. Faked in tests.
pub trait KeyboardState: Send + Sync {
    fn key_down(&self, kvk: u16) -> bool;
    fn flags(&self) -> Modifiers;
}

/// No keyboard state: every modifier stands for its left key. The state of tests and of non-macOS builds.
pub struct NoKeyboardState;

impl KeyboardState for NoKeyboardState {
    fn key_down(&self, _kvk: u16) -> bool {
        false
    }
    fn flags(&self) -> Modifiers {
        Modifiers::default()
    }
}

/// The TCC grants the system-key grab needs, as `CGPreflightListenEventAccess` and `AXIsProcessTrusted` report them.
/// Reading them never prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    /// "Monitoramento de Entrada" (Input Monitoring): `CGPreflightListenEventAccess`.
    pub input_monitoring: bool,
    /// "Acessibilidade" (Accessibility): `AXIsProcessTrusted`. An event tap that can swallow keys needs it (Apple:
    /// "Event taps receive key up and key down events if ... Access for assistive devices is enabled").
    pub accessibility: bool,
}

impl Access {
    pub fn complete(&self) -> bool {
        self.input_monitoring && self.accessibility
    }
}

/// What the UI shows about "capturar atalhos do sistema".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrabStatus {
    /// Not asked for, or the grabber is not running.
    #[default]
    Off,
    /// The event tap runs: Cmd+Tab and the other system shortcuts go to the host.
    Active,
    /// Asked for, but a grant is missing (the fields are what is granted now). Nothing was prompted and nothing retries:
    /// the UI offers [`crate::MacGrabber::request_access`] and the System Settings pane, then the user turns it on again.
    NeedsPermission(Access),
    /// Asked for, granted, but the tap could not be made or died.
    Failed,
}

struct Port {
    tx: SyncSender<InputEvent>,
    /// An event could not be queued (the consumer is behind): a `ReleaseAll` goes first as soon as there is room, so
    /// the host never keeps a key whose release was lost.
    lost: bool,
}

pub(crate) struct State {
    core: InputCore,
    port: Option<Port>,
    focused: bool,
    map: Option<PointerMap>,
    scale: f64,
    grab: GrabStatus,
    dropped: u64,
}

pub(crate) struct Inner {
    state: Mutex<State>,
    keyboard: Box<dyn KeyboardState>,
}

enum Sent {
    Yes,
    Full,
}

impl State {
    fn deliver(&mut self, events: Vec<InputEvent>) {
        for event in events {
            let Some(port) = self.port.as_mut() else {
                return;
            };
            if port.lost {
                if port.tx.try_send(InputEvent::ReleaseAll).is_err() {
                    self.dropped += 1;
                    continue;
                }
                port.lost = false;
            }
            let pointer = matches!(event, InputEvent::Pointer { .. });
            let wheel = matches!(event, InputEvent::Wheel { .. });
            let sent = match port.tx.try_send(event) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => Sent::Yes,
                Err(TrySendError::Full(_)) => Sent::Full,
            };
            if let Sent::Full = sent {
                self.dropped += 1;
                if pointer {
                    // A position is idempotent: the next move or click sends its own.
                    self.core.forget_pointer();
                } else if !wheel {
                    // A key or button record was lost: let the host release everything and forget what we held, so
                    // both sides agree again.
                    port.lost = true;
                    self.core.release_all();
                }
            }
        }
    }

    fn apply(&mut self, outcome: Outcome) -> bool {
        let taken = outcome.taken;
        self.deliver(outcome.events);
        taken
    }
}

/// The viewer's input; cheap to clone, every clone is the same input.
#[derive(Clone)]
pub struct MacInput {
    pub(crate) inner: Arc<Inner>,
}

impl MacInput {
    /// With the OS keyboard state on macOS, without it elsewhere (modifiers then stand for their left key).
    pub fn new(config: InputConfig) -> Self {
        #[cfg(target_os = "macos")]
        let keyboard: Box<dyn KeyboardState> = Box::new(crate::platform::SystemKeyboardState);
        #[cfg(not(target_os = "macos"))]
        let keyboard: Box<dyn KeyboardState> = Box::new(NoKeyboardState);
        Self::with_keyboard_state(config, keyboard)
    }

    pub fn with_keyboard_state(config: InputConfig, keyboard: Box<dyn KeyboardState>) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    core: InputCore::new(config),
                    port: None,
                    focused: false,
                    map: None,
                    scale: 1.0,
                    grab: GrabStatus::Off,
                    dropped: 0,
                }),
                keyboard,
            }),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The pointer mapping: the renderer's `map_pointer`, shared with this thread (the renderer lives on the video
    /// thread, so the app passes a closure over whatever it publishes, e.g. the current layout in an `Arc<Mutex<_>>`).
    pub fn set_pointer_map(&self, map: Option<PointerMap>) {
        self.lock().map = map;
    }

    /// Window pixels per point of the screen the viewer is on (`NSWindow.backingScaleFactor`): the shell's mouse events
    /// are in points, the renderer's `map_pointer` in pixels. Call again when the window changes screen.
    pub fn set_backing_scale(&self, pixels_per_point: f64) {
        if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            self.lock().scale = pixels_per_point;
        }
    }

    /// New mode, layout comparison, swap, display or scroll setting; applies at once.
    pub fn set_config(&self, config: InputConfig) {
        self.lock().core.set_config(config);
    }

    pub fn config(&self) -> InputConfig {
        self.lock().core.config()
    }

    /// Starts sending on `tx` (the grabber's `start` does this). Modifiers already held are sent at once when the video
    /// view has the focus.
    pub fn attach(&self, tx: SyncSender<InputEvent>) {
        let mut st = self.lock();
        st.port = Some(Port { tx, lost: false });
        if st.focused {
            let flags = self.inner.keyboard.flags();
            let probe = |k: u16| self.inner.keyboard.key_down(k);
            let events = st.core.focus_gained(flags, &probe);
            st.deliver(events);
        }
    }

    /// Stops sending: the host is told to release everything first.
    pub fn detach(&self) {
        let mut st = self.lock();
        if st.port.is_some() {
            let events = st.core.release_all();
            st.deliver(events);
        }
        st.port = None;
    }

    pub fn is_attached(&self) -> bool {
        self.lock().port.is_some()
    }

    /// Records the consumer was too slow to take since the input was created.
    pub fn dropped_events(&self) -> u64 {
        self.lock().dropped
    }

    /// What the UI shows about "capturar atalhos do sistema".
    pub fn grab_status(&self) -> GrabStatus {
        self.lock().grab
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn set_grab_status(&self, status: GrabStatus) {
        self.lock().grab = status;
    }

    /// System-key capture went off: a key the tap swallowed is released on the host.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn system_keys_off(&self) {
        let mut st = self.lock();
        if st.core.system_keys_off() {
            st.deliver(vec![InputEvent::ReleaseAll]);
        }
    }

    /// One key event of the event tap (the tap's thread). True when it was swallowed and sent.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn tap_key(&self, kvk: u16, down: bool, flags: Modifiers) -> bool {
        let mut st = self.lock();
        if st.port.is_none() || (down && !st.focused) {
            return false;
        }
        let probe = |k: u16| self.inner.keyboard.key_down(k);
        let outcome = st.core.tap_key(kvk, down, flags, &probe);
        st.apply(outcome)
    }
}

impl ViewerInput for MacInput {
    fn focus(&self, focused: bool) {
        let mut st = self.lock();
        st.focused = focused;
        if st.port.is_none() {
            return;
        }
        let events = if focused {
            let flags = self.inner.keyboard.flags();
            let probe = |k: u16| self.inner.keyboard.key_down(k);
            st.core.focus_gained(flags, &probe)
        } else {
            st.core.release_all()
        };
        st.deliver(events);
    }

    fn captures_keyboard(&self) -> bool {
        let st = self.lock();
        st.port.is_some() && st.focused
    }

    fn key(&self, msg: &KeyMsg) -> bool {
        let mut st = self.lock();
        if st.port.is_none() {
            return false;
        }
        let probe: KeyProbe<'_> = &|k: u16| self.inner.keyboard.key_down(k);
        let outcome = st.core.key(msg, probe);
        st.apply(outcome)
    }

    fn modifiers_changed(&self, modifiers: Modifiers) -> bool {
        let mut st = self.lock();
        if st.port.is_none() {
            return false;
        }
        let probe: KeyProbe<'_> = &|k: u16| self.inner.keyboard.key_down(k);
        let outcome = st.core.modifiers_changed(modifiers, probe);
        st.apply(outcome)
    }

    fn mouse(&self, msg: MouseMsg) -> bool {
        let mut st = self.lock();
        if st.port.is_none() {
            return false;
        }
        let map = st.map.clone();
        let scale = st.scale;
        let outcome = st.core.mouse(msg, map.as_deref().map(|f| f as &dyn Fn(i32, i32) -> Option<(u16, u16)>), scale);
        st.apply(outcome)
    }

    fn hides_pointer(&self) -> bool {
        let st = self.lock();
        st.port.is_some() && st.focused && st.core.config().hide_local_pointer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::mpsc::{Receiver, sync_channel};
    use swapdesk_keys::modifier;
    use swapdesk_proto::MouseButton;
    use swapdesk_shell_mac::{MouseKind, PointerButton};

    /// A fake keyboard: one key down at a time (by kVK), and the flags.
    struct Fake {
        down: AtomicU16,
        flags: Mutex<Modifiers>,
    }

    impl KeyboardState for Arc<Fake> {
        fn key_down(&self, kvk: u16) -> bool {
            self.down.load(Ordering::Relaxed) == kvk
        }
        fn flags(&self) -> Modifiers {
            *self.flags.lock().expect("flags")
        }
    }

    fn rig(config: InputConfig, cap: usize) -> (MacInput, Receiver<InputEvent>, Arc<Fake>) {
        let fake = Arc::new(Fake { down: AtomicU16::new(0xFFFF), flags: Mutex::new(Modifiers::default()) });
        let input = MacInput::with_keyboard_state(config, Box::new(Arc::clone(&fake)));
        let (tx, rx) = sync_channel(cap);
        input.attach(tx);
        (input, rx, fake)
    }

    fn drain(rx: &Receiver<InputEvent>) -> Vec<InputEvent> {
        rx.try_iter().collect()
    }

    fn key(kvk: u16, down: bool, mods: Modifiers) -> KeyMsg {
        KeyMsg { key_code: kvk, down, repeat: false, text: None, modifiers: mods }
    }

    fn map(x: i32, y: i32) -> Option<(u16, u16)> {
        (x >= 0 && y >= 0 && x < 100 && y < 100).then_some((x as u16 * 600, y as u16 * 600))
    }

    #[test]
    fn nothing_is_taken_until_a_consumer_is_attached() {
        let input = MacInput::new(InputConfig::default());
        assert!(!input.key(&key(0, true, Modifiers::default())));
        assert!(!input.modifiers_changed(Modifiers::SHIFT));
        assert!(!input.mouse(MouseMsg { kind: MouseKind::Move, x: 1.0, y: 1.0, modifiers: Modifiers::default() }));
        input.focus(true);
        assert!(!input.captures_keyboard(), "no consumer, no capture");
        assert!(!input.hides_pointer());
    }

    #[test]
    fn keys_modifiers_and_the_pointer_reach_the_channel() {
        let (input, rx, fake) = rig(InputConfig::default(), 64);
        input.focus(true);
        assert!(input.captures_keyboard() && input.hides_pointer());
        fake.down.store(0x3B, Ordering::Relaxed);
        assert!(input.modifiers_changed(Modifiers::CONTROL));
        assert!(input.key(&key(0x08, true, Modifiers::CONTROL)));
        input.set_pointer_map(Some(Arc::new(map)));
        input.set_backing_scale(2.0);
        assert!(input.mouse(MouseMsg {
            kind: MouseKind::Button { button: PointerButton::Left, down: true },
            x: 10.0,
            y: 5.0,
            modifiers: Modifiers::CONTROL,
        }));
        let got = drain(&rx);
        assert_eq!(got.len(), 4, "{got:?}");
        assert!(
            matches!(got[0], InputEvent::Key { usage: 0xE0, down: true, modifiers, .. } if modifiers == modifier::LCTRL)
        );
        assert!(matches!(got[1], InputEvent::Key { usage: 0x06, down: true, .. }));
        assert_eq!(
            got[2],
            InputEvent::Pointer { display: 0, x: 20 * 600, y: 10 * 600 },
            "points times scale, through the map"
        );
        assert_eq!(got[3], InputEvent::Button { button: MouseButton::Left, down: true });
    }

    #[test]
    fn losing_the_focus_sends_release_all_and_gaining_it_sends_the_held_modifiers() {
        let (input, rx, fake) = rig(InputConfig::default(), 64);
        input.focus(true);
        fake.down.store(0x38, Ordering::Relaxed);
        input.modifiers_changed(Modifiers::SHIFT);
        drain(&rx);
        input.focus(false);
        assert_eq!(drain(&rx), vec![InputEvent::ReleaseAll]);
        assert!(!input.captures_keyboard());
        // Back with Shift still held on the keyboard.
        *fake.flags.lock().expect("flags") = Modifiers::SHIFT;
        input.focus(true);
        let got = drain(&rx);
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], InputEvent::Key { usage: 0xE1, down: true, .. }));
    }

    #[test]
    fn detach_releases_everything_and_stops_taking_events() {
        let (input, rx, _fake) = rig(InputConfig::default(), 64);
        input.focus(true);
        input.key(&key(0x00, true, Modifiers::default()));
        drain(&rx);
        input.detach();
        assert_eq!(drain(&rx), vec![InputEvent::ReleaseAll]);
        assert!(!input.is_attached());
        assert!(!input.key(&key(0x00, false, Modifiers::default())));
    }

    #[test]
    fn a_slow_consumer_costs_a_release_all_before_the_next_record_and_never_blocks() {
        let (input, rx, _fake) = rig(InputConfig::default(), 2);
        input.focus(true);
        assert!(input.key(&key(0x00, true, Modifiers::default())));
        assert!(input.key(&key(0x01, true, Modifiers::default())));
        assert!(
            input.key(&key(0x02, true, Modifiers::default())),
            "queue full: the record is lost, the call still returns"
        );
        assert_eq!(input.dropped_events(), 1);
        assert!(matches!(rx.try_recv(), Ok(InputEvent::Key { usage: 0x04, .. })));
        assert!(matches!(rx.try_recv(), Ok(InputEvent::Key { usage: 0x16, .. })));
        // Room again: the next record is preceded by ReleaseAll (the host drops what it held, the input forgot it).
        assert!(input.key(&key(0x03, true, Modifiers::default())));
        assert_eq!(rx.try_recv(), Ok(InputEvent::ReleaseAll));
        assert!(
            matches!(rx.try_recv(), Ok(InputEvent::Key { usage: 0x09, .. })),
            "the record that follows the release"
        );
    }

    #[test]
    fn a_dropped_position_is_not_remembered_as_sent() {
        let (input, rx, _fake) = rig(InputConfig::default(), 1);
        input.focus(true);
        input.set_pointer_map(Some(Arc::new(map)));
        let mv = |x: f64| MouseMsg { kind: MouseKind::Move, x, y: 1.0, modifiers: Modifiers::default() };
        input.mouse(mv(1.0));
        input.mouse(mv(2.0)); // queue of one is full: dropped
        assert_eq!(rx.try_recv(), Ok(InputEvent::Pointer { display: 0, x: 600, y: 600 }));
        input.mouse(mv(2.0));
        assert_eq!(
            rx.try_recv(),
            Ok(InputEvent::Pointer { display: 0, x: 1200, y: 600 }),
            "the lost position is sent at the next event"
        );
    }

    #[test]
    fn the_tap_path_swallows_system_shortcuts_only_while_focused_and_attached() {
        let (input, rx, fake) = rig(InputConfig::default(), 64);
        fake.down.store(0x37, Ordering::Relaxed);
        assert!(!input.tap_key(0x30, true, Modifiers::COMMAND), "not focused: macOS gets Cmd+Tab");
        input.focus(true);
        drain(&rx);
        assert!(input.tap_key(0x30, true, Modifiers::COMMAND));
        assert!(!input.tap_key(0x00, true, Modifiers::default()), "ordinary keys go through the view");
        let got = drain(&rx);
        assert!(matches!(got[0], InputEvent::Key { usage: 0xE3, down: true, .. }));
        assert!(matches!(got[1], InputEvent::Key { usage: 0x2B, down: true, .. }));
        assert!(input.tap_key(0x30, false, Modifiers::default()), "its release is swallowed even if focus moved on");
        input.detach();
        assert!(!input.tap_key(0x30, true, Modifiers::COMMAND), "detached: macOS gets it");
    }

    #[test]
    fn system_keys_off_under_a_swallowed_key_sends_release_all() {
        let (input, rx, _fake) = rig(InputConfig::default(), 64);
        input.focus(true);
        input.tap_key(0x31, true, Modifiers::COMMAND);
        drain(&rx);
        input.system_keys_off();
        assert_eq!(drain(&rx), vec![InputEvent::ReleaseAll]);
        input.system_keys_off();
        assert_eq!(drain(&rx), vec![], "nothing held, nothing to release");
    }

    #[test]
    fn grab_status_starts_off_and_a_bad_scale_is_ignored() {
        let input = MacInput::new(InputConfig::default());
        assert_eq!(input.grab_status(), GrabStatus::Off);
        input.set_backing_scale(f64::NAN);
        input.set_backing_scale(-1.0);
        input.set_backing_scale(0.0);
        input.set_grab_status(GrabStatus::NeedsPermission(Access { input_monitoring: true, accessibility: false }));
        assert!(matches!(input.grab_status(), GrabStatus::NeedsPermission(a) if !a.complete()));
    }

    #[test]
    fn config_changes_apply_to_the_running_input() {
        let (input, rx, _fake) = rig(InputConfig::default(), 64);
        input.focus(true);
        input.set_config(InputConfig { swap_ctrl_cmd: true, num_lock: false, ..InputConfig::default() });
        let mut msg = key(0x08, true, Modifiers::default());
        msg.text = Some("c".into());
        input.key(&msg);
        assert_eq!(
            drain(&rx),
            vec![InputEvent::Key { usage_page: 7, usage: 0x06, down: true, modifiers: 0, locks: 0 }]
        );
        assert!(!input.config().num_lock);
    }
}
