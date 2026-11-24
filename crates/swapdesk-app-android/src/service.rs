//! The service inside the app (docs/ANDROID.md 5, the Mac's D27): a `ServiceCore` and the UI hub in the app's process,
//! and a `ServiceBackend` connected to the hub over an in-memory pipe. The core owns every socket, the device key,
//! the trust store and the activity log, as the service does on a computer.
//!
//! A phone that does not host only dials: the hub is told not to listen ([`UiHub::set_listening`]), the core receives
//! nothing, and discovery browses without announcing. No port is bound before, while or after it pairs.
//!
//! The Mac app has its own assembly (`swapdesk-app-mac` `service.rs`), which also hosts, picks its ports among the
//! accounts of the machine and asks for an administrator; what the two share lives in `swapdesk-host` (the pipe,
//! the hub, the core, the file trace) and in `swapdesk-ui` (the backend).
//!
//! Everything here is generic over the C3 traits, so it builds and is tested on every host with the C7 fakes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_host::pipe;
use swapdesk_host::service::{ServiceConfig, ServiceCore, ServiceDeps};
use swapdesk_host::ui_hub::{
    ConnInfo, DevOptions, DnsSdPresence, FileAuditLog, HubParams, NoHooks, NoPresence, Presence, UiHub, load_settings,
};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake};
use swapdesk_lan::discovery::Config as DnsSdConfig;
use swapdesk_lan::txt::Txt;
use swapdesk_platform::{Clock, Level, NetInterfaces, SecretStore, TraceSink};
use swapdesk_proto::Os;
use swapdesk_session::{ConsentRequest, HostUi, PromptAnswer, PromptRequest};
use swapdesk_trust::{Identity, SystemWallClock, TrustHandle, UserRef, WallClock};
use swapdesk_ui::UiWaker;
use swapdesk_ui::service_backend::{ServiceBackend, ViewerSink};
use tokio::sync::oneshot;

/// The one interface of the in-process hub: the person holding the phone.
const UI_SESSION: u32 = 1;

/// What the phone's service is built from.
pub struct PhoneParams {
    /// The phone's name until `settings.bin` says otherwise ("Pixel de Ana").
    pub default_name: String,
    /// The Android version a peer sees in `Hello` ("16").
    pub os_version: String,
    /// Where the device key, the trust store and the settings live (C5): the sealed store of `swapdesk-sys-android`.
    pub secrets: Arc<dyn SecretStore>,
    /// The app's private folder: the activity log lives here.
    pub dir: PathBuf,
    /// The phone's networks as Java reports them (`swapdesk-sys-android` `Networks`).
    pub interfaces: Arc<dyn NetInterfaces>,
    pub clock: Arc<dyn Clock>,
    pub trace: Arc<dyn TraceSink>,
    /// Browse for computers with DNS-SD. Tests turn it off.
    pub discovery: bool,
    /// Development and tests only: accept and dial peers on loopback.
    pub dev_loopback: bool,
}

/// The `HostUi` the configuration starts with, replaced by the hub's before the core starts.
struct NobodyYet;

