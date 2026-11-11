//! The viewer pipeline's keyboard decisions with no window, no hook and no decoder (the pipeline is never focused, so the
//! window-message path is the one in use): the viewer's own shortcuts (D30), "release the keyboard" and the IME's text.
//! The system-wide hook path of the same shortcuts is `swapdesk-input-win`'s `grab` tests (state) and its interactive
//! test (the real hook).
#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_app::viewer::{ViewerOptions, ViewerOut, ViewerPipeline};
use swapdesk_proto::{InputEvent, KeyframeRequest, ReceiverReport};
use swapdesk_shell_win::{KeyMsg, MouseKind, MouseMsg, PointerButton, ViewerHost};
use swapdesk_ui::{Command, Permission, Permissions, ReleaseKey, Role, SessionState, UiEvent, UiWaker, ViewerAction};

const SESSION: u64 = 7;

#[derive(Default)]
struct Recorder {
    inputs: Mutex<Vec<InputEvent>>,
}

impl ViewerOut for Recorder {
    fn input(&self, _session_id: u64, events: Vec<InputEvent>) {
        self.inputs.lock().unwrap().extend(events);
    }
    fn keyframe(&self, _: u64, _: KeyframeRequest) {}
    fn report(&self, _: u64, _: ReceiverReport) {}
}

#[derive(Default)]
struct CountingWaker(Mutex<u32>);

impl UiWaker for CountingWaker {
    fn wake(&self) {
        *self.0.lock().unwrap() += 1;
    }
}

struct Rig {
    pipeline: Arc<ViewerPipeline>,
    recorder: Arc<Recorder>,
    waker: Arc<CountingWaker>,
}

fn rig() -> Rig {
    let pipeline = ViewerPipeline::new(ViewerOptions { on_present: None, audio: false, ..ViewerOptions::default() })
        .expect("pipeline");
    let recorder = Arc::new(Recorder::default());
    pipeline.set_out(recorder.clone());
    let waker = Arc::new(CountingWaker::default());
    pipeline.set_waker(waker.clone());
    pipeline.observe_event(&UiEvent::Session {
        session_id: SESSION,
        device_id: "pc".into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::Control),
        },
    });
    Rig { pipeline, recorder, waker }
}

fn key(vk: u32, scan: u32, extended: bool, down: bool) -> KeyMsg {
    KeyMsg { vk, scan, extended, down, text: None, dead: false, locks: 0 }
}

