//! Flow proofs of PROTOCOL.md 3: pairing on loopback through quinn, lockout, crash, unsolicited attempts,
//! re-pairing, the exporter binding and the never-logged values.

mod common;

use std::time::Duration;

use common::*;
use swapdesk_proto::Wire;
use swapdesk_proto::{
    EXPORTER_LABEL, EXPORTER_LEN, ErrorCode, Message, NONCE_LEN, Os, PROTOCOL_VERSION, PairConfirm, PairHello,
    PairResult, PairReveal,
};
use swapdesk_trust::pairing::{Timeouts, commitment, compute_sas};
use swapdesk_trust::tls::pairing_client_config;
use swapdesk_trust::{
    AuditEvent, Identity, InitiateOptions, ModeOptions, TrustError, connect_and_initiate, device_id, respond,
};

fn digits(sas: swapdesk_proto::Sas) -> String {
    sas.to_string()
}

#[test]
fn pair_two_instances() {
    run(async {
        let a = Node::new("alice-pc", TestUi::answering(true));
        let b = Node::new("bob-mac", TestUi::answering(true));
        let (ra, rb) = pair_once(&a, &b, InitiateOptions::default()).await;
        let (oa, ob) = (ra.expect("initiator pinned"), rb.outcome.clone().expect("responder pinned"));

        // Both screens showed the same six digits.
        let (ca, cb) = (a.ui.last_code().unwrap(), b.ui.last_code().unwrap());
        assert_eq!(ca, cb);
        let shown = digits(ca);
        assert_eq!(shown.len(), 7, "two groups of three digits and a space: {shown}");
        assert!(shown.chars().filter(|c| c.is_ascii_digit()).count() == 6);

        // Each pinned the other, with no grants.
        assert_eq!(oa.device.spki, *b.identity.spki());
        assert_eq!(ob.device.spki, *a.identity.spki());
        assert_eq!(oa.device.name, "bob-mac");
        assert_eq!(ob.device.name, "alice-pc");
        for (me, other) in [(&a, &b), (&b, &a)] {
            let disk = me.disk();
            assert_eq!(disk.devices.len(), 1);
            let d = &disk.devices[0];
            assert_eq!(d.device_id, device_id(other.identity.spki()));
            assert!(d.grants_here.is_empty());
            assert!(d.unattended_granted_by.is_none() && d.grants_there.is_none() && d.wol_macs.is_empty());
            assert_eq!(disk.lockout.provisional, None, "the provisional failure is cleared by the pinning");
            assert!(disk.lockout.failures.is_empty());
            assert_eq!(
                me.audit.entries().iter().map(|e| e.event.clone()).collect::<Vec<_>>(),
                vec![AuditEvent::Paired]
            );
        }
        // Pairing mode ended on both sides.
        assert!(!a.mode.is_open() && !b.mode.is_open());
    });
}

#[test]
fn a_mismatch_pins_nothing_and_counts_a_failure() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(false));
        let (ra, rb) = pair_once(&a, &b, InitiateOptions::default()).await;
        assert!(ra.is_err());
        assert!(!rb.ok);
        assert_eq!(rb.code, ErrorCode::SasRejected);
        for n in [&a, &b] {
            let disk = n.disk();
            assert!(disk.devices.is_empty(), "{} pinned a key after a mismatch", n.name);
            assert_eq!(disk.lockout.failures.len(), 1, "{} did not count the failure", n.name);
            assert_eq!(disk.lockout.provisional, None);
            assert_eq!(n.audit.entries().len(), 1);
            assert!(matches!(n.audit.entries()[0].event, AuditEvent::PairFailed(ErrorCode::SasRejected)));
        }
    });
}

#[test]
fn the_initiator_pressing_different_also_pins_nothing() {
    run(async {
        let a = Node::new("a", TestUi::answering(false));
        let b = Node::new("b", TestUi::answering(true));
        let (ra, rb) = pair_once(&a, &b, InitiateOptions::default()).await;
        assert!(ra.is_err() && !rb.ok);
        assert!(a.disk().devices.is_empty() && b.disk().devices.is_empty());
        assert_eq!(b.disk().lockout.failures.len(), 1);
    });
}

#[test]
fn three_failures_lock_out_and_survive_a_restart() {
    run(async {
        let b = Node::new("b", TestUi::answering(false));
        for _ in 0..3 {
            let a = Node::new("a", TestUi::answering(true));
            let (ra, rb) = pair_once(&a, &b, InitiateOptions::default()).await;
            assert!(ra.is_err() && !rb.ok);
        }
        let now = b.ctx.wall.unix_ms();
        let on_disk = b.disk();
        assert!(on_disk.lockout.remaining_ms(now).is_some(), "three failures lock pairing");
        assert!(on_disk.devices.is_empty());

        // The process restarts: the lockout is read back from disk.
        let b2 = b.restart(TestUi::answering(true));
        assert!(b2.trust.read(|s| s.lockout.remaining_ms(now)).is_some(), "the lockout survived the restart");
        let a = Node::new("a-locked-out", TestUi::answering(true));
        let (ra, rb) = pair_once(&a, &b2, InitiateOptions::default()).await;
        assert!(matches!(ra, Err(TrustError::Refused(ErrorCode::PairingLocked))), "{ra:?}");
        assert_eq!(rb.code, ErrorCode::PairingLocked);
        assert!(a.disk().devices.is_empty() && b2.disk().devices.is_empty());
        assert!(b2.audit.entries().iter().any(|e| e.event == AuditEvent::PairingLocked));
        assert!(b2.ui.last_code().is_none(), "no code was ever shown while locked");

        // After the 15 minutes the lock is gone and pairing works again.
        b2.clock.advance(Duration::from_secs(15 * 60 + 5));
        let a = Node::new("a-later", TestUi::answering(true));
        let (ra, rb) = pair_once(&a, &b2, InitiateOptions::default()).await;
        assert!(ra.is_ok() && rb.ok);
        assert_eq!(b2.disk().devices.len(), 1);
    });
}