impl HostUi for NobodyYet {
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

/// The running service of the phone.
pub struct PhoneService {
    hub: UiHub,
    /// Ends the hub's read of the pipe at shutdown (the backend keeps the other end open).
    cancel: Arc<AtomicBool>,
    core: Option<Arc<ServiceCore>>,
    serve: Option<JoinHandle<()>>,
    backend: Arc<ServiceBackend>,
    identity: Arc<Identity>,
    trust: Arc<TrustHandle>,
}

impl PhoneService {
    /// Starts the core and the hub and connects a backend to the hub; `sink` receives the viewer's media.
    pub fn start(p: PhoneParams, waker: Arc<dyn UiWaker>, sink: Option<Arc<dyn ViewerSink>>) -> Result<Self, String> {
        let PhoneParams { default_name, os_version, secrets, dir, interfaces, clock, trace, discovery, dev_loopback } =
            p;
        let wall = SystemWallClock;
        let identity =
            Arc::new(Identity::load_or_create(&*secrets, wall.unix_ms()).map_err(|e| format!("identity: {e}"))?);
        let trust = TrustHandle::open(Arc::clone(&secrets)).map_err(|e| format!("trust store: {e}"))?;
        let settings = load_settings(&*secrets, &default_name)?;

        let deps = ServiceDeps {
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            clock: Arc::clone(&clock),
            trace: Arc::clone(&trace),
            ui: Arc::new(NobodyYet),
            interfaces: Arc::clone(&interfaces),
        };
        let mut config = ServiceConfig::new(&settings.name, Os::Android, &os_version, deps);
        // A phone that does not host listens on nothing: no session endpoint, with pairings or without.
        config.receiving = false;
        config.allow_loopback_peers = dev_loopback;

        std::fs::create_dir_all(&dir).map_err(|e| format!("the app's folder: {e}"))?;
        let audit = Arc::new(
            FileAuditLog::open(&dir, wall.unix_ms(), Arc::clone(&trace)).map_err(|e| format!("activity log: {e}"))?,
        );
        let presence: Arc<dyn Presence> = if discovery {
            let own =
                Txt { name: settings.name.clone(), os: Os::Android, fingerprint: identity.device_id(), pairing: false };
            // It browses; its own record is never announced while it does not host.
            match DnsSdPresence::start(
                Arc::clone(&interfaces),
                own,
                DnsSdConfig { announce: false, ..DnsSdConfig::default() },
            ) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    trace.log(Level::Warn, "app", format_args!("discovery is off: {e}"));
                    Arc::new(NoPresence)
                }
            }
        } else {
            Arc::new(NoPresence)
        };
        let hub = UiHub::new(HubParams {
            os: Os::Android,
            os_version,
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            secrets,
            settings,
            interfaces,
            presence,
            audit,
            clock,
            wall: Arc::new(SystemWallClock),
            trace: Arc::clone(&trace),
            hooks: Arc::new(NoHooks),
            dev: dev_loopback.then(DevOptions::default),
        });
        hub.set_listening(false);
        config.ui = hub.host_ui();
        let core = Arc::new(ServiceCore::start(config).map_err(|e| format!("the service core did not start: {e:?}"))?);
        hub.bind_core(&core);
        hub.set_console_session(Some(UI_SESSION));
        trace.log(Level::Info, "app", format_args!("service core up, listening on {:?}", core.local_addrs()));

        let cancel = Arc::new(AtomicBool::new(false));
        let (client_end, mut server_end) = pipe::duplex();
        server_end.reader.cancel_with(Arc::clone(&cancel));
        let serving = hub.clone();
        let log = Arc::clone(&trace);
        let serve = std::thread::Builder::new()
            .name("swapdesk-ui-hub".into())
            .spawn(move || {
                let (mut reader, mut writer) =
                    (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
                let served = Handshake::server(&mut reader, &mut writer, UI_SESSION)
                    .map_err(|e| e.to_string())
                    .and_then(|_| serving.serve(reader, writer, app_connection()).map_err(|e| e.to_string()));
                if let Err(e) = served {
                    log.log(Level::Debug, "app", format_args!("the UI hub connection ended: {e}"));
                }
            })
            .map_err(|e| format!("hub thread: {e}"))?;
        let backend =
            ServiceBackend::start_with_sink(client_end.reader, client_end.writer, waker, std::process::id(), sink)
                .map_err(|e| format!("hub handshake: {e}"))?;
        Ok(Self { hub, cancel, core: Some(core), serve: Some(serve), backend: Arc::new(backend), identity, trust })
    }

    /// The C6 backend the interface talks to.
    pub fn backend(&self) -> Arc<ServiceBackend> {
        Arc::clone(&self.backend)
    }

    pub fn hub(&self) -> &UiHub {
        &self.hub
    }

    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    pub fn trust(&self) -> &Arc<TrustHandle> {
        &self.trust
    }

    /// The phone's networks changed (Java reported a new list): the core and discovery follow.
    pub fn network_changed(&self) {
        self.hub.network_changed();
    }

    /// Every address this phone listens on: the session endpoints and the pairing endpoints. Empty while it does not
    /// host.
    pub fn listening_on(&self) -> Vec<std::net::SocketAddr> {
        let mut all = self.core.as_ref().map(|c| c.local_addrs().to_vec()).unwrap_or_default();
        all.extend(self.hub.pair_local_addrs());
        all
    }

    /// Stops the hub and the core, waiting a few seconds for the last handles. Safe to call twice.
    pub fn shutdown(&mut self) {
        self.hub.shutdown();
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(t) = self.serve.take() {
            let _ = t.join();
        }
        let end = Instant::now() + Duration::from_secs(3);
        let mut core = self.core.take();
        while let Some(c) = core.take() {
            match Arc::try_unwrap(c) {
                Ok(c) => c.shutdown(),
                Err(c) if Instant::now() < end => {
                    core = Some(c);
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(c) => drop(c),
            }
        }
    }
}

impl Drop for PhoneService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The app's one connection to its own hub: the person holding the phone. Never an administrator: nothing a phone
/// does needs one, and it grants nothing that does.
fn app_connection() -> ConnInfo {
    ConnInfo {
        session_id: UI_SESSION,
        user: UserRef { id: "phone-user".into(), name: String::new() },
        admin: false,
        can_elevate: false,
    }
}
