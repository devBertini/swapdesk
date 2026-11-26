//! The installed service brings its own firewall rules to the shape of this version at every start (D94).
//!
//! An update through the updater swaps the files only (UPDATES.md 6.2): `install.ps1`, which makes the rules, does not
//! run. 1.2.0 changed them (D93, D94: any remote address on every profile, and the probe port 47819), so a machine
//! updated from 1.1.0 would keep 1.1.0's rules (LocalSubnet, Private and Domain, from 47820) and refuse the search and
//! every peer from another network. The service runs as SYSTEM and sets the two rules of the group `Swapdesk` with
//! `netsh advfirewall firewall set rule name=<rule> new ...`, which changes a rule that exists and fails on one that
//! does not: a rule the person removed is not made again. A development instance (`--console`) never touches the
//! firewall.

/// The rule of the probe, session and pairing ports (install.ps1 `$RuleSession`).
pub const RULE_SESSION: &str = "Swapdesk-UDP-Session-Pairing";
/// The rule of mDNS (install.ps1 `$RuleMdns`).
pub const RULE_MDNS: &str = "Swapdesk-UDP-mDNS";
/// The ports of `RULE_SESSION` (install.ps1 `$PortsSession`).
pub const PORTS_SESSION: &str = "47819-47883";

/// The arguments of the two `netsh` calls, in order.
pub fn calls() -> [Vec<String>; 2] {
    let set = |name: &str, new: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = ["advfirewall", "firewall", "set", "rule"].iter().map(|s| s.to_string()).collect();
        v.push(format!("name={name}"));
        v.push("new".into());
        v.extend(new.iter().map(|s| s.to_string()));
        v
    };
    [
        set(RULE_SESSION, &[&format!("localport={PORTS_SESSION}"), "remoteip=any", "profile=any"]),
        set(RULE_MDNS, &["profile=any"]),
    ]
}

/// Runs the two calls on a thread of its own (the service's start does not wait for them) and leaves one trace line
/// per rule.
#[cfg(windows)]
pub fn bring_up_to_date(trace: std::sync::Arc<dyn swapdesk_platform::TraceSink>) {
    use swapdesk_platform::Level;
    let _ = std::thread::Builder::new().name("swapdesk-firewall".into()).spawn(move || {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // The system's own netsh, never one found on PATH.
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let netsh = std::path::Path::new(&root).join("System32").join("netsh.exe");
        for args in calls() {
            let rule = args[4].trim_start_matches("name=").to_string();
            let out = std::process::Command::new(&netsh)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    trace.log(Level::Info, "firewall", format_args!("rule {rule} is up to date"));
                }
                Ok(o) => trace.log(
                    Level::Warn,
                    "firewall",
                    format_args!(
                        "rule {rule} was not changed (exit {:?}): {}",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stdout).trim()
                    ),
                ),
                Err(e) => trace.log(Level::Warn, "firewall", format_args!("netsh did not run: {e}")),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules the service sets are the ones install.ps1 makes, with the same names and ports.
    #[test]
    fn the_rules_match_the_installer() {
        let script = include_str!("../../../packaging/windows/install.ps1");
        assert!(script.contains(&format!("$RuleSession    = '{RULE_SESSION}'")));
        assert!(script.contains(&format!("$RuleMdns       = '{RULE_MDNS}'")));
        assert!(script.contains(&format!("$PortsSession   = '{PORTS_SESSION}'")));
        assert!(script.contains("-Profile Any -RemoteAddress Any"));
        let [session, mdns] = calls();
        assert_eq!(
            session.join(" "),
            "advfirewall firewall set rule name=Swapdesk-UDP-Session-Pairing new localport=47819-47883 remoteip=any profile=any"
        );
        assert_eq!(mdns.join(" "), "advfirewall firewall set rule name=Swapdesk-UDP-mDNS new profile=any");
    }
}
