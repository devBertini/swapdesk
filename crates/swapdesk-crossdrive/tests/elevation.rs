//! "Continuar" of the administrator confirmation, end to end without the operating system (docs/ELEVATION.md 2.1): a
//! real `UiHub` serves the asking interface over an in-memory pipe, the real `ServiceBackend` and `UiState` drive it
//! the way a shell does, and the platform's `Elevator` is replaced by one that runs the elevated instance's side
//! (`elevated_client::run`) against the hub's restricted loop over a second pipe. What the UAC prompt and the
//! program's own start do is Windows' and is the owner's hand check; what the service decides on the way is proved here,
//! the refusals as much as the pass.

use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_fakes::{FakeNetInterfaces, LogCapture, MemorySecretStore, MonotonicClock};
use swapdesk_host::service::{ServiceConfig, ServiceCore, ServiceDeps};
use swapdesk_host::ui_hub::{ConnInfo, DevOptions, HubParams, MachineSettings, NoHooks, NoPresence, UiHub};
use swapdesk_ipc::{ElevAction, ElevationTicket, FrameReader, FrameWriter, Handshake};
use swapdesk_proto::{Os, PermissionSet};
use swapdesk_trust::{
    AuditEvent, Identity, MemoryAudit, PairedDevice, SystemWallClock, TrustHandle, UserRef, device_id_hex,
};
use swapdesk_ui::elevated_client::{self, ElevatedExit};
use swapdesk_ui::service_backend::{Elevator, ElevatorRun, ServiceBackend};
use swapdesk_ui::state::ElevPhase;
use swapdesk_ui::{
    ApiCode, Command, ControlId, ElevatedAction, Language, Permission, Permissions, Reply, UiBackend, UiEvent, UiState,
    UiWaker,
};

const SID: &str = "S-1-5-21-1";
const WAIT: Duration = Duration::from_secs(20);

// ---------- in-memory duplex pipe ----------

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    left: Vec<u8>,
    pos: usize,
}

struct PipeWriter {
    tx: Sender<Vec<u8>>,
}

struct End {
    reader: PipeReader,
    writer: PipeWriter,
}

