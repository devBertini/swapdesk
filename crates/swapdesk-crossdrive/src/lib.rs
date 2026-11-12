//! Development harness for the cross-machine proof of stage 23 (`scripts/mac/cross-session.sh`).
//!
//! Two scripted users, each driving a machine's UI hub through the C6 [`swapdesk_ui::UiBackend`] exactly as the
//! shells do, with no human click:
//!
//! * [`host::host_drive`], on the machine that is controlled (the installed Windows service, through its UI pipe):
//!   opens pairing, confirms the code the viewer reports, grants the viewer VIEW, CONTROL and AUDIO, accepts the
//!   incoming request, waits for the session to end and forgets the device.
//! * [`viewer::viewer_drive`], on the controlling machine (a headless Mac with its own in-process core): adds the host
//!   by address, pairs, connects with control, decodes the video it receives, sends a few small pointer moves, counts
//!   the audio datagrams, disconnects and forgets the device.
//!
//! The two sides exchange the pairing codes through their stdout markers (`HOST_CODE`, `VIEWER_CODE`) and a file each
//! (`peer_code`, `go`) that the orchestrator writes: this crate never talks to the other machine itself.
//! Everything is generic over the backend and the decoder, so it is tested on one machine with two in-process hubs.

pub mod clip;
pub mod files;
pub mod host;
pub mod pump;
pub mod viewer;

pub use host::{HostConfig, host_drive, host_stay};
pub use pump::{Pump, wait_file};
pub use viewer::{ClipStep, DecoderFactory, Media, ViewerConfig, ViewerReport, viewer_drive};
