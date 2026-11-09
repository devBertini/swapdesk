//! Pairing for the UI (`PairOpen`, `PairStart`, `PairConfirm`, `PairCancel`, PROTOCOL.md 3, D12): the hub opens the
//! pairing endpoints while pairing mode is open, runs `swapdesk-trust`'s responder on them and its initiator on
//! `PairStart`, and turns the ceremony's screen calls into `Notify::Pairing`.
//!
//! One pairing screen at a time per machine (the trust handle has one pairing slot for both roles). The pairing
//! endpoints exist only while the mode is open: the responder's service closes them when it ends.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use quinn::Endpoint;
use swapdesk_ipc::{ApiCode, Notify, PairingState, Text};
use swapdesk_lan::guard::{check_incoming, find_on_link};
use swapdesk_lan::socket::{bind_quic_all_approved, bind_quic_socket};
use swapdesk_platform::Level;
use swapdesk_proto::Sas;
use swapdesk_trust::{
    Decision, InitiateOptions, LocalInfo, ModeOptions, PairOutcome, PairUi, PairingContext, PairingRate,
    PairingService, PeerInfo, ServeOptions, Timeouts, TrustError, connect_and_initiate, device_id_hex,
    endpoint_from_socket, open_pairing_endpoint,
};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

use super::views::{MANUAL_PREFIX, api_of, parse_device_id};
use super::{Hub, lock};

/// How often a busy port is tried again when the endpoints of the last pairing are still closing.
const BIND_TRIES: u32 = 10;

#[derive(Default)]
struct Inner {
    /// The pairing screen's id; `None` when no pairing is open.
    id: Option<u32>,
    peer_name: Option<String>,
    /// The comparison waits for the user's verdict here.
    confirm: Option<oneshot::Sender<bool>>,
    /// The initiator's task.
    task: Option<AbortHandle>,
    endpoints: Vec<Endpoint>,
    cancelled: bool,
}

/// The pairing state of the machine.
#[derive(Default)]
pub(crate) struct PairState {
    inner: Mutex<Inner>,
    /// Serve tasks of the pairing endpoints still running.
    serving: Arc<AtomicUsize>,
    /// One connection attempt per source address per 30 s (PROTOCOL.md 3), across every opening of the mode.
    rate: Arc<PairingRate>,
}

/// The screen side of a ceremony: what the trust crate shows becomes a `Pairing` notification.
pub(crate) struct HubPairUi {
    pub hub: Weak<Hub>,
}

impl PairUi for HubPairUi {
    fn peer_announced(&self, peer: &PeerInfo) {
        let Some(hub) = self.hub.upgrade() else { return };
        let id = {
            let mut s = lock(&hub.pairing.inner);
            s.peer_name = Some(peer.name.clone());
            s.id
        };
        if let Some(id) = id {
            hub.notify(Notify::Pairing { pairing_id: id, state: PairingState::Waiting });
        }
    }

    fn show_code(&self, sas: Sas) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        let Some(hub) = self.hub.upgrade() else { return rx };
        let (id, peer_name) = {
            let mut s = lock(&hub.pairing.inner);
            if s.cancelled || s.id.is_none() {
                // The screen is gone: the sender drops, which is "São diferentes".
                return rx;
            }
            s.confirm = Some(tx);
            (s.id, s.peer_name.clone().unwrap_or_default())
        };
        if let Some(id) = id {
            hub.notify(Notify::Pairing {
                pairing_id: id,
                state: PairingState::ShowCode { code: Text(sas.to_string()), peer_name },
            });
        }
        rx
    }

    fn ended(&self, result: &swapdesk_trust::Result<PairOutcome>) {
        let Some(hub) = self.hub.upgrade() else { return };
        let state = match result {
            Ok(out) => PairingState::Done { device_id: device_id_hex(&out.device.device_id) },
            Err(e) => PairingState::Failed { reason: pair_error_code(e) },
        };
        hub.end_pairing(state);
        if result.is_ok() {
            hub.devices_changed();
        }
    }
}

/// The `ApiCode` a pairing failure is shown as.
fn pair_error_code(e: &TrustError) -> ApiCode {
    match e {
        TrustError::Locked { .. } => ApiCode::Locked,
        TrustError::Timeout(_) => ApiCode::Timeout,
        TrustError::NotInPairingMode => ApiCode::Refused,
        TrustError::UnknownDevice => ApiCode::NotFound,
        other => api_of(other.code()),
    }
}

