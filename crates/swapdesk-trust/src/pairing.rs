//! Pairing by compared codes (PROTOCOL.md 3, decisions D5 and D12): a hash commitment, both nonces, the TLS
//! exporter and both keys feed one 6-digit code that both users compare. The peer is pinned only after both
//! press "Os códigos são iguais". The responder writes a provisional failed attempt to disk before it sends its
//! nonce, so a crash or a closed screen counts as a failure.
//!
//! Nothing here logs a nonce, a key, the exporter output or the code.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::{RecvStream, SendStream};
use ring::rand::{SecureRandom, SystemRandom};
use swapdesk_platform::{Level, TraceSink};
use swapdesk_proto::{
    CommitmentInput, EXPORTER_LABEL, EXPORTER_LEN, ErrorCode, FrameHeader, MAX_PAIR_MAJORS, Message, NONCE_LEN, Os,
    PROTOCOL_VERSION, PairConfirm, PairHello, PairNonce, PairResult, PairReveal, Sas, SasInput, Wire,
};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::audit::{AuditEntry, AuditEvent, AuditSink, DeviceRef};
use crate::clock::WallClock;
use crate::error::{Result, TrustError};
use crate::gate::gate_pairing;
use crate::identity::Identity;
use crate::ids::{DeviceId, Spki, device_id, device_id_hex, sha256};
use crate::mode::PairingMode;
use crate::records::{MAX_DEVICES, PairedDevice, TrustStore};
use crate::store::{PairingGuard, PinLookup, TrustHandle};

/// What this machine tells the peer about itself (`PairHello` / `PairNonce`).
#[derive(Debug, Clone)]
pub struct LocalInfo {
    pub name: String,
    pub os: Os,
    pub os_version: String,
}

/// What the peer announced, and its key.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub name: String,
    pub os: Os,
    pub os_version: String,
    pub device_id: DeviceId,
    pub spki: Spki,
}

/// The screen side of a pairing: the code to show and the user's verdict.
pub trait PairUi: Send + Sync {
    /// The responder's screen shows "Parear com <peer>" (step 2). Called on the initiator when `PairNonce`
    /// arrives.
    fn peer_announced(&self, peer: &PeerInfo);

    /// Shows the six digits (step 4). The answer is `true` when the user pressed "Os códigos são iguais" and
    /// `false` for "São diferentes"; a dropped sender counts as `false`.
    fn show_code(&self, sas: Sas) -> oneshot::Receiver<bool>;

    /// The attempt ended; `Ok` means the peer is pinned.
    fn ended(&self, result: &Result<PairOutcome>);
}

/// Step limits (PROTOCOL.md 3, GUESS values).
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// From the handshake to the reveal (step 3).
    pub handshake_to_reveal: Duration,
    /// The users' comparison (step 5).
    pub comparison: Duration,
    /// How long to wait for the peer's `PairResult` after our own.
    pub result: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake_to_reveal: Duration::from_secs(10),
            comparison: Duration::from_secs(120),
            result: Duration::from_secs(5),
        }
    }
}

/// A finished pairing.
#[derive(Debug, Clone)]
pub struct PairOutcome {
    /// The pinned device, with no grants.
    pub device: PairedDevice,
    /// The entry this ceremony replaced ("chave substituída"), if the user asked for a re-pairing.
    pub replaced: Option<DeviceId>,
}

/// Everything a pairing needs from the service.
pub struct PairingContext {
    pub identity: Arc<Identity>,
    pub trust: Arc<TrustHandle>,
    pub mode: Arc<PairingMode>,
    pub wall: Arc<dyn WallClock>,
    pub ui: Arc<dyn PairUi>,
    pub trace: Arc<dyn TraceSink>,
    pub audit: Arc<dyn AuditSink>,
    pub local: LocalInfo,
    pub timeouts: Timeouts,
}

/// The initiator's choices when it starts.
#[derive(Debug, Clone, Copy, Default)]
pub struct InitiateOptions {
    /// The device the user picked in "Parear": the responder's key must be this one.
    pub expect: Option<DeviceId>,
    /// The ceremony replaces this entry.
    pub replaces: Option<DeviceId>,
}

// ---------- pure parts: commitment and code ----------

/// `commit = H("swapdesk commit v1" || spki_I || nonce_I)` (step 1).
pub fn commitment(spki_i: &Spki, nonce_i: &[u8; NONCE_LEN]) -> [u8; 32] {
    sha256(&CommitmentInput { spki_i, nonce_i }.preimage())
}