#[test]
fn paired_devices_keep_their_pins_while_pairing_is_locked() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        let (ra, _) = pair_once(&a, &b, InitiateOptions::default()).await;
        assert!(ra.is_ok());
        let now = b.ctx.wall.unix_ms();
        b.trust.update(|s| s.lockout.locked_until_ms = now + 1_000_000).unwrap();
        assert!(swapdesk_trust::PinLookup::pinned(&*b.trust, a.identity.spki()).is_some());
    });
}

#[test]
fn a_crash_between_the_nonce_and_the_result_counts_as_a_provisional_failure() {
    run(async {
        let b = Node::new("b", TestUi::holding());
        let a = Node::new("a", TestUi::answering(true));
        a.open_pairing_mode();
        b.open_pairing_mode();

        let socket = loopback_socket();
        let addr = socket.local_addr().unwrap();
        let endpoint = swapdesk_trust::open_pairing_endpoint(socket, &b.identity).unwrap();
        let responder = {
            let (ctx, ep) = (b.ctx.clone(), endpoint.clone());
            tokio::spawn(async move {
                let conn = ep.accept().await.unwrap().accept().unwrap().await.unwrap();
                respond(&ctx, &conn).await.map(|_| ())
            })
        };
        let client = client_endpoint();
        let initiator = {
            let (ctx, client) = (a.ctx.clone(), client.clone());
            tokio::spawn(async move {
                connect_and_initiate(&client, &ctx, addr, InitiateOptions::default()).await.map(|_| ())
            })
        };
        b.ui.wait_announced().await;

        // Step 2 is past: the provisional failure is already on disk.
        let disk = b.disk();
        assert!(disk.lockout.provisional.is_some());
        assert!(disk.lockout.failures.is_empty());

        // The process dies here: nothing after this point runs on B.
        responder.abort();
        let _ = responder.await;
        endpoint.close(0u32.into(), b"crash");
        let _ = initiator.await;
        assert!(b.ui.last_ended().is_none(), "the dead process told nobody anything");
        assert!(b.disk().lockout.provisional.is_some());

        // After the restart the leftover provisional entry counts. Two more failed attempts make three.
        let b2 = b.restart(TestUi::answering(false));
        for _ in 0..2 {
            let a = Node::new("a", TestUi::answering(true));
            let (ra, rb) = pair_once(&a, &b2, InitiateOptions::default()).await;
            assert!(ra.is_err() && !rb.ok);
        }
        let now = b2.ctx.wall.unix_ms();
        assert!(b2.disk().lockout.remaining_ms(now).is_some(), "the crashed attempt counted as the first failure");
        assert!(b2.disk().devices.is_empty());
    });
}

#[test]
fn unsolicited_pairing_is_refused() {
    run(async {
        // 1. This machine is not in pairing mode: its pairing endpoint does not exist. Nothing answers.
        let silent = loopback_socket();
        let silent_addr = silent.local_addr().unwrap();
        drop(silent);
        let a = Node::new("a", TestUi::answering(true));
        a.open_pairing_mode();
        let attempt = tokio::time::timeout(
            Duration::from_secs(2),
            connect_and_initiate(&client_endpoint(), &a.ctx, silent_addr, InitiateOptions::default()),
        )
        .await;
        assert!(!matches!(attempt, Ok(Ok(_))), "a closed endpoint must not pair");
        assert!(a.disk().devices.is_empty());

        // 2. The endpoint is open but pairing mode has ended: the Incoming is refused, no prompt, no log entry.
        let b = Node::new("b", TestUi::answering(true));
        b.open_pairing_mode();
        let server = start_responder(&b);
        tokio::time::sleep(Duration::from_millis(20)).await;
        b.mode.close();
        let a2 = Node::new("a2", TestUi::answering(true));
        a2.open_pairing_mode();
        let attempt = tokio::time::timeout(
            Duration::from_secs(5),
            connect_and_initiate(&client_endpoint(), &a2.ctx, server.addr, InitiateOptions::default()),
        )
        .await;
        assert!(matches!(attempt, Ok(Err(_))), "{attempt:?}");
        let _ = tokio::time::timeout(Duration::from_secs(2), server.task).await;
        assert!(b.ui.announced.lock().unwrap().is_empty(), "no prompt on the unsolicited side");
        assert!(b.ui.last_code().is_none());
        assert!(b.audit.entries().is_empty(), "no log entry for an unsolicited pairing");
        assert_eq!(b.disk().lockout, Default::default(), "no failure counted either");
        assert!(b.disk().devices.is_empty());

        // 3. The initiating user did not open "Parear": the service never starts a pairing.
        let a3 = Node::new("a3", TestUi::answering(true));
        let r = connect_and_initiate(&client_endpoint(), &a3.ctx, silent_addr, InitiateOptions::default()).await;
        assert!(matches!(r, Err(TrustError::NotInPairingMode)));
    });
}

#[test]
fn a_device_the_users_did_not_pick_is_refused() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        let (ra, rb) =
            pair_once_with(&a, &b, InitiateOptions::default(), ModeOptions { expect: Some([7; 16]), replaces: None })
                .await;
        assert!(matches!(ra, Err(TrustError::Refused(ErrorCode::NotPermitted))), "{ra:?}");
        assert_eq!(rb.code, ErrorCode::NotPermitted);
        assert!(b.disk().devices.is_empty() && b.disk().lockout.failures.is_empty());

        // The initiator's user picked a device too: a responder with another key is not accepted.
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        let (ra, _) = pair_once(&a, &b, InitiateOptions { expect: Some([9; 16]), replaces: None }).await;
        assert!(matches!(ra, Err(TrustError::Protocol(ErrorCode::NotPermitted))), "{ra:?}");
        assert!(a.disk().devices.is_empty());
    });
}

