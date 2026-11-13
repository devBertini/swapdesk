//! The quick actions proof across machines (F-XTERM-01, F-ACT-01), the viewer's half.
//!
//! Two checks, neither of which may disturb the host:
//!
//! * **The list the viewer offers toward that OS.** A `UiState` is fed the device list and the active viewer session
//!   exactly as the shells feed theirs, and its quick action buttons ([`swapdesk_ui::UiState::quick_actions`]) must be
//!   Lock, the task monitor under the host system's own name, Sleep and Restart, in that order. Logged as
//!   `ACTIONS_LIST_OK`.
//! * **One action run for real, where it is harmless.** Only the task monitor is ever run, never lock, sleep or
//!   restart, and only toward a host where it opens without a question: Activity Monitor on a Mac. Task Manager asks
//!   Windows for an elevation (a User Account Control prompt on the host's screen), so toward a Windows host the run
//!   is skipped and said so (`ACTIONS_RUN_SKIPPED`). The host audits the run; `host.rs` reads its audit file when it
//!   has a folder for it (`HOST_AUDIT`) and the orchestrator puts the two together as `ACTIONS_OK`.

use std::time::Duration;

use swapdesk_ui::strings::text;
use swapdesk_ui::{
    ActionKind, Command, ElevationLevel, Language, Msg, MsgKey, Os, Permissions, Reply, Role, SessionState, UiEvent,
    UiState,
};

use crate::pump::Pump;

const REPLY: Duration = Duration::from_secs(10);

/// The four quick actions in the order they are offered, with the key of the label that fits the host's system.
pub fn expected_list(host_os: Os) -> [(ActionKind, MsgKey); 4] {
    let task = match host_os {
        Os::Windows => MsgKey::ActTaskMonitorWindows,
        Os::MacOs => MsgKey::ActTaskMonitorMac,
    };
    [
        (ActionKind::Lock, MsgKey::ActLock),
        (ActionKind::TaskMonitor, task),
        (ActionKind::Sleep, MsgKey::ActSleep),
        (ActionKind::Restart, MsgKey::ActRestart),
    ]
}

/// Whether the harmless run is done toward a host of this system.
pub fn runs_task_monitor(host_os: Os) -> bool {
    host_os == Os::MacOs
}

/// Runs the checks on viewer session `session_id` toward `device_id`, a host of `host_os`.
pub fn actions_round(
    pump: &mut Pump<'_>,
    session_id: u64,
    device_id: &str,
    host_os: Os,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let Reply::Devices(devices) = pump.ask(Command::ListDevices, REPLY)? else {
        return Err("ListDevices did not reply the devices".into());
    };
    let mut state = UiState::new(Language::En, false);
    state.apply(UiEvent::DevicesChanged(devices));
    state.apply(UiEvent::Session {
        session_id,
        device_id: device_id.to_string(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE,
            elevation: ElevationLevel::Unavailable,
        },
    });
    let offered = state.quick_actions();
    let want = expected_list(host_os);
    if offered != want {
        return Err(format!("the viewer offers {offered:?} toward a {host_os:?} host, expected {want:?}"));
    }
    let names: Vec<String> = offered.iter().map(|(_, k)| text(Language::En, &Msg::new(*k))).collect();
    log(&format!("ACTIONS_LIST_OK host_os={host_os:?} offered={}", names.join(" | ")));

    if !runs_task_monitor(host_os) {
        log(
            "ACTIONS_RUN_SKIPPED Task Manager asks Windows for an elevation (a UAC prompt on the host's screen); lock, sleep and restart are never run",
        );
        return Ok(());
    }
    pump.ask(Command::RunAction { session_id, action: ActionKind::TaskMonitor }, REPLY)?;
    log("ACTIONS_RAN action=TaskMonitor");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_the_four_actions_with_the_hosts_own_task_monitor() {
        let mac = expected_list(Os::MacOs);
        let win = expected_list(Os::Windows);
        assert_eq!(
            mac.map(|(a, _)| a),
            [ActionKind::Lock, ActionKind::TaskMonitor, ActionKind::Sleep, ActionKind::Restart]
        );
        assert_eq!(text(Language::En, &Msg::new(mac[1].1)), "Activity Monitor");
        assert_eq!(text(Language::En, &Msg::new(win[1].1)), "Task Manager");
    }

    #[test]
    fn only_the_task_monitor_toward_a_mac_ever_runs() {
        assert!(runs_task_monitor(Os::MacOs));
        assert!(!runs_task_monitor(Os::Windows));
    }
}
