//! Swap and chat on the core's own API (stage 41, F-SWAP-01, F-CHAT-01): two `ServiceCore`s on 127.0.0.1 with the
//! interface scripted by [`ExtrasHooks`]. The UI hub's tests run the same flows through the UI pipe.

use std::collections::HashMap;

use swapdesk_extras::chat::ChatError;
use swapdesk_swap::{AskEnd, SwapOutcome, SwapPrompt};

use super::*;
use crate::service::{ChatNote, ChatSendError, ExtrasHooks};

type Opened = Result<(ViewerHandle, ViewerMessages), ConnectError>;

/// A machine's interface: answers the swap card, knows where to dial, keeps what it was told.
struct Scripted {
    accept: Mutex<bool>,
    addrs: Mutex<HashMap<DeviceId, SocketAddr>>,
    asked: Mutex<Vec<SwapPrompt>>,
    closed: Mutex<Vec<(u32, AskEnd)>>,
    chat: Mutex<Vec<(u64, ChatNote)>>,
    viewing: Mutex<Vec<(DeviceId, Opened)>>,
}

impl Scripted {
    fn new(accept: bool) -> Arc<Self> {
        Arc::new(Self {
            accept: Mutex::new(accept),
            addrs: Mutex::default(),
            asked: Mutex::default(),
            closed: Mutex::default(),
            chat: Mutex::default(),
            viewing: Mutex::default(),
        })
    }
}

impl ExtrasHooks for Scripted {
    fn swap_ask(&self, prompt: SwapPrompt) -> oneshot::Receiver<bool> {
        lock(&self.asked).push(prompt);
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(*lock(&self.accept));
        rx
    }

    fn swap_ask_closed(&self, request_id: u32, how: AskEnd) {
        lock(&self.closed).push((request_id, how));
    }

    fn address_of(&self, device: &DeviceId) -> Option<SocketAddr> {
        lock(&self.addrs).get(device).copied()
    }

    fn swap_viewing(&self, device: DeviceId, opened: Opened) {
        lock(&self.viewing).push((device, opened));
    }

    fn chat(&self, session_id: u64, note: ChatNote) {
        lock(&self.chat).push((session_id, note));
    }
}

/// The two cores, paired with `CONTROL` stored on both sides, each with its scripted interface.
struct Pair {
    host: Machine,
    viewer: Machine,
    host_hooks: Arc<Scripted>,
    viewer_hooks: Arc<Scripted>,
}

impl Pair {
    fn new(host_accepts: bool) -> Self {
        let host = Machine::new("host-pc", true, false);
        let viewer = Machine::new("viewer-pc", true, false);
        Machine::pair(&host, &viewer, ALL);
        // The viewer machine must have granted CONTROL to the host machine too, or a swap cannot make it the host.
        viewer
            .trust
            .update(|s| s.devices.iter_mut().for_each(|d| d.grants_here = PermissionSet::CONTROL))
            .expect("store");
        let (host_hooks, viewer_hooks) = (Scripted::new(host_accepts), Scripted::new(true));
        lock(&host_hooks.addrs).insert(viewer.device(), viewer.addr());
        lock(&viewer_hooks.addrs).insert(host.device(), host.addr());
        assert!(host.core.set_extras(host_hooks.clone()));
        assert!(viewer.core.set_extras(viewer_hooks.clone()));
        assert!(!host.core.set_extras(Scripted::new(true)), "the hooks are set once");
        Self { host, viewer, host_hooks, viewer_hooks }
    }
}

/// The session a machine's hooks were handed for a swap.
fn opened_for(hooks: &Scripted, device: DeviceId) -> (ViewerHandle, Seen) {
    wait_for("the new session of the swap", || {
        let mut v = lock(&hooks.viewing);
        let i = v.iter().position(|(d, _)| *d == device)?;
        Some(v.remove(i).1)
    })
    .map(|(handle, msgs)| (handle, Seen::new(msgs)))
    .expect("the swap connection opens")
}