#[test]
fn a_key_that_is_already_pinned_gets_already_paired_and_a_changed_key_replaces_after_a_full_ceremony() {
    run(async {
        let a = Node::new("alice-pc", TestUi::answering(true));
        let b = Node::new("bob-mac", TestUi::answering(true));
        let (ra, _) = pair_once(&a, &b, InitiateOptions::default()).await;
        assert!(ra.is_ok());
        let a_id = device_id(a.identity.spki());
        b.trust.update(|s| s.devices[0].grants_here = swapdesk_proto::PermissionSet::CONTROL).unwrap();

        // A forgot B; B still has A. B answers ALREADY_PAIRED; nobody is locked out by it.
        a.trust.update(|s| s.devices.clear()).unwrap();
        let (ra, rb) = pair_once(&a, &b, InitiateOptions::default()).await;
        assert!(matches!(ra, Err(TrustError::Refused(ErrorCode::AlreadyPaired))), "{ra:?}");
        assert_eq!(rb.code, ErrorCode::AlreadyPaired);
        assert!(b.disk().lockout.failures.is_empty() && a.disk().lockout.failures.is_empty());
        assert_eq!(
            b.disk().devices[0].grants_here,
            swapdesk_proto::PermissionSet::CONTROL,
            "the refused attempt changed nothing"
        );

        // A reinstalled: a new key under the same name. B's user opened "Parear" on that device's entry.
        let a2 = Node::new("alice-pc", TestUi::answering(true));
        let (ra, rb) =
            pair_once_with(&a2, &b, InitiateOptions::default(), ModeOptions { expect: None, replaces: Some(a_id) })
                .await;
        assert!(ra.is_ok(), "{ra:?}");
        let out = rb.outcome.unwrap();
        assert_eq!(out.replaced, Some(a_id));
        let disk = b.disk();
        assert_eq!(disk.devices.len(), 1);
        assert_eq!(disk.devices[0].spki, *a2.identity.spki());
        assert!(disk.devices[0].grants_here.is_empty(), "the replaced key's grants do not carry over");
        assert!(b.audit.entries().iter().any(|e| e.event == AuditEvent::KeyReplaced { old: a_id }));
    });
}

// ---------- a hand-written initiator ----------

struct Manual {
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    identity: Identity,
}

async fn manual_connect(addr: std::net::SocketAddr) -> (Manual, quinn::Endpoint) {
    let identity = Identity::generate(0).unwrap();
    let client = client_endpoint();
    let cfg = pairing_client_config(&identity).unwrap();
    let conn = client.connect_with(cfg, addr, swapdesk_trust::tls::SERVER_NAME).unwrap().await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    (Manual { conn, send, recv, identity }, client)
}

fn hello(commit: [u8; 32]) -> PairHello {
    PairHello { name: "manual".into(), os: Os::MacOs, os_version: "15".into(), majors: vec![PROTOCOL_VERSION], commit }
}

#[test]
fn a_commitment_that_does_not_open_is_a_mismatch_and_a_failed_attempt() {
    run(async {
        let b = Node::new("b", TestUi::answering(true));
        b.open_pairing_mode();
        let server = start_responder(&b);
        let (mut m, _client) = manual_connect(server.addr).await;
        let (committed, revealed) = ([1u8; NONCE_LEN], [2u8; NONCE_LEN]);
        write_msg(&mut m.send, hello(commitment(m.identity.spki(), &committed))).await;
        assert!(matches!(read_msg(&mut m.recv).await, Message::PairNonce(_)));
        write_msg(&mut m.send, PairReveal { nonce_i: revealed }).await;
        match read_msg(&mut m.recv).await {
            Message::PairResult(PairResult { ok: false, code: ErrorCode::CommitMismatch }) => {}
            other => panic!("expected COMMIT_MISMATCH, got {other:?}"),
        }
        let ended = b.ui.wait_ended().await;
        assert_eq!(ended.code, ErrorCode::CommitMismatch);
        let disk = b.disk();
        assert!(disk.devices.is_empty());
        assert_eq!(disk.lockout.failures.len(), 1);
        assert!(b.ui.last_code().is_none(), "no code is shown for a commitment that did not open");
    });
}

#[test]
fn a_stalled_initiator_times_out_and_only_a_started_attempt_counts() {
    run(async {
        let timeouts = Timeouts { handshake_to_reveal: Duration::from_millis(400), ..Timeouts::default() };

        // Connects and says nothing: no nonce was sent, so nothing is counted.
        let b = Node::with_timeouts("b", TestUi::answering(true), timeouts);
        b.open_pairing_mode();
        let server = start_responder(&b);
        let (_m, _c) = manual_connect(server.addr).await;
        assert_eq!(b.wait_attempt_over(0).await.code, ErrorCode::Timeout);
        assert!(b.disk().lockout.failures.is_empty() && b.disk().lockout.provisional.is_none());
        // It never owned the slot: the screen heard nothing and pairing mode is still open for the real peer.
        assert!(b.ui.last_ended().is_none() && b.mode.is_open());

        // Says hello, gets the nonce, never reveals: the attempt counts.
        let b = Node::with_timeouts("b", TestUi::answering(true), timeouts);
        b.open_pairing_mode();
        let server = start_responder(&b);
        let (mut m, _c) = manual_connect(server.addr).await;
        write_msg(&mut m.send, hello([0; 32])).await;
        assert!(matches!(read_msg(&mut m.recv).await, Message::PairNonce(_)));
        assert_eq!(b.ui.wait_ended().await.code, ErrorCode::Timeout);
        assert_eq!(b.disk().lockout.failures.len(), 1);
        assert!(b.disk().lockout.provisional.is_none());
    });
}

