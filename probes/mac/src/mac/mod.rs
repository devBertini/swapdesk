//! macOS-only subcommands (macos.md 12b).

mod cg;
mod dns;
mod sck;
mod vt;

use crate::d;
use crate::log::{self, emit};
use crate::net;
use crate::Args;
use objc2_foundation::{NSProcessInfo, NSString};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use std::time::Duration;

const BUNDLE_ID: &str = "local.swapdesk.probe";

/// Returns false for an unknown subcommand.
pub fn run(cmd: &str, args: &Args) -> bool {
    match cmd {
        "info" => info(),
        "tcc" => cg::tcc(args.flag("request")),
        "sck" => sck::run(args.num("seconds", 5), args.flag("audio")),
        "cgstream" => cg::cgstream(args.num("seconds", 5)),
        "inject" => cg::inject(args.get("text").unwrap_or("abc"), args.num("delay", 5), args.num("keycode", 0xFF) as u16),
        "keytap" => cg::keytap(args.num("seconds", 30), args.flag("active")),
        "bonjour" => dns::run(args.num("seconds", 30)),
        "smapp" => smapp(args.pos.first().map(String::as_str), args.pos.get(1).map(String::as_str)),
        "vt" => vt::run(args),
        "daemon-run" => run_loop(true, args),
        "agent-run" => run_loop(false, args),
        _ => return false,
    }
    true
}

fn info() {
    // SAFETY: plain property read on the shared NSProcessInfo.
    let v = NSProcessInfo::processInfo().operatingSystemVersion();
    let (uid, euid) = log::uid();
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let session_dict = objc2_core_graphics::CGSessionCopyCurrentDictionary()
        .map_or_else(|| "NULL (no console session: login window, SSH or daemon)".to_string(), |dict| format!("{dict:?}"));
    emit(
        "info",
        true,
        &d!(
            "macos" => format!("{}.{}.{}", v.majorVersion, v.minorVersion, v.patchVersion),
            "arch" => std::env::consts::ARCH,
            "uid" => uid,
            "euid" => euid,
            "exe" => exe.clone(),
            "build_nonce" => log::NONCE,
            "cg_session_dictionary" => session_dict,
            "codesign" => codesign_summary(&exe)
        ),
    );
}

fn codesign_summary(exe: &str) -> String {
    match std::process::Command::new("/usr/bin/codesign").args(["-dvv", exe]).output() {
        Ok(out) => String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| ["Identifier=", "Authority=", "TeamIdentifier=", "CodeDirectory", "CDHash=", "Signature="].iter().any(|k| l.contains(k)))
            .collect::<Vec<_>>()
            .join(" | "),
        Err(e) => format!("codesign not run: {e}"),
    }
}

fn status_name(s: SMAppServiceStatus) -> &'static str {
    if s == SMAppServiceStatus::NotRegistered {
        "notRegistered"
    } else if s == SMAppServiceStatus::Enabled {
        "enabled"
    } else if s == SMAppServiceStatus::RequiresApproval {
        "requiresApproval"
    } else if s == SMAppServiceStatus::NotFound {
        "notFound"
    } else {
        "unknown"
    }
}

fn smapp(op: Option<&str>, role: Option<&str>) {
    let (Some(op), Some(role)) = (op, role) else {
        emit("smapp", false, &d!("error" => "usage: smapp <register|status|unregister|open-settings> <daemon|agent|agent-inject|loginwindow>"));
        return;
    };
    if op == "open-settings" {
        // SAFETY: class method with no arguments.
        unsafe { SMAppService::openSystemSettingsLoginItems() };
        emit("smapp.open_settings", true, &d!());
        return;
    }
    let plist = match role {
        "daemon" => format!("{BUNDLE_ID}.daemon.plist"),
        "agent" => format!("{BUNDLE_ID}.agent.plist"),
        "agent-inject" => format!("{BUNDLE_ID}.agent.inject.plist"),
        "loginwindow" => format!("{BUNDLE_ID}.loginwindow.plist"),
        other => {
            emit("smapp", false, &d!("error" => format!("unknown role {other}")));
            return;
        }
    };
    let name = NSString::from_str(&plist);
    // SAFETY: class constructors taking an NSString.
    let service = unsafe {
        if role == "daemon" {
            SMAppService::daemonServiceWithPlistName(&name)
        } else {
            SMAppService::agentServiceWithPlistName(&name)
        }
    };
    let check = format!("smapp.{op}.{role}");
    // SAFETY: plain calls on a live service object.
    unsafe {
        match op {
            "register" => {
                let result = service.registerAndReturnError();
                let err = result.as_ref().err().map(|e| format!("{} (domain {}, code {})", e.localizedDescription(), e.domain(), e.code()));
                emit(&check, result.is_ok(), &d!("plist" => plist, "status" => status_name(service.status()), "error" => err.unwrap_or_default()));
            }
            "unregister" => {
                let result = service.unregisterAndReturnError();
                let err = result.as_ref().err().map(|e| format!("{} (domain {}, code {})", e.localizedDescription(), e.domain(), e.code()));
                emit(&check, result.is_ok(), &d!("plist" => plist, "status" => status_name(service.status()), "error" => err.unwrap_or_default()));
            }
            _ => {
                let status = service.status();
                emit(&format!("smapp.status.{role}"), status == SMAppServiceStatus::Enabled, &d!("plist" => plist, "status" => status_name(status)));
            }
        }
    }
}

