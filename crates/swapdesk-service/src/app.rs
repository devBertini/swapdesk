//! The service body, shared by the SCM path and `--console`: open the stores, start the core and the hub, keep the
//! helpers and the UI pipes up, follow the session watcher, stop cleanly. Only wiring: every decision lives in
//! `swapdesk-host`, every Windows call in a `*-win` crate.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_capture_win::QpcClock;
use swapdesk_host::accounts::{Account, AccountFactory, AccountSeat, Accounts, PORT_HELD};
use swapdesk_host::netcache::CachedInterfaces;
use swapdesk_host::service::{ConsoleRead, TerminalEnv};
use swapdesk_host::service::{
    CoreEvent, DEFAULT_SESSION_PORT, MachineShared, ServiceConfig, ServiceCore, ServiceDeps, ServiceError,
};
use swapdesk_host::ui_hub::presence::FilteredPresence;
use swapdesk_host::ui_hub::store::{FileAuditLog, load_settings};
use swapdesk_host::ui_hub::{DevOptions, DnsSdPresence, HubParams, NoPresence, Presence, SystemHooks, UiHub};
use swapdesk_lan::discovery::Config as DnsSdConfig;
use swapdesk_lan::txt::Txt;
use swapdesk_net_win::{NetworkWatcher, WindowsNetInterfaces};
use swapdesk_platform::{Level, NetInterface, NetInterfaces, PtySpawner, SessionEvent, SessionWatcher, TraceSink};
use swapdesk_proto::{Os, PermissionSet};
use swapdesk_secret_win::{FolderSpec, WinSecretStore};
use swapdesk_sys_win::pty::{ConPtySpawner, Shell};
use swapdesk_sys_win::{
    FileTrace, InstallPolicy, KeepAwake, ServiceContext, active_console_session, console_state, current_user_sid,
    send_sas, session_user_sid,
};
use swapdesk_trust::{Identity, SystemWallClock, TrustHandle, WallClock};

use crate::helpers::{CoreOf, HowToStart, Launch, Supervisor};
use crate::options::Options;
use crate::ui_pipe::{Mode, Route, UiPipes};

/// The service's name with the SCM.
pub const SERVICE_NAME: &str = "SwapdeskService";
/// How often the interface list is read to catch what the OS gives no notice of: the network category (Public or
/// Private) and the firewall state. A change of adapter or address is announced by the OS (`NetworkWatcher`) and does not
/// wait for this.
const NETWORK_RECONCILE: Duration = Duration::from_secs(30);
/// The same when the OS notices could not be registered: the old pace, so a network change is still found.
const NETWORK_FALLBACK_POLL: Duration = Duration::from_secs(5);
/// A coarse look at the helpers and the stop request when nothing else has woken the loop: a helper that died has already
/// closed its pipe and woken it, so this only guards against a notice that never came.
const WATCHDOG: Duration = Duration::from_secs(30);

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
    Ok(Env { store: Arc::new(store), trace })
}

/// The discovery record of an account (ACCOUNTS.md 6, GRANTS-SYNC.md 4.5): its slot's session port, and the `minor`
/// key every 1.1 record carries.
pub(crate) fn account_discovery(seat: &AccountSeat) -> DnsSdConfig {
    DnsSdConfig { service_port: seat.session_port, ..DnsSdConfig::default() }
}

/// The name a peer sees of an account until its user changes it (D55): "<computer> · <Windows account>", the
/// computer's name alone when the account's name cannot be read.
fn default_account_name(sid: &str) -> String {
    account_name(&computer_name(), swapdesk_sys_win::account_name_of_sid(sid).ok().as_deref())
}

/// "<computer> · <account>", within the name's limit (64 bytes, cut at a character).
pub(crate) fn account_name(computer: &str, account: Option<&str>) -> String {
    let mut name = match account.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("{computer} · {a}"),
        None => computer.to_string(),
    };
    while name.len() > 64 {
        name.pop();
    }
    name
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

