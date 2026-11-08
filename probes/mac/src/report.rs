//! `report`: summarises every probe log into a results file the session can read.
//! The AUTO block is rewritten each run; the MANUAL block (the owner's yes/no lines) is kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MANUAL_MARK: &str = "=== MANUAL";

const MANUAL_TEMPLATE: &str = "=== MANUAL (the owner fills these lines: sim / nao / ? and a short note; kept when `report` runs again) ===
P0.macos_version_and_fdesetup:
P0.sdk_declaration_CGWindowListCreateImage:
P0.sdk_kVK_values_0x0A_0x32:
P1.codesign_verify_ok_per_signer:
P2.daemon_register_requires_approval_then_enabled:
P2.agent_enabled_after_approving_daemon:
P3.loginwindow_via_smappservice_error_text:
P4.screen_recording_prompt_name_shown:
P4.settings_list_entry_name:
P4.sck_video_frames_and_audio_in_aqua_agent:
P4.accessibility_granted_post_event_true:
P4.lock_screen_capture_frames_and_luma:
P4.lock_screen_inject_pointer_moved_and_dots:
P5.loginwindow_sck_frames:
P5.loginwindow_cgstream_fallback_frames:
P6.loginwindow_pointer_square_plain:
P6.loginwindow_text_appeared_plain:
P6.loginwindow_inject_with_prelogin_section:
P7.devid_tcc_survives_rebuild:
P7.adhoc_tcc_survives_rebuild:
P7.selfsigned_tcc_survives_rebuild:
P7.adhoc_and_selfsigned_smappservice_daemon_registers:
P8.terminal_no_prompt_and_ok:
P8.app_prompt_and_errors_before_allow:
P8.bonjour_instance_seen_by_dns_sd:
P8.agent_prompt_name:
P8.daemon_ok_no_prompt:
P8.loginwindow_agent_ok_no_prompt:
P8.ipc_unix_socket_ok_no_prompt:
P9.key_left_of_1_code_and_char:
P9.key_right_of_left_shift_code_and_char:
P9.cedilla_code_and_char:
P9.right_option_code_and_char:
P9.keyboard_layout:
P9.active_tap_swallows_cmd_tab_and_permission_asked:
P10.wake_for_network_access_options:
P10.wakes_on_magic_packet_lid_open:
P10.wakes_on_magic_packet_lid_closed:
P11.filevault_unlock_over_ssh_optional:
P12.vt_low_latency_optional:
";

#[derive(Default)]
struct Row {
    ok: u32,
    fail: u32,
    last_ok: String,
    last_fail: String,
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let i = line.find(&pat)? + pat.len();
    let rest = &line[i..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(&stripped[..end])
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        Some(&rest[..end])
    }
}

fn short(s: &str) -> String {
    if s.chars().count() > 360 {
        let mut t: String = s.chars().take(360).collect();
        t.push_str("...");
        t
    } else {
        s.to_string()
    }
}

fn log_files(only: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Library/Logs/SwapdeskProbe")];
    if let Some(h) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(h).join("Library/Logs/SwapdeskProbe"));
    }
    if let Some(dir) = only {
        dirs = vec![PathBuf::from(dir)];
    }
    let mut files = Vec::new();
    for dir in dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "log") {
                    files.push(p);
                }
            }
        }
    }
    files.sort();
    files
}

pub fn run(out: &Path, logs: Option<&str>) {
    let manual = std::fs::read_to_string(out)
        .ok()
        .and_then(|s| s.find(MANUAL_MARK).map(|i| s[i..].to_string()))
        .unwrap_or_else(|| MANUAL_TEMPLATE.to_string());

    let mut text = String::new();
    text.push_str(&format!(
        "Swapdesk M0-mac probe results. Written by `swapdesk-probe report` at {} (build nonce {}).\n",
        crate::log::now_iso(),
        crate::log::NONCE
    ));
    text.push_str("=== AUTO (rewritten on every run; one line per log file and check: ok count, fail count, last ok, last fail) ===\n");
    for file in log_files(logs) {
        let name = file.display().to_string();
        let Ok(content) = std::fs::read_to_string(&file) else {
            text.push_str(&format!("[{name}] unreadable (run `sudo` for root logs)\n"));
            continue;
        };
        let mut rows: BTreeMap<String, Row> = BTreeMap::new();
        for line in content.lines() {
            let (Some(check), Some(ok)) = (field(line, "check"), field(line, "ok")) else { continue };
            let row = rows.entry(check.to_string()).or_default();
            let summary = format!(
                "t={} session={} uid={} signer={} detail={}",
                field(line, "t").unwrap_or("?"),
                field(line, "session").unwrap_or("?"),
                field(line, "uid").unwrap_or("?"),
                field(line, "signer").unwrap_or("?"),
                line.find("\"detail\":").map_or("", |i| &line[i + 9..]),
            );
            if ok == "true" {
                row.ok += 1;
                row.last_ok = short(&summary);
            } else {
                row.fail += 1;
                row.last_fail = short(&summary);
            }
        }
        text.push_str(&format!("[{name}] {} lines\n", content.lines().count()));
        for (check, row) in rows {
            text.push_str(&format!("  {check}: ok={} fail={}\n", row.ok, row.fail));
            if !row.last_ok.is_empty() {
                text.push_str(&format!("    last ok  : {}\n", row.last_ok));
            }
            if !row.last_fail.is_empty() {
                text.push_str(&format!("    last fail: {}\n", row.last_fail));
            }
        }
    }
    text.push('\n');
    text.push_str(&manual);
    match std::fs::write(out, &text) {
        Ok(()) => println!("wrote {}", out.display()),
        Err(e) => eprintln!("cannot write {}: {e}", out.display()),
    }
}