fn text(t: &str) -> KeyMsg {
    KeyMsg { vk: 0, scan: 0, extended: false, down: true, text: Some(t.into()), dead: false, locks: 0 }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn sent(r: &Rig) -> Vec<InputEvent> {
    // The input thread batches: give it a moment, then read what it has written.
    std::thread::sleep(Duration::from_millis(150));
    r.recorder.inputs.lock().unwrap().clone()
}

#[test]
fn a_shortcut_wakes_the_ui_and_is_taken_by_the_shell_and_its_keys_never_reach_the_host() {
    let r = rig();
    for m in [key(0xA2, 0x1D, false, true), key(0xA4, 0x38, false, true)] {
        assert!(r.pipeline.key(&m));
    }
    let wakes_before = *r.waker.0.lock().unwrap();
    assert!(r.pipeline.key(&key(0x42, 0x30, false, true)), "Ctrl+Alt+B is taken");
    assert!(r.pipeline.key(&key(0x42, 0x30, false, false)));
    // Letting go of Ctrl+Alt with B between was no release; F11 then is full screen.
    assert!(r.pipeline.key(&key(0xA4, 0x38, false, false)));
    assert!(r.pipeline.key(&key(0xA2, 0x1D, false, false)));
    assert!(r.pipeline.key(&key(0x7A, 0x57, false, true)), "F11 is taken");
    assert_eq!(*r.waker.0.lock().unwrap(), wakes_before + 2, "each shortcut wakes the UI thread once");
    assert_eq!(r.pipeline.take_shortcuts(), vec![ViewerAction::ToggleBar, ViewerAction::FullScreen]);
    assert!(r.pipeline.take_shortcuts().is_empty(), "taken once");
    let events = sent(&r);
    assert!(
        events.iter().all(|e| !matches!(e, InputEvent::Key { usage: 0x05 | 0x44, .. })),
        "the letters of a shortcut are not for the host: {events:?}"
    );
    assert!(events.iter().any(|e| matches!(e, InputEvent::Key { usage: 0xE0, down: true, .. })), "Ctrl still goes");
}

#[test]
fn the_release_key_gives_the_keyboard_back_until_a_click_on_the_picture() {
    let r = rig();
    assert!(r.pipeline.captures_keyboard());
    r.pipeline.key(&key(0xA2, 0x1D, false, true));
    r.pipeline.key(&key(0xA4, 0x38, false, true));
    r.pipeline.key(&key(0xA4, 0x38, false, false));
    // The shell's drain takes the shortcut; the release itself is the pipeline's, not the shell's.
    assert_eq!(r.pipeline.take_shortcuts(), vec![]);
    assert!(!r.pipeline.captures_keyboard(), "the window's keys go to this PC now");
    assert!(!r.pipeline.key(&key(0x41, 0x1E, false, true)), "a key is left to the window");
    wait_for("the host to let go of the held keys", || {
        r.recorder.inputs.lock().unwrap().contains(&InputEvent::ReleaseAll)
    });

    // A click outside the picture does not take it back; one on the picture does.
    r.pipeline.mouse(MouseMsg {
        kind: MouseKind::Button { button: PointerButton::Left, down: true },
        x: 5,
        y: 5,
        inside: false,
    });
    assert!(!r.pipeline.captures_keyboard());
    r.pipeline.mouse(MouseMsg {
        kind: MouseKind::Button { button: PointerButton::Left, down: true },
        x: 50,
        y: 50,
        inside: true,
    });
    assert!(r.pipeline.captures_keyboard());
}

#[test]
fn focus_coming_back_captures_the_keyboard_again_and_the_chord_follows_the_setting() {
    let r = rig();
    r.pipeline.observe_command(&Command::SetReleaseKey { key: ReleaseKey::RightCtrl });
    // Left Ctrl + Alt is not the chord any more.
    for down in [true, false] {
        r.pipeline.key(&key(0xA2, 0x1D, false, down));
    }
    assert!(r.pipeline.take_shortcuts().is_empty() && r.pipeline.captures_keyboard());
    r.pipeline.key(&key(0xA3, 0x1D, true, true));
    r.pipeline.key(&key(0xA3, 0x1D, true, false));
    r.pipeline.take_shortcuts();
    assert!(!r.pipeline.captures_keyboard(), "right Ctrl released the keyboard");
    r.pipeline.focus(false);
    r.pipeline.focus(true);
    assert!(r.pipeline.captures_keyboard(), "back in the window, the keyboard is captured again");
}

#[test]
fn ime_text_is_sent_as_a_text_record_and_its_process_keys_are_dropped() {
    let r = rig();
    // The IME takes the key-downs: VK_PROCESSKEY, nothing for the host. The result arrives as text.
    assert!(r.pipeline.key(&key(0xE5, 0x1E, false, true)));
    assert!(r.pipeline.key(&key(0xE5, 0x1E, false, false)));
    assert!(r.pipeline.key(&text("日本")));
    wait_for("the text record", || r.recorder.inputs.lock().unwrap().iter().any(|e| matches!(e, InputEvent::Text(_))));
    let events = sent(&r);
    assert_eq!(events, vec![InputEvent::Text("日本".into())], "no key record for the process keys");
}

#[test]
fn nothing_is_sent_without_control() {
    let pipeline = ViewerPipeline::new(ViewerOptions { on_present: None, audio: false, ..ViewerOptions::default() })
        .expect("pipeline");
    assert!(!pipeline.captures_keyboard());
    assert!(!pipeline.key(&text("a")));
    assert!(!pipeline.key(&key(0x7A, 0x57, false, true)));
    assert!(pipeline.take_shortcuts().is_empty());
}