impl Hub {
    fn pairing_context(self: &Arc<Self>) -> Arc<PairingContext> {
        let s = self.settings();
        Arc::new(PairingContext {
            identity: Arc::clone(&self.p.identity),
            trust: Arc::clone(&self.p.trust),
            mode: Arc::clone(&self.mode),
            wall: Arc::clone(&self.p.wall),
            ui: Arc::new(HubPairUi { hub: Arc::downgrade(self) }),
            trace: Arc::clone(&self.p.trace),
            audit: Arc::new(super::views::HubAudit { hub: Arc::downgrade(self) }),
            local: LocalInfo { name: s.name, os: self.p.os, os_version: self.p.os_version.clone() },
            timeouts: Timeouts::default(),
        })
    }

    /// The addresses the pairing endpoints listen on now.
    pub fn pair_local_addrs(&self) -> Vec<SocketAddr> {
        lock(&self.pairing.inner).endpoints.iter().filter_map(|e| e.local_addr().ok()).collect()
    }

    /// Opens the pairing endpoints on every approved address (and the development addresses) and serves them.
    fn open_pair_endpoints(self: &Arc<Self>, ctx: &Arc<PairingContext>) -> Result<Vec<Endpoint>, ApiCode> {
        let rt = self.rt().ok_or(ApiCode::ServiceDown)?;
        let port = self.settings().pair_port;
        let list = self.p.interfaces.list().unwrap_or_default();
        let mut sockets = Vec::new();
        for (addr, socket) in bind_quic_all_approved(&list, port) {
            match socket {
                Ok(s) => sockets.push(s),
                Err(e) => self.log(
                    Level::Warn,
                    format_args!("cannot open the pairing endpoint on {} ({}): {e}", addr.addr, addr.if_name),
                ),
            }
        }
        for addr in self.p.dev.iter().flat_map(|d| d.pair_listen.iter()) {
            let mut tried = 0;
            loop {
                match bind_quic_socket(*addr) {
                    Ok(s) => {
                        sockets.push(s);
                        break;
                    }
                    // The endpoint of the previous pairing may still be letting go of its port.
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tried < BIND_TRIES => {
                        tried += 1;
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        self.log(Level::Warn, format_args!("cannot open the pairing endpoint on {addr}: {e}"));
                        break;
                    }
                }
            }
        }
        if sockets.is_empty() {
            return Err(ApiCode::Refused);
        }
        let _enter = rt.enter();
        let service = PairingService::with_options(
            Arc::clone(ctx),
            ServeOptions {
                filter: Some(self.pairing_filter()),
                admitted: Some(self.pairing_admitted()),
                ..ServeOptions::default()
            },
        );
        let mut endpoints = Vec::new();
        for socket in sockets {
            let endpoint = open_pairing_endpoint(socket, &self.p.identity).map_err(|e| {
                self.log(Level::Warn, format_args!("pairing endpoint: {e}"));
                ApiCode::Internal
            })?;
            let serving = Arc::clone(&self.pairing.serving);
            serving.fetch_add(1, Ordering::SeqCst);
            let (service, e2) = (Arc::clone(&service), endpoint.clone());
            rt.spawn(async move {
                service.serve(e2).await;
                serving.fetch_sub(1, Ordering::SeqCst);
            });
            endpoints.push(endpoint);
        }
        Ok(endpoints)
    }

    /// The filter of the pairing endpoint: the LAN guard (PROTOCOL.md 1: on-link, not loopback, not this machine),
    /// then one connection attempt per source address per 30 s (PROTOCOL.md 3). A source that is not validated yet
    /// gets `retry()` and costs its turn nothing; a second attempt inside the interval is ignored before any
    /// handshake and without an audit entry.
    fn pairing_filter(&self) -> swapdesk_trust::IncomingFilter {
        let (nets, allow_loopback) = (Arc::clone(&self.p.interfaces), self.allow_loopback());
        let rate = Arc::clone(&self.pairing.rate);
        Arc::new(move |incoming| {
            let remote = incoming.remote_address().ip();
            let local = incoming.local_ip();
            let on_link = if allow_loopback && remote.is_loopback() && local.is_some_and(|l| l.is_loopback()) {
                true
            } else if let (Ok(list), Some(local)) = (nets.list(), local) {
                check_incoming(&list, local, remote).is_ok()
            } else {
                false
            };
            if !on_link {
                return Decision::Refuse;
            }
            rate.judge(incoming)
        })
    }

