//! Unit tests of `UiState`: every event and every activation that changes what a screen shows.

use crate::api::*;
use crate::interact::{ControlId, UiRole};
use crate::mock::{AIR, DESKTOP, ENTRY, MAC, Scenario};
use crate::state::*;
use crate::strings::MsgKey;

fn base() -> UiState {
    Scenario::Dispositivos.prepared_state()
}

fn viewer() -> UiState {
    Scenario::Visualizador.prepared_state()
}

fn reply_ok(r: Reply) -> UiEvent {
    UiEvent::Reply { ticket: Ticket(0), result: Ok(r) }
}

fn tracked_error(s: &mut UiState, command: &Command, code: ApiCode) {
    let t = Ticket(5000);
    s.track(t, command);
    s.apply(UiEvent::Reply { ticket: t, result: Err(ApiError::new(code)) });
}

#[test]
fn permissions_keep_their_implications() {
    let p = Permissions::NONE.with(Permission::Control);
    assert!(p.has(Permission::View) && p.has(Permission::Control));
    let p = p.without(Permission::View);
    assert!(!p.has(Permission::Control), "dropping View drops what needs it");
    let p = Permissions::NONE.with(Permission::AdminTerminal);
    assert!(p.has(Permission::Terminal));
    assert!(!p.without(Permission::Terminal).has(Permission::AdminTerminal));
    assert!(
        Permission::Unattended.needs_admin()
            && Permission::AdminTerminal.needs_admin()
            && !Permission::Files.needs_admin()
    );
    assert_eq!(Permissions::NONE.with(Permission::Unattended).0, 0b1000_0001);
}

#[test]
fn devices_events_fill_the_list_and_keep_a_valid_selection() {
    let mut s = UiState::default();
    assert_eq!(s.selected_device(), None);
    s.apply(reply_ok(Reply::Devices(crate::mock::Scenario::Dispositivos.prepared_state().devices().to_vec())));
    assert_eq!(s.selected_device().unwrap().id, MAC, "the first paired device is selected");
    let mut only_air = s.devices().to_vec();
    only_air.retain(|d| d.id == AIR);
    s.apply(UiEvent::DevicesChanged(only_air));
    assert_eq!(s.selected_device().unwrap().id, AIR, "a vanished selection falls back");
    s.apply(reply_ok(Reply::Device(DeviceView { name: "Novo".into(), id: "novo".into(), ..s.devices()[0].clone() })));
    assert_eq!(s.devices().len(), 2);
}

#[test]
fn selecting_and_navigating_change_the_screen() {
    let mut s = base();
    assert_eq!(s.screen(), Screen::Devices);
    s.activate(ControlId::NavSettings);
    assert_eq!(s.screen(), Screen::Settings);
    s.activate(ControlId::NavHome);
    assert_eq!(s.screen(), Screen::Home);
    s.activate(ControlId::HomeActivityAll);
    assert_eq!(s.screen(), Screen::Activity);
    let i = s.devices().iter().position(|d| d.id == DESKTOP).unwrap() as u16;
    s.activate(ControlId::Device(i));
    assert_eq!(s.screen(), Screen::Devices);
    assert_eq!(s.selected_device().unwrap().id, DESKTOP);
    s.activate(ControlId::Tab(DeviceTab::History));
    assert_eq!(s.tab(), DeviceTab::History);
    s.activate(ControlId::Tab(DeviceTab::Overview));
    s.activate(ControlId::EditGrants);
    assert_eq!(s.tab(), DeviceTab::Permissions, "Editar leads to the permissions tab");
    // Choosing another device goes back to its overview.
    s.activate(ControlId::Device(0));
    assert_eq!(s.tab(), DeviceTab::Overview);
}

#[test]
fn connect_buttons_send_the_right_intent() {
    let mut s = base();
    let a = s.activate(ControlId::ViewOnly);
    assert_eq!(a.commands, vec![Command::Connect { device_id: MAC.into(), intent: Intent::View, unattended: false }]);
    // The card owns the page now: its buttons are gone until it closes.
    s.track(Ticket(8), &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: Ticket(8), result: Ok(Reply::SessionId(ENTRY)) });
    s.activate(ControlId::ConnectCancel);
    s.apply(session_event(ENTRY, MAC, SessionState::Ended { reason: None }));
    let a = s.activate(ControlId::Control);
    assert_eq!(
        a.commands,
        vec![Command::Connect { device_id: MAC.into(), intent: Intent::Control, unattended: false }]
    );
    // The SessionId reply names the entry, which is not a session: no viewer window yet.
    let t = Ticket(9);
    s.track(t, &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: t, result: Ok(Reply::SessionId(ENTRY)) });
    assert_eq!(s.windows(), vec![WindowKind::Main]);
    assert!(s.viewer_session().is_none());
    assert_eq!(s.sessions().len(), 1);
    assert_eq!(s.sessions()[0].id, ENTRY);
}

#[test]
fn an_unpaired_device_offers_pairing_not_connecting() {
    let mut s = base();
    s.select_device(AIR);
    let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::PairWithDevice));
    assert!(!ids.contains(&ControlId::Control));
    let a = s.activate(ControlId::PairWithDevice);
    assert_eq!(a.commands, vec![Command::PairOpen { device_id: Some(AIR.into()) }]);
    assert_eq!(s.screen(), Screen::Pairing);
    // The pairing id starts the initiator against that device (found by the end-to-end UI test: nothing sent it).
    assert!(s.take_followups().is_empty());
    s.apply(reply_ok(Reply::PairingId(7)));
    assert_eq!(s.take_followups(), vec![Command::PairStart { pairing_id: 7, device_id: AIR.into() }]);
    assert!(s.take_followups().is_empty(), "sent once");
}

#[test]
fn grants_toggle_with_their_implications_and_forget_waits_for_done() {
    let mut s = base();
    s.activate(ControlId::Tab(DeviceTab::Permissions));
    let a = s.activate(ControlId::Grant(Permission::View));
    let d = s.device_by_id(MAC).unwrap();
    assert!(
        !d.grants_here.has(Permission::View) && !d.grants_here.has(Permission::Control),
        "View off takes Control with it"
    );
    assert!(
        matches!(&a.commands[0], Command::SetGrants { device_id, grants } if device_id == MAC && *grants == d.grants_here)
    );
    let a = s.activate(ControlId::Forget);
    assert_eq!(a.commands, vec![Command::ForgetDevice { device_id: MAC.into() }]);
    let t = Ticket(3);
    s.track(t, &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: t, result: Ok(Reply::Done) });
    assert!(s.device_by_id(MAC).is_none());
    assert_eq!(s.tab(), DeviceTab::Overview);
}

#[test]
fn the_pairing_flow_from_open_to_done() {
    let mut s = base();
    let a = s.activate(ControlId::PairThisComputer);
    assert_eq!(a.commands, vec![Command::PairOpen { device_id: None }]);
    assert_eq!(s.pairing().unwrap().state, PairingState::Open { remaining_s: 120 });
    assert!(s.needs_tick());
    s.apply(reply_ok(Reply::PairingId(4)));
    assert_eq!(s.pairing().unwrap().id, 4);
    assert!(s.take_followups().is_empty(), "\"Parear este computador\" waits to be found: it starts nothing");
    s.apply(UiEvent::Pairing {
        pairing_id: 4,
        state: PairingState::ShowCode { code: "111 222".into(), peer_name: "MacBook Air".into() },
    });
    let a = s.activate(ControlId::PairSame);
    assert_eq!(a.commands, vec![Command::PairConfirm { pairing_id: 4, matches: true }]);
    s.apply(UiEvent::Pairing { pairing_id: 4, state: PairingState::Waiting });
    assert_eq!(s.pairing().unwrap().state, PairingState::Waiting);
    s.apply(UiEvent::Pairing { pairing_id: 4, state: PairingState::Done { device_id: AIR.into() } });
    s.activate(ControlId::PairDone);
    assert_eq!(s.screen(), Screen::Devices);
    assert_eq!(s.selected_device().unwrap().id, AIR);
}

#[test]
fn pairing_can_be_cancelled_refused_failed_and_retried() {
    let mut s = Scenario::Pareamento.prepared_state();
    let a = s.activate(ControlId::PairDifferent);
    assert!(matches!(a.commands[0], Command::PairConfirm { matches: false, .. }));
    assert_eq!(s.screen(), Screen::Devices);

    let mut s = Scenario::ParearEsteComputador.prepared_state();
    let a = s.activate(ControlId::PairCancel);
    assert_eq!(a.commands, vec![Command::PairCancel { pairing_id: 1 }]);
    assert_eq!(s.screen(), Screen::Home, "closing the pairing from Início returns there");

    let mut s = base();
    s.activate(ControlId::PairThisComputer);
    s.apply(UiEvent::Pairing { pairing_id: 1, state: PairingState::Failed { reason: ApiCode::Locked } });
    let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::PairRetry) && ids.contains(&ControlId::PairClose));
    let a = s.activate(ControlId::PairRetry);
    assert_eq!(a.commands, vec![Command::PairOpen { device_id: None }]);
    s.apply(UiEvent::Pairing { pairing_id: 1, state: PairingState::Failed { reason: ApiCode::Timeout } });
    s.activate(ControlId::PairClose);
    assert_eq!(s.screen(), Screen::Devices);
    // The service refusing to open pairing shows the failure screen.
    let mut s = base();
    let a = s.activate(ControlId::PairThisComputer);
    tracked_error(&mut s, &a.commands[0], ApiCode::Locked);
    assert!(matches!(s.pairing().unwrap().state, PairingState::Failed { reason: ApiCode::Locked }));
}

#[test]
fn the_incoming_dialog_answers_with_what_is_ticked_and_never_unattended() {
    let mut s = Scenario::PedidoConexao.prepared_state();
    assert!(s.incoming_session_open());
    s.activate(ControlId::Allow(Permission::Terminal));
    s.activate(ControlId::Allow(Permission::Audio));
    let a = s.activate(ControlId::Accept);
    let Command::AnswerIncoming { request_id: 7, decision: Decision::Accept { grants } } = &a.commands[0] else {
        panic!("{:?}", a.commands)
    };
    assert!(grants.has(Permission::Terminal) && !grants.has(Permission::Audio) && grants.has(Permission::Control));
    assert!(!grants.has(Permission::Unattended) && !grants.has(Permission::AdminTerminal), "D8");
    assert!(s.incoming().is_none());

    let mut s = Scenario::PedidoConexao.prepared_state();
    let a = s.activate(ControlId::ViewOnlyAccept);
    assert!(matches!(a.commands[0], Command::AnswerIncoming { decision: Decision::ViewOnly, .. }));
    let mut s = Scenario::PedidoConexao.prepared_state();
    let a = s.activate(ControlId::Decline);
    assert!(matches!(a.commands[0], Command::AnswerIncoming { decision: Decision::Decline, .. }));
}

#[test]
fn remembering_unattended_is_a_separate_admin_checked_grant() {
    let mut s = Scenario::PedidoConexao.prepared_state();
    // The design's mac already has Unattended; take it away to see the new grant.
    let mut devices = s.devices().to_vec();
    devices[0].grants_here = Permissions::NONE.with(Permission::Control);
    s.apply(UiEvent::DevicesChanged(devices));
    s.activate(ControlId::RememberUnattended);
    let a = s.activate(ControlId::Accept);
    assert_eq!(a.commands.len(), 2);
    let Command::SetGrants { grants, .. } = &a.commands[1] else { panic!() };
    assert!(grants.has(Permission::Unattended));
    tracked_error(&mut s, &a.commands[1], ApiCode::NeedsAdmin);
    assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::Elevation));
    // Declining never grants anything.
    let mut s = Scenario::PedidoConexao.prepared_state();
    s.activate(ControlId::RememberUnattended);
    assert_eq!(s.activate(ControlId::Decline).commands.len(), 1);
}

/// The owner's defect of 11/11: a Mac paired again (nothing stored for it), the request card accepted with control
/// and "entrar sem perguntar" ticked; the stored grant was the old store plus Unattended, so View alone, and every
/// later entry without a prompt could only watch. What is remembered is what this card permitted, plus Unattended.
#[test]
fn remembering_unattended_stores_what_the_card_permitted_not_the_old_store() {
    let mut s = Scenario::PedidoConexao.prepared_state();
    let mut devices = s.devices().to_vec();
    devices[0].grants_here = Permissions::NONE;
    s.apply(UiEvent::DevicesChanged(devices));
    s.activate(ControlId::Allow(Permission::Terminal));
    s.activate(ControlId::RememberUnattended);
    let a = s.activate(ControlId::Accept);
    let Command::AnswerIncoming { decision: Decision::Accept { grants: accepted }, .. } = &a.commands[0] else {
        panic!("{:?}", a.commands)
    };
    let Command::SetGrants { grants, .. } = &a.commands[1] else { panic!("{:?}", a.commands) };
    assert_eq!(*grants, accepted.with(Permission::Unattended), "the card's grants and Unattended");
    assert!(grants.has(Permission::Control));

    // "Só deixar ver" with the box ticked remembers watching without a prompt, nothing more.
    let mut s = Scenario::PedidoConexao.prepared_state();
    let mut devices = s.devices().to_vec();
    devices[0].grants_here = Permissions::NONE;
    s.apply(UiEvent::DevicesChanged(devices));
    s.activate(ControlId::RememberUnattended);
    let a = s.activate(ControlId::ViewOnlyAccept);
    let Command::SetGrants { grants, .. } = &a.commands[1] else { panic!("{:?}", a.commands) };
    assert_eq!(*grants, Permissions::NONE.with(Permission::Unattended));

    // What the store held besides (the administrator terminal, granted before) stays.
    let mut s = Scenario::PedidoConexao.prepared_state();
    let mut devices = s.devices().to_vec();
    devices[0].grants_here = Permissions::NONE.with(Permission::AdminTerminal);
    s.apply(UiEvent::DevicesChanged(devices));
    s.activate(ControlId::RememberUnattended);
    let a = s.activate(ControlId::Accept);
    let Command::SetGrants { grants, .. } = &a.commands[1] else { panic!("{:?}", a.commands) };
    assert!(
        grants.has(Permission::AdminTerminal) && grants.has(Permission::Control) && grants.has(Permission::Unattended)
    );
}

#[test]
fn incoming_requests_close_by_event_and_countdown() {
    let mut s = Scenario::PedidoConexao.prepared_state();
    assert_eq!(s.incoming().unwrap().remaining_s, 18);
    s.tick();
    assert_eq!(s.incoming().unwrap().remaining_s, 17);
    s.apply(UiEvent::IncomingClosed { request_id: 99, reason: ApiCode::Timeout });
    assert!(s.incoming().is_some(), "another request's close is ignored");
    s.apply(UiEvent::IncomingClosed { request_id: 7, reason: ApiCode::Timeout });
    assert!(s.incoming().is_none());
    assert!(!s.needs_tick());
}

#[test]
fn the_consent_card_answers_for_one_feature() {
    let mut s = Scenario::Consentimento.prepared_state();
    assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::Consent));
    let t = s.dialog_texts(&DialogKind::Consent);
    assert!(t.title.contains("terminal") && t.title.contains("MacBook do escritório"));
    let a = s.activate(ControlId::DialogPrimary);
    assert!(matches!(a.commands[0], Command::AnswerIncoming { request_id: 8, decision: Decision::Accept { .. } }));
    let mut s = Scenario::Consentimento.prepared_state();
    let a = s.activate(ControlId::DialogSecondary);
    assert!(matches!(a.commands[0], Command::AnswerIncoming { decision: Decision::Decline, .. }));
    assert_eq!(s.dialog(WindowKind::Main), None);
}

#[test]
fn the_elevation_step_runs_its_phases() {
    let mut s = Scenario::Elevacao.prepared_state();
    assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::Elevation));
    let a = s.activate(ControlId::DialogPrimary);
    let Command::Elevate { action, run, .. } = a.commands[0].clone() else { panic!() };
    assert_eq!(s.elevation().unwrap().phase, ElevPhase::Waiting);
    // The wrong action's result is ignored.
    s.apply(UiEvent::ElevationResult {
        action: ElevatedAction::OpenAdminTerminal { session_id: 1 },
        run,
        result: Ok(()),
    });
    assert!(s.elevation().is_some());
    s.apply(UiEvent::ElevationResult { action: action.clone(), run, result: Err(ApiError::new(ApiCode::Refused)) });
    assert_eq!(s.elevation().unwrap().phase, ElevPhase::Failed(ApiCode::Refused));
    let a = s.activate(ControlId::DialogPrimary);
    let Command::Elevate { run: retry, .. } = a.commands[0].clone() else { panic!("retry") };
    assert_ne!(retry, run, "a retry is a new run");
    // A late result of the first run does not decide the retry.
    s.apply(UiEvent::ElevationResult { action: action.clone(), run, result: Ok(()) });
    assert_eq!(s.elevation().unwrap().phase, ElevPhase::Waiting);
    s.apply(UiEvent::ElevationResult { action, run: retry, result: Ok(()) });
    assert!(s.elevation().is_none());

    let mut s = Scenario::Elevacao.prepared_state();
    s.activate(ControlId::DialogSecondary);
    assert!(s.elevation().is_none(), "Cancelar closes it");
}

#[test]
fn a_confirmed_elevation_resends_the_command_that_needed_it() {
    let mut s = Scenario::Elevacao.prepared_state();
    let a = s.activate(ControlId::DialogPrimary);
    let Command::Elevate { action, run, .. } = a.commands[0].clone() else { panic!() };
    let ElevatedAction::SetGrants { device_id, grants } = action.clone() else { panic!("{action:?}") };
    let _ = s.take_followups();
    s.apply(UiEvent::ElevationResult { action, run, result: Ok(()) });
    assert!(s.elevation().is_none(), "the step closes");
    assert_eq!(s.take_followups(), vec![Command::SetGrants { device_id, grants }]);
    assert!(s.take_followups().is_empty(), "sent once");

    // A failure resends nothing.
    let mut s = Scenario::Elevacao.prepared_state();
    let a = s.activate(ControlId::DialogPrimary);
    let Command::Elevate { action, run, .. } = a.commands[0].clone() else { panic!() };
    let _ = s.take_followups();
    s.apply(UiEvent::ElevationResult { action, run, result: Err(ApiError::new(ApiCode::Refused)) });
    assert!(s.take_followups().is_empty());
    // The other two actions resend theirs: a_confirmed_elevate_session_sends_elevate_session_again and
    // a_confirmed_admin_terminal_opens_the_administrator_terminal.
}

#[test]
fn a_failed_elevate_reply_shows_in_the_step_not_the_strip() {
    let mut s = Scenario::Elevacao.prepared_state();
    let a = s.activate(ControlId::DialogPrimary);
    s.track(Ticket(7000), &a.commands[0]);
    assert_eq!(s.elevation().unwrap().phase, ElevPhase::Waiting);
    s.apply(UiEvent::Reply { ticket: Ticket(7000), result: Err(ApiError::new(ApiCode::Timeout)) });
    assert_eq!(s.elevation().unwrap().phase, ElevPhase::Failed(ApiCode::Timeout));
    assert!(s.error().is_none(), "no strip over the content");

    // The 120 s of the command run out in the state's own clock: the same.
    let mut s = Scenario::Elevacao.prepared_state();
    let a = s.activate(ControlId::DialogPrimary);
    s.track(Ticket(7001), &a.commands[0]);
    for _ in 0..120 {
        s.tick();
    }
    assert_eq!(s.elevation().unwrap().phase, ElevPhase::Failed(ApiCode::Timeout));
    assert!(s.error().is_none());

    // The step was cancelled meanwhile: nothing to show anywhere.
    let mut s = Scenario::Elevacao.prepared_state();
    let a = s.activate(ControlId::DialogPrimary);
    s.track(Ticket(7002), &a.commands[0]);
    s.activate(ControlId::DialogSecondary);
    s.apply(UiEvent::Reply { ticket: Ticket(7002), result: Err(ApiError::new(ApiCode::Timeout)) });
    assert!(s.elevation().is_none() && s.error().is_none());
}

