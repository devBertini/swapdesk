//! The host banner is for the person at the Mac alone: its guard (`swapdesk-shell-mac`) tells the controller's input
//! from a real device by the mark the host's input sink (`swapdesk-input-mac`) puts on what it posts. The two crates
//! do not know each other; this keeps their numbers equal.
#![cfg(target_os = "macos")]

#[test]
fn the_mark_the_host_puts_on_its_input_is_the_one_the_banner_guard_looks_for() {
    assert_eq!(swapdesk_input_mac::POSTED_MARK, swapdesk_shell_mac::BANNER_POSTED_MARK);
}
