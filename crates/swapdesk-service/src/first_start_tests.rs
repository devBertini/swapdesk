//! ACCOUNTS.md 15 S5 and 10: the first 1.1 start over the files a 1.0 install left, with the real folder store in a
//! temporary folder (the current user stands for SYSTEM, as in development).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use swapdesk_host::accounts::{AccountFactory, Accounts};
use swapdesk_host::ui_hub::MachineSettings;
use swapdesk_host::ui_hub::store::{AccountsIndex, load_settings, store_settings};
use swapdesk_platform::{Level, SecretStore, TraceSink};
use swapdesk_proto::{Os, PermissionSet, Wire};
use swapdesk_secret_win::{FolderSpec, WinSecretStore};
use swapdesk_sys_win::current_user_sid;
use swapdesk_trust::records::PairedDevice;
use swapdesk_trust::{Identity, TrustHandle};

use super::{AccountData, RootFacts, open_account_data, root_facts};

const A: &str = "S-1-5-21-1-2-3-1001";
const NOW: u64 = 1_760_000_000_000;

struct NoTrace;

impl TraceSink for NoTrace {
    fn log(&self, _level: Level, _target: &str, _args: std::fmt::Arguments<'_>) {}
}

/// A temporary parent folder, removed at the end.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("swapdesk-s5-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open_root(parent: &Path) -> WinSecretStore {
    let me = current_user_sid().unwrap();
    let spec = FolderSpec { owner: me.clone(), full_control: vec![me.clone()], trusted_owners: vec![me] };
    WinSecretStore::open_in(&spec, parent, None).unwrap()
}

/// What a 1.0 install leaves at the root: its key, a pairing with grants both ways, its settings (another port and
/// a name) and an activity log. Returns the 1.0 key's device id.
fn leave_1_0_data(root: &WinSecretStore) -> swapdesk_trust::DeviceId {
    let identity = Identity::load_or_create(root, NOW).unwrap();
    let peer = Identity::generate(NOW).unwrap();
    let trust = TrustHandle::open(Arc::new(open_root(root.dir().parent().unwrap()))).unwrap();
    trust
        .update(|s| {
            let mut d = PairedDevice::new_paired(*peer.spki(), "Mac da sala".into(), Os::MacOs, "15".into(), NOW, None);
            d.grants_here = PermissionSet::CONTROL | PermissionSet::UNATTENDED;
            d.grants_there = Some(PermissionSet::VIEW);
            s.devices.push(d);
        })
        .unwrap();
    let mut settings = MachineSettings::new("Sala");
    settings.port = 51_000;
    settings.pair_port = 51_001;
    store_settings(root, &settings).unwrap();
    fs::write(root.dir().join("audit-sessions.log"), b"1.0 activity").unwrap();
    identity.device_id()
}

/// Every file at the root (not the folders), by name.
fn root_files(root: &WinSecretStore) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root.dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), fs::read(e.path()).unwrap()))
        .collect()
}

fn accounts_over(root: &Arc<WinSecretStore>, base: u16) -> Accounts {
    let factory: AccountFactory = Box::new(|_, _, _| Err("not built in this test".into()));
    Accounts::new(base, root.clone(), factory, Box::new(|| NOW), Arc::new(NoTrace))
}

fn first_start(root: &Arc<WinSecretStore>) -> (RootFacts, Accounts, AccountData) {
    let facts = root_facts(root).unwrap();
    let accounts = accounts_over(root, facts.port);
    let seat = accounts.seat_for(A).unwrap();
    let data = open_account_data(root, &seat, "PC-DA-SALA", NOW, &NoTrace).unwrap();
    (facts, accounts, data)
}

#[test]
fn a_first_1_1_start_on_1_0_files_gives_no_account_any_1_0_data() {
    let t = Temp::new("first");
    let root = Arc::new(open_root(&t.0));
    let old_key = leave_1_0_data(&root);
    let facts = root_facts(&root).unwrap();
    assert_eq!(facts, RootFacts { port: 51_000, had_1_0: true }, "only the port and that 1.0 was here");
    // The index is written at the first start, with no account made from the 1.0 files.
    let _accounts = accounts_over(&root, facts.port);
    let index = AccountsIndex::decode(&root.load("accounts.bin").unwrap().expect("written")).unwrap();
    assert!(index.accounts.is_empty());
    let (_, _, data) = first_start(&root);
    assert_ne!(data.identity.device_id(), old_key, "a new key");
    assert!(data.trust.read(|s| s.devices.is_empty()), "no pairing, no grant");
    assert_eq!(data.settings.name, "PC-DA-SALA", "the 1.0 name is not carried");
    assert!(data.store.dir().starts_with(root.dir().join("accounts")));
    assert!(!data.store.dir().join("audit-sessions.log").exists(), "no 1.0 activity");
}