#[test]
fn each_failure_code_has_its_line_in_the_step() {
    let line = |code: ApiCode| {
        let mut s = Scenario::Elevacao.prepared_state();
        let a = s.activate(ControlId::DialogPrimary);
        let Command::Elevate { action, run, .. } = a.commands[0].clone() else { panic!() };
        s.apply(UiEvent::ElevationResult { action, run, result: Err(ApiError::new(code)) });
        let t = s.dialog_texts(&DialogKind::Elevation);
        assert_eq!(t.title, s.tm(&Msg::new(MsgKey::ElevFailedTitle)));
        assert_eq!(t.body, vec![s.tm(&Msg::new(MsgKey::ElevFailedBody))], "{code:?}");
        t.extra
    };
    let tm = |key: MsgKey| Some(Scenario::Elevacao.prepared_state().tm(&Msg::new(key)));
    // D56: a declined confirmation says so.
    assert_eq!(line(ApiCode::Refused), tm(MsgKey::ElevDeclined));
    assert_eq!(line(ApiCode::NotPermitted), tm(MsgKey::ElevNotAdmin));
    assert_eq!(line(ApiCode::Timeout), tm(MsgKey::ElevTimedOut));
    assert_eq!(line(ApiCode::NotFound), tm(MsgKey::ErrNotFound));
    assert_eq!(line(ApiCode::Busy), tm(MsgKey::ErrBusy));
    assert_eq!(line(ApiCode::ServiceDown), tm(MsgKey::ErrServiceDown));
    assert_eq!(line(ApiCode::Internal), tm(MsgKey::ErrInternal));
    let all: Vec<_> = [ApiCode::NotPermitted, ApiCode::Timeout, ApiCode::NotFound, ApiCode::Busy, ApiCode::Internal]
        .into_iter()
        .map(line)
        .collect();
    for (i, a) in all.iter().enumerate() {
        assert!(a.as_ref().is_some_and(|t| !t.is_empty()));
        assert!(all[i + 1..].iter().all(|b| b != a), "each code reads its own line");
    }
}

#[test]
fn other_admin_only_commands_also_lead_to_the_elevation_step() {
    let mut s = viewer();
    tracked_error(&mut s, &Command::ElevateSession { session_id: 1 }, ApiCode::NeedsAdmin);
    assert!(matches!(s.elevation().unwrap().action, ElevatedAction::ElevateSession { session_id: 1 }));
    // ACCOUNTS.md 9: entering without a prompt asks nothing on this machine any more.
    let mut s = viewer();
    let cmd = Command::Connect { device_id: MAC.into(), intent: Intent::Control, unattended: true };
    tracked_error(&mut s, &cmd, ApiCode::NeedsAdmin);
    assert!(s.elevation().is_none());
    let mut s = viewer();
    tracked_error(&mut s, &Command::TermOpen { session_id: 1, admin: true, cols: 80, rows: 24 }, ApiCode::NeedsAdmin);
    assert!(matches!(s.elevation().unwrap().action, ElevatedAction::OpenAdminTerminal { session_id: 1 }));
    // A refusal that is not about administrators shows the notice strip instead.
    let mut s = base();
    tracked_error(&mut s, &Command::ListDevices, ApiCode::ServiceDown);
    assert_eq!(s.error().unwrap().code, ApiCode::ServiceDown);
    s.activate(ControlId::ErrorClose);
    assert!(s.error().is_none());
}

#[test]
fn session_events_open_and_close_the_windows() {
    let mut s = base();
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::View),
            elevation: Default::default(),
        },
    });
    assert_eq!(s.windows(), vec![WindowKind::Main, WindowKind::Viewer]);
    s.activate(ControlId::TbChat);
    assert_eq!(s.drawer(), Some(DrawerTab::Chat));
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Ended { reason: Some(ApiCode::Refused) },
    });
    assert_eq!(s.windows(), vec![WindowKind::Main]);
    assert_eq!(s.drawer(), None, "the drawer closes with the session");
    assert_eq!(s.error().unwrap().code, ApiCode::Refused, "a refused session says so");

    let mut s = base();
    s.apply(UiEvent::Session {
        session_id: 2,
        device_id: MAC.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Host,
            granted: Permissions::NONE,
            elevation: Default::default(),
        },
    });
    assert_eq!(s.windows(), vec![WindowKind::Main, WindowKind::Banner]);
    s.apply(UiEvent::Session {
        session_id: 2,
        device_id: MAC.into(),
        state: SessionState::Ended { reason: Some(ApiCode::NotFound) },
    });
    assert_eq!(s.windows(), vec![WindowKind::Main]);
    assert!(s.error().is_none(), "a normal end is silent");
}

#[test]
fn reconnecting_and_waking_states_show_where_the_design_puts_them() {
    let s = Scenario::EstadoReconectando.prepared_state();
    assert!(matches!(s.viewer_session().unwrap().state, SessionState::Reconnecting { attempt: 2 }));
    let ids: Vec<_> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::ReconnectDisconnect) && !ids.contains(&ControlId::TbSwap));

    let mut s = Scenario::EstadoAcordando.prepared_state();
    assert!(s.waking_session().is_some());
    assert_eq!(s.windows(), vec![WindowKind::Main], "no viewer yet");
    assert!(s.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::WakeCancel));
    assert!(s.needs_tick());
    s.tick();
    assert!(matches!(s.waking_session().unwrap().state, SessionState::Waking { remaining_s: 35 }));
    let a = s.activate(ControlId::WakeCancel);
    assert!(matches!(a.commands[0], Command::Disconnect { session_id: 3 }));
}

#[test]
fn network_notices_replace_the_content_with_their_card() {
    // The Windows notices open Windows' own Network & internet page through the shell, with no command.
    let mut s = Scenario::EstadoRedePublica.prepared_state();
    assert_eq!(s.notices(), vec![NoticeKind::Public]);
    let a = s.activate(ControlId::Notice(NoticeKind::Public));
    assert!(a.commands.is_empty(), "{:?}", a.commands);
    assert_eq!(a.effect, Some(Effect::OpenWebPage));
    assert_eq!(s.take_web_page().as_deref(), Some("ms-settings:network-status"));
    assert!(crate::state::openable_page("ms-settings:network-status"));
    assert!(!crate::state::openable_page("ms-settings:windowsupdate"), "only the listed system pages");
    assert!(!crate::state::openable_page("file:///C:/Windows/System32/cmd.exe"));
    // The Mac notices are the app's own doing: each click is an effect the shell acts on, never a backend command
    // (the real backend has no op for one).
    for (sc, kind, effect) in [
        (Scenario::EstadoServicoMac, NoticeKind::Service, Effect::OpenLoginItems),
        (Scenario::EstadoPermissoesMac, NoticeKind::MacPerms, Effect::RequestOsPermissions),
    ] {
        let mut s = sc.prepared_state();
        assert_eq!(s.notices(), vec![kind], "{}", sc.name());
        let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
        assert!(ids.contains(&ControlId::Notice(kind)));
        let a = s.activate(ControlId::Notice(kind));
        assert!(a.commands.is_empty(), "{}: {:?}", sc.name(), a.commands);
        assert_eq!(a.effect, Some(effect), "{}", sc.name());
    }
    // "Conceder permissões" asks for what the notice lists, in its order, and the first one's pane is a page the
    // shells may open.
    let mut s = Scenario::EstadoPermissoesMac.prepared_state();
    s.activate(ControlId::Notice(NoticeKind::MacPerms));
    let asked = s.take_os_permission_request();
    assert_eq!(asked, vec![OsPermission::Accessibility, OsPermission::InputMonitoring]);
    assert!(s.take_os_permission_request().is_empty(), "taken once");
    for p in [OsPermission::Accessibility, OsPermission::InputMonitoring, OsPermission::ScreenRecording] {
        assert!(crate::state::openable_page(crate::state::os_permission_page(p)), "{p:?}");
    }
    assert_eq!(
        crate::state::os_permission_page(OsPermission::Accessibility),
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
    );
    assert_eq!(
        crate::state::os_permission_page(OsPermission::InputMonitoring),
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
    );
    assert_eq!(
        crate::state::os_permission_page(OsPermission::ScreenRecording),
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
    );
    let s = Scenario::EstadoPermissoesMac.prepared_state();
    let (title, body, _) = s.notice_texts(NoticeKind::MacPerms);
    assert_eq!(title, "Faltam 2 permissões no Mac");
    assert!(body.contains("Acessibilidade") && body.contains("Monitoramento de Entrada"));
    let (title, body, _) = Scenario::EstadoRedePublica.prepared_state().notice_texts(NoticeKind::Public);
    assert_eq!(title, "O Windows marcou esta rede como Pública");
    assert!(body.contains("Wi-Fi Casa"));
    // The macOS permissions come back through a reply too.
    let mut s = Scenario::EstadoPermissoesMac.prepared_state();
    s.apply(reply_ok(Reply::MissingPermissions(Vec::new())));
    assert!(s.notices().is_empty());
}

fn network_with(notices: Vec<Notice>) -> NetworkStatus {
    NetworkStatus {
        label: "Wi-Fi Casa".into(),
        address: Some("192.168.0.12".into()),
        category: Some(NetCategoryView::Private),
        notices,
    }
}

/// WAITING.md row 25: after "Conceder permissões" the notice says it waits for the person in System Settings, until
/// the list of missing permissions changes or the notice goes.
#[test]
fn after_conceder_permissoes_the_notice_waits_until_the_missing_list_changes_or_the_notice_goes() {
    const WAITING: &str = "Esperando você permitir nos Ajustes do Sistema…";
    let mut s = Scenario::EstadoPermissoesMac.prepared_state();
    let (title, listed, button) = s.notice_texts(NoticeKind::MacPerms);
    assert!(listed.contains("Acessibilidade") && !listed.contains(WAITING), "before the click: the list. {listed}");
    // The click asks macOS and the notice says it waits, keeping its title and its button (a second try is allowed).
    s.activate(ControlId::Notice(NoticeKind::MacPerms));
    let (t, body, b) = s.notice_texts(NoticeKind::MacPerms);
    assert_eq!((t.as_str(), body.as_str(), b.as_str()), (title.as_str(), WAITING, button.as_str()));
    // The same list again (the Mac re-read the grants on activation and nothing changed): still waiting.
    s.apply(UiEvent::NetworkChanged(network_with(vec![Notice::MacPermissionsMissing(vec![
        OsPermission::Accessibility,
        OsPermission::InputMonitoring,
    ])])));
    assert_eq!(s.notice_texts(NoticeKind::MacPerms).1, WAITING);
    s.apply(reply_ok(Reply::MissingPermissions(vec![OsPermission::Accessibility, OsPermission::InputMonitoring])));
    assert_eq!(s.notice_texts(NoticeKind::MacPerms).1, WAITING);
    // The person allowed one: the list changed, so the notice lists what is still missing and waits no more.
    s.apply(reply_ok(Reply::MissingPermissions(vec![OsPermission::InputMonitoring])));
    let (t, body, _) = s.notice_texts(NoticeKind::MacPerms);
    assert_eq!(t, "Falta 1 permissão no Mac");
    assert!(body.contains("Monitoramento de Entrada") && !body.contains(WAITING), "{body}");
    // Pressing the button again waits again, for the new list.
    s.activate(ControlId::Notice(NoticeKind::MacPerms));
    assert_eq!(s.take_os_permission_request(), vec![OsPermission::InputMonitoring]);
    assert_eq!(s.notice_texts(NoticeKind::MacPerms).1, WAITING);
    // The last one allowed: the notice goes, and a notice that comes back later with the same list does not wait.
    s.apply(reply_ok(Reply::MissingPermissions(Vec::new())));
    assert!(s.notices().is_empty());
    s.apply(reply_ok(Reply::MissingPermissions(vec![OsPermission::InputMonitoring])));
    assert!(!s.notice_texts(NoticeKind::MacPerms).1.contains(WAITING), "a new notice is not the old wait");
    // The service's own push (the network status) that drops the notice ends the wait too.
    s.activate(ControlId::Notice(NoticeKind::MacPerms));
    assert_eq!(s.notice_texts(NoticeKind::MacPerms).1, WAITING);
    s.apply(UiEvent::NetworkChanged(network_with(Vec::new())));
    assert!(s.notices().is_empty());
    s.apply(UiEvent::NetworkChanged(network_with(vec![Notice::MacPermissionsMissing(vec![
        OsPermission::InputMonitoring,
    ])])));
    assert!(!s.notice_texts(NoticeKind::MacPerms).1.contains(WAITING));
}

/// The wait line follows the interface's language.
#[test]
fn the_mac_permissions_wait_line_is_in_the_interface_language() {
    let mut s = Scenario::EstadoPermissoesMac.prepared_state();
    s.activate(ControlId::Notice(NoticeKind::MacPerms));
    s.set_language(Language::En);
    assert_eq!(s.notice_texts(NoticeKind::MacPerms).1, "Waiting for you to allow it in System Settings…");
}

#[test]
fn a_refused_receiving_folder_shows_a_notice_with_its_name_and_the_button_opens_files_and_folders() {
    let mut s = Scenario::EstadoPastaRecebimentoMac.prepared_state();
    assert_eq!(s.notices(), vec![NoticeKind::ReceiveFolder]);
    let (title, body, button) = s.notice_texts(NoticeKind::ReceiveFolder);
    assert_eq!(title, "O Swapdesk não pode salvar na pasta de recebimento");
    assert!(body.contains("Downloads") && !body.contains("{0}"), "{body}");
    assert_eq!(button, "Abrir Arquivos e Pastas");
    let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::Notice(NoticeKind::ReceiveFolder)));
    let a = s.activate(ControlId::Notice(NoticeKind::ReceiveFolder));
    assert!(a.commands.is_empty(), "{:?}", a.commands);
    assert_eq!(a.effect, Some(Effect::OpenWebPage));
    let page = s.take_web_page().expect("a page to open");
    assert_eq!(page, "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders");
    assert!(crate::state::openable_page(&page));
    // Once access works, the status comes again without the notice.
    s.apply(UiEvent::NetworkChanged(NetworkStatus {
        label: "Wi-Fi Casa".into(),
        address: None,
        category: None,
        notices: Vec::new(),
    }));
    assert!(s.notices().is_empty());
}

#[test]
fn swap_dialogs_and_events() {
    let mut s = Scenario::Inverter.prepared_state();
    let a = s.activate(ControlId::DialogPrimary);
    assert!(matches!(a.commands[0], Command::RequestSwap { session_id: 1 }));
    assert_eq!(s.dialog(WindowKind::Viewer), None);
    let mut s = Scenario::Inverter.prepared_state();
    s.activate(ControlId::DialogSecondary);
    assert!(s.swap().is_none());

    let mut s = Scenario::InverterPedido.prepared_state();
    s.tick();
    let t = s.dialog_texts(&DialogKind::SwapAsk);
    assert!(t.extra.unwrap().contains("13"));
    let a = s.activate(ControlId::DialogPrimary);
    assert_eq!(a.commands, vec![Command::AnswerSwap { request_id: 11, accept: true }]);
    let mut s = Scenario::InverterPedido.prepared_state();
    let a = s.activate(ControlId::DialogSecondary);
    assert_eq!(a.commands, vec![Command::AnswerSwap { request_id: 11, accept: false }]);

    let mut s = Scenario::EstadoInversaoRecusada.prepared_state();
    assert!(s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::SwapDeclinedClose));
    s.activate(ControlId::SwapDeclinedClose);
    assert!(s.swap().is_none());
    s.apply(UiEvent::Swap {
        session_id: 1,
        event: SwapEvent::Asked { request_id: 1, by: "x".into(), remaining_s: 20 },
    });
    s.apply(UiEvent::Swap { session_id: 1, event: SwapEvent::Done });
    assert!(s.swap().is_none());
    // Asking from the banner, and the reply to a request.
    let mut s = Scenario::Banner.prepared_state();
    assert!(matches!(s.activate(ControlId::BannerSwap).commands[0], Command::RequestSwap { session_id: 2 }));
    assert!(matches!(s.activate(ControlId::BannerDisconnect).commands[0], Command::Disconnect { session_id: 2 }));
    let mut s = Scenario::Inverter.prepared_state();
    s.apply(reply_ok(Reply::RequestId(5)));
    assert!(matches!(s.swap().unwrap().phase, SwapPhase::Confirm), "only a sent request moves on");
    s.activate(ControlId::DialogPrimary);
    assert!(matches!(s.swap().unwrap().phase, SwapPhase::Requested { request_id: 0 }));
    s.apply(reply_ok(Reply::RequestId(5)));
    assert!(matches!(s.swap().unwrap().phase, SwapPhase::Requested { request_id: 5 }));
}

#[test]
fn the_toolbar_sends_session_commands_and_toggles() {
    let mut s = viewer();
    assert!(matches!(s.activate(ControlId::TbScreen).commands[0], Command::ListDisplays { session_id: 1 }));
    assert!(matches!(s.activate(ControlId::TbFit).commands[0], Command::SetScale { scale: Scale::OneToOne, .. }));
    assert!(matches!(s.activate(ControlId::TbFit).commands[0], Command::SetScale { scale: Scale::Fit, .. }));
    assert!(matches!(s.activate(ControlId::TbAudio).commands[0], Command::SetAudio { on: false, .. }));
    assert!(!s.audio_on());
    assert!(matches!(s.activate(ControlId::TbGrab).commands[0], Command::SetGrab { on: false, .. }));
    // The keys button opens the list of keys under itself; the one picked is sent.
    let a = s.activate(ControlId::TbKeys);
    assert!(a.commands.is_empty());
    assert_eq!(a.effect, Some(Effect::Menu(SelectId::SpecialKeys)));
    assert_eq!(SelectId::SpecialKeys.anchor(), ControlId::TbKeys);
    let names: Vec<String> = s
        .select_options(SelectId::SpecialKeys)
        .into_iter()
        .map(|(n, on)| {
            assert!(!on, "a list of actions: nothing is checked");
            n
        })
        .collect();
    assert_eq!(names.len(), SpecialKeys::ALL.len());
    assert_eq!(names[0], "Ctrl+Alt+Del");
    for (i, keys) in SpecialKeys::ALL.iter().enumerate() {
        let a = s.choose(SelectId::SpecialKeys, i);
        assert_eq!(a.commands, vec![Command::SendKeys { session_id: 1, keys: *keys }]);
    }
    assert!(s.choose(SelectId::SpecialKeys, SpecialKeys::ALL.len()).commands.is_empty(), "past the list: nothing");
    assert!(matches!(s.activate(ControlId::TbDisconnect).commands[0], Command::Disconnect { session_id: 1 }));
    let t = Ticket(8);
    s.track(t, &Command::Disconnect { session_id: 1 });
    s.apply(UiEvent::Reply { ticket: t, result: Ok(Reply::Done) });
    assert!(s.viewer_session().is_none());
}

#[test]
fn the_drawer_opens_per_tab_and_asks_for_its_data_once() {
    let mut s = viewer();
    let a = s.activate(ControlId::TbTerminal);
    assert!(matches!(a.commands[0], Command::TermOpen { admin: false, .. }));
    assert_eq!(s.drawer(), Some(DrawerTab::Terminal));
    s.apply(UiEvent::Term { term_id: 1, event: TermEvent::Opened { account: "alex".into() } });
    assert_eq!(s.term_account(), Some("alex"));
    s.activate(ControlId::DrawerTab(DrawerTab::Chat));
    assert_eq!(s.drawer(), Some(DrawerTab::Chat));
    let a = s.activate(ControlId::DrawerTab(DrawerTab::Terminal));
    assert!(a.commands.is_empty(), "the terminal is already open");
    assert!(matches!(
        s.activate(ControlId::Action(ActionKind::Lock)).commands[0],
        Command::RunAction { action: ActionKind::Lock, .. }
    ));
    s.activate(ControlId::DrawerClose);
    assert_eq!(s.drawer(), None);
    s.activate(ControlId::TbFiles);
    assert!(s.explorer_open(), "the folder button opens the Arquivos view");
    s.activate(ControlId::TbFiles);
    assert!(!s.explorer_open(), "and closes it");
    assert_eq!(s.drawer(), None);
    s.apply(UiEvent::Term { term_id: 1, event: TermEvent::Exited { code: Some(0) } });
    assert_eq!(s.term_account(), None);
}

