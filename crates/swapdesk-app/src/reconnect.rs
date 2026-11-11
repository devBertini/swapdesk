//! The interface's link to the service when the service is not there yet, or goes away (WAITING.md 4, row 23).
//!
//! * At launch the pipe is tried at once and then every second for [`Timing::window`] (30 s): an interface that the
//!   sign-in started before the service finds it when it comes up. Commands sent meanwhile wait and go out once the
//!   pipe is open; the interface shows its placeholders while they wait.
//! * A pipe that breaks while the interface runs (the service restarted, an update replaced it) gets the same window
//!   of retries. The requests that were out when it broke fail with `ServiceDown`, as before. Once the pipe is back the
//!   interface hears [`UiEvent::ServiceReconnected`]: it ends what the old service held and asks for its data again.
//! * When the window passes without a pipe, every waiting command and every later one is answered `ServiceDown`, the
//!   state the interface already shows ("O serviço do Swapdesk não está respondendo.").
//!
//! Each link numbers its own tickets; this backend gives the interface its own and translates the replies, so a
//! ticket the interface holds never changes when the link under it does.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_ui::{ApiCode, ApiError, Command, Ticket, UiBackend, UiEvent, UiWaker};

/// A link to the service, and how to tell that it broke.
pub struct Linked {
    pub backend: Arc<dyn UiBackend>,
    pub is_down: Box<dyn Fn() -> bool + Send + Sync>,
}

/// Opens a link; `None` while the service cannot be reached.
pub type Connector = Box<dyn Fn() -> Option<Linked> + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long the pipe is tried before the interface is told the service is down.
    pub window: Duration,
    /// The time between two tries.
    pub every: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { window: Duration::from_secs(30), every: Duration::from_secs(1) }
    }
}

enum Phase {
    /// No link: commands wait. `since` starts the window; `broke` is a link that was up before.
    Waiting {
        since: Instant,
        queue: Vec<(Ticket, Command)>,
        broke: bool,
    },
    Up(Linked),
    Down,
}

struct State {
    phase: Phase,
    /// The link's ticket of each command out, and the interface's.
    tickets: HashMap<Ticket, Ticket>,
}

struct Inner {
    state: Mutex<State>,
    events: Mutex<VecDeque<UiEvent>>,
    next: AtomicU64,
    waker: Arc<dyn UiWaker>,
    connector: Connector,
    timing: Timing,
    stop: AtomicBool,
}

/// The backend of the Windows interface: the service's pipe, opened and opened again (see the module).
pub struct Reconnecting {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Reconnecting {
    /// Tries the pipe once now, then keeps trying on a thread of its own as the module says.
    pub fn start(connector: Connector, waker: Arc<dyn UiWaker>, timing: Timing) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                phase: Phase::Waiting { since: Instant::now(), queue: Vec::new(), broke: false },
                tickets: HashMap::new(),
            }),
            events: Mutex::new(VecDeque::new()),
            next: AtomicU64::new(1),
            waker,
            connector,
            timing,
            stop: AtomicBool::new(false),
        });
        inner.step();
        let worker = Arc::clone(&inner);
        let _ = std::thread::Builder::new().name("swapdesk-ui-reconnect".into()).spawn(move || {
            while !worker.stop.load(Ordering::SeqCst) {
                std::thread::sleep(worker.timing.every);
                worker.step();
            }
        });
        Self { inner }
    }

    /// The pipe is open now.
    pub fn is_up(&self) -> bool {
        matches!(lock(&self.inner.state).phase, Phase::Up(_))
    }
}

impl Drop for Reconnecting {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }
}

impl Inner {
    fn push(&self, events: impl IntoIterator<Item = UiEvent>) {
        let mut q = lock(&self.events);
        let before = q.len();
        q.extend(events);
        let woke = q.len() > before;
        drop(q);
        if woke {
            self.waker.wake();
        }
    }

    fn down_reply(ticket: Ticket) -> UiEvent {
        UiEvent::Reply { ticket, result: Err(ApiError::new(ApiCode::ServiceDown)) }
    }

    /// The link's events with the interface's tickets; a reply to a ticket nobody holds is dropped.
    fn translate(tickets: &mut HashMap<Ticket, Ticket>, events: Vec<UiEvent>) -> Vec<UiEvent> {
        events
            .into_iter()
            .filter_map(|e| match e {
                UiEvent::Reply { ticket, result } => {
                    tickets.remove(&ticket).map(|ticket| UiEvent::Reply { ticket, result })
                }
                other => Some(other),
            })
            .collect()
    }

