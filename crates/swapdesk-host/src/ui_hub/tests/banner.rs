//! F-SESS-03, security review finding F3: a session of an account at the console never runs with no window to show
//! its banner. The service starts the window (a fake launcher here), an entry without a prompt waits for it, a window
//! that quits pauses the sessions it showed, and a locked screen or the sign-in screen, where no banner can be seen,
//! keep the behaviour ACCOUNTS.md 5.1 promises.

use super::extras::active_as;
use super::*;

const STORED: PermissionSet = PermissionSet::UNATTENDED.union(PermissionSet::CONTROL);

/// A host with no window of its user connected, a launcher that only records the sessions it was asked for, and a
/// viewer machine that enters without a prompt.
struct Rig {
    host: Machine,
    viewer: Machine,
    viewer_ui: Client,
    launches: Arc<Mutex<Vec<u32>>>,
}

impl Rig {
    fn new() -> Self {
        let mut host = Machine::new("host-pc");
        let viewer = Machine::new("viewer-pc");
        host.pin(&viewer, STORED);
        viewer.pin(&host, PermissionSet::EMPTY);
        host.attach_helper(IpcRole::HelperSystem);
        host.attach_helper(IpcRole::HelperUser);
        viewer.sees_minor(&host, false, swapdesk_proto::PROTO_MINOR);
        host.hub.set_console_session(Some(1));
        let launches: Arc<Mutex<Vec<u32>>> = Arc::default();
        let record = Arc::clone(&launches);
        host.hub.set_window_launcher(Arc::new(move |session| {
            lock(&record).push(session);
            Ok(())
        }));
        let viewer_ui = viewer.client(info(true));
        Self { host, viewer, viewer_ui, launches }
    }

    fn press(&mut self) -> u64 {
        let op = UiOp::Connect { device_id: self.host.hex(), intent: Intent::Control, unattended: false };
        let req = self.viewer_ui.request(op);
        self.viewer_ui.entry_of(req)
    }

    fn launched(&self) -> Vec<u32> {
        lock(&self.launches).clone()
    }

    fn finish(self) {
        let Rig { host, viewer, viewer_ui, .. } = self;
        drop(viewer_ui);
        viewer.finish();
        host.finish();
    }
}

fn is_active(m: &Ipc) -> bool {
    matches!(m, Ipc::UiNotify(Notify::Session { state: SessionState::Active { .. }, .. }))
}

/// The window of the host's user quits (Salir in the tray): its pipe says goodbye.
fn quit(mut window: Client) {
    window.send(&Ipc::Bye { code: swapdesk_proto::ErrorCode::NoError });
}

#[test]
fn an_entry_without_a_prompt_starts_the_window_and_waits_for_it_to_connect() {
    let mut rig = Rig::new();
    let entry = rig.press();
    // The service is asked to start the window in the console session, and nothing opens meanwhile.
    wait_for("the launcher", || (!rig.launched().is_empty()).then_some(()));
    assert_eq!(rig.launched(), vec![1], "once, in the console session");
    assert!(rig.viewer_ui.nothing_within(Duration::from_millis(600), is_active), "no session without the banner");
    assert!(rig.host.core.host().sessions().is_empty(), "nothing is live on the host");

    // The window connects: the entry goes through, and its banner is told of the session.
    let mut window = rig.host.client(info(false));
    let session = active_as(&mut rig.viewer_ui, Role::Viewer, &[entry]);
    let banner = active_as(&mut window, Role::Host, &[]);
    assert_eq!(banner, session, "the window that came up shows the session");
    assert_eq!(rig.launched(), vec![1], "no second start");
    drop(window);
    rig.finish();
}