struct Conf {
    tests: Vec<String>,
    every: u64,
    peer: Option<String>,
    port: u16,
    text: String,
    inject_delay: u64,
    keycode: u16,
    token: String,
}

/// Defaults from the command line, overridden each cycle by `<logdir>/<name>.conf` (`key=value` lines), so a
/// signed plist never needs editing: keys tests, every, peer, port, text, inject-delay, inject-keycode, inject-token.
fn load_conf(args: &Args, daemon: bool) -> Conf {
    let mut conf = Conf {
        tests: args.get("tests").unwrap_or(if daemon { "info,net" } else { "info,tcc,sck" }).split(',').map(str::to_string).collect(),
        every: args.num("every", 15),
        peer: args.get("peer").map(str::to_string),
        port: args.num("port", 47999) as u16,
        text: args.get("text").unwrap_or("abc").to_string(),
        inject_delay: args.num("inject-delay", 5),
        keycode: args.num("keycode", 0xFF) as u16,
        token: String::new(),
    };
    let Some(ctx) = log::ctx() else { return conf };
    let path = log::log_dir().join(format!("{}.conf", ctx.name));
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "tests" => conf.tests = v.split(',').map(|s| s.trim().to_string()).collect(),
                "every" => conf.every = v.parse().unwrap_or(conf.every),
                "peer" => conf.peer = Some(v.to_string()),
                "port" => conf.port = v.parse().unwrap_or(conf.port),
                "text" => conf.text = v.to_string(),
                "inject-delay" => conf.inject_delay = v.parse().unwrap_or(conf.inject_delay),
                "inject-keycode" => conf.keycode = v.parse().unwrap_or(conf.keycode),
                "inject-token" => conf.token = v.to_string(),
                _ => {}
            }
        }
    }
    conf
}

/// `daemon-run` and `agent-run`: started by launchd, loop forever so the process is not short-lived
/// (TN3179), running the listed checks every `every` seconds. `inject` runs once per start and again whenever
/// `inject-token` changes.
fn run_loop(daemon: bool, args: &Args) {
    let mode = if daemon { "daemon-run" } else { "agent-run" };
    emit("run.start", true, &d!("mode" => mode, "pid" => u64::from(std::process::id())));
    if daemon {
        let port = args.num("port", 47999) as u16;
        std::thread::spawn(move || net::net_echo(port, 0));
        std::thread::spawn(net::ipc_serve);
    }
    let mut cycle = 0u64;
    let mut injected_for: Option<String> = None;
    loop {
        let conf = load_conf(args, daemon);
        emit("run.cycle", true, &d!("cycle" => cycle, "tests" => conf.tests.join(","), "every_s" => conf.every));
        for test in &conf.tests {
            match test.as_str() {
                "info" => info(),
                "tcc" => cg::tcc(false),
                "tcc-request" => cg::tcc(true),
                "sck" => sck::run(3, false),
                "sck-audio" => sck::run(5, true),
                "cgstream" => cg::cgstream(3),
                "inject" => {
                    if injected_for.as_deref() != Some(conf.token.as_str()) {
                        injected_for = Some(conf.token.clone());
                        cg::inject(&conf.text, conf.inject_delay, conf.keycode);
                    }
                }
                "net" => match &conf.peer {
                    Some(peer) => net::net(peer, conf.port, 1),
                    None => emit("net", false, &d!("error" => "no peer configured (--peer or peer= in the conf file)")),
                },
                "ipc" => net::ipc_client(),
                "bonjour" => dns::run(5),
                other => emit("run.unknown_test", false, &d!("test" => other)),
            }
        }
        cycle += 1;
        std::thread::sleep(Duration::from_secs(conf.every.max(1)));
    }
}
