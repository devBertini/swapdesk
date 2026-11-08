use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

use crate::link::{DropReason, Impairment, Link, Verdict};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Packet counters of one socket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets handed to `try_send`.
    pub sent: u64,
    pub dropped_loss: u64,
    /// Dropped because the link was down: at send time, while queued, or on arrival.
    pub dropped_down: u64,
    pub dropped_queue: u64,
    /// Packets that left for the real socket.
    pub delivered: u64,
    /// Datagrams `poll_recv` handed to quinn.
    pub received: u64,
}

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    dropped_loss: AtomicU64,
    dropped_down: AtomicU64,
    dropped_queue: AtomicU64,
    delivered: AtomicU64,
    received: AtomicU64,
}

struct Pending {
    at: Duration,
    seq: u64,
    dest: SocketAddr,
    data: Vec<u8>,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pending {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

struct Shared {
    link: Mutex<Link>,
    /// Mirror of the link state for the receive path and the sender task.
    up: AtomicBool,
    start: Instant,
    queue: Mutex<BinaryHeap<Reverse<Pending>>>,
    wake: Notify,
    closed: AtomicBool,
    next_seq: AtomicU64,
    counters: Counters,
}

impl Shared {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

/// Control of a running socket, usable from the test while the endpoint owns the socket.
#[derive(Clone)]
pub struct LinkHandle(Arc<Shared>);

impl LinkHandle {
    /// Takes the link down (everything sent, queued or received is dropped) or brings it back up.
    pub fn set_link_up(&self, up: bool) {
        lock(&self.0.link).set_up(up);
        self.0.up.store(up, Ordering::SeqCst);
    }

    pub fn is_link_up(&self) -> bool {
        self.0.up.load(Ordering::SeqCst)
    }

    /// Replaces the impairments from the next packet on.
    pub fn set_impairment(&self, config: Impairment) {
        lock(&self.0.link).set_config(config);
    }

    pub fn stats(&self) -> Stats {
        let c = &self.0.counters;
        Stats {
            sent: c.sent.load(Ordering::SeqCst),
            dropped_loss: c.dropped_loss.load(Ordering::SeqCst),
            dropped_down: c.dropped_down.load(Ordering::SeqCst),
            dropped_queue: c.dropped_queue.load(Ordering::SeqCst),
            delivered: c.delivered.load(Ordering::SeqCst),
            received: c.received.load(Ordering::SeqCst),
        }
    }
}

/// A real UDP socket that impairs what it sends: loss, delay with jitter, reordering and a bandwidth cap, from a
/// seeded generator, plus a link that can go down and up while it runs. While the link is down it also drops what
/// arrives. Impairments act on the sending direction only: give both endpoints of a test an `ImpairedSocket` to
/// impair both ways.
///
/// Needs a tokio runtime with time and net enabled, at `bind` and afterwards (a task sends delayed packets).
pub struct ImpairedSocket {
    io: Arc<UdpSocket>,
    shared: Arc<Shared>,
}

impl fmt::Debug for ImpairedSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImpairedSocket").field("local_addr", &self.io.local_addr().ok()).finish_non_exhaustive()
    }
}

impl ImpairedSocket {
    /// Binds a UDP socket at `addr` (use `127.0.0.1:0` in tests).
    pub fn bind(addr: SocketAddr, config: Impairment, seed: u64) -> io::Result<Self> {
        let std_socket = std::net::UdpSocket::bind(addr)?;
        Self::from_std(std_socket, config, seed)
    }

    pub fn from_std(socket: std::net::UdpSocket, config: Impairment, seed: u64) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let io = Arc::new(UdpSocket::from_std(socket)?);
        let shared = Arc::new(Shared {
            link: Mutex::new(Link::new(config, seed)),
            up: AtomicBool::new(true),
            start: Instant::now(),
            queue: Mutex::new(BinaryHeap::new()),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            next_seq: AtomicU64::new(0),
            counters: Counters::default(),
        });
        tokio::spawn(sender_task(Arc::clone(&io), Arc::clone(&shared)));
        Ok(Self { io, shared })
    }

    pub fn handle(&self) -> LinkHandle {
        LinkHandle(Arc::clone(&self.shared))
    }

    /// A quinn endpoint over this socket.
    pub fn into_endpoint(self, server_config: Option<quinn::ServerConfig>) -> io::Result<quinn::Endpoint> {
        quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            server_config,
            Arc::new(self),
            Arc::new(quinn::TokioRuntime),
        )
    }
}

impl Drop for ImpairedSocket {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.wake.notify_one();
    }
}

