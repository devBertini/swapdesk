//! The backend of a UI that could not reach the service: every command fails with `ServiceDown` at once, which is the
//! state the UI already knows how to show.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use swapdesk_ui::{ApiCode, ApiError, Command, Reply, Ticket, UiBackend, UiEvent, UiWaker};

/// Every command fails with `ServiceDown` at once.
pub struct DownBackend {
    events: Mutex<VecDeque<UiEvent>>,
    next: AtomicU64,
    waker: Arc<dyn UiWaker>,
}

impl DownBackend {
    pub fn new(waker: Arc<dyn UiWaker>) -> Self {
        Self { events: Mutex::new(VecDeque::new()), next: AtomicU64::new(1), waker }
    }
}

impl UiBackend for DownBackend {
    fn send(&self, _command: Command) -> Ticket {
        let ticket = Ticket(self.next.fetch_add(1, Ordering::Relaxed));
        let result: Result<Reply, ApiError> = Err(ApiError::new(ApiCode::ServiceDown));
        self.events.lock().unwrap_or_else(PoisonError::into_inner).push_back(UiEvent::Reply { ticket, result });
        self.waker.wake();
        ticket
    }

    fn drain(&self) -> Vec<UiEvent> {
        self.events.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct CountingWaker(AtomicUsize);
    impl UiWaker for CountingWaker {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_down_backend_answers_every_command_with_service_down_and_wakes_the_ui() {
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let backend = DownBackend::new(waker.clone());
        let a = backend.send(Command::ListDevices);
        let b = backend.send(Command::GetSettings);
        assert_ne!(a, b);
        assert_eq!(waker.0.load(Ordering::SeqCst), 2);
        let events = backend.drain();
        assert_eq!(events.len(), 2);
        for (e, t) in events.iter().zip([a, b]) {
            match e {
                UiEvent::Reply { ticket, result: Err(err) } => {
                    assert_eq!(*ticket, t);
                    assert_eq!(err.code, ApiCode::ServiceDown);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(backend.drain().is_empty());
    }
}
