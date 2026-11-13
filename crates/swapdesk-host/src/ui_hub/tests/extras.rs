//! The proof of stage 41 through two hubs on two cores (127.0.0.1, the development option), each with a fake UI over
//! the UI pipe: swap both ways (F-SWAP-01) and its refusals (F-SWAP-02), chat both ways with its read state and its
//! end (F-CHAT-01), the resume of a session whose link was cut, with no new prompt (F-NET-01), and Wake-on-LAN
//! through a recording socket (F-WAKE-01).

use std::collections::HashMap;
use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};

use swapdesk_extras::wake::{WakeConfig, WakeSocket};
use swapdesk_ipc::{SwapNote, SwapWhy};

use super::*;

/// What the host user's grants to the viewer machine are, and what the viewer's grants to the host machine are
/// (each side needs `CONTROL` stored for the other, or a swap cannot make it the host: PROTOCOL.md 6.3 step 1).
pub(super) const CV: PermissionSet = PermissionSet::CONTROL;
const SWAP_ASK_OP: u16 = 22;
const CHAT_SEND_OP: u16 = 24;
const WAKE_OP: u16 = 26;

/// A host machine and a viewer machine that have paired, with a UI each at the console.
struct Duo {
    host: Machine,
    viewer: Machine,
    host_ui: Client,
    viewer_ui: Client,
}

impl Duo {
    fn new() -> Self {
        Self::with_helpers(false)
    }

    fn with_helpers(helpers: bool) -> Self {
        let mut host = Machine::new("host-pc");
        let viewer = Machine::new("viewer-pc");
        host.pin(&viewer, ALL);
        viewer.pin(&host, CV);
        if helpers {
            host.attach_helper(IpcRole::HelperSystem);
            host.attach_helper(IpcRole::HelperUser);
        }
        viewer.sees(&host, false);
        host.sees(&viewer, false);
        host.hub.set_console_session(Some(1));
        viewer.hub.set_console_session(Some(1));
        let host_ui = host.client(info(false));
        let viewer_ui = viewer.client(info(false));
        Self { host, viewer, host_ui, viewer_ui }
    }

    /// The viewer connects to the host and the host's user accepts. Returns the session id.
    fn open(&mut self) -> u64 {
        let req = self.viewer_ui.request(UiOp::Connect {
            device_id: self.host.hex(),
            intent: Intent::Control,
            unattended: false,
        });
        let incoming = self.host_ui.notify("IncomingRequest", |n| match n {
            Notify::IncomingRequest(r) => Some(r.clone()),
            _ => None,
        });
        assert!(
            self.host_ui
                .call(UiOp::AnswerIncoming {
                    request_id: incoming.request_id,
                    decision: Decision::Accept,
                    grants: ALL.bits()
                })
                .is_ok()
        );
        let id = self.viewer_ui.connected(req);
        active_as(&mut self.viewer_ui, Role::Viewer, &[]);
        active_as(&mut self.host_ui, Role::Host, &[]);
        id
    }

    fn finish(self) {
        let Duo { host, viewer, host_ui, viewer_ui } = self;
        drop((host_ui, viewer_ui));
        viewer.finish();
        host.finish();
    }
}

/// The id of the next session that goes active in `role` on this UI, other than the ones in `not`.
pub(super) fn active_as(ui: &mut Client, role: Role, not: &[u64]) -> u64 {
    ui.notify("an active session", |n| match n {
        Notify::Session { session_id, state, .. }
            if !not.contains(session_id) && grants_active(state).is_some_and(|(r, _)| r == role) =>
        {
            Some(*session_id)
        }
        _ => None,
    })
}

pub(super) fn ended(ui: &mut Client, session_id: u64) -> Option<ApiCode> {
    ui.notify("the end of a session", |n| match n {
        Notify::Session { session_id: id, state: SessionState::Ended { reason }, .. } if *id == session_id => {
            Some(*reason)
        }
        _ => None,
    })
}

fn ask_card(m: &Ipc) -> bool {
    matches!(m, Ipc::UiNotify(Notify::Swap { note: SwapNote::Asked { .. }, .. }))
}

