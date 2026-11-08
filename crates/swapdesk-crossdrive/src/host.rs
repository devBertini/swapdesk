//! The scripted user of the machine being controlled.
//!
//! Markers printed through `log`, read by the orchestrator: `HOST_PAIR_OPEN`, `HOST_CODE <code>`, `HOST_READY <id>`
//! (the viewer may connect now), `HOST_SESSION_ACTIVE`, `HOST_SESSION_ENDED`, `HOST_FORGOTTEN`.

use std::path::PathBuf;
use std::time::Duration;

use swapdesk_ui::{
    Command, Decision, PairingState, Permissions, Reply, RequestKind, Role, SessionState, UiBackend, UiEvent,
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

    let end = pump.wait("the hosted session to end", cfg.session_timeout, |e| match e {
        UiEvent::Session { session_id: s, state: SessionState::Ended { reason }, .. } if *s == session_id => {
            Some(*reason)
        }
        _ => None,
    });
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
