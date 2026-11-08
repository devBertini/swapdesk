//! The service inside the app (D27): a `ServiceCore` and the UI hub in this process, and the shell's `ServiceBackend`
//! connected to the hub over an in-memory pipe ([`crate::pipe`]). Until the stage 24 daemon exists the app owns the LAN
//! sockets; then only [`InProcessService::start`] changes (the backend will read a Unix socket instead).
//!
//! Order (the hub's own doc): build the hub, give `hub.host_ui()` to the core's config, start the core, then
//! `hub.bind_core`. The config is built before the hub (the hub needs the identity, the trust store and the settings it
//! holds), so it starts with a placeholder `HostUi` that is replaced by the hub's.
//!
//! Everything here is generic over the C3 traits: the macOS pieces (login keychain, physical interfaces) are chosen by
//! [`crate::app`], so this module builds and is tested on any host with the C7 fakes.

use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_core_mac::{MacServiceParams, MacServiceSetup, mac_service_config};
use swapdesk_host::service::ServiceCore;
use swapdesk_host::ui_hub::{
    ConnInfo, DnsSdPresence, FileAuditLog, HubParams, NoHooks, NoPresence, Presence, SystemHooks, UiHub,
};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake};
use swapdesk_lan::discovery::Config as DnsSdConfig;
use swapdesk_lan::txt::Txt;
use swapdesk_platform::{Clock, Level, NetInterfaces, SecretStore, TraceSink};
use swapdesk_proto::Os;
use swapdesk_session::{ConsentRequest, HostUi, PromptAnswer, PromptRequest};
use swapdesk_trust::{SystemWallClock, UserRef, WallClock};
use swapdesk_ui::UiWaker;
use swapdesk_ui::service_backend::{ServiceBackend, ViewerSink};
use tokio::sync::oneshot;

/// The console session of the in-process hub: the one UI of this process is the host's user (prompts and the host
/// banner go to it).
const UI_SESSION: u32 = 1;

/// What the service is built from.
pub struct ServiceParams {
    /// The machine's name until `settings.bin` says otherwise ("MacBook de Fulano").
    pub default_name: String,
    pub os_version: String,
    pub secrets: Arc<dyn SecretStore>,
    /// The C5 folder: the audit log lives here.
    pub dir: PathBuf,
    pub interfaces: Arc<dyn NetInterfaces>,
    pub clock: Arc<dyn Clock>,
    pub trace: Arc<dyn TraceSink>,
    /// Announce this machine and browse for others with DNS-SD. Tests turn it off.
    pub discovery: bool,
    /// Development and tests: also listen on this address and accept loopback peers.
    pub dev_loopback: Option<SocketAddrV4>,
    /// Development and tests: override the session and pairing ports (0 lets the system choose).
    pub ports: Option<(u16, u16)>,
    pub hooks: Arc<dyn SystemHooks>,
}

impl ServiceParams {
    pub fn new(
        default_name: &str,
        os_version: &str,
        secrets: Arc<dyn SecretStore>,
        dir: PathBuf,
        interfaces: Arc<dyn NetInterfaces>,
        clock: Arc<dyn Clock>,
        trace: Arc<dyn TraceSink>,
    ) -> Self {
        Self {
            default_name: default_name.into(),
            os_version: os_version.into(),
            secrets,
            dir,
            interfaces,
            clock,
            trace,
            discovery: true,
            dev_loopback: None,
            ports: None,
            hooks: Arc::new(NoHooks),
        }
    }
}

/// The `HostUi` the config is built with; the hub's replaces it before the core starts.
struct PlaceholderUi;

