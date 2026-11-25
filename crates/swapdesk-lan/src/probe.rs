//! Direct probe (F-FIND-05, D94): finds Swapdesk computers that mDNS cannot reach (another subnet, a VPN, a router
//! that isolates devices, a NAT such as the Android emulator's), by asking addresses directly over UDP.
//!
//! - Every announcing device answers on [`PROBE_PORT`] with its record (the same TXT keys as its DNS-SD record, the
//!   session port and the protocol minor), and only while its record is announced. mDNS cannot carry this: on macOS a
//!   unicast datagram to 5353 goes to the system's responder, not to Swapdesk (seen 25/11: the Mac answered nothing).
//! - A request is [`REQUEST_LEN`] bytes, at least as long as any reply, so the responder never sends more than it
//!   received (no amplification), and the responder answers at most [`REPLIES_PER_SECOND`] requests a second.
//! - The prober searches once when discovery starts (the app opens) and again each time it is asked
//!   ([`crate::discovery::Discovery::search`]: the person pressed "Buscar"), never on a timer (the owner, 25/11). A
//!   search asks the addresses that answered before, the hints (addresses the user typed or paired with), every host
//!   of the /24 around each own address, of the /24 of each hint and of the home ranges in [`HOME_RANGES`] (most home
//!   routers hand out one of them; a phone behind a NAT, such as the emulator's, reaches its computers through them).
//!   A device found by probe that does not answer the next search is lost.
//! - Replies pass the same checks as a DNS-SD answer: the TXT decodes, it is not this device, and the guard keeps the
//!   source address ([`link_for_route`]).

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_platform::{IfKind, NetInterfaces};

use crate::discovery::{Merger, Peer, Source};
use crate::guard::{ApprovedAddr, approved_addrs, link_for_route};
use crate::socket::bind_quic_socket;
use crate::txt::{FINGERPRINT_BYTES, Txt, read_minor};

/// UDP port every announcing device answers probes on, the same on every computer: below the session's 47820, where
/// no account's or development instance's pair of ports ever sits (ACCOUNTS.md: 47820 + 2k; instances 47820 + 10n).
/// On a computer with several accounts or instances running, the first to bind it answers, with its own session
/// port; the others are still found by mDNS.
pub const PROBE_PORT: u16 = 47819;
/// Length of every request: at least the longest reply.
pub const REQUEST_LEN: usize = 256;
/// Requests answered per second, at most.
pub const REPLIES_PER_SECOND: u32 = 100;
/// The /24 networks of most home routers, swept from a Wi-Fi or Ethernet address in every full round.
pub const HOME_RANGES: [[u8; 3]; 4] = [[192, 168, 0], [192, 168, 1], [10, 0, 0], [10, 0, 1]];
/// How long a round waits for replies after its last request.
const LISTEN: Duration = Duration::from_millis(1200);

const REQUEST_MAGIC: &[u8; 8] = b"SWDPRB1?";
const REPLY_MAGIC: &[u8; 8] = b"SWDPRB1!";

/// The request: the magic, then zeros to [`REQUEST_LEN`].
pub fn request() -> Vec<u8> {
    let mut v = vec![0u8; REQUEST_LEN];
    v[..8].copy_from_slice(REQUEST_MAGIC);
    v
}

fn is_request(b: &[u8]) -> bool {
    b.len() == REQUEST_LEN && b.starts_with(REQUEST_MAGIC)
}

/// The reply for `own`: magic, session port (u16 BE), then each TXT entry as key length (u8), key, value length (u8),
/// value. `None` when it would not fit in [`REQUEST_LEN`] or the record does not encode.
pub fn reply(own: &Txt, port: u16, minor: u16) -> Option<Vec<u8>> {
    let mut props = own.encode().ok()?;
    props.extend(crate::txt::minor_entry(minor));
    let mut v = Vec::with_capacity(REQUEST_LEN);
    v.extend_from_slice(REPLY_MAGIC);
    v.extend_from_slice(&port.to_be_bytes());
    for (k, val) in &props {
        let (k, val) = (k.as_bytes(), val.as_bytes());
        v.push(u8::try_from(k.len()).ok()?);
        v.extend_from_slice(k);
        v.push(u8::try_from(val.len()).ok()?);
        v.extend_from_slice(val);
    }
    (v.len() <= REQUEST_LEN).then_some(v)
}

