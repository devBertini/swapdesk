//! Session layer of Swapdesk (PROTOCOL.md 4 to 6, contract C3 and the `swapdesk-session` row of CONTRACTS.md):
//! the control-stream state machine of a host and of a viewer, permission enforcement, the decision with its
//! 20 s auto-decline, resume and swap tokens, the liveness probe, caps per session, and the [`ChannelHandler`]
//! registry that the feature crates (swap, terminal, files, extras) plug into.
//!
//! * [`host::HostService`] owns the live sessions of a machine as a host. It is given the trust store, a
//!   [`host::HostUi`] (presence, the prompt, consent, lock-on-end) and the registry, and serves each accepted,
//!   gated connection with [`host::HostService::serve`].
//! * [`viewer::ViewerSession`] opens a session as a viewer and reports what happens in it.
//! * [`ctx::SessionCtx`] is what a handler sees of its session: grants, pause state, sending, gated tasks.
//!
//! No `unsafe`. Every wait has a timer; time is tokio's, so a test can pause it.

// Outside tests an unwrap is a crash of the service: return the error or use `expect` with the reason.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

mod actor;
pub mod caps;
pub mod ctx;
pub mod forward;
mod grants_sync;
pub mod host;
pub mod link;
pub mod liveness;
pub mod machine;
pub mod policy;
pub mod registry;
pub mod resume;
mod router;
mod util;
pub mod viewer;

pub use caps::{CapTracker, CapVerdict, Caps};
pub use ctx::{DesktopState, EndKind, SessionCtx, SessionEnd};
pub use forward::{ForwardHandler, Forwarded, VIDEO_CONTROL_TYPES};
pub use host::{
    ConsentRequest, HostConfig, HostEvent, HostService, HostUi, PromptAnswer, PromptEnd, PromptRequest, SessionInfo,
    StartInfo,
};
pub use link::{CONTROL_PRIORITY, ControlIo, Inbound, LinkError, Outbox, SendError, memory_pair};
pub use liveness::{Liveness, LivenessConfig, Verdict};
pub use machine::MachineHosting;
pub use policy::{Role, priority_of};
pub use registry::{ChannelHandler, IncomingStream, RegisterError, Registry};
pub use resume::{ResumeError, ResumeTable, SwapTokens};
pub use util::random_token;
pub use viewer::{GrantInfo, OpenError, ViewerConfig, ViewerEvent, ViewerSession};
