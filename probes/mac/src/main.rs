//! swapdesk-probe: the throwaway M0-mac probe (docs/research/macos.md section 12).
//! One binary; the macOS parts live in `mac` behind `cfg(target_os = "macos")`; `net`, `net-echo`,
//! `wol` and `report` use std only, so the same source builds on Windows as the peer.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod log;
#[cfg(target_os = "macos")]
mod mac;
mod net;
mod report;

use std::collections::HashMap;

/// `--key value` and `--flag` options after the subcommand; everything else is positional.
pub struct Args {
    pub pos: Vec<String>,
    opts: HashMap<String, String>,
}

impl Args {
    fn parse(raw: &[String]) -> Self {
        let mut pos = Vec::new();
        let mut opts = HashMap::new();
        let mut i = 0;
        while i < raw.len() {
            if let Some(key) = raw[i].strip_prefix("--") {
                if i + 1 < raw.len() && !raw[i + 1].starts_with("--") {
                    opts.insert(key.to_string(), raw[i + 1].clone());
                    i += 2;
                } else {
                    opts.insert(key.to_string(), "true".to_string());
                    i += 1;
                }
            } else {
                pos.push(raw[i].clone());
                i += 1;
            }
        }
        Args { pos, opts }
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.opts.get(key).map(String::as_str)
    }
    pub fn flag(&self, key: &str) -> bool {
        self.opts.contains_key(key)
    }
    pub fn num(&self, key: &str, default: u64) -> u64 {
        self.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
    }
}

const USAGE: &str = "swapdesk-probe <info|tcc|sck|cgstream|inject|keytap|net|bonjour|smapp|daemon-run|agent-run|net-echo|wol|vt|report> [options]
  global: --log <path>  --name <logname>
  info
  tcc [--request]
  sck [--seconds N] [--audio]
  cgstream [--seconds N]
  inject [--text S] [--delay S] [--keycode N]
  keytap --seconds N [--active]
  net --peer IP --port P --seconds N
  bonjour --seconds N
  smapp <register|status|unregister|open-settings> <daemon|agent|agent-inject|loginwindow>
  daemon-run | agent-run [--tests list] [--every S] [--peer IP] [--port P] [--text S] [--inject-delay S]
  net-echo --port P [--seconds N]
  wol --mac AA:BB:CC:DD:EE:FF --broadcast A.B.C.255
  vt [--frames N] [--size WxH] [--force-key a,b,c]
  report [--out path] [--logs dir]";

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = raw.first().cloned() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let args = Args::parse(&raw[1..]);
    let default_name = match cmd.as_str() {
        "agent-run" => "agent",
        "daemon-run" => "daemon",
        _ => "app",
    };
    let name = args.get("name").unwrap_or(default_name).to_string();
    log::init(args.get("log").map(str::to_string), name);

    match cmd.as_str() {
        "net" => net::net(
            args.get("peer").unwrap_or("127.0.0.1"),
            args.num("port", 47999) as u16,
            args.num("seconds", 20),
        ),
        "net-echo" => net::net_echo(args.num("port", 47999) as u16, args.num("seconds", 0)),
        "wol" => net::wol(
            args.get("mac").unwrap_or(""),
            args.get("broadcast").unwrap_or("255.255.255.255"),
        ),
        "report" => report::run(std::path::Path::new(args.get("out").unwrap_or("results.txt")), args.get("logs")),
        "-h" | "--help" | "help" => println!("{USAGE}"),
        other => {
            #[cfg(target_os = "macos")]
            {
                if !mac::run(other, &args) {
                    eprintln!("unknown subcommand: {other}\n{USAGE}");
                    std::process::exit(2);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                eprintln!("subcommand {other} is macOS-only; this build has net, net-echo, wol and report\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
}