fn duplex() -> (End, End) {
    let (a_tx, a_rx) = channel();
    let (b_tx, b_rx) = channel();
    (
        End { reader: PipeReader { rx: b_rx, left: Vec::new(), pos: 0 }, writer: PipeWriter { tx: a_tx } },
        End { reader: PipeReader { rx: a_rx, left: Vec::new(), pos: 0 }, writer: PipeWriter { tx: b_tx } },
    )
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.left.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.left = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.left.len() - self.pos);
        buf[..n].copy_from_slice(&self.left[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx.send(buf.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

// ---------- the service: one machine with one paired device ----------

struct Machine {
    core: Arc<ServiceCore>,
    hub: UiHub,
    trust: Arc<TrustHandle>,
    audit: Arc<MemoryAudit>,
    /// The paired device's id, as the interface names it.
    device: String,
}

impl Machine {
    fn new() -> Self {
        let identity = Arc::new(Identity::generate(0).expect("identity"));
        let secrets = Arc::new(MemorySecretStore::new());
        let trust = TrustHandle::open(secrets.clone()).expect("trust");
        let trace = LogCapture::new();
        let interfaces = Arc::new(FakeNetInterfaces::new(Vec::new()));
        let clock = Arc::new(MonotonicClock::new());
        let audit = Arc::new(MemoryAudit::new());
        let hub = UiHub::new(HubParams {
            os: Os::Windows,
            os_version: "test".into(),
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            secrets: secrets.clone(),
            settings: MachineSettings::new("sala"),
            interfaces: interfaces.clone(),
            presence: Arc::new(NoPresence),
            audit: audit.clone(),
            clock: clock.clone(),
            wall: Arc::new(SystemWallClock),
            trace: Arc::new(trace.clone()),
            hooks: Arc::new(NoHooks),
            dev: Some(DevOptions { pair_listen: Vec::new() }),
        });
        let deps = ServiceDeps {
            identity,
            trust: Arc::clone(&trust),
            clock,
            trace: Arc::new(trace),
            ui: hub.host_ui(),
            interfaces,
        };
        let cfg = ServiceConfig::new("sala", Os::Windows, "test", deps)
            .dev_loopback(std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0));
        let core = Arc::new(ServiceCore::start(cfg).expect("core starts"));
        hub.bind_core(&core);
        // The interface of this test runs in session 1, the console's: only the console's UI may ask for a ticket.
        hub.set_console_session(Some(1));
        // A device paired earlier, which may control this machine.
        let other = Identity::generate(0).expect("identity");
        trust
            .update(|s| {
                let mut d =
                    PairedDevice::new_paired(*other.spki(), "other-pc".into(), Os::Windows, "11".into(), 1, None);
                d.grants_here = PermissionSet::CONTROL;
                s.devices.push(d);
            })
            .expect("store");
        let device = device_id_hex(&other.device_id());
        Self { core, hub, trust, audit, device }
    }

    fn granted_here(&self) -> PermissionSet {
        self.trust.read(|s| s.devices[0].grants_here)
    }

    fn grant_changes(&self) -> Vec<UserRef> {
        self.audit
            .entries()
            .into_iter()
            .filter_map(|e| match e.event {
                AuditEvent::GrantsChanged { to, by, .. } if to.contains(PermissionSet::UNATTENDED) => Some(by),
                _ => None,
            })
            .collect()
    }

    fn shutdown(self) {
        let Machine { core, hub, .. } = self;
        hub.shutdown();
        let end = Instant::now() + Duration::from_secs(3);
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
    }
}

fn user() -> UserRef {
    UserRef { id: SID.into(), name: "tester".into() }
}

/// The asking interface: an administrator account's filtered token, which may ask for a ticket but not approve one.
fn asker() -> ConnInfo {
    ConnInfo { session_id: 1, user: user(), admin: false, can_elevate: true }
}

/// The elevated instance's connection.
fn approver(admin: bool) -> ConnInfo {
    ConnInfo { session_id: 1, user: user(), admin, can_elevate: true }
}

/// The hub serves one end of a pipe as the asking UI and a `ServiceBackend` drives the other.
fn ui(hub: &UiHub, info: ConnInfo) -> (ServiceBackend, JoinHandle<()>) {
    let (client_end, server_end) = duplex();
    let hub = hub.clone();
    let serve = std::thread::spawn(move || {
        let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
        Handshake::server(&mut reader, &mut writer, 1).expect("server handshake");
        let _ = hub.serve(reader, writer, info);
    });
    let backend = ServiceBackend::start_with_sink(client_end.reader, client_end.writer, Arc::new(NoWake), 4321, None)
        .expect("handshake");
    (backend, serve)
}

// ---------- the platform's elevator, replaced ----------

/// What the elevated instance was shown and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    action: ElevAction,
    peer: String,
}

#[derive(Clone)]
enum Uac {
    /// The person says "No" at the system's prompt: no instance runs.
    No,
    /// The person says "Yes": the instance runs with this connection and answers `approve` in its window.
    Yes { info: ConnInfo, approve: bool },
}

struct FakeElevator {
    hub: UiHub,
    uac: Uac,
    runs: Arc<Mutex<Vec<ElevatorRun>>>,
    shown: Arc<Mutex<Vec<Shown>>>,
    exits: Arc<Mutex<Vec<ElevatedExit>>>,
}

impl Elevator for FakeElevator {
    fn run(&self, ticket: &ElevationTicket, _line: &str, _deadline: Instant) -> ElevatorRun {
        let run = match &self.uac {
            Uac::No => ElevatorRun::Declined,
            Uac::Yes { info, approve } => {
                let (client_end, server_end) = duplex();
                let hub = self.hub.clone();
                let info = info.clone();
                let serve = std::thread::spawn(move || {
                    let (mut reader, mut writer) =
                        (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
                    Handshake::server(&mut reader, &mut writer, 1).expect("server handshake");
                    let _ = hub.serve_elevator(reader, writer, info);
                });
                let shown = self.shown.clone();
                let approve = *approve;
                let exit = elevated_client::run(client_end.reader, client_end.writer, ticket, |action, peer, _left| {
                    shown.lock().unwrap().push(Shown { action: action.clone(), peer: peer.to_owned() });
                    approve
                });
                serve.join().expect("the elevator loop ends");
                self.exits.lock().unwrap().push(exit);
                ElevatorRun::Ended { exit_code: Some(u32::from(exit.code())) }
            }
        };
        self.runs.lock().unwrap().push(run);
        run
    }
}

// ---------- the interface, driven as a shell drives it ----------

struct Shell {
    backend: ServiceBackend,
    state: UiState,
}

impl Shell {
    fn send(&mut self, command: Command) {
        let ticket = self.backend.send(command.clone());
        self.state.track(ticket, &command);
    }

    /// Delivers what the backend queued to the state and sends the commands the state wants sent, until `done`.
    fn until(&mut self, what: &str, done: impl Fn(&UiState) -> bool) {
        let end = Instant::now() + WAIT;
        loop {
            for event in self.backend.drain() {
                self.state.apply(event);
            }
            for command in self.state.take_followups() {
                self.send(command);
            }
            if done(&self.state) {
                return;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}; the step is {:?}", self.state.elevation());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The button of the step that is on screen.
    fn press(&mut self, id: ControlId) {
        for command in self.state.activate(id).commands {
            self.send(command);
        }
    }

    /// A command straight to the service, outside the state: its answer.
    fn ask(&mut self, command: Command) -> Result<Reply, ApiCode> {
        let ticket = self.backend.send(command);
        let end = Instant::now() + WAIT;
        loop {
            for event in self.backend.drain() {
                if let UiEvent::Reply { ticket: t, result } = &event
                    && *t == ticket
                {
                    return result.clone().map_err(|e| e.code);
                }
                self.state.apply(event);
            }
            assert!(Instant::now() < end, "no answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Rig {
    machine: Machine,
    shell: Shell,
    serve: JoinHandle<()>,
    runs: Arc<Mutex<Vec<ElevatorRun>>>,
    shown: Arc<Mutex<Vec<Shown>>>,
    exits: Arc<Mutex<Vec<ElevatedExit>>>,
}

impl Rig {
    fn new(uac: Uac) -> Self {
        let machine = Machine::new();
        let (backend, serve) = ui(&machine.hub, asker());
        let (runs, shown, exits) = (Arc::default(), Arc::default(), Arc::default());
        backend.set_elevator(Arc::new(FakeElevator {
            hub: machine.hub.clone(),
            uac,
            runs: Arc::clone(&runs),
            shown: Arc::clone(&shown),
            exits: Arc::clone(&exits),
        }));
        let shell = Shell { backend, state: UiState::new(Language::En, false) };
        Rig { machine, shell, serve, runs, shown, exits }
    }

    /// The grants the card's switch asks for: control plus entering without asking.
    fn wanted() -> Permissions {
        Permissions::NONE.with(Permission::Control).with(Permission::Unattended)
    }

    /// Asks for `wanted()`: the service answers `NeedsAdmin` and the step opens.
    fn ask_for_unattended(&mut self) {
        let command = Command::SetGrants { device_id: self.machine.device.clone(), grants: Self::wanted() };
        self.shell.send(command);
        self.shell.until("the step to open", |s| s.elevation().is_some());
        let step = self.shell.state.elevation().unwrap();
        assert_eq!(step.phase, ElevPhase::Ask);
        assert_eq!(
            step.action,
            ElevatedAction::SetGrants { device_id: self.machine.device.clone(), grants: Self::wanted() }
        );
        assert_eq!(self.machine.granted_here(), PermissionSet::CONTROL, "nothing changed yet");
    }

    fn finish(self) {
        let Rig { machine, shell, serve, .. } = self;
        drop(shell);
        let _ = serve.join();
        machine.shutdown();
    }
}

#[test]
fn continuar_runs_the_elevated_confirmation_and_the_step_closes_with_the_grant_applied() {
    let mut rig = Rig::new(Uac::Yes { info: approver(true), approve: true });
    rig.ask_for_unattended();

    // "Continuar".
    rig.shell.press(ControlId::DialogPrimary);
    rig.shell.until("the step to close", |s| s.elevation().is_none());
    // The resend of the command went through on the pass, and the service wrote the grant down.
    let end = Instant::now() + WAIT;
    while !rig.machine.granted_here().contains(PermissionSet::UNATTENDED) {
        assert!(Instant::now() < end, "the grant never landed");
        rig.shell.until("a quiet moment", |_| true);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(rig.machine.granted_here().bits(), Rig::wanted().0);

    // The instance was shown the action as the service holds it, and ended confirmed.
    assert_eq!(
        *rig.shown.lock().unwrap(),
        vec![Shown {
            action: ElevAction::SetGrants { device_id: rig.machine.device.clone(), grants: Rig::wanted().0 },
            peer: "other-pc".into(),
        }]
    );
    assert_eq!(*rig.exits.lock().unwrap(), vec![ElevatedExit::Confirmed]);
    assert_eq!(*rig.runs.lock().unwrap(), vec![ElevatorRun::Ended { exit_code: Some(0) }]);
    // The audit entry names the asking connection's user (the approver is the same user here).
    assert_eq!(rig.machine.grant_changes(), vec![user()]);
    assert!(rig.shell.state.error().is_none(), "no error strip: {:?}", rig.shell.state.error());

    // The pass was used up: widening again is a new change that needs a new confirmation.
    let again = Permissions::NONE.with(Permission::Control).with(Permission::Unattended).with(Permission::Audio);
    let answer = rig.shell.ask(Command::SetGrants { device_id: rig.machine.device.clone(), grants: again });
    assert_eq!(answer, Err(ApiCode::NeedsAdmin));
    rig.finish();
}

#[test]
fn a_declined_uac_shows_the_failure_and_changes_nothing() {
    let mut rig = Rig::new(Uac::No);
    rig.ask_for_unattended();

    rig.shell.press(ControlId::DialogPrimary);
    rig.shell.until("the step to fail", |s| matches!(s.elevation().map(|e| &e.phase), Some(ElevPhase::Failed(_))));
    assert_eq!(rig.shell.state.elevation().unwrap().phase, ElevPhase::Failed(ApiCode::Refused));
    assert!(rig.shell.state.error().is_none(), "the reason is in the step, not the strip");
    assert_eq!(*rig.runs.lock().unwrap(), vec![ElevatorRun::Declined]);
    assert!(rig.shown.lock().unwrap().is_empty(), "no instance ran");

    // The ticket was ended and no pass was made: the same command is still refused, and nothing was granted.
    let answer = rig.shell.ask(Command::SetGrants { device_id: rig.machine.device.clone(), grants: Rig::wanted() });
    assert_eq!(answer, Err(ApiCode::NeedsAdmin));
    assert_eq!(rig.machine.granted_here(), PermissionSet::CONTROL);
    assert!(rig.machine.grant_changes().is_empty());

    // "Tentar de novo" asks a new ticket; this time the person says no again inside the window: same result.
    rig.shell.press(ControlId::DialogPrimary);
    rig.shell.until("the second run", |_| rig.runs.lock().unwrap().len() == 2);
    rig.finish();
}

#[test]
fn an_unelevated_elevated_instance_gets_needs_admin_and_no_pass() {
    // The prompt was answered, but the process that came up is not an administrator's (a policy, an unusual token).
    let mut rig = Rig::new(Uac::Yes { info: approver(false), approve: true });
    rig.ask_for_unattended();

    rig.shell.press(ControlId::DialogPrimary);
    rig.shell.until("the step to fail", |s| matches!(s.elevation().map(|e| &e.phase), Some(ElevPhase::Failed(_))));
    // The service said the process is no administrator (exit code 6): the step says it was not permitted.
    assert_eq!(*rig.exits.lock().unwrap(), vec![ElevatedExit::NotAdmin]);
    assert_eq!(ElevatedExit::NotAdmin.code(), 6);
    assert_eq!(*rig.runs.lock().unwrap(), vec![ElevatorRun::Ended { exit_code: Some(6) }]);
    assert_eq!(rig.shell.state.elevation().unwrap().phase, ElevPhase::Failed(ApiCode::NotPermitted));
    assert!(rig.shown.lock().unwrap().is_empty(), "the action is not even described to a process that cannot approve");

    // No pass: the command is refused as before, and nothing changed.
    let answer = rig.shell.ask(Command::SetGrants { device_id: rig.machine.device.clone(), grants: Rig::wanted() });
    assert_eq!(answer, Err(ApiCode::NeedsAdmin));
    assert_eq!(rig.machine.granted_here(), PermissionSet::CONTROL);
    assert!(rig.machine.grant_changes().is_empty());
    rig.finish();
}