/// The six digits both users compare (step 4).
pub fn compute_sas(
    ek: &[u8; EXPORTER_LEN],
    spki_i: &Spki,
    spki_r: &Spki,
    nonce_i: &[u8; NONCE_LEN],
    nonce_r: &[u8; NONCE_LEN],
) -> Sas {
    Sas::from_digest(&sha256(&SasInput { ek, spki_i, spki_r, nonce_i, nonce_r }.preimage()))
}

fn random_nonce() -> Result<[u8; NONCE_LEN]> {
    let mut n = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut n).map_err(|_| TrustError::Crypto("random"))?;
    Ok(n)
}

fn export(conn: &quinn::Connection) -> Result<[u8; EXPORTER_LEN]> {
    let mut ek = [0u8; EXPORTER_LEN];
    conn.export_keying_material(&mut ek, EXPORTER_LABEL, b"").map_err(|_| TrustError::Crypto("exporter"))?;
    Ok(ek)
}

// ---------- framing on the pairing stream (section 5.1) ----------

async fn within<T>(deadline: Instant, what: &'static str, fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout_at(deadline, fut).await.map_err(|_| TrustError::Timeout(what))?
}

fn lost<E>(_: E) -> TrustError {
    TrustError::Tls("pairing stream closed".into())
}

async fn write_msg(send: &mut SendStream, msg: impl Into<Message>) -> Result<()> {
    let frame = msg.into().encode_frame()?;
    send.write_all(&frame).await.map_err(lost)
}

/// One varint's bytes off the stream (1, 2, 4 or 8 bytes); the codec checks minimality afterwards.
async fn read_varint_bytes(recv: &mut RecvStream, out: &mut Vec<u8>) -> Result<()> {
    let mut first = [0u8; 1];
    recv.read_exact(&mut first).await.map_err(lost)?;
    out.push(first[0]);
    let rest = (1usize << (first[0] >> 6)) - 1;
    let mut buf = [0u8; 7];
    recv.read_exact(&mut buf[..rest]).await.map_err(lost)?;
    out.extend_from_slice(&buf[..rest]);
    Ok(())
}

/// One framed message; the header is checked (known type, length limit) before the payload is read.
async fn read_msg(recv: &mut RecvStream) -> Result<Message> {
    let mut head = Vec::with_capacity(4);
    read_varint_bytes(recv, &mut head).await?;
    read_varint_bytes(recv, &mut head).await?;
    let header = FrameHeader::decode(&head)?;
    let mut payload = vec![0u8; header.len];
    recv.read_exact(&mut payload).await.map_err(lost)?;
    let msg = Message::decode_payload(u64::from(header.type_id), &payload)?;
    if msg.is_pairing() { Ok(msg) } else { Err(TrustError::Protocol(ErrorCode::ProtocolViolation)) }
}

/// Sends `PairResult { ok: false, code }` and lets it reach the peer before the connection closes.
async fn refuse(send: &mut SendStream, code: ErrorCode) {
    let _ = write_msg(send, PairResult { ok: false, code }).await;
    end_stream(send).await;
}

async fn end_stream(send: &mut SendStream) {
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(1), send.stopped()).await;
}

fn remote_v4(conn: &quinn::Connection) -> [u8; 4] {
    match conn.remote_address() {
        SocketAddr::V4(a) => a.ip().octets(),
        SocketAddr::V6(_) => [0; 4],
    }
}

// ---------- the shared half of the ceremony ----------

struct Ceremony<'a> {
    ctx: &'a PairingContext,
    conn: &'a quinn::Connection,
    send: SendStream,
    recv: RecvStream,
    spki_i: Spki,
    spki_r: Spki,
    nonce_i: [u8; NONCE_LEN],
    nonce_r: [u8; NONCE_LEN],
    peer: PeerInfo,
    replaces: Option<DeviceId>,
}

