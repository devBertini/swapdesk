//! The viewer input path on the MacBook, with no window and no event tap: the commands and events the UI exchanges (seen
//! by `ObservingBackend`) decide whether the viewer's keys and pointer reach the host, and what reaches it is recorded
//! at `ViewerOut::input`. `system_keys` is false, so nothing here asks macOS for Input Monitoring or Accessibility and
//! nothing intercepts this Mac's real keyboard. Run with `scripts/mac/remote.sh test -p swapdesk-app-mac`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_input_mac::GrabStatus;
use swapdesk_platform::{Rect, ScaleMode};
use swapdesk_proto::InputEvent;
use swapdesk_render_mac::layout::Layout;
use swapdesk_shell_mac::{KeyMsg, Modifiers, MouseKind, MouseMsg, ViewerInput};
use swapdesk_ui::{Command, Permission, Permissions, Role, SessionState, Ticket, UiBackend, UiEvent};

use crate::layout_state::PointerLayout;
use crate::media_tests::Recorder;
use crate::pipeline::{MacViewer, ObservingBackend, ViewerOptions};

const WAIT: Duration = Duration::from_secs(5);
const SESSION: u64 = 9;

/// The UI's backend: what the test queues is what the UI drains; every command is recorded.
#[derive(Default)]
struct Scripted {
    events: Mutex<Vec<UiEvent>>,
    sent: Mutex<Vec<Command>>,
    next: AtomicU64,
}

