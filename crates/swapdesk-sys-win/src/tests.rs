//! Tests run unelevated as the current user, who is also the development-mode pipe owner (C2.4). Nothing here needs
//! SYSTEM: the production token path, the SCM dispatcher with a real service and `SendSAS` are compile-checked only.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use swapdesk_platform::{ErrorKind, Level, SessionEvent, SessionWatcher, TraceSink};
use swapdesk_proto::Desktop;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, WM_DISPLAYCHANGE, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE,
    WTS_CONSOLE_CONNECT, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use super::*;
use crate::sys::{open_process_token, token_is_elevated, wide};

const WAIT: Duration = Duration::from_secs(10);

fn me() -> String {
    current_user_sid().unwrap()
}

fn own_pid() -> u32 {
    std::process::id()
}

fn unique(prefix: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{prefix}-{}-{}", own_pid(), N.fetch_add(1, Ordering::SeqCst))
}

/// A fresh temp folder, removed at the end.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(unique("swapdesk-sys-win"));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn dev_spec(name: &str, max: u32) -> PipeSpec {
    PipeSpec { name: name.to_string(), acl: PipeAcl::dev().unwrap(), max_instances: max }
}

// ---------- pipes ----------

/// Serves one connection from a client thread: the server returns the accepted end, the client thread's result is joined by the caller.
fn connect_pair(name: &str) -> (PipeConn, PipeConn) {
    let mut listener = PipeListener::bind(dev_spec(name, 1)).unwrap();
    let client_name = name.to_string();
    let client = std::thread::spawn(move || connect(&client_name, own_pid(), WAIT).unwrap());
    let server = listener.accept(Some(WAIT)).unwrap();
    (server, client.join().unwrap())
}

#[test]
fn pipe_exchanges_bytes_both_ways_concurrently_with_split_halves() {
    // 1 MiB each way is far beyond the 64 KiB pipe buffers: both sides must write while the other reads, which a
    // synchronous handle (a write stuck behind a pending read) could not do.
    const N: usize = 1 << 20;
    let (server, client) = connect_pair(&unique("t-duplex"));
    let (mut s_read, mut s_write) = server.split().unwrap();
    let (mut c_read, mut c_write) = client.split().unwrap();
    let pattern = |seed: u8| (0..N).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect::<Vec<u8>>();
    let (to_client, to_server) = (pattern(1), pattern(2));
    let (tc, ts) = (to_client.clone(), to_server.clone());

    let t1 = std::thread::spawn(move || s_write.write_all(&tc).unwrap());
    let t2 = std::thread::spawn(move || {
        let mut got = vec![0u8; N];
        s_read.read_exact(&mut got).unwrap();
        got
    });
    let t3 = std::thread::spawn(move || c_write.write_all(&ts).unwrap());
    let mut got_client = vec![0u8; N];
    c_read.read_exact(&mut got_client).unwrap();

    t1.join().unwrap();
    t3.join().unwrap();
    assert_eq!(t2.join().unwrap(), to_server);
    assert_eq!(got_client, to_client);
}

#[test]
fn pipe_try_clone_lets_one_thread_read_while_another_writes() {
    let (mut server, client) = connect_pair(&unique("t-clone"));
    let mut server_reader = server.try_clone().unwrap();
    let reader = std::thread::spawn(move || {
        let mut b = [0u8; 4];
        server_reader.read_exact(&mut b).unwrap();
        b
    });
    // The reader is blocked in ReadFile on the same handle while this thread writes.
    std::thread::sleep(Duration::from_millis(100));
    server.write_all(b"pong").unwrap();
    let mut client = client;
    let mut b = [0u8; 4];
    client.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"pong");
    client.write_all(b"ping").unwrap();
    assert_eq!(&reader.join().unwrap(), b"ping");
}

