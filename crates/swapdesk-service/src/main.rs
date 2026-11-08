//! `swapdesk-service.exe` (CONTRACTS.md C2.1, C2.4): the LocalSystem service of Swapdesk, or with `--console` the same
//! code as the current user for development. Only wiring: the logic is `swapdesk_host`, the Windows calls are the
//! `*-win` crates.
//!
//! ```text
//! swapdesk-service.exe                                      the Windows service SwapdeskService (started by the SCM)
//! swapdesk-service.exe --console [--instance N] [--dev-loopback]
//! ```
//!
//! * `--console`: development mode (C2.4). The same code as the current user, no SCM, stores and trace in
//!   `%LOCALAPPDATA%\Swapdesk-dev\<instance>`, helpers started with `CreateProcessW` from this exe's own folder. It
//!   stops on `stop` typed on stdin or when a file named `stop` appears in that folder.
//! * `--instance N` (development only, 0 to 99): a second copy on one PC. Pipe names get the suffix `-N`
//!   (`\\.\pipe\swapdesk\ui-<session>-N`, `helper-<role>-<session>-<hex>-N`) and both UDP ports move up by `10 * N`
//!   (session 47820 + 10 N, pairing 47821 + 10 N).
//! * `--dev-loopback` (development only): `ServiceConfig::dev_loopback`. The instance listens on 127.0.0.1 only (no
//!   LAN socket, no mDNS) and accepts loopback peers; another instance is added with `AddByAddress` as
//!   `127.0.0.1:<its session port>`. Never use it on a machine that should be reachable.
//!
//! No firewall rule and no install: the installer owns both. The command line parser is in [`options`].

// Outside tests an unwrap is a crash of the service: return the error or use `expect` with the reason.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

mod options;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod helpers;
#[cfg(windows)]
mod ui_pipe;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match options::parse(&args) {
        Err(e) => {
            eprintln!("swapdesk-service: {e}\n{}", options::USAGE);
            ExitCode::from(2)
        }
        Ok(options::Parsed::Help) => {
            println!("{}", options::USAGE);
            ExitCode::SUCCESS
        }
        Ok(options::Parsed::Run(o)) => run(o),
    }
}

#[cfg(not(windows))]
fn run(_: options::Options) -> ExitCode {
    eprintln!("swapdesk-service runs on Windows only");
    ExitCode::from(2)
}

#[cfg(windows)]
fn run(o: options::Options) -> ExitCode {
    use swapdesk_sys_win::{ServiceContext, ServiceError, StopSignal, WindowWatcher, run_service, session_channel};

    let exit = |code: u32| ExitCode::from(u8::try_from(code).unwrap_or(1));
    if !o.console {
        return match run_service(app::SERVICE_NAME, move |ctx| app::run(o, ctx)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(ServiceError::NotAService) => {
                eprintln!(
                    "swapdesk-service: not started by the service control manager; use --console for development"
                );
                ExitCode::from(2)
            }
            Err(e) => {
                eprintln!("swapdesk-service: {e}");
                ExitCode::from(1)
            }
        };
    }
    // Development: the session and power messages come from a hidden window, and the process stops on `stop` typed on
    // stdin or on a file named `stop` in the dev folder (an unattended background run is stopped with the file).
    let dir = match app::dev_dir(&o) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("swapdesk-service: {e}");
            return ExitCode::from(1);
        }
    };
    let (events, watcher) = session_channel();
    let stop = StopSignal::new();
    let _window = match WindowWatcher::spawn(events.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("swapdesk-service: no session window ({e}); session changes are not followed");
            None
        }
    };
    {
        let (stop, dir) = (stop.clone(), dir.clone());
        std::thread::spawn(move || {
            let file = dir.join("stop");
            loop {
                if file.exists() {
                    let _ = std::fs::remove_file(&file);
                    stop.set();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        });
    }
    {
        let (stop, events) = (stop.clone(), events.clone());
        std::thread::spawn(move || {
            let mut line = String::new();
            // End of input is not a request to stop (a background run has none); the word is.
            while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
                if matches!(line.trim(), "stop" | "quit" | "q") {
                    stop.set();
                    events.stop();
                    return;
                }
                line.clear();
            }
        });
    }
    exit(app::run(o, ServiceContext { watcher, events, stop }))
}
