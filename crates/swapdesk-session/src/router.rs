//! Incoming QUIC streams (PROTOCOL.md 4): every stream starts with its kind; the session reads it, checks it is a
//! kind this side may receive, that the session is live and that the permission is held, and hands it to the state
//! machine, which gives it to the handler that owns the kind (after `on_start`, in order with everything else the
//! handler hears). A stream before the grant, of a kind the peer may not open, or of an unknown kind is a fatal
//! violation.

use quinn::{RecvStream, SendStream, VarInt};
use swapdesk_proto::{ErrorCode, ErrorMessage, StreamKind, Wire};
use tokio::task::AbortHandle;

use crate::ctx::{Ev, SessionCtx};
use crate::link::FRAME_STEP_TIMEOUT;
use crate::policy::{Role, stream_allowed, stream_required};
use crate::registry::{IncomingStream, Registry};

/// On the viewer, a stream of the host may overtake the message that announces the grant; past this wait it is a
/// stream before the grant. (The host sets the session live before it says `SessionGrant`, so the viewer's own
/// streams never need it: on the host a stream before the grant is a violation at once.)
const GRANT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

fn code(c: ErrorCode) -> VarInt {
    VarInt::from_u32(u32::from(c.tag()))
}

/// Starts the two acceptors of a connection (the control stream is already taken). Returns their handles so the
/// session can stop them.
pub(crate) fn spawn_acceptors(ctx: &SessionCtx) -> Vec<AbortHandle> {
    let Some(conn) = ctx.connection().cloned() else { return Vec::new() };
    let uni = {
        let (ctx, conn) = (ctx.clone(), conn.clone());
        tokio::spawn(async move {
            while let Ok(recv) = conn.accept_uni().await {
                let ctx = ctx.clone();
                tokio::spawn(async move { route(ctx, recv, None).await });
            }
        })
    };
    let bi = {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            while let Ok((send, recv)) = conn.accept_bi().await {
                let ctx = ctx.clone();
                tokio::spawn(async move { route(ctx, recv, Some(send)).await });
            }
        })
    };
    vec![uni.abort_handle(), bi.abort_handle()]
}

/// The kind a stream starts with. `Ok(None)`: the peer reset the stream (or the connection went) before its kind was
/// read, which a sender does on purpose with a stale video frame (PROTOCOL.md 4): not a violation, the stream is just
/// gone. A stream that ends without a kind, or stalls before it, is a peer that is not following the format.
async fn read_kind(recv: &mut RecvStream) -> Result<Option<StreamKind>, ErrorCode> {
    use quinn::ReadExactError::{FinishedEarly, ReadError};
    let mut first = [0u8; 1];
    match tokio::time::timeout(FRAME_STEP_TIMEOUT, recv.read_exact(&mut first)).await {
        Ok(Ok(())) => {}
        Ok(Err(ReadError(_))) => return Ok(None),
        Ok(Err(FinishedEarly(_))) | Err(_) => return Err(ErrorCode::ProtocolViolation),
    }
    let mut buf = vec![first[0]];
    let rest = (1usize << (first[0] >> 6)) - 1;
    if rest > 0 {
        let mut more = [0u8; 7];
        match tokio::time::timeout(FRAME_STEP_TIMEOUT, recv.read_exact(&mut more[..rest])).await {
            Ok(Ok(())) => buf.extend_from_slice(&more[..rest]),
            Ok(Err(ReadError(_))) => return Ok(None),
            Ok(Err(FinishedEarly(_))) | Err(_) => return Err(ErrorCode::ProtocolViolation),
        }
    }
    StreamKind::decode(&buf).map(Some).map_err(|e| e.code())
}

/// Stops a stream whose permission is not held: `NOT_PERMITTED` to the peer's stream and in a control message.
fn refuse(ctx: &SessionCtx, stream: &mut IncomingStream, why: ErrorCode) {
    let _ = stream.recv.stop(code(why));
    if let Some(s) = stream.send.as_mut() {
        let _ = s.reset(code(why));
    }
    if why == ErrorCode::NotPermitted {
        let _ = ctx.try_send(ErrorMessage { code: ErrorCode::NotPermitted, fatal: false, about_type: 0, about_id: 0 });
    }
}

/// Whether the session holds what a stream of this kind needs, on either side: the host checks what the viewer may
/// send, the viewer what the host may send it (a clipboard or file stream needs the grant on both).
fn permitted(ctx: &SessionCtx, stream: &IncomingStream) -> bool {
    ctx.granted().contains(stream_required(stream.kind))
}

async fn route(ctx: SessionCtx, mut recv: RecvStream, send: Option<SendStream>) {
    let kind = match read_kind(&mut recv).await {
        Ok(Some(k)) => k,
        Ok(None) => return,
        Err(c) => return ctx.violation(c),
    };
    let bidi = send.is_some();
    // Nothing but the session and keepalive messages flows before the grant, and no other stream may be opened.
    let live = stream_allowed(ctx.role(), kind, bidi)
        && match ctx.role() {
            Role::Host => ctx.is_active(),
            Role::Viewer => ctx.wait_active(GRANT_WAIT).await,
        };
    if !live {
        return ctx.violation(ErrorCode::ProtocolViolation);
    }
    let mut stream = IncomingStream { kind, recv, send };
    if !permitted(&ctx, &stream) {
        return refuse(&ctx, &mut stream, ErrorCode::NotPermitted);
    }
    // The state machine hands it to the handler, after `on_start` and in order with the handler's other calls. A
    // session that is gone drops it, which stops it.
    let _ = ctx.inner.ev.send(Ev::Stream(stream)).await;
}

/// On the state machine's task: the stream of a live session goes to the handler that owns its kind. The grants are
/// looked at again, since they may have changed while the stream waited in the queue.
pub(crate) fn dispatch(ctx: &SessionCtx, registry: &Registry, mut stream: IncomingStream) {
    if !permitted(ctx, &stream) {
        return refuse(ctx, &mut stream, ErrorCode::NotPermitted);
    }
    match registry.for_stream(stream.kind) {
        Some(h) => h.on_stream(ctx, stream),
        None => refuse(ctx, &mut stream, ErrorCode::Unavailable),
    }
}