#[test]
fn chat_transfers_and_clipboard_events() {
    let mut s = viewer();
    s.apply(UiEvent::Chat {
        session_id: 1,
        message: ChatView { msg_id: 1, mine: true, text: "a".into(), sent_at_ms: 1, read: false },
    });
    s.apply(UiEvent::Chat {
        session_id: 1,
        message: ChatView { msg_id: 1, mine: true, text: "a".into(), sent_at_ms: 1, read: false },
    });
    assert_eq!(s.chat().len(), 1, "the same message id is one message");
    s.apply(UiEvent::ChatRead { session_id: 1, up_to: 1 });
    assert!(s.chat()[0].read);
    let a = s.submit_edit(EditId::Chat, " olá ".into());
    assert_eq!(a.commands, vec![Command::ChatSend { session_id: 1, text: "olá".into() }]);
    assert!(s.submit_edit(EditId::Chat, "   ".into()).commands.is_empty());
    assert_eq!(s.activate(ControlId::ChatSend).effect, Some(Effect::SubmitEdit(EditId::Chat)));
    s.apply(reply_ok(Reply::Chat(ChatView { msg_id: 2, mine: true, text: "b".into(), sent_at_ms: 2, read: false })));
    assert_eq!(s.chat().len(), 2);

    for state in [TransferState::Queued, TransferState::Done] {
        s.apply(UiEvent::Transfer { transfer_id: 1, session_id: 1, name: "f".into(), outgoing: true, state });
    }
    assert_eq!(s.transfers().len(), 1);
    assert_eq!(s.transfers()[0].state, TransferState::Done);

    assert!(matches!(s.activate(ControlId::ClipToggle).commands[0], Command::SetClipboardSync { on: false, .. }));
    assert!(!s.clipboard_on());
    s.apply(UiEvent::ClipboardSkipped { session_id: 1, reason: ApiCode::NotPermitted });
    assert_eq!(s.error().unwrap().code, ApiCode::NotPermitted);
    let shown = s.error_text(&s.error().unwrap().clone());
    assert_eq!(shown, "O que foi copiado veio marcado como privado, como uma senha, e não foi enviado.", "{shown}");
    s.apply(UiEvent::ClipboardSkipped { session_id: 1, reason: ApiCode::Invalid });
    assert_eq!(s.error_text(&s.error().unwrap().clone()), "O que foi copiado é grande demais e não foi enviado.");
    s.apply(UiEvent::ClipboardSkipped { session_id: 1, reason: ApiCode::Refused });
    assert!(s.error_text(&s.error().unwrap().clone()).contains("tipo de conteúdo"));
}

#[test]
fn stats_redraw_only_when_the_option_is_on() {
    let mut s = viewer();
    assert!(s.settings().unwrap().show_stats);
    let c = s.apply(UiEvent::Stats { session_id: 1, stats: Stats { latency_ms: 5, fps: 30, kbps: 1, hevc: false } });
    assert!(c.hits(WindowKind::Viewer));
    assert_eq!(s.viewer_session().unwrap().stats.unwrap().fps, 30);
    let mut s = base();
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE,
            elevation: Default::default(),
        },
    });
    let c = s.apply(UiEvent::Stats { session_id: 1, stats: Stats { latency_ms: 5, fps: 30, kbps: 1, hevc: false } });
    assert!(c.is_none(), "no redraw for numbers nobody sees");
}

#[test]
fn activity_alert_block_and_export() {
    let mut s = Scenario::Atividade.prepared_state();
    assert!(s.alert().is_some(), "the unknown-device row raises the alert");
    let rows = s.activity().len();
    let a = s.activate(ControlId::BlockAlert);
    assert_eq!(a.commands, vec![Command::BlockAddress { ipv4: "192.168.0.57".into() }]);
    assert!(s.alert().is_none());
    assert_eq!(s.activate(ControlId::Export).effect, Some(Effect::SaveLog));
    s.apply(UiEvent::ActivityAlert(AuditView {
        at_ms: 1,
        device_name: String::new(),
        what: Msg::new(crate::MsgKey::AuditUnknownPair).with("10.0.0.9"),
        duration_s: None,
        alert: true,
    }));
    assert_eq!(s.activity().len(), rows + 1);
    assert_eq!(s.alert().unwrap().what.args[0], "10.0.0.9");
}

#[test]
fn a_removed_device_is_named_removed_never_by_its_key() {
    // Defect after 1.0: the service sends a device no longer paired with no name; both screens name it in words.
    let mut s = base();
    let row = |what: Msg| AuditView { at_ms: 1, device_name: String::new(), what, duration_s: None, alert: false };
    s.apply(reply_ok(Reply::Activity(vec![
        row(Msg::new(MsgKey::AuditSwap).with("")),
        row(Msg::new(MsgKey::AuditPaired)),
    ])));
    let lines = match s.home().cards.into_iter().find(|c| matches!(c, crate::home::HomeCard::Recent { .. })) {
        Some(crate::home::HomeCard::Recent { lines, .. }) => lines,
        _ => panic!("no recent card"),
    };
    assert_eq!(lines.len(), 2);
    assert!(
        lines.iter().all(|l| l.who == "Dispositivo removido"),
        "{:?}",
        lines.iter().map(|l| &l.who).collect::<Vec<_>>()
    );
    assert!(lines[0].what.ends_with("dispositivo removido"), "{}", lines[0].what);
    let swap = s.activity()[0].clone();
    s.apply(UiEvent::LanguageChanged(Language::En));
    assert_eq!(s.activity_who(&swap), "Removed device");
    assert!(s.activity_what(&swap).ends_with("removed device"), "{}", s.activity_what(&swap));
}

#[test]
fn a_device_list_push_reads_the_activity_again_once_and_a_closed_alert_stays_closed() {
    // Defect after 1.0: Início missed a session until the app was reopened. The service logs a session before it pushes
    // the device list, so each push asks for the activity again (one request at a time).
    let mut s = Scenario::Atividade.prepared_state();
    let _ = s.take_followups();
    s.activate(ControlId::BlockAlert);
    let _ = s.take_followups();
    assert!(s.alert().is_none());
    let devices = s.devices().to_vec();
    let refresh = Command::ActivityList { before_ms: None, limit: 200 };

    s.apply(UiEvent::DevicesChanged(devices.clone()));
    assert_eq!(s.take_followups(), vec![refresh.clone()]);
    s.apply(UiEvent::DevicesChanged(devices.clone()));
    assert!(s.take_followups().is_empty(), "one refresh at a time");

    let mut rows = s.activity().to_vec();
    rows.insert(
        0,
        AuditView {
            at_ms: 2,
            device_name: "MacBook".into(),
            what: Msg::new(MsgKey::AuditControlled),
            duration_s: Some(60),
            alert: false,
        },
    );
    s.apply(reply_ok(Reply::Activity(rows)));
    assert_eq!(s.activity()[0].what.key, MsgKey::AuditControlled, "the new session shows");
    assert!(s.alert().is_none(), "the old alert row does not raise the closed alert again");
    s.apply(UiEvent::DevicesChanged(devices));
    assert_eq!(s.take_followups(), vec![refresh], "the next push asks again");
}

#[test]
fn every_setting_toggles_and_every_choice_applies() {
    let mut s = Scenario::Ajustes.prepared_state();
    let before = s.settings().unwrap().clone();
    for (id, check) in [
        (
            ControlId::OpenAtLogin,
            (|a: &Settings, b: &Settings| a.open_at_login != b.open_at_login) as fn(&Settings, &Settings) -> bool,
        ),
        (ControlId::LockOnEnd, |a, b| a.lock_on_end != b.lock_on_end),
        (ControlId::KeepAwake, |a, b| a.keep_awake != b.keep_awake),
        (ControlId::ShowStats, |a, b| a.show_stats != b.show_stats),
        (ControlId::SwapCtrlCmd, |a, b| a.swap_ctrl_cmd != b.swap_ctrl_cmd),
    ] {
        let a = s.activate(id);
        assert!(check(&before, s.settings().unwrap()), "{id:?}");
        assert!(matches!(a.commands[0], Command::SetSettings { .. }));
        s.activate(id);
    }
    assert_eq!(s.settings().unwrap(), &before, "toggling twice is a round trip");

    assert_eq!(s.activate(ControlId::Select(SelectId::Quality)).effect, Some(Effect::Menu(SelectId::Quality)));
    assert_eq!(s.select_options(SelectId::Quality).len(), 3);
    s.choose(SelectId::Quality, 1);
    assert_eq!(s.settings().unwrap().quality, Quality::Text);
    assert_eq!(s.select_value(SelectId::Quality), "Texto nítido");
    s.choose(SelectId::Keyboard, 2);
    assert_eq!(s.settings().unwrap().keyboard_mode, KeyboardMode::Text);
    s.choose(SelectId::Language, 1);
    assert_eq!(s.language(), Language::En, "the language changes the whole interface at once");
    assert_eq!(s.t(crate::MsgKey::SettingsTitle), "Settings");
    assert_eq!(s.activate(ControlId::Edit(EditId::Name)).effect, Some(Effect::FocusEdit(EditId::Name)));
    let a = s.submit_edit(EditId::Name, "  Novo nome ".into());
    assert!(matches!(&a.commands[0], Command::SetSettings { settings } if settings.name == "Novo nome"));
    assert!(s.submit_edit(EditId::Name, "Novo nome".into()).commands.is_empty(), "unchanged");
    assert!(s.submit_edit(EditId::Name, "  ".into()).commands.is_empty(), "empty is ignored");
}

#[test]
fn settings_and_theme_events_change_the_interface() {
    let mut s = base();
    assert!(!s.dark());
    assert!(s.apply(UiEvent::ThemeChanged { dark: true }).hits(WindowKind::Main));
    assert!(s.dark());
    assert!(s.apply(UiEvent::ThemeChanged { dark: true }).is_none(), "no change, no redraw");
    s.apply(UiEvent::LanguageChanged(Language::En));
    assert_eq!(s.language(), Language::En);
    let mut set = s.settings().unwrap().clone();
    set.language = Language::PtBr;
    s.apply(UiEvent::SettingsChanged(set));
    assert_eq!(s.language(), Language::En, "the language is per user (C5.1): the service's copy never overrides it");
    assert_eq!(s.settings().unwrap().language, Language::En, "Ajustes shows the language in use");
}

#[test]
fn a_refused_language_change_is_undone() {
    let mut s = base();
    let before = s.language();
    let a = s.choose(SelectId::Language, if before == Language::En { 0 } else { 1 });
    assert_ne!(s.language(), before);
    tracked_error(&mut s, &a.commands[0], ApiCode::Refused);
    assert_eq!(s.language(), before, "the undo brings the language back with the rest of the settings");
}

#[test]
fn the_theme_select_follows_the_language_one_and_decides_the_appearance() {
    let mut s = base();
    s.navigate(crate::Nav::Settings);
    let ids: Vec<ControlId> = s.controls(WindowKind::Main).iter().map(|c| c.id).collect();
    let lang = ids.iter().position(|i| *i == ControlId::Select(SelectId::Language)).unwrap();
    assert_eq!(ids[lang + 1], ControlId::Select(SelectId::Theme), "Tema sits right after Idioma");
    assert_eq!(s.select_options(SelectId::Theme).len(), 3);
    assert_eq!(s.select_value(SelectId::Theme), "Sistema", "the default follows the OS");
    assert_eq!(s.activate(ControlId::Select(SelectId::Theme)).effect, Some(Effect::Menu(SelectId::Theme)));

    assert!(!s.dark());
    let a = s.choose(SelectId::Theme, 2);
    assert!(matches!(&a.commands[0], Command::SetSettings { settings } if settings.theme == Theme::Dark));
    assert!(s.dark(), "Escuro with a light OS");
    assert_eq!(s.select_value(SelectId::Theme), "Escuro");
    s.apply(UiEvent::ThemeChanged { dark: true });
    s.choose(SelectId::Theme, 1);
    assert!(!s.dark(), "Claro with a dark OS");
    s.choose(SelectId::Theme, 0);
    assert!(s.dark(), "Sistema shows the OS again");
    assert!(s.apply(UiEvent::ThemeChanged { dark: false }).hits(WindowKind::Main));
    assert!(!s.dark(), "and follows its changes live");
    s.choose(SelectId::Language, 1);
    assert_eq!(s.select_value(SelectId::Theme), "System");
    assert_eq!(s.t(crate::MsgKey::FieldTheme), "Theme");
}

#[test]
fn a_chosen_theme_ignores_os_changes_and_the_services_copy_and_a_refusal_undoes_it() {
    let mut s = base();
    let a = s.choose(SelectId::Theme, 2);
    assert!(s.apply(UiEvent::ThemeChanged { dark: true }).is_none(), "already dark: no redraw");
    assert!(s.apply(UiEvent::ThemeChanged { dark: false }).is_none(), "the OS no longer matters");
    assert!(s.dark());
    let mut set = s.settings().unwrap().clone();
    set.theme = Theme::Light;
    s.apply(UiEvent::SettingsChanged(set));
    assert_eq!(s.theme(), Theme::Dark, "per user: the service's copy never overrides it");
    assert_eq!(s.settings().unwrap().theme, Theme::Dark);
    tracked_error(&mut s, &a.commands[0], ApiCode::Refused);
    assert_eq!(s.theme(), Theme::System, "the undo brings the theme back with the rest of the settings");
}

#[test]
fn countdowns_stop_at_zero_and_the_timer_stops_with_them() {
    let mut s = Scenario::ParearEsteComputador.prepared_state();
    for _ in 0..200 {
        s.tick();
    }
    assert_eq!(s.pairing().unwrap().state, PairingState::Open { remaining_s: 0 });
    assert_eq!(crate::format::countdown(102), "1:42");
    let mut s = base();
    assert!(!s.needs_tick(), "an idle window has no timer");
    assert!(s.tick().is_none());
}

#[test]
fn the_keyboard_order_starts_in_the_sidebar_and_every_device_is_reachable() {
    let s = base();
    let c = s.controls(WindowKind::Main);
    assert_eq!(c[0].id, ControlId::NavHome);
    assert!(matches!(c[1].id, ControlId::Device(_)));
    let devices = c.iter().filter(|c| matches!(c.id, ControlId::Device(_))).count();
    assert_eq!(devices, 3);
    // Tab order: sidebar, then header, tabs, content.
    let pos = |id: ControlId| c.iter().position(|c| c.id == id).unwrap();
    assert!(pos(ControlId::NavSettings) < pos(ControlId::ViewOnly));
    assert!(pos(ControlId::Control) < pos(ControlId::Tab(DeviceTab::Overview)));
    assert!(pos(ControlId::Tab(DeviceTab::History)) < pos(ControlId::EditGrants));
}

#[test]
fn a_modal_dialog_takes_every_control_of_its_window() {
    for sc in [Scenario::PedidoConexao, Scenario::Consentimento, Scenario::Elevacao, Scenario::InverterPedido] {
        let s = sc.prepared_state();
        let c = s.controls(WindowKind::Main);
        assert!(
            c.iter().all(|c| c.region == crate::Region::Dialog),
            "{}: the sidebar is not reachable behind a dialog",
            sc.name()
        );
    }
}

// ---------- stage 41: swap reasons, chat, wake, reconnect ----------

fn labels(s: &UiState, kind: WindowKind) -> String {
    s.controls(kind).into_iter().map(|c| c.label).collect::<Vec<_>>().join(" | ")
}

#[test]
fn a_declined_swap_says_why_on_the_viewer_card_and_on_the_hosts_strip() {
    for (why, needle) in [
        (SwapWhy::Declined, "recusou"),
        (SwapWhy::TimedOut, "Ninguém respondeu em 20 segundos"),
        (SwapWhy::NobodyPresent, "ninguém no"),
        (SwapWhy::NotPermitted, "permissão de controlar este computador"),
        (SwapWhy::Busy, "em andamento"),
    ] {
        let mut s = viewer();
        s.apply(UiEvent::Swap { session_id: 1, event: SwapEvent::Declined { why } });
        assert_eq!(s.swap().unwrap().phase, SwapPhase::Declined(why));
        let text = labels(&s, WindowKind::Viewer);
        assert!(text.contains(needle), "{why:?}: {text}");
        assert!(
            s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::SwapDeclinedClose),
            "{why:?} has its close button"
        );
        // The title says "did not accept" for a refusal and "cannot now" for what could not even be asked.
        let (title, _) = s.swap_declined_texts();
        assert_eq!(
            title.contains("não aceitou"),
            matches!(why, SwapWhy::Declined | SwapWhy::TimedOut | SwapWhy::NobodyPresent),
            "{why:?}: {title}"
        );
    }
    // The host has no viewer window: the strip over its content says it.
    for (why, code, needle) in [
        (SwapWhy::Declined, ApiCode::Refused, "recusou"),
        (SwapWhy::TimedOut, ApiCode::Timeout, ""),
        (SwapWhy::NobodyPresent, ApiCode::Refused, "ninguém no"),
        (SwapWhy::NotPermitted, ApiCode::NotPermitted, "permissão de controlar o computador dele"),
        (SwapWhy::Busy, ApiCode::Busy, ""),
    ] {
        let mut s = Scenario::Banner.prepared_state();
        s.apply(UiEvent::Swap { session_id: 2, event: SwapEvent::Declined { why } });
        assert!(s.swap().is_none(), "no card without a viewer window");
        let e = s.error().expect("the strip says why").clone();
        assert_eq!(e.code, code);
        assert!(s.error_text(&e).contains(needle), "{why:?}: {}", s.error_text(&e));
    }
}

#[test]
fn chat_counts_unread_reads_them_when_the_panel_opens_and_keeps_each_sides_numbering() {
    let mut s = viewer();
    s.open_drawer(None);
    let theirs = |id: u32, text: &str| UiEvent::Chat {
        session_id: 1,
        message: ChatView { msg_id: id, mine: false, text: text.into(), sent_at_ms: 1, read: false },
    };
    s.apply(theirs(1, "oi"));
    assert_eq!(s.chat_unread(), 1);
    let chat_label =
        |s: &UiState| s.controls(WindowKind::Viewer).into_iter().find(|c| c.id == ControlId::TbChat).unwrap().label;
    assert_eq!(chat_label(&s), "Chat, 1 não lidas", "the badge is in the button's name too");
    // Each side numbers its own messages from 1: the same number is two messages.
    s.apply(UiEvent::Chat {
        session_id: 1,
        message: ChatView { msg_id: 1, mine: true, text: "e aí".into(), sent_at_ms: 2, read: false },
    });
    assert_eq!(s.chat().len(), 2);
    assert_eq!(s.chat_unread(), 1, "what I wrote is not unread");

    // The session coming up already sent the audio switch.
    assert_eq!(s.take_followups(), vec![Command::SetAudio { session_id: 1, on: true }]);
    // Opening the panel reads everything and tells the peer how far.
    let a = s.activate(ControlId::TbChat);
    assert_eq!(a.commands, vec![Command::ChatMarkRead { session_id: 1, up_to: 1 }]);
    assert_eq!(s.chat_unread(), 0);
    assert_eq!(chat_label(&s), "Chat");
    // With the panel open a message is read as it arrives, and the peer hears it at once.
    s.apply(theirs(2, "tudo bem?"));
    assert_eq!(s.chat_unread(), 0);
    assert_eq!(s.take_followups(), vec![Command::ChatMarkRead { session_id: 1, up_to: 2 }]);
    // Closing the panel and a new message: unread again, nothing sent.
    s.activate(ControlId::TbChat);
    s.apply(theirs(3, "?"));
    assert_eq!(s.chat_unread(), 1);
    assert!(s.take_followups().is_empty());
    // What the peer read is shown on my messages.
    s.apply(UiEvent::ChatRead { session_id: 1, up_to: 1 });
    assert!(s.chat().iter().find(|m| m.mine).unwrap().read);
    // The messages vanish with the session.
    s.apply(UiEvent::Session { session_id: 1, device_id: MAC.into(), state: SessionState::Ended { reason: None } });
    assert!(s.chat().is_empty());

    // The host's banner button carries the count as well.
    let mut h = Scenario::Banner.prepared_state();
    assert_eq!(
        h.controls(WindowKind::Banner).iter().find(|c| c.id == ControlId::BannerChat).unwrap().label,
        "Abrir chat"
    );
    h.apply(UiEvent::Chat {
        session_id: 2,
        message: ChatView { msg_id: 1, mine: false, text: "oi".into(), sent_at_ms: 1, read: false },
    });
    assert_eq!(
        h.controls(WindowKind::Banner).iter().find(|c| c.id == ControlId::BannerChat).unwrap().label,
        "Abrir chat, 1 não lidas"
    );
}

