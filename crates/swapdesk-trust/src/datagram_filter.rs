//! A UDP socket that drops datagrams before quinn parses them (F5 of the 13/11 security review).
//!
//! quinn answers some datagrams on its own, before the application sees an `Incoming`: Version Negotiation for an
//! unknown version, a stateless reset for a packet of no connection, a Retry for an Initial. A device that is blocked,
//! or that sits on a network the owner did not approve, must get no answer at all (PROTOCOL.md 1, D65). So the
//! endpoint's socket is wrapped: every received datagram goes through a [`DatagramFilter`], and one the filter
//! refuses is dropped silently, as if it had never arrived.
//!
//! The filter runs for every datagram of every live session, on a runtime worker: it must be cheap and never wait for
//! the OS.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, Endpoint, EndpointConfig, ServerConfig, UdpPoller};

use crate::error::{Result, TrustError};

/// Decides, from a datagram's source address and the local address it arrived on (`None` when the platform did not
/// say), whether quinn may see it. `false` drops it silently.
pub type DatagramFilter = Arc<dyn Fn(IpAddr, Option<IpAddr>) -> bool + Send + Sync>;

/// Batches in a row that were dropped entirely before the socket yields the worker: a flood of refused datagrams
/// must not keep one task from letting others run.
const DROPPED_BATCHES_BEFORE_YIELD: usize = 64;

struct FilteredSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    keep: DatagramFilter,
}

impl fmt::Debug for FilteredSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilteredSocket").field("inner", &self.inner).finish_non_exhaustive()
    }
}

impl AsyncUdpSocket for FilteredSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        for _ in 0..DROPPED_BATCHES_BEFORE_YIELD {
            let received = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => n,
                other => return other,
            };
            // Keep the accepted datagrams at the front: quinn reads `meta[..n]` and `bufs[..n]`.
            let mut kept = 0;
            for i in 0..received {
                if (self.keep)(meta[i].addr.ip(), meta[i].dst_ip) {
                    if i != kept {
                        bufs.swap(i, kept);
                        meta.swap(i, kept);
                    }
                    kept += 1;
                }
            }
            if kept > 0 {
                return Poll::Ready(Ok(kept));
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// [`crate::endpoint_from_socket`] with `keep` between the socket and quinn. Needs a tokio runtime, like it.
pub fn endpoint_from_socket_filtered(
    socket: UdpSocket,
    server: Option<ServerConfig>,
    keep: DatagramFilter,
) -> Result<Endpoint> {
    let runtime = quinn::default_runtime().ok_or_else(|| TrustError::Tls("no async runtime".into()))?;
    let inner = runtime.wrap_udp_socket(socket).map_err(|e| TrustError::Tls(e.to_string()))?;
    Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        server,
        Arc::new(FilteredSocket { inner, keep }),
        runtime,
    )
    .map_err(|e| TrustError::Tls(e.to_string()))
}