fn asked(ui: &mut Client, session_id: u64) -> (u32, String, u16) {
    ui.notify("the swap card", |n| match n {
        Notify::Swap { session_id: id, note: SwapNote::Asked { request_id, by, remaining_s } } if *id == session_id => {
            Some((*request_id, by.clone(), *remaining_s))
        }
        _ => None,
    })
}

fn swap_note(ui: &mut Client, session_id: u64) -> SwapNote {
    ui.notify("a swap note", |n| match n {
        Notify::Swap { session_id: id, note } if *id == session_id && !matches!(note, SwapNote::Asked { .. }) => {
            Some(note.clone())
        }
        _ => None,
    })
}

fn request_swap(ui: &mut Client, session_id: u64) {
    let req = ui.request(UiOp::RequestSwap { session_id });
    assert!(
        matches!(ui.reply(req, SWAP_ASK_OP), Ok(ReplyBody::RequestId(_))),
        "the reply comes at once with a request number"
    );
}

// ---------- F-SWAP-01: accepted, both ways ----------

#[test]
fn swap_runs_both_ways_and_the_card_goes_only_to_the_uis_it_was_sent_to() {
    let mut d = Duo::new();
    let first = d.open();

    // Swap 1: the viewer presses "Inverter", the host's user accepts, the host starts viewing.
    request_swap(&mut d.viewer_ui, first);
    let (card, by, remaining) = asked(&mut d.host_ui, first);
    assert_eq!(by, "other-pc", "the card names the machine that asks");
    assert!((1..=20).contains(&remaining), "the 20 s countdown: {remaining}");
    // A UI the card did not go to cannot answer it.
    let mut stranger =
        d.host.client(ConnInfo { session_id: 9, user: user("S-1-5-21-2"), admin: false, can_elevate: false });
    assert_eq!(stranger.call(UiOp::AnswerSwap { request_id: card, accept: true }).unwrap_err(), ApiCode::NotPermitted);
    assert_eq!(
        d.host_ui.call(UiOp::AnswerSwap { request_id: card + 100, accept: true }).unwrap_err(),
        ApiCode::NotFound
    );
    assert!(matches!(d.host_ui.call(UiOp::AnswerSwap { request_id: card, accept: true }).unwrap(), ReplyBody::Done));
    assert_eq!(swap_note(&mut d.viewer_ui, first), SwapNote::Done, "the asker's card closes");
    assert_eq!(ended(&mut d.viewer_ui, first), None, "a swap is a normal end");
    assert_eq!(ended(&mut d.host_ui, first), None);
    let second = active_as(&mut d.host_ui, Role::Viewer, &[first]);
    assert_eq!(active_as(&mut d.viewer_ui, Role::Host, &[first]), second, "one new session, seen from both sides");

    // Swap 2, the other way: the host of now (the old viewer) is asked by the viewer of now.
    request_swap(&mut d.host_ui, second);
    let (card, by, _) = asked(&mut d.viewer_ui, second);
    assert_eq!(by, "other-pc");
    assert!(d.viewer_ui.call(UiOp::AnswerSwap { request_id: card, accept: true }).is_ok());
    assert_eq!(swap_note(&mut d.host_ui, second), SwapNote::Done);
    // The first session may still report itself once more on its way out: only a new id counts.
    let third = active_as(&mut d.viewer_ui, Role::Viewer, &[first, second]);
    assert_eq!(active_as(&mut d.host_ui, Role::Host, &[first, second]), third);
    assert!(![first, second].contains(&third));
    // The activity log of the machine that went back to viewing says it swapped.
    wait_for("the swap in the audit", || {
        d.viewer.audit.entries().iter().any(|e| matches!(e.event, AuditEvent::Swapped)).then_some(())
    });
    drop(stranger);
    d.finish();
}

#[test]
fn the_host_can_ask_too_and_the_viewer_becomes_the_host() {
    let mut d = Duo::new();
    let first = d.open();
    // The banner's "Inverter": the host asks, the viewer's UI owns the card.
    request_swap(&mut d.host_ui, first);
    let (card, by, _) = asked(&mut d.viewer_ui, first);
    assert_eq!(by, "other-pc");
    assert!(d.viewer_ui.call(UiOp::AnswerSwap { request_id: card, accept: true }).is_ok());
    assert_eq!(swap_note(&mut d.host_ui, first), SwapNote::Done);
    let second = active_as(&mut d.host_ui, Role::Viewer, &[first]);
    assert_eq!(active_as(&mut d.viewer_ui, Role::Host, &[first]), second);
    d.finish();
}