#[test]
fn server_sees_the_client_pid_and_session() {
    let (server, client) = connect_pair(&unique("t-pid"));
    assert_eq!(server.client_pid().unwrap(), own_pid());
    assert_eq!(server.client_session_id().unwrap(), session_of_pid(own_pid()).unwrap());
    assert_eq!(server.server_pid().unwrap(), own_pid());
    assert_eq!(client.server_pid().unwrap(), own_pid());
}

#[test]
fn client_accepts_the_right_server_pid_and_refuses_a_wrong_one() {
    let right = unique("t-srvpid-ok");
    let _l1 = PipeListener::bind(dev_spec(&right, 1)).unwrap();
    let conn = connect(&right, own_pid(), WAIT).unwrap();
    conn.check_server_pid(own_pid()).unwrap();

    let wrong = unique("t-srvpid-bad");
    let _l2 = PipeListener::bind(dev_spec(&wrong, 1)).unwrap();
    let bad_pid = own_pid() + 4;
    match connect(&wrong, bad_pid, WAIT) {
        Err(PipeError::ServerPidMismatch { expected, actual }) => {
            assert_eq!(expected, bad_pid);
            assert_eq!(actual, own_pid());
        }
        other => panic!("expected a PID mismatch, got {other:?}"),
    }
}

#[test]
fn second_first_instance_create_is_detected_as_a_squat() {
    let name = unique("t-squat");
    let _owner = PipeListener::bind(dev_spec(&name, 4)).unwrap();
    match PipeListener::bind(dev_spec(&name, 4)) {
        Err(e) => assert!(e.is_squatted(), "expected Squatted, got {e:?}"),
        Ok(_) => panic!("the second FIRST_PIPE_INSTANCE create must fail"),
    }
}

#[test]
fn dacl_read_back_has_exactly_the_two_expected_entries() {
    let name = unique("t-dacl");
    let listener = PipeListener::bind(dev_spec(&name, 1)).unwrap();
    let aces = listener.dacl().unwrap();
    let me = me();
    assert_eq!(aces.len(), 2, "{aces:?}");
    for ace in &aces {
        assert_eq!(ace.ace_type, 0, "access allowed");
        assert_eq!(ace.sid.as_deref(), Some(me.as_str()), "dev mode puts the current user in both rows");
    }
    assert_eq!(aces[0].mask, 0x001F_01FF, "row one: full control");
    assert_eq!(aces[1].mask, crate::pipe::USER_ROW_MASK, "row two: read and write, no create-instance");
}

#[test]
fn production_shaped_dacls_read_back_system_then_user_and_system_only() {
    let me = me();
    let both = PipeListener::bind(PipeSpec {
        name: unique("t-dacl-sys"),
        acl: PipeAcl::system_and_user(me.clone()),
        max_instances: 1,
    })
    .unwrap();
    let aces = both.dacl().unwrap();
    assert_eq!(
        aces.iter().map(|a| a.sid.clone().unwrap()).collect::<Vec<_>>(),
        vec!["S-1-5-18".to_string(), me.clone()]
    );

    let only =
        PipeListener::bind(PipeSpec { name: unique("t-dacl-sysonly"), acl: PipeAcl::system_only(), max_instances: 1 })
            .unwrap();
    let aces = only.dacl().unwrap();
    assert_eq!(aces.len(), 1);
    assert_eq!(aces[0].sid.as_deref(), Some("S-1-5-18"));

    assert_eq!(
        PipeAcl::system_and_user("S-1-5-21-1-2-3-1001").to_sddl().unwrap(),
        "D:P(A;;0x1f01ff;;;S-1-5-18)(A;;0x12019b;;;S-1-5-21-1-2-3-1001)"
    );
}

