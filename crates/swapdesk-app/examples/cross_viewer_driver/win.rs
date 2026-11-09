//! Development tool of the stage 24 proof: this PC as the controlling machine, with no human at it.
//!
//! Connects to the UI pipe of the installed `SwapdeskService` as the logged-in user, exactly as `Swapdesk.exe` does
//! (`swapdesk_app::connect::open_pipe`), picks an already paired device, and runs `swapdesk_crossdrive::viewer_drive`
//! in its paired mode: connects with control, decodes the video with the Windows H.264 decoder, counts the audio
//! datagrams and the host's cursor updates, sends a few small pointer moves and ends the session. Nothing is paired and
//! nothing is forgotten: the pairing outlives the tool.
//!
//!     cross_viewer_driver --device <id prefix or name part> [--seconds N] [--no-input]
//!
//! `--seconds` (default 20) is how long the session stays open at least; `--no-input` sends no pointer move. Markers go
//! to stdout and the last two lines are the counts (`VIEWER_COUNTS ...`) and `VIEWER_RESULT PASS` or
//! `VIEWER_RESULT FAIL <why>`. The exit code is 0 only when the session went active, video frames were decoded and not
//! black, and audio datagrams arrived. The pointer proof is reported (`POINTER ...`) but is not part of the exit code.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use swapdesk_app::cli::Options;
use swapdesk_app::connect::open_pipe;
use swapdesk_codec_win::{DecoderOptions, MfH264Decoder};
use swapdesk_crossdrive::{DecoderFactory, Media, Pump, ViewerConfig, ViewerReport, viewer_drive};
use swapdesk_platform::VideoDecoder;
use swapdesk_ui::service_backend::ServiceBackend;
use swapdesk_ui::{Command, DeviceView, Reply, UiWaker};

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

/// The one paired device whose id starts with `wanted` or whose name contains it (case-insensitive).
fn pick(devices: &[DeviceView], wanted: &str) -> Result<String, String> {
    let w = wanted.to_lowercase();
    let hits: Vec<&DeviceView> = devices
        .iter()
        .filter(|d| d.paired && (d.id.starts_with(wanted) || d.name.to_lowercase().contains(&w)))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => {
            let paired: Vec<String> =
                devices.iter().filter(|d| d.paired).map(|d| format!("{} ({})", d.name, d.id)).collect();
            Err(format!("no paired device matches {wanted:?}; paired: [{}]", paired.join(", ")))
        }
        many => Err(format!(
            "{wanted:?} matches {} paired devices: [{}]",
            many.len(),
            many.iter().map(|d| format!("{} ({})", d.name, d.id)).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Whether the report meets the proof: active, decoded, not black, audio.
fn verdict(r: &ViewerReport) -> Result<(), String> {
    let mut missing = Vec::new();
    if !r.session_active {
        missing.push("the session never went active".to_string());
    }
    if r.frames_decoded == 0 {
        missing.push(format!(
            "no video frame decoded (received {}, errors {}: {})",
            r.video_received, r.decode_errors, r.last_decode_error
        ));
    }
    if r.nonblack_pct.is_none_or(|p| p == 0) {
        missing.push(format!("the decoded picture is black or was not sampled ({:?})", r.nonblack_pct));
    }
    if r.audio_datagrams == 0 {
        missing.push("no audio datagram arrived".to_string());
    }
    if missing.is_empty() { Ok(()) } else { Err(missing.join("; ")) }
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(device) = value(&args, "--device") else {
        eprintln!("usage: cross_viewer_driver --device <id prefix or name part> [--seconds N] [--no-input]");
        return ExitCode::from(2);
    };
    let seconds = value(&args, "--seconds").and_then(|v| v.parse::<u64>().ok()).unwrap_or(20);
    let log = |line: &str| println!("{line}");
    let fail = |why: &str| {
        println!("VIEWER_RESULT FAIL {why}");
        ExitCode::FAILURE
    };

    let conn = match open_pipe(&Options::default()) {
        Ok(c) => c,
        Err(e) => return fail(&format!("cannot open the UI pipe: {e}")),
    };
    let (reader, writer) = match conn.split() {
        Ok(rw) => rw,
        Err(e) => return fail(&format!("cannot split the pipe: {e}")),
    };
    // The decoder is built on the decode thread (it is not `Send`); CPU BGRA output, which the black check reads.
    let factory: DecoderFactory =
        Box::new(|| -> Box<dyn VideoDecoder> { Box::new(MfH264Decoder::new(DecoderOptions::default())) });
    let media = Media::new(factory);
    let backend =
        match ServiceBackend::start_with_sink(reader, writer, Arc::new(NoWake), std::process::id(), Some(media.sink()))
        {
            Ok(b) => b,
            Err(e) => {
                media.finish();
                return fail(&format!("handshake: {e}"));
            }
        };
    log(&format!("VIEWER_CONNECTED win_session={}", backend.win_session()));

    let devices = match Pump::new(&backend).ask(Command::ListDevices, Duration::from_secs(10)) {
        Ok(Reply::Devices(list)) => list,
        Ok(other) => {
            media.finish();
            return fail(&format!("unexpected reply to ListDevices: {other:?}"));
        }
        Err(e) => {
            media.finish();
            return fail(&e);
        }
    };
    let id = match pick(&devices, &device) {
        Ok(id) => id,
        Err(e) => {
            media.finish();
            return fail(&e);
        }
    };
    log(&format!("VIEWER_DEVICE {id}"));

    let mut cfg = ViewerConfig::new("", PathBuf::new(), PathBuf::new(), PathBuf::new());
    cfg.paired = Some(id);
    cfg.send_input = !args.iter().any(|a| a == "--no-input");
    cfg.hold = Duration::from_secs(seconds);
    let (report, result) = viewer_drive(&backend, &media, &cfg, &log);
    // The backend must not deliver video once the decode thread is gone.
    drop(backend);
    media.finish();
    println!("{}", report.counts_line());
    let verdict = verdict(&report);
    match (verdict, result) {
        (Ok(()), Ok(())) => {
            println!("VIEWER_RESULT PASS");
            ExitCode::SUCCESS
        }
        // The proof is met but a side step (the pointer moves) did not pass: still a pass, said so.
        (Ok(()), Err(e)) => {
            println!("VIEWER_RESULT PASS (note: {e})");
            ExitCode::SUCCESS
        }
        (Err(why), Ok(())) => fail(&why),
        (Err(why), Err(e)) => fail(&format!("{why} ({e})")),
    }
}
