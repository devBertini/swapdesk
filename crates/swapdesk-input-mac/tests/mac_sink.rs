//! The Mac sink and clipboard against the real system. Only the interactive test moves the pointer; it is ignored
//! by default (the Mac may be in use) and needs the Accessibility grant of the process that runs it.
#![cfg(target_os = "macos")]

use objc2_core_graphics::{CGEvent, CGEventSource, CGEventSourceStateID};
use swapdesk_input_mac::{MacClipboard, MacInputSink, can_post};
use swapdesk_platform::{
    AdapterId, ClipContent, ClipRead, ClipboardProvider, DisplayId, DisplayInfo, ErrorKind, InputSink, OsGrant, Rect,
};
use swapdesk_proto::InputEvent;

fn display(bounds: Rect, scale_milli: u16) -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(1),
        name: String::new(),
        bounds,
        scale_milli,
        refresh_mhz: 60_000,
        rotation: 0,
        primary: true,
        adapter: AdapterId(0),
    }
}

#[test]
fn without_accessibility_apply_is_permission_denied_and_nothing_is_held() {
    let trusted = can_post();
    eprintln!("INFO accessibility granted to this process: {trusted}");
    if trusted {
        eprintln!(
            "INFO this process is trusted: the denial cannot be observed here, only that nothing errors on a no-op"
        );
        let mut sink = MacInputSink::new();
        assert!(sink.release_all().is_ok());
        return;
    }
    let mut sink = MacInputSink::new();
    sink.displays_changed(&[display(Rect { x: 0, y: 0, width: 100, height: 100 }, 1000)]);
    for event in [
        InputEvent::Key { usage_page: 7, usage: 0x04, down: true, modifiers: 0, locks: 0 },
        InputEvent::Pointer { display: 1, x: 10, y: 10 },
        InputEvent::Text("a".into()),
        InputEvent::Wheel { dx: 0, dy: 120 },
    ] {
        let e = sink.apply(&event).unwrap_err();
        assert_eq!(e.kind, ErrorKind::PermissionDenied(OsGrant::Accessibility), "{e}");
    }
    assert!(sink.held_keys().is_empty(), "a refused key down is not tracked");
    assert!(sink.release_all().is_ok(), "nothing held, nothing to post");
    assert!(sink.follow_input_desktop().is_ok());
}

#[test]
#[ignore = "interactive: moves the Mac's pointer; needs the Accessibility grant"]
fn interactive_the_pointer_lands_on_the_planned_point() {
    assert!(can_post(), "grant Accessibility to the process that runs this test");
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).unwrap();
    let start = CGEvent::location(CGEvent::new(Some(&source)).as_deref());
    let mut sink = MacInputSink::new();
    // A 1000x1000 point display at the origin at scale 1: wire (32768, 32768) is point (500, 500) after rounding.
    sink.displays_changed(&[display(Rect { x: 0, y: 0, width: 1000, height: 1000 }, 1000)]);
    sink.apply(&InputEvent::Pointer { display: 1, x: 32768, y: 32768 }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let at = CGEvent::location(CGEvent::new(Some(&source)).as_deref());
    eprintln!("INFO pointer was ({}, {}), now ({}, {})", start.x, start.y, at.x, at.y);
    assert!((at.x - 500.0).abs() <= 1.0 && (at.y - 500.0).abs() <= 1.0, "pointer at ({}, {})", at.x, at.y);
    sink.apply(&InputEvent::Pointer { display: 1, x: 0, y: 0 }).unwrap();
    sink.release_all().unwrap();
}

#[test]
fn the_clipboard_text_round_trips_and_the_change_count_moves() {
    let mut clip = MacClipboard::new();
    let before = clip.change_count().unwrap();
    let text = format!("swapdesk clipboard test ação {before}");
    clip.write(&ClipContent::Text(text.clone())).unwrap();
    let after = clip.change_count().unwrap();
    assert!(after > before, "change count {before} -> {after}");
    assert_eq!(clip.read().unwrap(), ClipRead::Content(ClipContent::Text(text)));
}

#[test]
fn the_clipboard_refuses_what_is_not_a_png_and_what_is_too_big() {
    let mut clip = MacClipboard::new();
    let e = clip.write(&ClipContent::Png(b"not a png".to_vec())).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidInput);
    let big = "x".repeat(swapdesk_proto::MAX_CLIPBOARD_TEXT as usize + 1);
    assert_eq!(clip.write(&ClipContent::Text(big)).unwrap_err().kind, ErrorKind::InvalidInput);
}
