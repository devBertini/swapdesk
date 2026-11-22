//! Whose a mouse event over the host banner is (`docs/SPEC.md` F-SESS-03): the banner belongs to the person at the
//! computer, so an event of a real device over it is the banner's, and an event the controller's input injected is
//! never the banner's and goes on to what lies under it. No system call here: the Windows shell feeds it from its
//! low-level mouse hook, the Mac shell from its event tap, and both do what the [`Verdict`] says.

/// The mark the Mac host's input sink puts in the `kCGEventSourceUserData` field of every event it posts
/// ("Swapdesk" in ASCII; `swapdesk-input-mac`'s `POSTED_MARK` is the same number, and a test of the Mac app keeps
/// the two equal): how the Mac's event tap knows an event came from the controller.
pub const MAC_POSTED_MARK: i64 = 0x5377_6170_6465_736B;

/// A mouse event as the banner cares about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Move,
    LeftDown,
    LeftUp,
}

/// What becomes of a mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not the banner's: delivered as if the banner were not there.
    Pass,
    /// A real pointer moves over the banner.
    Hover,
    /// The pointer is not over the banner any more.
    Leave,
    /// The real left button went down on the banner.
    Press,
    /// The real left button went up after a press on the banner; `inside` when it is still over it.
    Release { inside: bool },
}

impl Verdict {
    /// The event stops here: what lies under the banner does not get it.
    pub fn swallows(self) -> bool {
        matches!(self, Verdict::Press | Verdict::Release { .. })
    }
}

/// Whose a mouse event is: the banner's when it comes from a real device over the banner, never when it was injected.
#[derive(Debug, Default)]
pub struct Guard {
    /// The real pointer was over the banner at its last move.
    over: bool,
    /// The real left button went down on the banner and is still down.
    pressed: bool,
}

impl Guard {
    /// `injected`: the event did not come from a device (`LLMHF_INJECTED` or `LLMHF_LOWER_IL_INJECTED`).
    /// `inside`: its point lies in the banner's rectangle.
    pub fn on_event(&mut self, event: MouseEvent, injected: bool, inside: bool) -> Verdict {
        if injected {
            // The controller's pointer: never the banner's. It only takes the highlight away when it carries the
            // one pointer of the screen off the banner (the highlight shows on this screen alone).
            if event == MouseEvent::Move && self.over && !inside && !self.pressed {
                self.over = false;
                return Verdict::Leave;
            }
            return Verdict::Pass;
        }
        match event {
            MouseEvent::Move if inside => {
                self.over = true;
                Verdict::Hover
            }
            MouseEvent::Move if self.over => {
                self.over = false;
                Verdict::Leave
            }
            MouseEvent::Move => Verdict::Pass,
            MouseEvent::LeftDown if inside => {
                self.over = true;
                self.pressed = true;
                Verdict::Press
            }
            MouseEvent::LeftDown => Verdict::Pass,
            // Only the release of a press that began on the banner is the banner's: a drag that merely passes over
            // it ends where the person lets go.
            MouseEvent::LeftUp if self.pressed => {
                self.pressed = false;
                self.over = inside;
                Verdict::Release { inside }
            }
            MouseEvent::LeftUp => Verdict::Pass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MouseEvent::{LeftDown, LeftUp, Move};

    const REAL: bool = false;
    const INJECTED: bool = true;
    const INSIDE: bool = true;
    const OUTSIDE: bool = false;

    #[test]
    fn nothing_injected_is_ever_the_banners() {
        // The controller moves over the banner, clicks it, drags across it: every event passes.
        let mut g = Guard::default();
        for event in [Move, LeftDown, LeftUp, Move, LeftDown, Move, LeftUp] {
            assert_eq!(g.on_event(event, INJECTED, INSIDE), Verdict::Pass, "{event:?}");
        }
        // Also in the middle of what the person at the computer does: an injected release does not end their press.
        assert_eq!(g.on_event(LeftDown, REAL, INSIDE), Verdict::Press);
        assert_eq!(g.on_event(LeftUp, INJECTED, INSIDE), Verdict::Pass);
        assert_eq!(g.on_event(LeftDown, INJECTED, INSIDE), Verdict::Pass);
        assert_eq!(g.on_event(LeftUp, REAL, INSIDE), Verdict::Release { inside: true });
    }

    #[test]
    fn a_real_click_on_the_banner_is_the_banners_and_stops_there() {
        let mut g = Guard::default();
        assert_eq!(g.on_event(Move, REAL, INSIDE), Verdict::Hover);
        let press = g.on_event(LeftDown, REAL, INSIDE);
        let release = g.on_event(LeftUp, REAL, INSIDE);
        assert_eq!((press, release), (Verdict::Press, Verdict::Release { inside: true }));
        assert!(press.swallows() && release.swallows(), "what lies under the banner gets neither");
        assert!(!Verdict::Hover.swallows() && !Verdict::Leave.swallows(), "the pointer itself always moves");
    }

    #[test]
    fn a_press_that_leaves_the_banner_is_no_click_and_its_release_is_still_the_banners() {
        let mut g = Guard::default();
        assert_eq!(g.on_event(LeftDown, REAL, INSIDE), Verdict::Press);
        assert_eq!(g.on_event(Move, REAL, OUTSIDE), Verdict::Leave);
        // The window under the pointer never saw the press: it must not see a release alone.
        let release = g.on_event(LeftUp, REAL, OUTSIDE);
        assert_eq!(release, Verdict::Release { inside: false });
        assert!(release.swallows());
        assert_eq!(g.on_event(Move, REAL, OUTSIDE), Verdict::Pass, "nothing is left to tell");
    }

    #[test]
    fn real_events_away_from_the_banner_and_a_drag_that_passes_over_it_are_left_alone() {
        let mut g = Guard::default();
        assert_eq!(g.on_event(Move, REAL, OUTSIDE), Verdict::Pass);
        assert_eq!(g.on_event(LeftDown, REAL, OUTSIDE), Verdict::Pass);
        // The drag crosses the banner (it lights up) and ends on it: the release belongs to where the press was.
        assert_eq!(g.on_event(Move, REAL, INSIDE), Verdict::Hover);
        assert_eq!(g.on_event(LeftUp, REAL, INSIDE), Verdict::Pass);
    }

    #[test]
    fn the_controllers_pointer_takes_the_highlight_away_and_nothing_else() {
        let mut g = Guard::default();
        assert_eq!(g.on_event(Move, REAL, INSIDE), Verdict::Hover);
        // The one pointer of the screen is carried off the banner by the controller.
        assert_eq!(g.on_event(Move, INJECTED, OUTSIDE), Verdict::Leave);
        assert_eq!(g.on_event(Move, INJECTED, OUTSIDE), Verdict::Pass);
        // Carried back onto it: no highlight, the banner does not react to the controller.
        assert_eq!(g.on_event(Move, INJECTED, INSIDE), Verdict::Pass);
    }
}
