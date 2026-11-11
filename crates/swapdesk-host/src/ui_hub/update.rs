//! The machine's updater as the interfaces reach it (UPDATES.md 2.2, 10, 13): one [`Updater`] per machine, shared by
//! the account hubs (the service builds it; the Mac app too, for its one account). Ops 51 to 56; every change is told
//! to every hub's interfaces as `UpdateChanged`. The long work (a check, a download) runs on a thread of its own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use swapdesk_ipc::{ApiCode, Notify, ReplyBody, UpdatePhase, UpdateView};
use swapdesk_update::Version;
use swapdesk_update::manager::{CheckFailure, InstallAsk, InstallFailure, Phase, UpdateManager};

use super::{Conn, Hub};

/// The hook that hands a verified package to the applier.
pub type ReadyHook = Box<dyn Fn(std::path::PathBuf, &Version) -> Result<(), String> + Send + Sync>;

/// What the updater needs of the machine, given by the service.
pub struct UpdaterHooks {
    /// Milliseconds of the wall clock (the schedule and "last check").
    pub now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
    /// A jitter within ±1 hour for the next period, picked at random.
    pub jitter_ms: Box<dyn Fn() -> i64 + Send + Sync>,
    /// Live sessions on the whole machine, hosted or viewed, any account (10.3).
    pub live_sessions: Box<dyn Fn() -> usize + Send + Sync>,
    /// A verified package of this version is ready at this path, and every interface was told `UpdateRestarting`:
    /// the applier takes over (UPDATES.md 6.2 step 4). An `Err` is told as a failed install.
    pub ready: ReadyHook,
}

/// The machine's updater.
pub struct Updater {
    manager: UpdateManager,
    hooks: UpdaterHooks,
    cancel: AtomicBool,
    hubs: Mutex<Vec<Weak<Hub>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The wire view of the manager's.
pub fn view_to_wire(v: &swapdesk_update::manager::UpdateView) -> UpdateView {
    let phase = match &v.phase {
        Phase::Idle => UpdatePhase::Idle,
        Phase::Checking => UpdatePhase::Checking,
        Phase::Offered { version, manual } => UpdatePhase::Offered { version: version.to_string(), manual: *manual },
        Phase::Downloading { version, done, total } => {
            UpdatePhase::Downloading { version: version.to_string(), done: *done, total: *total }
        }
        Phase::Verifying { version } => UpdatePhase::Verifying { version: version.to_string() },
        Phase::Ready { version } => UpdatePhase::Ready { version: version.to_string() },
        Phase::Failed(f) => UpdatePhase::Failed { reason: failure_tag(*f) },
    };
    UpdateView {
        running: v.running.to_string(),
        phase,
        check_on: v.check_on,
        last_check_ms: v.last_check_ms,
        dismissed: v.dismissed.as_ref().map(ToString::to_string),
        last_failure: v.last_failure.map(|f| match f {
            CheckFailure::Network => 0,
            CheckFailure::RateLimited => 1,
            CheckFailure::Signature => 2,
        }),
        rolled_back: v.rolled_back.as_ref().map(ToString::to_string),
        updated: v.updated,
    }
}

/// `InstallFailure` as the tag `UpdatePhase::Failed` carries.
pub fn failure_tag(f: InstallFailure) -> u8 {
    match f {
        InstallFailure::NotConsole => 0,
        InstallFailure::NeedsConfirmation => 1,
        InstallFailure::NotOffered => 2,
        InstallFailure::Busy => 3,
        InstallFailure::Download => 4,
        InstallFailure::Mismatch => 5,
        InstallFailure::Disk => 6,
        InstallFailure::Cancelled => 7,
    }
}

impl Updater {
    pub fn new(manager: UpdateManager, hooks: UpdaterHooks) -> Arc<Self> {
        Arc::new(Self { manager, hooks, cancel: AtomicBool::new(false), hubs: Mutex::new(Vec::new()) })
    }

    pub fn view(&self) -> UpdateView {
        view_to_wire(&self.manager.view())
    }

