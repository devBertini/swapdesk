//! `swapdesk-measure`: the session budgets of docs/SPEC.md measured through the whole chain on this PC, on the
//! synthetic sources of contract C7. A development tool, never shipped; `scripts/measure.py` builds what it needs,
//! runs it for every configuration and writes docs/PERFORMANCE.md.
//!
//! ```text
//! swapdesk-measure session --size 1920x1080 --fps 60 --encoder nvenc|quicksync --seconds 30 [--warmup 5]
//!                          --logs <folder> [--out <file>] [--instances <viewer>,<host>] [--service <exe>]
//!                          [--display <n>] [--viewer-adapter min|high] [--background]
//! swapdesk-measure path [--seconds 10] [--payload <bytes>] [--fps 60] [--out <file>]
//! swapdesk-measure idle --exe <program> [--settle 10] [--seconds 30] [-- <the program's arguments>]
//! swapdesk-measure --role <system|user> --pipe <name> --server-pid <pid>     (started by the service: the helper)
//! ```
//!
//! `session` (see [`run`]): two real services on loopback, this process as both users and as the viewer.
//! `path` (see [`path`]): the frame path alone, in this process, under a counting allocator.
//! `idle` (see [`idle`]): the memory and the idle CPU of a program it starts (B7 and B8 for `Swapdesk.exe`).
//! The last form is how the service starts its helper; `session` puts this executable beside the service under the
//! helper's name ([`helper`]).
//!
//! Exit codes: 0 the run measured a session, 1 it could not, 2 a wrong command line.
#![cfg_attr(not(windows), allow(unused))]

#[cfg(windows)]
#[path = "../../swapdesk-render-win/tests/common/harness.rs"]
mod harness;
#[cfg(windows)]
mod helper;
#[cfg(windows)]
mod idle;
#[cfg(windows)]
mod path;
#[cfg(windows)]
mod proc;
#[cfg(windows)]
mod run;
#[cfg(windows)]
mod source;
#[cfg(windows)]
mod stats;

#[cfg(windows)]
#[global_allocator]
static ALLOCATOR: path::Counting = path::Counting;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--role") => helper::run(&args),
        Some("session") => finish(session(&args[1..])),
        Some("path") => finish(frame_path(&args[1..])),
        Some("idle") => finish(idle_program(&args[1..])),
        _ => report(Failure::Usage("expected `session`, `path`, `idle` or the helper's arguments".into())),
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("swapdesk-measure runs on Windows only");
    std::process::exit(2);
}

#[cfg(windows)]
enum Failure {
    Usage(String),
    Run(String),
}

#[cfg(windows)]
type Outcome = Result<(stats::Fields, Option<std::path::PathBuf>), Failure>;

#[cfg(windows)]
fn report(f: Failure) -> i32 {
    match f {
        Failure::Usage(e) => {
            eprintln!("swapdesk-measure: {e}");
            2
        }
        Failure::Run(e) => {
            eprintln!("swapdesk-measure: the run failed: {e}");
            1
        }
    }
}

/// Prints the result, writes it to `--out`, and turns a run that measured nothing into exit 1.
#[cfg(windows)]
fn finish(result: Outcome) -> i32 {
    let (fields, out) = match result {
        Ok(r) => r,
        Err(f) => return report(f),
    };
    let text = fields.render();
    print!("{text}");
    if let Some(out) = out
        && let Err(e) = std::fs::write(&out, &text)
    {
        return report(Failure::Run(format!("{}: {e}", out.display())));
    }
    match fields.get("valid") {
        Some("yes") => 0,
        other => report(Failure::Run(format!("not a valid measurement ({})", other.unwrap_or("no verdict")))),
    }
}

/// The value after `name`, parsed.
#[cfg(windows)]
fn value<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>, Failure> {
    let Some(i) = args.iter().position(|a| a == name) else { return Ok(None) };
    let text = args.get(i + 1).ok_or_else(|| Failure::Usage(format!("{name} needs a value")))?;
    text.parse().map(Some).map_err(|_| Failure::Usage(format!("{name} {text}: not understood")))
}

#[cfg(windows)]
fn session(args: &[String]) -> Outcome {
    let size: String = value(args, "--size")?.unwrap_or_else(|| "1920x1080".into());
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .filter(|(w, h)| *w >= 512 && *h >= 64)
        .ok_or_else(|| Failure::Usage(format!("--size {size}: expected <width>x<height>, at least 512x64")))?;
    let encoder: String = value(args, "--encoder")?.unwrap_or_else(|| "nvenc".into());
    let family = source::Family::parse(&encoder)
        .ok_or_else(|| Failure::Usage(format!("--encoder {encoder}: expected nvenc or quicksync")))?;
    let instances: String = value(args, "--instances")?.unwrap_or_else(|| "71,72".into());
    let instances = instances
        .split_once(',')
        .and_then(|(v, h)| Some((v.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .filter(|(v, h)| v != h && *v <= 99 && *h <= 99)
        .ok_or_else(|| Failure::Usage(format!("--instances {instances}: expected two different numbers up to 99")))?;
    let logs: std::path::PathBuf =
        value(args, "--logs")?.ok_or_else(|| Failure::Usage("--logs <folder> is required".into()))?;
    let cfg = run::RunConfig {
        width,
        height,
        fps: value(args, "--fps")?.unwrap_or(60),
        family,
        seconds: value(args, "--seconds")?.unwrap_or(30),
        warmup: value(args, "--warmup")?.unwrap_or(5),
        logs,
        instances,
        service: value(args, "--service")?,
        display: value(args, "--display")?,
        background: args.iter().any(|a| a == "--background"),
        viewer_high: match value::<String>(args, "--viewer-adapter")?.as_deref() {
            None | Some("min") => false,
            Some("high") => true,
            Some(other) => return Err(Failure::Usage(format!("--viewer-adapter {other}: expected min or high"))),
        },
    };
    if cfg.seconds < 5 || cfg.fps == 0 {
        return Err(Failure::Usage("--seconds is at least 5 and --fps at least 1".into()));
    }
    let fields = run::run(&cfg).map_err(Failure::Run)?;
    Ok((fields, value(args, "--out")?))
}

#[cfg(windows)]
fn frame_path(args: &[String]) -> Outcome {
    let cfg = path::PathConfig {
        seconds: value(args, "--seconds")?.unwrap_or(10),
        payload: value(args, "--payload")?.unwrap_or(40_000),
        fps: value(args, "--fps")?.unwrap_or(60),
    };
    if cfg.seconds < 2 || cfg.fps == 0 || cfg.payload == 0 {
        return Err(Failure::Usage("--seconds is at least 2, --fps and --payload at least 1".into()));
    }
    let fields = path::run(&cfg).map_err(Failure::Run)?;
    Ok((fields, value(args, "--out")?))
}

#[cfg(windows)]
fn idle_program(args: &[String]) -> Outcome {
    // What follows `--` belongs to the program.
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (own, theirs) = (&args[..split], args.get(split + 1..).unwrap_or_default());
    let cfg = idle::IdleConfig {
        exe: value(own, "--exe")?.ok_or_else(|| Failure::Usage("--exe <program> is required".into()))?,
        args: theirs.to_vec(),
        settle: value(own, "--settle")?.unwrap_or(10),
        seconds: value(own, "--seconds")?.unwrap_or(30),
    };
    let fields = idle::run(&cfg).map_err(Failure::Run)?;
    Ok((fields, value(own, "--out")?))
}
