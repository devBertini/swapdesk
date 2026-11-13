//! The proof of 18d-2 for the hub: a fake UI client speaking `swapdesk-ipc` over an in-memory duplex to a [`UiHub`]
//! on a real [`ServiceCore`] (127.0.0.1, the development option). Two machines in one process pair through two hubs,
//! and a session runs end to end with a real `run_helper` loop on the host.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_fakes::{
    FakeClock, FakeNetInterfaces, InputRecord, LogCapture, MemorySecretStore, MonotonicClock, Pacing,
    RecordingInputSink, SineSource, SyntheticScreen,
};
use swapdesk_ipc::{
    ApiCode, Decision, Events, FrameReader, FrameWriter, Handshake, IpcError, IpcRole, Message as Ipc, Notify,
    PairingState, ReplyBody, RequestKind, Role, SessionState, SpecialKeys, UiOp, status_code,
};
use swapdesk_platform::MonoMicros;
use swapdesk_proto::{Desktop, InputEvent, Intent, Os, PermissionSet};
use swapdesk_trust::{
    AuditEntry, AuditEvent, AuditSink, DeviceId, DeviceRef, Identity, MemoryAudit, PairedDevice, SystemWallClock,
    TrustHandle, UserRef, device_id_hex,
};
use swapdesk_ui::MsgKey;

use super::extras::ENTRY_ID_BIT;
use super::presence::{FakePresence, PresencePeer};
use super::store::{MachineSettings, SETTINGS_NAME, load_settings};
use super::{ConnInfo, DevOptions, HubParams, SystemHooks, UiHub};
use crate::helper::{HelperDeps, run_helper};
use crate::helper_tests::Passthrough;
use crate::service::{ServiceConfig, ServiceCore, ServiceDeps};
use crate::testpipe::{PipeWriter, duplex_blocking};

mod accounts;
mod clip_actions;
mod connect;
mod elevation;
mod extras;
mod files;
mod grants_sync;
mod presence;
mod resume;
mod security;
mod terminal;

/// `UiRequest.op` of `Connect`.
const CONNECT_OP: u16 = 11;
const WAIT: Duration = Duration::from_secs(10);
const ALL: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::AUDIO);

#[allow(dead_code)]
fn lo() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the hub asked of the machine.
#[derive(Default)]
struct TestHooks {
    awake: Mutex<Vec<bool>>,
}

impl SystemHooks for TestHooks {
    fn keep_awake(&self, on: bool) {
        lock(&self.awake).push(on);
    }
}

fn user(sid: &str) -> UserRef {
    UserRef { id: sid.into(), name: "alex".into() }
}

fn info(admin: bool) -> ConnInfo {
    // An elevated token belongs to an administrator account; a plain one is a standard account here (the
    // elevation tests build the filtered administrator token themselves).
    ConnInfo { session_id: 1, user: user("S-1-5-21-1"), admin, can_elevate: admin }
}

/// One machine: a core, its hub and the stores a test looks into.
struct Machine {
    core: Arc<ServiceCore>,
    hub: UiHub,
    identity: Arc<Identity>,
    trust: Arc<TrustHandle>,
    secrets: Arc<MemorySecretStore>,
    audit: Arc<MemoryAudit>,
    presence: Arc<FakePresence>,
    hooks: Arc<TestHooks>,
    helpers: Vec<(RecordingInputSink, JoinHandle<()>)>,
    log: LogCapture,
    /// The interface list the hub and the core read (a test changes it to play a network change).
    nets: Arc<FakeNetInterfaces>,
}

impl Machine {
    fn new(name: &str) -> Self {
        Self::with_interfaces(name, FakeNetInterfaces::new(Vec::new()))
    }

    fn with_interfaces(name: &str, interfaces: FakeNetInterfaces) -> Self {
        Self::build(name, interfaces, true)
    }

    /// `dev`: the development option (loopback peers, a pairing endpoint on 127.0.0.1); off is the production shape.
    fn build(name: &str, interfaces: FakeNetInterfaces, dev: bool) -> Self {
        Self::build_at(name, interfaces, dev, None)
    }

    /// `ports`: fixed (session, pairing) ports on 127.0.0.1, for a peer reached through a typed address.
    /// A machine whose session endpoint announces protocol minor `minor` (0 plays a host of 1.0).
    fn with_minor(name: &str, minor: u16) -> Self {
        Self::build_full(name, FakeNetInterfaces::new(Vec::new()), true, None, minor)
    }

    fn build_at(name: &str, interfaces: FakeNetInterfaces, dev: bool, ports: Option<(u16, u16)>) -> Self {
        Self::build_full(name, interfaces, dev, ports, swapdesk_proto::PROTO_MINOR)
    }

    fn build_full(name: &str, interfaces: FakeNetInterfaces, dev: bool, ports: Option<(u16, u16)>, minor: u16) -> Self {
        let identity = Arc::new(Identity::generate(0).expect("identity"));
        let secrets = Arc::new(MemorySecretStore::new());
        let trust = TrustHandle::open(secrets.clone()).expect("trust");
        let audit = Arc::new(MemoryAudit::new());
        let presence = FakePresence::new();
        let hooks = Arc::new(TestHooks::default());
        let trace = LogCapture::new();
        let log = trace.clone();
        let interfaces = Arc::new(interfaces);
        let nets = Arc::clone(&interfaces);
        let clock = Arc::new(MonotonicClock::new());
        let hub = UiHub::new(HubParams {
            os: Os::Windows,
            os_version: "11 24H2".into(),
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            secrets: secrets.clone(),
            settings: MachineSettings::new(name),
            interfaces: interfaces.clone(),
            presence: presence.clone(),
            audit: audit.clone(),
            clock: clock.clone(),
            wall: Arc::new(SystemWallClock),
            trace: Arc::new(trace.clone()),
            hooks: hooks.clone(),
            dev: dev.then(|| DevOptions {
                pair_listen: vec![SocketAddrV4::new(Ipv4Addr::LOCALHOST, ports.map_or(0, |p| p.1))],
            }),
        });
        let deps = ServiceDeps {
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            clock,
            trace: Arc::new(trace),
            ui: hub.host_ui(),
            interfaces,
        };
        let mut cfg = ServiceConfig::new(name, Os::Windows, "11 24H2", deps);
        let session_port = ports.map_or(0, |p| p.0);
        cfg.session_port = session_port;
        cfg.proto_minor = minor;
        let cfg = if dev { cfg.dev_loopback(SocketAddrV4::new(Ipv4Addr::LOCALHOST, session_port)) } else { cfg };
        let core = Arc::new(ServiceCore::start(cfg).expect("core starts"));
        hub.bind_core(&core);
        // The UIs of `info()` run in session 1, the console's: what a connection may do (pair, grant, connect, swap)
        // is the console's alone. A test about another session connects with its own `ConnInfo`.
        hub.set_console_session(Some(1));
        Self { core, hub, identity, trust, secrets, audit, presence, hooks, helpers: Vec::new(), log, nets }
    }

    fn id(&self) -> DeviceId {
        self.identity.device_id()
    }

    fn hex(&self) -> String {
        device_id_hex(&self.id())
    }

    fn session_addr(&self) -> SocketAddr {
        self.core.local_addrs()[0]
    }

    fn client(&self, info: ConnInfo) -> Client {
        Client::connect(&self.hub, info)
    }

    /// The pairing ceremony, done by hand: each knows the other's key; `grants_here` is what this machine lets
    /// `other` do.
    fn pin(&self, other: &Machine, grants_here: PermissionSet) {
        self.trust
            .update(|s| {
                let mut d = PairedDevice::new_paired(
                    *other.identity.spki(),
                    "other-pc".into(),
                    Os::Windows,
                    "11".into(),
                    1,
                    None,
                );
                d.grants_here = grants_here;
                s.devices.push(d);
            })
            .expect("store");
        // A real pairing goes through the hub, which then listens and announces (ACCOUNTS.md 6).
        self.hub.hub.update_receiving();
    }

    /// Shows `other` on the network at its session address, with a record of 1.0 (no `minor` key).
    fn sees(&self, other: &Machine, pairing_open: bool) {
        self.sees_minor(other, pairing_open, 0);
    }

    /// As `sees`, with the protocol minor `other`'s record carries.
    fn sees_minor(&self, other: &Machine, pairing_open: bool, proto_minor: u16) {
        let SocketAddr::V4(addr) = other.session_addr() else { unreachable!() };
        self.presence.set(vec![PresencePeer {
            device_id: other.id(),
            name: "other-pc".into(),
            os: Os::Windows,
            pairing_open,
            addrs: vec![*addr.ip()],
            session_port: addr.port(),
            pair_port: other.hub.pair_local_addrs().first().map_or(0, SocketAddr::port),
            proto_minor,
        }]);
    }

    /// A helper process stand-in: `run_helper` on its own thread behind a pipe pair, attached to the core.
    fn attach_helper(&mut self, role: IpcRole) {
        self.attach_helper_with(role, |deps| deps);
    }