#[test]
fn an_asleep_device_offers_acordar_and_the_waking_entry_runs_to_its_end() {
    let mut s = base();
    s.select_device(DESKTOP);
    let ids = |s: &UiState| s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert!(
        ids(&s).contains(&ControlId::Wake)
            && !ids(&s).contains(&ControlId::Control)
            && !ids(&s).contains(&ControlId::ViewOnly)
    );
    assert_eq!(s.controls(WindowKind::Main).iter().find(|c| c.id == ControlId::Wake).unwrap().label, "Acordar");
    // A device that is online has no such button.
    let mut awake = base();
    awake.select_device(MAC);
    assert!(!awake.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::Wake));

    let a = s.activate(ControlId::Wake);
    assert_eq!(a.commands, vec![Command::Wake { device_id: DESKTOP.into() }]);
    // The reply opens the entry at the full minute; "Acordando" and its cancel take the place of the buttons.
    let command = a.commands[0].clone();
    let t = Ticket(31);
    s.track(t, &command);
    s.apply(UiEvent::Reply { ticket: t, result: Ok(Reply::SessionId(77)) });
    assert!(matches!(s.waking_session().unwrap().state, SessionState::Waking { remaining_s: 60 }));
    assert!(ids(&s).contains(&ControlId::WakeCancel) && !ids(&s).contains(&ControlId::Wake));
    assert!(labels(&s, WindowKind::Main).contains("Acordando o Desktop do escritório"));
    assert!(s.needs_tick());
    s.apply(UiEvent::Session {
        session_id: 77,
        device_id: DESKTOP.into(),
        state: SessionState::Waking { remaining_s: 41 },
    });
    s.tick();
    assert!(matches!(s.waking_session().unwrap().state, SessionState::Waking { remaining_s: 40 }));
    // Cancelling asks the service to stop it.
    assert!(matches!(s.activate(ControlId::WakeCancel).commands[0], Command::Disconnect { session_id: 77 }));

    // The minute ran out: the entry goes and the strip says the device did not show.
    s.apply(UiEvent::Session {
        session_id: 77,
        device_id: DESKTOP.into(),
        state: SessionState::Ended { reason: Some(ApiCode::Timeout) },
    });
    assert!(s.waking_session().is_none());
    let e = s.error().expect("the failure is said").clone();
    assert!(s.error_text(&e).contains("não apareceu na rede"), "{}", s.error_text(&e));
    // A wake that ends normally (cancelled, or connected) is silent.
    let mut quiet = base();
    quiet.select_device(DESKTOP);
    quiet.apply(UiEvent::Session {
        session_id: 78,
        device_id: DESKTOP.into(),
        state: SessionState::Waking { remaining_s: 60 },
    });
    quiet.apply(UiEvent::Session {
        session_id: 78,
        device_id: DESKTOP.into(),
        state: SessionState::Ended { reason: None },
    });
    assert!(quiet.error().is_none() && quiet.waking_session().is_none());
}

#[test]
fn a_lost_session_shows_the_attempt_and_says_so_when_the_minute_ends() {
    let mut s = viewer();
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Reconnecting { attempt: 1 },
    });
    let text = labels(&s, WindowKind::Viewer);
    assert!(text.contains("Reconectando ao") && text.contains("Tentativa 1"), "{text}");
    assert!(
        s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::ReconnectDisconnect),
        "the user can still give up"
    );
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Reconnecting { attempt: 3 },
    });
    assert!(labels(&s, WindowKind::Viewer).contains("Tentativa 3"));
    // Back: the toolbar returns, no strip.
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::Control),
            elevation: Default::default(),
        },
    });
    assert!(s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::TbDisconnect));
    assert!(s.error().is_none());
    // Not back within the minute: the session ends and the strip says it could not reconnect.
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Reconnecting { attempt: 9 },
    });
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Ended { reason: Some(ApiCode::Timeout) },
    });
    assert!(s.viewer_session().is_none());
    let e = s.error().expect("the failure is said").clone();
    assert!(s.error_text(&e).contains("Não deu para reconectar"), "{}", s.error_text(&e));
}

#[test]
fn a_pending_swap_request_disables_the_buttons_that_ask_until_the_outcome_comes() {
    // From the banner: the click sends the request and the button waits.
    let mut h = Scenario::Banner.prepared_state();
    let swap_disabled =
        |s: &UiState| s.controls(WindowKind::Banner).iter().find(|c| c.id == ControlId::BannerSwap).unwrap().disabled;
    assert!(!swap_disabled(&h));
    assert!(matches!(h.activate(ControlId::BannerSwap).commands[0], Command::RequestSwap { session_id: 2 }));
    assert!(swap_disabled(&h) && h.swap_requested());
    assert!(h.activate(ControlId::BannerSwap).commands.is_empty(), "one request at a time");
    h.apply(UiEvent::Swap { session_id: 2, event: SwapEvent::Declined { why: SwapWhy::Declined } });
    assert!(!swap_disabled(&h), "the outcome frees it");
    assert!(h.error().is_some());
    // From the viewer: the card's "Pedir inversão" leaves the toolbar button waiting.
    let mut v = Scenario::Inverter.prepared_state();
    v.activate(ControlId::DialogPrimary);
    let tb = |s: &UiState| s.controls(WindowKind::Viewer).iter().find(|c| c.id == ControlId::TbSwap).unwrap().disabled;
    assert!(tb(&v));
    assert!(v.activate(ControlId::TbSwap).commands.is_empty());
    v.apply(UiEvent::Swap { session_id: 1, event: SwapEvent::Done });
    assert!(!tb(&v) && v.swap().is_none());
    // A request the service refuses at once leaves nothing waiting.
    let mut v = Scenario::Inverter.prepared_state();
    let a = v.activate(ControlId::DialogPrimary);
    tracked_error(&mut v, &a.commands[0], ApiCode::NotFound);
    assert!(!v.swap_requested() && !tb(&v));
}

fn session_event(id: u64, device: &str, state: SessionState) -> UiEvent {
    UiEvent::Session { session_id: id, device_id: device.into(), state }
}

fn disp(id: u16, primary: bool) -> DisplayView {
    DisplayView { id, name: String::new(), width: 100, height: 100, scale_milli: 1000, primary }
}

#[test]
fn fit_is_readable_toggled_in_its_spec_and_reverts_when_the_scale_is_refused() {
    let mut s = viewer();
    let toggled = |s: &UiState| {
        s.controls(WindowKind::Viewer).into_iter().find(|c| c.id == ControlId::TbFit).and_then(|c| c.toggled)
    };
    assert!(s.fit());
    assert_eq!(toggled(&s), Some(true));
    let a = s.activate(ControlId::TbFit);
    s.track(Ticket(1), &a.commands[0]);
    assert!(!s.fit());
    assert_eq!(toggled(&s), Some(false));
    s.apply(UiEvent::Reply { ticket: Ticket(1), result: Err(ApiError::new(ApiCode::Internal)) });
    assert!(s.fit(), "a refused SetScale does not desync the shell");
}

#[test]
fn quality_and_view_only_are_listed_only_when_the_shell_offers_them_and_revert_on_refusal() {
    let mut s = viewer();
    let ids = |s: &UiState| s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert!(
        !ids(&s).contains(&ControlId::TbQuality) && !ids(&s).contains(&ControlId::TbViewOnly),
        "the Windows toolbar has neither"
    );
    s.set_viewer_extras(true);
    assert!(ids(&s).contains(&ControlId::TbQuality) && ids(&s).contains(&ControlId::TbViewOnly));

    let a = s.activate(ControlId::TbViewOnly);
    assert_eq!(a.commands, vec![Command::SetViewOnly { session_id: 1, on: true }]);
    s.track(Ticket(1), &a.commands[0]);
    assert!(s.view_only());
    s.apply(UiEvent::Reply { ticket: Ticket(1), result: Err(ApiError::new(ApiCode::NotPermitted)) });
    assert!(!s.view_only());

    let a = s.choose_quality(Quality::Motion);
    assert_eq!(a.commands, vec![Command::SetQuality { session_id: 1, quality: Quality::Motion }]);
    s.track(Ticket(2), &a.commands[0]);
    assert_eq!(s.quality(), Quality::Motion);
    let spec = s.controls(WindowKind::Viewer).into_iter().find(|c| c.id == ControlId::TbQuality).unwrap();
    assert_eq!(spec.value.as_deref(), Some("Movimento fluido"));
    s.apply(UiEvent::Reply { ticket: Ticket(2), result: Err(ApiError::new(ApiCode::Internal)) });
    assert_eq!(s.quality(), Quality::Auto);
}

#[test]
fn a_display_listing_belongs_to_the_session_its_command_named() {
    let mut s = base();
    s.apply(session_event(
        1,
        MAC,
        SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE,
            elevation: Default::default(),
        },
    ));
    s.apply(session_event(2, AIR, SessionState::Connecting));
    s.track(Ticket(7), &Command::ListDisplays { session_id: 2 });
    s.apply(UiEvent::Reply { ticket: Ticket(7), result: Ok(Reply::Displays(vec![disp(4, false), disp(5, true)])) });
    assert_eq!(s.sessions()[0].display_count(), 0, "the first session is not the one asked");
    assert_eq!(s.sessions()[1].display_count(), 2);
    assert_eq!(s.sessions()[1].display, Some(5), "the primary is the current one");
    // A reply nobody asked for is dropped.
    s.apply(reply_ok(Reply::Displays(vec![disp(1, true)])));
    assert_eq!(s.sessions()[0].display_count(), 0);
    // The event names its session and keeps a valid current display.
    s.apply(UiEvent::Displays { session_id: 2, displays: vec![disp(4, true)] });
    assert_eq!(s.sessions()[1].display, Some(4));
}

#[test]
fn a_viewer_session_coming_up_sends_the_audio_switch_once() {
    let mut s = base();
    s.apply(session_event(3, MAC, SessionState::Connecting));
    assert!(s.take_followups().is_empty(), "nothing before the session is up");
    let active = SessionState::Active {
        since_ms: 0,
        role: Role::Viewer,
        granted: Permissions::NONE,
        elevation: Default::default(),
    };
    s.apply(session_event(3, MAC, active.clone()));
    assert_eq!(s.take_followups(), vec![Command::SetAudio { session_id: 3, on: true }]);
    s.apply(session_event(3, MAC, active));
    assert!(s.take_followups().is_empty(), "an update of a live session sends nothing");
    let host = SessionState::Active {
        since_ms: 0,
        role: Role::Host,
        granted: Permissions::NONE,
        elevation: Default::default(),
    };
    s.apply(session_event(4, MAC, host));
    assert!(s.take_followups().is_empty(), "the host side has no audio switch");
}

#[test]
fn the_display_listing_is_asked_once_per_viewer_session() {
    let mut s = base();
    assert!(s.take_display_request().is_none());
    s.apply(session_event(3, MAC, SessionState::Connecting));
    assert!(s.take_display_request().is_none(), "not before the session is up");
    s.apply(session_event(
        3,
        MAC,
        SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE,
            elevation: Default::default(),
        },
    ));
    assert_eq!(s.take_display_request(), Some(Command::ListDisplays { session_id: 3 }));
    assert!(s.take_display_request().is_none());
}

fn active(role: Role, granted: Permissions) -> SessionState {
    SessionState::Active { since_ms: 0, role, granted, elevation: Default::default() }
}

#[test]
fn the_first_active_state_of_a_session_asks_to_minimize_the_main_window_once() {
    for role in [Role::Viewer, Role::Host] {
        let mut s = base();
        assert!(!s.take_minimize_request());
        s.apply(session_event(3, MAC, SessionState::Connecting));
        assert!(!s.take_minimize_request(), "not before the session is up ({role:?})");
        s.apply(session_event(3, MAC, active(role, Permissions::NONE)));
        assert!(s.take_minimize_request(), "asked at the first Active state ({role:?})");
        assert!(!s.take_minimize_request(), "asked once ({role:?})");
    }
}

#[test]
fn a_reconnect_or_a_grant_change_does_not_ask_again() {
    let mut s = base();
    s.apply(session_event(3, MAC, active(Role::Viewer, Permissions::NONE)));
    assert!(s.take_minimize_request());
    s.apply(session_event(3, MAC, SessionState::Paused { desktop: DesktopKind::Locked }));
    assert!(!s.take_minimize_request());
    s.apply(session_event(3, MAC, active(Role::Viewer, Permissions::NONE)));
    assert!(!s.take_minimize_request(), "active again after a pause");
    s.apply(session_event(3, MAC, SessionState::Reconnecting { attempt: 1 }));
    s.apply(session_event(3, MAC, active(Role::Viewer, Permissions::NONE)));
    assert!(!s.take_minimize_request(), "active again after a reconnect");
    let more = Permissions::NONE.with(Permission::Control);
    s.apply(session_event(3, MAC, active(Role::Viewer, more)));
    assert!(!s.take_minimize_request(), "a change of granted permissions");
}

#[test]
fn a_new_session_asks_again() {
    let mut s = base();
    s.apply(session_event(3, MAC, active(Role::Viewer, Permissions::NONE)));
    assert!(s.take_minimize_request());
    s.apply(session_event(3, MAC, SessionState::Ended { reason: None }));
    s.apply(session_event(4, MAC, active(Role::Host, Permissions::NONE)));
    assert!(s.take_minimize_request(), "the session a swap creates is a new one");
    assert!(!s.take_minimize_request());
}

/// Owner, 11/11: the main window minimized at a session's start comes back when that session ends, on the computer
/// that controlled; on the computer that was controlled it stays minimized.
#[test]
fn the_end_of_a_session_this_computer_controlled_asks_once_to_restore_the_window() {
    let mut s = base();
    s.apply(session_event(3, MAC, active(Role::Viewer, Permissions::NONE)));
    assert!(s.take_minimize_request());
    assert!(!s.take_restore_request(), "not while the session goes on");
    s.apply(session_event(3, MAC, SessionState::Paused { desktop: DesktopKind::Locked }));
    assert!(!s.take_restore_request(), "a pause is no end");
    s.apply(session_event(3, MAC, SessionState::Ended { reason: None }));
    assert!(s.take_restore_request(), "the session ended");
    assert!(!s.take_restore_request(), "asked once");
}

#[test]
fn the_controlled_computer_keeps_its_window_minimized_after_the_session() {
    let mut s = base();
    s.apply(session_event(3, MAC, active(Role::Host, Permissions::NONE)));
    assert!(s.take_minimize_request(), "it minimizes on both computers (D47)");
    s.apply(session_event(3, MAC, SessionState::Ended { reason: None }));
    assert!(!s.take_restore_request(), "the controlled computer stays minimized (D60)");
}

#[test]
fn the_window_comes_back_only_when_no_session_is_left_and_never_for_one_that_did_not_hide_it() {
    let mut s = base();
    s.apply(session_event(3, MAC, active(Role::Viewer, Permissions::NONE)));
    assert!(s.take_minimize_request());
    s.apply(session_event(4, AIR, active(Role::Viewer, Permissions::NONE)));
    assert!(s.take_minimize_request());
    s.apply(session_event(3, MAC, SessionState::Ended { reason: None }));
    assert!(!s.take_restore_request(), "the other session still goes on");
    s.apply(session_event(4, AIR, SessionState::Ended { reason: Some(ApiCode::Timeout) }));
    assert!(s.take_restore_request(), "the last one ended");
    // A session that never started minimized nothing, so its end restores nothing.
    s.apply(session_event(5, MAC, SessionState::Prompted { remaining_s: 30 }));
    s.apply(session_event(5, MAC, SessionState::Ended { reason: Some(ApiCode::Refused) }));
    assert!(!s.take_minimize_request() && !s.take_restore_request());
}

#[test]
fn a_session_that_never_starts_asks_nothing() {
    let mut s = base();
    s.apply(session_event(3, MAC, SessionState::Prompted { remaining_s: 30 }));
    s.apply(session_event(3, MAC, SessionState::Ended { reason: Some(ApiCode::Refused) }));
    s.apply(session_event(4, MAC, SessionState::Connecting));
    s.apply(session_event(4, MAC, SessionState::Ended { reason: Some(ApiCode::Timeout) }));
    assert!(!s.take_minimize_request());
}

#[test]
fn the_connect_flow_keeps_connecting_prompted_and_the_ended_reason_until_dismissed() {
    let mut s = base();
    assert!(s.connect_flow(MAC).is_none());
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 42 }));
    let f = s.connect_flow(MAC).unwrap();
    assert_eq!(f.phase, ConnectPhase::Prompted { remaining_s: 42 });
    assert_eq!(f.entry, ENTRY);
    assert!(s.connect_flow(AIR).is_none(), "the flow belongs to its device's page");
    assert!(s.viewer_session().is_none(), "the prompt opens no viewer window");
    s.tick();
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Prompted { remaining_s: 41 });

    s.apply(session_event(ENTRY, MAC, SessionState::Connecting));
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Connecting);

    s.apply(session_event(ENTRY, MAC, SessionState::NotOpened { why: ConnectWhy::NotPermitted }));
    let f = s.connect_flow(MAC).unwrap();
    assert_eq!(f.phase, ConnectPhase::Ended { why: ConnectWhy::NotPermitted });
    assert_eq!(f.entry, ENTRY);
    assert!(s.error().is_none(), "the card says it, not the strip");
    assert_eq!(s.dismiss_connect(), Changed::MAIN);
    assert!(s.connect_flow(MAC).is_none());
    assert_eq!(s.dismiss_connect(), Changed::NONE);

    // A normal end of an entry says nothing.
    s.apply(session_event(ENTRY + 1, MAC, SessionState::Ended { reason: None }));
    assert!(s.connect_flow(MAC).is_none());
    // A new attempt replaces an old ending.
    s.apply(session_event(ENTRY + 2, MAC, SessionState::NotOpened { why: ConnectWhy::Busy }));
    s.apply(session_event(ENTRY + 3, MAC, SessionState::Connecting));
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Connecting);
    s.apply(session_event(ENTRY + 3, MAC, SessionState::Ended { reason: None }));
    assert!(s.connect_flow(MAC).is_none(), "the old ending went with the new attempt");
}

fn all_whys() -> [ConnectWhy; 8] {
    [
        ConnectWhy::Declined,
        ConnectWhy::TimedOut,
        ConnectWhy::NobodyPresent,
        ConnectWhy::Busy,
        ConnectWhy::Forgotten,
        ConnectWhy::NotPermitted,
        ConnectWhy::Unreachable,
        ConnectWhy::Failed,
    ]
}