/// Reads a reply: the record, the session port and the minor. `None` for anything malformed.
pub fn read_reply(b: &[u8]) -> Option<(Txt, u16, u16)> {
    if b.len() > REQUEST_LEN || !b.starts_with(REPLY_MAGIC) || b.len() < 10 {
        return None;
    }
    let port = u16::from_be_bytes([b[8], b[9]]);
    let mut entries: Vec<(&str, &[u8])> = Vec::new();
    let mut i = 10;
    while i < b.len() {
        let kl = usize::from(b[i]);
        let k = std::str::from_utf8(b.get(i + 1..i + 1 + kl)?).ok()?;
        i += 1 + kl;
        let vl = usize::from(*b.get(i)?);
        let val = b.get(i + 1..i + 1 + vl)?;
        i += 1 + vl;
        entries.push((k, val));
    }
    let txt = Txt::decode(&entries).ok()?;
    if port == 0 {
        return None;
    }
    Some((txt, port, read_minor(&entries)))
}

/// The hosts of the /24 that holds `a`, without .0 and .255.
fn slash24(a: Ipv4Addr) -> impl Iterator<Item = Ipv4Addr> {
    let [x, y, z, _] = a.octets();
    (1..=254u8).map(move |d| Ipv4Addr::new(x, y, z, d))
}

/// The addresses of one search: `answered` and `hints`, the /24 around each own address (the subnet itself when it
/// is smaller), the /24 of each hint, and the home ranges when a Wi-Fi or Ethernet address exists. Never an own
/// address.
pub fn targets(approved: &[ApprovedAddr], answered: &[Ipv4Addr], hints: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let own: BTreeSet<Ipv4Addr> = approved.iter().map(|a| a.addr).collect();
    let mut set: BTreeSet<Ipv4Addr> = answered.iter().chain(hints).copied().collect();
    {
        for a in approved {
            if a.prefix >= 24 && a.prefix <= 30 {
                let mask = u32::MAX << (32 - u32::from(a.prefix));
                let net = u32::from(a.addr) & mask;
                let last = net | !mask;
                set.extend((net + 1..last).map(Ipv4Addr::from));
            } else if a.prefix < 24 {
                set.extend(slash24(a.addr));
            }
        }
        for h in hints {
            set.extend(slash24(*h));
        }
        if approved.iter().any(|a| matches!(a.kind, IfKind::Ethernet | IfKind::WiFi)) {
            for [x, y, z] in HOME_RANGES {
                set.extend(slash24(Ipv4Addr::new(x, y, z, 1)));
            }
        }
    }
    set.retain(|t| {
        !own.contains(t) && !t.is_loopback() && !t.is_multicast() && !t.is_broadcast() && !t.is_unspecified()
    });
    set.into_iter().collect()
}

/// Answers probes with the reply `current` holds (none while the record is withdrawn).
pub(crate) struct Responder {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Responder {
    pub(crate) fn start(port: u16, current: Arc<Mutex<Option<Vec<u8>>>>) -> io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))?;
        socket.set_read_timeout(Some(Duration::from_millis(250)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new().name("swapdesk-probe-answer".into()).spawn(move || {
                let mut buf = [0u8; REQUEST_LEN + 1];
                let (mut second, mut sent) = (Instant::now(), 0u32);
                while !stop.load(Ordering::Acquire) {
                    let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
                    if !is_request(&buf[..n]) {
                        continue;
                    }
                    if second.elapsed() >= Duration::from_secs(1) {
                        (second, sent) = (Instant::now(), 0);
                    }
                    if sent >= REPLIES_PER_SECOND {
                        continue;
                    }
                    let answer = current.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                    if let Some(answer) = answer {
                        sent += 1;
                        let _ = socket.send_to(&answer, from);
                    }
                }
            })?
        };
        Ok(Self { stop, worker: Some(worker) })
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// What the last search did, for the trace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchReport {
    /// Requests sent.
    pub asked: usize,
    /// Replies read (any source, before the checks).
    pub replies: usize,
    /// Devices kept after the checks.
    pub found: usize,
    /// Why nothing was sent, when nothing was.
    pub error: Option<String>,
}

#[derive(Default)]
struct Asked {
    stop: bool,
    /// Searches asked and not started yet (at most one waits: asking during a search adds one more after it).
    search: bool,
    /// Searches finished.
    done: u64,
    last: SearchReport,
}

/// What the prober shares with its owner.
#[derive(Default)]
pub(crate) struct ProberShared {
    pub(crate) hints: Mutex<Vec<Ipv4Addr>>,
    asked: Mutex<Asked>,
    wake: Condvar,
}

impl ProberShared {
    /// Asks for a search (it runs at once, or right after the one running).
    pub(crate) fn search(&self) {
        self.asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).search = true;
        self.wake.notify_all();
    }

    /// Searches finished so far.
    pub(crate) fn searches_done(&self) -> u64 {
        self.asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).done
    }

    /// What the last search did.
    pub(crate) fn last_search(&self) -> SearchReport {
        self.asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).last.clone()
    }
}

pub(crate) struct Prober {
    shared: Arc<ProberShared>,
    worker: Option<JoinHandle<()>>,
}