#[test]
fn a_window_that_quits_mid_session_pauses_it_and_the_window_started_again_shows_the_session() {
    let mut rig = Rig::new();
    let window = rig.host.client(info(false));
    let entry = rig.press();
    let session = active_as(&mut rig.viewer_ui, Role::Viewer, &[entry]);
    assert!(rig.launched().is_empty(), "the window was there: nothing to start");
    let id = rig.host.core.host().sessions()[0].session_id;
    let live = rig.host.core.host().session(id).expect("live");
    wait_for("input allowed", || live.can_inject().then_some(()));

    quit(window);
    wait_for("the service starting the window again", || (!rig.launched().is_empty()).then_some(()));
    assert_eq!(rig.launched(), vec![1]);
    wait_for("the pause", || live.is_paused().then_some(()));
    assert!(!live.can_inject(), "no input without the banner");

    // The new window connects and is shown the live session; the session goes on.
    let mut window = rig.host.client(info(false));
    let shown = active_as(&mut window, Role::Host, &[]);
    assert_eq!(shown, session, "the same session");
    wait_for("the resume", || (!live.is_paused()).then_some(()));
    assert!(rig.viewer_ui.call(UiOp::Disconnect { session_id: session }).is_ok());
    drop(window);
    rig.finish();
}

/// ACCOUNTS.md 5.1: at the lock screen, a secure desktop or the sign-in screen no banner can be seen, so an entry
/// without a prompt still goes in there with no window; the window is required again the moment the user's own desktop
/// is back.
#[test]
fn the_lock_screen_and_the_sign_in_screen_need_no_window_and_the_users_desktop_does() {
    const SID: &str = "S-1-5-21-1-2-3-1001";
    let rig = Rig::new();
    let (ui, hub) = (&rig.host.hub, &rig.host.hub.hub);
    // No launcher: no window is required at all (the Mac, a development run).
    let bare = Machine::new("bare-pc");
    assert!(bare.hub.hub.banner_ready(), "no launcher, nothing required");
    bare.finish();

    ui.set_account(SID);
    ui.set_console_user(Some(SID.to_string()));
    assert!(!hub.banner_ready(), "the user's desktop with no window");
    rig.host.hub.set_desktop(Desktop::Locked);
    assert!(hub.banner_ready(), "the lock screen");
    rig.host.hub.set_desktop(Desktop::Default);
    assert!(!hub.banner_ready());
    rig.host.hub.set_helper_desktop(Desktop::Secure);
    assert!(hub.banner_ready(), "a UAC prompt");
    rig.host.hub.set_helper_desktop(Desktop::Default);
    ui.set_console_user(None);
    assert!(hub.banner_ready(), "the sign-in screen: nobody is signed in, no session to show a banner in");
    ui.set_console_user(Some("S-1-5-21-1-2-3-1002".into()));
    assert!(hub.banner_ready(), "another account at the console: this account's sessions are away anyway");
    ui.set_console_user(Some(SID.to_string()));
    let window = rig.host.client(info(false));
    wait_for("the window", || hub.banner_ready().then_some(()));
    quit(window);
    wait_for("the window to be gone", || (!hub.banner_ready()).then_some(()));
    rig.finish();
}

#[test]
fn an_entry_at_the_lock_screen_goes_in_with_no_window_and_is_paused_when_the_desktop_comes_back() {
    let mut rig = Rig::new();
    rig.host.hub.set_desktop(Desktop::Locked);
    let entry = rig.press();
    let session = active_as(&mut rig.viewer_ui, Role::Viewer, &[entry]);
    assert!(rig.launched().is_empty(), "nobody could see a banner at the lock screen: nothing was started");
    let id = rig.host.core.host().sessions()[0].session_id;
    let live = rig.host.core.host().session(id).expect("live");
    // The user unlocks and no window runs: the banner is owed now.
    rig.host.hub.set_desktop(Desktop::Default);
    wait_for("the window being started", || (!rig.launched().is_empty()).then_some(()));
    wait_for("the pause", || live.is_paused().then_some(()));
    let window = rig.host.client(info(false));
    wait_for("the resume", || (!live.is_paused()).then_some(()));
    assert!(rig.viewer_ui.call(UiOp::Disconnect { session_id: session }).is_ok());
    drop(window);
    rig.finish();
}
