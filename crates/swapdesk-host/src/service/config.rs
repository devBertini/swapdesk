//! What a [`super::ServiceCore`] is built from.

use std::net::SocketAddrV4;
use std::sync::Arc;

use swapdesk_platform::{ActionRunner, Clock, NetInterfaces, TraceSink};
use swapdesk_proto::{AudioCodec, Capabilities, Codec, H264Profile, KbKind, KeyboardInfo, Os, VideoCap};
use swapdesk_session::HostUi;
use swapdesk_trust::{Identity, SystemWallClock, TrustHandle, WallClock};

/// The default session port (PROTOCOL.md 1: GUESS, configurable).
pub const DEFAULT_SESSION_PORT: u16 = 47820;
/// The default pairing port (PROTOCOL.md 1: GUESS, configurable).
pub const DEFAULT_PAIR_PORT: u16 = 47821;

/// Everything the core needs from its caller. The binary fills it from the Windows crates and the C5 store; tests
/// fill it with fakes.
pub struct ServiceConfig {
    /// This machine's name, OS and versions, as the peer sees them in `Hello` and `HelloAck`.
    pub name: String,
    pub os: Os,
    pub os_version: String,
    pub app_version: String,
    /// What this build can do (C1 `Capabilities`); the default describes H.264 decode and Opus audio.
    pub capabilities: Capabilities,
    /// The device key (C5), used in both roles.
    pub identity: Arc<Identity>,
    /// The trust store: who is pinned and what each device may do here.
    pub trust: Arc<TrustHandle>,
    /// The session endpoint's UDP port on every approved address; 0 lets the system choose (tests).
    pub session_port: u16,
    /// The pairing endpoint's port. Reserved: the pairing endpoint arrives with the pairing slice, the core does
    /// not open it yet.
    pub pair_port: u16,
    /// The monotonic clock of this machine (C3), shared with the helpers' timestamps.
    pub clock: Arc<dyn Clock>,
    /// Unix milliseconds, for the session state events.
    pub wall: Arc<dyn WallClock>,
    pub trace: Arc<dyn TraceSink>,
    /// The host's user: presence, the prompt, consent, lock on end (the service wires the UI pipe here later).
    pub ui: Arc<dyn HostUi>,
    /// The network facts for the LAN guard (C3 `NetInterfaces`).
    pub interfaces: Arc<dyn NetInterfaces>,
    /// `HelloAck.win_session` the helpers are told.
    pub win_session: u32,
    /// `StartAudio.exclude_pid` (0 = none).
    pub audio_exclude_pid: u32,
    /// Development and tests only: also listen on these addresses (the LAN guard still judges every peer).
    pub listen_extra: Vec<SocketAddrV4>,
    /// Development and tests only: accept and dial peers on loopback. The guard keeps refusing every other
    /// non-LAN peer, and the default is `false` (PROTOCOL.md 1: a loopback peer is never accepted).
    pub allow_loopback_peers: bool,
    /// The runner the service itself uses for a quick action when no user helper is attached (Windows:
    /// `WinActions::service()`: lock, sleep, restart). `None`: such an action is refused as unavailable.
    pub service_actions: Option<Box<dyn ActionRunner + Send>>,
}

/// The collaborators of a core, the part of the configuration with no sensible default.
pub struct ServiceDeps {
    pub identity: Arc<Identity>,
    pub trust: Arc<TrustHandle>,
    pub clock: Arc<dyn Clock>,
    pub trace: Arc<dyn TraceSink>,
    pub ui: Arc<dyn HostUi>,
    pub interfaces: Arc<dyn NetInterfaces>,
}

impl ServiceConfig {
    pub fn new(name: &str, os: Os, os_version: &str, deps: ServiceDeps) -> Self {
        let ServiceDeps { identity, trust, clock, trace, ui, interfaces } = deps;
        Self {
            name: name.into(),
            os,
            os_version: os_version.into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: default_capabilities(),
            identity,
            trust,
            session_port: DEFAULT_SESSION_PORT,
            pair_port: DEFAULT_PAIR_PORT,
            clock,
            wall: Arc::new(SystemWallClock),
            trace,
            ui,
            interfaces,
            win_session: 0,
            audio_exclude_pid: 0,
            listen_extra: Vec::new(),
            allow_loopback_peers: false,
            service_actions: None,
        }
    }

    /// The runner for quick actions when no user helper is attached (see [`ServiceConfig::service_actions`]).
    #[must_use]
    pub fn with_service_actions(mut self, runner: impl ActionRunner + Send + 'static) -> Self {
        self.service_actions = Some(Box::new(runner));
        self
    }

    /// Development and tests only: listen on `addr` too and accept loopback peers there.
    #[must_use]
    pub fn dev_loopback(mut self, addr: SocketAddrV4) -> Self {
        self.listen_extra.push(addr);
        self.allow_loopback_peers = true;
        self
    }
}

/// H.264 up to 4K60 and Opus; keyboard unknown until the shell says.
pub fn default_capabilities() -> Capabilities {
    Capabilities {
        video_decode: vec![VideoCap {
            codec: Codec::H264,
            profile: H264Profile::High,
            max_level: 52,
            max_width: 3840,
            max_height: 2160,
            max_fps: 60,
        }],
        audio: vec![AudioCodec::Opus48Stereo],
        max_datagram: 1200,
        keyboard: KeyboardInfo { kind: KbKind::Unknown, layout: String::new() },
        features: 0,
    }
}
