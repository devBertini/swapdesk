//! Windows system glue of Swapdesk (contracts C2.1, C2.3, C2.4 "Development mode", C3): named pipes with explicit
//! DACLs and the local-client checks, helper launch, the `SessionWatcher`, service control, keep-awake, `SendSAS`
//! and the file trace log. Windows only; this is one of the crates that may use `unsafe` (every block states what it relies on).
//! Stage 20 adds `ActionRunner` and the ConPTY spawner here.
#![cfg(windows)]

mod acl;
pub mod actions;
pub mod desktop;
pub mod dirwatch;
pub mod disk;
pub mod elevate;
mod env;
pub mod fs;
pub mod http;
pub mod instance;
pub mod launch;
pub mod login;
pub mod panic_hook;
pub mod pipe;
pub mod power;
mod privileges;
pub mod pty;
pub mod sas;
pub mod service;
mod sys;
pub mod trace;
pub mod udp;
pub mod update_apply;
pub mod watcher;

pub use acl::AceInfo;
pub use desktop::{classify_input_desktop, input_desktop, input_desktop_name};
pub use dirwatch::DirWatch;
pub use elevate::{ElevatedProcess, RunAsError, WaitOutcome, run_as_admin};
pub use fs::free_bytes;
pub use launch::{
    ChildProcess, HelperRole, InstallError, InstallPolicy, RestartBackoff, TRUSTED_INSTALLER_SID, UserTokenChoice,
    VerifiedExe, active_console_session, build_command_line, check_no_other_writers, choose_user_token, launch_helper,
    launch_helper_dev, quote_arg, session_user_sid,
};
pub use panic_hook::{CRASH_MARKER, CRASH_SEEN, install_panic_hook, take_crash_marker, user_log_dir};
pub use pipe::{
    ClientIdentity, PIPE_PREFIX, PipeAcl, PipeConn, PipeError, PipeListener, PipeOwner, PipeReader, PipeSpec,
    PipeWriter, connect, helper_pipe_name, random_hex_128, session_of_pid, ui_pipe_name,
};
pub use power::KeepAwake;
pub use sas::{policy_lets_services_send, sas_available, sas_policy_allows_service, sas_policy_value, send_sas};
pub use service::{ServiceContext, ServiceError, ServiceStatus, StopSignal, query_service, run_service, service_pid};
pub use sys::{account_name_of_sid, current_account_is_administrator, current_user_sid};
pub use trace::FileTrace;
pub use watcher::{
    SessionEventSender, WinSessionWatcher, WindowWatcher, console_state, session_channel, session_user_name,
};

#[cfg(test)]
mod tests;
