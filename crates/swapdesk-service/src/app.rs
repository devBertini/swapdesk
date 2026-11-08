//! The service body, shared by the SCM path and `--console`: open the stores, start the core and the hub, keep the
//! helpers and the UI pipes up, follow the session watcher, stop cleanly. Only wiring: every decision lives in
//! `swapdesk-host`, every Windows call in a `*-win` crate.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_capture_win::QpcClock;
use swapdesk_host::service::TerminalEnv;
use swapdesk_host::service::{
    CoreEvent, DEFAULT_PAIR_PORT, DEFAULT_SESSION_PORT, ServiceConfig, ServiceCore, ServiceDeps,
};
use swapdesk_host::ui_hub::store::{FileAuditLog, load_settings};
use swapdesk_host::ui_hub::{DevOptions, DnsSdPresence, HubParams, NoPresence, Presence, SystemHooks, UiHub};
use swapdesk_lan::discovery::Config as DnsSdConfig;
use swapdesk_lan::txt::Txt;
use swapdesk_net_win::WindowsNetInterfaces;
use swapdesk_platform::{Level, NetInterface, NetInterfaces, PtySpawner, SessionEvent, SessionWatcher, TraceSink};
use swapdesk_proto::{Os, PermissionSet};
use swapdesk_secret_win::{FolderSpec, WinSecretStore};
use swapdesk_sys_win::pty::{ConPtySpawner, Shell};
use swapdesk_sys_win::{
    FileTrace, InstallPolicy, KeepAwake, ServiceContext, active_console_session, console_state, current_user_sid,
    send_sas, session_user_sid,
};
use swapdesk_trust::{Identity, SystemWallClock, TrustHandle, WallClock};

use crate::helpers::{Launch, Supervisor};
use crate::options::Options;
use crate::ui_pipe::{Mode, UiPipes};

/// The service's name with the SCM.
pub const SERVICE_NAME: &str = "SwapdeskService";
/// How often the interface list is read for a change (the watcher has no network source yet).
const NETWORK_POLL: Duration = Duration::from_secs(5);
/// How long one look at the session watcher lasts: also the pace of the helper supervision.
const TICK: Duration = Duration::from_millis(250);

/// The dev folder of an instance: `%LOCALAPPDATA%\Swapdesk-dev\<instance>`.
pub fn dev_dir(opts: &Options) -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("%LOCALAPPDATA% is not set")?;
    Ok(PathBuf::from(base).join("Swapdesk-dev").join(opts.instance_name()))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the machine tells the core about terminals (stage 43): who is logged in at the console, and the administrator
/// spawner. The administrator terminal is the one shell this process starts: with the service's own SYSTEM token in
/// session 0 (`ConPtySpawner::current_user` refuses an administrator request from any other token, so a `--console`
/// run as a user never makes one). User terminals never start here: they run in the user helper.
struct WinTerminalEnv {
    /// `--console`: the helpers run as the current user, who is the console user.
    dev: bool,
    session: Mutex<Option<u32>>,
    admin: Arc<dyn PtySpawner>,
}

impl WinTerminalEnv {
    fn new(dev: bool, session: Option<u32>) -> Arc<Self> {
        Arc::new(Self {
            dev,
            session: Mutex::new(session),
            admin: Arc::new(ConPtySpawner::current_user(Shell::user_default(), None)),
        })
    }

    fn set_session(&self, session: Option<u32>) {
        *lock(&self.session) = session;
    }
}

impl TerminalEnv for WinTerminalEnv {
    fn console_user(&self) -> Option<String> {
        if self.dev {
            return current_user_sid().ok();
        }
        // `WTSQueryUserToken` fails when nobody is logged on in the session.
        let session = (*lock(&self.session))?;
        session_user_sid(session).ok()
    }

    fn admin_spawner(&self) -> Option<Arc<dyn PtySpawner>> {
        Some(Arc::clone(&self.admin))
    }
}

/// What the machine does for the hub.
struct WinHooks {
    awake: Mutex<Option<KeepAwake>>,
    trace: Arc<dyn TraceSink>,
}

impl SystemHooks for WinHooks {
    fn keep_awake(&self, on: bool) {
        let mut slot = lock(&self.awake);
        if slot.is_none() {
            match KeepAwake::new() {
                Ok(k) => *slot = Some(k),
                Err(e) => {
                    return self.trace.log(Level::Warn, "service", format_args!("keep-awake is unavailable: {e}"));
                }
            }
        }
        if let Some(Err(e)) = slot.as_ref().map(|k| k.set(on)) {
            self.trace.log(Level::Warn, "service", format_args!("keep-awake {on} failed: {e}"));
        }
    }

    fn lock_screen(&self) {
        // The workstation lock is an action of the user helper (stage 20's ActionRunner); nothing in this build runs it.
        self.trace.log(
            Level::Warn,
            "service",
            format_args!("lock on end was asked; the lock action is not wired in this build"),
        );
    }

    fn service_enabled(&self, on: bool) {
        self.trace.log(
            Level::Info,
            "service",
            format_args!("the service switch is now {on}; the installer owns what it does"),
        );
    }
}

