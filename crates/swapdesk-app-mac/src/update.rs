//! The updater of `Swapdesk.app` (`docs/UPDATES.md` 2.2, 7): one per installed app, in the app's own service, never in
//! a scratch run. A verified package becomes a bundle beside the running one, checked against the running app's
//! signing identity, swapped in with `RENAME_SWAP`; the old bundle's binary then runs as the watcher
//! (`--update-watch`), the app quits, and the watcher opens the new app and waits for its `committed`.
//!
//! Files: `~/Library/Application Support/Swapdesk/updates/` (0700): the downloaded package, `update-state.txt`, the
//! `journal` and `watcher.log`.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use swapdesk_host::ui_hub::update::{Updater, UpdaterHooks};
use swapdesk_platform::{Level, TraceSink};
use swapdesk_sys_mac::update_mac::{
    LocationRefusal, MacWatcher, append_journal, bundle_of, check_location, executable_of, hidden_path, read_journal,
    same_signer, stage_bundle, swap,
};
use swapdesk_trust::{SystemWallClock, WallClock};
use swapdesk_update::Version;
use swapdesk_update::apply::{Journal, Outcome};
use swapdesk_update::manager::InstallFailure;

/// The updater's folder inside the app's support folder.
pub const FOLDER: &str = "updates";

/// This build's version.
pub fn running_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version { major: 0, minor: 0, patch: 0, pre: None })
}

/// The live-session count of the app's one hub, known once the service is up.
pub type LiveSessions = Arc<std::sync::OnceLock<Box<dyn Fn() -> usize + Send + Sync>>>;