    /// As `attach_helper`, with `tune` given the helper's dependencies first (the terminal proxy's spawner).
    fn attach_helper_with(
        &mut self,
        role: IpcRole,
        tune: impl FnOnce(
            HelperDeps<SyntheticScreen, Passthrough, RecordingInputSink, SineSource>,
        ) -> HelperDeps<SyntheticScreen, Passthrough, RecordingInputSink, SineSource>,
    ) {
        let sink = RecordingInputSink::new();
        let (core_end, helper_end) = duplex_blocking();
        let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
        let audio_clock = Arc::clone(&clock);
        let input = sink.clone();
        let deps = tune(HelperDeps::new(
            |_| Ok(SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime)),
            |_frame, _config, _backend| Ok(Passthrough { n: 0, size: None }),
            move || Ok(input.clone()),
            move || Ok(SineSource::new(audio_clock.clone()).realtime()),
            clock,
        ));
        let thread = std::thread::spawn(move || {
            let _ = run_helper(helper_end.reader, helper_end.writer, deps, role, 4242);
        });
        self.core.attach_helper(role, core_end.reader, core_end.writer).expect("attach");
        self.helpers.push((sink, thread));
    }

    /// The input the user helper (prompted sessions) received.
    fn user_input(&self) -> RecordingInputSink {
        self.helpers.last().expect("a helper").0.clone()
    }

    /// Ends the machine: hub, core, helpers.
    fn finish(self) {
        let Machine { core, hub, helpers, .. } = self;
        hub.shutdown();
        // The hub's tasks hold the core only for an instant; wait for the owner's handle to be the last one.
        let end = Instant::now() + Duration::from_secs(2);
        let mut core = Some(core);
        while let Some(c) = core.take() {
            match Arc::try_unwrap(c) {
                Ok(c) => c.shutdown(),
                Err(c) if Instant::now() < end => {
                    core = Some(c);
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(c) => drop(c),
            }
        }
        for (_, t) in helpers {
            let _ = t.join();
        }
    }
}

/// A fake UI: the client end of a UI pipe, with a reader thread and a backlog of what has not been looked at yet.
struct Client {
    writer: Arc<Mutex<FrameWriter<PipeWriter>>>,
    inbox: Receiver<Ipc>,
    backlog: Vec<Ipc>,
    next_req: u32,
    _serve: JoinHandle<Result<(), IpcError>>,
}

impl Client {
    fn connect(hub: &UiHub, info: ConnInfo) -> Self {
        Self::connect_with_disk(hub, info, None)
    }

    /// A UI that also serves the disk calls of the service (C2.4 "Files") with `disk`, as the real UI does.
    fn connect_with_disk(hub: &UiHub, info: ConnInfo, disk: Option<Arc<swapdesk_files::FsServer>>) -> Self {
        let (client_end, server_end) = duplex_blocking();
        let hub = hub.clone();
        let serve = std::thread::spawn(move || {
            let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
            let hello = Handshake::server(&mut reader, &mut writer, 7)?;
            assert_eq!(hello.role, IpcRole::Ui);
            hub.serve(reader, writer, info)
        });
        let (mut reader, mut writer) = (FrameReader::new(client_end.reader), FrameWriter::new(client_end.writer));
        let ack = Handshake::client(&mut reader, &mut writer, IpcRole::Ui, 1234).expect("handshake");
        assert_eq!(ack.win_session, 7, "HelloAck carries the Windows session");
        let (tx, inbox) = channel();
        let writer = Arc::new(Mutex::new(writer));
        let answering = Arc::clone(&writer);
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = reader.read_message() {
                if let (Some(disk), Ipc::FsRequest { .. }) = (&disk, &msg) {
                    if let Some(reply) = disk.serve(msg) {
                        let _ = lock(&answering).write_message(&reply);
                    }
                    continue;
                }
                // A UI that serves a disk also has "no clipboard" (as a real UI without a provider does): the service's
                // clipboard calls of a viewer session are answered at once, so the session's handlers start without
                // waiting for the 5 s clipboard timeout.
                if disk.is_some() {
                    let req = match &msg {
                        Ipc::ClipChangeCount { req } | Ipc::ClipRead { req } | Ipc::ClipWrite { req, .. } => Some(*req),
                        _ => None,
                    };
                    if let Some(req) = req {
                        let _ = lock(&answering).write_message(&Ipc::ClipReply {
                            req,
                            body: swapdesk_ipc::ClipReplyBody::Failed(swapdesk_ipc::ClipFail::Other),
                        });
                        continue;
                    }
                }
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Self { writer, inbox, backlog: Vec::new(), next_req: 1, _serve: serve }
    }

    fn send(&mut self, msg: &Ipc) {
        lock(&self.writer).write_message(msg).expect("write");
    }

    /// Sends a request and returns its number.
    fn request(&mut self, op: UiOp) -> u32 {
        let req = self.next_req;
        self.next_req += 1;
        self.send(&Ipc::UiRequest { req, op });
        req
    }

    /// The first message `f` picks, from what was set aside or what arrives within `WAIT`; the others stay.
    fn take<T>(&mut self, what: &str, f: impl FnMut(&Ipc) -> Option<T>) -> T {
        self.take_within(WAIT, what, f)
    }

    /// [`Client::take`] with a wait of `limit`.
    fn take_within<T>(&mut self, limit: Duration, what: &str, mut f: impl FnMut(&Ipc) -> Option<T>) -> T {
        if let Some(i) = self.backlog.iter().position(|m| f(m).is_some()) {
            let m = self.backlog.remove(i);
            return f(&m).expect("picked");
        }
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(m) => match f(&m) {
                    Some(v) => return v,
                    None => self.backlog.push(m),
                },
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
                Err(RecvTimeoutError::Disconnected) => panic!("the pipe closed while waiting for {what}"),
            }
        }
    }

    /// The reply to `req`, decoded with `op`.
    fn reply(&mut self, req: u32, op: u16) -> Result<ReplyBody, ApiCode> {
        let (status, body) = self.take("a UiReply", |m| match m {
            Ipc::UiReply { req: r, status, body } if *r == req => Some((*status, body.0.clone())),
            _ => None,
        });
        match status_code(status).expect("a known status") {
            Some(code) => Err(code),
            None => Ok(ReplyBody::decode(op, &body).expect("the body decodes")),
        }
    }

    /// The reply to a `Connect` (the id of its entry, which comes at once).
    fn entry_of(&mut self, req: u32) -> u64 {
        let ReplyBody::SessionId(entry) = self.reply(req, CONNECT_OP).expect("the connect is taken up") else {
            panic!("not an entry id")
        };
        assert_ne!(entry & ENTRY_ID_BIT, 0, "an entry id has the entry bit: {entry:#x}");
        entry
    }

    /// The session a `Connect` opened: the id of the first `Active` this UI hears of, after the entry's reply. The
    /// notification stays in the backlog for a test that looks at it too.
    fn connected(&mut self, req: u32) -> u64 {
        let entry = self.entry_of(req);
        let (id, msg) = self.take("the session's Active", |m| match m {
            Ipc::UiNotify(Notify::Session { session_id, state: SessionState::Active { .. }, .. }) => {
                Some((*session_id, m.clone()))
            }
            _ => None,
        });
        assert_ne!(id, entry, "the session has its own id, not the entry's");
        self.backlog.insert(0, msg);
        id
    }

    fn call(&mut self, op: UiOp) -> Result<ReplyBody, ApiCode> {
        let code = op.code();
        let req = self.request(op);
        self.reply(req, code)
    }

    fn notify<T>(&mut self, what: &str, mut f: impl FnMut(&Notify) -> Option<T>) -> T {
        self.take(what, |m| match m {
            Ipc::UiNotify(n) => f(n),
            _ => None,
        })
    }

    fn pairing(&mut self, pairing_id: u32, what: &str, mut f: impl FnMut(&PairingState) -> bool) -> PairingState {
        self.notify(what, |n| match n {
            Notify::Pairing { pairing_id: id, state } if *id == pairing_id && f(state) => Some(state.clone()),
            _ => None,
        })
    }

    /// Forgets what has arrived and was not looked at (video included): a test that waits for what comes next.
    fn clear(&mut self) {
        self.backlog.clear();
        while self.inbox.try_recv().is_ok() {}
    }

    /// True when nothing matching `f` has arrived within `window`.
    fn nothing_within(&mut self, window: Duration, mut f: impl FnMut(&Ipc) -> bool) -> bool {
        let end = Instant::now() + window;
        loop {
            if self.backlog.iter().any(&mut f) {
                return false;
            }
            let left = end.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(m) => self.backlog.push(m),
                Err(_) => return true,
            }
        }
    }
}

fn device_of(r: Result<ReplyBody, ApiCode>) -> swapdesk_ipc::DeviceView {
    match r.expect("a device") {
        ReplyBody::Device(d) => d,
        other => panic!("not a device: {other:?}"),
    }
}

// ---------- the device list, this device, the network, the settings ----------