#[test]
fn impersonation_reports_the_current_user_and_the_elevation_of_this_token() {
    let (mut server, mut client) = connect_pair(&unique("t-imp"));
    // The server must read something first: the impersonation uses the last message read.
    client.write_all(b"hello").unwrap();
    let mut b = [0u8; 5];
    server.read_exact(&mut b).unwrap();
    let id = server.identify_client().unwrap();
    assert_eq!(id.user_sid, me());
    let own_elevated = token_is_elevated(open_process_token(TOKEN_QUERY).unwrap().0).unwrap();
    assert_eq!(id.elevated, own_elevated);
    if !own_elevated {
        assert!(!id.is_admin(), "an unelevated token is never admin (C2.3): {id:?}");
    }
}

#[test]
fn accept_times_out_and_the_listener_stays_usable() {
    let name = unique("t-accept");
    let mut listener = PipeListener::bind(dev_spec(&name, 1)).unwrap();
    assert_eq!(listener.accept(Some(Duration::from_millis(150))).unwrap_err(), PipeError::Timeout);
    let n = name.clone();
    let client = std::thread::spawn(move || connect(&n, own_pid(), WAIT).unwrap());
    let server = listener.accept(Some(WAIT)).unwrap();
    assert_eq!(server.client_pid().unwrap(), own_pid());
    drop(client.join().unwrap());
    // A single-instance pipe accepts once.
    match listener.accept(Some(Duration::from_millis(10))) {
        Err(PipeError::Os(e)) => assert_eq!(e.kind, ErrorKind::Closed),
        other => panic!("expected Closed, got {other:?}"),
    }
}

#[test]
fn multi_instance_listener_keeps_the_name_owned_between_clients() {
    let name = unique("t-multi");
    let mut listener = PipeListener::bind(dev_spec(&name, 4)).unwrap();
    for round in 0..3u8 {
        let n = name.clone();
        let client = std::thread::spawn(move || {
            let mut c = connect(&n, own_pid(), WAIT).unwrap();
            c.write_all(&[round]).unwrap();
        });
        let mut server = listener.accept(Some(WAIT)).unwrap();
        let mut b = [0u8; 1];
        server.read_exact(&mut b).unwrap();
        assert_eq!(b[0], round);
        client.join().unwrap();
    }
}

