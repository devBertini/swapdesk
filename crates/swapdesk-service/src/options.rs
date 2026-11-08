//! The command line of `swapdesk-service.exe`.
//!
//! * no argument: the Windows service `SwapdeskService`, started by the SCM;
//! * `--console`: development mode (C2.4): the same code as the current user, without the SCM, in the dev folder
//!   `%LOCALAPPDATA%\Swapdesk-dev\<instance>`; stop it with `stop` on stdin or by creating the file `stop` in that
//!   folder;
//! * `--instance <n>` (development only, 0 to 99): a second copy on one PC. Pipe names get the suffix `-<n>`
//!   (`ui-<session>-<n>`, `helper-<role>-<session>-<hex>-<n>`), and the session and pairing UDP ports move up by
//!   `10 * n` (47820 + 10 n and 47821 + 10 n);
//! * `--dev-loopback` (development only): `ServiceConfig::dev_loopback`. The instance listens on 127.0.0.1 only
//!   (no LAN socket, no mDNS), accepts loopback peers, and a peer of another instance is named by its address and
//!   session port in `AddByAddress` (`127.0.0.1:47830`). Never use it on a machine that should be reachable.

pub const USAGE: &str = "swapdesk-service.exe [--console [--instance <n>] [--dev-loopback]]\n\
  (no argument)       run as the Windows service SwapdeskService (started by the SCM)\n\
  --console           development mode: run as the current user in %LOCALAPPDATA%\\Swapdesk-dev\\<instance>\n\
  --instance <n>      development only (0..99): pipe names get the suffix -<n>, both UDP ports move up by 10*n\n\
  --dev-loopback      development only: listen on 127.0.0.1 only and accept loopback peers\n\
  --help              this text";

/// The highest `--instance`: 47821 + 10 * 99 stays below 65535 with room to spare.
pub const MAX_INSTANCE: u16 = 99;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    pub console: bool,
    pub instance: Option<u16>,
    pub dev_loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Options),
    Help,
}

impl Options {
    /// The pipe-name suffix: `-<n>` with `--instance`, else empty.
    pub fn suffix(&self) -> String {
        self.instance.map_or_else(String::new, |n| format!("-{n}"))
    }

    /// What `--instance` adds to both UDP ports.
    pub fn port_offset(&self) -> u16 {
        self.instance.map_or(0, |n| n * 10)
    }

    /// The name of the dev folder under `Swapdesk-dev`.
    pub fn instance_name(&self) -> String {
        self.instance.map_or_else(|| "default".to_string(), |n| n.to_string())
    }
}

pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--console" => o.console = true,
            "--dev-loopback" => o.dev_loopback = true,
            "--instance" => {
                let v = it.next().ok_or("--instance needs a number")?;
                let n: u16 = v.parse().map_err(|_| format!("--instance {v}: not a number"))?;
                if n > MAX_INSTANCE {
                    return Err(format!("--instance {n}: at most {MAX_INSTANCE}"));
                }
                o.instance = Some(n);
            }
            "--help" | "-h" | "/?" => return Ok(Parsed::Help),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    // The production service never carries a development switch: a service that answered on loopback or beside
    // another instance would be a different program.
    if !o.console && (o.instance.is_some() || o.dev_loopback) {
        return Err("--instance and --dev-loopback are development options: they need --console".into());
    }
    Ok(Parsed::Run(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Parsed, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_default_is_the_service_and_the_dev_switches_need_console() {
        assert_eq!(p(&[]), Ok(Parsed::Run(Options::default())));
        assert!(p(&["--instance", "1"]).is_err());
        assert!(p(&["--dev-loopback"]).is_err());
        assert_eq!(p(&["--help"]), Ok(Parsed::Help));
        assert!(p(&["--bogus"]).is_err());
    }

    #[test]
    fn an_instance_suffixes_pipes_and_moves_ports_by_ten() {
        let Ok(Parsed::Run(o)) = p(&["--console", "--instance", "1", "--dev-loopback"]) else { panic!() };
        assert_eq!(
            (o.console, o.dev_loopback, o.suffix(), o.port_offset(), o.instance_name()),
            (true, true, "-1".to_string(), 10, "1".to_string())
        );
        let Ok(Parsed::Run(plain)) = p(&["--console"]) else { panic!() };
        assert_eq!(
            (plain.suffix(), plain.port_offset(), plain.instance_name()),
            (String::new(), 0, "default".to_string())
        );
        assert!(p(&["--console", "--instance", "100"]).is_err());
        assert!(p(&["--console", "--instance", "x"]).is_err());
        assert!(p(&["--console", "--instance"]).is_err());
    }
}