#[test]
fn a_comparison_that_nobody_answers_times_out_and_counts() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::with_timeouts(
            "b",
            TestUi::holding(),
            Timeouts { comparison: Duration::from_millis(500), ..Timeouts::default() },
        );
        let (ra, rb) = pair_once(&a, &b, InitiateOptions::default()).await;
        assert_eq!(rb.code, ErrorCode::Timeout);
        assert!(ra.is_err());
        assert!(a.disk().devices.is_empty() && b.disk().devices.is_empty());
        assert_eq!(b.disk().lockout.failures.len(), 1);
    });
}

#[test]
fn the_wire_matches_an_independent_initiator_and_nothing_secret_is_logged() {
    run(async {
        let b = Node::new("b", TestUi::answering(true));
        b.open_pairing_mode();
        let server = start_responder(&b);
        let (mut m, _client) = manual_connect(server.addr).await;
        let spki_i = *m.identity.spki();
        let nonce_i = [0x5A; NONCE_LEN];
        write_msg(&mut m.send, hello(commitment(&spki_i, &nonce_i))).await;
        let Message::PairNonce(nonce) = read_msg(&mut m.recv).await else { panic!("expected PairNonce") };
        assert_eq!(nonce.name, "b");
        write_msg(&mut m.send, PairReveal { nonce_i }).await;

        let mut ek = [0u8; EXPORTER_LEN];
        m.conn.export_keying_material(&mut ek, EXPORTER_LABEL, b"").unwrap();
        let sas = compute_sas(&ek, &spki_i, b.identity.spki(), &nonce_i, &nonce.nonce_r);
        write_msg(&mut m.send, PairConfirm { matches: true }).await;
        assert!(matches!(read_msg(&mut m.recv).await, Message::PairConfirm(PairConfirm { matches: true })));
        assert!(matches!(read_msg(&mut m.recv).await, Message::PairResult(PairResult { ok: true, .. })));
        write_msg(&mut m.send, PairResult { ok: true, code: ErrorCode::NoError }).await;
        let _ = m.send.finish();
        let ended = b.ui.wait_ended().await;
        assert!(ended.ok);

        // The responder showed exactly the code an independent implementation computes from the wire.
        assert_eq!(b.ui.last_code().unwrap(), sas);
        assert_eq!(b.disk().devices[0].spki, spki_i);

        // Never logged (C2.3): the keys, both nonces, the exporter output and the code, in every encoding.
        let log = &b.log;
        let record = b.identity.encode().unwrap();
        assert_eq!(record[2], 0x40, "pkcs8 length is a two-byte varint");
        let pkcs8 = &record[4..4 + 85];
        log.forbid("device private key", pkcs8);
        log.forbid("initiator private key", &m.identity.encode().unwrap()[4..4 + 85]);
        log.forbid("nonce_i", nonce_i);
        log.forbid("nonce_r", nonce.nonce_r);
        log.forbid("exporter output", ek);
        log.forbid("sas digits", format!("{:06}", sas.value()));
        log.forbid("sas shown", sas.to_string());
        log.forbid("commitment", commitment(&spki_i, &nonce_i));
        assert!(!log.lines().is_empty(), "the pairing logged nothing at all, so the check proves nothing");
        log.assert_clean();

        // The check can fail: a line that carries the exporter output is caught.
        use swapdesk_platform::{Level, TraceSink};
        log.log(Level::Info, "pair", format_args!("ek {}", ek.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        assert_eq!(log.violations().len(), 1);
    });
}

#[test]
fn a_real_pairing_logs_no_code_and_no_key_on_either_side() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        let (ra, rb) = pair_once(&a, &b, InitiateOptions::default()).await;
        assert!(ra.is_ok() && rb.ok);
        let sas = a.ui.last_code().unwrap();
        for n in [&a, &b] {
            let record = n.identity.encode().unwrap();
            n.log.forbid("private key", &record[4..4 + 85]);
            n.log.forbid("sas digits", format!("{:06}", sas.value()));
            n.log.forbid("sas shown", sas.to_string());
            assert!(n.log.lines().len() >= 2);
            n.log.assert_clean();
        }
    });
}

// ---------- the code ----------

type Inputs = ([u8; 32], [u8; 44], [u8; 44], [u8; 32], [u8; 32]);

fn base() -> Inputs {
    use swapdesk_trust::ids::spki_from_public_key;
    ([1; 32], spki_from_public_key(&[2; 32]), spki_from_public_key(&[3; 32]), [4; 32], [5; 32])
}

#[test]
fn both_keys_and_both_nonces_and_the_exporter_enter_the_code() {
    use swapdesk_trust::ids::spki_from_public_key;
    let (ek, si, sr, ni, nr) = base();
    let reference = compute_sas(&ek, &si, &sr, &ni, &nr);
    assert_eq!(reference, compute_sas(&ek, &si, &sr, &ni, &nr), "deterministic");
    let other_i = spki_from_public_key(&[9; 32]);
    let other_r = spki_from_public_key(&[8; 32]);
    assert_ne!(reference, compute_sas(&ek, &other_i, &sr, &ni, &nr), "the initiator's key is in the code");
    assert_ne!(reference, compute_sas(&ek, &si, &other_r, &ni, &nr), "the responder's key is in the code");
    assert_ne!(reference, compute_sas(&[0; 32], &si, &sr, &ni, &nr), "the exporter output is in the code");
    assert_ne!(reference, compute_sas(&ek, &si, &sr, &[0; 32], &nr), "nonce_I is in the code");
    assert_ne!(reference, compute_sas(&ek, &si, &sr, &ni, &[0; 32]), "nonce_R is in the code");
    // The commitment binds the initiator's key and nonce.
    assert_ne!(commitment(&si, &ni), commitment(&other_i, &ni));
    assert_ne!(commitment(&si, &ni), commitment(&si, &nr));
    assert!(reference.value() < 1_000_000);
}

