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

/// A [`NetInterfaces`] that reads the real one at most once per [`TTL`]. A failed read is not kept: the next call tries
/// again.
pub struct CachedInterfaces {
    inner: Arc<dyn NetInterfaces>,
    ttl: Duration,
    slot: Mutex<Slot>,
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
        Self { inner, ttl, slot: Mutex::new(Slot::default()) }
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
        // The OS is asked with the lock released: nobody waits on this enumeration but its caller.
        let read = self.inner.list();
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        slot.refreshing = false;
        let list = read?;
        // A network change during the read makes this answer old: it is returned, not kept.
        if slot.generation == generation {
            slot.kept = Some((Instant::now(), list.clone()));
        }
        Ok(list)
    }
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
}
