//! Wiring of the macOS app's in-process `ServiceCore` (D27), the Mac counterpart of what
//! `swapdesk-service/src/app.rs` does on Windows: open the identity and the trust store over a `SecretStore`, read the
//! machine settings, and assemble the `ServiceConfig` the core starts from.
//!
//! * [`mac_service_config`] is generic over the C3 traits, so it builds and is unit-tested on any host with the fakes.
//! * `mac_login_service_config` (macOS only) fills it with the real thing: the user's login keychain
//!   (`swapdesk-secret-mac`) and the physical Wi-Fi/Ethernet interface list (`swapdesk-net-mac`).
//!
//! Why a crate of its own: `swapdesk-net-mac` and `swapdesk-secret-mac` hold the Apple `unsafe` and depend on nothing but
//! `swapdesk-platform`, which keeps them checkable from the Windows PC; the `ServiceConfig` lives in `swapdesk-host`
//! (QUIC, `ring`), which only the Mac can build for macOS. This crate holds no `unsafe` and is the only place that knows
//! both sides.
//!
//! Stage 24 (the root daemon) calls the same function with a System-keychain `SecretStore` and keeps everything else.

use std::net::SocketAddrV4;
use std::sync::Arc;

use swapdesk_host::service::{ServiceConfig, ServiceDeps};
use swapdesk_host::ui_hub::{MachineSettings, load_settings};
use swapdesk_platform::{Clock, NetInterfaces, SecretStore, TraceSink};
use swapdesk_proto::Os;
use swapdesk_session::HostUi;
use swapdesk_trust::{Identity, SystemWallClock, TrustHandle, WallClock};

/// What the builder needs from its caller.
pub struct MacServiceParams {
    /// This machine's default name, used until a `settings.bin` says otherwise ("MacBook de Fulano").
    pub default_name: String,
    /// The macOS version the peer sees in `Hello` ("26.0").
    pub os_version: String,
    /// Where `identity.bin`, `trust.bin` and `settings.bin` live (C5).
    pub secrets: Arc<dyn SecretStore>,
    /// The network facts for the LAN guard (C3): `MacNetInterfaces` in the app.
    pub interfaces: Arc<dyn NetInterfaces>,
    /// The machine's monotonic clock, shared with whatever timestamps frames (C3). The same instance the capture side uses.
    pub clock: Arc<dyn Clock>,
    pub trace: Arc<dyn TraceSink>,
    /// The host's user: presence, the prompt, consent, lock on end (the UI hub's `host_ui()` in the app).
    pub ui: Arc<dyn HostUi>,
    /// Development and tests only: also listen on this address and accept loopback peers (`ServiceConfig::dev_loopback`).
    pub dev_loopback: Option<SocketAddrV4>,
    /// Development and tests only: override the session and pairing ports of `settings.bin` (0 lets the system choose).
    pub ports: Option<(u16, u16)>,
}

/// What the app needs back: the config to start the core with, and the handles it shares with the UI hub.
pub struct MacServiceSetup {
    /// Start the core with this; the app may set `config.capabilities.keyboard` and `config.audio_exclude_pid` first.
    pub config: ServiceConfig,
    pub identity: Arc<Identity>,
    pub trust: Arc<TrustHandle>,
    /// The machine settings as stored (or the defaults).
    pub settings: MachineSettings,
}

/// Opens the identity (creating it on first run), the trust store and the settings from `p.secrets`, and builds the
/// `ServiceConfig` for a Mac: `Os::MacOs`, the settings' name and ports (or `p.ports`), the dev loopback address when asked.
pub fn mac_service_config(p: MacServiceParams) -> Result<MacServiceSetup, String> {
    let MacServiceParams { default_name, os_version, secrets, interfaces, clock, trace, ui, dev_loopback, ports } = p;
    let identity =
        Arc::new(Identity::load_or_create(&*secrets, SystemWallClock.unix_ms()).map_err(|e| format!("identity: {e}"))?);
    let trust = TrustHandle::open(Arc::clone(&secrets)).map_err(|e| format!("trust store: {e}"))?;
    let mut settings = load_settings(&*secrets, &default_name)?;
    if let Some((session, pair)) = ports {
        settings.port = session;
        settings.pair_port = pair;
    }
    let deps = ServiceDeps { identity: Arc::clone(&identity), trust: Arc::clone(&trust), clock, trace, ui, interfaces };
    let mut config = ServiceConfig::new(&settings.name, Os::MacOs, &os_version, deps);
    config.session_port = settings.port;
    config.pair_port = settings.pair_port;
    if let Some(addr) = dev_loopback {
        config = config.dev_loopback(addr);
    }
    Ok(MacServiceSetup { config, identity, trust, settings })
}