// ---------- F-SWAP-02: refused, and why ----------

#[test]
fn a_declined_or_refused_swap_leaves_the_session_alone_and_says_why() {
    let mut d = Duo::new();
    let s = d.open();

    // The other user says no.
    request_swap(&mut d.viewer_ui, s);
    let (card, _, _) = asked(&mut d.host_ui, s);
    assert!(d.host_ui.call(UiOp::AnswerSwap { request_id: card, accept: false }).is_ok());
    assert_eq!(swap_note(&mut d.viewer_ui, s), SwapNote::Declined { why: SwapWhy::Declined });
    // And the other way round.
    request_swap(&mut d.host_ui, s);
    let (card, _, _) = asked(&mut d.viewer_ui, s);
    assert!(d.viewer_ui.call(UiOp::AnswerSwap { request_id: card, accept: false }).is_ok());
    assert_eq!(swap_note(&mut d.host_ui, s), SwapNote::Declined { why: SwapWhy::Declined });

    // Nobody at the asked side: refused at once, without a card.
    d.host.hub.set_console_session(None);
    request_swap(&mut d.viewer_ui, s);
    assert_eq!(swap_note(&mut d.viewer_ui, s), SwapNote::Declined { why: SwapWhy::NobodyPresent });
    d.host.hub.set_console_session(Some(1));

    // The side about to be controlled has not granted CONTROL: refused at once, nothing is sent to the other side.
    d.viewer
        .trust
        .update(|st| st.devices.iter_mut().for_each(|dev| dev.grants_here = PermissionSet::EMPTY))
        .expect("store");
    request_swap(&mut d.viewer_ui, s);
    assert_eq!(swap_note(&mut d.viewer_ui, s), SwapNote::Declined { why: SwapWhy::NotPermitted });
    assert!(
        d.host_ui.nothing_within(Duration::from_millis(300), |m| matches!(
            m,
            Ipc::UiNotify(Notify::Swap { note: SwapNote::Asked { .. }, .. })
        )),
        "no prompt at the other side"
    );

    // The session was never touched: its UI still drives it.
    assert!(d.viewer_ui.call(UiOp::SelectDisplay { session_id: s, display: 0 }).is_ok());
    // A session that is not this UI's own is not its to swap.
    assert_eq!(d.viewer_ui.call(UiOp::RequestSwap { session_id: s + 1 }).unwrap_err(), ApiCode::NotFound);
    d.finish();
}

// ---------- swap across systems (F-XSWAP-01, 1.1) ----------

#[test]
fn a_swap_toward_another_os_asks_like_any_other() {
    use swapdesk_proto::Os;
    let mut d = Duo::new();
    let s = d.open();
    // Both records say the other side is a Mac (this test machine is Windows): the question reaches the user.
    d.viewer.trust.update(|st| st.devices.iter_mut().for_each(|dev| dev.os = Os::MacOs)).expect("store");
    d.host.trust.update(|st| st.devices.iter_mut().for_each(|dev| dev.os = Os::MacOs)).expect("store");
    request_swap(&mut d.viewer_ui, s);
    let (card, _, _) = asked(&mut d.host_ui, s);
    assert!(d.host_ui.call(UiOp::AnswerSwap { request_id: card, accept: false }).is_ok());
    assert!(matches!(swap_note(&mut d.viewer_ui, s), SwapNote::Declined { .. }));
    d.finish();
}

// ---------- F-CHAT-01 ----------