#[test]
fn swap_both_ways_over_two_cores() {
    let p = Pair::new(true);
    let (handle, _seen) =
        p.viewer.core.connect(p.host.addr(), p.host.device(), Intent::Control).expect("the session opens");
    let first = handle.session_id();
    assert!(!p.viewer.core.swap_pending(first));

    // The viewer asks: the host's interface is asked as the one that will view; it says yes; the host machine
    // opens the reverse session to the viewer machine with the token the viewer machine armed.
    let outcome = p.viewer.core.runtime().block_on(p.viewer.core.request_swap(first)).expect("a known session");
    assert_eq!(outcome, SwapOutcome::Swapped);
    {
        let asked = lock(&p.host_hooks.asked);
        assert_eq!((asked.len(), asked[0].device, asked[0].asked_becomes_host), (1, p.viewer.device(), false));
    }
    let (second, mut seen) = opened_for(&p.host_hooks, p.viewer.device());
    assert_ne!(second.session_id(), first, "a new session");
    assert!(second.granted().contains(PermissionSet::CONTROL), "with the grants the viewer machine stored for it");
    assert_eq!(handle.wait_ended(WAIT).expect("the old session ends").code, ErrorCode::Swapped);
    wait_for("the old session to leave the host", || p.host.core.host().sessions().is_empty().then_some(()));
    seen.next("Active", |o| {
        matches!(o, ViewerOut::State(c) if matches!(c.state, SessionState::Active { .. })).then_some(())
    });

    // The other way: the host machine, viewing now, asks; the viewer machine (hosting) accepts; it views again.
    let outcome =
        p.host.core.runtime().block_on(p.host.core.request_swap(second.session_id())).expect("a known session");
    assert_eq!(outcome, SwapOutcome::Swapped);
    let (third, _seen) = opened_for(&p.viewer_hooks, p.host.device());
    assert!(third.ctx().is_active());
    assert_eq!(second.wait_ended(WAIT).expect("the second session ends").code, ErrorCode::Swapped);
    // The asked side's cards were closed as answered.
    assert!(lock(&p.host_hooks.closed).iter().all(|(_, how)| *how == AskEnd::Answered));

    // A session nobody has is not swappable.
    assert_eq!(p.host.core.runtime().block_on(p.host.core.request_swap(1)), Err(ApplyError::NoSuchSession));
    drop((handle, second, third));
    p.host.core.shutdown();
    p.viewer.core.shutdown();
}

#[test]
fn a_swap_without_an_interface_is_declined_and_the_session_stays() {
    let host = Machine::new("host-pc", true, false);
    let viewer = Machine::new("viewer-pc", true, false);
    Machine::pair(&host, &viewer, ALL);
    viewer.trust.update(|s| s.devices.iter_mut().for_each(|d| d.grants_here = PermissionSet::CONTROL)).expect("store");
    // No hooks on the host: nobody to ask, so the question is declined.
    let (handle, _seen) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    let outcome =
        viewer.core.runtime().block_on(viewer.core.request_swap(handle.session_id())).expect("a known session");
    assert_eq!(outcome, SwapOutcome::Declined(ErrorCode::Declined));
    assert!(handle.ctx().is_active());
    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

#[test]
fn chat_both_ways_over_two_cores_and_gone_with_the_session() {
    let p = Pair::new(true);
    let (handle, _seen) =
        p.viewer.core.connect(p.host.addr(), p.host.device(), Intent::Control).expect("the session opens");
    let s = handle.session_id();
    let rt = p.viewer.core.runtime();

    let sent = rt.block_on(p.viewer.core.chat_send(s, "oi")).expect("sent");
    assert_eq!(sent.msg_id, 1);
    let (id, note) = wait_for("the host to hear it", || lock(&p.host_hooks.chat).first().cloned());
    assert_eq!(id, s);
    let ChatNote::Received(entry) = note else { panic!("not a message") };
    assert_eq!((entry.text.as_str(), entry.mine, entry.read), ("oi", false, false));
    // The host reads it: the viewer's copy shows it read, and the interface hears it.
    p.host.core.runtime().block_on(p.host.core.chat_mark_read(s)).expect("a known session");
    wait_for("the read state", || {
        lock(&p.viewer_hooks.chat).iter().any(|(_, n)| *n == ChatNote::ReadByPeer { up_to: 1 }).then_some(())
    });
    assert!(p.viewer.core.chat_entries(s)[0].read);
    // And back.
    p.host.core.runtime().block_on(p.host.core.chat_send(s, "olá")).expect("sent");
    wait_for("the viewer to hear it", || {
        lock(&p.viewer_hooks.chat)
            .iter()
            .any(|(_, n)| matches!(n, ChatNote::Received(e) if e.text == "olá"))
            .then_some(())
    });
    assert_eq!((p.viewer.core.chat_entries(s).len(), p.host.core.chat_entries(s).len()), (2, 2));

    // Refusals.
    assert_eq!(rt.block_on(p.viewer.core.chat_send(s, "  ")), Err(ChatSendError::Chat(ChatError::Empty)));
    assert_eq!(
        rt.block_on(p.viewer.core.chat_send(s, &"x".repeat(5000))),
        Err(ChatSendError::Chat(ChatError::TooLong))
    );
    assert_eq!(rt.block_on(p.viewer.core.chat_send(s + 1, "x")), Err(ChatSendError::NoSuchSession));

    // The messages vanish when the session ends.
    handle.disconnect();
    handle.wait_ended(WAIT).expect("ends");
    wait_for("the chat to vanish", || {
        (p.viewer.core.chat_entries(s).is_empty() && p.host.core.chat_entries(s).is_empty()).then_some(())
    });
    drop(handle);
    p.host.core.shutdown();
    p.viewer.core.shutdown();
}
