//! The scripted user of the machine being controlled.
//!
//! Markers printed through `log`, read by the orchestrator: `HOST_PAIR_OPEN`, `HOST_CODE <code>`, `HOST_READY <id>`
//! (the viewer may connect now), `HOST_SESSION_ACTIVE`, `HOST_SESSION_ENDED`, `HOST_FORGOTTEN`.
//!
//! While the session is active the host's user also answers the chat proof ([`crate::chat`]): `HOST_CHAT_GOT <text>`
//! is what its interface received (the message is then marked read), `HOST_CHAT_SENT` follows the sending of the text
//! in the file [`HostConfig::chat_say`], `HOST_CHAT_READ_BY_PEER` is the viewer's read state; and it logs the session
//! going `HOST_SESSION_RECONNECTING` and `HOST_SESSION_ACTIVE_AGAIN` for the reconnect proof.
//!
//! With [`HostConfig::swap`] it accepts the swap the viewer asks for (`HOST_SWAP_ASKED`, `HOST_SWAP_ACCEPTED`), times
//! the reverse session it then opens as the viewer (`HOST_SWAP_VIEWING ... after_accept_ms=`, from the moment its
//! answer was sent to the new session being active here), notes any pairing or request that came up meanwhile
//! (`HOST_SWAP_PROMPT_SEEN`) and stays until the reverse session ends (`HOST_SWAP_REVERSE_ENDED`). With
//! [`HostConfig::audit`] it reads the host's own audit file once the session is over: `HOST_AUDIT terminal_opened=N
//! actions=[..] swapped=N` (the terminal and the quick action of the terminal proof leave their lines there).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use swapdesk_trust::{AuditEvent, decode_log};
use swapdesk_ui::{
    ApiCode, Command, Decision, DeviceStatus, PairingState, Permissions, Reply, RequestKind, Role, SessionState,
    SwapEvent, UiBackend, UiEvent,
};

use crate::pump::{Pump, wait_file};

const REPLY: Duration = Duration::from_secs(10);

/// How long after a lost session the host's resume token works (PROTOCOL.md 6.4), with a margin for the poll.
pub const RESUME_WINDOW: Duration = Duration::from_secs(65);

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
    /// Swap proof: accept the swap the viewer asks for and watch the reverse session ([`crate::swap`]).
    pub swap: bool,
    /// Reconnect proof: a session that ends as lost (the host's interface reports `Ended` with the timeout reason once
    /// the link has been silent for QUIC's idle timeout) is not the end: the script waits [`RESUME_WINDOW`] for the
    /// viewer to take the same session back (`HOST_SESSION_LOST`, then `HOST_SESSION_ACTIVE_AGAIN`) and only a
    /// final end (a `Bye`) finishes it. Without the switch the first `Ended` finishes the script, and the device is
    /// forgotten at once, which kills the resume token.
    pub reconnect: bool,
    /// Terminal and quick action proof: where to read this host's own audit file once the session is over.
    pub audit: Option<AuditCheck>,
}

/// Where the host's audit files are, and whether a quick action is expected in them.
pub struct AuditCheck {
    /// The folder of the host's data (`audit-sessions.log` is looked for in it and up to four levels below).
    pub dir: PathBuf,
    /// A quick action ran: wait for its line (up to a few seconds) before reporting.
    pub wait_action: bool,
}

/// What the audit files hold from a moment on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AuditCounts {
    pub terminal_opened: u32,
    pub actions: Vec<String>,
    pub swapped: u32,
}

fn audit_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                audit_files(&path, depth - 1, out);
            }
        } else if path.file_name().is_some_and(|n| n == "audit-sessions.log") {
            out.push(path);
        }
    }
}

