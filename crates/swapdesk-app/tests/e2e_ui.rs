//! The real `Swapdesk.exe` windows pair two machines and open a session with video, driven through the screens only.
//!
//! The M3 gate (`swapdesk-service/tests/gate_m3.rs`) drives two development services with a fake UI client over the
//! pipes. This test starts the same two services (`--console --instance N --dev-loopback`, 1 = viewer, 2 = host) and
//! the two real windows (`Swapdesk.exe --instance N --service-pid <pid>`) and drives the windows through UI Automation
//! (the AccessKit adapter of each window): a PowerShell helper (`tests/e2e/uia.ps1`; this crate forbids `unsafe`)
//! uses the Invoke and Window patterns only. No mouse and no keyboard input is sent anywhere.
//!
//! Steps: the host window opens pairing mode, the viewer window adds the host, starts the pairing, both windows show
//! the same code and confirm it, the viewer clicks "Controlar", the host window accepts, the viewer window opens, the
//! video shows, Disconnect, close.
//!
//! Two things the screens cannot do yet, so the test uses a third UI client on the viewer service's pipe for exactly
//! those two calls (named in the output as WORKAROUND) and records each as a DEFECT: the screens have no address field
//! (discovery is off in `--dev-loopback`, so no device appears) and the screens never send `PairStart` after
//! `PairOpen` (the viewer waits in "Open" forever). Everything else is the windows' own work.
//!
//! Run it on an idle PC (it opens windows and takes screenshots; the PNGs are kept in a folder this test prints):
//! `cargo nextest run -p swapdesk-app --run-ignored only e2e_ui --no-capture`.
#![cfg(windows)]

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use swapdesk_ipc::{ApiCode, FrameReader, FrameWriter, Handshake, IpcRole, Message, ReplyBody, UiOp, status_code};
use swapdesk_sys_win::{PipeWriter, active_console_session, connect};

const WAIT: Duration = Duration::from_secs(20);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAIN_WINDOW: &str = "^Swapdesk$";
const VIEWER_WINDOW: &str = ", Swapdesk$";

// The strings of crates/swapdesk-ui/src/strings.rs (pt-BR, the default language of the app).
const PAIR_THIS_COMPUTER: &str = "Parear este computador";
const CANCEL: &str = "Cancelar";
const CODES_MATCH: &str = "Os códigos são iguais";
const PAIR_DONE: &str = "Pronto";
const CONTROL: &str = "Controlar";
const ACCEPT: &str = "Aceitar";
const ALLOW_CONTROL: &str = "Controlar";
const DISCONNECT: &str = "Desconectar";

fn until<T>(what: &str, limit: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn dev_dir(instance: u16) -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("%LOCALAPPDATA%"))
        .join("Swapdesk-dev")
        .join(instance.to_string())
}

/// Deletes what an earlier run left in a dev folder, file by file (nothing but our own tree).
fn clear_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            clear_dir(&p);
            let _ = std::fs::remove_dir(&p);
        } else {
            std::fs::remove_file(&p).unwrap_or_else(|err| panic!("cannot delete {}: {err}", p.display()));
        }
    }
}

fn pids_in_trace(trace: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for line in trace.lines().filter(|l| l.contains("helper started (pid ")) {
        if let Some(rest) = line.split("helper started (pid ").nth(1)
            && let Some(n) = rest.split(')').next().and_then(|n| n.trim().parse::<u32>().ok())
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

fn image_of(pid: u32) -> Option<String> {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.starts_with('"'))?;
    Some(line.split("\",\"").next()?.trim_matches('"').to_string())
}

fn kill_pid(pid: u32) {
    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).creation_flags(CREATE_NO_WINDOW).output();
}

/// The folder of the binaries this build made (`target/debug`): the test exe lives in `target/debug/deps`.
fn bin_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    exe.parent().and_then(Path::parent).expect("target/debug").to_path_buf()
}

/// One real `swapdesk-service --console --dev-loopback` process. Dropping it kills what a failed run left.
struct Service {
    instance: u16,
    child: Child,
    dir: PathBuf,
    out: PathBuf,
    helpers: Vec<u32>,
}

