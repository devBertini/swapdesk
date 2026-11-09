//! Endpoint setup: the session endpoint, and the pairing endpoint that exists only while pairing mode is open
//! (PROTOCOL.md 1 and 3). Socket options (TTL 1, the LAN guard, address checks) belong to `swapdesk-lan`; these
//! functions take the socket the caller made.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use quinn::{Endpoint, EndpointConfig, Incoming, ServerConfig};
use swapdesk_platform::Level;
use swapdesk_proto::ErrorCode;

use crate::error::{Result, TrustError};
use crate::gate::{gate_session_client, gate_session_server};
use crate::identity::Identity;
use crate::ids::{DeviceId, Spki};
use crate::pairing::{InitiateOptions, PairOutcome, PairingContext, initiate, respond};
use crate::store::PinLookup;
use crate::tls::{
    SERVER_NAME, pairing_client_config, pairing_server_config, session_client_config, session_server_config,
};

/// A quinn endpoint on a socket the caller made, optionally accepting connections.
pub fn endpoint_from_socket(socket: UdpSocket, server: Option<ServerConfig>) -> Result<Endpoint> {
    Endpoint::new(
        EndpointConfig::default(),
        server,
        socket,
        quinn::default_runtime().ok_or_else(|| TrustError::Tls("no async runtime".into()))?,
    )
    .map_err(|e| TrustError::Tls(e.to_string()))
}

/// The session endpoint (pinned keys only), open while the service runs.
pub fn open_session_endpoint(socket: UdpSocket, identity: &Identity, trust: Arc<dyn PinLookup>) -> Result<Endpoint> {
    endpoint_from_socket(socket, Some(session_server_config(identity, trust)?))
}

/// The pairing endpoint. Call it only when pairing mode opens, and close the endpoint when it ends.
pub fn open_pairing_endpoint(socket: UdpSocket, identity: &Identity) -> Result<Endpoint> {
    endpoint_from_socket(socket, Some(pairing_server_config(identity)?))
}

const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);

/// Session client: connect to `addr` expecting the device whose key is `expected`, then run the gate.
pub async fn connect_session(
    endpoint: &Endpoint,
    identity: &Identity,
    addr: SocketAddr,
    expected: Spki,
) -> Result<quinn::Connection> {
    let connecting = endpoint
        .connect_with(session_client_config(identity, expected)?, addr, SERVER_NAME)
        .map_err(|e| TrustError::Tls(e.to_string()))?;
    let conn = tokio::time::timeout(HANDSHAKE_LIMIT, connecting)
        .await
        .map_err(|_| TrustError::Timeout("handshake"))?
        .map_err(|e| TrustError::Tls(e.to_string()))?;
    gate_session_client(&conn, &expected)?;
    Ok(conn)
}

/// Session server: finish the handshake of an accepted `Incoming`, then run the gate before any stream.
/// Returns the connection and the pinned device it belongs to.
pub async fn accept_session(incoming: Incoming, trust: &dyn PinLookup) -> Result<(quinn::Connection, DeviceId)> {
    let connecting = incoming.accept().map_err(|e| TrustError::Tls(e.to_string()))?;
    let conn = tokio::time::timeout(HANDSHAKE_LIMIT, connecting)
        .await
        .map_err(|_| TrustError::Timeout("handshake"))?
        .map_err(|e| TrustError::Tls(e.to_string()))?;
    let id = gate_session_server(&conn, trust)?;
    Ok((conn, id))
}

/// Pairing client: connect to the responder's pairing endpoint and run the initiator's side.
pub async fn connect_and_initiate(
    endpoint: &Endpoint,
    ctx: &PairingContext,
    addr: SocketAddr,
    options: InitiateOptions,
) -> Result<PairOutcome> {
    ctx.mode.current().ok_or(TrustError::NotInPairingMode)?;
    // One pairing at a time per machine, in either role: no handshake is spent while one runs.
    if ctx.trust.pairing_in_progress() {
        return Err(TrustError::Protocol(ErrorCode::Busy));
    }
    let connecting = endpoint
        .connect_with(pairing_client_config(&ctx.identity)?, addr, SERVER_NAME)
        .map_err(|e| TrustError::Tls(e.to_string()))?;
    let conn = tokio::time::timeout(HANDSHAKE_LIMIT, connecting)
        .await
        .map_err(|_| TrustError::Timeout("handshake"))?
        .map_err(|e| TrustError::Tls(e.to_string()))?;
    initiate(ctx, &conn, options).await
}

/// What the owner of the pre-handshake checks (PROTOCOL.md 1: on-link, not loopback, not this machine, per-address
/// rate) decides for one `Incoming`, before any handshake is spent on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Go on: the handshake may run.
    Accept,
    /// Answer with a refusal.
    Refuse,
    /// Send nothing back (a blocked address, or over the hard limit).
    Ignore,
    /// Ask the sender to prove its address first (over the soft limit); an address already proved is ignored.
    Retry,
}