#[test]
fn the_1_0_root_files_stay_byte_for_byte_and_still_load_with_the_1_0_decoder() {
    let t = Temp::new("bytes");
    let root = Arc::new(open_root(&t.0));
    let old_key = leave_1_0_data(&root);
    let before = root_files(&root);
    let (_, accounts, data) = first_start(&root);
    data.trust
        .update(|s| {
            let peer = Identity::generate(NOW).unwrap();
            s.devices.push(PairedDevice::new_paired(*peer.spki(), "novo".into(), Os::Windows, "11".into(), NOW, None));
        })
        .unwrap();
    drop((accounts, data));
    let mut after = root_files(&root);
    assert!(after.remove("accounts.bin").is_some(), "the only new file at the root");
    assert_eq!(after, before, "every 1.0 file byte for byte");
    // 1.0 reads them as it left them.
    assert_eq!(Identity::load_or_create(&*root, NOW).unwrap().device_id(), old_key);
    let trust = TrustHandle::open(root.clone()).unwrap();
    assert_eq!(trust.read(|s| s.devices.len()), 1);
    assert_eq!(load_settings(&*root, "x").unwrap().name, "Sala");
}

#[test]
fn the_first_account_gets_a_new_key_and_slot_0_on_the_1_0_port() {
    let t = Temp::new("slot");
    let root = Arc::new(open_root(&t.0));
    leave_1_0_data(&root);
    let (_, accounts, data) = first_start(&root);
    let seat = accounts.seat_for(A).unwrap();
    assert_eq!((seat.slot, seat.session_port, seat.pair_port), (0, 51_000, 51_001));
    assert_eq!((data.settings.port, data.settings.pair_port), (51_000, 51_001));
}

#[test]
fn a_second_start_after_a_downgrade_finds_its_accounts_and_still_ignores_the_root_data() {
    let t = Temp::new("again");
    let root = Arc::new(open_root(&t.0));
    leave_1_0_data(&root);
    let (_, accounts, data) = first_start(&root);
    let key = data.identity.device_id();
    drop((accounts, data));
    // Back on 1.0 for a while: it pairs again at the root.
    let trust = TrustHandle::open(root.clone()).unwrap();
    trust
        .update(|s| {
            let peer = Identity::generate(NOW).unwrap();
            s.devices.push(PairedDevice::new_paired(*peer.spki(), "outro".into(), Os::Windows, "11".into(), NOW, None));
        })
        .unwrap();
    drop(trust);
    // 1.1 again: the account as it left it, and still nothing of the root.
    let (_, accounts, data) = first_start(&root);
    assert_eq!(accounts.seat_for(A).unwrap().slot, 0);
    assert_eq!(data.identity.device_id(), key, "its own key");
    assert!(data.trust.read(|s| s.devices.is_empty()), "the root's pairings stay the root's");
}

#[test]
fn a_computer_with_no_1_0_data_shows_no_note() {
    let t = Temp::new("fresh");
    let root = Arc::new(open_root(&t.0));
    assert!(!root_facts(&root).unwrap().had_1_0);
}

/// D55: every account, the first included, is named "<computer> · <account>" until its user changes it.
#[test]
fn every_account_is_named_computer_dot_account() {
    assert_eq!(super::account_name("SALA", Some("ana")), "SALA · ana");
    assert_eq!(super::account_name("SALA", None), "SALA", "an account name that cannot be read");
    assert_eq!(super::account_name("SALA", Some(" ")), "SALA");
    let long = super::account_name(&"x".repeat(62), Some("ana"));
    assert!(long.len() <= 64 && swapdesk_host::ui_hub::store::valid_name(&long), "{long:?}");
    // The real lookup reads this test's own account.
    let me = swapdesk_sys_win::current_user_sid().unwrap();
    assert!(!swapdesk_sys_win::account_name_of_sid(&me).unwrap().is_empty());
}

/// GRANTS-SYNC.md 8 S9: each account's record announces its own slot's port and carries `minor`.
#[test]
fn each_accounts_record_carries_minor() {
    let t = Temp::new("minor");
    let root = Arc::new(open_root(&t.0));
    let accounts = accounts_over(&root, 47_820);
    for (sid, port) in [(A, 47_820u16), ("S-1-5-21-1-2-3-1002", 47_822)] {
        let config = super::account_discovery(&accounts.seat_for(sid).unwrap());
        assert_eq!((config.service_port, config.minor), (port, swapdesk_proto::PROTO_MINOR));
        assert!(config.minor >= 1);
    }
}