impl Ceremony<'_> {
    /// Steps 4 to 6: compute the code, show it, exchange the verdicts, pin.
    async fn run(mut self) -> Result<PairOutcome> {
        let ctx = self.ctx;
        let ek = export(self.conn)?;
        let sas = compute_sas(&ek, &self.spki_i, &self.spki_r, &self.nonce_i, &self.nonce_r);
        let deadline = Instant::now() + ctx.timeouts.comparison;
        ctx.trace.log(
            Level::Info,
            "pair",
            format_args!("code shown to the user, peer {}", device_id_hex(&self.peer.device_id)),
        );
        let verdict = ctx.ui.show_code(sas);

        // The local user's answer; no answer in time is a failed attempt.
        let local = match tokio::time::timeout_at(deadline, verdict).await {
            Ok(Ok(matches)) => matches,
            Ok(Err(_)) => false,
            Err(_) => {
                refuse(&mut self.send, ErrorCode::Timeout).await;
                return Err(TrustError::Timeout("comparison"));
            }
        };
        write_msg(&mut self.send, PairConfirm { matches: local }).await?;
        if !local {
            refuse(&mut self.send, ErrorCode::SasRejected).await;
            return Err(TrustError::SasRejected);
        }

        // The peer's verdict. Its screen shows nothing about ours, so this waits for its user.
        let remote = match within(deadline, "peer comparison", read_msg(&mut self.recv)).await {
            Ok(Message::PairConfirm(c)) => c.matches,
            Ok(Message::PairResult(r)) if !r.ok => return Err(TrustError::Refused(r.code)),
            Ok(_) => {
                refuse(&mut self.send, ErrorCode::ProtocolViolation).await;
                return Err(TrustError::Protocol(ErrorCode::ProtocolViolation));
            }
            Err(TrustError::Timeout(what)) => {
                refuse(&mut self.send, ErrorCode::Timeout).await;
                return Err(TrustError::Timeout(what));
            }
            Err(e) => return Err(e),
        };
        if !remote {
            refuse(&mut self.send, ErrorCode::SasRejected).await;
            return Err(TrustError::SasRejected);
        }

        // Both confirmed: pin, clear the provisional failure, tell the peer.
        let now = ctx.wall.unix_ms();
        let device = PairedDevice::new_paired(
            self.peer.spki,
            self.peer.name.clone(),
            self.peer.os,
            self.peer.os_version.clone(),
            now,
            Some(remote_v4(self.conn)),
        );
        // One change: the pin and the cleared provisional failure land together, or nothing does.
        let pinned = ctx.trust.try_update(|s| {
            let replaced = pin(s, device.clone(), self.replaces)?;
            s.lockout.succeed();
            Ok(replaced)
        });
        let replaced = match pinned {
            Ok(replaced) => replaced,
            Err(e) => {
                refuse(&mut self.send, e.code()).await;
                return Err(e);
            }
        };
        // Both confirmations are in and the store is written: from here nothing that fails undoes the pairing.
        let _ = write_msg(&mut self.send, PairResult { ok: true, code: ErrorCode::NoError }).await;
        // The peer's own result is a courtesy: we already pinned on both confirmations.
        let _ = tokio::time::timeout(ctx.timeouts.result, read_msg(&mut self.recv)).await;
        end_stream(&mut self.send).await;
        self.conn.close(quinn::VarInt::from_u32(u32::from(ErrorCode::NoError.tag())), b"");
        ctx.trace.log(Level::Info, "pair", format_args!("paired with {}", device_id_hex(&device.device_id)));
        Ok(PairOutcome { device, replaced })
    }
}

/// Adds the device; a re-pairing first removes the entry it replaces. Returns the replaced ID. Everything is
/// validated before anything changes, so a refusal leaves `store` exactly as it was.
fn pin(store: &mut TrustStore, device: PairedDevice, replaces: Option<DeviceId>) -> Result<Option<DeviceId>> {
    let replaced = replaces.filter(|old| store.find_by_id(old).is_some());
    let kept =
        store.devices.iter().filter(|d| Some(d.device_id) != replaced && d.device_id != device.device_id).count();
    if kept >= MAX_DEVICES {
        return Err(TrustError::Protocol(ErrorCode::TooLarge));
    }
    if let Some(old) = &replaced {
        store.forget(old);
    }
    store.devices.retain(|d| d.device_id != device.device_id);
    store.devices.push(device);
    Ok(replaced)
}

fn check_majors(majors: &[u16]) -> Result<()> {
    if majors.len() <= MAX_PAIR_MAJORS && majors.contains(&PROTOCOL_VERSION) {
        Ok(())
    } else {
        Err(TrustError::Protocol(ErrorCode::UnsupportedVersion))
    }
}

fn local_hello_fields(local: &LocalInfo) -> (String, Os, String, Vec<u16>) {
    (local.name.clone(), local.os, local.os_version.clone(), vec![PROTOCOL_VERSION])
}

// ---------- bookkeeping shared by both roles ----------

