//! The host banner's files line (FILES-UI.md 6): transfers, a change for 10 s, the other-disks note once, no names.

use crate::api::*;
use crate::format::Clock;
use crate::mock::Scenario;
use crate::state::UiState;

const HOSTED: u64 = 2;

fn hosted() -> UiState {
    let mut s = Scenario::Banner.prepared_state();
    s.set_clock(Clock { now_ms: 1_000_000, utc_offset_min: 0 });
    s
}

fn at(s: &mut UiState, ms: u64) {
    s.set_clock(Clock { now_ms: ms, utc_offset_min: 0 });
}

fn transfer(s: &mut UiState, id: u32, outgoing: bool, state: TransferState) {
    s.apply(UiEvent::Transfer { transfer_id: id, session_id: HOSTED, name: "secret-name.pdf".into(), outgoing, state });
}

fn running() -> TransferState {
    TransferState::Running { done: 1, total: 10, bytes_per_s: 1 }
}

fn line(s: &UiState) -> Option<String> {
    s.banner_files_line()
}

#[test]
fn no_files_no_line() {
    assert_eq!(line(&hosted()), None);
}

#[test]
fn the_line_counts_files_sent_to_the_viewer_while_they_run_and_never_names_one() {
    let mut s = hosted();
    for id in 1..=5 {
        transfer(&mut s, id, true, running());
    }
    transfer(&mut s, 1, true, TransferState::Done);
    transfer(&mut s, 2, true, TransferState::Done);
    assert_eq!(line(&s).as_deref(), Some("Copiando arquivos: 2 de 5"));
    assert!(!s.banner_text("PC", s.host_session().unwrap()).contains("secret"));
    assert!(s.controls(crate::WindowKind::Banner).iter().all(|c| !c.label.contains("secret")));
    for id in 3..=5 {
        transfer(&mut s, id, true, TransferState::Done);
    }
    assert_eq!(line(&s), None, "when the last file ends the line goes");
}

#[test]
fn files_arriving_here_say_recebendo_and_a_new_run_counts_from_zero() {
    let mut s = hosted();
    transfer(&mut s, 7, false, running());
    transfer(&mut s, 8, false, running());
    transfer(&mut s, 7, false, TransferState::Done);
    assert_eq!(line(&s).as_deref(), Some("Recebendo arquivos: 1 de 2"));
    transfer(&mut s, 8, false, TransferState::Failed { reason: ApiCode::Timeout });
    assert_eq!(line(&s), None);
    transfer(&mut s, 9, false, running());
    assert_eq!(line(&s).as_deref(), Some("Recebendo arquivos: 0 de 1"));
}

#[test]
fn a_transfer_of_another_session_never_reaches_the_banner() {
    let mut s = hosted();
    s.apply(UiEvent::Transfer { transfer_id: 1, session_id: 99, name: "x".into(), outgoing: true, state: running() });
    assert_eq!(line(&s), None);
}

#[test]
fn a_change_shows_for_ten_seconds_and_then_is_gone_and_the_timer_is_due_when_it_ends() {
    let mut s = hosted();
    let t0 = 1_000_000;
    let cases = [
        (1u8, 1u32, "Renomeou 1 item"),
        (2, 3, "Moveu 3 itens"),
        (3, 4, "Mandou 4 itens para a Lixeira"),
        (0, 1, "Criou uma pasta"),
    ];
    for (n, (op, count, text)) in cases.into_iter().enumerate() {
        let start = t0 + n as u64 * 100_000;
        at(&mut s, start);
        s.apply(UiEvent::HostFilesChanged { session_id: HOSTED, op, count });
        assert_eq!(line(&s).as_deref(), Some(text));
        assert_eq!(s.banner_files_expiry_ms(start), Some(10_000));
        at(&mut s, start + 9_999);
        assert_eq!(line(&s).as_deref(), Some(text), "still there at 9.999 s");
        at(&mut s, start + 10_000);
        assert_eq!(line(&s), None, "gone at 10 s");
    }
}

#[test]
fn the_banner_timer_wakes_for_the_end_of_the_note_before_the_next_minute() {
    let mut s = hosted();
    let since = match s.host_session().unwrap().state {
        SessionState::Active { since_ms, .. } => since_ms,
        _ => unreachable!(),
    };
    let now = since + 5_000;
    at(&mut s, now);
    assert_eq!(s.banner_minute_delay_ms(now), Some(55_000));
    s.apply(UiEvent::HostFilesChanged { session_id: HOSTED, op: 1, count: 1 });
    assert_eq!(s.banner_minute_delay_ms(now), Some(10_000));
    assert_eq!(s.banner_minute_delay_ms(now + 4_000), Some(6_000));
    at(&mut s, now + 10_000);
    assert_eq!(s.banner_minute_delay_ms(now + 10_000), Some(45_000));
}

#[test]
fn changes_of_one_kind_inside_the_window_add_up() {
    let mut s = hosted();
    s.apply(UiEvent::HostFilesChanged { session_id: HOSTED, op: 3, count: 1 });
    at(&mut s, 1_005_000);
    s.apply(UiEvent::HostFilesChanged { session_id: HOSTED, op: 3, count: 2 });
    assert_eq!(line(&s).as_deref(), Some("Mandou 3 itens para a Lixeira"));
    // A change that did nothing (count 0) says nothing.
    at(&mut s, 2_000_000);
    s.apply(UiEvent::HostFilesChanged { session_id: HOSTED, op: 2, count: 0 });
    assert_eq!(line(&s), None);
}

#[test]
fn the_other_disks_note_shows_once_per_session_for_ten_seconds() {
    let mut s = hosted();
    let t0 = 1_000_000;
    s.apply(UiEvent::HostFilesReach { session_id: HOSTED });
    assert_eq!(line(&s).as_deref(), Some("Abrindo arquivos de outros discos"));
    at(&mut s, t0 + 10_000);
    assert_eq!(line(&s), None);
    // A second reach (another kind of volume) says nothing: once per session.
    s.apply(UiEvent::HostFilesReach { session_id: HOSTED });
    assert_eq!(line(&s), None);
    // The next hosted session says it again.
    s.apply(UiEvent::Session {
        session_id: HOSTED,
        device_id: crate::mock::DESKTOP.into(),
        state: SessionState::Ended { reason: None },
    });
    s.apply(UiEvent::Session {
        session_id: 3,
        device_id: crate::mock::DESKTOP.into(),
        state: SessionState::Active {
            since_ms: t0,
            role: Role::Host,
            granted: Permissions::NONE.with(Permission::View),
            elevation: Default::default(),
        },
    });
    s.apply(UiEvent::HostFilesReach { session_id: 3 });
    assert_eq!(line(&s).as_deref(), Some("Abrindo arquivos de outros discos"));
}

#[test]
fn a_transfer_outranks_a_change_and_the_change_comes_back_after_it() {
    let mut s = hosted();
    s.apply(UiEvent::HostFilesChanged { session_id: HOSTED, op: 1, count: 1 });
    transfer(&mut s, 1, true, running());
    assert_eq!(line(&s).as_deref(), Some("Copiando arquivos: 0 de 1"));
    transfer(&mut s, 1, true, TransferState::Done);
    assert_eq!(line(&s).as_deref(), Some("Renomeou 1 item"));
}

#[test]
fn a_viewer_sessions_files_events_do_not_touch_the_banner() {
    let mut s = hosted();
    s.apply(UiEvent::HostFilesChanged { session_id: 1, op: 1, count: 1 });
    s.apply(UiEvent::HostFilesReach { session_id: 1 });
    assert_eq!(line(&s), None);
}