#[test]
fn peer_close_reads_as_eof_and_writes_as_broken_pipe_and_timeouts_and_cancel_work() {
    let (mut server, client) = connect_pair(&unique("t-close"));
    server.set_read_timeout(Some(Duration::from_millis(100)));
    let mut b = [0u8; 1];
    assert_eq!(server.read(&mut b).unwrap_err().kind(), std::io::ErrorKind::TimedOut);

    // cancel_io from another thread unblocks a pending read.
    server.set_read_timeout(None);
    let mut reader = server.try_clone().unwrap();
    let blocked = std::thread::spawn(move || reader.read(&mut [0u8; 1]));
    std::thread::sleep(Duration::from_millis(100));
    server.cancel_io();
    assert!(blocked.join().unwrap().is_err());

    drop(client);
    assert_eq!(server.read(&mut b).unwrap(), 0, "EOF");
    assert_eq!(server.write(b"x").unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
}

#[test]
fn connecting_to_a_missing_pipe_is_unavailable() {
    match connect(&unique("t-missing"), own_pid(), Duration::from_millis(50)) {
        Err(PipeError::Os(e)) => assert_eq!(e.kind, ErrorKind::Unavailable),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn pipe_names_are_plain_and_random_hex_is_128_bits() {
    let a = random_hex_128().unwrap();
    let b = random_hex_128().unwrap();
    assert_eq!(a.len(), 32);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(a, b);
    assert_eq!(ui_pipe_name(3), "ui-3");
    assert_eq!(helper_pipe_name(HelperRole::System, 2, &a), format!("helper-system-2-{a}"));
    assert_eq!(helper_pipe_name(HelperRole::User, 7, "ab"), "helper-user-7-ab");
    for bad in ["", "a\\b", "..\\x", "a b", "a/b"] {
        assert!(PipeListener::bind(dev_spec(bad, 1)).is_err(), "{bad:?}");
        assert!(connect(bad, own_pid(), Duration::from_millis(10)).is_err(), "{bad:?}");
    }
}

// ---------- launch ----------

#[test]
fn backoff_doubles_to_thirty_seconds_and_resets() {
    let mut b = RestartBackoff::new();
    let secs: Vec<u64> = (0..9).map(|_| b.next_delay().as_secs()).collect();
    assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30, 30]);
    b.reset();
    assert_eq!(b.next_delay().as_secs(), 1);
    assert_eq!(b.next_delay().as_secs(), 2);
    // A short run keeps counting; a long one starts over.
    assert_eq!(b.after_exit(Duration::from_secs(5)).as_secs(), 4);
    assert_eq!(b.after_exit(Duration::from_secs(61)).as_secs(), 1);
    // Many failures never overflow.
    let mut b = RestartBackoff::new();
    for _ in 0..500 {
        assert!(b.next_delay() <= RestartBackoff::MAX);
    }
}

#[test]
fn command_line_quoting_follows_the_msvc_rules() {
    assert_eq!(quote_arg("plain"), "plain");
    assert_eq!(quote_arg(""), "\"\"");
    assert_eq!(quote_arg("a b"), "\"a b\"");
    assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
    assert_eq!(quote_arg("C:\\dir with space\\"), "\"C:\\dir with space\\\\\"");
    assert_eq!(quote_arg("back\\slash"), "back\\slash");
    let line = build_command_line(
        Path::new("C:\\Program Files\\Swapdesk\\swapdesk-helper.exe"),
        &["--role".into(), "user".into(), "--pipe".into(), "helper-user-1-ab".into()],
    );
    assert_eq!(line, "\"C:\\Program Files\\Swapdesk\\swapdesk-helper.exe\" --role user --pipe helper-user-1-ab");
}

#[test]
fn user_token_choice_takes_the_linked_token_only_for_a_full_one() {
    assert_eq!(choose_user_token(1), UserTokenChoice::UseAsIs);
    assert_eq!(choose_user_token(3), UserTokenChoice::UseAsIs);
    assert_eq!(choose_user_token(2), UserTokenChoice::UseLinked);
    assert_eq!(choose_user_token(0), UserTokenChoice::Refuse);
    assert_eq!(choose_user_token(9), UserTokenChoice::Refuse);
}

fn system32(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join(name)
}

#[test]
fn dev_launch_starts_a_process_as_this_user_and_reports_its_exit_code() {
    let cmd = system32("cmd.exe");
    let child = launch_helper_dev(&cmd, &["/c".into(), "exit 7".into()]).unwrap();
    assert_ne!(child.pid(), 0);
    assert_eq!(child.user_token_elevation_type(), None);
    assert_eq!(child.wait(WAIT).unwrap(), Some(7));
    assert_eq!(child.try_wait().unwrap(), Some(7));
}

#[test]
fn dev_launch_child_can_be_killed() {
    let child = launch_helper_dev(&system32("PING.EXE"), &["-n".into(), "60".into(), "127.0.0.1".into()]).unwrap();
    assert_eq!(child.try_wait().unwrap(), None);
    child.kill().unwrap();
    assert_eq!(child.wait(WAIT).unwrap(), Some(1));
}

#[test]
fn dev_launch_of_a_missing_exe_fails() {
    assert!(launch_helper_dev(Path::new("C:\\no\\such\\swapdesk-helper.exe"), &[]).is_err());
}

fn create_dir_with_sddl(path: &Path, sddl: &str) {
    let w = wide(path.as_os_str()).unwrap();
    let s = wide(std::ffi::OsStr::new(sddl)).unwrap();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `s` is a null-terminated wide string; `sd` is a live out-parameter, freed with LocalFree below via the leak guard.
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        },
        0
    );
    let _free = crate::sys::LocalMem(sd);
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    // SAFETY: `w` and `sa` are live for the call.
    assert_ne!(unsafe { CreateDirectoryW(w.as_ptr(), &sa) }, 0, "CreateDirectoryW {}", path.display());
}