/// The app's updater; `None` when the folder cannot be made or the app does not run from a bundle.
pub fn app_updater(support: &Path, live: LiveSessions, trace: Arc<dyn TraceSink>) -> Option<Arc<Updater>> {
    use swapdesk_update::manager::{FIRST_DELAY_MS, PERIOD_JITTER_MS, Setup, UpdateManager};
    let dir = support.join(FOLDER);
    if std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o700)))
        .is_err()
    {
        trace.log(Level::Warn, "update", format_args!("the update folder could not be made: no updater"));
        return None;
    }
    let spread =
        || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as u64);
    let setup = Setup {
        running: running_version(),
        target: "macos-arm64".into(),
        base: "https://github.com/devBertini/swapdesk/releases".into(),
        keys: swapdesk_update::keys::RELEASE_KEYS.to_vec(),
        dev: false,
        first_delay_ms: FIRST_DELAY_MS.0 + spread() % (FIRST_DELAY_MS.1 - FIRST_DELAY_MS.0),
    };
    let now = || SystemWallClock.unix_ms();
    let manager = UpdateManager::new(
        setup,
        Arc::new(swapdesk_sys_mac::http::MacHttp::new()),
        Arc::new(StateFile(dir.join("update-state.txt"))),
        Arc::new(UpdateDir(dir.clone())),
        now(),
    );
    let ready_trace = Arc::clone(&trace);
    let hooks = UpdaterHooks {
        now_ms: Box::new(now),
        jitter_ms: Box::new(move || (spread() % (2 * PERIOD_JITTER_MS)) as i64 - PERIOD_JITTER_MS as i64),
        live_sessions: Box::new(move || live.get().map_or(0, |count| count())),
        ready: Box::new(move |path, new| {
            let swapped = swap_in(&dir, &path, new);
            match &swapped {
                Ok(()) => {
                    ready_trace.log(Level::Info, "update", format_args!("{new} is in place: the app quits for it"));
                    quit_app();
                }
                Err((_, e)) => {
                    ready_trace.log(Level::Error, "update", format_args!("the update to {new} did not run: {e}"))
                }
            }
            swapped.map_err(|(why, _)| why)
        }),
    };
    let updater = Updater::new(manager, hooks);
    // The schedule (UPDATES.md 2.2): ticked every minute.
    let ticking = Arc::downgrade(&updater);
    let _ = std::thread::Builder::new().name("swapdesk-update-tick".into()).spawn(move || {
        while let Some(u) = ticking.upgrade() {
            u.tick();
            drop(u);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    Some(updater)
}

/// The text a location refusal is told with (UPDATES.md 10.4 holds the interface's words).
fn refusal(r: &LocationRefusal) -> &'static str {
    match r {
        LocationRefusal::Translocated => "the app runs from a translocated copy: move it to Applications",
        LocationRefusal::NotWritable => "only an administrator of this Mac can replace the app",
    }
}

/// UPDATES.md 7.2 steps 2 to 5: the location, the staged bundle, its signature, the journal, the swap and the watcher.
/// On an error before the swap nothing changed; after it, the swap is undone. The error says what the interface tells.
fn swap_in(dir: &Path, package: &Path, new: &Version) -> Result<(), (InstallFailure, String)> {
    if let Some(exe) = std::env::current_exe().ok().as_deref().and_then(bundle_of)
        && let Err(r) = check_location(&exe)
    {
        let why = match r {
            LocationRefusal::Translocated => InstallFailure::Translocated,
            LocationRefusal::NotWritable => InstallFailure::Unwritable,
        };
        let _ = std::fs::remove_file(package);
        return Err((why, refusal(&r).to_string()));
    }
    swap_in_checked(dir, package, new).map_err(|e| (InstallFailure::Disk, e))
}

fn swap_in_checked(dir: &Path, package: &Path, new: &Version) -> Result<(), String> {
    let old = running_version();
    let exe = std::env::current_exe().map_err(|e| format!("this app's path: {e}"))?;
    let running = bundle_of(&exe).ok_or("this app does not run from a bundle")?;
    check_location(&running).map_err(|r| refusal(&r).to_string())?;
    let bytes = std::fs::read(package).map_err(|e| format!("reading the package: {e}"));
    let _ = std::fs::remove_file(package);
    let pkg = swapdesk_update::package::Package::read(&bytes?).map_err(|e| format!("the package: {e:?}"))?;
    let hidden = hidden_path(&running, new);
    stage_bundle(&pkg, &hidden)?;
    if let Err(e) = same_signer(&hidden, &running) {
        let _ = std::fs::remove_dir_all(&hidden);
        return Err(format!("the new app is {e}"));
    }
    let journal = dir.join(swapdesk_sys_mac::update_mac::JOURNAL);
    append_journal(&journal, &Journal::Applying { old: old.clone(), new: new.clone() })
        .map_err(|e| format!("the journal: {e}"))?;
    swap(&hidden, &running).map_err(|e| format!("the swap: {e}"))?;
    // The old bundle is now at the hidden path: its binary watches.
    let started =
        executable_of(&hidden).ok_or_else(|| "the old bundle has no executable".to_string()).and_then(|watcher| {
            std::process::Command::new(watcher)
                .arg("--update-watch")
                .arg(std::process::id().to_string())
                .arg(old.to_string())
                .arg(new.to_string())
                .arg(&running)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                // Its own process group: it outlives the app.
                .process_group(0)
                .spawn()
                .map(drop)
                .map_err(|e| format!("starting the watcher: {e}"))
        });
    if let Err(e) = started {
        let _ = swap(&hidden, &running);
        let _ = append_journal(&journal, &Journal::Failed { old, new: new.clone() });
        let _ = std::fs::remove_dir_all(&hidden);
        return Err(e);
    }
    Ok(())
}

/// Asks AppKit to quit (on the main thread); the `atexit` teardown runs as on any quit.
fn quit_app() {
    dispatch2::DispatchQueue::main().exec_async(|| {
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            objc2_app_kit::NSApplication::sharedApplication(mtm).terminate(None);
        }
    });
}

