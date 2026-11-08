//! The macOS calls of `swapdesk-input-mac` against the real system. Nothing here injects input into the session and
//! nothing calls `request_access` (it would raise a prompt on the owner's screen): the grants are only read.
//!
//! The event tap needs Input Monitoring and Accessibility for the process that runs the tests (the Terminal or the
//! SSH daemon's wrapper that started `cargo nextest`). Without them the tap tests check the reported state and say so;
//! with them they also make and stop a real tap (still without a key event: no input is posted).
#![cfg(target_os = "macos")]

use std::sync::mpsc::sync_channel;
use std::time::Duration;

use swapdesk_input_mac::KeyboardState;
use swapdesk_input_mac::{
    GrabStatus, InputConfig, MacGrabber, MacInput, SystemKeyboardState, access, detect_keyboard_kind,
};
use swapdesk_keys::KbKind;
use swapdesk_platform::{ErrorKind, InputGrabber, OsGrant};
use swapdesk_proto::InputEvent;

#[test]
fn the_grants_are_read_without_a_prompt_and_the_keyboard_kind_is_known() {
    let a = access();
    eprintln!("INFO input monitoring granted: {}, accessibility granted: {}", a.input_monitoring, a.accessibility);
    let kind = detect_keyboard_kind();
    eprintln!("INFO keyboard kind of this Mac: {kind:?}");
    assert!(KbKind::ALL.contains(&kind));
    assert_eq!(InputConfig::detected().keyboard, kind);
}

#[test]
fn the_system_keyboard_state_reads_modifiers_and_keys_without_a_grant() {
    let state = SystemKeyboardState;
    // Nobody types during the run; a held key would only make the numbers differ, not break the calls.
    let flags = state.flags();
    let any_modifier_key =
        [0x37, 0x36, 0x38, 0x3C, 0x3A, 0x3D, 0x3B, 0x3E].into_iter().filter(|k| state.key_down(*k)).count();
    eprintln!("INFO flags {flags:?}, modifier keys down {any_modifier_key}");
}

#[test]
fn system_key_capture_reports_a_missing_grant_as_a_state_and_never_loops_on_a_prompt() {
    let input = MacInput::new(InputConfig::detected());
    let mut grabber = MacGrabber::new(input.clone());
    let (tx, rx) = sync_channel::<InputEvent>(16);
    assert_eq!(input.grab_status(), GrabStatus::Off);
    grabber.set_grab_system_keys(true).expect("asked before start: only remembered");
    assert_eq!(input.grab_status(), GrabStatus::Off, "nothing runs before start");
    grabber.start(tx.clone()).expect("start");
    assert!(grabber.start(tx).is_err(), "a second start is refused");
    let have = access();
    if have.complete() {
        assert_eq!(input.grab_status(), GrabStatus::Active, "the grants are there: the tap runs");
        grabber.set_grab_system_keys(false).expect("off");
        assert_eq!(input.grab_status(), GrabStatus::Off);
        grabber.set_grab_system_keys(true).expect("on again: a fresh tap");
        assert_eq!(input.grab_status(), GrabStatus::Active);
    } else {
        eprintln!(
            "SKIP (partly) the tap itself: the test process lacks {have:?}; grant it in System Settings > Privacy & Security > Input Monitoring and Accessibility"
        );
        assert!(
            matches!(input.grab_status(), GrabStatus::NeedsPermission(a) if a == have),
            "state shows what is missing"
        );
        for _ in 0..3 {
            let err = grabber.set_grab_system_keys(true).expect_err("still missing");
            let want = if have.input_monitoring { OsGrant::Accessibility } else { OsGrant::InputMonitoring };
            assert_eq!(err.kind, ErrorKind::PermissionDenied(want));
        }
        grabber.set_grab_system_keys(false).expect("off is always fine");
        assert_eq!(input.grab_status(), GrabStatus::Off);
    }
    grabber.stop();
    assert_eq!(input.grab_status(), GrabStatus::Off);
    assert!(!input.is_attached());
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)),
        Ok(InputEvent::ReleaseAll),
        "stop tells the host to release everything"
    );
}

#[test]
fn the_grabber_stops_on_drop_and_can_start_again() {
    let input = MacInput::new(InputConfig::default());
    {
        let mut g = MacGrabber::new(input.clone());
        let (tx, _rx) = sync_channel::<InputEvent>(4);
        g.start(tx).expect("start");
        assert!(input.is_attached());
    }
    assert!(!input.is_attached(), "dropping the grabber detaches the input");
    let mut g = MacGrabber::new(input.clone());
    let (tx, rx) = sync_channel::<InputEvent>(4);
    g.start(tx).expect("start again");
    g.stop();
    assert_eq!(rx.try_recv(), Ok(InputEvent::ReleaseAll));
}