fn trusted_for_tests() -> Vec<String> {
    vec![me(), "S-1-5-18".into(), "S-1-5-32-544".into()]
}

#[test]
fn install_check_accepts_a_strict_folder_and_its_file_and_refuses_a_permissive_one() {
    let temp = Temp::new();
    let me = me();
    let strict = temp.0.join("strict");
    create_dir_with_sddl(&strict, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)"));
    let exe = strict.join("swapdesk-helper.exe");
    fs::write(&exe, b"MZ").unwrap();
    check_no_other_writers(&strict, &trusted_for_tests()).unwrap();
    check_no_other_writers(&exe, &trusted_for_tests()).unwrap();

    // Everyone may modify: refused, naming Everyone.
    let loose = temp.0.join("loose");
    create_dir_with_sddl(&loose, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;0x1301bf;;;WD)"));
    match check_no_other_writers(&loose, &trusted_for_tests()) {
        Err(InstallError::Writable { sid, .. }) => assert_eq!(sid, "S-1-1-0"),
        other => panic!("expected Writable, got {other:?}"),
    }
    // The file inherits the same entry.
    let loose_exe = loose.join("swapdesk-helper.exe");
    fs::write(&loose_exe, b"MZ").unwrap();
    assert!(matches!(check_no_other_writers(&loose_exe, &trusted_for_tests()), Err(InstallError::Writable { .. })));

    // An inherit-only entry does not touch the folder itself.
    let inherit_only = temp.0.join("inherit-only");
    create_dir_with_sddl(&inherit_only, &format!("D:P(A;OICI;FA;;;{me})(A;OICIIO;GA;;;WD)"));
    check_no_other_writers(&inherit_only, &trusted_for_tests()).unwrap();

    // The owner can always rewrite the DACL, so it must be trusted.
    match check_no_other_writers(&strict, &["S-1-5-18".to_string()]) {
        Err(InstallError::OwnerNotTrusted { .. } | InstallError::Writable { .. }) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn install_policy_accepts_the_real_system32_cmd_and_refuses_a_user_folder() {
    let policy = InstallPolicy {
        root: PathBuf::from(std::env::var_os("SystemRoot").unwrap()),
        trusted_sids: vec!["S-1-5-18".into(), "S-1-5-32-544".into(), TRUSTED_INSTALLER_SID.into()],
    };
    let verified = policy.verify(&system32("cmd.exe")).unwrap();
    assert!(verified.path().ends_with("cmd.exe"));

    // A file in the temp folder is outside the root, and the folder is writable by its owner only: refused for the root first.
    let temp = Temp::new();
    let exe = temp.0.join("swapdesk-helper.exe");
    fs::write(&exe, b"MZ").unwrap();
    assert!(matches!(policy.verify(&exe), Err(InstallError::OutsideRoot { .. })));
    // Under a root that contains it, the user's own temp folder fails the trusted-owner test of the production list.
    let in_temp = InstallPolicy { root: temp.0.clone(), trusted_sids: policy.trusted_sids.clone() };
    assert!(in_temp.verify(&exe).is_err());
    assert!(InstallPolicy::production().unwrap().root.ends_with("Program Files"));
}

// ---------- watcher ----------

#[test]
fn state_reports_the_console_session_and_user() {
    let (_tx, mut watcher) = session_channel();
    let state = watcher.state().unwrap();
    assert_eq!(state.session_id, active_console_session());
    assert_eq!(state.desktop, Desktop::Default);
    let own_session = session_of_pid(own_pid()).unwrap();
    if state.session_id == Some(own_session) {
        // This test runs in the console session: the user is the one running it.
        let want = std::env::var("USERNAME").unwrap();
        assert!(state.user.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(&want)), "{state:?} vs {want}");
        assert_eq!(session_user_name(own_session).unwrap(), state.user);
    }
    println!("console state: {state:?}");
}

#[test]
fn events_flow_through_the_channel_and_a_repeated_resume_arrives_once() {
    let (tx, mut watcher) = session_channel();
    let t = Duration::from_millis(200);
    assert_eq!(watcher.next_event(Duration::from_millis(20)).unwrap(), None);
    tx.suspending();
    tx.resumed();
    tx.resumed();
    tx.desktop_switched(Desktop::Locked);
    tx.displays_changed();
    tx.network_changed();
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::Suspending));
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::Resumed));
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::DesktopSwitched(Desktop::Locked)));
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::DisplaysChanged));
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::NetworkChanged));
    assert_eq!(watcher.next_event(Duration::from_millis(20)).unwrap(), None);
    tx.console_changed();
    match watcher.next_event(t).unwrap() {
        Some(SessionEvent::ConsoleChanged(s)) => assert_eq!(s.session_id, active_console_session()),
        other => panic!("{other:?}"),
    }
    tx.stop();
    assert_eq!(watcher.next_event(t).unwrap_err().kind, ErrorKind::Closed);
    assert_eq!(watcher.next_event(t).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn a_wake_returns_next_event_at_once_with_no_event_and_does_not_end_the_watcher() {
    let (tx, mut watcher) = session_channel();
    let began = std::time::Instant::now();
    let waker = {
        let tx = tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            tx.wake();
        })
    };
    // A long wait is cut short by the wake, which reads as "nothing to report, look at your own queues".
    assert_eq!(watcher.next_event(Duration::from_secs(20)).unwrap(), None);
    let waited = began.elapsed();
    assert!((Duration::from_millis(100)..Duration::from_secs(5)).contains(&waited), "waited {waited:?}");
    waker.join().unwrap();
    tx.network_changed();
    assert_eq!(watcher.next_event(Duration::from_millis(200)).unwrap(), Some(SessionEvent::NetworkChanged));
}