#[test]
fn a_different_tls_session_gives_a_different_exporter_and_so_a_different_code() {
    run(async {
        let a = Identity::generate(0).unwrap();
        let b = Identity::generate(0).unwrap();
        let server_ep = swapdesk_trust::open_pairing_endpoint(loopback_socket(), &b).unwrap();
        let addr = server_ep.local_addr().unwrap();
        let client = client_endpoint();

        let mut eks = Vec::new();
        for _ in 0..2 {
            let ep = server_ep.clone();
            let accept = tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() });
            let conn = client
                .connect_with(pairing_client_config(&a).unwrap(), addr, swapdesk_trust::tls::SERVER_NAME)
                .unwrap()
                .await
                .unwrap();
            let server_conn = accept.await.unwrap();
            let (mut ek_c, mut ek_s) = ([0u8; EXPORTER_LEN], [0u8; EXPORTER_LEN]);
            conn.export_keying_material(&mut ek_c, EXPORTER_LABEL, b"").unwrap();
            server_conn.export_keying_material(&mut ek_s, EXPORTER_LABEL, b"").unwrap();
            assert_eq!(ek_c, ek_s, "both ends of one TLS session export the same bytes");
            eks.push(ek_c);
            conn.close(0u32.into(), b"");
        }
        assert_ne!(eks[0], eks[1], "another TLS session exports other bytes");
        let (_, si, sr, ni, nr) = base();
        assert_ne!(compute_sas(&eks[0], &si, &sr, &ni, &nr), compute_sas(&eks[1], &si, &sr, &ni, &nr));
    });
}

#[test]
fn a_comparison_longer_than_the_idle_timeout_keeps_the_connection_alive() {
    run(async {
        // The pairing transport idles out after 10 s; the keep-alive (2 s) must carry a slow comparison past it.
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::holding());
        a.open_pairing_mode();
        b.open_pairing_mode();
        let server = start_responder(&b);
        let client = client_endpoint();
        let initiator = {
            let (ctx, client, addr) = (a.ctx.clone(), client.clone(), server.addr);
            tokio::spawn(async move {
                connect_and_initiate(&client, &ctx, addr, InitiateOptions::default()).await.map(|_| ())
            })
        };
        b.ui.wait_announced().await;
        tokio::time::sleep(Duration::from_secs(12)).await;
        b.ui.answer_held(true);
        assert!(initiator.await.unwrap().is_ok(), "the connection idled out during the comparison");
        assert!(b.ui.wait_ended().await.ok);
        assert_eq!(a.disk().devices.len(), 1);
    });
}

// ---------- stage 34 ----------

/// Starts a real ceremony from `a` to `b` and returns once `b`'s screen shows "Parear com <a>" (the attempt owns
/// the machine's pairing slot, `b`'s user has not answered).
async fn start_held_ceremony(
    a: &Node,
    b: &Node,
    server: &Responder,
) -> tokio::task::JoinHandle<Result<(), TrustError>> {
    let (ctx, client, addr) = (a.ctx.clone(), client_endpoint(), server.addr);
    let initiator =
        tokio::spawn(
            async move { connect_and_initiate(&client, &ctx, addr, InitiateOptions::default()).await.map(|_| ()) },
        );
    b.ui.wait_announced().await;
    until("the code is on b's screen", || b.ui.last_code().is_some()).await;
    initiator
}

#[test]
fn a_garbage_connection_and_a_busy_second_connection_leave_the_real_ceremony_running() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::holding());
        a.open_pairing_mode();
        b.open_pairing_mode();
        let server = start_responder(&b);
        let initiator = start_held_ceremony(&a, &b, &server).await;

        // Garbage: a well-formed frame of the wrong kind where `PairHello` belongs.
        let (mut garbage, _g) = manual_connect(server.addr).await;
        write_msg(&mut garbage.send, PairConfirm { matches: true }).await;
        match read_msg(&mut garbage.recv).await {
            Message::PairResult(PairResult { ok: false, .. }) => {}
            other => panic!("expected a refusal, got {other:?}"),
        }

        // A second pairing while one runs: BUSY.
        let (mut second, _s) = manual_connect(server.addr).await;
        write_msg(&mut second.send, hello(commitment(second.identity.spki(), &[1; NONCE_LEN]))).await;
        match read_msg(&mut second.recv).await {
            Message::PairResult(PairResult { ok: false, code: ErrorCode::Busy }) => {}
            other => panic!("expected BUSY, got {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Neither closed the mode nor ended the screen of the ceremony that owns the slot.
        assert!(b.mode.is_open(), "a refused connection closed pairing mode under the running ceremony");
        assert!(b.ui.last_ended().is_none(), "a refused connection ended the running ceremony's screen");
        let disk = b.disk();
        assert!(disk.lockout.provisional.is_some() && disk.lockout.failures.is_empty(), "{:?}", disk.lockout);

        // The real ceremony goes on to the end.
        b.ui.answer_held(true);
        assert!(initiator.await.unwrap().is_ok());
        assert!(b.ui.wait_ended().await.ok);
        assert_eq!(b.disk().devices.len(), 1);
        assert!(b.disk().lockout.failures.is_empty());
    });
}