/// What 1.1 reads of the machine's root folder at start (ACCOUNTS.md 10.2, D55): the base port of the slots and
/// whether a 1.0 install left its key there (for the one-time note). The 1.0 key, trust store, logs and every other
/// setting are never read: no account receives any 1.0 data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootFacts {
    pub port: u16,
    pub had_1_0: bool,
}

pub(crate) fn root_facts(root: &WinSecretStore) -> Result<RootFacts, String> {
    let settings = load_settings(root, "")?;
    // Only whether the file is there: the 1.0 key is never loaded.
    let had_1_0 = root.dir().join(swapdesk_trust::identity::IDENTITY_NAME).exists();
    Ok(RootFacts { port: settings.port, had_1_0 })
}

/// An account's own data (ACCOUNTS.md 10.1), all from its own folder: a new account gets a new key, an empty trust
/// store and default settings at its slot's ports.
pub(crate) struct AccountData {
    pub store: Arc<WinSecretStore>,
    pub identity: Arc<Identity>,
    pub trust: Arc<TrustHandle>,
    pub settings: swapdesk_host::ui_hub::MachineSettings,
}

pub(crate) fn open_account_data(
    root: &WinSecretStore,
    seat: &AccountSeat,
    default_name: &str,
    now_ms: u64,
    trace: &dyn TraceSink,
) -> Result<AccountData, String> {
    let store = Arc::new(root.open_account(&seat.sid, Some(trace)).map_err(|e| format!("account folder: {e}"))?);
    let identity = Arc::new(Identity::load_or_create(&*store, now_ms).map_err(|e| format!("identity: {e}"))?);
    let trust = TrustHandle::open(store.clone()).map_err(|e| format!("trust store: {e}"))?;
    let mut settings = load_settings(&*store, default_name)?;
    settings.port = seat.session_port;
    settings.pair_port = seat.pair_port;
    Ok(AccountData { store, identity, trust, settings })
}

/// Who is at the console session now (ACCOUNTS.md 5.1). In development the current user stands for a logged-in user.
/// A logged-in user whose SID cannot be read is `Unreadable`, never nobody: that fails closed.
fn read_console(dev: bool) -> ConsoleRead {
    let Ok(state) = console_state() else { return ConsoleRead::Unreadable };
    if state.user.is_none() {
        return ConsoleRead::Nobody;
    }
    let sid = if dev { current_user_sid().ok() } else { state.session_id.and_then(|s| session_user_sid(s).ok()) };
    sid.map_or(ConsoleRead::Unreadable, ConsoleRead::User)
}

/// How often a console user that could not be read is read again.
const CONSOLE_RETRY: Duration = Duration::from_secs(1);

/// The event channel of every account's core, with its core.
type CoreEvents = Arc<Mutex<Vec<(Arc<ServiceCore>, std::sync::mpsc::Receiver<CoreEvent>)>>>;