fn mac_name(s: &UiState) -> String {
    s.device_by_id(MAC).unwrap().name.clone()
}

/// The service answers the click's `Connect` with the entry's id.
fn reply_entry(s: &mut UiState, a: &Activation) {
    s.track(Ticket(70), &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: Ticket(70), result: Ok(Reply::SessionId(ENTRY)) });
}

/// A state where `Controlar` was pressed on the Mac and the service answered with `ENTRY`.
fn connecting() -> UiState {
    let mut s = base();
    let a = s.activate(ControlId::Control);
    s.track(Ticket(40), &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: Ticket(40), result: Ok(Reply::SessionId(ENTRY)) });
    s
}

#[test]
fn controlar_shows_the_connect_card_at_once_and_sends_one_connect() {
    let mut s = base();
    let a = s.activate(ControlId::Control);
    // In the frame of the click, before any reply: one Connect, the card, and the focus on Cancelar.
    assert_eq!(
        a.commands,
        vec![Command::Connect { device_id: MAC.into(), intent: Intent::Control, unattended: false }]
    );
    assert_eq!(a.effect, Some(Effect::Focus(ControlId::ConnectCancel)));
    assert!(a.changed.hits(WindowKind::Main));
    let card = s.wait_card(MAC).expect("the card shows before the reply");
    assert_eq!(card.phase, WaitPhase::Connecting);
    assert_eq!(card.icon, WaitIcon::Clock);
    assert_eq!(card.title, format!("Conectando ao {}", mac_name(&s)));
    assert_eq!(card.body, "Abrindo a conexão pela rede local.");
    assert_eq!(card.bar, Some(WaitBar { fraction: 0.0, text: None }), "the track alone while there is no deadline");
    assert_eq!(
        card.buttons,
        vec![WaitButton {
            id: ControlId::ConnectCancel,
            label: "Cancelar".into(),
            side: ButtonSide::Left,
            primary: false
        }]
    );
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Connecting);
    assert_eq!(s.connect_flow(MAC).unwrap().entry, 0, "no id until the reply");
    assert_eq!(s.windows(), vec![WindowKind::Main]);

    // The page: only the card's controls, none of the buttons, tabs or the rename the card replaces.
    let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::ConnectCancel));
    for gone in [ControlId::Control, ControlId::ViewOnly, ControlId::Wake, ControlId::RenameDevice] {
        assert!(!ids.contains(&gone), "{gone:?} stays off while the card shows");
    }
    assert!(!ids.iter().any(|i| matches!(i, ControlId::Tab(_))));
    let d = s.device_by_id(MAC).unwrap().clone();
    assert!(!s.rename_available(&d));

    // The reply gives the entry its id.
    s.track(Ticket(41), &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: Ticket(41), result: Ok(Reply::SessionId(ENTRY)) });
    assert_eq!(s.connect_flow(MAC).unwrap().entry, ENTRY);
    assert_eq!(s.sessions().len(), 1);
}

#[test]
fn a_second_press_while_waiting_sends_nothing() {
    let mut s = base();
    let first = s.activate(ControlId::Control);
    assert_eq!(first.commands.len(), 1);
    s.track(Ticket(71), &first.commands[0]);
    for id in [ControlId::Control, ControlId::ViewOnly] {
        let again = s.activate(id);
        assert!(again.commands.is_empty(), "{id:?} while the first connect waits");
    }
    s.apply(UiEvent::Reply { ticket: Ticket(71), result: Ok(Reply::SessionId(ENTRY)) });
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 20 }));
    assert!(s.activate(ControlId::Control).commands.is_empty());
    assert_eq!(s.sessions().len(), 1, "one entry, not two");
}

#[test]
fn the_entry_counts_the_hosts_seconds_down() {
    let mut s = connecting();
    assert!(s.needs_tick(), "the interface's own give-up counts from the start");
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 20 }));
    let card = s.wait_card(MAC).unwrap();
    assert_eq!(card.phase, WaitPhase::Prompted);
    assert_eq!(card.title, format!("Esperando o {} aceitar", mac_name(&s)));
    assert!(card.body.starts_with(&format!("O {} está mostrando o seu pedido.", mac_name(&s))), "{}", card.body);
    assert_eq!(card.bar, Some(WaitBar { fraction: 0.0, text: Some("0:20".into()) }));
    for _ in 0..6 {
        s.tick();
    }
    let bar = s.wait_card(MAC).unwrap().bar.unwrap();
    assert_eq!(bar.text.as_deref(), Some("0:14"));
    assert!((bar.fraction - 0.3).abs() < 1e-6, "{}", bar.fraction);
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Prompted { remaining_s: 14 });
    for _ in 0..14 {
        s.tick();
    }
    let bar = s.wait_card(MAC).unwrap().bar.unwrap();
    assert_eq!((bar.text.as_deref(), bar.fraction), (Some("0:00"), 1.0), "it stops at zero and waits for the service");
}

#[test]
fn no_viewer_window_opens_while_the_entry_waits() {
    let mut s = connecting();
    assert_eq!(s.windows(), vec![WindowKind::Main]);
    s.apply(session_event(ENTRY, MAC, SessionState::Connecting));
    assert!(s.viewer_session().is_none());
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 20 }));
    assert!(s.viewer_session().is_none());
    assert_eq!(s.windows(), vec![WindowKind::Main]);
    assert!(!s.take_minimize_request());
    assert!(s.take_display_request().is_none());
}

#[test]
fn an_accepted_connect_opens_the_viewer_and_the_card_goes() {
    let mut s = connecting();
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 12 }));
    // The session's own Active comes first, under its own id, then the entry ends.
    s.apply(session_event(9, MAC, active(Role::Viewer, Permissions::NONE.with(Permission::Control))));
    assert_eq!(s.windows(), vec![WindowKind::Main, WindowKind::Viewer], "the window opens on Active");
    assert_eq!(s.viewer_session().unwrap().id, 9);
    assert!(s.wait_card(MAC).is_some(), "the card is still there until the entry ends");
    s.apply(session_event(ENTRY, MAC, SessionState::Ended { reason: None }));
    assert!(s.wait_card(MAC).is_none() && s.connect_flow(MAC).is_none());
    assert!(s.error().is_none());
    assert_eq!(s.windows(), vec![WindowKind::Main, WindowKind::Viewer]);
    assert_eq!(s.sessions().len(), 1);
    let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(!ids.contains(&ControlId::ConnectCancel));
}

#[test]
fn each_why_reads_its_own_line() {
    let mut lines = Vec::new();
    for why in all_whys() {
        let mut s = connecting();
        s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 9 }));
        s.apply(session_event(ENTRY, MAC, SessionState::NotOpened { why }));
        assert!(s.sessions().is_empty(), "the entry went ({why:?})");
        assert!(s.error().is_none(), "no strip over the card ({why:?})");
        assert!(!s.needs_tick(), "nothing counts once it ended ({why:?})");
        let card = s.wait_card(MAC).unwrap_or_else(|| panic!("an end card for {why:?}"));
        assert_eq!((card.phase, card.icon, card.bar.is_none()), (WaitPhase::Ended, WaitIcon::ShieldAlert, true));
        assert_eq!(card.title, format!("Não deu para conectar ao {}", mac_name(&s)));
        assert_eq!(
            card.buttons,
            vec![
                WaitButton {
                    id: ControlId::ConnectClose,
                    label: "Fechar".into(),
                    side: ButtonSide::Left,
                    primary: false
                },
                WaitButton {
                    id: ControlId::ConnectRetry,
                    label: "Tentar de novo".into(),
                    side: ButtonSide::Right,
                    primary: true
                },
            ]
        );
        assert_eq!(
            s.take_focus_request(),
            Some(ControlId::ConnectClose),
            "focus goes to Fechar, not to Tentar de novo"
        );
        assert_eq!(s.take_focus_request(), None);
        lines.push(card.body);
    }
    let name = |t: &str| t.replace("{0}", &mac_name(&connecting()));
    assert_eq!(lines[0], name("O {0} recusou."));
    assert_eq!(lines[1], name("O {0} não respondeu em 20 segundos."));
    assert_eq!(lines[2], name("Não há ninguém no {0} agora."));
    assert_eq!(lines[3], name("O {0} já está em outra sessão. Tente de novo quando ela terminar."));
    assert_eq!(lines[4], name("O {0} não reconhece mais este computador. Pareie os dois de novo."));
    assert_eq!(lines[5], name("O {0} não permite esta conexão."));
    assert_eq!(lines[6], name("O {0} não respondeu pela rede. Confira se ele está ligado e nesta mesma rede."));
    assert_eq!(lines[7], "Erro interno.");
    let mut sorted = lines.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 8, "every line differs: {lines:?}");
}

#[test]
fn cancel_sends_disconnect_for_the_entry() {
    let mut s = connecting();
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 15 }));
    let a = s.activate(ControlId::ConnectCancel);
    assert_eq!(a.commands, vec![Command::Disconnect { session_id: ENTRY }]);
    assert_eq!(a.effect, Some(Effect::Focus(ControlId::Control)), "focus goes back to the button that started it");
    // The service ends the entry normally: the page is the normal page at once, with no card.
    s.apply(session_event(ENTRY, MAC, SessionState::Ended { reason: None }));
    assert!(s.wait_card(MAC).is_none() && s.error().is_none());
    assert!(s.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::Control));

    // A view-only connect gives focus back to Assistir.
    let mut s = base();
    let a = s.activate(ControlId::ViewOnly);
    reply_entry(&mut s, &a);
    assert_eq!(s.activate(ControlId::ConnectCancel).effect, Some(Effect::Focus(ControlId::ViewOnly)));
}

#[test]
fn a_cancel_before_the_reply_is_sent_when_the_id_arrives() {
    let mut s = base();
    let a = s.activate(ControlId::Control);
    s.track(Ticket(50), &a.commands[0]);
    let c = s.activate(ControlId::ConnectCancel);
    assert!(c.commands.is_empty(), "there is no id to cancel yet");
    s.apply(UiEvent::Reply { ticket: Ticket(50), result: Ok(Reply::SessionId(ENTRY)) });
    assert_eq!(s.take_followups(), vec![Command::Disconnect { session_id: ENTRY }]);
}

#[test]
fn escape_cancels_the_wait_and_closes_the_end_card() {
    let mut s = base();
    assert_eq!(s.escape_target(WindowKind::Main), None);
    s.activate(ControlId::Control);
    assert_eq!(s.escape_target(WindowKind::Main), Some(ControlId::ConnectCancel));
    assert_eq!(s.escape_target(WindowKind::Viewer), None, "the main window only");
    s.apply(session_event(ENTRY, MAC, SessionState::NotOpened { why: ConnectWhy::Declined }));
    assert_eq!(s.escape_target(WindowKind::Main), Some(ControlId::ConnectClose));
    // The ended card closes, and the page is whole again.
    let a = s.activate(ControlId::ConnectClose);
    assert!(a.commands.is_empty());
    assert_eq!(a.effect, Some(Effect::Focus(ControlId::Control)));
    assert!(s.wait_card(MAC).is_none());
    assert_eq!(s.escape_target(WindowKind::Main), None);
    // A dialog over the window takes Esc itself.
    let mut s = Scenario::PedidoConexao.prepared_state();
    s.apply(session_event(ENTRY, MAC, SessionState::Connecting));
    assert_eq!(s.escape_target(WindowKind::Main), None);
}

#[test]
fn the_interface_gives_up_after_60_s_without_news() {
    let mut s = connecting();
    s.apply(session_event(ENTRY, MAC, SessionState::Connecting));
    for _ in 0..59 {
        s.tick();
    }
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Connecting);
    assert!(s.take_followups().is_empty());
    s.tick();
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Ended { why: ConnectWhy::Failed });
    assert!(s.sessions().is_empty());
    assert_eq!(s.wait_card(MAC).unwrap().phase, WaitPhase::Ended);
    assert_eq!(s.take_followups(), vec![Command::Disconnect { session_id: ENTRY }], "a silent service is told to stop");
    assert!(s.error().is_none());
    assert!(!s.needs_tick());

    // The seconds of a prompt count as waiting too.
    let mut s = connecting();
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 20 }));
    for _ in 0..60 {
        s.tick();
    }
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Ended { why: ConnectWhy::Failed });
}

#[test]
fn a_connect_the_service_does_not_answer_in_5_s_ends_the_card() {
    let mut s = base();
    let a = s.activate(ControlId::Control);
    s.track(Ticket(60), &a.commands[0]);
    for _ in 0..4 {
        s.tick();
    }
    assert_eq!(s.wait_card(MAC).unwrap().phase, WaitPhase::Connecting);
    s.tick();
    assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Ended { why: ConnectWhy::Failed });
    assert!(s.sessions().is_empty() && s.error().is_none());
}

#[test]
fn an_early_refusal_of_the_connect_ends_the_card_with_its_why() {
    for (code, why) in [
        (ApiCode::NotFound, ConnectWhy::Unreachable),
        (ApiCode::Internal, ConnectWhy::Failed),
        (ApiCode::ServiceDown, ConnectWhy::Failed),
        (ApiCode::NotPermitted, ConnectWhy::Failed),
    ] {
        let mut s = base();
        let a = s.activate(ControlId::Control);
        tracked_error(&mut s, &a.commands[0], code);
        assert!(s.sessions().is_empty(), "{code:?}");
        assert_eq!(s.connect_flow(MAC).unwrap().phase, ConnectPhase::Ended { why }, "{code:?}");
        assert!(s.error().is_none(), "the card says it ({code:?})");
    }
    // ACCOUNTS.md 9: entering without a prompt never asks for an administrator on this machine (the elevation step
    // comes later, inside the session).
    let mut s = base();
    let cmd = Command::Connect { device_id: MAC.into(), intent: Intent::Control, unattended: true };
    tracked_error(&mut s, &cmd, ApiCode::NeedsAdmin);
    assert!(s.elevation().is_none());
}

#[test]
fn tentar_de_novo_sends_the_same_intent_again() {
    for (button, intent) in [(ControlId::ViewOnly, Intent::View), (ControlId::Control, Intent::Control)] {
        let mut s = base();
        let a = s.activate(button);
        reply_entry(&mut s, &a);
        s.apply(session_event(ENTRY, MAC, SessionState::NotOpened { why: ConnectWhy::TimedOut }));
        let a = s.activate(ControlId::ConnectRetry);
        assert_eq!(a.commands, vec![Command::Connect { device_id: MAC.into(), intent, unattended: false }]);
        assert_eq!(a.effect, Some(Effect::Focus(ControlId::ConnectCancel)));
        assert_eq!(s.wait_card(MAC).unwrap().phase, WaitPhase::Connecting, "the card is back at once");
        assert!(s.activate(ControlId::ConnectRetry).commands.is_empty(), "nothing to retry while it waits");
    }
}

#[test]
fn the_end_card_stays_until_closed_retried_or_another_device_is_picked() {
    let mut s = base();
    s.activate(ControlId::Control);
    s.apply(session_event(ENTRY, MAC, SessionState::NotOpened { why: ConnectWhy::Busy }));
    s.tick();
    assert!(s.wait_card(MAC).is_some(), "time does not close it");
    s.select_device(DESKTOP);
    assert!(s.wait_card(MAC).is_none(), "another device's page drops it");
    s.select_device(MAC);
    assert!(s.wait_card(MAC).is_none());
}

#[test]
fn the_sidebar_row_says_connecting_and_waiting() {
    let mut s = base();
    let online = s.device_status_line(s.device_by_id(MAC).unwrap());
    assert!(online.starts_with("Online"), "{online}");
    let row = |s: &UiState| {
        s.controls(WindowKind::Main).into_iter().find(|c| c.id == ControlId::Device(0)).map(|c| c.label).unwrap()
    };
    assert_eq!(s.devices()[0].id, MAC);
    s.activate(ControlId::Control);
    let status = |s: &UiState| s.device_status_line(s.device_by_id(MAC).unwrap());
    assert_eq!(status(&s), "Conectando…");
    assert!(row(&s).ends_with(", Conectando…"), "{}", row(&s));
    s.apply(session_event(ENTRY, MAC, SessionState::Prompted { remaining_s: 20 }));
    assert_eq!(status(&s), "Esperando aceitar");
    assert!(row(&s).ends_with(", Esperando aceitar"), "{}", row(&s));
    // Other devices keep their own line.
    assert_eq!(s.device_status_line(s.device_by_id(DESKTOP).unwrap()).split(',').next(), Some("Dormindo"));
    s.apply(session_event(ENTRY, MAC, SessionState::NotOpened { why: ConnectWhy::Declined }));
    assert_eq!(status(&s), online);
    // Waking says so too.
    s.apply(session_event(ENTRY + 1, DESKTOP, SessionState::Waking { remaining_s: 60 }));
    assert_eq!(s.device_status_line(s.device_by_id(DESKTOP).unwrap()), "Acordando…");
}

#[test]
fn a_waking_card_fills_its_bar_against_the_minute_a_wake_lasts_whatever_value_it_saw_first() {
    // The scenario's first news of the wake is "36 s left": the bar is 24 of 60 full, not empty.
    let state = crate::mock::Scenario::EstadoAcordando.prepared_state();
    let card = state.selected_wait_card().expect("the waking card");
    let bar = card.bar.expect("a wake counts down");
    assert_eq!(bar.text.as_deref(), Some("0:36"));
    assert!((bar.fraction - 0.4).abs() < 0.01, "{}", bar.fraction);
}

#[test]
fn a_wakes_connect_phase_reads_the_entrys_end_the_new_way() {
    let mut s = base();
    s.select_device(DESKTOP);
    s.apply(session_event(ENTRY, DESKTOP, SessionState::Waking { remaining_s: 60 }));
    s.apply(session_event(ENTRY, DESKTOP, SessionState::Waking { remaining_s: 41 }));
    let card = s.wait_card(DESKTOP).unwrap();
    assert_eq!((card.phase, card.icon), (WaitPhase::Waking, WaitIcon::Power));
    assert_eq!(card.title, "Acordando o Desktop do escritório");
    assert_eq!(card.body, s.t(MsgKey::WakingBody));
    let bar = card.bar.unwrap();
    assert_eq!(bar.text.as_deref(), Some("0:41"), "the waking card has the countdown row too");
    assert!((bar.fraction - 19.0 / 60.0).abs() < 1e-6);
    assert_eq!(card.buttons.len(), 1);
    assert_eq!(card.buttons[0].id, ControlId::WakeCancel);
    // The device appeared: the same entry connects, then waits for the person, then ends with its reason.
    s.apply(session_event(ENTRY, DESKTOP, SessionState::Connecting));
    let card = s.wait_card(DESKTOP).unwrap();
    assert_eq!(card.phase, WaitPhase::Connecting);
    assert_eq!(card.buttons[0].id, ControlId::ConnectCancel);
    s.apply(session_event(ENTRY, DESKTOP, SessionState::Prompted { remaining_s: 20 }));
    assert_eq!(s.wait_card(DESKTOP).unwrap().bar.unwrap().text.as_deref(), Some("0:20"), "the bar restarts from 20 s");
    s.apply(session_event(ENTRY, DESKTOP, SessionState::NotOpened { why: ConnectWhy::NobodyPresent }));
    assert!(s.waking_session().is_none() && s.error().is_none());
    let card = s.wait_card(DESKTOP).unwrap();
    assert_eq!(card.phase, WaitPhase::Ended);
    assert_eq!(card.body, "Não há ninguém no Desktop do escritório agora.");
    // The entry's cancel sends Disconnect for the entry in any phase.
    let mut s = base();
    s.select_device(DESKTOP);
    s.apply(session_event(ENTRY, DESKTOP, SessionState::Connecting));
    assert_eq!(s.activate(ControlId::ConnectCancel).commands, vec![Command::Disconnect { session_id: ENTRY }]);
}