/// The socket a round sends from. On Android it is bound to the first approved address, so the platform's step binds
/// it to that network; elsewhere it is unbound and the system routes each request.
fn round_socket(approved: &[ApprovedAddr]) -> io::Result<UdpSocket> {
    let s = if cfg!(target_os = "android") {
        let first = approved.first().ok_or_else(|| io::Error::other("no approved address"))?;
        bind_quic_socket(SocketAddrV4::new(first.addr, 0))?
    } else {
        bind_quic_socket(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?
    };
    s.set_read_timeout(Some(Duration::from_millis(50)))?;
    Ok(s)
}

impl Prober {
    pub(crate) fn start(
        nets: Arc<dyn NetInterfaces>,
        own_fp: [u8; FINGERPRINT_BYTES],
        port: u16,
        merger: Arc<Merger>,
        shared: Arc<ProberShared>,
    ) -> io::Result<Self> {
        shared.search();
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("swapdesk-probe".into())
                .spawn(move || run_prober(&*nets, own_fp, port, &merger, &shared))?
        };
        Ok(Self { shared, worker: Some(worker) })
    }
}

impl Drop for Prober {
    fn drop(&mut self) {
        self.shared.asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).stop = true;
        self.shared.wake.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn run_prober(
    nets: &dyn NetInterfaces,
    own_fp: [u8; FINGERPRINT_BYTES],
    port: u16,
    merger: &Merger,
    shared: &ProberShared,
) {
    // What the last search found, by fingerprint.
    let mut seen: HashMap<[u8; FINGERPRINT_BYTES], Peer> = HashMap::new();
    let req = request();
    loop {
        {
            let mut asked = shared.asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while !asked.stop && !asked.search {
                asked = shared.wake.wait(asked).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if asked.stop {
                return;
            }
            asked.search = false;
        }
        let list = nets.list().unwrap_or_default();
        let approved = approved_addrs(&list);
        let answered: Vec<Ipv4Addr> = seen.values().flat_map(|p| p.links.iter().map(|l| l.remote)).collect();
        let hints = shared.hints.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let mut heard: HashMap<[u8; FINGERPRINT_BYTES], Peer> = HashMap::new();
        let mut report = SearchReport::default();
        match round_socket(&approved) {
            _ if approved.is_empty() => report.error = Some("no approved network".into()),
            Err(e) => report.error = Some(format!("socket: {e}")),
            Ok(socket) => {
                for (i, t) in targets(&approved, &answered, &hints).into_iter().enumerate() {
                    match socket.send_to(&req, SocketAddrV4::new(t, port)) {
                        Ok(_) => report.asked += 1,
                        Err(e) if report.error.is_none() => report.error = Some(format!("send to {t}: {e}")),
                        Err(_) => {}
                    }
                    if i % 64 == 63 {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    report.replies += drain(&socket, &list, own_fp, &mut heard);
                }
                let end = Instant::now() + LISTEN;
                while Instant::now() < end {
                    report.replies += drain(&socket, &list, own_fp, &mut heard);
                }
            }
        }
        report.found = heard.len();
        for fp in seen.keys().filter(|fp| !heard.contains_key(*fp)).copied().collect::<Vec<_>>() {
            seen.remove(&fp);
            merger.set(Source::Probe, fp, None);
        }
        for (fp, peer) in heard {
            if seen.get(&fp) != Some(&peer) {
                seen.insert(fp, peer.clone());
                merger.set(Source::Probe, fp, Some(peer));
            }
        }
        let mut asked = shared.asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        asked.done += 1;
        asked.last = report;
    }
}

/// Reads every reply waiting on `socket` into `heard`; returns how many datagrams it read.
fn drain(
    socket: &UdpSocket,
    list: &[swapdesk_platform::NetInterface],
    own_fp: [u8; FINGERPRINT_BYTES],
    heard: &mut HashMap<[u8; FINGERPRINT_BYTES], Peer>,
) -> usize {
    let mut buf = [0u8; REQUEST_LEN + 1];
    let mut read = 0;
    socket.set_nonblocking(true).ok();
    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(got) => got,
            // Windows reports a port that refused an earlier request as an error of a later read: skip it.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(_) => break,
        };
        read += 1;
        let SocketAddr::V4(from) = from else { continue };
        let Some((txt, port, minor)) = read_reply(&buf[..n]) else { continue };
        if txt.fingerprint == own_fp {
            continue;
        }
        let Ok(link) = link_for_route(list, *from.ip()) else { continue };
        let fp = txt.fingerprint;
        let entry = heard.entry(fp).or_insert_with(|| Peer { txt, port, minor, links: Vec::new() });
        if !entry.links.contains(&link) {
            entry.links.push(link);
            entry.links.sort_by_key(|l| (l.if_index, u32::from(l.remote)));
        }
    }
    socket.set_nonblocking(false).ok();
    std::thread::sleep(Duration::from_millis(1));
    read
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_proto::Os;

    fn own() -> Txt {
        Txt {
            name: "MacBook do escritório".into(),
            os: Os::MacOs,
            fingerprint: [0xAB; FINGERPRINT_BYTES],
            pairing: true,
        }
    }

    fn addr(a: [u8; 4], prefix: u8, kind: IfKind) -> ApprovedAddr {
        ApprovedAddr { if_index: 7, if_name: "wlan0".into(), kind, addr: Ipv4Addr::from(a), prefix }
    }

    #[test]
    fn a_reply_reads_back_and_never_outgrows_the_request() {
        let mut long = own();
        long.name = "x".repeat(crate::txt::MAX_NAME_BYTES);
        for t in [own(), long] {
            let r = reply(&t, 47820, 3).unwrap();
            assert!(r.len() <= REQUEST_LEN, "{} bytes", r.len());
            assert_eq!(read_reply(&r), Some((t, 47820, 3)));
        }
        assert_eq!(request().len(), REQUEST_LEN);
        assert!(is_request(&request()));
        assert!(!is_request(&request()[..100]));
    }

    #[test]
    fn malformed_replies_are_dropped() {
        let r = reply(&own(), 47820, 1).unwrap();
        for bad in [&r[..r.len() - 1], &r[..9], b"SWDPRB1?xxxxxxxx".as_slice(), &[]] {
            assert_eq!(read_reply(bad), None);
        }
        let mut zero = r.clone();
        zero[8] = 0;
        zero[9] = 0;
        assert_eq!(read_reply(&zero), None, "port 0");
        let mut big = r;
        big.resize(REQUEST_LEN + 1, 0);
        assert_eq!(read_reply(&big), None);
    }

    #[test]
    fn a_search_sweeps_the_own_subnet_the_hints_and_the_home_ranges_and_never_this_device() {
        let wifi = addr([10, 0, 2, 16], 24, IfKind::WiFi);
        let full = targets(std::slice::from_ref(&wifi), &[Ipv4Addr::new(10, 9, 9, 9)], &[Ipv4Addr::new(172, 16, 5, 9)]);
        assert!(full.contains(&Ipv4Addr::new(10, 9, 9, 9)), "an address that answered before");
        assert!(!full.contains(&wifi.addr));
        assert!(full.contains(&Ipv4Addr::new(10, 0, 2, 2)));
        assert!(full.contains(&Ipv4Addr::new(172, 16, 5, 200)));
        assert!(full.contains(&Ipv4Addr::new(192, 168, 1, 3)));
        assert!(full.contains(&Ipv4Addr::new(192, 168, 0, 21)));
        assert!(!full.iter().any(|t| t.octets()[3] == 0 || t.octets()[3] == 255));
        assert_eq!(full.len(), 254 * 6 - 1 + 1);
    }

    #[test]
    fn a_small_subnet_is_swept_whole_a_large_one_by_its_slash24_and_mobile_data_skips_the_home_ranges() {
        let small = addr([100, 64, 9, 5], 30, IfKind::Other);
        assert_eq!(targets(&[small], &[], &[]), vec![Ipv4Addr::new(100, 64, 9, 6)]);
        let vpn = addr([10, 8, 0, 2], 32, IfKind::Vpn);
        assert!(targets(&[vpn], &[], &[]).is_empty());
        let large = addr([172, 20, 7, 9], 16, IfKind::Ethernet);
        let t = targets(&[large], &[], &[]);
        assert!(t.contains(&Ipv4Addr::new(172, 20, 7, 1)) && !t.contains(&Ipv4Addr::new(172, 20, 8, 1)));
    }

    /// The responder answers a request with the record it holds now, nothing while it holds none, and ignores
    /// anything that is not a request.
    #[test]
    fn the_responder_answers_requests_only_while_it_holds_a_record() {
        let current = Arc::new(Mutex::new(reply(&own(), 47820, 1)));
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = {
            let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            s.local_addr().unwrap().port()
        };
        let _r = Responder::start(port, Arc::clone(&current)).unwrap();
        probe.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let to = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let mut buf = [0u8; REQUEST_LEN + 1];
        probe.send_to(&request(), to).unwrap();
        let n = probe.recv(&mut buf).unwrap();
        assert_eq!(read_reply(&buf[..n]), Some((own(), 47820, 1)));
        probe.send_to(b"SWDPRB1?", to).unwrap();
        *current.lock().unwrap() = None;
        probe.send_to(&request(), to).unwrap();
        probe.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
        assert!(probe.recv(&mut buf).is_err(), "nothing for a short datagram or a withdrawn record");
    }
}