fn run_with(opts: &Options, ctx: ServiceContext, env: Env) -> Result<(), String> {
    let ServiceContext { mut watcher, events: ctx_events, stop } = ctx;
    let Env { store, trace } = env;
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

    // What 1.1 reads of the root folder (ACCOUNTS.md 10.2): the base of the port slots and whether 1.0 data is there.
    let mut machine = root_facts(&store)?;
    if opts.instance.is_some() {
        machine.port = DEFAULT_SESSION_PORT + opts.port_offset();
    }
    // The interface list is read at most once a second and again at once when the network changes: the LAN guard asks
    // for it on every incoming connection, pairing and presence on every event.
    let inner: Arc<dyn NetInterfaces> =
        if opts.dev_loopback { Arc::new(LoopbackOnly) } else { Arc::new(WindowsNetInterfaces) };
    let cached = Arc::new(CachedInterfaces::new(inner));
    let interfaces: Arc<dyn NetInterfaces> = cached.clone();
    let clock = Arc::new(QpcClock);
    let session = active_console_session();
    let term_env = WinTerminalEnv::new(opts.console, session);
    // Each account's core reports its events here: the loop below reads them all.
    let core_events: CoreEvents = Arc::default();

    // ACCOUNTS.md 15 S3: one identity, trust store, hub and core per account, built the first time the account needs
    // one (its user opens Swapdesk), and at start for every account the machine knows.
    let factory: AccountFactory = {
        let (root, trace, interfaces, clock, term_env) =
            (Arc::clone(&store), Arc::clone(&trace), Arc::clone(&interfaces), clock.clone(), term_env.clone());
        let (core_events, events, had_1_0) = (Arc::clone(&core_events), ctx_events.clone(), machine.had_1_0);
        let (dev_loopback, dev_mode) = (opts.dev_loopback, opts.console);
        Box::new(move |seat: &AccountSeat, shared: &MachineShared, own: Arc<Mutex<Vec<swapdesk_trust::DeviceId>>>| {
            let wall = SystemWallClock;
            let AccountData { store, identity, trust, settings } =
                open_account_data(&root, seat, &default_account_name(&seat.sid), wall.unix_ms(), &*trace)?;
            let dir = store.dir().to_path_buf();
            let audit = Arc::new(
                FileAuditLog::open(&dir, wall.unix_ms(), Arc::clone(&trace)).map_err(|e| format!("audit log: {e}"))?,
            );
            let presence: Arc<dyn Presence> = if dev_loopback {
                Arc::new(NoPresence)
            } else {
                let own_txt = Txt {
                    name: settings.name.clone(),
                    os: Os::Windows,
                    fingerprint: identity.device_id(),
                    pairing: false,
                };
                let config = account_discovery(seat);
                match DnsSdPresence::start(Arc::clone(&interfaces), own_txt, config) {
                    Ok(p) => Arc::new(FilteredPresence::new(Arc::new(p), own)),
                    Err(e) => {
                        trace.log(Level::Warn, "service", format_args!("discovery is off for an account: {e}"));
                        Arc::new(NoPresence)
                    }
                }
            };
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
                dev: dev_loopback.then(|| DevOptions { pair_listen: vec![lo(seat.pair_port)] }),
            });
            let receiving = trust.read(|s| !s.devices.is_empty());
            let deps = ServiceDeps {
                identity: Arc::clone(&identity),
                trust,
                clock: clock.clone(),
                trace: Arc::clone(&trace),
                ui: hub.host_ui(),
                interfaces: Arc::clone(&interfaces),
            };
            let mut cfg = ServiceConfig::new(&settings.name, Os::Windows, "Windows", deps);
            cfg.session_port = seat.session_port;
            cfg.pair_port = seat.pair_port;
            cfg.win_session = active_console_session().unwrap_or(0);
            cfg.machine = Some(shared.clone());
            cfg.receiving = receiving;
            // A quick action with no user helper attached runs here: lock, sleep and restart as system calls.
            cfg = cfg.with_service_actions(swapdesk_sys_win::actions::WinActions::service());
            if dev_loopback {
                cfg = cfg.dev_loopback(lo(seat.session_port));
            } else {
                // ACCOUNTS.md 5.5: the slot's port, exclusively on every address, from the start.
                cfg.wildcard = Some(Arc::new(|port| {
                    swapdesk_sys_win::udp::bind_exclusive(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
                }));
            }
            let core = match ServiceCore::start(cfg) {
                Ok(c) => Arc::new(c),
                // The slot's port is held: name the holder, and the machine gives the account the next free slot.
                Err(ServiceError::Bind { addr, error }) => {
                    let holder = swapdesk_sys_win::udp::udp_port_owner(addr.port()).map_or_else(
                        || "a program that could not be named".to_string(),
                        |(pid, name)| format!("{} (process {pid})", name.unwrap_or_else(|| "a program".into())),
                    );
                    return Err(format!("{PORT_HELD}{} by {holder} ({error})", addr.port()));
                }
                Err(e) => return Err(e.to_string()),
            };
            hub.set_account(&seat.sid);
            hub.set_had_1_0(had_1_0);
            core.set_account(&seat.sid);
            hub.bind_core(&core);
            core.set_terminal_env(term_env.clone());
            hub.set_console_session(active_console_session());
            hub.set_console_user(read_console(dev_mode).hub_user());
            core.set_console_probe(Arc::new(move || read_console(dev_mode)));
            let rx = core.take_events().ok_or("the core's events were taken")?;
            // The loop sleeps until something wakes it: the core says so after each event it queues.
            core.set_event_waker({
                let events = events.clone();
                Arc::new(move || events.wake())
            });
            lock(&core_events).push((Arc::clone(&core), rx));
            Ok(Account { seat: seat.clone(), device_id: identity.device_id(), hub, core })
        })
    };
    let accounts = Arc::new(Accounts::new(
        machine.port,
        store.clone(),
        factory,
        Box::new(|| SystemWallClock.unix_ms()),
        Arc::clone(&trace),
    ));
    for (sid, why) in accounts.start_known() {
        trace.log(Level::Warn, "service", format_args!("an account could not start ({}): {why}", sid.len()));
    }

    let console = console_state().map_err(|e| format!("console state: {e}"))?;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let mode = if opts.console { Mode::Dev } else { Mode::Production };
    let route: Route = {
        let accounts = Arc::clone(&accounts);
        Arc::new(move |sid: &str| accounts.account(sid).map(|a| a.hub.clone()))
    };
    let pipes = UiPipes::routed(route, Arc::clone(&trace), mode, opts.suffix(), Arc::clone(&stop_flag));
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
    // The helpers attach to the account of the user at the console; with one account only, to that one (the sign-in
    // screen of a single-user machine). S4 of ACCOUNTS.md binds this to the console with the fence.
    let core_of: CoreOf = {
        let (accounts, dev) = (Arc::clone(&accounts), opts.console);
        Arc::new(move || {
            let sid = read_console(dev).hub_user();
            accounts.host_core(sid.as_deref())
        })
    };
    let mut supervisor = Supervisor::new(
        core_of,
        Arc::clone(&trace),
        HowToStart { launch, suffix: opts.suffix(), exe },
        (session, console.user.is_some()),
        {
            let events = ctx_events.clone();
            Arc::new(move || events.wake())
        },
    );
    trace.log(Level::Info, "service", format_args!("running; console session {session:?}"));

    // The OS tells when an adapter or an address changes; the watcher turns a burst of notices into one event. Not in
    // `--dev-loopback`, which has no network.
    let net_watcher = if opts.dev_loopback {
        None
    } else {
        let events = ctx_events.clone();
        match NetworkWatcher::start(move || events.network_changed()) {
            Ok(w) => Some(w),
            Err(e) => {
                trace.log(
                    Level::Warn,
                    "service",
                    format_args!("network notices are unavailable ({e}): polling instead"),
                );
                None
            }
        }
    };
    let reconcile = if net_watcher.is_some() || opts.dev_loopback { NETWORK_RECONCILE } else { NETWORK_FALLBACK_POLL };
    let mut last_net = interfaces.list().ok();
    let mut next_net = Instant::now() + reconcile;
    let mut next_watchdog = Instant::now() + WATCHDOG;
    let mut console_retry: Option<Instant> = None;
    let every_hub = |f: &dyn Fn(&UiHub)| {
        for a in accounts.all() {
            f(&a.hub);
        }
    };
    while !stop.is_set() {
        // Sleep until the earliest of: a restart that is due, the next network look, the watchdog. A session event, a
        // core event, a finished helper start or the network watcher wakes the loop sooner.
        let deadline =
            [supervisor.next_wake(), Some(next_net), Some(next_watchdog), console_retry].into_iter().flatten().min();
        let timeout = deadline.map_or(WATCHDOG, |d| d.saturating_duration_since(Instant::now()));
        match watcher.next_event(timeout) {
            Ok(Some(SessionEvent::ConsoleChanged(state))) => {
                let read = read_console(opts.console);
                console_retry = (read == ConsoleRead::Unreadable).then(|| Instant::now() + CONSOLE_RETRY);
                let user = read.hub_user();
                every_hub(&|h| {
                    h.set_console_session(state.session_id);
                    h.set_console_user(user.clone());
                });
                term_env.set_session(state.session_id);
                if let Some(s) = state.session_id {
                    pipes.ensure(s);
                }
                supervisor.console_changed(state.session_id, state.user.is_some());
            }
            Ok(Some(SessionEvent::DesktopSwitched(d))) => every_hub(&|h| h.set_desktop(d)),
            Ok(Some(SessionEvent::NetworkChanged)) => {
                cached.invalidate();
                last_net = interfaces.list().ok();
                next_net = Instant::now() + reconcile;
                every_hub(&|h| h.network_changed());
            }
            Ok(Some(SessionEvent::Resumed)) => {
                trace.log(Level::Info, "service", format_args!("resumed from sleep"));
                cached.invalidate();
                last_net = interfaces.list().ok();
                next_net = Instant::now() + reconcile;
                every_hub(&|h| h.network_changed());
            }
            Ok(Some(SessionEvent::Suspending)) => trace.log(Level::Info, "service", format_args!("suspending")),
            Ok(Some(SessionEvent::DisplaysChanged)) => {
                trace.log(Level::Debug, "service", format_args!("displays changed"))
            }
            Ok(None) => {}
            // The watcher closed: the service is stopping.
            Err(_) => break,
        }
        let pending: Vec<(Arc<ServiceCore>, CoreEvent)> = lock(&core_events)
            .iter()
            .flat_map(|(core, rx)| rx.try_iter().map(|ev| (Arc::clone(core), ev)).collect::<Vec<_>>())
            .collect();
        for (core, ev) in pending {
            match ev {
                CoreEvent::HelperGone { pid, .. } => supervisor.helper_gone(pid),
                CoreEvent::SasRequested { .. } => sas(&core, &*trace),
                CoreEvent::HelperAttached { .. } | CoreEvent::HelperFault { .. } => {}
            }
        }
        // A logged-in user that could not be read keeps every account away (fails closed) until it can be.
        if console_retry.is_some_and(|t| Instant::now() >= t) {
            let read = read_console(opts.console);
            console_retry = (read == ConsoleRead::Unreadable).then(|| Instant::now() + CONSOLE_RETRY);
            if read != ConsoleRead::Unreadable {
                let user = read.hub_user();
                every_hub(&|h| h.set_console_user(user.clone()));
            }
        }
        supervisor.tick();
        let now = Instant::now();
        if now >= next_net {
            next_net = now + reconcile;
            // What the OS gives no notice of (category, firewall) shows only in a fresh read.
            cached.invalidate();
            let fresh = interfaces.list().ok();
            if fresh != last_net {
                last_net = fresh;
                every_hub(&|h| h.network_changed());
            }
        }
        if now >= next_watchdog {
            next_watchdog = now + WATCHDOG;
        }
    }
    drop(net_watcher);

    trace.log(Level::Info, "service", format_args!("stopping"));
    every_hub(&|h| h.shutdown());
    supervisor.stop_all();
    pipes.stop_all();
    drop(supervisor);
    // The last handles: each core ends its sessions and its runtime.
    lock(&core_events).clear();
    drop(accounts);
    trace.log(Level::Info, "service", format_args!("stopped"));
    Ok(())
}

#[cfg(test)]
#[path = "first_start_tests.rs"]
mod first_start_tests;