impl Service {
    fn start(instance: u16, logs: &Path) -> Self {
        let dir = dev_dir(instance);
        std::fs::create_dir_all(&dir).expect("dev folder");
        clear_dir(&dir);
        let out = logs.join(format!("service-{instance}.out"));
        let file = File::create(&out).expect("service log file");
        let err = file.try_clone().expect("clone log handle");
        let exe = bin_dir().join("swapdesk-service.exe");
        assert!(
            exe.exists(),
            "{} is missing: build with `cargo build -p swapdesk-service -p swapdesk-helper` first",
            exe.display()
        );
        let child = Command::new(exe)
            .args(["--console", "--instance", &instance.to_string(), "--dev-loopback"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("the service starts");
        Self { instance, child, dir, out, helpers: Vec::new() }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.dir.join("trace.log")).unwrap_or_default()
    }

    fn refresh_helpers(&mut self) {
        for p in pids_in_trace(&self.trace()) {
            if !self.helpers.contains(&p) {
                self.helpers.push(p);
            }
        }
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    fn keep_trace(&self, logs: &Path) {
        let _ = std::fs::copy(self.dir.join("trace.log"), logs.join(format!("service-{}-trace.log", self.instance)));
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.refresh_helpers();
        if std::thread::panicking() {
            let t = self.trace();
            let lines: Vec<&str> = t.lines().collect();
            eprintln!("---- trace of instance {} (last 60 of {} lines) ----", self.instance, lines.len());
            for l in &lines[lines.len().saturating_sub(60)..] {
                eprintln!("{l}");
            }
        }
        if self.alive() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        for &p in &self.helpers {
            if image_of(p).is_some_and(|n| n.eq_ignore_ascii_case("swapdesk-helper.exe")) {
                kill_pid(p);
            }
        }
    }
}

/// One real `Swapdesk.exe` process (a GUI program: its windows are what the test drives).
struct App {
    child: Child,
}

impl App {
    fn start(instance: u16, service_pid: u32) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_Swapdesk"))
            .args(["--instance", &instance.to_string(), "--service-pid", &service_pid.to_string()])
            .env_remove("SWAPDESK_LANG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Swapdesk.exe starts");
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The UI Automation helper process.

fn hex(s: &str) -> String {
    if s.is_empty() { "-".into() } else { s.bytes().map(|b| format!("{b:02x}")).collect() }
}

fn unhex(h: &str) -> String {
    let bytes: Vec<u8> = (0..h.len() / 2).filter_map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

struct Uia {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

/// What `find` reports for the first match.
#[derive(Debug, Clone)]
struct Found {
    window: String,
    #[allow(dead_code)]
    kind: String,
    enabled: bool,
    name: String,
}

/// How a control is looked up.
#[derive(Clone, Copy)]
struct Sel<'a> {
    pid: u32,
    /// A regex on the window's name (`None`: any window of the process).
    window: Option<&'a str>,
    /// `exact`, `contains` or `regex`.
    mode: &'a str,
    name: &'a str,
    /// A UI Automation control type (`Button`, `Edit`...), or none.
    kind: Option<&'a str>,
}

impl<'a> Sel<'a> {
    fn button(pid: u32, window: &'a str, name: &'a str) -> Self {
        Self { pid, window: Some(window), mode: "exact", name, kind: Some("Button") }
    }

    fn any(pid: u32, window: Option<&'a str>, mode: &'a str, name: &'a str) -> Self {
        Self { pid, window, mode, name, kind: None }
    }

    fn args(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.pid,
            self.window.map_or("-".into(), hex),
            self.mode,
            hex(self.name),
            self.kind.map_or("-".into(), hex)
        )
    }
}

impl Uia {
    fn spawn() -> Self {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("e2e").join("uia.ps1");
        assert!(script.exists(), "{} is missing", script.display());
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("powershell starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let me = Self { child, stdin, lines };
        let ready = me.lines.recv_timeout(Duration::from_secs(60)).expect("the UI Automation helper says ready");
        assert_eq!(ready.trim().trim_start_matches('\u{feff}'), "ready");
        me
    }

    fn ask(&mut self, cmd: &str) -> Result<String, String> {
        writeln!(self.stdin, "{cmd}").map_err(|e| format!("write to the helper: {e}"))?;
        self.stdin.flush().map_err(|e| format!("flush: {e}"))?;
        let l = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .map_err(|e| format!("the helper did not answer `{}`: {e}", cmd.split(' ').next().unwrap_or("")))?;
        let (tag, body) = l.split_once(' ').unwrap_or((l.as_str(), ""));
        let text = unhex(body);
        match tag {
            "OK" => Ok(text),
            _ => Err(text),
        }
    }

    fn windows(&mut self, pid: u32) -> String {
        self.ask(&format!("windows {pid}")).unwrap_or_else(|e| format!("(windows failed: {e})"))
    }

    fn tree(&mut self, pid: u32) -> String {
        self.ask(&format!("tree {pid}")).unwrap_or_else(|e| format!("(tree failed: {e})"))
    }

    fn find(&mut self, sel: Sel) -> Result<Option<Found>, String> {
        let text = self.ask(&format!("find {}", sel.args()))?;
        if text.trim() == "NONE" {
            return Ok(None);
        }
        let f: Vec<&str> = text.trim_end_matches(['\r', '\n']).splitn(5, '\t').collect();
        if f.len() < 5 || f[0] != "FOUND" {
            return Err(format!("unexpected find reply: {text}"));
        }
        Ok(Some(Found {
            window: f[1].into(),
            kind: f[2].into(),
            enabled: f[3].eq_ignore_ascii_case("true"),
            name: f[4].into(),
        }))
    }

    fn invoke(&mut self, sel: Sel) -> Result<String, String> {
        let text = self.ask(&format!("invoke {}", sel.args()))?;
        if text.starts_with("INVOKED") {
            Ok(text.trim().replace('\t', " | "))
        } else {
            Err(format!("invoke: {}", text.trim()))
        }
    }

    fn close(&mut self, pid: u32) {
        let _ = self.ask(&format!("close {pid}"));
    }

    /// One PNG per window of the process; returns the helper's report lines.
    fn shot(&mut self, pid: u32, prefix: &Path) -> String {
        self.ask(&format!("shot {pid} {}", hex(&prefix.to_string_lossy())))
            .unwrap_or_else(|e| format!("(shot failed: {e})"))
    }
}

impl Drop for Uia {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit");
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end && self.child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// A third UI client on a service's pipe, for the two calls the screens cannot make (see the module comment).

struct SideClient {
    name: &'static str,
    writer: FrameWriter<PipeWriter>,
    control: Receiver<Message>,
    backlog: Vec<Message>,
    next_req: u32,
}

impl SideClient {
    fn connect(name: &'static str, session: u32, instance: u16, server_pid: u32) -> Self {
        let pipe = format!("ui-{session}-{instance}");
        let conn =
            until(&format!("the UI pipe {pipe}"), WAIT, || connect(&pipe, server_pid, Duration::from_secs(1)).ok());
        let (reader, writer) = conn.split().expect("split");
        let (mut reader, mut writer) = (FrameReader::new(reader), FrameWriter::new(writer));
        Handshake::client(&mut reader, &mut writer, IpcRole::Ui, std::process::id()).expect("HelloAck");
        let (tx, control) = channel();
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = reader.read_message() {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Self { name, writer, control, backlog: Vec::new(), next_req: 1000 }
    }

    fn call(&mut self, op: UiOp) -> Result<ReplyBody, ApiCode> {
        let op_code = op.code();
        let req = self.next_req;
        self.next_req += 1;
        self.writer
            .write_message(&Message::UiRequest { req, op })
            .unwrap_or_else(|e| panic!("{}: write failed: {e:?}", self.name));
        let end = Instant::now() + WAIT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.control.recv_timeout(left) {
                Ok(Message::UiReply { req: r, status, body }) if r == req => {
                    return match status_code(status).expect("a known status") {
                        Some(code) => Err(code),
                        None => Ok(ReplyBody::decode(op_code, &body.0).expect("the body decodes")),
                    };
                }
                Ok(other) => self.backlog.push(other),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("{}: timed out waiting for the reply to request {req}", self.name)
                }
                Err(RecvTimeoutError::Disconnected) => panic!("{}: the pipe closed", self.name),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------

/// The steps, their one-line results, the screenshots and the failures.
struct Run {
    uia: Uia,
    logs: PathBuf,
    shots: PathBuf,
    started: Instant,
    failures: Vec<String>,
    shot_no: u32,
}

impl Run {
    /// A screenshot of every window of both apps, named after the step.
    fn snap(&mut self, label: &str, pids: &[(&str, u32)]) {
        self.shot_no += 1;
        for (who, pid) in pids {
            let prefix = self.shots.join(format!("{:02}-{label}-{who}", self.shot_no));
            let report = self.uia.shot(*pid, &prefix);
            for line in report.lines() {
                println!("    shot {who}: {line}");
            }
        }
    }

    fn dump(&mut self, label: &str, pid: u32) -> PathBuf {
        let path = self.logs.join(format!("tree-{label}.txt"));
        let text = format!("{}\n\n{}\n", self.uia.windows(pid), self.uia.tree(pid));
        let _ = std::fs::write(&path, text);
        path
    }

    /// Waits until `sel` is found (and enabled when `enabled`), polling every 250 ms.
    fn wait_for(&mut self, label: &str, sel: Sel, enabled: bool, limit: Duration) -> Result<Found, String> {
        let end = Instant::now() + limit;
        loop {
            match self.uia.find(sel) {
                Ok(Some(f)) if f.enabled || !enabled => return Ok(f),
                Ok(_) => {}
                Err(e) => return Err(format!("{label}: the helper failed: {e}")),
            }
            if Instant::now() >= end {
                let tree = self.dump(label, sel.pid);
                return Err(format!(
                    "{label}: no {} \"{}\" within {} s; tree dump {}",
                    sel.kind.unwrap_or("control"),
                    sel.name,
                    limit.as_secs(),
                    tree.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Waits for the control, then invokes it through UI Automation.
    fn click(&mut self, label: &str, sel: Sel) -> Result<String, String> {
        self.wait_for(label, sel, true, WAIT)?;
        self.uia.invoke(sel).map_err(|e| format!("{label}: {e}"))
    }

    /// Prints the step's line and records a failure.
    fn step(&mut self, n: u32, what: &str, result: Result<String, String>) -> bool {
        let t = self.started.elapsed().as_secs_f64();
        match result {
            Ok(text) => {
                println!("step {n} {what}: OK [{t:.1} s] {text}");
                true
            }
            Err(e) => {
                println!("step {n} {what}: FAIL [{t:.1} s] {e}");
                self.failures.push(format!("step {n} {what}: {e}"));
                false
            }
        }
    }

    /// A defect the test works around; the run goes on.
    fn defect(&mut self, text: String) {
        println!("DEFECT: {text}");
        self.failures.push(format!("DEFECT: {text}"));
    }
}

fn app_summary(uia: &mut Uia, pid: u32) -> String {
    uia.windows(pid)
        .lines()
        .map(|l| l.split(" | ").take(2).collect::<Vec<_>>().join(": "))
        .collect::<Vec<_>>()
        .join("; ")
}

#[test]
#[ignore = "interactive: drives real windows; run on an idle PC"]
fn e2e_ui_two_windows_pair_and_open_a_session_with_video() {
    let started = Instant::now();
    let root = std::env::temp_dir().join(format!("swapdesk-e2e-ui-{}", std::process::id()));
    let logs = root.join("logs");
    let shots = root.join("shots");
    std::fs::create_dir_all(&logs).expect("log folder");
    std::fs::create_dir_all(&shots).expect("shot folder");
    println!(
        "folder: {}\n  screenshots: {}\n  logs and tree dumps: {}",
        root.display(),
        shots.display(),
        logs.display()
    );
    let session = active_console_session().expect("a console session");

    let uia = Uia::spawn();
    let mut run = Run { uia, logs: logs.clone(), shots, started, failures: Vec::new(), shot_no: 0 };

    // ---- step 1: two services, two windows ----
    let mut viewer_svc = Service::start(1, &logs);
    let mut host_svc = Service::start(2, &logs);
    for svc in [&mut viewer_svc, &mut host_svc] {
        until(&format!("both helpers of instance {}", svc.instance), WAIT, || {
            assert!(svc.alive(), "instance {} exited early; see {}", svc.instance, svc.out.display());
            let t = svc.trace();
            (t.contains("system helper started") && t.contains("user helper started")).then_some(())
        });
        svc.refresh_helpers();
    }
    // The apps connect once at start: the pipes exist now (the helpers attach after the pipe is served).
    let mut side = SideClient::connect("side-viewer", session, 1, viewer_svc.pid());
    let viewer_app = App::start(1, viewer_svc.pid());
    let host_app = App::start(2, host_svc.pid());
    let (vp, hp) = (viewer_app.pid(), host_app.pid());
    let r = run
        .wait_for("viewer-main", Sel::any(vp, Some(MAIN_WINDOW), "regex", "."), false, WAIT)
        .and_then(|_| run.wait_for("host-main", Sel::any(hp, Some(MAIN_WINDOW), "regex", "."), false, WAIT))
        .map(|_| {
            format!(
                "services pid {}/{}, helpers {:?}/{:?}; viewer app pid {vp} [{}]; host app pid {hp} [{}]",
                viewer_svc.pid(),
                host_svc.pid(),
                viewer_svc.helpers,
                host_svc.helpers,
                app_summary(&mut run.uia, vp),
                app_summary(&mut run.uia, hp)
            )
        });
    let up = run.step(1, "services and windows up", r);
    std::thread::sleep(Duration::from_millis(800));
    run.snap("start", &[("viewer", vp), ("host", hp)]);
    run.dump("start-viewer", vp);
    run.dump("start-host", hp);

    let mut paired = false;
    let mut manual_id = String::new();
    if up {
        // ---- step 2: the host window opens pairing mode ----
        let r = run.click("host-pair-this-computer", Sel::button(hp, MAIN_WINDOW, PAIR_THIS_COMPUTER)).and_then(|c| {
            run.wait_for("host-pairing-open", Sel::button(hp, MAIN_WINDOW, CANCEL), true, WAIT)
                .map(|_| format!("clicked ({c}); the pairing screen shows Cancelar"))
        });
        run.step(2, "host: pairing mode open", r);
        run.snap("host-pairing-open", &[("host", hp)]);

        // ---- step 3: the viewer window adds the host by address ----
        let tree = run.dump("viewer-before-add", vp);
        let tree_text = std::fs::read_to_string(&tree).unwrap_or_default();
        if !tree_text.lines().any(|l| l.trim_start().starts_with("Edit ")) {
            run.defect(format!(
                "no screen has an address field: the viewer's main window has no Edit control ({}). EditId::Address is only a number in crates/swapdesk-shell-win/src/window.rs (edit_code); screens_main.rs places Name and screens_viewer.rs places Chat, nothing places Address. In --dev-loopback no discovery runs, so the window can never learn the host; and UiState::submit_edit(EditId::Address) parses a bare Ipv4Addr, so a typed `127.0.0.1:47840` would be dropped even with a field",
                tree.display()
            ));
        }
        println!("WORKAROUND 1: AddByAddress 127.0.0.1:47840 sent over a side client of the viewer service's pipe");
        match side.call(UiOp::AddByAddress { ipv4: "127.0.0.1:47840".into() }) {
            Ok(ReplyBody::Device(d)) => manual_id = d.id,
            other => run.failures.push(format!("step 3: AddByAddress over the side pipe answered {other:?}")),
        }
        let r = run.wait_for(
            "viewer-device-row",
            Sel { pid: vp, window: Some(MAIN_WINDOW), mode: "contains", name: "127.0.0.1", kind: Some("Button") },
            true,
            WAIT,
        );
        let r = r.map(|f| format!("the sidebar shows the typed device as button \"{}\"", f.name));
        let ok = run.step(3, "viewer: the host is listed", r);
        run.snap("viewer-device-listed", &[("viewer", vp)]);
        run.dump("viewer-after-add", vp);

        // ---- step 4: the viewer window starts the pairing, both windows show the code ----
        if ok && !manual_id.is_empty() {
            // The only device is selected already (a selected control has no Invoke pattern in UI Automation, so it
            // could not be clicked anyway); with another one selected the row would be clicked first.
            let pair_button = Sel::button(vp, MAIN_WINDOW, "Parear com 127.0.0.1");
            let row =
                Sel { pid: vp, window: Some(MAIN_WINDOW), mode: "contains", name: "127.0.0.1", kind: Some("Button") };
            let r = match run.uia.find(pair_button) {
                Ok(Some(_)) => run.click("viewer-pair-with-device", pair_button),
                _ => run
                    .click("viewer-select-device", row)
                    .and_then(|_| run.click("viewer-pair-with-device", pair_button)),
            };
            let ok = run.step(4, "viewer: Parear com 127.0.0.1 clicked", r);
            if ok {
                // The screens never send PairStart: give the viewer window a fair time to show a code by itself.
                let waited = Instant::now();
                let by_itself = loop {
                    let code = run.uia.find(Sel::any(vp, Some(MAIN_WINDOW), "regex", "^Código ")).ok().flatten();
                    if code.is_some() || waited.elapsed() > Duration::from_secs(5) {
                        break code;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                };
                run.snap("viewer-pairing-after-click", &[("viewer", vp)]);
                if by_itself.is_none() {
                    let tree = run.dump("viewer-pairing-stuck", vp);
                    run.defect(format!(
                        "after \"Parear com ...\" the viewer window never starts the ceremony: 5 s later it still shows the Open screen and no code ({}). UiState::activate(PairWithDevice) sends Command::PairOpen only and Reply::PairingId only stores the id; Command::PairStart is built by nobody (grep crates/swapdesk-ui/src: only service_backend/convert.rs mentions it), and the service starts the initiator only on PairStart (ui_hub/pairing.rs pair_start)",
                        tree.display()
                    ));
                    println!(
                        "WORKAROUND 2: PairStart sent over the side client (PairOpen to read the screen's pairing id first)"
                    );
                    let id = match side.call(UiOp::PairOpen { device_id: Some(manual_id.clone()) }) {
                        Ok(ReplyBody::PairingId(id)) => Some(id),
                        other => {
                            run.failures.push(format!("step 4: PairOpen over the side pipe answered {other:?}"));
                            None
                        }
                    };
                    if let Some(id) = id {
                        match side.call(UiOp::PairStart { pairing_id: id, device_id: manual_id.clone() }) {
                            Ok(ReplyBody::Done) => {}
                            other => {
                                run.failures.push(format!("step 4: PairStart over the side pipe answered {other:?}"))
                            }
                        }
                    }
                }
                // ---- step 5: the same code on both windows ----
                let code_sel = |pid| Sel::any(pid, Some(MAIN_WINDOW), "regex", "^Código ");
                let r = run
                    .wait_for("viewer-code", code_sel(vp), false, WAIT)
                    .and_then(|v| run.wait_for("host-code", code_sel(hp), false, WAIT).map(|h| (v, h)));
                let r = r.and_then(|(v, h)| {
                    if v.name == h.name {
                        Ok(format!("both windows show \"{}\"", v.name))
                    } else {
                        Err(format!("the codes differ: viewer \"{}\", host \"{}\"", v.name, h.name))
                    }
                });
                let ok = run.step(5, "both windows show the same code", r);
                run.snap("codes", &[("viewer", vp), ("host", hp)]);

                // ---- step 6: both confirm ----
                if ok {
                    let r = run
                        .click("viewer-codes-match", Sel::button(vp, MAIN_WINDOW, CODES_MATCH))
                        .and_then(|_| run.click("host-codes-match", Sel::button(hp, MAIN_WINDOW, CODES_MATCH)))
                        .and_then(|_| {
                            run.wait_for("viewer-pair-done", Sel::button(vp, MAIN_WINDOW, PAIR_DONE), true, WAIT)
                        })
                        .and_then(|_| {
                            run.wait_for("host-pair-done", Sel::button(hp, MAIN_WINDOW, PAIR_DONE), true, WAIT)
                        })
                        .map(|_| "both confirmed, both windows show Pronto".to_string());
                    paired = run.step(6, "both confirm the code", r);
                    run.snap("paired", &[("viewer", vp), ("host", hp)]);
                    if paired {
                        let r = run
                            .click("viewer-pronto", Sel::button(vp, MAIN_WINDOW, PAIR_DONE))
                            .and_then(|_| run.click("host-pronto", Sel::button(hp, MAIN_WINDOW, PAIR_DONE)));
                        run.step(6, "both windows close the pairing screen (Pronto)", r.map(|_| "clicked".to_string()));
                        // The typed placeholder should be gone now that the device is paired (it is the same machine).
                        std::thread::sleep(Duration::from_millis(500));
                        let stale = Sel {
                            pid: vp,
                            window: Some(MAIN_WINDOW),
                            mode: "contains",
                            name: "127.0.0.1",
                            kind: Some("Button"),
                        };
                        if let Ok(Some(f)) = run.uia.find(stale) {
                            let tree = run.dump("viewer-after-pairing", vp);
                            run.defect(format!(
                                "after the pairing the viewer's sidebar still lists the typed address as an unpaired device (button \"{}\", next to the paired device of the same machine); see {} and shots/*viewer-pairing*. ui_hub/views.rs device_views() only hides a typed address that a discovered peer explains, not one that was paired through",
                                f.name,
                                tree.display()
                            ));
                        }
                    }
                }
            }
        }
    }

    let mut session_open = false;
    let mut viewer_shots = String::new();
    if paired {
        // ---- step 7: the viewer clicks Controlar on the host device ----
        let r = run.click("viewer-controlar", Sel::button(vp, MAIN_WINDOW, CONTROL));
        let ok = run.step(7, "viewer: Controlar clicked", r);

        // ---- step 8: the host window shows the request and accepts ----
        if ok {
            let r = run.wait_for("host-incoming-request", Sel::button(hp, MAIN_WINDOW, ACCEPT), true, WAIT);
            let ok = run.step(
                8,
                "host: the incoming request is shown",
                r.map(|f| format!("button \"{}\" in window \"{}\"", f.name, f.window)),
            );
            run.snap("host-incoming", &[("host", hp)]);
            if ok {
                // The boxes start at the stored grants, which are empty after a pairing, and "Aceitar" with nothing
                // ticked is a refusal (host.rs: "an empty pick is a refusal"): tick "Controlar" (it implies viewing).
                let r = run.click(
                    "host-allow-control",
                    Sel {
                        pid: hp,
                        window: Some(MAIN_WINDOW),
                        mode: "exact",
                        name: ALLOW_CONTROL,
                        kind: Some("CheckBox"),
                    },
                );
                run.step(8, "host: the box Controlar ticked", r);
                println!(
                    "    NOTE: the boxes start at the stored grants (empty after a pairing); a run that clicked Aceitar with nothing ticked was refused at once (trace: \"a request from ... was refused: Declined\", viewer: \"session did not open: Refused(Declined)\")"
                );
                run.dump("host-incoming-ticked", hp);
                run.snap("host-incoming-ticked", &[("host", hp)]);
                let r = run.click("host-accept", Sel::button(hp, MAIN_WINDOW, ACCEPT));
                run.step(8, "host: Aceitar clicked", r);

                // ---- step 9: the viewer window opens ----
                let viewer =
                    Sel { pid: vp, window: Some(VIEWER_WINDOW), mode: "exact", name: DISCONNECT, kind: Some("Button") };
                let r = run.wait_for("viewer-window", viewer, true, Duration::from_secs(30));
                let windows = format!(
                    "viewer app windows: [{}]; host app windows: [{}]",
                    app_summary(&mut run.uia, vp),
                    app_summary(&mut run.uia, hp)
                );
                session_open = run.step(
                    9,
                    "viewer: the viewer window is open",
                    r.map(|f| format!("window \"{}\" has button \"{}\"; {windows}", f.window, f.name)),
                );
            }
        }
    }

    if session_open {
        // ---- step 10: video ----
        // The window is the viewer; wait for frames to reach it, then look at it and at the service's own trace.
        std::thread::sleep(Duration::from_secs(6));
        run.snap("session", &[("viewer", vp), ("host", hp)]);
        viewer_shots = run.uia.shot(vp, &run.shots.join("video-check-viewer"));
        run.dump("session-viewer", vp);
        run.dump("session-host", hp);
        viewer_svc.refresh_helpers();
        let t1 = viewer_svc.trace();
        let t2 = host_svc.trace();
        let evidence: Vec<&str> = t1
            .lines()
            .chain(t2.lines())
            .filter(|l| l.to_ascii_lowercase().contains("video") || l.to_ascii_lowercase().contains("stream"))
            .take(8)
            .collect();
        println!("    service traces mentioning video/stream (first 8): {evidence:#?}");
        let viewer_line = viewer_shots
            .lines()
            .find(|l| l.contains("png") && !l.starts_with("skipped"))
            .map(str::to_string)
            .unwrap_or_default();
        println!("    viewer window shot: {viewer_line}");
        let distinct = viewer_line
            .split("distinct colours ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0);
        let r = if distinct > 30 {
            Ok(format!("the viewer window shows a picture ({distinct} distinct colours in a 40x40 sample)"))
        } else {
            Err(format!("the viewer window looks blank or flat ({distinct} distinct colours)"))
        };
        run.step(10, "viewer: the video shows", r);

        // ---- step 11: Disconnect ----
        let r = run.click("viewer-disconnect", Sel::button(vp, VIEWER_WINDOW, DISCONNECT)).and_then(|c| {
            let end = Instant::now() + WAIT;
            loop {
                if run.uia.find(Sel::any(vp, Some(VIEWER_WINDOW), "regex", ".")).ok().flatten().is_none() {
                    break Ok(format!(
                        "clicked ({c}); the viewer window is gone; viewer app windows: [{}]; host app windows: [{}]",
                        app_summary(&mut run.uia, vp),
                        app_summary(&mut run.uia, hp)
                    ));
                }
                if Instant::now() >= end {
                    break Err("the viewer window is still open 20 s after Desconectar".to_string());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        run.step(11, "viewer: Desconectar", r);
        std::thread::sleep(Duration::from_millis(500));
        run.snap("disconnected", &[("viewer", vp), ("host", hp)]);
    }

    // ---- step 12: close everything ----
    let mut viewer_app = viewer_app;
    let mut host_app = host_app;
    run.uia.close(vp);
    run.uia.close(hp);
    let mut closed_by_window = true;
    for app in [&mut viewer_app, &mut host_app] {
        let end = Instant::now() + Duration::from_secs(10);
        while app.alive() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(100));
        }
        if app.alive() {
            closed_by_window = false;
            let _ = app.child.kill();
            let _ = app.child.wait();
        }
    }
    drop(side);
    viewer_svc.refresh_helpers();
    host_svc.refresh_helpers();
    for svc in [&viewer_svc, &host_svc] {
        std::fs::write(svc.dir.join("stop"), b"stop").expect("stop file");
    }
    let mut codes = Vec::new();
    for svc in [&mut viewer_svc, &mut host_svc] {
        let end = Instant::now() + WAIT;
        let status = loop {
            if let Some(s) = svc.child.try_wait().expect("wait") {
                break Some(s);
            }
            if Instant::now() >= end {
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        svc.refresh_helpers();
        svc.keep_trace(&logs);
        codes.push((svc.instance, status.and_then(|s| s.code())));
    }
    let mut tracked: Vec<u32> = viewer_svc.helpers.iter().chain(host_svc.helpers.iter()).copied().collect();
    tracked.extend([viewer_svc.pid(), host_svc.pid(), vp, hp]);
    let mut lingering = Vec::new();
    for &pid in &tracked {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let image = image_of(pid);
            if !image.as_deref().is_some_and(|n| n.to_ascii_lowercase().starts_with("swapdesk")) {
                break;
            }
            if Instant::now() >= end {
                lingering.push((pid, image.unwrap_or_default()));
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    for (pid, _) in &lingering {
        kill_pid(*pid);
    }
    let r = if lingering.is_empty() && codes.iter().all(|(_, c)| *c == Some(0)) {
        Ok(format!(
            "the apps {} on close; service exit codes {codes:?}; tracked pids {tracked:?}, none left",
            if closed_by_window { "exited" } else { "had to be killed" }
        ))
    } else {
        Err(format!("service exit codes {codes:?}, lingering {lingering:?}"))
    };
    run.step(12, "everything closed", r);
    if !closed_by_window {
        run.failures.push("step 12: an app window did not close through its Window pattern within 10 s".into());
    }
    let _ = viewer_shots;

    println!("e2e_ui took {:.1} s; screenshots: {}", run.started.elapsed().as_secs_f64(), run.shots.display());
    assert!(
        run.failures.is_empty(),
        "the e2e run found {} problem(s):\n  {}",
        run.failures.len(),
        run.failures.join("\n  ")
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Stage 40: the native address input is usable on screen (the real window with the scripted mock backend).

/// Finds the address EDIT (control id 3) of a process by its window handles, then checks and drives it. Win32 only
/// through P/Invoke because this crate forbids `unsafe`; no `SendInput`, no pointer: messages go to the EDIT's HWND.
/// Windows PowerShell 5.1, ASCII only. Arguments: pid, folder for the two PNGs, text to type.
const EDIT_PROBE: &str = r#"
param([int]$ProcId, [string]$OutDir, [string]$Text)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class Sd {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, EnumProc p, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, StringBuilder l);
    // GetWindowText does not read another process's EDIT; WM_GETTEXT is marshalled by the system.
    public static string TextOf(IntPtr h) { var s = new StringBuilder(256); SendMessage(h, 0x000D, (IntPtr)256, s); return s.ToString(); }
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint f);
    public static List<IntPtr> Tops(uint pid) {
        var l = new List<IntPtr>();
        EnumWindows((h, _) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid) l.Add(h); return true; }, IntPtr.Zero);
        return l;
    }
    public static IntPtr FindEdit(uint pid, out IntPtr top) {
        IntPtr found = IntPtr.Zero, owner = IntPtr.Zero;
        foreach (var t in Tops(pid)) {
            IntPtr hit = IntPtr.Zero;
            EnumChildWindows(t, (h, _) => {
                var c = new StringBuilder(64); GetClassName(h, c, 64);
                if (c.ToString() == "Edit" && GetDlgCtrlID(h) == 3) hit = h;
                return true;
            }, IntPtr.Zero);
            if (hit != IntPtr.Zero) { found = hit; owner = t; }
        }
        top = owner;
        return found;
    }
}
'@
[void][Sd]::SetProcessDPIAware()
function Shot([IntPtr]$h, [string]$path) {
    $r = New-Object Sd+RECT
    [void][Sd]::GetWindowRect($h, [ref]$r)
    $w = [int]($r.R - $r.L); $hh = [int]($r.B - $r.T)
    Write-Output ('shot_size=' + $w + 'x' + $hh + ' at ' + $r.L + ',' + $r.T)
    $bmp = New-Object System.Drawing.Bitmap -ArgumentList $w, $hh
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen([int]$r.L, [int]$r.T, 0, 0, (New-Object System.Drawing.Size -ArgumentList $w, $hh))
    $g.Dispose()
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $script:last = $bmp
}
function Colours($a) {
    $set = @{}
    for ($y = 0; $y -lt $a.Height; $y++) { for ($x = 0; $x -lt $a.Width; $x++) { $set[$a.GetPixel($x, $y).ToArgb()] = 1 } }
    $set.Count
}
function PixDiff($a, $b) {
    $n = 0
    for ($y = 0; $y -lt $a.Height; $y++) { for ($x = 0; $x -lt $a.Width; $x++) {
        if ($a.GetPixel($x, $y).ToArgb() -ne $b.GetPixel($x, $y).ToArgb()) { $n++ } } }
    $n
}
$top = [IntPtr]::Zero
$edit = [IntPtr]::Zero
for ($i = 0; $i -lt 80 -and $edit -eq [IntPtr]::Zero; $i++) { $edit = [Sd]::FindEdit([uint32]$ProcId, [ref]$top); if ($edit -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 250 } }
if ($edit -eq [IntPtr]::Zero) { 'found=0'; exit 0 }
'found=1'
'is_window=' + [Sd]::IsWindow($edit)
# Bring the window above the others so the screen pixels are its own.
[void][Sd]::SetWindowPos($top, [IntPtr]::new(-1), 0, 0, 0, 0, 0x0053)
[void][Sd]::SetWindowPos($top, [IntPtr]::new(-2), 0, 0, 0, 0, 0x0053)
Start-Sleep -Milliseconds 600
'visible=' + [Sd]::IsWindowVisible($edit)
$r = New-Object Sd+RECT; [void][Sd]::GetWindowRect($edit, [ref]$r)
$c = New-Object Sd+RECT; [void][Sd]::GetClientRect($top, [ref]$c)
$p = New-Object Sd+POINT; [void][Sd]::ClientToScreen($top, [ref]$p)
'edit_rect=' + ($r.L - $p.X) + ',' + ($r.T - $p.Y) + ',' + ($r.R - $p.X) + ',' + ($r.B - $p.Y)
'client_size=' + $c.R + ',' + $c.B
'text_before=' + [Sd]::TextOf($edit)
Shot $edit (Join-Path $OutDir 'address-edit-before.png')
$before = $script:last
'colours_before=' + (Colours $before)
foreach ($ch in $Text.ToCharArray()) { [void][Sd]::PostMessage($edit, 0x0102, [IntPtr][int]$ch, [IntPtr]1); Start-Sleep -Milliseconds 20 }
Start-Sleep -Milliseconds 300
'text_after=' + [Sd]::TextOf($edit)
Shot $edit (Join-Path $OutDir 'address-edit-after.png')
$after = $script:last
'colours_after=' + (Colours $after)
'pixels_changed=' + (PixDiff $before $after) + ' of ' + ($before.Width * $before.Height)
'is_window_after=' + [Sd]::IsWindow($edit)
# Enter, as the edit's own key handler takes it (WM_KEYDOWN VK_RETURN): the commit does not wait for the 150 ms.
[void][Sd]::PostMessage($edit, 0x0100, [IntPtr]0x0D, [IntPtr]1)
'done=1'
"#;

#[test]
#[ignore = "interactive: drives a real window and reads its pixels from the screen; run on an idle PC"]
fn e2e_ui_address_input_is_visible_takes_typing_and_submits() {
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("test-results");
    std::fs::create_dir_all(&out).expect("test-results folder");
    let out =
        PathBuf::from(out.canonicalize().expect("test-results path").to_string_lossy().trim_start_matches(r"\\?\"));
    let script = std::env::temp_dir().join(format!("swapdesk-edit-probe-{}.ps1", std::process::id()));
    // A UTF-8 BOM keeps Windows PowerShell 5.1 honest even though the script is ASCII.
    std::fs::write(&script, format!("\u{feff}{EDIT_PROBE}")).expect("probe script");

    let mut uia = Uia::spawn();
    let child = Command::new(env!("CARGO_BIN_EXE_Swapdesk"))
        .args(["--mock", "dispositivos"])
        .env_remove("SWAPDESK_LANG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Swapdesk.exe --mock starts");
    let app = App { child };
    let pid = app.pid();
    let probe = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .args(["-ProcId", &pid.to_string(), "-OutDir"])
        .arg(&out)
        .args(["-Text", "192.168.0.21"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("powershell runs the probe");
    let _ = std::fs::remove_file(&script);
    let text = String::from_utf8_lossy(&probe.stdout).into_owned();
    println!("probe output:\n{text}\nprobe stderr: {}", String::from_utf8_lossy(&probe.stderr));
    let get = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(&format!("{k}="))).map(str::to_string);
    assert_eq!(get("found").as_deref(), Some("1"), "no EDIT with control id 3 in the window of pid {pid}");
    assert_eq!(get("is_window").as_deref(), Some("True"));
    assert_eq!(get("is_window_after").as_deref(), Some("True"), "the EDIT was destroyed while typing");
    assert_eq!(get("visible").as_deref(), Some("True"), "the EDIT is not visible");
    let rect: Vec<i32> = get("edit_rect").expect("edit_rect").split(',').map(|n| n.parse().expect("number")).collect();
    let client: Vec<i32> =
        get("client_size").expect("client_size").split(',').map(|n| n.parse().expect("number")).collect();
    assert!(rect[2] > rect[0] && rect[3] > rect[1], "empty EDIT rect {rect:?}");
    assert!(
        rect[0] >= 0 && rect[1] >= 0 && rect[2] <= client[0] && rect[3] <= client[1],
        "EDIT rect {rect:?} is outside the client area {client:?}"
    );
    // EM_GETCUEBANNER cannot be asked from another process (the system does not marshal its buffer), so the cue
    // banner is proven by pixels: the empty field already shows glyphs (more than a flat fill plus a border).
    let colours_before: u32 = get("colours_before").expect("colours_before").parse().expect("number");
    assert!(colours_before >= 4, "the empty field shows no cue banner text ({colours_before} colours)");
    assert_eq!(get("text_before").as_deref(), Some(""));
    assert_eq!(get("text_after").as_deref(), Some("192.168.0.21"), "typed characters did not reach the EDIT");
    let changed: u64 =
        get("pixels_changed").expect("pixels_changed").split(' ').next().and_then(|n| n.parse().ok()).expect("number");
    assert!(changed > 20, "the field's pixels did not change after typing ({changed}); see {}", out.display());
    assert_eq!(get("done").as_deref(), Some("1"));

    // Enter and the 150 ms debounce reach the UI thread: it keeps answering (UIA still sees the window and the field).
    // That the UI turns the text into `Command::AddByAddress` is proven by the unit test
    // `r_ui_4_the_address_field_acts_150_ms_after_the_last_key`; the scripted mock answers it with `Done` and shows
    // nothing, so no window effect can be read from outside.
    std::thread::sleep(Duration::from_millis(600));
    let tree = uia.tree(pid);
    assert!(
        tree.contains("Window | Swapdesk"),
        "the window stopped answering after Enter; tree:
{tree}"
    );
    assert!(
        tree.contains("192.168.0.21"),
        "the field lost its text after Enter; tree:
{tree}"
    );
}
