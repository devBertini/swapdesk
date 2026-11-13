//! What the viewer pipeline says to the service (`ViewerInput`, `ViewerKeyframe`, `ViewerReport`) and the input thread
//! that batches the records of the window and the grabber into `ViewerInput` messages.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_proto::{InputEvent, KeyframeRequest, ReceiverReport};
use swapdesk_ui::service_backend::ServiceBackend;

/// Input records waiting for the input thread.
pub const INPUT_QUEUE: usize = 2048;
/// The input thread gathers records for this long before it writes a batch (moves are coalesced within it).
pub const INPUT_WINDOW: Duration = Duration::from_millis(2);
pub const INPUT_BATCH: usize = 256;

/// Where the pipeline sends what the viewer says to the service (`ViewerInput`, `ViewerKeyframe`, `ViewerReport`).
pub trait ViewerOut: Send + Sync {
    fn input(&self, session_id: u64, events: Vec<InputEvent>);
    fn keyframe(&self, session_id: u64, request: KeyframeRequest);
    fn report(&self, session_id: u64, report: ReceiverReport);
}

impl ViewerOut for ServiceBackend {
    fn input(&self, session_id: u64, events: Vec<InputEvent>) {
        self.viewer_input(session_id, events);
    }
    fn keyframe(&self, session_id: u64, request: KeyframeRequest) {
        self.viewer_keyframe(session_id, request);
    }
    fn report(&self, session_id: u64, report: ReceiverReport) {
        self.viewer_report(session_id, report);
    }
}

/// The shared slot of the output, set once the backend exists (the backend needs the pipeline as its sink first).
pub type OutSlot = Arc<Mutex<Option<Arc<dyn ViewerOut>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One wheel notch in wire units.
const NOTCH: i16 = 120;

/// Keeps one `Pointer` record per run of consecutive ones (the newest), so a fast mouse does not flood the pipe, and
/// adds up consecutive `Wheel` records (a trackpad's small steps) while each axis stays within one notch of one sign: an
/// app that counts a notch per wheel message whatever its size (Windows Terminal in VT mouse mode) must not lose the
/// notches a bigger sum would hide. Every other record keeps its place and order.
pub fn coalesce(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for e in events {
        if matches!(e, InputEvent::Pointer { .. }) && matches!(out.last(), Some(InputEvent::Pointer { .. })) {
            out.pop();
        }
        if let (InputEvent::Wheel { dx, dy }, Some(InputEvent::Wheel { dx: px, dy: py })) = (&e, out.last_mut())
            && let (Some(x), Some(y)) = (within_notch(*px, *dx), within_notch(*py, *dy))
        {
            (*px, *py) = (x, y);
            continue;
        }
        out.push(e);
    }
    out
}

/// The sum of two wheel deltas of one axis when it stays within one notch and neither turns the other around.
fn within_notch(a: i16, b: i16) -> Option<i16> {
    if a != 0 && b != 0 && (a > 0) != (b > 0) {
        return None;
    }
    let sum = a.checked_add(b)?;
    (sum.unsigned_abs() <= NOTCH.unsigned_abs()).then_some(sum)
}

/// The body of the input thread: takes records from `rx`, gathers a batch for [`INPUT_WINDOW`], and sends it to
/// `out` for the session in `session` (0: input is not allowed, the batch is dropped). Ends when `rx` closes or `stop`
/// is set.
pub fn input_loop(
    rx: &Receiver<InputEvent>,
    out: &Mutex<Option<Arc<dyn ViewerOut>>>,
    session: &AtomicU64,
    stop: &AtomicBool,
) {
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) if !stop.load(Ordering::SeqCst) => continue,
            Err(_) => return,
        };
        let mut batch = vec![first];
        let deadline = Instant::now() + INPUT_WINDOW;
        while batch.len() < INPUT_BATCH {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(e) => batch.push(e),
                Err(_) => break,
            }
        }
        let id = session.load(Ordering::SeqCst);
        if id == 0 {
            continue;
        }
        let target = lock(out).clone();
        if let Some(o) = target {
            o.input(id, coalesce(batch));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[derive(Default)]
    struct Rec(Mutex<Vec<(u64, Vec<InputEvent>)>>);
    impl ViewerOut for Rec {
        fn input(&self, session_id: u64, events: Vec<InputEvent>) {
            lock(&self.0).push((session_id, events));
        }
        fn keyframe(&self, _: u64, _: KeyframeRequest) {}
        fn report(&self, _: u64, _: ReceiverReport) {}
    }

    fn pointer(x: u16) -> InputEvent {
        InputEvent::Pointer { display: 0, x, y: 0 }
    }

    fn wheel(dx: i16, dy: i16) -> InputEvent {
        InputEvent::Wheel { dx, dy }
    }

    #[test]
    fn small_wheel_steps_add_up_within_one_notch_and_nothing_else_moves() {
        // A trackpad's steps in one batch: summed while the sum stays within a notch, then a new record.
        let got = coalesce(vec![wheel(0, -18), wheel(0, -15), wheel(0, -12), wheel(0, -90), wheel(0, -9)]);
        assert_eq!(got, vec![wheel(0, -45), wheel(0, -99)]);
        // Every unit is still there.
        let sum: i32 = got.iter().map(|e| if let InputEvent::Wheel { dy, .. } = e { i32::from(*dy) } else { 0 }).sum();
        assert_eq!(sum, -144);
        // A whole notch, a turn around, a big record or a record between: never merged.
        assert_eq!(coalesce(vec![wheel(0, 120), wheel(0, 3)]), vec![wheel(0, 120), wheel(0, 3)]);
        assert_eq!(coalesce(vec![wheel(0, 6), wheel(0, -3)]), vec![wheel(0, 6), wheel(0, -3)]);
        assert_eq!(coalesce(vec![wheel(-240, 0), wheel(0, 0)]), vec![wheel(-240, 0), wheel(0, 0)]);
        assert_eq!(
            coalesce(vec![wheel(0, 6), pointer(1), wheel(0, 6)]),
            vec![wheel(0, 6), pointer(1), wheel(0, 6)],
            "a wheel scrolls where the pointer is"
        );
        // Both axes of a diagonal add up together.
        assert_eq!(coalesce(vec![wheel(-12, 9), wheel(-12, 9)]), vec![wheel(-24, 18)]);
        assert_eq!(coalesce(vec![wheel(i16::MIN, 0), wheel(i16::MIN, 0)]).len(), 2);
    }

    #[test]
    fn the_input_loop_batches_coalesces_and_drops_without_a_session() {
        let rec = Arc::new(Rec::default());
        let out: Arc<Mutex<Option<Arc<dyn ViewerOut>>>> = Arc::new(Mutex::new(Some(rec.clone() as Arc<dyn ViewerOut>)));
        let session = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = sync_channel(64);
        let (o, s, st) = (Arc::clone(&out), Arc::clone(&session), Arc::clone(&stop));
        let t = std::thread::spawn(move || input_loop(&rx, &o, &s, &st));
        // No session: dropped.
        tx.send(pointer(1)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(lock(&rec.0).is_empty());
        // With a session: one batch, the run of moves reduced to the newest.
        session.store(9, Ordering::SeqCst);
        for e in [pointer(1), pointer(2), InputEvent::ReleaseAll, pointer(3)] {
            tx.send(e).unwrap();
        }
        let end = Instant::now() + Duration::from_secs(2);
        while lock(&rec.0).is_empty() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::SeqCst);
        drop(tx);
        t.join().unwrap();
        let got = lock(&rec.0).clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], (9, vec![pointer(2), InputEvent::ReleaseAll, pointer(3)]));
    }
}
