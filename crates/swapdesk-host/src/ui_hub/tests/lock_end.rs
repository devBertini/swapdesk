//! "Bloquear ao terminar" (F-SESS-03) through the hub: the hook is asked to lock the console session the account is
//! signed in at, when a hosted session ends with the setting on; with it off, with another account at the console, or
//! with nobody there, it is not asked. (The session layer's own rules, once per end and never for a lost link it keeps
//! resumable, are proved in `swapdesk-session` `resume.rs`.)

use swapdesk_fakes::{FakeActions, FakeClipboard};
use swapdesk_ipc::SettingsUpdate;

use super::clip_actions::open;
use super::*;

const ME: &str = "S-1-5-21-1";
const OTHER: &str = "S-1-5-21-2";

fn locks(m: &Machine) -> Vec<u32> {
    lock(&m.hooks.locks).clone()
}

fn set_lock_on_end(ui: &mut Client, on: bool) {
    let update = SettingsUpdate { name: "sala".into(), service_enabled: true, lock_on_end: on, keep_awake: true };
    assert!(ui.call(UiOp::SetSettings(update)).is_ok());
}

#[test]
fn the_hook_is_asked_for_the_console_session_the_account_is_at() {
    let m = Machine::new("sala");
    m.hub.set_console_session(Some(7));
    m.hub.host_ui().lock_screen();
    assert_eq!(locks(&m), vec![7], "a machine of one account locks its console");

    // Several accounts: the one at the console has a screen to lock.
    m.hub.set_account(ME);
    m.hub.set_console_user(Some(ME.into()));
    m.hub.host_ui().lock_screen();
    assert_eq!(locks(&m), vec![7, 7]);
    m.finish();
}

#[test]
fn nothing_is_locked_when_another_account_or_nobody_is_at_the_console() {
    let m = Machine::new("sala");
    m.hub.set_account(ME);
    m.hub.set_console_session(Some(7));
    m.hub.set_console_user(Some(OTHER.into()));
    m.hub.host_ui().lock_screen();
    m.hub.set_console_user(None);
    m.hub.host_ui().lock_screen();
    m.hub.set_console_user(Some(ME.into()));
    m.hub.set_console_session(None);
    m.hub.host_ui().lock_screen();
    assert!(locks(&m).is_empty(), "the screen of another account is not this one's to lock");
    m.finish();
}

#[test]
fn a_hosted_session_that_ends_locks_the_console_with_the_setting_on_and_not_with_it_off() {
    let (actions, clip) = (FakeActions::new(), FakeClipboard::new());
    // Off (the default): the session ends and nothing is locked.
    let mut p = open(&actions, &clip);
    assert!(!p.host.hub.settings().lock_on_end);
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: p.session }).is_ok());
    wait_for("the host's session to end", || p.host.core.host().sessions().is_empty().then_some(()));
    std::thread::sleep(Duration::from_millis(300));
    assert!(locks(&p.host).is_empty(), "the setting is off");
    drop((p.host_ui, p.viewer_ui));
    p.viewer.finish();
    p.host.finish();

    // On: the same end locks the console session (1 in this fixture), once.
    let mut p = open(&actions, &clip);
    set_lock_on_end(&mut p.host_ui, true);
    assert!(p.host.hub.settings().lock_on_end);
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: p.session }).is_ok());
    wait_for("the lock", || (!locks(&p.host).is_empty()).then_some(()));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(locks(&p.host), vec![1]);
    drop((p.host_ui, p.viewer_ui));
    p.viewer.finish();
    p.host.finish();
}
