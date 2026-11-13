//! The interface list without the cost of reading it every time.
//!
//! `NetInterfaces::list` is an enumeration of the machine's adapters (on Windows `GetAdaptersAddresses`, the network
//! list manager and the firewall: COM calls that can take tens of milliseconds). The LAN guard asks for it on every
//! incoming connection, pairing and presence ask on every event, and a peer on the LAN can make those events as often
//! as it likes. [`CachedInterfaces`] answers from a copy no older than [`TTL`] and is emptied the moment the network is
//! known to have changed ([`CachedInterfaces::invalidate`], called with every `NetworkChanged`), so the answer is
//! never older than the news. [`list_async`] reads the list from a runtime worker without blocking it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_platform::{NetInterface, NetInterfaces, PlatformError, Result};

/// How long a copy of the list is good.
pub const TTL: Duration = Duration::from_secs(1);

/// How old an expired copy may be and still answer [`NetInterfaces::warm_copy`] (a dial or an accept a person waits for):
/// the network-change news empties the copy at once, so this only bounds a copy whose news never came.
pub const WARM_LIMIT: Duration = Duration::from_secs(30);

/// A [`NetInterfaces`] that reads the real one at most once per [`TTL`]. A failed read is not kept: the next call tries
/// again.
pub struct CachedInterfaces {
    inner: Arc<dyn NetInterfaces>,
    ttl: Duration,
    warm_limit: Duration,
    slot: Arc<Mutex<Slot>>,
}

#[derive(Default)]
struct Slot {
    kept: Option<(Instant, Vec<NetInterface>)>,
    /// A read of the real list is under way on some thread.
    refreshing: bool,
    /// Bumped by `invalidate`: a read that began before it must not be kept.
    generation: u64,
}

impl CachedInterfaces {
    pub fn new(inner: Arc<dyn NetInterfaces>) -> Self {
        Self::with_ttl(inner, TTL)
    }

    pub fn with_ttl(inner: Arc<dyn NetInterfaces>, ttl: Duration) -> Self {
        Self::with_limits(inner, ttl, WARM_LIMIT)
    }

    pub fn with_limits(inner: Arc<dyn NetInterfaces>, ttl: Duration, warm_limit: Duration) -> Self {
        Self { inner, ttl, warm_limit, slot: Arc::new(Mutex::new(Slot::default())) }
    }

    /// The network changed (or may have): the next `list` reads the real one.
    pub fn invalidate(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        slot.kept = None;
        slot.generation += 1;
    }
}

impl NetInterfaces for CachedInterfaces {
    fn list(&self) -> Result<Vec<NetInterface>> {
        let generation = {
            let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((at, list)) = slot.kept.as_ref() {
                // A fresh copy, or an expired one while another thread is already reading the new one: the caller takes
                // what there is and does not wait for the OS (a worker thread may be the caller).
                if at.elapsed() < self.ttl || slot.refreshing {
                    return Ok(list.clone());
                }
            }
            slot.refreshing = true;
            slot.generation
        };
        read_and_keep(&*self.inner, &self.slot, generation)
    }

    /// The copy, when there is one that is fresh or expired for less than the warm limit; an expired one starts the
    /// refresh on a thread of its own (one at a time), so the caller never waits for the OS. Nothing after a network
    /// change ([`CachedInterfaces::invalidate`]): the news empties the copy, and the caller reads.
    fn warm_copy(&self) -> Option<Vec<NetInterface>> {
        let (list, generation) = {
            let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
            let (at, list) = slot.kept.as_ref()?;
            let age = at.elapsed();
            if age < self.ttl {
                return Some(list.clone());
            }
            if age >= self.warm_limit {
                return None;
            }
            let list = list.clone();
            if slot.refreshing {
                return Some(list);
            }
            slot.refreshing = true;
            (list, slot.generation)
        };
        let (inner, slot) = (Arc::clone(&self.inner), Arc::clone(&self.slot));
        let started = std::thread::Builder::new().name("net-refresh".into()).spawn({
            let slot = Arc::clone(&slot);
            move || {
                let _ = read_and_keep(&*inner, &slot, generation);
            }
        });
        if started.is_err() {
            slot.lock().unwrap_or_else(PoisonError::into_inner).refreshing = false;
        }
        Some(list)
    }
}

/// Reads the OS (the caller took `refreshing` and holds no lock) and keeps the answer unless a network change overtook
/// the read. A failed read is not kept.
fn read_and_keep(inner: &dyn NetInterfaces, slot: &Mutex<Slot>, generation: u64) -> Result<Vec<NetInterface>> {
    let read = inner.list();
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    slot.refreshing = false;
    let list = read?;
    // A network change during the read makes this answer old: it is returned, not kept.
    if slot.generation == generation {
        slot.kept = Some((Instant::now(), list.clone()));
    }
    Ok(list)
}

