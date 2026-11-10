//! The M3 gate (plan stage 18, milestone M3): "a Windows-to-Windows loopback session through the service, with control
//! and audio". Two real `swapdesk-service.exe --console --dev-loopback` processes (instances 1 and 2) on this PC, each
//! with its real helpers (DDA capture, hardware H.264 encoder, SendInput, WASAPI loopback), and two UI clients over the
//! real UI pipes `ui-<session>-1` and `-2`:
//!
//! 1. both services start and their helpers attach;
//! 2. the two UIs pair the instances (typed loopback address, same code on both screens, both confirm);
//! 3. instance 1 (viewer) connects with intent Control, the UI of instance 2 (host) accepts VIEW | CONTROL | AUDIO;
//! 4. video: about 5 s of `ViewerVideo` decoded with the Media Foundation decoder;
//! 5. control: one pointer move to 25% / 25% of the shown display and back, read with `GetCursorPos`;
//! 6. audio: `SetAudio` on, `ViewerAudio` datagrams arrive and parse (and the Opus packets decode);
//! 7. Disconnect ends both sessions, both services stop with their stop files and no process of this run remains.
//!
//! The crate forbids `unsafe`, so the pointer is read and (only if the check fails) put back by a small PowerShell
//! helper process of this test. Run it on an idle PC, it moves the pointer:
//! `cargo nextest run -p swapdesk-service --run-ignored only -E 'test(gate_m3)' --no-capture`.
#![cfg(windows)]

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use swapdesk_audio::{FrameDecoder, parse_datagram};
use swapdesk_codec_win::{DecoderOptions, MfH264Decoder};
use swapdesk_ipc::{
    ApiCode, Decision, Events, FrameReader, FrameWriter, Handshake, IpcRole, Message, Notify, PairingState, ReplyBody,
    RequestKind, Role, SessionState, UiOp, status_code,
};
use swapdesk_platform::{DisplayId, Pixels};
use swapdesk_proto::{InputEvent, Intent, PermissionSet, VideoHeader};
use swapdesk_sys_win::{PipeWriter, active_console_session, connect};

const WAIT: Duration = Duration::from_secs(20);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `UiRequest.op` of `Connect`.
const CONNECT_OP: u16 = 11;

/// Records a failed check of a step in `$failures` instead of stopping the run.
macro_rules! soft {
    ($failures:expr, $cond:expr, $($msg:tt)+) => {
        if !$cond {
            $failures.push(format!($($msg)+));
        }
    };
}

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

/// Deletes what an earlier run left in a dev folder, file by file (no recursive removal of anything but our own tree).
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

/// The image name of a live process, by PID.
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

/// One real `swapdesk-service --console` process. Dropping it kills what a failed run left (the service and the
/// helpers it started, by PID).
struct Service {
    instance: u16,
    child: Child,
    dir: PathBuf,
    out: PathBuf,
    helpers: Vec<u32>,
    stopped: bool,
}

