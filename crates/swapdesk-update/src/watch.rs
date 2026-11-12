//! The Mac watcher's sequence (`docs/UPDATES.md` 7.2 steps 5 to 7), OS-free: after the app swapped the staged bundle
//! into its own path and quit, the watcher (the old bundle's binary at its hidden path) waits for the old process to
//! end, opens the new app, waits for its `committed`, and swaps back and reopens the old app when it does not come.
//! `swapdesk-sys-mac` `update_mac.rs` implements [`Watcher`]; tests fake it.

use std::time::Duration;

use crate::Version;
use crate::apply::{COMMIT_WAIT, Journal, Outcome};

/// How long the app that started the watcher has to quit.
pub const QUIT_WAIT: Duration = Duration::from_secs(30);

/// What the watcher does to the Mac.
pub trait Watcher {
    /// Waits up to `limit` for the process that started the watcher to end.
    fn wait_quit(&mut self, limit: Duration) -> bool;
    /// Opens the app at the running path (the new bundle after the swap).
    fn open(&mut self) -> Result<(), String>;
    /// Waits up to `limit` for the journal's `committed <new>`.
    fn wait_commit(&mut self, new: &Version, limit: Duration) -> bool;
    /// Ends the app running from the running path.
    fn end_running(&mut self);
    /// Swaps the hidden bundle and the running one again (`RENAME_SWAP`).
    fn swap_back(&mut self) -> Result<(), String>;
    /// Deletes the hidden bundle (the old one, after a commit).
    fn remove_hidden(&mut self);
    fn journal(&mut self, entry: &Journal) -> Result<(), String>;
}

/// The watcher's run: `Committed` when the new app commits; `RolledBack` when the old app runs again from its path;
/// `Broken` when the swap back failed (the old bundle waits at its hidden path).
pub fn watch(w: &mut dyn Watcher, old: &Version, new: &Version) -> Outcome {
    if !w.wait_quit(QUIT_WAIT) {
        // The old app did not quit: it still runs from the running path, which now holds the new bundle. Swap back so
        // that the next start is the old version again.
        return back(w, old, new, "the app did not quit".into());
    }
    if let Err(e) = w.open() {
        return back(w, old, new, format!("opening the new app: {e}"));
    }
    if w.wait_commit(new, COMMIT_WAIT) {
        w.remove_hidden();
        return Outcome::Committed;
    }
    w.end_running();
    back(w, old, new, "the new app did not commit in time".into())
}

fn back(w: &mut dyn Watcher, old: &Version, new: &Version, why: String) -> Outcome {
    if let Err(e) = w.swap_back() {
        let _ = w.journal(&Journal::Broken { old: old.clone(), new: new.clone() });
        return Outcome::Broken(format!("{why}; the swap back failed too: {e}"));
    }
    let _ = w.journal(&Journal::Failed { old: old.clone(), new: new.clone() });
    match w.open() {
        Ok(()) => Outcome::RolledBack(why),
        // The old bundle is in place; the person opens it.
        Err(e) => Outcome::RolledBack(format!("{why}; reopening the old app failed: {e}")),
    }
}

#[cfg(test)]
mod tests;
