//! Slice A1's proof on the device: the parts of the core whose cross-compile says nothing about run time. Each check
//! runs the code a session runs, not a stand-in: the device key (`ring`), the session endpoint with pinned keys
//! (`rustls` and `quinn` on `tokio`), and the audio codec (libopus, built by the NDK's CMake).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use swapdesk_audio::{FRAME_SAMPLES, FRAME_VALUES, FrameDecoder, FrameEncoder};
use swapdesk_platform::SecretStore;
use swapdesk_proto::Os;
use swapdesk_trust::tls::{SERVER_NAME, pairing_client_config};
use swapdesk_trust::{
    Identity, PairedDevice, TrustHandle, accept_session, connect_session, device_id, endpoint_from_socket,
    open_session_endpoint,
};

/// How long [`probe`] waits for any answer.
const PROBE_LIMIT: Duration = Duration::from_secs(5);

/// How long the handshake check may take before it counts as failed.
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);

/// Secrets kept in memory for the length of one run. The app's own store wraps its files with the Android Keystore
/// (`swapdesk-sys-android`).
#[derive(Default)]
struct Memory(Mutex<HashMap<String, Vec<u8>>>);

impl Memory {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl SecretStore for Memory {
    fn load(&self, name: &str) -> swapdesk_platform::Result<Option<Vec<u8>>> {
        Ok(self.map().get(name).cloned())
    }