/// One connection's attempt. Only the attempt that holds the machine's pairing slot (`owner`) may end the screen's
/// pairing and close pairing mode; a connection that never got the slot (busy, refused before step 2, garbage, a
/// timeout before the first message) must not disturb the ceremony that has it.
#[derive(Default)]
struct Attempt {
    peer: std::sync::Mutex<Option<DeviceId>>,
    began: std::sync::atomic::AtomicBool,
    owner: std::sync::Mutex<Option<PairingGuard>>,
}

impl Attempt {
    fn set_peer(&self, id: DeviceId) {
        *self.peer.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }

    /// Takes the slot; `Busy` when another attempt of either role holds it.
    fn take_slot(&self, ctx: &PairingContext) -> Result<()> {
        let guard = ctx.trust.try_begin_pairing().ok_or(TrustError::Protocol(ErrorCode::Busy))?;
        *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
        Ok(())
    }

    /// Gives the slot back before step 2 (a refusal that is not a ceremony).
    fn release_slot(&self) {
        self.owner.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    fn owns_slot(&self) -> bool {
        self.owner.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

/// Runs `body`, then does what a failed attempt requires. A dropped future skips this on purpose: the
/// provisional entry stays on disk and counts at the next start (a crash, or a closed screen).
async fn finish<F>(ctx: &PairingContext, conn: &quinn::Connection, attempt: &Attempt, body: F) -> Result<PairOutcome>
where
    F: Future<Output = Result<PairOutcome>>,
{
    use std::sync::atomic::Ordering;
    let result = body.await;
    let now = ctx.wall.unix_ms();
    let peer_id = *attempt.peer.lock().unwrap_or_else(|e| e.into_inner());
    let device = match peer_id {
        Some(id) if ctx.trust.read(|s| s.find_by_id(&id).is_some()) => DeviceRef::Known(id),
        id => DeviceRef::Unknown { addr: remote_v4(conn), device_id: id },
    };
    match &result {
        Ok(out) => {
            ctx.audit.append(AuditEntry {
                time_ms: now,
                device: DeviceRef::Known(out.device.device_id),
                event: AuditEvent::Paired,
                duration_s: None,
            });
            if let Some(old) = out.replaced {
                ctx.audit.append(AuditEntry {
                    time_ms: now,
                    device: DeviceRef::Known(out.device.device_id),
                    event: AuditEvent::KeyReplaced { old },
                    duration_s: None,
                });
            }
        }
        Err(e) => {
            if attempt.began.load(Ordering::SeqCst) {
                // The provisional failure becomes a counted one. If the write fails the provisional entry is
                // still on disk, which counts too.
                let _ = ctx.trust.update(|s| s.lockout.fail(now));
            }
            // A failure that reached step 2 counted against the lockout (`PairFailed`); one that did not is what an
            // unauthenticated peer can repeat at will (`PairRefused`), and the audit rate limit covers only that.
            let event = if matches!(e, TrustError::Locked { .. }) {
                AuditEvent::PairingLocked
            } else if attempt.began.load(Ordering::SeqCst) {
                AuditEvent::PairFailed(e.code())
            } else {
                AuditEvent::PairRefused(e.code())
            };
            ctx.audit.append(AuditEntry { time_ms: now, device, event, duration_s: None });
            ctx.trace.log(Level::Warn, "pair", format_args!("pairing failed: {:?}", e.code()));
            conn.close(quinn::VarInt::from_u32(u32::from(e.code().tag())), b"");
        }
    }
    if attempt.owns_slot() {
        ctx.mode.close();
        ctx.ui.ended(&result);
    }
    result
}

// ---------- responder ----------

/// The responder's side of section 3, after the handshake on the pairing endpoint. The machine has one pairing
/// slot for both roles: a second attempt gets `PairResult { BUSY }` and changes nothing of the one that has it.
pub async fn respond(ctx: &PairingContext, conn: &quinn::Connection) -> Result<PairOutcome> {
    let attempt = Attempt::default();
    finish(ctx, conn, &attempt, respond_steps(ctx, conn, &attempt)).await
}

async fn respond_steps(ctx: &PairingContext, conn: &quinn::Connection, attempt: &Attempt) -> Result<PairOutcome> {
    let deadline = Instant::now() + ctx.timeouts.handshake_to_reveal;
    let spki_i = gate_pairing(conn)?;
    let peer_id = device_id(&spki_i);
    attempt.set_peer(peer_id);

    // The initiator opens the one bidirectional stream with `PairHello`; nothing exists before that.
    let (mut send, mut recv) = within(deadline, "stream", async { conn.accept_bi().await.map_err(lost) }).await?;
    let hello = match within(deadline, "PairHello", read_msg(&mut recv)).await? {
        Message::PairHello(h) => h,
        _ => {
            refuse(&mut send, ErrorCode::ProtocolViolation).await;
            return Err(TrustError::Protocol(ErrorCode::ProtocolViolation));
        }
    };

    // Step 0 answers, in the order of the table: busy, locked, already paired.
    let checks = (|| -> Result<Option<DeviceId>> {
        attempt.take_slot(ctx)?;
        let options = ctx.mode.current().ok_or(TrustError::NotInPairingMode)?;
        let now = ctx.wall.unix_ms();
        if let Some(left) = ctx.trust.read(|s| s.lockout.remaining_ms(now)) {
            return Err(TrustError::Locked { remaining_ms: left });
        }
        if ctx.trust.pinned(&spki_i).is_some() {
            return Err(TrustError::Protocol(ErrorCode::AlreadyPaired));
        }
        if options.expect.is_some_and(|e| e != peer_id) {
            return Err(TrustError::Protocol(ErrorCode::NotPermitted));
        }
        check_majors(&hello.majors)?;
        Ok(options.replaces)
    })();
    let replaces = match checks {
        Ok(r) => r,
        Err(e) => {
            // Not a ceremony: the slot (if this attempt took it) goes back and nothing else is touched.
            attempt.release_slot();
            refuse(&mut send, e.code()).await;
            return Err(e);
        }
    };

    // Step 2: the provisional failed attempt is on disk before the nonce is sent.
    let nonce_r = random_nonce()?;
    let now = ctx.wall.unix_ms();
    match ctx.trust.update(|s| s.lockout.begin(now)) {
        Ok(Ok(())) => attempt.began.store(true, std::sync::atomic::Ordering::SeqCst),
        Ok(Err(left)) => {
            attempt.release_slot();
            refuse(&mut send, ErrorCode::PairingLocked).await;
            return Err(TrustError::Locked { remaining_ms: left });
        }
        Err(e) => {
            attempt.release_slot();
            refuse(&mut send, ErrorCode::IoFailed).await;
            return Err(e);
        }
    }
    let (name, os, os_version, majors) = local_hello_fields(&ctx.local);
    write_msg(&mut send, PairNonce { name, os, os_version, majors, nonce_r }).await?;
    let peer = PeerInfo {
        name: hello.name.clone(),
        os: hello.os,
        os_version: hello.os_version.clone(),
        device_id: peer_id,
        spki: spki_i,
    };
    ctx.ui.peer_announced(&peer);
    ctx.trace.log(Level::Info, "pair", format_args!("pairing started with {}", device_id_hex(&peer_id)));

    // Step 3: the reveal must open the commitment.
    let nonce_i = match within(deadline, "PairReveal", read_msg(&mut recv)).await? {
        Message::PairReveal(PairReveal { nonce_i }) => nonce_i,
        _ => {
            refuse(&mut send, ErrorCode::ProtocolViolation).await;
            return Err(TrustError::Protocol(ErrorCode::ProtocolViolation));
        }
    };
    if commitment(&spki_i, &nonce_i) != hello.commit {
        refuse(&mut send, ErrorCode::CommitMismatch).await;
        return Err(TrustError::Protocol(ErrorCode::CommitMismatch));
    }

    let spki_r = *ctx.identity.spki();
    Ceremony { ctx, conn, send, recv, spki_i, spki_r, nonce_i, nonce_r, peer, replaces }.run().await
}

// ---------- initiator ----------

/// The initiator's side of section 3, after the handshake on the pairing endpoint.
pub async fn initiate(ctx: &PairingContext, conn: &quinn::Connection, options: InitiateOptions) -> Result<PairOutcome> {
    let attempt = Attempt::default();
    finish(ctx, conn, &attempt, initiate_steps(ctx, conn, options, &attempt)).await
}

async fn initiate_steps(
    ctx: &PairingContext,
    conn: &quinn::Connection,
    options: InitiateOptions,
    attempt: &Attempt,
) -> Result<PairOutcome> {
    let deadline = Instant::now() + ctx.timeouts.handshake_to_reveal;
    // One pairing at a time per machine, whatever the role: a running one is never disturbed by this call.
    attempt.take_slot(ctx)?;
    // The service never starts a pairing its user did not ask for.
    let mode = ctx.mode.current().ok_or(TrustError::NotInPairingMode)?;
    let replaces = mode.replaces.or(options.replaces);
    // The device the user picked is the one the mode names, unless this call names another.
    let expect = options.expect.or(mode.expect);
    let spki_r = gate_pairing(conn)?;
    let peer_id = device_id(&spki_r);
    attempt.set_peer(peer_id);
    if expect.is_some_and(|e| e != peer_id) {
        return Err(TrustError::Protocol(ErrorCode::NotPermitted));
    }
    let now = ctx.wall.unix_ms();
    if let Some(left) = ctx.trust.read(|s| s.lockout.remaining_ms(now)) {
        return Err(TrustError::Locked { remaining_ms: left });
    }
    if ctx.trust.pinned(&spki_r).is_some() {
        return Err(TrustError::Protocol(ErrorCode::AlreadyPaired));
    }

    // Step 1.
    let spki_i = *ctx.identity.spki();
    let nonce_i = random_nonce()?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(lost)?;
    let (name, os, os_version, majors) = local_hello_fields(&ctx.local);
    write_msg(&mut send, PairHello { name, os, os_version, majors, commit: commitment(&spki_i, &nonce_i) }).await?;

    // Step 2 (or a refusal: locked, busy, already paired).
    let nonce = match within(deadline, "PairNonce", read_msg(&mut recv)).await? {
        Message::PairNonce(n) => n,
        Message::PairResult(r) if !r.ok => return Err(TrustError::Refused(r.code)),
        _ => {
            refuse(&mut send, ErrorCode::ProtocolViolation).await;
            return Err(TrustError::Protocol(ErrorCode::ProtocolViolation));
        }
    };
    // The initiator counts its own failures the same way, from the moment the responder committed to a nonce.
    let now = ctx.wall.unix_ms();
    match ctx.trust.update(|s| s.lockout.begin(now)) {
        Ok(Ok(())) => attempt.began.store(true, std::sync::atomic::Ordering::SeqCst),
        Ok(Err(left)) => {
            refuse(&mut send, ErrorCode::PairingLocked).await;
            return Err(TrustError::Locked { remaining_ms: left });
        }
        Err(e) => {
            refuse(&mut send, ErrorCode::IoFailed).await;
            return Err(e);
        }
    }
    if let Err(e) = check_majors(&nonce.majors) {
        refuse(&mut send, e.code()).await;
        return Err(e);
    }
    let peer = PeerInfo {
        name: nonce.name.clone(),
        os: nonce.os,
        os_version: nonce.os_version.clone(),
        device_id: peer_id,
        spki: spki_r,
    };
    ctx.ui.peer_announced(&peer);
    ctx.trace.log(Level::Info, "pair", format_args!("pairing started with {}", device_id_hex(&peer_id)));

    // Step 3.
    write_msg(&mut send, PairReveal { nonce_i }).await?;
    Ceremony { ctx, conn, send, recv, spki_i, spki_r, nonce_i, nonce_r: nonce.nonce_r, peer, replaces }.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::spki_from_public_key;

    fn dev(seed: u8) -> PairedDevice {
        PairedDevice::new_paired(spki_from_public_key(&[seed; 32]), "pc".into(), Os::MacOs, "15".into(), 1, None)
    }

    fn full(extra: usize) -> TrustStore {
        let mut store = TrustStore::default();
        for i in 0..MAX_DEVICES + extra {
            store.devices.push(dev(i as u8 + 1));
        }
        store.lockout.provisional = Some(5);
        store
    }

    #[test]
    fn a_refused_pin_leaves_the_store_exactly_as_it_was() {
        // A store one over the cap, as a damaged file could hold: replacing an entry frees one slot, not two.
        let mut store = full(1);
        let before = store.clone();
        let first = store.devices[0].device_id;
        let err = pin(&mut store, dev(200), Some(first)).unwrap_err();
        assert!(matches!(err, TrustError::Protocol(ErrorCode::TooLarge)), "{err:?}");
        assert_eq!(store, before, "the replaced entry must not be gone after a refusal");
    }

    #[test]
    fn a_new_device_in_a_full_store_is_refused_and_a_replacement_at_the_cap_is_not() {
        let mut store = full(0);
        let before = store.clone();
        assert!(pin(&mut store, dev(200), None).is_err());
        assert_eq!(store, before);

        let first = store.devices[0].device_id;
        assert_eq!(pin(&mut store, dev(200), Some(first)).unwrap(), Some(first));
        assert_eq!(store.devices.len(), MAX_DEVICES);
        assert!(store.find_by_id(&first).is_none());
        assert!(store.devices.iter().any(|d| d.spki == spki_from_public_key(&[200; 32])));
    }
}