impl UiBackend for Scripted {
    fn send(&self, command: Command) -> Ticket {
        self.sent.lock().unwrap().push(command);
        Ticket(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn drain(&self) -> Vec<UiEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

struct Rig {
    viewer: Arc<MacViewer>,
    rec: Arc<Recorder>,
    inner: Arc<Scripted>,
    backend: ObservingBackend,
}

impl Rig {
    fn new() -> Self {
        let viewer =
            MacViewer::new(ViewerOptions { on_present: None, audio: false, system_keys: false, wake: Box::new(|| {}) })
                .expect("the pipeline starts");
        let rec = Arc::new(Recorder::default());
        viewer.set_out(rec.clone());
        // A picture is on screen: 160x96 fitted into 800x600 window pixels.
        let area = Rect { x: 0, y: 0, width: 800, height: 600 };
        let layout = Layout::compute((160, 96), (800, 600), ScaleMode::Fit).expect("layout");
        viewer.layout().set(Some(PointerLayout { area, frame: (160, 96), layout }));
        // "Teclas físicas": the key records below are physical keys. In "Automático" toward a Windows host a letter goes
        // as text, which the viewer now reads from this Mac's own keyboard layout, not from the message's characters.
        viewer.set_user_prefs(swapdesk_ui::KeyboardMode::Physical, false);
        let inner = Arc::new(Scripted::default());
        let backend = ObservingBackend::new(inner.clone(), Arc::clone(&viewer));
        Self { viewer, rec, inner, backend }
    }

    fn session_event(&self, state: SessionState) {
        self.inner.events.lock().unwrap().push(UiEvent::Session {
            session_id: SESSION,
            device_id: "dev-1".into(),
            state,
        });
        assert_eq!(self.backend.drain().len(), 1, "the event passes through unchanged");
    }

    fn active(&self, granted: Permissions) {
        self.session_event(SessionState::Active { since_ms: 0, role: Role::Viewer, granted });
    }

    fn recorded(&self) -> Vec<(u64, InputEvent)> {
        self.rec
            .inputs
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(id, events)| events.iter().cloned().map(move |e| (*id, e)))
            .collect()
    }

    fn wait_for(&self, what: &str, pred: impl Fn(&[(u64, InputEvent)]) -> bool) -> Vec<(u64, InputEvent)> {
        let end = Instant::now() + WAIT;
        loop {
            let got = self.recorded();
            if pred(&got) {
                return got;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}: {got:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.viewer.shutdown();
    }
}

fn key_a(down: bool) -> KeyMsg {
    // kVK_ANSI_A, no text: a physical key (HID usage page 7, usage 0x04).
    KeyMsg { key_code: 0x00, down, repeat: false, text: None, modifiers: Modifiers::default() }
}

fn control() -> Permissions {
    Permissions::NONE.with(Permission::Control)
}

#[test]
fn a_session_with_control_feeds_the_keys_and_the_pointer_to_the_host_and_focus_loss_releases_them() {
    let rig = Rig::new();
    let input = rig.viewer.input();
    assert!(!input.key(&key_a(true)), "no session: nothing is taken");
    rig.active(control());
    assert_eq!(rig.viewer.input_session(), SESSION);
    assert!(rig.viewer.grabbing(), "the viewer's input is attached");
    assert!(matches!(input.grab_status(), GrabStatus::Off), "system_keys is false: no tap, no permission asked");

    input.focus(true);
    assert!(input.captures_keyboard());
    assert!(input.key(&key_a(true)) && input.key(&key_a(false)));
    input.set_backing_scale(2.0);
    // 100x150 points at 2 pixels per point is the window pixel (200, 300), inside the picture.
    assert!(input.mouse(MouseMsg { kind: MouseKind::Move, x: 100.0, y: 150.0, modifiers: Modifiers::default() }));
    let expected = rig.viewer.layout().map(200, 300).expect("the pointer is over the picture");

    let got =
        rig.wait_for("the key and pointer records", |g| g.iter().any(|(_, e)| matches!(e, InputEvent::Pointer { .. })));
    assert!(got.iter().all(|(id, _)| *id == SESSION), "every record carries the session: {got:?}");
    assert!(
        got.iter().any(|(_, e)| matches!(e, InputEvent::Key { usage_page: 7, usage: 0x04, down: true, .. })),
        "{got:?}"
    );
    assert!(
        got.iter().any(|(_, e)| matches!(e, InputEvent::Key { usage_page: 7, usage: 0x04, down: false, .. })),
        "{got:?}"
    );
    assert!(
        got.iter().any(|(_, e)| *e == InputEvent::Pointer { display: 0, x: expected.0, y: expected.1 }),
        "{got:?} wanted {expected:?}"
    );

    input.focus(false);
    assert!(!input.captures_keyboard());
    rig.wait_for("ReleaseAll after the focus left", |g| g.iter().any(|(_, e)| *e == InputEvent::ReleaseAll));
}

#[test]
fn without_the_control_permission_nothing_is_attached_or_sent() {
    let rig = Rig::new();
    rig.active(Permissions::NONE.with(Permission::View));
    assert_eq!(rig.viewer.input_session(), 0);
    assert!(!rig.viewer.grabbing());
    let input = rig.viewer.input();
    input.focus(true);
    assert!(!input.key(&key_a(true)), "not taken: the view is view-only by grant");
    assert!(!input.captures_keyboard());
    std::thread::sleep(Duration::from_millis(150));
    assert!(rig.recorded().is_empty());
}

#[test]
fn view_only_and_the_end_of_the_session_let_go_of_what_the_host_holds() {
    let rig = Rig::new();
    rig.active(control());
    let input = rig.viewer.input();
    input.focus(true);
    assert!(input.key(&key_a(true)));
    rig.wait_for("the key press", |g| g.iter().any(|(_, e)| matches!(e, InputEvent::Key { down: true, .. })));

    // "Somente ver" on: the host is told to release everything, now, for the session that held it; then input stops.
    rig.backend.send(Command::SetViewOnly { session_id: SESSION, on: true });
    assert_eq!(rig.viewer.input_session(), 0);
    assert!(!rig.viewer.grabbing());
    assert!(
        rig.recorded().iter().any(|(id, e)| *id == SESSION && *e == InputEvent::ReleaseAll),
        "{:?}",
        rig.recorded()
    );
    assert!(!input.key(&key_a(true)), "view-only: the key stays local");
    assert_eq!(rig.inner.sent.lock().unwrap().len(), 1, "the command still reaches the service");

    // Off again: the same session takes input again.
    rig.backend.send(Command::SetViewOnly { session_id: SESSION, on: false });
    assert_eq!(rig.viewer.input_session(), SESSION);
    assert!(rig.viewer.grabbing());

    // The session ends: input stops for good.
    rig.session_event(SessionState::Ended { reason: None });
    assert_eq!(rig.viewer.input_session(), 0);
    assert!(!rig.viewer.grabbing());
}

#[test]
fn the_grab_switch_changes_nothing_while_system_keys_is_off() {
    let rig = Rig::new();
    rig.active(control());
    rig.backend.send(Command::SetGrab { session_id: SESSION, on: true });
    rig.backend.send(Command::SetGrab { session_id: SESSION, on: false });
    assert!(rig.viewer.grabbing(), "the view's own keys keep working");
    assert!(matches!(rig.viewer.input().grab_status(), GrabStatus::Off));
}
