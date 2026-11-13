//! Development tool of the stage 24 proof: this PC as the controlling machine, with no human at it.
//!
//! Connects to the UI pipe of the installed `SwapdeskService` (or of a development instance: `--instance N
//! --service-pid P`) as the logged-in user, exactly as `Swapdesk.exe` does
//! (`swapdesk_app::connect::open_pipe`), picks an already paired device, and runs `swapdesk_crossdrive::viewer_drive`
//! in its paired mode: connects with control, decodes the video with the Windows H.264 decoder, counts the audio
//! datagrams and the host's cursor updates, sends a few small pointer moves and ends the session. Nothing is paired and
//! nothing is forgotten: the pairing outlives the tool.
//!
//!     cross_viewer_driver --device <id prefix or name part> [--seconds N] [--no-input]
//!     cross_viewer_driver --pair <host ipv4> --dir <folder> [--seconds N] [--no-input] [--type] [--clip] [--files] [--chat] [--term] [--swap] [--no-media] [--reconnect]
//!
//! `--pair` is the full script against a host run by `Swapdesk --drive-host` (a scratch identity on the Mac): it adds
//! the host by address, prints `VIEWER_CODE <code>`, reads the host's code from `<folder>/peer-code.txt`, waits for
//! `<folder>/go` (the host has granted) and reads the pointer anchor from `<folder>/anchor` ("x,y" in pointer units);
//! the orchestrator writes those three files. The pairing is forgotten at the end.
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
fn verdict(r: &ViewerReport, no_media: bool) -> Result<(), String> {
    let mut missing = Vec::new();
    if !r.session_active {
        missing.push("the session never went active".to_string());
    }
    if no_media {
        return if missing.is_empty() { Ok(()) } else { Err(missing.join("; ")) };
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
    let device = value(&args, "--device");
    let pair = value(&args, "--pair").zip(value(&args, "--dir"));
    if device.is_none() == pair.is_none() {
        eprintln!(
            "usage: cross_viewer_driver --device <id prefix or name part> [--seconds N] [--no-input]
       cross_viewer_driver --pair <host ipv4> --dir <folder> [--seconds N] [--no-input] [--type] [--clip] [--files] [--chat] [--term] [--swap] [--no-media] [--reconnect]"
        );
        return ExitCode::from(2);
    }
    let seconds = value(&args, "--seconds").and_then(|v| v.parse::<u64>().ok()).unwrap_or(20);
    let log = |line: &str| println!("{line}");
    let fail = |why: &str| {
        println!("VIEWER_RESULT FAIL {why}");
        ExitCode::FAILURE
    };

    // `--instance N --service-pid P`: a development instance of the service (`swapdesk-service --console --instance
    // N`) instead of the installed one, which development never drives.
    let options = Options {
        instance: value(&args, "--instance").and_then(|v| v.parse().ok()),
        service_pid: value(&args, "--service-pid").and_then(|v| v.parse().ok()),
        ..Options::default()
    };
    let conn = match open_pipe(&options) {
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

    let mut cfg = match (&device, &pair) {
        (Some(device), _) => match paired_config(&backend, device) {
            Ok(cfg) => cfg,
            Err(e) => {
                media.finish();
                return fail(&e);
            }
        },
        (None, Some((ipv4, dir))) => {
            let dir = PathBuf::from(dir);
            ViewerConfig::new(ipv4, dir.join("peer-code.txt"), dir.join("go"), dir.join("anchor"))
        }
        (None, None) => unreachable!("checked above"),
    };
    cfg.send_input = !args.iter().any(|a| a == "--no-input");
    cfg.hold = Duration::from_secs(seconds);
    // `--type`: the keyboard proof; the orchestrator writes `<dir>/type` once the host's text editor has the focus.
    if let (true, Some((_, dir))) = (args.iter().any(|a| a == "--type"), &pair) {
        cfg.type_after = Some(PathBuf::from(dir).join("type"));
    }
    // `--clip`: the copy and paste proof through this user's clipboard; the orchestrator works the host's in `<dir>`.
    if let (true, Some((_, dir))) = (args.iter().any(|a| a == "--clip"), &pair) {
        backend.set_clipboard(swapdesk_input_win::clipboard::WinClipboard::new());
        cfg.clip = Some(swapdesk_crossdrive::ClipStep {
            dir: PathBuf::from(dir),
            provider: std::sync::Mutex::new(Box::new(swapdesk_input_win::clipboard::WinClipboard::new())),
        });
    }
    // `--files`: the files proof; the orchestrator works the host's disk in `<dir>`.
    if let (true, Some((_, dir))) = (args.iter().any(|a| a == "--files"), &pair) {
        cfg.files = Some(PathBuf::from(dir));
    }
    // `--chat`: the chat proof; the orchestrator carries the texts to and from the host through `<dir>`.
    if let (true, Some((_, dir))) = (args.iter().any(|a| a == "--chat"), &pair) {
        cfg.chat = Some(PathBuf::from(dir));
    }
    // `--term`: the terminal and quick actions proof; the orchestrator writes the host's console user into
    // `<dir>/term-expect-user`.
    if let (true, Some((_, dir))) = (args.iter().any(|a| a == "--term"), &pair) {
        cfg.term = Some(PathBuf::from(dir));
    }
    // `--swap`: asks for a swap and expects the reverse session (runs last, not with `--reconnect`).
    cfg.swap = pair.is_some() && args.iter().any(|a| a == "--swap");
    // `--no-media`: no picture is waited for (a host that cannot capture one, such as a locked Mac): the session, the
    // terminal, the quick actions, the chat and the swap are proven, the picture is not.
    cfg.no_media = args.iter().any(|a| a == "--no-media");
    // `--reconnect`: the orchestrator drops the link for about 10 s and the session must come back (runs last).
    if let (true, Some((_, dir))) = (args.iter().any(|a| a == "--reconnect"), &pair) {
        cfg.reconnect = Some(PathBuf::from(dir));
    }
    // `--fresh-host`: the orchestrator starts the host after this viewer and writes `<dir>/host-up`.
    if let (true, Some((_, dir))) = (args.iter().any(|a| a == "--fresh-host"), &pair) {
        cfg.host_up = Some(PathBuf::from(dir).join("host-up"));
    }
    let (report, result) = viewer_drive(&backend, &media, &cfg, &log);
    // The backend must not deliver video once the decode thread is gone.
    drop(backend);
    media.finish();
    println!("{}", report.counts_line());
    let verdict = verdict(&report, cfg.no_media)
        .and_then(|()| match &report.clip_failed {
            Some(why) => Err(format!("copy and paste: {why}")),
            None => Ok(()),
        })
        .and_then(|()| match &report.files_failed {
            Some(why) => Err(format!("files: {why}")),
            None => Ok(()),
        })
        .and_then(|()| match &report.chat_failed {
            Some(why) => Err(format!("chat: {why}")),
            None => Ok(()),
        })
        .and_then(|()| match &report.term_failed {
            Some(why) => Err(format!("terminal: {why}")),
            None => Ok(()),
        })
        .and_then(|()| match &report.actions_failed {
            Some(why) => Err(format!("quick actions: {why}")),
            None => Ok(()),
        })
        .and_then(|()| match &report.swap_failed {
            Some(why) => Err(format!("swap: {why}")),
            None => Ok(()),
        })
        .and_then(|()| match &report.reconnect_failed {
            Some(why) => Err(format!("reconnect: {why}")),
            None => Ok(()),
        });
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

/// The paired mode's config: the one paired device `wanted` names.
fn paired_config(backend: &ServiceBackend, wanted: &str) -> Result<ViewerConfig, String> {
    let devices = match Pump::new(backend).ask(Command::ListDevices, Duration::from_secs(10))? {
        Reply::Devices(list) => list,
        other => return Err(format!("unexpected reply to ListDevices: {other:?}")),
    };
    let id = pick(&devices, wanted)?;
    println!("VIEWER_DEVICE {id}");
    let mut cfg = ViewerConfig::new("", PathBuf::new(), PathBuf::new(), PathBuf::new());
    cfg.paired = Some(id);
    Ok(cfg)
}