#[test]
fn a_dir_watch_sleeps_until_a_file_appears_and_wakes_for_each_change() {
    let dir = Temp::new();
    let watch = std::sync::Arc::new(DirWatch::new(&dir.0).unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = {
        let watch = std::sync::Arc::clone(&watch);
        std::thread::spawn(move || {
            while watch.wait().is_ok() {
                if tx.send(()).is_err() {
                    return;
                }
            }
        })
    };
    // Nothing changes: the thread sleeps in the kernel and reports nothing.
    assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());
    let began = std::time::Instant::now();
    fs::write(dir.0.join("stop"), b"").unwrap();
    rx.recv_timeout(WAIT).expect("woken by the new file");
    assert!(began.elapsed() < Duration::from_secs(2), "woke {:?} after the file", began.elapsed());
    // Armed again: deleting it is the next change.
    fs::remove_file(dir.0.join("stop")).unwrap();
    rx.recv_timeout(WAIT).expect("woken by the removal");
    drop(rx);
    // The waiter ends at its next wake (the send fails): make one.
    fs::write(dir.0.join("again"), b"").unwrap();
    waiter.join().unwrap();
}

#[test]
fn a_dropped_sender_closes_the_watcher() {
    let (tx, mut watcher) = session_channel();
    drop(tx);
    assert_eq!(watcher.next_event(Duration::from_millis(20)).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn session_and_power_messages_map_to_events() {
    use crate::watcher::{Raw, power_event, session_change_event};
    assert_eq!(session_change_event(WTS_CONSOLE_CONNECT), Some(Raw::ConsoleChanged));
    assert_eq!(session_change_event(WTS_SESSION_LOCK), Some(Raw::Desktop(Desktop::Locked)));
    assert_eq!(session_change_event(WTS_SESSION_UNLOCK), Some(Raw::Desktop(Desktop::Default)));
    assert_eq!(session_change_event(0x7777), None);
    assert_eq!(power_event(PBT_APMSUSPEND), Some(Raw::Suspending));
    assert_eq!(power_event(PBT_APMRESUMEAUTOMATIC), Some(Raw::Resumed));
    assert_eq!(power_event(0x7777), None);
}

#[test]
fn the_hidden_window_turns_posted_messages_into_events() {
    let (tx, mut watcher) = session_channel();
    let window = WindowWatcher::spawn(tx).unwrap();
    let hwnd = window.hwnd() as HWND;
    let post = |msg: u32, w: u32| {
        // SAFETY: `hwnd` is the live window of `window`; posting to it is safe from any thread.
        assert_ne!(unsafe { PostMessageW(hwnd, msg, w as usize, 0) }, 0);
    };
    post(WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK);
    post(WM_POWERBROADCAST, PBT_APMSUSPEND);
    post(WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC);
    post(WM_DISPLAYCHANGE, 0);
    post(WM_WTSSESSION_CHANGE, WTS_CONSOLE_CONNECT);
    let t = WAIT;
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::DesktopSwitched(Desktop::Locked)));
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::Suspending));
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::Resumed));
    assert_eq!(watcher.next_event(t).unwrap(), Some(SessionEvent::DisplaysChanged));
    assert!(matches!(watcher.next_event(t).unwrap(), Some(SessionEvent::ConsoleChanged(_))));
    drop(window);
    // The window is gone with its sender: the watcher closes.
    assert_eq!(watcher.next_event(t).unwrap_err().kind, ErrorKind::Closed);
}