#[test]
fn this_device_network_devices_and_the_manual_address() {
    let m = Machine::with_interfaces("sala", FakeNetInterfaces::windows_typical());
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let mut ui = m.client(info(false));

    let ReplyBody::ThisDevice(this) = ui.call(UiOp::GetThisDevice).unwrap() else { panic!() };
    assert_eq!((this.name.as_str(), this.os, this.ready), ("sala", Os::Windows, swapdesk_ipc::Ready::ReadyToReceive));
    let ReplyBody::Network(net) = ui.call(UiOp::GetNetwork).unwrap() else { panic!() };
    assert_eq!(net.label, "Ethernet", "the first approved interface");
    assert!(net.address.is_some());

    // A paired device that is not on the network, then on it, then an unpaired one with its pairing open.
    let ReplyBody::Devices(list) = ui.call(UiOp::ListDevices).unwrap() else { panic!() };
    assert_eq!(list.len(), 1);
    assert!(list[0].paired && list[0].grants_here == PermissionSet::CONTROL.bits());
    assert_eq!(list[0].status, swapdesk_ipc::DeviceStatus::Offline);
    assert_eq!(list[0].id, other.hex());

    let stranger_id = Identity::generate(0).expect("identity").device_id();
    m.presence.set(vec![
        PresencePeer {
            device_id: other.id(),
            name: "other-pc".into(),
            os: Os::Windows,
            pairing_open: false,
            addrs: vec![Ipv4Addr::new(192, 168, 0, 20)],
            session_port: 47820,
            pair_port: 47821,
            proto_minor: 0,
        },
        PresencePeer {
            device_id: stranger_id,
            name: "stranger".into(),
            os: Os::MacOs,
            pairing_open: true,
            addrs: vec![Ipv4Addr::new(192, 168, 0, 21)],
            session_port: 47820,
            pair_port: 47821,
            proto_minor: 0,
        },
    ]);
    // The presence listener pushed the new list.
    let pushed = ui.notify("DevicesChanged", |n| match n {
        Notify::DevicesChanged(l) if l.len() == 2 => Some(l.clone()),
        _ => None,
    });
    assert_eq!(pushed[0].status, swapdesk_ipc::DeviceStatus::Online { address: "192.168.0.20".into(), rtt_ms: 0 });
    assert_eq!(pushed[1].status, swapdesk_ipc::DeviceStatus::OnNetworkUnpaired { pairing_open: true });
    assert!(!pushed[1].paired);

    // F-FIND-03: off-link and own-address typed addresses are refused, a loopback instance is named with its port
    // (development mode), and what is not an address is invalid.
    assert_eq!(ui.call(UiOp::AddByAddress { ipv4: "8.8.8.8".into() }).unwrap_err(), ApiCode::Refused);
    assert_eq!(ui.call(UiOp::AddByAddress { ipv4: "banana".into() }).unwrap_err(), ApiCode::Invalid);
    assert_eq!(
        ui.call(UiOp::AddByAddress { ipv4: format!("127.0.0.1:{}", m.session_addr().port()) }).unwrap_err(),
        ApiCode::Refused,
        "this machine's own endpoint"
    );
    let typed = device_of(ui.call(UiOp::AddByAddress { ipv4: "127.0.0.1:47830".into() }));
    assert_eq!((typed.paired, typed.name.as_str()), (false, "127.0.0.1"));
    assert_eq!(typed.status, swapdesk_ipc::DeviceStatus::OnNetworkUnpaired { pairing_open: false });
    ui.notify("DevicesChanged with the typed address", |n| {
        matches!(n, Notify::DevicesChanged(l) if l.iter().any(|d| d.id == typed.id)).then_some(())
    });

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn the_production_shape_refuses_loopback_ports_and_off_link_addresses() {
    let m = Machine::build("sala", FakeNetInterfaces::windows_typical(), false);
    let mut ui = m.client(info(false));
    let add = |ui: &mut Client, text: &str| ui.call(UiOp::AddByAddress { ipv4: text.into() });
    assert_eq!(add(&mut ui, "127.0.0.1").unwrap_err(), ApiCode::Refused, "loopback");
    assert_eq!(add(&mut ui, "127.0.0.1:47830").unwrap_err(), ApiCode::Refused, "loopback, with a port or without");
    assert_eq!(add(&mut ui, "192.168.1.50:banana").unwrap_err(), ApiCode::Invalid, "a port is a number");
    assert_eq!(add(&mut ui, "192.168.1.50:70000").unwrap_err(), ApiCode::Invalid, "a port fits 16 bits");
    assert_eq!(add(&mut ui, "192.168.1.20").unwrap_err(), ApiCode::Refused, "this machine's own address");
    assert_eq!(add(&mut ui, "10.9.9.9").unwrap_err(), ApiCode::Refused, "off-link");
    assert_eq!(add(&mut ui, "192.168.1.50 ").map(|_| ()).err(), None, "on-link, surrounding space ignored");
    let ReplyBody::Devices(list) = ui.call(UiOp::ListDevices).unwrap() else { panic!() };
    assert_eq!(list.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["addr:192.168.1.50"]);
    // A Swapdesk on another session port (an account's own port, a development instance) is named with it.
    assert_eq!(device_of(add(&mut ui, "192.168.1.50:47830")).id, "addr:192.168.1.50:47830");
    assert_eq!(add(&mut ui, "10.9.9.9:47830").unwrap_err(), ApiCode::Refused, "off-link, whatever the port");
    drop(ui);
    m.finish();
}

#[test]
fn settings_are_validated_persisted_and_pushed() {
    let m = Machine::new("sala");
    let mut ui = m.client(info(false));
    let ReplyBody::Settings(s) = ui.call(UiOp::GetSettings).unwrap() else { panic!() };
    assert_eq!((s.name.as_str(), s.service_enabled, s.lock_on_end, s.keep_awake), ("sala", true, false, true));

    let update = swapdesk_ipc::SettingsUpdate {
        name: "quarto".into(),
        service_enabled: false,
        lock_on_end: true,
        keep_awake: false,
    };
    assert!(matches!(ui.call(UiOp::SetSettings(update)).unwrap(), ReplyBody::Done));
    let pushed = ui.notify("SettingsChanged", |n| match n {
        Notify::SettingsChanged(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(
        (pushed.name.as_str(), pushed.service_enabled, pushed.lock_on_end, pushed.keep_awake),
        ("quarto", false, true, false)
    );
    let on_disk = load_settings(&*m.secrets, "x").expect("settings.bin");
    assert_eq!(
        on_disk,
        MachineSettings {
            name: "quarto".into(),
            service_enabled: false,
            lock_on_end: true,
            keep_awake: false,
            ..MachineSettings::new("x")
        }
    );
    assert!(swapdesk_platform::SecretStore::load(&*m.secrets, SETTINGS_NAME).unwrap().is_some());

    let blank =
        swapdesk_ipc::SettingsUpdate { name: "  ".into(), service_enabled: true, lock_on_end: false, keep_awake: true };
    assert_eq!(ui.call(UiOp::SetSettings(blank)).unwrap_err(), ApiCode::Invalid);
    assert_eq!(m.hub.settings().name, "quarto", "a refused change leaves the settings");

    drop(ui);
    m.finish();
}

// ---------- grants and the administrator checks ----------

#[test]
fn grants_are_validated_and_unattended_needs_an_administrator_connection() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let id = other.hex();
    let mut plain = m.client(info(false));
    let mut admin = m.client(info(true));

    // PROTOCOL.md 6.1: reserved bits and a broken implication are invalid, before anything else is looked at.
    assert_eq!(plain.call(UiOp::SetGrants { device_id: id.clone(), grants: 0x8000 }).unwrap_err(), ApiCode::Invalid);
    assert_eq!(
        plain.call(UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::BIT_CONTROL }).unwrap_err(),
        ApiCode::Invalid,
        "CONTROL without VIEW"
    );
    assert_eq!(plain.call(UiOp::SetGrants { device_id: "nope".into(), grants: 1 }).unwrap_err(), ApiCode::Invalid);
    assert_eq!(
        plain.call(UiOp::SetGrants { device_id: device_id_hex(&[9; 16]), grants: 1 }).unwrap_err(),
        ApiCode::NotFound
    );

    // C2.3, D7: granting UNATTENDED or ADMIN_TERMINAL from a connection that is not elevated is NeedsAdmin.
    assert_eq!(
        plain.call(UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::UNATTENDED.bits() }).unwrap_err(),
        ApiCode::NeedsAdmin
    );
    assert_eq!(
        plain
            .call(UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::ADMIN_TERMINAL.bits() })
            .unwrap_err(),
        ApiCode::NeedsAdmin
    );
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here), PermissionSet::CONTROL, "nothing changed");
    // Entry without a prompt is no longer an administrator's alone (D10's check on `Connect` went with ACCOUNTS.md
    // 8.1): any account's connect is taken, the flag is not read, and the host decides.
    assert!(plain.call(UiOp::Connect { device_id: id.clone(), intent: Intent::Control, unattended: true }).is_ok());

    // What needs no administrator goes through.
    let view_files = (PermissionSet::VIEW.union(PermissionSet::FILES)).bits();
    assert!(plain.call(UiOp::SetGrants { device_id: id.clone(), grants: view_files }).is_ok());
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here.bits()), view_files);

    // The administrator connection may add UNATTENDED; the grantor is recorded (D13) and the list is pushed.
    let both = PermissionSet::UNATTENDED.union(PermissionSet::FILES).bits();
    assert!(admin.call(UiOp::SetGrants { device_id: id.clone(), grants: both }).is_ok());
    let (grants, by) = m.trust.read(|s| (s.devices[0].grants_here.bits(), s.devices[0].unattended_granted_by.clone()));
    assert_eq!((grants, by), (both, Some(user("S-1-5-21-1"))));
    let pushed = plain.notify("DevicesChanged", |n| match n {
        Notify::DevicesChanged(l) if l[0].grants_here == both => Some(l.clone()),
        _ => None,
    });
    assert_eq!(pushed[0].grants_here, both);
    // Taking it away narrows: any connection may, and the grantor goes.
    assert!(plain.call(UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::FILES.bits() }).is_ok());
    assert_eq!(m.trust.read(|s| s.devices[0].unattended_granted_by.clone()), None);
    assert!(m.audit.entries().iter().filter(|e| matches!(e.event, AuditEvent::GrantsChanged { .. })).count() >= 3);

    // Rename: only the local name of the record changes, the list is pushed, and bad names are refused.
    let rename = |device_id: &str, name: &str| UiOp::RenameDevice { device_id: device_id.into(), name: name.into() };
    assert!(plain.call(rename(&id, "  Sala de estar  ")).is_ok());
    assert_eq!(m.trust.read(|s| s.devices[0].name.clone()), "Sala de estar", "trimmed and stored");
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here.bits()), PermissionSet::FILES.bits(), "grants untouched");
    let pushed = plain.notify("the renamed list", |n| match n {
        Notify::DevicesChanged(l) if l[0].name == "Sala de estar" => Some(l.clone()),
        _ => None,
    });
    assert_eq!(pushed[0].name, "Sala de estar");
    // (A name over 64 bytes never gets here: the wire codec refuses it, see swapdesk-ipc `tests/rename.rs`.)
    for bad in ["", "   ", "a\nb", "tab\there"] {
        assert_eq!(plain.call(rename(&id, bad)).unwrap_err(), ApiCode::Invalid, "{bad:?}");
    }
    assert!(plain.call(rename(&id, &"x".repeat(64))).is_ok());
    assert_eq!(m.trust.read(|s| s.devices[0].name.len()), 64);
    assert_eq!(plain.call(rename("zz", "Nome")).unwrap_err(), ApiCode::Invalid);
    assert_eq!(plain.call(rename(&"0".repeat(32), "Nome")).unwrap_err(), ApiCode::NotFound);

    // Forget: the key and grants go.
    assert!(plain.call(UiOp::ForgetDevice { device_id: id.clone() }).is_ok());
    assert!(m.trust.read(|s| s.devices.is_empty()));
    assert_eq!(plain.call(UiOp::ForgetDevice { device_id: id }).unwrap_err(), ApiCode::NotFound);

    drop((plain, admin));
    m.finish();
    other.finish();
}