/// The macOS version string ("15.1", "27.2") from `sw_vers -productVersion`; "macOS" when it cannot be read.
#[cfg(target_os = "macos")]
pub fn macos_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "macOS".to_string())
}

/// Stage 23: [`mac_service_config`] over the real Mac: secrets in the logged-in user's login keychain with the C5 folder
/// under `~/Library/Application Support/Swapdesk`, the physical-interface list of `swapdesk-net-mac`, and the macOS
/// version from [`macos_version`]. Returns the store as well: the app puts its audit and trace logs in `store.dir()`.
#[cfg(target_os = "macos")]
pub fn mac_login_service_config(
    default_name: &str,
    clock: Arc<dyn Clock>,
    trace: Arc<dyn TraceSink>,
    ui: Arc<dyn HostUi>,
) -> Result<(MacServiceSetup, Arc<swapdesk_secret_mac::MacSecretStore>), String> {
    let home = std::env::var_os("HOME").ok_or("$HOME is not set")?;
    let store = Arc::new(
        swapdesk_secret_mac::MacSecretStore::open(&swapdesk_secret_mac::SecretLocation::login(std::path::Path::new(
            &home,
        )))
        .map_err(|e| format!("secret store: {e}"))?,
    );
    let setup = mac_service_config(MacServiceParams {
        default_name: default_name.to_string(),
        os_version: macos_version(),
        secrets: store.clone(),
        interfaces: Arc::new(swapdesk_net_mac::MacNetInterfaces),
        clock,
        trace,
        ui,
        dev_loopback: None,
        ports: None,
    })?;
    Ok((setup, store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use swapdesk_fakes::{FakeNetInterfaces, LogCapture, MemorySecretStore, MonotonicClock};
    use swapdesk_session::{ConsentRequest, PromptAnswer, PromptRequest};
    use tokio::sync::oneshot;

    struct NoUi;

    impl HostUi for NoUi {
        fn somebody_present(&self) -> bool {
            false
        }
        fn prompt(&self, _request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
            oneshot::channel().1
        }
        fn consent(&self, _request: ConsentRequest) -> oneshot::Receiver<bool> {
            oneshot::channel().1
        }
    }

    fn params(secrets: Arc<dyn SecretStore>) -> MacServiceParams {
        MacServiceParams {
            default_name: "Test Mac".into(),
            os_version: "27.2".into(),
            secrets,
            interfaces: Arc::new(FakeNetInterfaces::new(Vec::new())),
            clock: Arc::new(MonotonicClock::new()),
            trace: Arc::new(LogCapture::new()),
            ui: Arc::new(NoUi),
            dev_loopback: None,
            ports: None,
        }
    }

    #[test]
    fn the_config_is_a_mac_with_the_stored_identity_and_default_settings() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let first = mac_service_config(params(Arc::clone(&secrets))).expect("first run");
        assert_eq!(first.config.os, Os::MacOs);
        assert_eq!(first.config.os_version, "27.2");
        assert_eq!(first.config.name, "Test Mac");
        assert_eq!((first.config.session_port, first.config.pair_port), (47820, 47821));
        assert!(first.config.listen_extra.is_empty() && !first.config.allow_loopback_peers);
        // The second run reads the same identity back from the store.
        let second = mac_service_config(params(secrets)).expect("second run");
        assert_eq!(first.identity.device_id(), second.identity.device_id());
    }

    #[test]
    fn dev_options_set_the_loopback_listener_and_the_ports() {
        let mut p = params(Arc::new(MemorySecretStore::new()));
        p.dev_loopback = Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        p.ports = Some((0, 0));
        let setup = mac_service_config(p).expect("config");
        assert_eq!(setup.config.listen_extra, vec![SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)]);
        assert!(setup.config.allow_loopback_peers);
        assert_eq!((setup.config.session_port, setup.config.pair_port), (0, 0));
        assert_eq!((setup.settings.port, setup.settings.pair_port), (0, 0));
    }

    #[test]
    fn stored_settings_name_the_machine() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        swapdesk_host::ui_hub::store_settings(&*secrets, &MachineSettings::new("Sala")).expect("store settings");
        let setup = mac_service_config(params(secrets)).expect("config");
        assert_eq!(setup.config.name, "Sala");
    }
}
