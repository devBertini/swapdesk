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

/// Keeps one `Pointer` record per run of consecutive ones (the newest), so a fast mouse does not flood the pipe;
/// every other record keeps its place and order.
pub fn coalesce(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for e in events {
        if matches!(e, InputEvent::Pointer { .. }) && matches!(out.last(), Some(InputEvent::Pointer { .. })) {
            out.pop();
        }
        out.push(e);
    }
    out
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