impl Service {
    fn start(instance: u16, logs: &Path) -> Self {
        let dir = dev_dir(instance);
        std::fs::create_dir_all(&dir).expect("dev folder");
        clear_dir(&dir);
        let out = logs.join(format!("service-{instance}.out"));
        let file = File::create(&out).expect("service log file");
        let err = file.try_clone().expect("clone log handle");
        let child = Command::new(env!("CARGO_BIN_EXE_swapdesk-service"))
            .args(["--console", "--instance", &instance.to_string(), "--dev-loopback"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("the service starts");
        Self { instance, child, dir, out, helpers: Vec::new(), stopped: false }
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

    /// Keeps the service's trace next to its stdout log.
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
            eprintln!("---- trace of instance {} (last 80 of {} lines) ----", self.instance, lines.len());
            for l in &lines[lines.len().saturating_sub(80)..] {
                eprintln!("{l}");
            }
            if let Ok(o) = std::fs::read_to_string(&self.out) {
                eprintln!("---- stdout/stderr of instance {} ----\n{o}", self.instance);
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

/// A UI client on a real UI pipe: a reader thread sorts what arrives into the control messages, the video and the audio.
struct Client {
    name: &'static str,
    writer: FrameWriter<PipeWriter>,
    control: Receiver<Message>,
    video: Receiver<(Instant, u64, VideoHeader, Vec<u8>)>,
    audio: Receiver<(Instant, u64, Vec<u8>)>,
    backlog: Vec<Message>,
    next_req: u32,
}

impl Client {
    fn connect(name: &'static str, session: u32, instance: u16, server_pid: u32) -> Self {
        let pipe = format!("ui-{session}-{instance}");
        let conn =
            until(&format!("the UI pipe {pipe}"), WAIT, || connect(&pipe, server_pid, Duration::from_secs(1)).ok());
        let (reader, writer) = conn.split().expect("split");
        let (mut reader, mut writer) = (FrameReader::new(reader), FrameWriter::new(writer));
        let ack = Handshake::client(&mut reader, &mut writer, IpcRole::Ui, std::process::id()).expect("HelloAck");
        assert_eq!(ack.win_session, session, "HelloAck names the pipe's session");
        let (ctl_tx, control) = channel();
        let (vid_tx, video) = channel();
        let (aud_tx, audio) = channel();
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = reader.read_message() {
                let at = Instant::now();
                let sent = match msg {
                    Message::ViewerVideo { session_id, header, payload } => {
                        vid_tx.send((at, session_id, header, payload.0.into())).is_ok()
                    }
                    Message::ViewerAudio { session_id, datagram } => aud_tx.send((at, session_id, datagram.0)).is_ok(),
                    other => ctl_tx.send(other).is_ok(),
                };
                if !sent {
                    break;
                }
            }
        });
        Self { name, writer, control, video, audio, backlog: Vec::new(), next_req: 1 }
    }

    fn send(&mut self, msg: &Message) {
        self.writer.write_message(msg).unwrap_or_else(|e| panic!("{}: write failed: {e:?}", self.name));
    }

    fn request(&mut self, op: UiOp) -> u32 {
        let req = self.next_req;
        self.next_req += 1;
        self.send(&Message::UiRequest { req, op });
        req
    }

    /// The first control message `f` picks, from what was set aside or what arrives within `limit`.
    fn take<T>(&mut self, what: &str, limit: Duration, mut f: impl FnMut(&Message) -> Option<T>) -> T {
        if let Some(i) = self.backlog.iter().position(|m| f(m).is_some()) {
            let m = self.backlog.remove(i);
            return f(&m).expect("picked");
        }
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.control.recv_timeout(left) {
                Ok(m) => match f(&m) {
                    Some(v) => return v,
                    None => self.backlog.push(m),
                },
                Err(RecvTimeoutError::Timeout) => panic!("{}: timed out waiting for {what}", self.name),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("{}: the pipe closed while waiting for {what}", self.name)
                }
            }
        }
    }

    fn reply(&mut self, req: u32, op: u16, limit: Duration) -> Result<ReplyBody, ApiCode> {
        let (status, body) = self.take("a UiReply", limit, |m| match m {
            Message::UiReply { req: r, status, body } if *r == req => Some((*status, body.0.clone())),
            _ => None,
        });
        match status_code(status).expect("a known status") {
            Some(code) => Err(code),
            None => Ok(ReplyBody::decode(op, &body).expect("the body decodes")),
        }
    }

    fn call(&mut self, op: UiOp) -> Result<ReplyBody, ApiCode> {
        let code = op.code();
        let req = self.request(op);
        self.reply(req, code, WAIT)
    }

    fn notify<T>(&mut self, what: &str, mut f: impl FnMut(&Notify) -> Option<T>) -> T {
        self.take(what, WAIT, |m| match m {
            Message::UiNotify(n) => f(n),
            _ => None,
        })
    }

    fn pairing(&mut self, id: u32, what: &str, mut f: impl FnMut(&PairingState) -> bool) -> PairingState {
        self.notify(what, |n| match n {
            Notify::Pairing { pairing_id, state } if *pairing_id == id && f(state) => Some(state.clone()),
            _ => None,
        })
    }

    fn drop_video(&self) {
        while self.video.try_recv().is_ok() {}
    }
}

/// The pointer, read and put back by a PowerShell process (this crate cannot call Win32 itself).
struct Pointer {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

const POINTER_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
[StructLayout(LayoutKind.Sequential)] public struct SdPt { public int X; public int Y; }
public static class SdNative {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out SdPt p);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
}
'@
[void][SdNative]::SetProcessDpiAwarenessContext([IntPtr](-4))
[Console]::Out.WriteLine('ready'); [Console]::Out.Flush()
while ($true) {
    $line = [Console]::In.ReadLine()
    if ($null -eq $line -or $line -eq 'quit') { break }
    $parts = $line.Split(' ')
    if ($parts[0] -eq 'set') {
        [void][SdNative]::SetCursorPos([int]$parts[1], [int]$parts[2])
    }
    $p = New-Object SdPt
    [void][SdNative]::GetCursorPos([ref]$p)
    [Console]::Out.WriteLine("$($p.X) $($p.Y)"); [Console]::Out.Flush()
}
"#;

impl Pointer {
    fn spawn(logs: &Path) -> Self {
        let script = logs.join("pointer.ps1");
        std::fs::write(&script, POINTER_SCRIPT).expect("pointer script");
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
        Self { child, stdin, lines }
    }

