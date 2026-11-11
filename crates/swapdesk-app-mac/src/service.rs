//! The service inside the app (D27): a `ServiceCore` and the UI hub in this process, and the shell's `ServiceBackend`
//! connected to the hub over an in-memory pipe ([`crate::pipe`]). Until the stage 24 daemon exists the app owns the LAN
//! sockets; then only [`InProcessService::start`] changes (the backend will read a Unix socket instead).
//!
//! Order (the hub's own doc): build the hub, give `hub.host_ui()` to the core's config, start the core, then
//! `hub.bind_core`. The config is built before the hub (the hub needs the identity, the trust store and the settings it
//! holds), so it starts with a placeholder `HostUi` that is replaced by the hub's.
//!
//! Hosting (stage 24, slice a): when [`ServiceParams::host_helper`] is set, a supervisor thread runs the host's helper
//! loop (`swapdesk_host::helper::run_helper` over the Mac capture, encoder, input and audio crates, chosen by
//! [`crate::app`]) on its own thread and attaches it to the core as the user helper over a bounded in-memory pipe
//! ([`crate::pipe::bounded_duplex`]), restarting it when it ends. This is the in-process stand-in for the Aqua agent of
//! `docs/CONTRACTS.md` C2.2 until the daemon and the agent exist.
//!
//! Network changes: [`ServiceParams::network_notices`] starts the source that says when the network changed
//! ([`crate::app`] gives SystemConfiguration's dynamic store, `swapdesk_net_mac::NetworkWatcher`; tests inject one), and
//! each notice calls `UiHub::network_changed`: the session endpoint and DNS-SD move to the new address and the shell
//! hears of it. No timer looks. Without it a Mac whose Wi-Fi address changed under the running app kept listening and
//! announcing on the old one and vanished from every other machine's list.
//!
//! Administrator confirmation (`docs/ELEVATION-MAC.md`): the app's one connection may ask for a ticket (`can_elevate`) but is
//! never an administrator itself. When [`ServiceParams::admin_confirm`] is set, the backend gets a
//! [`crate::elevate::MacElevator`] that asks the system through it and, only after an approval, confirms the ticket in
//! this process.
//!
//! Everything here is generic over the C3 traits: the macOS pieces (login keychain, physical interfaces) are chosen by
//! [`crate::app`], so this module builds and is tested on any host with the C7 fakes.

use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::elevate::{AdminConfirm, MacElevator};
use swapdesk_core_mac::{MacServiceParams, MacServiceSetup, mac_service_config};
use swapdesk_host::service::ServiceCore;
use swapdesk_host::ui_hub::{
    ConnInfo, DnsSdPresence, FileAuditLog, HubParams, NoHooks, NoPresence, Presence, SystemHooks, UiHub,
};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake, IpcRole};
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
/// Chunks queued toward either side of the host helper's pipe before a write waits (see [`crate::pipe::bounded_duplex`]).
/// A chunk is one write: a video frame over 16 KiB is two (its head, then its payload), so this holds 32 such frames.
const HELPER_PIPE_BOUND: usize = 64;
/// Pause before the host helper is started again after it ended; doubled after each quick end, up to the maximum.
const HELPER_RESTART: Duration = Duration::from_millis(500);
const HELPER_RESTART_MAX: Duration = Duration::from_secs(10);
/// Starts a source of network change notices: it calls the given function once per burst of changes until the
/// returned guard is dropped.
pub type NetworkNotices = Box<dyn FnOnce(Box<dyn Fn() + Send + Sync>) -> Result<Box<dyn Send>, String> + Send>;
/// A helper that ran at least this long ended normally: the next restart starts from [`HELPER_RESTART`] again.
const HELPER_HEALTHY: Duration = Duration::from_secs(30);

