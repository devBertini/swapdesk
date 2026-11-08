//! Real `SendInput` into a window this test creates and focuses (stage 16). The window's own message loop reads what
//! arrives: Unicode mode must give the exact text, physical mode the exact scan codes, the pointer the exact client
//! point. Input is injected only while the test window is the foreground window; the cursor is put back at the end.
#![cfg(windows)]

mod common;

use common::{CursorGuard, Ev, TestWindow, aware, chars, cursor, key_downs, serial};
use std::time::Duration;
use swapdesk_capture_win::displays;
use swapdesk_input_win::{WinInputSink, open_sink};
use swapdesk_keys::modifier;
use swapdesk_platform::{DisplayInfo, InputSink};
use swapdesk_proto::{InputEvent, MouseButton};
use windows::core::w;

struct Rig {
    _serial: common::Serial,
    _cursor: CursorGuard,
    win: TestWindow,
    sink: WinInputSink,
    displays: Vec<DisplayInfo>,
}

fn rig() -> Rig {
    let serial = serial();
    aware();
    let cursor = CursorGuard::new();
    let displays = displays().expect("displays");
    let primary = displays.iter().find(|d| d.primary).expect("a primary display").clone();
    let win = TestWindow::open((primary.bounds.x + 200, primary.bounds.y + 200), (640, 400));
    let mut sink = open_sink();
    sink.displays_changed(&displays);
    Rig { _serial: serial, _cursor: cursor, win, sink, displays }
}

impl Rig {
    fn apply(&mut self, e: InputEvent) {
        self.win.guard();
        self.sink.apply(&e).expect("apply");
    }

    fn tap(&mut self, usage: u16, mods: u8) {
        self.apply(InputEvent::Key { usage_page: 7, usage, down: true, modifiers: mods, locks: self.locks() });
        self.apply(InputEvent::Key { usage_page: 7, usage, down: false, modifiers: mods, locks: self.locks() });
    }

    fn locks(&self) -> u8 {
        use swapdesk_input_win::sink::Injector;
        self.sink.injector().lock_state().unwrap_or(0)
    }

    fn key(&mut self, usage: u16, down: bool, mods: u8) {
        let locks = self.locks();
        self.apply(InputEvent::Key { usage_page: 7, usage, down, modifiers: mods, locks });
    }