#[test]
fn the_send_keys_names_follow_the_peer_system() {
    let mut s = viewer();
    let names = |s: &UiState| s.special_keys().into_iter().map(|(_, n)| n).collect::<Vec<_>>();
    let mac_peer = s.device_by_id(&s.viewer_session().unwrap().device_id).unwrap().os == Os::MacOs;
    let expected: [&str; 5] = if mac_peer {
        ["Ctrl+Alt+Del", "Cmd+Tab", "Cmd", "Cmd+Q", "Print Screen"]
    } else {
        ["Ctrl+Alt+Del", "Alt+Tab", "Windows", "Alt+F4", "Print Screen"]
    };
    assert_eq!(names(&s), expected);
    let a = s.send_keys(SpecialKeys::PrintScreen);
    assert_eq!(a.commands, vec![Command::SendKeys { session_id: 1, keys: SpecialKeys::PrintScreen }]);
}

#[test]
fn a_quick_action_the_host_did_not_run_says_where_and_a_refused_sync_toggle_reverts() {
    let mut s = viewer();
    let a = s.activate(ControlId::Action(ActionKind::Lock));
    let Command::RunAction { session_id, .. } = a.commands[0].clone() else { panic!("not a RunAction") };
    tracked_error(&mut s, &a.commands[0], ApiCode::NotPermitted);
    let e = s.error().expect("a notice").clone();
    assert_eq!(e.code, ApiCode::NotPermitted);
    let detail = e.detail.as_ref().expect("the detail names the computer");
    assert_eq!(detail.key, crate::strings::MsgKey::ActionFailed);
    assert_eq!(detail.args, vec![s.session_peer_name(session_id)]);
    assert!(s.error_text(&e).ends_with('.'));

    let a = s.activate(ControlId::ClipToggle);
    assert!(!s.clipboard_on());
    tracked_error(&mut s, &a.commands[0], ApiCode::NotFound);
    assert!(s.clipboard_on(), "a refused SetClipboardSync does not desync the switch");
}

// ---------- the terminal drawer (stage 43) ----------

fn open_terminal(s: &mut UiState) -> Vec<Command> {
    s.activate(ControlId::TbTerminal).commands
}

/// Sends the commands the way the shell does (tracked), and answers the open like the service: the id, then the account.
fn answer_open(s: &mut UiState, cmds: &[Command], term_id: u32) {
    let t = Ticket(9000 + u64::from(term_id));
    s.track(t, &cmds[0]);
    s.apply(UiEvent::Reply { ticket: t, result: Ok(Reply::TermId(term_id)) });
    s.apply(UiEvent::Term { term_id, event: TermEvent::Opened { account: "alex".into() } });
}

#[test]
fn the_terminal_is_asked_for_when_the_panel_first_opens_and_goes_through_its_phases() {
    let mut s = viewer();
    assert_eq!(*s.term_phase(), TermPhase::Idle, "R-UI-11: nothing built before the panel opens");
    let cmds = open_terminal(&mut s);
    assert!(matches!(cmds[..], [Command::TermOpen { session_id: 1, admin: false, cols: 80, rows: 24 }]));
    assert_eq!(*s.term_phase(), TermPhase::Opening { admin: false });
    assert_eq!(s.term_id(), None, "the id comes with the reply");
    answer_open(&mut s, &cmds, 7);
    assert_eq!(*s.term_phase(), TermPhase::Open { admin: false });
    assert_eq!((s.term_id(), s.term_account()), (Some(7), Some("alex")));
    // The grid is a control while a shell runs, so the keyboard can reach it.
    let ids: Vec<_> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::TermGrid));
    assert!(!ids.contains(&ControlId::TermReopen) && !ids.contains(&ControlId::TermAdmin));
    // Closing and reopening the panel keeps the same shell.
    s.activate(ControlId::DrawerClose);
    assert!(open_terminal(&mut s).is_empty());
    assert_eq!(s.term_id(), Some(7));
}

#[test]
fn a_shell_that_ends_says_so_and_offers_to_open_again() {
    let mut s = viewer();
    let cmds = open_terminal(&mut s);
    answer_open(&mut s, &cmds, 7);
    // The end of an older terminal changes nothing.
    s.apply(UiEvent::Term { term_id: 3, event: TermEvent::Exited { code: Some(1) } });
    assert_eq!(*s.term_phase(), TermPhase::Open { admin: false });
    s.apply(UiEvent::Term { term_id: 7, event: TermEvent::Exited { code: Some(3) } });
    assert_eq!(*s.term_phase(), TermPhase::Exited { code: Some(3), admin: false });
    let ids: Vec<_> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::TermReopen) && !ids.contains(&ControlId::TermGrid));
    // The panel does not open another by itself when shown again; the button does.
    s.activate(ControlId::DrawerClose);
    assert!(open_terminal(&mut s).is_empty());
    let a = s.activate(ControlId::TermReopen);
    assert!(matches!(a.commands[..], [Command::TermOpen { admin: false, .. }]));
    assert_eq!(*s.term_phase(), TermPhase::Opening { admin: false });
    assert_eq!(s.term_account(), None);
}

#[test]
fn a_refused_open_shows_its_reason_in_the_panel_and_the_button_tries_again() {
    for (code, key) in [
        (ApiCode::Refused, MsgKey::TermFailNobody),
        (ApiCode::Timeout, MsgKey::TermFailTimedOut),
        (ApiCode::NotPermitted, MsgKey::TermFailNotPermitted),
    ] {
        let mut s = viewer();
        let cmds = open_terminal(&mut s);
        let t = Ticket(9100);
        s.track(t, &cmds[0]);
        let detail = (code == ApiCode::Refused).then(|| Msg::new(key));
        s.apply(UiEvent::Reply { ticket: t, result: Err(ApiError { code, detail }) });
        assert_eq!(*s.term_phase(), TermPhase::Failed { why: Msg::new(key), admin: false }, "{code:?}");
        assert!(s.error().is_none(), "the panel says it in place, not the strip over the viewer");
        let ids: Vec<_> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
        assert!(ids.contains(&ControlId::TermReopen));
        // Showing the panel again (after closing it) asks once more, since nothing is running.
        s.activate(ControlId::DrawerClose);
        assert!(matches!(open_terminal(&mut s)[..], [Command::TermOpen { .. }]));
    }
}

#[test]
fn the_administrator_terminal_is_offered_only_with_its_grant_and_is_labelled() {
    let mut s = viewer();
    assert!(!s.admin_terminal_offered(), "the session holds no ADMIN_TERMINAL");
    let granted = Permissions::NONE.with(Permission::View).with(Permission::Control).with(Permission::AdminTerminal);
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Active { since_ms: 1, role: Role::Viewer, granted, elevation: Default::default() },
    });
    assert!(s.admin_terminal_offered());
    let cmds = open_terminal(&mut s);
    answer_open(&mut s, &cmds, 7);
    let ids: Vec<_> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::TermAdmin), "offered next to the user's shell");
    // Opening it closes the user's shell first (one terminal per window) and is labelled as the administrator's.
    let a = s.activate(ControlId::TermAdmin);
    assert!(
        matches!(a.commands[..], [Command::TermClose { term_id: 7 }, Command::TermOpen { admin: true, .. }]),
        "{:?}",
        a.commands
    );
    assert!(s.term_is_admin());
    s.apply(UiEvent::Term { term_id: 8, event: TermEvent::Opened { account: "SYSTEM".into() } });
    assert_eq!(*s.term_phase(), TermPhase::Open { admin: true });
    let ids: Vec<_> = s.controls(WindowKind::Viewer).into_iter().map(|c| c.id).collect();
    assert!(!ids.contains(&ControlId::TermAdmin), "already the administrator's");
}

#[test]
fn an_administrator_open_from_a_plain_connection_leads_to_the_elevation_step_not_a_failure() {
    let mut s = viewer();
    let cmd = Command::TermOpen { session_id: 1, admin: true, cols: 80, rows: 24 };
    tracked_error(&mut s, &cmd, ApiCode::NeedsAdmin);
    assert!(matches!(s.elevation().unwrap().action, ElevatedAction::OpenAdminTerminal { session_id: 1 }));
    assert_eq!(*s.term_phase(), TermPhase::Idle, "nothing failed: the user is asked to elevate");
}

#[test]
fn the_end_of_the_session_resets_the_terminal() {
    let mut s = viewer();
    let cmds = open_terminal(&mut s);
    answer_open(&mut s, &cmds, 7);
    s.apply(UiEvent::Session { session_id: 1, device_id: MAC.into(), state: SessionState::Ended { reason: None } });
    assert_eq!((s.term_phase().clone(), s.term_id(), s.term_account()), (TermPhase::Idle, None, None));
}

#[test]
fn an_open_waits_up_to_25_seconds_for_the_hosts_user() {
    // D13: the host's user has 20 s to answer, and the shell then starts.
    assert_eq!(Command::TermOpen { session_id: 1, admin: false, cols: 80, rows: 24 }.timeout_s(), 25);
    assert_eq!(Command::TermWrite { term_id: 1, bytes: vec![] }.timeout_s(), 5);
}

// ---------- rename a paired device ----------

fn rename_ticket(s: &mut UiState, command: &Command) -> Ticket {
    let t = Ticket(4100);
    s.track(t, command);
    t
}

#[test]
fn rename_turns_the_name_into_a_field_and_saves_through_one_command() {
    let mut s = base();
    let ids = |s: &UiState| s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert!(ids(&s).contains(&ControlId::RenameDevice) && !ids(&s).contains(&ControlId::Edit(EditId::Rename)));

    let a = s.activate(ControlId::RenameDevice);
    assert!(a.commands.is_empty() && s.renaming().is_some());
    let spec = s.controls(WindowKind::Main).into_iter().find(|c| c.id == ControlId::Edit(EditId::Rename)).unwrap();
    assert_eq!(spec.value.as_deref(), Some("MacBook do escritório"), "prefilled with the current name");
    let ids = ids(&s);
    assert!(ids.contains(&ControlId::RenameSave) && ids.contains(&ControlId::RenameCancel));
    assert!(!ids.contains(&ControlId::RenameDevice));

    // "Salvar" asks the shell to read the field; the text is trimmed and sent once.
    assert_eq!(s.activate(ControlId::RenameSave).effect, Some(Effect::SubmitEdit(EditId::Rename)));
    let a = s.submit_edit(EditId::Rename, "  Mac da sala  ".into());
    let command = Command::RenameDevice { device_id: MAC.into(), name: "Mac da sala".into() };
    assert_eq!(a.commands, vec![command.clone()]);
    let t = rename_ticket(&mut s, &command);
    s.apply(UiEvent::Reply { ticket: t, result: Ok(Reply::Done) });
    assert_eq!(s.device_by_id(MAC).unwrap().name, "Mac da sala");
    assert!(s.renaming().is_none(), "the field closes when the service says done");
    assert!(s.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::RenameDevice));
}

#[test]
fn rename_refuses_bad_names_inline_and_cancel_or_leaving_closes_it() {
    let mut s = base();
    s.activate(ControlId::RenameDevice);
    let long = "n".repeat(65);
    for (text, key) in [
        ("   ", MsgKey::RenameEmpty),
        ("", MsgKey::RenameEmpty),
        (long.as_str(), MsgKey::RenameTooLong),
        ("a\u{7}b", MsgKey::RenameControlChars),
    ] {
        let a = s.submit_edit(EditId::Rename, text.into());
        assert!(a.commands.is_empty(), "{text:?}");
        let shown = s.renaming().unwrap().error.clone().expect("an inline error");
        assert_eq!(s.rename_error_text(&shown), s.t(key), "{text:?}");
        let texts: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.label).collect();
        assert!(texts.contains(&s.t(key)), "the error is on the page");
    }
    // The same name closes the field without a command.
    assert!(s.submit_edit(EditId::Rename, "MacBook do escritório".into()).commands.is_empty());
    assert!(s.renaming().is_none());

    // The service refuses: the field stays and says so inline, with no error strip.
    s.activate(ControlId::RenameDevice);
    let a = s.submit_edit(EditId::Rename, "Outro".into());
    let t = rename_ticket(&mut s, &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: t, result: Err(ApiError::new(ApiCode::Internal)) });
    assert!(s.error().is_none() && s.renaming().is_some_and(|r| r.error.is_some()));
    assert_eq!(s.device_by_id(MAC).unwrap().name, "MacBook do escritório");

    // Cancel, and leaving the device, close it.
    s.activate(ControlId::RenameCancel);
    assert!(s.renaming().is_none());
    s.activate(ControlId::RenameDevice);
    s.select_device(AIR);
    s.select_device(MAC);
    assert!(s.renaming().is_none());
    // A device that is not paired has no rename action.
    let unpaired = s.devices().iter().find(|d| !d.paired).map(|d| d.id.clone());
    if let Some(unpaired) = unpaired {
        s.select_device(&unpaired);
        assert!(!s.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::RenameDevice));
    }
}

// ---------- swap only toward the same OS ----------

#[test]
fn swap_is_offered_only_toward_a_machine_of_this_os() {
    // This mock machine is Windows. The banner scenario hosts for the Windows Desktop: offered.
    let mut s = Scenario::Banner.prepared_state();
    let has = |s: &UiState, w, id| s.controls(w).iter().any(|c| c.id == id);
    assert!(has(&s, WindowKind::Banner, ControlId::BannerSwap));
    // The same session with a Mac: offered too since 1.1 (F-XSWAP-01).
    s.apply(UiEvent::Session {
        session_id: 2,
        device_id: MAC.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Host,
            granted: Permissions::NONE.with(Permission::View),
            elevation: Default::default(),
        },
    });
    assert!(has(&s, WindowKind::Banner, ControlId::BannerSwap));
    assert!(
        has(&s, WindowKind::Banner, ControlId::BannerDisconnect) && has(&s, WindowKind::Banner, ControlId::BannerChat)
    );

    // The viewer toolbar: a Mac peer and a Windows peer both offer the swap (1.1, F-XSWAP-01).
    let v = viewer();
    assert!(has(&v, WindowKind::Viewer, ControlId::TbSwap) && has(&v, WindowKind::Viewer, ControlId::TbDisconnect));
    let mut v = viewer();
    v.apply(UiEvent::Session {
        session_id: 1,
        device_id: DESKTOP.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::View),
            elevation: Default::default(),
        },
    });
    assert!(has(&v, WindowKind::Viewer, ControlId::TbSwap));
}

// ---------- the banner's minute ----------

#[test]
fn the_banner_timer_is_due_at_the_next_minute_boundary_and_only_while_hosting() {
    let mut s = Scenario::Banner.prepared_state();
    let since = 5 * 60_000 + 1_000;
    s.apply(UiEvent::Session {
        session_id: 2,
        device_id: DESKTOP.into(),
        state: SessionState::Active {
            since_ms: since,
            role: Role::Host,
            granted: Permissions::NONE.with(Permission::View),
            elevation: Default::default(),
        },
    });
    assert_eq!(s.banner_minute_delay_ms(since), Some(60_000), "on the boundary the next one is a minute away");
    assert_eq!(s.banner_minute_delay_ms(since + 59_990), Some(10));
    assert_eq!(s.banner_minute_delay_ms(since + 61_000), Some(59_000));
    // The text moves with the clock: "1 min" until two minutes, then "2 min".
    let h = s.host_session().cloned().unwrap();
    let at = |s: &mut UiState, ms: u64| {
        s.set_clock(crate::format::Clock { now_ms: ms, utc_offset_min: 0 });
        s.banner_parts("PC", &h).1
    };
    assert!(at(&mut s, since + 30_000).ends_with("1 min"));
    assert!(at(&mut s, since + 125_000).ends_with("2 min"));
    // No hosted session, no timer.
    s.apply(UiEvent::Session { session_id: 2, device_id: DESKTOP.into(), state: SessionState::Ended { reason: None } });
    assert_eq!(s.banner_minute_delay_ms(since), None);
    assert_eq!(viewer().banner_minute_delay_ms(0), None, "a viewer session has no banner");
}

// ---------- D35: the home screen ----------

#[test]
fn the_window_starts_on_the_home_and_a_device_page_is_left_with_inicio() {
    let mut s = Scenario::Inicio.prepared_state();
    assert_eq!(s.screen(), Screen::Home);
    assert!(s.is_home());
    let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(!ids.contains(&ControlId::Control), "no device page behind the home");
    let i = s.devices().iter().position(|d| d.id == DESKTOP).unwrap() as u16;
    s.activate(ControlId::Device(i));
    assert_eq!(s.screen(), Screen::Devices);
    s.activate(ControlId::NavHome);
    assert_eq!(s.screen(), Screen::Home);
    s.activate(ControlId::NavSettings);
    assert_eq!(s.screen(), Screen::Settings);
    s.activate(ControlId::NavHome);
    assert_eq!(s.screen(), Screen::Home);
}

#[test]
fn the_home_shows_the_name_the_state_the_network_chip_and_the_two_cards() {
    let s = Scenario::Inicio.prepared_state();
    let h = s.home();
    assert_eq!(h.name.as_deref(), Some("Notebook da sala"));
    assert_eq!(h.status.as_deref(), Some("Pronto para receber"));
    assert!(h.ready);
    let chip = h.network.expect("chip");
    assert_eq!(chip.text, "Só na rede local: Wi-Fi Casa, 192.168.0.12");
    assert!(!chip.warn);
    assert_eq!(h.cards.len(), 2);
    let crate::home::HomeCard::Recent { lines, .. } = &h.cards[1] else { panic!("recent card") };
    assert_eq!(lines.len(), 5, "the latest five of six entries");
    assert_eq!(lines[0].who, "MacBook do escritório");
    // The same lines the Atividade screen lists, newest first.
    let rows: Vec<_> = s.controls(WindowKind::Main).into_iter().filter(|c| c.role == UiRole::Row).collect();
    assert_eq!(rows.len(), 5);
    assert!(rows[0].label.starts_with(&lines[0].when));
}

#[test]
fn the_network_chip_warns_on_a_public_network_and_the_recent_card_has_an_empty_line() {
    let s = Scenario::EstadoRedePublica.prepared_state();
    assert!(s.home().network.expect("chip").warn);
    let mut s = Scenario::Inicio.prepared_state();
    s.apply(UiEvent::Reply { ticket: Ticket(0), result: Ok(Reply::Activity(Vec::new())) });
    let c = s.controls(WindowKind::Main);
    assert!(c.iter().any(|c| c.label == "Ainda não aconteceu nada por aqui."));
    assert!(c.iter().any(|c| c.id == ControlId::HomeActivityAll), "Ver tudo stays");
    assert!(c.iter().all(|c| c.role != UiRole::Row));
}

#[test]
fn ver_tudo_opens_atividade_and_forgetting_the_last_device_page_falls_back_to_the_home() {
    let mut s = Scenario::Inicio.prepared_state();
    let a = s.activate(ControlId::HomeActivityAll);
    assert_eq!(s.screen(), Screen::Activity);
    assert!(a.commands.is_empty(), "the log is cached");
    s.activate(ControlId::NavHome);
    s.apply(UiEvent::DevicesChanged(Vec::new()));
    s.navigate(Nav::Devices);
    assert_eq!(s.screen(), Screen::Home, "a device page with no device is the home");
}

#[test]
fn the_sidebar_holds_only_navigation_and_the_device_lists() {
    let s = Scenario::Inicio.prepared_state();
    let side: Vec<_> =
        s.controls(WindowKind::Main).into_iter().filter(|c| c.region == crate::Region::Sidebar).collect();
    for c in &side {
        assert!(
            matches!(c.id, ControlId::NavHome | ControlId::NavSettings | ControlId::Device(_)),
            "{:?} does not belong in the sidebar",
            c.id
        );
    }
    assert_eq!(side.len(), 2 + s.devices().len());
}