/// The pre-handshake filter: sees the `Incoming` (its `remote_address()`, `local_ip()`, `remote_address_validated()`)
/// and nothing else has happened yet.
pub type IncomingFilter = Arc<dyn Fn(&Incoming) -> Decision + Send + Sync>;

/// Called with an `Incoming` the service has admitted past every check, its in-flight cap included, before the
/// handshake runs: where a per-address turn is recorded (a turn dropped by the cap is not spent).
pub type AdmittedHook = Arc<dyn Fn(&Incoming) + Send + Sync>;

/// The load limit of PROTOCOL.md 1: at most 16 handshakes in flight per machine.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 16;

/// How a [`PairingService`] guards its accept loop.
#[derive(Clone)]
pub struct ServeOptions {
    /// Connections being handshaken or answered at once; one more is ignored (the hard limit).
    pub max_in_flight: usize,
    /// Runs on every `Incoming` before anything else of this service looks at it. `None` accepts all.
    pub filter: Option<IncomingFilter>,
    /// Runs on every `Incoming` that passed the filter and got a place under `max_in_flight`.
    pub admitted: Option<AdmittedHook>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self { max_in_flight: DEFAULT_MAX_IN_FLIGHT, filter: None, admitted: None }
    }
}

/// The responder's pairing service: answers connections on the pairing endpoint while pairing mode is open,
/// one pairing at a time, and closes the endpoint when the mode ends and no connection is being served.
pub struct PairingService {
    ctx: Arc<PairingContext>,
    options: ServeOptions,
    in_flight: AtomicUsize,
}

/// One connection's place under the cap, given back when it is dropped.
struct InFlight(Arc<PairingService>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

const ACCEPT_POLL: Duration = Duration::from_millis(100);

impl PairingService {
    pub fn new(ctx: Arc<PairingContext>) -> Arc<Self> {
        Self::with_options(ctx, ServeOptions::default())
    }

    pub fn with_options(ctx: Arc<PairingContext>, options: ServeOptions) -> Arc<Self> {
        Arc::new(Self { ctx, options, in_flight: AtomicUsize::new(0) })
    }

    /// Connections being served right now.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    fn reserve(self: &Arc<Self>) -> Option<InFlight> {
        let max = self.options.max_in_flight;
        self.in_flight.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < max).then_some(n + 1)).ok()?;
        Some(InFlight(Arc::clone(self)))
    }

    /// Serves `endpoint` until pairing mode ends and nothing is being served. Every `Incoming` is judged before a
    /// handshake is spent on it: with the mode closed it is ignored (nothing answers an unsolicited pairing, so
    /// nothing says anyone listens), then the filter decides, then the cap on concurrent handshakes.
    pub async fn serve(self: Arc<Self>, endpoint: Endpoint) {
        loop {
            let open = self.ctx.mode.is_open();
            if !open && self.in_flight() == 0 {
                break;
            }
            match tokio::time::timeout(ACCEPT_POLL, endpoint.accept()).await {
                Err(_) => {}
                Ok(None) => return,
                Ok(Some(incoming)) => self.dispatch(incoming),
            }
        }
        endpoint.close(quinn::VarInt::from_u32(u32::from(ErrorCode::NoError.tag())), b"pairing mode ended");
    }

    fn dispatch(self: &Arc<Self>, incoming: Incoming) {
        if !self.ctx.mode.is_open() {
            return incoming.ignore();
        }
        match self.options.filter.as_ref().map_or(Decision::Accept, |f| f(&incoming)) {
            Decision::Accept => {}
            Decision::Refuse => return incoming.refuse(),
            Decision::Ignore => return incoming.ignore(),
            Decision::Retry => {
                if let Err(e) = incoming.retry() {
                    e.into_incoming().ignore();
                }
                return;
            }
        }
        let Some(place) = self.reserve() else { return incoming.ignore() };
        if let Some(admitted) = &self.options.admitted {
            admitted(&incoming);
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.handle(incoming).await;
            drop(place);
        });
    }

    async fn handle(&self, incoming: Incoming) {
        let ctx = &self.ctx;
        let conn = match incoming.accept() {
            Ok(connecting) => match tokio::time::timeout(HANDSHAKE_LIMIT, connecting).await {
                Ok(Ok(conn)) => Some(conn),
                _ => None,
            },
            Err(_) => None,
        };
        match conn {
            Some(conn) => {
                let _ = respond(ctx, &conn).await;
            }
            None => ctx.trace.log(Level::Debug, "pair", format_args!("a pairing handshake did not finish")),
        }
    }
}