    fn ready(&mut self) {
        let l = self.lines.recv_timeout(Duration::from_secs(60)).expect("the pointer helper says ready");
        assert_eq!(l.trim(), "ready");
    }

    fn ask(&mut self, cmd: &str) -> (i32, i32) {
        writeln!(self.stdin, "{cmd}").expect("write to the pointer helper");
        self.stdin.flush().expect("flush");
        let l = self.lines.recv_timeout(Duration::from_secs(10)).expect("the pointer helper answers");
        let mut it = l.split_whitespace().map(|n| n.parse::<i32>().expect("a number"));
        (it.next().expect("x"), it.next().expect("y"))
    }

    fn pos(&mut self) -> (i32, i32) {
        self.ask("pos")
    }

    fn set(&mut self, x: i32, y: i32) -> (i32, i32) {
        self.ask(&format!("set {x} {y}"))
    }
}

impl Drop for Pointer {
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

/// 25%: `round(v * (size - 1) / 65535) + origin` of PROTOCOL.md 7.3, inverted and forward.
fn pixel_of(v: u16, origin: i32, size: u32) -> i32 {
    let last = i64::from(size) - 1;
    i32::try_from((i64::from(v) * last + 32767) / 65535 + i64::from(origin)).expect("fits")
}

fn value_of(pixel: i32, origin: i32, size: u32) -> u16 {
    let last = i64::from(size) - 1;
    let off = (i64::from(pixel) - i64::from(origin)).clamp(0, last);
    u16::try_from((off * 65535 + last / 2) / last.max(1)).expect("fits")
}

#[test]
#[ignore = "interactive: moves the pointer; run on an idle PC"]
fn gate_m3_a_loopback_session_through_the_service_with_control_and_audio() {
    let started = Instant::now();
    let logs = std::env::temp_dir().join(format!("swapdesk-gate-m3-{}", std::process::id()));
    std::fs::create_dir_all(&logs).expect("log folder");
    println!("logs: {}", logs.display());
    let session = active_console_session().expect("a console session");
    // The host is instance 2. `SWAPDESK_GATE_HOST_INSTANCE=0` runs it as instance 0 (the default ports 47820/47821): a
    // diagnostic switch for the hub's address lookup after a pairing through a typed address, which dials the default
    // session port (see the report of 18f); the gate itself runs with instance 2.
    let host_instance: u16 =
        std::env::var("SWAPDESK_GATE_HOST_INSTANCE").ok().and_then(|v| v.parse().ok()).unwrap_or(2);

    // ---- step 1: two real services, their helpers, the pointer helper ----
    let mut pointer = Pointer::spawn(&logs);
    let mut viewer_svc = Service::start(1, &logs);
    let mut host_svc = Service::start(host_instance, &logs);
    for svc in [&mut viewer_svc, &mut host_svc] {
        until(&format!("both helpers of instance {}", svc.instance), WAIT, || {
            assert!(svc.alive(), "instance {} exited early; see {}", svc.instance, svc.out.display());
            let t = svc.trace();
            (t.contains("system helper started") && t.contains("user helper started")).then_some(())
        });
        svc.refresh_helpers();
    }
    println!(
        "step 1 services: viewer pid {} helpers {:?}, host pid {} helpers {:?}, {} ms",
        viewer_svc.pid(),
        viewer_svc.helpers,
        host_svc.pid(),
        host_svc.helpers,
        started.elapsed().as_millis()
    );
    let mut ui1 = Client::connect("viewer-ui", session, 1, viewer_svc.pid());
    let mut ui2 = Client::connect("host-ui", session, host_instance, host_svc.pid());

    // ---- step 2: pairing ----
    let t = Instant::now();
    let host_port = 47820 + 10 * host_instance;
    let ReplyBody::Device(typed) =
        ui1.call(UiOp::AddByAddress { ipv4: format!("127.0.0.1:{host_port}") }).expect("AddByAddress")
    else {
        panic!("not Device")
    };
    let ReplyBody::PairingId(id2) = ui2.call(UiOp::PairOpen { device_id: None }).expect("PairOpen host") else {
        panic!("not PairingId")
    };
    let ReplyBody::PairingId(id1) =
        ui1.call(UiOp::PairOpen { device_id: Some(typed.id.clone()) }).expect("PairOpen viewer")
    else {
        panic!("not PairingId")
    };
    assert!(matches!(
        ui1.call(UiOp::PairStart { pairing_id: id1, device_id: typed.id.clone() }).expect("PairStart"),
        ReplyBody::Done
    ));
    let code_of = |ui: &mut Client, id| match ui.pairing(id, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. }))
    {
        PairingState::ShowCode { code, peer_name } => (code.0, peer_name),
        _ => unreachable!(),
    };
    let (code1, peer1) = code_of(&mut ui1, id1);
    let (code2, peer2) = code_of(&mut ui2, id2);
    assert_eq!(code1, code2, "the same code on both screens");
    assert!(ui1.call(UiOp::PairConfirm { pairing_id: id1, matches: true }).is_ok());
    assert!(ui2.call(UiOp::PairConfirm { pairing_id: id2, matches: true }).is_ok());
    let PairingState::Done { device_id: host_hex } =
        ui1.pairing(id1, "Done", |s| matches!(s, PairingState::Done { .. }))
    else {
        unreachable!()
    };
    let PairingState::Done { device_id: viewer_hex } =
        ui2.pairing(id2, "Done", |s| matches!(s, PairingState::Done { .. }))
    else {
        unreachable!()
    };
    println!(
        "step 2 pairing: code {code1} ({peer1:?} / {peer2:?}), host {} viewer {}, {} ms",
        &host_hex[..8],
        &viewer_hex[..8],
        t.elapsed().as_millis()
    );