    /// One look: a waiting link is tried, an open one is checked, the window is minded.
    fn step(&self) {
        let mut out = Vec::new();
        // Lock order everywhere: the state, then the events.
        {
            let mut st = lock(&self.state);
            let State { phase, tickets } = &mut *st;
            match phase {
                Phase::Up(link) if (link.is_down)() => {
                    // What the broken link still holds (its `ServiceDown` replies) goes out before the retries.
                    out.extend(Self::translate(tickets, link.backend.drain()));
                    *phase = Phase::Waiting { since: Instant::now(), queue: Vec::new(), broke: true };
                }
                Phase::Waiting { since, queue, broke } => {
                    if let Some(link) = (self.connector)() {
                        for (outer, command) in queue.drain(..) {
                            tickets.insert(link.backend.send(command), outer);
                        }
                        if *broke {
                            out.push(UiEvent::ServiceReconnected);
                        }
                        *phase = Phase::Up(link);
                    } else if since.elapsed() >= self.timing.window {
                        out.extend(queue.drain(..).map(|(t, _)| Self::down_reply(t)));
                        *phase = Phase::Down;
                    }
                }
                Phase::Up(_) | Phase::Down => {}
            }
            // Pushed before the state is let go: `ServiceReconnected` comes before any reply of the new link.
            self.push(out);
        }
    }
}

impl UiBackend for Reconnecting {
    fn send(&self, command: Command) -> Ticket {
        let outer = Ticket(self.inner.next.fetch_add(1, Ordering::Relaxed));
        let mut st = lock(&self.inner.state);
        let State { phase, tickets } = &mut *st;
        match phase {
            Phase::Up(link) => {
                tickets.insert(link.backend.send(command), outer);
            }
            Phase::Waiting { queue, .. } => queue.push((outer, command)),
            Phase::Down => {
                drop(st);
                self.inner.push([Inner::down_reply(outer)]);
            }
        }
        outer
    }