    /// Where a source address's 30 s turn is spent: only for an attempt the pairing service admitted past its cap.
    fn pairing_admitted(&self) -> swapdesk_trust::AdmittedHook {
        let rate = Arc::clone(&self.pairing.rate);
        Arc::new(move |incoming| rate.note(incoming))
    }

    /// `PairOpen`: pairing mode for 2 minutes (D12), the endpoints, the `pair=1` announcement, the screen's id.
    pub fn pair_open(self: &Arc<Self>, device_id: Option<String>) -> Result<u32, ApiCode> {
        let rt = self.rt().ok_or(ApiCode::ServiceDown)?;
        if self.p.trust.pairing_in_progress() {
            return Err(ApiCode::Busy);
        }
        if self.p.trust.read(|s| s.lockout.remaining_ms(self.p.wall.unix_ms())).is_some() {
            return Err(ApiCode::Locked);
        }
        let expect = match device_id.as_deref() {
            None => None,
            // A typed address names no key: the pairing learns it.
            Some(t) if t.starts_with(MANUAL_PREFIX) => None,
            Some(t) => Some(parse_device_id(t).ok_or(ApiCode::Invalid)?),
        };
        let ctx = self.pairing_context();
        let id = {
            let mut s = lock(&self.pairing.inner);
            self.mode.enter(ModeOptions { expect, replaces: None });
            if self.pairing.serving.load(Ordering::SeqCst) == 0 {
                match self.open_pair_endpoints(&ctx) {
                    Ok(endpoints) => s.endpoints = endpoints,
                    Err(code) => {
                        self.mode.close();
                        return Err(code);
                    }
                }
            }
            let id = *s.id.get_or_insert_with(|| self.fresh_id());
            s.cancelled = false;
            s.peer_name = None;
            id
        };
        self.p.presence.set_pairing(true);
        let me = Arc::clone(self);
        rt.spawn(async move { me.watch_pairing_mode(id).await });
        self.notify(Notify::Pairing { pairing_id: id, state: PairingState::Open { remaining_s: self.remaining_s() } });
        Ok(id)
    }

    fn remaining_s(&self) -> u16 {
        u16::try_from(self.mode.remaining_micros().div_ceil(1_000_000)).unwrap_or(u16::MAX)
    }

    /// Ends the screen when the 2 minutes pass with nothing started.
    async fn watch_pairing_mode(self: Arc<Self>, id: u32) {
        loop {
            let left = self.mode.remaining_micros();
            if left == 0 && !self.p.trust.pairing_in_progress() {
                break;
            }
            // A ceremony that began in time may outlast the mode (the comparison has its own limit).
            tokio::time::sleep(Duration::from_micros(left.max(1_000_000)) + Duration::from_millis(20)).await;
        }
        let expired = {
            let s = lock(&self.pairing.inner);
            s.id == Some(id) && !s.cancelled
        };
        if expired {
            self.end_pairing_of(id, PairingState::Failed { reason: ApiCode::Timeout });
        }
    }

    /// The current pairing ended with `state`.
    pub fn end_pairing(self: &Arc<Self>, state: PairingState) {
        let id = lock(&self.pairing.inner).id;
        if let Some(id) = id {
            self.end_pairing_of(id, state);
        }
    }

    fn end_pairing_of(self: &Arc<Self>, id: u32, state: PairingState) {
        {
            let mut s = lock(&self.pairing.inner);
            if s.id != Some(id) {
                return;
            }
            s.id = None;
            s.confirm = None;
            s.task = None;
            s.peer_name = None;
            s.endpoints.clear();
        }
        self.mode.close();
        self.p.presence.set_pairing(false);
        self.notify(Notify::Pairing { pairing_id: id, state });
    }

