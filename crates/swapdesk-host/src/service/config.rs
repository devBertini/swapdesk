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
    /// This machine has a hardware HEVC encoder (the probe's answer, `swapdesk_codec_win::hevc_support().0`); with it a
    /// session may be encoded in HEVC (F-VIEW-07, [`session_codec`]). Default `false`: H.264 only.
    pub host_hevc_hardware: bool,
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
    /// The protocol minor the session endpoint announces: this build's own. Tests play a host of 1.0 with 0.
    pub proto_minor: u16,
    /// What this core shares with the cores of the machine's other accounts (ACCOUNTS.md 5.6); `None`: its own.
    pub machine: Option<super::MachineShared>,
    /// Whether it listens on the approved addresses from the start (an account without pairings does not; the hub
    /// turns it on with `ServiceCore::set_receiving`).
    pub receiving: bool,
    /// ACCOUNTS.md 5.5: binds the session port on the wildcard address, exclusively (Windows: `SO_EXCLUSIVEADDRUSE`),
    /// once at start and whether or not the account receives yet, so no other local account can take it. The guard
    /// still refuses any connection whose arrival address is not approved. `None`: one socket per approved address.
    pub wildcard: Option<WildcardBinder>,
}

/// Binds the session port on the wildcard address (see [`ServiceConfig::wildcard`]).
pub type WildcardBinder = Arc<dyn Fn(u16) -> std::io::Result<std::net::UdpSocket> + Send + Sync>;

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
            host_hevc_hardware: false,
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
            proto_minor: swapdesk_proto::PROTO_MINOR,
            machine: None,
            receiving: true,
            wildcard: None,
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

/// H.264 up to 4K60 and Opus; keyboard unknown until the shell says. No HEVC: see [`capabilities_with_hevc`].
pub fn default_capabilities() -> Capabilities {
    capabilities_with_hevc(false)
}

/// [`default_capabilities`], with an HEVC decode entry when `hevc_decode_hardware` says this machine's hardware decodes
/// HEVC (PROTOCOL.md 5.3: probed at start, absent otherwise; the caller passes `swapdesk_codec_win::hevc_support().1`).
/// The entry reads as HEVC Main, level 5.1 (`general_level_idc` 153, listed as 153 / 3 = 51: 3840x2160 at 60 fps fits
/// level 5.1), up to 4K60 like the H.264 entry. A host sends it to a minor-0 peer through `Capabilities::for_minor`,
/// which leaves the entry out.
pub fn capabilities_with_hevc(hevc_decode_hardware: bool) -> Capabilities {
    let mut video_decode = vec![VideoCap {
        codec: Codec::H264,
        profile: H264Profile::High,
        max_level: 52,
        max_width: 3840,
        max_height: 2160,
        max_fps: 60,
    }];
    if hevc_decode_hardware {
        video_decode.push(VideoCap {
            codec: Codec::Hevc,
            profile: H264Profile::Main,
            max_level: 51,
            max_width: 3840,
            max_height: 2160,
            max_fps: 60,
        });
    }
    Capabilities {
        video_decode,
        audio: vec![AudioCodec::Opus48Stereo],
        max_datagram: 1200,
        keyboard: KeyboardInfo { kind: KbKind::Unknown, layout: String::new() },
        features: 0,
    }
}

/// F-VIEW-07, the host's side: the codec a session's video is encoded in. HEVC only when the viewer's `Capabilities`
/// (`None`: not received) list an HEVC decoder, the viewer announced a minor that knows it, and `host_hevc_hardware` says
/// this machine has a hardware HEVC encoder; H.264 in every other case.
pub fn session_codec(viewer_caps: Option<&Capabilities>, viewer_minor: u16, host_hevc_hardware: bool) -> Codec {
    viewer_caps.map_or(Codec::H264, |caps| caps.pick_codec(viewer_minor, host_hevc_hardware))
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn the_default_capabilities_advertise_no_hevc() {
        assert!(default_capabilities().video_decode.iter().all(|c| c.codec == Codec::H264));
        assert_eq!(default_capabilities(), capabilities_with_hevc(false));
    }

    #[test]
    fn hevc_is_advertised_only_when_the_hardware_decodes_it() {
        let with = capabilities_with_hevc(true);
        let hevc: Vec<_> = with.video_decode.iter().filter(|c| c.codec == Codec::Hevc).collect();
        assert_eq!(hevc.len(), 1);
        let h = hevc[0];
        assert_eq!((h.profile, h.max_level, h.max_width, h.max_height), (H264Profile::Main, 51, 3840, 2160));
        assert!(with.video_decode.iter().any(|c| c.codec == Codec::H264), "H.264 stays listed");
        assert!(capabilities_with_hevc(false).video_decode.iter().all(|c| c.codec != Codec::Hevc));
    }

    #[test]
    fn a_minor_zero_peer_is_not_told_about_hevc() {
        let told = capabilities_with_hevc(true).for_minor(0);
        assert!(told.video_decode.iter().all(|c| c.codec == Codec::H264));
    }

    #[test]
    fn the_session_codec_is_hevc_only_when_both_machines_and_the_minor_allow_it() {
        let hevc = capabilities_with_hevc(true);
        let h264_only = capabilities_with_hevc(false);
        assert_eq!(session_codec(Some(&hevc), 1, true), Codec::Hevc);
        assert_eq!(session_codec(Some(&hevc), 1, false), Codec::H264, "no hardware encoder on the host");
        assert_eq!(session_codec(Some(&h264_only), 1, true), Codec::H264, "the viewer does not decode HEVC");
        assert_eq!(session_codec(Some(&hevc), 0, true), Codec::H264, "a minor-0 viewer never gets HEVC");
        assert_eq!(session_codec(None, 1, true), Codec::H264, "capabilities not received yet");
    }
}
