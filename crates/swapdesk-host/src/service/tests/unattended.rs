//! F-UNAT-01 on the service side, with the host's two helpers behind spies: Ctrl+Alt+Del for a viewer's `SendSpecial`
//! (only an elevated, unattended session; never while another account is at the console), and where the input of a
//! session goes on a desktop that is not the user's own.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use swapdesk_proto::{Desktop, Intent, PermissionSet, Special};
use swapdesk_trust::UserRef;

use super::*;

/// The host stores `UNATTENDED` for the viewer's device; the rig's `sas` counts what the service would send.
struct Unattended {
    rig: Rig,
    sas: Arc<AtomicUsize>,
}

impl Unattended {
    fn new() -> Self {
        let rig = Rig::new();
        rig.host
            .trust
            .update(|s| {
                for d in &mut s.devices {
                    d.grants_here = ALL | PermissionSet::UNATTENDED;
                    d.unattended_granted_by = Some(UserRef { id: "S-1-5-21-1-2-3-1001".into(), name: "admin".into() });
                }
            })
            .expect("store");
        let sas = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&sas);
        rig.host.core.set_sas_sender(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        Self { rig, sas }
    }

    /// An entry without a prompt, as the hub makes it toward a host of minor 1.
    fn enter(&self) -> (ViewerHandle, Seen) {
        let (handle, msgs) = self
            .rig
            .viewer
            .core
            .connect_asking(self.rig.host.addr(), self.rig.host.device(), Intent::Control, true, PermissionSet::EMPTY)
            .expect("the unattended session opens");
        assert_eq!(handle.mode(), swapdesk_proto::GrantMode::Unattended);
        (handle, Seen::new(msgs))
    }

    fn elevate(&self, handle: &ViewerHandle) {
        assert_eq!(self.rig.viewer.core.runtime().block_on(handle.elevate()), Ok(()));
        assert!(handle.granted().contains(PermissionSet::UNATTENDED), "{:?}", handle.granted());
    }

    fn special(&self, handle: &ViewerHandle) {
        handle
            .ctx()
            .try_send(swapdesk_proto::SendSpecial { key: Special::SecureAttention })
            .expect("the message is queued");
    }
}

#[test]
fn ctrl_alt_del_is_sent_only_for_an_elevated_unattended_session() {
    let t = Unattended::new();
    let (handle, _seen) = t.enter();
    // Entered without a prompt and not elevated: the session holds no UNATTENDED, so the host refuses.
    t.special(&handle);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(t.sas.load(Ordering::SeqCst), 0, "no UNATTENDED, no secure attention sequence");
    // Elevated: it goes, once.
    t.elevate(&handle);
    t.special(&handle);
    wait_for("the secure attention sequence", || (t.sas.load(Ordering::SeqCst) == 1).then_some(()));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(t.sas.load(Ordering::SeqCst), 1, "one request, one sequence");
    drop(handle);
    t.rig.finish();
}

#[test]
fn a_prompted_session_never_gets_ctrl_alt_del() {
    let t = Unattended::new();
    let (handle, _seen) = t.rig.connect();
    assert!(!handle.granted().contains(PermissionSet::UNATTENDED));
    t.special(&handle);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(t.sas.load(Ordering::SeqCst), 0);
    drop(handle);
    t.rig.finish();
}

#[test]
fn ctrl_alt_del_is_refused_while_another_account_is_at_the_console() {
    let t = Unattended::new();
    let (handle, _seen) = t.enter();
    t.elevate(&handle);
    t.rig.host.core.host().set_desktop(Desktop::AccountAway);
    wait_for("the session to be away", || (handle.ctx().desktop_state().desktop == Desktop::AccountAway).then_some(()));
    t.special(&handle);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(t.sas.load(Ordering::SeqCst), 0, "the sequence would land on another account's session");
    // Back at the console of its own account: it goes.
    t.rig.host.core.host().set_desktop(Desktop::Default);
    wait_for("the session to be back", || (handle.ctx().desktop_state().desktop == Desktop::Default).then_some(()));
    t.special(&handle);
    wait_for("the secure attention sequence", || (t.sas.load(Ordering::SeqCst) == 1).then_some(()));
    drop(handle);
    t.rig.finish();
}

#[test]
fn a_core_without_a_sender_answers_unavailable_and_sends_nothing() {
    let rig = Rig::new();
    rig.host
        .trust
        .update(|s| {
            for d in &mut s.devices {
                d.grants_here = ALL | PermissionSet::UNATTENDED;
                d.unattended_granted_by = Some(UserRef { id: "S-1-5-21-1-2-3-1001".into(), name: "admin".into() });
            }
        })
        .expect("store");
    let (handle, _msgs) = rig
        .viewer
        .core
        .connect_asking(rig.host.addr(), rig.host.device(), Intent::Control, true, PermissionSet::EMPTY)
        .expect("the unattended session opens");
    assert_eq!(rig.viewer.core.runtime().block_on(handle.elevate()), Ok(()));
    handle.ctx().try_send(swapdesk_proto::SendSpecial { key: Special::SecureAttention }).expect("queued");
    // Nothing to assert on the wire here without a sender to count: the relay logs the refusal.
    wait_for("the refusal in the log", || {
        rig.host
            .trace
            .lines()
            .iter()
            .any(|l| l.text.contains("no sender for the secure attention sequence"))
            .then_some(())
    });
    drop(handle);
    rig.finish();
}

#[test]
fn input_on_a_protected_desktop_goes_to_the_system_helper_for_an_elevated_session_and_nowhere_otherwise() {
    let t = Unattended::new();
    let (handle, _seen) = t.enter();
    t.elevate(&handle);
    // The host reports a protected desktop (the media helper's `DesktopChanged` reaches the sessions this way).
    t.rig.host.core.host().set_desktop(Desktop::Secure);
    wait_for("the session to follow", || (handle.ctx().desktop_state().desktop == Desktop::Secure).then_some(()));
    assert!(!handle.ctx().is_paused(), "an UNATTENDED session is not paused on a protected desktop");
    let typed = vec![swapdesk_proto::InputEvent::Text("pin".into())];
    handle.send_input(typed.clone()).expect("accepted");
    wait_for("the input to reach the system helper", || (t.rig.system.sink.events() == typed).then_some(()));
    assert!(t.rig.user.sink.events().is_empty(), "the user helper never gets a record for a protected desktop");

    // A prompted session on the same host (a device holds one session at a time: the first one ends) is paused on
    // that desktop and its input goes nowhere.
    drop(handle);
    std::thread::sleep(Duration::from_millis(300));
    let (prompted, _seen) = t.rig.connect();
    t.rig.host.core.host().set_desktop(Desktop::Secure);
    wait_for("the prompted session to be paused", || {
        let c = prompted.ctx();
        (c.desktop_state().desktop == Desktop::Secure && c.is_paused()).then_some(())
    });
    let before = (t.rig.system.sink.events().len(), t.rig.user.sink.events().len());
    let _ = prompted.send_input(vec![swapdesk_proto::InputEvent::Text("nope".into())]);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!((t.rig.system.sink.events().len(), t.rig.user.sink.events().len()), before);
    drop(prompted);
    t.rig.finish();
}
