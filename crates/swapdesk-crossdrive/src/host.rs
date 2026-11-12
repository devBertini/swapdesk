//! The scripted user of the machine being controlled.
//!
//! Markers printed through `log`, read by the orchestrator: `HOST_PAIR_OPEN`, `HOST_CODE <code>`, `HOST_READY <id>`
//! (the viewer may connect now), `HOST_SESSION_ACTIVE`, `HOST_SESSION_ENDED`, `HOST_FORGOTTEN`.
//!
//! While the session is active the host's user also answers the chat proof ([`crate::chat`]): `HOST_CHAT_GOT <text>`
//! is what its interface received (the message is then marked read), `HOST_CHAT_SENT` follows the sending of the text
//! in the file [`HostConfig::chat_say`], `HOST_CHAT_READ_BY_PEER` is the viewer's read state; and it logs the session
//! going `HOST_SESSION_RECONNECTING` and `HOST_SESSION_ACTIVE_AGAIN` for the reconnect proof.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use swapdesk_ui::{
    ApiCode, Command, Decision, PairingState, Permissions, Reply, RequestKind, Role, SessionState, UiBackend, UiEvent,
};

use crate::pump::{Pump, wait_file};

const REPLY: Duration = Duration::from_secs(10);

pub struct HostConfig {
    /// The file the orchestrator writes the viewer's code to; compared with ours before `PairConfirm`.
    pub peer_code: PathBuf,
    /// How long the whole pairing may take (pairing mode itself lasts 2 minutes, D12).
    pub pair_timeout: Duration,
    /// How long to wait for the viewer's request, and then for its session to end.
    pub session_timeout: Duration,
    /// What the incoming request is answered with (VIEW, CONTROL and AUDIO).
    pub grants: Permissions,
    /// The file the orchestrator writes a chat text to; once it exists the text is sent to the viewer on the hosted
    /// session (`ChatSend`). `None`: the host sends no chat message.
    pub chat_say: Option<PathBuf>,
}

/// What `host_drive` learned, for the caller's last line.
#[derive(Debug, Default, Clone)]
pub struct HostOutcome {
    pub peer_device: Option<String>,
    pub session_active: bool,
    pub session_ended: bool,
    pub forgotten: bool,
}

/// Runs the host side to the end. The device paired in this run is forgotten on every path that got as far as pairing.
pub fn host_drive(backend: &dyn UiBackend, cfg: &HostConfig, log: &dyn Fn(&str)) -> (HostOutcome, Result<(), String>) {
    let mut pump = Pump::new(backend);
    let mut out = HostOutcome::default();
    let result = script(&mut pump, cfg, log, &mut out);
    if let Some(device) = out.peer_device.clone() {
        match forget(&mut pump, &device) {
            Ok(()) => {
                out.forgotten = true;
                log("HOST_FORGOTTEN");
            }
            Err(e) => log(&format!("HOST_FORGET_FAILED {e}")),
        }
    }
    (out, result)
}

fn forget(pump: &mut Pump<'_>, device: &str) -> Result<(), String> {
    pump.ask(Command::ForgetDevice { device_id: device.to_string() }, REPLY)?;
    match pump.ask(Command::ListDevices, REPLY)? {
        Reply::Devices(list) if list.iter().any(|d| d.id == device && d.paired) => {
            Err("the device is still listed as paired".into())
        }
        Reply::Devices(_) => Ok(()),
        other => Err(format!("unexpected reply to ListDevices: {other:?}")),
    }
}

fn script(pump: &mut Pump<'_>, cfg: &HostConfig, log: &dyn Fn(&str), out: &mut HostOutcome) -> Result<(), String> {
    let Reply::PairingId(pairing_id) = pump.ask(Command::PairOpen { device_id: None }, REPLY)? else {
        return Err("PairOpen did not reply a pairing id".into());
    };
    log("HOST_PAIR_OPEN");

    let outcome = pair(pump, cfg, log, pairing_id);
    let device = match outcome {
        Ok(d) => d,
        Err(e) => {
            // A screen still open is closed; a refusal means it already ended.
            let _ = pump.request(Command::PairCancel { pairing_id }, REPLY);
            return Err(e);
        }
    };
    out.peer_device = Some(device.clone());
    log(&format!("HOST_PAIRED {device}"));

    pump.ask(Command::SetGrants { device_id: device.clone(), grants: cfg.grants }, REPLY)?;
    log(&format!("HOST_READY {device}"));

    let (request_id, kind) = pump.wait("the viewer's request", cfg.session_timeout, |e| match e {
        UiEvent::IncomingRequest(r) if r.device.id == device => Some((r.request_id, r.kind.clone())),
        _ => None,
    })?;
    if !matches!(kind, RequestKind::Session { .. }) {
        return Err(format!("the incoming request is not a session: {kind:?}"));
    }
    pump.ask(Command::AnswerIncoming { request_id, decision: Decision::Accept { grants: cfg.grants } }, REPLY)?;
    log(&format!("HOST_ACCEPTED request={request_id}"));

    let (session_id, granted) = pump.wait("the hosted session to go active", cfg.session_timeout, |e| match e {
        UiEvent::Session { session_id, device_id, state: SessionState::Active { role: Role::Host, granted, .. } }
            if *device_id == device =>
        {
            Some((*session_id, *granted))
        }
        _ => None,
    })?;
    out.session_active = true;
    log(&format!("HOST_SESSION_ACTIVE session={session_id} granted={:#06x}", granted.0));

    let end = watch_session(pump, cfg, log, session_id);
    match end {
        Ok(reason) => {
            out.session_ended = true;
            log(&format!("HOST_SESSION_ENDED reason={reason:?}"));
            Ok(())
        }
        Err(e) => {
            // The viewer never ended it: end it from here so nothing is left running.
            let _ = pump.request(Command::Disconnect { session_id }, REPLY);
            Err(e)
        }
    }
}