#[test]
fn activity_names_a_removed_device_by_no_name_never_by_its_key() {
    // Defect after 1.0: a forgotten device's lines showed its key in hex. The service now sends an empty name and the
    // interface writes "Dispositivo removido" in its own language.
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let mut plain = m.client(info(false));
    for event in [AuditEvent::Paired, AuditEvent::Swapped] {
        m.audit.append(AuditEntry { time_ms: 1_000, device: DeviceRef::Known(other.id()), event, duration_s: None });
    }
    // A control session from the other machine: one line, with the duration its end recorded.
    let started = AuditEvent::SessionStarted {
        direction: swapdesk_trust::Direction::Incoming,
        mode: swapdesk_proto::GrantMode::Prompted,
        granted: PermissionSet::VIEW.union(PermissionSet::CONTROL),
        desktop: Desktop::Default,
    };
    let device = DeviceRef::Known(other.id());
    m.audit.append(AuditEntry { time_ms: 2_000, device, event: started, duration_s: None });
    let ended = AuditEvent::SessionEnded(swapdesk_proto::ErrorCode::NoError);
    m.audit.append(AuditEntry { time_ms: 122_000, device, event: ended, duration_s: Some(120) });
    let lines = |c: &mut Client| {
        let ReplyBody::Activity(l) = c.call(UiOp::ActivityList { before_ms: None, limit: 10 }).unwrap() else {
            panic!()
        };
        l
    };
    let before = lines(&mut plain);
    assert_eq!(before.len(), 3, "{before:?}");
    assert_eq!(MsgKey::ALL[usize::from(before[0].what.key)], MsgKey::AuditControlled);
    assert_eq!(
        (before[0].at_ms, before[0].duration_s),
        (2_000, Some(120)),
        "the line sits at the start, with the end's duration"
    );
    assert!(before.iter().all(|l| l.device_name == "other-pc"), "{before:?}");

    assert!(plain.call(UiOp::ForgetDevice { device_id: other.hex() }).is_ok());
    let after = lines(&mut plain);
    assert_eq!(after.len(), 3);
    for l in &after {
        assert_eq!(l.device_name, "", "a removed device has no name on the wire: {l:?}");
        assert!(l.what.args.iter().all(|a| a.0.is_empty()), "nor in its line: {l:?}");
    }

    drop(plain);
    m.finish();
    other.finish();
}

// ---------- pairing ----------

#[test]
fn two_machines_pair_through_their_hubs_and_both_trust_stores_hold_the_other() {
    let a = Machine::new("alfa");
    let b = Machine::new("beta");
    let mut ua = a.client(info(false));
    let mut ub = b.client(info(false));

    // Both users open "Parear" (D12): the mode, the endpoints and the screens' ids.
    let ReplyBody::PairingId(id_b) = ub.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    let ReplyBody::PairingId(id_a) = ua.call(UiOp::PairOpen { device_id: Some(b.hex()) }).unwrap() else { panic!() };
    assert!(
        matches!(ub.pairing(id_b, "Open", |s| matches!(s, PairingState::Open { .. })), PairingState::Open { remaining_s } if remaining_s > 100)
    );
    assert!(
        matches!(ua.pairing(id_a, "Open", |s| matches!(s, PairingState::Open { .. })), PairingState::Open { remaining_s } if remaining_s > 100)
    );
    assert!(a.presence.pairing_announced() && b.presence.pairing_announced(), "pair=1 in both announcements");
    assert!(!b.hub.pair_local_addrs().is_empty(), "the responder's pairing endpoint is open");

    // A can see B (its record says pair=1) and starts the ceremony; a wrong id or a closed mode are refused.
    a.sees(&b, true);
    assert_eq!(ua.call(UiOp::PairStart { pairing_id: id_a + 100, device_id: b.hex() }).unwrap_err(), ApiCode::Invalid);
    assert_eq!(
        ua.call(UiOp::PairStart { pairing_id: id_a, device_id: device_id_hex(&[3; 16]) }).unwrap_err(),
        ApiCode::NotFound
    );
    assert!(matches!(ua.call(UiOp::PairStart { pairing_id: id_a, device_id: b.hex() }).unwrap(), ReplyBody::Done));

    // The same six digits on both screens.
    let code = |ui: &mut Client, id| match ui.pairing(id, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. })) {
        PairingState::ShowCode { code, peer_name } => (code.0, peer_name),
        _ => unreachable!(),
    };
    let (code_a, peer_a) = code(&mut ua, id_a);
    let (code_b, peer_b) = code(&mut ub, id_b);
    assert_eq!(code_a, code_b);
    assert_eq!(code_a.len(), 7, "two groups of three digits");
    assert_eq!((peer_a.as_str(), peer_b.as_str()), ("beta", "alfa"));

    // Confirm on both: a verdict for another pairing is not found; then both are pinned.
    assert_eq!(ua.call(UiOp::PairConfirm { pairing_id: id_a + 100, matches: true }).unwrap_err(), ApiCode::NotFound);
    assert!(ua.call(UiOp::PairConfirm { pairing_id: id_a, matches: true }).is_ok());
    assert!(ub.call(UiOp::PairConfirm { pairing_id: id_b, matches: true }).is_ok());
    let done_a = ua.pairing(id_a, "Done", |s| matches!(s, PairingState::Done { .. }));
    let done_b = ub.pairing(id_b, "Done", |s| matches!(s, PairingState::Done { .. }));
    assert_eq!(done_a, PairingState::Done { device_id: b.hex() });
    assert_eq!(done_b, PairingState::Done { device_id: a.hex() });
    assert!(a.trust.read(|s| s.find_by_id(&b.id()).is_some()), "A holds B");
    assert!(b.trust.read(|s| s.find_by_id(&a.id()).is_some()), "B holds A");
    assert!(a.trust.read(|s| s.devices[0].grants_here.is_empty()), "no grants right after pairing");
    wait_for("pair=1 off again", || (!a.presence.pairing_announced() && !b.presence.pairing_announced()).then_some(()));

    // The list is pushed, the audit has the pairing, and Atividade shows it.
    ua.notify("DevicesChanged with the new device", |n| {
        matches!(n, Notify::DevicesChanged(l) if l.iter().any(|d| d.paired && d.id == b.hex())).then_some(())
    });
    assert!(a.audit.entries().iter().any(|e| e.event == AuditEvent::Paired));
    let ReplyBody::Activity(lines) = ua.call(UiOp::ActivityList { before_ms: None, limit: 10 }).unwrap() else {
        panic!()
    };
    assert_eq!(lines.len(), 1);
    assert_eq!(MsgKey::ALL[usize::from(lines[0].what.key)], MsgKey::AuditPaired);
    assert_eq!(lines[0].device_name, "beta");
    assert!(!lines[0].alert);

    // The screen is over: a late verdict has nothing to answer, and cancelling it is not found.
    assert_eq!(ua.call(UiOp::PairConfirm { pairing_id: id_a, matches: true }).unwrap_err(), ApiCode::NotFound);
    assert_eq!(ua.call(UiOp::PairCancel { pairing_id: id_a }).unwrap_err(), ApiCode::NotFound);

    drop((ua, ub));
    a.finish();
    b.finish();
}

