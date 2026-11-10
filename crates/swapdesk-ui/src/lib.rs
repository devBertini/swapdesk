//! `swapdesk-ui`: everything the native shells must agree on (contract C6, no `unsafe`).
//!
//! The commands and events of the in-process UI API, the view state they change, the strings (pt-BR, en), the design
//! tokens, the accessibility tree of each window and the mock backend. The shells (`swapdesk-shell-win`, later
//! `swapdesk-shell-mac`) only draw this state and wire the OS events to it.
//!
//! Responsiveness rules (SPEC, D20) live here where they are about state: placeholders after 100 ms
//! (`state::Fetch`), optimistic changes with undo, merged in-flight commands (`inflight`), cached data, the 150 ms
//! debounce (`debounce`), page-wise lists (`list`), parallel start (`UiState::startup_commands`), timeouts
//! (`UiState::tick`) and the slow-work trace (`trace`).

pub mod a11y;
pub mod api;
pub mod debounce;
pub mod elevated_client;
pub mod format;
pub mod home;
pub mod inflight;
pub mod interact;
pub mod list;
pub mod mock;
pub mod prefs;
pub mod service_backend;
pub mod shortcuts;
pub mod state;
pub mod strings;
pub mod tokens;
pub mod trace;

#[cfg(test)]
mod a11y_tests;
#[cfg(test)]
mod notice_tests;
#[cfg(test)]
mod shortcut_tests;
#[cfg(test)]
mod state_tests;

pub use api::*;
pub use interact::{ControlId, ControlSpec, DialogTexts, FILES_PAGE, Region, UiRole};
pub use shortcuts::{ReleaseKey, ViewerAction, ViewerOs};
pub use state::{
    Activation, ButtonSide, Changed, ConnectFlow, ConnectPhase, DataKind, DeviceTab, DialogKind, DrawerTab, EditId,
    Effect, Fetch, Nav, NoticeKind, RenameDraft, Screen, SelectId, SwapPhase, TermPhase, UiState, WaitBar, WaitButton,
    WaitCard, WaitIcon, WaitPhase, WindowKind,
};
pub use strings::MsgKey;