impl HostUi for PlaceholderUi {
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

/// The running service: the core, the hub, the thread serving the in-memory pipe, and the backend the shell uses.
pub struct InProcessService {
    hub: UiHub,
    /// Ends the hub's read of the pipe at shutdown (the shell's backend keeps the other end open).
    cancel: Arc<std::sync::atomic::AtomicBool>,
    core: Option<Arc<ServiceCore>>,
    serve: Option<JoinHandle<()>>,
    backend: Arc<ServiceBackend>,
}

impl InProcessService {
    /// Starts the core and the hub and connects a backend to the hub; `sink` receives the viewer media.
    pub fn start(p: ServiceParams, waker: Arc<dyn UiWaker>, sink: Option<Arc<dyn ViewerSink>>) -> Result<Self, String> {
        let ServiceParams {
            default_name,
            os_version,
            secrets,
            dir,
            interfaces,
            clock,
            trace,
            discovery,
            dev_loopback,
            ports,
            hooks,
        } = p;
        let MacServiceSetup { mut config, identity, trust, settings } = mac_service_config(MacServiceParams {
            default_name,
            os_version: os_version.clone(),
            secrets: Arc::clone(&secrets),
            interfaces: Arc::clone(&interfaces),
            clock: Arc::clone(&clock),
            trace: Arc::clone(&trace),
            ui: Arc::new(PlaceholderUi),
            dev_loopback,
            ports,
        })?;
        let wall = SystemWallClock;
        let audit = Arc::new(
            FileAuditLog::open(&dir, wall.unix_ms(), Arc::clone(&trace)).map_err(|e| format!("audit log: {e}"))?,
        );
        let presence: Arc<dyn Presence> = if discovery {
            let own =
                Txt { name: settings.name.clone(), os: Os::MacOs, fingerprint: identity.device_id(), pairing: false };
            match DnsSdPresence::start(
                Arc::clone(&interfaces),
                own,
                DnsSdConfig { service_port: settings.port, ..DnsSdConfig::default() },
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
            os: Os::MacOs,
            os_version,
            identity,
            trust,
            secrets,
            settings,
            interfaces,
            presence,
            audit,
            clock,
            wall: Arc::new(SystemWallClock),
            trace: Arc::clone(&trace),
            hooks,
            dev: dev_loopback.map(|_| swapdesk_host::ui_hub::DevOptions::default()),
        });
        config.ui = hub.host_ui();
        let core = Arc::new(ServiceCore::start(config).map_err(|e| format!("the service core did not start: {e:?}"))?);
        hub.bind_core(&core);
        hub.set_console_session(Some(UI_SESSION));
        trace.log(Level::Info, "app", format_args!("service core up on {:?}", core.local_addrs()));

        let (client_end, mut server_end) = crate::pipe::duplex();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        server_end.reader.cancel_with(Arc::clone(&cancel));
        let serving = hub.clone();
        let log = Arc::clone(&trace);
        let serve = std::thread::Builder::new()
            .name("swapdesk-ui-hub".into())
            .spawn(move || {
                let (mut reader, mut writer) =
                    (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
                let served =
                    Handshake::server(&mut reader, &mut writer, UI_SESSION).map_err(|e| e.to_string()).and_then(|_| {
                        let user = UserRef { id: "local-user".into(), name: std::env::var("USER").unwrap_or_default() };
                        // Not an administrator connection: nothing in the Mac app grants UNATTENDED or ADMIN_TERMINAL (stage 24).
                        serving
                            .serve(reader, writer, ConnInfo { session_id: UI_SESSION, user, admin: false })
                            .map_err(|e| e.to_string())
                    });
                if let Err(e) = served {
                    log.log(Level::Debug, "app", format_args!("the UI hub connection ended: {e}"));
                }
            })
            .map_err(|e| format!("hub thread: {e}"))?;
        let backend =
            ServiceBackend::start_with_sink(client_end.reader, client_end.writer, waker, std::process::id(), sink)
                .map_err(|e| format!("hub handshake: {e}"))?;
        Ok(Self { hub, cancel, core: Some(core), serve: Some(serve), backend: Arc::new(backend) })
    }

    /// The C6 backend the shell talks to (also the pipeline's output side).
    pub fn backend(&self) -> Arc<ServiceBackend> {
        Arc::clone(&self.backend)
    }

    pub fn hub(&self) -> &UiHub {
        &self.hub
    }

    /// The addresses the session endpoint listens on.
    pub fn local_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.core.as_ref().map(|c| c.local_addrs().to_vec()).unwrap_or_default()
    }

    /// Stops the hub and the core, waiting a few seconds for the last handles. Safe to call twice.
    pub fn shutdown(&mut self) {
        self.hub.shutdown();
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.serve.take() {
            // The hub says goodbye and the cancelled read ends its serve loop; the thread then returns.
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

impl Drop for InProcessService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use swapdesk_fakes::{FakeNetInterfaces, LogCapture, MemorySecretStore, MonotonicClock};
    use swapdesk_ui::{Command, Reply, UiBackend, UiEvent};

    struct CountingWaker(AtomicUsize);
    impl UiWaker for CountingWaker {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn params(tag: &str) -> (ServiceParams, PathBuf) {
        let dir = std::env::temp_dir().join(format!("swapdesk-app-mac-svc-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = ServiceParams::new(
            "Mac de teste",
            "27.2",
            Arc::new(MemorySecretStore::new()),
            dir.clone(),
            Arc::new(FakeNetInterfaces::new(Vec::new())),
            Arc::new(MonotonicClock::new()),
            Arc::new(LogCapture::new()),
        );
        p.discovery = false;
        p.dev_loopback = Some(SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0));
        p.ports = Some((0, 0));
        (p, dir)
    }

    fn wait_for_reply(backend: &ServiceBackend, waker: &CountingWaker) -> Vec<UiEvent> {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let events = backend.drain();
            if !events.is_empty() {
                return events;
            }
            assert!(Instant::now() < end, "no reply from the hub (wakes: {})", waker.0.load(Ordering::SeqCst));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_shells_backend_talks_to_the_in_process_hub_as_a_mac() {
        let (p, dir) = params("hub");
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let mut svc = InProcessService::start(p, waker.clone(), None).expect("the service starts");
        assert!(!svc.local_addrs().is_empty(), "the session endpoint listens");
        let backend = svc.backend();
        let ticket = backend.send(Command::GetThisDevice);
        let events = wait_for_reply(&backend, &waker);
        match &events[0] {
            UiEvent::Reply { ticket: t, result: Ok(Reply::ThisDevice(d)) } => {
                assert_eq!(*t, ticket);
                assert_eq!(d.os, swapdesk_ui::Os::MacOs);
                assert_eq!(d.name, "Mac de teste");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(waker.0.load(Ordering::SeqCst) >= 1, "the UI thread is woken for the reply");
        // The device list answers too (nobody is paired).
        backend.send(Command::ListDevices);
        let events = wait_for_reply(&backend, &waker);
        assert!(
            matches!(&events[0], UiEvent::Reply { result: Ok(Reply::Devices(d)), .. } if d.is_empty()),
            "{events:?}"
        );
        drop(backend);
        svc.shutdown();
        svc.shutdown(); // twice is fine
        let _ = std::fs::remove_dir_all(&dir);
    }
}