/// Runs the host's helper loop over the two ends of its pipe until the service closes it or it fails; `Err` carries
/// why. Called on a thread of its own by the supervisor, again after each end.
pub type HostHelper = Arc<dyn Fn(crate::pipe::PipeReader, crate::pipe::PipeWriter) -> Result<(), String> + Send + Sync>;

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
    /// This Mac hosts sessions: the helper loop the supervisor runs and attaches (`None`: viewer only).
    pub host_helper: Option<HostHelper>,
    /// Where network change notices come from (`None`: none, the network is taken as it was at the start).
    pub network_notices: Option<NetworkNotices>,
    /// How the administrator is asked for (`None`: nobody is, and an `Elevate` ends in an error). [`crate::app`] gives the
    /// system's dialog on a normal run; tests give a scripted one.
    pub admin_confirm: Option<Arc<dyn AdminConfirm>>,
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
            host_helper: None,
            network_notices: None,
            admin_confirm: None,
        }
    }
}

/// The app's one connection to its own hub. Not an administrator connection: nothing in the Mac app grants `UNATTENDED` or
/// `ADMIN_TERMINAL` by itself. It `can_elevate`: it may ask for a ticket, and the administrator's name and password typed
/// into macOS's dialog are what let [`MacElevator`] confirm it (`docs/ELEVATION-MAC.md` 3.4).
pub(crate) fn app_connection() -> ConnInfo {
    let user = UserRef { id: "local-user".into(), name: std::env::var("USER").unwrap_or_default() };
    ConnInfo { session_id: UI_SESSION, user, admin: false, can_elevate: true }
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
    /// The host helper's supervisor, when this Mac hosts.
    supervisor: Option<JoinHandle<()>>,
    backend: Arc<ServiceBackend>,
    /// The network notices' guard; dropped first at shutdown.
    net_watch: Option<Box<dyn Send>>,
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
            host_helper,
            network_notices,
            admin_confirm,
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
        let net_watch = match network_notices {
            Some(start) => {
                let (hub, trace) = (hub.clone(), Arc::clone(&trace));
                Some(start(Box::new(move || {
                    trace.log(Level::Info, "app", format_args!("the network changed"));
                    hub.network_changed();
                }))?)
            }
            None => None,
        };

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let supervisor = match host_helper {
            Some(helper) => Some(spawn_supervisor(&core, helper, Arc::clone(&cancel), Arc::clone(&trace))?),
            None => None,
        };
        let (client_end, mut server_end) = crate::pipe::duplex();
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
        // The clipboard sync of the viewer role reads and writes this user's pasteboard (C2.4 0x60 to 0x62), as the
        // Windows UI does with its own clipboard.
        #[cfg(target_os = "macos")]
        backend.set_clipboard(swapdesk_input_mac::MacClipboard::new());
        if let Some(confirm) = admin_confirm {
            backend.set_elevator(Arc::new(MacElevator::new(hub.clone(), app_connection(), confirm)));
        }
        Ok(Self {
            hub,
            cancel,
            core: Some(core),
            serve: Some(serve),
            supervisor,
            backend: Arc::new(backend),
            net_watch,
        })
    }

    /// The C6 backend the shell talks to (also the pipeline's output side).
    pub fn backend(&self) -> Arc<ServiceBackend> {
        Arc::clone(&self.backend)
    }

    pub fn hub(&self) -> &UiHub {
        &self.hub
    }

    /// A host helper is attached to the core right now (this Mac can be viewed and controlled).
    pub fn host_helper_attached(&self) -> bool {
        self.core.as_ref().is_some_and(|c| c.helper(swapdesk_host::service::HelperKind::User).is_some())
    }

    /// The addresses the session endpoint listens on.
    pub fn local_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.core.as_ref().map(|c| c.local_addrs().to_vec()).unwrap_or_default()
    }

    /// Stops the hub and the core, waiting a few seconds for the last handles. Safe to call twice.
    pub fn shutdown(&mut self) {
        // No network news for a hub that is going away.
        drop(self.net_watch.take());
        self.hub.shutdown();
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.serve.take() {
            // The hub says goodbye and the cancelled read ends its serve loop; the thread then returns.
            let _ = t.join();
        }
        if let Some(t) = self.supervisor.take() {
            // The cancelled reads end the helper loop and the service's reader of it; the supervisor then stops.
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

/// Starts the thread that keeps the host helper attached: a fresh bounded pipe, the helper on its own thread at one end,
/// `attach_helper` (user helper) at the other; when the helper ends it is started again after a pause, until `cancel`.
/// The thread holds the core only weakly, so shutdown is not kept waiting for it.
fn spawn_supervisor(
    core: &Arc<ServiceCore>,
    helper: HostHelper,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    trace: Arc<dyn TraceSink>,
) -> Result<JoinHandle<()>, String> {
    let core = Arc::downgrade(core);
    std::thread::Builder::new()
        .name("swapdesk-host-helper".into())
        .spawn(move || {
            let mut pause = HELPER_RESTART;
            while !cancel.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut service_end, mut helper_end) = crate::pipe::bounded_duplex(HELPER_PIPE_BOUND);
                service_end.reader.cancel_with(Arc::clone(&cancel));
                helper_end.reader.cancel_with(Arc::clone(&cancel));
                let run = Arc::clone(&helper);
                let started = Instant::now();
                let thread = match std::thread::Builder::new()
                    .name("swapdesk-host-helper-loop".into())
                    .spawn(move || run(helper_end.reader, helper_end.writer))
                {
                    Ok(t) => t,
                    Err(e) => {
                        trace.log(Level::Error, "app", format_args!("host helper thread: {e}"));
                        return;
                    }
                };
                let attached = match core.upgrade() {
                    Some(core) => core.attach_helper(IpcRole::HelperUser, service_end.reader, service_end.writer),
                    None => return,
                };
                if let Err(e) = &attached {
                    trace.log(Level::Warn, "app", format_args!("the host helper did not attach: {e:?}"));
                }
                let ended = thread.join();
                match ended {
                    Ok(Ok(())) => trace.log(Level::Info, "app", format_args!("the host helper ended")),
                    Ok(Err(e)) => trace.log(Level::Warn, "app", format_args!("the host helper failed: {e}")),
                    Err(_) => trace.log(Level::Error, "app", format_args!("the host helper panicked")),
                }
                pause = if started.elapsed() >= HELPER_HEALTHY { HELPER_RESTART } else { pause };
                let until = Instant::now() + pause;
                while Instant::now() < until && !cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                pause = (pause * 2).min(HELPER_RESTART_MAX);
            }
        })
        .map_err(|e| format!("host helper supervisor: {e}"))
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

    /// Emits the encode counter as the payload; keyframe on the first and on forced frames.
    struct Passthrough(u32);

    impl swapdesk_platform::VideoEncoder for Passthrough {
        fn configure(
            &mut self,
            _config: &swapdesk_platform::EncoderConfig,
        ) -> swapdesk_platform::Result<swapdesk_platform::EncoderInfo> {
            self.0 = 0;
            Ok(swapdesk_platform::EncoderInfo {
                name: "passthrough".into(),
                hardware: false,
                profile: swapdesk_proto::H264Profile::Main,
                level: 40,
                low_latency: true,
            })
        }
        fn encode(
            &mut self,
            frame: &swapdesk_platform::Frame,
            force_keyframe: bool,
        ) -> swapdesk_platform::Result<Vec<swapdesk_platform::EncodedFrame>> {
            let first = self.0 == 0;
            self.0 += 1;
            Ok(vec![swapdesk_platform::EncodedFrame {
                captured_at: frame.captured_at,
                encode_us: 1,
                keyframe: first || force_keyframe,
                config_change: first,
                width: frame.width as u16,
                height: frame.height as u16,
                annex_b: self.0.to_be_bytes().to_vec(),
            }])
        }
        fn set_rate(&mut self, _kbps: u32, _fps: u16) -> swapdesk_platform::Result<()> {
            Ok(())
        }
        fn flush(&mut self) -> swapdesk_platform::Result<Vec<swapdesk_platform::EncodedFrame>> {
            Ok(Vec::new())
        }
    }

    fn fake_helper(runs: Arc<AtomicUsize>) -> HostHelper {
        Arc::new(move |reader, writer| {
            runs.fetch_add(1, Ordering::SeqCst);
            let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
            let audio_clock = Arc::clone(&clock);
            let deps = swapdesk_host::helper::HelperDeps::new(
                |_| Ok(swapdesk_fakes::SyntheticScreen::new(320, 200, 30)),
                |_, _, _| Ok(Passthrough(0)),
                || Ok(swapdesk_fakes::RecordingInputSink::new()),
                move || Ok(swapdesk_fakes::SineSource::new(Arc::clone(&audio_clock))),
                clock,
            );
            swapdesk_host::helper::run_helper(reader, writer, deps, IpcRole::HelperUser, std::process::id())
                .map(|_| ())
                .map_err(|e| format!("{e:?}"))
        })
    }

    #[test]
    fn the_host_helper_is_attached_kept_and_stopped_with_the_service() {
        let (mut p, dir) = params("host");
        let runs = Arc::new(AtomicUsize::new(0));
        p.host_helper = Some(fake_helper(Arc::clone(&runs)));
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let mut svc = InProcessService::start(p, waker, None).expect("the service starts");
        let end = Instant::now() + Duration::from_secs(10);
        while !svc.host_helper_attached() {
            assert!(Instant::now() < end, "the host helper never attached");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "a healthy helper is not restarted");
        let stopping = Instant::now();
        svc.shutdown();
        assert!(stopping.elapsed() < Duration::from_secs(5), "shutdown waits for the helper only briefly");
        assert!(!svc.host_helper_attached());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_host_helper_that_fails_is_started_again() {
        let (mut p, dir) = params("restart");
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        p.host_helper = Some(Arc::new(move |_reader, _writer| {
            counted.fetch_add(1, Ordering::SeqCst);
            Err("no screen".into())
        }));
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let mut svc = InProcessService::start(p, waker, None).expect("the service starts");
        let end = Instant::now() + Duration::from_secs(5);
        while runs.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < end, "the failed helper was not started again");
            std::thread::sleep(Duration::from_millis(10));
        }
        svc.shutdown();
        let after = runs.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(runs.load(Ordering::SeqCst), after, "nothing restarts after shutdown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_address_change_under_the_running_app_reaches_the_hub_and_the_shell() {
        // The field case (09/11): the app started on 192.168.1.2, DHCP moved the Wi-Fi to 192.168.1.3, and nothing told
        // the hub, so DNS-SD stayed on the old address and the PC listed this Mac as offline.
        let (mut p, dir) = params("netnotice");
        let wifi = |last: u8| swapdesk_platform::NetInterface {
            index: 11,
            name: "en0".into(),
            kind: swapdesk_platform::IfKind::WiFi,
            up: true,
            mac: None,
            ipv4: vec![(std::net::Ipv4Addr::new(192, 168, 1, last), 24)],
            category: None,
            inbound_blocked: None,
        };
        let nets = Arc::new(FakeNetInterfaces::new(vec![wifi(2)]));
        p.interfaces = nets.clone();
        // The notice source the app gives is SystemConfiguration's; this one hands its callback to the test.
        type Notify = Box<dyn Fn() + Send + Sync>;
        let notify: Arc<std::sync::Mutex<Option<Notify>>> = Arc::default();
        let slot = Arc::clone(&notify);
        p.network_notices = Some(Box::new(move |changed| {
            *slot.lock().unwrap() = Some(changed);
            Ok(Box::new(()) as Box<dyn Send>)
        }));
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let mut svc = InProcessService::start(p, waker, None).expect("the service starts");
        let backend = svc.backend();
        nets.set(vec![wifi(3)]);
        // No notice, no news: nothing looks at the list on its own.
        std::thread::sleep(Duration::from_millis(300));
        assert!(!backend.drain().iter().any(|e| matches!(e, UiEvent::NetworkChanged(_))), "nothing polls");
        (notify.lock().unwrap().as_ref().expect("the source was started"))();
        let end = Instant::now() + Duration::from_secs(10);
        let status = 'found: loop {
            for e in backend.drain() {
                if let UiEvent::NetworkChanged(s) = e {
                    break 'found s;
                }
            }
            assert!(Instant::now() < end, "the shell never heard of the network change");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.address.as_deref(), Some("192.168.1.3"));
        let stopping = Instant::now();
        svc.shutdown();
        assert!(stopping.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
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

    // ---------- the administrator confirmation (docs/ELEVATION-MAC.md 5.2, M3) ----------

    use crate::elevate::Confirmation;
    use swapdesk_ui::state::ElevPhase;
    use swapdesk_ui::{ApiCode, ControlId, DeviceTab, DialogKind, Language, Permission, UiState, WindowKind};
    use test_support::{Rig, Scripted, grants_with_unattended, initial_grants};

    /// What the shell does with the commands an activation returns: send each and track its ticket.
    fn send_all(rig: &Rig, state: &mut UiState, commands: Vec<Command>) {
        for command in commands {
            let ticket = rig.backend.send(command.clone());
            state.track(ticket, &command);
        }
    }

    /// Applies what the service says to `state` until `done` holds.
    fn settle(rig: &Rig, state: &mut UiState, what: &str, done: impl Fn(&UiState) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(state) {
            assert!(Instant::now() < end, "never reached: {what}");
            for event in rig.inbox.drain() {
                state.apply(event);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The window of the Mac app up to the unattended switch of the paired device, as the shell loads it.
    fn open_permissions(rig: &Rig) -> UiState {
        let mut state = UiState::new(Language::PtBr, false);
        send_all(rig, &mut state, vec![Command::GetThisDevice, Command::ListDevices]);
        settle(rig, &mut state, "the device list", |s| s.this_device().is_some() && !s.devices().is_empty());
        state.select_device(&rig.device);
        state.activate(ControlId::Tab(DeviceTab::Permissions));
        state
    }

    fn unattended_here(state: &UiState, device: &str) -> bool {
        state.device_by_id(device).is_some_and(|d| d.grants_here.has(Permission::Unattended))
    }

    #[test]
    fn the_app_connection_can_elevate_and_is_not_admin() {
        let c = app_connection();
        assert!(c.can_elevate, "it may ask for a ticket");
        assert!(!c.admin, "it is never an administrator itself");
        assert_eq!((c.session_id, c.user.id.as_str()), (UI_SESSION, "local-user"));

        // Through the hub: the widening is refused as `NeedsAdmin` (not an administrator), and a ticket is given (it can
        // ask), so the scripted approval ends the way an administrator's would.
        let confirm = Scripted::new(Confirmation::Approved);
        let rig = Rig::start("conn", Some(confirm.clone()));
        assert_eq!(rig.set_grants(grants_with_unattended()), Err(ApiCode::NeedsAdmin));
        assert_eq!(rig.elevate(), Ok(()));
        assert_eq!(confirm.calls(), 1);
    }

    #[test]
    fn continuar_asks_the_system_and_the_unattended_grant_passes_once() {
        let confirm = Scripted::new(Confirmation::Approved);
        let rig = Rig::start("continuar", Some(confirm.clone()));
        let mut state = open_permissions(&rig);
        assert!(!unattended_here(&state, &rig.device));

        // The switch asks the service; the service answers `NeedsAdmin` and the step opens.
        let a = state.activate(ControlId::Grant(Permission::Unattended));
        send_all(&rig, &mut state, a.commands);
        settle(&rig, &mut state, "the elevation step", |s| s.dialog(WindowKind::Main) == Some(DialogKind::Elevation));
        assert_eq!(confirm.calls(), 0, "nothing asked the system before Continuar");
        assert_eq!(rig.grants_here(), initial_grants().0);

        // "Continuar": the system is asked once, the step closes and the resend passes.
        let a = state.activate(ControlId::DialogPrimary);
        assert_eq!(state.elevation().map(|e| e.phase.clone()), Some(ElevPhase::Waiting));
        send_all(&rig, &mut state, a.commands);
        settle(&rig, &mut state, "the step closing", |s| s.elevation().is_none());
        // Opening Permissões also asked for a refresh of the column (op 39); the resend is the point here.
        let followups: Vec<Command> =
            state.take_followups().into_iter().filter(|c| !matches!(c, Command::RefreshGrants { .. })).collect();
        assert!(matches!(followups.as_slice(), [Command::SetGrants { .. }]), "{followups:?}");
        send_all(&rig, &mut state, followups);
        settle(&rig, &mut state, "the grant in the list", |s| unattended_here(s, &rig.device));
        assert_eq!(confirm.calls(), 1);
        assert_eq!(rig.grants_here(), grants_with_unattended().0, "the trust store holds it");
        assert_eq!(
            rig.unattended_granted_by().as_deref(),
            Some("local-user"),
            "the account that asked, not the approver"
        );

        // The pass was used up: take the grant away, and asking for it again is `NeedsAdmin` and a step again.
        let a = state.activate(ControlId::Grant(Permission::Unattended));
        send_all(&rig, &mut state, a.commands);
        settle(&rig, &mut state, "the grant taken away", |s| !unattended_here(s, &rig.device));
        let a = state.activate(ControlId::Grant(Permission::Unattended));
        send_all(&rig, &mut state, a.commands);
        settle(&rig, &mut state, "the second step", |s| s.dialog(WindowKind::Main) == Some(DialogKind::Elevation));
        assert_eq!(confirm.calls(), 1, "no second dialog without a second Continuar");
        assert_eq!(rig.grants_here(), initial_grants().0);
    }

    #[test]
    fn a_cancelled_system_dialog_shows_the_failure_and_changes_nothing() {
        let confirm = Scripted::new(Confirmation::Cancelled);
        let rig = Rig::start("cancelled", Some(confirm.clone()));
        let mut state = open_permissions(&rig);
        let a = state.activate(ControlId::Grant(Permission::Unattended));
        send_all(&rig, &mut state, a.commands);
        settle(&rig, &mut state, "the elevation step", |s| s.dialog(WindowKind::Main) == Some(DialogKind::Elevation));
        let a = state.activate(ControlId::DialogPrimary);
        send_all(&rig, &mut state, a.commands);
        settle(&rig, &mut state, "the failure", |s| {
            s.elevation().is_some_and(|e| e.phase == ElevPhase::Failed(ApiCode::Refused))
        });
        assert!(
            state.take_followups().iter().all(|c| matches!(c, Command::RefreshGrants { .. })),
            "nothing is sent again"
        );
        assert_eq!(confirm.calls(), 1);
        assert_eq!(rig.grants_here(), initial_grants().0, "nothing changed");
        assert!(!unattended_here(&state, &rig.device));
        // "Tentar de novo" asks the system again, and a second cancel ends the same way.
        let a = state.activate(ControlId::DialogPrimary);
        assert!(matches!(a.commands.as_slice(), [Command::Elevate { .. }]));
        send_all(&rig, &mut state, a.commands);
        let end = Instant::now() + Duration::from_secs(10);
        while confirm.calls() < 2 {
            assert!(Instant::now() < end, "the retry never asked the system");
            for event in rig.inbox.drain() {
                state.apply(event);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rig.grants_here(), initial_grants().0);
    }
}

/// What the tests of this module and of [`crate::elevate`] share: a service with one paired device and a scripted
/// `AdminConfirm` that never shows a dialog.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use swapdesk_fakes::{FakeNetInterfaces, LogCapture, MemorySecretStore, MonotonicClock};
    use swapdesk_proto::Os;
    use swapdesk_trust::{Identity, PairedDevice, TrustHandle, device_id_hex};
    use swapdesk_ui::service_backend::ServiceBackend;
    use swapdesk_ui::{ApiCode, Command, Permission, Permissions, Reply, UiBackend, UiEvent, UiWaker};

    use super::{InProcessService, ServiceParams};
    use crate::elevate::{AdminConfirm, Confirmation};

    struct NoWaker;
    impl UiWaker for NoWaker {
        fn wake(&self) {}
    }

    /// What the paired device may do before anything is asked: control (and so view).
    pub(crate) fn initial_grants() -> Permissions {
        Permissions::NONE.with(Permission::Control)
    }

    /// The widening that needs an administrator.
    pub(crate) fn grants_with_unattended() -> Permissions {
        initial_grants().with(Permission::Unattended)
    }

    /// An `AdminConfirm` that answers as told, after `delay`, and runs `during` once while it is asking: no dialog, ever.
    pub(crate) struct Scripted {
        answer: Confirmation,
        delay: Duration,
        calls: AtomicUsize,
        during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Scripted {
        /// Answers at once.
        pub(crate) fn new(answer: Confirmation) -> Arc<Self> {
            Self::slow(answer, Duration::ZERO)
        }

        /// Answers after `delay`, as a person who takes their time.
        pub(crate) fn slow(answer: Confirmation, delay: Duration) -> Arc<Self> {
            Arc::new(Self { answer, delay, calls: AtomicUsize::new(0), during: Mutex::new(None) })
        }

        /// `f` runs once, on the asking thread, while the dialog would be on screen.
        pub(crate) fn while_asking(&self, f: impl FnOnce() + Send + 'static) {
            *self.during.lock().unwrap() = Some(Box::new(f));
        }

        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl AdminConfirm for Scripted {
        fn confirm(&self, _line: &str) -> Confirmation {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(f) = self.during.lock().unwrap().take() {
                f();
            }
            std::thread::sleep(self.delay);
            self.answer
        }
    }

    /// The events of a backend, shared by whoever waits for one: an event nobody asked for stays for the next waiter.
    pub(crate) struct Inbox {
        backend: Arc<ServiceBackend>,
        seen: Mutex<Vec<UiEvent>>,
    }

    impl Inbox {
        /// Everything that came in and nobody took.
        pub(crate) fn drain(&self) -> Vec<UiEvent> {
            let mut seen = self.seen.lock().unwrap();
            seen.extend(self.backend.drain());
            std::mem::take(&mut *seen)
        }

        /// The first event `pick` accepts, taken out; waits up to ten seconds.
        pub(crate) fn wait_for(&self, pick: impl Fn(&UiEvent) -> bool) -> UiEvent {
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                {
                    let mut seen = self.seen.lock().unwrap();
                    seen.extend(self.backend.drain());
                    if let Some(i) = seen.iter().position(&pick) {
                        return seen.remove(i);
                    }
                }
                assert!(Instant::now() < end, "the event never came");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// A running service with one paired device holding `CONTROL`, and its backend.
    pub(crate) struct Rig {
        pub(crate) svc: InProcessService,
        pub(crate) backend: Arc<ServiceBackend>,
        pub(crate) inbox: Arc<Inbox>,
        /// The paired device's id as the interface names it.
        pub(crate) device: String,
        secrets: Arc<MemorySecretStore>,
        dir: std::path::PathBuf,
    }

    impl Rig {
        pub(crate) fn start(tag: &str, admin_confirm: Option<Arc<dyn AdminConfirm>>) -> Self {
            let dir = std::env::temp_dir().join(format!("swapdesk-app-mac-elev-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let secrets = Arc::new(MemorySecretStore::new());
            // The pairing ceremony, done by hand before the service opens its trust store.
            let device = {
                let trust = TrustHandle::open(secrets.clone()).expect("trust store");
                let other = Identity::generate(0).expect("identity");
                let mut paired =
                    PairedDevice::new_paired(*other.spki(), "PC de teste".into(), Os::Windows, "11".into(), 1, None);
                paired.grants_here = swapdesk_proto::PermissionSet::CONTROL;
                let id = device_id_hex(&paired.device_id);
                trust.update(|s| s.devices.push(paired)).expect("store");
                id
            };
            let mut p = ServiceParams::new(
                "Mac de teste",
                "27.2",
                secrets.clone(),
                dir.clone(),
                Arc::new(FakeNetInterfaces::new(Vec::new())),
                Arc::new(MonotonicClock::new()),
                Arc::new(LogCapture::new()),
            );
            p.discovery = false;
            p.dev_loopback = Some(std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0));
            p.ports = Some((0, 0));
            p.admin_confirm = admin_confirm;
            let svc = InProcessService::start(p, Arc::new(NoWaker), None).expect("the service starts");
            let backend = svc.backend();
            let inbox = Arc::new(Inbox { backend: Arc::clone(&backend), seen: Mutex::new(Vec::new()) });
            Self { svc, backend, inbox, device, secrets, dir }
        }

        /// `SetGrants` for the paired device and the service's answer.
        pub(crate) fn set_grants(&self, grants: Permissions) -> Result<(), ApiCode> {
            let t = self.backend.send(Command::SetGrants { device_id: self.device.clone(), grants });
            let event = self.inbox.wait_for(|e| matches!(e, UiEvent::Reply { ticket, .. } if *ticket == t));
            let UiEvent::Reply { result, .. } = event else { unreachable!() };
            result.map(|_: Reply| ()).map_err(|e| e.code)
        }

        /// The widening that needs an administrator.
        pub(crate) fn widen(&self) -> Result<(), ApiCode> {
            self.set_grants(grants_with_unattended())
        }

        /// Takes the widening back: open to any console connection.
        pub(crate) fn narrow(&self) -> Result<(), ApiCode> {
            self.set_grants(initial_grants())
        }

        /// `Command::Elevate` for the widening, through the backend's worker, and its `ElevationResult`.
        pub(crate) fn elevate(&self) -> Result<(), ApiCode> {
            let action = swapdesk_ui::ElevatedAction::SetGrants {
                device_id: self.device.clone(),
                grants: grants_with_unattended(),
            };
            self.backend.send(Command::Elevate { action: action.clone(), line: String::new(), run: 1 });
            let event = self.inbox.wait_for(|e| matches!(e, UiEvent::ElevationResult { .. }));
            let UiEvent::ElevationResult { action: got, run, result } = event else { unreachable!() };
            assert_eq!(run, 1, "the run it answers");
            assert_eq!(got, action);
            result.map_err(|e| e.code)
        }

        fn read<R>(&self, f: impl FnOnce(&swapdesk_trust::TrustStore) -> R) -> R {
            // A fresh handle reads `trust.bin` as the service wrote it: the proof does not rely on the service's own copy.
            TrustHandle::open(self.secrets.clone()).expect("trust store").read(f)
        }

        /// The device's grants as the trust store holds them, as the interface's bits.
        pub(crate) fn grants_here(&self) -> u16 {
            self.read(|s| s.devices[0].grants_here.bits())
        }

        /// The account that granted `UNATTENDED`, when one did.
        pub(crate) fn unattended_granted_by(&self) -> Option<String> {
            self.read(|s| s.devices[0].unattended_granted_by.clone().map(|u| u.id))
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.svc.shutdown();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