    /// `PairStart`: this machine's user picked `device_id`; run the initiator against it.
    pub fn pair_start(self: &Arc<Self>, pairing_id: u32, device_id: &str) -> Result<(), ApiCode> {
        let rt = self.rt().ok_or(ApiCode::ServiceDown)?;
        if lock(&self.pairing.inner).id != Some(pairing_id) {
            return Err(ApiCode::Invalid);
        }
        if lock(&self.pairing.inner).task.is_some() {
            return Err(ApiCode::Busy);
        }
        // Inside pairing mode only (D12): the service never starts a pairing its user did not open.
        if !self.mode.is_open() {
            return Err(ApiCode::Refused);
        }
        let (ip, pair_port, expect) = self.pair_target(device_id)?;
        let manual = device_id.starts_with(MANUAL_PREFIX);
        let local = if self.allow_loopback() && ip.is_loopback() {
            Ipv4Addr::LOCALHOST
        } else {
            let list = self.p.interfaces.list().map_err(|_| ApiCode::Refused)?;
            find_on_link(&list, ip).map_err(|_| ApiCode::Refused)?.local
        };
        let ctx = self.pairing_context();
        let me = Arc::clone(self);
        let task = rt.spawn(async move {
            let result = async {
                let socket =
                    bind_quic_socket(SocketAddrV4::new(local, 0)).map_err(|e| TrustError::Tls(e.to_string()))?;
                let endpoint = endpoint_from_socket(socket, None)?;
                connect_and_initiate(
                    &endpoint,
                    &ctx,
                    SocketAddr::V4(SocketAddrV4::new(ip, pair_port)),
                    InitiateOptions { expect, replaces: None },
                )
                .await
            }
            .await;
            match result {
                // A device paired through a typed address is dialled there later (its session port is the one
                // before the pairing port), not at the default port of its stored address.
                Ok(out) if manual => {
                    lock(&me.learned).insert(out.device.device_id, SocketAddrV4::new(ip, pair_port.saturating_sub(1)));
                    me.devices_changed();
                }
                Ok(_) => {}
                // A failure before the ceremony never reached the screen's `ended`; this is its only report. After
                // a ceremony the id is already gone and the call does nothing.
                Err(e) => me.end_pairing_of(pairing_id, PairingState::Failed { reason: pair_error_code(&e) }),
            }
        });
        lock(&self.pairing.inner).task = Some(task.abort_handle());
        self.notify(Notify::Pairing { pairing_id, state: PairingState::Waiting });
        Ok(())
    }

    /// The address and key a `PairStart` aims at: a device seen on the network, or an address the user typed.
    fn pair_target(&self, device_id: &str) -> Result<(Ipv4Addr, u16, Option<[u8; 16]>), ApiCode> {
        if device_id.starts_with(MANUAL_PREFIX) {
            let m = lock(&self.manual).iter().find(|m| m.id() == device_id).cloned().ok_or(ApiCode::NotFound)?;
            return Ok((m.addr, m.pair_port(), None));
        }
        let id = parse_device_id(device_id).ok_or(ApiCode::Invalid)?;
        let peer = self.p.presence.peers().into_iter().find(|p| p.device_id == id).ok_or(ApiCode::NotFound)?;
        let ip = *peer.addrs.first().ok_or(ApiCode::NotFound)?;
        Ok((ip, peer.pair_port, Some(id)))
    }

    /// `PairConfirm`: the user's verdict on the code.
    pub fn pair_confirm(&self, pairing_id: u32, matches: bool) -> Result<(), ApiCode> {
        let tx = {
            let mut s = lock(&self.pairing.inner);
            if s.id != Some(pairing_id) {
                return Err(ApiCode::NotFound);
            }
            s.confirm.take()
        };
        tx.ok_or(ApiCode::NotFound)?.send(matches).map_err(|_| ApiCode::NotFound)
    }

    /// `PairCancel`: the screen closed. A ceremony past the nonce counts as a failed attempt (F-PAIR-02).
    pub fn pair_cancel(self: &Arc<Self>, pairing_id: u32) -> Result<(), ApiCode> {
        if lock(&self.pairing.inner).id != Some(pairing_id) {
            return Err(ApiCode::NotFound);
        }
        self.cancel_pairing();
        Ok(())
    }

    /// Closes pairing mode and whatever runs for it, silently.
    pub fn cancel_pairing(&self) {
        let (task, endpoints) = {
            let mut s = lock(&self.pairing.inner);
            let had = s.id.take().is_some();
            s.cancelled = had;
            s.confirm = None;
            s.peer_name = None;
            (s.task.take(), std::mem::take(&mut s.endpoints))
        };
        if let Some(t) = task {
            t.abort();
        }
        self.mode.close();
        for e in endpoints {
            e.close(quinn::VarInt::from_u32(0), b"pairing closed");
        }
        self.p.presence.set_pairing(false);
    }
}
