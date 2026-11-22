//! Both users press "Parear" on each other's card at once (review of 22/11/2026, A3): each machine dials the other's
//! pairing endpoint, so two ceremonies cross. One of them must run to the end, whatever the order of the packets, and
//! a peer of 1.0 or 1.1.0 (which never gives way) must still pair with this build.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use swapdesk_proto::{ErrorCode, Message, Os, PROTOCOL_VERSION, PairConfirm, PairHello, PairResult, PairReveal};
use swapdesk_trust::pairing::commitment;
use swapdesk_trust::tls::{SERVER_NAME, pairing_client_config};
use swapdesk_trust::{DeviceId, Identity, InitiateOptions, TrustError, connect_and_initiate, open_pairing_endpoint};
use tokio::sync::Notify;

/// What each side of a crossing ended with, once both screens heard the end.
async fn both_ended(a: &Node, b: &Node) -> (Ended, Ended) {
    let (ea, eb) = (a.ui.wait_ended().await, b.ui.wait_ended().await);
    // A second end would come right after the first: give it the time to arrive.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (ea, eb)
}

fn ends(node: &Node) -> usize {
    node.ui.ended.lock().unwrap().len()
}

fn spawn_initiator(node: &Node, to: SocketAddr, after: Duration) -> tokio::task::JoinHandle<Result<(), TrustError>> {
    let ctx = node.ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        connect_and_initiate(&client_endpoint(), &ctx, to, InitiateOptions::default()).await.map(|_| ())
    })
}

/// Review of 22/11/2026, A3: when both users pressed "Parear" on each other's card at once, each machine's initiator
/// took its own pairing slot and each responder then answered the other's `PairHello` with `BUSY`, so both pairings
/// failed. Now the machine with the higher device ID gives way and answers the other's ceremony, which runs to the end.
#[test]
fn two_machines_that_start_toward_each_other_at_once_pair_once() {
    run(async {
        // The second press comes at once or a little later, from either side: every order of the packets.
        for (first_is_a, delay_ms) in [(true, 0), (false, 0), (true, 3), (false, 3), (true, 20), (false, 20)] {
            let a = Node::new("a", TestUi::answering(true));
            let b = Node::new("b", TestUi::answering(true));
            a.open_pairing_mode();
            b.open_pairing_mode();
            let (sa, sb) = (start_responder(&a), start_responder(&b));
            let delay = Duration::from_millis(delay_ms);
            let (da, db) = if first_is_a { (Duration::ZERO, delay) } else { (delay, Duration::ZERO) };
            let ia = spawn_initiator(&a, sb.addr, da);
            let ib = spawn_initiator(&b, sa.addr, db);
            let (ea, eb) = both_ended(&a, &b).await;
            let case = format!("a first: {first_is_a}, {delay_ms} ms, a's id above b's: {}", a.id() > b.id());
            assert!(ea.ok && eb.ok, "{case}: both screens must end paired: {ea:?} {eb:?}");
            assert_eq!((ends(&a), ends(&b)), (1, 1), "{case}: each screen hears one end");
            for (me, other) in [(&a, &b), (&b, &a)] {
                let disk = me.disk();
                assert_eq!(disk.devices.len(), 1, "{case}: {} pinned {:?}", me.name, disk.devices);
                assert_eq!(disk.devices[0].spki, *other.identity.spki(), "{case}");
                assert!(disk.lockout.failures.is_empty() && disk.lockout.provisional.is_none(), "{case}");
            }
            // The initiator that gave way ended too, quietly; neither is left running.
            for task in [ia, ib] {
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .expect("an initiator still runs")
                    .unwrap()
                    .ok();
            }
            for s in [sa, sb] {
                s.task.abort();
                s.endpoint.close(0u32.into(), b"test over");
            }
        }
    });
}

trait Id {
    fn id(&self) -> DeviceId;
}

impl Id for Node {
    fn id(&self) -> DeviceId {
        self.identity.device_id()
    }
}

/// A key whose device ID is above (`above`) or below the one of `than`.
fn identity_ordered(than: DeviceId, above: bool) -> Arc<Identity> {
    loop {
        let identity = Identity::generate(0).unwrap();
        if (identity.device_id() > than) == above {
            return Arc::new(identity);
        }
    }
}