#[test]
fn a_cancelled_pairing_closes_the_mode_and_a_mismatch_pins_nothing() {
    let a = Machine::new("alfa");
    let b = Machine::new("beta");
    let mut ua = a.client(info(false));
    let mut ub = b.client(info(false));

    // Open, then cancel: the mode and the announcement go, quietly.
    let ReplyBody::PairingId(id) = ua.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    assert!(a.hub.hub.mode.is_open());
    assert!(ua.call(UiOp::PairCancel { pairing_id: id }).is_ok());
    assert!(!a.hub.hub.mode.is_open() && !a.presence.pairing_announced());
    assert_eq!(
        ua.call(UiOp::PairStart { pairing_id: id, device_id: b.hex() }).unwrap_err(),
        ApiCode::Invalid,
        "no pairing is open"
    );

    // Open both again; the users see different codes and one presses "São diferentes".
    let ReplyBody::PairingId(id_a) = ua.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    let ReplyBody::PairingId(id_b) = ub.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    a.sees(&b, true);
    assert!(ua.call(UiOp::PairStart { pairing_id: id_a, device_id: b.hex() }).is_ok());
    ua.pairing(id_a, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. }));
    ub.pairing(id_b, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. }));
    assert!(ub.call(UiOp::PairConfirm { pairing_id: id_b, matches: false }).is_ok());
    let failed_b = ub.pairing(id_b, "Failed", |s| matches!(s, PairingState::Failed { .. }));
    assert!(matches!(failed_b, PairingState::Failed { reason: ApiCode::Refused }));
    // A's user has not answered yet: its screen learns of the refusal once it does.
    assert!(ua.call(UiOp::PairConfirm { pairing_id: id_a, matches: true }).is_ok());
    ua.pairing(id_a, "Failed", |s| matches!(s, PairingState::Failed { .. }));
    assert!(a.trust.read(|s| s.devices.is_empty()) && b.trust.read(|s| s.devices.is_empty()), "nothing pinned");
    assert!(b.trust.read(|s| s.lockout.failures.len() == 1), "a counted failed attempt");

    drop((ua, ub));
    a.finish();
    b.finish();
}

// ---------- a session end to end ----------

fn grants_active(state: &SessionState) -> Option<(Role, u16)> {
    match state {
        SessionState::Active { role, granted, .. } => Some((*role, *granted)),
        _ => None,
    }
}

#[test]
fn connect_prompts_the_hosts_ui_then_video_flows_and_disconnect_ends_both_sides() {
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, ALL);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    viewer.sees(&host, false);
    host.hub.set_console_session(Some(1));

    let mut host_ui = host.client(info(false));
    let mut viewer_ui = viewer.client(info(false));

    // "Connect" waits for the host's user: the request reaches the host's UI, which answers.
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let incoming = host_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    assert_eq!(incoming.kind, RequestKind::Session { intent: Intent::Control });
    assert_eq!(incoming.device.id, viewer.hex());
    assert_eq!(incoming.preset, ALL.bits(), "the stored grants are the preset");
    assert!(incoming.timeout_s > 0 && incoming.timeout_s <= 20);
    // A reserved bit is invalid whatever the decision; a UI that is not the console's may not answer.
    let mut stranger =
        host.client(ConnInfo { session_id: 9, user: user("S-1-5-21-2"), admin: false, can_elevate: false });
    assert_eq!(
        stranger
            .call(UiOp::AnswerIncoming {
                request_id: incoming.request_id,
                decision: Decision::Accept,
                grants: ALL.bits()
            })
            .unwrap_err(),
        ApiCode::NotPermitted
    );
    assert_eq!(
        host_ui
            .call(UiOp::AnswerIncoming { request_id: incoming.request_id, decision: Decision::Accept, grants: 0x4000 })
            .unwrap_err(),
        ApiCode::Invalid
    );
    assert_eq!(
        host_ui.call(UiOp::AnswerIncoming { request_id: 999, decision: Decision::Decline, grants: 0 }).unwrap_err(),
        ApiCode::NotFound
    );
    assert!(
        host_ui
            .call(UiOp::AnswerIncoming {
                request_id: incoming.request_id,
                decision: Decision::Accept,
                grants: ALL.bits()
            })
            .is_ok()
    );

    let session_id = viewer_ui.connected(req);
    assert_ne!(session_id, 0);

    // Both UIs see it active: the viewer as Viewer, the host's banner as Host.
    let (role, granted) = viewer_ui.notify("Active on the viewer", |n| match n {
        Notify::Session { session_id: id, state, .. } if *id == session_id => grants_active(state),
        _ => None,
    });
    assert_eq!((role, granted), (Role::Viewer, ALL.bits()));
    let (role, granted) = host_ui.notify("Active on the host", |n| match n {
        Notify::Session { session_id: id, device_id, state } if *id == session_id && *device_id == viewer.hex() => {
            grants_active(state)
        }
        _ => None,
    });
    assert_eq!((role, granted), (Role::Host, ALL.bits()));
    wait_for("keep awake on the host", || (lock(&host.hooks.awake).last() == Some(&true)).then_some(()));

    // Video reaches the viewer's UI as ViewerVideo with the session id; the display list as a notification.
    let header = viewer_ui.take("a ViewerVideo", |m| match m {
        Ipc::ViewerVideo { session_id: id, header, .. } if *id == session_id => Some(*header),
        _ => None,
    });
    assert!(header.keyframe, "the first frame is a keyframe");
    assert_eq!((header.width, header.height), (64, 48));
    let displays = viewer_ui.notify("Displays", |n| match n {
        Notify::Displays { session_id: id, list } if *id == session_id => Some(list.clone()),
        _ => None,
    });
    assert_eq!((displays.len(), displays[0].width, displays[0].height), (1, 64, 48));
    // `ListDisplays` answers with the list the notification carried; another connection or session has none.
    assert_eq!(viewer_ui.call(UiOp::ListDisplays { session_id }).unwrap(), ReplyBody::Displays(displays.clone()));
    assert_eq!(viewer_ui.call(UiOp::ListDisplays { session_id: session_id + 1 }).unwrap_err(), ApiCode::NotFound);
    viewer_ui.notify("Stats", |n| {
        matches!(n, Notify::Stats { session_id: id, fps, .. } if *id == session_id && *fps > 0).then_some(())
    });

    // The UI's input goes to the host's user helper (a prompted session never reaches the SYSTEM injector).
    let pointer = InputEvent::Pointer { display: 0, x: 100, y: 200 };
    viewer_ui.send(&Ipc::ViewerInput { session_id, events: Events(vec![pointer.clone()]) });
    // Special keys: Alt+Tab is input, Ctrl+Alt+Del needs an UNATTENDED session.
    assert!(matches!(
        viewer_ui.call(UiOp::SendKeys { session_id, keys: SpecialKeys::AltTab }).unwrap(),
        ReplyBody::Done
    ));
    assert_eq!(
        viewer_ui.call(UiOp::SendKeys { session_id, keys: SpecialKeys::CtrlAltDel }).unwrap_err(),
        ApiCode::NotPermitted
    );
    let alt_tab = super::ops::key_events(SpecialKeys::AltTab, false);
    let sink = host.user_input();
    let want_len = 1 + alt_tab.len();
    wait_for("the input at the user helper", || (sink.events().len() >= want_len).then_some(()));
    let mut want = vec![pointer];
    want.extend(alt_tab);
    assert_eq!(sink.events(), want);
    assert!(sink.records().iter().all(|r| matches!(
        r.what,
        InputRecord::Event(_)
            | InputRecord::ReleaseAll
            | InputRecord::DisplaysChanged(_)
            | InputRecord::FollowInputDesktop
    )));

    // View-only on the UI stops forwarding input; the other controls answer.
    assert!(viewer_ui.call(UiOp::SetViewOnly { session_id, on: true }).is_ok());
    assert_eq!(
        viewer_ui.call(UiOp::SendKeys { session_id, keys: SpecialKeys::AltTab }).unwrap_err(),
        ApiCode::NotPermitted
    );
    assert!(viewer_ui.call(UiOp::SetViewOnly { session_id, on: false }).is_ok());
    assert!(viewer_ui.call(UiOp::SelectDisplay { session_id, display: 0 }).is_ok());
    assert!(viewer_ui.call(UiOp::SetQuality { session_id, quality: swapdesk_proto::Preset::Text }).is_ok());
    // F-VIEW-06: the viewer's area is accepted for the session and for no other.
    assert!(
        viewer_ui.call(UiOp::SetViewSize { session_id, width: 32, height: 24, max_fps: 60 }).is_ok(),
        "SetViewSize answers Done"
    );
    assert_eq!(
        viewer_ui
            .call(UiOp::SetViewSize { session_id: session_id + 1, width: 32, height: 24, max_fps: 60 })
            .unwrap_err(),
        ApiCode::NotFound
    );
    assert!(viewer_ui.call(UiOp::SetAudio { session_id, on: true }).is_ok());
    assert_eq!(
        viewer_ui.call(UiOp::SelectDisplay { session_id: session_id + 1, display: 0 }).unwrap_err(),
        ApiCode::NotFound
    );
    // A session id is not a capability on another connection.
    let mut other_ui = viewer.client(info(false));
    assert_eq!(other_ui.call(UiOp::Disconnect { session_id }).unwrap_err(), ApiCode::NotFound);
    assert_eq!(other_ui.call(UiOp::ListDisplays { session_id }).unwrap_err(), ApiCode::NotFound);

    // Disconnect: both sides hear a normal end (no reason), and the audit and "keep awake" follow.
    assert!(viewer_ui.call(UiOp::Disconnect { session_id }).is_ok());
    let ended = |n: &Notify| match n {
        Notify::Session { session_id: id, state: SessionState::Ended { reason }, .. } if *id == session_id => {
            Some(*reason)
        }
        _ => None,
    };
    assert_eq!(viewer_ui.notify("Ended on the viewer", ended), None);
    assert_eq!(host_ui.notify("Ended on the host", ended), None);
    wait_for("keep awake off", || (lock(&host.hooks.awake).last() == Some(&false)).then_some(()));
    wait_for("the end in the host's audit", || {
        host.audit.entries().iter().any(|e| matches!(e.event, AuditEvent::SessionEnded(_))).then_some(())
    });
    let started: Vec<_> =
        host.audit.entries().into_iter().filter(|e| matches!(e.event, AuditEvent::SessionStarted { .. })).collect();
    assert_eq!(started.len(), 1);
    wait_for("the viewer's audit", || {
        viewer.audit.entries().iter().any(|e| matches!(e.event, AuditEvent::SessionEnded(_))).then_some(())
    });
    // The host's device list now carries the last session.
    let ReplyBody::Devices(list) = host_ui.call(UiOp::ListDevices).unwrap() else { panic!() };
    assert!(
        list[0]
            .last_session
            .as_ref()
            .is_some_and(|s| s.direction == swapdesk_ipc::Direction::Incoming && s.desktop == Desktop::Default)
    );

    drop((host_ui, viewer_ui, stranger, other_ui));
    viewer.finish();
    host.finish();
}