    // ---- step 3: connect, the host's UI answers ----
    pointer.ready();
    let accepted = PermissionSet::VIEW.union(PermissionSet::CONTROL).union(PermissionSet::AUDIO);
    let connect_req =
        ui1.request(UiOp::Connect { device_id: host_hex.clone(), intent: Intent::Control, unattended: false });
    let incoming = ui2.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    assert_eq!(incoming.kind, RequestKind::Session { intent: Intent::Control });
    assert_eq!(incoming.device.id, viewer_hex);
    let accepted_at = Instant::now();
    assert!(
        ui2.call(UiOp::AnswerIncoming {
            request_id: incoming.request_id,
            decision: Decision::Accept,
            grants: accepted.bits()
        })
        .is_ok()
    );
    // The reply is the id of the connect's entry; the session has its own id, the one of its first `Active`.
    let ReplyBody::SessionId(entry) = ui1.reply(connect_req, CONNECT_OP, WAIT).expect("the connect is taken up") else {
        panic!("not SessionId")
    };
    assert_ne!(entry, 0);
    let active = |state: &SessionState| match state {
        SessionState::Active { role, granted, .. } => Some((*role, *granted)),
        _ => None,
    };
    let (session_id, role, granted) = ui1.notify("Active on the viewer", |n| match n {
        Notify::Session { session_id: id, state, .. } if *id != entry => active(state).map(|(r, g)| (*id, r, g)),
        _ => None,
    });
    assert_ne!(session_id, 0);
    assert_eq!((role, granted), (Role::Viewer, accepted.bits()));
    let (role, granted) = ui2.notify("Active on the host", |n| match n {
        Notify::Session { session_id: id, state, .. } if *id == session_id => active(state),
        _ => None,
    });
    assert_eq!((role, granted), (Role::Host, accepted.bits()));
    println!(
        "step 3 session: id {session_id}, Active on both with grants {:#06x}, {} ms after Accept",
        accepted.bits(),
        accepted_at.elapsed().as_millis()
    );

