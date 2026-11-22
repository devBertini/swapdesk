//! The net under the picture, and the pipeline's own trace (review of 22/11/2026).
//!
//! A viewer whose picture stopped while its session went on left no trace of why: every failure of the decoder or of
//! the renderer was kept only in memory. And some failures healed nothing: a renderer that could not be built, a
//! present that kept failing, a decoder that took frames and gave no pictures.
//!
//! * [`StallWatch`] tells a still screen from a stalled pipeline by what the viewer itself sees: frames keep
//!   arriving and no picture comes out. A host whose screen does not change sends no frames, so stillness never
//!   looks like a stall. When it fires the platform's decode thread builds its decoder (and what draws) again and
//!   asks for a keyframe.
//! * [`Notes`] hands each failure to the application's trace, a few lines for each kind, never a flood.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Frames arriving for this long with no picture is a stall.
pub const STALL_AFTER: Duration = Duration::from_secs(3);
/// The longest wait between two rebuilds while the stall goes on.
const STALL_MAX: Duration = Duration::from_secs(30);

/// Watches that the frames which arrive turn into pictures.
#[derive(Debug)]
pub struct StallWatch {
    /// When the first frame arrived that no picture has followed yet.
    since: Option<Instant>,
    /// How long frames may arrive with no picture before the next rebuild.
    wait: Duration,
}

impl Default for StallWatch {
    fn default() -> Self {
        Self { since: None, wait: STALL_AFTER }
    }
}

impl StallWatch {
    /// A frame arrived for the decoder.
    pub fn frame(&mut self, now: Instant) {
        self.since.get_or_insert(now);
    }

    /// A picture came out: it reached the screen (or the thread that puts it there).
    pub fn picture(&mut self) {
        self.since = None;
        self.wait = STALL_AFTER;
    }

    /// Nothing to watch any more: the window went, or another stream starts.
    pub fn clear(&mut self) {
        self.since = None;
        self.wait = STALL_AFTER;
    }

    /// True once when frames have arrived for the wait with no picture: the caller builds its decoder again and
    /// asks for a keyframe. While that does not bring a picture the wait doubles, up to half a minute, so a stream
    /// that cannot be shown at all costs a rebuild now and then, not a loop.
    pub fn stalled(&mut self, now: Instant) -> bool {
        match self.since {
            Some(t) if now.saturating_duration_since(t) >= self.wait => {
                self.since = None;
                self.wait = (self.wait * 2).min(STALL_MAX);
                true
            }
            _ => false,
        }
    }
}

/// Where a line of the pipeline's trace goes (the application's trace log).
pub type NoteHook = Arc<dyn Fn(&str) + Send + Sync>;

/// The same kind of line is written at most this often; the ones in between are counted.
const NOTE_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct NoteState {
    hook: Option<NoteHook>,
    /// The kind last written, when, and how many of it were held back since.
    last: Option<(String, Instant, u32)>,
}

/// The pipeline's failures on their way to the trace. A failure that repeats with every frame is one line and a
/// count, not sixty lines a second.
#[derive(Default)]
pub struct Notes(Mutex<NoteState>);

impl Notes {
    pub fn set_hook(&self, hook: NoteHook) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).hook = Some(hook);
    }

    /// One failure of kind `what` (a short fixed word: "decode", "present", "stalled"), with its detail.
    pub fn note(&self, what: &str, detail: &str) {
        self.note_at(Instant::now(), what, detail);
    }

    fn note_at(&self, now: Instant, what: &str, detail: &str) {
        let mut st = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(hook) = st.hook.clone() else { return };
        let held = match &mut st.last {
            Some((kind, at, held)) if kind == what && now.saturating_duration_since(*at) < NOTE_EVERY => {
                *held += 1;
                return;
            }
            Some((kind, _, held)) if kind == what => *held,
            _ => 0,
        };
        st.last = Some((what.to_owned(), now, 0));
        drop(st);
        if held > 0 {
            hook(&format!("viewer {what}: {detail} ({held} more like it since the last line)"));
        } else {
            hook(&format!("viewer {what}: {detail}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_screen_is_never_a_stall_and_frames_without_pictures_are() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut w = StallWatch::default();
        // Nothing arrives (the host's screen does not change): never a stall.
        assert!(!w.stalled(at(60_000)));
        // Frames arrive and pictures follow: never a stall.
        for ms in (0..10_000).step_by(33) {
            w.frame(at(ms));
            assert!(!w.stalled(at(ms)));
            w.picture();
        }
        // Frames arrive and nothing comes out: a stall after three seconds, told once.
        w.frame(at(10_000));
        w.frame(at(11_000));
        assert!(!w.stalled(at(12_900)));
        assert!(w.stalled(at(13_000)));
        assert!(!w.stalled(at(13_001)), "told once");
        // It goes on: the next one comes after twice the wait, then four times, never more than half a minute.
        w.frame(at(13_100));
        assert!(!w.stalled(at(18_900)));
        assert!(w.stalled(at(19_100)));
        w.frame(at(19_200));
        assert!(!w.stalled(at(31_000)));
        assert!(w.stalled(at(31_200)));
        for _ in 0..6 {
            let from = at(100_000);
            w.frame(from);
            assert!(w.stalled(from + STALL_MAX));
        }
        // A picture puts the wait back.
        w.picture();
        w.frame(at(200_000));
        assert!(w.stalled(at(203_000)));
        // And a window that went has nothing to watch.
        w.frame(at(300_000));
        w.clear();
        assert!(!w.stalled(at(400_000)));
    }

    #[test]
    fn a_failure_that_repeats_is_one_line_and_a_count() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let notes = Notes::default();
        notes.note("decode", "before a hook is set nothing is written");
        let l = Arc::clone(&lines);
        notes.set_hook(Arc::new(move |s| l.lock().unwrap().push(s.to_owned())));
        let t0 = Instant::now();
        for i in 0..300u64 {
            notes.note_at(t0 + Duration::from_millis(i * 16), "decode", "status -12909");
        }
        notes.note_at(t0 + Duration::from_secs(5), "stalled", "the decoder is built again");
        notes.note_at(t0 + Duration::from_secs(6), "decode", "status -12909");
        notes.note_at(t0 + Duration::from_secs(7), "decode", "status -12909");
        notes.note_at(t0 + Duration::from_secs(17), "decode", "status -12909");
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[0], "viewer decode: status -12909");
        assert_eq!(lines[1], "viewer stalled: the decoder is built again");
        // Another kind in between starts the count again; a kind that repeats says how many it held back.
        assert_eq!(lines[2], "viewer decode: status -12909");
        assert_eq!(lines[3], "viewer decode: status -12909 (1 more like it since the last line)");
    }
}