#[test]
fn with_no_ui_at_the_host_a_request_is_declined_at_once() {
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, ALL);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperUser);
    viewer.sees(&host, false);
    host.hub.set_console_session(Some(1));
    let mut viewer_ui = viewer.client(info(false));

    let started = Instant::now();
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::View, unattended: false });
    let entry = viewer_ui.entry_of(req);
    let why = viewer_ui.notify("the end of the entry", |n| match n {
        Notify::Session { session_id, state: SessionState::NotOpened { why }, .. } if *session_id == entry => {
            Some(*why)
        }
        _ => None,
    });
    assert_eq!(
        why,
        swapdesk_ipc::ConnectWhy::NobodyPresent,
        "viewer {:?} host {:?}",
        viewer.log.lines(),
        host.log.lines()
    );
    assert!(started.elapsed() < Duration::from_secs(5), "declined without waiting for the 20 s");
    assert!(
        viewer_ui.nothing_within(Duration::from_millis(100), |m| matches!(
            m,
            Ipc::UiNotify(Notify::Session { state: SessionState::Active { .. }, .. })
        )),
        "no session was announced"
    );

    drop(viewer_ui);
    viewer.finish();
    host.finish();
}

#[test]
fn a_message_that_is_not_for_a_ui_ends_the_connection() {
    let m = Machine::new("sala");
    let (client_end, server_end) = duplex_blocking();
    let hub = m.hub.clone();
    let serve = std::thread::spawn(move || {
        let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
        Handshake::server(&mut reader, &mut writer, 7)?;
        hub.serve(reader, writer, info(false))
    });
    let (mut reader, mut writer) = (FrameReader::new(client_end.reader), FrameWriter::new(client_end.writer));
    Handshake::client(&mut reader, &mut writer, IpcRole::Ui, 1).unwrap();
    // A helper message on a UI pipe is a protocol error: the service answers Bye and ends the connection.
    writer.write_message(&Ipc::StopAudio).unwrap();
    assert!(matches!(
        reader.read_message().unwrap(),
        Some(Ipc::Bye { code: swapdesk_proto::ErrorCode::ProtocolViolation })
    ));
    assert!(serve.join().unwrap().is_err());
    m.finish();
}

/// A pipe writer that blocks while its gate is shut, as a pipe whose reader (the UI) stopped reading.
struct ShutGate {
    inner: PipeWriter,
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl std::io::Write for ShutGate {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (m, cv) = &*self.open;
        let _open = cv.wait_while(lock(m), |open| !*open).unwrap();
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_ui_that_stops_reading_is_cut_and_the_owner_is_told_so_it_can_close_the_pipe() {
    let m = Machine::new("sala");
    let (client_end, server_end) = duplex_blocking();
    let gate = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
    let (cut_tx, cut_rx) = channel();
    let hub = m.hub.clone();
    let server_gate = Arc::clone(&gate);
    let serve = std::thread::spawn(move || {
        let mut reader = FrameReader::new(server_end.reader);
        let mut writer = FrameWriter::new(ShutGate { inner: server_end.writer, open: server_gate });
        Handshake::server(&mut reader, &mut writer, 7)?;
        hub.serve_cancellable(
            reader,
            writer,
            info(false),
            Arc::new(move || {
                let _ = cut_tx.send(());
            }),
        )
    });
    let (mut reader, mut writer) = (FrameReader::new(client_end.reader), FrameWriter::new(client_end.writer));
    Handshake::client(&mut reader, &mut writer, IpcRole::Ui, 1).unwrap();
    wait_for("the connection", || (m.hub.connections() == 1).then_some(()));
    // The UI stops reading: the pipe's writer stalls, and the service goes on telling the UI things.
    *lock(&gate.0) = false;
    for i in 0..3000u32 {
        m.hub.hub.notify(Notify::ChatRead { session_id: 1, up_to: i });
    }
    cut_rx.recv_timeout(Duration::from_secs(5)).expect("the owner is told the connection was cut");
    // What the owner does with it (cancelling the pipe's I/O): the stalled write returns and the read ends.
    {
        let (open, cv) = &*gate;
        *lock(open) = true;
        cv.notify_all();
    }
    drop(writer);
    let ended = serve.join().unwrap();
    assert!(ended.is_ok(), "{ended:?}");
    assert_eq!(m.hub.connections(), 0);
    drop(reader);
    m.finish();
}

/// Two free UDP ports in a row on 127.0.0.1 (session, pairing), as a typed address implies (PROTOCOL.md 1). They
/// come from below Windows' dynamic range (49152 and up), which hands out ephemeral ports in sequence and would give
/// the second one to the next socket bound to port 0.
fn free_port_pair() -> (u16, u16) {
    let start = 41_000 + (std::process::id() % 2_000) as u16 * 2;
    for p in (start..48_000).step_by(2) {
        let a = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, p));
        let b = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, p + 1));
        if a.is_ok() && b.is_ok() {
            return (p, p + 1);
        }
    }
    panic!("no two free ports in a row");
}