// ---------- the per-user record (C5.1 ui.bin) ----------

use crate::prefs::{PrefsStore, UserPrefs};
use std::sync::{Arc, Mutex};

/// A store in memory that keeps what was written, to be handed to a second "process".
#[derive(Clone, Default)]
struct MemPrefs(Arc<Mutex<Option<Vec<u8>>>>);

impl PrefsStore for MemPrefs {
    fn read(&self) -> std::io::Result<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        *self.0.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}

#[test]
fn a_changed_theme_language_and_the_rest_are_saved_and_restored_at_start() {
    let mem = MemPrefs::default();
    let mut s = base();
    s.load_prefs(Box::new(mem.clone()));
    assert_eq!(mem.read().unwrap(), None, "loading writes nothing");
    s.choose(SelectId::Theme, 2);
    s.choose(SelectId::Language, 1);
    s.choose(SelectId::Quality, 2);
    s.choose(SelectId::Keyboard, 1);
    s.activate(ControlId::ShowStats);
    s.activate(ControlId::SwapCtrlCmd);
    s.set_release_key(crate::ReleaseKey::RightCtrl);
    let saved = UserPrefs::decode(&mem.read().unwrap().expect("written")).expect("a valid record");
    assert_eq!(saved.theme, Theme::Dark);
    assert_eq!(saved.language, Language::En);
    assert_eq!(saved.quality, Quality::Motion);
    assert_eq!(saved.keyboard_mode, KeyboardMode::Physical);
    assert_eq!(saved.release_key, crate::ReleaseKey::RightCtrl);
    assert_eq!(saved.show_stats, !Scenario::Dispositivos.prepared_state().settings().unwrap().show_stats);

    // A new process: the first frame (no settings yet) already has the chosen theme and language.
    let mut next = UiState::new(Language::PtBr, false);
    next.load_prefs(Box::new(mem.clone()));
    assert!(next.dark(), "Escuro from the first frame, on a light OS");
    assert_eq!(next.language(), Language::En);
    assert_eq!(next.theme(), Theme::Dark);
    // The service answers its defaults for the per-user fields; the UI's copy wins.
    let from_service = Settings {
        language: Language::PtBr,
        theme: Theme::System,
        quality: Quality::Auto,
        keyboard_mode: KeyboardMode::Auto,
        ..base().settings().unwrap().clone()
    };
    next.apply(UiEvent::SettingsChanged(from_service));
    let set = next.settings().unwrap();
    assert_eq!(
        (set.theme, set.language, set.quality, set.keyboard_mode),
        (Theme::Dark, Language::En, Quality::Motion, KeyboardMode::Physical)
    );
    assert_eq!(set.show_stats, saved.show_stats);
    assert_eq!(set.swap_ctrl_cmd, saved.swap_ctrl_cmd);
    assert_eq!(next.user_prefs(), saved);
    // Loading again writes nothing new: the file stays as it was.
    assert_eq!(UserPrefs::decode(&mem.read().unwrap().unwrap()), Ok(saved));
}

#[test]
fn a_missing_or_damaged_record_starts_with_the_defaults_and_a_refusal_undoes_the_saved_change() {
    let mem = MemPrefs::default();
    *mem.0.lock().unwrap() = Some(b"garbage".to_vec());
    let mut s = base();
    s.load_prefs(Box::new(mem.clone()));
    assert_eq!((s.theme(), s.language()), (Theme::System, Language::PtBr));
    let a = s.choose(SelectId::Theme, 2);
    assert_eq!(
        UserPrefs::decode(&mem.read().unwrap().unwrap()).unwrap().theme,
        Theme::Dark,
        "the bad file is replaced"
    );
    tracked_error(&mut s, &a.commands[0], ApiCode::Refused);
    assert_eq!(UserPrefs::decode(&mem.read().unwrap().unwrap()).unwrap().theme, Theme::System, "the undo is saved too");
    assert_eq!(s.theme(), Theme::System);
}

#[test]
fn a_first_start_without_ui_bin_keeps_the_system_language() {
    // No record yet: the window started in the system's language (English here), and it stays.
    let mut s = UiState::new(Language::En, false);
    s.load_prefs(Box::new(MemPrefs::default()));
    assert_eq!(s.user_prefs().language, Language::En);
    assert_eq!(s.user_prefs().theme, Theme::System);
}

#[test]
fn pairing_texts_never_point_at_a_time_the_screen_does_not_show() {
    // Owner, 09/11: "dentro desse tempo" with no time shown.
    let s = Scenario::ParearEsteComputador.prepared_state();
    let specs = s.controls(WindowKind::Main);
    let body = specs.iter().position(|c| c.label.contains("tempo abaixo")).expect("the body");
    let timer = &specs[body + 1];
    assert_eq!(timer.role, UiRole::Progress, "the time follows the text that refers to it: {timer:?}");
    let left = timer.value.clone().expect("the time drawn large");
    assert!(left.contains(':') && timer.label.contains(&left), "{timer:?}");

    // An unpaired device's panel has no countdown: its text names no time.
    let mut s = Scenario::Dispositivos.prepared_state();
    let unpaired = s.devices().iter().position(|d| !d.paired).expect("an unpaired device in the scenario");
    s.activate(ControlId::Device(unpaired as u16));
    let texts: Vec<String> = s.controls(WindowKind::Main).into_iter().map(|c| c.label).collect();
    assert!(texts.iter().any(|t| t.starts_with("Este computador ainda não está pareado")), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("desse tempo") || t.contains("tempo abaixo")), "{texts:?}");
}

#[test]
fn the_login_item_the_system_reports_shows_in_the_switch_and_is_kept() {
    let mem = MemPrefs::default();
    let mut s = UiState::new(Language::PtBr, false);
    s.load_prefs(Box::new(mem.clone()));
    // Before the settings arrive: the shell found the login item registered.
    s.set_open_at_login(true);
    let saved = UserPrefs::decode(&mem.read().unwrap().expect("written")).expect("a valid record");
    assert!(saved.open_at_login);
    // The service answers its default (off) for the per-user field; the switch stays on.
    let from_service = Settings { open_at_login: false, ..base().settings().unwrap().clone() };
    s.apply(UiEvent::SettingsChanged(from_service));
    assert!(s.settings().unwrap().open_at_login);
    // With the settings already there, the switch follows at once.
    s.set_open_at_login(false);
    assert!(!s.settings().unwrap().open_at_login);
    assert!(!UserPrefs::decode(&mem.read().unwrap().unwrap()).unwrap().open_at_login);
    // The person turns it on in the window: the command carries it, for the shell to act on.
    let act = s.activate(ControlId::OpenAtLogin);
    assert!(
        matches!(act.commands.as_slice(), [Command::SetSettings { settings }] if settings.open_at_login),
        "{:?}",
        act.commands
    );
}

// ---------- the other waits (docs/WAITING.md section 4, rows 7, 8, 9, 14, 21, 22) ----------

#[test]
fn a_hosted_session_that_reconnects_keeps_the_banner_and_opens_no_viewer() {
    let mut s = base();
    s.apply(session_event(2, MAC, active(Role::Host, Permissions::NONE.with(Permission::Control))));
    assert_eq!(s.windows(), vec![WindowKind::Main, WindowKind::Banner]);
    for lost in [SessionState::Reconnecting { attempt: 0 }, SessionState::Paused { desktop: DesktopKind::Locked }] {
        s.apply(session_event(2, MAC, lost.clone()));
        assert!(s.viewer_session().is_none(), "no viewer window opens on the host ({lost:?})");
        assert_eq!(s.host_session().map(|h| h.id), Some(2), "the banner stays ({lost:?})");
        assert_eq!(s.windows(), vec![WindowKind::Main, WindowKind::Banner], "{lost:?}");
        assert!(!s.controls(WindowKind::Banner).is_empty(), "{lost:?}");
    }
    // While it reconnects the banner says so, in the existing text.
    s.apply(session_event(2, MAC, SessionState::Reconnecting { attempt: 1 }));
    let h = s.host_session().unwrap().clone();
    let (who, since) = s.banner_parts("MacBook", &h);
    assert_eq!(who, "Reconectando ao MacBook");
    assert!(since.is_empty());
    assert_eq!(s.banner_text("MacBook", &h), "Reconectando ao MacBook");
    // Back: the banner is as before.
    s.apply(session_event(2, MAC, active(Role::Host, Permissions::NONE)));
    assert_eq!(s.windows(), vec![WindowKind::Main, WindowKind::Banner]);
    // A viewer session that reconnects keeps its window and no banner appears.
    let mut v = viewer();
    v.apply(session_event(1, MAC, SessionState::Reconnecting { attempt: 1 }));
    assert_eq!(v.windows(), vec![WindowKind::Main, WindowKind::Viewer]);
    assert!(v.host_session().is_none());
}

#[test]
fn reconnecting_counts_the_minute_down() {
    let mut s = viewer();
    // Attempt 0 (the loss was just seen): the track alone, nothing counted yet.
    s.apply(session_event(1, MAC, SessionState::Reconnecting { attempt: 0 }));
    assert_eq!(s.reconnect_wait(), Some(WaitBar { fraction: 0.0, text: None }));
    assert!(s.needs_tick());
    s.tick();
    s.tick();
    assert_eq!(s.reconnect_wait().unwrap().text, None, "the minute starts at the first attempt");
    // The first attempt starts the 60 s.
    s.apply(session_event(1, MAC, SessionState::Reconnecting { attempt: 1 }));
    let bar = s.reconnect_wait().unwrap();
    assert_eq!((bar.text.as_deref(), bar.fraction), (Some("1:00"), 0.0));
    for _ in 0..15 {
        s.tick();
    }
    let bar = s.reconnect_wait().unwrap();
    assert_eq!(bar.text.as_deref(), Some("0:45"));
    assert!((bar.fraction - 0.25).abs() < 1e-6, "{}", bar.fraction);
    // A later attempt does not restart it.
    s.apply(session_event(1, MAC, SessionState::Reconnecting { attempt: 2 }));
    assert_eq!(s.reconnect_wait().unwrap().text.as_deref(), Some("0:45"));
    // It stops at zero.
    for _ in 0..70 {
        s.tick();
    }
    let bar = s.reconnect_wait().unwrap();
    assert_eq!((bar.text.as_deref(), bar.fraction), (Some("0:00"), 1.0));
    // The accessibility tree reads the time too.
    assert!(
        s.controls(WindowKind::Viewer).iter().any(|c| c.role == UiRole::Progress && c.value.as_deref() == Some("0:00"))
    );
    // Back, and lost again: a new minute.
    s.apply(session_event(1, MAC, active(Role::Viewer, Permissions::NONE)));
    assert_eq!(s.reconnect_wait(), None);
    s.apply(session_event(1, MAC, SessionState::Reconnecting { attempt: 1 }));
    assert_eq!(s.reconnect_wait().unwrap().text.as_deref(), Some("1:00"));
}

#[test]
fn asking_to_swap_shows_who_is_asked_and_counts_22_s() {
    let mut s = Scenario::InverterAguardando.prepared_state();
    assert!(s.swap_requested());
    let w = s.swap_wait().expect("the card shows while the request waits");
    assert_eq!(w.session_id, 1);
    assert_eq!(w.title, "Esperando o Desktop do escritório aceitar a inversão");
    assert!(w.body.contains("Desktop do escritório") && w.body.contains("20 segundos"), "{}", w.body);
    // The scenario ran 6 of the 22 seconds.
    assert_eq!(w.bar.text.as_deref(), Some("0:16"));
    assert!((w.bar.fraction - 6.0 / 22.0).abs() < 1e-6, "{}", w.bar.fraction);
    // No button but the toolbar's, whose swap button waits.
    let ids: Vec<_> = s.controls(WindowKind::Viewer).into_iter().map(|c| (c.id, c.disabled)).collect();
    assert!(ids.contains(&(ControlId::TbSwap, true)));
    assert!(!ids.iter().any(|(id, _)| *id == ControlId::SwapDeclinedClose));
    assert!(s.needs_tick());
    // It counts to the end and stops there.
    for _ in 0..30 {
        s.tick();
    }
    let w = s.swap_wait().unwrap();
    assert_eq!((w.bar.text.as_deref(), w.bar.fraction), (Some("0:00"), 1.0));
    // The outcome closes it.
    s.apply(UiEvent::Swap { session_id: 1, event: SwapEvent::Declined { why: SwapWhy::TimedOut } });
    assert!(s.swap_wait().is_none());

    // A fresh request counts from 22 again.
    let mut s = Scenario::Inverter.prepared_state();
    assert!(s.swap_wait().is_none(), "the confirm card is not the wait");
    s.activate(ControlId::DialogPrimary);
    assert_eq!(s.swap_wait().unwrap().bar.text.as_deref(), Some("0:22"));
    assert!(s.controls(WindowKind::Viewer).iter().any(|c| c.role == UiRole::Progress));

    // The host's banner says it in its own line, and the viewer card is not shown there.
    let mut h = Scenario::Banner.prepared_state();
    let before = h.banner_text("Desktop", h.host_session().unwrap());
    assert!(before.contains("controla"), "{before}");
    h.activate(ControlId::BannerSwap);
    assert!(h.swap_wait().is_none());
    let session = h.host_session().unwrap().clone();
    assert_eq!(h.banner_parts("Desktop", &session).0, "Esperando o Desktop aceitar a inversão");
    assert_eq!(h.banner_text("Desktop", &session), "Esperando o Desktop aceitar a inversão");
    assert!(h.needs_tick());
    h.apply(UiEvent::Swap { session_id: 2, event: SwapEvent::Declined { why: SwapWhy::Declined } });
    assert!(h.banner_text("Desktop", &session).contains("controla"));
}

#[test]
fn after_the_codes_match_the_card_says_it_waits_and_offers_cancel() {
    let s = Scenario::Pareamento.prepared_state();
    assert!(s.pair_wait().is_none());
    let ids = |s: &UiState| s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert!(ids(&s).contains(&ControlId::PairSame) && ids(&s).contains(&ControlId::PairDifferent));
    let text = labels(&s, WindowKind::Main);
    assert!(text.contains("Se forem diferentes"), "{text}");

    let s = Scenario::PareamentoConfirmando.prepared_state();
    let w = s.pair_wait().expect("the card waits");
    assert_eq!(w.body, "O outro lado precisa confirmar o código. Isso leva alguns segundos.");
    assert_eq!(
        w.buttons,
        vec![WaitButton {
            id: ControlId::PairCancel,
            label: "Cancelar".into(),
            side: ButtonSide::Left,
            primary: false
        }]
    );
    let text = labels(&s, WindowKind::Main);
    assert!(text.contains("O outro lado precisa confirmar") && !text.contains("Se forem diferentes"), "{text}");
    assert!(text.contains("482 913"), "the code stays on screen: {text}");
    assert_eq!(
        s.controls(WindowKind::Main).iter().filter(|c| c.role == UiRole::Button).map(|c| c.id).collect::<Vec<_>>(),
        vec![ControlId::PairCancel]
    );
    // Cancel after the confirmation closes the card and tells the service.
    let mut s = s;
    let a = s.activate(ControlId::PairCancel);
    assert_eq!(a.commands, vec![Command::PairCancel { pairing_id: 1 }]);
    assert!(s.pairing().is_none() && s.pair_wait().is_none());
    // The other side's answer (Done or Failed) ends the step too.
    let mut s = Scenario::PareamentoConfirmando.prepared_state();
    s.apply(UiEvent::Pairing { pairing_id: 1, state: PairingState::Failed { reason: ApiCode::Refused } });
    assert!(s.pair_wait().is_none());
}

#[test]
fn a_pending_action_disables_the_actions_and_says_so() {
    let mut s = viewer();
    s.activate(ControlId::TbTerminal);
    let action_buttons = |s: &UiState| {
        s.controls(WindowKind::Viewer)
            .into_iter()
            .filter(|c| matches!(c.id, ControlId::Action(_)))
            .map(|c| c.disabled)
            .collect::<Vec<_>>()
    };
    assert_eq!(action_buttons(&s), vec![false; 4]);
    assert!(s.action_wait().is_none() && !s.actions_disabled());
    let a = s.activate(ControlId::Action(ActionKind::Sleep));
    assert_eq!(a.commands, vec![Command::RunAction { session_id: 1, action: ActionKind::Sleep }]);
    s.track(Ticket(40), &a.commands[0]);
    assert_eq!(action_buttons(&s), vec![true; 4], "every quick action waits");
    assert!(s.actions_disabled());
    let w = s.action_wait().expect("the line shows");
    assert_eq!(w.session_id, 1);
    assert!(w.line.starts_with("Esperando o ") && w.line.ends_with("executar a ação…"), "{}", w.line);
    assert!(labels(&s, WindowKind::Viewer).contains("executar a ação"));
    assert!(s.activate(ControlId::Action(ActionKind::Lock)).commands.is_empty(), "one action at a time");
    // The answer frees them, a failure included.
    s.apply(UiEvent::Reply { ticket: Ticket(40), result: Ok(Reply::Done) });
    assert_eq!(action_buttons(&s), vec![false; 4]);
    assert!(s.action_wait().is_none());
    let a = s.activate(ControlId::Action(ActionKind::Lock));
    s.track(Ticket(41), &a.commands[0]);
    assert!(s.actions_disabled());
    s.apply(UiEvent::Reply { ticket: Ticket(41), result: Err(ApiError::new(ApiCode::NotPermitted)) });
    assert!(!s.actions_disabled() && s.action_wait().is_none());
    // The interface's own 30 s limit frees them as well.
    let a = s.activate(ControlId::Action(ActionKind::Lock));
    s.track(Ticket(42), &a.commands[0]);
    for _ in 0..30 {
        s.tick();
    }
    assert!(!s.actions_disabled());
}

#[test]
fn the_elevated_window_answers_only_by_its_buttons_and_declines_when_the_time_runs_out() {
    let make = || UiState::for_elevated_confirm(Language::PtBr, false, "Permitir X".into(), 3);
    let s = make();
    assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::ElevatedConfirm));
    let t = s.dialog_texts(&DialogKind::ElevatedConfirm);
    assert_eq!(t.title, "Confirmação de administrador");
    assert_eq!(t.body, ["Permitir X", "Confirme como administrador para aplicar esta mudança."]);
    assert_eq!((t.secondary.as_deref(), t.primary.as_deref()), (Some("Cancelar"), Some("Confirmar")));
    assert_eq!(s.confirm_answer(), None);
    assert!(s.needs_tick());
    // "Confirmar" approves, and the first answer stands.
    let mut s = make();
    assert!(s.activate(ControlId::DialogPrimary).commands.is_empty());
    assert_eq!(s.confirm_answer(), Some(true));
    s.activate(ControlId::DialogSecondary);
    assert_eq!(s.confirm_answer(), Some(true));
    assert!(!s.needs_tick());
    // "Cancelar", which is also what Esc means there, declines.
    let mut s = make();
    assert_eq!(s.escape_target(WindowKind::Main), Some(ControlId::DialogSecondary));
    s.activate(ControlId::DialogSecondary);
    assert_eq!(s.confirm_answer(), Some(false));
    // The ticket's time running out declines.
    let mut s = make();
    s.tick();
    s.tick();
    assert_eq!(s.confirm_answer(), None);
    s.tick();
    assert_eq!(s.confirm_answer(), Some(false));
    // The normal interface never has one.
    assert!(UiState::new(Language::PtBr, false).elevated_confirm().is_none());
    assert_eq!(base().escape_target(WindowKind::Main), None);
}

// ---------- WAITING.md 4, rows 11 and 12: the host asks its user about the terminal or the files ----------

fn consent_pending(feature: Feature, remaining_s: u16) -> UiEvent {
    UiEvent::ConsentPending { session_id: 1, feature, remaining_s }
}

