//! The machine layer of `docs/ACCOUNTS.md` (D44, slice S3): one identity, one trust store, one core and one hub per
//! account of the machine, each on a port slot of its own.
//!
//! * **Slots.** Account slot `k` (0 to 31) listens on session port `base + 2k` and pairing port `base + 2k + 1`
//!   (`base` is the machine's `settings.bin` port, 47820). A slot is the account's for as long as its folder exists;
//!   the lowest free one is given to an account the first time it needs one. The index is `accounts.bin` (14.2).
//! * **Building.** What an account is made of (its store, key, trust, settings, audit, discovery, hub and core) is
//!   the caller's, through an [`AccountFactory`]: the Windows service makes them under `%ProgramData%`, tests in
//!   memory. Every account's core gets the same [`MachineShared`] (one hosted session, one set of handshake caps)
//!   and every account's presence hides this machine's other accounts ([`Accounts::own_ids`]).
//! * **Who reaches what.** An interface reaches the hub of its own user's account only (the service routes its pipe
//!   by the client's SID): no op names an account, so none can touch another's pairings, grants or activity.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use swapdesk_platform::{Level, SecretStore, TraceSink};
use swapdesk_proto::Wire;
use swapdesk_trust::DeviceId;

use crate::service::{MachineShared, ServiceCore};
use crate::ui_hub::UiHub;
use crate::ui_hub::store::{ACCOUNTS_NAME, AccountEntry, AccountsIndex, MAX_ACCOUNTS};

/// Where an account sits on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSeat {
    pub sid: String,
    pub slot: u8,
    pub session_port: u16,
    pub pair_port: u16,
}

/// The ports of slot `slot` above `base`: (session, pairing).
pub fn slot_ports(base: u16, slot: u8) -> (u16, u16) {
    let session = base.saturating_add(2 * u16::from(slot));
    (session, session.saturating_add(1))
}

/// One account at work: its key's device ID, its hub and its core.
pub struct Account {
    pub seat: AccountSeat,
    pub device_id: DeviceId,
    pub hub: UiHub,
    pub core: Arc<ServiceCore>,
}

/// Builds an account at its seat, with what the machine shares. `own_ids` is the list of this machine's account
/// device IDs, for the account's presence to hide them.
pub type AccountFactory =
    Box<dyn Fn(&AccountSeat, &MachineShared, Arc<Mutex<Vec<DeviceId>>>) -> Result<Account, String> + Send + Sync>;

/// Every account of the machine.
pub struct Accounts {
    base_port: u16,
    /// The machine's own store: `accounts.bin` lives there.
    store: Arc<dyn SecretStore>,
    index: Mutex<AccountsIndex>,
    live: Mutex<Vec<Arc<Account>>>,
    factory: AccountFactory,
    shared: MachineShared,
    own_ids: Arc<Mutex<Vec<DeviceId>>>,
    wall_ms: Box<dyn Fn() -> u64 + Send + Sync>,
    trace: Arc<dyn TraceSink>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Accounts {
    /// Reads `accounts.bin` from `store` (a damaged index is set aside as `accounts.bin.damaged` and the machine
    /// starts with none, with one trace line).
    pub fn new(
        base_port: u16,
        store: Arc<dyn SecretStore>,
        factory: AccountFactory,
        wall_ms: Box<dyn Fn() -> u64 + Send + Sync>,
        trace: Arc<dyn TraceSink>,
    ) -> Self {
        let index = match store.load(ACCOUNTS_NAME) {
            Ok(Some(bytes)) => AccountsIndex::decode(&bytes).unwrap_or_else(|e| {
                trace.log(Level::Warn, "accounts", format_args!("accounts.bin does not read ({e}): set aside"));
                let _ = store.store("accounts.bin.damaged", &bytes);
                AccountsIndex::default()
            }),
            Ok(None) => AccountsIndex::default(),
            Err(e) => {
                trace.log(Level::Warn, "accounts", format_args!("accounts.bin cannot be read ({e})"));
                AccountsIndex::default()
            }
        };
        Self {
            base_port,
            store,
            index: Mutex::new(index),
            live: Mutex::new(Vec::new()),
            factory,
            shared: MachineShared::new(),
            own_ids: Arc::new(Mutex::new(Vec::new())),
            wall_ms,
            trace,
        }
    }

    /// The device IDs of this machine's accounts at work (what each account's presence hides).
    pub fn own_ids(&self) -> Arc<Mutex<Vec<DeviceId>>> {
        Arc::clone(&self.own_ids)
    }

    /// What every account's core shares.
    pub fn shared(&self) -> &MachineShared {
        &self.shared
    }

    /// The seat of `sid`: the slot it has, or the lowest free one (written to `accounts.bin` first).
    pub fn seat_for(&self, sid: &str) -> Result<AccountSeat, String> {
        let mut index = lock(&self.index);
        let slot = match index.accounts.iter().find(|a| a.sid == sid) {
            Some(a) => a.slot,
            None => {
                let used: Vec<u8> = index.accounts.iter().map(|a| a.slot).collect();
                let Some(slot) = (0..MAX_ACCOUNTS as u8).find(|s| !used.contains(s)) else {
                    return Err("every slot is taken: this account cannot receive connections".into());
                };
                let mut next = index.clone();
                next.accounts.push(AccountEntry { sid: sid.into(), slot, created_ms: (self.wall_ms)() });
                let bytes = next.encode().map_err(|e| e.to_string())?;
                self.store.store(ACCOUNTS_NAME, &bytes).map_err(|e| format!("accounts.bin: {e}"))?;
                *index = next;
                slot
            }
        };
        let (session_port, pair_port) = slot_ports(self.base_port, slot);
        Ok(AccountSeat { sid: sid.into(), slot, session_port, pair_port })
    }

    /// The account of `sid` at work, built at its seat the first time it is asked for.
    pub fn account(&self, sid: &str) -> Result<Arc<Account>, String> {
        if let Some(a) = self.get(sid) {
            return Ok(a);
        }
        let seat = self.seat_for(sid)?;
        let account = Arc::new((self.factory)(&seat, &self.shared, self.own_ids())?);
        let mut live = lock(&self.live);
        // Another thread may have built it meanwhile: the first one stays.
        if let Some(a) = live.iter().find(|a| a.seat.sid == sid) {
            return Ok(Arc::clone(a));
        }
        lock(&self.own_ids).push(account.device_id);
        live.push(Arc::clone(&account));
        self.trace.log(
            Level::Info,
            "accounts",
            format_args!("account in slot {} on ports {}/{}", seat.slot, seat.session_port, seat.pair_port),
        );
        Ok(account)
    }

    /// The account of `sid`, if it is at work.
    pub fn get(&self, sid: &str) -> Option<Arc<Account>> {
        lock(&self.live).iter().find(|a| a.seat.sid == sid).cloned()
    }

    /// Every account at work.
    pub fn all(&self) -> Vec<Arc<Account>> {
        lock(&self.live).clone()
    }

    /// Builds every account of the index, so an account with pairings listens from the start, logged in or not.
    /// Returns the SIDs that could not be built, with why.
    pub fn start_known(&self) -> Vec<(String, String)> {
        let sids: Vec<String> = lock(&self.index).accounts.iter().map(|a| a.sid.clone()).collect();
        sids.into_iter().filter_map(|sid| self.account(&sid).err().map(|e| (sid, e))).collect()
    }
}