/// Reads `nets` on a blocking thread: the enumeration never runs on one of the runtime's workers.
pub async fn list_async(nets: &Arc<dyn NetInterfaces>) -> Result<Vec<NetInterface>> {
    let nets = Arc::clone(nets);
    tokio::task::spawn_blocking(move || nets.list()).await.unwrap_or_else(|e| {
        Err(PlatformError {
            kind: swapdesk_platform::ErrorKind::Os,
            op: "NetInterfaces::list",
            os_code: None,
            detail: format!("the read ended early: {e}"),
        })
    })
}

/// As [`list_async`], but a copy the cache can hand over at once ([`NetInterfaces::warm_copy`]) is taken without a
/// trip to a blocking thread. For the paths a person waits on.
pub async fn list_warm_async(nets: &Arc<dyn NetInterfaces>) -> Result<Vec<NetInterface>> {
    match nets.warm_copy() {
        Some(list) => Ok(list),
        None => list_async(nets).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        reads: AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
    }

    impl NetInterfaces for Counting {
        fn list(&self) -> Result<Vec<NetInterface>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(PlatformError {
                    kind: swapdesk_platform::ErrorKind::Os,
                    op: "test",
                    os_code: None,
                    detail: "down".into(),
                });
            }
            Ok(Vec::new())
        }
    }

    fn counting() -> (Arc<Counting>, CachedInterfaces) {
        let inner = Arc::new(Counting { reads: AtomicUsize::new(0), fail: false.into() });
        let cache = CachedInterfaces::new(inner.clone());
        (inner, cache)
    }

    #[test]
    fn a_hundred_calls_inside_the_ttl_read_the_real_list_once() {
        let (inner, cache) = counting();
        for _ in 0..100 {
            cache.list().unwrap();
        }
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_copy_expires_after_the_ttl_and_a_network_change_empties_it_at_once() {
        let inner = Arc::new(Counting { reads: AtomicUsize::new(0), fail: false.into() });
        let cache = CachedInterfaces::with_ttl(inner.clone(), Duration::from_millis(60));
        cache.list().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        cache.list().unwrap();
        assert_eq!(inner.reads.load(Ordering::SeqCst), 2, "read again after the ttl");
        let (inner, cache) = counting();
        cache.list().unwrap();
        cache.invalidate();
        cache.list().unwrap();
        assert_eq!(inner.reads.load(Ordering::SeqCst), 2, "read again after invalidate, well inside the ttl");
    }

    #[test]
    fn a_failed_read_is_not_kept() {
        let (inner, cache) = counting();
        inner.fail.store(true, Ordering::SeqCst);
        assert!(cache.list().is_err());
        inner.fail.store(false, Ordering::SeqCst);
        assert!(cache.list().is_ok(), "the next call reads again");
        assert_eq!(inner.reads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn concurrent_callers_after_the_first_read_share_it() {
        let (inner, cache) = counting();
        let cache = Arc::new(cache);
        cache.list().unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || cache.list().unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);
    }

    /// A list whose real read stops until the test lets it go.
    struct Gated {
        entered: Mutex<std::sync::mpsc::Sender<()>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        stopped: std::sync::atomic::AtomicBool,
    }

    impl NetInterfaces for Gated {
        fn list(&self) -> Result<Vec<NetInterface>> {
            if self.stopped.load(Ordering::SeqCst) {
                self.entered.lock().unwrap().send(()).unwrap();
                let _ = self.release.lock().unwrap().recv_timeout(Duration::from_secs(10));
            }
            Ok(Vec::new())
        }
    }

    fn gated(stopped: bool) -> (Arc<Gated>, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let g = Arc::new(Gated {
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            stopped: stopped.into(),
        });
        (g, entered_rx, release_tx)
    }

    #[test]
    fn a_refresh_under_way_never_makes_another_caller_wait_for_the_os() {
        let (g, entered, release) = gated(false);
        let cache = Arc::new(CachedInterfaces::with_ttl(g.clone(), Duration::from_millis(30)));
        cache.list().unwrap();
        std::thread::sleep(Duration::from_millis(60));
        // The copy expired; the next read of the OS stops.
        g.stopped.store(true, Ordering::SeqCst);
        let refresher = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || cache.list().unwrap())
        };
        entered.recv_timeout(Duration::from_secs(5)).expect("the refresh is in the OS");
        // Another caller (a worker thread) gets the expired copy at once instead of waiting behind the refresh.
        let (tx, rx) = std::sync::mpsc::channel();
        let other = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || tx.send(cache.list().is_ok()).unwrap())
        };
        let answered = rx.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        refresher.join().unwrap();
        other.join().unwrap();
        assert_eq!(answered, Ok(true), "a caller waited for another caller's enumeration");
    }

    #[test]
    fn a_read_that_a_network_change_overtook_is_returned_but_not_kept() {
        let (g, entered, release) = gated(true);
        let cache = Arc::new(CachedInterfaces::new(g));
        let reader = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || cache.list().unwrap())
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        cache.invalidate();
        release.send(()).unwrap();
        reader.join().unwrap();
        // Nothing was kept: the next call reads the OS again (and stops in the gate until released).
        let again = {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || cache.list().unwrap())
        };
        entered.recv_timeout(Duration::from_secs(5)).expect("read again: the old read was not kept");
        release.send(()).unwrap();
        again.join().unwrap();
    }

    #[test]
    fn list_async_runs_the_read_on_a_blocking_thread_and_leaves_the_runtime_free() {
        struct Slow {
            entered: Mutex<std::sync::mpsc::Sender<std::thread::ThreadId>>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl NetInterfaces for Slow {
            fn list(&self) -> Result<Vec<NetInterface>> {
                let _ = self.entered.lock().unwrap().send(std::thread::current().id());
                let _ = self.release.lock().unwrap().recv_timeout(Duration::from_secs(10));
                Ok(Vec::new())
            }
        }
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let nets: Arc<dyn NetInterfaces> =
            Arc::new(Slow { entered: Mutex::new(entered_tx), release: Mutex::new(release_rx) });
        // One worker thread: a read on it would stop the timer below from firing.
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_time().build().unwrap();
        let this_thread = std::thread::current().id();
        rt.block_on(async {
            let read = tokio::spawn(async move { list_async(&nets).await });
            let reader = tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(5)).unwrap())
                .await
                .unwrap();
            assert_ne!(reader, this_thread);
            tokio::time::timeout(Duration::from_millis(200), std::future::pending::<()>()).await.unwrap_err();
            assert!(!read.is_finished(), "the read is still stopped in the OS");
            release_tx.send(()).unwrap();
            assert!(read.await.unwrap().is_ok());
        });
    }

    /// A list whose real read takes `delay_ms` (settable), as the Windows enumeration does under load or while the
    /// firewall's COM service is busy.
    struct Slowed {
        delay_ms: std::sync::atomic::AtomicU64,
        reads: AtomicUsize,
    }

    impl NetInterfaces for Slowed {
        fn list(&self) -> Result<Vec<NetInterface>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(self.delay_ms.load(Ordering::SeqCst)));
            Ok(Vec::new())
        }
    }

    fn slowed(ttl_ms: u64, warm_ms: u64) -> (Arc<Slowed>, Arc<CachedInterfaces>, Arc<dyn NetInterfaces>) {
        let inner = Arc::new(Slowed { delay_ms: 0.into(), reads: AtomicUsize::new(0) });
        let cache = Arc::new(CachedInterfaces::with_limits(
            inner.clone(),
            Duration::from_millis(ttl_ms),
            Duration::from_millis(warm_ms),
        ));
        let nets: Arc<dyn NetInterfaces> = cache.clone();
        (inner, cache, nets)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap()
    }

    /// B4: the reverse dial of a swap (and the accept of one) must not wait for the OS to enumerate the adapters
    /// when the copy has only just expired: the harness saw 1.4 s once, the enumeration being the slow step.
    #[test]
    fn a_dial_after_the_copy_expired_does_not_wait_for_a_slow_enumeration() {
        let (inner, _cache, nets) = slowed(30, 60_000);
        let rt = runtime();
        rt.block_on(list_warm_async(&nets)).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        inner.delay_ms.store(500, Ordering::SeqCst);
        let started = Instant::now();
        rt.block_on(list_warm_async(&nets)).unwrap();
        let waited = started.elapsed();
        assert!(waited < Duration::from_millis(100), "the dial waited {waited:?} for the enumeration");
        // The copy is refreshed behind the dial's back.
        let until = Instant::now() + Duration::from_secs(5);
        while inner.reads.load(Ordering::SeqCst) < 2 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(inner.reads.load(Ordering::SeqCst), 2, "the refresh ran in the background");
    }

    #[test]
    fn a_network_change_or_a_copy_past_the_warm_limit_is_read_before_the_dial_goes_on() {
        let (inner, cache, nets) = slowed(30, 200);
        let rt = runtime();
        rt.block_on(list_warm_async(&nets)).unwrap();
        inner.delay_ms.store(150, Ordering::SeqCst);
        // After the news there is no copy: the dial waits for a real read.
        cache.invalidate();
        let started = Instant::now();
        rt.block_on(list_warm_async(&nets)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150), "no copy was left after a network change");
        // Past the warm limit the copy is too old to hand over.
        std::thread::sleep(Duration::from_millis(250));
        let before = inner.reads.load(Ordering::SeqCst);
        let started = Instant::now();
        rt.block_on(list_warm_async(&nets)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150), "a copy older than the warm limit was handed over");
        assert_eq!(inner.reads.load(Ordering::SeqCst), before + 1);
    }

    #[test]
    fn only_one_background_refresh_runs_at_a_time() {
        let (inner, _cache, nets) = slowed(20, 60_000);
        let rt = runtime();
        rt.block_on(list_warm_async(&nets)).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        inner.delay_ms.store(300, Ordering::SeqCst);
        for _ in 0..20 {
            rt.block_on(list_warm_async(&nets)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(inner.reads.load(Ordering::SeqCst), 2, "one first read and one refresh");
    }
}
