//! The one hosted session a machine may have, across the cores of its accounts (`docs/ACCOUNTS.md` 5.6).
//!
//! Each account's core caps its own live sessions at one, under its own lock. With several accounts on one machine
//! that is not enough: two unattended entries at the sign-in screen, one to each account, could both commit. So the
//! machine holds one reservation that a core's commit takes atomically before any grant, and that goes when the
//! core's last live session ends. The core's own cap stays as a second check.

use std::sync::{Arc, Mutex, PoisonError};

/// The machine-wide hosting reservation, shared by every account's `HostService`.
#[derive(Debug, Default)]
pub struct MachineHosting {
    /// The core that hosts now (its identity, see [`crate::host::HostService`]); `None` when nobody does.
    holder: Mutex<Option<usize>>,
}

impl MachineHosting {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Takes the reservation for `core`, or keeps it when `core` holds it already. `false` when another core hosts.
    pub(crate) fn take(&self, core: usize) -> bool {
        let mut h = self.holder.lock().unwrap_or_else(PoisonError::into_inner);
        match *h {
            None => {
                *h = Some(core);
                true
            }
            Some(c) => c == core,
        }
    }

    /// `core` hosts no session any more.
    pub(crate) fn release(&self, core: usize) {
        let mut h = self.holder.lock().unwrap_or_else(PoisonError::into_inner);
        if *h == Some(core) {
            *h = None;
        }
    }

    /// Another core than `core` hosts now.
    pub(crate) fn held_by_other(&self, core: usize) -> bool {
        self.holder.lock().unwrap_or_else(PoisonError::into_inner).is_some_and(|c| c != core)
    }

    /// Some core hosts now.
    pub fn is_held(&self) -> bool {
        self.holder.lock().unwrap_or_else(PoisonError::into_inner).is_some()
    }
}
