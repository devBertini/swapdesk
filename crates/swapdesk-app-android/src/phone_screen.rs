//! The phone's screen: the shell that shows the state (`swapdesk_shell_android::PhoneShell`), held on the thread that
//! calls in, which on Android is the main thread of the app (every call of `Shell` comes from the view or from the
//! main Looper).
//!
//! Plain Rust: the desktop gate builds and tests it; `jni_entry` only carries values in and out.

use std::cell::RefCell;
use std::sync::Arc;

use swapdesk_shell_android::canvas_ops::FRAME;
use swapdesk_shell_android::{FrameCost, Metrics, PhoneShell};
use swapdesk_ui::{UiBackend, UiState};

/// The densities a frame is painted at: Android's go from 0.75 to 4, and a development view that fits a desktop
/// window into the phone's screen may go below.
const DENSITY: std::ops::RangeInclusive<f32> = 0.25..=8.0;
/// A side longer than this, in device pixels, is no screen.
const SIDE_MAX: i32 = 16_384;

struct Screen {
    shell: Option<PhoneShell>,
    /// The shell is the app's own, over its service; not a development mock.
    live: bool,
    /// The monospaced face's ascent and descent as Java read them; Menlo's until then.
    mono: (f32, f32),
}

thread_local! {
    static SCREEN: RefCell<Screen> =
        const { RefCell::new(Screen { shell: None, live: false, mono: (0.93, 0.24) }) };
}

/// The ascent and descent of the phone's monospaced face, as fractions of its size.
pub fn set_mono_metrics(ascent: f32, descent: f32) {
    if ascent.is_finite() && descent.is_finite() && ascent > 0.0 && descent >= 0.0 && ascent + descent < 4.0 {
        SCREEN.with_borrow_mut(|s| s.mono = (ascent, descent));
    }
}

/// Shows `state` from the next frame on, with `backend` behind it, and sends what a shell sends when it starts.
/// `live` says it is the app's own state over its service ([`is_live`]).
pub fn show(state: UiState, backend: Arc<dyn UiBackend>, live: bool) {
    let mut shell = PhoneShell::new(state, backend);
    shell.start();
    SCREEN.with_borrow_mut(|s| {
        s.shell = Some(shell);
        s.live = live;
    });
}

/// Whether the screen shows the app's own state over its service. Android makes a new activity for the same
/// process (the person came back to the app; a change the manifest does not take itself): the state it finds is
/// the one it left, with the pairing or the page that was on the screen.
pub fn is_live() -> bool {
    SCREEN.with(|s| s.try_borrow().is_ok_and(|s| s.live && s.shell.is_some()))
}

/// Runs `f` on the shell, when there is one. A call that arrives while another is under way on this thread (the
/// state calling back into the view) is dropped.
pub fn with_shell<R>(f: impl FnOnce(&mut PhoneShell) -> R) -> Option<R> {
    SCREEN.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.shell.as_mut().map(f)))
}

/// One pass of a frame for a view of `width_px` by `height_px` at `density` device pixels per DIP: the frame to
/// replay, or the list of text to measure first (`swapdesk_shell_android::canvas_ops`). With nothing to show, or a
/// size that is no screen, the frame is empty.
pub fn paint(width_px: i32, height_px: i32, density: f32) -> Vec<u8> {
    let sized = (1..=SIDE_MAX).contains(&width_px) && (1..=SIDE_MAX).contains(&height_px) && DENSITY.contains(&density);
    if !sized {
        return vec![FRAME];
    }
    SCREEN.with_borrow_mut(|s| {
        let metrics = Metrics::new(s.mono.0, s.mono.1);
        match s.shell.as_mut() {
            Some(shell) => shell.paint(metrics, width_px as f32, height_px as f32, density),
            None => vec![FRAME],
        }
    })
}

/// Java's answer to the last list of text to measure.
pub fn measured(answer: &[u8]) -> Result<usize, &'static str> {
    with_shell(|shell| shell.measured(answer)).unwrap_or(Err("no screen to measure for"))
}

