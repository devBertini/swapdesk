//! The host side of a lost link (docs/WAITING.md row 22): the core keeps a lost session resumable for its window, so
//! the host's UI shows it as reconnecting until it resumes or the window ends, never as ended at once.

use swapdesk_proto::ErrorCode;
use swapdesk_session::{EndKind, HostEvent, SessionEnd};

use super::*;

const SESSION: u64 = 41;

fn lost() -> SessionEnd {
    SessionEnd { kind: EndKind::Lost, code: ErrorCode::NoError }
}

fn started(other: &Machine) -> HostEvent {
    HostEvent::SessionStarted {
        device: other.id(),
        session_id: SESSION,
        mode: swapdesk_proto::GrantMode::Prompted,
        granted: ALL,
    }
}

fn ended_event(other: &Machine, end: SessionEnd, resumable: bool) -> HostEvent {
    HostEvent::Ended { device: other.id(), session_id: SESSION, end, resumable }
}

fn is_ended(m: &Ipc) -> bool {
    matches!(m, Ipc::UiNotify(Notify::Session { session_id: SESSION, state: SessionState::Ended { .. }, .. }))
}

fn reconnecting(ui: &mut Client) {
    ui.notify("the hosted session reconnecting", |n| match n {
        Notify::Session { session_id: SESSION, state: SessionState::Reconnecting { .. }, .. } => Some(()),
        _ => None,
    });
}

/// A machine with a console UI that hosts the session `SESSION` for `other`.
fn hosting() -> (Machine, Machine, Client) {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, ALL);
    let mut ui = m.client(info(false));
    m.hub.host_ui().event(started(&other));
    active_as_host(&mut ui);
    (m, other, ui)
}

fn active_as_host(ui: &mut Client) {
    ui.notify("the hosted session active", |n| match n {
        Notify::Session { session_id: SESSION, state, .. } => {
            grants_active(state).is_some_and(|(r, _)| r == Role::Host).then_some(())
        }
        _ => None,
    });
}

#[test]
fn a_resumable_end_shows_the_hosted_session_as_reconnecting_not_ended() {
    let (m, other, mut ui) = hosting();
    m.hub.host_ui().event(HostEvent::PeerOffline { device: other.id(), session_id: SESSION });
    reconnecting(&mut ui);
    // The core's connection is judged lost: the token lives for the window, so the session is still the host's.
    m.hub.host_ui().event(ended_event(&other, lost(), true));
    reconnecting(&mut ui);
    assert!(ui.nothing_within(Duration::from_millis(400), is_ended), "a resumable end is not an end for the host's UI");
    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_resumed_session_is_active_again_and_never_ended() {
    let (m, other, mut ui) = hosting();
    m.hub.host_ui().event(ended_event(&other, lost(), true));
    reconnecting(&mut ui);
    // The viewer is back: the core starts the same session again.
    m.hub.host_ui().event(started(&other));
    active_as_host(&mut ui);
    assert!(ui.nothing_within(Duration::from_millis(400), is_ended));
    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_resumable_end_becomes_ended_when_the_window_ends() {
    let (m, other, mut ui) = hosting();
    m.hub.set_resume_window(Duration::from_millis(300));
    m.hub.host_ui().event(ended_event(&other, lost(), true));
    reconnecting(&mut ui);
    let reason = ui.notify("the end after the window", |n| match n {
        Notify::Session { session_id: SESSION, state: SessionState::Ended { reason }, .. } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, Some(ApiCode::Timeout), "a lost link that never came back reads as a silence");
    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_session_resumed_then_lost_again_gets_a_whole_new_window() {
    let (m, other, mut ui) = hosting();
    m.hub.set_resume_window(Duration::from_millis(600));
    m.hub.host_ui().event(ended_event(&other, lost(), true));
    reconnecting(&mut ui);
    std::thread::sleep(Duration::from_millis(400));
    m.hub.host_ui().event(started(&other));
    active_as_host(&mut ui);
    m.hub.host_ui().event(ended_event(&other, lost(), true));
    reconnecting(&mut ui);
    // The first loss's window (600 ms from its start) is over by now; the second one's is not.
    assert!(ui.nothing_within(Duration::from_millis(400), is_ended), "the old window must not end the new loss");
    ui.notify("the end after the second window", |n| {
        matches!(n, Notify::Session { session_id: SESSION, state: SessionState::Ended { .. }, .. }).then_some(())
    });
    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_final_end_is_ended_at_once() {
    let (m, other, mut ui) = hosting();
    let bye = SessionEnd { kind: EndKind::PeerBye, code: ErrorCode::NoError };
    m.hub.host_ui().event(ended_event(&other, bye, false));
    let reason = ui.notify("the end", |n| match n {
        Notify::Session { session_id: SESSION, state: SessionState::Ended { reason }, .. } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, None);
    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn the_hosts_user_disconnecting_a_reconnecting_session_ends_it_for_good() {
    let (m, other, mut ui) = hosting();
    m.hub.host_ui().event(ended_event(&other, lost(), true));
    reconnecting(&mut ui);
    assert!(ui.call(UiOp::Disconnect { session_id: SESSION }).is_ok(), "the banner's button works while it reconnects");
    let reason = ui.notify("the end", |n| match n {
        Notify::Session { session_id: SESSION, state: SessionState::Ended { reason }, .. } => Some(*reason),
        _ => None,
    });
    assert_eq!(reason, None, "the user's own end");
    // The window's timer finds nothing left to end.
    m.hub.set_resume_window(Duration::from_millis(1));
    assert!(ui.nothing_within(Duration::from_millis(300), is_ended));
    drop(ui);
    m.finish();
    other.finish();
}