#[test]
fn one_pairing_guard_covers_the_initiator_and_the_responder_role() {
    run(async {
        // Direction 1: `b` answers a ceremony; its own user starts one toward `c` meanwhile. Refused as BUSY, and
        // `b`'s provisional failure is not settled as a failure by it.
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::holding());
        let c = Node::new("c", TestUi::answering(true));
        a.open_pairing_mode();
        b.open_pairing_mode();
        c.open_pairing_mode();
        let b_server = start_responder(&b);
        let c_server = start_responder(&c);
        let running = start_held_ceremony(&a, &b, &b_server).await;
        let local = connect_and_initiate(&client_endpoint(), &b.ctx, c_server.addr, InitiateOptions::default()).await;
        assert!(matches!(local, Err(TrustError::Protocol(ErrorCode::Busy))), "{local:?}");
        let disk = b.disk();
        assert!(
            disk.lockout.provisional.is_some() && disk.lockout.failures.is_empty(),
            "the running attempt's provisional failure was touched: {:?}",
            disk.lockout
        );
        assert!(
            c.ui.announced.lock().unwrap().is_empty() && c.disk().devices.is_empty(),
            "the second attempt reached the peer"
        );
        assert!(b.mode.is_open() && b.ui.last_ended().is_none());
        b.ui.answer_held(true);
        assert!(running.await.unwrap().is_ok());
        let disk = b.disk();
        assert!(
            disk.lockout.failures.is_empty() && disk.lockout.provisional.is_none() && disk.devices.len() == 1,
            "{:?}",
            disk.lockout
        );

        // Direction 2: `m` is the initiator of a ceremony that waits for both users; a connection arrives on its
        // own pairing endpoint. BUSY, and again nothing of the running attempt is settled.
        let m = Node::new("m", TestUi::holding());
        let p = Node::new("p", TestUi::holding());
        m.open_pairing_mode();
        p.open_pairing_mode();
        let m_server = start_responder(&m);
        let p_server = start_responder(&p);
        let (ctx, client, addr) = (m.ctx.clone(), client_endpoint(), p_server.addr);
        let initiating = tokio::spawn(async move {
            connect_and_initiate(&client, &ctx, addr, InitiateOptions::default()).await.map(|_| ())
        });
        m.ui.wait_announced().await;
        until("both codes are shown", || m.ui.last_code().is_some() && p.ui.last_code().is_some()).await;
        let (mut visitor, _v) = manual_connect(m_server.addr).await;
        write_msg(&mut visitor.send, hello(commitment(visitor.identity.spki(), &[2; NONCE_LEN]))).await;
        match read_msg(&mut visitor.recv).await {
            Message::PairResult(PairResult { ok: false, code: ErrorCode::Busy }) => {}
            other => panic!("expected BUSY from the machine that is itself pairing, got {other:?}"),
        }
        let disk = m.disk();
        assert!(disk.lockout.provisional.is_some() && disk.lockout.failures.is_empty(), "{:?}", disk.lockout);
        m.ui.answer_held(true);
        p.ui.answer_held(true);
        assert!(initiating.await.unwrap().is_ok());
        let disk = m.disk();
        assert!(
            disk.lockout.failures.is_empty() && disk.lockout.provisional.is_none() && disk.devices.len() == 1,
            "{:?}",
            disk.lockout
        );
    });
}

#[test]
fn an_incoming_after_the_mode_ended_gets_no_answer_at_all() {
    run(async {
        // The mode ends while a ceremony still runs (the endpoint stays up for it): a new connection is ignored,
        // not refused, so nothing tells the sender that anything listens.
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::holding());
        a.open_pairing_mode();
        b.open_pairing_mode();
        let server = start_responder(&b);
        let initiator = start_held_ceremony(&a, &b, &server).await;
        b.mode.close();

        let identity = Identity::generate(0).unwrap();
        let connecting = client_endpoint()
            .connect_with(pairing_client_config(&identity).unwrap(), server.addr, swapdesk_trust::tls::SERVER_NAME)
            .unwrap();
        let attempt = tokio::time::timeout(Duration::from_millis(1500), connecting).await;
        assert!(attempt.is_err(), "the unsolicited connection got an answer: {:?}", attempt.map(|r| r.map(|_| ())));

        b.ui.answer_held(true);
        assert!(initiator.await.unwrap().is_ok());
    });
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never happened: {what}");
}

#[test]
fn a_filtered_address_is_never_handshaken() {
    use std::sync::{Arc, Mutex};
    use swapdesk_trust::{Decision, IncomingFilter, ServeOptions};
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        a.open_pairing_mode();
        b.open_pairing_mode();
        let ignored_client = client_endpoint();
        let refused_client = client_endpoint();
        let (ignored_port, refused_port) =
            (ignored_client.local_addr().unwrap().port(), refused_client.local_addr().unwrap().port());
        let judged = Arc::new(Mutex::new(Vec::new()));
        let filter: IncomingFilter = {
            let judged = Arc::clone(&judged);
            Arc::new(move |incoming: &quinn::Incoming| {
                let port = incoming.remote_address().port();
                judged.lock().unwrap().push(port);
                match port {
                    p if p == ignored_port => Decision::Ignore,
                    p if p == refused_port => Decision::Refuse,
                    _ => Decision::Accept,
                }
            })
        };
        let server = start_responder_with(&b, ServeOptions { filter: Some(filter), ..ServeOptions::default() });

        // Ignored: nothing comes back, not even a refusal.
        let attempt = tokio::time::timeout(
            Duration::from_millis(1500),
            connect_and_initiate(&ignored_client, &a.ctx, server.addr, InitiateOptions::default()),
        )
        .await;
        assert!(attempt.is_err(), "an ignored address got an answer: {attempt:?}");
        // Refused: an answer at once, but no handshake and no pairing.
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            connect_and_initiate(&refused_client, &a.ctx, server.addr, InitiateOptions::default()),
        )
        .await;
        assert!(matches!(refused, Ok(Err(_))), "{refused:?}");
        for port in [ignored_port, refused_port] {
            assert!(judged.lock().unwrap().contains(&port), "the filter never saw port {port}");
        }
        assert_eq!(server.service.in_flight(), 0);
        assert!(
            b.audit.entries().is_empty() && b.ui.announced.lock().unwrap().is_empty(),
            "a filtered address reached the pairing code"
        );
        assert!(
            b.log.lines().iter().all(|l| !l.text.contains("handshake")),
            "a handshake was started for a filtered address: {:?}",
            b.log.lines()
        );

        // Anyone else pairs as usual, through the same service.
        assert!(
            connect_and_initiate(&client_endpoint(), &a.ctx, server.addr, InitiateOptions::default()).await.is_ok()
        );
        assert_eq!(b.disk().devices.len(), 1);
        until("the responder audited the pairing", || !b.audit.entries().is_empty()).await;
        assert_eq!(b.audit.entries().len(), 1);
    });
}