#[test]
fn a_device_paired_through_a_typed_address_is_dialled_at_that_address() {
    // The M3 gate found it: after pairing with "127.0.0.1:<port>", Connect dialled the default port.
    let ports = free_port_pair();
    let b = Machine::build_at("beta", FakeNetInterfaces::new(Vec::new()), true, Some(ports));
    let a = Machine::new("alfa");
    let mut ua = a.client(info(false));
    let mut ub = b.client(info(false));
    let opened = ub.call(UiOp::PairOpen { device_id: None });
    let Ok(ReplyBody::PairingId(id_b)) = opened else { panic!("{opened:?}: {:?}", b.log.lines()) };
    let ReplyBody::PairingId(id_a) = ua.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    let typed = device_of(ua.call(UiOp::AddByAddress { ipv4: format!("127.0.0.1:{}", ports.0) }));
    assert!(matches!(ua.call(UiOp::PairStart { pairing_id: id_a, device_id: typed.id }).unwrap(), ReplyBody::Done));
    ua.pairing(id_a, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. }));
    ub.pairing(id_b, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. }));
    assert!(ua.call(UiOp::PairConfirm { pairing_id: id_a, matches: true }).is_ok());
    assert!(ub.call(UiOp::PairConfirm { pairing_id: id_b, matches: true }).is_ok());
    ua.pairing(id_a, "Done", |s| matches!(s, PairingState::Done { .. }));
    wait_for("the typed address is learned", || {
        a.hub.hub.address_of(&b.id()).filter(|x| *x == SocketAddrV4::new(Ipv4Addr::LOCALHOST, ports.0)).map(|_| ())
    });
    // The placeholder of the typed address leaves the list: the paired device stands for it now.
    wait_for("the typed placeholder is gone", || {
        (!a.hub.hub.device_views().iter().any(|d| d.id.starts_with("addr:"))).then_some(())
    });
    drop((ua, ub));
    a.finish();
    b.finish();
}

#[test]
fn a_just_paired_device_gets_a_valid_preset_that_the_accept_takes() {
    // Nothing stored yet (just paired): the preset is what the viewer asked for, without UNATTENDED and
    // ADMIN_TERMINAL. Masking with those sets once dropped VIEW too, and "Aceitar" then answered Invalid.
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, PermissionSet::EMPTY);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    viewer.sees(&host, false);
    host.hub.set_console_session(Some(1));
    let mut host_ui = host.client(info(false));
    let mut viewer_ui = viewer.client(info(false));

    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let incoming = host_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    let preset = PermissionSet::from_bits(incoming.preset).expect("the preset is a valid set");
    assert!(preset.contains(PermissionSet::CONTROL), "control asked for: {:#06x}", incoming.preset);
    assert!(!preset.contains(PermissionSet::UNATTENDED) && !preset.contains(PermissionSet::ADMIN_TERMINAL));
    assert!(
        host_ui
            .call(UiOp::AnswerIncoming {
                request_id: incoming.request_id,
                decision: Decision::Accept,
                grants: incoming.preset
            })
            .is_ok(),
        "the preset as it came is accepted"
    );
    let session_id = viewer_ui.connected(req);
    assert!(viewer_ui.call(UiOp::Disconnect { session_id }).is_ok());
    drop((host_ui, viewer_ui));
    host.finish();
    viewer.finish();
}

/// ACCOUNTS.md 15 S2: the three new records of 14.2 read back what they wrote and refuse what breaks their rules.
#[test]
fn accounts_index_account_settings_and_peer_ports_round_trip() {
    use super::store::{AccountEntry, AccountSettings, AccountsIndex, PeerPort, PeerPorts};
    use swapdesk_proto::Wire;
    let index = AccountsIndex {
        accounts: vec![
            AccountEntry { sid: "S-1-5-21-1-2-3-1001".into(), slot: 0, created_ms: 1_700_000_000_000 },
            AccountEntry { sid: "S-1-5-21-1-2-3-1002".into(), slot: 1, created_ms: 1_700_000_100_000 },
        ],
    };
    assert_eq!(AccountsIndex::decode(&index.encode().unwrap()).unwrap(), index);
    let mut twice = index.clone();
    twice.accounts[1].slot = 0;
    assert!(AccountsIndex::decode(&twice.encode().unwrap()).is_err(), "a slot twice");
    let mut big_slot = index.clone();
    big_slot.accounts[1].slot = 32;
    assert!(AccountsIndex::decode(&big_slot.encode().unwrap()).is_err(), "slot 32 does not exist");

    let settings =
        AccountSettings { name: "Sala · Alex".into(), receive: false, lock_on_end: true, keep_awake: false };
    assert_eq!(AccountSettings::decode(&settings.encode().unwrap()).unwrap(), settings);
    assert_eq!(
        AccountSettings::new("Sala"),
        AccountSettings { name: "Sala".into(), receive: true, lock_on_end: false, keep_awake: true }
    );

    let ports = PeerPorts {
        entries: vec![
            PeerPort { device_id: [1; 16], session_port: 47822, seen_ms: 5 },
            PeerPort { device_id: [2; 16], session_port: 47820, seen_ms: 6 },
        ],
    };
    assert_eq!(PeerPorts::decode(&ports.encode().unwrap()).unwrap(), ports);
    let mut dup = ports.clone();
    dup.entries[1].device_id = [1; 16];
    assert!(PeerPorts::decode(&dup.encode().unwrap()).is_err(), "a device twice");
    // A record of another format is refused, never misread.
    let mut bytes = ports.encode().unwrap();
    bytes[0] = 9;
    assert!(PeerPorts::decode(&bytes).is_err());

    // A damaged account.bin reads as the defaults, with a trace line.
    let secrets = MemorySecretStore::new();
    let log = LogCapture::new();
    swapdesk_platform::SecretStore::store(&secrets, super::store::ACCOUNT_SETTINGS_NAME, b"junk").unwrap();
    let read = super::store::load_account_settings(&secrets, "Sala", &log);
    assert_eq!(read, AccountSettings::new("Sala"));
    assert!(log.lines().iter().any(|l| l.text.contains("account.bin")));
}

/// ACCOUNTS.md 6 and 15 S3: an account with no pairing neither listens on the approved addresses nor announces its
/// record; it does while its pairing mode is open, and from its first pairing until its last one is forgotten.
#[test]
fn an_account_without_pairings_neither_listens_nor_announces() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    assert!(!m.core.receiving(), "nothing paired: no listening");
    assert_eq!(m.presence.announced(), Some(false), "and no record");
    let mut ui = m.client(info(false));
    let ReplyBody::PairingId(id) = ui.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    assert!(m.core.receiving() && m.presence.announced() == Some(true), "pairing mode open");
    assert!(ui.call(UiOp::PairCancel { pairing_id: id }).is_ok());
    assert!(!m.core.receiving() && m.presence.announced() == Some(false), "pairing mode closed");
    m.pin(&other, PermissionSet::CONTROL);
    assert!(m.core.receiving() && m.presence.announced() == Some(true), "a pairing");
    assert!(ui.call(UiOp::ForgetDevice { device_id: other.hex() }).is_ok());
    assert!(!m.core.receiving() && m.presence.announced() == Some(false), "the last pairing forgotten");
    drop(ui);
    m.finish();
    other.finish();
}

/// ACCOUNTS.md 5.2 and 15 S3: in production a typed `address:port` reaches an account on another slot, and pairing by
/// that address goes to the next port; port 0 is not a port.
#[test]
fn a_typed_address_with_a_port_is_accepted_in_production_and_pairs_on_the_next_port() {
    let m = Machine::build("sala", FakeNetInterfaces::windows_typical(), false);
    let mut ui = m.client(info(false));
    let typed = device_of(ui.call(UiOp::AddByAddress { ipv4: "192.168.1.50:47822".into() }));
    assert_eq!(typed.id, "addr:192.168.1.50:47822");
    let manual = lock(&m.hub.hub.manual).last().cloned().expect("kept");
    assert_eq!((manual.session_port, manual.pair_port()), (47822, 47823), "pairs on the next port");
    assert_eq!(
        ui.call(UiOp::AddByAddress { ipv4: "192.168.1.50:0".into() }).unwrap_err(),
        ApiCode::Invalid,
        "port 0 is not a port"
    );
    drop(ui);
    m.finish();
}

/// ACCOUNTS.md 5.2: a paired device seen on a port other than the default is dialled there when discovery no longer
/// shows it, also after a restart (the port is kept in `peers.bin`), and forgetting it forgets the port.
#[test]
fn a_paired_device_out_of_discovery_is_dialled_at_its_last_seen_port() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    m.trust.update(|s| s.devices[0].last_addrs = vec![[192, 168, 1, 50]]).unwrap();
    let id = other.id();
    assert_eq!(m.hub.hub.address_of(&id).map(|a| a.port()), Some(crate::service::DEFAULT_SESSION_PORT));
    // Discovery shows it on slot 1 of its machine.
    let mut seen = other_record(&other);
    seen.session_port = 47822;
    seen.addrs = vec![Ipv4Addr::new(192, 168, 1, 50)];
    m.presence.set(vec![seen]);
    assert_eq!(m.hub.hub.address_of(&id), Some(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 47822)));
    // Out of discovery: the last address at the last port.
    m.presence.set(Vec::new());
    assert_eq!(m.hub.hub.address_of(&id), Some(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 47822)));
    // Kept on disk: a fresh read of the store finds it.
    let kept = super::store::load_peer_ports(&*m.secrets);
    assert_eq!(kept.entries.iter().find(|e| e.device_id == id).map(|e| e.session_port), Some(47822));
    // Forget: the port goes with the device.
    let mut ui = m.client(info(false));
    assert!(ui.call(UiOp::ForgetDevice { device_id: other.hex() }).is_ok());
    assert!(super::store::load_peer_ports(&*m.secrets).entries.is_empty());
    drop(ui);
    m.finish();
    other.finish();
}