// ---------- service ----------

#[test]
fn querying_a_missing_service_is_not_found() {
    assert_eq!(
        query_service("SwapdeskNoSuchService").unwrap_err(),
        ServiceError::NotFound("SwapdeskNoSuchService".into())
    );
    assert_eq!(
        service_pid("SwapdeskNoSuchService").unwrap_err(),
        ServiceError::NotFound("SwapdeskNoSuchService".into())
    );
    assert!(matches!(query_service("bad\\name"), Err(ServiceError::Os(_))));
}

#[test]
fn querying_a_running_system_service_gives_its_pid() {
    let pid = service_pid("EventLog").unwrap();
    assert_ne!(pid, 0);
    assert_eq!(query_service("EventLog").unwrap().state, 4, "SERVICE_RUNNING");
}

#[test]
fn run_service_outside_the_scm_reports_not_a_service() {
    // The only test that touches the process-wide dispatcher state.
    let r = run_service("SwapdeskTest", |_ctx| 0);
    assert_eq!(r.unwrap_err(), ServiceError::NotAService);
    assert_eq!(run_service("SwapdeskTest", |_ctx| 0).unwrap_err(), ServiceError::AlreadyStarted);
}

#[test]
fn the_stop_signal_wakes_a_waiter() {
    let s = StopSignal::new();
    assert!(!s.wait(Duration::from_millis(20)));
    let s2 = s.clone();
    let t = std::thread::spawn(move || s2.wait(WAIT));
    std::thread::sleep(Duration::from_millis(50));
    s.set();
    assert!(t.join().unwrap());
    assert!(s.is_set());
}

// ---------- power and SAS ----------

#[test]
fn keep_awake_turns_on_and_off() {
    let k = KeepAwake::new().unwrap();
    assert!(!k.is_on());
    k.set(true).unwrap();
    assert!(k.is_on());
    k.set(true).unwrap();
    k.set(false).unwrap();
    assert!(!k.is_on());
    k.set(true).unwrap();
    drop(k); // clears the request and ends the thread
}

#[test]
fn sas_dll_loads_from_system32_and_exports_send_sas() {
    assert!(sas_available());
    // `send_sas` itself is never called here: only a service may send the sequence.
}

// ---------- trace ----------

