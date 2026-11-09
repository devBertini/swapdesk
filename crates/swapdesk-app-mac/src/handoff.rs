//! The hand-over from the decode thread to the main thread (the renderer's layer edits belong to the main thread; the
//! decoder's thread rules keep it off it).
//!
//! The decode thread pushes decoded pictures and cursor updates; the main thread drains them. The queue holds at most one
//! picture (the newest: an older one that nobody drew is never worth drawing, so a busy main thread costs frames, not
//! latency) and a bounded list of cursor updates (applied in order, because a shape may ride on an earlier one). One wake
//! is outstanding at a time: while the main thread has not drained, further pushes only fill the queue.
//!
//! Generic over the picture type so the queue rules are tested on any host; the Mac instantiates it with
//! [`crate::main_side::MainFrame`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use swapdesk_platform::CursorUpdate;

/// Cursor updates kept waiting.
pub const MAX_CURSORS: usize = 256;

/// One cursor update for the renderer; `drawn` is whether a shape is known and the cursor visible (the local pointer may
/// then be hidden).
#[derive(Debug, Clone)]
pub struct CursorItem {
    pub update: CursorUpdate,
    pub drawn: bool,
}

struct Queue<F> {
    frame: Option<F>,
    cursors: VecDeque<CursorItem>,
    /// Pictures replaced by a newer one before the main thread drew them, since the last drain.
    replaced: u64,
}

/// What one drain took.
pub struct Drained<F> {
    pub frame: Option<F>,
    pub cursors: Vec<CursorItem>,
    pub replaced: u64,
}

pub struct Handoff<F> {
    queue: Mutex<Queue<F>>,
    pending: AtomicBool,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl<F> Handoff<F> {
    /// `wake` asks the main thread to call [`Handoff::drain`] (a block on the main dispatch queue in the app); it is
    /// called at most once between two drains.
    pub fn new(wake: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            queue: Mutex::new(Queue { frame: None, cursors: VecDeque::new(), replaced: 0 }),
            pending: AtomicBool::new(false),
            wake,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue<F>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        if !self.pending.swap(true, Ordering::SeqCst) {
            (self.wake)();
        }
    }

    /// The newest picture; replaces one that was not drawn yet.
    pub fn push_frame(&self, frame: F) {
        {
            let mut q = self.lock();
            if q.frame.replace(frame).is_some() {
                q.replaced += 1;
            }
        }
        self.notify();
    }

    pub fn push_cursor(&self, item: CursorItem) {
        {
            let mut q = self.lock();
            if q.cursors.len() >= MAX_CURSORS {
                q.cursors.pop_front();
            }
            q.cursors.push_back(item);
        }
        self.notify();
    }

    /// Wakes the main thread with nothing queued, so it applies a state change (the local pointer switch) at once.
    pub fn poke(&self) {
        self.notify();
    }

    /// Takes everything queued. The wake flag is cleared first, so a push that lands after the take wakes again.
    pub fn drain(&self) -> Drained<F> {
        self.pending.store(false, Ordering::SeqCst);
        let mut q = self.lock();
        Drained {
            frame: q.frame.take(),
            cursors: q.cursors.drain(..).collect(),
            replaced: std::mem::take(&mut q.replaced),
        }
    }

    /// Drops what is queued (the window went away) and allows a new wake.
    pub fn clear(&self) {
        drop(self.drain());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use swapdesk_platform::DisplayId;

    fn item(x: i32) -> CursorItem {
        CursorItem { update: CursorUpdate { shape: None, x, y: 0, visible: true, display: DisplayId(0) }, drawn: true }
    }

    fn rig() -> (Handoff<u32>, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wakes);
        (
            Handoff::new(Box::new(move || {
                w.fetch_add(1, Ordering::SeqCst);
            })),
            wakes,
        )
    }

    #[test]
    fn only_the_newest_picture_is_kept_and_one_wake_is_outstanding() {
        let (h, wakes) = rig();
        h.push_frame(1);
        h.push_frame(2);
        h.push_cursor(item(5));
        h.push_frame(3);
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "one wake until the main thread drains");
        let d = h.drain();
        assert_eq!((d.frame, d.replaced, d.cursors.len()), (Some(3), 2, 1));
        h.push_frame(4);
        assert_eq!(wakes.load(Ordering::SeqCst), 2, "a push after the drain wakes again");
        assert_eq!(h.drain().frame, Some(4));
        assert!(h.drain().frame.is_none());
    }

    #[test]
    fn cursor_updates_keep_their_order_and_are_bounded() {
        let (h, _) = rig();
        for i in 0..(MAX_CURSORS as i32 + 10) {
            h.push_cursor(item(i));
        }
        let d = h.drain();
        assert_eq!(d.cursors.len(), MAX_CURSORS);
        assert_eq!(d.cursors[0].update.x, 10, "the oldest went first");
        assert_eq!(d.cursors.last().map(|c| c.update.x), Some(MAX_CURSORS as i32 + 9));
    }

    #[test]
    fn clear_drops_the_queue_and_rearms_the_wake() {
        let (h, wakes) = rig();
        h.push_frame(1);
        h.clear();
        assert!(h.drain().frame.is_none());
        h.push_frame(2);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }
}