fn other_record(other: &Machine) -> PresencePeer {
    PresencePeer {
        device_id: other.id(),
        name: "other-pc".into(),
        os: Os::Windows,
        pairing_open: false,
        addrs: vec![Ipv4Addr::LOCALHOST],
        session_port: crate::service::DEFAULT_SESSION_PORT,
        pair_port: crate::service::DEFAULT_PAIR_PORT,
        proto_minor: 0,
    }
}

/// ACCOUNTS.md 9.2: an elevation the viewer asked for is written to the host's activity as `Elevated`, shown as
/// "Controle elevado".
#[test]
fn an_elevation_is_written_to_the_activity_as_elevated_control() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL | PermissionSet::UNATTENDED);
    let mut plain = m.client(info(false));
    let granted = PermissionSet::CONTROL | PermissionSet::UNATTENDED;
    m.hub.host_ui().event(swapdesk_session::HostEvent::Elevated { device: other.id(), session_id: 7, granted });
    let ReplyBody::Activity(lines) = plain.call(UiOp::ActivityList { before_ms: None, limit: 10 }).unwrap() else {
        panic!()
    };
    assert_eq!(MsgKey::ALL[usize::from(lines[0].what.key)], MsgKey::AuditElevated, "{lines:?}");
    assert!(m.audit.entries().iter().any(|e| e.event == AuditEvent::Elevated));
    drop(plain);
    m.finish();
    other.finish();
}

/// Review of S3/S4, M6 (ACCOUNTS.md 5.5): when another program holds the account's pairing port, pairing does not
/// open (a peer's "Parear" would reach that program), and it says so instead of opening on what is left.
#[test]
fn pairing_does_not_open_when_its_port_is_held_by_another_program() {
    // The squatter takes its port from the system and keeps it: no window where another test could take it first.
    let squatter = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let pair_port = squatter.local_addr().unwrap().port();
    let session_port = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
    let m = Machine::build_at("sala", FakeNetInterfaces::new(Vec::new()), true, Some((session_port, pair_port)));
    let mut ui = m.client(info(false));
    assert_eq!(ui.call(UiOp::PairOpen { device_id: None }).unwrap_err(), ApiCode::Busy);
    assert!(m.log.lines().iter().any(|l| l.text.contains("held by another program")));
    drop(squatter);
    assert!(ui.call(UiOp::PairOpen { device_id: None }).is_ok(), "free again: pairing opens");
    drop(ui);
    m.finish();
}

/// An HTTP client that never answers (the hub's update tests need no download).
struct NoNet;

impl swapdesk_update::http::HttpGet for NoNet {
    fn get(
        &self,
        _url: &str,
        _max: u64,
        _sink: &mut dyn std::io::Write,
        _progress: &mut dyn FnMut(u64),
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), swapdesk_update::http::HttpError> {
        Err(swapdesk_update::http::HttpError::Network("no network in this test".into()))
    }
}

struct NoSaved;

impl swapdesk_update::manager::SavedStore for NoSaved {
    fn load(&self) -> swapdesk_update::manager::Saved {
        swapdesk_update::manager::Saved::default()
    }

    fn save(&self, _saved: &swapdesk_update::manager::Saved) {}
}

struct NoFolder;

impl swapdesk_update::manager::UpdateFolder for NoFolder {
    fn create(&self, _name: &str) -> std::io::Result<Box<dyn std::io::Write + Send>> {
        Err(std::io::Error::other("no folder in this test"))
    }

    fn remove(&self, _name: &str) {}

    fn path(&self, name: &str) -> std::path::PathBuf {
        name.into()
    }
}

/// UPDATES.md 10 and 13 (U4): the updater through the hub: the view, the install refused from a UI that is not the
/// console's and with a live session unless confirmed, the switch only from the console, and nothing without an
/// updater (a development instance).
#[test]
fn the_updater_answers_through_the_hub_with_its_refusals() {
    use swapdesk_update::manager::{Setup, UpdateManager};
    let m = Machine::new("sala");
    let mut ui = m.client(info(false));
    assert_eq!(ui.call(UiOp::UpdateGet).unwrap_err(), ApiCode::NotFound, "no updater: a development instance");
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let setup = Setup {
        running: swapdesk_update::Version::parse("1.1.0").unwrap(),
        target: "windows-x64".into(),
        base: "https://example.invalid/releases".into(),
        keys: Vec::new(),
        dev: false,
        first_delay_ms: 60_000,
    };
    let manager = UpdateManager::new(setup, Arc::new(NoNet), Arc::new(NoSaved), Arc::new(NoFolder), 0);
    let hooks = super::update::UpdaterHooks {
        now_ms: Box::new(|| 0),
        jitter_ms: Box::new(|| 0),
        live_sessions: {
            let live = Arc::clone(&live);
            Box::new(move || live.load(std::sync::atomic::Ordering::SeqCst))
        },
        ready: Box::new(|_, _| Ok(())),
    };
    m.hub.set_updater(super::update::Updater::new(manager, hooks));
    let ReplyBody::Update(view) = ui.call(UiOp::UpdateGet).unwrap() else { panic!() };
    assert_eq!((view.running.as_str(), view.check_on), ("1.1.0", true));
    let install = || UiOp::UpdateInstall { version: "1.1.1".into(), end_sessions: false };
    // Not the console's interface: refused, as the switch is.
    let mut other = m.client(ConnInfo { session_id: 2, ..info(false) });
    assert_eq!(other.call(install()).unwrap_err(), ApiCode::NotPermitted);
    assert_eq!(other.call(UiOp::UpdateSetCheck { on: false }).unwrap_err(), ApiCode::NotPermitted);
    // The console's, with a live session and no confirmation: refused until "Encerrar e atualizar".
    live.store(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(ui.call(install()).unwrap_err(), ApiCode::Busy);
    // The switch from the console: told to the interfaces.
    assert!(ui.call(UiOp::UpdateSetCheck { on: false }).is_ok());
    let told = ui.notify("UpdateChanged", |n| match n {
        Notify::UpdateChanged(v) => Some(v.check_on),
        _ => None,
    });
    assert!(!told);
    drop((ui, other));
    m.finish();
}

/// D65 (owner, 12/11): "Bloquear este dispositivo" blocks the device behind the address by its MAC, read from the
/// neighbour table; the reply is the list as it now is; Desbloquear removes it; a device whose MAC cannot be learned
/// is not blocked (its address would later belong to someone else).
#[test]
fn a_device_is_blocked_by_its_mac_and_unblocked_from_the_list() {
    struct Table;
    impl swapdesk_platform::NeighborTable for Table {
        fn cached_mac(&self, peer: std::net::IpAddr) -> Option<[u8; 6]> {
            (peer == std::net::IpAddr::from([192, 168, 0, 57])).then_some([0x02, 0x5a, 0xd0, 0, 0, 0x39])
        }
    }
    let m = Machine::new("sala");
    let mut ui = m.client(info(false));
    assert!(matches!(ui.call(UiOp::ListBlocked), Ok(ReplyBody::Blocked(l)) if l.is_empty()));
    let block = |ipv4: &str| UiOp::BlockDevice { ipv4: ipv4.into(), name: String::new() };
    assert_eq!(ui.call(block("192.168.0.57")).unwrap_err(), ApiCode::NotFound, "no neighbour table: nothing known");

    m.hub.set_neighbors(Arc::new(Table));
    let Ok(ReplyBody::Blocked(list)) = ui.call(block("192.168.0.57")) else { panic!("the list after blocking") };
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].mac.as_str(), list[0].last_ipv4.as_str()), ("02:5a:d0:00:00:39", "192.168.0.57"));
    assert!(m.trust.is_blocked_mac(&[0x02, 0x5a, 0xd0, 0, 0, 0x39]), "kept in the trust handle (blocked.bin)");
    assert_eq!(ui.call(block("192.168.0.58")).unwrap_err(), ApiCode::NotFound, "a MAC that cannot be learned");
    assert_eq!(ui.call(block("not an address")).unwrap_err(), ApiCode::Invalid);

    let Ok(ReplyBody::Blocked(list)) = ui.call(UiOp::UnblockDevice { mac: "02:5A:D0:00:00:39".into() }) else {
        panic!("the list after unblocking")
    };
    assert!(list.is_empty() && !m.trust.any_blocked_mac());
    assert_eq!(ui.call(UiOp::UnblockDevice { mac: "02:5a:d0:00:00:39".into() }).unwrap_err(), ApiCode::NotFound);
    drop(ui);
    m.finish();
}