/// What the last frame cost.
pub fn cost() -> FrameCost {
    with_shell(|shell| shell.cost()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use swapdesk_shell_android::canvas_ops::{NEEDS, Op, decode};
    use swapdesk_shell_android::text_measure::{Measured, Run, decode_needs, encode_measured};
    use swapdesk_ui::mock::Scenario;
    use swapdesk_ui::{Command, ControlId, Nav, Ticket, UiEvent};

    use super::*;

    /// A backend that answers nothing.
    struct Silent;

    impl UiBackend for Silent {
        fn send(&self, _: Command) -> Ticket {
            Ticket(1)
        }
        fn drain(&self) -> Vec<UiEvent> {
            Vec::new()
        }
    }

    /// What the view does: paint, measure what is asked, paint again, until a frame.
    fn frame(width_px: i32, height_px: i32, density: f32) -> Vec<Op> {
        for _ in 0..8 {
            let bytes = paint(width_px, height_px, density);
            if bytes[0] == FRAME {
                return decode(&bytes).expect("the frame reads back");
            }
            assert_eq!(bytes[0], NEEDS);
            let answer: Vec<Measured> = decode_needs(&bytes)
                .expect("the list reads back")
                .iter()
                .map(|k| Measured {
                    lines: vec![vec![Run {
                        bold: false,
                        width: k.text.chars().count() as f32 * f32::from_bits(k.size_px) * 0.5,
                        text: k.text.clone(),
                    }]],
                })
                .collect();
            measured(&encode_measured(&answer)).expect("the measures are taken");
        }
        panic!("no frame");
    }

    #[test]
    fn the_screen_is_empty_until_there_is_a_state_and_then_paints_it_at_the_views_size() {
        assert_eq!(paint(1080, 2400, 2.625), vec![FRAME], "nothing to show yet");
        assert!(with_shell(|_| ()).is_none() && measured(&[]).is_err() && cost() == FrameCost::default());
        assert!(!is_live());
        show(Scenario::Dispositivos.prepared_state(), Arc::new(Silent), false);
        assert!(!is_live(), "a mock is not the app's own state");
        set_mono_metrics(0.9, 0.3);
        set_mono_metrics(f32::NAN, 0.3);

        for bad in [(0, 2400, 2.625), (1080, -1, 2.625), (1080, 2400, 0.0), (1080, 2400, f32::NAN), (99_999, 10, 1.0)] {
            assert_eq!(paint(bad.0, bad.1, bad.2), vec![FRAME], "{bad:?} is no screen");
        }

        let ops = frame(2400, 1080, 1.5);
        let texts = ops.iter().filter(|o| matches!(o, Op::Text { .. })).count();
        assert!(texts > 10 && cost().lists >= 1 && cost().labels >= texts / 2, "{texts} runs, {:?}", cost());
        // 1600 by 720 DIPs is a desktop's size: the sidebar is there, with its two navigation rows.
        let said = |ops: &[Op], what: &str| ops.iter().any(|o| matches!(o, Op::Text { text, .. } if text == what));
        assert!(said(&ops, "Início") && said(&ops, "Ajustes"), "the desktop layout on a large screen");
        // Nothing is drawn outside the view: every fill starts inside it.
        for op in &ops {
            if let Op::FillRect(r, _) = op {
                assert!(r[0] <= 2400.0 && r[1] <= 1080.0, "{op:?}");
            }
        }
        // The same view again costs no measure; another density measures anew.
        let _ = frame(2400, 1080, 1.5);
        assert_eq!(cost(), FrameCost::default());
        // A phone held upright is 411 DIPs wide: the compact layout, where a computer's page has no sidebar.
        let phone = frame(1080, 2400, 2.625);
        assert!(cost().lists >= 1);
        assert!(!said(&phone, "Início") && !said(&phone, "Ajustes") && said(&phone, "Controlar"), "the compact layout");

        // The shell is the one the view's calls reach: back leaves the computer's page.
        assert_eq!(
            with_shell(|shell| (shell.takes_back(), shell.back(), shell.state().nav())),
            Some((true, true, Nav::Home))
        );
        let home = frame(1080, 2400, 2.625);
        assert!(said(&home, "Swapdesk") && !said(&home, "Controlar"), "the first pane");
        assert!(
            with_shell(|shell| shell
                .output()
                .is_some_and(|o| o.widgets.iter().any(|w| w.id == ControlId::NavSettings)))
            .unwrap_or(false)
        );
        show(Scenario::Inicio.phone_state(), Arc::new(Silent), true);
        assert!(is_live());
    }
}