    fn store(&self, name: &str, bytes: &[u8]) -> swapdesk_platform::Result<()> {
        self.map().insert(name.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn delete(&self, name: &str) -> swapdesk_platform::Result<()> {
        self.map().remove(name);
        Ok(())
    }
}

/// One check: what it found, or why it failed.
type Check = fn() -> Result<String, String>;

/// Runs every check. The first line is `SELFTEST <passed>/<all>`; then one line per check, `OK <name>: ...` or
/// `FAIL <name>: ...`.
pub fn run() -> String {
    let checks: [(&str, Check); 3] = [("keys", keys), ("quic", quic), ("opus", opus)];
    let lines: Vec<(bool, String)> = checks
        .iter()
        .map(|(name, check)| match check() {
            Ok(what) => (true, format!("OK {name}: {what}")),
            Err(why) => (false, format!("FAIL {name}: {why}")),
        })
        .collect();
    let passed = lines.iter().filter(|(ok, _)| *ok).count();
    let mut out = format!("SELFTEST {passed}/{}", lines.len());
    for (_, line) in &lines {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// Development harness: whether a Swapdesk pairing endpoint answers at `addr` from this device (a computer has one
/// only while "Parear este computador" is open). `PROBE ANSWERED` says the QUIC handshake completed, which needs
/// packets both ways; `PROBE TIMEOUT` that nothing came back. Nothing of the pairing ceremony is started.
pub fn probe(addr: &str) -> String {
    let Ok(to) = addr.parse::<SocketAddr>() else { return format!("PROBE FAIL: {addr:?} is not an address") };
    let attempt = async {
        let me = Identity::generate(0).map_err(said("a key"))?;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(said("a socket"))?;
        let client = endpoint_from_socket(socket, None).map_err(said("the endpoint"))?;
        let config = pairing_client_config(&me).map_err(said("the pairing configuration"))?;
        let connecting = client.connect_with(config, to, SERVER_NAME).map_err(said("the dial"))?;
        Ok::<_, String>(match tokio::time::timeout(PROBE_LIMIT, connecting).await {
            Err(_) => format!("PROBE TIMEOUT: nothing answered at {to} in {} s", PROBE_LIMIT.as_secs()),
            Ok(Ok(conn)) => {
                let from = conn.local_ip().map_or_else(|| "this device".to_owned(), |ip| ip.to_string());
                conn.close(0u32.into(), b"");
                format!("PROBE ANSWERED: the pairing endpoint at {to} completed the handshake with {from}")
            }
            Ok(Err(e)) => format!("PROBE REFUSED: {to} answered and the handshake failed ({e})"),
        })
    };
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(attempt).unwrap_or_else(|why| format!("PROBE FAIL: {why}")),
        Err(e) => format!("PROBE FAIL: the runtime: {e}"),
    }
}

fn said<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

/// Two device keys, and the device IDs derived from them.
fn keys() -> Result<String, String> {
    let a = Identity::generate(0).map_err(said("the first key"))?;
    let b = Identity::generate(0).map_err(said("the second key"))?;
    let (ia, ib) = (device_id(a.spki()), device_id(b.spki()));
    if ia == ib {
        return Err("two generated keys gave one device ID".into());
    }
    Ok(format!("two device keys, IDs {:02x}{:02x}.. and {:02x}{:02x}..", ia[0], ia[1], ib[0], ib[1]))
}

/// A device with its trust store, as a session endpoint needs them.
struct Node {
    identity: Identity,
    trust: Arc<TrustHandle>,
}

impl Node {
    fn new() -> Result<Self, String> {
        Ok(Self {
            identity: Identity::generate(0).map_err(said("a key"))?,
            trust: TrustHandle::open(Arc::new(Memory::default())).map_err(said("a trust store"))?,
        })
    }

    fn pin(&self, other: &Node) -> Result<(), String> {
        let spki = *other.identity.spki();
        self.trust
            .update(|s| {
                s.devices.push(PairedDevice::new_paired(spki, "selftest".into(), Os::Android, "0".into(), 1, None))
            })
            .map_err(said("a pin"))
    }
}

/// Two devices that pinned each other open a session connection over this device's loopback: the handshake with
/// pinned keys, the host's gate, and bytes on a stream in both directions.
fn quic() -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(said("the runtime"))?;
    runtime.block_on(async {
        match tokio::time::timeout(HANDSHAKE_LIMIT, handshake()).await {
            Ok(result) => result,
            Err(_) => Err(format!("nothing after {} s", HANDSHAKE_LIMIT.as_secs())),
        }
    })
}

async fn handshake() -> Result<String, String> {
    let (viewer, host) = (Node::new()?, Node::new()?);
    viewer.pin(&host)?;
    host.pin(&viewer)?;
    let loopback = || UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).map_err(said("a loopback socket"));

    let host_socket = loopback()?;
    let addr = host_socket.local_addr().map_err(said("the host's address"))?;
    let endpoint =
        open_session_endpoint(host_socket, &host.identity, host.trust.clone()).map_err(said("the host endpoint"))?;
    let trust = host.trust.clone();
    let accepting = tokio::spawn(async move {
        let incoming = endpoint.accept().await.ok_or("the host endpoint closed")?;
        let (conn, id) = accept_session(incoming, &*trust).await.map_err(said("the host's accept"))?;
        let (mut send, mut recv) = conn.accept_bi().await.map_err(said("the host's stream"))?;
        let got = recv.read_to_end(16).await.map_err(said("the host's read"))?;
        send.write_all(b"pong").await.map_err(said("the host's write"))?;
        send.finish().map_err(said("the host's finish"))?;
        // The connection stays open until the viewer has read the answer.
        conn.closed().await;
        Ok::<_, String>((id, got))
    });

    let client = endpoint_from_socket(loopback()?, None).map_err(said("the viewer endpoint"))?;
    let conn = connect_session(&client, &viewer.identity, addr, *host.identity.spki())
        .await
        .map_err(said("the viewer's connect"))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(said("the viewer's stream"))?;
    send.write_all(b"ping").await.map_err(said("the viewer's write"))?;
    send.finish().map_err(said("the viewer's finish"))?;
    let answer = recv.read_to_end(16).await.map_err(said("the viewer's read"))?;
    conn.close(0u32.into(), b"");

    let (id, got) = accepting.await.map_err(said("the host task"))??;
    if id != device_id(viewer.identity.spki()) {
        return Err("the host's gate named another device".into());
    }
    if got != b"ping" || answer != b"pong" {
        return Err(format!("the stream carried {got:?} and {answer:?}"));
    }
    Ok(format!("pinned-key handshake on {addr}, the gate named the viewer, 4 bytes each way"))
}

/// One 10 ms frame of a 440 Hz tone through the encoder and back.
fn opus() -> Result<String, String> {
    let mut frame = [0f32; FRAME_VALUES];
    for (i, pair) in frame.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.25;
        *pair = [v, v];
    }
    let mut encoder = FrameEncoder::new().map_err(|e| format!("the encoder: {e:?}"))?;
    let packet = encoder.encode(&frame).map_err(|e| format!("the encode: {e:?}"))?.to_vec();
    let mut decoder = FrameDecoder::new().map_err(|e| format!("the decoder: {e:?}"))?;
    let mut back = [0f32; FRAME_VALUES];
    let samples = decoder.decode(&packet, &mut back).map_err(|e| format!("the decode: {e:?}"))?;
    if samples != FRAME_SAMPLES {
        return Err(format!("{samples} samples came back, not {FRAME_SAMPLES}"));
    }
    Ok(format!("a {}-byte packet, {samples} samples back", packet.len()))
}