/// Reads the host's audit files under `dir` and counts the terminal, quick action and swap lines from `since_ms` on.
pub fn audit_counts(dir: &Path, since_ms: u64) -> Result<AuditCounts, String> {
    let mut files = Vec::new();
    audit_files(dir, 4, &mut files);
    if files.is_empty() {
        return Err(format!("no audit-sessions.log under {}", dir.display()));
    }
    let mut counts = AuditCounts::default();
    for file in files {
        let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let read = decode_log(&bytes).map_err(|e| format!("{}: {e:?}", file.display()))?;
        for e in read.entries.into_iter().filter(|e| e.time_ms >= since_ms) {
            match e.event {
                AuditEvent::TerminalOpened { .. } => counts.terminal_opened += 1,
                AuditEvent::ActionRun(a) => counts.actions.push(format!("{a:?}")),
                AuditEvent::Swapped => counts.swapped += 1,
                _ => {}
            }
        }
    }
    Ok(counts)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// Logs `HOST_AUDIT` from the audit files, waiting a little for the writer when a line is still missing.
fn report_audit(check: &AuditCheck, since_ms: u64, log: &dyn Fn(&str)) {
    let end = Instant::now() + Duration::from_secs(4);
    loop {
        let late = Instant::now() >= end;
        match audit_counts(&check.dir, since_ms) {
            Ok(c) if late || (c.terminal_opened > 0 && (!check.wait_action || !c.actions.is_empty())) => {
                log(&format!(
                    "HOST_AUDIT terminal_opened={} actions=[{}] swapped={}",
                    c.terminal_opened,
                    c.actions.join(","),
                    c.swapped
                ));
                return;
            }
            Err(e) if late => {
                log(&format!("HOST_AUDIT_FAILED {e}"));
                return;
            }
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// A swap this host accepted: when, which session is the reverse one, and whether a prompt came up meanwhile.
struct Swap {
    accepted_at: Instant,
    reverse: Option<u64>,
    prompted: bool,
    old_ended: bool,
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
    let started_ms = now_ms();
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

    let end = watch_session(pump, cfg, log, session_id, &device);
    match end {
        Ok(reason) => {
            out.session_ended = true;
            log(&format!("HOST_SESSION_ENDED reason={reason:?}"));
            if let Some(check) = &cfg.audit {
                report_audit(check, started_ms, log);
            }
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
    device: &str,
) -> Result<Option<ApiCode>, String> {
    let deadline = Instant::now() + cfg.session_timeout;
    let mut cursor = 0;
    let mut sent = false;
    let mut reconnecting = false;
    // Reconnect proof: when the session ended as lost, and so how long the viewer has to take it back.
    let mut lost_at = None::<Instant>;
    let mut swap = None::<Swap>;
    loop {
        for e in pump.fresh(&mut cursor) {
            match e {
                // The viewer asks for a swap: the answer is a yes, and the clock for the reverse session starts.
                UiEvent::Swap { event: SwapEvent::Asked { request_id, by, .. }, .. } if cfg.swap && swap.is_none() => {
                    log(&format!("HOST_SWAP_ASKED request={request_id} by={by}"));
                    // The reverse connection is dialled to the address the other side's announcement gave: give the
                    // announcement a few seconds to reach this machine (it follows the pairing), and say what was seen.
                    let wait_end = Instant::now() + Duration::from_secs(8);
                    let status = loop {
                        let status = match pump.ask(Command::ListDevices, REPLY) {
                            Ok(Reply::Devices(list)) => list.iter().find(|d| d.id == device).map(|d| d.status.clone()),
                            _ => None,
                        };
                        if matches!(status, Some(DeviceStatus::Online { .. })) || Instant::now() >= wait_end {
                            break status;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    };
                    log(&format!("HOST_SWAP_PEER_STATUS {status:?}"));
                    let accepted_at = Instant::now();
                    pump.ask(Command::AnswerSwap { request_id, accept: true }, REPLY)?;
                    log("HOST_SWAP_ACCEPTED");
                    swap = Some(Swap { accepted_at, reverse: None, prompted: false, old_ended: false });
                }
                UiEvent::Session {
                    session_id: s,
                    device_id,
                    state: SessionState::Active { role: Role::Viewer, .. },
                } if s != session_id && device_id == device => {
                    if let Some(sw) = swap.as_mut().filter(|sw| sw.reverse.is_none()) {
                        sw.reverse = Some(s);
                        log(&format!(
                            "HOST_SWAP_VIEWING new_session={s} after_accept_ms={}",
                            sw.accepted_at.elapsed().as_millis()
                        ));
                    }
                }
                UiEvent::Pairing { .. } | UiEvent::IncomingRequest(_) if swap.is_some() => {
                    if let Some(sw) = swap.as_mut() {
                        sw.prompted = true;
                    }
                    log("HOST_SWAP_PROMPT_SEEN a pairing or a request came up during the swap");
                }
                UiEvent::Session { session_id: s, state: SessionState::Ended { reason }, .. }
                    if swap.as_ref().is_some_and(|sw| sw.reverse == Some(s)) =>
                {
                    let (prompted, old_ended) = swap.as_ref().map_or((false, false), |sw| (sw.prompted, sw.old_ended));
                    log(&format!(
                        "HOST_SWAP_REVERSE_ENDED reason={reason:?} no_prompt={} old_ended={old_ended}",
                        !prompted
                    ));
                    return Ok(reason);
                }
                UiEvent::Session { session_id: s, state, .. } if s == session_id => match state {
                    SessionState::Ended { reason } => match swap.as_mut() {
                        // The swap closes the old session first: the reverse one is what is left to watch.
                        Some(sw) => {
                            sw.old_ended = true;
                            log(&format!("HOST_SWAP_OLD_SESSION_ENDED reason={reason:?}"));
                        }
                        // A lost link: the host's interface says the session ended, but its token lives on.
                        None if cfg.reconnect && lost_at.is_none() && reason == Some(ApiCode::Timeout) => {
                            lost_at = Some(Instant::now());
                            log("HOST_SESSION_LOST");
                        }
                        None => return Ok(reason),
                    },
                    SessionState::Reconnecting { .. } if !reconnecting => {
                        reconnecting = true;
                        log("HOST_SESSION_RECONNECTING");
                    }
                    SessionState::Active { .. } if reconnecting || lost_at.is_some() => {
                        reconnecting = false;
                        lost_at = None;
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
        if let Some(lost) = lost_at
            && lost.elapsed() >= RESUME_WINDOW
        {
            return Err(format!("the viewer did not take the lost session back within {}s", RESUME_WINDOW.as_secs()));
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use swapdesk_ui::Ticket;

    use super::*;

    /// A backend that plays a list of event batches, one per `drain`.
    struct Played(Mutex<VecDeque<Vec<UiEvent>>>);

    impl UiBackend for Played {
        fn send(&self, _command: Command) -> Ticket {
            Ticket(0)
        }
        fn drain(&self) -> Vec<UiEvent> {
            self.0.lock().unwrap().pop_front().unwrap_or_default()
        }
    }

    fn session(state: SessionState) -> Vec<UiEvent> {
        vec![UiEvent::Session { session_id: 7, device_id: "peer".into(), state }]
    }

    fn active() -> SessionState {
        SessionState::Active {
            since_ms: 1,
            role: Role::Host,
            granted: Permissions::NONE,
            elevation: swapdesk_ui::ElevationLevel::Unavailable,
        }
    }

    /// What a host's interface shows when the viewer's link drops and comes back (seen on the Mac, 13/11): the silent
    /// link first reads `Reconnecting`, then, once QUIC's idle timeout has passed, the session `Ended` with the timeout
    /// reason although its resume token lives on; the viewer's resume then makes the same session `Active` again.
    fn drop_and_return() -> Played {
        Played(Mutex::new(VecDeque::from([
            session(SessionState::Reconnecting { attempt: 0 }),
            session(SessionState::Ended { reason: Some(ApiCode::Timeout) }),
            session(active()),
            session(SessionState::Ended { reason: None }),
        ])))
    }

    fn watch(backend: &Played, reconnect: bool) -> (Result<Option<ApiCode>, String>, Vec<String>) {
        let cfg = HostConfig {
            peer_code: PathBuf::new(),
            pair_timeout: Duration::from_secs(1),
            session_timeout: Duration::from_secs(5),
            grants: Permissions::NONE,
            chat_say: None,
            swap: false,
            reconnect,
            audit: None,
        };
        let lines = Mutex::new(Vec::new());
        let log = |l: &str| lines.lock().unwrap().push(l.to_string());
        let mut pump = Pump::new(backend);
        let end = watch_session(&mut pump, &cfg, &log, 7, "peer");
        (end, lines.into_inner().unwrap())
    }

    #[test]
    fn without_the_switch_a_lost_session_is_the_end_of_the_script() {
        let (end, lines) = watch(&drop_and_return(), false);
        assert_eq!(end, Ok(Some(ApiCode::Timeout)));
        assert_eq!(lines, ["HOST_SESSION_RECONNECTING"]);
    }

    #[test]
    fn with_the_reconnect_switch_a_lost_session_is_waited_out_until_the_viewer_takes_it_back() {
        let (end, lines) = watch(&drop_and_return(), true);
        assert_eq!(end, Ok(None), "the end that finishes the script is the viewer's own");
        assert_eq!(lines, ["HOST_SESSION_RECONNECTING", "HOST_SESSION_LOST", "HOST_SESSION_ACTIVE_AGAIN"]);
    }

    #[test]
    fn a_final_end_during_the_reconnect_proof_still_finishes_the_script() {
        let bye = Played(Mutex::new(VecDeque::from([session(SessionState::Ended { reason: Some(ApiCode::Refused) })])));
        assert_eq!(watch(&bye, true).0, Ok(Some(ApiCode::Refused)));
    }
}