/// A machine of 1.0 or 1.1.0, written by hand from PROTOCOL.md 3: its responder answers every `PairHello` with
/// `BUSY` (its own initiator holds its one pairing slot) when `answer` is notified, and its initiator never gives way.
struct OldPeer {
    identity: Arc<Identity>,
    addr: SocketAddr,
    /// Set once its responder has read a `PairHello`.
    hello_read: Arc<Notify>,
    /// Notified when its responder may send its `BUSY`.
    answer: Arc<Notify>,
    /// Set once its responder has sent `BUSY`.
    answered: Arc<Notify>,
    _endpoint: quinn::Endpoint,
    _held: Arc<Mutex<Vec<quinn::Connection>>>,
}

impl OldPeer {
    fn start(identity: Arc<Identity>) -> Self {
        let socket = loopback_socket();
        let addr = socket.local_addr().unwrap();
        let endpoint = open_pairing_endpoint(socket, &identity).unwrap();
        let (hello_read, answer, answered) =
            (Arc::new(Notify::new()), Arc::new(Notify::new()), Arc::new(Notify::new()));
        let held = Arc::new(Mutex::new(Vec::new()));
        {
            let (endpoint, hello_read, answer, answered, held) =
                (endpoint.clone(), hello_read.clone(), answer.clone(), answered.clone(), held.clone());
            tokio::spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    let Ok(conn) = incoming.accept().unwrap().await else { continue };
                    held.lock().unwrap().push(conn.clone());
                    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
                    assert!(matches!(read_msg(&mut recv).await, Message::PairHello(_)));
                    hello_read.notify_one();
                    answer.notified().await;
                    write_msg(&mut send, PairResult { ok: false, code: ErrorCode::Busy }).await;
                    let _ = send.finish();
                    answered.notify_one();
                }
            });
        }
        Self { identity, addr, hello_read, answer, answered, _endpoint: endpoint, _held: held }
    }

    /// Its initiator's whole ceremony against `addr`, its user pressing "Os códigos são iguais".
    async fn initiate(&self, addr: SocketAddr) -> Result<(), String> {
        let client = client_endpoint();
        let cfg = pairing_client_config(&self.identity).unwrap();
        let conn = client.connect_with(cfg, addr, SERVER_NAME).unwrap().await.map_err(|e| e.to_string())?;
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let nonce_i = [0x33; swapdesk_proto::NONCE_LEN];
        let commit = commitment(self.identity.spki(), &nonce_i);
        let hello = PairHello {
            name: "old".into(),
            os: Os::MacOs,
            os_version: "15".into(),
            majors: vec![PROTOCOL_VERSION],
            commit,
        };
        write_msg(&mut send, hello).await;
        match read_msg(&mut recv).await {
            Message::PairNonce(_) => {}
            other => return Err(format!("instead of the nonce: {other:?}")),
        }
        write_msg(&mut send, PairReveal { nonce_i }).await;
        write_msg(&mut send, PairConfirm { matches: true }).await;
        match read_msg(&mut recv).await {
            Message::PairConfirm(PairConfirm { matches: true }) => {}
            other => return Err(format!("instead of the peer's confirmation: {other:?}")),
        }
        match read_msg(&mut recv).await {
            Message::PairResult(PairResult { ok: true, .. }) => {}
            other => return Err(format!("instead of the result: {other:?}")),
        }
        write_msg(&mut send, PairResult { ok: true, code: ErrorCode::NoError }).await;
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(1), send.stopped()).await;
        Ok(())
    }
}

/// Review of 22/11/2026, A3, against a peer of 1.0 or 1.1.0: the old peer answers `BUSY` first, then its own
/// `PairHello` arrives. Before, this machine's initiator ended the pairing on the `BUSY` and closed the mode, so the old
/// peer's ceremony found nothing open either. Now a `BUSY` from the very device this machine dials makes it wait a
/// moment for that device's own attempt and answer it, whichever of the two device IDs is higher.
#[test]
fn a_peer_of_before_that_answers_busy_first_is_answered_when_its_own_ceremony_arrives() {
    run(async {
        for above in [true, false] {
            let b = Node::new("b", TestUi::answering(true));
            b.open_pairing_mode();
            let sb = start_responder(&b);
            let old = OldPeer::start(identity_ordered(b.id(), above));
            old.answer.notify_one();
            let ib = spawn_initiator(&b, old.addr, Duration::ZERO);
            old.answered.notified().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let theirs = old.initiate(sb.addr).await;
            let case = format!("the old peer's id above: {above}");
            assert_eq!(theirs, Ok(()), "{case}: the old peer's ceremony");
            let eb = b.ui.wait_ended().await;
            assert!(eb.ok, "{case}: {eb:?}");
            assert_eq!(b.disk().devices.len(), 1, "{case}");
            assert_eq!(b.disk().devices[0].spki, *old.identity.spki(), "{case}");
            assert!(b.disk().lockout.failures.is_empty(), "{case}");
            tokio::time::timeout(Duration::from_secs(5), ib).await.expect("the initiator still runs").unwrap().ok();
            assert_eq!(ends(&b), 1, "{case}");
            sb.task.abort();
        }
    });
}