/// The watcher (`--update-watch <pid> <old> <new> <running bundle>`, UPDATES.md 7.2 steps 6 and 7): no window, no
/// `NSApplication`. Exit codes: 0 committed, 1 rolled back, 3 broken, 2 refused.
pub fn watch(pid: &str, old: &str, new: &str, running: &str) -> i32 {
    let (Ok(pid), Ok(old), Ok(new)) = (pid.parse::<i32>(), Version::parse(old), Version::parse(new)) else { return 2 };
    let Some(home) = std::env::var_os("HOME") else { return 2 };
    let dir = Path::new(&home).join("Library").join("Application Support").join("Swapdesk").join(FOLDER);
    let Some(hidden) = std::env::current_exe().ok().as_deref().and_then(bundle_of) else { return 2 };
    let log = |line: &str| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("watcher.log")) {
            let _ = writeln!(f, "{} {line}", SystemWallClock.unix_ms());
        }
    };
    let mut watcher = MacWatcher {
        running: PathBuf::from(running),
        hidden,
        pid,
        journal: dir.join(swapdesk_sys_mac::update_mac::JOURNAL),
        poll: Duration::from_millis(250),
    };
    log(&format!("watching {old} -> {new}"));
    let outcome = swapdesk_update::watch::watch(&mut watcher, &old, &new);
    log(&format!("{outcome:?}"));
    match outcome {
        Outcome::Committed => 0,
        Outcome::RolledBack(_) | Outcome::Refused(_) => 1,
        Outcome::Broken(_) => 3,
    }
}

/// What a starting app does with the journal: the new version commits (its bundle was checked whole before the swap,
/// so there is no mixed set to finish on a Mac); the old version that finds `applying` with no watcher alive was never
/// swapped or was swapped back, and records `failed`. Returns how the last update ended, once: `Ok` this version came
/// by it, `Err` the version rolled back to this one. The journal goes once the update has ended.
pub fn at_start(support: &Path, trace: &dyn TraceSink) -> Option<Result<(), Version>> {
    let dir = support.join(FOLDER);
    let journal = dir.join(swapdesk_sys_mac::update_mac::JOURNAL);
    let me = running_version();
    let running = std::env::current_exe().ok().as_deref().and_then(bundle_of)?;
    let last = read_journal(&journal)?;
    match last {
        Journal::Applying { new, .. } if new == me => {
            match append_journal(&journal, &Journal::Committed { new: me.clone() }) {
                Ok(()) => {
                    trace.log(Level::Info, "update", format_args!("updated to {me}: committed"));
                    Some(Ok(()))
                }
                Err(e) => {
                    trace.log(Level::Error, "update", format_args!("the commit of {me} was not written: {e}"));
                    None
                }
            }
        }
        Journal::Applying { old, new } if old == me => {
            let watcher_alive = executable_of(&hidden_path(&running, &new))
                .is_some_and(|exe| !swapdesk_sys_mac::update_mac::pids_of(&exe).is_empty());
            if !watcher_alive {
                let _ = append_journal(&journal, &Journal::Failed { old, new });
            }
            None
        }
        Journal::Committed { new } => {
            // The old bundle at the hidden path goes (the watcher removes it; a power cut may have left it).
            let _ = std::fs::remove_dir_all(hidden_path(&running, &new));
            let _ = std::fs::remove_file(&journal);
            None
        }
        Journal::Failed { old, new } if old == me => {
            let _ = std::fs::remove_dir_all(hidden_path(&running, &new));
            let _ = std::fs::remove_file(&journal);
            trace.log(Level::Warn, "update", format_args!("{new} did not start; {me} was restored"));
            Some(Err(new))
        }
        Journal::Broken { old, new } => {
            trace.log(
                Level::Error,
                "update",
                format_args!(
                    "{new} did not start and {old} could not be put back: it waits at {}",
                    hidden_path(&running, &new).display()
                ),
            );
            None
        }
        _ => None,
    }
}

/// The updater's state file.
struct StateFile(PathBuf);

impl swapdesk_update::manager::SavedStore for StateFile {
    fn load(&self) -> swapdesk_update::manager::Saved {
        std::fs::read_to_string(&self.0).map(|t| swapdesk_update::manager::Saved::from_text(&t)).unwrap_or_default()
    }

    fn save(&self, saved: &swapdesk_update::manager::Saved) {
        let _ = std::fs::write(&self.0, saved.to_text());
    }
}

/// The updater's folder.
struct UpdateDir(PathBuf);

impl swapdesk_update::manager::UpdateFolder for UpdateDir {
    fn create(&self, name: &str) -> std::io::Result<Box<dyn std::io::Write + Send>> {
        Ok(Box::new(std::fs::File::create(self.0.join(name))?))
    }

    fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.0.join(name));
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn free_bytes(&self) -> Option<u64> {
        self.0.ancestors().find(|p| p.is_dir()).and_then(swapdesk_sys_mac::free_bytes)
    }
}