    fn drain(&self) -> Vec<UiEvent> {
        // The state first, as `step` takes them: what it pushed and the link it opened are seen together.
        let mut st = lock(&self.inner.state);
        let mut events: Vec<UiEvent> = lock(&self.inner.events).drain(..).collect();
        let State { phase, tickets } = &mut *st;
        if let Phase::Up(link) = phase {
            events.extend(Inner::translate(tickets, link.backend.drain()));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use swapdesk_ui::Reply;

    struct Wake(AtomicUsize);
    impl UiWaker for Wake {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A service stand-in: answers every command `Done` with tickets of its own numbering (from 1000), and breaks on
    /// demand, failing what it still holds.
    struct FakeLink {
        next: AtomicU64,
        out: Mutex<Vec<UiEvent>>,
        down: Arc<AtomicBool>,
        sent: Mutex<Vec<Command>>,
    }

    impl UiBackend for FakeLink {
        fn send(&self, command: Command) -> Ticket {
            let t = Ticket(self.next.fetch_add(1, Ordering::SeqCst));
            lock(&self.sent).push(command);
            let result = if self.down.load(Ordering::SeqCst) {
                Err(ApiError::new(ApiCode::ServiceDown))
            } else {
                Ok(Reply::Done)
            };
            lock(&self.out).push(UiEvent::Reply { ticket: t, result });
            t
        }
        fn drain(&self) -> Vec<UiEvent> {
            std::mem::take(&mut *lock(&self.out))
        }
    }

    /// A link the service opened and the switch that breaks it.
    type Opened = (Arc<FakeLink>, Arc<AtomicBool>);

    /// A service that comes up when `up` is set; each link it opens is kept for the test to look at.
    struct Service {
        up: Arc<AtomicBool>,
        links: Arc<Mutex<Vec<Opened>>>,
    }

    impl Service {
        fn new(up: bool) -> Self {
            Self { up: Arc::new(AtomicBool::new(up)), links: Arc::default() }
        }

        fn connector(&self) -> Connector {
            let (up, links) = (Arc::clone(&self.up), Arc::clone(&self.links));
            Box::new(move || {
                if !up.load(Ordering::SeqCst) {
                    return None;
                }
                let down = Arc::new(AtomicBool::new(false));
                let link = Arc::new(FakeLink {
                    next: AtomicU64::new(1000),
                    out: Mutex::default(),
                    down: Arc::clone(&down),
                    sent: Mutex::default(),
                });
                lock(&links).push((Arc::clone(&link), Arc::clone(&down)));
                let flag = Arc::clone(&down);
                Some(Linked { backend: link, is_down: Box::new(move || flag.load(Ordering::SeqCst)) })
            })
        }

        /// The current link breaks (the service process ended).
        fn crash(&self) {
            self.up.store(false, Ordering::SeqCst);
            if let Some((_, down)) = lock(&self.links).last() {
                down.store(true, Ordering::SeqCst);
            }
        }
    }

    fn fast() -> Timing {
        Timing { window: Duration::from_millis(400), every: Duration::from_millis(20) }
    }

    fn wait_events(b: &Reconnecting, want: usize) -> Vec<UiEvent> {
        let end = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < want && Instant::now() < end {
            got.extend(b.drain());
            std::thread::sleep(Duration::from_millis(5));
        }
        got
    }

    fn reply_of(e: &UiEvent) -> (Ticket, Result<(), ApiCode>) {
        match e {
            UiEvent::Reply { ticket, result } => (*ticket, result.as_ref().map(|_| ()).map_err(|e| e.code)),
            other => panic!("not a reply: {other:?}"),
        }
    }

    #[test]
    fn commands_sent_before_the_service_is_up_wait_and_go_out_when_it_comes() {
        let service = Service::new(false);
        let b = Reconnecting::start(service.connector(), Arc::new(Wake(AtomicUsize::new(0))), fast());
        let a = b.send(Command::ListDevices);
        let c = b.send(Command::GetSettings);
        std::thread::sleep(Duration::from_millis(60));
        assert!(b.drain().is_empty(), "nothing is answered while the service is not there yet");
        service.up.store(true, Ordering::SeqCst);
        let got = wait_events(&b, 2);
        let replies: Vec<_> = got.iter().map(reply_of).collect();
        assert_eq!(replies, vec![(a, Ok(())), (c, Ok(()))], "the interface's own tickets, in order");
        assert!(b.is_up());
        let link = Arc::clone(&lock(&service.links)[0].0);
        assert_eq!(*lock(&link.sent), vec![Command::ListDevices, Command::GetSettings]);
        assert!(!got.iter().any(|e| matches!(e, UiEvent::ServiceReconnected)), "a first link is no reconnect");
    }

    #[test]
    fn a_service_that_never_comes_answers_service_down_after_the_window() {
        let service = Service::new(false);
        let b = Reconnecting::start(service.connector(), Arc::new(Wake(AtomicUsize::new(0))), fast());
        let t = b.send(Command::ListDevices);
        let got = wait_events(&b, 1);
        assert_eq!(got.iter().map(reply_of).collect::<Vec<_>>(), vec![(t, Err(ApiCode::ServiceDown))]);
        // From then on every command is answered at once.
        let u = b.send(Command::GetSettings);
        assert_eq!(b.drain().iter().map(reply_of).collect::<Vec<_>>(), vec![(u, Err(ApiCode::ServiceDown))]);
    }

    #[test]
    fn a_link_that_breaks_is_opened_again_and_the_interface_is_told_to_ask_again() {
        let service = Service::new(true);
        let b = Reconnecting::start(service.connector(), Arc::new(Wake(AtomicUsize::new(0))), fast());
        assert!(b.is_up(), "a service that is there is found at once");
        let first = b.send(Command::ListDevices);
        assert_eq!(b.drain().iter().map(reply_of).collect::<Vec<_>>(), vec![(first, Ok(()))]);

        service.crash();
        let end = Instant::now() + Duration::from_secs(5);
        while b.is_up() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!b.is_up(), "the break is seen");
        // While the service restarts, new commands wait; then they go to the new link.
        let waiting = b.send(Command::GetNetwork);
        std::thread::sleep(Duration::from_millis(60));
        assert!(b.drain().is_empty(), "it waits, it is not refused");
        service.up.store(true, Ordering::SeqCst);
        let got = wait_events(&b, 2);
        assert!(matches!(got[0], UiEvent::ServiceReconnected), "{got:?}");
        assert_eq!(reply_of(&got[1]), (waiting, Ok(())));
        assert_eq!(lock(&service.links).len(), 2, "a second link");
    }
}
