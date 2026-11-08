//! Smoke test of the real process (slice 18d-2): starts `swapdesk-service --console --instance 1 --dev-loopback`,
//! waits for its UI pipe `\\.\pipe\swapdesk\ui-<session>-1`, does the client handshake over
//! `swapdesk_sys_win::pipe::connect` (which also checks the server PID), asks `GetThisDevice`, looks for both helpers
//! in the trace, then stops it with the `stop` file of its dev folder and checks it ends with exit code 0.
//!
//! On demand only: it starts a real service process and the two helpers in `%LOCALAPPDATA%\Swapdesk-dev\1`. Build the
//! helper first (`cargo build -p swapdesk-helper`: the service starts the one beside its own exe), then
//! `cargo nextest run -p swapdesk-service --run-ignored only`.
#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use swapdesk_ipc::{FrameReader, FrameWriter, Handshake, IpcRole, Message, ReplyBody, UiOp};
use swapdesk_sys_win::{active_console_session, connect};

const WAIT: Duration = Duration::from_secs(20);

fn dev_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("%LOCALAPPDATA%")).join("Swapdesk-dev").join("1")
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stops the child by its stop file, else by its PID (the one this test started).
struct Service(Child);

impl Drop for Service {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
    }
}

#[test]
#[ignore = "starts a real swapdesk-service --console process and its helpers in %LOCALAPPDATA%\\Swapdesk-dev\\1: run on demand"]
fn the_console_service_answers_a_ui_client_on_its_pipe_and_stops_cleanly() {
    let dir = dev_dir();
    std::fs::create_dir_all(&dir).expect("dev folder");
    // What the trace says below must be this run's.
    let _ = std::fs::remove_file(dir.join("stop"));
    let _ = std::fs::remove_file(dir.join("trace.log"));
    let session = active_console_session().expect("a console session");
    let child = Command::new(env!("CARGO_BIN_EXE_swapdesk-service"))
        .args(["--console", "--instance", "1", "--dev-loopback"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the service starts");
    let pid = child.id();
    let mut service = Service(child);

    // The pipe name is `ui-<session>-<instance>`; `connect` checks the server process is the one we started.
    let name = format!("ui-{session}-1");
    let conn = until("the UI pipe", || {
        assert!(service.0.try_wait().expect("wait").is_none(), "the service exited early");
        connect(&name, pid, Duration::from_secs(1)).ok()
    });
    let (reader, writer) = conn.split().expect("split");
    let (mut reader, mut writer) = (FrameReader::new(reader), FrameWriter::new(writer));
    let ack = Handshake::client(&mut reader, &mut writer, IpcRole::Ui, std::process::id()).expect("HelloAck");
    assert_eq!(ack.win_session, session, "HelloAck names the pipe's session");
    println!("HelloAck win_session={}", ack.win_session);

    writer.write_message(&Message::UiRequest { req: 1, op: UiOp::GetThisDevice }).expect("write");
    let (status, body) = loop {
        match reader.read_message().expect("read") {
            Some(Message::UiReply { req: 1, status, body }) => break (status, body.0),
            Some(_) => {}
            None => panic!("the pipe closed"),
        }
    };
    assert_eq!(status, 0, "GetThisDevice succeeds");
    let ReplyBody::ThisDevice(this) = ReplyBody::decode(UiOp::GetThisDevice.code(), &body).expect("body") else {
        panic!()
    };
    println!("GetThisDevice name={:?} os={:?} ready={:?}", this.name, this.os, this.ready);
    assert!(!this.name.is_empty());

    // Both helpers started in the console session on their own pipes.
    let trace = dir.join("trace.log");
    let text = until("both helpers in the trace", || {
        let t = std::fs::read_to_string(&trace).unwrap_or_default();
        (t.contains("system helper started") && t.contains("user helper started")).then_some(t)
    });
    for line in text.lines().filter(|l| l.contains("helper started") || l.contains("UI pipe") || l.contains("attached"))
    {
        println!("trace: {line}");
    }

    drop((reader, writer));
    std::fs::write(dir.join("stop"), b"stop").expect("stop file");
    let status = until("the service to stop", || service.0.try_wait().expect("wait"));
    assert!(status.success(), "clean stop, got {status:?}");
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    assert!(text.contains("stopped"), "the trace says it stopped");
}