    // From here on a failed check is recorded and the next step still runs, so one run shows everything that is wrong.
    let mut failures: Vec<String> = Vec::new();
    let displays = ui1.notify("Displays", |n| match n {
        Notify::Displays { session_id: id, list } if *id == session_id => Some(list.clone()),
        _ => None,
    });
    let shown = displays.iter().find(|d| d.primary).unwrap_or(&displays[0]).clone();
    let display_id = shown.id;
    let bounds = swapdesk_capture_win::displays()
        .expect("the host's displays")
        .into_iter()
        .find(|d| d.id == DisplayId(display_id))
        .expect("the shown display")
        .bounds;
    soft!(
        failures,
        (shown.width, shown.height) == (bounds.width, bounds.height),
        "step 4: the display list says {}x{}, the machine says {}x{}",
        shown.width,
        shown.height,
        bounds.width,
        bounds.height
    );

    println!(
        "displays: {:?}; shown: id {display_id} {}x{} at {},{}",
        swapdesk_capture_win::displays()
            .expect("displays")
            .iter()
            .map(|d| (d.id.0, d.bounds.x, d.bounds.y, d.bounds.width, d.bounds.height, d.primary))
            .collect::<Vec<_>>(),
        bounds.width,
        bounds.height,
        bounds.x,
        bounds.y
    );

