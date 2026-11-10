//! The helper of a measured host: `swapdesk-helper.exe`'s wiring (the service's pipe, `run_helper`, the hardware
//! encoder on the capture's device) with the synthetic sources of [`crate::source`] in place of the screen and the
//! sound card. The measurement copies this executable beside the service under the helper's name, so the service
//! starts it exactly as it starts its own helper and no shipped binary carries a measurement switch.

use std::sync::Arc;
use std::time::Duration;

use swapdesk_capture_win::QpcClock;
use swapdesk_codec_win::{EncoderBackend, H264Encoder, H264EncoderOptions};
use swapdesk_fakes::RecordingInputSink;
use swapdesk_host::helper::{HelperDeps, run_helper};
use swapdesk_ipc::{IpcRole, VideoBackend};
use swapdesk_platform::{EncoderConfig, Frame};

use crate::source::{Family, SourceSpec, TimedScreen, capture_device, marked_sine};

struct Args {
    role: IpcRole,
    pipe: String,
    server_pid: u32,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let (mut role, mut pipe, mut server_pid) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--role" => {
                role = Some(match value()?.as_str() {
                    "system" => IpcRole::HelperSystem,
                    "user" => IpcRole::HelperUser,
                    other => return Err(format!("unknown role {other}")),
                })
            }
            "--pipe" => pipe = Some(value()?),
            "--server-pid" => server_pid = Some(value()?.parse::<u32>().map_err(|_| "bad --server-pid".to_string())?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        role: role.ok_or("--role is required")?,
        pipe: pipe.ok_or("--pipe is required")?,
        server_pid: server_pid.ok_or("--server-pid is required")?,
    })
}

/// The encoder under test, on the device the frames live on. The family is fixed by the run, never left to `Auto`: a
/// silent fallback would put another encoder's numbers under this one's name.
fn encoder(family: Family) -> impl Fn(&Frame, &EncoderConfig, VideoBackend) -> swapdesk_platform::Result<H264Encoder> {
    move |_first, _config, _backend| {
        let mut options = H264EncoderOptions {
            backend: match family {
                Family::Nvenc => EncoderBackend::Nvenc,
                Family::QuickSync => EncoderBackend::MediaFoundation,
            },
            ..Default::default()
        };
        options.mf.device = capture_device();
        Ok(H264Encoder::new(options))
    }
}

/// Exit codes as the real helper's: 0 after Shutdown/Bye/EOF, 2 bad arguments, 3 pipe refused, 4 the loop failed.
pub fn run(args: &[String]) -> i32 {
    let args = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("swapdesk-measure (helper): {e}");
            return 2;
        }
    };
    let Some(spec) = SourceSpec::from_env() else {
        eprintln!("swapdesk-measure (helper): {} is not set or not understood", crate::source::SPEC_VAR);
        return 2;
    };
    let conn = match swapdesk_sys_win::pipe::connect(&args.pipe, args.server_pid, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("swapdesk-measure (helper): pipe refused: {e:?}");
            return 3;
        }
    };
    let (reader, writer) = match conn.split() {
        Ok(rw) => rw,
        Err(e) => {
            eprintln!("swapdesk-measure (helper): pipe split: {e}");
            return 3;
        }
    };
    let (screen_spec, markers) = (spec.clone(), spec.markers.clone());
    let deps = HelperDeps::new(
        move |_display| TimedScreen::new(screen_spec.clone()),
        encoder(spec.family),
        || Ok(RecordingInputSink::new()),
        move || Ok(marked_sine(markers.clone())),
        Arc::new(QpcClock),
    );
    match run_helper(reader, writer, deps, args.role, std::process::id()) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("swapdesk-measure (helper): {e:?}");
            4
        }
    }
}