#[test]
fn the_cap_on_concurrent_handshakes_holds() {
    use swapdesk_trust::ServeOptions;
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::holding());
        a.open_pairing_mode();
        b.open_pairing_mode();
        // The hook where a per-address turn is spent runs only for attempts that got a place under the cap.
        let admitted_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook: swapdesk_trust::AdmittedHook = {
            let n = std::sync::Arc::clone(&admitted_count);
            std::sync::Arc::new(move |_: &quinn::Incoming| {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        };
        let server = start_responder_with(
            &b,
            ServeOptions { max_in_flight: 2, admitted: Some(hook), ..ServeOptions::default() },
        );
        let initiator = start_held_ceremony(&a, &b, &server).await;
        assert_eq!(server.service.in_flight(), 1);

        // A second connection handshakes and sits there, saying nothing: the cap is reached.
        let (second, _s) = manual_connect(server.addr).await;
        until("two in flight", || server.service.in_flight() == 2).await;

        // A third is ignored before any handshake: nothing answers, and the count does not move.
        let identity = Identity::generate(0).unwrap();
        let third = client_endpoint()
            .connect_with(pairing_client_config(&identity).unwrap(), server.addr, swapdesk_trust::tls::SERVER_NAME)
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), third).await.is_err(),
            "a connection over the cap was handshaken"
        );
        assert_eq!(server.service.in_flight(), 2);
        assert_eq!(
            admitted_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "an attempt turned away by the cap was reported as admitted (its turn would be spent)"
        );

        // Room again once the second goes away: a new connection is handshaken (and told BUSY by the ceremony).
        second.conn.close(0u32.into(), b"");
        until("one in flight", || server.service.in_flight() == 1).await;
        let (mut fourth, _f) = manual_connect(server.addr).await;
        write_msg(&mut fourth.send, hello(commitment(fourth.identity.spki(), &[3; NONCE_LEN]))).await;
        match read_msg(&mut fourth.recv).await {
            Message::PairResult(PairResult { ok: false, code: ErrorCode::Busy }) => {}
            other => panic!("expected BUSY, got {other:?}"),
        }
        b.ui.answer_held(true);
        assert!(initiator.await.unwrap().is_ok());
    });
}

#[test]
fn the_initiator_honours_the_device_its_mode_names() {
    run(async {
        // The user picked a device (not this one) when opening "Parear": the call names nothing, the mode does.
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        a.mode.enter(ModeOptions { expect: Some([9; 16]), replaces: None });
        b.open_pairing_mode();
        let server = start_responder(&b);
        let r = connect_and_initiate(&client_endpoint(), &a.ctx, server.addr, InitiateOptions::default()).await;
        assert!(matches!(r, Err(TrustError::Protocol(ErrorCode::NotPermitted))), "{r:?}");
        assert!(a.disk().devices.is_empty() && b.disk().devices.is_empty());

        // The mode names the right device: it pairs.
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        a.mode.enter(ModeOptions { expect: Some(b.identity.device_id()), replaces: None });
        b.open_pairing_mode();
        let server = start_responder(&b);
        assert!(
            connect_and_initiate(&client_endpoint(), &a.ctx, server.addr, InitiateOptions::default()).await.is_ok()
        );

        // The call's own choice wins over the mode's.
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        a.mode.enter(ModeOptions { expect: Some([9; 16]), replaces: None });
        b.open_pairing_mode();
        let server = start_responder(&b);
        let options = InitiateOptions { expect: Some(b.identity.device_id()), replaces: None };
        assert!(connect_and_initiate(&client_endpoint(), &a.ctx, server.addr, options).await.is_ok());
    });
}

// ---------- stage 66: per-address limits (B9) and the initiator's choice (B12) ----------

/// A pairing handshake attempted from a fresh client endpoint (one more source port, the same source address).
async fn try_handshake(addr: std::net::SocketAddr, within: Duration) -> Result<quinn::Connection, &'static str> {
    let identity = Identity::generate(0).unwrap();
    let connecting = client_endpoint()
        .connect_with(pairing_client_config(&identity).unwrap(), addr, swapdesk_trust::tls::SERVER_NAME)
        .unwrap();
    match tokio::time::timeout(within, connecting).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(_)) => Err("refused"),
        Err(_) => Err("no answer"),
    }
}