fn read_lines(p: &Path) -> Vec<String> {
    fs::read_to_string(p).unwrap().lines().map(str::to_owned).collect()
}

#[test]
fn trace_writes_one_line_per_record_with_the_documented_fields() {
    let t = Temp::new();
    let log = FileTrace::open(&t.0, "service.log", Level::Debug).unwrap();
    log.log(Level::Info, "net", format_args!("listening on {}", 7777));
    log.log(Level::Error, "pair", format_args!("two\nlines\r\nin one"));
    let lines = read_lines(log.path());
    assert_eq!(lines.len(), 2);
    let mut f = lines[0].splitn(4, ' ');
    let ms: u128 = f.next().unwrap().parse().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    assert!(now.abs_diff(ms) < 60_000);
    assert_eq!((f.next().unwrap(), f.next().unwrap(), f.next().unwrap()), ("INFO", "net", "listening on 7777"));
    assert!(lines[1].ends_with(" ERROR pair two lines  in one"), "{}", lines[1]);
}

#[test]
fn trace_drops_records_below_the_level_and_appends_across_opens() {
    let t = Temp::new();
    {
        let log = FileTrace::open(&t.0, "a.log", Level::Warn).unwrap();
        log.log(Level::Debug, "x", format_args!("debug"));
        log.log(Level::Info, "x", format_args!("info"));
        log.log(Level::Warn, "x", format_args!("warn"));
        log.log(Level::Error, "x", format_args!("error"));
    }
    let log = FileTrace::open(&t.0, "a.log", Level::Warn).unwrap();
    log.log(Level::Error, "x", format_args!("again"));
    let texts: Vec<String> =
        read_lines(log.path()).iter().map(|l| l.splitn(4, ' ').nth(3).unwrap().to_string()).collect();
    assert_eq!(texts, ["warn", "error", "again"]);
    assert!(FileTrace::open(&t.0, "..\\evil.log", Level::Info).is_err());
}

#[test]
fn trace_rotates_at_the_cap_and_keeps_one_old_file() {
    let t = Temp::new();
    let cap = 400u64;
    let log = FileTrace::open_with_cap(&t.0, "r.log", Level::Debug, cap).unwrap();
    for i in 0..60 {
        log.log(Level::Info, "rot", format_args!("record number {i:03}"));
    }
    let main = t.0.join("r.log");
    let old = t.0.join("r.log.1");
    assert!(old.exists());
    assert!(fs::metadata(&main).unwrap().len() <= cap);
    assert!(fs::metadata(&old).unwrap().len() <= cap);
    let names: Vec<_> =
        fs::read_dir(&t.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    // The newest record is in the main file; nothing was torn in the middle of a line.
    let main_lines = read_lines(&main);
    assert!(main_lines.last().unwrap().ends_with("record number 059"));
    for l in main_lines.iter().chain(read_lines(&old).iter()) {
        assert!(l.contains(" INFO rot record number "), "{l}");
    }
}

#[test]
fn trace_is_thread_safe() {
    let t = Temp::new();
    let log = std::sync::Arc::new(FileTrace::open(&t.0, "mt.log", Level::Debug).unwrap());
    let handles: Vec<_> = (0..4)
        .map(|n| {
            let log = log.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    log.log(Level::Info, "mt", format_args!("thread {n} line {i}"));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let lines = read_lines(log.path());
    assert_eq!(lines.len(), 800);
    assert!(lines.iter().all(|l| l.splitn(4, ' ').nth(3).is_some_and(|t| t.starts_with("thread "))));
}

#[test]
fn session_user_sid_is_refused_without_the_tcb_privilege() {
    // This test runs as a standard user: WTSQueryUserToken needs SE_TCB_NAME (the SYSTEM service), so the call
    // must fail with an error, never a wrong SID.
    let session = crate::active_console_session().expect("a console session");
    assert!(crate::session_user_sid(session).is_err());
}