    /// A point of the window's client area as a wire point on the primary display, and the pixel it resolves to.
    fn point_in_window(&self, client: (i32, i32)) -> ((u16, u16), (i32, i32)) {
        let d = self.displays.iter().find(|d| d.primary).unwrap();
        let px = (self.win.origin.0 + client.0, self.win.origin.1 + client.1);
        let wire = |p: i32, origin: i32, size: u32| -> u16 {
            let v = f64::from(p - origin) * 65535.0 / f64::from(size - 1);
            v.round() as u16
        };
        ((wire(px.0, d.bounds.x, d.bounds.width), wire(px.1, d.bounds.y, d.bounds.height)), px)
    }
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn unicode_mode_types_the_exact_text() {
    let mut r = rig();
    let text = "Ação ç ~ ^ ´ `";
    r.apply(InputEvent::Text(text.to_owned()));
    let evs = r.win.wait_for(Duration::from_secs(3), |e| chars(e).chars().count() >= text.chars().count());
    assert_eq!(chars(&evs), text, "events: {evs:?}");
    // No dead-key composition happened: no dead characters reached the window.
    assert!(!evs.iter().any(|e| matches!(e, Ev::DeadChar(_))), "{evs:?}");
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn unicode_mode_sends_characters_outside_the_bmp_as_surrogate_pairs() {
    let mut r = rig();
    let text = "a\u{1F600}b\u{10348}";
    r.apply(InputEvent::Text(text.to_owned()));
    let evs = r.win.wait_for(Duration::from_secs(3), |e| chars(e).chars().count() >= 4);
    assert_eq!(chars(&evs), text, "events: {evs:?}");
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn physical_mode_delivers_the_expected_scan_codes() {
    let mut r = rig();
    // (usage, expected scan, expected extended flag)
    let keys: [(u16, u8, bool); 14] = [
        (0x04, 0x1E, false), // A
        (0x33, 0x27, false), // ABNT2 c-cedilla position
        (0x87, 0x73, false), // ABNT2 / ? (International1)
        (0x64, 0x56, false), // \ | near left Shift
        (0x85, 0x7E, false), // ABNT2 keypad comma
        (0x2C, 0x39, false), // Space
        (0x2B, 0x0F, false), // Tab
        (0x52, 0x48, true),  // Up arrow
        (0x60, 0x48, false), // keypad 8
        (0x58, 0x1C, true),  // keypad Enter
        (0xE4, 0x1D, true),  // right Ctrl
        (0x4A, 0x47, true),  // Home
        (0x4F, 0x4D, true),  // Right arrow
        (0x28, 0x1C, false), // Enter
    ];
    for (usage, ..) in keys {
        r.tap(usage, 0);
    }
    let evs = r.win.wait_for(Duration::from_secs(3), |e| key_downs(e).len() >= keys.len());
    let got = key_downs(&evs);
    let want: Vec<(u8, bool)> = keys.iter().map(|k| (k.1, k.2)).collect();
    assert_eq!(got, want, "events: {evs:?}");
    let ups = evs.iter().filter(|e| matches!(e, Ev::KeyUp { .. })).count();
    assert_eq!(ups, keys.len(), "every key went up: {evs:?}");
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn physical_mode_on_abnt2_types_ccedilla_tilde_composition_and_altgr_without_a_stray_ctrl() {
    let mut r = rig();
    let loaded_here = r.win.use_layout(w!("00010416"));
    r.win.settle(Duration::from_millis(50));
    // "Ação": Shift+A, c-cedilla key, tilde (dead) + A, O.
    r.key(0xE1, true, modifier::LSHIFT);
    r.tap(0x04, modifier::LSHIFT);
    r.key(0xE1, false, 0);
    r.tap(0x33, 0);
    r.tap(0x34, 0);
    r.tap(0x04, 0);
    r.tap(0x12, 0);
    let evs = r.win.wait_for(Duration::from_secs(3), |e| chars(e).chars().count() >= 4);
    assert_eq!(chars(&evs), "A\u{e7}\u{e3}o", "events: {evs:?}");

    // AltGr + Q is "/" on ABNT2; the sink injects one extended right Alt and no Ctrl of its own.
    r.win.settle(Duration::from_millis(50));
    r.key(0xE6, true, modifier::RALT);
    r.tap(0x14, modifier::RALT);
    r.key(0xE6, false, 0);
    let evs = r.win.wait_for(Duration::from_secs(3), |e| chars(e).contains('/'));
    assert_eq!(chars(&evs), "/", "events: {evs:?}");
    let ctrl_downs = evs.iter().filter(|e| matches!(e, Ev::KeyDown { vk: 0x11 | 0xA2 | 0xA3, .. })).count();
    println!("AltGr events on ABNT2 (loaded here: {loaded_here}): {evs:?}");
    // Windows adds its own left Ctrl for AltGr (once); a Ctrl from the sink would make it two.
    assert_eq!(ctrl_downs, 1, "events: {evs:?}");
    assert!(r.sink.held_keys().is_empty());
}

impl Rig {
    /// Injects the pointer record until the cursor is seen exactly on `expect`. A real mouse or touchpad moved by a hand
    /// right after the injection would otherwise make the check flaky; the cursor must be seen exactly there at least
    /// once in a few tries, and the last reading is reported when it never is.
    fn place(&mut self, display: u16, x: u16, y: u16, expect: (i32, i32)) -> Vec<Ev> {
        let mut all = Vec::new();
        let mut last = (0, 0);
        for _ in 0..8 {
            self.apply(InputEvent::Pointer { display, x, y });
            // Poll right away: the injected move is applied within a millisecond or two.
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(30) {
                last = cursor();
                if last == expect {
                    all.extend(self.win.settle(Duration::from_millis(5)));
                    return all;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            all.extend(self.win.settle(Duration::from_millis(5)));
        }
        panic!("display {display} wire ({x},{y}): the cursor never landed on {expect:?} (last reading {last:?})");
    }
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn pointer_lands_on_the_expected_client_point_and_on_every_display_corner() {
    let mut r = rig();
    // Every display: its two corners resolve to the exact pixels.
    let list = r.displays.clone();
    println!("displays on this PC: {}", list.len());
    for d in &list {
        println!("  display {}: {:?} scale {}", d.id.0, d.bounds, d.scale_milli);
        for (x, y, ex, ey) in [
            (0u16, 0u16, d.bounds.x, d.bounds.y),
            (65535, 65535, d.bounds.x + d.bounds.width as i32 - 1, d.bounds.y + d.bounds.height as i32 - 1),
        ] {
            r.place(d.id.0, x, y, (ex, ey));
        }
    }
    // The window point: the client coordinates the window receives are the expected ones.
    let primary_id = r.displays.iter().find(|d| d.primary).unwrap().id.0;
    for client in [(100, 60), (320, 200), (5, 5), (630, 390)] {
        let (wire, px) = r.point_in_window(client);
        let mut seen = false;
        for _ in 0..8 {
            // Start away from the target so a move message is certain.
            r.apply(InputEvent::Pointer { display: primary_id, x: 0, y: 0 });
            r.win.settle(Duration::from_millis(20));
            r.apply(InputEvent::Pointer { display: primary_id, x: wire.0, y: wire.1 });
            let evs = r.win.wait_for(Duration::from_millis(400), |e| {
                e.iter().any(|m| matches!(m, Ev::Move { x, y } if (*x, *y) == client))
            });
            if evs.iter().any(|m| matches!(m, Ev::Move { x, y } if (*x, *y) == client)) {
                seen = true;
                break;
            }
        }
        assert!(seen, "the window never received a move at client {client:?}");
        r.place(primary_id, wire.0, wire.1, px);
        assert_eq!(r.win.to_client(px.0, px.1), client);
    }
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn buttons_and_wheels_reach_the_window_at_the_pointer() {
    let mut r = rig();
    let client = (200, 150);
    let (wire, _) = r.point_in_window(client);
    let primary_id = r.displays.iter().find(|d| d.primary).unwrap().id.0;
    // A hand on the real mouse moves the cursor between the move and the click now and then: the click must land exactly
    // on the client point at least once in a few tries (the pointer record and the button records go back to back).
    for (button, id) in
        [(MouseButton::Left, 1u8), (MouseButton::Right, 2), (MouseButton::Back, 4), (MouseButton::Forward, 5)]
    {
        let mut last: Vec<Ev> = Vec::new();
        let mut ok = false;
        for _ in 0..10 {
            r.apply(InputEvent::Pointer { display: primary_id, x: wire.0, y: wire.1 });
            r.apply(InputEvent::Button { button, down: true });
            r.apply(InputEvent::Button { button, down: false });
            last = r.win.wait_for(Duration::from_millis(300), |e| {
                e.iter().any(|m| matches!(m, Ev::ButtonUp { button: b, .. } if *b == id))
            });
            if last.contains(&Ev::ButtonDown { button: id, x: client.0, y: client.1 })
                && last.contains(&Ev::ButtonUp { button: id, x: client.0, y: client.1 })
            {
                ok = true;
                break;
            }
        }
        assert!(ok, "button {id} never clicked exactly at {client:?}; last events {last:?}");
    }
    // One record at a time: Windows adds up wheel messages that wait together in the queue (+120 then -240 arrives
    // as -120), which is the receiver's coalescing and not a loss in the sink. A wheel turned by a hand on the real
    // mouse may add events of its own, so each expected message must be among those received.
    for (dx, dy, expect) in [
        (0i16, 120i16, vec![Ev::Wheel { delta: 120 }]),
        (0, -240, vec![Ev::Wheel { delta: -240 }]),
        (120, 0, vec![Ev::HWheel { delta: 120 }]),
        (-60, 30, vec![Ev::Wheel { delta: 30 }, Ev::HWheel { delta: -60 }]),
    ] {
        r.apply(InputEvent::Wheel { dx, dy });
        let evs = r.win.wait_for(Duration::from_secs(2), |e| expect.iter().all(|x| e.contains(x)));
        for x in &expect {
            assert!(evs.contains(x), "record dx={dx} dy={dy}: {x:?} missing in {evs:?}");
        }
    }
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn release_all_lifts_the_held_key_and_button_on_the_real_desktop() {
    let mut r = rig();
    let client = (300, 100);
    let (wire, _) = r.point_in_window(client);
    let primary_id = r.displays.iter().find(|d| d.primary).unwrap().id.0;
    r.apply(InputEvent::Pointer { display: primary_id, x: wire.0, y: wire.1 });
    r.key(0x04, true, 0);
    r.apply(InputEvent::Button { button: MouseButton::Left, down: true });
    r.win.settle(Duration::from_millis(60));
    assert_eq!(r.sink.held_keys(), vec![(7, 0x04)]);
    assert_eq!(r.sink.held_buttons(), vec![1]);
    r.apply(InputEvent::ReleaseAll);
    let evs = r.win.wait_for(Duration::from_secs(2), |e| {
        e.iter().any(|m| matches!(m, Ev::KeyUp { scan: 0x1E, .. }))
            && e.iter().any(|m| matches!(m, Ev::ButtonUp { button: 1, .. }))
    });
    assert!(evs.iter().any(|m| matches!(m, Ev::KeyUp { scan: 0x1E, .. })), "{evs:?}");
    assert!(evs.iter().any(|m| matches!(m, Ev::ButtonUp { button: 1, .. })), "{evs:?}");
    assert!(r.sink.held_keys().is_empty() && r.sink.held_buttons().is_empty());
}

#[test]
#[ignore = "interactive: injects real keyboard/mouse input; run on an idle PC with --run-ignored only"]
fn lock_state_follows_a_real_caps_lock_toggle_and_is_restored() {
    let mut r = rig();
    let before = r.locks();
    r.key(0x39, true, 0);
    r.key(0x39, false, 0);
    r.win.settle(Duration::from_millis(100));
    let mid = r.locks();
    // Put it back whatever happened.
    r.key(0x39, true, 0);
    r.key(0x39, false, 0);
    r.win.settle(Duration::from_millis(100));
    let after = r.locks();
    assert_eq!(mid ^ before, 1, "Caps Lock toggled (before {before:#05b}, mid {mid:#05b})");
    assert_eq!(after, before, "restored");
}