async fn sender_task(io: Arc<UdpSocket>, shared: Arc<Shared>) {
    loop {
        if shared.closed.load(Ordering::SeqCst) {
            return;
        }
        let now = shared.now();
        let next = {
            let mut q = lock(&shared.queue);
            match q.peek() {
                Some(Reverse(p)) if p.at <= now => q.pop().map(|Reverse(p)| Ok(p)),
                Some(Reverse(p)) => Some(Err(p.at - now)),
                None => None,
            }
        };
        match next {
            Some(Ok(p)) => {
                if shared.up.load(Ordering::SeqCst) {
                    // A send error is a lost packet, as on a real network.
                    if io.send_to(&p.data, p.dest).await.is_ok() {
                        shared.counters.delivered.fetch_add(1, Ordering::SeqCst);
                    }
                } else {
                    shared.counters.dropped_down.fetch_add(1, Ordering::SeqCst);
                }
            }
            Some(Err(wait)) => {
                let _ = tokio::time::timeout(wait, shared.wake.notified()).await;
            }
            None => shared.wake.notified().await,
        }
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for ImpairedSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let s = &self.shared;
        s.counters.sent.fetch_add(1, Ordering::SeqCst);
        let verdict = {
            let now = s.now();
            lock(&s.link).decide(now, transmit.contents.len())
        };
        match verdict {
            Verdict::Drop(DropReason::Loss) => {
                s.counters.dropped_loss.fetch_add(1, Ordering::SeqCst);
            }
            Verdict::Drop(DropReason::LinkDown) => {
                s.counters.dropped_down.fetch_add(1, Ordering::SeqCst);
            }
            Verdict::Drop(DropReason::QueueFull) => {
                s.counters.dropped_queue.fetch_add(1, Ordering::SeqCst);
            }
            Verdict::Deliver { at } => {
                let seq = s.next_seq.fetch_add(1, Ordering::SeqCst);
                lock(&s.queue).push(Reverse(Pending {
                    at,
                    seq,
                    dest: transmit.destination,
                    data: transmit.contents.to_vec(),
                }));
                s.wake.notify_one();
            }
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut buf = ReadBuf::new(&mut bufs[0]);
            match self.io.poll_recv_from(cx, &mut buf) {
                Poll::Pending => return Poll::Pending,
                // Windows reports an earlier send to a closed port as a reset on the next receive.
                Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(addr)) => {
                    if !self.shared.up.load(Ordering::SeqCst) {
                        self.shared.counters.dropped_down.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                    let len = buf.filled().len();
                    meta[0] = RecvMeta { addr, len, stride: len, ecn: None, dst_ip: None };
                    self.shared.counters.received.fetch_add(1, Ordering::SeqCst);
                    return Poll::Ready(Ok(1));
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    fn may_fragment(&self) -> bool {
        // quinn then keeps its initial MTU; no don't-fragment option is set on this socket.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn lo() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn tx(dest: SocketAddr, data: &[u8]) -> Transmit<'_> {
        Transmit { destination: dest, ecn: None, contents: data, segment_size: None, src_ip: None }
    }

    /// Receives on a plain tokio socket until nothing arrives for `idle`.
    async fn drain(rx: &UdpSocket, idle: Duration) -> Vec<Vec<u8>> {
        let mut got = Vec::new();
        let mut buf = [0u8; 2048];
        while let Ok(Ok(n)) = tokio::time::timeout(idle, rx.recv(&mut buf)).await {
            got.push(buf[..n].to_vec());
        }
        got
    }

    #[test]
    fn loss_on_a_real_socket_matches_the_counters() {
        rt().block_on(async {
            let rx = UdpSocket::bind(lo()).await.unwrap();
            let dest = rx.local_addr().unwrap();
            let s = ImpairedSocket::bind(lo(), Impairment { loss: 0.10, ..Impairment::default() }, 99).unwrap();
            let h = s.handle();
            let n = 5_000u32;
            for i in 0..n {
                s.try_send(&tx(dest, &i.to_le_bytes())).unwrap();
                if i % 50 == 49 {
                    // Let the sender task and the OS buffer breathe, so only the model loses packets.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    tokio::task::yield_now().await;
                }
            }
            let got = drain(&rx, Duration::from_millis(300)).await;
            let st = h.stats();
            assert_eq!(st.sent, u64::from(n));
            assert_eq!(st.delivered + st.dropped_loss, u64::from(n));
            assert!((400..=600).contains(&st.dropped_loss), "{st:?}");
            assert_eq!(got.len() as u64, st.delivered, "{st:?}");
            // Order is kept when only loss is on.
            let ids: Vec<u32> = got.iter().map(|d| u32::from_le_bytes(d[..4].try_into().unwrap())).collect();
            assert!(ids.windows(2).all(|w| w[0] < w[1]));
        });
    }

    #[test]
    fn link_down_drops_everything_both_ways_and_up_restores() {
        rt().block_on(async {
            let plain = UdpSocket::bind(lo()).await.unwrap();
            let plain_addr = plain.local_addr().unwrap();
            let s = Arc::new(ImpairedSocket::bind(lo(), Impairment::default(), 1).unwrap());
            let h = s.handle();
            let s_addr = s.local_addr().unwrap();

            h.set_link_up(false);
            assert!(!h.is_link_up());
            for i in 0..100u8 {
                s.try_send(&tx(plain_addr, &[i])).unwrap();
            }
            assert!(drain(&plain, Duration::from_millis(200)).await.is_empty());
            assert_eq!(h.stats().dropped_down, 100);

            // Receive path: a datagram from outside is discarded while down.
            plain.send_to(b"in", s_addr).await.unwrap();
            let mut storage = [0u8; 64];
            let mut metas = [RecvMeta::default()];
            let polled = tokio::time::timeout(
                Duration::from_millis(200),
                std::future::poll_fn(|cx| {
                    let mut bufs = [IoSliceMut::new(&mut storage)];
                    s.poll_recv(cx, &mut bufs, &mut metas)
                }),
            )
            .await;
            assert!(polled.is_err(), "nothing may come through a down link");
            assert_eq!(h.stats().received, 0);

            h.set_link_up(true);
            s.try_send(&tx(plain_addr, b"back")).unwrap();
            assert_eq!(drain(&plain, Duration::from_millis(300)).await, vec![b"back".to_vec()]);
            plain.send_to(b"in2", s_addr).await.unwrap();
            let n = std::future::poll_fn(|cx| {
                let mut bufs = [IoSliceMut::new(&mut storage)];
                s.poll_recv(cx, &mut bufs, &mut metas)
            })
            .await
            .unwrap();
            assert_eq!((n, metas[0].len), (1, 3));
            assert_eq!(&storage[..3], b"in2");
        });
    }

    #[test]
    fn queued_packets_are_lost_when_the_link_goes_down() {
        rt().block_on(async {
            let rx = UdpSocket::bind(lo()).await.unwrap();
            let dest = rx.local_addr().unwrap();
            let cfg = Impairment { delay: Duration::from_millis(150), ..Impairment::default() };
            let s = ImpairedSocket::bind(lo(), cfg, 1).unwrap();
            let h = s.handle();
            for i in 0..10u8 {
                s.try_send(&tx(dest, &[i])).unwrap();
            }
            h.set_link_up(false);
            assert!(drain(&rx, Duration::from_millis(400)).await.is_empty());
            assert_eq!(h.stats().dropped_down, 10);
        });
    }

    #[test]
    fn delay_is_applied_and_changes_at_run_time() {
        rt().block_on(async {
            let rx = UdpSocket::bind(lo()).await.unwrap();
            let dest = rx.local_addr().unwrap();
            let cfg = Impairment { delay: Duration::from_millis(80), ..Impairment::default() };
            let s = ImpairedSocket::bind(lo(), cfg, 1).unwrap();
            let t = Instant::now();
            s.try_send(&tx(dest, b"slow")).unwrap();
            let mut buf = [0u8; 16];
            rx.recv(&mut buf).await.unwrap();
            assert!(t.elapsed() >= Duration::from_millis(75), "{:?}", t.elapsed());
            s.handle().set_impairment(Impairment::default());
            let t = Instant::now();
            s.try_send(&tx(dest, b"fast")).unwrap();
            rx.recv(&mut buf).await.unwrap();
            assert!(t.elapsed() < Duration::from_millis(60), "{:?}", t.elapsed());
        });
    }

    #[test]
    fn bandwidth_cap_paces_real_packets() {
        rt().block_on(async {
            let rx = UdpSocket::bind(lo()).await.unwrap();
            let dest = rx.local_addr().unwrap();
            // 400 kbit/s, 500-byte packets: 10 ms each.
            let cfg = Impairment { bandwidth_bps: Some(400_000), ..Impairment::default() };
            let s = ImpairedSocket::bind(lo(), cfg, 1).unwrap();
            let t = Instant::now();
            for _ in 0..10 {
                s.try_send(&tx(dest, &[0u8; 500])).unwrap();
            }
            let mut buf = [0u8; 600];
            for _ in 0..10 {
                rx.recv(&mut buf).await.unwrap();
            }
            assert!(t.elapsed() >= Duration::from_millis(95), "{:?}", t.elapsed());
        });
    }

    #[test]
    fn dropping_the_socket_ends_its_task() {
        rt().block_on(async {
            let s = ImpairedSocket::bind(lo(), Impairment::default(), 1).unwrap();
            let shared = Arc::clone(&s.shared);
            drop(s);
            for _ in 0..50 {
                if Arc::strong_count(&shared) == 1 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("sender task still holds the shared state");
        });
    }
}