/// No interfaces at all: `--dev-loopback` instances listen on 127.0.0.1 only.
struct LoopbackOnly;

impl NetInterfaces for LoopbackOnly {
    fn list(&self) -> swapdesk_platform::Result<Vec<NetInterface>> {
        Ok(Vec::new())
    }
}

/// The stores and the trace of this run.
struct Env {
    store: Arc<WinSecretStore>,
    dir: PathBuf,
    trace: Arc<dyn TraceSink>,
}

fn open_env(opts: &Options) -> Result<Env, String> {
    let level = if opts.console { Level::Debug } else { Level::Info };
    let (store, dir) = if opts.console {
        let dir = dev_dir(opts)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // The current user takes the place SYSTEM has in production (the DACL keeps its shape).
        let sid = current_user_sid().map_err(|e| e.to_string())?;
        let spec = FolderSpec { owner: sid.clone(), full_control: vec![sid.clone()], trusted_owners: vec![sid] };
        let store = WinSecretStore::open_in(&spec, &dir, None).map_err(|e| format!("secret store: {e}"))?;
        (store, dir)
    } else {
        let store = WinSecretStore::open(None).map_err(|e| format!("secret store: {e}"))?;
        let dir = store.dir().to_path_buf();
        (store, dir)
    };
    // C2.3: the trace lives in the C5 folder in production (same DACL); in the dev folder in development.
    let trace_dir = if opts.console { dir.clone() } else { store.dir().to_path_buf() };
    let trace: Arc<dyn TraceSink> =
        Arc::new(FileTrace::open(&trace_dir, "trace.log", level).map_err(|e| format!("trace log: {e}"))?);
    // D12: a panic leaves its record in the same folder as the trace.
    swapdesk_sys_win::install_panic_hook(&trace_dir, "swapdesk-service");
    Ok(Env { store: Arc::new(store), dir, trace })
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME").ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Swapdesk PC".to_string())
}

fn sas(core: &ServiceCore, trace: &dyn TraceSink) {
    // A helper asked for Ctrl+Alt+Del: only an UNATTENDED session may have it (D11).
    if core.host().sessions().iter().any(|s| s.granted.contains(PermissionSet::UNATTENDED)) {
        match send_sas() {
            Ok(()) => trace.log(Level::Info, "service", format_args!("the secure attention sequence was sent")),
            Err(e) => trace.log(Level::Warn, "service", format_args!("SendSAS failed: {e}")),
        }
    } else {
        trace.log(
            Level::Warn,
            "service",
            format_args!(
                "a helper asked for the secure attention sequence in a session that is not UNATTENDED; ignored"
            ),
        );
    }
}

/// Runs the service until `ctx.stop` is set or the watcher closes. The exit code: 0 clean, 1 could not start.
pub fn run(opts: Options, ctx: ServiceContext) -> u32 {
    let env = match open_env(&opts) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("swapdesk-service: {e}");
            return 1;
        }
    };
    let trace = Arc::clone(&env.trace);
    match run_with(&opts, ctx, env) {
        Ok(()) => 0,
        Err(e) => {
            trace.log(Level::Error, "service", format_args!("did not start: {e}"));
            eprintln!("swapdesk-service: {e}");
            1
        }
    }
}