/// Until the hosted session ends: shows the viewer's chat messages (and marks them read), sends the chat text once
/// the orchestrator wrote it, and logs the session's drops and returns.
fn watch_session(
    pump: &mut Pump<'_>,
    cfg: &HostConfig,
    log: &dyn Fn(&str),
    session_id: u64,
) -> Result<Option<ApiCode>, String> {
    let deadline = Instant::now() + cfg.session_timeout;
    let mut cursor = 0;
    let mut sent = false;
    let mut reconnecting = false;
    loop {
        for e in pump.fresh(&mut cursor) {
            match e {
                UiEvent::Session { session_id: s, state, .. } if s == session_id => match state {
                    SessionState::Ended { reason } => return Ok(reason),
                    SessionState::Reconnecting { .. } if !reconnecting => {
                        reconnecting = true;
                        log("HOST_SESSION_RECONNECTING");
                    }
                    SessionState::Active { .. } if reconnecting => {
                        reconnecting = false;
                        log("HOST_SESSION_ACTIVE_AGAIN");
                    }
                    _ => {}
                },
                UiEvent::Chat { session_id: s, message } if s == session_id && !message.mine => {
                    log(&format!("HOST_CHAT_GOT {}", message.text));
                    pump.ask(Command::ChatMarkRead { session_id, up_to: message.msg_id }, REPLY)?;
                }
                UiEvent::ChatRead { session_id: s, .. } if s == session_id => log("HOST_CHAT_READ_BY_PEER"),
                _ => {}
            }
        }
        if !sent
            && let Some(file) = &cfg.chat_say
            && let Ok(text) = std::fs::read_to_string(file)
            && !text.trim().is_empty()
        {
            sent = true;
            pump.ask(Command::ChatSend { session_id, text: text.trim().to_string() }, REPLY)?;
            log("HOST_CHAT_SENT");
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out after {}s waiting for the hosted session to end",
                cfg.session_timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The ceremony as the user sees it: the code appears, it is compared with the viewer's, the verdict goes back.
fn pair(pump: &mut Pump<'_>, cfg: &HostConfig, log: &dyn Fn(&str), pairing_id: u32) -> Result<String, String> {
    let code = pump.wait("the pairing code", cfg.pair_timeout, |e| match e {
        UiEvent::Pairing { pairing_id: id, state: PairingState::ShowCode { code, .. } } if *id == pairing_id => {
            Some(Ok(code.clone()))
        }
        UiEvent::Pairing { pairing_id: id, state: PairingState::Failed { reason } } if *id == pairing_id => {
            Some(Err(format!("pairing failed: {reason:?}")))
        }
        _ => None,
    })??;
    log(&format!("HOST_CODE {code}"));
    let peer = wait_file(&cfg.peer_code, "the viewer's code", cfg.pair_timeout)?;
    let matches = peer == code;
    pump.ask(Command::PairConfirm { pairing_id, matches }, REPLY)?;
    if !matches {
        return Err(format!("the codes differ (host {code}, viewer {peer})"));
    }
    pump.wait("the pairing to finish", cfg.pair_timeout, |e| match e {
        UiEvent::Pairing { pairing_id: id, state: PairingState::Done { device_id } } if *id == pairing_id => {
            Some(Ok(device_id.clone()))
        }
        UiEvent::Pairing { pairing_id: id, state: PairingState::Failed { reason } } if *id == pairing_id => {
            Some(Err(format!("pairing failed: {reason:?}")))
        }
        _ => None,
    })?
}

/// The host's user for hand testing between machines: answers every session request of an already paired device with
/// `grants`, for as long as it runs. It pairs nothing and forgets nothing, so a pairing outlives the tool.
pub fn host_stay(backend: &dyn UiBackend, grants: Permissions, log: &dyn Fn(&str)) -> Result<(), String> {
    let mut pump = Pump::new(backend);
    log("HOST_STAY");
    loop {
        let (request_id, device, kind) =
            pump.wait("a session request", Duration::from_secs(u64::from(u32::MAX)), |e| match e {
                UiEvent::IncomingRequest(r) => Some((r.request_id, r.device.id.clone(), r.kind.clone())),
                _ => None,
            })?;
        if !matches!(kind, RequestKind::Session { .. }) {
            log(&format!("HOST_IGNORED request={request_id} kind={kind:?}"));
            continue;
        }
        pump.ask(Command::AnswerIncoming { request_id, decision: Decision::Accept { grants } }, REPLY)?;
        log(&format!("HOST_ACCEPTED request={request_id} device={device}"));
    }
}