    /// The applier restored the running version because `version` did not start (UPDATES.md 6.4): it is not offered
    /// again by itself.
    pub fn rolled_back(&self, version: &Version) {
        self.manager.rolled_back(version);
        self.changed();
    }

    /// The running version came by an update that committed at this start: its card shows once.
    pub fn updated(&self) {
        self.manager.updated();
        self.changed();
    }

    /// Every account hub's interfaces hear of the changes.
    pub(super) fn subscribe(&self, hub: &Arc<Hub>) {
        let mut hubs = lock(&self.hubs);
        hubs.retain(|h| h.strong_count() > 0);
        hubs.push(Arc::downgrade(hub));
    }

    fn changed(&self) {
        let view = self.view();
        for hub in lock(&self.hubs).iter().filter_map(Weak::upgrade) {
            hub.notify(Notify::UpdateChanged(view.clone()));
        }
    }

    /// The schedule: the caller ticks it (every minute is plenty); a check that ran is told.
    pub fn tick(&self) {
        if self.manager.tick((self.hooks.now_ms)(), (self.hooks.jitter_ms)()) {
            self.changed();
        }
    }

    fn check_now(self: &Arc<Self>) {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let _ = me.manager.check((me.hooks.now_ms)(), (me.hooks.jitter_ms)());
            me.changed();
        });
    }

    fn install(self: &Arc<Self>, version: Version, ask: InstallAsk) {
        self.cancel.store(false, Ordering::Release);
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            let result = me.manager.install(&version, ask, &me.cancel);
            me.changed();
            if let Ok(path) = result {
                // Each interface saves its window state and exits (UPDATES.md 6.2 step 4); the applier then stops the
                // service.
                for hub in lock(&me.hubs).iter().filter_map(Weak::upgrade) {
                    hub.notify(Notify::UpdateRestarting);
                }
                if (me.hooks.ready)(path, &version).is_err() {
                    me.manager.ready_failed();
                    me.changed();
                }
            }
        });
    }
}

impl Hub {
    /// Ops 51 to 56 (UPDATES.md 10, 13). `NotFound` on a machine whose service has no updater (a development
    /// instance builds none).
    pub(super) fn update_op(self: &Arc<Self>, conn: &Conn, op: &swapdesk_ipc::UiOp) -> Result<ReplyBody, ApiCode> {
        use swapdesk_ipc::UiOp as O;
        let Some(updater) = self.updater.get().cloned() else { return Err(ApiCode::NotFound) };
        updater.subscribe(self);
        match op {
            O::UpdateGet => Ok(ReplyBody::Update(updater.view())),
            O::UpdateCheckNow => {
                updater.check_now();
                Ok(ReplyBody::Done)
            }
            O::UpdateInstall { version, end_sessions } => {
                let version = Version::parse(version).map_err(|_| ApiCode::Invalid)?;
                let ask = InstallAsk {
                    from_console: self.is_console(conn),
                    live_sessions: (updater.hooks.live_sessions)(),
                    end_sessions: *end_sessions,
                };
                // The refusals that need no download are answered now (10.3).
                if !ask.from_console {
                    return Err(self.api_error(ApiCode::NotPermitted, "an update from a UI that is not the console's"));
                }
                if ask.live_sessions > 0 && !ask.end_sessions {
                    return Err(ApiCode::Busy);
                }
                updater.install(version, ask);
                Ok(ReplyBody::Done)
            }
            O::UpdateCancel => {
                updater.cancel.store(true, Ordering::Release);
                Ok(ReplyBody::Done)
            }
            O::UpdateDismiss { version } => {
                updater.manager.dismiss(&Version::parse(version).map_err(|_| ApiCode::Invalid)?);
                updater.changed();
                Ok(ReplyBody::Done)
            }
            O::UpdateSetCheck { on } => {
                self.require_console(conn, "the update switch from a UI that is not the console's")?;
                updater.manager.set_check(*on);
                updater.changed();
                Ok(ReplyBody::Done)
            }
            _ => Err(ApiCode::Internal),
        }
    }
}