fn run_with(opts: &Options, ctx: ServiceContext, env: Env) -> Result<(), String> {
    let ServiceContext { mut watcher, events: _events, stop } = ctx;
    let Env { store, dir, trace } = env;
    let wall = SystemWallClock;
    trace.log(
        Level::Info,
        "service",
        format_args!(
            "starting ({}, instance {:?}, loopback {})",
            if opts.console { "console" } else { "service" },
            opts.instance,
            opts.dev_loopback
        ),
    );

    let identity = Arc::new(Identity::load_or_create(&*store, wall.unix_ms()).map_err(|e| format!("identity: {e}"))?);
    let trust = TrustHandle::open(store.clone()).map_err(|e| format!("trust store: {e}"))?;
    let mut settings = load_settings(&*store, &computer_name())?;
    if opts.instance.is_some() {
        settings.port = DEFAULT_SESSION_PORT + opts.port_offset();
        settings.pair_port = DEFAULT_PAIR_PORT + opts.port_offset();
    }
    let audit =
        Arc::new(FileAuditLog::open(&dir, wall.unix_ms(), Arc::clone(&trace)).map_err(|e| format!("audit log: {e}"))?);
    let interfaces: Arc<dyn NetInterfaces> =
        if opts.dev_loopback { Arc::new(LoopbackOnly) } else { Arc::new(WindowsNetInterfaces) };
    let presence: Arc<dyn Presence> = if opts.dev_loopback {
        Arc::new(NoPresence)
    } else {
        let own =
            Txt { name: settings.name.clone(), os: Os::Windows, fingerprint: identity.device_id(), pairing: false };
        match DnsSdPresence::start(
            Arc::clone(&interfaces),
            own,
            DnsSdConfig { service_port: settings.port, ..DnsSdConfig::default() },
        ) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                trace.log(Level::Warn, "service", format_args!("discovery is off: {e}"));
                Arc::new(NoPresence)
            }
        }
    };
    let clock = Arc::new(QpcClock);
    let lo = |port: u16| SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let hub = UiHub::new(HubParams {
        os: Os::Windows,
        // No safe call gives the build number; a version query belongs in `swapdesk-sys-win`.
        os_version: "Windows".into(),
        identity: Arc::clone(&identity),
        trust: Arc::clone(&trust),
        secrets: store.clone(),
        settings: settings.clone(),
        interfaces: Arc::clone(&interfaces),
        presence,
        audit,
        clock: clock.clone(),
        wall: Arc::new(SystemWallClock),
        trace: Arc::clone(&trace),
        hooks: Arc::new(WinHooks { awake: Mutex::new(None), trace: Arc::clone(&trace) }),
        dev: opts.dev_loopback.then(|| DevOptions { pair_listen: vec![lo(settings.pair_port)] }),
    });

    let console = console_state().map_err(|e| format!("console state: {e}"))?;
    let session = active_console_session();
    let deps = ServiceDeps {
        identity,
        trust,
        clock,
        trace: Arc::clone(&trace),
        ui: hub.host_ui(),
        interfaces: Arc::clone(&interfaces),
    };
    let mut cfg = ServiceConfig::new(&settings.name, Os::Windows, "Windows", deps);
    cfg.session_port = settings.port;
    cfg.pair_port = settings.pair_port;
    cfg.win_session = session.unwrap_or(0);
    // A quick action with no user helper attached runs here: lock, sleep and restart as system calls (stage 42).
    cfg = cfg.with_service_actions(swapdesk_sys_win::actions::WinActions::service());
    if opts.dev_loopback {
        cfg = cfg.dev_loopback(lo(settings.port));
    }
    let core = Arc::new(ServiceCore::start(cfg).map_err(|e| e.to_string())?);
    hub.bind_core(&core);
    let term_env = WinTerminalEnv::new(opts.console, session);
    core.set_terminal_env(term_env.clone());
    hub.set_console_session(session);
    let events = core.take_events().ok_or("the core's events were taken")?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let mode = if opts.console { Mode::Dev } else { Mode::Production };
    let pipes = UiPipes::new(hub.clone(), Arc::clone(&trace), mode, opts.suffix(), Arc::clone(&stop_flag));
    if let Some(s) = session {
        pipes.ensure(s);
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("swapdesk-helper.exe");
    let launch = if opts.console {
        Launch::Dev
    } else {
        Launch::Production(InstallPolicy::production().map_err(|e| e.to_string())?)
    };
    if !exe.exists() {
        trace.log(Level::Error, "service", format_args!("{} does not exist: no helper can start", exe.display()));
    }
    let mut supervisor = Supervisor::new(
        Arc::clone(&core),
        Arc::clone(&trace),
        launch,
        opts.suffix(),
        exe,
        session,
        console.user.is_some(),
    );
    trace.log(Level::Info, "service", format_args!("running; console session {session:?}"));

    let mut last_net = interfaces.list().ok();
    let mut next_net = Instant::now() + NETWORK_POLL;
    while !stop.is_set() {
        match watcher.next_event(TICK) {
            Ok(Some(SessionEvent::ConsoleChanged(state))) => {
                hub.set_console_session(state.session_id);
                term_env.set_session(state.session_id);
                if let Some(s) = state.session_id {
                    pipes.ensure(s);
                }
                supervisor.console_changed(state.session_id, state.user.is_some());
            }
            Ok(Some(SessionEvent::DesktopSwitched(d))) => {
                hub.set_desktop(d);
                core.host().set_desktop(d);
            }
            Ok(Some(SessionEvent::NetworkChanged)) => hub.network_changed(),
            Ok(Some(SessionEvent::Resumed)) => {
                trace.log(Level::Info, "service", format_args!("resumed from sleep"));
                hub.network_changed();
            }
            Ok(Some(SessionEvent::Suspending)) => trace.log(Level::Info, "service", format_args!("suspending")),
            Ok(Some(SessionEvent::DisplaysChanged)) => {
                trace.log(Level::Debug, "service", format_args!("displays changed"))
            }
            Ok(None) => {}
            // The watcher closed: the service is stopping.
            Err(_) => break,
        }
        while let Ok(ev) = events.try_recv() {
            match ev {
                CoreEvent::HelperGone { pid, .. } => supervisor.helper_gone(pid),
                CoreEvent::SasRequested { .. } => sas(&core, &*trace),
                CoreEvent::HelperAttached { .. } | CoreEvent::HelperFault { .. } => {}
            }
        }
        supervisor.tick();
        if Instant::now() >= next_net {
            next_net = Instant::now() + NETWORK_POLL;
            let now = interfaces.list().ok();
            if now != last_net {
                last_net = now;
                hub.network_changed();
            }
        }
    }

    trace.log(Level::Info, "service", format_args!("stopping"));
    hub.shutdown();
    supervisor.stop_all();
    pipes.stop_all();
    drop(supervisor);
    // The last handle: the core ends its sessions and its runtime.
    drop(core);
    drop(hub);
    trace.log(Level::Info, "service", format_args!("stopped"));
    Ok(())
}
