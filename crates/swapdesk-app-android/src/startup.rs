//! The app's one core (docs/ANDROID.md 5): started once by `Core.start`, fed the phone's networks by `Core.networks`.
//!
//! What crosses from Java is small and goes one way: the app's private folder, the data key the Keystore unwrapped,
//! the phone's name and Android version, and at every change the list of networks. Everything else stays here.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use swapdesk_host::filetrace::{DEFAULT_CAP, FileTrace};
use swapdesk_lan::socket::{AfterBind, set_after_bind};
use swapdesk_platform::{Level, NetInterfaces, TraceSink};
use swapdesk_sys_android::clock::MonotonicClock;
use swapdesk_sys_android::facts::{self, Networks};
use swapdesk_sys_android::logcat::Logcat;
use swapdesk_sys_android::network;
use swapdesk_sys_android::sealed::{KEY_LEN, SealedStore};
use swapdesk_ui::UiWaker;

use crate::phone_service::{PhoneParams, PhoneService};

/// The trace of the app: the file in its private folder, and the system log for a development build to follow.
struct Tee(FileTrace, Logcat);

impl TraceSink for Tee {
    fn log(&self, level: Level, target: &'static str, what: core::fmt::Arguments<'_>) {
        self.0.log(level, target, what);
        self.1.log(level, target, what);
    }
}

/// Until the shared screens run on the phone, nothing waits to be woken.
struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

pub(crate) struct App {
    /// The app's private folder, where the harness meets its orchestrator.
    #[cfg(feature = "dev-harness")]
    pub dir: PathBuf,
    pub service: PhoneService,
}

static NETWORKS: OnceLock<Arc<Networks>> = OnceLock::new();
static APP: Mutex<Option<App>> = Mutex::new(None);

/// The phone's networks. They exist before the core starts: Java may report them first.
pub(crate) fn networks() -> &'static Arc<Networks> {
    NETWORKS.get_or_init(|| Arc::new(Networks::new()))
}

pub(crate) fn app() -> MutexGuard<'static, Option<App>> {
    APP.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a development build may ask at the start. A build without the harness takes none.
#[derive(Default)]
struct Dev {
    /// Send QUIC with this TTL instead of 1: the emulator's own router is one hop between the guest and its host.
    ttl: Option<u32>,
    /// Accept and dial loopback peers.
    loopback: bool,
}

#[cfg(feature = "dev-harness")]
fn dev_options(spec: &str) -> Result<Dev, String> {
    let mut dev = Dev::default();
    for word in spec.split_whitespace() {
        if let Some(Ok(hops)) = word.strip_prefix("ttl=").map(str::parse::<u32>) {
            dev.ttl = Some(hops);
        } else if word == "loopback" {
            dev.loopback = true;
        } else {
            return Err(format!("{word:?} is no development option"));
        }
    }
    Ok(dev)
}

#[cfg(not(feature = "dev-harness"))]
fn dev_options(spec: &str) -> Result<Dev, String> {
    if spec.trim().is_empty() { Ok(Dev::default()) } else { Err("this build takes no development options".into()) }
}

/// The step on every QUIC socket: bound to the approved network that holds its address, or the bind fails. A
/// development build may then raise the TTL for the emulator.
fn after_bind(dev: &Dev) -> AfterBind {
    let bind = network::quic_binder(Arc::clone(networks()));
    match dev.ttl {
        Some(hops) => Box::new(move |socket| {
            bind(socket)?;
            socket.set_ttl(hops)
        }),
        None => bind,
    }
}

/// `Core.start`: opens the sealed store with `key` in `<dir>/secrets`, the trace in `dir`, and starts the service.
/// A second call starts nothing.
pub fn start(dir: &str, key: &[u8], name: &str, os_version: &str, dev: &str) -> Result<String, String> {
    let mut app = app();
    if app.is_some() {
        return Ok("the core is already running".into());
    }
    let key: &[u8; KEY_LEN] =
        key.try_into().map_err(|_| format!("the data key has {} bytes, not {KEY_LEN}", key.len()))?;
    let dev = dev_options(dev)?;
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("the app's folder: {e}"))?;
    let secrets =
        Arc::new(SealedStore::open(&dir.join("secrets"), key).map_err(|e| format!("the secret store: {}", e.detail))?);
    let file = FileTrace::open(&dir, "trace.log", Level::Debug, DEFAULT_CAP).map_err(|e| format!("the trace: {e}"))?;
    let trace: Arc<dyn TraceSink> = Arc::new(Tee(file, Logcat { max_level: Level::Info }));
    if !set_after_bind(after_bind(&dev)) {
        return Err("the socket step was already installed".into());
    }
    let service = PhoneService::start(
        PhoneParams {
            default_name: name.to_owned(),
            os_version: os_version.to_owned(),
            secrets,
            dir: dir.clone(),
            interfaces: Arc::clone(networks()) as Arc<dyn NetInterfaces>,
            clock: Arc::new(MonotonicClock::new()),
            trace: Arc::clone(&trace),
            discovery: true,
            dev_loopback: dev.loopback,
        },
        Arc::new(NoWake),
        None,
    )?;
    trace.log(Level::Info, "app", format_args!("core {} started", crate::version()));
    *app = Some(App {
        #[cfg(feature = "dev-harness")]
        dir,
        service,
    });
    Ok("started".into())
}

/// `Core.networks`: Java's newest list of networks. The guard reads it from now on, the process is bound to the
/// approved network (for the discovery library's sockets), and the running core follows the change. Returns what
/// is approved now, for the log.
pub fn networks_changed(bytes: &[u8]) -> Result<String, String> {
    let list = facts::decode(bytes).map_err(|e| e.to_string())?;
    let nets = networks();
    let changed = nets.set(list);
    let process = nets.process_network();
    network::bind_process(process.unwrap_or(network::UNSPECIFIED)).map_err(|e| format!("binding the process: {e}"))?;
    if changed && let Some(app) = app().as_ref() {
        app.service.network_changed();
    }
    let interfaces = nets.list().unwrap_or_default();
    let approved: Vec<String> = swapdesk_lan::guard::approved_addrs(&interfaces)
        .iter()
        .map(|a| format!("{} {}/{}", a.if_name, a.addr, a.prefix))
        .collect();
    Ok(format!("{} network(s), approved: [{}]", nets.snapshot().len(), approved.join(", ")))
}