/// Review of 22/11/2026, A3 and A11: an attempt refused `BUSY` before step 2 spent its source's 30 s turn, so the
/// user's retry was ignored without a word and read as a timeout. A `BUSY` given only because this machine's own
/// initiator dials that same device (this side keeps its ceremony; a peer of 1.0 or 1.1.0 does not give way) now
/// gives the turn back: the retry is answered at once.
#[test]
fn a_busy_given_to_the_very_device_this_side_dials_does_not_spend_its_turn() {
    use swapdesk_trust::{AdmittedHook, CrossedHook, IncomingFilter, PairingRate, ServeOptions};
    run(async {
        let b = Node::new("b", TestUi::answering(true));
        b.open_pairing_mode();
        let rate = Arc::new(PairingRate::new());
        let filter: IncomingFilter = {
            let rate = Arc::clone(&rate);
            Arc::new(move |incoming: &quinn::Incoming| rate.judge(incoming))
        };
        let admitted: AdmittedHook = {
            let rate = Arc::clone(&rate);
            Arc::new(move |incoming: &quinn::Incoming| rate.note(incoming))
        };
        let crossed: CrossedHook = {
            let rate = Arc::clone(&rate);
            Arc::new(move |from: SocketAddr| rate.forgive(from.ip()))
        };
        let sb = start_responder_with(
            &b,
            ServeOptions {
                filter: Some(filter),
                admitted: Some(admitted),
                crossed: Some(crossed),
                ..ServeOptions::default()
            },
        );
        // The old peer's ID is the higher one: this machine keeps its own ceremony and answers the old peer `BUSY`.
        let old = OldPeer::start(identity_ordered(b.id(), true));
        let ib = spawn_initiator(&b, old.addr, Duration::ZERO);
        old.hello_read.notified().await;
        let theirs = old.initiate(sb.addr).await;
        assert!(theirs.as_ref().is_err_and(|e| e.contains("Busy")), "{theirs:?}");

        // The old peer's user tries again at once: answered, not ignored for 30 s.
        let mut given_back = false;
        for _ in 0..60 {
            if rate.tracked() == 0 {
                given_back = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(given_back, "the source's turn was spent by a BUSY of a crossing");
        let identity = Identity::generate(0).unwrap();
        let retry =
            client_endpoint().connect_with(pairing_client_config(&identity).unwrap(), sb.addr, SERVER_NAME).unwrap();
        let retry = tokio::time::timeout(Duration::from_millis(1500), retry).await;
        assert!(matches!(retry, Ok(Ok(_))), "the retry inside 30 s was not answered");
        old.answer.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(5), ib).await;
        sb.task.abort();
    });
}

/// Review of 22/11/2026, A3, against a peer of 1.0 or 1.1.0 whose `PairHello` comes before its `BUSY`: this machine,
/// with the higher device ID, gives way at once and answers the old peer's ceremony.
#[test]
fn the_higher_id_gives_way_to_a_peer_of_before_whose_hello_comes_first() {
    run(async {
        let b = Node::new("b", TestUi::answering(true));
        b.open_pairing_mode();
        let sb = start_responder(&b);
        let old = OldPeer::start(identity_ordered(b.id(), false));
        let ib = spawn_initiator(&b, old.addr, Duration::ZERO);
        // This machine's `PairHello` reached the old peer, which holds its answer; the old peer's own ceremony starts.
        old.hello_read.notified().await;
        let theirs = old.initiate(sb.addr).await;
        assert_eq!(theirs, Ok(()), "the old peer's ceremony");
        let eb = b.ui.wait_ended().await;
        assert!(eb.ok, "{eb:?}");
        assert_eq!(b.disk().devices[0].spki, *old.identity.spki());
        // The initiator that gave way did not wait for the old peer's answer.
        tokio::time::timeout(Duration::from_secs(5), ib).await.expect("the initiator still runs").unwrap().ok();
        assert_eq!(ends(&b), 1);
        old.answer.notify_one();
        sb.task.abort();
    });
}