#[test]
fn chat_travels_both_ways_with_its_read_state_and_vanishes_when_the_session_ends() {
    let mut d = Duo::new();
    let s = d.open();

    let send =
        |ui: &mut Client, text: &str| ui.call(UiOp::ChatSend { session_id: s, text: swapdesk_ipc::Text(text.into()) });
    let ReplyBody::Chat(mine) = send(&mut d.viewer_ui, "oi").unwrap() else { panic!() };
    assert_eq!((mine.msg_id, mine.text.0.as_str()), (1, "oi"), "the reply is the message as stored");
    let theirs = d.host_ui.notify("the message at the host", |n| match n {
        Notify::Chat { session_id, item } if *session_id == s => Some(item.clone()),
        _ => None,
    });
    assert_eq!((theirs.msg_id, theirs.text.0.as_str(), theirs.sent_at_ms), (1, "oi", mine.sent_at_ms));
    // The host's user reads it: the viewer is told.
    assert!(d.host_ui.call(UiOp::ChatMarkRead { session_id: s }).is_ok());
    let up_to = d.viewer_ui.notify("read by the peer", |n| match n {
        Notify::ChatRead { session_id, up_to } if *session_id == s => Some(*up_to),
        _ => None,
    });
    assert_eq!(up_to, 1);
    // And the other way.
    let ReplyBody::Chat(back) = send(&mut d.host_ui, "olá").unwrap() else { panic!() };
    assert_eq!(back.text.0, "olá");
    let got = d.viewer_ui.notify("the message at the viewer", |n| match n {
        Notify::Chat { session_id, item } if *session_id == s => Some(item.clone()),
        _ => None,
    });
    assert_eq!(got.text.0, "olá");
    assert_eq!(d.viewer.core.chat_entries(s).len(), 2);
    assert_eq!(d.host.core.chat_entries(s).len(), 2);

    // Empty text and unknown sessions are refused; another connection's session id is not a capability.
    assert_eq!(send(&mut d.viewer_ui, "   ").unwrap_err(), ApiCode::Invalid);
    assert_eq!(
        d.viewer_ui.call(UiOp::ChatSend { session_id: s + 1, text: swapdesk_ipc::Text("x".into()) }).unwrap_err(),
        ApiCode::NotFound
    );
    let mut other = d.viewer.client(info(false));
    assert_eq!(
        other.call(UiOp::ChatSend { session_id: s, text: swapdesk_ipc::Text("x".into()) }).unwrap_err(),
        ApiCode::NotFound
    );
    assert_eq!(other.call(UiOp::ChatMarkRead { session_id: s }).unwrap_err(), ApiCode::NotFound);
    let _ = CHAT_SEND_OP;

    // The messages vanish with the session.
    assert!(d.viewer_ui.call(UiOp::Disconnect { session_id: s }).is_ok());
    assert_eq!(ended(&mut d.viewer_ui, s), None);
    wait_for("the chat to vanish on both machines", || {
        (d.viewer.core.chat_entries(s).is_empty() && d.host.core.chat_entries(s).is_empty()).then_some(())
    });
    drop(other);
    d.finish();
}

// ---------- F-NET-01: the link is cut, the session comes back without a new prompt ----------