    // ---- step 4: video ----
    match ui1.video.recv_timeout(Duration::from_secs(8)) {
        Err(_) => {
            failures.push("step 4: no ViewerVideo within 8 s of the session being Active".into());
            println!("step 4 video: NO FRAME within 8 s");
        }
        Ok((first_at, sid, first_header, first_payload)) => {
            soft!(failures, sid == session_id, "step 4: a frame of session {sid}");
            let first_frame_ms = first_at.saturating_duration_since(accepted_at).as_millis();
            soft!(failures, first_header.keyframe, "step 4: the first frame is not a keyframe");
            let mut frames = vec![(first_at, first_header, first_payload)];
            let window_end = first_at + Duration::from_secs(5);
            while let Some(left) = window_end.checked_duration_since(Instant::now()) {
                match ui1.video.recv_timeout(left) {
                    Ok((at, _, h, p)) => frames.push((at, h, p)),
                    Err(_) => break,
                }
            }
            let last_at = frames.last().expect("frames").0;
            let span = last_at.saturating_duration_since(first_at).as_secs_f64().max(0.001);
            let received = frames.len();
            let fps = (received as f64 - 1.0) / span;
            soft!(
                failures,
                first_header.display_id == display_id,
                "step 4: frames of display {}, expected {display_id}",
                first_header.display_id
            );
            soft!(
                failures,
                (u32::from(first_header.width), u32::from(first_header.height)) == (bounds.width, bounds.height),
                "step 4: the stream is {}x{}, the display {}x{}",
                first_header.width,
                first_header.height,
                bounds.width,
                bounds.height
            );
            soft!(
                failures,
                frames.windows(2).all(|w| w[1].1.frame_id > w[0].1.frame_id),
                "step 4: frame ids do not increase"
            );
            let keyframes = frames.iter().filter(|f| f.1.keyframe).count();
            let bytes: usize = frames.iter().map(|f| f.2.len()).sum();
            let mut decoder = MfH264Decoder::new(DecoderOptions::default());
            let t = Instant::now();
            let (mut decoded, mut wrong_size, mut errors) = (0usize, 0usize, 0usize);
            for (_, header, payload) in &frames {
                match decoder.decode_settled(header, payload) {
                    Ok(Some(f)) => {
                        decoded += 1;
                        let Pixels::Cpu { stride, data } = &f.pixels else { panic!("CPU output expected") };
                        if (f.width, f.height) != (bounds.width, bounds.height)
                            || *stride < f.width * 4
                            || data.len() < (*stride * f.height) as usize
                        {
                            wrong_size += 1;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        errors += 1;
                        eprintln!("decode error: {e:?}");
                    }
                }
            }
            println!(
                "step 4 video: {received} frames received in {span:.2} s = {fps:.1} fps ({keyframes} keyframes, {} KiB), first frame {first_frame_ms} ms after Accept, decoded {decoded} ({errors} errors, {wrong_size} wrong size) in {} ms, {}x{} display {display_id}, decoder {:?}",
                bytes / 1024,
                t.elapsed().as_millis(),
                bounds.width,
                bounds.height,
                decoder.mft_name()
            );
            // DDA delivers a frame only when the screen changes, so the rate here follows what the desktop shows;
            // throughput is measured with the synthetic sources (project rule; stage 15 B2), not by this gate.
            soft!(
                failures,
                received >= 10,
                "step 4: {received} frames in about 5 s ({fps:.1} fps), expected at least 10"
            );
            soft!(failures, errors == 0, "step 4: {errors} decode errors");
            soft!(failures, wrong_size == 0, "step 4: {wrong_size} decoded pictures of the wrong size");
            soft!(failures, decoded == received, "step 4: {decoded} of {received} frames decoded");
        }
    }
    ui1.drop_video();

    // ---- step 5: control (two pointer moves) ----
    let original = pointer.pos();
    let (w, h) = (bounds.width, bounds.height);
    let target = (pixel_of(16384, bounds.x, w), pixel_of(16384, bounds.y, h));
    let near = |a: (i32, i32), b: (i32, i32)| (a.0 - b.0).abs() <= 2 && (a.1 - b.1).abs() <= 2;
    let moved = |ui: &mut Client, pointer: &mut Pointer, display: u16, v: (u16, u16), want: (i32, i32)| {
        ui.send(&Message::ViewerInput {
            session_id,
            events: Events(vec![InputEvent::Pointer { display, x: v.0, y: v.1 }]),
        });
        let t = Instant::now();
        let end = t + Duration::from_secs(2);
        let mut trail: Vec<(i32, i32)> = Vec::new();
        loop {
            let now = pointer.pos();
            if trail.last() != Some(&now) {
                trail.push(now);
            }
            if near(now, want) {
                return Ok((now, t.elapsed().as_millis()));
            }
            if Instant::now() >= end {
                return Err(trail);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let there = moved(&mut ui1, &mut pointer, display_id, (16384, 16384), target);
    // The display that holds the starting position (any display of the host takes a pointer event).
    let home = swapdesk_capture_win::displays().expect("displays").into_iter().find(|d| {
        let b = d.bounds;
        original.0 >= b.x
            && original.0 < b.x + b.width as i32
            && original.1 >= b.y
            && original.1 < b.y + b.height as i32
    });
    let inside = home.is_some();
    let back = match &home {
        Some(d) => moved(
            &mut ui1,
            &mut pointer,
            d.id.0,
            (value_of(original.0, d.bounds.x, d.bounds.width), value_of(original.1, d.bounds.y, d.bounds.height)),
            original,
        ),
        // No display holds it (a gap of the virtual desktop): only the helper can put it back.
        None => Ok((pointer.set(original.0, original.1), 0)),
    };
    let mut restored_by_helper = false;
    if back.is_err() || there.is_err() {
        // Whatever happened, the pointer ends where it was.
        pointer.set(original.0, original.1);
        restored_by_helper = true;
    }
    println!(
        "step 5 control: pointer {original:?} -> expected {target:?} (25%/25% of {w}x{h} at {},{}): {there:?}; back to {original:?}: {back:?}{}{}",
        bounds.x,
        bounds.y,
        if inside { "" } else { " (restored by the helper: no display holds the start)" },
        if restored_by_helper { " (put back by the helper after a failed move)" } else { "" }
    );
    soft!(
        failures,
        there.is_ok(),
        "step 5: the pointer did not reach {target:?} within 2 px in 2 s (last seen {:?}, started at {original:?})",
        there.as_ref().err()
    );
    soft!(
        failures,
        back.is_ok(),
        "step 5: the pointer did not come back to {original:?} within 2 px in 2 s (last seen {:?})",
        back.as_ref().err()
    );

    // ---- step 6: audio ----
    while ui1.audio.try_recv().is_ok() {}
    ui1.drop_video();
    soft!(failures, ui1.call(UiOp::SetAudio { session_id, on: true }).is_ok(), "step 6: SetAudio was refused");
    let end = Instant::now() + Duration::from_secs(2);
    let mut datagrams = Vec::new();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match ui1.audio.recv_timeout(left) {
            Ok((_, sid, d)) => {
                soft!(failures, sid == session_id, "step 6: audio of session {sid}");
                datagrams.push(d);
            }
            Err(_) => break,
        }
    }
    let mut opus = FrameDecoder::new().expect("an Opus decoder");
    let (mut muted, mut parse_errors, mut decode_errors, mut decoded_audio, mut peak) =
        (0usize, 0usize, 0usize, 0usize, 0f32);
    let mut seqs = Vec::new();
    let mut pcm = vec![0f32; 5760 * 2];
    for d in &datagrams {
        match parse_datagram(d) {
            Ok((header, payload)) => {
                seqs.push(header.seq);
                if header.muted {
                    muted += 1;
                } else {
                    match opus.decode(payload, &mut pcm) {
                        Ok(n) => {
                            decoded_audio += 1;
                            peak = pcm[..n.min(pcm.len())].iter().fold(peak, |p, s| p.max(s.abs()));
                        }
                        Err(e) => {
                            decode_errors += 1;
                            eprintln!("opus decode error: {e:?}");
                        }
                    }
                }
            }
            Err(e) => {
                parse_errors += 1;
                eprintln!("audio parse error: {e}");
            }
        }
    }
    let ordered = seqs.windows(2).all(|w| w[1] > w[0]);
    println!(
        "step 6 audio: {} datagrams in 2 s ({muted} muted, {decoded_audio} Opus packets decoded, peak {peak:.4}, {parse_errors} parse errors, {decode_errors} decode errors, seq increasing: {ordered})",
        datagrams.len()
    );
    soft!(failures, datagrams.len() >= 50, "step 6: {} audio datagrams in 2 s, expected at least 50", datagrams.len());
    soft!(failures, parse_errors == 0, "step 6: {parse_errors} datagrams did not parse");
    soft!(failures, decode_errors == 0, "step 6: {decode_errors} Opus packets did not decode");
    soft!(failures, ordered, "step 6: audio sequence numbers do not increase");

    // ---- step 7: disconnect, stop, nothing left ----
    soft!(failures, ui1.call(UiOp::Disconnect { session_id }).is_ok(), "step 7: Disconnect was refused");
    let ended = |n: &Notify| match n {
        Notify::Session { session_id: id, state: SessionState::Ended { reason }, .. } if *id == session_id => {
            Some(*reason)
        }
        _ => None,
    };
    let r1 = ui1.notify("Ended on the viewer", ended);
    let r2 = ui2.notify("Ended on the host", ended);
    soft!(
        failures,
        (r1, r2) == (None, None),
        "step 7: end reasons {r1:?} (viewer) and {r2:?} (host), expected a normal end on both"
    );
    drop((ui1, ui2));
    viewer_svc.refresh_helpers();
    host_svc.refresh_helpers();
    for svc in [&viewer_svc, &host_svc] {
        std::fs::write(svc.dir.join("stop"), b"stop").expect("stop file");
    }
    let mut codes = Vec::new();
    for svc in [&mut viewer_svc, &mut host_svc] {
        let status = until(&format!("instance {} to stop", svc.instance), WAIT, || svc.child.try_wait().expect("wait"));
        svc.stopped = true;
        svc.refresh_helpers();
        svc.keep_trace(&logs);
        soft!(
            failures,
            svc.trace().contains("stopped"),
            "step 7: the trace of instance {} does not say it stopped",
            svc.instance
        );
        codes.push((svc.instance, status.code()));
    }
    let mut tracked: Vec<u32> = viewer_svc.helpers.iter().chain(host_svc.helpers.iter()).copied().collect();
    tracked.extend([viewer_svc.pid(), host_svc.pid()]);
    let mut lingering = Vec::new();
    for &pid in &tracked {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let image = image_of(pid);
            if !image.as_deref().is_some_and(|n| n.to_ascii_lowercase().starts_with("swapdesk-")) {
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
    println!(
        "step 7 end: Ended {{ reason: None }} on both, exit codes {codes:?}, tracked pids {tracked:?}, lingering {lingering:?}, logs {}",
        logs.display()
    );
    soft!(failures, codes.iter().all(|(_, c)| *c == Some(0)), "step 7: exit codes {codes:?}, expected 0 from both");
    soft!(failures, lingering.is_empty(), "step 7: processes of this run were still alive: {lingering:?}");
    println!("gate_m3 took {:.1} s", started.elapsed().as_secs_f64());
    assert!(failures.is_empty(), "the gate failed:\n  {}", failures.join("\n  "));
}