#[test]
fn the_terminal_says_that_the_host_asks_its_user_and_counts_down_until_the_answer() {
    let mut s = viewer();
    let cmds = open_terminal(&mut s);
    assert_eq!(s.consent_wait(Feature::Terminal), None, "nobody is asked yet");
    s.apply(consent_pending(Feature::Terminal, 20));
    let bar = s.consent_wait(Feature::Terminal).expect("the question shows");
    assert_eq!((bar.fraction, bar.text.as_deref()), (0.0, Some("0:20")));
    assert_eq!(s.consent_wait(Feature::Files), None, "only for the feature asked about");
    for _ in 0..5 {
        s.tick();
    }
    let bar = s.consent_wait(Feature::Terminal).unwrap();
    assert_eq!(bar.text.as_deref(), Some("0:15"));
    assert!((bar.fraction - 0.25).abs() < 1e-6);
    // The person said yes: the shell opens and the line goes.
    answer_open(&mut s, &cmds, 7);
    assert_eq!(s.consent_wait(Feature::Terminal), None);
}

#[test]
fn a_refused_terminal_ends_the_question_and_a_question_of_another_session_is_ignored() {
    let mut s = viewer();
    let cmds = open_terminal(&mut s);
    s.apply(UiEvent::ConsentPending { session_id: 99, feature: Feature::Terminal, remaining_s: 20 });
    assert_eq!(s.consent_wait(Feature::Terminal), None, "not this window's session");
    s.apply(consent_pending(Feature::Terminal, 20));
    tracked_error(&mut s, &cmds[0], ApiCode::Refused);
    assert_eq!(s.consent_wait(Feature::Terminal), None);
    assert!(matches!(s.term_phase(), TermPhase::Failed { .. }));
}

#[test]
fn the_files_say_that_the_host_asks_its_user_until_the_listing_comes() {
    let mut s = viewer();
    s.activate(ControlId::TbFiles);
    s.apply(consent_pending(Feature::Files, 20));
    assert_eq!(s.consent_wait(Feature::Files).unwrap().text.as_deref(), Some("0:20"));
    s.apply(reply_ok(Reply::Listing(Listing { path: String::new(), entries: Vec::new(), truncated: false })));
    assert_eq!(s.consent_wait(Feature::Files), None);
    // The first listing waits for the 20 s question and then lists: its limit covers both.
    assert_eq!(Command::FilesList { session_id: 1, path: None }.timeout_s(), 25);
    // A send whose transfer starts was answered too.
    s.apply(consent_pending(Feature::Files, 20));
    s.apply(UiEvent::Transfer {
        transfer_id: 1,
        session_id: 1,
        name: "a.bin".into(),
        outgoing: true,
        state: TransferState::Queued,
    });
    assert!(s.consent_wait(Feature::Files).is_some(), "queued: still asked");
    s.apply(UiEvent::Transfer {
        transfer_id: 1,
        session_id: 1,
        name: "a.bin".into(),
        outgoing: true,
        state: TransferState::Failed { reason: ApiCode::Refused },
    });
    assert_eq!(s.consent_wait(Feature::Files), None);
}

/// WAITING.md 4, row 23: the link to the service broke and is open again. The sessions of the old service ended with
/// it, and the interface asks for its data again.
#[test]
fn a_reconnected_service_ends_the_old_sessions_and_the_data_is_asked_for_again() {
    let mut s = viewer();
    assert!(s.viewer_session().is_some());
    s.take_followups();
    s.apply(UiEvent::ServiceReconnected);
    assert!(s.viewer_session().is_none(), "the session of the old service is gone");
    let again = s.take_followups();
    for c in [Command::GetThisDevice, Command::GetNetwork, Command::ListDevices, Command::GetSettings] {
        assert!(again.contains(&c), "{c:?} is asked again: {again:?}");
    }
}

/// Review of the elevation, findings 1 and 8: the confirmation names every machine-level permission the change
/// leaves, and a device name cannot carry control or direction-override characters into it.
#[test]
fn the_confirmation_names_every_machine_level_permission_and_a_clean_device_name() {
    use crate::interact::{elevation_action_text, shown_name};
    let both = Permissions(Permission::Unattended.bit() | Permission::AdminTerminal.bit() | Permission::View.bit());
    let action = ElevatedAction::SetGrants { device_id: MAC.into(), grants: both };
    let text = elevation_action_text(Language::PtBr, &action, "Mac");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].contains("Entrar sem perguntar") && lines[1].contains("terminal de administrador"), "{text}");
    let only_terminal = ElevatedAction::SetGrants {
        device_id: MAC.into(),
        grants: Permissions(Permission::AdminTerminal.bit() | Permission::View.bit()),
    };
    assert_eq!(elevation_action_text(Language::PtBr, &only_terminal, "Mac").lines().count(), 1);
    assert_eq!(shown_name("Mac\u{202e}cod.exe\n\u{2066}x\u{7}"), "Maccod.exex");
    let named = elevation_action_text(Language::PtBr, &action, "Ma\u{202e}c");
    assert!(!named.contains('\u{202e}'), "{named:?}");
}

/// ELEVATION-MAC.md M5: a Mac serves no administrator terminal in this version (D32), so on a Mac its switch is off,
/// disabled and does nothing, with the line that says why; on Windows it is a switch like the others.
#[test]
fn on_a_mac_the_administrator_terminal_switch_is_off_and_inert_with_its_line() {
    let mut s = Scenario::PermissoesNoMac.prepared_state();
    assert!(!s.grant_available(Permission::AdminTerminal));
    assert!(s.grant_available(Permission::Unattended), "only the administrator terminal");
    let row = s
        .controls(WindowKind::Main)
        .into_iter()
        .find(|c| c.id == ControlId::Grant(Permission::AdminTerminal))
        .expect("the row is there");
    assert!(row.disabled && row.toggled == Some(false), "{row:?}");
    assert_eq!(row.description.as_deref(), Some("Indisponível no macOS nesta versão"));
    let a = s.activate(ControlId::Grant(Permission::AdminTerminal));
    assert!(a.commands.is_empty(), "nothing is sent");

    let s = Scenario::Permissoes.prepared_state();
    assert!(s.grant_available(Permission::AdminTerminal), "Windows serves it");
}

/// GRANTS-SYNC.md 8 S7: opening Permissões of a paired device asks the service to refresh what the other side
/// allows, once per opening; a device that is not paired has no such column.
#[test]
fn opening_permissions_asks_the_service_to_refresh_that_device() {
    let mut s = base();
    s.take_followups();
    s.activate(ControlId::Tab(DeviceTab::Permissions));
    let device = s.selected_device().unwrap().id.clone();
    assert_eq!(s.take_followups(), vec![Command::RefreshGrants { device_id: device }]);
    // Already on the tab: nothing more.
    s.activate(ControlId::Tab(DeviceTab::Permissions));
    assert!(s.take_followups().is_empty());
    // Away and back: once more.
    s.activate(ControlId::Tab(DeviceTab::Overview));
    s.activate(ControlId::Tab(DeviceTab::Permissions));
    assert_eq!(s.take_followups().len(), 1);
}

/// ELEVATION.md slice 7: a confirmed "Elevar controle" sends op 50 again, once.
#[test]
fn a_confirmed_elevate_session_sends_elevate_session_again() {
    let mut s = viewer();
    tracked_error(&mut s, &Command::ElevateSession { session_id: 1 }, ApiCode::NeedsAdmin);
    let a = s.activate(ControlId::DialogPrimary);
    let Command::Elevate { action, run, .. } = a.commands[0].clone() else { panic!("{:?}", a.commands) };
    assert_eq!(action, ElevatedAction::ElevateSession { session_id: 1 });
    let _ = s.take_followups();
    s.apply(UiEvent::ElevationResult { action, run, result: Ok(()) });
    assert!(s.elevation().is_none());
    assert_eq!(s.take_followups(), vec![Command::ElevateSession { session_id: 1 }]);
    assert!(s.take_followups().is_empty());
}

/// ELEVATION.md slice 7: a confirmed administrator terminal opens it in that session's panel.
#[test]
fn a_confirmed_admin_terminal_opens_the_administrator_terminal() {
    let mut s = viewer();
    tracked_error(&mut s, &Command::TermOpen { session_id: 1, admin: true, cols: 80, rows: 24 }, ApiCode::NeedsAdmin);
    let a = s.activate(ControlId::DialogPrimary);
    let Command::Elevate { action, run, .. } = a.commands[0].clone() else { panic!("{:?}", a.commands) };
    let _ = s.take_followups();
    s.apply(UiEvent::ElevationResult { action, run, result: Ok(()) });
    assert_eq!(s.take_followups(), vec![Command::TermOpen { session_id: 1, admin: true, cols: 80, rows: 24 }]);
    assert_eq!(s.term_phase(), &TermPhase::Opening { admin: true });
}

/// ACCOUNTS.md 9 and D55: a session that entered without a prompt offers "Elevar controle" on its bar and, paused at
/// a protected screen, on a card over the picture; both send op 50. A prompted session offers neither.
#[test]
fn an_unattended_session_offers_elevar_controle_on_the_bar_and_on_the_paused_card() {
    let mut s = Scenario::VisualizadorElevar.prepared_state();
    assert!(s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::TbElevate));
    let a = s.activate(ControlId::TbElevate);
    assert_eq!(a.commands, vec![Command::ElevateSession { session_id: 1 }]);
    assert!(s.toolbar_hint(ControlId::TbElevate).is_some_and(|h| h.contains("tela de entrada")));

    let mut s = Scenario::VisualizadorPausadoElevar.prepared_state();
    let (body, button) = s.paused_card().expect("the card");
    assert!(body.contains("tela protegida") && button);
    let a = s.activate(ControlId::PausedElevate);
    assert_eq!(a.commands, vec![Command::ElevateSession { session_id: 1 }]);

    // The account left the console: the card says so, with nothing to press.
    let s = Scenario::VisualizadorContaAusente.prepared_state();
    let (body, button) = s.paused_card().expect("the card");
    assert!(body.contains("Esta conta não está mais na tela") && !button);
    assert!(!s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::TbElevate), "nothing to elevate");

    // A prompted session: no button. Paused at a Mac's lock screen, the card says the Mac is locked and nobody can
    // unlock it from here (D24), with nothing to press.
    let mut s = viewer();
    assert!(!s.controls(WindowKind::Viewer).iter().any(|c| c.id == ControlId::TbElevate));
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: MAC.into(),
        state: SessionState::Paused { desktop: DesktopKind::Locked },
    });
    let (body, button) = s.paused_card().expect("the Mac's lock card");
    assert!(body.contains("bloqueado") && !button, "{body}");
    assert!(s.activate(ControlId::TbElevate).commands.is_empty());
}

/// ACCOUNTS.md 9.2: the host's banner says that the session was elevated.
#[test]
fn the_banner_of_an_elevated_session_says_controle_elevado() {
    let s = Scenario::BannerElevado.prepared_state();
    let h = s.host_session().unwrap().clone();
    assert_eq!(s.banner_text("Mesa", &h), "Mesa controla este computador · Controle elevado");
    let s = Scenario::Banner.prepared_state();
    let h = s.host_session().unwrap().clone();
    assert!(!s.banner_text("Mesa", &h).contains("Controle elevado"));
}

/// D56: the confirmation of a standard account says Windows will ask for an administrator's name and password; an
/// administrator account keeps the body of 1.0.
#[test]
fn a_standard_accounts_confirmation_asks_for_an_administrators_name_and_password() {
    let body = |admin: bool| {
        let mut s = Scenario::Elevacao.prepared_state();
        s.set_admin_account(admin);
        s.dialog_texts(&DialogKind::Elevation).body.join(" ")
    };
    assert!(body(false).contains("nome e a senha de um administrador"), "{}", body(false));
    assert!(!body(true).contains("nome e a senha"), "{}", body(true));
}

/// F-VIEW-02: the toolbar's screen button lists the other computer's displays, the shown one checked, and a pick
/// asks the host to switch.
#[test]
fn the_screen_button_lists_the_hosts_displays_and_a_pick_switches() {
    let mut s = viewer();
    let d = |id: u16, name: &str, primary: bool| DisplayView {
        id,
        name: name.into(),
        width: 2560,
        height: 1440,
        scale_milli: 1500,
        primary,
    };
    let a = s.activate(ControlId::TbScreen);
    assert!(matches!(a.commands[0], Command::ListDisplays { session_id: 1 }));
    assert_eq!(a.effect, None, "nothing known yet: the list is asked");
    s.track(Ticket(77), &Command::ListDisplays { session_id: 1 });
    s.apply(UiEvent::Reply {
        ticket: Ticket(77),
        result: Ok(Reply::Displays(vec![d(1, "", true), d(2, "LG", false)])),
    });
    let a = s.activate(ControlId::TbScreen);
    assert_eq!(a.effect, Some(Effect::Menu(SelectId::HostDisplay)));
    assert_eq!(SelectId::HostDisplay.anchor(), ControlId::TbScreen);
    let opts = s.select_options(SelectId::HostDisplay);
    assert_eq!(opts[0], ("Tela 1 (principal), 2560 × 1440".to_string(), true));
    assert_eq!(opts[1], ("LG, 2560 × 1440".to_string(), false));
    let a = s.choose(SelectId::HostDisplay, 1);
    assert_eq!(a.commands, vec![Command::SelectDisplay { session_id: 1, display_id: 2 }]);
    assert!(s.select_options(SelectId::HostDisplay)[1].1, "the pick is checked");
}

/// D65: Ajustes lists the devices blocked by MAC, each with Desbloquear; unblocking takes the row away at once and
/// asks the service, whose reply is the list as it then is.
#[test]
fn ajustes_lists_the_blocked_devices_and_unblocking_one_takes_it_away() {
    use crate::api::BlockedItem;
    let mut s = Scenario::Ajustes.prepared_state();
    let item = |mac: &str, ip: &str| BlockedItem {
        mac: mac.into(),
        last_ipv4: ip.into(),
        name: String::new(),
        blocked_at_ms: 0,
    };
    s.apply(UiEvent::Reply {
        ticket: crate::api::Ticket(0),
        result: Ok(Reply::Blocked(vec![
            item("02:00:00:00:00:01", "192.168.0.57"),
            item("02:00:00:00:00:02", "192.168.0.9"),
        ])),
    });
    let ids: Vec<ControlId> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::Unblock(0)) && ids.contains(&ControlId::Unblock(1)));
    let rows = s.blocked_rows();
    assert_eq!(rows[0].0, "Dispositivo sem nome");
    assert!(rows[0].1.starts_with("02:00:00:00:00:01 · último endereço 192.168.0.57"), "{}", rows[0].1);

    let a = s.activate(ControlId::Unblock(0));
    assert_eq!(a.commands, vec![Command::UnblockDevice { mac: "02:00:00:00:00:01".into() }]);
    assert_eq!(s.blocked_rows().len(), 1, "the row goes at once");
    s.apply(UiEvent::Reply { ticket: crate::api::Ticket(0), result: Ok(Reply::Blocked(Vec::new())) });
    let texts: Vec<String> = s.controls(WindowKind::Main).into_iter().map(|c| c.label).collect();
    assert!(texts.contains(&"Nenhum dispositivo bloqueado.".to_owned()), "{texts:?}");
}

/// F-VIEW-06: the viewer window's area reaches the host as `SetViewSize`, once per change; 1:1 mode reports native.
#[test]
fn the_viewer_area_is_reported_in_physical_pixels_in_fit_and_as_native_in_one_to_one() {
    let mut s = viewer();
    let set =
        |id: u64, w: u16, h: u16, hz: u16| Command::SetViewSize { session_id: id, width: w, height: h, max_fps: hz };

    assert_eq!(s.set_view_area(1280, 720, 144), vec![set(1, 1280, 720, 144)], "Fit reports the area");
    assert_eq!(s.set_view_area(1280, 720, 144), vec![], "nothing when the size is unchanged");
    assert_eq!(s.set_view_area(1000, 720, 144), vec![set(1, 1000, 720, 144)], "a resize");
    assert_eq!(s.set_view_area(1000, 720, 60), vec![set(1, 1000, 720, 60)], "the monitor's refresh changed");

    // 1:1 reports 0x0 at once, and the resizes that follow are not reported.
    let a = s.activate(ControlId::TbFit);
    assert!(!s.fit());
    assert!(a.commands.contains(&set(1, 0, 0, 60)), "{:?}", a.commands);
    assert_eq!(s.set_view_area(800, 600, 60), vec![], "1:1 stays native whatever the window does");

    // Back to Fit: the latest area.
    let a = s.activate(ControlId::TbFit);
    assert!(s.fit());
    assert!(a.commands.contains(&set(1, 800, 600, 60)), "{:?}", a.commands);
}

#[test]
fn no_viewer_session_reports_no_area() {
    let mut s = UiState::default();
    assert_eq!(s.set_view_area(1280, 720, 60), vec![]);
}

// ---------- the first-run guide card (F-UI-04) ----------

#[test]
fn an_empty_prefs_store_shows_the_first_run_guide_card() {
    let mut s = UiState::new(Language::PtBr, false);
    assert!(s.guide_card().is_none(), "before the store is read nothing shows");
    s.load_prefs(Box::new(MemPrefs::default()));
    let (title, body) = s.guide_card().expect("an empty store is a first start");
    assert_eq!(title, "Bem-vindo ao Swapdesk");
    assert!(!body.is_empty());
}

#[test]
fn guide_skip_and_guide_done_each_close_the_card_and_write_the_record_and_it_never_comes_back() {
    for id in [ControlId::GuideSkip, ControlId::GuideDone] {
        let mem = MemPrefs::default();
        let mut s = UiState::new(Language::PtBr, false);
        s.load_prefs(Box::new(mem.clone()));
        assert!(s.guide_card().is_some());
        assert_eq!(mem.read().unwrap(), None, "showing the card writes nothing");
        s.activate(id);
        assert!(s.guide_card().is_none(), "{id:?} closes the guide");
        let bytes = mem.read().unwrap().expect("closing writes the record");
        assert!(UserPrefs::decode(&bytes).is_ok(), "a valid record");
        // A second start over the same store: no card.
        let mut next = UiState::new(Language::PtBr, false);
        next.load_prefs(Box::new(mem.clone()));
        assert!(next.guide_card().is_none(), "{id:?}: the guide is dismissed for good");
    }
}

// ---------- the stats line names the codec (F-VIEW-07) ----------

/// The overlay shall name the codec in use: the toolbar's session line (also read with the toolbar) ends with the
/// numbers and "HEVC" when the session's pictures come in HEVC, "H.264" otherwise, and shows no codec when Ajustes
/// hides the numbers.
#[test]
fn the_toolbar_session_line_names_the_codec_hevc_or_h264() {
    let line = |s: &UiState| s.viewer_session_line(&s.viewer_session().unwrap().clone());
    let stats = |hevc| UiEvent::Stats { session_id: 1, stats: Stats { latency_ms: 18, fps: 60, kbps: 9000, hevc } };

    let mut s = viewer();
    assert!(s.settings().unwrap().show_stats);
    s.apply(stats(true));
    let l = line(&s);
    assert!(l.ends_with("18 ms, 60 qps, HEVC"), "{l}");
    s.apply(stats(false));
    let l = line(&s);
    assert!(l.ends_with("18 ms, 60 qps, H.264") && !l.contains("HEVC"), "{l}");

    // Read with the toolbar: the same text is in the accessible list.
    s.apply(stats(true));
    let in_bar = s.controls(WindowKind::Viewer).into_iter().any(|c| c.label.ends_with("HEVC"));
    assert!(in_bar, "the toolbar's text names the codec");

    // Ajustes hides the numbers: no codec either.
    let mut hidden = viewer();
    hidden.activate(ControlId::ShowStats);
    hidden.apply(stats(true));
    assert!(!hidden.settings().unwrap().show_stats);
    assert!(!line(&hidden).contains("HEVC") && !line(&hidden).contains("H.264"), "{}", line(&hidden));
}