/// A UDP relay between the viewer and the host's endpoint that can be cut: while it is, nothing crosses either way,
/// as a Wi-Fi that dropped. Every client address gets its own upstream socket, so a new dial gets through once the
/// relay is open again.
struct Relay {
    port: u16,
    cut: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Relay {
    fn start(target: SocketAddr) -> Self {
        let front = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("relay socket");
        front.set_read_timeout(Some(Duration::from_millis(50))).expect("timeout");
        let port = front.local_addr().expect("addr").port();
        let (cut, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (cut2, stop2) = (Arc::clone(&cut), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut upstream: HashMap<SocketAddr, UdpSocket> = HashMap::new();
            let mut buf = vec![0u8; 65_536];
            while !stop2.load(Ordering::SeqCst) {
                let Ok((n, client)) = front.recv_from(&mut buf) else { continue };
                if cut2.load(Ordering::SeqCst) {
                    continue;
                }
                let up = upstream.entry(client).or_insert_with(|| {
                    let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("upstream socket");
                    s.connect(target).expect("connect");
                    s.set_read_timeout(Some(Duration::from_millis(50))).expect("timeout");
                    let (back, front, cut, stop) = (
                        s.try_clone().expect("clone"),
                        front.try_clone().expect("clone"),
                        Arc::clone(&cut2),
                        Arc::clone(&stop2),
                    );
                    std::thread::spawn(move || {
                        let mut b = vec![0u8; 65_536];
                        while !stop.load(Ordering::SeqCst) {
                            if let Ok(n) = back.recv(&mut b)
                                && !cut.load(Ordering::SeqCst)
                            {
                                let _ = front.send_to(&b[..n], client);
                            }
                        }
                    });
                    s
                });
                let _ = up.send(&buf[..n]);
            }
        });
        Self { port, cut, stop }
    }

    fn cut(&self, on: bool) {
        self.cut.store(on, Ordering::SeqCst);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_cut_link_is_resumed_with_the_same_session_and_no_new_prompt() {
    let mut d = Duo::with_helpers(true);
    // The viewer reaches the host through the relay.
    let relay = Relay::start(d.host.session_addr());
    d.viewer.presence.set(vec![PresencePeer {
        device_id: d.host.id(),
        name: "host-pc".into(),
        os: Os::Windows,
        pairing_open: false,
        addrs: vec![Ipv4Addr::LOCALHOST],
        session_port: relay.port,
        pair_port: 0,
        proto_minor: 0,
    }]);
    let s = d.open();
    d.viewer_ui.take("video before the cut", |m| {
        matches!(m, Ipc::ViewerVideo { session_id, .. } if *session_id == s).then_some(())
    });

    // The Wi-Fi drops: the idle timeout (6 s) ends the connection on both sides, and the viewer says "Reconectando N".
    relay.cut(true);
    let attempt = d.viewer_ui.notify("Reconectando", |n| match n {
        Notify::Session { session_id, state: SessionState::Reconnecting { attempt }, .. }
            if *session_id == s && *attempt >= 1 =>
        {
            Some(*attempt)
        }
        _ => None,
    });
    assert!(attempt >= 1);
    // What is in the pipe from before the cut is not what comes next.
    d.viewer_ui.clear();
    relay.cut(false);

    // The network is back: the same session returns, granted as before, and the host's user is not asked again.
    let (role, granted) = d.viewer_ui.notify("the session is back", |n| match n {
        Notify::Session { session_id, state, .. } if *session_id == s => grants_active(state),
        _ => None,
    });
    assert_eq!((role, granted), (Role::Viewer, ALL.bits()));
    assert!(
        d.host_ui
            .nothing_within(Duration::from_millis(300), |m| matches!(m, Ipc::UiNotify(Notify::IncomingRequest(_)))),
        "a resume asks nobody"
    );
    // The host's UI watched the same loss: its session was reconnecting and is active again, and it was never ended
    // (WAITING.md row 22: the banner stays and no viewer window opens on the host).
    d.host_ui.notify("the hosted session reconnecting", |n| match n {
        Notify::Session { session_id, state: SessionState::Reconnecting { .. }, .. } if *session_id == s => Some(()),
        _ => None,
    });
    let (role, granted) = d.host_ui.notify("the hosted session is back", |n| match n {
        Notify::Session { session_id, state, .. } if *session_id == s => grants_active(state),
        _ => None,
    });
    assert_eq!((role, granted), (Role::Host, ALL.bits()));
    assert!(
        d.host_ui.nothing_within(Duration::from_millis(300), |m| matches!(
            m,
            Ipc::UiNotify(Notify::Session { session_id, state: SessionState::Ended { .. }, .. }) if *session_id == s
        )),
        "a session the core resumed never ended for the host's UI"
    );
    // The handle works on the new connection: a control message, a chat message and media all cross it.
    assert!(d.viewer_ui.call(UiOp::SetAudio { session_id: s, on: true }).is_ok());
    assert!(d.viewer_ui.call(UiOp::ChatSend { session_id: s, text: swapdesk_ipc::Text("voltei".into()) }).is_ok());
    d.host_ui.notify("chat after the resume", |n| {
        matches!(n, Notify::Chat { session_id, .. } if *session_id == s).then_some(())
    });
    d.viewer_ui.take("video after the resume", |m| {
        matches!(m, Ipc::ViewerVideo { session_id, .. } if *session_id == s).then_some(())
    });
    // "Desconectar" ends it for good.
    assert!(d.viewer_ui.call(UiOp::Disconnect { session_id: s }).is_ok());
    assert_eq!(ended(&mut d.viewer_ui, s), None);
    drop(relay);
    d.finish();
}

#[test]
fn disconnect_while_reconnecting_ends_the_session_and_stops_the_attempts() {
    let mut d = Duo::new();
    let relay = Relay::start(d.host.session_addr());
    d.viewer.presence.set(vec![PresencePeer {
        device_id: d.host.id(),
        name: "host-pc".into(),
        os: Os::Windows,
        pairing_open: false,
        addrs: vec![Ipv4Addr::LOCALHOST],
        session_port: relay.port,
        pair_port: 0,
        proto_minor: 0,
    }]);
    let s = d.open();
    relay.cut(true);
    d.viewer_ui.notify("Reconectando", |n| matches!(n, Notify::Session { session_id, state: SessionState::Reconnecting { attempt }, .. } if *session_id == s && *attempt >= 1).then_some(()));
    assert!(d.viewer_ui.call(UiOp::Disconnect { session_id: s }).is_ok());
    assert_eq!(ended(&mut d.viewer_ui, s), None, "a user's own end, not a failure");
    drop(relay);
    d.finish();
}

// ---------- F-WAKE-01 ----------

/// A socket that remembers what was sent through it.
type Sent = Arc<Mutex<Vec<(Vec<u8>, SocketAddrV4)>>>;

struct RecordingWake(Sent);

impl WakeSocket for RecordingWake {
    fn send_to(&mut self, packet: &[u8], dest: SocketAddrV4) -> std::io::Result<()> {
        lock(&self.0).push((packet.to_vec(), dest));
        Ok(())
    }
}

const MAC: [u8; 6] = [0x3C, 0x52, 0x82, 0xAA, 0xBB, 0xCC];

/// A machine on `windows_typical` interfaces whose paired device `other` is asleep: its MAC and last address stored.
pub(super) fn sleeper(other: &Machine) -> (Machine, Sent) {
    let m = Machine::with_interfaces("sala", FakeNetInterfaces::windows_typical());
    m.pin(other, CV);
    m.trust
        .update(|s| {
            for d in &mut s.devices {
                d.wol_macs = vec![MAC];
                d.last_addrs = vec![[192, 168, 1, 50]];
            }
        })
        .expect("store");
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sock = Arc::clone(&sent);
    let cfg = WakeConfig {
        give_up: Duration::from_secs(3),
        resend: Duration::from_millis(100),
        poll: Duration::from_millis(20),
    };
    m.hub.set_wake(
        Arc::new(move |_route| Ok(Box::new(RecordingWake(Arc::clone(&sock))) as Box<dyn WakeSocket + Send>)),
        cfg,
    );
    (m, sent)
}

pub(super) fn waking(ui: &mut Client, id: u64) -> u16 {
    ui.notify("Acordando", |n| match n {
        Notify::Session { session_id, state: SessionState::Waking { remaining_s }, .. } if *session_id == id => {
            Some(*remaining_s)
        }
        _ => None,
    })
}

#[test]
fn wake_sends_the_magic_packet_to_the_route_broadcast_and_connects_when_the_device_appears() {
    let mut other = Machine::new("other-pc");
    let (m, sent) = sleeper(&other);
    other.pin(&m, ALL);
    other.attach_helper(IpcRole::HelperUser);
    other.hub.set_console_session(Some(1));
    m.hub.set_console_session(Some(1));
    let mut ui = m.client(info(false));
    let mut other_ui = other.client(info(false));

    // The list says it is asleep and can be woken.
    let ReplyBody::Devices(list) = ui.call(UiOp::ListDevices).unwrap() else { panic!() };
    assert_eq!((list[0].status.clone(), list[0].wakeable), (swapdesk_ipc::DeviceStatus::Asleep, true));

    let req = ui.request(UiOp::Wake { device_id: other.hex(), intent: Intent::Control });
    let ReplyBody::SessionId(wake_id) = ui.reply(req, WAKE_OP).expect("the attempt starts") else { panic!() };
    assert_eq!(waking(&mut ui, wake_id), 3, "the window the attempt runs");
    wait_for("the magic packet", || (!lock(&sent).is_empty()).then_some(()));
    {
        let sent = lock(&sent);
        let (packet, dest) = &sent[0];
        assert_eq!(
            *dest,
            SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 255), 9),
            "the broadcast of the interface that holds the last address"
        );
        assert_eq!(packet.as_slice(), swapdesk_lan::wol::magic_packet(MAC).as_slice());
    }
    wait_for("the packet is repeated", || (lock(&sent).len() >= 2).then_some(()));

    // The device shows up on the network: the hub connects, the other machine's user accepts.
    m.sees(&other, false);
    let incoming = other_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    assert!(
        other_ui
            .call(UiOp::AnswerIncoming {
                request_id: incoming.request_id,
                decision: Decision::Accept,
                grants: ALL.bits()
            })
            .is_ok()
    );
    assert_eq!(ended(&mut ui, wake_id), None, "the waking entry ends normally");
    let session = active_as(&mut ui, Role::Viewer, &[wake_id]);
    assert_ne!(session, wake_id);
    let before = lock(&sent).len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(lock(&sent).len(), before, "no packets once it appeared");
    drop((ui, other_ui));
    m.finish();
    other.finish();
}

#[test]
fn wake_gives_up_after_its_window_and_a_cancel_stops_it() {
    let other = Machine::new("other-pc");
    let (m, sent) = sleeper(&other);
    let mut ui = m.client(info(false));

    // The minute (3 s here) runs out with nobody appearing.
    let req = ui.request(UiOp::Wake { device_id: other.hex(), intent: Intent::Control });
    let ReplyBody::SessionId(id) = ui.reply(req, WAKE_OP).unwrap() else { panic!() };
    waking(&mut ui, id);
    assert_eq!(ended(&mut ui, id), Some(ApiCode::Timeout));

    // "Cancelar" stops a running attempt at once.
    let req = ui.request(UiOp::Wake { device_id: other.hex(), intent: Intent::Control });
    let ReplyBody::SessionId(id) = ui.reply(req, WAKE_OP).unwrap() else { panic!() };
    waking(&mut ui, id);
    assert!(ui.call(UiOp::Disconnect { session_id: id }).is_ok());
    assert_eq!(ended(&mut ui, id), None);
    std::thread::sleep(Duration::from_millis(150));
    let after_cancel = lock(&sent).len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(lock(&sent).len(), after_cancel, "nothing is sent after the cancel");

    // What cannot be woken is refused: unknown device, no stored hardware address, an address off every approved link.
    assert_eq!(
        ui.call(UiOp::Wake { device_id: "00".repeat(16), intent: Intent::Control }).unwrap_err(),
        ApiCode::NotFound
    );
    m.trust.update(|s| s.devices.iter_mut().for_each(|d| d.last_addrs = vec![[10, 9, 9, 9]])).expect("store");
    assert_eq!(ui.call(UiOp::Wake { device_id: other.hex(), intent: Intent::Control }).unwrap_err(), ApiCode::Refused);
    m.trust.update(|s| s.devices.iter_mut().for_each(|d| d.wol_macs.clear())).expect("store");
    assert_eq!(ui.call(UiOp::Wake { device_id: other.hex(), intent: Intent::Control }).unwrap_err(), ApiCode::Invalid);
    drop(ui);
    m.finish();
    other.finish();
}

// ---------- review: a swap that makes this machine the host is the console's to ask and to answer ----------

#[test]
fn a_ui_outside_the_console_neither_hears_nor_answers_nor_asks_for_a_swap_that_makes_this_machine_the_host() {
    let mut d = Duo::new();
    let first = d.open();

    // The user at the console of the viewer machine moves to session 2; the UI of session 1 stays connected and
    // still owns the viewer session.
    d.viewer.hub.set_console_session(Some(2));
    let mut console =
        d.viewer.client(ConnInfo { session_id: 2, user: user("S-1-5-21-3"), admin: false, can_elevate: false });

    // It cannot ask for the swap itself: that would make this machine the host, with the swap token skipping the
    // prompt and the presence check.
    assert_eq!(d.viewer_ui.call(UiOp::RequestSwap { session_id: first }).unwrap_err(), ApiCode::NotPermitted);

    // The host asks: the card goes to the console's UI, not to the UI that owns the viewer session.
    let req = d.host_ui.request(UiOp::RequestSwap { session_id: first });
    assert!(matches!(
        d.host_ui.reply(req, UiOp::RequestSwap { session_id: first }.code()),
        Ok(ReplyBody::RequestId(_))
    ));
    let (card, _, _) = console.notify("the swap card", |n| match n {
        Notify::Swap { note: SwapNote::Asked { request_id, by, remaining_s }, .. } => {
            Some((*request_id, by.clone(), *remaining_s))
        }
        _ => None,
    });
    assert!(d.viewer_ui.nothing_within(Duration::from_millis(500), ask_card), "the other session's UI got the card");
    // Nor can that UI answer it, whatever it guessed of the number.
    assert_eq!(
        d.viewer_ui.call(UiOp::AnswerSwap { request_id: card, accept: true }).unwrap_err(),
        ApiCode::NotPermitted
    );
    // The console's UI can.
    assert!(console.call(UiOp::AnswerSwap { request_id: card, accept: false }).is_ok());

    drop(console);
    d.finish();
}

/// ACCOUNTS.md 5.6 (review of S6, finding 8): the elevation kills the resume token, so after a lost link the viewer
/// enters again without a prompt, unelevated, and the button is offered again.
#[test]
fn a_reconnect_after_elevate_enters_again_unelevated() {
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, PermissionSet::UNATTENDED | PermissionSet::CONTROL);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    let relay = Relay::start(host.session_addr());
    viewer.presence.set(vec![PresencePeer {
        device_id: host.id(),
        name: "host-pc".into(),
        os: Os::Windows,
        pairing_open: false,
        addrs: vec![Ipv4Addr::LOCALHOST],
        session_port: relay.port,
        pair_port: 0,
        proto_minor: 1,
    }]);
    host.hub.set_console_session(Some(1));
    viewer.hub.set_console_session(Some(1));
    let mut viewer_ui = viewer.client(ConnInfo { can_elevate: true, ..info(false) });
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let entry = viewer_ui.entry_of(req);
    let s = active_as(&mut viewer_ui, Role::Viewer, &[entry]);
    let elevation_of = |ui: &mut Client| {
        ui.notify("the session's state", |n| match n {
            Notify::Session { session_id, state: SessionState::Active { elevation, .. }, .. } if *session_id == s => {
                Some(*elevation)
            }
            _ => None,
        })
    };
    let ReplyBody::ElevationTicket { ticket, .. } = viewer_ui
        .call(UiOp::ElevationBegin { action: swapdesk_ipc::ElevAction::ElevateSession { session_id: s } })
        .expect("a ticket")
    else {
        panic!()
    };
    viewer.hub.confirm_elevation(&ticket, &ConnInfo { admin: true, can_elevate: true, ..info(false) }, true).unwrap();
    assert!(viewer_ui.call(UiOp::ElevateSession { session_id: s }).is_ok());
    assert!(host.core.host().sessions()[0].granted.contains(PermissionSet::UNATTENDED));