#[test]
fn a_second_pairing_attempt_from_one_address_inside_thirty_seconds_is_ignored_without_an_audit_entry() {
    use std::sync::{Arc, Mutex};
    use swapdesk_trust::{Decision, IncomingFilter, PairingRate, ServeOptions};
    run(async {
        let b = Node::new("b", TestUi::answering(true));
        b.open_pairing_mode();
        let rate = Arc::new(PairingRate::new());
        let judged = Arc::new(Mutex::new(Vec::new()));
        let filter: IncomingFilter = {
            let (rate, judged) = (Arc::clone(&rate), Arc::clone(&judged));
            Arc::new(move |incoming: &quinn::Incoming| {
                let validated = incoming.remote_address_validated();
                let decision = rate.judge(incoming);
                judged.lock().unwrap().push((validated, decision));
                decision
            })
        };
        let admitted: swapdesk_trust::AdmittedHook = {
            let rate = Arc::clone(&rate);
            Arc::new(move |incoming: &quinn::Incoming| rate.note(incoming))
        };
        let server = start_responder_with(
            &b,
            ServeOptions { filter: Some(filter), admitted: Some(admitted), ..ServeOptions::default() },
        );

        // The first attempt: the source is retried to prove its address (no turn spent), then handshakes.
        let first = try_handshake(server.addr, Duration::from_secs(5)).await.expect("the first attempt is served");
        assert_eq!(*judged.lock().unwrap(), [(false, Decision::Retry), (true, Decision::Accept)]);

        // A second attempt from the same address (another port) inside 30 s: retried, then ignored: nothing answers.
        let second = try_handshake(server.addr, Duration::from_millis(1500)).await;
        assert_eq!(second.err(), Some("no answer"), "the second attempt was answered");
        // (The client retransmits its Initial while it waits, so the ignored decision may repeat.)
        let mut decisions = judged.lock().unwrap().clone();
        decisions.dedup();
        assert_eq!(
            decisions,
            [(false, Decision::Retry), (true, Decision::Accept), (false, Decision::Retry), (true, Decision::Ignore)]
        );
        assert_eq!(server.service.in_flight(), 1, "only the first attempt holds a place");
        assert!(b.audit.entries().is_empty(), "an ignored attempt wrote an audit entry: {:?}", b.audit.entries());
        drop(first);
    });
}

#[test]
fn every_connection_is_retried_first_and_the_fifth_validated_one_from_an_address_is_ignored() {
    use std::sync::{Arc, Mutex};
    use swapdesk_trust::{Admission, MAX_PER_ADDRESS, SessionLimits, open_pairing_endpoint, turn_away};
    run(async {
        let b = Node::new("b", TestUi::answering(true));
        let endpoint = open_pairing_endpoint(loopback_socket(), &b.identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let limits = SessionLimits::new();
        let seen = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        // The accept loop of the session endpoint in miniature: the limit first, then the handshake; an admitted
        // connection keeps its place for as long as the test holds it.
        let held = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let (limits, seen, held) = (limits.clone(), Arc::clone(&seen), Arc::clone(&held));
            tokio::spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    match limits.judge(&incoming) {
                        Admission::Admit(slot) => {
                            seen.lock().unwrap().push("admit");
                            if let Ok(connecting) = incoming.accept()
                                && let Ok(conn) = connecting.await
                            {
                                held.lock().unwrap().push((conn, slot));
                            }
                        }
                        Admission::Retry => {
                            seen.lock().unwrap().push("retry");
                            turn_away(incoming, true);
                        }
                        Admission::Ignore => {
                            seen.lock().unwrap().push("ignore");
                            turn_away(incoming, false);
                        }
                    }
                }
            })
        };

        let local = std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST);
        let mut clients = Vec::new();
        for n in 0..MAX_PER_ADDRESS {
            clients.push(
                try_handshake(addr, Duration::from_secs(5)).await.unwrap_or_else(|e| panic!("connection {n}: {e}")),
            );
        }
        assert_eq!(limits.live_for(local), MAX_PER_ADDRESS);

        // The fifth: the unvalidated Initial is retried, the validated one is ignored, and no handshake finishes.
        let fifth = try_handshake(addr, Duration::from_millis(1500)).await;
        assert_eq!(fifth.err(), Some("no answer"), "the fifth connection was answered");
        // Every connection was retried to prove its address first; only the four validated ones were admitted, and
        // the fifth, validated, was ignored. (The client retransmits its Initial while it waits, so a decision may
        // repeat.)
        let log = seen.lock().unwrap().clone();
        let count = |what: &str| log.iter().filter(|s| **s == what).count();
        assert_eq!(count("admit"), MAX_PER_ADDRESS, "{log:?}");
        assert!(count("retry") > MAX_PER_ADDRESS, "every attempt, the fifth too, was retried first: {log:?}");
        assert_eq!(log.last(), Some(&"ignore"), "{log:?}");
        assert_eq!(limits.live_for(local), MAX_PER_ADDRESS);

        // A connection that ends gives its place back: the next one is served.
        let (conn, slot) = held.lock().unwrap().pop().expect("a held connection");
        conn.close(0u32.into(), b"");
        drop(slot);
        drop(conn);
        try_handshake(addr, Duration::from_secs(5)).await.expect("a place was freed");
        server.abort();
    });
}

#[test]
fn an_initiator_whose_chosen_device_is_not_the_one_that_answers_counts_no_failure() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        // The user picked a device (`[9; 16]`); a machine announcing `pair=1` with another key answers. The initiator
        // ends the attempt at the key check, before the responder commits to a nonce: nothing counts against it.
        let (ra, _) = pair_once(&a, &b, InitiateOptions { expect: Some([9; 16]), replaces: None }).await;
        assert!(matches!(ra, Err(TrustError::Protocol(ErrorCode::NotPermitted))), "{ra:?}");
        let disk = a.disk();
        assert!(disk.lockout.failures.is_empty() && disk.lockout.provisional.is_none(), "{:?}", disk.lockout);
        assert!(b.disk().lockout.failures.is_empty(), "the responder never reached step 2 either");
    });
}