    // The Wi-Fi drops, then comes back: no resume (the token died with the elevation), a new entry without a prompt.
    relay.cut(true);
    viewer_ui.notify("Reconectando", |n| match n {
        Notify::Session { session_id, state: SessionState::Reconnecting { attempt }, .. }
            if *session_id == s && *attempt >= 1 =>
        {
            Some(())
        }
        _ => None,
    });
    viewer_ui.clear();
    relay.cut(false);
    assert_eq!(elevation_of(&mut viewer_ui), 1, "back, unelevated, the button offered again");
    wait_for("the host's new session", || {
        let hosted = host.core.host().sessions();
        (hosted.len() == 1 && !hosted[0].granted.contains(PermissionSet::UNATTENDED)).then_some(())
    });
    drop(viewer_ui);
    drop(relay);
    viewer.finish();
    host.finish();
}

/// Review of S6, finding 7: once the host no longer stores `UNATTENDED` for this device, the session offers no
/// elevation (its `Elevate` would be refused).
#[test]
fn the_button_goes_when_the_host_revokes_unattended() {
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, PermissionSet::UNATTENDED | PermissionSet::CONTROL);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    viewer.sees_minor(&host, false, 1);
    host.hub.set_console_session(Some(1));
    let mut host_ui = host.client(info(true));
    let mut viewer_ui = viewer.client(info(false));
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let entry = viewer_ui.entry_of(req);
    let s = active_as(&mut viewer_ui, Role::Viewer, &[entry]);
    assert_eq!(viewer.core.viewer(s).unwrap().elevation(), 1);
    let revoke = UiOp::SetGrants { device_id: viewer.hex(), grants: PermissionSet::CONTROL.bits() };
    assert!(host_ui.call(revoke).is_ok());
    wait_for("the button to go", || (viewer.core.viewer(s)?.elevation() == 0).then_some(()));
    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}
